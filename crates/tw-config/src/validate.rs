//! 语义校验。**解析成功不等于配置对**，而错误信息要能直接行动。

use crate::{Config, SCHEMA_VERSION};

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error(
        "配置的 schema 版本为 {found}，当前版本的 twcore 最高支持 {supported}。请升级应用，或将配置改回旧格式"
    )]
    SchemaTooNew { found: u32, supported: u32 },
    #[error(
        "配置中没有任何网关密钥（clients），所有请求都会被拒绝。首次启动时应已自动生成一个网关密钥"
    )]
    NoClients,
    #[error("上游名称重复：{0}。路由规则按名称引用上游，名称必须唯一")]
    DuplicateProvider(String),
    #[error("网关密钥名称重复：{0}")]
    DuplicateClient(String),
    #[error("网关密钥「{0}」与「{1}」的值相同。网关按密钥区分客户端，密钥值必须唯一")]
    DuplicateKey(String, String),
    #[error("上游「{name}」的接口地址不是 http 或 https 地址：{url}")]
    BadBaseUrl { name: String, url: String },
    #[error("网关密钥「{name}」的值为空")]
    EmptyKey { name: String },
    #[error("路由配置有误：{0}")]
    Routing(#[from] tw_engine::RouteError),
    #[error(
        "「{0}」既是上游名称又是策略组名称，规则中的 to 无法确定指向哪一个，请修改其中一个名称"
    )]
    NameCollision(String),
    #[error("listen.gateway.allow_from 中的「{entry}」有误：{reason}")]
    BadCidr { entry: String, reason: String },
    #[error("上游「{name}」的凭据：{source}")]
    Credential {
        name: String,
        source: crate::CredentialError,
    },
    #[error("{0}")]
    Pricing(#[from] tw_pricing::SheetError),
    #[error("上游「{provider}」使用的价目表「{sheet}」不存在")]
    UnknownPriceSheet { provider: String, sheet: String },
    #[error(
        "上游「{name}」的启用范围（models_only）为空，该上游将不提供任何模型。如需暂停使用该上游，请将其停用（disabled: true）"
    )]
    EmptyModelsOnly { name: String },
    #[error("上游「{name}」的启用范围（models_only）中存在空项")]
    BlankModelsOnly { name: String },
    #[error("{what}名称「{name}」以 __ 开头。以 __ 开头的名称留给内置项，请使用其他名称")]
    ReservedName { what: &'static str, name: String },
}

pub fn validate(cfg: &Config) -> Result<(), ValidationError> {
    // 版本检查放在最前面：一个来自更新版本的配置，我们对它的任何
    // 其他判断都不作数（「schema 太新」）。
    if cfg.version > SCHEMA_VERSION {
        return Err(ValidationError::SchemaTooNew {
            found: cfg.version,
            supported: SCHEMA_VERSION,
        });
    }
    // **零个 provider 是合法的**，这是实现时改的一个设计：
    //
    // 首次运行的第一步是生成一份还没有上游的配置，如果那样的
    // 配置过不了校验，core 就起不来 —— 而 core 起不来意味着控制面也
    // 起不来，UI 连「你还没配上游」都说不出口，只能显示一个启动失败。
    //
    // 正确的分工是：**配置合法 ≠ 能转发**。零 provider 的配置能加载、
    // 控制面能起来、引导流程能跑；数据面在收到请求时给一条说清楚下一
    // 步的错误。这和「配一个 API 就能用」是同一条线 —— 那句话的
    // 前提是应用能打开。
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
        p.check_credential()
            .map_err(|source| ValidationError::Credential {
                name: p.name.clone(),
                source,
            })?;
        // **空范围不是「全部」，也不是一个合理的「停用」。**两种读法各有
        // 人会当真，而停用有自己的开关
        if let Some(only) = &p.models_only {
            if only.is_empty() {
                return Err(ValidationError::EmptyModelsOnly {
                    name: p.name.clone(),
                });
            }
            if only.iter().any(|m| m.trim().is_empty()) {
                return Err(ValidationError::BlankModelsOnly {
                    name: p.name.clone(),
                });
            }
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
    // `__` 开头的名字留给内置项（「全部上游」在配置里叫 `__all__`）。**撞上了
    // 不是报一个重名那么简单**：规则写 `to: __all__` 时指的是谁，就取决于
    // 实现顺序了
    let reserved = |n: &str| n.starts_with(tw_engine::RESERVED_PREFIX);
    let named = cfg
        .providers
        .iter()
        .map(|p| ("上游", &p.name))
        .chain(cfg.groups.iter().map(|g| ("策略组", &g.name)))
        .chain(cfg.routes.iter().map(|r| ("路由", &r.name)));
    for (what, name) in named {
        if reserved(name) {
            return Err(ValidationError::ReservedName {
                what,
                name: name.clone(),
            });
        }
    }

    // provider 和组不能同名。`to: x` 指向哪个会变成一个靠实现顺序决定
    // 的问题 —— 而那种问题在换一个人读代码的时候就会变成 bug。
    let group_names: std::collections::HashSet<&str> =
        cfg.groups.iter().map(|g| g.name.as_str()).collect();
    for p in &cfg.providers {
        if group_names.contains(p.name.as_str()) {
            return Err(ValidationError::NameCollision(p.name.clone()));
        }
    }

    // 来源白名单的 CIDR 在加载时查。**写错一条的后果是它永远不匹配**，
    // 而表现是「局域网里那台机器连不上」—— 一条完全看不出原因的故障。
    for entry in &cfg.listen.gateway.allow_from {
        if let Err(e) = entry.parse::<std::net::IpAddr>()
            && entry.parse::<CidrLike>().is_err()
        {
            let _ = e;
            return Err(ValidationError::BadCidr {
                entry: entry.clone(),
                reason: "不是合法的 IP 地址或 CIDR，应写成 192.168.0.0/16 的形式".to_string(),
            });
        }
    }

    // 价目表本身（名字、倍率、单价），以及上游选的价目表在不在。
    // **选了一张不存在的价目表不能静默退回默认价** —— 那样算出来的钱
    // 看起来正常，而用户设的折扣从来没生效过。
    cfg.pricing.validate()?;
    for p in &cfg.providers {
        if let Some(sheet) = &p.pricing
            && cfg.pricing.sheet(sheet).is_none()
        {
            return Err(ValidationError::UnknownPriceSheet {
                provider: p.name.clone(),
                sheet: sheet.clone(),
            });
        }
    }

    // 路由规则的目标、比较式写法，都在这里查。**一条永远不命中、或者
    // 指向不存在的 provider 的规则，在运行时是完全静默的**（
    // 「我明明配了为什么不生效」）。
    cfg.engine().validate()?;
    Ok(())
}

/// 最小的 CIDR 形状校验。**真正的匹配逻辑在 tw-gateway::access** ——
/// 这里只是不想让 tw-config 依赖数据面，而「这条写法对不对」是配置层
/// 该回答的问题。
struct CidrLike;

impl std::str::FromStr for CidrLike {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        let (ip, prefix) = s.split_once('/').ok_or(())?;
        let addr: std::net::IpAddr = ip.parse().map_err(|_| ())?;
        let p: u8 = prefix.parse().map_err(|_| ())?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        if p > max { Err(()) } else { Ok(CidrLike) }
    }
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
            ..Default::default()
        }
    }
    fn c(name: &str, key: &str) -> Client {
        Client {
            name: name.into(),
            key: key.into(),
            ..Default::default()
        }
    }
    fn p(name: &str, url: &str) -> Provider {
        Provider {
            name: name.into(),
            base_url: url.into(),
            key: Some(crate::Secret::new("sk-x")),
            protocol: None,
            ..Default::default()
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
    fn a_key_written_as_a_mapping_says_what_to_write_instead() {
        let y = "version: 1\nclients:\n  - name: c\n    key: tw-k\nproviders:\n  - name: p\n    base_url: https://api.example.com\n    key:\n      whatever: 1\n";
        let e = crate::try_parse(y).unwrap_err().message;
        assert!(e.contains("字符串"), "没说该写成什么：{e}");
    }

    #[test]
    fn an_oauth_typo_lets_serde_say_which_field() {
        // **serde 自己的话比我们能补的任何一句都准**
        let y = "version: 1\nclients:\n  - name: c\n    key: tw-k\nproviders:\n  - name: p\n    base_url: https://api.example.com\n    oauth:\n      refresh: r\n      endpoint: https://a/token\n      refresh_befor: 5m\n";
        let e = crate::try_parse(y).unwrap_err().message;
        assert!(e.contains("refresh_befor"), "{e}");
        assert!(e.contains("refresh_before"), "没提示正确的拼法：{e}");
    }

    #[test]
    fn an_oauth_missing_a_required_field_says_which_one() {
        let y = "version: 1\nclients:\n  - name: c\n    key: tw-k\nproviders:\n  - name: p\n    base_url: https://api.example.com\n    oauth:\n      endpoint: https://a/token\n";
        let e = crate::try_parse(y).unwrap_err().message;
        assert!(e.contains("refresh"), "{e}");
    }

    #[test]
    fn a_credential_problem_names_the_upstream() {
        let mut x = p("r", "https://relay.example");
        x.key = Some(crate::Secret::new("  "));
        let e = validate(&cfg(vec![c("d", "tw-1")], vec![x]))
            .unwrap_err()
            .to_string();
        assert!(e.contains("「r」"), "{e}");
    }

    #[test]
    fn an_upstream_without_any_credential_is_valid() {
        // 本地 Ollama 这类不要密钥。以前要写一个占位值，那是在让配置说谎
        let mut x = p("ollama", "http://127.0.0.1:11434");
        x.key = None;
        assert!(validate(&cfg(vec![c("d", "tw-1")], vec![x])).is_ok());
    }

    #[test]
    fn a_config_with_no_providers_is_valid_because_the_app_must_still_open() {
        // 首次运行生成的就是这种配置。如果它过不了校验，core 起不来，
        // 控制面也起不来，UI 连「你还没配上游」都说不出口。
        assert!(validate(&cfg(vec![c("d", "tw-1")], vec![])).is_ok());
    }

    #[test]
    fn a_config_with_no_clients_is_still_refused() {
        // 没有网关密钥的话任何请求都会被拒 —— 那不是「还没配完」，
        // 是首次运行的生成逻辑出了问题。
        assert!(matches!(
            validate(&cfg(vec![], vec![p("r", "https://x.com")])),
            Err(ValidationError::NoClients)
        ));
    }

    #[test]
    fn a_broken_cidr_in_the_allow_list_is_caught_at_load_time() {
        // 写错一条的后果是它永远不匹配，而表现是「局域网里那台机器连
        // 不上」—— 一条完全看不出原因的故障。
        let mut k = cfg(vec![c("d", "tw-1")], vec![p("r", "https://x.com")]);
        k.listen.gateway.allow_from = vec!["192.168.0.0/99".into()];
        assert!(matches!(validate(&k), Err(ValidationError::BadCidr { .. })));
        k.listen.gateway.allow_from = vec!["192.168.0.0/16".into(), "10.0.0.5".into()];
        assert!(validate(&k).is_ok(), "裸 IP 也该接受");
    }

    #[test]
    fn exposing_the_gateway_defaults_the_allow_list_to_private_ranges() {
        // **不是放行所有**。想放开得手动写 0.0.0.0/0，那时他
        // 至少知道自己做了什么。
        let mut k = cfg(vec![c("d", "tw-1")], vec![p("r", "https://x.com")]);
        assert!(
            k.listen.gateway.effective_allow_from().is_empty(),
            "loopback 下不填"
        );
        k.listen.gateway.bind = crate::Bind::All;
        let eff = k.listen.gateway.effective_allow_from();
        assert!(eff.iter().any(|s| s == "192.168.0.0/16"), "{eff:?}");
        // 用户写了就用他的，不要偷偷加
        k.listen.gateway.allow_from = vec!["10.1.2.0/24".into()];
        assert_eq!(k.listen.gateway.effective_allow_from(), vec!["10.1.2.0/24"]);
    }

    #[test]
    fn an_empty_model_scope_is_refused_and_points_at_disabling_instead() {
        let mut prov = p("relay", "https://relay.example");
        prov.models_only = Some(vec![]);
        let e = validate(&cfg(vec![c("a", "tw-a")], vec![prov])).unwrap_err();
        assert!(matches!(e, ValidationError::EmptyModelsOnly { .. }), "{e}");
        assert!(e.to_string().contains("disabled"), "{e}");
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
    fn names_that_start_with_two_underscores_are_reserved() {
        // 「全部上游」在配置里叫 `__all__`。一个同名的上游或策略组会让
        // `to: __all__` 指向谁取决于实现顺序
        let e = validate(&cfg(
            vec![c("d", "tw-1")],
            vec![p("__all__", "https://relay.example")],
        ))
        .unwrap_err();
        assert!(matches!(e, ValidationError::ReservedName { .. }), "{e}");
        assert!(e.to_string().contains("__all__"), "{e}");
    }

    #[test]
    fn error_messages_say_what_to_do_next() {
        // 错误信息是降低使用难度最有效的杠杆。判据不是「说清
        // 哪里错了」，是「说清接下来做什么」。
        let e = validate(&cfg(vec![c("d", "tw-1")], vec![p("r", "api.example.com")])).unwrap_err();
        assert!(e.to_string().contains("http"), "{e}");
    }
}
