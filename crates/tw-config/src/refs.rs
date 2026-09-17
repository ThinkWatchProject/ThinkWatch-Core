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
