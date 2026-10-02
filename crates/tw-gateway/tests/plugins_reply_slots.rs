//! 回答实例的名额，从客户端到假上游走一整圈。
//!
//! 一个回答钩子的实例从回答开始活到回答结束，整个进程同时活着的有上限（见
//! `tw_gateway::plugin::pool`）。这里证明两件事：名额满了按插件的 `on_error` 处置（拒绝是
//! 这个请求失败，跳过是这次回答原样过去）；名额**一定还得回来** —— 回答结束、客户端半路
//! 走了、上游半路断了、WebSocket 上一次回答完了或者连接断了。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocketUpgrade};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::watch;
use tw_api::{OnError, Permission, ReplyMode};
use tw_config::{Client, Config, Listen, Protocol, Provider};
use tw_gateway::plugin::host::double::{self, Double};
use tw_gateway::plugin::pool::Pool;
use tw_gateway::plugin::{Active, PluginSet, RunRecord};

fn ev(v: Value) -> String {
    format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap())
}

/// 一个流式回答的开头：到第一段文字为止
fn head() -> String {
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
    ]
    .concat()
}

/// 剩下的：第二段文字和收尾
fn tail() -> String {
    [
        ev(
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"there"}}),
        ),
        ev(json!({"type":"content_block_stop","index":0})),
        ev(
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
        ),
        ev(json!({"type":"message_stop"})),
    ]
    .concat()
}

/// 假上游：先吐开头，等 `release` 变成 true 再吐完 —— 一个还在说话的模型
async fn held(release: watch::Receiver<bool>) -> SocketAddr {
    let app = Router::new().fallback(axum::routing::post(move || {
        let mut rx = release.clone();
        async move {
            let s = async_stream::stream! {
                yield Ok::<_, std::io::Error>(bytes::Bytes::from(head()));
                while !*rx.borrow_and_update() {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
                yield Ok(bytes::Bytes::from(tail()));
            };
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from_stream(s))
                .unwrap()
        }
    }));
    listen(app).await
}

/// 假上游：吐完开头就把连接掐断（说好的长度没发完）
async fn breaking() -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 64 * 1024];
                let _ = s.read(&mut buf).await;
                let body = head();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len() + 900
                );
                let _ = s.write_all(resp.as_bytes()).await;
                let _ = s.flush().await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                drop(s);
            });
        }
    });
    addr
}

async fn listen(app: Router) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

struct Gw {
    addr: SocketAddr,
    state: tw_gateway::AppState,
    runs: Arc<Mutex<Vec<RunRecord>>>,
}

impl Gw {
    fn live(&self) -> usize {
        self.state.plugin_pool.live_replies()
    }

    /// 等活着的回答实例回到 `n` 个。等不到就失败
    async fn settles_at(&self, n: usize) {
        for _ in 0..100 {
            if self.live() == n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("{} reply instances are still alive, not {n}", self.live());
    }

    /// 这个插件每次出错时记下的消息码
    fn error_codes(&self, id: &str) -> Vec<String> {
        self.runs
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.run.plugin_id == id)
            .filter_map(|r| r.run.error.as_ref().map(|m| m.code.clone()))
            .collect()
    }
}

/// 网关：一家 `protocol` 的上游，回答实例的名额是 `slots` 个
async fn gateway(
    base: SocketAddr,
    protocol: Protocol,
    entries: Vec<Arc<Active>>,
    slots: usize,
) -> Gw {
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "claude-code".into(),
            key: "tw-testkey".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "up".into(),
            base_url: format!("http://{base}"),
            key: Some("sk-upstream".into()),
            protocol: Some(protocol),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut state = tw_gateway::AppState::new(cfg).unwrap();
    state.plugin_pool = Arc::new(Pool::with_replies(2, 16, slots));
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
    let addr = tw_gateway::serve(state.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    Gw { addr, state, runs }
}

/// 逐段改写的插件：每段文字一到就大写交出去，回答还没说完时客户端就看得到它改过的
fn upper(on_error: OnError) -> Arc<Active> {
    let d = Double::new("upper")
        .permit(&[Permission::ReplyText])
        .mode(ReplyMode::Stream)
        .on_text(|t| Some(t.to_uppercase()));
    let mut a = double::active("upper", d);
    a.on_error = on_error;
    Arc::new(a)
}

/// 发一个流式请求
async fn send(gw: &Gw) -> reqwest::Response {
    let body = json!({ "model": "claude-sonnet-4-5", "max_tokens": 64, "stream": true,
                       "messages": [{ "role": "user", "content": "hi" }] });
    reqwest::Client::new()
        .post(format!("http://{}/v1/messages", gw.addr))
        .header("x-api-key", "tw-testkey")
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap()
}

/// 一条流式回答读到第一段文字为止：这时这个回答的实例已经活着了
async fn open(
    gw: &Gw,
) -> (
    impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin,
    String,
) {
    let r = send(gw).await;
    assert_eq!(r.status(), 200);
    let mut s = Box::pin(r.bytes_stream());
    let mut got = Vec::new();
    while let Some(c) = s.next().await {
        got.extend_from_slice(&c.unwrap());
        if String::from_utf8_lossy(&got).contains("text_delta") {
            break;
        }
    }
    (s, String::from_utf8_lossy(&got).into_owned())
}

/// 读完剩下的
async fn rest(
    mut s: impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin,
    mut got: String,
) -> String {
    while let Some(c) = s.next().await {
        got.push_str(&String::from_utf8_lossy(&c.unwrap()));
    }
    got
}

/// 流里的全部文字
fn text(body: &str) -> String {
    body.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter_map(|v| v["delta"]["text"].as_str().map(str::to_string))
        .collect()
}

/// 名额满了而策略是拒绝：这个请求失败，码是 `gw.plugin.reply_busy`。占着名额的那个回答
/// 说完，名额还回来，下一个回答照常有插件
#[tokio::test]
async fn a_full_house_fails_the_request_under_reject_and_a_finished_answer_hands_its_slot_on() {
    let (release, rx) = watch::channel(false);
    let gw = gateway(
        held(rx).await,
        Protocol::Anthropic,
        vec![upper(OnError::Reject)],
        1,
    )
    .await;
    let (first, got) = open(&gw).await;
    assert_eq!(text(&got), "HELLO ");
    assert_eq!(gw.live(), 1);

    let r = send(&gw).await;
    assert_eq!(r.status(), 403);
    assert_eq!(
        r.headers()
            .get("x-thinkwatch-error")
            .and_then(|v| v.to_str().ok()),
        Some("denied")
    );
    let body: Value = r.json().await.unwrap();
    assert_eq!(
        body["error"]["message"],
        "[ThinkWatch] Plugin `upper` was not started for this answer: the limit of 1 plugins \
         running on answers at the same time was reached."
    );
    assert_eq!(gw.live(), 1, "the refused request took a slot");

    release.send_replace(true);
    let got = rest(first, got).await;
    assert_eq!(text(&got), "HELLO THERE");
    gw.settles_at(0).await;
    // 名额回来了：下一个回答照常有插件
    let (s, got) = open(&gw).await;
    let got = rest(s, got).await;
    assert_eq!(text(&got), "HELLO THERE");
    gw.settles_at(0).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(gw.error_codes("upper"), ["gw.plugin.reply_busy"]);
}

/// 名额满了而策略是跳过：这次回答原样过去，记一次出错
#[tokio::test]
async fn a_full_house_lets_the_answer_through_untouched_under_skip() {
    let (release, rx) = watch::channel(false);
    let gw = gateway(
        held(rx).await,
        Protocol::Anthropic,
        vec![upper(OnError::Skip)],
        1,
    )
    .await;
    let (first, got_first) = open(&gw).await;
    let (second, got_second) = open(&gw).await;
    assert_eq!(gw.live(), 1);
    release.send_replace(true);
    assert_eq!(text(&rest(first, got_first).await), "HELLO THERE");
    assert_eq!(
        text(&rest(second, got_second).await),
        "hello there",
        "the answer past the cap was not passed through as it was"
    );
    gw.settles_at(0).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(gw.error_codes("upper"), ["gw.plugin.reply_busy"]);
}

/// 客户端半路走了：名额跟着这次回答一起还回来，不等上游说完
#[tokio::test]
async fn a_client_that_walks_away_returns_the_slot() {
    let (_release, rx) = watch::channel(false);
    let gw = gateway(
        held(rx).await,
        Protocol::Anthropic,
        vec![upper(OnError::Reject)],
        4,
    )
    .await;
    let (s, got) = open(&gw).await;
    assert_eq!(text(&got), "HELLO ");
    assert_eq!(gw.live(), 1);
    drop(s);
    gw.settles_at(0).await;
}

/// 上游半路断了：这次回答以错误收尾，名额还回来
#[tokio::test]
async fn an_upstream_that_breaks_off_returns_the_slot() {
    let gw = gateway(
        breaking().await,
        Protocol::Anthropic,
        vec![upper(OnError::Reject)],
        4,
    )
    .await;
    let r = send(&gw).await;
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap_or_default();
    assert_eq!(text(&body), "HELLO ");
    assert!(body.contains("event: error"), "{body}");
    gw.settles_at(0).await;
}

// ───────────────────────────────────────────────────────── WebSocket

/// WebSocket 假上游：每个 `response.create` 先回 created 和一段文字，等 `release` 变成
/// true 再回完
async fn held_ws(release: watch::Receiver<bool>) -> SocketAddr {
    let app = Router::new().route(
        "/backend-api/codex/responses",
        axum::routing::any(move |ws: WebSocketUpgrade| {
            let mut rx = release.clone();
            async move {
                ws.on_upgrade(move |mut sock| async move {
                    while let Some(Ok(m)) = sock.recv().await {
                        let Message::Text(_) = m else { continue };
                        let frames = [
                            json!({"type":"response.created","response":{"id":"resp_1","status":"in_progress","output":[]}}),
                            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg","role":"assistant","content":[]}}),
                            json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg","delta":"hel"}),
                        ];
                        for f in frames {
                            if sock.send(Message::Text(f.to_string().into())).await.is_err() {
                                return;
                            }
                        }
                        while !*rx.borrow_and_update() {
                            if rx.changed().await.is_err() {
                                return;
                            }
                        }
                        let frames = [
                            json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg","delta":"lo"}),
                            json!({"type":"response.output_text.done","output_index":0,"content_index":0,"item_id":"msg","text":"hello"}),
                            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg","role":"assistant","content":[{"type":"output_text","text":"hello"}]}}),
                            json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[{"type":"message","id":"msg","role":"assistant","content":[{"type":"output_text","text":"hello"}]}]}}),
                        ];
                        for f in frames {
                            if sock.send(Message::Text(f.to_string().into())).await.is_err() {
                                return;
                            }
                        }
                    }
                })
            }
        }),
    );
    listen(app).await
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// 连上网关、发一帧 `response.create`，读到第一段文字为止
async fn ws_open(gw: &Gw) -> WsStream {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = format!("ws://{}/backend-api/codex/responses", gw.addr)
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("x-api-key", "tw-testkey".parse().unwrap());
    let (mut sock, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let frame = json!({ "type": "response.create", "model": "gpt-5", "input": "hi" });
    sock.send(tokio_tungstenite::tungstenite::Message::Text(
        frame.to_string().into(),
    ))
    .await
    .unwrap();
    loop {
        let m = tokio::time::timeout(Duration::from_secs(5), sock.next())
            .await
            .expect("no first delta")
            .unwrap()
            .unwrap();
        if m.into_text().unwrap().contains("output_text.delta") {
            return sock;
        }
    }
}

/// WebSocket 上一次回答一组实例：**这次回答完了就还**（连接还开着），连接半路断了也还
#[tokio::test]
async fn a_websocket_answer_returns_its_slot_when_it_completes_and_when_the_client_leaves() {
    let (release, rx) = watch::channel(false);
    let gw = gateway(
        held_ws(rx).await,
        Protocol::OpenaiResponses,
        vec![upper(OnError::Reject)],
        4,
    )
    .await;
    // 半路走掉
    let sock = ws_open(&gw).await;
    assert_eq!(gw.live(), 1);
    drop(sock);
    gw.settles_at(0).await;

    // 说完：连接还开着，名额已经回来了
    let mut sock = ws_open(&gw).await;
    assert_eq!(gw.live(), 1);
    release.send_replace(true);
    loop {
        let m = tokio::time::timeout(Duration::from_secs(5), sock.next())
            .await
            .expect("the answer did not complete")
            .unwrap()
            .unwrap();
        if m.into_text().unwrap().contains("response.completed") {
            break;
        }
    }
    gw.settles_at(0).await;
    drop(sock);
}
