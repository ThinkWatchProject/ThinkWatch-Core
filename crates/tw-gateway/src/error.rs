//! 错误契约：**客户端看到的必须是它自己方言的错误
//! 结构**，而不是我们的。一个 Anthropic 客户端收到 OpenAI 形状的错误
//! body，会在解析时炸掉，然后报一个和真实原因无关的错。
//!
//! 另外每个错误都带 `x-thinkwatch-error` 头和 `[ThinkWatch]` 前缀 ——
//! 让人一眼看出这一层是谁，而不是去怀疑上游。

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tw_dialect::ir::Dialect;
use tw_types::Msg;
#[cfg(test)]
use tw_types::msg;

/// 失败源。分类的意义在于**用户能看出该去哪儿修**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// 网关密钥不对或没带
    Auth,
    /// 配置有问题（没有可用上游等）
    Config,
    /// 上游连不上
    Upstream,
    /// 请求本身有问题
    Request,
    /// 上游说「慢点」：对面的额度到顶了。回 502 的话客户端会当成「服务器
    /// 坏了」而不是「该退避了」，而它们该做的事完全不同。
    RateLimited,
    /// 一条 `deny` 规则挡下来的。
    ///
    /// **和 `Request` 分开是有理由的**：`Request` 说的是「你这个请求本身
    /// 有问题」，而这里请求完全合法，是策略不让。混在一起的话，用户会
    /// 去改他的请求，而该改的是规则。表里它是 403 +
    /// `permission_error`。
    Denied,
    /// 这个接口在这一家上游那里没有，客户端该换一种办法。
    ///
    /// **501 `not_supported` 是 Claude Code 认的回答**：它的网关对接约定里，Bedrock
    /// 上游数不了 token 时就回这个，客户端会改用一次 `max_tokens: 1` 的请求来数。回
    /// 400 的话它当成请求写错了。对外的词表（`x-thinkwatch-error`、`RequestFailed.source`）
    /// 里算 `request`：服务不了的是这个请求要的接口
    NotSupported,
    /// 能服务这个请求的上游都满着（各自的 `max_concurrent`，见 [`crate::slots`]），等过了
    /// 也没空出来。**和上游限流一样回 429，另外带 `Retry-After`**：请求本身没问题，过一会儿
    /// 再来就能发出去 —— 客户端该退避再试，不是放弃。对外的词表里算 `rate_limited`
    Busy,
    /// 请求体超过网关的上限（见 `server::intake`）。**413，不是 400**：Anthropic 的格式里
    /// 它是 `request_too_large`，和 Anthropic 自己嫌请求太大时一样。对外的词表里算 `request`
    TooLarge,
    /// 上游在 `failover.idle_timeout_secs` 里一直没有内容，候选也用完了。**504，不是 502**：
    /// 上游没坏，是没在时限里回话 —— Anthropic 的格式里它是 `timeout_error`，Gemini 是
    /// `DEADLINE_EXCEEDED`。对外的词表里算 `upstream`
    Timeout,
    /// 在界面上手动中止的（见 [`crate::abort`]）。**499**：客户端的 SDK 不重试 4xx
    /// （408、409、429 除外），不会把用户叫停的请求自己再发一遍；Google 的接口给「操作被
    /// 取消」用的也是它。对外的词表里是 `aborted`
    Aborted,
}

/// 上游都满着时告诉客户端过几秒再来（`Retry-After`）。
///
/// **不按 `slot_wait_secs` 算**：那么久已经在网关里等过了，再让客户端干等同样久没有意义。
/// 重试进来照样排队等空位，所以这个数只管客户端别立刻打回来
pub const BUSY_RETRY_AFTER_SECS: u64 = 5;

impl Source {
    /// `x-thinkwatch-error` 头和 `RequestFailed.source` 共用的词表。
    pub fn slug(&self) -> &'static str {
        match self {
            Source::Auth => "auth",
            Source::Config => "config",
            Source::Upstream => "upstream",
            Source::Request => "request",
            Source::RateLimited | Source::Busy => "rate_limited",
            Source::Denied => "denied",
            Source::NotSupported | Source::TooLarge => "request",
            Source::Timeout => "upstream",
            Source::Aborted => "aborted",
        }
    }
    fn status(&self) -> StatusCode {
        match self {
            Source::Auth => StatusCode::UNAUTHORIZED,
            Source::Config => StatusCode::INTERNAL_SERVER_ERROR,
            Source::Upstream => StatusCode::BAD_GATEWAY,
            Source::Request => StatusCode::BAD_REQUEST,
            // 429 而不是 503：客户端至少知道这是限流，可以退避。
            Source::RateLimited | Source::Busy => StatusCode::TOO_MANY_REQUESTS,
            Source::Denied => StatusCode::FORBIDDEN,
            Source::NotSupported => StatusCode::NOT_IMPLEMENTED,
            Source::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Source::Timeout => StatusCode::GATEWAY_TIMEOUT,
            Source::Aborted => StatusCode::from_u16(499).expect("499 is a valid status code"),
        }
    }
}

/// 一句话发给 AI 客户端时的样子：错误体、错误帧、WebSocket 上替被拦下的那一帧发的都是它。
/// 安全日志记的「客户端收到的那句话」也照它写（[`crate::guard::detail`]），两边对得上。
pub fn client_notice(detail: &Msg) -> String {
    format!("[ThinkWatch] {}", detail.text)
}

#[derive(Debug)]
pub struct GatewayError {
    pub source: Source,
    /// 为什么失败。**发给 AI 客户端的是 `detail.text`**（它们只认字符串），
    /// 界面拿 `detail.code` 去自己的词表里找句子。见 [`tw_types::Msg`]。
    pub detail: Msg,
    /// 用哪种格式的形状回。**认证失败时还不知道格式**（key 就是没认出
    /// 来），所以先按 Anthropic —— 桌面版的主用例是 Claude Code。
    pub dialect: Dialect,
    /// 什么时候能再来：密钥的用量上限拒绝的（见 [`crate::key_limits`]），和上游都满着的
    /// （[`Source::Busy`]）。别的没有
    pub retry: Option<Retry>,
}

/// 密钥的用量上限拒绝了一个请求，或者上游都满着（[`Source::Busy`]）：客户端什么时候能再来。
///
/// 写成响应头：`Retry-After`（秒）。滚动窗口另带 `retry-after-ms` 和 `x-should-retry: true`
/// —— Anthropic、OpenAI 的官方 SDK 按毫秒那个等，等完自己重试。**自然周期用完了带
/// `x-should-retry: false`**：到下一期之前重试多少次都一样，SDK 照默认的退避连试几次只会
/// 让用户多等；OpenAI 格式的错误体再带 `code: insufficient_quota`，和 OpenAI 自己额度用完
/// 时一样 —— 认这个码的客户端（Codex）会直接停下来告诉用户，不再重试。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    /// 多久之后，毫秒
    pub after_ms: u64,
    /// 这一期用完了，到重置之前重试也没用
    pub until_reset: bool,
}

impl GatewayError {
    pub fn new(source: Source, detail: Msg) -> Self {
        Self {
            source,
            detail,
            dialect: Dialect::Anthropic,
            retry: None,
        }
    }

    pub fn with_retry(mut self, r: Retry) -> Self {
        self.retry = Some(r);
        self
    }

    /// 给人看的那句话。
    pub fn message(&self) -> &str {
        &self.detail.text
    }

    /// 在原因后面补上尝试过哪几家上游。
    ///
    /// **码不变，补的是同一条消息的细节。**界面照 `code` 说它自己那句话，
    /// 需要时从 `attempts` 参数里取这份链路；只读字符串的客户端拿到的是
    /// 补过的 `text`。
    pub fn with_attempts(mut self, attempts: &[String]) -> Self {
        if attempts.is_empty() {
            return self;
        }
        let chain = attempts.join(" → ");
        self.detail.text = format!("{} (tried: {chain})", self.detail.text);
        self.detail.args.insert("attempts".into(), chain);
        self
    }
    /// 认出客户端之后补上格式。**忘了调只会退回 Anthropic 形状**，
    /// 那是个安全的默认，不是一个静默的错误。
    pub fn in_dialect(mut self, d: Dialect) -> Self {
        self.dialect = d;
        self
    }
    pub fn rate_limited(detail: Msg) -> Self {
        Self::new(Source::RateLimited, detail)
    }
    /// 上游都满着：429，[`BUSY_RETRY_AFTER_SECS`] 之后再来，可以重试
    pub fn busy(detail: Msg) -> Self {
        Self::new(Source::Busy, detail).with_retry(Retry {
            after_ms: BUSY_RETRY_AFTER_SECS * 1000,
            until_reset: false,
        })
    }
    pub fn denied(detail: Msg) -> Self {
        Self::new(Source::Denied, detail)
    }
    pub fn auth(detail: Msg) -> Self {
        Self::new(Source::Auth, detail)
    }
    pub fn config(detail: Msg) -> Self {
        Self::new(Source::Config, detail)
    }
    pub fn upstream(detail: Msg) -> Self {
        Self::new(Source::Upstream, detail)
    }
    pub fn request(detail: Msg) -> Self {
        Self::new(Source::Request, detail)
    }
    pub fn too_large(detail: Msg) -> Self {
        Self::new(Source::TooLarge, detail)
    }
    pub fn timeout(detail: Msg) -> Self {
        Self::new(Source::Timeout, detail)
    }
    /// 手动中止（见 [`crate::abort`]）。句子只有一句，码是 [`tw_api::ABORTED`]
    pub fn aborted() -> Self {
        Self::new(
            Source::Aborted,
            tw_types::msg!("gw.request.aborted" => "The request was aborted by the user."),
        )
    }
}

impl GatewayError {
    /// 发给客户端的那句话。`[ThinkWatch]` 前缀不是装饰：没有它，用户看到
    /// 一个 401 会先去查上游的密钥 —— 而问题在中间这一层。
    fn client_message(&self) -> String {
        client_notice(&self.detail)
    }

    /// 流中途断掉时，唯一还能说话的地方是流本身。
    ///
    /// 首字节已经发出去了，状态码和响应头都改不了 —— 什么都不做的话，
    /// 客户端看到的是一个**戛然而止的流**，而截断和「答完了」在 SSE
    /// 里长得一模一样。帧的形状见 [`tw_dialect::convert::error_frame`]。
    pub fn sse_frame(&self) -> String {
        tw_dialect::convert::error_frame(
            self.dialect,
            self.source.status().as_u16(),
            &self.client_message(),
        )
    }

    /// 这个错误的响应体，**客户端格式的形状**（见
    /// [`tw_dialect::convert::error_body`]）。
    ///
    /// 非流式的响应体被扣下来时要顶替它的位置：状态码和响应头那时已经
    /// 发出去了，body 是唯一还能说话的地方 —— 和 `sse_frame` 在流上扮
    /// 演的是同一个角色。
    pub fn body_bytes(&self) -> Vec<u8> {
        let body = tw_dialect::convert::error_body(
            self.dialect,
            self.source.status().as_u16(),
            &self.client_message(),
        );
        // 自然周期用完了：OpenAI 格式里写成额度用完（见 [`Retry`]）
        if self.retry.is_some_and(|r| r.until_reset)
            && matches!(self.dialect, Dialect::Chat | Dialect::Responses)
            && let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&body)
            && let Some(e) = v.get_mut("error").and_then(|e| e.as_object_mut())
        {
            e.insert("code".into(), "insufficient_quota".into());
            return v.to_string().into_bytes();
        }
        body
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let mut resp = (self.source.status(), self.body_bytes()).into_response();
        let h = resp.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        h.insert(
            "x-thinkwatch-error",
            HeaderValue::from_static(self.source.slug()),
        );
        if let Some(r) = self.retry {
            let secs = r.after_ms.div_ceil(1000).max(1);
            h.insert(header::RETRY_AFTER, HeaderValue::from(secs));
            if r.until_reset {
                h.insert("x-should-retry", HeaderValue::from_static("false"));
            } else {
                h.insert("retry-after-ms", HeaderValue::from(r.after_ms.max(1)));
                h.insert("x-should-retry", HeaderValue::from_static("true"));
            }
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_of(e: GatewayError) -> (StatusCode, String, serde_json::Value) {
        let r = e.into_response();
        let status = r.status();
        let slug = r
            .headers()
            .get("x-thinkwatch-error")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let b = to_bytes(r.into_body(), 64 * 1024).await.unwrap();
        (status, slug, serde_json::from_slice(&b).unwrap())
    }

    #[tokio::test]
    async fn an_auth_failure_is_401_and_says_who_rejected_it() {
        let (status, slug, json) =
            body_of(GatewayError::auth(msg!("t.auth" => "the key is wrong"))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(slug, "auth");
        // 客户端解析的是 Anthropic 的形状
        assert_eq!(json["error"]["type"], "authentication_error");
        // 用户一眼看出是哪一层拒的
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("[ThinkWatch]")
        );
    }

    #[tokio::test]
    async fn upstream_failures_are_502_not_500() {
        // 500 会让人怀疑我们；502 说清楚是上游那边。
        let (status, slug, _) = body_of(GatewayError::upstream(
            msg!("t.upstream" => "cannot connect"),
        ))
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(slug, "upstream");
    }

    #[tokio::test]
    async fn each_source_maps_to_its_own_status_and_slug() {
        for (e, want_status, want_slug) in [
            (
                GatewayError::config(msg!("t.x" => "x")),
                StatusCode::INTERNAL_SERVER_ERROR,
                "config",
            ),
            (
                GatewayError::request(msg!("t.x" => "x")),
                StatusCode::BAD_REQUEST,
                "request",
            ),
        ] {
            let (status, slug, _) = body_of(e).await;
            assert_eq!(status, want_status);
            assert_eq!(slug, want_slug);
        }
    }

    #[tokio::test]
    async fn each_client_gets_the_error_in_its_own_shape() {
        let e = || GatewayError::rate_limited(msg!("t.x" => "slow down"));
        let (_, _, json) = body_of(e().in_dialect(Dialect::Chat)).await;
        assert_eq!(json["error"]["type"], "rate_limit_error");
        assert!(json["error"]["param"].is_null());
        let (_, _, json) = body_of(e().in_dialect(Dialect::Gemini)).await;
        assert_eq!(json["error"]["code"], 429);
        assert_eq!(json["error"]["status"], "RESOURCE_EXHAUSTED");
        // 上游坏了在 Google 的词表里是 UNAVAILABLE，不是 INTERNAL（那是说我们自己坏了）
        let (_, _, json) =
            body_of(GatewayError::upstream(msg!("t.x" => "x")).in_dialect(Dialect::Gemini)).await;
        assert_eq!(json["error"]["status"], "UNAVAILABLE");
    }

    #[tokio::test]
    async fn busy_upstreams_are_a_429_that_says_when_to_come_back() {
        let r = GatewayError::busy(msg!("t.x" => "all busy"))
            .in_dialect(Dialect::Chat)
            .into_response();
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(r.headers()["retry-after"], "5");
        // 和密钥用量上限的滚动窗口同一套头：可以重试，毫秒的那个给 SDK 用
        assert_eq!(r.headers()["retry-after-ms"], "5000");
        assert_eq!(r.headers()["x-should-retry"], "true");
        assert_eq!(r.headers()["x-thinkwatch-error"], "rate_limited");
        let b = to_bytes(r.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(json["error"]["type"], "rate_limit_error");
        // 上游自己的限流不带：要等多久由上游说，网关不替它编一个数
        let r = GatewayError::rate_limited(msg!("t.x" => "slow down")).into_response();
        assert!(!r.headers().contains_key("retry-after"));
    }

    /// 密钥的用量上限拒绝时：多久之后能再来，到重置之前该不该重试。
    #[tokio::test]
    async fn a_used_up_key_says_when_to_come_back_and_whether_to_retry() {
        let e = || GatewayError::rate_limited(msg!("t.x" => "x"));
        let head =
            |r: &Response, h: &str| r.headers().get(h).map(|v| v.to_str().unwrap().to_string());
        let body = |r: Response| async {
            let b = to_bytes(r.into_body(), 64 * 1024).await.unwrap();
            serde_json::from_slice::<serde_json::Value>(&b).unwrap()
        };
        // 这一期用完了：到重置那一刻，别重试；OpenAI 的两种格式写成额度用完
        let until_reset = Retry {
            after_ms: 3_600_500,
            until_reset: true,
        };
        for d in [Dialect::Chat, Dialect::Responses] {
            let r = e().with_retry(until_reset).in_dialect(d).into_response();
            assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(head(&r, "retry-after").as_deref(), Some("3601"));
            assert_eq!(head(&r, "x-should-retry").as_deref(), Some("false"));
            assert_eq!(head(&r, "retry-after-ms"), None);
            let v = body(r).await;
            assert_eq!(v["error"]["code"], "insufficient_quota", "{d:?}");
            assert_eq!(v["error"]["type"], "rate_limit_error");
        }
        let v = body(e().with_retry(until_reset).into_response()).await;
        assert_eq!(
            v["error"]["type"], "rate_limit_error",
            "Anthropic 的形状不变"
        );
        let v = body(
            e().with_retry(until_reset)
                .in_dialect(Dialect::Gemini)
                .into_response(),
        )
        .await;
        assert_eq!(v["error"]["status"], "RESOURCE_EXHAUSTED");
        // 滚动窗口：准确到毫秒，可以重试
        let r = e()
            .with_retry(Retry {
                after_ms: 1_500,
                until_reset: false,
            })
            .in_dialect(Dialect::Chat)
            .into_response();
        assert_eq!(head(&r, "retry-after").as_deref(), Some("2"));
        assert_eq!(head(&r, "retry-after-ms").as_deref(), Some("1500"));
        assert_eq!(head(&r, "x-should-retry").as_deref(), Some("true"));
        assert!(body(r).await["error"]["code"].is_null());
        // 别的错误什么都不加
        let r = e().into_response();
        assert_eq!(head(&r, "retry-after"), None);
        assert_eq!(head(&r, "x-should-retry"), None);
    }

    #[tokio::test]
    async fn a_timeout_is_504_and_an_abort_is_499() {
        let (status, slug, json) =
            body_of(GatewayError::timeout(msg!("t.x" => "quiet")).in_dialect(Dialect::Gemini))
                .await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(slug, "upstream");
        assert_eq!(json["error"]["status"], "DEADLINE_EXCEEDED");
        let (_, _, json) = body_of(GatewayError::timeout(msg!("t.x" => "quiet"))).await;
        assert_eq!(json["error"]["type"], "timeout_error");
        let (status, slug, json) = body_of(GatewayError::aborted()).await;
        assert_eq!(status.as_u16(), 499);
        assert_eq!(slug, "aborted");
        assert_eq!(GatewayError::aborted().detail.code, tw_api::ABORTED);
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("[ThinkWatch]")
        );
        let (_, _, json) = body_of(GatewayError::aborted().in_dialect(Dialect::Gemini)).await;
        assert_eq!(json["error"]["status"], "CANCELLED");
    }

    #[test]
    fn a_responses_stream_is_told_with_response_failed() {
        // Chat 形状的 `{"error":…}` 在 Responses 的流里是一帧没人认的数据，客户端
        // 看到的是流没说完就断了
        let f = GatewayError::upstream(msg!("t.x" => "reset"))
            .in_dialect(Dialect::Responses)
            .sse_frame();
        assert!(f.starts_with("event: response.failed\n"), "{f}");
        assert!(f.contains("[ThinkWatch] reset"), "{f}");
        let a = GatewayError::upstream(msg!("t.x" => "reset")).sse_frame();
        assert!(a.starts_with("event: error\n"), "{a}");
    }
}
