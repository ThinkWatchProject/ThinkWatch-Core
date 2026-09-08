//! 路由引擎：规则 + 策略组。
//!
//! **层 0 不是特例**（DESIGN.md §3.4）。「不配规则也能跑」在内部被展开成
//! 「一个包含全部 provider 的 fallback 组 + 一条指向它的兜底规则」，
//! 所以引擎只有一条代码路径，用户看到的却是零配置可用。

use serde::{Deserialize, Serialize};

use crate::facts::RequestFacts;
use crate::rule::{MatchError, When};

/// 策略组类型。
///
/// **默认是 `fallback` 而不是负载均衡**，理由很硬：负载均衡会摧毁
/// prompt cache。缓存按 provider 隔离，长会话每轮跳一家就永远命中不了，
/// 而缓存命中与否成本差 5 到 10 倍 —— 随机分流的代价可能是账单翻几倍，
/// 换来的是「分散负载」，**而单用户桌面场景根本没有需要分散的负载**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum GroupType {
    /// 按顺序，第一个健康的就用。缓存友好
    #[default]
    Fallback,
    /// 手动指定一个。缓存友好
    Select,
    /// 轮流。**必须开会话粘滞，否则缓存全废**
    LoadBalance,
}

impl GroupType {
    /// 这个策略会不会让 prompt cache 不稳定。
    ///
    /// UI 上选中时要给一句提示 —— **不要让用户为了省 20% 的单价，
    /// 付出丢掉 90% 缓存折扣的代价**（§3.4）。
    pub fn hurts_cache(&self) -> bool {
        matches!(self, GroupType::LoadBalance)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    #[serde(default, rename = "type")]
    pub kind: GroupType,
    pub providers: Vec<String>,
    /// `load-balance` 必须开。同一个会话固定走同一家，缓存才能保住。
    #[serde(default)]
    pub session_affinity: bool,
    /// `select` 用：当前选中的那个
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    /// **每条规则有名字**。日志里、UI 里、试算结果里都能引用它 ——
    /// 「命中第 4 条」远不如「命中『带缓存的必须走官方』」有用。
    pub name: String,
    #[serde(default)]
    pub when: When,
    /// **可以直接指 provider，不需要先建组**（§3.4 层 1）。大多数分流
    /// 需求到这一层就解决了，不必引入策略组这个概念。
    pub to: String,
}

/// 一次路由的结果，**带上为什么**。
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// 选中的 provider，按优先级排 —— 第一个是首选，后面是故障转移的
    /// 备选。**故障转移和路由是同一次决策的两个部分**，分开算会让「切
    /// 到哪一家」变成一个没人能解释的结果。
    pub candidates: Vec<String>,
    /// 命中了哪条规则。日志和 UI 都要显示它
    pub matched_rule: String,
    /// 经过了哪个组（直接指 provider 时是 None）
    pub via_group: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum RouteError {
    #[error("没有任何规则命中，而且没有兜底规则。加一条不带 `when` 的规则收尾。")]
    NoMatch,
    #[error("规则 `{rule}` 指向 `{target}`，但既没有这个 provider 也没有这个组")]
    UnknownTarget { rule: String, target: String },
    #[error("组 `{0}` 里一个 provider 都没有")]
    EmptyGroup(String),
    #[error(transparent)]
    Match(#[from] MatchError),
}

pub struct Engine {
    routes: Vec<Route>,
    groups: Vec<Group>,
    providers: Vec<String>,
}

impl Engine {
    /// 建引擎。
    ///
    /// **层 0 在这里被展开**：没有 routes 时补一条兜底规则指向一个自动
    /// 生成的、包含全部 provider 的 fallback 组。这样下面的求值逻辑
    /// 只有一条路径。
    pub fn new(providers: Vec<String>, mut groups: Vec<Group>, mut routes: Vec<Route>) -> Self {
        // **没有 provider 时什么都不合成。**合成一个空组会让它过不了
        // `validate`（空组是配置错误），而零 provider 的配置必须合法 ——
        // 否则首次运行时 core 起不来，控制面也起不来，UI 连「你还没配
        // 上游」都说不出口（tw-config::validate 里那段注释）。
        //
        // 这条今天已经立过一次，又被这里破坏了一次。测试抓住了。
        if routes.is_empty() && !providers.is_empty() {
            const ALL: &str = "__all__";
            groups.push(Group {
                name: ALL.to_string(),
                kind: GroupType::Fallback,
                providers: providers.clone(),
                session_affinity: false,
                selected: None,
            });
            routes.push(Route {
                name: "默认：按声明顺序故障转移".to_string(),
                when: When::default(),
                to: ALL.to_string(),
            });
        }
        Self {
            routes,
            groups,
            providers,
        }
    }

    /// 加载时校验。**规则写错了要在这里说，不要等请求进来** ——
    /// 一条指向不存在的 provider 的规则，在运行时的表现是每个命中它的
    /// 请求都失败，而用户看不出是哪条规则的问题。
    pub fn validate(&self) -> Result<(), RouteError> {
        for r in &self.routes {
            r.when.validate()?;
            self.resolve_target(r)?;
        }
        for g in &self.groups {
            if g.providers.is_empty() {
                return Err(RouteError::EmptyGroup(g.name.clone()));
            }
        }
        Ok(())
    }

    /// 阶段一：按请求性质选出候选 provider。
    pub fn route(&self, facts: &RequestFacts) -> Result<Decision, RouteError> {
        for r in &self.routes {
            if r.when.matches(facts)? {
                let (candidates, via_group) = self.resolve_target(r)?;
                return Ok(Decision {
                    candidates,
                    matched_rule: r.name.clone(),
                    via_group,
                });
            }
        }
        Err(RouteError::NoMatch)
    }

    fn resolve_target(&self, r: &Route) -> Result<(Vec<String>, Option<String>), RouteError> {
        // provider 优先于组。同名时按 provider 解释 —— 而校验会挡住
        // 同名的情况，所以这个优先级实际上不会被用到。
        if self.providers.iter().any(|p| p == &r.to) {
            return Ok((vec![r.to.clone()], None));
        }
        if let Some(g) = self.groups.iter().find(|g| g.name == r.to) {
            return Ok((self.expand_group(g), Some(g.name.clone())));
        }
        Err(RouteError::UnknownTarget {
            rule: r.name.clone(),
            target: r.to.clone(),
        })
    }

    fn expand_group(&self, g: &Group) -> Vec<String> {
        match g.kind {
            // 顺序就是优先级。第一个健康的就用，缓存持续命中。
            GroupType::Fallback => g.providers.clone(),
            GroupType::Select => {
                // 选中的排头，其余仍然留着做故障转移 —— **手动选一家不
                // 等于放弃容错**，那家挂了照样该切。
                let mut out = Vec::with_capacity(g.providers.len());
                if let Some(sel) = &g.selected
                    && g.providers.contains(sel)
                {
                    out.push(sel.clone());
                }
                out.extend(
                    g.providers
                        .iter()
                        .filter(|p| Some(*p) != g.selected.as_ref())
                        .cloned(),
                );
                out
            }
            // 轮转的实际选择在数据面（要按会话粘滞），引擎只给出集合。
            GroupType::LoadBalance => g.providers.clone(),
        }
    }

    pub fn groups(&self) -> &[Group] {
        &self.groups
    }
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(model: &str) -> RequestFacts {
        RequestFacts {
            model: model.into(),
            client: "claude-code".into(),
            dialect: "anthropic".into(),
            ..Default::default()
        }
    }

    fn route(name: &str, when_yaml: &str, to: &str) -> Route {
        Route {
            name: name.into(),
            when: serde_yaml_ng::from_str(when_yaml).unwrap(),
            to: to.into(),
        }
    }

    #[test]
    fn layer_zero_needs_no_rules_at_all() {
        // 「只配 provider」是最小可用配置，而且对不少人就够了：
        // 「官方为主，挂了走中转」零规则就能满足（§3.4 层 0）。
        let e = Engine::new(vec!["official".into(), "relay".into()], vec![], vec![]);
        let d = e.route(&facts("claude-sonnet-4-5")).unwrap();
        assert_eq!(d.candidates, ["official", "relay"], "按声明顺序故障转移");
        assert!(e.validate().is_ok());
    }

    #[test]
    fn no_providers_yields_no_synthesised_group() {
        // 零 provider 是首次运行的合法状态。合成一个空组会让配置校验
        // 失败，而那会让 core 起不来 —— 于是 UI 连「你还没配上游」都
        // 说不出口。
        let e = Engine::new(vec![], vec![], vec![]);
        assert!(e.groups().is_empty());
        assert!(e.routes().is_empty());
        assert!(e.validate().is_ok(), "零 provider 的配置必须合法");
    }

    #[test]
    fn layer_zero_is_not_a_special_case_in_the_code() {
        // 它被展开成「一个全量 fallback 组 + 一条兜底规则」，所以求值
        // 只有一条路径。这个测试盯着那个展开，而不是它的表面行为。
        let e = Engine::new(vec!["a".into()], vec![], vec![]);
        assert_eq!(e.groups().len(), 1);
        assert_eq!(e.routes().len(), 1);
        assert!(e.routes()[0].when.is_catch_all());
    }

    #[test]
    fn a_rule_can_point_straight_at_a_provider_without_a_group() {
        // 大多数分流需求到这一层就解决了，不必引入策略组这个概念。
        let e = Engine::new(
            vec!["official".into(), "relay".into()],
            vec![],
            vec![
                route("opus 走官方", "{ model: claude-opus-* }", "official"),
                route("兜底", "{}", "relay"),
            ],
        );
        let d = e.route(&facts("claude-opus-4-5")).unwrap();
        assert_eq!(d.candidates, ["official"]);
        assert_eq!(d.via_group, None);
        assert_eq!(d.matched_rule, "opus 走官方");
    }

    #[test]
    fn rules_are_tried_in_order_and_the_first_match_wins() {
        let e = Engine::new(
            vec!["a".into(), "b".into()],
            vec![],
            vec![
                route("先", "{}", "a"),
                route("后", "{ model: claude-* }", "b"),
            ],
        );
        assert_eq!(
            e.route(&facts("claude-opus-4-5")).unwrap().candidates,
            ["a"]
        );
    }

    #[test]
    fn no_catch_all_and_no_match_says_what_to_add() {
        // 「没有规则命中」是配置问题，而错误信息要说清下一步。
        let e = Engine::new(
            vec!["a".into()],
            vec![],
            vec![route("只管 opus", "{ model: claude-opus-* }", "a")],
        );
        let err = e.route(&facts("claude-sonnet-4-5")).unwrap_err();
        assert_eq!(err, RouteError::NoMatch);
        assert!(err.to_string().contains("兜底"));
    }

    #[test]
    fn a_group_expands_to_its_providers_in_order() {
        let g = Group {
            name: "pool".into(),
            kind: GroupType::Fallback,
            providers: vec!["official".into(), "relay".into()],
            session_affinity: false,
            selected: None,
        };
        let e = Engine::new(
            vec!["official".into(), "relay".into()],
            vec![g],
            vec![route("走池子", "{}", "pool")],
        );
        let d = e.route(&facts("x")).unwrap();
        assert_eq!(d.candidates, ["official", "relay"]);
        assert_eq!(d.via_group.as_deref(), Some("pool"));
    }

    #[test]
    fn select_puts_the_chosen_one_first_but_keeps_the_rest_for_failover() {
        // **手动选一家不等于放弃容错。**那家挂了照样该切，否则「select」
        // 就变成了「单点故障」。
        let g = Group {
            name: "pool".into(),
            kind: GroupType::Select,
            providers: vec!["a".into(), "b".into(), "c".into()],
            session_affinity: false,
            selected: Some("b".into()),
        };
        let e = Engine::new(
            vec!["a".into(), "b".into(), "c".into()],
            vec![g],
            vec![route("x", "{}", "pool")],
        );
        assert_eq!(e.route(&facts("x")).unwrap().candidates, ["b", "a", "c"]);
    }

    #[test]
    fn a_rule_pointing_nowhere_is_caught_at_load_time() {
        // 运行时的表现是「每个命中它的请求都失败」，而用户看不出是哪条
        // 规则的问题。
        let e = Engine::new(vec!["a".into()], vec![], vec![route("x", "{}", "typo")]);
        let err = e.validate().unwrap_err();
        assert!(matches!(err, RouteError::UnknownTarget { .. }));
        assert!(err.to_string().contains("typo"));
    }

    #[test]
    fn a_broken_comparison_in_a_rule_fails_validation() {
        let e = Engine::new(
            vec!["a".into()],
            vec![],
            vec![route("x", r#"{ input_tokens: "200k" }"#, "a")],
        );
        assert!(e.validate().is_err());
    }

    #[test]
    fn an_empty_group_is_caught() {
        let g = Group {
            name: "empty".into(),
            kind: GroupType::Fallback,
            providers: vec![],
            session_affinity: false,
            selected: None,
        };
        let e = Engine::new(vec!["a".into()], vec![g], vec![route("x", "{}", "empty")]);
        assert!(matches!(e.validate(), Err(RouteError::EmptyGroup(_))));
    }

    #[test]
    fn only_load_balance_is_flagged_as_bad_for_cache() {
        // 这张表要在 UI 上直接显示，因为它决定了用户的账单（§3.4）。
        assert!(!GroupType::Fallback.hurts_cache());
        assert!(!GroupType::Select.hurts_cache());
        assert!(GroupType::LoadBalance.hurts_cache());
        // 而默认必须是不伤缓存的那个
        assert_eq!(GroupType::default(), GroupType::Fallback);
    }

    #[test]
    fn the_decision_says_which_rule_matched() {
        // 「命中第 4 条」远不如「命中『带缓存的必须走官方』」有用。
        let e = Engine::new(
            vec!["official".into()],
            vec![],
            vec![route("带缓存的必须走官方", "{ cache: true }", "official")],
        );
        let mut f = facts("x");
        f.cache = true;
        assert_eq!(e.route(&f).unwrap().matched_rule, "带缓存的必须走官方");
    }
}
