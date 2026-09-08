//! 端到端：起一个假上游，起网关，用 Claude Code 会发的那种请求打过去。
//!
//! 这些测试存在的理由很具体 —— M0 的验收标准是「Claude Code 指向本地能
//! 正常干活，含流式和工具调用」。单元测试证明不了这件事，因为它的失败
//! 模式全在接缝上：头被剔错、body 被动过、流被缓冲。

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use tw_config::{Client, Config, Listen, Provider};

/// 假上游收到的东西，测试拿它来断言。
#[derive(Default, Debug)]
struct Seen {
    headers: HeaderMap,
    body: Vec<u8>,
}

async fn start_upstream(sse: bool) -> (SocketAddr, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let s = seen.clone();
    let app = Router::new()
        .route(
            "/v1/messages",
            post(move |State(s): State<Arc<Mutex<Seen>>>, headers: HeaderMap, body: bytes::Bytes| async move {
                {
                    let mut g = s.lock().unwrap();
                    g.headers = headers;
                    g.body = body.to_vec();
                }
                if sse {
                    // 分三帧发，这样「有没有被整块缓冲」是可观测的。
                    axum::response::Response::builder()
                        .header("content-type", "text/event-stream")
                        .header("anthropic-ratelimit-unified-5h-utilization", "0.62")
                        .body(axum::body::Body::from(
                            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n\
                             event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\n\
                             event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                        ))
                        .unwrap()
                } else {
                    axum::response::Response::builder()
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(r#"{"type":"message","id":"msg_01"}"#))
                        .unwrap()
                }
            }),
        )
        .with_state(s);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, seen)
}

async fn start_gateway(upstream: SocketAddr) -> SocketAddr {
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "claude-code".into(),
            key: "tw-testkey".into(),
        }],
        providers: vec![Provider {
            name: "mock".into(),
            base_url: format!("http://{upstream}"),
            key: "sk-upstream-secret".into(),
            protocol: Some(tw_config::Protocol::Anthropic),
        }],
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, tw_gateway::router(state)).await.unwrap() });
    addr
}

/// Claude Code 真会发的那种 body，带工具定义。
const CLAUDE_BODY: &str = r#"{"model":"claude-sonnet-4-5","max_tokens":1024,"messages":[{"role":"user","content":"hi"}],"tools":[{"name":"Read","input_schema":{"type":"object","properties":{"file_path":{"type":"string"}}}}],"stream":true}"#;

#[tokio::test]
async fn a_claude_code_request_reaches_the_upstream_byte_for_byte() {
    let (up, seen) = start_upstream(false).await;
    let gw = start_gateway(up).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-testkey")
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(CLAUDE_BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let g = seen.lock().unwrap();
    // 出站直通：请求体一个字节都不能变。工具定义、schema、嵌套对象，
    // 任何重新序列化都可能改动它 —— 而那正是缓存杀手。
    assert_eq!(String::from_utf8_lossy(&g.body), CLAUDE_BODY);
    // 方言头必须转过去，否则上游不认
    assert_eq!(g.headers.get("anthropic-version").unwrap(), "2023-06-01");
}

#[tokio::test]
async fn the_gateway_key_is_swapped_for_the_real_one_and_never_leaks_upstream() {
    let (up, seen) = start_upstream(false).await;
    let gw = start_gateway(up).await;

    reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-testkey")
        .body(CLAUDE_BODY)
        .send()
        .await
        .unwrap();

    let g = seen.lock().unwrap();
    // 上游拿到的是它自己的 key
    assert_eq!(g.headers.get("x-api-key").unwrap(), "sk-upstream-secret");
    // 我们的网关密钥一个字节都不能出现在发给中转站的请求里
    let all: String = g
        .headers
        .iter()
        .map(|(k, v)| format!("{k}:{}", v.to_str().unwrap_or("")))
        .collect();
    assert!(!all.contains("tw-testkey"), "网关密钥泄漏给了上游：{all}");
}

#[tokio::test]
async fn streaming_is_not_buffered_and_upstream_headers_survive() {
    let (up, _) = start_upstream(true).await;
    let gw = start_gateway(up).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-testkey")
        .body(CLAUDE_BODY)
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    // 订阅额度的头必须原样回来 —— 那是 §4.3.2 零成本白捡的数据来源，
    // 剔掉它等于把那个功能的输入掐了。
    assert_eq!(
        resp.headers()
            .get("anthropic-ratelimit-unified-5h-utilization")
            .unwrap(),
        "0.62"
    );
    // content-length 不能跟着来：我们重新分块了，它对不上
    assert!(resp.headers().get("content-length").is_none());

    let text = resp.text().await.unwrap();
    assert!(text.contains("message_start"));
    assert!(text.contains("message_stop"));
}

#[tokio::test]
async fn a_request_without_a_key_is_refused_before_anything_reaches_the_upstream() {
    let (up, seen) = start_upstream(false).await;
    let gw = start_gateway(up).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .body(CLAUDE_BODY)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
    assert_eq!(resp.headers().get("x-thinkwatch-error").unwrap(), "auth");
    let body: serde_json::Value = resp.json().await.unwrap();
    // 客户端解析的是自己方言的错误结构
    assert_eq!(body["error"]["type"], "authentication_error");
    // 一眼看出是哪一层拒的
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("[ThinkWatch]")
    );
    // 而且上游根本没被打扰
    assert!(seen.lock().unwrap().body.is_empty());
}

#[tokio::test]
async fn a_wrong_key_is_refused_and_the_upstream_key_is_not_burned() {
    let (up, seen) = start_upstream(false).await;
    let gw = start_gateway(up).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-wrong")
        .body(CLAUDE_BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    // 一个探测本机端口的脚本不该消耗掉用户的额度（§5.4）
    assert!(seen.lock().unwrap().body.is_empty());
}

#[tokio::test]
async fn healthz_needs_no_key() {
    let (up, _) = start_upstream(false).await;
    let gw = start_gateway(up).await;
    let r = reqwest::get(format!("http://{gw}/healthz")).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "ok");
}
