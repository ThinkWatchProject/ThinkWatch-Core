//! 错误契约：**客户端看到的必须是它自己方言的错误
//! 结构**，而不是我们的。一个 Anthropic 客户端收到 OpenAI 形状的错误
//! body，会在解析时炸掉，然后报一个和真实原因无关的错。
//!
//! 另外每个错误都带 `x-thinkwatch-error` 头和 `[ThinkWatch]` 前缀 ——
//! 让人一眼看出这一层是谁，而不是去怀疑上游。

use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
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
    /// 我们这一层排不下了。**这是唯一一个我们主动拒绝的场景**，
    /// 而它的存在是为了防止队列撑爆内存。
    Overloaded,
    /// 上游说「慢点」。**和 `Overloaded` 分开**：那是我们自己的队列满了，
    /// 这是对面的额度到顶了。回 502 的话客户端会当成「服务器坏了」而不是
    /// 「该退避了」，而它们该做的事完全不同。
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
            Source::Overloaded => "overloaded",
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
            Source::Overloaded => StatusCode::TOO_MANY_REQUESTS,
            Source::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Source::Denied => StatusCode::FORBIDDEN,
        }
    }
    /// Anthropic 的 error.type 词表。
    fn anthropic_type(&self) -> &'static str {
        match self {
            Source::Auth => "authentication_error",
            Source::Config => "api_error",
            Source::Upstream => "api_error",
            Source::Request => "invalid_request_error",
            Source::Overloaded | Source::RateLimited => "rate_limit_error",
            Source::Denied => "permission_error",
        }
    }

    /// OpenAI 的 `error.type` 词表。**和 Anthropic 的不是一套词** ——
    /// 直接把 `authentication_error` 塞进 OpenAI 形状里，客户端的错误
    /// 分支会全部走空。
    fn openai_type(&self) -> &'static str {
        match self {
            Source::Auth => "invalid_request_error",
            Source::Config | Source::Upstream => "server_error",
            Source::Request => "invalid_request_error",
            Source::Overloaded | Source::RateLimited => "rate_limit_exceeded",
            Source::Denied => "invalid_request_error",
        }
    }

    /// Google 的 `status`。
    fn google_status(&self) -> &'static str {
        match self {
            Source::Auth => "UNAUTHENTICATED",
            Source::Config | Source::Upstream => "UNAVAILABLE",
            Source::Request => "INVALID_ARGUMENT",
            Source::Overloaded | Source::RateLimited => "RESOURCE_EXHAUSTED",
            Source::Denied => "PERMISSION_DENIED",
        }
    }
}

/// 入站方言。**错误体要用它的原生形状** —— 一个 Anthropic 客户端收到
/// OpenAI 形状的 error body，会在解析时炸掉，然后报一个和真实原因完全
/// 无关的错。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dialect {
    /// 猜不出时的默认。桌面版的主用例是 Claude Code
    #[default]
    Anthropic,
    Openai,
    Gemini,
}

impl Dialect {
    /// 从客户端把 key 放在哪儿推断。**这是我们唯一可靠的线索** ——
    /// 路径和 UA 都可以被中间层改写，而 key 的位置是 SDK 自己决定的。
    pub fn from_key_position(p: crate::auth::KeyPosition) -> Self {
        match p {
            crate::auth::KeyPosition::AnthropicHeader => Dialect::Anthropic,
            crate::auth::KeyPosition::GoogleHeader => Dialect::Gemini,
            crate::auth::KeyPosition::Bearer => Dialect::Openai,
        }
    }
}

#[derive(Debug)]
pub struct GatewayError {
    pub source: Source,
    /// 为什么失败。**发给 AI 客户端的是 `detail.text`**（它们只认字符串），
    /// 界面拿 `detail.code` 去自己的词表里找句子。见 [`tw_types::Msg`]。
    pub detail: Msg,
    /// 用哪种方言的形状回。**认证失败时还不知道方言**（key 就是没认出
    /// 来），所以它有默认值而不是必填。
    pub dialect: Dialect,
}

impl GatewayError {
    pub fn new(source: Source, detail: Msg) -> Self {
        Self {
            source,
            detail,
            dialect: Dialect::default(),
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
    /// 认出客户端之后补上方言。**忘了调只会退回 Anthropic 形状**，
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
    /// 流中途断掉时，唯一还能说话的地方是流本身。
    ///
    /// 首字节已经发出去了，状态码和响应头都改不了 —— 什么都不做的话，
    /// 客户端看到的是一个**戛然而止的流**，而截断和「答完了」在 SSE
    /// 里长得一模一样。
    pub fn sse_frame(&self) -> String {
        let msg = format!("[ThinkWatch] {}", self.detail.text);
        let data = match self.dialect {
            Dialect::Anthropic => serde_json::json!({
                "type": "error",
                "error": { "type": self.source.anthropic_type(), "message": msg },
            }),
            Dialect::Openai => serde_json::json!({
                "error": {
                    "message": msg,
                    "type": self.source.openai_type(),
                    "code": self.source.slug(),
                }
            }),
            Dialect::Gemini => serde_json::json!({
                "error": {
                    "code": self.source.status().as_u16(),
                    "message": msg,
                    "status": self.source.google_status(),
                }
            }),
        };
        format!("event: error\ndata: {data}\n\n")
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        // `[ThinkWatch]` 前缀不是装饰。没有它，用户看到一个 401 会先去
        // 查上游的密钥 —— 而问题在中间这一层。
        let msg = format!("[ThinkWatch] {}", self.detail.text);
        let body = match self.dialect {
            Dialect::Anthropic => serde_json::json!({
                "type": "error",
                "error": { "type": self.source.anthropic_type(), "message": msg },
            }),
            // OpenAI 没有外层的 `type`，而 `param` / `code` 是它自己那套
            Dialect::Openai => serde_json::json!({
                "error": {
                    "message": msg,
                    "type": self.source.openai_type(),
                    "param": serde_json::Value::Null,
                    "code": self.source.slug(),
                }
            }),
            Dialect::Gemini => serde_json::json!({
                "error": {
                    "code": self.source.status().as_u16(),
                    "message": msg,
                    "status": self.source.google_status(),
                }
            }),
        };
        let mut resp = (self.source.status(), axum::Json(body)).into_response();
        resp.headers_mut().insert(
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
}
