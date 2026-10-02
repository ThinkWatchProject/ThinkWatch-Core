//! 拼起来给插件看的一段文字，改完之后落回原来的那几段。
//!
//! 系统提示在原文里常常是几段：Anthropic 的几个文字块（最后一块上挂着缓存断点）、
//! Chat 开头的几条 system 消息、Gemini `systemInstruction` 的几个部分。插件看到的是
//! 用分隔符拼起来的一整段；它改完之后，**能对上的段原样留着**（连同块上的缓存断点），
//! 只有中间改了的那几段换掉：
//!
//! - 在末尾接一段 → 新加一段，原来的都不动（缓存的前缀还在）；
//! - 在开头加一段 → 新加一段放在最前；
//! - 改了某一段 → 只换那一段，它身上的别的字段留着；
//! - 中间几段改得对不上段数了 → 整个落在这几段的最后一段上（缓存断点多半在那儿），
//!   其余几段去掉。

/// 一段的去向。顺序就是改完之后的顺序；没出现的段是删掉了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    Keep(usize),
    Replace(usize, String),
    Insert(String),
}

/// `old` 用 `sep` 拼起来之后被改成了 `new`：每一段怎么办。
///
/// **按结果拼回去一定就是 `new`**（测试钉着这一条）。`old` 里不该有空段。
pub fn diff(old: &[&str], sep: &str, new: &str) -> Vec<Seg> {
    let n = old.len();
    if old.join(sep) == new {
        return (0..n).map(Seg::Keep).collect();
    }
    if n == 0 {
        return if new.is_empty() {
            Vec::new()
        } else {
            vec![Seg::Insert(new.to_string())]
        };
    }
    // 开头对得上几段：那几段之后要么到头，要么紧跟分隔符
    let mut p = 0;
    for k in (1..=n).rev() {
        let head = old[..k].join(sep);
        if new.starts_with(&head) && (new.len() == head.len() || new[head.len()..].starts_with(sep))
        {
            p = k;
            break;
        }
    }
    let head_end = if p > 0 { old[..p].join(sep).len() } else { 0 };
    // 结尾对得上几段，不和开头那几段重叠。夹在中间的那一截要么正好是共用的一个分隔符，
    // 要么是「分隔符 + 中间的字 + 分隔符」
    let mut q = 0;
    for k in (1..=(n - p)).rev() {
        let tail = old[n - k..].join(sep);
        if new.len() < tail.len() || !new.ends_with(&tail) {
            continue;
        }
        let start = new.len() - tail.len();
        if start < head_end || !new.is_char_boundary(start) {
            continue;
        }
        let region = &new[head_end..start];
        let ok = if p > 0 {
            region == sep
                || (region.len() >= 2 * sep.len()
                    && region.starts_with(sep)
                    && region.ends_with(sep))
        } else {
            region.is_empty() || region.ends_with(sep)
        };
        if ok {
            q = k;
            break;
        }
    }
    let tail_start = new.len()
        - if q > 0 {
            old[n - q..].join(sep).len()
        } else {
            0
        };
    let region = &new[head_end..tail_start];
    // 中间什么都没有时，这一截是共用的分隔符（两头都有段时）或者空的
    let nothing = if p > 0 && q > 0 { sep } else { "" };
    let present = region != nothing;
    let mut mid = region;
    if present {
        if p > 0 {
            mid = &mid[sep.len()..];
        }
        if q > 0 {
            mid = &mid[..mid.len() - sep.len()];
        }
    }
    let mut out: Vec<Seg> = (0..p).map(Seg::Keep).collect();
    let middle: Vec<usize> = (p..n - q).collect();
    if present {
        if middle.is_empty() {
            out.push(Seg::Insert(mid.to_string()));
        } else {
            let pieces: Vec<&str> = mid.split(sep).collect();
            if pieces.len() == middle.len() {
                for (&i, piece) in middle.iter().zip(pieces) {
                    out.push(if piece == old[i] {
                        Seg::Keep(i)
                    } else {
                        Seg::Replace(i, piece.to_string())
                    });
                }
            } else {
                out.push(Seg::Replace(
                    *middle.last().expect("not empty"),
                    mid.to_string(),
                ));
            }
        }
    }
    out.extend((n - q..n).map(Seg::Keep));
    out
}

/// 按 `diff` 的结果拼回去，测试用来验证「落回去之后拼起来就是插件写的那一段」
#[cfg(test)]
pub fn rejoin(old: &[&str], sep: &str, segs: &[Seg]) -> String {
    segs.iter()
        .map(|s| match s {
            Seg::Keep(i) => old[*i].to_string(),
            Seg::Replace(_, t) | Seg::Insert(t) => t.clone(),
        })
        .collect::<Vec<_>>()
        .join(sep)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEP: &str = "\n\n";

    #[test]
    fn appending_a_paragraph_adds_a_segment_and_keeps_the_rest() {
        let old = ["账单头", "你是 Claude Code", "很长的系统提示"];
        let new = format!("{}\n\n今天是 2026-10-02", old.join(SEP));
        assert_eq!(
            diff(&old, SEP, &new),
            [
                Seg::Keep(0),
                Seg::Keep(1),
                Seg::Keep(2),
                Seg::Insert("今天是 2026-10-02".into())
            ]
        );
    }

    #[test]
    fn prepending_and_editing_one_segment_touch_only_that_much() {
        let old = ["A", "B", "C"];
        assert_eq!(
            diff(&old, SEP, "X\n\nA\n\nB\n\nC"),
            [
                Seg::Insert("X".into()),
                Seg::Keep(0),
                Seg::Keep(1),
                Seg::Keep(2)
            ]
        );
        assert_eq!(
            diff(&old, SEP, "A\n\nB2\n\nC"),
            [Seg::Keep(0), Seg::Replace(1, "B2".into()), Seg::Keep(2)]
        );
        assert_eq!(diff(&old, SEP, "A\n\nC"), [Seg::Keep(0), Seg::Keep(2)]);
        assert_eq!(diff(&old, SEP, ""), []);
        // 末尾直接接字、不带分隔符：改的是最后一段
        assert_eq!(
            diff(&old, SEP, "A\n\nB\n\nC!"),
            [Seg::Keep(0), Seg::Keep(1), Seg::Replace(2, "C!".into())]
        );
    }

    #[test]
    fn a_rewrite_that_does_not_line_up_lands_on_the_last_middle_segment() {
        let old = ["A", "B", "C", "D"];
        assert_eq!(
            diff(&old, SEP, "A\n\n全部重写\n\nD"),
            [
                Seg::Keep(0),
                Seg::Replace(2, "全部重写".into()),
                Seg::Keep(3)
            ]
        );
    }

    #[test]
    fn nothing_before_means_one_new_segment() {
        assert_eq!(diff(&[], SEP, "新的"), [Seg::Insert("新的".into())]);
        assert_eq!(diff(&[], SEP, ""), []);
    }

    #[test]
    fn whatever_the_edit_the_segments_join_back_to_it() {
        let olds: [&[&str]; 4] = [&["A"], &["A", "B"], &["A", "B", "C"], &["x", "x", "x"]];
        let news = [
            "",
            "A",
            "B",
            "AB",
            "A\n\nB",
            "B\n\nA",
            "A\n\nB\n\nC\n\nD",
            "Z\n\nA",
            "A\n\n\n\nB",
            "\n\n",
            "A\n\n",
            "\n\nA",
            "x\n\nx",
            "x",
            "完全不一样",
        ];
        for old in olds {
            for new in news {
                let segs = diff(old, SEP, new);
                assert_eq!(rejoin(old, SEP, &segs), new, "{old:?} → {new:?}: {segs:?}");
            }
        }
    }
}
