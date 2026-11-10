//! Client model-list presentation. Never use this filter for admission or routing.

use std::collections::{BTreeMap, BTreeSet};

/// Original names replaced in this already-authorized, protocol-filtered list.
/// An unavailable/inaccessible alias cannot hide another usable name. Alias
/// identities always win, including aliases sharing a target or their own name.
pub fn hidden_originals(
    cfg: &tw_config::Config,
    allowed: &[String],
) -> BTreeMap<String, Vec<String>> {
    let allowed: BTreeSet<&str> = allowed.iter().map(String::as_str).collect();
    let mut hidden: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for alias in cfg
        .aliases
        .iter()
        .filter(|a| a.hide_originals && allowed.contains(a.name.as_str()))
    {
        for original in &alias.models {
            if allowed.contains(original.as_str()) && !cfg.aliases.contains(original) {
                hidden
                    .entry(original.clone())
                    .or_default()
                    .push(alias.name.clone());
            }
        }
    }
    hidden
}

/// Discovery is a view of existing permissions, not a replacement for them.
pub fn models(
    cfg: &tw_config::Config,
    catalog: &tw_engine::Catalog,
    protocols: Option<&[&str]>,
    allow: Option<&[String]>,
) -> Vec<String> {
    let allowed = catalog.resolve_allowed(protocols, allow);
    let hidden = hidden_originals(cfg, &allowed);
    allowed
        .into_iter()
        .filter(|name| !hidden.contains_key(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tw_engine::{Catalog, ProviderModels};

    fn cfg(text: &str) -> tw_config::Config {
        tw_config::Config {
            aliases: serde_yaml_ng::from_str(text).unwrap(),
            ..Default::default()
        }
    }
    fn catalog(cfg: &tw_config::Config) -> Catalog {
        Catalog::build(&[
            ProviderModels {
                provider: "a".into(),
                protocol: "anthropic".into(),
                known: true,
                models: vec!["raw-a".into(), "raw-b".into(), "other".into()],
            },
            ProviderModels {
                provider: "b".into(),
                protocol: "openai-responses".into(),
                known: true,
                models: vec!["raw-c".into()],
            },
        ])
        .with_aliases(
            cfg.aliases
                .iter()
                .map(|a| (a.name.as_str(), a.models.as_slice())),
        )
    }
    #[test]
    fn discovery_filters_only_exact_originals_and_keeps_admission_and_inventory() {
        let cfg = cfg(
            "short: {models: [raw-a, raw-b], hide_originals: true}\nother-alias: raw-a\nghost: {models: [missing], hide_originals: true}",
        );
        let c = catalog(&cfg);
        assert_eq!(
            models(&cfg, &c, None, None),
            ["other", "other-alias", "raw-c", "short"]
        );
        assert!(c.admits("raw-a", None, None));
        assert_eq!(c.offers("a", "raw-a"), Some(true));
        assert!(models(&cfg, &c, None, Some(&[])).is_empty());
        assert_eq!(
            models(&cfg, &c, Some(&["openai-responses"]), None),
            ["raw-c"]
        );
        assert_eq!(models(&cfg, &c, None, Some(&["short".into()])), ["short"]);
        // A target grant inherits aliases, but never grants the other targets.
        assert_eq!(
            models(&cfg, &c, None, Some(&["raw-a".into()])),
            ["other-alias", "short"]
        );
        assert!(!c.admits("raw-b", None, Some(&["raw-a".into()])));
    }
    #[test]
    fn shared_targets_restore_only_after_last_hiding_alias_is_disabled_or_deleted() {
        let mut cfg = cfg(
            "a: {models: [raw-a], hide_originals: true}\nb: {models: [raw-a], hide_originals: true}",
        );
        assert_eq!(
            hidden_originals(&cfg, &catalog(&cfg).resolve_allowed(None, None))["raw-a"],
            ["a", "b"]
        );
        cfg.aliases.0.remove(0);
        assert!(!models(&cfg, &catalog(&cfg), None, None).contains(&"raw-a".into()));
        cfg.aliases.0[0].hide_originals = false;
        assert!(models(&cfg, &catalog(&cfg), None, None).contains(&"raw-a".into()));
    }
    #[test]
    fn alias_identity_and_unavailable_aliases_cannot_hide_names() {
        let cfg = cfg(
            "raw-a: {models: [raw-a, raw-b], hide_originals: true}\nshort: {models: [raw-c], hide_originals: true}",
        );
        let c = catalog(&cfg);
        assert!(models(&cfg, &c, None, None).contains(&"raw-a".into()));
        // Apply only to aliases present in the current authorized/protocol list.
        assert!(hidden_originals(&cfg, &["raw-c".into()]).is_empty());
        assert!(!models(&cfg, &c, Some(&["anthropic"]), None).contains(&"short".into()));
        assert!(Catalog::default().resolve_allowed(None, None).is_empty());
        assert!(models(&cfg, &Catalog::default(), None, None).is_empty());
    }
}
