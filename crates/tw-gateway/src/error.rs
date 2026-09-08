//! 错误契约（DESIGN.md §4.6.1）：**客户端看到的必须是它自己方言的错误
//! 结构**，而不是我们的。一个 Anthropic 客户端收到 OpenAI 形状的错误
//! body，会在解析时炸掉，然后报一个和真实原因无关的错。
//!
//! 另外每个错误都带 `x-thinkwatch-error` 头和 `[ThinkWatch]` 前缀 ——
//! 让人一眼看出这一层是谁，而不是去怀疑上游。

use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

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
}

impl Source {
    fn slug(&self) -> &'static str {
        match self {
            Source::Auth => "auth",
            Source::Config => "config",
            Source::Upstream => "upstream",
            Source::Request => "request",
        }
    }
    fn status(&self) -> StatusCode {
        match self {
            Source::Auth => StatusCode::UNAUTHORIZED,
            Source::Config => StatusCode::INTERNAL_SERVER_ERROR,
            Source::Upstream => StatusCode::BAD_GATEWAY,
            Source::Request => StatusCode::BAD_REQUEST,
        }
    }
    /// Anthropic 的 error.type 词表。
    fn anthropic_type(&self) -> &'static str {
        match self {
            Source::Auth => "authentication_error",
            Source::Config => "api_error",
            Source::Upstream => "api_error",
            Source::Request => "invalid_request_error",
        }
    }
}

#[derive(Debug)]
pub struct GatewayError {
    pub source: Source,
    pub message: String,
}

impl GatewayError {
    pub fn new(source: Source, message: impl Into<String>) -> Self {
        Self {
            source,
            message: message.into(),
        }
    }
    pub fn auth(m: impl Into<String>) -> Self {
        Self::new(Source::Auth, m)
    }
    pub fn config(m: impl Into<String>) -> Self {
        Self::new(Source::Config, m)
    }
    pub fn upstream(m: impl Into<String>) -> Self {
        Self::new(Source::Upstream, m)
    }
    pub fn request(m: impl Into<String>) -> Self {
        Self::new(Source::Request, m)
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        // `[ThinkWatch]` 前缀不是装饰。没有它，用户看到一个 401 会先去
        // 查上游的密钥 —— 而问题在中间这一层。
        let body = serde_json::json!({
            "type": "error",
            "error": {
                "type": self.source.anthropic_type(),
                "message": format!("[ThinkWatch] {}", self.message),
            }
        });
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
        let (status, slug, json) = body_of(GatewayError::auth("密钥不对")).await;
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
        let (status, slug, _) = body_of(GatewayError::upstream("连不上")).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(slug, "upstream");
    }

    #[tokio::test]
    async fn each_source_maps_to_its_own_status_and_slug() {
        for (e, want_status, want_slug) in [
            (
                GatewayError::config("x"),
                StatusCode::INTERNAL_SERVER_ERROR,
                "config",
            ),
            (
                GatewayError::request("x"),
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
