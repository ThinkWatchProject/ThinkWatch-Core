//! 交接时的 WebSocket：旧 core 排空开始后，Responses 的连接**答完手上这一轮**以 1012（服务重启）
//! 关闭，闲着的当场关闭 —— 客户端重连，连到新 core 上。

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tw_config::{Client, Config, Provider};

/// 像 Responses 的 WebSocket 那样回答的上游：每个 `response.create` 回 created、一段文字，等
/// `hold` 变成 true 再回 completed
async fn responder(hold: tokio::sync::watch::Receiver<bool>) -> SocketAddr {
    let app = Router::new().route(
        "/backend-api/codex/responses",
        axum::routing::any(move |ws: WebSocketUpgrade| {
            let hold = hold.clone();
            async move { ws.on_upgrade(move |sock| answer(sock, hold)) }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

async fn answer(mut sock: WebSocket, mut hold: tokio::sync::watch::Receiver<bool>) {
    let mut n = 0;
    while let Some(Ok(m)) = sock.recv().await {
        if !matches!(m, Message::Text(_)) {
            continue;
        }
        n += 1;
        let id = format!("resp_{n}");
        let head = [
            serde_json::json!({"type":"response.created","response":{"id":id,"status":"in_progress","model":"gpt-5","output":[]}}),
            serde_json::json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg","delta":"hello"}),
        ];
        for f in head {
            if sock
                .send(Message::Text(f.to_string().into()))
                .await
                .is_err()
            {
                return;
            }
        }
        while !*hold.borrow_and_update() {
            if hold.changed().await.is_err() {
                return;
            }
        }
        let done = serde_json::json!({"type":"response.completed","response":{"id":id,"status":"completed","model":"gpt-5","output":[],"usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}});
        if sock
            .send(Message::Text(done.to_string().into()))
            .await
            .is_err()
        {
            return;
        }
    }
}

async fn gateway(up: SocketAddr) -> (SocketAddr, tw_gateway::AppState) {
    let cfg = Config {
        version: 1,
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
    let addr = tw_gateway::serve(state.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    (addr, state)
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
    let (s, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    s
}

fn create() -> WsMsg {
    WsMsg::Text(
        serde_json::json!({
            "type": "response.create",
            "model": "gpt-5",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
        })
        .to_string()
        .into(),
    )
}

async fn next(c: &mut Socket) -> WsMsg {
    tokio::time::timeout(Duration::from_secs(5), c.next())
        .await
        .expect("nothing came")
        .expect("the connection ended without a close frame")
        .unwrap()
}

/// 读到这一轮的第一段文字为止：上游已经在回答这一轮了
async fn until_text(c: &mut Socket) {
    loop {
        if let WsMsg::Text(t) = next(c).await
            && t.contains("output_text.delta")
        {
            return;
        }
    }
}

/// 读文字帧，直到关闭帧：交回读到的每一帧的 `type`，和关闭码
async fn until_close(c: &mut Socket) -> (Vec<String>, Option<CloseCode>) {
    let mut kinds = Vec::new();
    loop {
        match next(c).await {
            WsMsg::Text(t) => {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                kinds.push(v["type"].as_str().unwrap_or_default().to_string());
            }
            WsMsg::Close(f) => return (kinds, f.map(|f| f.code)),
            _ => {}
        }
    }
}

/// 闲着的连接：排空一开始就以 1012 关闭
#[tokio::test]
async fn an_idle_connection_is_closed_with_service_restart_when_draining_begins() {
    let (_tx, hold) = tokio::sync::watch::channel(true);
    let up = responder(hold).await;
    let (gw, state) = gateway(up).await;
    let mut c = connect(gw).await;
    // 先答完一轮：连接是活的，只是此刻闲着
    c.send(create()).await.unwrap();
    loop {
        if let WsMsg::Text(t) = next(&mut c).await
            && t.contains("response.completed")
        {
            break;
        }
    }

    let _rx = state.begin_drain();
    let (kinds, code) = until_close(&mut c).await;
    assert!(kinds.is_empty(), "{kinds:?}");
    assert_eq!(code, Some(CloseCode::Restart));
}

/// 在答的连接：这一轮照常答完（客户端收到 completed），然后才以 1012 关闭
#[tokio::test]
async fn a_connection_in_the_middle_of_a_turn_finishes_it_before_closing() {
    let (tx, hold) = tokio::sync::watch::channel(false);
    let up = responder(hold).await;
    let (gw, state) = gateway(up).await;
    let mut c = connect(gw).await;
    c.send(create()).await.unwrap();
    until_text(&mut c).await;

    let _rx = state.begin_drain();
    // 排空开始了，这一轮还没答完：连接不能关
    assert!(
        tokio::time::timeout(Duration::from_millis(200), c.next())
            .await
            .is_err(),
        "这一轮还没答完，连接就有动静了"
    );
    tx.send(true).unwrap();
    let (kinds, code) = until_close(&mut c).await;
    assert_eq!(kinds, ["response.completed"]);
    assert_eq!(code, Some(CloseCode::Restart));
}
