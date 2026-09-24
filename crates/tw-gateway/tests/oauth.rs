//! OAuth 凭据的验收（第 4 类）。
//!
//! 单元测试证明不了这里的东西：**换 token 发生在故障转移循环里面**，
//! 而它的失败模式全在接缝上 —— 上游到底收到了哪个 token、第二个请求
//! 有没有又换一次、token 端点挂了会不会把整个网关拖下水。

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::routing::post;
use tw_config::{Client, Config, Listen, OAuth, Provider};

const REFRESH: &str = "rt-ORIGINAL-refresh-token";
/// 服务器换发的那个。**每次都不一样** —— 真的会轮换的服务器就是这样，
/// 而返回一个常量的假服务器会让「第二次轮换」根本不存在（写这条测试
/// 时先掉进去过一次：测试数据错了，不是代码错了）。
fn rotated(n: usize) -> String {
    format!("rt-SERVER-ROTATED-{n}")
}

#[derive(Default)]
struct Token {
    /// 换了几次。**这个数字就是缓存有没有在工作。**
    calls: AtomicUsize,
    /// 每次收到的 refresh_token
    seen: Mutex<Vec<String>>,
    /// 有效期。`None` = 不告诉客户端
    expires_in: Option<u64>,
    /// 每次都换发一个新的 refresh token（很多 OAuth2 服务器这么干）
    rotate: bool,
}

/// 表单取值。**不引第三方**，这几行比一个 dev-dependency 便宜。
fn form_get(body: &str, key: &str) -> String {
    body.split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| percent_decode(v))
        .unwrap_or_default()
}

fn percent_decode(s: &str) -> String {
    let b = s.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(x) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(x);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 假的 token 端点。
async fn start_token_endpoint(t: Arc<Token>) -> SocketAddr {
    let app = Router::new()
        .route(
            "/token",
            post(|State(t): State<Arc<Token>>, body: String| async move {
                let n = t.calls.fetch_add(1, Ordering::SeqCst) + 1;
                let got = form_get(&body, "refresh_token");
                t.seen.lock().unwrap().push(got);
                let mut v = serde_json::json!({
                    "access_token": format!("at-issued-{n}"),
                    "token_type": "Bearer",
                });
                if let Some(e) = t.expires_in {
                    v["expires_in"] = e.into();
                }
                if t.rotate {
                    v["refresh_token"] = rotated(n).into();
                }
                axum::Json(v)
            }),
        )
        .with_state(t);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

/// 假上游：记下它收到的凭据头，回一个最小的 Anthropic 响应。
async fn start_upstream() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    let app = Router::new()
        .route(
            "/v1/messages",
            post(
                |State(s): State<Arc<Mutex<Vec<String>>>>, headers: axum::http::HeaderMap| async move {
                    // 凭据可能在 x-api-key 也可能在 authorization —— 两个都记
                    let mut line = String::new();
                    for h in ["x-api-key", "authorization"] {
                        if let Some(v) = headers.get(h).and_then(|v| v.to_str().ok()) {
                            line.push_str(&format!("{h}={v};"));
                        }
                    }
                    s.lock().unwrap().push(line);
                    axum::response::Response::builder()
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(
                            r#"{"type":"message","content":[{"type":"text","text":"ok"}]}"#,
                        ))
                        .unwrap()
                },
            ),
        )
        .with_state(s);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, seen)
}

fn oauth_provider(name: &str, upstream: SocketAddr, token_url: &str) -> Provider {
    Provider {
        name: name.into(),
        base_url: format!("http://{upstream}"),
        oauth: Some(OAuth {
            access: None,
            expires_at: None,
            refresh: REFRESH.into(),
            endpoint: token_url.into(),
            client_id: Some("tw-test".into()),
            client_secret: None,
            refresh_before: Some("5m".into()),
        }),
        protocol: Some(tw_config::Protocol::Anthropic),
        ..Default::default()
    }
}

async fn start_gateway(providers: Vec<Provider>) -> (SocketAddr, tw_gateway::AppState) {
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "claude-code".into(),
            key: "tw-testkey".into(),
            ..Default::default()
        }],
        providers,
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let handed = state.clone();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    tokio::spawn(async move { tw_gateway::serve(state, addr).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(60)).await;
    (addr, handed)
}

async fn ask(gw: SocketAddr) -> reqwest::StatusCode {
    reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-testkey")
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-sonnet-4-5","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap()
        .status()
}

/// 验收第一条：**上游收到的是换回来的 access token，不是 refresh token。**
///
/// 搞错的话，refresh token 会被发给每一个上游 —— 而它换得出无数个
/// access token。这是这个功能最坏的失败模式。
#[tokio::test]
async fn the_upstream_gets_the_access_token_and_never_the_refresh_token() {
    let t = Arc::new(Token {
        expires_in: Some(3600),
        ..Default::default()
    });
    let token_addr = start_token_endpoint(t.clone()).await;
    let (up, seen) = start_upstream().await;
    let (gw, _s) = start_gateway(vec![oauth_provider(
        "p",
        up,
        &format!("http://{token_addr}/token"),
    )])
    .await;

    assert_eq!(ask(gw).await, 200);

    let got = seen.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "上游应该收到一次请求：{got:?}");
    assert!(
        got[0].contains("at-issued-1"),
        "上游拿到的不是换回来的：{got:?}"
    );
    assert!(
        !got[0].contains(REFRESH),
        "**refresh token 泄漏给上游了**：{got:?}"
    );
    assert_eq!(t.calls.load(Ordering::SeqCst), 1);
    assert_eq!(t.seen.lock().unwrap()[0], REFRESH);
}

/// 验收第二条：**第二个请求不再换一次。**
///
/// 没有缓存的话，每个请求前面挂一次 token 端点的往返 —— 那是把一个
/// 一次性成本加到了数据面的每一跳上。
#[tokio::test]
async fn a_second_request_reuses_the_token_instead_of_exchanging_again() {
    let t = Arc::new(Token {
        expires_in: Some(3600),
        ..Default::default()
    });
    let token_addr = start_token_endpoint(t.clone()).await;
    let (up, seen) = start_upstream().await;
    let (gw, _s) = start_gateway(vec![oauth_provider(
        "p",
        up,
        &format!("http://{token_addr}/token"),
    )])
    .await;

    for _ in 0..3 {
        assert_eq!(ask(gw).await, 200);
    }
    assert_eq!(
        t.calls.load(Ordering::SeqCst),
        1,
        "三个请求换了不止一次 token"
    );
    // 三次都用的是同一个
    for line in seen.lock().unwrap().iter() {
        assert!(line.contains("at-issued-1"), "{line}");
    }
}

/// 提前量比有效期还长时，**不能变成每个请求刷一次**。
///
/// `expires_in: 60` 配 `refresh_before: 5m` 减出来是负的。当成「现在就该
/// 刷」的话，每个请求前面都挂一次往返 —— 比不刷更糟。
#[tokio::test]
async fn a_lead_longer_than_the_lifetime_does_not_refresh_every_request() {
    let t = Arc::new(Token {
        expires_in: Some(60),
        ..Default::default()
    });
    let token_addr = start_token_endpoint(t.clone()).await;
    let (up, _seen) = start_upstream().await;
    let (gw, _s) = start_gateway(vec![oauth_provider(
        "p",
        up,
        &format!("http://{token_addr}/token"),
    )])
    .await;

    for _ in 0..3 {
        assert_eq!(ask(gw).await, 200);
    }
    assert_eq!(t.calls.load(Ordering::SeqCst), 1, "每个请求都去换了一次");
}

/// 服务器没给 `expires_in` 时，**不能每次都当成过期了**。
#[tokio::test]
async fn a_token_without_an_expiry_is_used_until_something_says_otherwise() {
    let t = Arc::new(Token::default()); // expires_in: None
    let token_addr = start_token_endpoint(t.clone()).await;
    let (up, _seen) = start_upstream().await;
    let (gw, _s) = start_gateway(vec![oauth_provider(
        "p",
        up,
        &format!("http://{token_addr}/token"),
    )])
    .await;

    for _ in 0..3 {
        assert_eq!(ask(gw).await, 200);
    }
    assert_eq!(t.calls.load(Ordering::SeqCst), 1);
}

/// 验收第三条：**token 端点挂了，只是这一家挂了。**
///
/// 不切换的话，一家 OAuth 上游的 token 端点抽风会让整个网关不可用 ——
/// 而那正是故障转移存在的理由。
#[tokio::test]
async fn a_dead_token_endpoint_fails_over_instead_of_taking_the_gateway_down() {
    let (up, seen) = start_upstream().await;
    let mut broken = oauth_provider("oauth-broken", up, "http://127.0.0.1:1/token");
    broken.name = "oauth-broken".into();
    let backup = Provider {
        name: "backup".into(),
        base_url: format!("http://{up}"),
        key: Some("sk-plain-backup".into()),
        protocol: Some(tw_config::Protocol::Anthropic),
        ..Default::default()
    };
    let (gw, _s) = start_gateway(vec![broken, backup]).await;

    assert_eq!(ask(gw).await, 200, "换 token 失败之后没有切到下一家");
    let got = seen.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    assert!(got[0].contains("sk-plain-backup"), "{got:?}");
}

/// 验收第四条：**服务器换发 refresh token 时，新的要送出去写回配置。**
///
/// 换发的那一刻旧的就在服务端作废了 —— 不写回等于让 config.yaml 从那
/// 一秒起就是坏的，只是症状延迟到下次重启。这条测试盯的是数据面这一半：
/// 每一次轮换都要送到 sink 上，**一次都不能漏**（漏掉一次，文件里就是
/// 一个作废了的 token）。
#[tokio::test]
async fn every_rotation_reaches_the_sink_so_none_is_lost() {
    let t = Arc::new(Token {
        expires_in: Some(1), // 1 秒就该续 —— 逼出连续几次轮换
        rotate: true,
        ..Default::default()
    });
    let token_addr = start_token_endpoint(t.clone()).await;
    let (up, _seen) = start_upstream().await;
    let (gw, state) = start_gateway(vec![oauth_provider(
        "p",
        up,
        &format!("http://{token_addr}/token"),
    )])
    .await;

    // 装一个假的写回端 —— 真的那个在 tw-control 里
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    state.set_renewal_sink(tx);

    assert_eq!(ask(gw).await, 200);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(ask(gw).await, 200);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(ask(gw).await, 200);

    assert_eq!(t.calls.load(Ordering::SeqCst), 3, "没有按 expires_in 去续");
    // 第二次开始用的是服务器换发的那个，不是配置里的
    assert_eq!(
        t.seen.lock().unwrap().clone(),
        vec![REFRESH.to_string(), rotated(1), rotated(2)],
        "第二次之后没有用服务器换发的那个"
    );

    // **两次轮换，两条都要到。**在这儿去重的话，config.yaml 里会留下
    // 一个已经作废的 token
    let mut got = Vec::new();
    while let Ok(r) = rx.try_recv() {
        got.push(r);
    }
    // **三次换 token 就是三次轮换** —— 第一次也算：服务器在第一次
    // 交换时就换发了新的 refresh token，配置里那个从那时起已经作废
    assert_eq!(got.len(), 3, "轮换漏送了：{got:?}");
    assert_eq!(
        got.iter().map(|r| r.refresh.clone()).collect::<Vec<_>>(),
        vec![Some(rotated(1)), Some(rotated(2)), Some(rotated(3))],
        "送出去的不是每一次的新值"
    );
    // access token 和过期时间跟着一起送：它们也要写回配置
    assert_eq!(
        got.iter().map(|r| r.access.clone()).collect::<Vec<_>>(),
        vec!["at-issued-1", "at-issued-2", "at-issued-3"]
    );
    for r in &got {
        assert_eq!(r.provider, "p");
        assert!(r.expires_at.is_some(), "{r:?}");
        assert!(!r.endpoint.contains("rt-"), "{}", r.endpoint);
        assert!(
            !format!("{r:?}").contains("at-issued"),
            "调试输出里不能有 token：{r:?}"
        );
    }
}

/// 没有控制面时（网关单独跑），轮换**只报不写，而且要说清没写**。
#[tokio::test]
async fn without_a_config_manager_the_rotation_is_reported_as_not_persisted() {
    let t = Arc::new(Token {
        expires_in: Some(1),
        rotate: true,
        ..Default::default()
    });
    let token_addr = start_token_endpoint(t.clone()).await;
    let (up, _seen) = start_upstream().await;
    let (gw, state) = start_gateway(vec![oauth_provider(
        "p",
        up,
        &format!("http://{token_addr}/token"),
    )])
    .await;
    let mut rx = state.bus.subscribe();

    assert_eq!(ask(gw).await, 200);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(ask(gw).await, 200);

    let mut seen = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if let tw_api::Event::CredentialRotated {
            provider,
            persisted,
            detail,
            ..
        } = &e
        {
            seen.push((provider.clone(), *persisted, detail.clone()));
        }
    }
    // **失败每次都说。**它是个没解决的问题，而且重启之前不解决这家
    // 上游就废了 —— 「同一句话别说第二遍」只压成功的那句
    assert_eq!(seen.len(), 2, "失败被压掉了：{seen:?}");
    for one in &seen {
        assert_eq!(one.0, "p");
        assert!(!one.1, "没写却说写了：{seen:?}");
        assert!(one.2.text.contains("not written back"), "{seen:?}");
    }
    // 事件里一个 token 都不能有
    assert!(!seen.iter().any(|s| s.2.text.contains("rt-")), "{seen:?}");
}

/// 用户在 config.yaml 里换了 refresh token 之后，**缓存不能盖住这个修改**。
///
/// 盖住的话：用户换了新凭据、重载配置、然后发现还在用旧的报 401，而
/// 配置文件里明明是对的。那是最难查的一类。
#[tokio::test]
async fn editing_the_refresh_token_in_the_config_invalidates_the_cache() {
    let t = Arc::new(Token {
        expires_in: Some(3600),
        ..Default::default()
    });
    let token_addr = start_token_endpoint(t.clone()).await;
    let (up, _seen) = start_upstream().await;
    let url = format!("http://{token_addr}/token");
    let cache = tw_gateway::oauth::Cache::new();
    let http = reqwest::Client::new();

    let mut cfg = oauth_provider("p", up, &url).oauth.unwrap();
    let (a, _) = cache.token("p", &cfg, &http).await.unwrap();
    assert_eq!(a, "at-issued-1");
    // 没改配置：不再换
    let (again, _) = cache.token("p", &cfg, &http).await.unwrap();
    assert_eq!(again, "at-issued-1");
    assert_eq!(t.calls.load(Ordering::SeqCst), 1);

    // 用户改了 config.yaml 里的 refresh token
    cfg.refresh = "rt-USER-PUT-A-NEW-ONE-IN".into();
    let (fresh, _) = cache.token("p", &cfg, &http).await.unwrap();
    assert_eq!(fresh, "at-issued-2", "缓存盖住了用户的修改");
    assert_eq!(
        t.seen.lock().unwrap()[1],
        "rt-USER-PUT-A-NEW-ONE-IN",
        "用的还是旧的 refresh token"
    );
}

/// token 端点回 400 时，**它的正文要打码再往外说**。
///
/// OAuth 服务器的错误里经常把请求参数回显出来 —— 那里面有 refresh token。
#[tokio::test]
async fn an_error_body_from_the_token_endpoint_is_masked_before_being_reported() {
    let app = Router::new().route(
        "/token",
        post(|body: String| async move {
            // 真实的 token 端点确实会这么回显
            // **字段名故意起成 `provided`。**叫 `refresh_token=` 的话
            // `mask_body` 会按参数名认出来 —— 那条测试就变成在测
            // `mask_body`，而不是在测这一层
            let echoed = form_get(&body, "refresh_token");
            (
                axum::http::StatusCode::BAD_REQUEST,
                format!("{{\"error\":\"invalid_grant\",\"provided\":\"{echoed}\"}}"),
            )
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    let cache = tw_gateway::oauth::Cache::new();
    // **真实的 refresh token 长这样：不透明，不像任何已知的密钥格式。**
    // 靠 `mask_body` 的模式匹配挡不住它 —— 而这正是这条测试要证明的
    let cfg = OAuth {
        access: None,
        expires_at: None,
        refresh: "1//0gLdOPAQUE-nothing-matches-this-shape".into(),
        endpoint: format!("http://{addr}/token"),
        client_id: None,
        client_secret: None,
        refresh_before: None,
    };
    let e = cache
        .token("p", &cfg, &reqwest::Client::new())
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("invalid_grant"), "该说的话没说：{e}");
    assert!(
        !e.contains("OPAQUE-nothing-matches"),
        "**凭据从错误消息里漏出去了**：{e}"
    );
    assert!(e.contains("<omitted>"), "抹掉了但没说抹掉了什么：{e}");
}
