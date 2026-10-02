//! 试跑：对着存下来的请求和回答跑一遍，前后两份都换过占位符，什么都不留。

use std::sync::Arc;

use serde_json::{Value, json};
use tw_dialect::ir::Dialect;

use super::*;
use crate::plugin::host::Invocation;
use crate::plugin::host::double::Double;
use tw_api::{Permission, PluginLogLevel};

const KEY: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";

fn rules() -> Arc<tw_guard::redact::rules::RuleSet> {
    Arc::new(tw_guard::redact::rules::RuleSet::defaults())
}

fn request() -> Vec<u8> {
    json!({
        "model": "claude-sonnet-4-5", "max_tokens": 64, "stream": true,
        "system": "You are helpful.",
        "messages": [{ "role": "user", "content": format!("my key is {KEY}") }]
    })
    .to_string()
    .into_bytes()
}

fn sse(chunks: &[Value]) -> Vec<u8> {
    chunks
        .iter()
        .map(|c| format!("event: {}\ndata: {c}\n\n", c["type"].as_str().unwrap()))
        .collect::<String>()
        .into_bytes()
}

fn anthropic_answer() -> Vec<u8> {
    sse(&[
        json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","model":"m","content":[],"usage":{"input_tokens":1,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":format!("your key {KEY} works")}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
        json!({"type":"message_stop"}),
    ])
}

fn both() -> Arc<dyn PluginHost> {
    Double::new("both")
        .permit(&[Permission::System, Permission::ReplyText])
        .on_request(|mut view, _| {
            view["system"] = json!("You are helpful. Today is Friday.");
            let mut inv = Invocation::ok(RequestOutcome::Changed(view));
            inv.logs.push(LogLine {
                level: PluginLogLevel::Info,
                text: "added the date".into(),
            });
            inv
        })
        .on_text(|t| Some(t.to_uppercase()))
        .into_host()
}

#[tokio::test]
async fn a_trial_shows_both_sides_masked_and_leaves_no_trace() {
    let pool = Arc::new(Pool::new(1, 4));
    let body = request();
    let answer = anthropic_answer();
    let t = run(
        pool,
        both(),
        &Default::default(),
        rules(),
        Some(StoredRequest {
            path: "/v1/messages",
            query: None,
            body: &body,
            client: Some("claude-code"),
            upstream: "anthropic",
            sent_model: "claude-sonnet-4-5",
        }),
        Some(StoredReply {
            body: &answer,
            upstream: Dialect::Anthropic,
            provider: "anthropic",
        }),
    )
    .await;
    assert_eq!(t.error, None);
    let req = t.request.unwrap();
    assert_eq!(req.outcome, Outcome::Changed);
    assert!(req.after.contains("Today is Friday."), "{}", req.after);
    for s in [&req.before, &req.after] {
        assert!(!s.contains(KEY), "{s}");
        assert!(s.contains("<<TW_SECRET_1>>"), "{s}");
    }
    let rep = t.reply.unwrap();
    assert_eq!(rep.outcome, Outcome::Changed);
    assert!(
        rep.after.contains("YOUR KEY <<TW_SECRET_1>> WORKS"),
        "{}",
        rep.after
    );
    assert!(!rep.before.contains(KEY) && !rep.after.contains(KEY));
    // 日志交给调用方，按钩子分好
    assert_eq!(t.logs.len(), 1);
    assert_eq!(t.logs[0].0, Hook::Request);
    assert_eq!(t.logs[0].1.text, "added the date");
}

#[tokio::test]
async fn an_answer_from_another_format_is_read_in_the_clients_format() {
    let pool = Arc::new(Pool::new(1, 4));
    let body = request();
    let chat = [
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hel\"}}]}\n\n",
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    ]
    .concat();
    let upper = Double::new("upper")
        .permit(&[Permission::ReplyText])
        .on_text(|t| Some(t.to_uppercase()))
        .into_host();
    let t = run(
        pool,
        upper,
        &Default::default(),
        rules(),
        Some(StoredRequest {
            path: "/v1/messages",
            query: None,
            body: &body,
            client: None,
            upstream: "anthropic",
            sent_model: "claude-sonnet-4-5",
        }),
        Some(StoredReply {
            body: chat.as_bytes(),
            upstream: Dialect::Chat,
            provider: "deepseek",
        }),
    )
    .await;
    assert_eq!(t.error, None);
    assert!(t.request.is_none(), "the plugin has no request hook");
    let rep = t.reply.unwrap();
    let after: Value = serde_json::from_str(&rep.after).unwrap();
    // Anthropic 客户端看到的整包
    assert_eq!(after["content"][0]["text"], "HELLO");
}

#[tokio::test]
async fn a_rejection_is_reported_without_an_after() {
    let pool = Arc::new(Pool::new(1, 4));
    let body = request();
    let no = Double::new("no")
        .permit(&[Permission::Messages])
        .on_request(|_, _| Invocation::ok(RequestOutcome::Rejected("not today".into())))
        .into_host();
    let t = run(
        pool,
        no,
        &Default::default(),
        rules(),
        Some(StoredRequest {
            path: "/v1/messages",
            query: None,
            body: &body,
            client: None,
            upstream: "anthropic",
            sent_model: "claude-sonnet-4-5",
        }),
        None,
    )
    .await;
    let req = t.request.unwrap();
    assert_eq!(req.outcome, Outcome::Rejected);
    assert_eq!(req.before, req.after);
    let e = t.error.unwrap();
    assert_eq!(e.code, "gw.plugin.rejected");
    assert!(e.text.contains("not today"));
}

/// `ctx` 按那一行记下的路由给：回答它的那一家、发给它的模型名、客户端要的模型 —— 视图里
/// 的模型名和 `ctx.model` 是同一个
#[tokio::test]
async fn a_trial_gives_the_plugin_the_routing_the_request_had() {
    let pool = Arc::new(Pool::new(1, 4));
    let body = request();
    let saw = Arc::new(std::sync::Mutex::new(Value::Null));
    let s = saw.clone();
    let look = Double::new("look")
        .permit(&[Permission::Params])
        .on_request(move |view, ctx| {
            *s.lock().unwrap() = json!({ "view": view, "ctx": ctx });
            Invocation::ok(RequestOutcome::Unchanged)
        })
        .into_host();
    let t = run(
        pool,
        look,
        &Default::default(),
        rules(),
        Some(StoredRequest {
            path: "/v1/messages",
            query: None,
            body: &body,
            client: Some("claude-code"),
            upstream: "relay",
            sent_model: "glm-4.6",
        }),
        None,
    )
    .await;
    assert_eq!(t.error, None);
    let saw = saw.lock().unwrap().clone();
    assert_eq!(saw["ctx"]["upstream"], "relay");
    assert_eq!(saw["ctx"]["model"], "glm-4.6");
    assert_eq!(saw["ctx"]["requested_model"], "claude-sonnet-4-5");
    assert_eq!(saw["view"]["model"], "glm-4.6");
    assert_eq!(saw["view"]["params"]["model"], "glm-4.6");
}
