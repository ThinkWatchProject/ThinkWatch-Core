//! 带着上游封存内容的对话（见 `tw_dialect::sealed`）送回封存它的那一家；别家解不开时，失败的
//! 说明讲清楚怎么回事 —— 走真实的管线，上游是两个各自封存、各自只认自己封存的假上游。

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::routing::any;
use tokio::sync::broadcast::Receiver;
use tw_api::{AttemptOutcome, AttemptView, Event, Stay};
use tw_config::{Client, Config, Provider};
use tw_engine::{RouteSet, Rule};

/// 一个假的 OpenAI 账号：回答里带一段它封存的推理（`gAAAA-<名字>`），请求里有别家封存的就
/// 拒绝 —— 和 OpenAI 一样回 400 `invalid_encrypted_content`；`in_stream` 的像 Codex 后端那样
/// 回 200，在流里报这个错。`down` 打开时回 503。
struct Account {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    down: Arc<AtomicBool>,
}

async fn account(name: &'static str, in_stream: bool) -> Account {
    let hits = Arc::new(AtomicUsize::new(0));
    let down = Arc::new(AtomicBool::new(false));
    let (h, d) = (hits.clone(), down.clone());
    let app = Router::new().fallback(any(move |body: String| {
        let (h, d) = (h.clone(), d.clone());
        async move {
            h.fetch_add(1, Ordering::SeqCst);
            let builder = axum::response::Response::builder();
            if d.load(Ordering::SeqCst) {
                return builder.status(503).body(axum::body::Body::from("down")).unwrap();
            }
            let foreign = body
                .match_indices("gAAAA-")
                .any(|(i, _)| !body[i + 6..].starts_with(name));
            if foreign && in_stream {
                let frame = r#"{"type":"error","error":{"type":"invalid_request_error","code":"invalid_encrypted_content","message":"Encrypted function output content could not be decrypted or decoded."}}"#;
                return builder
                    .header("content-type", "text/event-stream")
                    .body(axum::body::Body::from(format!(
                        "event: error\ndata: {frame}\n\n"
                    )))
                    .unwrap();
            }
            if foreign {
                return builder
                    .status(400)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"error":{"message":"The encrypted content for item rs_1 could not be verified.","type":"invalid_request_error","param":null,"code":"invalid_encrypted_content"}}"#,
                    ))
                    .unwrap();
            }
            let answer = format!(
                r#"{{"id":"resp_1","object":"response","status":"completed","model":"gpt-5","output":[{{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"gAAAA-{name}"}},{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"ok"}}]}}],"usage":{{"input_tokens":3,"output_tokens":1,"total_tokens":4}}}}"#
            );
            builder
                .header("content-type", "application/json")
                .body(axum::body::Body::from(answer))
                .unwrap()
        }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Account { addr, hits, down }
}

fn provider(name: &str, up: &Account) -> Provider {
    Provider {
        name: name.into(),
        base_url: format!("http://{}/v1", up.addr),
        key: Some("sk-x".into()),
        protocol: Some(tw_config::Protocol::OpenaiResponses),
        ..Default::default()
    }
}

/// 甲乙两家。`to` 是唯一一条规则送去的地方：一家，或者轮流（`池`，没有缓存可留，每一轮
/// 都轮到下一家）
fn cfg(accounts: &[(&str, &Account)], to: &str) -> Config {
    Config {
        clients: vec![Client {
            name: "我".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: accounts.iter().map(|(n, a)| provider(n, a)).collect(),
        groups: vec![tw_engine::Group {
            name: "池".into(),
            kind: tw_engine::GroupType::LoadBalance,
            providers: accounts.iter().map(|(n, _)| (*n).into()).collect(),
            selected: None,
            balance_by: Default::default(),
        }],
        routes: vec![RouteSet::default_with(vec![Rule {
            name: "Codex".into(),
            when: serde_yaml_ng::from_str("{}").unwrap(),
            to: Some(to.into()),
            set: None,
            deny: None,
        }])],
        ..Default::default()
    }
}

async fn serve(state: tw_gateway::AppState) -> (SocketAddr, Receiver<Event>) {
    let events = state.bus.subscribe();
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, events)
}

/// 这段对话的第 `turn` 轮：前面每一轮都由用户说一句、模型带回 `sealed` 里那家封存的推理；
/// `agents` 时最后还有一条子代理交回来的、加密了的话
fn body(turn: usize, sealed: &str, agents: bool) -> String {
    let mut input = Vec::new();
    for i in 0..turn {
        input.push(format!(r#"{{"role":"user","content":"第 {i} 轮"}}"#));
        if i + 1 < turn {
            input.push(format!(
                r#"{{"type":"reasoning","id":"rs_{i}","summary":[],"encrypted_content":"gAAAA-{sealed}"}}"#
            ));
        }
    }
    if agents {
        input.push(format!(
            r#"{{"type":"agent_message","author":"/root/worker","recipient":"/root","content":[{{"type":"input_text","text":"Message Type: MESSAGE\nPayload:\n"}},{{"type":"encrypted_content","encrypted_content":"gAAAA-{sealed}"}}]}}"#
        ));
    }
    format!(
        r#"{{"model":"gpt-5","instructions":"You are Codex.","prompt_cache_key":"对话-1","store":false,"input":[{}]}}"#,
        input.join(",")
    )
}

/// 结局：路由事件里试过的上游、留下的理由，和失败时的那句话
struct Outcome {
    status: u16,
    attempts: Vec<AttemptView>,
    stayed: Option<Stay>,
    failed: Option<tw_types::Msg>,
}

async fn ask(gw: SocketAddr, rx: &mut Receiver<Event>, body: String) -> Outcome {
    ask_in(gw, rx, "codex-1", body).await
}

/// [`ask`]，在会话 `session` 里
async fn ask_in(gw: SocketAddr, rx: &mut Receiver<Event>, session: &str, body: String) -> Outcome {
    let status = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{gw}/v1/responses"))
        .header("authorization", "Bearer tw-k")
        .header("session_id", session)
        .body(body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    let mut routed = (Vec::new(), None);
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("5 秒内没等到结局")
            .expect("事件流断了");
        match ev {
            Event::RequestRouted {
                attempts, affinity, ..
            } => routed = (attempts, affinity.and_then(|a| a.stayed)),
            Event::RequestFinished { .. } => {
                return Outcome {
                    status,
                    attempts: routed.0,
                    stayed: routed.1,
                    failed: None,
                };
            }
            Event::RequestFailed { message, .. } => {
                return Outcome {
                    status,
                    attempts: routed.0,
                    stayed: routed.1,
                    failed: Some(message),
                };
            }
            _ => {}
        }
    }
}

fn served_by(o: &Outcome) -> Vec<(&str, AttemptOutcome)> {
    o.attempts
        .iter()
        .map(|a| (a.provider.as_str(), a.outcome))
        .collect()
}

#[tokio::test]
async fn sealed_agent_messages_go_back_to_the_account_that_sealed_them() {
    let (a, b) = (account("甲", false).await, account("乙", false).await);
    // 一个请求最多等一秒；退避的一秒是一百毫秒，不浮动
    let waiting = |to: &str| {
        let mut c = cfg(&[("甲", &a), ("乙", &b)], to);
        c.failover.wait_secs = 1;
        c
    };
    let mut state = tw_gateway::AppState::new(waiting("甲")).unwrap();
    state.retry_backoff = tw_gateway::retry::Backoff {
        tick: Duration::from_millis(100),
        jitter: 0.0,
    };
    let (gw, mut rx) = serve(state.clone()).await;
    let o = ask(gw, &mut rx, body(1, "甲", false)).await;
    assert_eq!(served_by(&o), [("甲", AttemptOutcome::Served)]);

    // 改了配置：规则现在送去乙。带着甲封存的、子代理交回来的话的这段对话照旧回甲，乙不做备选
    state.reload(waiting("乙")).unwrap();
    let o = ask(gw, &mut rx, body(2, "甲", true)).await;
    assert_eq!(o.status, 200);
    assert_eq!(served_by(&o), [("甲", AttemptOutcome::Served)]);
    assert_eq!(o.stayed, Some(Stay::Sealed));
    assert_eq!(b.hits.load(Ordering::SeqCst), 0);

    // 甲发不出去：退避着重试的也只是甲，到期限了报甲的错，不转给解不开的乙
    a.down.store(true, Ordering::SeqCst);
    let o = ask(gw, &mut rx, body(3, "甲", true)).await;
    assert_ne!(o.status, 200);
    assert!(o.attempts.len() > 1, "{:#?}", o.attempts);
    assert!(
        o.attempts.iter().all(|a| a.provider == "甲"),
        "{:#?}",
        o.attempts
    );
    assert_eq!(b.hits.load(Ordering::SeqCst), 0);

    // 新的对话照新的规则走
    let fresh = body(1, "甲", false).replace("对话-1", "对话-2");
    let o = ask_in(gw, &mut rx, "codex-2", fresh).await;
    assert_eq!(served_by(&o), [("乙", AttemptOutcome::Served)]);
}

#[tokio::test]
async fn a_conversation_whose_sealing_upstream_was_removed_is_refused_plainly() {
    let (a, b) = (account("甲", false).await, account("乙", false).await);
    let state = tw_gateway::AppState::new(cfg(&[("甲", &a), ("乙", &b)], "甲")).unwrap();
    let (gw, mut rx) = serve(state.clone()).await;
    ask(gw, &mut rx, body(1, "甲", false)).await;

    state.reload(cfg(&[("乙", &b)], "乙")).unwrap();
    let o = ask(gw, &mut rx, body(2, "甲", true)).await;
    assert_ne!(o.status, 200);
    assert!(o.attempts.is_empty(), "{:#?}", o.attempts);
    let m = o.failed.expect("该失败");
    assert_eq!(m.code, "gw.route.sealed_upstream_gone", "{m:?}");
    assert_eq!(b.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn sealed_reasoning_alone_prefers_its_account_and_can_still_fail_over() {
    let (a, b) = (account("甲", false).await, account("乙", false).await);
    let state = tw_gateway::AppState::new(cfg(&[("甲", &a), ("乙", &b)], "池")).unwrap();
    let (gw, mut rx) = serve(state).await;
    let first = ask(gw, &mut rx, body(1, "甲", false)).await;
    let holder = first.attempts.last().unwrap().provider.clone();
    let other = if holder == "甲" { "乙" } else { "甲" };

    // 没有缓存可留，轮流本该轮到另一家；带着封存的推理，回封存它的那一家
    for turn in 2..4 {
        let o = ask(gw, &mut rx, body(turn, &holder, false)).await;
        assert_eq!(served_by(&o), [(holder.as_str(), AttemptOutcome::Served)]);
        assert_eq!(o.stayed, Some(Stay::Sealed));
    }

    // 那一家发不出去：转给另一家，去掉解不开的推理接着回答（见 `crate::seal`）
    let (down, _) = if holder == "甲" { (&a, &b) } else { (&b, &a) };
    down.down.store(true, Ordering::SeqCst);
    let o = ask(gw, &mut rx, body(4, &holder, false)).await;
    assert_eq!(o.status, 200, "{:?}", o.failed);
    assert_eq!(o.attempts.last().unwrap().provider, other);
}

#[tokio::test]
async fn an_upstream_that_cannot_read_the_history_says_so_plainly() {
    // 认不出是谁封存的（core 重启过、记录超期了）：照规则送去乙，乙解不开
    for in_stream in [false, true] {
        let b = account("乙", in_stream).await;
        let state = tw_gateway::AppState::new(cfg(&[("乙", &b)], "乙")).unwrap();
        let (gw, mut rx) = serve(state).await;
        let mut sent = body(2, "甲", true);
        if in_stream {
            sent = sent.replace(r#""store":false"#, r#""store":false,"stream":true"#);
        }
        let o = ask(gw, &mut rx, sent).await;
        let m = o.failed.expect("该失败");
        assert_eq!(m.code, "gw.upstream.sealed_elsewhere", "{in_stream}: {m:?}");
        assert_eq!(m.args.get("upstream").map(String::as_str), Some("乙"));
    }
}
