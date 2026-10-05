//! 回答里的模型名写成客户端用的名称（见 `tw_gateway::answer_model`），端到端。
//!
//! 发出去的名称 S 和客户端要的 N 不一样（这里用规则的 `set.model` 造出来），上游答的
//! A 和 S 是同一个模型时，客户端看到的是 N：四种格式、整包和流、直通和转换，加上
//! `openai-model` / `x-openai-model` 两个头。A 是别的模型、S 和 N 一样时一个字节都不动。
//! 请求记录里的模型名照旧是上游的原话 A。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{OriginalUri, State};
use serde_json::Value;
use serde_json::json;
use tokio::sync::broadcast::Receiver;
use tw_api::{Event, Permission};
use tw_config::{Client, Config, Protocol, Provider};
use tw_gateway::plugin::host::double::{self, Double};
use tw_gateway::plugin::{Invocation, PluginSet, RequestOutcome};

/// 一个假上游的回答
#[derive(Clone)]
struct Reply {
    status: u16,
    content_type: &'static str,
    headers: Vec<(&'static str, &'static str)>,
    body: String,
}

fn ok(content_type: &'static str, body: impl Into<String>) -> Reply {
    Reply {
        status: 200,
        content_type,
        headers: Vec::new(),
        body: body.into(),
    }
}

const JSON: &str = "application/json";
const SSE: &str = "text/event-stream";

/// 收到的生成请求：路径和正文。**只记 POST**：网关会在后台来取模型清单
type Seen = Arc<Mutex<Vec<(String, Value)>>>;

async fn upstream(reply: Reply) -> (SocketAddr, Seen) {
    let seen: Seen = Arc::default();
    let app = Router::new()
        .fallback(
            move |State(s): State<Seen>,
                  method: axum::http::Method,
                  OriginalUri(uri): OriginalUri,
                  body: bytes::Bytes| {
                let reply = reply.clone();
                async move {
                    if method == axum::http::Method::POST {
                        let v = serde_json::from_slice(&body).unwrap_or(Value::Null);
                        s.lock().unwrap().push((uri.to_string(), v));
                    }
                    let mut b = axum::response::Response::builder()
                        .status(reply.status)
                        .header("content-type", reply.content_type);
                    for (k, v) in &reply.headers {
                        b = b.header(*k, *v);
                    }
                    b.body(axum::body::Body::from(reply.body)).unwrap()
                }
            },
        )
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, seen)
}

fn provider(name: &str, up: SocketAddr, protocol: Protocol) -> Provider {
    Provider {
        name: name.into(),
        base_url: format!("http://{up}"),
        key: Some("sk-x".into()),
        protocol: Some(protocol),
        ..Default::default()
    }
}

/// 默认路由：客户端要 `client` 时发 `sent`，都去 `to`。`sent` 为空就不改写
fn routes(client: &str, sent: &str, to: &str) -> String {
    let rename = if sent.is_empty() {
        String::new()
    } else {
        format!(
            r#"
    - name: 改名
      when: {{ model: "{client}" }}
      set: {{ model: "{sent}" }}"#
        )
    };
    format!(
        r#"
- name: default
  rules:{rename}
    - name: 兜底
      to: {to}
"#
    )
}

async fn serve(
    providers: Vec<Provider>,
    groups: Vec<tw_engine::Group>,
    routes: &str,
) -> (SocketAddr, Receiver<Event>) {
    serve_with(providers, groups, routes, Vec::new()).await
}

/// 装着插件 `plugins` 的网关
async fn serve_with(
    providers: Vec<Provider>,
    groups: Vec<tw_engine::Group>,
    routes: &str,
    plugins: Vec<Double>,
) -> (SocketAddr, Receiver<Event>) {
    let cfg = Config {
        clients: vec![Client {
            name: "me".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers,
        groups,
        // 规则从 YAML 读：和用户写在 config.yaml 里的是同一个形状
        routes: serde_yaml_ng::from_str(routes).unwrap(),
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    state.swap_plugins(PluginSet::new(
        plugins
            .into_iter()
            .enumerate()
            .map(|(i, d)| Arc::new(double::active(&format!("p{i}"), d)))
            .collect(),
    ));
    let events = state.bus.subscribe();
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, events)
}

/// 客户端收到的
struct Got {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: String,
    /// 结局里记的上游原话
    answered: Option<String>,
}

async fn ask(gw: SocketAddr, rx: &mut Receiver<Event>, path: &str, body: &str) -> Got {
    let r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{gw}{path}"))
        .header("x-api-key", "tw-k")
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let headers = r.headers().clone();
    let body = r.text().await.unwrap();
    let answered = loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("5 秒内没等到结局")
            .expect("事件流断了");
        match ev {
            Event::RequestFinished { answered_model, .. }
            | Event::RequestFailed { answered_model, .. }
            | Event::RequestCancelled { answered_model, .. } => break answered_model,
            _ => {}
        }
    };
    Got {
        status,
        headers,
        body,
        answered,
    }
}

/// 回答里所有写着模型名的地方，按出现的顺序：整包的、流里每一帧的（SSE 或 JSON 数组）
fn models(body: &str) -> Vec<String> {
    fn of(v: &Value, out: &mut Vec<String>) {
        for m in [
            &v["model"],
            &v["message"]["model"],
            &v["response"]["model"],
            &v["modelVersion"],
        ] {
            if let Some(s) = m.as_str() {
                out.push(s.to_string());
            }
        }
    }
    let mut out = Vec::new();
    let t = body.trim_start();
    if t.starts_with('{') || t.starts_with('[') {
        match serde_json::from_str::<Value>(t).expect("不是 JSON") {
            Value::Array(items) => items.iter().for_each(|v| of(v, &mut out)),
            v => of(&v, &mut out),
        }
    } else {
        for line in body.lines() {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                of(&v, &mut out);
            }
        }
    }
    out
}

/// 回答里每一处模型名都是 `want`，而且至少有一处
fn all_are(got: &Got, want: &str) {
    let found = models(&got.body);
    assert!(!found.is_empty(), "回答里没有模型名：{}", got.body);
    assert!(
        found.iter().all(|m| m == want),
        "应该都是 {want}：{found:?}\n{}",
        got.body
    );
}

// ---- 上游的原话 ----

const CLAUDE_A: &str = "claude-opus-5-20261001";

fn anthropic_whole() -> String {
    format!(
        r#"{{"id":"msg_1","type":"message","role":"assistant","model":"{CLAUDE_A}","content":[{{"type":"text","text":"hi"}}],"stop_reason":"end_turn","usage":{{"input_tokens":3,"output_tokens":1}}}}"#
    )
}

fn anthropic_stream() -> String {
    [
        format!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"{CLAUDE_A}\",\"content\":[],\"usage\":{{\"input_tokens\":3,\"output_tokens\":1}}}}}}\n\n"
        ),
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n".into(),
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n".into(),
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n".into(),
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n".into(),
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".into(),
    ]
    .concat()
}

const GPT_A: &str = "gpt-6-2026-09-01";

fn chat_whole(model: &str) -> String {
    format!(
        r#"{{"id":"c1","object":"chat.completion","created":1,"model":"{model}","choices":[{{"index":0,"message":{{"role":"assistant","content":"hi"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}}}"#
    )
}

fn chat_stream(model: &str) -> String {
    format!(
        "data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"{model}\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"hi\"}}}}]}}\n\n\
         data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"{model}\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":3,\"completion_tokens\":1,\"total_tokens\":4}}}}\n\n\
         data: [DONE]\n\n"
    )
}

const CODEX_A: &str = "gpt-6-codex-2026-09-30";

fn responses_whole() -> String {
    format!(
        r#"{{"id":"resp_1","object":"response","created_at":1,"status":"completed","model":"{CODEX_A}","output":[{{"type":"message","id":"m1","role":"assistant","status":"completed","content":[{{"type":"output_text","text":"hi","annotations":[]}}]}}],"usage":{{"input_tokens":3,"output_tokens":1,"total_tokens":4}}}}"#
    )
}

fn responses_stream() -> String {
    format!(
        "event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":1,\"status\":\"in_progress\",\"model\":\"{CODEX_A}\",\"output\":[]}}}}\n\n\
         event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"item_id\":\"m1\",\"output_index\":0,\"content_index\":0,\"delta\":\"hi\"}}\n\n\
         event: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":1,\"status\":\"completed\",\"model\":\"{CODEX_A}\",\"output\":[{{\"type\":\"message\",\"id\":\"m1\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{{\"type\":\"output_text\",\"text\":\"hi\",\"annotations\":[]}}]}}],\"usage\":{{\"input_tokens\":3,\"output_tokens\":1,\"total_tokens\":4}}}}}}\n\n"
    )
}

const GEMINI_A: &str = "gemini-3-pro-preview-09-2026";

fn gemini_chunk(text: &str, last: bool) -> String {
    let finish = if last {
        r#","finishReason":"STOP""#
    } else {
        ""
    };
    format!(
        r#"{{"candidates":[{{"content":{{"role":"model","parts":[{{"text":"{text}"}}]}}{finish},"index":0}}],"usageMetadata":{{"promptTokenCount":3,"candidatesTokenCount":1,"totalTokenCount":4}},"modelVersion":"{GEMINI_A}","responseId":"r1"}}"#
    )
}

fn gemini_sse() -> String {
    format!(
        "data: {}\r\n\r\ndata: {}\r\n\r\n",
        gemini_chunk("h", false),
        gemini_chunk("i", true)
    )
}

fn gemini_array() -> String {
    format!(
        "[{}\r\n,{}]",
        gemini_chunk("h", false),
        gemini_chunk("i", true)
    )
}

// ---- 客户端的请求 ----

fn anthropic_req(model: &str, stream: bool) -> String {
    format!(
        r#"{{"model":"{model}","max_tokens":16,"stream":{stream},"messages":[{{"role":"user","content":"hi"}}]}}"#
    )
}

fn chat_req(model: &str, stream: bool) -> String {
    format!(
        r#"{{"model":"{model}","stream":{stream},"messages":[{{"role":"user","content":"hi"}}]}}"#
    )
}

fn responses_req(model: &str, stream: bool) -> String {
    format!(r#"{{"model":"{model}","stream":{stream},"input":"hi"}}"#)
}

const GEMINI_REQ: &str = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;

/// 一个上游、一条改名规则，问一次
async fn once(
    reply: Reply,
    protocol: Protocol,
    client: &str,
    sent: &str,
    path: &str,
    body: &str,
) -> (Got, Seen) {
    let (up, seen) = upstream(reply).await;
    let (gw, mut rx) = serve(
        vec![provider("up", up, protocol)],
        Vec::new(),
        &routes(client, sent, "up"),
    )
    .await;
    (ask(gw, &mut rx, path, body).await, seen)
}

/// 上游收到的最后一个生成请求
fn sent_to(seen: &Seen) -> (String, Value) {
    seen.lock()
        .unwrap()
        .last()
        .cloned()
        .expect("上游没收到生成请求")
}

// ---- 直通 ----

#[tokio::test]
async fn anthropic_passthrough_whole_and_stream() {
    for (stream, reply) in [
        (false, ok(JSON, anthropic_whole())),
        (true, ok(SSE, anthropic_stream())),
    ] {
        let (got, seen) = once(
            reply,
            Protocol::Anthropic,
            "opus",
            "claude-opus-5",
            "/v1/messages",
            &anthropic_req("opus", stream),
        )
        .await;
        assert_eq!(got.status, 200, "{}", got.body);
        assert_eq!(sent_to(&seen).1["model"], "claude-opus-5", "发出去的是 S");
        all_are(&got, "opus");
        // 记下的是上游的原话：体检按它和 S 比
        assert_eq!(got.answered.as_deref(), Some(CLAUDE_A), "stream={stream}");
    }
}

#[tokio::test]
async fn chat_passthrough_whole_and_stream_renames_every_chunk() {
    for (stream, reply) in [
        (false, ok(JSON, chat_whole(GPT_A))),
        (true, ok(SSE, chat_stream(GPT_A))),
    ] {
        let (got, _) = once(
            reply,
            Protocol::OpenaiChat,
            "smart",
            "gpt-6",
            "/v1/chat/completions",
            &chat_req("smart", stream),
        )
        .await;
        assert_eq!(got.status, 200, "{}", got.body);
        all_are(&got, "smart");
        if stream {
            assert_eq!(models(&got.body).len(), 2, "每个 chunk 都换");
            assert!(got.body.ends_with("data: [DONE]\n\n"), "{}", got.body);
        }
        assert_eq!(got.answered.as_deref(), Some(GPT_A));
    }
}

#[tokio::test]
async fn responses_passthrough_whole_and_stream() {
    for (stream, reply) in [
        (false, ok(JSON, responses_whole())),
        (true, ok(SSE, responses_stream())),
    ] {
        let (got, _) = once(
            reply,
            Protocol::OpenaiResponses,
            "codex",
            "gpt-6-codex",
            "/v1/responses",
            &responses_req("codex", stream),
        )
        .await;
        assert_eq!(got.status, 200, "{}", got.body);
        all_are(&got, "codex");
        if stream {
            assert_eq!(
                models(&got.body).len(),
                2,
                "response.created 和 response.completed"
            );
        }
        assert_eq!(got.answered.as_deref(), Some(CODEX_A));
    }
}

#[tokio::test]
async fn gemini_passthrough_whole_sse_and_json_array() {
    for (path, reply) in [
        (
            "/v1beta/models/gem:generateContent",
            ok(JSON, gemini_chunk("hi", true)),
        ),
        (
            "/v1beta/models/gem:streamGenerateContent?alt=sse",
            ok(SSE, gemini_sse()),
        ),
        (
            "/v1beta/models/gem:streamGenerateContent",
            ok(JSON, gemini_array()),
        ),
    ] {
        let (got, seen) = once(
            reply,
            Protocol::Gemini,
            "gem",
            "gemini-3-pro",
            path,
            GEMINI_REQ,
        )
        .await;
        assert_eq!(got.status, 200, "{path}: {}", got.body);
        assert!(
            sent_to(&seen).0.contains("/models/gemini-3-pro:"),
            "Gemini 的模型在路径里：{:?}",
            sent_to(&seen).0
        );
        all_are(&got, "gem");
        assert_eq!(got.answered.as_deref(), Some(GEMINI_A), "{path}");
    }
}

// ---- 转换 ----

/// 上游说 Chat、客户端说 Anthropic
#[tokio::test]
async fn an_anthropic_client_on_a_chat_upstream() {
    for (stream, reply) in [
        (false, ok(JSON, chat_whole(GPT_A))),
        (true, ok(SSE, chat_stream(GPT_A))),
    ] {
        let (got, _) = once(
            reply,
            Protocol::OpenaiChat,
            "smart",
            "gpt-6",
            "/v1/messages",
            &anthropic_req("smart", stream),
        )
        .await;
        assert_eq!(got.status, 200, "{}", got.body);
        assert!(
            got.body.contains("\"type\":\"message"),
            "没转成 Anthropic：{}",
            got.body
        );
        all_are(&got, "smart");
        // 记下的是上游自己写的，不是转给客户端的那一版
        assert_eq!(got.answered.as_deref(), Some(GPT_A));
    }
}

/// 上游说 Anthropic，客户端分别说 Chat、Responses、Gemini（SSE、JSON 数组、整包）
#[tokio::test]
async fn chat_responses_and_gemini_clients_on_an_anthropic_upstream() {
    let cases: Vec<(&str, String, Reply)> = vec![
        (
            "/v1/chat/completions",
            chat_req("opus", false),
            ok(JSON, anthropic_whole()),
        ),
        (
            "/v1/chat/completions",
            chat_req("opus", true),
            ok(SSE, anthropic_stream()),
        ),
        (
            "/v1/responses",
            responses_req("opus", false),
            ok(JSON, anthropic_whole()),
        ),
        (
            "/v1/responses",
            responses_req("opus", true),
            ok(SSE, anthropic_stream()),
        ),
        (
            "/v1beta/models/opus:generateContent",
            GEMINI_REQ.into(),
            ok(JSON, anthropic_whole()),
        ),
        (
            "/v1beta/models/opus:streamGenerateContent?alt=sse",
            GEMINI_REQ.into(),
            ok(SSE, anthropic_stream()),
        ),
        (
            "/v1beta/models/opus:streamGenerateContent",
            GEMINI_REQ.into(),
            ok(SSE, anthropic_stream()),
        ),
    ];
    for (path, body, reply) in cases {
        let (got, seen) = once(
            reply,
            Protocol::Anthropic,
            "opus",
            "claude-opus-5",
            path,
            &body,
        )
        .await;
        assert_eq!(got.status, 200, "{path}: {}", got.body);
        assert_eq!(sent_to(&seen).1["model"], "claude-opus-5", "{path}");
        all_are(&got, "opus");
        assert_eq!(got.answered.as_deref(), Some(CLAUDE_A), "{path}");
    }
}

/// 客户端要整包、上游给了流（收齐），和客户端要流、上游给了整包（写成流）
#[tokio::test]
async fn collected_and_unrolled_answers_are_renamed_too() {
    let (got, _) = once(
        ok(SSE, chat_stream(GPT_A)),
        Protocol::OpenaiChat,
        "smart",
        "gpt-6",
        "/v1/messages",
        &anthropic_req("smart", false),
    )
    .await;
    assert_eq!(got.status, 200, "{}", got.body);
    assert!(got.body.starts_with('{'), "收成整包：{}", got.body);
    all_are(&got, "smart");

    let (got, _) = once(
        ok(JSON, chat_whole(GPT_A)),
        Protocol::OpenaiChat,
        "smart",
        "gpt-6",
        "/v1/messages",
        &anthropic_req("smart", true),
    )
    .await;
    assert_eq!(got.status, 200, "{}", got.body);
    assert!(got.body.starts_with("event:"), "写成流：{}", got.body);
    all_are(&got, "smart");
}

// ---- 响应头 ----

#[tokio::test]
async fn the_openai_model_headers_follow_the_same_rule() {
    let mut reply = ok(SSE, chat_stream(GPT_A));
    reply.headers = vec![("openai-model", GPT_A), ("x-openai-model", "gpt-6")];
    let (got, _) = once(
        reply,
        Protocol::OpenaiChat,
        "smart",
        "gpt-6",
        "/v1/chat/completions",
        &chat_req("smart", true),
    )
    .await;
    assert_eq!(got.headers["openai-model"], "smart");
    assert_eq!(got.headers["x-openai-model"], "smart");
    all_are(&got, "smart");
}

// ---- 不改的 ----

/// 上游真换了模型：客户端必须看得见。正文和头都原样
#[tokio::test]
async fn a_different_model_in_the_answer_is_left_alone() {
    for stream in [false, true] {
        let mut reply = if stream {
            ok(SSE, chat_stream("gpt-6-mini"))
        } else {
            ok(JSON, chat_whole("gpt-6-mini"))
        };
        reply.headers = vec![("openai-model", "gpt-6-mini")];
        let original = reply.body.clone();
        let (got, _) = once(
            reply,
            Protocol::OpenaiChat,
            "smart",
            "gpt-6",
            "/v1/chat/completions",
            &chat_req("smart", stream),
        )
        .await;
        assert_eq!(got.body, original, "一个字节都不动");
        assert_eq!(got.headers["openai-model"], "gpt-6-mini");
        assert_eq!(got.answered.as_deref(), Some("gpt-6-mini"));
    }
}

/// 发出去的就是客户端要的名称：上游写的带日期的快照照原样交给客户端
#[tokio::test]
async fn nothing_changes_when_the_sent_name_is_the_clients() {
    for stream in [false, true] {
        let mut reply = if stream {
            ok(SSE, chat_stream(GPT_A))
        } else {
            ok(JSON, chat_whole(GPT_A))
        };
        reply.headers = vec![("openai-model", GPT_A)];
        let original = reply.body.clone();
        let (got, _) = once(
            reply,
            Protocol::OpenaiChat,
            "gpt-6",
            "",
            "/v1/chat/completions",
            &chat_req("gpt-6", stream),
        )
        .await;
        assert_eq!(got.body, original);
        assert_eq!(got.headers["openai-model"], GPT_A);
    }
}

/// 上游回的错误是它的原话，不改
#[tokio::test]
async fn an_upstream_error_is_not_touched() {
    let body = r#"{"error":{"message":"bad","type":"invalid_request_error"},"model":"gpt-6"}"#;
    let reply = Reply {
        status: 400,
        content_type: JSON,
        headers: vec![("openai-model", "gpt-6")],
        body: body.into(),
    };
    let (got, _) = once(
        reply,
        Protocol::OpenaiChat,
        "smart",
        "gpt-6",
        "/v1/chat/completions",
        &chat_req("smart", false),
    )
    .await;
    assert_eq!(got.status, 400);
    assert_eq!(got.body, body);
    assert_eq!(got.headers["openai-model"], "gpt-6");
}

// ---- 故障转移 ----

/// 第一家失败、第二家接下，第二阶段的规则给第二家发了另一个名字：**按接下的那一跳发出去的
/// 名字认**。按第一跳的 `gpt-6` 认的话，Claude 的回答就对不上，原样漏给客户端
#[tokio::test]
async fn after_failover_the_name_the_answering_hop_sent_decides() {
    let (official, official_seen) = upstream(Reply {
        status: 503,
        content_type: JSON,
        headers: Vec::new(),
        body: r#"{"error":{"message":"overloaded"}}"#.into(),
    })
    .await;
    let (relay, relay_seen) = upstream(ok(SSE, anthropic_stream())).await;
    let routes = r#"
- name: default
  rules:
    - name: 先试 GPT
      when: { model: smart }
      set: { model: gpt-6 }
    - name: 中转上换成 Claude
      when: { provider_would_be: relay }
      set: { model: claude-opus-5 }
    - name: 兜底
      to: 全部
"#;
    let (gw, mut rx) = serve(
        vec![
            provider("official", official, Protocol::Anthropic),
            provider("relay", relay, Protocol::Anthropic),
        ],
        vec![tw_engine::Group {
            name: "全部".into(),
            kind: tw_engine::GroupType::Fallback,
            providers: vec!["official".into(), "relay".into()],
            selected: None,
        }],
        routes,
    )
    .await;
    let got = ask(gw, &mut rx, "/v1/messages", &anthropic_req("smart", true)).await;
    assert_eq!(got.status, 200, "{}", got.body);
    assert_eq!(sent_to(&official_seen).1["model"], "gpt-6");
    assert_eq!(sent_to(&relay_seen).1["model"], "claude-opus-5");
    all_are(&got, "smart");
    assert_eq!(got.answered.as_deref(), Some(CLAUDE_A));
}

// ---- 插件 ----

/// 插件在请求钩子里换了发出去的模型名：和规则改写一样，回答里写回客户端要的那个
#[tokio::test]
async fn a_name_a_plugin_sent_is_renamed_back() {
    for stream in [false, true] {
        let (up, seen) = upstream(if stream {
            ok(SSE, anthropic_stream())
        } else {
            ok(JSON, anthropic_whole())
        })
        .await;
        let swap = Double::new("swap model")
            .permit(&[Permission::Params])
            .on_request(|mut view, _| {
                view["params"]["model"] = json!("claude-opus-5");
                Invocation::ok(RequestOutcome::Changed(view))
            });
        let (gw, mut rx) = serve_with(
            vec![provider("up", up, Protocol::Anthropic)],
            Vec::new(),
            &routes("opus", "", "up"),
            vec![swap],
        )
        .await;
        let got = ask(gw, &mut rx, "/v1/messages", &anthropic_req("opus", stream)).await;
        assert_eq!(got.status, 200, "{}", got.body);
        assert_eq!(sent_to(&seen).1["model"], "claude-opus-5");
        all_are(&got, "opus");
        assert_eq!(got.answered.as_deref(), Some(CLAUDE_A));
    }
}

/// 回答钩子排在改名之后：插件补出来的帧照抄前一帧的外壳，抄到的是客户端的名称；插件的
/// `ctx.model` 仍是发出去的那个
#[tokio::test]
async fn reply_hooks_see_the_sent_name_in_ctx_and_their_frames_carry_the_clients() {
    for (mode, stream) in [
        (tw_api::ReplyMode::Block, true),
        (tw_api::ReplyMode::Stream, true),
        (tw_api::ReplyMode::Block, false),
    ] {
        let (up, _) = upstream(if stream {
            ok(SSE, chat_stream(GPT_A))
        } else {
            ok(JSON, chat_whole(GPT_A))
        })
        .await;
        let ctx: Arc<Mutex<Option<Value>>> = Arc::default();
        let saw = ctx.clone();
        let upper = Double::new("upper")
            .permit(&[Permission::ReplyText])
            .mode(mode)
            .on_reply(true, false, false, move |c| {
                *saw.lock().unwrap() = Some(c);
                Ok(Box::new(double::Closures {
                    text: Box::new(|t| Invocation::ok(Some(t.to_uppercase()))),
                    end: Box::new(|| Invocation::ok(None)),
                    tool: Box::new(|_| {
                        Invocation::ok(tw_gateway::plugin::ToolCallOutcome::Unchanged)
                    }),
                }))
            });
        let (gw, mut rx) = serve_with(
            vec![provider("up", up, Protocol::OpenaiChat)],
            Vec::new(),
            &routes("smart", "gpt-6", "up"),
            vec![upper],
        )
        .await;
        let got = ask(
            gw,
            &mut rx,
            "/v1/chat/completions",
            &chat_req("smart", stream),
        )
        .await;
        assert_eq!(got.status, 200, "{}", got.body);
        assert!(got.body.contains("HI"), "插件没改到：{}", got.body);
        all_are(&got, "smart");
        let c = ctx.lock().unwrap().clone().expect("回答钩子没起");
        assert_eq!(
            (&c["model"], &c["requested_model"]),
            (&json!("gpt-6"), &json!("smart"))
        );
    }
}
