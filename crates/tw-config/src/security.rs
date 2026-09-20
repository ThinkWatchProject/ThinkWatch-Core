//! 三态：关闭 / 观察 / 拦截。
//!
//! **出厂时都停在「观察」。**
//!
//! 安全功能第一次接触用户的方式如果是「误报打断了正在跑的任务」，它就
//! 死了 —— 用户会关掉整个功能，而且再也不会打开。但直接关掉又等于白做。
//!
//! 「观察」是唯一合理的默认值：它不打扰任何人，却在悄悄攒一件事 ——
//! **属于你自己的证据**。跑上一周，界面上出现的不是一句「我们有安全
//! 功能」，而是「过去 7 天，有 3 个请求把你的 API key 发给了 relay-cn」。
//! 这比任何功能介绍都有说服力，因为它说的是已经发生在你身上的事。

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
    /// `label()` 是给人看的中文，**这个是写回 YAML 用的**。两者必须分开：
    /// 界面上把「观察」原样写进 config.yaml 的话，下一次加载会因为
    /// 「不是合法取值」整份被拒 —— 而这一层刻意不做静默回落，
    /// 所以那是一次真的、用户看不懂的启动失败。
    pub fn slug(&self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Observe => "observe",
            Mode::Enforce => "enforce",
        }
    }
}

/// 用户自己加的一条扫描规则。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanRule {
    pub id: String,
    pub pattern: String,
    /// **为什么它值得看一眼。**没有这一句，一条命中就只是个规则 id
    pub why: String,
    /// `dangerous`（命令）或 `injection`（提示注入）。不写按前者算
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// `high` 或 `medium`。**不写按 medium 算** —— 用户新加的规则默认
    /// 只告警不切断，要它动手得自己写明白（「零值 = 安全」）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
}

/// 扫描规则的用户改动。
///
/// **加法加停用，不是整份替换。**替换看起来更「干净」，但它有和
/// cc-switch 那个白名单一模一样的毛病：用户复制一份内置规则再
/// 改两条之后，**他那份就永远停在复制的那一刻了** —— 我们后来加的每一条
/// 新攻击模式都到不了他机器上，而他不会察觉。
///
/// 所以：加的写进 `add`，不要的写进 `disable`（按 id）。「现在到底哪些
/// 规则生效」这个问题由界面回答，而不是靠逼用户抄一份。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanRules {
    /// 在内置规则之外再加这些
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add: Vec<ScanRule>,
    /// 停用内置规则里的这几条，按 id
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disable: Vec<String>,
}

/// 三道防线。
///
/// **比 `{ enabled: false } + shadow_mode: true` 清爽得多** —— 那是两个
/// 字段表达一件事，用户得在脑子里做一次组合才知道当前到底什么状态。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Security {
    /// 出站脱敏。「拦截」态的动作是**替换** —— 把密钥换成占位符再发出去
    #[serde(default)]
    pub redact: Mode,
    /// 入站审查。「拦截」态的动作是**切断**
    #[serde(default)]
    pub inspect_tools: Mode,
    /// 配置扫描。「拦截」态的动作是**告警** —— 它本来就不删东西
    #[serde(default)]
    pub scan_configs: Mode,
    /// 扫描规则的增删。
    ///
    /// **它住在这里，而不是另一个文件里**：`config.yaml` 是唯一
    /// 的配置文件，而规则集是用户会去调的策略，不是数据。住在这里还白捡
    /// 了变更历史和一键回滚 —— 单独一个文件那两样都没有。
    #[serde(default, skip_serializing_if = "is_default_scan_rules")]
    pub scan_rules: ScanRules,
}

fn is_default_scan_rules(r: &ScanRules) -> bool {
    r.add.is_empty() && r.disable.is_empty()
}

impl Security {
    /// 「拦截」态在这条防线上具体做什么。
    ///
    /// **界面上要显示各自的动词，不要统一叫「拦截」** —— 三件事差得
    /// 很远，而用户点「切到拦截」时应该清楚知道会发生什么。
    pub fn enforce_verb(line: &str) -> &'static str {
        match line {
            "redact" => "replaces",
            "inspect_tools" => "cuts off",
            "scan_configs" => "reports",
            _ => "enforces",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `slug()` 必须真的能被反序列化回来。
    ///
    /// 界面要能改这三个开关，而它写回 config.yaml 的就是这个字符串。
    /// 写错一个词的后果不是「按默认值来」—— 这一层刻意不做静默回落
    /// （「以为自己开了拦截，其实只在观察」是最糟的状态），所以
    /// 整份配置会被拒，表现成一次用户看不懂的启动失败。
    ///
    /// 所以这条不是在测一个 getter，是在把界面和加载器之间那个约定钉住。
    #[test]
    fn every_slug_round_trips_through_yaml() {
        for m in [Mode::Off, Mode::Observe, Mode::Enforce] {
            let yaml = format!("redact: {}\n", m.slug());
            let back: Security = serde_yaml_ng::from_str(&yaml)
                .unwrap_or_else(|e| panic!("slug {:?} 读不回来：{e}", m.slug()));
            assert_eq!(back.redact, m, "slug {:?} 解析成了别的档", m.slug());
        }
    }

    #[test]
    fn everything_ships_in_observe_mode() {
        // **这条测的是一个产品决定，不是一段逻辑。**默认值改成 Off 就
        // 等于白做，改成 Enforce 就等于用误报去打断用户第一次使用。
        let s = Security::default();
        assert_eq!(s.redact, Mode::Observe);
        assert_eq!(s.inspect_tools, Mode::Observe);
        assert_eq!(s.scan_configs, Mode::Observe);
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
        // 三件事差得很远，统一叫「拦截」会让用户不知道自己在开什么。
        assert_eq!(Security::enforce_verb("redact"), "replaces");
        assert_eq!(Security::enforce_verb("inspect_tools"), "cuts off");
        assert_eq!(Security::enforce_verb("scan_configs"), "reports");
    }

    #[test]
    fn only_the_line_you_wrote_moves() {
        let s: Security = serde_yaml_ng::from_str("redact: enforce").unwrap();
        assert_eq!(s.redact, Mode::Enforce);
        assert_eq!(s.inspect_tools, Mode::Observe, "别的两条被顺手改了");
    }

    #[test]
    fn a_misspelled_mode_is_an_error_not_a_silent_off() {
        // **「observ」被静默当成默认值最糟**：用户以为自己关掉了，
        // 而它还在跑；或者以为自己开了拦截，而它只在观察。
        assert!(serde_yaml_ng::from_str::<Security>("redact: observ").is_err());
        assert!(serde_yaml_ng::from_str::<Security>("redcat: off").is_err());
    }
}
