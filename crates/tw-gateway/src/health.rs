//! 上游健康与熔断。
//!
//! **只有两个旋钮**（DESIGN.md §4.2）：连续失败阈值和冷却时长。cc-switch
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

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 连续失败多少次算坏。
pub const FAILURE_THRESHOLD: u32 = 3;

/// 熔断之后多久放一个探测过去。
pub const COOLDOWN: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Closed,
    /// 熔断中。到点之后第一个请求会被放过去探路（半开）
    Open,
}

#[derive(Debug, Default)]
struct Entry {
    consecutive_failures: u32,
    opened_at: Option<Instant>,
}

/// 每个 provider 的健康状态。
///
/// **不持久化**（§4.2）。桌面应用重启频繁，把「这家挂了」的判断带过重启
/// 意味着用户重启后第一个请求还在被上一次的故障惩罚 —— 而重启本身往往
/// 就是他为了解决问题做的事。
#[derive(Default)]
pub struct Health {
    inner: Mutex<HashMap<String, Entry>>,
}

impl Health {
    pub fn new() -> Self {
        Self::default()
    }

    /// 这家现在能进候选链吗。
    ///
    /// **和「能不能收一个探测请求」是两件事**，所以是两个方法。合成一个
    /// 的话，光是排候选顺序就会把半开状态的唯一探测机会用掉。
    pub fn is_available(&self, name: &str) -> bool {
        self.state(name) == State::Closed
    }

    pub fn state(&self, name: &str) -> State {
        let g = self.inner.lock().unwrap();
        let Some(e) = g.get(name) else {
            return State::Closed;
        };
        match e.opened_at {
            Some(t) if t.elapsed() < COOLDOWN => State::Open,
            // 冷却到点了就当成 Closed —— 下一个请求自然成为探测。
            _ => State::Closed,
        }
    }

    pub fn record_success(&self, name: &str) {
        let mut g = self.inner.lock().unwrap();
        let e = g.entry(name.to_string()).or_default();
        e.consecutive_failures = 0;
        e.opened_at = None;
    }

    pub fn record_failure(&self, name: &str) {
        let mut g = self.inner.lock().unwrap();
        let e = g.entry(name.to_string()).or_default();
        e.consecutive_failures += 1;
        if e.consecutive_failures >= FAILURE_THRESHOLD {
            e.opened_at = Some(Instant::now());
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
}
