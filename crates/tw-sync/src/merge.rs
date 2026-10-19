//! 三方合并：以共同的上一版本为基准，本机当前配置和同步位置的最新版本按**合并单位**逐一比较。
//!
//! 一个对象（上游、路由、插件……）拆成几个合并单位（上游是连接、模型、计费、其他四组；全局
//! 设置逐个字段；别的对象整个一个单位），拆法由 `tw_pack::model` 定。这里只认「对象 → 单位名
//! → 规范化的值」，所以和配置长什么样无关，表驱动测试直接写值。
//!
//! | 本机相对基准 | 同步位置相对基准 | 结果 |
//! |---|---|---|
//! | 未变 | 未变 | 不变 |
//! | 已修改或新增 | 未变 | 采用本机 |
//! | 未变 | 已修改或新增 | 采用同步位置 |
//! | 已删除 | 未变 | 删除 |
//! | 未变 | 已删除 | 删除 |
//! | 已修改 | 已修改，内容相同 | 不变 |
//! | 已修改或新增 | 已修改或新增，内容不同 | 冲突 |
//! | 已删除 | 已修改 | 冲突 |
//! | 已修改 | 已删除 | 冲突 |
//!
//! **冲突不阻断合并**：其余单位照常合并。冲突的那几个单位（或者那个被删的对象）两边各留各的
//! —— 本机的结果里是本机的，要上传的结果里是同步位置上已有的，等人处理。
//!
//! 没有基准（基准版本哪里都找不到）时按两方比较：两边都有、内容不同的单位是冲突；只有一边
//! 有的对象当作那一边新增的，不当作另一边删掉的 —— 分不清的时候，留着比删掉好。

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

/// 一个对象：合并单位 → 规范化的值
pub type Units = BTreeMap<String, Value>;

/// 一份配置拆成的对象
pub type Objects<K> = BTreeMap<K, Units>;

/// 冲突的形状
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 两边都改了（或者都新增了）同一个单位，内容不同
    Modified,
    /// 本机删了，同步位置改了
    LocalDeleted,
    /// 本机改了，同步位置删了
    RemoteDeleted,
}

/// 这次合并新发现的一处冲突
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found<K> {
    pub key: K,
    /// 冲突的单位。一边删了对象时是另一边改过的那几个
    pub units: BTreeSet<String>,
    pub kind: Kind,
}

/// 合并的输入
pub struct Input<'a, K> {
    /// 共同的上一版本。`None` 是哪里都找不到，按两方比较
    pub base: Option<&'a Objects<K>>,
    pub local: &'a Objects<K>,
    pub remote: &'a Objects<K>,
    /// 整个对象各留各的，不比较：不同步的上游和随之不同步的项目，以及一边删了对象、还没
    /// 处理的冲突
    pub hold_items: &'a BTreeSet<K>,
    /// 这几个单位各留各的：还没处理的冲突
    pub hold_units: &'a BTreeSet<(K, String)>,
    /// 逐字段合并、全是默认值时整个对象不在的那几种（全局设置的一节、Lite 的通用偏好）：
    /// **不在就是每个字段都是默认值**，不是被删了。一边把一节改回默认、另一边改了这一节的
    /// 另一个字段，各管各的字段，不是「删了对改了」
    pub fieldwise: &'a dyn Fn(&K) -> bool,
}

/// 合并的结果
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome<K> {
    /// 本机应用的样子
    pub local: Objects<K>,
    /// 上传的样子
    pub shared: Objects<K>,
    pub conflicts: Vec<Found<K>>,
}

/// 一个单位在一边的样子：没有这个对象、或者对象里没有这个单位都是 `None`
fn unit<'a>(o: Option<&'a Units>, u: &str) -> Option<&'a Value> {
    o.and_then(|m| m.get(u))
}

pub fn merge<K: Ord + Clone>(i: Input<'_, K>) -> Outcome<K> {
    let empty = Objects::new();
    let base = i.base.unwrap_or(&empty);
    let mut out = Outcome {
        local: Objects::new(),
        shared: Objects::new(),
        conflicts: Vec::new(),
    };
    let keys: BTreeSet<&K> = base
        .keys()
        .chain(i.local.keys())
        .chain(i.remote.keys())
        .collect();
    let none = Units::new();
    for k in keys {
        let (mut b, mut l, mut r) = (base.get(k), i.local.get(k), i.remote.get(k));
        let fieldwise = (i.fieldwise)(k);
        if fieldwise {
            // 没有基准时也照样：不在就是全是默认值
            b = b.or(Some(&none));
            l = l.or(Some(&none));
            r = r.or(Some(&none));
        }
        let mut put = |local: Option<&Units>, shared: Option<&Units>| {
            // 逐字段的那几种合并出来一个字段都没有：全是默认值，不写
            if let Some(v) = local.filter(|v| !(fieldwise && v.is_empty())) {
                out.local.insert(k.clone(), v.clone());
            }
            if let Some(v) = shared.filter(|v| !(fieldwise && v.is_empty())) {
                out.shared.insert(k.clone(), v.clone());
            }
        };
        if i.hold_items.contains(k) {
            put(l, r);
            continue;
        }
        match (b, l, r) {
            (_, None, None) => {}
            // 有基准，一边删了
            (Some(b), None, Some(r)) => {
                if r == b {
                    // 同步位置没动、本机删了：删
                } else {
                    put(None, Some(r));
                    out.conflicts.push(Found {
                        key: k.clone(),
                        units: changed(b, r),
                        kind: Kind::LocalDeleted,
                    });
                }
            }
            (Some(b), Some(l), None) => {
                if l == b {
                    // 本机没动、同步位置删了：删
                } else {
                    put(Some(l), None);
                    out.conflicts.push(Found {
                        key: k.clone(),
                        units: changed(b, l),
                        kind: Kind::RemoteDeleted,
                    });
                }
            }
            // 没有基准、只有一边有：那一边新增的
            (None, Some(l), None) => put(Some(l), Some(l)),
            (None, None, Some(r)) => put(Some(r), Some(r)),
            // 两边都有：逐个单位比
            (b, Some(l), Some(r)) => {
                let names: BTreeSet<&String> = b
                    .into_iter()
                    .flat_map(|m| m.keys())
                    .chain(l.keys())
                    .chain(r.keys())
                    .collect();
                let (mut lo, mut sh) = (Units::new(), Units::new());
                let mut clash = BTreeSet::new();
                for u in names {
                    let (bv, lv, rv) = (unit(b, u), l.get(u), r.get(u));
                    let mut set = |lo_v: Option<&Value>, sh_v: Option<&Value>| {
                        if let Some(v) = lo_v {
                            lo.insert(u.clone(), v.clone());
                        }
                        if let Some(v) = sh_v {
                            sh.insert(u.clone(), v.clone());
                        }
                    };
                    if i.hold_units.contains(&(k.clone(), u.clone())) {
                        set(lv, rv);
                    } else if lv == rv || rv == bv {
                        // 两边一样，或者同步位置没动：本机的
                        set(lv, lv);
                    } else if lv == bv {
                        // 本机没动：同步位置的
                        set(rv, rv);
                    } else {
                        set(lv, rv);
                        clash.insert(u.clone());
                    }
                }
                put(Some(&lo), Some(&sh));
                if !clash.is_empty() {
                    out.conflicts.push(Found {
                        key: k.clone(),
                        units: clash,
                        kind: Kind::Modified,
                    });
                }
            }
        }
    }
    out
}

/// 两个样子之间变了的单位
pub fn changed(a: &Units, b: &Units) -> BTreeSet<String> {
    a.keys()
        .chain(b.keys())
        .filter(|u| a.get(*u) != b.get(*u))
        .cloned()
        .collect()
}

/// 一份配置相对基准有没有变（不看 `hold_items`、`hold_units` 里的）。决定要不要上传
pub fn differs<K: Ord + Clone>(
    base: &Objects<K>,
    now: &Objects<K>,
    hold_items: &BTreeSet<K>,
    hold_units: &BTreeSet<(K, String)>,
) -> bool {
    let keys: BTreeSet<&K> = base.keys().chain(now.keys()).collect();
    keys.into_iter().any(|k| {
        if hold_items.contains(k) {
            return false;
        }
        match (base.get(k), now.get(k)) {
            (None, None) => false,
            (Some(_), None) | (None, Some(_)) => true,
            (Some(a), Some(b)) => changed(a, b)
                .iter()
                .any(|u| !hold_units.contains(&(k.clone(), u.clone()))),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 一个对象：单位名和值成对写
    fn obj(units: &[(&str, Value)]) -> Units {
        units
            .iter()
            .map(|(u, v)| (u.to_string(), v.clone()))
            .collect()
    }

    fn objs(items: &[(&str, Option<Units>)]) -> Objects<String> {
        items
            .iter()
            .filter_map(|(k, v)| v.clone().map(|v| (k.to_string(), v)))
            .collect()
    }

    fn run(
        base: Option<Option<Units>>,
        local: Option<Units>,
        remote: Option<Units>,
    ) -> Outcome<String> {
        let b = base.map(|b| objs(&[("p", b)]));
        let l = objs(&[("p", local)]);
        let r = objs(&[("p", remote)]);
        merge(Input {
            base: b.as_ref(),
            local: &l,
            remote: &r,
            hold_items: &BTreeSet::new(),
            hold_units: &BTreeSet::new(),
            fieldwise: &|_| false,
        })
    }

    /// 方案 6.6 的每一行，在一个合并单位上，外加两边都删、两边都新增、没有基准。
    ///
    /// 每一行：基准、本机、同步位置（`None` 是没有这个对象），期望本机的结果、上传的结果、
    /// 冲突的形状。
    #[test]
    fn every_row_of_the_merge_table() {
        let a = || Some(obj(&[("conn", json!("A"))]));
        let b2 = || Some(obj(&[("conn", json!("B"))]));
        let c = || Some(obj(&[("conn", json!("C"))]));
        type Row = (
            &'static str,
            Option<Option<Units>>,
            Option<Units>,
            Option<Units>,
            Option<Units>,
            Option<Units>,
            Option<Kind>,
        );
        let rows: Vec<Row> = vec![
            ("未变 / 未变 → 不变", Some(a()), a(), a(), a(), a(), None),
            (
                "本机修改 / 未变 → 采用本机",
                Some(a()),
                b2(),
                a(),
                b2(),
                b2(),
                None,
            ),
            (
                "本机新增 / 未变 → 采用本机",
                Some(None),
                b2(),
                None,
                b2(),
                b2(),
                None,
            ),
            (
                "未变 / 同步位置修改 → 采用同步位置",
                Some(a()),
                a(),
                b2(),
                b2(),
                b2(),
                None,
            ),
            (
                "未变 / 同步位置新增 → 采用同步位置",
                Some(None),
                None,
                b2(),
                b2(),
                b2(),
                None,
            ),
            (
                "本机删除 / 未变 → 删除",
                Some(a()),
                None,
                a(),
                None,
                None,
                None,
            ),
            (
                "未变 / 同步位置删除 → 删除",
                Some(a()),
                a(),
                None,
                None,
                None,
                None,
            ),
            ("两边都删 → 删除", Some(a()), None, None, None, None, None),
            (
                "两边改成一样 → 不变",
                Some(a()),
                b2(),
                b2(),
                b2(),
                b2(),
                None,
            ),
            (
                "两边新增成一样 → 不变",
                Some(None),
                b2(),
                b2(),
                b2(),
                b2(),
                None,
            ),
            (
                "两边改得不一样 → 冲突",
                Some(a()),
                b2(),
                c(),
                b2(),
                c(),
                Some(Kind::Modified),
            ),
            (
                "两边新增得不一样 → 冲突",
                Some(None),
                b2(),
                c(),
                b2(),
                c(),
                Some(Kind::Modified),
            ),
            (
                "本机删除 / 同步位置修改 → 冲突",
                Some(a()),
                None,
                b2(),
                None,
                b2(),
                Some(Kind::LocalDeleted),
            ),
            (
                "本机修改 / 同步位置删除 → 冲突",
                Some(a()),
                b2(),
                None,
                b2(),
                None,
                Some(Kind::RemoteDeleted),
            ),
            ("没有基准，一样 → 不变", None, a(), a(), a(), a(), None),
            (
                "没有基准，不一样 → 冲突",
                None,
                a(),
                b2(),
                a(),
                b2(),
                Some(Kind::Modified),
            ),
            (
                "没有基准，只有本机有 → 留着",
                None,
                a(),
                None,
                a(),
                a(),
                None,
            ),
            (
                "没有基准，只有同步位置有 → 拿来",
                None,
                None,
                a(),
                a(),
                a(),
                None,
            ),
        ];
        for (what, base, local, remote, want_local, want_shared, want_conflict) in rows {
            let o = run(base, local, remote);
            assert_eq!(o.local.get("p").cloned(), want_local, "{what}：本机");
            assert_eq!(o.shared.get("p").cloned(), want_shared, "{what}：上传");
            assert_eq!(
                o.conflicts.first().map(|c| c.kind),
                want_conflict,
                "{what}：冲突"
            );
            if want_conflict.is_some() {
                assert_eq!(o.conflicts.len(), 1, "{what}");
                assert_eq!(o.conflicts[0].key, "p", "{what}");
                assert_eq!(
                    o.conflicts[0].units,
                    BTreeSet::from(["conn".to_string()]),
                    "{what}"
                );
            }
        }
    }

    /// 同一个上游，两边改的是不同的单位：自动合并，没有冲突
    #[test]
    fn changes_to_different_units_of_one_object_merge_by_themselves() {
        let base = obj(&[
            ("conn", json!("A")),
            ("models", json!(["m1"])),
            ("billing", json!("x")),
        ]);
        let local = obj(&[
            ("conn", json!("A2")),
            ("models", json!(["m1"])),
            ("billing", json!("x")),
        ]);
        let remote = obj(&[
            ("conn", json!("A")),
            ("models", json!(["m1", "m2"])),
            ("billing", json!("x")),
        ]);
        let o = run(Some(Some(base)), Some(local), Some(remote));
        let want = obj(&[
            ("conn", json!("A2")),
            ("models", json!(["m1", "m2"])),
            ("billing", json!("x")),
        ]);
        assert!(o.conflicts.is_empty(), "{:?}", o.conflicts);
        assert_eq!(o.local["p"], want);
        assert_eq!(o.shared["p"], want);
    }

    /// 一个单位冲突，其余单位照常合并；冲突的那一个本机留本机的、上传留同步位置的
    #[test]
    fn a_conflict_on_one_unit_does_not_hold_back_the_others() {
        let base = obj(&[("conn", json!("A")), ("models", json!(1))]);
        let local = obj(&[("conn", json!("L")), ("models", json!(1))]);
        let remote = obj(&[("conn", json!("R")), ("models", json!(2))]);
        let o = run(Some(Some(base)), Some(local), Some(remote));
        assert_eq!(
            o.local["p"],
            obj(&[("conn", json!("L")), ("models", json!(2))])
        );
        assert_eq!(
            o.shared["p"],
            obj(&[("conn", json!("R")), ("models", json!(2))])
        );
        assert_eq!(o.conflicts.len(), 1);
        assert_eq!(o.conflicts[0].units, BTreeSet::from(["conn".to_string()]));
    }

    /// 删了的一边对改了的一边：冲突列出的是改了的那几个单位
    #[test]
    fn a_delete_against_an_edit_names_the_edited_units() {
        let base = obj(&[
            ("conn", json!("A")),
            ("models", json!(1)),
            ("other", json!(0)),
        ]);
        let remote = obj(&[
            ("conn", json!("A")),
            ("models", json!(2)),
            ("other", json!(5)),
        ]);
        let o = run(Some(Some(base)), None, Some(remote.clone()));
        assert_eq!(o.local.get("p"), None);
        assert_eq!(o.shared.get("p"), Some(&remote));
        assert_eq!(
            o.conflicts[0].units,
            BTreeSet::from(["models".to_string(), "other".to_string()])
        );
    }

    /// 不同步的项目、还没处理的冲突：两边各留各的，不比较，也不再报一次冲突
    #[test]
    fn held_items_and_units_pass_through_on_each_side() {
        let base = objs(&[
            ("ollama", Some(obj(&[("conn", json!("base"))]))),
            (
                "relay",
                Some(obj(&[("conn", json!("A")), ("models", json!(1))])),
            ),
        ]);
        let local = objs(&[
            ("ollama", Some(obj(&[("conn", json!("local"))]))),
            (
                "relay",
                Some(obj(&[("conn", json!("L")), ("models", json!(1))])),
            ),
        ]);
        let remote = objs(&[
            ("ollama", Some(obj(&[("conn", json!("remote"))]))),
            (
                "relay",
                Some(obj(&[("conn", json!("R")), ("models", json!(2))])),
            ),
        ]);
        let hold_items = BTreeSet::from(["ollama".to_string()]);
        let hold_units = BTreeSet::from([("relay".to_string(), "conn".to_string())]);
        let o = merge(Input {
            base: Some(&base),
            local: &local,
            remote: &remote,
            hold_items: &hold_items,
            hold_units: &hold_units,
            fieldwise: &|_| false,
        });
        assert!(o.conflicts.is_empty(), "{:?}", o.conflicts);
        assert_eq!(o.local["ollama"], obj(&[("conn", json!("local"))]));
        assert_eq!(o.shared["ollama"], obj(&[("conn", json!("remote"))]));
        assert_eq!(
            o.local["relay"],
            obj(&[("conn", json!("L")), ("models", json!(2))])
        );
        assert_eq!(
            o.shared["relay"],
            obj(&[("conn", json!("R")), ("models", json!(2))])
        );
        // 只有本机有、不同步的：不上传
        let local_only = objs(&[("ollama", Some(obj(&[("conn", json!("local"))])))]);
        let o = merge(Input {
            base: None,
            local: &local_only,
            remote: &Objects::new(),
            hold_items: &hold_items,
            hold_units: &BTreeSet::new(),
            fieldwise: &|_| false,
        });
        assert!(o.shared.is_empty());
        assert_eq!(o.local.len(), 1);
    }

    /// 全局设置一节：一边改回了默认（整节不在了），另一边改了这一节的另一个字段 —— 各管各的
    /// 字段，不是删了对改了
    #[test]
    fn a_field_level_object_reset_to_defaults_is_not_a_delete() {
        let base = objs(&[("failover", Some(obj(&[("pause_secs", json!(100))])))]);
        let local = objs(&[(
            "failover",
            Some(obj(&[
                ("pause_secs", json!(100)),
                ("failures_to_pause", json!(7)),
            ])),
        )]);
        let remote = Objects::new();
        let o = merge(Input {
            base: Some(&base),
            local: &local,
            remote: &remote,
            hold_items: &BTreeSet::new(),
            hold_units: &BTreeSet::new(),
            fieldwise: &|k: &String| k == "failover",
        });
        assert!(o.conflicts.is_empty(), "{:?}", o.conflicts);
        let want = obj(&[("failures_to_pause", json!(7))]);
        assert_eq!(o.local["failover"], want);
        assert_eq!(o.shared["failover"], want);
        // 两边都改回默认：整节不在
        let o = merge(Input {
            base: Some(&base),
            local: &Objects::new(),
            remote: &Objects::new(),
            hold_items: &BTreeSet::new(),
            hold_units: &BTreeSet::new(),
            fieldwise: &|_: &String| true,
        });
        assert!(o.local.is_empty() && o.shared.is_empty() && o.conflicts.is_empty());
    }

    #[test]
    fn whether_there_is_anything_to_upload() {
        let base = objs(&[("a", Some(obj(&[("conn", json!(1)), ("models", json!(1))])))]);
        let same = base.clone();
        let none = BTreeSet::new();
        assert!(!differs(&base, &same, &none, &BTreeSet::new()));
        let edited = objs(&[("a", Some(obj(&[("conn", json!(2)), ("models", json!(1))])))]);
        assert!(differs(&base, &edited, &none, &BTreeSet::new()));
        // 改的正是还没处理的冲突那一个单位：不算
        let held = BTreeSet::from([("a".to_string(), "conn".to_string())]);
        assert!(!differs(&base, &edited, &none, &held));
        // 新增了一个不同步的上游：不算
        let mut more = base.clone();
        more.insert("ollama".into(), obj(&[("conn", json!(1))]));
        assert!(differs(&base, &more, &none, &BTreeSet::new()));
        assert!(!differs(
            &base,
            &more,
            &BTreeSet::from(["ollama".to_string()]),
            &BTreeSet::new()
        ));
    }
}
