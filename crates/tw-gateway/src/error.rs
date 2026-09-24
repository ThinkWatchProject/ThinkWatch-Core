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
}

impl Source {
    /// `x-thinkwatch-error` 头和 `RequestFailed.source` 共用的词表。
    pub fn slug(&self) -> &'static str {
        match self {
            Source::Auth => "auth",
            Source::Config => "config",
            Source::Upstream => "upstream",
            Source::Request => "request",
            Source::RateLimited => "rate_limited",
            Source::Denied => "denied",
        }
    }
    fn status(&self) -> StatusCode {
        match self {
            Source::Auth => StatusCode::UNAUTHORIZED,
            Source::Config => StatusCode::INTERNAL_SERVER_ERROR,
            Source::Upstream => StatusCode::BAD_GATEWAY,
            Source::Request => StatusCode::BAD_REQUEST,
            // 429 而不是 503：客户端至少知道这是限流，可以退避。
            Source::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Source::Denied => StatusCode::FORBIDDEN,
        }
    }
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
}

impl GatewayError {
    pub fn new(source: Source, detail: Msg) -> Self {
        Self {
            source,
            detail,
            dialect: Dialect::Anthropic,
        }
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
}

impl GatewayError {
    /// 发给客户端的那句话。`[ThinkWatch]` 前缀不是装饰：没有它，用户看到
    /// 一个 401 会先去查上游的密钥 —— 而问题在中间这一层。
    fn client_message(&self) -> String {
        format!("[ThinkWatch] {}", self.detail.text)
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
        tw_dialect::convert::error_body(
            self.dialect,
            self.source.status().as_u16(),
            &self.client_message(),
        )
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
