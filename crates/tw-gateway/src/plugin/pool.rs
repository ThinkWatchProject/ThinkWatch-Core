//! 跑插件的专用线程池，和回答实例的名额。
//!
//! 插件调用是阻塞的、吃 CPU 的（一次请求钩子最多跑两百毫秒）。放在 tokio 的工作线程
//! 上调，几个慢插件就能把整个数据面的线程占满 —— 那时连不走插件的请求也一起卡住。
//! 所以一律交给这里：几根自己的线程，**排队的数量有上限**，满了的话调用方异步地等，
//! 不占着 tokio 的线程。
//!
//! 线程**第一次用到时才起**：绝大多数用户一个插件都没装，不该为它多几根闲着的线程。
//!
//! # 回答实例的名额
//!
//! 回答钩子的实例从回答开始活到回答结束（不变式 I3），一个最多占
//! `tw_plugin::Limits::reply_memory`（64 MiB）。流式的回答一开就是几分钟，同时在流的
//! 回答越多，活着的实例就越多 —— 不设上限的话，内存跟着并发的流一起涨。所以整个进程
//! 同时活着的回答实例最多 [`MAX_LIVE_REPLIES`] 个：起实例之前先拿一个名额
//! （[`Pool::reply_slot`]）。**拿不到不等**：回答已经到了，等一个名额就是让客户端干等，
//! 满了就按那个插件的 `on_error` 处置 —— 拒绝是这个请求失败，跳过是这次回答绕过它。
//! 名额跟着实例走（[`Slot`]），实例扔掉的那一刻还回来。

use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};

/// 同时活着的回答实例最多这么多个（见模块说明）。按一个实例 64 MiB 的上限算，最坏
/// 2 GiB；实际的插件一个实例多半不到 1 MiB
pub const MAX_LIVE_REPLIES: usize = 32;

type Job = Box<dyn FnOnce() + Send>;

/// 池子坏了：任务 panic 了，或者线程起不来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolError {
    Panicked,
    Unavailable(String),
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PoolError::Panicked => f.write_str("the plugin call panicked"),
            PoolError::Unavailable(why) => write!(f, "no thread could run the plugin: {why}"),
        }
    }
}

pub struct Pool {
    threads: usize,
    /// 正在跑的加排着的，最多这么多
    permits: Arc<tokio::sync::Semaphore>,
    /// 回答实例的名额（见模块说明）
    replies: Arc<tokio::sync::Semaphore>,
    /// 名额一共几个
    reply_cap: usize,
    tx: OnceLock<Result<Mutex<mpsc::Sender<Job>>, String>>,
}

/// 一个回答实例占着的名额（见 [`Pool::reply_slot`]）。**和实例放在一起**：实例在哪儿被
/// 扔掉 —— 回答收尾、插件出错被拿掉、客户端走了、插件线程上 panic —— 名额就在哪儿还回去
#[derive(Debug)]
pub struct Slot {
    _permit: tokio::sync::OwnedSemaphorePermit,
}

/// 一根线程的栈。**跑沙箱的线程至少要 2 MiB**（wasm 自己最多用 1 MiB，外面还有宿主
/// 那一侧的调用），这里明着给足，不靠平台的默认值（Windows 上只有 1 MiB）
pub(crate) const STACK: usize = 8 * 1024 * 1024;

impl Pool {
    /// `threads` 根线程，最多 `queue` 个任务在跑或者排着；回答实例的名额是
    /// [`MAX_LIVE_REPLIES`] 个。
    pub fn new(threads: usize, queue: usize) -> Self {
        Self::with_replies(threads, queue, MAX_LIVE_REPLIES)
    }

    /// 同 [`Pool::new`]，回答实例的名额是 `replies` 个。**测试用**：拿一个小数，不用开几十
    /// 条流就能把名额占满
    pub fn with_replies(threads: usize, queue: usize, replies: usize) -> Self {
        let threads = threads.max(1);
        Self {
            threads,
            permits: Arc::new(tokio::sync::Semaphore::new(queue.max(threads))),
            replies: Arc::new(tokio::sync::Semaphore::new(replies)),
            reply_cap: replies,
            tx: OnceLock::new(),
        }
    }

    /// 给一个回答实例拿一个名额。**满了是 `None`，不等**（理由见模块说明）
    pub fn reply_slot(&self) -> Option<Slot> {
        self.replies
            .clone()
            .try_acquire_owned()
            .ok()
            .map(|p| Slot { _permit: p })
    }

    /// 回答实例的名额一共几个
    pub fn reply_cap(&self) -> usize {
        self.reply_cap
    }

    /// 此刻活着的回答实例（占着的名额）
    pub fn live_replies(&self) -> usize {
        self.reply_cap
            .saturating_sub(self.replies.available_permits())
    }

    /// 按机器的核数定：至少两根，最多八根；排队的是线程数的四倍。
    pub fn default_size() -> Self {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        let threads = cores.clamp(2, 8);
        Self::new(threads, threads * 4)
    }

    fn sender(&self) -> Result<&Mutex<mpsc::Sender<Job>>, PoolError> {
        self.tx
            .get_or_init(|| {
                let (tx, rx) = mpsc::channel::<Job>();
                let rx = Arc::new(Mutex::new(rx));
                for i in 0..self.threads {
                    let rx = rx.clone();
                    std::thread::Builder::new()
                        .name(format!("tw-plugin-{i}"))
                        .stack_size(STACK)
                        .spawn(move || {
                            loop {
                                // 锁只在取任务时拿着，跑任务时放开
                                let job = match rx.lock() {
                                    Ok(r) => r.recv(),
                                    Err(_) => return,
                                };
                                match job {
                                    Ok(job) => job(),
                                    // 池子被丢掉了
                                    Err(_) => return,
                                }
                            }
                        })
                        .map_err(|e| e.to_string())?;
                }
                Ok(Mutex::new(tx))
            })
            .as_ref()
            .map_err(|e| PoolError::Unavailable(e.clone()))
    }

    /// 在池子里跑 `f`，等它的结果。**调用方的 future 被丢掉时任务照样跑完**，结果没人要
    /// 而已 —— 插件调用自己有 CPU 上限，不会一直占着线程。
    pub async fn run<T, F>(&self, f: F) -> Result<T, PoolError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| PoolError::Unavailable(e.to_string()))?;
        let (done, wait) = tokio::sync::oneshot::channel();
        let job: Job = Box::new(move || {
            let _permit = permit;
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
                .map_err(|_| PoolError::Panicked);
            let _ = done.send(out);
        });
        self.sender()?
            .lock()
            .map_err(|e| PoolError::Unavailable(e.to_string()))?
            .send(job)
            .map_err(|e| PoolError::Unavailable(e.to_string()))?;
        wait.await
            .map_err(|_| PoolError::Unavailable("the plugin thread went away".into()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn work_runs_on_the_plugin_threads_and_comes_back() {
        let pool = Pool::new(2, 4);
        let (v, there) = pool
            .run(|| (21 * 2, std::thread::current().name().map(str::to_string)))
            .await
            .unwrap();
        assert_eq!(v, 42);
        assert!(there.unwrap().starts_with("tw-plugin-"));
    }

    #[tokio::test]
    async fn a_panicking_call_is_an_error_and_the_pool_keeps_working() {
        let pool = Pool::new(1, 1);
        let r: Result<(), _> = pool.run(|| panic!("boom")).await;
        assert_eq!(r, Err(PoolError::Panicked));
        assert_eq!(pool.run(|| 7).await, Ok(7));
    }

    #[test]
    fn reply_slots_run_out_without_waiting_and_come_back_when_dropped() {
        let pool = Pool::with_replies(1, 1, 2);
        let a = pool.reply_slot().expect("the first slot");
        let b = pool.reply_slot().expect("the second slot");
        assert_eq!(pool.live_replies(), 2);
        assert!(pool.reply_slot().is_none(), "a third slot past the cap");
        drop(a);
        assert_eq!(pool.live_replies(), 1);
        let c = pool.reply_slot().expect("the slot that came back");
        drop((b, c));
        assert_eq!(pool.live_replies(), 0);
        assert_eq!(Pool::default_size().reply_cap(), MAX_LIVE_REPLIES);
    }

    #[tokio::test]
    async fn more_calls_than_threads_wait_their_turn() {
        let pool = Arc::new(Pool::new(2, 2));
        let mut handles = Vec::new();
        for i in 0..16u64 {
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
                pool.run(move || {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    i
                })
                .await
                .unwrap()
            }));
        }
        let mut sum = 0;
        for h in handles {
            sum += h.await.unwrap();
        }
        assert_eq!(sum, (0..16).sum::<u64>());
    }
}
