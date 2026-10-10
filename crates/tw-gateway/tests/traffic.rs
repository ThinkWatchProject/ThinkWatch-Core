//! 一个请求在网关和上游之间走了多少流量、从哪个出口出去的，端到端（见 `tw_gateway::traffic`）。
//!
//! - 故障转移：**每一跳都算**，连不上的那一跳没发出去、不算；失败的那一跳读过的错误正文算
//! - 收到的按线上的样子数，**解压之前**：上游压缩过的回答数压缩着的字节，交给客户端的是解开的
//! - 走代理的上游：结局的出口、尝试链上每一跳的代理都是它的名字；直连的没有

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::OriginalUri;
use axum::routing::any;
use tw_api::Event;
use tw_config::{Client, Config, Protocol, Provider};

mod common;
use common::spare_port;

const MODEL: &str = "claude-sonnet-4-5";

/// 客户端发的请求体。同格式直通、一个字节都不改，每一跳发给上游的就是它
fn asked() -> String {
    format!(
        r#"{{"model":"{MODEL}","max_tokens":16,"messages":[{{"role":"user","content":"hi"}}]}}"#
    )
}

/// 上游的一个整包回答
const ANSWER: &str = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[{"type":"text","text":"hello there"}],"stop_reason":"end_turn","usage":{"input_tokens":12,"output_tokens":3}}"#;

/// 一家忙不过来的上游：503，带着它的错误正文
const OVERLOADED: &str =
    r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;

async fn listen(app: Router) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

fn provider(name: &str, base_url: String) -> Provider {
    Provider {
        name: name.into(),
        base_url,
        key: Some("sk-x".into()),
        protocol: Some(Protocol::Anthropic),
        ..Default::default()
    }
}

async fn gateway(cfg: Config) -> (SocketAddr, tokio::sync::broadcast::Receiver<Event>) {
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let rx = state.bus.subscribe();
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, rx)
}

fn cfg(providers: Vec<Provider>) -> Config {
    Config {
        version: 1,
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers,
        ..Default::default()
    }
}

/// 发一个请求，交回状态码和**原样**的正文（测试用的客户端不解压：看到的就是网关发出的字节）
async fn post(gw: SocketAddr) -> (u16, bytes::Bytes) {
    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("content-type", "application/json")
        .header("x-api-key", "tw-k")
        .body(asked())
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.bytes().await.unwrap())
}

/// 这个请求的路由事件和结局
async fn routed_and_ended(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> (Event, Event) {
    let mut routed = None;
    loop {
        let e = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("no ending")
            .unwrap();
        match e {
            Event::RequestRouted { .. } => routed = Some(e),
            Event::RequestFinished { .. }
            | Event::RequestFailed { .. }
            | Event::RequestCancelled { .. } => {
                return (routed.expect("routed before the ending"), e);
            }
            _ => {}
        }
    }
}

/// 三家：第一家连不上，第二家回 503，第三家回一个 zstd 压缩过的整包回答。
///
/// **发出去的是两跳的请求体**：连不上的那一跳一个字节都没出去。**收回来的是 503 的错误正文
/// 加上压缩着的回答** —— 解压之后的长度不是流量；回答本身解开了交给客户端（见
/// `tw_gateway::inflate`），数的还是线上的字节。全都直连：没有出口
#[tokio::test]
async fn every_hop_counts_and_what_arrived_is_counted_before_decompression() {
    let dead = format!("http://127.0.0.1:{}", spare_port());
    let busy = listen(Router::new().fallback(any(|| async {
        axum::response::Response::builder()
            .status(503)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(OVERLOADED))
            .unwrap()
    })))
    .await;
    let packed = zstd::stream::encode_all(ANSWER.as_bytes(), 3).unwrap();
    assert_ne!(packed.len(), ANSWER.len());
    let wire = packed.clone();
    let ok = listen(Router::new().fallback(any(move || {
        let wire = wire.clone();
        async move {
            axum::response::Response::builder()
                .header("content-type", "application/json")
                .header("content-encoding", "zstd")
                .body(axum::body::Body::from(wire))
                .unwrap()
        }
    })))
    .await;
    let (gw, mut rx) = gateway(cfg(vec![
        provider("dead", dead),
        provider("busy", format!("http://{busy}")),
        provider("ok", format!("http://{ok}")),
    ]))
    .await;

    let (status, body) = post(gw).await;
    assert_eq!(status, 200);
    assert_eq!(
        String::from_utf8_lossy(&body),
        ANSWER,
        "回答解开了交给客户端"
    );

    let (routed, end) = routed_and_ended(&mut rx).await;
    let Event::RequestRouted {
        attempts, egress, ..
    } = &routed
    else {
        unreachable!()
    };
    let tried: Vec<_> = attempts.iter().map(|a| a.provider.as_str()).collect();
    assert_eq!(tried, ["dead", "busy", "ok"], "{attempts:?}");
    assert!(attempts.iter().all(|a| a.proxy.is_none()), "{attempts:?}");
    assert_eq!(*egress, None, "直连的没有出口");
    match end {
        Event::RequestFinished {
            status: 200,
            sent_bytes,
            received_bytes,
            ..
        } => {
            assert_eq!(sent_bytes, 2 * asked().len() as u64, "连不上的那一跳不算");
            assert_eq!(
                received_bytes,
                (OVERLOADED.len() + packed.len()) as u64,
                "503 的错误正文加上压缩着的回答"
            );
        }
        other => panic!("该是一次结束，实际 {other:?}"),
    }
}

/// 每一家都连不上：**一跳都没发出去，没有流量可说** —— 不是一对 0
#[tokio::test]
async fn a_request_that_never_left_has_no_traffic() {
    let (gw, mut rx) = gateway(cfg(vec![provider(
        "dead",
        format!("http://127.0.0.1:{}", spare_port()),
    )]))
    .await;
    let (status, _) = post(gw).await;
    assert_eq!(status, 502);
    let (_, end) = routed_and_ended(&mut rx).await;
    assert!(
        matches!(
            end,
            Event::RequestFailed {
                sent_bytes: None,
                received_bytes: None,
                ..
            }
        ),
        "{end:?}"
    );
}

/// 走代理的上游：请求从代理出去，结局的出口和尝试链上那一跳的代理都是它的名字。
///
/// 假代理就是一个 HTTP 服务：经 HTTP 代理发往 `http://` 的请求，请求行写的是完整的地址，
/// 它照样答 —— 上游的地址根本解析不到，答上了就说明走的是代理
#[tokio::test]
async fn a_proxied_upstream_names_its_egress() {
    let seen: Arc<Mutex<Vec<String>>> = Default::default();
    let log = seen.clone();
    let proxy = listen(
        Router::new().fallback(any(move |OriginalUri(uri): OriginalUri| {
            let log = log.clone();
            async move {
                log.lock().unwrap().push(uri.to_string());
                axum::response::Response::builder()
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(ANSWER))
                    .unwrap()
            }
        })),
    )
    .await;
    let mut p = provider("relay", "http://relay.invalid".into());
    p.proxy = "机场".into();
    let mut c = cfg(vec![p]);
    c.proxies = vec![tw_config::Proxy {
        name: "机场".into(),
        kind: tw_config::ProxyKind::Http,
        addr: proxy.to_string(),
        auth: None,
    }];
    let (gw, mut rx) = gateway(c).await;

    let (status, body) = post(gw).await;
    assert_eq!(status, 200, "{body:?}");
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        ["http://relay.invalid/v1/messages"],
        "请求该经过代理"
    );

    let (routed, end) = routed_and_ended(&mut rx).await;
    let Event::RequestRouted {
        attempts, egress, ..
    } = &routed
    else {
        unreachable!()
    };
    assert_eq!(egress.as_deref(), Some("机场"));
    assert_eq!(attempts[0].proxy.as_deref(), Some("机场"));
    assert!(
        matches!(
            end,
            Event::RequestFinished { sent_bytes, received_bytes, .. }
                if sent_bytes == asked().len() as u64 && received_bytes == ANSWER.len() as u64
        ),
        "{end:?}"
    );
}

/// 一家都没接下时，出口是最后一个发到了上游的那一跳走的：它的流量是从那儿出去的
#[tokio::test]
async fn a_failed_request_names_the_egress_of_the_last_hop_that_reached_an_upstream() {
    let proxy = listen(Router::new().fallback(any(|| async {
        axum::response::Response::builder()
            .status(503)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(OVERLOADED))
            .unwrap()
    })))
    .await;
    let mut p = provider("relay", "http://relay.invalid".into());
    p.proxy = "机场".into();
    let mut c = cfg(vec![
        p,
        provider("dead", format!("http://127.0.0.1:{}", spare_port())),
    ]);
    c.proxies = vec![tw_config::Proxy {
        name: "机场".into(),
        kind: tw_config::ProxyKind::Http,
        addr: proxy.to_string(),
        auth: None,
    }];
    let (gw, mut rx) = gateway(c).await;
    let (status, _) = post(gw).await;
    assert_eq!(status, 502);

    let (routed, end) = routed_and_ended(&mut rx).await;
    let Event::RequestRouted {
        attempts, egress, ..
    } = &routed
    else {
        unreachable!()
    };
    let proxies: Vec<_> = attempts.iter().map(|a| a.proxy.as_deref()).collect();
    assert_eq!(proxies, [Some("机场"), None], "{attempts:?}");
    assert_eq!(egress.as_deref(), Some("机场"));
    assert!(
        matches!(
            end,
            Event::RequestFailed { sent_bytes: Some(s), received_bytes: Some(r), .. }
                if s == asked().len() as u64 && r == OVERLOADED.len() as u64
        ),
        "{end:?}"
    );
}

/// 整条连接一行的 WebSocket（Realtime）：**数的是两个方向数据帧的载荷**。发出去的是客户端的
/// 帧（脱敏之后、真正发出去的那一份），收回来的是上游的帧（还原占位符之前）；控制帧不算
#[tokio::test]
async fn a_websocket_connection_counts_the_frames_both_ways() {
    use axum::extract::ws::{Message, WebSocketUpgrade};
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as Frame;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    // 回显，另外每一帧多回一句
    let up = listen(Router::new().route(
        "/v1/realtime",
        any(|ws: WebSocketUpgrade| async move {
            ws.on_upgrade(|mut sock| async move {
                while let Some(Ok(m)) = sock.recv().await {
                    let more = matches!(m, Message::Text(_));
                    if sock.send(m).await.is_err() {
                        break;
                    }
                    if more && sock.send(Message::Text("ack".into())).await.is_err() {
                        break;
                    }
                }
            })
        }),
    ))
    .await;
    let mut p = provider("rt", format!("http://{up}"));
    p.protocol = Some(Protocol::OpenaiResponses);
    let (gw, mut rx) = gateway(cfg(vec![p])).await;

    let mut req = format!("ws://{gw}/v1/realtime?model=gpt-realtime")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", "Bearer tw-k".parse().unwrap());
    let (mut c, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    const TEXT: &str = r#"{"type":"session.update"}"#;
    const BINARY: &[u8] = &[1, 2, 3, 4, 5];
    c.send(Frame::Text(TEXT.into())).await.unwrap();
    c.send(Frame::Binary(BINARY.to_vec().into())).await.unwrap();
    c.send(Frame::Ping(vec![9; 8].into())).await.unwrap();
    // 回显的那一帧、多回的一句、回显的二进制帧
    let mut got = 0;
    while got < 3 {
        let m = tokio::time::timeout(Duration::from_secs(3), c.next())
            .await
            .expect("等回帧超时")
            .unwrap()
            .unwrap();
        if matches!(m, Frame::Text(_) | Frame::Binary(_)) {
            got += 1;
        }
    }
    c.close(None).await.unwrap();

    let (_, end) = routed_and_ended(&mut rx).await;
    match end {
        Event::RequestFinished {
            status: 101,
            sent_bytes,
            received_bytes,
            ..
        } => {
            assert_eq!(sent_bytes, (TEXT.len() + BINARY.len()) as u64);
            assert_eq!(
                received_bytes,
                (TEXT.len() + "ack".len() + BINARY.len()) as u64
            );
        }
        other => panic!("该是一次结束，实际 {other:?}"),
    }
}
