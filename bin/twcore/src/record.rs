//! 请求记录：观测层怎么接上网关。
//!
//! **每一步失败都只是「没有历史记录」，不是「起不来」。**这条边界写死在代码形状里：
//! 这里交出去的是 `Option`，而不是 `Result` —— 调用方连处理错误的机会都不该有，因为
//! 没有任何一种处理方式是「不转发了」。
//!
//! # 接替者晚一点打开（升级不断线）
//!
//! 交接之后旧 core 还在排空，**它还在写自己的记录**。新 core 这时候打开请求库，两个进程
//! 一起写；新版本提高了 SCHEMA 的话，新的一打开就把旧库重建了，旧 core 写的全丢。所以
//! 接替者（[`open_later`]）先给控制面一个占位的（内存库，查历史是空的），等旧 core 的
//! 进程没了才打开真的库；之间的记录在内存里排队（[`Queue`]），满了丢最早的、记个数。
//!
//! 正文不排队：存储层不收，网关那一头按字节记账的队列（`bodies::QUEUED_MAX`）满了自己丢，
//! 一样不挡转发。

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

pub type Store = Arc<tokio::sync::Mutex<tw_store::Recorder>>;

/// 观测层接上网关的那几头。
pub struct Wiring {
    /// **收整条总线，不只是一个订阅端。**存储层算完价钱要往回报一条
    /// （见 `Event::RequestPriced`）—— 它是这条链上唯一知道单价的地方。
    pub bus: tw_observe::EventBus,
    /// **和网关同一份价格簿**，不是一份副本：改了价目表，下一个结束的请求就按新价算
    pub pricing: tw_pricing::Shared,
    pub bodies: tokio::sync::mpsc::Receiver<tw_gateway::bodies::BodyRecord>,
    pub runs: tokio::sync::mpsc::Receiver<tw_gateway::plugin::RunRecord>,
    /// 每记下一行请求，交给网关的密钥用量上限结算（见 `tw_control::key_limits`）
    pub settled: tw_store::SettleHook,
}

/// 现在就打开（平常的启动）。
pub fn open_now(dir: &Path, w: Wiring) -> Option<Store> {
    // **存储层自己的那条通道，不订阅界面的广播**（见 `EventBus::record_feed`）：广播上
    // 掉队丢的是界面上的几个点，记录丢的是那几个请求
    let Some(events) = w.bus.record_feed() else {
        tracing::warn!(
            "request recording is already attached to the event bus; not starting a second one"
        );
        return None;
    };
    let (db, blobs) = match tw_store::open(dir) {
        Ok(opened) => opened,
        Err(e) => {
            tracing::warn!(
                "request recording could not start, so nothing is recorded this run; forwarding is unaffected: {e}"
            );
            return None;
        }
    };
    resume_ids(&w.bus, &db);
    let tx = bodies_to_disk(w.bodies);
    let recorder = tw_store::task::spawn(
        // 算完价钱往回报一条 —— 见 `Event::RequestPriced`。这里是唯一
        // 同时看得见总线和存储层的地方，所以接线在这儿完成。
        tw_store::Recorder::new(db, blobs, w.pricing)
            .reporting_to(w.bus)
            .settling_to(w.settled),
        events,
        tx,
    );
    tw_store::task::record_plugin_runs(recorder.clone(), plugin_runs(w.runs));
    Some(recorder)
}

/// 接替者：旧 core（`predecessor`）的进程没了才打开请求库，之间的记录排队。
///
/// 交出去的是占位的那一份（内存库）；打开之后**原地换成真的**，控制面拿着的还是同一个
/// `Arc`。`on_open` 在真的库打开、排队的记录写进去之前调一次（密钥用量从记录重建，和
/// 平常启动时同一个时机：在这个进程自己的记录进库之前）。返回的 `watch` 在打开之后
/// （或者打开失败、不再等之后）变成 `true`。
pub fn open_later(
    dir: &Path,
    w: Wiring,
    predecessor: u32,
    on_open: Box<dyn FnOnce(&tw_store::Db) + Send>,
) -> (Option<Store>, watch::Receiver<bool>) {
    let (opened_tx, opened) = watch::channel(false);
    let Some(mut events) = w.bus.record_feed() else {
        tracing::warn!(
            "request recording is already attached to the event bus; not starting a second one"
        );
        opened_tx.send_replace(true);
        return (None, opened);
    };
    let placeholder = match tw_store::Db::in_memory() {
        Ok(db) => tw_store::Recorder::new(
            db,
            tw_store::Blobs::new(dir.join("blobs")),
            w.pricing.clone(),
        ),
        Err(e) => {
            tracing::warn!(
                "request recording could not start, so nothing is recorded this run; forwarding is unaffected: {e}"
            );
            opened_tx.send_replace(true);
            return (None, opened);
        }
    };
    let store: Store = Arc::new(tokio::sync::Mutex::new(placeholder));
    // 正文这一头先不收（见模块文档）；插件的运行记录也等库开了再接
    let body_rx = bodies_to_disk(w.bodies);
    let run_rx = plugin_runs(w.runs);
    let shared = store.clone();
    let dir = dir.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("tw-recorder".into())
        .spawn(move || {
            let mut queue = Queue::default();
            wait_for_exit(predecessor, &mut events, &mut queue);
            let (db, blobs) = match tw_store::open(&dir) {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!(
                        "request recording could not start, so nothing is recorded this run; forwarding is unaffected: {e}"
                    );
                    opened_tx.send_replace(true);
                    // 通道要有人收，否则总线那一头一直记「丢了」
                    while events.blocking_recv().is_some() {}
                    return;
                }
            };
            resume_ids(&w.bus, &db);
            on_open(&db);
            let blobs_root = blobs.root().to_path_buf();
            *shared.blocking_lock() = tw_store::Recorder::new(db, blobs, w.pricing)
                .reporting_to(w.bus)
                .settling_to(w.settled);
            spawn_body_writer(blobs_root, body_rx);
            tw_store::task::record_plugin_runs(shared.clone(), run_rx);
            let (n, dropped) = (queue.len(), queue.dropped);
            if dropped > 0 {
                tracing::warn!(
                    dropped,
                    "the request records queued while the previous core drained overflowed; the oldest {dropped} events were not recorded"
                );
            }
            {
                let mut g = shared.blocking_lock();
                for ev in queue.drain() {
                    g.on_event(&ev);
                }
            }
            tracing::info!(queued = n, "the previous core has exited; request recording is open");
            opened_tx.send_replace(true);
            record_loop(&shared, &mut events);
        });
    if let Err(e) = spawned {
        tracing::warn!("the recorder thread could not start, so it records nothing this run: {e}");
        return (None, opened);
    }
    (Some(store), opened)
}

/// 旧 core 最多排空这么久（`handover::DRAIN_LIMIT` 加一点余量）。到了还没退，照样打开
const PREDECESSOR_WAIT: Duration = Duration::from_secs(35 * 60);

/// 等旧 core 的进程没了，之间把记录收进队列。
fn wait_for_exit(pid: u32, events: &mut tw_observe::RecordFeed, queue: &mut Queue) {
    let started = Instant::now();
    let mut last_check = Instant::now() - Duration::from_secs(1);
    loop {
        while let Some(ev) = events.try_recv() {
            queue.push(ev);
        }
        queue.dropped += events.take_dropped();
        if last_check.elapsed() >= Duration::from_millis(500) {
            last_check = Instant::now();
            if !crate::proc::alive(pid) {
                return;
            }
            if started.elapsed() >= PREDECESSOR_WAIT {
                tracing::warn!(
                    pid,
                    "the previous core is still running long past its drain limit; opening the request history anyway"
                );
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 记录线程拿着锁一次最多吃几条事件（同 `tw_store::task`）。
const BATCH: usize = 64;

/// 平常的记录循环，和 `tw_store::task::spawn` 里那条一样。
fn record_loop(shared: &Store, events: &mut tw_observe::RecordFeed) {
    while let Some(ev) = events.blocking_recv() {
        {
            let mut g = shared.blocking_lock();
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
}

/// 一个请求大约几条事件（开始、响应头、首字、路由、结局、价钱）。排队的上限按请求数说，
/// 数的是事件
const EVENTS_PER_RECORD: usize = 6;
/// 排队最多多少条记录（方案 9.4）
pub const QUEUE_MAX_RECORDS: usize = 10_000;
/// 排队最多多少字节（方案 9.4）
pub const QUEUE_MAX_BYTES: usize = 256 * 1024 * 1024;

/// 接替者等旧 core 退出时排着的记录。**满了丢最早的**，记个数。
pub struct Queue {
    items: VecDeque<(tw_api::Event, usize)>,
    bytes: usize,
    max_events: usize,
    max_bytes: usize,
    pub dropped: u64,
}

impl Default for Queue {
    fn default() -> Self {
        Self::with_limits(QUEUE_MAX_RECORDS * EVENTS_PER_RECORD, QUEUE_MAX_BYTES)
    }
}

impl Queue {
    pub fn with_limits(max_events: usize, max_bytes: usize) -> Self {
        Self {
            items: VecDeque::new(),
            bytes: 0,
            max_events,
            max_bytes,
            dropped: 0,
        }
    }

    pub fn push(&mut self, ev: tw_api::Event) {
        // 按序列化之后的长度记：事件里没有正文，最长的是带错误原因、内容命中的那几条
        let size = serde_json::to_vec(&ev).map(|v| v.len()).unwrap_or(256);
        while !self.items.is_empty()
            && (self.items.len() >= self.max_events || self.bytes + size > self.max_bytes)
        {
            if let Some((_, n)) = self.items.pop_front() {
                self.bytes -= n;
                self.dropped += 1;
            }
        }
        if size > self.max_bytes {
            self.dropped += 1;
            return;
        }
        self.bytes += size;
        self.items.push_back((ev, size));
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn drain(&mut self) -> impl Iterator<Item = tw_api::Event> + '_ {
        self.bytes = 0;
        self.items.drain(..).map(|(e, _)| e)
    }
}

/// **接着上次的号往下发。**这里是唯一同时看得见总线和库的地方。读不到就照常跑：那顶多
/// 是这一次重启又压掉几条历史，比起因为一次查询失败而不记录任何东西要好得多。
fn resume_ids(bus: &tw_observe::EventBus, db: &tw_store::Db) {
    match db.last_request_id() {
        Ok(0) => {}
        Ok(last) => {
            tracing::debug!(last, "request ids resume from where they left off");
            bus.resume_after(last);
        }
        Err(e) => tracing::warn!(
            "could not read the last request id, so this run may overwrite the oldest records: {e}"
        ),
    }
}

/*
  两边的 body 结构在这里对接，**落盘的不是原文**：脱敏规则认得出的值在这里换掉、
  打码（`BodyRecord::for_disk`，见 `tw_gateway::bodies`）。放在阻塞线程上 ——
  一份 4 MB 的正文要扫好几遍，占着异步线程的话，同一个线程上的转发都得等它。

  交给存储层的这一头**只留一个空位**。等着落盘的正文由网关那一头按字节记账
  （`bodies::QUEUED_MAX`），一份正文的额度要等存储层收下它才还回去；这里再开一个
  大口子的话，攒在这里的那些就没人管了。
*/
fn bodies_to_disk(
    mut bodies: tokio::sync::mpsc::Receiver<tw_gateway::bodies::BodyRecord>,
) -> tokio::sync::mpsc::Receiver<tw_store::StoredBody> {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        while let Some(b) = bodies.recv().await {
            let Ok(disk) = tokio::task::spawn_blocking(move || b.for_disk()).await else {
                continue;
            };
            let tw_gateway::bodies::ForDisk {
                id,
                at_ms,
                kind,
                body,
                original_len,
                held,
            } = disk;
            let mapped = tw_store::StoredBody {
                id,
                at_ms,
                which: match kind {
                    tw_gateway::bodies::BodyKind::Request => tw_store::Which::Request,
                    tw_gateway::bodies::BodyKind::Response => tw_store::Which::Response,
                    tw_gateway::bodies::BodyKind::AfterPlugins => tw_store::Which::AfterPlugins,
                    tw_gateway::bodies::BodyKind::UpstreamRequest => {
                        tw_store::Which::UpstreamRequest
                    }
                    tw_gateway::bodies::BodyKind::ClientResponse => tw_store::Which::ClientResponse,
                    tw_gateway::bodies::BodyKind::Heads => tw_store::Which::Heads,
                },
                body,
                original_len,
            };
            if tx.send(mapped).await.is_err() {
                return;
            }
            // 存储层收下了：额度还回去
            drop(held);
        }
    });
    rx
}

/// 接替者的正文写盘线程（平常由 `tw_store::task::spawn` 起）。
fn spawn_body_writer(
    root: std::path::PathBuf,
    mut rx: tokio::sync::mpsc::Receiver<tw_store::StoredBody>,
) {
    let blobs = tw_store::Blobs::new(root);
    let spawned = std::thread::Builder::new()
        .name("tw-bodies".into())
        .spawn(move || {
            while let Some(b) = rx.blocking_recv() {
                blobs.put_with_len(b.at_ms, b.id as i64, b.which, &b.body, b.original_len);
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("the tw-bodies thread could not start, so it records nothing this run: {e}");
    }
}

/// 插件的运行记录同样在这里对接：网关那边的一次运行，换成存储层的一行
fn plugin_runs(
    mut runs: tokio::sync::mpsc::Receiver<tw_gateway::plugin::RunRecord>,
) -> tokio::sync::mpsc::Receiver<tw_store::PluginRunRow> {
    let (run_tx, run_rx) = tokio::sync::mpsc::channel(tw_gateway::plugin::RUN_CHANNEL_CAP);
    tokio::spawn(async move {
        while let Some(r) = runs.recv().await {
            let row = tw_store::PluginRunRow {
                request_id: r.request_id as i64,
                at_ms: r.at_ms as i64,
                plugin_id: r.run.plugin_id,
                plugin_name: r.run.plugin_name,
                hook: r.run.hook,
                outcome: r.run.outcome,
                error: r.run.error,
                cpu_us: r.run.cpu_us.min(i64::MAX as u64) as i64,
                detail: r.run.detail.map(|d| d.to_string()),
            };
            if run_tx.send(row).await.is_err() {
                return;
            }
        }
    });
    run_rx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: u64) -> tw_api::Event {
        tw_api::Event::ConfigReloaded {
            id,
            version: "v".into(),
            origin: tw_api::ConfigOrigin::External,
            at_ms: 0,
        }
    }

    fn ids(q: &mut Queue) -> Vec<u64> {
        q.drain()
            .map(|e| match e {
                tw_api::Event::ConfigReloaded { id, .. } => id,
                _ => unreachable!(),
            })
            .collect()
    }

    #[test]
    fn a_full_queue_drops_the_oldest_and_counts_them() {
        let mut q = Queue::with_limits(3, usize::MAX);
        for i in 1..=5 {
            q.push(ev(i));
        }
        assert_eq!(q.dropped, 2);
        assert_eq!(ids(&mut q), [3, 4, 5], "留下的是最新的");
    }

    #[test]
    fn the_byte_limit_also_drops_the_oldest() {
        let one = serde_json::to_vec(&ev(1)).unwrap().len();
        let mut q = Queue::with_limits(usize::MAX, one * 2 + one / 2);
        for i in 1..=4 {
            q.push(ev(i));
        }
        assert_eq!(q.len(), 2);
        assert_eq!(q.dropped, 2);
        assert_eq!(ids(&mut q), [3, 4]);
    }

    #[test]
    fn the_default_limits_are_the_documented_ones() {
        let q = Queue::default();
        assert_eq!(q.max_events, QUEUE_MAX_RECORDS * EVENTS_PER_RECORD);
        assert_eq!(q.max_bytes, 256 * 1024 * 1024);
    }
}
