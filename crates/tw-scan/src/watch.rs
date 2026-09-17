//! 盯着配置面的变化。
//!
//! 为什么持续监控有价值，而不只是「打开页面扫一次」：
//!
//! > 你 clone 了一个看起来正常的仓库，它带着 `.claude/settings.json`。
//! > 客户端确实有信任提示，但**信任是一次性的、目录级的** —— 你点了
//! > 信任之后，该目录下的这些文件后续被改动（比如你 `git pull` 了一次）
//! > 不会重新提示。
//!
//! 所以是**首次扫描 + 变更时 diff 扫描**。而 diff 那一半才是真正值钱的：
//! **「一个用了半年的 skill 突然多了一段零宽字符」这个信号，比「这个
//! 文件里有可疑内容」强得多。**
//!
//! # 范围仍然是那几个目录
//!
//! 监听的目录集合就是 [`crate::sources`] 划定的那一批，一个不多。无界的
//! FSEvents 监听既是性能问题，也和「空闲时接近零」的目标冲突。
//!
//! **盯目录不盯文件**（和 tw-config 的监听同一条理由）：编辑器保存是
//! 「写临时文件再 rename」，盯着文件的监听会在第一次保存之后永久失效，
//! 而且不报任何错。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 聚合窗口。一次保存会触发好几个事件（建临时文件、rename、改属性）。
pub const DEBOUNCE: Duration = Duration::from_millis(300);

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("无法监听 {path} 的变化：{source}")]
    Start {
        path: PathBuf,
        source: notify::Error,
    },
}

/// 一个活着的监听。**扔掉它就停止监听。**
pub struct Watch {
    _inner: notify::RecommendedWatcher,
}

/// 要盯的目录集合。
///
/// 从来源列表反推：每份文件所在的目录各盯一次，去重。**不递归** ——
/// `~/.claude/` 底下有 `projects/`、`todos/`、`shell-snapshots/` 这些
/// 每分钟都在变的东西，递归盯它等于给自己找一个永不停歇的事件源。
///
/// 代价是 `skills/<名字>/SKILL.md` 这种一层深的要单独加进来，所以这里
/// 收的是**每个来源文件自己的父目录**，而不是几个根目录。
pub fn dirs_for(sources: &[crate::sources::Source]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for s in sources {
        if let Some(d) = s.path.parent()
            && d.is_dir()
            && seen.insert(d.to_path_buf())
        {
            out.push(d.to_path_buf());
        }
    }
    out.sort();
    out
}

/// 我们关心的文件后缀。
///
/// 这个过滤器不是优化，是**必需品**：`~/.claude.json` 的父目录是
/// `$HOME` —— 那是全机器最忙的目录之一（每个应用都在往那儿写点东西）。
/// 不过滤的话，别人写一次 `.zsh_history` 我们就重扫一遍几十个文件，
/// 而目标是「空闲时接近零」。
const INTERESTING: &[&str] = &["md", "json", "toml", "yaml", "yml"];

fn interesting(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| INTERESTING.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// 盯住这些目录，聚合出的每一次改动发一个信号。
///
/// 发的是「有事发生了」而不是「文件现在长这样」：**读文件是调用方的
/// 事** —— 事件到达时写入可能还没完成。
pub fn watch(dirs: &[PathBuf]) -> Result<(Watch, tokio::sync::mpsc::Receiver<()>), WatchError> {
    use notify::{EventKind, RecursiveMode, Watcher as _};

    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let (raw_tx, raw_rx) = std::sync::mpsc::channel::<()>();

    let mut w = notify::recommended_watcher(move |ev: notify::Result<notify::Event>| {
        let Ok(ev) = ev else { return };
        // 只关心内容和存在性。**访问时间之类的不算** —— 备份和索引工具
        // 会大量产生它们，而那会让「空闲时接近零」变成一句空话。
        if !matches!(
            ev.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        ) {
            return;
        }
        // 只有我们关心的那几种文件算数。**这一条撑着「空闲接近零」** ——
        // `$HOME` 在监听集合里（`~/.claude.json` 的父目录就是它）。
        //
        // 我们自己写的备份和旁文件也不算：接管一次会触发一轮扫描，
        // 那一轮又什么都发现不了。
        if !ev.paths.iter().any(|p| interesting(p) && !is_ours(p)) {
            return;
        }
        let _ = raw_tx.send(());
    })
    .map_err(|source| WatchError::Start {
        path: PathBuf::new(),
        source,
    })?;

    for d in dirs {
        w.watch(d, RecursiveMode::NonRecursive)
            .map_err(|source| WatchError::Start {
                path: d.clone(),
                source,
            })?;
    }

    std::thread::spawn(move || {
        while raw_rx.recv().is_ok() {
            loop {
                match raw_rx.recv_timeout(DEBOUNCE) {
                    Ok(()) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
            // 满了就丢：信号是「去看一眼」，堆积两个和一个是一回事
            let _ = tx.try_send(());
        }
    });

    Ok((Watch { _inner: w }, rx))
}

/// 这个路径是我们自己写的吗。
fn is_ours(p: &Path) -> bool {
    p.to_str().is_some_and(|s| {
        s.contains(crate::sources::SIDECAR_MARK)
            || s.contains(".thinkwatch-")
            || s.ends_with(".tmp")
    })
}

/// 上一次看到的样子，用来算「这次新出现了什么」。
///
/// **只记指纹，不记内容。**这些文件里有用户的提示词和密钥，把它们的
/// 全文留在内存里没有任何必要。
#[derive(Debug, Default)]
pub struct Seen {
    /// 每条发现的指纹
    known: HashSet<String>,
    /// 扫过了没有。**第一次扫的结果不算「新出现」** —— 否则用户第一次
    /// 打开就会被一屏「新发现」砸中，而那些东西可能在他机器上放了半年
    primed: bool,
}

fn key(f: &crate::report::Finding) -> String {
    format!("{}|{}|{}|{}", f.path.display(), f.rule, f.line, f.excerpt)
}

impl Seen {
    /// 吃掉一次扫描结果，返回**这次新出现的那些**。
    pub fn diff(&mut self, findings: &[crate::report::Finding]) -> Vec<crate::report::Finding> {
        let fresh: Vec<_> = findings
            .iter()
            .filter(|f| !self.known.contains(&key(f)))
            .cloned()
            .collect();
        self.known = findings.iter().map(key).collect();
        if !self.primed {
            self.primed = true;
            return Vec::new();
        }
        fresh
    }
    pub fn primed(&self) -> bool {
        self.primed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{Finding, Level};
    use crate::sources::Kind;

    fn f(path: &str, rule: &str, line: usize) -> Finding {
        Finding {
            level: Level::High,
            rule: rule.into(),
            kind: Kind::Skill,
            client: "claude-code".into(),
            path: PathBuf::from(path),
            line,
            title: "t".into(),
            detail: "d".into(),
            excerpt: "e".into(),
        }
    }

    #[test]
    fn the_first_scan_is_not_reported_as_new() {
        // 否则用户第一次打开就会被一屏「新发现」砸中，而那些东西可能
        // 在他机器上放了半年。
        let mut seen = Seen::default();
        assert!(
            seen.diff(&[f("a", "zero_width", 1), f("b", "tag", 2)])
                .is_empty()
        );
        assert!(seen.primed());
    }

    #[test]
    fn only_what_just_appeared_is_reported() {
        // **「一个用了半年的 skill 突然多了一段零宽字符」这个信号，
        // 比「这个文件里有可疑内容」强得多。**
        let mut seen = Seen::default();
        seen.diff(&[f("a", "zero_width", 1)]);
        let fresh = seen.diff(&[f("a", "zero_width", 1), f("a", "tag", 9)]);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].rule, "tag");
    }

    #[test]
    fn something_that_disappears_and_comes_back_is_reported_again() {
        // 用户改掉了又被改回来，那是一次新的事件。
        let mut seen = Seen::default();
        seen.diff(&[f("a", "tag", 1)]);
        assert!(seen.diff(&[]).is_empty());
        assert_eq!(seen.diff(&[f("a", "tag", 1)]).len(), 1);
    }

    #[test]
    fn the_same_finding_moving_to_another_line_counts_as_new() {
        // 行号变了通常意味着文件被编辑过 —— 那正是我们想知道的时刻。
        let mut seen = Seen::default();
        seen.diff(&[f("a", "tag", 1)]);
        assert_eq!(seen.diff(&[f("a", "tag", 40)]).len(), 1);
    }

    #[test]
    fn the_seen_set_holds_no_file_contents() {
        // 这些文件里有用户的提示词和密钥。
        let mut seen = Seen::default();
        let mut secret = f("a", "tag", 1);
        secret.detail = "这里有一段很私密的提示词内容".into();
        secret.title = "标题里也有私密内容".into();
        seen.diff(&[secret]);
        let dump = format!("{:?}", seen);
        assert!(!dump.contains("私密"), "{dump}");
    }

    #[test]
    fn our_own_files_do_not_trigger_a_rescan() {
        // 接管一次会写旁文件和备份。让它触发一轮扫描的话，那一轮什么
        // 都发现不了，纯属白烧 CPU。
        assert!(is_ours(Path::new(
            "/Users/x/.claude/settings.json.thinkwatch.json"
        )));
        assert!(is_ours(Path::new(
            "/Users/x/.claude/.settings.json.thinkwatch-123.tmp"
        )));
        assert!(!is_ours(Path::new("/Users/x/.claude/settings.json")));
        assert!(!is_ours(Path::new("/Users/x/.claude/CLAUDE.md")));
    }

    #[tokio::test]
    async fn an_edit_in_a_watched_directory_produces_one_signal() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join(".claude");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("CLAUDE.md"), "# 一\n").unwrap();

        let (_w, mut rx) = watch(std::slice::from_ref(&dir)).unwrap();
        std::fs::write(dir.join("CLAUDE.md"), "# 二\n").unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .is_ok(),
            "没收到信号"
        );
    }

    #[tokio::test]
    async fn writing_an_unrelated_file_in_a_watched_directory_stays_quiet() {
        // **这一条撑着「空闲时 CPU 接近零」。**`$HOME` 在监听集合里
        // （`~/.claude.json` 的父目录就是它），而那是全机器最忙的目录
        // 之一 —— 别人写一次 `.zsh_history` 我们就重扫几十个文件的话，
        // 这个功能会变成一个后台耗电器。
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().to_path_buf();
        let (_w, mut rx) = watch(std::slice::from_ref(&dir)).unwrap();

        std::fs::write(dir.join(".zsh_history"), "别人的东西\n").unwrap();
        std::fs::write(dir.join("settings.json.thinkwatch.json"), "{}").unwrap();
        std::fs::write(dir.join("x.sock"), "").unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(1200), rx.recv())
                .await
                .is_err(),
            "被无关文件吵醒了"
        );

        // 而我们关心的那种照样能叫醒它
        std::fs::write(dir.join("CLAUDE.md"), "# 改了\n").unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .is_ok(),
            "该醒的时候没醒"
        );
    }

    #[tokio::test]
    async fn a_directory_we_do_not_watch_stays_quiet() {
        // 范围就是范围。全盘监听既是性能问题，也和「空闲接近零」冲突。
        let d = tempfile::tempdir().unwrap();
        let watched = d.path().join("watched");
        let other = d.path().join("other");
        std::fs::create_dir_all(&watched).unwrap();
        std::fs::create_dir_all(&other).unwrap();

        let (_w, mut rx) = watch(&[watched]).unwrap();
        std::fs::write(other.join("x.md"), "改了别处\n").unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(1200), rx.recv())
                .await
                .is_err(),
            "盯了不该盯的目录"
        );
    }

    #[test]
    fn the_watched_directories_come_from_the_source_list_and_nowhere_else() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        std::fs::create_dir_all(home.join(".claude/skills/x")).unwrap();
        std::fs::write(home.join(".claude/settings.json"), "{}").unwrap();
        std::fs::write(home.join(".claude/skills/x/SKILL.md"), "---\n---\n").unwrap();

        let dirs = dirs_for(&crate::sources::user_level(home));
        assert!(dirs.contains(&home.join(".claude")));
        // 一层深的 skill 目录要各自被盯到 —— 我们不递归
        assert!(dirs.contains(&home.join(".claude/skills/x")));
        assert!(
            !dirs.contains(&home.to_path_buf()),
            "不该盯整个 home：{dirs:?}"
        );
    }
}
