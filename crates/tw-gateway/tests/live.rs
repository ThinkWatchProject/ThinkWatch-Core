//! 「正在服务中的请求」数得对不对。
//!
//! 桌面版更新时要等这个数归零再重启网关。**数少了**，重启会掐断一个正在
//! 流式输出的任务；**数多了**（某条路径漏减），等的人永远等不到零。两种
//! 错误在单元测试里都看不出来 —— 它们都出在响应体的生命周期上，而那只有
//! 真的起一个网关、真的让字节流过去才看得见。

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::routing::any;
use futures::StreamExt;
use tw_config::{Client, Config, Provider};

/// Anthropic 流的第一帧。**输入和缓存读在这里就是齐的**，输出是个占位的
/// 1 —— 累计输出要等流的末尾才报。
const MESSAGE_START: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5000,\"cache_read_input_tokens\":4000,\"output_tokens\":1}}}\n\n";

/// 先吐一帧，然后很久不吐下一帧 —— 一个正在思考的模型。
async fn slow_stream_upstream() -> SocketAddr {
    let app = Router::new().fallback(any(|| async {
        let body = async_stream::stream! {
            yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(MESSAGE_START));
            tokio::time::sleep(Duration::from_secs(60)).await;
            yield Ok(bytes::Bytes::from_static(b"event: message_stop\ndata: {}\n\n"));
        };
        axum::response::Response::builder()
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from_stream(body))
            .unwrap()
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

/// 一条完整的流：开头报输入，末尾报累计输出。
async fn complete_stream_upstream() -> SocketAddr {
    let app = Router::new().fallback(any(|| async {
        let mut frames = MESSAGE_START.to_vec();
        frames.extend_from_slice(
            b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":777}}\n\n\
              event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        axum::response::Response::builder()
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from(frames))
            .unwrap()
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

/// 说好 900 字节，发完第一帧就挂断 —— 上游中途没了。
///
/// **用裸 TCP 写**：框架替我们把响应收尾得好好的，而要模拟的恰恰是一个
/// 没收尾的响应。
async fn breaking_upstream() -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let mut head = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                                 content-length: 900\r\n\r\n"
                    .to_vec();
                head.extend_from_slice(MESSAGE_START);
                let _ = s.write_all(&head).await;
                let _ = s.flush().await;
                drop(s);
            });
        }
    });
    a
}

async fn fast_upstream() -> SocketAddr {
    let app = Router::new().fallback(any(|| async {
        axum::response::Response::builder()
            .header("content-type", "application/json")
            .body(axum::body::Body::from(r#"{"ok":true}"#))
            .unwrap()
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

/// 起一个网关，**把计数器留一份在外面** —— 状态被 move 进服务之后就
/// 摸不到了，而克隆出来的计数器数的是同一个数。
async fn serve(upstream: SocketAddr) -> (SocketAddr, tw_gateway::live::Live) {
    let (addr, live, _) = serve_watched(upstream).await;
    (addr, live)
}

/// 同上，再把事件流也留一份。**在服务起来之前订阅** —— 之后才订的话，
/// 早到的事件就看不见了。
async fn serve_watched(
    upstream: SocketAddr,
) -> (
    SocketAddr,
    tw_gateway::live::Live,
    tokio::sync::broadcast::Receiver<tw_api::Event>,
) {
    let cfg = Config {
        clients: vec![Client {
            name: "claude-code".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "up".into(),
            base_url: format!("http://{upstream}"),
            key: "sk-x".into(),
            protocol: Some(tw_config::Protocol::Anthropic),
            ..Default::default()
        }],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let live = state.live.clone();
    let events = state.bus.subscribe();
    let addr = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, addr).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, live, events)
}

fn post(gw: SocketAddr) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"m","stream":true,"messages":[]}"#)
}

/// 等计数到某个值。**不是 sleep 一下再看** —— 响应体在另一个任务里被
/// 丢掉，那一刻不由测试决定；固定睡多久都是在赌。
async fn settles_at(live: &tw_gateway::live::Live, want: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while live.count() != want {
        assert!(
            std::time::Instant::now() < deadline,
            "计数停在 {}，等的是 {want}",
            live.count()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 这条是这个计数器存在的理由。
///
/// 第一个字节发出去的时候 handler 早就返回了。数到 handler 返回为止的
/// 实现在这里会报 0 —— 然后桌面版就会在一个正在吐字的任务中间重启网关。
#[tokio::test]
async fn a_request_still_streaming_is_still_in_flight() {
    let (gw, live) = serve(slow_stream_upstream().await).await;
    assert_eq!(live.count(), 0);

    let resp = post(gw).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let mut body = resp.bytes_stream();
    let first = body.next().await.unwrap().unwrap();
    assert!(first.starts_with(b"event: message_start"));

    // 响应头和第一帧都到了，上游还在「思考」
    assert_eq!(live.count(), 1, "流还没结束，它就不该被当成已经结束");

    drop(body);
}

/// 客户端中途走了（Claude Code 里按一下 Esc 就是这个）。
///
/// **这是最容易漏减的那条路径**：流不是跑完的，是被 hyper 丢掉的，流
/// 末尾的收尾代码一行都不会执行。漏减的后果是这个数永远归不了零。
#[tokio::test]
async fn a_client_that_walks_away_mid_stream_stops_counting() {
    let (gw, live) = serve(slow_stream_upstream().await).await;

    let resp = post(gw).send().await.unwrap();
    let mut body = resp.bytes_stream();
    body.next().await.unwrap().unwrap();
    settles_at(&live, 1).await;

    drop(body);
    settles_at(&live, 0).await;
}

/// 这个网关报出来的所有结局（结束、失败、取消）。
///
/// **等到第一个之后再多等一会儿**：要验的是「恰好一个」，而多出来的那个
/// 如果存在，会紧跟着第一个到 —— 它们出自同一次收尾。
async fn endings(rx: &mut tokio::sync::broadcast::Receiver<tw_api::Event>) -> Vec<tw_api::Event> {
    let mut got = Vec::new();
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if matches!(
            ev,
            tw_api::Event::RequestFinished { .. }
                | tw_api::Event::RequestFailed { .. }
                | tw_api::Event::RequestCancelled { .. }
        ) {
            if got.is_empty() {
                deadline = tokio::time::Instant::now() + Duration::from_millis(500);
            }
            got.push(ev);
        }
    }
    got
}

/// 同一个按 Esc 的客户端，**这回看它有没有留下记录**。
///
/// 流末尾报结局的那几行在这条路径上一行都不会执行。什么都不报的话，存储层
/// 永远等不到这个请求：上游已经为五千个输入 token 计了费，这一行却永远
/// 不落库。
#[tokio::test]
async fn a_client_that_walks_away_mid_stream_is_reported_once_as_cancelled() {
    let (gw, live, mut events) = serve_watched(slow_stream_upstream().await).await;

    let resp = post(gw).send().await.unwrap();
    let mut body = resp.bytes_stream();
    body.next().await.unwrap().unwrap();
    drop(body);
    settles_at(&live, 0).await;

    let got = endings(&mut events).await;
    assert_eq!(got.len(), 1, "该恰好有一个结局：{got:?}");
    match &got[0] {
        tw_api::Event::RequestCancelled {
            status,
            bytes,
            usage,
            ..
        } => {
            assert_eq!(*status, 200);
            assert_eq!(
                *bytes,
                MESSAGE_START.len() as u64,
                "断开之前从上游收到的就是第一帧"
            );
            let u = usage.expect("第一帧里的用量没有带上");
            assert_eq!((u.input, u.cache_read), (5000, 4000));
        }
        other => panic!("该是一次取消，实际 {other:?}"),
    }
}

/// 跑完的流**只报结束**。Drop 那一侧不知道「已经报过了」的话，每一个正常
/// 跑完的请求都会再被记一次取消 —— 同一行写两遍，第二遍还是估算的金额。
#[tokio::test]
async fn a_stream_that_runs_to_its_end_is_not_also_reported_as_cancelled() {
    let (gw, _live, mut events) = serve_watched(complete_stream_upstream().await).await;
    let text = post(gw).send().await.unwrap().text().await.unwrap();
    assert!(text.contains("message_stop"), "{text}");

    let got = endings(&mut events).await;
    assert_eq!(got.len(), 1, "该恰好有一个结局：{got:?}");
    assert!(
        matches!(&got[0], tw_api::Event::RequestFinished { usage: Some(u), .. } if u.output == 777),
        "该是带着累计输出的结束：{got:?}"
    );
}

/// 上游中途断了：**报失败，不报取消**。网关随后往客户端补的那个错误帧，
/// 在它发出去之前结局就已经报过了。
#[tokio::test]
async fn a_stream_the_upstream_breaks_is_failed_and_not_also_cancelled() {
    let (gw, _live, mut events) = serve_watched(breaking_upstream().await).await;
    let text = post(gw)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap_or_default();
    assert!(text.contains("event: error"), "{text:?}");

    let got = endings(&mut events).await;
    assert_eq!(got.len(), 1, "该恰好有一个结局：{got:?}");
    assert!(
        matches!(&got[0], tw_api::Event::RequestFailed { .. }),
        "该是一次失败：{got:?}"
    );
}

#[tokio::test]
async fn a_finished_request_stops_counting() {
    let (gw, live) = serve(fast_upstream().await).await;
    let text = post(gw).send().await.unwrap().text().await.unwrap();
    assert!(text.contains("ok"));
    settles_at(&live, 0).await;
}

/// 在进入管线之前就被拒掉的请求，也必须把通行证还回来。
#[tokio::test]
async fn a_rejected_request_does_not_leak_a_count() {
    let (gw, live) = serve(fast_upstream().await).await;
    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "not-a-key")
        .body(r#"{"model":"m","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    settles_at(&live, 0).await;
}

/// 一条 WebSocket 连接开着多久，它就算在服务中多久。
///
/// 这条路径的通行证走的是另一条线：它被挪进升级之后的那个任务里，而不是
/// 挪进响应体。漏挪的话，升级完成那一刻计数就归零 —— 而 Codex 的整次会话
/// 都跑在这条连接上。
#[tokio::test]
async fn an_open_websocket_is_in_flight_until_it_closes() {
    use axum::extract::ws::WebSocketUpgrade;
    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let up = {
        let app = Router::new().route(
            "/backend-api/codex/responses",
            any(|ws: WebSocketUpgrade| async move {
                ws.on_upgrade(|mut sock| async move {
                    while let Some(Ok(m)) = sock.recv().await {
                        if sock.send(m).await.is_err() {
                            break;
                        }
                    }
                })
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        a
    };
    let (gw, live) = serve(up).await;

    let mut req = format!("ws://{gw}/backend-api/codex/responses")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("x-api-key", "tw-k".parse().unwrap());
    let (mut c, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    c.send(tokio_tungstenite::tungstenite::Message::Text("hi".into()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), c.next())
        .await
        .expect("等回帧超时")
        .unwrap()
        .unwrap();

    // 升级早就完成了，连接还开着
    settles_at(&live, 1).await;

    drop(c);
    settles_at(&live, 0).await;
}
