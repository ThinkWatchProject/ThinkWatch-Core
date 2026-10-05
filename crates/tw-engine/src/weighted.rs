//! 策略组的成员和 `load-balance` 的权重：成员怎么写，平滑加权轮询怎么轮。
//!
//! 轮法是 nginx 的平滑加权轮询：每排一次，这一轮的每个成员给自己的「当前权重」加上
//! 自己的权重，加完最大的那个排头，排头的再减去这一轮的总权重。7:3 排出来是
//! 甲乙甲甲甲乙甲甲乙甲 —— 穿插着来，而不是先连着七次甲、再连着三次乙；权重都一样
//! 就是挨个轮。
//!
//! **轮的是有效权重**：成员的权重乘上 `balance_by` 按快慢、成败算出的系数
//! （[`crate::engine::balance_factors`]，只按权重分时全是 1），见 [`effective`]。
//!
//! **当前权重不在这里**：它由网关记着（`tw_gateway::balance`），排序时经
//! [`Facts::current_weight`] 传进来，排完、会话粘性也定了之后，网关按 [`advance`]
//! 记账。引擎因此还是纯函数：试算拿同一份状态算、不记账，说的就是数据面下一个新对话
//! 会排给谁。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::engine::{Facts, Group, balance_factors};

/// 权重最小是 1：0 等于把它从组里拿掉，那就该真的拿掉
pub const WEIGHT_MIN: u32 = 1;

/// 权重最大是 100：比例上够用，当前权重也不会大到溢出
pub const WEIGHT_MAX: u32 = 100;

/// 有效权重放大多少倍再取整。系数是小数（0.05、0.39…），当前权重是整数：直接取整的话，
/// 权重 1 乘 0.39 就成了 0。放大一千倍，小数点后三位都算数；系数都是 1 时各家同样
/// 放大，轮出来的次序和不放大一模一样
const SCALE: f64 = 1000.0;

/// 一个成员这一轮的有效权重：权重 × 系数，放大 [`SCALE`] 倍取整，**最少是 1** ——
/// 系数再小，它也还在轮里，偶尔轮到一次（成败系数的下限也是这个意思）。
///
/// 最大是 100 × 10（快慢）× 1（成败）× 1000 = 一百万，当前权重攒得再多也远不到 i64 的边
pub fn effective(weight: u32, factor: f64) -> i64 {
    ((f64::from(weight) * factor * SCALE).round() as i64).max(1)
}

/// 策略组的一个成员：上游的名字，和它在 `load-balance` 里的权重。
///
/// 配置里写成名字（权重 1），或者写成 `{ name, weight }`。**权重是 1 的写回去还是
/// 名字**（见 [`members`]）：没写过权重的配置，读进来再写回去一个字节都不变。
///
/// 只有 `load-balance` 用得上权重，别的类型写了不是 1 的权重，校验拒绝
/// （`engine.group_weight_not_load_balance`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Member {
    /// 上游的名字
    pub name: String,
    /// 新对话按权重的比例分给各个成员。不写是 1
    #[serde(default = "one")]
    pub weight: u32,
}

fn one() -> u32 {
    1
}

impl Member {
    /// 只写了名字的成员：权重 1
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            weight: 1,
        }
    }
}

impl From<&str> for Member {
    fn from(name: &str) -> Self {
        Self::named(name)
    }
}

impl From<String> for Member {
    fn from(name: String) -> Self {
        Self::named(name)
    }
}

/// `providers` 列表的读写：一项是名字，或者 `{ name, weight }`。
///
/// 读的时候一个字符串是名字、一个映射是带权重的成员（和 serde 的 untagged 同一种写法），
/// 只是手写了：untagged 对写错的地方只会说「哪一种都不像」，而 `wieght` 这种拼错要说出
/// 是哪个字段。写的时候权重 1 写成名字。
pub(crate) mod members {
    use serde::de::{self, Deserializer, MapAccess, Visitor};
    use serde::ser::{SerializeSeq, Serializer};
    use serde::{Deserialize, Serialize};

    use super::Member;

    pub fn serialize<S: Serializer>(v: &[Member], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(v.len()))?;
        for m in v {
            if m.weight == 1 {
                seq.serialize_element(&m.name)?;
            } else {
                seq.serialize_element(m)?;
            }
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Member>, D::Error> {
        Ok(Vec::<Item>::deserialize(d)?
            .into_iter()
            .map(|i| i.0)
            .collect())
    }

    /// 列表里的一项
    struct Item(Member);

    impl Serialize for Item {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            self.0.serialize(s)
        }
    }

    impl<'de> Deserialize<'de> for Item {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> Visitor<'de> for V {
                type Value = Item;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("the name of an upstream, or {name, weight}")
                }
                fn visit_str<E: de::Error>(self, v: &str) -> Result<Item, E> {
                    Ok(Item(Member::named(v)))
                }
                fn visit_string<E: de::Error>(self, v: String) -> Result<Item, E> {
                    Ok(Item(Member::named(v)))
                }
                // 没加引号、YAML 读成数或真假的名字（`[2024, b]`）：照名字收下，和规则的
                // `to` 一样（见 [`crate::engine::scalar_name`]）。这一项以前只读字符串，
                // serde_yaml 给的是原文 —— 不收下的话，一份原来读得进的配置就读不进了
                fn visit_bool<E: de::Error>(self, v: bool) -> Result<Item, E> {
                    Ok(Item(Member::named(v.to_string())))
                }
                fn visit_i64<E: de::Error>(self, v: i64) -> Result<Item, E> {
                    Ok(Item(Member::named(v.to_string())))
                }
                fn visit_u64<E: de::Error>(self, v: u64) -> Result<Item, E> {
                    Ok(Item(Member::named(v.to_string())))
                }
                fn visit_i128<E: de::Error>(self, v: i128) -> Result<Item, E> {
                    Ok(Item(Member::named(v.to_string())))
                }
                fn visit_u128<E: de::Error>(self, v: u128) -> Result<Item, E> {
                    Ok(Item(Member::named(v.to_string())))
                }
                fn visit_f64<E: de::Error>(self, v: f64) -> Result<Item, E> {
                    Ok(Item(Member::named(crate::engine::scalar_name(v))))
                }
                fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Item, A::Error> {
                    // 照 `Member` 自己的规矩读：写错的字段名由 serde 说出来
                    Member::deserialize(de::value::MapAccessDeserializer::new(map)).map(Item)
                }
            }
            d.deserialize_any(V)
        }
    }
}

/// 这一轮参加轮询的成员和它们的有效权重（[`effective`]），按 `members` 的顺序。
///
/// 系数按**全部候选**算（[`balance_factors`]，快慢比的是候选里测到的那几家的中位数），
/// 和试算列出来的那一份一样；算完再去掉停着的。
///
/// **停着的不算**（[`Facts::paused`]）：熔断、冷却着的那一家这一次本来就会被跳过，
/// 轮到它的那一次落到组里排在它后面的那一家，那一家就平白多拿一份。**并发数满着的也
/// 不算**（[`Facts::busy`]）：网关当场跳过它，这一份记在它头上的话，它答得越多越满、
/// 越满越被记空账，拿到的比它的权重少。停着的、满着的加起来是全部时都算 —— 那时网关
/// 照样一家家试（fail-open）、等先空出来的那一家，排头的还是按权重来。
fn round<'a>(g: &Group, members: &'a [String], f: &Facts) -> Vec<(&'a str, i64)> {
    let factors = balance_factors(g.balance_by, members, f);
    let all: Vec<(&'a str, i64)> = members
        .iter()
        .zip(factors)
        .map(|(m, x)| (m.as_str(), effective(g.weight(m), x)))
        .collect();
    let up: Vec<(&'a str, i64)> = all
        .iter()
        .copied()
        .filter(|(m, _)| !f.paused.contains(*m) && !f.busy.contains(*m))
        .collect();
    if up.is_empty() { all } else { up }
}

/// 下一个排头：当前权重加上自己的有效权重，最大的那个；一样大取组里靠前的。
///
/// `members` 是这次的候选（服务不了这个请求的已经去掉了）：不在里面的成员这一轮不参加，
/// 剩下的按各自的权重分。
pub fn lead<'a>(g: &Group, members: &'a [String], f: &Facts) -> Option<&'a str> {
    let mut best: Option<(&str, i64)> = None;
    for (m, w) in round(g, members, f) {
        let v = f.current_weight.get(m).copied().unwrap_or(0) + w;
        if best.is_none_or(|(_, b)| v > b) {
            best = Some((m, v));
        }
    }
    best.map(|(m, _)| m)
}

/// 记一次账，交回记过之后的当前权重：这一轮的每个成员加上自己的有效权重，`leader`
/// 再减去这一轮的总和。
///
/// `leader` 是**会话粘性之后实际排头的那一家**，不一定是 [`lead`] 挑的那个：一段对话
/// 留在了上次回答它的那一家，这一次就记在那一家头上，之后的新对话把差的补回去。
/// `members` 和 `f` 要和排序时的一样（同一轮；系数也就是排序时的那一份）。`leader`
/// 不在这一轮里时什么都不记。
pub fn advance(g: &Group, members: &[String], f: &Facts, leader: &str) -> HashMap<String, i64> {
    let round = round(g, members, f);
    let mut out = f.current_weight.clone();
    if !round.iter().any(|(m, _)| *m == leader) {
        return out;
    }
    let total: i64 = round.iter().map(|(_, w)| w).sum();
    for (m, w) in &round {
        *out.entry((*m).to_string()).or_insert(0) += w;
    }
    *out.entry(leader.to_string()).or_insert(0) -= total;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{BalanceBy, GroupType, order_by};

    fn group(members: &[(&str, u32)]) -> Group {
        Group {
            name: "池子".into(),
            kind: GroupType::LoadBalance,
            providers: members
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

    fn names(g: &Group) -> Vec<String> {
        g.providers.iter().map(|m| m.name.clone()).collect()
    }

    /// 像网关那样排 `n` 次、每次记在排头的那一家，交回每次的排头
    fn run(g: &Group, members: &[String], f: &mut Facts, n: usize) -> Vec<String> {
        (0..n)
            .map(|_| {
                let first = order_by(g, members, f)[0].clone();
                f.current_weight = advance(g, members, f, &first);
                first
            })
            .collect()
    }

    /// 7:3 穿插着来，十次里正好七次甲、三次乙，然后从头再来一遍
    #[test]
    fn seven_to_three_interleaves() {
        let g = group(&[("甲", 7), ("乙", 3)]);
        let mut f = Facts::default();
        let got = run(&g, &names(&g), &mut f, 20);
        let want = ["甲", "乙", "甲", "甲", "甲", "乙", "甲", "甲", "乙", "甲"];
        assert_eq!(got[..10], want);
        assert_eq!(got[10..], want, "十次之后回到起点");
        assert!(f.current_weight.values().all(|v| *v == 0), "{f:?}");
    }

    /// 权重都一样就是挨个轮
    #[test]
    fn equal_weights_take_turns() {
        for w in [1, 5] {
            let g = group(&[("甲", w), ("乙", w), ("丙", w)]);
            let mut f = Facts::default();
            assert_eq!(
                run(&g, &names(&g), &mut f, 6),
                ["甲", "乙", "丙", "甲", "乙", "丙"]
            );
        }
    }

    /// 排头的之后，其余的按组里的顺序跟着，一个都不少 —— 故障转移还要用它们
    #[test]
    fn the_rest_follow_in_the_groups_order() {
        let g = group(&[("甲", 1), ("乙", 1), ("丙", 1)]);
        let f = Facts {
            current_weight: [("乙".to_string(), 5)].into_iter().collect(),
            ..Default::default()
        };
        assert_eq!(order_by(&g, &names(&g), &f), ["乙", "甲", "丙"]);
    }

    /// 不在候选里的成员（停用了、没有这个模型）这一轮不参加，当前权重也不动；剩下的
    /// 照各自的权重分
    #[test]
    fn a_member_left_out_of_the_candidates_sits_the_round_out() {
        let g = group(&[("甲", 3), ("乙", 1), ("丙", 2)]);
        let without: Vec<String> = vec!["甲".into(), "丙".into()];
        let mut f = Facts::default();
        let got = run(&g, &without, &mut f, 50);
        assert_eq!(got.iter().filter(|m| *m == "甲").count(), 30);
        assert_eq!(got.iter().filter(|m| *m == "丙").count(), 20);
        assert_eq!(f.current_weight.get("乙"), None, "没参加的不记账");

        // 已经轮了一阵之后才缺席：比例照样是剩下的那几家的
        let mut f = Facts::default();
        run(&g, &names(&g), &mut f, 7);
        let before = f.current_weight.get("乙").copied();
        let got = run(&g, &without, &mut f, 100);
        let a = got.iter().filter(|m| *m == "甲").count() as i64;
        assert!((a - 60).abs() <= 2, "甲排头 {a} 次");
        assert_eq!(f.current_weight.get("乙").copied(), before);
    }

    /// 并发数满着的同样不参加，当前权重也不动：空出来之后接着按权重轮，不欠也不多。
    /// 满着的和停着的加起来是全部时都参加
    #[test]
    fn busy_members_sit_out_unless_every_member_is_busy_or_paused() {
        let g = group(&[("甲", 1), ("乙", 1), ("丙", 1)]);
        let mut f = Facts {
            busy: ["甲".to_string()].into_iter().collect(),
            ..Default::default()
        };
        assert_eq!(
            run(&g, &names(&g), &mut f, 4),
            ["乙", "丙", "乙", "丙"],
            "满着的那一家轮不到，没答的不记在它头上"
        );
        assert_eq!(f.current_weight.get("甲"), None, "满着的不记账");
        // 空出来了：从它没欠账的样子接着轮
        f.busy.clear();
        let got = run(&g, &names(&g), &mut f, 30);
        assert_eq!(got.iter().filter(|m| *m == "甲").count(), 10, "{got:?}");

        let mut f = Facts {
            busy: ["甲".to_string(), "乙".to_string()].into_iter().collect(),
            paused: ["丙".to_string()].into_iter().collect(),
            ..Default::default()
        };
        assert_eq!(run(&g, &names(&g), &mut f, 3), ["甲", "乙", "丙"]);
    }

    /// 停着的（熔断、冷却）同样不参加；全都停着时都参加
    #[test]
    fn paused_members_sit_out_unless_every_member_is_paused() {
        let g = group(&[("甲", 1), ("乙", 1), ("丙", 1)]);
        let mut f = Facts {
            paused: ["甲".to_string()].into_iter().collect(),
            ..Default::default()
        };
        assert_eq!(
            run(&g, &names(&g), &mut f, 4),
            ["乙", "丙", "乙", "丙"],
            "停着的那一家轮不到，它那一份不会落到它后面那一家头上"
        );
        let mut f = Facts {
            paused: names(&g).into_iter().collect(),
            ..Default::default()
        };
        assert_eq!(run(&g, &names(&g), &mut f, 3), ["甲", "乙", "丙"]);
    }

    /// 记在实际排头的那一家头上：粘性把甲留在了前面，下一个新对话轮到乙
    #[test]
    fn the_member_that_actually_led_is_charged() {
        let g = group(&[("甲", 1), ("乙", 1)]);
        let members = names(&g);
        let mut f = Facts::default();
        assert_eq!(run(&g, &members, &mut f, 1), ["甲"]);
        // 按轮到的该是乙，一段对话留在了甲
        assert_eq!(order_by(&g, &members, &f)[0], "乙");
        f.current_weight = advance(&g, &members, &f, "甲");
        // 新对话：两次都是乙，把差的补回来
        assert_eq!(run(&g, &members, &mut f, 2), ["乙", "乙"]);
        assert_eq!(run(&g, &members, &mut f, 2), ["甲", "乙"]);
        // 不在这一轮里的名字什么都不记
        let before = f.current_weight.clone();
        assert_eq!(advance(&g, &members, &f, "别家"), before);
    }

    /// 权重 1 写回去是名字，写了别的权重写回去是映射；数、真假照名字读
    #[test]
    fn a_member_is_a_name_or_a_name_with_a_weight() {
        let text = "name: 池子\ntype: load-balance\nproviders:\n- 甲\n- name: 乙\n  weight: 3\n- name: 丙\n  weight: 1\n- 2024\n";
        let g: Group = serde_yaml_ng::from_str(text).unwrap();
        assert_eq!(
            g.providers
                .iter()
                .map(|m| (m.name.as_str(), m.weight))
                .collect::<Vec<_>>(),
            [("甲", 1), ("乙", 3), ("丙", 1), ("2024", 1)]
        );
        assert_eq!(g.weight("乙"), 3);
        assert_eq!(g.weight("没有这家"), 1);
        let back = serde_yaml_ng::to_string(&g).unwrap();
        assert_eq!(
            back,
            "name: 池子\ntype: load-balance\nproviders:\n- 甲\n- name: 乙\n  weight: 3\n- 丙\n- '2024'\n"
        );
        // 只写名字的组，写回去和原来一样
        let plain = "name: 池子\ntype: fallback\nproviders:\n- 甲\n- 乙\n";
        let g: Group = serde_yaml_ng::from_str(plain).unwrap();
        assert_eq!(serde_yaml_ng::to_string(&g).unwrap(), plain);
    }

    /// 拼错的字段说出是哪一个，不是一句「哪一种都不像」
    #[test]
    fn a_misspelled_member_field_is_named() {
        let e = serde_yaml_ng::from_str::<Group>(
            "name: g\ntype: load-balance\nproviders: [a, {name: b, wieght: 3}]\n",
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("wieght"), "{e}");
        assert!(e.contains("providers"), "{e}");
        let e = serde_yaml_ng::from_str::<Group>("name: g\nproviders: [a, {weight: 3}]\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("name"), "{e}");
    }

    /// 按 `by` 分的组：成员和权重同 [`group`]
    fn balanced(members: &[(&str, u32)], by: BalanceBy) -> Group {
        Group {
            balance_by: by,
            ..group(members)
        }
    }

    /// 排 `n` 次，数每一家排头了几次
    fn count(g: &Group, f: &mut Facts, n: usize) -> HashMap<String, usize> {
        let mut out = HashMap::new();
        for m in run(g, &names(g), f, n) {
            *out.entry(m).or_default() += 1;
        }
        out
    }

    fn ttfb(v: &[(&str, u32)]) -> HashMap<String, u32> {
        v.iter().map(|(n, t)| (n.to_string(), *t)).collect()
    }

    /// 有效权重：权重 × 系数，放大一千倍取整，最少是 1
    #[test]
    fn the_effective_weight_is_the_weight_times_the_factor() {
        assert_eq!(effective(1, 1.0), 1000);
        assert_eq!(effective(7, 1.0), 7000);
        assert_eq!(effective(3, 0.390625), 1172, "1171.875 取整");
        assert_eq!(effective(100, 10.0), 1_000_000);
        // 系数再小也还在轮里
        assert_eq!(effective(1, 0.0), 1);
        assert_eq!(effective(1, 1e-9), 1);
    }

    /// 按快慢分：第一段内容来得快的那一家分到的新对话多。一整圈（有效权重之和那么多次）下来，
    /// 各家排头的次数正好是各自的有效权重，当前权重回到零
    #[test]
    fn latency_gives_the_faster_member_more_new_conversations() {
        let g = balanced(&[("甲", 1), ("乙", 1)], BalanceBy::Latency);
        // 中位数 250：甲 (250/100)² = 6.25，乙 (250/400)² = 0.390625
        let mut f = Facts {
            ttfb_ms: ttfb(&[("甲", 100), ("乙", 400)]),
            ..Default::default()
        };
        let got = count(&g, &mut f, 6250 + 391);
        assert_eq!((got["甲"], got["乙"]), (6250, 391));
        assert!(f.current_weight.values().all(|v| *v == 0), "{f:?}");
        // 只按权重分时，同样的样本不起作用：挨个轮
        let g = balanced(&[("甲", 1), ("乙", 1)], BalanceBy::Weights);
        assert_eq!(run(&g, &names(&g), &mut f, 4), ["甲", "乙", "甲", "乙"]);
    }

    /// 按成败分：常失败的那一家分得少，但还在轮里 —— 成功率 0.1 的系数本该是 0.01，
    /// 抬到下限 0.05，每 21 个新对话里有它一个，它恢复了才看得出来
    #[test]
    fn health_demotes_a_flaky_member_but_keeps_it_in_rotation() {
        let g = balanced(&[("稳", 1), ("抖", 1)], BalanceBy::Health);
        let mut f = Facts {
            success: [("稳".to_string(), 1.0), ("抖".to_string(), 0.1)]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let firsts = run(&g, &names(&g), &mut f, 1050);
        assert_eq!(firsts.iter().filter(|m| *m == "抖").count(), 50);
        for window in firsts.chunks(21) {
            assert!(window.iter().any(|m| m == "抖"), "{window:?}");
        }
        // 一次都没成功过的也一样：停用它是熔断的事，这里只是少分
        f.success.insert("抖".into(), 0.0);
        f.current_weight.clear();
        let got = count(&g, &mut f, 1050);
        assert_eq!(got["抖"], 50);
    }

    /// 权重和系数相乘：3:1 的组，慢的那一家（权重 3）系数 0.39，快的（权重 1）系数 6.25
    /// —— 有效权重 1172 : 6250，快的那一家反过来分得多
    #[test]
    fn weights_and_factors_multiply() {
        let g = balanced(&[("慢", 3), ("快", 1)], BalanceBy::LatencyHealth);
        let mut f = Facts {
            ttfb_ms: ttfb(&[("慢", 400), ("快", 100)]),
            ..Default::default()
        };
        let got = count(&g, &mut f, 1172 + 6250);
        assert_eq!((got["慢"], got["快"]), (1172, 6250));
        // 再加上成败：快的那一家成功率 0.5，系数再乘 0.25 —— 6.25 × 0.25 × 1000 = 1563
        let mut f = Facts {
            ttfb_ms: ttfb(&[("慢", 400), ("快", 100)]),
            success: [("快".to_string(), 0.5)].into_iter().collect(),
            ..Default::default()
        };
        let got = count(&g, &mut f, 1172 + 1563);
        assert_eq!((got["慢"], got["快"]), (1172, 1563));
    }

    /// 各家系数一样时，次序和只按权重分一模一样：7:3 照样穿插着来，不是先七后三
    #[test]
    fn equal_factors_keep_the_exact_interleaving() {
        let want = ["甲", "乙", "甲", "甲", "甲", "乙", "甲", "甲", "乙", "甲"];
        let cases = [
            // 一样快：系数都是 1
            Facts {
                ttfb_ms: ttfb(&[("甲", 200), ("乙", 200)]),
                ..Default::default()
            },
            // 成功率一样：系数都是 0.25，比例不变
            Facts {
                success: [("甲".to_string(), 0.5), ("乙".to_string(), 0.5)]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        ];
        for mut f in cases {
            let g = balanced(&[("甲", 7), ("乙", 3)], BalanceBy::LatencyHealth);
            assert_eq!(run(&g, &names(&g), &mut f, 10), want, "{f:?}");
            assert!(f.current_weight.values().all(|v| *v == 0), "{f:?}");
        }
    }
}
