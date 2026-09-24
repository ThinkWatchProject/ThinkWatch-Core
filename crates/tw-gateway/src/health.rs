//! 上游健康与熔断。
//!
//! 状态机是共用的（`tw-breaker`，企业版的路由和 MCP 熔断跑的是同一个），
//! 这里是桌面版的**规矩**：
//!
//! **只有两个旋钮**：连续失败阈值和冷却时长。cc-switch
//! 那套有五个，其中「错误率 + 最小请求数」这条路径在单用户场景下几乎永远
//! 打不满 —— 要先攒够十几个请求才生效，而那时用户早就自己发现了。
//!
//! 两条硬边界，它们比参数重要得多：
//!
//! 1. **全部候选都被熔断时放行，而不是拒绝。**服务器还能指望「换个实例
//!    试试」，单用户桌面没有第二条路 —— 宁可放行到一个可能坏的上游让用户
//!    看见真实错误，也不要返回一个我们自己编的「无可用上游」。
//! 2. **只有一个候选时完全旁路熔断器。**否则唯一的上游一旦被自己熔断，
//!    就把用户锁死了，熔断纯粹是自伤。

use std::time::Duration;

pub use tw_breaker::State;
use tw_breaker::{Breakers, Policy, Trip};

/// 连续失败多少次算坏。
pub const FAILURE_THRESHOLD: u32 = 3;

/// 熔断之后多久放一个探测过去。
pub const COOLDOWN: Duration = Duration::from_secs(60);

/// 桌面版的规矩：连续失败 3 次打开，冷却 60 秒，一次成功就闭合。
pub const POLICY: Policy = Policy {
    trip: Trip::Consecutive(FAILURE_THRESHOLD),
    cooldown: COOLDOWN,
    probes: 1,
};

/// 每个 provider 的健康状态。
///
/// **不持久化**。桌面应用重启频繁，把「这家挂了」的判断带过重启
/// 意味着用户重启后第一个请求还在被上一次的故障惩罚 —— 而重启本身往往
/// 就是他为了解决问题做的事。
pub struct Health {
    breakers: Breakers<String>,
}

impl Default for Health {
    fn default() -> Self {
        Self::new()
    }
}

impl Health {
    pub fn new() -> Self {
        Self {
            breakers: Breakers::new(POLICY),
        }
    }

    #[cfg(test)]
    fn with_clock(clock: std::sync::Arc<dyn Fn() -> i64 + Send + Sync>) -> Self {
        Self {
            breakers: Breakers::with_clock(POLICY, clock),
        }
    }

    /// 这家现在能进候选链吗。冷却到点的算半开，放行。
    ///
    /// **和「能不能收一个探测请求」是两件事**：只看不改，光是排候选顺序
    /// 不该把半开状态的探测机会用掉。
    pub fn is_available(&self, name: &str) -> bool {
        self.breakers.admits(&name.to_string())
    }

    pub fn state(&self, name: &str) -> State {
        self.breakers.state(&name.to_string())
    }

    /// 记一次成功。**返回的是状态变化，没变就是 `None`。**
    ///
    /// 调用方要拿它去发事件：熔断开合是界面上看得见的状态，而看得见的
    /// 状态必须能被推出去 —— 否则界面只能轮询。
    pub fn record_success(&self, name: &str) -> Option<State> {
        self.breakers.record(&name.to_string(), true)
    }

    /// 记一次失败，同样返回状态变化。
    pub fn record_failure(&self, name: &str) -> Option<State> {
        self.breakers.record(&name.to_string(), false)
    }

    /// 这家还要等多久才轮到探测。见 [`tw_breaker::Breaker::cooldown_left`]。
    ///
    /// **专门给「报一条恢复」的定时器用。**
    pub fn cooldown_left(&self, name: &str) -> Option<Duration> {
        self.breakers.cooldown_left(&name.to_string())
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
        now.store(COOLDOWN.as_millis() as i64, Ordering::SeqCst);
        assert!(h.is_available("a"), "冷却到点就放一个过去");
        assert_eq!(h.cooldown_left("a"), Some(Duration::ZERO));
        // 探测又失败：再报一次打开
        assert_eq!(h.record_failure("a"), Some(State::Open));
        assert!(!h.is_available("a"));
    }
}
