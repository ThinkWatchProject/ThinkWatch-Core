//! 这台设备自己记着的同步状态：数据目录里的 `sync/`。
//!
//! ```text
//! sync/
//!   base.twconfig    上次同步的那一版（合并基准），不加密，不计入本机保留的 10 个版本
//!   bases/           之前的几个基准（最近 8 个，按版本标识）：同步位置上早删了的共同上一版本
//!                    从这里找
//!   keyring          同步位置上的 keyring 最近一次解得开的那一份（以口令加密，原样）：同步位置
//!                    上的丢了、被换成旧的时，由人决定放回去
//!   state.json       上次同步的时间、失败从什么时候开始、因同步停用的插件和上游、地址经同步
//!                    变更的上游、读不出来的文件
//! ```
//!
//! 同步位置上的版本被删掉（设备离线期间）不影响合并：基准在这里。停止同步时整个目录删掉。

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tw_types::{Msg, msg};

pub const DIR: &str = "sync";
const BASE: &str = "base.twconfig";
const BASES: &str = "bases";
const KEYRING: &str = "keyring";
const STATE: &str = "state.json";
/// `bases/` 里留几个
const KEEP_BASES: usize = 8;

/// `state.json`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// 这份状态是对着哪个同步位置的（[`place`]）。换了位置就作废
    pub place: String,
    /// 基准是哪一版（版本标识）
    pub base: Option<String>,
    pub last_synced_ms: Option<u64>,
    /// 从这个时刻起一直没同步成（连续失败）。成功一次就清掉
    pub failing_since_ms: Option<u64>,
    /// 最近一次失败的原因
    pub error: Option<Msg>,
    /// 不加密时经同步新增、或者脚本变了的插件，在这台设备上停用着：插件 id → 停用时的脚本
    /// （合并单位的值）。在本机启用之后去掉
    pub held_plugins: BTreeMap<String, Value>,
    /// 不加密时经同步改了地址的上游：名字 → 改之前和改之后的地址
    pub url_changed: BTreeMap<String, UrlChange>,
    /// 上次写自己的设备记录是什么时候
    pub device_record_ms: u64,
    /// 上次写设备记录时是不是暂停着（版本过新）
    pub device_record_paused: bool,
    /// 等这台设备登录的上游（名字）：别的设备上登录着、这台没有，于是在这台停用着。停用是
    /// 这台自己的事，不上传；登录之后照同步的样子启用
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub held_login: BTreeSet<String>,
    /// 见过的 keyring 的最大代号（[`crate::keyring::Keyring::generation`]）
    pub keyring_gen: u64,
    /// 同步位置上读不出来的文件：名字 → 第一次读不出来的时刻。读出来了就去掉
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub unreadable: BTreeMap<String, u64>,
    /// 同步位置看上去是空的：从什么时候起、连着几轮
    pub empty_since_ms: Option<u64>,
    pub empty_rounds: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UrlChange {
    pub from: String,
    pub to: String,
    /// 改它的设备的名称
    #[serde(default)]
    pub device: Option<String>,
    #[serde(default)]
    pub at_ms: u64,
}

/// 一个同步位置的指纹：种类加地址，不含凭据（换了密码还是同一个位置）
pub fn place(target: &tw_config::sync::Target) -> String {
    use tw_config::sync::Target;
    match target {
        Target::Folder { path, .. } => format!("folder:{}", path.trim_end_matches(['/', '\\'])),
        Target::Webdav { url, .. } => format!("webdav:{}", url.trim_end_matches('/')),
        Target::S3 {
            endpoint,
            bucket,
            prefix,
            ..
        } => format!(
            "s3:{}/{bucket}/{}",
            endpoint.trim_end_matches('/'),
            prefix.trim_matches('/')
        ),
    }
}

fn io(path: &Path, e: std::io::Error) -> Msg {
    msg!(
        "sync.local.io", path = path.display(), detail = e =>
        "{path} could not be used: {detail}"
    )
}

/// 原子写一个只有本机用户读得到的文件
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), Msg> {
    let dir = path.parent().unwrap_or(Path::new("."));
    tw_config::private_dir::create(dir).map_err(|e| io(dir, e))?;
    let r: u64 = rand::random();
    let tmp = dir.join(format!(".tmp-{r:016x}"));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let res = opts
        .open(&tmp)
        .and_then(|mut f| f.write_all(bytes).and_then(|()| f.sync_all()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(io(path, e));
    }
    Ok(())
}

/// 这台设备的同步状态目录
#[derive(Debug, Clone)]
pub struct Local {
    dir: PathBuf,
}

impl Local {
    /// `data_dir` 是配置文件所在的目录
    pub fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join(DIR),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn state(&self) -> State {
        std::fs::read(self.dir.join(STATE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save_state(&self, s: &State) -> Result<(), Msg> {
        write_private(
            &self.dir.join(STATE),
            &serde_json::to_vec_pretty(s).expect("plain data serializes"),
        )
    }

    /// 基准版本的字节（不加密的配置包）。没有是 `None`
    pub fn base(&self) -> Option<Vec<u8>> {
        std::fs::read(self.dir.join(BASE)).ok()
    }

    /// 换基准。`id` 是它的版本标识：同时留一份在 `bases/`，只留最近的几个
    pub fn save_base(&self, id: &str, bytes: &[u8]) -> Result<(), Msg> {
        write_private(&self.dir.join(BASE), bytes)?;
        let dir = self.dir.join(BASES);
        write_private(&dir.join(format!("{id}.twconfig")), bytes)?;
        let mut all: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".twconfig"))
                    .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
                    .collect()
            })
            .unwrap_or_default();
        all.sort();
        let extra = all.len().saturating_sub(KEEP_BASES);
        for (_, p) in all.into_iter().take(extra) {
            let _ = std::fs::remove_file(p);
        }
        Ok(())
    }

    /// 之前的基准里版本标识是 `id` 的那一个
    pub fn base_by_id(&self, id: &str) -> Option<Vec<u8>> {
        if id.contains(['/', '\\']) || id.starts_with('.') {
            return None;
        }
        std::fs::read(self.dir.join(BASES).join(format!("{id}.twconfig"))).ok()
    }

    /// 记着的 keyring（同步位置上最近一次解得开的那一份）
    pub fn keyring(&self) -> Option<Vec<u8>> {
        std::fs::read(self.dir.join(KEYRING)).ok()
    }

    pub fn save_keyring(&self, bytes: &[u8]) -> Result<(), Msg> {
        if self.keyring().as_deref() == Some(bytes) {
            return Ok(());
        }
        write_private(&self.dir.join(KEYRING), bytes)
    }

    /// 停止同步：整个目录删掉
    pub fn clear(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_and_base_survive_a_restart_and_are_private() {
        let d = tempfile::tempdir().unwrap();
        let l = Local::new(d.path());
        assert_eq!(l.state(), State::default());
        assert!(l.base().is_none());
        let s = State {
            place: "folder:/x".into(),
            base: Some("0000000001-aaaaaaaa".into()),
            last_synced_ms: Some(5),
            ..Default::default()
        };
        l.save_state(&s).unwrap();
        l.save_base("0000000001-aaaaaaaa", b"PK").unwrap();
        let again = Local::new(d.path());
        assert_eq!(again.state(), s);
        assert_eq!(again.base().as_deref(), Some(&b"PK"[..]));
        assert_eq!(
            again.base_by_id("0000000001-aaaaaaaa").as_deref(),
            Some(&b"PK"[..])
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.path().join("sync/base.twconfig"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        again.clear();
        assert!(!d.path().join("sync").exists());
    }

    #[test]
    fn only_the_latest_few_bases_are_kept() {
        let d = tempfile::tempdir().unwrap();
        let l = Local::new(d.path());
        for i in 1..=12u64 {
            l.save_base(&format!("{i:010}-aaaaaaaa"), format!("v{i}").as_bytes())
                .unwrap();
            // 修改时间要分得出先后
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(l.base_by_id("0000000001-aaaaaaaa").is_none());
        assert!(l.base_by_id("0000000004-aaaaaaaa").is_none());
        assert_eq!(
            l.base_by_id("0000000005-aaaaaaaa").as_deref(),
            Some(&b"v5"[..])
        );
        assert_eq!(
            l.base_by_id("0000000012-aaaaaaaa").as_deref(),
            Some(&b"v12"[..])
        );
        assert!(l.base_by_id("../state.json").is_none());
    }

    #[test]
    fn the_place_ignores_credentials() {
        use tw_config::sync::Target;
        let a = Target::Webdav {
            url: "https://h/dav/".into(),
            user: "a".into(),
            pass: "1".into(),
            proxy: "direct".into(),
        };
        let b = Target::Webdav {
            url: "https://h/dav".into(),
            user: "b".into(),
            pass: "2".into(),
            proxy: "corp".into(),
        };
        assert_eq!(place(&a), place(&b));
    }
}
