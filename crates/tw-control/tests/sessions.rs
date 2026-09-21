//! 会话端点的形状。
//!
//! 各种轮次怎么数在 tw-store 里测；这里盯的是**界面拿到的那份 JSON**。
//! 订阅那几轮要作为一个数、每一轮的计费方式要作为一个字段送到界面 ——
//! 否则界面手里只有「没有金额」，只能把订阅说成「无法计价」。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

const BASE: &str = "version: 1\nclients:\n  - name: 我\n    key: tw-一把钥匙就够\nproviders:\n  - name: 官方\n    base_url: https://api.anthropic.com\n    key: sk-x\n";

/// 一轮 Codex 的请求。按 token 计费的那一轮带着金额，订阅制的没有。
fn turn(id: i64, billing: &str) -> tw_store::db::RequestRow {
    let per_token = billing == "per-token";
    tw_store::db::RequestRow {
        id,
        at_ms: 1000 + id,
        client: "codex".into(),
        client_hint: None,
        session: Some("s1".into()),
        tool_calls: None,
        flagged: None,
        redacted: None,
        provider: if per_token { "官方" } else { "ChatGPT" }.into(),
        model: "gpt-5-codex".into(),
        path: "/v1/responses".into(),
        status: Some(200),
        ttfb_ms: Some(100),
        duration_ms: Some(200),
        bytes: Some(10),
        input_tokens: Some(50),
        output_tokens: Some(20),
        cache_read_tokens: None,
        cache_write_tokens: None,
        cost_micros: per_token.then_some(1_000),
        cost_estimated: false,
        error: None,
        local: false,
        cancelled: false,
        routing: None,
        billing: billing.into(),
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
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        gateway_addr: None,
        store: Some(Arc::new(tokio::sync::Mutex::new(rec))),
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        home: d.path().join("home"),
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

/// 报这个问题的那种会话：Codex 走 ChatGPT 账号，一轮按量计费的都没有。
/// **界面要能说「订阅额度 2 轮」，而不是「无法计价」。**
#[tokio::test]
async fn a_session_served_by_a_subscription_says_so_rather_than_looking_unpriced() {
    let (_d, app) = app(&[turn(1, "subscription"), turn(2, "subscription")]);

    let list = get(&app, "/sessions").await;
    let s = &list[0];
    assert_eq!(s["priced_turns"], 0);
    assert_eq!(s["unpriced_turns"], 0);
    assert_eq!(s["no_usage_turns"], 0);
    assert_eq!(s["subscription_turns"], 2, "{s}");

    let detail = get(&app, "/sessions/s1").await;
    assert_eq!(detail["session"]["subscription_turns"], 2);
    for t in detail["turns"].as_array().unwrap() {
        assert_eq!(t["cost_micros"], serde_json::Value::Null);
        assert_eq!(t["billing"], "subscription", "{t}");
    }
}

/// 混着走的会话：金额只合计按量计费的那一轮，订阅那一轮单独数。
#[tokio::test]
async fn a_mixed_session_keeps_the_money_and_the_subscription_apart() {
    let (_d, app) = app(&[turn(1, "per-token"), turn(2, "subscription")]);

    let s = &get(&app, "/sessions").await[0];
    assert_eq!(s["cost_micros"], 1_000);
    assert_eq!(s["priced_turns"], 1);
    assert_eq!(s["subscription_turns"], 1);

    let detail = get(&app, "/sessions/s1").await;
    let billing: Vec<_> = detail["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["billing"].as_str().unwrap())
        .collect();
    assert_eq!(billing, ["per-token", "subscription"]);
}

/// **请求和会话是同一批记录的两个粒度，界面要能在两者之间走动。**
///
/// 少了这个字段，两个粒度之间就没有门：看着一条很贵的请求，问不出它
/// 属于哪次任务。库里这一列一直都在，只是没有交出来。
#[tokio::test]
async fn a_request_in_the_history_says_which_session_it_belongs_to() {
    let mut orphan = turn(3, "per-token");
    orphan.session = None;
    let (_d, app) = app(&[turn(1, "per-token"), turn(2, "per-token"), orphan]);

    let rows = get(&app, "/history?limit=10").await;
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 3);

    let by_id = |id: i64| {
        rows.iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("没有 id={id} 这一行"))
    };
    assert_eq!(by_id(1)["session"], "s1");
    assert_eq!(by_id(2)["session"], "s1");
    // 认不出会话的请求（拼不出指纹的、老记录）不该冒充属于某一次
    assert!(by_id(3).get("session").is_none(), "{}", by_id(3));

    // 会话那一端报的是同一个 id —— 两边对得上才走得通
    let list = get(&app, "/sessions").await;
    assert_eq!(list[0]["id"], "s1");
}

/// 时间窗走到端点上是不是还成立。
///
/// **query string 这一层单独验。**`Window` 上面那段注释记着一次教训：
/// 不带参数时一切正常，带上时间窗才 400 —— 而单元测试看不见它，它只在
/// 真的经过一次 query string 解析时才发生。
#[tokio::test]
async fn the_endpoints_take_a_time_window_off_the_query_string() {
    // turn(id) 的 at_ms 是 1000 + id
    let (_d, app) = app(&[turn(1, "per-token"), turn(2, "per-token")]);

    let all = get(&app, "/history?limit=10").await;
    assert_eq!(all.as_array().unwrap().len(), 2);

    let narrowed = get(&app, "/history?from_ms=1002&to_ms=1002&limit=10").await;
    let narrowed = narrowed.as_array().unwrap();
    assert_eq!(narrowed.len(), 1, "{narrowed:?}");
    assert_eq!(narrowed[0]["id"], 2);

    // 只给一端：从那时起到现在
    let from_only = get(&app, "/history?from_ms=1002").await;
    assert_eq!(from_only.as_array().unwrap().len(), 1);

    // 会话那一端：窗口只盖住第二轮，整条会话照样两轮
    let s = get(&app, "/sessions?from_ms=1002&to_ms=1002").await;
    assert_eq!(s.as_array().unwrap().len(), 1);
    assert_eq!(s[0]["turns"], 2, "跨边界的会话被截断了：{s}");

    // 完全错开的窗口筛得掉
    let none = get(&app, "/sessions?from_ms=9000&to_ms=9999").await;
    assert!(none.as_array().unwrap().is_empty(), "{none}");
}
