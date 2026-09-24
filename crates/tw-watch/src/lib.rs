//! 盯几个目录，一阵改动聚合成一个信号。
//!
//! 配置文件的热重载（tw-config）和客户端配置面的变更扫描（桌面端的 tw-scan）要的是
//! 同一件事，**只写一遍**：两份各写各的，去抖、事件种类、通道满了怎么办迟早长得不一样，
//! 而那种不一致的表现是「这边改了会重载、那边改了没反应」。各自不同的只有两样：
//! 盯哪几个目录、哪些路径算数（[`watch`] 的 `relevant`）。
//!
//! 三个非做不可的细节：
//!
//! **一、盯目录，不盯文件。**编辑器保存不是「往原文件写」，而是「写一个临时文件
//! 再 rename 过去」。rename 之后原来那个 inode 就没人指了 —— 盯着文件的监听会在
//! 第一次保存之后**永久失效**，而且不报任何错。
//!
//! **二、不递归。**配置旁边就是历史目录；`~/.claude/` 底下有每分钟都在变的东西。
//! 递归盯它们等于给自己找一个永不停歇的事件源。
//!
//! **三、去抖。**一次保存常常触发好几个事件（建临时文件、rename、改属性、改
//! mtime）。不聚合的话，一次保存会触发好几轮重读。

use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("changes to {path} could not be watched: {source}")]
    Start {
        path: PathBuf,
        source: notify::Error,
    },
}

/// 一个活着的监听。**扔掉它就停止监听** —— 所以调用方必须留着它。
pub struct Watch {
    _inner: notify::RecommendedWatcher,
}

/// 盯住 `dirs`（不递归），改动涉及的路径里有一个让 `relevant` 说是的，就算一次；
/// 一次之后 `debounce` 之内的都并进去，然后往通道里发**一个**信号。
///
/// 发的是「有事发生了」而不是「文件现在长这样」：**读文件是调用方的事** —— 只有
/// 它知道要不要读（比如上一次是自己刚写的），而事件到达时写入可能还没完成。
///
/// 只看内容和存在性的变化（建、改、删）。**访问时间之类的不算** —— 备份和索引
/// 工具会大量产生它们。
///
/// 通道里最多攒一个信号：满了就丢。信号的意思是「去看一眼」，攒两个和攒一个是
/// 一回事。
pub fn watch(
    dirs: &[PathBuf],
    debounce: Duration,
    relevant: impl Fn(&Path) -> bool + Send + 'static,
) -> Result<(Watch, tokio::sync::mpsc::Receiver<()>), WatchError> {
    use notify::{EventKind, RecursiveMode, Watcher as _};

    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let (raw_tx, raw_rx) = std::sync::mpsc::channel::<()>();
    let mut w = notify::recommended_watcher(move |ev: notify::Result<notify::Event>| {
        let Ok(ev) = ev else { return };
        if !matches!(
            ev.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        ) {
            return;
        }
        if ev.paths.iter().any(|p| relevant(p)) {
            let _ = raw_tx.send(());
        }
    })
    .map_err(|source| WatchError::Start {
        path: dirs.first().cloned().unwrap_or_default(),
        source,
    })?;
    for d in dirs {
        w.watch(d, RecursiveMode::NonRecursive)
            .map_err(|source| WatchError::Start {
                path: d.clone(),
                source,
            })?;
    }

    // notify 的回调跑在它自己的线程上，这里把它接到 tokio 上并去抖。
    std::thread::spawn(move || {
        while raw_rx.recv().is_ok() {
            // 收到一个之后，把窗口内后续的全部吞掉 —— 一次保存的那一串事件因此只
            // 产生一个信号。
            loop {
                match raw_rx.recv_timeout(debounce) {
                    Ok(()) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
            let _ = tx.try_send(());
        }
    });

    Ok((Watch { _inner: w }, rx))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_millis(200);

    /// 等一个信号，**区分三种结果**：收到了、窗口内没动静、通道关了（监听线程
    /// 没了之后永远不会再有信号 —— 不分开的话它和「收到了」分不清）。
    #[derive(Debug, PartialEq)]
    enum Signal {
        Got,
        Quiet,
        Closed,
    }

    async fn recv(rx: &mut tokio::sync::mpsc::Receiver<()>, within: Duration) -> Signal {
        match tokio::time::timeout(within, rx.recv()).await {
            Ok(Some(())) => Signal::Got,
            Ok(None) => Signal::Closed,
            Err(_) => Signal::Quiet,
        }
    }

    fn only_md(p: &Path) -> bool {
        p.extension().is_some_and(|e| e == "md")
    }

    #[tokio::test]
    async fn a_burst_of_writes_is_one_signal() {
        let d = tempfile::tempdir().unwrap();
        let (_w, mut rx) = watch(&[d.path().to_path_buf()], WINDOW, only_md).unwrap();
        for i in 0..10 {
            std::fs::write(d.path().join("a.md"), format!("{i}\n")).unwrap();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(recv(&mut rx, Duration::from_secs(3)).await, Signal::Got);
        assert_eq!(
            recv(&mut rx, Duration::from_millis(600)).await,
            Signal::Quiet,
            "连写十次产生了不止一个信号"
        );
    }

    #[tokio::test]
    async fn what_the_filter_turns_down_stays_quiet() {
        let d = tempfile::tempdir().unwrap();
        let (_w, mut rx) = watch(&[d.path().to_path_buf()], WINDOW, only_md).unwrap();
        std::fs::write(d.path().join("x.log"), "x\n").unwrap();
        assert_eq!(
            recv(&mut rx, Duration::from_millis(800)).await,
            Signal::Quiet
        );
        std::fs::write(d.path().join("x.md"), "x\n").unwrap();
        assert_eq!(recv(&mut rx, Duration::from_secs(3)).await, Signal::Got);
    }

    #[tokio::test]
    async fn a_subdirectory_is_not_watched() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("history")).unwrap();
        let (_w, mut rx) = watch(&[d.path().to_path_buf()], WINDOW, only_md).unwrap();
        std::fs::write(d.path().join("history/1.md"), "x\n").unwrap();
        assert_eq!(
            recv(&mut rx, Duration::from_millis(800)).await,
            Signal::Quiet,
            "递归了"
        );
    }

    #[tokio::test]
    async fn several_directories_are_watched_at_once() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let (_w, mut rx) = watch(&[a, b.clone()], WINDOW, only_md).unwrap();
        std::fs::write(b.join("x.md"), "x\n").unwrap();
        assert_eq!(recv(&mut rx, Duration::from_secs(3)).await, Signal::Got);
    }

    #[test]
    fn a_directory_that_does_not_exist_is_an_error_that_names_it() {
        let d = tempfile::tempdir().unwrap();
        let missing = d.path().join("nope");
        let Err(WatchError::Start { path, .. }) =
            watch(std::slice::from_ref(&missing), WINDOW, only_md)
        else {
            panic!("盯一个不存在的目录没报错");
        };
        assert_eq!(path, missing);
    }
}
