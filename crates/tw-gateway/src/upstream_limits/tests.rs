use super::*;

use crate::key_limits::TestClock;
use tw_config::LimitPer;

/// 东八区。2026-10-05 是周一
const CST: i32 = 8 * 3600;

fn at(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .unwrap()
        .timestamp_millis()
}

fn parse(yaml: &str) -> Vec<KeyLimit> {
    serde_yaml_ng::from_str(yaml).unwrap()
}

struct Bed {
    limits: UpstreamLimits,
    bus: tw_observe::EventBus,
    set: Vec<KeyLimit>,
}

/// 上游 `u` 带着这几条上限（`v` 没有上限），时钟从 `now` 起
fn bed(now: &str, yaml: &str) -> Bed {
    let bus = tw_observe::EventBus::new();
    let limits = UpstreamLimits::with_clock(bus.clone(), Arc::new(TestClock::new(at(now), CST)));
    let set = parse(yaml);
    limits.configure(&config(&set));
    Bed { limits, bus, set }
}

fn config(set: &[KeyLimit]) -> tw_config::Config {
    tw_config::Config {
        providers: vec![
            tw_config::Provider {
                name: "u".into(),
                base_url: "https://u.example".into(),
                limits: set.to_vec(),
                ..Default::default()
            },
            tw_config::Provider {
                name: "v".into(),
                base_url: "https://v.example".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}

impl Bed {
    fn now(&self) -> i64 {
        self.limits.now()
    }

    fn state(&self) -> LimitState {
        self.limits.state("u", &self.set)
    }

    /// 一跳发往 `u`，上游接下了：交给请求号
    fn send(&self, id: u64, ask: Ask) {
        self.bus.emit(started(id));
        self.limits.reserve("u", &self.set, &ask).bind(id);
    }

    /// 存储层记下了这一行：请求在总线上结束，然后结算。`hops` 是算数的那几跳
    fn done(&self, id: u64, hops: &[&str], served: Option<&str>, rec: Recorded) {
        self.bus.emit(finished(id));
        let hops: Vec<String> = hops.iter().map(|h| h.to_string()).collect();
        self.limits.settle(id, self.now(), &hops, served, &rec);
    }

    fn used(&self) -> Vec<u64> {
        self.limits
            .view("u", &self.set)
            .iter()
            .map(|v| v.used)
            .collect()
    }
}

fn started(id: u64) -> tw_api::Event {
    tw_api::Event::RequestStarted {
        id,
        client: "k".into(),
        client_hint: None,
        session: None,
        peer: None,
        key_masked: None,
        route: "default".into(),
        rule: "r".into(),
        group: None,
        rewritten_by: vec![],
        provider: "u".into(),
        billing: tw_api::Billing::PerToken,
        model: "m".into(),
        method: "POST".into(),
        path: "/v1/messages".into(),
        input_estimate: None,
        session_log_bytes: None,
        at_ms: 0,
    }
}

fn finished(id: u64) -> tw_api::Event {
    tw_api::Event::RequestFinished {
        id,
        model: "m".into(),
        status: 200,
        sent_bytes: 0,
        received_bytes: 0,
        duration_ms: 0,
        usage: None,
        tokens_per_sec: None,
        answered_model: None,
    }
}

/// 记下的一行：发往了上游、成功了
fn row(input: u64, output: u64, cache_read: u64, cost: i64) -> Recorded {
    Recorded {
        client: "k".into(),
        path: "/v1/messages".into(),
        local: false,
        reached: true,
        requests: 1,
        input,
        output,
        cache_read,
        cache_write: 0,
        cost_micros: cost,
    }
}

fn ask(tokens: u64, cost: i64) -> Ask {
    Ask {
        tokens,
        cost_micros: cost,
    }
}

// ───────────────────────────────────────────────── 滚动窗口

#[tokio::test(start_paused = true)]
async fn a_full_window_says_when_it_frees_and_frees_then() {
    let b = bed(
        "2026-10-05T10:00:00+08:00",
        "[{per: minute, every: 5, requests: 2}]",
    );
    let t0 = b.now();
    assert_eq!(b.state(), LimitState::Free);
    b.send(1, Ask::default());
    tokio::time::advance(Duration::from_secs(60)).await;
    b.send(2, Ask::default());
    // 两个都发出去了：窗口满了，第一个在 5 分钟时滑出去
    assert_eq!(
        b.state(),
        LimitState::WindowFull {
            until_ms: (t0 + 300_000) as u64,
            limit: 0
        }
    );
    // 结算不改请求数：发出去就算
    b.done(1, &["u"], Some("u"), row(10, 10, 0, 0));
    b.done(2, &["u"], Some("u"), row(10, 10, 0, 0));
    assert_eq!(b.used(), [2]);
    assert!(matches!(b.state(), LimitState::WindowFull { .. }));
    tokio::time::advance(Duration::from_secs(240)).await;
    assert_eq!(b.state(), LimitState::Free, "第一个滑出去了");
    assert_eq!(b.used(), [1]);
}

#[tokio::test(start_paused = true)]
async fn tokens_count_for_the_upstream_that_answered_when_settled() {
    let b = bed("2026-10-05T10:00:00+08:00", "[{per: hour, tokens: 1000}]");
    // 估了 600 占着
    b.send(1, ask(600, 0));
    assert_eq!(b.used(), [600]);
    assert_eq!(b.state(), LimitState::Free);
    // 跑了 50 分钟才结束，用了 900：记在结束的那一刻
    tokio::time::advance(Duration::from_secs(50 * 60)).await;
    let settled = b.now();
    b.done(1, &["u"], Some("u"), row(500, 400, 0, 0));
    assert_eq!(b.used(), [900]);
    b.send(2, ask(200, 0));
    assert_eq!(
        b.state(),
        LimitState::WindowFull {
            until_ms: (settled + 3_600_000) as u64,
            limit: 0
        },
        "900 那一笔滑出去才空"
    );
    // 用量记在接下请求的那一家：别的跳只算请求数
    b.done(2, &["v", "u"], Some("v"), row(5_000, 5_000, 0, 0));
    assert_eq!(b.used(), [900]);
}

// ───────────────────────────────────────────────── 自然周期

#[tokio::test(start_paused = true)]
async fn a_used_up_day_is_skipped_until_local_midnight() {
    let b = bed(
        "2026-10-05T23:59:00+08:00",
        "[{per: minute, requests: 1}, {per: day, cost: 1}]",
    );
    b.send(1, ask(0, 400_000));
    b.done(1, &["u"], Some("u"), row(1, 1, 0, 1_200_000));
    // 天和分钟都满了：天的先报，等不到
    assert_eq!(
        b.state(),
        LimitState::PeriodUsedUp {
            until_ms: at("2026-10-06T00:00:00+08:00") as u64,
            limit: 1
        }
    );
    let v = b.limits.view("u", &b.set);
    assert_eq!(
        (v[1].used, v[1].reached, v[1].resets_at_ms),
        (
            1_200_000,
            true,
            Some(at("2026-10-06T00:00:00+08:00") as u64)
        )
    );
    assert_eq!((v[0].every, v[0].resets_at_ms), (1, None));
    // 过了零点，新的一天从 0 起；分钟那条也滑出去了
    tokio::time::advance(Duration::from_secs(61)).await;
    assert_eq!(b.state(), LimitState::Free);
    assert_eq!(b.used(), [0, 0]);
}

#[tokio::test(start_paused = true)]
async fn requests_in_flight_fill_a_period_before_they_are_settled() {
    let b = bed("2026-10-05T10:00:00+08:00", "[{per: week, requests: 2}]");
    b.send(1, Ask::default());
    b.send(2, Ask::default());
    assert_eq!(
        b.state(),
        LimitState::PeriodUsedUp {
            until_ms: at("2026-10-12T00:00:00+08:00") as u64,
            limit: 0
        },
        "下周一零点"
    );
    b.done(1, &["u"], Some("u"), row(1, 1, 0, 0));
    b.done(2, &["u"], Some("u"), row(1, 1, 0, 0));
    assert_eq!(b.used(), [2]);
}

// ───────────────────────────────────────────────── 预留、还回、结算

#[tokio::test(start_paused = true)]
async fn a_hop_answered_429_or_never_sent_gives_everything_back() {
    let b = bed(
        "2026-10-05T10:00:00+08:00",
        "[{per: minute, requests: 1}, {per: day, tokens: 100}]",
    );
    let hold = b.limits.reserve("u", &b.set, &ask(80, 0));
    assert_eq!(b.used(), [1, 80]);
    assert!(matches!(b.state(), LimitState::WindowFull { .. }));
    hold.release();
    assert_eq!(b.used(), [0, 0]);
    assert_eq!(b.state(), LimitState::Free);
    // 丢掉也一样
    drop(b.limits.reserve("u", &b.set, &ask(80, 0)));
    assert_eq!(b.used(), [0, 0]);
    // 没设上限的那家什么都不记
    let none = b.limits.reserve("v", &[], &ask(80, 0));
    assert!(format!("{none:?}").contains("false"));
}

#[tokio::test(start_paused = true)]
async fn the_record_decides_which_hops_count() {
    let b = bed(
        "2026-10-05T10:00:00+08:00",
        "[{per: minute, requests: 5}, {per: day, requests: 5}]",
    );
    // 一个请求发了三跳到 u：头一跳回了 429 由管线还回去了，第二跳超时（算），第三跳接下了
    b.bus.emit(started(1));
    b.limits.reserve("u", &b.set, &Ask::default()).release();
    b.limits.reserve("u", &b.set, &Ask::default()).bind(1);
    b.limits.reserve("u", &b.set, &Ask::default()).bind(1);
    assert_eq!(b.used(), [2, 2]);
    b.done(1, &["u", "u"], Some("u"), row(1, 1, 0, 0));
    assert_eq!(b.used(), [2, 2], "两跳各算一个");
    // 记录里没有的那一跳把窗口里记上的也还回去
    b.send(2, Ask::default());
    b.send(2, Ask::default());
    b.done(2, &["u"], Some("u"), row(1, 1, 0, 0));
    assert_eq!(b.used(), [3, 3]);
    // 数 token 的请求、网关自己答的不算
    b.send(3, Ask::default());
    let mut count = row(0, 0, 0, 0);
    count.path = "/v1/messages/count_tokens".into();
    b.done(3, &["u"], Some("u"), count);
    assert_eq!(b.used(), [3, 3]);
}

#[tokio::test(start_paused = true)]
async fn a_reservation_whose_row_never_comes_is_let_go_after_its_request_ended() {
    let b = bed("2026-10-05T10:00:00+08:00", "[{per: day, tokens: 100}]");
    b.send(1, ask(80, 0));
    b.bus.emit(finished(1));
    assert_eq!(b.used(), [80], "刚结束：再等存储层一会儿");
    tokio::time::advance(Duration::from_millis(books::GRACE_MS as u64)).await;
    assert_eq!(b.used(), [0]);
}

#[tokio::test(start_paused = true)]
async fn cache_reads_count_only_when_asked_and_cost_is_what_was_recorded() {
    let b = bed(
        "2026-10-05T10:00:00+08:00",
        "[{per: day, tokens: 1000000}, {per: day, tokens: 1000000, cache_reads: true}, \
         {per: hour, every: 2, cost: 5}]",
    );
    b.send(1, ask(0, 0));
    b.done(1, &["u"], Some("u"), row(100, 50, 10_000, 30_000));
    assert_eq!(b.used(), [150, 10_150, 30_000]);
}

// ───────────────────────────────────────────────── 重启、换期、改名

#[tokio::test(start_paused = true)]
async fn a_restart_adds_the_periods_back_from_the_store() {
    let b = bed(
        "2026-10-07T12:00:00+08:00",
        "[{per: day, requests: 10}, {per: week, tokens: 10000}, {per: month, cost: 50}]",
    );
    let day = at("2026-10-07T00:00:00+08:00");
    let week = at("2026-10-05T00:00:00+08:00");
    let month = at("2026-10-01T00:00:00+08:00");
    let mut asked = Vec::new();
    b.limits.rebuild(|since| {
        asked.push(since);
        // 今天：u 3 跳、用量 100；这周早些时候再加 2 跳、用量 200；这个月再早些再加 1 跳、
        // $2。v 的不算在 u 头上；数 token 的不算
        let mut rows = vec![(
            "u".to_string(),
            Recorded {
                requests: 3,
                ..row(50, 50, 0, 1_000_000)
            },
        )];
        if since <= week {
            rows.push((
                "u".to_string(),
                Recorded {
                    requests: 2,
                    ..row(100, 100, 0, 0)
                },
            ));
        }
        if since <= month {
            rows.push(("u".to_string(), row(0, 0, 0, 2_000_000)));
        }
        rows.push(("v".to_string(), row(9_000, 9_000, 0, 9_000_000)));
        let mut count = row(5_000, 0, 0, 0);
        count.path = "/v1/messages/count_tokens".into();
        rows.push(("u".to_string(), count));
        rows
    });
    assert_eq!(asked, [day, week, month]);
    assert_eq!(b.used(), [3, 300, 3_000_000]);
    assert_eq!(b.state(), LimitState::Free);
    // 加回来的之上照常记
    b.send(1, Ask::default());
    b.done(1, &["u"], Some("u"), row(10, 10, 0, 0));
    assert_eq!(b.used(), [4, 320, 3_000_000]);
}

#[tokio::test(start_paused = true)]
async fn a_period_whose_start_moved_is_read_back_once() {
    let b = bed("2026-10-05T23:59:30+08:00", "[{per: day, requests: 5}]");
    let asked: Arc<Mutex<Vec<Moved>>> = Arc::default();
    let log = asked.clone();
    b.limits
        .reread_with(Arc::new(move |m| log.lock().unwrap().push(m)));
    b.send(1, Ask::default());
    b.done(1, &["u"], Some("u"), row(1, 1, 0, 0));
    assert_eq!(b.used(), [1]);
    // 过了零点：换新的一天，要一次读回
    tokio::time::advance(Duration::from_secs(60)).await;
    assert_eq!(b.used(), [0]);
    assert_eq!(b.used(), [0]);
    let m = {
        let got = asked.lock().unwrap();
        assert_eq!(got.len(), 1, "一期只要一次");
        got[0].clone()
    };
    assert_eq!(
        (m.key.as_str(), m.per, m.start),
        ("u", LimitPer::Day, at("2026-10-06T00:00:00+08:00"))
    );
    b.limits.reread(
        &m,
        &[
            (
                "u".to_string(),
                Recorded {
                    requests: 4,
                    ..row(1, 1, 0, 0)
                },
            ),
            ("v".to_string(), row(1, 1, 0, 0)),
        ],
    );
    assert_eq!(b.used(), [4]);
}

#[tokio::test(start_paused = true)]
async fn a_renamed_upstream_keeps_what_it_used() {
    let b = bed("2026-10-05T10:00:00+08:00", "[{per: day, requests: 3}]");
    b.send(1, Ask::default());
    b.done(1, &["u"], Some("u"), row(1, 1, 0, 0));
    b.send(2, Ask::default());
    b.limits.rename("u", "u2");
    assert_eq!(b.limits.view("u2", &b.set)[0].used, 2);
    assert_eq!(b.limits.view("u", &b.set)[0].used, 0);
}

// ───────────────────────────────────────────────── 等它变化

/// 叫没叫醒：给 `changed` 一点时间，看它返回了没有
async fn woke(f: impl std::future::Future<Output = ()>, within: Duration) -> bool {
    tokio::time::timeout(within, f).await.is_ok()
}

#[tokio::test(start_paused = true)]
async fn changed_wakes_when_a_reservation_comes_back() {
    let b = bed("2026-10-05T10:00:00+08:00", "[{per: day, requests: 1}]");
    let hold = b.limits.reserve("u", &b.set, &Ask::default());
    assert!(matches!(b.state(), LimitState::PeriodUsedUp { .. }));
    let limits = &b.limits;
    let wait = limits.changed("u");
    tokio::pin!(wait);
    // 还没变：一秒之内不醒
    assert!(!woke(wait.as_mut(), Duration::from_secs(1)).await);
    hold.release();
    assert!(woke(wait, Duration::from_millis(1)).await);
    assert_eq!(b.state(), LimitState::Free);
}

#[tokio::test(start_paused = true)]
async fn changed_wakes_when_the_window_slides() {
    let b = bed("2026-10-05T10:00:00+08:00", "[{per: minute, requests: 1}]");
    b.send(1, Ask::default());
    let t0 = b.now();
    tokio::time::advance(Duration::from_secs(20)).await;
    // 40 秒之后滑出去：到点就醒，不用谁叫
    b.limits.changed("u").await;
    assert_eq!(b.now(), t0 + 60_000);
    assert_eq!(b.state(), LimitState::Free);
}

#[tokio::test(start_paused = true)]
async fn changed_wakes_when_a_settle_or_new_limits_arrive() {
    let b = bed("2026-10-05T10:00:00+08:00", "[{per: hour, tokens: 100}]");
    b.send(1, ask(500, 0));
    assert!(matches!(b.state(), LimitState::WindowFull { .. }));
    let wait = b.limits.changed("u");
    tokio::pin!(wait);
    assert!(!woke(wait.as_mut(), Duration::from_secs(1)).await);
    // 结算：估多了，实际只用了 10
    b.done(1, &["u"], Some("u"), row(5, 5, 0, 0));
    assert!(woke(wait, Duration::from_millis(1)).await);
    assert_eq!(b.state(), LimitState::Free);
    // 配置换了也叫醒：上限可能松了
    b.send(2, ask(500, 0));
    let wait = b.limits.changed("u");
    tokio::pin!(wait);
    assert!(!woke(wait.as_mut(), Duration::from_secs(1)).await);
    b.limits
        .configure(&config(&parse("[{per: hour, tokens: 100000}]")));
    assert!(woke(wait, Duration::from_millis(1)).await);
}
