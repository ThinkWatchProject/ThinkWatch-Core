//! 并发上限：**排队，不要拒绝**。
//!
//! 一个失控的脚本能在几秒内打出几百个请求，打爆额度，也会撞上游的限流。
//! 所以要有上限（DESIGN.md §4.7）。
//!
//! 但**超限时应该排队而不是拒绝**：客户端收到 429 通常不会优雅重试，
//! 你会看到一堆莫名其妙的失败；排队则只是变慢。这个区别对 Claude Code
//! 这类客户端尤其重要 —— 它把 429 当成硬失败，一个本来只需要多等两秒的
//! 请求会变成一次任务中断。
//!
//! 队列必须有上限和超时：失控的脚本会把队列撑爆，内存跟着涨 —— **那比
//! 拒绝更糟**。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub use tw_types::Limits;

#[derive(Debug, thiserror::Error)]
pub enum LimitError {
    #[error("排队超过 {0:?} 还没轮到。上游可能全都卡住了，或者并发上限设得太低。")]
    Timeout(Duration),
    #[error("等待队列已满（{0} 个）。有东西在疯狂发请求 —— 检查一下是不是有脚本失控了。")]
    QueueFull(usize),
}

/// 三个维度叠加，取最严的那个（§4.7）。
pub struct Gate {
    limits: Limits,
    global: Arc<Semaphore>,
    per_provider: std::sync::Mutex<HashMap<String, Arc<Semaphore>>>,
    per_client: std::sync::Mutex<HashMap<String, Arc<Semaphore>>>,
    /// 正在排队的数量。**这是队列上限的依据** —— 不是信号量的等待者数，
    /// 因为那个数拿不到。
    queued: Arc<std::sync::atomic::AtomicUsize>,
}

/// 拿到的通行证。**Drop 时自动归还** —— 手工释放迟早会在某条错误路径上
/// 漏掉，而漏掉的表现是并发数只减不增，最后所有请求一起卡死。
#[derive(Debug)]
pub struct Pass {
    _permits: Vec<OwnedSemaphorePermit>,
}

impl Gate {
    pub fn new(limits: Limits) -> Self {
        Self {
            global: Arc::new(Semaphore::new(limits.max_concurrent)),
            per_provider: Default::default(),
            per_client: Default::default(),
            queued: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            limits,
        }
    }

    fn sem(
        map: &std::sync::Mutex<HashMap<String, Arc<Semaphore>>>,
        key: &str,
        n: usize,
    ) -> Arc<Semaphore> {
        map.lock()
            .unwrap()
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(n)))
            .clone()
    }

    /// 取一张通行证，排队等着。
    ///
    /// `client_limit` 是这个客户端自己的上限（§3.3.1 的 `max_concurrent`）。
    /// 监听局域网时这是刚需 —— **某台机器上的失控脚本不该能占满全部并发**。
    pub async fn acquire(
        &self,
        provider: &str,
        client: &str,
        client_limit: Option<usize>,
    ) -> Result<Pass, LimitError> {
        use std::sync::atomic::Ordering;

        let depth = self.queued.fetch_add(1, Ordering::SeqCst);
        // 计数器无论走哪条路都要还回去，所以用一个 guard。
        struct Dec(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Dec {
            fn drop(&mut self) {
                self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _dec = Dec(self.queued.clone());

        if depth >= self.limits.queue_depth {
            return Err(LimitError::QueueFull(self.limits.queue_depth));
        }

        let timeout = Duration::from_secs(self.limits.queue_timeout_secs);
        let mut sems = vec![
            self.global.clone(),
            Self::sem(&self.per_provider, provider, self.limits.per_provider),
        ];
        if let Some(n) = client_limit {
            sems.push(Self::sem(&self.per_client, client, n));
        }

        // **按固定顺序取**（全局 → provider → client）。顺序不固定的话
        // 两个请求可能各拿一半然后互等 —— 一个只在高并发下偶发的死锁，
        // 是最难查的那种。
        let acquire_all = async {
            let mut permits = Vec::with_capacity(sems.len());
            for s in sems {
                permits.push(s.acquire_owned().await.expect("信号量不会被关闭"));
            }
            permits
        };
        match tokio::time::timeout(timeout, acquire_all).await {
            Ok(permits) => Ok(Pass { _permits: permits }),
            Err(_) => Err(LimitError::Timeout(timeout)),
        }
    }

    pub fn queued(&self) -> usize {
        self.queued.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(max: usize, per: usize, depth: usize, timeout: u64) -> Gate {
        Gate::new(Limits {
            max_concurrent: max,
            per_provider: per,
            queue_depth: depth,
            queue_timeout_secs: timeout,
        })
    }

    #[tokio::test]
    async fn under_the_limit_everything_passes_immediately() {
        let g = gate(4, 4, 64, 30);
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(g.acquire("p", "c", None).await.unwrap());
        }
        assert_eq!(held.len(), 4);
    }

    #[tokio::test]
    async fn over_the_limit_it_queues_rather_than_rejecting() {
        // **这是这个模块存在的理由。**客户端收到 429 通常不会优雅重试，
        // 一个本来只需要多等两秒的请求会变成一次任务中断。
        let g = Arc::new(gate(1, 1, 64, 30));
        let first = g.acquire("p", "c", None).await.unwrap();

        let g2 = g.clone();
        let waiter = tokio::spawn(async move { g2.acquire("p", "c", None).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "第二个应该在排队，而不是被拒");

        drop(first);
        assert!(waiter.await.unwrap().is_ok(), "前一个走了就该轮到它");
    }

    #[tokio::test]
    async fn a_permit_is_returned_when_it_is_dropped() {
        // 手工释放迟早会在某条错误路径上漏掉，而漏掉的表现是并发数
        // 只减不增，最后所有请求一起卡死。
        let g = gate(1, 1, 64, 1);
        {
            let _p = g.acquire("p", "c", None).await.unwrap();
        }
        // 没有归还的话这里会等满超时
        assert!(g.acquire("p", "c", None).await.is_ok());
    }

    #[tokio::test]
    async fn queuing_too_long_gives_up_with_a_message_that_names_the_cause() {
        let g = Arc::new(gate(1, 1, 64, 1));
        let _held = g.acquire("p", "c", None).await.unwrap();
        let e = g.acquire("p", "c", None).await.unwrap_err();
        assert!(matches!(e, LimitError::Timeout(_)));
        assert!(e.to_string().contains("并发上限"), "{e}");
    }

    #[tokio::test]
    async fn a_full_queue_is_the_one_case_that_really_refuses() {
        // 队列必须有上限：失控的脚本会把队列撑爆，内存跟着涨 ——
        // 那比拒绝更糟。
        let g = Arc::new(gate(1, 1, 2, 30));
        let _held = g.acquire("p", "c", None).await.unwrap();
        let mut waiters = Vec::new();
        for _ in 0..2 {
            let g2 = g.clone();
            waiters.push(tokio::spawn(
                async move { g2.acquire("p", "c", None).await },
            ));
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
        let e = g.acquire("p", "c", None).await.unwrap_err();
        assert!(matches!(e, LimitError::QueueFull(2)), "{e:?}");
        assert!(e.to_string().contains("脚本"), "错误要指向真正的原因：{e}");
        for w in waiters {
            w.abort();
        }
    }

    #[tokio::test]
    async fn different_providers_do_not_block_each_other() {
        // per_provider 是独立的：一个慢上游不该拖住另一个。
        let g = gate(8, 1, 64, 1);
        let _a = g.acquire("slow", "c", None).await.unwrap();
        assert!(g.acquire("fast", "c", None).await.is_ok());
    }

    #[tokio::test]
    async fn a_per_client_limit_stops_one_machine_taking_everything() {
        // 监听局域网时这是刚需：某台机器上的失控脚本不该能占满全部并发。
        let g = Arc::new(gate(8, 8, 64, 1));
        let _a = g.acquire("p", "greedy", Some(1)).await.unwrap();
        assert!(
            g.acquire("p", "greedy", Some(1)).await.is_err(),
            "同一个客户端的第二个该等"
        );
        assert!(
            g.acquire("p", "other", Some(1)).await.is_ok(),
            "别的客户端不受影响"
        );
    }

    #[tokio::test]
    async fn the_strictest_of_the_three_dimensions_wins() {
        // 全局 8、单上游 1 —— 生效的是 1。
        let g = gate(8, 1, 64, 1);
        let _a = g.acquire("p", "c", None).await.unwrap();
        assert!(g.acquire("p", "c", None).await.is_err());
    }
}
