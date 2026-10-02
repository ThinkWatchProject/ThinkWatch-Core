//! WebSocket 那一路（Codex 的 Responses WebSocket）：假上游和客户端。
//!
//! 客户端每发一帧 `response.create`，假上游回一整串 Responses 事件：created、一段
//! 文字（一个字符一帧）、可选的一个函数调用（参数分三片）、completed。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};

pub const WS_PATH: &str = "/backend-api/codex/responses";

/// 假上游对每一帧 `response.create` 的回答
#[derive(Clone, Debug)]
pub enum WsAnswer {
    /// 一段文字
    Text(String),
    /// 把这一帧里第一段带 `<<` 或 `sk-ant-` 的字符串原样说一遍
    Echo,
    /// 一句话，接一个函数调用
    Call { name: String, arguments: Value },
}

#[derive(Clone)]
pub struct WsUpstream {
    pub addr: SocketAddr,
    seen: Arc<Mutex<Vec<String>>>,
}

#[derive(Clone)]
struct WsState {
    seen: Arc<Mutex<Vec<String>>>,
    answer: WsAnswer,
}

impl WsUpstream {
    pub async fn start(answer: WsAnswer) -> WsUpstream {
        let seen: Arc<Mutex<Vec<String>>> = Default::default();
        let st = WsState {
            seen: seen.clone(),
            answer,
        };
        let app = Router::new()
            .route(
                WS_PATH,
                axum::routing::any(
                    |State(st): State<WsState>, ws: WebSocketUpgrade| async move {
                        ws.on_upgrade(move |sock| serve(sock, st))
                    },
                ),
            )
            .with_state(st);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        WsUpstream { addr, seen }
    }

    /// 收到的每一帧，原文
    pub fn frames(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

async fn serve(mut sock: WebSocket, st: WsState) {
    while let Some(Ok(m)) = sock.recv().await {
        let Message::Text(t) = m else { continue };
        let n = {
            let mut seen = st.seen.lock().unwrap();
            seen.push(t.to_string());
            seen.len()
        };
        let text = match &st.answer {
            WsAnswer::Text(s) => s.clone(),
            WsAnswer::Echo => t
                .split('"')
                .find(|p| p.contains("<<") || p.contains("sk-ant-"))
                .unwrap_or("（没看到）")
                .to_string(),
            WsAnswer::Call { .. } => "我看一下。".to_string(),
        };
        for f in events(n, &text, &st.answer) {
            if sock
                .send(Message::Text(f.to_string().into()))
                .await
                .is_err()
            {
                return;
            }
        }
    }
}

/// 一次 Responses 回答的全部事件
fn events(n: usize, text: &str, answer: &WsAnswer) -> Vec<Value> {
    let id = format!("resp_{n}");
    let msg = json!({ "type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
                      "content": [{ "type": "output_text", "text": text, "annotations": [] }] });
    let mut out = vec![
        json!({ "type": "response.created", "response": { "id": id, "status": "in_progress", "output": [] } }),
        json!({ "type": "response.output_item.added", "output_index": 0,
                "item": { "type": "message", "id": "msg_1", "role": "assistant", "status": "in_progress", "content": [] } }),
        json!({ "type": "response.content_part.added", "item_id": "msg_1", "output_index": 0, "content_index": 0,
                "part": { "type": "output_text", "text": "", "annotations": [] } }),
    ];
    for c in text.chars() {
        out.push(
            json!({ "type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0,
                         "content_index": 0, "delta": c.to_string() }),
        );
    }
    out.push(
        json!({ "type": "response.output_text.done", "item_id": "msg_1", "output_index": 0,
                     "content_index": 0, "text": text }),
    );
    out.push(json!({ "type": "response.content_part.done", "item_id": "msg_1", "output_index": 0,
                     "content_index": 0, "part": { "type": "output_text", "text": text, "annotations": [] } }));
    out.push(json!({ "type": "response.output_item.done", "output_index": 0, "item": msg }));
    let mut output = vec![msg];
    if let WsAnswer::Call { name, arguments } = answer {
        let args = arguments.to_string();
        let item = |status: &str, args: &str| {
            json!({ "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": name,
                    "arguments": args, "status": status })
        };
        out.push(json!({ "type": "response.output_item.added", "output_index": 1, "item": item("in_progress", "") }));
        let chars: Vec<char> = args.chars().collect();
        for part in chars.chunks(chars.len().div_ceil(3).max(1)) {
            out.push(
                json!({ "type": "response.function_call_arguments.delta", "item_id": "fc_1",
                             "output_index": 1, "delta": part.iter().collect::<String>() }),
            );
        }
        out.push(
            json!({ "type": "response.function_call_arguments.done", "item_id": "fc_1",
                         "output_index": 1, "arguments": args }),
        );
        out.push(json!({ "type": "response.output_item.done", "output_index": 1, "item": item("completed", &args) }));
        output.push(item("completed", &args));
    }
    out.push(json!({ "type": "response.completed",
                     "response": { "id": id, "status": "completed", "output": output,
                                   "usage": { "input_tokens": 10, "output_tokens": 5, "total_tokens": 15 } } }));
    out
}

/// 只有这一个 WebSocket 上游的配置
pub fn ws_config(up: &WsUpstream, security: tw_config::Security) -> tw_config::Config {
    tw_config::Config {
        version: 1,
        listen: tw_config::Listen::default(),
        clients: vec![tw_config::Client {
            name: "codex".into(),
            key: super::KEY.into(),
            ..Default::default()
        }],
        providers: vec![tw_config::Provider {
            name: "relay".into(),
            base_url: format!("http://{}", up.addr),
            key: Some("sk-upstream".into()),
            protocol: Some(tw_config::Protocol::OpenaiResponses),
            ..Default::default()
        }],
        security,
        ..Default::default()
    }
}

/// 网关那一头的客户端
pub struct WsClient {
    sock: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl WsClient {
    pub async fn connect(gw: SocketAddr, key: &str) -> WsClient {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut req = format!("ws://{gw}{WS_PATH}").into_client_request().unwrap();
        req.headers_mut().insert("x-api-key", key.parse().unwrap());
        let (sock, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        WsClient { sock }
    }

    /// 发一帧 `response.create`，收这次回答的全部帧，直到 completed / failed，或者
    /// 连接断了、半秒内没有新帧
    pub async fn ask(&mut self, frame: Value) -> Vec<String> {
        self.sock
            .send(tokio_tungstenite::tungstenite::Message::Text(
                frame.to_string().into(),
            ))
            .await
            .unwrap();
        let mut got = Vec::new();
        while let Ok(Some(Ok(m))) =
            tokio::time::timeout(Duration::from_millis(1500), self.sock.next()).await
        {
            let Ok(t) = m.into_text() else { continue };
            let t = t.to_string();
            let end = t.contains("\"response.completed\"") || t.contains("\"response.failed\"");
            got.push(t);
            if end {
                break;
            }
        }
        got
    }
}

/// 客户端收到的文字（`response.output_text.delta` 拼起来）
pub fn ws_text(frames: &[String]) -> String {
    frames
        .iter()
        .filter_map(|f| serde_json::from_str::<Value>(f).ok())
        .filter(|v| v["type"] == "response.output_text.delta")
        .filter_map(|v| v["delta"].as_str().map(str::to_string))
        .collect()
}

/// 客户端收到的函数调用：`(名字, 参数原文)`，取 `response.output_item.done` 里的
pub fn ws_calls(frames: &[String]) -> Vec<(String, String)> {
    frames
        .iter()
        .filter_map(|f| serde_json::from_str::<Value>(f).ok())
        .filter(|v| {
            v["type"] == "response.output_item.done" && v["item"]["type"] == "function_call"
        })
        .map(|v| {
            (
                v["item"]["name"].as_str().unwrap_or_default().to_string(),
                v["item"]["arguments"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}
