//! 编过的插件的 manifest，存在插件目录的 `.manifests.json` 里，**只拿来显示**。
//!
//! 停用着的插件不为它起运行时（见 [`super::load::Plugins::build`]）：沙箱一起来就是几 MB
//! 常驻内存，而一个插件都没打开的用户不该为它付这个钱。插件页上要的名字、说明、权限和
//! 设置项就从这里读。
//!
//! - **键是批准的那份字节的 SHA-256**；整份文件带着运行时和这份格式的版本（[`version`]），
//!   版本对不上（core 升级、沙箱换了）就整份不认，等下一次运行时起来时重建；读不出来、
//!   写坏了也一样当作没有。没有可用的那一条时，插件只按 id 和状态列出来，**不为了列个
//!   名字去起运行时**。
//! - 只有 core 写它，和插件文件一样只给自己（0600）。
//! - **安全上的判断一律不用它**：打开插件、改改得了工具调用的插件的代码、批准它改过的文件、
//!   试跑之前，都先真的编一遍、看编出来的 manifest。它是用户目录里的一个文件，被人改了只是
//!   显示不对。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::plugin::engine::{Hooks, Manifest, SettingSpec};
use crate::plugin::set::Scope;

/// 文件名，在插件目录里。点开头：它不是插件
pub const FILE: &str = ".manifests.json";

/// 这份格式自己的版本。**manifest 的读法或者这里的写法改了就加一**（2：多了 `requests`；
/// 3：多了 `on_error`，设置的 `default` 换成了 `value`）
const FORMAT: u32 = 3;

/// 缓存认的版本：core 的版本、沙箱的哈希、这份格式的版本，三样有一样不同就不认
pub fn version() -> String {
    format!(
        "{}/{}/{FORMAT}",
        env!("CARGO_PKG_VERSION"),
        tw_plugin::GUEST_WASM_SHA256
    )
}

/// `.manifests.json` 在哪儿。`dir` 是配置文件所在的目录
pub fn path_in(dir: &Path) -> PathBuf {
    tw_config::plugins::dir_in(dir).join(FILE)
}

/// 文件里的样子
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    version: String,
    /// 批准的那份字节的 SHA-256（小写十六进制）→ manifest
    manifests: BTreeMap<String, Entry>,
}

/// 一个 manifest 写进文件的样子。**和 [`Manifest`] 一一对应**，只是带着 serde
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    name: String,
    api: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    permissions: Vec<tw_api::Permission>,
    requests: Vec<tw_api::RequestKind>,
    scope: ScopeEntry,
    on_error: tw_api::OnError,
    reply_mode: tw_api::ReplyMode,
    settings: Vec<SettingEntry>,
    hooks: HooksEntry,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeEntry {
    clients: Vec<String>,
    models: Vec<String>,
    upstreams: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingEntry {
    key: String,
    kind: tw_api::SettingKind,
    label: String,
    value: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HooksEntry {
    request: bool,
    reply_text: bool,
    reply_text_end: bool,
    tool_call: bool,
}

impl From<&Manifest> for Entry {
    fn from(m: &Manifest) -> Self {
        Self {
            name: m.name.clone(),
            api: m.api,
            description: m.description.clone(),
            permissions: m.permissions.clone(),
            requests: m.requests.clone(),
            scope: ScopeEntry {
                clients: m.scope.clients.clone(),
                models: m.scope.models.clone(),
                upstreams: m.scope.upstreams.clone(),
            },
            on_error: m.on_error,
            reply_mode: m.reply_mode,
            settings: m
                .settings
                .iter()
                .map(|s| SettingEntry {
                    key: s.key.clone(),
                    kind: s.kind,
                    label: s.label.clone(),
                    value: s.value.clone(),
                })
                .collect(),
            hooks: HooksEntry {
                request: m.hooks.request,
                reply_text: m.hooks.reply_text,
                reply_text_end: m.hooks.reply_text_end,
                tool_call: m.hooks.tool_call,
            },
        }
    }
}

impl From<&Entry> for Manifest {
    fn from(e: &Entry) -> Self {
        Self {
            name: e.name.clone(),
            api: e.api,
            description: e.description.clone(),
            permissions: e.permissions.clone(),
            requests: e.requests.clone(),
            scope: Scope {
                clients: e.scope.clients.clone(),
                models: e.scope.models.clone(),
                upstreams: e.scope.upstreams.clone(),
            },
            on_error: e.on_error,
            reply_mode: e.reply_mode,
            settings: e
                .settings
                .iter()
                .map(|s| SettingSpec {
                    key: s.key.clone(),
                    kind: s.kind,
                    label: s.label.clone(),
                    value: s.value.clone(),
                })
                .collect(),
            hooks: Hooks {
                request: e.hooks.request,
                reply_text: e.hooks.reply_text,
                reply_text_end: e.hooks.reply_text_end,
                tool_call: e.hooks.tool_call,
            },
        }
    }
}

/// 进程里的那一份，对着一个插件目录。**读一次**，之后改的时候连文件一起写
#[derive(Debug, Default)]
pub(crate) struct Cache {
    /// 对着的是哪个目录。换了目录就重新读
    dir: Option<PathBuf>,
    entries: HashMap<String, Manifest>,
}

impl Cache {
    /// 换到这个目录上（还没读过就读一次）
    fn at(&mut self, dir: &Path) {
        if self.dir.as_deref() == Some(dir) {
            return;
        }
        self.entries = read(&path_in(dir)).unwrap_or_default();
        self.dir = Some(dir.to_path_buf());
    }

    /// 这份字节的 manifest，有就给
    pub(crate) fn get(&mut self, dir: &Path, sha256: &str) -> Option<Manifest> {
        self.at(dir);
        self.entries.get(sha256).cloned()
    }

    /// 记下一个编出来的 manifest。和记着的一样就不写文件
    pub(crate) fn put(&mut self, dir: &Path, sha256: &str, m: &Manifest) {
        self.at(dir);
        if self.entries.get(sha256) == Some(m) {
            return;
        }
        self.entries.insert(sha256.to_string(), m.clone());
        self.save(dir);
    }

    /// 只留这几份字节的。**配置里不再有的插件，它的那一条跟着走**
    pub(crate) fn keep(&mut self, dir: &Path, wanted: &std::collections::HashSet<String>) {
        self.at(dir);
        let before = self.entries.len();
        self.entries.retain(|sha, _| wanted.contains(sha));
        if self.entries.len() != before {
            self.save(dir);
        }
    }

    fn save(&self, dir: &Path) {
        let stored = Stored {
            version: version(),
            manifests: self
                .entries
                .iter()
                .map(|(k, m)| (k.clone(), Entry::from(m)))
                .collect(),
        };
        let path = path_in(dir);
        let written = serde_json::to_vec_pretty(&stored)
            .map_err(std::io::Error::other)
            .and_then(|mut bytes| {
                bytes.push(b'\n');
                tw_config::private_dir::create(&tw_config::plugins::dir_in(dir))?;
                write_private(&path, &bytes)
            });
        // 写不成只是下一次启动要多起一次运行时才列得全，不影响别的
        if let Err(e) = written {
            tracing::debug!(file = %path.display(), "the plugin manifest cache could not be written: {e}");
        }
    }
}

/// 读一份缓存。**版本对不上、读不出来、写坏了都是没有**
fn read(path: &Path) -> Option<HashMap<String, Manifest>> {
    let bytes = std::fs::read(path).ok()?;
    let stored: Stored = serde_json::from_slice(&bytes).ok()?;
    if stored.version != version() {
        return None;
    }
    Some(
        stored
            .manifests
            .iter()
            .filter(|(sha, _)| tw_config::plugins::valid_sha256(sha))
            .map(|(sha, e)| (sha.clone(), Manifest::from(e)))
            .collect(),
    )
}

/// 一个 manifest 写成 JSON（默认插件预先算好的那一份就是这么生成的）
#[cfg(test)]
pub(crate) fn to_json(m: &Manifest) -> serde_json::Value {
    serde_json::to_value(Entry::from(m)).unwrap_or_default()
}

/// 从 JSON 读回一个 manifest
pub(crate) fn from_json(v: &serde_json::Value) -> Option<Manifest> {
    let e: Entry = serde_json::from_value(v.clone()).ok()?;
    Some(Manifest::from(&e))
}

/// 原子地写一个只给自己看的文件：建的那一刻就是 0600，写完再改名过去
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let written = opts.open(&tmp).and_then(|mut f| {
        f.write_all(bytes)?;
        f.sync_all()
    });
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            name: "附加日期".into(),
            api: 1,
            description: Some("在系统提示里写上今天的日期".into()),
            permissions: vec![tw_api::Permission::System],
            requests: vec![tw_api::RequestKind::Conversation],
            scope: Scope {
                clients: vec![],
                models: vec!["deepseek*".into()],
                upstreams: vec![],
            },
            on_error: tw_api::OnError::Skip,
            reply_mode: tw_api::ReplyMode::Block,
            settings: vec![SettingSpec {
                key: "note".into(),
                kind: tw_api::SettingKind::String,
                label: "附加内容".into(),
                value: "第一行\n第二行".into(),
            }],
            hooks: Hooks {
                request: true,
                ..Default::default()
            },
        }
    }

    const SHA: &str = "6f1c000000000000000000000000000000000000000000000000000000000abc";

    /// 写下去、换一个进程（新的一份）读回来，一模一样；文件只给自己
    #[test]
    fn a_manifest_written_once_reads_back_in_the_next_process() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Cache::default();
        c.put(dir.path(), SHA, &manifest());
        let mut again = Cache::default();
        assert_eq!(again.get(dir.path(), SHA), Some(manifest()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path_in(dir.path()))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    /// 版本不对、写坏了：整份不认
    #[test]
    fn another_version_or_a_broken_file_is_no_cache_at_all() {
        let dir = tempfile::tempdir().unwrap();
        Cache::default().put(dir.path(), SHA, &manifest());
        let path = path_in(dir.path());
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace(&version(), "0.0.0/old/1")).unwrap();
        assert_eq!(Cache::default().get(dir.path(), SHA), None);
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(Cache::default().get(dir.path(), SHA), None);
    }

    /// 配置里不再有的插件，它那一条跟着走
    #[test]
    fn only_the_hashes_still_in_use_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Cache::default();
        let other = SHA.replace("abc", "def");
        c.put(dir.path(), SHA, &manifest());
        c.put(dir.path(), &other, &manifest());
        c.keep(dir.path(), &[other.clone()].into_iter().collect());
        let mut again = Cache::default();
        assert_eq!(again.get(dir.path(), SHA), None);
        assert!(again.get(dir.path(), &other).is_some());
    }

    #[test]
    fn json_round_trips() {
        let m = manifest();
        assert_eq!(from_json(&to_json(&m)), Some(m));
    }
}
