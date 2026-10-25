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

/// 最后一条是工具结果，而发起调用的那一轮没有 Anthropic 签发的思考块。
///
/// Claude 按预算思考时，工具调用那一轮必须以思考块开头。那一轮来自别家（或者当时
/// 没开思考）时手里没有这样的块，开着思考发过去是 400 —— 这时只能不开。Anthropic
/// 和 Bedrock 上的 Claude 是同一个规矩。
pub(crate) fn continues_tool_turn_without_thinking(messages: &[crate::ir::Message]) -> bool {
    use crate::ir::{Part, Role, Thinking, Vendor, merge_roles};
    let merged = merge_roles(messages.to_vec());
    let [.., assistant, user] = merged.as_slice() else {
        return false;
    };
    if assistant.role != Role::Assistant
        || !user.parts.iter().any(|p| matches!(p, Part::ToolResult(_)))
        || !assistant
            .parts
            .iter()
            .any(|p| matches!(p, Part::ToolCall(_)))
    {
        return false;
    }
    !matches!(
        assistant.parts.first(),
        Some(Part::Thinking(Thinking { signature: Some(s), .. })) if s.vendor == Vendor::Anthropic
    )
}

/// 由低到高的全部档位
pub const LEVELS: [Effort; 6] = [
    Effort::Minimal,
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::XHigh,
    Effort::Max,
];

/// `levels` 里离 `want` 最近的那一档。一样近的取高的那一档：上游说「只有 high 和 max」时，
/// 要 xhigh 的客户端要的是多想，不是少想。`levels` 是空的是 `None`
pub fn nearest(levels: &[Effort], want: Effort) -> Option<Effort> {
    let rank = |e: Effort| e as i32;
    levels
        .iter()
        .copied()
        .min_by_key(|e| ((rank(*e) - rank(want)).abs(), std::cmp::Reverse(rank(*e))))
}

/// 推理强度被上游拒了之后，重发时怎么写。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortFix {
    /// 改成这一档
    Use(Effort),
    /// 去掉强度，按模型的默认
    Drop,
}

/// 上游这个错误是不是因为推理强度：是的话，重发时怎么写。`sent` 是这一跳发出去的那一档。
///
/// **只认 400**，而且原话（不分大小写）里要说到 `effort`、或者整词说到发出去的那一档、或者
/// 整词说到另外至少两档 —— 「请使用 low、high 或 max」这样不提 `effort` 的也认得出。
///
/// 重发写哪一档看原话里的档位（不算发出去的那一档）：恰好一档后面紧跟着「or below」「or
/// lower」「以下」的，那一档是上限，就用它；不然取离发出去的那一档最近的（[`nearest`]）；
/// 一档都没说的，去掉强度。
pub fn effort_rejection(status: u16, error_body: &str, sent: Effort) -> Option<EffortFix> {
    if status != 400 {
        return None;
    }
    let text = error_body.to_lowercase();
    let found = levels_in(&text);
    let others: Vec<(usize, Effort)> = found.iter().copied().filter(|(_, e)| *e != sent).collect();
    let mut distinct: Vec<Effort> = others.iter().map(|(_, e)| *e).collect();
    distinct.sort();
    distinct.dedup();
    let about_effort =
        text.contains("effort") || found.iter().any(|(_, e)| *e == sent) || distinct.len() >= 2;
    if !about_effort {
        return None;
    }
    let mut ceilings: Vec<Effort> = others
        .iter()
        .filter(|(end, _)| ceiling_after(&text[*end..]))
        .map(|(_, e)| *e)
        .collect();
    ceilings.sort();
    ceilings.dedup();
    if let [only] = ceilings.as_slice() {
        return Some(EffortFix::Use(*only));
    }
    Some(match nearest(&distinct, sent) {
        Some(e) => EffortFix::Use(e),
        None => EffortFix::Drop,
    })
}

/// 小写的 `text` 里整词出现的档位名：每一处的结尾位置和是哪一档
fn levels_in(text: &str) -> Vec<(usize, Effort)> {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = Vec::new();
    for e in LEVELS {
        let name = openai(e);
        for (at, _) in text.match_indices(name) {
            let end = at + name.len();
            let before = text[..at].chars().next_back();
            let after = text[end..].chars().next();
            if !before.is_some_and(word) && !after.is_some_and(word) {
                out.push((end, e));
            }
        }
    }
    out.sort_by_key(|(end, _)| *end);
    out
}

/// 档位名后面紧跟着的是不是「以下」：中间可以隔着引号、括号和空白
fn ceiling_after(rest: &str) -> bool {
    let rest = rest.trim_start_matches(|c: char| {
        c.is_whitespace() || matches!(c, '\'' | '"' | '`' | '’' | '”' | '」' | ')' | '）')
    });
    ["or below", "or lower", "以下"]
        .iter()
        .any(|p| rest.starts_with(p))
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
    fn the_nearest_level_breaks_ties_upward() {
        use Effort::*;
        assert_eq!(nearest(&[Low, High, Max], Medium), Some(High));
        assert_eq!(nearest(&[High, Max], XHigh), Some(Max));
        assert_eq!(nearest(&[Low, Medium], Max), Some(Medium));
        assert_eq!(nearest(&[Minimal], High), Some(Minimal));
        assert_eq!(nearest(&[], High), None);
    }

    #[test]
    fn an_effort_rejection_names_what_to_send_instead() {
        use Effort::*;
        // Anthropic：列出认的那几档 → 离 xhigh 最近的，high 和 max 一样近，取 max
        let anthropic = r#"{"type":"error","error":{"type":"invalid_request_error","message":"The parameter `output_config.effort` specified in the request are not valid: expected `low`, `medium`, `high` or `max`, but got `xhigh` instead."}}"#;
        assert_eq!(
            effort_rejection(400, anthropic, XHigh),
            Some(EffortFix::Use(Max))
        );
        // 「high 或以下」：上限
        let ceiling = "output_config.effort 'xhigh' is not supported when thinking is disabled on this model. Use effort 'high' or below, or enable thinking.";
        assert_eq!(
            effort_rejection(400, ceiling, XHigh),
            Some(EffortFix::Use(High))
        );
        assert_eq!(
            effort_rejection(400, "effort max is too high; use medium or lower", Max),
            Some(EffortFix::Use(Medium))
        );
        // 不提 effort、也不提发出去的那一档，列了别的几档
        let glm = r#"{"error":{"code":"1210","message":"该模型始终思考，不支持关闭思考；请使用 low、high 或 max。"}}"#;
        assert_eq!(
            effort_rejection(400, glm, Medium),
            Some(EffortFix::Use(High))
        );
        // 说到了强度、没说用哪一档：去掉
        assert_eq!(
            effort_rejection(400, "reasoning_effort is not supported by this model", High),
            Some(EffortFix::Drop)
        );
        assert_eq!(
            effort_rejection(400, "Unsupported value: 'xhigh'", XHigh),
            Some(EffortFix::Drop)
        );
        // 与强度无关的 400、不是 400 的：不管
        assert_eq!(
            effort_rejection(
                400,
                r#"{"error":{"message":"messages: field required"}}"#,
                High
            ),
            None
        );
        assert_eq!(effort_rejection(500, anthropic, XHigh), None);
        // 只说到一个别的档位、不是整词的不算
        assert_eq!(
            effort_rejection(400, "max_tokens must be at most 8192", High),
            None
        );
        assert_eq!(effort_rejection(400, "xhighly unusual", High), None);
    }

    #[test]
    fn openai_none_turns_reasoning_off() {
        assert_eq!(parse_openai("none"), Some(None));
        assert_eq!(parse_openai("xhigh"), Some(Some(Effort::XHigh)));
        assert_eq!(parse_openai("?"), None);
    }
}
