//! provider 抽象与各家实现。
//!
//! DTO 和 `CallCtx` 在 `tw-types`；这里只有「怎么把一个请求发出去」。

use futures::Stream;
use std::pin::Pin;

use tw_types::{
    CallCtx, ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, GatewayError,
    parse_retry_after_seconds, substitute_template,
};

pub use tw_types::*;

pub mod providers;

pub use providers::protocol::UpstreamProtocol;

/// Shared base for all AI providers. Holds the HTTP client, base URL,
/// and custom header templates. Previously each provider duplicated
/// these three fields and the identical `new()`, `with_custom_headers()`,
/// and `resolve_headers()` methods.
pub struct ProviderBase {
    pub base_url: String,
    pub client: reqwest::Client,
    pub custom_headers: Vec<(String, String)>,
}

impl ProviderBase {
    pub fn new(base_url: String) -> Self {
        // Wall-clock bounds on upstream HTTP. Without these a hung
        // upstream pins a connection forever; failover only retries
        // across routes, not within a stuck attempt. The 5-min total
        // is generous enough for slow LLM completions but cuts off
        // truly stuck calls; the 10s connect timeout is short because
        // a healthy upstream resolves and TCPs in well under that.
        // SSRF defense: don't follow upstream redirects on the LLM
        // gateway hot path. `base_url` is admin-supplied; a malicious
        // (or compromised) provider returning `302 Location: http://
        // 169.254.169.254/...` would silently steer traffic into
        // internal infra. Same reasoning as the MCP pool client and
        // the shared http_client in init.rs.
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(300))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client builder cannot fail on stable inputs");
        Self {
            // Normalize here, once, for every provider: each one builds
            // its endpoint as `format!("{base_url}/v1/…")`, so an
            // admin-entered trailing slash (the browser hands you one
            // for free when you paste a URL) produced `https://host//v1/
            // chat/completions` and a bare 404 from upstream. The
            // "Test connection" probe trimmed already, which is why a
            // provider could pass its check and still fail every real
            // inference call.
            base_url: base_url.trim_end_matches('/').to_string(),
            client,
            custom_headers: Vec::new(),
        }
    }

    pub fn with_custom_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.custom_headers = headers;
        self
    }

    /// Resolve `{{...}}` template variables in custom header values from
    /// `ctx.attrs`. A placeholder whose key is absent resolves to the
    /// empty string rather than being left literal — an upstream that
    /// receives `X-User: {{user_id}}` is worse than one that receives
    /// `X-User:`, because the literal looks like a working config.
    pub fn resolve_headers(&self, ctx: &CallCtx) -> Vec<(String, String)> {
        self.custom_headers
            .iter()
            .map(|(k, v)| (k.clone(), substitute_template(v, &ctx.attrs)))
            .collect()
    }

    /// Append the caller-resolved custom headers to a `RequestBuilder`.
    /// Centralizes what would otherwise be duplicated in every
    /// provider's `chat_completion` and `stream_chat_completion`.
    /// Also injects `x-trace-id` from `ctx` so the upstream log line and
    /// the gateway log line share a correlation id (OBS-01).
    pub fn apply_custom_headers(
        &self,
        builder: reqwest::RequestBuilder,
        ctx: &CallCtx,
    ) -> reqwest::RequestBuilder {
        let mut builder = Self::apply_headers(builder, &self.resolve_headers(ctx));
        if let Some(ref trace_id) = ctx.trace_id {
            builder = builder.header("x-trace-id", trace_id.as_str());
        }
        builder
    }

    /// Append a pre-resolved header list to a `RequestBuilder`.
    /// Streaming providers resolve headers before spawning the
    /// `async_stream!` block (since `&self` can't cross the `'static`
    /// boundary) and call this from inside the stream.
    pub fn apply_headers(
        mut builder: reqwest::RequestBuilder,
        headers: &[(String, String)],
    ) -> reqwest::RequestBuilder {
        for (k, v) in headers {
            builder = builder.header(k, v);
        }
        builder
    }

    /// Validate an upstream response status and translate non-2xx
    /// outcomes into the canonical `GatewayError` variants
    /// (`UpstreamRateLimited` / `UpstreamAuthError` / `ProviderError`).
    /// On a 2xx response the response is returned unchanged so the
    /// caller can continue parsing the body.
    ///
    /// `provider_label` appears in the user-visible error message, so
    /// each provider passes its own friendly name (e.g. "OpenAI",
    /// "Anthropic", "Bedrock").
    pub async fn check_status(
        resp: reqwest::Response,
        provider_label: &str,
    ) -> Result<reqwest::Response, GatewayError> {
        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            // Snag the upstream's own `Retry-After` (if it sent one)
            // so we can echo it to our client; without this, a client
            // with naive 3× retry policies just hammers the upstream
            // through the same quota window — observed in the field.
            let retry_after_secs = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after_seconds);
            return Err(GatewayError::UpstreamRateLimited { retry_after_secs });
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(GatewayError::UpstreamAuthError);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            // Upstream error bodies sometimes carry sensitive operational
            // detail (internal stack traces on 500, AWS request-ids /
            // account-ids on Bedrock, full debug strings on Vertex AI).
            // Previously we forwarded the body verbatim into the client-
            // facing JSON `error.message` field, turning the gateway into
            // a leak vector for whatever the upstream chose to surface.
            //
            // Log the full body server-side at WARN so operators can still
            // debug, but truncate the client-facing copy to a length that
            // captures the standard `{"error": {"message": "...", ...}}`
            // shape from OpenAI / Anthropic / Gemini without leaking
            // multi-page debug payloads.
            const CLIENT_MAX: usize = 512;
            tracing::warn!(
                provider = provider_label,
                status = %status,
                body = %body,
                "upstream provider returned non-2xx"
            );
            let truncated = if body.len() > CLIENT_MAX {
                // Char-boundary safe truncation — `body.split_at(N)`
                // would panic if N lands inside a multibyte UTF-8
                // sequence, and provider errors regularly include
                // non-ASCII (i18n'd messages from Bedrock, Chinese
                // model-name errors from Tongyi, etc.).
                let mut end = CLIENT_MAX;
                while end > 0 && !body.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}…[truncated]", &body[..end])
            } else {
                body
            };
            return Err(GatewayError::ProviderError(format!(
                "{provider_label} returned {status}: {truncated}"
            )));
        }
        Ok(resp)
    }

    /// Wrap `RequestBuilder::send` to map transport-level failures into
    /// `GatewayError::NetworkError` so callers don't have to repeat the
    /// `.map_err(|e| GatewayError::NetworkError(e.to_string()))?` line.
    pub async fn send(builder: reqwest::RequestBuilder) -> Result<reqwest::Response, GatewayError> {
        builder
            .send()
            .await
            .map_err(|e| GatewayError::NetworkError(e.to_string()))
    }
}

pub trait AiProvider: Send + Sync {
    fn name(&self) -> &str;

    fn chat_completion(
        &self,
        request: ChatCompletionRequest,
        ctx: CallCtx,
    ) -> impl std::future::Future<Output = Result<ChatCompletionResponse, GatewayError>> + Send;

    /// `ctx` is taken by value, like `request`: the returned stream is
    /// `'static` and outlives any borrow the caller could lend us.
    fn stream_chat_completion(
        &self,
        request: ChatCompletionRequest,
        ctx: CallCtx,
    ) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>>;
}

/// Dyn-compatible version of `AiProvider` that boxes the future returned by
/// `chat_completion`. This is necessary because `AiProvider::chat_completion`
/// uses `impl Future` (RPITIT) which is not dyn-compatible.
///
/// All types implementing `AiProvider` automatically implement `DynAiProvider`.
pub trait DynAiProvider: Send + Sync {
    fn name(&self) -> &str;

    fn chat_completion_boxed(
        &self,
        request: ChatCompletionRequest,
        ctx: CallCtx,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<ChatCompletionResponse, GatewayError>>
                + Send
                + '_,
        >,
    >;

    fn stream_chat_completion(
        &self,
        request: ChatCompletionRequest,
        ctx: CallCtx,
    ) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>>;
}

impl<T: AiProvider> DynAiProvider for T {
    fn name(&self) -> &str {
        AiProvider::name(self)
    }

    fn chat_completion_boxed(
        &self,
        request: ChatCompletionRequest,
        ctx: CallCtx,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<ChatCompletionResponse, GatewayError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(AiProvider::chat_completion(self, request, ctx))
    }

    fn stream_chat_completion(
        &self,
        request: ChatCompletionRequest,
        ctx: CallCtx,
    ) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>> {
        AiProvider::stream_chat_completion(self, request, ctx)
    }
}
