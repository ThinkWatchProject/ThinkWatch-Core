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
//!
//! **一个插件都没打开时不起运行时**：沙箱一起来就是几 MB 常驻内存，而 core 自带的默认
//! 插件装上时都停用着 —— 一个插件都没打开的用户不该为它付这个钱。这时停用的插件照样读
//! 文件、算哈希，但不编：它们是「休眠」的（[`PluginHost::dormant`]，跑不了任何钩子），
//! 列表上显示的 manifest 来自缓存（[`super::manifests`]，只拿来显示），缓存里没有就只有
//! id。有一个插件开着，运行时反正要起，所有插件照常编，缓存跟着补齐。
//!
//! **出错时怎么办、范围、设置的值都在插件文件里**（契约附录四），配置里只有 id、文件、
//! 哈希和开关。所以它们跟着 manifest 走：编得出来就是编出来的那一份；编不出来（运行时
//! 起不来、新版 core 不认它的写法）就照批准的那份字节里写着的读（manifest 是纯数据，
//! 不编也读得出来，见 [`super::source::declared`]）；连那也读不出来，才按出厂的：出错时
//! 拒绝、什么请求都管。

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use sha2::{Digest, Sha256};
use tw_types::{Msg, msg};

use crate::plugin::engine::{Engine, LoadError, MAX_SOURCE, Manifest};
use crate::plugin::host::PluginHost;
use crate::plugin::manifests;
use crate::plugin::set::{Active, Broken, LogRing, PluginSet, Scope, State, Stats};
use crate::plugin::source;

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
    /// 编过的 manifest 的缓存（[`manifests`]），**只拿来显示**休眠的插件
    shown: Mutex<manifests::Cache>,
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
            shown: Mutex::default(),
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

    /// 记下一个编出来的 manifest，给以后显示休眠的插件用（[`manifests`]）。配置里的插件编
    /// 过之后 [`Self::build`] 自己会记；**默认插件那一路不编**（它带着预先算好的
    /// manifest），装上之前从这里记一笔
    pub fn remember(&self, sha256: &str, m: &Manifest) {
        if let Some(dir) = self.dir() {
            self.shown
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .put(&dir, sha256, m);
        }
    }

    /// 照这份配置建一份插件。**不会失败**：哪个插件有问题，问题落在它自己的状态上。
    pub fn build(&self, config: &tw_config::Config) -> PluginSet {
        let dir = self.dir();
        let engine = self.engine();
        let mut used = HashSet::new();
        let mut out = Vec::with_capacity(config.plugins.len());
        // 有一个开着，运行时反正要起：全都编。一个都没开：停用的只读文件、不编（休眠）
        let awake = config.plugins.iter().any(|p| p.enabled);
        for p in &config.plugins {
            let track = self.track(&p.id);
            let loaded = match dir.as_deref() {
                Some(dir) if awake || p.enabled => {
                    let loaded = self.load_one(dir, p, &*engine, &mut used);
                    // 编出来的记一笔：之后（比如下一次启动）它停用着、运行时没起时，列表
                    // 照样说得出它是什么
                    if let Some(m) = &loaded.manifest {
                        self.remember(&p.sha256, m);
                    }
                    loaded
                }
                Some(dir) => self.dormant_one(dir, p, &mut used),
                None => Loaded::broken(Broken::Error(not_located())),
            };
            let m = loaded.manifest.as_ref();
            // 出错时怎么办、范围：manifest 里的；读不出 manifest 就照批准的那份字节里写着的，
            // 再不行按出厂的（见模块说明）
            let (on_error, scope) = match m {
                Some(m) => (m.on_error, m.scope.clone()),
                None => loaded
                    .declared
                    .clone()
                    .unwrap_or((tw_api::OnError::Reject, Scope::default())),
            };
            out.push(Arc::new(Active {
                id: p.id.clone(),
                name: m.map_or_else(|| p.id.clone(), |m| m.name.clone()),
                enabled: p.enabled,
                on_error,
                scope,
                permissions: m.map(|m| m.permissions.clone()).unwrap_or_default(),
                // 读不出 manifest 的按不写 `requests` 的算（见 `Active::requests`）
                requests: m.map_or_else(
                    || crate::plugin::engine::DEFAULT_REQUESTS.to_vec(),
                    |m| m.requests.clone(),
                ),
                reply_mode: m.map_or(tw_api::ReplyMode::Block, |m| m.reply_mode),
                hooks: m.map(|m| m.hooks).unwrap_or_default(),
                settings: m.map(values_of).unwrap_or_default(),
                manifest: loaded.manifest,
                state: loaded.state,
                stats: track.stats,
                logs: track.logs,
            }));
        }
        // 只留这一份还用得着的：编译结果、计数和日志、显示用的 manifest。删掉的插件，
        // 它的计数跟着走
        self.compiled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|k, _| used.contains(k));
        if let Some(dir) = dir.as_deref() {
            let approved: HashSet<String> =
                config.plugins.iter().map(|p| p.sha256.clone()).collect();
            self.shown
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .keep(dir, &approved);
        }
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
    ) -> Loaded {
        let path = p.path_in(dir);
        let bytes = match read_capped(&path) {
            Ok(b) => b,
            // 文件没了也是「变了」：批准过的那一份不在原处了
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return self.approved(dir, p, engine, used, Broken::Changed);
            }
            Err(e) => {
                return self.approved(
                    dir,
                    p,
                    engine,
                    used,
                    Broken::Error(msg!(
                        "gw.plugin.unreadable", file = &p.file, detail = e =>
                        "The plugin file {file} cannot be read: {detail}"
                    )),
                );
            }
        };
        let sha: [u8; 32] = Sha256::digest(&bytes).into();
        if hex(&sha) != p.sha256 {
            return self.approved(dir, p, engine, used, Broken::Changed);
        }
        used.insert(sha);
        match self.compile(engine, sha, &bytes) {
            Ok(host) => Loaded {
                manifest: Some(host.manifest().clone()),
                state: State::Ready(host),
                declared: None,
            },
            Err(e) => Loaded {
                state: State::Broken(Broken::Error(e.msg())),
                manifest: None,
                // 编不出来：文件里写着的照样算数
                declared: source::declared(&bytes),
            },
        }
    }

    /// 停用着、运行时没起的一个插件：**不编**。文件照样读、哈希照样比（「文件变了」照样
    /// 查得出来）；这个进程里编过的直接拿来用，没编过的是休眠的，显示用的 manifest 从缓存
    /// 里拿，缓存里没有就只有 id（出错时怎么办和范围照文件里写着的）
    fn dormant_one(
        &self,
        dir: &Path,
        p: &tw_config::Plugin,
        used: &mut HashSet<[u8; 32]>,
    ) -> Loaded {
        let approved = unhex(&p.sha256);
        let compiled = approved.and_then(|sha| {
            let cache = self.compiled.lock().unwrap_or_else(PoisonError::into_inner);
            match cache.get(&sha) {
                Some(Ok(host)) => Some((sha, host.clone())),
                _ => None,
            }
        });
        let shown = match &compiled {
            Some((sha, host)) => {
                used.insert(*sha);
                self.remember(&p.sha256, host.manifest());
                Some(host.manifest().clone())
            }
            None => self
                .shown
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(dir, &p.sha256),
        };
        let path = p.path_in(dir);
        let bytes = match read_capped(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Loaded::shown(Broken::Changed, shown);
            }
            Err(e) => {
                return Loaded::shown(
                    Broken::Error(msg!(
                        "gw.plugin.unreadable", file = &p.file, detail = e =>
                        "The plugin file {file} cannot be read: {detail}"
                    )),
                    shown,
                );
            }
        };
        let sha: [u8; 32] = Sha256::digest(&bytes).into();
        if hex(&sha) != p.sha256 {
            return Loaded::shown(Broken::Changed, shown);
        }
        if let Some((_, host)) = compiled {
            return Loaded {
                state: State::Ready(host),
                manifest: shown,
                declared: None,
            };
        }
        let declared = shown.is_none().then(|| source::declared(&bytes)).flatten();
        let host = Dormant {
            manifest: shown.clone().unwrap_or_else(|| placeholder(&p.id)),
            sha256: sha,
        };
        Loaded {
            state: State::Ready(Arc::new(host)),
            manifest: shown,
            declared,
        }
    }

    /// 文件变了（或者读不了）时，批准过的那一份：它的 manifest **只拿来显示**（名字、权限、
    /// 设置项）和定它管哪些请求、出了错怎么办，不跑。底稿也不是那一份了（被人动过、没了）
    /// 就什么都没有。
    fn approved(
        &self,
        dir: &Path,
        p: &tw_config::Plugin,
        engine: &dyn Engine,
        used: &mut HashSet<[u8; 32]>,
        why: Broken,
    ) -> Loaded {
        let state = State::Broken(why);
        let Some(bytes) = read_capped(&tw_config::plugins::approved_path(dir, &p.id))
            .ok()
            .filter(|b| sha256_hex(b) == p.sha256)
        else {
            return Loaded {
                state,
                manifest: None,
                declared: None,
            };
        };
        let sha: [u8; 32] = Sha256::digest(&bytes).into();
        used.insert(sha);
        let manifest = self
            .compile(engine, sha, &bytes)
            .ok()
            .map(|host| host.manifest().clone());
        let declared = manifest
            .is_none()
            .then(|| source::declared(&bytes))
            .flatten();
        Loaded {
            state,
            manifest,
            declared,
        }
    }

    fn compile(&self, engine: &dyn Engine, sha: [u8; 32], bytes: &[u8]) -> Compiled {
        let mut cache = self.compiled.lock().unwrap_or_else(PoisonError::into_inner);
        cache
            .entry(sha)
            .or_insert_with(|| engine.load(bytes))
            .clone()
    }
}

/// 一个插件读下来的样子（[`Plugins::build`] 用）
struct Loaded {
    state: State,
    /// 编出来的（或者缓存里的）manifest
    manifest: Option<Manifest>,
    /// 读不出 manifest 时，批准的那份字节里写着的出错时怎么办和范围
    declared: Option<(tw_api::OnError, Scope)>,
}

impl Loaded {
    fn broken(why: Broken) -> Self {
        Self {
            state: State::Broken(why),
            manifest: None,
            declared: None,
        }
    }

    fn shown(why: Broken, manifest: Option<Manifest>) -> Self {
        Self {
            state: State::Broken(why),
            manifest,
            declared: None,
        }
    }
}

/// 交给插件的设置：manifest 里每个设置此刻的值
pub fn values_of(m: &Manifest) -> serde_json::Map<String, serde_json::Value> {
    m.settings
        .iter()
        .map(|s| (s.key.clone(), s.value.clone()))
        .collect()
}

/// 停用着、这个进程里还没编过的插件（见 [`Plugins::build`]）。**跑不了任何钩子**（
/// [`PluginHost`] 的默认实现一律报错）：要它跑之前，调用方先真的编一遍。手里的 manifest
/// 是缓存里的那一份或者只有名字的占位，**只拿来显示**
struct Dormant {
    manifest: Manifest,
    sha256: [u8; 32],
}

impl PluginHost for Dormant {
    fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    fn sha256(&self) -> [u8; 32] {
        self.sha256
    }
    fn dormant(&self) -> bool {
        true
    }
}

/// 缓存里没有它的 manifest 时的占位：名字就是 id，什么权限、钩子都没有
fn placeholder(id: &str) -> Manifest {
    Manifest {
        name: id.to_string(),
        api: 1,
        description: None,
        permissions: Vec::new(),
        requests: crate::plugin::engine::DEFAULT_REQUESTS.to_vec(),
        scope: Scope::default(),
        on_error: tw_api::OnError::Reject,
        reply_mode: tw_api::ReplyMode::Block,
        settings: Vec::new(),
        hooks: Default::default(),
    }
}

/// 小写十六进制的 SHA-256 读回字节。写法不对是 None
fn unhex(s: &str) -> Option<[u8; 32]> {
    if !tw_config::plugins::valid_sha256(s) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
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

fn not_located() -> Msg {
    msg!(
        "gw.plugin.not_located" =>
        "The plugin files cannot be found: the gateway has not been told where its configuration \
         lives."
    )
}

/// 插件文件和批准过的那一份不一样了。通知里说它，跳过、拒掉的那一次运行上记的也是它
pub fn file_changed(plugin: &str) -> Msg {
    msg!(
        "gw.plugin.file_changed", plugin = plugin =>
        "The file of plugin `{plugin}` changed on disk, so it no longer runs. Review \
         the change and approve it in the app."
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
                Broken::Changed => file_changed(&p.name),
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
                   "settings": {"note": {"type": "string", "label": "附加内容", "value": "今天"},
                                "days": {"type": "number", "label": "天数", "value": 1}}}),
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
            }],
            ..Default::default()
        });
        let Some(Broken::Error(m)) = set.get("a").unwrap().broken() else {
            panic!("loaded without knowing where");
        };
        assert_eq!(m.code, "gw.plugin.not_located");
    }

    /// 出错时怎么办、范围、设置的值都是插件文件里写着的
    #[test]
    fn on_error_scope_and_settings_come_from_the_file() {
        let bed = Bed::new();
        let src = source(
            json!({"name": "附加日期", "api": 1, "permissions": ["system"],
                   "match": {"models": ["claude-*"], "upstreams": ["relay"]},
                   "on_error": "skip",
                   "settings": {"note": {"type": "string", "label": "附加内容", "value": "明天"},
                                "days": {"type": "number", "label": "天数"}}}),
            &["onRequest"],
        );
        let set = bed.build(vec![bed.install("add-date", &src)]);
        let a = set.get("add-date").unwrap();
        assert!(a.ready().is_some(), "{:?}", a.state);
        assert_eq!(a.on_error, tw_api::OnError::Skip);
        assert_eq!(a.scope.models, ["claude-*"]);
        assert_eq!(a.scope.upstreams, ["relay"]);
        assert_eq!(a.settings["note"], json!("明天"));
        assert_eq!(a.settings["days"], json!(0));
    }

    /// 编不出来（运行时起不来）：出错时怎么办和范围照批准的那份字节里写着的读，不编也读得
    /// 出来 —— 不因为编不了就变成「什么都管、一律拒绝」。文件变了时照底稿里的
    #[test]
    fn a_plugin_that_does_not_compile_keeps_the_on_error_and_scope_its_file_declares() {
        let bed = Bed::new();
        let src = source(
            json!({"name": "x", "api": 1, "permissions": ["system"],
                   "match": {"models": ["gpt-*"]}, "on_error": "skip"}),
            &["onRequest"],
        );
        let p = bed.install("x", &src);
        bed.plugins
            .set_engine(Arc::new(crate::plugin::Unavailable::default()));
        let set = bed.build(vec![p.clone()]);
        let a = set.get("x").unwrap();
        assert!(
            matches!(a.broken(), Some(Broken::Error(_))),
            "{:?}",
            a.state
        );
        assert!(a.manifest.is_none());
        assert_eq!(a.on_error, tw_api::OnError::Skip);
        assert_eq!(a.scope.models, ["gpt-*"]);

        // 文件被改了：照底稿（批准的那一份）
        std::fs::write(
            tw_config::plugins::file_path(bed.dir.path(), "x"),
            "export const manifest = { on_error: \"reject\" };",
        )
        .unwrap();
        let set = bed.build(vec![p.clone()]);
        let a = set.get("x").unwrap();
        assert_eq!(a.broken(), Some(&Broken::Changed));
        assert_eq!(a.on_error, tw_api::OnError::Skip);
        assert_eq!(a.scope.models, ["gpt-*"]);

        // 底稿也对不上：说不出来，按出厂的
        std::fs::write(
            tw_config::plugins::approved_path(bed.dir.path(), "x"),
            "export const manifest = { on_error: \"skip\" };",
        )
        .unwrap();
        let set = bed.build(vec![p]);
        let a = set.get("x").unwrap();
        assert_eq!(a.on_error, tw_api::OnError::Reject);
        assert_eq!(a.scope, Scope::default());
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

    fn off(mut p: tw_config::Plugin) -> tw_config::Plugin {
        p.enabled = false;
        p
    }

    /// 一个插件都没开：停用的不编（不起运行时），休眠着；缓存里没有就只有 id
    #[test]
    fn with_nothing_enabled_a_disabled_plugin_is_not_compiled() {
        let bed = Bed::new();
        let engine = Arc::new(Counting(Default::default()));
        bed.plugins.set_engine(engine.clone());
        let set = bed.build(vec![off(bed.install("add-date", &add_date()))]);
        assert_eq!(engine.count(), 0);
        let a = set.get("add-date").unwrap();
        let host = a.ready().expect("a dormant plugin is not broken");
        assert!(host.dormant());
        assert_eq!(a.name, "add-date");
        assert!(a.manifest.is_none() && a.permissions.is_empty());
        // 跑不了：钩子一律报错
        assert!(host.on_request(json!({}), json!({})).result.is_err());
    }

    /// 编过一次的记在缓存里：下一个进程里它停用着，列表照样说得出它是什么，而且不编
    #[test]
    fn a_dormant_plugin_shows_the_manifest_remembered_from_an_earlier_compile() {
        let bed = Bed::new();
        let p = bed.install("add-date", &add_date());
        bed.build(vec![p.clone()]);
        let next = Plugins::new(Arc::new(Counting(Default::default())));
        next.set_dir(bed.dir.path().to_path_buf());
        let set = next.build(&tw_config::Config {
            plugins: vec![off(p)],
            ..Default::default()
        });
        let a = set.get("add-date").unwrap();
        assert!(a.ready().unwrap().dormant());
        assert_eq!(a.name, "附加日期");
        assert_eq!(a.permissions, [tw_api::Permission::System]);
        assert_eq!(a.settings["note"], json!("今天"));
    }

    /// 休眠的插件、缓存里没有：只有 id，可出错时怎么办和范围照文件里写着的说得出来
    #[test]
    fn a_dormant_plugin_without_a_cached_manifest_still_shows_its_on_error_and_scope() {
        let bed = Bed::new();
        let src = source(
            json!({"name": "x", "api": 1, "permissions": ["system"],
                   "match": {"clients": ["codex"]}, "on_error": "skip"}),
            &["onRequest"],
        );
        let engine = Arc::new(Counting(Default::default()));
        bed.plugins.set_engine(engine.clone());
        let set = bed.build(vec![off(bed.install("x", &src))]);
        assert_eq!(engine.count(), 0);
        let a = set.get("x").unwrap();
        assert!(a.ready().unwrap().dormant());
        assert!(a.manifest.is_none());
        assert_eq!(a.on_error, tw_api::OnError::Skip);
        assert_eq!(a.scope.clients, ["codex"]);
    }

    /// 有一个开着，运行时反正要起：全都编，停用的也编（缓存跟着补齐）
    #[test]
    fn once_one_plugin_is_enabled_every_plugin_is_compiled() {
        let bed = Bed::new();
        let engine = Arc::new(Counting(Default::default()));
        bed.plugins.set_engine(engine.clone());
        let other = add_date().replace("附加日期", "另一个");
        let set = bed.build(vec![
            off(bed.install("add-date", &add_date())),
            bed.install("other", &other),
        ]);
        assert_eq!(engine.count(), 2);
        let a = set.get("add-date").unwrap();
        assert!(!a.ready().unwrap().dormant());
        assert_eq!(a.name, "附加日期");
    }

    /// 休眠的插件文件变了：照样是「变了」（只算哈希，不编），显示的是缓存里批准的那一份
    #[test]
    fn a_dormant_plugin_whose_file_changed_is_changed_without_compiling() {
        let bed = Bed::new();
        let p = bed.install("add-date", &add_date());
        bed.plugins.remember(&p.sha256, &add_date_manifest());
        let engine = Arc::new(Counting(Default::default()));
        bed.plugins.set_engine(engine.clone());
        std::fs::write(
            tw_config::plugins::file_path(bed.dir.path(), "add-date"),
            "changed",
        )
        .unwrap();
        let set = bed.build(vec![off(p)]);
        let a = set.get("add-date").unwrap();
        assert_eq!(a.broken(), Some(&Broken::Changed));
        assert_eq!(a.name, "附加日期");
        assert_eq!(engine.count(), 0);
    }

    /// 缓存里是另一份字节的（插件换过源码）：不拿来冒充
    #[test]
    fn a_cached_manifest_of_other_bytes_is_not_used() {
        let bed = Bed::new();
        let old = bed.install("add-date", &add_date());
        bed.plugins.remember(&old.sha256, &add_date_manifest());
        let newer = add_date().replace("附加日期", "新的一版");
        let p = bed.install("add-date", &newer);
        let set = bed.build(vec![off(p)]);
        let a = set.get("add-date").unwrap();
        assert_eq!(a.name, "add-date");
        assert!(a.manifest.is_none());
    }

    fn add_date_manifest() -> Manifest {
        FakeEngine
            .load(add_date().as_bytes())
            .unwrap()
            .manifest()
            .clone()
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
