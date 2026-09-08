//! HTTP 服务：路由、身份识别、转发。

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{OriginalUri, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{any, get};
use bytes::Bytes;
use futures::TryStreamExt;

use crate::auth::{extract_key, key_eq};
use crate::error::GatewayError;
use crate::forward;

/// 256 MiB。大到能装下几张 4K 图的 base64（膨胀 33%），小到失控的
/// 客户端打不爆内存。
const MAX_BODY: usize = 256 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<tw_config::Config>,
    pub http: reqwest::Client,
}

impl AppState {
    pub fn new(config: tw_config::Config) -> Result<Self, GatewayError> {
        let http = reqwest::Client::builder()
            // 分段超时（§4.7）。**没有整体超时** —— 一个跑了六分钟的
            // Opus 任务不该被中间层掐断，让客户端自己决定何时放弃。
            .connect_timeout(std::time::Duration::from_secs(10))
            // 响应头超时覆盖不到 DNS 和 TCP 握手，所以上面那条必须显式
            // 设置：DNS 被污染解析到黑洞 IP 时，建连会等满内核重传。
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            // HTTP/2 的死连接探测。NAT / 代理会静默丢弃空闲连接，两端
            // 都以为还活着，下一个请求要等满内核重传（分钟级）。挂
            // Clash / Surge 的桌面用户几乎必踩，而默认是不发 PING 的。
            .http2_keep_alive_interval(std::time::Duration::from_secs(15))
            .http2_keep_alive_timeout(std::time::Duration::from_secs(15))
            .http2_keep_alive_while_idle(true)
            .build()
            .map_err(|e| GatewayError::config(format!("HTTP 客户端建不起来：{e}")))?;
        Ok(Self {
            config: Arc::new(config),
            http,
        })
    }

    /// 密钥 → 客户端名字。
    fn identify(&self, headers: &HeaderMap, query: Option<&str>) -> Result<String, GatewayError> {
        let Some(key) = extract_key(headers, query) else {
            return Err(GatewayError::auth(
                "请求没带网关密钥。把 config.yaml 里 clients 段的那把 key 配到客户端上。",
            ));
        };
        self.config
            .clients
            .iter()
            .find(|c| key_eq(&c.key, &key))
            .map(|c| c.name.clone())
            .ok_or_else(|| {
                // 不回显收到的 key，哪怕是打码的 —— 回显会让「猜密钥」
                // 这件事有了反馈信号。
                GatewayError::auth(
                    "网关密钥不认识。检查客户端配置里的 key 和 config.yaml 是否一致。",
                )
            })
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        // M0 只有透传：任何方法、任何路径都往上游送。M1 加路由时，
        // 这里会先过规则引擎再决定送给谁。
        .fallback(any(passthrough))
        .with_state(state)
}

async fn passthrough(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, GatewayError> {
    let client_name = state.identify(&headers, query.as_deref())?;
    forward::check_body_size(&body, MAX_BODY)?;

    // M0 没有路由，取第一个 provider。M1 会把这里换成规则引擎 ——
    // 接缝留在这一行。
    let provider = state
        .config
        .providers
        .first()
        .ok_or_else(|| GatewayError::config("配置里一个 provider 都没有"))?;

    let key = provider.resolved_key().map_err(|e| {
        GatewayError::config(format!("provider `{}` 的密钥展开失败：{e}", provider.name))
    })?;

    let url = forward::upstream_url(&provider.base_url, uri.path(), query.as_deref());
    let method = reqwest::Method::from_bytes(b"POST").expect("POST 是合法方法");

    tracing::debug!(
        client = %client_name,
        provider = %provider.name,
        url = %tw_secret::redact_url(&url),
        bytes = body.len(),
        "转发"
    );

    let mut req = state.http.request(method, &url);
    req = forward::forward_headers(req, &headers);
    req = forward::apply_credential(req, provider.effective_protocol(), &key);
    let upstream = req
        .body(body)
        .send()
        .await
        .map_err(forward::map_reqwest_error)?;

    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let out_headers = forward::response_headers(upstream.headers());

    // 流式：**不缓冲**。整块缓冲会把 SSE 变成一次性交付，客户端那边
    // 看起来就是「卡住很久然后一下全出来」。
    let stream = upstream.bytes_stream().map_err(std::io::Error::other);
    let mut resp = Response::new(Body::from_stream(stream));
    *resp.status_mut() = status;
    *resp.headers_mut() = out_headers;
    Ok(resp)
}

/// 起服务。返回实际绑定的地址 —— 端口写 0 时调用方需要知道拿到了哪个。
pub async fn serve(state: AppState, addr: std::net::SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let actual = listener.local_addr()?;
    tracing::info!(%actual, "网关已监听");
    axum::serve(listener, router(state)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tw_config::{Client, Config, Listen, Provider};

    fn cfg() -> Config {
        Config {
            version: 1,
            listen: Listen::default(),
            clients: vec![Client {
                name: "default".into(),
                key: "tw-good".into(),
            }],
            providers: vec![Provider {
                name: "r".into(),
                base_url: "https://example.invalid".into(),
                key: "sk-1".into(),
                protocol: None,
            }],
        }
    }

    fn hdr(k: &'static str, v: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert(k, axum::http::HeaderValue::from_str(v).unwrap());
        m
    }

    #[test]
    fn a_known_key_resolves_to_its_client_name() {
        let s = AppState::new(cfg()).unwrap();
        assert_eq!(
            s.identify(&hdr("x-api-key", "tw-good"), None).unwrap(),
            "default"
        );
    }

    #[test]
    fn no_key_is_rejected_and_the_message_says_where_to_put_one() {
        let s = AppState::new(cfg()).unwrap();
        let e = s.identify(&HeaderMap::new(), None).unwrap_err();
        assert_eq!(e.source, crate::error::Source::Auth);
        assert!(e.message.contains("clients"));
    }

    #[test]
    fn an_unknown_key_is_rejected_without_echoing_it_back() {
        // 回显会给「猜密钥」这件事一个反馈信号，哪怕只是打码的。
        let s = AppState::new(cfg()).unwrap();
        let e = s.identify(&hdr("x-api-key", "tw-wrong"), None).unwrap_err();
        assert!(!e.message.contains("tw-wrong"));
        assert!(!e.message.contains("tw-wr"));
    }

    #[test]
    fn the_key_can_arrive_in_any_of_the_four_positions() {
        let s = AppState::new(cfg()).unwrap();
        assert!(s.identify(&hdr("x-api-key", "tw-good"), None).is_ok());
        assert!(s.identify(&hdr("x-goog-api-key", "tw-good"), None).is_ok());
        assert!(
            s.identify(&hdr("authorization", "Bearer tw-good"), None)
                .is_ok()
        );
        assert!(s.identify(&HeaderMap::new(), Some("key=tw-good")).is_ok());
    }
}
