//! 第一个 token 什么时候到、之后吐得多快。
//!
//! 起真网关、真上游：要验的是时刻 —— 响应头、第一个 token、结束各在什么时候，
//! 以及速度是从哪一刻算起的。这些在单元测试里只能假装。

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::routing::any;
use bytes::Bytes;
use tokio::sync::broadcast::Receiver;
use tw_api::Event;
use tw_config::{Client, Config, Protocol, Provider};

const MESSAGE_START: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":50,\"output_tokens\":1}}}\n\n";
const TEXT_BLOCK: &[u8] = b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n";
const TEXT_DELTA: &[u8] = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"word \"}}\n\n";
const MESSAGE_END: &[u8] = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":120}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

/// 上游读完输入要多久：响应头早就回了，第一个字在这之后
const PREFILL: Duration = Duration::from_millis(300);

async fn listen(app: Router) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

/// 一条流：开场帧马上发，读完输入之后一个字一个字地吐，最后报累计输出。
fn streaming(frames_after_prefill: Vec<&'static [u8]>, first: &'static [u8]) -> Router {
    Router::new().fallback(any(move || {
        let frames = frames_after_prefill.clone();
        async move {
            let body = async_stream::stream! {
                yield Ok::<_, std::io::Error>(Bytes::from_static(first));
                tokio::time::sleep(PREFILL).await;
                for f in frames {
                    yield Ok(Bytes::from_static(f));
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            };
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from_stream(body))
                .unwrap()
        }
    }))
}

fn cfg(upstream: SocketAddr, protocol: Protocol) -> Config {
    Config {
        clients: vec![Client {
            name: "me".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "up".into(),
            base_url: format!("http://{upstream}"),
            key: Some("sk-x".into()),
            protocol: Some(protocol),
            ..Default::default()
        }],
        ..Default::default()
    }
}

async fn serve(cfg: Config) -> (SocketAddr, Receiver<Event>) {
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let events = state.bus.subscribe();
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, events)
}

/// 一个请求从开始到结局的全部事件
async fn until_finished(rx: &mut Receiver<Event>) -> Vec<Event> {
    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        let end = matches!(
            ev,
            Event::RequestFinished { .. }
                | Event::RequestFailed { .. }
                | Event::RequestCancelled { .. }
        );
        got.push(ev);
        if end {
            break;
        }
    }
    got
}

/// （响应头，第一个 token，总耗时，速度）
fn timings(got: &[Event]) -> (Option<u64>, Option<u64>, u64, Option<u32>) {
    let ttfb = got.iter().find_map(|e| match e {
        Event::RequestHeaders { ttfb_ms, .. } => Some(*ttfb_ms),
        _ => None,
    });
    let ttft = got.iter().find_map(|e| match e {
        Event::RequestFirstToken { ttft_ms, .. } => Some(*ttft_ms),
        _ => None,
    });
    let Some(Event::RequestFinished {
        duration_ms,
        tokens_per_sec,
        ..
    }) = got.last()
    else {
        panic!("该以一条结束收尾：{got:?}");
    };
    (ttfb, ttft, *duration_ms, *tokens_per_sec)
}

/// **响应头和第一个 token 是两个时刻。**流式的响应头一收到请求就回，第一个字要等上游
/// 读完输入；速度从第一个字算到结束 —— 从响应头算的话，读输入的那 300 毫秒会被当成
/// 生成的时间
#[tokio::test]
async fn a_stream_opens_after_its_headers_and_its_rate_counts_from_there() {
    let mut frames = vec![TEXT_BLOCK, TEXT_DELTA];
    frames.extend([TEXT_DELTA; 5]);
    frames.push(MESSAGE_END);
    let up = listen(streaming(frames, MESSAGE_START)).await;
    let (gw, mut rx) = serve(cfg(up, Protocol::Anthropic)).await;
    let text = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"claude-sonnet-5","stream":true,"messages":[]}"#)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(text.contains("message_stop"), "{text}");

    let got = until_finished(&mut rx).await;
    let (ttfb, ttft, duration, rate) = timings(&got);
    let (ttfb, ttft) = (ttfb.expect("没有响应头"), ttft.expect("没有第一个 token"));
    assert!(
        ttfb < PREFILL.as_millis() as u64,
        "响应头该在读输入之前：{ttfb}"
    );
    assert!(
        ttft >= PREFILL.as_millis() as u64,
        "第一个 token 早于第一个字：{ttft}"
    );
    assert_eq!(
        got.iter()
            .filter(|e| matches!(e, Event::RequestFirstToken { .. }))
            .count(),
        1,
        "{got:?}"
    );
    assert_eq!(
        rate,
        Some((120 * 1000 / (duration - ttft)) as u32),
        "{got:?}"
    );
}

/// 整段一起到的：响应头要等生成完才回，没有「第一个」。**不给速度** —— 以前从响应头
/// 算，总耗时减去响应头只剩几毫秒，这一条就是几万 token/秒
#[tokio::test]
async fn a_whole_response_has_no_first_token_and_no_rate() {
    let up = listen(Router::new().fallback(any(|| async {
        tokio::time::sleep(PREFILL).await;
        axum::response::Response::builder()
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"type":"message","role":"assistant","content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":50,"output_tokens":500}}"#,
            ))
            .unwrap()
    })))
    .await;
    let (gw, mut rx) = serve(cfg(up, Protocol::Anthropic)).await;
    reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"claude-sonnet-5","messages":[]}"#)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let got = until_finished(&mut rx).await;
    let (ttfb, ttft, _, rate) = timings(&got);
    assert!(ttfb.is_some(), "{got:?}");
    assert_eq!((ttft, rate), (None, None), "{got:?}");
}

/// **推理被隐藏的**（Chat Completions 的推理模型）：第一个字之前模型想了半天，一个字
/// 都没吐，用量里的 2000 个推理 token 生成在分母之外。扣掉它们，剩下 20 个是在第一个字
/// 之后吐的
#[tokio::test]
async fn hidden_reasoning_is_left_out_of_the_rate() {
    const DELTA: &[u8] =
        b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"word \"}}]}\n\n";
    const START: &[u8] =
        b"data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n";
    const END: &[u8] = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2020,\"completion_tokens_details\":{\"reasoning_tokens\":2000}}}\n\ndata: [DONE]\n\n";
    let mut frames = vec![DELTA; 6];
    frames.push(END);
    let up = listen(streaming(frames, START)).await;
    let (gw, mut rx) = serve(cfg(up, Protocol::OpenaiChat)).await;
    reqwest::Client::new()
        .post(format!("http://{gw}/v1/chat/completions"))
        .bearer_auth("tw-k")
        .body(r#"{"model":"o5","stream":true,"messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let got = until_finished(&mut rx).await;
    let (_, ttft, duration, rate) = timings(&got);
    let ttft = ttft.expect("没有第一个 token");
    assert!(ttft >= PREFILL.as_millis() as u64, "{ttft}");
    assert_eq!(
        rate,
        Some((20 * 1000 / (duration - ttft)) as u32),
        "{got:?}"
    );
}
