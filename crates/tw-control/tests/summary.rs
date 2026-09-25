//! 汇总端点的形状。
//!
//! 各种请求怎么数在 tw-store 里测；这里盯的是**界面拿到的那份 JSON**：按模型
//! 分层的趋势图上，每一格的每一项都要自己说出缺着多少钱。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

const BASE: &str = "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: 我\n    key: tw-一把钥匙就够\nproviders:\n  - name: 官方\n    base_url: https://api.anthropic.com\n    key: sk-x\n";

/// 一条有用量、有价格的请求。
fn req(id: i64, model: &str) -> tw_store::db::RequestRow {
    tw_store::db::RequestRow {
        key_masked: None,
        peer: None,
        id,
        at_ms: 1000 + id,
        client: "我".into(),
        client_hint: None,
        session: None,
        provider: "官方".into(),
        model: model.into(),
        path: "/v1/messages".into(),
        status: Some(200),
        ttfb_ms: Some(100),
        duration_ms: Some(200),
        bytes: Some(10),
        input_tokens: Some(50),
        output_tokens: Some(20),
        cache_read_tokens: None,
        cache_write_tokens: None,
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

/// 趋势图的每一格、每一项都带着「没有价格」和「没有用量」的条数。
///
/// 金额是 0 的那一项，界面要分得清是价目表里没有它、上游没报用量，还是确实
/// 没花钱 —— 以前只有整格的数，只能笼统地说「这一格少算了」。
#[tokio::test]
async fn every_group_in_the_trend_says_how_much_of_its_money_is_missing() {
    let mut unknown = req(2, "中转站自己起的名字");
    unknown.cost_micros = None;
    let mut silent = req(3, "claude-sonnet-4-5");
    silent.input_tokens = None;
    silent.output_tokens = None;
    silent.cost_micros = None;
    let (_d, app) = app(&[req(1, "claude-sonnet-4-5"), unknown, silent]);

    let v = get(
        &app,
        "/summary/buckets/by?from_ms=0&to_ms=10000&bucket_ms=10000&dim=model",
    )
    .await;
    let groups = v.as_array().unwrap();
    assert_eq!(groups.len(), 2, "{v}");
    let group = |name: &str| {
        groups
            .iter()
            .find(|g| g["name"] == name)
            .unwrap_or_else(|| panic!("没有「{name}」这一项：{v}"))
    };
    let sonnet = group("claude-sonnet-4-5");
    assert_eq!(sonnet["requests"], 2);
    assert_eq!(sonnet["cost_micros_exact"], 1_000);
    assert_eq!(sonnet["unpriced_requests"], 0, "{sonnet}");
    assert_eq!(sonnet["no_usage_requests"], 1, "{sonnet}");
    // 金额是 0，但不是免费的：价目表里没有它
    let unknown = group("中转站自己起的名字");
    assert_eq!(unknown["cost_micros_exact"], 0);
    assert_eq!(unknown["unpriced_requests"], 1, "{unknown}");
    assert_eq!(unknown["no_usage_requests"], 0, "{unknown}");

    // 按上游、按密钥分也一样带着；这三条同一家、同一把，合成一项
    for dim in ["provider", "client"] {
        let v = get(
            &app,
            &format!("/summary/buckets/by?from_ms=0&to_ms=10000&bucket_ms=10000&dim={dim}"),
        )
        .await;
        assert_eq!(v.as_array().unwrap().len(), 1, "{dim}: {v}");
        assert_eq!(v[0]["unpriced_requests"], 1, "{dim}: {v}");
        assert_eq!(v[0]["no_usage_requests"], 1, "{dim}: {v}");
    }

    // 一条都没有的一段：没有组
    let v = get(
        &app,
        "/summary/buckets/by?from_ms=20000&to_ms=30000&bucket_ms=10000&dim=model",
    )
    .await;
    assert_eq!(v, serde_json::json!([]));
}
