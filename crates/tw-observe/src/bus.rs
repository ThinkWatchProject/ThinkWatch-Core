//! 事件总线。数据面往里丢，控制面的订阅者往外拿。
//!
//! 用 broadcast 而不是 mpsc，因为可能有多个订阅者（UI 开着、CLI 也在
//! tail）。容量有上限且**满了丢最老的**：观测数据丢几条不影响任何人，
//! 而让数据面因为没人读事件而阻塞是不可接受的 —— **观测的失败不能
//! 拖垮数据面**。
//!
//! 注意这条豁免**不适用于成本记账**：那类数据丢了账单永久对不
//! 上，必须走别的路径。这里只走「丢了只是图上少个点」的东西。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

const CAPACITY: usize = 1024;

/// 生成速率看最近这么久里跑完的请求
pub const RATE_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<tw_api::Event>,
    next_id: Arc<AtomicU64>,
    /// 在跑的和最近跑完的。见 [`EventBus::in_flight`]、[`EventBus::live`]
    tally: Arc<Mutex<Tally>>,
}

/// 按事件数出来的现状。
#[derive(Default)]
struct Tally {
    /// 开始了、还没有结局的请求，按 id
    open: BTreeMap<u64, Open>,
    /// 在跑的请求首字节用了多久，毫秒。生成用时要从总耗时里减掉它
    ttfb: HashMap<u64, u64>,
    /// 最近跑完的：（什么时候结束的，输出了多少 token，生成用了多少毫秒）
    done: VecDeque<(Instant, u64, u64)>,
}

/// 一个还没有结局的请求。
struct Open {
    /// 开始事件记下的那一刻。**跑了多久按它算**：单调时钟，不怕系统时间被拨
    since: Instant,
    /// 到目前为止关于它的事件，按发生的先后，第一条是开始事件（见 [`about_the_request`]）
    events: Vec<tw_api::Event>,
}

/// 这条事件说的是它挂着的那个请求本身吗 —— 是的话，半路才来的一方要重建那个请求，
/// 就少不了它。
///
/// **一个一个列出来，不写通配**：新加一种事件，编译器会逼着人在这里想一遍它算
/// 不算。挂着请求 id、说的却是别的事的不算：`QuotaSeen` 说的是那家上游现在还剩
/// 多少额度（`/quota` 答得了），`RequestPriced` 在结局之后才到。
fn about_the_request(ev: &tw_api::Event) -> bool {
    use tw_api::Event as E;
    match ev {
        E::RequestHeaders { .. }
        | E::RequestRouted { .. }
        | E::Translated { .. }
        | E::SecretsFound { .. }
        | E::HiddenTextFound { .. }
        | E::ContentMatched { .. }
        | E::OutputLimited { .. }
        | E::ToolCallFlagged { .. } => true,
        // 开始和三种结局由 `track_at` 自己管
        E::RequestStarted { .. }
        | E::RequestFinished { .. }
        | E::RequestFailed { .. }
        | E::RequestCancelled { .. }
        // 挂着请求的号、说的却是别的事的，和根本不挂在请求上的
        | E::RequestPriced { .. }
        | E::QuotaSeen { .. }
        | E::QuotaExhausted { .. }
        | E::LocallyAnswered { .. }
        | E::CredentialRotated { .. }
        | E::CredentialExpired { .. }
        | E::LoginFinished { .. }
        | E::HealthChanged { .. }
        | E::ModelsChanged { .. }
        | E::ProxyChanged { .. }
        | E::AuthChanged { .. }
        | E::ListenChanged { .. }
        | E::ConfigReloaded { .. }
        | E::ConfigRejected { .. }
        | E::EventsDropped { .. } => false,
    }
}

/// 此刻的系统时间，Unix 毫秒。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Tally {
    fn forget(&mut self, now: Instant) {
        while self
            .done
            .front()
            .is_some_and(|(at, _, _)| now.saturating_duration_since(*at) > RATE_WINDOW)
        {
            self.done.pop_front();
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CAPACITY);
        Self {
            tx,
            next_id: Arc::new(AtomicU64::new(1)),
            tally: Arc::default(),
        }
    }

    /// 拿一个请求 id。同一个请求的四个事件共用它。
    /// 现在发到第几号了，**不占号**。
    ///
    /// 给 `load-balance` 当轮转的种子用：它要一个单调、便宜、
    /// 每个请求都不同的数，而事件序号正好是。**不能用 `next_id`** ——
    /// 那会凭空占掉一个号，让事件流里出现一个不存在的 id。
    pub fn peek_id(&self) -> u64 {
        self.next_id.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// 从这个号之后接着发。
    ///
    /// **计数器出厂从 1 开始，而库是跨重启的。**存储层落库用的是
    /// `INSERT OR REPLACE`（同一个请求要写两次），所以重启后重新发出
    /// 的号会一条一条顶掉历史上的同号记录。启动时把已经用掉的最大号
    /// 交给它，那条路就断了。
    ///
    /// **只进不退**：同一个进程里被调用两次，或者调用时已经发过号了，
    /// 都不会把计数器往回拨 —— 往回拨就是在制造重号。
    pub fn resume_after(&self, id: u64) {
        self.next_id.fetch_max(id + 1, Ordering::Relaxed);
    }

    /// 发一条。**没有订阅者不是错误** —— 没开 UI 的时候数据面照常跑。
    ///
    /// **先记账再发**（见 [`EventBus::in_flight`]）：订阅者收到一个结局的
    /// 时候，快照里已经没有它了。
    pub fn emit(&self, ev: tw_api::Event) {
        self.track(&ev);
        let _ = self.tx.send(ev);
    }

    /// 开始的记下，有了结局的划掉。
    ///
    /// **这里什么都不能 panic。**`Ending` 在 Drop 里也会走到这儿，那时可能
    /// 正处在一次 unwind 里 —— 再 panic 一次，整个进程就没了。锁中毒了也照样
    /// 拿里面的表：少记一笔的快照，好过一个没了的网关。
    fn track(&self, ev: &tw_api::Event) {
        self.track_at(ev, Instant::now());
    }

    fn track_at(&self, ev: &tw_api::Event, now: Instant) {
        use tw_api::Event as E;
        let mut t = self.tally.lock().unwrap_or_else(|p| p.into_inner());
        match ev {
            E::RequestStarted { id, .. } => {
                t.open.insert(
                    *id,
                    Open {
                        since: now,
                        events: vec![ev.clone()],
                    },
                );
            }
            E::RequestHeaders { id, ttfb_ms, .. } => {
                if let Some(o) = t.open.get_mut(id) {
                    o.events.push(ev.clone());
                    t.ttfb.insert(*id, *ttfb_ms);
                }
            }
            E::RequestFinished {
                id,
                duration_ms,
                usage,
                ..
            } => {
                t.open.remove(id);
                let ttfb = t.ttfb.remove(id);
                // **只数跑完了的。**取消的、断掉的输出不全，拿来算速率只会偏低；
                // 没有响应头的（WebSocket）不知道生成用了多久，不瞎算
                if let (Some(u), Some(ttfb)) = (usage, ttfb) {
                    let gen_ms = duration_ms.saturating_sub(ttfb);
                    if u.output > 0 && gen_ms > 0 {
                        t.done.push_back((now, u.output, gen_ms));
                    }
                }
                t.forget(now);
            }
            E::RequestFailed { id, .. } | E::RequestCancelled { id, .. } => {
                t.open.remove(id);
                t.ttfb.remove(id);
            }
            // 别的事件挂着的 id 要么是一个请求的，要么是它自己取的号 —— 号从同一个
            // 计数器里取，不会和一个在跑的请求撞上
            other if about_the_request(other) => {
                if let Some(o) = t.open.get_mut(&other.id()) {
                    o.events.push(other.clone());
                }
            }
            _ => {}
        }
    }

    /// 此刻还在跑的请求：每个到目前为止的事件，**原样**、按发生的先后；请求按 id
    /// 从小到大（也就是开始的先后）。连同 core 此刻的时钟。
    ///
    /// 给**半路才来听的一方**用。事件流只送订阅之后发生的事，一个在那之前
    /// 就开始、此刻还没结束的请求，它的开始事件、响应头、路由早就发过了 ——
    /// 只补开始的话，那一行就一直没有状态码、画不出它最后走到了哪家上游。把这些
    /// 事件当成补发的处理，就和从头听起一样。
    ///
    /// **和 `Status::in_flight` 不是一个数。**那个从连接进到数据面就算，比
    /// 开始事件早，排队等名额的、鉴权没过的都在里面 —— 它回答的是「现在
    /// 重启会掐断几个连接」。这里只有发过开始事件的请求，和事件流里说的是
    /// 同一批。
    ///
    /// 表不会只进不出：每个开始事件都欠着恰好一个结局，由 `Ending` 保证。
    pub fn in_flight(&self) -> tw_api::InFlight {
        let t = self.tally.lock().unwrap_or_else(|p| p.into_inner());
        tw_api::InFlight {
            now_ms: now_ms(),
            requests: t
                .open
                .iter()
                .map(|(id, o)| tw_api::InFlightRequest {
                    id: *id,
                    events: o.events.clone(),
                })
                .collect(),
        }
    }

    /// 此刻的实时读数：在跑的请求（和 [`EventBus::in_flight`] 同一批），和最近
    /// 一分钟跑完的请求平均每秒生成多少 token。
    ///
    /// **量的是生成的快慢**：每个请求的输出除以它生成用的时间（总耗时减去首字节），
    /// 按 token 加权。拿「一分钟里输出了多少」去除以六十的话，一个跑了一分钟才
    /// 结束的长请求，要等它结束之后才把速率一次性摊进来，数字跟着请求的长短忽高
    /// 忽低。
    pub fn live(&self) -> tw_api::LiveView {
        self.live_at(Instant::now())
    }

    fn live_at(&self, now: Instant) -> tw_api::LiveView {
        let mut t = self.tally.lock().unwrap_or_else(|p| p.into_inner());
        t.forget(now);
        let (tokens, ms) = t
            .done
            .iter()
            .fold((0u64, 0u64), |(tok, ms), (_, out, gen_ms)| {
                (tok + out, ms + gen_ms)
            });
        let mut running: Vec<tw_api::RunningView> =
            t.open.values().filter_map(|o| running(o, now)).collect();
        running.sort_by_key(|r| (r.at_ms, r.id));
        tw_api::LiveView {
            running,
            tokens_per_sec: (ms > 0)
                .then(|| (tokens.saturating_mul(1000) / ms).min(u32::MAX as u64) as u32),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<tw_api::Event> {
        self.tx.subscribe()
    }

    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

/// 一个在跑的请求此刻的样子：开始事件里的，加上路由报出来的那家上游。
fn running(o: &Open, now: Instant) -> Option<tw_api::RunningView> {
    let tw_api::Event::RequestStarted {
        id,
        client,
        client_hint,
        session,
        route,
        rule,
        group,
        model,
        provider,
        at_ms,
        ..
    } = o.events.first()?
    else {
        return None;
    };
    // 接下它的是尝试链里 `served` 的那一跳。路由事件到之前没有
    let upstream = o.events.iter().find_map(|e| match e {
        tw_api::Event::RequestRouted { attempts, .. } => attempts
            .iter()
            .find(|a| a.outcome == tw_api::AttemptOutcome::Served)
            .map(|a| a.provider.clone()),
        _ => None,
    });
    Some(tw_api::RunningView {
        id: *id,
        client: client.clone(),
        client_hint: client_hint.clone(),
        model: model.clone(),
        provider: provider.clone(),
        at_ms: *at_ms,
        elapsed_ms: now.saturating_duration_since(o.since).as_millis() as u64,
        session: session.clone(),
        route: route.clone(),
        rule: rule.clone(),
        group: group.clone(),
        upstream,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(id: u64) -> tw_api::Event {
        tw_api::Event::RequestStarted {
            key_masked: None,
            peer: None,
            id,
            client: "c".into(),
            client_hint: None,
            session: None,
            route: "default".into(),
            rule: "catch-all".into(),
            group: Some("__all__".into()),
            rewritten_by: vec![],
            provider: "p".into(),
            billing: tw_api::Billing::PerToken,
            model: "m".into(),
            method: "POST".into(),
            path: "/v1/messages".into(),
            at_ms: 1_000 + id,
        }
    }

    /// 快照里是**开始了、还没有结局**的那些。三种结局都算结局，响应头不算。
    #[test]
    fn in_flight_is_what_started_and_has_not_ended() {
        let b = EventBus::new();
        for id in 1..=4 {
            b.emit(started(id));
        }
        b.emit(tw_api::Event::RequestFinished {
            id: 1,
            model: "m".into(),
            status: 200,
            bytes: 0,
            duration_ms: 0,
            usage: None,
        });
        b.emit(tw_api::Event::RequestFailed {
            id: 2,
            model: "m".into(),
            source: tw_api::FailureSource::Upstream,
            message: tw_api::Msg {
                code: "t.x".into(),
                args: Default::default(),
                text: "x".into(),
            },
            bytes: None,
            duration_ms: None,
            usage: None,
        });
        b.emit(tw_api::Event::RequestCancelled {
            id: 3,
            model: "m".into(),
            status: None,
            bytes: 0,
            duration_ms: 0,
            usage: None,
        });
        b.emit(tw_api::Event::RequestHeaders {
            id: 4,
            status: 200,
            ttfb_ms: 5,
        });

        let open = b.in_flight().requests;
        assert_eq!(open.iter().map(|r| r.id).collect::<Vec<_>>(), [4]);
        // **原样**：听的人拿它当补发的事件，字段一个都不能少
        assert!(
            matches!(&open[0].events[0], tw_api::Event::RequestStarted { model, at_ms: 1004, .. } if model == "m"),
            "{open:?}"
        );
    }

    fn routed(id: u64, served_by: &str) -> tw_api::Event {
        tw_api::Event::RequestRouted {
            id,
            route: "default".into(),
            rule: "catch-all".into(),
            group: Some("__all__".into()),
            rewritten_by: vec![],
            denied_by: None,
            attempts: vec![
                tw_api::AttemptView {
                    provider: "p".into(),
                    outcome: tw_api::AttemptOutcome::Status,
                    status: Some(503),
                    error: None,
                    ms: 10,
                },
                tw_api::AttemptView {
                    provider: served_by.into(),
                    outcome: tw_api::AttemptOutcome::Served,
                    status: Some(200),
                    error: None,
                    ms: 20,
                },
            ],
            billing: tw_api::Billing::PerToken,
        }
    }

    /// 半路才来的一方要的是**每个在跑的请求到目前为止的全部**：开始、响应头、路由、
    /// 防护的记录，按发生的先后 —— 只给开始的话，那一行没有状态码、画不到上游。
    /// 说上游现状的额度不跟着请求走；结束了的整条不在
    #[test]
    fn in_flight_replays_everything_a_running_request_has_seen_so_far() {
        let b = EventBus::new();
        b.emit(started(1));
        b.emit(started(2));
        b.emit(headers(1, 30));
        b.emit(tw_api::Event::QuotaSeen {
            id: 1,
            provider: "p".into(),
            windows: vec![],
            at_ms: 0,
        });
        b.emit(routed(1, "q"));
        b.emit(tw_api::Event::Translated {
            id: 1,
            provider: "q".into(),
            from: tw_api::Dialect::Anthropic,
            to: tw_api::Dialect::OpenaiResponses,
            dropped: vec![],
            at_ms: 0,
        });
        // 自己取号的事件不会挂到哪个请求上
        let own = b.next_id();
        b.emit(tw_api::Event::HealthChanged {
            id: own,
            provider: "p".into(),
            state: tw_api::BreakerState::Open,
            at_ms: 0,
        });
        b.emit(finished(2, 10, 1));

        let snap = b.in_flight();
        assert!(
            snap.now_ms > 1_700_000_000_000,
            "core 的时钟：{}",
            snap.now_ms
        );
        assert_eq!(snap.requests.len(), 1, "{snap:?}");
        let kinds: Vec<&str> = snap.requests[0]
            .events
            .iter()
            .map(|e| match e {
                tw_api::Event::RequestStarted { .. } => "started",
                tw_api::Event::RequestHeaders { .. } => "headers",
                tw_api::Event::RequestRouted { .. } => "routed",
                tw_api::Event::Translated { .. } => "translated",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["started", "headers", "routed", "translated"]);
    }

    /// 没有订阅者的时候照样记 —— 这份快照就是给还没来的订阅者准备的。
    /// 按 id 排，也就是按开始的先后。
    #[test]
    fn in_flight_is_kept_with_no_subscriber_and_in_start_order() {
        let b = EventBus::new();
        assert!(b.in_flight().requests.is_empty());
        for id in [7, 3, 5] {
            b.emit(started(id));
        }
        assert_eq!(b.subscriber_count(), 0);
        assert_eq!(
            b.in_flight()
                .requests
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            [3, 5, 7]
        );
    }

    fn headers(id: u64, ttfb_ms: u64) -> tw_api::Event {
        tw_api::Event::RequestHeaders {
            id,
            status: 200,
            ttfb_ms,
        }
    }

    fn finished(id: u64, duration_ms: u64, output: u64) -> tw_api::Event {
        tw_api::Event::RequestFinished {
            id,
            model: "m".into(),
            status: 200,
            bytes: 1,
            duration_ms,
            usage: Some(tw_api::UsageView {
                input: 10,
                output,
                ..Default::default()
            }),
        }
    }

    /// **量的是生成的快慢**：首字节之前那一段不算，几个请求按 token 加权。
    #[test]
    fn the_rate_is_output_over_generation_time() {
        let b = EventBus::new();
        let now = Instant::now();
        // 首字节 1 秒，之后 2 秒生成了 100 个
        b.track_at(&started(1), now);
        b.track_at(&headers(1, 1_000), now);
        b.track_at(&finished(1, 3_000, 100), now);
        assert_eq!(b.live_at(now).tokens_per_sec, Some(50));
        // 又一个：1 秒生成了 50 个。合起来 150 个 / 3 秒
        b.track_at(&started(2), now);
        b.track_at(&headers(2, 500), now);
        b.track_at(&finished(2, 1_500, 50), now);
        assert_eq!(b.live_at(now).tokens_per_sec, Some(50));
    }

    /// 一分钟之前跑完的不算；**没有就是没有**，不是 0
    #[test]
    fn an_idle_minute_has_no_rate() {
        let b = EventBus::new();
        let then = Instant::now();
        assert_eq!(b.live_at(then).tokens_per_sec, None);
        b.track_at(&started(1), then);
        b.track_at(&headers(1, 100), then);
        b.track_at(&finished(1, 1_100, 40), then);
        assert_eq!(b.live_at(then).tokens_per_sec, Some(40));
        let later = then + RATE_WINDOW + Duration::from_secs(1);
        assert_eq!(b.live_at(later).tokens_per_sec, None);
    }

    /// 没有响应头的（WebSocket）、失败和取消的，都不拿来算速率
    #[test]
    fn only_finished_requests_with_their_headers_make_a_rate() {
        let b = EventBus::new();
        let now = Instant::now();
        b.track_at(&started(1), now);
        b.track_at(&finished(1, 2_000, 100), now);
        b.track_at(&started(2), now);
        b.track_at(&headers(2, 100), now);
        b.track_at(
            &tw_api::Event::RequestCancelled {
                id: 2,
                model: "m".into(),
                status: Some(200),
                bytes: 1,
                duration_ms: 2_000,
                usage: Some(tw_api::UsageView {
                    output: 100,
                    ..Default::default()
                }),
            },
            now,
        );
        let live = b.live_at(now);
        assert_eq!(live.tokens_per_sec, None);
        assert!(live.running.is_empty());
    }

    /// 菜单的「进行中」要知道是谁、用什么模型、跑了多久；开始得早的在前
    #[test]
    fn the_running_list_says_who_what_and_since_when() {
        let b = EventBus::new();
        let mut ev = started(7);
        if let tw_api::Event::RequestStarted {
            client_hint,
            session,
            at_ms,
            ..
        } = &mut ev
        {
            *client_hint = Some("codex".into());
            *session = Some("fp-900".into());
            *at_ms = 900;
        }
        let then = Instant::now();
        b.track_at(&started(3), then);
        b.track_at(&ev, then);
        let live = b.live_at(then + Duration::from_millis(1_500));
        assert_eq!(
            live.running.iter().map(|r| r.id).collect::<Vec<_>>(),
            [7, 3]
        );
        assert_eq!(
            live.running[0],
            tw_api::RunningView {
                id: 7,
                client: "c".into(),
                client_hint: Some("codex".into()),
                model: "m".into(),
                provider: "p".into(),
                at_ms: 900,
                // **core 数的**，按它自己的单调时钟 —— 不靠界面的时钟去减 `at_ms`
                elapsed_ms: 1_500,
                session: Some("fp-900".into()),
                route: "default".into(),
                rule: "catch-all".into(),
                group: Some("__all__".into()),
                // 路由还没报出结论：不知道最后是哪家接下的
                upstream: None,
            }
        );
    }

    /// 路由报出结论之后，在跑的那一条知道是哪家接下的：尝试链里 `served` 的那一跳，
    /// 不是开始时的首选
    #[test]
    fn a_running_request_names_the_upstream_that_took_it_once_routed() {
        let b = EventBus::new();
        b.emit(started(1));
        b.emit(routed(1, "q"));
        let live = b.live();
        assert_eq!(live.running[0].provider, "p");
        assert_eq!(live.running[0].upstream.as_deref(), Some("q"));
    }

    #[test]
    fn ids_resume_after_the_number_the_store_already_has() {
        let b = EventBus::new();
        b.resume_after(1258);
        assert_eq!(b.next_id(), 1259);
        assert_eq!(b.next_id(), 1260);
    }

    #[test]
    fn resuming_never_hands_out_a_number_twice() {
        // **只进不退。**往回拨就是在制造重号，而重号在存储层是覆盖。
        let b = EventBus::new();
        assert_eq!(b.next_id(), 1);
        assert_eq!(b.next_id(), 2);
        b.resume_after(0);
        assert_eq!(b.next_id(), 3, "被拨回去了");
        b.resume_after(100);
        b.resume_after(50);
        assert_eq!(b.next_id(), 101);
    }

    #[tokio::test]
    async fn emitting_with_no_subscribers_is_not_an_error() {
        // 没开 UI 的时候数据面照常跑 —— 这是最常见的状态。
        let b = EventBus::new();
        b.emit(tw_api::Event::RequestFinished {
            id: 1,
            model: String::new(),
            status: 200,
            bytes: 0,
            duration_ms: 0,
            usage: None,
        });
        assert_eq!(b.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn a_subscriber_receives_what_is_emitted_after_it_subscribes() {
        let b = EventBus::new();
        let mut rx = b.subscribe();
        b.emit(tw_api::Event::RequestFinished {
            id: 42,
            model: String::new(),
            status: 200,
            bytes: 1,
            duration_ms: 2,
            usage: None,
        });
        assert_eq!(rx.recv().await.unwrap().id(), 42);
    }

    #[tokio::test]
    async fn ids_are_unique_and_monotonic() {
        let b = EventBus::new();
        let a = b.next_id();
        let c = b.next_id();
        assert!(c > a);
    }

    #[tokio::test]
    async fn a_slow_subscriber_lags_but_does_not_block_the_producer() {
        // 这是关键性质：观测端跟不上，数据面**不能**因此变慢。
        let b = EventBus::new();
        let mut rx = b.subscribe();
        for i in 0..(CAPACITY as u64 + 100) {
            b.emit(tw_api::Event::RequestFinished {
                id: i,
                model: String::new(),
                status: 200,
                bytes: 0,
                duration_ms: 0,
                usage: None,
            });
        }
        // 生产端全程没阻塞；消费端会收到一个 Lagged
        match rx.recv().await {
            Err(broadcast::error::RecvError::Lagged(n)) => assert!(n > 0),
            other => panic!("应该 lag，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn two_subscribers_both_get_the_event() {
        // UI 开着、CLI 也在 tail —— 这是支持的用法。
        let b = EventBus::new();
        let mut a = b.subscribe();
        let mut c = b.subscribe();
        b.emit(tw_api::Event::RequestFinished {
            id: 9,
            model: String::new(),
            status: 200,
            bytes: 0,
            duration_ms: 0,
            usage: None,
        });
        assert_eq!(a.recv().await.unwrap().id(), 9);
        assert_eq!(c.recv().await.unwrap().id(), 9);
    }
}
