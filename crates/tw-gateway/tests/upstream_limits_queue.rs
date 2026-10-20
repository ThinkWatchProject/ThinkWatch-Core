//! 上游的用量上限接在管线上（`tw_gateway::upstream_limits` + `tw_gateway::throttle`），端到端。
//!
//! 分钟、小时的窗口满了：请求等它空出来再发；天、周、月用完了：跳过，都用完了回 429、到重置
//! 之前别重试；回了 429 的、没发出去的那一跳不算进这家的上限。时钟可以往前拨（[`Ahead`]）：
//! 拨到第一个请求之后 59 秒，这一分钟的窗口不到一秒就空出来，测试不用干等一分钟。

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use tw_api::{AttemptOutcome, AttemptView, Event, ServeSkip, ThrottleKind};
use tw_gateway::key_limits::Clock;

/// 假上游的一次答复
#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
}

fn ok() -> Reply {
    Reply {
        status: 200,
        headers: Vec::new(),
        body: r#"{"type":"message","role":"assistant","model":"claude-sonnet-5","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":1}}"#.into(),
    }
}

/// 限流：暂停 `ms` 毫秒
fn limited(ms: u64) -> Reply {
    Reply {
        status: 429,
        headers: vec![("retry-after-ms", ms.to_string())],
        body: r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
            .into(),
    }
}

/// 一家假上游：先按 `script` 的顺序答，答完了一律答 `then`。数着收到了几个请求
struct Up {
    addr: SocketAddr,
    hits: Arc<Mutex<Vec<Instant>>>,
}

impl Up {
    fn hits(&self) -> usize {
        self.hits.lock().unwrap().len()
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
            let mut b = axum::response::Response::builder()
                .status(r.status)
                .header("content-type", "application/json");
            for (k, v) in &r.headers {
                b = b.header(*k, v.as_str());
            }
            b.body(axum::body::Body::from(r.body)).unwrap()
        }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Up { addr, hits }
}

/// 一个没人听的地址：连不上
async fn nobody() -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    addr
}

/// 系统时钟，可以往前拨
struct Ahead(AtomicI64);

impl Ahead {
    fn new() -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(0)))
    }
    fn jump(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for Ahead {
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

/// 甲（和乙）按顺序组成「池」，默认路由把请求都交给它们。每一家带着自己的 `limits`（YAML）
fn config(ups: &[(SocketAddr, &str)], wait_secs: u64) -> tw_config::Config {
    let names = ["甲", "乙"];
    let mut yaml =
        "version: 1\ndefault_route: 默认\nclients:\n  - name: me\n    key: tw-k\nproviders:\n"
            .to_string();
    for (n, (addr, limits)) in names.iter().zip(ups) {
        yaml += &format!(
            "  - name: {n}\n    base_url: http://{addr}\n    key: sk-x\n    protocol: anthropic\n    limits: {limits}\n"
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

    async fn id(&self, model: &str) -> u64 {
        self.until(&format!(" {model} 开始"), |evs| {
            evs.iter().find_map(|e| match e {
                Event::RequestStarted { id, model: m, .. } if m == model => Some(*id),
                _ => None,
            })
        })
        .await
    }

    async fn routed(&self, model: &str) -> Vec<AttemptView> {
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

    async fn failed_code(&self, model: &str) -> String {
        let id = self.id(model).await;
        self.until(&format!(" {model} 失败"), |evs| {
            evs.iter().find_map(|e| match e {
                Event::RequestFailed { id: i, message, .. } if *i == id => {
                    Some(message.code.clone())
                }
                _ => None,
            })
        })
        .await
    }

    fn all(&self) -> Vec<Event> {
        self.0.lock().unwrap().clone()
    }
}

async fn serve(
    cfg: tw_config::Config,
    clock: Arc<dyn Clock>,
) -> (SocketAddr, tw_gateway::AppState, Log) {
    let mut state = tw_gateway::AppState::new(cfg).unwrap();
    state.set_key_limits_clock(clock);
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

async fn ask(gw: SocketAddr, model: &str) -> reqwest::Response {
    let body = format!(
        r#"{{"model":"{model}","max_tokens":16,"messages":[{{"role":"user","content":"{model}"}}]}}"#
    );
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .header("anthropic-version", "2023-06-01")
        .body(body)
        .send()
        .await
        .unwrap()
}

fn head(r: &reqwest::Response, h: &str) -> Option<String> {
    r.headers().get(h).map(|v| v.to_str().unwrap().to_string())
}

/// 这一家第 `i` 条上限此刻用了多少（`ProviderView::limits` 里的那个数）
fn used(state: &tw_gateway::AppState, name: &str, i: usize) -> u64 {
    let cfg = state.config();
    let p = cfg.providers.iter().find(|p| p.name == name).unwrap();
    state.upstream_limits.view(name, &p.limits)[i].used
}

/// 一分钟一个请求：第二个请求**等窗口空出来再发**，客户端照常拿到回答。尝试链上先是跳过的
/// 那一跳（`window_full`），再是等过之后发出去的那一跳；这一家一满就报「用量已满」
#[tokio::test]
async fn a_full_minute_window_makes_the_request_wait_and_then_send() {
    let up = upstream(Vec::new(), ok()).await;
    let clock = Ahead::new();
    let (gw, state, log) = serve(
        config(&[(up.addr, "[{per: minute, requests: 1}]")], 10),
        clock.clone(),
    )
    .await;
    assert_eq!(ask(gw, "第一个").await.status(), 200);
    assert_eq!(used(&state, "甲", 0), 1);
    // 用满了就报：不用等哪个请求结束
    log.until("用量已满", |evs| {
        evs.iter().find_map(|e| match e {
            Event::ThrottleChanged {
                provider,
                throttle: Some(t),
                ..
            } if provider == "甲" && t.kind == ThrottleKind::WindowFull => Some(()),
            _ => None,
        })
    })
    .await;
    // 拨到第一个请求之后 59.2 秒：再过不到一秒滑出去
    clock.jump(59_200);
    let throttle = state.throttles.view("甲").expect("still full");
    assert_eq!(throttle.kind, ThrottleKind::WindowFull);
    assert_eq!(throttle.limit, Some(0));

    let t = Instant::now();
    let r = ask(gw, "第二个").await;
    assert_eq!(r.status(), 200);
    let took = t.elapsed();
    assert!(
        took >= Duration::from_millis(500) && took < Duration::from_secs(3),
        "该等窗口空出来：{took:?}"
    );
    assert_eq!(up.hits(), 2);

    let chain = log.routed("第二个").await;
    assert_eq!(chain.len(), 2, "{chain:#?}");
    assert_eq!(chain[0].skipped, Some(ServeSkip::WindowFull), "{chain:#?}");
    assert_eq!(chain[1].outcome, AttemptOutcome::Served);
    assert!(chain[1].queued_ms.is_some_and(|ms| ms >= 400), "{chain:#?}");
    let id = log.id("第二个").await;
    assert!(
        log.all().iter().any(|e| matches!(
            e,
            Event::RequestQueued { id: i, upstreams, .. } if *i == id && upstreams == &["甲".to_string()]
        )),
        "没报排队"
    );
    // 空出来的那个位置当场给了它：这一家又满了，报的是新的恢复时刻
    let fulls: Vec<u64> = log
        .all()
        .iter()
        .filter_map(|e| match e {
            Event::ThrottleChanged {
                provider,
                throttle: Some(t),
                ..
            } if provider == "甲" && t.kind == ThrottleKind::WindowFull => Some(t.until_ms),
            _ => None,
        })
        .collect();
    assert!(
        fulls.len() >= 2 && fulls.last() > fulls.first(),
        "{fulls:?}"
    );
    // 用量上限满了不是失败：不熔断
    assert!(
        !log.all()
            .iter()
            .any(|e| matches!(e, Event::HealthChanged { .. }))
    );
}

/// `every: 5`：窗口是最近 5 分钟。一分钟之后还满着；离空出来比这个请求能等的还久，当场 429，
/// `Retry-After` 是还要多久；拨到快满 5 分钟时，等一下就发得出去
#[tokio::test]
async fn an_every_five_minutes_window_spans_five_minutes() {
    let up = upstream(Vec::new(), ok()).await;
    let clock = Ahead::new();
    let (gw, _state, log) = serve(
        config(&[(up.addr, "[{per: minute, every: 5, requests: 1}]")], 2),
        clock.clone(),
    )
    .await;
    assert_eq!(ask(gw, "第一个").await.status(), 200);
    clock.jump(61_000);

    let t = Instant::now();
    let r = ask(gw, "第二个").await;
    assert!(t.elapsed() < Duration::from_secs(1), "等不到就不该干等");
    assert_eq!(r.status(), 429);
    assert_eq!(head(&r, "x-should-retry").as_deref(), Some("true"));
    let after: u64 = head(&r, "retry-after").unwrap().parse().unwrap();
    assert!((237..=240).contains(&after), "Retry-After {after}");
    let after_ms: u64 = head(&r, "retry-after-ms").unwrap().parse().unwrap();
    assert!(
        (236_000..=239_000).contains(&after_ms),
        "retry-after-ms {after_ms}"
    );
    assert_eq!(log.failed_code("第二个").await, "gw.wait.window_full");
    assert_eq!(up.hits(), 1);

    // 离第一个请求 5 分钟差 800 毫秒
    clock.jump(238_200);
    let r = ask(gw, "第三个").await;
    assert_eq!(r.status(), 200);
    assert_eq!(up.hits(), 2);
}

/// 一天一个请求：甲今天用完了，第二个请求**跳过它**、发给乙，尝试链上记着
/// `period_used_up`；两家都用完了，429 说清是哪几家，到重置之前别重试
/// （`x-should-retry: false`，没有 `retry-after-ms`）
#[tokio::test]
async fn a_used_up_day_is_skipped_and_all_used_up_is_final() {
    let (a, b) = (
        upstream(Vec::new(), ok()).await,
        upstream(Vec::new(), ok()).await,
    );
    let noon = chrono::DateTime::parse_from_rfc3339("2026-10-05T12:00:00+08:00")
        .unwrap()
        .timestamp_millis();
    let clock = Arc::new(tw_gateway::key_limits::TestClock::new(noon, 8 * 3600));
    let day = "[{per: day, requests: 1}]";
    let (gw, state, log) = serve(config(&[(a.addr, day), (b.addr, day)], 10), clock).await;

    assert_eq!(ask(gw, "第一个").await.status(), 200);
    assert_eq!((a.hits(), b.hits()), (1, 0));
    let throttle = state.throttles.view("甲").expect("used up");
    assert_eq!(throttle.kind, ThrottleKind::PeriodUsedUp);
    assert_eq!(throttle.limit, Some(0));

    let t = Instant::now();
    assert_eq!(ask(gw, "第二个").await.status(), 200);
    assert!(t.elapsed() < Duration::from_secs(1), "用完了的不该等");
    assert_eq!((a.hits(), b.hits()), (1, 1));
    let chain = log.routed("第二个").await;
    assert_eq!(chain.len(), 2, "{chain:#?}");
    assert_eq!(
        (chain[0].provider.as_str(), chain[0].skipped),
        ("甲", Some(ServeSkip::PeriodUsedUp))
    );
    assert_eq!(
        (chain[1].provider.as_str(), chain[1].outcome),
        ("乙", AttemptOutcome::Served)
    );

    let t = Instant::now();
    let r = ask(gw, "第三个").await;
    assert!(t.elapsed() < Duration::from_secs(1), "用完了的不该等");
    assert_eq!(r.status(), 429);
    assert_eq!(head(&r, "x-should-retry").as_deref(), Some("false"));
    assert_eq!(head(&r, "retry-after-ms"), None);
    // 中午到零点
    let after: u64 = head(&r, "retry-after").unwrap().parse().unwrap();
    assert!((43_190..=43_200).contains(&after), "Retry-After {after}");
    let v: serde_json::Value = r.json().await.unwrap();
    let said = v["error"]["message"].as_str().unwrap();
    assert!(said.contains("`甲`, `乙`"), "{said}");
    assert_eq!(log.failed_code("第三个").await, "gw.wait.used_up");
    assert_eq!((a.hits(), b.hits()), (1, 1));
}

/// 回了 429 的那一跳上游没接：**不算进这家的请求数上限**，预留当场还回去。一分钟一个请求的
/// 上游先回 429、暂停结束后接下：这个请求照常拿到回答，这一分钟只用了一个
#[tokio::test]
async fn a_rate_limited_hop_does_not_count_toward_the_upstreams_limit() {
    let up = upstream(vec![limited(300)], ok()).await;
    let (gw, state, log) = serve(
        config(&[(up.addr, "[{per: minute, requests: 1}]")], 10),
        Ahead::new(),
    )
    .await;
    let r = ask(gw, "一个").await;
    assert_eq!(r.status(), 200);
    assert_eq!(up.hits(), 2);
    assert_eq!(used(&state, "甲", 0), 1, "回了 429 的那一跳不算");
    let chain = log.routed("一个").await;
    assert_eq!(chain.len(), 2, "{chain:#?}");
    assert_eq!(chain[0].status, Some(429));
    assert!(chain[0].pause_ms.is_some());
    assert!(!chain[0].counts_for_upstream());
    assert!(chain[1].counts_for_upstream());
}

/// 没发出去的、回了 429 换了下一家的：甲占着的都还回去，用量上限上什么都没留下
#[tokio::test]
async fn a_reservation_is_given_back_when_the_hop_is_refused_or_never_sent() {
    // 甲回 429（暂停很久），请求换乙
    let (a, b) = (
        upstream(vec![limited(60_000)], ok()).await,
        upstream(Vec::new(), ok()).await,
    );
    let limit = "[{per: minute, requests: 1}, {per: day, tokens: 1000000}]";
    let (gw, state, _log) = serve(
        config(&[(a.addr, limit), (b.addr, limit)], 10),
        Ahead::new(),
    )
    .await;
    assert_eq!(ask(gw, "一个").await.status(), 200);
    assert_eq!((a.hits(), b.hits()), (1, 1));
    assert_eq!(used(&state, "甲", 0), 0);
    assert_eq!(used(&state, "甲", 1), 0, "token 的预留也还回去");
    assert_eq!(used(&state, "乙", 0), 1);

    // 甲连不上：一个字节都没发出去
    let b = upstream(Vec::new(), ok()).await;
    let (gw, state, _log) = serve(
        config(&[(nobody().await, limit), (b.addr, limit)], 10),
        Ahead::new(),
    )
    .await;
    assert_eq!(ask(gw, "一个").await.status(), 200);
    assert_eq!(b.hits(), 1);
    assert_eq!(used(&state, "甲", 0), 0);
    assert_eq!(used(&state, "甲", 1), 0);
    assert_eq!(used(&state, "乙", 0), 1);
}
