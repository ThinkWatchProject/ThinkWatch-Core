//! `load-balance` 组轮到了谁：每个组、每个成员此刻的「当前权重」（平滑加权轮询，
//! 轮法见 [`tw_engine::weighted`]）。
//!
//! - **做决定的那一刻就记账**，不等回答成功：同时进来的一批请求一个接一个地排，后一个
//!   看到的是前一个记过账的样子，于是分散到各家，而不是都排给同一家。从排序到记账一直
//!   拿着锁（[`Turn`]）。
//! - **记在会话粘性之后实际排头的那一家头上**：一段对话留在上次回答它的那一家（见
//!   [`crate::affinity`]），按权重本该轮到的那一家这一次没轮上 —— 账记给留下的那一家，
//!   之后的新对话把差的补回去，长期看各家拿到的还是配置的比例。
//! - **组的成员或权重变了就从头轮**：旧的当前权重是按旧的比例攒下的。
//!
//! 只在内存里：core 重启之后从头轮，差的最多是一轮。**跨配置重载存活**，没改的组接着轮。
//! 试算只读不记（[`Balance::peek`]）：它说的是下一个新对话会排给谁，说完了不该改变答案。

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use tw_engine::{Facts, Group, Member};

/// 一个组轮到哪儿了
struct Round {
    /// 这份状态是按哪一组成员、哪一组权重攒的。和配置对不上就作废
    members: Vec<Member>,
    /// 成员 → 当前权重。没有的是 0
    current: HashMap<String, i64>,
}

/// 每个 `load-balance` 组轮到哪儿了，按组名记。
#[derive(Default)]
pub struct Balance {
    rounds: Mutex<HashMap<String, Round>>,
}

impl Balance {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Round>> {
        // 锁中毒了照样拿里面的表：轮得偏一点，好过一个不转发的网关
        self.rounds.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 开始给组 `g` 排一次。**拿到它就拿着锁**，直到 [`Turn::charge`] 记完账（或者丢掉它）。
    ///
    /// 组的成员或权重和记着的不一样（配置改了）就从头轮。
    pub fn turn<'a>(&'a self, g: &'a Group) -> Turn<'a> {
        let mut rounds = self.lock();
        if rounds.get(&g.name).is_none_or(|r| r.members != g.providers) {
            rounds.insert(
                g.name.clone(),
                Round {
                    members: g.providers.clone(),
                    current: HashMap::new(),
                },
            );
        }
        Turn { rounds, group: g }
    }

    /// 组 `g` 此刻的当前权重，**只看不记**。试算用：它和数据面读同一份，说完了不改变答案
    pub fn peek(&self, g: &Group) -> HashMap<String, i64> {
        self.lock()
            .get(&g.name)
            .filter(|r| r.members == g.providers)
            .map(|r| r.current.clone())
            .unwrap_or_default()
    }
}

/// 正在给一个组排的这一次：排序读它的当前权重，排完、粘性也定了之后记账。
pub struct Turn<'a> {
    rounds: MutexGuard<'a, HashMap<String, Round>>,
    group: &'a Group,
}

impl Turn<'_> {
    /// 这个组此刻的当前权重：排序要的那一份（[`Facts::current_weight`]）
    pub fn current(&self) -> HashMap<String, i64> {
        self.rounds
            .get(&self.group.name)
            .map(|r| r.current.clone())
            .unwrap_or_default()
    }

    /// 记账：这一次排头的是 `leader`。`members` 和 `f` 是排序时的那一份候选和事实 ——
    /// 粘性只换了次序，没换集合，这一轮还是同一轮
    pub fn charge(mut self, members: &[String], f: &Facts, leader: &str) {
        let next = tw_engine::weighted::advance(self.group, members, f, leader);
        if let Some(r) = self.rounds.get_mut(&self.group.name) {
            r.current = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(weights: &[(&str, u32)]) -> Group {
        Group {
            name: "池子".into(),
            kind: tw_engine::GroupType::LoadBalance,
            providers: weights
                .iter()
                .map(|(n, w)| Member {
                    name: n.to_string(),
                    weight: *w,
                })
                .collect(),
            selected: None,
            balance_by: Default::default(),
        }
    }

    /// 像管线那样排一次：读状态、排序、记在排头的那一家头上
    fn once(b: &Balance, g: &Group) -> String {
        let members = g.names();
        let t = b.turn(g);
        let f = Facts {
            current_weight: t.current(),
            ..Default::default()
        };
        let first = tw_engine::order_by(g, &members, &f)[0].clone();
        t.charge(&members, &f, &first);
        first
    }

    #[test]
    fn it_remembers_where_each_group_is() {
        let b = Balance::default();
        let g = group(&[("甲", 2), ("乙", 1)]);
        let got: Vec<String> = (0..6).map(|_| once(&b, &g)).collect();
        assert_eq!(got, ["甲", "乙", "甲", "甲", "乙", "甲"]);
    }

    /// 只看不记：看多少次，下一个都还是同一家
    #[test]
    fn peeking_does_not_move_it() {
        let b = Balance::default();
        let g = group(&[("甲", 1), ("乙", 1)]);
        once(&b, &g);
        let seen = b.peek(&g);
        for _ in 0..3 {
            assert_eq!(b.peek(&g), seen);
        }
        assert_eq!(once(&b, &g), "乙");
    }

    /// 成员或权重变了：从头轮。没变的接着轮
    #[test]
    fn a_changed_group_starts_over() {
        let b = Balance::default();
        let g = group(&[("甲", 1), ("乙", 1)]);
        assert_eq!(once(&b, &g), "甲");
        // 同一份配置重载了一次：接着轮
        assert_eq!(once(&b, &g.clone()), "乙");
        assert_eq!(once(&b, &g), "甲");
        // 权重改了：攒下的作废，试算也看不到旧的
        let reweighted = group(&[("甲", 1), ("乙", 3)]);
        assert!(b.peek(&reweighted).is_empty());
        assert_eq!(once(&b, &reweighted), "乙");
        // 多了一个成员：同样从头轮
        let grown = group(&[("甲", 1), ("乙", 3), ("丙", 1)]);
        assert!(b.peek(&grown).is_empty());
    }
}
