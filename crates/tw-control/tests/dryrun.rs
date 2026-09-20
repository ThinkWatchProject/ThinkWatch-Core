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
  - name: 真轮询
    type: load-balance
    session_affinity: false
    providers: [官方, 中转]
routes:
  - name: default
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
        chatgpt: Default::default(),
        home: d.path().join("home"),
    };
    (d, tw_control::router(state))
}

/// 试算要说清按哪条路由算。没指定密钥、路由、草稿时，用唯一那把密钥
async fn run(app: &axum::Router, body: &str) -> tw_api::DryRunResult {
    let mut v: serde_json::Value = serde_json::from_str(body).unwrap();
    if v.get("client").is_none() && v.get("route").is_none() && v.get("draft").is_none() {
        v["client"] = "我".into();
    }
    let (st, b) = send(app, &v.to_string()).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    serde_json::from_str(&b).unwrap()
}

async fn send(app: &axum::Router, body: &str) -> (StatusCode, String) {
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
    let st = r.status();
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (st, String::from_utf8_lossy(&b).to_string())
}

#[tokio::test]
async fn a_plain_request_falls_through_to_the_catch_all() {
    let (_d, app) = app();
    let r = run(&app, r#"{"model":"claude-sonnet-4-5"}"#).await;
    assert_eq!(r.outcome, "route");
    assert_eq!(r.rule.as_deref(), Some("其余都试试"));
    assert_eq!(r.via_group.as_deref(), Some("都试试"));
    assert_eq!(r.candidates.len(), 2);
    assert_eq!(r.route, "default");
    // 开着会话粘滞（默认）的轮询不伤缓存：同一次对话始终落在同一家。
    // 一律标成危险是假警报，而假警报会让人学会忽略这一栏
    assert!(!r.hurts_cache, "粘滞的轮询不该报");
}

#[tokio::test]
async fn round_robin_without_affinity_is_said_to_hurt_the_cache() {
    // **要直说 —— 它决定账单。**
    let (_d, app) = app();
    let r = run(
        &app,
        r#"{"model":"claude-sonnet-4-5","draft":{"name":"草稿","rules":[{"name":"兜底","to":"真轮询"}]}}"#,
    )
    .await;
    assert!(r.hurts_cache);
    assert_eq!(r.route, "草稿");
}

#[tokio::test]
async fn without_a_key_or_a_route_it_refuses_instead_of_guessing() {
    // 以前悄悄拿配置里第一把密钥来算，而那把可能指定了另一条路由
    let (_d, app) = app();
    let (st, body) = send(&app, r#"{"model":"claude-sonnet-4-5"}"#).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    let (st, _) = send(&app, r#"{"model":"m","client":"没有这把"}"#).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = send(&app, r#"{"model":"m","route":"没有这条"}"#).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_route_can_be_tried_by_name_and_a_draft_as_written() {
    let (_d, app) = app();
    let r = run(&app, r#"{"model":"claude-sonnet-4-5","route":"default"}"#).await;
    assert_eq!(r.route, "default");
    assert_eq!(r.rule.as_deref(), Some("其余都试试"));

    let r = run(
        &app,
        r#"{"model":"m","draft":{"name":"新路由","rules":[{"name":"只走中转","to":"中转"}]}}"#,
    )
    .await;
    assert_eq!(r.candidates, ["中转"]);
    // 草稿里写错的规则在求值之前就说出来
    let (st, body) = send(
        &app,
        r#"{"model":"m","draft":{"name":"x","rules":[{"name":"坏的","to":"没有这家"}]}}"#,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(body.contains("没有这家"), "{body}");
}

#[tokio::test]
async fn the_trace_says_which_rule_decided_and_which_only_added_something() {
    let (_d, app) = app();
    let r = run(
        &app,
        r#"{"model":"claude-sonnet-4-5","cache":true,"input_tokens":300000}"#,
    )
    .await;
    let effect = |name: &str| {
        r.trace
            .iter()
            .find(|t| t.name == name)
            .and_then(|t| t.effect.clone())
    };
    assert_eq!(effect("带缓存的必须走官方").as_deref(), Some("decide"));
    assert_eq!(effect("超长上下文降级").as_deref(), Some("apply"));
    assert_eq!(
        effect("其余都试试").as_deref(),
        Some("none"),
        "命中了，但去向早已决定"
    );
    assert_eq!(effect("图片一律拒绝"), None, "没命中就没有作用");
}

#[tokio::test]
async fn the_mismatch_is_the_condition_that_really_failed() {
    // 两个数量条件：前一个满足，后一个不满足。以前报的是前一个
    let (_d, app) = app();
    let draft = |extra: &str| {
        format!(
            r#"{{"model":"m","input_tokens":200000{extra},"draft":{{"name":"x","rules":[
                {{"name":"两个比较","conditions":[
                    {{"field":"input_tokens","values":[">100k"]}},
                    {{"field":"max_tokens","values":["<4k"]}}],"to":"中转"}},
                {{"name":"兜底","to":"官方"}}]}}}}"#
        )
    };
    let r = run(&app, &draft(r#","max_tokens":8000"#)).await;
    let m = r.trace[0].mismatch.as_ref().unwrap();
    assert_eq!((m.field.as_str(), m.got.as_str()), ("max_tokens", "8000"));
    // 请求没写 max_tokens：实际值留空，而不是报一个 0
    let r = run(&app, &draft("")).await;
    let m = r.trace[0].mismatch.as_ref().unwrap();
    assert_eq!((m.field.as_str(), m.got.as_str()), ("max_tokens", ""));

    // `assistant_internal` 是五类里的任意一类：卡住的是模型，不是它
    let r = run(
        &app,
        r#"{"model":"m","intent":"titling","draft":{"name":"x","rules":[
            {"name":"辅助请求走中转","conditions":[
                {"field":"intent","values":["assistant_internal"]},
                {"field":"model","values":["claude-*"]}],"to":"中转"},
            {"name":"兜底","to":"官方"}]}}"#,
    )
    .await;
    assert_eq!(r.trace[0].mismatch.as_ref().unwrap().field, "model");
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
    // 要说清要什么、实际是什么
    assert_eq!(
        cache_rule.mismatch,
        Some(tw_api::MismatchView {
            field: "cache".into(),
            want: vec!["true".into()],
            got: "false".into(),
        })
    );

    let long = r.trace.iter().find(|t| t.name == "超长上下文降级").unwrap();
    assert_eq!(
        long.mismatch.as_ref().map(|m| m.field.as_str()),
        Some("input_tokens"),
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
    // 换模型会作废整个缓存，而那在长会话里可能比不换还贵 —— 所以它单独
    // 是一项，界面才能在这一项旁边说出来
    assert!(
        r.set
            .iter()
            .any(|s| s.field == "model" && s.value == "claude-haiku-4-5"),
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

#[tokio::test]
async fn a_candidate_in_another_format_is_listed_as_converted() {
    // 试算要说出来：规则把一个 Chat 客户端分到了 Anthropic 上游，请求会被转换，
    // 转不过去的字段会被丢掉
    let (_d, app) = app();
    let r = run(
        &app,
        r#"{"model":"claude-sonnet-4-5","dialect":"openai-chat"}"#,
    )
    .await;
    assert_eq!(
        r.converted,
        vec![tw_api::ConvertedView {
            provider: "官方".into(),
            from: "openai-chat".into(),
            to: "anthropic".into(),
        }],
        "协议认不出来的中转站直通，不算转换"
    );
    let r = run(&app, r#"{"model":"claude-sonnet-4-5"}"#).await;
    assert!(r.converted.is_empty(), "{:?}", r.converted);
}
