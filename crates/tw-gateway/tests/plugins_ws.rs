//! WebSocket 那条路上的插件：一次 `response.create` 一次请求钩子，上游的每一次回答
//! 一组回答钩子。和 HTTP 那条路的一跳同样的位置、同样的规矩：这条路只有一跳（升级时
//! 连定的那一家），`ctx.upstream` 就是它，运行记在第 0 跳上。

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
    Active, Broken, Invocation, PluginSet, RequestOutcome, RunError, RunRecord,
    State as PluginState, ToolCallOutcome,
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
    gateway_with(up, entries, tw_config::Security::default())
        .await
        .0
}

/// 网关，连同记下的每一次插件运行
async fn gateway_with(
    up: SocketAddr,
    entries: Vec<Arc<Active>>,
    security: tw_config::Security,
) -> (SocketAddr, Arc<Mutex<Vec<RunRecord>>>) {
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
        security,
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    state.swap_plugins(PluginSet::new(entries));
    let runs: Arc<Mutex<Vec<RunRecord>>> = Arc::default();
    let (tx, mut rx) = tokio::sync::mpsc::channel(tw_gateway::plugin::RUN_CHANNEL_CAP);
    state.set_plugin_sink(tx);
    let r = runs.clone();
    tokio::spawn(async move {
        while let Some(rec) = rx.recv().await {
            r.lock().unwrap().push(rec);
        }
    });
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;
    (addr, runs)
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
    entry_with(id, d, |_| {})
}

fn entry_with(id: &str, d: Double, f: impl FnOnce(&mut Active)) -> Arc<Active> {
    let mut a = double::active(id, d);
    a.name = format!("Plugin {id}");
    f(&mut a);
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
            // 这条路只有一跳：上游是这条连接连的那一家，模型名就是这一帧写的
            assert_eq!(ctx["upstream"], "up");
            assert_eq!(ctx["model"], "gpt-5.1-codex");
            assert_eq!(ctx["requested_model"], "gpt-5.1-codex");
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

/// 范围按这条连接连的那一家算：只管别家的插件不跑，只管别家的坏插件也不拦；管这一家的
/// 照常跑，运行记在第 0 跳上，回答钩子的 `ctx` 和请求钩子的一样
#[tokio::test]
async fn scope_follows_the_upstream_of_the_connection() {
    let (up, seen) = upstream().await;
    let reply_ctx = Arc::new(Mutex::new(Value::Null));
    let rc = reply_ctx.clone();
    let here = Double::new("here")
        .permit(&[Permission::System, Permission::ReplyText])
        .on_request(|mut view, _| {
            view["system"] = json!("for up");
            Invocation::ok(RequestOutcome::Changed(view))
        })
        .on_reply(true, false, false, move |ctx| {
            *rc.lock().unwrap() = ctx;
            Ok(Box::new(Closures {
                text: Box::new(|_| Invocation::ok(None)),
                end: Box::new(|| Invocation::ok(None)),
                tool: Box::new(|_| Invocation::ok(ToolCallOutcome::Unchanged)),
            }))
        });
    let elsewhere = Double::new("elsewhere")
        .permit(&[Permission::System])
        .on_request(|_, _| panic!("ran for an upstream outside its scope"));
    let broken_elsewhere = {
        let mut a = double::active("old", Double::new("Old"));
        a.state = PluginState::Broken(Broken::Changed);
        a.scope.upstreams = vec!["relay-*".into()];
        Arc::new(a)
    };
    let (gw, runs) = gateway_with(
        up,
        vec![
            entry("here", here),
            entry_with("elsewhere", elsewhere, |a| {
                a.scope.upstreams = vec!["relay-*".into()]
            }),
            broken_elsewhere,
        ],
        Default::default(),
    )
    .await;
    let mut c = connect(gw).await;
    c.send(create("hi")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert!(
        frames.last().unwrap().contains("response.completed"),
        "{frames:?}"
    );
    assert_eq!(seen.lock().unwrap()[0]["instructions"], "for up");
    let ctx = reply_ctx.lock().unwrap().clone();
    assert_eq!(ctx["upstream"], "up");
    assert_eq!(ctx["model"], "gpt-5.1-codex");
    assert_eq!(ctx["requested_model"], "gpt-5.1-codex");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let runs: Vec<(String, String, u64)> = runs
        .lock()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r.run.plugin_id.clone(),
                r.run.hook.slug().to_string(),
                r.run.detail.as_ref().unwrap()["attempt"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        runs,
        [
            ("here".to_string(), "request".to_string(), 0),
            ("here".to_string(), "reply".to_string(), 0)
        ]
    );
}

/// 插件往 `response.create` 里加的内容照样过请求防护：拦下就切断，上游什么都没收到
#[tokio::test]
async fn content_a_plugin_adds_to_a_response_create_is_screened() {
    let (up, seen) = upstream().await;
    let adds = Double::new("adds")
        .permit(&[Permission::Messages])
        .on_request(|mut view, _| {
            view["messages"][0]["parts"][0]["text"] = json!("the forbidden-plan");
            Invocation::ok(RequestOutcome::Changed(view))
        });
    let security = tw_config::Security {
        content: tw_config::ContentPolicy {
            mode: tw_config::SecurityMode::Enforce,
            custom: vec![tw_config::CustomContentRule {
                name: "no plan".into(),
                pattern: "forbidden-plan".into(),
                matching: Default::default(),
                action: tw_config::ContentAction::Block,
                disabled: false,
            }],
            ..Default::default()
        },
        ..Default::default()
    };
    let (gw, _) = gateway_with(up, vec![entry("adds", adds)], security).await;
    let mut c = connect(gw).await;
    c.send(create("hi")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert!(
        frames.iter().any(|f| f.contains("no plan")),
        "the client was not told why: {frames:?}"
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "{:?}",
        seen.lock().unwrap()
    );
}

/// Realtime 那样的 WebSocket 上游：记下收到的每一帧，原样回一帧
async fn realtime_upstream() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let app = Router::new()
        .route(
            "/v1/realtime",
            axum::routing::any(
                |State(seen): State<Arc<Mutex<Vec<String>>>>, ws: WebSocketUpgrade| async move {
                    ws.on_upgrade(move |mut sock: WebSocket| async move {
                        while let Some(Ok(m)) = sock.recv().await {
                            let Message::Text(t) = m else { continue };
                            seen.lock().unwrap().push(t.to_string());
                            if sock.send(Message::Text(t)).await.is_err() {
                                return;
                            }
                        }
                    })
                },
            ),
        )
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, seen)
}

/// 不是 Responses 的 WebSocket（比如 Realtime 的 `/v1/realtime`）：不属于插件处理的任何一种
/// 请求，**所有插件都不管** —— 出错时拒绝的、跳过的都一样：接上，帧原样过去，什么都不记
#[tokio::test]
async fn a_websocket_plugins_do_not_handle_passes_through_unrecorded() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let look = || {
        Double::new("look")
            .permit(&[Permission::Messages])
            .on_request(|_, _| Invocation::ok(RequestOutcome::Unchanged))
    };
    let request = |gw: SocketAddr| {
        let mut req = format!("ws://{gw}/v1/realtime")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("x-api-key", "tw-wskey".parse().unwrap());
        req
    };
    let item = json!({ "type": "conversation.item.create",
                       "item": { "type": "message", "role": "user",
                                 "content": [{ "type": "input_text", "text": "secret plan" }] } })
    .to_string();

    for on_error in [tw_api::OnError::Reject, tw_api::OnError::Skip] {
        let (up, seen) = realtime_upstream().await;
        let (gw, runs) = gateway_with(
            up,
            vec![entry_with("look", look(), |a| a.on_error = on_error)],
            Default::default(),
        )
        .await;
        let (mut c, _) = tokio_tungstenite::connect_async(request(gw))
            .await
            .unwrap_or_else(|e| panic!("{on_error:?}: the upgrade was refused: {e}"));
        c.send(WsMsg::Text(item.clone().into())).await.unwrap();
        let back = tokio::time::timeout(Duration::from_secs(3), c.next())
            .await
            .expect("no echo")
            .unwrap()
            .unwrap();
        assert_eq!(back.into_text().unwrap().as_str(), item, "{on_error:?}");
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            std::slice::from_ref(&item),
            "{on_error:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(runs.lock().unwrap().is_empty(), "{on_error:?}");
    }
}
