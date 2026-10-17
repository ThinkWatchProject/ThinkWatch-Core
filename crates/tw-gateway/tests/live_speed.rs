//! `/live` 的速度是此刻的：流还在吐的时候就有，按收到的输出现估；结束时按上游报的实数
//! 校准这家这个模型的估法。
//!
//! 起真网关、真上游：要验的是「流到一半」那一刻总线上的读数，单元测试里只能假装。

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::routing::any;
use bytes::Bytes;
use tw_config::{Client, Config, Protocol, Provider};

const MESSAGE_START: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":50,\"output_tokens\":1}}}\n\n";
const TEXT_BLOCK: &[u8] = b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n";
/// 八个 ASCII 字节：估出来两个 token
const TEXT_DELTA: &[u8] = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"words ab\"}}\n\n";
/// 上游说一共输出了 300 个
const MESSAGE_END: &[u8] = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":300}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

/// 上游读完输入要多久：第一个字在这之后
const PREFILL: Duration = Duration::from_millis(400);

async fn listen(app: Router) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

/// 开场帧马上发，读完输入之后每 `every` 吐一段，一共 `deltas` 段，最后报累计输出
fn streaming(deltas: usize, every: Duration) -> Router {
    Router::new().fallback(any(move || async move {
        let body = async_stream::stream! {
            yield Ok::<_, std::io::Error>(Bytes::from_static(MESSAGE_START));
            tokio::time::sleep(PREFILL).await;
            yield Ok(Bytes::from_static(TEXT_BLOCK));
            for _ in 0..deltas {
                yield Ok(Bytes::from_static(TEXT_DELTA));
                tokio::time::sleep(every).await;
            }
            yield Ok(Bytes::from_static(MESSAGE_END));
        };
        axum::response::Response::builder()
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from_stream(body))
            .unwrap()
    }))
}

fn cfg(upstream: SocketAddr) -> Config {
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
            protocol: Some(Protocol::Anthropic),
            ..Default::default()
        }],
        ..Default::default()
    }
}

struct Gateway {
    addr: SocketAddr,
    bus: tw_observe::EventBus,
    pace: std::sync::Arc<tw_gateway::pace::Pace>,
}

async fn serve(cfg: Config) -> Gateway {
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let (bus, pace) = (state.bus.clone(), state.pace.clone());
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    Gateway { addr, bus, pace }
}

async fn ask(gw: SocketAddr) -> String {
    reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"claude-sonnet-5","stream":true,"messages":[]}"#)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

/// **流到一半就有速度。**还在读输入的时候有请求在跑、却没有字：合计是 0，这个请求没有
/// 速度；开口之后每秒约 20 个（每 100 毫秒一段、一段估两个）
#[tokio::test]
async fn the_speed_is_there_while_the_stream_is_still_running() {
    let up = listen(streaming(30, Duration::from_millis(100))).await;
    let gw = serve(cfg(up)).await;
    let request = tokio::spawn(ask(gw.addr));

    tokio::time::sleep(Duration::from_millis(200)).await;
    let live = gw.bus.live();
    assert_eq!(live.running.len(), 1, "{live:?}");
    assert_eq!(live.tokens_per_sec, Some(0));
    assert_eq!(live.running[0].tokens_per_sec, None);
    assert_eq!(live.running[0].output_tokens, None);

    tokio::time::sleep(Duration::from_millis(1_600)).await;
    let live = gw.bus.live();
    let r = &live.running[0];
    let rate = r.tokens_per_sec.expect("opened by now");
    assert!((12..=30).contains(&rate), "{rate} tokens/s");
    assert_eq!(live.tokens_per_sec, Some(rate));
    let out = r.output_tokens.unwrap();
    assert!((20..=40).contains(&out), "{out} tokens so far");

    let text = request.await.unwrap();
    assert!(text.contains("message_stop"), "{text}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let live = gw.bus.live();
    assert!(live.running.is_empty());
    assert_eq!(live.tokens_per_sec, None);
}

/// **结束时按上游的实数校准。**估出 60 个（30 段、每段两个），上游说 300 个：之后这家这个
/// 模型的估数乘 2（比例最多到 2）
#[tokio::test]
async fn a_finished_stream_teaches_the_estimate() {
    let up = listen(streaming(30, Duration::from_millis(5))).await;
    let gw = serve(cfg(up)).await;
    assert_eq!(gw.pace.ratio("up", "claude-sonnet-5"), 1.0);
    let text = ask(gw.addr).await;
    assert!(text.contains("message_stop"), "{text}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(gw.pace.ratio("up", "claude-sonnet-5"), 2.0);
}
