use crate::Config;

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
    for value in [&c.language, &c.region] {
        if value.len() > 35
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err("language and region must be empty or short language/region codes".into());
        }
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
