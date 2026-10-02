//! WebSocket 那条路上的插件：一次 `response.create` 一次请求钩子，上游的每一次回答
//! 一组回答钩子。和 HTTP 那条路同样的位置、同样的规矩。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tw_api::Permission;
use tw_config::{Client, Config, Listen, Provider};
use tw_gateway::plugin::host::double;
use tw_gateway::plugin::host::double::{Closures, Double};
use tw_gateway::plugin::{
    Active, Invocation, PluginSet, RequestOutcome, RunError, ToolCallOutcome,
};

/// 假上游：记下收到的每一帧，每个 `response.create` 回一次完整的回答
async fn upstream() -> (SocketAddr, Arc<Mutex<Vec<Value>>>) {
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let app = Router::new()
        .route(
            "/backend-api/codex/responses",
            axum::routing::any(
                |State(seen): State<Arc<Mutex<Vec<Value>>>>, ws: WebSocketUpgrade| async move {
                    ws.on_upgrade(move |sock| answer(sock, seen))
                },
            ),
        )
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, seen)
}

async fn answer(mut sock: WebSocket, seen: Arc<Mutex<Vec<Value>>>) {
    while let Some(Ok(m)) = sock.recv().await {
        let Message::Text(t) = m else { continue };
        let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
        let n = {
            let mut s = seen.lock().unwrap();
            s.push(v);
            s.len()
        };
        let id = format!("resp_{n}");
        let mut seq = 0;
        let mut ev = |kind: &str, mut v: Value| {
            v["type"] = json!(kind);
            v["sequence_number"] = json!(seq);
            seq += 1;
            v.to_string()
        };
        let frames = vec![
            ev(
                "response.created",
                json!({"response":{"id":id,"status":"in_progress","output":[]}}),
            ),
            ev(
                "response.output_item.added",
                json!({"output_index":0,"item":{"type":"message","id":"msg","role":"assistant","content":[]}}),
            ),
            ev(
                "response.output_text.delta",
                json!({"output_index":0,"content_index":0,"item_id":"msg","delta":"hel"}),
            ),
            ev(
                "response.output_text.delta",
                json!({"output_index":0,"content_index":0,"item_id":"msg","delta":"lo"}),
            ),
            ev(
                "response.output_text.done",
                json!({"output_index":0,"content_index":0,"item_id":"msg","text":"hello"}),
            ),
            ev(
                "response.output_item.done",
                json!({"output_index":0,"item":{"type":"message","id":"msg","role":"assistant","content":[{"type":"output_text","text":"hello"}]}}),
            ),
            ev(
                "response.completed",
                json!({"response":{"id":id,"status":"completed","output":[{"type":"message","id":"msg","role":"assistant","content":[{"type":"output_text","text":"hello"}]}]}}),
            ),
        ];
        for f in frames {
            if sock.send(Message::Text(f.into())).await.is_err() {
                return;
            }
        }
    }
}

async fn gateway(up: SocketAddr, entries: Vec<Arc<Active>>) -> SocketAddr {
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "codex".into(),
            key: "tw-wskey".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "up".into(),
            base_url: format!("http://{up}"),
            key: Some("sk-upstream".into()),
            protocol: Some(tw_config::Protocol::OpenaiResponses),
            ..Default::default()
        }],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    state.swap_plugins(PluginSet::new(entries));
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;
    addr
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(gw: SocketAddr) -> Socket {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = format!("ws://{gw}/backend-api/codex/responses")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("x-api-key", "tw-wskey".parse().unwrap());
    req.headers_mut()
        .insert("originator", "codex_cli_rs".parse().unwrap());
    tokio_tungstenite::connect_async(req).await.unwrap().0
}

fn create(text: &str) -> WsMsg {
    WsMsg::Text(
        json!({
            "type": "response.create",
            "model": "gpt-5.1-codex",
            "instructions": "You are Codex.",
            "input": [{ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] }],
            "stream": true
        })
        .to_string()
        .into(),
    )
}

/// 收到这一次回答的结尾（或者连接断了）为止的每一帧
async fn one_answer(c: &mut Socket) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(Some(Ok(m))) = tokio::time::timeout(Duration::from_secs(3), c.next()).await {
        let WsMsg::Text(t) = m else { continue };
        let t = t.to_string();
        let end = t.contains("\"response.completed\"") || t.contains("\"response.failed\"");
        out.push(t);
        if end {
            break;
        }
    }
    out
}

fn entry(id: &str, d: Double) -> Arc<Active> {
    let mut a = double::active(id, d);
    a.name = format!("Plugin {id}");
    Arc::new(a)
}

#[tokio::test]
async fn each_response_create_goes_through_the_request_hook_and_each_answer_through_the_reply_hook()
{
    let (up, seen) = upstream().await;
    let both = Double::new("both")
        .permit(&[Permission::System, Permission::ReplyText])
        .on_request(|mut view, ctx| {
            assert_eq!(ctx["format"], "openai_responses");
            assert_eq!(ctx["client"], "codex");
            view["system"] = json!("You are Codex. Today is Friday.");
            Invocation::ok(RequestOutcome::Changed(view))
        })
        .on_text(|t| Some(t.to_uppercase()));
    let gw = gateway(up, vec![entry("both", both)]).await;
    let mut c = connect(gw).await;
    for round in 0..2 {
        c.send(create("hi")).await.unwrap();
        let frames = one_answer(&mut c).await;
        let text: String = frames
            .iter()
            .filter_map(|f| serde_json::from_str::<Value>(f).ok())
            .filter(|v| v["type"] == "response.output_text.delta")
            .filter_map(|v| v["delta"].as_str().map(str::to_string))
            .collect();
        assert_eq!(text, "HELLO", "round {round}: {frames:?}");
        let completed: Value = serde_json::from_str(frames.last().unwrap()).unwrap();
        assert_eq!(
            completed["response"]["output"][0]["content"][0]["text"],
            "HELLO"
        );
        let sent = seen.lock().unwrap()[round].clone();
        assert_eq!(sent["instructions"], "You are Codex. Today is Friday.");
        // 不是插件改的字段原样
        assert_eq!(sent["type"], "response.create");
    }
}

#[tokio::test]
async fn a_rejected_response_create_cuts_the_connection_with_the_reason() {
    let (up, seen) = upstream().await;
    let no = Double::new("no")
        .permit(&[Permission::Messages])
        .on_request(|_, _| Invocation::ok(RequestOutcome::Rejected("blocked word".into())));
    let gw = gateway(up, vec![entry("no", no)]).await;
    let mut c = connect(gw).await;
    c.send(create("hi")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert!(
        frames
            .iter()
            .any(|f| f
                .contains("[ThinkWatch] Plugin `Plugin no` refused this request: blocked word")),
        "{frames:?}"
    );
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_failing_reply_plugin_fails_that_answer_and_the_connection_stays() {
    let (up, _) = upstream().await;
    let flaky = Double::new("flaky")
        .permit(&[Permission::ReplyText])
        .on_reply(true, false, false, |_| {
            Ok(Box::new(Closures {
                text: Box::new(|_| Invocation::err(RunError::CpuLimit)),
                end: Box::new(|| Invocation::ok(None)),
                tool: Box::new(|_| Invocation::ok(ToolCallOutcome::Unchanged)),
            }))
        });
    let gw = gateway(up, vec![entry("flaky", flaky)]).await;
    let mut c = connect(gw).await;
    for _ in 0..2 {
        c.send(create("hi")).await.unwrap();
        let frames = one_answer(&mut c).await;
        let last: Value = serde_json::from_str(frames.last().unwrap()).unwrap();
        assert_eq!(last["type"], "response.failed", "{frames:?}");
        assert!(
            last["response"]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("failed while handling the answer"),
            "{last}"
        );
        assert!(!frames.iter().any(|f| f.contains("hel")), "{frames:?}");
    }
}
