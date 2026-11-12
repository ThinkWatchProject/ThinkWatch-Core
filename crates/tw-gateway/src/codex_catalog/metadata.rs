//! Project reported API model capabilities without adopting native instructions.

use serde_json::{Value, json};

pub(super) fn ordinary(model: &Value) -> Option<Value> {
    let id = model["id"].as_str().filter(|id| !id.trim().is_empty())?;
    let mut out = json!({"slug": id});
    if let Some(context) = model["context_window"].as_u64().filter(|n| *n > 0) {
        out["context_window"] = json!(context);
    }
    if let Some(modalities) = model["input_modalities"].as_array() {
        out["input_modalities"] = json!(
            modalities
                .iter()
                .filter(|m| *m == "text" || *m == "image")
                .collect::<Vec<_>>()
        );
    }
    let reported = crate::model_capabilities::reported(model);
    if let Some(effort) = reported.reasoning_effort {
        set_reasoning(&mut out, &effort);
    }
    if let Some(reasoning) = reported.reasoning {
        out["reasoning"] = json!(reasoning);
    }
    if let Some(max_output) = reported.max_output_tokens {
        out["max_output_tokens"] = json!(max_output);
    }
    Some(out)
}

/// Preserve native descriptions for discovered levels; hand-written levels get plain labels.
pub(super) fn set_reasoning(out: &mut Value, effort: &tw_config::ReasoningEffort) {
    let levels: Vec<_> = effort.supported_levels.iter().map(|level| {
        out["supported_reasoning_levels"].as_array()
            .and_then(|levels| levels.iter().find(|v| v["effort"] == level.slug())).cloned()
            .unwrap_or_else(|| json!({"effort":level.slug(),"description":format!("Reasoning effort: {level}")}))
    }).collect();
    out["supported_reasoning_levels"] = json!(levels);
    out["default_reasoning_level"] = json!(effort.default_level);
}

/// Definitions may disagree on prompts or tools while sharing effort choices.
/// Keep only levels that every routing candidate explicitly supports.
pub(super) fn apply_reasoning(
    out: &mut Value,
    profiles: &[Option<tw_config::ReasoningEffort>],
    requires_translation: bool,
    first_definition: Option<&Value>,
) {
    let common = crate::model_capabilities::common_effort(
        &profiles.iter().map(Option::as_ref).collect::<Vec<_>>(),
        requires_translation,
    )
    .unwrap_or_default();
    if let Some(levels) = first_definition.and_then(|p| p["supported_reasoning_levels"].as_array())
    {
        out["supported_reasoning_levels"] = json!(levels);
    }
    set_reasoning(out, &common);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_capabilities_are_reported_values_without_instructions_or_tools() {
        let out = ordinary(&json!({
            "id":"private-model", "context_window":1048576,
            "input_modalities":["text","image","audio"],
            "effort":{"supported_levels":["low","high","max"],"default_level":"high"},
            "base_instructions":"Do not copy", "experimental_supported_tools":["do_not_copy"]
        }))
        .unwrap();
        assert_eq!(out["context_window"], 1048576);
        assert_eq!(out["input_modalities"], json!(["text", "image"]));
        assert_eq!(out["default_reasoning_level"], "high");
        assert_eq!(out["supported_reasoning_levels"][2]["effort"], "max");
        assert!(out.get("base_instructions").is_none());
        assert!(out.get("experimental_supported_tools").is_none());
    }

    #[test]
    fn missing_and_malformed_capabilities_do_not_create_claims_or_invalid_codex_levels() {
        assert!(ordinary(&json!({"id":" "})).is_none());
        let mut out = ordinary(&json!({"id":"private-model","context_window":-1})).unwrap();
        assert!(out.get("context_window").is_none());
        assert!(out.get("supported_reasoning_levels").is_none());
        let profile = ordinary(&json!({"id":"private-model","effort":{
            "supported_levels":["high",true,"unknown","max","high"],"default_level":"invalid"
        }}))
        .unwrap();
        assert!(profile["default_reasoning_level"].is_null());
        out = json!({});
        apply_reasoning(
            &mut out,
            &[crate::model_capabilities::reported(&profile).reasoning_effort],
            false,
            Some(&profile),
        );
        assert_eq!(
            out["supported_reasoning_levels"].as_array().unwrap().len(),
            2
        );
        assert_eq!(out["default_reasoning_level"], "high");
    }
}
