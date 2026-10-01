//! `POST /history/search` 交给界面的那份 JSON。
//!
//! 怎么筛、怎么翻页、怎么读正文在 tw-store 里测；这里盯的是**线上的样子**：行和
//! `GET /history` 同一个形状（安全记录挂在行上），按正文对上的带着摘录，翻页的位置和
//! 停下的原因都在。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

const BASE: &str = "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: 我\n    key: tw-一把钥匙就够\nproviders:\n  - name: 官方\n    base_url: https://api.anthropic.com\n    key: sk-x\n";

/// 一条请求：现在开始的，`id` 越大越新
fn req(id: i64, client: &str) -> tw_store::db::RequestRow {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    tw_store::db::RequestRow {
        session_log_bytes: None,
        key_masked: None,
        peer: None,
        id,
        at_ms: now - 1000 + id,
        client: client.into(),
        client_hint: Some("claude-code".into()),
        session: None,
        provider: "官方".into(),
        model: "claude-sonnet-4-5".into(),
        path: "/v1/messages".into(),
        status: Some(200),
        ttfb_ms: Some(100),
        ttft_ms: Some(300),
        duration_ms: Some(2000),
        tokens_per_sec: Some(80),
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

/// 一个带着请求库和正文目录的控制面。`bodies`：(请求号, 请求体)
fn app(
    rows: &[tw_store::db::RequestRow],
    bodies: &[(i64, String)],
) -> (tempfile::TempDir, axum::Router) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, BASE).unwrap();
    let db = tw_store::Db::open(&d.path().join("data.db")).unwrap();
    let blobs = tw_store::Blobs::new(d.path().join("blobs"));
    for r in rows {
        db.insert(r).unwrap();
        if let Some((_, body)) = bodies.iter().find(|(id, _)| *id == r.id) {
            assert!(blobs.put(r.at_ms, r.id, tw_store::Which::Request, body.as_bytes()));
        }
    }
    db.insert_security_event(&tw_store::SecurityEvent {
        at_ms: rows[0].at_ms,
        request_id: rows[0].id,
        guard: tw_api::Guard::Redact,
        rule: "anthropic".into(),
        custom: false,
        action: tw_api::SecurityOutcome::Replaced,
        provider: "官方".into(),
        client: "我".into(),
        tool: None,
        excerpt: "sk-an…".into(),
        count: 1,
    })
    .unwrap();
    let rec = tw_store::Recorder::new(
        db,
        blobs,
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

async fn search(app: &axum::Router, q: serde_json::Value) -> serde_json::Value {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(<tw_api::ep::HistorySearch as tw_api::Endpoint>::PATH)
                .header("content-type", "application/json")
                .body(Body::from(q.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let st = r.status();
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    assert_eq!(st, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    // 照契约的类型读得回来：界面拿到的就是这个形状
    serde_json::from_slice::<tw_api::HistorySearchPage>(&b).unwrap();
    serde_json::from_slice(&b).unwrap()
}

fn ids(page: &serde_json::Value) -> Vec<i64> {
    page["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn a_record_search_pages_back_with_the_history_row_shape() {
    let rows: Vec<_> = (1..=5)
        .map(|i| req(i, if i % 2 == 0 { "codex" } else { "我" }))
        .collect();
    let (_d, app) = app(&rows, &[]);

    // 什么条件都不给：最近的在前，和 `GET /history` 一样
    let p = search(&app, serde_json::json!({})).await;
    assert_eq!(ids(&p), [5, 4, 3, 2, 1]);
    assert_eq!(p["stopped"], "end");
    assert!(p["next"].is_null(), "{p}");
    assert!(p["bodies_since_ms"].is_null(), "{p}");
    // 行和 `GET /history` 同一个样子，安全记录挂在行上
    let first = &p["rows"][4];
    assert_eq!(first["client_hint"], "claude-code");
    assert_eq!(first["security"][0]["guard"], "redact", "{first}");

    // 一页两条：`next` 交回去接着翻
    let p = search(&app, serde_json::json!({"q": "CODEX", "limit": 1})).await;
    assert_eq!(ids(&p), [4]);
    assert_eq!(p["stopped"], "full");
    let next = p["next"].clone();
    assert_eq!(next["id"], 4);
    let p = search(
        &app,
        serde_json::json!({"q": "codex", "limit": 1, "before": next}),
    )
    .await;
    assert_eq!(ids(&p), [2]);
    // 正好是最后一条：说找完了
    assert_eq!(p["stopped"], "end", "{p}");
}

#[tokio::test]
async fn a_content_search_brings_back_a_masked_excerpt() {
    let rows: Vec<_> = (1..=3).map(|i| req(i, "我")).collect();
    let body = |text: &str| {
        serde_json::json!({"model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": text}]})
        .to_string()
    };
    let (_d, app) = app(
        &rows,
        &[
            (
                1,
                body("我的 key 是 sk-ant-api03-WIREKEYAAAAAAAAAAAAAAAAAAAA，帮我看看"),
            ),
            (2, body("别的话")),
        ],
    );
    let p = search(&app, serde_json::json!({"q": "我的 key", "content": true})).await;
    assert_eq!(ids(&p), [1]);
    let hit = &p["hits"][0];
    assert_eq!(hit["id"], 1);
    assert_eq!(hit["side"], "request");
    assert_eq!(hit["matched"], "我的 key");
    let after = hit["after"].as_str().unwrap();
    assert!(after.starts_with(" 是 sk-an…"), "{hit}");
    assert!(!after.contains("WIREKEYAAAA"), "摘录里漏了密钥：{hit}");
    assert!(p["bodies_since_ms"].as_i64().is_some(), "{p}");
    assert_eq!(p["stopped"], "end");
    // 不开正文：只按记录找，正文里的词找不到
    let p = search(&app, serde_json::json!({"q": "我的 key"})).await;
    assert!(ids(&p).is_empty(), "{p}");
    assert!(p["bodies_since_ms"].is_null());
}

#[tokio::test]
async fn without_a_store_the_search_says_recording_is_unavailable() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, BASE).unwrap();
    let cfg: tw_config::Config = serde_yaml_ng::from_str(BASE).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let app = tw_control::router(ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        store: None,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    });
    let r = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/history/search")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    let m: tw_api::ErrorBody = serde_json::from_slice(&b).unwrap();
    assert_eq!(m.code, "control.store_unavailable");
}

/// 按正文找只读生成回答的请求，**认路径的办法和网关是同一个**：网关只解码这些请求，
/// 记录里存的也是它们的请求体。两边各写一份（tw-store 不能依赖网关），这里逐条核对。
#[test]
fn the_store_reads_the_same_paths_as_generation_calls_as_the_gateway() {
    use tw_gateway::client_api::ClientApi;
    for path in [
        "/v1/messages",
        "/messages",
        "/v1/messages/",
        "/v1/messages/count_tokens",
        "/v1/complete",
        "/v1/chat/completions",
        "/chat/completions",
        "/v1/completions",
        "/v1/embeddings",
        "/v1/responses",
        "/v1/responses/compact",
        "/responses",
        "/backend-api/codex/responses",
        "/backend-api/codex/responses/compact",
        "/v1beta/models/gemini-2.5-pro:generateContent",
        "/v1beta/models/gemini-2.5-pro:streamGenerateContent",
        "/v1/models/gemini-2.5-flash:generateContent",
        "/v1beta/models/gemini-2.5-pro:countTokens",
        "/v1beta/models/text-embedding-004:embedContent",
        "/v1/models",
        "/health_check",
        "/",
    ] {
        let gateway = ClientApi::of_path(path)
            .filter(|_| ClientApi::generates(path))
            .map(|a| a.dialect());
        assert_eq!(
            tw_store::search::text::client_dialect(path),
            gateway,
            "{path}"
        );
    }
}
