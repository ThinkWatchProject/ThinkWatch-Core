//! 上游失败的原因：该不该换下一家，这家停用多久。
//!
//! **上游多半说了原因**，在状态码、`Retry-After`、额度头或错误正文里。读出来
//! 之后两件事就定了：
//!
//! - **换不换。**5xx 和限流换，这不用说；401、403（凭据被拒）、402（没钱了）、
//!   404（这家没有这个模型）也换 —— 下一家用的是另一把密钥、另一个账户，很可能
//!   答得了。400、422 多半是请求本身的问题，换了也一样被拒，**只有正文说的是
//!   余额、额度或者模型不可用时才换**。
//! - **停多久**（见 [`crate::health`]）。余额不足要等充值，额度用完要等到重置
//!   时刻，限流按 `Retry-After`；说不出原因的才走「连续几次、冷却翻倍」。
//!
//! 流式回答开头在流里报的错误（Anthropic 的 `overloaded_error`、Codex 的
//! `response.failed`）也走这里：[`crate::server`] 的流开头判断把它换算成对应的
//! 状态码和正文，再交给 [`classify`]。

use std::time::Duration;

use http::HeaderMap;

/// 一家上游这一次为什么没答上来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// 余额不足、欠费
    NoBalance,
    /// 订阅额度用完了。上游说了什么时候重置的，是那一刻（Unix 毫秒）
    QuotaUsedUp { resets_at_ms: Option<u64> },
    /// 被限流。上游说了多久之后再来的，是那么久
    RateLimited { retry_after: Option<Duration> },
    /// 这家没有这个模型，或者不支持。**换下一家，不算这家坏了** —— 它对别的
    /// 模型照样好好的
    ModelUnavailable,
    /// 凭据被拒
    AuthRejected,
    /// 说不出原因的失败：5xx、过载、连接断了
    Unexplained,
}

/// 这个回答该怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 这家没答上来，换下一家
    Failed(Cause),
    /// 请求本身的问题：原样交给客户端，不换、不记这家的账
    ClientError,
}

/// 错误正文最多看多少字节。错误都很短，读太多只是在等一个不是错误的东西
pub const BODY_PEEK: usize = 64 * 1024;

/// 看一个非 2xx 的回答。`body` 是正文的开头（最多 [`BODY_PEEK`]）。
pub fn classify(status: u16, headers: &HeaderMap, body: &[u8], now_ms: u64) -> Verdict {
    let text = String::from_utf8_lossy(body).to_lowercase();
    // **正文说的原因优先于状态码**：没钱了有人回 402，有人回 429、400、403，
    // 甚至 500；额度用完有人回 429，有人回 403
    if says(&text, NO_BALANCE) {
        return Verdict::Failed(Cause::NoBalance);
    }
    let used_up = used_up_reset(headers, now_ms);
    if says(&text, QUOTA_USED_UP) || (status == 429 && used_up.is_some()) {
        return Verdict::Failed(Cause::QuotaUsedUp {
            resets_at_ms: used_up.flatten().or_else(|| reset_in_body(body, now_ms)),
        });
    }
    match status {
        429 => Verdict::Failed(Cause::RateLimited {
            retry_after: retry_after(headers, now_ms).or_else(|| retry_delay_in_body(body)),
        }),
        402 => Verdict::Failed(Cause::NoBalance),
        401 | 403 => Verdict::Failed(Cause::AuthRejected),
        404 => Verdict::Failed(Cause::ModelUnavailable),
        400 | 422 if says(&text, MODEL_UNAVAILABLE) => Verdict::Failed(Cause::ModelUnavailable),
        s if s >= 500 => Verdict::Failed(Cause::Unexplained),
        _ => Verdict::ClientError,
    }
}

/// 余额不足。**要具体到不会误伤**：只写「quota」的话，Gemini 每分钟的限流
/// （`Quota exceeded for metric …`）也会被当成没钱了
const NO_BALANCE: &[&str] = &[
    "insufficient_quota",
    "exceeded your current quota",
    "insufficient balance",
    "insufficient_balance",
    "insufficient account balance",
    "credit balance is too low",
    "balance is too low",
    "payment required",
    "billing_hard_limit",
    "arrearage",
    "余额不足",
    "欠费",
];

/// 订阅额度用完。和限流的区别是**要等到重置时刻**，不是几十秒
const QUOTA_USED_UP: &[&str] = &[
    "usage_limit_reached",
    "usage limit reached",
    "usage limit has been reached",
    "hit your usage limit",
    "reached your usage limit",
    "使用上限",
    "额度已用完",
    "额度已用尽",
];

/// 这家没有这个模型、不支持它
const MODEL_UNAVAILABLE: &[&str] = &[
    "model_not_found",
    "model not found",
    "no such model",
    "unknown model",
    "unsupported model",
    "model is not supported",
    "model not supported",
    "is not a valid model",
    "invalid model",
    "模型不存在",
    "不支持该模型",
    "不支持的模型",
];

fn says(text: &str, words: &[&str]) -> bool {
    words.iter().any(|w| text.contains(w))
}

/// 额度头说用完了没有：`None` 是没说用完，`Some(None)` 是说用完了、没说什么时候
/// 重置，`Some(Some(t))` 是在 `t` 重置。
///
/// 好几个窗口都满了的时候取**最晚**的那个：早的那个重置了，晚的还满着，照样用不了
fn used_up_reset(headers: &HeaderMap, now_ms: u64) -> Option<Option<u64>> {
    let q = crate::quota::from_headers(headers, now_ms);
    let full: Vec<_> = q
        .windows
        .iter()
        .filter(|w| w.rejected() || w.used_percent >= 100.0)
        .collect();
    if full.is_empty() {
        return None;
    }
    Some(full.iter().filter_map(|w| w.resets_at_ms).max())
}

/// `Retry-After`：秒数，或者一个 HTTP 日期。
pub fn retry_after(headers: &HeaderMap, now_ms: u64) -> Option<Duration> {
    let v = headers
        .get(http::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = chrono::DateTime::parse_from_rfc2822(v)
        .ok()?
        .timestamp_millis();
    let left = u64::try_from(at).ok()?.checked_sub(now_ms)?;
    Some(Duration::from_millis(left))
}

/// Gemini 把「多久之后再来」写在正文里：`"retryDelay": "33s"`。
fn retry_delay_in_body(body: &[u8]) -> Option<Duration> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let details = v.pointer("/error/details")?.as_array()?;
    details.iter().find_map(|d| {
        let s = d.get("retryDelay")?.as_str()?;
        let secs: f64 = s.strip_suffix('s')?.parse().ok()?;
        (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs))
    })
}

/// 正文里的重置时刻。Codex 的 `usage_limit_reached` 带 `resets_at`（Unix 秒）或
/// `resets_in_seconds`，有时在 `error` 里面。**已经过去的不算**
fn reset_in_body(body: &[u8], now_ms: u64) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let scopes = [Some(&v), v.get("error")];
    let at = scopes.into_iter().flatten().find_map(|o| {
        if let Some(s) = o.get("resets_at").and_then(serde_json::Value::as_u64) {
            return Some(s.saturating_mul(1000));
        }
        o.get("resets_in_seconds")
            .and_then(serde_json::Value::as_u64)
            .map(|s| now_ms.saturating_add(s.saturating_mul(1000)))
    })?;
    (at > now_ms).then_some(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;

    fn verdict(status: u16, body: &str) -> Verdict {
        classify(status, &HeaderMap::new(), body.as_bytes(), NOW)
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn a_plain_bad_request_goes_back_to_the_client() {
        // 请求本身有问题，换一家也一样被拒，还白白记在那家账上
        assert_eq!(
            verdict(
                400,
                r#"{"error":{"type":"invalid_request_error","message":"max_tokens: must be positive"}}"#
            ),
            Verdict::ClientError
        );
        assert_eq!(verdict(413, "too large"), Verdict::ClientError);
    }

    #[test]
    fn a_missing_balance_is_recognised_whatever_the_status() {
        for (status, body) in [
            (
                400,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API."}}"#,
            ),
            (
                429,
                r#"{"error":{"code":"insufficient_quota","message":"You exceeded your current quota"}}"#,
            ),
            (402, r#"{"error":{"message":"Insufficient Balance"}}"#),
            (403, r#"{"error":{"code":"Arrearage"}}"#),
            (
                429,
                r#"{"error":{"code":"1113","message":"余额不足或无可用资源包"}}"#,
            ),
        ] {
            assert_eq!(
                verdict(status, body),
                Verdict::Failed(Cause::NoBalance),
                "{body}"
            );
        }
    }

    #[test]
    fn a_used_up_quota_waits_for_the_reset_the_upstream_names() {
        let body = format!(
            r#"{{"error":{{"type":"usage_limit_reached","message":"The usage limit has been reached","resets_at":{}}}}}"#,
            NOW / 1000 + 3600
        );
        assert_eq!(
            verdict(429, &body),
            Verdict::Failed(Cause::QuotaUsedUp {
                resets_at_ms: Some(NOW + 3_600_000)
            })
        );
        // 只说了还有多少秒
        assert_eq!(
            verdict(
                429,
                r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":60}}"#
            ),
            Verdict::Failed(Cause::QuotaUsedUp {
                resets_at_ms: Some(NOW + 60_000)
            })
        );
    }

    #[test]
    fn a_429_with_a_rejected_subscription_window_is_a_used_up_quota() {
        // Claude 订阅：正文只说 rate_limit_error，额度头说 5 小时窗口满了
        let h = headers(&[
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-utilization", "1.0"),
            ("anthropic-ratelimit-unified-5h-reset", "7200"),
        ]);
        assert_eq!(
            classify(
                429,
                &h,
                br#"{"type":"error","error":{"type":"rate_limit_error"}}"#,
                NOW
            ),
            Verdict::Failed(Cause::QuotaUsedUp {
                resets_at_ms: Some(NOW + 7_200_000)
            })
        );
    }

    #[test]
    fn a_rate_limit_takes_retry_after_from_the_header_or_the_body() {
        let h = headers(&[("retry-after", "30")]);
        assert_eq!(
            classify(429, &h, b"{}", NOW),
            Verdict::Failed(Cause::RateLimited {
                retry_after: Some(Duration::from_secs(30))
            })
        );
        // Gemini 的每分钟限流写着「Quota exceeded」，**不是没钱了**
        let gemini = r#"{"error":{"code":429,"message":"Quota exceeded for metric: generate_content_free_tier_requests","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"33s"}]}}"#;
        assert_eq!(
            verdict(429, gemini),
            Verdict::Failed(Cause::RateLimited {
                retry_after: Some(Duration::from_secs(33))
            })
        );
        assert_eq!(
            verdict(429, "slow down"),
            Verdict::Failed(Cause::RateLimited { retry_after: None })
        );
    }

    #[test]
    fn credentials_and_missing_models_move_on() {
        assert_eq!(
            verdict(401, "invalid x-api-key"),
            Verdict::Failed(Cause::AuthRejected)
        );
        assert_eq!(
            verdict(403, "forbidden"),
            Verdict::Failed(Cause::AuthRejected)
        );
        assert_eq!(verdict(404, ""), Verdict::Failed(Cause::ModelUnavailable));
        assert_eq!(
            verdict(
                400,
                r#"{"error":{"code":"model_not_found","message":"The model `x` does not exist"}}"#
            ),
            Verdict::Failed(Cause::ModelUnavailable)
        );
        assert_eq!(
            verdict(422, r#"{"detail":"Unsupported model: foo"}"#),
            Verdict::Failed(Cause::ModelUnavailable)
        );
    }

    #[test]
    fn server_errors_are_unexplained_unless_the_body_says_more() {
        assert_eq!(verdict(503, "busy"), Verdict::Failed(Cause::Unexplained));
        assert_eq!(
            verdict(529, "overloaded"),
            Verdict::Failed(Cause::Unexplained)
        );
        assert_eq!(
            verdict(500, "余额不足"),
            Verdict::Failed(Cause::NoBalance),
            "有的中转把没钱了报成 500"
        );
    }

    #[test]
    fn retry_after_reads_an_http_date() {
        let at = chrono::DateTime::from_timestamp_millis((NOW + 90_000) as i64)
            .unwrap()
            .to_rfc2822();
        let h = headers(&[("retry-after", &at)]);
        assert_eq!(retry_after(&h, NOW), Some(Duration::from_secs(90)));
    }
}
