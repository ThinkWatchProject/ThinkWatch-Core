//! `/in-flight`：此刻还在跑的请求。
//!
//! 半路才来听事件流的一方拿它补上漏掉的开始事件。这里盯的是**界面拿到的
//! 那份 JSON**：和事件流里的开始事件同一个形状，有了结局的不在里面。

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
        session_fp: None,
        provider: "官方".into(),
        billing: tw_api::Billing::PerToken,
        model: model.into(),
        method: "POST".into(),
        path: "/v1/messages".into(),
        at_ms: 1_000 + id,
    }
}

async fn get(router: axum::Router) -> Vec<serde_json::Value> {
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

/// 开始了没结束的在里面，结束了的不在；每一条都是一个完整的开始事件。
#[tokio::test]
async fn it_lists_the_start_of_every_request_that_has_not_ended() {
    let (_d, bus, router) = app();
    bus.emit(started(1, "claude-sonnet-5"));
    bus.emit(started(2, "gpt-5.5"));
    bus.emit(tw_api::Event::RequestFinished {
        id: 1,
        model: "claude-sonnet-5".into(),
        status: 200,
        bytes: 10,
        duration_ms: 5,
        usage: None,
    });

    let got = get(router).await;
    assert_eq!(got.len(), 1, "{got:?}");
    // **和事件流里的开始事件同一个形状** —— 界面拿它原样补一条开始
    assert_eq!(got[0]["kind"], "request_started");
    assert_eq!(got[0]["id"], 2);
    assert_eq!(got[0]["model"], "gpt-5.5");
    assert_eq!(got[0]["client"], "我");
    assert_eq!(got[0]["at_ms"], 1_002);
}

/// 什么都没在跑：一个空数组，不是 404，也不是 null。
#[tokio::test]
async fn nothing_running_is_an_empty_list() {
    let (_d, _bus, router) = app();
    assert!(get(router).await.is_empty());
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
}
