//! 上游暂时发不出去：回了 429 之后的暂停，用量上限（见 [`crate::upstream_limits`]），和等它们的
//! 请求排的队。
//!
//! **这些都不是失败。**上游没坏，只是要等一会儿：不进熔断的账，不算进成功率，`load-balance`
//! 也不因此少分给它（见 [`crate::health`]）。请求先换下一家；没有下一家了，在它们里面等先能发的
//! 那一家，等到这个请求的等待期限（`failover.wait_secs`）为止（见 `server::pipeline::hop`）。
//!
//! 四种样子，前三种等得到，最后一种不等：
//!
//! - **满着**（`max_concurrent`）：哪个请求结束了就空出来，事先不知道是什么时候（见
//!   [`crate::slots`]）；
//! - **限流暂停**：上游回了 429（不是额度用完）。暂停多久按上游说的 —— `retry-after-ms`、
//!   `Retry-After`、正文里的等待时间（[`pause_hint`]）—— 没说就按这一家连续被限流的次数退避
//!   （[`backoff`]），最多 `failover.rate_limit_max_pause_secs`；
//! - **滚动窗口满了**：分钟、小时的用量上限，最近的用量滑出窗口就又能发，什么时候是算得出的；
//! - **这一期用完了**：天、周、月的用量上限，到下一期之前等多久都一样 —— 跳过，不等。
//!
//! **每家一个队，先来先发**：留在这一家的对话和别的请求排在同一个队里，不插队。一个请求在几家
//! 的队里同时排着，哪一家先放它就去哪一家，别的队它当场离开（[`Waiter`]）。暂停结束时**只放
//! 队头一个过去探路**（[`Probe`]）：它拿到不是 429 的响应头，或者发出去 [`PROBE_GRACE`] 还没有
//! 结论，这一家就算好了，后面的照并发和用量上限放；它又拿到 429，这一家接着暂停（退避接着往上
//! 走），它回到队头。滚动窗口空出来不用探路：用量是本地数的，空出多少放多少。
//!
//! 客户端走了、请求被手动中止了：当场离开队，什么都不占。
//!
//! **只在内存里**：跨配置重载存活，重启就没了 —— 重启之后第一个请求照常发，上游还在限流的话
//! 再回一个 429，这里重新暂停。
//!
//! 状态变了（开始暂停、暂停结束、用量上限满了或空出来）报 [`tw_api::Event::ThrottleChanged`]，
//! 界面显示它；它自己会好，不发通知。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use http::HeaderMap;
use tokio::sync::watch;
use tw_api::ThrottleKind;
use tw_config::KeyLimit;

use crate::upstream_limits::LimitState;

/// 暂停结束后放过去探路的那一个，发出去这么久还没有结论（响应头没到）就当这一家好了。
/// 上游要是还在限流，多半当场就回 429；一个长回答的响应头到得慢，不该让后面的一直等
pub const PROBE_GRACE: Duration = Duration::from_secs(10);

/// 上游没说暂停多久时，按这一家连续被限流的次数退避：第一次 5 秒，第二次 10 秒……第五次起
/// 每次 60 秒
const BACKOFF_SECS: [u64; 5] = [5, 10, 20, 40, 60];

/// 一家上游的用量上限写在哪儿（`providers[].limits`，见 [`crate::upstream_limits`]）
pub fn usage_limits(p: &tw_config::Provider) -> &[KeyLimit] {
    &p.limits
}

/// 这一家连续第 `strikes` 次被限流、上游又没说暂停多久时，暂停多久
pub fn backoff(strikes: u32) -> Duration {
    let i = (strikes.max(1) - 1).min(BACKOFF_SECS.len() as u32 - 1) as usize;
    Duration::from_secs(BACKOFF_SECS[i])
}

/// 上游在 429 里说了暂停多久没有。**按这个顺序取第一个**：`retry-after-ms`（毫秒，Anthropic、
/// OpenAI 的 SDK 认它）、`Retry-After`（秒数或 HTTP 日期）、正文里的等待时间（Gemini 的
/// `retryDelay`）。都没有是 None，按退避算
pub fn pause_hint(headers: &HeaderMap, body: &[u8], now_ms: u64) -> Option<Duration> {
    retry_after_ms(headers)
        .or_else(|| crate::failure::retry_after(headers, now_ms))
        .or_else(|| crate::failure::retry_delay_in_body(body))
}

fn retry_after_ms(headers: &HeaderMap) -> Option<Duration> {
    let v: f64 = headers
        .get("retry-after-ms")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (v.is_finite() && v >= 0.0).then(|| Duration::from_secs_f64(v / 1000.0))
}

/// 一家上游此刻能不能发、对这一个请求来说（[`Throttles::gate`]）。
pub enum Gate {
    /// 能发。暂停刚结束时，这一个是探路的（带着 [`Probe`]）
    Go(Option<Probe>),
    /// 等得到：限流暂停着（`rate_limited`）或者滚动窗口满了（`window_full`）。`until_ms` 是
    /// 预计又能发的那一刻；前面还有人排着、探路的还没发出去时不知道，是 None
    Wait {
        kind: ThrottleKind,
        until_ms: Option<u64>,
    },
    /// 这一期用完了：跳过，不等。`limit` 是哪一条上限
    UsedUp { until_ms: u64, limit: u32 },
}

/// 只看不占的 [`Gate`]：不拿探路的名额
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Peek {
    Open,
    Wait {
        kind: ThrottleKind,
        until_ms: Option<u64>,
    },
    UsedUp {
        until_ms: u64,
        limit: u32,
    },
}

type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// 每家上游的暂停、排队和探路。**跨重载存活**，按上游的名字记。
pub struct Throttles {
    ups: Mutex<HashMap<String, Up>>,
    limits: Arc<crate::upstream_limits::UpstreamLimits>,
    bus: tw_observe::EventBus,
    settings: Mutex<Settings>,
    clock: Clock,
    /// 排队的、探路的各自的号
    next: AtomicU64,
}

#[derive(Default)]
struct Settings {
    max_pause: Duration,
    /// 每家的用量上限：报状态、等窗口空出来时要看
    limits: HashMap<String, Vec<KeyLimit>>,
    /// 配置换了几次：看着用量上限的那些后台任务，配置换了就收工
    generation: u64,
}

/// 一家上游记着的。
struct Up {
    /// 暂停到这一刻（Unix 毫秒）。没暂停过是 0
    until_ms: u64,
    /// 最近一次收到 429 的时刻。**在它之前发出去的请求的答复不算数**：一阵同时被限流的请求
    /// 只算一次，暂停之前发出去的一个长回答到了响应头，也不说明这家好了
    limited_at_ms: u64,
    /// 连续被限流的次数（退避按它算）。拿到别的答复就归零
    strikes: u32,
    /// 暂停结束了、还没有结论：只放一个过去探路
    probing: bool,
    /// 探路的那一个：它的号，和发出去的时刻（还没发出去是 None）
    probe: Option<(u64, Option<u64>)>,
    /// 排着队的，先来的在前
    queue: VecDeque<u64>,
    /// 队是为什么排起来的：后来的、不在队里的请求看到的就是这个
    queue_kind: ThrottleKind,
    /// 有变化就加一：排队的请求盯着它
    tx: watch::Sender<u64>,
    /// 最近一次报出去的样子（[`tw_api::Event::ThrottleChanged`]）
    shown: Option<tw_api::Throttle>,
}

impl Up {
    fn new() -> Self {
        Self {
            until_ms: 0,
            limited_at_ms: 0,
            strikes: 0,
            probing: false,
            probe: None,
            queue: VecDeque::new(),
            queue_kind: ThrottleKind::RateLimited,
            tx: watch::channel(0).0,
            shown: None,
        }
    }

    fn bump(&self) {
        self.tx.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// 探路的发出去 [`PROBE_GRACE`] 还没有结论：当这一家好了
    fn settle_probe(&mut self, now: u64) {
        if self.probing
            && let Some((_, Some(sent))) = self.probe
            && sent + PROBE_GRACE.as_millis() as u64 <= now
        {
            self.probing = false;
            self.probe = None;
            self.bump();
        }
    }
}

impl Throttles {
    pub fn new(
        bus: tw_observe::EventBus,
        limits: Arc<crate::upstream_limits::UpstreamLimits>,
    ) -> Self {
        Self::with_clock(bus, limits, Arc::new(crate::server::now_ms))
    }

    fn with_clock(
        bus: tw_observe::EventBus,
        limits: Arc<crate::upstream_limits::UpstreamLimits>,
        clock: Clock,
    ) -> Self {
        Self {
            ups: Mutex::new(HashMap::new()),
            limits,
            bus,
            settings: Mutex::new(Settings {
                max_pause: Duration::from_secs(
                    tw_config::Failover::default().rate_limit_max_pause_secs,
                ),
                ..Default::default()
            }),
            clock,
            next: AtomicU64::new(1),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Up>> {
        // 锁中毒了照样拿里面的表：少暂停一次，好过一个不转发的网关
        self.ups.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn settings(&self) -> MutexGuard<'_, Settings> {
        self.settings.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    /// 照这份配置换设置：最长暂停多久、每家的用量上限。**暂停着的照旧暂停着**。用量上限有变化的
    /// 那几家起一个后台任务看着它们，状态变了就报（见 [`Self::refresh`]）
    pub fn configure(self: &Arc<Self>, cfg: &tw_config::Config) {
        let generation = {
            let mut s = self.settings();
            s.max_pause = Duration::from_secs(cfg.failover.rate_limit_max_pause_secs);
            s.limits = cfg
                .providers
                .iter()
                .filter(|p| !usage_limits(p).is_empty())
                .map(|p| (p.name.clone(), usage_limits(p).to_vec()))
                .collect();
            s.generation += 1;
            s.generation
        };
        let names: Vec<String> = self.settings().limits.keys().cloned().collect();
        // 配置换了，用量上限可能删了：报一遍现在的样子
        let known: Vec<String> = self.lock().keys().cloned().collect();
        for n in known.iter().chain(&names) {
            self.refresh(n);
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        for name in names {
            let me = Arc::downgrade(self);
            rt.spawn(async move {
                loop {
                    let Some(t) = me.upgrade() else { return };
                    if t.settings().generation != generation {
                        return;
                    }
                    let until = t.view(&name).map(|v| v.until_ms);
                    let wake = async {
                        match until {
                            Some(at) => tokio::time::sleep(t.left(at)).await,
                            None => std::future::pending().await,
                        }
                    };
                    tokio::select! {
                        _ = t.limits.changed(&name) => {}
                        _ = wake => {}
                    }
                    if t.settings().generation != generation {
                        return;
                    }
                    t.refresh(&name);
                    t.bump(&name);
                }
            });
        }
    }

    /// 离 `at_ms` 还有多久。已经过了的是 0
    fn left(&self, at_ms: u64) -> Duration {
        Duration::from_millis(at_ms.saturating_sub(self.now()))
    }

    /// 这一家的用量上限此刻的样子（[`crate::upstream_limits::UpstreamLimits::state`]），时刻换成
    /// 这里的时钟：用量上限按它自己的时钟算（测试会拨它），暂停、等待期限、`Retry-After` 都按
    /// 这里的。**换的是还要多久**，两个时钟一样时什么都不变
    fn limit_state(&self, name: &str, limits: &[KeyLimit]) -> LimitState {
        let mine = |until_ms: u64| {
            let left = (until_ms as i64)
                .saturating_sub(self.limits.now_ms())
                .max(0) as u64;
            self.now().saturating_add(left)
        };
        match self.limits.state(name, limits) {
            LimitState::Free => LimitState::Free,
            LimitState::WindowFull { until_ms, limit } => LimitState::WindowFull {
                until_ms: mine(until_ms),
                limit,
            },
            LimitState::PeriodUsedUp { until_ms, limit } => LimitState::PeriodUsedUp {
                until_ms: mine(until_ms),
                limit,
            },
        }
    }

    /// 这一家的用量上限（[`Self::configure`] 记下的）
    fn limits_of(&self, name: &str) -> Vec<KeyLimit> {
        self.settings()
            .limits
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    fn bump(&self, name: &str) {
        if let Some(up) = self.lock().get(name) {
            up.bump();
        }
    }

    /// 这一家有变化时会动的那个。排队的请求盯着它（见 [`first_ready`]）
    pub fn watch(&self, name: &str) -> watch::Receiver<u64> {
        self.lock()
            .entry(name.to_string())
            .or_insert_with(Up::new)
            .tx
            .subscribe()
    }

    /// 这一家此刻对这一个请求（`waiter`，在排队的是它的号）能不能发。**能发、又该探路时拿走
    /// 探路的名额**（[`Gate::Go`] 带着 [`Probe`]）。`limits` 是这一家的用量上限
    pub fn gate(self: &Arc<Self>, name: &str, limits: &[KeyLimit], waiter: Option<u64>) -> Gate {
        match self.check(name, limits, waiter, true) {
            (Peek::Open, ticket) => Gate::Go(ticket.map(|ticket| Probe {
                throttles: self.clone(),
                name: name.to_string(),
                ticket,
                sent: false,
            })),
            (Peek::Wait { kind, until_ms }, _) => Gate::Wait { kind, until_ms },
            (Peek::UsedUp { until_ms, limit }, _) => Gate::UsedUp { until_ms, limit },
        }
    }

    /// 只看不占：一个不在任何队里的请求此刻看到的样子。数 token 的请求、排序、会话粘性看它
    pub fn peek(&self, name: &str, limits: &[KeyLimit]) -> Peek {
        self.check(name, limits, None, false).0
    }

    fn check(
        &self,
        name: &str,
        limits: &[KeyLimit],
        waiter: Option<u64>,
        take: bool,
    ) -> (Peek, Option<u64>) {
        let window = match self.limit_state(name, limits) {
            LimitState::PeriodUsedUp { until_ms, limit } => {
                return (Peek::UsedUp { until_ms, limit }, None);
            }
            LimitState::WindowFull { until_ms, limit: _ } => Some(until_ms),
            LimitState::Free => None,
        };
        let now = self.now();
        let mut map = self.lock();
        let Some(up) = map.get_mut(name) else {
            return match window {
                Some(t) => (
                    Peek::Wait {
                        kind: ThrottleKind::WindowFull,
                        until_ms: Some(t),
                    },
                    None,
                ),
                None => (Peek::Open, None),
            };
        };
        up.settle_probe(now);
        if up.until_ms > now {
            let until = up.until_ms.max(window.unwrap_or(0));
            return (
                Peek::Wait {
                    kind: ThrottleKind::RateLimited,
                    until_ms: Some(until),
                },
                None,
            );
        }
        if let Some(t) = window {
            return (
                Peek::Wait {
                    kind: ThrottleKind::WindowFull,
                    until_ms: Some(t),
                },
                None,
            );
        }
        // 先来先发：前面有人排着，就轮不到它
        if let Some(head) = up.queue.front()
            && Some(*head) != waiter
        {
            return (
                Peek::Wait {
                    kind: up.queue_kind,
                    until_ms: None,
                },
                None,
            );
        }
        if up.probing {
            return match up.probe {
                // 探路的已经在路上了：等它的结论
                Some((_, sent)) => (
                    Peek::Wait {
                        kind: ThrottleKind::RateLimited,
                        until_ms: sent.map(|t| t + PROBE_GRACE.as_millis() as u64),
                    },
                    None,
                ),
                None if take => {
                    let ticket = self.next.fetch_add(1, Ordering::Relaxed);
                    up.probe = Some((ticket, None));
                    (Peek::Open, Some(ticket))
                }
                None => (
                    Peek::Wait {
                        kind: ThrottleKind::RateLimited,
                        until_ms: Some(now),
                    },
                    None,
                ),
            };
        }
        (Peek::Open, None)
    }

    /// 留在这一家的对话还留不留（见 [`crate::affinity`]）：这一期用完了的、限流暂停要到
    /// `deadline_ms`（这个请求的等待期限）之后才结束的，放开 —— 和熔断着的一样，留在一家发不出去
    /// 的上游上，省下的缓存一分钱都兑现不了。等得到的照旧留，到时候等它
    pub fn keeps_conversation(&self, name: &str, limits: &[KeyLimit], deadline_ms: u64) -> bool {
        match self.peek(name, limits) {
            Peek::UsedUp { .. } => false,
            Peek::Wait {
                kind: ThrottleKind::RateLimited,
                until_ms: Some(t),
            } => t <= deadline_ms,
            _ => true,
        }
    }

    /// 发给这一家的、`sent_at_ms` 发出去的一跳回了 429（不是额度用完）：暂停这一家。`hint` 是上游
    /// 说的暂停多久（[`pause_hint`]），没说按退避（[`backoff`]）。**已经暂停着的延到两个里晚的那个**。
    /// 交回从此刻起还要暂停多久
    pub fn limited(
        self: &Arc<Self>,
        name: &str,
        sent_at_ms: u64,
        hint: Option<Duration>,
    ) -> Duration {
        let now = self.now();
        let max = self.settings().max_pause;
        let until = {
            let mut map = self.lock();
            let up = map.entry(name.to_string()).or_insert_with(Up::new);
            // 上一次 429 之后才发出去的又被限流了，才是连续的又一次。同一阵里被限流的那几个只算
            // 一次
            if sent_at_ms > up.limited_at_ms {
                up.strikes = up.strikes.saturating_add(1);
            }
            let pause = hint.unwrap_or_else(|| backoff(up.strikes)).min(max);
            up.until_ms = up
                .until_ms
                .max(now.saturating_add(pause.as_millis() as u64));
            up.limited_at_ms = now;
            // 探路的被拒了，或者别人在探路时被拒了：暂停结束后重新探
            up.probing = true;
            up.probe = None;
            up.bump();
            up.until_ms
        };
        self.refresh(name);
        // 暂停结束那一刻也是一次变化，没有哪个请求会触发它
        self.at(name, until);
        Duration::from_millis(until.saturating_sub(now))
    }

    /// 发给这一家的、`sent_at_ms` 发出去的一跳拿到了不是 429 的响应头：连续限流的次数归零；
    /// 在探路的话，这一家好了，排着的照并发和用量上限放。**最近一次 429 之前发出去的不算**
    pub fn answered(&self, name: &str, sent_at_ms: u64) {
        let changed = {
            let mut map = self.lock();
            let Some(up) = map.get_mut(name) else { return };
            if sent_at_ms <= up.limited_at_ms && up.limited_at_ms > 0 {
                return;
            }
            up.strikes = 0;
            let changed = up.probing;
            up.probing = false;
            up.probe = None;
            if changed {
                up.bump();
            }
            changed
        };
        if changed {
            self.refresh(name);
        }
    }

    /// 到 `at_ms` 时重报一次这一家的样子、叫醒排队的
    fn at(self: &Arc<Self>, name: &str, at_ms: u64) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (me, name, wait) = (Arc::downgrade(self), name.to_string(), self.left(at_ms));
        rt.spawn(async move {
            tokio::time::sleep(wait).await;
            if let Some(t) = me.upgrade() {
                t.refresh(&name);
                t.bump(&name);
            }
        });
    }

    /// 这一家此刻暂时发不出去的样子：限流暂停、滚动窗口满了、这一期用完了。**几样都有时给
    /// 又能发得最晚的那一样**。能发是 None。探路那几秒不算：暂停已经结束了
    pub fn view(&self, name: &str) -> Option<tw_api::Throttle> {
        let now = self.now();
        let paused = self
            .lock()
            .get(name)
            .filter(|up| up.until_ms > now)
            .map(|up| tw_api::Throttle {
                kind: ThrottleKind::RateLimited,
                until_ms: up.until_ms,
                limit: None,
            });
        let limited = match self.limit_state(name, &self.limits_of(name)) {
            LimitState::Free => None,
            LimitState::WindowFull { until_ms, limit } => Some(tw_api::Throttle {
                kind: ThrottleKind::WindowFull,
                until_ms,
                limit: Some(limit),
            }),
            LimitState::PeriodUsedUp { until_ms, limit } => Some(tw_api::Throttle {
                kind: ThrottleKind::PeriodUsedUp,
                until_ms,
                limit: Some(limit),
            }),
        };
        match (paused, limited) {
            (Some(a), Some(b)) => Some(if b.until_ms > a.until_ms { b } else { a }),
            (a, b) => a.or(b),
        }
    }

    /// 这一家的样子和上次报的不一样了就报一条 [`tw_api::Event::ThrottleChanged`]
    pub fn refresh(&self, name: &str) {
        let now = self.view(name);
        {
            let mut map = self.lock();
            let up = map.entry(name.to_string()).or_insert_with(Up::new);
            if up.shown == now {
                return;
            }
            up.shown = now.clone();
        }
        let id = self.bus.next_id();
        self.bus.emit(tw_api::Event::ThrottleChanged {
            id,
            provider: name.to_string(),
            throttle: now,
            at_ms: self.now(),
        });
    }

    /// 一个要排队的请求。**丢掉就离开它排着的每一个队**
    pub fn waiter(self: &Arc<Self>) -> Waiter {
        Waiter {
            throttles: self.clone(),
            id: self.next.fetch_add(1, Ordering::Relaxed),
            joined: Vec::new(),
        }
    }

    fn join(&self, name: &str, id: u64, kind: ThrottleKind, front: bool) {
        let mut map = self.lock();
        let up = map.entry(name.to_string()).or_insert_with(Up::new);
        if up.queue.is_empty() {
            up.queue_kind = kind;
        }
        match up.queue.iter().position(|w| *w == id) {
            Some(0) => {}
            Some(at) if front => {
                up.queue.remove(at);
                up.queue.push_front(id);
            }
            Some(_) => {}
            None if front => up.queue.push_front(id),
            None => up.queue.push_back(id),
        }
    }

    fn leave(&self, name: &str, id: u64) {
        let mut map = self.lock();
        if let Some(up) = map.get_mut(name)
            && let Some(at) = up.queue.iter().position(|w| *w == id)
        {
            up.queue.remove(at);
            // 队头走了：下一个该看看轮没轮到它
            if at == 0 {
                up.bump();
            }
        }
    }

    #[cfg(test)]
    fn queue(&self, name: &str) -> Vec<u64> {
        self.lock()
            .get(name)
            .map(|u| u.queue.iter().copied().collect())
            .unwrap_or_default()
    }
}

/// 一个在排队的请求。在哪几家的队里排着，**丢掉就全部离开**：客户端走了、被手动中止了，
/// 后面的不用替它等。
pub struct Waiter {
    throttles: Arc<Throttles>,
    pub id: u64,
    joined: Vec<String>,
}

impl Waiter {
    /// 排到这一家的队尾。已经在队里的不动
    pub fn join(&mut self, name: &str, kind: ThrottleKind) {
        self.throttles.join(name, self.id, kind, false);
        if !self.joined.iter().any(|n| n == name) {
            self.joined.push(name.to_string());
        }
    }

    /// 排到这一家的队头：探路的又被限流了，回到它原来的位置
    pub fn join_front(&mut self, name: &str) {
        self.throttles
            .join(name, self.id, ThrottleKind::RateLimited, true);
        if !self.joined.iter().any(|n| n == name) {
            self.joined.push(name.to_string());
        }
    }

    /// 离开排着的每一个队：去了一家，或者不等了
    pub fn leave_all(&mut self) {
        for n in std::mem::take(&mut self.joined) {
            self.throttles.leave(&n, self.id);
        }
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        self.leave_all();
    }
}

/// 暂停结束后放过去探路的那一个。**发出去时说一声**（[`Probe::sent`]）：从那一刻起
/// [`PROBE_GRACE`] 还没有结论，这一家就算好了。没发出去就丢掉的（没占到位置、被中止了），名额
/// 还回去，下一个接着探
pub struct Probe {
    throttles: Arc<Throttles>,
    name: String,
    ticket: u64,
    sent: bool,
}

impl Probe {
    /// 这一跳发出去了
    pub fn sent(&mut self) {
        self.sent = true;
        let at = {
            let now = self.throttles.now();
            let mut map = self.throttles.lock();
            let Some(up) = map.get_mut(&self.name) else {
                return;
            };
            match &mut up.probe {
                Some((t, sent)) if *t == self.ticket => *sent = Some(now),
                _ => return,
            }
            up.bump();
            now + PROBE_GRACE.as_millis() as u64
        };
        self.throttles.at(&self.name, at);
    }

    /// 名额还有效：拿到之后这一家没有又被限流
    pub fn valid(&self) -> bool {
        let now = self.throttles.now();
        self.throttles.lock().get(&self.name).is_some_and(|up| {
            up.until_ms <= now && matches!(up.probe, Some((t, _)) if t == self.ticket)
        })
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        if self.sent {
            return;
        }
        let mut map = self.throttles.lock();
        if let Some(up) = map.get_mut(&self.name)
            && matches!(up.probe, Some((t, None)) if t == self.ticket)
        {
            up.probe = None;
            up.bump();
        }
    }
}

/// 等到了的那一家：占到的位置，和探路的名额（该探路时）。
pub struct Ready {
    pub slot: crate::slots::Slot,
    pub probe: Option<Probe>,
}

/// 在 `names`（每一家和它的用量上限）里等先能发的那一家：不暂停、用量上限没满、排到了它、
/// 有空位。交回它在 `names` 里的位置和 [`Ready`]；等到 `until` 都没有是 None。
///
/// 等的时候 `waiter` 排在等得到的那几家的队里，**等到了、等不到都离开所有的队**：等不到的请求
/// 接着去试别的候选，不该还占着队头。满着的那几家照 [`crate::slots`] 的规矩等空位（不排这里的
/// 队）。**同时能发的按 `names` 的顺序**，也就是候选的顺序。
pub(crate) async fn first_ready(
    state: &crate::AppState,
    waiter: &mut Waiter,
    names: &[(&str, &[KeyLimit])],
    until: tokio::time::Instant,
) -> Option<(usize, Ready)> {
    let got = wait_ready(state, waiter, names, until).await;
    waiter.leave_all();
    got
}

async fn wait_ready(
    state: &crate::AppState,
    waiter: &mut Waiter,
    names: &[(&str, &[KeyLimit])],
    until: tokio::time::Instant,
) -> Option<(usize, Ready)> {
    let throttles = &state.throttles;
    // 拿到了探路的名额、在等空位的那几家：名额攥着，空出位置就用它发
    let mut held: HashMap<usize, Probe> = HashMap::new();
    loop {
        // **先盯上再看**：看的时候发生的变化不会漏掉
        let mut changes: Vec<watch::Receiver<u64>> =
            names.iter().map(|(n, _)| throttles.watch(n)).collect();
        for c in &mut changes {
            c.borrow_and_update();
        }
        let mut slot_waits: Vec<usize> = Vec::new();
        let mut wake: Option<u64> = None;
        for (i, (name, limits)) in names.iter().enumerate() {
            if held.get(&i).is_some_and(|p| !p.valid()) {
                held.remove(&i);
            }
            let probe = match held.remove(&i) {
                Some(p) => Some(p),
                None => match throttles.gate(name, limits, Some(waiter.id)) {
                    Gate::Go(probe) => probe,
                    Gate::Wait { kind, until_ms } => {
                        waiter.join(name, kind);
                        if let Some(t) = until_ms {
                            wake = Some(wake.map_or(t, |w| w.min(t)));
                        }
                        continue;
                    }
                    // 等的时候这一期用完了：不等它了，到期限之前也重置不了
                    Gate::UsedUp { .. } => continue,
                },
            };
            match state.slots.try_take(name) {
                Some(slot) => return Some((i, Ready { slot, probe })),
                None => {
                    if let Some(p) = probe {
                        held.insert(i, p);
                    }
                    slot_waits.push(i);
                }
            }
        }
        if tokio::time::Instant::now() >= until {
            return None;
        }
        let full: Vec<&str> = slot_waits.iter().map(|i| names[*i].0).collect();
        let changed =
            futures::future::select_all(changes.iter_mut().map(|c| Box::pin(c.changed())));
        let windows = futures::future::select_all(
            names
                .iter()
                .map(|(n, _)| Box::pin(state.upstream_limits.changed(n))),
        );
        let woken = async {
            match wake {
                Some(at) => tokio::time::sleep(throttles.left(at)).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            got = state.slots.first_free(&full, until), if !full.is_empty() => {
                let (k, slot) = got?;
                let i = slot_waits[k];
                let (name, limits) = names[i];
                // 空出位置的这一刻它还能发吗：等的时候它可能又被限流了
                let probe = match held.remove(&i).filter(Probe::valid) {
                    Some(p) => Some(p),
                    None => match throttles.gate(name, limits, Some(waiter.id)) {
                        Gate::Go(probe) => probe,
                        _ => continue,
                    },
                };
                return Some((i, Ready { slot, probe }));
            }
            _ = changed => {}
            _ = windows => {}
            _ = woken => {}
            _ = tokio::time::sleep_until(until) => return None,
        }
    }
}

/// 等到最后还发不出去的一家，为什么（[`refusal`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stuck {
    /// 满着（`max_concurrent`）
    Busy,
    /// 限流暂停着：预计又能发的那一刻，不知道是 None
    RateLimited(Option<u64>),
    /// 滚动窗口满了：同上
    WindowFull(Option<u64>),
    /// 这一期用完了：重置的那一刻
    UsedUp(u64),
}

/// 没有哪一家在等待期限之前能发：客户端收到的 429。**说出每一家、各是为什么**；`Retry-After`
/// 是最先能发的那一家还要多久（满着的按 [`crate::error::BUSY_RETRY_AFTER_SECS`] 算），可以重试。
/// 全都是这一期用完了的，`Retry-After` 是最早的重置，**到那之前别重试**（`x-should-retry: false`，
/// 见 [`crate::error::Retry`]）。都只是满着的，和以前一样是 [`crate::error::Source::Busy`]
pub fn refusal(stuck: &[(String, Stuck)], now_ms: u64) -> crate::GatewayError {
    use crate::GatewayError;
    use tw_types::msg;
    let names = |f: &dyn Fn(&Stuck) -> bool| {
        stuck
            .iter()
            .filter(|(_, s)| f(s))
            .map(|(n, _)| format!("`{n}`"))
            .collect::<Vec<_>>()
    };
    let busy = names(&|s| matches!(s, Stuck::Busy));
    let rate_limited = names(&|s| matches!(s, Stuck::RateLimited(_)));
    let window_full = names(&|s| matches!(s, Stuck::WindowFull(_)));
    let used_up = names(&|s| matches!(s, Stuck::UsedUp(_)));
    let all = |v: &Vec<String>| v.len() == stuck.len();
    let list = |v: &Vec<String>| {
        if v.is_empty() {
            "—".to_string()
        } else {
            v.join(", ")
        }
    };
    if all(&busy) {
        return GatewayError::busy(msg!(
            "gw.busy_all", upstreams = list(&busy) =>
            "Every upstream that can serve this request is at its concurrency limit \
             (max_concurrent): {upstreams}. None had a free slot in time; try again shortly."
        ));
    }
    if all(&used_up) {
        let reset = stuck
            .iter()
            .filter_map(|(_, s)| match s {
                Stuck::UsedUp(t) => Some(*t),
                _ => None,
            })
            .min()
            .unwrap_or(now_ms);
        return GatewayError::rate_limited(msg!(
            "gw.wait.used_up", upstreams = list(&used_up) =>
            "Every upstream that can serve this request has used up one of its usage limits for \
             the current day, week or month: {upstreams}. Retrying before the earliest reset does \
             not help."
        ))
        .with_retry(crate::error::Retry {
            after_ms: reset.saturating_sub(now_ms).max(1),
            until_reset: true,
        });
    }
    // 最先能发的那一家还要多久。说不出的（探路的还没回话、前面有人排着）按探路的宽限算
    let after_ms = stuck
        .iter()
        .filter_map(|(_, s)| match s {
            Stuck::Busy => Some(crate::error::BUSY_RETRY_AFTER_SECS * 1000),
            Stuck::RateLimited(t) | Stuck::WindowFull(t) => {
                Some(t.map_or(PROBE_GRACE.as_millis() as u64, |t| t.saturating_sub(now_ms)))
            }
            Stuck::UsedUp(_) => None,
        })
        .min()
        .unwrap_or(PROBE_GRACE.as_millis() as u64)
        .max(1);
    let why = if all(&rate_limited) {
        msg!(
            "gw.wait.rate_limited", upstreams = list(&rate_limited) =>
            "Every upstream that can serve this request is paused after answering 429 (rate \
             limited): {upstreams}. None took requests again in time; try again shortly."
        )
    } else if all(&window_full) {
        msg!(
            "gw.wait.window_full", upstreams = list(&window_full) =>
            "Every upstream that can serve this request is at one of its per-minute or per-hour \
             usage limits: {upstreams}. None had room again in time; try again shortly."
        )
    } else {
        msg!(
            "gw.wait.mixed",
            rate_limited = list(&rate_limited), window_full = list(&window_full),
            used_up = list(&used_up), busy = list(&busy) =>
            "No upstream that can serve this request became available in time. Paused after \
             answering 429: {rate_limited}. At a per-minute or per-hour usage limit: \
             {window_full}. Used up for the current day, week or month: {used_up}. At the \
             concurrency limit: {busy}. Try again shortly."
        )
    };
    GatewayError::rate_limited(why).with_retry(crate::error::Retry {
        after_ms,
        until_reset: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    const T0: u64 = 1_800_000_000_000;

    fn clocked() -> (Arc<Throttles>, Arc<AtomicU64>) {
        let now = Arc::new(AtomicU64::new(T0));
        let bus = tw_observe::EventBus::new();
        let limits = Arc::new(crate::upstream_limits::UpstreamLimits::new(bus.clone()));
        let t = Arc::new(Throttles::with_clock(bus, limits, {
            let now = now.clone();
            Arc::new(move || now.load(Ordering::SeqCst))
        }));
        (t, now)
    }

    fn advance(now: &AtomicU64, ms: u64) {
        now.fetch_add(ms, Ordering::SeqCst);
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    fn is_go(g: &Gate) -> bool {
        matches!(g, Gate::Go(_))
    }

    #[test]
    fn the_pause_takes_what_the_upstream_says_in_order() {
        let gemini = br#"{"error":{"details":[{"retryDelay":"33s"}]}}"#;
        // 毫秒的那个最准，先看它
        let h = headers(&[("retry-after-ms", "1500"), ("retry-after", "30")]);
        assert_eq!(
            pause_hint(&h, gemini, T0),
            Some(Duration::from_millis(1500))
        );
        // 然后是 Retry-After：秒数，或者一个 HTTP 日期
        let h = headers(&[("retry-after", "30")]);
        assert_eq!(pause_hint(&h, gemini, T0), Some(Duration::from_secs(30)));
        let at = chrono::DateTime::from_timestamp_millis((T0 + 90_000) as i64)
            .unwrap()
            .to_rfc2822();
        let h = headers(&[("retry-after", &at)]);
        assert_eq!(pause_hint(&h, b"{}", T0), Some(Duration::from_secs(90)));
        // 头里都没说，看正文
        assert_eq!(
            pause_hint(&HeaderMap::new(), gemini, T0),
            Some(Duration::from_secs(33))
        );
        // 写坏了的不算，接着往下看
        let h = headers(&[("retry-after-ms", "soon"), ("retry-after", "7")]);
        assert_eq!(pause_hint(&h, b"", T0), Some(Duration::from_secs(7)));
        assert_eq!(pause_hint(&HeaderMap::new(), b"slow down", T0), None);
    }

    #[test]
    fn without_a_hint_the_pause_backs_off_and_any_other_answer_starts_over() {
        let (t, now) = clocked();
        let mut got = Vec::new();
        for _ in 0..7 {
            // 每一次都是上一次暂停结束之后才发出去的那一个（探路的）
            let sent = now.load(Ordering::SeqCst);
            let d = t.limited("甲", sent, None);
            got.push(d.as_secs());
            advance(&now, d.as_millis() as u64);
        }
        assert_eq!(got, [5, 10, 20, 40, 60, 60, 60]);
        // 拿到一个不是 429 的答复：从头退避
        t.answered("甲", now.load(Ordering::SeqCst));
        let sent = now.load(Ordering::SeqCst);
        assert_eq!(t.limited("甲", sent, None), Duration::from_secs(5));
    }

    #[test]
    fn a_burst_of_429s_counts_once_and_a_later_one_extends_the_pause() {
        let (t, now) = clocked();
        // 同一阵发出去的五个都被限流：只算一次，暂停 5 秒
        for _ in 0..5 {
            t.limited("甲", T0, None);
        }
        assert!(matches!(
            t.peek("甲", &[]),
            Peek::Wait { kind: ThrottleKind::RateLimited, until_ms: Some(u) } if u == T0 + 5_000
        ));
        // 暂停中又一个说了 30 秒：延到晚的那个
        advance(&now, 1_000);
        let d = t.limited("甲", T0, Some(Duration::from_secs(30)));
        assert_eq!(d, Duration::from_secs(30));
        // 说得更短的不往前提
        let d = t.limited("甲", T0, Some(Duration::from_secs(1)));
        assert_eq!(d, Duration::from_secs(30));
        // 最长暂停按配置封顶
        let (t, _) = clocked();
        let d = t.limited("乙", T0, Some(Duration::from_secs(99_999)));
        assert_eq!(d, Duration::from_secs(3600));
    }

    #[test]
    fn after_a_pause_only_one_request_goes_ahead_until_it_is_answered() {
        let (t, now) = clocked();
        t.limited("甲", T0, None);
        assert!(!is_go(&t.gate("甲", &[], None)), "暂停中");
        advance(&now, 5_000);
        // 暂停结束：头一个去探路
        let Gate::Go(Some(mut probe)) = t.gate("甲", &[], None) else {
            panic!("暂停结束了该放一个去探路");
        };
        // 它在路上的时候别的都等
        assert!(matches!(
            t.gate("甲", &[], None),
            Gate::Wait {
                kind: ThrottleKind::RateLimited,
                until_ms: None
            }
        ));
        probe.sent();
        let sent = now.load(Ordering::SeqCst);
        assert!(matches!(
            t.peek("甲", &[]),
            Peek::Wait { until_ms: Some(u), .. } if u == sent + 10_000
        ));
        // 它拿到了响应头，不是 429：放开
        t.answered("甲", sent);
        assert!(matches!(t.gate("甲", &[], None), Gate::Go(None)));
    }

    #[test]
    fn a_probe_that_is_limited_again_pauses_longer_and_one_without_a_verdict_passes_after_the_grace()
     {
        let (t, now) = clocked();
        t.limited("甲", T0, None);
        advance(&now, 5_000);
        let Gate::Go(Some(mut probe)) = t.gate("甲", &[], None) else {
            panic!()
        };
        probe.sent();
        let sent = now.load(Ordering::SeqCst);
        // 探路的又被限流：退避接着往上走
        assert_eq!(t.limited("甲", sent, None), Duration::from_secs(10));
        assert!(!probe.valid());
        drop(probe);
        advance(&now, 10_000);
        let Gate::Go(Some(mut probe)) = t.gate("甲", &[], None) else {
            panic!()
        };
        probe.sent();
        drop(probe);
        // 一直没有结论：10 秒之后当它好了
        advance(&now, 9_999);
        assert!(!is_go(&t.gate("甲", &[], None)));
        advance(&now, 1);
        assert!(matches!(t.gate("甲", &[], None), Gate::Go(None)));
    }

    #[test]
    fn a_probe_dropped_before_it_was_sent_lets_the_next_one_go() {
        let (t, now) = clocked();
        t.limited("甲", T0, None);
        advance(&now, 5_000);
        let Gate::Go(Some(probe)) = t.gate("甲", &[], None) else {
            panic!()
        };
        // 没占到位置、被中止了：名额还回去
        drop(probe);
        assert!(matches!(t.gate("甲", &[], None), Gate::Go(Some(_))));
    }

    #[test]
    fn an_answer_to_a_request_sent_before_the_429_does_not_end_the_pause() {
        let (t, now) = clocked();
        advance(&now, 1_000);
        t.limited("甲", T0 + 1_000, None);
        advance(&now, 5_000);
        // 暂停之前发出去的长回答这时才到响应头：不说明这家好了
        t.answered("甲", T0);
        assert!(matches!(t.gate("甲", &[], None), Gate::Go(Some(_))));
    }

    #[test]
    fn the_queue_lets_them_go_in_order_and_a_leaver_makes_room() {
        let (t, now) = clocked();
        t.limited("甲", T0, None);
        let (mut a, mut b) = (t.waiter(), t.waiter());
        a.join("甲", ThrottleKind::RateLimited);
        b.join("甲", ThrottleKind::RateLimited);
        assert_eq!(t.queue("甲"), [a.id, b.id]);
        advance(&now, 5_000);
        // 后来的、不在队里的不插队
        assert!(matches!(
            t.gate("甲", &[], None),
            Gate::Wait {
                kind: ThrottleKind::RateLimited,
                until_ms: None
            }
        ));
        assert!(!is_go(&t.gate("甲", &[], Some(b.id))), "b 排在 a 后面");
        // 队头探路
        let Gate::Go(Some(mut probe)) = t.gate("甲", &[], Some(a.id)) else {
            panic!()
        };
        a.leave_all();
        probe.sent();
        let sent = now.load(Ordering::SeqCst);
        // 探路的又被限流：它回到队头
        t.limited("甲", sent, None);
        a.join_front("甲");
        assert_eq!(t.queue("甲"), [a.id, b.id]);
        // a 不等了（客户端走了）：b 排到了头
        drop(a);
        assert_eq!(t.queue("甲"), [b.id]);
        advance(&now, 10_000);
        assert!(matches!(t.gate("甲", &[], Some(b.id)), Gate::Go(Some(_))));
    }

    #[tokio::test]
    async fn every_change_is_reported_once() {
        let (t, now) = clocked();
        let mut rx = t.bus.subscribe();
        t.limited("甲", T0, None);
        t.limited("甲", T0, None);
        advance(&now, 5_000);
        t.refresh("甲");
        t.refresh("甲");
        let mut got = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let tw_api::Event::ThrottleChanged {
                provider, throttle, ..
            } = ev
            {
                got.push((provider, throttle));
            }
        }
        assert_eq!(
            got,
            vec![
                (
                    "甲".to_string(),
                    Some(tw_api::Throttle {
                        kind: ThrottleKind::RateLimited,
                        until_ms: T0 + 5_000,
                        limit: None,
                    })
                ),
                ("甲".to_string(), None),
            ]
        );
    }

    #[test]
    fn the_final_429_names_each_upstream_and_when_to_come_back() {
        let n = |v: &[(&str, Stuck)]| {
            v.iter()
                .map(|(a, b)| (a.to_string(), *b))
                .collect::<Vec<_>>()
        };
        // 都在暂停：最先恢复的那一家还要多久，可以重试
        let e = refusal(
            &n(&[
                ("甲", Stuck::RateLimited(Some(T0 + 30_000))),
                ("乙", Stuck::RateLimited(Some(T0 + 1_500))),
            ]),
            T0,
        );
        assert_eq!(e.detail.code, "gw.wait.rate_limited");
        assert_eq!(e.detail.arg("upstreams"), "`甲`, `乙`");
        assert_eq!(e.source, crate::error::Source::RateLimited);
        assert_eq!(
            e.retry,
            Some(crate::error::Retry {
                after_ms: 1_500,
                until_reset: false
            })
        );
        // 全都是这一期用完了：最早的重置，别重试
        let e = refusal(
            &n(&[
                ("甲", Stuck::UsedUp(T0 + 7_200_000)),
                ("乙", Stuck::UsedUp(T0 + 3_600_000)),
            ]),
            T0,
        );
        assert_eq!(e.detail.code, "gw.wait.used_up");
        assert_eq!(
            e.retry,
            Some(crate::error::Retry {
                after_ms: 3_600_000,
                until_reset: true
            })
        );
        // 只是满着：和以前一样
        let e = refusal(&n(&[("甲", Stuck::Busy)]), T0);
        assert_eq!(e.detail.code, "gw.busy_all");
        assert_eq!(e.source, crate::error::Source::Busy);
        assert_eq!(e.retry.unwrap().after_ms, 5_000);
        // 几样都有：每一家各在它那一类里；用完了的不算进多久之后再来
        let e = refusal(
            &n(&[
                ("甲", Stuck::WindowFull(Some(T0 + 20_000))),
                ("乙", Stuck::UsedUp(T0 + 1_000)),
                ("丙", Stuck::Busy),
            ]),
            T0,
        );
        assert_eq!(e.detail.code, "gw.wait.mixed");
        assert_eq!(e.detail.arg("window_full"), "`甲`");
        assert_eq!(e.detail.arg("used_up"), "`乙`");
        assert_eq!(e.detail.arg("busy"), "`丙`");
        assert_eq!(e.detail.arg("rate_limited"), "—");
        assert_eq!(
            e.retry,
            Some(crate::error::Retry {
                after_ms: 5_000,
                until_reset: false
            })
        );
        // 已经过了的时刻不给 0
        let e = refusal(&n(&[("甲", Stuck::WindowFull(Some(T0 - 5)))]), T0);
        assert_eq!(e.retry.unwrap().after_ms, 1);
    }
}
