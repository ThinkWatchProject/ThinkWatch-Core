//! 密钥的用量上限在控制面上：`GET /keys` 里每一条上限用了多少、什么时候重置、哪些模型
//! 没有价格；保存密钥时写进配置；改名带着用量走；重启之后从请求记录里加回来；存储层
//! 每记下一行就结算。

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
  - name: k
    key: tw-kkkk
    limits:
      - { per: day, cost: 5 }
      - { per: minute, requests: 30 }
  - name: plain
    key: tw-pppp
    limits:
      - { per: week, requests: 100 }
  - name: small
    key: tw-ssss
    limits:
      - { per: day, cost: 0.5 }
providers:
  - name: 中转
    base_url: http://127.0.0.1:9
    key: sk-x
    protocol: anthropic
    models: [claude-sonnet-4-5, relay-own-model]
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
    gw: tw_gateway::AppState,
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
        gw,
        rec,
        path: p,
    }
}

/// 一个跑完的请求的几条事件：密钥 `key`，开始于 `at`，按 Sonnet 4.5 的价（$3/M 输入；
/// 超过 20 万的整个请求换长上下文的价，测试里不碰它）
fn request(id: u64, key: &str, at: i64, input: u64) -> Vec<Event> {
    vec![
        Event::RequestStarted {
            id,
            client: key.into(),
            client_hint: None,
            session: None,
            peer: None,
            key_masked: None,
            route: "default".into(),
            rule: "r".into(),
            group: None,
            rewritten_by: vec![],
            provider: "中转".into(),
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
            attempts: vec![tw_api::AttemptView {
                provider: "中转".into(),
                model: None,
                outcome: tw_api::AttemptOutcome::Served,
                status: Some(200),
                error: None,
                ms: 5,
                usage: None,
                queued_ms: None,
                skipped: None,
            }],
            billing: tw_api::Billing::PerToken,
        },
        Event::RequestFinished {
            id,
            model: String::new(),
            status: 200,
            bytes: 1,
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

async fn keys(b: &Bed) -> serde_json::Value {
    let (st, v) = call(&b.app, "GET", "/keys", serde_json::Value::Null).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v
}

fn key<'a>(list: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|k| k["name"] == name)
        .unwrap_or_else(|| panic!("没有密钥「{name}」：{list}"))
}

/// 存储层每记下一行就结算：`GET /keys` 里看得到这一天花了多少、什么时候重置；设了费用
/// 上限的密钥带着它用得到、却没有价格的模型。
#[tokio::test]
async fn the_key_list_shows_what_each_limit_has_used() {
    let b = bed(None);
    for e in request(1, "k", ms(NOON), 100_000) {
        b.rec.lock().await.on_event(&e);
    }
    // 没有价格的模型、不计费的上游：记下的费用是空的、是 0，费用上限都算 0
    let mut unpriced = request(2, "k", ms(NOON), 100_000);
    if let Event::RequestStarted { model, .. } = &mut unpriced[0] {
        *model = "relay-own-model".into();
    }
    let mut free = request(3, "k", ms(NOON), 100_000);
    if let Event::RequestRouted { billing, .. } = &mut free[1] {
        *billing = tw_api::Billing::Free;
    }
    for e in unpriced.iter().chain(&free) {
        b.rec.lock().await.on_event(e);
    }
    let rows = b.rec.lock().await.db().key_usage_since(0).unwrap();
    assert_eq!(rows.iter().map(|r| r.requests).sum::<i64>(), 3);
    let list = keys(&b).await;
    let k = key(&list, "k");
    assert_eq!(k["limits"][0]["per"], "day");
    assert_eq!(k["limits"][0]["measure"], "cost");
    assert_eq!(k["limits"][0]["max"], 5_000_000);
    assert_eq!(k["limits"][0]["used"], 300_000, "十万输入 × $3/M");
    assert_eq!(k["limits"][0]["reached"], false);
    assert_eq!(
        k["limits"][0]["resets_at_ms"],
        ms("2026-10-06T00:00:00+08:00")
    );
    assert_eq!(k["limits"][1]["per"], "minute");
    assert!(
        k["limits"][1].get("resets_at_ms").is_none(),
        "滚动的没有重置时刻"
    );
    assert_eq!(k["unpriced_models"], serde_json::json!(["relay-own-model"]));
    // 没有费用上限的不提醒；没有上限的，列表是空的
    assert!(key(&list, "plain").get("unpriced_models").is_none());
    assert_eq!(key(&list, "default")["limits"], serde_json::json!([]));
}

/// 重启：这一天、这一周、这个月的数从请求记录里加回来。别的密钥的、前一天的不算进来。
#[tokio::test]
async fn a_restart_adds_this_periods_usage_back_from_the_records() {
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("data.db");
    let mut earlier = tw_store::Recorder::new(
        tw_store::Db::open(&file).unwrap(),
        tw_store::Blobs::new(d.path().join("blobs")),
        tw_pricing::shared(tw_pricing::PriceBook::builtin().unwrap()),
    );
    let events = [
        request(1, "k", ms("2026-10-05T09:00:00+08:00"), 100_000),
        request(2, "k", ms("2026-10-04T23:00:00+08:00"), 100_000),
        request(3, "plain", ms("2026-10-05T08:00:00+08:00"), 10),
        // 上周日的：这一周不算
        request(4, "plain", ms("2026-10-04T08:00:00+08:00"), 10),
    ];
    for e in events.iter().flatten() {
        earlier.on_event(e);
    }
    drop(earlier);
    // 上一次运行的库原样交给这一次
    let b = bed(Some(tw_store::Db::open(&file).unwrap()));
    let list = keys(&b).await;
    assert_eq!(
        key(&list, "k")["limits"][0]["used"],
        300_000,
        "前一天的不算"
    );
    assert_eq!(key(&list, "plain")["limits"][0]["used"], 1);
}

/// 保存密钥时写进配置，费用在界面上是微分、配置里写美元；写错了是配置校验的那句话。
/// 改名之后用过的跟着走。
#[tokio::test]
async fn saving_a_key_writes_its_limits_and_a_rename_keeps_what_it_used() {
    let b = bed(None);
    for e in request(1, "k", ms(NOON), 100_000) {
        b.rec.lock().await.on_event(&e);
    }
    let (st, v) = call(
        &b.app,
        "PUT",
        "/keys/k",
        serde_json::json!({ "key": { "name": "k2", "limits": [
            { "per": "day", "measure": "cost", "max": 250_000 },
            { "per": "hour", "measure": "tokens", "max": 1000, "cache_reads": true },
        ] } }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let cfg = tw_config::try_parse(&std::fs::read_to_string(&b.path).unwrap()).unwrap();
    let k2 = cfg.clients.iter().find(|c| c.name == "k2").unwrap();
    assert_eq!(k2.limits[0].cost, Some(0.25));
    assert_eq!(k2.limits[1].tokens, Some(1000));
    assert!(k2.limits[1].cache_reads);
    // 改了名：今天花掉的还在，而且已经超过了新的上限
    let list = keys(&b).await;
    let k2 = key(&list, "k2");
    assert_eq!(k2["limits"][0]["used"], 300_000);
    assert_eq!(k2["limits"][0]["reached"], true);

    let (st, v) = call(
        &b.app,
        "PUT",
        "/keys/k2",
        serde_json::json!({ "key": { "name": "k2", "limits": [
            { "per": "day", "measure": "requests", "max": 0 },
        ] } }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert!(
        v.to_string().contains("config.key_limit_not_positive"),
        "{v}"
    );
    let (st, v) = call(
        &b.app,
        "PUT",
        "/keys/k2",
        serde_json::json!({ "key": { "name": "k2", "limits": [
            { "per": "day", "measure": "cost", "max": 1_000_000, "cache_reads": true },
        ] } }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert!(
        v.to_string().contains("config.key_limit_cache_reads"),
        "{v}"
    );
}

/// 一把密钥这一期到了八成、到了顶：总线上各报一次，界面拿它发系统通知。
#[tokio::test]
async fn reaching_a_limit_is_told_on_the_bus() {
    let b = bed(None);
    let mut rx = b.gw.bus.subscribe();
    // 一天 $0.5：$0.45、$0.51，先过八成，再过顶
    for e in request(1, "small", ms(NOON), 150_000) {
        b.rec.lock().await.on_event(&e);
    }
    for e in request(2, "small", ms(NOON), 20_000) {
        b.rec.lock().await.on_event(&e);
    }
    let mut told = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if let Event::KeyLimitAlert {
            key,
            per,
            measure,
            used,
            reached,
            ..
        } = e
        {
            told.push((key, per, measure, used, reached));
        }
    }
    assert_eq!(
        told,
        [
            (
                "small".to_string(),
                tw_api::LimitPer::Day,
                tw_api::LimitMeasure::Cost,
                450_000,
                false
            ),
            (
                "small".to_string(),
                tw_api::LimitPer::Day,
                tw_api::LimitMeasure::Cost,
                510_000,
                true
            ),
        ]
    );
}

/// 一个没发到上游的请求的几条事件：密钥 `key`，开始于 `at`。`attempts` 是尝试链（`None` 是
/// 一直没有路由事件 —— 内容过滤在发往哪一家之前就拒了），最后是网关自己的一句拒绝
fn refused(
    id: u64,
    key: &str,
    at: i64,
    attempts: Option<Vec<tw_api::AttemptView>>,
    code: &str,
    source: tw_api::FailureSource,
) -> Vec<Event> {
    let mut evs = request(id, key, at, 0);
    evs.pop();
    match attempts {
        Some(a) => {
            if let Event::RequestRouted { attempts, .. } = &mut evs[1] {
                *attempts = a;
            }
        }
        None => {
            evs.remove(1);
        }
    }
    evs.push(Event::RequestFailed {
        id,
        model: String::new(),
        source,
        message: tw_api::Msg {
            code: code.into(),
            args: Default::default(),
            text: "refused".into(),
        },
        bytes: None,
        duration_ms: Some(1),
        usage: None,
        answered_model: None,
    });
    evs
}

fn hop(outcome: tw_api::AttemptOutcome, code: &str, busy: bool) -> tw_api::AttemptView {
    tw_api::AttemptView {
        provider: "中转".into(),
        model: None,
        outcome,
        status: None,
        error: Some(tw_api::Msg {
            code: code.into(),
            args: Default::default(),
            text: "x".into(),
        }),
        ms: 1,
        usage: None,
        queued_ms: None,
        skipped: busy.then_some(tw_api::ServeSkip::Busy),
    }
}

/// 没发到上游的请求**不算进用量**：上游都满着回的 429（尝试链上只有跳过的）、被内容过滤拒掉
/// 的（一跳都没有）、在那一跳上被阶段二的规则拒了的、凭据取不到的。客户端照着 `Retry-After`
/// 重试，不该把自己的上限用光。发出去之后才失败的（超时）照样算；重启之后从记录里加回来时
/// 是同一个判断
#[tokio::test]
async fn a_request_that_never_reached_an_upstream_does_not_count() {
    use tw_api::AttemptOutcome::Error;
    use tw_api::FailureSource::{Config, Denied, RateLimited, Upstream};
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("data.db");
    let b = bed(Some(tw_store::Db::open(&file).unwrap()));
    let at = ms(NOON);
    let events = [
        refused(
            1,
            "plain",
            at,
            Some(vec![hop(Error, "gw.busy_upstream", true)]),
            "gw.busy_all",
            RateLimited,
        ),
        refused(2, "plain", at, None, "gw.content.denied", Denied),
        refused(
            3,
            "plain",
            at,
            Some(vec![hop(Error, "gw.route.denied", false)]),
            "gw.route.denied",
            Denied,
        ),
        refused(
            4,
            "plain",
            at,
            Some(vec![hop(Error, "gw.upstream.credential_failed", false)]),
            "gw.upstream.credential_failed",
            Config,
        ),
        // 发出去了、上游没在时限内回话：它可能已经在算了
        refused(
            5,
            "plain",
            at,
            Some(vec![hop(Error, "gw.upstream.timeout", false)]),
            "gw.upstream.timeout",
            Upstream,
        ),
        request(6, "plain", at, 10),
    ];
    for e in events.iter().flatten() {
        b.rec.lock().await.on_event(e);
    }
    let list = keys(&b).await;
    assert_eq!(
        key(&list, "plain")["limits"][0]["used"],
        2,
        "只有超时的和答上了的算"
    );
    drop(b);
    // 重启：从记录里加回来，同一个判断
    let b = bed(Some(tw_store::Db::open(&file).unwrap()));
    let list = keys(&b).await;
    assert_eq!(key(&list, "plain")["limits"][0]["used"], 2);
}
