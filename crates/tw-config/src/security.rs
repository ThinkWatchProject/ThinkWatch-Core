//! 五项防护：出站脱敏、工具调用审查、藏匿字符、内容过滤、输出长度。
//!
//! # 全局的，不按上游、不按路由
//!
//! 以前「脱哪几类」写在每个上游上（官方端点默认不脱），「当不当它可信」也
//! 写在上游上，路由规则还能再加一层。三层叠在一起的结果是没人说得清一个
//! 请求到底按什么规格走 —— 观察档按全部类别检测、拦截档按上游的类别替换，
//! 于是同一个请求观察时报「检测到外泄」，切到拦截后一处不换。
//!
//! 现在每项防护各有一个档位（有规则的还有一套规则），对所有请求一视同仁。
//!
//! # 三态：关闭 / 观察 / 拦截
//!
//! **出厂时停在「观察」**（输出长度除外：它没有一个说得过去的出厂上限，出厂是关的）。
//!
//! 安全功能第一次接触用户的方式如果是「误报打断了正在跑的任务」，它就
//! 死了 —— 用户会关掉整个功能，而且再也不会打开。但直接关掉又等于白做。
//!
//! 「观察」是唯一合理的默认值：它不打扰任何人，却在悄悄攒一件事 ——
//! **属于你自己的证据**。跑上一周，界面上出现的不是一句「我们有安全
//! 功能」，而是「过去 7 天，有 3 个请求把你的 API key 发了出去」。
//! 这比任何功能介绍都有说服力，因为它说的是已经发生在你身上的事。
//!
//! # 规则：内置的开关 + 自定义的
//!
//! **加法加停用，不是整份替换。**替换看起来更「干净」，但用户复制一份内置
//! 规则再改两条之后，**他那份就永远停在复制的那一刻了** —— 我们后来加的每
//! 一条都到不了他机器上，而他不会察觉。所以内置规则只记「改过默认开关的
//! 那几条」，自定义规则另起一个列表。
//!
//! 规则住在 `config.yaml` 里，而不是另一个文件：它是用户会去调的策略，
//! 不是数据，而且住在这里白捡了变更历史和一键回滚。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// 什么都不做。确定不需要，或者误报太烦
    Off,
    /// 照常检测，**只记录，不改变任何行为**。默认
    #[default]
    Observe,
    /// 检测并动手
    Enforce,
}

impl Mode {
    pub fn detects(&self) -> bool {
        !matches!(self, Mode::Off)
    }
    /// 会不会改变请求的去向或内容。**观察态永远是 false。**
    pub fn acts(&self) -> bool {
        matches!(self, Mode::Enforce)
    }
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Observe => "observe",
            Mode::Enforce => "enforce",
        }
    }

    /// 配置文件里写的那个词。
    ///
    /// `label()` 是给人看的，**这个是写回 YAML 用的**。界面上把「观察」原样
    /// 写进 config.yaml 的话，下一次加载会因为「不是合法取值」整份被拒 ——
    /// 而这一层刻意不做静默回落。
    pub fn slug(&self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Observe => "observe",
            Mode::Enforce => "enforce",
        }
    }

    pub fn from_slug(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Mode::Off),
            "observe" => Some(Mode::Observe),
            "enforce" => Some(Mode::Enforce),
            _ => None,
        }
    }
}

/// 一条工具调用规则命中之后，在拦截档下做什么。观察档一律只记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolAction {
    /// 切断响应。客户端拿不到完整的调用，也就执行不了
    Cut,
    /// 只记录，调用照常返回。**不写就是它** —— 手写的规则默认只记不切，要它
    /// 动手得自己写明白（「零值 = 安全」）
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
        match s {
            "cut" => Some(ToolAction::Cut),
            "record" => Some(ToolAction::Record),
            _ => None,
        }
    }
}

/// 用户自己写的一条脱敏规则：匹配到的整段按凭据处理。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomRedactRule {
    /// 日志和界面上显示的名字，也是它的标识
    pub name: String,
    /// 正则表达式
    pub pattern: String,
    /// 停用。**规则原样留着**，打开就回来
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
}

/// 用户自己写的一条工具调用规则：按工具调用的参数匹配。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomToolRule {
    pub name: String,
    pub pattern: String,
    /// 拦截档下命中之后做什么
    #[serde(default, skip_serializing_if = "is_default")]
    pub action: ToolAction,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
}

/// 出站脱敏：请求发出前，按规则查找凭据。拦截档的动作是**替换**。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

/// 工具调用审查：检查上游返回的工具调用参数。拦截档的动作是**切断**，
/// 只对处置为「切断」的规则。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPolicy {
    #[serde(default, skip_serializing_if = "is_default")]
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enable: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disable: Vec<String>,
    /// 内置规则在拦截档下做什么，**只写和出厂不一样的**：`rm-rf-root: cut`。
    ///
    /// 内置规则提供的只是一条正则和一个出厂的处置；命中之后切不切，和自定义
    /// 规则一样由用户定。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub actions: BTreeMap<String, ToolAction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom: Vec<CustomToolRule>,
}

impl RedactPolicy {
    /// 启用着的自定义规则，`(名字, 正则)`
    pub fn active_custom(&self) -> impl Iterator<Item = (&str, &str)> {
        self.custom
            .iter()
            .filter(|c| !c.disabled)
            .map(|c| (c.name.as_str(), c.pattern.as_str()))
    }
}

/// 工具调用审查的内置规则 id：tw-guard 内置规则里「危险命令」那一组。
///
/// `config.yaml` 按 id 引用它们（`security.inspect_tools` 的 `enable` /
/// `disable` / `actions`），校验要认得出。
fn tool_rule_ids() -> impl Iterator<Item = &'static str> {
    tw_guard::tools::rules::builtin()
        .dangerous
        .iter()
        .map(|s| s.id.as_str())
}

impl ToolAction {
    /// 一条内置规则出厂时在拦截档下做什么。
    pub fn factory(spec: &tw_guard::tools::rules::RuleSpec) -> Self {
        if spec.high() {
            ToolAction::Cut
        } else {
            ToolAction::Record
        }
    }
}

impl ToolPolicy {
    /// 这一份配置下的工具调用审查规则：内置的去掉停用的、按改过的处置走，
    /// 再加上启用着的自定义规则。
    ///
    /// 自定义规则的正则在配置校验时已经编过一次；这里再编失败只可能是有人绕过
    /// 了校验，照样当错误返回，不静默跳过。
    pub fn rules(
        &self,
    ) -> Result<tw_guard::tools::rules::Rules, tw_guard::tools::rules::RuleError> {
        tw_guard::tools::rules::tool_rules(
            &self.disable,
            |id| self.cut(id),
            self.custom
                .iter()
                .filter(|c| !c.disabled)
                .map(|c| tw_guard::tools::rules::Custom {
                    name: &c.name,
                    pattern: &c.pattern,
                    cut: c.action == ToolAction::Cut,
                }),
        )
    }

    /// 只有一条内置规则，**不管它启用没有**，处置按这份配置走。安全页上
    /// 「试一条停用着的规则」用它。
    pub fn one_builtin(&self, id: &str) -> Option<tw_guard::tools::rules::Rules> {
        tw_guard::tools::rules::one_builtin(id, self.cut(id))
    }

    /// 用户改过这条内置规则的处置的话，改成了什么。
    fn cut(&self, id: &str) -> Option<bool> {
        self.actions.get(id).map(|a| *a == ToolAction::Cut)
    }
}

/// 藏匿字符：调用方发来的正文里（连同工具结果）人眼看不见、模型读得到的字符。
/// 拦截档的动作是**拒绝这个请求**。
///
/// 只查在任何正文里都没有正当用途的两种（`tw_guard::hidden::SMUGGLING`）：标签字符
/// 和双向控制符。零宽连接符组成表情、波斯文要零宽不连字，那几种不查。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HiddenPolicy {
    #[serde(default, skip_serializing_if = "is_default")]
    pub mode: Mode,
    /// 不查这几种，按 slug：`tag` / `bidi`
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disable: Vec<String>,
}

impl HiddenPolicy {
    /// 要查的那几种
    pub fn kinds(&self) -> Vec<tw_guard::hidden::Kind> {
        tw_guard::hidden::SMUGGLING
            .into_iter()
            .filter(|k| !self.disable.iter().any(|d| d == k.slug()))
            .collect()
    }
}

/// 一条内容规则命中之后，在拦截档下做什么。观察档一律只记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentAction {
    /// 不发出去
    Block,
    /// 只记录。**手写的规则不写就是它**（「零值 = 安全」）
    #[default]
    Record,
}

impl ContentAction {
    pub fn slug(&self) -> &'static str {
        match self {
            ContentAction::Block => "block",
            ContentAction::Record => "record",
        }
    }
    pub fn from_slug(s: &str) -> Option<Self> {
        match s {
            "block" => Some(ContentAction::Block),
            "record" => Some(ContentAction::Record),
            _ => None,
        }
    }
    /// 一条内置规则出厂时在拦截档下做什么：出厂处置是 `block` 的拦，其余只记
    pub fn factory(b: &tw_guard::content::Builtin) -> Self {
        if b.action == tw_guard::content::Action::Block {
            ContentAction::Block
        } else {
            ContentAction::Record
        }
    }
    fn engine(self) -> tw_guard::content::Action {
        match self {
            ContentAction::Block => tw_guard::content::Action::Block,
            ContentAction::Record => tw_guard::content::Action::Warn,
        }
    }
}

/// 一条内容规则怎么认。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentMatch {
    /// 不分大小写的子串。**不写就是它**：关键词是最常见的写法
    #[default]
    Contains,
    /// 不分大小写的正则
    Regex,
}

impl ContentMatch {
    pub fn slug(&self) -> &'static str {
        self.engine().slug()
    }
    pub fn from_slug(s: &str) -> Option<Self> {
        match tw_guard::content::Match::from_slug(s)? {
            tw_guard::content::Match::Contains => Some(ContentMatch::Contains),
            tw_guard::content::Match::Regex => Some(ContentMatch::Regex),
        }
    }
    pub fn engine(self) -> tw_guard::content::Match {
        match self {
            ContentMatch::Contains => tw_guard::content::Match::Contains,
            ContentMatch::Regex => tw_guard::content::Match::Regex,
        }
    }
}

/// 用户自己写的一条内容规则。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomContentRule {
    pub name: String,
    pub pattern: String,
    #[serde(rename = "match", default, skip_serializing_if = "is_default")]
    pub matching: ContentMatch,
    #[serde(default, skip_serializing_if = "is_default")]
    pub action: ContentAction,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
}

/// 内容过滤：调用方发来的正文里（连同工具结果）出现了某个词或某种写法。
/// 拦截档的动作是**拒绝这个请求**，只对处置为「拦」的规则。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    /// 内置规则在拦截档下做什么，**只写和出厂不一样的**
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub actions: BTreeMap<String, ContentAction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom: Vec<CustomContentRule>,
}

impl ContentPolicy {
    /// 这条内置规则现在开着吗
    pub fn builtin_on(&self, b: &tw_guard::content::Builtin) -> bool {
        if b.on_by_default {
            !self.disable.contains(&b.id)
        } else {
            self.enable.contains(&b.id)
        }
    }

    /// 这条内置规则在拦截档下做什么（改过的按改过的）
    pub fn builtin_action(&self, b: &tw_guard::content::Builtin) -> ContentAction {
        self.actions
            .get(&b.id)
            .copied()
            .unwrap_or_else(|| ContentAction::factory(b))
    }

    /// 这一份配置下的规则：开着的内置规则按改过的处置走，再加上启用着的自定义规则。
    pub fn rules(&self) -> Result<tw_guard::content::Rules, tw_guard::content::BadRule> {
        use tw_guard::content::{RuleInput, Rules, builtins};
        let builtin = builtins()
            .iter()
            .filter(|b| self.builtin_on(b))
            .map(|b| RuleInput {
                action: self.builtin_action(b).engine(),
                ..RuleInput::from(b)
            });
        let custom = self
            .custom
            .iter()
            .filter(|c| !c.disabled)
            .map(|c| RuleInput {
                id: &c.name,
                name: &c.name,
                custom: true,
                pattern: &c.pattern,
                matching: c.matching.engine(),
                action: c.action.engine(),
            });
        Rules::build(builtin.chain(custom))
    }

    /// 只有一条内置规则，**不管它开没开**，处置按这份配置走。安全页上「试一条」用它
    pub fn one_builtin(&self, id: &str) -> Option<tw_guard::content::Rules> {
        let b = tw_guard::content::builtin(id)?;
        tw_guard::content::Rules::build([tw_guard::content::RuleInput {
            action: self.builtin_action(b).engine(),
            ..tw_guard::content::RuleInput::from(b)
        }])
        .ok()
    }
}

/// 输出长度：模型一次回答的正文最多多少个字符。拦截档的动作是**切断**：流从超过的
/// 那一帧起不再发，整包整份不发。
///
/// **出厂是关的。**「多长算失控」没有一个对所有人都说得过去的数，开着一个随手定的
/// 上限，只会在某次正常的长回答上突然截断。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputLimitPolicy {
    #[serde(default = "mode_off", skip_serializing_if = "is_off")]
    pub mode: Mode,
    /// 按字符数（Unicode 标量），不是字节
    #[serde(
        default = "default_max_chars",
        skip_serializing_if = "is_default_max_chars"
    )]
    pub max_chars: usize,
}

/// 输出长度出厂的上限（只在打开之后才用得上）
pub const DEFAULT_MAX_CHARS: usize = 100_000;
/// 输出长度最多能设多大。**再大就是写错了** —— 没有模型一次回答得出一百万个字
pub const MAX_CHARS_CEILING: usize = 1_000_000;

impl Default for OutputLimitPolicy {
    fn default() -> Self {
        Self {
            mode: Mode::Off,
            max_chars: DEFAULT_MAX_CHARS,
        }
    }
}

impl OutputLimitPolicy {
    pub fn limit(&self) -> tw_guard::output::Limit {
        tw_guard::output::Limit {
            max: self.max_chars,
            unit: tw_guard::output::Unit::Chars,
        }
    }
}

fn mode_off() -> Mode {
    Mode::Off
}
fn is_off(m: &Mode) -> bool {
    *m == Mode::Off
}
fn default_max_chars() -> usize {
    DEFAULT_MAX_CHARS
}
fn is_default_max_chars(n: &usize) -> bool {
    *n == DEFAULT_MAX_CHARS
}

/// 五项防护。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Security {
    #[serde(default, skip_serializing_if = "is_default")]
    pub redact: RedactPolicy,
    #[serde(default, skip_serializing_if = "is_default")]
    pub inspect_tools: ToolPolicy,
    #[serde(default, skip_serializing_if = "is_default")]
    pub hidden_text: HiddenPolicy,
    #[serde(default, skip_serializing_if = "is_default")]
    pub content: ContentPolicy,
    #[serde(default, skip_serializing_if = "is_default")]
    pub output_limit: OutputLimitPolicy,
}

impl Security {
    /// 第一个认不出的内置规则 id，连同它写在哪一项下面。
    ///
    /// **认不出就是配置错**，和写错一个字段名一样：静默跳过的话，它的表现是
    /// 「我明明停用了它，怎么还在报」。
    pub(crate) fn unknown_rule(&self) -> Option<(&'static str, &str)> {
        let r = &self.redact;
        let t = &self.inspect_tools;
        r.enable
            .iter()
            .chain(&r.disable)
            .find(|id| tw_guard::redact::rules::builtin(id).is_none())
            .map(|id| ("redact", id.as_str()))
            .or_else(|| {
                t.enable
                    .iter()
                    .chain(&t.disable)
                    .chain(t.actions.keys())
                    .find(|id| !tool_rule_ids().any(|t| t == id.as_str()))
                    .map(|id| ("inspect_tools", id.as_str()))
            })
            .or_else(|| {
                let c = &self.content;
                c.enable
                    .iter()
                    .chain(&c.disable)
                    .chain(c.actions.keys())
                    .find(|id| tw_guard::content::builtin(id).is_none())
                    .map(|id| ("content", id.as_str()))
            })
            .or_else(|| {
                self.hidden_text
                    .disable
                    .iter()
                    .find(|id| {
                        !tw_guard::hidden::SMUGGLING
                            .iter()
                            .any(|k| k.slug() == id.as_str())
                    })
                    .map(|id| ("hidden_text", id.as_str()))
            })
    }

    /// 「拦截」态在这项防护上具体做什么。
    ///
    /// **界面上要显示各自的动词，不要统一叫「拦截」** —— 两件事差得很远，
    /// 而用户点「切到拦截」时应该清楚知道会发生什么。
    pub fn enforce_verb(line: &str) -> &'static str {
        match line {
            "redact" => "replaces",
            "inspect_tools" | "output_limit" => "cuts off",
            "hidden_text" | "content" => "refuses",
            _ => "enforces",
        }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `slug()` 必须真的能被反序列化回来。
    ///
    /// 界面要能改档位，而它写回 config.yaml 的就是这个字符串。写错一个词的
    /// 后果不是「按默认值来」—— 这一层刻意不做静默回落，所以整份配置会被拒，
    /// 表现成一次用户看不懂的启动失败。
    #[test]
    fn every_slug_round_trips_through_yaml() {
        for m in [Mode::Off, Mode::Observe, Mode::Enforce] {
            let yaml = format!("redact:\n  mode: {}\n", m.slug());
            let back: Security = serde_yaml_ng::from_str(&yaml)
                .unwrap_or_else(|e| panic!("slug {:?} 读不回来：{e}", m.slug()));
            assert_eq!(back.redact.mode, m, "slug {:?} 解析成了别的档", m.slug());
            assert_eq!(Mode::from_slug(m.slug()), Some(m));
        }
        for a in [ToolAction::Cut, ToolAction::Record] {
            assert_eq!(ToolAction::from_slug(a.slug()), Some(a));
        }
    }

    /// 配置到规则的翻译：停用、改处置、自定义规则的启停，一样不能丢。
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
        // 只试一条时也按改过的处置走，而且不管它停没停用
        assert!(p.one_builtin("rm-rf-root").unwrap().rules[0].high);
        assert!(p.one_builtin("chmod-777").is_some());
    }

    #[test]
    fn everything_ships_in_observe_mode() {
        // **这条测的是一个产品决定，不是一段逻辑。**默认值改成 Off 就
        // 等于白做，改成 Enforce 就等于用误报去打断用户第一次使用。
        let s = Security::default();
        assert_eq!(s.redact.mode, Mode::Observe);
        assert_eq!(s.inspect_tools.mode, Mode::Observe);
    }

    #[test]
    fn observe_detects_but_never_acts() {
        // 「只记录，不改变任何行为」是这一态的全部承诺。
        assert!(Mode::Observe.detects());
        assert!(!Mode::Observe.acts(), "观察态动手了 —— 那就不是观察了");
        assert!(!Mode::Off.detects());
        assert!(Mode::Enforce.acts());
    }

    #[test]
    fn each_line_of_defence_has_its_own_verb() {
        // 两件事差得很远，统一叫「拦截」会让用户不知道自己在开什么。
        assert_eq!(Security::enforce_verb("redact"), "replaces");
        assert_eq!(Security::enforce_verb("inspect_tools"), "cuts off");
    }

    #[test]
    fn only_the_line_you_wrote_moves() {
        let s: Security = serde_yaml_ng::from_str("redact:\n  mode: enforce").unwrap();
        assert_eq!(s.redact.mode, Mode::Enforce);
        assert_eq!(s.inspect_tools.mode, Mode::Observe, "别的一项被顺手改了");
    }

    #[test]
    fn a_misspelled_mode_is_an_error_not_a_silent_off() {
        // **「observ」被静默当成默认值最糟**：用户以为自己关掉了，
        // 而它还在跑；或者以为自己开了拦截，而它只在观察。
        assert!(serde_yaml_ng::from_str::<Security>("redact:\n  mode: observ").is_err());
        assert!(serde_yaml_ng::from_str::<Security>("redcat:\n  mode: off").is_err());
    }

    #[test]
    fn the_new_guards_ship_as_decided_and_write_nothing_by_default() {
        let s = Security::default();
        assert_eq!(s.hidden_text.mode, Mode::Observe);
        assert_eq!(s.content.mode, Mode::Observe);
        assert_eq!(s.output_limit.mode, Mode::Off, "输出长度出厂是关的");
        assert_eq!(s.output_limit.max_chars, DEFAULT_MAX_CHARS);
        let out = serde_yaml_ng::to_string(&s).unwrap();
        assert_eq!(out.trim(), "{}", "{out}");
        // 关的就是不写；写了 observe 要能读回来
        let back: Security =
            serde_yaml_ng::from_str("output_limit:\n  mode: observe\n  max_chars: 5000\n").unwrap();
        assert_eq!(back.output_limit.mode, Mode::Observe);
        assert_eq!(back.output_limit.max_chars, 5000);
    }

    #[test]
    fn the_content_policy_reaches_the_rules() {
        let p: ContentPolicy = serde_yaml_ng::from_str(
            "enable: [jailbreak]\ndisable: [ignore-all-previous]\nactions:\n  jailbreak: record\ncustom:\n  - name: 内部代号\n    pattern: project-x\n  - name: 正则\n    pattern: 'secret\\s+plan'\n    match: regex\n    action: block\n  - name: 停用的\n    pattern: zzz\n    disabled: true\n",
        )
        .unwrap();
        assert_eq!(p.custom[0].matching, ContentMatch::Contains, "不写就是子串");
        assert_eq!(p.custom[0].action, ContentAction::Record, "不写就是只记");
        let rs = p.rules().unwrap();
        let action = |id: &str| rs.rules.iter().find(|r| r.id == id).map(|r| r.action);
        use tw_guard::content::Action;
        assert_eq!(action("ignore-previous-instructions"), Some(Action::Block));
        assert_eq!(action("ignore-all-previous"), None, "停用的还在");
        assert_eq!(
            action("jailbreak"),
            Some(Action::Warn),
            "改成只记的没按改过的走"
        );
        assert_eq!(action("act-as"), None, "出厂关着的开了");
        assert_eq!(action("内部代号"), Some(Action::Warn));
        assert_eq!(action("正则"), Some(Action::Block));
        assert_eq!(action("停用的"), None);
        assert!(p.one_builtin("act-as").is_some(), "关着的也能单独试");
    }

    #[test]
    fn a_hidden_kind_can_be_switched_off() {
        let p = HiddenPolicy {
            disable: vec!["bidi".into()],
            ..Default::default()
        };
        assert_eq!(p.kinds(), vec![tw_guard::hidden::Kind::Tag]);
        assert_eq!(HiddenPolicy::default().kinds().len(), 2);
    }

    #[test]
    fn unknown_ids_on_the_new_guards_are_named() {
        let mut s = Security::default();
        s.content.actions = [("jailbrake".to_string(), ContentAction::Block)].into();
        assert_eq!(s.unknown_rule(), Some(("content", "jailbrake")));
        let mut s = Security::default();
        s.hidden_text.disable = vec!["zero_width".into()];
        assert_eq!(
            s.unknown_rule(),
            Some(("hidden_text", "zero_width")),
            "只有两种可以关"
        );
    }

    #[test]
    fn a_custom_tool_rule_records_unless_it_says_to_cut() {
        // 手写的规则默认只记不切 —— 要它动手得自己写明白
        let p: ToolPolicy = serde_yaml_ng::from_str(
            "custom:\n  - name: 删除集群资源\n    pattern: 'kubectl\\s+delete'\n",
        )
        .unwrap();
        assert_eq!(p.custom[0].action, ToolAction::Record);
        // 默认值不写回文件
        let out = serde_yaml_ng::to_string(&p).unwrap();
        assert!(!out.contains("action"), "{out}");
        assert!(!out.contains("disabled"), "{out}");
        assert!(!out.contains("mode"), "{out}");
    }
}
