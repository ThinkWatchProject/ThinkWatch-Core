//! 事件总线。数据面往里丢，控制面的订阅者往外拿。
//!
//! 用 broadcast 而不是 mpsc，因为可能有多个订阅者（UI 开着、CLI 也在
//! tail）。容量有上限且**满了丢最老的**：观测数据丢几条不影响任何人，
//! 而让数据面因为没人读事件而阻塞是不可接受的 —— **观测的失败不能
//! 拖垮数据面**。
//!
//! 注意这条豁免**不适用于成本记账**：那类数据丢了账单永久对不
//! 上，必须走别的路径。这里只走「丢了只是图上少个点」的东西。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

const CAPACITY: usize = 1024;

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<tw_api::Event>,
    next_id: Arc<AtomicU64>,
    /// 开始了、还没有结局的请求：它们的开始事件，按 id。见 [`EventBus::in_flight`]
    open: Arc<Mutex<BTreeMap<u64, tw_api::Event>>>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CAPACITY);
        Self {
            tx,
            next_id: Arc::new(AtomicU64::new(1)),
            open: Arc::default(),
        }
    }

    /// 拿一个请求 id。同一个请求的四个事件共用它。
    /// 现在发到第几号了，**不占号**。
    ///
    /// 给 `load-balance` 当轮转的种子用：它要一个单调、便宜、
    /// 每个请求都不同的数，而事件序号正好是。**不能用 `next_id`** ——
    /// 那会凭空占掉一个号，让事件流里出现一个不存在的 id。
    pub fn peek_id(&self) -> u64 {
        self.next_id.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// 从这个号之后接着发。
    ///
    /// **计数器出厂从 1 开始，而库是跨重启的。**存储层落库用的是
    /// `INSERT OR REPLACE`（同一个请求要写两次），所以重启后重新发出
    /// 的号会一条一条顶掉历史上的同号记录。启动时把已经用掉的最大号
    /// 交给它，那条路就断了。
    ///
    /// **只进不退**：同一个进程里被调用两次，或者调用时已经发过号了，
    /// 都不会把计数器往回拨 —— 往回拨就是在制造重号。
    pub fn resume_after(&self, id: u64) {
        self.next_id.fetch_max(id + 1, Ordering::Relaxed);
    }

    /// 发一条。**没有订阅者不是错误** —— 没开 UI 的时候数据面照常跑。
    ///
    /// **先记账再发**（见 [`EventBus::in_flight`]）：订阅者收到一个结局的
    /// 时候，快照里已经没有它了。
    pub fn emit(&self, ev: tw_api::Event) {
        self.track(&ev);
        let _ = self.tx.send(ev);
    }

    /// 开始的记下，有了结局的划掉。
    ///
    /// **这里什么都不能 panic。**`Ending` 在 Drop 里也会走到这儿，那时可能
    /// 正处在一次 unwind 里 —— 再 panic 一次，整个进程就没了。锁中毒了也照样
    /// 拿里面的表：少记一笔的快照，好过一个没了的网关。
    fn track(&self, ev: &tw_api::Event) {
        use tw_api::Event as E;
        let mut open = self.open.lock().unwrap_or_else(|p| p.into_inner());
        match ev {
            E::RequestStarted { id, .. } => {
                open.insert(*id, ev.clone());
            }
            E::RequestFinished { id, .. }
            | E::RequestFailed { id, .. }
            | E::RequestCancelled { id, .. } => {
                open.remove(id);
            }
            _ => {}
        }
    }

    /// 此刻还在跑的请求：它们的开始事件，**原样**，按 id 从小到大。
    ///
    /// 给**半路才来听的一方**用。事件流只送订阅之后发生的事，一个在那之前
    /// 就开始、此刻还没结束的请求，它的开始事件早就发过了 —— 听的人数
    /// 「进行中」时就漏掉它，直到它结束。桌面版概览的实时档每次打开都是
    /// 这样。把这份快照当成补发的开始事件处理，就和从头听起一样。
    ///
    /// **和 `Status::in_flight` 不是一个数。**那个从连接进到数据面就算，比
    /// 开始事件早，排队等名额的、鉴权没过的都在里面 —— 它回答的是「现在
    /// 重启会掐断几个连接」。这里只有发过开始事件的请求，和事件流里说的是
    /// 同一批。
    ///
    /// 表不会只进不出：每个开始事件都欠着恰好一个结局，由 `Ending` 保证。
    pub fn in_flight(&self) -> Vec<tw_api::Event> {
        self.open
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<tw_api::Event> {
        self.tx.subscribe()
    }

    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(id: u64) -> tw_api::Event {
        tw_api::Event::RequestStarted {
            key_masked: None,
            peer: None,
            id,
            client: "c".into(),
            client_hint: None,
            session_fp: None,
            provider: "p".into(),
            model: "m".into(),
            method: "POST".into(),
            path: "/v1/messages".into(),
            at_ms: 1_000 + id,
        }
    }

    /// 快照里是**开始了、还没有结局**的那些。三种结局都算结局，响应头不算。
    #[test]
    fn in_flight_is_what_started_and_has_not_ended() {
        let b = EventBus::new();
        for id in 1..=4 {
            b.emit(started(id));
        }
        b.emit(tw_api::Event::RequestFinished {
            id: 1,
            model: "m".into(),
            status: 200,
            bytes: 0,
            duration_ms: 0,
            usage: None,
        });
        b.emit(tw_api::Event::RequestFailed {
            id: 2,
            model: "m".into(),
            source: "upstream".into(),
            message: tw_api::Msg {
                code: "t.x".into(),
                args: Default::default(),
                text: "x".into(),
            },
            bytes: None,
            duration_ms: None,
            usage: None,
        });
        b.emit(tw_api::Event::RequestCancelled {
            id: 3,
            model: "m".into(),
            status: None,
            bytes: 0,
            duration_ms: 0,
            usage: None,
        });
        b.emit(tw_api::Event::RequestHeaders {
            id: 4,
            status: 200,
            ttfb_ms: 5,
        });

        let open = b.in_flight();
        assert_eq!(open.iter().map(|e| e.id()).collect::<Vec<_>>(), [4]);
        // **原样**：听的人拿它当补发的开始事件，字段一个都不能少
        assert!(
            matches!(&open[0], tw_api::Event::RequestStarted { model, at_ms: 1004, .. } if model == "m"),
            "{open:?}"
        );
    }

    /// 没有订阅者的时候照样记 —— 这份快照就是给还没来的订阅者准备的。
    /// 按 id 排，也就是按开始的先后。
    #[test]
    fn in_flight_is_kept_with_no_subscriber_and_in_start_order() {
        let b = EventBus::new();
        assert!(b.in_flight().is_empty());
        for id in [7, 3, 5] {
            b.emit(started(id));
        }
        assert_eq!(b.subscriber_count(), 0);
        assert_eq!(
            b.in_flight().iter().map(|e| e.id()).collect::<Vec<_>>(),
            [3, 5, 7]
        );
    }

    #[test]
    fn ids_resume_after_the_number_the_store_already_has() {
        let b = EventBus::new();
        b.resume_after(1258);
        assert_eq!(b.next_id(), 1259);
        assert_eq!(b.next_id(), 1260);
    }

    #[test]
    fn resuming_never_hands_out_a_number_twice() {
        // **只进不退。**往回拨就是在制造重号，而重号在存储层是覆盖。
        let b = EventBus::new();
        assert_eq!(b.next_id(), 1);
        assert_eq!(b.next_id(), 2);
        b.resume_after(0);
        assert_eq!(b.next_id(), 3, "被拨回去了");
        b.resume_after(100);
        b.resume_after(50);
        assert_eq!(b.next_id(), 101);
    }

    #[tokio::test]
    async fn emitting_with_no_subscribers_is_not_an_error() {
        // 没开 UI 的时候数据面照常跑 —— 这是最常见的状态。
        let b = EventBus::new();
        b.emit(tw_api::Event::RequestFinished {
            id: 1,
            model: String::new(),
            status: 200,
            bytes: 0,
            duration_ms: 0,
            usage: None,
        });
        assert_eq!(b.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn a_subscriber_receives_what_is_emitted_after_it_subscribes() {
        let b = EventBus::new();
        let mut rx = b.subscribe();
        b.emit(tw_api::Event::RequestFinished {
            id: 42,
            model: String::new(),
            status: 200,
            bytes: 1,
            duration_ms: 2,
            usage: None,
        });
        assert_eq!(rx.recv().await.unwrap().id(), 42);
    }

    #[tokio::test]
    async fn ids_are_unique_and_monotonic() {
        let b = EventBus::new();
        let a = b.next_id();
        let c = b.next_id();
        assert!(c > a);
    }

    #[tokio::test]
    async fn a_slow_subscriber_lags_but_does_not_block_the_producer() {
        // 这是关键性质：观测端跟不上，数据面**不能**因此变慢。
        let b = EventBus::new();
        let mut rx = b.subscribe();
        for i in 0..(CAPACITY as u64 + 100) {
            b.emit(tw_api::Event::RequestFinished {
                id: i,
                model: String::new(),
                status: 200,
                bytes: 0,
                duration_ms: 0,
                usage: None,
            });
        }
        // 生产端全程没阻塞；消费端会收到一个 Lagged
        match rx.recv().await {
            Err(broadcast::error::RecvError::Lagged(n)) => assert!(n > 0),
            other => panic!("应该 lag，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn two_subscribers_both_get_the_event() {
        // UI 开着、CLI 也在 tail —— 这是支持的用法。
        let b = EventBus::new();
        let mut a = b.subscribe();
        let mut c = b.subscribe();
        b.emit(tw_api::Event::RequestFinished {
            id: 9,
            model: String::new(),
            status: 200,
            bytes: 0,
            duration_ms: 0,
            usage: None,
        });
        assert_eq!(a.recv().await.unwrap().id(), 9);
        assert_eq!(c.recv().await.unwrap().id(), 9);
    }
}
