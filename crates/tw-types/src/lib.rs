//! 共享类型：请求/响应 DTO、调用上下文、网关错误、给人看的话。
//!
//! 这一层刻意零业务依赖 —— 它是企业版和桌面版都要认的契约，
//! 任何在这里出现的第三方 crate 都会被两边同时背上。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 一条给人看的话：一个稳定的码、填进句子的参数，以及英文原句。
///
/// **core 不翻译，只出英文。**桌面版有中英两套界面，而这些句子是从这里
/// 发出去的。在这里翻译等于把「界面现在是什么语言」塞进一个同时服务
/// 命令行、桌面版和企业版的网关进程 —— 那个问题在这一层没有答案。
///
/// 所以这里给的是码加参数：界面拿 `code` 去自己的词表里找句子，用 `args`
/// 填空。没有词表的一方（命令行、第三方客户端）显示 `text` —— 一句英文
/// 总好过一个码。
///
/// **码是契约，句子不是。**改措辞不用动码，界面那边什么都不用做；只有
/// 语义变了才换码 —— 那时界面里那条旧文案会随着码一起失效，而不是
/// 悄悄留在那里说着一件不再为真的事。
///
/// 码的写法是点分小写，从粗到细：`l1.dns.timeout`、`l1.proxy.auth_rejected`。
/// 第一段是发出它的那一层。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Msg {
    pub code: String,
    /// 填进句子里的参数，按名字取。**值已经写成字符串** —— 界面只是
    /// 把它们放进自己那句话里，不需要知道原来是什么类型
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub args: BTreeMap<String, String>,
    /// 英文原句，参数已经填好
    pub text: String,
}

impl std::fmt::Display for Msg {
    /// 命令行和日志里就用英文原句。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl Msg {
    /// 取一个参数。**没有就是空串** —— 调用方是在拼一句话，为一个缺席的
    /// 参数 panic 没有意义
    pub fn arg(&self, name: &str) -> &str {
        self.args.get(name).map(String::as_str).unwrap_or_default()
    }
}

/// 造一条 [`Msg`]。
///
/// ```ignore
/// msg!("l1.dns.no_records", host = host => "`{host}` resolved, but to no addresses");
/// ```
///
/// 参数先 `let` 出来，所以句子里直接写 `{host}` 就能取到，同时它们原样
/// 进 `args` —— **两边不会说不同的话**，这正是分开写最容易出错的地方。
#[macro_export]
macro_rules! msg {
    ($code:expr $(, $name:ident = $value:expr)* $(,)? => $($fmt:tt)+) => {{
        $(let $name = $value;)*
        $crate::Msg {
            code: ($code).into(),
            args: ::std::collections::BTreeMap::from([
                $((
                    ::std::string::String::from(stringify!($name)),
                    ::std::string::ToString::to_string(&$name),
                ),)*
            ]),
            text: format!($($fmt)+),
        }
    }};
}

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

/// 日志留多久。
///
/// **两个期限，因为两种东西的代价差三个数量级。**请求和响应的正文一条
/// 几十 KB，一天就能堆出几百 MB；而一行记录（时刻、模型、用量、金额）
/// 只有几百字节，留一个季度也不过几十 MB。用一个期限管住两者，等于
/// 要么早早丢掉「上个月花了多少」，要么让磁盘替正文买单。
///
/// **总量上限是给突发准备的。**按天数算出来的占用取决于用量，而用量
/// 会有一周十倍于平时的时候 —— 没有上限的话，那一周会把磁盘吃光，
/// 而用户直到硬盘满了才知道。
///
/// 住在 tw-types 的理由和 [`Limits`] 一样：配置和存储层都要认它。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retention {
    /// 请求和响应的正文留几天。**大头在这儿**
    #[serde(default = "d_body_days")]
    pub body_days: u64,
    /// 一行记录留几天。它撑着「上个月花了多少」那类问题
    #[serde(default = "d_row_days")]
    pub row_days: u64,
    /// 正文总共最多占多少字节。超了从最旧的整天开始删
    #[serde(default = "d_body_max_bytes")]
    pub body_max_bytes: u64,
}

fn d_body_days() -> u64 {
    7
}
fn d_row_days() -> u64 {
    90
}
fn d_body_max_bytes() -> u64 {
    2 * 1024 * 1024 * 1024
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            body_days: d_body_days(),
            row_days: d_row_days(),
            body_max_bytes: d_body_max_bytes(),
        }
    }
}

/// 并发上限。住在 tw-types 是因为配置和数据面都要认
/// 它，而它本身只是几个数字 —— 不该为此让 tw-config 依赖 tw-gateway。
///
/// **没有全局上限。**一个本机网关同时在跑的请求，就是这台电脑上几个客户端
/// 各自开着的那几个会话；再压一道总闸，挡住的只会是用户自己的并行任务。
/// 要防的是另外两件事：一个变慢的上游占住所有请求（`per_provider`），和
/// 某一把密钥后面的失控脚本（密钥自己的 `max_concurrent`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// 配置里的每一个结构都拒未知字段，唯独这一个在 tw-types 里，漏了。
// 表现是 `max_body_bytes: 8388608` 写进去不报错、也不生效 —— 用户改
// 了个上限，界面说写成功了，什么都没发生。（这一层别的类型是聊天接口
// 的线上类型，上游随时会加字段，那些恰恰不能拒。）
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// 单个上游
    #[serde(default = "d_per_provider")]
    pub per_provider: usize,
    /// 队列上限。满了才真的拒绝
    #[serde(default = "d_queue_depth")]
    pub queue_depth: usize,
    /// 排太久还是要放弃
    #[serde(default = "d_queue_timeout")]
    pub queue_timeout_secs: u64,
}

fn d_per_provider() -> usize {
    8
}
fn d_queue_depth() -> usize {
    64
}
fn d_queue_timeout() -> u64 {
    30
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            per_provider: d_per_provider(),
            queue_depth: d_queue_depth(),
            queue_timeout_secs: d_queue_timeout(),
        }
    }
}

/// RFC1918 三个私网段 + 回环。监听非 loopback 时 `allow_from` 的默认值。
/// 住在这里是因为配置层要用它填默认值，数据面要用它
/// 判断 —— 而它只是一组字符串。
pub const PRIVATE_RANGES: &[&str] = &[
    "127.0.0.0/8",
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "::1/128",
    "fc00::/7",
];

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

    #[test]
    fn a_message_carries_its_arguments_beside_the_sentence() {
        // 界面要照自己的语序重写这句话，所以参数必须单独拿得到 ——
        // 从成句的英文里再切出主机名是切不准的。
        let m = msg!(
            "l1.dns.timeout", host = "api.example.com", seconds = 8u64 =>
            "Resolving {host} did not finish within {seconds} s."
        );
        assert_eq!(m.code, "l1.dns.timeout");
        assert_eq!(m.arg("host"), "api.example.com");
        assert_eq!(m.arg("seconds"), "8");
        assert_eq!(
            m.text,
            "Resolving api.example.com did not finish within 8 s."
        );
        // 没有的参数是空串，不是 panic
        assert_eq!(m.arg("port"), "");
    }

    #[test]
    fn a_message_without_arguments_leaves_the_map_out_of_the_wire() {
        let m = msg!("l1.config.no_host" => "The endpoint address has no host name.");
        assert!(m.args.is_empty());
        let wire = serde_json::to_value(&m).unwrap();
        assert!(wire.get("args").is_none(), "{wire}");
        assert_eq!(
            serde_json::from_value::<Msg>(wire).unwrap(),
            m,
            "少了 args 也要读得回来"
        );
    }
}
