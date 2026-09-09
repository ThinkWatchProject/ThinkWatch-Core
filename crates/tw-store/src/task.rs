//! 起一个后台任务，把事件流写进库里（DESIGN.md §8）。
//!
//! **它订阅事件总线，而不是被数据面直接调用。**这条边界是刻意的：写库
//! 慢了、库锁住了、磁盘满了，转发这条路上一行代码都不会等它（§4.7）。

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::recorder::Recorder;

/// 多久回收一次。
///
/// 一小时。**不是启动时跑一次就完** —— 一个开着不关的桌面应用会连续跑
/// 好几天，而 body 的保留策略是按天算的。
const GC_EVERY: std::time::Duration = std::time::Duration::from_secs(3600);

/// metadata 留多少天（§8）。
pub const METADATA_KEEP_DAYS: u64 = 90;

/// 起来。返回的 handle 给别的地方查历史用 —— **同一个 Recorder**，
/// 不是第二个连接：两个连接会让「刚写进去的还查不到」变成可能。
pub fn spawn(
    recorder: Recorder,
    mut rx: tokio::sync::broadcast::Receiver<tw_api::Event>,
) -> Arc<Mutex<Recorder>> {
    let shared = Arc::new(Mutex::new(recorder));
    let r = shared.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => r.lock().await.on_event(&ev),
                // **落后了就跳过，不要退出。**广播通道满的时候丢的是
                // 观测记录，而退出会让之后的记录全部丢失。
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("观测落后了 {n} 条，这些请求不会出现在历史里");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    let r = shared.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(GC_EVERY);
        // 第一次立刻跑：上次退出之后攒下的过期数据该清了
        loop {
            tick.tick().await;
            let now = now_ms();
            let freed = r.lock().await.gc(
                now,
                crate::blobs::KEEP_DAYS,
                METADATA_KEEP_DAYS,
                crate::blobs::MAX_BYTES,
            );
            if freed > 0 {
                tracing::info!(mb = freed / 1024 / 1024, "回收了过期的请求体");
            }
        }
    });
    shared
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
