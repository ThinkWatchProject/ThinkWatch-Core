//! 熔断的状态机。
//!
//! 三处在用：桌面版的上游健康、企业版的路由健康、企业版的 MCP 服务器。
//! 三处的**前提**不一样 —— 桌面版一个用户、进程内、全部熔断时照样放行；
//! 企业版多副本共享 Redis、按错误率判断、熔断了就把路由过滤掉 —— 但状态机
//! 本身是同一个：闭合、打开、冷却到点之后半开、半开时的结果决定开合。
//!
//! 所以这里只放状态机，不放前提：
//!
//! - **状态是纯数据**（[`Breaker`]，可以序列化），**转换是纯函数**，时间由
//!   调用方给。放在一把锁后面（[`Breakers`]）还是放进 Redis，由调用方定。
//! - **触发条件二选一**（[`Trip`]）：连续失败 N 次，或者窗口里的错误率。
//!   窗口怎么统计是存储那边的事，这里只要统计结果（[`Tally`]）。
//! - 「全部熔断了怎么办」「只有一个候选时要不要熔断」是调用方的策略，不在
//!   这里。
//!
//! # 三条规则，对所有调用方一样
//!
//! 1. **冷却到点就是半开**，不需要谁来「推」它一下。以前企业版要等一个请求
//!    落在这条路由上才会从打开变半开，而打开的路由根本选不到 —— 它就一直
//!    开着，直到存储里那个键过期。
//! 2. **半开时放行，结果决定开合**：成功够 `probes` 次就闭合，失败一次就
//!    重新打开。
//! 3. **打开期间收到的结果也算数**：熔断之前就发出去的请求晚到了。成功按
//!    一次探测记，失败则把冷却重新计起 —— 它说明上游还是坏的。

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    #[default]
    Closed,
    /// 熔断中，不放行
    Open,
    /// 冷却到点了，放行，下一个结果决定开合
    HalfOpen,
}

/// 什么时候打开。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trip {
    /// 连续失败这么多次
    Consecutive(u32),
    /// 窗口里至少有 `min_samples` 个结果，且错误占比达到 `percent`
    ErrorRate { percent: u32, min_samples: u32 },
}

/// 一个熔断器的规矩。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub trip: Trip,
    /// 打开之后多久变半开
    pub cooldown: Duration,
    /// 半开时要成功多少次才闭合。至少 1
    pub probes: u32,
}

/// 窗口里的统计，**包含这一次结果在内**。按错误率触发时要它；按连续失败
/// 触发时给默认值就行。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub total: u32,
    pub errors: u32,
}

/// 一个熔断器知道的全部东西。
///
/// **是纯数据**：放在锁后面，或者整个写进 Redis，都行。存储里读不出来的
/// （键过期了、格式不认识）就当 `Default` —— 闭合、什么都不记得，那是最
/// 保守的起点。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Breaker {
    #[serde(default)]
    pub state: State,
    /// 最近一次打开的时刻，Unix 毫秒。只在打开 / 半开时有意义
    #[serde(default)]
    pub opened_at_ms: i64,
    /// 连续失败了几次
    #[serde(default)]
    pub failures: u32,
    /// 半开以来成功了几次
    #[serde(default)]
    pub probe_successes: u32,
}

impl Breaker {
    /// 此刻是什么状态。**冷却到点的打开就是半开**，不改任何东西 —— 选路时
    /// 用它，不必为了看一眼状态去写存储。
    pub fn state_at(&self, p: &Policy, now_ms: i64) -> State {
        match self.state {
            State::Open if self.cooled(p, now_ms) => State::HalfOpen,
            s => s,
        }
    }

    /// 现在放不放行。
    pub fn admits(&self, p: &Policy, now_ms: i64) -> bool {
        self.state_at(p, now_ms) != State::Open
    }

    /// 放一个请求过去，**并把「冷却到点」落成半开**。状态变了就返回新状态。
    ///
    /// 要把半开这一步报出去的调用方用它（企业版的 MCP 熔断把它推给仪表盘）；
    /// 只想知道能不能放的用 [`Self::admits`]。
    pub fn admit(&mut self, p: &Policy, now_ms: i64) -> (bool, Option<State>) {
        if self.state == State::Open && self.cooled(p, now_ms) {
            self.state = State::HalfOpen;
            self.probe_successes = 0;
            return (true, Some(State::HalfOpen));
        }
        (self.state != State::Open, None)
    }

    /// 还要等多久才半开。
    ///
    /// `None`：不在打开状态。`Some(0)`：冷却走完了，下一个请求就是探测。
    /// 要按时报「恢复」的定时器靠它分清「到点了」和「中途又失败、冷却被
    /// 顶到更晚了」。
    pub fn cooldown_left(&self, p: &Policy, now_ms: i64) -> Option<Duration> {
        if self.state != State::Open {
            return None;
        }
        let elapsed = u64::try_from(now_ms.saturating_sub(self.opened_at_ms)).unwrap_or(0);
        Some(p.cooldown.saturating_sub(Duration::from_millis(elapsed)))
    }

    /// 记一次结果。**返回的是状态变化，没变就是 `None`** —— 调用方拿它去发
    /// 事件，而一个每次失败都报一条的事件就退化成了另一个请求流。
    ///
    /// 比较的是此刻的状态（[`Self::state_at`]）：冷却到点之后那次探测又失败，
    /// 会再报一次 `Open` —— 中间确实经过了一段「可以再试」的时间。
    pub fn record(&mut self, ok: bool, tally: Tally, p: &Policy, now_ms: i64) -> Option<State> {
        let before = self.state_at(p, now_ms);
        match (before, ok) {
            (State::Closed, true) => self.failures = 0,
            (State::Closed, false) => {
                self.failures += 1;
                if trips(p.trip, self.failures, tally) {
                    self.open(now_ms);
                }
            }
            (State::HalfOpen | State::Open, true) => {
                self.probe_successes += 1;
                if self.probe_successes >= p.probes.max(1) {
                    *self = Breaker::default();
                } else {
                    self.state = State::HalfOpen;
                }
            }
            (State::HalfOpen | State::Open, false) => {
                self.failures += 1;
                self.open(now_ms);
            }
        }
        let after = self.state_at(p, now_ms);
        (after != before).then_some(after)
    }

    fn open(&mut self, now_ms: i64) {
        self.state = State::Open;
        self.opened_at_ms = now_ms;
        self.probe_successes = 0;
    }

    fn cooled(&self, p: &Policy, now_ms: i64) -> bool {
        let cooldown = i64::try_from(p.cooldown.as_millis()).unwrap_or(i64::MAX);
        now_ms.saturating_sub(self.opened_at_ms) >= cooldown
    }
}

fn trips(t: Trip, failures: u32, tally: Tally) -> bool {
    match t {
        Trip::Consecutive(n) => failures >= n.max(1),
        Trip::ErrorRate {
            percent,
            min_samples,
        } => {
            tally.total >= min_samples.max(1)
                && u64::from(tally.errors) * 100 >= u64::from(percent) * u64::from(tally.total)
        }
    }
}

/// 现在是 Unix 毫秒几。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// 一组放在进程内存里的熔断器，按 `K` 区分。
///
/// **不持久化。**进程内的熔断器活多久就记多久；要跨副本共享的调用方
/// 把 [`Breaker`] 自己存到别处。
///
/// 只用于按连续失败触发的熔断器 —— 按错误率触发要一个窗口，而窗口怎么统计
/// 是存储的事。
pub struct Breakers<K> {
    policy: Policy,
    map: Mutex<HashMap<K, Breaker>>,
    clock: Clock,
}

impl<K: Eq + Hash + Clone> Breakers<K> {
    pub fn new(policy: Policy) -> Self {
        Self::with_clock(policy, Arc::new(now_ms))
    }

    /// 时间由调用方给。测试用它把时间拨快，不必真等冷却。
    pub fn with_clock(policy: Policy, clock: Clock) -> Self {
        Self {
            policy,
            map: Mutex::new(HashMap::new()),
            clock,
        }
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    fn with<R>(&self, key: &K, f: impl FnOnce(&mut Breaker, &Policy, i64) -> R) -> R {
        let now = (self.clock)();
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let b = map.entry(key.clone()).or_default();
        f(b, &self.policy, now)
    }

    pub fn state(&self, key: &K) -> State {
        let now = (self.clock)();
        let map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        map.get(key)
            .map_or(State::Closed, |b| b.state_at(&self.policy, now))
    }

    pub fn admits(&self, key: &K) -> bool {
        self.state(key) != State::Open
    }

    /// 见 [`Breaker::admit`]。
    pub fn admit(&self, key: &K) -> (bool, Option<State>) {
        self.with(key, |b, p, now| b.admit(p, now))
    }

    pub fn record(&self, key: &K, ok: bool) -> Option<State> {
        self.with(key, |b, p, now| b.record(ok, Tally::default(), p, now))
    }

    pub fn cooldown_left(&self, key: &K) -> Option<Duration> {
        let now = (self.clock)();
        let map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        map.get(key)?.cooldown_left(&self.policy, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    const SEC: i64 = 1000;

    fn consecutive(n: u32, probes: u32) -> Policy {
        Policy {
            trip: Trip::Consecutive(n),
            cooldown: Duration::from_secs(60),
            probes,
        }
    }

    fn rate() -> Policy {
        Policy {
            trip: Trip::ErrorRate {
                percent: 50,
                min_samples: 4,
            },
            cooldown: Duration::from_secs(30),
            probes: 1,
        }
    }

    fn fail(b: &mut Breaker, p: &Policy, now: i64) -> Option<State> {
        b.record(false, Tally::default(), p, now)
    }

    #[test]
    fn only_the_failure_that_flips_it_reports_a_change() {
        let p = consecutive(3, 1);
        let mut b = Breaker::default();
        assert_eq!(fail(&mut b, &p, 0), None);
        assert_eq!(fail(&mut b, &p, 0), None);
        assert_eq!(fail(&mut b, &p, 0), Some(State::Open), "第三次才是变化");
        assert_eq!(fail(&mut b, &p, 1), None, "已经开着了");
    }

    #[test]
    fn one_success_wipes_the_streak() {
        // 「连续」是关键词。断续的失败说明是偶发
        let p = consecutive(3, 1);
        let mut b = Breaker::default();
        fail(&mut b, &p, 0);
        fail(&mut b, &p, 0);
        b.record(true, Tally::default(), &p, 0);
        fail(&mut b, &p, 0);
        assert!(b.admits(&p, 0));
    }

    #[test]
    fn a_cooled_breaker_is_half_open_without_anyone_touching_it() {
        // 以前企业版要等一个请求落在这条路由上才半开，而打开的路由选不到
        let p = consecutive(1, 1);
        let mut b = Breaker::default();
        fail(&mut b, &p, 0);
        assert_eq!(b.state_at(&p, 59 * SEC), State::Open);
        assert_eq!(b.state_at(&p, 60 * SEC), State::HalfOpen);
        assert!(b.admits(&p, 60 * SEC));
    }

    #[test]
    fn a_probe_decides() {
        let p = consecutive(1, 1);
        let mut b = Breaker::default();
        fail(&mut b, &p, 0);
        // 探测失败：重新打开，这是一次变化（中间确实可以再试过）
        assert_eq!(fail(&mut b, &p, 60 * SEC), Some(State::Open));
        assert_eq!(b.cooldown_left(&p, 60 * SEC), Some(Duration::from_secs(60)));
        // 探测成功：闭合
        assert_eq!(
            b.record(true, Tally::default(), &p, 120 * SEC),
            Some(State::Closed)
        );
        assert_eq!(b, Breaker::default());
    }

    #[test]
    fn several_probes_can_be_required() {
        let p = consecutive(1, 3);
        let mut b = Breaker::default();
        fail(&mut b, &p, 0);
        let ok = |b: &mut Breaker| b.record(true, Tally::default(), &p, 60 * SEC);
        assert_eq!(ok(&mut b), None, "还是半开");
        assert_eq!(b.state, State::HalfOpen, "半开落进了状态里");
        assert_eq!(ok(&mut b), None);
        assert_eq!(ok(&mut b), Some(State::Closed));
    }

    #[test]
    fn a_late_failure_while_open_restarts_the_cooldown() {
        // 熔断之前发出去的请求晚到了，又失败 —— 上游还是坏的
        let p = consecutive(1, 1);
        let mut b = Breaker::default();
        fail(&mut b, &p, 0);
        assert_eq!(fail(&mut b, &p, 30 * SEC), None);
        assert_eq!(b.cooldown_left(&p, 30 * SEC), Some(Duration::from_secs(60)));
    }

    #[test]
    fn a_late_success_while_open_counts_as_a_probe() {
        let p = consecutive(1, 1);
        let mut b = Breaker::default();
        fail(&mut b, &p, 0);
        assert_eq!(
            b.record(true, Tally::default(), &p, SEC),
            Some(State::Closed)
        );
    }

    #[test]
    fn admit_lands_the_half_open_step_once() {
        let p = consecutive(1, 2);
        let mut b = Breaker::default();
        fail(&mut b, &p, 0);
        assert_eq!(b.admit(&p, SEC), (false, None));
        assert_eq!(b.admit(&p, 60 * SEC), (true, Some(State::HalfOpen)));
        assert_eq!(b.admit(&p, 61 * SEC), (true, None));
    }

    #[test]
    fn an_error_rate_needs_enough_samples() {
        let p = rate();
        let mut b = Breaker::default();
        let t = |total, errors| Tally { total, errors };
        assert_eq!(b.record(false, t(3, 3), &p, 0), None, "样本不够");
        assert_eq!(b.record(false, t(4, 2), &p, 0), Some(State::Open));
    }

    #[test]
    fn an_error_rate_below_the_line_stays_closed() {
        let p = rate();
        let mut b = Breaker::default();
        assert_eq!(
            b.record(
                false,
                Tally {
                    total: 10,
                    errors: 4
                },
                &p,
                0
            ),
            None
        );
        assert_eq!(b.state, State::Closed);
    }

    #[test]
    fn it_round_trips_through_json_and_forgets_on_garbage() {
        let p = consecutive(1, 1);
        let mut b = Breaker::default();
        fail(&mut b, &p, 5);
        let back: Breaker = serde_json::from_str(&serde_json::to_string(&b).unwrap()).unwrap();
        assert_eq!(back, b);
        // 状态名是 snake_case，和企业版界面读到的一样
        assert_eq!(
            serde_json::to_string(&State::HalfOpen).unwrap(),
            "\"half_open\""
        );
        // 缺字段就是默认
        let empty: Breaker = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, Breaker::default());
    }

    #[test]
    fn breakers_keep_one_per_key_on_a_clock_the_caller_controls() {
        let now = Arc::new(AtomicI64::new(0));
        let clock = {
            let now = now.clone();
            Arc::new(move || now.load(Ordering::SeqCst))
        };
        let bs = Breakers::with_clock(consecutive(2, 1), clock);
        assert_eq!(bs.record(&"a", false), None);
        assert_eq!(bs.record(&"a", false), Some(State::Open));
        assert!(!bs.admits(&"a"));
        assert!(bs.admits(&"b"), "一个键一个");
        now.store(60 * SEC, Ordering::SeqCst);
        assert_eq!(bs.state(&"a"), State::HalfOpen);
        assert_eq!(bs.admit(&"a"), (true, Some(State::HalfOpen)));
        assert_eq!(bs.record(&"a", true), Some(State::Closed));
    }
}
