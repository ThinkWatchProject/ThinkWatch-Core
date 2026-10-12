//! 回答正文的过滤：内容规则里查回答的那几条（`scope` 含 `response`）在模型的回答上怎么找、
//! 怎么删、怎么换。
//!
//! # 不攒整份回答
//!
//! 流式的回答一个字一个字地到，整份攒齐了再发，客户端看到的就是「卡很久然后一下全出来」。
//! 所以每一路正文（[`Lane`]）**只扣住可能还会长成一处命中的那一截尾巴**，前面的照常发：
//!
//! - 「包含」规则：一处命中的字数就是要找的那段字的字数（不分大小写时一个字也只对一个字），
//!   扣住的不超过最长那条的字数减一；
//! - 「正则」规则：按正则算得出最长能匹配多少字的，扣那么多（用到 `\b`、`$` 这类要看下一个
//!   字的，多扣一个）；算不出、或者超过 [`REGEX_HOLD_MAX`] 的，扣 [`REGEX_HOLD_MAX`] 个字。
//!   比它长的命中，开头那一截可能已经发出去了 —— 规则对话框里写明了这一条。
//!
//! 一处命中只在它前后的字都到齐了才认定（[`Lane::push`]）：认定了的删掉或者换掉，命中的字
//! 一个都不发给客户端。一路正文结束、整条流结束、或者出错的时候，扣着的尾巴按到这里为止的
//! 样子查完、发出去（[`Lane::finish`]）—— 丢掉它就是丢掉模型说过的字。
//!
//! **流式和整份是同一个结果。**同一段正文不管被切成多少块、切在哪个字中间，删出来、换出来
//! 的和整份一次处理的一模一样（比 [`REGEX_HOLD_MAX`] 长的正则命中除外）；整份的回答就是
//! 「一块」（[`Lane::whole`]）。
//!
//! # 观察档不扣
//!
//! 观察档（或者查回答的规则全是「仅记录」）不改任何字，也就不用扣：收到什么马上发什么，只是
//! 在旁边照样找、照样记。
//!
//! # 按格式、按路
//!
//! 一条流里怎么拆出每一路正文、一路结束时扣着的尾巴怎么补成一帧、`output_text.done` 这类
//! 带着整段正文的帧怎么改成和发出去的一致，在 [`frames`]。

use std::ops::Range;
use std::sync::Arc;

use super::{Action, Match, Rules, apply_edits, merge_edits};

pub mod frames;

pub use frames::{AnswerHit, ArrayFilter, Body, FrameFilter, LaneKey, SseFilter, whole};

/// 正则规则最多扣住多少个字。比它长的命中，开头那一截可能已经发出去了
pub const REGEX_HOLD_MAX: usize = 256;

/// 一条规则在一次回答里最多留几处的位置。再多的只数个数
pub const PLACES_MAX: usize = 50;

/// 命中前后各留多少个字
const CONTEXT: usize = 80;

/// 命中的那一段最多留多少个字
const MATCHED_KEEP: usize = 512;

/// 查回答的规则，编好的。**和配置一起建、一起换**，每次回答各起各的 [`Lane`]。
#[derive(Debug)]
pub struct Answer {
    rules: Vec<AnswerRule>,
    /// 每一路最多扣住几个字：各条规则要扣的取大的
    hold: usize,
    /// 有没有会改字的规则（删除、替换）。没有的话不用扣
    edits: bool,
}

#[derive(Debug)]
struct AnswerRule {
    id: String,
    name: String,
    custom: bool,
    pattern: String,
    matching: Match,
    case_sensitive: bool,
    action: Action,
    replacement: String,
    /// 关键词也编成正则：不分大小写时一个字对一个字（Unicode 的简单大小写对应），字数好算
    re: regex::Regex,
    /// 这条规则要扣几个字
    hold: usize,
    /// 一处命中最长多少个字算得出来、也不超过 [`REGEX_HOLD_MAX`]。算不出的那种，一处命中
    /// 碰到了收到的末尾就可能还在长
    bounded: bool,
}

impl Answer {
    /// 从一组内容规则里挑出查回答的那几条编好。一条都没有是 `None`：**调用方据此整条短路**。
    ///
    /// 码位规则不查回答（配置校验时就拒了），万一混进来也跳过。
    pub fn build(rules: &Rules) -> Option<Arc<Answer>> {
        let mut out = Vec::new();
        for r in rules.rules.iter().filter(|r| r.sides.response) {
            let Some(a) = AnswerRule::new(r) else {
                continue;
            };
            out.push(a);
        }
        if out.is_empty() {
            return None;
        }
        Some(Arc::new(Answer {
            hold: out.iter().map(|r| r.hold).max().unwrap_or(0),
            edits: out.iter().any(|r| r.action.edits()),
            rules: out,
        }))
    }

    /// 每一路最多扣住几个字
    pub fn hold(&self) -> usize {
        self.hold
    }

    /// 有没有会改字的规则（删除、替换）。都是仅记录的话，第三档下也不改客户端收到的字
    pub fn changes_text(&self) -> bool {
        self.edits
    }

    /// 这一处是哪条规则命中的
    pub fn rule(&self, f: &Found) -> Option<RuleRef<'_>> {
        self.rules.get(f.rule).map(RuleRef)
    }

    /// 第几条规则（[`Found::rule_index`]）
    pub fn rule_at(&self, i: usize) -> Option<RuleRef<'_>> {
        self.rules.get(i).map(RuleRef)
    }

    /// 一路新的正文。`acts`：第三档（规则按各自的动作改字）；观察档是 `false`，只记录
    pub fn lane(self: &Arc<Self>, acts: bool) -> Lane {
        Lane::new(self.clone(), acts)
    }
}

impl AnswerRule {
    fn new(r: &super::Rule) -> Option<Self> {
        let (source, literal) = match r.matching {
            Match::Contains => (regex::escape(&r.pattern), true),
            Match::Regex => (r.pattern.trim().to_string(), false),
            Match::Codepoints => return None,
        };
        if source.is_empty() {
            return None;
        }
        let re = crate::bounded_with(&source, !r.case_sensitive).ok()?;
        let (hold, bounded) = if literal {
            (r.pattern.chars().count().saturating_sub(1), true)
        } else {
            regex_hold(&source, !r.case_sensitive)
        };
        Some(AnswerRule {
            id: r.id.clone(),
            name: r.name.clone(),
            custom: r.custom,
            pattern: r.pattern.clone(),
            matching: r.matching,
            case_sensitive: r.case_sensitive,
            action: r.action,
            replacement: r.replacement.clone(),
            re,
            hold,
            bounded,
        })
    }

    /// 命中的字换成什么：替换是替换文本，删除是 `None`（删掉），仅记录不改
    fn edit_with(&self) -> Option<&str> {
        (self.action == Action::Replace).then_some(self.replacement.as_str())
    }
}

/// 一条正则要扣几个字，和一处命中的长度有没有上限：最长能匹配几个字，用到要看下一个字的
/// 断言（`\b`、`$`）时多扣一个。算不出、超过上限的扣 [`REGEX_HOLD_MAX`]
fn regex_hold(source: &str, ignore_case: bool) -> (usize, bool) {
    let parsed = regex_syntax::ParserBuilder::new()
        .case_insensitive(ignore_case)
        .build()
        .parse(source);
    let Ok(hir) = parsed else {
        return (REGEX_HOLD_MAX, false);
    };
    let Some(max) = max_chars(&hir) else {
        return (REGEX_HOLD_MAX, false);
    };
    let hold = if hir.properties().look_set().is_empty() {
        max.saturating_sub(1)
    } else {
        max
    };
    if hold > REGEX_HOLD_MAX {
        (REGEX_HOLD_MAX, false)
    } else {
        (hold, true)
    }
}

/// 一条正则最多能匹配几个字。没有上限（`+`、`*`）是 `None`。字符类、不分大小写的字母都是
/// 一个字；字节写的字面量按它解出来的字数算
fn max_chars(hir: &regex_syntax::hir::Hir) -> Option<usize> {
    use regex_syntax::hir::HirKind;
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => Some(0),
        HirKind::Literal(l) => Some(String::from_utf8_lossy(&l.0).chars().count()),
        HirKind::Class(_) => Some(1),
        HirKind::Repetition(r) => {
            let n = usize::try_from(r.max?).ok()?;
            max_chars(&r.sub)?.checked_mul(n)
        }
        HirKind::Capture(c) => max_chars(&c.sub),
        HirKind::Concat(xs) => xs
            .iter()
            .try_fold(0usize, |acc, x| acc.checked_add(max_chars(x)?)),
        HirKind::Alternation(xs) => xs
            .iter()
            .try_fold(0usize, |acc, x| Some(acc.max(max_chars(x)?))),
    }
}

/// 一处命中：在这一路正文里的字节区间（从这一路的开头算），和前后的字。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// 第几条规则（[`Answer`] 里的顺序）
    pub(crate) rule: usize,
    /// 在这一路正文里的位置，按收到的原文
    pub at: Range<usize>,
    /// 前后的字（原文，没打码）。一条规则在一路里只留前 [`PLACES_MAX`] 处
    pub place: Option<Place>,
}

impl Found {
    /// 第几条规则（[`Answer`] 里的顺序，[`Answer::rule_at`]）
    pub fn rule_index(&self) -> usize {
        self.rule
    }
}

/// 一处命中前后的字：前面最多 80 个字，命中的那一段最多 512 个字，后面最多 80 个字。
/// **是原文**：模型的回答里可能就有一把还原回来的密钥，报出去之前由调用方打码
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Place {
    pub before: String,
    pub matched: String,
    pub after: String,
}

/// 一路正文上的过滤。
///
/// 喂进来的是这一路一段一段的正文（[`Lane::push`]），交回现在可以发给客户端的那一段；
/// 一路结束时 [`Lane::finish`] 交回扣着的尾巴。**同一路的正文要按先后喂给同一个 `Lane`**：
/// 扣住的尾巴只能接回它自己那一路。
#[derive(Debug)]
pub struct Lane {
    answer: Arc<Answer>,
    acts: bool,
    /// 收到的原文：开头留着已经发出去的一小段（前文，正则的 `\b` 和命中前后的字要看它），
    /// 后面是还没发的
    buf: String,
    /// `buf[0]` 在这一路正文里是第几个字节
    base: usize,
    /// `buf` 里发到哪儿了
    sent: usize,
    /// 每条规则下一次从 `buf` 的哪儿接着找：之前的每个位置都已经认定了
    scan: Vec<usize>,
    /// 认定了、还没发出去的要改的地方：`buf` 里的区间和第几条规则
    edits: Vec<(Range<usize>, usize)>,
    /// 认定了的命中，还没被取走的
    found: Vec<Found>,
    /// `found` 里后面的字还没收齐的那几处：`(found 的下标, 下一个要接的字在这一路里的位置)`
    open: Vec<(usize, usize)>,
    /// 每条规则在这一路里留了几处的前后文
    placed: Vec<usize>,
    done: bool,
}

impl Lane {
    fn new(answer: Arc<Answer>, acts: bool) -> Self {
        let n = answer.rules.len();
        Lane {
            answer,
            acts,
            buf: String::new(),
            base: 0,
            sent: 0,
            scan: vec![0; n],
            edits: Vec::new(),
            found: Vec::new(),
            open: Vec::new(),
            placed: vec![0; n],
            done: false,
        }
    }

    /// 会不会改字：第三档，而且有删除、替换规则
    fn edits(&self) -> bool {
        self.acts && self.answer.edits
    }

    /// 喂一段，交回现在可以发出去的。观察档原样交回
    pub fn push(&mut self, text: &str) -> String {
        if self.done {
            return text.to_string();
        }
        self.buf.push_str(text);
        self.step(false)
    }

    /// 这一路结束了：扣着的查完，交回剩下的。之后再喂的原样交回
    pub fn finish(&mut self) -> String {
        if self.done {
            return String::new();
        }
        let out = self.step(true);
        self.done = true;
        out
    }

    /// 一整段一次处理完：整份的回答、管理界面上的「按回答测试」
    pub fn whole(&mut self, text: &str) -> String {
        let mut out = self.push(text);
        out.push_str(&self.finish());
        out
    }

    /// 认定了的命中，取走。后面的字还没收齐的那几处，后面的字就到这里为止
    pub fn take_found(&mut self) -> Vec<Found> {
        self.open.clear();
        std::mem::take(&mut self.found)
    }

    /// 这一处是哪条规则命中的
    pub fn rule_of(&self, f: &Found) -> Option<RuleRef<'_>> {
        self.answer.rule(f)
    }

    fn step(&mut self, last: bool) -> String {
        let len = self.buf.len();
        // 从这儿起的字还可能是一处没收齐的命中的一部分：最后 `hold` 个字。会改字的时候发到的
        // 地方不会越过它（见下面的 `upto`）
        let cut = if last {
            len
        } else {
            back_chars(&self.buf, len, self.answer.hold)
        };
        for ri in 0..self.answer.rules.len() {
            self.look(ri, cut, last);
        }
        // 发到哪儿：会改字的话，跨在 `cut` 上的那一段改动整个留到下一次；不改字的话收到什么
        // 发什么
        let edits = self.edits();
        let mut upto = if edits { cut } else { len };
        let merged = merge_edits(self.edits.clone(), |ri| {
            self.answer.rules.get(ri).and_then(AnswerRule::edit_with)
        });
        if let Some((r, _)) = merged.iter().find(|(r, _)| r.start < upto && r.end > upto) {
            upto = r.start;
        }
        let mine: Vec<(Range<usize>, &str)> = merged
            .iter()
            .filter(|(r, _)| r.end <= upto)
            .map(|(r, w)| (r.start - self.sent..r.end - self.sent, *w))
            .collect();
        let out = match self.buf.get(self.sent..upto) {
            Some(piece) => apply_edits(piece, &mine),
            None => String::new(),
        };
        self.edits.retain(|(r, _)| r.start >= upto);
        self.sent = upto.max(self.sent);
        self.fill(last);
        self.trim();
        out
    }

    /// 第 `ri` 条规则接着找，认定能认定的
    fn look(&mut self, ri: usize, cut: usize, last: bool) {
        let answer = self.answer.clone();
        let Some(rule) = answer.rules.get(ri) else {
            return;
        };
        let len = self.buf.len();
        let mut from = self.scan[ri];
        while from <= len {
            let Some(m) = rule.re.find_at(&self.buf, from) else {
                from = if last { len + 1 } else { from.max(cut) };
                break;
            };
            // 从 `cut` 起的命中，后面的字还没到齐：下一次再认
            if !last && m.start() >= cut {
                from = from.max(cut);
                break;
            }
            // 空的匹配不是命中（`x*` 在哪儿都「匹配」）：往后挪一个字接着找
            if m.is_empty() {
                from = next_char(&self.buf, m.start());
                continue;
            }
            // 长度没有上限的正则碰到了收到的末尾：它可能还在长，这一处放弃，开头照常发
            // （模块说明里那条明说的边界）
            if !last && !rule.bounded && m.end() == len {
                from = from.max(cut);
                break;
            }
            let at = m.range();
            from = at.end;
            self.commit(ri, at);
        }
        self.scan[ri] = from;
    }

    fn commit(&mut self, ri: usize, at: Range<usize>) {
        let edits = self.answer.rules.get(ri).is_some_and(|r| r.action.edits());
        if self.edits() && edits {
            self.edits.push((at.clone(), ri));
        }
        let place = (self.placed[ri] < PLACES_MAX).then(|| {
            self.placed[ri] += 1;
            let lead = back_chars(&self.buf, at.start, CONTEXT);
            Place {
                before: self.buf.get(lead..at.start).unwrap_or_default().to_string(),
                matched: self
                    .buf
                    .get(at.clone())
                    .unwrap_or_default()
                    .chars()
                    .take(MATCHED_KEEP)
                    .collect(),
                after: String::new(),
            }
        });
        if place.is_some() {
            self.open.push((self.found.len(), self.base + at.end));
        }
        self.found.push(Found {
            rule: ri,
            at: self.base + at.start..self.base + at.end,
            place,
        });
    }

    /// 后面的字还没收齐的那几处，接上收到的。够了 80 个字、或者这一路结束了就不再接
    fn fill(&mut self, last: bool) {
        let (buf, base) = (&self.buf, self.base);
        let found = &mut self.found;
        self.open.retain_mut(|(k, next)| {
            let Some(p) = found.get_mut(*k).and_then(|f| f.place.as_mut()) else {
                return false;
            };
            let have = p.after.chars().count();
            let from = next.saturating_sub(base);
            let more: String = buf
                .get(from..)
                .unwrap_or_default()
                .chars()
                .take(CONTEXT.saturating_sub(have))
                .collect();
            *next += more.len();
            p.after.push_str(&more);
            !last && p.after.chars().count() < CONTEXT
        });
    }

    /// 发出去、找过了的字丢掉，只留前面一小段当前文
    fn trim(&mut self) {
        let low = self
            .scan
            .iter()
            .copied()
            .chain(self.edits.iter().map(|(r, _)| r.start))
            .fold(self.sent, usize::min)
            .min(self.buf.len());
        // 还没收齐后面的字的那几处，要的字都在 `low` 之后（它们的命中已经认定，`scan` 越过了它）
        let drop = back_chars(&self.buf, low, CONTEXT);
        if drop == 0 {
            return;
        }
        self.buf.drain(..drop);
        self.base += drop;
        self.sent -= drop;
        for s in &mut self.scan {
            *s = s.saturating_sub(drop);
        }
        for (r, _) in &mut self.edits {
            *r = r.start - drop..r.end - drop;
        }
    }
}

/// 一条查回答的规则，给报命中的调用方看。
#[derive(Debug, Clone, Copy)]
pub struct RuleRef<'a>(&'a AnswerRule);

impl RuleRef<'_> {
    /// 内置规则的 id，或者自定义规则的名字
    pub fn id(&self) -> &str {
        &self.0.id
    }
    pub fn name(&self) -> &str {
        &self.0.name
    }
    pub fn custom(&self) -> bool {
        self.0.custom
    }
    /// 写的样子，原样
    pub fn pattern(&self) -> &str {
        &self.0.pattern
    }
    pub fn matching(&self) -> Match {
        self.0.matching
    }
    pub fn case_sensitive(&self) -> bool {
        self.0.case_sensitive
    }
    /// 第三档下做什么
    pub fn action(&self) -> Action {
        self.0.action
    }
    pub fn replacement(&self) -> &str {
        &self.0.replacement
    }
}

/// `end` 往前数 `n` 个字的那个位置（不够就是 0）。`end` 要在字符边界上
fn back_chars(s: &str, end: usize, n: usize) -> usize {
    if n == 0 {
        return end.min(s.len());
    }
    s.get(..end)
        .and_then(|head| head.char_indices().rev().nth(n - 1))
        .map_or(0, |(i, _)| i)
}

/// `i` 之后的下一个字符边界。在末尾的话越过末尾一个字节（找的循环就此停下）
fn next_char(s: &str, i: usize) -> usize {
    match s.get(i..).and_then(|rest| rest.chars().next()) {
        Some(c) => i + c.len_utf8(),
        None => s.len() + 1,
    }
}

#[cfg(test)]
mod tests;
