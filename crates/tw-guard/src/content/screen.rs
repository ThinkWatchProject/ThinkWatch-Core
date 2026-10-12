//! 在请求原文上查、删：两个网关的内容过滤都从 [`screen`] 进。
//!
//! # 结论
//!
//! - `off`：不查；
//! - `observe`：照样查，**每条命中都只记录**，请求原样发出 —— 处置档下会拒、会删的，
//!   这里一样报出来，结论和切过去之后得到的一致；
//! - `enforce`：命中了「拒绝」规则的，请求不发出去（拒绝的那几条记「已拒绝」，别的
//!   记「仅记录」，没发出去也就没删）；否则「删除」规则命中的字全部删掉、「替换」规则命中
//!   的字换成它的替换文本再发（记「已删除」「已替换」），「仅记录」规则照常记。
//!
//! 只用查请求的规则（[`super::Sides::request`]）；只查回答的规则在 [`super::answer`]。
//!
//! # 删掉之后再查一遍
//!
//! 零宽字符拆开的关键词（`jail\u{200B}break`），删掉零宽字符就拼回来了 —— 而拒绝规则
//! 查的是删之前的样子，拼回来的这一句会原样、干干净净地发给模型。所以删过之后再查一
//! 遍：这时才命中的拒绝规则照样拒绝，这时才冒出来的「删除」「替换」规则命中接着删、
//! 接着换，**直到再查不出可改的为止**。再查只查上一遍改过的那几段。一层套一层写到
//! [`MAX_ROUNDS`] 遍还没删干净的，那一段整段删掉：发出去的正文里不留一处命中。观察档也照
//! 这样查一遍（在一份副本上），报的和处置档下会发生的一样。
//!
//! 换上去的替换文本不再查：它是规则写的，不是调用方的话。和它重叠的命中不算，不然一条
//! 把 `*` 换成 `***` 的规则会一遍一遍地换它自己。
//!
//! # 删在原文上
//!
//! 在客户端格式的原始 JSON 上按消息结构找到调用方的正文（[`tw_dialect::caller`]），
//! 改完再写回字节。**只动调用方的正文**（用户消息、其中的工具结果），系统提示、模型
//! 自己的话、工具定义不动 —— 和查的范围一致。拿到新的请求体之后，调用方要用它重新
//! 解码中间表示：之后的转换、留档、脱敏、每一跳都用删过的那一份。

use std::ops::Range;

use serde_json::Value;
use tw_dialect::ir::Dialect;

use super::{Action, Hit, Rules, Scope};
use crate::policy::Mode;

/// 最多删、换几遍。正常的正文一两遍就干净了；到这么多遍还在冒出新命中的只能是刻意一层套
/// 一层写出来的，那一段整段删掉（见模块说明）
const MAX_ROUNDS: usize = 64;

/// 一条命中最后怎么样了。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "ContentOutcome"))]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// 仅记录：请求照常发出
    Recorded,
    /// 命中的字删掉之后发出
    Stripped,
    /// 命中的字换成规则的替换文本之后发出
    Replaced,
    /// 请求被拒，没有发出去
    Blocked,
}

words!(Outcome {
    Recorded = "recorded",
    Stripped = "stripped",
    Replaced = "replaced",
    Blocked = "blocked",
});

/// 一条规则的命中，和它最后怎么样了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenHit {
    pub hit: Hit,
    pub outcome: Outcome,
}

/// 一个请求查下来的结论。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Screening {
    /// 每条命中的规则一条，按发现的先后
    pub hits: Vec<ScreenHit>,
    /// 拒绝时，说了算的那条命中（`hits` 的下标）
    pub refused: Option<usize>,
    /// 删过之后的请求体；没删是 `None`
    pub body: Option<bytes::Bytes>,
}

impl Screening {
    /// 拒绝了这个请求的那条命中
    pub fn refusal(&self) -> Option<&ScreenHit> {
        self.refused.and_then(|i| self.hits.get(i))
    }
}

/// 查一个请求：`body` 是客户端发来的原文，`dialect` 是它的格式。
///
/// 解不开（不是 JSON、不是对象）的请求体没有调用方的正文可言，什么都不报。没法按消息
/// 结构读的文字（WebSocket 上一帧解不开的）用 [`screen_text`]。
pub fn screen(mode: Mode, rules: &Rules, dialect: Dialect, body: &[u8]) -> Screening {
    if !mode.detects() || !rules.checks_requests() {
        return Screening::default();
    }
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return Screening::default();
    };
    screen_value(mode, rules, dialect, &v)
}

/// [`screen`]，请求体已经解析好了（调用方为别的事解过一遍）：`v` 是它解出来的样子，不再
/// 解一遍。删过的请求体照它写出来，和 [`screen`] 写的一样。
pub fn screen_value(mode: Mode, rules: &Rules, dialect: Dialect, v: &Value) -> Screening {
    if !mode.detects() || !rules.checks_requests() {
        return Screening::default();
    }
    let spots = tw_dialect::caller::spots(dialect, v);
    if spots.is_empty() {
        return Screening::default();
    }
    let e = {
        let segments: Vec<(&str, bool)> = spots
            .iter()
            .map(|s| (s.get(v).unwrap_or_default(), s.in_tool_result))
            .collect();
        evaluate(rules, &segments, Scope::default())
    };
    let mut out = conclude(mode, e.hits, e.refused);
    if mode.acts()
        && out.refused.is_none()
        && let Some(texts) = e.texts
    {
        // 要删的时候才抄一份来改
        let mut v = v.clone();
        for (s, t) in spots.iter().zip(texts) {
            if let Some(x) = s.get_mut(&mut v) {
                *x = t;
            }
        }
        // 刚从字节解出来的值写回去，不会失败；真失败了宁可原样发，也不发半个请求体
        out.body = serde_json::to_vec(&v).ok().map(bytes::Bytes::from);
        if out.body.is_none() {
            for h in &mut out.hits {
                h.outcome = Outcome::Recorded;
            }
        }
    }
    out
}

/// 查一段没法按消息结构读的文字（桌面版 WebSocket 上解不开的帧）：**只用码位规则**。
///
/// 关键词和正则按整段原文查的话，系统提示里的话也会被当成调用方的；看不见的字符在
/// 任何地方都没有正当用途，整段查没有误伤谁。原文多半还是 JSON，看不见的字符可能写成
/// 转义（`\u200b`、代理对写的 `\udb40\udc49`），这里也认；删的时候整个转义序列一起删。
/// 删过之后的文字在 [`Screening::body`] 里。
pub fn screen_text(mode: Mode, rules: &Rules, text: &str) -> Screening {
    if !mode.detects() || !rules.checks_requests() {
        return Screening::default();
    }
    let scope = Scope {
        escapes: true,
        codepoints_only: true,
        keep_all: false,
    };
    let e = evaluate(rules, &[(text, false)], scope);
    let mut out = conclude(mode, e.hits, e.refused);
    if mode.acts()
        && out.refused.is_none()
        && let Some(mut texts) = e.texts
    {
        out.body = Some(bytes::Bytes::from(texts.swap_remove(0)));
    }
    out
}

/// 一条命中的规则在查过的正文里的每一处（见 [`places`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulePlaces {
    /// 出厂规则的 id，或者自定义规则的名字
    pub rule: String,
    pub custom: bool,
    /// 每一处：在第几段正文里、在那一段里的字节区间，按先后。删过一遍之后才冒出来的换算
    /// 回删之前的原文；码位规则连在一起的几个字符是一处
    pub at: Vec<(usize, Range<usize>)>,
}

/// 照 [`screen_value`] 的查法查一遍 —— 查、删、再查，**不看档位** —— 交回每条命中的规则的
/// 每一处。段是调用方正文的那几个字符串（[`tw_dialect::caller::spots`]，一起交回去），下标
/// 对得上。
///
/// 安全日志说得出一条规则在请求里命中了哪几处、各在哪儿；[`screen_value`] 的结论只留每条
/// 规则的第一处。只在有命中的请求上再查这一遍。
pub fn places(
    rules: &Rules,
    dialect: Dialect,
    v: &Value,
) -> (Vec<tw_dialect::caller::Spot>, Vec<RulePlaces>) {
    let spots = tw_dialect::caller::spots(dialect, v);
    if spots.is_empty() || !rules.checks_requests() {
        return (spots, Vec::new());
    }
    let segments: Vec<(&str, bool)> = spots
        .iter()
        .map(|s| (s.get(v).unwrap_or_default(), s.in_tool_result))
        .collect();
    let scope = Scope {
        keep_all: true,
        ..Scope::default()
    };
    let e = evaluate(rules, &segments, scope);
    (spots, gather(e))
}

/// [`places`]，查的是一段没法按消息结构读的文字（同 [`screen_text`]：只用码位规则，认得
/// JSON 的 `\uXXXX` 写法）。只有一段，下标都是 0，区间是原文里的（转义序列整个算）。
pub fn places_text(rules: &Rules, text: &str) -> Vec<RulePlaces> {
    if !rules.checks_requests() {
        return Vec::new();
    }
    let scope = Scope {
        keep_all: true,
        escapes: true,
        codepoints_only: true,
    };
    gather(evaluate(rules, &[(text, false)], scope))
}

fn gather(e: Evaluation) -> Vec<RulePlaces> {
    let mut out: Vec<RulePlaces> = e
        .hits
        .iter()
        .map(|h| RulePlaces {
            rule: h.rule.clone(),
            custom: h.custom,
            at: Vec::new(),
        })
        .collect();
    for (h, s, r) in e.all {
        if let Some(p) = out.get_mut(h) {
            p.at.push((s, r));
        }
    }
    for p in &mut out {
        p.at.sort_by_key(|(s, r)| (*s, r.start, r.end));
        p.at.dedup();
    }
    out
}

/// 按档位定每条命中的结局。
fn conclude(mode: Mode, hits: Vec<Hit>, refused: Option<usize>) -> Screening {
    let enforce = mode.acts();
    let refused = refused.filter(|_| enforce);
    let hits = hits
        .into_iter()
        .map(|hit| {
            let outcome = match (enforce, refused.is_some(), hit.action) {
                (true, true, Action::Block) => Outcome::Blocked,
                (true, false, Action::Strip) => Outcome::Stripped,
                (true, false, Action::Replace) => Outcome::Replaced,
                _ => Outcome::Recorded,
            };
            ScreenHit { hit, outcome }
        })
        .collect();
    Screening {
        hits,
        refused,
        body: None,
    }
}

/// 照处置档查下来的结果（不管档位：档位由调用方定结局）。
#[derive(Debug, Default)]
pub(crate) struct Evaluation {
    /// 每条命中的规则一处，按发现的先后
    pub(crate) hits: Vec<Hit>,
    /// 要拒绝时，说了算的那一条：发现它的那一遍里第一条拒绝规则
    pub(crate) refused: Option<usize>,
    /// 删过之后每段正文的样子；没删、或者要拒绝的是 `None`
    pub(crate) texts: Option<Vec<String>>,
    /// `keep_all` 时：每条命中规则的每一处，换算回原文。`(hits 的下标, 第几段, 区间)`
    pub(crate) all: Vec<(usize, usize, Range<usize>)>,
}

/// 查、删、再查，直到没有可删的（见模块说明）。
pub(crate) fn evaluate(rules: &Rules, segments: &[(&str, bool)], scope: Scope) -> Evaluation {
    let first = rules.detect(segments, scope, &[]);
    let mut by_rule: Vec<Option<usize>> = vec![None; rules.rules.len()];
    for (h, &r) in first.rules.iter().enumerate() {
        by_rule[r] = Some(h);
    }
    let mut e = Evaluation {
        refused: first.hits.iter().position(|h| h.action == Action::Block),
        hits: first.hits,
        texts: None,
        all: first
            .all
            .into_iter()
            .flat_map(|(h, s, ranges)| ranges.into_iter().map(move |r| (h, s, r)))
            .collect(),
    };
    let mut edits = first.edits;
    if e.refused.is_some() || edits.iter().all(Vec::is_empty) {
        return e;
    }
    let mut texts: Vec<String> = segments.iter().map(|(t, _)| t.to_string()).collect();
    // 每段正文每一遍改了哪些（按那一遍改之前的位置，和换上去的字有几个字节），把后来的位置
    // 换算回原文用
    let mut rounds: Vec<Vec<Round>> = vec![Vec::new(); segments.len()];
    // 每段正文里换上去的替换文本在哪儿（按现在的位置）：再查时不算
    let mut kept: Vec<Vec<Range<usize>>> = vec![Vec::new(); segments.len()];
    for _ in 0..MAX_ROUNDS {
        // 这一遍改过的那几段：只有它们会冒出新的命中
        let mut changed = vec![false; segments.len()];
        for (si, todo) in edits.into_iter().enumerate() {
            let merged = rules.merged(todo);
            if !merged.is_empty() {
                texts[si] = super::apply_edits(&texts[si], &merged);
                kept[si] = shift_kept(&kept[si], &merged);
                changed[si] = true;
            }
            rounds[si].push(merged.iter().map(|(r, w)| (r.clone(), w.len())).collect());
        }
        let now: Vec<(&str, bool)> = texts
            .iter()
            .zip(segments)
            .zip(&changed)
            .map(|((t, (_, in_tool_result)), &c)| {
                (if c { t.as_str() } else { "" }, *in_tool_result)
            })
            .collect();
        let next = rules.detect(&now, scope, &kept);
        let mut blocked = None;
        let mut index = Vec::with_capacity(next.hits.len());
        for (k, mut h) in next.hits.into_iter().enumerate() {
            let r = next.rules[k];
            match by_rule[r] {
                // 先前就命中的规则：删除、替换规则又冒出来的几处算进去
                Some(at) => {
                    if h.action.edits() {
                        e.hits[at].count += h.count;
                    }
                    index.push(at);
                }
                None => {
                    h.bytes = back(&rounds[next.segments[k]], h.bytes);
                    if h.action == Action::Block && blocked.is_none() {
                        blocked = Some(e.hits.len());
                    }
                    e.hits.push(h);
                    by_rule[r] = Some(e.hits.len() - 1);
                    index.push(e.hits.len() - 1);
                }
            }
        }
        // 先前就在的那几处换回原文还是原来的位置，只记新冒出来的
        for (k, s, ranges) in next.all {
            for r in ranges {
                let item = (index[k], s, back(&rounds[s], r));
                if !e.all.contains(&item) {
                    e.all.push(item);
                }
            }
        }
        if blocked.is_some() {
            e.refused = blocked;
            return e;
        }
        edits = next.edits;
        if edits.iter().all(Vec::is_empty) {
            break;
        }
    }
    // 到头了还有可改的：那几段整段删掉，不留一处命中发出去
    for (si, todo) in edits.iter().enumerate() {
        if !todo.is_empty() {
            texts[si].clear();
        }
    }
    e.texts = Some(texts);
    e
}

/// 一段正文一遍里的改动：改之前的区间，和换上去的字有几个字节（删掉是 0）
type Round = Vec<(Range<usize>, usize)>;

/// 这一遍改过之后，换上去的替换文本各在哪儿：先前的几段跟着前面的改动挪（改动从不和它们
/// 重叠，见 [`Rules::detect`] 的 `kept`），这一遍换上去的接着记。按先后
fn shift_kept(kept: &[Range<usize>], edits: &[(Range<usize>, &str)]) -> Vec<Range<usize>> {
    let mut out = Vec::with_capacity(kept.len() + edits.len());
    // 改之前的位置 → 改之后的：前面每一处改动让后面挪了多少
    let mut delta: isize = 0;
    let mut k = kept.iter().peekable();
    let moved = |r: &Range<usize>, delta: isize| {
        r.start.saturating_add_signed(delta)..r.end.saturating_add_signed(delta)
    };
    for (r, with) in edits {
        while let Some(old) = k.next_if(|old| old.end <= r.start) {
            out.push(moved(old, delta));
        }
        let at = r.start.saturating_add_signed(delta);
        if !with.is_empty() {
            out.push(at..at + with.len());
        }
        delta += with.len() as isize - r.len() as isize;
    }
    out.extend(k.map(|old| moved(old, delta)));
    out
}

/// 改过几遍之后的一个区间，换算回改之前的原文：从最后一遍往回，一遍一遍地换。
///
/// 起点落在一处删掉的地方上时，换到删掉的那段**后面**（不把删掉的字算进命中）；终点
/// 落在那里时换到**前面**。`jail‹U+200B›break` 删掉零宽字符后命中的 `jailbreak`，换回
/// 原文是包括零宽字符在内的整段。落在换上去的替换文本里的，起点换到被换掉的那段的开头、
/// 终点换到它的结尾。
fn back(rounds: &[Round], r: Range<usize>) -> Range<usize> {
    let mut r = r;
    for edits in rounds.iter().rev() {
        r = unshift(edits, r.start, false)..unshift(edits, r.end, true);
    }
    r
}

/// 改过之后的位置 `p` 换回改之前的。`edits` 是那一遍的改动：改之前的区间，和换上去的字有
/// 几个字节（删掉是 0）
fn unshift(edits: &[(Range<usize>, usize)], p: usize, end: bool) -> usize {
    // 改之后比改之前多出来的字节数（删掉的是负的）
    let mut delta: isize = 0;
    for (r, ins) in edits {
        // 这一处改动在改过之后的文字里从哪儿到哪儿
        let at = r.start.saturating_add_signed(delta);
        let fin = at + ins;
        if p < at || (end && p == at) {
            break;
        }
        if p < fin {
            return if end { r.end } else { r.start };
        }
        delta += *ins as isize - r.len() as isize;
    }
    p.saturating_add_signed(-delta)
}

#[cfg(test)]
mod tests {
    use super::super::Action::{Block, Record, Strip};
    use super::super::Match::{Codepoints as Points, Contains, Regex};
    use super::super::RuleInput;
    use super::*;
    use serde_json::json;

    fn rules(list: &[(&'static str, &'static str, super::super::Match, Action)]) -> Rules {
        Rules::build(
            list.iter()
                .map(|&(id, pattern, matching, action)| RuleInput {
                    id,
                    name: id,
                    custom: true,
                    pattern,
                    matching,
                    action,
                    ..RuleInput::default()
                }),
        )
        .unwrap()
    }

    fn body(v: Value) -> Vec<u8> {
        serde_json::to_vec(&v).unwrap()
    }

    fn sent(s: &Screening) -> Value {
        serde_json::from_slice(s.body.as_ref().expect("a new body")).unwrap()
    }

    fn outcomes(s: &Screening) -> Vec<(&str, Outcome)> {
        s.hits
            .iter()
            .map(|h| (h.hit.rule.as_str(), h.outcome))
            .collect()
    }

    const ZW: &str = "U+200B–U+200D, U+2060, U+FEFF";

    #[test]
    fn off_does_not_look_and_observe_only_records() {
        let rs = rules(&[("j", "jailbreak", Contains, Block)]);
        let b = body(json!({"messages": [{"role": "user", "content": "jailbreak"}]}));
        assert_eq!(
            screen(Mode::Off, &rs, Dialect::Anthropic, &b),
            Screening::default()
        );
        let s = screen(Mode::Observe, &rs, Dialect::Anthropic, &b);
        assert_eq!(outcomes(&s), [("j", Outcome::Recorded)]);
        assert!(s.refused.is_none() && s.body.is_none());
    }

    #[test]
    fn a_block_refuses_and_nothing_is_stripped() {
        let rs = rules(&[
            ("j", "jailbreak", Contains, Block),
            ("tags", "U+E0000–U+E007F", Points, Strip),
            ("hi", "hello", Contains, Record),
        ]);
        let b = body(json!({"messages": [{"role": "user",
            "content": "hello \u{E0041} jailbreak"}]}));
        let s = screen(Mode::Enforce, &rs, Dialect::Anthropic, &b);
        assert_eq!(
            outcomes(&s),
            [
                ("j", Outcome::Blocked),
                ("tags", Outcome::Recorded),
                ("hi", Outcome::Recorded)
            ]
        );
        assert_eq!(s.refusal().unwrap().hit.rule, "j");
        assert!(s.body.is_none(), "没发出去，也就没删");
    }

    #[test]
    fn strip_deletes_every_place_and_only_in_the_callers_text() {
        let rs = rules(&[
            ("tags", "U+E0000–U+E007F", Points, Strip),
            ("x", "secret", Contains, Strip),
            ("re", r"code-\d+", Regex, Strip),
            ("hi", "hello", Contains, Record),
        ]);
        let b = body(json!({
            "system": "secret \u{E0041} stays in the system prompt",
            "messages": [
                {"role": "user", "content": "hello SECRET a\u{E0041}\u{E0042}b code-1 and Secret code-22"},
                {"role": "assistant", "content": "secret said the model"},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t",
                    "content": "page: secret\u{E0043}"}]},
            ]
        }));
        let s = screen(Mode::Enforce, &rs, Dialect::Anthropic, &b);
        assert!(s.refused.is_none());
        let by: Vec<(&str, Outcome, usize)> = s
            .hits
            .iter()
            .map(|h| (h.hit.rule.as_str(), h.outcome, h.hit.count))
            .collect();
        assert_eq!(
            by,
            [
                ("tags", Outcome::Stripped, 3),
                ("x", Outcome::Stripped, 3),
                ("re", Outcome::Stripped, 2),
                ("hi", Outcome::Recorded, 1),
            ]
        );
        let v = sent(&s);
        assert_eq!(v["messages"][0]["content"], "hello  ab  and  ");
        assert_eq!(v["messages"][2]["content"][0]["content"], "page: ");
        assert_eq!(
            v["system"], "secret \u{E0041} stays in the system prompt",
            "系统提示不动"
        );
        assert_eq!(v["messages"][1]["content"], "secret said the model");
        // 观察档报的一样，只是都只记录、原样发
        let o = screen(Mode::Observe, &rs, Dialect::Anthropic, &b);
        assert!(o.body.is_none());
        assert!(o.hits.iter().all(|h| h.outcome == Outcome::Recorded));
        assert_eq!(o.hits.len(), 4);
    }

    #[test]
    fn a_keyword_split_by_hidden_characters_is_caught_once_they_are_deleted() {
        // 零宽字符拆开的关键词，删掉零宽字符就拼回来了：拼回来的那一句不能干干净净地发出去
        let rs = rules(&[
            ("ignore", "ignore previous instructions", Contains, Block),
            ("zw", ZW, Points, Strip),
        ]);
        let b = body(json!({"messages": [{"role": "user",
            "content": "ig\u{200B}nore previous instructions"}]}));
        let s = screen(Mode::Enforce, &rs, Dialect::Anthropic, &b);
        assert_eq!(
            outcomes(&s),
            [("zw", Outcome::Recorded), ("ignore", Outcome::Blocked)]
        );
        assert_eq!(s.refusal().unwrap().hit.rule, "ignore");
        assert!(s.body.is_none());
        // 换回原文：整段，包括删掉的那个字符
        let text = "ig\u{200B}nore previous instructions";
        assert_eq!(&text[s.hits[1].hit.bytes.clone()], text);
        // 观察档报的一样
        let o = screen(Mode::Observe, &rs, Dialect::Anthropic, &b);
        assert_eq!(
            o.hits
                .iter()
                .map(|h| h.hit.rule.as_str())
                .collect::<Vec<_>>(),
            ["zw", "ignore"]
        );
        assert!(o.refused.is_none());
    }

    #[test]
    fn deletion_goes_on_until_nothing_reappears() {
        // 删一遍又拼出一个来的，接着删
        let rs = rules(&[("x", "secret", Contains, Strip)]);
        let b = body(json!({"messages": [{"role": "user", "content": "sesecretcret!"}]}));
        let s = screen(Mode::Enforce, &rs, Dialect::Anthropic, &b);
        assert_eq!(sent(&s)["messages"][0]["content"], "!");
        assert_eq!(s.hits[0].hit.count, 2);
    }

    #[test]
    fn a_body_that_cannot_be_read_is_left_alone() {
        let rs = rules(&[("j", "jailbreak", Contains, Block)]);
        for b in [&b"not json"[..], b"[\"jailbreak\"]", b"{}"] {
            assert_eq!(
                screen(Mode::Enforce, &rs, Dialect::Chat, b),
                Screening::default()
            );
        }
    }

    #[test]
    fn the_new_body_still_carries_everything_else() {
        let rs = rules(&[("zw", ZW, Points, Strip)]);
        let b = body(json!({
            "model": "m", "max_tokens": 9, "stream": true,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "a\u{200B}b", "cache_control": {"type": "ephemeral"}},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
            ]}]
        }));
        let s = screen(Mode::Enforce, &rs, Dialect::Anthropic, &b);
        let v = sent(&s);
        assert_eq!(v["messages"][0]["content"][0]["text"], "ab");
        assert_eq!(
            v["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(v["messages"][0]["content"][1]["source"]["data"], "AAAA");
        assert_eq!(
            (v["model"].as_str(), v["max_tokens"].as_u64()),
            (Some("m"), Some(9))
        );
    }

    #[test]
    fn raw_text_is_checked_for_code_points_only_written_either_way() {
        let rs = rules(&[
            ("j", "jailbreak", Contains, Block),
            ("tags", "U+E0000–U+E007F", Points, Strip),
        ]);
        let text = r#"{"type":"x","note":"jailbreak a\udb40\udc41b c\u{E0042}d"}"#
            .replace("\\u{E0042}", "\u{E0042}");
        let s = screen_text(Mode::Enforce, &rs, &text);
        assert_eq!(
            outcomes(&s),
            [("tags", Outcome::Stripped)],
            "关键词不按整段原文查"
        );
        assert_eq!(s.hits[0].hit.count, 2);
        assert_eq!(s.hits[0].hit.revealed, "AB");
        assert_eq!(
            std::str::from_utf8(s.body.as_ref().unwrap()).unwrap(),
            r#"{"type":"x","note":"jailbreak ab cd"}"#
        );
        // 转义过的反斜杠后面的不是转义
        let plain = r#"{"note":"C:\\udb40\\udc41"}"#;
        assert!(screen_text(Mode::Enforce, &rs, plain).hits.is_empty());
        // 拒绝也照样
        let block = rules(&[("tags", "U+E0000–U+E007F", Points, Block)]);
        let s = screen_text(Mode::Enforce, &block, &text);
        assert_eq!(outcomes(&s), [("tags", Outcome::Blocked)]);
        assert!(s.body.is_none());
    }

    #[test]
    fn positions_are_mapped_back_through_each_deletion() {
        // 删掉了 2..5 和 8..9：删过之后，这两处在 2 和 5
        let c1 = vec![(2..5, 0), (8..9, 0)];
        assert_eq!(unshift(&c1, 2, false), 5, "起点越过删掉的");
        assert_eq!(unshift(&c1, 2, true), 2, "终点停在删掉的前面");
        assert_eq!(unshift(&c1, 6, true), 10);
        assert_eq!(back(std::slice::from_ref(&c1), 0..2), 0..2);
        // 两遍：先删了 1..2，再删了 0..1
        let once = |r: Range<usize>| vec![(r, 0)];
        assert_eq!(back(&[once(1..2), once(0..1)], 0..1), 2..3);
        // 2..5 换成了三个字节、8..9 换成了一个：改过之后在 2..5 和 8..9
        let c2 = vec![(2..5, 3), (8..9, 1)];
        assert_eq!(unshift(&c2, 5, false), 5, "替换文本之后的起点");
        assert_eq!(unshift(&c2, 2, true), 2, "替换文本之前的终点");
        assert_eq!(
            unshift(&c2, 3, false),
            2,
            "落在替换文本里的起点换到被换掉的开头"
        );
        assert_eq!(unshift(&c2, 4, true), 5, "终点换到结尾");
        // 换得比原来长：1..2 换成了五个字节，之后的整体往后挪了四个
        let c3 = vec![(1..2, 5)];
        assert_eq!(unshift(&c3, 7, false), 3);
        assert_eq!(back(&[c3], 6..8), 2..4);
    }

    /// 替换文本不再查：一条把 `*` 换成 `***` 的规则换一遍就停，计数是原文里的几处
    #[test]
    fn a_replacement_is_not_matched_again() {
        let rs = Rules::build([RuleInput {
            id: "star",
            name: "star",
            custom: true,
            pattern: "*",
            action: Action::Replace,
            ..RuleInput::default()
        }])
        .unwrap();
        let b = body(json!({"messages": [{"role": "user", "content": "a*b**c"}]}));
        let s = screen(Mode::Enforce, &rs, Dialect::Anthropic, &b);
        assert_eq!(sent(&s)["messages"][0]["content"], "a***b******c");
        assert_eq!(outcomes(&s), [("star", Outcome::Replaced)]);
        assert_eq!(s.hits[0].hit.count, 3);
    }

    /// 替换规则命中的字换成它的替换文本；和删除规则重叠的合成一段，换成替换文本。只查回答的
    /// 规则不参与请求这一边
    #[test]
    fn replace_writes_the_replacement_and_answer_only_rules_stay_out() {
        let rs = Rules::build([
            RuleInput {
                id: "code",
                name: "code",
                custom: true,
                pattern: "北极星",
                action: Action::Replace,
                replacement: "[已隐去]",
                ..RuleInput::default()
            },
            RuleInput {
                id: "star",
                name: "star",
                custom: true,
                pattern: "星项",
                action: Action::Strip,
                ..RuleInput::default()
            },
            RuleInput {
                id: "answer-only",
                name: "answer-only",
                custom: true,
                pattern: "发布",
                action: Action::Strip,
                sides: super::super::Sides {
                    request: false,
                    response: true,
                },
                ..RuleInput::default()
            },
        ])
        .unwrap();
        let b = body(
            json!({"messages": [{"role": "user", "content": "请把北极星项目的发布说明发来"}]}),
        );
        let s = screen(Mode::Enforce, &rs, Dialect::Anthropic, &b);
        assert_eq!(
            sent(&s)["messages"][0]["content"],
            "请把[已隐去]目的发布说明发来"
        );
        assert_eq!(
            outcomes(&s),
            [("code", Outcome::Replaced), ("star", Outcome::Stripped)]
        );
        // 观察档照样报，只是都只记录
        let o = screen(Mode::Observe, &rs, Dialect::Anthropic, &b);
        assert!(o.body.is_none());
        assert!(o.hits.iter().all(|h| h.outcome == Outcome::Recorded));
        // 只有只查回答的规则：请求这一边什么都不查
        let only = Rules::of(rs.rules[2..].to_vec());
        assert_eq!(
            screen(Mode::Enforce, &only, Dialect::Anthropic, &b),
            Screening::default()
        );
    }

    /// 小写之后单个字的字节数变了、总长没变的正文（`Ⱥ` 小写多一个字节，开尔文符号少两个）：
    /// 删、换的是命中的那几个字，不偏不倚；请求和回答两边认的一样
    #[test]
    fn a_letter_whose_lowercase_has_another_length_does_not_shift_the_cut() {
        // 码位用 `char::from_u32` 拼：测试里直接写出来的转义，经过某些编辑工具会变成真字符
        let ch = |n: u32| char::from_u32(n).unwrap();
        let (a_bar, kelvin) = (ch(0x23A), ch(0x212A));
        let text = format!("{a_bar}{a_bar} abc {kelvin}");
        for (action, want) in [
            (Action::Strip, format!("{a_bar}{a_bar}  {kelvin}")),
            (Action::Replace, format!("{a_bar}{a_bar} *** {kelvin}")),
        ] {
            let input = |sides| RuleInput {
                id: "r",
                name: "r",
                custom: true,
                pattern: "ABC",
                action,
                sides,
                ..RuleInput::default()
            };
            let rs = Rules::build([input(super::super::Sides::default())]).unwrap();
            let b = body(json!({"messages": [{"role": "user", "content": text}]}));
            let s = screen(Mode::Enforce, &rs, Dialect::Anthropic, &b);
            assert_eq!(
                sent(&s)["messages"][0]["content"],
                want.as_str(),
                "{action:?}"
            );
            let answer = super::super::answer::Answer::build(
                &Rules::build([input(super::super::Sides {
                    request: false,
                    response: true,
                })])
                .unwrap(),
            )
            .unwrap();
            assert_eq!(answer.lane(true).whole(&text), want, "{action:?}");
        }
        // 开尔文符号本身也是 `k`
        let rs = rules(&[("k", "k", Contains, Strip)]);
        let b =
            body(json!({"messages": [{"role": "user", "content": format!("{kelvin}{a_bar}k")}]}));
        let s = screen(Mode::Enforce, &rs, Dialect::Anthropic, &b);
        assert_eq!(sent(&s)["messages"][0]["content"], a_bar.to_string());
    }

    /// 一层套一层写出来的：删到不再冒出新命中为止；套到 [`MAX_ROUNDS`] 层还没删干净的，那一段
    /// 整段删掉，发出去的正文里不留一处命中
    #[test]
    fn nested_matches_are_removed_until_none_is_left() {
        let rs = rules(&[("ab", "ab", Contains, Strip)]);
        let nested = |depth: usize| format!("x{}{}y", "a".repeat(depth), "b".repeat(depth));
        let b = body(json!({"messages": [
            {"role": "user", "content": nested(10)},
            {"role": "user", "content": "untouched"},
        ]}));
        let s = screen(Mode::Enforce, &rs, Dialect::Anthropic, &b);
        assert_eq!(sent(&s)["messages"][0]["content"], "xy");
        assert_eq!(sent(&s)["messages"][1]["content"], "untouched");
        assert_eq!(s.hits[0].hit.count, 10);
        let b = body(json!({"messages": [{"role": "user", "content": nested(MAX_ROUNDS + 5)}]}));
        let s = screen(Mode::Enforce, &rs, Dialect::Anthropic, &b);
        assert_eq!(sent(&s)["messages"][0]["content"], "");
        // 替换也一样，换上去的字不再查：`aabb` 换一遍是 `a#b`，`#` 和两边拼不成新的命中
        let rs = Rules::build([RuleInput {
            id: "ab",
            name: "ab",
            custom: true,
            pattern: "ab",
            action: Action::Replace,
            replacement: "#",
            ..RuleInput::default()
        }])
        .unwrap();
        let b = body(json!({"messages": [{"role": "user", "content": "aabb"}]}));
        let s = screen(Mode::Enforce, &rs, Dialect::Anthropic, &b);
        assert_eq!(sent(&s)["messages"][0]["content"], "a#b");
    }

    /// 分大小写的规则只认写的那样
    #[test]
    fn a_case_sensitive_rule_matches_only_as_written() {
        let rs = Rules::build([
            RuleInput {
                id: "exact",
                name: "exact",
                custom: true,
                pattern: "Polaris",
                action: Action::Strip,
                case_sensitive: true,
                ..RuleInput::default()
            },
            RuleInput {
                id: "re",
                name: "re",
                custom: true,
                pattern: "TKT-[0-9]+",
                matching: Regex,
                action: Action::Strip,
                case_sensitive: true,
                ..RuleInput::default()
            },
        ])
        .unwrap();
        let b =
            body(json!({"messages": [{"role": "user", "content": "Polaris polaris TKT-1 tkt-2"}]}));
        let s = screen(Mode::Enforce, &rs, Dialect::Anthropic, &b);
        assert_eq!(sent(&s)["messages"][0]["content"], " polaris  tkt-2");
    }

    /// 每条命中的规则的每一处：哪个字符串、在它里面的哪一段。删过一遍之后才拼出来的也在，
    /// 换算回删之前的原文
    #[test]
    fn places_name_every_spot_a_rule_matched() {
        let rs = rules(&[
            ("j", "jailbreak", Contains, Block),
            ("zw", ZW, Points, Strip),
            ("hi", "hello", Contains, Record),
        ]);
        let v = json!({"messages": [
            {"role": "user", "content": "hello, hello"},
            {"role": "assistant", "content": "hello from the model"},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t", "content": "jail\u{200B}break"}
            ]}
        ]});
        let (spots, places) = super::places(&rs, Dialect::Anthropic, &v);
        let of = |id: &str| places.iter().find(|p| p.rule == id).unwrap().at.clone();
        // 模型自己的话不查
        assert_eq!(of("hi"), [(0, 0..5), (0, 7..12)]);
        assert_eq!(spots[0].get(&v), Some("hello, hello"));
        // 删掉零宽字符才拼出来的那个词：区间是删之前的，连同零宽字符
        let text = spots[1].get(&v).unwrap();
        let [(s, r)] = &of("j")[..] else {
            panic!("{places:?}")
        };
        assert_eq!((*s, &text[r.clone()]), (1, "jail\u{200B}break"));
        assert_eq!(of("zw").len(), 1);
        assert!(spots[1].in_tool_result);

        // 没法按结构读的一段：区间是原文里的，转义序列整个算
        let raw = format!("{{\"x\":\"a{}u200bb\"}}", '\\');
        let places = super::places_text(&rs, &raw);
        let [p] = &places[..] else {
            panic!("{places:?}")
        };
        let (s, r) = &p.at[0];
        assert_eq!(*s, 0);
        assert_eq!(&raw[r.clone()], format!("{}u200b", '\\'));
        // 画出来是那个字符
        let zw = rs.rules.iter().find(|r| r.id == "zw").unwrap();
        assert_eq!(zw.draw(&raw[r.clone()], true), "‹U+200B›");
        assert_eq!(zw.draw("a\u{200B}\u{200B}b", false), "a‹U+200B ×2›b");
        let hi = rs.rules.iter().find(|r| r.id == "hi").unwrap();
        assert_eq!(hi.draw("a\u{200B}b", false), "a\u{200B}b", "别的规则原样");
    }
}
