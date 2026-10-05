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
///
/// 名称分两侧：
///
/// - **客户端那一侧**（[`Self::all`]、[`Self::providers_for`]、列表和准入）：真模型
///   和别名都在。别名登记在**有清单、能服务它**的上游名下；和别名同名的真模型让给
///   别名（别名优先：请求这个名称就按别名处理，同名真模型没列进别名的那家服务不了它）。
/// - **发出去的那一侧**（[`Self::offers`]、[`Self::count_for`]）：每家清单里的原名。
///   别名只是客户端的叫法，指定模型、第二阶段改写的名字原样发出，有没有看这家自己的清单。
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// 客户端那一侧的名称 → 能提供它的 provider（保序，和声明顺序一致）
    by_model: BTreeMap<String, Vec<String>>,
    /// provider → 它清单里的模型（启用范围内）
    offered: BTreeMap<String, BTreeSet<String>>,
    /// provider，按声明顺序
    order: Vec<String>,
    /// provider → 它说的方言
    protocols: BTreeMap<String, String>,
    /// 有模型清单的 provider。**不在里面的不是「什么都不提供」，是「不知道」**
    listed: BTreeSet<String>,
    /// 别名 → 它的模型列表（有序）。**整张表**，谁都服务不了的也在：继承要看它
    aliases: BTreeMap<String, Vec<String>>,
    /// 别名 → 有清单、能服务它的 provider 和发给它的名称，按声明顺序
    served: BTreeMap<String, Vec<(String, String)>>,
}

impl Catalog {
    pub fn build(sources: &[ProviderModels]) -> Self {
        let mut c = Self::default();
        for s in sources {
            c.protocols.insert(s.provider.clone(), s.protocol.clone());
            if !c.order.contains(&s.provider) {
                c.order.push(s.provider.clone());
            }
            if s.known {
                c.listed.insert(s.provider.clone());
            }
            let offered = c.offered.entry(s.provider.clone()).or_default();
            for m in &s.models {
                offered.insert(m.clone());
                let v = c.by_model.entry(m.clone()).or_default();
                if !v.contains(&s.provider) {
                    v.push(s.provider.clone());
                }
            }
        }
        c
    }

    /// 登记别名表（配置里的 `aliases`，名称和它的模型列表，按书写顺序）。
    ///
    /// 一个别名登记在哪家名下：**有清单**、清单里有它列表里任一名称的上游，发过去用
    /// 按列表顺序排在最前的那个（和 `tw_gateway::models::resolve` 对有清单的上游
    /// 得出的一样 —— 清单已经按启用范围过滤过）。没有清单的上游不登记，和真模型
    /// 一样不进列表；发不发给它是挑候选时的事，那时它照样当作能服务。
    ///
    /// 和别名同名的真模型从客户端那一侧拿掉：别名列表里有这个名字的，那几家照样
    /// 以别名的身份在；没列进来的，请求这个名字也到不了它们。
    pub fn with_aliases<'a>(
        mut self,
        aliases: impl IntoIterator<Item = (&'a str, &'a [String])>,
    ) -> Self {
        for (name, models) in aliases {
            self.aliases.insert(name.to_string(), models.to_vec());
            self.by_model.remove(name);
            let served: Vec<(String, String)> = self
                .order
                .iter()
                .filter(|p| self.listed.contains(*p))
                .filter_map(|p| {
                    models
                        .iter()
                        .find(|m| self.offers(p, m) == Some(true))
                        .map(|m| (p.clone(), m.clone()))
                })
                .collect();
            if !served.is_empty() {
                self.by_model.insert(
                    name.to_string(),
                    served.iter().map(|(p, _)| p.clone()).collect(),
                );
                self.served.insert(name.to_string(), served);
            }
        }
        self
    }

    /// 这家清单里有没有这个名字（原名，不经过别名表）。`None` = 不知道：它没有模型清单。
    ///
    /// **不知道不等于不能。**把没有清单的上游当成「什么都没有」，一家不
    /// 实现 `/v1/models` 的中转站就再也收不到请求 —— 而它可能什么都能服务。
    pub fn offers(&self, provider: &str, model: &str) -> Option<bool> {
        self.listed.contains(provider).then(|| {
            self.offered
                .get(provider)
                .is_some_and(|ms| ms.contains(model))
        })
    }

    /// 这家清单里有几个模型。别名不算 —— 它们是别家模型的另一个叫法。
    pub fn count_for(&self, provider: &str) -> usize {
        self.offered.get(provider).map_or(0, BTreeSet::len)
    }

    /// 全集：客户端那一侧的名称，别名在内。
    pub fn all(&self) -> Vec<&str> {
        self.by_model.keys().map(|s| s.as_str()).collect()
    }

    /// 谁能提供这个名称。别名是有清单、能服务它的那几家。
    pub fn providers_for(&self, model: &str) -> &[String] {
        self.by_model
            .get(model)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// 这个名称是别名时，它的模型列表（有序）。谁都服务不了的别名也有。
    pub fn alias(&self, name: &str) -> Option<&[String]> {
        self.aliases.get(name).map(Vec::as_slice)
    }

    /// 有清单、能服务这个别名的上游，和发给它的名称，按声明顺序。
    pub fn served(&self, alias: &str) -> &[(String, String)] {
        self.served.get(alias).map(Vec::as_slice).unwrap_or(&[])
    }

    /// 这个别名按列表顺序第一个有上游提供的名称，和提供它的头一家。
    /// `/v1/models` 给别名的元数据按它查。不是别名、或者没有谁服务它时是 `None`。
    pub fn first_served(&self, alias: &str) -> Option<(&str, &str)> {
        let served = self.served.get(alias)?;
        self.aliases.get(alias)?.iter().find_map(|m| {
            served
                .iter()
                .find(|(_, s)| s == m)
                .map(|(p, s)| (p.as_str(), s.as_str()))
        })
    }

    pub fn is_empty(&self) -> bool {
        self.offered.values().all(BTreeSet::is_empty)
    }

    /// `allow` 放不放行这个名称。
    ///
    /// **继承只从真名到别名**：写上游的模型名（glob）时，指向它的别名也放行 —— 别名
    /// 列表里任一名称匹配就算；写别名时只管别名本身，它列表里的模型不跟着放出来。
    ///
    /// `name` 是**客户端那一侧的名称**：客户端写的、规则改写的、插件改的。原样发出的名字
    /// （指定模型、阶段二改的）不经过别名表，按名字本身对 glob 就是了。
    pub fn allows(&self, name: &str, patterns: &[String]) -> bool {
        let hit = |m: &str| patterns.iter().any(|p| glob_match(p, m));
        hit(name)
            || self
                .aliases
                .get(name)
                .is_some_and(|ms| ms.iter().any(|m| hit(m)))
    }

    /// **唯一的真相来源**。
    ///
    /// `GET /v1/models` 把它列出来，`POST /v1/messages` 检查请求的模型在
    /// 不在里面 —— 同一个函数。这样列表和准入不可能不一致，而 one-api 和
    /// new-api 都栽在「准入检查散落到各处」这件事上。
    ///
    /// 别名和真模型一起列：别名按能服务它的那几家过滤方言，按 [`Self::allows`]
    /// 继承 `allow`。
    ///
    /// `allow` 的三种状态语义**必须分明**，因为那两个项目在这里正好相反
    /// （一个里空表示全放行，另一个表示全禁止）：
    ///
    /// - `None`（不写）→ 只按方言过滤。默认，绝大多数情况
    /// - `Some(非空)` → 在方言过滤的基础上再按 glob 保留
    /// - `Some(空)` → **一个都不给**。「临时禁用这个客户端」的正当用法
    pub fn resolve_allowed(
        &self,
        protocols: Option<&[&str]>,
        allow: Option<&[String]>,
    ) -> Vec<String> {
        self.by_model
            .iter()
            .filter(|(_, provs)| match protocols {
                // 至少有一家能服务这种请求的上游提供它（同协议，或者有现成转换）
                Some(ok) => provs.iter().any(|p| {
                    self.protocols
                        .get(p)
                        .is_some_and(|x| ok.contains(&x.as_str()))
                }),
                // 不知道方言就不过滤 —— 猜错了会把用户能用的模型藏起来，
                // 而那比多列几个更难查。
                None => true,
            })
            .map(|(m, _)| m)
            .filter(|m| allow.is_none_or(|patterns| self.allows(m, patterns)))
            .cloned()
            .collect()
    }

    /// 这个客户端能用这个模型吗。和上面同一套逻辑。
    pub fn admits(
        &self,
        model: &str,
        protocols: Option<&[&str]>,
        allow: Option<&[String]>,
    ) -> bool {
        self.resolve_allowed(protocols, allow)
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
            c.resolve_allowed(Some(&["anthropic"]), None),
            owned(&["claude-haiku-4-5", "claude-opus-4-1", "claude-sonnet-4-5"])
        );
        assert_eq!(
            c.resolve_allowed(Some(&["openai-chat"]), None),
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
            c.resolve_allowed(Some(&["anthropic"]), Some(&owned(&["claude-haiku-*"]))),
            owned(&["claude-haiku-4-5"])
        );
    }

    #[test]
    fn an_empty_allow_list_means_none_not_all() {
        // **必须特意定死。**one-api 和 new-api 在这里语义正好相反，
        // 分歧本身就说明容易搞错。YAML 里「字段缺失」和「空数组」天然
        // 可区分，所以我们能表达得清楚。
        let c = catalog();
        assert!(
            c.resolve_allowed(Some(&["anthropic"]), Some(&[]))
                .is_empty()
        );
        assert!(!c.resolve_allowed(Some(&["anthropic"]), None).is_empty());
    }

    #[test]
    fn exact_names_in_allow_pin_a_fixed_list() {
        // glob 是模式过滤，精确名字就是固定列表 —— 同一个字段，两种用法。
        let c = catalog();
        assert_eq!(
            c.resolve_allowed(
                Some(&["anthropic"]),
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
            c.resolve_allowed(Some(&["anthropic"]), Some(&owned(&["gpt-9"])))
                .is_empty()
        );
    }

    #[test]
    fn admission_and_listing_agree_by_construction() {
        // 这是这个模块存在的核心理由：**一个函数，两处调用**。
        // 两处各写一遍的话，「列表里有但用不了」这种状态迟早出现。
        let c = catalog();
        for dialect in [Some(&["anthropic"][..]), Some(&["openai-chat"][..]), None] {
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
        assert!(c.resolve_allowed(Some(&["anthropic"]), None).is_empty());
        assert!(!c.admits("anything", None, None));
    }

    // ─────────────────────────────────────────────── 别名

    /// 同一个模型在三家叫法不同，外加一个没有清单的中转站
    fn with_aliases() -> Catalog {
        aliased(
            Catalog::build(&[
                pm(
                    "bedrock",
                    "bedrock",
                    &["us.anthropic.claude-sonnet-5-v1:0", "claude-haiku-5"],
                ),
                pm(
                    "anthropic",
                    "anthropic",
                    &["claude-sonnet-5", "claude-haiku-5"],
                ),
                // 有一个叫 gpt-x 的真模型，别名 gpt-x 却没列它
                pm(
                    "relay",
                    "openai-chat",
                    &["gpt-x", "anthropic/claude-sonnet-5"],
                ),
                pm("openai", "openai-chat", &["gpt-x-2026"]),
                pm("no-list", "anthropic", &[]),
            ]),
            &[
                (
                    "claude-sonnet-5",
                    &[
                        "claude-sonnet-5",
                        "us.anthropic.claude-sonnet-5-v1:0",
                        "anthropic/claude-sonnet-5",
                    ],
                ),
                ("gpt-x", &["gpt-x-2026"]),
                ("sonnet", &["claude-sonnet-5"]),
                // 哪家都没有
                ("ghost", &["nobody-has-this"]),
            ],
        )
    }

    /// 给目录登记一张别名表
    fn aliased(c: Catalog, table: &[(&str, &[&str])]) -> Catalog {
        let t: Vec<(String, Vec<String>)> = table
            .iter()
            .map(|(n, ms)| (n.to_string(), owned(ms)))
            .collect();
        c.with_aliases(t.iter().map(|(n, ms)| (n.as_str(), ms.as_slice())))
    }

    #[test]
    fn an_alias_is_offered_by_each_listed_upstream_that_has_any_of_its_names() {
        let c = with_aliases();
        // 按声明顺序；每家发它自己的那个名字
        assert_eq!(
            c.providers_for("claude-sonnet-5"),
            ["bedrock", "anthropic", "relay"]
        );
        assert_eq!(
            c.served("claude-sonnet-5"),
            [
                ("bedrock".into(), "us.anthropic.claude-sonnet-5-v1:0".into()),
                ("anthropic".into(), "claude-sonnet-5".into()),
                ("relay".into(), "anthropic/claude-sonnet-5".into()),
            ]
        );
        assert_eq!(c.providers_for("sonnet"), ["anthropic"]);
        // 谁都服务不了的别名不列，但继承还要看它的列表
        assert!(c.providers_for("ghost").is_empty());
        assert!(!c.all().contains(&"ghost"));
        assert_eq!(c.alias("ghost"), Some(&owned(&["nobody-has-this"])[..]));
        assert_eq!(c.alias("claude-haiku-5"), None);
        // 别名不算进一家的模型数：那是别家模型的另一个叫法
        assert_eq!(c.count_for("anthropic"), 2);
    }

    #[test]
    fn an_upstream_without_a_list_does_not_list_an_alias() {
        // 和真模型一样：不知道它有什么，就不替它承诺。发不发给它是挑候选时的事
        let c = aliased(
            Catalog::build(&[pm("no-list", "anthropic", &[])]),
            &[("sonnet", &["claude-sonnet-5"])],
        );
        assert!(c.providers_for("sonnet").is_empty());
        assert!(c.is_empty());
        assert_eq!(c.offers("no-list", "claude-sonnet-5"), None);
    }

    #[test]
    fn an_alias_takes_its_name_from_a_real_model_it_does_not_list() {
        let c = with_aliases();
        // relay 的 gpt-x 没列进别名 gpt-x：这个名称归别名，到不了 relay
        assert_eq!(c.providers_for("gpt-x"), ["openai"]);
        // 只在客户端那一侧让出来：relay 的清单里照样有 gpt-x（指定模型原样发它）
        assert_eq!(c.offers("relay", "gpt-x"), Some(true));
        assert_eq!(c.offers("bedrock", "claude-sonnet-5"), Some(false));
        // 和自己列表里某个名称同名的别名只出现一次
        let all = c.all();
        assert_eq!(all.iter().filter(|m| **m == "claude-sonnet-5").count(), 1);
        assert_eq!(
            all,
            [
                "anthropic/claude-sonnet-5",
                "claude-haiku-5",
                "claude-sonnet-5",
                "gpt-x",
                "gpt-x-2026",
                "sonnet",
                "us.anthropic.claude-sonnet-5-v1:0",
            ]
        );
    }

    #[test]
    fn an_alias_nobody_serves_still_takes_its_name() {
        // 请求这个名称按别名处理，而别名谁都服务不了：列出 relay 的同名真模型就是
        // 一个用不了的承诺
        let c = aliased(
            Catalog::build(&[pm("relay", "openai-chat", &["gpt-x"])]),
            &[("gpt-x", &["gpt-x-2026"])],
        );
        assert!(c.all().is_empty());
        assert!(!c.admits("gpt-x", None, None));
        assert_eq!(c.offers("relay", "gpt-x"), Some(true));
    }

    #[test]
    fn an_alias_is_filtered_by_the_dialects_of_the_upstreams_that_serve_it() {
        let c = with_aliases();
        // gpt-x 只有 openai-chat 的上游服务它
        assert!(c.admits("gpt-x", Some(&["openai-chat"]), None));
        assert!(!c.admits("gpt-x", Some(&["anthropic"]), None));
        assert!(c.admits("claude-sonnet-5", Some(&["openai-chat"]), None));
    }

    #[test]
    fn allowing_a_real_name_lets_its_aliases_through_but_not_the_other_way() {
        let c = with_aliases();
        // 写上游的模型名：指向它的别名一起放行
        assert_eq!(
            c.resolve_allowed(None, Some(&owned(&["us.anthropic.*"]))),
            owned(&["claude-sonnet-5", "us.anthropic.claude-sonnet-5-v1:0"])
        );
        assert_eq!(
            c.resolve_allowed(None, Some(&owned(&["gpt-x-*"]))),
            owned(&["gpt-x", "gpt-x-2026"])
        );
        // 写别名：只有别名本身，它列表里的名字不跟着出来
        assert_eq!(
            c.resolve_allowed(None, Some(&owned(&["sonnet"]))),
            owned(&["sonnet"])
        );
        assert_eq!(
            c.resolve_allowed(None, Some(&owned(&["gpt-x"]))),
            owned(&["gpt-x"])
        );
        // 和别名同名的那个名称就是别名：写它放行别名，别家的叫法不跟着出来
        assert_eq!(
            c.resolve_allowed(None, Some(&owned(&["claude-sonnet-5"]))),
            owned(&["claude-sonnet-5", "sonnet"])
        );
        // 准入和列表是同一个函数
        let allow = owned(&["us.anthropic.*"]);
        assert!(c.admits("claude-sonnet-5", None, Some(&allow)));
        assert!(!c.admits("sonnet", None, Some(&allow)));
        assert!(!c.admits("anthropic/claude-sonnet-5", None, Some(&allow)));
    }

    #[test]
    fn admission_and_listing_agree_with_aliases_too() {
        let c = with_aliases();
        let mut names: Vec<&str> = c.all();
        names.extend(["ghost", "nobody-has-this"]);
        for dialect in [Some(&["anthropic"][..]), Some(&["openai-chat"][..]), None] {
            for allow in [
                None,
                Some(owned(&["claude-*"])),
                Some(owned(&["gpt-x"])),
                Some(owned(&["nobody-*"])),
                Some(vec![]),
            ] {
                let listed = c.resolve_allowed(dialect, allow.as_deref());
                for m in &names {
                    assert_eq!(
                        c.admits(m, dialect, allow.as_deref()),
                        listed.iter().any(|x| x == m),
                        "{m} {dialect:?} {allow:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn an_alias_takes_its_metadata_from_its_first_listed_name_that_is_served() {
        let c = with_aliases();
        // 列表头一个 claude-sonnet-5 有人提供：按提供它的头一家查
        assert_eq!(
            c.first_served("claude-sonnet-5"),
            Some(("anthropic", "claude-sonnet-5"))
        );
        assert_eq!(c.first_served("gpt-x"), Some(("openai", "gpt-x-2026")));
        assert_eq!(c.first_served("ghost"), None);
        assert_eq!(c.first_served("claude-haiku-5"), None);
    }
}
