//! 上游的用量上限在控制面上：新建、保存上游时写进配置，概览里每一条用了多少；改名带着
//! 用量走；存储层每记下一行就结算（算数的每一跳一个请求，用量记在接下它的那一家）；重启
//! 之后从请求记录里加回来。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_api::Event;
use tw_control::{ConfigManager, ControlState};

const CONFIG: &str = "version: 1
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
clients:
  - name: default
    key: tw-aaaa
providers:
  - name: 中转
    base_url: http://127.0.0.1:9
    key: sk-x
    protocol: anthropic
    models: [claude-sonnet-4-5]
    limits:
      - { per: day, requests: 10 }
      - { per: hour, every: 5, cost: 1 }
  - name: 备用
    base_url: http://127.0.0.1:8
    key: sk-y
    protocol: anthropic
    models: [claude-sonnet-4-5]
";

/// 中午，东八区。天的上限不会碰巧在测试跑到一半时过零点
const NOON: &str = "2026-10-05T12:00:00+08:00";

fn ms(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .unwrap()
        .timestamp_millis()
}

struct Bed {
    _dir: tempfile::TempDir,
    app: axum::Router,
    rec: Arc<tokio::sync::Mutex<tw_store::Recorder>>,
    path: std::path::PathBuf,
}

/// `db`：上一次运行留下的请求记录（重启）；没有就是一个空库
fn bed(db: Option<tw_store::Db>) -> Bed {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, CONFIG).unwrap();
    let cfg = tw_config::try_parse(CONFIG).unwrap();
    let mut gw = tw_gateway::AppState::new(cfg).unwrap();
    gw.set_key_limits_clock(Arc::new(tw_gateway::key_limits::TestClock::new(
        ms(NOON),
        8 * 3600,
    )));
    let db = db.unwrap_or_else(|| tw_store::Db::in_memory().unwrap());
    // 重启：第一个请求之前把这一期加回来，和 twcore 起来时一样
    tw_control::key_limits::rebuild(&gw, &db);
    let rec = tw_store::Recorder::new(
        db,
        tw_store::Blobs::new(d.path().join("blobs")),
        gw.pricing.clone(),
    )
    .settling_to(tw_control::key_limits::settle_hook(&gw));
    let rec = Arc::new(tokio::sync::Mutex::new(rec));
    let state = ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: Arc::new(ConfigManager::new(p.clone(), gw.clone(), gw.bus.clone())),
        gateway: gw.clone(),
        store: Some(rec.clone()),
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    };
    Bed {
        app: tw_control::router(state),
        _dir: d,
        rec,
        path: p,
    }
}

fn attempt(provider: &str, status: u16) -> tw_api::AttemptView {
    tw_api::AttemptView {
        provider: provider.into(),
        model: None,
        outcome: if status == 200 {
            tw_api::AttemptOutcome::Served
        } else {
            tw_api::AttemptOutcome::Status
        },
        status: Some(status),
        error: None,
        ms: 5,
        usage: None,
        queued_ms: None,
        skipped: None,
        pause_ms: None,
        proxy: None,
    }
}

/// 一个跑完的请求的几条事件：开始于 `at`，尝试链是 `attempts`，按 Sonnet 4.5 的价（$3/M
/// 输入）
fn request(id: u64, at: i64, attempts: Vec<tw_api::AttemptView>, input: u64) -> Vec<Event> {
    vec![
        Event::RequestStarted {
            id,
            client: "default".into(),
            client_hint: None,
            session: None,
            peer: None,
            key_masked: None,
            route: "default".into(),
            rule: "r".into(),
            group: None,
            rewritten_by: vec![],
            provider: attempts[0].provider.clone(),
            billing: tw_api::Billing::PerToken,
            model: "claude-sonnet-4-5".into(),
            method: "POST".into(),
            path: "/v1/messages".into(),
            input_estimate: None,
            session_log_bytes: None,
            at_ms: at as u64,
        },
        Event::RequestRouted {
            id,
            route: "default".into(),
            rule: "r".into(),
            group: None,
            rewritten_by: vec![],
            denied_by: None,
            affinity: None,
            attempts,
            billing: tw_api::Billing::PerToken,
            egress: None,
        },
        Event::RequestFinished {
            id,
            model: String::new(),
            status: 200,
            sent_bytes: 0,
            received_bytes: 1,
            duration_ms: 1,
            usage: Some(tw_api::UsageView {
                input,
                output: 0,
                cache_read: 0,
                cache_write: 0,
                cache_1h: false,
            }),
            tokens_per_sec: None,
            answered_model: None,
        },
    ]
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
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    let text = String::from_utf8_lossy(&b).to_string();
    (
        st,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
    )
}

/// 概览里的那一家
async fn upstream(b: &Bed, name: &str) -> serde_json::Value {
    let (st, v) = call(&b.app, "GET", "/overview", serde_json::Value::Null).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("没有上游「{name}」：{v}"))
}

fn config(b: &Bed) -> tw_config::Config {
    tw_config::try_parse(&std::fs::read_to_string(&b.path).unwrap()).unwrap()
}

/// 存储层每记下一行就结算：发到「中转」的每一跳算一个请求（回 429 的不算），费用算在接下
/// 请求的那一家。概览里每一条带着设置和用量。
#[tokio::test]
async fn the_overview_shows_what_each_upstream_limit_has_used() {
    let b = bed(None);
    // 「中转」回了 429，换「备用」接下：中转不算
    let events = [
        request(
            1,
            ms(NOON),
            vec![attempt("中转", 429), attempt("备用", 200)],
            10,
        ),
        // 「中转」接下：一个请求，十万输入 $0.30
        request(2, ms(NOON), vec![attempt("中转", 200)], 100_000),
        // 「中转」回 500，换「备用」接下：中转算一个请求，费用不是它的
        request(
            3,
            ms(NOON),
            vec![attempt("中转", 500), attempt("备用", 200)],
            100_000,
        ),
    ];
    for e in events.iter().flatten() {
        b.rec.lock().await.on_event(e);
    }
    let u = upstream(&b, "中转").await;
    let limits = &u["limits"];
    assert_eq!(limits[0]["per"], "day");
    assert_eq!(limits[0]["every"], 1);
    assert_eq!(limits[0]["measure"], "requests");
    assert_eq!(limits[0]["max"], 10);
    assert_eq!(limits[0]["used"], 2);
    assert_eq!(limits[0]["resets_at_ms"], ms("2026-10-06T00:00:00+08:00"));
    assert_eq!(limits[1]["per"], "hour");
    assert_eq!(limits[1]["every"], 5);
    assert_eq!(limits[1]["measure"], "cost");
    assert_eq!(limits[1]["used"], 300_000, "十万输入 × $3/M");
    assert!(
        limits[1].get("resets_at_ms").is_none(),
        "滚动的没有重置时刻"
    );
    assert_eq!(limits[1]["reached"], false);
    // 没设上限的，列表是空的
    assert_eq!(upstream(&b, "备用").await["limits"], serde_json::json!([]));
}

/// 新建、保存上游时写进配置：every 是 1 的不写，费用在界面上是微分、配置里写美元；写错了
/// 是配置校验的那句话。改名之后用过的跟着走。
#[tokio::test]
async fn saving_an_upstream_writes_its_limits_and_a_rename_keeps_what_it_used() {
    let b = bed(None);
    let (st, v) = call(
        &b.app,
        "POST",
        "/providers",
        serde_json::json!({ "provider": {
            "name": "新的",
            "base_url": "https://relay.example",
            "key": "sk-new",
            "protocol": "anthropic",
            "limits": [
                { "per": "minute", "every": 30, "measure": "requests", "max": 60 },
                { "per": "week", "every": 1, "measure": "cost", "max": 2_500_000 },
            ],
        } }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let cfg = config(&b);
    let p = cfg.providers.iter().find(|p| p.name == "新的").unwrap();
    assert_eq!(
        (p.limits[0].per, p.limits[0].every, p.limits[0].requests),
        (tw_config::LimitPer::Minute, Some(30), Some(60))
    );
    assert_eq!((p.limits[1].every, p.limits[1].cost), (None, Some(2.5)));
    let text = std::fs::read_to_string(&b.path).unwrap();
    assert!(text.contains("every: 30"), "{text}");
    let u = upstream(&b, "新的").await;
    assert_eq!(
        (&u["limits"][0]["every"], &u["limits"][1]["max"]),
        (&serde_json::json!(30), &serde_json::json!(2_500_000))
    );

    // 「中转」用掉一个请求，然后改名、换上限：用量跟着走
    for e in request(1, ms(NOON), vec![attempt("中转", 200)], 10) {
        b.rec.lock().await.on_event(&e);
    }
    let (st, v) = call(
        &b.app,
        "PUT",
        &tw_api::ep::UpdateProvider::path("中转"),
        serde_json::json!({ "provider": {
            "name": "中转二",
            "base_url": "http://127.0.0.1:9",
            "key": "sk-x",
            "protocol": "anthropic",
            "models": ["claude-sonnet-4-5"],
            "limits": [ { "per": "day", "every": 1, "measure": "requests", "max": 1 } ],
        } }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let u = upstream(&b, "中转二").await;
    assert_eq!(u["limits"][0]["used"], 1);
    assert_eq!(u["limits"][0]["reached"], true);
    assert_eq!(config(&b).providers[0].limits.len(), 1);

    // 写错了：校验的那句话，码是上游自己的
    for (limit, code) in [
        (
            serde_json::json!({ "per": "day", "every": 1, "measure": "requests", "max": 0 }),
            "config.upstream_limit_not_positive",
        ),
        (
            serde_json::json!({ "per": "day", "every": 3, "measure": "requests", "max": 5 }),
            "config.upstream_limit_every_calendar",
        ),
        (
            serde_json::json!({ "per": "hour", "every": 25, "measure": "requests", "max": 5 }),
            "config.upstream_limit_every",
        ),
        (
            serde_json::json!({ "per": "day", "every": 1, "measure": "cost", "max": 1_000_000,
                                "cache_reads": true }),
            "config.upstream_limit_cache_reads",
        ),
    ] {
        let (st, v) = call(
            &b.app,
            "PUT",
            &tw_api::ep::UpdateProvider::path("中转二"),
            serde_json::json!({ "provider": {
                "name": "中转二",
                "base_url": "http://127.0.0.1:9",
                "key": "sk-x",
                "protocol": "anthropic",
                "limits": [limit],
            } }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
        assert!(v.to_string().contains(code), "{code}: {v}");
    }
}

/// 重启：这一天、这一周、这个月的数从请求记录里加回来，按尝试链数请求、按接下它的那一家记
/// 用量。前一天的不算进今天。
#[tokio::test]
async fn a_restart_adds_this_periods_upstream_usage_back_from_the_records() {
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("data.db");
    let mut earlier = tw_store::Recorder::new(
        tw_store::Db::open(&file).unwrap(),
        tw_store::Blobs::new(d.path().join("blobs")),
        tw_pricing::shared(tw_pricing::PriceBook::builtin().unwrap()),
    );
    let events = [
        request(
            1,
            ms("2026-10-05T09:00:00+08:00"),
            vec![attempt("中转", 200)],
            100_000,
        ),
        request(
            2,
            ms("2026-10-05T10:00:00+08:00"),
            vec![
                attempt("中转", 500),
                attempt("中转", 429),
                attempt("备用", 200),
            ],
            100_000,
        ),
        // 前一天的：今天不算
        request(
            3,
            ms("2026-10-04T23:00:00+08:00"),
            vec![attempt("中转", 200)],
            100_000,
        ),
    ];
    for e in events.iter().flatten() {
        earlier.on_event(e);
    }
    drop(earlier);
    let b = bed(Some(tw_store::Db::open(&file).unwrap()));
    let u = upstream(&b, "中转").await;
    assert_eq!(
        u["limits"][0]["used"], 2,
        "今天两跳算数：接下的一跳、回 500 的一跳"
    );
    assert_eq!(u["limits"][1]["used"], 0, "滚动窗口从空的开始");
}
