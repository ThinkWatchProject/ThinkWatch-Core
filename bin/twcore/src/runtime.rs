//! 网关的运行时按机器自动定规模；任务由 Tokio 调度，用户无需写 runtime 配置。

use anyhow::{Context, Result};

/// Linux 的 THP 会把小对象所在的稀疏堆按 2 MiB 驻留，抵消线程回收的收益。
/// 只约束本进程，不改系统设置，也不要求用户提供分配器环境变量。
pub fn init_allocator() {
    #[cfg(all(target_os = "linux", feature = "mimalloc"))]
    {
        // SAFETY: PR_SET_THP_DISABLE 只接受整数参数，不读写 Rust 的内存。
        // 必须在创建工作线程之前调用，使其继承该进程的策略。
        let result = unsafe {
            libc::prctl(
                libc::PR_SET_THP_DISABLE,
                1 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            )
        };
        if result != 0 {
            tracing::debug!(error = %std::io::Error::last_os_error(), "could not disable transparent huge pages");
        }
    }
}

pub fn build() -> Result<tokio::runtime::Runtime> {
    let policy = tw_gateway::resources::policy();
    // 保留 Tokio 原有的显式覆盖，但自动值本身就适用于大核服务器。
    let workers = workers(
        policy.workers,
        std::env::var("TOKIO_WORKER_THREADS").ok().as_deref(),
    )?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .thread_keep_alive(tw_gateway::resources::CPU_KEEP_ALIVE)
        .enable_all()
        .build()?;
    tracing::info!(
        workers,
        heavy_jobs = policy.heavy_jobs,
        cpu_idle_seconds = tw_gateway::resources::CPU_KEEP_ALIVE.as_secs(),
        body_budget_mib = policy.body_budget / (1024 * 1024),
        allocator = if cfg!(feature = "mimalloc") {
            "mimalloc"
        } else {
            "system"
        },
        "automatically sized gateway runtime"
    );
    Ok(rt)
}

fn workers(automatic: usize, explicit: Option<&str>) -> Result<usize> {
    match explicit {
        None => Ok(automatic),
        Some(value) => value
            .parse::<std::num::NonZeroUsize>()
            .map(usize::from)
            .with_context(|| {
                format!("TOKIO_WORKER_THREADS must be a positive integer, got `{value}`")
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(target_os = "linux", feature = "mimalloc"))]
    #[test]
    fn allocator_disables_transparent_huge_pages_for_this_process() {
        init_allocator();
        // SAFETY: PR_GET_THP_DISABLE returns the process flag and takes integer arguments.
        let disabled = unsafe {
            libc::prctl(
                libc::PR_GET_THP_DISABLE,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            )
        };
        assert_eq!(disabled, 1);
    }

    #[test]
    fn explicit_tokio_override_is_optional_and_validated() {
        assert_eq!(workers(8, None).unwrap(), 8);
        assert_eq!(workers(8, Some("16")).unwrap(), 16);
        for bad in ["0", "", "-1", "eight"] {
            assert!(workers(8, Some(bad)).is_err());
        }
    }
}
