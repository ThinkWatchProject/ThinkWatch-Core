//! 压缩过的请求体和上游回答，端到端（见 `tw_gateway::inflate`、`server::intake`）。
//!
//! - 客户端压缩着发（`Content-Encoding: gzip` / `zstd`）：直通和转换两条路，上游收到的都是
//!   解开的请求，不带 `Content-Encoding`
//! - 压坏了的是 400，按客户端的格式回，上游什么都收不到
//! - 上游压缩着回（整包和 SSE）：客户端收到的是解开的，不带 `Content-Encoding`；流式的
//!   一条一条到，不等整个流结束
//!
//! 解开之后超限的在 `server::intake` 的单元测试里（上限是 256 MiB，端到端要真解出那么多）。

use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{OriginalUri, State};
use axum::http::HeaderMap;
use futures::StreamExt;
use serde_json::{Value, json};
use tw_config::{Client, Config, Listen, Protocol, Provider};

#[derive(Debug, Default, Clone)]
struct Seen {
    uri: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

/// 假上游怎么回
#[derive(Clone)]
enum Reply {
    /// 没压缩的整包
    Plain(&'static str),
    /// gzip 压缩的整包
    Gzip(&'static str),
    /// gzip 压缩的 SSE，每条事件一段、段与段之间停一会儿，像边生成边压缩那样
    GzipStream(&'static [&'static str]),
}

const ANSWER: &str = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[{"type":"text","text":"hello there"}],"stop_reason":"end_turn","usage":{"input_tokens":12,"output_tokens":3}}"#;

const STREAM: &[&str] = &[
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-5\",\"content\":[],\"usage\":{\"input_tokens\":12,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello there\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
];

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// 一个 gzip 流分几段：每段 flush 一次，拿到那一刻压出来的字节
fn gzip_frames(parts: &[&str]) -> Vec<Vec<u8>> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut frames = Vec::new();
    for p in parts {
        e.write_all(p.as_bytes()).unwrap();
        e.flush().unwrap();
        frames.push(std::mem::take(e.get_mut()));
    }
    frames.push(e.finish().unwrap());
    frames
}

async fn upstream(reply: Reply) -> (SocketAddr, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let app = Router::new()
        .fallback(
            move |State(s): State<Arc<Mutex<Seen>>>,
                  OriginalUri(uri): OriginalUri,
                  headers: HeaderMap,
                  body: bytes::Bytes| {
                let reply = reply.clone();
                async move {
                    *s.lock().unwrap() = Seen {
                        uri: uri.to_string(),
                        headers,
                        body: body.to_vec(),
                    };
                    let b = axum::response::Response::builder();
                    match reply {
                        Reply::Plain(text) => b
                            .header("content-type", "application/json")
                            .body(axum::body::Body::from(text))
                            .unwrap(),
                        Reply::Gzip(text) => {
                            let packed = gzip(text.as_bytes());
                            b.header("content-type", "application/json")
                                .header("content-encoding", "gzip")
                                .header("content-length", packed.len())
                                .body(axum::body::Body::from(packed))
                                .unwrap()
                        }
                        Reply::GzipStream(events) => {
                            let frames = gzip_frames(events);
                            let stream = async_stream::stream! {
                                for f in frames {
                                    yield Ok::<_, std::io::Error>(bytes::Bytes::from(f));
                                    tokio::time::sleep(Duration::from_millis(150)).await;
                                }
                            };
                            b.header("content-type", "text/event-stream")
                                .header("content-encoding", "gzip")
                                .body(axum::body::Body::from_stream(stream))
                                .unwrap()
                        }
                    }
                }
            },
        )
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, seen)
}

async fn gateway(up: SocketAddr) -> SocketAddr {
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "up".into(),
            base_url: format!("http://{up}"),
            key: Some("sk-upstream".into()),
            protocol: Some(Protocol::Anthropic),
            ..Default::default()
        }],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

/// 发一个请求体（已经按 `encoding` 压好的），交回响应。测试用的客户端不解压，看到的就是
/// 网关发出的字节
async fn post(
    gw: SocketAddr,
    path: &str,
    encoding: Option<&str>,
    body: Vec<u8>,
) -> reqwest::Response {
    let mut r = reqwest::Client::new()
        .post(format!("http://{gw}{path}"))
        .header("x-api-key", "tw-k")
        .header("content-type", "application/json");
    if let Some(e) = encoding {
        r = r.header("content-encoding", e);
    }
    r.body(body).send().await.unwrap()
}

fn claude_request() -> String {
    json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi there"}]
    })
    .to_string()
}

#[tokio::test]
async fn a_gzip_request_body_reaches_the_upstream_decoded_on_the_passthrough_route() {
    let (up, seen) = upstream(Reply::Plain(ANSWER)).await;
    let gw = gateway(up).await;
    let plain = claude_request();
    let r = post(gw, "/v1/messages", Some("gzip"), gzip(plain.as_bytes())).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());

    let s = seen.lock().unwrap().clone();
    assert_eq!(s.uri, "/v1/messages");
    assert_eq!(
        String::from_utf8_lossy(&s.body),
        plain,
        "上游收到的该是解开的、一个字节都没改的请求体"
    );
    assert!(
        s.headers.get("content-encoding").is_none(),
        "客户端的 Content-Encoding 不该到上游：{:?}",
        s.headers
    );
    assert_eq!(s.headers.get("content-type").unwrap(), "application/json");
}

#[tokio::test]
async fn a_zstd_request_body_on_a_converted_route_is_decoded_before_conversion() {
    let (up, seen) = upstream(Reply::Plain(ANSWER)).await;
    let gw = gateway(up).await;
    let chat = json!({
        "model": "claude-sonnet-4-5",
        "messages": [{"role": "system", "content": "简短"}, {"role": "user", "content": "hi there"}]
    })
    .to_string();
    let packed = zstd::stream::encode_all(chat.as_bytes(), 3).unwrap();
    let r = post(gw, "/v1/chat/completions", Some("zstd"), packed).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());

    let s = seen.lock().unwrap().clone();
    assert_eq!(s.uri, "/v1/messages", "转换过的请求发到 Anthropic 的路径");
    assert!(
        s.headers.get("content-encoding").is_none(),
        "{:?}",
        s.headers
    );
    let sent: Value = serde_json::from_slice(&s.body).expect("上游收到的该是 JSON");
    assert_eq!(sent["system"][0]["text"], "简短");
    assert_eq!(sent["messages"][0]["content"][0]["text"], "hi there");
}

#[tokio::test]
async fn a_compressed_count_tokens_request_is_decoded_too() {
    let (up, seen) = upstream(Reply::Plain(r#"{"input_tokens":12}"#)).await;
    let gw = gateway(up).await;
    let plain = claude_request();
    let r = post(
        gw,
        "/v1/messages/count_tokens",
        Some("gzip"),
        gzip(plain.as_bytes()),
    )
    .await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let s = seen.lock().unwrap().clone();
    assert_eq!(s.uri, "/v1/messages/count_tokens");
    assert_eq!(String::from_utf8_lossy(&s.body), plain);
    assert!(s.headers.get("content-encoding").is_none());
}

#[tokio::test]
async fn a_corrupt_compressed_body_is_a_400_in_the_clients_format() {
    let (up, seen) = upstream(Reply::Plain(ANSWER)).await;
    let gw = gateway(up).await;
    let r = post(
        gw,
        "/v1/messages",
        Some("gzip"),
        b"this is not gzip".to_vec(),
    )
    .await;
    assert_eq!(r.status(), 400);
    assert_eq!(r.headers().get("x-thinkwatch-error").unwrap(), "request");
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["type"], "error", "{body}");
    assert_eq!(body["error"]["type"], "invalid_request_error", "{body}");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("gzip"), "{message}");
    assert!(seen.lock().unwrap().body.is_empty(), "上游不该收到任何东西");
}

#[tokio::test]
async fn an_encoding_the_gateway_does_not_decode_is_refused() {
    let (up, seen) = upstream(Reply::Plain(ANSWER)).await;
    let gw = gateway(up).await;
    let r = post(gw, "/v1/messages", Some("br"), b"whatever".to_vec()).await;
    assert_eq!(r.status(), 400);
    let text = r.text().await.unwrap();
    assert!(text.contains("br"), "{text}");
    assert!(seen.lock().unwrap().body.is_empty());
}

#[tokio::test]
async fn an_upstream_that_answers_gzip_json_reaches_the_client_decoded() {
    let (up, _) = upstream(Reply::Gzip(ANSWER)).await;
    let gw = gateway(up).await;
    let r = post(gw, "/v1/messages", None, claude_request().into_bytes()).await;
    assert_eq!(r.status(), 200);
    assert!(
        r.headers().get("content-encoding").is_none(),
        "解开了就不能再说它压缩着：{:?}",
        r.headers()
    );
    assert_eq!(r.headers().get("content-type").unwrap(), "application/json");
    let body = r.bytes().await.unwrap();
    assert_eq!(String::from_utf8_lossy(&body), ANSWER);
}

#[tokio::test]
async fn an_upstream_that_streams_gzip_sse_reaches_the_client_decoded_event_by_event() {
    let (up, _) = upstream(Reply::GzipStream(STREAM)).await;
    let gw = gateway(up).await;
    let r = post(gw, "/v1/messages", None, claude_request().into_bytes()).await;
    assert_eq!(r.status(), 200);
    assert!(
        r.headers().get("content-encoding").is_none(),
        "{:?}",
        r.headers()
    );
    assert_eq!(
        r.headers().get("content-type").unwrap(),
        "text/event-stream"
    );

    let mut chunks: Vec<(Duration, bytes::Bytes)> = Vec::new();
    let started = std::time::Instant::now();
    let mut s = r.bytes_stream();
    while let Some(c) = s.next().await {
        chunks.push((started.elapsed(), c.unwrap()));
    }
    let text: String = chunks
        .iter()
        .map(|(_, c)| String::from_utf8_lossy(c).into_owned())
        .collect();
    assert_eq!(text, STREAM.concat(), "解开的 SSE 该和上游压之前的一模一样");
    // 上游每隔 150 毫秒发一段；第一段到客户端的时候最后一段还没发出来。整个流攒到结束才
    // 交的话，第一块要等到 5 × 150 毫秒之后
    let (first, _) = &chunks[0];
    assert!(
        *first < Duration::from_millis(450),
        "第一条事件 {first:?} 才到：流被攒起来了"
    );
    assert!(chunks.len() >= 2, "{} 块", chunks.len());
}
