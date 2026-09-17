//! 推理强度在几家之间怎么对应。
//!
//! 几家控制推理的旋钮不一样：Anthropic 旧模型用 token 预算，新模型用 `effort`；
//! OpenAI 只有 `effort`；Gemini 2.5 用预算，Gemini 3 用等级。**这里的对应是约定，
//! 不是换算** —— 同一个 `high` 在两家花的 token 不会一样。取值参照 Claude Code 的
//! 三档思考预算（4000 / 10000 / 31999）。

use crate::ir::{Effort, Reasoning};

pub fn budget_of_effort(e: Effort) -> u64 {
    match e {
        Effort::Minimal => 1024,
        Effort::Low => 4000,
        Effort::Medium => 10000,
        Effort::High => 32000,
        Effort::XHigh => 48000,
        Effort::Max => 64000,
    }
}

pub fn effort_of_budget(b: u64) -> Effort {
    match b {
        0..2048 => Effort::Minimal,
        2048..8000 => Effort::Low,
        8000..20000 => Effort::Medium,
        20000..40000 => Effort::High,
        40000..56000 => Effort::XHigh,
        _ => Effort::Max,
    }
}

/// 客户端给了强度就用强度，只给了预算就按预算折算
pub fn effort(r: &Reasoning) -> Option<Effort> {
    r.effort.or(r.budget.map(effort_of_budget))
}

/// 客户端给了预算就用预算，只给了强度就按强度折算
pub fn budget(r: &Reasoning) -> Option<u64> {
    r.budget.or(r.effort.map(budget_of_effort))
}

/// OpenAI 的写法。`none` 是关掉推理，读成 `Some(None)`
pub fn parse_openai(s: &str) -> Option<Option<Effort>> {
    Some(match s {
        "none" => None,
        "minimal" => Some(Effort::Minimal),
        "low" => Some(Effort::Low),
        "medium" => Some(Effort::Medium),
        "high" => Some(Effort::High),
        "xhigh" => Some(Effort::XHigh),
        "max" => Some(Effort::Max),
        _ => return None,
    })
}

pub fn openai(e: Effort) -> &'static str {
    match e {
        Effort::Minimal => "minimal",
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh => "xhigh",
        Effort::Max => "max",
    }
}

/// Anthropic 的 `output_config.effort` 没有 `minimal`
pub fn anthropic(e: Effort) -> &'static str {
    match e {
        Effort::Minimal | Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh => "xhigh",
        Effort::Max => "max",
    }
}

pub fn parse_anthropic(s: &str) -> Option<Effort> {
    Some(match s {
        "low" => Effort::Low,
        "medium" => Effort::Medium,
        "high" => Effort::High,
        "xhigh" => Effort::XHigh,
        "max" => Effort::Max,
        _ => return None,
    })
}

/// Gemini 3 的 `thinkingLevel`
pub fn gemini_level(e: Effort) -> &'static str {
    match e {
        Effort::Minimal => "MINIMAL",
        Effort::Low => "LOW",
        Effort::Medium => "MEDIUM",
        Effort::High | Effort::XHigh | Effort::Max => "HIGH",
    }
}

pub fn parse_gemini_level(s: &str) -> Option<Effort> {
    Some(match s.to_ascii_uppercase().as_str() {
        "MINIMAL" => Effort::Minimal,
        "LOW" => Effort::Low,
        "MEDIUM" => Effort::Medium,
        "HIGH" => Effort::High,
        _ => return None,
    })
}

/// 这个 Claude 模型用自适应思考（`thinking.type: adaptive` + `output_config.effort`）。
///
/// **按模型名判断，因为两种写法互不兼容**：4.5 及更早的模型只认 `enabled` +
/// `budget_tokens`，写 `adaptive` 返回 400；4.7 及以后的模型只认 `adaptive`，写
/// `enabled` 返回 400；4.6 两种都认，`enabled` 已弃用。不是 `claude-` 开头的模型名
/// （DeepSeek、Kimi 这类 Anthropic 兼容接口）按旧写法：兼容实现跟的是早先的接口。
pub fn claude_adaptive(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    if m.contains("fable") || m.contains("mythos") {
        return true;
    }
    let Some(i) = m.find("claude-") else {
        return false;
    };
    // claude-<系列>-<主版本>[-<次版本>]：次版本是一两位数字，八位的是日期
    let mut parts = m[i + "claude-".len()..].split('-');
    let family = parts.next().unwrap_or("");
    if !matches!(family, "opus" | "sonnet" | "haiku") {
        return false;
    }
    let Some(major) = parts.next().and_then(|p| p.parse::<u32>().ok()) else {
        return false;
    };
    let minor = parts
        .next()
        .filter(|p| p.len() <= 2)
        .and_then(|p| p.parse::<u32>().ok())
        .unwrap_or(0);
    (major, minor) >= (4, 6)
}

/// 这个 Gemini 模型用 `thinkingLevel` 而不是 `thinkingBudget`
pub fn gemini_uses_level(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    let m = m.strip_prefix("models/").unwrap_or(&m);
    m.strip_prefix("gemini-")
        .and_then(|rest| rest.split(['-', '.']).next())
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_models_are_split_at_4_6() {
        for m in [
            "claude-opus-4-6",
            "claude-sonnet-4-6-20260217",
            "claude-opus-4-7",
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-fable-5-1",
            "anthropic/claude-opus-4-8",
        ] {
            assert!(claude_adaptive(m), "{m}");
        }
        for m in [
            "claude-sonnet-4-5",
            "claude-sonnet-4-5-20250929",
            "claude-opus-4-1",
            "claude-haiku-4-5",
            "claude-3-7-sonnet-20250219",
            "claude-sonnet-4-20250514",
            "deepseek-chat",
            "kimi-k2",
        ] {
            assert!(!claude_adaptive(m), "{m}");
        }
    }

    #[test]
    fn gemini_3_uses_levels() {
        assert!(gemini_uses_level("gemini-3-pro-preview"));
        assert!(gemini_uses_level("models/gemini-3.1-flash"));
        assert!(!gemini_uses_level("gemini-2.5-pro"));
        assert!(!gemini_uses_level("gemma-3"));
    }

    #[test]
    fn budget_and_effort_round_trip_to_the_same_tier() {
        for e in [
            Effort::Minimal,
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::XHigh,
            Effort::Max,
        ] {
            assert_eq!(effort_of_budget(budget_of_effort(e)), e);
        }
    }

    #[test]
    fn openai_none_turns_reasoning_off() {
        assert_eq!(parse_openai("none"), Some(None));
        assert_eq!(parse_openai("xhigh"), Some(Some(Effort::XHigh)));
        assert_eq!(parse_openai("?"), None);
    }
}
