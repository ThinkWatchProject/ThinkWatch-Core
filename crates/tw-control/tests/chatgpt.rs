//! ChatGPT 账号：浏览器登录、用量、额度重置卡、删除时吊销。
//!
//! **全程只对本机的假 OpenAI**：登录页、token 端点、吊销接口和 ChatGPT 后端都是假的。
//! 重置卡用掉就回不来，测试绝不能碰真实账号。
//!
//! 浏览器那一步由测试自己访问回调地址来模拟。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Form, FromRequest, OriginalUri, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::Digest;
use tower::ServiceExt;
use tw_control::chatgpt::{Accounts, Endpoints};
use tw_control::{ConfigManager, ControlState};
use tw_gateway::chatgpt::CLIENT_ID;

// ---------------------------------------------------------------- 假 OpenAI

#[derive(Default)]
struct OpenAi {
    /// token 端点收到的请求：登录换 token 是表单，刷新是 JSON，一律摊成键值
    token_forms: Mutex<Vec<HashMap<String, String>>>,
    /// ChatGPT 后端拒绝这个 access token（它在别处被吊销了）
    reject_access: Mutex<Option<String>>,
    /// 吊销接口收到的请求体
    revoked: Mutex<Vec<Value>>,
    /// ChatGPT 后端收到的 (路径和查询串, 请求头)
    backend: Mutex<Vec<(String, HeaderMap)>>,
    /// 用卡接口收到的请求体
    consumed: Mutex<Vec<Value>>,
    /// 这个账号不能用设备码登录：换码回 404
    device_closed: Mutex<bool>,
    /// 问到第几次才算批准。0 就是第一次问就批
    device_approve_after: Mutex<u32>,
    /// 设备码的两个接口收到的请求体
    device_asks: Mutex<Vec<Value>>,
}

fn jwt(claims: Value) -> String {
    format!(
        "eyJhbGciOiJub25lIn0.{}.sig",
        URL_SAFE_NO_PAD.encode(claims.to_string())
    )
}

async fn token(
    State(o): State<Arc<OpenAi>>,
    headers: HeaderMap,
    body: String,
) -> axum::response::Response {
    let json_body = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/json"));
    let f: HashMap<String, String> = if json_body {
        serde_json::from_str::<HashMap<String, Value>>(&body)
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k, v.as_str().unwrap_or_default().to_string()))
            .collect()
    } else {
        let Form(f) = Form::<HashMap<String, String>>::from_request(
            Request::builder()
                .method("POST")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
            &(),
        )
        .await
        .unwrap();
        f
    };
    let refresh = f.get("grant_type").map(String::as_str) == Some("refresh_token");
    let bad = f.get("code").map(String::as_str) == Some("bad");
    let slow = f.get("code").map(String::as_str) == Some("slow");
    o.token_forms.lock().unwrap().push(f);
    if slow {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    if bad {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "invalid_grant", "error_description": "Invalid authorization code"})),
        )
            .into_response();
    }
    if refresh {
        return axum::Json(json!({
            "access_token": "at-refreshed",
            "refresh_token": "rt-rotated",
            "expires_in": 864000
        }))
        .into_response();
    }
    axum::Json(json!({
        "id_token": jwt(json!({
            "email": "someone@example.com",
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-1", "chatgpt_plan_type": "plus"}
        })),
        "access_token": "at-login",
        "refresh_token": "rt-login",
        "expires_in": 864000
    }))
    .into_response()
}

/// 换一个一次性码。**没开放这条路的账号回 404**
async fn device_code(
    State(o): State<Arc<OpenAi>>,
    axum::Json(v): axum::Json<Value>,
) -> axum::response::Response {
    o.device_asks.lock().unwrap().push(v);
    if *o.device_closed.lock().unwrap() {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"detail": "Not Found"})),
        )
            .into_response();
    }
    axum::Json(json!({"device_auth_id": "dev-1", "user_code": "ABCD-1234", "interval": 1}))
        .into_response()
}

/// 批准了没有。**没批准是 403**，批准了才给授权码和服务端生成的 verifier
async fn device_token(
    State(o): State<Arc<OpenAi>>,
    axum::Json(v): axum::Json<Value>,
) -> axum::response::Response {
    let asked = {
        let mut asks = o.device_asks.lock().unwrap();
        asks.push(v);
        asks.iter().filter(|a| a.get("user_code").is_some()).count() as u32
    };
    if asked <= *o.device_approve_after.lock().unwrap() {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({"detail": "Forbidden"})),
        )
            .into_response();
    }
    axum::Json(json!({
        "authorization_code": "code-from-device",
        "code_challenge": "challenge-from-server",
        "code_verifier": "verifier-from-server",
    }))
    .into_response()
}

async fn start_openai(o: Arc<OpenAi>) -> Endpoints {
    fn seen(o: &OpenAi, uri: &OriginalUri, headers: HeaderMap) {
        o.backend.lock().unwrap().push((uri.0.to_string(), headers));
    }
    let app = axum::Router::new()
        .route("/oauth/token", post(token))
        .route("/api/accounts/deviceauth/usercode", post(device_code))
        .route("/api/accounts/deviceauth/token", post(device_token))
        .route(
            "/oauth/revoke",
            post(|State(o): State<Arc<OpenAi>>, axum::Json(v): axum::Json<Value>| async move {
                o.revoked.lock().unwrap().push(v);
                axum::Json(json!({}))
            }),
        )
        .route(
            "/backend-api/codex/models",
            get(|State(o): State<Arc<OpenAi>>, uri: OriginalUri, h: HeaderMap| async move {
                seen(&o, &uri, h);
                axum::Json(json!({"models": [{"slug": "gpt-5.5", "visibility": "list", "supported_in_api": true}]}))
            }),
        )
        .route(
            "/backend-api/wham/usage",
            get(|State(o): State<Arc<OpenAi>>, uri: OriginalUri, h: HeaderMap| async move {
                let rejected = o.reject_access.lock().unwrap().as_ref().is_some_and(|t| {
                    h.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {t}"))
                });
                seen(&o, &uri, h);
                if rejected {
                    return (StatusCode::UNAUTHORIZED, axum::Json(json!({"detail": "Unauthorized"})));
                }
                (StatusCode::OK, axum::Json(json!({
                    "user_id": "user-1",
                    "account_id": "acct-1",
                    "email": "someone@example.com",
                    "plan_type": "plus",
                    "rate_limit": {
                        "allowed": true,
                        "limit_reached": false,
                        "primary_window": {"used_percent": 21, "limit_window_seconds": 604800, "reset_after_seconds": 410907, "reset_at": 1790000000},
                        "secondary_window": {"used_percent": 0, "limit_window_seconds": 0, "reset_after_seconds": 0, "reset_at": 0}
                    },
                    "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
                    "rate_limit_reset_credits": {"available_count": 2}
                })))
            }),
        )
        .route(
            "/backend-api/wham/rate-limit-reset-credits",
            get(|State(o): State<Arc<OpenAi>>, uri: OriginalUri, h: HeaderMap| async move {
                seen(&o, &uri, h);
                axum::Json(json!({
                    "credits": [
                        {"id": "cr-1", "reset_type": "rate_limit", "status": "available", "granted_at": "2026-09-01T00:00:00Z", "expires_at": "2026-12-01T00:00:00Z", "title": "Rate limit reset"},
                        {"id": "cr-0", "reset_type": "rate_limit", "status": "redeemed", "granted_at": "2026-08-01T00:00:00Z", "expires_at": null, "title": null, "description": null}
                    ],
                    "available_count": 1
                }))
            }),
        )
        .route(
            "/backend-api/wham/rate-limit-reset-credits/consume",
            post(
                |State(o): State<Arc<OpenAi>>, uri: OriginalUri, h: HeaderMap, axum::Json(v): axum::Json<Value>| async move {
                    seen(&o, &uri, h);
                    o.consumed.lock().unwrap().push(v);
                    axum::Json(json!({"code": "reset", "windows_reset": 1}))
                },
            ),
        )
        .with_state(o);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Endpoints {
        issuer: format!("http://{addr}"),
        token: format!("http://{addr}/oauth/token"),
        revoke: format!("http://{addr}/oauth/revoke"),
        backend: format!("http://{addr}/backend-api/codex"),
        // 系统挑空闲端口：并行跑的测试各用各的
        ports: vec![0],
    }
}

// ---------------------------------------------------------------- 控制面

const RELAY: &str = "version: 1
clients:
  - name: c
    key: tw-k
providers:
  - name: relay
    base_url: https://relay.example
    key: sk-relay
    protocol: anthropic
";

/// 已经登录过的账号。access token 还有很久才过期，用它不用先换
fn logged_in(name: &str, e: &Endpoints, refresh: &str) -> String {
    format!(
        "  - name: {name}
    base_url: {backend}
    protocol: chatgpt
    billing: subscription
    oauth:
      access: at-cfg
      expires_at: 2099-01-01T00:00:00Z
      refresh: {refresh}
      endpoint: {token}
      client_id: {CLIENT_ID}
    headers:
      ChatGPT-Account-Id: acct-1
",
        backend = e.backend,
        token = e.token,
    )
}

struct Bed {
    dir: tempfile::TempDir,
    app: axum::Router,
    events: tokio::sync::broadcast::Receiver<tw_api::Event>,
    openai: Arc<OpenAi>,
    endpoints: Endpoints,
}

impl Bed {
    fn file(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("config.yaml")).unwrap()
    }
    fn provider(&self, name: &str) -> Option<tw_config::Provider> {
        tw_config::try_parse(&self.file())
            .unwrap()
            .providers
            .into_iter()
            .find(|p| p.name == name)
    }
    async fn call(&self, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let r = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let st = r.status();
        let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let text = String::from_utf8_lossy(&b).to_string();
        (
            st,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }
    /// 等下一条登录结果事件
    async fn login_finished(&mut self) -> (String, String, Option<String>) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(tw_api::Event::LoginFinished {
                    login,
                    status,
                    provider,
                    ..
                }) = self.events.recv().await
                {
                    return (login, status, provider);
                }
            }
        })
        .await
        .expect("5 秒内应当收到登录结果")
    }
}

/// `extra` 追加在 providers 之下，拿得到假 OpenAI 的地址
async fn bed(extra: impl FnOnce(&Endpoints) -> String) -> Bed {
    let openai = Arc::new(OpenAi::default());
    let endpoints = start_openai(openai.clone()).await;
    let yaml = format!("{RELAY}{}", extra(&endpoints));
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, &yaml).unwrap();
    let cfg = tw_config::try_parse(&yaml).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let events = bus.subscribe();
    let state = ControlState {
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        store: None,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Arc::new(Accounts::new(endpoints.clone())),
        home: d.path().join("home"),
    };
    Bed {
        app: tw_control::router(state),
        dir: d,
        events,
        openai,
        endpoints,
    }
}

/// 浏览器：授权完成后跳回回调地址
async fn browser(url: &str) -> (u16, String) {
    let r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(url.replace("://localhost:", "://127.0.0.1:"))
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

struct Started {
    id: String,
    redirect_uri: String,
    state: String,
    challenge: String,
    query: HashMap<String, String>,
}

async fn start_login(b: &Bed, body: Value) -> Started {
    let (st, v) = b.call("POST", "/chatgpt/login", body).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let url = reqwest::Url::parse(v["authorize_url"].as_str().unwrap()).unwrap();
    assert_eq!(
        url.as_str().split('?').next().unwrap(),
        format!("{}/oauth/authorize", b.endpoints.issuer)
    );
    let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
    Started {
        id: v["id"].as_str().unwrap().to_string(),
        redirect_uri: query["redirect_uri"].clone(),
        state: query["state"].clone(),
        challenge: query["code_challenge"].clone(),
        query,
    }
}

async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..100 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("2 秒内没有等到：{what}");
}

// ---------------------------------------------------------------- 登录

#[tokio::test]
async fn a_browser_login_writes_the_account_into_the_config_and_hands_back_to_the_app() {
    let mut b = bed(|_| String::new()).await;
    let s = start_login(&b, json!({"return_to": "thinkwatch://chatgpt/login"})).await;

    // 登录页上说明是谁：授权码 + PKCE，来源是 ThinkWatch
    assert_eq!(s.query["response_type"], "code");
    assert_eq!(s.query["client_id"], CLIENT_ID);
    assert_eq!(s.query["code_challenge_method"], "S256");
    assert_eq!(s.query["originator"], "thinkwatch");
    assert!(s.query["scope"].split(' ').any(|x| x == "offline_access"));
    assert!(
        s.redirect_uri.starts_with("http://localhost:")
            && s.redirect_uri.ends_with("/auth/callback"),
        "{}",
        s.redirect_uri
    );
    let (st, v) = b
        .call("GET", &format!("/chatgpt/login/{}", s.id), Value::Null)
        .await;
    assert_eq!(
        (st, v["status"].as_str()),
        (StatusCode::OK, Some("pending"))
    );

    let (code, page) = browser(&format!("{}?code=code-1&state={}", s.redirect_uri, s.state)).await;
    assert_eq!(code, 200);
    assert!(page.contains("Signed in to ChatGPT"), "{page}");
    assert!(
        page.contains("thinkwatch://chatgpt/login"),
        "登录完成后要能回到应用：{page}"
    );

    // 换 token 用的是发出去的那一对 PKCE 和回调地址
    let form = b.openai.token_forms.lock().unwrap()[0].clone();
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], "code-1");
    assert_eq!(form["client_id"], CLIENT_ID);
    assert_eq!(form["redirect_uri"], s.redirect_uri);
    assert_eq!(
        URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(form["code_verifier"].as_bytes())),
        s.challenge
    );

    let (st, v) = b
        .call("GET", &format!("/chatgpt/login/{}", s.id), Value::Null)
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["status"], "done");
    assert_eq!(v["provider"], "chatgpt");
    assert_eq!(v["plan"], "plus");
    assert_eq!(
        b.login_finished().await,
        (s.id.clone(), "done".into(), Some("chatgpt".into()))
    );

    // 凭据和别的上游一样在配置里：refresh、access、过期时间、账户 ID
    let p = b.provider("chatgpt").expect("登录之后配置里应当有这个上游");
    assert_eq!(p.effective_protocol(), Some(tw_config::Protocol::Chatgpt));
    assert_eq!(p.base_url, b.endpoints.backend);
    assert_eq!(p.billing, Some(tw_config::Billing::Subscription));
    let o = p.oauth.as_ref().unwrap();
    assert_eq!(o.refresh, "rt-login");
    assert_eq!(o.access.as_deref(), Some("at-login"));
    assert_eq!(o.endpoint, b.endpoints.token);
    assert_eq!(o.client_id.as_deref(), Some(CLIENT_ID));
    let expires = chrono::DateTime::parse_from_rfc3339(o.expires_at.as_deref().unwrap()).unwrap();
    let left = expires.with_timezone(&chrono::Utc) - chrono::Utc::now();
    assert!(
        (left.num_seconds() - 864000).abs() < 60,
        "过期时间应当是十天后：{expires}"
    );
    assert_eq!(
        p.headers.get("ChatGPT-Account-Id").unwrap().value.raw(),
        "acct-1"
    );
    assert!(b.provider("relay").is_some(), "别的上游不动");

    // 模型清单马上就去问，不等下一轮
    eventually("登录后获取模型清单", || {
        b.openai
            .backend
            .lock()
            .unwrap()
            .iter()
            .any(|(path, _)| path.starts_with("/backend-api/codex/models?client_version="))
    })
    .await;
}

#[tokio::test]
async fn a_callback_that_arrives_twice_exchanges_the_code_once() {
    let b = bed(|_| String::new()).await;
    let s = start_login(&b, json!({})).await;
    // 浏览器有时会把同一个跳转发两次。token 端点慢一点，让两次挤在一起
    let url = format!("{}?code=slow&state={}", s.redirect_uri, s.state);
    let ((_, one), (_, two)) = tokio::join!(browser(&url), browser(&url));
    assert!(one.contains("Signed in to ChatGPT"), "{one}");
    assert_eq!(one, two);
    assert_eq!(b.openai.token_forms.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn the_callback_server_stops_once_the_login_is_settled() {
    let b = bed(|_| String::new()).await;
    let s = start_login(&b, json!({})).await;
    let (_, page) = browser(&format!("{}?code=code-1&state={}", s.redirect_uri, s.state)).await;
    assert!(page.contains("Signed in to ChatGPT"), "{page}");
    let addr = s
        .redirect_uri
        .trim_start_matches("http://localhost:")
        .split('/')
        .next()
        .unwrap()
        .to_string();
    let mut closed = false;
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(format!("127.0.0.1:{addr}"))
            .await
            .is_err()
        {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(closed, "登录完成后回调端口应当让出来");
}

#[tokio::test]
async fn a_callback_with_the_wrong_state_does_not_end_the_login() {
    let mut b = bed(|_| String::new()).await;
    let s = start_login(&b, json!({})).await;

    // 别的网页来试探这个端口：不理它，登录还在等
    let (_, page) = browser(&format!("{}?code=stolen&state=guess", s.redirect_uri)).await;
    assert!(page.contains("This sign-in link has expired"), "{page}");
    let (_, v) = b
        .call("GET", &format!("/chatgpt/login/{}", s.id), Value::Null)
        .await;
    assert_eq!(v["status"], "pending");
    assert!(b.openai.token_forms.lock().unwrap().is_empty());

    // 用户在授权页上拒绝了
    let (_, page) = browser(&format!(
        "{}?error=access_denied&error_description=The+user+denied+access&state={}",
        s.redirect_uri, s.state
    ))
    .await;
    assert!(
        page.contains("The ChatGPT sign-in did not finish"),
        "{page}"
    );
    let (_, v) = b
        .call("GET", &format!("/chatgpt/login/{}", s.id), Value::Null)
        .await;
    assert_eq!(v["status"], "failed");
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("The user denied access"),
        "{v}"
    );
    assert_eq!(b.login_finished().await, (s.id, "failed".into(), None));
    assert!(b.provider("chatgpt").is_none());
}

#[tokio::test]
async fn a_rejected_code_fails_the_login_and_leaves_the_config_alone() {
    let mut b = bed(|_| String::new()).await;
    let before = b.file();
    let s = start_login(&b, json!({})).await;
    let (_, page) = browser(&format!("{}?code=bad&state={}", s.redirect_uri, s.state)).await;
    assert!(
        page.contains("The ChatGPT sign-in did not finish"),
        "{page}"
    );
    assert!(page.contains("400"), "{page}");
    let (_, v) = b
        .call("GET", &format!("/chatgpt/login/{}", s.id), Value::Null)
        .await;
    assert_eq!(v["status"], "failed");
    assert!(
        !v["error"].as_str().unwrap().contains("bad"),
        "授权码不进错误信息：{v}"
    );
    assert_eq!(b.login_finished().await.1, "failed");
    assert_eq!(b.file(), before);
}

#[tokio::test]
async fn logging_in_again_replaces_the_credentials_and_keeps_the_settings() {
    let b = bed(|e| {
        logged_in("chatgpt", e, "rt-old").replace(
            "    billing: subscription\n",
            "    billing: subscription\n    models_only: [gpt-5.5]\n",
        )
    })
    .await;
    let s = start_login(&b, json!({"name": "chatgpt"})).await;
    let (_, page) = browser(&format!("{}?code=code-1&state={}", s.redirect_uri, s.state)).await;
    assert!(page.contains("Signed in to ChatGPT"), "{page}");
    let p = b.provider("chatgpt").unwrap();
    let o = p.oauth.as_ref().unwrap();
    assert_eq!(
        (o.refresh.as_str(), o.access.as_deref()),
        ("rt-login", Some("at-login"))
    );
    assert_eq!(p.models_only.as_deref(), Some(&["gpt-5.5".to_string()][..]));
    assert_eq!(p.headers.len(), 1, "账户 ID 只留一个");
    assert_eq!(
        p.headers.get("chatgpt-account-id").unwrap().value.raw(),
        "acct-1"
    );
}

#[tokio::test]
async fn a_login_that_cannot_be_saved_is_refused_before_the_browser_opens() {
    let b = bed(|_| String::new()).await;
    let (st, v) = b
        .call("POST", "/chatgpt/login", json!({"name": "relay"}))
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    let (st, v) = b
        .call("POST", "/chatgpt/login", json!({"proxy": "nowhere"}))
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    // 跳转地址只能回应用，不能是网页：否则就是一个开放跳转
    for bad in [
        "https://evil.example/",
        "javascript:alert(1)",
        "file:///etc/passwd",
    ] {
        let (st, v) = b
            .call("POST", "/chatgpt/login", json!({"return_to": bad}))
            .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{bad}: {v}");
    }
}

#[tokio::test]
async fn a_new_login_replaces_the_one_still_waiting_and_a_login_can_be_cancelled() {
    let mut b = bed(|_| String::new()).await;
    let first = start_login(&b, json!({})).await;
    let second = start_login(&b, json!({})).await;
    assert_eq!(
        b.login_finished().await,
        (first.id.clone(), "cancelled".into(), None)
    );
    let (st, _) = b
        .call("GET", &format!("/chatgpt/login/{}", first.id), Value::Null)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // 被替代的那次，回调已经没人接了
    let stale = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(
            format!("{}?code=code-1&state={}", first.redirect_uri, first.state)
                .replace("://localhost:", "://127.0.0.1:"),
        )
        .send()
        .await;
    assert!(stale.is_err() || first.redirect_uri == second.redirect_uri);
    assert!(b.openai.token_forms.lock().unwrap().is_empty());

    let (st, v) = b
        .call(
            "DELETE",
            &format!("/chatgpt/login/{}", second.id),
            Value::Null,
        )
        .await;
    assert_eq!(
        (st, v["status"].as_str()),
        (StatusCode::OK, Some("cancelled"))
    );
    assert_eq!(
        b.login_finished().await,
        (second.id.clone(), "cancelled".into(), None)
    );
    let (_, v) = b
        .call("GET", &format!("/chatgpt/login/{}", second.id), Value::Null)
        .await;
    assert_eq!(v["status"], "cancelled");
}

// ---------------------------------------------------------------- 设备码登录

#[tokio::test]
async fn a_device_login_waits_for_the_other_device_and_saves_the_account() {
    let mut b = bed(|_| String::new()).await;
    // 第一次问的时候还没批，要接着等
    *b.openai.device_approve_after.lock().unwrap() = 1;
    let (st, v) = b
        .call("POST", "/chatgpt/login", json!({"mode": "device"}))
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    // 给用户的是码和一个地址，这台机器上不开浏览器
    let id = v["id"].as_str().unwrap().to_string();
    assert_eq!(v["user_code"], "ABCD-1234");
    assert_eq!(
        v["verification_url"],
        json!(format!("{}/codex/device", b.endpoints.issuer))
    );
    assert!(v["authorize_url"].is_null(), "{v}");
    let (_, v) = b
        .call("GET", &format!("/chatgpt/login/{id}"), Value::Null)
        .await;
    assert_eq!(v["status"], "pending");

    assert_eq!(
        b.login_finished().await,
        (id.clone(), "done".into(), Some("chatgpt".into()))
    );

    // 换码时如实说明自己是谁
    let asked = b.openai.device_asks.lock().unwrap().clone();
    assert_eq!(asked[0]["client_id"], CLIENT_ID);
    // 之后每次都带着这一次的设备码去问
    assert!(asked.len() >= 3, "问了几次：{}", asked.len());
    for ask in &asked[1..] {
        assert_eq!(ask["device_auth_id"], "dev-1");
        assert_eq!(ask["user_code"], "ABCD-1234");
    }

    // 换 token 用服务端给的 verifier 和设备码那条路的回调地址
    let form = b.openai.token_forms.lock().unwrap()[0].clone();
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], "code-from-device");
    assert_eq!(form["code_verifier"], "verifier-from-server");
    assert_eq!(
        form["redirect_uri"],
        format!("{}/deviceauth/callback", b.endpoints.issuer)
    );

    // 之后和浏览器登录完全一样：凭据进配置，模型清单马上问
    let p = b.provider("chatgpt").expect("登录之后配置里应当有这个上游");
    assert_eq!(p.effective_protocol(), Some(tw_config::Protocol::Chatgpt));
    assert_eq!(p.oauth.as_ref().unwrap().refresh, "rt-login");
    assert_eq!(
        p.headers.get("ChatGPT-Account-Id").unwrap().value.raw(),
        "acct-1"
    );
    let (_, v) = b
        .call("GET", &format!("/chatgpt/login/{id}"), Value::Null)
        .await;
    assert_eq!(v["plan"], "plus");
    eventually("登录后获取模型清单", || {
        b.openai
            .backend
            .lock()
            .unwrap()
            .iter()
            .any(|(path, _)| path.starts_with("/backend-api/codex/models?client_version="))
    })
    .await;
}

#[tokio::test]
async fn an_account_without_device_login_is_told_to_use_the_browser() {
    let b = bed(|_| String::new()).await;
    *b.openai.device_closed.lock().unwrap() = true;
    let (st, v) = b
        .call("POST", "/chatgpt/login", json!({"mode": "device"}))
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert!(
        v.to_string().contains("Sign in on this computer"),
        "要告诉用户改用浏览器登录：{v}"
    );
    assert!(b.provider("chatgpt").is_none());
}

#[tokio::test]
async fn a_login_mode_that_is_not_understood_is_refused() {
    let b = bed(|_| String::new()).await;
    let (st, _) = b
        .call("POST", "/chatgpt/login", json!({"mode": "carrier-pigeon"}))
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------- 用量与重置卡

#[tokio::test]
async fn usage_shows_the_limits_without_personal_details() {
    let b = bed(|e| logged_in("chatgpt", e, "rt-cfg")).await;
    // 还没有请求经过，额度就还不知道
    let (_, v) = b.call("GET", "/quota", Value::Null).await;
    assert_eq!(v, json!([]));
    let (st, v) = b
        .call("GET", "/providers/chatgpt/chatgpt/usage", Value::Null)
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["plan"], "plus");
    assert_eq!(v["reset_credits"], 2);
    assert_eq!(
        v["windows"],
        json!([{"window": "weekly", "used_percent": 21.0, "reset_in_secs": 410907}]),
        "长度为 0 的窗口不算"
    );
    // 邮箱要给：账号不止一个时，它是用户分辨哪个是哪个的唯一一项
    assert_eq!(v["email"], "someone@example.com");
    // 用户 ID 和账户 ID 不给：界面读不出是谁，而它们一旦出去就会进日志
    let text = v.to_string();
    assert!(
        !text.contains("user-1") && !text.contains("acct-1"),
        "{text}"
    );

    // 问来的额度就是这个上游的额度：冷启动之后界面不用等第一次请求
    let (_, q) = b.call("GET", "/quota", Value::Null).await;
    assert_eq!(q[0]["provider"], "chatgpt");
    assert_eq!(q[0]["windows"], v["windows"]);

    // 如实说明是谁，带着配置里的 access token 和账户 ID；不是对话，不带会话 ID
    let seen = b.openai.backend.lock().unwrap();
    let (path, h) = seen.last().unwrap();
    assert_eq!(path, "/backend-api/wham/usage");
    let header = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("");
    assert_eq!(header("authorization"), "Bearer at-cfg");
    assert_eq!(header("chatgpt-account-id"), "acct-1");
    assert_eq!(header("originator"), "thinkwatch");
    assert_eq!(header("user-agent"), tw_gateway::chatgpt::user_agent());
    assert!(h.get("session-id").is_none());
    assert!(
        b.openai.token_forms.lock().unwrap().is_empty(),
        "access token 没过期，不用换"
    );
}

#[tokio::test]
async fn a_revoked_access_token_is_renewed_once_and_the_call_goes_through() {
    let b = bed(|e| logged_in("chatgpt", e, "rt-cfg")).await;
    *b.openai.reject_access.lock().unwrap() = Some("at-cfg".into());
    let (st, v) = b
        .call("GET", "/providers/chatgpt/chatgpt/usage", Value::Null)
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let auth: Vec<String> = b
        .openai
        .backend
        .lock()
        .unwrap()
        .iter()
        .map(|(_, h)| h["authorization"].to_str().unwrap().to_string())
        .collect();
    assert_eq!(auth, ["Bearer at-cfg", "Bearer at-refreshed"]);
    let forms = b.openai.token_forms.lock().unwrap();
    assert_eq!(forms.len(), 1);
    assert_eq!(forms[0]["grant_type"], "refresh_token");
    assert_eq!(forms[0]["refresh_token"], "rt-cfg");
}

#[tokio::test]
async fn only_a_chatgpt_account_has_usage_and_reset_credits() {
    let b = bed(|_| String::new()).await;
    let (st, _) = b
        .call("GET", "/providers/relay/chatgpt/usage", Value::Null)
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = b
        .call("GET", "/providers/nobody/chatgpt/resets", Value::Null)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = b
        .call(
            "POST",
            "/providers/relay/chatgpt/resets",
            json!({"idempotency_key": "op-1"}),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(b.openai.consumed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reset_credits_are_listed_and_used_only_when_asked() {
    let b = bed(|e| logged_in("chatgpt", e, "rt-cfg")).await;

    let (st, v) = b
        .call("GET", "/providers/chatgpt/chatgpt/resets", Value::Null)
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["available_count"], 1);
    assert_eq!(v["credits"][0]["id"], "cr-1");
    assert_eq!(v["credits"][0]["expires_at"], "2026-12-01T00:00:00Z");
    assert_eq!(v["credits"][1]["status"], "redeemed");
    assert!(
        b.openai.consumed.lock().unwrap().is_empty(),
        "看清单不能用掉卡"
    );

    // 没有幂等键不发：重试时后端靠它认出同一次操作
    for key in ["", "   "] {
        let (st, _) = b
            .call(
                "POST",
                "/providers/chatgpt/chatgpt/resets",
                json!({"idempotency_key": key}),
            )
            .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }
    assert!(b.openai.consumed.lock().unwrap().is_empty());

    let (st, v) = b
        .call(
            "POST",
            "/providers/chatgpt/chatgpt/resets",
            json!({"idempotency_key": "op-1", "credit_id": "cr-1"}),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v, json!({"code": "reset", "windows_reset": 1}));
    let (st, _) = b
        .call(
            "POST",
            "/providers/chatgpt/chatgpt/resets",
            json!({"idempotency_key": "op-2"}),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        *b.openai.consumed.lock().unwrap(),
        vec![
            json!({"redeem_request_id": "op-1", "credit_id": "cr-1"}),
            json!({"redeem_request_id": "op-2"}),
        ]
    );
    let seen = b.openai.backend.lock().unwrap();
    let (_, h) = seen.last().unwrap();
    assert_eq!(h.get("originator").unwrap(), "thinkwatch");
}

// ---------------------------------------------------------------- 删除时吊销

#[tokio::test]
async fn deleting_a_logged_in_account_revokes_its_login() {
    let b = bed(|e| logged_in("chatgpt", e, "rt-cfg")).await;
    let (st, v) = b.call("DELETE", "/providers/chatgpt", Value::Null).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(b.provider("chatgpt").is_none());
    eventually("吊销 refresh token", || {
        !b.openai.revoked.lock().unwrap().is_empty()
    })
    .await;
    assert_eq!(
        b.openai.revoked.lock().unwrap()[0],
        json!({"token": "rt-cfg", "token_type_hint": "refresh_token", "client_id": CLIENT_ID})
    );
}

#[tokio::test]
async fn a_login_still_used_by_another_upstream_is_not_revoked() {
    let b = bed(|e| {
        let other = format!(
            "  - name: company-sso
    base_url: https://llm.example
    protocol: openai-chat
    oauth:
      refresh: rt-sso
      endpoint: {}
      client_id: {CLIENT_ID}
",
            e.token
        );
        format!(
            "{}{}{other}",
            logged_in("chatgpt", e, "rt-shared"),
            logged_in("chatgpt-copy", e, "rt-shared")
        )
    })
    .await;
    // 复制出来的上游共用同一份登录
    let (st, v) = b
        .call("DELETE", "/providers/chatgpt-copy", Value::Null)
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // 不是 ChatGPT 账号上游
    let (st, v) = b
        .call("DELETE", "/providers/company-sso", Value::Null)
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(b.openai.revoked.lock().unwrap().is_empty());
}
