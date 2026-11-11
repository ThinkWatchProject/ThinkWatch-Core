//! Project reported API model capabilities without adopting native instructions.

use std::collections::BTreeSet;

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
    if let Some(levels) = model["effort"]["supported_levels"].as_array() {
        let mut seen = BTreeSet::new();
        let levels: Vec<_> = levels
            .iter()
            .filter_map(Value::as_str)
            .filter(|effort| {
                matches!(*effort, "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra")
                    && seen.insert(*effort)
            })
            .map(|effort| json!({"effort": effort, "description": format!("Reasoning effort: {effort}")}))
            .collect();
        let default = model["effort"]["default_level"]
            .as_str()
            .filter(|effort| levels.iter().any(|level| level["effort"] == *effort));
        out["supported_reasoning_levels"] = json!(levels);
        out["default_reasoning_level"] = json!(default);
    }
    Some(out)
}

/// Definitions may disagree on prompts or tools while sharing effort choices.
/// Keep only levels that every routing candidate explicitly supports.
pub(super) fn apply_reasoning(
    out: &mut Value,
    profiles: &[Option<&Value>],
    requires_translation: bool,
) {
    let first = profiles.first().copied().flatten();
    let mut levels = first
        .and_then(|p| p["supported_reasoning_levels"].as_array())
        .cloned()
        .unwrap_or_default();
    levels.retain(|level| {
        level["effort"].as_str().is_some_and(|effort| {
            (!requires_translation || tw_dialect::think::parse_openai(effort).is_some())
                && profiles.iter().all(|profile| {
                    profile
                        .and_then(|p| p["supported_reasoning_levels"].as_array())
                        .is_some_and(|levels| levels.iter().any(|v| v["effort"] == effort))
                })
        })
    });
    let default = first
        .and_then(|p| p["default_reasoning_level"].as_str())
        .filter(|effort| levels.iter().any(|v| v["effort"] == *effort))
        .or_else(|| {
            levels
                .iter()
                .find(|v| v["effort"] == "medium")
                .or_else(|| levels.first())
                .and_then(|v| v["effort"].as_str())
        });
    out["default_reasoning_level"] = json!(default);
    out["supported_reasoning_levels"] = json!(levels);
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
        apply_reasoning(&mut out, &[Some(&profile)], false);
        assert_eq!(
            out["supported_reasoning_levels"].as_array().unwrap().len(),
            2
        );
        assert_eq!(out["default_reasoning_level"], "high");
    }
}
