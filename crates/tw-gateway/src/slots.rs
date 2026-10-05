//! 每家上游自己的并发上限（`providers[].max_concurrent`）。
//!
//! 有的中转站、有的账号同时只接几个请求，多出来的那个到了上游直接被拒 —— 回一个 429，
//! 或者一句说不清的错误。上限记在网关这一侧，**一个请求在发出去之前就知道这家满了**，
//! 由管线决定等还是换（见 `server::pipeline::hop`）：
//!
//! - 这段对话留在这家是为了它的缓存（[`crate::affinity`]）：等它空出一个位置，等不到
//!   再换下一家，缓存就丢在这家了；
//! - 别的候选满着：当场跳过，试下一家；
//! - 候选都满着：在它们里面等先空出来的那一家，等不到就回 429。
//!
//! 等多久是 `failover.slot_wait_secs`，**一个请求合起来算**：等的时候客户端一个字节都
//! 收不到。**等不是失败**：满着的上游不停用、不进熔断的账。
//!
//! 一个位置从发出请求占到回答交完、或者客户端走掉，由 [`Slot`] 的 Drop 还回去。和每把
//! 密钥的闸（[`crate::limits`]）一样，**跨重载存活**，上限改了在原来那个信号量上加减：
//! 换一个新的，在跑的请求占着的位置就不算数了。上限去掉了的，等着的请求当场放行。
//!
//! **不占位置的两种**：数 token 的请求，它不跑模型、一眨眼就回来；WebSocket 的连接（见
//! [`crate::ws`]），一条连接跑好几轮、中间可以闲着很久，占着的话一条没在答话的连接就能
//! 把这家堵死。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};
use tokio::time::Instant;

#[derive(Default)]
pub struct Slots {
    per_upstream: Mutex<HashMap<String, Arc<Pool>>>,
}

/// 一家上游的位置。
struct Pool {
    sem: Arc<Semaphore>,
    books: Mutex<Books>,
}

struct Books {
    limit: usize,
    /// 调小上限时还没收回的位置。此刻空着的当场收，在跑的请求占着的那些等它们结束时收
    /// （见 [`Slot`] 的 Drop）
    owed: usize,
}

/// 占着的一个位置。**Drop 时自动归还** —— 手工释放迟早会在某条错误路径上漏掉，漏掉的
/// 表现是这家的位置只减不增，最后发给它的请求全都在等。没设上限的上游给的是空的一个。
pub struct Slot {
    held: Option<(Arc<Pool>, OwnedSemaphorePermit)>,
}

impl Slot {
    /// 这家不限并发：什么都不占
    pub fn free() -> Self {
        Self { held: None }
    }

    fn of(pool: Arc<Pool>, permit: OwnedSemaphorePermit) -> Self {
        Self {
            held: Some((pool, permit)),
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let Some((pool, permit)) = self.held.take() else {
            return;
        };
        let mut books = pool.books.lock().unwrap_or_else(|p| p.into_inner());
        if books.owed > 0 {
            books.owed -= 1;
            permit.forget();
        }
    }
}

impl Slots {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Pool>>> {
        self.per_upstream.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn pool(&self, name: &str) -> Option<Arc<Pool>> {
        self.lock().get(name).cloned()
    }

    /// 照这份配置定每家的上限。启动时和每次重载时调。
    ///
    /// **改了上限的在原地加减**：调大的，等着的请求马上进；调小的，在跑的照样跑完，新的
    /// 要等它们降到新上限之下。去掉了上限的（或者这家上游没了）关掉它的信号量：等着的
    /// 请求当场放行，在跑的那些占着的位置随它们结束作废。
    pub fn configure(&self, providers: &[tw_config::Provider]) {
        let limit_of = |name: &str| {
            providers
                .iter()
                .find(|p| p.name == name)
                .and_then(|p| p.max_concurrent)
        };
        let mut per = self.lock();
        per.retain(|name, pool| {
            let keep = limit_of(name).is_some();
            if !keep {
                pool.sem.close();
            }
            keep
        });
        for p in providers {
            let Some(limit) = p.max_concurrent.map(|n| n as usize) else {
                continue;
            };
            match per.get(&p.name) {
                Some(pool) => pool.resize(limit),
                None => {
                    per.insert(
                        p.name.clone(),
                        Arc::new(Pool {
                            sem: Arc::new(Semaphore::new(limit)),
                            books: Mutex::new(Books { limit, owed: 0 }),
                        }),
                    );
                }
            }
        }
    }

    /// 这家此刻的上限。不限是 `None`
    pub fn limit(&self, name: &str) -> Option<usize> {
        let pool = self.pool(name)?;
        let books = pool.books.lock().unwrap_or_else(|p| p.into_inner());
        Some(books.limit)
    }

    /// 不等：有空位就占一个，这家满着是 `None`。不限并发的上游一律给一个空的。
    pub fn try_take(&self, name: &str) -> Option<Slot> {
        let Some(pool) = self.pool(name) else {
            return Some(Slot::free());
        };
        match pool.sem.clone().try_acquire_owned() {
            Ok(permit) => Some(Slot::of(pool, permit)),
            // 上限刚去掉
            Err(TryAcquireError::Closed) => Some(Slot::free()),
            Err(TryAcquireError::NoPermits) => None,
        }
    }

    /// 等这一家空出一个位置，最多等到 `until`。到点还满着是 `None`。
    pub async fn take_by(&self, name: &str, until: Instant) -> Option<Slot> {
        self.first_free(&[name], until).await.map(|(_, slot)| slot)
    }

    /// 几家里先空出来的那一家：它在 `names` 里的位置和占到的位置。**同时空出来的按
    /// `names` 的顺序**，也就是候选的顺序。最多等到 `until`，到点都还满着是 `None`；
    /// `until` 已经过了的话只看一眼。
    ///
    /// 没等到的那几家不留下什么：等着的那一份被丢掉时，tokio 把已经分给它的位置还回去。
    pub async fn first_free<S: AsRef<str>>(
        &self,
        names: &[S],
        until: Instant,
    ) -> Option<(usize, Slot)> {
        for (i, name) in names.iter().enumerate() {
            if let Some(slot) = self.try_take(name.as_ref()) {
                return Some((i, slot));
            }
        }
        if Instant::now() >= until {
            return None;
        }
        let waits: Vec<_> = names
            .iter()
            .enumerate()
            .filter_map(|(i, name)| {
                let pool = self.pool(name.as_ref())?;
                Some(Box::pin(async move {
                    match pool.sem.clone().acquire_owned().await {
                        Ok(permit) => (i, Slot::of(pool, permit)),
                        // 等的时候上限去掉了：不用再等
                        Err(_) => (i, Slot::free()),
                    }
                }))
            })
            .collect();
        if waits.is_empty() {
            // 看完那一眼之后上限都去掉了
            return Some((0, Slot::free()));
        }
        tokio::time::timeout_at(until, futures::future::select_all(waits))
            .await
            .ok()
            .map(|(got, _, _)| got)
    }
}

impl Pool {
    /// 上限改了：**在原来那个信号量上加减，不换新的。**换新的话，在跑的请求占着的位置
    /// 还算在旧的那个上，新旧各放一份，那一阵发给这家的可以到两个上限之和。
    fn resize(&self, limit: usize) {
        let mut books = self.books.lock().unwrap_or_else(|p| p.into_inner());
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

    fn limited(name: &str, n: Option<u32>) -> tw_config::Provider {
        tw_config::Provider {
            name: name.into(),
            base_url: "https://x".into(),
            max_concurrent: n,
            ..Default::default()
        }
    }

    fn slots(cfg: &[(&str, Option<u32>)]) -> Arc<Slots> {
        let s = Arc::new(Slots::default());
        s.configure(&cfg.iter().map(|(n, l)| limited(n, *l)).collect::<Vec<_>>());
        s
    }

    fn after(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    /// 让等着的请求有机会走一步
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    #[test]
    fn an_upstream_without_a_limit_never_fills() {
        let s = slots(&[("甲", None)]);
        let held: Vec<_> = (0..100).map(|_| s.try_take("甲").unwrap()).collect();
        assert_eq!(held.len(), 100);
        // 配置里没有的也一样：不知道它的上限，就是不限
        assert!(s.try_take("不认识").is_some());
    }

    #[test]
    fn a_full_upstream_says_so_at_once_and_a_dropped_slot_is_returned() {
        let s = slots(&[("甲", Some(2)), ("乙", Some(1))]);
        let a = s.try_take("甲").unwrap();
        let _b = s.try_take("甲").unwrap();
        assert!(s.try_take("甲").is_none(), "上限 2，第三个该被告知满了");
        // 一家满着不挡另一家
        assert!(s.try_take("乙").is_some());
        drop(a);
        assert!(s.try_take("甲").is_some(), "还回去的位置没有回来");
    }

    #[tokio::test]
    async fn a_wait_ends_with_the_slot_or_at_the_deadline() {
        let s = slots(&[("甲", Some(1))]);
        let first = s.try_take("甲").unwrap();
        let t = Instant::now();
        assert!(s.take_by("甲", after(80)).await.is_none());
        assert!(t.elapsed() >= Duration::from_millis(80), "没等到点就放弃了");

        let s2 = s.clone();
        let waiter = tokio::spawn(async move { s2.take_by("甲", after(5_000)).await });
        settle().await;
        assert!(!waiter.is_finished());
        drop(first);
        let got = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("空出来了却没轮到它")
            .unwrap();
        assert!(got.is_some());
    }

    #[tokio::test]
    async fn among_several_the_first_to_free_wins_and_ties_go_by_order() {
        let s = slots(&[("甲", Some(1)), ("乙", Some(1)), ("丙", Some(1))]);
        let (a, b, c) = (
            s.try_take("甲").unwrap(),
            s.try_take("乙").unwrap(),
            s.try_take("丙").unwrap(),
        );
        let s2 = s.clone();
        let waiter = tokio::spawn(async move {
            s2.first_free(&["甲", "乙", "丙"], after(5_000))
                .await
                .map(|(i, _)| i)
        });
        settle().await;
        drop(c);
        assert_eq!(waiter.await.unwrap(), Some(2), "丙先空出来");
        // 都空着的时候按顺序：头一个
        drop((a, b));
        let (i, _) = s.first_free(&["乙", "甲"], after(0)).await.unwrap();
        assert_eq!(i, 0);
        // 等不到的那几家没有被占走位置
        assert!(s.try_take("甲").is_some());
        assert!(s.try_take("乙").is_some());
    }

    #[tokio::test]
    async fn a_deadline_already_past_only_takes_a_look() {
        let s = slots(&[("甲", Some(1))]);
        let _held = s.try_take("甲").unwrap();
        let t = Instant::now();
        assert!(s.first_free(&["甲"], Instant::now()).await.is_none());
        assert!(t.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn a_waiter_dropped_just_as_it_is_granted_gives_the_slot_back() {
        // 客户端在轮到它的那一刻走了：那个位置不能跟着丢
        let s = slots(&[("甲", Some(1))]);
        let first = s.try_take("甲").unwrap();
        let s2 = s.clone();
        let waiter = tokio::spawn(async move { s2.take_by("甲", after(5_000)).await });
        settle().await;
        drop(first);
        waiter.abort();
        let _ = waiter.await;
        settle().await;
        assert!(s.try_take("甲").is_some(), "位置跟着走掉的请求丢了");
    }

    #[tokio::test]
    async fn raising_a_limit_lets_a_waiter_in_at_once() {
        let s = slots(&[("甲", Some(1))]);
        let _a = s.try_take("甲").unwrap();
        let s2 = s.clone();
        let waiter = tokio::spawn(async move { s2.take_by("甲", after(5_000)).await });
        settle().await;
        assert!(!waiter.is_finished());
        s.configure(&[limited("甲", Some(2))]);
        let got = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("调大之后等着的还在等")
            .unwrap();
        assert!(got.is_some());
    }

    #[tokio::test]
    async fn lowering_a_limit_counts_the_requests_already_running() {
        // 从 2 调到 1：在跑的两个都结束之前，新来的不能进
        let s = slots(&[("甲", Some(2))]);
        let a = s.try_take("甲").unwrap();
        let b = s.try_take("甲").unwrap();
        s.configure(&[limited("甲", Some(1))]);
        drop(a);
        assert!(s.try_take("甲").is_none(), "还有一个在跑，已经到新上限了");
        drop(b);
        let c = s.try_take("甲");
        assert!(c.is_some());
        assert!(s.try_take("甲").is_none(), "新上限是 1");
        // 调回来：欠着的先抵掉，上限回到 2
        s.configure(&[limited("甲", Some(2))]);
        let d = s.try_take("甲");
        assert!(d.is_some());
        assert!(s.try_take("甲").is_none(), "上限 2，两个都占着");
    }

    #[tokio::test]
    async fn removing_a_limit_lets_every_waiter_go_and_nothing_breaks() {
        let s = slots(&[("甲", Some(1))]);
        let running = s.try_take("甲").unwrap();
        let s2 = s.clone();
        let waiter = tokio::spawn(async move { s2.take_by("甲", after(5_000)).await });
        settle().await;
        // 上限去掉（上游删了也一样）
        s.configure(&[limited("甲", None)]);
        let got = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("上限去掉了还在等")
            .unwrap();
        assert!(got.is_some());
        assert!(s.try_take("甲").is_some(), "不限了");
        // 在跑的那个结束：它的位置属于一个已经关掉的信号量，还回去不出事
        drop(running);
        // 再设回上限：重新数
        s.configure(&[limited("甲", Some(1))]);
        let _one = s.try_take("甲").unwrap();
        assert!(s.try_take("甲").is_none());
        s.configure(&[]);
        assert!(s.try_take("甲").is_some(), "上游没了，不再限它");
    }
}
