//! 重放走的出站路径。
//!
//! **重放必须和数据面走同一条出站路径**：同一家上游的同一个 client，带着它该走的
//! 代理。以前重放用的是一个读系统代理的公用 client —— 本机的上游（`127.0.0.1`）
//! 被送进了系统代理，拿回来的是代理的 503 页面；指定了代理的上游反过来绕过了
//! 它的代理。两种都是在比另一条路。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

/// 一个只会回同一句话的 HTTP 服务。当上游用，也当 HTTP 代理用 —— 对 `http://`
/// 目标，代理收到的就是一个请求行里带完整 URL 的普通请求。
async fn answering(status: u16, body: &'static str) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = l.accept().await else {
                return;
            };
            tokio::spawn(async move {
                // 读到头结束再回；请求体很小，和头一起到
                let mut buf = vec![0u8; 64 * 1024];
                let mut seen = 0;
                while !buf[..seen].windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf[seen..]).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => seen += n,
                    }
                }
                let resp = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    addr
}

/// 子进程靠这个环境变量认出自己（见 `replays_ignore_the_system_proxy`）
const CHILD: &str = "TW_REPLAY_SYSTEM_PROXY_CHILD";

fn row(id: i64, provider: &str) -> tw_store::db::RequestRow {
    tw_store::db::RequestRow {
        session_log_bytes: None,
        key_masked: None,
        peer: None,
        id,
        at_ms: 1000 + id,
        client: "我".into(),
        client_hint: None,
        session: None,
        provider: provider.into(),
        model: "claude-sonnet-4-5".into(),
        sent_model: "claude-sonnet-4-5".into(),
        answered_model: None,
        path: "/v1/messages".into(),
        status: Some(200),
        ttfb_ms: Some(100),
        ttft_ms: None,
        duration_ms: Some(200),
        tokens_per_sec: None,
        bytes: Some(10),
        input_tokens: Some(5),
        output_tokens: Some(2),
        cache_read_tokens: None,
        cache_write_tokens: None,
        input_estimate: None,
        cost_micros: Some(0),
        cost_estimated: false,
        error: None,
        local: false,
        cancelled: false,
        routing: None,
        billing: tw_api::Billing::Free,
        cache_saved_micros: None,
        price_source: None,
        translated: None,
    }
}

/// 一个记着一条请求（连同请求体）的控制面。
fn app(config: &str) -> (tempfile::TempDir, axum::Router) {
    app_with(
        config,
        br#"{"model":"claude-sonnet-4-5","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#,
    )
}

/// 同上，存着的请求体是 `body`。
fn app_with(config: &str, body: &[u8]) -> (tempfile::TempDir, axum::Router) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, config).unwrap();
    let db = tw_store::Db::open(&d.path().join("data.db")).unwrap();
    let r = row(1, "本机");
    db.insert(&r).unwrap();
    let blobs = tw_store::Blobs::new(d.path().join("blobs"));
    assert!(blobs.put(r.at_ms, r.id, tw_store::Which::Request, body));
    let rec = tw_store::Recorder::new(
        db,
        blobs,
        tw_pricing::shared(tw_pricing::PriceBook::builtin().unwrap()),
    );
    let cfg: tw_config::Config = serde_yaml_ng::from_str(config).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let state = ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        store: Some(Arc::new(tokio::sync::Mutex::new(rec))),
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    };
    (d, tw_control::router(state))
}

async fn replay(app: &axum::Router, provider: &str) -> serde_json::Value {
    let req = tw_api::ReplayRequest {
        id: 1,
        provider: provider.into(),
    };
    let r = app
        .clone()
        .oneshot(
            Request::post("/replay/run")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&req).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(status, StatusCode::OK, "{v}");
    v
}

/// 系统代理开着的时候，重放照样直连、照样走上游自己的代理：两条都在一个子进程里跑，
/// 系统代理是一个谁来都回 503 的假代理，和真机上撞到的那一页一样。
///
/// **系统代理只写进子进程的环境变量。**reqwest 建 client 时从环境变量读系统代理，而在
/// 这个跑着别的测试的进程里改环境变量（`set_var`）是未定义行为：别的线程里的 C 代码在
/// 同时读它 —— SQLite 第一次打开库时读 `TMPDIR`，aws-lc 初始化时读 CPU 特性的开关 ——
/// 而 glibc 的 `setenv` 会挪动整张环境表，读的那一方踩到释放了的内存。这个文件以前就这么
/// 改，Linux 的 CI 上崩过一次（SIGSEGV，四条测试刚开始跑）。子进程的环境在它起来之前就
/// 定好了，没有谁去改。
#[tokio::test]
async fn replays_ignore_the_system_proxy() {
    let proxy = answering(503, "via the system proxy").await;
    let url = format!("http://{proxy}");
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"));
    child
        .args([
            "--exact",
            "child_with_a_system_proxy",
            "--include-ignored",
            "--nocapture",
        ])
        .env(CHILD, "1");
    for k in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
    ] {
        child.env(k, &url);
    }
    for k in ["NO_PROXY", "no_proxy"] {
        child.env_remove(k);
    }
    // 等子进程的时候，这个运行时还要接着替假代理接客
    let out = tokio::task::spawn_blocking(move || child.output())
        .await
        .unwrap()
        .expect("run the child");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("1 passed"), "{text}");
}

#[tokio::test]
#[ignore = "only runs as the child of replays_ignore_the_system_proxy"]
async fn child_with_a_system_proxy() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    // 系统代理真的在：不在的话，下面两条什么都没证明
    let system = std::env::var("HTTP_PROXY").expect("the parent sets the system proxy");
    assert!(system.starts_with("http://127.0.0.1:"), "{system}");
    a_direct_upstream_is_replayed_directly().await;
    an_upstream_with_a_proxy_is_replayed_through_that_proxy().await;
}

/// 直连的本机上游：系统里开着代理也不走它。数据面转发这一家时就是这样。
async fn a_direct_upstream_is_replayed_directly() {
    let upstream = answering(200, "from the upstream").await;
    let (_d, app) = app(&format!(
        "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: 我\n    key: tw-一把钥匙就够\nproviders:\n  \
         - name: 本机\n    base_url: http://{upstream}\n    key: sk-x\n    billing: free\n"
    ));

    let v = replay(&app, "本机").await;
    assert_eq!(v["status"], 200, "{v}");
    assert_eq!(v["body"], "from the upstream", "{v}");
}

/// 指定了代理的上游：重放走它的代理，而不是直连或系统代理。
async fn an_upstream_with_a_proxy_is_replayed_through_that_proxy() {
    let proxy = answering(200, "via its own proxy").await;
    // 没有人听的地址：直连的话只会连不上
    let (_d, app) = app(&format!(
        "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: 我\n    key: tw-一把钥匙就够\nproxies:\n  \
         - {{ name: 自己的, type: http, addr: \"{proxy}\" }}\nproviders:\n  \
         - name: 本机\n    base_url: http://127.0.0.1:9\n    key: sk-x\n    billing: free\n    \
         proxy: 自己的\n"
    ));

    let v = replay(&app, "本机").await;
    assert_eq!(v["status"], 200, "{v}");
    assert_eq!(v["body"], "via its own proxy", "{v}");
}

/// Bedrock 只收 Converse，而录下来的请求是客户端的格式：先说清楚，不发
#[tokio::test]
async fn a_bedrock_upstream_is_not_offered_a_replay_it_cannot_take() {
    let (_d, app) = app(
        "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: 我\n    key: tw-一把钥匙就够\nproviders:\n  \
         - name: br\n    base_url: https://bedrock-runtime.us-east-1.amazonaws.com\n    key: ABSK-x\n",
    );
    for path in ["/replay/quote", "/replay/run"] {
        let req = tw_api::ReplayRequest {
            id: 1,
            provider: "br".into(),
        };
        let r = app
            .clone()
            .oneshot(
                Request::post(path)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&req).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CONFLICT, "{path}");
        let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["code"], "control.replay_bedrock", "{path}: {v}");
    }
}

/// 一个把收到的请求体原样回给你的上游，顺手记下收到了什么。
async fn echoing() -> (SocketAddr, Arc<std::sync::Mutex<String>>) {
    let seen: Arc<std::sync::Mutex<String>> = Arc::default();
    let s = seen.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |body: String| {
            let s = s.clone();
            async move {
                *s.lock().unwrap() = body.clone();
                body
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, seen)
}

/// 存下来的请求（拦截档下存的是换过的那一份）里写着 1 号；这一次又找到一把密钥（比如
/// 那之后加了一条规则，或者观察档下存的）。**新找到的拿 2 号**：拿 1 号的话，回答里的
/// 1 号会被还原成这把新的，而它原本指的是另一样。
#[tokio::test]
async fn a_replay_numbers_new_finds_after_the_placeholders_already_stored() {
    const KEY: &str = "sk-ant-api03-REPLAYKEYAAAAAAAAAAAAAAAA";
    let (upstream, seen) = echoing().await;
    let stored = format!(
        r#"{{"model":"claude-sonnet-4-5","max_tokens":8,"messages":[{{"role":"user","content":"旧的 <<TW_SECRET_1>>，新的 {KEY}"}}]}}"#
    );
    let (_d, app) = app_with(
        &format!(
            "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: 我\n    key: tw-一把钥匙就够\nproviders:\n  \
             - name: 本机\n    base_url: http://{upstream}\n    key: sk-x\n    billing: free\n\
             security:\n  redact:\n    mode: enforce\n"
        ),
        stored.as_bytes(),
    );

    let v = replay(&app, "本机").await;
    let sent = seen.lock().unwrap().clone();
    assert!(
        sent.contains("旧的 <<TW_SECRET_1>>，新的 <<TW_SECRET_2>>"),
        "{sent}"
    );
    // 1 号没有原值，原样留着；2 号还原成新找到的那把（给人看的打了码）
    let body = v["body"].as_str().unwrap();
    assert!(
        body.contains("旧的 <<TW_SECRET_1>>，新的 sk-an…AAAA"),
        "{v}"
    );
}
