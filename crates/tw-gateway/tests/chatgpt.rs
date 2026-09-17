//! ChatGPT 账号上游，端到端。
//!
//! 假的 token 端点和假的 Codex 后端，按 2026-09-18 实测到的样子回：流式响应**不带
//! Content-Type**、额度在 `x-codex-*` 头里、token 端点收 JSON。验的是网关把这些接缝接对了：
//!
//! - 身份是 ThinkWatch 自己的，客户端报的来源不转发
//! - 请求改成 Codex 后端接受的样子（只收流式、不认输出上限、路径是 `/responses`）
//! - 没有 Content-Type 的流照样按流处理；客户端要整包时由网关收齐
//! - 401 换一次 token 重发；并发请求只刷新一次；refresh token 作废只报一次
//! - 额度用完只报一次

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{OriginalUri, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use serde_json::{Value, json};
use tw_config::credential::{Header, Headers};
use tw_config::{Client, Config, Listen, OAuth, Protocol, Provider, Secret};
use tw_gateway::chatgpt::{ACCOUNT_HEADER, CLIENT_ID};

// ---------------------------------------------------------------- 假 token 端点

#[derive(Default)]
struct TokenServer {
    calls: AtomicUsize,
    /// 每次收到的 (Content-Type, 请求体)
    seen: Mutex<Vec<(String, String)>>,
    /// 回 400 invalid_grant：refresh token 已被吊销
    revoked: bool,
    /// 回应之前等多久。并发刷新的测试靠它让请求挤在一起
    delay_ms: u64,
}

async fn start_token_server(t: Arc<TokenServer>) -> String {
    let app = Router::new()
        .route(
            "/oauth/token",
            post(
                |State(t): State<Arc<TokenServer>>, headers: HeaderMap, body: String| async move {
                    let n = t.calls.fetch_add(1, Ordering::SeqCst) + 1;
                    let ct = headers
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    t.seen.lock().unwrap().push((ct, body));
                    if t.delay_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(t.delay_ms)).await;
                    }
                    if t.revoked {
                        return (
                            axum::http::StatusCode::BAD_REQUEST,
                            axum::Json(json!({
                                "error": "invalid_grant",
                                "error_description": "Refresh token has been revoked"
                            })),
                        );
                    }
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(json!({
                            "access_token": format!("at-{n}"),
                            "refresh_token": format!("rt-{n}"),
                            "id_token": "id",
                            "expires_in": 864000
                        })),
                    )
                },
            ),
        )
        .with_state(t);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}/oauth/token")
}

// ---------------------------------------------------------------- 假 Codex 后端

#[derive(Default)]
struct Backend {
    calls: AtomicUsize,
    /// 每次收到的 (路径和查询串, 请求头, 请求体)
    seen: Mutex<Vec<(String, HeaderMap, Value)>>,
    /// 用这个 access token 的请求回 401
    reject: Option<String>,
    /// 不论什么 token 都回 401
    reject_all: bool,
    /// 额度用完：回 429
    exhausted: bool,
}

/// 实测的事件顺序：一条消息，回复 "ok"
fn stream_body() -> String {
    let ev = |kind: &str, mut v: Value| {
        v["type"] = json!(kind);
        format!("event: {kind}\ndata: {v}\n\n")
    };
    [
        ev("response.created", json!({"response": {"id": "resp_1", "model": "gpt-5.5", "status": "in_progress"}})),
        ev("response.in_progress", json!({"response": {"id": "resp_1", "status": "in_progress"}})),
        ev("response.output_item.added", json!({"output_index": 0, "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}})),
        ev("response.content_part.added", json!({"output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": ""}})),
        ev("response.output_text.delta", json!({"output_index": 0, "content_index": 0, "delta": "ok"})),
        ev("response.output_text.done", json!({"output_index": 0, "content_index": 0, "text": "ok"})),
        ev("response.content_part.done", json!({"output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": "ok"}})),
        ev("response.output_item.done", json!({"output_index": 0, "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": [{"type": "output_text", "text": "ok"}]}})),
        ev("response.completed", json!({"response": {"id": "resp_1", "status": "completed", "usage": {"input_tokens": 23, "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0}, "output_tokens": 5, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 28}}})),
    ]
    .concat()
}

async fn start_backend(b: Arc<Backend>) -> String {
    async fn responses(
        State(b): State<Arc<Backend>>,
        OriginalUri(uri): OriginalUri,
        headers: HeaderMap,
        body: String,
    ) -> axum::response::Response {
        b.calls.fetch_add(1, Ordering::SeqCst);
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let parsed = serde_json::from_str(&body).unwrap_or(Value::Null);
        b.seen
            .lock()
            .unwrap()
            .push((uri.to_string(), headers, parsed));
        let builder = axum::response::Response::builder();
        if b.reject_all
            || b.reject
                .as_deref()
                .is_some_and(|t| auth == format!("Bearer {t}"))
        {
            return builder
                .status(401)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(r#"{"detail":"Unauthorized"}"#))
                .unwrap();
        }
        if b.exhausted {
            return builder
                .status(429)
                .header("content-type", "application/json")
                .header("x-codex-primary-used-percent", "100")
                .header("x-codex-primary-window-minutes", "10080")
                .header("x-codex-primary-reset-after-seconds", "3600")
                .body(axum::body::Body::from(
                    r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached"}}"#,
                ))
                .unwrap();
        }
        // **不带 Content-Type**：实测的 Codex 后端就是这样
        builder
            .status(200)
            .header("x-codex-primary-used-percent", "21")
            .header("x-codex-primary-window-minutes", "10080")
            .header("x-codex-primary-reset-after-seconds", "410912")
            .header("x-codex-secondary-used-percent", "0")
            .header("x-codex-secondary-window-minutes", "0")
            .body(axum::body::Body::from(stream_body()))
            .unwrap()
    }
    async fn models(
        State(b): State<Arc<Backend>>,
        OriginalUri(uri): OriginalUri,
        headers: HeaderMap,
    ) -> axum::Json<Value> {
        b.seen
            .lock()
            .unwrap()
            .push((uri.to_string(), headers, Value::Null));
        axum::Json(json!({"models": [
            {"slug": "gpt-5.5", "visibility": "list", "supported_in_api": true},
            {"slug": "codex-auto-review", "visibility": "hide", "supported_in_api": true}
        ]}))
    }
    let app = Router::new()
        .route("/backend-api/codex/responses", post(responses))
        .route("/backend-api/codex/models", get(models))
        .with_state(b);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}/backend-api/codex")
}

// ---------------------------------------------------------------- 网关

fn chatgpt_provider(base_url: &str, token_url: &str) -> Provider {
    Provider {
        name: "chatgpt".into(),
        base_url: base_url.into(),
        protocol: Some(Protocol::Chatgpt),
        oauth: Some(OAuth {
            access: None,
            expires_at: None,
            refresh: "rt-0".into(),
            endpoint: token_url.into(),
            client_id: Some(CLIENT_ID.into()),
            client_secret: None,
            refresh_before: None,
        }),
        headers: Headers::new(vec![Header {
            name: ACCOUNT_HEADER.into(),
            value: Secret::new("acc-123"),
        }]),
        redact: Some(vec![]),
        ..Default::default()
    }
}

async fn start_gateway(
    p: Provider,
) -> (
    SocketAddr,
    tw_gateway::AppState,
    tokio::sync::broadcast::Receiver<tw_api::Event>,
) {
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![p],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let rx = state.bus.subscribe();
    let handed = state.clone();
    let addr = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, addr).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(60)).await;
    (addr, handed, rx)
}

async fn post_json(
    gw: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
    body: Value,
) -> (u16, String, String) {
    let mut r = reqwest::Client::new()
        .post(format!("http://{gw}{path}"))
        .header("content-type", "application/json");
    for (k, v) in headers {
        r = r.header(*k, *v);
    }
    let resp = r.body(body.to_string()).send().await.unwrap();
    let status = resp.status().as_u16();
    let ct = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    (status, ct, resp.text().await.unwrap())
}

/// 收一小段时间内的事件
async fn drain(rx: &mut tokio::sync::broadcast::Receiver<tw_api::Event>) -> Vec<tw_api::Event> {
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        out.push(e);
    }
    out
}

fn codex_request(stream: bool) -> Value {
    json!({
        "model": "gpt-5.5",
        "instructions": "You are a coding agent.",
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
        "stream": stream,
        "store": true,
        "max_output_tokens": 512,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "conv-1"
    })
}

// ---------------------------------------------------------------- 测试

#[tokio::test]
async fn a_codex_cli_request_reaches_the_backend_as_thinkwatch() {
    let tokens = Arc::new(TokenServer::default());
    let token_url = start_token_server(tokens.clone()).await;
    let backend = Arc::new(Backend::default());
    let base = start_backend(backend.clone()).await;
    let (gw, _, mut rx) = start_gateway(chatgpt_provider(&base, &token_url)).await;

    let (status, ct, body) = post_json(
        gw,
        "/v1/responses",
        &[
            ("authorization", "Bearer tw-k"),
            ("originator", "codex_cli_rs"),
            ("user-agent", "codex_cli_rs/0.153.0 (Mac OS 26.0; arm64)"),
            ("session_id", "conv-1"),
            ("version", "0.153.0"),
        ],
        codex_request(true),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        ct, "text/event-stream",
        "没有 Content-Type 的流要按流交给客户端"
    );
    assert!(
        body.contains("response.completed") && body.contains("\"ok\""),
        "{body}"
    );

    let seen = backend.seen.lock().unwrap().clone();
    let (uri, headers, sent) = &seen[0];
    assert_eq!(uri, "/backend-api/codex/responses");
    let h = |k: &str| headers.get(k).map(|v| v.to_str().unwrap().to_string());
    assert_eq!(
        h("originator").as_deref(),
        Some("thinkwatch"),
        "来源要如实写 ThinkWatch"
    );
    assert!(
        h("user-agent").unwrap().starts_with("thinkwatch/"),
        "{:?}",
        h("user-agent")
    );
    assert_eq!(
        h("session-id").as_deref(),
        Some("conv-1"),
        "会话 ID 沿用客户端的"
    );
    assert_eq!(h("session_id"), None);
    assert_eq!(h("version"), None);
    assert_eq!(h("chatgpt-account-id").as_deref(), Some("acc-123"));
    assert_eq!(h("authorization").as_deref(), Some("Bearer at-1"));
    assert_eq!(h("accept").as_deref(), Some("text/event-stream"));

    assert_eq!(sent["stream"], true);
    assert_eq!(sent["store"], false);
    assert!(sent.get("max_output_tokens").is_none(), "{sent}");
    assert_eq!(sent["prompt_cache_key"], "conv-1", "其余字段原样发");
    assert_eq!(sent["instructions"], "You are a coding agent.");

    // Codex 刷新用 JSON
    let (token_ct, token_body) = tokens.seen.lock().unwrap()[0].clone();
    assert!(token_ct.starts_with("application/json"), "{token_ct}");
    let tb: Value = serde_json::from_str(&token_body).unwrap();
    assert_eq!(tb["grant_type"], "refresh_token");
    assert_eq!(tb["refresh_token"], "rt-0");
    assert_eq!(tb["client_id"], CLIENT_ID);

    let events = drain(&mut rx).await;
    let dropped = events.iter().find_map(|e| match e {
        tw_api::Event::Translated { dropped, .. } => Some(dropped.clone()),
        _ => None,
    });
    assert_eq!(
        dropped,
        Some(vec!["max_output_tokens".to_string()]),
        "删掉的字段要说出来"
    );
    let windows = events
        .iter()
        .find_map(|e| match e {
            tw_api::Event::QuotaSeen { windows, .. } => Some(windows.clone()),
            _ => None,
        })
        .expect("额度头要读");
    assert_eq!(windows.len(), 1, "没启用的窗口不列：{windows:?}");
    assert_eq!(windows[0].window, "weekly");
    assert_eq!(windows[0].used_percent, 21.0);
}

#[tokio::test]
async fn a_client_that_wants_a_whole_answer_gets_one_from_the_stream_only_backend() {
    let tokens = Arc::new(TokenServer::default());
    let token_url = start_token_server(tokens.clone()).await;
    let backend = Arc::new(Backend::default());
    let base = start_backend(backend.clone()).await;
    let (gw, _, mut rx) = start_gateway(chatgpt_provider(&base, &token_url)).await;

    // Anthropic 客户端，不要流：转换成 Responses，发出去是流，收齐了回一个整包
    let (status, ct, body) = post_json(
        gw,
        "/v1/messages",
        &[("x-api-key", "tw-k")],
        json!({"model": "gpt-5.5", "max_tokens": 1000, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(ct, "application/json");
    let v: Value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
    assert_eq!(v["type"], "message", "{v}");
    assert_eq!(v["content"][0]["text"], "ok", "{v}");

    let sent = backend.seen.lock().unwrap()[0].2.clone();
    assert_eq!(sent["stream"], true, "{sent}");
    assert!(sent.get("max_output_tokens").is_none(), "{sent}");
    let dropped = drain(&mut rx).await.into_iter().find_map(|e| match e {
        tw_api::Event::Translated { dropped, .. } => Some(dropped),
        _ => None,
    });
    assert!(
        dropped
            .unwrap_or_default()
            .contains(&"max_tokens".to_string()),
        "按客户端的写法说丢了哪个字段"
    );

    // Responses 客户端直通，不要流：同样收齐
    let (status, ct, body) = post_json(
        gw,
        "/v1/responses",
        &[("authorization", "Bearer tw-k")],
        codex_request(false),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(ct, "application/json");
    let v: Value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
    assert!(body.contains("\"ok\""), "{v}");
}

#[tokio::test]
async fn a_401_gets_one_fresh_token_and_one_retry() {
    // 配置里那个 access token 在别处被吊销了
    let tokens = Arc::new(TokenServer::default());
    let token_url = start_token_server(tokens.clone()).await;
    let backend = Arc::new(Backend {
        reject: Some("at-0".into()),
        ..Default::default()
    });
    let base = start_backend(backend.clone()).await;
    let mut p = chatgpt_provider(&base, &token_url);
    p.oauth.as_mut().unwrap().access = Some("at-0".into());
    let (gw, _, _rx) = start_gateway(p).await;

    let (status, _, body) = post_json(
        gw,
        "/v1/responses",
        &[("authorization", "Bearer tw-k")],
        codex_request(true),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(tokens.calls.load(Ordering::SeqCst), 1);
    assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
    let seen = backend.seen.lock().unwrap().clone();
    assert_eq!(
        seen[1].1.get("authorization").unwrap(),
        "Bearer at-1",
        "重发用的是换回来的新 token"
    );
}

#[tokio::test]
async fn an_upstream_that_rejects_a_fresh_token_does_not_get_a_refresh_per_request() {
    // 刚换来的 token 也被拒：问题不在 token。每个请求都去换的话，refresh token 每次轮换一个
    let tokens = Arc::new(TokenServer::default());
    let token_url = start_token_server(tokens.clone()).await;
    let backend = Arc::new(Backend {
        reject_all: true,
        ..Default::default()
    });
    let base = start_backend(backend.clone()).await;
    let (gw, _, _rx) = start_gateway(chatgpt_provider(&base, &token_url)).await;

    for _ in 0..3 {
        let (status, _, body) = post_json(
            gw,
            "/v1/responses",
            &[("authorization", "Bearer tw-k")],
            codex_request(true),
        )
        .await;
        assert_eq!(status, 401, "{body}");
    }
    assert_eq!(
        tokens.calls.load(Ordering::SeqCst),
        1,
        "只在第一次拿 token 时换过"
    );
    assert_eq!(
        backend.calls.load(Ordering::SeqCst),
        3,
        "同一个 token 不重发"
    );
}

#[tokio::test]
async fn concurrent_requests_refresh_the_token_only_once() {
    // refresh token 只能用一次：两个请求同时拿它去刷，后到的会让整条登录作废
    let tokens = Arc::new(TokenServer {
        delay_ms: 300,
        ..Default::default()
    });
    let token_url = start_token_server(tokens.clone()).await;
    let backend = Arc::new(Backend::default());
    let base = start_backend(backend.clone()).await;
    let (gw, _, _rx) = start_gateway(chatgpt_provider(&base, &token_url)).await;

    let asks = (0..5).map(|_| {
        post_json(
            gw,
            "/v1/responses",
            &[("authorization", "Bearer tw-k")],
            codex_request(true),
        )
    });
    for (status, _, body) in futures::future::join_all(asks).await {
        assert_eq!(status, 200, "{body}");
    }
    assert_eq!(
        tokens.calls.load(Ordering::SeqCst),
        1,
        "并发请求只该刷新一次"
    );
}

#[tokio::test]
async fn a_revoked_login_fails_fast_and_is_reported_once() {
    let tokens = Arc::new(TokenServer {
        revoked: true,
        ..Default::default()
    });
    let token_url = start_token_server(tokens.clone()).await;
    let backend = Arc::new(Backend::default());
    let base = start_backend(backend.clone()).await;
    let (gw, state, mut rx) = start_gateway(chatgpt_provider(&base, &token_url)).await;

    for _ in 0..3 {
        let (status, _, body) = post_json(
            gw,
            "/v1/responses",
            &[("authorization", "Bearer tw-k")],
            codex_request(true),
        )
        .await;
        assert_ne!(status, 200, "{body}");
    }
    assert_eq!(
        tokens.calls.load(Ordering::SeqCst),
        1,
        "作废的 refresh token 不该每个请求都去撞一次 token 端点"
    );
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    let expired: Vec<_> = drain(&mut rx)
        .await
        .into_iter()
        .filter(|e| matches!(e, tw_api::Event::CredentialExpired { .. }))
        .collect();
    assert_eq!(expired.len(), 1, "只在失效的那一刻报一次：{expired:?}");

    let cfg = state.config();
    let p = &cfg.providers[0];
    let (why, relogin) = state
        .oauth
        .failure(&p.name, p.oauth.as_ref().unwrap())
        .expect("失败要能查到");
    assert!(relogin);
    assert!(why.contains("invalid_grant"), "{why}");
}

#[tokio::test]
async fn an_exhausted_window_is_reported_once_and_the_429_reaches_the_client() {
    let tokens = Arc::new(TokenServer::default());
    let token_url = start_token_server(tokens.clone()).await;
    let backend = Arc::new(Backend {
        exhausted: true,
        ..Default::default()
    });
    let base = start_backend(backend.clone()).await;
    let (gw, _, mut rx) = start_gateway(chatgpt_provider(&base, &token_url)).await;

    for _ in 0..2 {
        let (status, _, body) = post_json(
            gw,
            "/v1/responses",
            &[("authorization", "Bearer tw-k")],
            codex_request(true),
        )
        .await;
        assert_eq!(status, 429, "{body}");
    }
    let exhausted: Vec<_> = drain(&mut rx)
        .await
        .into_iter()
        .filter_map(|e| match e {
            tw_api::Event::QuotaExhausted {
                window,
                reset_in_secs,
                ..
            } => Some((window, reset_in_secs)),
            _ => None,
        })
        .collect();
    assert_eq!(exhausted, vec![("weekly".to_string(), Some(3600))]);
}

#[tokio::test]
async fn models_come_from_the_codex_models_endpoint() {
    let backend = Arc::new(Backend::default());
    let base = start_backend(backend.clone()).await;
    let headers = vec![("authorization".to_string(), "Bearer at".to_string())];
    let got = tw_gateway::probe(
        &reqwest::Client::new(),
        &base,
        &headers,
        Some(Protocol::Chatgpt),
    )
    .await;
    assert!(got.ok, "{got:?}");
    match &got.models {
        tw_gateway::ModelList::Listed { models } => {
            assert_eq!(models, &vec!["gpt-5.5".to_string()], "后端隐藏的模型不列")
        }
        other => panic!("{other:?}"),
    }
    let uri = backend.seen.lock().unwrap()[0].0.clone();
    assert_eq!(uri, "/backend-api/codex/models?client_version=0.0.0");
}
