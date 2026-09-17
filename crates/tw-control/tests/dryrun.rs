//! 路由试算。
//!
//! 这些测试盯的是**「为什么没走我以为的那条」**能不能答出来 —— 只答
//! 「走了哪条」是不够的那半个答案。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

const CFG: &str = r#"version: 1
clients:
  - name: 我
    key: tw-k
providers:
  - name: 官方
    base_url: https://api.anthropic.com
    key: sk-a
  - name: 中转
    base_url: https://relay.example.com
    key: sk-b
groups:
  - name: 都试试
    type: load-balance
    providers: [官方, 中转]
routes:
  - name: 默认
    rules:
      - name: 带缓存的必须走官方
        when: { cache: true }
        to: 官方
      - name: 超长上下文降级
        when: { input_tokens: ">200k" }
        set: { model: claude-haiku-4-5 }
      - name: 图片一律拒绝
        when: { image: true }
        deny: 这个上游不收图片
      - name: 其余都试试
        to: 都试试
"#;

fn app() -> (tempfile::TempDir, axum::Router) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, CFG).unwrap();
    let cfg: tw_config::Config = serde_yaml_ng::from_str(CFG).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let state = ControlState {
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        gateway_addr: None,
        store: None,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        home: d.path().join("home"),
    };
    (d, tw_control::router(state))
}

async fn run(app: &axum::Router, body: &str) -> tw_api::DryRunResult {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dryrun")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&b).unwrap()
}

#[tokio::test]
async fn a_plain_request_falls_through_to_the_catch_all() {
    let (_d, app) = app();
    let r = run(&app, r#"{"model":"claude-sonnet-4-5"}"#).await;
    assert_eq!(r.outcome, "route");
    assert_eq!(r.rule.as_deref(), Some("其余都试试"));
    assert_eq!(r.via_group.as_deref(), Some("都试试"));
    assert_eq!(r.candidates.len(), 2);
    // **要直说 —— 它决定账单。**负载均衡会让 prompt cache 不稳定
    assert!(r.hurts_cache, "load_balance 会打散缓存，试算里就该说");
}

#[tokio::test]
async fn a_cached_request_is_pinned_to_the_official_upstream() {
    let (_d, app) = app();
    let r = run(&app, r#"{"model":"claude-sonnet-4-5","cache":true}"#).await;
    assert_eq!(r.rule.as_deref(), Some("带缓存的必须走官方"));
    assert_eq!(r.candidates, vec!["官方".to_string()]);
    assert!(!r.hurts_cache);
}

#[tokio::test]
async fn every_rule_that_did_not_match_says_what_it_wanted() {
    // **「为什么没走我以为的那条」才是用户在问的问题。**报一句
    // 「不匹配」等于什么都没说。
    let (_d, app) = app();
    let r = run(&app, r#"{"model":"claude-sonnet-4-5"}"#).await;
    let cache_rule = r
        .trace
        .iter()
        .find(|t| t.name == "带缓存的必须走官方")
        .unwrap();
    assert_eq!(cache_rule.verdict, "skipped");
    let why = cache_rule.why.as_deref().unwrap();
    assert!(why.contains("cache"), "{why}");
    assert!(
        why.contains("true") && why.contains("false"),
        "要说清要什么、实际是什么：{why}"
    );

    let long = r.trace.iter().find(|t| t.name == "超长上下文降级").unwrap();
    assert!(
        long.why.as_deref().unwrap().contains("input_tokens"),
        "{long:?}"
    );
}

#[tokio::test]
async fn a_deny_rule_reports_the_reason_the_client_would_see() {
    // 一个没有理由的拒绝和一个 bug 在用户眼里没有区别。
    let (_d, app) = app();
    let r = run(&app, r#"{"model":"claude-sonnet-4-5","image":true}"#).await;
    assert_eq!(r.outcome, "deny");
    assert_eq!(r.reason.as_deref(), Some("这个上游不收图片"));
    assert!(r.candidates.is_empty());
}

#[tokio::test]
async fn a_set_only_rule_still_shows_up_as_matched_and_says_what_it_changes() {
    // set 是从**所有**命中的规则累积的，不只是第一条 —— 试算得体现
    // 这一点，否则用户会以为只有那条 `to` 生效了。
    let (_d, app) = app();
    let r = run(
        &app,
        r#"{"model":"claude-sonnet-4-5","input_tokens":300000}"#,
    )
    .await;
    assert_eq!(r.rule.as_deref(), Some("其余都试试"), "to 还是兜底那条给的");
    assert!(
        r.trace
            .iter()
            .any(|t| t.name == "超长上下文降级" && t.verdict == "matched"),
        "{:?}",
        r.trace
    );
    assert!(
        r.set.iter().any(|s| s.contains("claude-haiku-4-5")),
        "{:?}",
        r.set
    );
    // 换模型会作废整个缓存，而那在长会话里可能比不换还贵
    assert!(
        r.set.iter().any(|s| s.contains("prompt cache")),
        "{:?}",
        r.set
    );
}

#[tokio::test]
async fn the_dry_run_changes_nothing_and_sends_nothing() {
    // 它只算。跑一百遍和跑零遍，对上游和配置文件来说没有区别。
    let (d, app) = app();
    let before = std::fs::read_to_string(d.path().join("config.yaml")).unwrap();
    for _ in 0..5 {
        run(&app, r#"{"model":"x","cache":true}"#).await;
    }
    assert_eq!(
        std::fs::read_to_string(d.path().join("config.yaml")).unwrap(),
        before
    );
}

#[tokio::test]
async fn the_defaults_describe_an_ordinary_request() {
    // 用户只该改他关心的那一两个字段。
    let (_d, app) = app();
    let r = run(&app, r#"{"model":"claude-sonnet-4-5"}"#).await;
    assert_eq!(r.outcome, "route");
    let trace: Vec<_> = r.trace.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(trace.len(), 4, "每条规则都要有个交代：{trace:?}");
}
