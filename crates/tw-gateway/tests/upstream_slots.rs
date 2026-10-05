//! 上游的并发上限（`providers[].max_concurrent`）：满着的上游，新的对话当场跳过，留在它
//! 上面的对话等它空出来，都满着时等先空出来的那一家、等不到回 429 —— 走真实的管线，看
//! 尝试链里说的和实际去的那一家。
//!
//! 每把密钥的上限（`clients[].max_concurrent`）也在这里：它和上游的位置一样，占到回答
//! 交完为止。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::routing::post;
use bytes::Bytes;
use futures::StreamExt;
use tokio::sync::Semaphore;
use tw_api::{AttemptOutcome, AttemptView, Event, FailureSource, ServeSkip, Stay};

/// 一家假的 Anthropic 上游。请求里写着 `HOLD` 的：先吐开头和一段内容，然后一直等到测试
/// 放行（[`Up::release`]）才说完 —— 一个正在长篇作答的模型，占着一个位置。别的当场答完
struct Up {
    addr: SocketAddr,
    /// 收到的生成请求数
    hits: Arc<AtomicUsize>,
    release: Arc<Semaphore>,
}

impl Up {
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
    /// 放走一个占着位置的请求
    fn release(&self) {
        self.release.add_permits(1);
    }
}

const OPENING: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n\
event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n";

const CLOSING: &[u8] =
    b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

async fn upstream() -> Up {
    let hits = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Semaphore::new(0));
    let (h, r) = (hits.clone(), release.clone());
    let app = Router::new().route(
        "/v1/messages",
        post(move |body: Bytes| {
            let (hits, release) = (h.clone(), r.clone());
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                if String::from_utf8_lossy(&body).contains("HOLD") {
                    let stream = async_stream::stream! {
                        yield Ok::<_, std::io::Error>(Bytes::from_static(OPENING));
                        if let Ok(p) = release.acquire().await {
                            p.forget();
                        }
                        yield Ok(Bytes::from_static(CLOSING));
                    };
                    return axum::response::Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(axum::body::Body::from_stream(stream))
                        .unwrap();
                }
                // 读了 5000 token 的缓存：这段对话跨轮也值得留下
                axum::response::Response::builder()
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":3,"cache_read_input_tokens":5000,"output_tokens":1}}"#,
                    ))
                    .unwrap()
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Up {
        addr,
        hits,
        release,
    }
}

/// 甲、乙两家按顺序（`fallback`）的组「池」，默认路由「默认」把请求都交给它。`limits` 是两家
/// 各自的上限。
///
/// **`default_route` 要写**：不写的话默认路由叫 `default`，「默认」这条路由谁也不走，请求走的
/// 是内置的「全部上游」组 —— 测的就不是这个组了（[`Log::routed`] 会说出来）
fn config(a: &Up, b: &Up, limits: (Option<u32>, Option<u32>), wait_secs: u64) -> tw_config::Config {
    let limit = |n: Option<u32>| {
        n.map(|n| format!("    max_concurrent: {n}\n"))
            .unwrap_or_default()
    };
    let yaml = format!(
        "version: 1
default_route: 默认
clients:
  - name: me
    key: tw-k
providers:
  - name: 甲
    base_url: http://{}
    key: sk-x
    protocol: anthropic
{}  - name: 乙
    base_url: http://{}
    key: sk-x
    protocol: anthropic
{}groups:
  - name: 池
    type: fallback
    providers: [甲, 乙]
routes:
  - name: 默认
    rules:
      - name: 全部
        to: 池
failover:
  slot_wait_secs: {wait_secs}
",
        a.addr,
        limit(limits.0),
        b.addr,
        limit(limits.1),
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

    /// 那个请求的尝试链和对话留在哪一家的说明。**它走的是「默认」路由上的组「池」**（见
    /// [`config`]）：走到内置的「全部上游」组上的话，测的就不是这个组了
    async fn routed(&self, model: &str) -> (Vec<AttemptView>, Option<Stay>) {
        let id = self.id(model).await;
        let (route, group, attempts, stayed) = self
            .until(&format!(" {model} 的路由"), |evs| {
                evs.iter().find_map(|e| match e {
                    Event::RequestRouted {
                        id: i,
                        route,
                        group,
                        attempts,
                        affinity,
                        ..
                    } if *i == id => Some((
                        route.clone(),
                        group.clone(),
                        attempts.clone(),
                        affinity.as_ref().and_then(|a| a.stayed),
                    )),
                    _ => None,
                })
            })
            .await;
        assert_eq!(
            (route.as_str(), group.as_deref()),
            ("默认", Some("池")),
            "{model} 没走配置里的那个组"
        );
        (attempts, stayed)
    }

    async fn finished(&self, model: &str) {
        let id = self.id(model).await;
        self.until(&format!(" {model} 结束"), |evs| {
            evs.iter()
                .any(|e| matches!(e, Event::RequestFinished { id: i, .. } if *i == id))
                .then_some(())
        })
        .await
    }

    fn health_changed(&self) -> bool {
        self.0
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, Event::HealthChanged { .. }))
    }
}

async fn serve(cfg: tw_config::Config) -> (SocketAddr, tw_gateway::AppState, Log) {
    serve_state(tw_gateway::AppState::new(cfg).unwrap()).await
}

async fn serve_state(state: tw_gateway::AppState) -> (SocketAddr, tw_gateway::AppState, Log) {
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

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

/// 一个请求。`conversation` 给了的话带着会话头，`messages` 是整段对话
fn ask(
    gw: SocketAddr,
    model: &str,
    conversation: Option<&str>,
    messages: &str,
    stream: bool,
) -> reqwest::RequestBuilder {
    let body = format!(
        r#"{{"model":"{model}","max_tokens":16,"stream":{stream},"system":"你是一个助手","messages":{messages}}}"#
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

fn user(text: &str) -> String {
    format!(r#"{{"role":"user","content":"{text}"}}"#)
}

/// 同一轮里回传的工具结果：这段对话留在上次回答它的那一家
fn tool_round() -> String {
    r#"{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"文件内容"}]}"#.to_string()
}

/// 起一个占着位置的长请求（模型名 `model`），等到它已经在那家答上了。交回读完它的那个任务
async fn hold(gw: SocketAddr, log: &Log, model: &str) -> tokio::task::JoinHandle<String> {
    let r = ask(
        gw,
        model,
        None,
        &format!("[{}]", user(&format!("HOLD {model}"))),
        true,
    );
    let task = tokio::spawn(async move { r.send().await.unwrap().text().await.unwrap() });
    // 路由事件在开头的内容到了之后才发：那时它已经占着位置在答了
    log.routed(model).await;
    task
}

fn busy(a: &AttemptView) -> bool {
    a.outcome == AttemptOutcome::Error && a.skipped == Some(ServeSkip::Busy)
}

fn served(a: &AttemptView) -> bool {
    a.outcome == AttemptOutcome::Served
}

#[tokio::test]
async fn a_new_conversation_skips_a_full_upstream_and_waiting_is_not_a_failure() {
    let (a, b) = (upstream().await, upstream().await);
    let (gw, state, log) = serve(config(&a, &b, (Some(1), None), 5)).await;
    let held = hold(gw, &log, "占着").await;
    assert_eq!(a.hits(), 1);

    // 甲满着：新的对话不等它，当场换乙
    let st = ask(gw, "新的", None, &format!("[{}]", user("你好")), false)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(st, 200);
    let (chain, _) = log.routed("新的").await;
    assert_eq!(chain.len(), 2, "{chain:#?}");
    assert_eq!(chain[0].provider, "甲");
    assert!(busy(&chain[0]), "{chain:#?}");
    assert_eq!(chain[0].queued_ms, None, "新的对话不等");
    assert_eq!(
        chain[0].error.as_ref().map(|m| m.code.as_str()),
        Some("gw.busy_upstream")
    );
    assert_eq!(chain[1].provider, "乙");
    assert!(served(&chain[1]));
    assert_eq!(a.hits(), 1, "满着的甲不该收到这个请求");

    // 满着不是失败：甲没有停用，熔断也没动
    for _ in 0..5 {
        let r = ask(gw, "又一个", None, &format!("[{}]", user("再来")), false);
        assert_eq!(r.send().await.unwrap().status(), 200);
    }
    assert_eq!(state.health.state("甲"), tw_gateway::health::State::Closed);
    assert!(!log.health_changed(), "满着跳过被算成了失败");
    // 按成败分的负载均衡看的成功率也不记它：甲只答过占着的那一个，跳过的六次不算样本
    assert_eq!(
        state.health.success_rates(&["甲".to_string()]).get("甲"),
        None
    );

    // 占着的那个答完，位置还回来：下一个请求照常去甲
    a.release();
    assert!(held.await.unwrap().contains("message_stop"));
    log.finished("占着").await;
    // 换一段对话：「你好」那段由乙答过、缓存热着，会留在乙
    let st = ask(gw, "之后", None, &format!("[{}]", user("另一件事")), false)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(st, 200);
    let (chain, _) = log.routed("之后").await;
    assert_eq!(chain.len(), 1, "{chain:#?}");
    assert_eq!(chain[0].provider, "甲");
}

/// 这段对话第一轮由甲回答，交回第二个请求（同一轮里回传工具结果）的请求体
async fn conversation_on_a(gw: SocketAddr, log: &Log, model: &str) -> String {
    let first = user("帮我改一下");
    let st = ask(gw, model, Some("对话-1"), &format!("[{first}]"), false)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(st, 200);
    let (chain, _) = log.routed(model).await;
    assert_eq!(chain.last().unwrap().provider, "甲");
    // 记下由谁回答是在结局里做的
    log.finished(model).await;
    format!("[{first},{}]", tool_round())
}

#[tokio::test]
async fn a_conversation_that_stays_on_a_full_upstream_waits_for_its_slot() {
    let (a, b) = (upstream().await, upstream().await);
    let (gw, _state, log) = serve(config(&a, &b, (Some(1), None), 10)).await;
    let next = conversation_on_a(gw, &log, "对话").await;
    let held = hold(gw, &log, "占着").await;

    // 同一轮的下一个请求：留在甲是为了它的缓存，所以等甲空出来，不去乙
    let r = ask(gw, "对话", Some("对话-1"), &next, false);
    let waiting = tokio::spawn(async move { r.send().await.unwrap().status() });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiting.is_finished(), "该在等甲的空位");
    assert_eq!(b.hits(), 0, "等的时候去了乙");

    a.release();
    assert_eq!(waiting.await.unwrap(), 200);
    held.await.unwrap();
    let (chain, stayed) = {
        // 这段对话的两个请求模型名一样，取后一个的路由
        let id = log
            .until("第二个请求", |evs| {
                evs.iter()
                    .filter_map(|e| match e {
                        Event::RequestStarted { id, model, .. } if model == "对话" => Some(*id),
                        _ => None,
                    })
                    .nth(1)
            })
            .await;
        log.until("它的路由", |evs| {
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
    };
    assert_eq!(stayed, Some(Stay::Turn));
    assert_eq!(chain.len(), 1, "{chain:#?}");
    assert_eq!(chain[0].provider, "甲");
    assert!(served(&chain[0]));
    let queued = chain[0].queued_ms.expect("等过的那一跳没记等了多久");
    assert!(queued >= 250, "等了 {queued} 毫秒");
    assert!(chain[0].ms < queued, "等的时间不算在这一跳自己的耗时里");
    assert_eq!(b.hits(), 0);
}

#[tokio::test]
async fn after_the_wait_the_conversation_moves_on_and_answers_elsewhere() {
    let (a, b) = (upstream().await, upstream().await);
    let (gw, _state, log) = serve(config(&a, &b, (Some(1), None), 1)).await;
    let next = conversation_on_a(gw, &log, "对话").await;
    let _held = hold(gw, &log, "占着").await;

    let t = std::time::Instant::now();
    let st = ask(gw, "对话", Some("对话-1"), &next, false)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(st, 200);
    assert!(t.elapsed() >= Duration::from_millis(900), "没等够就走了");
    let id = log
        .until("第二个请求", |evs| {
            evs.iter()
                .filter_map(|e| match e {
                    Event::RequestStarted { id, model, .. } if model == "对话" => Some(*id),
                    _ => None,
                })
                .nth(1)
        })
        .await;
    let chain = log
        .until("它的路由", |evs| {
            evs.iter().find_map(|e| match e {
                Event::RequestRouted {
                    id: i, attempts, ..
                } if *i == id => Some(attempts.clone()),
                _ => None,
            })
        })
        .await;
    assert_eq!(chain.len(), 2, "{chain:#?}");
    assert_eq!(chain[0].provider, "甲");
    assert!(busy(&chain[0]));
    let queued = chain[0].queued_ms.expect("等过甲，却没记等了多久");
    assert!(queued >= 900, "等了 {queued} 毫秒");
    assert_eq!(chain[1].provider, "乙");
    assert!(served(&chain[1]));
    assert_eq!(chain[1].queued_ms, None, "乙有空位，没等");
}

#[tokio::test]
async fn when_every_upstream_is_full_the_request_waits_and_is_told_to_come_back() {
    let (a, b) = (upstream().await, upstream().await);
    let (gw, _state, log) = serve(config(&a, &b, (Some(1), Some(1)), 1)).await;
    let _on_a = hold(gw, &log, "占着甲").await;
    // 甲满了，这一个跳到乙，把乙也占满
    let _on_b = hold(gw, &log, "占着乙").await;
    assert_eq!((a.hits(), b.hits()), (1, 1));

    let t = std::time::Instant::now();
    let r = ask(gw, "挤不进", None, &format!("[{}]", user("你好")), false)
        .send()
        .await
        .unwrap();
    assert!(t.elapsed() >= Duration::from_millis(900), "没等就拒了");
    assert_eq!(r.status(), 429);
    assert_eq!(r.headers()["retry-after"], "5");
    assert_eq!(r.headers()["x-thinkwatch-error"], "rate_limited");
    let json: serde_json::Value = r.json().await.unwrap();
    // 客户端认得的形状：Anthropic 的限流错误
    assert_eq!(json["error"]["type"], "rate_limit_error", "{json}");
    let text = json["error"]["message"].as_str().unwrap();
    assert!(text.contains("`甲`") && text.contains("`乙`"), "{text}");

    let id = log.id("挤不进").await;
    let (source, code) = log
        .until("失败", |evs| {
            evs.iter().find_map(|e| match e {
                Event::RequestFailed {
                    id: i,
                    source,
                    message,
                    ..
                } if *i == id => Some((*source, message.code.clone())),
                _ => None,
            })
        })
        .await;
    assert_eq!(source, FailureSource::RateLimited);
    assert_eq!(code, "gw.busy_all");
    let (chain, _) = log.routed("挤不进").await;
    assert_eq!(chain.len(), 2, "{chain:#?}");
    assert!(chain.iter().all(busy), "{chain:#?}");
    assert_eq!((a.hits(), b.hits()), (1, 1), "满着的上游一个都不该收到它");
}

#[tokio::test]
async fn when_every_upstream_is_full_the_first_to_free_takes_the_request() {
    let (a, b) = (upstream().await, upstream().await);
    let (gw, _state, log) = serve(config(&a, &b, (Some(1), Some(1)), 10)).await;
    let _on_a = hold(gw, &log, "占着甲").await;
    let on_b = hold(gw, &log, "占着乙").await;

    let r = ask(gw, "等着", None, &format!("[{}]", user("你好")), false);
    let waiting = tokio::spawn(async move { r.send().await.unwrap().status() });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiting.is_finished());
    // 乙先空出来
    b.release();
    on_b.await.unwrap();
    assert_eq!(waiting.await.unwrap(), 200);

    let (chain, _) = log.routed("等着").await;
    assert_eq!(chain.len(), 3, "{chain:#?}");
    assert!(busy(&chain[0]) && busy(&chain[1]), "{chain:#?}");
    assert_eq!(chain[2].provider, "乙");
    assert!(served(&chain[2]));
    assert!(chain[2].queued_ms.is_some_and(|ms| ms >= 250), "{chain:#?}");
    assert_eq!(a.hits(), 1);
}

#[tokio::test]
async fn a_client_that_leaves_mid_stream_gives_the_slot_back() {
    let (a, b) = (upstream().await, upstream().await);
    let (gw, _state, log) = serve(config(&a, &b, (Some(1), None), 5)).await;

    // 读到第一段就走（Claude Code 里按一下 Esc）。上游那边还在答
    let resp = ask(gw, "走了", None, &format!("[{}]", user("HOLD 走了")), true)
        .send()
        .await
        .unwrap();
    let mut body = resp.bytes_stream();
    body.next().await.unwrap().unwrap();
    drop(body);
    let id = log.id("走了").await;
    log.until("取消", |evs| {
        evs.iter()
            .any(|e| matches!(e, Event::RequestCancelled { id: i, .. } if *i == id))
            .then_some(())
    })
    .await;

    let st = ask(gw, "下一个", None, &format!("[{}]", user("你好")), false)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(st, 200);
    let (chain, _) = log.routed("下一个").await;
    assert_eq!(chain.len(), 1, "走掉的那个还占着甲的位置：{chain:#?}");
    assert_eq!(chain[0].provider, "甲");
    assert_eq!(b.hits(), 0);
}

#[tokio::test]
async fn raising_the_limit_on_reload_lets_a_waiting_request_go() {
    let (a, b) = (upstream().await, upstream().await);
    let (gw, state, log) = serve(config(&a, &b, (Some(1), Some(1)), 10)).await;
    let _on_a = hold(gw, &log, "占着甲").await;
    let _on_b = hold(gw, &log, "占着乙").await;

    let r = ask(gw, "等着", None, &format!("[{}]", user("你好")), false);
    let waiting = tokio::spawn(async move { r.send().await.unwrap().status() });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiting.is_finished());
    // 甲调到 2：等着的那个马上进去
    state
        .reload(config(&a, &b, (Some(2), Some(1)), 10))
        .unwrap();
    let st = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .expect("调大了上限，等着的还在等")
        .unwrap();
    assert_eq!(st, 200);
    let (chain, _) = log.routed("等着").await;
    assert_eq!(chain.last().unwrap().provider, "甲", "{chain:#?}");
}

#[tokio::test]
async fn counting_tokens_does_not_wait_for_a_slot() {
    let (a, b) = (upstream().await, upstream().await);
    let (gw, _state, log) = serve(config(&a, &b, (Some(1), None), 10)).await;
    let _held = hold(gw, &log, "占着").await;

    // 甲满着，数 token 照样发给甲：它不跑模型。假的上游没有这个接口（404），由网关自己估
    let st = client()
        .post(format!("http://{gw}/v1/messages/count_tokens"))
        .header("x-api-key", "tw-k")
        .header("anthropic-version", "2023-06-01")
        .body(format!(
            r#"{{"model":"数","system":"你是一个助手","messages":[{}]}}"#,
            user("数一数")
        ))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(st, 200);
    let (chain, _) = log.routed("数").await;
    assert_eq!(chain.len(), 1, "{chain:#?}");
    assert_eq!(chain[0].provider, "甲");
    assert_eq!(chain[0].outcome, AttemptOutcome::Estimated);
    assert_eq!(chain[0].status, Some(404), "该是发给了甲，而不是跳过");
    assert_eq!(chain[0].skipped, None);
}

/// 一把密钥的上限管的是整个回答：流还在走，它就还占着那一份。以前通行证在响应头交出去
/// 时就还了，上限 1 的密钥照样能同时跑好几条流
#[tokio::test]
async fn a_keys_limit_holds_until_the_streamed_answer_ends() {
    let (a, b) = (upstream().await, upstream().await);
    let mut cfg = config(&a, &b, (None, None), 10);
    cfg.clients[0].max_concurrent = Some(1);
    let (gw, _state, log) = serve(cfg).await;
    let held = hold(gw, &log, "占着").await;

    // 同一把密钥的第二个请求：等前一条流走完
    let r = ask(gw, "第二个", None, &format!("[{}]", user("你好")), false);
    let waiting = tokio::spawn(async move { r.send().await.unwrap().status() });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !waiting.is_finished(),
        "前一条流还在走，这把密钥已经到上限了"
    );
    assert_eq!(a.hits() + b.hits(), 1, "第二个不该发出去");

    a.release();
    assert!(held.await.unwrap().contains("message_stop"));
    let st = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .expect("前一条流走完了，第二个还在等")
        .unwrap();
    assert_eq!(st, 200);
}

/// `load-balance` 组里满着的那一家不参加这一轮：轮到它的话它被当场跳过，这一份却记在它头上，
/// 它答得越多越满、越满越被记空账，拿到的比它的权重少
#[tokio::test]
async fn a_full_member_of_a_load_balance_group_sits_its_turns_out() {
    let (a, b) = (upstream().await, upstream().await);
    let mut cfg = config(&a, &b, (Some(1), None), 5);
    cfg.groups[0].kind = tw_engine::GroupType::LoadBalance;
    let (gw, state, log) = serve(cfg).await;
    let group = state.runtime().engine.groups()[0].clone();
    // 头一个轮到甲，占着它的那个位置
    let held = hold(gw, &log, "占着").await;
    assert_eq!(a.hits(), 1);
    let before = state.balance.peek(&group);

    // 甲满着的时候，每个新对话都排给乙：没有一个先轮到甲、再被跳过
    for i in 0..4 {
        let model = format!("新的{i}");
        let st = ask(gw, &model, None, &format!("[{}]", user(&model)), false)
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(st, 200);
        let (chain, _) = log.routed(&model).await;
        assert_eq!(chain.len(), 1, "轮到了满着的甲：{chain:#?}");
        assert_eq!(chain[0].provider, "乙");
    }
    assert_eq!(
        state.balance.peek(&group).get("甲"),
        before.get("甲"),
        "满着的甲被记了账"
    );
    a.release();
    held.await.unwrap();
}

/// 用量上限看的时钟：跟着真实时间走，还能往后拨
struct Ahead(std::sync::atomic::AtomicI64);

impl tw_gateway::key_limits::Clock for Ahead {
    fn now_ms(&self) -> i64 {
        tw_gateway::key_limits::SystemClock.now_ms() + self.0.load(Ordering::SeqCst)
    }
    fn period(&self, per: tw_config::LimitPer, at_ms: i64) -> (i64, i64) {
        tw_gateway::key_limits::SystemClock.period(per, at_ms)
    }
    fn show(&self, at_ms: i64) -> String {
        tw_gateway::key_limits::SystemClock.show(at_ms)
    }
}

/// 一个请求只有一段可等的时间：等密钥的分钟上限用掉的，等上游空位时就少等那么久。两段各给
/// 一份的话，这个请求要等两倍那么久才收到 429
#[tokio::test]
async fn the_key_limit_wait_and_the_slot_wait_share_one_budget() {
    use tw_gateway::key_limits::Clock as _;
    let (a, b) = (upstream().await, upstream().await);
    let mut cfg = config(&a, &b, (Some(1), Some(1)), 2);
    cfg.clients[0].limits = serde_yaml_ng::from_str("[{per: minute, requests: 2}]").unwrap();
    let mut state = tw_gateway::AppState::new(cfg).unwrap();
    let clock = Arc::new(Ahead(Default::default()));
    state.set_key_limits_clock(clock.clone());
    let (gw, _state, log) = serve_state(state).await;
    let _on_a = hold(gw, &log, "占着甲").await;
    let _on_b = hold(gw, &log, "占着乙").await;
    // 拨到第一个请求之后 59 秒：这一分钟的两个用满了，第一个再过不到一秒滑出去
    clock.0.store(59_000, Ordering::SeqCst);
    assert!(clock.now_ms() > 0);

    let t = std::time::Instant::now();
    let r = ask(gw, "挤不进", None, &format!("[{}]", user("你好")), false)
        .send()
        .await
        .unwrap();
    let took = t.elapsed();
    assert_eq!(r.status(), 429);
    let id = log.id("挤不进").await;
    let code = log
        .until("失败", |evs| {
            evs.iter().find_map(|e| match e {
                Event::RequestFailed { id: i, message, .. } if *i == id => {
                    Some(message.code.clone())
                }
                _ => None,
            })
        })
        .await;
    assert_eq!(code, "gw.busy_all", "该是等过了分钟上限、再等上游的空位");
    assert!(
        took >= Duration::from_millis(1_800) && took < Duration::from_millis(2_500),
        "等了 {took:?}：两段该共用 2 秒"
    );
}
