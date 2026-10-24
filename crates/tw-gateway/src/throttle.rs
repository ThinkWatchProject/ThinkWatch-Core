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
//!   （[`backoff`]），最多 `failover.rate_limit_max_pause_secs`。说了等于没说的（`Retry-After: 0`）
//!   当没说；
//! - **滚动窗口满了**：分钟、小时的用量上限，最近的用量滑出窗口就又能发，什么时候是算得出的；
//! - **这一期用完了**：天、周、月的用量上限，到下一期之前等多久都一样 —— 跳过，不等。
//!
//! **每家一个队，先来先发**：留在这一家的对话和别的请求排在同一个队里，不插队。一个请求在几家
//! 的队里同时排着，哪一家先放它就去哪一家，别的队它当场离开（[`Waiter`]）。暂停结束时**只放
//! 队头一个过去探路**（[`Probe`]）：它拿到不是 429 的答复（压着开头的回答要等到第一段内容，见
//! `hop`），或者发出去 [`PROBE_GRACE`] 还没有结论，这一家就算好了，后面的照并发和用量上限放；它
//! 又被限流，这一家接着暂停（退避接着往上走），它下一次等的时候回到队头。它没有结论 —— 连不上、
//! 没有内容超时了、回的是额度用完 —— 不再探路，排着的照常放，这一家怎么样交给熔断
//! （[`Throttles::released`]）。滚动窗口空出来不用探路：用量是本地数的，空出多少放多少。
//!
//! 客户端走了、请求被手动中止了：当场离开队，什么都不占。
//!
//! **只在内存里**：跨配置重载存活，重启就没了 —— 重启之后第一个请求照常发，上游还在限流的话
//! 再回一个 429，这里重新暂停。暂停多久按单调时钟和墙上的时钟里走得多的那个算：系统时间被往回拨，
//! 暂停不跟着变长；合上盖子睡过去的时间算进暂停（单调时钟那时不走）。
//!
//! 状态变了（开始暂停、暂停结束、用量上限满了或空出来）报 [`tw_api::Event::ThrottleChanged`]，
//! 界面显示它；它自己会好，不发通知。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use http::HeaderMap;
use tokio::sync::watch;
use tw_api::ThrottleKind;
use tw_config::KeyLimit;

use crate::upstream_limits::LimitState;

/// 暂停结束后放过去探路的那一个，发出去这么久还没有结论就当这一家好了。上游要是还在限流，
/// 多半当场就回 429；一个长回答的开头到得慢，不该让后面的一直等。
///
/// **这不是知道的恢复时刻**：探路的多半早就有了结论。等不等得到（`hop` 的 `hopeless`、会话
/// 粘性）把它当成说不出；只有交给客户端的 `Retry-After` 拿它估一个数（[`refusal`]）
pub const PROBE_GRACE: Duration = Duration::from_secs(10);

/// 上游说的暂停短于这个的当没说（[`Throttles::limited`]）：照它暂停等于不暂停，探路的当场又发、
/// 又被限流，一直空转到这个请求的等待期限。按秒说的（`Retry-After`、正文里的）不到一秒就当没说
/// （见 [`crate::failure::MIN_RETRY_AFTER`]）
const MIN_HINT: Duration = Duration::from_millis(1);

/// 上游没说暂停多久时，按这一家连续被限流的次数退避：第一次 5 秒，第二次 10 秒……第五次起
/// 每次 60 秒
const BACKOFF_SECS: [u64; 5] = [5, 10, 20, 40, 60];

/// 一家最近一次被限流之后这么久之内，它还「可能在限流」（[`Throttles::wary`]）
const WARY_FOR: Duration = Duration::from_secs(300);

/// 两个时钟走的差，到了这么多才当是系统时间被拨过（见 [`Span::end_ms`]）；两个时钟换算出来的
/// 时刻差不到这么多的当是同一刻（见 [`Throttles::limit_state`]）。先后读两个时钟，差会抖一两毫秒
const SLACK_MS: u64 = 1_000;

/// 等一个时刻到来时，最多睡这么久就起来再看一眼。**tokio 的定时器按单调时钟走**，合上盖子睡着的
/// 时候它不走：醒来时暂停按墙上的时钟早就结束了，定时器却还要睡完剩下的
const NAP: Duration = Duration::from_secs(1);

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
/// `retryDelay`）。都没有是 None，按退避算。说了等于没说的（不到一毫秒的 `retry-after-ms`，
/// 不到一秒的另外两样）当没说，往下看
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
    // 大得离谱的（`1e300`）照最长的算、交给最长暂停封顶：浮点换整数是饱和的，**不能 panic**
    (v.is_finite() && v >= MIN_HINT.as_millis() as f64).then(|| Duration::from_millis(v as u64))
}

/// 一家上游此刻能不能发、对这一个请求来说（[`Throttles::gate`]）。
pub enum Gate {
    /// 能发。暂停刚结束时，这一个是探路的（带着 [`Probe`]）
    Go(Option<Probe>),
    /// 等得到：限流暂停着（`rate_limited`）或者滚动窗口满了（`window_full`）。`until_ms` 是
    /// 预计又能发的那一刻；前面还有人排着、探路的还没有结论时说不出，是 None
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

/// 墙上的时钟，Unix 毫秒
type Wall = Arc<dyn Fn() -> u64 + Send + Sync>;

/// 单调时钟
type Mono = Arc<dyn Fn() -> Instant + Send + Sync>;

/// 同一刻，在两个时钟上
#[derive(Debug, Clone, Copy)]
struct Moment {
    mono: Instant,
    wall: u64,
}

impl Moment {
    /// 从这一刻到 `now` 过了多久：**两个时钟里走得多的那个**。合上盖子睡着的时候单调时钟不走
    /// （macOS、Linux 都是），墙上的照走 —— 睡着的时间要算进去；系统时间被往回拨，墙上的倒退、
    /// 单调的照走 —— 拨回去也拉不长
    fn since(&self, now: Moment) -> Duration {
        now.mono
            .saturating_duration_since(self.mono)
            .max(Duration::from_millis(now.wall.saturating_sub(self.wall)))
    }
}

/// 从某一刻起的一段时间：限流暂停、探路的宽限。**两个时钟哪个先走完就结束**（见
/// [`Moment::since`]）
#[derive(Debug, Clone, Copy)]
struct Span {
    from: Moment,
    len: Duration,
}

impl Span {
    /// 还剩多久。结束了是 0
    fn left(&self, now: Moment) -> Duration {
        self.len.saturating_sub(self.from.since(now))
    }

    fn over(&self, now: Moment) -> bool {
        self.left(now).is_zero()
    }

    /// 结束的那一刻，墙上的（报出去的 `until_ms`）。系统时间被往回拨过 —— 墙上的比单调的少走了
    /// [`SLACK_MS`] 以上 —— 起点在墙上跟着拨回去：报出去的时刻按拨过的算，还要等多久不变，之后
    /// 再报还是这一刻。**不按两个时钟的差现算**：先后读两个时钟，差会抖，报出去的时刻跟着抖的话，
    /// 同一个暂停会报好几次（[`Throttles::refresh`] 比的是报过的样子）
    fn end_ms(&mut self, now: Moment) -> u64 {
        let mono = now
            .mono
            .saturating_duration_since(self.from.mono)
            .as_millis() as i64;
        let wall = now.wall as i64 - self.from.wall as i64;
        if wall.saturating_add(SLACK_MS as i64) < mono {
            self.from.wall = now.wall.saturating_sub(mono as u64);
        }
        self.from.wall.saturating_add(self.len.as_millis() as u64)
    }
}

/// 每家上游的暂停、排队和探路。**跨重载存活**，按上游的名字记。
///
/// 两个时钟（见 [`Moment`]）：暂停、宽限、「最近被限流过」**按两个里走得多的那个算**，合上盖子
/// 睡过去的时间算进去，系统时间往回拨也拉不长。报出去的时刻（`until_ms`、事件的 `at_ms`）、请求
/// 的等待期限、上游说的 HTTP 日期是墙上的（Unix 毫秒）；一跳是什么时候发出去的看单调的
pub struct Throttles {
    ups: Mutex<HashMap<String, Up>>,
    limits: Arc<crate::upstream_limits::UpstreamLimits>,
    bus: tw_observe::EventBus,
    settings: Mutex<Settings>,
    wall: Wall,
    mono: Mono,
    /// 排队的、探路的各自的号
    next: AtomicU64,
    /// 看着用量上限的那些后台任务（[`Self::configure`]），一家一个。**配置换了就全换掉**：
    /// 删掉、改了名的那一家，它的任务不会再被叫醒
    watchers: Mutex<Vec<tokio::task::AbortHandle>>,
}

#[derive(Default)]
struct Settings {
    max_pause: Duration,
    /// 每家的用量上限：报状态、等窗口空出来时要看
    limits: HashMap<String, Vec<KeyLimit>>,
}

/// 一家上游记着的。
struct Up {
    /// 暂停着的话，从哪一刻起、多久
    pause: Option<Span>,
    /// 最近一次收到 429 的时刻。**在它之前发出去的请求的答复不算数**：一阵同时被限流的请求
    /// 只算一次，暂停之前发出去的一个长回答到了响应头，也不说明这家好了
    limited_at: Option<Moment>,
    /// 连续被限流的次数（退避按它算）。拿到别的答复就归零
    strikes: u32,
    /// 暂停结束了、还没有结论：只放一个过去探路
    probing: bool,
    /// 探路的那一个：它的号，和发出去的时刻（还没发出去是 None）
    probe: Option<(u64, Option<Moment>)>,
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
            pause: None,
            limited_at: None,
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

    /// 此刻还在暂停着的话，那一段
    fn paused(&mut self, now: Moment) -> Option<&mut Span> {
        self.pause.as_mut().filter(|p| !p.over(now))
    }

    /// 探路的发出去 [`PROBE_GRACE`] 还没有结论：当这一家好了
    fn settle_probe(&mut self, now: Moment) {
        if self.probing
            && let Some((_, Some(sent))) = self.probe
            && grace(sent).over(now)
        {
            self.probing = false;
            self.probe = None;
            self.bump();
        }
    }

    /// 这个号排着的话离开队
    fn leave(&mut self, id: u64) {
        if let Some(at) = self.queue.iter().position(|w| *w == id) {
            self.queue.remove(at);
            // 队头走了：下一个该看看轮没轮到它
            if at == 0 {
                self.bump();
            }
        }
    }
}

/// `sent` 发出去的探路的那一个的宽限
fn grace(sent: Moment) -> Span {
    Span {
        from: sent,
        len: PROBE_GRACE,
    }
}

/// 拿着这张探路名额的那一家。**认号不认名字**：拿到名额之后这一家可能改了名（[`Throttles::rename`]），
/// 名额跟着走，[`Probe`] 手上的还是旧名字
fn holding(map: &mut HashMap<String, Up>, ticket: u64) -> Option<(&String, &mut Up)> {
    map.iter_mut()
        .find(|(_, up)| matches!(up.probe, Some((t, _)) if t == ticket))
}

impl Throttles {
    pub fn new(
        bus: tw_observe::EventBus,
        limits: Arc<crate::upstream_limits::UpstreamLimits>,
    ) -> Self {
        Self::with_clocks(
            bus,
            limits,
            Arc::new(crate::server::now_ms),
            Arc::new(Instant::now),
        )
    }

    fn with_clocks(
        bus: tw_observe::EventBus,
        limits: Arc<crate::upstream_limits::UpstreamLimits>,
        wall: Wall,
        mono: Mono,
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
            wall,
            mono,
            next: AtomicU64::new(1),
            watchers: Mutex::new(Vec::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Up>> {
        // 锁中毒了照样拿里面的表：少暂停一次，好过一个不转发的网关
        self.ups.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn settings(&self) -> MutexGuard<'_, Settings> {
        self.settings.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 此刻，墙上的时钟（Unix 毫秒）
    fn now(&self) -> u64 {
        (self.wall)()
    }

    /// 此刻，在两个时钟上
    fn moment(&self) -> Moment {
        Moment {
            mono: (self.mono)(),
            wall: self.now(),
        }
    }

    /// 照这份配置换设置：最长暂停多久、每家的用量上限。**暂停着的照旧暂停着**。有用量上限的
    /// 那几家各起一个后台任务看着，状态变了就报（见 [`Self::refresh`]）；上一份配置的那些当场
    /// 收工 —— 删掉、改了名的那一家的用量不会再变，它的任务不收工就一直挂着，重载一次多一批
    pub fn configure(self: &Arc<Self>, cfg: &tw_config::Config) {
        let names: Vec<String> = {
            let mut s = self.settings();
            s.max_pause = Duration::from_secs(cfg.failover.rate_limit_max_pause_secs);
            s.limits = cfg
                .providers
                .iter()
                .filter(|p| !usage_limits(p).is_empty())
                .map(|p| (p.name.clone(), usage_limits(p).to_vec()))
                .collect();
            s.limits.keys().cloned().collect()
        };
        // 配置换了，用量上限可能删了：报一遍现在的样子
        let known: Vec<String> = self.lock().keys().cloned().collect();
        for n in known.iter().chain(&names) {
            self.refresh(n);
        }
        let mut watchers = self.watchers.lock().unwrap_or_else(|p| p.into_inner());
        for w in watchers.drain(..) {
            w.abort();
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        for name in names {
            let me = Arc::downgrade(self);
            let task = rt.spawn(async move {
                loop {
                    // **等的时候不攥着它**：它没了（网关关了）任务跟着收工，见 `Drop`
                    let (limits, nap) = {
                        let Some(t) = me.upgrade() else { return };
                        let nap = t.view(&name).map(|v| t.nap(v.until_ms));
                        (t.limits.clone(), nap)
                    };
                    let wake = async {
                        match nap {
                            Some(d) => tokio::time::sleep(d).await,
                            None => std::future::pending().await,
                        }
                    };
                    tokio::select! {
                        _ = limits.changed(&name) => {}
                        _ = wake => {}
                    }
                    let Some(t) = me.upgrade() else { return };
                    t.refresh(&name);
                    t.bump(&name);
                }
            });
            watchers.push(task.abort_handle());
        }
    }

    /// 等墙上的 `at_ms` 到来，这一次睡多久：离它还有多久，最多 [`NAP`]。已经过了的是 0
    fn nap(&self, at_ms: u64) -> Duration {
        Duration::from_millis(at_ms.saturating_sub(self.now())).min(NAP)
    }

    /// 这一家的用量上限此刻的样子（[`crate::upstream_limits::UpstreamLimits::state`]），时刻换成
    /// 这里的时钟：用量上限按它自己的时钟算（测试会拨它），暂停、等待期限、`Retry-After` 都按
    /// 这里的。两个时钟各读一次；**差不到 [`SLACK_MS`] 的当是同一个**，时刻原样交回 —— 不然先后
    /// 读的两个时钟差一毫秒，报出去的时刻就差一毫秒，同一个样子报好几次
    fn limit_state(&self, name: &str, limits: &[KeyLimit]) -> LimitState {
        let theirs = self.limits.now_ms();
        let mine = self.now() as i64;
        let same = mine.abs_diff(theirs) <= SLACK_MS;
        let convert = |until_ms: u64| {
            if same {
                until_ms
            } else {
                let left = (until_ms as i64).saturating_sub(theirs).max(0);
                mine.saturating_add(left).max(0) as u64
            }
        };
        match self.limits.state(name, limits) {
            LimitState::Free => LimitState::Free,
            LimitState::WindowFull { until_ms, limit } => LimitState::WindowFull {
                until_ms: convert(until_ms),
                limit,
            },
            LimitState::PeriodUsedUp { until_ms, limit } => LimitState::PeriodUsedUp {
                until_ms: convert(until_ms),
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
        let now = self.moment();
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
        if let Some(pause) = up.paused(now) {
            let until = pause.end_ms(now).max(window.unwrap_or(0));
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
                // 探路的已经在路上了（或者拿了名额、正要发）：等它的结论。**什么时候有结论说不出**
                // —— 宽限期的结束只是最晚的那一刻（见 [`PROBE_GRACE`]）
                Some(_) => (
                    Peek::Wait {
                        kind: ThrottleKind::RateLimited,
                        until_ms: None,
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
                        until_ms: Some(now.wall),
                    },
                    None,
                ),
            };
        }
        (Peek::Open, None)
    }

    /// 这一家此刻**可能在限流**：在探路、连续被限流还没有别的答复，或者最近
    /// （[`WARY_FOR`]）被限流过。最后一家的流式回答这时也压着开头（见 `hop`）：流的开头报的
    /// 限流要排队等它恢复，不交给客户端。一直好好的上游不压 —— 客户端照常当场拿到响应头和
    /// 开头的那几个事件
    pub fn wary(&self, name: &str) -> bool {
        let now = self.moment();
        self.lock().get(name).is_some_and(|up| {
            up.probing || up.strikes > 0 || up.limited_at.is_some_and(|t| t.since(now) < WARY_FOR)
        })
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

    /// 发给这一家的、`sent` 发出去的一跳被限流了（回了 429，或者在流的开头报了限流；额度用完
    /// 不算）：暂停这一家。`hint` 是上游说的暂停多久（[`pause_hint`]），没说、说了等于没说
    /// （不到 [`MIN_HINT`]）按退避（[`backoff`]）。**已经暂停着的延到两个里晚的那个**。交回从此刻起
    /// 还要暂停多久
    pub fn limited(
        self: &Arc<Self>,
        name: &str,
        sent: Instant,
        hint: Option<Duration>,
    ) -> Duration {
        let now = self.moment();
        let max = self.settings().max_pause;
        let hint = hint.filter(|d| *d >= MIN_HINT);
        let pause = {
            let mut map = self.lock();
            let up = map.entry(name.to_string()).or_insert_with(Up::new);
            // 上一次 429 之后才发出去的又被限流了，才是连续的又一次。同一阵里被限流的那几个只算
            // 一次
            if up.limited_at.is_none_or(|t| sent > t.mono) {
                up.strikes = up.strikes.saturating_add(1);
            }
            let len = hint.unwrap_or_else(|| backoff(up.strikes)).min(max);
            let fresh = Span { from: now, len };
            let pause = match up.pause {
                Some(p) if p.left(now) >= len => p,
                _ => fresh,
            };
            up.pause = Some(pause);
            up.limited_at = Some(now);
            // 探路的被拒了，或者别人在探路时被拒了：暂停结束后重新探
            up.probing = true;
            up.probe = None;
            up.bump();
            pause
        };
        self.refresh(name);
        // 暂停结束那一刻也是一次变化，没有哪个请求会触发它
        self.at(name, pause);
        pause.left(now)
    }

    /// 发给这一家的、`sent` 发出去的一跳有了不是限流的答复：连续限流的次数归零；在探路的话，
    /// 这一家好了，排着的照并发和用量上限放。**最近一次 429 之前发出去的不算**。
    ///
    /// 答复是什么时候算的看调用方：压着开头的回答（见 `hop`、`ws::turn`）要等到第一段内容 ——
    /// 响应头是 200、流里接着报限流的上游，响应头不说明它好了
    pub fn answered(&self, name: &str, sent: Instant) {
        let changed = {
            let mut map = self.lock();
            let Some(up) = map.get_mut(name) else { return };
            if up.limited_at.is_some_and(|t| sent <= t.mono) {
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

    /// 发给这一家的、`sent` 发出去的一跳**没有结论**：连不上、发出去之后断了、到无响应超时还
    /// 没有内容，或者回的 429 是额度用完、没钱了（等不到，见 [`crate::health`]）。在探路的话不再
    /// 探：排着的照常放，这一家怎么样交给熔断 —— 不然后面的要干等完宽限期，再一拥而上，打到一家
    /// 熔断刚停用的上游上（排着的看得见它停用了，见 [`first_ready`]）。连续限流的次数不动：这不是
    /// 一个不是限流的答复。最近一次 429 之前发出去的不算
    pub fn released(&self, name: &str, sent: Instant) {
        {
            let mut map = self.lock();
            let Some(up) = map.get_mut(name) else { return };
            if !up.probing || up.limited_at.is_some_and(|t| sent <= t.mono) {
                return;
            }
            up.probing = false;
            up.probe = None;
            up.bump();
        }
        self.refresh(name);
    }

    /// 上游改了名（控制面改的）：暂停、退避、排着的队、探路的跟着走，和用量上限的账一样（见
    /// [`crate::upstream_limits::UpstreamLimits::rename`]）。探路的名额跟着走也不会丢：[`Probe`]
    /// 认号不认名字。等旧名字的叫醒：它们手上是改名之前的配置，再看时旧名字下什么都没有，照常发
    pub fn rename(self: &Arc<Self>, from: &str, to: &str) {
        if from == to {
            return;
        }
        let now = self.moment();
        let (pause, sent) = {
            let mut map = self.lock();
            let Some(old) = map.remove(from) else { return };
            old.bump();
            // 新名字下多半什么都没有；改名和重载之间已经有请求用新名字发过的，两份并起来
            let up = map.entry(to.to_string()).or_insert_with(Up::new);
            if up.queue.is_empty() {
                up.queue_kind = old.queue_kind;
            }
            up.pause = match (up.pause, old.pause) {
                (Some(a), Some(b)) => Some(if b.left(now) > a.left(now) { b } else { a }),
                (a, b) => a.or(b),
            };
            up.limited_at = match (up.limited_at, old.limited_at) {
                (Some(a), Some(b)) => Some(if b.mono > a.mono { b } else { a }),
                (a, b) => a.or(b),
            };
            up.strikes = up.strikes.max(old.strikes);
            up.probing |= old.probing;
            if up.probe.is_none() {
                up.probe = old.probe;
            }
            for w in old.queue {
                if !up.queue.contains(&w) {
                    up.queue.push_back(w);
                }
            }
            up.bump();
            (up.pause, up.probe.and_then(|(_, sent)| sent))
        };
        self.refresh(to);
        // 到点时的那一次变化原来挂在旧名字上
        if let Some(pause) = pause.filter(|p| !p.over(now)) {
            self.at(to, pause);
        }
        if let Some(sent) = sent {
            self.at(to, grace(sent));
        }
    }

    /// 交接：每家的限流暂停（见 [`crate::handover`]）。时刻换成「还剩多久」「过去多久」：
    /// 单调时钟出不了这个进程。**排着的队和探路的名额不交**：那是旧 core 手上的请求
    pub(crate) fn handover_export(&self) -> Vec<crate::handover::wire::ThrottleEntry> {
        let now = self.moment();
        let ms = |d: Duration| d.as_millis().min(u128::from(u64::MAX)) as u64;
        self.lock()
            .iter()
            .filter_map(|(name, up)| {
                let pause_left_ms = up.pause.filter(|p| !p.over(now)).map(|p| ms(p.left(now)));
                let limited_ago_ms = up.limited_at.map(|t| ms(t.since(now)));
                if pause_left_ms.is_none()
                    && limited_ago_ms.is_none()
                    && up.strikes == 0
                    && !up.probing
                {
                    return None;
                }
                Some(crate::handover::wire::ThrottleEntry {
                    upstream: name.clone(),
                    pause_left_ms,
                    limited_ago_ms,
                    strikes: up.strikes,
                    probing: up.probing,
                })
            })
            .collect()
    }

    /// 交接：接进旧 core 的限流暂停。`lag_ms` 是快照导出到现在过了多久，从剩下的里扣掉。这里
    /// 已经被限流过的那一家不动 —— 那是更新的事。暂停结束那一刻照样报一次（[`Self::at`]）
    pub(crate) fn handover_import(
        self: &Arc<Self>,
        entries: Vec<crate::handover::wire::ThrottleEntry>,
        lag_ms: u64,
    ) {
        let now = self.moment();
        let back = |ago_ms: u64| {
            let d = Duration::from_millis(ago_ms);
            Moment {
                mono: now.mono.checked_sub(d).unwrap_or(now.mono),
                wall: now.wall.saturating_sub(ago_ms),
            }
        };
        let mut touched = Vec::new();
        {
            let mut map = self.lock();
            for e in entries {
                let up = map.entry(e.upstream.clone()).or_insert_with(Up::new);
                if up.pause.is_some() || up.limited_at.is_some() {
                    continue;
                }
                up.pause = e
                    .pause_left_ms
                    .map(|ms| ms.saturating_sub(lag_ms))
                    .filter(|ms| *ms > 0)
                    .map(|ms| Span {
                        from: now,
                        len: Duration::from_millis(ms),
                    });
                up.limited_at = e.limited_ago_ms.map(|ago| back(ago.saturating_add(lag_ms)));
                up.strikes = e.strikes;
                up.probing = e.probing;
                up.bump();
                touched.push((e.upstream, up.pause));
            }
        }
        for (name, pause) in touched {
            self.refresh(&name);
            if let Some(p) = pause {
                self.at(&name, p);
            }
        }
    }

    /// `span` 结束时重报一次这一家的样子、叫醒排队的。**睡一会儿起来看一眼**（[`NAP`]）：睡着的
    /// 时候单调时钟不走，只按它睡，醒来后还要再等完剩下的
    fn at(self: &Arc<Self>, name: &str, span: Span) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (me, name) = (Arc::downgrade(self), name.to_string());
        rt.spawn(async move {
            loop {
                let left = match me.upgrade() {
                    Some(t) => span.left(t.moment()),
                    None => return,
                };
                if left.is_zero() {
                    break;
                }
                tokio::time::sleep(left.min(NAP)).await;
            }
            if let Some(t) = me.upgrade() {
                t.refresh(&name);
                t.bump(&name);
            }
        });
    }

    /// 这一家此刻暂时发不出去的样子：限流暂停、滚动窗口满了、这一期用完了。**几样都有时给
    /// 又能发得最晚的那一样**。能发是 None。探路那几秒不算：暂停已经结束了
    pub fn view(&self, name: &str) -> Option<tw_api::Throttle> {
        let now = self.moment();
        let paused = self
            .lock()
            .get_mut(name)
            .and_then(|up| up.paused(now).map(|p| p.end_ms(now)))
            .map(|until_ms| tw_api::Throttle {
                kind: ThrottleKind::RateLimited,
                until_ms,
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
            owed: Vec::new(),
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

    /// 离开这一家的队
    fn leave(&self, name: &str, id: u64) {
        if let Some(up) = self.lock().get_mut(name) {
            up.leave(id);
        }
    }

    /// 离开每一家的队。**认号不认名字**：排着的时候这一家可能改了名（[`Self::rename`]）
    fn leave_everywhere(&self, id: u64) {
        for up in self.lock().values_mut() {
            up.leave(id);
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

impl Drop for Throttles {
    fn drop(&mut self) {
        for w in self
            .watchers
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
        {
            w.abort();
        }
    }
}

/// 一个在排队的请求。在哪几家的队里排着，**丢掉就全部离开**：客户端走了、被手动中止了，
/// 后面的不用替它等。
pub struct Waiter {
    throttles: Arc<Throttles>,
    pub id: u64,
    joined: Vec<String>,
    /// 探过路、又被限流了的那几家：下一次等的时候回到它们的队头（[`Self::keep_place`]）
    owed: Vec<String>,
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
    fn join_front(&mut self, name: &str) {
        self.throttles
            .join(name, self.id, ThrottleKind::RateLimited, true);
        if !self.joined.iter().any(|n| n == name) {
            self.joined.push(name.to_string());
        }
    }

    /// 探路的又被限流了：它在这一家的队头的位置留着，**下一次等的时候才回去**（[`first_ready`]）。
    /// 它先去试别的候选、在别处发出去的话，不该在这里占着队头 —— 这一家的暂停结束了，后面的却要
    /// 等它在别处的那一跳
    pub fn keep_place(&mut self, name: &str) {
        if !self.owed.iter().any(|n| n == name) {
            self.owed.push(name.to_string());
        }
    }

    /// 离开这一家的队：不等它了
    fn leave(&mut self, name: &str) {
        if let Some(at) = self.joined.iter().position(|n| n == name) {
            self.joined.remove(at);
            self.throttles.leave(name, self.id);
        }
    }

    /// 离开排着的每一个队：去了一家，或者不等了
    pub fn leave_all(&mut self) {
        if !std::mem::take(&mut self.joined).is_empty() {
            self.throttles.leave_everywhere(self.id);
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
/// 还回去，下一个接着探。
///
/// **名额认号不认名字**（[`holding`]）：拿到名额、还没发出去的时候这一家改了名，名额跟着新名字
/// 走，这里说的发出去、还回去照样落在它身上 —— 不然新名字下一直挂着一张没人管的名额，宽限期
/// 不从它算起，这一家再也放不出一个请求
pub struct Probe {
    throttles: Arc<Throttles>,
    ticket: u64,
    sent: bool,
}

impl Probe {
    /// 这一跳发出去了
    pub fn sent(&mut self) {
        self.sent = true;
        let now = self.throttles.moment();
        let name = {
            let mut map = self.throttles.lock();
            let Some((name, up)) = holding(&mut map, self.ticket) else {
                return;
            };
            if let Some((_, sent)) = &mut up.probe {
                *sent = Some(now);
            }
            up.bump();
            name.clone()
        };
        self.throttles.at(&name, grace(now));
    }

    /// 发出去了、却不作数：被手动中止了、签不了名。名额还回去，下一个接着探
    pub fn withdraw(mut self) {
        self.sent = true;
        if let Some((_, up)) = holding(&mut self.throttles.lock(), self.ticket) {
            up.probe = None;
            up.bump();
        }
    }

    /// 名额还有效：拿到之后这一家没有又被限流
    pub fn valid(&self) -> bool {
        let now = self.throttles.moment();
        holding(&mut self.throttles.lock(), self.ticket)
            .is_some_and(|(_, up)| up.paused(now).is_none())
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        if self.sent {
            return;
        }
        if let Some((_, up)) = holding(&mut self.throttles.lock(), self.ticket)
            && matches!(up.probe, Some((_, None)))
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

/// 等的结局（[`first_ready`]）。
pub(crate) enum Outcome {
    /// 等到了：它在 `names` 里的位置，和占到的
    Ready(usize, Ready),
    /// 到等待期限都没有
    Late,
    /// 不用等了：等着的每一家要么这一期用完了，要么熔断着、额度用完了（还有别的能用时，见
    /// [`first_ready`]）。到期限之前也发不出去
    Gone,
}

/// 在 `names`（每一家和它的用量上限）里等先能发的那一家：不暂停、用量上限没满、排到了它、
/// 有空位。交回它在 `names` 里的位置和 [`Ready`]；等到 `until` 都没有是 [`Outcome::Late`]。
/// **过了 `until` 就不再放**：等到了之后再发出去，就过了这个请求的等待期限。
///
/// 等的时候 `waiter` 排在等得到的那几家的队里（探过路又被限流的那几家排在队头，见
/// [`Waiter::keep_place`]），**等到了、等不到都离开所有的队**：等不到的请求接着去试别的候选，
/// 不该还占着队头。满着的那几家照 [`crate::slots`] 的规矩等空位（不排这里的队）。**同时能发的
/// 按 `names` 的顺序**，也就是候选的顺序。
///
/// 等的时候这一期用完了的不再等：到期限之前也重置不了。熔断着、额度用完了的（[`crate::health`]）
/// 也不再等 —— **还有别的能用时**；全都停用着的照样等，和挑候选时一样（`Health::filter`），一家
/// 都不剩的时候宁可试一试。一家都不剩是 [`Outcome::Gone`]，当场交回
pub(crate) async fn first_ready(
    state: &crate::AppState,
    waiter: &mut Waiter,
    names: &[(&str, &[KeyLimit])],
    until: tokio::time::Instant,
) -> Outcome {
    let got = wait_ready(state, waiter, names, until).await;
    waiter.leave_all();
    got
}

async fn wait_ready(
    state: &crate::AppState,
    waiter: &mut Waiter,
    names: &[(&str, &[KeyLimit])],
    until: tokio::time::Instant,
) -> Outcome {
    let throttles = &state.throttles;
    // 探过路、又被限流了的：回到那几家的队头
    for (name, _) in names {
        if let Some(at) = waiter.owed.iter().position(|n| n == name) {
            waiter.owed.remove(at);
            waiter.join_front(name);
        }
    }
    // 拿到了探路的名额、在等空位的那几家：名额攥着，空出位置就用它发
    let mut held: HashMap<usize, Probe> = HashMap::new();
    loop {
        if tokio::time::Instant::now() >= until {
            return Outcome::Late;
        }
        // **先盯上再看**：看的时候发生的变化不会漏掉
        let mut changes: Vec<watch::Receiver<u64>> =
            names.iter().map(|(n, _)| throttles.watch(n)).collect();
        for c in &mut changes {
            c.borrow_and_update();
        }
        let any_alive = names.iter().any(|(n, _)| state.health.is_available(n));
        let mut slot_waits: Vec<usize> = Vec::new();
        let mut wake: Option<u64> = None;
        let mut gone = 0;
        for (i, (name, limits)) in names.iter().enumerate() {
            if held.get(&i).is_some_and(|p| !p.valid()) {
                held.remove(&i);
            }
            // 停用了：交给熔断照常管，不再等它
            if any_alive && !state.health.is_available(name) {
                held.remove(&i);
                waiter.leave(name);
                gone += 1;
                continue;
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
                    Gate::UsedUp { .. } => {
                        waiter.leave(name);
                        gone += 1;
                        continue;
                    }
                },
            };
            match state.slots.try_take(name) {
                Some(slot) => return Outcome::Ready(i, Ready { slot, probe }),
                None => {
                    if let Some(p) = probe {
                        held.insert(i, p);
                    }
                    slot_waits.push(i);
                }
            }
        }
        if gone == names.len() {
            return Outcome::Gone;
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
                Some(at) => tokio::time::sleep(throttles.nap(at)).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            got = state.slots.first_free(&full, until), if !full.is_empty() => {
                let Some((k, slot)) = got else { return Outcome::Late };
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
                return Outcome::Ready(i, Ready { slot, probe });
            }
            _ = changed => {}
            _ = windows => {}
            _ = woken => {}
            _ = tokio::time::sleep_until(until) => return Outcome::Late,
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

    /// 单调时钟上的零点在墙上 `T0` 之前这么久：测试里说「`T0` 之前一毫秒发出去的」也说得出
    const HEAD: u64 = 3_600_000;

    /// 拨得动的两个时钟
    struct Clocks {
        wall: Arc<AtomicU64>,
        /// 单调时钟，从 `base` 起的毫秒
        mono: Arc<AtomicU64>,
        base: Instant,
    }

    impl Clocks {
        fn new() -> Self {
            Self {
                wall: Arc::new(AtomicU64::new(T0)),
                mono: Arc::new(AtomicU64::new(HEAD)),
                base: Instant::now(),
            }
        }

        fn throttles(&self) -> Arc<Throttles> {
            let bus = tw_observe::EventBus::new();
            let limits = Arc::new(crate::upstream_limits::UpstreamLimits::new(bus.clone()));
            self.throttles_with(bus, limits)
        }

        fn throttles_with(
            &self,
            bus: tw_observe::EventBus,
            limits: Arc<crate::upstream_limits::UpstreamLimits>,
        ) -> Arc<Throttles> {
            let (wall, mono, base) = (self.wall.clone(), self.mono.clone(), self.base);
            Arc::new(Throttles::with_clocks(
                bus,
                limits,
                Arc::new(move || wall.load(Ordering::SeqCst)),
                Arc::new(move || base + Duration::from_millis(mono.load(Ordering::SeqCst))),
            ))
        }

        /// 两个时钟一起走
        fn advance(&self, ms: u64) {
            self.wall.fetch_add(ms, Ordering::SeqCst);
            self.mono.fetch_add(ms, Ordering::SeqCst);
        }

        /// 墙上的此刻
        fn now(&self) -> u64 {
            self.wall.load(Ordering::SeqCst)
        }

        /// 墙上 `ms` 那一刻在单调时钟上是哪一刻（两个时钟一起走、没拨过时）
        fn at(&self, ms: u64) -> Instant {
            self.base + Duration::from_millis(ms + HEAD - T0)
        }

        /// 单调时钟上的此刻
        fn sent(&self) -> Instant {
            self.base + Duration::from_millis(self.mono.load(Ordering::SeqCst))
        }
    }

    fn clocked() -> (Arc<Throttles>, Clocks) {
        let c = Clocks::new();
        (c.throttles(), c)
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
        let (t, c) = clocked();
        let mut got = Vec::new();
        for _ in 0..7 {
            // 每一次都是上一次暂停结束之后才发出去的那一个（探路的）
            c.advance(1);
            let d = t.limited("甲", c.sent(), None);
            got.push(d.as_secs());
            c.advance(d.as_millis() as u64);
        }
        assert_eq!(got, [5, 10, 20, 40, 60, 60, 60]);
        // 拿到一个不是 429 的答复：从头退避
        c.advance(1);
        t.answered("甲", c.sent());
        c.advance(1);
        assert_eq!(t.limited("甲", c.sent(), None), Duration::from_secs(5));
    }

    #[test]
    fn a_burst_of_429s_counts_once_and_a_later_one_extends_the_pause() {
        let (t, c) = clocked();
        // 同一阵发出去的五个都被限流：只算一次，暂停 5 秒
        for _ in 0..5 {
            t.limited("甲", c.at(T0), None);
        }
        assert!(matches!(
            t.peek("甲", &[]),
            Peek::Wait { kind: ThrottleKind::RateLimited, until_ms: Some(u) } if u == T0 + 5_000
        ));
        // 暂停中又一个说了 30 秒：延到晚的那个
        c.advance(1_000);
        let d = t.limited("甲", c.at(T0), Some(Duration::from_secs(30)));
        assert_eq!(d, Duration::from_secs(30));
        // 说得更短的不往前提
        let d = t.limited("甲", c.at(T0), Some(Duration::from_secs(1)));
        assert_eq!(d, Duration::from_secs(30));
        // 最长暂停按配置封顶
        let (t, c) = clocked();
        let d = t.limited("乙", c.at(T0), Some(Duration::from_secs(99_999)));
        assert_eq!(d, Duration::from_secs(3600));
    }

    #[test]
    fn after_a_pause_only_one_request_goes_ahead_until_it_is_answered() {
        let (t, c) = clocked();
        t.limited("甲", c.at(T0), None);
        assert!(!is_go(&t.gate("甲", &[], None)), "暂停中");
        c.advance(5_000);
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
        let sent = c.sent();
        // 什么时候有结论说不出：宽限期的结束不是知道的恢复时刻
        assert!(matches!(
            t.peek("甲", &[]),
            Peek::Wait {
                kind: ThrottleKind::RateLimited,
                until_ms: None
            }
        ));
        assert!(t.keeps_conversation("甲", &[], c.now() + 1_000));
        // 它拿到了响应头，不是 429：放开
        t.answered("甲", sent);
        assert!(matches!(t.gate("甲", &[], None), Gate::Go(None)));
    }

    #[test]
    fn a_probe_that_is_limited_again_pauses_longer_and_one_without_a_verdict_passes_after_the_grace()
     {
        let (t, c) = clocked();
        t.limited("甲", c.at(T0), None);
        c.advance(5_000);
        let Gate::Go(Some(mut probe)) = t.gate("甲", &[], None) else {
            panic!()
        };
        probe.sent();
        // 探路的又被限流：退避接着往上走
        assert_eq!(t.limited("甲", c.sent(), None), Duration::from_secs(10));
        assert!(!probe.valid());
        drop(probe);
        c.advance(10_000);
        let Gate::Go(Some(mut probe)) = t.gate("甲", &[], None) else {
            panic!()
        };
        probe.sent();
        drop(probe);
        // 一直没有结论：10 秒之后当它好了
        c.advance(9_999);
        assert!(!is_go(&t.gate("甲", &[], None)));
        c.advance(1);
        assert!(matches!(t.gate("甲", &[], None), Gate::Go(None)));
    }

    #[test]
    fn a_probe_dropped_before_it_was_sent_lets_the_next_one_go() {
        let (t, c) = clocked();
        t.limited("甲", c.at(T0), None);
        c.advance(5_000);
        let Gate::Go(Some(probe)) = t.gate("甲", &[], None) else {
            panic!()
        };
        // 没占到位置、被中止了：名额还回去
        drop(probe);
        assert!(matches!(t.gate("甲", &[], None), Gate::Go(Some(_))));
    }

    #[test]
    fn an_answer_to_a_request_sent_before_the_429_does_not_end_the_pause() {
        let (t, c) = clocked();
        c.advance(1_000);
        t.limited("甲", c.at(T0 + 1_000), None);
        c.advance(5_000);
        // 暂停之前发出去的长回答这时才到响应头：不说明这家好了
        t.answered("甲", c.at(T0));
        assert!(matches!(t.gate("甲", &[], None), Gate::Go(Some(_))));
        // **按单调时钟比**，不差一毫秒：429 那一刻之后才发出去的，哪怕只晚一点点，也是新的一跳
        let (t, c) = clocked();
        t.limited("甲", c.sent(), None);
        let after = c.sent() + Duration::from_micros(1);
        c.advance(5_000);
        let Gate::Go(Some(mut probe)) = t.gate("甲", &[], None) else {
            panic!()
        };
        probe.sent();
        t.answered("甲", after);
        assert!(matches!(t.gate("甲", &[], None), Gate::Go(None)));
    }

    #[test]
    fn the_queue_lets_them_go_in_order_and_a_leaver_makes_room() {
        let (t, c) = clocked();
        t.limited("甲", c.at(T0), None);
        let (mut a, mut b) = (t.waiter(), t.waiter());
        a.join("甲", ThrottleKind::RateLimited);
        b.join("甲", ThrottleKind::RateLimited);
        assert_eq!(t.queue("甲"), [a.id, b.id]);
        c.advance(5_000);
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
        // 探路的又被限流：它回到队头
        t.limited("甲", c.sent(), None);
        a.join_front("甲");
        assert_eq!(t.queue("甲"), [a.id, b.id]);
        // a 不等了（客户端走了）：b 排到了头
        drop(a);
        assert_eq!(t.queue("甲"), [b.id]);
        c.advance(10_000);
        assert!(matches!(t.gate("甲", &[], Some(b.id)), Gate::Go(Some(_))));
    }

    #[tokio::test]
    async fn every_change_is_reported_once() {
        let (t, c) = clocked();
        let mut rx = t.bus.subscribe();
        t.limited("甲", c.at(T0), None);
        t.limited("甲", c.at(T0), None);
        c.advance(5_000);
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

    /// 用量上限的时钟：比墙上的慢 `behind` 毫秒
    struct Behind {
        wall: Arc<AtomicU64>,
        behind: u64,
    }

    impl crate::key_limits::Clock for Behind {
        fn now_ms(&self) -> i64 {
            (self.wall.load(Ordering::SeqCst) - self.behind) as i64
        }
        fn period(&self, per: tw_config::LimitPer, at_ms: i64) -> (i64, i64) {
            crate::key_limits::SystemClock.period(per, at_ms)
        }
        fn show(&self, at_ms: i64) -> String {
            crate::key_limits::SystemClock.show(at_ms)
        }
    }

    /// 用量上限的时钟和这里的先后读，差一毫秒：报出去的时刻**不跟着抖**，同一个样子只报一次
    #[tokio::test]
    async fn a_full_window_is_reported_once_although_the_two_clocks_are_read_apart() {
        let c = Clocks::new();
        let bus = tw_observe::EventBus::new();
        let limits = Arc::new(crate::upstream_limits::UpstreamLimits::with_clock(
            bus.clone(),
            Arc::new(Behind {
                wall: c.wall.clone(),
                behind: 1,
            }),
        ));
        let t = c.throttles_with(bus.clone(), limits.clone());
        let set: Vec<KeyLimit> = serde_yaml_ng::from_str("[{per: minute, requests: 1}]").unwrap();
        let cfg = tw_config::Config {
            providers: vec![tw_config::Provider {
                name: "甲".into(),
                base_url: "https://a.example".into(),
                limits: set.clone(),
                ..Default::default()
            }],
            ..Default::default()
        };
        limits.configure(&cfg);
        t.configure(&cfg);
        let mut rx = bus.subscribe();
        let _hold = limits.reserve("甲", &set, &crate::key_limits::Ask::default());
        let until = T0 - 1 + 60_000;
        for _ in 0..3 {
            assert_eq!(t.view("甲").map(|v| v.until_ms), Some(until));
            t.refresh("甲");
            c.advance(1);
        }
        let shown: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|ev| match ev {
                tw_api::Event::ThrottleChanged { throttle, .. } => Some(throttle),
                _ => None,
            })
            .collect();
        assert_eq!(shown.len(), 1, "{shown:?}");
    }

    /// 重载一次不多一批后台任务：上一份配置的那些收工，删掉了用量上限的那一家不留任务
    #[tokio::test]
    async fn reloading_does_not_pile_up_watchers() {
        let (t, _c) = clocked();
        let with = |limits: &str| tw_config::Config {
            providers: vec![tw_config::Provider {
                name: "甲".into(),
                base_url: "https://a.example".into(),
                limits: serde_yaml_ng::from_str(limits).unwrap(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let alive = || async {
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            tokio::runtime::Handle::current()
                .metrics()
                .num_alive_tasks()
        };
        let before = alive().await;
        for _ in 0..5 {
            t.configure(&with("[{per: minute, requests: 1}]"));
        }
        assert_eq!(alive().await, before + 1);
        // 用量上限删了（改名、删掉上游也一样）：它的任务不再被叫醒，要当场收工
        t.configure(&with("[]"));
        assert_eq!(alive().await, before);
        // 网关没了，任务跟着收工
        t.configure(&with("[{per: minute, requests: 1}]"));
        assert_eq!(alive().await, before + 1);
        drop(t);
        assert_eq!(alive().await, before);
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

    #[test]
    fn a_hint_that_says_nothing_falls_back_to_the_backoff_and_an_absurd_one_is_capped() {
        let (t, c) = clocked();
        // `retry-after-ms: 0`、不到一毫秒：照它暂停等于不暂停，按退避算
        assert_eq!(
            t.limited("甲", c.at(T0), Some(Duration::ZERO)),
            Duration::from_secs(5)
        );
        c.advance(5_000);
        assert_eq!(
            t.limited("甲", c.at(T0 + 5_000), Some(Duration::from_micros(400))),
            Duration::from_secs(10)
        );
        let h = headers(&[("retry-after-ms", "0"), ("retry-after", "0")]);
        assert_eq!(pause_hint(&h, b"", T0), None);
        let h = headers(&[("retry-after-ms", "0.4"), ("retry-after", "3")]);
        assert_eq!(pause_hint(&h, b"", T0), Some(Duration::from_secs(3)));
        // 大得离谱的不 panic，照最长暂停封顶
        let h = headers(&[("retry-after-ms", "1e300")]);
        let hint = pause_hint(&h, b"", T0);
        assert!(hint.is_some_and(|d| d > Duration::from_secs(3600)));
        let (t, c) = clocked();
        assert_eq!(t.limited("乙", c.at(T0), hint), Duration::from_secs(3600));
    }

    #[test]
    fn the_pause_runs_on_the_monotonic_clock_when_the_system_time_moves_back() {
        let (t, c) = clocked();
        assert_eq!(t.limited("甲", c.sent(), None), Duration::from_secs(5));
        // 系统时间往回拨了一小时：暂停照样只剩 5 秒，报出去的时刻按拨过的算
        c.wall.fetch_sub(3_600_000, Ordering::SeqCst);
        assert!(matches!(
            t.peek("甲", &[]),
            Peek::Wait { until_ms: Some(u), .. } if u == T0 - 3_600_000 + 5_000
        ));
        assert_eq!(t.view("甲").unwrap().until_ms, T0 - 3_600_000 + 5_000);
        c.mono.fetch_add(4_999, Ordering::SeqCst);
        assert!(!is_go(&t.gate("甲", &[], None)));
        c.mono.fetch_add(1, Ordering::SeqCst);
        let Gate::Go(Some(mut probe)) = t.gate("甲", &[], None) else {
            panic!("暂停该结束了");
        };
        // 探路的宽限也拉不长：系统时间再往回拨，宽限照样 10 秒
        probe.sent();
        c.wall.fetch_sub(3_600_000, Ordering::SeqCst);
        c.mono.fetch_add(10_000, Ordering::SeqCst);
        assert!(matches!(t.gate("甲", &[], None), Gate::Go(None)));
    }

    /// 合上盖子睡着的时候单调时钟不走、墙上的走：**睡过去的时间算进暂停**，醒来时该结束的已经
    /// 结束了，不再等完剩下的（最长要等一小时）
    #[test]
    fn a_pause_slept_through_is_over_when_the_machine_wakes() {
        let (t, c) = clocked();
        assert_eq!(
            t.limited("甲", c.sent(), Some(Duration::from_secs(600))),
            Duration::from_secs(600)
        );
        // 睡了五分钟：还剩五分钟，报出去的结束时刻不变
        c.wall.fetch_add(300_000, Ordering::SeqCst);
        assert!(matches!(
            t.peek("甲", &[]),
            Peek::Wait { until_ms: Some(u), .. } if u == T0 + 600_000
        ));
        // 醒着又过了五分钟（两个时钟一起走）：结束
        c.advance(299_999);
        assert!(!is_go(&t.gate("甲", &[], None)));
        c.advance(1);
        assert!(t.view("甲").is_none());
        let Gate::Go(Some(mut probe)) = t.gate("甲", &[], None) else {
            panic!("暂停该结束了");
        };
        // 一下子睡过整个暂停：醒来就结束
        probe.sent();
        let sent = c.sent();
        c.advance(1);
        assert_eq!(
            t.limited(
                "甲",
                sent + Duration::from_millis(1),
                Some(Duration::from_secs(3_000))
            ),
            Duration::from_secs(3_000)
        );
        c.wall.fetch_add(3_000_000, Ordering::SeqCst);
        assert!(t.view("甲").is_none());
        let Gate::Go(Some(mut probe)) = t.gate("甲", &[], None) else {
            panic!("睡过了整个暂停，醒来就该结束");
        };
        // 探路的宽限也一样：发出去之后睡着了，醒来就过了宽限
        probe.sent();
        assert!(!is_go(&t.gate("甲", &[], None)));
        c.wall.fetch_add(10_000, Ordering::SeqCst);
        assert!(matches!(t.gate("甲", &[], None), Gate::Go(None)));
    }

    #[test]
    fn a_probe_without_a_verdict_hands_the_queue_back_without_waiting_for_the_grace() {
        let (t, c) = clocked();
        t.limited("甲", c.at(T0), None);
        c.advance(5_000);
        let Gate::Go(Some(mut probe)) = t.gate("甲", &[], None) else {
            panic!()
        };
        probe.sent();
        let sent = c.sent();
        assert!(!is_go(&t.gate("甲", &[], None)));
        // 暂停之前发出去的那一跳连不上：不是探路的，不算
        t.released("甲", c.at(T0 - 1));
        assert!(!is_go(&t.gate("甲", &[], None)));
        // 探路的连不上（或者回的是额度用完）：不再探，排着的照常放，交给熔断
        t.released("甲", sent);
        assert!(matches!(t.gate("甲", &[], None), Gate::Go(None)));
        // 连续限流的次数没有归零：下一次接着往上退避
        c.advance(1);
        assert_eq!(t.limited("甲", c.sent(), None), Duration::from_secs(10));
        // 被中止的探路：名额还回去，下一个接着探
        c.advance(10_000);
        let Gate::Go(Some(mut probe)) = t.gate("甲", &[], None) else {
            panic!()
        };
        probe.sent();
        assert!(!is_go(&t.gate("甲", &[], None)));
        probe.withdraw();
        assert!(matches!(t.gate("甲", &[], None), Gate::Go(Some(_))));
    }

    #[test]
    fn an_upstream_is_wary_while_probing_and_for_a_while_after_a_429() {
        let (t, c) = clocked();
        assert!(!t.wary("甲"), "没被限流过");
        t.limited("甲", c.at(T0), None);
        assert!(t.wary("甲"));
        c.advance(5_000);
        t.answered("甲", c.at(T0 + 5_000));
        // 探路的过了、连续的次数归零：最近被限流过，还要当心
        assert!(t.wary("甲"));
        c.advance(295_000);
        assert!(!t.wary("甲"));
    }

    #[tokio::test]
    async fn a_renamed_upstream_keeps_its_pause_and_its_queue() {
        let (t, c) = clocked();
        let mut rx = t.bus.subscribe();
        t.limited("甲", c.at(T0), Some(Duration::from_secs(30)));
        let mut a = t.waiter();
        a.join("甲", ThrottleKind::RateLimited);
        let old = t.watch("甲");
        t.rename("甲", "乙");
        // 暂停和队跟着新名字走；旧名字下什么都没有了
        assert!(matches!(
            t.peek("乙", &[]),
            Peek::Wait { kind: ThrottleKind::RateLimited, until_ms: Some(u) } if u == T0 + 30_000
        ));
        assert_eq!(t.queue("乙"), [a.id]);
        assert_eq!(t.peek("甲", &[]), Peek::Open);
        // 等旧名字的被叫醒了
        assert!(old.has_changed().unwrap_or(true));
        // 新名字报一次现在的样子
        let mut shown = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let tw_api::Event::ThrottleChanged {
                provider, throttle, ..
            } = ev
            {
                shown.push((provider, throttle.map(|t| t.until_ms)));
            }
        }
        assert_eq!(
            shown.last(),
            Some(&("乙".to_string(), Some(T0 + 30_000))),
            "{shown:?}"
        );
        // 它排着的时候改了名：离开时照样从新名字的队里走掉
        drop(a);
        assert!(t.queue("乙").is_empty());
        // 退避跟着走：暂停结束后再被限流是第二次
        c.advance(30_000);
        assert_eq!(
            t.limited("乙", c.at(T0 + 30_000), None),
            Duration::from_secs(10)
        );
    }

    /// 探路的名额拿到了、还没发出去时改了名：名额跟着新名字走，[`Probe`] 手上的是旧名字也照样
    /// 落在它身上 —— 发出去从那一刻起算宽限，没发出去就还回去
    #[test]
    fn a_probe_ticket_follows_a_rename() {
        for send in [false, true] {
            let (t, c) = clocked();
            t.limited("甲", c.at(T0), None);
            c.advance(5_000);
            let Gate::Go(Some(mut probe)) = t.gate("甲", &[], None) else {
                panic!()
            };
            t.rename("甲", "乙");
            assert!(probe.valid());
            assert!(!is_go(&t.gate("乙", &[], None)), "探路的还没有结论");
            if send {
                probe.sent();
                drop(probe);
                c.advance(PROBE_GRACE.as_millis() as u64);
                assert!(matches!(t.gate("乙", &[], None), Gate::Go(None)));
            } else {
                drop(probe);
                assert!(matches!(t.gate("乙", &[], None), Gate::Go(Some(_))));
            }
        }
    }
}
