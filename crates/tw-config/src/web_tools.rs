use crate::Config;
use icu_locale_core::LanguageIdentifier;
use tw_api::{WebSearchLocale, WebToolsConfig};

/// Read the execution host, never the UI/browser's language. A service with only
/// C/POSIX configured has no language preference; leave that to the search engine.
pub fn system_locale() -> WebSearchLocale {
    sys_locale::get_locale()
        .and_then(|s| parse_locale(&s))
        .unwrap_or_default()
}

fn parse_locale(value: &str) -> Option<WebSearchLocale> {
    let normalized = value.split(['.', '@']).next()?.replace('_', "-");
    let mut id: LanguageIdentifier = normalized.parse().ok()?;
    if id.language.is_unknown() {
        return None;
    }
    let region = id.region.take().map(|r| r.to_string()).unwrap_or_default();
    // Preserve the writing system when a platform supplies zh-CN/zh-TW without
    // a script. Do not infer a country from a language (English is not always US).
    if id.language.as_str() == "zh" && id.script.is_none() {
        id.script = match region.as_str() {
            "CN" | "SG" => Some("Hans".parse().expect("script")),
            "TW" | "HK" | "MO" => Some("Hant".parse().expect("script")),
            _ => None,
        };
    }
    Some(WebSearchLocale {
        language: id.to_string(),
        region,
    })
}

/// Overrides are independent. Keep empty configuration fields empty on disk so
/// later system changes continue to apply.
pub fn resolve_locale(config: &WebToolsConfig, system: &WebSearchLocale) -> WebSearchLocale {
    WebSearchLocale {
        language: if config.language.is_empty() {
            system.language.clone()
        } else {
            config.language.clone()
        },
        region: if config.region.is_empty() {
            system.region.clone()
        } else {
            config.region.to_ascii_uppercase()
        },
    }
}

pub(crate) fn validate(cfg: &Config) -> Result<(), String> {
    let c = &cfg.web_tools;
    for (name, value, min, max) in [
        ("max_results", c.max_results, 1, 20),
        ("read_pages", c.read_pages, 0, 5),
        ("max_page_chars", c.max_page_chars, 1000, 50_000),
        ("timeout_secs", c.timeout_secs, 1, 120),
        ("max_concurrent", c.max_concurrent, 1, 16),
        ("interval_ms", c.interval_ms, 250, 60_000),
        ("cache_secs", c.cache_secs, 0, 86_400),
        ("max_tool_calls", c.max_tool_calls, 1, 32),
    ] {
        if !(min..=max).contains(&value) {
            return Err(format!("{name} must be between {min} and {max}"));
        }
    }
    if c.engines
        .iter()
        .collect::<std::collections::HashSet<_>>()
        .len()
        != c.engines.len()
    {
        return Err("engines must not contain duplicates".into());
    }
    if c.proxy != crate::SYSTEM
        && c.proxy != crate::DIRECT
        && !cfg.proxies.iter().any(|p| p.name == c.proxy)
    {
        return Err("proxy must be system, direct, or an existing proxy name".into());
    }
    if c.allowed_domains.len() > 100 || c.blocked_domains.len() > 100 {
        return Err("each domain filter supports at most 100 hostnames".into());
    }
    for domain in c.allowed_domains.iter().chain(&c.blocked_domains) {
        if !valid_domain(domain) {
            return Err(
                "domain rules must be hostnames without a scheme, path, port, or wildcard".into(),
            );
        }
    }
    if !c.language.is_empty()
        && (c.language.len() > 35
            || c.language
                .parse::<LanguageIdentifier>()
                .ok()
                .is_none_or(|id| id.language.is_unknown()))
    {
        return Err("language must be empty or a BCP 47 language tag".into());
    }
    if !c.region.is_empty()
        && (c.region.len() != 2 || !c.region.bytes().all(|b| b.is_ascii_alphabetic()))
    {
        return Err("region must be empty or a two-letter country/region code".into());
    }
    Ok(())
}

fn valid_domain(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_locales_preserve_script_and_do_not_invent_a_region() {
        for (input, language, region) in [
            ("zh_CN.UTF-8", "zh-Hans", "CN"),
            ("zh-TW", "zh-Hant", "TW"),
            ("zh-Hant-HK", "zh-Hant", "HK"),
            ("pt_BR.UTF-8", "pt", "BR"),
            ("en", "en", ""),
        ] {
            assert_eq!(
                parse_locale(input),
                Some(WebSearchLocale {
                    language: language.into(),
                    region: region.into()
                })
            );
        }
        for unknown in ["", "C", "C.UTF-8", "POSIX", "und", "en--US"] {
            assert_eq!(parse_locale(unknown), None, "{unknown}");
        }
    }

    #[test]
    fn manual_overrides_are_independent_and_auto_is_not_persisted() {
        let system = parse_locale("zh-Hans-CN").unwrap();
        let mut config = WebToolsConfig::default();
        assert_eq!(resolve_locale(&config, &system), system);
        config.language = "en".into();
        assert_eq!(resolve_locale(&config, &system).region, "CN");
        config.language.clear();
        config.region = "us".into();
        let effective = resolve_locale(&config, &system);
        assert_eq!(effective.language, "zh-Hans");
        assert_eq!(effective.region, "US");
        assert!(config.language.is_empty());
    }

    #[test]
    fn invalid_web_configuration_is_rejected_before_reload() {
        let mut cfg = Config::default();
        assert!(validate(&cfg).is_ok());
        cfg.web_tools.max_concurrent = 0;
        assert!(validate(&cfg).is_err());
        cfg.web_tools.max_concurrent = 4;
        cfg.web_tools.allowed_domains = vec!["https://example.org/private".into()];
        assert!(validate(&cfg).is_err());
        cfg.web_tools.allowed_domains = vec!["docs.example.org".into()];
        cfg.web_tools.proxy = "missing".into();
        assert!(validate(&cfg).is_err());
    }
}
