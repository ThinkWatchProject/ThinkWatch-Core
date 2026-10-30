//! 生成过慢时中断（`providers[].min_speed`，见 `tw_gateway::slow`），端到端。
//!
//! 假上游按脚本隔一阵发一段：每段正文 80 个 ASCII 字符，估出来是 20 个 token。测试把一秒调成
//! 五十毫秒（`AppState::speed_tick`）：判定窗口 20「秒」是一秒，长静默 5「秒」是 250 毫秒，
//! 每 50 毫秒看一次。最低速度写 15，也就是每 50 毫秒 15 个 token（每秒 300 个）：
//!
//! - 快：每 10 毫秒一段（每秒一千多个，在计时粗的机器上也有好几百）
//! - 慢：每 200 毫秒一段（每秒 100 个）
//!
//! 它记下自己的响应被丢掉了没有：回答被结束时，连接要真的断开，上游才会停下。

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{Value, json};
use tw_api::Event;
use tw_config::{Client, Config, Failover, MinSpeed, Protocol, Provider};

/// 测试里的「一秒」
const TICK: Duration = Duration::from_millis(50);
/// 最低速度
const MIN: u32 = 15;

/// 一段正文：80 个字符，估出来 20 个 token
const TEXT: &str =
    "abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzab";

const MESSAGE_START: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"usage\":{\"input_tokens\":1200,\"output_tokens\":1}}}\n\n";
const BLOCK_START: &str = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n";
const PING: &str = "event: ping\ndata: {\"type\":\"ping\"}\n\n";
const END: &str = "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
     event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2000}}\n\n\
     event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

fn word() -> Bytes {
    Bytes::from(format!(
        "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{TEXT}\"}}}}\n\n"
    ))
}

/// 一个假上游的回答：隔多少毫秒发哪一段，发完了挂不挂着（挂着时每 100 毫秒一行注释）
#[derive(Clone, Default)]
struct Script {
    steps: Vec<(u64, Bytes)>,
    hang: bool,
    content_type: Option<&'static str>,
}

impl Script {
    /// Anthropic 的流开了头
    fn opened() -> Self {
        Self {
            steps: vec![
                (0, Bytes::from_static(MESSAGE_START.as_bytes())),
                (0, Bytes::from_static(BLOCK_START.as_bytes())),
            ],
            ..Default::default()
        }
    }

    /// 每 `every` 毫秒一段，持续 `ms` 毫秒
    fn words(mut self, every: u64, ms: u64) -> Self {
        for _ in 0..(ms / every).max(1) {
            self.steps.push((every, word()));
        }
        self
    }

    /// `ms` 毫秒没有内容（中间隔 100 毫秒一个心跳）
    fn quiet(mut self, ms: u64) -> Self {
        for _ in 0..ms / 100 {
            self.steps.push((100, Bytes::from_static(PING.as_bytes())));
        }
        self
    }

    fn fast(self, ms: u64) -> Self {
        self.words(10, ms)
    }

    fn slow(self, ms: u64) -> Self {
        self.words(200, ms)
    }

    fn end(mut self) -> Self {
        self.steps.push((0, Bytes::from_static(END.as_bytes())));
        self
    }

    fn hang(mut self) -> Self {
        self.hang = true;
        self
    }
}

struct Upstream {
    addr: SocketAddr,
    dropped: Arc<AtomicBool>,
}

/// 被丢掉时举旗
struct Flag(Arc<AtomicBool>);

impl Drop for Flag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

async fn upstream(script: Script) -> Upstream {
    let dropped = Arc::new(AtomicBool::new(false));
    let d = dropped.clone();
    let app = Router::new().fallback(axum::routing::any(move || {
        let (script, d) = (script.clone(), d.clone());
        async move {
            let flag = Flag(d);
            let steps = futures::stream::iter(script.steps).then(|(ms, chunk)| async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Ok::<_, std::convert::Infallible>(chunk)
            });
            let hang = script.hang;
            let tail = futures::stream::unfold((), move |_| async move {
                if !hang {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                Some((Ok(Bytes::from_static(b": keep-alive\n\n")), ()))
            });
            let body = steps.chain(tail).map(move |x| {
                let _ = &flag;
                x
            });
            axum::response::Response::builder()
                .status(200)
                .header(
                    "content-type",
                    script.content_type.unwrap_or("text/event-stream"),
                )
                .body(axum::body::Body::from_stream(body))
                .unwrap()
        }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Upstream { addr, dropped }
}

fn provider(up: &Upstream, min_speed: Option<u32>) -> Provider {
    Provider {
        name: "local".into(),
        base_url: format!("http://{}", up.addr),
        key: Some("sk-upstream".into()),
        protocol: Some(Protocol::Anthropic),
        min_speed: min_speed.map(|tokens_per_sec| MinSpeed {
            tokens_per_sec,
            window_secs: 20,
        }),
        ..Default::default()
    }
}

async fn gateway(
    p: Provider,
) -> (
    SocketAddr,
    tokio::sync::broadcast::Receiver<Event>,
    tw_gateway::AppState,
) {
    let cfg = Config {
        version: 1,
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![p],
        // 一次失败就停用：看得出这一家记了一次失败
        failover: Failover {
            failures_to_pause: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut state = tw_gateway::AppState::new(cfg).unwrap();
    state.speed_tick = TICK;
    let rx = state.bus.subscribe();
    let addr = tw_gateway::serve(state.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, rx, state)
}

fn messages(stream: bool) -> Value {
    json!({"model": "claude-sonnet-5", "max_tokens": 1024, "stream": stream,
           "messages": [{"role": "user", "content": "Say hello."}]})
}

/// 发出去，交回状态码和整个正文
async fn post(gw: SocketAddr, path: &str, body: &Value) -> (u16, String) {
    let resp = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{gw}{path}"))
        .header("content-type", "application/json")
        .header("x-api-key", "tw-k")
        .header("x-goog-api-key", "tw-k")
        .header("authorization", "Bearer tw-k")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap())
}

/// 请求的结局（结束或失败；取消是 panic）
async fn ending(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> Event {
    loop {
        let e = tokio::time::timeout(Duration::from_secs(15), rx.recv())
            .await
            .expect("没等到结局")
            .unwrap();
        match e {
            Event::RequestFinished { .. } | Event::RequestFailed { .. } => return e,
            Event::RequestCancelled { .. } => panic!("不该是取消：{e:?}"),
            _ => {}
        }
    }
}

/// 结局是生成太慢：交回参数和速度
fn too_slow(e: &Event) -> (tw_api::Msg, Option<u32>) {
    match e {
        Event::RequestFailed {
            source,
            message,
            tokens_per_sec,
            ..
        } => {
            assert_eq!(*source, tw_api::FailureSource::Upstream, "{e:?}");
            assert_eq!(message.code, "gw.upstream.too_slow", "{e:?}");
            (message.clone(), *tokens_per_sec)
        }
        other => panic!("该是生成太慢：{other:?}"),
    }
}

fn finished(e: &Event) {
    assert!(
        matches!(e, Event::RequestFinished { status: 200, .. }),
        "该好好结束：{e:?}"
    );
}

/// 等旗举起来，最多两秒
async fn eventually(flag: &AtomicBool) -> bool {
    for _ in 0..40 {
        if flag.load(Ordering::SeqCst) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

// ───────────────────────────────────────────── 不中断

/// 1. 全程都快：照常答完
#[tokio::test]
async fn a_stream_that_stays_fast_is_left_alone() {
    let up = upstream(Script::opened().fast(2_000).end()).await;
    let (gw, mut rx, state) = gateway(provider(&up, Some(MIN))).await;
    let (status, body) = post(gw, "/v1/messages", &messages(true)).await;
    assert_eq!(status, 200);
    assert!(body.contains("message_stop"), "{body}");
    assert!(!body.contains("event: error"), "{body}");
    finished(&ending(&mut rx).await);
    assert_eq!(
        state.health.state("local"),
        tw_gateway::health::State::Closed
    );
}

/// 3. 中途慢了一阵（不到一个窗口）又快起来：不中断
#[tokio::test]
async fn a_short_slowdown_is_not_enough() {
    let up = upstream(Script::opened().fast(1_200).slow(400).fast(1_200).end()).await;
    let (gw, mut rx, _) = gateway(provider(&up, Some(MIN))).await;
    let (status, body) = post(gw, "/v1/messages", &messages(true)).await;
    assert_eq!(status, 200);
    assert!(!body.contains("event: error"), "{body}");
    finished(&ending(&mut rx).await);
}

/// 4. 中途停了 600 毫秒（12「秒」，长静默）之后照常快：静着的那段不算，不中断
#[tokio::test]
async fn a_long_pause_partway_through_is_not_counted_as_slow() {
    let up = upstream(Script::opened().fast(1_200).quiet(600).fast(1_200).end()).await;
    let (gw, mut rx, _) = gateway(provider(&up, Some(MIN))).await;
    let (status, body) = post(gw, "/v1/messages", &messages(true)).await;
    assert_eq!(status, 200);
    assert!(!body.contains("event: error"), "{body}");
    finished(&ending(&mut rx).await);
}

/// 5. 第一个 token 之前等了很久（30「秒」）：那是无响应超时的事，不中断
#[tokio::test]
async fn waiting_for_the_first_token_is_not_judged() {
    let up = upstream(Script::opened().quiet(1_500).fast(1_200).end()).await;
    let (gw, mut rx, _) = gateway(provider(&up, Some(MIN))).await;
    let (status, body) = post(gw, "/v1/messages", &messages(true)).await;
    assert_eq!(status, 200);
    assert!(!body.contains("event: error"), "{body}");
    finished(&ending(&mut rx).await);
}

/// 6. 没设最低速度的上游：慢也照常答完
#[tokio::test]
async fn an_upstream_without_a_minimum_is_left_alone() {
    let up = upstream(Script::opened().slow(1_600).end()).await;
    let (gw, mut rx, _) = gateway(provider(&up, None)).await;
    let (status, body) = post(gw, "/v1/messages", &messages(true)).await;
    assert_eq!(status, 200);
    assert!(body.contains("message_stop"), "{body}");
    finished(&ending(&mut rx).await);
}

/// 6. 整包的回答：设了也不管
#[tokio::test]
async fn a_whole_answer_is_not_judged() {
    let whole = json!({"id": "msg_1", "type": "message", "role": "assistant",
        "model": "claude-sonnet-5", "content": [{"type": "text", "text": "hello"}],
        "stop_reason": "end_turn", "usage": {"input_tokens": 5, "output_tokens": 1}});
    let up = upstream(Script {
        steps: vec![(1_500, Bytes::from(whole.to_string()))],
        content_type: Some("application/json"),
        ..Default::default()
    })
    .await;
    let (gw, mut rx, _) = gateway(provider(&up, Some(MIN))).await;
    let (status, body) = post(gw, "/v1/messages", &messages(false)).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("hello"), "{body}");
    finished(&ending(&mut rx).await);
}

// ───────────────────────────────────────────── 中断

/// 2. 中途掉速并持续了一个窗口：以 503 的错误帧结束，断开上游，这一家记一次失败；
/// 失败带着结束那一刻的速度，输出按估数记
#[tokio::test]
async fn a_stream_that_slows_down_for_a_window_is_ended() {
    let up = upstream(Script::opened().fast(500).slow(5_000).hang()).await;
    let (gw, mut rx, state) = gateway(provider(&up, Some(MIN))).await;
    let t = std::time::Instant::now();
    let (status, body) = post(gw, "/v1/messages", &messages(true)).await;
    // 响应头早交出去了：状态码改不了，错误在流里说
    assert_eq!(status, 200);
    assert!(
        t.elapsed() < Duration::from_secs(4),
        "该在掉速一个窗口之后就结束，用了 {:?}",
        t.elapsed()
    );
    let last = body.trim_end().rsplit("\n\n").next().unwrap();
    assert!(last.starts_with("event: error"), "{body}");
    assert!(last.contains("\"overloaded_error\""), "{last}");
    assert!(
        last.contains("[ThinkWatch] Upstream `local` generated"),
        "{last}"
    );
    assert!(body.contains(TEXT), "已经生成的照常交出去：{body}");
    let e = ending(&mut rx).await;
    let (msg, rate) = too_slow(&e);
    assert_eq!(msg.arg("upstream"), "local");
    assert_eq!(msg.arg("min"), MIN.to_string());
    assert_eq!(msg.arg("secs"), "20");
    let rate = rate.expect("失败带着速度");
    assert!(rate < MIN, "{rate}");
    assert_eq!(msg.arg("rate"), rate.to_string());
    assert!(
        !msg.text.starts_with("the response stream broke"),
        "流没断，是按设定结束的：{}",
        msg.text
    );
    // 上游报的输入照用；输出它只报了个占位的 1，按估数记
    let Event::RequestFailed {
        usage: Some(u),
        duration_ms,
        ..
    } = &e
    else {
        panic!("{e:?}")
    };
    assert_eq!(u.input, 1200);
    assert!(u.output >= 20 * 20, "估出来的输出：{u:?}");
    assert!(duration_ms.is_some());
    assert!(eventually(&up.dropped).await, "上游的连接要断开");
    assert_eq!(
        state.health.state("local"),
        tw_gateway::health::State::Open,
        "这一家记了一次失败"
    );
}

/// 估出来的速度还说不出（扣掉长静默之后不到窗口的一半）时不下结论：静着的上游交给无响应超时
#[tokio::test]
async fn a_stream_that_goes_quiet_is_left_to_the_idle_timeout() {
    let up = upstream(Script::opened().fast(1_200).quiet(1_500).fast(300).end()).await;
    let (gw, mut rx, _) = gateway(provider(&up, Some(MIN))).await;
    let (status, body) = post(gw, "/v1/messages", &messages(true)).await;
    assert_eq!(status, 200);
    assert!(!body.contains("event: error"), "{body}");
    finished(&ending(&mut rx).await);
}

// ───────────────────────────────────────────── 各种客户端格式的错误帧

/// 7. 转换过的流按客户端的格式、按 503 收尾
#[tokio::test]
async fn every_client_format_gets_its_own_retryable_error() {
    let cases: [(&str, Value, &[&str]); 3] = [
        (
            "/v1/chat/completions",
            json!({"model": "claude-sonnet-5", "stream": true,
                   "messages": [{"role": "user", "content": "hi"}]}),
            &["\"error\"", "Upstream `local` generated"],
        ),
        (
            "/v1/responses",
            json!({"model": "claude-sonnet-5", "stream": true, "input": "hi"}),
            &["response.failed", "\"server_error\""],
        ),
        (
            "/v1beta/models/claude-sonnet-5:streamGenerateContent?alt=sse",
            json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]}),
            &["\"UNAVAILABLE\"", "\"code\":503"],
        ),
    ];
    for (path, body, expect) in cases {
        let up = upstream(Script::opened().fast(300).slow(5_000).hang()).await;
        let (gw, mut rx, _) = gateway(provider(&up, Some(MIN))).await;
        let (status, out) = post(gw, path, &body).await;
        assert_eq!(status, 200, "{path}: {out}");
        let last = out.trim_end().rsplit("\n\n").next().unwrap();
        for e in expect {
            assert!(out.contains(e), "{path}: {e} not in {last}");
        }
        too_slow(&ending(&mut rx).await);
        assert!(eventually(&up.dropped).await, "{path}");
    }
}

/// 7. Bedrock 的 eventstream：拆成 SSE 之后照样测速，照样以 Anthropic 的 503 收尾
#[tokio::test]
async fn a_bedrock_eventstream_is_measured_too() {
    use aws_smithy_eventstream::frame::write_message_to;
    use aws_smithy_types::event_stream::{Header, HeaderValue, Message};
    fn event(kind: &str, payload: Value) -> Bytes {
        let m = Message::new(payload.to_string().into_bytes())
            .add_header(Header::new(
                ":message-type",
                HeaderValue::String("event".to_string().into()),
            ))
            .add_header(Header::new(
                ":event-type",
                HeaderValue::String(kind.to_string().into()),
            ))
            .add_header(Header::new(
                ":content-type",
                HeaderValue::String("application/json".into()),
            ));
        let mut out = Vec::new();
        write_message_to(&m, &mut out).unwrap();
        Bytes::from(out)
    }
    let delta = || {
        event(
            "contentBlockDelta",
            json!({"contentBlockIndex": 0, "delta": {"text": TEXT}}),
        )
    };
    let mut steps = vec![(0, event("messageStart", json!({"role": "assistant"})))];
    steps.extend((0..30).map(|_| (10, delta())));
    steps.extend((0..25).map(|_| (200, delta())));
    let up = upstream(Script {
        steps,
        hang: false,
        content_type: Some("application/vnd.amazon.eventstream"),
    })
    .await;
    let p = Provider {
        protocol: Some(Protocol::Bedrock),
        key: Some("ABSK-test".into()),
        ..provider(&up, Some(MIN))
    };
    let (gw, mut rx, _) = gateway(p).await;
    let (status, out) = post(
        gw,
        "/v1/messages",
        &json!({"model": "us.anthropic.claude-sonnet-4-5-20250929-v1:0", "max_tokens": 64,
                "stream": true, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, 200, "{out}");
    assert!(out.contains("\"overloaded_error\""), "{out}");
    too_slow(&ending(&mut rx).await);
}
