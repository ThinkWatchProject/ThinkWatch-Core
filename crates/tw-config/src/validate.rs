//! 语义校验。**解析成功不等于配置对**，而错误信息要能直接行动。

use tw_types::{Msg, msg};

use crate::{Config, SCHEMA_VERSION};

/// 配置合起来不成立的地方。
///
/// **英文只写一遍**：`Display` 就是 [`ValidationError::msg`] 的原句，界面拿码去翻。
/// `what` 这类参数是一个英文词（`upstream`、`redaction`），界面按它查自己的词表。
#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("{}", self.msg())]
    SchemaTooNew { found: u32, supported: u32 },
    #[error("{}", self.msg())]
    NoClients,
    #[error("{}", self.msg())]
    DuplicateProvider(String),
    #[error("{}", self.msg())]
    DuplicateClient(String),
    #[error("{}", self.msg())]
    DuplicateKey(String, String),
    #[error("{}", self.msg())]
    MissingDefaultKey(String),
    #[error("{}", self.msg())]
    DisabledDefaultKey(String),
    #[error("{}", self.msg())]
    DuplicateClientKey(String, String, String),
    #[error("{}", self.msg())]
    BadBaseUrl { name: String, url: String },
    #[error("{}", self.msg())]
    EmptyKey { name: String },
    #[error("{}", self.msg())]
    ZeroConcurrency { name: String },
    #[error("{}", self.msg())]
    Routing(#[from] tw_engine::RouteError),
    #[error("{}", self.msg())]
    NameCollision(String),
    #[error("{}", self.msg())]
    BadCidr { entry: String },
    #[error("{}", self.msg())]
    Credential {
        name: String,
        source: crate::CredentialError,
    },
    #[error(transparent)]
    Pricing(#[from] tw_pricing::SheetError),
    #[error("{}", self.msg())]
    UnknownPriceSheet { provider: String, sheet: String },
    #[error("{}", self.msg())]
    EmptyModelsOnly { name: String },
    #[error("{}", self.msg())]
    BlankModelsOnly { name: String },
    #[error("{}", self.msg())]
    ReservedName { what: &'static str, name: String },
    #[error("{}", self.msg())]
    EmptyRuleName { what: &'static str },
    #[error("{}", self.msg())]
    DuplicateRuleName { what: &'static str, name: String },
    #[error("{}", self.msg())]
    EmptyRulePattern { what: &'static str, name: String },
    /// `detail` 是正则库的原话
    #[error("{}", self.msg())]
    BadRulePattern {
        what: &'static str,
        name: String,
        detail: String,
    },
    #[error("{}", self.msg())]
    UnknownRule { guard: &'static str, id: String },
    #[error("{}", self.msg())]
    OutputLimitRange { max: usize, ceiling: usize },
    #[error("{}", self.msg())]
    ControlKeyMissing,
    #[error("{}", self.msg())]
    ControlKeyInvalid,
    #[error("{}", self.msg())]
    RemotePortZero,
    #[error("{}", self.msg())]
    RemotePortIsGateway { port: u16 },
    #[error("{}", self.msg())]
    BadRemoteCidr { entry: String },
}

impl ValidationError {
    /// 给人看的那句话，带码。
    pub fn msg(&self) -> Msg {
        use ValidationError::*;
        match self {
            SchemaTooNew { found, supported } => msg!(
                "config.schema_too_new", found = found, supported = supported =>
                "the configuration is schema version {found}, and this twcore supports up to \
                 {supported}. Upgrade the app, or put the configuration back in the older form"
            ),
            NoClients => msg!(
                "config.no_clients" =>
                "the configuration has no gateway key under `clients`, so every request is \
                 refused. One is generated on first start"
            ),
            DuplicateProvider(name) => msg!(
                "config.duplicate_upstream", upstream = name =>
                "the upstream name {upstream} appears twice. Routing rules refer to an upstream by \
                 name, so names have to be unique"
            ),
            DuplicateClient(name) => msg!(
                "config.duplicate_key_name", key = name =>
                "the gateway key name {key} appears twice"
            ),
            DuplicateKey(a, b) => msg!(
                "config.duplicate_key_value", key = a, other = b =>
                "gateway keys `{key}` and `{other}` have the same value. The gateway tells clients \
                 apart by key, so the values have to be unique"
            ),
            MissingDefaultKey(key) => msg!(
                "config.default_key_missing", key = key =>
                "default_key points at gateway key `{key}`, which does not exist"
            ),
            DisabledDefaultKey(key) => msg!(
                "config.default_key_disabled", key = key =>
                "the default gateway key `{key}` is disabled. Every client that has not been \
                 pointed at the gateway explicitly uses it, and disabling it breaks all of them"
            ),
            DuplicateClientKey(a, b, client) => msg!(
                "config.duplicate_client_key", key = a, other = b, client = client =>
                "gateway keys `{key}` and `{other}` both say they were made for client `{client}`. \
                 A client has exactly one"
            ),
            BadBaseUrl { name, url } => msg!(
                "config.bad_base_url", upstream = name, url = url =>
                "the endpoint of upstream `{upstream}` is neither http nor https: {url}"
            ),
            EmptyKey { name } => msg!(
                "config.empty_key", key = name =>
                "the value of gateway key `{key}` is empty"
            ),
            ZeroConcurrency { name } => msg!(
                "config.zero_concurrency", key = name =>
                "gateway key `{key}` has max_concurrent: 0, so every request made with it would \
                 wait forever. Leave max_concurrent out for no limit"
            ),
            // 路由那几句本身就说清了是哪条规则、哪个组，前面不用再垫一句
            Routing(e) => e.msg(),
            NameCollision(name) => msg!(
                "config.name_collision", name = name =>
                "`{name}` is the name of both an upstream and a group, so a rule's `to` cannot say \
                 which one it means. Rename one of them"
            ),
            BadCidr { entry } => msg!(
                "config.bad_allow_from", entry = entry =>
                "`{entry}` in listen.gateway.allow_from is wrong: not a valid IP address or CIDR; \
                 it is written as 192.168.0.0/16"
            ),
            // **码还是凭据那一条的码，多带一个 `upstream`。**同一句话在编辑上游的
            // 对话框里不用说是哪个上游（就是正在改的那个），在整份配置的校验里
            // 必须说 —— 为此给每一条再造一个「带上游名」的码，码表就翻了一倍
            Credential { name, source } => {
                let mut m = source.msg();
                m.text = format!("the credential of upstream `{name}`: {}", m.text);
                m.args.insert("upstream".into(), name.clone());
                m
            }
            Pricing(e) => e.msg(),
            UnknownPriceSheet { provider, sheet } => msg!(
                "config.unknown_price_sheet", upstream = provider, sheet = sheet =>
                "upstream `{upstream}` uses price sheet `{sheet}`, which does not exist"
            ),
            EmptyModelsOnly { name } => msg!(
                "config.empty_models_only", upstream = name =>
                "the scope of upstream `{upstream}` (models_only) is empty, so it offers no model \
                 at all. To pause the upstream, disable it instead (disabled: true)"
            ),
            BlankModelsOnly { name } => msg!(
                "config.blank_models_only", upstream = name =>
                "the scope of upstream `{upstream}` (models_only) has an empty entry"
            ),
            ReservedName { what, name } => msg!(
                "config.reserved_name", what = what, name = name =>
                "the {what} name `{name}` starts with __, which is reserved for built-ins. Use a \
                 different name"
            ),
            EmptyRuleName { what } => msg!(
                "config.rule_name_empty", what = what =>
                "a custom {what} rule has no name"
            ),
            DuplicateRuleName { what, name } => msg!(
                "config.rule_name_taken", what = what, name = name =>
                "the custom {what} rule name `{name}` appears twice"
            ),
            EmptyRulePattern { what, name } => msg!(
                "config.rule_pattern_empty", what = what, name = name =>
                "the pattern of custom {what} rule `{name}` is empty"
            ),
            BadRulePattern { what, name, detail } => msg!(
                "config.rule_pattern_bad", what = what, name = name, detail = detail =>
                "the pattern of custom {what} rule `{name}` is not a valid regular expression: \
                 {detail}"
            ),
            UnknownRule { guard, id } => msg!(
                "config.unknown_rule", guard = guard, rule = id =>
                "security.{guard} names `{rule}`, which is not a built-in rule"
            ),
            OutputLimitRange { max, ceiling } => msg!(
                "config.output_limit_range", max = max, ceiling = ceiling =>
                "security.output_limit.max_chars is {max}; it has to be between 1 and {ceiling}"
            ),
            ControlKeyMissing => msg!(
                "config.control_key_missing" =>
                "the configuration has no listen.control.key, the key the desktop app connects \
                 with. twcore serve writes one when it starts; twcore control-key --rotate \
                 writes a new one"
            ),
            ControlKeyInvalid => msg!(
                "config.control_key_invalid" =>
                "listen.control.key has to be 64 hexadecimal characters. twcore control-key \
                 --rotate writes a new one"
            ),
            RemotePortZero => msg!(
                "config.remote_port_zero" =>
                "listen.control.remote.port is 0; it has to be between 1 and 65535. twcore remote \
                 enable picks a free one"
            ),
            RemotePortIsGateway { port } => msg!(
                "config.remote_port_is_gateway", port = port =>
                "listen.control.remote.port is {port}, the same as the gateway's port. The two \
                 need different ports"
            ),
            BadRemoteCidr { entry } => msg!(
                "config.bad_remote_allow_from", entry = entry =>
                "`{entry}` in listen.control.remote.allow_from is wrong: not a valid IP address or \
                 CIDR; it is written as 192.168.0.0/16"
            ),
        }
    }
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
    let mut for_client: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for c in &cfg.clients {
        if !names.insert(&c.name) {
            return Err(ValidationError::DuplicateClient(c.name.clone()));
        }
        if c.key.trim().is_empty() {
            return Err(ValidationError::EmptyKey {
                name: c.name.clone(),
            });
        }
        // 超出上限的请求不拒绝、也不设等待时限（见 tw-gateway 的 limits），
        // 上限是 0 的话，这把密钥的每个请求都会一直等下去
        if c.max_concurrent == Some(0) {
            return Err(ValidationError::ZeroConcurrency {
                name: c.name.clone(),
            });
        }
        if let Some(prev) = keys.insert(&c.key, &c.name) {
            return Err(ValidationError::DuplicateKey(
                prev.to_string(),
                c.name.clone(),
            ));
        }
        // 一个客户端两把钥匙，接管时就得猜用哪把 —— 而猜错的后果要等到
        // 那个客户端下一次发请求才看得见
        if let Some(owner) = c.client.as_deref().map(str::trim).filter(|x| !x.is_empty())
            && let Some(prev) = for_client.insert(owner, &c.name)
        {
            return Err(ValidationError::DuplicateClientKey(
                prev.to_string(),
                c.name.clone(),
                owner.to_string(),
            ));
        }
    }
    // 默认密钥是一个身份，不是「列表里的第一把」：指到一把不存在的钥匙上，
    // 接管和手动配置会各自落到不同的地方
    if let Some(name) = cfg
        .default_key
        .as_deref()
        .map(str::trim)
        .filter(|x| !x.is_empty())
    {
        match cfg.clients.iter().find(|c| c.name == name) {
            None => return Err(ValidationError::MissingDefaultKey(name.to_string())),
            Some(c) if c.disabled => {
                return Err(ValidationError::DisabledDefaultKey(name.to_string()));
            }
            Some(_) => {}
        }
    }
    // `__` 开头的名字留给内置项（「全部上游」在配置里叫 `__all__`）。**撞上了
    // 不是报一个重名那么简单**：规则写 `to: __all__` 时指的是谁，就取决于
    // 实现顺序了
    let reserved = |n: &str| n.starts_with(tw_engine::RESERVED_PREFIX);
    let named = cfg
        .providers
        .iter()
        .map(|p| ("upstream", &p.name))
        .chain(cfg.groups.iter().map(|g| ("group", &g.name)))
        .chain(cfg.routes.iter().map(|r| ("route", &r.name)));
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

    // 自定义规则。**写坏的正则在这里就拒绝**，而不是加载之后跳过那一条：
    // 一条静默失效的安全规则比没有更糟，因为用户以为它在
    check_rules(
        "redaction",
        cfg.security
            .redact
            .custom
            .iter()
            .map(|c| (c.name.as_str(), c.pattern.as_str())),
    )?;
    check_rules(
        "tool-call",
        cfg.security
            .inspect_tools
            .custom
            .iter()
            .map(|c| (c.name.as_str(), c.pattern.as_str())),
    )?;
    check_content_rules(&cfg.security.content.custom)?;
    let max = cfg.security.output_limit.max_chars;
    if max == 0 || max > crate::MAX_CHARS_CEILING {
        return Err(ValidationError::OutputLimitRange {
            max,
            ceiling: crate::MAX_CHARS_CEILING,
        });
    }
    // 按 id 开关、改处置的内置规则得真的存在。**写错一个 id 和写错一个字段名
    // 是同一种错**：跳过它，用户停用的那条会照样在报
    if let Some((guard, id)) = cfg.security.unknown_rule() {
        return Err(ValidationError::UnknownRule {
            guard,
            id: id.to_string(),
        });
    }
    // 控制面的钥匙。**缺了、短了、不是十六进制，整份配置都不收**，旧的继续
    // 服务：一份没有钥匙的配置换进来，下一条连接谁都进不来 —— 包括要把它
    // 改回去的那个界面；一把好猜的短钥匙和没有差不多
    match cfg.listen.control.key.as_deref() {
        None => return Err(ValidationError::ControlKeyMissing),
        Some(k) if tw_api::control::ControlKey::parse(k).is_err() => {
            return Err(ValidationError::ControlKeyInvalid);
        }
        Some(_) => {}
    }
    // 远程控制端口。**没开也照样查**：开关一拨就生效，写错的地方要在写下去
    // 的那一刻说，不是等到有人打开它的时候
    if let Some(r) = &cfg.listen.control.remote {
        if r.port == 0 {
            return Err(ValidationError::RemotePortZero);
        }
        if r.port == cfg.listen.gateway.port {
            return Err(ValidationError::RemotePortIsGateway { port: r.port });
        }
        for entry in &r.allow_from {
            if entry.parse::<std::net::IpAddr>().is_err() && entry.parse::<CidrLike>().is_err() {
                return Err(ValidationError::BadRemoteCidr {
                    entry: entry.clone(),
                });
            }
        }
    }
    Ok(())
}

/// 一组自定义规则：名字不空、不重复，正则编得过。
///
/// 上限和数据面编译时一样（`tw_guard::redact::rules::compile`）：一条要在每个请求上
/// 跑的正则，编出来的东西不能太大。
fn check_rules<'a>(
    what: &'static str,
    rules: impl Iterator<Item = (&'a str, &'a str)>,
) -> Result<(), ValidationError> {
    let mut seen = std::collections::HashSet::new();
    for (name, pattern) in rules {
        if name.trim().is_empty() {
            return Err(ValidationError::EmptyRuleName { what });
        }
        if !seen.insert(name) {
            return Err(ValidationError::DuplicateRuleName {
                what,
                name: name.to_string(),
            });
        }
        if pattern.is_empty() {
            return Err(ValidationError::EmptyRulePattern {
                what,
                name: name.to_string(),
            });
        }
        regex::RegexBuilder::new(pattern)
            .size_limit(1 << 20)
            .build()
            .map_err(|e| ValidationError::BadRulePattern {
                what,
                name: name.to_string(),
                detail: e.to_string(),
            })?;
    }
    Ok(())
}

/// 自定义的内容规则：名字不空、不重复，写着正则的编得过。**编法和数据面同一个**
/// （`tw_guard::content::Rule::new`：不分大小写、编译后的大小有上限）。
fn check_content_rules(rules: &[crate::CustomContentRule]) -> Result<(), ValidationError> {
    const WHAT: &str = "content";
    let mut seen = std::collections::HashSet::new();
    for c in rules {
        let name = c.name.as_str();
        if name.trim().is_empty() {
            return Err(ValidationError::EmptyRuleName { what: WHAT });
        }
        if !seen.insert(name) {
            return Err(ValidationError::DuplicateRuleName {
                what: WHAT,
                name: name.to_string(),
            });
        }
        if c.pattern.trim().is_empty() {
            return Err(ValidationError::EmptyRulePattern {
                what: WHAT,
                name: name.to_string(),
            });
        }
        tw_guard::content::Rule::new(tw_guard::content::RuleInput {
            id: name,
            name,
            custom: true,
            pattern: &c.pattern,
            matching: c.matching.engine(),
            action: tw_guard::content::Action::Warn,
        })
        .map_err(|e| ValidationError::BadRulePattern {
            what: WHAT,
            name: name.to_string(),
            detail: e.detail,
        })?;
    }
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
            listen: Listen {
                control: crate::ControlListen {
                    key: Some("c0ffee00".repeat(8)),
                    remote: None,
                },
                ..Default::default()
            },
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

    /// 控制面的钥匙：缺了、短了、不是十六进制，都不收。
    #[test]
    fn the_control_key_has_to_be_there_and_be_64_hex_characters() {
        let with = |k: Option<&str>| {
            let mut c = cfg(vec![c("d", "tw-1")], vec![]);
            c.listen.control.key = k.map(str::to_string);
            validate(&c)
        };
        assert!(with(Some(&"ab".repeat(32))).is_ok());
        assert!(with(Some(&"AB".repeat(32))).is_ok(), "大写也是十六进制");
        assert!(matches!(
            with(None),
            Err(ValidationError::ControlKeyMissing)
        ));
        for bad in [
            "",
            "abc",
            &"ab".repeat(31),
            &"zz".repeat(32),
            tw_api::control::KEY_MASK,
        ] {
            assert!(
                matches!(with(Some(bad)), Err(ValidationError::ControlKeyInvalid)),
                "{bad}"
            );
        }
        let m = ValidationError::ControlKeyMissing.msg();
        assert_eq!(m.code, "config.control_key_missing");
        assert!(m.text.contains("twcore control-key --rotate"), "{m:?}");
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
        let e = crate::try_parse(y).unwrap_err().message.text;
        assert!(e.contains("a string"), "没说该写成什么：{e}");
    }

    #[test]
    fn an_oauth_typo_lets_serde_say_which_field() {
        // **serde 自己的话比我们能补的任何一句都准**
        let y = "version: 1\nclients:\n  - name: c\n    key: tw-k\nproviders:\n  - name: p\n    base_url: https://api.example.com\n    oauth:\n      refresh: r\n      endpoint: https://a/token\n      refresh_befor: 5m\n";
        let e = crate::try_parse(y).unwrap_err().message.text;
        assert!(e.contains("refresh_befor"), "{e}");
        assert!(e.contains("refresh_before"), "没提示正确的拼法：{e}");
    }

    #[test]
    fn an_oauth_missing_a_required_field_says_which_one() {
        let y = "version: 1\nclients:\n  - name: c\n    key: tw-k\nproviders:\n  - name: p\n    base_url: https://api.example.com\n    oauth:\n      endpoint: https://a/token\n";
        let e = crate::try_parse(y).unwrap_err().message.text;
        assert!(e.contains("refresh"), "{e}");
    }

    #[test]
    fn a_credential_problem_names_the_upstream() {
        let mut x = p("r", "https://relay.example");
        x.key = Some(crate::Secret::new("  "));
        let e = validate(&cfg(vec![c("d", "tw-1")], vec![x]))
            .unwrap_err()
            .to_string();
        assert!(e.contains("`r`"), "{e}");
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
    fn a_zero_concurrency_limit_is_refused_rather_than_waiting_forever() {
        let mut k = cfg(vec![c("d", "tw-1")], vec![p("r", "https://x.com")]);
        k.clients[0].max_concurrent = Some(0);
        assert!(matches!(
            validate(&k),
            Err(ValidationError::ZeroConcurrency { .. })
        ));
        k.clients[0].max_concurrent = Some(1);
        assert!(validate(&k).is_ok());
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

    fn with_rules(redact: &[(&str, &str)], tools: &[(&str, &str)]) -> Config {
        let mut x = cfg(vec![c("default", "tw-a")], vec![]);
        x.security.redact.custom = redact
            .iter()
            .map(|(n, pat)| crate::CustomRedactRule {
                name: n.to_string(),
                pattern: pat.to_string(),
                disabled: false,
            })
            .collect();
        x.security.inspect_tools.custom = tools
            .iter()
            .map(|(n, pat)| crate::CustomToolRule {
                name: n.to_string(),
                pattern: pat.to_string(),
                action: crate::ToolAction::Cut,
                disabled: false,
            })
            .collect();
        x
    }

    #[test]
    fn custom_rules_that_compile_are_accepted() {
        let x = with_rules(
            &[("公司令牌", r"corp_[A-Za-z0-9]{32}")],
            &[("删除集群资源", r"kubectl\s+delete")],
        );
        assert!(validate(&x).is_ok(), "{:?}", validate(&x));
    }

    #[test]
    fn a_broken_pattern_is_refused_and_named() {
        // 一条静默失效的安全规则比没有更糟：用户以为它在
        let e = validate(&with_rules(&[("写坏了", "(")], &[])).unwrap_err();
        let m = e.to_string();
        assert!(m.contains("写坏了"), "{m}");
        assert!(matches!(e, ValidationError::BadRulePattern { .. }), "{e:?}");
        assert!(validate(&with_rules(&[], &[("空的", "")])).is_err());
    }

    #[test]
    fn a_rule_name_is_required_and_unique_within_its_line_of_defence() {
        assert!(matches!(
            validate(&with_rules(&[(" ", "x")], &[])),
            Err(ValidationError::EmptyRuleName { .. })
        ));
        assert!(matches!(
            validate(&with_rules(&[("同名", "a"), ("同名", "b")], &[])),
            Err(ValidationError::DuplicateRuleName { .. })
        ));
        // 两项防护各管各的名字
        assert!(validate(&with_rules(&[("同名", "a")], &[("同名", "b")])).is_ok());
    }

    #[test]
    fn content_rules_are_checked_like_the_others_and_a_keyword_is_not_a_regex() {
        let mut x = with_rules(&[], &[]);
        let rule = |name: &str, pattern: &str, matching| crate::CustomContentRule {
            name: name.into(),
            pattern: pattern.into(),
            matching,
            action: crate::ContentAction::Block,
            disabled: false,
        };
        // 子串里的括号不是正则
        x.security.content.custom = vec![rule("括号", "f(", crate::ContentMatch::Contains)];
        assert!(validate(&x).is_ok(), "{:?}", validate(&x));
        x.security.content.custom = vec![rule("括号", "f(", crate::ContentMatch::Regex)];
        assert!(matches!(
            validate(&x),
            Err(ValidationError::BadRulePattern {
                what: "content",
                ..
            })
        ));
        x.security.content.custom = vec![
            rule("同名", "a", crate::ContentMatch::Contains),
            rule("同名", "b", crate::ContentMatch::Contains),
        ];
        assert!(matches!(
            validate(&x),
            Err(ValidationError::DuplicateRuleName { .. })
        ));
    }

    #[test]
    fn the_output_limit_has_to_be_a_sensible_number() {
        let mut x = with_rules(&[], &[]);
        for bad in [0, crate::MAX_CHARS_CEILING + 1] {
            x.security.output_limit.max_chars = bad;
            assert!(
                matches!(validate(&x), Err(ValidationError::OutputLimitRange { .. })),
                "{bad}"
            );
        }
        x.security.output_limit.max_chars = 1;
        assert!(validate(&x).is_ok());
    }

    /// 按 id 写到的内置规则得真的存在：写错的 id 和写错的字段名一样拒绝，
    /// 说出是哪一项下的哪个 id。
    #[test]
    fn a_builtin_rule_id_that_does_not_exist_is_refused_and_named() {
        let mut x = with_rules(&[], &[]);
        x.security.redact.disable = vec!["jwt".into()];
        x.security.inspect_tools.actions = [("rm-rf-root".to_string(), crate::ToolAction::Record)]
            .into_iter()
            .collect();
        assert!(validate(&x).is_ok(), "{:?}", validate(&x));

        let mut typo = x.clone();
        typo.security.redact.enable = vec!["jwtt".into()];
        let e = validate(&typo).unwrap_err();
        assert!(
            matches!(
                e,
                ValidationError::UnknownRule {
                    guard: "redact",
                    ..
                }
            ),
            "{e:?}"
        );
        assert!(e.to_string().contains("jwtt"), "{e}");

        for tools in [
            crate::ToolPolicy {
                disable: vec!["rm-rf-rooot".into()],
                ..Default::default()
            },
            crate::ToolPolicy {
                actions: [("没有这条".to_string(), crate::ToolAction::Cut)]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        ] {
            let mut bad = x.clone();
            bad.security.inspect_tools = tools;
            assert!(
                matches!(
                    validate(&bad),
                    Err(ValidationError::UnknownRule {
                        guard: "inspect_tools",
                        ..
                    })
                ),
                "{:?}",
                validate(&bad)
            );
        }
    }
}

#[cfg(test)]
mod msg_codes {
    use super::*;
    use crate::CredentialError;
    use crate::edit::EditError;
    use crate::store::StoreError;

    /// 码非空、带层名、英文就是 `Display`、同一个枚举里不重复。
    fn check(prefix: &str, all: &[(Msg, String)]) {
        let mut seen = std::collections::HashSet::new();
        for (m, display) in all {
            assert!(m.code.starts_with(prefix), "{m:?}");
            assert!(!m.text.is_empty(), "{m:?}");
            assert_eq!(&m.text, display, "{m:?}");
            assert!(seen.insert(m.code.clone()), "码重复了：{}", m.code);
        }
    }

    #[test]
    fn every_credential_error_has_its_own_code() {
        use CredentialError::*;
        let all = [
            EmptyKey,
            KeyAndOauth,
            EmptyOauth,
            ClaudeSubscription,
            GoogleSubscription,
            ChatgptWithoutLogin,
            IdentityHeader("h".into()),
            TooManyHeaders,
            BadHeaderName("h".into()),
            ReservedHeader("h".into()),
            DuplicateHeader("h".into()),
            BadHeaderValue("h".into()),
            UnknownPlaceholder {
                name: "h".into(),
                placeholder: "{{x}}".into(),
            },
            TokenWithoutOauth("h".into()),
            KeyAndAuthHeader("h".into()),
            OauthAndAuthHeader("h".into()),
            NoToken,
        ];
        check(
            "config.credential.",
            &all.iter()
                .map(|e| (e.msg(), e.to_string()))
                .collect::<Vec<_>>(),
        );
        // `${ENV}` 展开不了的那几种自己有码，凭据里的 `Env` 原样用它们
        let env = [
            crate::SecretResolveError::MissingEnv("X".into()),
            crate::SecretResolveError::Unterminated { pos: 3 },
            crate::SecretResolveError::EmptyName,
        ];
        check(
            "config.secret.",
            &env.iter()
                .map(|e| (CredentialError::Env(e.clone()).msg(), e.to_string()))
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn every_store_and_edit_error_has_its_own_code() {
        let path = std::path::PathBuf::from("/x/config.yaml");
        let store = [
            StoreError::Io {
                path: path.clone(),
                source: std::io::Error::other("denied"),
            },
            StoreError::Missing { path },
            StoreError::Conflict {
                expected: "a".into(),
                current: "b".into(),
            },
        ];
        check(
            "config.store.",
            &store
                .iter()
                .map(|e| (e.msg(), e.to_string()))
                .collect::<Vec<_>>(),
        );
        let edit = [
            EditError::NameTaken {
                what: "proxy",
                name: "hk".into(),
            },
            EditError::NotFound {
                what: "proxy",
                name: "hk".into(),
            },
            EditError::Parse("x".into()),
            EditError::Multiline,
            EditError::Nameless { what: "proxy" },
            EditError::Unwritable("x".into()),
            EditError::SelfCheck("x".into()),
        ];
        check(
            "config.edit.",
            &edit
                .iter()
                .map(|e| (e.msg(), e.to_string()))
                .collect::<Vec<_>>(),
        );
        // 包着的那一层直接用它自己的码
        let e = EditError::Yaml(tw_yaml::PatchError::NotScalar("a".into()));
        assert_eq!(e.msg().code, "yaml.not_scalar");
    }

    #[test]
    fn every_validation_error_has_its_own_code() {
        use ValidationError::*;
        let all = [
            SchemaTooNew {
                found: 9,
                supported: 1,
            },
            NoClients,
            DuplicateProvider("a".into()),
            DuplicateClient("k".into()),
            DuplicateKey("k".into(), "j".into()),
            MissingDefaultKey("k".into()),
            DisabledDefaultKey("k".into()),
            DuplicateClientKey("k".into(), "j".into(), "codex".into()),
            BadBaseUrl {
                name: "a".into(),
                url: "ftp://x".into(),
            },
            EmptyKey { name: "k".into() },
            ZeroConcurrency { name: "k".into() },
            NameCollision("a".into()),
            BadCidr { entry: "x".into() },
            UnknownPriceSheet {
                provider: "a".into(),
                sheet: "s".into(),
            },
            EmptyModelsOnly { name: "a".into() },
            BlankModelsOnly { name: "a".into() },
            ReservedName {
                what: "upstream",
                name: "__a".into(),
            },
            EmptyRuleName { what: "redaction" },
            DuplicateRuleName {
                what: "redaction",
                name: "r".into(),
            },
            EmptyRulePattern {
                what: "redaction",
                name: "r".into(),
            },
            BadRulePattern {
                what: "redaction",
                name: "r".into(),
                detail: "unclosed group".into(),
            },
            UnknownRule {
                guard: "redact",
                id: "x".into(),
            },
            OutputLimitRange { max: 0, ceiling: 1 },
        ];
        check(
            "config.",
            &all.iter()
                .map(|e| (e.msg(), e.to_string()))
                .collect::<Vec<_>>(),
        );
        // 包着的那几层：码是里面那一句的，不是一个套话
        assert_eq!(
            Routing(tw_engine::RouteError::EmptyGroup("g".into()))
                .msg()
                .code,
            "engine.empty_group"
        );
        assert_eq!(
            Pricing(tw_pricing::SheetError::EmptyName).msg().code,
            "pricing.sheet.empty_name"
        );
        // 凭据那一条多带一个上游名，英文前面也补上
        let e = Credential {
            name: "官方".into(),
            source: CredentialError::EmptyKey,
        };
        let m = e.msg();
        assert_eq!(m.code, "config.credential.empty_key");
        assert_eq!(m.arg("upstream"), "官方");
        assert_eq!(m.text, e.to_string());
        assert!(m.text.contains("官方"), "{m:?}");
    }

    #[test]
    fn a_rejected_config_says_which_stage_and_line_around_serdes_words() {
        // 语法错：serde 的原话翻不了，外面那一层（哪一关、第几行）带码
        let r =
            crate::try_parse("version: 1\nclients:\n  - name: \"c\n    key: tw-k\n").unwrap_err();
        let m = r.msg();
        assert_eq!(m.code, "config.rejected_at", "{m:?}");
        assert_eq!(m.arg("stage"), "syntax");
        assert!(
            !m.arg("line").is_empty() && !m.arg("detail").is_empty(),
            "{m:?}"
        );
        // 语义错：直接就是那一句
        let r = crate::try_parse("version: 1\n").unwrap_err();
        assert_eq!(r.msg().code, "config.no_clients");
    }
}
