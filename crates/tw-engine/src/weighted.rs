//! 策略组的成员和 `load-balance` 的权重：成员怎么写，平滑加权轮询怎么轮。
//!
//! 轮法是 nginx 的平滑加权轮询：每排一次，这一轮的每个成员给自己的「当前权重」加上
//! 自己的权重，加完最大的那个排头，排头的再减去这一轮的总权重。7:3 排出来是
//! 甲乙甲甲甲乙甲甲乙甲 —— 穿插着来，而不是先连着七次甲、再连着三次乙；权重都一样
//! 就是挨个轮。
//!
//! **当前权重不在这里**：它由网关记着（`tw_gateway::balance`），排序时经
//! [`Facts::current_weight`] 传进来，排完、会话粘性也定了之后，网关按 [`advance`]
//! 记账。引擎因此还是纯函数：试算拿同一份状态算、不记账，说的就是数据面下一个新对话
//! 会排给谁。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::engine::{Facts, Group};

/// 权重最小是 1：0 等于把它从组里拿掉，那就该真的拿掉
pub const WEIGHT_MIN: u32 = 1;

/// 权重最大是 100：比例上够用，当前权重也不会大到溢出
pub const WEIGHT_MAX: u32 = 100;

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

/// 这一轮参加轮询的成员和它们的权重，按 `members` 的顺序。
///
/// **停着的不算**（[`Facts::paused`]）：熔断、冷却着的那一家这一次本来就会被跳过，
/// 轮到它的那一次落到组里排在它后面的那一家，那一家就平白多拿一份。全都停着时都算
/// —— 那时网关照样一家家试（fail-open），排头的还是按权重来。
fn round<'a>(g: &Group, members: &'a [String], f: &Facts) -> Vec<(&'a str, i64)> {
    let up: Vec<&'a String> = members.iter().filter(|m| !f.paused.contains(*m)).collect();
    let pool = if up.is_empty() {
        members.iter().collect()
    } else {
        up
    };
    pool.into_iter()
        .map(|m| (m.as_str(), i64::from(g.weight(m))))
        .collect()
}

/// 下一个排头：当前权重加上自己的权重，最大的那个；一样大取组里靠前的。
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

/// 记一次账，交回记过之后的当前权重：这一轮的每个成员加上自己的权重，`leader` 再减去
/// 这一轮的总权重。
///
/// `leader` 是**会话粘性之后实际排头的那一家**，不一定是 [`lead`] 挑的那个：一段对话
/// 留在了上次回答它的那一家，这一次就记在那一家头上，之后的新对话把差的补回去。
/// `members` 和 `f` 要和排序时的一样（同一轮）。`leader` 不在这一轮里时什么都不记。
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
    use crate::engine::{GroupType, order_by};

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
}
