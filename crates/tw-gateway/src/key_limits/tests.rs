use super::*;

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
    limits: Arc<KeyLimits>,
    bus: tw_observe::EventBus,
    rx: tokio::sync::broadcast::Receiver<tw_api::Event>,
    set: Vec<KeyLimit>,
}

/// 密钥 `k` 带着这几条上限，时钟从 `now` 起
fn bed(now: &str, yaml: &str) -> Bed {
    let bus = tw_observe::EventBus::new();
    let rx = bus.subscribe();
    let limits = Arc::new(KeyLimits::with_clock(
        bus.clone(),
        Arc::new(TestClock::new(at(now), CST)),
    ));
    let set = parse(yaml);
    let cfg = tw_config::Config {
        clients: vec![tw_config::Client {
            name: "k".into(),
            key: "tw-k".into(),
            limits: set.clone(),
            ..Default::default()
        }],
        ..Default::default()
    };
    limits.configure(&cfg);
    Bed {
        limits,
        bus,
        rx,
        set,
    }
}

impl Bed {
    fn now(&self) -> i64 {
        self.limits.clock.now_ms()
    }

    /// 不等：准入，过了就交回预留
    async fn try_admit(&self, ask: Ask) -> Result<Hold, Box<Refusal>> {
        self.limits.admit("k", &self.set, ask, Duration::ZERO).await
    }

    /// 准入一个请求、给它一个号、让它「在跑」（总线上开始了）
    async fn run(&self, id: u64, ask: Ask) -> Result<(), Box<Refusal>> {
        self.limits.calendar("k", &self.set)?;
        let hold = self.try_admit(ask).await?;
        self.bus.emit(started(id));
        hold.bind(id);
        Ok(())
    }

    /// 存储层记下了这一行：请求在总线上结束，然后结算
    fn done(&self, id: u64, rec: Recorded) {
        self.bus.emit(tw_api::Event::RequestFinished {
            id,
            model: "m".into(),
            status: 200,
            bytes: 0,
            duration_ms: 0,
            usage: None,
            tokens_per_sec: None,
            answered_model: None,
        });
        self.limits.settle(id, self.now(), &rec);
    }

    fn used(&self) -> Vec<u64> {
        self.limits
            .view("k", &self.set)
            .iter()
            .map(|v| v.used)
            .collect()
    }

    fn alerts(&mut self) -> Vec<(u64, bool)> {
        let mut out = Vec::new();
        while let Ok(e) = self.rx.try_recv() {
            if let tw_api::Event::KeyLimitAlert { used, reached, .. } = e {
                out.push((used, reached));
            }
        }
        out
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
        provider: "p".into(),
        billing: tw_api::Billing::PerToken,
        model: "m".into(),
        method: "POST".into(),
        path: "/v1/messages".into(),
        input_estimate: None,
        session_log_bytes: None,
        at_ms: 0,
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

// ───────────────────────────────────────────────── 自然周期

#[tokio::test(start_paused = true)]
async fn a_day_is_used_up_then_refused_until_local_midnight() {
    let b = bed("2026-10-05T23:59:00+08:00", "[{per: day, requests: 2}]");
    b.run(1, Ask::default()).await.unwrap();
    b.run(2, Ask::default()).await.unwrap();
    // 两个都还在跑：预留已经占满
    let r = b.run(3, Ask::default()).await.unwrap_err();
    assert_eq!(r.used, 2);
    assert_eq!(r.retry_after_ms, 60_000, "到本地零点还有一分钟");
    let (end, text) = r.resets.clone().unwrap();
    assert_eq!(end, at("2026-10-06T00:00:00+08:00"));
    assert_eq!(text, "2026-10-06 00:00 +08:00");
    let m = r.msg();
    assert_eq!(m.code, "gw.key_limit.requests_per_period");
    assert_eq!(
        m.text,
        "Gateway key `k` has reached its limit of 2 requests per day: 2 so far. It resets at \
         2026-10-06 00:00 +08:00."
    );
    assert_eq!(m.arg("resets_at_ms"), end.to_string());
    // 结算之后还是两个：一个请求就是一个
    b.done(1, row(10, 10, 0, 0));
    b.done(2, row(10, 10, 0, 0));
    assert_eq!(b.used(), [2]);
    assert!(b.run(3, Ask::default()).await.is_err());
    // 过了零点，新的一天从 0 起
    tokio::time::advance(Duration::from_secs(61)).await;
    assert_eq!(b.used(), [0]);
    b.run(4, Ask::default()).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_week_starts_on_monday_and_a_month_on_the_first() {
    // 周日晚上，也是这个月的最后一天
    let b = bed(
        "2026-05-31T23:00:00+08:00",
        "[{per: week, requests: 1}, {per: month, requests: 5}]",
    );
    b.run(1, Ask::default()).await.unwrap();
    b.done(1, row(1, 1, 0, 0));
    let r = b.run(2, Ask::default()).await.unwrap_err();
    assert_eq!(r.limit.per, LimitPer::Week);
    assert_eq!(r.resets.unwrap().0, at("2026-06-01T00:00:00+08:00"));
    assert_eq!(b.used(), [1, 1]);
    // 周一零点：周和月都重新算
    tokio::time::advance(Duration::from_secs(3600)).await;
    assert_eq!(b.used(), [0, 0]);
    b.run(2, Ask::default()).await.unwrap();
    b.done(2, row(1, 1, 0, 0));
    // 周二：这一周用过了，这个月还有
    tokio::time::advance(Duration::from_secs(86_400)).await;
    assert_eq!(b.used(), [1, 1]);
    assert_eq!(
        b.run(3, Ask::default()).await.unwrap_err().limit.per,
        LimitPer::Week
    );
}

/// 天、周、月按请求开始的时刻归期：零点前开始、零点后才记下的那一行算前一天的 ——
/// 从库里加回来时也是这样算的
#[tokio::test(start_paused = true)]
async fn a_request_that_started_yesterday_counts_for_yesterday() {
    let b = bed("2026-10-05T23:59:59+08:00", "[{per: day, cost: 1}]");
    let begun = b.now();
    b.run(1, Ask::default()).await.unwrap();
    tokio::time::advance(Duration::from_secs(5)).await;
    b.limits.settle(1, begun, &row(10, 10, 0, 900_000));
    assert_eq!(b.used(), [0]);
}

// ───────────────────────────────────────────────── 滚动窗口

#[tokio::test(start_paused = true)]
async fn a_rolling_minute_waits_for_its_next_slot_or_refuses() {
    let b = bed("2026-10-05T10:00:00+08:00", "[{per: minute, requests: 2}]");
    let t0 = b.now();
    b.run(1, Ask::default()).await.unwrap();
    tokio::time::advance(Duration::from_secs(10)).await;
    b.run(2, Ask::default()).await.unwrap();
    // 第一个要到 60 秒时才滑出去：50 秒，等不了 30 秒的就拒
    let r = b
        .limits
        .admit("k", &b.set, Ask::default(), Duration::from_secs(30))
        .await
        .unwrap_err();
    assert_eq!(r.retry_after_ms, 50_000);
    assert!(r.resets.is_none());
    let m = r.msg();
    assert_eq!(m.code, "gw.key_limit.requests_rolling");
    assert_eq!(
        m.text,
        "Gateway key `k` has reached its limit of 2 requests per minute: 2 in the last minute. \
         Try again in 50 s."
    );
    assert_eq!(b.limits.clock.now_ms(), t0 + 10_000, "拒的时候不等");
    // 再过 25 秒，空位 25 秒后出来：等得到就等
    tokio::time::advance(Duration::from_secs(25)).await;
    let hold = b
        .limits
        .admit("k", &b.set, Ask::default(), Duration::from_secs(30))
        .await
        .unwrap();
    drop(hold);
    assert_eq!(b.limits.clock.now_ms(), t0 + 60_000, "等到第一个滑出窗口");
}

/// 等待期限是整个请求的那一个（见 [`slot_wait`]），调用方给的是那一刻，不是从此刻起再给
/// 一整段：期限之前空不出来的就拒
#[tokio::test(start_paused = true)]
async fn a_rolling_wait_ends_at_the_requests_own_deadline() {
    let b = bed("2026-10-05T10:00:00+08:00", "[{per: minute, requests: 1}]");
    b.run(1, Ask::default()).await.unwrap();
    let until = tokio::time::Instant::now() + Duration::from_secs(30);
    // 35 秒之后，空位还要 25 秒才出来：期限已经过了，拒
    tokio::time::advance(Duration::from_secs(35)).await;
    let r = b
        .limits
        .admit_by("k", &b.set, Ask::default(), until)
        .await
        .unwrap_err();
    assert_eq!(r.retry_after_ms, 25_000);
    // 从此刻算起的一整段（`admit`）就等得到
    let t = b.now();
    let hold = b
        .limits
        .admit("k", &b.set, Ask::default(), Duration::from_secs(30))
        .await
        .unwrap();
    drop(hold);
    assert_eq!(b.now(), t + 25_000);
}

#[tokio::test(start_paused = true)]
async fn tokens_in_a_rolling_hour_count_when_they_are_settled() {
    let b = bed("2026-10-05T10:00:00+08:00", "[{per: hour, tokens: 1000}]");
    // 估了 600 占着
    b.run(1, ask(600, 0)).await.unwrap();
    assert_eq!(b.used(), [600]);
    // 跑了 50 分钟才结束，用了 900：记在结束的那一刻
    tokio::time::advance(Duration::from_secs(50 * 60)).await;
    b.done(1, row(500, 400, 0, 0));
    assert_eq!(b.used(), [900]);
    b.run(2, ask(200, 0)).await.unwrap();
    // 这时候已经超了：要等 900 那一笔滑出去，一小时
    let r = b.try_admit(ask(1, 0)).await.unwrap_err();
    assert_eq!(r.used, 1100);
    assert_eq!(r.retry_after_ms, 3_600_000);
    assert_eq!(r.msg().code, "gw.key_limit.tokens_rolling");
}

// ───────────────────────────────────────────────── 预留与结算

#[tokio::test(start_paused = true)]
async fn a_reservation_is_replaced_by_what_was_recorded() {
    let b = bed(
        "2026-10-05T10:00:00+08:00",
        "[{per: day, tokens: 1000}, {per: day, cost: 1}]",
    );
    b.run(1, ask(800, 300_000)).await.unwrap();
    b.run(2, ask(100, 50_000)).await.unwrap();
    assert_eq!(b.used(), [900, 350_000], "在跑的按估算占着");
    // 流式的跑完了：实数比估的多
    b.done(1, row(700, 300, 0, 420_000));
    assert_eq!(b.used(), [1100, 470_000]);
    // 用满了：新的请求被拒，哪怕只要一点点
    assert!(b.run(3, ask(1, 1)).await.is_err());
    // 客户端走掉的、断在半路的：记下多少算多少
    b.done(2, row(10, 0, 0, 3_000));
    assert_eq!(b.used(), [1010, 423_000]);
}

#[tokio::test(start_paused = true)]
async fn a_hold_that_never_got_a_request_number_gives_its_share_back() {
    let b = bed("2026-10-05T10:00:00+08:00", "[{per: day, tokens: 1000}]");
    let hold = b.try_admit(ask(900, 0)).await.unwrap();
    assert_eq!(b.used(), [900]);
    drop(hold);
    assert_eq!(b.used(), [0]);
}

/// 存储层丢了那一行（总线落后）：请求结束一分钟之后，预留放掉
#[tokio::test(start_paused = true)]
async fn a_reservation_whose_row_never_comes_is_let_go_after_its_request_ended() {
    let b = bed("2026-10-05T10:00:00+08:00", "[{per: day, tokens: 1000}]");
    b.run(1, ask(900, 0)).await.unwrap();
    tokio::time::advance(Duration::from_secs(600)).await;
    assert_eq!(b.used(), [900], "还在跑的一直占着");
    b.bus.emit(tw_api::Event::RequestCancelled {
        id: 1,
        model: "m".into(),
        status: None,
        bytes: 0,
        duration_ms: 0,
        usage: None,
        answered_model: None,
    });
    assert_eq!(b.used(), [900], "结束之后先等存储层");
    tokio::time::advance(Duration::from_secs(61)).await;
    assert_eq!(b.used(), [0]);
}

#[tokio::test(start_paused = true)]
async fn what_does_not_count_does_not_count() {
    let b = bed("2026-10-05T10:00:00+08:00", "[{per: day, requests: 100}]");
    let not = |f: &dyn Fn(&mut Recorded)| {
        let mut r = row(1, 1, 0, 0);
        f(&mut r);
        r
    };
    let cases = [
        not(&|r| r.local = true),
        not(&|r| r.path = "/v1/messages/count_tokens".into()),
        not(&|r| r.path = "/v1beta/models/gemini-2.5-pro:countTokens".into()),
        not(&|r| r.path = "/v1/responses/input_tokens".into()),
        // 没发到上游的：上限自己拒的、路由拒的、内容过滤拒的、上游都满着回了 429 的
        not(&|r| r.reached = false),
    ];
    for (i, r) in cases.iter().enumerate() {
        assert!(!r.counts(), "{r:?}");
        b.limits.settle(100 + i as u64, b.now(), r);
    }
    assert_eq!(b.used(), [0]);
    // 发到了上游，失败了、或者客户端走了：算
    let r = row(0, 0, 0, 0);
    assert!(r.counts(), "{r:?}");
    b.limits.settle(200, b.now(), &r);
    assert_eq!(b.used(), [1]);
}

/// 准入过了、却没发到上游的请求（上游都满着回了 429、内容过滤拒了）：结算时把准入时记上的
/// 都还回去 —— 预留，和滚动窗口里的那一个请求。客户端照着 `Retry-After` 过几秒重试，窗口里
/// 不该还留着上一次
#[tokio::test(start_paused = true)]
async fn a_request_that_never_reached_an_upstream_gives_back_what_it_took() {
    let b = bed(
        "2026-10-05T10:00:00+08:00",
        "[{per: minute, requests: 1}, {per: day, requests: 5}, {per: day, tokens: 1000}]",
    );
    b.run(1, ask(600, 0)).await.unwrap();
    assert_eq!(b.used(), [1, 1, 600]);
    assert!(b.try_admit(Ask::default()).await.is_err(), "这一分钟用满了");
    let mut busy = row(0, 0, 0, 0);
    busy.reached = false;
    b.done(1, busy);
    assert_eq!(b.used(), [0, 0, 0]);
    // 马上就能再来
    b.run(2, ask(600, 0)).await.unwrap();
    assert_eq!(b.used(), [1, 1, 600]);
    // 开始之前就被丢掉的（准入之后出了岔子）：一样都还回去
    b.done(2, row(1, 0, 0, 0));
    tokio::time::advance(Duration::from_secs(61)).await;
    let hold = b.try_admit(ask(100, 0)).await.unwrap();
    assert_eq!(b.used(), [1, 2, 101]);
    drop(hold);
    assert_eq!(b.used(), [0, 1, 1]);
}

#[tokio::test(start_paused = true)]
async fn unpriced_and_free_cost_nothing_and_cache_reads_count_only_when_asked() {
    let b = bed(
        "2026-10-05T10:00:00+08:00",
        "[{per: day, cost: 1}, {per: day, tokens: 1000}, \
         {per: day, tokens: 1000, cache_reads: true}]",
    );
    // 没有价格、不计费：存储层记的是 None / 0，交到这里都是 0
    b.done(1, row(100, 10, 500, 0));
    b.done(2, row(100, 10, 500, 0));
    assert_eq!(b.used(), [0, 220, 1220]);
    let m = b.run(3, Ask::default()).await.unwrap_err().msg();
    assert_eq!(m.code, "gw.key_limit.tokens_per_period");
    assert_eq!(m.arg("used"), "1220");
}

#[test]
fn cost_reads_as_dollars() {
    assert_eq!(dollars(5_000_000), "$5.00");
    assert_eq!(dollars(4_200), "$0.0042");
    assert_eq!(dollars(1_234_567), "$1.234567");
    assert_eq!(dollars(0), "$0.00");
}

#[tokio::test(start_paused = true)]
async fn a_cost_refusal_names_the_amounts_in_dollars() {
    let b = bed("2026-10-05T10:00:00+08:00", "[{per: day, cost: 5}]");
    b.done(1, row(1, 1, 0, 5_030_000));
    let r = b.run(2, Ask::default()).await.unwrap_err();
    let m = r.msg();
    assert_eq!(m.code, "gw.key_limit.cost_per_period");
    assert_eq!(
        m.text,
        "Gateway key `k` has reached its limit of $5.00 per day: $5.03 spent so far. It resets \
         at 2026-10-06 00:00 +08:00."
    );
    let e = r.error();
    assert_eq!(e.source, crate::error::Source::RateLimited);
    assert_eq!(
        e.retry,
        Some(crate::error::Retry {
            after_ms: (at("2026-10-06T00:00:00+08:00") - at("2026-10-05T10:00:00+08:00")) as u64,
            until_reset: true
        })
    );
}

// ───────────────────────────────────────────────── 重启、改名

#[tokio::test(start_paused = true)]
async fn a_restart_adds_the_periods_back_from_the_store() {
    let mut b = bed(
        "2026-10-07T10:00:00+08:00",
        "[{per: day, cost: 0.12}, {per: week, cost: 10}, {per: month, cost: 10}, \
         {per: minute, requests: 1}]",
    );
    let day = at("2026-10-07T00:00:00+08:00");
    let week = at("2026-10-05T00:00:00+08:00");
    let month = at("2026-10-01T00:00:00+08:00");
    let mut asked = Vec::new();
    b.limits.rebuild(|since| {
        asked.push(since);
        // 按开始时刻：今天的、这周早些时候的、这个月早些时候的
        let mut rows = vec![row(0, 0, 0, 100_000)];
        if since <= week {
            rows.push(row(0, 0, 0, 200_000));
        }
        if since <= month {
            rows.push(row(0, 0, 0, 400_000));
        }
        // 不算的那几种，加回来时一样不算
        let mut refused = row(0, 0, 0, 999_000);
        refused.reached = false;
        rows.push(refused);
        rows
    });
    assert_eq!(asked, [day, week, month]);
    assert_eq!(
        b.used(),
        [100_000, 300_000, 700_000, 0],
        "分钟窗口从空的开始"
    );
    // 天的那一条加回来就过了八成：重启前多半报过了，不再报
    assert!(b.alerts().is_empty());
    // 再花一点就到顶了：这一档没报过，报；八成那一档不补
    b.done(1, row(0, 0, 0, 50_000));
    assert_eq!(b.alerts(), [(150_000, true)]);
}

/// 换得了时区的时钟：东八区和西五区各一只，按开关取一只。此刻是同一刻
struct Moving {
    east: TestClock,
    west: TestClock,
    moved: std::sync::atomic::AtomicBool,
}

impl Moving {
    fn now(&self) -> &TestClock {
        if self.moved.load(std::sync::atomic::Ordering::SeqCst) {
            &self.west
        } else {
            &self.east
        }
    }
}

impl Clock for Moving {
    fn now_ms(&self) -> i64 {
        self.now().now_ms()
    }
    fn period(&self, per: LimitPer, at_ms: i64) -> (i64, i64) {
        self.now().period(per, at_ms)
    }
    fn show(&self, at_ms: i64) -> String {
        self.now().show(at_ms)
    }
}

/// 一期的开头变了（机器换了时区）：去请求记录里把新的这一期重新加起来，**一期只要一次** ——
/// 读回来之前来的请求看到的都是同一个开头，不该每个都去库里查一遍。读回来的数换上去；读回来
/// 时这一期的开头又变了的，不换
#[tokio::test(start_paused = true)]
async fn a_period_whose_start_moved_is_read_back_once() {
    let now = at("2026-10-05T10:00:00+08:00");
    let clock = Arc::new(Moving {
        east: TestClock::new(now, CST),
        west: TestClock::new(now, -5 * 3600),
        moved: Default::default(),
    });
    let limits = Arc::new(KeyLimits::with_clock(
        tw_observe::EventBus::new(),
        clock.clone(),
    ));
    let set = parse("[{per: day, requests: 100}]");
    limits.configure(&tw_config::Config {
        clients: vec![tw_config::Client {
            name: "k".into(),
            key: "tw-k".into(),
            limits: set.clone(),
            ..Default::default()
        }],
        ..Default::default()
    });
    let asked: Arc<Mutex<Vec<Moved>>> = Arc::default();
    let into = asked.clone();
    limits.reread_with(Arc::new(move |m| into.lock().unwrap().push(m)));
    let used = || limits.view("k", &set)[0].used;
    for id in 1..=2 {
        limits.settle(id, now, &row(1, 1, 0, 0));
    }
    assert_eq!(used(), 2);
    assert!(asked.lock().unwrap().is_empty(), "开头没变，不读");

    clock.moved.store(true, std::sync::atomic::Ordering::SeqCst);
    let west_day = at("2026-10-04T00:00:00-05:00");
    for _ in 0..3 {
        used();
        limits.calendar("k", &set).unwrap();
    }
    limits.settle(3, now, &row(1, 1, 0, 0));
    // 天、周、月的开头都跟着时区变了：各要一次
    let moved = asked.lock().unwrap().clone();
    let starts: Vec<(LimitPer, i64)> = moved.iter().map(|m| (m.per, m.start)).collect();
    assert_eq!(
        starts,
        [
            (LimitPer::Day, west_day),
            (LimitPer::Week, at("2026-09-28T00:00:00-05:00")),
            (LimitPer::Month, at("2026-10-01T00:00:00-05:00")),
        ],
        "一期只要一次"
    );
    assert!(moved.iter().all(|m| m.key == "k"));
    // 读回来之前：从 0 起，之后结算的照记
    assert_eq!(used(), 1);
    // 读回来了：西五区的今天有五个（刚结算的那一个已经在库里），别的密钥的不算
    let mut rows: Vec<Recorded> = (0..5).map(|_| row(1, 1, 0, 0)).collect();
    let mut other = row(1, 1, 0, 0);
    other.client = "别的".into();
    rows.push(other);
    limits.reread(&moved[0], &rows);
    assert_eq!(used(), 5);
    // 读回来的时候开头又变了：那是另一期，不换
    clock
        .moved
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let before = used();
    limits.reread(&moved[0], &rows[..1]);
    assert_eq!(used(), before, "旧的那一期读回来的数换到了新的这一期上");
    // 换回来：看过的这一期（天）又是新的开头，再要一次。周、月等下一次结算碰到时再要
    assert_eq!(asked.lock().unwrap().len(), 4);
}

#[tokio::test(start_paused = true)]
async fn a_renamed_key_keeps_what_it_used() {
    let b = bed("2026-10-05T10:00:00+08:00", "[{per: day, requests: 3}]");
    b.run(1, Ask::default()).await.unwrap();
    b.done(1, row(1, 1, 0, 0));
    b.run(2, Ask::default()).await.unwrap();
    b.limits.rename("k", "k2");
    assert_eq!(b.limits.view("k2", &b.set)[0].used, 2);
    // 在跑的那一个结算到新名字上（存储层那一行记的还是旧名字）
    b.done(2, row(1, 1, 0, 0));
    assert_eq!(b.limits.view("k2", &b.set)[0].used, 2);
    assert_eq!(b.limits.view("k", &b.set)[0].used, 0);
}

// ───────────────────────────────────────────────── 提醒

#[tokio::test(start_paused = true)]
async fn eighty_percent_and_the_limit_are_each_told_once_a_period() {
    let mut b = bed("2026-10-05T23:00:00+08:00", "[{per: day, requests: 5}]");
    for id in 1..=3 {
        b.run(id, Ask::default()).await.unwrap();
        b.done(id, row(1, 1, 0, 0));
    }
    assert!(b.alerts().is_empty(), "六成不报");
    b.run(4, Ask::default()).await.unwrap();
    b.done(4, row(1, 1, 0, 0));
    assert_eq!(b.alerts(), [(4, false)]);
    b.run(5, Ask::default()).await.unwrap();
    b.done(5, row(1, 1, 0, 0));
    assert_eq!(b.alerts(), [(5, true)]);
    // 之后被拒：不再报
    assert!(b.run(6, Ask::default()).await.is_err());
    assert!(b.alerts().is_empty());
    // 下一天重新算
    tokio::time::advance(Duration::from_secs(3600)).await;
    for id in 7..=11 {
        b.run(id, Ask::default()).await.unwrap();
        b.done(id, row(1, 1, 0, 0));
    }
    assert_eq!(b.alerts(), [(4, false), (5, true)]);
}

#[tokio::test(start_paused = true)]
async fn a_refusal_from_requests_still_in_flight_is_told_too() {
    let mut b = bed("2026-10-05T10:00:00+08:00", "[{per: day, requests: 1}]");
    b.run(1, Ask::default()).await.unwrap();
    // 还没结算，靠预留拒的：到顶这一档照样报，带着拒绝时看到的数
    assert!(b.run(2, Ask::default()).await.is_err());
    assert_eq!(b.alerts(), [(1, true)]);
    b.done(1, row(1, 1, 0, 0));
    assert!(b.alerts().is_empty(), "已经报过了");
}

#[tokio::test(start_paused = true)]
async fn the_view_says_how_much_is_left_and_when_it_resets() {
    let b = bed(
        "2026-10-05T10:00:00+08:00",
        "[{per: day, cost: 2.5}, {per: minute, requests: 1}]",
    );
    b.run(1, ask(0, 0)).await.unwrap();
    b.done(1, row(1, 1, 0, 2_500_000));
    let v = b.limits.view("k", &b.set);
    assert_eq!(v[0].measure, tw_api::LimitMeasure::Cost);
    assert_eq!(
        (v[0].max, v[0].used, v[0].reached),
        (2_500_000, 2_500_000, true)
    );
    assert_eq!(
        v[0].resets_at_ms,
        Some(at("2026-10-06T00:00:00+08:00") as u64)
    );
    assert_eq!(v[1].per, tw_api::LimitPer::Minute);
    assert_eq!(
        (v[1].used, v[1].reached, v[1].resets_at_ms),
        (1, true, None)
    );
}
