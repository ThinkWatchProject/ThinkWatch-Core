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
//!     actions: { email: record }   # 只写和出厂不同的；placeholder | record
//!     custom:
//!       - name: 内部项目号          # 名字也是标识，同一项里不许重名
//!         pattern: 'PRJ-\d{6}'
//!         match: regex             # contains | regex；不写 = contains（按原样找这段文字）
//!         case_sensitive: true     # 可选；不写 = 不分大小写
//!         label: PROJECT           # 可选；占位符 <<TW_PROJECT_1>>；不写 = SECRET
//!         action: record           # 可选；placeholder | record；不写 = placeholder
//!         disabled: true           # 可选
//!     exceptions:                  # 例外值：这几个值在任何规则下都不换、不记（最多 500 个）
//!       - sha256: 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
//!         masked: sk-an…7f9c       # 加入时打码后的样子，只给人看
//!         rule: openai-api-key     # 最先认出它的规则；手动加入、没有规则认得时不写
//!         added_at: 1767225600000  # 加入的时刻，毫秒
//!   inspect_tools:
//!     mode: observe
//!     actions: { rm-rf-root: record }   # 只写和出厂不同的；cut | record
//!     custom:
//!       - { name: 删除集群资源, pattern: 'kubectl\s+delete', action: cut }
//!   content:
//!     mode: observe
//!     enable: [zero-width]
//!     actions: { unicode-tags: block }  # 只写和出厂不同的；block | strip | replace | record
//!     custom:
//!       - name: 内部代号
//!         pattern: project-x
//!         scope: [request, response]  # request | response；不写 = [request]
//!         match: contains          # contains | regex | codepoints；不写 = contains
//!         case_sensitive: true     # 可选；不写 = 不分大小写（码位不论大小写）
//!         action: replace          # block | strip | replace | record；不写 = record
//!         replacement: '[已隐去]'   # 可选；动作是替换时换成它；不写 = ***
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
//! 第三档（界面上叫「开启」）下每条规则按自己的动作处理：出站脱敏替换为占位符或仅记录，
//! 工具调用审查切断或仅记录，内容过滤拒绝、删除、替换或仅记录。观察档（界面上叫「监控」）下
//! 每条规则都只记录。
//!
//! # 回答正文
//!
//! 内容过滤的自定义规则可以连模型回答的正文一起查（`scope` 含 `response`，见
//! [`crate::content::answer`]）。回答是流着发出去的，命中之前的字已经到了客户端，所以查回答
//! 的规则不能拒绝；码位规则也只查请求 —— 看不见的字符是注入的手段，模型自己的回答里没有
//! 那条路。这两种写法是配置错误。
//!
//! # 规则：内置的开关 + 自定义的
//!
//! **加法加停用，不是整份替换。**用户复制一份内置规则再改两条之后，他那份就永远停在
//! 复制的那一刻了 —— 我们后来加的每一条都到不了他那里，而他不会察觉。所以内置规则只
//! 记「改过出厂开关、改过处置的那几条」，自定义规则另起一个列表。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::content::{self, CodepointError, Codepoints};
use crate::redact::rules::{Pattern, RuleSet};
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

words!(Mode {
    Off = "off",
    Observe = "observe",
    Enforce = "enforce",
});

impl Mode {
    pub fn detects(&self) -> bool {
        !matches!(self, Mode::Off)
    }
    /// 会不会改变请求的去向或内容。**观察档永远是 false。**
    pub fn acts(&self) -> bool {
        matches!(self, Mode::Enforce)
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

words!(Guard {
    Redact = "redact",
    InspectTools = "inspect_tools",
    Content = "content",
});

impl Guard {
    /// 错误信息里怎么称呼这一项的规则：`redaction` / `tool-call` / `content`
    pub fn rule_noun(self) -> &'static str {
        match self {
            Guard::Redact => "redaction",
            Guard::InspectTools => "tool-call",
            Guard::Content => "content",
        }
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

words!(ToolAction {
    Cut = "cut",
    Record = "record",
});

impl ToolAction {
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
    /// 删除：把命中的字删掉，不留标记，再照常发出
    Strip,
    /// 替换：把命中的字换成规则写的替换文本（不写是 `***`）再发出。**不可还原**：回答里
    /// 出现的替换文本不换回去
    Replace,
    /// 仅记录。**手写的规则不写就是它**（「零值 = 安全」）
    #[default]
    Record,
}

words!(ContentAction {
    Block = "block",
    Strip = "strip",
    Replace = "replace",
    Record = "record",
});

impl ContentAction {
    /// 一条内置规则出厂时的处置
    pub fn factory(b: &content::Builtin) -> Self {
        Self::of(b.action)
    }
    /// 引擎里的那个处置在策略里的写法
    pub fn of(a: content::Action) -> Self {
        match a {
            content::Action::Block => ContentAction::Block,
            content::Action::Strip => ContentAction::Strip,
            content::Action::Replace => ContentAction::Replace,
            content::Action::Record => ContentAction::Record,
        }
    }
    /// 引擎里的那个处置
    pub fn engine(self) -> content::Action {
        match self {
            ContentAction::Block => content::Action::Block,
            ContentAction::Strip => content::Action::Strip,
            ContentAction::Replace => content::Action::Replace,
            ContentAction::Record => content::Action::Record,
        }
    }
}

/// 一条脱敏规则命中之后，在第三档下做什么。观察档一律只记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RedactAction {
    /// 换成占位符再发出，回答里出现这个占位符时换回原值。**不写就是它**
    #[default]
    Placeholder,
    /// 只记录：原样发出，记一条安全记录
    Record,
}

words!(RedactAction {
    Placeholder = "placeholder",
    Record = "record",
});

/// 一条内容规则查哪一边。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "lowercase")]
pub enum ContentScope {
    /// 调用方发来的请求：用户消息和其中的工具结果
    Request,
    /// 模型回答的正文：客户端看得到的文字（连同 Codex 自由格式工具的输入），不含工具调用
    /// 和思考过程
    Response,
}

words!(ContentScope {
    Request = "request",
    Response = "response",
});

/// 内容规则不写 `scope` 时查的那一边：只查请求
pub fn request_only() -> Vec<ContentScope> {
    vec![ContentScope::Request]
}

fn is_request_only(s: &[ContentScope]) -> bool {
    s == [ContentScope::Request]
}

/// 「替换」不写替换文本时换成的：三个星号。拼起来写：产品文字的检查把连着的两个星号当成
/// Markdown 的加粗
pub const DEFAULT_REPLACEMENT: &str = concat!("*", "*", "*");

fn default_replacement() -> String {
    DEFAULT_REPLACEMENT.to_string()
}

fn is_default_replacement(s: &str) -> bool {
    s == DEFAULT_REPLACEMENT
}

/// 一条内容规则怎么认。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "lowercase")]
pub enum ContentMatch {
    /// 子串，按原样找（`.`、`*` 没有特殊含义）。**不写就是它**：关键词是最常见的写法
    #[default]
    Contains,
    /// 正则
    Regex,
    /// 码位：`U+200B`、`U+E0000–U+E007F`，多个之间用逗号隔开
    Codepoints,
}

words!(ContentMatch {
    Contains = "contains",
    Regex = "regex",
    Codepoints = "codepoints",
});

impl ContentMatch {
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
    /// 要找的那段文字，或者正则（按 `match`）
    pub pattern: String,
    /// `contains`：按原样找这段文字（**不写就是它**：用户填的多半就是那个词，`.`、`*`
    /// 不该有特殊含义）；`regex`：正则。码位只有内容过滤认
    #[serde(rename = "match", default, skip_serializing_if = "is_default")]
    pub matching: ContentMatch,
    /// 分大小写。不写是不分
    #[serde(default, skip_serializing_if = "is_false")]
    pub case_sensitive: bool,
    /// 占位符里的标签：`PROJECT` 换成 `<<TW_PROJECT_1>>`。不写是 [`DEFAULT_LABEL`]；写的
    /// 就是它的，读进来也是不写（见 `label_or_default`）
    #[serde(
        default,
        deserialize_with = "label_or_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub label: Option<String>,
    /// 第三档下做什么：换成占位符（不写就是它），或者只记录
    #[serde(default, skip_serializing_if = "is_default")]
    pub action: RedactAction,
    /// 停用。**规则原样留着**，打开就回来
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
}

/// 出站脱敏：请求发出前，在整个请求里按规则查找凭据和个人信息。第三档下按各条规则的动作
/// 换成占位符或者只记录。
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
    /// 内置规则在第三档下做什么，**只写和出厂不一样的**：`email: record`
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub actions: BTreeMap<String, RedactAction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom: Vec<CustomRedactRule>,
    /// 例外值（见 [`crate::redact::exceptions`]）：值的 SHA-256 在这里的命中，哪条规则的都不换、
    /// 不记。最多 [`EXCEPTIONS_MAX`] 个
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exceptions: Vec<RedactException>,
}

pub use crate::redact::exceptions::EXCEPTIONS_MAX;

/// 一个例外值。**只存哈希和打码后的样子，不存原值**：例外表随配置导出、同步、备份。
///
/// 规则是保存的凭据（`thinkwatch-saved-secret`）的读得进来、**但不生效**：保存的凭据本就该换，
/// 界面上也不给它「列为例外」。不当成配置错误 —— 那样的话一份手改过的配置会整份读不进来，
/// 而它要的效果（不生效）本来就是对的。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct RedactException {
    /// 值（解开转义之后的样子）的 SHA-256，64 个小写十六进制字符
    pub sha256: String,
    /// 加入时打码后的样子（和安全日志里的一样），只给人看是哪一个
    #[serde(default)]
    pub masked: String,
    /// 最先认出它的规则：内置规则的 id 或者自定义规则的名字。手动加入、没有规则认得它时是空的
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rule: String,
    /// 加入的时刻，毫秒
    #[serde(default)]
    pub added_at: i64,
}

/// 一个值加不进例外表。`code()` 是稳定的码。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExceptionError {
    #[error("the value is empty")]
    Empty,
    /// 是一项保存的凭据：它本就该换
    #[error("a saved secret cannot be an exception")]
    Saved,
    /// 现在的策略本身编不起来（绕过了校验写进去的）
    #[error(transparent)]
    Policy(#[from] PolicyError),
}

impl ExceptionError {
    pub fn code(&self) -> &'static str {
        match self {
            ExceptionError::Empty => "exception_value_empty",
            ExceptionError::Saved => "exception_saved_secret",
            ExceptionError::Policy(e) => e.code(),
        }
    }
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
    /// 查哪一边：请求、回答正文，或者两边都查。不写是只查请求
    #[serde(default = "request_only", skip_serializing_if = "is_request_only")]
    pub scope: Vec<ContentScope>,
    #[serde(rename = "match", default, skip_serializing_if = "is_default")]
    pub matching: ContentMatch,
    /// 分大小写。不写是不分；码位规则不论大小写
    #[serde(default, skip_serializing_if = "is_false")]
    pub case_sensitive: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    pub action: ContentAction,
    /// 动作是替换时换成的文字。不写是 [`DEFAULT_REPLACEMENT`]
    #[serde(
        default = "default_replacement",
        skip_serializing_if = "is_default_replacement"
    )]
    pub replacement: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
}

impl CustomContentRule {
    /// 查不查调用方发来的请求
    pub fn checks_requests(&self) -> bool {
        self.scope.contains(&ContentScope::Request)
    }
    /// 查不查模型回答的正文
    pub fn checks_answers(&self) -> bool {
        self.scope.contains(&ContentScope::Response)
    }
}

/// 内容过滤：调用方发来的正文里（连同工具结果）、或者模型回答的正文里出现了某个词、某种
/// 写法或者某些字符。第三档下规则各自拒绝、删除、替换或者仅记录。
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
    #[error(
        "custom redaction rule `{name}` matches by `{}`; redaction rules match by contains or \
         regex",
        .matching.slug()
    )]
    BadMatch {
        name: String,
        matching: ContentMatch,
    },
    #[error("custom content rule `{name}` checks neither requests nor answers; its scope is empty")]
    EmptyScope { name: String },
    #[error(
        "custom content rule `{name}` checks answers and blocks; an answer is already on its way \
         when a match is found, so a rule that checks answers strips, replaces or records"
    )]
    BlockInAnswer { name: String },
    #[error(
        "custom content rule `{name}` checks answers and matches by codepoints; code points are \
         checked in requests only"
    )]
    CodepointsInAnswer { name: String },
    #[error(
        "security.redact.exceptions has {count} values; at most {} are allowed",
        EXCEPTIONS_MAX
    )]
    TooManyExceptions { count: usize },
    #[error(
        "the exception `{sha256}` is not a SHA-256 written as 64 lowercase hexadecimal characters"
    )]
    BadExceptionHash { sha256: String },
    #[error("the exception `{sha256}` appears twice")]
    DuplicateException { sha256: String },
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
            PolicyError::BadMatch { .. } => "rule_match_bad",
            PolicyError::EmptyScope { .. } => "rule_scope_empty",
            PolicyError::BlockInAnswer { .. } => "rule_scope_block",
            PolicyError::CodepointsInAnswer { .. } => "rule_scope_codepoints",
            PolicyError::TooManyExceptions { .. } => "redact_exceptions_too_many",
            PolicyError::BadExceptionHash { .. } => "redact_exception_hash_bad",
            PolicyError::DuplicateException { .. } => "redact_exception_duplicate",
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
            PolicyError::BadCodepoints { .. }
            | PolicyError::EmptyScope { .. }
            | PolicyError::BlockInAnswer { .. }
            | PolicyError::CodepointsInAnswer { .. } => Guard::Content,
            PolicyError::BadLabel { .. }
            | PolicyError::BadMatch { .. }
            | PolicyError::TooManyExceptions { .. }
            | PolicyError::BadExceptionHash { .. }
            | PolicyError::DuplicateException { .. } => Guard::Redact,
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
        check_ids(
            g,
            self.enable
                .iter()
                .chain(&self.disable)
                .chain(self.actions.keys()),
            |id| crate::redact::rules::builtin(id).is_some(),
        )?;
        self.check_exceptions()
    }

    /// 例外表：不超过 [`EXCEPTIONS_MAX`] 个，每个都是我们写出去的那种哈希，不重复。
    /// **规则名不查**：它只是给人看的记录，那条自定义规则之后删了、改了名，例外照样是那个值
    pub fn check_exceptions(&self) -> Result<(), PolicyError> {
        if self.exceptions.len() > EXCEPTIONS_MAX {
            return Err(PolicyError::TooManyExceptions {
                count: self.exceptions.len(),
            });
        }
        let mut seen = std::collections::HashSet::new();
        for e in &self.exceptions {
            let Some(d) = crate::redact::exceptions::parse_hex(&e.sha256) else {
                return Err(PolicyError::BadExceptionHash {
                    sha256: e.sha256.clone(),
                });
            };
            if !seen.insert(d) {
                return Err(PolicyError::DuplicateException {
                    sha256: e.sha256.clone(),
                });
            }
        }
        Ok(())
    }

    /// 编好的例外表，给规则集用（[`RuleSet::with_exceptions`]）。**保存的凭据那条规则的不算**
    /// （见 [`RedactException`]），写错的哈希跳过（校验时就拒绝了，到不了这里）。一个都不剩
    /// 是 `None`
    pub fn exception_set(&self) -> Option<std::sync::Arc<crate::redact::exceptions::Exceptions>> {
        crate::redact::exceptions::Exceptions::new(
            self.exceptions
                .iter()
                .filter(|e| e.rule != crate::redact::exceptions::SAVED_RULE)
                .filter_map(|e| crate::redact::exceptions::parse_hex(&e.sha256)),
        )
        .map(std::sync::Arc::new)
    }

    /// 这个哈希已经在例外表里了（不论是哪条规则加的）
    pub fn has_exception(&self, sha256: &str) -> bool {
        self.exceptions.iter().any(|e| e.sha256 == sha256)
    }

    /// 手动加入的一个值写成例外的样子（R5：用于还没出现在日志里的测试值）。**只返回哈希和
    /// 打码后的样子，原值不出这个函数**。
    ///
    /// 两头的空白去掉（粘贴时常带一个换行）。按现在的规则（不带例外）找一遍，有规则认得出
    /// 整个值的，按它打码、记下它；现在开着的规则都认不出时再按全部内置规则找一遍；都认不出
    /// 的 `rule` 是空的，打码按 [`crate::redact::rules::masked_unknown`]。
    ///
    /// `saved` 是网关手里那一份保存的凭据：是其中一项的不收（[`ExceptionError::Saved`]）。
    pub fn exception_of(
        &self,
        value: &str,
        saved: Option<std::sync::Arc<crate::redact::rules::Saved>>,
        added_at: i64,
    ) -> Result<RedactException, ExceptionError> {
        use crate::redact::rules::{BUILTINS, masked, masked_unknown};
        let value = value.trim();
        if value.is_empty() {
            return Err(ExceptionError::Empty);
        }
        if saved.as_ref().is_some_and(|s| s.source_of(value).is_some()) {
            return Err(ExceptionError::Saved);
        }
        let whole = |set: &RuleSet| {
            crate::redact::flow::hits_plain(value, set)
                .into_iter()
                .find(|h| h.bytes == (0..value.len()))
                .map(|h| h.rule)
        };
        let current = self.rules()?.without_exceptions().with_saved_set(saved);
        let rule = whole(&current).or_else(|| {
            let ids: Vec<&str> = BUILTINS.iter().map(|b| b.id).collect();
            whole(&RuleSet::only(&ids))
        });
        if rule
            .as_ref()
            .is_some_and(|r| !crate::redact::exceptions::exceptable(r))
        {
            return Err(ExceptionError::Saved);
        }
        Ok(RedactException {
            sha256: crate::redact::exceptions::sha256_hex(value),
            masked: rule
                .as_ref()
                .map_or_else(|| masked_unknown(value), |r| masked(r, value)),
            rule: rule.map(|r| r.id().to_string()).unwrap_or_default(),
            added_at,
        })
    }

    /// 这条内置规则在第三档下做什么（改过的按改过的）。出厂一律换成占位符
    pub fn builtin_action(&self, b: &crate::redact::rules::Builtin) -> RedactAction {
        self.actions.get(b.id).copied().unwrap_or_default()
    }

    /// 这条内置规则现在开着吗
    pub fn builtin_on(&self, b: &crate::redact::rules::Builtin) -> bool {
        if b.on_by_default {
            !self.disable.iter().any(|x| x == b.id)
        } else {
            self.enable.iter().any(|x| x == b.id)
        }
    }

    /// 这一份策略下的规则：内置的按开关，再加上启用着的自定义规则（各带自己的标签）。动作是
    /// 仅记录的那几条照样找、照样报，只是不换（见 [`RuleSet::records`]）。
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
                .with_rule(&c.name, redact_pattern(c)?, label.as_deref())
                .map_err(|e| PolicyError::BadPattern {
                    guard: Guard::Redact,
                    name: e.name,
                    detail: e.detail,
                })?;
            if c.action == RedactAction::Record {
                set = set.recording(crate::redact::rules::Rule::Custom(c.name.as_str().into()));
            }
        }
        for (id, a) in &self.actions {
            if *a == RedactAction::Record
                && let Some(b) = crate::redact::rules::builtin(id)
            {
                set = set.recording(crate::redact::rules::Rule::Builtin(b.id));
            }
        }
        Ok(set.with_exceptions(self.exception_set()))
    }

    /// 只有一条内置规则，**不管它开没开**，动作按这份策略走。「试一条」用它。不是内置规则的
    /// id 是 `None`
    pub fn one_builtin(&self, id: &str) -> Option<RuleSet> {
        let b = crate::redact::rules::builtin(id)?;
        // 例外跨规则：试一条规则时也照样不换
        let set = RuleSet::only(&[b.id]).with_exceptions(self.exception_set());
        Some(match self.builtin_action(b) {
            RedactAction::Record => set.recording(crate::redact::rules::Rule::Builtin(b.id)),
            RedactAction::Placeholder => set,
        })
    }
}

/// 一条自定义脱敏规则写的样子和怎么认。码位不是它认的写法
fn redact_pattern(c: &CustomRedactRule) -> Result<Pattern, PolicyError> {
    redact_match(&c.pattern, c.matching, c.case_sensitive).ok_or_else(|| PolicyError::BadMatch {
        name: c.name.clone(),
        matching: c.matching,
    })
}

/// 出站脱敏认的写法：按原样找、正则。码位是 `None`
pub fn redact_match(
    pattern: &str,
    matching: ContentMatch,
    case_sensitive: bool,
) -> Option<Pattern> {
    let regex = match matching {
        ContentMatch::Contains => false,
        ContentMatch::Regex => true,
        ContentMatch::Codepoints => return None,
    };
    Some(Pattern {
        text: pattern.to_string(),
        regex,
        case_sensitive,
    })
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
    redact_pattern(c)?
        .compile(&c.name)
        .map_err(|e| PolicyError::BadPattern {
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
        Ok(content::Rules::of(rules))
    }

    /// 只有一条内置规则，**不管它开没开**，处置按这份策略走。「试一条」用它。不是内置
    /// 规则的 id 是 `None`
    pub fn one_builtin(&self, id: &str) -> Option<content::Rules> {
        let b = content::builtin(id)?;
        Some(content::Rules::of(vec![self.builtin_rule(b)]))
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
    let name = || c.name.clone();
    if c.scope.is_empty() {
        return Err(PolicyError::EmptyScope { name: name() });
    }
    // 回答是流着发出去的：命中之前的字已经到了客户端，拒绝不了。码位规则只查请求
    if c.checks_answers() && c.action == ContentAction::Block {
        return Err(PolicyError::BlockInAnswer { name: name() });
    }
    if c.checks_answers() && c.matching == ContentMatch::Codepoints {
        return Err(PolicyError::CodepointsInAnswer { name: name() });
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
        case_sensitive: c.case_sensitive,
        sides: content::Sides {
            request: c.checks_requests(),
            response: c.checks_answers(),
        },
        replacement: &c.replacement,
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
  actions: { email: record }
  custom:
    - name: 内部项目号
      pattern: 'PRJ-\d{6}'
      label: PROJECT
      action: record
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
    - name: 项目代号
      pattern: 北极星
      scope: [request, response]
      match: contains
      case_sensitive: false
      action: replace
      replacement: "***"
"#;

    #[test]
    fn the_documented_shape_reads_and_round_trips_through_yaml_and_json() {
        let s: Security = serde_yaml_ng::from_str(EXAMPLE).unwrap();
        s.check().unwrap();
        assert_eq!(s.redact.custom[0].label.as_deref(), Some("PROJECT"));
        assert_eq!(s.redact.custom[0].action, RedactAction::Record);
        assert_eq!(s.redact.actions.get("email"), Some(&RedactAction::Record));
        assert_eq!(s.content.custom[0].action, ContentAction::Strip);
        assert_eq!(
            s.content.custom[0].scope,
            [ContentScope::Request],
            "不写就是只查请求"
        );
        assert_eq!(s.content.custom[0].replacement, DEFAULT_REPLACEMENT);
        let both = &s.content.custom[1];
        assert!(both.checks_requests() && both.checks_answers());
        assert_eq!(both.action, ContentAction::Replace);
        assert_eq!(
            s.content.actions.get("unicode-tags"),
            Some(&ContentAction::Block)
        );
        // YAML 和 JSON 都写得出、读得回，读回来一样
        let yaml = serde_yaml_ng::to_string(&s).unwrap();
        assert_eq!(serde_yaml_ng::from_str::<Security>(&yaml).unwrap(), s);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Security>(&json).unwrap(), s);
        // 出厂值不写回：observe、空名单、disabled: false、match: contains、只查请求、`***`、
        // 不分大小写都不在
        for gone in [
            "observe",
            "enable: []",
            "disabled: false",
            "match:",
            "replacement",
            "case_sensitive",
            "placeholder",
        ] {
            assert!(!yaml.contains(gone), "{gone} 写回去了：{yaml}");
        }
        assert_eq!(yaml.matches("scope:").count(), 1, "{yaml}");
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
            "redact:\n  custom:\n    - { name: a, pattern: b, action: strip }",
            "redact:\n  actions: { email: cut }",
            "content:\n  custom:\n    - { name: a, pattern: b, scope: [answer] }",
            "content:\n  custom:\n    - { name: a, pattern: b, scope: response }",
            "hidden_text:\n  mode: off",
            "output_limit:\n  mode: off",
        ] {
            assert!(serde_yaml_ng::from_str::<Security>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn slugs_round_trip() {
        for &m in Mode::ALL {
            assert_eq!(Mode::from_slug(m.slug()), Some(m));
            let back: Mode = serde_json::from_value(m.slug().into()).unwrap();
            assert_eq!(back, m);
        }
        for &g in Guard::ALL {
            assert_eq!(Guard::from_slug(g.slug()), Some(g));
            assert_eq!(serde_json::to_value(g).unwrap(), g.slug());
        }
        for a in [ToolAction::Cut, ToolAction::Record] {
            assert_eq!(ToolAction::from_slug(a.slug()), Some(a));
        }
        for &a in RedactAction::ALL {
            assert_eq!(RedactAction::from_slug(a.slug()), Some(a));
            assert_eq!(serde_json::to_value(a).unwrap(), a.slug());
        }
        for &a in ContentScope::ALL {
            assert_eq!(serde_json::to_value(a).unwrap(), a.slug());
        }
        for a in [
            ContentAction::Block,
            ContentAction::Strip,
            ContentAction::Replace,
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
                "redact:\n  custom:\n    - { name: a, pattern: '(', match: regex }",
                "rule_pattern_bad",
            ),
            (
                "redact:\n  custom:\n    - { name: a, pattern: x, match: codepoints }",
                "rule_match_bad",
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
            ("redact:\n  actions: { jwtt: record }", "unknown_rule"),
            (
                "content:\n  custom:\n    - { name: a, pattern: x, scope: [] }",
                "rule_scope_empty",
            ),
            (
                "content:\n  custom:\n    - { name: a, pattern: x, scope: [response], action: block }",
                "rule_scope_block",
            ),
            (
                "content:\n  custom:\n    - { name: a, pattern: 'U+200B', match: codepoints, scope: [request, response] }",
                "rule_scope_codepoints",
            ),
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
            "enable: [internal-ip]\ndisable: [jwt]\ncustom:\n  - { name: 项目号, pattern: 'PRJ-\\d{6}', match: regex, label: PROJECT }\n  - { name: 停用的, pattern: zzz, disabled: true }\n  - { name: 不写标签, pattern: 'corp_[a-z]{4}', match: regex }\n",
        )
        .unwrap();
        let rs = p.rules().unwrap();
        assert!(rs.is_on("internal-ip") && !rs.is_on("jwt") && rs.is_on("anthropic-api-key"));
        let text = "PRJ-123456 corp_abcd zzz";
        let hits = crate::redact::rules::scan_plain(text, &rs);
        let r = crate::redact::replace::apply_text(
            text,
            &hits,
            crate::redact::replace::Ledger::new(crate::redact::replace::Scheme::SECRET),
        );
        assert_eq!(r.text, "<<TW_PROJECT_1>> <<TW_SECRET_1>> zzz");
    }

    /// 不写 `match` 是按原样找：用户填的多半就是那个词，`.`、`*` 不该有特殊含义。不写
    /// `case_sensitive` 是不分大小写
    #[test]
    fn a_redact_rule_looks_for_its_text_as_written_unless_it_says_regex() {
        let rules = |yaml: &str| {
            serde_yaml_ng::from_str::<RedactPolicy>(yaml)
                .unwrap()
                .rules()
                .unwrap()
        };
        let found = |rs: &RuleSet, text: &str| -> Vec<String> {
            crate::redact::rules::scan_plain(text, rs)
                .iter()
                .filter(|h| h.rule.custom())
                .map(|h| text[h.bytes.clone()].to_string())
                .collect()
        };
        let plain = rules("custom:\n  - { name: a, pattern: 'a.b*' }\n");
        assert_eq!(found(&plain, "a.b* A.B* axbb"), ["a.b*", "A.B*"]);
        let exact = rules("custom:\n  - { name: a, pattern: 'a.b*', case_sensitive: true }\n");
        assert_eq!(found(&exact, "a.b* A.B* axbb"), ["a.b*"]);
        let regex = rules("custom:\n  - { name: a, pattern: 'a.b+', match: regex }\n");
        assert_eq!(found(&regex, "a.b AXBB"), ["a.b", "AXBB"]);
        let regex = rules(
            "custom:\n  - { name: a, pattern: 'a.b+', match: regex, case_sensitive: true }\n",
        );
        assert_eq!(found(&regex, "a.b AXBB"), ["a.b"]);
    }

    /// 动作是仅记录的脱敏规则照样找，只是不换：内置的按 `actions`，自定义的按 `action`
    #[test]
    fn record_only_redaction_rules_are_found_but_not_replaced() {
        let p: RedactPolicy = serde_yaml_ng::from_str(
            "actions: { anthropic-api-key: record }\ncustom:\n  - { name: 项目号, pattern: 'PRJ-\\d{6}', match: regex, action: record }\n  - { name: 工号, pattern: 'E-\\d{4}', match: regex }\n",
        )
        .unwrap();
        p.check().unwrap();
        let rs = p.rules().unwrap();
        use crate::redact::rules::Rule;
        assert!(rs.records(&Rule::Builtin("anthropic-api-key")));
        assert!(rs.records(&Rule::Custom("项目号".into())));
        assert!(!rs.records(&Rule::Custom("工号".into())));
        assert!(!rs.records(&Rule::Builtin("github-token")));
        let key = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";
        let body = format!(r#"{{"messages":[{{"content":"{key} PRJ-123456 E-1234"}}]}}"#);
        // 找到三处、报三处
        let shape = crate::redact::flow::Shape::Generate(tw_dialect::ir::Dialect::Anthropic);
        let found = crate::redact::flow::find(Mode::Enforce, &rs, body.as_bytes(), shape);
        assert_eq!(found.len(), 3);
        // 只换了工号：只记录的原样发，也不进账本
        let l = crate::redact::flow::look_hits(
            Mode::Enforce,
            &rs,
            body.as_bytes(),
            shape,
            crate::redact::replace::Ledger::new(crate::redact::replace::Scheme::SECRET),
        );
        assert_eq!(l.ledger.len(), 1);
        assert_eq!(l.hits.as_ref().map(Vec::len), Some(3));
        let (out, ledger) =
            crate::redact::flow::replace(Mode::Enforce, &rs, body.clone().into(), &l.ledger, shape);
        let out = String::from_utf8(out.to_vec()).unwrap();
        assert!(out.contains(key) && out.contains("PRJ-123456"), "{out}");
        assert!(
            out.contains("<<TW_SECRET_1>>") && !out.contains("E-1234"),
            "{out}"
        );
        assert_eq!(ledger.len(), 1);
        // 单独试一条：动作照这份策略
        let one = p.one_builtin("anthropic-api-key").unwrap();
        assert!(one.records(&Rule::Builtin("anthropic-api-key")));
        assert!(p.one_builtin("nope").is_none());
    }

    /// 查哪一边、分不分大小写、换成什么，一直带到编好的规则上
    #[test]
    fn content_scope_case_and_replacement_reach_the_rules() {
        let p: ContentPolicy = serde_yaml_ng::from_str(
            "custom:\n  - { name: 两边, pattern: Polaris, scope: [request, response], case_sensitive: true, action: replace, replacement: '[X]' }\n  - { name: 回答, pattern: 'TKT-\\d+', match: regex, scope: [response], action: strip }\n",
        )
        .unwrap();
        p.check().unwrap();
        let rs = p.rules().unwrap();
        let r = |id: &str| rs.rules.iter().find(|r| r.id == id).unwrap();
        let both = r("两边");
        assert!(both.sides.request && both.sides.response && both.case_sensitive);
        assert_eq!(both.replacement, "[X]");
        assert_eq!(both.action, content::Action::Replace);
        let answer = r("回答");
        assert!(!answer.sides.request && answer.sides.response);
        assert!(r("ignore-previous-instructions").sides == content::Sides::default());
        assert!(content::answer::Answer::build(&rs).is_some());
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
        let b = content::builtin("act-as").unwrap();
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

    // ---------------------------------------------------------------- 例外值

    const KEY: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";
    const OTHER: &str = "ghp_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

    fn excepting(values: &[&str], extra: &str) -> RedactPolicy {
        let mut p: RedactPolicy = serde_yaml_ng::from_str(extra).unwrap();
        p.exceptions = values
            .iter()
            .map(|v| RedactException {
                sha256: crate::redact::exceptions::sha256_hex(v),
                masked: crate::redact::rules::masked_unknown(v),
                rule: String::new(),
                added_at: 1,
            })
            .collect();
        p.check().unwrap();
        p
    }

    fn body(text: &str) -> String {
        serde_json::json!({"messages": [{"role": "user", "content": text}]}).to_string()
    }

    const SHAPE: crate::redact::flow::Shape =
        crate::redact::flow::Shape::Generate(tw_dialect::ir::Dialect::Anthropic);

    /// R4：例外的是这个值，不是这条规则下的这个值 —— 另一条规则（这里是一条自定义规则）认出
    /// 同一个值也不换、不报；别的值照常
    #[test]
    fn an_exception_holds_across_every_rule_in_both_modes() {
        let p = excepting(
            &[KEY],
            "custom:\n  - { name: 同一个值, pattern: 'sk-ant-api03-A+', match: regex }\n",
        );
        let rs = p.rules().unwrap();
        assert!(rs.exceptions().is_some_and(|e| e.len() == 1));
        let b = body(&format!("{KEY} 和 {OTHER}"));
        for mode in [Mode::Observe, Mode::Enforce] {
            let found = crate::redact::flow::find(mode, &rs, b.as_bytes(), SHAPE);
            assert_eq!(found.len(), 1, "{mode}: {found:?}");
            assert_eq!(found[0].rule.id(), "github-personal-token");
        }
        let l = crate::redact::flow::look_hits(
            Mode::Enforce,
            &rs,
            b.as_bytes(),
            SHAPE,
            crate::redact::replace::Ledger::new(crate::redact::replace::Scheme::SECRET),
        );
        let (out, ledger) =
            crate::redact::flow::replace(Mode::Enforce, &rs, b.clone().into(), &l.ledger, SHAPE);
        let out = String::from_utf8(out.to_vec()).unwrap();
        assert!(out.contains(KEY) && !out.contains(OTHER), "{out}");
        // 还原照旧：换掉的那一个换回来，例外的那个原样
        let echo = format!("{KEY} <<TW_SECRET_1>>");
        assert_eq!(
            crate::redact::replace::restore_text(&echo, &ledger),
            format!("{KEY} {OTHER}")
        );
        // 没有例外时认得出它（两条规则认到同一处，合成一处报）
        let mut none = p.clone();
        none.exceptions.clear();
        let found =
            crate::redact::flow::find(Mode::Observe, &none.rules().unwrap(), b.as_bytes(), SHAPE);
        assert_eq!(found.len(), 2, "{found:?}");
        // 只剩那条自定义规则时：它认得出，例外照样挡住它
        let mut custom_only = p.clone();
        custom_only.disable = vec!["anthropic-api-key".into()];
        let rs2 = custom_only.rules().unwrap();
        assert!(
            crate::redact::flow::find(Mode::Enforce, &rs2, body(KEY).as_bytes(), SHAPE).is_empty()
        );
        custom_only.exceptions.clear();
        let found = crate::redact::flow::find(
            Mode::Enforce,
            &custom_only.rules().unwrap(),
            body(KEY).as_bytes(),
            SHAPE,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].rule.id(), "同一个值");
        // 试一条规则时也一样
        assert!(
            crate::redact::rules::scan_plain(KEY, &p.one_builtin("anthropic-api-key").unwrap())
                .is_empty()
        );
        // 解码过的正文（插件那一层、企业版）同样不认
        assert!(crate::redact::rules::scan_text(KEY, &rs).is_empty());
    }

    /// 请求体里写着转义的值，按解开之后的样子比
    #[test]
    fn an_exception_matches_the_value_as_written_not_as_escaped() {
        let value = "line1\"quoted\"-PRJ-123456";
        let p = excepting(
            &[value],
            "custom:\n  - { name: 项目号, pattern: 'line1\"quoted\"-PRJ-\\d{6}', match: regex }\n",
        );
        let b = body(value);
        assert!(b.contains("\\\""), "{b}");
        assert!(
            crate::redact::flow::find(Mode::Observe, &p.rules().unwrap(), b.as_bytes(), SHAPE)
                .is_empty()
        );
    }

    /// 保存的凭据不能列为例外：配置里写着也不生效
    #[test]
    fn a_saved_secret_is_never_an_exception() {
        use crate::redact::rules::{SavedSecret, SavedSource, SavedSourceKind};
        let saved = "0123456789abcdef0123456789abcdef";
        let mut p = excepting(&[saved], "{}");
        p.exceptions[0].rule = crate::redact::exceptions::SAVED_RULE.into();
        p.check().unwrap();
        assert!(p.exception_set().is_none(), "这一条不算");
        let secret = SavedSecret {
            value: saved.into(),
            source: SavedSource {
                kind: SavedSourceKind::UpstreamKey,
                name: "A".into(),
            },
            hosts: vec![],
        };
        let rs = p.rules().unwrap().with_saved(vec![secret.clone()]);
        let found = crate::redact::flow::find(Mode::Observe, &rs, body(saved).as_bytes(), SHAPE);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].sha256, None, "保存的凭据连哈希都不留");
        // 规则写的不是它、值却是一项保存的凭据：哈希在表里也照样认
        let p = excepting(&[saved], "{}");
        let rs = p.rules().unwrap().with_saved(vec![secret.clone()]);
        let found = crate::redact::flow::find(Mode::Observe, &rs, body(saved).as_bytes(), SHAPE);
        assert_eq!(found.len(), 1, "{found:?}");
        // 也加不进去
        let set = crate::redact::rules::Saved::new(vec![secret]).map(std::sync::Arc::new);
        assert_eq!(
            RedactPolicy::default().exception_of(saved, set, 0),
            Err(ExceptionError::Saved)
        );
    }

    #[test]
    fn the_exception_list_is_checked() {
        let one = |sha256: &str| RedactException {
            sha256: sha256.into(),
            masked: String::new(),
            rule: String::new(),
            added_at: 0,
        };
        let mut p = RedactPolicy {
            exceptions: (0..EXCEPTIONS_MAX)
                .map(|i| one(&crate::redact::exceptions::sha256_hex(&i.to_string())))
                .collect(),
            ..Default::default()
        };
        p.check().unwrap();
        p.exceptions
            .push(one(&crate::redact::exceptions::sha256_hex("x")));
        let e = p.check().unwrap_err();
        assert_eq!(e.code(), "redact_exceptions_too_many");
        assert_eq!(e.guard(), Guard::Redact);
        let h = crate::redact::exceptions::sha256_hex("a");
        for (list, code) in [
            (vec![one("abc")], "redact_exception_hash_bad"),
            (vec![one(&h.to_uppercase())], "redact_exception_hash_bad"),
            (vec![one(&h), one(&h)], "redact_exception_duplicate"),
        ] {
            let p = RedactPolicy {
                exceptions: list,
                ..Default::default()
            };
            assert_eq!(p.check().unwrap_err().code(), code);
        }
        // 写成配置里的样子：规则是空的不写
        let p = RedactPolicy {
            exceptions: vec![RedactException {
                rule: String::new(),
                ..one(&h)
            }],
            ..Default::default()
        };
        let y = serde_yaml_ng::to_string(&p).unwrap();
        assert_eq!(
            y,
            format!("exceptions:\n- sha256: {h}\n  masked: ''\n  added_at: 0\n")
        );
        assert_eq!(serde_yaml_ng::from_str::<RedactPolicy>(&y).unwrap(), p);
        assert!(
            serde_yaml_ng::from_str::<RedactPolicy>("exceptions:\n- { sha256: x, value: y }")
                .is_err(),
            "不认的字段是错"
        );
    }

    /// R5：手动加入。认得出的按那条规则打码、记下规则；认不出的规则是空的、按最小心的打码。
    /// 返回的东西里没有原值
    #[test]
    fn a_value_added_by_hand_is_hashed_and_masked() {
        let p = RedactPolicy::default();
        let e = p.exception_of(&format!("  {KEY}\n"), None, 42).unwrap();
        assert_eq!(e.sha256, crate::redact::exceptions::sha256_hex(KEY));
        assert_eq!(e.rule, "anthropic-api-key");
        assert_eq!(
            e.masked,
            crate::redact::rules::masked(
                &crate::redact::rules::Rule::Builtin("anthropic-api-key"),
                KEY
            )
        );
        assert_eq!(e.added_at, 42);
        assert!(!format!("{e:?}").contains(&KEY[8..]), "{e:?}");
        // 出厂关着的规则也认：邮箱
        let e = p.exception_of("jane@example.com", None, 0).unwrap();
        assert_eq!(
            (e.rule.as_str(), e.masked.as_str()),
            ("email", "j…@example.com")
        );
        // 哪条都认不出
        let e = p.exception_of("my-fixture-value-123456", None, 0).unwrap();
        assert_eq!((e.rule.as_str(), e.masked.as_str()), ("", "…3456"));
        let e = p.exception_of("short", None, 0).unwrap();
        assert_eq!(e.masked, "…");
        assert_eq!(p.exception_of(" \n", None, 0), Err(ExceptionError::Empty));
    }
}
