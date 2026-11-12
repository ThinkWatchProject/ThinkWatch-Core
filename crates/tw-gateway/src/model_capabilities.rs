//! Parse only declared model capabilities, shared by background discovery and Codex.

use serde_json::Value;
use tw_config::{Protocol, ReasoningEffort, ReasoningLevel, ReportedSpec};

pub(crate) fn reported(model: &Value) -> ReportedSpec {
    let effort = model["supported_reasoning_levels"]
        .as_array()
        .map(|levels| (levels, &model["default_reasoning_level"]))
        .or_else(|| {
            model["effort"]["supported_levels"]
                .as_array()
                .map(|levels| (levels, &model["effort"]["default_level"]))
        });
    let reasoning_effort = effort.and_then(|(levels, default)| {
        let supported_levels: Vec<_> = levels
            .iter()
            .filter_map(|v| v.as_str().or_else(|| v["effort"].as_str()))
            .filter_map(ReasoningLevel::from_slug)
            .collect();
        // A nonempty but entirely unrecognized list is unknown, not a declaration of no levels.
        if !levels.is_empty() && supported_levels.is_empty() {
            return None;
        }
        let default_level = default
            .as_str()
            .and_then(ReasoningLevel::from_slug)
            .filter(|level| supported_levels.contains(level));
        let mut effort = ReasoningEffort {
            supported_levels,
            default_level,
        }
        .normalized();
        effort.default_level = default_level;
        Some(effort)
    });
    ReportedSpec {
        context_window: model["context_window"]
            .as_u64()
            .or_else(|| model["inputTokenLimit"].as_u64())
            .filter(|v| *v > 0),
        max_output_tokens: model["max_output_tokens"]
            .as_u64()
            .or_else(|| model["outputTokenLimit"].as_u64())
            .filter(|v| *v > 0),
        reasoning: model["reasoning"]
            .as_bool()
            .or_else(|| model["supports_reasoning"].as_bool())
            .or_else(|| model["thinking"].as_bool()),
        image_input: model["input_modalities"]
            .as_array()
            .map(|modalities| modalities.iter().any(|v| v == "image")),
        reasoning_effort,
    }
}

pub(crate) fn requires_translation(protocol: Option<Protocol>) -> bool {
    !matches!(
        protocol,
        Some(Protocol::OpenaiResponses | Protocol::Chatgpt)
    )
}

/// Protocol restrictions apply equally to manual and discovered declarations.
pub fn usable_effort(
    effort: &ReasoningEffort,
    protocol: Option<Protocol>,
) -> (ReasoningEffort, Vec<ReasoningLevel>) {
    let mut usable = effort.clone();
    let mut filtered = Vec::new();
    if requires_translation(protocol) {
        usable.supported_levels.retain(|level| {
            let supported = tw_dialect::think::parse_openai(level.slug()).is_some();
            if !supported {
                filtered.push(*level);
            }
            supported
        });
    }
    (usable.normalized(), filtered)
}

/// A public model/alias can offer only levels confirmed by every routing candidate.
pub(crate) fn common_effort(
    profiles: &[Option<&ReasoningEffort>],
    translate: bool,
) -> Option<ReasoningEffort> {
    let mut common = profiles.first().copied().flatten()?.clone();
    if profiles.iter().any(Option::is_none) {
        return None;
    }
    common.supported_levels.retain(|level| {
        (!translate || tw_dialect::think::parse_openai(level.slug()).is_some())
            && profiles
                .iter()
                .all(|p| p.is_some_and(|p| p.supported_levels.contains(level)))
    });
    Some(common.normalized())
}
