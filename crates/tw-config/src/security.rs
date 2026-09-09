//! 三态：关闭 / 观察 / 拦截（DESIGN.md §5.0）。
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
            Mode::Off => "关闭",
            Mode::Observe => "观察",
            Mode::Enforce => "拦截",
        }
    }
}

/// 三道防线（§5）。
///
/// **比 `{ enabled: false } + shadow_mode: true` 清爽得多** —— 那是两个
/// 字段表达一件事，用户得在脑子里做一次组合才知道当前到底什么状态。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Security {
    /// 出站脱敏。「拦截」态的动作是**替换** —— 把密钥换成占位符再发出去
    #[serde(default)]
    pub redact: Mode,
    /// 入站审查。「拦截」态的动作是**切断**（§5.2）
    #[serde(default)]
    pub inspect_tools: Mode,
    /// 配置扫描。「拦截」态的动作是**告警** —— 它本来就不删东西（§5.3）
    #[serde(default)]
    pub scan_configs: Mode,
}

impl Security {
    /// 「拦截」态在这条防线上具体做什么。
    ///
    /// **界面上要显示各自的动词，不要统一叫「拦截」** —— 三件事差得
    /// 很远，而用户点「切到拦截」时应该清楚知道会发生什么（§5.0）。
    pub fn enforce_verb(line: &str) -> &'static str {
        match line {
            "redact" => "替换",
            "inspect_tools" => "切断",
            "scan_configs" => "告警",
            _ => "拦截",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(Security::enforce_verb("redact"), "替换");
        assert_eq!(Security::enforce_verb("inspect_tools"), "切断");
        assert_eq!(Security::enforce_verb("scan_configs"), "告警");
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
