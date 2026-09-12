//! 盯着配置文件（DESIGN.md §3.8）。
//!
//! 两个非做不可的细节，少哪个都会表现成「有时候能自动重载，有时候不能」：
//!
//! **一、盯目录，不盯文件。**编辑器保存不是「往原文件写」，而是「写一个
//! 临时文件再 rename 过去」。rename 之后原来那个 inode 就没人指了 ——
//! 盯着文件的监听会在第一次保存之后**永久失效**，而且不报任何错。
//!
//! **二、去抖。**一次保存常常触发好几个事件（建临时文件、rename、改
//! 属性、改 mtime）。不聚合的话，一次保存会触发好几轮重载。

use std::path::{Path, PathBuf};
use std::time::Duration;

/// 聚合窗口。**200ms 是照着编辑器保存的节奏定的** —— 那一串事件通常
/// 在几十毫秒内发完，而人对「改完文件到界面反应」这件事的耐心是秒级的
/// （M2 的验收标准写的是三秒）。
pub const DEBOUNCE: Duration = Duration::from_millis(200);

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("盯不住 {path}：{source}")]
    Start {
        path: PathBuf,
        source: notify::Error,
    },
}

/// 一个活着的监听。**扔掉它就停止监听** —— 所以调用方必须留着它。
pub struct Watch {
    _inner: notify::RecommendedWatcher,
}

/// 盯住 `path`，每聚合出一次改动就往 channel 里发一个信号。
///
/// 发的是「有事发生了」而不是「文件现在长这样」：**读文件是调用方的
/// 事**，因为只有它知道要不要读（比如上一次自己刚写过）。而在这里读
/// 会引入一个更糟的问题 —— 事件到达时写入可能还没完成。
pub fn watch(path: &Path) -> Result<(Watch, tokio::sync::mpsc::Receiver<()>), WatchError> {
    use notify::{EventKind, RecursiveMode, Watcher as _};

    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let target = path.to_path_buf();
    let file_name = target.file_name().map(|s| s.to_os_string());
    // 目录不存在时盯不住 —— 但配置文件的目录在这一步之前已经建好了。
    let dir = target
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let (raw_tx, raw_rx) = std::sync::mpsc::channel::<()>();
    let mut w = notify::recommended_watcher(move |ev: notify::Result<notify::Event>| {
        let Ok(ev) = ev else { return };
        // 只关心内容/存在性的变化。**访问时间之类的不算** —— 有些工具
        // （备份、索引）会大量产生它们。
        if !matches!(
            ev.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        ) {
            return;
        }
        // 同目录下的其他文件（历史目录、我们自己的临时文件）不算
        let hit = ev.paths.iter().any(|p| match (&file_name, p.file_name()) {
            (Some(want), Some(got)) => want == got,
            _ => false,
        });
        if hit {
            let _ = raw_tx.send(());
        }
    })
    .map_err(|source| WatchError::Start {
        path: dir.clone(),
        source,
    })?;
    // 非递归：历史目录就在旁边，递归会把每一次快照都当成一次改动。
    w.watch(&dir, RecursiveMode::NonRecursive)
        .map_err(|source| WatchError::Start {
            path: dir.clone(),
            source,
        })?;

    // notify 的回调跑在它自己的线程上，这里把它接到 tokio 上并去抖。
    std::thread::spawn(move || {
        while raw_rx.recv().is_ok() {
            // 收到一个之后，把窗口内后续的全部吞掉 —— 一次保存的那一串
            // 事件因此只产生一个信号。
            loop {
                match raw_rx.recv_timeout(DEBOUNCE) {
                    Ok(()) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
            // 满了就丢：信号是「去看一眼」，堆积两个和堆积一个是一回事。
            let _ = tx.try_send(());
        }
    });

    Ok((Watch { _inner: w }, rx))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 等一个信号，**区分三种结果**。
    ///
    /// 原来这里是 `timeout(within, rx.recv()).await.is_ok()`，而
    /// `timeout` 返回的是 `Result<Option<()>, Elapsed>` —— `rx.recv()`
    /// 返回 `None`（通道关闭）时内层 future 也算完成，`.is_ok()` 同样
    /// 是 true。于是「监听线程死了」和「收到信号」不可区分：正向断言
    /// 会在监听已经死掉的情况下通过，反向断言会因为通道关闭而失败，
    /// 而两种失败的报错一模一样。
    #[derive(Debug, PartialEq)]
    enum Signal {
        /// 真的收到一个信号。
        Got,
        /// 窗口内没有动静 —— 这才是反向断言要的那个。
        Quiet,
        /// 通道关闭：发端已经没了，之后永远不会再有信号。
        Closed,
    }

    async fn recv(rx: &mut tokio::sync::mpsc::Receiver<()>, within: Duration) -> Signal {
        match tokio::time::timeout(within, rx.recv()).await {
            Ok(Some(())) => Signal::Got,
            Ok(None) => Signal::Closed,
            Err(_) => Signal::Quiet,
        }
    }

    /// 等到没有动静为止，然后才开始断言「不该有动静」。
    ///
    /// **macOS 的 FSEvents 会把建流之前刚发生的事件投递进来。**每个测试
    /// 都是先 `write(config.yaml)` 再 `watch()`，而那次写的事件完全可能
    /// 在流建立之后才到 —— 于是一条「旁边的文件不该吵醒我们」的断言，
    /// 收到的是自己刚才那次写。
    ///
    /// 其他测试碰不到这个：它们先断言「收到一个」，那一个正好把它吃掉了。
    /// 只有纯反向断言的测试会被它打中，而它在 CI 上的时序比本机紧，所以
    /// 只在 CI 上红。
    ///
    /// 不能无脑 drain 一次就走 —— 那会把真的回归也一起吃掉。这里的判据
    /// 是「连续一个去抖窗口内什么都没来」，也就是真的静下来了。
    async fn settle(rx: &mut tokio::sync::mpsc::Receiver<()>) {
        for _ in 0..20 {
            match recv(rx, DEBOUNCE + Duration::from_millis(100)).await {
                Signal::Got => continue,
                Signal::Quiet => return,
                Signal::Closed => panic!("监听在测试开始之前就死了"),
            }
        }
        panic!("监听一直在发信号，静不下来");
    }

    #[tokio::test]
    async fn an_edit_produces_exactly_one_signal() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.yaml");
        std::fs::write(&p, "a: 1\n").unwrap();
        let (_w, mut rx) = watch(&p).unwrap();

        std::fs::write(&p, "a: 2\n").unwrap();
        assert_eq!(
            recv(&mut rx, Duration::from_secs(3)).await,
            Signal::Got,
            "没收到信号"
        );
        // 去抖窗口之后不该再来一个
        assert_eq!(
            recv(&mut rx, Duration::from_millis(600)).await,
            Signal::Quiet,
            "一次改动产生了不止一个信号"
        );
    }

    #[tokio::test]
    async fn a_save_that_writes_a_temp_file_and_renames_still_works() {
        // **编辑器就是这么保存的。**盯着文件而不是目录的话，这里之后
        // 监听会永久失效，而且不报任何错。
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.yaml");
        std::fs::write(&p, "a: 1\n").unwrap();
        let (_w, mut rx) = watch(&p).unwrap();

        for i in 2..=3 {
            let tmp = d.path().join(format!("config.yaml.tmp{i}"));
            std::fs::write(&tmp, format!("a: {i}\n")).unwrap();
            std::fs::rename(&tmp, &p).unwrap();
            assert_eq!(
                recv(&mut rx, Duration::from_secs(3)).await,
                Signal::Got,
                "第 {i} 次 rename 保存没被发现"
            );
        }
    }

    #[tokio::test]
    async fn a_burst_of_writes_collapses_into_one_signal() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.yaml");
        std::fs::write(&p, "a: 0\n").unwrap();
        let (_w, mut rx) = watch(&p).unwrap();

        for i in 1..=10 {
            std::fs::write(&p, format!("a: {i}\n")).unwrap();
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
    async fn a_sibling_file_does_not_wake_us_up() {
        // 历史目录就在配置旁边，每存一版都会动那个目录。
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.yaml");
        std::fs::write(&p, "a: 1\n").unwrap();
        let (_w, mut rx) = watch(&p).unwrap();
        // 先把建流之前那次写可能产生的事件等掉，否则下面断言的是它。
        settle(&mut rx).await;

        std::fs::write(d.path().join("别的.yaml"), "x\n").unwrap();
        std::fs::create_dir_all(d.path().join("history")).unwrap();
        std::fs::write(d.path().join("history/1-ui-abc.yaml"), "x\n").unwrap();
        assert_eq!(
            recv(&mut rx, Duration::from_millis(800)).await,
            Signal::Quiet,
            "旁边的文件把我们吵醒了"
        );
    }

    #[tokio::test]
    async fn deleting_the_file_is_reported_too() {
        // 删掉配置文件是一件必须知道的事 —— 不然网关会拿着一份内存里的
        // 幽灵配置继续跑，而用户以为自己已经清空了它。
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.yaml");
        std::fs::write(&p, "a: 1\n").unwrap();
        let (_w, mut rx) = watch(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        assert_eq!(
            recv(&mut rx, Duration::from_secs(3)).await,
            Signal::Got,
            "删除没被发现"
        );
    }

    #[tokio::test]
    async fn watching_a_file_that_does_not_exist_yet_still_works() {
        // 首次运行时配置还没生成，而监听可能先起来。
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.yaml");
        let (_w, mut rx) = watch(&p).unwrap();
        std::fs::write(&p, "a: 1\n").unwrap();
        assert_eq!(
            recv(&mut rx, Duration::from_secs(3)).await,
            Signal::Got,
            "新建没被发现"
        );
    }
}
