//! 进程级的自动资源调度。配置重载、多个运行时共用同一份预算。
//!
//! CPU 重任务先异步等名额，再交给阻塞线程；等候者不会占满 Tokio 的线程。
//! 正文按字节占预算，并为解析、复制留三倍余量。预算不足时先退避重试，避免已经
//! 持有一半正文的请求互相等内存。名额跟着 Bytes 的最后一份引用归还。
//!
//! 单个合法的大正文可以独占预算：自动预算不改变 256 MiB 的单请求上限。这是
//! 正文缓冲的预算，不是对整个进程 RSS 的硬限制（插件、JSON 对象等另有开销）。

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[cfg(any(target_os = "linux", test))]
mod cgroup;

const MIB: usize = 1024 * 1024;
const BODY_HEADROOM: usize = 3;
const HEAVY_BODY: usize = 1024 * 1024;

/// 突发负载过去后回收阻塞线程；不改变常驻 I/O worker。
pub const CPU_KEEP_ALIVE: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub struct Policy {
    pub workers: usize,
    pub heavy_jobs: usize,
    pub body_budget: usize,
}

impl Policy {
    fn for_machine(parallelism: usize, memory: u64) -> Self {
        let workers = parallelism.clamp(1, 8);
        let memory = usize::try_from(memory).unwrap_or(usize::MAX);
        Self {
            workers,
            heavy_jobs: parallelism.max(1).min((memory / (128 * MIB)).max(1)),
            body_budget: (memory / 8).clamp(64 * MIB, 512 * MIB),
        }
    }
}

pub fn policy() -> &'static Policy {
    static POLICY: OnceLock<Policy> = OnceLock::new();
    POLICY.get_or_init(|| {
        let parallelism = std::thread::available_parallelism().map_or(1, usize::from);
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        // Linux 容器要看 cgroup 限额，不能按宿主机的几十 GB 分预算。
        let cgroup_limit = system.cgroup_limits().map(|c| c.total_memory);
        #[cfg(target_os = "linux")]
        let cgroup_limit = cgroup::memory_limit().or(cgroup_limit);
        let memory = effective_memory(system.total_memory(), cgroup_limit);
        Policy::for_machine(parallelism, memory)
    })
}

fn effective_memory(physical: u64, cgroup: Option<u64>) -> u64 {
    let physical = if physical == 0 {
        1024 * MIB as u64
    } else {
        physical
    };
    cgroup
        .filter(|&n| n > 0)
        .map_or(physical, |n| n.min(physical))
}

/// 小任务允许占用一半 I/O worker；并发超过它时同样交还调度权，避免小正文流量
/// 永远只能用常驻的八根线程。CPU 名额同时约束内联、阻塞和插件任务。
struct Scheduler {
    slots: Arc<Semaphore>,
    inline: Semaphore,
}

impl Scheduler {
    fn new(policy: &Policy) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(policy.heavy_jobs)),
            inline: Semaphore::new(policy.workers.div_ceil(2)),
        }
    }

    async fn slot(&self) -> OwnedSemaphorePermit {
        self.slots
            .clone()
            .acquire_owned()
            .await
            .expect("the process-wide heavy-work semaphore is never closed")
    }

    async fn run<T>(&self, size: usize, f: impl FnOnce() -> T) -> T {
        use tokio::runtime::{Handle, RuntimeFlavor};

        let _slot = self.slot().await;
        let inline = (size < HEAVY_BODY)
            .then(|| self.inline.try_acquire().ok())
            .flatten();
        let multi =
            Handle::try_current().is_ok_and(|h| h.runtime_flavor() == RuntimeFlavor::MultiThread);
        if inline.is_some() || !multi {
            f()
        } else {
            // 原线程执行借用栈上的闭包，Tokio 按需起替补线程继续处理 I/O。
            // 空闲线程按 runtime 的 keep-alive 回收；I/O worker 数不是 CPU 并发上限。
            tokio::task::block_in_place(f)
        }
    }
}

fn scheduler() -> &'static Scheduler {
    static SCHEDULER: OnceLock<Scheduler> = OnceLock::new();
    SCHEDULER.get_or_init(|| Scheduler::new(policy()))
}

pub(crate) async fn heavy_slot() -> OwnedSemaphorePermit {
    scheduler().slot().await
}

pub(crate) async fn heavy<T>(size: usize, f: impl FnOnce() -> T) -> T {
    scheduler().run(size, f).await
}

pub(crate) fn body_budget() -> &'static Arc<Budget> {
    static BUDGET: OnceLock<Arc<Budget>> = OnceLock::new();
    BUDGET.get_or_init(|| Budget::new(policy().body_budget))
}

pub(crate) struct Budget {
    limit: usize,
    used: Mutex<usize>,
}

impl Budget {
    pub(crate) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: Mutex::new(0),
        })
    }

    pub(crate) fn reserve(self: &Arc<Self>) -> Reservation {
        Reservation {
            budget: self.clone(),
            charged: 0,
        }
    }
}

pub(crate) struct Reservation {
    budget: Arc<Budget>,
    charged: usize,
}

impl Reservation {
    /// 在追加正文或解压输出之前调用。失败不会改变已经占用的预算。
    pub(crate) fn grow(&mut self, bytes: usize) -> bool {
        let charge = bytes.saturating_mul(BODY_HEADROOM).min(self.budget.limit);
        let extra = charge.saturating_sub(self.charged);
        let mut used = self.budget.used.lock().unwrap_or_else(|e| e.into_inner());
        if extra > self.budget.limit - *used {
            return false;
        }
        *used += extra;
        self.charged += extra;
        true
    }

    pub(crate) fn wrap(self, data: Bytes) -> Bytes {
        Bytes::from_owner(Buffered {
            data,
            _reservation: self,
        })
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut used = self.budget.used.lock().unwrap_or_else(|e| e.into_inner());
        *used -= self.charged;
    }
}

struct Buffered {
    data: Bytes,
    _reservation: Reservation,
}

impl AsRef<[u8]> for Buffered {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

pub(crate) fn memory_busy() -> crate::GatewayError {
    crate::GatewayError::busy(tw_types::msg!(
        "gw.request.memory_busy" =>
        "The gateway is processing large requests. Please retry shortly."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_policy_handles_small_machines_and_many_cores() {
        let small = Policy::for_machine(1, 256 * MIB as u64);
        assert_eq!(
            (small.workers, small.heavy_jobs, small.body_budget),
            (1, 1, 64 * MIB)
        );
        let large = Policy::for_machine(80, 128 * 1024 * MIB as u64);
        assert_eq!(
            (large.workers, large.heavy_jobs, large.body_budget),
            (8, 80, 512 * MIB)
        );
        assert_eq!(Policy::for_machine(80, 256 * MIB as u64).heavy_jobs, 2);
    }

    #[test]
    fn eighty_cpu_jobs_scale_past_two_io_workers_and_retire_when_idle() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Instant;

        // 模拟 80 核策略，验证真实线程并发和回收；不要求测试机有 80 个物理核。
        let live = Arc::new(AtomicUsize::new(0));
        let (started, stopped) = (live.clone(), live.clone());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_keep_alive(Duration::from_millis(100))
            .on_thread_start(move || {
                started.fetch_add(1, Ordering::SeqCst);
            })
            .on_thread_stop(move || {
                stopped.fetch_sub(1, Ordering::SeqCst);
            })
            .enable_all()
            .build()
            .unwrap();
        let scheduler = Arc::new(Scheduler::new(&Policy {
            workers: 2,
            ..Policy::for_machine(80, 128 << 30)
        }));
        // 大正文必定交出调度权；小正文在并发扫描时也应扩展。
        for size in [HEAVY_BODY, 32 * 1024] {
            rt.block_on(async {
                let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
                let (began, mut starts) = tokio::sync::mpsc::unbounded_channel();
                let mut jobs = Vec::new();
                for _ in 0..80 {
                    let (scheduler, release, began) =
                        (scheduler.clone(), release.clone(), began.clone());
                    jobs.push(tokio::spawn(async move {
                        scheduler
                            .run(size, || {
                                began.send(()).unwrap();
                                let guard = release.0.lock().unwrap();
                                let _guard = release
                                    .1
                                    .wait_timeout_while(guard, Duration::from_secs(10), |done| {
                                        !*done
                                    })
                                    .unwrap();
                            })
                            .await;
                    }));
                }
                for _ in 0..80 {
                    tokio::time::timeout(Duration::from_secs(5), starts.recv())
                        .await
                        .expect("all 80 jobs must run concurrently")
                        .unwrap();
                }
                assert_eq!(scheduler.slots.available_permits(), 0);
                assert!(live.load(Ordering::SeqCst) >= 80);
                // 已有 80 个 CPU 调用时，I/O 的计时器还能正常推进。
                tokio::time::sleep(Duration::from_millis(10)).await;
                *release.0.lock().unwrap() = true;
                release.1.notify_all();
                for job in jobs {
                    job.await.unwrap();
                }
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while live.load(Ordering::SeqCst) != 2 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(
                live.load(Ordering::SeqCst),
                2,
                "only I/O workers stay alive"
            );
        }
    }

    #[test]
    fn container_memory_caps_host_memory_and_unknown_values_have_a_fallback() {
        assert_eq!(effective_memory(64 << 30, Some(512 << 20)), 512 << 20);
        assert_eq!(effective_memory(1 << 30, Some(u64::MAX)), 1 << 30);
        assert_eq!(effective_memory(0, Some(0)), 1 << 30);
    }

    #[test]
    fn reservations_survive_bytes_clones_and_slices() {
        let budget = Budget::new(30);
        let mut reservation = budget.reserve();
        assert!(reservation.grow(5));
        let data = reservation.wrap(Bytes::from_static(b"hello"));
        let clone = data.clone();
        let slice = clone.slice(1..3);
        drop(data);
        drop(clone);
        assert_eq!(*budget.used.lock().unwrap(), 15);
        assert_eq!(&slice[..], b"el");
        drop(slice);
        assert_eq!(*budget.used.lock().unwrap(), 0);
    }

    #[test]
    fn failed_growth_releases_only_its_own_reservation() {
        let budget = Budget::new(30);
        let mut first = budget.reserve();
        let mut second = budget.reserve();
        assert!(first.grow(6));
        assert!(second.grow(4));
        assert!(!first.grow(7));
        drop(second);
        assert!(first.grow(7));
        drop(first);
        assert_eq!(*budget.used.lock().unwrap(), 0);
    }

    #[test]
    fn a_large_legal_body_can_run_exclusively_without_deadlocking() {
        let budget = Budget::new(30);
        let mut large = budget.reserve();
        assert!(large.grow(100));
        let mut small = budget.reserve();
        assert!(!small.grow(1));
        drop(large);
        assert!(small.grow(1));
    }
}
