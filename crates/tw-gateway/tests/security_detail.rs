//! 安全记录的细节跟着事件走，**路径是客户端发来的那一份里的**：四种客户端格式各发一个请求
//! （上游是 Anthropic，网关转换过格式再发），出站脱敏和内容过滤报出去的每一处都指回客户端
//! 那一份的位置，说得出是哪个工具的结果。

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::routing::post;
use serde_json::{Value, json};
use tw_api::{Event, HitLocation, HitPart};
use tw_config::{
    Client, Config, ContentPolicy, CustomContentRule, Listen, Provider, RedactPolicy, Security,
    SecurityMode,
};

const GATEWAY_KEY: &str = "tw-reh4xqqrzyvbutjacvjywb4e";
const KEY: &str = "sk-ant-api03-USERSOWNKEYAAAAAAAAAAAAAA";

/// 假上游：Anthropic 的 `/v1/messages`，回 `ok`
async fn upstream() -> SocketAddr {
    let app = Router::new().route(
        "/v1/messages",
        post(|| async {
            axum::Json(json!({
                "id": "msg", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5",
                "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }))
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

fn config(up: SocketAddr) -> Config {
    Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "claude-code".into(),
            key: GATEWAY_KEY.into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "relay".into(),
            base_url: format!("http://{up}"),
            key: Some("sk-upstream".into()),
            protocol: Some(tw_config::Protocol::Anthropic),
            ..Default::default()
        }],
        security: Security {
            redact: RedactPolicy {
                mode: SecurityMode::Observe,
                ..Default::default()
            },
            content: ContentPolicy {
                mode: SecurityMode::Observe,
                custom: vec![CustomContentRule {
                    name: "falcon".into(),
                    pattern: "falcon".into(),
                    matching: Default::default(),
                    action: tw_config::ContentAction::Record,
                    disabled: false,
                }],
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

/// 发一个请求，交回它的事件，直到它的结局
async fn send(up: SocketAddr, path: &str, body: Value) -> Vec<Event> {
    let state = tw_gateway::AppState::new(config(up)).unwrap();
    let mut rx = state.bus.subscribe();
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{addr}{path}"))
        .header("x-api-key", GATEWAY_KEY)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{path}: {}", r.text().await.unwrap());
    let mut out = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
        let end = matches!(
            ev,
            Event::RequestFinished { .. } | Event::RequestFailed { .. }
        );
        out.push(ev);
        if end {
            break;
        }
    }
    out
}

fn secret_locations(evs: &[Event]) -> Vec<HitLocation> {
    evs.iter()
        .find_map(|e| match e {
            Event::SecretsFound { items, .. } => Some(items[0].detail.locations.clone()),
            _ => None,
        })
        .expect("没有 SecretsFound")
}

fn content_locations(evs: &[Event]) -> Vec<HitLocation> {
    evs.iter()
        .find_map(|e| match e {
            Event::ContentMatched { detail, .. } => Some(detail.locations.clone()),
            _ => None,
        })
        .expect("没有 ContentMatched")
}

#[tokio::test]
async fn locations_point_into_the_body_each_client_sent() {
    let up = upstream().await;
    let result = format!("page with {KEY} and a falcon");
    let cases = [
        (
            "/v1/messages",
            json!({"model": "claude-sonnet-4-5", "max_tokens": 16, "messages": [
                {"role": "user", "content": "read it"},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "fetch", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": result}]}
            ]}),
            "messages[2].content[0].content",
            Some("user"),
        ),
        (
            "/v1/chat/completions",
            json!({"model": "claude-sonnet-4-5", "messages": [
                {"role": "user", "content": "read it"},
                {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function",
                    "function": {"name": "fetch", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": result}
            ]}),
            "messages[2].content",
            Some("tool"),
        ),
        (
            "/v1/responses",
            json!({"model": "claude-sonnet-4-5", "input": [
                {"role": "user", "content": "read it"},
                {"type": "function_call", "call_id": "c1", "name": "fetch", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": result}
            ]}),
            "input[2].output",
            None,
        ),
        (
            "/v1beta/models/claude-sonnet-4-5:generateContent",
            json!({"contents": [
                {"role": "user", "parts": [{"text": "read it"}]},
                {"role": "model", "parts": [{"functionCall": {"name": "fetch", "args": {}}}]},
                {"role": "user", "parts": [{"functionResponse": {"name": "fetch",
                    "response": {"output": result}}}]}
            ]}),
            "contents[2].parts[0].functionResponse.response.output",
            Some("user"),
        ),
    ];
    for (path, body, at, role) in cases {
        let evs = send(up, path, body).await;
        for (what, ls) in [
            ("redact", secret_locations(&evs)),
            ("content", content_locations(&evs)),
        ] {
            let [l] = &ls[..] else {
                panic!("{path} {what}: {ls:#?}")
            };
            assert_eq!(l.path, at, "{path} {what}");
            assert_eq!(l.part, HitPart::ToolResult, "{path} {what}");
            assert_eq!(l.message_index, Some(2), "{path} {what}");
            assert_eq!(l.role.as_deref(), role, "{path} {what}");
            assert_eq!(l.tool.as_deref(), Some("fetch"), "{path} {what}");
            let all = serde_json::to_string(l).unwrap();
            assert!(
                !all.contains(&KEY[5..KEY.len() - 4]),
                "{path} {what}: {all}"
            );
        }
    }
}
