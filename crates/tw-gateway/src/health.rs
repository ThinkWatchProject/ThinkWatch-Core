//! 上游健康与熔断。
//!
//! 状态机是共用的（`tw-breaker`，企业版的路由和 MCP 熔断跑的是同一个），
//! 这里是桌面版的**规矩**：
//!
//! **停用多久看失败的原因**（见 [`crate::failure`]）。上游说了原因的 —— 余额
//! 不足、额度用完、限流并给了 `Retry-After` —— 一次就停用，停到那个原因该过去
//! 的时候；说不出原因的（5xx、连不上）连续几次才停，停的时长每次翻倍，成功一次
//! 就回到起点。几个数都在配置的 `failover` 里。
//!
//! 两条硬边界，它们比参数重要得多：
//!
//! 1. **全部候选都被熔断时放行，而不是拒绝。**服务器还能指望「换个实例
//!    试试」，单用户桌面没有第二条路 —— 宁可放行到一个可能坏的上游让用户
//!    看见真实错误，也不要返回一个我们自己编的「无可用上游」。
//! 2. **只有一个候选时完全旁路熔断器。**否则唯一的上游一旦被自己熔断，
//!    就把用户锁死了，熔断纯粹是自伤。
//!
//! 同一批成败还记成每家最近的成功率（[`Health::success_rates`]），`load-balance`
//! 按成败分新对话时看它。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use tw_breaker::State;
use tw_breaker::{Breaker, Policy, Tally, Trip};

use crate::failure::Cause;

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// 一家上游记着的东西。
#[derive(Debug, Clone, Copy, Default)]
struct Entry {
    /// 说不出原因的失败走它：连续几次打开，冷却到点放一个探测过去
    breaker: Breaker,
    /// 它打开过几次（中间没有成功过）。冷却按它翻倍
    trips: u32,
    /// 上游说了原因的停用，到这一刻为止（Unix 毫秒）。**到点了也留着**，
    /// 直到下一次成功 —— 报「恢复」的定时器要靠它知道这家曾经停过
    held_until_ms: Option<i64>,
}

/// 成功率看最近多少次。
const RECENT_MAX: usize = 50;
/// 也只看最近这么久（毫秒）：**一家恢复了，半小时前的失败不该还压着它的份额**。
/// 请求稀疏的时候，按次数的窗口会把很久以前的事一直留着
const RECENT_MAX_AGE_MS: i64 = 30 * 60 * 1000;
/// 少于这么多次就说不出成功率。头一两次失败可能是碰巧，不该就此少分到新对话
const RECENT_MIN_SAMPLES: u32 = 5;

/// 每家最近的成败：最近 [`RECENT_MAX`] 次、[`RECENT_MAX_AGE_MS`] 以内。
///
/// **判据就是熔断的判据**：`record_*` 记一次，这里也记一次（见 [`Health::success_rates`]）。
#[derive(Default)]
struct Recent(HashMap<String, VecDeque<(i64, bool)>>);

impl Recent {
    fn record(&mut self, name: &str, ok: bool, now: i64) {
        let w = self.0.entry(name.to_string()).or_default();
        if w.len() == RECENT_MAX {
            w.pop_front();
        }
        w.push_back((now, ok));
    }

    /// 窗口里的统计。过了时的不算
    fn tally(&self, name: &str, now: i64) -> Tally {
        self.0
            .get(name)
            .into_iter()
            .flatten()
            .filter(|(at, _)| now.saturating_sub(*at) < RECENT_MAX_AGE_MS)
            .fold(Tally::default(), |t, (_, ok)| Tally {
                total: t.total + 1,
                errors: t.errors + u32::from(!ok),
            })
    }

    /// 成功率，0 到 1。样本不够是 `None`
    fn rate(&self, name: &str, now: i64) -> Option<f64> {
        let t = self.tally(name, now);
        (t.total >= RECENT_MIN_SAMPLES).then(|| f64::from(t.total - t.errors) / f64::from(t.total))
    }
}

/// 每个 provider 的健康状态。
///
/// **不持久化**。桌面应用重启频繁，把「这家挂了」的判断带过重启
/// 意味着用户重启后第一个请求还在被上一次的故障惩罚 —— 而重启本身往往
/// 就是他为了解决问题做的事。最近的成功率也一样：重启之后从头攒。
pub struct Health {
    map: Mutex<HashMap<String, Entry>>,
    /// 每家最近的成败（[`Self::success_rates`]）
    recent: Mutex<Recent>,
    /// 配置里的 `failover`。**跟着配置换**（[`Self::configure`]），状态不丢
    settings: Mutex<tw_config::Failover>,
    clock: Clock,
}

impl Default for Health {
    fn default() -> Self {
        Self::new()
    }
}

impl Health {
    pub fn new() -> Self {
        Self::with_clock(Arc::new(tw_breaker::now_ms))
    }

    fn with_clock(clock: Clock) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            recent: Mutex::new(Recent::default()),
            settings: Mutex::new(tw_config::Failover::default()),
            clock,
        }
    }

    /// 换一份 `failover` 设置。已经停用着的照旧停着；之后的判断按新的算
    pub fn configure(&self, settings: &tw_config::Failover) {
        if let Ok(mut s) = self.settings.lock() {
            *s = settings.clone();
        }
    }

    fn settings(&self) -> tw_config::Failover {
        self.settings.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// 这家此刻的熔断规矩：冷却随它打开过的次数翻倍
    fn policy(settings: &tw_config::Failover, trips: u32) -> Policy {
        let doubled = settings
            .pause_secs
            .saturating_mul(1u64 << trips.saturating_sub(1).min(20));
        Policy {
            trip: Trip::Consecutive(settings.failures_to_pause),
            cooldown: Duration::from_secs(doubled.min(settings.max_pause_secs)),
            probes: 1,
        }
    }

    fn state_of(e: &Entry, settings: &tw_config::Failover, now: i64) -> State {
        if e.held_until_ms.is_some_and(|t| now < t) {
            return State::Open;
        }
        e.breaker.state_at(&Self::policy(settings, e.trips), now)
    }

    fn entry(&self, name: &str) -> Entry {
        self.map
            .lock()
            .ok()
            .and_then(|m| m.get(name).copied())
            .unwrap_or_default()
    }

    /// 改一家的记录，返回状态变化（没变是 `None`）。
    fn update(
        &self,
        name: &str,
        f: impl FnOnce(&mut Entry, &tw_config::Failover, i64),
    ) -> Option<State> {
        let settings = self.settings();
        let now = (self.clock)();
        let mut map = self.map.lock().ok()?;
        let e = map.entry(name.to_string()).or_default();
        let before = Self::state_of(e, &settings, now);
        f(e, &settings, now);
        let after = Self::state_of(e, &settings, now);
        (after != before).then_some(after)
    }

    /// 这家现在能进候选链吗。冷却到点的算半开，放行。
    ///
    /// **和「能不能收一个探测请求」是两件事**：只看不改，光是排候选顺序
    /// 不该把半开状态的探测机会用掉。
    pub fn is_available(&self, name: &str) -> bool {
        self.state(name) != State::Open
    }

    pub fn state(&self, name: &str) -> State {
        Self::state_of(&self.entry(name), &self.settings(), (self.clock)())
    }

    /// 记一次成功。**返回的是状态变化，没变就是 `None`。**
    ///
    /// 调用方要拿它去发事件：熔断开合是界面上看得见的状态，而看得见的
    /// 状态必须能被推出去 —— 否则界面只能轮询。
    pub fn record_success(&self, name: &str) -> Option<State> {
        self.note(name, true);
        self.update(name, |e, s, now| {
            e.breaker
                .record(true, Tally::default(), &Self::policy(s, e.trips), now);
            e.trips = 0;
            e.held_until_ms = None;
        })
    }

    /// 记一次说不出原因的失败，同样返回状态变化。
    pub fn record_failure(&self, name: &str) -> Option<State> {
        self.note(name, false);
        self.update(name, |e, s, now| {
            let policy = Self::policy(s, e.trips);
            let before = e.breaker.state_at(&policy, now);
            e.breaker.record(false, Tally::default(), &policy, now);
            // 这一次打开了（或者探测失败又打开了）：下一段冷却翻倍。
            // 已经开着、冷却还没到时又失败的（只有一家候选时会这样）不算新的一次
            if before != State::Open && e.breaker.state == State::Open {
                e.trips = e.trips.saturating_add(1);
            }
        })
    }

    /// 记一次失败，按原因停用。返回状态变化。
    pub fn record_cause(&self, name: &str, cause: Cause) -> Option<State> {
        let s = self.settings();
        let now = (self.clock)();
        let secs =
            |n: u64| now.saturating_add(i64::try_from(n.saturating_mul(1000)).unwrap_or(i64::MAX));
        let until = match cause {
            Cause::ModelUnavailable => return None,
            Cause::NoBalance => secs(s.no_balance_pause_secs),
            Cause::QuotaUsedUp { resets_at_ms } => resets_at_ms
                .and_then(|t| i64::try_from(t).ok())
                .filter(|t| *t > now)
                .unwrap_or_else(|| secs(s.quota_pause_secs)),
            Cause::RateLimited {
                retry_after: Some(d),
            } if !d.is_zero() => secs(d.as_secs().max(1).min(s.rate_limit_max_pause_secs)),
            Cause::RateLimited { .. } | Cause::AuthRejected | Cause::Unexplained => {
                return self.record_failure(name);
            }
        };
        self.note(name, false);
        self.update(name, |e, _, _| {
            e.held_until_ms = Some(e.held_until_ms.map_or(until, |t| t.max(until)));
        })
    }

    /// 记进最近的成败
    fn note(&self, name: &str, ok: bool) {
        let now = (self.clock)();
        if let Ok(mut r) = self.recent.lock() {
            r.record(name, ok, now);
        }
    }

    /// 这几家最近的成功率，0 到 1：最近 50 次、30 分钟以内，至少 5 次才算。**样本不够的
    /// 不在里面**，不是「从不失败」。`load-balance` 按成败分新对话时用它
    /// （`tw_engine::balance_factors`）。
    ///
    /// 成败的判据就是熔断的判据，不另起一套：上面几个 `record_*` 记一次，这里就记一次。
    /// 于是 5xx、连不上、超时、限流、额度用完、没钱了、凭据被拒或取不到、流在第一段内容
    /// 之前断了都算失败；请求本身的问题（别的 4xx）算这家答上了；这家没有这个模型不记 ——
    /// 它对别的模型照样好好的。客户端中途走了的不经过这里，也不记。
    ///
    /// **换下一家、却不是这家的错的**（它只是慢，或者正忙）不该调 `record_*`：调了，它在
    /// 熔断和这里都会被算成一次失败。
    pub fn success_rates(&self, names: &[String]) -> HashMap<String, f64> {
        let now = (self.clock)();
        let Ok(r) = self.recent.lock() else {
            return HashMap::new();
        };
        names
            .iter()
            .filter_map(|n| r.rate(n, now).map(|s| (n.clone(), s)))
            .collect()
    }

    /// 这家还要等多久才轮到探测。
    ///
    /// `None`：没停用过（或者停用之后已经成功过）。`Some(0)`：到点了。
    ///
    /// **专门给「报一条恢复」的定时器用。**
    pub fn cooldown_left(&self, name: &str) -> Option<Duration> {
        let e = self.entry(name);
        let now = (self.clock)();
        let held = e
            .held_until_ms
            .map(|t| Duration::from_millis(u64::try_from(t.saturating_sub(now)).unwrap_or(0)));
        let breaker = e
            .breaker
            .cooldown_left(&Self::policy(&self.settings(), e.trips), now);
        match (held, breaker) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    /// 从候选里挑出还能用的，**并说明是不是 fail-open**。
    ///
    /// 返回的第二个值为真时表示「全都熔断了，我们还是把原列表还给你」——
    /// 调用方应该照常尝试，但可以在日志里说清这一点。
    pub fn filter<'a>(&self, candidates: &'a [String]) -> (Vec<&'a String>, bool) {
        // 只有一个候选：完全旁路。没有别的家可切，熔断纯粹是自伤。
        if candidates.len() <= 1 {
            return (candidates.iter().collect(), false);
        }
        let alive: Vec<&String> = candidates.iter().filter(|c| self.is_available(c)).collect();
        if alive.is_empty() {
            // fail-open。宁可放行到一个可能坏的上游让用户看见真实错误，
            // 也不要返回一个我们自己编的「无可用上游」——后者会让用户
            // 以为是我们坏了。
            (candidates.iter().collect(), true)
        } else {
            (alive, false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_fresh_provider_is_available() {
        let h = Health::new();
        assert!(h.is_available("a"));
        assert_eq!(h.state("a"), State::Closed);
    }

    #[test]
    fn only_the_failure_that_flips_it_reports_a_change() {
        // 每次失败都报一条的话，这个事件就退化成了另一个请求流 ——
        // 而订阅它的那一页要的是「什么时候变了」，不是「又失败了一次」。
        let h = Health::new();
        assert_eq!(h.record_failure("a"), None);
        assert_eq!(h.record_failure("a"), None);
        assert_eq!(h.record_failure("a"), Some(State::Open), "第三次才是变化");
        assert_eq!(h.record_failure("a"), None, "已经开着了，不是新的变化");
    }

    #[test]
    fn a_success_reports_the_close_only_when_it_was_open() {
        let h = Health::new();
        assert_eq!(h.record_success("a"), None, "本来就是好的");
        for _ in 0..3 {
            h.record_failure("a");
        }
        assert_eq!(h.record_success("a"), Some(State::Closed));
        assert_eq!(h.record_success("a"), None);
    }

    /// 这条守着「报恢复」那个定时器的两个判断。
    #[test]
    fn the_cooldown_clock_says_which_case_it_is() {
        let h = Health::new();
        assert_eq!(h.cooldown_left("a"), None, "没熔断过，没有什么可等的");
        for _ in 0..3 {
            h.record_failure("a");
        }
        let left = h.cooldown_left("a").expect("熔断了就该在等");
        assert!(!left.is_zero(), "刚熔断，冷却还没走完");

        // **中途又失败会把冷却顶到更晚。**定时器到点时看到的是一段新的
        // 剩余时间，它该接着等 —— 现在宣布恢复的话，界面会说一家正在
        // 连续失败的上游已经好了。
        h.record_failure("a");
        assert!(!h.cooldown_left("a").expect("还在熔断").is_zero());

        // 成功之后那次熔断就不存在了：`record_success` 已经报过恢复，
        // 定时器到点看到 None，闭嘴走人。
        h.record_success("a");
        assert_eq!(h.cooldown_left("a"), None);
    }

    #[test]
    fn it_takes_three_consecutive_failures_to_open() {
        let h = Health::new();
        h.record_failure("a");
        h.record_failure("a");
        assert!(h.is_available("a"), "两次还不够");
        h.record_failure("a");
        assert!(!h.is_available("a"));
    }

    #[test]
    fn one_success_wipes_the_streak() {
        // 「连续」是关键词。断续的失败说明是偶发，不说明这家坏了。
        let h = Health::new();
        h.record_failure("a");
        h.record_failure("a");
        h.record_success("a");
        h.record_failure("a");
        assert!(h.is_available("a"));
    }

    #[test]
    fn a_single_candidate_bypasses_the_breaker_entirely() {
        // **唯一的上游被自己熔断就把用户锁死了。**没有别的家可切的时候，
        // 熔断纯粹是自伤。
        let h = Health::new();
        for _ in 0..10 {
            h.record_failure("only");
        }
        let c = names(&["only"]);
        let (alive, failopen) = h.filter(&c);
        assert_eq!(alive.len(), 1);
        assert!(!failopen, "旁路不是 fail-open，它压根没进熔断判断");
    }

    #[test]
    fn broken_candidates_are_filtered_out_while_others_remain() {
        let h = Health::new();
        for _ in 0..3 {
            h.record_failure("bad");
        }
        let c = names(&["bad", "good"]);
        let (alive, failopen) = h.filter(&c);
        assert_eq!(alive, vec![&"good".to_string()]);
        assert!(!failopen);
    }

    #[test]
    fn when_everything_is_broken_we_fail_open_rather_than_refuse() {
        // 服务器还能指望「换个实例试试」，单用户桌面没有第二条路。
        // 返回一个我们自己编的「无可用上游」会让用户以为是我们坏了。
        let h = Health::new();
        for n in ["a", "b"] {
            for _ in 0..3 {
                h.record_failure(n);
            }
        }
        let c = names(&["a", "b"]);
        let (alive, failopen) = h.filter(&c);
        assert_eq!(alive.len(), 2, "全都还给调用方");
        assert!(failopen, "而且要说清这是 fail-open");
    }

    #[test]
    fn availability_and_probe_permission_are_separate_questions() {
        // 合成一个方法的话，光是排候选顺序就会把半开状态的唯一探测
        // 机会用掉 —— 然后真正的请求发现自己没得试了。
        let h = Health::new();
        for _ in 0..3 {
            h.record_failure("a");
        }
        // 反复问可用性不该改变任何状态
        for _ in 0..5 {
            assert!(!h.is_available("a"));
        }
        h.record_success("a");
        assert!(h.is_available("a"));
    }

    #[test]
    fn state_is_per_provider_not_global() {
        let h = Health::new();
        for _ in 0..3 {
            h.record_failure("a");
        }
        assert!(!h.is_available("a"));
        assert!(h.is_available("b"));
    }

    #[test]
    fn after_the_cooldown_the_next_request_is_a_probe() {
        use std::sync::atomic::{AtomicI64, Ordering};
        let now = std::sync::Arc::new(AtomicI64::new(0));
        let h = Health::with_clock({
            let now = now.clone();
            std::sync::Arc::new(move || now.load(Ordering::SeqCst))
        });
        for _ in 0..3 {
            h.record_failure("a");
        }
        assert!(!h.is_available("a"));
        now.store(60_000, Ordering::SeqCst);
        assert!(h.is_available("a"), "冷却到点就放一个过去");
        assert_eq!(h.cooldown_left("a"), Some(Duration::ZERO));
        // 探测又失败：再报一次打开
        assert_eq!(h.record_failure("a"), Some(State::Open));
        assert!(!h.is_available("a"));
    }

    fn clocked() -> (Health, std::sync::Arc<std::sync::atomic::AtomicI64>) {
        let now = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(1_000_000));
        let h = Health::with_clock({
            let now = now.clone();
            std::sync::Arc::new(move || now.load(std::sync::atomic::Ordering::SeqCst))
        });
        (h, now)
    }

    fn advance(now: &std::sync::atomic::AtomicI64, ms: i64) {
        now.fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    fn each_further_pause_doubles_until_the_ceiling_and_a_success_resets_it() {
        let (h, now) = clocked();
        for _ in 0..3 {
            h.record_failure("a");
        }
        assert_eq!(h.cooldown_left("a"), Some(Duration::from_secs(60)));
        // 探测又失败：第二段 120 秒
        advance(&now, 60_000);
        assert_eq!(h.record_failure("a"), Some(State::Open));
        assert_eq!(h.cooldown_left("a"), Some(Duration::from_secs(120)));
        advance(&now, 120_000);
        h.record_failure("a");
        assert_eq!(h.cooldown_left("a"), Some(Duration::from_secs(240)));
        advance(&now, 240_000);
        h.record_failure("a");
        advance(&now, 480_000);
        h.record_failure("a");
        assert_eq!(
            h.cooldown_left("a"),
            Some(Duration::from_secs(600)),
            "封顶在 max_pause_secs"
        );
        // 成功一次，下次打开又从 60 秒起
        advance(&now, 600_000);
        h.record_success("a");
        for _ in 0..3 {
            h.record_failure("a");
        }
        assert_eq!(h.cooldown_left("a"), Some(Duration::from_secs(60)));
    }

    #[test]
    fn a_stated_reason_sets_the_upstream_aside_at_once_for_as_long_as_it_says() {
        let (h, now) = clocked();
        assert_eq!(h.record_cause("a", Cause::NoBalance), Some(State::Open));
        assert_eq!(h.cooldown_left("a"), Some(Duration::from_secs(1800)));

        let reset = 1_000_000 + 5 * 3_600_000;
        h.record_cause(
            "b",
            Cause::QuotaUsedUp {
                resets_at_ms: Some(reset as u64),
            },
        );
        assert_eq!(h.cooldown_left("b"), Some(Duration::from_secs(5 * 3600)));
        h.record_cause("c", Cause::QuotaUsedUp { resets_at_ms: None });
        assert_eq!(h.cooldown_left("c"), Some(Duration::from_secs(3600)));

        // Retry-After 太长的按上限
        h.record_cause(
            "d",
            Cause::RateLimited {
                retry_after: Some(Duration::from_secs(99_999)),
            },
        );
        assert_eq!(h.cooldown_left("d"), Some(Duration::from_secs(3600)));

        // 到点了：能用，定时器看到 0；成功之后什么都不记得
        advance(&now, 1_800_000);
        assert!(h.is_available("a"));
        assert_eq!(h.cooldown_left("a"), Some(Duration::ZERO));
        h.record_success("a");
        assert_eq!(h.cooldown_left("a"), None);
    }

    #[test]
    fn a_missing_model_or_an_unstated_rate_limit_does_not_set_it_aside_at_once() {
        let (h, _) = clocked();
        assert_eq!(h.record_cause("a", Cause::ModelUnavailable), None);
        assert!(h.is_available("a"));
        // 没说多久的限流按说不出原因的失败算：连续三次才停
        for _ in 0..2 {
            h.record_cause("b", Cause::RateLimited { retry_after: None });
        }
        assert!(h.is_available("b"));
        h.record_cause("b", Cause::Unexplained);
        assert!(!h.is_available("b"));
    }

    #[test]
    fn the_settings_come_from_the_configuration() {
        let (h, _) = clocked();
        h.configure(&tw_config::Failover {
            failures_to_pause: 1,
            pause_secs: 5,
            no_balance_pause_secs: 7,
            ..Default::default()
        });
        assert_eq!(h.record_failure("a"), Some(State::Open), "一次就停");
        assert_eq!(h.cooldown_left("a"), Some(Duration::from_secs(5)));
        h.record_cause("b", Cause::NoBalance);
        assert_eq!(h.cooldown_left("b"), Some(Duration::from_secs(7)));
    }

    fn rate(h: &Health, name: &str) -> Option<f64> {
        h.success_rates(&names(&[name])).get(name).copied()
    }

    #[test]
    fn a_success_rate_needs_five_outcomes_and_looks_at_the_last_fifty() {
        let (h, _) = clocked();
        for _ in 0..4 {
            h.record_failure("a");
        }
        assert_eq!(rate(&h, "a"), None, "四次还说不出成功率");
        h.record_success("a");
        assert_eq!(rate(&h, "a"), Some(0.2));
        // 攒满 50 次：四次失败还在窗口里
        for _ in 0..45 {
            h.record_success("a");
        }
        assert_eq!(rate(&h, "a"), Some(46.0 / 50.0));
        // 再来四次成功，最早那四次失败被挤出窗口
        for _ in 0..4 {
            h.record_success("a");
        }
        assert_eq!(rate(&h, "a"), Some(1.0));
    }

    #[test]
    fn outcomes_older_than_half_an_hour_drop_out_so_a_recovered_upstream_regains_its_share() {
        let (h, now) = clocked();
        for _ in 0..10 {
            h.record_failure("a");
        }
        assert_eq!(rate(&h, "a"), Some(0.0));
        advance(&now, 29 * 60 * 1000);
        for _ in 0..5 {
            h.record_success("a");
        }
        assert_eq!(rate(&h, "a"), Some(5.0 / 15.0));
        // 那十次失败满半小时了：只剩后来的五次成功
        advance(&now, 60 * 1000);
        assert_eq!(rate(&h, "a"), Some(1.0));
        advance(&now, 30 * 60 * 1000);
        assert_eq!(rate(&h, "a"), None, "全都过了时，等于没有样本");
    }

    #[test]
    fn the_success_rate_counts_what_the_breaker_counts() {
        let (h, _) = clocked();
        // 上游的问题：说了原因的、没说原因的，都算失败
        for cause in [
            Cause::NoBalance,
            Cause::QuotaUsedUp { resets_at_ms: None },
            Cause::RateLimited {
                retry_after: Some(Duration::from_secs(30)),
            },
            Cause::RateLimited { retry_after: None },
            Cause::AuthRejected,
            Cause::Unexplained,
        ] {
            h.record_cause("a", cause);
        }
        h.record_failure("a");
        assert_eq!(rate(&h, "a"), Some(0.0), "七次失败");
        // 这家没有这个模型：不算它坏了，也不算它答上了
        for _ in 0..10 {
            h.record_cause("b", Cause::ModelUnavailable);
        }
        assert_eq!(rate(&h, "b"), None);
        for _ in 0..3 {
            h.record_success("a");
        }
        assert_eq!(rate(&h, "a"), Some(0.3));
    }

    #[test]
    fn success_rates_lists_only_the_asked_ones_with_enough_samples() {
        let (h, _) = clocked();
        for _ in 0..5 {
            h.record_success("a");
            h.record_failure("b");
            h.record_success("组外");
        }
        h.record_success("c");
        let got = h.success_rates(&names(&["a", "b", "c", "d"]));
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!((got["a"], got["b"]), (1.0, 0.0));
    }
}
