//! 谁引用了谁：删之前要知道，改名时要跟着改。
//!
//! 配置里的名字就是引用。一个上游改了名、而路由规则还写着旧名字，那份
//! 配置会被校验拒掉 —— 用户看到的是「改个名字都保存不了」；绕过校验的
//! 话更糟，规则会静默地不再命中。所以**改名是一次原子写入**：那一项和
//! 所有引用它的地方在同一个版本里一起变。

use serde_yaml_ng::Value;
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
            if rule.to.as_deref() == Some(name) {
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
            if rule.to.as_deref() == Some(old) {
                let mut p = base.to_vec();
                p.push(Step::key("to"));
                out = edit::set(&out, &p, Some(&s(new)))?;
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
                .filter(|rule| rule.to.as_deref() == Some(name))
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
                out.push(format!("路由「{}」的规则「{}」", route.name, rule.name));
            }
        }
    }
    if cfg.default_key.as_deref() == Some(name) {
        out.push("默认密钥".to_string());
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
            if rule.to.as_deref() == Some(old) {
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
  - name: 默认
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
                    route: "默认".into(),
                    rule: "长上下文".into()
                },
                ProviderRef::RuleCondition {
                    route: "默认".into(),
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
        assert!(e.message.contains("没有这张"), "{}", e.message);
    }

    #[test]
    fn the_rules_that_forward_to_a_group_are_found() {
        assert_eq!(
            group_refs(&cfg(CFG), "pool"),
            vec![RuleRef {
                route: "默认".into(),
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
            "    key: tw-k\n  - name: codex\n    key: tw-x\n    route: 默认\n",
        );
        let c = cfg(&text);
        assert_eq!(route_users(&c, "默认"), ["codex"]);
        let renamed = edit::upsert(
            &text,
            edit::ROUTES,
            Some("默认"),
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
        let out = rename_route(&renamed, &c, "默认", "通用").unwrap();
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
        let back = rename_route(&back, &after, "通用", "默认").unwrap();
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
}
