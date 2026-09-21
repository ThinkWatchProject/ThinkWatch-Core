//! 两项防护：出站脱敏、工具调用审查。
//!
//! # 全局的，不按上游、不按路由
//!
//! 以前「脱哪几类」写在每个上游上（官方端点默认不脱），「当不当它可信」也
//! 写在上游上，路由规则还能再加一层。三层叠在一起的结果是没人说得清一个
//! 请求到底按什么规格走 —— 观察档按全部类别检测、拦截档按上游的类别替换，
//! 于是同一个请求观察时报「检测到外泄」，切到拦截后一处不换。
//!
//! 现在两项防护各有一个档位和一套规则，对所有请求一视同仁。
//!
//! # 三态：关闭 / 观察 / 拦截
//!
//! **出厂时都停在「观察」。**
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

/// 两项防护。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Security {
    #[serde(default, skip_serializing_if = "is_default")]
    pub redact: RedactPolicy,
    #[serde(default, skip_serializing_if = "is_default")]
    pub inspect_tools: ToolPolicy,
}

impl Security {
    /// 「拦截」态在这项防护上具体做什么。
    ///
    /// **界面上要显示各自的动词，不要统一叫「拦截」** —— 两件事差得很远，
    /// 而用户点「切到拦截」时应该清楚知道会发生什么。
    pub fn enforce_verb(line: &str) -> &'static str {
        match line {
            "redact" => "replaces",
            "inspect_tools" => "cuts off",
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
    fn the_old_one_word_form_is_not_read() {
        // 以前是 `redact: enforce`。项目还没有存量用户，不做兼容：读不懂就
        // 说读不懂，而不是悄悄当成默认值。
        assert!(serde_yaml_ng::from_str::<Security>("redact: enforce").is_err());
        assert!(serde_yaml_ng::from_str::<Security>("scan_configs: observe").is_err());
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
