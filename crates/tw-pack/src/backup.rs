//! 本机备份：每次配置变更留一个版本，最近 [`KEEP`] 个。取代以前的修改记录。
//!
//! # 一版是什么
//!
//! 一个**完整的配置包**，不加密：配置原文（本机设置、ChatGPT 的登录都在里面，和 config.yaml
//! 本身的保护级别一样），配置里列着的插件的文件，Lite 的通用偏好。**控制面的钥匙不在里面**：
//! 存的是打码的原文 —— 恢复时本机设置本来就留着现在的。
//!
//! 文件在数据目录的 `history/` 里，名字是 `<毫秒时间戳>-<来源>.twconfig`，时间戳就是这一版
//! 的标识。目录 0700、文件 0600。
//!
//! # 什么时候留
//!
//! 每个写入配置的地方在写之前（[`record_before`]）和生效之后（[`record`]）各问一次。和最新
//! 那一版内容一样就不留：效果是「每个存在过的版本各一条」，最新那条就是现在跑着的。
//!
//! - **写之前那一次只看配置原文**：插件的写法是先写文件、再写配置，写之前那一刻磁盘上的插件
//!   已经是新的了 —— 拿旧的原文配新的插件存一版，那一版从来没有存在过
//! - **凭据轮换不留**（`Origin::Rotation`）：恢复到一次轮换之前，拿到的是一个已经作废的
//!   token。比较时 OAuth 的 token 也不算：轮换之后的下一次修改不会因此多出一版
//! - 以前的修改记录（`history/*.yaml`）在第一次留版本时删掉

use std::path::{Path, PathBuf};

use tw_config::history::Origin;
use tw_types::{Msg, msg};

use crate::{Pack, PackError, PackOrigin, Seal, Unseal, disk};

/// 本机留多少版
pub const KEEP: usize = 10;

/// 备份目录，相对配置文件所在的目录
pub const DIR: &str = "history";

/// 一个备份版本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backup {
    /// 标识：毫秒时间戳，严格递增
    pub id: String,
    pub at_ms: u64,
    pub origin: Origin,
    pub path: PathBuf,
}

/// 备份读写失败。**英文只写一遍**：`Display` 就是 [`BackupError::msg`] 的原句。
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("{}", self.msg())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{}", self.msg())]
    NotFound { id: String },
    #[error("{}", self.msg())]
    Pack(PackError),
}

impl BackupError {
    pub fn msg(&self) -> Msg {
        match self {
            BackupError::Io { path, source } => msg!(
                "backup.io", path = path.display(), detail = source =>
                "{path} could not be read or written: {detail}"
            ),
            BackupError::NotFound { id } => msg!(
                "backup.not_found", id = id =>
                "There is no backup version {id}."
            ),
            BackupError::Pack(e) => e.msg(),
        }
    }
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> BackupError + '_ {
    move |source| BackupError::Io {
        path: path.to_path_buf(),
        source,
    }
}

pub fn dir(config_path: &Path) -> PathBuf {
    disk::dir_of(config_path).join(DIR)
}

/// 全部版本，**从旧到新**。目录还不在是空的
pub fn list(config_path: &Path) -> Result<Vec<Backup>, BackupError> {
    let d = dir(config_path);
    let rd = match std::fs::read_dir(&d) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io(&d)(e)),
    };
    let mut out: Vec<Backup> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let stem = name.strip_suffix(&format!(".{}", crate::EXTENSION))?;
            let (at, origin) = stem.split_once('-')?;
            let at_ms: u64 = at.parse().ok()?;
            Some(Backup {
                id: at_ms.to_string(),
                at_ms,
                origin: Origin::parse(origin),
                path: e.path(),
            })
        })
        .collect();
    // 按时间戳排，不按名字的字典序：位数会变
    out.sort_by_key(|b| b.at_ms);
    Ok(out)
}

/// 按标识找一版
pub fn find(config_path: &Path, id: &str) -> Result<Backup, BackupError> {
    list(config_path)?
        .into_iter()
        .find(|b| b.id == id)
        .ok_or_else(|| BackupError::NotFound { id: id.to_string() })
}

/// 读出一版
pub fn open(b: &Backup) -> Result<Pack, BackupError> {
    let bytes = std::fs::read(&b.path).map_err(io(&b.path))?;
    crate::read(&bytes, Unseal::None).map_err(BackupError::Pack)
}

/// 生效之后：此刻磁盘上的样子（`text` 加上它列着的插件、通用偏好）和最新那一版不一样就
/// 留一版。返回留下的那一版
pub fn record(
    config_path: &Path,
    text: &str,
    origin: Origin,
) -> Result<Option<Backup>, BackupError> {
    if origin == Origin::Rotation || !readable(text) {
        return Ok(None);
    }
    let pack = disk::current(config_path, text, PackOrigin::from(origin));
    let all = list(config_path)?;
    if let Some(last) = all.last()
        && let Ok(prev) = open(last)
        && fingerprint(&prev) == fingerprint(&masked(&pack))
    {
        return Ok(None);
    }
    save(config_path, &pack, origin, all.last()).map(Some)
}

/// 写之前：`text`（磁盘上此刻的配置原文）和最新那一版的原文不一样才留一版（外部改动没来得
/// 及留、第一次运行）。**只比原文**，理由见模块说明。
///
/// 留下的这一版**来源记成外部改动**，不是 `origin`：`origin` 说的是接下来这次写，而写之前的
/// 这个样子不是它造成的 —— 说不清是谁改的，最可能就是有人直接改了文件
pub fn record_before(
    config_path: &Path,
    text: &str,
    origin: Origin,
) -> Result<Option<Backup>, BackupError> {
    if origin == Origin::Rotation || !readable(text) {
        return Ok(None);
    }
    let all = list(config_path)?;
    if let Some(last) = all.last()
        && let Ok(prev) = open(last)
        && text_fingerprint(&prev.config_yaml)
            == text_fingerprint(&tw_config::control_key::mask(text))
    {
        return Ok(None);
    }
    let pack = disk::current(config_path, text, PackOrigin::External);
    save(config_path, &pack, Origin::External, all.last()).map(Some)
}

/// 读得成 YAML（一个映射）吗。**读不成的原文不记成版本**（安全模式下启动时、改坏了的文件）：
/// 恢复不了它，它还会挤掉一个恢复得了的版本，整份恢复取本机那几节时也取不到东西
fn readable(text: &str) -> bool {
    serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text).is_ok_and(|v| v.is_mapping())
}

/// 从新往旧第一个读得成 YAML、和现在这份原文不一样的本机版本：配置坏到读不成 YAML 时，整份
/// 恢复从这里取本机那几节（[`crate::restore::whole`]）
pub fn last_readable(config_path: &Path, current_text: &str) -> Option<Pack> {
    let now = text_fingerprint(&tw_config::control_key::mask(current_text));
    list(config_path).ok()?.iter().rev().find_map(|b| {
        open(b)
            .ok()
            .filter(|p| readable(&p.config_yaml) && text_fingerprint(&p.config_yaml) != now)
    })
}

fn masked(pack: &Pack) -> Pack {
    Pack {
        config_yaml: tw_config::control_key::mask(&pack.config_yaml),
        ..pack.clone()
    }
}

fn save(
    config_path: &Path,
    pack: &Pack,
    origin: Origin,
    last: Option<&Backup>,
) -> Result<Backup, BackupError> {
    let d = dir(config_path);
    tw_config::private_dir::create(&d).map_err(io(&d))?;
    // **时间戳严格递增**：同一毫秒里连着留两版时，「哪一版更新」不能听 read_dir 的
    let now = crate::now_ms();
    let at_ms = match last {
        Some(l) if now <= l.at_ms => l.at_ms + 1,
        _ => now,
    };
    let mut pack = masked(pack);
    pack.manifest.created_at_ms = at_ms;
    let bytes = crate::write(&pack, Seal::None).map_err(BackupError::Pack)?;
    let path = d.join(format!("{at_ms}-{}.{}", origin.slug(), crate::EXTENSION));
    disk::write_private(&path, &bytes).map_err(io(&path))?;
    prune(config_path)?;
    Ok(Backup {
        id: at_ms.to_string(),
        at_ms,
        origin,
        path,
    })
}

/// 同步应用的那一版是从哪台设备来的：记在这一版的 manifest 里（`sync.device_name`），备份列表
/// 里写「同步自 MacBook Air」。**只改这一个字段**，别的字节照旧
pub fn note_device(b: &Backup, device: &str) -> Result<(), BackupError> {
    let mut pack = open(b)?;
    pack.manifest.sync = Some(serde_json::json!({ "device_name": device }));
    let bytes = crate::write(&pack, Seal::None).map_err(BackupError::Pack)?;
    disk::write_private(&b.path, &bytes).map_err(io(&b.path))
}

/// 一版是从哪台设备同步来的（[`note_device`] 记下的）
pub fn device_of(pack: &Pack) -> Option<String> {
    pack.manifest
        .sync
        .as_ref()?
        .get("device_name")?
        .as_str()
        .map(str::to_string)
}

/// 启动时：插件文件和配置里批准的哈希都对不上的（脚本和底稿都不是那一份 —— 写插件文件和写
/// 配置之间断了电），从最近的备份里找回哈希对得上的那一份写回去。返回修好的插件 id。
///
/// **底稿对得上的不动**：那是用户在磁盘上改了、还没批准的文件，不是断在半路
pub fn heal_plugins(config_path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(config_path) else {
        return Vec::new();
    };
    let Ok(doc) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&text) else {
        return Vec::new();
    };
    let entries: Vec<(String, String)> = doc
        .get(tw_config::plugins::DIR)
        .and_then(serde_yaml_ng::Value::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let id = p.get("id")?.as_str()?.to_string();
            let sha = p.get("sha256")?.as_str()?.to_string();
            tw_config::plugins::valid_id(&id).then_some((id, sha))
        })
        .collect();
    let hash = crate::model::sha256_hex;
    let mut healed = Vec::new();
    let mut backups: Option<Vec<Pack>> = None;
    for (id, sha) in entries {
        // **只在脚本和底稿都对不上批准的哈希时**才找回来（写插件文件和写配置之间断过电）。
        // 底稿对得上、脚本对不上是用户改了脚本还没批准：不动它，网关照常把它停用、等批准。
        // 没有底稿的说不清是哪一种，也不动；脚本文件没了的找回来，盖不掉谁的改动
        let heal = match disk::read_plugin(config_path, &id) {
            None => true,
            Some(f) => {
                hash(&f.script) != sha && f.approved.as_ref().is_some_and(|a| hash(a) != sha)
            }
        };
        if !heal {
            continue;
        }
        let packs = backups.get_or_insert_with(|| {
            list(config_path)
                .unwrap_or_default()
                .iter()
                .rev()
                .filter_map(|b| open(b).ok())
                .collect()
        });
        let found = packs.iter().find_map(|p| {
            let f = p.plugins.get(&id)?;
            [Some(&f.script), f.approved.as_ref()]
                .into_iter()
                .flatten()
                .find(|b| hash(b) == sha)
                .cloned()
        });
        let Some(bytes) = found else { continue };
        let dir = disk::dir_of(config_path);
        let ok = disk::write_private(&tw_config::plugins::file_path(&dir, &id), &bytes).is_ok()
            && disk::write_private(&tw_config::plugins::approved_path(&dir, &id), &bytes).is_ok();
        if ok {
            healed.push(id);
        }
    }
    healed
}

/// 超出 [`KEEP`] 的老版本删掉，以前的修改记录（`*.yaml`）也删掉
fn prune(config_path: &Path) -> Result<(), BackupError> {
    let all = list(config_path)?;
    if all.len() > KEEP {
        for b in &all[..all.len() - KEEP] {
            let _ = std::fs::remove_file(&b.path);
        }
    }
    let d = dir(config_path);
    if let Ok(rd) = std::fs::read_dir(&d) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let legacy = name.ends_with(".yaml")
                && name
                    .split('-')
                    .next()
                    .is_some_and(|t| t.parse::<u64>().is_ok());
            if legacy {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(())
}

/// 比较两版用的：原文里 OAuth 的 token 和过期时间去掉（轮换不算改动），加上插件、偏好
fn fingerprint(pack: &Pack) -> String {
    let mut h = blake3::Hasher::new();
    h.update(text_fingerprint(&pack.config_yaml).as_bytes());
    for (id, f) in &pack.plugins {
        h.update(id.as_bytes());
        h.update(&[0]);
        h.update(&f.script);
        h.update(&[1]);
        if let Some(a) = &f.approved {
            h.update(a);
        }
        h.update(&[2]);
    }
    if let Some(p) = &pack.client_prefs {
        h.update(p.to_string().as_bytes());
    }
    h.finalize().to_hex().to_string()
}

fn text_fingerprint(text: &str) -> String {
    let Ok(mut doc) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text) else {
        return blake3::hash(text.as_bytes()).to_hex().to_string();
    };
    if let Some(list) = doc
        .get_mut("providers")
        .and_then(serde_yaml_ng::Value::as_sequence_mut)
    {
        for p in list {
            if let Some(serde_yaml_ng::Value::Mapping(o)) = p.get_mut("oauth") {
                for k in ["access", "refresh", "expires_at"] {
                    o.remove(k);
                }
            }
        }
    }
    // 只比内容：注释和排版变了也算变了（那是用户改的），所以原文也要算进去 —— 去掉 token
    // 那几行之外的原文
    let mut lines = String::new();
    for l in text.lines() {
        let t = l.trim_start();
        if !(t.starts_with("access:") || t.starts_with("refresh:") || t.starts_with("expires_at:"))
        {
            lines.push_str(l);
            lines.push('\n');
        }
    }
    let canon = serde_json::to_string(&doc).unwrap_or_default();
    blake3::hash(format!("{canon}\n{lines}").as_bytes())
        .to_hex()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00";

    fn cfg(port: u16) -> String {
        format!(
            "version: 1\nlisten:\n  control:\n    key: {KEY}\n  gateway:\n    port: {port}\nclients:\n  - name: default\n    key: tw-a\n"
        )
    }

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.yaml");
        (d, p)
    }

    #[test]
    fn each_distinct_version_is_kept_once_and_reads_back_whole() {
        let (_d, p) = setup();
        let a = record(&p, &cfg(1), Origin::Ui).unwrap().unwrap();
        assert!(
            record(&p, &cfg(1), Origin::Ui).unwrap().is_none(),
            "same content twice"
        );
        assert!(record_before(&p, &cfg(1), Origin::Ui).unwrap().is_none());
        let b = record(&p, &cfg(2), Origin::Cli).unwrap().unwrap();
        assert!(b.at_ms > a.at_ms);
        let all = list(&p).unwrap();
        assert_eq!(all, vec![a.clone(), b.clone()]);
        let back = open(&a).unwrap();
        assert!(back.config_yaml.contains("port: 1"));
        // 钥匙不进备份
        assert!(!back.config_yaml.contains(KEY), "{}", back.config_yaml);
        assert_eq!(back.manifest.origin, PackOrigin::Ui);
        assert_eq!(find(&p, &b.id).unwrap(), b);
        assert!(matches!(
            find(&p, "nope"),
            Err(BackupError::NotFound { .. })
        ));
    }

    #[test]
    fn plugins_and_preferences_are_part_of_a_version() {
        let (_d, p) = setup();
        let text = format!(
            "{}plugins:\n  - id: a\n    file: plugins/a.js\n    sha256: {}\n",
            cfg(1),
            "0".repeat(64)
        );
        let files = crate::PluginFiles {
            script: b"v1".to_vec(),
            approved: None,
        };
        disk::write_plugins(
            &p,
            &Default::default(),
            &[("a".to_string(), files)].into_iter().collect(),
        )
        .unwrap();
        record(&p, &text, Origin::Ui).unwrap().unwrap();
        // 只改了插件文件：又一版
        disk::write_plugins(
            &p,
            &Default::default(),
            &[(
                "a".to_string(),
                crate::PluginFiles {
                    script: b"v2".to_vec(),
                    approved: None,
                },
            )]
            .into_iter()
            .collect(),
        )
        .unwrap();
        // 写之前那一次只看原文：原文没变，不留
        assert!(record_before(&p, &text, Origin::Ui).unwrap().is_none());
        let v2 = record(&p, &text, Origin::Ui).unwrap().unwrap();
        assert_eq!(open(&v2).unwrap().plugins["a"].script, b"v2");
        // 偏好变了：又一版
        disk::write_prefs(&p, Some(&serde_json::json!({"language": "en"}))).unwrap();
        let v3 = record(&p, &text, Origin::Ui).unwrap().unwrap();
        assert_eq!(
            open(&v3).unwrap().client_prefs,
            Some(serde_json::json!({"language": "en"}))
        );
    }

    /// 经同步应用的那一版记着来源设备，别的照旧
    #[test]
    fn a_synced_version_remembers_which_device_it_came_from() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.yaml");
        let text = "version: 1\nclients:\n  - name: default\n    key: tw-k\n";
        std::fs::write(&p, text).unwrap();
        let b = record(&p, text, Origin::Sync).unwrap().unwrap();
        assert_eq!(device_of(&open(&b).unwrap()), None);
        note_device(&b, "MacBook Air").unwrap();
        let back = open(&b).unwrap();
        assert_eq!(device_of(&back).as_deref(), Some("MacBook Air"));
        assert_eq!(back.config_yaml, open(&b).unwrap().config_yaml);
        assert_eq!(list(&p).unwrap().len(), 1, "不多出一版");
    }

    /// 写插件文件和写配置之间断了电：配置还是旧的，文件已经是新的。启动时从备份里找回旧的；
    /// 用户自己改了、还没批准的文件（底稿对得上）不动
    #[test]
    fn plugin_files_cut_off_halfway_are_put_back_from_a_backup() {
        let (_d, p) = setup();
        let files = |s: &str, approved: Option<&str>| crate::PluginFiles {
            script: s.as_bytes().to_vec(),
            approved: approved.map(|a| a.as_bytes().to_vec()),
        };
        let sha = crate::model::sha256_hex(b"v1");
        let text = format!(
            "{}plugins:\n  - id: a\n    file: plugins/a.js\n    sha256: {sha}\n  - id: b\n    file: plugins/b.js\n    sha256: {sha}\n  - id: c\n    file: plugins/c.js\n    sha256: {sha}\n",
            cfg(1)
        );
        std::fs::write(&p, &text).unwrap();
        disk::write_plugins(
            &p,
            &Default::default(),
            &[
                ("a".to_string(), files("v1", Some("v1"))),
                ("b".to_string(), files("v1", Some("v1"))),
                ("c".to_string(), files("v1", Some("v1"))),
            ]
            .into_iter()
            .collect(),
        )
        .unwrap();
        record(&p, &text, Origin::Ui).unwrap().unwrap();
        // a：两份都换成了新的（断在半路）；b：用户改了脚本，底稿还是批准的那份；c：用户改了
        // 脚本，底稿没了（说不清是哪一种，不动）
        disk::write_plugins(
            &p,
            &Default::default(),
            &[
                ("a".to_string(), files("v2", Some("v2"))),
                ("b".to_string(), files("edited", Some("v1"))),
                ("c".to_string(), files("edited", None)),
            ]
            .into_iter()
            .collect(),
        )
        .unwrap();
        assert_eq!(heal_plugins(&p), ["a"]);
        assert_eq!(disk::read_plugin(&p, "a").unwrap().script, b"v1");
        assert_eq!(disk::read_plugin(&p, "b").unwrap().script, b"edited");
        assert_eq!(disk::read_plugin(&p, "c").unwrap().script, b"edited");
        assert!(heal_plugins(&p).is_empty());
    }

    /// 读不成 YAML 的原文不记成版本；整份恢复取本机那几节时从新往旧找第一个读得成的
    #[test]
    fn unreadable_text_is_no_version_and_the_last_readable_one_is_found() {
        let (_d, p) = setup();
        record(&p, &cfg(1), Origin::Ui).unwrap().unwrap();
        record(&p, &cfg(2), Origin::Ui).unwrap().unwrap();
        let garbage = "listen: [\n  :::\n";
        assert!(serde_yaml_ng::from_str::<serde_yaml_ng::Value>(garbage).is_err());
        assert!(record(&p, garbage, Origin::External).unwrap().is_none());
        assert!(record_before(&p, garbage, Origin::Ui).unwrap().is_none());
        assert!(
            record(&p, "just a string\n", Origin::External)
                .unwrap()
                .is_none()
        );
        assert_eq!(list(&p).unwrap().len(), 2);
        let last = last_readable(&p, garbage).unwrap();
        assert!(last.config_yaml.contains("port: 2"), "{}", last.config_yaml);
        // 和现在这份一样的不算（它要是读得成，本机那几节本来就从现在这份取）
        let last = last_readable(&p, &cfg(2)).unwrap();
        assert!(last.config_yaml.contains("port: 1"), "{}", last.config_yaml);
    }

    #[test]
    fn a_rotation_leaves_no_version_and_does_not_count_as_a_change() {
        let (_d, p) = setup();
        let with = |rt: &str| {
            format!(
                "{}providers:\n  - name: c\n    base_url: https://x\n    oauth:\n      refresh: {rt}\n      endpoint: https://t\n",
                cfg(1)
            )
        };
        record(&p, &with("rt-1"), Origin::Ui).unwrap().unwrap();
        assert!(
            record(&p, &with("rt-2"), Origin::Rotation)
                .unwrap()
                .is_none()
        );
        // 轮换之后的下一次写：写之前那一次不因为 token 变了多留一版
        assert!(
            record_before(&p, &with("rt-2"), Origin::Ui)
                .unwrap()
                .is_none()
        );
        assert_eq!(list(&p).unwrap().len(), 1);
    }

    #[test]
    fn only_the_latest_versions_stay_and_the_old_history_goes() {
        let (_d, p) = setup();
        let d = dir(&p);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("1700000000000-ui-abcdef012345.yaml"), "a: 1\n").unwrap();
        std::fs::write(d.join("README.txt"), "hi").unwrap();
        for i in 0..KEEP + 5 {
            record(&p, &cfg(i as u16 + 1), Origin::Ui).unwrap().unwrap();
        }
        let all = list(&p).unwrap();
        assert_eq!(all.len(), KEEP);
        assert!(
            open(all.last().unwrap())
                .unwrap()
                .config_yaml
                .contains(&format!("port: {}", KEEP + 5))
        );
        assert!(!d.join("1700000000000-ui-abcdef012345.yaml").exists());
        assert!(
            d.join("README.txt").exists(),
            "files that are not ours stay"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&all[0].path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn versions_made_in_the_same_millisecond_keep_their_order() {
        let (_d, p) = setup();
        let stamps: Vec<u64> = (0..30)
            .map(|i| record(&p, &cfg(i + 1), Origin::Ui).unwrap().unwrap().at_ms)
            .collect();
        assert!(stamps.windows(2).all(|w| w[0] < w[1]), "{stamps:?}");
    }
}
