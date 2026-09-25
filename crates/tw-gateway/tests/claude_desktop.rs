//! Claude Desktop（第三方推理模式）和它内嵌的 Claude Code 对网关的要求，端到端。
//!
//! - 启动时的 `HEAD /api/hello` 预热：不要密钥，不打上游
//! - 推理请求打到 `/v1/messages?beta=true`，`anthropic-beta` 和 `cache_control` 原样到上游
//! - `/v1/messages/count_tokens` 照样转给 Anthropic 上游
//! - `/v1/models` 回 Anthropic 的列表格式，Claude 模型带上名字和档位
//! - 上游静默时，Anthropic 流里补 `ping`：客户端按字节计时，五分钟没有字节就放弃

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, Method};
use serde_json::{Value, json};
use tw_config::{Client, Config, Listen, Protocol, Provider};

#[derive(Default, Debug)]
struct Seen {
    method: Option<Method>,
    uri: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

/// 记下收到的请求、回一个 JSON 的假上游。
async fn recording_upstream() -> (SocketAddr, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let app = Router::new()
        .fallback(
            |State(s): State<Arc<Mutex<Seen>>>,
             method: Method,
             OriginalUri(uri): OriginalUri,
             headers: HeaderMap,
             body: bytes::Bytes| async move {
                *s.lock().unwrap() = Seen {
                    method: Some(method),
                    uri: uri.to_string(),
                    headers,
                    body: body.to_vec(),
                };
                axum::response::Response::builder()
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"input_tokens":3}"#))
                    .unwrap()
            },
        )
        .with_state(seen.clone());
    (serve(app).await, seen)
}

/// 按顺序发出 `parts` 的 SSE 上游。`None` 是一段静默。
async fn stalling_upstream(parts: Vec<Option<&'static str>>, pause: Duration) -> SocketAddr {
    let app = Router::new().fallback(move || {
        let parts = parts.clone();
        async move {
            let body = async_stream::stream! {
                for p in parts {
                    match p {
                        Some(text) => yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(text.as_bytes())),
                        None => tokio::time::sleep(pause).await,
                    }
                }
            };
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from_stream(body))
                .unwrap()
        }
    });
    serve(app).await
}

async fn serve(app: Router) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

fn provider(up: SocketAddr, protocol: Protocol, models: &[&str]) -> Provider {
    Provider {
        name: "up".into(),
        base_url: format!("http://{up}"),
        key: Some("sk-upstream".into()),
        protocol: Some(protocol),
        models: models.iter().map(|m| m.to_string()).collect(),
        ..Default::default()
    }
}

struct Gateway {
    addr: SocketAddr,
    events: tokio::sync::broadcast::Receiver<tw_api::Event>,
    bodies: tokio::sync::mpsc::Receiver<tw_gateway::bodies::BodyRecord>,
}

async fn gateway(p: Provider) -> Gateway {
    gateway_pinging_for(p, tw_gateway::PING_FOR).await
}

async fn gateway_pinging_for(p: Provider, ping_for: Duration) -> Gateway {
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "desktop".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![p],
        ..Default::default()
    };
    let mut state = tw_gateway::AppState::new(cfg).unwrap();
    // 心跳的间隔调短，一条测试不必干等十五秒
    state.ping_every = Duration::from_millis(100);
    state.ping_for = ping_for;
    let events = state.bus.subscribe();
    let (tx, bodies) = tokio::sync::mpsc::channel(16);
    state.set_body_sink(tx);
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    Gateway {
        addr,
        events,
        bodies,
    }
}

#[tokio::test]
async fn the_startup_probe_is_answered_without_a_key_or_the_upstream() {
    let (up, seen) = recording_upstream().await;
    let gw = gateway(provider(up, Protocol::Anthropic, &[])).await;
    let http = reqwest::Client::new();
    for method in [Method::HEAD, Method::GET] {
        let resp = http
            .request(method.clone(), format!("http://{}/api/hello", gw.addr))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{method}");
    }
    assert!(
        seen.lock().unwrap().method.is_none(),
        "预热探测被转给了上游"
    );
}

#[tokio::test]
async fn a_desktop_request_reaches_the_upstream_as_it_was_sent() {
    let (up, seen) = recording_upstream().await;
    let gw = gateway(provider(up, Protocol::Anthropic, &["claude-sonnet-4-5"])).await;
    let body = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 16,
        "system": [{"type": "text", "text": "be brief", "cache_control": {"type": "ephemeral"}}],
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
        ]}],
    });
    let beta = "extended-cache-ttl-2025-04-11,some-future-beta-2099-01-01";
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/messages?beta=true", gw.addr))
        .header("authorization", "Bearer tw-k")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", beta)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    let g = seen.lock().unwrap();
    assert_eq!(g.uri, "/v1/messages?beta=true");
    assert_eq!(g.headers.get("anthropic-beta").unwrap(), beta);
    assert_eq!(g.headers.get("anthropic-version").unwrap(), "2023-06-01");
    let sent: Value = serde_json::from_slice(&g.body).unwrap();
    assert_eq!(sent["system"], body["system"], "cache_control 被动过");
    assert_eq!(sent["messages"], body["messages"], "cache_control 被动过");
}

#[tokio::test]
async fn counting_tokens_goes_to_the_anthropic_upstream() {
    let (up, seen) = recording_upstream().await;
    let gw = gateway(provider(up, Protocol::Anthropic, &["claude-sonnet-4-5"])).await;
    let resp = reqwest::Client::new()
        .post(format!(
            "http://{}/v1/messages/count_tokens?beta=true",
            gw.addr
        ))
        .header("x-api-key", "tw-k")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "token-counting-2024-11-01")
        .json(
            &json!({"model": "claude-sonnet-4-5", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["input_tokens"], 3);
    let g = seen.lock().unwrap();
    assert_eq!(g.uri, "/v1/messages/count_tokens?beta=true");
    assert_eq!(
        g.headers.get("anthropic-beta").unwrap(),
        "token-counting-2024-11-01"
    );
}

#[tokio::test]
async fn models_are_listed_in_the_anthropic_shape() {
    let (up, _) = recording_upstream().await;
    let gw = gateway(provider(
        up,
        Protocol::Anthropic,
        &["claude-sonnet-4-5-20250929", "my-alias"],
    ))
    .await;
    let get = |path: &'static str| {
        reqwest::Client::new()
            .get(format!("http://{}{path}", gw.addr))
            .header("x-api-key", "tw-k")
            .header("anthropic-version", "2023-06-01")
            .send()
    };
    let list: Value = get("/v1/models?limit=1000")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["has_more"], false);
    assert_eq!(list["first_id"], "claude-sonnet-4-5-20250929");
    assert_eq!(list["last_id"], "my-alias");
    let data = list["data"].as_array().unwrap();
    assert_eq!(data.len(), 2, "{list}");
    let claude = &data[0];
    assert_eq!(claude["type"], "model");
    assert_eq!(claude["id"], "claude-sonnet-4-5-20250929");
    assert_eq!(claude["display_name"], "Claude Sonnet 4.5");
    assert_eq!(claude["anthropic_family_tier"], "sonnet");
    assert!(
        chrono::DateTime::parse_from_rfc3339(claude["created_at"].as_str().unwrap()).is_ok(),
        "{claude}"
    );
    // OpenAI 那几个字段照样在
    assert_eq!(claude["object"], "model");
    assert!(claude["created"].is_u64());
    // 看不出是 Claude 的：名字就是 ID，也不标档位
    let alias = &data[1];
    assert_eq!(alias["display_name"], "my-alias");
    assert!(alias.get("anthropic_family_tier").is_none(), "{alias}");

    let one: Value = get("/v1/models/claude-sonnet-4-5-20250929")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(one, *claude, "单点查询和列表里的应该是同一个对象");
}

#[tokio::test]
async fn an_openai_client_still_gets_the_openai_listing() {
    let (up, _) = recording_upstream().await;
    let gw = gateway(provider(up, Protocol::Anthropic, &["claude-sonnet-4-5"])).await;
    let list: Value = reqwest::Client::new()
        .get(format!("http://{}/v1/models", gw.addr))
        .header("authorization", "Bearer tw-k")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["object"], "list");
    let m = &list["data"][0];
    assert_eq!(m["id"], "claude-sonnet-4-5");
    assert!(m.get("type").is_none(), "{m}");
    assert!(list.get("has_more").is_none(), "{list}");
}

const CHAT_FIRST: &str = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-x\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hel\"}}]}\n\n";
const CHAT_REST: &str = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\n\
data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-x\",\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n\
data: [DONE]\n\n";

async fn stream_from(gw: &Gateway, path: &str, body: Value) -> String {
    let resp = reqwest::Client::new()
        .post(format!("http://{}{path}", gw.addr))
        .header("authorization", "Bearer tw-k")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.text().await.unwrap()
}

#[tokio::test]
async fn a_silent_converted_upstream_is_covered_with_pings() {
    let up = stalling_upstream(
        vec![Some(CHAT_FIRST), None, Some(CHAT_REST)],
        Duration::from_millis(700),
    )
    .await;
    let mut gw = gateway(provider(up, Protocol::OpenaiChat, &[])).await;
    let text = stream_from(
        &gw,
        "/v1/messages",
        json!({"model": "gpt-x", "max_tokens": 16, "stream": true,
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    let ping = "event: ping\ndata: {\"type\": \"ping\"}\n\n";
    assert!(text.matches(ping).count() >= 2, "{text}");
    // 心跳插在帧之间：去掉它们，剩下的是一条完整的 Anthropic 流
    let rest = text.replace(ping, "");
    assert!(rest.starts_with("event: message_start"), "{rest}");
    assert!(rest.contains("\"text\":\"hel\""), "{rest}");
    assert!(rest.contains("\"text\":\"lo\""), "{rest}");
    assert!(
        rest.trim_end().ends_with("\"type\":\"message_stop\"}"),
        "{rest}"
    );
    assert!(!rest.contains("ping"), "心跳拆进了别的帧：{rest}");

    // 用量照上游报的记；请求记录里存的是上游的原话，没有心跳
    let usage = loop {
        match gw.events.recv().await.unwrap() {
            tw_api::Event::RequestFinished { usage, .. } => break usage,
            tw_api::Event::RequestFailed { message, .. } => panic!("{message:?}"),
            _ => {}
        }
    };
    let usage = usage.expect("上游报了用量");
    assert_eq!((usage.input, usage.output), (5, 2));
    let mut recorded = String::new();
    while let Ok(Some(rec)) =
        tokio::time::timeout(Duration::from_millis(200), gw.bodies.recv()).await
    {
        recorded.push_str(&String::from_utf8_lossy(&rec.body));
    }
    assert!(recorded.contains("\"content\":\"lo\""), "{recorded}");
    assert!(!recorded.contains("ping"), "心跳进了请求记录：{recorded}");
}

/// 上游在排队时只发 SSE 注释（DeepSeek 的 `: keep-alive`）。转换时注释被丢掉，
/// 客户端什么都收不到 —— **心跳要看客户端那一边的静默**，不能被上游的注释推迟
#[tokio::test]
async fn upstream_comments_that_never_reach_the_client_do_not_hold_pings_back() {
    let mut parts = vec![Some(CHAT_FIRST)];
    for _ in 0..14 {
        parts.push(Some(": keep-alive\n\n"));
        parts.push(None);
    }
    parts.push(Some(CHAT_REST));
    let up = stalling_upstream(parts, Duration::from_millis(50)).await;
    let gw = gateway(provider(up, Protocol::OpenaiChat, &[])).await;
    let text = stream_from(
        &gw,
        "/v1/messages",
        json!({"model": "gpt-x", "max_tokens": 16, "stream": true,
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    let ping = "event: ping\ndata: {\"type\": \"ping\"}\n\n";
    assert!(text.matches(ping).count() >= 2, "{text}");
    let rest = text.replace(ping, "");
    assert!(!rest.contains("keep-alive"), "{rest}");
    assert!(
        rest.trim_end().ends_with("\"type\":\"message_stop\"}"),
        "{rest}"
    );
}

/// 上游一个字节都不发太久（连接半开了）：**不再补心跳**，让客户端自己的静默计时断开它。
/// 一直补的话这个请求永远挂着
#[tokio::test]
async fn pings_stop_once_the_upstream_has_been_silent_too_long() {
    let up = stalling_upstream(
        vec![Some(CHAT_FIRST), None, Some(CHAT_REST)],
        Duration::from_millis(1200),
    )
    .await;
    let gw = gateway_pinging_for(
        provider(up, Protocol::OpenaiChat, &[]),
        Duration::from_millis(350),
    )
    .await;
    let text = stream_from(
        &gw,
        "/v1/messages",
        json!({"model": "gpt-x", "max_tokens": 16, "stream": true,
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    let ping = "event: ping\ndata: {\"type\": \"ping\"}\n\n";
    // 静默 1.2 秒、每 0.1 秒一次：一直补的话有十一个。只补前 0.35 秒的
    let n = text.matches(ping).count();
    assert!((1..=4).contains(&n), "{n}: {text}");
    assert!(
        text.replace(ping, "")
            .trim_end()
            .ends_with("\"type\":\"message_stop\"}"),
        "{text}"
    );
}

#[tokio::test]
async fn a_ping_never_splits_a_frame_the_upstream_left_half_sent() {
    const START: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-5\",\"content\":[],\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n";
    const HALF: &str = "event: content_block_start\ndata: {\"type\":\"content_bl";
    const REST: &str = "ock_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let up = stalling_upstream(
        vec![Some(START), None, Some(HALF), None, Some(REST)],
        Duration::from_millis(500),
    )
    .await;
    let gw = gateway(provider(up, Protocol::Anthropic, &[])).await;
    let text = stream_from(
        &gw,
        "/v1/messages",
        json!({"model": "claude-sonnet-4-5", "max_tokens": 16, "stream": true,
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    let ping = "event: ping\ndata: {\"type\": \"ping\"}\n\n";
    // 第一段静默在帧之间，补了；第二段停在一帧中间，没有补
    assert!(text.contains(&format!("{START}{ping}")), "{text}");
    assert!(text.contains(&format!("{HALF}{REST}")), "{text}");
    assert_eq!(text.replace(ping, ""), format!("{START}{HALF}{REST}"));
}

#[tokio::test]
async fn other_formats_get_no_anthropic_pings() {
    let up = stalling_upstream(
        vec![Some(CHAT_FIRST), None, Some(CHAT_REST)],
        Duration::from_millis(500),
    )
    .await;
    let gw = gateway(provider(up, Protocol::OpenaiChat, &[])).await;
    let text = stream_from(
        &gw,
        "/v1/chat/completions",
        json!({"model": "gpt-x", "stream": true,
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert!(!text.contains("ping"), "{text}");
    assert_eq!(text, format!("{CHAT_FIRST}{CHAT_REST}"));
}
