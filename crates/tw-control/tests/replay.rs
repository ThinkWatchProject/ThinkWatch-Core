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
    app_of(config, body, row(1, "本机"))
}

/// 同上，存着的那一行是 `r`。
fn app_of(
    config: &str,
    body: &[u8],
    r: tw_store::db::RequestRow,
) -> (tempfile::TempDir, axum::Router) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, config).unwrap();
    let db = tw_store::Db::open(&d.path().join("data.db")).unwrap();
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
    let (status, v) = ask(app, "/replay/run", provider).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    v
}

/// 向 `path`（报价或重放）要第 1 条请求重放到 `provider`
async fn ask(app: &axum::Router, path: &str, provider: &str) -> (StatusCode, serde_json::Value) {
    let req = tw_api::ReplayRequest {
        id: 1,
        provider: provider.into(),
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
    let status = r.status();
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&b).unwrap())
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

/// 假上游收到的请求：路径和正文，按先后。
#[derive(Clone, Default)]
struct Seen(Arc<std::sync::Mutex<Vec<(String, String)>>>);

impl Seen {
    /// 最后收到的那一个
    fn last(&self) -> (String, String) {
        self.0
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("nothing was sent")
    }

    fn count(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

/// 一个把收到的请求体原样回给你的上游，顺手记下收到了什么：每一个请求的路径和正文。
async fn echoing() -> (SocketAddr, Seen) {
    let seen = Seen::default();
    let s = seen.clone();
    let app = axum::Router::new().fallback(axum::routing::post(
        move |uri: axum::http::Uri, body: String| {
            let s = s.clone();
            async move {
                s.0.lock()
                    .unwrap()
                    .push((uri.path().to_string(), body.clone()));
                body
            }
        },
    ));
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
    let (_, sent) = seen.last();
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

/// 一个别名表、几家上游（都指到同一个假上游），启用范围各不相同：
/// - `本机` 只有 `claude-*`，`中转` 只有 `anthropic/*`：`sonnet` 在两家各叫各的；
/// - `谷歌` 只有 `gemini-*`：`flash` 只有它服务得了；
/// - `别家` 只有 `gpt-*`：两个别名它都服务不了。
fn aliased(upstream: SocketAddr) -> String {
    format!(
        "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\n\
         clients:\n  - name: 我\n    key: tw-一把钥匙就够\n\
         aliases:\n  sonnet:\n    - claude-sonnet-4-5\n    - anthropic/claude-sonnet-4-5\n  \
         flash: gemini-2.5-flash\n\
         providers:\n  \
         - name: 本机\n    base_url: http://{upstream}\n    key: sk-x\n    models_only: [\"claude-*\"]\n  \
         - name: 中转\n    base_url: http://{upstream}\n    key: sk-x\n    models_only: [\"anthropic/*\"]\n  \
         - name: 谷歌\n    base_url: http://{upstream}\n    key: sk-x\n    models_only: [\"gemini-*\"]\n  \
         - name: 别家\n    base_url: http://{upstream}\n    key: sk-x\n    models_only: [\"gpt-*\"]\n"
    )
}

/// 原来那一次的尝试链：每一跳是 `(上游, 发给它的、和客户端要的不一样的模型名)`
fn routed(hops: &[(&str, Option<&str>)]) -> Option<String> {
    let attempts = hops
        .iter()
        .map(|(p, m)| tw_api::AttemptView {
            provider: p.to_string(),
            model: m.map(String::from),
            outcome: tw_api::AttemptOutcome::Served,
            status: Some(200),
            error: None,
            ms: 100,
        })
        .collect();
    Some(
        serde_json::to_string(&tw_api::RoutingView {
            route: "default".into(),
            rule: "兜底".into(),
            attempts,
            ..Default::default()
        })
        .unwrap(),
    )
}

/// 发出去的那一份请求体里的模型名
fn model_of(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    v["model"].as_str().unwrap_or_default().to_string()
}

/// 客户端要的是别名：重放到另一家，发的是**那一家自己的名字**，不是别名本身，也不是原来
/// 那一家的名字。报价也按这个名字。重放回原来那一家，发的是记录里它收到的那个
#[tokio::test]
async fn an_alias_is_replayed_under_the_chosen_upstreams_own_name() {
    let (upstream, seen) = echoing().await;
    let mut r = row(1, "本机");
    r.model = "sonnet".into();
    r.sent_model = "claude-sonnet-4-5".into();
    r.routing = routed(&[("本机", Some("claude-sonnet-4-5"))]);
    let (_d, app) = app_of(
        &aliased(upstream),
        br#"{"model":"sonnet","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#,
        r,
    );

    let (status, q) = ask(&app, "/replay/quote", "中转").await;
    assert_eq!(status, StatusCode::OK, "{q}");
    assert_eq!(q["model"], "anthropic/claude-sonnet-4-5", "{q}");

    replay(&app, "中转").await;
    let (path, sent) = seen.last();
    assert_eq!(path, "/v1/messages");
    assert_eq!(model_of(&sent), "anthropic/claude-sonnet-4-5", "{sent}");
    // 只换了模型名
    assert!(sent.contains(r#""content":"hi""#), "{sent}");

    replay(&app, "本机").await;
    assert_eq!(model_of(&seen.last().1), "claude-sonnet-4-5");
}

/// 规则把模型改写过：重放回原来那一家，发的是记录里它收到的那个名字（报价也按它）；重放到
/// 别的一家，规则没有作用在它身上，发的是客户端要的那个，请求体一个字节都不动
#[tokio::test]
async fn a_rewritten_request_goes_back_to_its_upstream_under_the_recorded_name() {
    let (upstream, seen) = echoing().await;
    let stored: &[u8] =
        br#"{"model":"claude-sonnet-4-5","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#;
    let mut r = row(1, "本机");
    r.sent_model = "claude-haiku-4-5".into();
    r.routing = routed(&[("别家", None), ("本机", Some("claude-haiku-4-5"))]);
    let (_d, app) = app_of(&aliased(upstream), stored, r);

    let (status, q) = ask(&app, "/replay/quote", "本机").await;
    assert_eq!(status, StatusCode::OK, "{q}");
    assert_eq!(q["model"], "claude-haiku-4-5", "{q}");
    // 按发出去的那个模型报价：输入是记录里的 5，输出按上次的估、至少 256
    let book = tw_pricing::PriceBook::builtin().unwrap();
    let usage = tw_pricing::Usage {
        input: 5,
        output: 256,
        ..Default::default()
    };
    let cost = |m: &str| match book.cost_for("本机", m, &usage, false) {
        tw_pricing::Cost::Known(c) | tw_pricing::Cost::Estimated(c) => c,
        other => panic!("{m} has no price: {other:?}"),
    };
    assert_ne!(cost("claude-haiku-4-5"), cost("claude-sonnet-4-5"));
    assert_eq!(q["cost_micros"], cost("claude-haiku-4-5"), "{q}");

    replay(&app, "本机").await;
    assert_eq!(model_of(&seen.last().1), "claude-haiku-4-5");

    replay(&app, "中转").await;
    assert_eq!(seen.last().1.as_bytes(), stored);
}

/// 这一家服务不了这个别名（列表里的名字它一个都没有）：报价和重放都说清楚，什么都不发
#[tokio::test]
async fn an_alias_the_upstream_cannot_serve_is_not_replayed() {
    let (upstream, seen) = echoing().await;
    let mut r = row(1, "本机");
    r.model = "sonnet".into();
    r.sent_model = "claude-sonnet-4-5".into();
    let (_d, app) = app_of(
        &aliased(upstream),
        br#"{"model":"sonnet","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#,
        r,
    );
    for path in ["/replay/quote", "/replay/run"] {
        let (status, v) = ask(&app, path, "别家").await;
        assert_eq!(status, StatusCode::CONFLICT, "{path}: {v}");
        assert_eq!(v["code"], "control.replay_alias_unserved", "{path}: {v}");
    }
    assert_eq!(seen.count(), 0);
}

/// Gemini 的模型写在路径里：换的是路径里的那一段，请求体一个字节都不动
#[tokio::test]
async fn a_gemini_alias_is_renamed_in_the_path() {
    let (upstream, seen) = echoing().await;
    let stored: &[u8] = br#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;
    let mut r = row(1, "谷歌");
    r.model = "flash".into();
    r.sent_model = "flash".into();
    r.path = "/v1beta/models/flash:generateContent".into();
    let (_d, app) = app_of(&aliased(upstream), stored, r);

    replay(&app, "谷歌").await;
    let (path, sent) = seen.last();
    assert_eq!(path, "/v1beta/models/gemini-2.5-flash:generateContent");
    assert_eq!(sent.as_bytes(), stored);
}
