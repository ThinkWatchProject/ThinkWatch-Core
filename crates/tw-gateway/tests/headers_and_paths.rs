//! 端到端：上游的请求头，和按路径认出来的客户端 API。
//!
//! 这几条各对着一个真实出过的问题：
//!
//! - 一键接管的 Claude Code 用 `Authorization: Bearer` 发网关密钥，被当成 OpenAI
//!   客户端，请求 Claude 模型被模型准入拒掉
//! - Gemini 的 `POST /v1beta/models/{model}:generateContent` 被只认 GET 的路由拒成 405
//! - 查询串里的 `key=`（网关密钥）被原样拼到上游地址上
//! - 只能填一把密钥，要求自定义鉴权头的中转站接不进来

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, Method};
use tw_config::{Client, Config, Header, Headers, Listen, Protocol, Provider, Secret};

#[derive(Default, Debug)]
struct Seen {
    method: Option<Method>,
    uri: String,
    headers: HeaderMap,
}

async fn start_upstream() -> (SocketAddr, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let app = Router::new()
        .fallback(
            |State(s): State<Arc<Mutex<Seen>>>,
             method: Method,
             OriginalUri(uri): OriginalUri,
             headers: HeaderMap| async move {
                let mut g = s.lock().unwrap();
                g.method = Some(method);
                g.uri = uri.to_string();
                g.headers = headers;
                axum::response::Response::builder()
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"ok":true}"#))
                    .unwrap()
            },
        )
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, seen)
}

async fn start_gateway(provider: Provider) -> SocketAddr {
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "laptop".into(),
            key: "tw-testkey".into(),
            ..Default::default()
        }],
        providers: vec![provider],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let addr = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, addr).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

fn anthropic(upstream: SocketAddr) -> Provider {
    Provider {
        name: "official".into(),
        base_url: format!("http://{upstream}"),
        key: Some("sk-upstream".into()),
        protocol: Some(Protocol::Anthropic),
        // 手写清单让模型目录不为空 —— 准入只在目录不为空时才拦
        models: vec!["claude-sonnet-4-5".into()],
        redact: Some(vec![]),
        ..Default::default()
    }
}

const BODY: &str =
    r#"{"model":"claude-sonnet-4-5","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#;

#[tokio::test]
async fn claude_code_with_a_bearer_key_can_use_an_anthropic_upstream() {
    let (up, seen) = start_upstream().await;
    let gw = start_gateway(anthropic(up)).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        // 接管写的是 ANTHROPIC_AUTH_TOKEN，Claude Code 把它发成 Bearer
        .header("authorization", "Bearer tw-testkey")
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(BODY)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(status, 200, "{text}");
    let g = seen.lock().unwrap();
    assert_eq!(g.headers.get("x-api-key").unwrap(), "sk-upstream");
    assert!(
        g.headers.get("authorization").is_none(),
        "网关密钥被转给了上游"
    );
}

#[tokio::test]
async fn a_bearer_key_with_the_anthropic_version_header_lists_anthropic_models() {
    let (up, _) = start_upstream().await;
    let gw = start_gateway(anthropic(up)).await;

    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("http://{gw}/v1/models"))
        .header("authorization", "Bearer tw-testkey")
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert_eq!(ids, ["claude-sonnet-4-5"], "{body}");
}

#[tokio::test]
async fn a_gemini_generate_content_call_is_forwarded_without_the_gateway_key() {
    let (up, seen) = start_upstream().await;
    let gw = start_gateway(Provider {
        name: "gemini".into(),
        base_url: format!("http://{up}"),
        key: Some("g-upstream".into()),
        protocol: Some(Protocol::Gemini),
        redact: Some(vec![]),
        ..Default::default()
    })
    .await;

    let resp = reqwest::Client::new()
        .post(format!(
            "http://{gw}/v1beta/models/gemini-2.5-pro:generateContent?key=tw-testkey&alt=sse"
        ))
        .header("content-type", "application/json")
        .body(r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(status, 200, "被路由挡掉了：{text}");
    let g = seen.lock().unwrap();
    assert_eq!(g.method, Some(Method::POST));
    assert_eq!(
        g.uri,
        "/v1beta/models/gemini-2.5-pro:generateContent?alt=sse"
    );
    assert!(
        !g.uri.contains("tw-testkey"),
        "网关密钥进了上游地址：{}",
        g.uri
    );
    assert_eq!(g.headers.get("x-goog-api-key").unwrap(), "g-upstream");
}

#[tokio::test]
async fn configured_headers_are_sent_and_replace_the_ones_the_client_sent() {
    let (up, seen) = start_upstream().await;
    let mut p = anthropic(up);
    p.headers = Headers::new(vec![
        Header {
            name: "anthropic-version".into(),
            value: Secret::new("2099-01-01"),
        },
        Header {
            name: "X-Relay-Caller".into(),
            value: Secret::new("{{client}}"),
        },
    ]);
    let gw = start_gateway(p).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-testkey")
        .header("anthropic-version", "2023-06-01")
        .header("x-thinkwatch-client", "codex")
        .header("content-type", "application/json")
        .body(BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let g = seen.lock().unwrap();
    let versions: Vec<_> = g.headers.get_all("anthropic-version").iter().collect();
    assert_eq!(versions, ["2099-01-01"], "同名头应该被盖掉，而不是并存");
    assert_eq!(g.headers.get("x-relay-caller").unwrap(), "laptop");
    assert!(g.headers.get("x-thinkwatch-client").is_none());
}

#[tokio::test]
async fn a_relay_can_take_its_credential_in_its_own_header() {
    let (up, seen) = start_upstream().await;
    let mut p = anthropic(up);
    p.key = None;
    p.headers = Headers::new(vec![Header {
        name: "X-Relay-Token".into(),
        value: Secret::new("rt-1"),
    }]);
    let gw = start_gateway(p).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-testkey")
        .header("content-type", "application/json")
        .body(BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let g = seen.lock().unwrap();
    assert_eq!(g.headers.get("x-relay-token").unwrap(), "rt-1");
    assert!(
        g.headers.get("x-api-key").is_none(),
        "没写 key 就不该发鉴权头"
    );
}
