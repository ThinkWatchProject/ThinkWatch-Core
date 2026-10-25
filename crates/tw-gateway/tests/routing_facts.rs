//! 一个请求的会话和路由，**在事件里说清楚**，不留给听的一方去猜。
//!
//! - 会话在开始时就定了，开始事件带着它（以前只有指纹，落库时才算出会话）；
//! - 走的路由、决定去向的规则、策略组、改写了它的规则，开始时就有，路由事件
//!   给终稿（加上第二阶段的改写和拒绝）；
//! - 认出了密钥、请求却一家上游都没去的（被规则拒绝、选中的上游都服务不了、密钥不让用
//!   那个模型、没有一条规则命中、还没配上游），照样有开始、路由和一条失败 —— 以前这些
//!   请求在事件里根本不存在。

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::routing::any;
use tokio::sync::broadcast::Receiver;
use tw_api::Event;
use tw_config::{Client, Config, Provider};
use tw_engine::{RouteSet, Rule, SetAction};

async fn upstream() -> SocketAddr {
    let app = Router::new().fallback(any(|| async {
        axum::response::Response::builder()
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"type":"message","content":[],"usage":{"input_tokens":3,"output_tokens":1}}"#,
            ))
            .unwrap()
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

fn provider(name: &str, up: SocketAddr) -> Provider {
    Provider {
        name: name.into(),
        base_url: format!("http://{up}"),
        key: Some("sk-x".into()),
        protocol: Some(tw_config::Protocol::Anthropic),
        ..Default::default()
    }
}

fn rule(
    name: &str,
    when: &str,
    to: Option<&str>,
    set: Option<SetAction>,
    deny: Option<&str>,
) -> Rule {
    Rule {
        name: name.into(),
        when: serde_yaml_ng::from_str(when).unwrap(),
        to: to.map(Into::into),
        set,
        deny: deny.map(str::to_string),
    }
}

fn thinking_off() -> Option<SetAction> {
    Some(SetAction {
        reasoning: Some(tw_engine::SetReasoning::Off),
        ..Default::default()
    })
}

/// 密钥 `tw-k` 绑在路由「工作」上，路由里是 `rules`。
fn cfg(providers: Vec<Provider>, rules: Vec<Rule>) -> Config {
    Config {
        clients: vec![Client {
            name: "我".into(),
            key: "tw-k".into(),
            route: Some("工作".into()),
            ..Default::default()
        }],
        providers,
        groups: vec![tw_engine::Group {
            name: "池".into(),
            kind: tw_engine::GroupType::Fallback,
            providers: vec!["up".into()],
            selected: None,
            balance_by: Default::default(),
        }],
        routes: vec![
            RouteSet {
                name: "工作".into(),
                rules,
            },
            RouteSet::default_with(vec![rule("兜底", "{}", Some("up"), None, None)]),
        ],
        ..Default::default()
    }
}

/// 起一个网关，**先订阅事件再起服务** —— 之后才订的话，早到的事件就看不见了。
async fn serve(cfg: Config) -> (SocketAddr, Receiver<Event>) {
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let events = state.bus.subscribe();
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, events)
}

async fn ask(gw: SocketAddr, body: &str) -> u16 {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(body.to_string())
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

fn chat(model: &str, first: &str) -> String {
    format!(
        r#"{{"model":"{model}","max_tokens":16,"system":"你是一个助手","messages":[{{"role":"user","content":"{first}"}}]}}"#
    )
}

/// 一个请求的开始、路由和结局，**按到达的顺序**，到结局为止。
async fn one_request(rx: &mut Receiver<Event>) -> Vec<Event> {
    let mut got = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("5 秒内没等到结局")
            .expect("事件流断了");
        let ending = matches!(
            ev,
            Event::RequestFinished { .. }
                | Event::RequestFailed { .. }
                | Event::RequestCancelled { .. }
        );
        if ending
            || matches!(
                ev,
                Event::RequestStarted { .. } | Event::RequestRouted { .. }
            )
        {
            got.push(ev);
        }
        if ending {
            return got;
        }
    }
}

/// 同一段对话的两个请求，**开始事件里就是同一个会话**；和落库的是同一个值
/// （`{指纹}-{头一个请求开始的时刻}`）。开始事件还带着第一阶段的结论：走的哪条
/// 路由、哪条规则决定的去向、经过哪个组、哪些规则改写了它。
#[tokio::test]
async fn a_start_names_its_session_and_the_first_half_of_its_route() {
    let up = upstream().await;
    let (gw, mut events) = serve(cfg(
        vec![provider("up", up)],
        vec![
            rule(
                "关掉思考",
                "{ model: claude-sonnet-* }",
                None,
                thinking_off(),
                None,
            ),
            rule("走池子", "{}", Some("池"), None, None),
        ],
    ))
    .await;

    assert_eq!(ask(gw, &chat("claude-sonnet-4-5", "帮我重构")).await, 200);
    let first = one_request(&mut events).await;
    assert_eq!(ask(gw, &chat("claude-sonnet-4-5", "帮我重构")).await, 200);
    let second = one_request(&mut events).await;
    assert_eq!(ask(gw, &chat("claude-sonnet-4-5", "另一件事")).await, 200);
    let other = one_request(&mut events).await;

    let session = |evs: &[Event]| match &evs[0] {
        Event::RequestStarted { session, .. } => session.clone(),
        other => panic!("头一条不是开始事件：{other:?}"),
    };
    let s = session(&first).expect("认得出的对话该有会话");
    assert_eq!(
        session(&second).as_deref(),
        Some(s.as_str()),
        "同一段对话分成了两次"
    );
    assert_ne!(session(&other).as_deref(), Some(s.as_str()));
    let Event::RequestStarted { at_ms, .. } = &first[0] else {
        unreachable!()
    };
    assert!(
        s.ends_with(&format!("-{at_ms}")),
        "会话 id 带着头一个请求开始的时刻：{s}"
    );

    let Event::RequestStarted {
        route,
        rule,
        group,
        rewritten_by,
        ..
    } = &first[0]
    else {
        unreachable!()
    };
    assert_eq!(route, "工作", "按密钥指定的路由，不是默认路由");
    assert_eq!(rule, "走池子");
    assert_eq!(group.as_deref(), Some("池"));
    assert_eq!(rewritten_by, &["关掉思考"]);

    let routed = first
        .iter()
        .find_map(|e| match e {
            Event::RequestRouted {
                route,
                rule,
                rewritten_by,
                denied_by,
                attempts,
                ..
            } => Some((route, rule, rewritten_by, denied_by, attempts)),
            _ => None,
        })
        .expect("没有路由事件");
    assert_eq!(routed.0, "工作");
    assert_eq!(routed.1, "走池子");
    assert_eq!(routed.2, &["关掉思考"]);
    assert_eq!(routed.3, &None);
    assert_eq!(routed.4[0].provider, "up");
}

/// 正文里没有任何能认人的东西：**没有会话**，不硬凑一个
#[tokio::test]
async fn a_request_with_nothing_to_recognise_has_no_session() {
    let up = upstream().await;
    let (gw, mut events) = serve(cfg(
        vec![provider("up", up)],
        vec![rule("走池子", "{}", Some("池"), None, None)],
    ))
    .await;
    assert_eq!(
        ask(
            gw,
            r#"{"model":"claude-sonnet-4-5","max_tokens":16,"messages":[]}"#
        )
        .await,
        200
    );
    let evs = one_request(&mut events).await;
    assert!(
        matches!(&evs[0], Event::RequestStarted { session: None, .. }),
        "{evs:?}"
    );
}

/// 规则拒绝了它（第一阶段）：**这一行以前根本不存在** —— 开始事件都没有，
/// 流量里看不见，那条规则命中了多少也数不到。现在是开始（一家上游都不去）、
/// 空尝试链的路由、一条来源为 `denied` 的失败。
#[tokio::test]
async fn a_request_a_rule_denies_starts_is_routed_nowhere_and_fails_as_denied() {
    let up = upstream().await;
    let (gw, mut events) = serve(cfg(
        vec![provider("up", up)],
        vec![
            rule(
                "不许用 opus",
                "{ model: claude-opus-* }",
                None,
                None,
                Some("这个项目不用 opus"),
            ),
            rule("走池子", "{}", Some("池"), None, None),
        ],
    ))
    .await;
    assert_eq!(ask(gw, &chat("claude-opus-4-1", "hi")).await, 403);

    let evs = one_request(&mut events).await;
    assert_eq!(evs.len(), 3, "{evs:?}");
    assert!(
        matches!(&evs[0], Event::RequestStarted { provider, rule, route, session: Some(_), .. }
            if provider.is_empty() && rule == "不许用 opus" && route == "工作"),
        "{evs:?}"
    );
    assert!(
        matches!(&evs[1], Event::RequestRouted { rule, attempts, denied_by: None, .. }
            if rule == "不许用 opus" && attempts.is_empty()),
        "{evs:?}"
    );
    assert!(
        matches!(&evs[2], Event::RequestFailed { source, message, .. }
            if source == "denied" && message.text.contains("这个项目不用 opus")),
        "{evs:?}"
    );
}

/// 规则选中的上游都服务不了（这里是停用了）：规则做了决定，请求一家上游都没去。
/// **也留一行**，记在那条规则上 —— 「规则明明命中了、请求却全失败」正是要看得见的
#[tokio::test]
async fn a_request_whose_chosen_upstreams_cannot_serve_it_is_recorded_under_its_rule() {
    let up = upstream().await;
    let mut off = provider("停用的", up);
    off.disabled = true;
    let (gw, mut events) = serve(cfg(
        vec![provider("up", up), off],
        vec![rule("走停用的", "{}", Some("停用的"), None, None)],
    ))
    .await;
    assert_ne!(ask(gw, &chat("claude-sonnet-4-5", "hi")).await, 200);

    let evs = one_request(&mut events).await;
    assert_eq!(evs.len(), 3, "{evs:?}");
    assert!(
        matches!(&evs[1], Event::RequestRouted { rule, attempts, .. }
            if rule == "走停用的" && attempts.is_empty()),
        "{evs:?}"
    );
    assert!(
        matches!(&evs[2], Event::RequestFailed { source, .. } if source == "config"),
        "{evs:?}"
    );
}

/// 发一个请求，交回状态码和响应体：被拒的请求留一行，**客户端收到的照旧**
async fn answer(gw: SocketAddr, body: &str) -> (u16, serde_json::Value) {
    let r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap())
}

/// 一家上游都没去的那一行：开始（不发往哪一家，模型是客户端写的）、空尝试链的路由、一条
/// 失败。交回三条事件里的路由、决定去向的规则，和失败的来源、错误
fn nowhere(evs: &[Event], model: &str) -> (String, String, String, tw_api::Msg) {
    assert_eq!(evs.len(), 3, "{evs:?}");
    let Event::RequestStarted {
        provider,
        model: started_model,
        route,
        rule,
        ..
    } = &evs[0]
    else {
        panic!("{evs:?}")
    };
    assert!(provider.is_empty(), "{evs:?}");
    assert_eq!(started_model, model, "{evs:?}");
    let Event::RequestRouted {
        route: routed_route,
        rule: routed_rule,
        attempts,
        denied_by: None,
        ..
    } = &evs[1]
    else {
        panic!("{evs:?}")
    };
    assert!(attempts.is_empty(), "{evs:?}");
    assert_eq!((routed_route, routed_rule), (route, rule), "{evs:?}");
    let Event::RequestFailed {
        model: failed_model,
        source,
        message,
        ..
    } = &evs[2]
    else {
        panic!("{evs:?}")
    };
    assert_eq!(failed_model, model, "{evs:?}");
    (
        route.clone(),
        rule.clone(),
        source.slug().to_string(),
        message.clone(),
    )
}

/// 密钥不让用的模型：准入没过。**照样留一行**，记在决定去向的那条规则上，模型是客户端写的
/// 那个，错误照告诉客户端的那一句 —— 以前这个请求在流量里根本看不见，没法排查。客户端收到
/// 的还是那个 400
#[tokio::test]
async fn a_model_the_key_may_not_use_is_recorded_under_its_rule() {
    let up = upstream().await;
    let mut config = cfg(
        vec![provider("up", up)],
        vec![rule("走池子", "{}", Some("池"), None, None)],
    );
    config.clients[0].allow = Some(vec!["claude-sonnet-*".into()]);
    let (gw, mut events) = serve(config).await;

    let (status, body) = answer(gw, &chat("claude-opus-4-1", "hi")).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        body["error"]["message"],
        "[ThinkWatch] Gateway key `我` may not use model claude-opus-4-1. GET /v1/models lists \
         the models that are available.",
        "{body}"
    );

    let evs = one_request(&mut events).await;
    let (route, rule, source, why) = nowhere(&evs, "claude-opus-4-1");
    assert_eq!((route.as_str(), rule.as_str()), ("工作", "走池子"));
    assert_eq!(source, "request");
    assert_eq!(why.code, "gw.model.not_allowed");
    assert_eq!(why.args["model"], "claude-opus-4-1");
    assert_eq!(why.args["key"], "我");

    // 让用的照常
    assert_eq!(ask(gw, &chat("claude-sonnet-4-5", "hi")).await, 200);
}

/// 没有一条规则命中：规则还没做出决定。**也留一行**，记在密钥走的那条路由上，决定去向的规则
/// 空着
#[tokio::test]
async fn a_request_no_rule_matches_is_recorded_on_its_route_with_no_rule() {
    let up = upstream().await;
    let (gw, mut events) = serve(cfg(
        vec![provider("up", up)],
        vec![rule(
            "只给 opus",
            "{ model: claude-opus-* }",
            Some("池"),
            None,
            None,
        )],
    ))
    .await;

    let (status, body) = answer(gw, &chat("claude-sonnet-4-5", "hi")).await;
    assert_eq!(status, 500, "{body}");
    let text = body["error"]["message"].as_str().unwrap();
    assert!(text.starts_with("[ThinkWatch] Routing failed: "), "{body}");

    let evs = one_request(&mut events).await;
    let (route, rule, source, why) = nowhere(&evs, "claude-sonnet-4-5");
    assert_eq!((route.as_str(), rule.as_str()), ("工作", ""));
    assert_eq!(source, "config");
    assert_eq!(why.code, "gw.route.failed");
    assert_eq!(format!("[ThinkWatch] {}", why.text), text);
}

/// 还没配上游：**也留一行**，记在密钥走的那条路由上，决定去向的规则空着。客户端收到的照旧
/// 是「还没配上游」那一句
#[tokio::test]
async fn a_request_with_no_upstream_configured_is_recorded() {
    let config = Config {
        clients: vec![Client {
            name: "我".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let (gw, mut events) = serve(config).await;

    let (status, body) = answer(gw, &chat("claude-sonnet-4-5", "hi")).await;
    assert_eq!(status, 500, "{body}");
    assert_eq!(
        body["error"]["message"],
        "[ThinkWatch] No upstream is configured yet. Add one in ThinkWatch Lite, or under \
         `providers` in config.yaml.",
        "{body}"
    );

    let evs = one_request(&mut events).await;
    let (route, rule, source, why) = nowhere(&evs, "claude-sonnet-4-5");
    assert_eq!(
        (route.as_str(), rule.as_str()),
        (tw_engine::DEFAULT_ROUTE, "")
    );
    assert_eq!(source, "config");
    assert_eq!(why.code, "gw.config.no_upstreams");
}

/// 选定上游之后才判断的规则拒绝了它（第二阶段）：**路由事件照样发**，说是哪条规则、
/// 在哪一家停下的 —— 被拒的那一跳在尝试链上，没有发出去
#[tokio::test]
async fn a_phase_two_denial_names_its_rule_and_the_upstream_it_stopped_at() {
    let up = upstream().await;
    let (gw, mut events) = serve(cfg(
        vec![provider("up", up)],
        vec![
            rule(
                "池子不收",
                "{ provider_would_be: up }",
                None,
                None,
                Some("不发给这家"),
            ),
            rule("走池子", "{}", Some("池"), None, None),
        ],
    ))
    .await;
    assert_eq!(ask(gw, &chat("claude-sonnet-4-5", "hi")).await, 403);

    let evs = one_request(&mut events).await;
    let Some(Event::RequestRouted {
        rule,
        denied_by,
        attempts,
        ..
    }) = evs
        .iter()
        .find(|e| matches!(e, Event::RequestRouted { .. }))
    else {
        panic!("第二阶段拒绝了，却没有路由事件：{evs:?}");
    };
    assert_eq!(rule, "走池子", "决定去向的还是第一阶段那条");
    assert_eq!(denied_by.as_deref(), Some("池子不收"));
    assert_eq!(attempts.len(), 1, "{attempts:?}");
    assert_eq!(
        (attempts[0].provider.as_str(), attempts[0].outcome.slug()),
        ("up", "error")
    );
    assert!(
        matches!(evs.last(), Some(Event::RequestFailed { source, .. }) if source == "denied"),
        "{evs:?}"
    );
}

/// 第二阶段又改写了它：**路由事件里的改写是全的**，开始事件里只有第一阶段的
#[tokio::test]
async fn phase_two_rewrites_join_the_routed_record() {
    let up = upstream().await;
    let (gw, mut events) = serve(cfg(
        vec![provider("up", up)],
        vec![
            rule(
                "池子里关思考",
                "{ provider_would_be: up }",
                None,
                thinking_off(),
                None,
            ),
            rule(
                "限长",
                "{}",
                None,
                Some(SetAction {
                    max_tokens: Some(8),
                    ..Default::default()
                }),
                None,
            ),
            rule("走池子", "{}", Some("池"), None, None),
        ],
    ))
    .await;
    assert_eq!(ask(gw, &chat("claude-sonnet-4-5", "hi")).await, 200);

    let evs = one_request(&mut events).await;
    assert!(
        matches!(&evs[0], Event::RequestStarted { rewritten_by, .. } if rewritten_by == &["限长"]),
        "{evs:?}"
    );
    assert!(
        evs.iter()
            .any(|e| matches!(e, Event::RequestRouted { rewritten_by, .. }
            if rewritten_by == &["限长", "池子里关思考"])),
        "{evs:?}"
    );
}

/// 规则改写了模型名：**尝试链上记着发出去的是哪个** —— 费用按它算，价格未知时
/// 该补的也是它。没改写的不记，发出去的就是客户端要的那个
#[tokio::test]
async fn the_attempt_names_the_model_a_rule_rewrote_the_request_to() {
    const BEDROCK: &str = "us.anthropic.claude-sonnet-4-5-20250929-v1:0";
    let up = upstream().await;
    let (gw, mut events) = serve(cfg(
        vec![provider("up", up)],
        vec![
            rule(
                "换成 Bedrock 的名字",
                "{ model: claude-sonnet-* }",
                None,
                Some(SetAction {
                    model: Some(BEDROCK.into()),
                    ..Default::default()
                }),
                None,
            ),
            rule(
                "原样",
                "{ model: claude-haiku-* }",
                None,
                Some(SetAction {
                    model: Some("claude-haiku-4-5".into()),
                    ..Default::default()
                }),
                None,
            ),
            rule("走池子", "{}", Some("池"), None, None),
        ],
    ))
    .await;
    let attempts = |evs: &[Event]| {
        evs.iter()
            .find_map(|e| match e {
                Event::RequestRouted { attempts, .. } => Some(attempts.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{evs:?}"))
    };

    assert_eq!(ask(gw, &chat("claude-sonnet-4-5", "hi")).await, 200);
    let evs = one_request(&mut events).await;
    assert_eq!(attempts(&evs)[0].model.as_deref(), Some(BEDROCK));

    // 改写成同一个名字等于没改写
    assert_eq!(ask(gw, &chat("claude-haiku-4-5", "hi")).await, 200);
    assert_eq!(attempts(&one_request(&mut events).await)[0].model, None);

    assert_eq!(ask(gw, &chat("claude-opus-4-1", "hi")).await, 200);
    assert_eq!(attempts(&one_request(&mut events).await)[0].model, None);
}
