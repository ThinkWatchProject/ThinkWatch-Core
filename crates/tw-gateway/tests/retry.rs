//! 最后一家暂时出错时退避重试（`tw_gateway::retry`），端到端。
//!
//! 假上游按脚本一个个地答：先过载、5xx、断流几次，之后照常答。退避的一秒在测试里调成一百毫秒
//! （`AppState::retry_backoff`），不浮动：退避 200、400、800 毫秒…… 等待期限
//! （`failover.wait_secs`）是真的秒数。

mod common;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use futures::StreamExt;
use tw_api::{AttemptOutcome, AttemptView, Event};

/// 测试里退避的一秒
const TICK: Duration = Duration::from_millis(100);

/// 假上游的一次答复
#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
    /// 流式的回答：响应头之后一段一段地给。有的话 `body` 不用
    chunks: Vec<String>,
    /// 给完 `chunks` 之后把连接断掉，不好好收尾
    broken: bool,
    content_type: &'static str,
}

/// 整包地照常答
fn ok() -> Reply {
    Reply {
        status: 200,
        headers: Vec::new(),
        body: r#"{"type":"message","role":"assistant","model":"claude-sonnet-5","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"cache_read_input_tokens":5000,"output_tokens":1}}"#.into(),
        chunks: Vec::new(),
        broken: false,
        content_type: "application/json",
    }
}

/// 回一个错误状态码，正文是 `body`
fn status(code: u16, body: &str) -> Reply {
    Reply {
        status: code,
        body: body.into(),
        ..ok()
    }
}

fn overloaded_503() -> Reply {
    status(
        503,
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
    )
}

/// Anthropic 流式回答里的一个事件
fn event(name: &str, data: &str) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

fn message_start() -> String {
    event(
        "message_start",
        r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-5","content":[],"stop_reason":null,"usage":{"input_tokens":3,"output_tokens":0}}}"#,
    )
}

fn text(word: &str) -> String {
    event(
        "content_block_delta",
        &format!(
            r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{word}"}}}}"#
        ),
    )
}

fn overloaded_event() -> String {
    event(
        "error",
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
    )
}

fn sse(chunks: Vec<String>) -> Reply {
    Reply {
        chunks,
        content_type: "text/event-stream",
        ..ok()
    }
}

/// 流式地照常答：开头、一段内容「ok」、收尾
fn streamed() -> Reply {
    sse(vec![
        message_start(),
        event(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        text("ok"),
        event(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        event(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
        ),
        event("message_stop", r#"{"type":"message_stop"}"#),
    ])
}

/// 响应头是 200、流也开了，开头之后在流里报过载，一段内容都还没有
fn overloaded_in_the_stream() -> Reply {
    sse(vec![message_start(), overloaded_event()])
}

/// 一家假上游：先按 `script` 的顺序答，答完了一律答 `then`
struct Up {
    addr: SocketAddr,
    hits: Arc<Mutex<Vec<Instant>>>,
}

impl Up {
    fn hits(&self) -> usize {
        self.hits.lock().unwrap().len()
    }
}

fn app(script: Vec<Reply>, then: Reply, hits: Arc<Mutex<Vec<Instant>>>) -> Router {
    let script = Arc::new(Mutex::new(VecDeque::from(script)));
    Router::new().fallback(axum::routing::any(move || {
        let (script, h, then) = (script.clone(), hits.clone(), then.clone());
        async move {
            h.lock().unwrap().push(Instant::now());
            let r = script.lock().unwrap().pop_front().unwrap_or(then);
            let mut b = axum::response::Response::builder()
                .status(r.status)
                .header("content-type", r.content_type);
            for (k, v) in &r.headers {
                b = b.header(*k, v.as_str());
            }
            if r.chunks.is_empty() {
                return b.body(axum::body::Body::from(r.body)).unwrap();
            }
            let broken = r.broken;
            let chunks = futures::stream::iter(r.chunks)
                .map(|c| Ok::<_, std::io::Error>(bytes::Bytes::from(c)))
                .chain(futures::stream::iter(
                    broken.then(|| Err(std::io::Error::other("cut"))),
                ));
            b.body(axum::body::Body::from_stream(chunks)).unwrap()
        }
    }))
}

async fn upstream(script: Vec<Reply>, then: Reply) -> Up {
    let hits = Arc::new(Mutex::new(Vec::new()));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = app(script, then, hits.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Up { addr, hits }
}

/// 一家或者两家（按顺序的组「池」）上游，默认路由把请求都交给它们
fn config(ups: &[(SocketAddr, &str)], wait_secs: u64) -> tw_config::Config {
    let names = ["甲", "乙"];
    let mut yaml =
        "version: 1\ndefault_route: 默认\nclients:\n  - name: me\n    key: tw-k\nproviders:\n"
            .to_string();
    for (n, (addr, protocol)) in names.iter().zip(ups) {
        yaml += &format!(
            "  - name: {n}\n    base_url: http://{addr}\n    key: sk-x\n    protocol: {protocol}\n"
        );
    }
    let members = names[..ups.len()].join(", ");
    yaml += &format!(
        "groups:\n  - name: 池\n    type: fallback\n    providers: [{members}]\nroutes:\n  - name: 默认\n    rules:\n      - name: 全部\n        to: 池\nfailover:\n  wait_secs: {wait_secs}\n"
    );
    serde_yaml_ng::from_str(&yaml).unwrap()
}

fn anthropic(ups: &[&Up], wait_secs: u64) -> tw_config::Config {
    let ups: Vec<_> = ups.iter().map(|u| (u.addr, "anthropic")).collect();
    config(&ups, wait_secs)
}

/// 总线上发生过的事件，后台一直收着
#[derive(Clone)]
struct Log(Arc<Mutex<Vec<Event>>>);

impl Log {
    async fn until<T>(&self, what: &str, f: impl Fn(&[Event]) -> Option<T>) -> T {
        for _ in 0..500 {
            if let Some(t) = f(&self.0.lock().unwrap()) {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("10 秒内没等到{what}：{:#?}", self.0.lock().unwrap());
    }

    /// 客户端写的模型是 `model` 的那个请求的号
    async fn id(&self, model: &str) -> u64 {
        self.until(&format!(" {model} 开始"), |evs| {
            evs.iter().find_map(|e| match e {
                Event::RequestStarted { id, model: m, .. } if m == model => Some(*id),
                _ => None,
            })
        })
        .await
    }

    /// 那个请求的尝试链
    async fn attempts(&self, model: &str) -> Vec<AttemptView> {
        let id = self.id(model).await;
        self.until(&format!(" {model} 的路由"), |evs| {
            evs.iter().find_map(|e| match e {
                Event::RequestRouted {
                    id: i, attempts, ..
                } if *i == id => Some(attempts.clone()),
                _ => None,
            })
        })
        .await
    }

    /// 那个请求的退避和排队事件，按先后
    async fn waits(&self, model: &str) -> Vec<Event> {
        let id = self.id(model).await;
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|e| match e {
                Event::RequestRetrying { id: i, .. }
                | Event::RequestQueued { id: i, .. }
                | Event::RequestDequeued { id: i, .. } => *i == id,
                _ => false,
            })
            .cloned()
            .collect()
    }
}

/// 起网关。退避的一秒是 `tick`，不浮动；流式回答最多压 `hold` 不给响应头
async fn serve_with(
    cfg: tw_config::Config,
    tick: Duration,
    hold: Duration,
) -> (SocketAddr, tw_gateway::AppState, Log) {
    let mut state = tw_gateway::AppState::new(cfg).unwrap();
    state.retry_backoff = tw_gateway::retry::Backoff { tick, jitter: 0.0 };
    state.opening_hold = hold;
    state.keepalive_every = Duration::from_millis(150);
    let mut rx = state.bus.subscribe();
    let log = Log(Arc::default());
    let into = log.0.clone();
    tokio::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            into.lock().unwrap().push(ev);
        }
    });
    let addr = tw_gateway::serve(state.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    (addr, state, log)
}

async fn serve(cfg: tw_config::Config) -> (SocketAddr, tw_gateway::AppState, Log) {
    serve_with(cfg, TICK, Duration::from_secs(10)).await
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

/// 一个 Anthropic 的请求，客户端写的模型是 `model`（认请求用）。`conversation` 给了的话带着
/// 会话头，`turn` 是这段对话的第几轮
fn ask_in(
    gw: SocketAddr,
    model: &str,
    stream: bool,
    conversation: Option<&str>,
    turn: usize,
) -> reqwest::RequestBuilder {
    let mut messages = Vec::new();
    for i in 0..turn {
        if i > 0 {
            messages.push(r#"{"role":"assistant","content":"ok"}"#.to_string());
        }
        let who = conversation.unwrap_or(model);
        messages.push(format!(r#"{{"role":"user","content":"{who}：第 {i} 句"}}"#));
    }
    let body = format!(
        r#"{{"model":"{model}","max_tokens":16,"stream":{stream},"system":"你是一个助手","messages":[{}]}}"#,
        messages.join(",")
    );
    let mut r = client()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .header("anthropic-version", "2023-06-01")
        .body(body);
    if let Some(c) = conversation {
        r = r.header("x-claude-code-session-id", c);
    }
    r
}

fn ask(gw: SocketAddr, model: &str) -> reqwest::RequestBuilder {
    ask_in(gw, model, false, None, 1)
}

fn ask_stream(gw: SocketAddr, model: &str) -> reqwest::RequestBuilder {
    ask_in(gw, model, true, None, 1)
}

/// 尝试链上每一跳的 (结果, 第几次重试, 退避过没有)
fn shape(chain: &[AttemptView]) -> Vec<(AttemptOutcome, Option<u32>, bool)> {
    chain
        .iter()
        .map(|a| (a.outcome, a.retry, a.backoff_ms.is_some()))
        .collect()
}

#[tokio::test]
async fn an_overload_in_the_stream_before_content_is_retried_on_the_lone_upstream() {
    let up = upstream(vec![overloaded_in_the_stream()], streamed()).await;
    let (gw, state, log) = serve(anthropic(&[&up], 10)).await;
    let r = tokio::spawn(ask_stream(gw, "单家流里过载").send());
    // 退避的时候半路才来看的一方也看得出它在重试
    let id = log.id("单家流里过载").await;
    let at = log
        .until("退避", |evs| {
            evs.iter().find_map(|e| match e {
                Event::RequestRetrying {
                    id: i,
                    upstreams,
                    attempt,
                    retry_at_ms,
                    until_ms,
                    at_ms,
                } if *i == id => Some((
                    upstreams.clone(),
                    *attempt,
                    *retry_at_ms - *at_ms,
                    *until_ms - *at_ms,
                )),
                _ => None,
            })
        })
        .await;
    let (upstreams, attempt, wait, left) = at;
    assert_eq!((upstreams, attempt), (vec!["甲".to_string()], 1));
    assert_eq!(wait, 200, "第一次退避两秒（测试里的两百毫秒）");
    assert!(left > 9_000 && left <= 10_000, "{left}");
    let r = r.await.unwrap().unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    assert!(body.contains("\"ok\""), "{body}");
    assert!(
        !body.contains("overloaded_error"),
        "过载那一次客户端看不到：{body}"
    );
    assert_eq!(up.hits(), 2);

    let chain = log.attempts("单家流里过载").await;
    assert_eq!(
        shape(&chain),
        [
            (AttemptOutcome::Error, None, false),
            (AttemptOutcome::Served, Some(1), true)
        ]
    );
    assert_eq!(
        chain[0].error.as_ref().unwrap().code,
        "gw.upstream.stream_opening_error"
    );
    assert!(chain[1].backoff_ms.unwrap() >= 190, "{:?}", chain[1]);
    // 退避和退避结束成对
    let waits = log.waits("单家流里过载").await;
    assert!(
        matches!(
            waits.as_slice(),
            [Event::RequestRetrying { .. }, Event::RequestDequeued { .. }]
        ),
        "{waits:#?}"
    );
    // 成功了就是成功：这一家照样记成功
    assert!(state.health.is_available("甲"));
}

/// Codex（Responses）的上游：200 之后在流里报 `server_is_overloaded`，一段内容都没有
#[tokio::test]
async fn a_responses_stream_that_reports_server_is_overloaded_is_retried() {
    let created = event(
        "response.created",
        r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","status":"in_progress"}}"#,
    );
    let overloaded = sse(vec![
        created.clone(),
        event(
            "response.in_progress",
            r#"{"type":"response.in_progress","sequence_number":1,"response":{"id":"resp_1","status":"in_progress"}}"#,
        ),
        event(
            "error",
            r#"{"type":"error","sequence_number":3,"error":{"type":"service_unavailable_error","code":"server_is_overloaded","message":"Our servers are currently overloaded. Please try again later.","param":null}}"#,
        ),
    ]);
    let answered = sse(vec![
        created,
        event(
            "response.output_text.delta",
            r#"{"type":"response.output_text.delta","sequence_number":1,"item_id":"m1","output_index":0,"content_index":0,"delta":"ok"}"#,
        ),
        event(
            "response.completed",
            r#"{"type":"response.completed","sequence_number":2,"response":{"id":"resp_1","status":"completed","usage":{"input_tokens":3,"output_tokens":1,"total_tokens":4}}}"#,
        ),
    ]);
    let up = upstream(vec![overloaded], answered).await;
    let (gw, _state, log) = serve(config(&[(up.addr, "openai-responses")], 10)).await;
    let r = client()
        .post(format!("http://{gw}/v1/responses"))
        .header("authorization", "Bearer tw-k")
        .header("content-type", "application/json")
        .body(r#"{"model":"codex-过载","stream":true,"input":"hi"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    assert!(body.contains("\"delta\":\"ok\""), "{body}");
    assert!(!body.contains("server_is_overloaded"), "{body}");
    assert_eq!(up.hits(), 2);
    let chain = log.attempts("codex-过载").await;
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[1].retry, Some(1));
}

#[tokio::test]
async fn an_immediate_503_is_retried_until_it_answers() {
    let up = upstream(vec![overloaded_503(), status(502, "bad gateway")], ok()).await;
    let (gw, _state, log) = serve(anthropic(&[&up], 10)).await;
    let t = Instant::now();
    let r = ask(gw, "两次 5xx").send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("\"ok\""));
    assert_eq!(up.hits(), 3);
    assert!(
        t.elapsed() >= Duration::from_millis(600),
        "退避 200 + 400 毫秒：{:?}",
        t.elapsed()
    );
    let chain = log.attempts("两次 5xx").await;
    assert_eq!(
        shape(&chain),
        [
            (AttemptOutcome::Status, None, false),
            (AttemptOutcome::Status, Some(1), true),
            (AttemptOutcome::Served, Some(2), true)
        ]
    );
    assert_eq!(chain[0].status, Some(503));
    assert_eq!(chain[1].status, Some(502));
    assert!(chain[2].backoff_ms.unwrap() >= 390, "{:?}", chain[2]);
    let waits = log.waits("两次 5xx").await;
    let attempts: Vec<u32> = waits
        .iter()
        .filter_map(|e| match e {
            Event::RequestRetrying { attempt, .. } => Some(*attempt),
            _ => None,
        })
        .collect();
    assert_eq!(attempts, [1, 2]);
    assert_eq!(waits.len(), 4, "每次退避都有结束：{waits:#?}");
}

#[tokio::test]
async fn an_upstream_that_cannot_be_reached_yet_is_retried() {
    let port = common::spare_port();
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let (gw, _state, log) = serve(config(&[(addr, "anthropic")], 10)).await;
    let r = tokio::spawn(ask(gw, "连不上").send());
    // 一开始没人听这个端口：连不上、开始退避之后才起来（Windows 上被拒的连接要重试一阵才报错，
    // 不能按时间猜它什么时候失败）
    let id = log.id("连不上").await;
    log.until("退避", |evs| {
        evs.iter()
            .any(|e| matches!(e, Event::RequestRetrying { id: i, .. } if *i == id))
            .then_some(())
    })
    .await;
    let hits = Arc::new(Mutex::new(Vec::new()));
    let l = tokio::net::TcpListener::bind(addr).await.unwrap();
    let up = app(Vec::new(), ok(), hits.clone());
    tokio::spawn(async move { axum::serve(l, up).await.unwrap() });
    let r = r.await.unwrap().unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(hits.lock().unwrap().len(), 1);
    let chain = log.attempts("连不上").await;
    assert_eq!(
        shape(&chain),
        [
            (AttemptOutcome::Error, None, false),
            (AttemptOutcome::Served, Some(1), true)
        ]
    );
}

#[tokio::test]
async fn a_stream_broken_before_content_is_retried() {
    let cut = Reply {
        broken: true,
        ..sse(vec![message_start()])
    };
    let up = upstream(vec![cut], streamed()).await;
    let (gw, _state, log) = serve(anthropic(&[&up], 10)).await;
    let r = ask_stream(gw, "断流").send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("\"ok\""));
    assert_eq!(up.hits(), 2);
    let chain = log.attempts("断流").await;
    assert_eq!(
        shape(&chain),
        [
            (AttemptOutcome::Error, None, false),
            (AttemptOutcome::Served, Some(1), true)
        ]
    );
}

#[tokio::test]
async fn an_error_after_content_is_not_retried() {
    let half = sse(vec![message_start(), text("partial"), overloaded_event()]);
    let up = upstream(vec![half], streamed()).await;
    let (gw, _state, log) = serve(anthropic(&[&up], 10)).await;
    let r = ask_stream(gw, "内容之后出错").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    assert!(body.contains("partial"), "{body}");
    assert!(body.contains("overloaded_error"), "{body}");
    assert_eq!(up.hits(), 1, "已经有内容交给了客户端，再发一遍会重复开头");
    let chain = log.attempts("内容之后出错").await;
    assert_eq!(shape(&chain), [(AttemptOutcome::Served, None, false)]);
}

#[tokio::test]
async fn failures_that_are_not_temporary_are_handed_over_at_once() {
    let cases = [
        (
            "请求本身错了",
            status(
                400,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: must be positive"}}"#,
            ),
            400,
        ),
        (
            "凭据被拒",
            status(
                401,
                r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
            ),
            401,
        ),
        (
            "没有这个模型",
            status(
                404,
                r#"{"type":"error","error":{"type":"not_found_error","message":"model: x"}}"#,
            ),
            404,
        ),
        (
            "额度用完",
            status(
                429,
                r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached"}}"#,
            ),
            429,
        ),
        (
            "没钱了",
            status(402, r#"{"error":{"message":"Insufficient Balance"}}"#),
            402,
        ),
    ];
    for (model, reply, code) in cases {
        let up = upstream(Vec::new(), reply).await;
        let (gw, _state, log) = serve(anthropic(&[&up], 10)).await;
        let r = ask(gw, model).send().await.unwrap();
        assert_eq!(r.status(), code, "{model}");
        assert_eq!(up.hits(), 1, "{model}");
        let waits = log.waits(model).await;
        assert!(waits.is_empty(), "{model}：{waits:#?}");
    }
}

/// 流里报的请求本身的问题（加密的历史解不开）：原样交出去，不重试
#[tokio::test]
async fn an_invalid_request_reported_in_the_stream_is_not_retried() {
    let refused = sse(vec![
        event(
            "response.created",
            r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1"}}"#,
        ),
        event(
            "error",
            r#"{"type":"error","sequence_number":1,"error":{"type":"invalid_request_error","code":"invalid_encrypted_content","message":"The encrypted content could not be verified."}}"#,
        ),
    ]);
    let up = upstream(Vec::new(), refused).await;
    let (gw, _state, log) = serve(config(&[(up.addr, "openai-responses")], 10)).await;
    let r = client()
        .post(format!("http://{gw}/v1/responses"))
        .header("authorization", "Bearer tw-k")
        .header("content-type", "application/json")
        .body(r#"{"model":"解不开","stream":true,"input":"hi"}"#)
        .send()
        .await
        .unwrap();
    let body = r.text().await.unwrap();
    assert!(body.contains("invalid_encrypted_content"), "{body}");
    assert_eq!(up.hits(), 1);
    assert!(log.waits("解不开").await.is_empty());
}

#[tokio::test]
async fn at_the_deadline_the_last_error_is_handed_over() {
    let up = upstream(Vec::new(), overloaded_503()).await;
    let (gw, _state, log) = serve(anthropic(&[&up], 1)).await;
    let t = Instant::now();
    let r = ask(gw, "一直过载").send().await.unwrap();
    // 退避 200、400 毫秒之后剩下的不够 800 毫秒：交出最后那个错误
    assert_eq!(r.status(), 502);
    let body = r.text().await.unwrap();
    assert!(body.contains("answered 503"), "{body}");
    assert_eq!(up.hits(), 3);
    assert!(
        t.elapsed() < Duration::from_millis(1500),
        "{:?}",
        t.elapsed()
    );
    let chain = log.attempts("一直过载").await;
    let retries: Vec<Option<u32>> = chain.iter().map(|a| a.retry).collect();
    assert_eq!(retries, [None, Some(1), Some(2)]);
}

#[tokio::test]
async fn wait_secs_zero_never_retries() {
    let up = upstream(vec![overloaded_in_the_stream()], streamed()).await;
    let (gw, _state, log) = serve(anthropic(&[&up], 0)).await;
    let r = ask_stream(gw, "不等").send().await.unwrap();
    // 等不起的最后一家不压开头：它在流里报的错原样到了客户端
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("overloaded_error"));
    assert_eq!(up.hits(), 1);
    assert!(log.waits("不等").await.is_empty());
}

#[tokio::test]
async fn retry_after_from_the_upstream_replaces_the_backoff() {
    let said = Reply {
        headers: vec![("retry-after-ms", "300".into())],
        ..overloaded_503()
    };
    let up = upstream(vec![said], ok()).await;
    // 退避的一秒是十秒：按退避算要等二十秒，比等待期限还长
    let (gw, _state, log) = serve_with(
        anthropic(&[&up], 10),
        Duration::from_secs(10),
        Duration::from_secs(10),
    )
    .await;
    let t = Instant::now();
    let r = ask(gw, "上游说了等多久").send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(t.elapsed() >= Duration::from_millis(300));
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    let chain = log.attempts("上游说了等多久").await;
    assert!(chain[1].backoff_ms.unwrap() >= 290, "{chain:#?}");
}

#[tokio::test]
async fn a_retry_after_beyond_the_deadline_gives_up_at_once() {
    let said = Reply {
        headers: vec![("retry-after", "5".into())],
        ..overloaded_503()
    };
    let up = upstream(Vec::new(), said).await;
    let (gw, _state, log) = serve(anthropic(&[&up], 2)).await;
    let t = Instant::now();
    let r = ask(gw, "等不到那时").send().await.unwrap();
    assert_eq!(r.status(), 502);
    assert_eq!(up.hits(), 1);
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "{:?}",
        t.elapsed()
    );
    assert!(log.waits("等不到那时").await.is_empty());
}

#[tokio::test]
async fn aborting_while_backing_off_ends_the_request_without_another_attempt() {
    let up = upstream(Vec::new(), overloaded_503()).await;
    // 退避两秒
    let (gw, state, log) = serve_with(
        anthropic(&[&up], 10),
        Duration::from_secs(1),
        Duration::from_secs(10),
    )
    .await;
    let r = tokio::spawn(ask(gw, "退避时中止").send());
    let id = log.id("退避时中止").await;
    log.until("退避", |evs| {
        evs.iter()
            .any(|e| matches!(e, Event::RequestRetrying { id: i, .. } if *i == id))
            .then_some(())
    })
    .await;
    let t = Instant::now();
    state.aborts.request(id).expect("还在跑");
    let r = r.await.unwrap().unwrap();
    assert_eq!(r.status(), 499);
    assert!(t.elapsed() < Duration::from_secs(1), "不等退避完");
    assert_eq!(up.hits(), 1);
    let chain = log.attempts("退避时中止").await;
    assert_eq!(chain.len(), 1, "尝试链上不多一跳：{chain:#?}");
    let waits = log.waits("退避时中止").await;
    assert!(
        matches!(
            waits.as_slice(),
            [Event::RequestRetrying { .. }, Event::RequestDequeued { .. }]
        ),
        "{waits:#?}"
    );
}

#[tokio::test]
async fn when_both_upstreams_are_overloaded_both_are_tried_again() {
    let a = upstream(Vec::new(), overloaded_503()).await;
    let b = upstream(vec![overloaded_503()], ok()).await;
    let (gw, _state, log) = serve(anthropic(&[&a, &b], 10)).await;
    let r = ask(gw, "两家都过载").send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!((a.hits(), b.hits()), (2, 2));
    let chain = log.attempts("两家都过载").await;
    let seen: Vec<(&str, Option<u32>, bool)> = chain
        .iter()
        .map(|a| (a.provider.as_str(), a.retry, a.backoff_ms.is_some()))
        .collect();
    // 退避只在一轮的头一跳上
    assert_eq!(
        seen,
        [
            ("甲", None, false),
            ("乙", None, false),
            ("甲", Some(1), true),
            ("乙", Some(1), false)
        ]
    );
    let waits = log.waits("两家都过载").await;
    assert!(
        matches!(
            &waits[0],
            Event::RequestRetrying { upstreams, .. } if upstreams == &["甲".to_string(), "乙".to_string()]
        ),
        "{waits:#?}"
    );
}

/// 这段对话留在乙那儿（缓存在那边）：重试只再试乙，甲出的错也是暂时的，照样不再试它
#[tokio::test]
async fn a_conversation_that_stays_on_an_upstream_retries_only_that_one() {
    let a = upstream(vec![overloaded_503()], overloaded_503()).await;
    let b = upstream(vec![ok(), overloaded_503(), overloaded_503()], ok()).await;
    let (gw, _state, log) = serve(anthropic(&[&a, &b], 10)).await;
    // 第一轮：甲过载，乙答了、读了缓存
    let r = ask_in(gw, "粘住", false, Some("会话一"), 1)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // 等第一轮的结局落定，第二轮才认得出它留在哪一家
    tokio::time::sleep(Duration::from_millis(100)).await;
    // 第二轮：乙在头上。乙、甲都过载，之后只再试乙
    let r = ask_in(gw, "粘住", false, Some("会话一"), 2)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(a.hits(), 2, "甲只在两轮里各试了一次");
    assert_eq!(b.hits(), 4);
    let events = log.0.lock().unwrap().clone();
    let rounds: Vec<Vec<String>> = events
        .iter()
        .filter_map(|e| match e {
            Event::RequestRetrying { upstreams, .. } => Some(upstreams.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(rounds, [vec!["乙".to_string()], vec!["乙".to_string()]]);
}

/// 压过了时限：客户端先拿到 `200` 和保活，重试到期限都没成，流里按它的格式报最后那个错误
#[tokio::test]
async fn past_the_hold_the_retries_go_on_under_keepalives_and_the_last_error_ends_the_stream() {
    let up = upstream(Vec::new(), overloaded_in_the_stream()).await;
    let (gw, _state, log) =
        serve_with(anthropic(&[&up], 2), TICK, Duration::from_millis(300)).await;
    let t = Instant::now();
    let r = ask_stream(gw, "压过时限还过载").send().await.unwrap();
    let headers_at = t.elapsed();
    assert_eq!(r.status(), 200);
    assert!(
        headers_at >= Duration::from_millis(300) && headers_at < Duration::from_millis(1000),
        "{headers_at:?}"
    );
    let body = r.text().await.unwrap();
    assert!(body.starts_with(": keep-alive\n\n"), "{body}");
    assert!(body.matches(": keep-alive").count() >= 3, "{body}");
    let tail = &body[body.find("event: error").expect("流里报了错")..];
    assert!(tail.contains("overloaded_error"), "{tail}");
    assert!(
        !body.contains("message_start"),
        "被放弃的开头一个字节都没交出去：{body}"
    );
    // 退避 200、400、800 毫秒之后剩下的不够 1.6 秒
    assert_eq!(up.hits(), 4);
    let chain = log.attempts("压过时限还过载").await;
    assert_eq!(chain.len(), 4);
}

#[tokio::test]
async fn past_the_hold_a_retry_that_answers_continues_the_stream() {
    let up = upstream(
        vec![overloaded_in_the_stream(), overloaded_in_the_stream()],
        streamed(),
    )
    .await;
    let (gw, _state, _log) = serve_with(
        anthropic(&[&up], 10),
        Duration::from_millis(200),
        Duration::from_millis(300),
    )
    .await;
    let r = ask_stream(gw, "压过时限后答了").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    assert!(body.starts_with(": keep-alive\n\n"), "{body}");
    assert!(body.contains("\"ok\""), "{body}");
    assert!(!body.contains("overloaded_error"), "{body}");
    assert_eq!(body.matches("event: message_start").count(), 1, "{body}");
    assert_eq!(up.hits(), 3);
}
