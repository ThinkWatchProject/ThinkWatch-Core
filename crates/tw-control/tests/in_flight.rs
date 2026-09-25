//! `/in-flight`：此刻还在跑的请求。
//!
//! 半路才来听事件流的一方拿它补上漏掉的事件。这里盯的是**界面拿到的那份
//! JSON**：core 的时钟，加上每个在跑的请求到目前为止的事件 —— 和事件流里同一个
//! 形状；有了结局的不在里面。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

const BASE: &str = "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: 我\n    key: tw-一把钥匙就够\nproviders:\n  - name: 官方\n    base_url: https://api.anthropic.com\n    key: sk-x\n";

fn app() -> (tempfile::TempDir, tw_observe::EventBus, axum::Router) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, BASE).unwrap();
    let cfg: tw_config::Config = serde_yaml_ng::from_str(BASE).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let state = ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus.clone())),
        gateway: gw,
        store: None,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    };
    (d, bus, tw_control::router(state))
}

fn started(id: u64, model: &str) -> tw_api::Event {
    tw_api::Event::RequestStarted {
        key_masked: None,
        peer: None,
        id,
        client: "我".into(),
        client_hint: Some("claude-code".into()),
        session: None,
        route: "default".into(),
        rule: "catch-all".into(),
        group: None,
        rewritten_by: vec![],
        provider: "官方".into(),
        billing: tw_api::Billing::PerToken,
        model: model.into(),
        method: "POST".into(),
        path: "/v1/messages".into(),
        session_log_bytes: None,
        at_ms: 1_000 + id,
    }
}

async fn get(router: axum::Router) -> serde_json::Value {
    let resp = router
        .oneshot(Request::get("/in-flight").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// 开始了没结束的在里面，结束了的不在；每一个都带着到目前为止的事件，第一条是
/// 完整的开始事件，之后是响应头、路由 —— 界面按顺序过一遍自己处理事件的代码，
/// 就和从头听起一样。
#[tokio::test]
async fn it_gives_every_running_request_with_what_has_happened_to_it_so_far() {
    let (_d, bus, router) = app();
    bus.emit(started(1, "claude-sonnet-5"));
    bus.emit(started(2, "gpt-5.5"));
    bus.emit(tw_api::Event::RequestHeaders {
        id: 2,
        status: 200,
        ttfb_ms: 700,
    });
    bus.emit(tw_api::Event::RequestRouted {
        id: 2,
        route: "default".into(),
        rule: "catch-all".into(),
        group: None,
        rewritten_by: vec![],
        denied_by: None,
        attempts: vec![tw_api::AttemptView {
            provider: "官方".into(),
            outcome: tw_api::AttemptOutcome::Served,
            status: Some(200),
            error: None,
            ms: 700,
        }],
        billing: tw_api::Billing::PerToken,
    });
    bus.emit(tw_api::Event::RequestFinished {
        id: 1,
        model: "claude-sonnet-5".into(),
        status: 200,
        bytes: 10,
        duration_ms: 5,
        usage: None,
    });

    let got = get(router).await;
    // **core 的时钟**：界面拿它减 `at_ms` 算跑了多久，不用自己的时钟
    assert!(
        got["now_ms"]
            .as_u64()
            .is_some_and(|t| t > 1_700_000_000_000),
        "{got}"
    );
    let requests = got["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 1, "{got}");
    assert_eq!(requests[0]["id"], 2);
    let events = requests[0]["events"].as_array().unwrap();
    // **和事件流里同一个形状** —— 界面拿它们原样补上
    assert_eq!(
        events
            .iter()
            .map(|e| e["kind"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["request_started", "request_headers", "request_routed"]
    );
    assert_eq!(events[0]["model"], "gpt-5.5");
    assert_eq!(events[0]["client"], "我");
    assert_eq!(events[0]["route"], "default");
    assert_eq!(events[0]["at_ms"], 1_002);
    assert_eq!(events[1]["status"], 200);
    assert_eq!(events[2]["attempts"][0]["provider"], "官方");
}

/// 什么都没在跑：一个空的列表，不是 404，也不是 null。
#[tokio::test]
async fn nothing_running_is_an_empty_list() {
    let (_d, _bus, router) = app();
    let got = get(router).await;
    assert_eq!(got["requests"], serde_json::json!([]), "{got}");
}

async fn live(router: axum::Router) -> serde_json::Value {
    let resp = router
        .oneshot(Request::get("/live").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// `/live`：菜单栏要的在跑的请求和生成速率，由 core 数好。
#[tokio::test]
async fn live_gives_the_running_requests_and_the_generation_rate() {
    let (_d, bus, router) = app();
    let idle = live(router.clone()).await;
    assert_eq!(idle["running"], serde_json::json!([]));
    // 这一分钟里没有跑完的：是空，不是 0
    assert!(idle["tokens_per_sec"].is_null(), "{idle}");

    bus.emit(started(1, "claude-sonnet-5"));
    bus.emit(tw_api::Event::RequestHeaders {
        id: 1,
        status: 200,
        ttfb_ms: 1_000,
    });
    bus.emit(tw_api::Event::RequestFinished {
        id: 1,
        model: "claude-sonnet-5".into(),
        status: 200,
        bytes: 10,
        duration_ms: 3_000,
        usage: Some(tw_api::UsageView {
            output: 100,
            ..Default::default()
        }),
    });
    bus.emit(started(2, "gpt-5.5"));

    let v = live(router).await;
    assert_eq!(v["tokens_per_sec"], 50, "{v}");
    assert_eq!(v["running"].as_array().unwrap().len(), 1);
    let r = &v["running"][0];
    assert_eq!(r["id"], 2);
    assert_eq!(r["client"], "我");
    assert_eq!(r["client_hint"], "claude-code");
    assert_eq!(r["model"], "gpt-5.5");
    assert_eq!(r["at_ms"], 1_002);
    // 路由的第一阶段开始时就有；跑了多久是 core 数的
    assert_eq!(r["route"], "default");
    assert_eq!(r["rule"], "catch-all");
    assert!(r["elapsed_ms"].is_u64(), "{v}");
    // 路由还没报出结论：不知道是哪家接下的
    assert!(r["upstream"].is_null(), "{v}");
}
