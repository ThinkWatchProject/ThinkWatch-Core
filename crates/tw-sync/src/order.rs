//! 上游、路由、插件的先后顺序：**内容和顺序分开合并**。
//!
//! 一边新增了一条路由、另一边新增了另一条，两边的顺序都变了 —— 这不该是冲突。所以顺序不按
//! 单位整个比，而是：
//!
//! 1. 两边都有、基准里也有的那些名字，比它们的相对先后。只有一边改了就用那一边的；两边改得
//!    一样也行；两边改得不一样是冲突（本机留本机的，上传的留同步位置的）。
//! 2. 新来的名字插在它在自己那一边的前一个名字后面。
//! 3. 最后只留下合并之后还在的那些项目。

use std::collections::BTreeSet;

/// 顺序合并的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    pub local: Vec<String>,
    pub shared: Vec<String>,
    /// 两边把同一批名字排成了不同的先后
    pub conflict: bool,
}

fn restrict(list: &[String], keep: &BTreeSet<&str>) -> Vec<String> {
    list.iter()
        .filter(|n| keep.contains(n.as_str()))
        .cloned()
        .collect()
}

/// `primary` 打底，把 `present` 里还没排上的名字按 `sources`（先看的在前）里各自的前一个
/// 名字插进去，最后只留 `present` 里有的
fn arrange(primary: &[String], sources: &[&[String]], present: &BTreeSet<String>) -> Vec<String> {
    let mut out: Vec<String> = primary
        .iter()
        .filter(|n| present.contains(*n))
        .cloned()
        .collect();
    for src in sources {
        for (i, n) in src.iter().enumerate() {
            if !present.contains(n) || out.contains(n) {
                continue;
            }
            // 它在这一边的前一个、已经排上了的名字
            let after = src[..i].iter().rev().find(|p| out.contains(*p));
            match after {
                Some(p) => {
                    let at = out.iter().position(|x| x == p).map_or(out.len(), |i| i + 1);
                    out.insert(at, n.clone());
                }
                None => out.insert(0, n.clone()),
            }
        }
    }
    // 哪一边的顺序里都没有的（不该有）：接在后面
    for n in present {
        if !out.contains(n) {
            out.push(n.clone());
        }
    }
    out
}

/// 合并一种顺序。`local_names`、`shared_names` 是合并之后两份结果里各自还在的那些项目
pub fn merge(
    base: Option<&[String]>,
    local: &[String],
    remote: &[String],
    local_names: &BTreeSet<String>,
    shared_names: &BTreeSet<String>,
    hold: bool,
) -> Merged {
    let base = base.unwrap_or(&[]);
    let common: BTreeSet<&str> = local
        .iter()
        .filter(|n| remote.contains(n) && (base.is_empty() || base.contains(n)))
        .map(String::as_str)
        .collect();
    let (b, l, r) = (
        restrict(base, &common),
        restrict(local, &common),
        restrict(remote, &common),
    );
    let (l_changed, r_changed) = (!base.is_empty() && l != b, !base.is_empty() && r != b);
    let no_base_disagree = base.is_empty() && l != r;
    let conflict = !hold && ((l_changed && r_changed && l != r) || no_base_disagree);
    let (local_primary, shared_primary): (&[String], &[String]) = if hold || conflict {
        (local, remote)
    } else if l_changed && !r_changed {
        (local, local)
    } else {
        (remote, remote)
    };
    Merged {
        local: arrange(local_primary, &[local, remote], local_names),
        shared: arrange(shared_primary, &[remote, local], shared_names),
        conflict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }
    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn two_sides_adding_different_items_is_not_a_conflict() {
        let m = merge(
            Some(&v(&["a", "b"])),
            &v(&["a", "b", "l"]),
            &v(&["r", "a", "b"]),
            &set(&["a", "b", "l", "r"]),
            &set(&["a", "b", "l", "r"]),
            false,
        );
        assert!(!m.conflict);
        assert_eq!(m.local, v(&["r", "a", "b", "l"]));
        assert_eq!(m.shared, m.local);
    }

    #[test]
    fn one_side_reordering_wins_and_both_reordering_differently_conflicts() {
        let all = set(&["a", "b", "c"]);
        let m = merge(
            Some(&v(&["a", "b", "c"])),
            &v(&["c", "a", "b"]),
            &v(&["a", "b", "c"]),
            &all,
            &all,
            false,
        );
        assert_eq!((m.local.clone(), m.conflict), (v(&["c", "a", "b"]), false));
        assert_eq!(m.shared, m.local);
        let m = merge(
            Some(&v(&["a", "b", "c"])),
            &v(&["c", "a", "b"]),
            &v(&["b", "a", "c"]),
            &all,
            &all,
            false,
        );
        assert!(m.conflict);
        assert_eq!(m.local, v(&["c", "a", "b"]));
        assert_eq!(m.shared, v(&["b", "a", "c"]));
        // 两边排成一样的：不冲突
        let m = merge(
            Some(&v(&["a", "b"])),
            &v(&["b", "a"]),
            &v(&["b", "a"]),
            &set(&["a", "b"]),
            &set(&["a", "b"]),
            false,
        );
        assert!(!m.conflict);
    }

    #[test]
    fn deleted_and_local_only_names_drop_out_of_the_upload() {
        // 本机有一个不同步的 ollama：本机的顺序里留着，上传的没有
        let m = merge(
            Some(&v(&["ollama", "relay", "official"])),
            &v(&["ollama", "relay", "official"]),
            &v(&["relay", "official"]),
            &set(&["ollama", "relay", "official"]),
            &set(&["relay", "official"]),
            false,
        );
        assert!(!m.conflict);
        assert_eq!(m.local, v(&["ollama", "relay", "official"]));
        assert_eq!(m.shared, v(&["relay", "official"]));
    }

    #[test]
    fn a_held_order_keeps_each_side_without_raising_it_again() {
        let all = set(&["a", "b"]);
        let m = merge(
            Some(&v(&["a", "b"])),
            &v(&["b", "a"]),
            &v(&["a", "b"]),
            &all,
            &all,
            true,
        );
        assert!(!m.conflict);
        assert_eq!(m.local, v(&["b", "a"]));
        assert_eq!(m.shared, v(&["a", "b"]));
    }
}
