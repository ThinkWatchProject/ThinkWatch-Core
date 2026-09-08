//! 规则的条件部分。
//!
//! **结构化对象，不是逗号分隔的字符串**（DESIGN.md §3.4）。初稿照搬了
//! Clash 的 `TYPE,VALUE,TARGET`，连括号嵌套都抄了 —— 那东西写不出来也
//! 读不懂，而且它的形状是为「匹配目标地址」设计的。
//!
//! 同一个 `when` 里多个条件默认 AND，覆盖九成需求，于是 `AND` 关键字和
//! 括号嵌套直接消失。

use serde::{Deserialize, Serialize};

use crate::facts::RequestFacts;
use crate::num::Compare;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct When {
    /// glob：`claude-opus-*`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialect: Option<String>,
    /// `">200k"` / `"<4k"`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_count: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
}

impl When {
    /// 一个条件都没写 = 兜底规则。
    pub fn is_catch_all(&self) -> bool {
        self.model.is_none()
            && self.client.is_none()
            && self.dialect.is_none()
            && self.input_tokens.is_none()
            && self.max_tokens.is_none()
            && self.tool_count.is_none()
            && self.cache.is_none()
            && self.tools.is_none()
            && self.image.is_none()
            && self.thinking.is_none()
            && self.stream.is_none()
    }

    /// 写死的条件语法对吗。**在加载配置时查，不要等到请求进来**
    /// —— 一条永远不命中的规则在运行时是完全静默的。
    pub fn validate(&self) -> Result<(), MatchError> {
        for (field, v) in [
            ("input_tokens", &self.input_tokens),
            ("max_tokens", &self.max_tokens),
            ("tool_count", &self.tool_count),
        ] {
            if let Some(s) = v {
                s.parse::<Compare>()
                    .map_err(|e| MatchError::BadCompare { field, source: e })?;
            }
        }
        Ok(())
    }

    pub fn matches(&self, f: &RequestFacts) -> Result<bool, MatchError> {
        if let Some(pat) = &self.model
            && !glob_match(pat, &f.model)
        {
            return Ok(false);
        }
        // client 和 dialect 是精确匹配：它们的取值是我们自己定义的一个
        // 小集合，通配符只会掩盖打错的名字。
        if let Some(c) = &self.client
            && c != &f.client
        {
            return Ok(false);
        }
        if let Some(d) = &self.dialect
            && d != &f.dialect
        {
            return Ok(false);
        }
        for (field, spec, value) in [
            ("input_tokens", &self.input_tokens, f.input_tokens as f64),
            (
                "max_tokens",
                &self.max_tokens,
                // 没写 max_tokens 的请求，任何比较都不该命中 —— 当成 0
                // 会让 `"<4k"` 意外匹配上。
                f.max_tokens.map(|m| m as f64).unwrap_or(f64::NAN),
            ),
            ("tool_count", &self.tool_count, f.tool_count as f64),
        ] {
            if let Some(s) = spec {
                let cmp: Compare = s
                    .parse()
                    .map_err(|e| MatchError::BadCompare { field, source: e })?;
                if value.is_nan() || !cmp.matches(value) {
                    return Ok(false);
                }
            }
        }
        for (want, got) in [
            (self.cache, f.cache),
            (self.tools, f.tools),
            (self.image, f.image),
            (self.thinking, f.thinking),
            (self.stream, f.stream),
        ] {
            if let Some(w) = want
                && w != got
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum MatchError {
    #[error("条件 `{field}` 写错了：{source}")]
    BadCompare {
        field: &'static str,
        source: crate::num::ParseError,
    },
}

/// 只支持 `*`，因为模型名里只需要这个。
///
/// 不引正则：模型名是 `claude-opus-4-5` 这种形状，`claude-opus-*` 已经
/// 覆盖全部真实需求，而一个完整的正则引擎会让规则变得能写出没人看得懂
/// 的东西。
pub fn glob_match(pattern: &str, s: &str) -> bool {
    // 大小写不敏感：上游对模型名的大小写并不一致，而用户不该关心这个。
    let (pattern, s) = (pattern.to_ascii_lowercase(), s.to_ascii_lowercase());
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else {
        return true;
    };
    if !s.starts_with(first) {
        return false;
    }
    let mut pos = first.len();
    let mut last_was_star = pattern.len() > first.len();
    for part in parts {
        if part.is_empty() {
            last_was_star = true;
            continue;
        }
        match s[pos..].find(part) {
            Some(i) => pos += i + part.len(),
            None => return false,
        }
        last_was_star = false;
    }
    last_was_star || pos == s.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f() -> RequestFacts {
        RequestFacts {
            model: "claude-sonnet-4-5".into(),
            client: "claude-code".into(),
            dialect: "anthropic".into(),
            input_tokens: 10_000,
            max_tokens: Some(4096),
            cache: false,
            tools: false,
            tool_count: 0,
            image: false,
            thinking: false,
            stream: true,
        }
    }

    fn when(yaml: &str) -> When {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    #[test]
    fn globs_do_what_people_expect_of_model_names() {
        assert!(glob_match("claude-opus-*", "claude-opus-4-5"));
        assert!(glob_match("*sonnet*", "claude-sonnet-4-5"));
        assert!(glob_match("claude-sonnet-4-5", "claude-sonnet-4-5"));
        assert!(!glob_match("claude-opus-*", "claude-sonnet-4-5"));
        assert!(glob_match("*", "anything"));
    }

    #[test]
    fn model_matching_ignores_case() {
        // 上游对模型名的大小写并不一致，而用户不该为此写两条规则。
        assert!(glob_match("Claude-Opus-*", "claude-opus-4-5"));
        assert!(glob_match("claude-opus-*", "CLAUDE-OPUS-4-5"));
    }

    #[test]
    fn a_prefix_pattern_does_not_match_a_longer_prefix_only() {
        // `claude-opus` 不该匹配 `claude-opus-4-5` —— 没有 * 就是精确。
        assert!(!glob_match("claude-opus", "claude-opus-4-5"));
    }

    #[test]
    fn conditions_in_one_when_are_and() {
        // 「同一个 when 里多个条件默认 AND」覆盖九成需求，于是 AND
        // 关键字和括号嵌套直接消失（§3.4）。
        let w = when("{ model: claude-sonnet-*, stream: true }");
        assert!(w.matches(&f()).unwrap());
        let w = when("{ model: claude-sonnet-*, stream: false }");
        assert!(!w.matches(&f()).unwrap(), "有一个不满足就整条不命中");
    }

    #[test]
    fn an_empty_when_is_the_catch_all() {
        assert!(When::default().is_catch_all());
        assert!(When::default().matches(&f()).unwrap());
        assert!(!when("{ model: x }").is_catch_all());
    }

    #[test]
    fn numeric_conditions_compare() {
        assert!(when(r#"{ input_tokens: "<200k" }"#).matches(&f()).unwrap());
        assert!(!when(r#"{ input_tokens: ">200k" }"#).matches(&f()).unwrap());
        assert!(when(r#"{ max_tokens: ">=4k" }"#).matches(&f()).unwrap());
    }

    #[test]
    fn a_missing_max_tokens_never_matches_a_comparison() {
        // 当成 0 的话，`max_tokens: "<4k"` 会意外命中所有没写这个字段
        // 的请求 —— 而那和用户的意思正好相反。
        let mut x = f();
        x.max_tokens = None;
        assert!(!when(r#"{ max_tokens: "<4k" }"#).matches(&x).unwrap());
        assert!(!when(r#"{ max_tokens: ">4k" }"#).matches(&x).unwrap());
    }

    #[test]
    fn boolean_conditions_match_both_ways() {
        let mut x = f();
        x.cache = true;
        assert!(when("{ cache: true }").matches(&x).unwrap());
        assert!(!when("{ cache: false }").matches(&x).unwrap());
        assert!(when("{ cache: false }").matches(&f()).unwrap());
    }

    #[test]
    fn a_broken_comparison_is_caught_at_load_time_not_at_request_time() {
        // 一条永远不命中的规则在运行时是完全静默的 —— 用户只会觉得
        // 「我明明配了」。加载时就报出来。
        let w = when(r#"{ input_tokens: "200k" }"#);
        assert!(w.validate().is_err());
        let e = w.validate().unwrap_err().to_string();
        assert!(e.contains("input_tokens"), "{e}");
    }

    #[test]
    fn an_unknown_condition_key_is_rejected_rather_than_ignored() {
        // 打错一个字段名（`input_token` 少个 s）而被静默忽略，会让规则
        // 变成一条兜底 —— 匹配所有请求，和用户想要的完全相反。
        assert!(serde_yaml_ng::from_str::<When>("{ input_token: \">4k\" }").is_err());
    }

    #[test]
    fn client_and_dialect_are_exact_not_globbed() {
        // 它们的取值是我们自己定义的小集合，通配符只会掩盖打错的名字。
        assert!(when("{ client: claude-code }").matches(&f()).unwrap());
        assert!(!when("{ client: claude-* }").matches(&f()).unwrap());
    }
}
