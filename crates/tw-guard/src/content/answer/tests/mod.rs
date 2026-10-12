use serde_json::{Value, json};
use tw_dialect::ir::Dialect;

use super::*;
use crate::content::{Action, Match, Outcome, RuleInput, Sides};

/// 码位用 `char::from_u32` 拼：测试里直接写出来的转义，经过某些编辑工具会变成真字符
fn ch(n: u32) -> char {
    char::from_u32(n).unwrap()
}

const ANSWER: Sides = Sides {
    request: false,
    response: true,
};

fn rule<'a>(id: &'a str, pattern: &'a str, matching: Match, action: Action) -> RuleInput<'a> {
    RuleInput {
        id,
        name: id,
        custom: true,
        pattern,
        matching,
        action,
        sides: ANSWER,
        ..RuleInput::default()
    }
}

fn answer(rules: Vec<RuleInput<'_>>) -> Arc<Answer> {
    Answer::build(&Rules::build(rules).unwrap()).expect("rules that check answers")
}

/// 一段正文切成几块喂进去，交回客户端收到的整段
fn streamed(a: &Arc<Answer>, acts: bool, pieces: &[&str]) -> String {
    let mut lane = a.lane(acts);
    let mut out = String::new();
    for p in pieces {
        out.push_str(&lane.push(p));
    }
    out.push_str(&lane.finish());
    out
}

fn whole_text(a: &Arc<Answer>, acts: bool, text: &str) -> String {
    a.lane(acts).whole(text)
}

#[test]
fn only_rules_that_check_answers_are_built_and_none_means_none() {
    let rs = Rules::build([
        RuleInput {
            id: "req",
            name: "req",
            pattern: "x",
            ..RuleInput::default()
        },
        RuleInput {
            id: "cp",
            name: "cp",
            pattern: "U+200B",
            matching: Match::Codepoints,
            sides: ANSWER,
            ..RuleInput::default()
        },
    ])
    .unwrap();
    assert!(
        Answer::build(&rs).is_none(),
        "只查请求的、码位规则都不查回答"
    );
    let a = answer(vec![rule("k", "北极星", Match::Contains, Action::Strip)]);
    assert_eq!(a.hold(), 2, "三个字的关键词扣两个字");
}

#[test]
fn contains_strips_replaces_and_records() {
    let a = answer(vec![
        rule("s", "secret", Match::Contains, Action::Strip),
        RuleInput {
            replacement: "[隐去]",
            ..rule("r", "北极星", Match::Contains, Action::Replace)
        },
        rule("n", "note", Match::Contains, Action::Record),
    ]);
    let text = "a SECRET note about 北极星 and Secret";
    assert_eq!(whole_text(&a, true, text), "a  note about [隐去] and ");
    // 观察档一个字都不改
    assert_eq!(whole_text(&a, false, text), text);
}

#[test]
fn a_match_split_at_every_position_is_still_caught() {
    let a = answer(vec![RuleInput {
        replacement: "***",
        ..rule("r", "北极星计划", Match::Contains, Action::Replace)
    }]);
    let text = "前文北极星计划后文，再来一次北极星计划";
    let want = "前文***后文，再来一次***";
    let cuts: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    for &i in &cuts {
        for &j in cuts.iter().filter(|&&j| j >= i) {
            let out = streamed(&a, true, &[&text[..i], &text[i..j], &text[j..]]);
            assert_eq!(out, want, "切在 {i} 和 {j}");
        }
    }
    // 一个字一块
    let pieces: Vec<String> = text.chars().map(String::from).collect();
    let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
    assert_eq!(streamed(&a, true, &refs), want);
}

/// 每种切法（切成三块的每一种，和一个字一块）流着处理，交回切法、发出去的和是不是
/// 「开头已经发了」的那种
fn cuts_of(a: &Arc<Answer>, text: &str) -> Vec<(String, String, bool)> {
    let cuts: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain([text.len()])
        .collect();
    let mut ways: Vec<(String, Vec<&str>)> = Vec::new();
    for &i in &cuts {
        for &j in cuts.iter().filter(|&&j| j >= i) {
            ways.push((
                format!("切在 {i} 和 {j}"),
                vec![&text[..i], &text[i..j], &text[j..]],
            ));
        }
    }
    let chars: Vec<String> = text.chars().map(String::from).collect();
    ways.push((
        "一个字一块".into(),
        chars.iter().map(String::as_str).collect(),
    ));
    ways.into_iter()
        .map(|(how, pieces)| {
            let mut lane = a.lane(true);
            let mut out = String::new();
            for p in &pieces {
                out.push_str(&lane.push(p));
            }
            out.push_str(&lane.finish());
            (how, out, lane.tally.short)
        })
        .collect()
}

/// 切在哪儿都和整份的一样
fn every_cut(a: &Arc<Answer>, text: &str, want: &str) {
    assert_eq!(whole_text(a, true, text), want, "{text:?}");
    for (how, out, _) in cuts_of(a, text) {
        assert_eq!(out, want, "{text:?} {how}");
    }
}

/// 套得比扣住的字还深：整份的删干净；流着的，开头已经发了的那几处没发的那一截删掉，发出去
/// 的里面找不到 `strip`；其余的切法和整份的一样
fn every_cut_safe(a: &Arc<Answer>, text: &str, want: &str, strip: &str) {
    assert_eq!(whole_text(a, true, text), want, "{text:?}");
    for (how, out, short) in cuts_of(a, text) {
        assert!(!out.contains(strip), "{text:?} {how} → {out:?}");
        if !short {
            assert_eq!(out, want, "{text:?} {how}");
        }
    }
}

/// 删掉一处之后前后的字接成了新的一处（`sesecretcret` 删掉中间的 `secret` 又是一个
/// `secret`）：接着删，直到不再冒出新的。整份的和流式的一样，切在哪儿都一样
#[test]
fn nested_strips_are_removed_until_none_is_left_wherever_the_text_is_cut() {
    let a = answer(vec![rule("s", "secret", Match::Contains, Action::Strip)]);
    for (text, want) in [
        ("sesecretcret", ""),
        ("前sesecretcret后", "前后"),
        ("sesesecretcretcret and SeSECRETcret!", " and !"),
        ("se secret cret", "se  cret"),
        ("secsecretret secretsecret", " "),
    ] {
        every_cut(&a, text, want);
    }
    // 两条规则互相接出来的：删掉 `X` 之后是 `ab`，删掉 `ab` 之后又是 `X`
    let b = answer(vec![
        rule("x", "X", Match::Contains, Action::Strip),
        rule("ab", "ab", Match::Contains, Action::Strip),
    ]);
    every_cut(&b, "1aXb2", "12");
    every_cut(&b, "1aabXbb2", "1b2");
    // 接了两层，外层的 `a` 可能在第二层接上之前就发出去了
    every_cut_safe(&b, "1aaXbb2", "12", "ab");
    // 要看前后一个字的正则：删掉 `X` 之后 `cat` 后面不再是字母
    let c = answer(vec![
        rule("x", "X", Match::Contains, Action::Strip),
        rule("w", r"\bcat\b", Match::Regex, Action::Strip),
    ]);
    every_cut(&c, "catX dog", " dog");
    // 删在这一路末尾的不算接缝：只有要看开头、结尾的规则会因为它变，一路删下去会把整段删光
    every_cut(&c, "a catX", "a cat");
    // 换成的字不再查：换上去的 `[x]` 和两边拼不成新的命中
    let r = answer(vec![RuleInput {
        replacement: "[x]",
        ..rule("s", "secret", Match::Contains, Action::Replace)
    }]);
    every_cut(&r, "sesecretcret", "se[x]cret");
    // 换成空的就是删掉
    let e = answer(vec![RuleInput {
        replacement: "",
        ..rule("s", "secret", Match::Contains, Action::Replace)
    }]);
    every_cut(&e, "sesecretcret", "");
    // 接出来的那一处也算一处命中，位置是原文里从外层的 `se` 到外层的 `cret`
    let mut lane = a.lane(true);
    lane.whole("前sesecretcret后");
    let m = lane.take();
    assert_eq!(m.counts, vec![2]);
    let outer = m.found.iter().find(|f| f.at.start == "前".len()).unwrap();
    assert_eq!(outer.at, "前".len().."前sesecretcret".len());
    let p = outer.place.as_ref().unwrap();
    assert_eq!(
        (p.before.as_str(), p.matched.as_str(), p.after.as_str()),
        ("前", "secret", "后")
    );
}

/// 套得太深（第一遍之后又过了 [`MAX_ROUNDS`] 遍还在改字）：从还在改的那一处起这一路不再
/// 发，前面的照发；发出去的里面一处命中都没有。套得浅的整份删干净，流着的发出去的里面也
/// 找不到
#[test]
fn nesting_beyond_the_bound_silences_the_rest_of_the_lane() {
    let a = answer(vec![rule("ab", "ab", Match::Contains, Action::Strip)]);
    let nested = |depth: usize| format!("x{}{}y", "a".repeat(depth), "b".repeat(depth));
    assert_eq!(whole_text(&a, true, &nested(MAX_ROUNDS)), "xy");
    assert_eq!(whole_text(&a, true, &nested(MAX_ROUNDS + 1)), "");
    let text = format!("{}tail", nested(MAX_ROUNDS + 5));
    // 删了 65 层还剩 4 层：从剩下的那一处往前一个字（`reach`）起不要了
    assert_eq!(whole_text(&a, true, &text), "xaaa");
    let pieces: Vec<String> = text.chars().map(String::from).collect();
    let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
    let out = streamed(&a, true, &refs);
    assert!(!out.contains("ab") && !out.contains("tail"), "{out}");
    every_cut_safe(&a, &nested(6), "xy", "ab");
}

/// 用到开头、结尾的规则只在原文上查一遍：删掉末尾的一个字，前一个字又成了末尾 —— 再查
/// 的话一路删下去整段就没了，而流着的那边早就发出去了。整份的和流式的一样，只删原文里的那
/// 一处；`\b` 这类规则删在这一路两头的也不再查
#[test]
fn rules_anchored_to_the_ends_do_not_erode_the_answer() {
    for (pattern, text, want) in [
        (r"(?s).\z", "hello world", "hello worl"),
        (r"\d$", "call 12345", "call 1234"),
        (r"^\w", "hello", "ello"),
        (r"\d(?m)$", "a1\nb22\n", "a\nb2\n"),
        (r".\b", "ab cd", "a"),
    ] {
        let a = answer(vec![RuleInput {
            case_sensitive: true,
            ..rule("e", pattern, Match::Regex, Action::Strip)
        }]);
        every_cut(&a, text, want);
    }
}

/// 整份回答里一路正文分在几个字段里：一起处理一遍，每个字留在原来的字段里
#[test]
fn a_lane_split_over_fields_is_filtered_once_and_split_back_where_the_text_was() {
    let a = answer(vec![
        rule("s", "secret", Match::Contains, Action::Strip),
        RuleInput {
            replacement: "[x]",
            ..rule("r", "北极星", Match::Contains, Action::Replace)
        },
    ]);
    let mut lane = a.lane(true);
    let parts = lane.whole_split(&["one se", "cret two 北极", "星 three sesecretcret"]);
    assert_eq!(parts, ["one ", " two [x]", " three "]);
    assert_eq!(lane.take().counts, vec![3, 1]);
    // 没改字的原样留在各自的字段里
    let mut lane = a.lane(true);
    assert_eq!(lane.whole_split(&["ab", "", "cd"]), ["ab", "", "cd"]);
}

/// 开头连着删了几处：交出去都在开头，都不算接缝。流着的照样多扣 `reach` 个字，不比整份的
/// 早发一个字
#[test]
fn deletions_at_the_very_start_do_not_send_text_early() {
    let a = answer(vec![rule("p", "(?s).[^ ]", Match::Regex, Action::Strip)]);
    for pieces in [
        &["abcdefg k", "hijklmn"][..],
        &["abcdefg k", "hijklm"],
        &["ab c k", "defg"],
        &["c k", "abc"],
        &["abc k", "abc"],
        &["abc k", "a"],
        &["abc ", "kabc"],
    ] {
        let whole = whole_text(&a, true, &pieces.concat());
        assert_eq!(streamed(&a, true, pieces), whole, "{pieces:?}");
    }
}

/// 换成非空的替换文本不留接缝：要看前后一个字的规则不因为换上去的字再认一遍、一个字一个字
/// 地换下去。整份的和流式的一样
#[test]
fn a_replacement_next_to_a_word_boundary_is_not_eaten_further() {
    for (pattern, with, text, want) in [
        (
            r"\d{4}\b",
            "****",
            "card 4111111111111111 ok".to_string(),
            "card 411111111111**** ok".to_string(),
        ),
        (
            r"\w\b",
            "[x]",
            "hello world".to_string(),
            "hell[x] worl[x]".to_string(),
        ),
        (
            r"\w\b",
            "[x]",
            format!("{} and the rest", "a".repeat(300)),
            format!("{}[x] an[x] th[x] res[x]", "a".repeat(299)),
        ),
    ] {
        let a = answer(vec![RuleInput {
            case_sensitive: true,
            replacement: with,
            ..rule("r", pattern, Match::Regex, Action::Replace)
        }]);
        assert_eq!(whole_text(&a, true, &text), want, "{pattern}");
        for size in [1, 7, 10_000] {
            let chars: Vec<char> = text.chars().collect();
            let pieces: Vec<String> = chars.chunks(size).map(|c| c.iter().collect()).collect();
            let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
            assert_eq!(streamed(&a, true, &refs), want, "{pattern} {size}");
        }
    }
}

#[test]
fn case_variants_beyond_ascii_are_the_same_word() {
    // 开尔文符号 K 和 k、带重音的大小写、希腊字母的 Σ 和 σ
    let kelvin = ch(0x212A);
    let word = format!("{kelvin}ey Éclair ΣΟΦΙΑ");
    let a = answer(vec![
        rule("k", "key", Match::Contains, Action::Strip),
        rule("e", "éclair", Match::Contains, Action::Strip),
        rule("s", "σοφια", Match::Contains, Action::Strip),
    ]);
    assert_eq!(whole_text(&a, true, &word), "  ");
    // 分大小写的只认写的那样
    let exact = answer(vec![RuleInput {
        case_sensitive: true,
        ..rule("k", "Key", Match::Contains, Action::Strip)
    }]);
    assert_eq!(whole_text(&exact, true, "key Key KEY"), "key  KEY");
}

#[test]
fn regex_rules_hold_back_only_what_they_can_match() {
    let a = answer(vec![rule("t", r"TKT-\d{6}", Match::Regex, Action::Strip)]);
    assert_eq!(a.hold(), 9, "最长十个字，扣九个");
    let b = answer(vec![rule("w", r"\bcat\b", Match::Regex, Action::Strip)]);
    assert_eq!(b.hold(), 3, "要看下一个字的多扣一个");
    let c = answer(vec![rule("u", r"x+", Match::Regex, Action::Strip)]);
    assert_eq!(c.hold(), REGEX_HOLD_MAX, "没有上限的扣满");
    // `\b` 要看下一个字：`cat` 后面紧跟着字母就不算，切在哪儿都一样
    let text = "cat catalog cat. concat cat";
    for i in 0..=text.len() {
        assert_eq!(
            streamed(&b, true, &[&text[..i], &text[i..]]),
            " catalog . concat ",
            "切在 {i}"
        );
    }
}

#[test]
fn a_match_longer_than_the_cap_may_have_its_start_sent() {
    // 模块说明里明说的边界：没有上限的正则，命中比扣住的长时开头那一截照常发
    let a = answer(vec![rule("x", r"x+", Match::Regex, Action::Strip)]);
    let long = "x".repeat(REGEX_HOLD_MAX * 3);
    let text = format!("a{long}b");
    let pieces: Vec<String> = text.chars().map(String::from).collect();
    let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
    let out = streamed(&a, true, &refs);
    assert!(out.starts_with('a') && out.ends_with('b'), "{out}");
    assert!(out.len() < text.len(), "后面那一截照样删");
    // 整份的一次删干净
    assert_eq!(whole_text(&a, true, &text), "ab");
    // 有上限的正则流着和整份一样
    let d = answer(vec![rule("d", r"\d{2,4}", Match::Regex, Action::Strip)]);
    let text = "1 22 333 4444 55555 666666";
    let pieces: Vec<String> = text.chars().map(String::from).collect();
    let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
    assert_eq!(streamed(&d, true, &refs), whole_text(&d, true, text));
}

#[test]
fn overlapping_rules_merge_and_the_replacement_wins() {
    let a = answer(vec![
        rule("s", "星计", Match::Contains, Action::Strip),
        RuleInput {
            replacement: "[X]",
            ..rule("r", "北极星", Match::Contains, Action::Replace)
        },
    ]);
    assert_eq!(whole_text(&a, true, "北极星计划"), "[X]划");
    // 只是挨着的各换各的
    let b = answer(vec![RuleInput {
        replacement: "*",
        ..rule("r", "ab", Match::Contains, Action::Replace)
    }]);
    assert_eq!(whole_text(&b, true, "ababab"), "***");
}

#[test]
fn observe_sends_everything_at_once_and_still_finds_matches_across_pieces() {
    let a = answer(vec![rule("k", "北极星", Match::Contains, Action::Strip)]);
    let mut lane = a.lane(false);
    assert_eq!(lane.push("请看北"), "请看北", "观察档不扣");
    assert_eq!(lane.push("极星。"), "极星。");
    assert_eq!(lane.finish(), "");
    let found = lane.take().found;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].at, "请看".len().."请看北极星".len());
    let p = found[0].place.as_ref().unwrap();
    assert_eq!(
        (p.before.as_str(), p.matched.as_str(), p.after.as_str()),
        ("请看", "北极星", "。")
    );
}

#[test]
fn the_held_tail_comes_out_when_the_lane_ends() {
    let a = answer(vec![rule("k", "secret", Match::Contains, Action::Strip)]);
    let mut lane = a.lane(true);
    assert_eq!(
        lane.push("this is sec"),
        "t",
        "最后五个字可能是命中的开头，再往前五个字可能和删掉之后的字接成一处：扣着"
    );
    assert_eq!(lane.finish(), "his is sec", "到头了也没成命中：照发");
    assert_eq!(lane.push("more"), "more", "结束之后的原样");
}

#[test]
fn context_around_a_match_is_kept_for_the_log() {
    let a = answer(vec![rule("k", "secret", Match::Contains, Action::Strip)]);
    let before = "前".repeat(100);
    let after = "后".repeat(100);
    let text = format!("{before}secret{after}");
    let mut lane = a.lane(true);
    for c in text.chars() {
        lane.push(&c.to_string());
    }
    lane.finish();
    let found = lane.take().found;
    let p = found[0].place.as_ref().unwrap();
    assert_eq!(p.before, "前".repeat(80));
    assert_eq!(p.matched, "secret");
    assert_eq!(p.after, "后".repeat(80));
}

/// 照定义直接算：第一遍每条删除、替换规则在原文上从头找（`find_iter`），之后每一遍在改过
/// 的整段上每个位置都试一次（和换上去的替换文本重叠的不算），合并、改，直到一处都没有。
/// 交回改出来的，和替换文本在哪儿。第一遍之后又过了 [`MAX_ROUNDS`] 遍还在改的是空的
fn reference(a: &Answer, text: &str) -> (String, Vec<Range<usize>>) {
    let mut t = text.to_string();
    let mut kept: Vec<Range<usize>> = Vec::new();
    // 上一遍改过的地方（替换文本两头都算），这一路的开头、结尾不算
    let mut seams: Vec<usize> = Vec::new();
    for round in 0..=MAX_ROUNDS {
        let mut edits = Vec::new();
        for (ri, r) in a.rules.iter().enumerate() {
            if !r.action.edits() || (round > 0 && r.anchored) {
                continue;
            }
            if round == 0 {
                let found = r.re.find_iter(&t).filter(|m| !m.is_empty());
                edits.extend(found.map(|m| (m.range(), ri)));
            } else {
                // 起点在某个接缝前 `hold` 个字以内的每一处
                let near = |m: &Range<usize>| {
                    seams.iter().any(|&j| {
                        let from = t[..j].char_indices().rev().nth(r.hold.max(1) - 1);
                        let from = if r.hold == 0 {
                            j
                        } else {
                            from.map_or(0, |(i, _)| i)
                        };
                        from <= m.start && m.start <= j
                    })
                };
                let found = every_match(&r.re, &t).into_iter();
                edits.extend(
                    found
                        .filter(|m| near(m) && !overlapping(&kept, m))
                        .map(|m| (m, ri)),
                );
            }
        }
        if edits.is_empty() {
            return (t, kept);
        }
        let merged = merge_edits(edits, |ri| a.rules[ri].edit_with());
        // 替换文本跟着改动挪，这一遍换上去的接着记；接缝是这一遍改过的地方
        let mut next = Vec::new();
        seams.clear();
        let mut delta: isize = 0;
        let mut old = kept.into_iter().peekable();
        for (r, w) in &merged {
            while let Some(k) = old.next_if(|k| k.end <= r.start) {
                next.push(k.start.saturating_add_signed(delta)..k.end.saturating_add_signed(delta));
            }
            let at = r.start.saturating_add_signed(delta);
            if !w.is_empty() {
                next.push(at..at + w.len());
            }
            // 只有删掉字的地方是接缝：换上去的替换文本两头不算
            if w.is_empty() {
                seams.push(at);
            }
            delta += w.len() as isize - r.len() as isize;
        }
        next.extend(
            old.map(|k| k.start.saturating_add_signed(delta)..k.end.saturating_add_signed(delta)),
        );
        kept = next;
        t = apply_edits(&t, &merged);
        seams.retain(|&j| j != 0 && j != t.len());
        seams.dedup();
        if round == MAX_ROUNDS && !seams.is_empty() {
            // 套得太深：从还在改的那一处（头一个接缝往前 `reach` 个字）起不要了
            let first = seams[0];
            let from = t[..first].char_indices().rev().nth(a.reach.max(1) - 1);
            let keep = if a.reach == 0 {
                first
            } else {
                from.map_or(0, |(i, _)| i)
            };
            t.truncate(keep);
            kept.retain(|k| k.end <= keep);
            return (t, kept);
        }
    }
    (t, kept)
}

/// 这条规则要不要看命中前后的一个字（`\b`、`$` 这类断言）
fn looks(r: &AnswerRule) -> bool {
    r.matching == Match::Regex && regex_hold(r.pattern.trim(), !r.case_sensitive).2
}

/// 每个位置上（从那儿起）的那一处命中，空的不算。互相重叠的都在
fn every_match(re: &regex::Regex, t: &str) -> Vec<Range<usize>> {
    t.char_indices()
        .filter_map(|(i, _)| re.find_at(t, i).filter(|m| m.start() == i && !m.is_empty()))
        .map(|m| m.range())
        .collect()
}

fn overlapping(spans: &[Range<usize>], m: &Range<usize>) -> bool {
    spans.iter().any(|s| s.start < m.end && m.start < s.end)
}

/// 一段正文按定义算出来的、整份一次处理的、随机切开流着处理的，三个一样；认出来的命中一样
/// 多；交出去的正文里，删除、替换规则在哪个位置上都找不到一处不和替换文本重叠的命中。
///
/// 流着处理的套得比扣住的字还深（开头已经发了）的时候只查发出去的里面没有命中，交回 `true`
fn agree(
    a: &Arc<Answer>,
    text: &str,
    acts: bool,
    next: &mut dyn FnMut(usize) -> usize,
    why: &str,
) -> bool {
    let whole = whole_text(a, acts, text);
    if acts {
        let (want, kept) = reference(a, text);
        assert_eq!(whole, want, "{text:?} {why}");
        // 要看前后一个字的不查：这一路的开头、结尾不算接缝（见 `Pass::piece`）
        for r in a.rules.iter().filter(|r| r.action.edits() && !looks(r)) {
            let left: Vec<_> = every_match(&r.re, &want)
                .into_iter()
                .filter(|m| !overlapping(&kept, m))
                .collect();
            assert!(left.is_empty(), "{text:?} → {want:?} 还有 {left:?} {why}");
        }
    } else {
        assert_eq!(whole, text);
    }
    // 随机切成几块（切在字符边界上：一路正文是解析好的字符串）
    let bounds: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain([text.len()])
        .collect();
    let mut cuts: Vec<usize> = (0..next(8)).map(|_| bounds[next(bounds.len())]).collect();
    cuts.sort_unstable();
    let mut pieces = Vec::new();
    let mut at = 0;
    for c in cuts {
        pieces.push(&text[at..c]);
        at = c;
    }
    pieces.push(&text[at..]);
    let mut lane = a.lane(acts);
    let mut out = String::new();
    for p in &pieces {
        out.push_str(&lane.push(p));
    }
    out.push_str(&lane.finish());
    let short = lane.tally.short;
    // 只有删除规则的：发出去的正文里一处都找不到。套得比扣住的字还深的时候，要看前后一个字
    // 的正则整个落在已经发出去的字里的那种收不回来（模块说明里那条明说的边界）
    if acts && a.rules.iter().all(|r| r.action != Action::Replace) {
        for r in a
            .rules
            .iter()
            .filter(|r| r.action == Action::Strip && !looks(r))
        {
            assert!(
                r.re.find(&out).is_none(),
                "{text:?} {pieces:?} → {out:?} {why}"
            );
        }
    }
    if short {
        return true;
    }
    assert_eq!(out, whole, "{text:?} {pieces:?} {why}");
    // 认出来的命中也一样多
    let streamed_found = lane.take().counts;
    let mut once = a.lane(acts);
    once.whole(text);
    assert_eq!(streamed_found, once.take().counts, "{text:?} {why}");
    false
}

fn xorshift(mut seed: u64) -> impl FnMut(usize) -> usize {
    move |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n.max(1) as u64) as usize
    }
}

/// 随机的正文、随机的规则、随机地切：流着处理出来的和整份一次处理的一模一样，和照定义算的
/// 也一样（[`agree`]）；不 panic
#[test]
fn streaming_agrees_with_the_whole_text_for_random_text_rules_and_cuts() {
    let mut next = xorshift(0x51f1_5eed_c0de_2026);
    let alphabet: Vec<String> = [
        "a", "b", "A", "B", "é", "É", "中", "文", " ", "1", "2", "😀", "ß",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([
        ch(0x212A).to_string(),
        ch(0x3A3).to_string(),
        ch(0x3C3).to_string(),
    ])
    .collect();
    let regexes = [
        r"a[b1]{1,3}",
        r"\d{2}",
        r"中文?",
        r"(?:ab|ba)é",
        r"\bab\b",
        r"[aA]{2}",
    ];
    let mut shorts = 0;
    for round in 0..400 {
        let word = |next: &mut dyn FnMut(usize) -> usize| -> String {
            (0..1 + next(3))
                .map(|_| alphabet[next(alphabet.len())].as_str())
                .collect()
        };
        let mut patterns: Vec<(String, Match, Action, bool, String)> = Vec::new();
        for _ in 0..1 + next(3) {
            let (p, m) = if next(2) == 0 {
                // 只有空白的关键词编不起来
                let w = word(&mut next);
                (
                    if w.trim().is_empty() {
                        "a".to_string()
                    } else {
                        w
                    },
                    Match::Contains,
                )
            } else {
                (regexes[next(regexes.len())].to_string(), Match::Regex)
            };
            let action = [Action::Strip, Action::Replace, Action::Record][next(3)];
            let with = ["***", "", "x", "中"][next(4)].to_string();
            patterns.push((p, m, action, next(2) == 0, with));
        }
        let a = build(&patterns);
        let text: String = (0..next(60))
            .map(|_| alphabet[next(alphabet.len())].as_str())
            .collect();
        shorts += usize::from(agree(
            &a,
            &text,
            round % 5 != 0,
            &mut next,
            &format!("{patterns:?}"),
        ));
    }
    // 套得比扣住的字还深的要是常见，和整份的比对就没测到什么
    assert!(shorts * 20 < 400, "{shorts}");
}

fn build(patterns: &[(String, Match, Action, bool, String)]) -> Arc<Answer> {
    let names: Vec<String> = (0..patterns.len()).map(|i| format!("r{i}")).collect();
    let inputs: Vec<RuleInput<'_>> = patterns
        .iter()
        .zip(&names)
        .map(|((p, m, action, cs, with), name)| RuleInput {
            case_sensitive: *cs,
            replacement: with,
            ..rule(name, p, *m, *action)
        })
        .collect();
    Answer::build(&Rules::build(inputs).unwrap()).unwrap()
}

/// 专找一层套一层的：字母少、关键词短，正文是把命中塞进命中中间拼出来的。删掉一处之后前后
/// 接成的新命中都删干净，流着的和整份的一样
#[test]
fn nested_matches_agree_for_random_rules_texts_and_cuts() {
    let mut next = xorshift(0x0e57_ed5e_eded_2026);
    let letters = ["a", "b", "c", "X", " "];
    let regexes = [r"a[bc]", r"\bab\b", r"a.?c", r"(?:ab|b)c", r"\bX", r"c$"];
    let mut shorts = 0;
    for round in 0..3000 {
        let mut patterns: Vec<(String, Match, Action, bool, String)> = Vec::new();
        for _ in 0..1 + next(3) {
            let (p, m) = if next(3) != 0 {
                let w: String = (0..1 + next(3))
                    .map(|_| letters[next(letters.len() - 1)])
                    .collect();
                (w, Match::Contains)
            } else {
                (regexes[next(regexes.len())].to_string(), Match::Regex)
            };
            let action = [
                Action::Strip,
                Action::Strip,
                Action::Replace,
                Action::Record,
            ][next(4)];
            let with = ["#", "", "a", "c"][next(4)].to_string();
            patterns.push((p, m, action, next(2) == 0, with));
        }
        let a = build(&patterns);
        // 先随便写一段，再挑一条规则的样子往随机的位置里一层一层地塞
        let mut text: String = (0..next(12))
            .map(|_| letters[next(letters.len())])
            .collect();
        for _ in 0..next(6) {
            let (p, m, ..) = &patterns[next(patterns.len())];
            let w = if *m == Match::Contains {
                p.clone()
            } else {
                ["ab", "ac", "abc", "bc", "X", "c"][next(6)].to_string()
            };
            let at = next(text.len() + 1);
            text.insert_str(at, &w);
        }
        shorts += usize::from(agree(
            &a,
            &text,
            round % 7 != 0,
            &mut next,
            &format!("{patterns:?}"),
        ));
    }
    eprintln!("流着的开头已经发了的：{shorts} / 3000");
    assert!(shorts * 10 < 3000, "{shorts}");
}

// ───────────────────────────────────────────────────────── 按格式

fn named(event: &str, v: Value) -> String {
    format!("event: {event}\ndata: {v}\n\n")
}

fn data(v: Value) -> String {
    format!("data: {v}\n\n")
}

/// 一条 SSE 流随机切成小块（切在任意字节上，包括一个字的中间）喂进去，交回整条输出
fn through(body: &mut Body, stream: &str, seed: u64) -> String {
    let bytes = stream.as_bytes();
    let mut seed = seed | 1;
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let n = 1 + (seed % 7) as usize;
        let end = (at + n).min(bytes.len());
        out.extend(body.process(&bytes[at..end]));
        at = end;
    }
    out.extend(body.flush());
    String::from_utf8(out).expect("output stays UTF-8")
}

/// SSE 输出里每一帧的 JSON
fn frames_of(out: &str) -> Vec<Value> {
    out.split("\n\n")
        .filter_map(|f| {
            f.lines()
                .find_map(|l| l.strip_prefix("data: "))
                .and_then(|d| serde_json::from_str(d).ok())
        })
        .collect()
}

fn star(id: &'static str, pattern: &'static str) -> Arc<Answer> {
    answer(vec![RuleInput {
        replacement: "***",
        ..rule(id, pattern, Match::Contains, Action::Replace)
    }])
}

#[test]
fn anthropic_text_deltas_are_filtered_and_the_tail_comes_before_the_block_stops() {
    let a = star("k", "北极星");
    let delta = |i: u64, t: &str| {
        named(
            "content_block_delta",
            json!({"type":"content_block_delta","index":i,"delta":{"type":"text_delta","text":t}}),
        )
    };
    let stream = [
        named("message_start", json!({"type":"message_start","message":{"content":[]}})),
        delta(0, "关于北"),
        delta(0, "极星的说明：北"),
        named("content_block_stop", json!({"type":"content_block_stop","index":0})),
        named(
            "content_block_delta",
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"q\":\"北极星\"}"}}),
        ),
        named("content_block_stop", json!({"type":"content_block_stop","index":1})),
        named("message_stop", json!({"type":"message_stop"})),
    ]
    .concat();
    for seed in [1, 7, 42, 99] {
        let mut body = Body::new(a.clone(), true, Dialect::Anthropic, true);
        let out = through(&mut body, &stream, seed);
        let frames = frames_of(&out);
        let text: String = frames
            .iter()
            .filter(|f| f["delta"]["type"] == "text_delta")
            .map(|f| f["delta"]["text"].as_str().unwrap())
            .collect();
        assert_eq!(text, "关于***的说明：北");
        // 扣着的「北」在块结束之前补出来
        let stop = frames
            .iter()
            .position(|f| f["type"] == "content_block_stop")
            .unwrap();
        let last_text = frames
            .iter()
            .rposition(|f| f["delta"]["type"] == "text_delta")
            .unwrap();
        assert!(last_text < stop);
        // 工具参数不碰
        assert!(out.contains(r#"{\"q\":\"北极星\"}"#), "{out}");
        let hits = body.take_hits();
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].count, hits[0].outcome), (1, Outcome::Replaced));
        assert_eq!(hits[0].places[0].0, "content[0].text");
    }
}

#[test]
fn chat_deltas_are_filtered_per_choice_and_flushed_before_done() {
    let a = star("k", "secret");
    let chunk = |content: Value, finish: Value| {
        data(
            json!({"id":"c1","object":"chat.completion.chunk","model":"m","choices":[{"index":0,"delta":content,"finish_reason":finish}]}),
        )
    };
    let stream = [
        chunk(json!({"role":"assistant","content":""}), Value::Null),
        chunk(json!({"content":"the sec"}), Value::Null),
        chunk(json!({"content":"ret is sec"}), Value::Null),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let mut body = Body::new(a, true, Dialect::Chat, true);
    let out = through(&mut body, &stream, 3);
    let frames = frames_of(&out);
    let text: String = frames
        .iter()
        .filter_map(|f| f["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "the *** is sec");
    // 扣着的尾巴补成一帧，抄着 id 和模型名，排在 `[DONE]` 前面
    let synth = frames.last().unwrap();
    assert_eq!(synth["id"], "c1");
    assert_eq!(synth["choices"][0]["delta"]["content"], "s sec");
    assert!(out.trim_end().ends_with("data: [DONE]"));
}

fn responses_event(kind: &str, mut v: Value) -> String {
    v["type"] = json!(kind);
    named(kind, v)
}

#[test]
fn responses_done_events_and_the_completed_output_say_what_was_sent() {
    let a = answer(vec![RuleInput {
        replacement: "[X]",
        ..rule("k", "北极星", Match::Contains, Action::Replace)
    }]);
    let full = "北极星项目，北极星。";
    let sent = "[X]项目，[X]。";
    let stream = [
        responses_event("response.created", json!({"response":{"id":"r","output":[]}})),
        responses_event(
            "response.output_text.delta",
            json!({"item_id":"m1","output_index":0,"content_index":0,"delta":"北极"}),
        ),
        responses_event(
            "response.output_text.delta",
            json!({"item_id":"m1","output_index":0,"content_index":0,"delta":"星项目，北"}),
        ),
        responses_event(
            "response.output_text.delta",
            json!({"item_id":"m1","output_index":0,"content_index":0,"delta":"极星。"}),
        ),
        responses_event(
            "response.output_text.done",
            json!({"item_id":"m1","output_index":0,"content_index":0,"text":full}),
        ),
        responses_event(
            "response.content_part.done",
            json!({"item_id":"m1","output_index":0,"content_index":0,"part":{"type":"output_text","text":full}}),
        ),
        responses_event(
            "response.output_item.done",
            json!({"output_index":0,"item":{"type":"message","id":"m1","content":[{"type":"output_text","text":full}]}}),
        ),
        // 自由格式工具的输入也是一路
        responses_event(
            "response.custom_tool_call_input.delta",
            json!({"item_id":"t1","output_index":1,"delta":"*** Add File: 北极"}),
        ),
        responses_event(
            "response.custom_tool_call_input.delta",
            json!({"item_id":"t1","output_index":1,"delta":"星.md"}),
        ),
        responses_event(
            "response.custom_tool_call_input.done",
            json!({"item_id":"t1","output_index":1,"input":"*** Add File: 北极星.md"}),
        ),
        responses_event(
            "response.output_item.done",
            json!({"output_index":1,"item":{"type":"custom_tool_call","id":"t1","name":"apply_patch","input":"*** Add File: 北极星.md"}}),
        ),
        // 一个增量都没有、只在收尾时出现的一段：在这里查一遍，只算一次
        responses_event(
            "response.completed",
            json!({"response":{"id":"r","output":[
                {"type":"message","id":"m1","content":[{"type":"output_text","text":full}]},
                {"type":"custom_tool_call","id":"t1","input":"*** Add File: 北极星.md"},
                {"type":"message","id":"m2","content":[{"type":"output_text","text":"又是北极星"}]},
            ]}}),
        ),
    ]
    .concat();
    for seed in [5, 11, 23] {
        let mut body = Body::new(a.clone(), true, Dialect::Responses, true);
        let out = through(&mut body, &stream, seed);
        let frames = frames_of(&out);
        let streamed: String = frames
            .iter()
            .filter(|f| f["type"] == "response.output_text.delta")
            .map(|f| f["delta"].as_str().unwrap())
            .collect();
        assert_eq!(streamed, sent);
        let by = |kind: &str| frames.iter().find(|f| f["type"] == kind).unwrap().clone();
        assert_eq!(by("response.output_text.done")["text"], sent);
        assert_eq!(by("response.content_part.done")["part"]["text"], sent);
        assert_eq!(
            by("response.output_item.done")["item"]["content"][0]["text"],
            sent
        );
        let input: String = frames
            .iter()
            .filter(|f| f["type"] == "response.custom_tool_call_input.delta")
            .map(|f| f["delta"].as_str().unwrap())
            .collect();
        assert_eq!(input, "*** Add File: [X].md");
        assert_eq!(
            by("response.custom_tool_call_input.done")["input"],
            "*** Add File: [X].md"
        );
        let done = by("response.completed");
        assert_eq!(done["response"]["output"][0]["content"][0]["text"], sent);
        assert_eq!(
            done["response"]["output"][1]["input"],
            "*** Add File: [X].md"
        );
        assert_eq!(
            done["response"]["output"][2]["content"][0]["text"],
            "又是[X]"
        );
        let hits = body.take_hits();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].count, 4,
            "两处正文、一处工具输入、一处只在收尾里出现的"
        );
        let paths: Vec<&str> = hits[0].places.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            [
                "output[0].content[0].text",
                "output[0].content[0].text",
                "output[1].input",
                "output[2].content[0].text"
            ]
        );
    }
}

#[test]
fn gemini_streams_as_sse_and_as_a_json_array() {
    let a = star("k", "secret");
    let chunk = |text: &str, finish: Option<&str>| {
        let mut c = json!({"content":{"role":"model","parts":[{"text":text}]},"index":0});
        if let Some(f) = finish {
            c["finishReason"] = json!(f);
        }
        json!({"candidates":[c],"modelVersion":"g"})
    };
    let thought = json!({"candidates":[{"content":{"parts":[{"text":"secret thoughts","thought":true}]},"index":0}]});
    let parts = [
        thought,
        chunk("my sec", None),
        chunk("ret and sec", None),
        chunk("", Some("STOP")),
    ];
    // SSE
    let sse: String = parts.iter().map(|v| data(v.clone())).collect();
    let mut body = Body::new(a.clone(), true, Dialect::Gemini, true);
    let out = through(&mut body, &sse, 9);
    let frames = frames_of(&out);
    let text: String = frames
        .iter()
        .flat_map(|f| {
            f["candidates"][0]["content"]["parts"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter(|p| p["thought"] != true)
        .filter_map(|p| p["text"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        text, "my *** and sec",
        "最后一帧带着 finishReason：扣着的接在它的正文后面"
    );
    assert!(out.contains("secret thoughts"), "思考过程不碰");
    // JSON 数组
    let array = format!(
        "[{}]",
        parts
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(",\r\n")
    );
    let mut body = Body::new(a, true, Dialect::Gemini, false);
    let out = through(&mut body, &array, 4);
    let v: Value = serde_json::from_str(&out).expect("still one JSON array");
    let text: String = v
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|f| {
            f["candidates"][0]["content"]["parts"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter(|p| p["thought"] != true)
        .filter_map(|p| p["text"].as_str().map(str::to_string))
        .collect();
    assert_eq!(text, "my *** and sec");
}

#[test]
fn a_json_array_that_ends_without_a_finish_reason_gets_the_tail_as_an_element() {
    let a = star("k", "secret");
    let chunk =
        |text: &str| json!({"candidates":[{"content":{"parts":[{"text":text}]},"index":0}]});
    let array = format!("[{},\r\n{}]", chunk("a sec"), chunk("ret b se"));
    let mut body = Body::new(a, true, Dialect::Gemini, false);
    let out = through(&mut body, &array, 2);
    let v: Value = serde_json::from_str(&out).expect("one JSON array");
    let text: String = v
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| {
            f["candidates"][0]["content"]["parts"][0]["text"]
                .as_str()
                .map(str::to_string)
        })
        .collect();
    assert_eq!(text, "a *** b se");
}

#[test]
fn bedrock_blocks_are_filtered_and_flushed_at_their_stop() {
    let a = star("k", "secret");
    let stream = [
        named("messageStart", json!({"role":"assistant"})),
        named(
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"text":"sec"}}),
        ),
        named(
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"text":"ret! se"}}),
        ),
        named("contentBlockStop", json!({"contentBlockIndex":0})),
        named("messageStop", json!({"stopReason":"end_turn"})),
    ]
    .concat();
    let mut body = Body::new(a, true, Dialect::Bedrock, true);
    let out = through(&mut body, &stream, 13);
    let frames = frames_of(&out);
    let text: String = frames
        .iter()
        .filter_map(|f| f["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, "***! se");
    // 扣着的尾巴在块结束之前补出来
    let stop = frames
        .iter()
        .position(|f| f.get("contentBlockIndex").is_some() && f.get("delta").is_none())
        .unwrap();
    let before: String = frames[..stop]
        .iter()
        .filter_map(|f| f["delta"]["text"].as_str())
        .collect();
    assert_eq!(before, "***! se");
}

#[test]
fn an_error_mid_stream_flushes_what_was_held() {
    let a = star("k", "secret");
    let stream = [
        named(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"so se"}}),
        ),
        named("error", json!({"type":"error","error":{"type":"overloaded_error"}})),
    ]
    .concat();
    let mut body = Body::new(a.clone(), true, Dialect::Anthropic, true);
    let out = through(&mut body, &stream, 1);
    let frames = frames_of(&out);
    let text: String = frames
        .iter()
        .filter_map(|f| f["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, "so se");
    assert_eq!(frames.last().unwrap()["type"], "error");
    // 流断了、什么收尾都没有：flush 时补出来
    let cut = named(
        "content_block_delta",
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"so se"}}),
    );
    let mut body = Body::new(a, true, Dialect::Anthropic, true);
    let out = through(&mut body, &cut, 1);
    let text: String = frames_of(&out)
        .iter()
        .filter_map(|f| f["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, "so se");
}

#[test]
fn observe_mode_changes_no_byte_and_still_reports() {
    let a = star("k", "secret");
    let stream = [
        named(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"a sec"}}),
        ),
        named(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ret"}}),
        ),
        named("content_block_stop", json!({"type":"content_block_stop","index":0})),
    ]
    .concat();
    let mut body = Body::new(a, false, Dialect::Anthropic, true);
    let out = through(&mut body, &stream, 6);
    assert_eq!(out, stream, "观察档一个字节都不改");
    let hits = body.take_hits();
    assert_eq!(hits[0].outcome, Outcome::Recorded);
    assert_eq!(hits[0].count, 1);
}

#[test]
fn a_frame_split_inside_a_character_is_put_back_together() {
    // 「星」的三个字节切在两块里
    let a = star("k", "北极星");
    let stream = named(
        "content_block_delta",
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"北极星"}}),
    );
    let at = stream.find('星').unwrap() + 1;
    let mut body = Body::new(a, true, Dialect::Anthropic, true);
    let mut out = body.process(&stream.as_bytes()[..at]);
    out.extend(body.process(&stream.as_bytes()[at..]));
    out.extend(body.flush());
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains(r#""text":"***""#), "{out}");
}

#[test]
fn whole_answers_are_filtered_in_every_format() {
    let a = star("k", "secret");
    let cases = [
        (
            Dialect::Anthropic,
            json!({"content":[{"type":"text","text":"a secret"},{"type":"tool_use","input":{"q":"secret"}}]}),
            "/content/0/text",
        ),
        (
            Dialect::Chat,
            json!({"choices":[{"index":0,"message":{"role":"assistant","content":"a secret"}}]}),
            "/choices/0/message/content",
        ),
        (
            Dialect::Responses,
            json!({"output":[{"type":"message","content":[{"type":"output_text","text":"a secret"}]}]}),
            "/output/0/content/0/text",
        ),
        (
            Dialect::Gemini,
            json!({"candidates":[{"content":{"parts":[{"text":"x","thought":true},{"text":"a secret"}]},"index":0}]}),
            "/candidates/0/content/parts/1/text",
        ),
        (
            Dialect::Bedrock,
            json!({"output":{"message":{"content":[{"text":"a secret"}]}}}),
            "/output/message/content/0/text",
        ),
    ];
    for (d, body, path) in cases {
        let (out, hits) = whole(&a, true, d, body.to_string().as_bytes());
        let v: Value = serde_json::from_slice(&out.expect("changed")).unwrap();
        assert_eq!(v.pointer(path).unwrap(), "a ***", "{d:?}");
        assert_eq!(hits.len(), 1, "{d:?}");
        // 观察档不改，照样报
        let (out, hits) = whole(&a, false, d, body.to_string().as_bytes());
        assert!(out.is_none() && hits.len() == 1, "{d:?}");
    }
    // 工具调用不碰
    let (out, _) = whole(
        &a,
        true,
        Dialect::Anthropic,
        json!({"content":[{"type":"tool_use","input":{"q":"secret"}}]})
            .to_string()
            .as_bytes(),
    );
    assert!(out.is_none());
    assert_eq!(
        whole(&a, true, Dialect::Chat, b"not json"),
        (None, Vec::new())
    );
}

// ───────────────────────────────────────────────────────── 复核时查出来的

/// Gemini 的文字被函数调用隔开就是两路：扣着的字不挪到调用后面，没有命中的整份回答一个
/// 字节都不动
#[test]
fn gemini_text_on_either_side_of_a_function_call_stays_where_it_is() {
    let a = star("k", "zzzzzzzzzz");
    let body = json!({"candidates":[{"index":0,"content":{"role":"model","parts":[
        {"text":"Let me check the weather."},
        {"functionCall":{"name":"weather","args":{"city":"Paris"}}},
        {"text":"Done."}
    ]}}]});
    let (out, hits) = whole(&a, true, Dialect::Gemini, body.to_string().as_bytes());
    assert!(
        out.is_none() && hits.is_empty(),
        "没命中的回答改了：{out:?}"
    );
    // 流着的：调用之前的字在调用那一帧之前发完
    let frames = [
        json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"Let me check the weather."}]}}]}),
        json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"functionCall":{"name":"weather","args":{}}}]}}]}),
        json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"Done."}]},"finishReason":"STOP"}]}),
    ];
    let stream: String = frames.iter().map(|f| data(f.clone())).collect();
    let mut b = Body::new(a.clone(), true, Dialect::Gemini, true);
    let out = frames_of(&through(&mut b, &stream, 5));
    let call = out
        .iter()
        .position(|f| {
            f["candidates"][0]["content"]["parts"][0]
                .get("functionCall")
                .is_some()
        })
        .unwrap();
    let text = |fs: &[Value]| -> String {
        fs.iter()
            .filter_map(|f| f["candidates"][0]["content"]["parts"][0]["text"].as_str())
            .collect()
    };
    assert_eq!(text(&out[..call]), "Let me check the weather.");
    assert_eq!(text(&out[call..]), "Done.");
    // 跨过调用的两段字不是一处命中
    let a = star("k", "weather.Done");
    let (out, hits) = whole(&a, true, Dialect::Gemini, body.to_string().as_bytes());
    assert!(out.is_none() && hits.is_empty());
    // 一个调用两边各有命中：各换各的
    let a = star("k", "e");
    let (out, _) = whole(&a, true, Dialect::Gemini, body.to_string().as_bytes());
    let v: Value = serde_json::from_slice(&out.unwrap()).unwrap();
    let parts = &v["candidates"][0]["content"]["parts"];
    assert_eq!(parts[0]["text"], "L***t m*** ch***ck th*** w***ath***r.");
    assert_eq!(parts[1]["functionCall"]["name"], "weather", "调用不碰");
    assert_eq!(parts[2]["text"], "Don***.");
}

/// 随机的 Gemini 回答：文字、函数调用、思考交错，切成随机的帧。被函数调用隔开的每一段文字
/// （思考不算隔开）流出来的和整段一次处理的一样，别的部分原样、按原来的先后；整份的回答没
/// 命中就一个字节都不动
#[test]
fn gemini_parts_mixed_with_calls_stream_like_their_runs() {
    let mut seed = 0x6e6d_1a5c_0ffe_e123_u64;
    let mut next = |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n.max(1) as u64) as usize
    };
    let words = ["ab", "ba", "a", "b", " ", "中文", "x"];
    for round in 0..300 {
        let a = star("k", ["ab", "ba", "aa", "中文x"][next(4)]);
        // 一串部分：文字或者函数调用 / 思考
        let mut parts: Vec<Value> = Vec::new();
        for _ in 0..1 + next(6) {
            match next(4) {
                0 => parts.push(json!({"functionCall":{"name":"f","args":{}}})),
                1 => parts.push(json!({"text":"ab","thought":true})),
                _ => {
                    let t: String = (0..next(8)).map(|_| words[next(words.len())]).collect();
                    parts.push(json!({"text": t}));
                }
            }
        }
        // 每一段连着的文字整段处理的样子
        let mut want: Vec<String> = Vec::new();
        let mut run = String::new();
        for p in &parts {
            if p.get("functionCall").is_some() {
                want.push(whole_text(&a, true, &std::mem::take(&mut run)));
            } else if p["thought"] != true {
                run.push_str(p["text"].as_str().unwrap());
            }
        }
        want.push(whole_text(&a, true, &run));
        // 流着的：一帧一个部分，文字再随机切开
        let mut frames: Vec<Value> = Vec::new();
        for p in &parts {
            match p["text"].as_str().filter(|_| p["thought"] != true) {
                Some(t) => {
                    let cs: Vec<char> = t.chars().collect();
                    let mut i = 0;
                    loop {
                        let e = (i + 1 + next(4)).min(cs.len());
                        let piece: String = cs[i..e].iter().collect();
                        frames.push(json!({"candidates":[{"index":0,"content":{"parts":[{"text":piece}]}}]}));
                        i = e;
                        if i >= cs.len() {
                            break;
                        }
                    }
                }
                None => frames.push(json!({"candidates":[{"index":0,"content":{"parts":[p]}}]})),
            }
        }
        frames.push(json!({"candidates":[{"index":0,"content":{"parts":[{"text":""}]},"finishReason":"STOP"}]}));
        let stream: String = frames.iter().map(|f| data(f.clone())).collect();
        let mut b = Body::new(a.clone(), true, Dialect::Gemini, true);
        let out = frames_of(&through(&mut b, &stream, round as u64 + 1));
        let mut got: Vec<String> = vec![String::new()];
        let mut others = Vec::new();
        for f in &out {
            for p in f["candidates"][0]["content"]["parts"].as_array().unwrap() {
                if p.get("functionCall").is_some() {
                    got.push(String::new());
                    others.push(p.clone());
                } else if p["thought"] == true {
                    others.push(p.clone());
                } else {
                    got.last_mut()
                        .unwrap()
                        .push_str(p["text"].as_str().unwrap());
                }
            }
        }
        assert_eq!(got, want, "{parts:?}");
        let want_others: Vec<Value> = parts
            .iter()
            .filter(|p| p.get("functionCall").is_some() || p["thought"] == true)
            .cloned()
            .collect();
        assert_eq!(others, want_others);
        // 整份
        let body = json!({"candidates":[{"index":0,"content":{"role":"model","parts":parts}}]});
        let (changed, _) = whole(&a, true, Dialect::Gemini, body.to_string().as_bytes());
        let joined: Vec<String> = parts
            .iter()
            .filter_map(|p| p["text"].as_str().filter(|_| p["thought"] != true))
            .map(str::to_string)
            .collect();
        match changed {
            None => {
                assert_eq!(want.concat(), joined.concat(), "没改却该改：{parts:?}")
            }
            Some(b) => {
                let v: Value = serde_json::from_slice(&b).unwrap();
                let mut got = vec![String::new()];
                for p in v["candidates"][0]["content"]["parts"].as_array().unwrap() {
                    if p.get("functionCall").is_some() {
                        got.push(String::new());
                    } else if p["thought"] != true {
                        got.last_mut()
                            .unwrap()
                            .push_str(p["text"].as_str().unwrap());
                    }
                }
                assert_eq!(got, want, "{parts:?}");
            }
        }
    }
}

/// 改了字的回答里，logprobs 不能把删掉的字带出去：流着的一律去掉，整份的改了字才去掉
#[test]
fn logprobs_do_not_carry_what_was_removed() {
    let a = answer(vec![rule("s", "secret", Match::Contains, Action::Strip)]);
    let body = json!({"choices":[{"index":0,
        "message":{"role":"assistant","content":"the secret is out"},
        "logprobs":{"content":[{"token":" secret","logprob":0.0,"bytes":[32],"top_logprobs":[]}]},
        "finish_reason":"stop"}]});
    let (out, _) = whole(&a, true, Dialect::Chat, body.to_string().as_bytes());
    let out = String::from_utf8(out.unwrap()).unwrap();
    assert!(!out.contains("secret"), "{out}");
    // 没改字：原样
    let clean = json!({"choices":[{"index":0,"message":{"content":"hello"},
        "logprobs":{"content":[{"token":"hello","logprob":0.0}]}}]});
    assert!(
        whole(&a, true, Dialect::Chat, clean.to_string().as_bytes())
            .0
            .is_none()
    );
    // 流着的 Chat
    let stream = data(
        json!({"id":"c","choices":[{"index":0,"delta":{"content":"a secret"},
        "logprobs":{"content":[{"token":" secret","logprob":0.0}]},"finish_reason":"stop"}]}),
    );
    let mut b = Body::new(a.clone(), true, Dialect::Chat, true);
    let out = through(&mut b, &stream, 3);
    assert!(!out.contains("secret"), "{out}");
    // Responses 的增量和整段
    let stream = [
        responses_event(
            "response.output_text.delta",
            json!({"item_id":"m","output_index":0,"content_index":0,"delta":"a secret",
                "logprobs":[{"token":" secret","logprob":0.0}]}),
        ),
        responses_event(
            "response.output_text.done",
            json!({"item_id":"m","output_index":0,"content_index":0,"text":"a secret",
                "logprobs":[{"token":" secret","logprob":0.0}]}),
        ),
    ]
    .concat();
    let mut b = Body::new(a.clone(), true, Dialect::Responses, true);
    let out = through(&mut b, &stream, 4);
    assert!(!out.contains("secret"), "{out}");
    // 观察档不改字，logprobs 留着
    let mut b = Body::new(a, false, Dialect::Responses, true);
    assert_eq!(through(&mut b, &stream, 4), stream);
}

/// Anthropic 开头一帧带着的字、拒答，都是正文
#[test]
fn block_start_text_and_refusals_are_filtered() {
    let a = answer(vec![rule("s", "secret", Match::Contains, Action::Strip)]);
    let stream = [
        named(
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"a secret "}}),
        ),
        named(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"and more"}}),
        ),
        named("content_block_stop", json!({"type":"content_block_stop","index":0})),
    ]
    .concat();
    let mut b = Body::new(a.clone(), true, Dialect::Anthropic, true);
    let out = through(&mut b, &stream, 2);
    assert!(!out.contains("secret"), "{out}");
    let text: String = frames_of(&out)
        .iter()
        .filter_map(|f| {
            f["content_block"]["text"]
                .as_str()
                .or(f["delta"]["text"].as_str())
        })
        .collect();
    assert_eq!(text, "a  and more");
    // Chat 的拒答
    let stream = [
        data(json!({"id":"c","choices":[{"index":0,"delta":{"refusal":"no sec"},"finish_reason":null}]})),
        data(json!({"id":"c","choices":[{"index":0,"delta":{"refusal":"ret here"},"finish_reason":"stop"}]})),
    ]
    .concat();
    let mut b = Body::new(a.clone(), true, Dialect::Chat, true);
    let out = through(&mut b, &stream, 6);
    let refusal: String = frames_of(&out)
        .iter()
        .filter_map(|f| f["choices"][0]["delta"]["refusal"].as_str())
        .collect();
    assert_eq!(refusal, "no  here");
    let (out, _) = whole(
        &a,
        true,
        Dialect::Chat,
        json!({"choices":[{"index":0,"message":{"content":null,"refusal":"a secret"}}]})
            .to_string()
            .as_bytes(),
    );
    assert!(!String::from_utf8(out.unwrap()).unwrap().contains("secret"));
    // Responses 的拒答：增量、`.done`、整段
    let stream = [
        responses_event(
            "response.refusal.delta",
            json!({"item_id":"m","output_index":0,"content_index":0,"delta":"a sec"}),
        ),
        responses_event(
            "response.refusal.delta",
            json!({"item_id":"m","output_index":0,"content_index":0,"delta":"ret"}),
        ),
        responses_event(
            "response.refusal.done",
            json!({"item_id":"m","output_index":0,"content_index":0,"refusal":"a secret"}),
        ),
        responses_event(
            "response.completed",
            json!({"response":{"output":[{"type":"message","content":[{"type":"refusal","refusal":"a secret"}]}]}}),
        ),
    ]
    .concat();
    let mut b = Body::new(a, true, Dialect::Responses, true);
    let out = through(&mut b, &stream, 8);
    assert!(!out.contains("secret"), "{out}");
    let refusal: String = frames_of(&out)
        .iter()
        .filter(|f| f["type"] == "response.refusal.delta")
        .filter_map(|f| f["delta"].as_str())
        .collect();
    assert_eq!(refusal, "a ");
}

/// 字节不是 UTF-8 的一帧、解析不了的一个数组元素：会改字时不原样转发
#[test]
fn broken_frames_do_not_carry_matches_through() {
    let a = answer(vec![rule("s", "secret", Match::Contains, Action::Strip)]);
    let raw = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"a secret \xff\"}}\n\n";
    let mut b = Body::new(a.clone(), true, Dialect::Anthropic, true);
    let mut out = b.process(raw);
    out.extend(b.flush());
    let out = String::from_utf8(out).expect("rewritten as UTF-8");
    assert!(!out.contains("secret") && out.contains('\u{FFFD}'), "{out}");
    // 数组里坏掉的一个元素：会改字时不发，数组照样完整
    let mut b = Body::new(a.clone(), true, Dialect::Gemini, false);
    let bytes = b"[{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]}}]},\r\n{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"a secret\",}]}}]}]";
    let mut out = b.process(bytes);
    out.extend(b.flush());
    let out = String::from_utf8(out).unwrap();
    assert!(!out.contains("secret"), "{out}");
    serde_json::from_str::<Value>(&out).expect("still one array");
    // 观察档原样
    let mut b = Body::new(a, false, Dialect::Gemini, false);
    let mut out = b.process(bytes);
    out.extend(b.flush());
    assert_eq!(out, bytes.to_vec());
}

/// 命中再多，一路正文里留下位置的也只有前几十处，个数照数
#[test]
fn many_matches_keep_counts_not_every_place() {
    let a = answer(vec![rule("a", "a", Match::Contains, Action::Record)]);
    let mut lane = a.lane(true);
    for _ in 0..200 {
        lane.push(&"a".repeat(500));
    }
    lane.finish();
    let m = lane.take();
    assert_eq!(m.total(), 100_000);
    assert_eq!(m.found.len(), PLACES_MAX);
    // 一次回答里各路合起来也一样：个数是全部，位置最多那么多
    let mut b = Body::new(a, true, Dialect::Anthropic, true);
    let stream: String = (0..50)
        .map(|_| {
            named(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"a".repeat(100)}}),
            )
        })
        .collect();
    through(&mut b, &stream, 1);
    let hits = b.take_hits();
    assert_eq!(hits[0].count, 5_000);
    assert_eq!(hits[0].places.len(), PLACES_MAX);
}

/// 每种格式在流里报错：扣着的尾巴补在错误那一帧**前面**，客户端拼出来的字一个不少
#[test]
fn held_text_comes_before_an_error_frame_in_every_format() {
    let a = star("k", "secret");
    let anthropic = |t: &str| {
        named(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":t}}),
        )
    };
    let chat = |t: &str| {
        data(json!({"id":"c","choices":[{"index":0,"delta":{"content":t},"finish_reason":null}]}))
    };
    let responses = |t: &str| {
        responses_event(
            "response.output_text.delta",
            json!({"item_id":"m","output_index":0,"content_index":0,"delta":t}),
        )
    };
    let gemini = |t: &str| json!({"candidates":[{"index":0,"content":{"parts":[{"text":t}]}}]});
    let cases: Vec<(Dialect, bool, String, String)> = vec![
        (
            Dialect::Anthropic,
            true,
            [
                anthropic("so se"),
                named(
                    "error",
                    json!({"type":"error","error":{"type":"overloaded_error"}}),
                ),
            ]
            .concat(),
            "\"overloaded_error\"".into(),
        ),
        (
            Dialect::Chat,
            true,
            [
                chat("so se"),
                data(json!({"error":{"message":"boom","type":"server_error"}})),
            ]
            .concat(),
            "\"boom\"".into(),
        ),
        (
            Dialect::Responses,
            true,
            [
                responses("so se"),
                responses_event("error", json!({"code":"server_error","message":"boom"})),
            ]
            .concat(),
            "\"boom\"".into(),
        ),
        (
            Dialect::Gemini,
            true,
            [
                data(gemini("so se")),
                data(json!({"error":{"code":500,"message":"boom"}})),
            ]
            .concat(),
            "\"boom\"".into(),
        ),
        (
            Dialect::Gemini,
            false,
            format!(
                "[{},\r\n{}]",
                gemini("so se"),
                json!({"error":{"code":500,"message":"boom"}})
            ),
            "\"boom\"".into(),
        ),
    ];
    for (d, sse, stream, error) in cases {
        let mut b = Body::new(a.clone(), true, d, sse);
        let out = through(&mut b, &stream, 3);
        let at = out.find(&error).unwrap_or_else(|| panic!("{d:?}: {out}"));
        let before = &out[..at];
        let text: String = if sse {
            frames_of(before)
                .iter()
                .filter_map(|f| {
                    f["delta"]["text"]
                        .as_str()
                        .or(f["choices"][0]["delta"]["content"].as_str())
                        .or(f["delta"].as_str())
                        .or(f["candidates"][0]["content"]["parts"][0]["text"].as_str())
                        .map(str::to_string)
                })
                .collect()
        } else {
            let v: Value = serde_json::from_str(&out).expect("still one array");
            v.as_array()
                .unwrap()
                .iter()
                .take_while(|e| e.get("error").is_none())
                .filter_map(|e| {
                    e["candidates"][0]["content"]["parts"][0]["text"]
                        .as_str()
                        .map(str::to_string)
                })
                .collect()
        };
        assert_eq!(text, "so se", "{d:?} sse={sse}: {out}");
        assert!(
            !out[at..].contains("so se") && !out[at..].contains("\"se\""),
            "{d:?}: 尾巴补在了错误之后：{out}"
        );
    }
}

/// 顶层的 `error` 只有是一个带着 `message` 或 `code` 的对象才算报错：`null`、`false`、空串、
/// 下面某一层里的 `error` 都不结束一路正文，切在两帧里的词照样认得出
#[test]
fn only_a_real_error_object_ends_the_lanes() {
    let a = star("k", "secret");
    let chat = |t: &str, extra: Value| {
        let mut v =
            json!({"id":"c","choices":[{"index":0,"delta":{"content":t},"finish_reason":null}]});
        if let (Some(m), Value::Object(e)) = (v.as_object_mut(), extra) {
            m.extend(e);
        }
        data(v)
    };
    for extra in [
        json!({"error": null}),
        json!({"error": false}),
        json!({"error": ""}),
        json!({"error": {}}),
        json!({"usage": {"error": {"message": "x"}}}),
    ] {
        let stream = [
            chat("a se", extra.clone()),
            chat("cret b", json!({})),
            chat("", json!({})),
        ]
        .concat();
        let mut b = Body::new(a.clone(), true, Dialect::Chat, true);
        let out = through(&mut b, &stream, 5);
        assert!(
            !out.contains("secret") && !out.contains("cret"),
            "{extra}: {out}"
        );
    }
    let g = |t: &str, extra: Value| {
        let mut v =
            json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":t}]}}]});
        if let (Some(m), Value::Object(e)) = (v.as_object_mut(), extra) {
            m.extend(e);
        }
        data(v)
    };
    let stream = [g("a se", json!({"error": null})), g("cret b", json!({}))].concat();
    let mut b = Body::new(a.clone(), true, Dialect::Gemini, true);
    let out = through(&mut b, &stream, 4);
    assert!(!out.contains("cret"), "{out}");
    // 只有错误、不带正文的一帧是在报错：字符串写的、没有 message 和 code 的对象，扣着的尾巴
    // 赶在它前面补出来
    for (dialect, first, err) in [
        (
            Dialect::Chat,
            chat("tail se", json!({})),
            json!({"error": "rate limited"}),
        ),
        (
            Dialect::Chat,
            chat("tail se", json!({})),
            json!({"error": {"type": "overloaded"}}),
        ),
        (
            Dialect::Gemini,
            g("tail se", json!({})),
            json!({"error": {"status": "UNAVAILABLE"}}),
        ),
    ] {
        let stream = [first, data(err.clone())].concat();
        let mut b = Body::new(a.clone(), true, dialect, true);
        let out = through(&mut b, &stream, 3);
        let tail = out.find("se").unwrap_or(usize::MAX);
        let error = out.find("error").unwrap_or(0);
        assert!(tail < error, "{dialect:?} {err}: 尾巴要在报错之前：{out}");
    }
}
