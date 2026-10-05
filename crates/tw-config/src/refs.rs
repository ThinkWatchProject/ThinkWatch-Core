//! 谁引用了谁：删之前要知道，改名时要跟着改。
//!
//! 配置里的名字就是引用。一个上游改了名、而路由规则还写着旧名字，那份
//! 配置会被校验拒掉 —— 用户看到的是「改个名字都保存不了」；绕过校验的
//! 话更糟，规则会静默地不再命中。所以**改名是一次原子写入**：那一项和
//! 所有引用它的地方在同一个版本里一起变。

use serde_yaml_ng::Value;
use tw_engine::Target;
use tw_engine::rule::OneOrMany;
use tw_yaml::Step;

use crate::Config;
use crate::edit::{self, EditError};

/// 引用了某个上游的一处。
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderRef {
    /// 路由规则的去向（`to`）
    RuleTarget { route: String, rule: String },
    /// 路由规则的条件（`when.provider_would_be`）
    RuleCondition { route: String, rule: String },
    /// 策略组的成员，或者 `select` 组当前选中的那一个
    Group { group: String },
}

/// 引用了 `name` 这个上游的所有地方，按配置里出现的顺序。
pub fn provider_refs(cfg: &Config, name: &str) -> Vec<ProviderRef> {
    let mut out = Vec::new();
    for route in &cfg.routes {
        for rule in &route.rules {
            // 转发给它，或者指定了它的模型
            if rule.to.as_ref().is_some_and(|t| t.mentions_provider(name)) {
                out.push(ProviderRef::RuleTarget {
                    route: route.name.clone(),
                    rule: rule.name.clone(),
                });
            }
            if rule
                .when
                .provider_would_be
                .as_ref()
                .is_some_and(|p| p.contains(name))
            {
                out.push(ProviderRef::RuleCondition {
                    route: route.name.clone(),
                    rule: rule.name.clone(),
                });
            }
        }
    }
    for g in &cfg.groups {
        if g.providers.iter().any(|p| p == name) || g.selected.as_deref() == Some(name) {
            out.push(ProviderRef::Group {
                group: g.name.clone(),
            });
        }
    }
    out
}

/// 用着 `name` 这个代理的上游。
pub fn proxy_users(cfg: &Config, name: &str) -> Vec<String> {
    cfg.providers
        .iter()
        .filter(|p| p.proxy == name)
        .map(|p| p.name.clone())
        .collect()
}

/// 选了 `name` 这张价目表的上游。
pub fn sheet_users(cfg: &Config, name: &str) -> Vec<String> {
    cfg.providers
        .iter()
        .filter(|p| p.pricing.as_deref() == Some(name))
        .map(|p| p.name.clone())
        .collect()
}

/// 把 `text` 里选了价目表 `old` 的上游都改成选 `new`。
pub fn rename_sheet(text: &str, cfg: &Config, old: &str, new: &str) -> Result<String, EditError> {
    let mut out = text.to_string();
    for (i, p) in cfg.providers.iter().enumerate() {
        if p.pricing.as_deref() == Some(old) {
            out = edit::set(
                &out,
                &[Step::key("providers"), Step::Index(i), Step::key("pricing")],
                Some(&Value::String(new.to_string())),
            )?;
        }
    }
    Ok(out)
}

/// 把 `text` 里引用上游 `old` 的地方都改成 `new`。`cfg` 是 `text` 解析
/// 出来的那一份 —— 下标要对得上。
pub fn rename_provider(
    text: &str,
    cfg: &Config,
    old: &str,
    new: &str,
) -> Result<String, EditError> {
    let mut out = text.to_string();
    let s = |v: &str| Value::String(v.to_string());
    for (r, route) in cfg.routes.iter().enumerate() {
        for (i, rule) in route.rules.iter().enumerate() {
            let base = [
                Step::key("routes"),
                Step::Index(r),
                Step::key("rules"),
                Step::Index(i),
            ];
            match &rule.to {
                Some(Target::Name(to)) if to == old => {
                    let mut p = base.to_vec();
                    p.push(Step::key("to"));
                    out = edit::set(&out, &p, Some(&s(new)))?;
                }
                // 指定模型：只改那一项的 `provider`，模型名和别的项原样不动
                Some(Target::Models(pinned)) => {
                    for (k, _) in pinned.iter().enumerate().filter(|(_, x)| x.provider == old) {
                        let mut p = base.to_vec();
                        p.extend([Step::key("to"), Step::Index(k), Step::key("provider")]);
                        out = edit::set(&out, &p, Some(&s(new)))?;
                    }
                }
                _ => {}
            }
            if let Some(pw) = &rule.when.provider_would_be
                && pw.contains(old)
            {
                let value = match pw {
                    OneOrMany::One(_) => s(new),
                    OneOrMany::Many(xs) => Value::Sequence(
                        xs.iter()
                            .map(|x| s(if x == old { new } else { x }))
                            .collect(),
                    ),
                };
                let mut p = base.to_vec();
                p.extend([Step::key("when"), Step::key("provider_would_be")]);
                out = edit::set(&out, &p, Some(&value))?;
            }
        }
    }
    for (g, group) in cfg.groups.iter().enumerate() {
        let base = [Step::key("groups"), Step::Index(g)];
        if group.providers.iter().any(|p| p == old) {
            let value = Value::Sequence(
                group
                    .providers
                    .iter()
                    .map(|x| s(if x == old { new } else { x }))
                    .collect(),
            );
            let mut p = base.to_vec();
            p.push(Step::key("providers"));
            out = edit::set(&out, &p, Some(&value))?;
        }
        if group.selected.as_deref() == Some(old) {
            let mut p = base.to_vec();
            p.push(Step::key("selected"));
            out = edit::set(&out, &p, Some(&s(new)))?;
        }
    }
    Ok(out)
}

/// 一条规则：它在哪条路由里、叫什么。
#[derive(Debug, Clone, PartialEq)]
pub struct RuleRef {
    pub route: String,
    pub rule: String,
}

/// 把请求转发给策略组 `name` 的规则，按配置里出现的顺序。
pub fn group_refs(cfg: &Config, name: &str) -> Vec<RuleRef> {
    cfg.routes
        .iter()
        .flat_map(|route| {
            route
                .rules
                .iter()
                .filter(|rule| rule.to.as_ref().and_then(Target::name) == Some(name))
                .map(|rule| RuleRef {
                    route: route.name.clone(),
                    rule: rule.name.clone(),
                })
        })
        .collect()
}

/// 指定了路由 `name` 的密钥。
///
/// **没指定路由的密钥不在这里**，哪怕 `name` 正是默认路由：它们用的是
/// 「默认路由」这个位置，不是这个名字 —— 默认路由换了，它们跟着换。
pub fn route_users(cfg: &Config, name: &str) -> Vec<String> {
    cfg.clients
        .iter()
        .filter(|c| c.route.as_deref() == Some(name))
        .map(|c| c.name.clone())
        .collect()
}

/// 把 `text` 里引用路由 `old` 的地方都改成 `new`：指定了它的密钥，以及
/// 默认路由。
///
/// **默认路由没写在配置里、被改名的又正是它时，要把新名字写进去** —— 不写
/// 的话，改完名默认路由仍是「叫默认的那条」，而那条已经不存在了。反过来，
/// 新名字就是「默认」时不写：默认值不写进文件。
pub fn rename_route(text: &str, cfg: &Config, old: &str, new: &str) -> Result<String, EditError> {
    let mut out = text.to_string();
    let s = |v: &str| Value::String(v.to_string());
    for (i, c) in cfg.clients.iter().enumerate() {
        if c.route.as_deref() == Some(old) {
            out = edit::set(
                &out,
                &[Step::key("clients"), Step::Index(i), Step::key("route")],
                Some(&s(new)),
            )?;
        }
    }
    let default = cfg
        .default_route
        .as_deref()
        .unwrap_or(tw_engine::DEFAULT_ROUTE);
    if default == old {
        let value = (new != tw_engine::DEFAULT_ROUTE).then(|| s(new));
        out = edit::set(&out, &[Step::key("default_route")], value.as_ref())?;
    }
    Ok(out)
}

/// 谁引用了网关密钥 `name`：按它分流的规则，以及「它是默认密钥」这件事。
///
/// **改名和删除都要先问它。**规则里的 `client` 是精确匹配一个密钥名字，
/// 名字变了而规则没跟着变，那条规则从此一次也不会命中 —— 而配置照样通过
/// 校验，用户只会发现「这条规则不知道什么时候起不灵了」。
pub fn client_refs(cfg: &Config, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    for route in &cfg.routes {
        for rule in &route.rules {
            if rule.when.client.as_deref() == Some(name) {
                out.push(format!("rule `{}` of route `{}`", rule.name, route.name));
            }
        }
    }
    if cfg.default_key.as_deref() == Some(name) {
        out.push("the default key".to_string());
    }
    out
}

/// 把 `text` 里引用密钥 `old` 的地方都改成 `new`：按它分流的规则，以及默认密钥。
pub fn rename_client(text: &str, cfg: &Config, old: &str, new: &str) -> Result<String, EditError> {
    let mut out = text.to_string();
    for (r, route) in cfg.routes.iter().enumerate() {
        for (i, rule) in route.rules.iter().enumerate() {
            if rule.when.client.as_deref() == Some(old) {
                out = edit::set(
                    &out,
                    &[
                        Step::key("routes"),
                        Step::Index(r),
                        Step::key("rules"),
                        Step::Index(i),
                        Step::key("when"),
                        Step::key("client"),
                    ],
                    Some(&Value::String(new.to_string())),
                )?;
            }
        }
    }
    if cfg.default_key.as_deref() == Some(old) {
        out = edit::set(
            &out,
            &[Step::key("default_key")],
            Some(&Value::String(new.to_string())),
        )?;
    }
    Ok(out)
}

/// 把 `text` 里转发给策略组 `old` 的规则都改成转发给 `new`。
pub fn rename_group(text: &str, cfg: &Config, old: &str, new: &str) -> Result<String, EditError> {
    let mut out = text.to_string();
    for (r, route) in cfg.routes.iter().enumerate() {
        for (i, rule) in route.rules.iter().enumerate() {
            if rule.to.as_ref().and_then(Target::name) == Some(old) {
                out = edit::set(
                    &out,
                    &[
                        Step::key("routes"),
                        Step::Index(r),
                        Step::key("rules"),
                        Step::Index(i),
                        Step::key("to"),
                    ],
                    Some(&Value::String(new.to_string())),
                )?;
            }
        }
    }
    Ok(out)
}

/// 写着某个别名的一条规则，和写在哪个字段上。
#[derive(Debug, Clone, PartialEq)]
pub struct AliasRuleRef {
    pub route: String,
    pub rule: String,
    /// `when.model` 或 `set.model`
    pub field: &'static str,
}

/// 写着别名 `name` 的地方。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AliasRefs {
    /// 可见模型（`allow`）里有一项正是这个名字的密钥
    pub keys: Vec<String>,
    pub rules: Vec<AliasRuleRef>,
}

impl AliasRefs {
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.rules.is_empty()
    }
}

/// 写着别名 `name` 的密钥和规则，按配置里出现的顺序。**按整个值比**：`allow` 里的
/// `claude-*` 放行的是一批名字，不是在说这个别名；改名时也不该去改它。
///
/// 阶段二规则（带 `provider_would_be`）的 `set.model` 不算：它原样发给已经选定的那家
/// 上游，不经过别名表 —— 写的是那家上游的模型名，碰巧和别名同名而已。
pub fn alias_refs(cfg: &Config, name: &str) -> AliasRefs {
    let mut out = AliasRefs::default();
    for c in &cfg.clients {
        if c.allow.iter().flatten().any(|m| m == name) {
            out.keys.push(c.name.clone());
        }
    }
    for route in &cfg.routes {
        for rule in &route.rules {
            let mut push = |field| {
                out.rules.push(AliasRuleRef {
                    route: route.name.clone(),
                    rule: rule.name.clone(),
                    field,
                })
            };
            if rule.when.model.as_deref() == Some(name) {
                push("when.model");
            }
            if sets_alias(rule, name) {
                push("set.model");
            }
        }
    }
    out
}

/// 这条规则的 `set.model` 是不是在说别名 `name`（阶段二的不是，见 [`alias_refs`]）
fn sets_alias(rule: &tw_engine::Rule, name: &str) -> bool {
    rule.when.provider_would_be.is_none()
        && rule.set.as_ref().and_then(|s| s.model.as_deref()) == Some(name)
}

/// 别名 `old` 改名成 `renamed`（改名之后的那一项：新名字和它的模型列表）时，把 `text`
/// 里跟着改的引用改成新名，返回改了哪些。`cfg` 是 `text` 解析出来的那一份 —— 下标要
/// 对得上。
///
/// 一般就是写着旧名的那些（[`alias_refs`]）。**旧名也是改名后列表里的一个模型名时，
/// 密钥的 `allow` 和规则的 `when.model` 不动**：改名之后旧名说的是那个真模型，而继承
/// （真名 → 列着它的别名）照样放行、匹配改名后的别名；改成新名反而把直接用真名的客户端
/// 挡在外面、让条件不再匹配它。`set.model` 照样改：它是要发的名称，不是在匹配，不继承
/// —— 留着旧名就成了到处发那个真名，原来按别名对到别的名称的那几家就到不了了。
///
/// **只换那一个值**（[`tw_yaml::set`]）：`allow` 里别的项、它们的写法和注释原样不动。
pub fn rename_alias(
    text: &str,
    cfg: &Config,
    old: &str,
    renamed: &crate::Alias,
) -> Result<(String, AliasRefs), EditError> {
    let matchers = !renamed.models.iter().any(|m| m == old);
    let mut out = text.to_string();
    let mut done = AliasRefs::default();
    let to = tw_yaml::Scalar::s(&renamed.name);
    for (i, c) in cfg.clients.iter().enumerate().filter(|_| matchers) {
        let mut hit = false;
        for (j, _) in c
            .allow
            .iter()
            .flatten()
            .enumerate()
            .filter(|(_, m)| *m == old)
        {
            out = tw_yaml::set(
                &out,
                &[
                    Step::key("clients"),
                    Step::Index(i),
                    Step::key("allow"),
                    Step::Index(j),
                ],
                &to,
            )?;
            hit = true;
        }
        if hit {
            done.keys.push(c.name.clone());
        }
    }
    for (r, route) in cfg.routes.iter().enumerate() {
        for (i, rule) in route.rules.iter().enumerate() {
            let at = |a: &str, b: &str| {
                [
                    Step::key("routes"),
                    Step::Index(r),
                    Step::key("rules"),
                    Step::Index(i),
                    Step::key(a),
                    Step::key(b),
                ]
            };
            let mut push = |field| {
                done.rules.push(AliasRuleRef {
                    route: route.name.clone(),
                    rule: rule.name.clone(),
                    field,
                })
            };
            if matchers && rule.when.model.as_deref() == Some(old) {
                out = tw_yaml::set(&out, &at("when", "model"), &to)?;
                push("when.model");
            }
            if sets_alias(rule, old) {
                out = tw_yaml::set(&out, &at("set", "model"), &to)?;
                push("set.model");
            }
        }
    }
    Ok((out, done))
}

/// 把 `text` 里用着代理 `old` 的上游都改成用 `new`。
pub fn rename_proxy(text: &str, cfg: &Config, old: &str, new: &str) -> Result<String, EditError> {
    let mut out = text.to_string();
    for (i, p) in cfg.providers.iter().enumerate() {
        if p.proxy == old {
            out = edit::set(
                &out,
                &[Step::key("providers"), Step::Index(i), Step::key("proxy")],
                Some(&Value::String(new.to_string())),
            )?;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &str = "version: 1
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
clients:
  - name: c
    key: tw-k
proxies:
  - name: hk
    type: socks5h
    addr: 127.0.0.1:7890
providers:
  - name: relay
    base_url: https://relay.example
    key: sk-a
    proxy: hk
  - name: 官方
    base_url: https://api.anthropic.com
    key: sk-b
groups:
  - name: pool
    type: fallback
    providers: [relay, 官方]
routes:
  - name: default
    rules:
      - name: 长上下文
        when: { input_tokens: '>200k' }
        to: relay
      - name: 中转脱敏
        when:
          provider_would_be: [relay]
        set: { max_tokens: 4096 }
      - name: 兜底
        to: pool
";

    fn cfg(text: &str) -> Config {
        crate::try_parse(text).unwrap_or_else(|e| panic!("{e}\n{text}"))
    }

    #[test]
    fn every_kind_of_reference_to_an_upstream_is_found() {
        let refs = provider_refs(&cfg(CFG), "relay");
        assert_eq!(
            refs,
            vec![
                ProviderRef::RuleTarget {
                    route: "default".into(),
                    rule: "长上下文".into()
                },
                ProviderRef::RuleCondition {
                    route: "default".into(),
                    rule: "中转脱敏".into()
                },
                ProviderRef::Group {
                    group: "pool".into()
                },
            ]
        );
        assert!(provider_refs(&cfg(CFG), "没有这家").is_empty());
    }

    #[test]
    fn renaming_an_upstream_moves_every_reference_in_one_write() {
        // 改名和它的引用必须在同一个版本里一起变 —— 否则中间那一版会被
        // 校验拒掉（规则指向一个不存在的名字）
        let c = cfg(CFG);
        let text = edit::upsert(
            CFG,
            edit::PROVIDERS,
            Some("relay"),
            &serde_yaml_ng::from_str(
                "name: relay-hk\nbase_url: https://relay.example\nkey: sk-a\nproxy: hk\n",
            )
            .unwrap(),
        )
        .unwrap();
        let out = rename_provider(&text, &c, "relay", "relay-hk").unwrap();
        let after = cfg(&out);
        assert!(provider_refs(&after, "relay").is_empty());
        assert_eq!(provider_refs(&after, "relay-hk").len(), 3);
        assert_eq!(after.groups[0].providers, ["relay-hk", "官方"]);
    }

    /// 指定模型里写着这一家，也是引用它：删之前要说，改名时只改那一项的 `provider`
    #[test]
    fn a_pinned_model_refers_to_its_upstream_and_follows_a_rename() {
        let text = CFG.replace(
            "        to: relay\n",
            "        to:\n          - { provider: relay, model: m-relay }\n          - { provider: 官方, model: m }\n",
        );
        let c = cfg(&text);
        assert_eq!(
            provider_refs(&c, "官方"),
            vec![
                ProviderRef::RuleTarget {
                    route: "default".into(),
                    rule: "长上下文".into()
                },
                ProviderRef::Group {
                    group: "pool".into()
                }
            ]
        );
        let renamed = edit::upsert(
            &text,
            edit::PROVIDERS,
            Some("relay"),
            &serde_yaml_ng::from_str(
                "name: relay-hk\nbase_url: https://relay.example\nkey: sk-a\nproxy: hk\n",
            )
            .unwrap(),
        )
        .unwrap();
        let out = rename_provider(&renamed, &c, "relay", "relay-hk").unwrap();
        let after = cfg(&out);
        assert_eq!(
            after.routes[0].rules[0].to,
            Some(Target::Models(vec![
                tw_engine::Pinned {
                    provider: "relay-hk".into(),
                    model: "m-relay".into()
                },
                tw_engine::Pinned {
                    provider: "官方".into(),
                    model: "m".into()
                },
            ]))
        );
        assert!(provider_refs(&after, "relay").is_empty());
    }

    #[test]
    fn renaming_a_price_sheet_moves_the_upstreams_that_use_it() {
        let text = CFG.replace("    proxy: hk\n", "    proxy: hk\n    pricing: 中转\n")
            + "pricing:\n  sheets:\n    - name: 中转\n      multiplier: 0.8\n";
        let c = cfg(&text);
        assert_eq!(sheet_users(&c, "中转"), ["relay"]);
        let renamed = edit::upsert(
            &text,
            edit::PRICE_SHEETS,
            Some("中转"),
            &serde_yaml_ng::from_str("name: 中转协议价\nmultiplier: 0.8\n").unwrap(),
        )
        .unwrap();
        let out = rename_sheet(&renamed, &c, "中转", "中转协议价").unwrap();
        let after = cfg(&out);
        assert_eq!(sheet_users(&after, "中转协议价"), ["relay"]);
    }

    #[test]
    fn a_provider_pointing_at_a_missing_sheet_is_a_config_error() {
        // 静默退回默认价的话，算出来的钱看起来正常，而折扣从没生效过
        let text = CFG.replace("    proxy: hk\n", "    proxy: hk\n    pricing: 没有这张\n");
        let e = crate::try_parse(&text).unwrap_err();
        assert!(e.message.text.contains("没有这张"), "{}", e.message);
    }

    #[test]
    fn the_rules_that_forward_to_a_group_are_found() {
        assert_eq!(
            group_refs(&cfg(CFG), "pool"),
            vec![RuleRef {
                route: "default".into(),
                rule: "兜底".into()
            }]
        );
        assert!(group_refs(&cfg(CFG), "没有这个组").is_empty());
    }

    #[test]
    fn renaming_a_group_moves_the_rules_that_forward_to_it() {
        let c = cfg(CFG);
        let text = edit::upsert(
            CFG,
            edit::GROUPS,
            Some("pool"),
            &serde_yaml_ng::from_str("name: 主力\ntype: fallback\nproviders: [relay, 官方]\n")
                .unwrap(),
        )
        .unwrap();
        let out = rename_group(&text, &c, "pool", "主力").unwrap();
        let after = cfg(&out);
        assert_eq!(group_refs(&after, "主力").len(), 1);
        assert!(group_refs(&after, "pool").is_empty());
    }

    #[test]
    fn renaming_a_route_moves_its_keys_and_the_default() {
        // 默认路由没写在配置里：改名之后要写进去，否则默认路由指向一条不存在的「默认」
        let text = CFG.replace(
            "    key: tw-k\n",
            "    key: tw-k\n  - name: codex\n    key: tw-x\n    route: default\n",
        );
        let c = cfg(&text);
        assert_eq!(route_users(&c, "default"), ["codex"]);
        let renamed = edit::upsert(
            &text,
            edit::ROUTES,
            Some("default"),
            &edit::parse(&text).unwrap()["routes"][0]
                .as_mapping()
                .map(|m| {
                    let mut m = m.clone();
                    m.insert("name".into(), "通用".into());
                    m
                })
                .unwrap(),
        )
        .unwrap();
        let out = rename_route(&renamed, &c, "default", "通用").unwrap();
        let after = cfg(&out);
        assert_eq!(after.default_route.as_deref(), Some("通用"));
        assert_eq!(route_users(&after, "通用"), ["codex"]);
        // 改回「默认」：默认值不写进文件
        let back = edit::upsert(
            &out,
            edit::ROUTES,
            Some("通用"),
            &edit::parse(&text).unwrap()["routes"][0]
                .as_mapping()
                .cloned()
                .unwrap(),
        )
        .unwrap();
        let back = rename_route(&back, &after, "通用", "default").unwrap();
        assert!(!back.contains("default_route"), "{back}");
    }

    #[test]
    fn renaming_a_proxy_moves_the_upstreams_that_use_it() {
        let c = cfg(CFG);
        let text = edit::upsert(
            CFG,
            edit::PROXIES,
            Some("hk"),
            &serde_yaml_ng::from_str("name: hk-socks\ntype: socks5h\naddr: 127.0.0.1:7890\n")
                .unwrap(),
        )
        .unwrap();
        let out = rename_proxy(&text, &c, "hk", "hk-socks").unwrap();
        let after = cfg(&out);
        assert_eq!(proxy_users(&after, "hk-socks"), ["relay"]);
        assert!(proxy_users(&after, "hk").is_empty());
    }

    const ALIASED: &str = "version: 1
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
clients:
  - name: c
    key: tw-k
    allow: [sonnet, 'claude-*', sonnet-x]  # 只给这几个
  - name: d
    key: tw-d
    allow:
      - gpt-5
providers:
  - name: relay
    base_url: https://relay.example
    key: sk-a
aliases:
  sonnet: [claude-sonnet-5, anthropic/claude-sonnet-5]
routes:
  - name: default
    rules:
      - name: 降级
        when: { model: sonnet }
        set: { model: sonnet }
      - name: 阶段二
        when:
          provider_would_be: relay
        set:
          model: sonnet
      - name: 通配
        when: { model: 'son*' }
        to: relay
";

    /// 只认整个值：`claude-*`、`son*` 是一批名字，阶段二的 `set.model` 原样发出 —— 都不算
    #[test]
    fn an_alias_is_referenced_by_whole_values_only() {
        let refs = alias_refs(&cfg(ALIASED), "sonnet");
        assert_eq!(refs.keys, ["c"]);
        assert_eq!(
            refs.rules,
            [
                AliasRuleRef {
                    route: "default".into(),
                    rule: "降级".into(),
                    field: "when.model"
                },
                AliasRuleRef {
                    route: "default".into(),
                    rule: "降级".into(),
                    field: "set.model"
                },
            ]
        );
        assert!(alias_refs(&cfg(ALIASED), "gpt").is_empty());
    }

    /// 改名只换那几个值：`allow` 里别的项、行内写法、注释原样
    #[test]
    fn renaming_an_alias_rewrites_exactly_the_values_that_name_it() {
        let c = cfg(ALIASED);
        let renamed = crate::Alias {
            name: "claude-sonnet".into(),
            models: vec!["claude-sonnet-5".into(), "anthropic/claude-sonnet-5".into()],
        };
        let text = edit::upsert_alias(ALIASED, Some("sonnet"), &renamed).unwrap();
        let (out, done) = rename_alias(&text, &c, "sonnet", &renamed).unwrap();
        // 改了的就是写着旧名的那些
        assert_eq!(done, alias_refs(&c, "sonnet"));
        assert!(
            out.contains("    allow: [claude-sonnet, 'claude-*', sonnet-x]  # 只给这几个\n"),
            "{out}"
        );
        let after = cfg(&out);
        let rules = &after.routes[0].rules;
        assert_eq!(rules[0].when.model.as_deref(), Some("claude-sonnet"));
        assert_eq!(
            rules[0].set.as_ref().unwrap().model.as_deref(),
            Some("claude-sonnet")
        );
        // 阶段二的那条原样发给上游，不是在说这个别名
        assert_eq!(
            rules[1].set.as_ref().unwrap().model.as_deref(),
            Some("sonnet")
        );
        assert_eq!(rules[2].when.model.as_deref(), Some("son*"));
        assert!(alias_refs(&after, "sonnet").is_empty());
        assert_eq!(alias_refs(&after, "claude-sonnet").keys, ["c"]);
        assert_eq!(after.aliases[0].name, "claude-sonnet");
    }

    /// 别名和它的一个模型同名（`claude-sonnet-5: [claude-sonnet-5, …]`），改名之后旧名
    /// 还在列表里：旧名说的是那个真模型，继承照样放行、匹配改名后的别名。`allow` 和
    /// `when.model` 改成新名的话，直接用真名的客户端就被挡在外面了 —— 不动，也不报。
    /// `set.model` 是要发的名称、不继承，照样改
    #[test]
    fn renaming_an_alias_that_lists_its_own_name_leaves_the_matching_references() {
        let text = ALIASED.replace(
            "  sonnet: [claude-sonnet-5, anthropic/claude-sonnet-5]\n",
            "  claude-sonnet-5: [claude-sonnet-5, anthropic/claude-sonnet-5]\n",
        );
        let text = text.replace("{ model: sonnet }", "{ model: claude-sonnet-5 }");
        let text = text.replace("[sonnet, 'claude-*'", "[claude-sonnet-5, 'claude-*'");
        let c = cfg(&text);
        assert_eq!(alias_refs(&c, "claude-sonnet-5").keys, ["c"]);
        let renamed = crate::Alias {
            name: "sonnet".into(),
            models: vec!["claude-sonnet-5".into(), "anthropic/claude-sonnet-5".into()],
        };
        let edited = edit::upsert_alias(&text, Some("claude-sonnet-5"), &renamed).unwrap();
        let (out, done) = rename_alias(&edited, &c, "claude-sonnet-5", &renamed).unwrap();
        assert!(done.keys.is_empty(), "{done:?}");
        assert_eq!(
            done.rules,
            [AliasRuleRef {
                route: "default".into(),
                rule: "降级".into(),
                field: "set.model"
            }]
        );
        assert!(
            out.contains("    allow: [claude-sonnet-5, 'claude-*', sonnet-x]  # 只给这几个\n"),
            "{out}"
        );
        let after = cfg(&out);
        let rules = &after.routes[0].rules;
        assert_eq!(rules[0].when.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(
            rules[0].set.as_ref().unwrap().model.as_deref(),
            Some("sonnet")
        );
        assert_eq!(after.aliases[0].name, "sonnet");

        // 改名的同时把旧名从列表里拿掉了：旧名不再指向这个别名，跟着改
        let narrowed = crate::Alias {
            name: "sonnet".into(),
            models: vec!["anthropic/claude-sonnet-5".into()],
        };
        let edited = edit::upsert_alias(&text, Some("claude-sonnet-5"), &narrowed).unwrap();
        let (out, done) = rename_alias(&edited, &c, "claude-sonnet-5", &narrowed).unwrap();
        assert_eq!(done, alias_refs(&c, "claude-sonnet-5"));
        let after = cfg(&out);
        assert_eq!(
            after.routes[0].rules[0].when.model.as_deref(),
            Some("sonnet")
        );
        assert_eq!(after.clients[0].allow.as_ref().unwrap()[0], "sonnet");
    }
}
