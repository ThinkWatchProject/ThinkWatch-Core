//! 无响应超时（`failover.idle_timeout_secs`）和手动中止（`tw_gateway::abort`），端到端。
//!
//! 假上游先回响应头（或者迟迟不回），之后按脚本隔一阵发一段，发完了挂着、隔一阵发一次心跳；
//! 它记下自己的响应被丢掉了没有 —— 网关放弃它、中止它时，连接要真的断开，上游才会停下。
//!
//! 超时写最短的 30 秒，测试把一秒调成三十分之一秒（`AppState::idle_tick`）：等的是 1 秒。

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{Value, json};
use tw_api::AttemptOutcome::{Aborted, Error, IdleTimeout, Served};
use tw_api::Event;
use tw_config::{Client, Config, Failover, Protocol, Provider};

/// 无响应超时在测试里有多长
const WINDOW: Duration = Duration::from_secs(1);

/// 一个假上游的样子：响应头之前等多久，之后按顺序隔多少毫秒发哪一段，发完了挂不挂着
#[derive(Clone)]
struct Script {
    header_delay_ms: u64,
    steps: Vec<(u64, &'static str)>,
    /// 发完了不收尾，每 100 毫秒发一次 `beat`，直到连接断开
    hang: bool,
    /// 挂着时发的心跳
    beat: &'static str,
    content_type: &'static str,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            header_delay_ms: 0,
            steps: Vec::new(),
            hang: false,
            beat: ": keep-alive\n\n",
            content_type: "text/event-stream",
        }
    }
}

/// 假上游被打了几次，以及它的响应（或者还没回的那个请求）被丢掉了没有
struct Upstream {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
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
    let hits = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let (h, d) = (hits.clone(), dropped.clone());
    let app = Router::new().fallback(axum::routing::any(move || {
        let (script, h, d) = (script.clone(), h.clone(), d.clone());
        async move {
            h.fetch_add(1, Ordering::SeqCst);
            // 响应头还没回时请求就被丢掉的，也要看得见
            let waiting = Flag(d.clone());
            tokio::time::sleep(Duration::from_millis(script.header_delay_ms)).await;
            std::mem::forget(waiting);
            let flag = Flag(d);
            let steps = futures::stream::iter(script.steps).then(|(ms, chunk)| async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Ok::<_, std::convert::Infallible>(Bytes::from_static(chunk.as_bytes()))
            });
            let (hang, beat) = (script.hang, script.beat);
            let tail = futures::stream::unfold((), move |_| async move {
                if !hang {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                Some((Ok(Bytes::from_static(beat.as_bytes())), ()))
            });
            let body = steps.chain(tail).map(move |x| {
                let _ = &flag;
                x
            });
            axum::response::Response::builder()
                .header("content-type", script.content_type)
                .body(axum::body::Body::from_stream(body))
                .unwrap()
        }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Upstream {
        addr,
        hits,
        dropped,
    }
}

const MESSAGE_START: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1200,\"cache_read_input_tokens\":800,\"output_tokens\":1}}}\n\n";
const PING: &str = "event: ping\ndata: {\"type\":\"ping\"}\n\n";
const THINKING: &str = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n\
     event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me think\"}}\n\n";
const WORD: &str = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"word \"}}\n\n";
const STOP: &str = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

/// 一段完整的回答，`word` 是正文
fn answer(word: &'static str) -> &'static str {
    let text = format!(
        "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{word}\"}}}}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    );
    Box::leak(text.into_boxed_str())
}

/// 开了流、报了输入，之后只有心跳
fn stalled() -> Script {
    Script {
        steps: vec![(0, MESSAGE_START), (0, PING)],
        hang: true,
        ..Default::default()
    }
}

/// 马上就答
fn prompt(word: &'static str) -> Script {
    Script {
        steps: vec![(0, MESSAGE_START), (0, answer(word))],
        ..Default::default()
    }
}

/// 整包的回答
fn whole(word: &'static str) -> &'static str {
    let text = format!(
        "{{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[{{\"type\":\"text\",\"text\":\"{word}\"}}],\"stop_reason\":\"end_turn\",\"usage\":{{\"input_tokens\":5,\"output_tokens\":1}}}}"
    );
    Box::leak(text.into_boxed_str())
}

fn provider(name: &str, up: &Upstream, protocol: Protocol) -> Provider {
    Provider {
        name: name.into(),
        base_url: format!("http://{}", up.addr),
        key: Some("sk-upstream".into()),
        protocol: Some(protocol),
        ..Default::default()
    }
}

/// 起网关，交回数据面的状态。`slot_wait_secs` 是等空位的期限
async fn gateway_with(
    providers: Vec<Provider>,
    slot_wait_secs: u64,
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
        providers,
        failover: Failover {
            idle_timeout_secs: 30,
            slot_wait_secs,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut state = tw_gateway::AppState::new(cfg).unwrap();
    state.idle_tick = WINDOW / 30;
    let rx = state.bus.subscribe();
    let addr = tw_gateway::serve(state.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, rx, state)
}

async fn gateway(
    providers: Vec<Provider>,
) -> (
    SocketAddr,
    tokio::sync::broadcast::Receiver<Event>,
    tw_gateway::AppState,
) {
    gateway_with(providers, 30).await
}

fn messages(stream: bool) -> Value {
    json!({"model": "claude-sonnet-5", "max_tokens": 1024, "stream": stream,
           "messages": [{"role": "user", "content": "Say hello."}]})
}

/// 发出去，交回状态码、`x-thinkwatch-error` 和整个正文
async fn send(gw: SocketAddr, path: &str, body: &Value) -> (u16, Option<String>, String) {
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
    let source = resp
        .headers()
        .get("x-thinkwatch-error")
        .map(|v| v.to_str().unwrap().to_string());
    (status, source, resp.text().await.unwrap())
}

async fn post(gw: SocketAddr, path: &str, body: &Value) -> (u16, String) {
    let (status, _, text) = send(gw, path, body).await;
    (status, text)
}

/// 下一个满足 `f` 的事件，最多等十秒
async fn next(
    rx: &mut tokio::sync::broadcast::Receiver<Event>,
    mut f: impl FnMut(&Event) -> bool,
) -> Event {
    loop {
        let e = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("没等到那个事件")
            .unwrap();
        if f(&e) {
            return e;
        }
    }
}

/// 这个请求的估算输入（开始事件带着）和尝试链
async fn estimate_and_attempts(
    rx: &mut tokio::sync::broadcast::Receiver<Event>,
) -> (Option<u64>, Vec<tw_api::AttemptView>) {
    let mut estimate = None;
    loop {
        match next(rx, |_| true).await {
            Event::RequestStarted { input_estimate, .. } => estimate = input_estimate,
            Event::RequestRouted { attempts, .. } => return (estimate, attempts),
            _ => {}
        }
    }
}

/// 请求的结局：结束是 Ok(状态码)，失败是 Err((来源, 码))，取消是 panic
async fn outcome(
    rx: &mut tokio::sync::broadcast::Receiver<Event>,
) -> Result<u16, (tw_api::FailureSource, String)> {
    match next(rx, |e| {
        matches!(
            e,
            Event::RequestFinished { .. }
                | Event::RequestFailed { .. }
                | Event::RequestCancelled { .. }
        )
    })
    .await
    {
        Event::RequestFinished { status, .. } => Ok(status),
        Event::RequestFailed {
            source, message, ..
        } => Err((source, message.code)),
        e => panic!("不该是取消：{e:?}"),
    }
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

fn outcomes(attempts: &[tw_api::AttemptView]) -> Vec<(&str, tw_api::AttemptOutcome)> {
    attempts
        .iter()
        .map(|a| (a.provider.as_str(), a.outcome))
        .collect()
}

// ───────────────────────────────────────────── 还没有内容：换下一家

#[tokio::test]
async fn a_quiet_first_upstream_is_dropped_after_the_window_and_the_next_one_answers() {
    let slow = upstream(stalled()).await;
    let good = upstream(prompt("hello")).await;
    let (gw, mut rx, state) = gateway(vec![
        provider("slow", &slow, Protocol::Anthropic),
        provider("good", &good, Protocol::Anthropic),
    ])
    .await;

    let t = Instant::now();
    let (status, text) = post(gw, "/v1/messages", &messages(true)).await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("hello"), "{text}");
    assert!(
        !text.contains("ping") && text.matches("event: message_start").count() == 1,
        "放弃的那一家的开头不该到客户端：{text}"
    );
    assert!(t.elapsed() >= WINDOW, "等够了才换");
    assert!(
        eventually(&slow.dropped).await,
        "放弃的那一家连接要断开，它才会停下"
    );

    let (_, attempts) = estimate_and_attempts(&mut rx).await;
    assert_eq!(
        outcomes(&attempts),
        [("slow", IdleTimeout), ("good", Served)]
    );
    let gave_up = &attempts[0];
    assert_eq!(gave_up.status, Some(200));
    assert_eq!(
        gave_up.usage,
        Some(tw_api::AttemptUsage {
            input: 1200,
            cache_read: 800,
            cache_write: 0,
            estimated: false,
        }),
        "上游在开头报了输入，记它报的"
    );
    let said = gave_up.error.as_ref().expect("说等了多久");
    assert_eq!(said.code, "gw.upstream.idle_timeout");
    assert_eq!(said.args["secs"], "30", "说的是配置写的秒数");
    assert_eq!(outcome(&mut rx).await, Ok(200));

    // **记一次失败**：和 5xx 一样算进停用的账，连着三次就停用
    for _ in 0..2 {
        let (status, _) = post(gw, "/v1/messages", &messages(true)).await;
        assert_eq!(status, 200);
    }
    assert!(!state.health.is_available("slow"), "三次超时之后停用");
    let rates = state
        .health
        .success_rates(&["slow".to_string(), "good".to_string()]);
    assert_eq!(rates.get("slow"), None, "不到五个样本不给成功率：{rates:?}");
    let (status, _) = post(gw, "/v1/messages", &messages(true)).await;
    assert_eq!(status, 200);
    assert_eq!(slow.hits.load(Ordering::SeqCst), 3, "停用之后不再先试它");
}

#[tokio::test]
async fn response_headers_that_never_come_time_out_the_same_way() {
    // 响应头都迟迟不来（中转站排着队）：一样从发出请求算起，到点放弃
    let queued = upstream(Script {
        header_delay_ms: 5_000,
        ..prompt("too late")
    })
    .await;
    let good = upstream(prompt("hello")).await;
    let (gw, mut rx, _) = gateway(vec![
        provider("queued", &queued, Protocol::Anthropic),
        provider("good", &good, Protocol::Anthropic),
    ])
    .await;
    let t = Instant::now();
    let (status, text) = post(gw, "/v1/messages", &messages(true)).await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("hello"), "{text}");
    assert!(t.elapsed() < Duration::from_secs(4), "没有等到响应头");
    assert!(eventually(&queued.dropped).await, "还在等的请求要断开");
    let (_, attempts) = estimate_and_attempts(&mut rx).await;
    assert_eq!(
        outcomes(&attempts),
        [("queued", IdleTimeout), ("good", Served)]
    );
    assert_eq!(attempts[0].status, None, "响应头没到，没有状态码");
    assert!(attempts[0].usage.is_some_and(|u| u.estimated));
}

#[tokio::test]
async fn when_no_upstream_is_left_the_client_gets_a_timeout_and_nothing_waits_forever() {
    // 两家的响应头都不来：第二家也是到点就不等了，客户端收到它那种格式的 504
    let a = upstream(Script {
        header_delay_ms: 60_000,
        ..prompt("never")
    })
    .await;
    let b = upstream(Script {
        header_delay_ms: 60_000,
        ..prompt("never")
    })
    .await;
    let (gw, mut rx, _) = gateway(vec![
        provider("a", &a, Protocol::Anthropic),
        provider("b", &b, Protocol::Anthropic),
    ])
    .await;
    let t = Instant::now();
    let (status, source, text) = send(gw, "/v1/messages", &messages(true)).await;
    assert_eq!(status, 504, "{text}");
    assert_eq!(source.as_deref(), Some("upstream"));
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["error"]["type"], "timeout_error", "{text}");
    let message = v["error"]["message"].as_str().unwrap();
    assert!(message.starts_with("[ThinkWatch]"), "{message}");
    assert!(message.contains("tried: a → b"), "{message}");
    let waited = t.elapsed();
    assert!(
        waited >= WINDOW * 2 && waited < Duration::from_secs(5),
        "{waited:?}"
    );
    assert!(eventually(&b.dropped).await);
    let (_, attempts) = estimate_and_attempts(&mut rx).await;
    assert_eq!(
        outcomes(&attempts),
        [("a", IdleTimeout), ("b", IdleTimeout)]
    );
    assert_eq!(
        outcome(&mut rx).await,
        Err((
            tw_api::FailureSource::Upstream,
            "gw.upstream.idle_timeout".into()
        ))
    );
}

#[tokio::test]
async fn the_last_upstream_that_says_nothing_ends_the_stream_with_an_error() {
    // 唯一的一家开了流、只发心跳：响应头已经交出去了（最后一家不压开头），到点由流里的
    // 一条错误收尾，记成没有内容的超时
    let only = upstream(stalled()).await;
    let (gw, mut rx, state) = gateway(vec![provider("only", &only, Protocol::Anthropic)]).await;
    let t = Instant::now();
    let (status, text) = post(gw, "/v1/messages", &messages(true)).await;
    assert_eq!(status, 200, "{text}");
    assert!(t.elapsed() < Duration::from_secs(4));
    assert!(text.contains("event: error"), "{text}");
    assert!(text.contains("timeout_error"), "{text}");
    assert!(eventually(&only.dropped).await);
    assert_eq!(
        outcome(&mut rx).await,
        Err((
            tw_api::FailureSource::Upstream,
            "gw.upstream.idle_timeout".into()
        ))
    );
    // 一个字都没给：这一家也记一次失败，三次之后停用（只有它一家时请求照样发给它）
    assert!(state.health.is_available("only"));
    for _ in 0..2 {
        let (status, _) = post(gw, "/v1/messages", &messages(true)).await;
        assert_eq!(status, 200);
    }
    assert!(
        !state.health.is_available("only"),
        "三次没有内容的超时之后停用"
    );
}

#[tokio::test]
async fn pings_alone_do_not_keep_an_upstream_alive() {
    // 只发 Anthropic 的 `ping` 事件、Responses 的 `response.in_progress`：心跳不算，照样超时
    for (protocol, path, body, start, beat) in [
        (
            Protocol::Anthropic,
            "/v1/messages",
            messages(true),
            MESSAGE_START,
            PING,
        ),
        (
            Protocol::OpenaiResponses,
            "/v1/responses",
            json!({"model": "gpt-5", "stream": true, "input": "Say hello."}),
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.in_progress\ndata: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        ),
        (
            Protocol::OpenaiChat,
            "/v1/chat/completions",
            json!({"model": "gpt-5", "stream": true, "messages": [{"role": "user", "content": "hi"}]}),
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{}}]}\n\n",
        ),
    ] {
        let only = upstream(Script {
            steps: vec![(0, start)],
            hang: true,
            beat,
            ..Default::default()
        })
        .await;
        let (gw, mut rx, _) = gateway(vec![provider("only", &only, protocol)]).await;
        let t = Instant::now();
        let (status, text) = post(gw, path, &body).await;
        assert_eq!(status, 200, "{protocol:?}: {text}");
        assert!(t.elapsed() < Duration::from_secs(4), "{protocol:?}");
        assert!(text.contains("[ThinkWatch]"), "{protocol:?}: {text}");
        assert_eq!(
            outcome(&mut rx).await,
            Err((
                tw_api::FailureSource::Upstream,
                "gw.upstream.idle_timeout".into()
            )),
            "{protocol:?}"
        );
    }
}

#[tokio::test]
async fn a_whole_answer_that_does_not_come_in_time_moves_on() {
    // 整包的请求：从发出去到整份回来算一段。响应头迟迟不来的，换下一家
    let slow = upstream(Script {
        header_delay_ms: 5_000,
        steps: vec![(0, whole("late"))],
        content_type: "application/json",
        ..Default::default()
    })
    .await;
    // 响应头马上到、正文一直不来（只有空格）的，也换
    let mute = upstream(Script {
        steps: vec![(0, " ")],
        hang: true,
        beat: " ",
        content_type: "application/json",
        ..Default::default()
    })
    .await;
    let good = upstream(Script {
        steps: vec![(0, whole("whole"))],
        content_type: "application/json",
        ..Default::default()
    })
    .await;
    let (gw, mut rx, _) = gateway(vec![
        provider("slow", &slow, Protocol::Anthropic),
        provider("mute", &mute, Protocol::Anthropic),
        provider("good", &good, Protocol::Anthropic),
    ])
    .await;
    let (status, text) = post(gw, "/v1/messages", &messages(false)).await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("whole"), "{text}");
    assert!(eventually(&slow.dropped).await && eventually(&mute.dropped).await);
    let (_, attempts) = estimate_and_attempts(&mut rx).await;
    assert_eq!(
        outcomes(&attempts),
        [
            ("slow", IdleTimeout),
            ("mute", IdleTimeout),
            ("good", Served)
        ]
    );
    assert_eq!(attempts[1].status, Some(200));

    // 只有一家、整份不来：504，不是一直等
    let only = upstream(Script {
        header_delay_ms: 60_000,
        steps: vec![(0, whole("never"))],
        content_type: "application/json",
        ..Default::default()
    })
    .await;
    let (gw, _, _) = gateway(vec![provider("only", &only, Protocol::Anthropic)]).await;
    let (status, source, text) = send(gw, "/v1/messages", &messages(false)).await;
    assert_eq!(status, 504, "{text}");
    assert_eq!(source.as_deref(), Some("upstream"));
}

// ───────────────────────────────────────────── 内容在来：不算超时

#[tokio::test]
async fn a_slow_but_steady_stream_is_not_cut_off() {
    // 每 400 毫秒一段，一共三秒：总时长远超 1 秒，但没有哪一段间隔超过
    let mut steps = vec![(0, MESSAGE_START)];
    steps.extend(std::iter::repeat_n((400, WORD), 7));
    steps.push((400, STOP));
    let steady = upstream(Script {
        steps,
        ..Default::default()
    })
    .await;
    let other = upstream(prompt("other")).await;
    let (gw, mut rx, _) = gateway(vec![
        provider("steady", &steady, Protocol::Anthropic),
        provider("other", &other, Protocol::Anthropic),
    ])
    .await;
    let t = Instant::now();
    let (status, text) = post(gw, "/v1/messages", &messages(true)).await;
    assert_eq!(status, 200, "{text}");
    assert!(t.elapsed() >= Duration::from_secs(3));
    assert_eq!(text.matches("word ").count(), 7, "{text}");
    assert!(!text.contains("event: error"), "{text}");
    assert_eq!(other.hits.load(Ordering::SeqCst), 0);
    let (_, attempts) = estimate_and_attempts(&mut rx).await;
    assert_eq!(outcomes(&attempts), [("steady", Served)]);
    assert_eq!(outcome(&mut rx).await, Ok(200));
}

#[tokio::test]
async fn thinking_counts_as_content() {
    // 开头在想（推理的字在流），正文要过一阵才来：每一段间隔都不到 1 秒，加起来超过
    let thinker = upstream(Script {
        steps: vec![
            (0, MESSAGE_START),
            (600, THINKING),
            (600, answer("thought")),
        ],
        ..Default::default()
    })
    .await;
    let other = upstream(prompt("other")).await;
    let (gw, mut rx, _) = gateway(vec![
        provider("thinker", &thinker, Protocol::Anthropic),
        provider("other", &other, Protocol::Anthropic),
    ])
    .await;
    let (status, text) = post(gw, "/v1/messages", &messages(true)).await;
    assert_eq!(status, 200, "{text}");
    assert!(
        text.contains("Let me think") && text.contains("thought"),
        "{text}"
    );
    assert_eq!(other.hits.load(Ordering::SeqCst), 0);
    let (_, attempts) = estimate_and_attempts(&mut rx).await;
    assert_eq!(outcomes(&attempts), [("thinker", Served)]);
    assert_eq!(outcome(&mut rx).await, Ok(200));
}

// ───────────────────────────────────────────── 已经有内容：报错收尾

#[tokio::test]
async fn content_then_silence_ends_with_an_error_the_client_understands() {
    for (protocol, path, body, said, end) in [
        (
            Protocol::Anthropic,
            "/v1/messages",
            messages(true),
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-5\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n\
             event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
             event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"half an ans\"}}\n\n",
            "event: error\n",
        ),
        (
            Protocol::OpenaiChat,
            "/v1/chat/completions",
            json!({"model": "gpt-5", "stream": true, "messages": [{"role": "user", "content": "hi"}]}),
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"half an ans\"}}]}\n\n",
            "\"error\"",
        ),
        (
            Protocol::OpenaiResponses,
            "/v1/responses",
            json!({"model": "gpt-5", "stream": true, "input": "Say hello."}),
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n\
             event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"half an ans\"}\n\n",
            "event: response.failed\n",
        ),
        (
            Protocol::Gemini,
            "/v1beta/models/gemini-3-pro:streamGenerateContent?alt=sse",
            json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]}),
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"half an ans\"}]}}]}\r\n\r\n",
            "DEADLINE_EXCEEDED",
        ),
    ] {
        let up = upstream(Script {
            steps: vec![(0, said)],
            hang: true,
            ..Default::default()
        })
        .await;
        // 两家：第一家的开头压到了内容，交出去之后才停住 —— 那时已经换不了
        let spare = upstream(prompt("spare")).await;
        let (gw, mut rx, state) = gateway(vec![
            provider("half", &up, protocol),
            provider("spare", &spare, protocol),
        ])
        .await;
        let (status, text) = post(gw, path, &body).await;
        assert_eq!(status, 200, "{protocol:?}: {text}");
        assert!(text.contains("half an ans"), "{protocol:?}: {text}");
        let tail = &text[text.find("half an ans").unwrap()..];
        assert!(tail.contains(end), "{protocol:?}: {text}");
        assert!(tail.contains("[ThinkWatch]"), "{protocol:?}: {text}");
        assert_eq!(spare.hits.load(Ordering::SeqCst), 0, "{protocol:?}");
        assert!(eventually(&up.dropped).await, "{protocol:?}");
        assert_eq!(
            outcome(&mut rx).await,
            Err((
                tw_api::FailureSource::Upstream,
                "gw.upstream.idle_timeout_mid_stream".into()
            )),
            "{protocol:?}"
        );
        // 答到一半停住的不记失败：和流在半路断了一样
        assert!(state.health.is_available("half"));
    }
}

// ───────────────────────────────────────────── 等空位不算在里面

#[tokio::test]
async fn waiting_for_a_slot_is_not_counted_as_silence() {
    // 唯一的一家满着：等了 1.5 秒才空出来，之后 600 毫秒出内容。等的那段不算，照常答上
    let late = upstream(Script {
        steps: vec![(0, MESSAGE_START), (600, answer("patience"))],
        ..Default::default()
    })
    .await;
    let (gw, mut rx, state) = gateway_with(
        vec![Provider {
            max_concurrent: Some(1),
            ..provider("late", &late, Protocol::Anthropic)
        }],
        5,
    )
    .await;
    let held = state.slots.try_take("late").expect("一个空位");
    let asking = tokio::spawn(async move { post(gw, "/v1/messages", &messages(true)).await });
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    drop(held);
    let (status, text) = asking.await.unwrap();
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("patience"), "{text}");
    assert!(!text.contains("event: error"), "{text}");
    let (_, attempts) = estimate_and_attempts(&mut rx).await;
    // 先是满着跳过的那一行，等到空位之后发出去的是第二行
    assert_eq!(outcomes(&attempts), [("late", Error), ("late", Served)]);
    assert!(attempts[1].queued_ms.is_some_and(|ms| ms >= 1_000));
}

/// 放弃的那一家占着的位置（`max_concurrent`，见 `tw_gateway::slots`）**当场**还回去：接下
/// 请求的那一家还在答，慢的那一家已经空出来了，不等这个请求结束
#[tokio::test]
async fn the_slot_of_an_upstream_given_up_on_is_free_at_once() {
    const CONTENT: &str = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n";
    let slow = upstream(stalled()).await;
    // 先答上，隔一会儿才说完：这段时间里请求还没结束
    let good = upstream(Script {
        steps: vec![(0, MESSAGE_START), (0, CONTENT), (800, STOP)],
        ..Default::default()
    })
    .await;
    let (gw, _, state) = gateway(vec![
        Provider {
            max_concurrent: Some(1),
            ..provider("slow", &slow, Protocol::Anthropic)
        },
        provider("good", &good, Protocol::Anthropic),
    ])
    .await;

    let asking = tokio::spawn(async move { post(gw, "/v1/messages", &messages(true)).await });
    // 还在等慢的那一家：它的位置占着
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(slow.hits.load(Ordering::SeqCst), 1);
    assert!(state.slots.try_take("slow").is_none(), "等着的时候占着位置");
    // 换到了好的那一家：慢的那一家的位置已经还回来了，这个请求还没结束
    for _ in 0..60 {
        if good.hits.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(good.hits.load(Ordering::SeqCst), 1);
    assert!(!asking.is_finished(), "测的是请求还在进行时");
    assert!(
        state.slots.try_take("slow").is_some(),
        "放弃的那一家的位置要当场还回去"
    );
    let (status, text) = asking.await.unwrap();
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("hello"), "{text}");
}

/// 并发数满着的那一家（`max_concurrent`）
fn full(name: &str, up: &Upstream) -> Provider {
    Provider {
        max_concurrent: Some(1),
        ..provider(name, up, Protocol::Anthropic)
    }
}

/// 跳过时满着的那一家，在放弃不出声的那一家之后空出来了：换到它
#[tokio::test]
async fn an_upstream_that_frees_meanwhile_takes_over() {
    let busy = upstream(prompt("freed")).await;
    let slow = upstream(stalled()).await;
    let (gw, mut rx, state) = gateway_with(
        vec![
            full("busy", &busy),
            provider("slow", &slow, Protocol::Anthropic),
        ],
        3,
    )
    .await;
    let held = state.slots.try_take("busy").expect("一个空位");
    let asking = tokio::spawn(async move { post(gw, "/v1/messages", &messages(true)).await });
    tokio::time::sleep(Duration::from_millis(1_400)).await;
    drop(held);
    let (status, text) = asking.await.unwrap();
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("freed"), "{text}");
    let (_, attempts) = estimate_and_attempts(&mut rx).await;
    assert_eq!(
        outcomes(&attempts),
        [("busy", Error), ("slow", IdleTimeout), ("busy", Served)]
    );
}

/// 剩下的那一家一直满着：不出声的那一家放弃之后等它到期限，等不到就交出超时，不是「都满着」
/// 的 429 —— 有一家真的收到过请求
#[tokio::test]
async fn a_next_upstream_that_never_frees_leaves_the_timeout_to_tell() {
    let busy = upstream(prompt("busy")).await;
    let slow = upstream(stalled()).await;
    let (gw, mut rx, state) = gateway_with(
        vec![
            full("busy", &busy),
            provider("slow", &slow, Protocol::Anthropic),
        ],
        2,
    )
    .await;
    let _held = state.slots.try_take("busy").expect("一个空位");
    let (status, source, text) = send(gw, "/v1/messages", &messages(true)).await;
    assert_eq!(status, 504, "{text}");
    assert_eq!(source.as_deref(), Some("upstream"));
    assert_eq!(busy.hits.load(Ordering::SeqCst), 0);
    let (_, attempts) = estimate_and_attempts(&mut rx).await;
    assert_eq!(
        outcomes(&attempts),
        [("busy", Error), ("slow", IdleTimeout)]
    );
}

// ───────────────────────────────────────────── 手动中止

/// 下一个开始的请求：号和会话
async fn started(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> (u64, Option<String>) {
    match next(rx, |e| matches!(e, Event::RequestStarted { .. })).await {
        Event::RequestStarted { id, session, .. } => (id, session),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn aborting_before_the_answer_starts_answers_499_and_leaves_the_upstream_alone() {
    let queued = upstream(Script {
        header_delay_ms: 60_000,
        ..prompt("never")
    })
    .await;
    let other = upstream(prompt("other")).await;
    let (gw, mut rx, state) = gateway(vec![
        provider("queued", &queued, Protocol::Anthropic),
        provider("other", &other, Protocol::Anthropic),
    ])
    .await;
    let asking = tokio::spawn(async move { send(gw, "/v1/messages", &messages(true)).await });
    let (id, _) = started(&mut rx).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    state.aborts.request(id).expect("还在跑");

    let (status, source, text) = asking.await.unwrap();
    assert_eq!(status, 499, "{text}");
    assert_eq!(source.as_deref(), Some("aborted"));
    let v: Value = serde_json::from_str(&text).unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("[ThinkWatch]"),
        "{text}"
    );
    assert!(eventually(&queued.dropped).await, "和上游的连接要断开");
    // 不换下一家
    assert_eq!(other.hits.load(Ordering::SeqCst), 0);
    let (_, attempts) = estimate_and_attempts(&mut rx).await;
    assert_eq!(outcomes(&attempts), [("queued", Aborted)]);
    assert_eq!(
        attempts[0].error.as_ref().map(|m| m.code.as_str()),
        Some(tw_api::ABORTED)
    );
    assert_eq!(
        outcome(&mut rx).await,
        Err((tw_api::FailureSource::Aborted, tw_api::ABORTED.into()))
    );
    // 不是上游的错
    assert!(state.health.is_available("queued"));
    for _ in 0..5 {
        state.health.record_success("queued");
    }
    assert_eq!(
        state
            .health
            .success_rates(&["queued".to_string()])
            .get("queued"),
        Some(&1.0),
        "中止不算一次失败"
    );
    // 结束了的再叫停：不在跑
    assert!(state.aborts.request(id).is_err());
    assert!(state.aborts.is_empty());
}

#[tokio::test]
async fn aborting_mid_stream_ends_the_answer_with_an_error_in_its_format() {
    let mut steps = vec![(0, MESSAGE_START)];
    steps.extend(std::iter::repeat_n((200, WORD), 50));
    let talker = upstream(Script {
        steps,
        ..Default::default()
    })
    .await;
    let (gw, mut rx, state) = gateway(vec![provider("talker", &talker, Protocol::Anthropic)]).await;
    let asking = tokio::spawn(async move { post(gw, "/v1/messages", &messages(true)).await });
    let (id, _) = started(&mut rx).await;
    // 等它答上几个字
    tokio::time::sleep(Duration::from_millis(700)).await;
    state.aborts.request(id).expect("还在跑");
    let (status, text) = asking.await.unwrap();
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("word "), "{text}");
    assert!(text.matches("word ").count() < 50, "没等到说完：{text}");
    let tail = &text[text.rfind("word ").unwrap()..];
    assert!(tail.contains("event: error"), "{text}");
    assert!(tail.contains("[ThinkWatch]"), "{text}");
    assert!(eventually(&talker.dropped).await, "上游要停下");
    match next(&mut rx, |e| matches!(e, Event::RequestFailed { .. })).await {
        Event::RequestFailed {
            source,
            message,
            usage,
            ..
        } => {
            assert_eq!(source, tw_api::FailureSource::Aborted);
            assert_eq!(message.code, tw_api::ABORTED);
            assert!(
                !message.text.contains("stream broke"),
                "中止不是流断了：{}",
                message.text
            );
            assert!(usage.is_some(), "上游已经计了费，用量照记");
        }
        _ => unreachable!(),
    }
    assert!(state.health.is_available("talker"));
}

#[tokio::test]
async fn aborting_a_session_stops_every_request_of_it_and_nothing_else() {
    let mut steps = vec![(0, MESSAGE_START)];
    steps.extend(std::iter::repeat_n((200, WORD), 50));
    let talker = upstream(Script {
        steps,
        ..Default::default()
    })
    .await;
    let (gw, mut rx, state) = gateway(vec![provider("talker", &talker, Protocol::Anthropic)]).await;
    // 同一段对话的两个请求（指纹一样，归同一次会话），和另一段对话的一个
    let other = json!({"model": "claude-sonnet-5", "max_tokens": 1024, "stream": true,
                       "system": "Another conversation entirely.",
                       "messages": [{"role": "user", "content": "Something else."}]});
    let a = tokio::spawn(async move { post(gw, "/v1/messages", &messages(true)).await });
    let (first, session) = started(&mut rx).await;
    let b = tokio::spawn(async move { post(gw, "/v1/messages", &messages(true)).await });
    let (second, again) = started(&mut rx).await;
    let c = tokio::spawn(async move { post(gw, "/v1/messages", &other).await });
    let (third, elsewhere) = started(&mut rx).await;
    let session = session.expect("认得出会话");
    assert_eq!(again.as_deref(), Some(session.as_str()));
    assert_ne!(elsewhere.as_deref(), Some(session.as_str()));
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(state.aborts.session(&session), vec![first, second]);
    for asking in [a, b] {
        let (status, text) = asking.await.unwrap();
        assert_eq!(status, 200);
        assert!(text.contains("event: error"), "{text}");
    }
    assert!(state.aborts.session(&session).is_empty(), "都结束了");
    // 另一段对话照常
    state.aborts.request(third).expect("另一段对话还在跑");
    let (_, text) = c.await.unwrap();
    assert!(text.contains("event: error"), "{text}");
}
