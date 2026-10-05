//! 开头慢就换下一家（`failover.next_on_slow_start`），端到端。
//!
//! 假上游先回响应头和开头的例行帧，之后按脚本隔一阵才出内容，或者一直只发心跳不出内容；
//! 它记下自己的响应被丢掉了没有 —— 网关放弃它时，连接要真的断开，上游才会停下。

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{Value, json};
use tw_config::{Client, Config, Failover, Protocol, Provider};

/// 一个假上游的样子：响应头之前等多久，之后按顺序隔多少毫秒发哪一段，发完了挂不挂着
#[derive(Clone)]
struct Script {
    header_delay_ms: u64,
    steps: Vec<(u64, &'static str)>,
    /// 发完了不收尾，每 100 毫秒发一行注释（SSE 的心跳），直到连接断开
    hang: bool,
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
            let hang = script.hang;
            let tail = futures::stream::unfold((), move |_| async move {
                if !hang {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                Some((Ok(Bytes::from_static(b": ping\n\n")), ()))
            });
            let body = steps.chain(tail).map(move |x| {
                let _ = &flag;
                x
            });
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
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
        header_delay_ms: 0,
        steps: vec![(0, MESSAGE_START), (0, PING)],
        hang: true,
    }
}

/// 马上就答
fn prompt(word: &'static str) -> Script {
    Script {
        header_delay_ms: 0,
        steps: vec![(0, MESSAGE_START), (0, answer(word))],
        hang: false,
    }
}

/// 开了流，隔 `ms` 毫秒才答
fn late(ms: u64, word: &'static str) -> Script {
    Script {
        header_delay_ms: 0,
        steps: vec![(0, MESSAGE_START), (ms, answer(word))],
        hang: false,
    }
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

/// 起网关。等 1 秒（测试里图快；配置校验要求开着时至少 5 秒，网关自己不查）
async fn gateway(
    providers: Vec<Provider>,
    switch: bool,
) -> (
    SocketAddr,
    tokio::sync::broadcast::Receiver<tw_api::Event>,
    Arc<tw_gateway::Health>,
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
            stream_start_wait_secs: 1,
            next_on_slow_start: switch,
            ..Default::default()
        },
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let rx = state.bus.subscribe();
    let health = state.health.clone();
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, rx, health)
}

fn messages(stream: bool) -> Value {
    json!({"model": "claude-sonnet-5", "max_tokens": 1024, "stream": stream,
           "messages": [{"role": "user", "content": "Say hello."}]})
}

async fn post(gw: SocketAddr, path: &str, body: &Value) -> (u16, String) {
    let resp = reqwest::Client::new()
        .post(format!("http://{gw}{path}"))
        .header("content-type", "application/json")
        .header("x-api-key", "tw-k")
        .header("authorization", "Bearer tw-k")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap())
}

/// 这个请求的估算输入（开始事件带着）和尝试链
async fn estimate_and_attempts(
    rx: &mut tokio::sync::broadcast::Receiver<tw_api::Event>,
) -> (Option<u64>, Vec<tw_api::AttemptView>) {
    let mut estimate = None;
    loop {
        let e = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("no routing event")
            .unwrap();
        match e {
            tw_api::Event::RequestStarted { input_estimate, .. } => estimate = input_estimate,
            tw_api::Event::RequestRouted { attempts, .. } => return (estimate, attempts),
            _ => {}
        }
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

#[tokio::test]
async fn a_slow_first_upstream_is_dropped_after_the_wait_and_the_next_one_answers() {
    let slow = upstream(stalled()).await;
    let good = upstream(prompt("hello")).await;
    let (gw, mut rx, health) = gateway(
        vec![
            provider("slow", &slow, Protocol::Anthropic),
            provider("good", &good, Protocol::Anthropic),
        ],
        true,
    )
    .await;

    let t = Instant::now();
    let (status, text) = post(gw, "/v1/messages", &messages(true)).await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("hello"), "{text}");
    assert!(
        !text.contains("ping"),
        "慢的那一家的开头不该到客户端：{text}"
    );
    assert!(t.elapsed() >= Duration::from_secs(1), "等够了才换");
    assert!(
        eventually(&slow.dropped).await,
        "放弃的那一家连接要断开，它才会停下"
    );

    let (_, attempts) = estimate_and_attempts(&mut rx).await;
    use tw_api::AttemptOutcome::{Served, SlowStart};
    assert_eq!(outcomes(&attempts), [("slow", SlowStart), ("good", Served)]);
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
    assert_eq!(
        gave_up.error.as_ref().map(|m| m.code.as_str()),
        Some("gw.slow_start")
    );

    // **慢不是坏**：不停用，几次之后也照样先试它
    for _ in 0..4 {
        let (status, _) = post(gw, "/v1/messages", &messages(true)).await;
        assert_eq!(status, 200);
    }
    assert!(health.is_available("slow"), "慢的那一家不该被停用");
    assert_eq!(slow.hits.load(Ordering::SeqCst), 5, "每次都还先试它");
    // 按成败分的负载均衡看的成功率也不记它：换走了五次，它一次都没失败过，也没答上过
    let rates = health.success_rates(&["slow".to_string(), "good".to_string()]);
    assert_eq!(rates.get("slow"), None, "{rates:?}");
    assert_eq!(rates.get("good"), Some(&1.0), "{rates:?}");
}

#[tokio::test]
async fn the_last_upstream_is_not_given_up_on() {
    // 第一家慢、被放弃；第二家也慢，但它是最后一家：照常等它
    let slow = upstream(stalled()).await;
    let last = upstream(late(1_600, "finally")).await;
    let (gw, mut rx, _) = gateway(
        vec![
            provider("slow", &slow, Protocol::Anthropic),
            provider("last", &last, Protocol::Anthropic),
        ],
        true,
    )
    .await;
    let (status, text) = post(gw, "/v1/messages", &messages(true)).await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("finally"), "{text}");
    let (_, attempts) = estimate_and_attempts(&mut rx).await;
    use tw_api::AttemptOutcome::{Served, SlowStart};
    assert_eq!(outcomes(&attempts), [("slow", SlowStart), ("last", Served)]);
}

#[tokio::test]
async fn a_next_upstream_that_is_paused_by_then_does_not_count() {
    // 第一家慢，第二家在等的时候被停用了：后面没有接得下的，第一家就是最后一家，照常等它
    let slow = upstream(late(1_600, "patience")).await;
    let other = upstream(prompt("other")).await;
    let (gw, mut rx, health) = gateway(
        vec![
            provider("slow", &slow, Protocol::Anthropic),
            provider("other", &other, Protocol::Anthropic),
        ],
        true,
    )
    .await;
    let pause = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        for _ in 0..3 {
            health.record_failure("other");
        }
        assert!(!health.is_available("other"));
    });
    let (status, text) = post(gw, "/v1/messages", &messages(true)).await;
    pause.await.unwrap();
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("patience"), "{text}");
    assert_eq!(other.hits.load(Ordering::SeqCst), 0);
    let (_, attempts) = estimate_and_attempts(&mut rx).await;
    assert_eq!(
        outcomes(&attempts),
        [("slow", tw_api::AttemptOutcome::Served)]
    );
}

#[tokio::test]
async fn a_next_upstream_that_cannot_take_the_request_does_not_count() {
    // Claude Code 搜网页的请求强制要用服务端工具 `web_search`，发不到 Chat 格式的上游：
    // 那一家接不下，第一家就是最后一家
    let slow = upstream(late(1_600, "searched")).await;
    let chat = upstream(prompt("other")).await;
    let (gw, mut rx, _) = gateway(
        vec![
            provider("slow", &slow, Protocol::Anthropic),
            provider("chat", &chat, Protocol::OpenaiChat),
        ],
        true,
    )
    .await;
    let body = json!({
        "model": "claude-sonnet-5", "max_tokens": 1024, "stream": true,
        "messages": [{"role": "user", "content": "Perform a web search for the query: bedrock pricing"}],
        "tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": 8}],
        "tool_choice": {"type": "tool", "name": "web_search"}
    });
    let (status, text) = post(gw, "/v1/messages", &body).await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("searched"), "{text}");
    assert_eq!(chat.hits.load(Ordering::SeqCst), 0);
    let (_, attempts) = estimate_and_attempts(&mut rx).await;
    assert_eq!(
        outcomes(&attempts),
        [("slow", tw_api::AttemptOutcome::Served)]
    );
}

#[tokio::test]
async fn switched_off_the_slow_start_is_handed_on_as_before() {
    let slow = upstream(late(1_600, "eventually")).await;
    let other = upstream(prompt("other")).await;
    let (gw, mut rx, _) = gateway(
        vec![
            provider("slow", &slow, Protocol::Anthropic),
            provider("other", &other, Protocol::Anthropic),
        ],
        false,
    )
    .await;
    let (status, text) = post(gw, "/v1/messages", &messages(true)).await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("eventually"), "{text}");
    assert_eq!(other.hits.load(Ordering::SeqCst), 0);
    let (_, attempts) = estimate_and_attempts(&mut rx).await;
    assert_eq!(
        outcomes(&attempts),
        [("slow", tw_api::AttemptOutcome::Served)]
    );
}

#[tokio::test]
async fn thinking_counts_as_content() {
    // 开头就在想（推理的字在流），正文要过一阵才来：这不是开头慢
    let thinker = upstream(Script {
        header_delay_ms: 0,
        steps: vec![
            (0, MESSAGE_START),
            (200, THINKING),
            (1_400, answer("thought")),
        ],
        hang: false,
    })
    .await;
    let other = upstream(prompt("other")).await;
    let (gw, mut rx, _) = gateway(
        vec![
            provider("thinker", &thinker, Protocol::Anthropic),
            provider("other", &other, Protocol::Anthropic),
        ],
        true,
    )
    .await;
    let (status, text) = post(gw, "/v1/messages", &messages(true)).await;
    assert_eq!(status, 200, "{text}");
    assert!(
        text.contains("Let me think") && text.contains("thought"),
        "{text}"
    );
    assert_eq!(other.hits.load(Ordering::SeqCst), 0);
    let (_, attempts) = estimate_and_attempts(&mut rx).await;
    assert_eq!(
        outcomes(&attempts),
        [("thinker", tw_api::AttemptOutcome::Served)]
    );
}

#[tokio::test]
async fn without_reported_usage_the_estimate_is_recorded_as_possibly_billed() {
    // Chat 格式的上游开头只发一块角色、不报用量：记网关估的输入
    let silent = upstream(Script {
        header_delay_ms: 0,
        steps: vec![(
            0,
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
        )],
        hang: true,
    })
    .await;
    let good = upstream(Script {
        header_delay_ms: 0,
        steps: vec![(
            0,
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi there\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        )],
        hang: false,
    })
    .await;
    let (gw, mut rx, _) = gateway(
        vec![
            provider("silent", &silent, Protocol::OpenaiChat),
            provider("good", &good, Protocol::OpenaiChat),
        ],
        true,
    )
    .await;
    let body = json!({"model": "gpt-5", "stream": true,
                      "messages": [{"role": "user", "content": "Say hello."}]});
    let (status, text) = post(gw, "/v1/chat/completions", &body).await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("hi there"), "{text}");
    assert!(eventually(&silent.dropped).await);
    let (estimate, attempts) = estimate_and_attempts(&mut rx).await;
    use tw_api::AttemptOutcome::{Served, SlowStart};
    assert_eq!(
        outcomes(&attempts),
        [("silent", SlowStart), ("good", Served)]
    );
    let estimate = estimate.expect("解得开的请求有估算");
    assert!(estimate > 0);
    assert_eq!(
        attempts[0].usage,
        Some(tw_api::AttemptUsage {
            input: estimate,
            cache_read: 0,
            cache_write: 0,
            estimated: true,
        })
    );
}

#[tokio::test]
async fn response_headers_that_never_come_count_as_a_slow_start() {
    // 响应头都迟迟不来（中转站排着队）：一样从发出请求算起，到点放弃
    let queued = upstream(Script {
        header_delay_ms: 5_000,
        ..prompt("too late")
    })
    .await;
    let good = upstream(prompt("hello")).await;
    let (gw, mut rx, _) = gateway(
        vec![
            provider("queued", &queued, Protocol::Anthropic),
            provider("good", &good, Protocol::Anthropic),
        ],
        true,
    )
    .await;
    let t = Instant::now();
    let (status, text) = post(gw, "/v1/messages", &messages(true)).await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("hello"), "{text}");
    assert!(t.elapsed() < Duration::from_secs(4), "没有等到响应头");
    assert!(eventually(&queued.dropped).await, "还在等的请求要断开");
    let (_, attempts) = estimate_and_attempts(&mut rx).await;
    use tw_api::AttemptOutcome::{Served, SlowStart};
    assert_eq!(
        outcomes(&attempts),
        [("queued", SlowStart), ("good", Served)]
    );
    assert_eq!(attempts[0].status, None, "响应头没到，没有状态码");
    assert!(attempts[0].usage.is_some_and(|u| u.estimated));
}

#[tokio::test]
async fn a_request_that_does_not_stream_is_not_switched() {
    // 整包的请求本来就要等全部生成完：响应头来得晚也照常等
    let slow = upstream(Script {
        header_delay_ms: 1_600,
        ..prompt("whole")
    })
    .await;
    let other = upstream(prompt("other")).await;
    let (gw, mut rx, _) = gateway(
        vec![
            provider("slow", &slow, Protocol::Anthropic),
            provider("other", &other, Protocol::Anthropic),
        ],
        true,
    )
    .await;
    let (status, text) = post(gw, "/v1/messages", &messages(false)).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(other.hits.load(Ordering::SeqCst), 0);
    let (_, attempts) = estimate_and_attempts(&mut rx).await;
    assert_eq!(
        outcomes(&attempts),
        [("slow", tw_api::AttemptOutcome::Served)]
    );
}
