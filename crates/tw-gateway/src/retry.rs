//! 最后一家暂时出错时退避重试。
//!
//! 候选都试过了，最后那一跳在**第一段内容之前**出了暂时的错 —— 过载（503、529、流里的
//! `overloaded_error`、`server_is_overloaded`）、别的 5xx、连不上、流在内容之前断了。这时客户端
//! 什么都还没收到，等一会儿再发一遍它察觉不到；只有一家上游的请求更是这样，没有下一家可换，
//! 上游过载几秒钟的事，不该把错误交给客户端。
//!
//! 几条规矩（做法见 `server::pipeline::hop`）：
//!
//! - **只重试暂时的错**（[`transient`]），也就是说不出原因的失败。限流有自己的队（见
//!   [`crate::throttle`]）；额度用完、没钱了、凭据被拒、没有这个模型、请求本身的问题，再发一遍
//!   也一样；没有内容超时的已经等过一整个无响应超时，不再等。
//! - **在这个请求的等待期限之内**（`failover.wait_secs`，和排队共用一段）：剩下的时间不够下一次
//!   退避就不等了，交出最后那个错误。`0` 是不重试。已经发出去的一跳不受期限限制。
//! - **把暂时出错的那几家再试一轮**，按候选的顺序，在冷却的跳过（都在冷却的照样试，和开头的
//!   熔断过滤一样）；留在一家的对话（会话粘性、封存的历史）只再试那一家。每一跳和故障转移的
//!   一跳一样：插件、内容过滤、脱敏、参数改写都照常跑，熔断的账照常记。
//! - **退避 2、4、8 秒，之后每次 16 秒**，上下浮动两成，免得一起失败的请求一起再来；上游说了
//!   多久之后再来的（`retry-after-ms`、`Retry-After`）按它说的。

use std::time::Duration;

use crate::failure::Cause;

/// 第 1、2、3 次重试之前退避几秒，之后每次都是最后那个数
const STEPS: [u32; 4] = [2, 4, 8, 16];

/// 退避多久。
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    /// 退避的一秒有多长。**测试会把它调短**
    pub tick: Duration,
    /// 上下浮动多少（`0.2` 是两成）。**测试设成 0**：退避多久是定的
    pub jitter: f64,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            tick: Duration::from_secs(1),
            jitter: 0.2,
        }
    }
}

impl Backoff {
    /// 第 `n` 次重试（从 1 数）之前退避多久
    pub fn before(&self, n: u32) -> Duration {
        let i = (n.max(1) - 1).min(STEPS.len() as u32 - 1) as usize;
        let base = self.tick.saturating_mul(STEPS[i]);
        let jitter = self.jitter.clamp(0.0, 0.5);
        if jitter == 0.0 {
            return base;
        }
        base.mul_f64(1.0 + rand::random_range(-jitter..=jitter))
    }
}

/// 这个原因是不是暂时的错：说不出原因的失败（5xx、过载）。别的见模块说明
pub fn transient(cause: Cause) -> bool {
    cause == Cause::Unexplained
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_doubles_up_to_sixteen_seconds() {
        let b = Backoff {
            tick: Duration::from_secs(1),
            jitter: 0.0,
        };
        let secs: Vec<u64> = (1..=6).map(|n| b.before(n).as_secs()).collect();
        assert_eq!(secs, [2, 4, 8, 16, 16, 16]);
    }

    #[test]
    fn the_jitter_stays_within_a_fifth() {
        let b = Backoff::default();
        for _ in 0..200 {
            let d = b.before(1);
            assert!(
                (1600..=2400).contains(&d.as_millis()),
                "{d:?} 不在 2 秒上下两成之内"
            );
        }
    }

    #[test]
    fn only_unexplained_failures_are_retried() {
        assert!(transient(Cause::Unexplained));
        for c in [
            Cause::NoBalance,
            Cause::QuotaUsedUp { resets_at_ms: None },
            Cause::RateLimited { retry_after: None },
            Cause::ModelUnavailable,
            Cause::AuthRejected,
        ] {
            assert!(!transient(c), "{c:?}");
        }
    }
}
