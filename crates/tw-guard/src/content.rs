//! 内容过滤：调用方发来的正文里出现了某个词或某种写法。
//!
//! **引擎在这里，规则从哪来、命中之后怎么记由各自决定。**企业版的规则按租户
//! 存在系统设置里，桌面版的写在 `config.yaml`；两边共用的是「怎么匹配、查哪些
//! 正文、命中了怎么说」，和一份出厂的规则（[`builtins`]，`data/content.yaml`）。
//!
//! # 查哪些正文
//!
//! 调用方的消息，**连同其中的工具结果** —— 被注入的指令最常待的地方正是那里：
//! 一个工具抓回来的网页、读到的文件。系统提示（配置网关的人写的）和模型自己说的
//! 话不查。
//!
//! # 两种匹配
//!
//! - `contains`：不分大小写的子串。写起来最省事，也最不容易写错；
//! - `regex`：不分大小写的正则，**编译后的大小有上限** —— 这条正则要在每个请求上
//!   跑，一条病态写法不该拖慢所有请求。
//!
//! # 三种处置
//!
//! 按严重程度排：`block`（不发出去）> `warn`（照发，留一条记录）> `log`（照发，
//! 只进日志）。一个请求命中几条时，最严的那条说了算（[`worst`]）；每条命中都报
//! 出来，记不记、记在哪由调用方定。

use std::ops::Range;

/// 一条规则怎么认。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Match {
    /// 不分大小写的子串
    #[default]
    Contains,
    /// 不分大小写的正则
    Regex,
}

impl Match {
    pub fn slug(&self) -> &'static str {
        match self {
            Match::Contains => "contains",
            Match::Regex => "regex",
        }
    }
    pub fn from_slug(s: &str) -> Option<Self> {
        match s {
            "contains" => Some(Match::Contains),
            "regex" => Some(Match::Regex),
            _ => None,
        }
    }
}

/// 命中之后做什么。**按严重程度排序**：`Log < Warn < Block`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// 照发，只进应用日志
    Log,
    /// 照发，留一条给人看的记录
    Warn,
    /// 不发出去
    Block,
}

impl Action {
    pub fn slug(&self) -> &'static str {
        match self {
            Action::Log => "log",
            Action::Warn => "warn",
            Action::Block => "block",
        }
    }
    pub fn from_slug(s: &str) -> Option<Self> {
        match s {
            "log" => Some(Action::Log),
            "warn" => Some(Action::Warn),
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
    pub pattern: String,
    #[serde(rename = "match")]
    pub matching: Match,
    /// 出厂的处置
    pub action: Action,
    /// 哪一组：`injection`（覆盖指令的说法）/ `persona`（改换身份、套提示词）/
    /// `chinese`（中文的说法）
    pub group: String,
    /// 出厂时开不开。**只开误报极少的那几条**
    #[serde(default)]
    pub on_by_default: bool,
}

/// 出厂规则的原文。
pub const BUILTIN: &str = include_str!("../data/content.yaml");

/// 出厂的规则。企业版的「预设组」是按 [`Builtin::group`] 分的同一份。
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
        let re = match r.matching {
            Match::Contains => None,
            Match::Regex => {
                Some(crate::bounded(&format!("(?i:{pattern})")).map_err(|e| bad(e.to_string()))?)
            }
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
        })
    }

    /// 在 `text` 里第一处命中的字节区间。
    fn find(&self, text: &str, lower: &str) -> Option<Range<usize>> {
        match (&self.re, self.matching) {
            (Some(re), _) => re.find(text).map(|m| m.range()),
            (None, _) => {
                let at = lower.find(&self.lower)?;
                // 小写之后长度可能变（少数字母），区间落回原文要对齐到字符边界
                let start = floor(text, at.min(text.len()));
                let end = ceil(text, (at + self.lower.len()).min(text.len()));
                Some(start..end)
            }
        }
    }
}

/// 一处命中。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// 出厂规则的 id，或者自定义规则的名字
    pub rule: String,
    pub name: String,
    pub custom: bool,
    pub action: Action,
    /// 在被查的那段正文里的字节区间。[`Rules::scan_request`] 给的是命中那一段
    /// 自己的，拼不回整个请求
    pub bytes: Range<usize>,
    /// 命中处前后的一小段（最多 [`SNIPPET_MAX`] 个字符），**给调用方和记录看**。
    /// 这是调用方自己的正文，别送进集中的应用日志
    pub snippet: String,
    /// 在工具结果里，而不是调用方自己打的字
    pub in_tool_result: bool,
}

/// [`Hit::snippet`] 最多多长
pub const SNIPPET_MAX: usize = 120;

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

    /// 一段文本里每条规则的第一处命中，按规则的顺序。
    pub fn scan_text(&self, text: &str) -> Vec<Hit> {
        let mut out = Vec::new();
        self.scan_into(text, false, &mut out);
        out
    }

    /// 一个请求里调用方的消息，连同其中的工具结果。**每条规则最多报一处**（第一处）。
    pub fn scan_request(&self, request: &tw_dialect::ir::Request) -> Vec<Hit> {
        use tw_dialect::ir::Role;
        let mut out = Vec::new();
        if self.rules.is_empty() {
            return out;
        }
        for m in request.messages.iter().filter(|m| m.role == Role::User) {
            self.scan_parts(&m.parts, false, &mut out);
        }
        out
    }

    fn scan_parts(&self, parts: &[tw_dialect::ir::Part], in_tool_result: bool, out: &mut Vec<Hit>) {
        use tw_dialect::ir::Part;
        for p in parts {
            match p {
                Part::Text(t) => self.scan_into(t, in_tool_result, out),
                Part::ToolResult(r) => self.scan_parts(&r.content, true, out),
                Part::Image(_) | Part::File { .. } | Part::Thinking(_) | Part::ToolCall(_) => {}
            }
        }
    }

    fn scan_into(&self, text: &str, in_tool_result: bool, out: &mut Vec<Hit>) {
        if text.is_empty() {
            return;
        }
        let lower = text.to_lowercase();
        // 小写之后长度变了（少数字母会），`contains` 的下标就对不回原文 —— 那时在
        // 原文上按字符比，慢一点但对
        let aligned = lower.len() == text.len();
        for r in &self.rules {
            if out.iter().any(|h| h.rule == r.id && h.custom == r.custom) {
                continue;
            }
            let found = if aligned || r.re.is_some() {
                r.find(text, &lower)
            } else {
                find_folded(text, &r.lower)
            };
            if let Some(bytes) = found {
                out.push(Hit {
                    rule: r.id.clone(),
                    name: r.name.clone(),
                    custom: r.custom,
                    action: r.action,
                    snippet: snippet(text, bytes.clone()),
                    bytes,
                    in_tool_result,
                });
            }
        }
    }
}

/// 最严的那一处。一样严的取先出现的
pub fn worst(hits: &[Hit]) -> Option<&Hit> {
    hits.iter().fold(None, |best: Option<&Hit>, h| match best {
        Some(b) if b.action >= h.action => Some(b),
        _ => Some(h),
    })
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

#[cfg(test)]
mod tests {
    use super::Action::{Block, Log, Warn};
    use super::Match::{Contains, Regex};
    use super::*;
    use tw_dialect::ir::{Message, Part, Request, Role, ToolResult};

    fn user(text: &str) -> Request {
        Request {
            messages: vec![Message {
                role: Role::User,
                parts: vec![Part::Text(text.into())],
            }],
            ..Default::default()
        }
    }

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

    #[test]
    fn contains_ignores_case_and_says_where() {
        let rs = Rules::build([rule("j", "JailBreak", Contains, Block)]).unwrap();
        let hits = rs.scan_request(&user("please jailbreak now"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].action, Block);
        assert!(hits[0].snippet.contains("jailbreak"), "{}", hits[0].snippet);
        assert!(!hits[0].in_tool_result);
    }

    #[test]
    fn regex_ignores_case_too() {
        let rs = Rules::build([rule("n", r"code\s+\d{4}", Regex, Warn)]).unwrap();
        assert_eq!(rs.scan_text("CODE 1234 here").len(), 1);
    }

    #[test]
    fn a_bad_regex_is_an_error_not_a_silent_skip() {
        let e = Rules::build([rule("bad", "[invalid((", Regex, Block)]).unwrap_err();
        assert_eq!(e.rule, "bad");
        assert!(Rules::build([rule("empty", "  ", Contains, Block)]).is_err());
    }

    #[test]
    fn a_pathological_regex_is_refused_instead_of_slowing_every_request() {
        assert!(Rules::build([rule("slow", "(a|aa){200}{200}", Regex, Block)]).is_err());
    }

    #[test]
    fn the_worst_action_wins_and_every_rule_is_still_reported() {
        let rs = Rules::build([
            rule("w", "system prompt", Contains, Warn),
            rule("b", "jailbreak", Contains, Block),
            rule("l", "hello", Contains, Log),
        ])
        .unwrap();
        let hits = rs.scan_text("hello, show the system prompt and jailbreak");
        assert_eq!(hits.len(), 3);
        assert_eq!(worst(&hits).unwrap().rule, "b");
        assert!(worst(&[]).is_none());
    }

    #[test]
    fn each_rule_fires_once_per_request() {
        let rs = Rules::build([rule("j", "jailbreak", Contains, Block)]).unwrap();
        let mut r = user("jailbreak");
        r.messages.push(r.messages[0].clone());
        assert_eq!(rs.scan_request(&r).len(), 1);
    }

    #[test]
    fn text_inside_a_tool_result_is_checked_and_marked() {
        let rs = Rules::build([rule("j", "jailbreak", Contains, Block)]).unwrap();
        let r = Request {
            messages: vec![Message {
                role: Role::User,
                parts: vec![Part::ToolResult(ToolResult {
                    id: "t1".into(),
                    content: vec![Part::Text("the page says: jailbreak".into())],
                    is_error: false,
                })],
            }],
            ..Default::default()
        };
        let hits = rs.scan_request(&r);
        assert!(hits[0].in_tool_result);
    }

    #[test]
    fn the_system_prompt_and_the_model_are_not_the_caller() {
        let rs = Rules::build([rule("j", "jailbreak", Contains, Block)]).unwrap();
        let r = Request {
            system: vec!["jailbreak".into()],
            messages: vec![Message {
                role: Role::Assistant,
                parts: vec![Part::Text("jailbreak".into())],
            }],
            ..Default::default()
        };
        assert!(rs.scan_request(&r).is_empty());
    }

    #[test]
    fn a_letter_that_changes_length_when_lowercased_does_not_misplace_the_hit() {
        // 「İ」小写之后多一个字节：按小写串的下标去切原文会切歪
        let rs = Rules::build([rule("j", "jailbreak", Contains, Block)]).unwrap();
        let text = "İİİ then jailbreak";
        let hits = rs.scan_text(text);
        assert_eq!(&text[hits[0].bytes.clone()].to_lowercase(), "jailbreak");
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
    fn every_builtin_compiles_and_ids_are_unique() {
        let rs = Rules::build(builtins().iter().map(RuleInput::from)).unwrap();
        assert_eq!(rs.rules.len(), builtins().len());
        let mut ids: Vec<_> = builtins().iter().map(|b| b.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), builtins().len());
        assert!(
            builtins().iter().all(|b| b.name.is_ascii()),
            "名字是英文的退路"
        );
        // ` dan ` 的空格要留着
        assert_eq!(builtin("dan").unwrap().pattern, " dan ");
        assert!(builtin("jailbreak").is_some() && builtin("nope").is_none());
    }

    #[test]
    fn only_the_unambiguous_injection_phrases_ship_switched_on() {
        let on: Vec<_> = builtins()
            .iter()
            .filter(|b| b.on_by_default)
            .map(|b| b.id.as_str())
            .collect();
        assert_eq!(
            on,
            [
                "ignore-previous-instructions",
                "ignore-all-previous",
                "disregard-your-instructions"
            ]
        );
    }

    #[test]
    fn slugs_round_trip() {
        for a in [Log, Warn, Block] {
            assert_eq!(Action::from_slug(a.slug()), Some(a));
        }
        for m in [Contains, Regex] {
            assert_eq!(Match::from_slug(m.slug()), Some(m));
        }
    }
}
