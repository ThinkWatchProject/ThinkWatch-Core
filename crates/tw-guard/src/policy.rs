//! 三项防护的策略：出站脱敏、工具调用审查、内容过滤。
//!
//! **两个产品存的是同一份。**桌面版写在 `config.yaml` 的 `security:` 下，企业版存在
//! 系统设置的 `security.redact` / `security.inspect_tools` / `security.content` 三个键里
//! （JSON）。形状、出厂值、校验、编译成规则集都在这里；各自只管存在哪儿、错误怎么报。
//!
//! ```yaml
//! security:
//!   redact:
//!     mode: observe                # off | observe | enforce，出厂 observe
//!     enable: [email]              # 打开出厂关着的内置规则（按 id）
//!     disable: [jwt]               # 关掉出厂开着的内置规则
//!     custom:
//!       - name: 内部项目号          # 名字也是标识，同一项里不许重名
//!         pattern: 'PRJ-\d{6}'     # 正则
//!         label: PROJECT           # 可选；占位符 <<TW_PROJECT_1>>；不写 = SECRET
//!         disabled: true           # 可选
//!   inspect_tools:
//!     mode: observe
//!     actions: { rm-rf-root: record }   # 只写和出厂不同的；cut | record
//!     custom:
//!       - { name: 删除集群资源, pattern: 'kubectl\s+delete', action: cut }
//!   content:
//!     mode: observe
//!     enable: [zero-width]
//!     actions: { unicode-tags: block }  # 只写和出厂不同的；block | strip | record
//!     custom:
//!       - name: 内部代号
//!         pattern: project-x
//!         match: contains          # contains | regex | codepoints；不写 = contains
//!         action: strip            # block | strip | record；不写 = record
//! ```
//!
//! 每个字段都是「不写就是出厂值」，写回时出厂值也不写：一份没改过的策略是 `{}`。
//! 认不出的字段一律是错（`deny_unknown_fields`）—— 拼错一个字段名被静默当成出厂值，
//! 用户会以为自己关掉了它。
//!
//! # 三态：关闭 / 观察 / 第三档
//!
//! **出厂时停在「观察」。**安全功能第一次接触用户的方式如果是「误报打断了正在跑的
//! 任务」，它就死了 —— 用户会关掉整个功能，而且再也不会打开。但直接关掉又等于白做。
//! 「观察」不打扰任何人，却在悄悄攒一件事：**属于用户自己的证据**。跑上一周，界面上
//! 出现的不是一句「我们有安全功能」，而是「过去 7 天，有 3 个请求把你的 API key 发了
//! 出去」。
//!
//! 第三档按各自的动作命名：出站脱敏「替换」、工具调用审查「切断」、内容过滤「处置」
//! （规则各自拒绝、删除或仅记录）。
//!
//! # 规则：内置的开关 + 自定义的
//!
//! **加法加停用，不是整份替换。**用户复制一份内置规则再改两条之后，他那份就永远停在
//! 复制的那一刻了 —— 我们后来加的每一条都到不了他那里，而他不会察觉。所以内置规则只
//! 记「改过出厂开关、改过处置的那几条」，自定义规则另起一个列表。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::content::{self, CodepointError, Codepoints};
use crate::redact::rules::RuleSet;
use crate::tools::rules as tool_rules;

/// 一项防护的档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "GuardMode"))]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// 什么都不做
    Off,
    /// 照常检测，**只记录，不改变任何行为**。出厂值
    #[default]
    Observe,
    /// 检测并动手：替换、切断、或者按规则的处置
    Enforce,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Off, Mode::Observe, Mode::Enforce];

    pub fn detects(&self) -> bool {
        !matches!(self, Mode::Off)
    }
    /// 会不会改变请求的去向或内容。**观察档永远是 false。**
    pub fn acts(&self) -> bool {
        matches!(self, Mode::Enforce)
    }
    /// 配置里写的那个词
    pub fn slug(&self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Observe => "observe",
            Mode::Enforce => "enforce",
        }
    }
    pub fn from_slug(s: &str) -> Option<Self> {
        Mode::ALL.into_iter().find(|m| m.slug() == s)
    }
}

/// 哪一项防护。配置里 `security` 下的那个键，也是管理接口路径里的那一段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum Guard {
    /// 出站脱敏
    #[serde(rename = "redact")]
    Redact,
    /// 工具调用审查
    #[serde(rename = "inspect_tools")]
    InspectTools,
    /// 内容过滤
    #[serde(rename = "content")]
    Content,
}

impl Guard {
    pub const ALL: [Guard; 3] = [Guard::Redact, Guard::InspectTools, Guard::Content];

    pub fn slug(self) -> &'static str {
        match self {
            Guard::Redact => "redact",
            Guard::InspectTools => "inspect_tools",
            Guard::Content => "content",
        }
    }
    pub fn from_slug(s: &str) -> Option<Self> {
        Guard::ALL.into_iter().find(|g| g.slug() == s)
    }
    /// 错误信息里怎么称呼这一项的规则：`redaction` / `tool-call` / `content`
    pub fn rule_noun(self) -> &'static str {
        match self {
            Guard::Redact => "redaction",
            Guard::InspectTools => "tool-call",
            Guard::Content => "content",
        }
    }
}

impl std::fmt::Display for Guard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.slug())
    }
}

/// 一条工具调用规则命中之后，在「切断」档下做什么。观察档一律只记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolAction {
    /// 切断响应。客户端拿不到完整的调用，也就执行不了
    Cut,
    /// 只记录，调用照常返回。**不写就是它** —— 手写的规则默认只记不切，要它动手得
    /// 自己写明白（「零值 = 安全」）
    #[default]
    Record,
}

impl ToolAction {
    pub fn slug(&self) -> &'static str {
        match self {
            ToolAction::Cut => "cut",
            ToolAction::Record => "record",
        }
    }
    pub fn from_slug(s: &str) -> Option<Self> {
        [ToolAction::Cut, ToolAction::Record]
            .into_iter()
            .find(|a| a.slug() == s)
    }
    /// 一条内置规则出厂时在「切断」档下做什么：能一步拿到执行权或者拿走凭据的切断
    pub fn factory(spec: &tool_rules::RuleSpec) -> Self {
        if spec.high() {
            ToolAction::Cut
        } else {
            ToolAction::Record
        }
    }
}

/// 一条内容规则命中之后，在「处置」档下做什么。观察档一律只记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentAction {
    /// 拒绝：请求不发出去，客户端收到原因
    Block,
    /// 删除：把命中的字从用户消息和工具结果里删掉，再照常发出
    Strip,
    /// 仅记录。**手写的规则不写就是它**（「零值 = 安全」）
    #[default]
    Record,
}

impl ContentAction {
    pub fn slug(&self) -> &'static str {
        match self {
            ContentAction::Block => "block",
            ContentAction::Strip => "strip",
            ContentAction::Record => "record",
        }
    }
    pub fn from_slug(s: &str) -> Option<Self> {
        [
            ContentAction::Block,
            ContentAction::Strip,
            ContentAction::Record,
        ]
        .into_iter()
        .find(|a| a.slug() == s)
    }
    /// 一条内置规则出厂时的处置。出厂文件里写 `warn` / `log` 的（`record` 以前的名字）
    /// 是仅记录
    pub fn factory(b: &content::Builtin) -> Self {
        match b.action {
            content::Action::Block => ContentAction::Block,
            content::Action::Strip => ContentAction::Strip,
            content::Action::Record | content::Action::Warn | content::Action::Log => {
                ContentAction::Record
            }
        }
    }
    /// 引擎里的那个处置
    pub fn engine(self) -> content::Action {
        match self {
            ContentAction::Block => content::Action::Block,
            ContentAction::Strip => content::Action::Strip,
            ContentAction::Record => content::Action::Record,
        }
    }
}

/// 一条内容规则怎么认。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "lowercase")]
pub enum ContentMatch {
    /// 不分大小写的子串。**不写就是它**：关键词是最常见的写法
    #[default]
    Contains,
    /// 不分大小写的正则
    Regex,
    /// 码位：`U+200B`、`U+E0000–U+E007F`，多个之间用逗号隔开
    Codepoints,
}

impl ContentMatch {
    pub fn slug(&self) -> &'static str {
        self.engine().slug()
    }
    pub fn from_slug(s: &str) -> Option<Self> {
        Some(Self::of(content::Match::from_slug(s)?))
    }
    pub fn engine(self) -> content::Match {
        match self {
            ContentMatch::Contains => content::Match::Contains,
            ContentMatch::Regex => content::Match::Regex,
            ContentMatch::Codepoints => content::Match::Codepoints,
        }
    }
    pub fn of(m: content::Match) -> Self {
        match m {
            content::Match::Contains => ContentMatch::Contains,
            content::Match::Regex => ContentMatch::Regex,
            content::Match::Codepoints => ContentMatch::Codepoints,
        }
    }
}

/// 占位符里标签前面的那一段：`<<TW_PROJECT_1>>` 的 `TW_`
pub const LABEL_PREFIX: &str = "TW_";

/// 不写标签的规则用的标签：`<<TW_SECRET_1>>`
pub const DEFAULT_LABEL: &str = "SECRET";

/// 标签最长多少个字符
pub const LABEL_MAX: usize = 24;

/// 一个标签写得对不对：大写字母开头，其余是大写字母、数字、下划线，最多 [`LABEL_MAX`]
/// 个字符。**它要原样放进占位符**：一个空格、一个尖括号，占位符就和正文分不开了
pub fn label_ok(label: &str) -> bool {
    let b = label.as_bytes();
    !b.is_empty()
        && b.len() <= LABEL_MAX
        && b[0].is_ascii_uppercase()
        && b.iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

/// 一个标签在占位符里的样子：`PROJECT` → `TW_PROJECT`（占位符是 `<<TW_PROJECT_1>>`）
pub fn placeholder_label(label: &str) -> String {
    format!("{LABEL_PREFIX}{label}")
}

/// 用户自己写的一条脱敏规则：匹配到的整段换成占位符。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomRedactRule {
    /// 日志和界面上显示的名字，也是它的标识
    pub name: String,
    /// 正则表达式
    pub pattern: String,
    /// 占位符里的标签：`PROJECT` 换成 `<<TW_PROJECT_1>>`。不写是 [`DEFAULT_LABEL`]；写的
    /// 就是它的，读进来也是不写（见 `label_or_default`）
    #[serde(
        default,
        deserialize_with = "label_or_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub label: Option<String>,
    /// 停用。**规则原样留着**，打开就回来
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
}

/// 出站脱敏：请求发出前，在整个请求里按规则查找凭据和个人信息。第三档是**替换**。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedactPolicy {
    #[serde(default, skip_serializing_if = "is_default")]
    pub mode: Mode,
    /// 打开这几条出厂时关着的内置规则，按 id
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enable: Vec<String>,
    /// 关掉这几条内置规则，按 id
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disable: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom: Vec<CustomRedactRule>,
}

/// 用户自己写的一条工具调用规则：按工具调用的参数匹配。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomToolRule {
    pub name: String,
    pub pattern: String,
    /// 「切断」档下命中之后做什么
    #[serde(default, skip_serializing_if = "is_default")]
    pub action: ToolAction,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
}

/// 工具调用审查：检查上游返回的工具调用参数。第三档是**切断**，只对处置为「切断」
/// 的规则。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPolicy {
    #[serde(default, skip_serializing_if = "is_default")]
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enable: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disable: Vec<String>,
    /// 内置规则在「切断」档下做什么，**只写和出厂不一样的**：`rm-rf-root: cut`
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub actions: BTreeMap<String, ToolAction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom: Vec<CustomToolRule>,
}

/// 用户自己写的一条内容规则。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomContentRule {
    pub name: String,
    /// 关键词、正则，或者码位（`U+200B, U+E0000–U+E007F`）
    pub pattern: String,
    #[serde(rename = "match", default, skip_serializing_if = "is_default")]
    pub matching: ContentMatch,
    #[serde(default, skip_serializing_if = "is_default")]
    pub action: ContentAction,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
}

/// 内容过滤：调用方发来的正文里（连同工具结果）出现了某个词、某种写法或者某些字符。
/// 第三档是**处置**：规则各自拒绝、删除或者仅记录。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentPolicy {
    #[serde(default, skip_serializing_if = "is_default")]
    pub mode: Mode,
    /// 打开这几条出厂时关着的内置规则，按 id
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enable: Vec<String>,
    /// 关掉这几条内置规则，按 id
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disable: Vec<String>,
    /// 内置规则在「处置」档下做什么，**只写和出厂不一样的**
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub actions: BTreeMap<String, ContentAction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom: Vec<CustomContentRule>,
}

/// 三项防护。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Security {
    #[serde(default, skip_serializing_if = "is_default")]
    pub redact: RedactPolicy,
    #[serde(default, skip_serializing_if = "is_default")]
    pub inspect_tools: ToolPolicy,
    #[serde(default, skip_serializing_if = "is_default")]
    pub content: ContentPolicy,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// 标签写的是出厂的那个（[`DEFAULT_LABEL`]）就和没写一样。管理界面把出厂的标签显式填在
/// 框里，拼回整份策略存的时候会带上它 —— 存回来的不该因此和没写的那一份不一样
fn label_or_default<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Option::<String>::deserialize(d)?.filter(|l| l != DEFAULT_LABEL))
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

// ---------------------------------------------------------------- 校验

/// 一份策略过不了校验。`code()` 是稳定的码，`Display` 是英文的一句话。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("a custom {} rule has no name", .guard.rule_noun())]
    EmptyName { guard: Guard },
    #[error("the custom {} rule name `{name}` appears twice", .guard.rule_noun())]
    DuplicateName { guard: Guard, name: String },
    #[error("the pattern of custom {} rule `{name}` is empty", .guard.rule_noun())]
    EmptyPattern { guard: Guard, name: String },
    #[error(
        "the pattern of custom {} rule `{name}` is not a valid regular expression: {detail}",
        .guard.rule_noun()
    )]
    BadPattern {
        guard: Guard,
        name: String,
        detail: String,
    },
    #[error("the code points of custom content rule `{name}` are not written right: {reason}")]
    BadCodepoints {
        name: String,
        reason: CodepointError,
    },
    #[error(
        "the placeholder name of custom redaction rule `{name}` is `{label}`; it has to be 1 to \
         24 capital letters, digits and underscores, starting with a letter"
    )]
    BadLabel { name: String, label: String },
    #[error("security.{guard} names `{id}`, which is not a built-in rule")]
    UnknownRule { guard: Guard, id: String },
}

impl PolicyError {
    /// 稳定的码。桌面版映射成自己的消息码（`config.` + 它），企业版放进 400 的回答里
    pub fn code(&self) -> &'static str {
        match self {
            PolicyError::EmptyName { .. } => "rule_name_empty",
            PolicyError::DuplicateName { .. } => "rule_name_taken",
            PolicyError::EmptyPattern { .. } => "rule_pattern_empty",
            PolicyError::BadPattern { .. } => "rule_pattern_bad",
            PolicyError::BadCodepoints { .. } => "rule_codepoints_bad",
            PolicyError::BadLabel { .. } => "rule_label_bad",
            PolicyError::UnknownRule { .. } => "unknown_rule",
        }
    }

    /// 出在哪一项上
    pub fn guard(&self) -> Guard {
        match self {
            PolicyError::EmptyName { guard }
            | PolicyError::DuplicateName { guard, .. }
            | PolicyError::EmptyPattern { guard, .. }
            | PolicyError::BadPattern { guard, .. }
            | PolicyError::UnknownRule { guard, .. } => *guard,
            PolicyError::BadCodepoints { .. } => Guard::Content,
            PolicyError::BadLabel { .. } => Guard::Redact,
        }
    }
}

/// 自定义规则的名字：不空、不重复
fn check_names<'a>(
    guard: Guard,
    names: impl IntoIterator<Item = &'a str>,
) -> Result<(), PolicyError> {
    let mut seen = std::collections::HashSet::new();
    for name in names {
        if name.trim().is_empty() {
            return Err(PolicyError::EmptyName { guard });
        }
        if !seen.insert(name) {
            return Err(PolicyError::DuplicateName {
                guard,
                name: name.to_string(),
            });
        }
    }
    Ok(())
}

/// 内置规则的 id 得真的存在。**写错一个 id 和写错一个字段名是同一种错**：跳过它，
/// 用户停用的那条会照样在报
fn check_ids<'a>(
    guard: Guard,
    ids: impl IntoIterator<Item = &'a String>,
    known: impl Fn(&str) -> bool,
) -> Result<(), PolicyError> {
    match ids.into_iter().find(|id| !known(id)) {
        Some(id) => Err(PolicyError::UnknownRule {
            guard,
            id: id.clone(),
        }),
        None => Ok(()),
    }
}

impl Security {
    /// 校验整份策略：自定义规则的名字、正则、码位、标签，按 id 引用的内置规则。
    /// 遇到第一处错就停。
    pub fn check(&self) -> Result<(), PolicyError> {
        self.redact.check()?;
        self.inspect_tools.check()?;
        self.content.check()
    }
}

impl RedactPolicy {
    pub fn check(&self) -> Result<(), PolicyError> {
        let g = Guard::Redact;
        check_names(g, self.custom.iter().map(|c| c.name.as_str()))?;
        for c in &self.custom {
            custom_redact(c)?;
        }
        check_ids(g, self.enable.iter().chain(&self.disable), |id| {
            crate::redact::rules::builtin(id).is_some()
        })
    }

    /// 这条内置规则现在开着吗
    pub fn builtin_on(&self, b: &crate::redact::rules::Builtin) -> bool {
        if b.on_by_default {
            !self.disable.iter().any(|x| x == b.id)
        } else {
            self.enable.iter().any(|x| x == b.id)
        }
    }

    /// 这一份策略下的规则：内置的按开关，再加上启用着的自定义规则（各带自己的标签）。
    pub fn rules(&self) -> Result<RuleSet, PolicyError> {
        let mut set = RuleSet::build(
            &self.enable,
            &self.disable,
            std::iter::empty::<(&str, &str)>(),
        )
        .map_err(|e| PolicyError::BadPattern {
            guard: Guard::Redact,
            name: e.name,
            detail: e.detail,
        })?;
        for c in self.custom.iter().filter(|c| !c.disabled) {
            let label = custom_redact(c)?;
            set = set
                .with_labeled(&c.name, &c.pattern, label.as_deref())
                .map_err(|e| PolicyError::BadPattern {
                    guard: Guard::Redact,
                    name: e.name,
                    detail: e.detail,
                })?;
        }
        Ok(set)
    }
}

/// 一条自定义脱敏规则编得过吗；编得过的话，它在占位符里的标签（不写是 `None`，用
/// 账本的默认标签）
fn custom_redact(c: &CustomRedactRule) -> Result<Option<String>, PolicyError> {
    if c.pattern.is_empty() {
        return Err(PolicyError::EmptyPattern {
            guard: Guard::Redact,
            name: c.name.clone(),
        });
    }
    crate::redact::rules::compile(&c.name, &c.pattern).map_err(|e| PolicyError::BadPattern {
        guard: Guard::Redact,
        name: c.name.clone(),
        detail: e.detail,
    })?;
    match &c.label {
        None => Ok(None),
        Some(l) if label_ok(l) => Ok(Some(placeholder_label(l))),
        Some(l) => Err(PolicyError::BadLabel {
            name: c.name.clone(),
            label: l.clone(),
        }),
    }
}

/// 工具调用审查的内置规则 id：内置规则里「危险命令」那一组
fn tool_builtin(id: &str) -> Option<&'static tool_rules::RuleSpec> {
    tool_rules::builtin().dangerous.iter().find(|s| s.id == id)
}

impl ToolPolicy {
    pub fn check(&self) -> Result<(), PolicyError> {
        let g = Guard::InspectTools;
        check_names(g, self.custom.iter().map(|c| c.name.as_str()))?;
        for c in &self.custom {
            custom_tool(c)?;
        }
        check_ids(
            g,
            self.enable
                .iter()
                .chain(&self.disable)
                .chain(self.actions.keys()),
            |id| tool_builtin(id).is_some(),
        )
    }

    /// 这条内置规则在「切断」档下做什么（改过的按改过的）
    pub fn builtin_action(&self, spec: &tool_rules::RuleSpec) -> ToolAction {
        self.actions
            .get(&spec.id)
            .copied()
            .unwrap_or_else(|| ToolAction::factory(spec))
    }

    /// 这一份策略下的规则：内置的去掉停用的、按改过的处置走，再加上启用着的自定义规则。
    pub fn rules(&self) -> Result<tool_rules::Rules, PolicyError> {
        tool_rules::tool_rules(
            &self.disable,
            |id| self.actions.get(id).map(|a| *a == ToolAction::Cut),
            self.custom
                .iter()
                .filter(|c| !c.disabled)
                .map(|c| tool_rules::Custom {
                    name: &c.name,
                    pattern: &c.pattern,
                    cut: c.action == ToolAction::Cut,
                }),
        )
        .map_err(|tool_rules::RuleError::BadPattern { name, detail }| {
            PolicyError::BadPattern {
                guard: Guard::InspectTools,
                name,
                detail,
            }
        })
    }

    /// 只有一条内置规则，**不管它启用没有**，处置按这份策略走。管理界面上「试一条
    /// 停用着的规则」用它。不是内置规则的 id 是 `None`
    pub fn one_builtin(&self, id: &str) -> Option<tool_rules::Rules> {
        tool_rules::one_builtin(id, self.actions.get(id).map(|a| *a == ToolAction::Cut))
    }
}

fn custom_tool(c: &CustomToolRule) -> Result<(), PolicyError> {
    if c.pattern.is_empty() {
        return Err(PolicyError::EmptyPattern {
            guard: Guard::InspectTools,
            name: c.name.clone(),
        });
    }
    tool_rules::single(&c.name, &c.pattern, false)
        .map(|_| ())
        .map_err(
            |tool_rules::RuleError::BadPattern { detail, .. }| PolicyError::BadPattern {
                guard: Guard::InspectTools,
                name: c.name.clone(),
                detail,
            },
        )
}

impl ContentPolicy {
    pub fn check(&self) -> Result<(), PolicyError> {
        let g = Guard::Content;
        check_names(g, self.custom.iter().map(|c| c.name.as_str()))?;
        for c in &self.custom {
            custom_content(c)?;
        }
        check_ids(
            g,
            self.enable
                .iter()
                .chain(&self.disable)
                .chain(self.actions.keys()),
            |id| content::builtin(id).is_some(),
        )
    }

    /// 这条内置规则现在开着吗
    pub fn builtin_on(&self, b: &content::Builtin) -> bool {
        if b.on_by_default {
            !self.disable.contains(&b.id)
        } else {
            self.enable.contains(&b.id)
        }
    }

    /// 这条内置规则在「处置」档下做什么（改过的按改过的）
    pub fn builtin_action(&self, b: &content::Builtin) -> ContentAction {
        self.actions
            .get(&b.id)
            .copied()
            .unwrap_or_else(|| ContentAction::factory(b))
    }

    /// 这一份策略下的规则：开着的内置规则按改过的处置走，再加上启用着的自定义规则。
    pub fn rules(&self) -> Result<content::Rules, PolicyError> {
        let mut rules = Vec::new();
        for b in content::builtins().iter().filter(|b| self.builtin_on(b)) {
            rules.push(self.builtin_rule(b));
        }
        for c in self.custom.iter().filter(|c| !c.disabled) {
            rules.push(custom_content(c)?);
        }
        Ok(content::Rules { rules })
    }

    /// 只有一条内置规则，**不管它开没开**，处置按这份策略走。「试一条」用它。不是内置
    /// 规则的 id 是 `None`
    pub fn one_builtin(&self, id: &str) -> Option<content::Rules> {
        let b = content::builtin(id)?;
        Some(content::Rules {
            rules: vec![self.builtin_rule(b)],
        })
    }

    fn builtin_rule(&self, b: &content::Builtin) -> content::Rule {
        content::Rule::new(content::RuleInput {
            action: self.builtin_action(b).engine(),
            ..content::RuleInput::from(b)
        })
        // 随二进制一起编进来的规则，编不起来是构建出了错 —— 测试会先挂
        .expect("the built-in content rules compile")
    }
}

/// 编一条自定义内容规则。码位写错的说清是哪一项、错在哪
fn custom_content(c: &CustomContentRule) -> Result<content::Rule, PolicyError> {
    if c.pattern.trim().is_empty() {
        return Err(PolicyError::EmptyPattern {
            guard: Guard::Content,
            name: c.name.clone(),
        });
    }
    if c.matching == ContentMatch::Codepoints {
        Codepoints::parse(&c.pattern).map_err(|reason| PolicyError::BadCodepoints {
            name: c.name.clone(),
            reason,
        })?;
    }
    content::Rule::new(content::RuleInput {
        id: &c.name,
        name: &c.name,
        custom: true,
        pattern: &c.pattern,
        matching: c.matching.engine(),
        action: c.action.engine(),
    })
    .map_err(|e| PolicyError::BadPattern {
        guard: Guard::Content,
        name: c.name.clone(),
        detail: e.detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 约定里的那一份，原样
    const EXAMPLE: &str = r#"
redact:
  mode: observe
  enable: [email]
  disable: [jwt]
  custom:
    - name: 内部项目号
      pattern: 'PRJ-\d{6}'
      label: PROJECT
      disabled: true
inspect_tools:
  mode: observe
  enable: []
  disable: []
  actions: { rm-rf-root: record }
  custom:
    - { name: 删除集群资源, pattern: 'kubectl\s+delete', action: cut, disabled: false }
content:
  mode: observe
  enable: [zero-width]
  disable: [act-as]
  actions: { unicode-tags: block }
  custom:
    - name: 内部代号
      pattern: project-x
      match: contains
      action: strip
      disabled: false
"#;

    #[test]
    fn the_documented_shape_reads_and_round_trips_through_yaml_and_json() {
        let s: Security = serde_yaml_ng::from_str(EXAMPLE).unwrap();
        s.check().unwrap();
        assert_eq!(s.redact.custom[0].label.as_deref(), Some("PROJECT"));
        assert_eq!(s.content.custom[0].action, ContentAction::Strip);
        assert_eq!(
            s.content.actions.get("unicode-tags"),
            Some(&ContentAction::Block)
        );
        // YAML 和 JSON 都写得出、读得回，读回来一样
        let yaml = serde_yaml_ng::to_string(&s).unwrap();
        assert_eq!(serde_yaml_ng::from_str::<Security>(&yaml).unwrap(), s);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Security>(&json).unwrap(), s);
        // 出厂值不写回：observe、空名单、disabled: false、match: contains 都不在
        for gone in ["observe", "enable: []", "disabled: false", "match:"] {
            assert!(!yaml.contains(gone), "{gone} 写回去了：{yaml}");
        }
        // 企业版一项一个键，各存各的
        let content: ContentPolicy =
            serde_json::from_value(serde_json::to_value(&s.content).unwrap()).unwrap();
        assert_eq!(content, s.content);
    }

    #[test]
    fn an_untouched_policy_is_empty_and_ships_in_observe() {
        let s = Security::default();
        assert_eq!(serde_yaml_ng::to_string(&s).unwrap().trim(), "{}");
        assert_eq!(serde_json::to_string(&s).unwrap(), "{}");
        assert_eq!(serde_json::to_string(&s.redact).unwrap(), "{}");
        for m in [s.redact.mode, s.inspect_tools.mode, s.content.mode] {
            assert_eq!(m, Mode::Observe, "出厂是观察档");
        }
        assert_eq!(serde_json::from_str::<Security>("{}").unwrap(), s);
    }

    #[test]
    fn misspellings_are_errors_not_silent_defaults() {
        // 「observ」被静默当成出厂值最糟：用户以为自己关掉了，而它还在跑
        for bad in [
            "redact:\n  mode: observ",
            "redcat:\n  mode: off",
            "content:\n  custom:\n    - { name: a, pattern: b, action: warn }",
            "content:\n  custom:\n    - { name: a, pattern: b, match: glob }",
            "redact:\n  custom:\n    - { name: a, pattern: b, lable: X }",
            "hidden_text:\n  mode: off",
            "output_limit:\n  mode: off",
        ] {
            assert!(serde_yaml_ng::from_str::<Security>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn slugs_round_trip() {
        for m in Mode::ALL {
            assert_eq!(Mode::from_slug(m.slug()), Some(m));
            let back: Mode = serde_json::from_value(m.slug().into()).unwrap();
            assert_eq!(back, m);
        }
        for g in Guard::ALL {
            assert_eq!(Guard::from_slug(g.slug()), Some(g));
            assert_eq!(serde_json::to_value(g).unwrap(), g.slug());
        }
        for a in [ToolAction::Cut, ToolAction::Record] {
            assert_eq!(ToolAction::from_slug(a.slug()), Some(a));
        }
        for a in [
            ContentAction::Block,
            ContentAction::Strip,
            ContentAction::Record,
        ] {
            assert_eq!(ContentAction::from_slug(a.slug()), Some(a));
            assert_eq!(serde_json::to_value(a).unwrap(), a.slug());
        }
        for m in [
            ContentMatch::Contains,
            ContentMatch::Regex,
            ContentMatch::Codepoints,
        ] {
            assert_eq!(ContentMatch::from_slug(m.slug()), Some(m));
            assert_eq!(serde_json::to_value(m).unwrap(), m.slug());
        }
    }

    fn err(yaml: &str) -> PolicyError {
        serde_yaml_ng::from_str::<Security>(yaml)
            .unwrap()
            .check()
            .unwrap_err()
    }

    #[test]
    fn every_kind_of_mistake_has_its_own_code() {
        let cases = [
            (
                "redact:\n  custom:\n    - { name: ' ', pattern: a }",
                "rule_name_empty",
            ),
            (
                "inspect_tools:\n  custom:\n    - { name: a, pattern: x }\n    - { name: a, pattern: y }",
                "rule_name_taken",
            ),
            (
                "content:\n  custom:\n    - { name: a, pattern: '  ' }",
                "rule_pattern_empty",
            ),
            (
                "redact:\n  custom:\n    - { name: a, pattern: '(' }",
                "rule_pattern_bad",
            ),
            (
                "content:\n  custom:\n    - { name: a, pattern: '[', match: regex }",
                "rule_pattern_bad",
            ),
            (
                "content:\n  custom:\n    - { name: a, pattern: 'U+GG', match: codepoints }",
                "rule_codepoints_bad",
            ),
            (
                "redact:\n  custom:\n    - { name: a, pattern: x, label: project }",
                "rule_label_bad",
            ),
            ("redact:\n  enable: [jwtt]", "unknown_rule"),
            ("inspect_tools:\n  actions: { nope: cut }", "unknown_rule"),
            ("content:\n  disable: [jailbrake]", "unknown_rule"),
        ];
        for (yaml, code) in cases {
            let e = err(yaml);
            assert_eq!(e.code(), code, "{yaml}: {e}");
        }
        assert_eq!(
            err("content:\n  disable: [jailbrake]"),
            PolicyError::UnknownRule {
                guard: Guard::Content,
                id: "jailbrake".into()
            }
        );
        assert_eq!(
            err("content:\n  disable: [jailbrake]").to_string(),
            "security.content names `jailbrake`, which is not a built-in rule"
        );
        let e = err(
            "content:\n  custom:\n    - { name: a, pattern: 'U+200D-U+200B', match: codepoints }",
        );
        assert!(
            matches!(
                &e,
                PolicyError::BadCodepoints {
                    reason: CodepointError::Reversed { .. },
                    ..
                }
            ),
            "{e:?}"
        );
        assert_eq!(e.guard(), Guard::Content);
        // 一个关键词不是正则：`f(` 当关键词没问题
        serde_yaml_ng::from_str::<Security>(
            "content:\n  custom:\n    - { name: a, pattern: 'f(' }",
        )
        .unwrap()
        .check()
        .unwrap();
    }

    #[test]
    fn a_label_written_as_the_default_is_the_same_as_none() {
        // 管理界面把出厂的标签显式填在框里，拼回整份策略存回来，不该和没写的不一样
        let written: RedactPolicy =
            serde_json::from_str(r#"{"custom":[{"name":"a","pattern":"x","label":"SECRET"}]}"#)
                .unwrap();
        let bare: RedactPolicy =
            serde_json::from_str(r#"{"custom":[{"name":"a","pattern":"x"}]}"#).unwrap();
        assert_eq!(written, bare);
        assert_eq!(written.custom[0].label, None);
        assert_eq!(
            serde_json::to_string(&written).unwrap(),
            r#"{"custom":[{"name":"a","pattern":"x"}]}"#
        );
    }

    #[test]
    fn labels_are_capital_letters_digits_and_underscores() {
        for ok in ["SECRET", "PROJECT", "ID_2", "A", "ABCDEFGHIJKLMNOPQRSTUVWX"] {
            assert!(label_ok(ok), "{ok}");
        }
        for bad in [
            "",
            "project",
            "2FA",
            "_X",
            "A-B",
            "A B",
            "ABCDEFGHIJKLMNOPQRSTUVWXY",
            "É",
        ] {
            assert!(!label_ok(bad), "{bad}");
        }
        assert_eq!(placeholder_label("PROJECT"), "TW_PROJECT");
    }

    #[test]
    fn the_redact_policy_reaches_the_rules_with_labels() {
        let p: RedactPolicy = serde_yaml_ng::from_str(
            "enable: [internal-ip]\ndisable: [jwt]\ncustom:\n  - { name: 项目号, pattern: 'PRJ-\\d{6}', label: PROJECT }\n  - { name: 停用的, pattern: zzz, disabled: true }\n  - { name: 不写标签, pattern: 'corp_[a-z]{4}' }\n",
        )
        .unwrap();
        let rs = p.rules().unwrap();
        assert!(rs.is_on("internal-ip") && !rs.is_on("jwt") && rs.is_on("anthropic-api-key"));
        let text = "PRJ-123456 corp_abcd zzz";
        let hits = crate::redact::rules::scan_plain(text, &rs);
        let r = crate::redact::replace::apply(
            text,
            &hits,
            crate::redact::replace::Ledger::new(crate::redact::replace::Scheme::SECRET),
        );
        assert_eq!(r.text, "<<TW_PROJECT_1>> <<TW_SECRET_1>> zzz");
    }

    #[test]
    fn the_tool_policy_reaches_the_rules() {
        let p = ToolPolicy {
            disable: vec!["chmod-777".to_string()],
            actions: [
                ("rm-rf-root".to_string(), ToolAction::Cut),
                ("curl-pipe-sh".to_string(), ToolAction::Record),
            ]
            .into(),
            custom: vec![
                CustomToolRule {
                    name: "删除集群资源".to_string(),
                    pattern: r"kubectl\s+delete".to_string(),
                    action: ToolAction::Cut,
                    disabled: false,
                },
                CustomToolRule {
                    name: "停用的".to_string(),
                    pattern: "zzz".to_string(),
                    action: ToolAction::Cut,
                    disabled: true,
                },
            ],
            ..Default::default()
        };
        let rs = p.rules().unwrap();
        let high = |id: &str| rs.rules.iter().find(|r| r.id == id).map(|r| r.high);
        assert_eq!(high("chmod-777"), None, "停用的还在");
        assert_eq!(high("rm-rf-root"), Some(true));
        assert_eq!(high("curl-pipe-sh"), Some(false));
        assert_eq!(high("base64-decode-exec"), Some(true), "没改的照出厂");
        assert_eq!(high("删除集群资源"), Some(true));
        assert_eq!(high("停用的"), None, "停用的自定义规则还在");
        assert!(p.one_builtin("rm-rf-root").unwrap().rules[0].high);
        assert!(p.one_builtin("chmod-777").is_some(), "停用的也能单独试");
        assert!(p.one_builtin("nope").is_none());
    }

    #[test]
    fn the_content_policy_reaches_the_rules() {
        let p: ContentPolicy = serde_yaml_ng::from_str(
            "enable: [jailbreak, zero-width]\ndisable: [ignore-all-previous]\nactions:\n  jailbreak: record\n  unicode-tags: block\ncustom:\n  - name: 内部代号\n    pattern: project-x\n  - name: 正则\n    pattern: 'secret\\s+plan'\n    match: regex\n    action: block\n  - name: 码位\n    pattern: 'U+E000-U+F8FF'\n    match: codepoints\n    action: strip\n  - name: 停用的\n    pattern: zzz\n    disabled: true\n",
        )
        .unwrap();
        p.check().unwrap();
        assert_eq!(p.custom[0].matching, ContentMatch::Contains, "不写就是子串");
        assert_eq!(p.custom[0].action, ContentAction::Record, "不写就是仅记录");
        let rs = p.rules().unwrap();
        use crate::content::Action;
        let action = |id: &str| rs.rules.iter().find(|r| r.id == id).map(|r| r.action);
        assert_eq!(action("ignore-previous-instructions"), Some(Action::Block));
        assert_eq!(action("ignore-all-previous"), None, "停用的还在");
        assert_eq!(
            action("jailbreak"),
            Some(Action::Record),
            "改过的没按改过的走"
        );
        assert_eq!(action("unicode-tags"), Some(Action::Block));
        assert_eq!(action("bidi-controls"), Some(Action::Strip), "出厂就是删除");
        assert_eq!(
            action("zero-width"),
            Some(Action::Strip),
            "打开的出厂关着的"
        );
        assert_eq!(action("private-use"), None);
        assert_eq!(action("act-as"), None, "出厂关着的开了");
        assert_eq!(action("system-prompt"), None);
        assert_eq!(action("内部代号"), Some(Action::Record));
        assert_eq!(action("正则"), Some(Action::Block));
        assert_eq!(action("码位"), Some(Action::Strip));
        assert_eq!(action("停用的"), None);
        assert!(p.one_builtin("act-as").is_some(), "关着的也能单独试");
        // 出厂写的 warn / log 是仅记录
        let b = content::builtin("act-as").unwrap();
        assert_eq!(b.action, content::Action::Warn);
        assert_eq!(ContentAction::factory(b), ContentAction::Record);
    }

    #[test]
    fn every_builtin_id_the_policy_accepts_is_one_the_engines_know() {
        let mut s = Security::default();
        s.redact.enable = crate::redact::rules::BUILTINS
            .iter()
            .map(|b| b.id.to_string())
            .collect();
        s.inspect_tools.actions = tool_rules::builtin()
            .dangerous
            .iter()
            .map(|r| (r.id.clone(), ToolAction::Cut))
            .collect();
        s.content.actions = content::builtins()
            .iter()
            .map(|b| (b.id.clone(), ContentAction::Strip))
            .collect();
        s.check().unwrap();
        assert!(s.redact.rules().is_ok() && s.inspect_tools.rules().is_ok());
        assert_eq!(
            s.content.rules().unwrap().rules.len(),
            content::builtins()
                .iter()
                .filter(|b| b.on_by_default)
                .count()
        );
    }
}
