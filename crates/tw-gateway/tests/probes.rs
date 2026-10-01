//! 不是 POST 的请求在网关就地回 404，不送上游、不记录。
//!
//! 上游的接口都是 POST，网关发出去时也一律用 POST。别的方法落到透传上，是客户端在探路：
//! Hermes Agent 起来时拿 GET 挨个问 `/api/v1/models`、`/api/tags`、`/v1/props`、`/props`、
//! `/version`，看网关是不是 LM Studio、Ollama、llama.cpp、vLLM。原来它们被改成空正文的 POST
//! 发给上游，每个都记成一条失败的请求；上游要是回了 200，Hermes 还会把网关认成那种服务。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{OriginalUri, State};
use reqwest::Method;
use serde_json::{Value, json};
use tw_config::{Client, Config, Listen, Protocol, Provider};

type Log = Arc<Mutex<Vec<(String, String)>>>;

/// 一个假上游：记下收到的每个请求（方法和路径），什么都回 200 —— 最糟的那种上游，
/// 探测要是漏过去，客户端就会把网关认错
async fn upstream() -> (SocketAddr, Log) {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .fallback(
            |State(log): State<Log>, method: axum::http::Method, OriginalUri(uri): OriginalUri| async move {
                log.lock().unwrap().push((method.to_string(), uri.to_string()));
                axum::Json(json!({
                    "id": "chatcmpl-1", "object": "chat.completion", "model": "m",
                    "models": [], "version": "0.0.0",
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 2}
                }))
            },
        )
        .with_state(log.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, log)
}

async fn gateway(up: SocketAddr) -> (SocketAddr, tokio::sync::broadcast::Receiver<tw_api::Event>) {
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "relay".into(),
            base_url: format!("http://{up}"),
            key: Some("sk-upstream".into()),
            protocol: Some(Protocol::OpenaiChat),
            ..Default::default()
        }],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let rx = state.bus.subscribe();
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, rx)
}

/// 总线上说到请求的那些事件（开始、结局）。别的（熔断、模型清单）不算
fn request_events(rx: &mut tokio::sync::broadcast::Receiver<tw_api::Event>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        let v = serde_json::to_value(&ev).unwrap();
        let kind = v["kind"].as_str().unwrap_or_default().to_string();
        if kind.starts_with("request_") {
            out.push(kind);
        }
    }
    out
}

#[tokio::test]
async fn local_server_probes_get_a_404_from_the_gateway_itself() {
    let (up, log) = upstream().await;
    let (gw, mut rx) = gateway(up).await;
    let http = reqwest::Client::new();
    for path in [
        "/api/v1/models",
        "/api/tags",
        "/v1/props",
        "/props",
        "/version",
    ] {
        let resp = http
            .get(format!("http://{gw}{path}"))
            .header("authorization", "Bearer tw-k")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "{path}");
        let v: Value = resp.json().await.unwrap();
        assert_eq!(v["error"]["type"], "not_found_error", "{path}: {v}");
        let message = v["error"]["message"].as_str().unwrap();
        assert!(message.starts_with("[ThinkWatch] GET "), "{path}: {v}");
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        log.lock().unwrap().is_empty(),
        "a probe reached the upstream: {:?}",
        log.lock().unwrap()
    );
    assert_eq!(
        request_events(&mut rx),
        Vec::<String>::new(),
        "a probe was recorded as a request"
    );
}

#[tokio::test]
async fn no_method_but_post_is_forwarded() {
    let (up, log) = upstream().await;
    let (gw, mut rx) = gateway(up).await;
    let http = reqwest::Client::new();
    for (method, path) in [
        (Method::GET, "/v1/chat/completions"),
        (Method::HEAD, "/api/tags"),
        (Method::OPTIONS, "/v1/messages"),
        (Method::DELETE, "/v1/responses/resp_1"),
        (Method::PUT, "/v1/chat/completions"),
    ] {
        let resp = http
            .request(method.clone(), format!("http://{gw}{path}"))
            .header("authorization", "Bearer tw-k")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "{method} {path}");
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(log.lock().unwrap().is_empty(), "{:?}", log.lock().unwrap());
    assert_eq!(request_events(&mut rx), Vec::<String>::new());

    // POST 照常送到上游、照常记录
    let resp = http
        .post(format!("http://{gw}/v1/chat/completions"))
        .header("authorization", "Bearer tw-k")
        .json(&json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    assert_eq!(
        *log.lock().unwrap(),
        vec![("POST".to_string(), "/v1/chat/completions".to_string())]
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(request_events(&mut rx).contains(&"request_started".to_string()));
}

/// 没带密钥的照旧是 401：陌生的来访者探不出网关会怎么回
#[tokio::test]
async fn a_probe_without_a_key_is_still_turned_away() {
    let (up, log) = upstream().await;
    let (gw, _rx) = gateway(up).await;
    let resp = reqwest::Client::new()
        .get(format!("http://{gw}/api/tags"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    assert!(log.lock().unwrap().is_empty());
}

/// 带 `anthropic-version` 的按 Anthropic 的格式回错误，外面多一层 `type`
#[tokio::test]
async fn the_404_speaks_the_clients_format() {
    let (up, _log) = upstream().await;
    let (gw, _rx) = gateway(up).await;
    let resp = reqwest::Client::new()
        .get(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["type"], "error", "{v}");
    assert_eq!(v["error"]["type"], "not_found_error", "{v}");
}
