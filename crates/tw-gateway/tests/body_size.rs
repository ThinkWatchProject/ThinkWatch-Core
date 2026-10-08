//! 请求体的大小上限（Core#295）：网关只有自己那一道 256 MiB，体在鉴权之后才读。
//!
//! 原来体由 axum 的 `Bytes` 提取器读，它自带 2 MB 的默认上限 —— 贴了几张截图的对话
//! 一过 2 MiB 就被拒成 413，到不了网关自己的检查；而且体在来源、密钥检查之前就被
//! 整个读进内存。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::routing::post;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tw_config::{Client, Config, Listen, Provider};

/// 假上游。**它自己也得关掉 axum 的 2 MB 上限**，不然大请求过了网关、倒在这里
async fn start_upstream() -> (SocketAddr, Arc<Mutex<usize>>) {
    let got = Arc::new(Mutex::new(0usize));
    let app = Router::new()
        .route(
            "/v1/messages",
            post(
                |State(got): State<Arc<Mutex<usize>>>, body: bytes::Bytes| async move {
                    *got.lock().unwrap() = body.len();
                    axum::response::Response::builder()
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(
                            r#"{"type":"message","id":"msg_01"}"#,
                        ))
                        .unwrap()
                },
            ),
        )
        .layer(DefaultBodyLimit::disable())
        .with_state(got.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, got)
}

async fn start_gateway(upstream: SocketAddr) -> SocketAddr {
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "claude-code".into(),
            key: "tw-testkey".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "mock".into(),
            base_url: format!("http://{upstream}"),
            key: Some("sk-upstream".into()),
            protocol: Some(tw_config::Protocol::Anthropic),
            ..Default::default()
        }],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap()
}

/// 一个 `size` 字节、合法的 Anthropic 请求：一段很长的用户消息
fn body_of(size: usize) -> String {
    let head =
        r#"{"model":"claude-sonnet-4-5","max_tokens":16,"messages":[{"role":"user","content":""#;
    let tail = r#""}]}"#;
    format!("{head}{}{tail}", "x".repeat(size - head.len() - tail.len()))
}

#[tokio::test]
async fn a_request_over_axums_default_two_mib_reaches_the_upstream() {
    let (up, got) = start_upstream().await;
    let gw = start_gateway(up).await;
    let body = body_of(3 * 1024 * 1024);

    let resp = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-testkey")
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    assert_eq!(*got.lock().unwrap(), body.len());
}

/// 只发请求头、声明一个 `length` 字节的体、一个字节都不发，读回状态行和整个响应。
///
/// **回得来就说明网关没在等体**：体要是先读，这里会一直挂到超时
async fn headers_only(gw: SocketAddr, key: &str, length: u64) -> (u16, String) {
    let mut s = tokio::net::TcpStream::connect(gw).await.unwrap();
    let req = format!(
        "POST /v1/messages HTTP/1.1\r\nhost: {gw}\r\nx-api-key: {key}\r\n\
         anthropic-version: 2023-06-01\r\ncontent-type: application/json\r\n\
         content-length: {length}\r\nconnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out))
        .await
        .expect("网关在等请求体，没有先回答")
        .unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("不是 HTTP 响应：{text}"));
    (status, text)
}

#[tokio::test]
async fn a_request_with_a_wrong_key_is_refused_before_its_body_is_read() {
    let (up, _) = start_upstream().await;
    let gw = start_gateway(up).await;
    let (status, _) = headers_only(gw, "tw-wrong", 100 * 1024 * 1024).await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn a_declared_length_over_the_limit_is_refused_unread_in_the_clients_format() {
    let (up, got) = start_upstream().await;
    let gw = start_gateway(up).await;
    let (status, text) = headers_only(gw, "tw-testkey", 300 * 1024 * 1024).await;
    assert_eq!(status, 413);
    assert!(text.contains("request_too_large"), "{text}");
    assert!(text.contains("x-thinkwatch-error: request"), "{text}");
    assert_eq!(*got.lock().unwrap(), 0, "超限的请求不该发到上游");
}
