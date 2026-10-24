//! 最后一家暂时出错时退避重试。
//!
//! 候选都试过了，最后那一跳在**第一段内容之前**出了暂时的错 —— 过载（503、529、流里的
//! `overloaded_error`、`server_is_overloaded`）、别的 5xx、连不上、流在内容之前断了。这时客户端
//! 什么都还没收到，等一会儿再发一遍它察觉不到；只有一家上游的请求更是这样，没有下一家可换，
//! 上游过载几秒钟的事，不该把错误交给客户端。
//!
//! 几条规矩（做法见 `server::pipeline::hop`）：
//!
//! - **只重试暂时的错**：状态码只认 500、502、503、504、529（[`transient_status`]），流里报的错
//!   只认名单里的码（[`transient_kind`]），正文说是请求本身的问题的不算（[`request_fault`]）；
//!   连不上、流在内容之前断了也算。限流有自己的队（见 [`crate::throttle`]）；额度用完、没钱了、
//!   凭据被拒、没有这个模型、请求本身的问题，再发一遍也一样；没有内容超时的已经等过一整个
//!   无响应超时，不再等。别的 5xx（501、505、Cloudflare 的 52x……）照旧换下一家，只是不再试同一家
//! - **在这个请求的等待期限之内**（`failover.wait_secs`，和排队共用一段）：剩下的时间不够下一次
//!   退避就不等了，交出最后那个错误。`0` 是不重试。已经发出去的一跳不受期限限制。
//! - **把暂时出错的那几家再试一轮**，按候选的顺序，在冷却的跳过（都在冷却的照样试，和开头的
//!   熔断过滤一样）；留在一家的对话（会话粘性、封存的历史）只再试那一家。每一跳和故障转移的
//!   一跳一样：插件、内容过滤、脱敏、参数改写都照常跑，熔断的账照常记。
//! - **退避 2、4、8 秒，之后每次 16 秒**，上下浮动两成，免得一起失败的请求一起再来；上游说了
//!   多久之后再来的（`retry-after-ms`、`Retry-After`）比这长就按它说的，**比这短的不算**：一家
//!   回 `retry-after-ms: 5` 的中转不能让一个请求变成几千个。

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

/// 第 `n` 次重试之前退避多久：`hint` 是上游说的多久之后再来，只能比退避长，不能比它短
pub fn wait(backoff: Duration, hint: Option<Duration>) -> Duration {
    hint.map_or(backoff, |h| h.max(backoff))
}

/// 这个原因是不是暂时的错：说不出原因的失败（5xx、过载）。别的见模块说明
pub fn transient(cause: Cause) -> bool {
    cause == Cause::Unexplained
}

/// 这个状态码是不是暂时的错：服务端出错、网关出错、过载、网关超时。别的 5xx（没实现、版本
/// 不支持、Cloudflare 的 52x）再发一遍多半也一样
pub fn transient_status(status: u16) -> bool {
    matches!(status, 500 | 502 | 503 | 504 | 529)
}

/// 流里报的错，码或者类型是这几个的才是暂时的：OpenAI、Anthropic 的服务端出错和过载，Gemini
/// 的 `UNAVAILABLE`、`INTERNAL`，Bedrock 的两种服务端异常。**认不出的一律不算** —— 图片解不开、
/// 内容被拦这类错同一家再发一遍也一样
const TRANSIENT_KINDS: &[&str] = &[
    "server_error",
    "api_error",
    "overloaded_error",
    "server_is_overloaded",
    "slow_down",
    "UNAVAILABLE",
    "INTERNAL",
    "serviceUnavailableException",
    "internalServerException",
];

/// 上游在流里报的这个码（或类型）是不是暂时的错（见 [`TRANSIENT_KINDS`]）
pub fn transient_kind(kind: &str) -> bool {
    TRANSIENT_KINDS.contains(&kind)
}

/// 错误正文说的是请求本身的问题（`invalid_request_error` 这一类）：状态码是 5xx 也不重试 ——
/// 同一家再发一遍也一样
pub fn request_fault(body: &[u8]) -> bool {
    const FAULTS: &[&str] = &["invalid_request_error", "invalid_request", "bad_request"];
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    [
        "/error/type",
        "/error/code",
        "/response/error/type",
        "/response/error/code",
    ]
    .iter()
    .filter_map(|p| v.pointer(p).and_then(|t| t.as_str()))
    .any(|t| FAULTS.contains(&t))
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
    fn a_hint_lengthens_the_backoff_and_never_shortens_it() {
        let two = Duration::from_secs(2);
        assert_eq!(wait(two, None), two);
        assert_eq!(wait(two, Some(Duration::from_millis(5))), two);
        assert_eq!(
            wait(two, Some(Duration::from_secs(9))),
            Duration::from_secs(9)
        );
    }

    #[test]
    fn only_some_statuses_and_kinds_are_temporary() {
        for s in [500, 502, 503, 504, 529] {
            assert!(transient_status(s), "{s}");
        }
        for s in [501, 505, 520, 522, 524, 527, 599] {
            assert!(!transient_status(s), "{s}");
        }
        assert!(transient_kind("server_is_overloaded"));
        assert!(transient_kind("UNAVAILABLE"));
        for k in [
            "invalid_image",
            "content_filter",
            "invalid_encrypted_content",
            "",
        ] {
            assert!(!transient_kind(k), "{k}");
        }
    }

    #[test]
    fn a_request_error_in_the_body_is_not_temporary() {
        let refused = br#"{"type":"error","error":{"type":"invalid_request_error","code":"invalid_encrypted_content","message":"x"}}"#;
        assert!(request_fault(refused));
        let failed = br#"{"type":"response.failed","response":{"error":{"code":"invalid_request","message":"x"}}}"#;
        assert!(request_fault(failed));
        let overloaded =
            br#"{"error":{"type":"service_unavailable_error","code":"server_is_overloaded"}}"#;
        assert!(!request_fault(overloaded));
        assert!(!request_fault(b"bad gateway"));
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
