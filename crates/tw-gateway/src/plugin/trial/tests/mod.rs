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

/// 存下来的一个请求：路径和请求体，没记客户端、没记发出去的模型名
fn stored<'a>(path: &'a str, body: &'a [u8]) -> StoredRequest<'a> {
    StoredRequest {
        path,
        query: None,
        body,
        client: None,
        upstream: "up",
        sent_model: "",
    }
}

/// 试跑数 token、嵌入这些请求，和它们当时一样看：Gemini 包着的数 token 改里面那一份、
/// 只写回模型名；插件没声明的那种请求、插件不管的接口，说清当时它就没跑
#[tokio::test]
async fn a_trial_reads_counting_and_unhandled_requests_as_they_were_read() {
    let tune = || {
        Double::new("tune")
            .permit(&[Permission::System, Permission::Params])
            .on_request(|mut view, _| {
                view["system"] = json!("Be brief.");
                view["params"]["max_tokens"] = json!(99);
                Invocation::ok(RequestOutcome::Changed(view))
            })
            .into_host()
    };
    let wrapped = json!({ "generateContentRequest": { "model": "models/gemini-2.5-pro",
        "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }] } })
    .to_string();
    let t = run(
        Arc::new(Pool::new(1, 4)),
        tune(),
        &Default::default(),
        rules(),
        Some(stored(
            "/v1beta/models/gemini-2.5-pro:countTokens",
            wrapped.as_bytes(),
        )),
        None,
    )
    .await;
    assert_eq!(t.error, None);
    let req = t.request.unwrap();
    assert_eq!(req.outcome, Outcome::Changed);
    let after: Value = serde_json::from_str(&req.after).unwrap();
    let inner = &after["generateContentRequest"];
    assert_eq!(inner["systemInstruction"]["parts"][0]["text"], "Be brief.");
    assert!(inner.get("generationConfig").is_none(), "{after}");

    // 只处理对话的插件当时不在嵌入请求的范围里
    let embeddings = br#"{"model":"text-embedding-3-small","input":["hi"]}"#;
    let t = run(
        Arc::new(Pool::new(1, 4)),
        tune(),
        &Default::default(),
        rules(),
        Some(stored("/v1/embeddings", embeddings)),
        None,
    )
    .await;
    assert!(t.request.is_none());
    assert_eq!(
        t.error.map(|m| m.code).as_deref(),
        Some("gw.plugin.not_declared")
    );
    // 插件一律不管的接口
    let t = run(
        Arc::new(Pool::new(1, 4)),
        tune(),
        &Default::default(),
        rules(),
        Some(stored("/v1/images/generations", br#"{"prompt":"a cat"}"#)),
        None,
    )
    .await;
    assert!(t.request.is_none());
    let e = t.error.unwrap();
    assert_eq!(
        (e.code.as_str(), e.arg("path")),
        ("gw.plugin.not_applicable", "/v1/images/generations")
    );
}

/// 去掉记号的插件，声明了嵌入和补全
fn scrubbing() -> Arc<dyn PluginHost> {
    Double::new("scrub")
        .permit(&[Permission::Messages])
        .requests(&[
            tw_api::RequestKind::Embeddings,
            tw_api::RequestKind::Completions,
        ])
        .on_request(|mut view, ctx| {
            assert_ne!(ctx["format"], "anthropic");
            for m in view["messages"].as_array_mut().unwrap() {
                for p in m["parts"].as_array_mut().unwrap() {
                    if let Some(t) = p["text"].as_str() {
                        p["text"] = json!(t.replace("CLASSIFIED", "[removed]"));
                    }
                }
            }
            Invocation::ok(RequestOutcome::Changed(view))
        })
        .into_host()
}

/// 试跑存下来的嵌入、补全请求：和当时一样，一项输入一条消息；前后两份打过码；`ctx.format`
/// 说得出是哪一种
#[tokio::test]
async fn a_trial_runs_on_stored_embeddings_and_completions_requests() {
    let cases: [(&str, Value, &str, &str); 3] = [
        (
            "/v1/embeddings",
            json!({ "model": "text-embedding-3-small",
                    "input": ["the CLASSIFIED plan", format!("key {KEY}"), [1, 2]] }),
            "/input/0",
            "the [removed] plan",
        ),
        (
            "/v1/completions",
            json!({ "model": "gpt-3.5-turbo-instruct", "prompt": format!("Say hi to CLASSIFIED {KEY}"),
                    "max_tokens": 5 }),
            "/prompt",
            "Say hi to [removed] <<TW_SECRET_1>>",
        ),
        (
            "/v1beta/models/gemini-embedding-001:batchEmbedContents",
            json!({ "requests": [{ "model": "models/gemini-embedding-001",
                                   "content": { "parts": [{ "text": format!("the CLASSIFIED plan {KEY}") }] } }] }),
            "/requests/0/content/parts/0/text",
            "the [removed] plan <<TW_SECRET_1>>",
        ),
    ];
    for (path, body, at, want) in cases {
        let bytes = body.to_string().into_bytes();
        let t = run(
            Arc::new(Pool::new(1, 4)),
            scrubbing(),
            &Default::default(),
            rules(),
            Some(stored(path, &bytes)),
            // 回答钩子不在嵌入、补全上跑：存着的回答试跑也不看
            Some(StoredReply {
                body: br#"{"object":"list","data":[]}"#,
                upstream: Dialect::Chat,
                provider: "up",
            }),
        )
        .await;
        assert_eq!(t.error, None, "{path}");
        assert!(t.reply.is_none(), "{path}");
        let req = t.request.unwrap();
        assert_eq!(req.outcome, Outcome::Changed, "{path}");
        let after: Value = serde_json::from_str(&req.after).unwrap();
        assert_eq!(after.pointer(at).unwrap(), want, "{path}");
        assert!(
            !req.before.contains(KEY) && !req.after.contains(KEY),
            "{path}"
        );
    }
}
