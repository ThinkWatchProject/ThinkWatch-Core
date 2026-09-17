//! 模型列表：每个客户端看到什么。
//!
//! 三句话：
//!
//! 1. **模型列表不用你写。** 去问每个上游「你有哪些模型」，汇总起来。
//! 2. **每个客户端看到的是这份汇总的一个子集**，默认按它说什么方言过滤。
//! 3. **想再限制，加一行 `allow`。**
//!
//! 模型选了之后走哪个上游是另一回事 —— 那是路由规则的职责。
//! **这两件事分开，是整个设计的关键**：让目录也决定去向，就会和路由
//! 打架，而「一个 250k 的 sonnet 请求听谁的」没有自然的答案。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::rule::glob_match;

/// 一家上游能提供什么。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModels {
    pub provider: String,
    /// 这家说的方言。客户端按方言过滤时用它
    pub protocol: String,
    /// 知不知道这家有什么。**不知道时 `models` 是空的，而那不代表它什么都
    /// 不提供**；知道、但按启用范围过滤完是空的，才是真的什么都不提供
    pub known: bool,
    pub models: Vec<String>,
}

/// 汇总后的目录。
///
/// **不含「这个模型走哪家」的决策** —— 只记录「谁能提供」。去向由路由
/// 决定，这里只回答准入。
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// 模型 → 能提供它的 provider（保序，和声明顺序一致）
    by_model: BTreeMap<String, Vec<String>>,
    /// provider → 它说的方言
    protocols: BTreeMap<String, String>,
    /// 有模型清单的 provider。**不在里面的不是「什么都不提供」，是「不知道」**
    listed: BTreeSet<String>,
}

impl Catalog {
    pub fn build(sources: &[ProviderModels]) -> Self {
        let mut by_model: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut protocols = BTreeMap::new();
        let mut listed = BTreeSet::new();
        for s in sources {
            protocols.insert(s.provider.clone(), s.protocol.clone());
            if s.known {
                listed.insert(s.provider.clone());
            }
            for m in &s.models {
                let v = by_model.entry(m.clone()).or_default();
                if !v.contains(&s.provider) {
                    v.push(s.provider.clone());
                }
            }
        }
        Self {
            by_model,
            protocols,
            listed,
        }
    }

    /// 这家能不能提供这个模型。`None` = 不知道：它没有模型清单。
    ///
    /// **不知道不等于不能。**把没有清单的上游当成「什么都没有」，一家不
    /// 实现 `/v1/models` 的中转站就再也收不到请求 —— 而它可能什么都能服务。
    pub fn offers(&self, provider: &str, model: &str) -> Option<bool> {
        self.listed
            .contains(provider)
            .then(|| self.providers_for(model).iter().any(|p| p == provider))
    }

    /// 这家能服务几个模型。
    pub fn count_for(&self, provider: &str) -> usize {
        self.by_model
            .values()
            .filter(|ps| ps.iter().any(|p| p == provider))
            .count()
    }

    /// 全集。
    pub fn all(&self) -> Vec<&str> {
        self.by_model.keys().map(|s| s.as_str()).collect()
    }

    /// 谁能提供这个模型。
    pub fn providers_for(&self, model: &str) -> &[String] {
        self.by_model
            .get(model)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn is_empty(&self) -> bool {
        self.by_model.is_empty()
    }

    /// **唯一的真相来源**。
    ///
    /// `GET /v1/models` 把它列出来，`POST /v1/messages` 检查请求的模型在
    /// 不在里面 —— 同一个函数。这样列表和准入不可能不一致，而 one-api 和
    /// new-api 都栽在「准入检查散落到各处」这件事上。
    ///
    /// `allow` 的三种状态语义**必须分明**，因为那两个项目在这里正好相反
    /// （一个里空表示全放行，另一个表示全禁止）：
    ///
    /// - `None`（不写）→ 只按方言过滤。默认，绝大多数情况
    /// - `Some(非空)` → 在方言过滤的基础上再按 glob 保留
    /// - `Some(空)` → **一个都不给**。「临时禁用这个客户端」的正当用法
    pub fn resolve_allowed(&self, dialect: Option<&str>, allow: Option<&[String]>) -> Vec<String> {
        let by_dialect: BTreeSet<&str> = self
            .by_model
            .iter()
            .filter(|(_, provs)| match dialect {
                // 至少有一家说这个方言的上游能提供它
                Some(d) => provs
                    .iter()
                    .any(|p| self.protocols.get(p).map(|x| x.as_str()) == Some(d)),
                // 不知道方言就不过滤 —— 猜错了会把用户能用的模型藏起来，
                // 而那比多列几个更难查。
                None => true,
            })
            .map(|(m, _)| m.as_str())
            .collect();

        match allow {
            None => by_dialect.into_iter().map(String::from).collect(),
            Some(patterns) => by_dialect
                .into_iter()
                .filter(|m| patterns.iter().any(|p| glob_match(p, m)))
                .map(String::from)
                .collect(),
        }
    }

    /// 这个客户端能用这个模型吗。和上面同一套逻辑。
    pub fn admits(&self, model: &str, dialect: Option<&str>, allow: Option<&[String]>) -> bool {
        self.resolve_allowed(dialect, allow)
            .iter()
            .any(|m| m == model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pm(p: &str, proto: &str, models: &[&str]) -> ProviderModels {
        ProviderModels {
            provider: p.into(),
            protocol: proto.into(),
            known: !models.is_empty(),
            models: models.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn catalog() -> Catalog {
        // 跟着走的那个例子
        Catalog::build(&[
            pm(
                "anthropic-official",
                "anthropic",
                &["claude-opus-4-1", "claude-sonnet-4-5", "claude-haiku-4-5"],
            ),
            pm(
                "relay-cn",
                "anthropic",
                &["claude-sonnet-4-5", "claude-opus-4-1"],
            ),
            pm("ollama", "openai-chat", &["qwen3-coder", "llama-4"]),
        ])
    }

    fn owned(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_upstream_without_a_list_is_unknown_not_empty() {
        let c = Catalog::build(&[
            pm("relay-cn", "anthropic", &["claude-sonnet-4-5"]),
            pm("no-list", "anthropic", &[]),
        ]);
        assert_eq!(c.offers("relay-cn", "claude-sonnet-4-5"), Some(true));
        assert_eq!(c.offers("relay-cn", "claude-opus-4-1"), Some(false));
        assert_eq!(c.offers("no-list", "claude-opus-4-1"), None);
        assert_eq!(c.count_for("relay-cn"), 1);
        assert_eq!(c.count_for("no-list"), 0);
    }

    #[test]
    fn merging_records_who_can_serve_each_model() {
        let c = catalog();
        assert_eq!(
            c.providers_for("claude-opus-4-1"),
            ["anthropic-official", "relay-cn"]
        );
        assert_eq!(c.providers_for("claude-haiku-4-5"), ["anthropic-official"]);
        assert_eq!(c.providers_for("不存在的"), Vec::<String>::new());
    }

    #[test]
    fn provider_order_is_preserved_because_it_is_the_failover_order() {
        // 声明顺序就是故障转移的优先级（层 0）。目录不能把它打乱。
        let c = Catalog::build(&[
            pm("second", "anthropic", &["m"]),
            pm("first", "anthropic", &["m"]),
        ]);
        assert_eq!(c.providers_for("m"), ["second", "first"]);
    }

    #[test]
    fn a_client_sees_only_what_its_dialect_can_reach() {
        // Claude Code 说 anthropic 方言 → 看不到 ollama 的模型。
        let c = catalog();
        assert_eq!(
            c.resolve_allowed(Some("anthropic"), None),
            owned(&["claude-haiku-4-5", "claude-opus-4-1", "claude-sonnet-4-5"])
        );
        assert_eq!(
            c.resolve_allowed(Some("openai-chat"), None),
            owned(&["llama-4", "qwen3-coder"])
        );
    }

    #[test]
    fn an_unknown_dialect_filters_nothing_rather_than_hiding_things() {
        // 猜错方言会把用户能用的模型藏起来，而「少了一个模型」比
        // 「多了一个」难查得多 —— 后者一试就知道，前者根本不知道该找什么。
        let c = catalog();
        assert_eq!(c.resolve_allowed(None, None).len(), 5);
    }

    #[test]
    fn allow_narrows_on_top_of_the_dialect_filter() {
        let c = catalog();
        assert_eq!(
            c.resolve_allowed(Some("anthropic"), Some(&owned(&["claude-haiku-*"]))),
            owned(&["claude-haiku-4-5"])
        );
    }

    #[test]
    fn an_empty_allow_list_means_none_not_all() {
        // **必须特意定死。**one-api 和 new-api 在这里语义正好相反，
        // 分歧本身就说明容易搞错。YAML 里「字段缺失」和「空数组」天然
        // 可区分，所以我们能表达得清楚。
        let c = catalog();
        assert!(c.resolve_allowed(Some("anthropic"), Some(&[])).is_empty());
        assert!(!c.resolve_allowed(Some("anthropic"), None).is_empty());
    }

    #[test]
    fn exact_names_in_allow_pin_a_fixed_list() {
        // glob 是模式过滤，精确名字就是固定列表 —— 同一个字段，两种用法。
        let c = catalog();
        assert_eq!(
            c.resolve_allowed(
                Some("anthropic"),
                Some(&owned(&["claude-opus-4-1", "claude-sonnet-4-5"]))
            ),
            owned(&["claude-opus-4-1", "claude-sonnet-4-5"])
        );
    }

    #[test]
    fn allowing_something_the_upstreams_do_not_have_conjures_nothing() {
        // 「列表即承诺」：写在 allow 里的模型必须在推导出的全集里，
        // 否则它不会凭空出现。
        let c = catalog();
        assert!(
            c.resolve_allowed(Some("anthropic"), Some(&owned(&["gpt-9"])))
                .is_empty()
        );
    }

    #[test]
    fn admission_and_listing_agree_by_construction() {
        // 这是这个模块存在的核心理由：**一个函数，两处调用**。
        // 两处各写一遍的话，「列表里有但用不了」这种状态迟早出现。
        let c = catalog();
        for dialect in [Some("anthropic"), Some("openai-chat"), None] {
            for allow in [None, Some(owned(&["claude-*"])), Some(vec![])] {
                let listed = c.resolve_allowed(dialect, allow.as_deref());
                for m in &listed {
                    assert!(c.admits(m, dialect, allow.as_deref()), "{m} 列了却不准入");
                }
                for m in c.all() {
                    if !listed.iter().any(|x| x == m) {
                        assert!(!c.admits(m, dialect, allow.as_deref()), "{m} 没列却准入");
                    }
                }
            }
        }
    }

    #[test]
    fn an_empty_catalog_admits_nothing_but_does_not_panic() {
        // 上游一个都没探到时（都不实现 /v1/models 且没写 models 兜底）。
        let c = Catalog::default();
        assert!(c.is_empty());
        assert!(c.resolve_allowed(Some("anthropic"), None).is_empty());
        assert!(!c.admits("anything", None, None));
    }
}
