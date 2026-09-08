//! 语义校验。**解析成功不等于配置对**，而错误信息要能直接行动。

use crate::{Config, SCHEMA_VERSION};

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error(
        "配置 schema 版本是 {found}，这个版本的 twcore 只认到 {supported}。\n请升级应用，或把配置改回旧格式。"
    )]
    SchemaTooNew { found: u32, supported: u32 },
    #[error("配置里一个 provider 都没有。至少要有一个上游才能转发请求。")]
    NoProviders,
    #[error(
        "配置里一个 client 都没有。没有网关密钥的话，任何请求都会被拒绝 —— 首次启动本应自动生成一把。"
    )]
    NoClients,
    #[error("provider 名字重复：{0}。名字是路由规则里引用它的方式，必须唯一。")]
    DuplicateProvider(String),
    #[error("client 名字重复：{0}")]
    DuplicateClient(String),
    #[error("两个 client 用了同一把密钥（{0} 和 {1}）。密钥就是身份，重复等于这两个客户端分不开。")]
    DuplicateKey(String, String),
    #[error("provider `{name}` 的 base_url 不是 http/https：{url}")]
    BadBaseUrl { name: String, url: String },
    #[error("client `{name}` 的密钥是空的")]
    EmptyKey { name: String },
    #[error(
        "provider `{name}` 的密钥是空的。要连不需要密钥的上游（比如本地 Ollama），写一个占位值即可。"
    )]
    EmptyProviderKey { name: String },
}

pub fn validate(cfg: &Config) -> Result<(), ValidationError> {
    // 版本检查放在最前面：一个来自更新版本的配置，我们对它的任何
    // 其他判断都不作数（§9.7 的「schema 太新」）。
    if cfg.version > SCHEMA_VERSION {
        return Err(ValidationError::SchemaTooNew {
            found: cfg.version,
            supported: SCHEMA_VERSION,
        });
    }
    if cfg.providers.is_empty() {
        return Err(ValidationError::NoProviders);
    }
    if cfg.clients.is_empty() {
        return Err(ValidationError::NoClients);
    }

    let mut seen = std::collections::HashSet::new();
    for p in &cfg.providers {
        if !seen.insert(&p.name) {
            return Err(ValidationError::DuplicateProvider(p.name.clone()));
        }
        if !(p.base_url.starts_with("http://") || p.base_url.starts_with("https://")) {
            return Err(ValidationError::BadBaseUrl {
                name: p.name.clone(),
                url: p.base_url.clone(),
            });
        }
        if p.key.trim().is_empty() {
            return Err(ValidationError::EmptyProviderKey {
                name: p.name.clone(),
            });
        }
    }

    let mut names = std::collections::HashSet::new();
    let mut keys: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for c in &cfg.clients {
        if !names.insert(&c.name) {
            return Err(ValidationError::DuplicateClient(c.name.clone()));
        }
        if c.key.trim().is_empty() {
            return Err(ValidationError::EmptyKey {
                name: c.name.clone(),
            });
        }
        if let Some(prev) = keys.insert(&c.key, &c.name) {
            return Err(ValidationError::DuplicateKey(
                prev.to_string(),
                c.name.clone(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Client, Listen, Provider};

    fn cfg(clients: Vec<Client>, providers: Vec<Provider>) -> Config {
        Config {
            version: 1,
            listen: Listen::default(),
            clients,
            providers,
        }
    }
    fn c(name: &str, key: &str) -> Client {
        Client {
            name: name.into(),
            key: key.into(),
        }
    }
    fn p(name: &str, url: &str) -> Provider {
        Provider {
            name: name.into(),
            base_url: url.into(),
            key: "sk-x".into(),
            protocol: None,
        }
    }

    #[test]
    fn a_valid_minimal_config_passes() {
        assert!(validate(&cfg(vec![c("d", "tw-1")], vec![p("r", "https://x.com")])).is_ok());
    }

    #[test]
    fn a_newer_schema_is_refused_before_anything_else_is_judged() {
        // 顺序很重要：来自新版本的配置，我们对它的其他判断都不作数。
        let mut k = cfg(vec![], vec![]); // 同时还缺 provider 和 client
        k.version = 99;
        assert!(matches!(
            validate(&k),
            Err(ValidationError::SchemaTooNew { found: 99, .. })
        ));
    }

    #[test]
    fn duplicate_client_keys_are_refused() {
        // 密钥即身份，两个客户端共用一把等于它们分不开 —— 分开算钱、
        // 分开路由都做不到，而用户会以为配好了。
        let e = validate(&cfg(
            vec![c("a", "tw-same"), c("b", "tw-same")],
            vec![p("r", "https://x.com")],
        ));
        assert!(matches!(e, Err(ValidationError::DuplicateKey(..))));
    }

    #[test]
    fn empty_config_says_which_half_is_missing() {
        assert!(matches!(
            validate(&cfg(vec![], vec![])),
            Err(ValidationError::NoProviders)
        ));
        assert!(matches!(
            validate(&cfg(vec![], vec![p("r", "https://x.com")])),
            Err(ValidationError::NoClients)
        ));
    }

    #[test]
    fn a_base_url_without_a_scheme_is_refused() {
        // 「api.example.com」是最常见的手滑，而它的失败模式是建连时的
        // 一个费解错误。在这里挡住，说清楚。
        assert!(matches!(
            validate(&cfg(vec![c("d", "tw-1")], vec![p("r", "api.example.com")])),
            Err(ValidationError::BadBaseUrl { .. })
        ));
    }

    #[test]
    fn error_messages_say_what_to_do_next() {
        // 错误信息是降低使用难度最有效的杠杆（§0.6）。判据不是「说清
        // 哪里错了」，是「说清接下来做什么」。
        let e = validate(&cfg(vec![c("d", "tw-1")], vec![])).unwrap_err();
        assert!(e.to_string().contains("至少要有一个上游"));
    }
}
