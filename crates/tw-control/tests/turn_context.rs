//! 一轮的上下文端点（`GET /sessions/{id}/turns/{turn}/context`）和会话详情里每一轮的
//! `context_window`。
//!
//! 按块怎么估在 tw-engine 里测；这里盯的是**界面拿到的那份 JSON**：四种格式的请求体都读得
//! 出四块、请求体不在了是什么样、两种 404 各是哪个码。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};
use tw_store::Which;

const BASE: &str = "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: 我\n    key: tw-一把钥匙就够\nproviders:\n  - name: 官方\n    base_url: https://api.anthropic.com\n    key: sk-x\n    model_specs:\n      handwritten: { context_window: 4096 }\n";

/// 一轮：`path` 说客户端是哪种格式
fn turn(id: i64, session: &str, model: &str, path: &str) -> tw_store::db::RequestRow {
    tw_store::db::RequestRow {
        session_log_bytes: None,
        key_masked: None,
        peer: None,
        id,
        at_ms: 1_790_000_000_000 + id,
        client: "claude-code".into(),
        client_hint: None,
        session: Some(session.into()),
        provider: "官方".into(),
        model: model.into(),
        sent_model: model.into(),
        answered_model: None,
        path: path.into(),
        status: Some(200),
        ttfb_ms: Some(100),
        ttft_ms: None,
        duration_ms: Some(200),
        tokens_per_sec: None,
        sent_bytes: Some(10),
        received_bytes: Some(10),
        egress: None,
        input_tokens: Some(1234),
        output_tokens: Some(20),
        cache_read_tokens: Some(1000),
        cache_write_tokens: None,
        input_estimate: None,
        cost_micros: Some(1_000),
        cost_estimated: false,
        error: None,
        local: false,
        cancelled: false,
        routing: None,
        billing: tw_api::Billing::PerToken,
        cache_saved_micros: None,
        price_source: None,
        translated: None,
    }
}

/// 一个带着请求库和正文的控制面。`bodies`：哪一条的请求体（没存下的不在里面）
fn app(
    rows: &[tw_store::db::RequestRow],
    bodies: &[(i64, Value)],
) -> (tempfile::TempDir, axum::Router) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, BASE).unwrap();
    let db = tw_store::Db::open(&d.path().join("data.db")).unwrap();
    for r in rows {
        db.insert(r).unwrap();
    }
    let blobs = tw_store::Blobs::new(d.path().join("blobs"));
    for (id, request) in bodies {
        let at = rows.iter().find(|r| r.id == *id).unwrap().at_ms;
        assert!(blobs.put(at, *id, Which::Request, request.to_string().as_bytes()));
    }
    let rec = tw_store::Recorder::new(
        db,
        blobs,
        tw_pricing::shared(tw_pricing::PriceBook::builtin().unwrap()),
    );
    let cfg: tw_config::Config = serde_yaml_ng::from_str(BASE).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let state = ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        store: Some(Arc::new(tokio::sync::Mutex::new(rec))),
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    };
    (d, tw_control::router(state))
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let r = app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let st = r.status();
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (st, serde_json::from_slice(&b).unwrap())
}

/// 四块都是正数、加起来是 `total`
fn well_formed(parts: &Value) {
    let n = |k: &str| {
        parts[k]
            .as_u64()
            .unwrap_or_else(|| panic!("{k} in {parts}"))
    };
    assert!(n("system") > 0, "{parts}");
    assert!(n("tools") > 0, "{parts}");
    assert!(n("history") > 0, "{parts}");
    assert!(n("last_user") > 0, "{parts}");
    assert_eq!(
        n("total"),
        n("system") + n("tools") + n("history") + n("last_user")
    );
}

/// Anthropic：`system`、`tools`、消息；最后一条用户消息带着工具结果。实测的两个数来自
/// 记录，窗口来自价目表，和会话详情里那一轮的 `context_window` 是同一个数
#[tokio::test]
async fn an_anthropic_body_is_split_into_four_parts() {
    let body = json!({"model": "claude-sonnet-4-5", "max_tokens": 100,
    "system": "You are a careful assistant.",
    "tools": [{"name": "Read", "description": "read a file", "input_schema": {"type": "object"}}],
    "messages": [
        {"role": "user", "content": "first question about the code"},
        {"role": "assistant", "content": [{"type": "text", "text": "let me look"},
            {"type": "tool_use", "id": "t1", "name": "Read", "input": {"p": "a.rs"}}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1",
            "content": "fn main() {}"}]}
    ]});
    let (_d, app) = app(
        &[turn(1, "s1", "claude-sonnet-4-5", "/v1/messages")],
        &[(1, body)],
    );

    let (st, v) = get(&app, "/sessions/s1/turns/1/context").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["kept"], true);
    assert_eq!(v["input_tokens"], 1234);
    assert_eq!(v["cache_read_tokens"], 1000);
    let window = v["window"]
        .as_u64()
        .expect("the price table knows the model");
    assert!(window >= 200_000, "{v}");
    well_formed(&v["parts"]);
    // 系统提示 28 个 ASCII 字符
    assert_eq!(v["parts"]["system"], 7);

    let (st, detail) = get(&app, "/sessions/s1").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(detail["turns"][0]["context_window"], window);
}

/// Chat：system、developer 消息算系统提示；工具结果的消息是用户角色，所以最后一条
/// 用户消息就是那条工具结果
#[tokio::test]
async fn a_chat_body_is_split_into_four_parts() {
    let body = json!({"model": "gpt-5", "stream": false,
    "tools": [{"type": "function", "function": {"name": "read", "description": "read",
        "parameters": {"type": "object"}}}],
    "messages": [
        {"role": "system", "content": "You are a careful assistant."},
        {"role": "developer", "content": "Answer briefly."},
        {"role": "user", "content": "first question about the code"},
        {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function",
            "function": {"name": "read", "arguments": "{\"p\":\"a.rs\"}"}}]},
        {"role": "tool", "tool_call_id": "c1", "content": "fn main() {}"}
    ]});
    let (_d, app) = app(
        &[turn(1, "s1", "gpt-5", "/v1/chat/completions")],
        &[(1, body)],
    );

    let (st, v) = get(&app, "/sessions/s1/turns/1/context").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["kept"], true);
    well_formed(&v["parts"]);
    // 28 + 15 个 ASCII 字符，两段各自取整
    assert_eq!(v["parts"]["system"], 7 + 4);
    // 最后一条用户消息只有那条工具结果："fn main() {}" 12 个字符加上消息开销 3
    assert_eq!(v["parts"]["last_user"], 3 + 3);
}

/// Responses：`instructions` 和 developer 项算系统提示，`function_call_output` 是用户角色
#[tokio::test]
async fn a_responses_body_is_split_into_four_parts() {
    let body = json!({"model": "gpt-5-codex", "instructions": "You are a careful assistant.",
    "tools": [{"type": "function", "name": "read", "description": "read",
        "parameters": {"type": "object"}}],
    "input": [
        {"role": "developer", "content": "Answer briefly."},
        {"role": "user", "content": "first question about the code"},
        {"type": "function_call", "call_id": "c1", "name": "read", "arguments": "{\"p\":\"a.rs\"}"},
        {"type": "function_call_output", "call_id": "c1", "output": "fn main() {}"}
    ]});
    let (_d, app) = app(
        &[turn(1, "s1", "gpt-5-codex", "/v1/responses")],
        &[(1, body)],
    );

    let (st, v) = get(&app, "/sessions/s1/turns/1/context").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["kept"], true);
    well_formed(&v["parts"]);
    assert_eq!(v["parts"]["system"], 7 + 4);
    assert_eq!(v["parts"]["last_user"], 3 + 3);
}

/// Gemini：`systemInstruction`、`tools[].functionDeclarations`、`contents`；`functionResponse`
/// 在用户角色里
#[tokio::test]
async fn a_gemini_body_is_split_into_four_parts() {
    let body = json!({
    "systemInstruction": {"parts": [{"text": "You are a careful assistant."}]},
    "tools": [{"functionDeclarations": [{"name": "read", "description": "read",
        "parameters": {"type": "object"}}]}],
    "contents": [
        {"role": "user", "parts": [{"text": "first question about the code"}]},
        {"role": "model", "parts": [{"functionCall": {"name": "read", "args": {"p": "a.rs"}}}]},
        {"role": "user", "parts": [{"functionResponse": {"name": "read",
            "response": {"output": "fn main() {}"}}}]}
    ]});
    let (_d, app) = app(
        &[turn(
            1,
            "s1",
            "gemini-2.5-pro",
            "/v1beta/models/gemini-2.5-pro:generateContent",
        )],
        &[(1, body)],
    );

    let (st, v) = get(&app, "/sessions/s1/turns/1/context").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["kept"], true);
    well_formed(&v["parts"]);
    assert_eq!(v["parts"]["system"], 7);
}

/// 请求体没存下来（或者清掉了）：`kept` 是 false、`parts` 是 null，别的照给
#[tokio::test]
async fn a_turn_whose_body_is_gone_says_so() {
    let (_d, app) = app(&[turn(1, "s1", "claude-sonnet-4-5", "/v1/messages")], &[]);

    let (st, v) = get(&app, "/sessions/s1/turns/1/context").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["kept"], false);
    assert!(v["parts"].is_null(), "{v}");
    assert_eq!(v["input_tokens"], 1234);
    assert_eq!(v["cache_read_tokens"], 1000);
    assert!(v["window"].as_u64().is_some(), "{v}");
}

/// 手写的规格优先于价目表；价目表也不认识的模型，窗口是 null
#[tokio::test]
async fn the_window_is_the_spec_known_now() {
    let (_d, app) = app(
        &[
            turn(1, "s1", "handwritten", "/v1/messages"),
            turn(2, "s1", "nobody-knows-this-model", "/v1/messages"),
        ],
        &[],
    );

    let (_, v) = get(&app, "/sessions/s1/turns/1/context").await;
    assert_eq!(v["window"], 4096);
    let (_, v) = get(&app, "/sessions/s1/turns/2/context").await;
    assert!(v["window"].is_null(), "{v}");

    let (_, detail) = get(&app, "/sessions/s1").await;
    assert_eq!(detail["turns"][0]["context_window"], 4096);
    assert!(detail["turns"][1]["context_window"].is_null(), "{detail}");
}

/// 会话不存在和那一轮不在这次会话里是两个 404，各用会话端点已有的码
#[tokio::test]
async fn unknown_sessions_and_turns_are_two_different_404s() {
    let (_d, app) = app(
        &[
            turn(1, "s1", "claude-sonnet-4-5", "/v1/messages"),
            turn(2, "s2", "claude-sonnet-4-5", "/v1/messages"),
        ],
        &[],
    );

    let (st, v) = get(&app, "/sessions/nope/turns/1/context").await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(v["code"], "control.session_not_found");

    // 第 2 轮是别的会话的
    let (st, v) = get(&app, "/sessions/s1/turns/2/context").await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(v["code"], "control.request_not_found");

    let (st, v) = get(&app, "/sessions/s1/turns/99/context").await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(v["code"], "control.request_not_found");
}
