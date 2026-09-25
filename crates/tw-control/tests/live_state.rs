//! 界面不靠轮询也能对得上的那几样**现状**。
//!
//! 变化走事件流，但界面总有晚到的时候：窗口才打开、页面才挂上、事件流掉了几条。
//! 那时它问的是这里 —— 所以事件说过的每一种状态，这里都要答得出来：还在跑的
//! 请求、被拒的凭据、不通的代理。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

mod common;
use common::spare_port;

fn control(
    d: &tempfile::TempDir,
    yaml: &str,
    store: Option<Arc<tokio::sync::Mutex<tw_store::Recorder>>>,
) -> (tw_gateway::AppState, axum::Router) {
    let p = d.path().join("config.yaml");
    std::fs::write(&p, yaml).unwrap();
    let cfg: tw_config::Config = serde_yaml_ng::from_str(yaml).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let state = ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw.clone(),
        store,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    };
    (gw, tw_control::router(state))
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, serde_json::Value) {
    let r = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = r.status();
    let body = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or_default())
}

/// 把网关真的跑起来：凭据和代理的状态只有真实转发才会碰到。
async fn serve(gw: tw_gateway::AppState) -> SocketAddr {
    let addr = tw_gateway::serve(gw, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    addr
}

async fn ask(gw: SocketAddr) -> u16 {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"claude-sonnet-4-5","max_tokens":16,"messages":[]}"#)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// 一个确定没人在听的地址（见 [`spare_port`]）。
fn dead_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], spare_port()))
}

// ---------------------------------------------------------------- 还在跑的请求

const BASE: &str = "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: 我\n    key: tw-k\nproviders:\n  - name: 官方\n    base_url: https://api.anthropic.com\n    key: sk-x\n";

fn recorder(d: &tempfile::TempDir) -> Arc<tokio::sync::Mutex<tw_store::Recorder>> {
    Arc::new(tokio::sync::Mutex::new(tw_store::Recorder::new(
        tw_store::Db::open(&d.path().join("data.db")).unwrap(),
        tw_store::Blobs::new(d.path().join("blobs")),
        tw_pricing::shared(tw_pricing::PriceBook::builtin().unwrap()),
    )))
}

fn started(id: u64) -> tw_api::Event {
    tw_api::Event::RequestStarted {
        id,
        client: "我".into(),
        client_hint: Some("claude-code".into()),
        session_fp: None,
        peer: None,
        key_masked: None,
        provider: "官方".into(),
        billing: tw_api::Billing::PerToken,
        model: "claude-sonnet-4-5".into(),
        method: "POST".into(),
        path: "/v1/messages".into(),
        at_ms: 1_758_000_000_000,
    }
}

/// **记录要等结局才落库**，而详情里最常被点开的恰恰是那个跑了很久的请求。
/// 以前点开它得到的是「没有这个请求」，结束了也不会自己补上。
#[tokio::test]
async fn a_request_still_running_can_be_opened_and_becomes_whole_when_it_ends() {
    let d = tempfile::tempdir().unwrap();
    let store = recorder(&d);
    let (_, app) = control(&d, BASE, Some(store.clone()));
    {
        let mut rec = store.lock().await;
        rec.on_event(&started(7));
        rec.record_body(
            1_758_000_000_000,
            7,
            tw_store::Which::Request,
            br#"{"model":"claude-sonnet-4-5"}"#,
            29,
        );
        rec.on_event(&tw_api::Event::RequestHeaders {
            id: 7,
            status: 200,
            ttfb_ms: 900,
        });
        rec.on_event(&tw_api::Event::RequestRouted {
            id: 7,
            rule: "catch-all".into(),
            group: Some("__all__".into()),
            attempts: vec![tw_api::AttemptView {
                provider: "官方".into(),
                outcome: tw_api::AttemptOutcome::Served,
                status: Some(200),
                error: None,
                ms: 900,
            }],
            billing: tw_api::Billing::PerToken,
        });
    }

    let (st, v) = get(&app, "/request/7").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["in_flight"], true);
    // 到目前为止知道的：身份、上游、响应头、尝试链
    assert_eq!(v["row"]["model"], "claude-sonnet-4-5");
    assert_eq!(v["row"]["status"], 200);
    assert_eq!(v["row"]["ttfb_ms"], 900);
    assert_eq!(v["row"]["routing"]["rule"], "catch-all");
    // 还没有的就是没有：耗时、用量、金额
    assert!(v["row"]["duration_ms"].is_null(), "{v}");
    assert!(v["row"]["input_tokens"].is_null(), "{v}");
    assert!(v["row"]["cost_micros"].is_null(), "{v}");
    // 请求体开始时就存下了，响应体要等结局
    assert!(
        v["request_body"]["text"]
            .as_str()
            .is_some_and(|t| t.contains("claude-sonnet-4-5")),
        "{v}"
    );
    assert!(v["response_body"].is_null(), "{v}");

    store
        .lock()
        .await
        .on_event(&tw_api::Event::RequestFinished {
            id: 7,
            model: "claude-sonnet-4-5".into(),
            status: 200,
            bytes: 120,
            duration_ms: 4_000,
            usage: None,
        });
    let (st, v) = get(&app, "/request/7").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["in_flight"], false, "结束了还说在跑");
    assert_eq!(v["row"]["duration_ms"], 4_000);
}

#[tokio::test]
async fn a_request_that_never_started_is_still_not_found() {
    let d = tempfile::tempdir().unwrap();
    let (_, app) = control(&d, BASE, Some(recorder(&d)));
    let (st, _) = get(&app, "/request/99").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------- 凭据与代理

/// **熔断看不见被拒的凭据**（4xx 不算失败），所以它要自己一项。以前只有那一条
/// 事件：界面晚打开，上游页上就看不出是哪一家的凭据坏了。
#[tokio::test]
async fn the_overview_says_which_upstream_rejects_its_credential() {
    let up = {
        let app = axum::Router::new().fallback(|| async { StatusCode::UNAUTHORIZED });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        a
    };
    let d = tempfile::tempdir().unwrap();
    let yaml = format!(
        "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: 我\n    key: tw-k\nproviders:\n  - name: 中转\n    base_url: http://{up}\n    protocol: anthropic\n    key: sk-stale\n"
    );
    let (gw, app) = control(&d, &yaml, None);
    let (_, v) = get(&app, "/overview").await;
    assert!(v["providers"][0]["auth_rejected"].is_null(), "{v}");

    let addr = serve(gw).await;
    assert_eq!(ask(addr).await, 401);
    let (_, v) = get(&app, "/overview").await;
    assert_eq!(v["providers"][0]["auth_rejected"], 401, "{v}");
}

/// 代理挂掉的现状。**不定时探测**：经它的转发失败之后才检一次，检出来的原因要
/// 留着，页面什么时候打开都看得见。
#[tokio::test]
async fn the_overview_says_which_proxy_is_down_and_why() {
    let dead = dead_addr();
    let d = tempfile::tempdir().unwrap();
    let yaml = format!(
        "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: 我\n    key: tw-k\nproxies:\n  - name: 代理一\n    type: http\n    addr: {dead}\nproviders:\n  - name: 中转\n    base_url: http://127.0.0.1:9\n    protocol: anthropic\n    key: sk-x\n    proxy: 代理一\n"
    );
    let (gw, app) = control(&d, &yaml, None);
    let mut rx = gw.bus.subscribe();
    let (_, v) = get(&app, "/overview").await;
    assert!(v["proxies"][0]["unreachable"].is_null(), "{v}");

    let addr = serve(gw).await;
    assert_ne!(ask(addr).await, 200);
    // 检查是在后台做的，等它报出来
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(tw_api::Event::ProxyChanged { state, .. }) = rx.recv().await
                && state == tw_api::ProxyState::Unreachable
            {
                return;
            }
        }
    })
    .await
    .expect("10 秒内没有检出代理不通");

    let (_, v) = get(&app, "/overview").await;
    let fault = &v["proxies"][0]["unreachable"];
    assert!(
        fault["detail"]["code"]
            .as_str()
            .is_some_and(|c| c.starts_with("l1.")),
        "要说清为什么不通：{v}"
    );
    assert_eq!(fault["failed"]["peer"], "proxy", "{v}");
}
