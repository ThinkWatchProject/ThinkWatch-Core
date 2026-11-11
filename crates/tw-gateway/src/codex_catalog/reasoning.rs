//! Reasoning choices require actual levels, not just a `reasoning: true` flag.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde_json::{Value, json};

pub(super) fn profile(model: &str, version: &str) -> Option<&'static Value> {
    static PROFILES: OnceLock<BTreeMap<String, Value>> = OnceLock::new();
    let profiles = PROFILES.get_or_init(|| {
        serde_json::from_str(include_str!("../../data/codex_reasoning.json"))
            .expect("bundled reasoning profiles are valid JSON")
    });
    // Providers commonly qualify an unchanged model ID with their namespace.
    // Resolve this against the routing candidate, never the public alias name.
    let name = model.rsplit('/').next()?.to_ascii_lowercase();
    let profile = profiles.get(&name)?;
    let minimum = profile["minimal_client_version"].as_str()?;
    (parse_version(version)? >= parse_version(minimum)?).then_some(profile)
}

fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let parsed = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(parsed)
}

/// Definitions may disagree on prompts or tools while sharing effort choices.
/// Keep only levels that every routing candidate explicitly supports.
pub(super) fn apply(out: &mut Value, profiles: &[Option<&Value>], requires_translation: bool) {
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
    fn every_bundled_profile_has_valid_unique_choices_and_an_available_default() {
        let profiles: BTreeMap<String, Value> =
            serde_json::from_str(include_str!("../../data/codex_reasoning.json")).unwrap();
        for (model, profile) in profiles {
            assert_eq!(model, model.to_ascii_lowercase());
            assert_eq!(profile.as_object().unwrap().len(), 3, "{model}");
            assert!(parse_version(profile["minimal_client_version"].as_str().unwrap()).is_some());
            let levels = profile["supported_reasoning_levels"].as_array().unwrap();
            let mut efforts = std::collections::BTreeSet::new();
            for level in levels {
                let effort = level["effort"].as_str().unwrap();
                assert!(matches!(
                    effort,
                    "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
                ));
                assert!(efforts.insert(effort), "duplicate effort in {model}");
                assert!(!level["description"].as_str().unwrap().trim().is_empty());
            }
            assert!(efforts.contains(profile["default_reasoning_level"].as_str().unwrap()));
        }
    }

    #[test]
    fn bundled_profiles_are_gated_by_client_version_and_exact_model_identity() {
        assert!(profile("openai/gpt-6-astra", "0.153.0").is_some());
        assert!(profile("gpt-6-astra", "0.152.9").is_none());
        assert!(profile("DeepSeek/DeepSeek-V4.1-Flash", "0.166.0").is_some());
        assert!(profile("deepseek-v4.1-flash", "0.165.0").is_none());
        assert!(profile("deepseek-reasoner", "0.166.0").is_none());
        assert!(profile("deepseek-v99-flash", "0.166.0").is_none());
        assert!(profile("gpt-5.5-custom", "0.166.0").is_none());
    }
}
