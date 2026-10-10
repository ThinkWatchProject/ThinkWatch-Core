//! 内容过滤：调用方发来的正文里出现了某个词、某种写法，或者某些字符。
//!
//! **引擎在这里，规则从哪来、命中之后怎么记由各自决定。**企业版的规则存在系统设置里，
//! 桌面版的写在 `config.yaml`，形状都是 [`crate::policy::ContentPolicy`]；两边共用的是
//! 「怎么匹配、查哪些正文、命中了怎么说、怎么删」，和一份出厂的规则（[`builtins`]，
//! `data/content.yaml`）。
//!
//! # 查哪些正文
//!
//! 调用方的消息，**连同其中的工具结果** —— 被注入的指令最常待的地方正是那里：
//! 一个工具抓回来的网页、读到的文件。系统提示（配置网关的人写的）和模型自己说的
//! 话不查。请求上的入口是 [`screen`]，它在原文上找这些正文（[`tw_dialect::caller`]），
//! 删也删在原文上。
//!
//! # 三种匹配
//!
//! - `contains`：不分大小写的子串。写起来最省事，也最不容易写错；
//! - `regex`：不分大小写的正则，**编译后的大小有上限** —— 这条正则要在每个请求上
//!   跑，一条病态写法不该拖慢所有请求；
//! - `codepoints`：按字符本身认（[`Codepoints`]）。看不见的字符写不成关键词，出厂的
//!   「隐藏字符」一组（标签字符、双向控制符、零宽字符、私用区）就是这一种。
//!
//! # 三种处置
//!
//! 按严重程度排：`block`（拒绝，请求不发出去）> `strip`（删掉命中的字再发）>
//! `record`（照发，记一条）。一个请求命中几条时，有拒绝就拒绝；每条命中都报出来，
//! 记不记、记在哪由调用方定。

use std::ops::Range;

mod codepoints;
mod screen;

pub(crate) use codepoints::visible as codepoints_visible;
pub use codepoints::{CodepointError, Codepoints, MAX_ITEMS as MAX_CODEPOINT_ITEMS};
pub(crate) use screen::evaluate;
pub use screen::{
    Outcome, RulePlaces, ScreenHit, Screening, places, places_text, screen, screen_text,
    screen_value,
};

/// 一条规则怎么认。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Match {
    /// 不分大小写的子串
    #[default]
    Contains,
    /// 不分大小写的正则
    Regex,
    /// 按字符本身认：`U+200B`、`U+E0000–U+E007F`
    Codepoints,
}

impl Match {
    pub fn slug(&self) -> &'static str {
        match self {
            Match::Contains => "contains",
            Match::Regex => "regex",
            Match::Codepoints => "codepoints",
        }
    }
    pub fn from_slug(s: &str) -> Option<Self> {
        match s {
            "contains" => Some(Match::Contains),
            "regex" => Some(Match::Regex),
            "codepoints" => Some(Match::Codepoints),
            _ => None,
        }
    }
}

/// 命中之后做什么。**按严重程度排序**：`Record < Strip < Block`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// 照发，记一条
    Record,
    /// 把命中的字删掉再发
    Strip,
    /// 不发出去
    Block,
}

impl Action {
    pub fn slug(&self) -> &'static str {
        match self {
            Action::Record => "record",
            Action::Strip => "strip",
            Action::Block => "block",
        }
    }
    pub fn from_slug(s: &str) -> Option<Self> {
        match s {
            "record" => Some(Action::Record),
            "strip" => Some(Action::Strip),
            "block" => Some(Action::Block),
            _ => None,
        }
    }
}

/// 一条出厂的规则（`data/content.yaml`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Builtin {
    /// 稳定的标识。配置按它引用，**改了就是另一条规则**
    pub id: String,
    /// 英文名。界面按 id 查自己的名称表，查不到才用它
    pub name: String,
    /// 码位规则写的是码位（`U+E0000–U+E007F`）
    pub pattern: String,
    #[serde(rename = "match")]
    pub matching: Match,
    /// 出厂的处置
    pub action: Action,
    /// 哪一组：`invisible`（隐藏字符）/ `injection`（覆盖指令的说法）/ `persona`（改换
    /// 身份、套提示词）/ `chinese`（中文的说法）
    pub group: String,
    /// 出厂时开不开。**只开误报极少的那几条**
    #[serde(default)]
    pub on_by_default: bool,
    /// 为什么值得看一眼（英文，界面按 id 查译文）。规则名说不清的才写
    #[serde(default)]
    pub why: String,
}

/// 出厂规则的原文。
pub const BUILTIN: &str = include_str!("../data/content.yaml");

/// 出厂的规则，按界面上的顺序。
pub fn builtins() -> &'static [Builtin] {
    static ALL: std::sync::OnceLock<Vec<Builtin>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        // 随二进制一起编进来的文件，读不了是构建出了错 —— 测试会先挂
        serde_yaml_ng::from_str(BUILTIN).expect("data/content.yaml is valid")
    })
}

/// 按 id 找一条出厂规则。
pub fn builtin(id: &str) -> Option<&'static Builtin> {
    builtins().iter().find(|b| b.id == id)
}

/// 编一条规则要的东西。
#[derive(Debug, Clone, Copy)]
pub struct RuleInput<'a> {
    /// 出厂规则的 id，或者自定义规则的名字
    pub id: &'a str,
    pub name: &'a str,
    pub custom: bool,
    pub pattern: &'a str,
    pub matching: Match,
    pub action: Action,
}

impl<'a> From<&'a Builtin> for RuleInput<'a> {
    fn from(b: &'a Builtin) -> Self {
        RuleInput {
            id: &b.id,
            name: &b.name,
            custom: false,
            pattern: &b.pattern,
            matching: b.matching,
            action: b.action,
        }
    }
}

/// 一条编好的规则。
#[derive(Debug, Clone)]
pub struct Rule {
    pub id: String,
    pub name: String,
    pub custom: bool,
    pub pattern: String,
    pub matching: Match,
    pub action: Action,
    /// `contains` 用：小写过的样子
    lower: String,
    re: Option<regex::Regex>,
    points: Option<Codepoints>,
}

/// 一条规则编不起来。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("content rule `{rule}`: {detail}")]
pub struct BadRule {
    pub rule: String,
    pub detail: String,
}

impl Rule {
    pub fn new(r: RuleInput<'_>) -> Result<Rule, BadRule> {
        let bad = |detail: String| BadRule {
            rule: r.id.to_string(),
            detail,
        };
        let pattern = r.pattern.trim();
        if pattern.is_empty() {
            return Err(bad("the pattern is empty".into()));
        }
        let (re, points) = match r.matching {
            Match::Contains => (None, None),
            Match::Regex => (
                Some(crate::bounded(&format!("(?i:{pattern})")).map_err(|e| bad(e.to_string()))?),
                None,
            ),
            Match::Codepoints => (
                None,
                Some(Codepoints::parse(pattern).map_err(|e| bad(e.to_string()))?),
            ),
        };
        Ok(Rule {
            id: r.id.to_string(),
            name: r.name.to_string(),
            custom: r.custom,
            // `contains` 的首尾空格有意义（` dan ` 就靠它不误伤 `dance`），原样留着
            pattern: r.pattern.to_string(),
            matching: r.matching,
            action: r.action,
            lower: r.pattern.to_lowercase(),
            re,
            points,
        })
    }

    /// 码位规则的那一组码位
    pub fn codepoints(&self) -> Option<&Codepoints> {
        self.points.as_ref()
    }

    /// `text` 给人看的样子：码位规则认的字符画出来（`‹U+200B›`，连成一串的写成
    /// `‹U+E0049 ×12›`，和 [`Hit::snippet`] 一样），别的规则原样。`escapes`：JSON 的
    /// `\uXXXX` 写法也算它写的那个字符（[`screen_text`] 查的是原文）。
    pub fn draw(&self, text: &str, escapes: bool) -> String {
        let Some(p) = &self.points else {
            return text.to_string();
        };
        let mut out = String::with_capacity(text.len());
        let mut run: Option<(char, usize)> = None;
        let flush = |run: &mut Option<(char, usize)>, out: &mut String| match run.take() {
            Some((c, 1)) => out.push_str(&codepoints::visible(c)),
            Some((c, n)) => out.push_str(&format!("‹U+{:04X} ×{n}›", c as u32)),
            None => {}
        };
        for (_, c) in chars(text, escapes) {
            if p.contains(c) {
                match &mut run {
                    Some((_, n)) => *n += 1,
                    None => run = Some((c, 1)),
                }
                continue;
            }
            flush(&mut run, &mut out);
            out.push(c);
        }
        flush(&mut run, &mut out);
        out
    }

    /// 在 `text` 里的全部命中：按先后、互不重叠的字节区间，和一共几处（码位规则是
    /// 几个字符，连在一起的几个字符是一段区间）。`lower` 是 `text` 小写过的样子，
    /// 只有 `contains` 用；`escapes`：码位规则也认 JSON 的 `\uXXXX` 写法（见
    /// [`screen_text`]）。
    fn find_all(&self, text: &str, lower: &mut Lower<'_>, escapes: bool) -> Found {
        if let Some(p) = &self.points {
            return find_points(p, text, escapes);
        }
        if let Some(re) = &self.re {
            // 空的匹配不是命中：`x*` 这种写法在哪儿都「匹配」，删也删不掉什么
            let ranges: Vec<_> = re
                .find_iter(text)
                .filter(|m| !m.is_empty())
                .map(|m| m.range())
                .collect();
            return Found::of(ranges);
        }
        Found::of(find_contains(text, lower.get(), &self.lower))
    }
}

/// 一段正文小写过的样子，用到才算
struct Lower<'a> {
    text: &'a str,
    lower: Option<String>,
}

impl<'a> Lower<'a> {
    fn new(text: &'a str) -> Self {
        Lower { text, lower: None }
    }
    fn get(&mut self) -> &str {
        self.lower.get_or_insert_with(|| lowercase(self.text))
    }
}

/// 和 `text.to_lowercase()` 一样的结果，快一些：ASCII 的一段整段转，别的字符逐个转（标准库
/// 碰到头一个 ASCII 以外的字符之后就逐个字符地转了，夹着中文、`→` 的正文几乎整段都是）。
///
/// 按上下文转的只有 `Σ`（在词尾是 `ς`）：正文里有它就整段交给标准库。
fn lowercase(text: &str) -> String {
    if memchr::memmem::find(text.as_bytes(), "Σ".as_bytes()).is_some() {
        return text.to_lowercase();
    }
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        let start = i;
        i = ascii_end(b, i);
        if i > start {
            let at = out.len();
            out.push_str(&text[start..i]);
            out[at..].make_ascii_lowercase();
        }
        // 停在一个多字节字符的头一个字节上：字符边界
        if let Some(c) = text.get(i..).and_then(|rest| rest.chars().next()) {
            out.extend(c.to_lowercase());
            i += c.len_utf8();
        }
    }
    out
}

/// 从 `i` 起连着的 ASCII 字节到哪儿为止。八个一组地看
fn ascii_end(b: &[u8], mut i: usize) -> usize {
    const HIGH: u64 = 0x8080_8080_8080_8080;
    while let Some(word) = b.get(i..i + 8) {
        let word = u64::from_ne_bytes(word.try_into().unwrap_or_default());
        if word & HIGH != 0 {
            break;
        }
        i += 8;
    }
    while i < b.len() && b[i].is_ascii() {
        i += 1;
    }
    i
}

/// 一条规则在一段正文里的全部命中
#[derive(Debug, Default)]
struct Found {
    ranges: Vec<Range<usize>>,
    count: usize,
}

impl Found {
    fn of(ranges: Vec<Range<usize>>) -> Self {
        Found {
            count: ranges.len(),
            ranges,
        }
    }
}

/// 不分大小写的子串，全部出现的地方。
///
/// 小写之后长度没变时在小写串上找、下标对回原文；变了（少数字母会）就在原文上逐个
/// 字符地比，慢一点但对。
fn find_contains(text: &str, lower: &str, needle: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    if needle.is_empty() {
        return out;
    }
    if lower.len() == text.len() {
        // 按字节找（SIMD）：每段正文、每条规则都要从头找到尾。和 `match_indices` 一样是
        // 一个接一个、互不重叠的
        for at in memchr::memmem::find_iter(lower.as_bytes(), needle.as_bytes()) {
            // 区间落回原文要对齐到字符边界
            let start = floor(text, at);
            let end = ceil(text, at + needle.len());
            if out.last().is_none_or(|p: &Range<usize>| p.end <= start) {
                out.push(start..end);
            }
        }
        return out;
    }
    let mut from = 0;
    while from < text.len() {
        let Some(r) = find_folded(&text[from..], needle) else {
            break;
        };
        let r = from + r.start..from + r.end;
        from = r.end.max(r.start + 1);
        from = ceil(text, from);
        out.push(r);
    }
    out
}

/// 一段正文里每一个字符（按出现的先后）和它的字节区间。`escapes`：JSON 的 `\uXXXX`
/// 写法（连同代理对 `\udb40\udc49`）算作它写的那个字符，区间是整个转义序列；别的
/// 转义（`\\`、`\n`）照字面算两个字符 —— 这样 `\\u200b` 里的 `u200b` 不会被当成转义。
fn chars(text: &str, escapes: bool) -> impl Iterator<Item = (Range<usize>, char)> + '_ {
    let bytes = text.as_bytes();
    let mut i = 0;
    // 上一个字符是开始一个别的转义的反斜杠：这一个照字面算，哪怕它也是反斜杠
    let mut literal = false;
    std::iter::from_fn(move || {
        while i < text.len() {
            let at = i;
            if escapes && !literal && bytes[at] == b'\\' {
                if let Some((c, len)) = unescape(&text[at..]) {
                    i = at + len;
                    match c {
                        Some(c) => return Some((at..i, c)),
                        // 半个代理对不是字符，整个跳过
                        None => continue,
                    }
                }
                literal = true;
                i = at + 1;
                return Some((at..i, '\\'));
            }
            literal = false;
            let c = text[at..].chars().next()?;
            i = at + c.len_utf8();
            return Some((at..i, c));
        }
        None
    })
}

/// 读一个 `\uXXXX`（或者代理对写的两个）：那个字符和整个序列的长度。只有半个代理对的，
/// 字符是 `None`（它不是一个字符），序列照样跳过
fn unescape(s: &str) -> Option<(Option<char>, usize)> {
    let unit = |s: &str| -> Option<u32> {
        let hex = s.strip_prefix("\\u")?.get(..4)?;
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(hex, 16).ok()
    };
    let hi = unit(s)?;
    if (0xD800..0xDC00).contains(&hi)
        && let Some(lo) = s.get(6..).and_then(unit)
        && (0xDC00..0xE000).contains(&lo)
    {
        let c = char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00));
        return Some((c, 12));
    }
    Some((char::from_u32(hi), 6))
}

/// 码位规则在一段正文里的命中：连在一起的几个字符算一段，`count` 是字符数。
fn find_points(p: &Codepoints, text: &str, escapes: bool) -> Found {
    let mut out = Found::default();
    // 纯 ASCII 的正文里没有 ASCII 以外的字符；没写转义的话也不会藏着别的
    if p.min() > 0x7F && text.is_ascii() && (!escapes || !text.contains("\\u")) {
        return out;
    }
    let mut hit = |r: Range<usize>| {
        out.count += 1;
        match out.ranges.last_mut() {
            Some(last) if last.end == r.start => last.end = r.end,
            _ => out.ranges.push(r),
        }
    };
    // 码位都在 ASCII 以外、又不认转义：ASCII 的字节一个都命中不了，跳过去，只解码别的
    // 字符。夹着中文、`→` 的正文（工具读回来的代码）就不再逐个字符地解一遍
    if p.min() > 0x7F && !escapes {
        for (r, c) in non_ascii(text) {
            if p.contains(c) {
                hit(r);
            }
        }
    } else {
        for (r, c) in chars(text, escapes) {
            if p.contains(c) {
                hit(r);
            }
        }
    }
    out
}

/// 一段正文里 ASCII 以外的每一个字符和它的字节区间，按出现的先后。ASCII 的字节八个一组
/// 地跳过（[`ascii_end`]）。
fn non_ascii(text: &str) -> impl Iterator<Item = (Range<usize>, char)> + '_ {
    let b = text.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        i = ascii_end(b, i);
        // 停在一个多字节字符的头一个字节上：字符边界
        let c = text.get(i..)?.chars().next()?;
        let at = i;
        i += c.len_utf8();
        Some((at..i, c))
    })
}

/// 一处命中。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// 出厂规则的 id，或者自定义规则的名字
    pub rule: String,
    pub name: String,
    pub custom: bool,
    pub action: Action,
    /// 这条规则怎么认。码位规则命中的是看不见的字符，说给人听的话和别的不一样
    pub matching: Match,
    /// 第一处在它那一段正文里的字节区间（删过一遍之后才出现的，换算回删之前的原文）。
    /// **拼不回整个请求**：每段正文各数各的
    pub bytes: Range<usize>,
    /// 第一处前后的一小段（最多 [`SNIPPET_MAX`] 个字符），**给调用方和记录看**。码位
    /// 规则命中的字符画成看得见的样子（`‹U+E0049›`，连成一串的写成 `‹U+E0049 ×12›`）。
    /// 这是调用方自己的正文，别送进集中的应用日志
    pub snippet: String,
    /// 第一处在工具结果里，而不是调用方自己打的字
    pub in_tool_result: bool,
    /// 这条规则在这次查的全部正文里命中了几处；码位规则是几个字符
    pub count: usize,
    /// 码位规则命中了标签字符时，它们解出来的 ASCII 原文（最多 [`REVEAL_MAX`] 个
    /// 字符）：**藏的是什么一眼看得见** —— 光说「有 40 个标签字符」，没人判断得了它要
    /// 干什么。别的时候是空的
    pub revealed: String,
}

/// [`Hit::snippet`] 最多多长
pub const SNIPPET_MAX: usize = 120;

/// [`Hit::revealed`] 最多多长
pub const REVEAL_MAX: usize = 120;

/// 一组规则。
#[derive(Debug, Clone, Default)]
pub struct Rules {
    pub rules: Vec<Rule>,
}

impl Rules {
    pub fn none() -> Self {
        Self::default()
    }

    /// 编一组规则。**一条编不起来整组都不要** —— 静默跳过的话，它的表现是
    /// 「我明明写了这条规则，怎么不拦」。要跳过坏规则的调用方自己逐条 [`Rule::new`]。
    pub fn build<'a>(inputs: impl IntoIterator<Item = RuleInput<'a>>) -> Result<Self, BadRule> {
        Ok(Self {
            rules: inputs
                .into_iter()
                .map(Rule::new)
                .collect::<Result<_, _>>()?,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// 一段文本里命中的规则，每条报第一处，按规则的顺序。请求上的入口是 [`screen`]。
    pub fn scan_text(&self, text: &str) -> Vec<Hit> {
        self.detect(&[(text, false)], Scope::default()).hits
    }

    /// 查一遍：每条命中的规则一处，按发现的先后（正文的先后，同一段里按规则的顺序）。
    fn detect(&self, segments: &[(&str, bool)], scope: Scope) -> Detection {
        let mut d = Detection {
            hits: Vec::new(),
            rules: Vec::new(),
            segments: Vec::new(),
            strips: vec![Vec::new(); segments.len()],
            all: Vec::new(),
        };
        let mut seen: Vec<Option<usize>> = vec![None; self.rules.len()];
        for (si, &(text, in_tool_result)) in segments.iter().enumerate() {
            if text.is_empty() {
                continue;
            }
            let mut lower = Lower::new(text);
            for (ri, r) in self.rules.iter().enumerate() {
                if scope.codepoints_only && r.points.is_none() {
                    continue;
                }
                let f = r.find_all(text, &mut lower, scope.escapes);
                let Some(first) = f.ranges.first().cloned() else {
                    continue;
                };
                let at = match seen[ri] {
                    Some(at) => {
                        d.hits[at].count += f.count;
                        at
                    }
                    None => {
                        d.hits.push(Hit {
                            rule: r.id.clone(),
                            name: r.name.clone(),
                            custom: r.custom,
                            action: r.action,
                            matching: r.matching,
                            snippet: match &r.points {
                                Some(p) => points_snippet(p, text, first.clone(), scope.escapes),
                                None => snippet(text, first.clone()),
                            },
                            bytes: first,
                            in_tool_result,
                            count: f.count,
                            revealed: String::new(),
                        });
                        d.rules.push(ri);
                        d.segments.push(si);
                        seen[ri] = Some(d.hits.len() - 1);
                        d.hits.len() - 1
                    }
                };
                if r.points.is_some() {
                    reveal(&mut d.hits[at].revealed, text, &f.ranges, scope.escapes);
                }
                if r.action == Action::Strip {
                    d.strips[si].extend(f.ranges.iter().cloned());
                }
                if scope.keep_all {
                    d.all.push((at, si, f.ranges));
                }
            }
            d.strips[si] = merge(std::mem::take(&mut d.strips[si]));
        }
        d
    }
}

/// 一遍查的范围。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Scope {
    /// 记下每条规则的每一处（试一试要把它们都标出来）
    pub(crate) keep_all: bool,
    /// 码位规则也认 JSON 的 `\uXXXX` 写法
    pub(crate) escapes: bool,
    /// 只用码位规则
    pub(crate) codepoints_only: bool,
}

/// 一遍查的结果。
#[derive(Debug)]
struct Detection {
    /// 每条命中的规则一处
    hits: Vec<Hit>,
    /// `hits` 里每一处是第几条规则
    rules: Vec<usize>,
    /// `hits` 里每一处在第几段正文里
    segments: Vec<usize>,
    /// 每段正文里要删的区间（处置为 `strip` 的规则命中的全部地方），按先后、已合并
    strips: Vec<Vec<Range<usize>>>,
    /// `keep_all` 时：每条规则在每段里的全部区间，`(hits 的下标, 第几段, 区间)`
    all: Vec<(usize, usize, Vec<Range<usize>>)>,
}

/// 排序、合并重叠和相接的区间
fn merge(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|r| r.start);
    let mut out: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match out.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// 标签字符是「ASCII 平移到 U+E0000 之上」：减回去就是藏的那个字。接在 `out` 后面，
/// 到 [`REVEAL_MAX`] 个字符为止
fn reveal(out: &mut String, text: &str, ranges: &[Range<usize>], escapes: bool) {
    for r in ranges {
        for (_, c) in chars(&text[r.clone()], escapes) {
            if out.chars().count() >= REVEAL_MAX {
                return;
            }
            if let Some(plain) = (c as u32)
                .checked_sub(0xE0000)
                .filter(|_| ('\u{E0000}'..='\u{E007F}').contains(&c))
                .and_then(char::from_u32)
                .filter(|p| p.is_ascii_graphic() || *p == ' ')
            {
                out.push(plain);
            }
        }
    }
}

/// `i` 往前挪到字符边界上
fn floor(text: &str, mut i: usize) -> usize {
    i = i.min(text.len());
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// `i` 往后挪到字符边界上
fn ceil(text: &str, mut i: usize) -> usize {
    i = i.min(text.len());
    while !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// 小写之后长度对不上时的子串查找：逐个字符起点比。
fn find_folded(text: &str, needle_lower: &str) -> Option<Range<usize>> {
    for (i, _) in text.char_indices() {
        let mut folded = String::new();
        for (j, c) in text[i..].char_indices() {
            folded.extend(c.to_lowercase());
            if folded.len() >= needle_lower.len() {
                if folded.starts_with(needle_lower) {
                    return Some(i..i + j + c.len_utf8());
                }
                break;
            }
        }
    }
    None
}

/// 命中处前后的一小段：前面带十个字节，整段不超过 [`SNIPPET_MAX`] 个字符，
/// 截过的一头用 `…` 标出来。
fn snippet(text: &str, hit: Range<usize>) -> String {
    let start = floor(text, hit.start.saturating_sub(10));
    let body: String = text[start..].chars().take(SNIPPET_MAX).collect();
    let end = start + body.len();
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(&body);
    if end < text.len() {
        out.push('…');
    }
    out
}

/// 码位规则命中处前后的一小段：命中的字符画成 `‹U+E0049›`，连成一串的写成
/// `‹U+E0049 ×12›`（一串里第一个的码位和一共几个）。画出来之后不超过 [`SNIPPET_MAX`]
/// 个字符，截过的一头用 `…` 标出来。
fn points_snippet(p: &Codepoints, text: &str, hit: Range<usize>, escapes: bool) -> String {
    let start = floor(text, hit.start.saturating_sub(10));
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    let mut shown = 0usize;
    // 眼下这一串：第一个字符、几个
    let mut run: Option<(char, usize)> = None;
    let flush = |run: &mut Option<(char, usize)>, out: &mut String, shown: &mut usize| {
        if let Some((c, n)) = run.take() {
            let mark = if n == 1 {
                codepoints::visible(c)
            } else {
                format!("‹U+{:04X} ×{n}›", c as u32)
            };
            *shown += mark.chars().count();
            out.push_str(&mark);
        }
    };
    let mut end = text.len();
    for (r, c) in chars(&text[start..], escapes) {
        if p.contains(c) {
            match &mut run {
                Some((_, n)) => *n += 1,
                None => run = Some((c, 1)),
            }
            continue;
        }
        flush(&mut run, &mut out, &mut shown);
        if shown >= SNIPPET_MAX {
            end = start + r.start;
            break;
        }
        out.push(c);
        shown += 1;
    }
    flush(&mut run, &mut out, &mut shown);
    if end < text.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::Action::{Block, Record, Strip};
    use super::Match::{Codepoints as Points, Contains, Regex};
    use super::*;
    use crate::policy::Mode;
    use serde_json::json;
    use tw_dialect::ir::Dialect;

    fn rule<'a>(id: &'a str, pattern: &'a str, matching: Match, action: Action) -> RuleInput<'a> {
        RuleInput {
            id,
            name: id,
            custom: true,
            pattern,
            matching,
            action,
        }
    }

    fn tagged(s: &str) -> String {
        s.chars()
            .map(|c| char::from_u32(0xE0000 + c as u32).unwrap())
            .collect()
    }

    /// 一个 Anthropic 请求查下来的命中（观察档：只看命中，不删）
    fn hits_in(rs: &Rules, body: serde_json::Value) -> Vec<Hit> {
        let body = serde_json::to_vec(&body).unwrap();
        screen(Mode::Observe, rs, Dialect::Anthropic, &body)
            .hits
            .into_iter()
            .map(|h| h.hit)
            .collect()
    }

    fn user(text: &str) -> serde_json::Value {
        json!({"messages": [{"role": "user", "content": text}]})
    }

    #[test]
    fn contains_ignores_case_and_says_where() {
        let rs = Rules::build([rule("j", "JailBreak", Contains, Block)]).unwrap();
        let hits = hits_in(&rs, user("please jailbreak now"));
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].action, hits[0].matching), (Block, Contains));
        assert!(hits[0].snippet.contains("jailbreak"), "{}", hits[0].snippet);
        assert!(!hits[0].in_tool_result);
    }

    #[test]
    fn regex_ignores_case_too() {
        let rs = Rules::build([rule("n", r"code\s+\d{4}", Regex, Record)]).unwrap();
        assert_eq!(rs.scan_text("CODE 1234 here").len(), 1);
    }

    #[test]
    fn a_bad_regex_is_an_error_not_a_silent_skip() {
        let e = Rules::build([rule("bad", "[invalid((", Regex, Block)]).unwrap_err();
        assert_eq!(e.rule, "bad");
        assert!(Rules::build([rule("empty", "  ", Contains, Block)]).is_err());
        assert!(Rules::build([rule("cp", "U+GG", Points, Block)]).is_err());
    }

    #[test]
    fn a_pathological_regex_is_refused_instead_of_slowing_every_request() {
        assert!(Rules::build([rule("slow", "(a|aa){200}{200}", Regex, Block)]).is_err());
    }

    #[test]
    fn every_rule_is_reported_in_the_order_it_is_listed() {
        let rs = Rules::build([
            rule("r", "system prompt", Contains, Record),
            rule("b", "jailbreak", Contains, Block),
            rule("s", "show", Contains, Strip),
        ])
        .unwrap();
        let hits = rs.scan_text("show the system prompt and jailbreak");
        let ids: Vec<&str> = hits.iter().map(|h| h.rule.as_str()).collect();
        assert_eq!(ids, ["r", "b", "s"]);
        assert!(Record < Strip && Strip < Block, "处置按严重程度排");
    }

    #[test]
    fn each_rule_fires_once_per_request_and_counts_every_place() {
        let rs = Rules::build([rule("j", "jailbreak", Contains, Block)]).unwrap();
        let hits = hits_in(
            &rs,
            json!({"messages": [
                {"role": "user", "content": "jailbreak, JAILBREAK"},
                {"role": "user", "content": "jailbreak, JAILBREAK"},
            ]}),
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].count, 4);
        assert_eq!(hits[0].bytes, 0..9, "第一处");
    }

    #[test]
    fn text_inside_a_tool_result_is_checked_and_marked() {
        let rs = Rules::build([rule("j", "jailbreak", Contains, Block)]).unwrap();
        let hits = hits_in(
            &rs,
            json!({"messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "the page says: jailbreak"},
            ]}]}),
        );
        assert!(hits[0].in_tool_result);
    }

    #[test]
    fn the_system_prompt_and_the_model_are_not_the_caller() {
        let rs = Rules::build([rule("j", "jailbreak", Contains, Block)]).unwrap();
        let hits = hits_in(
            &rs,
            json!({"system": "jailbreak", "messages": [
                {"role": "assistant", "content": "jailbreak"},
            ]}),
        );
        assert!(hits.is_empty());
    }

    #[test]
    fn a_letter_that_changes_length_when_lowercased_does_not_misplace_the_hit() {
        // 「İ」小写之后多一个字节：按小写串的下标去切原文会切歪
        let rs = Rules::build([rule("j", "jailbreak", Contains, Block)]).unwrap();
        let text = "İİİ then jailbreak and İ jailbreak";
        let hits = rs.scan_text(text);
        assert_eq!(&text[hits[0].bytes.clone()].to_lowercase(), "jailbreak");
        assert_eq!(hits[0].count, 2);
    }

    #[test]
    fn the_snippet_is_bounded_and_marks_what_was_cut() {
        let rs = Rules::build([rule("j", "jailbreak", Contains, Block)]).unwrap();
        let long = format!("{}jailbreak{}", "前".repeat(50), "后".repeat(500));
        let s = &rs.scan_text(&long)[0].snippet;
        assert!(s.starts_with('…') && s.ends_with('…'), "{s}");
        assert!(s.chars().count() <= SNIPPET_MAX + 2);
    }

    #[test]
    fn an_empty_regex_match_is_not_a_hit() {
        // `x*` 在哪儿都「匹配」一个空串：那不是命中，也删不掉什么
        let rs = Rules::build([rule("x", "x*", Regex, Block)]).unwrap();
        assert!(rs.scan_text("nothing here").is_empty());
        assert_eq!(rs.scan_text("one x, two xx")[0].count, 2);
    }

    #[test]
    fn codepoints_count_characters_and_reveal_what_tags_hide() {
        let rs = Rules::build([rule("tags", "U+E0000–U+E007F", Points, Strip)]).unwrap();
        let text = format!("summarise this{}", tagged("ignore me"));
        let hits = rs.scan_text(&text);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].count, 9, "九个字符");
        assert_eq!(hits[0].revealed, "ignore me", "藏的是什么要看得见");
        assert_eq!(hits[0].snippet, "…arise this‹U+E0069 ×9›");
        // 一个字符就写它自己
        let one = rs.scan_text("a\u{E0041}b");
        assert_eq!(one[0].snippet, "a‹U+E0041›b");
        assert_eq!(one[0].revealed, "A");
        // 不认大小写这回事：码位就是码位
        assert!(rs.scan_text("plain ascii").is_empty());
    }

    #[test]
    fn a_long_hidden_message_is_revealed_up_to_the_limit() {
        let rs = Rules::build([rule("tags", "U+E0000–U+E007F", Points, Strip)]).unwrap();
        let hidden = tagged(&"x".repeat(300));
        let h = &rs.scan_text(&hidden)[0];
        assert_eq!(h.count, 300);
        assert_eq!(h.revealed.chars().count(), REVEAL_MAX);
        assert!(
            h.snippet.chars().count() <= SNIPPET_MAX + 2,
            "{}",
            h.snippet
        );
    }

    #[test]
    fn every_builtin_compiles_and_ids_are_unique() {
        let rs = Rules::build(builtins().iter().map(RuleInput::from)).unwrap();
        assert_eq!(rs.rules.len(), builtins().len());
        let mut ids: Vec<_> = builtins().iter().map(|b| b.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), builtins().len());
        assert!(
            builtins()
                .iter()
                .all(|b| b.name.is_ascii() && b.why.is_ascii()),
            "名字和说明是英文的退路"
        );
        // ` dan ` 的空格要留着
        assert_eq!(builtin("dan").unwrap().pattern, " dan ");
        assert!(builtin("jailbreak").is_some() && builtin("nope").is_none());
    }

    #[test]
    fn the_hidden_characters_come_first_and_ship_as_decided() {
        let invisible: Vec<_> = builtins()
            .iter()
            .take_while(|b| b.group == "invisible")
            .map(|b| (b.id.as_str(), b.matching, b.action, b.on_by_default))
            .collect();
        assert_eq!(
            invisible,
            [
                ("unicode-tags", Points, Strip, true),
                ("bidi-controls", Points, Strip, true),
                ("zero-width", Points, Strip, false),
                ("private-use", Points, Strip, false),
            ]
        );
        assert!(
            builtins()
                .iter()
                .skip(4)
                .all(|b| b.group != "invisible" && b.matching != Points),
            "隐藏字符一组排在最前，别处没有码位规则"
        );
        let range = |id: &str| builtin(id).unwrap().pattern.clone();
        assert_eq!(range("unicode-tags"), "U+E0000–U+E007F");
        assert_eq!(range("bidi-controls"), "U+202A–U+202E, U+2066–U+2069");
        assert_eq!(range("zero-width"), "U+200B–U+200D, U+2060, U+FEFF");
        assert_eq!(
            range("private-use"),
            "U+E000–U+F8FF, U+F0000–U+FFFFD, U+100000–U+10FFFD"
        );
        // 说明里讲清代价：零宽字符和私用区在正常文字里也有
        assert!(builtin("zero-width").unwrap().why.contains("Emoji"));
        assert!(builtin("private-use").unwrap().why.contains("icon fonts"));
        assert!(
            builtins()
                .iter()
                .filter(|b| b.group == "invisible")
                .all(|b| !b.why.is_empty())
        );
    }

    #[test]
    fn only_the_unambiguous_rules_ship_switched_on() {
        let on: Vec<_> = builtins()
            .iter()
            .filter(|b| b.on_by_default)
            .map(|b| b.id.as_str())
            .collect();
        assert_eq!(
            on,
            [
                "unicode-tags",
                "bidi-controls",
                "ignore-previous-instructions",
                "ignore-all-previous",
                "disregard-your-instructions"
            ]
        );
    }

    #[test]
    fn ordinary_text_in_any_script_passes_the_hidden_character_rules() {
        // 表情里的零宽连接符、波斯文的零宽不连字是正常的 —— 出厂开着的两条不查它们
        let rs = Rules::build(
            builtins()
                .iter()
                .filter(|b| b.on_by_default && b.group == "invisible")
                .map(RuleInput::from),
        )
        .unwrap();
        for s in [
            "👨\u{200D}👩\u{200D}👧 family",
            "Привет, как дела?",
            "می\u{200C}خواهم",
            "π ≈ 3.14",
        ] {
            assert!(rs.scan_text(s).is_empty(), "{s}");
        }
        assert_eq!(rs.scan_text("abc\u{202E}fed").len(), 1, "双向覆盖");
    }

    #[test]
    fn slugs_round_trip() {
        for a in [Record, Strip, Block] {
            assert_eq!(Action::from_slug(a.slug()), Some(a));
        }
        for m in [Contains, Regex, Points] {
            assert_eq!(Match::from_slug(m.slug()), Some(m));
        }
    }

    #[test]
    fn escapes_count_as_the_character_they_write_only_when_asked() {
        let p = Codepoints::parse("U+200B, U+E0000–U+E007F").unwrap();
        // 反斜杠由 `char::from(92)` 拼：测试里直接写出来的转义，经过某些编辑工具会变成真字符
        let b = char::from(92);
        let text = format!("a{b}u200bb {b}udb40{b}udc49 {b}{b}u200b {b}u200");
        let f = find_points(&p, &text, true);
        assert_eq!(f.count, 2, "{:?}", f.ranges);
        assert_eq!(text[f.ranges[0].clone()], format!("{b}u200b"));
        assert_eq!(text[f.ranges[1].clone()], format!("{b}udb40{b}udc49"));
        assert_eq!(find_points(&p, &text, false).count, 0);
        assert!(text.is_ascii(), "例子里不该有真的不可见字符");
    }

    /// 码位都在 ASCII 以外时只解码 ASCII 以外的字符：找到的和逐个字符看的一样 —— 几段、
    /// 每段从哪到哪、一共几个
    #[test]
    fn skipping_ascii_finds_what_looking_at_every_character_found() {
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        // 码位用 `char::from_u32` 拼：测试里直接写出来的转义，经过某些编辑工具会变成真字符
        let ch = |n: u32| char::from_u32(n).unwrap().to_string();
        let pieces: Vec<String> = [
            "a",
            "Z",
            " ",
            "0",
            "\"",
            "{",
            "→",
            "密钥",
            "é",
            "😀",
            "abcdefghijklmnop",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain(
            [
                0x200B, 0x202E, 0x2066, 0xE0041, 0xE007F, 0xE0000, 0x7F, 0x80, 0xFEFF,
            ]
            .into_iter()
            .map(ch),
        )
        .collect();
        let sets = [
            "U+E0000–U+E007F",
            "U+202A–U+202E, U+2066–U+2069",
            "U+200B, U+FEFF",
            "U+0080–U+10FFFF",
        ]
        .map(|p| Codepoints::parse(p).unwrap());
        for _ in 0..5_000 {
            let text: String = (0..next(60))
                .map(|_| pieces[next(pieces.len())].as_str())
                .collect();
            for p in &sets {
                assert!(p.min() > 0x7F);
                let fast = find_points(p, &text, false);
                let mut slow = Found::default();
                for (r, c) in chars(&text, false) {
                    if p.contains(c) {
                        slow.count += 1;
                        match slow.ranges.last_mut() {
                            Some(last) if last.end == r.start => last.end = r.end,
                            _ => slow.ranges.push(r),
                        }
                    }
                }
                assert_eq!(
                    (fast.count, fast.ranges),
                    (slow.count, slow.ranges),
                    "{text:?}"
                );
            }
        }
    }

    /// 快一些的小写和标准库的一字不差：`Σ` 在词尾、词中，会变长的（`İ`）、四个字节的、
    /// 组合字符、开尔文符号这类转成 ASCII 的
    #[test]
    fn lowercasing_by_runs_gives_what_the_standard_library_gives() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        // 码位用 `char::from_u32` 拼：测试里直接写出来的转义，经过某些编辑工具会变成真字符
        let ch = |n: u32| char::from_u32(n).unwrap().to_string();
        let pieces: Vec<String> = ["A", "z", " ", "IGNORE", "Previous", ".", "→", "密钥", "😀"]
            .iter()
            .map(|s| s.to_string())
            .chain(
                [
                    0x03A3, 0x0391, 0x0130, 0x212A, 0x1E9E, 0x00DF, 0x01C5, 0xFB00, 0x0301, 0x2167,
                    0x10400, 0x00C9, 0x0049, 0x0131, 0x03C2,
                ]
                .into_iter()
                .map(ch),
            )
            .collect();
        for _ in 0..20_000 {
            let text: String = (0..next(30))
                .map(|_| pieces[next(pieces.len())].as_str())
                .collect();
            assert_eq!(lowercase(&text), text.to_lowercase(), "{text:?}");
        }
        let long = format!("{}{}", "Ignore Previous ".repeat(10), ch(0x212A));
        assert_eq!(lowercase(&long), long.to_lowercase());
    }
}
