//! 上游回了错误、原样交给客户端的那一轮**是失败的**，界面读的每一处都这么说。
//!
//! 整条路走一遍：真的网关、一个按最后一句话回答 / 回 400 / 吐一帧就不再说话的假上游、
//! 存储层写进临时目录、控制面。同一次会话里三轮：成功的、上游回了 400 的、客户端中途
//! 走掉的。然后从界面读的几个端点看它们：会话详情（`TurnView`）、流量（`/history`）、
//! 概览（`/summary`）、会话列表、对话记录。
//!
//! 以前上游的 4xx 落库时 `error` 是空的：会话详情把那一轮当成成功，对话里只剩用户的那句
//! 话，没有回答，也没有原因。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::StreamExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

const TOO_LONG: &str = "prompt is too long: 212000 tokens > 200000 maximum";

const MESSAGE_START: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-4-5\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n";

/// 假上游，看最后一句话：`too long` 回 400（Anthropic 的错误体），`slow` 吐一帧之后不再
/// 说话（客户端会走掉），别的好好回答
async fn upstream() -> SocketAddr {
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(|body: bytes::Bytes| async move {
            let v: Value = serde_json::from_slice(&body).unwrap_or_default();
            let last = v["messages"]
                .as_array()
                .and_then(|m| m.last())
                .and_then(|m| m["content"].as_str())
                .unwrap_or_default()
                .to_string();
            let answer = axum::response::Response::builder();
            match last.as_str() {
                "too long" => answer
                    .status(400)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"type": "error", "error": {"type": "invalid_request_error", "message": TOO_LONG}})
                            .to_string(),
                    ))
                    .unwrap(),
                "slow" => {
                    let first = futures::stream::once(async {
                        Ok::<_, std::io::Error>(bytes::Bytes::from_static(MESSAGE_START.as_bytes()))
                    });
                    let stall = futures::stream::once(async {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        Ok(bytes::Bytes::new())
                    });
                    answer
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(first.chain(stall)))
                        .unwrap()
                }
                _ => answer
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"type": "message", "model": "claude-sonnet-4-5", "role": "assistant",
                               "content": [{"type": "text", "text": "好"}], "stop_reason": "end_turn",
                               "usage": {"input_tokens": 10, "output_tokens": 5}})
                        .to_string(),
                    ))
                    .unwrap(),
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

struct World {
    _dir: tempfile::TempDir,
    blobs: std::path::PathBuf,
    gw: SocketAddr,
    store: Arc<tokio::sync::Mutex<tw_store::Recorder>>,
    app: axum::Router,
}

/// 网关、存储层、控制面，**和 `twcore` 一样接**（正文也落盘：对话记录要读它）。
async fn world() -> World {
    let up = upstream().await;
    let d = tempfile::tempdir().unwrap();
    let yaml = format!(
        "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\n\
         clients:\n  - name: 我\n    key: tw-k\n\
         providers:\n  - name: 中转\n    base_url: http://{up}\n    key: sk-upstream\n    protocol: anthropic\n"
    );
    let p = d.path().join("config.yaml");
    std::fs::write(&p, &yaml).unwrap();
    let cfg: tw_config::Config = serde_yaml_ng::from_str(&yaml).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();

    let (sink, mut bodies) = tw_gateway::bodies::channel();
    gw.set_body_sink(sink);
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        while let Some(b) = bodies.recv().await {
            let disk = tokio::task::spawn_blocking(move || b.for_disk())
                .await
                .unwrap();
            let which = match disk.kind {
                tw_gateway::bodies::BodyKind::Request => tw_store::Which::Request,
                tw_gateway::bodies::BodyKind::Response => tw_store::Which::Response,
            };
            let stored = tw_store::StoredBody {
                id: disk.id,
                at_ms: disk.at_ms,
                which,
                body: disk.body,
                original_len: disk.original_len,
            };
            if tx.send(stored).await.is_err() {
                return;
            }
        }
    });
    let blobs = d.path().join("blobs");
    let store = tw_store::task::spawn(
        tw_store::Recorder::new(
            tw_store::Db::open(&d.path().join("data.db")).unwrap(),
            tw_store::Blobs::new(blobs.clone()),
            tw_pricing::shared(tw_pricing::PriceBook::builtin().unwrap()),
        ),
        gw.bus.subscribe(),
        rx,
    );
    let state = ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), gw.bus.clone())),
        gateway: gw.clone(),
        store: Some(store.clone()),
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    };
    let app = tw_control::router(state);
    let addr = tw_gateway::serve(gw, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    World {
        _dir: d,
        blobs,
        gw: addr,
        store,
        app,
    }
}

/// 同一次会话的一轮：开头那句话都一样（会话按它认）。`later` 是第一轮之后带着的：上一轮
/// 的回答，和这一轮新说的
fn turn(later: &[&str], stream: bool) -> String {
    let mut messages = vec![json!({"role": "user", "content": "把这个函数改短一点"})];
    for (i, text) in later.iter().enumerate() {
        let role = if i % 2 == 0 { "assistant" } else { "user" };
        messages.push(json!({"role": role, "content": text}));
    }
    json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 64,
        "stream": stream,
        "messages": messages,
    })
    .to_string()
}

fn send(w: &World, body: String) -> reqwest::RequestBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{}/v1/messages", w.gw))
        .header("x-api-key", "tw-k")
        .header("content-type", "application/json")
        .body(body)
}

/// 等第 `n` 条请求落库、它的请求体和响应体都落了盘，交回它的号
async fn landed(w: &World, n: i64) -> i64 {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let id = {
            let g = w.store.lock().await;
            (g.db().count().unwrap() >= n).then(|| g.db().recent(None, 1).unwrap()[0].id)
        };
        if let Some(id) = id {
            let names = stored_names(&w.blobs);
            if [format!("{id}.req"), format!("{id}.res")]
                .iter()
                .all(|f| names.contains(f))
            {
                return id;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "第 {n} 条请求没有落库，或者正文没有落盘"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 正文目录里每一个文件的名字
fn stored_names(root: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|day| {
            std::fs::read_dir(day.path())
                .into_iter()
                .flatten()
                .flatten()
        })
        .map(|f| f.file_name().to_string_lossy().to_string())
        .collect()
}

async fn get(app: &axum::Router, path: &str) -> Value {
    let r = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK, "{path}");
    let b = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&b).unwrap()
}

#[tokio::test]
async fn an_error_the_upstream_answered_is_a_failed_turn_everywhere_the_ui_reads() {
    let w = world().await;

    // 第一轮：好好回答了
    let r = send(&w, turn(&[], false)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let done = landed(&w, 1).await;

    // 第二轮：上游回 400，原话原样交给客户端
    let r = send(&w, turn(&["好", "too long"], false))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains(TOO_LONG));
    let refused = landed(&w, 2).await;

    // 第三轮：客户端拿到第一帧就走了（Claude Code 里按一下 Esc）
    let r = send(&w, turn(&["好", "slow"], true)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let mut body = r.bytes_stream();
    body.next().await.unwrap().unwrap();
    drop(body);
    let left = landed(&w, 3).await;

    // ── 会话详情：每一轮带着状态码；上游回了错误的那一轮是失败，原因是上游的原话
    let sessions = get(&w.app, "/sessions").await;
    let sessions = sessions.as_array().unwrap();
    assert_eq!(sessions.len(), 1, "三轮该是同一次会话：{sessions:?}");
    let s = &sessions[0];
    assert_eq!(
        (s["turns"].as_u64(), s["errors"].as_u64()),
        (Some(3), Some(1)),
        "{s}"
    );
    let id = s["id"].as_str().unwrap();
    let detail = get(&w.app, &format!("/sessions/{id}")).await;
    let turns = detail["turns"].as_array().unwrap();
    let ids: Vec<i64> = turns.iter().map(|t| t["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, [done, refused, left]);

    let (ok, failed, cancelled) = (&turns[0], &turns[1], &turns[2]);
    assert_eq!(ok["status"], 200, "{ok}");
    assert_eq!(ok["error"], Value::Null, "{ok}");
    assert_eq!(ok["cancelled"], false);

    assert_eq!(failed["status"], 400, "{failed}");
    assert_eq!(failed["cancelled"], false);
    let why = &failed["error"];
    assert_eq!(why["code"], "gw.upstream.status_message", "{why}");
    assert_eq!(why["args"]["upstream"], "中转");
    assert_eq!(why["args"]["status"], "400");
    assert_eq!(why["args"]["message"], TOO_LONG);
    assert_eq!(
        why["text"].as_str().unwrap(),
        format!("Upstream `中转` answered 400: {TOO_LONG}")
    );

    // 取消照旧：不是失败，带着响应头那一刻的状态码
    assert_eq!(cancelled["cancelled"], true, "{cancelled}");
    assert_eq!(
        cancelled["error"],
        Value::Null,
        "取消被算成了失败：{cancelled}"
    );
    assert_eq!(cancelled["status"], 200);

    // ── 流量：同一批行，同一个说法
    let history = get(&w.app, "/history").await;
    let row = |rid: i64| {
        history
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"].as_i64() == Some(rid))
            .unwrap_or_else(|| panic!("流量里没有 {rid}：{history}"))
            .clone()
    };
    let (ok, failed, cancelled) = (row(done), row(refused), row(left));
    assert_eq!((&ok["status"], &ok["error"]), (&json!(200), &Value::Null));
    assert_eq!(failed["status"], 400);
    assert_eq!(failed["error"], *why);
    assert_eq!(
        (&cancelled["cancelled"], &cancelled["error"]),
        (&json!(true), &Value::Null)
    );

    // ── 概览：失败一条，取消的不算
    let summary = get(&w.app, "/summary?from_ms=0&to_ms=9999999999999").await;
    assert_eq!(summary["requests"], 3, "{summary}");
    assert_eq!(summary["failed"], 1, "{summary}");

    // ── 对话：失败的那一轮没有回答，也不算缺 —— 原因在会话详情里
    let transcript = get(&w.app, &format!("/sessions/{id}/transcript")).await;
    // 对话里的请求号是字符串
    let key = refused.to_string();
    let t = transcript["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == key.as_str())
        .unwrap_or_else(|| panic!("对话里没有 {refused}：{transcript}"))
        .clone();
    assert_eq!(t["output"], json!([]), "{t}");
    assert_eq!(t["gaps"], json!([]), "{t}");
    assert_eq!(
        t["input"][0]["parts"][0],
        json!({"kind": "text", "text": "too long"}),
        "{t}"
    );
}
