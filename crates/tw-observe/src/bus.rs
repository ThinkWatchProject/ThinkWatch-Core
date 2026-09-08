//! 事件总线。数据面往里丢，控制面的订阅者往外拿。
//!
//! 用 broadcast 而不是 mpsc，因为可能有多个订阅者（UI 开着、CLI 也在
//! tail）。容量有上限且**满了丢最老的**：观测数据丢几条不影响任何人，
//! 而让数据面因为没人读事件而阻塞是不可接受的（§4.7「观测的失败不能
//! 拖垮数据面」）。
//!
//! 注意这条豁免**不适用于成本记账**（§4.7）：那类数据丢了账单永久对不
//! 上，必须走别的路径。这里只走「丢了只是图上少个点」的东西。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::broadcast;

const CAPACITY: usize = 1024;

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<tw_api::Event>,
    next_id: Arc<AtomicU64>,
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
        }
    }

    /// 拿一个请求 id。同一个请求的四个事件共用它。
    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// 发一条。**没有订阅者不是错误** —— 没开 UI 的时候数据面照常跑。
    pub fn emit(&self, ev: tw_api::Event) {
        let _ = self.tx.send(ev);
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

    #[tokio::test]
    async fn emitting_with_no_subscribers_is_not_an_error() {
        // 没开 UI 的时候数据面照常跑 —— 这是最常见的状态。
        let b = EventBus::new();
        b.emit(tw_api::Event::RequestFinished {
            id: 1,
            status: 200,
            bytes: 0,
            duration_ms: 0,
        });
        assert_eq!(b.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn a_subscriber_receives_what_is_emitted_after_it_subscribes() {
        let b = EventBus::new();
        let mut rx = b.subscribe();
        b.emit(tw_api::Event::RequestFinished {
            id: 42,
            status: 200,
            bytes: 1,
            duration_ms: 2,
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
                status: 200,
                bytes: 0,
                duration_ms: 0,
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
            status: 200,
            bytes: 0,
            duration_ms: 0,
        });
        assert_eq!(a.recv().await.unwrap().id(), 9);
        assert_eq!(c.recv().await.unwrap().id(), 9);
    }
}
