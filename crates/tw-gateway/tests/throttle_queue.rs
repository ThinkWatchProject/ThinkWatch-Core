//! 上游回 429 之后：这一家暂停，请求排队等它（`tw_gateway::throttle`），端到端。
//!
//! 假上游按脚本一个个地答：先回几个 429（带着暂停多久），之后照常答。它记下每个请求到的时刻
//! —— 暂停结束时只放一个过去探路、探路的有了结论别的才跟上，看的就是这些时刻。暂停写成几百
//! 毫秒（`retry-after-ms`），测试不用干等几秒的退避。

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use futures::StreamExt;
use tw_api::{AttemptOutcome, AttemptView, Event, ServeSkip, Stay, ThrottleKind};

/// 假上游的一次答复
#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
    /// 响应头之前等多久
    delay_ms: u64,
    /// 流式的回答：响应头之后一段一段地给，每一段之前等多久。有的话 `body` 不用
    chunks: Vec<(u64, String)>,
}

/// 照常答的那一种：读了 5000 token 的缓存（这段对话跨轮值得留下）
fn ok() -> Reply {
    Reply {
        status: 200,
        headers: Vec::new(),
        body: r#"{"type":"message","role":"assistant","model":"claude-sonnet-5","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"cache_read_input_tokens":5000,"output_tokens":1}}"#.into(),
        delay_ms: 0,
        chunks: Vec::new(),
    }
}

/// 限流：Anthropic 的样子，`headers` 说暂停多久
fn limited(headers: &[(&'static str, &str)]) -> Reply {
    Reply {
        status: 429,
        headers: headers.iter().map(|(k, v)| (*k, v.to_string())).collect(),
        body: r#"{"type":"error","error":{"type":"rate_limit_error","message":"Number of request tokens has exceeded your per-minute rate limit"}}"#.into(),
        delay_ms: 0,
        chunks: Vec::new(),
    }
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

/// 流式地照常答：开头、一段内容「ok」、收尾
fn streamed() -> Reply {
    let chunks = [
        message_start(),
        event(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
        ),
        event(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        event(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
        ),
        event("message_stop", r#"{"type":"message_stop"}"#),
    ];
    Reply {
        status: 200,
        headers: Vec::new(),
        body: String::new(),
        delay_ms: 0,
        chunks: chunks.into_iter().map(|c| (0, c)).collect(),
    }
}

/// 响应头是 200、流也开了，开头之后 `after_ms` 在流里报限流（一段内容都还没有），说 1 秒之后
/// 再来
fn limited_in_the_stream(after_ms: u64) -> Reply {
    Reply {
        chunks: vec![
            (0, message_start()),
            (
                after_ms,
                event(
                    "error",
                    r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down","details":[{"retryDelay":"1s"}]}}"#,
                ),
            ),
        ],
        ..streamed()
    }
}

/// 一家假上游：先按 `script` 的顺序答，答完了一律答 `then`
struct Up {
    addr: SocketAddr,
    /// 每个请求到的时刻
    hits: Arc<Mutex<Vec<Instant>>>,
}

impl Up {
    fn hits(&self) -> usize {
        self.hits.lock().unwrap().len()
    }
    fn times(&self) -> Vec<Instant> {
        self.hits.lock().unwrap().clone()
    }
}

async fn upstream(script: Vec<Reply>, then: Reply) -> Up {
    let script = Arc::new(Mutex::new(VecDeque::from(script)));
    let hits = Arc::new(Mutex::new(Vec::new()));
    let h = hits.clone();
    let app = Router::new().fallback(axum::routing::any(move || {
        let (script, h, then) = (script.clone(), h.clone(), then.clone());
        async move {
            h.lock().unwrap().push(Instant::now());
            let r = script.lock().unwrap().pop_front().unwrap_or(then);
            tokio::time::sleep(Duration::from_millis(r.delay_ms)).await;
            let streams = !r.chunks.is_empty();
            let mut b = axum::response::Response::builder().status(r.status).header(
                "content-type",
                if streams {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            );
            for (k, v) in &r.headers {
                b = b.header(*k, v.as_str());
            }
            if !streams {
                return b.body(axum::body::Body::from(r.body)).unwrap();
            }
            let chunks = futures::stream::iter(r.chunks).then(|(ms, c)| async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Ok::<_, std::io::Error>(bytes::Bytes::from(c))
            });
            b.body(axum::body::Body::from_stream(chunks)).unwrap()
        }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Up { addr, hits }
}

/// 一家或者两家（按顺序的组「池」）上游，默认路由把请求都交给它们
fn config(ups: &[&Up], wait_secs: u64) -> tw_config::Config {
    let names = ["甲", "乙"];
    let mut yaml =
        "version: 1\ndefault_route: 默认\nclients:\n  - name: me\n    key: tw-k\nproviders:\n"
            .to_string();
    for (n, u) in names.iter().zip(ups) {
        yaml += &format!(
            "  - name: {n}\n    base_url: http://{}\n    key: sk-x\n    protocol: anthropic\n",
            u.addr
        );
    }
    let members = names[..ups.len()].join(", ");
    yaml += &format!(
        "groups:\n  - name: 池\n    type: fallback\n    providers: [{members}]\nroutes:\n  - name: 默认\n    rules:\n      - name: 全部\n        to: 池\nfailover:\n  wait_secs: {wait_secs}\n"
    );
    serde_yaml_ng::from_str(&yaml).unwrap()
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

    /// 那个请求的尝试链，和这段对话留在上次那一家的理由
    async fn routed(&self, model: &str) -> (Vec<AttemptView>, Option<Stay>) {
        let id = self.id(model).await;
        self.until(&format!(" {model} 的路由"), |evs| {
            evs.iter().find_map(|e| match e {
                Event::RequestRouted {
                    id: i,
                    attempts,
                    affinity,
                    ..
                } if *i == id => Some((attempts.clone(), affinity.as_ref().and_then(|a| a.stayed))),
                _ => None,
            })
        })
        .await
    }

    fn all(&self) -> Vec<Event> {
        self.0.lock().unwrap().clone()
    }
}

async fn serve(cfg: tw_config::Config) -> (SocketAddr, tw_gateway::AppState, Log) {
    let state = tw_gateway::AppState::new(cfg).unwrap();
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

/// 一个整包的请求。`conversation` 给了的话带着会话头，`turn` 是这段对话的第几轮（用户说了
/// 几次话）。**开头那句按对话写**：开头一字不差的请求会被认成同一段对话（见
/// `tw_gateway::affinity::identity`），留在上次回答它的那一家
fn ask(
    gw: SocketAddr,
    model: &str,
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
        r#"{{"model":"{model}","max_tokens":16,"system":"你是一个助手","messages":[{}]}}"#,
        messages.join(",")
    );
    let mut r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .header("anthropic-version", "2023-06-01")
        .body(body);
    if let Some(c) = conversation {
        r = r.header("x-claude-code-session-id", c);
    }
    r
}

/// 一个流式的请求
fn ask_stream(gw: SocketAddr, model: &str) -> reqwest::RequestBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .header("anthropic-version", "2023-06-01")
        .body(format!(
            r#"{{"model":"{model}","max_tokens":16,"stream":true,"messages":[{{"role":"user","content":"{model}"}}]}}"#
        ))
}

/// 直接让一家暂停这么久，像它刚回了一个 429
fn pause(state: &tw_gateway::AppState, name: &str, ms: u64) {
    state.throttles.limited(
        name,
        std::time::Instant::now(),
        Some(Duration::from_millis(ms)),
    );
}

#[tokio::test]
async fn a_lone_upstream_that_rate_limits_is_waited_for_and_then_answers() {
    let up = upstream(vec![limited(&[("retry-after-ms", "400")])], ok()).await;
    let (gw, state, log) = serve(config(&[&up], 10)).await;
    let t = Instant::now();
    let r = tokio::spawn(ask(gw, "一个", None, 1).send());
    // 在排队：半路才来看的一方也看得出它在等哪一家
    let id = log.id("一个").await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let flying = state.bus.in_flight();
    let me = flying.requests.iter().find(|r| r.id == id).unwrap();
    assert!(
        me.events.iter().any(|e| matches!(
            e,
            Event::RequestQueued { upstreams, .. } if upstreams == &["甲".to_string()]
        )),
        "{:#?}",
        me.events
    );
    let r = r.await.unwrap().unwrap();
    assert_eq!(r.status(), 200, "客户端察觉不到那一次 429");
    assert!(r.text().await.unwrap().contains("\"ok\""));
    assert!(
        t.elapsed() >= Duration::from_millis(380),
        "{:?}",
        t.elapsed()
    );
    let times = up.times();
    assert_eq!(times.len(), 2);
    assert!(
        times[1] - times[0] >= Duration::from_millis(380),
        "没等暂停结束就又发了：{:?}",
        times[1] - times[0]
    );

    let (chain, _) = log.routed("一个").await;
    assert_eq!(chain.len(), 2, "{chain:#?}");
    assert_eq!(
        (chain[0].outcome, chain[0].status),
        (AttemptOutcome::Status, Some(429))
    );
    assert!(
        chain[0]
            .pause_ms
            .is_some_and(|ms| (300..=400).contains(&ms)),
        "{chain:#?}"
    );
    assert_eq!(chain[1].outcome, AttemptOutcome::Served);
    assert_eq!(chain[1].pause_ms, None);
    assert!(
        chain[1].queued_ms.is_some_and(|ms| ms >= 250),
        "等暂停的那一段该记在发出去的那一跳上：{chain:#?}"
    );
    // 暂停不是失败：不熔断；开始暂停、暂停结束各报一次
    let evs = log.all();
    assert!(!evs.iter().any(|e| matches!(e, Event::HealthChanged { .. })));
    let changes: Vec<Option<ThrottleKind>> = evs
        .iter()
        .filter_map(|e| match e {
            Event::ThrottleChanged {
                provider, throttle, ..
            } if provider == "甲" => Some(throttle.as_ref().map(|t| t.kind)),
            _ => None,
        })
        .collect();
    assert_eq!(changes, [Some(ThrottleKind::RateLimited), None], "{evs:#?}");
    let queued = evs
        .iter()
        .filter(|e| matches!(e, Event::RequestQueued { id: i, .. } if *i == id))
        .count();
    assert_eq!(queued, 1, "一次排队报一条");
    // 不排了也报一条：在路由事件之前（路由事件要等整条尝试链走完）
    let at = |f: &dyn Fn(&Event) -> bool| evs.iter().position(f);
    let queued = at(&|e| matches!(e, Event::RequestQueued { id: i, .. } if *i == id)).unwrap();
    let dequeued =
        at(&|e| matches!(e, Event::RequestDequeued { id: i, .. } if *i == id)).expect("没报不排了");
    let routed = at(&|e| matches!(e, Event::RequestRouted { id: i, .. } if *i == id)).unwrap();
    assert!(queued < dequeued && dequeued < routed, "{evs:#?}");
    assert_eq!(
        evs.iter()
            .filter(|e| matches!(e, Event::RequestDequeued { id: i, .. } if *i == id))
            .count(),
        1
    );
}

#[tokio::test]
async fn after_a_pause_one_request_goes_first_and_the_rest_follow_its_answer() {
    // 头一个被限流 300 毫秒；之后的每个都要 400 毫秒才回响应头
    let slow = Reply {
        delay_ms: 400,
        ..ok()
    };
    let up = upstream(vec![limited(&[("retry-after-ms", "300")])], slow).await;
    let (gw, _state, _log) = serve(config(&[&up], 10)).await;
    let first = tokio::spawn(ask(gw, "一", None, 1).send());
    tokio::time::sleep(Duration::from_millis(100)).await;
    // 暂停着的时候又来了两个：排队
    let more: Vec<_> = ["二", "三"]
        .into_iter()
        .map(|m| tokio::spawn(ask(gw, m, None, 1).send()))
        .collect();
    assert_eq!(first.await.unwrap().unwrap().status(), 200);
    for r in more {
        assert_eq!(r.await.unwrap().unwrap().status(), 200);
    }
    let times = up.times();
    assert_eq!(times.len(), 4, "429 一次，之后三个各一次");
    // 暂停结束：只放一个过去探路。它的响应头 400 毫秒后才到，别的等它
    assert!(times[1] - times[0] >= Duration::from_millis(280));
    for later in &times[2..] {
        assert!(
            *later - times[1] >= Duration::from_millis(350),
            "探路的还没有结论别的就发了：{:?}",
            *later - times[1]
        );
    }
}

#[tokio::test]
async fn when_nothing_recovers_in_time_the_client_gets_a_429_saying_when() {
    let up = upstream(Vec::new(), limited(&[("retry-after", "30")])).await;
    let (gw, _state, log) = serve(config(&[&up], 2)).await;
    let t = Instant::now();
    let r = ask(gw, "一", None, 1).send().await.unwrap();
    // 暂停 30 秒，这个请求最多等 2 秒：不用干等，当场说
    assert!(
        t.elapsed() < Duration::from_millis(1500),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(r.status(), 429);
    let h = |k: &str| r.headers().get(k).map(|v| v.to_str().unwrap().to_string());
    assert_eq!(h("x-thinkwatch-error").as_deref(), Some("rate_limited"));
    assert_eq!(h("x-should-retry").as_deref(), Some("true"));
    let secs: u64 = h("retry-after").unwrap().parse().unwrap();
    assert!((29..=30).contains(&secs), "{secs}");
    let ms: u64 = h("retry-after-ms").unwrap().parse().unwrap();
    assert!((28_000..=30_000).contains(&ms), "{ms}");
    let body: serde_json::Value = r.json().await.unwrap();
    let said = body["error"]["message"].as_str().unwrap();
    assert!(
        said.contains("paused after answering 429 (rate limited): `甲`"),
        "{said}"
    );
    assert_eq!(up.hits(), 1);
    let failed = log
        .until("失败", |evs| {
            evs.iter().find_map(|e| match e {
                Event::RequestFailed {
                    source, message, ..
                } => Some((*source, message.code.clone())),
                _ => None,
            })
        })
        .await;
    assert_eq!(failed.0.slug(), "rate_limited");
    assert_eq!(failed.1, "gw.wait.rate_limited");

    // 还暂停着：下一个请求不发给它，尝试链上说它在暂停
    let r = ask(gw, "二", None, 1).send().await.unwrap();
    assert_eq!(r.status(), 429);
    assert_eq!(up.hits(), 1, "暂停着还发了");
    let (chain, _) = log.routed("二").await;
    assert_eq!(chain.len(), 1, "{chain:#?}");
    assert_eq!(chain[0].skipped, Some(ServeSkip::Throttled));
    assert_eq!(
        chain[0].error.as_ref().map(|m| m.code.as_str()),
        Some("gw.upstream.paused")
    );
}

#[tokio::test]
async fn counting_tokens_is_estimated_while_the_upstream_is_paused() {
    let up = upstream(vec![limited(&[("retry-after-ms", "60000")])], ok()).await;
    let (gw, state, log) = serve(config(&[&up], 10)).await;
    let count = |model: &str| {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("http://{gw}/v1/messages/count_tokens"))
            .header("x-api-key", "tw-k")
            .header("anthropic-version", "2023-06-01")
            .body(format!(
                r#"{{"model":"{model}","messages":[{{"role":"user","content":"数一数这句话"}}]}}"#
            ))
    };
    // 数 token 的请求被限流了：这一家暂停，这一次由网关估，不排队
    let t = Instant::now();
    let r = count("一").send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-thinkwatch-local"], "1");
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    let (chain, _) = log.routed("一").await;
    assert_eq!(chain.len(), 2, "{chain:#?}");
    assert_eq!(chain[0].status, Some(429));
    assert!(chain[0].pause_ms.is_some());
    assert_eq!(chain[1].outcome, AttemptOutcome::Estimated);
    assert_eq!(up.hits(), 1);
    assert!(state.throttles.view("甲").is_some(), "该暂停着");

    // 还暂停着：不发，直接估
    let r = count("二").send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-thinkwatch-local"], "1");
    let body: serde_json::Value = r.json().await.unwrap();
    assert!(
        body["input_tokens"].as_u64().is_some_and(|n| n > 0),
        "{body}"
    );
    assert_eq!(up.hits(), 1);
    let (chain, _) = log.routed("二").await;
    assert_eq!(chain.len(), 1, "{chain:#?}");
    assert_eq!(chain[0].outcome, AttemptOutcome::Estimated);
}

#[tokio::test]
async fn aborting_a_queued_request_takes_it_out_of_the_queue() {
    let up = upstream(Vec::new(), ok()).await;
    let (gw, state, log) = serve(config(&[&up], 10)).await;
    pause(&state, "甲", 800);
    let r = tokio::spawn(ask(gw, "一", None, 1).send());
    let id = log.id("一").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    state.aborts.request(id).expect("还在跑");
    let r = r.await.unwrap().unwrap();
    assert_eq!(r.status(), 499);
    let source = log
        .until("中止", |evs| {
            evs.iter().find_map(|e| match e {
                Event::RequestFailed { id: i, source, .. } if *i == id => Some(*source),
                _ => None,
            })
        })
        .await;
    assert_eq!(source.slug(), "aborted");
    assert_eq!(up.hits(), 0);
    // 被中止也是不排了
    assert!(
        log.all()
            .iter()
            .any(|e| matches!(e, Event::RequestDequeued { id: i, .. } if *i == id))
    );
    // 它不在队里了：暂停结束后下一个请求当场就发（它要是还排在队头，后面的要一直等它）
    tokio::time::sleep(Duration::from_millis(800)).await;
    let t = Instant::now();
    let r = ask(gw, "二", None, 1).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "{:?}",
        t.elapsed()
    );
}

#[tokio::test]
async fn a_conversation_waits_for_its_paused_upstream_and_a_new_one_moves_on() {
    let (a, b) = (
        upstream(Vec::new(), ok()).await,
        upstream(Vec::new(), ok()).await,
    );
    let (gw, state, log) = serve(config(&[&a, &b], 5)).await;
    // 这段对话由甲回答，读了 5000 token 的缓存：下一轮留在甲
    let r = ask(gw, "第一轮", Some("对话"), 1).send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.text().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 甲暂停半秒：这段对话等它；新的对话当场换乙
    pause(&state, "甲", 500);
    let t = Instant::now();
    let stays = tokio::spawn(ask(gw, "第二轮", Some("对话"), 2).send());
    let fresh = ask(gw, "新的", Some("别的对话"), 1).send().await.unwrap();
    assert_eq!(fresh.status(), 200);
    assert!(t.elapsed() < Duration::from_millis(400), "新的对话等了甲");
    let (chain, _) = log.routed("新的").await;
    assert_eq!(chain.len(), 2, "{chain:#?}");
    assert_eq!(chain[0].provider, "甲");
    assert_eq!(chain[0].skipped, Some(ServeSkip::Throttled));
    assert_eq!(chain[0].queued_ms, None, "新的对话不等");
    assert_eq!(
        (chain[1].provider.as_str(), chain[1].outcome),
        ("乙", AttemptOutcome::Served)
    );

    let r = stays.await.unwrap().unwrap();
    assert_eq!(r.status(), 200);
    // 等到甲的暂停过了才发出去。不看它排了多久：机器忙时这个请求晚一点才进队，排队的
    // 时长跟着变短，可它照样是等完了暂停才走的
    assert!(
        t.elapsed() >= Duration::from_millis(450),
        "{:?}",
        t.elapsed()
    );
    r.text().await.unwrap();
    let (chain, stayed) = log.routed("第二轮").await;
    assert_eq!(stayed, Some(Stay::Cache));
    assert_eq!(chain.len(), 1, "{chain:#?}");
    assert_eq!(
        (chain[0].provider.as_str(), chain[0].outcome),
        ("甲", AttemptOutcome::Served)
    );
    assert!(chain[0].queued_ms.is_some(), "{chain:#?}");
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 甲要暂停一分钟，这个请求最多等 5 秒：等也白等，不再留在甲，照常排序
    pause(&state, "甲", 60_000);
    let t = Instant::now();
    let r = ask(gw, "第三轮", Some("对话"), 3).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "{:?}",
        t.elapsed()
    );
    let (chain, stayed) = log.routed("第三轮").await;
    assert_eq!(stayed, None, "这段对话该放开甲");
    assert_eq!(chain.len(), 2, "{chain:#?}");
    assert_eq!(chain[0].skipped, Some(ServeSkip::Throttled));
    assert_eq!(chain[1].provider, "乙");
}

#[tokio::test]
async fn a_rate_limited_upstream_moves_the_request_on_and_stays_out_of_the_breaker() {
    // 甲一直限流，乙照常：每个请求都去了乙，甲暂停之后不再挨发
    let a = upstream(Vec::new(), limited(&[("retry-after", "30")])).await;
    let b = upstream(Vec::new(), ok()).await;
    let (gw, state, log) = serve(config(&[&a, &b], 10)).await;
    for m in ["一", "二", "三", "四"] {
        let r = ask(gw, m, None, 1).send().await.unwrap();
        assert_eq!(r.status(), 200);
    }
    assert_eq!(a.hits(), 1, "暂停着还发给了甲");
    assert_eq!(b.hits(), 4);
    let (chain, _) = log.routed("一").await;
    assert_eq!(chain[0].status, Some(429));
    assert!(
        chain[0].pause_ms.is_some_and(|ms| ms > 29_000),
        "{chain:#?}"
    );
    let (chain, _) = log.routed("二").await;
    assert_eq!(chain[0].skipped, Some(ServeSkip::Throttled), "{chain:#?}");
    // 限流不进熔断，也不算进成功率
    assert!(state.health.is_available("甲"));
    assert!(state.health.success_rates(&["甲".to_string()]).is_empty());
    assert!(
        !log.all()
            .iter()
            .any(|e| matches!(e, Event::HealthChanged { .. }))
    );
}

/// 探路的又被限流了，回到甲的队头，接着去试乙（乙要 5 秒）。**它在乙那儿的时候不该挡着甲的队**：
/// 甲的暂停 300 毫秒后就结束了，排在后面的请求该轮到探路，而不是等到期限回 429
#[tokio::test]
async fn a_probe_limited_again_does_not_hold_the_queue_while_it_tries_another_upstream() {
    let a = upstream(
        vec![
            limited(&[("retry-after-ms", "300")]),
            limited(&[("retry-after-ms", "300")]),
        ],
        ok(),
    )
    .await;
    let slow = Reply {
        delay_ms: 5000,
        ..ok()
    };
    let b = upstream(vec![ok()], slow).await;
    let mut cfg = config(&[&a, &b], 2);
    cfg.providers[1].max_concurrent = Some(1);
    let (gw, _state, _log) = serve(cfg).await;
    // 甲 429（暂停 300 毫秒），乙当场答
    let r = ask(gw, "零", None, 1).send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.text().await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    // 探路的又被限流，去了乙（占着乙唯一的位置 5 秒）
    let probe = tokio::spawn(ask(gw, "一", None, 1).send());
    tokio::time::sleep(Duration::from_millis(150)).await;
    // 甲暂停着、乙满着：在两家里等
    let t = Instant::now();
    let r = ask(gw, "二", None, 1).send().await.unwrap();
    assert_eq!(
        r.status(),
        200,
        "甲早就能发了，却等了 {:?}，甲只收到 {} 个请求",
        t.elapsed(),
        a.hits()
    );
    let _ = probe.await;
}

/// 上游写了一个大得离谱的 `retry-after-ms`：照上限暂停，不能把请求弄崩
#[tokio::test]
async fn an_absurd_retry_after_ms_is_capped_rather_than_crashing_the_request() {
    let up = upstream(vec![limited(&[("retry-after-ms", "1e300")])], ok()).await;
    let (gw, _state, _log) = serve(config(&[&up], 1)).await;
    let r = ask(gw, "一", None, 1)
        .send()
        .await
        .expect("网关该回话，不是断开连接");
    assert_eq!(r.status(), 429);
}

/// 上游一直回 429 和 `Retry-After: 0`：不能不停地重发，也不能过了等待期限还在发
#[tokio::test]
async fn a_zero_retry_after_neither_loops_nor_outlives_the_deadline() {
    let up = upstream(Vec::new(), limited(&[("retry-after", "0")])).await;
    let (gw, _state, _log) = serve(config(&[&up], 2)).await;
    // 最多等 2 秒的请求，5 秒还没结束就是停不下来了
    let r = tokio::time::timeout(Duration::from_secs(5), ask(gw, "一", None, 1).send()).await;
    let hits = up.hits();
    assert!(r.is_ok(), "过了期限 3 秒还在重发，已经发了 {hits} 次");
    assert!(hits < 20, "两秒里发了 {hits} 次");
}

/// 探路的发出去了还没有结论：宽限期的结束不是「知道的恢复时刻」。期限比宽限期近的请求
/// 该等探路的结论，不该当场回 429
#[tokio::test]
async fn a_request_waits_for_a_probe_in_flight_even_when_its_deadline_is_within_the_grace() {
    let slow = Reply {
        delay_ms: 1000,
        ..ok()
    };
    let up = upstream(vec![limited(&[("retry-after-ms", "300")])], slow).await;
    let (gw, _state, _log) = serve(config(&[&up], 4)).await;
    // 头一个：429，300 毫秒后探路，1300 毫秒时探路的有了结论
    let first = tokio::spawn(ask(gw, "一", None, 1).send());
    tokio::time::sleep(Duration::from_millis(500)).await;
    let t = Instant::now();
    let r = ask(gw, "二", None, 1).send().await.unwrap();
    assert_eq!(
        r.status(),
        200,
        "{:?} 就回了 429，探路的不到一秒后就过了",
        t.elapsed()
    );
    assert_eq!(first.await.unwrap().unwrap().status(), 200);
}

/// 回答压着开头的时候，响应头是 200 不说明这一家好了：探路的拿到 200、流里接着报限流，**它的
/// 结论是又被限流**。排在后面的不能在响应头到的那一刻就一拥而上，退避也不归零
#[tokio::test]
async fn a_probe_whose_stream_reports_a_rate_limit_after_a_200_head_does_not_open_the_queue() {
    let up = upstream(
        vec![
            limited(&[("retry-after-ms", "300")]),
            // 探路的：响应头当场到，限流 300 毫秒之后才在流里报
            limited_in_the_stream(300),
        ],
        streamed(),
    )
    .await;
    let (gw, _state, log) = serve(config(&[&up], 10)).await;
    let first = tokio::spawn(ask_stream(gw, "一").send());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let more: Vec<_> = ["二", "三"]
        .into_iter()
        .map(|m| tokio::spawn(ask_stream(gw, m).send()))
        .collect();
    for r in std::iter::once(first).chain(more) {
        let r = r.await.unwrap().unwrap();
        assert_eq!(r.status(), 200);
        let text = r.text().await.unwrap();
        assert!(text.contains("text_delta"), "{text}");
        assert!(
            !text.contains("rate_limit_error"),
            "客户端看到了限流：{text}"
        );
    }
    let times = up.times();
    assert_eq!(times.len(), 5, "429、探路的、再探一次、后面两个");
    for later in &times[2..] {
        assert!(
            *later - times[1] >= Duration::from_millis(1_200),
            "探路的在流里又被限流了，后面的却跟着发了：{:?}",
            *later - times[1]
        );
    }
    let (chain, _) = log.routed("一").await;
    assert_eq!(chain.len(), 3, "{chain:#?}");
    assert!(
        chain[1]
            .pause_ms
            .is_some_and(|ms| (900..=1_000).contains(&ms)),
        "{chain:#?}"
    );
    assert_eq!(chain[2].outcome, AttemptOutcome::Served);
}

/// 只有一家、它刚限流过：流的开头（内容之前）又报了限流，和回 429 一样暂停、排队、再发，
/// **客户端察觉不到**。一直好好的最后一家不压开头（客户端当场拿到响应头），见 `hop`
#[tokio::test]
async fn a_rate_limit_inside_the_stream_before_any_content_is_invisible_on_the_last_upstream() {
    let up = upstream(
        vec![
            limited(&[("retry-after-ms", "300")]),
            ok(),
            limited_in_the_stream(0),
        ],
        streamed(),
    )
    .await;
    let (gw, _state, log) = serve(config(&[&up], 10)).await;
    // 先被限流一次（等过之后照常答了）：这一家最近限流过
    assert_eq!(ask(gw, "零", None, 1).send().await.unwrap().status(), 200);
    let r = ask_stream(gw, "一").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    assert!(text.contains("text_delta"), "{text}");
    assert!(
        !text.contains("rate_limit_error"),
        "客户端看到了限流：{text}"
    );
    let times = up.times();
    assert_eq!(times.len(), 4);
    assert!(times[3] - times[2] >= Duration::from_millis(950));
    let (chain, _) = log.routed("一").await;
    assert_eq!(chain.len(), 2, "{chain:#?}");
    assert_eq!(chain[0].outcome, AttemptOutcome::Error);
    assert!(
        chain[0]
            .pause_ms
            .is_some_and(|ms| (900..=1_000).contains(&ms)),
        "{chain:#?}"
    );
    assert_eq!(chain[1].outcome, AttemptOutcome::Served);
    assert!(chain[1].queued_ms.is_some_and(|ms| ms >= 900), "{chain:#?}");
}

/// 探路的拿到的 429 是额度用完了（等不到）：它没有「好了没有」的结论，**排在后面的不用干等完
/// 宽限期**，照常发，这一家怎么样由熔断说了算
#[tokio::test]
async fn a_probe_answered_with_a_used_up_quota_lets_the_queue_go_at_once() {
    let used_up = Reply {
        body: r#"{"type":"error","error":{"type":"rate_limit_error","message":"You have reached your usage limit for this plan"}}"#.into(),
        delay_ms: 400,
        ..limited(&[])
    };
    let up = upstream(vec![limited(&[("retry-after-ms", "300")]), used_up], ok()).await;
    let (gw, _state, _log) = serve(config(&[&up], 20)).await;
    let first = tokio::spawn(ask(gw, "一", None, 1).send());
    // 探路的发出去了、还没有结论：这时来的排在后面等它
    tokio::time::sleep(Duration::from_millis(450)).await;
    let t = Instant::now();
    let r = ask(gw, "二", None, 1).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "额度用完的探路之后还干等了宽限期：{:?}",
        t.elapsed()
    );
    // 探路的那个请求自己收到的是额度用完
    assert_eq!(first.await.unwrap().unwrap().status(), 429);
    assert_eq!(up.hits(), 3);
}

/// 暂停结束、探路的名额拿到了还没发出去（插件、取密钥、等空位的时候）上游改了名：名额的
/// 主人认的是旧名字，新名字下**不能一直挂着这张发不出去的名额** —— 它不会有结论，宽限期也
/// 不从它算起，这一家就再也放不出一个请求
#[tokio::test]
async fn renaming_while_an_unsent_probe_ticket_is_out_does_not_wedge_the_upstream() {
    let up = upstream(Vec::new(), ok()).await;
    let (_gw, state, _log) = serve(config(&[&up], 10)).await;
    pause(&state, "甲", 50);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let ticket = match state.throttles.gate("甲", &[], None) {
        tw_gateway::throttle::Gate::Go(Some(p)) => p,
        _ => panic!("暂停结束后头一个该拿到探路的名额"),
    };
    state.throttles.rename("甲", "丙");
    // 名额的主人放弃了（没发出去），或者照旧名字说它发出去了：新名字下都该接着有人能探路
    drop(ticket);
    tokio::time::sleep(tw_gateway::throttle::PROBE_GRACE + Duration::from_millis(500)).await;
    assert!(
        matches!(
            state.throttles.gate("丙", &[], None),
            tw_gateway::throttle::Gate::Go(_)
        ),
        "改名之后 {:?} 还发不出去：{:?}",
        tw_gateway::throttle::PROBE_GRACE,
        state.throttles.peek("丙", &[])
    );
}

/// `failover.wait_secs: 0` 是不等：一家满着时当场回 429。**它没排过队**，不该报
/// `RequestQueued`（界面会把它显示成「排队中」）
#[tokio::test]
async fn a_request_that_is_not_allowed_to_wait_is_never_reported_as_queued() {
    let slow = Reply {
        delay_ms: 1000,
        ..ok()
    };
    let up = upstream(Vec::new(), slow).await;
    let mut cfg = config(&[&up], 0);
    cfg.providers[0].max_concurrent = Some(1);
    let (gw, _state, log) = serve(cfg).await;
    let first = tokio::spawn(ask(gw, "一", None, 1).send());
    tokio::time::sleep(Duration::from_millis(200)).await;
    let r = ask(gw, "二", None, 1).send().await.unwrap();
    assert_eq!(r.status(), 429);
    let id = log.id("二").await;
    let queued: Vec<Event> = log
        .all()
        .into_iter()
        .filter(|e| {
            matches!(e, Event::RequestQueued { id: i, .. } | Event::RequestDequeued { id: i, .. } if *i == id)
        })
        .collect();
    assert!(queued.is_empty(), "不等的请求报了排队：{queued:#?}");
    assert_eq!(first.await.unwrap().unwrap().status(), 200);
}
