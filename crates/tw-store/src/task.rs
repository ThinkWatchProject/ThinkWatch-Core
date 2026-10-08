//! 起几个后台线程，把事件流写进库里、把正文写到盘上。
//!
//! **它收事件总线上的事件，而不是被数据面直接调用。**这条边界是刻意的：写库
//! 慢了、库锁住了、磁盘满了，转发这条路上一行代码都不会等它。
//!
//! **跑在自己的线程上，不在异步运行时里。**写库、写几 MB 的正文、`chmod` 都是会挡住线程的
//! 调用：放在运行时的工作线程上，同一个线程上排着的转发和控制面请求都得等它。

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::blobs::{Blobs, Which};
use crate::recorder::Recorder;

/// 记录线程拿着锁一次最多吃几条事件。**拿着锁的时候查询在等**：一条几十微秒，一批也就
/// 几毫秒；一条一拿又白白多出许多次抢锁
const BATCH: usize = 64;

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
/// 线程上。记录和请求那一行各走各的路，落库不分先后（见 `Db::insert_plugin_run`）。
pub fn record_plugin_runs(
    recorder: Arc<Mutex<Recorder>>,
    mut runs: tokio::sync::mpsc::Receiver<crate::db::PluginRunRow>,
) {
    thread("tw-plugin-runs", move || {
        while let Some(r) = runs.blocking_recv() {
            recorder.blocking_lock().record_plugin_run(&r);
        }
    });
}

/// 起来。返回的 handle 给别的地方查历史用 —— **同一个 Recorder**，
/// 不是第二个连接：两个连接会让「刚写进去的还查不到」变成可能。
///
/// 事件从总线给存储层的那条通道来（[`tw_observe::EventBus::record_feed`]），**不订阅界面的
/// 广播**：那是个一千来格的环，界面落后时存储层多半也落后，丢掉的那几个请求永远不在库里。
///
/// **正文不拿锁**：正文目录只是一个路径，写一份 4 MB 的正文时，记录和查询不必等它。
///
/// **回收不在这里。**它按配置里的 `retention` 走（`twcore` 每小时读一次配置，调 [`gc`]）。
/// 这里以前另跑着一个按写死的 7 天、90 天、2 GB 回收的循环，和按配置的那个同时跑：配得比出厂
/// 更宽的期限和上限，每小时被它削回出厂值一次。
pub fn spawn(
    recorder: Recorder,
    mut events: tw_observe::RecordFeed,
    mut bodies: tokio::sync::mpsc::Receiver<StoredBody>,
) -> Arc<Mutex<Recorder>> {
    let blobs = Blobs::new(recorder.blobs().root().to_path_buf());
    let shared = Arc::new(Mutex::new(recorder));
    thread("tw-bodies", move || {
        // 慢磁盘只会让通道积压然后丢，不会让请求变慢
        while let Some(b) = bodies.blocking_recv() {
            blobs.put_with_len(b.at_ms, b.id as i64, b.which, &b.body, b.original_len);
        }
    });
    let r = shared.clone();
    thread("tw-recorder", move || {
        while let Some(ev) = events.blocking_recv() {
            {
                let mut g = r.blocking_lock();
                g.on_event(&ev);
                for _ in 1..BATCH {
                    let Some(ev) = events.try_recv() else { break };
                    g.on_event(&ev);
                }
            }
            // **落后了就丢，但要说出来。**通道满了丢的是观测记录，而等着它会拖慢转发
            let dropped = events.take_dropped();
            if dropped > 0 {
                tracing::warn!(
                    dropped,
                    "recording fell behind; {dropped} events were not recorded"
                );
            }
        }
    });
    shared
}

/// 回收一轮：过期的正文、超出总量的正文、过期的记录行。返回删掉了多少字节的正文。
///
/// **删正文的时候不拿锁**：正文目录只是一个路径，一次删几个 GB 可以要好几秒，而那期间记录
/// 和查询都要这把锁。删行要拿：行在库里，一小时一轮删的只是这一小时过期的那些。
///
/// 会挡住当前线程：在阻塞线程上调。
pub fn gc(
    shared: &Mutex<Recorder>,
    now_ms: i64,
    keep_days: u64,
    metadata_keep_days: u64,
    max_bytes: u64,
) -> u64 {
    let (blobs, transcripts) = {
        let g = shared.blocking_lock();
        (Blobs::new(g.blobs().root().to_path_buf()), g.transcripts())
    };
    let freed = blobs.gc(now_ms, keep_days, max_bytes);
    // 删掉的正文可能在记着的对话里：记着的那些作废，下次重读
    if freed > 0 {
        transcripts.invalidate();
    }
    shared.blocking_lock().prune(now_ms, metadata_keep_days);
    freed
}

/// 起一条有名字的线程。**起不来只记一行**：那时这一路什么都不记，转发照常
fn thread(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(f) {
        tracing::warn!("the {name} thread could not start, so it records nothing this run: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Ask;

    const NOW: i64 = 1_790_000_000_000;
    const DAY: i64 = 86_400_000;

    /// 回收删了正文，记着的对话跟着作废：之后读出来的那一轮说出缺了什么，而不是删掉之前的
    /// 样子
    #[test]
    fn reclaiming_bodies_forgets_the_transcripts_read_from_them() {
        let d = tempfile::tempdir().unwrap();
        let rec = Recorder::new(
            crate::Db::in_memory().unwrap(),
            Blobs::new(d.path().join("blobs")),
            tw_pricing::shared(tw_pricing::PriceBook::builtin().unwrap()),
        );
        let mut row = crate::db::tests::row(1, NOW);
        row.session = Some("s".into());
        rec.db().insert(&row).unwrap();
        let request =
            br#"{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"hi"}]}"#;
        let answer = br#"{"type":"message","content":[{"type":"text","text":"hello"}]}"#;
        rec.record_body(NOW, 1, Which::Request, request, request.len());
        rec.record_body(NOW, 1, Which::Response, answer, answer.len());
        let shared = Mutex::new(rec);
        let read = || {
            let (rows, blobs, transcripts) = {
                let g = shared.blocking_lock();
                (
                    g.db().session_requests("s").unwrap(),
                    Blobs::new(g.blobs().root().to_path_buf()),
                    g.transcripts(),
                )
            };
            transcripts.read(
                &blobs,
                &Ask {
                    session: "s",
                    rows: &rows,
                    from_turn: 0,
                    running: None,
                    now_ms: NOW + 3 * DAY,
                },
            )
        };
        let first = read();
        assert!(first.turns[0].gaps.is_empty(), "{first:?}");
        assert!(!first.turns[0].output.is_empty());

        assert!(gc(&shared, NOW + 3 * DAY, 1, 90, u64::MAX) > 0);
        let again = read();
        assert_eq!(
            again.turns[0].gaps,
            [
                tw_api::TranscriptGap::RequestMissing,
                tw_api::TranscriptGap::ResponseMissing
            ]
        );
    }
}
