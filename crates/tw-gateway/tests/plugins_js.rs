//! 真的 JavaScript 插件，从配置一路跑到假上游再回到客户端。
//!
//! 别的插件测试用替身证明接线；这里证明**真的运行时接在了那条线上**：插件文件按配置
//! 读进来、哈希对上了才编（沙箱是 `tw-plugin`），请求钩子改的请求是上游收到的那一份，
//! 回答钩子改的是客户端收到的那一份，插件看到的密钥是占位符，日志和计数记在插件上。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use bytes::Bytes;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tw_config::{Client, Config, Listen, Plugin, Protocol, Provider};

const USER_KEY: &str = "sk-ant-api03-USERSOWNKEYAAAAAAAAAAAAAA";

const FRIDAY: &str = r#"
export const manifest = {
  name: "Friday",
  api: 1,
  permissions: ["system", "messages", "reply.text", "reply.tool_calls"],
  settings: { day: { type: "string", label: "Day", value: "Saturday" } },
};

export function onRequest(req, ctx) {
  const last = req.messages[req.messages.length - 1];
  console.log("the user said: " + last.parts.map((p) => p.text || "").join(""));
  req.system = (req.system || "") + " Today is " + ctx.settings.day + ".";
  return req;
}

export function onReplyText(text) {
  return text.toUpperCase();
}

export function onToolCall(call) {
  if (call.name === "Bash") return null;
}
"#;

/// 假上游：记下收到的请求体，回一条带一段文字、两个工具调用的流
async fn upstream(seen: Arc<Mutex<Vec<Value>>>) -> SocketAddr {
    let app = Router::new().fallback(axum::routing::post(move |body: Bytes| {
        let seen = seen.clone();
        async move {
            seen.lock()
                .unwrap()
                .push(serde_json::from_slice(&body).unwrap_or(Value::Null));
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from(answer()))
                .unwrap()
        }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

fn ev(v: Value) -> String {
    format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap())
}

fn tool(index: u32, id: &str, name: &str, input: &str) -> String {
    [
        ev(
            json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),
        ),
        ev(
            json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":input}}),
        ),
        ev(json!({"type":"content_block_stop","index":index})),
    ]
    .concat()
}

fn answer() -> String {
    [
        ev(
            json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[],"usage":{"input_tokens":3,"output_tokens":1}}}),
        ),
        ev(
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        ),
        ev(
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello "}}),
        ),
        ev(
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"there"}}),
        ),
        ev(json!({"type":"content_block_stop","index":0})),
        tool(1, "toolu_1", "Bash", r#"{"command":"ls"}"#),
        tool(2, "toolu_2", "Read", r#"{"path":"a.txt"}"#),
        ev(
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}),
        ),
        ev(json!({"type":"message_stop"})),
    ]
    .concat()
}

fn sha256_hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

#[tokio::test]
async fn a_javascript_plugin_rewrites_the_request_and_the_answer() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let up = upstream(seen.clone()).await;

    // 插件文件放在配置旁边，配置里记着批准的那一份的哈希
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("plugins")).unwrap();
    std::fs::write(tmp.path().join("plugins/friday.js"), FRIDAY).unwrap();
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "claude-code".into(),
            key: "tw-testkey".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "anthropic".into(),
            base_url: format!("http://{up}"),
            key: Some("sk-upstream".into()),
            protocol: Some(Protocol::Anthropic),
            ..Default::default()
        }],
        plugins: vec![Plugin {
            id: "friday".into(),
            file: "plugins/friday.js".into(),
            sha256: sha256_hex(FRIDAY.as_bytes()),
            enabled: true,
        }],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    // 控制面拿到配置文件的位置时就是这么告诉网关的：插件这时才读得到文件
    state.set_config_dir(tmp.path().to_path_buf());
    let active = state.runtime().plugins.get("friday").cloned().unwrap();
    assert!(
        active.ready().is_some(),
        "the plugin did not load: {:?}",
        active.broken()
    );
    assert_eq!(active.name, "Friday");

    let addr = tw_gateway::serve(state.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let r = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .header("x-api-key", "tw-testkey")
        .header("content-type", "application/json")
        .body(
            json!({
                "model": "claude-sonnet-4-5", "max_tokens": 64, "stream": true,
                "system": "Be brief.",
                "messages": [{ "role": "user", "content": format!("deploy with {USER_KEY}") }]
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();

    // 请求钩子：上游收到的是改过的那一份，用的是配置里的设置
    let sent = seen.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    let system = sent[0]["system"].to_string();
    assert!(system.contains("Be brief. Today is Saturday."), "{system}");

    // 回答钩子：文字是大写的，Bash 那个调用被丢掉，Read 那个留着、编号接上
    let frames: Vec<Value> = body
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect();
    let text: String = frames
        .iter()
        .filter_map(|v| v["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, "HELLO THERE", "{body}");
    let tools: Vec<(u64, &str)> = frames
        .iter()
        .filter(|v| v["type"] == "content_block_start" && v["content_block"]["type"] == "tool_use")
        .map(|v| {
            (
                v["index"].as_u64().unwrap(),
                v["content_block"]["name"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(tools, [(1, "Read")], "{body}");
    assert!(!body.contains("Bash"), "{body}");

    // 插件看到的是占位符，不是密钥；日志记在这个插件上、挂着请求号
    let logs = active.logs.lines();
    assert_eq!(logs.len(), 1, "{logs:?}");
    assert!(
        logs[0]
            .text
            .starts_with("the user said: deploy with <<TW_SECRET_"),
        "{logs:?}"
    );
    assert!(!logs[0].text.contains(USER_KEY));
    assert!(logs[0].request_id.is_some());
    // 请求一次、回答一次，两次都改了东西
    let stats = active.stats.view();
    assert_eq!((stats.calls, stats.changed, stats.errors), (2, 2, 0));
}

/// 插件抛了错、设的是出错时拒绝：请求不发出去，客户端收到说得清的错误
#[tokio::test]
async fn a_javascript_plugin_that_throws_stops_the_request() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let up = upstream(seen.clone()).await;
    let src = r#"
export const manifest = { name: "Strict", api: 1, permissions: ["messages"] };
export function onRequest(req) {
  throw new Error("no messages today");
}
"#;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("plugins")).unwrap();
    std::fs::write(tmp.path().join("plugins/strict.js"), src).unwrap();
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "claude-code".into(),
            key: "tw-testkey".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "anthropic".into(),
            base_url: format!("http://{up}"),
            key: Some("sk-upstream".into()),
            protocol: Some(Protocol::Anthropic),
            ..Default::default()
        }],
        plugins: vec![Plugin {
            id: "strict".into(),
            file: "plugins/strict.js".into(),
            sha256: sha256_hex(src.as_bytes()),
            enabled: true,
        }],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    state.set_config_dir(tmp.path().to_path_buf());
    let addr = tw_gateway::serve(state.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let r = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .header("x-api-key", "tw-testkey")
        .header("content-type", "application/json")
        .body(
            json!({
                "model": "claude-sonnet-4-5", "max_tokens": 64,
                "messages": [{ "role": "user", "content": "hi" }]
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let v: Value = r.json().await.unwrap();
    let message = v["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("Plugin `Strict` failed") && message.contains("no messages today"),
        "{v}"
    );
    assert!(seen.lock().unwrap().is_empty(), "the request was sent");
    let stats = state.runtime().plugins.get("strict").unwrap().stats.view();
    assert_eq!((stats.calls, stats.errors), (1, 1));
}

/// 插件只改了系统提示：它没碰的数字（`2.0`、超过 2^53 的整数）在 JavaScript 里走了一趟，
/// 上游收到的还是原来的写法 —— 工具定义一个字节都不变，提示缓存才不会失效
#[tokio::test]
async fn numbers_a_plugin_did_not_touch_reach_the_upstream_as_they_were() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let up = upstream(seen.clone()).await;
    let src = r#"
export const manifest = {
  name: "Note", api: 1, permissions: ["system", "messages", "tools", "params"],
};
export function onRequest(req) {
  req.system = (req.system || "") + " Note.";
  return req;
}
"#;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("plugins")).unwrap();
    std::fs::write(tmp.path().join("plugins/note.js"), src).unwrap();
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "claude-code".into(),
            key: "tw-testkey".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "anthropic".into(),
            base_url: format!("http://{up}"),
            key: Some("sk-upstream".into()),
            protocol: Some(Protocol::Anthropic),
            ..Default::default()
        }],
        plugins: vec![Plugin {
            id: "note".into(),
            file: "plugins/note.js".into(),
            sha256: sha256_hex(src.as_bytes()),
            enabled: true,
        }],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    state.set_config_dir(tmp.path().to_path_buf());
    let addr = tw_gateway::serve(state.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let request = json!({
        "model": "claude-sonnet-4-5", "max_tokens": 64, "stream": true, "temperature": 1.0,
        "system": "Be brief.",
        "messages": [
            { "role": "user", "content": "read it" },
            { "role": "assistant", "content": [
                { "type": "tool_use", "id": "toolu_1", "name": "Read",
                  "input": { "path": "a.txt", "limit": 2.0, "seed": 12345678901234567890u64 } }
            ]},
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "toolu_1", "content": "hello" }
            ]}
        ],
        "tools": [
            { "name": "Read", "description": "Read a file",
              "input_schema": { "type": "object", "properties": {
                  "limit": { "type": "number", "minimum": 0.5, "maximum": 2.0 } } } }
        ]
    });
    let r = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .header("x-api-key", "tw-testkey")
        .header("content-type", "application/json")
        .body(request.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let _ = r.text().await.unwrap();
    let sent = seen.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["system"], "Be brief. Note.");
    for k in ["messages", "tools", "temperature"] {
        assert_eq!(sent[0][k].to_string(), request[k].to_string(), "{k}");
    }
}

/// 声明了嵌入和补全的插件，在真的沙箱里：`requests` 读得出来，一项输入一条消息，上游收到
/// 的是改过的那一份，一串 token 原样。只处理对话的那一个（一跑就抛错、出错时拒绝）**不跑
/// 在这些请求上**，也拦不着它们
#[tokio::test]
async fn a_javascript_plugin_that_declares_embeddings_rewrites_their_inputs() {
    const SCRUB: &str = r#"
export const manifest = {
  name: "Scrub", api: 1, permissions: ["messages"],
  requests: ["conversation", "embeddings", "completions"],
};
export function onRequest(req, ctx) {
  console.log(ctx.format + " " + req.messages.length + " " + req.format);
  for (const m of req.messages) {
    for (const p of m.parts) {
      if (p.type === "text") p.text = p.text.replaceAll("PROJECT-X", "[removed]");
    }
  }
  return req;
}
"#;
    const STRICT: &str = r#"
export const manifest = { name: "Strict", api: 1, permissions: ["messages"] };
export function onRequest(req) {
  throw new Error("conversations only");
}
"#;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let up = upstream(seen.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("plugins")).unwrap();
    std::fs::write(tmp.path().join("plugins/scrub.js"), SCRUB).unwrap();
    std::fs::write(tmp.path().join("plugins/strict.js"), STRICT).unwrap();
    let plugin = |id: &str, src: &str| Plugin {
        id: id.into(),
        file: format!("plugins/{id}.js"),
        sha256: sha256_hex(src.as_bytes()),
        enabled: true,
    };
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "claude-code".into(),
            key: "tw-testkey".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "openai".into(),
            base_url: format!("http://{up}"),
            key: Some("sk-upstream".into()),
            protocol: Some(Protocol::OpenaiChat),
            ..Default::default()
        }],
        plugins: vec![plugin("scrub", SCRUB), plugin("strict", STRICT)],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    state.set_config_dir(tmp.path().to_path_buf());
    let rt = state.runtime();
    let scrub = rt.plugins.get("scrub").cloned().unwrap();
    assert!(scrub.ready().is_some(), "{:?}", scrub.broken());
    assert_eq!(
        scrub.requests,
        [
            tw_api::RequestKind::Conversation,
            tw_api::RequestKind::Embeddings,
            tw_api::RequestKind::Completions
        ]
    );
    let strict = rt.plugins.get("strict").cloned().unwrap();
    assert_eq!(strict.requests, [tw_api::RequestKind::Conversation]);
    let addr = tw_gateway::serve(state.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let post = |path: &'static str, body: Value| async move {
        reqwest::Client::new()
            .post(format!("http://{addr}{path}"))
            .header("authorization", "Bearer tw-testkey")
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap()
            .status()
    };
    let status = post(
        "/v1/embeddings",
        json!({ "model": "text-embedding-3-small", "input": ["PROJECT-X plan", [101, 102]] }),
    )
    .await;
    assert_eq!(status, 200);
    let status = post(
        "/v1/completions",
        json!({ "model": "gpt-3.5-turbo-instruct", "prompt": "Summarize PROJECT-X", "max_tokens": 8 }),
    )
    .await;
    assert_eq!(status, 200);
    let sent = seen.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0]["input"], json!(["[removed] plan", [101, 102]]));
    assert_eq!(sent[1]["prompt"], "Summarize [removed]");
    assert_eq!(sent[1]["max_tokens"], 8);
    let logs: Vec<String> = scrub.logs.lines().into_iter().map(|l| l.text).collect();
    assert_eq!(
        logs,
        [
            "openai_embeddings 2 openai_embeddings",
            "openai_completions 1 openai_completions"
        ]
    );
    let st = scrub.stats.view();
    assert_eq!((st.calls, st.changed, st.errors), (2, 2, 0));
    // 只处理对话的那一个一次都没跑
    assert_eq!(strict.stats.view(), tw_api::PluginStats::default());
}
