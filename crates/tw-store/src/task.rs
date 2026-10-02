//! 起一个后台任务，把事件流写进库里。
//!
//! **它订阅事件总线，而不是被数据面直接调用。**这条边界是刻意的：写库
//! 慢了、库锁住了、磁盘满了，转发这条路上一行代码都不会等它。

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::blobs::Which;
use crate::recorder::Recorder;

/// 一份等着落盘的 body。
///
/// **这个类型属于 store，不属于网关。**让 store 依赖网关会把「观测」
/// 挂到「转发」下面，而它们是两件平级的事（两层划分） ——
/// 网关那边有自己的同形结构，接线在 `twcore` 里完成，那是唯一同时看得
/// 见两边的地方。
#[derive(Debug)]
pub struct StoredBody {
    pub id: u64,
    pub at_ms: i64,
    pub which: Which,
    /// 落盘的那一份：**已经换过、打过码**（网关的 `bodies::BodyRecord::for_disk`）。
    /// 比 [`crate::blobs::MAX_ONE`] 长的由这里截
    pub body: bytes::Bytes,
    /// 原始长度。截断了要能说出来（见 [`crate::Blobs::put_with_len`]）
    pub original_len: usize,
}

/// 把插件的运行记录写进库里，**和正文同一个道理**：数据面只管交出去，写库在这条
/// 任务上。记录和请求那一行各走各的路，落库不分先后（见 `Db::insert_plugin_run`）。
pub fn record_plugin_runs(
    recorder: Arc<Mutex<Recorder>>,
    mut runs: tokio::sync::mpsc::Receiver<crate::db::PluginRunRow>,
) {
    tokio::spawn(async move {
        while let Some(r) = runs.recv().await {
            recorder.lock().await.record_plugin_run(&r);
        }
    });
}

/// 起来。返回的 handle 给别的地方查历史用 —— **同一个 Recorder**，
/// 不是第二个连接：两个连接会让「刚写进去的还查不到」变成可能。
///
/// **回收不在这里。**它按配置里的 `retention` 走（`twcore` 每小时读一次配置，调
/// [`Recorder::gc`]）。这里以前另跑着一个按写死的 7 天、90 天、2 GB 回收的循环，和按
/// 配置的那个同时跑：配得比出厂更宽的期限和上限，每小时被它削回出厂值一次。
pub fn spawn(
    recorder: Recorder,
    mut rx: tokio::sync::broadcast::Receiver<tw_api::Event>,
    mut bodies: tokio::sync::mpsc::Receiver<StoredBody>,
) -> Arc<Mutex<Recorder>> {
    let shared = Arc::new(Mutex::new(recorder));
    let r = shared.clone();
    tokio::spawn(async move {
        while let Some(b) = bodies.recv().await {
            // **写盘在这条任务上，不在转发那条路上。**慢磁盘只会让
            // 通道积压然后丢，不会让请求变慢。
            r.lock()
                .await
                .record_body(b.at_ms, b.id, b.which, &b.body, b.original_len);
        }
    });
    let r = shared.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => r.lock().await.on_event(&ev),
                // **落后了就跳过，不要退出。**广播通道满的时候丢的是
                // 观测记录，而退出会让之后的记录全部丢失。
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("recording fell behind; {n} requests were not recorded");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    shared
}
