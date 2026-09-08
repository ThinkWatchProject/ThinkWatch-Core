//! 共享类型：请求/响应 DTO、调用上下文、网关错误。
//!
//! 这一层刻意零业务依赖 —— 它是企业版和桌面版都要认的契约，
//! 任何在这里出现的第三方 crate 都会被两边同时背上。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

/// Per-call metadata that is *not* part of the request payload: caller
/// identity for header-template substitution, plus the trace id that
/// correlates the downstream request, the gateway log and the upstream
/// log line.
///
/// This used to live on `ChatCompletionRequest` as three `#[serde(skip)]`
/// fields. That was wrong in a way that only shows up at the seams:
/// `ChatCompletionRequest` is the *wire format*, and a struct that
/// serializes to the upstream body should not also be the carrier for
/// "who is calling". Every construction site paid for it with three
/// lines of `None`, and anything that legitimately built a request
/// without a caller (cache probes, the protocol prober, Bedrock's
/// internal re-shaping) had to opt out of fields it never wanted.
///
/// `attrs` is an open dictionary rather than named fields on purpose:
/// the substitution engine only does `{{key}}` → value and does not
/// understand what any key means. Adding `{{team_id}}` to a header
/// template becomes a caller-side change, not a signature change here.
#[derive(Debug, Clone, Default)]
pub struct CallCtx {
    /// Forwarded upstream as `x-trace-id` when present (OBS-01).
    pub trace_id: Option<String>,
    /// Values for `{{...}}` placeholders in custom header templates.
    /// Conventional keys: `user_id`, `user_email`.
    pub attrs: std::collections::HashMap<String, String>,
}

impl CallCtx {
    /// Convenience for the common enterprise case: caller identity plus
    /// a trace id. Empty/absent values are simply not inserted, so a
    /// template referencing a missing key resolves to the empty string
    /// (the previous behaviour).
    pub fn new(
        trace_id: Option<String>,
        user_id: Option<String>,
        user_email: Option<String>,
    ) -> Self {
        let mut attrs = std::collections::HashMap::new();
        if let Some(v) = user_id {
            attrs.insert("user_id".to_string(), v);
        }
        if let Some(v) = user_email {
            attrs.insert("user_email".to_string(), v);
        }
        Self { trace_id, attrs }
    }

    /// Trace-only context, for internal calls with no caller identity.
    pub fn trace(trace_id: impl Into<String>) -> Self {
        Self {
            trace_id: Some(trace_id.into()),
            attrs: std::collections::HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: serde_json::Value,
    /// Pass-through bucket for the rest of the OpenAI / Anthropic
    /// message envelope: `tool_call_id` (required when `role: "tool"`),
    /// `tool_calls` (assistant-side function invocations), `name`
    /// (legacy function-call / multi-user labelling), `refusal`,
    /// vendor annotations.
    ///
    /// Without this flatten, serde quietly drops anything we don't
    /// declare — the gateway then forwards a stripped message and
    /// the upstream 400s with `missing field "tool_call_id"` the
    /// first time the conversation uses tools, with no signal that
    /// the gateway ate the field on the way through.
    ///
    /// Construct with `..Default::default()` if only role + content
    /// matter so future additions to this struct don't ripple across
    /// every literal in the codebase.
    #[serde(flatten, default, skip_serializing_if = "is_empty_extras")]
    pub extra: serde_json::Value,
}

fn is_empty_extras(v: &serde_json::Value) -> bool {
    matches!(v, serde_json::Value::Null)
        || matches!(v, serde_json::Value::Object(o) if o.is_empty())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub index: u32,
    pub message: ChatMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: serde_json::Value,
    pub finish_reason: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// Catch-all upstream failure that doesn't fit one of the more
    /// specific variants below. Prefer `ProviderHttpError` /
    /// `ProviderTimeout` / `ProviderInvalidResponse` when the cause
    /// is known so dashboards can split errors by class instead of
    /// regex'ing the message.
    #[error("Provider error: {0}")]
    ProviderError(String),
    /// Upstream returned a non-2xx, non-429, non-401 status. The
    /// status is kept structured so error-classifier metrics stay
    /// readable and the gateway can classify retry-eligible 5xx
    /// versus poison 4xx without parsing the message.
    #[error("Provider HTTP {status}: {message}")]
    ProviderHttpError { status: u16, message: String },
    /// Upstream took longer than the configured timeout. Distinct
    /// from a network drop because the request reached the upstream
    /// — only the response was missing in time.
    #[error("Provider timeout: {0}")]
    ProviderTimeout(String),
    /// Upstream responded but the body wasn't parseable as the
    /// expected schema (chat completion / messages / etc.). Almost
    /// always indicates an upstream incident or a model-specific
    /// quirk, and is poison for retries — failover should still
    /// happen but retry against the SAME upstream is pointless.
    #[error("Provider returned invalid response: {0}")]
    ProviderInvalidResponse(String),
    #[error("Request transform error: {0}")]
    TransformError(String),
    #[error("Network error: {0}")]
    NetworkError(String),
    /// Upstream returned 429. `retry_after_secs` captures the value
    /// parsed off the upstream's `Retry-After` header (delta-seconds
    /// form per RFC 7231) so we can echo it to our client and stop
    /// clients spinning into a tight retry loop while quota is still
    /// burning. `None` means the upstream didn't tell us — we pick a
    /// conservative default downstream.
    #[error("Rate limited by upstream")]
    UpstreamRateLimited { retry_after_secs: Option<u32> },
    #[error("Authentication failed with upstream")]
    UpstreamAuthError,
    /// Local rate limit / budget cap was hit. The String is the rule
    /// label so the response body can tell the caller WHICH limit
    /// fired (e.g. "user requests/5h", "api_key tokens/1d",
    /// "monthly budget"). Maps to 429 in `IntoResponse`.
    #[error("Rate limited: {0}")]
    LocalRateLimited(String),
}

impl GatewayError {
    /// Canonical HTTP status code for this error variant. Single source
    /// of truth shared between the response wire status
    /// (`GatewayErrorResponse::into_response`), the non-streaming log
    /// row writer, and the streaming `StreamOutcome::UpstreamError`
    /// path — drift between any of these would make the gateway_logs
    /// `status_code` field disagree with what the client saw, leading
    /// operators to chase phantom 502s for what was actually a 429.
    pub fn status_code(&self) -> i64 {
        match self {
            GatewayError::ProviderError(_) => 502,
            GatewayError::ProviderHttpError { status, .. } => i64::from(*status),
            GatewayError::ProviderTimeout(_) => 504,
            GatewayError::ProviderInvalidResponse(_) => 502,
            GatewayError::TransformError(_) => 400,
            GatewayError::NetworkError(_) => 502,
            GatewayError::UpstreamRateLimited { .. } | GatewayError::LocalRateLimited(_) => 429,
            GatewayError::UpstreamAuthError => 401,
        }
    }

    /// Short stable tag derived from the variant name. Used as a
    /// dashboard-friendly label (Prometheus value, gateway_logs
    /// `error_type` field). Never localize — operators grep on these.
    pub fn error_tag(&self) -> &'static str {
        match self {
            GatewayError::ProviderError(_) => "ProviderError",
            GatewayError::ProviderHttpError { .. } => "ProviderHttpError",
            GatewayError::ProviderTimeout(_) => "ProviderTimeout",
            GatewayError::ProviderInvalidResponse(_) => "ProviderInvalidResponse",
            GatewayError::TransformError(_) => "TransformError",
            GatewayError::NetworkError(_) => "NetworkError",
            GatewayError::UpstreamRateLimited { .. } => "UpstreamRateLimited",
            GatewayError::LocalRateLimited(_) => "LocalRateLimited",
            GatewayError::UpstreamAuthError => "UpstreamAuthError",
        }
    }

    /// Hint, in seconds, for `Retry-After` on a 429 response. For
    /// upstream limits we echo the upstream's own header when present;
    /// for local limits we fall back to a conservative 30s so naive
    /// clients don't spin into a tight retry loop while the bucket is
    /// still refilling. Capped at one hour to keep the header sane
    /// even when an upstream returns an absurd value.
    pub fn retry_after_secs(&self) -> Option<u32> {
        const HARD_CAP_SECS: u32 = 3600;
        const LOCAL_DEFAULT_SECS: u32 = 30;
        match self {
            GatewayError::UpstreamRateLimited { retry_after_secs } => {
                retry_after_secs.map(|s| s.min(HARD_CAP_SECS))
            }
            GatewayError::LocalRateLimited(_) => Some(LOCAL_DEFAULT_SECS),
            _ => None,
        }
    }
}

/// Parse RFC 7231 `Retry-After` (delta-seconds form). HTTP-date is
/// intentionally not supported — the absolute-time variant is
/// effectively unused by upstream LLM providers and would require
/// dragging in a date parser plus clock-skew handling for a vanishingly
/// rare path. Bad input silently maps to None, mirroring how a missing
/// header is treated; a malformed header is no better than no header.
pub fn parse_retry_after_seconds(value: &str) -> Option<u32> {
    value.trim().parse::<u32>().ok()
}

/// Shared base for all AI providers. Holds the HTTP client, base URL,
/// and custom header templates. Previously each provider duplicated
/// these three fields and the identical `new()`, `with_custom_headers()`,

/// Replace every `{{key}}` occurrence in `template` with `attrs[key]`,
/// or with the empty string when the key is absent.
pub fn substitute_template(
    template: &str,
    attrs: &std::collections::HashMap<String, String>,
) -> String {
    // Fast path: most header values carry no placeholder at all.
    if !template.contains("{{") {
        return template.to_string();
    }
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let key = after[..end].trim();
                if let Some(v) = attrs.get(key) {
                    out.push_str(v);
                }
                rest = &after[end + 2..];
            }
            // Unterminated `{{` — emit the rest verbatim rather than
            // silently truncating a header value.
            None => {
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod call_ctx_tests {
    use super::*;

    fn attrs(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn substitutes_known_keys() {
        let a = attrs(&[("user_id", "u1"), ("user_email", "a@b.c")]);
        assert_eq!(substitute_template("{{user_id}}", &a), "u1");
        assert_eq!(
            substitute_template("id={{user_id}};mail={{user_email}}", &a),
            "id=u1;mail=a@b.c"
        );
    }

    #[test]
    fn missing_key_becomes_empty_not_literal() {
        // The old implementation had the same behaviour via `unwrap_or("")`.
        // Keeping it: a literal `{{user_id}}` reaching the upstream looks
        // like a working config and is harder to diagnose than a blank.
        assert_eq!(substitute_template("{{nope}}", &attrs(&[])), "");
        assert_eq!(substitute_template("x{{nope}}y", &attrs(&[])), "xy");
    }

    #[test]
    fn passes_through_values_without_placeholders() {
        let a = attrs(&[("user_id", "u1")]);
        assert_eq!(substitute_template("plain", &a), "plain");
        assert_eq!(substitute_template("", &a), "");
    }

    #[test]
    fn unterminated_placeholder_is_kept_verbatim() {
        // Truncating here would silently shorten a header value.
        assert_eq!(substitute_template("a{{user", &attrs(&[])), "a{{user");
    }

    #[test]
    fn new_skips_absent_identity() {
        let ctx = CallCtx::new(Some("t1".into()), None, Some("a@b.c".into()));
        assert_eq!(ctx.trace_id.as_deref(), Some("t1"));
        assert!(!ctx.attrs.contains_key("user_id"));
        assert_eq!(
            ctx.attrs.get("user_email").map(String::as_str),
            Some("a@b.c")
        );
    }
}
