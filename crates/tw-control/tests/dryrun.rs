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
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
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
    app_with(CFG)
}

fn app_with(text: &str) -> (tempfile::TempDir, axum::Router) {
    let (d, app, _) = app_and_gateway(text);
    (d, app)
}

/// [`app_with`]，连同它的数据面：要看试算和数据面是不是读的同一份状态
fn app_and_gateway(text: &str) -> (tempfile::TempDir, axum::Router, tw_gateway::AppState) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, text).unwrap();
    let cfg: tw_config::Config = serde_yaml_ng::from_str(text).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let gw_for_test = gw.clone();
    let bus = gw.bus.clone();
    let state = ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        store: None,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    };
    (d, tw_control::router(state), gw_for_test)
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
    assert_eq!(r.outcome.slug(), "route");
    assert_eq!(r.rule.as_deref(), Some("其余都试试"));
    assert_eq!(r.via_group.as_deref(), Some("都试试"));
    assert_eq!(r.candidates.len(), 2);
    assert_eq!(r.route, "default");
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
            .and_then(|t| t.effect.map(|e| e.slug()))
    };
    assert_eq!(effect("带缓存的必须走官方"), Some("decide"));
    assert_eq!(effect("超长上下文降级"), Some("apply"));
    assert_eq!(effect("其余都试试"), Some("none"), "命中了，但去向早已决定");
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
    assert_eq!((m.field.slug(), m.got.as_str()), ("max_tokens", "8000"));
    // 请求没写 max_tokens：实际值留空，而不是报一个 0
    let r = run(&app, &draft("")).await;
    let m = r.trace[0].mismatch.as_ref().unwrap();
    assert_eq!((m.field.slug(), m.got.as_str()), ("max_tokens", ""));

    // `assistant_internal` 是五类里的任意一类：卡住的是模型，不是它。
    // 起标题出厂是转发，带着类别到得了规则
    let r = run(
        &app,
        r#"{"model":"m","intent":"titling","draft":{"name":"x","rules":[
            {"name":"辅助请求走中转","conditions":[
                {"field":"intent","values":["assistant_internal"]},
                {"field":"model","values":["claude-*"]}],"to":"中转"},
            {"name":"兜底","to":"官方"}]}}"#,
    )
    .await;
    assert_eq!(r.trace[0].mismatch.as_ref().unwrap().field.slug(), "model");
}

#[tokio::test]
async fn a_cached_request_is_pinned_to_the_official_upstream() {
    let (_d, app) = app();
    let r = run(&app, r#"{"model":"claude-sonnet-4-5","cache":true}"#).await;
    assert_eq!(r.rule.as_deref(), Some("带缓存的必须走官方"));
    assert_eq!(r.candidates, vec!["官方".to_string()]);
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
    assert_eq!(cache_rule.verdict.slug(), "skipped");
    // 要说清要什么、实际是什么
    assert_eq!(
        cache_rule.mismatch,
        Some(tw_api::MismatchView {
            field: tw_api::ConditionField::Cache,
            want: vec!["true".into()],
            got: "false".into(),
        })
    );

    let long = r.trace.iter().find(|t| t.name == "超长上下文降级").unwrap();
    assert_eq!(
        long.mismatch.as_ref().map(|m| m.field.slug()),
        Some("input_tokens"),
        "{long:?}"
    );
}

#[tokio::test]
async fn a_deny_rule_reports_the_reason_the_client_would_see() {
    // 一个没有理由的拒绝和一个 bug 在用户眼里没有区别。
    let (_d, app) = app();
    let r = run(&app, r#"{"model":"claude-sonnet-4-5","image":true}"#).await;
    assert_eq!(r.outcome.slug(), "deny");
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
            .any(|t| t.name == "超长上下文降级" && t.verdict.slug() == "matched"),
        "{:?}",
        r.trace
    );
    // 换模型会作废整个缓存，而那在长会话里可能比不换还贵 —— 所以它单独
    // 是一项，界面才能在这一项旁边说出来
    assert!(
        r.set
            .iter()
            .any(|s| s.field.slug() == "model" && s.value == "claude-haiku-4-5"),
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

/// `load-balance` 的试算读数据面记着的那一份轮询状态，**只读不记**：试算几次都还是同一家
/// 排头；数据面排过之后，试算跟着变。每个候选带着它的权重
#[tokio::test]
async fn a_weighted_group_is_tried_from_the_data_planes_state_without_moving_it() {
    let text = CFG.replace(
        "    providers: [官方, 中转]\n",
        "    providers: [{ name: 官方, weight: 3 }, 中转]\n",
    );
    let (_d, app, gw) = app_and_gateway(&text);
    let weights = |r: &tw_api::DryRunResult| {
        r.candidate_models
            .iter()
            .map(|c| (c.provider.clone(), c.weight))
            .collect::<Vec<_>>()
    };
    for _ in 0..5 {
        let r = run(&app, r#"{"model":"claude-sonnet-4-5"}"#).await;
        assert_eq!(r.candidates, ["官方", "中转"]);
        assert_eq!(
            weights(&r),
            [("官方".to_string(), Some(3)), ("中转".to_string(), Some(1))]
        );
    }
    // 数据面排了两次，都排给了官方：3:1 的下一个是中转
    let rt = gw.runtime();
    let g = rt
        .engine
        .groups()
        .iter()
        .find(|g| g.name == "都试试")
        .unwrap();
    let members = g.names();
    for _ in 0..2 {
        let turn = gw.balance.turn(g);
        let f = tw_engine::Facts {
            current_weight: turn.current(),
            ..Default::default()
        };
        assert_eq!(rt.engine.order(Some(&g.name), &members, &f)[0], "官方");
        turn.charge(&members, &f, "官方");
    }
    let before = gw.balance.peek(g);
    for _ in 0..3 {
        let r = run(&app, r#"{"model":"claude-sonnet-4-5"}"#).await;
        assert_eq!(r.candidates, ["中转", "官方"], "排头的之后其余按组里的顺序");
    }
    assert_eq!(gw.balance.peek(g), before, "试算不记账");

    // 不经过 `load-balance` 的候选不带权重
    let r = run(&app, r#"{"model":"claude-sonnet-4-5","cache":true}"#).await;
    assert_eq!(weights(&r), [("官方".to_string(), None)]);
}

#[tokio::test]
async fn the_defaults_describe_an_ordinary_request() {
    // 用户只该改他关心的那一两个字段。
    let (_d, app) = app();
    let r = run(&app, r#"{"model":"claude-sonnet-4-5"}"#).await;
    assert_eq!(r.outcome.slug(), "route");
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
            from: tw_api::Dialect::OpenaiChat,
            to: tw_api::Dialect::Anthropic,
        }],
        "协议认不出来的中转站直通，不算转换"
    );
    let r = run(&app, r#"{"model":"claude-sonnet-4-5"}"#).await;
    assert!(r.converted.is_empty(), "{:?}", r.converted);
}

/// 辅助请求先过 `client_probes` 那一层，规则轮不到它。
///
/// **不看那一层的话，试算会对一个根本到不了规则的请求给出一条路线。**
/// 连通性检查出厂是本地应答、一个字节都不出本机，而试算会说它走某个上游
/// —— 用户拿着一个对的答案去查一个不存在的现象。
#[tokio::test]
async fn a_locally_answered_probe_never_reaches_the_rules() {
    let (_d, app) = app();
    let r = run(
        &app,
        r#"{"model":"claude-sonnet-4-5","intent":"health_check"}"#,
    )
    .await;
    assert_eq!(r.outcome.slug(), "intercepted");
    assert!(r.candidates.is_empty(), "本地应答不会发给任何上游：{r:?}");
    assert!(r.rule.is_none());
    // 一条规则都没求值，所以明细是空的。一份「全部未命中」的明细会
    // 读成「规则写错了」
    assert!(r.trace.is_empty(), "不该列出没求过的规则：{r:?}");
}

/// 转发的那几类和普通请求一样走完规则。起标题出厂就是转发。
#[tokio::test]
async fn a_forwarded_probe_is_evaluated_like_any_other_request() {
    let (_d, app) = app();
    let r = run(&app, r#"{"model":"claude-sonnet-4-5","intent":"titling"}"#).await;
    assert_eq!(r.outcome.slug(), "route");
    assert_eq!(r.rule.as_deref(), Some("其余都试试"));
    assert!(!r.trace.is_empty(), "转发的要走完规则：{r:?}");

    // 出厂本地应答的那几类，设成转发也一样
    let (_d, app) = app_with(&format!("{CFG}client_probes:\n  health_check: forward\n"));
    let r = run(
        &app,
        r#"{"model":"claude-sonnet-4-5","intent":"health_check"}"#,
    )
    .await;
    assert_eq!(r.outcome.slug(), "route");
    assert!(!r.trace.is_empty(), "{r:?}");
}

/// 规则里的辅助请求条件对转发的类别成立，不用再另外设置什么。
#[tokio::test]
async fn an_intent_rule_catches_a_forwarded_probe() {
    let (_d, app) = app();
    let draft = r#"{"name":"x","rules":[
        {"name":"标题走中转","conditions":[{"field":"intent","values":["titling"]}],"to":"中转"},
        {"name":"兜底","to":"官方"}]}"#;
    let r = run(
        &app,
        &format!(r#"{{"model":"claude-sonnet-4-5","intent":"titling","draft":{draft}}}"#),
    )
    .await;
    assert_eq!(r.rule.as_deref(), Some("标题走中转"), "{r:?}");
    // 普通请求不带类别，同一条规则不命中
    let r = run(
        &app,
        &format!(r#"{{"model":"claude-sonnet-4-5","draft":{draft}}}"#),
    )
    .await;
    assert_eq!(r.rule.as_deref(), Some("兜底"), "{r:?}");
}

// ─────────────────────────────────────────────────────────── 别名与指定模型

/// 三家上游各用各的写法：官方 `claude-*`、中转 `anthropic/*`、智谱 `glm-*`
const ALIASED: &str = r#"version: 1
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
clients:
  - name: 我
    key: tw-k
providers:
  - name: 官方
    base_url: https://api.anthropic.com
    key: sk-a
    models_only: ["claude-*"]
  - name: 中转
    base_url: https://relay.example.com
    key: sk-b
    models_only: ["anthropic/*"]
  - name: 智谱
    base_url: https://zhipu.example.com
    key: sk-c
    models_only: ["glm-*"]
aliases:
  opus: [claude-opus-5, anthropic/claude-opus-5]
  glm-air: glm-5-air-0414
routes:
  - name: default
    rules:
      - name: 指定
        when: { model: pinned }
        to:
          - { provider: 智谱, model: glm-5 }
          - { provider: 官方, model: claude-opus-5 }
      - name: 中转写法的 Opus
        when: { model: "anthropic/*" }
        to: 中转
      - name: Sonnet 换成 Opus
        when: { model: claude-sonnet-* }
        set: { model: opus }
      - name: 智谱的叫法
        when: { provider_would_be: 智谱, model: glm-* }
        set: { model: glm-air }
      - name: 其余
        to: __all__
"#;

/// 每个候选：（上游，发给它的名字，来历）
fn sent(r: &tw_api::DryRunResult) -> Vec<(&str, Option<&str>, Option<&str>)> {
    assert_eq!(
        r.candidate_models
            .iter()
            .map(|c| c.provider.as_str())
            .collect::<Vec<_>>(),
        r.candidates,
        "和候选一一对应"
    );
    r.candidate_models
        .iter()
        .map(|c| {
            (
                c.provider.as_str(),
                c.sent_model.as_deref(),
                c.model_via.as_deref(),
            )
        })
        .collect()
}

/// 每个候选发出去的名字和来历：别名对到各家、规则改写（改成别名也照常对）、指定模型、
/// 阶段二原样；和客户端写的一样时没有来历
#[tokio::test]
async fn each_candidate_shows_the_name_it_is_sent_and_why() {
    let (_d, app) = app_with(ALIASED);

    // 请求别名：写中转写法（真名）的那条规则也管到它
    let r = run(&app, r#"{"model":"opus"}"#).await;
    assert_eq!(r.rule.as_deref(), Some("中转写法的 Opus"), "{r:?}");
    assert_eq!(
        sent(&r),
        [("中转", Some("anthropic/claude-opus-5"), Some("alias"))]
    );
    let t = r
        .trace
        .iter()
        .find(|t| t.name == "中转写法的 Opus")
        .unwrap();
    assert_eq!(t.verdict, tw_api::RuleVerdict::Matched);

    // 请求真名：那条规则不管它，没对上的条件照实说
    let r = run(&app, r#"{"model":"claude-opus-5"}"#).await;
    assert_eq!(r.rule.as_deref(), Some("其余"), "{r:?}");
    assert_eq!(sent(&r), [("官方", Some("claude-opus-5"), None)]);
    let t = r
        .trace
        .iter()
        .find(|t| t.name == "中转写法的 Opus")
        .unwrap();
    assert_eq!(t.mismatch.as_ref().unwrap().got, "claude-opus-5");

    // 规则改写成别名：照常对到各家；对不到的那一家跳过
    let r = run(&app, r#"{"model":"claude-sonnet-5"}"#).await;
    assert_eq!(
        sent(&r),
        [
            ("官方", Some("claude-opus-5"), Some("rule")),
            ("中转", Some("anthropic/claude-opus-5"), Some("rule")),
        ]
    );
    assert_eq!(
        r.skipped
            .iter()
            .map(|s| s.provider.as_str())
            .collect::<Vec<_>>(),
        ["智谱"]
    );

    // 指定模型：按列表的顺序，原样
    let r = run(&app, r#"{"model":"pinned"}"#).await;
    assert_eq!(
        sent(&r),
        [
            ("智谱", Some("glm-5"), Some("pinned")),
            ("官方", Some("claude-opus-5"), Some("pinned")),
        ]
    );

    // 阶段二改的名字原样发：`glm-air` 是别名也不对到 `glm-5-air-0414`
    let r = run(&app, r#"{"model":"glm-5"}"#).await;
    assert_eq!(sent(&r), [("智谱", Some("glm-air"), Some("rule"))]);
}

/// `cheapest` 按每一家发出去的名字比价：同一个别名在两家是两个模型、两个价
#[tokio::test]
async fn cheapest_prices_each_upstream_by_the_name_it_is_sent() {
    let (_d, app) = app_with(
        r#"version: 1
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
clients:
  - name: 我
    key: tw-k
providers:
  - name: 贵的
    base_url: https://a.example.com
    key: sk-a
    models_only: ["claude-opus-*"]
  - name: 便宜的
    base_url: https://b.example.com
    key: sk-b
    models_only: ["claude-haiku-*"]
aliases:
  快: [claude-opus-4-1, claude-haiku-4-5]
groups:
  - name: 省钱
    type: cheapest
    providers: [贵的, 便宜的]
routes:
  - name: default
    rules:
      - name: 省钱
        to: 省钱
"#,
    );
    let r = run(&app, r#"{"model":"快"}"#).await;
    // 按别名本身比的话两家都算不出价钱，次序不变
    assert_eq!(
        sent(&r),
        [
            ("便宜的", Some("claude-haiku-4-5"), Some("alias")),
            ("贵的", Some("claude-opus-4-1"), Some("alias")),
        ]
    );
}

/// 给了密钥：发给哪一家的名字这把密钥不让用，那一家跳过，说是密钥的事 —— 和数据面一样，
/// 故障转移到不了它。指定的、阶段二改的名字按名字本身看
#[tokio::test]
async fn a_candidate_whose_model_the_key_may_not_use_is_skipped() {
    let cfg = ALIASED.replace(
        "clients:\n  - name: 我\n    key: tw-k\n",
        "clients:\n  - name: 我\n    key: tw-k\n  - name: 只用智谱\n    key: tw-z\n    allow: [glm-*]\n  - name: 只用 glm-5\n    key: tw-5\n    allow: [glm-5]\n",
    );
    let (_d, app) = app_with(&cfg);
    let skipped = |r: &tw_api::DryRunResult| {
        r.skipped
            .iter()
            .map(|s| (s.provider.clone(), s.reason))
            .collect::<Vec<_>>()
    };

    // 指定模型：官方的 claude-opus-5 这把密钥不让用
    let r = run(&app, r#"{"model":"pinned","client":"只用智谱"}"#).await;
    assert_eq!(sent(&r), [("智谱", Some("glm-5"), Some("pinned"))]);
    assert_eq!(
        skipped(&r),
        [("官方".to_string(), tw_api::ServeSkip::NotAllowed)]
    );

    // 阶段二给智谱改的 glm-air 原样看：只许 glm-5 的密钥不让用它，别的两家本来就不在范围里
    let r = run(&app, r#"{"model":"glm-5","client":"只用 glm-5"}"#).await;
    assert_eq!(r.outcome, tw_api::DryRunOutcome::Unavailable, "{r:?}");
    assert_eq!(
        skipped(&r),
        [
            ("官方".to_string(), tw_api::ServeSkip::OutOfScope),
            ("中转".to_string(), tw_api::ServeSkip::OutOfScope),
            ("智谱".to_string(), tw_api::ServeSkip::NotAllowed),
        ]
    );
    // 不给密钥（按路由试算）不看范围
    let r = run(&app, r#"{"model":"glm-5","route":"default"}"#).await;
    assert_eq!(sent(&r), [("智谱", Some("glm-air"), Some("rule"))]);
}
