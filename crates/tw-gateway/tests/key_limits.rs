//! 密钥的用量上限，走真的网关：被拒的请求在每一种客户端格式里长什么样、带什么响应头，
//! 流量里有没有它那一行，数 token 的请求和 WebSocket 连接怎么算。
//!
//! 数和等的细节在 `tw_gateway::key_limits` 的单元测试里；这里看的是它接在管线上的样子。

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::routing::any;
use tw_config::{Client, Config, Provider};

/// 什么都回 200 的上游，数着收到了几个请求
async fn upstream() -> (SocketAddr, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let app = Router::new().fallback(any(move || {
        let h = h.clone();
        async move {
            h.fetch_add(1, Ordering::SeqCst);
            axum::response::Response::builder()
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    r#"{"id":"m","type":"message","role":"assistant","model":"claude-sonnet-4-5",
                        "content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn",
                        "usage":{"input_tokens":10,"output_tokens":2},"input_tokens":10}"#,
                ))
                .unwrap()
        }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (a, hits)
}

/// 网关，密钥 `k` 带着这几条上限。**时钟定在中午**：天的上限不会碰巧在测试跑到一半时
/// 过零点。先订阅事件再起服务
async fn gateway(
    up: SocketAddr,
    limits: &str,
) -> (SocketAddr, tokio::sync::broadcast::Receiver<tw_api::Event>) {
    let cfg = Config {
        clients: vec![Client {
            name: "k".into(),
            key: "tw-k".into(),
            limits: serde_yaml_ng::from_str(limits).unwrap(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "官方".into(),
            base_url: format!("http://{up}"),
            key: Some("sk-x".into()),
            protocol: Some(tw_config::Protocol::Anthropic),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut state = tw_gateway::AppState::new(cfg).unwrap();
    let noon = chrono::DateTime::parse_from_rfc3339("2026-10-05T12:00:00+08:00")
        .unwrap()
        .timestamp_millis();
    state.set_key_limits_clock(Arc::new(tw_gateway::key_limits::TestClock::new(
        noon,
        8 * 3600,
    )));
    let rx = state.bus.subscribe();
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, rx)
}

const MESSAGE: &str =
    r#"{"model":"claude-sonnet-4-5","max_tokens":5,"messages":[{"role":"user","content":"hi"}]}"#;

async fn anthropic(gw: SocketAddr, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{gw}{path}"))
        .header("x-api-key", "tw-k")
        .header("anthropic-version", "2023-06-01")
        .body(MESSAGE)
        .send()
        .await
        .unwrap()
}

fn head(r: &reqwest::Response, h: &str) -> Option<String> {
    r.headers().get(h).map(|v| v.to_str().unwrap().to_string())
}

/// 一天一个请求：第二个在四种格式里都是 429，说清是哪把密钥、哪一条、用了多少、什么
/// 时候重置；到重置之前别重试，OpenAI 的两种格式写成额度用完。**一个字节都没发给上游**
#[tokio::test]
async fn a_used_up_day_is_refused_in_each_clients_own_shape() {
    let (up, hits) = upstream().await;
    let (gw, _rx) = gateway(up, "[{per: day, requests: 1}]").await;
    assert_eq!(anthropic(gw, "/v1/messages").await.status(), 200);
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let r = anthropic(gw, "/v1/messages").await;
    assert_eq!(r.status(), 429);
    assert_eq!(
        head(&r, "x-thinkwatch-error").as_deref(),
        Some("rate_limited")
    );
    assert_eq!(head(&r, "x-should-retry").as_deref(), Some("false"));
    // 中午到零点：十二个小时
    assert_eq!(head(&r, "retry-after").as_deref(), Some("43200"));
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["error"]["type"], "rate_limit_error");
    assert_eq!(
        v["error"]["message"],
        "[ThinkWatch] Gateway key `k` has reached its limit of 1 requests per day: 1 so far. It \
         resets at 2026-10-06 00:00 +08:00."
    );

    let client = reqwest::Client::new();
    for path in ["/v1/chat/completions", "/v1/responses"] {
        let r = client
            .post(format!("http://{gw}{path}"))
            .header("authorization", "Bearer tw-k")
            .body(r#"{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"hi"}],"input":"hi"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 429, "{path}");
        assert_eq!(head(&r, "x-should-retry").as_deref(), Some("false"));
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["error"]["code"], "insufficient_quota", "{path}: {v}");
        assert_eq!(v["error"]["type"], "rate_limit_error");
    }
    let r = client
        .post(format!(
            "http://{gw}/v1beta/models/claude-sonnet-4-5:generateContent"
        ))
        .header("x-goog-api-key", "tw-k")
        .body(r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 429);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["error"]["status"], "RESOURCE_EXHAUSTED");
    assert_eq!(hits.load(Ordering::SeqCst), 1, "被拒的发给了上游");
}

/// 被拒的请求流量里照样有一行：开始、一条空的尝试链、一个带着上限那句话的失败。
#[tokio::test]
async fn a_refused_request_is_recorded_like_other_refusals() {
    let (up, _hits) = upstream().await;
    let (gw, mut rx) = gateway(up, "[{per: day, requests: 1}]").await;
    anthropic(gw, "/v1/messages").await;
    let r = anthropic(gw, "/v1/messages").await;
    assert_eq!(r.status(), 429);
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Ok(e)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        seen.push(e);
    }
    let failed = seen
        .iter()
        .find_map(|e| match e {
            tw_api::Event::RequestFailed {
                id,
                message,
                source,
                ..
            } => Some((*id, message.clone(), *source)),
            _ => None,
        })
        .expect("被拒的请求没有结局");
    assert_eq!(failed.1.code, "gw.key_limit.requests_per_period");
    assert_eq!(failed.1.arg("key"), "k");
    assert_eq!(failed.1.arg("per"), "day");
    assert_eq!(failed.2, tw_api::FailureSource::RateLimited);
    assert!(seen.iter().any(|e| matches!(
        e,
        tw_api::Event::RequestStarted { id, .. } if *id == failed.0
    )));
    assert!(seen.iter().any(|e| matches!(
        e,
        tw_api::Event::RequestRouted { id, attempts, .. } if *id == failed.0 && attempts.is_empty()
    )));
    // 被拒的那一刻到了顶：报一条，给界面发系统通知
    assert!(seen.iter().any(|e| matches!(
        e,
        tw_api::Event::KeyLimitAlert { key, reached: true, .. } if key == "k"
    )));
}

/// 一分钟一个：第二个等不到空位（要等快一分钟，超过 30 秒），拒，并说准多久之后再来，
/// 可以重试。
#[tokio::test]
async fn a_rolling_limit_says_exactly_when_to_retry() {
    let (up, _hits) = upstream().await;
    let (gw, _rx) = gateway(up, "[{per: minute, requests: 1}]").await;
    assert_eq!(anthropic(gw, "/v1/messages").await.status(), 200);
    let r = anthropic(gw, "/v1/messages").await;
    assert_eq!(r.status(), 429);
    assert_eq!(head(&r, "x-should-retry").as_deref(), Some("true"));
    let ms: u64 = head(&r, "retry-after-ms").unwrap().parse().unwrap();
    assert!((50_000..=60_000).contains(&ms), "{ms}");
    let secs: u64 = head(&r, "retry-after").unwrap().parse().unwrap();
    assert_eq!(secs, ms.div_ceil(1000));
    let v: serde_json::Value = r.json().await.unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("1 requests per minute"),
        "{v}"
    );
}

/// 数 token 的请求不跑模型、不收钱：不算用量，用满了也照样放行。
#[tokio::test]
async fn counting_tokens_is_not_counted_and_not_refused() {
    let (up, _hits) = upstream().await;
    let (gw, _rx) = gateway(up, "[{per: day, requests: 1}]").await;
    for _ in 0..3 {
        assert_eq!(
            anthropic(gw, "/v1/messages/count_tokens").await.status(),
            200
        );
    }
    assert_eq!(anthropic(gw, "/v1/messages").await.status(), 200);
    assert_eq!(anthropic(gw, "/v1/messages").await.status(), 429);
    assert_eq!(
        anthropic(gw, "/v1/messages/count_tokens").await.status(),
        200
    );
}

/// WebSocket 一条连接算一个请求，**连上之前**看：用满了，升级就是一个 429。
#[tokio::test]
async fn a_websocket_connection_is_admitted_when_it_opens() {
    let (up, _hits) = upstream().await;
    let (gw, _rx) = gateway(up, "[{per: day, requests: 1}]").await;
    assert_eq!(anthropic(gw, "/v1/messages").await.status(), 200);
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = format!("ws://{gw}/v1/responses")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", "Bearer tw-k".parse().unwrap());
    match tokio_tungstenite::connect_async(req).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => {
            assert_eq!(r.status(), 429);
            assert_eq!(
                r.headers().get("x-should-retry").unwrap().to_str().unwrap(),
                "false"
            );
        }
        other => panic!("升级没有被拒：{:?}", other.map(|_| ())),
    }
}
