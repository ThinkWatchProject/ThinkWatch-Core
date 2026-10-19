//! 数据目录里和配置包有关的几样文件：插件、Lite 的通用偏好、固定密钥。
//!
//! 路径都相对配置文件所在的目录，和插件文件一样（`tw_config::plugins`）：远程 core 也是，
//! 文件在 core 那台机器上。**只给自己看**：目录 0700、文件 0600（unix；Windows 上数据目录
//! 的 ACL 由 `tw_config::private_dir` 管）。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::{Manifest, Pack, PackKind, PackOrigin, PluginFiles};

/// Lite 的通用偏好存在哪个文件里
pub const PREFS_FILE: &str = "client-prefs.json";

/// 固定密钥（本机接管的客户端在用的网关密钥）存在哪个文件里
pub const PINNED_FILE: &str = "pinned-keys.json";

/// 配置文件所在的目录
pub fn dir_of(config_path: &Path) -> PathBuf {
    match config_path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

pub fn prefs_path(config_path: &Path) -> PathBuf {
    dir_of(config_path).join(PREFS_FILE)
}

pub fn pinned_path(config_path: &Path) -> PathBuf {
    dir_of(config_path).join(PINNED_FILE)
}

/// 存着的通用偏好。没有、读不成 JSON 都是 None：偏好丢了不该挡住别的
pub fn read_prefs(config_path: &Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(prefs_path(config_path)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 存下通用偏好。None 是删掉
/// 这份通用偏好超出了上限吗（[`tw_api::CLIENT_PREFS_MAX`]，写成紧凑的 JSON 之后）
pub fn prefs_too_large(prefs: &serde_json::Value) -> bool {
    prefs.to_string().len() > tw_api::CLIENT_PREFS_MAX
}

/// 存下通用偏好，None 是删掉。**超出上限的不写**（`InvalidInput`）：界面、导入、恢复、同步、
/// 命令行写偏好都走这里，谁也绕不过去
pub fn write_prefs(config_path: &Path, prefs: Option<&serde_json::Value>) -> std::io::Result<()> {
    let path = prefs_path(config_path);
    match prefs {
        Some(p) if prefs_too_large(p) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "the client preferences are larger than {} KiB",
                tw_api::CLIENT_PREFS_MAX / 1024
            ),
        )),
        Some(p) => {
            let bytes = serde_json::to_vec_pretty(p).map_err(std::io::Error::other)?;
            write_private(&path, &bytes)
        }
        None => match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    }
}

/// 最多记几个来源。多出来的去掉最久没报过的那一个：**来源不过期**（Lite 每次连上 core 都
/// 重报一次，一个来源再报是换掉它自己那一组），这个上限挡的是没完没了的新来源
pub const PINNED_SOURCES_MAX: usize = 64;
/// 来源标识最长几个字节
pub const PINNED_SOURCE_MAX: usize = 128;
/// 一个来源最多报几把密钥
pub const PINNED_NAMES_MAX: usize = 1024;
/// 一把密钥的名字最长几个字节
pub const PINNED_NAME_MAX: usize = 256;

/// 一个来源报上来的固定密钥。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PinnedSource {
    pub names: BTreeSet<String>,
    /// 最近一次报上来的时候。来源多过 [`PINNED_SOURCES_MAX`] 时去掉最久没报的
    #[serde(default)]
    pub updated_at_ms: u64,
}

/// 存着的固定密钥，**按来源**：几个 Lite 连着同一个 core（远程模式）时各报各的，取并集。
/// 没有、读不懂都是空的。早先的写法（一个名字数组，或者 `{"names": [...]}`）读作来源空串
pub fn read_pinned_sources(config_path: &Path) -> BTreeMap<String, PinnedSource> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum File {
        Sources {
            sources: BTreeMap<String, PinnedSource>,
        },
        Names {
            names: BTreeSet<String>,
        },
        Plain(BTreeSet<String>),
    }
    let Some(file) = std::fs::read(pinned_path(config_path))
        .ok()
        .and_then(|b| serde_json::from_slice::<File>(&b).ok())
    else {
        return BTreeMap::new();
    };
    match file {
        File::Sources { sources } => sources,
        File::Names { names } | File::Plain(names) => BTreeMap::from([(
            String::new(),
            PinnedSource {
                names,
                updated_at_ms: 0,
            },
        )]),
    }
}

/// 各来源的并集
pub fn pinned_union(sources: &BTreeMap<String, PinnedSource>) -> BTreeSet<String> {
    sources
        .values()
        .flat_map(|s| s.names.iter().cloned())
        .collect()
}

/// 存着的固定密钥：各个来源的并集
pub fn read_pinned(config_path: &Path) -> BTreeSet<String> {
    pinned_union(&read_pinned_sources(config_path))
}

/// 报上来的一组超出了上限
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinnedTooLarge {
    Source,
    Names,
    Name,
}

/// 一个来源报上来一组固定密钥：换掉它上次报的。来源多过上限时去掉最久没报过的。**不写盘**
pub fn with_pinned(
    sources: &BTreeMap<String, PinnedSource>,
    source: &str,
    names: &BTreeSet<String>,
    now_ms: u64,
) -> Result<BTreeMap<String, PinnedSource>, PinnedTooLarge> {
    if source.len() > PINNED_SOURCE_MAX {
        return Err(PinnedTooLarge::Source);
    }
    if names.len() > PINNED_NAMES_MAX {
        return Err(PinnedTooLarge::Names);
    }
    if names.iter().any(|n| n.len() > PINNED_NAME_MAX) {
        return Err(PinnedTooLarge::Name);
    }
    let mut out = sources.clone();
    out.insert(
        source.to_string(),
        PinnedSource {
            names: names.clone(),
            updated_at_ms: now_ms,
        },
    );
    while out.len() > PINNED_SOURCES_MAX {
        let Some(oldest) = out
            .iter()
            .filter(|(k, _)| k.as_str() != source)
            .min_by_key(|(_, s)| s.updated_at_ms)
            .map(|(k, _)| k.clone())
        else {
            break;
        };
        out.remove(&oldest);
    }
    Ok(out)
}

/// 存下全部来源
pub fn write_pinned_sources(
    config_path: &Path,
    sources: &BTreeMap<String, PinnedSource>,
) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(&serde_json::json!({ "sources": sources }))
        .map_err(std::io::Error::other)?;
    write_private(&pinned_path(config_path), &bytes)
}

/// 一个来源报上来一组固定密钥，读文件、换掉、写回去。**只给单独一个进程用**（命令行、测试）；
/// 跑着的 core 在锁里改它记着的那一份（`ConfigManager::set_pinned_keys`）
pub fn write_pinned(
    config_path: &Path,
    source: &str,
    names: &BTreeSet<String>,
) -> std::io::Result<BTreeMap<String, PinnedSource>> {
    let sources = with_pinned(
        &read_pinned_sources(config_path),
        source,
        names,
        crate::now_ms(),
    )
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{e:?}")))?;
    write_pinned_sources(config_path, &sources)?;
    Ok(sources)
}

/// 配置原文里列着的插件 id，按顺序。读不成 YAML 是空的
pub fn plugin_ids(text: &str) -> Vec<String> {
    let Ok(doc) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text) else {
        return Vec::new();
    };
    doc.get(tw_config::plugins::DIR)
        .and_then(serde_yaml_ng::Value::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(|p| p.get("id").and_then(serde_yaml_ng::Value::as_str))
        .filter(|id| tw_config::plugins::valid_id(id))
        .map(str::to_string)
        .collect()
}

/// 一个插件在磁盘上的两份文件。脚本不在就是 None
pub fn read_plugin(config_path: &Path, id: &str) -> Option<PluginFiles> {
    let dir = dir_of(config_path);
    let script = std::fs::read(tw_config::plugins::file_path(&dir, id)).ok()?;
    let approved = std::fs::read(tw_config::plugins::approved_path(&dir, id)).ok();
    Some(PluginFiles { script, approved })
}

/// 此刻的样子配成一个包：`text` 是配置原文，插件是它列着的那几个在磁盘上的文件，加上
/// 存着的通用偏好。**原文原样放进去**：要拿去哪儿（备份、导出）由调用方决定剔掉什么
pub fn current(config_path: &Path, text: &str, origin: PackOrigin) -> Pack {
    let plugins: BTreeMap<String, PluginFiles> = plugin_ids(text)
        .into_iter()
        .filter_map(|id| read_plugin(config_path, &id).map(|f| (id, f)))
        .collect();
    Pack {
        manifest: Manifest::new(PackKind::Full, origin),
        config_yaml: text.to_string(),
        plugins,
        client_prefs: read_prefs(config_path),
    }
}

/// 写之前的样子。配置没写成时照它还原
#[derive(Debug, Default)]
pub struct Undo(Vec<(PathBuf, Option<Vec<u8>>)>);

impl Undo {
    /// 还原。哪个文件还原不了只记一行：手里已经有一个更要紧的错误
    pub fn restore(self) {
        for (path, before) in self.0.into_iter().rev() {
            let r = match before {
                Some(b) => write_private(&path, &b),
                None => match std::fs::remove_file(&path) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                    _ => Ok(()),
                },
            };
            if let Err(e) = r {
                tracing::warn!(path = %path.display(), "a file could not be put back: {e}");
            }
        }
    }
}

/// 插件文件换成 `after` 的样子：有变化的写下去，`before` 里有、`after` 里没有的删掉
/// （连同底稿）。`after` 里没有底稿的，原来的底稿删掉。返回还原用的
pub fn write_plugins(
    config_path: &Path,
    before: &BTreeMap<String, PluginFiles>,
    after: &BTreeMap<String, PluginFiles>,
) -> std::io::Result<Undo> {
    let dir = dir_of(config_path);
    let plugins = tw_config::plugins::dir_in(&dir);
    let approved = plugins.join(tw_config::plugins::APPROVED_DIR);
    let mut undo = Undo::default();
    let put = |path: PathBuf, bytes: Option<&[u8]>, undo: &mut Undo| -> std::io::Result<()> {
        let old = match std::fs::read(&path) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        if old.as_deref() == bytes {
            return Ok(());
        }
        match bytes {
            Some(b) => {
                for d in [&plugins, &approved] {
                    tw_config::private_dir::create(d)?;
                }
                write_private(&path, b)?;
            }
            None => std::fs::remove_file(&path)?,
        }
        undo.0.push((path, old));
        Ok(())
    };
    let result = (|| {
        for (id, files) in after {
            put(
                tw_config::plugins::file_path(&dir, id),
                Some(&files.script),
                &mut undo,
            )?;
            put(
                tw_config::plugins::approved_path(&dir, id),
                files.approved.as_deref(),
                &mut undo,
            )?;
        }
        for id in before.keys().filter(|id| !after.contains_key(*id)) {
            put(tw_config::plugins::file_path(&dir, id), None, &mut undo)?;
            put(tw_config::plugins::approved_path(&dir, id), None, &mut undo)?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => Ok(undo),
        Err(e) => {
            undo.restore();
            Err(e)
        }
    }
}

/// 原子地写一个只给自己看的文件：**建的那一刻就是 0600**，写完再改名过去
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        tw_config::private_dir::create(dir)?;
    }
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

    fn files(s: &str) -> PluginFiles {
        PluginFiles {
            script: s.as_bytes().to_vec(),
            approved: Some(s.as_bytes().to_vec()),
        }
    }

    #[test]
    fn plugin_files_are_swapped_in_and_put_back_on_undo() {
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("config.yaml");
        let before = BTreeMap::from([
            ("a".to_string(), files("a1")),
            ("b".to_string(), files("b1")),
        ]);
        write_plugins(&cfg, &BTreeMap::new(), &before).unwrap();
        assert_eq!(read_plugin(&cfg, "a").unwrap(), files("a1"));
        let after = BTreeMap::from([
            ("a".to_string(), files("a2")),
            ("c".to_string(), files("c1")),
        ]);
        let undo = write_plugins(&cfg, &before, &after).unwrap();
        assert_eq!(read_plugin(&cfg, "a").unwrap(), files("a2"));
        assert!(read_plugin(&cfg, "b").is_none(), "removed plugins go");
        assert_eq!(read_plugin(&cfg, "c").unwrap(), files("c1"));
        undo.restore();
        assert_eq!(read_plugin(&cfg, "a").unwrap(), files("a1"));
        assert_eq!(read_plugin(&cfg, "b").unwrap(), files("b1"));
        assert!(read_plugin(&cfg, "c").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = tw_config::plugins::file_path(d.path(), "a");
            assert_eq!(
                std::fs::metadata(p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn prefs_and_pinned_keys_round_trip_and_a_bad_file_reads_as_none() {
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("config.yaml");
        assert_eq!(read_prefs(&cfg), None);
        let p = serde_json::json!({"language": "zh"});
        write_prefs(&cfg, Some(&p)).unwrap();
        assert_eq!(read_prefs(&cfg), Some(p));
        write_prefs(&cfg, None).unwrap();
        assert_eq!(read_prefs(&cfg), None);
        write_prefs(&cfg, None).unwrap();
        // 超出上限的不写
        let big = serde_json::json!({ "big": "x".repeat(tw_api::CLIENT_PREFS_MAX) });
        let e = write_prefs(&cfg, Some(&big)).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(read_prefs(&cfg), None);
        std::fs::write(prefs_path(&cfg), b"{not json").unwrap();
        assert_eq!(read_prefs(&cfg), None);

        // 两个 Lite 各报各的，取并集；一个来源再报一次是换掉它自己的那一组
        let a: BTreeSet<String> = ["codex".to_string()].into_iter().collect();
        let b: BTreeSet<String> = ["claude-code".to_string()].into_iter().collect();
        write_pinned(&cfg, "lite-a", &a).unwrap();
        write_pinned(&cfg, "lite-b", &b).unwrap();
        assert_eq!(read_pinned(&cfg), a.union(&b).cloned().collect());
        write_pinned(&cfg, "lite-a", &BTreeSet::new()).unwrap();
        assert_eq!(read_pinned(&cfg), b);
        // 来源不过期：很久以前报的照样算
        let mut sources = read_pinned_sources(&cfg);
        sources.get_mut("lite-b").unwrap().updated_at_ms = 1;
        assert_eq!(pinned_union(&sources), b);
    }

    /// 早先的写法读作来源空串，不读成空的
    #[test]
    fn an_older_pinned_file_reads_as_the_blank_source() {
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("config.yaml");
        let codex: BTreeSet<String> = ["codex".to_string()].into();
        for old in [&br#"{"names":["codex"]}"#[..], br#"["codex"]"#] {
            std::fs::write(pinned_path(&cfg), old).unwrap();
            assert_eq!(read_pinned(&cfg), codex);
            assert_eq!(read_pinned_sources(&cfg)[""].names, codex);
        }
        // 再报一次（有来源的）：早先那一组还在，并集
        let b: BTreeSet<String> = ["claude-code".to_string()].into();
        write_pinned(&cfg, "lite-a", &b).unwrap();
        assert_eq!(read_pinned(&cfg), codex.union(&b).cloned().collect());
    }

    #[test]
    fn pinned_sources_have_limits() {
        let none = BTreeMap::new();
        let one: BTreeSet<String> = ["k".to_string()].into();
        let long = "x".repeat(PINNED_SOURCE_MAX + 1);
        assert_eq!(
            with_pinned(&none, &long, &one, 1),
            Err(PinnedTooLarge::Source)
        );
        let many: BTreeSet<String> = (0..=PINNED_NAMES_MAX).map(|i| i.to_string()).collect();
        assert_eq!(
            with_pinned(&none, "a", &many, 1),
            Err(PinnedTooLarge::Names)
        );
        let wide: BTreeSet<String> = ["n".repeat(PINNED_NAME_MAX + 1)].into();
        assert_eq!(with_pinned(&none, "a", &wide, 1), Err(PinnedTooLarge::Name));
        // 来源多过上限：去掉最久没报过的那一个
        let mut all = BTreeMap::new();
        for i in 0..PINNED_SOURCES_MAX {
            all = with_pinned(&all, &format!("s{i}"), &one, 10 + i as u64).unwrap();
        }
        let all = with_pinned(&all, "new", &one, 1000).unwrap();
        assert_eq!(all.len(), PINNED_SOURCES_MAX);
        assert!(!all.contains_key("s0") && all.contains_key("s1") && all.contains_key("new"));
    }

    #[test]
    fn the_current_state_is_the_config_and_the_plugins_it_lists() {
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("config.yaml");
        write_plugins(
            &cfg,
            &BTreeMap::new(),
            &BTreeMap::from([
                ("a".to_string(), files("a")),
                ("stray".to_string(), files("s")),
            ]),
        )
        .unwrap();
        let text = "version: 1\nplugins:\n  - { id: a, file: plugins/a.js, sha256: x }\n  - { id: gone, file: plugins/gone.js, sha256: x }\n";
        let p = current(&cfg, text, PackOrigin::Ui);
        assert_eq!(p.plugins.keys().collect::<Vec<_>>(), ["a"]);
        assert_eq!(p.config_yaml, text);
    }
}
