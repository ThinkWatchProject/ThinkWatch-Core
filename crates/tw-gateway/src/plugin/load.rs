//! 加载：读文件、算哈希、和批准的比、编译。
//!
//! **不变式 I9：跑的只能是批准过的那一份字节。**文件只读一次，哈希算的就是这一次读到
//! 的字节，编译的也是这些字节 —— 不是先哈希一遍、再另读一遍去编。哈希和配置里的
//! `sha256` 对不上（文件改了、没了）就是「文件变了」，不跑。
//!
//! **一个插件出了问题，只停它自己**：读不了、编不了、设置对不上，都落在那一个插件的
//! 状态上，配置照样换入（`Runtime::build` 从这里拿不到错误）。
//!
//! 每次换配置都会把所有插件文件重读一遍、重算哈希 —— 这本身就是「文件变了」的一道
//! 检查；另有一个盯着 `plugins/` 目录的监听（在控制面），文件一动就单独重载一次插件。
//! 编译的结果按哈希缓存：同一份字节不编第二遍。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use sha2::{Digest, Sha256};
use tw_types::{Msg, msg};

use crate::plugin::engine::{Engine, LoadError, MAX_SOURCE, Manifest};
use crate::plugin::host::PluginHost;
use crate::plugin::set::{Active, Broken, LogRing, PluginSet, Scope, State, Stats};

/// 一份编译结果：编好的插件，或者编不成的原因
type Compiled = Result<Arc<dyn PluginHost>, LoadError>;

/// 插件这件事里**跨重载存活**的部分：运行时、文件在哪儿、每个插件的计数和日志、
/// 编译结果的缓存，以及运行记录交给谁存。
pub struct Plugins {
    engine: RwLock<Arc<dyn Engine>>,
    /// 配置文件所在的目录。插件文件的路径相对它。**由控制面告诉我们**（它知道配置
    /// 文件在哪儿）；还不知道时每个插件都加载不了
    dir: RwLock<Option<PathBuf>>,
    tracks: Mutex<HashMap<String, Track>>,
    compiled: Mutex<HashMap<[u8; 32], Compiled>>,
    sink: Mutex<Option<RunSender>>,
    /// 改插件文件和改配置是一件事的两半（写文件、写哈希）。**控制面改的时候攥着它**，
    /// 目录监听重载插件之前也要拿到它 —— 不然监听可能正好落在两半之间，把一个马上就要
    /// 对上的文件当成「变了」报出去
    pub edits: tokio::sync::Mutex<()>,
}

/// 一个插件的计数和日志。按 id 挂着，每份插件拿到同一对
#[derive(Clone, Default)]
struct Track {
    stats: Arc<Stats>,
    logs: Arc<LogRing>,
}

/// 一次运行，交给存储层落库（`plugin_runs` 一行）。
#[derive(Debug, Clone)]
pub struct RunRecord {
    pub request_id: u64,
    pub at_ms: u64,
    pub run: crate::plugin::PluginRun,
}

/// 运行记录往哪儿交。`None` 是观测层没起来：只计数、不落库。
pub type RunSender = tokio::sync::mpsc::Sender<RunRecord>;

/// 通道容量。**一条记录几百字节**，比正文那条通道宽得多：每个请求上每个插件一条，
/// 而丢一条就是请求上少了一次运行的记录（不变式 I10）。满了还是丢 —— 观测不能挡住转发
pub const RUN_CHANNEL_CAP: usize = 4096;

impl Plugins {
    pub fn new(engine: Arc<dyn Engine>) -> Self {
        Self {
            engine: RwLock::new(engine),
            dir: RwLock::new(None),
            tracks: Mutex::default(),
            compiled: Mutex::default(),
            sink: Mutex::default(),
            edits: tokio::sync::Mutex::new(()),
        }
    }

    pub fn engine(&self) -> Arc<dyn Engine> {
        self.engine
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// 换一个运行时。**缓存一起清掉**：同一份字节在新运行时上要重新编
    pub fn set_engine(&self, engine: Arc<dyn Engine>) {
        *self.engine.write().unwrap_or_else(PoisonError::into_inner) = engine;
        self.compiled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    pub fn dir(&self) -> Option<PathBuf> {
        self.dir
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// 记下配置文件所在的目录。返回它是不是变了
    pub fn set_dir(&self, dir: PathBuf) -> bool {
        let mut g = self.dir.write().unwrap_or_else(PoisonError::into_inner);
        if g.as_ref() == Some(&dir) {
            return false;
        }
        *g = Some(dir);
        true
    }

    pub fn set_sink(&self, tx: RunSender) {
        *self.sink.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
    }

    /// 交一条运行记录出去。**满了就丢，绝不等待**
    pub(crate) fn offer(&self, rec: RunRecord) {
        let tx = self
            .sink
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(tx) = tx {
            let _ = tx.try_send(rec);
        }
    }

    /// 编一份源码看看，**不留任何东西**：不进缓存、不碰计数。装之前给人过目用
    pub fn inspect(&self, source: &[u8]) -> Compiled {
        self.engine().load(source)
    }

    /// 编一份马上要装上（或批准）的源码，**结果留进缓存**：紧接着的那次重载按哈希
    /// 找到它，不在换配置的那一路上再编一遍。没装成的，下一次重载清掉
    pub fn prepare(&self, source: &[u8]) -> Compiled {
        let engine = self.engine();
        self.compile(&*engine, Sha256::digest(source).into(), source)
    }

    /// 照这份配置建一份插件。**不会失败**：哪个插件有问题，问题落在它自己的状态上。
    pub fn build(&self, config: &tw_config::Config) -> PluginSet {
        let dir = self.dir();
        let engine = self.engine();
        let mut used = HashSet::new();
        let mut out = Vec::with_capacity(config.plugins.len());
        for p in &config.plugins {
            let track = self.track(&p.id);
            let (state, manifest) = match dir.as_deref() {
                Some(dir) => self.load_one(dir, p, &*engine, &mut used),
                None => (State::Broken(Broken::Error(not_located())), None),
            };
            let (state, settings) = match (state, &manifest) {
                (state, None) => (state, serde_json::Map::new()),
                (state, Some(m)) => match settings_of(m, &p.settings) {
                    Ok(s) => (state, s),
                    // 设置对不上：照样显示它（manifest 在），但不跑
                    Err(why) => (State::Broken(Broken::Error(why)), serde_json::Map::new()),
                },
            };
            let m = manifest.as_ref();
            out.push(Arc::new(Active {
                id: p.id.clone(),
                name: m.map_or_else(|| p.id.clone(), |m| m.name.clone()),
                enabled: p.enabled,
                on_error: p.on_error.into(),
                scope: scope_of(&p.scope),
                permissions: m.map(|m| m.permissions.clone()).unwrap_or_default(),
                reply_mode: m.map_or(tw_api::ReplyMode::Block, |m| m.reply_mode),
                hooks: m.map(|m| m.hooks).unwrap_or_default(),
                settings,
                manifest,
                state,
                stats: track.stats,
                logs: track.logs,
            }));
        }
        // 只留这一份还用得着的：编译结果、计数和日志。删掉的插件，它的计数跟着走
        self.compiled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|k, _| used.contains(k));
        self.tracks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|id, _| config.plugins.iter().any(|p| &p.id == id));
        PluginSet::new(out)
    }

    fn track(&self, id: &str) -> Track {
        self.tracks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(id.to_string())
            .or_default()
            .clone()
    }

    /// 一个插件：它此刻的状态，和能读出来的 manifest。
    fn load_one(
        &self,
        dir: &Path,
        p: &tw_config::Plugin,
        engine: &dyn Engine,
        used: &mut HashSet<[u8; 32]>,
    ) -> (State, Option<Manifest>) {
        let path = p.path_in(dir);
        let bytes = match read_capped(&path) {
            Ok(b) => b,
            // 文件没了也是「变了」：批准过的那一份不在原处了
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (
                    State::Broken(Broken::Changed),
                    self.approved_manifest(dir, p, engine, used),
                );
            }
            Err(e) => {
                return (
                    State::Broken(Broken::Error(msg!(
                        "gw.plugin.unreadable", file = &p.file, detail = e =>
                        "The plugin file {file} cannot be read: {detail}"
                    ))),
                    self.approved_manifest(dir, p, engine, used),
                );
            }
        };
        let sha: [u8; 32] = Sha256::digest(&bytes).into();
        if hex(&sha) != p.sha256 {
            return (
                State::Broken(Broken::Changed),
                self.approved_manifest(dir, p, engine, used),
            );
        }
        used.insert(sha);
        match self.compile(engine, sha, &bytes) {
            Ok(host) => {
                let m = host.manifest().clone();
                (State::Ready(host), Some(m))
            }
            Err(e) => (State::Broken(Broken::Error(e.msg())), None),
        }
    }

    /// 文件变了时，批准过的那一份的 manifest —— **只拿来显示**（名字、权限、设置项），
    /// 不跑。底稿也不是那一份了（被人动过、没了）就没有。
    fn approved_manifest(
        &self,
        dir: &Path,
        p: &tw_config::Plugin,
        engine: &dyn Engine,
        used: &mut HashSet<[u8; 32]>,
    ) -> Option<Manifest> {
        let bytes = read_capped(&tw_config::plugins::approved_path(dir, &p.id)).ok()?;
        let sha: [u8; 32] = Sha256::digest(&bytes).into();
        if hex(&sha) != p.sha256 {
            return None;
        }
        used.insert(sha);
        let host = self.compile(engine, sha, &bytes).ok()?;
        Some(host.manifest().clone())
    }

    fn compile(&self, engine: &dyn Engine, sha: [u8; 32], bytes: &[u8]) -> Compiled {
        let mut cache = self.compiled.lock().unwrap_or_else(PoisonError::into_inner);
        cache
            .entry(sha)
            .or_insert_with(|| engine.load(bytes))
            .clone()
    }
}

/// 读一个文件，**最多读到上限多一个字节**：再大的插件反正编不了，哈希也一定对不上
pub fn read_capped(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_SOURCE as u64 + 1)
        .read_to_end(&mut buf)?;
    Ok(buf)
}

/// SHA-256 的小写十六进制
pub fn hex(sha: &[u8; 32]) -> String {
    sha.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// 一份字节的 SHA-256，小写十六进制
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes).into())
}

fn scope_of(s: &tw_config::PluginScope) -> Scope {
    Scope {
        clients: s.clients.clone(),
        models: s.models.clone(),
        upstreams: s.upstreams.clone(),
    }
}

fn not_located() -> Msg {
    msg!(
        "gw.plugin.not_located" =>
        "The plugin files cannot be found: the gateway has not been told where its configuration \
         lives."
    )
}

/// 交给插件的设置：manifest 的默认值，配置里写了的盖上去。**键和类型都要对得上**：
/// 插件没声明的键、类型不对的值，都是错 —— 悄悄丢掉的话，用户改的设置看着在，其实
/// 不起作用。
pub fn settings_of<V: serde::Serialize>(
    m: &Manifest,
    configured: &BTreeMap<String, V>,
) -> Result<serde_json::Map<String, serde_json::Value>, Msg> {
    if let Some(key) = configured
        .keys()
        .find(|k| !m.settings.iter().any(|s| &s.key == *k))
    {
        return Err(msg!(
            "gw.plugin.setting_unknown", key = key =>
            "Setting `{key}` is not one the plugin declares."
        ));
    }
    let mut out = serde_json::Map::new();
    for spec in &m.settings {
        let value = match configured.get(&spec.key) {
            None => spec.default.clone(),
            Some(v) => {
                let v = serde_json::to_value(v).unwrap_or(serde_json::Value::Null);
                if !fits(spec.kind, &v) {
                    return Err(msg!(
                        "gw.plugin.setting_type", key = &spec.key, kind = spec.kind.slug() =>
                        "Setting `{key}` has to be a {kind}."
                    ));
                }
                v
            }
        };
        out.insert(spec.key.clone(), value);
    }
    Ok(out)
}

/// 这个值是不是这种设置的类型
pub fn fits(kind: tw_api::SettingKind, v: &serde_json::Value) -> bool {
    matches!(
        (kind, v),
        (tw_api::SettingKind::String, serde_json::Value::String(_))
            | (tw_api::SettingKind::Number, serde_json::Value::Number(_))
            | (tw_api::SettingKind::Boolean, serde_json::Value::Bool(_))
    )
}

/// 换了一份插件之后要说一声的：**启用着的插件刚变成跑不了**（文件变了、加载出错），
/// 或者跑不了的原因变了。一直跑不了的不再说第二遍 —— 每改一次配置都重报一遍，用户
/// 很快就学会了不看。
pub fn newly_broken(old: &PluginSet, new: &PluginSet) -> Vec<(Arc<Active>, Msg)> {
    new.all()
        .iter()
        .filter(|p| p.enabled)
        .filter_map(|p| {
            let b = p.broken()?;
            let before = old
                .get(&p.id)
                .filter(|o| o.enabled)
                .and_then(|o| o.broken());
            if before == Some(b) {
                return None;
            }
            let why = match b {
                Broken::Changed => msg!(
                    "gw.plugin.file_changed", plugin = &p.name =>
                    "The file of plugin `{plugin}` changed on disk, so it no longer runs. Review \
                     the change and approve it in the app."
                ),
                Broken::Error(m) => m.clone(),
            };
            Some((p.clone(), why))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::fake::{FakeEngine, source};
    use serde_json::json;

    fn add_date() -> String {
        source(
            json!({"name": "附加日期", "api": 1, "permissions": ["system"],
                   "settings": {"note": {"type": "string", "label": "附加内容", "default": "今天"},
                                "days": {"type": "number", "label": "天数", "default": 1}}}),
            &["onRequest"],
        )
    }

    struct Bed {
        dir: tempfile::TempDir,
        plugins: Plugins,
    }

    impl Bed {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let plugins = Plugins::new(Arc::new(FakeEngine));
            plugins.set_dir(dir.path().to_path_buf());
            Self { dir, plugins }
        }
        /// 装一个：插件文件、底稿，返回配置里的那一条
        fn install(&self, id: &str, src: &str) -> tw_config::Plugin {
            let file = tw_config::plugins::file_path(self.dir.path(), id);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, src).unwrap();
            let approved = tw_config::plugins::approved_path(self.dir.path(), id);
            std::fs::create_dir_all(approved.parent().unwrap()).unwrap();
            std::fs::write(&approved, src).unwrap();
            tw_config::Plugin {
                id: id.into(),
                file: tw_config::Plugin::file_for(id),
                sha256: sha256_hex(src.as_bytes()),
                enabled: true,
                on_error: tw_config::PluginOnError::Reject,
                scope: Default::default(),
                settings: Default::default(),
            }
        }
        fn build(&self, plugins: Vec<tw_config::Plugin>) -> PluginSet {
            self.plugins.build(&tw_config::Config {
                plugins,
                ..Default::default()
            })
        }
    }

    #[test]
    fn an_approved_file_loads_ready_with_its_manifest_and_default_settings() {
        let bed = Bed::new();
        let set = bed.build(vec![bed.install("add-date", &add_date())]);
        let p = set.get("add-date").unwrap();
        assert!(p.ready().is_some(), "{:?}", p.state);
        assert_eq!(p.name, "附加日期");
        assert_eq!(p.permissions, [tw_api::Permission::System]);
        assert!(p.hooks.request);
        assert_eq!(p.settings["note"], json!("今天"));
        assert_eq!(p.settings["days"], json!(1));
    }

    /// I9：哈希对不上就不跑 —— 改过的代码一行都不执行，显示的还是批准的那一份
    #[test]
    fn a_file_that_no_longer_matches_its_hash_is_changed_and_does_not_run() {
        let bed = Bed::new();
        let p = bed.install("add-date", &add_date());
        let file = tw_config::plugins::file_path(bed.dir.path(), "add-date");
        std::fs::write(&file, format!("{}// 一行改动\n", add_date())).unwrap();
        let set = bed.build(vec![p]);
        let a = set.get("add-date").unwrap();
        assert_eq!(a.broken(), Some(&Broken::Changed));
        assert!(a.ready().is_none());
        // 批准过的那一份还在：名字、权限照样显示
        assert_eq!(a.name, "附加日期");
        assert_eq!(a.permissions, [tw_api::Permission::System]);
    }

    #[test]
    fn a_missing_file_is_changed_too() {
        let bed = Bed::new();
        let p = bed.install("add-date", &add_date());
        std::fs::remove_file(tw_config::plugins::file_path(bed.dir.path(), "add-date")).unwrap();
        let set = bed.build(vec![p]);
        assert_eq!(
            set.get("add-date").unwrap().broken(),
            Some(&Broken::Changed)
        );
    }

    /// 底稿也被人动过：说不出它是什么插件，只剩 id
    #[test]
    fn a_tampered_approved_copy_is_not_used_even_for_display() {
        let bed = Bed::new();
        let p = bed.install("add-date", &add_date());
        let other = add_date().replace("附加日期", "别的");
        std::fs::write(
            tw_config::plugins::file_path(bed.dir.path(), "add-date"),
            &other,
        )
        .unwrap();
        std::fs::write(
            tw_config::plugins::approved_path(bed.dir.path(), "add-date"),
            &other,
        )
        .unwrap();
        let set = bed.build(vec![p]);
        let a = set.get("add-date").unwrap();
        assert_eq!(a.broken(), Some(&Broken::Changed));
        assert_eq!(a.name, "add-date");
        assert!(a.manifest.is_none());
    }

    #[test]
    fn a_load_error_is_the_plugins_own_and_says_why() {
        let bed = Bed::new();
        let src = format!("{}// @@syntax@@\n", add_date());
        let set = bed.build(vec![bed.install("bad", &src)]);
        let Some(Broken::Error(m)) = set.get("bad").unwrap().broken() else {
            panic!("a syntax error loaded");
        };
        assert_eq!(m.code, "gw.plugin.syntax_at");
        // 两行源码之后的那一行
        assert_eq!(m.arg("line"), "3");
    }

    #[test]
    fn without_an_engine_every_plugin_fails_to_load() {
        let bed = Bed::new();
        bed.plugins
            .set_engine(Arc::new(crate::plugin::Unavailable::default()));
        let set = bed.build(vec![bed.install("add-date", &add_date())]);
        let Some(Broken::Error(m)) = set.get("add-date").unwrap().broken() else {
            panic!("loaded without an engine");
        };
        assert_eq!(m.code, "gw.plugin.engine");
    }

    #[test]
    fn without_a_directory_nothing_loads() {
        let plugins = Plugins::new(Arc::new(FakeEngine));
        let set = plugins.build(&tw_config::Config {
            plugins: vec![tw_config::Plugin {
                id: "a".into(),
                file: "plugins/a.js".into(),
                sha256: "0".repeat(64),
                enabled: true,
                on_error: Default::default(),
                scope: Default::default(),
                settings: Default::default(),
            }],
            ..Default::default()
        });
        let Some(Broken::Error(m)) = set.get("a").unwrap().broken() else {
            panic!("loaded without knowing where");
        };
        assert_eq!(m.code, "gw.plugin.not_located");
    }

    #[test]
    fn settings_have_to_be_declared_and_of_their_type() {
        let bed = Bed::new();
        let mut p = bed.install("add-date", &add_date());
        p.settings.insert("note".into(), "明天".into());
        p.settings.insert("days".into(), 3.into());
        let set = bed.build(vec![p.clone()]);
        let a = set.get("add-date").unwrap();
        assert!(a.ready().is_some(), "{:?}", a.state);
        assert_eq!(a.settings["note"], json!("明天"));
        assert_eq!(a.settings["days"], json!(3));

        let mut wrong = p.clone();
        wrong.settings.insert("days".into(), "three".into());
        let set = bed.build(vec![wrong]);
        let Some(Broken::Error(m)) = set.get("add-date").unwrap().broken() else {
            panic!("a string ran as a number");
        };
        assert_eq!(
            (m.code.as_str(), m.arg("kind")),
            ("gw.plugin.setting_type", "number")
        );

        let mut unknown = p;
        unknown.settings.insert("colour".into(), "red".into());
        let set = bed.build(vec![unknown]);
        let Some(Broken::Error(m)) = set.get("add-date").unwrap().broken() else {
            panic!("an undeclared setting ran");
        };
        assert_eq!(m.code, "gw.plugin.setting_unknown");
    }

    /// 计数和日志跨重载：改设置、批准文件不该把「跑了多少次」清零；删掉的插件跟着走
    #[test]
    fn stats_survive_a_rebuild_and_leave_with_the_plugin() {
        let bed = Bed::new();
        let p = bed.install("add-date", &add_date());
        let first = bed.build(vec![p.clone()]);
        first.get("add-date").unwrap().stats.note(
            &crate::plugin::PluginRun {
                plugin_id: "add-date".into(),
                plugin_name: "附加日期".into(),
                hook: tw_api::PluginHook::Request,
                outcome: tw_api::PluginOutcome::Changed,
                error: None,
                cpu_us: 10,
                detail: None,
            },
            1,
        );
        let again = bed.build(vec![p.clone()]);
        assert_eq!(again.get("add-date").unwrap().stats.view().calls, 1);
        bed.build(Vec::new());
        let back = bed.build(vec![p]);
        assert_eq!(back.get("add-date").unwrap().stats.view().calls, 0);
    }

    struct Counting(std::sync::atomic::AtomicUsize);

    impl Engine for Counting {
        fn load(&self, s: &[u8]) -> Result<Arc<dyn PluginHost>, LoadError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            FakeEngine.load(s)
        }
    }

    impl Counting {
        fn count(&self) -> usize {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// 同一份字节不编第二遍
    #[test]
    fn the_same_bytes_are_compiled_once() {
        let bed = Bed::new();
        let engine = Arc::new(Counting(Default::default()));
        bed.plugins.set_engine(engine.clone());
        let p = bed.install("add-date", &add_date());
        bed.build(vec![p.clone()]);
        bed.build(vec![p]);
        assert_eq!(engine.count(), 1);
    }

    /// 装之前编好的那一份，装上之后的重载直接拿来用；只是看看的不留
    #[test]
    fn a_prepared_source_is_not_compiled_again_but_an_inspected_one_is_not_kept() {
        let bed = Bed::new();
        let engine = Arc::new(Counting(Default::default()));
        bed.plugins.set_engine(engine.clone());
        let src = add_date();
        assert!(bed.plugins.inspect(src.as_bytes()).is_ok());
        assert!(bed.plugins.prepare(src.as_bytes()).is_ok());
        assert_eq!(engine.count(), 2);
        bed.build(vec![bed.install("add-date", &src)]);
        assert_eq!(engine.count(), 2, "the installed source was compiled again");
    }

    #[test]
    fn only_a_new_breakage_is_announced() {
        let bed = Bed::new();
        let p = bed.install("add-date", &add_date());
        let ok = bed.build(vec![p.clone()]);
        std::fs::write(
            tw_config::plugins::file_path(bed.dir.path(), "add-date"),
            "changed",
        )
        .unwrap();
        let changed = bed.build(vec![p.clone()]);
        let said = newly_broken(&ok, &changed);
        assert_eq!(said.len(), 1);
        assert_eq!(said[0].1.code, "gw.plugin.file_changed");
        assert_eq!(said[0].1.arg("plugin"), "附加日期");
        // 还是变了的样子：不再说
        let still = bed.build(vec![p.clone()]);
        assert!(newly_broken(&changed, &still).is_empty());
        // 停用着的不说
        let mut off = p;
        off.enabled = false;
        let off = bed.build(vec![off]);
        assert!(newly_broken(&ok, &off).is_empty());
    }

    /// 一个读不下的大文件：只读到上限多一个字节，哈希对不上，就是变了
    #[test]
    fn a_huge_file_is_read_only_up_to_the_limit() {
        let bed = Bed::new();
        let p = bed.install("add-date", &add_date());
        let file = tw_config::plugins::file_path(bed.dir.path(), "add-date");
        std::fs::write(&file, vec![b'x'; MAX_SOURCE * 3]).unwrap();
        assert_eq!(read_capped(&file).unwrap().len(), MAX_SOURCE + 1);
        let set = bed.build(vec![p]);
        assert_eq!(
            set.get("add-date").unwrap().broken(),
            Some(&Broken::Changed)
        );
    }
}
