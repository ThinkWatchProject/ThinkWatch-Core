//! 一段对话留在上次回答它的那一家，一轮开始时的路由决定沿用到这一轮结束 ——
//! 走真实的管线，看路由事件里说的和实际去的那一家。

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::routing::any;
use tokio::sync::broadcast::Receiver;
use tw_api::{AffinityView, Event, Stay};
use tw_config::{Client, Config, Provider};
use tw_engine::{RouteSet, Rule};

/// 回一个读了 5000 token 缓存的回答：跨轮值得留
async fn upstream() -> SocketAddr {
    let app = Router::new().fallback(any(|| async {
        axum::response::Response::builder()
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"type":"message","content":[],"usage":{"input_tokens":3,"cache_read_input_tokens":5000,"output_tokens":1}}"#,
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

fn rule(name: &str, when: &str, to: &str) -> Rule {
    Rule {
        name: name.into(),
        when: serde_yaml_ng::from_str(when).unwrap(),
        to: Some(to.into()),
        set: None,
        deny: None,
    }
}

/// 输入大的直接去乙，其余在甲乙之间轮流
fn cfg(up: SocketAddr) -> Config {
    Config {
        clients: vec![Client {
            name: "我".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![provider("甲", up), provider("乙", up)],
        groups: vec![tw_engine::Group {
            name: "池".into(),
            kind: tw_engine::GroupType::LoadBalance,
            providers: vec!["甲".into(), "乙".into()],
            selected: None,
        }],
        routes: vec![RouteSet::default_with(vec![
            rule("大输入", "{ input_tokens: \">2000\" }", "乙"),
            rule("轮流", "{}", "池"),
        ])],
        ..Default::default()
    }
}

async fn serve(cfg: Config) -> (SocketAddr, Receiver<Event>) {
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let events = state.bus.subscribe();
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, events)
}

/// 发一个请求，交回它的路由事件：规则、实际回答的那一家、`affinity`
async fn ask(
    gw: SocketAddr,
    rx: &mut Receiver<Event>,
    messages: &str,
) -> (String, String, Option<AffinityView>) {
    ask_in(gw, rx, "会话-1", messages).await
}

/// [`ask`]，在会话 `session` 里
async fn ask_in(
    gw: SocketAddr,
    rx: &mut Receiver<Event>,
    session: &str,
    messages: &str,
) -> (String, String, Option<AffinityView>) {
    let body = format!(
        r#"{{"model":"claude-sonnet-4-5","max_tokens":16,"system":"你是一个助手","messages":{messages}}}"#
    );
    let st = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .header("x-claude-code-session-id", session)
        .body(body)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(st, 200);
    let mut routed = None;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("5 秒内没等到结局")
            .expect("事件流断了");
        match ev {
            Event::RequestRouted {
                rule,
                attempts,
                affinity,
                ..
            } => routed = Some((rule, attempts.last().unwrap().provider.clone(), affinity)),
            Event::RequestFinished { .. } => return routed.expect("没有路由事件"),
            Event::RequestFailed { message, .. } => panic!("失败了：{message:?}"),
            _ => {}
        }
    }
}

fn user(text: &str) -> String {
    format!(r#"{{"role":"user","content":"{text}"}}"#)
}

fn tool_round(i: usize, output: &str) -> String {
    format!(
        r#"{{"role":"assistant","content":[{{"type":"tool_use","id":"t{i}","name":"Read","input":{{}}}}]}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t{i}","content":"{output}"}}]}}"#
    )
}

#[tokio::test]
async fn a_turn_keeps_its_route_and_upstream_and_a_warm_cache_keeps_the_next_turn() {
    let up = upstream().await;
    let (gw, mut rx) = serve(cfg(up)).await;
    // 大到让「大输入」那条规则命中
    let big = "读到的文件内容 ".repeat(1500);

    // 第一轮开头：输入小，轮到池子里的某一家
    let first = user("帮我重构");
    let (rule, answered, affinity) = ask(gw, &mut rx, &format!("[{first}]")).await;
    assert_eq!(rule, "轮流");
    assert_eq!(affinity, None, "头一次没什么可沿用的");

    // 同一轮里工具结果回来了，输入变大：**规则不重新求值，也不换家**
    let (rule, again, affinity) =
        ask(gw, &mut rx, &format!("[{first},{}]", tool_round(1, &big))).await;
    assert_eq!(rule, "轮流", "一轮半路按输入大小换了规则");
    assert_eq!(again, answered, "一轮半路换了家");
    assert_eq!(
        affinity,
        Some(AffinityView {
            held_route: true,
            stayed: Some(Stay::Turn),
        })
    );

    // 新的一轮、输入小：规则重新求值，上一次读了 5000 token 缓存、刚刚才答 —— 留下
    let history = format!(
        "{first},{},{{\"role\":\"assistant\",\"content\":\"好了\"}}",
        tool_round(1, "短")
    );
    let (rule, stayed, affinity) =
        ask(gw, &mut rx, &format!("[{history},{}]", user("再加个测试"))).await;
    assert_eq!(rule, "轮流");
    assert_eq!(stayed, answered, "缓存热着，新的一轮却轮到了别家");
    assert_eq!(
        affinity,
        Some(AffinityView {
            held_route: false,
            stayed: Some(Stay::Cache),
        })
    );

    // 新的一轮、输入大：重新求值，命中「大输入」
    let (rule, to, _) = ask(gw, &mut rx, &format!("[{history},{}]", user(&big))).await;
    assert_eq!(rule, "大输入");
    assert_eq!(to, "乙");
}

/// `load-balance` 记在实际排头的那一家头上：一段对话的新一轮留在了甲（缓存热着），按
/// 权重本该轮到乙 —— 这一次算甲的，之后的两个新对话都去乙，把差的补回来
#[tokio::test]
async fn the_upstream_a_conversation_stays_on_is_charged_and_new_conversations_make_up_for_it() {
    let up = upstream().await;
    let (gw, mut rx) = serve(cfg(up)).await;
    let first = user("帮我重构");
    // 会话一的第一轮：从头轮，甲
    let (_, to, _) = ask_in(gw, &mut rx, "会话-1", &format!("[{first}]")).await;
    assert_eq!(to, "甲");
    // 会话一的第二轮：缓存热着，留在甲
    let history = format!("{first},{{\"role\":\"assistant\",\"content\":\"好了\"}}");
    let (_, to, affinity) = ask_in(
        gw,
        &mut rx,
        "会话-1",
        &format!("[{history},{}]", user("再加个测试")),
    )
    .await;
    assert_eq!(to, "甲");
    assert_eq!(affinity.and_then(|a| a.stayed), Some(Stay::Cache));
    // 新对话：都去乙
    for s in ["会话-2", "会话-3"] {
        let (_, to, _) = ask_in(gw, &mut rx, s, &format!("[{}]", user(s))).await;
        assert_eq!(to, "乙", "{s}");
    }
    // 补齐了，接着挨个轮
    let (_, to, _) = ask_in(gw, &mut rx, "会话-4", &format!("[{}]", user("会话-4"))).await;
    assert_eq!(to, "甲");
}
