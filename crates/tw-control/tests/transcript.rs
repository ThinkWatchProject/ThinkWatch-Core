//! 对话记录端点（`GET /sessions/{id}/transcript`）的形状。
//!
//! 四种格式怎么读、前后两个请求怎么比在 tw-store 里测；这里盯的是**界面拿到的那份
//! JSON**，一个字段一个字段地钉住：桌面端照着它写，改了哪个字段名那边就悄悄读不到了。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};
use tw_store::Which;

const BASE: &str = "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: 我\n    key: tw-一把钥匙就够\nproviders:\n  - name: 官方\n    base_url: https://api.anthropic.com\n    key: sk-x\n";

/// Claude Code 的一轮
fn turn(id: i64) -> tw_store::db::RequestRow {
    tw_store::db::RequestRow {
        session_log_bytes: None,
        key_masked: None,
        peer: None,
        id,
        at_ms: 1_790_000_000_000 + id,
        client: "claude-code".into(),
        client_hint: None,
        session: Some("s1".into()),
        provider: "官方".into(),
        model: "claude-sonnet-4-5".into(),
        sent_model: "claude-sonnet-4-5".into(),
        answered_model: None,
        path: "/v1/messages".into(),
        status: Some(200),
        ttfb_ms: Some(100),
        ttft_ms: None,
        duration_ms: Some(200),
        tokens_per_sec: None,
        bytes: Some(10),
        input_tokens: Some(50),
        output_tokens: Some(20),
        cache_read_tokens: None,
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

/// 一个带着请求库和正文的控制面。`bodies`：哪一条、请求体、回答（没存下的是 None）
fn app(
    rows: &[tw_store::db::RequestRow],
    bodies: &[(i64, Option<Value>, Option<Value>)],
) -> (tempfile::TempDir, axum::Router) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, BASE).unwrap();
    let db = tw_store::Db::open(&d.path().join("data.db")).unwrap();
    for r in rows {
        db.insert(r).unwrap();
    }
    let blobs = tw_store::Blobs::new(d.path().join("blobs"));
    for (id, request, response) in bodies {
        let at = rows.iter().find(|r| r.id == *id).unwrap().at_ms;
        for (which, body) in [(Which::Request, request), (Which::Response, response)] {
            if let Some(b) = body {
                assert!(blobs.put(at, *id, which, b.to_string().as_bytes()));
            }
        }
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

/// 每一种块、每一个字段都在：请求号是字符串，可以为空的字段写成 null 而不是省掉，
/// 块和缺口是界面认的那几个词。工具结果里的密钥打了码，图片只有类型和大小
#[tokio::test]
async fn the_transcript_is_the_shape_the_desktop_app_reads() {
    let png = "QUJD".repeat(2);
    let first = json!({"model": "claude-sonnet-4-5", "system": "sys", "messages": [
        {"role": "user", "content": [
            {"type": "text", "text": "hi"},
            {"type": "image", "source": {"type": "url", "url": "https://example.com/a.png"}}
        ]}
    ]});
    let answer = json!({"type": "message", "content": [
        {"type": "redacted_thinking", "data": "abc"},
        {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {}},
        {"type": "text", "text": "hello"},
        {"type": "tool_use", "id": "t1", "name": "Read", "input": {"p": 1}}
    ]});
    let second = json!({"model": "claude-sonnet-4-5", "system": "sys", "messages": [
        {"role": "user", "content": [
            {"type": "text", "text": "hi"},
            {"type": "image", "source": {"type": "url", "url": "https://example.com/a.png"}}
        ]},
        {"role": "assistant", "content": answer["content"]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": [
            {"type": "text", "text": "key sk-ant-api03-SECRETSECRETSECRET"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": png}}
        ]}]}
    ]});
    let (_d, app) = app(
        &[turn(1), turn(2)],
        &[(1, Some(first), Some(answer)), (2, Some(second), None)],
    );

    let (st, t) = get(&app, "/sessions/s1/transcript").await;
    assert_eq!(st, StatusCode::OK, "{t}");
    assert_eq!(
        t,
        json!({
            "session": "s1",
            "system": "sys",
            "total_turns": 2,
            // 第二轮没有回答（不会再被改写），两轮都早就结束了
            "settled_turns": 2,
            "turns": [
                {
                    "id": "1",
                    "restart": false,
                    "system_changed": null,
                    "input": [{"role": "user", "parts": [
                        {"kind": "text", "text": "hi"},
                        {"kind": "image", "media_type": null, "bytes": null}
                    ]}],
                    "output": [
                        {"kind": "thinking", "text": ""},
                        {"kind": "other", "label": "server_tool_use"},
                        {"kind": "text", "text": "hello"},
                        {"kind": "tool_call", "id": "t1", "name": "Read", "input": "{\"p\":1}"}
                    ],
                    "gaps": []
                },
                {
                    "id": "2",
                    "restart": false,
                    "system_changed": null,
                    "input": [{"role": "tool", "parts": [
                        {"kind": "tool_result", "call_id": "t1", "text": "key sk-an…CRET", "is_error": false},
                        {"kind": "image", "media_type": "image/png", "bytes": 6}
                    ]}],
                    "output": [],
                    "gaps": ["response_missing"]
                }
            ]
        })
    );

    // 和会话详情是同样的请求、同样的顺序
    let (_, detail) = get(&app, "/sessions/s1").await;
    let ids: Vec<String> = detail["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].to_string())
        .collect();
    assert_eq!(ids, ["1", "2"]);
}

/// 没有这次会话：404，和会话详情同一个码
#[tokio::test]
async fn an_unknown_session_is_not_found() {
    let (_d, app) = app(&[turn(1)], &[]);
    let (st, body) = get(&app, "/sessions/nope/transcript").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "control.session_not_found");
    assert_eq!(body["args"]["id"], "nope");
    let (st, body) = get(&app, "/sessions/nope").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "control.session_not_found");
}

/// 正文都清掉了的会话照样有它的每一轮，每一轮说出缺了什么
#[tokio::test]
async fn a_session_whose_bodies_are_gone_still_lists_its_turns() {
    let (_d, app) = app(&[turn(1), turn(2)], &[]);
    let (st, t) = get(&app, "/sessions/s1/transcript").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(t["system"], Value::Null);
    for x in t["turns"].as_array().unwrap() {
        assert_eq!(
            x["gaps"],
            json!(["request_missing", "response_missing"]),
            "{x}"
        );
        assert_eq!(x["input"], json!([]));
    }
}

/// `from_turn`：只要从那一轮起的那些，形状不变，`total_turns` 说一共几轮。比总轮数还大的
/// 是空的。不给和给 0 一样，是整段
#[tokio::test]
async fn from_turn_gives_the_turns_from_there_on() {
    let first =
        json!({"model": "claude-sonnet-4-5", "messages": [{"role": "user", "content": "一"}]});
    let answer = json!({"type": "message", "content": [{"type": "text", "text": "回一"}]});
    let second = json!({"model": "claude-sonnet-4-5", "messages": [
        {"role": "user", "content": "一"},
        {"role": "assistant", "content": "回一"},
        {"role": "user", "content": "二"}
    ]});
    let (_d, app) = app(
        &[turn(1), turn(2)],
        &[(1, Some(first), Some(answer)), (2, Some(second), None)],
    );

    let (st, all) = get(&app, "/sessions/s1/transcript").await;
    assert_eq!(st, StatusCode::OK, "{all}");
    let (_, zero) = get(&app, "/sessions/s1/transcript?from_turn=0").await;
    assert_eq!(zero, all);

    let (st, tail) = get(&app, "/sessions/s1/transcript?from_turn=1").await;
    assert_eq!(st, StatusCode::OK, "{tail}");
    assert_eq!(tail["total_turns"], 2);
    assert_eq!(tail["settled_turns"], all["settled_turns"]);
    assert_eq!(tail["system"], all["system"]);
    assert_eq!(tail["turns"], json!([all["turns"][1]]));
    assert_eq!(tail["turns"][0]["id"], "2");

    let (st, none) = get(&app, "/sessions/s1/transcript?from_turn=9").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(none["total_turns"], 2);
    assert_eq!(none["turns"], json!([]));

    // 不是数的 `from_turn` 是请求写错了
    let (st, body) = get(&app, "/sessions/s1/transcript?from_turn=x").await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
}
