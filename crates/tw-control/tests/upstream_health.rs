//! 上游体检端点的形状。
//!
//! 每一项怎么数在 tw-store 里测；这里盯的是**界面拿到的那份 JSON** 和缺省的时间窗：
//! 不给参数是最近 7 天，不是别的聚合那样的「今天」。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

const BASE: &str = "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: 我\n    key: tw-一把钥匙就够\nproviders:\n  - name: 中转\n    base_url: https://relay.example.com\n    key: sk-x\n";

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// 一条成功的请求：发给中转，回答里写了模型名，报了用量、有估算。
fn req(id: i64, at_ms: i64, answered: &str) -> tw_store::db::RequestRow {
    tw_store::db::RequestRow {
        session_log_bytes: None,
        key_masked: None,
        peer: None,
        id,
        at_ms,
        client: "我".into(),
        client_hint: None,
        session: None,
        provider: "中转".into(),
        model: "claude-sonnet-4-5".into(),
        sent_model: "claude-sonnet-4-5".into(),
        answered_model: Some(answered.into()),
        path: "/v1/messages".into(),
        status: Some(200),
        ttfb_ms: Some(100),
        ttft_ms: Some(700),
        duration_ms: Some(2_000),
        tokens_per_sec: Some(80),
        sent_bytes: Some(10),
        received_bytes: Some(10),
        egress: None,
        input_tokens: Some(12_000),
        output_tokens: Some(20),
        cache_read_tokens: Some(0),
        cache_write_tokens: Some(0),
        input_estimate: Some(10_000),
        cost_micros: Some(1_000),
        cost_estimated: false,
        error: None,
        local: false,
        cancelled: false,
        routing: None,
        billing: tw_api::Billing::PerToken,
        cache_saved_micros: None,
        price_source: None,
        translated: None,
    }
}

fn app(rows: &[tw_store::db::RequestRow]) -> (tempfile::TempDir, axum::Router) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, BASE).unwrap();
    let db = tw_store::Db::open(&d.path().join("data.db")).unwrap();
    for r in rows {
        db.insert(r).unwrap();
    }
    let rec = tw_store::Recorder::new(
        db,
        tw_store::Blobs::new(d.path().join("blobs")),
        tw_pricing::shared(tw_pricing::PriceBook::builtin().unwrap()),
    );
    let cfg: tw_config::Config = serde_yaml_ng::from_str(BASE).unwrap();
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

async fn get(app: &axum::Router, path: &str) -> serde_json::Value {
    let r = app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK, "{path}");
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&b).unwrap()
}

/// 不给时间窗：**最近 7 天**。六天前的那条在里面，八天前的不在
#[tokio::test]
async fn without_a_window_it_looks_at_the_last_seven_days() {
    let now = now_ms();
    let day = 86_400_000;
    let (_d, app) = app(&[
        req(1, now - 6 * day, "claude-sonnet-4-5-20250929"),
        req(2, now - 8 * day, "claude-sonnet-4-5-20250929"),
        req(3, now - 1_000, "claude-opus-4-1-20250805"),
    ]);
    let v = get(&app, "/upstreams/health").await;
    let (from, to) = (v["from_ms"].as_i64().unwrap(), v["to_ms"].as_i64().unwrap());
    assert_eq!(to - from, 7 * day, "{v}");
    assert!((to - now).abs() < 60_000, "终点该是现在：{v}");
    let relay = &v["upstreams"][0];
    assert_eq!(relay["upstream"], "中转");
    assert_eq!(relay["requests"], 2, "{relay}");
    // 回答里写的 opus 和发出去的 sonnet 对不上
    assert_eq!(relay["models"]["named"], 2, "{relay}");
    assert_eq!(relay["models"]["differed"], 1, "{relay}");
    assert_eq!(
        relay["models"]["examples"][0],
        serde_json::json!({"sent": "claude-sonnet-4-5", "answered": "claude-opus-4-1-20250805", "count": 1})
    );
    assert_eq!(relay["input"]["all"]["median"], 1.2, "{relay}");
    assert_eq!(relay["input"]["all"]["samples"], 2, "{relay}");
    assert_eq!(relay["ttft_ms"]["p50"], 700, "{relay}");
    assert_eq!(relay["tokens_per_sec"]["samples"], 2, "{relay}");
    // 库里有更早的记录：整段都是全的
    assert_eq!(v["covered_since_ms"], v["from_ms"], "{v}");
}

/// 给了时间窗就按它数；一条都没有的一段是空的清单，不是一组零
#[tokio::test]
async fn a_window_narrows_it_and_an_empty_one_has_no_upstreams() {
    let (_d, app) = app(&[req(1, 5_000, "claude-sonnet-4-5")]);
    let v = get(&app, "/upstreams/health?from_ms=0&to_ms=10000").await;
    assert_eq!(
        (v["from_ms"].as_i64(), v["to_ms"].as_i64()),
        (Some(0), Some(10_000))
    );
    assert_eq!(v["upstreams"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["covered_since_ms"], 5_000, "记录从这一条开始：{v}");

    let v = get(&app, "/upstreams/health?from_ms=20000&to_ms=30000").await;
    assert_eq!(v["upstreams"], serde_json::json!([]), "{v}");
    // 只给了终点：往前数 7 天
    let v = get(&app, "/upstreams/health?to_ms=700000000").await;
    assert_eq!(v["from_ms"].as_i64(), Some(700_000_000 - 7 * 86_400_000));
}
