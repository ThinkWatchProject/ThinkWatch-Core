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
//! # 删过的地方再查，直到删干净
//!
//! 删掉一处之后，前后的字挨上了，可能又接成一处：`sesecretcret` 删掉中间的 `secret`，剩下的
//! 又是 `secret`。和请求那边一样，**删到不再冒出新命中为止**：删过、换过的字交给下一遍
//! （`Pass`），下一遍只在改过的地方（接缝）附近再找 —— 一处命中要用到的字里没有接缝，它在
//! 上一遍就在，上一遍认得出就已经改掉了。替换上去的字是规则写的，不再查。第一遍之后又过了
//! [`MAX_ROUNDS`] 遍还在改字的只能是刻意一层套一层写出来的：这一路剩下的整个不发（请求那边
//! 是那一段整段删掉）。
//!
//! 接出来的命中起点最远在接缝前 `reach` 个字（会改字的规则要扣的取大的）。所以会改字的时候，
//! 最后一遍发给客户端的再多扣 `reach` 个字：以后哪儿删了字，接缝前面这些字还在手里。
//!
//! **整份一次处理的（[`Lane::whole`]）一定删干净。流式的，同一段正文不管被切成多少块、切在
//! 哪个字中间，删出来、换出来的和整份的一模一样**，除了两种：比 [`REGEX_HOLD_MAX`] 长的正则
//! 命中；套得比多扣的那 `reach` 个字还深、接出来的命中开头已经发出去了的 —— 发了的收不回来，
//! 没发的那一截删掉，客户端收不到一整处。要看前后一个字的正则（`\b`、`$`）整个落在已经发出
//! 去的字里、只因为后面的字删了才成了命中的，就留在客户端那里了。
//!
//! # 观察档不扣
//!
//! 观察档（或者查回答的规则全是「仅记录」）不改任何字，也就不用扣：收到什么马上发什么，只是
//! 在旁边照样找、照样记。「仅记录」的规则只看模型的原文，不看删过之后接出来的。
//!
//! # 按格式、按路
//!
//! 一条流里怎么拆出每一路正文、一路结束时扣着的尾巴怎么补成一帧、`output_text.done` 这类
//! 带着整段正文的帧怎么改成和发出去的一致，在 [`frames`]。

use std::ops::Range;
use std::sync::Arc;

use super::{Action, MAX_ROUNDS, Match, Rules, apply_edits, merge_edits};

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
    /// 删过字的地方（接缝）前面要多扣几个字：接缝前后的字可能接成一处新命中，起点最远在接缝
    /// 前这么多个字（会改字的规则要扣的取大的）。改动不会留下接缝的（都是换成非空的替换
    /// 文本、又没有要看前后字的正则）是 0
    reach: usize,
    /// 会改字的规则里有要看前后一个字的（`\b`、`$`）：替换文本两头也是接缝
    looks: bool,
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
    /// 要看命中前后的一个字（`\b`、`$` 这类断言）
    looks: bool,
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
        let editing = || out.iter().filter(|r| r.action.edits());
        let looks = editing().any(|r| r.looks);
        // 删掉字（删除，或者换成空的）才会让前后的字挨上；换成非空的替换文本，只有要看前后
        // 一个字的正则会因为它变
        let seams = looks || editing().any(|r| r.edit_with().is_none_or(str::is_empty));
        Some(Arc::new(Answer {
            hold: out.iter().map(|r| r.hold).max().unwrap_or(0),
            edits: editing().next().is_some(),
            reach: if seams {
                editing().map(|r| r.hold).max().unwrap_or(0)
            } else {
                0
            },
            looks,
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
        let (hold, bounded, looks) = if literal {
            (r.pattern.chars().count().saturating_sub(1), true, false)
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
            looks,
        })
    }

    /// 命中的字换成什么：替换是替换文本，删除是 `None`（删掉），仅记录不改
    fn edit_with(&self) -> Option<&str> {
        (self.action == Action::Replace).then_some(self.replacement.as_str())
    }
}

/// 一条正则要扣几个字、一处命中的长度有没有上限、用没用到要看前后一个字的断言：最长能匹配
/// 几个字，用到 `\b`、`$` 这类断言时多扣一个。算不出、超过上限的扣 [`REGEX_HOLD_MAX`]。
///
/// 也是一处命中最多用到哪些字：从起点往后 `hold + 1` 个（断言要看起点前面那一个）
fn regex_hold(source: &str, ignore_case: bool) -> (usize, bool, bool) {
    let parsed = regex_syntax::ParserBuilder::new()
        .case_insensitive(ignore_case)
        .build()
        .parse(source);
    let Ok(hir) = parsed else {
        return (REGEX_HOLD_MAX, false, true);
    };
    let looks = !hir.properties().look_set().is_empty();
    let Some(max) = max_chars(&hir) else {
        return (REGEX_HOLD_MAX, false, looks);
    };
    let hold = if looks { max } else { max.saturating_sub(1) };
    if hold > REGEX_HOLD_MAX {
        (REGEX_HOLD_MAX, false, looks)
    } else {
        (hold, true, looks)
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

/// 一路正文里认定了的命中（[`Lane::take`]）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Matches {
    /// 留了前后文的那几处，按先后
    pub found: Vec<Found>,
    /// 每条规则一共几处，按 [`Answer`] 里的顺序（[`Answer::rule_at`]）
    pub counts: Vec<usize>,
}

impl Matches {
    /// 一共几处
    pub fn total(&self) -> usize {
        self.counts.iter().sum()
    }
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
///
/// 里面是一遍接一遍（[`Pass`]）：第一遍在模型的原文上找，删、换过的字交给下一遍，下一遍只在
/// 改过的地方（接缝）附近再找一次，直到不再冒出新命中。下一遍要到第一遍改了字才有。
#[derive(Debug)]
pub struct Lane {
    answer: Arc<Answer>,
    acts: bool,
    passes: Vec<Pass>,
    tally: Tally,
    /// 最近发给客户端的一小段（新起一遍时当它的前文：`\b` 要看前一个字）
    told: String,
    /// 第一遍之后又过了 [`MAX_ROUNDS`] 遍还在改字：这一路剩下的整个不发
    silent: bool,
    done: bool,
}

/// 一路里认定了的命中，各遍共用
#[derive(Debug)]
struct Tally {
    /// 认定了的命中里留了前后文的那几处（每条规则在这一路里最多 [`PLACES_MAX`] 处），还没被
    /// 取走的。**再多的只数个数**（`counts`）：一段回答里同一个词出现几万次，不该攒几万条
    found: Vec<Found>,
    /// 每条规则认定了几处，还没被取走的
    counts: Vec<usize>,
    /// 每一处都留下位置（管理界面的「按回答测试」：样本有多长就标多少处）
    keep_all: bool,
    /// 每条规则在这一路里留了几处的前后文
    placed: Vec<usize>,
    /// 有一处接出来的命中，开头已经发给客户端了（套得比扣住的字还深）：没发的那一截删掉了，
    /// 和整份处理出来的不一样
    short: bool,
}

/// 一遍。第一遍（`round == 0`）收的是模型的原文，后面每一遍收的是上一遍改过、交出来的字
/// （[`Piece`]）。
///
/// 一遍里找、认定、扣住尾巴的办法都一样：认定了的命中先记着（`edits`），发到它后面时才改。
/// 后面几遍只看接缝附近：一处命中要用到的字里没有接缝，它在上一遍就已经在了 —— 上一遍认得
/// 出就已经改掉了，认不出（和替换文本重叠）这一遍也一样认不出。
#[derive(Debug)]
struct Pass {
    /// 第几遍，从 0 起
    round: usize,
    /// 收到的字：开头留着已经交出去的一小段（前文，正则的 `\b` 和命中前后的字要看它），
    /// 后面是还没交的
    buf: String,
    /// `buf[0]` 在这一遍收到的字里是第几个字节
    base: usize,
    /// `buf` 里交到哪儿了
    sent: usize,
    /// 每条规则下一次从 `buf` 的哪儿接着找：之前的每个位置都已经认定了
    scan: Vec<usize>,
    /// 认定了、还没交出去的要改的地方：`buf` 里的区间和第几条规则
    edits: Vec<(Range<usize>, usize)>,
    /// `found` 里后面的字还没收齐的那几处：`(found 的下标, 下一个要接的字在这一遍收到的字里
    /// 的位置)`
    open: Vec<(usize, usize)>,
    /// 后面几遍：`buf` 里前几遍换上去的替换文本，按先后。和它们重叠的不算命中（替换文本是
    /// 规则写的，不是模型的话）
    kept: Vec<Range<usize>>,
    /// 后面几遍：`buf` 里上一遍改过的地方（接缝），按先后
    seams: Vec<usize>,
    /// 后面几遍：`buf` 的每个位置（含末尾）在这一路原文里的位置，当起点用的和当终点用的
    /// （见 [`Piece::map`]）
    map: Vec<(usize, usize)>,
}

/// 一遍交给下一遍的一段字。
#[derive(Debug, Default)]
struct Piece {
    text: String,
    /// `text` 的每个位置（含末尾）在这一路原文里的位置：`(当起点, 当终点)`。落在删掉的地方
    /// 的，当起点换到删掉的那段后面、当终点换到前面；落在替换文本里的，当起点是被换掉的那段
    /// 的开头、当终点是它的结尾。只在要交给下一遍时才算
    map: Vec<(usize, usize)>,
    /// `text` 里所有换上去的替换文本（各遍的），按先后
    kept: Vec<Range<usize>>,
    /// 这一遍改过的地方里，可能接出新命中的那几个接缝，按先后
    seams: Vec<usize>,
}

impl Lane {
    fn new(answer: Arc<Answer>, acts: bool) -> Self {
        let n = answer.rules.len();
        Lane {
            passes: vec![Pass::new(0, n)],
            tally: Tally {
                found: Vec::new(),
                counts: vec![0; n],
                keep_all: false,
                placed: vec![0; n],
                short: false,
            },
            answer,
            acts,
            told: String::new(),
            silent: false,
            done: false,
        }
    }

    /// 每一处都留下位置，不只前 [`PLACES_MAX`] 处。只给样本有限的「测试」用
    pub fn keeping_all(mut self) -> Self {
        self.tally.keep_all = true;
        self
    }

    /// 喂一段，交回现在可以发出去的。观察档原样交回
    pub fn push(&mut self, text: &str) -> String {
        if self.done {
            return text.to_string();
        }
        if let Some(first) = self.passes.first_mut() {
            first.buf.push_str(text);
        }
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

    /// 一整段一次处理完：整份的回答、管理界面上的「按回答测试」。全在手里，一个字都不用先发，
    /// 套多少层都删干净（到 [`MAX_ROUNDS`] 为止）
    pub fn whole(&mut self, text: &str) -> String {
        if self.done {
            return text.to_string();
        }
        if let Some(first) = self.passes.first_mut() {
            first.buf.push_str(text);
        }
        let out = self.step(true);
        self.done = true;
        out
    }

    /// 认定了的命中，取走：留了前后文的那几处，和每条规则一共几处。后面的字还没收齐的那几处，
    /// 后面的字就到这里为止
    pub fn take(&mut self) -> Matches {
        for p in &mut self.passes {
            p.open.clear();
        }
        let n = self.tally.counts.len();
        Matches {
            found: std::mem::take(&mut self.tally.found),
            counts: std::mem::replace(&mut self.tally.counts, vec![0; n]),
        }
    }

    /// 这一处是哪条规则命中的
    pub fn rule_of(&self, f: &Found) -> Option<RuleRef<'_>> {
        self.answer.rule(f)
    }

    /// 一遍一遍地过，交回最后一遍交出来的
    fn step(&mut self, last: bool) -> String {
        if self.silent {
            return String::new();
        }
        let answer = self.answer.clone();
        let edits = self.acts && answer.edits;
        let mut piece: Option<Piece> = None;
        let mut k = 0;
        loop {
            if k == self.passes.len() {
                let p = piece.unwrap_or_default();
                if p.seams.is_empty() {
                    if edits && answer.reach > 0 {
                        self.told.push_str(&p.text);
                        let drop = back_chars(&self.told, self.told.len(), CONTEXT);
                        self.told.drain(..drop);
                    }
                    return p.text;
                }
                // 接缝处还可能接出新命中：再过一遍。套了这么多层的只能是刻意写出来的，这一路
                // 剩下的整个不发（和请求那边整段删掉一样）
                if k > MAX_ROUNDS {
                    self.silent = true;
                    return String::new();
                }
                self.passes
                    .push(Pass::after(k, answer.rules.len(), &self.told, p));
            } else if let (Some(p), Some(pass)) = (piece.take(), self.passes.get_mut(k)) {
                pass.feed(p);
            }
            let tail = k + 1 == self.passes.len();
            let Some(pass) = self.passes.get_mut(k) else {
                return String::new();
            };
            piece = Some(pass.step(&answer, &mut self.tally, edits, last, tail));
            k += 1;
        }
    }
}

impl Pass {
    fn new(round: usize, rules: usize) -> Self {
        Pass {
            round,
            buf: String::new(),
            base: 0,
            sent: 0,
            scan: vec![0; rules],
            edits: Vec::new(),
            open: Vec::new(),
            kept: Vec::new(),
            seams: Vec::new(),
            map: Vec::new(),
        }
    }

    /// 新起的一遍，收上一遍交来的第一段。前文是最近发给客户端的那一小段：已经发了、改不了，
    /// 给 `\b` 和命中前面的字看，也看接出来的命中是不是从那里头起的（[`Pass::recheck`]）
    fn after(round: usize, rules: usize, told: &str, piece: Piece) -> Self {
        let mut p = Pass::new(round, rules);
        p.buf.push_str(told);
        p.sent = told.len();
        // 前文的位置没有对照：都按这一段的开头算
        let first = piece.map.first().map_or((0, 0), |&(_, e)| (e, e));
        p.map = vec![first; told.len() + 1];
        p.feed(piece);
        p
    }

    /// 上一遍交来的一段接在后面
    fn feed(&mut self, piece: Piece) {
        let off = self.buf.len();
        self.buf.push_str(&piece.text);
        self.map.pop();
        self.map.extend(piece.map);
        self.kept
            .extend(piece.kept.into_iter().map(|r| r.start + off..r.end + off));
        self.seams.extend(piece.seams.into_iter().map(|j| j + off));
    }

    /// `buf` 里一个位置在这一路原文里的位置：`(当起点, 当终点)`
    fn at(&self, i: usize) -> (usize, usize) {
        if self.round == 0 {
            (self.base + i, self.base + i)
        } else {
            self.map.get(i).copied().unwrap_or_default()
        }
    }

    /// 找、认定，交出现在能交的。`edits`：这一路会不会改字。`tail`：这是不是最后一遍（交出去
    /// 的就到客户端了）
    fn step(
        &mut self,
        answer: &Answer,
        tally: &mut Tally,
        edits: bool,
        last: bool,
        tail: bool,
    ) -> Piece {
        let len = self.buf.len();
        // 从这儿起的字还可能是一处没收齐的命中的一部分：最后 `hold` 个字。会改字的时候交到的
        // 地方不会越过它（见下面的 `upto`）
        let hold = if self.round == 0 {
            answer.hold
        } else {
            answer.reach
        };
        let cut = if last {
            len
        } else {
            back_chars(&self.buf, len, hold)
        };
        for ri in 0..answer.rules.len() {
            if self.round == 0 {
                self.look(answer, tally, ri, cut, last, edits);
            } else {
                self.recheck(answer, tally, ri, cut, last);
            }
        }
        // 交到哪儿：会改字的话，跨在 `cut` 上的那一段改动整个留到下一次；不改字的话收到什么
        // 交什么
        let mut upto = if edits { cut } else { len };
        let merged = merge_edits(self.edits.clone(), |ri| {
            answer.rules.get(ri).and_then(AnswerRule::edit_with)
        });
        let straddle = |at: usize| {
            merged
                .iter()
                .find(|(r, _)| r.start < at && r.end > at)
                .map_or(at, |(r, _)| r.start)
        };
        upto = straddle(upto).max(self.sent);
        let live = |w: &str| answer.reach > 0 && (w.is_empty() || answer.looks);
        let seamed = merged
            .iter()
            .any(|(r, w)| r.start >= self.sent && r.end <= upto && live(w));
        // 最后一遍交给客户端的，还要离以后的改动至少 `reach` 个字：以后在 `upto` 之后删了字，
        // 接缝前面这些字可能和后面的接成一处命中，得还在手里。这一次就有接缝的，整段交给下一遍
        if tail && !last && edits && answer.reach > 0 && !seamed {
            upto = straddle(back_chars(&self.buf, upto, answer.reach)).max(self.sent);
        }
        let mine: Vec<(Range<usize>, &str)> = merged
            .iter()
            .filter(|(r, _)| r.start >= self.sent && r.end <= upto)
            .map(|(r, w)| (r.start - self.sent..r.end - self.sent, *w))
            .collect();
        let text = match self.buf.get(self.sent..upto) {
            Some(piece) => apply_edits(piece, &mine),
            None => String::new(),
        };
        let out = if !tail || mine.iter().any(|(_, w)| live(w)) {
            self.piece(text, &mine, upto, &live)
        } else {
            Piece {
                text,
                ..Piece::default()
            }
        };
        self.edits.retain(|(r, _)| r.start >= upto);
        self.sent = upto;
        self.fill(tally, last);
        self.trim();
        out
    }

    /// 交出去的这一段（`buf[sent..upto]` 照 `mine` 改过的 `text`）配上对照、替换文本、接缝
    fn piece(
        &self,
        text: String,
        mine: &[(Range<usize>, &str)],
        upto: usize,
        live: &dyn Fn(&str) -> bool,
    ) -> Piece {
        let at = |i: usize| self.at(self.sent + i);
        let n = upto.saturating_sub(self.sent);
        let mut map = Vec::with_capacity(text.len() + 1);
        let mut kept = Vec::new();
        let mut seams = Vec::new();
        // 前几遍的替换文本（和这一遍的改动不重叠），换到交出去的位置
        let old: Vec<Range<usize>> = self
            .kept
            .iter()
            .filter(|k| k.end > self.sent && k.start < upto)
            .map(|k| k.start.max(self.sent) - self.sent..k.end.min(upto) - self.sent)
            .collect();
        let mut old = old.into_iter().peekable();
        let mut delta: isize = 0;
        let moved = |r: Range<usize>, d: isize| {
            r.start.saturating_add_signed(d)..r.end.saturating_add_signed(d)
        };
        // 删掉的地方：交出去的那个位置当终点换到删掉的前面
        let mut end_at: Option<usize> = None;
        let mut pos = 0;
        for (r, w) in mine {
            while let Some(k) = old.next_if(|k| k.end <= r.start) {
                kept.push(moved(k, delta));
            }
            for i in pos..r.start {
                let (s, e) = at(i);
                map.push((s, end_at.take().unwrap_or(e)));
            }
            let o = map.len();
            let ((sa, ea), (_, eb)) = (at(r.start), at(r.end));
            if w.is_empty() {
                end_at = Some(end_at.unwrap_or(ea));
            } else {
                map.push((sa, end_at.take().unwrap_or(ea)));
                map.extend(std::iter::repeat_n((sa, eb), w.len() - 1));
                kept.push(o..o + w.len());
            }
            if live(w) {
                seams.push(o);
                if !w.is_empty() {
                    seams.push(o + w.len());
                }
            }
            delta += w.len() as isize - r.len() as isize;
            pos = r.end;
        }
        kept.extend(old.map(|k| moved(k, delta)));
        for i in pos..=n {
            let (s, e) = at(i);
            map.push((s, end_at.take().unwrap_or(e)));
        }
        seams.dedup();
        Piece {
            text,
            map,
            kept,
            seams,
        }
    }

    /// 第一遍：第 `ri` 条规则接着找，认定能认定的
    fn look(
        &mut self,
        answer: &Answer,
        tally: &mut Tally,
        ri: usize,
        cut: usize,
        last: bool,
        edits: bool,
    ) {
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
            let edit = (edits && rule.action.edits()).then(|| at.clone());
            self.commit(tally, ri, at, edit);
        }
        self.scan[ri] = from;
    }

    /// 后面几遍：第 `ri` 条规则在接缝附近每个位置上都试一次。一处命中要用到的字从它的起点
    /// 往后最多 `hold + 1` 个（[`regex_hold`]），所以只有起点在接缝前 `hold` 个字以内的才可能
    /// 是新冒出来的。只看删除、替换规则
    fn recheck(&mut self, answer: &Answer, tally: &mut Tally, ri: usize, cut: usize, last: bool) {
        let len = self.buf.len();
        let limit = if last { len + 1 } else { cut };
        let Some(rule) = answer.rules.get(ri).filter(|r| r.action.edits()) else {
            self.scan[ri] = cut;
            return;
        };
        let mut p = self.scan[ri];
        let mut k = self.seams.partition_point(|&j| j < p);
        while let Some(&j) = self.seams.get(k) {
            let mut q = back_chars(&self.buf, j, rule.hold).max(p);
            while q <= j && q < limit {
                match rule.re.find_at(&self.buf, q) {
                    Some(m) if m.start() <= j && m.start() < limit => {
                        q = m.start();
                        let whole = last || rule.bounded || m.end() < len;
                        if !m.is_empty() && whole && !super::overlaps(&self.kept, &m.range()) {
                            // 开头已经发出去了（只有新起的一遍、前文里才有）：发了的收不回来，
                            // 没发的那一截删掉，客户端收不到一整处
                            let edit =
                                (m.end() > self.sent).then(|| m.start().max(self.sent)..m.end());
                            if m.start() < self.sent {
                                tally.short = true;
                            }
                            self.commit(tally, ri, m.range(), edit);
                        }
                        q = next_char(&self.buf, q);
                    }
                    _ => {
                        q = if last {
                            next_char(&self.buf, j)
                        } else {
                            next_char(&self.buf, j).min(limit)
                        };
                        break;
                    }
                }
            }
            p = p.max(q);
            if p >= limit {
                break;
            }
            k += 1;
        }
        self.scan[ri] = if last { len } else { p.max(cut) };
    }

    /// 认定一处命中：`at` 是命中的地方，`edit` 是要改的地方（不改字是 `None`）
    fn commit(
        &mut self,
        tally: &mut Tally,
        ri: usize,
        at: Range<usize>,
        edit: Option<Range<usize>>,
    ) {
        if let Some(e) = edit {
            self.edits.push((e, ri));
        }
        let place = (tally.placed[ri] < PLACES_MAX).then(|| {
            tally.placed[ri] += 1;
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
        if let Some(c) = tally.counts.get_mut(ri) {
            *c += 1;
        }
        if place.is_none() && !tally.keep_all {
            return;
        }
        if place.is_some() {
            self.open.push((tally.found.len(), self.base + at.end));
        }
        let (start, end) = (self.at(at.start).0, self.at(at.end).1);
        tally.found.push(Found {
            rule: ri,
            at: start.min(end)..end.max(start),
            place,
        });
    }

    /// 后面的字还没收齐的那几处，接上收到的。够了 80 个字、或者这一路结束了就不再接
    fn fill(&mut self, tally: &mut Tally, last: bool) {
        let (buf, base) = (&self.buf, self.base);
        let found = &mut tally.found;
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

    /// 交出去、找过了的字丢掉，只留前面一小段当前文
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
        if self.round > 0 {
            self.map.drain(..drop.min(self.map.len()));
            self.kept.retain(|k| k.end > drop);
            for k in &mut self.kept {
                *k = k.start.saturating_sub(drop)..k.end - drop;
            }
            self.seams.retain(|&j| j >= drop);
            for j in &mut self.seams {
                *j -= drop;
            }
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
