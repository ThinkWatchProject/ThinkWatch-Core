//! 变更历史与回滚（DESIGN.md §3.8、M2 验收「改坏能一键回滚」）。
//!
//! **每次写盘之前，先把当下这一版存起来。**存的是**改之前**那一版而不
//! 是改之后那一版 —— 「回滚」这个动作要的是「回到我动它之前」，而这两
//! 者只在「每次都成功」的世界里等价。
//!
//! 存的是整份文本，不是 diff。理由很实际：一份配置几 KB，五十个版本也
//! 就几百 KB，而**用 diff 存会让回滚依赖一条完整的链** —— 中间任何一环
//! 坏掉，后面全部作废。回滚是出事时用的功能，它自己不能有脆弱的前提。

use std::path::{Path, PathBuf};

use crate::store::{self, StoreError};

/// 留多少版。
///
/// 五十版在一份几 KB 的配置上是几百 KB，而**回滚要找的那一版几乎总在
/// 最近几次里** —— 更早的版本留着只是安慰。
pub const KEEP: usize = 50;

/// 一次改动的来源。**记下来是为了让历史列表能读懂** ——
/// 「界面改的」和「你自己在编辑器里改的」在排查时是完全不同的线索。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Ui,
    Cli,
    /// 外部编辑（编辑器、脚本），我们是从文件监听发现的
    External,
    Rollback,
}

impl Origin {
    fn slug(&self) -> &'static str {
        match self {
            Origin::Ui => "ui",
            Origin::Cli => "cli",
            Origin::External => "ext",
            Origin::Rollback => "rollback",
        }
    }
    fn parse(s: &str) -> Origin {
        match s {
            "ui" => Origin::Ui,
            "cli" => Origin::Cli,
            "rollback" => Origin::Rollback,
            _ => Origin::External,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Origin::Ui => "界面",
            Origin::Cli => "命令行",
            Origin::External => "外部编辑",
            Origin::Rollback => "回滚",
        }
    }
}

/// 历史里的一版。
#[derive(Debug, Clone)]
pub struct Version {
    pub file: PathBuf,
    /// 毫秒时间戳。**文件名里就带着它** —— 历史目录直接按名字排序就是
    /// 时间序，不必读每个文件的 mtime（备份工具会把 mtime 全改成同一天）
    pub at_ms: u64,
    pub origin: Origin,
    /// 内容版本号，和 `store::version_of` 是同一个
    pub version: String,
    pub bytes: u64,
}

fn history_dir(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("history")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 把当前这一版存进历史。
///
/// **内容和上一版一模一样时什么都不做。**否则一次「保存但没改动」会在
/// 历史里堆一版噪音，而回滚列表里全是同一份内容的时候，它就没用了。
pub fn snapshot(
    config_path: &Path,
    text: &str,
    origin: Origin,
) -> Result<Option<Version>, StoreError> {
    let dir = history_dir(config_path);
    let version = store::version_of(text);
    if let Some(last) = list(config_path)?.last()
        && last.version == version
    {
        return Ok(None);
    }
    std::fs::create_dir_all(&dir).map_err(|source| StoreError::Io {
        path: dir.clone(),
        source,
    })?;
    let at_ms = now_ms();
    // 名字里带上时间、来源和版本号 —— 光看文件名就能读懂这一版是什么。
    let file = dir.join(format!(
        "{at_ms}-{}-{}.yaml",
        origin.slug(),
        &version["blake3:".len()..]
    ));
    store::write_atomic(&file, text)?;
    prune(config_path)?;
    Ok(Some(Version {
        file,
        at_ms,
        origin,
        version,
        bytes: text.len() as u64,
    }))
}

/// 历史列表，**从旧到新**。
pub fn list(config_path: &Path) -> Result<Vec<Version>, StoreError> {
    let dir = history_dir(config_path);
    let rd = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(StoreError::Io { path: dir, source }),
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(v) = parse_name(&name) else { continue };
        let bytes = e.metadata().map(|m| m.len()).unwrap_or(0);
        out.push(Version {
            file: e.path(),
            bytes,
            ..v
        });
    }
    // 按时间戳排，不按文件名的字典序 —— 时间戳位数会变（跨过 10 的
    // 幂次），那时字典序和时间序就分家了。
    out.sort_by_key(|v| v.at_ms);
    Ok(out)
}

fn parse_name(name: &str) -> Option<Version> {
    let stem = name.strip_suffix(".yaml")?;
    let mut it = stem.splitn(3, '-');
    let at_ms: u64 = it.next()?.parse().ok()?;
    let origin = Origin::parse(it.next()?);
    let hash = it.next()?;
    Some(Version {
        file: PathBuf::new(),
        at_ms,
        origin,
        version: format!("blake3:{hash}"),
        bytes: 0,
    })
}

/// 超出上限的老版本删掉。
fn prune(config_path: &Path) -> Result<(), StoreError> {
    let all = list(config_path)?;
    if all.len() <= KEEP {
        return Ok(());
    }
    for v in &all[..all.len() - KEEP] {
        let _ = std::fs::remove_file(&v.file);
    }
    Ok(())
}

/// 取某一版的内容。
pub fn read(v: &Version) -> Result<String, StoreError> {
    std::fs::read_to_string(&v.file).map_err(|source| StoreError::Io {
        path: v.file.clone(),
        source,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum RollbackError {
    #[error("历史里没有 {0} 这一版")]
    NoSuchVersion(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// 回到某一版。
///
/// **回滚本身也是一次改动，也要进历史。**否则「回滚之后发现回错了」
/// 就没有退路了 —— 而那是回滚最常见的用法之一。
pub fn rollback(config_path: &Path, version: &str) -> Result<String, RollbackError> {
    let all = list(config_path)?;
    let target = all
        .iter()
        .rev()
        .find(|v| v.version == version || v.version.ends_with(version))
        .ok_or_else(|| RollbackError::NoSuchVersion(version.to_string()))?;
    let text = read(target)?;
    // 先把「现在这一版」存进历史，再覆盖
    if let Ok(cur) = store::read(config_path) {
        snapshot(config_path, &cur.text, Origin::Rollback)?;
    }
    store::write_atomic(config_path, &text)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.yaml");
        std::fs::write(&p, "version: 1\nport: 8788\n").unwrap();
        (d, p)
    }

    #[test]
    fn a_snapshot_can_be_read_back_byte_for_byte() {
        let (_d, p) = setup();
        let text = "version: 1\n# 注释也要留着\nport: 8788\n";
        let v = snapshot(&p, text, Origin::Ui).unwrap().unwrap();
        assert_eq!(read(&v).unwrap(), text);
    }

    #[test]
    fn saving_the_same_content_twice_does_not_add_a_second_version() {
        // 一次「保存但没改动」在历史里堆一版噪音，而回滚列表里全是同一
        // 份内容的时候它就没用了。
        let (_d, p) = setup();
        snapshot(&p, "a: 1\n", Origin::Ui).unwrap().unwrap();
        assert!(snapshot(&p, "a: 1\n", Origin::Ui).unwrap().is_none());
        assert_eq!(list(&p).unwrap().len(), 1);
    }

    #[test]
    fn the_origin_of_each_change_survives_a_restart() {
        // 「界面改的」和「你自己在编辑器里改的」在排查时是完全不同的线索，
        // 而它必须能从磁盘上读回来 —— 内存里记着的东西活不过一次重启。
        let (_d, p) = setup();
        snapshot(&p, "a: 1\n", Origin::Ui).unwrap();
        snapshot(&p, "a: 2\n", Origin::External).unwrap();
        snapshot(&p, "a: 3\n", Origin::Cli).unwrap();
        let got: Vec<Origin> = list(&p).unwrap().iter().map(|v| v.origin).collect();
        assert_eq!(got, vec![Origin::Ui, Origin::External, Origin::Cli]);
    }

    #[test]
    fn history_is_ordered_oldest_first_even_across_a_digit_boundary() {
        // 按文件名的字典序排会在时间戳位数变化时和时间序分家。
        let (_d, p) = setup();
        let dir = history_dir(&p);
        std::fs::create_dir_all(&dir).unwrap();
        for (ms, body) in [(999u64, "a: 1\n"), (1000, "a: 2\n"), (10000, "a: 3\n")] {
            std::fs::write(
                dir.join(format!("{ms}-ui-{}.yaml", &store::hash_of(body)[..12])),
                body,
            )
            .unwrap();
        }
        let got: Vec<u64> = list(&p).unwrap().iter().map(|v| v.at_ms).collect();
        assert_eq!(got, vec![999, 1000, 10000]);
    }

    #[test]
    fn rolling_back_restores_the_old_text_exactly() {
        let (_d, p) = setup();
        let old = "version: 1\n# 我要找回这个注释\nport: 8788\n";
        let v = snapshot(&p, old, Origin::Ui).unwrap().unwrap();
        std::fs::write(&p, "version: 1\nport: 9999\n").unwrap();
        let back = rollback(&p, &v.version).unwrap();
        assert_eq!(back, old);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), old);
    }

    #[test]
    fn rolling_back_is_itself_undoable() {
        // **「回滚之后发现回错了」是回滚最常见的用法之一。**
        let (_d, p) = setup();
        let v1 = snapshot(&p, "a: 1\n", Origin::Ui).unwrap().unwrap();
        std::fs::write(&p, "a: 2\n").unwrap();
        rollback(&p, &v1.version).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a: 1\n");
        // 回滚前那一版（a: 2）进了历史，所以能再回去
        let v2 = list(&p)
            .unwrap()
            .into_iter()
            .find(|v| v.origin == Origin::Rollback)
            .expect("回滚没留下退路");
        assert_eq!(read(&v2).unwrap(), "a: 2\n");
        rollback(&p, &v2.version).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a: 2\n");
    }

    #[test]
    fn a_short_version_prefix_is_enough_to_name_a_version() {
        // 用户会从界面上抄一小截，或者只记得前几位。
        let (_d, p) = setup();
        let v = snapshot(&p, "a: 1\n", Origin::Ui).unwrap().unwrap();
        std::fs::write(&p, "a: 2\n").unwrap();
        let short = &v.version["blake3:".len()..];
        assert_eq!(rollback(&p, short).unwrap(), "a: 1\n");
    }

    #[test]
    fn asking_for_a_version_that_is_not_there_says_so() {
        let (_d, p) = setup();
        let e = rollback(&p, "blake3:deadbeef").unwrap_err();
        assert!(matches!(e, RollbackError::NoSuchVersion(_)), "{e:?}");
    }

    #[test]
    fn old_versions_are_pruned_but_the_recent_ones_all_survive() {
        let (_d, p) = setup();
        for i in 0..KEEP + 10 {
            snapshot(&p, &format!("a: {i}\n"), Origin::Ui).unwrap();
        }
        let all = list(&p).unwrap();
        assert_eq!(all.len(), KEEP);
        // 留下的是最近的那些
        assert_eq!(
            read(all.last().unwrap()).unwrap(),
            format!("a: {}\n", KEEP + 9)
        );
    }

    #[test]
    fn history_files_are_0600_too_because_they_contain_the_same_keys() {
        // 一份历史版本和当前配置一样有明文密钥。权限漏在这里等于没漏在
        // 那里 —— 而这类遗漏最容易发生在「顺手加的辅助功能」上。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let (_d, p) = setup();
            let v = snapshot(&p, "key: sk-secret\n", Origin::Ui)
                .unwrap()
                .unwrap();
            let mode = std::fs::metadata(&v.file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "历史文件权限是 {mode:o}");
        }
    }

    #[test]
    fn an_empty_history_is_not_an_error() {
        // 第一次运行时历史目录还不存在。那是正常状态。
        let (_d, p) = setup();
        assert!(list(&p).unwrap().is_empty());
    }

    #[test]
    fn a_stray_file_in_the_history_directory_is_ignored_not_fatal() {
        // 用户往那个目录里丢过东西、或者别的工具留下过东西。
        let (_d, p) = setup();
        let dir = history_dir(&p);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("README.txt"), "hi").unwrap();
        std::fs::write(dir.join("不是时间戳-ui-abc.yaml"), "a: 1\n").unwrap();
        snapshot(&p, "a: 1\n", Origin::Ui).unwrap();
        assert_eq!(list(&p).unwrap().len(), 1);
    }
}
