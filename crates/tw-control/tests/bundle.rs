//! 诊断包和请求重放的几条纪律：诊断包里没有明文密钥、没有正文，给人读的；
//! 重放先报价再花钱，截断过的正文不拿去重放。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

const BASE: &str = "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: 我\n    key: tw-一把钥匙就够\nproviders:\n  - name: 官方\n    base_url: https://api.anthropic.com\n    key: sk-x\n";

struct Bed {
    _dir: tempfile::TempDir,
    app: axum::Router,
}

fn bed() -> Bed {
    bed_with_store(None)
}

fn bed_with_store(store: Option<std::sync::Arc<tokio::sync::Mutex<tw_store::Recorder>>>) -> Bed {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, BASE).unwrap();
    let cfg: tw_config::Config = serde_yaml_ng::from_str(BASE).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let state = ControlState {
        shutdown: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        store,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    };
    Bed {
        app: tw_control::router(state),
        _dir: d,
    }
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, String) {
    let r = app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let st = r.status();
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (st, String::from_utf8_lossy(&b).to_string())
}

async fn post(app: &axum::Router, path: &str, body: &str) -> (StatusCode, String) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let st = r.status();
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (st, String::from_utf8_lossy(&b).to_string())
}

// ---------------------------------------------------------------- 请求重放

#[tokio::test]
async fn replaying_a_truncated_body_is_refused_rather_than_misleading() {
    // **截断之后的 body 是另一个请求。**拿它跑出来的结果去比对，比不跑
    // 更糟 —— 用户会以为那是同一条。
    let d = tempfile::tempdir().unwrap();
    let db = tw_store::Db::open(&d.path().join("data.db")).unwrap();
    let blobs = tw_store::Blobs::new(d.path().join("blobs"));
    let mut row = tw_store::db::RequestRow {
        key_masked: None,
        peer: None,
        id: 1,
        at_ms: 1000,
        client: "我".into(),
        client_hint: None,
        session: None,
        provider: "relay".into(),
        model: "claude-sonnet-4-5".into(),
        path: "/v1/messages".into(),
        status: Some(200),
        ttfb_ms: Some(100),
        duration_ms: Some(200),
        bytes: Some(10),
        input_tokens: Some(50),
        output_tokens: Some(20),
        cache_read_tokens: None,
        cache_write_tokens: None,
        cost_micros: None,
        cost_estimated: false,
        error: None,
        local: false,
        cancelled: false,
        routing: None,
        billing: tw_api::Billing::PerToken,
        cache_saved_micros: None,
        price_source: None,
        translated: None,
    };
    row.id = 1;
    db.insert(&row).unwrap();
    // 存的时候说清「原本更长」
    blobs.put_with_len(1000, 1, tw_store::Which::Request, b"half", 9_999_999);

    let rec = tw_store::Recorder::new(
        db,
        blobs,
        tw_pricing::shared(tw_pricing::PriceBook::builtin().unwrap()),
    );
    let store = std::sync::Arc::new(tokio::sync::Mutex::new(rec));

    let b = bed_with_store(Some(store));
    let (st, body) = post(&b.app, "/replay/quote", r#"{"id":1,"provider":"官方"}"#).await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("cannot be replayed as it was"), "{body}");
}

#[tokio::test]
async fn a_quote_is_required_before_spending_money() {
    // 和 L3 测速同一条纪律：报价和真跑是两个端点。
    let b = bed();
    // 没有观测层时两个端点都该明说，而不是假装成功
    let (st, _) = post(&b.app, "/replay/quote", r#"{"id":1,"provider":"官方"}"#).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    let (st, _) = post(&b.app, "/replay/run", r#"{"id":1,"provider":"官方"}"#).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn replaying_a_request_that_is_gone_says_so() {
    let d = tempfile::tempdir().unwrap();
    let db = tw_store::Db::open(&d.path().join("data.db")).unwrap();
    let blobs = tw_store::Blobs::new(d.path().join("blobs"));
    let rec = tw_store::Recorder::new(
        db,
        blobs,
        tw_pricing::shared(tw_pricing::PriceBook::builtin().unwrap()),
    );
    let b = bed_with_store(Some(std::sync::Arc::new(tokio::sync::Mutex::new(rec))));
    let (st, body) = post(&b.app, "/replay/quote", r#"{"id":42,"provider":"官方"}"#).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
}

// ---------------------------------------------------------------- 诊断包

#[tokio::test]
async fn the_diagnostic_bundle_never_carries_a_key_in_the_clear() {
    // **我们是一个看得见所有 API key 的网关**，而这份东西会被贴进 issue。
    // 这条测试是那句话的全部保障。
    let b = bed();
    let (st, text) = get(&b.app, "/diagnostics").await;
    assert_eq!(st, StatusCode::OK);
    // 配置里那两把
    assert!(
        !text.contains("tw-一把钥匙就够"),
        "网关密钥漏出来了：\n{text}"
    );
    assert!(!text.contains("sk-x"), "上游密钥漏出来了：\n{text}");
    // 但要说得出有几把、叫什么 —— 排查时那是有用的
    assert!(text.contains("我"), "{text}");
    assert!(text.contains("官方"), "{text}");
}

#[tokio::test]
async fn the_bundle_is_something_a_person_will_actually_read() {
    // **用户在交出去之前会看一眼；看不懂的东西他不会看**，也就没法发现
    // 里面有什么不该有的。所以是 Markdown 不是 JSON dump。
    let b = bed();
    let (_, text) = get(&b.app, "/diagnostics").await;
    assert!(
        text.starts_with("# ThinkWatch diagnostics bundle"),
        "{text}"
    );
    for section in [
        "## Versions",
        "## Listening",
        "## Upstreams",
        "## Security",
        "## Request recording",
        "## config.yaml",
    ] {
        assert!(text.contains(section), "少了 {section}：\n{text}");
    }
    // 第一屏就要提醒他自己检查一遍
    assert!(text.contains("read this through once more"), "{text}");
}

#[tokio::test]
async fn the_bundle_says_it_has_no_bodies_because_that_is_the_dangerous_part() {
    // 请求体最有用也最危险。需要的话在请求详情页里单独看 —— 那一页是
    // 他自己打开的，不会被顺手贴进 issue。
    let b = bed();
    let (_, text) = get(&b.app, "/diagnostics").await;
    assert!(
        text.contains("carries no request or response bodies"),
        "{text}"
    );
}

#[tokio::test]
async fn a_bundle_without_observability_says_so_rather_than_showing_zeros() {
    // 「没有记录」和「记录了零条」是两个结论。
    let b = bed();
    let (_, text) = get(&b.app, "/diagnostics").await;
    assert!(
        text.contains("Not running, so nothing from this period was recorded"),
        "{text}"
    );
    assert!(!text.contains("Requests recorded | 0"), "{text}");
}
