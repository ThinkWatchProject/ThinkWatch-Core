//! `/summary/routes`：各条路由走了多少请求、各条规则命中了多少。
//!
//! 起真网关、真上游，事件经总线落库，再从控制面问 —— 这条链上任何一环没把
//! 路由记下来（开始事件、路由事件、落库、聚合），数出来的就不对。**被规则拒绝的
//! 请求也要在里面**：它们以前连开始事件都没有，流量里看不见，也数不到。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

/// 什么都答 200 的上游
async fn upstream() -> SocketAddr {
    let app = axum::Router::new().fallback(axum::routing::any(|| async {
        axum::response::Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"type":"message","content":[],"usage":{"input_tokens":3,"output_tokens":1}}"#,
            ))
            .unwrap()
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

fn yaml(up: SocketAddr) -> String {
    format!(
        r#"version: 1
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
clients:
  - name: 我
    key: tw-k
    route: 工作
providers:
  - name: up
    base_url: http://{up}
    key: sk-x
    protocol: anthropic
routes:
  - name: 工作
    rules:
      - name: 不许用 opus
        when: {{ model: claude-opus-* }}
        deny: 这个项目不用 opus
      - name: 关掉思考
        when: {{ model: claude-sonnet-* }}
        set: {{ thinking: false }}
      - name: 从没命中过
        when: {{ model: gpt-* }}
        to: up
      - name: 兜底
        to: up
"#
    )
}

async fn ask(gw: SocketAddr, model: &str) -> u16 {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(format!(
            r#"{{"model":"{model}","max_tokens":16,"messages":[{{"role":"user","content":"hi"}}]}}"#
        ))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn every_route_and_rule_counts_what_went_through_it_including_denials() {
    let d = tempfile::tempdir().unwrap();
    let text = yaml(upstream().await);
    let p = d.path().join("config.yaml");
    std::fs::write(&p, &text).unwrap();
    let cfg: tw_config::Config = serde_yaml_ng::from_str(&text).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    // 和 twcore 一样：存储层订阅总线，事件落库
    let (_bodies, rx) = tokio::sync::mpsc::channel(8);
    let store = tw_store::task::spawn(
        tw_store::Recorder::new(
            tw_store::Db::open(&d.path().join("data.db")).unwrap(),
            tw_store::Blobs::new(d.path().join("blobs")),
            tw_pricing::shared(tw_pricing::PriceBook::builtin().unwrap()),
        ),
        gw.bus.subscribe(),
        rx,
    );
    let state = ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), gw.bus.clone())),
        gateway: gw.clone(),
        store: Some(store.clone()),
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    };
    let app = tw_control::router(state);
    let addr = tw_gateway::serve(gw, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();

    let before = now_ms();
    assert_eq!(ask(addr, "claude-sonnet-4-5").await, 200);
    assert_eq!(ask(addr, "claude-haiku-4-5").await, 200);
    assert_eq!(ask(addr, "claude-opus-4-1").await, 403, "该被规则拒绝");

    // 落库在另一个任务上：等三行都写下
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while store.lock().await.db().count().unwrap() < 3 {
        assert!(std::time::Instant::now() < deadline, "三个请求没有都落库");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 窗口的右端给到将来：缺省是「此刻」，而最后那个请求可能就开始在这一毫秒
    let resp = app
        .oneshot(
            Request::get("/summary/routes?from_ms=0&to_ms=99999999999999")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let stats: tw_api::RouteStats = serde_json::from_slice(&body).unwrap();
    // 库是这个测试里新建的：记录从第一个请求开始，不是问的起点（0）
    let covered = stats.covered_since_ms.expect("库里有请求，记录有起点");
    assert!(
        (before..=now_ms()).contains(&covered),
        "记录应当从第一个请求开始：{covered}，测试开始于 {before}"
    );
    let got = &stats.routes;
    assert_eq!(got.len(), 1, "{got:?}");
    let work = &got[0];
    assert_eq!(work.route, "工作", "按密钥指定的路由记，不是默认路由");
    assert_eq!((work.requests, work.failed), (3, 1), "{work:?}");
    let rule = |name: &str| {
        work.rules
            .iter()
            .find(|h| h.rule == name)
            .map(|h| (h.decided, h.requests, h.failed))
    };
    assert_eq!(rule("兜底"), Some((2, 2, 0)));
    assert_eq!(
        rule("不许用 opus"),
        Some((1, 1, 1)),
        "被拒绝的请求也是命中：它失败了"
    );
    assert_eq!(
        rule("关掉思考"),
        Some((0, 1, 0)),
        "只附加改写的规则也数得到，只是没决定去向"
    );
    assert_eq!(rule("从没命中过"), None, "没命中过的规则不在里面");
}

/// 还没有一条记录的库：线上的起点是 null，界面不会把空表读成「一周都没命中」。
#[tokio::test]
async fn a_store_with_no_history_yet_says_nothing_is_covered() {
    let d = tempfile::tempdir().unwrap();
    let text = yaml(SocketAddr::from(([127, 0, 0, 1], 9)));
    let p = d.path().join("config.yaml");
    std::fs::write(&p, &text).unwrap();
    let cfg: tw_config::Config = serde_yaml_ng::from_str(&text).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let (_bodies, rx) = tokio::sync::mpsc::channel(8);
    let store = tw_store::task::spawn(
        tw_store::Recorder::new(
            tw_store::Db::open(&d.path().join("data.db")).unwrap(),
            tw_store::Blobs::new(d.path().join("blobs")),
            tw_pricing::shared(tw_pricing::PriceBook::builtin().unwrap()),
        ),
        gw.bus.subscribe(),
        rx,
    );
    let state = ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), gw.bus.clone())),
        gateway: gw,
        store: Some(store),
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    };
    let resp = tw_control::router(state)
        .oneshot(
            Request::get("/summary/routes?from_ms=0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"covered_since_ms": null, "routes": []})
    );
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// 没有存储层（记录没起来）时说清楚，不是一张空表 —— 空表读起来是「没有请求」。
#[tokio::test]
async fn without_a_store_it_says_so() {
    let d = tempfile::tempdir().unwrap();
    let text = yaml(SocketAddr::from(([127, 0, 0, 1], 9)));
    let p = d.path().join("config.yaml");
    std::fs::write(&p, &text).unwrap();
    let cfg: tw_config::Config = serde_yaml_ng::from_str(&text).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let state = ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), gw.bus.clone())),
        gateway: gw,
        store: None,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    };
    let resp = tw_control::router(state)
        .oneshot(Request::get("/summary/routes").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_ne!(resp.status(), StatusCode::OK);
}
