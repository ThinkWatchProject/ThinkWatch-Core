//! 端到端：上游的请求头，和按路径认出来的客户端 API。
//!
//! 这几条各对着一个真实出过的问题：
//!
//! - 一键接管的 Claude Code 用 `Authorization: Bearer` 发网关密钥，被当成 OpenAI
//!   客户端，请求 Claude 模型被模型准入拒掉
//! - Gemini 的 `POST /v1beta/models/{model}:generateContent` 被只认 GET 的路由拒成 405
//! - 查询串里的 `key=`（网关密钥）被原样拼到上游地址上
//! - 只能填一把密钥，要求自定义鉴权头的中转站接不进来
//! - 客户端的请求头几乎原样转给上游：Claude Code 的 `x-app`、`x-stainless-*`、会话 ID，
//!   浏览器的 Cookie，全都到了上游手里。现在每种上游只收它的协议要的（见 `egress`）

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
    body: Vec<u8>,
}

async fn start_upstream() -> (SocketAddr, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let app = Router::new()
        .fallback(
            |State(s): State<Arc<Mutex<Seen>>>,
             method: Method,
             OriginalUri(uri): OriginalUri,
             headers: HeaderMap,
             body: axum::body::Bytes| async move {
                let mut g = s.lock().unwrap();
                g.method = Some(method);
                g.uri = uri.to_string();
                g.headers = headers;
                g.body = body.to_vec();
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
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
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
            value: Secret::new("thinkwatch"),
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
    assert_eq!(g.headers.get("x-relay-caller").unwrap(), "thinkwatch");
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

#[tokio::test]
async fn a_disabled_key_is_refused_and_says_it_was_disabled_on_purpose() {
    let (up, _seen) = start_upstream().await;
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![
            Client {
                name: "laptop".into(),
                key: "tw-testkey".into(),
                ..Default::default()
            },
            Client {
                name: "试用".into(),
                key: "tw-paused".into(),
                disabled: true,
                ..Default::default()
            },
        ],
        providers: vec![anthropic(up)],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .header("x-api-key", "tw-paused")
        .header("content-type", "application/json")
        .body(BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let text = resp.text().await.unwrap();
    // **和「密钥无效」是两回事**：说成无效的话，用户会去查客户端配置，
    // 而那里什么问题都没有
    assert!(text.contains("disabled"), "{text}");
    assert!(text.contains("试用"), "要说清是哪一把：{text}");

    // 同一个网关上，没停用的那把照常能用
    let ok = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .header("x-api-key", "tw-testkey")
        .header("content-type", "application/json")
        .body(BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
}

/// 一个只回 307 的上游：让客户端去 `to` 那里再发一遍。
async fn start_redirecting_upstream(to: String) -> SocketAddr {
    let app = Router::new().fallback(move || {
        let to = to.clone();
        async move {
            axum::response::Response::builder()
                .status(307)
                .header("location", to)
                .body(axum::body::Body::empty())
                .unwrap()
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

#[tokio::test]
async fn a_redirecting_upstream_cannot_lead_the_credentials_elsewhere() {
    // `localhost` 和 `127.0.0.1` 对 reqwest 是两台主机 —— 跟过去的话，
    // 它只摘 `authorization`，`x-api-key` 和自定义头都会照带
    let (thief, stolen) = start_upstream().await;
    let up = start_redirecting_upstream(format!("http://localhost:{}/steal", thief.port())).await;
    let mut p = anthropic(up);
    p.headers = Headers::new(vec![Header {
        name: "x-relay-token".into(),
        value: Secret::new("relay-secret"),
    }]);
    let gw = start_gateway(p).await;

    let resp = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-testkey")
        .header("content-type", "application/json")
        .body(BODY)
        .send()
        .await
        .unwrap();
    // 3xx 原样交还给客户端，由它决定跟不跟
    assert_eq!(resp.status(), 307);
    let g = stolen.lock().unwrap();
    assert!(g.method.is_none(), "网关跟了重定向，凭据被带到了 {}", g.uri);
}

#[tokio::test]
async fn the_public_data_client_still_follows_a_redirect() {
    // 价目表那个客户端什么凭据都不带，托管地址搬家时要跟得过去
    let (target, seen) = start_upstream().await;
    let up = start_redirecting_upstream(format!("http://localhost:{}/moved", target.port())).await;
    let r = tw_gateway::public_client()
        .unwrap()
        .get(format!("http://{up}/prices.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(seen.lock().unwrap().uri, "/moved");
}

// ───────────────────────────────────────────── 每种上游收到的请求头

/// Claude Code 实际会带的请求头，外加浏览器类客户端会带的
const CLAUDE_CODE: &[(&str, &str)] = &[
    ("x-api-key", "tw-testkey"),
    ("user-agent", "claude-cli/2.1.1 (external, cli)"),
    ("x-app", "cli"),
    ("anthropic-version", "2023-06-01"),
    (
        "anthropic-beta",
        "claude-code-20250219,interleaved-thinking-2025-05-14",
    ),
    ("anthropic-dangerous-direct-browser-access", "true"),
    ("x-stainless-lang", "js"),
    ("x-stainless-os", "MacOS"),
    ("x-stainless-runtime-version", "v22.18.0"),
    ("x-stainless-timeout", "600"),
    ("x-claude-code-session-id", "sess-1"),
    ("accept", "application/json"),
    ("content-type", "application/json"),
    ("cookie", "a=b"),
    ("origin", "http://localhost:3000"),
];

const CLAUDE_CODE_BODY: &str = r#"{"model":"claude-sonnet-4-5","max_tokens":16,"metadata":{"user_id":"user_abc_account_123_session_456"},"messages":[{"role":"user","content":"hi"}]}"#;

async fn post(gw: SocketAddr, path: &str, headers: &[(&str, &str)], body: &str) -> u16 {
    let mut r = reqwest::Client::new().post(format!("http://{gw}{path}"));
    for (k, v) in headers {
        r = r.header(*k, *v);
    }
    let resp = r.body(body.to_string()).send().await.unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    assert_eq!(status, 200, "{text}");
    status
}

fn names(h: &HeaderMap) -> Vec<String> {
    let mut v: Vec<String> = h.keys().map(|k| k.to_string()).collect();
    v.sort();
    v
}

#[tokio::test]
async fn an_anthropic_upstream_gets_the_request_and_nothing_about_the_client() {
    let (up, seen) = start_upstream().await;
    let gw = start_gateway(anthropic(up)).await;
    post(gw, "/v1/messages", CLAUDE_CODE, CLAUDE_CODE_BODY).await;

    let g = seen.lock().unwrap();
    assert_eq!(
        names(&g.headers),
        [
            "accept",
            "anthropic-beta",
            "anthropic-version",
            "content-length",
            "content-type",
            "host",
            "user-agent",
            "x-api-key",
        ]
    );
    assert!(
        g.headers
            .get("user-agent")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("thinkwatch/")
    );
    assert_eq!(g.headers.get("x-api-key").unwrap(), "sk-upstream");
    assert_eq!(
        g.headers.get("anthropic-beta").unwrap(),
        "claude-code-20250219,interleaved-thinking-2025-05-14"
    );
    // 请求体原样，只少了 Claude Code 自动填的身份
    let v: serde_json::Value = serde_json::from_slice(&g.body).unwrap();
    assert!(v.get("metadata").is_none(), "{v}");
    assert_eq!(v["messages"][0]["content"], "hi");
}

#[tokio::test]
async fn an_upstream_that_asks_for_it_gets_the_clients_own_identity() {
    let (up, seen) = start_upstream().await;
    let mut p = anthropic(up);
    p.forward_client_identity = true;
    let gw = start_gateway(p).await;
    post(gw, "/v1/messages", CLAUDE_CODE, CLAUDE_CODE_BODY).await;

    let g = seen.lock().unwrap();
    assert_eq!(
        g.headers.get("user-agent").unwrap(),
        "claude-cli/2.1.1 (external, cli)"
    );
    assert_eq!(g.headers.get("x-app").unwrap(), "cli");
    let v: serde_json::Value = serde_json::from_slice(&g.body).unwrap();
    assert_eq!(
        v["metadata"]["user_id"], "user_abc_account_123_session_456",
        "{v}"
    );
    // 转发的是客户端的身份，不是它的 SDK 细节、浏览器状态和凭据
    for n in [
        "x-stainless-lang",
        "x-stainless-timeout",
        "x-claude-code-session-id",
        "cookie",
        "origin",
        "anthropic-dangerous-direct-browser-access",
        "authorization",
    ] {
        assert!(g.headers.get(n).is_none(), "{n}");
    }
    assert_eq!(g.headers.get("x-api-key").unwrap(), "sk-upstream");
}

#[tokio::test]
async fn an_anthropic_upstream_gets_a_version_even_when_the_client_sent_none() {
    let (up, seen) = start_upstream().await;
    let gw = start_gateway(anthropic(up)).await;
    post(
        gw,
        "/v1/messages",
        &[
            ("x-api-key", "tw-testkey"),
            ("content-type", "application/json"),
        ],
        BODY,
    )
    .await;
    let g = seen.lock().unwrap();
    let versions: Vec<_> = g.headers.get_all("anthropic-version").iter().collect();
    assert_eq!(versions, ["2023-06-01"]);
}

#[tokio::test]
async fn an_openai_upstream_takes_the_organization_from_its_own_configuration() {
    let (up, seen) = start_upstream().await;
    let p = Provider {
        name: "openai".into(),
        base_url: format!("http://{up}/v1"),
        key: Some("sk-upstream".into()),
        protocol: Some(Protocol::OpenaiChat),
        models: vec!["gpt-5.5".into()],
        headers: Headers::new(vec![Header {
            name: "OpenAI-Organization".into(),
            value: Secret::new("org-upstream"),
        }]),
        ..Default::default()
    };
    let gw = start_gateway(p).await;
    post(
        gw,
        "/v1/chat/completions",
        &[
            ("authorization", "Bearer tw-testkey"),
            ("content-type", "application/json"),
            ("user-agent", "OpenAI/Python 1.99.0"),
            ("x-stainless-lang", "python"),
            ("openai-organization", "org-client"),
            ("openai-project", "proj-client"),
            ("idempotency-key", "idem-1"),
            ("x-client-request-id", "req-1"),
        ],
        r#"{"model":"gpt-5.5","messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    let g = seen.lock().unwrap();
    assert_eq!(
        names(&g.headers),
        [
            "accept",
            "authorization",
            "content-length",
            "content-type",
            "host",
            "idempotency-key",
            "openai-organization",
            "user-agent",
            "x-client-request-id",
        ]
    );
    // 客户端没写 accept：这是 HTTP 库自己的默认值，不是客户端的
    assert_eq!(g.headers.get("accept").unwrap(), "*/*");
    assert_eq!(
        g.headers.get("authorization").unwrap(),
        "Bearer sk-upstream"
    );
    let orgs: Vec<_> = g.headers.get_all("openai-organization").iter().collect();
    assert_eq!(orgs, ["org-upstream"], "组织属于这家的密钥，不取客户端的");
}

#[tokio::test]
async fn a_gemini_upstream_gets_nothing_from_the_client_but_the_body_type() {
    let (up, seen) = start_upstream().await;
    let p = Provider {
        name: "gemini".into(),
        base_url: format!("http://{up}"),
        key: Some("g-upstream".into()),
        protocol: Some(Protocol::Gemini),
        models: vec!["gemini-3-pro-preview".into()],
        ..Default::default()
    };
    let gw = start_gateway(p).await;
    post(
        gw,
        "/v1beta/models/gemini-3-pro-preview:generateContent",
        &[
            ("x-goog-api-key", "tw-testkey"),
            ("content-type", "application/json"),
            ("user-agent", "GeminiCLI/0.40.0 (darwin; arm64)"),
            (
                "x-goog-api-client",
                "google-genai-sdk/1.30.0 gl-node/v22.18.0",
            ),
            ("x-gemini-api-privileged-user-id", "install-1"),
            ("x-server-timeout", "600"),
        ],
        r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
    )
    .await;
    let g = seen.lock().unwrap();
    assert_eq!(
        names(&g.headers),
        [
            "accept",
            "content-length",
            "content-type",
            "host",
            "user-agent",
            "x-goog-api-key",
        ]
    );
    assert_eq!(g.headers.get("accept").unwrap(), "*/*");
    assert_eq!(g.headers.get("x-goog-api-key").unwrap(), "g-upstream");
}
