//! M5 验收：出站脱敏与回显还原（DESIGN.md §5.1）。
//!
//! 验收标准的原话是：**走中转时密钥被替换成占位符且回显能还原，走官方时
//! 原样透传。**这个文件就在证明那一句。
//!
//! 单元测试证明不了它，因为它的失败模式全在接缝上：脱敏发生在故障转移
//! 循环里面（换了 provider 就换一套规格），还原发生在流上（占位符会被切在
//! 两个 chunk 中间），而两者中间隔着整条管线。

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::routing::post;
use tw_config::{Client, Config, Listen, Provider, Security, SecurityMode};

/// 用户粘进对话里的那把 key。
const USER_KEY: &str = "sk-ant-api03-USERSOWNKEYAAAAAAAAAAAAAA";

/// 假上游：把收到的 body 存下来，然后**把它原样回显**。
///
/// 回显是刻意的 —— 模型确实会重复你给它的东西（「你说的这个
/// `sk-ant-…` 是……」），而那正是还原要处理的情况。
async fn start_upstream(sse: bool) -> (SocketAddr, Arc<Mutex<Vec<u8>>>) {
    let seen: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    let app = Router::new()
        .route(
            "/v1/messages",
            post(
                move |State(s): State<Arc<Mutex<Vec<u8>>>>, body: bytes::Bytes| async move {
                    *s.lock().unwrap() = body.to_vec();
                    let text = String::from_utf8_lossy(&body).to_string();
                    // 把请求里出现的东西挑出来放进回答里
                    let echoed = text
                        .split('"')
                        .find(|p| p.contains("<<TW_SECRET_") || p.contains("sk-ant-"))
                        .unwrap_or("（没看到）")
                        .to_string();
                    if sse {
                        // **一个字符一帧。**占位符必然被切碎 —— 而那正是
                        // 流式还原存在的全部理由
                        let mut out = String::from(
                            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
                        );
                        for c in echoed.chars() {
                            out.push_str(&format!(
                                "event: content_block_delta\ndata: {{\"delta\":{{\"text\":{}}}}}\n\n",
                                serde_json::to_string(&c.to_string()).unwrap()
                            ));
                        }
                        out.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
                        axum::response::Response::builder()
                            .header("content-type", "text/event-stream")
                            .body(axum::body::Body::from(out))
                            .unwrap()
                    } else {
                        axum::response::Response::builder()
                            .header("content-type", "application/json")
                            .body(axum::body::Body::from(format!(
                                "{{\"type\":\"message\",\"text\":{}}}",
                                serde_json::to_string(&echoed).unwrap()
                            )))
                            .unwrap()
                    }
                },
            ),
        )
        .with_state(s);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, seen)
}

async fn start_gateway(providers: Vec<Provider>, mode: SecurityMode) -> SocketAddr {
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "claude-code".into(),
            key: "tw-testkey".into(),
            ..Default::default()
        }],
        providers,
        security: Security {
            redact: mode,
            ..Default::default()
        },
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let addr = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, addr).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

fn provider(name: &str, base: SocketAddr, official: bool) -> Provider {
    Provider {
        name: name.into(),
        base_url: format!("http://{base}"),
        key: "sk-upstream".into(),
        protocol: Some(tw_config::Protocol::Anthropic),
        // 测试里连的是 127.0.0.1，域名判据用不上，所以直接写清楚
        redact: Some(if official {
            vec![]
        } else {
            vec![tw_redact::rules::Kind::ApiKeys]
        }),
        ..Default::default()
    }
}

fn body_with_key() -> String {
    format!(
        r#"{{"model":"claude-sonnet-4-5","max_tokens":64,"messages":[{{"role":"user","content":"我的 key 是 {USER_KEY}，帮我看看"}}]}}"#
    )
}

async fn ask(gw: SocketAddr, body: &str, stream: bool) -> String {
    let b = if stream {
        body.replace("\"max_tokens\":64", "\"max_tokens\":64,\"stream\":true")
    } else {
        body.to_string()
    };
    reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-testkey")
        .header("content-type", "application/json")
        .body(b)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_relay_never_sees_the_key_but_the_client_gets_it_back() {
    // **验收标准的前半句。**
    let (up, seen) = start_upstream(false).await;
    let gw = start_gateway(vec![provider("relay", up, false)], SecurityMode::Enforce).await;

    let got = ask(gw, &body_with_key(), false).await;

    // 上游没看见真 key
    let sent = String::from_utf8(seen.lock().unwrap().clone()).unwrap();
    assert!(!sent.contains(USER_KEY), "中转站看见了真 key：{sent}");
    assert!(sent.contains("<<TW_SECRET_1>>"), "没换成占位符：{sent}");
    // 而客户端拿回来的是原值
    assert!(got.contains(USER_KEY), "回显没还原：{got}");
    assert!(!got.contains("<<TW_SECRET_"), "占位符漏给客户端了：{got}");
}

#[tokio::test]
async fn the_official_endpoint_gets_the_body_byte_for_byte() {
    // **验收标准的后半句。**你让 Claude Code 调试一个 .env 问题，
    // 它得真看见里面的值才帮得上忙（§5.1）。
    let (up, seen) = start_upstream(false).await;
    let gw = start_gateway(vec![provider("official", up, true)], SecurityMode::Enforce).await;

    let body = body_with_key();
    ask(gw, &body, false).await;

    let sent = String::from_utf8(seen.lock().unwrap().clone()).unwrap();
    assert_eq!(sent, body, "官方端点上 body 被动过了");
}

#[tokio::test]
async fn a_placeholder_cut_into_single_characters_still_comes_back_whole() {
    // 假上游一个字符一帧地发 —— 占位符必然被切碎，而那正是流式还原
    // 存在的全部理由。
    let (up, seen) = start_upstream(true).await;
    let gw = start_gateway(vec![provider("relay", up, false)], SecurityMode::Enforce).await;

    let got = ask(gw, &body_with_key(), true).await;

    let sent = String::from_utf8(seen.lock().unwrap().clone()).unwrap();
    assert!(!sent.contains(USER_KEY), "{sent}");
    // 一帧一个字符地拼回来，最后必须是完整的原值
    let joined: String = got
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<serde_json::Value>(d).ok())
        .filter_map(|v| v["delta"]["text"].as_str().map(|s| s.to_string()))
        .collect();
    // 假上游回显的是整句话，所以这里比的是「原值回来了、占位符没漏」
    assert!(joined.contains(USER_KEY), "流式还原没拼回来：{joined}");
    assert!(!joined.contains("<<TW_SECRET_"), "{joined}");
    assert!(!got.contains("<<TW_SECRET_"), "占位符漏给客户端了：{got}");
}

#[tokio::test]
async fn observe_mode_changes_nothing_on_the_wire() {
    // §5.0：观察态**只记录，不改变任何行为**。出厂默认停在这里。
    let (up, seen) = start_upstream(false).await;
    let gw = start_gateway(vec![provider("relay", up, false)], SecurityMode::Observe).await;

    let body = body_with_key();
    let got = ask(gw, &body, false).await;

    assert_eq!(
        String::from_utf8(seen.lock().unwrap().clone()).unwrap(),
        body,
        "观察态动了 body"
    );
    assert!(got.contains(USER_KEY));
}

#[tokio::test]
async fn failing_over_from_official_to_a_relay_redacts_on_the_second_hop() {
    // **这是脱敏为什么必须住在故障转移循环里面。**从官方切到中转的那一
    // 刻，正是最需要它的时刻 —— 而循环外面算一次的话，第二跳会拿着为
    // 官方算出来的规格（也就是「不脱」）把 key 发出去。
    let dead = {
        // 一个立刻 500 的上游
        let app = Router::new().route(
            "/v1/messages",
            post(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        a
    };
    let (up, seen) = start_upstream(false).await;
    let gw = start_gateway(
        vec![
            provider("official", dead, true),
            provider("relay", up, false),
        ],
        SecurityMode::Enforce,
    )
    .await;

    let got = ask(gw, &body_with_key(), false).await;

    let sent = String::from_utf8(seen.lock().unwrap().clone()).unwrap();
    assert!(
        !sent.contains(USER_KEY),
        "转移到中转之后仍然把真 key 发过去了：{sent}"
    );
    assert!(sent.contains("<<TW_SECRET_1>>"), "{sent}");
    assert!(got.contains(USER_KEY), "还原没跟上故障转移：{got}");
}

#[tokio::test]
async fn the_ui_is_told_what_was_replaced_without_being_told_the_value() {
    // **界面上必须能看到脱敏发生了什么**，否则用户会怀疑是脱敏搞坏了
    // 功能然后把它关掉（§5.1）。但事件里不能带原值。
    let (up, _) = start_upstream(false).await;
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "claude-code".into(),
            key: "tw-testkey".into(),
            ..Default::default()
        }],
        providers: vec![provider("relay", up, false)],
        security: Security {
            redact: SecurityMode::Enforce,
            ..Default::default()
        },
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let mut rx = state.bus.subscribe();
    let addr = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    let s2 = state.clone();
    tokio::spawn(async move { tw_gateway::serve(s2, addr).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;

    ask(addr, &body_with_key(), false).await;

    let mut found = None;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
        if let tw_api::Event::Redacted {
            items, provider, ..
        } = ev
        {
            found = Some((items, provider));
            break;
        }
    }
    let (items, provider) = found.expect("没发脱敏事件");
    assert_eq!(provider, "relay");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, "api-keys");
    assert_eq!(items[0].count, 1);
    let dump = format!("{items:?}");
    assert!(!dump.contains("USERSOWNKEY"), "事件里带出了原值：{dump}");
}
