//! DeepSeek Harness（dsh）的请求在网关上：发给 DeepSeek 官方原样透传，发给别家先清理。
//!
//! 清理本身在 `tw_dialect::harness` 里有单元测试；这里验的是网关把它接对了：
//!
//! - 同格式直通和格式转换两条路，发给别家的请求里都没有 dsh 的扩展字段、中途的
//!   system 条目和增删工具的块，也不带它自己的请求头；丢掉了什么记在这一跳上
//! - 发给 DeepSeek 官方时一个字节都不改，请求头照发；要转换格式时扩展字段照样带上
//! - 开始事件说得出请求带没带会话日志、有多大
//! - `/v1/files` 回 404，dsh 才会退回内联图片
//!
//! 「DeepSeek 官方」认的是上游地址的 host，测试里的假上游在 127.0.0.1 上。所以那几条
//! 把上游写成 `http://api.deepseek.com`，再让这家走一个 HTTP 代理，代理就是假上游：
//! 它收到的正是发给 DeepSeek 的那一份。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{OriginalUri, State};
use axum::http::HeaderMap;
use serde_json::{Value, json};
use tw_config::{Client, Config, Listen, Protocol, Provider};

const UA: &str = "deepseek-harness/0.1.7 (+https://github.com/deepseek-ai/deepseek-harness)";

#[derive(Debug, Clone)]
struct Seen {
    uri: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

type Log = Arc<Mutex<Vec<Seen>>>;

/// 一个假上游：记下收到的每个请求，按上游的格式回一个整包。也可以当 HTTP 代理用
async fn upstream(protocol: Protocol) -> (SocketAddr, Log) {
    let reply = match protocol {
        Protocol::Anthropic => json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 2}
        }),
        _ => json!({
            "id": "chatcmpl-1", "object": "chat.completion", "model": "m",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2}
        }),
    };
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .fallback(
            move |State(log): State<Log>,
                  OriginalUri(uri): OriginalUri,
                  headers: HeaderMap,
                  body: bytes::Bytes| {
                let reply = reply.clone();
                async move {
                    log.lock().unwrap().push(Seen {
                        uri: uri.to_string(),
                        headers,
                        body: body.to_vec(),
                    });
                    axum::response::Response::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(reply.to_string()))
                        .unwrap()
                }
            },
        )
        .with_state(log.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, log)
}

/// 上游收到的那个生成请求（启动时也许还有别的请求，比如拉模型列表）
fn generation(log: &Log) -> Seen {
    log.lock()
        .unwrap()
        .iter()
        .rev()
        .find(|s| s.uri.ends_with("/messages") || s.uri.ends_with("/chat/completions"))
        .cloned()
        .expect("the upstream got no generation request")
}

/// 一家别的上游（不是 DeepSeek 官方）
async fn elsewhere(protocol: Protocol) -> (Provider, Vec<tw_config::Proxy>, Log) {
    let (addr, log) = upstream(protocol).await;
    let p = Provider {
        name: "relay".into(),
        base_url: format!("http://{addr}"),
        key: Some("sk-upstream".into()),
        protocol: Some(protocol),
        ..Default::default()
    };
    (p, Vec::new(), log)
}

/// DeepSeek 官方：地址是它的，请求经代理落到假上游上
async fn deepseek(protocol: Protocol) -> (Provider, Vec<tw_config::Proxy>, Log) {
    let (addr, log) = upstream(protocol).await;
    let base_url = match protocol {
        Protocol::Anthropic => "http://api.deepseek.com/anthropic",
        _ => "http://api.deepseek.com",
    };
    let p = Provider {
        name: "deepseek".into(),
        base_url: base_url.into(),
        key: Some("sk-upstream".into()),
        protocol: Some(protocol),
        proxy: "fake".into(),
        ..Default::default()
    };
    let proxies = vec![tw_config::Proxy {
        name: "fake".into(),
        kind: tw_config::ProxyKind::Http,
        addr: addr.to_string(),
        auth: None,
    }];
    (p, proxies, log)
}

async fn gateway(
    (p, proxies, log): (Provider, Vec<tw_config::Proxy>, Log),
) -> (
    SocketAddr,
    tokio::sync::broadcast::Receiver<tw_api::Event>,
    Log,
) {
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![p],
        proxies,
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let rx = state.bus.subscribe();
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, rx, log)
}

/// dsh 发请求的样子：它的 User-Agent 和自己的三个头
async fn send(gw: SocketAddr, path: &str, extra: &[(&str, &str)], body: &str) -> (u16, String) {
    let mut r = reqwest::Client::new()
        .post(format!("http://{gw}{path}"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer tw-k")
        .header("user-agent", UA)
        .header(
            "x-deepseek-harness-user-id",
            "0b0c6a52-2f0e-4c1a-9d5e-3a1f5e2b7c11",
        )
        .header("x-deepseek-harness-session-id", "s-1")
        .header("x-deepseek-harness-compact", "1");
    for (k, v) in extra {
        r = r.header(*k, *v);
    }
    let resp = r.body(body.to_string()).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap())
}

const TOOL_BETA: &str = "mid-conversation-tool-changes-2026-07-01";

fn session_log() -> Value {
    json!({
        "version": 1, "sessionFormatVersion": 2,
        "session": {"version": 2, "id": "s-1", "createdAt": 1780000000000u64, "cwd": "/work"},
        "afterSeq": -1, "throughSeq": 1,
        "events": [
            {"type": "turn/start", "seq": 0, "time": 1780000000001u64, "data": {"turn": 1}},
            {"type": "user", "seq": 1, "time": 1780000000002u64, "data": {"text": "hi"}}
        ]
    })
}

/// dsh 0.1.7：Anthropic Messages，带会话日志、中途的系统提示和增删工具
fn messages_request() -> Value {
    json!({
        "model": "deepseek-v4-pro",
        "max_tokens": 32000,
        "system": "You are DeepSeek Harness.",
        "thinking": {"type": "enabled"},
        "output_config": {"effort": "high"},
        "tools": [
            {"name": "read", "description": "Read a file", "input_schema": {"type": "object"}},
            {"name": "web", "description": "Search the web", "input_schema": {"type": "object"}, "defer_loading": true}
        ],
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            {"role": "system", "content": [
                {"type": "text", "text": "The project root is /work."},
                {"type": "tool_addition", "tool": {"type": "tool_reference", "name": "web"}}
            ]},
            {"role": "assistant", "content": [{"type": "text", "text": "hello"}]},
            {"role": "user", "content": [{"type": "text", "text": "search it"}]},
            {"role": "system", "content": [
                {"type": "tool_removal", "tool": {"type": "tool_reference", "name": "web"}}
            ]}
        ],
        "dsh_session_log": session_log(),
        "dsh_plugin_packages": {"version": 1, "packages": [{"name": "@deepseek-ai/dsh-example", "version": "0.1.1"}]}
    })
}

/// dsh 0.1.5：Chat Completions，带会话日志
fn chat_request() -> Value {
    json!({
        "model": "deepseek-v4-flash",
        "messages": [
            {"role": "system", "content": "You are DeepSeek Harness."},
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello", "reasoning_content": "greet"},
            {"role": "system", "content": "The project root is /work."},
            {"role": "user", "content": "search it"}
        ],
        "thinking": {"type": "enabled"},
        "reasoning_effort": "high",
        "dsh_session_log": session_log(),
        "dsh_plugin_packages": {"version": 1, "packages": []}
    })
}

/// 发给别家的那一份里不能再有的东西
fn assert_clean(s: &Seen) {
    let v: Value = serde_json::from_slice(&s.body).unwrap();
    let o = v.as_object().unwrap();
    assert!(!o.keys().any(|k| k.starts_with("dsh_")), "{v}");
    let text = String::from_utf8_lossy(&s.body);
    assert!(
        !text.contains("tool_addition") && !text.contains("tool_removal"),
        "{v}"
    );
    assert!(!text.contains("defer_loading"), "{v}");
    for (name, _) in s.headers.iter() {
        assert!(
            !name.as_str().starts_with("x-deepseek-harness-"),
            "{name} was forwarded"
        );
    }
    if let Some(b) = s.headers.get("anthropic-beta") {
        assert!(!b.to_str().unwrap().contains("mid-conversation"), "{b:?}");
    }
}

/// 这个请求的开始事件里的会话日志大小，和这一跳丢掉的字段
async fn events(
    rx: &mut tokio::sync::broadcast::Receiver<tw_api::Event>,
) -> (
    Option<u64>,
    Option<(tw_api::Dialect, tw_api::Dialect, Vec<String>)>,
) {
    let mut log = None;
    let mut translated = None;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
        match ev {
            tw_api::Event::RequestStarted {
                session_log_bytes, ..
            } => log = session_log_bytes,
            tw_api::Event::Translated {
                from, to, dropped, ..
            } => translated = Some((from, to, dropped)),
            tw_api::Event::RequestFinished { .. } | tw_api::Event::RequestFailed { .. } => break,
            _ => {}
        }
    }
    (log, translated)
}

fn log_len() -> u64 {
    serde_json::to_vec(&session_log()).unwrap().len() as u64
}

// ───────────────────────────────────────────────────────── dsh 0.1.7（Anthropic）

#[tokio::test]
async fn messages_to_another_anthropic_upstream_are_cleaned_in_place() {
    let (gw, mut rx, log) = gateway(elsewhere(Protocol::Anthropic).await).await;
    let beta = format!("files-api-2025-04-14,{TOOL_BETA}");
    let (status, body) = send(
        gw,
        "/v1/messages",
        &[
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", &beta),
        ],
        &messages_request().to_string(),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let s = generation(&log);
    assert_eq!(s.uri, "/v1/messages");
    assert_clean(&s);
    // 别的 beta 照发，User-Agent 照发
    assert_eq!(
        s.headers.get("anthropic-beta").unwrap(),
        "files-api-2025-04-14"
    );
    assert_eq!(s.headers.get("user-agent").unwrap(), UA);
    let v: Value = serde_json::from_slice(&s.body).unwrap();
    assert_eq!(
        v["system"],
        "You are DeepSeek Harness.\n\nThe project root is /work."
    );
    let roles: Vec<&str> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "user"]);
    // Anthropic 的 `enabled` 要带预算
    assert_eq!(v["thinking"]["type"], "enabled");
    assert!(v["thinking"]["budget_tokens"].as_u64().unwrap() >= 1024);

    let (bytes, translated) = events(&mut rx).await;
    assert_eq!(bytes, Some(log_len()));
    let (from, to, dropped) = translated.expect("what was dropped is said");
    assert_eq!(
        (from, to),
        (tw_api::Dialect::Anthropic, tw_api::Dialect::Anthropic)
    );
    assert_eq!(
        dropped,
        [
            "messages.content.tool_addition",
            "messages.content.tool_removal",
            "tools.defer_loading"
        ]
    );
}

/// `anthropic-beta` 分两行发：清理的那一行去掉，**另一行照发**
#[tokio::test]
async fn betas_sent_on_separate_lines_all_survive_the_cleaning() {
    let (gw, _rx, log) = gateway(elsewhere(Protocol::Anthropic).await).await;
    let (status, body) = send(
        gw,
        "/v1/messages",
        &[
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", TOOL_BETA),
            ("anthropic-beta", "files-api-2025-04-14"),
        ],
        &messages_request().to_string(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let s = generation(&log);
    assert_clean(&s);
    let betas: Vec<_> = s.headers.get_all("anthropic-beta").iter().collect();
    assert_eq!(betas, ["files-api-2025-04-14"]);
}

/// 不生成回答的请求（数 token）也是 dsh 发的：发给别家时一样不带它的头和会话日志
#[tokio::test]
async fn counting_tokens_elsewhere_is_cleaned_too() {
    let (gw, _rx, log) = gateway(elsewhere(Protocol::Anthropic).await).await;
    let mut req = messages_request();
    req.as_object_mut().unwrap().remove("max_tokens");
    let (status, body) = send(
        gw,
        "/v1/messages/count_tokens",
        &[
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", TOOL_BETA),
        ],
        &req.to_string(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let s = log
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|s| s.uri.ends_with("/count_tokens"))
        .cloned()
        .expect("the upstream got no count_tokens request");
    assert_clean(&s);
    assert!(s.headers.get("anthropic-beta").is_none());
}

#[tokio::test]
async fn messages_to_an_openai_upstream_are_cleaned_by_the_conversion() {
    let (gw, mut rx, log) = gateway(elsewhere(Protocol::OpenaiChat).await).await;
    let (status, body) = send(
        gw,
        "/v1/messages",
        &[
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", TOOL_BETA),
        ],
        &messages_request().to_string(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(answer["content"][0]["text"], "ok");

    let s = generation(&log);
    assert_eq!(s.uri, "/v1/chat/completions");
    assert_clean(&s);
    assert!(s.headers.get("anthropic-beta").is_none());
    let v: Value = serde_json::from_slice(&s.body).unwrap();
    // 中途的系统提示并进了开头的那一条
    let system: Vec<&str> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "system")
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(
        system
            .concat()
            .matches("The project root is /work.")
            .count(),
        1
    );
    assert!(system.concat().starts_with("You are DeepSeek Harness."));
    assert_eq!(v["tools"].as_array().unwrap().len(), 2);

    let (bytes, translated) = events(&mut rx).await;
    assert_eq!(bytes, Some(log_len()));
    let (from, to, dropped) = translated.unwrap();
    assert_eq!(
        (from, to),
        (tw_api::Dialect::Anthropic, tw_api::Dialect::OpenaiChat)
    );
    for d in [
        "messages.content.tool_addition",
        "messages.content.tool_removal",
        "tools.defer_loading",
    ] {
        assert!(dropped.iter().any(|x| x == d), "{d} not in {dropped:?}");
    }
}

#[tokio::test]
async fn messages_to_deepseek_go_through_untouched() {
    let (gw, mut rx, log) = gateway(deepseek(Protocol::Anthropic).await).await;
    let body = messages_request().to_string();
    let (status, answer) = send(
        gw,
        "/v1/messages",
        &[
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", TOOL_BETA),
        ],
        &body,
    )
    .await;
    assert_eq!(status, 200, "{answer}");

    let s = generation(&log);
    assert_eq!(s.uri, "http://api.deepseek.com/anthropic/v1/messages");
    assert_eq!(s.body, body.as_bytes(), "a single byte changed");
    assert_eq!(
        s.headers.get("x-deepseek-harness-session-id").unwrap(),
        "s-1"
    );
    assert_eq!(s.headers.get("x-deepseek-harness-compact").unwrap(), "1");
    assert_eq!(s.headers.get("anthropic-beta").unwrap(), TOOL_BETA);

    let (bytes, translated) = events(&mut rx).await;
    assert_eq!(bytes, Some(log_len()), "the log is flagged whoever gets it");
    assert!(translated.is_none(), "{translated:?}");
}

// ───────────────────────────────────────────────────────── dsh 0.1.5（Chat）

#[tokio::test]
async fn chat_to_another_openai_upstream_loses_only_the_extensions() {
    let (gw, mut rx, log) = gateway(elsewhere(Protocol::OpenaiChat).await).await;
    let (status, body) = send(gw, "/chat/completions", &[], &chat_request().to_string()).await;
    assert_eq!(status, 200, "{body}");

    let s = generation(&log);
    assert_eq!(s.uri, "/chat/completions");
    assert_clean(&s);
    let v: Value = serde_json::from_slice(&s.body).unwrap();
    // Chat 本来就有 system 角色：messages 原样
    assert_eq!(v["messages"], chat_request()["messages"]);
    assert_eq!(v["reasoning_effort"], "high");

    let (bytes, translated) = events(&mut rx).await;
    assert_eq!(bytes, Some(log_len()));
    // 只去掉了扩展字段：没有转不过去的东西要报
    assert!(translated.is_none(), "{translated:?}");
}

#[tokio::test]
async fn chat_to_an_anthropic_upstream_is_cleaned_by_the_conversion() {
    let (gw, mut rx, log) = gateway(elsewhere(Protocol::Anthropic).await).await;
    let (status, body) = send(gw, "/v1/chat/completions", &[], &chat_request().to_string()).await;
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(answer["choices"][0]["message"]["content"], "ok");

    let s = generation(&log);
    assert_eq!(s.uri, "/v1/messages");
    assert_clean(&s);
    let v: Value = serde_json::from_slice(&s.body).unwrap();
    let system: Vec<&str> = v["system"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        system,
        ["You are DeepSeek Harness.", "The project root is /work."]
    );
    assert!(
        v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["role"] != "system")
    );
    assert_eq!(v["thinking"]["type"], "enabled");

    let (bytes, translated) = events(&mut rx).await;
    assert_eq!(bytes, Some(log_len()));
    let (from, to, _) = translated.unwrap();
    assert_eq!(
        (from, to),
        (tw_api::Dialect::OpenaiChat, tw_api::Dialect::Anthropic)
    );
}

#[tokio::test]
async fn chat_to_deepseek_goes_through_untouched() {
    let (gw, _rx, log) = gateway(deepseek(Protocol::OpenaiChat).await).await;
    let body = chat_request().to_string();
    let (status, answer) = send(gw, "/v1/chat/completions", &[], &body).await;
    assert_eq!(status, 200, "{answer}");

    let s = generation(&log);
    assert_eq!(s.uri, "http://api.deepseek.com/v1/chat/completions");
    assert_eq!(s.body, body.as_bytes(), "a single byte changed");
    assert_eq!(
        s.headers.get("x-deepseek-harness-user-id").unwrap(),
        "0b0c6a52-2f0e-4c1a-9d5e-3a1f5e2b7c11"
    );
}

#[tokio::test]
async fn converted_for_deepseek_the_extensions_ride_along() {
    // 客户端说 Anthropic、DeepSeek 这家配成 Chat：格式要转，扩展照样带给 DeepSeek
    let (gw, _rx, log) = gateway(deepseek(Protocol::OpenaiChat).await).await;
    let (status, answer) = send(
        gw,
        "/v1/messages",
        &[("anthropic-version", "2023-06-01")],
        &messages_request().to_string(),
    )
    .await;
    assert_eq!(status, 200, "{answer}");

    let s = generation(&log);
    assert_eq!(s.uri, "http://api.deepseek.com/v1/chat/completions");
    let v: Value = serde_json::from_slice(&s.body).unwrap();
    assert_eq!(v["dsh_session_log"], session_log());
    assert_eq!(v["dsh_plugin_packages"]["version"], 1);
    assert_eq!(
        s.headers.get("x-deepseek-harness-session-id").unwrap(),
        "s-1"
    );
}

// ───────────────────────────────────────────────────────── 别的请求

#[tokio::test]
async fn a_request_without_the_harness_is_not_touched() {
    let (gw, mut rx, log) = gateway(elsewhere(Protocol::Anthropic).await).await;
    let body = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 100,
        "thinking": {"type": "enabled", "budget_tokens": 2048},
        "messages": [{"role": "user", "content": "hi"}]
    })
    .to_string();
    let resp = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.195 (external, cli)")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(generation(&log).body, body.as_bytes());
    let (bytes, translated) = events(&mut rx).await;
    assert_eq!(bytes, None);
    assert!(translated.is_none());
}

#[tokio::test]
async fn the_files_api_is_not_found_so_images_go_inline() {
    let (gw, _rx, log) = gateway(elsewhere(Protocol::Anthropic).await).await;
    let c = reqwest::Client::new();
    for (path, anthropic) in [
        ("/v1/files", true),
        ("/v1/files", false),
        ("/files", false),
        ("/v1/files/file-abc", true),
        ("/files?limit=10", false),
    ] {
        let mut r = c
            .post(format!("http://{gw}{path}"))
            .header("authorization", "Bearer tw-k")
            .header("user-agent", UA)
            .body("--x\r\n");
        if anthropic {
            r = r.header("anthropic-version", "2023-06-01");
        }
        let resp = r.send().await.unwrap();
        assert_eq!(resp.status(), 404, "{path}");
        let v: Value = resp.json().await.unwrap();
        // 两种形状都把原因放在 `error.type` / `error.message`，Anthropic 的外面还有一层 `type`
        assert_eq!(v["error"]["type"], "not_found_error", "{path}: {v}");
        let message = v["error"]["message"].as_str().unwrap();
        assert!(message.starts_with("[ThinkWatch]"), "{v}");
        assert_eq!(v.get("type").is_some(), anthropic, "{path}: {v}");
    }
    let got = c
        .get(format!("http://{gw}/v1/files"))
        .header("authorization", "Bearer tw-k")
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 404);
    assert!(
        log.lock().unwrap().iter().all(|s| !s.uri.contains("files")),
        "the upload reached an upstream"
    );
}
