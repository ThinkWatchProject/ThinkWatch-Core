//! Z.ai / BigModel 账号：登录换一把 API key，写成一条 anthropic 上游。
//!
//! **全程只对本机的假 Z.ai**：登录接口、业务接口、建密钥的接口都是假的。浏览器那一步
//! 不需要模拟 —— 授权回的是对方的服务端，这边只有轮询。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::{get, post};
use serde_json::{Value, json};
use tower::ServiceExt;
use tw_control::zai::{Accounts, Endpoints};
use tw_control::{ConfigManager, ControlState};

// ---------------------------------------------------------------- 假 Z.ai

#[derive(Default)]
struct Zai {
    /// 还要回几次 `pending` 才算授权完
    pending_left: Mutex<u32>,
    /// 轮询回一个「HTTP 200 包着业务错误码」的回包
    poll_business_error: Mutex<bool>,
    /// 账号下已经有的密钥
    keys: Mutex<Vec<Value>>,
    /// `copy` 接口不给 secret 那一半
    no_secret: Mutex<bool>,
    /// 各接口收到的 (路径, 请求头, 请求体)
    seen: Mutex<Vec<(String, HeaderMap, Value)>>,
}

impl Zai {
    fn note(&self, path: &str, headers: HeaderMap, body: Value) {
        self.seen
            .lock()
            .unwrap()
            .push((path.to_string(), headers, body));
    }
    fn auth_of(&self, path: &str) -> Option<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _, _)| p == path)
            .and_then(|(_, h, _)| h.get("authorization").cloned())
            .and_then(|v| v.to_str().ok().map(str::to_string))
    }
    fn hit(&self, path: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _, _)| p == path)
            .count()
    }
    fn body_of(&self, path: &str) -> Option<Value> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _, _)| p == path)
            .map(|(_, _, b)| b.clone())
    }
}

/// 它们的回包外壳：业务码在 `code` 里，成功是 0
fn wrapped(data: Value) -> axum::Json<Value> {
    axum::Json(json!({ "code": 0, "msg": "", "data": data }))
}

async fn init(
    State(z): State<Arc<Zai>>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> axum::Json<Value> {
    z.note("/api/v1/oauth/cli/init", headers, body);
    wrapped(json!({
        "flow_id": "flow-1",
        "authorize_url": "https://chat.example/api/oauth/authorize?client_id=theirs",
        // 绝对时刻，秒
        "expires_at": 4_000_000_000u64,
        "poll_interval_sec": 1,
        "poll_token": "the-server-echo",
    }))
}

async fn poll(
    State(z): State<Arc<Zai>>,
    Path(flow): Path<String>,
    headers: HeaderMap,
) -> axum::Json<Value> {
    z.note("/api/v1/oauth/cli/poll", headers, json!({ "flow": flow }));
    if *z.poll_business_error.lock().unwrap() {
        // **HTTP 200，业务码是失败** —— 实测它们就是这么回的
        return axum::Json(json!({"code": 401, "msg": "token expired or incorrect"}));
    }
    {
        let mut left = z.pending_left.lock().unwrap();
        if *left > 0 {
            *left -= 1;
            return wrapped(json!({ "status": "pending" }));
        }
    }
    wrapped(json!({
        "status": "ready",
        "token": "zcode-jwt",
        "user": {"user_id": "u-1", "email": "someone@example.com", "name": "Someone"},
        "zai": {"access_token": "oauth-access", "refresh_token": "oauth-refresh"},
        "bigmodel": {"access_token": "oauth-access"},
    }))
}

async fn business_login(
    State(z): State<Arc<Zai>>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> axum::Json<Value> {
    z.note("/api/auth/z/login", headers, body);
    wrapped(json!({ "access_token": "biz-token" }))
}

async fn customer(State(z): State<Arc<Zai>>, headers: HeaderMap) -> axum::Json<Value> {
    z.note("/api/biz/customer/getCustomerInfo", headers, Value::Null);
    wrapped(json!({"organizations": [
        {"organizationId": "o1", "organizationName": "某个默认机构", "projects": [
            {"projectId": "p1", "projectName": "默认项目"}
        ]}
    ]}))
}

async fn list_keys(State(z): State<Arc<Zai>>, headers: HeaderMap) -> axum::Json<Value> {
    z.note("/api_keys:list", headers, Value::Null);
    let keys = z.keys.lock().unwrap().clone();
    wrapped(Value::Array(keys))
}

async fn create_key(
    State(z): State<Arc<Zai>>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> axum::Json<Value> {
    z.note("/api_keys:create", headers, body.clone());
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let made = json!({"apiKey": "made-key", "name": name});
    z.keys.lock().unwrap().push(made.clone());
    wrapped(made)
}

async fn copy_key(
    State(z): State<Arc<Zai>>,
    Path((_org, _project, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> axum::Json<Value> {
    z.note("/api_keys:copy", headers, json!({ "id": id }));
    if *z.no_secret.lock().unwrap() {
        return wrapped(json!({}));
    }
    wrapped(json!({ "secretKey": "the-secret" }))
}

async fn start_zai(z: Arc<Zai>) -> Endpoints {
    let app = axum::Router::new()
        .route("/api/v1/oauth/cli/init", post(init))
        .route("/api/v1/oauth/cli/poll/{flow}", get(poll))
        .route("/api/auth/z/login", post(business_login))
        .route("/api/biz/customer/getCustomerInfo", get(customer))
        .route(
            "/api/biz/v1/organization/{org}/projects/{project}/api_keys",
            get(list_keys).post(create_key),
        )
        .route(
            "/api/biz/v1/organization/{org}/projects/{project}/api_keys/copy/{id}",
            get(copy_key),
        )
        .with_state(z);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Endpoints {
        platform: format!("http://{addr}/api/v1"),
        zai_business: format!("http://{addr}"),
        bigmodel_business: format!("http://{addr}"),
        zai_upstream: format!("http://{addr}/api/anthropic"),
        bigmodel_upstream: format!("http://{addr}/api/bigmodel-anthropic"),
    }
}

// ---------------------------------------------------------------- 控制面

const BASE: &str = "version: 1
clients:
  - name: c
    key: tw-k
providers:
  - name: relay
    base_url: https://relay.example
    key: sk-relay
    protocol: anthropic
";

struct Bed {
    dir: tempfile::TempDir,
    app: axum::Router,
    events: tokio::sync::broadcast::Receiver<tw_api::Event>,
    zai: Arc<Zai>,
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
    async fn login_finished(&mut self) -> (String, Option<String>, Option<String>) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(tw_api::Event::LoginFinished {
                    status,
                    provider,
                    error,
                    ..
                }) = self.events.recv().await
                {
                    return (status, provider, error);
                }
            }
        })
        .await
        .expect("5 秒内应当收到登录结果")
    }
}

/// `extra` 追加在 providers 之下，拿得到假 Z.ai 的地址
async fn bed(extra: impl FnOnce(&Endpoints) -> String) -> Bed {
    let zai = Arc::new(Zai::default());
    let endpoints = start_zai(zai.clone()).await;
    let yaml = format!("{BASE}{}", extra(&endpoints));
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
        chatgpt: Default::default(),
        zai: Arc::new(Accounts::new(endpoints.clone())),
        home: d.path().join("home"),
    };
    Bed {
        app: tw_control::router(state),
        dir: d,
        events,
        zai,
        endpoints,
    }
}

// ---------------------------------------------------------------- 登录

#[tokio::test]
async fn a_login_writes_the_account_into_the_config_as_an_anthropic_upstream() {
    let mut b = bed(|_| String::new()).await;
    let (st, v) = b.call("POST", "/zai/login", json!({})).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        v["authorize_url"], "https://chat.example/api/oauth/authorize?client_id=theirs",
        "授权地址原样交给界面去打开"
    );
    assert!(
        v["expires_in_secs"].as_u64().unwrap() <= 15 * 60,
        "平台给的期限再长也不超过我们的上限：{v}"
    );

    let (status, provider, error) = b.login_finished().await;
    assert_eq!(status, "done", "{error:?}");
    assert_eq!(provider.as_deref(), Some("zai"));

    let p = b.provider("zai").expect("上游应当写进了配置");
    assert_eq!(p.base_url, b.endpoints.zai_upstream);
    assert_eq!(p.protocol, Some(tw_config::Protocol::Anthropic));
    // **密钥是 `id.secret` 两段拼起来的**
    assert_eq!(
        p.key.as_ref().unwrap().resolve().unwrap(),
        "made-key.the-secret"
    );
    assert!(p.oauth.is_none(), "这类上游不用 OAuth，不该写 oauth 块");
    // 计费是默认那档：订阅账号也按价目表算费用
    assert_eq!(p.billing, tw_config::Billing::PerToken);
    // 原有的上游一个都没动
    assert!(b.file().contains("sk-relay"));

    // 轮询凭据是我们自己生成的，不是平台回显的那个
    let init_auth = b.zai.auth_of("/api/v1/oauth/cli/init").unwrap();
    let poll_auth = b.zai.auth_of("/api/v1/oauth/cli/poll").unwrap();
    assert_eq!(init_auth, poll_auth);
    assert!(!init_auth.contains("the-server-echo"), "{init_auth}");
    assert_eq!(
        b.zai.body_of("/api/v1/oauth/cli/init").unwrap()["provider"],
        "zai"
    );
    // 建 key 那一步认的是业务令牌，不是 OAuth 的 access token
    assert_eq!(
        b.zai.auth_of("/api/biz/customer/getCustomerInfo").unwrap(),
        "Bearer biz-token"
    );
    assert_eq!(
        b.zai.body_of("/api/auth/z/login").unwrap()["token"],
        "oauth-access"
    );
    // 建的那把 key 用我们自己的名字，不借用别人的
    assert_eq!(
        b.zai.body_of("/api_keys:create").unwrap()["name"],
        "thinkwatch"
    );
}

#[tokio::test]
async fn every_request_says_who_it_is_from() {
    let mut b = bed(|_| String::new()).await;
    let (st, _) = b.call("POST", "/zai/login", json!({})).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(b.login_finished().await.0, "done");
    let seen = b.zai.seen.lock().unwrap();
    assert!(!seen.is_empty());
    for (path, headers, _) in seen.iter() {
        let ua = headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert_eq!(ua, tw_gateway::user_agent(), "{path} 没有如实说自己是谁");
    }
}

#[tokio::test]
async fn the_authorization_is_waited_for_until_it_is_ready() {
    let mut b = bed(|_| String::new()).await;
    *b.zai.pending_left.lock().unwrap() = 2;
    let (st, v) = b.call("POST", "/zai/login", json!({})).await;
    assert_eq!(st, StatusCode::OK);
    let id = v["id"].as_str().unwrap().to_string();

    let (st, v) = b.call("GET", &format!("/zai/login/{id}"), json!({})).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["status"], "pending");

    assert_eq!(b.login_finished().await.0, "done");
    assert!(b.zai.hit("/api/v1/oauth/cli/poll") >= 3);
    let (_, v) = b.call("GET", &format!("/zai/login/{id}"), json!({})).await;
    assert_eq!(v["status"], "done");
    assert_eq!(v["account"], "someone@example.com");
}

#[tokio::test]
async fn a_business_error_code_inside_a_200_fails_the_login() {
    // **这是它们真实的回法**：HTTP 200，错在正文里。只看状态码的话，这一步会被
    // 当成成功，然后在后面某一步报一句和真实原因无关的错。
    let mut b = bed(|_| String::new()).await;
    *b.zai.poll_business_error.lock().unwrap() = true;
    let (st, _) = b.call("POST", "/zai/login", json!({})).await;
    assert_eq!(st, StatusCode::OK);
    let (status, provider, error) = b.login_finished().await;
    assert_eq!(status, "failed");
    assert!(provider.is_none());
    let why = error.unwrap();
    assert!(why.contains("token expired or incorrect"), "{why}");
    assert!(b.provider("zai").is_none(), "失败的登录不能写配置");
}

#[tokio::test]
async fn signing_in_again_only_replaces_the_key() {
    let mut b = bed(|e| {
        format!(
            "  - name: zai
    base_url: {upstream}
    protocol: anthropic
    key: old-key.old-secret
    proxy: system
    models_only:
      - glm-4.6
    disabled: true
",
            upstream = e.zai_upstream
        )
    })
    .await;
    // 账号里已经有我们的那把 key 了：复用，不再建一把
    b.zai
        .keys
        .lock()
        .unwrap()
        .push(json!({"apiKey": "made-key", "name": "thinkwatch"}));

    let (st, _) = b
        .call("POST", "/zai/login", json!({"proxy": "system"}))
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(b.login_finished().await.0, "done");

    let p = b.provider("zai").unwrap();
    assert_eq!(
        p.key.as_ref().unwrap().resolve().unwrap(),
        "made-key.the-secret"
    );
    // 用户调过的设置一个都不能动
    assert_eq!(p.proxy, "system");
    assert_eq!(p.models_only.as_deref(), Some(&["glm-4.6".to_string()][..]));
    assert!(p.disabled);
    assert_eq!(b.zai.hit("/api_keys:create"), 0, "已经有的 key 应当复用");
}

#[tokio::test]
async fn an_upstream_of_another_service_with_the_same_name_is_a_conflict() {
    let b = bed(|_| {
        "  - name: zai
    base_url: https://relay.example/anthropic
    key: sk-someone-elses
    protocol: anthropic
"
        .to_string()
    })
    .await;
    let (st, v) = b.call("POST", "/zai/login", json!({})).await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert!(
        b.file().contains("sk-someone-elses"),
        "别人的密钥不能被换掉"
    );
    assert_eq!(b.zai.hit("/api/v1/oauth/cli/init"), 0, "拦在开始之前");
}

#[tokio::test]
async fn bigmodel_signs_in_with_the_oauth_token_itself_and_may_get_a_key_without_a_secret() {
    let mut b = bed(|_| String::new()).await;
    *b.zai.no_secret.lock().unwrap() = true;
    let (st, v) = b
        .call("POST", "/zai/login", json!({"family": "bigmodel"}))
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (status, provider, error) = b.login_finished().await;
    assert_eq!(status, "done", "{error:?}");
    assert_eq!(provider.as_deref(), Some("bigmodel"));

    let p = b.provider("bigmodel").unwrap();
    assert_eq!(p.base_url, b.endpoints.bigmodel_upstream);
    // secret 那一半没有时，BigModel 的 key 就是它自己
    assert_eq!(p.key.as_ref().unwrap().resolve().unwrap(), "made-key");
    // 它的业务接口直接认 OAuth 的 access token，**不带 `Bearer ` 前缀**
    assert_eq!(
        b.zai.auth_of("/api/biz/customer/getCustomerInfo").unwrap(),
        "oauth-access"
    );
    assert_eq!(b.zai.hit("/api/auth/z/login"), 0, "这一家不需要换业务令牌");
}

#[tokio::test]
async fn z_ai_without_the_secret_half_is_a_failure_rather_than_an_unusable_key() {
    let mut b = bed(|_| String::new()).await;
    *b.zai.no_secret.lock().unwrap() = true;
    let (st, _) = b.call("POST", "/zai/login", json!({})).await;
    assert_eq!(st, StatusCode::OK);
    let (status, _, error) = b.login_finished().await;
    assert_eq!(status, "failed");
    assert!(error.unwrap().contains("secret"), "要说清缺的是哪一半");
    assert!(b.provider("zai").is_none());
}

#[tokio::test]
async fn an_unknown_family_and_an_unknown_proxy_are_both_refused() {
    let b = bed(|_| String::new()).await;
    let (st, _) = b
        .call("POST", "/zai/login", json!({"family": "openai"}))
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = b.call("POST", "/zai/login", json!({"proxy": "nope"})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(b.zai.hit("/api/v1/oauth/cli/init"), 0);
}

#[tokio::test]
async fn a_login_can_be_cancelled_and_a_new_one_replaces_the_old() {
    let mut b = bed(|_| String::new()).await;
    // 一直 pending：不取消就不会结束
    *b.zai.pending_left.lock().unwrap() = u32::MAX;
    let (_, first) = b.call("POST", "/zai/login", json!({})).await;
    let id = first["id"].as_str().unwrap().to_string();

    let (st, v) = b
        .call("DELETE", &format!("/zai/login/{id}"), json!({}))
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "cancelled");
    assert_eq!(b.login_finished().await.0, "cancelled");
    assert!(b.provider("zai").is_none());

    // 取消过的那次不再认
    let (st, _) = b.call("GET", &format!("/zai/login/{id}"), json!({})).await;
    assert_eq!(st, StatusCode::OK, "取消后的状态还查得到");

    // 新的一次登录顶掉旧的
    *b.zai.pending_left.lock().unwrap() = 0;
    let (_, second) = b.call("POST", "/zai/login", json!({})).await;
    assert_ne!(second["id"], first["id"]);
    assert_eq!(b.login_finished().await.0, "done");
    let (st, _) = b.call("GET", &format!("/zai/login/{id}"), json!({})).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "旧的那次已经不在了");
}
