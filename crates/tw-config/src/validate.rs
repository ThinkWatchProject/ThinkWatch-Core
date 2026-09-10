//! 语义校验。**解析成功不等于配置对**，而错误信息要能直接行动。

use crate::{Config, SCHEMA_VERSION};

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error(
        "配置 schema 版本是 {found}，这个版本的 twcore 只认到 {supported}。\n请升级应用，或把配置改回旧格式。"
    )]
    SchemaTooNew { found: u32, supported: u32 },
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
    #[error("路由配置有问题：{0}")]
    Routing(#[from] tw_engine::RouteError),
    #[error("`{0}` 既是 provider 名又是组名。规则里的 `to` 会指向哪个是不确定的，改掉其中一个。")]
    NameCollision(String),
    #[error("listen.gateway.allow_from 里的 `{entry}` 写错了：{reason}")]
    BadCidr { entry: String, reason: String },
    /// **配置文件不该能执行程序。**§3.1 把「配置被同步、被分享、被 AI
    /// 改」当成目标场景，那时抄一份配置就等于跑一段代码 —— 而用户对一个
    /// 网关配置文件的心理预期是「里面是设置」。
    #[error(
        "provider `{name}` 用的是 `exec` 凭据，而它已经不支持了：配置文件不该能执行程序。\n把密钥直接写在 key: 里，或者写成 ${{VAR}} 从环境变量取。"
    )]
    ExecRemoved { name: String },
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
    // **零个 provider 是合法的**，这是实现时改的一个设计：
    //
    // 首次运行的第一步是生成一份还没有上游的配置（§7.6），如果那样的
    // 配置过不了校验，core 就起不来 —— 而 core 起不来意味着控制面也
    // 起不来，UI 连「你还没配上游」都说不出口，只能显示一个启动失败。
    //
    // 正确的分工是：**配置合法 ≠ 能转发**。零 provider 的配置能加载、
    // 控制面能起来、引导流程能跑；数据面在收到请求时给一条说清楚下一
    // 步的错误。这和 §0.6「配一个 API 就能用」是同一条线 —— 那句话的
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
        // **`exec` 在这一关拦下来，而不是等到请求时。**它已经删掉了
        // （配置文件不该能执行程序，见 Secret 的文档），而一份带
        // `exec` 的老配置必须在这里就说清楚 —— 让它加载成功、再让
        // 每个请求各自失败，是最难查的那种坏法。
        if matches!(p.key, crate::Secret::Exec { .. }) {
            return Err(ValidationError::ExecRemoved {
                name: p.name.clone(),
            });
        }
        if p.key.is_blank() {
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
                reason: "不是合法的 IP 或 CIDR，写法是 `192.168.0.0/16`".to_string(),
            });
        }
    }

    // 路由规则的目标、比较式写法，都在这里查。**一条永远不命中、或者
    // 指向不存在的 provider 的规则，在运行时是完全静默的**（§7.11 的
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
            key: crate::Secret::Literal("sk-x".into()),
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
        // **不是放行所有**（§5.4）。想放开得手动写 0.0.0.0/0，那时他
        // 至少知道自己做了什么。
        let mut k = cfg(vec![c("d", "tw-1")], vec![p("r", "https://x.com")]);
        assert!(
            k.listen.gateway.effective_allow_from().is_empty(),
            "loopback 下不填"
        );
        k.listen.gateway.bind = crate::Bind::Lan;
        let eff = k.listen.gateway.effective_allow_from();
        assert!(eff.iter().any(|s| s == "192.168.0.0/16"), "{eff:?}");
        // 用户写了就用他的，不要偷偷加
        k.listen.gateway.allow_from = vec!["10.1.2.0/24".into()];
        assert_eq!(k.listen.gateway.effective_allow_from(), vec!["10.1.2.0/24"]);
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
        let e = validate(&cfg(vec![c("d", "tw-1")], vec![p("r", "api.example.com")])).unwrap_err();
        assert!(e.to_string().contains("http"), "{e}");
    }
}
