//! 每把密钥自己的并发上限。
//!
//! **网关本身不设上限** —— 没有全局的、没有单个上游的，也没有队列。本机
//! 网关同时在跑的，就是这台电脑上几个客户端各自开着的会话；再压一道闸，
//! 挡住的只会是用户自己的并行任务。唯一的上限是用户给某一把密钥设的
//! `max_concurrent`：那把密钥后面若是一个失控的脚本，它占不走别人的份。
//!
//! 超出上限的请求**等前面的结束**，不拒绝：客户端收到 429 通常不会优雅
//! 重试 —— Claude Code 把它当成硬失败，一个本来只需要多等两秒的请求会变成
//! 一次任务中断。等多久由客户端决定：它断开连接，这个请求就不再等。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Default)]
pub struct Gate {
    per_key: Mutex<HashMap<String, Arc<Key>>>,
}

/// 一把密钥的闸。
struct Key {
    sem: Arc<Semaphore>,
    books: Mutex<Books>,
}

struct Books {
    limit: usize,
    /// 调小上限时还没收回的通行证数。此刻空着的当场收，在跑的请求手里
    /// 那些等它们结束时收（见 [`Pass`] 的 Drop）
    owed: usize,
}

/// 拿到的通行证。**Drop 时自动归还** —— 手工释放迟早会在某条错误路径上
/// 漏掉，而漏掉的表现是并发数只减不增，最后这把密钥的请求一起卡死。
pub struct Pass {
    held: Option<(Arc<Key>, OwnedSemaphorePermit)>,
}

impl Drop for Pass {
    fn drop(&mut self) {
        let Some((key, permit)) = self.held.take() else {
            return;
        };
        let mut books = key.books.lock().unwrap();
        if books.owed > 0 {
            books.owed -= 1;
            permit.forget();
        }
    }
}

impl Gate {
    /// 取一张通行证。`limit` 是这把密钥此刻配置里的 `max_concurrent`，
    /// 没设就不等。
    pub async fn acquire(&self, key: &str, limit: Option<usize>) -> Pass {
        let Some(limit) = limit else {
            return Pass { held: None };
        };
        let k = self
            .per_key
            .lock()
            .unwrap()
            .entry(key.to_string())
            .or_insert_with(|| {
                Arc::new(Key {
                    sem: Arc::new(Semaphore::new(limit)),
                    books: Mutex::new(Books { limit, owed: 0 }),
                })
            })
            .clone();
        k.resize(limit);
        let permit = k
            .sem
            .clone()
            .acquire_owned()
            .await
            .expect("the semaphore is never closed");
        Pass {
            held: Some((k, permit)),
        }
    }
}

impl Key {
    /// 上限改了：**在原来那个信号量上加减，不换新的。**换新的话，在跑的
    /// 请求的通行证还算在旧的那个上，新旧各放一份，那一阵的实际并发可以
    /// 到两个上限之和。
    fn resize(&self, limit: usize) {
        let mut books = self.books.lock().unwrap();
        if limit > books.limit {
            // 先抵掉还欠着的，剩下的才是真要多放的
            let more = limit - books.limit;
            let cancel = more.min(books.owed);
            books.owed -= cancel;
            self.sem.add_permits(more - cancel);
        } else if limit < books.limit {
            let less = books.limit - limit;
            let now = self.sem.forget_permits(less);
            books.owed += less - now;
        }
        books.limit = limit;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 让排着的请求有机会走一步
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    async fn soon(g: &Gate, key: &str, limit: Option<usize>) -> Pass {
        tokio::time::timeout(Duration::from_secs(1), g.acquire(key, limit))
            .await
            .expect("该马上拿到")
    }

    #[tokio::test]
    async fn without_a_limit_nothing_waits() {
        // 网关本身不设上限：几个客户端各开几个会话，本来就该同时跑
        let g = Gate::default();
        let mut held = Vec::new();
        for _ in 0..100 {
            held.push(soon(&g, "k", None).await);
        }
        assert_eq!(held.len(), 100);
    }

    #[tokio::test]
    async fn over_a_keys_limit_a_request_waits_rather_than_being_refused() {
        // **这是这个模块存在的理由。**客户端收到 429 通常不会优雅重试，
        // 一个本来只需要多等两秒的请求会变成一次任务中断。
        let g = Arc::new(Gate::default());
        let first = g.acquire("k", Some(1)).await;

        let g2 = g.clone();
        let waiter = tokio::spawn(async move { g2.acquire("k", Some(1)).await });
        settle().await;
        assert!(!waiter.is_finished(), "第二个应该在等，而不是被拒");

        drop(first);
        waiter.await.expect("前一个走了就该轮到它");
    }

    #[tokio::test(start_paused = true)]
    async fn the_wait_has_no_deadline() {
        // 等多久由客户端决定。网关自己定一个放弃的时限，是替用户做了一个
        // 用户自己没做过的决定
        let g = Arc::new(Gate::default());
        let first = g.acquire("k", Some(1)).await;
        let g2 = g.clone();
        let waiter = tokio::spawn(async move { g2.acquire("k", Some(1)).await });
        tokio::time::sleep(Duration::from_secs(3600)).await;
        assert!(!waiter.is_finished());
        drop(first);
        waiter.await.unwrap();
    }

    #[tokio::test]
    async fn a_permit_is_returned_when_it_is_dropped() {
        let g = Gate::default();
        drop(g.acquire("k", Some(1)).await);
        // 没有归还的话这里会一直等
        soon(&g, "k", Some(1)).await;
    }

    #[tokio::test]
    async fn one_keys_limit_does_not_hold_up_another_key() {
        let g = Gate::default();
        let _greedy = g.acquire("greedy", Some(1)).await;
        soon(&g, "other", Some(1)).await;
        soon(&g, "unlimited", None).await;
    }

    #[tokio::test]
    async fn raising_a_limit_lets_the_next_request_in_at_once() {
        let g = Gate::default();
        let _a = g.acquire("k", Some(1)).await;
        soon(&g, "k", Some(2)).await;
    }

    #[tokio::test]
    async fn lowering_a_limit_counts_the_requests_already_running() {
        // 从 2 调到 1：在跑的两个都结束之前，新来的不能进 —— 换一个新的
        // 信号量的话它会马上进，那一阵实际并发是 3
        let g = Arc::new(Gate::default());
        let a = g.acquire("k", Some(2)).await;
        let b = g.acquire("k", Some(2)).await;

        let g2 = g.clone();
        let waiter = tokio::spawn(async move { g2.acquire("k", Some(1)).await });
        settle().await;
        assert!(!waiter.is_finished(), "调小之后超出的那个进来了");

        drop(a);
        settle().await;
        assert!(!waiter.is_finished(), "还有一个在跑，已经到新上限了");

        drop(b);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("在跑的都结束了还没轮到它")
            .unwrap();
    }

    #[tokio::test]
    async fn lowering_then_raising_again_ends_where_it_started() {
        // 调小时欠下的，调回来时先抵掉 —— 否则来回改两次，上限就悄悄变了
        let g = Gate::default();
        let a = g.acquire("k", Some(2)).await;
        let b = g.acquire("k", Some(2)).await;
        g.per_key.lock().unwrap()["k"].resize(1);
        g.per_key.lock().unwrap()["k"].resize(2);
        drop(a);
        drop(b);
        let _c = soon(&g, "k", Some(2)).await;
        let _d = soon(&g, "k", Some(2)).await;
        let g = Arc::new(g);
        let g2 = g.clone();
        let third = tokio::spawn(async move { g2.acquire("k", Some(2)).await });
        settle().await;
        assert!(!third.is_finished(), "上限 2，第三个不该进");
        third.abort();
    }
}
