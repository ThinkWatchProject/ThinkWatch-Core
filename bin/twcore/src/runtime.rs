//! 网关的运行时按机器自动定规模；任务由 Tokio 调度，用户无需写 runtime 配置。

use anyhow::{Context, Result};

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

    #[test]
    fn explicit_tokio_override_is_optional_and_validated() {
        assert_eq!(workers(8, None).unwrap(), 8);
        assert_eq!(workers(8, Some("16")).unwrap(), 16);
        for bad in ["0", "", "-1", "eight"] {
            assert!(workers(8, Some(bad)).is_err());
        }
    }
}
