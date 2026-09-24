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

/// 一个假的系统代理：谁的请求进来都回 503，和真机上撞到的那一页一样。
///
/// **同一个测试进程里只设一次**：reqwest 建 client 时读环境变量，这个文件里的
/// 测试都在它之后建 client。代理跑在自己的线程和运行时上 —— 每个
/// `#[tokio::test]` 有自己的运行时，挂在头一个测试上的话，那个测试一结束它就没了
fn system_proxy() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                tx.send(answering(503, "via the system proxy").await)
                    .unwrap();
                std::future::pending::<()>().await
            });
        });
        let url = format!("http://{}", rx.recv().unwrap());
        // SAFETY: 这个测试文件里只有这里写环境变量，而且写在任何 client 建起来之前
        unsafe {
            for k in [
                "HTTP_PROXY",
                "http_proxy",
                "HTTPS_PROXY",
                "https_proxy",
                "ALL_PROXY",
            ] {
                std::env::set_var(k, &url);
            }
            for k in ["NO_PROXY", "no_proxy"] {
                std::env::remove_var(k);
            }
        }
    });
}

fn row(id: i64, provider: &str) -> tw_store::db::RequestRow {
    tw_store::db::RequestRow {
        key_masked: None,
        peer: None,
        id,
        at_ms: 1000 + id,
        client: "我".into(),
        client_hint: None,
        session: None,
        provider: provider.into(),
        model: "claude-sonnet-4-5".into(),
        path: "/v1/messages".into(),
        status: Some(200),
        ttfb_ms: Some(100),
        duration_ms: Some(200),
        bytes: Some(10),
        input_tokens: Some(5),
        output_tokens: Some(2),
        cache_read_tokens: None,
        cache_write_tokens: None,
        cost_micros: Some(0),
        cost_estimated: false,
        error: None,
        local: false,
        cancelled: false,
        routing: None,
        billing: "free".into(),
        cache_saved_micros: None,
        price_source: None,
        translated: None,
    }
}

/// 一个记着一条请求（连同请求体）的控制面。
fn app(config: &str) -> (tempfile::TempDir, axum::Router) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, config).unwrap();
    let db = tw_store::Db::open(&d.path().join("data.db")).unwrap();
    let r = row(1, "本机");
    db.insert(&r).unwrap();
    let blobs = tw_store::Blobs::new(d.path().join("blobs"));
    let body = br#"{"model":"claude-sonnet-4-5","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#;
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
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        store: Some(Arc::new(tokio::sync::Mutex::new(rec))),
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
        home: d.path().join("home"),
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

/// 直连的本机上游：系统里开着代理也不走它。数据面转发这一家时就是这样。
#[tokio::test]
async fn a_direct_upstream_is_replayed_directly_even_with_a_system_proxy() {
    system_proxy();
    let upstream = answering(200, "from the upstream").await;
    let (_d, app) = app(&format!(
        "version: 1\nclients:\n  - name: 我\n    key: tw-一把钥匙就够\nproviders:\n  \
         - name: 本机\n    base_url: http://{upstream}\n    key: sk-x\n    billing: free\n"
    ));

    let v = replay(&app, "本机").await;
    assert_eq!(v["status"], 200, "{v}");
    assert_eq!(v["body"], "from the upstream", "{v}");
}

/// 指定了代理的上游：重放走它的代理，而不是直连或系统代理。
#[tokio::test]
async fn an_upstream_with_a_proxy_is_replayed_through_that_proxy() {
    system_proxy();
    let proxy = answering(200, "via its own proxy").await;
    // 没有人听的地址：直连的话只会连不上
    let (_d, app) = app(&format!(
        "version: 1\nclients:\n  - name: 我\n    key: tw-一把钥匙就够\nproxies:\n  \
         - {{ name: 自己的, type: http, addr: \"{proxy}\" }}\nproviders:\n  \
         - name: 本机\n    base_url: http://127.0.0.1:9\n    key: sk-x\n    billing: free\n    \
         proxy: 自己的\n"
    ));

    let v = replay(&app, "本机").await;
    assert_eq!(v["status"], 200, "{v}");
    assert_eq!(v["body"], "via its own proxy", "{v}");
}
