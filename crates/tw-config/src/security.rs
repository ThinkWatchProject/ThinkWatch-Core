//! 三项防护：出站脱敏、工具调用审查、内容过滤。
//!
//! **形状、出厂值、校验、编译都在共享层**（[`tw_guard::policy`]）：企业版存在系统设置里
//! 的是同一份，两边一样。这里只剩桌面版自己的事 —— 它写在 `config.yaml` 的 `security:`
//! 下（[`crate::Config::security`]），校验出的错说成配置错误的消息码（[`policy_msg`]）。
//!
//! 全局的，不按上游、不按路由：以前「脱哪几类」写在每个上游上，路由规则还能再加一层，
//! 三层叠在一起没人说得清一个请求到底按什么规格走。现在每项防护一个档位、一套规则，
//! 对所有请求一视同仁。
//!
//! 规则住在 `config.yaml` 里，而不是另一个文件：它是用户会去调的策略，不是数据，而且
//! 住在这里白捡了变更历史和一键回滚。

pub use tw_guard::policy::{
    ContentAction, ContentMatch, ContentPolicy, CustomContentRule, CustomRedactRule,
    CustomToolRule, Mode, PolicyError, RedactPolicy, Security, ToolAction, ToolPolicy,
};
use tw_types::{Msg, msg};

/// 一份策略过不了校验时说给人听的话。码是 `config.` 加上 [`PolicyError::code`]：
/// 改一处配置、整份校验，和在界面上存一条规则时报的是同一句。
pub fn policy_msg(e: &PolicyError) -> Msg {
    let what = e.guard().rule_noun();
    match e {
        PolicyError::EmptyName { .. } => msg!(
            "config.rule_name_empty", what = what =>
            "a custom {what} rule has no name"
        ),
        PolicyError::DuplicateName { name, .. } => msg!(
            "config.rule_name_taken", what = what, name = name.clone() =>
            "the custom {what} rule name `{name}` appears twice"
        ),
        PolicyError::EmptyPattern { name, .. } => msg!(
            "config.rule_pattern_empty", what = what, name = name.clone() =>
            "the pattern of custom {what} rule `{name}` is empty"
        ),
        PolicyError::BadPattern { name, detail, .. } => msg!(
            "config.rule_pattern_bad", what = what, name = name.clone(), detail = detail.clone() =>
            "the pattern of custom {what} rule `{name}` is not a valid regular expression: \
             {detail}"
        ),
        PolicyError::BadCodepoints { name, reason } => msg!(
            "config.rule_codepoints_bad", name = name.clone(), detail = reason.to_string() =>
            "the code points of custom content rule `{name}` are not written right: {detail}"
        ),
        PolicyError::BadLabel { name, label } => msg!(
            "config.rule_label_bad", name = name.clone(), label = label.clone() =>
            "the placeholder name of custom redaction rule `{name}` is `{label}`; it has to be \
             1 to 24 capital letters, digits and underscores, starting with a letter"
        ),
        PolicyError::UnknownRule { guard, id } => msg!(
            "config.unknown_rule", guard = guard.slug(), rule = id.clone() =>
            "security.{guard} names `{rule}`, which is not a built-in rule"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 删掉的两项写在配置里是错：**静默忽略的话**，用户以为隐藏字符还按那一项查着
    #[test]
    fn the_two_removed_guards_are_configuration_errors() {
        for gone in [
            "hidden_text:\n  mode: off",
            "output_limit:\n  mode: enforce",
        ] {
            assert!(serde_yaml_ng::from_str::<Security>(gone).is_err(), "{gone}");
        }
        let s: Security = serde_yaml_ng::from_str("content:\n  mode: enforce").unwrap();
        assert_eq!(s.content.mode, Mode::Enforce);
        assert_eq!(s.redact.mode, Mode::Observe, "别的一项被顺手改了");
    }

    #[test]
    fn every_policy_error_is_a_config_message_with_its_own_code() {
        let errors = [
            (
                "redact:\n  custom:\n    - { name: ' ', pattern: a }",
                "config.rule_name_empty",
            ),
            (
                "inspect_tools:\n  custom:\n    - { name: a, pattern: x }\n    - { name: a, pattern: y }",
                "config.rule_name_taken",
            ),
            (
                "content:\n  custom:\n    - { name: a, pattern: '  ' }",
                "config.rule_pattern_empty",
            ),
            (
                "redact:\n  custom:\n    - { name: a, pattern: '(' }",
                "config.rule_pattern_bad",
            ),
            (
                "content:\n  custom:\n    - { name: a, pattern: 'U+GG', match: codepoints }",
                "config.rule_codepoints_bad",
            ),
            (
                "redact:\n  custom:\n    - { name: a, pattern: x, label: lower }",
                "config.rule_label_bad",
            ),
            ("content:\n  disable: [jailbrake]", "config.unknown_rule"),
        ];
        for (yaml, code) in errors {
            let e = serde_yaml_ng::from_str::<Security>(yaml)
                .unwrap()
                .check()
                .unwrap_err();
            let m = policy_msg(&e);
            assert_eq!(m.code, code, "{yaml}");
            assert_eq!(code.strip_prefix("config."), Some(e.code()));
        }
        let m = policy_msg(
            &serde_yaml_ng::from_str::<Security>("content:\n  disable: [jailbrake]")
                .unwrap()
                .check()
                .unwrap_err(),
        );
        assert_eq!(m.args.get("guard").map(String::as_str), Some("content"));
        assert_eq!(m.args.get("rule").map(String::as_str), Some("jailbrake"));
    }
}
