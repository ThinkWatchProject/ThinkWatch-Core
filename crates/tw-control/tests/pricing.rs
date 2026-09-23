//! 价目表：自定义价目表的增删改、查价、默认价目表的刷新。
//!
//! 写配置的那几条，断言落在**文件本身**上；刷新的那几条，用一个本地的
//! 假数据源 —— 测试不联网，也不碰开发者自己的配置目录。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_control::pricing::{Schedule, Updater};
use tw_control::{ConfigManager, ControlState};

const BASE: &str = "version: 1
clients:
  - name: c
    key: tw-k
providers:
  - name: 官方
    base_url: https://api.anthropic.com
    key: sk-official
  - name: relay-hk
    base_url: https://relay.example
    key: sk-relay
";

struct Bed {
    dir: tempfile::TempDir,
    state: ControlState,
    app: axum::Router,
}

impl Bed {
    fn file(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("config.yaml")).unwrap()
    }
    fn parsed(&self) -> tw_config::Config {
        tw_config::try_parse(&self.file()).unwrap()
    }
    fn data_file(&self) -> std::path::PathBuf {
        self.dir.path().join("model_prices.json")
    }
}

fn bed_with(yaml: &str, updater: Updater) -> Bed {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, yaml).unwrap();
    let cfg = tw_config::try_parse(yaml).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let state = ControlState {
        shutdown: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        store: None,
        started: std::time::Instant::now(),
        price_updater: Arc::new(updater),
        chatgpt: Default::default(),
        zai: Default::default(),
        // **测试里绝不能碰开发者自己的配置**
        home: d.path().join("home"),
    };
    Bed {
        app: tw_control::router(state.clone()),
        state,
        dir: d,
    }
}

fn bed(yaml: &str) -> Bed {
    bed_with(yaml, Updater::default())
}

async fn call(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let st = r.status();
    let b = axum::body::to_bytes(r.into_body(), 1 << 22).await.unwrap();
    let text = String::from_utf8_lossy(&b).to_string();
    let v = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
    (st, v)
}

/// 一条覆盖价：每百万 tokens 的美元，五个字段都写明。
fn full(input: f64, output: f64) -> serde_json::Value {
    serde_json::json!({
        "input": input,
        "output": output,
        "cache_read": input / 10.0,
        "cache_write_5m": input * 1.25,
        "cache_write_1h": input * 2.0,
    })
}

fn relay_sheet(name: &str) -> serde_json::Value {
    serde_json::json!({ "sheet": {
        "name": name,
        "multiplier": 0.8,
        "models": { "claude-sonnet-4-5-thinking": full(3.0, 15.0) },
    }})
}

// ─────────────────────────────────────────────────────────── 自定义价目表

#[tokio::test]
async fn a_sheet_is_created_chosen_renamed_and_deleted_only_once_nobody_uses_it() {
    let b = bed(BASE);
    let (st, body) = call(&b.app, "POST", "/pricing/sheets", relay_sheet("中转协议价")).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let sheet = b.parsed().pricing.sheets[0].clone();
    assert_eq!(sheet.multiplier, 0.8);
    assert_eq!(
        sheet.models["claude-sonnet-4-5-thinking"].cache_write_1h,
        6.0
    );

    // 上游选它
    let (st, body) = call(
        &b.app,
        "PUT",
        "/providers/relay-hk",
        serde_json::json!({ "provider": { "name": "relay-hk", "pricing": "中转协议价" } }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (_, ov) = call(&b.app, "GET", "/overview", serde_json::json!(null)).await;
    assert_eq!(
        ov["price_sheets"][0]["used_by"],
        serde_json::json!(["relay-hk"])
    );
    assert_eq!(ov["price_sheets"][0]["overrides"], 1);
    assert_eq!(ov["providers"][1]["pricing"], "中转协议价");

    // 改名：选了它的上游跟着改，同一个版本
    let (st, body) = call(
        &b.app,
        "PUT",
        "/pricing/sheets/中转协议价",
        relay_sheet("中转价"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(b.parsed().providers[1].pricing.as_deref(), Some("中转价"));

    // 还在用：删不掉，并且说清是谁在用
    let (st, body) = call(
        &b.app,
        "DELETE",
        "/pricing/sheets/中转价",
        serde_json::json!(null),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert!(
        body["text"].as_str().unwrap().contains("relay-hk"),
        "{body}"
    );

    // 上游改回默认价目表之后才能删；删完不留空段
    let (st, body) = call(
        &b.app,
        "PUT",
        "/providers/relay-hk",
        serde_json::json!({ "provider": { "name": "relay-hk" } }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = call(
        &b.app,
        "DELETE",
        "/pricing/sheets/中转价",
        serde_json::json!(null),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(b.file(), BASE);
}

#[tokio::test]
async fn a_sheet_saved_with_its_upstreams_assigns_them_in_the_same_version() {
    let b = bed(BASE);
    let versions = || {
        tw_config::history::list(&b.dir.path().join("config.yaml"))
            .unwrap()
            .len()
    };
    let mut body = relay_sheet("中转协议价");
    body["used_by"] = serde_json::json!(["relay-hk"]);
    let (st, v) = call(&b.app, "POST", "/pricing/sheets", body).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        b.parsed().providers[1].pricing.as_deref(),
        Some("中转协议价")
    );

    // 改名，同时换一家用它：原来那家改回默认价目表。**一次保存一个版本**
    let mut body = relay_sheet("中转价");
    body["used_by"] = serde_json::json!(["官方"]);
    let before = versions();
    let (st, v) = call(&b.app, "PUT", "/pricing/sheets/中转协议价", body).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(versions(), before + 1, "价目表和上游的选择是一次保存");
    let cfg = b.parsed();
    assert_eq!(cfg.providers[0].pricing.as_deref(), Some("中转价"));
    assert_eq!(cfg.providers[1].pricing, None);

    // 不给就不动上游的选择
    let (st, v) = call(
        &b.app,
        "PUT",
        "/pricing/sheets/中转价",
        relay_sheet("中转价"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(b.parsed().providers[0].pricing.as_deref(), Some("中转价"));

    // 点名一家不存在的上游：什么都不写
    let file = b.file();
    let mut body = relay_sheet("中转价");
    body["used_by"] = serde_json::json!(["没有这家"]);
    let (st, _) = call(&b.app, "PUT", "/pricing/sheets/中转价", body).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(b.file(), file);
}

#[tokio::test]
async fn a_sheets_full_definition_can_be_read_back_for_editing() {
    let b = bed(BASE);
    call(&b.app, "POST", "/pricing/sheets", relay_sheet("中转协议价")).await;
    let (st, v) = call(
        &b.app,
        "GET",
        "/pricing/sheets/中转协议价",
        serde_json::json!(null),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["multiplier"], 0.8);
    assert_eq!(
        v["models"]["claude-sonnet-4-5-thinking"]["cache_write_1h"],
        6.0
    );
    let (st, _) = call(
        &b.app,
        "GET",
        "/pricing/sheets/没有这张",
        serde_json::json!(null),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_upstream_cannot_choose_a_sheet_that_does_not_exist() {
    // 静默退回默认价的话，算出来的钱看起来正常，而折扣从没生效过
    let b = bed(BASE);
    let (st, body) = call(
        &b.app,
        "PUT",
        "/providers/relay-hk",
        serde_json::json!({ "provider": { "name": "relay-hk", "pricing": "没有这张" } }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["text"].as_str().unwrap().contains("没有这张"),
        "{body}"
    );
    assert_eq!(b.file(), BASE);
}

#[tokio::test]
async fn a_sheet_with_a_half_written_override_is_refused_with_the_reason() {
    let b = bed(BASE);
    let (st, body) = call(
        &b.app,
        "POST",
        "/pricing/sheets",
        serde_json::json!({ "sheet": {
            "name": "中转",
            "models": { "m": {
                "input": 1, "output": 2, "cache_read": 0.1, "cache_write_5m": 1.25,
                "cache_write_1h": 2, "input_above_200k": 2,
            }},
        }}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["text"].as_str().unwrap().contains("long-context"),
        "{body}"
    );
    assert_eq!(b.file(), BASE);
}

// ─────────────────────────────────────────────────────────── 查价

#[tokio::test]
async fn a_saved_sheet_prices_each_model_and_says_where_the_price_came_from() {
    let b = bed(BASE);
    call(&b.app, "POST", "/pricing/sheets", relay_sheet("中转协议价")).await;
    let (st, r) = call(
        &b.app,
        "POST",
        "/pricing/query",
        serde_json::json!({
            "sheet": { "kind": "named", "name": "中转协议价" },
            "models": ["claude-sonnet-4-5", "claude-sonnet-4-5-thinking", "中转站自己起的名字"],
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    let items = &r["items"];
    assert_eq!(items[0]["source"]["kind"], "scaled");
    assert_eq!(items[0]["source"]["multiplier"], 0.8);
    assert_eq!(items[0]["price"]["input"], 2.4);
    // 倍率同样作用于缓存单价
    assert_eq!(items[0]["price"]["cache_read"], 0.24);
    assert_eq!(items[0]["max_input_tokens"], 200_000);
    assert_eq!(items[1]["source"]["kind"], "override");
    assert_eq!(items[1]["price"]["input"], 3.0);
    // 无法计价就是无法计价，不是 0
    assert!(items[2].get("price").is_none(), "{r}");
    assert!(items[2].get("source").is_none(), "{r}");
}

#[tokio::test]
async fn a_draft_is_priced_without_being_saved() {
    let b = bed(BASE);
    let (st, r) = call(
        &b.app,
        "POST",
        "/pricing/query",
        serde_json::json!({
            "sheet": { "kind": "draft", "sheet": { "name": "草稿", "multiplier": 0.5 } },
            "models": ["claude-sonnet-4-5"],
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["items"][0]["price"]["input"], 1.5);
    assert_eq!(b.file(), BASE);

    let (st, _) = call(
        &b.app,
        "POST",
        "/pricing/query",
        serde_json::json!({ "sheet": { "kind": "named", "name": "没有这张" }, "models": ["m"] }),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_search_lists_the_names_clients_send_before_platform_keys() {
    let b = bed(BASE);
    let (st, r) = call(
        &b.app,
        "POST",
        "/pricing/query",
        serde_json::json!({ "sheet": { "kind": "default" }, "search": "Claude-Sonnet-4-5", "limit": 3 }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    let items = r["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert!(r["matched"].as_u64().unwrap() > 3, "{r}");
    let first = items[0]["model"].as_str().unwrap();
    assert!(!first.contains('/') && !first.contains(':'), "{first}");
}

// ─────────────────────────────────────────────────────────── 默认价目表

#[tokio::test]
async fn turning_auto_update_off_writes_one_line_and_turning_it_back_on_removes_it() {
    let b = bed(BASE);
    let (st, body) = call(&b.app, "GET", "/pricing", serde_json::json!(null)).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["auto_update"], true);
    assert_eq!(body["source"], "builtin");

    let (st, body) = call(
        &b.app,
        "PUT",
        "/pricing/auto_update",
        serde_json::json!({ "on": false }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(b.file(), format!("{BASE}pricing:\n  auto_update: false\n"));
    let (_, body) = call(&b.app, "GET", "/pricing", serde_json::json!(null)).await;
    assert_eq!(body["auto_update"], false);

    call(
        &b.app,
        "PUT",
        "/pricing/auto_update",
        serde_json::json!({ "on": true }),
    )
    .await;
    assert_eq!(b.file(), BASE);
}

/// 一个假的价格数据源。返回它的地址和被请求了几次。
async fn price_source(status: u16, body: &'static str) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let app = axum::Router::new().fallback(move || {
        let h = h.clone();
        async move {
            h.fetch_add(1, Ordering::SeqCst);
            (StatusCode::from_u16(status).unwrap(), body)
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app).await.unwrap();
    });
    (format!("http://{addr}/model_prices.json"), hits)
}

const DATASET: &str = r#"{
  "claude-sonnet-4-5": { "input_cost_per_token": 4e-6, "output_cost_per_token": 2e-5, "max_input_tokens": 200000 },
  "新出的模型": { "input_cost_per_token": 1e-6, "output_cost_per_token": 2e-6 }
}"#;

#[tokio::test]
async fn a_refresh_saves_the_new_table_and_prices_with_it_straight_away() {
    let (url, hits) = price_source(200, DATASET).await;
    let b = bed_with(BASE, Updater::new(url, Schedule::default()));
    let (st, r) = call(&b.app, "POST", "/pricing/refresh", serde_json::json!(null)).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert!(r["changed"].as_u64().unwrap() > 0, "{r}");
    let status = &r["status"];
    assert_eq!(status["source"], "fetched");
    assert_eq!(status["models"], 2);
    assert_eq!(
        status["date"],
        chrono::Local::now().format("%Y-%m-%d").to_string()
    );
    assert!(status["checked_at_ms"].as_u64().is_some(), "{r}");
    assert!(status.get("error").is_none(), "{r}");

    // 存下来了：重启之后用的还是这一份
    assert_eq!(std::fs::read_to_string(b.data_file()).unwrap(), DATASET);
    let reloaded = tw_control::pricing::load_table(&b.dir.path().join("config.yaml"));
    assert_eq!(reloaded.len(), 2);

    // 不用重启就按新表计价
    let (_, q) = call(
        &b.app,
        "POST",
        "/pricing/query",
        serde_json::json!({ "sheet": { "kind": "default" }, "models": ["新出的模型", "claude-sonnet-4-5"] }),
    )
    .await;
    assert_eq!(q["items"][0]["price"]["input"], 1.0);
    assert_eq!(q["items"][1]["price"]["input"], 4.0);
    assert_eq!(
        b.state.gateway.pricing.load().table().date,
        status["date"].as_str().unwrap()
    );
}

#[tokio::test]
async fn a_failed_refresh_keeps_the_table_and_reports_why() {
    for (code, body, says) in [
        (500, "internal error", "HTTP 500"),
        (
            200,
            r#"{"error": "rate limited"}"#,
            "may not be a price data set",
        ),
    ] {
        let (url, _) = price_source(code, body).await;
        let b = bed_with(BASE, Updater::new(url, Schedule::default()));
        let (st, r) = call(&b.app, "POST", "/pricing/refresh", serde_json::json!(null)).await;
        assert_eq!(st, StatusCode::BAD_GATEWAY, "{r}");
        assert!(r["text"].as_str().unwrap().contains(says), "{r}");
        let (_, status) = call(&b.app, "GET", "/pricing", serde_json::json!(null)).await;
        assert_eq!(status["source"], "builtin");
        assert!(status["error"].as_str().unwrap().contains(says), "{status}");
        assert!(!b.data_file().exists());
    }
}

#[tokio::test]
async fn the_background_refresh_waits_while_auto_update_is_off_and_runs_once_it_is_on() {
    let (url, hits) = price_source(200, DATASET).await;
    let quick = Schedule {
        first_check: Duration::from_millis(20),
        tick: Duration::from_millis(20),
        every: Duration::from_secs(24 * 3600),
        retry: Duration::from_secs(3600),
    };
    let b = bed_with(
        &format!("{BASE}pricing:\n  auto_update: false\n"),
        Updater::new(url, quick),
    );
    tw_control::pricing::spawn(b.state.clone());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 0, "关着的时候不该联网");

    let (st, body) = call(
        &b.app,
        "PUT",
        "/pricing/auto_update",
        serde_json::json!({ "on": true }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !b.data_file().exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(b.data_file().exists(), "打开之后没有刷新");
    // 刚刷新过：之后几轮都不再联网
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}
