//! 路由与策略组：按资源增删改，以及更换默认路由。
//!
//! # 为什么是资源而不是补丁
//!
//! 和上游、代理同一个理由（见 `resources.rs`），另加两条路由特有的：
//!
//! 1. **规则的顺序就是语义。**通用补丁只有追加和删除，界面「添加规则」只能
//!    把新规则追加到末尾，也就是兜底规则之后 —— 那条规则的转发永远不会
//!    执行，而配置完全合法。整条路由一次交过来，顺序由界面定。
//! 2. **路由被密钥按名字引用。**改名、删除都要连带改密钥，而删除时密钥改用
//!    哪条路由，是用户要做的决定，不是这里替他做的。
//!
//! 一次保存就是一个版本。

use std::collections::HashSet;

use axum::Json;
use axum::extract::{Path, Query, State};
use serde_yaml_ng::Value;
use tw_config::edit::{self, EditError};
use tw_config::history::Origin;
use tw_config::refs;
use tw_engine::rule::{OneOrMany, When};
use tw_engine::{BalanceBy, Group, GroupType, RouteSet, Rule, SetAction};
use tw_types::{Msg, msg};
use tw_yaml::Step;

use crate::contract::RouterExt;
use crate::resources::{checked_name, invalid, mapping};
use crate::{ApplyError, ControlState, Fail, apply_fail};
use tw_api::ep;

pub fn router() -> axum::Router<ControlState> {
    axum::Router::new()
        .at(ep::CreateRoute, create_route)
        .at(ep::UpdateRoute, update_route)
        .at(ep::DeleteRoute, delete_route)
        .at(ep::SetDefaultRoute, set_default_route)
        .at(ep::CreateGroup, create_group)
        .at(ep::UpdateGroup, update_group)
        .at(ep::DeleteGroup, delete_group)
        .at(ep::KnownModels, known_models)
}

// ─────────────────────────────────────────────────────────── 路由

async fn create_route(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::RouteSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let set = to_route(&req.route, cfg).map_err(invalid)?;
            // 合成的默认路由不在配置里，但它的名字已经被占了
            if cfg.engine().routes().iter().any(|r| r.name == set.name) {
                return Err(name_taken("route", &set.name));
            }
            let out = edit::upsert(text, edit::ROUTES, None, &mapping(&set)?)?;
            match &req.keys {
                Some(keys) => assign_keys(&out, cfg, None, &set.name, keys),
                None => Ok(out),
            }
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn update_route(
    State(s): State<ControlState>,
    Path(name): Path<String>,
    Json(req): Json<tw_api::RouteSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let engine = cfg.engine();
            let set = to_route(&req.route, cfg).map_err(invalid)?;
            if set.name != name && engine.routes().iter().any(|r| r.name == set.name) {
                return Err(name_taken("route", &set.name));
            }
            let current = if cfg.routes.iter().any(|r| r.name == name) {
                Some(name.as_str())
            } else if engine.is_builtin_route(&name) {
                // 合成的默认路由：**保存即写进配置**，从此它就是一条普通的路由
                None
            } else {
                return Err(not_found("route", &name));
            };
            let mut out = edit::upsert(text, edit::ROUTES, current, &mapping(&set)?)?;
            if set.name != name {
                // **和路由本身在同一个版本里改** —— 分两次写的话，中间那一版
                // 的密钥指向一条不存在的路由，会被校验拒掉
                out = refs::rename_route(&out, cfg, &name, &set.name)?;
            }
            match &req.keys {
                Some(keys) => assign_keys(&out, cfg, Some(&name), &set.name, keys),
                None => Ok(out),
            }
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn delete_route(
    State(s): State<ControlState>,
    Path(name): Path<String>,
    Query(q): Query<tw_api::RouteDelete>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(q.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let engine = cfg.engine();
            if engine.default_route() == name {
                return Err(ApplyError::InUse(msg!(
                    "control.default_route_cannot_delete", route = name =>
                    "Route `{route}` is the default route and cannot be deleted. Make another route \
                     the default first."
                )));
            }
            if !cfg.routes.iter().any(|r| r.name == name) {
                return Err(not_found("route", &name));
            }
            // 改用默认路由 = 不指定路由：默认路由换了，它们跟着换
            let target = q
                .reassign_to
                .as_deref()
                .filter(|t| !t.is_empty() && *t != engine.default_route());
            if let Some(t) = target {
                if t == name {
                    return Err(invalid(msg!(
                        "control.reassign_to_deleted_route", route = name =>
                        "a key cannot be moved onto route `{route}`, which is being deleted"
                    )));
                }
                if engine.rules_of(t).is_none() {
                    return Err(not_found("route", t));
                }
            }
            let mut out = text.to_string();
            for (i, c) in cfg.clients.iter().enumerate() {
                if c.route.as_deref() == Some(name.as_str()) {
                    let value = target.map(|t| Value::String(t.to_string()));
                    out = edit::set(&out, &route_of(i), value.as_ref())?;
                }
            }
            Ok(edit::remove(&out, edit::ROUTES, &name)?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn set_default_route(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::DefaultRouteSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let engine = cfg.engine();
            if engine.rules_of(&req.name).is_none() {
                return Err(not_found("route", &req.name));
            }
            let mut out = text.to_string();
            let current = engine.default_route();
            // 合成的默认路由不在配置里。**换掉之前先写进去**，它才留得下来，
            // 继续作为一条普通路由供密钥指定
            if current != req.name && engine.is_builtin_route(current) {
                let set = RouteSet {
                    name: current.to_string(),
                    rules: engine.rules_of(current).unwrap_or_default().to_vec(),
                };
                out = edit::upsert(&out, edit::ROUTES, None, &mapping(&set)?)?;
            }
            // 默认值不写进文件
            let value =
                (req.name != tw_engine::DEFAULT_ROUTE).then(|| Value::String(req.name.clone()));
            Ok(edit::set(
                &out,
                &[Step::key("default_route")],
                value.as_ref(),
            )?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

/// 让恰好 `keys` 这几把密钥使用路由 `name`。
///
/// `cfg` 是写之前的那一份（下标对得上）；`old` 是改名前的名字 —— 改名的
/// 引用已经跟着改过，所以原来用 `old` 的密钥这时候用的是 `name`。**移出去
/// 的密钥改用默认路由**，不指定任何一条。
fn assign_keys(
    text: &str,
    cfg: &tw_config::Config,
    old: Option<&str>,
    name: &str,
    keys: &[String],
) -> Result<String, ApplyError> {
    if let Some(missing) = keys
        .iter()
        .find(|k| !cfg.clients.iter().any(|c| &c.name == *k))
    {
        return Err(not_found("gateway key", missing));
    }
    let mut out = text.to_string();
    for (i, c) in cfg.clients.iter().enumerate() {
        let current = match c.route.as_deref() {
            Some(r) if Some(r) == old => Some(name),
            other => other,
        };
        match (keys.contains(&c.name), current == Some(name)) {
            (true, false) => {
                out = edit::set(&out, &route_of(i), Some(&Value::String(name.to_string())))?;
            }
            (false, true) => out = edit::set(&out, &route_of(i), None)?,
            _ => {}
        }
    }
    Ok(out)
}

fn route_of(client: usize) -> [Step; 3] {
    [
        Step::key("clients"),
        Step::Index(client),
        Step::key("route"),
    ]
}

fn to_route(input: &tw_api::RouteInput, cfg: &tw_config::Config) -> Result<RouteSet, Msg> {
    let name = checked_name(&input.name, "route")?;
    reserved(&name, "route")?;
    let mut seen = HashSet::new();
    let rules = input
        .rules
        .iter()
        .map(|r| {
            let rule = to_rule(r, cfg)?;
            if !seen.insert(rule.name.clone()) {
                return Err(msg!(
                    "control.route.duplicate_rule", rule = rule.name =>
                    "the route has two rules named `{rule}`"
                ));
            }
            Ok(rule)
        })
        .collect::<Result<Vec<_>, Msg>>()?;
    Ok(RouteSet { name, rules })
}

/// 界面交过来的一条规则 → 配置里的规则。**写法错在哪儿，这里就说哪儿。**
pub(crate) fn to_rule(input: &tw_api::RuleInput, cfg: &tw_config::Config) -> Result<Rule, Msg> {
    let name = checked_name(&input.name, "rule")?;
    let when = when_from(&name, &input.conditions, cfg)?;
    let to = input.to.as_ref().and_then(target_from);
    let deny = match input.deny.as_deref().map(str::trim) {
        Some("") => {
            return Err(msg!(
                "control.rule.deny_needs_reason", rule = name =>
                "rule `{rule}`: a denial needs a reason"
            ));
        }
        other => other.map(str::to_string),
    };
    if to.is_some() && deny.is_some() {
        return Err(msg!(
            "control.rule.forward_and_deny", rule = name =>
            "rule `{rule}` cannot both forward and deny"
        ));
    }
    let set = input.set.as_ref().and_then(|s| {
        let a = SetAction {
            model: s
                .model
                .as_deref()
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(str::to_string),
            max_tokens: s.max_tokens,
            thinking: s.thinking,
            only_at_session_start: false,
        };
        (!a.is_empty()).then_some(a)
    });
    Ok(Rule {
        name,
        when,
        to,
        set,
        deny,
    })
}

/// 界面交过来的去向 → 配置里的 `to`。名字是空的就是没有去向；**指定模型的列表原样
/// 交给引擎校验**，空列表、不存在的上游、空的模型名都由它说出是哪条规则的哪一项。
fn target_from(t: &tw_api::RuleTarget) -> Option<tw_engine::Target> {
    match t {
        tw_api::RuleTarget::Name(n) => {
            let n = n.trim();
            (!n.is_empty()).then(|| tw_engine::Target::from(n))
        }
        tw_api::RuleTarget::Models(list) => Some(tw_engine::Target::Models(
            list.iter()
                .map(|p| tw_engine::Pinned {
                    provider: p.provider.trim().to_string(),
                    model: p.model.trim().to_string(),
                })
                .collect(),
        )),
    }
}

/// 配置里的 `to` → 视图。和 [`target_from`] 互逆。
fn target_view(t: &tw_engine::Target) -> tw_api::RuleTarget {
    match t {
        tw_engine::Target::Name(n) => tw_api::RuleTarget::Name(n.clone()),
        tw_engine::Target::Models(list) => tw_api::RuleTarget::Models(
            list.iter()
                .map(|p| tw_api::PinnedModel {
                    provider: p.provider.clone(),
                    model: p.model.clone(),
                })
                .collect(),
        ),
    }
}

/// 客户端格式，和请求路径判出来的那一套一致。
const DIALECTS: &[&str] = &["anthropic", "openai-chat", "openai-responses", "gemini"];
/// 辅助请求的类别。`assistant_internal` 是五类的总称。
const INTENTS: &[&str] = &[
    "assistant_internal",
    "health_check",
    "warmup",
    "titling",
    "topic_detect",
    "suggestion",
];

/// 条件列表 → `when`。
///
/// **和视图用同一套写法**（`ConditionView`），界面拿到什么交回什么。取值
/// 在这里查：打错的客户端格式、不存在的密钥或上游，写进去的后果都是这条
/// 规则永远不命中，而那是完全静默的。
///
/// **每一句都带着规则名**（`rule` 参数）：一条路由里有好几条规则，只说
/// 「条件没填值」的话，用户得一条条去找。
fn when_from(
    rule: &str,
    conds: &[tw_api::ConditionView],
    cfg: &tw_config::Config,
) -> Result<When, Msg> {
    let mut w = When::default();
    let mut seen = HashSet::new();
    for c in conds {
        let field = c.field.slug();
        if !seen.insert(c.field) {
            return Err(msg!(
                "control.rule.condition_twice", rule = rule, field = field =>
                "rule `{rule}`: condition `{field}` appears twice"
            ));
        }
        let vals: Vec<String> = c
            .values
            .iter()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect();
        let no_value = || {
            msg!(
                "control.rule.condition_no_value", rule = rule, field = field =>
                "rule `{rule}`: condition `{field}` has no value"
            )
        };
        let one = || -> Result<String, Msg> {
            match vals.as_slice() {
                [v] => Ok(v.clone()),
                [] => Err(no_value()),
                _ => Err(msg!(
                    "control.rule.condition_one_value", rule = rule, field = field =>
                    "rule `{rule}`: condition `{field}` takes exactly one value"
                )),
            }
        };
        let flag = || -> Result<bool, Msg> {
            match one()?.as_str() {
                "true" => Ok(true),
                "false" => Ok(false),
                v => Err(msg!(
                    "control.rule.condition_not_bool", rule = rule, field = field, value = v =>
                    "rule `{rule}`: condition `{field}` takes true or false, and got `{value}`"
                )),
            }
        };
        let many = |allowed: &dyn Fn(&str) -> bool,
                    unknown: &dyn Fn(&str) -> Msg|
         -> Result<OneOrMany, Msg> {
            if let Some(bad) = vals.iter().find(|v| !allowed(v.as_str())) {
                return Err(unknown(bad));
            }
            match vals.as_slice() {
                [] => Err(no_value()),
                [v] => Ok(OneOrMany::One(v.clone())),
                vs => Ok(OneOrMany::Many(vs.to_vec())),
            }
        };
        use tw_api::ConditionField as F;
        match c.field {
            F::Model => w.model = Some(one()?),
            F::Client => {
                let k = one()?;
                if !cfg.clients.iter().any(|c| c.name == k) {
                    return Err(msg!(
                        "control.rule.no_such_key", rule = rule, key = k =>
                        "rule `{rule}`: there is no gateway key `{key}`"
                    ));
                }
                w.client = Some(k);
            }
            F::Dialect => {
                let d = one()?;
                if !DIALECTS.contains(&d.as_str()) {
                    return Err(msg!(
                        "control.rule.unknown_dialect", rule = rule, dialect = d =>
                        "rule `{rule}`: `{dialect}` is not a client format we support"
                    ));
                }
                w.dialect = Some(d);
            }
            F::InputTokens => w.input_tokens = Some(one()?),
            F::MaxTokens => w.max_tokens = Some(one()?),
            F::ToolCount => w.tool_count = Some(one()?),
            F::Cache => w.cache = Some(flag()?),
            F::Tools => w.tools = Some(flag()?),
            F::Image => w.image = Some(flag()?),
            F::Thinking => w.thinking = Some(flag()?),
            F::Stream => w.stream = Some(flag()?),
            F::Intent => {
                w.intent = Some(many(&|v| INTENTS.contains(&v), &|v| {
                    msg!(
                        "control.rule.no_such_probe_class", rule = rule, class = v =>
                        "rule `{rule}`: there is no auxiliary-request class `{class}`"
                    )
                })?)
            }
            F::ProviderWouldBe => {
                w.provider_would_be = Some(many(
                    &|v| cfg.providers.iter().any(|p| p.name == v),
                    &|v| {
                        msg!(
                            "control.rule.no_such_upstream", rule = rule, upstream = v =>
                            "rule `{rule}`: there is no upstream `{upstream}`"
                        )
                    },
                )?)
            }
        }
    }
    // 比较式写错了（`200k` 少了比较符）：保存之前就说
    //
    // 那句话是引擎的（`engine.compare.*`），不认识规则名。**码不变，多带一个
    // `rule`**，英文前面补上是哪条规则 —— 为此再给每一种写法错误造一个「带
    // 规则名」的码，码表就翻了一倍
    w.validate().map_err(|e| {
        let mut m = e.msg();
        m.text = format!("rule `{rule}`: {}", m.text);
        m.args.insert("rule".into(), rule.to_string());
        m
    })?;
    Ok(w)
}

// ─────────────────────────────────────────────────────────── 策略组

async fn create_group(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::GroupSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let g = to_group(&req.group, cfg).map_err(invalid)?;
            Ok(edit::upsert(text, edit::GROUPS, None, &mapping(&g)?)?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn update_group(
    State(s): State<ControlState>,
    Path(name): Path<String>,
    Json(req): Json<tw_api::GroupSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, cfg| {
            builtin_group(&name)?;
            let g = to_group(&req.group, cfg).map_err(invalid)?;
            let mut out = edit::upsert(text, edit::GROUPS, Some(&name), &mapping(&g)?)?;
            if g.name != name {
                out = refs::rename_group(&out, cfg, &name, &g.name)?;
            }
            Ok(out)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn delete_group(
    State(s): State<ControlState>,
    Path(name): Path<String>,
    Query(q): Query<tw_api::BaseVersion>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(q.base_version.as_deref(), Origin::Ui, |text, cfg| {
            builtin_group(&name)?;
            let used = refs::group_refs(cfg, &name);
            if !used.is_empty() {
                let by = used
                    .iter()
                    .map(|r| format!("rule `{}` of route `{}`", r.rule, r.route))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(ApplyError::InUse(msg!(
                    "control.group_in_use", group = name, refs = by =>
                    "Group `{group}` is still referenced by {refs}; drop those references before \
                     deleting it."
                )));
            }
            Ok(edit::remove(text, edit::GROUPS, &name)?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

fn builtin_group(name: &str) -> Result<(), ApplyError> {
    if tw_engine::is_builtin_group(name) {
        return Err(ApplyError::InUse(msg!(
            "control.builtin_group_fixed" =>
            "The built-in group of every upstream follows the upstream list; it cannot be edited \
             or deleted."
        )));
    }
    Ok(())
}

/// 契约里的策略组类型和引擎里的是同一组词，两边各写一遍，靠这两个穷尽的
/// `match` 对齐。
pub(crate) fn group_kind(t: GroupType) -> tw_api::GroupKind {
    match t {
        GroupType::Fallback => tw_api::GroupKind::Fallback,
        GroupType::Select => tw_api::GroupKind::Select,
        GroupType::LoadBalance => tw_api::GroupKind::LoadBalance,
        GroupType::UrlTest => tw_api::GroupKind::UrlTest,
        GroupType::Cheapest => tw_api::GroupKind::Cheapest,
    }
}

fn group_type(k: tw_api::GroupKind) -> GroupType {
    match k {
        tw_api::GroupKind::Fallback => GroupType::Fallback,
        tw_api::GroupKind::Select => GroupType::Select,
        tw_api::GroupKind::LoadBalance => GroupType::LoadBalance,
        tw_api::GroupKind::UrlTest => GroupType::UrlTest,
        tw_api::GroupKind::Cheapest => GroupType::Cheapest,
    }
}

/// 界面看到的权重：`load-balance` 组每个成员都在（没写的是 1），别的类型是空的
pub(crate) fn group_weights(g: &Group) -> std::collections::BTreeMap<String, u32> {
    match g.kind {
        GroupType::LoadBalance => g
            .providers
            .iter()
            .map(|m| (m.name.clone(), m.weight))
            .collect(),
        _ => Default::default(),
    }
}

/// `balance_by` 的两份词表，和 [`group_kind`] 同一个理由。
pub(crate) fn balance_by_view(b: BalanceBy) -> tw_api::BalanceBy {
    match b {
        BalanceBy::Weights => tw_api::BalanceBy::Weights,
        BalanceBy::Latency => tw_api::BalanceBy::Latency,
        BalanceBy::Health => tw_api::BalanceBy::Health,
        BalanceBy::LatencyHealth => tw_api::BalanceBy::LatencyHealth,
    }
}

fn balance_by_of(b: tw_api::BalanceBy) -> BalanceBy {
    match b {
        tw_api::BalanceBy::Weights => BalanceBy::Weights,
        tw_api::BalanceBy::Latency => BalanceBy::Latency,
        tw_api::BalanceBy::Health => BalanceBy::Health,
        tw_api::BalanceBy::LatencyHealth => BalanceBy::LatencyHealth,
    }
}

fn to_group(input: &tw_api::GroupInput, cfg: &tw_config::Config) -> Result<Group, Msg> {
    let name = checked_name(&input.name, "group")?;
    reserved(&name, "group")?;
    if cfg.providers.iter().any(|p| p.name == name) {
        return Err(msg!(
            "control.group.name_is_upstream", name = name =>
            "`{name}` is already the name of an upstream. A rule points at an upstream or a group \
             by name, so the two cannot share one."
        ));
    }
    let kind = group_type(input.kind);
    let mut providers = Vec::new();
    for p in &input.providers {
        let p = p.trim();
        if !cfg.providers.iter().any(|x| x.name == p) {
            return Err(msg!(
                "control.group.no_such_upstream", upstream = p =>
                "there is no upstream `{upstream}`"
            ));
        }
        if providers.iter().any(|x| x == p) {
            return Err(msg!(
                "control.group.upstream_twice", upstream = p =>
                "upstream `{upstream}` appears twice"
            ));
        }
        providers.push(p.to_string());
    }
    if providers.is_empty() {
        return Err(msg!(
            "control.group.empty" =>
            "a group needs at least one upstream"
        ));
    }
    // 权重：只有 `load-balance` 用，1 到 100，没给的成员是 1。和引擎的校验同样的规矩
    // （`engine.group_weight_*`），这里先说，说的是界面上那一格
    let mut weights = std::collections::HashMap::new();
    for (p, w) in input.weights.iter().flatten() {
        let p = p.trim();
        if !providers.iter().any(|x| x == p) {
            return Err(msg!(
                "control.group.weight_not_member", upstream = p =>
                "a weight is given for `{upstream}`, which is not a member"
            ));
        }
        if *w != 1 && kind != GroupType::LoadBalance {
            return Err(msg!(
                "control.group.weight_not_load_balance", upstream = p, weight = w =>
                "upstream `{upstream}` has a weight of {weight}; only a load-balance group uses \
                 weights"
            ));
        }
        if !(tw_engine::weighted::WEIGHT_MIN..=tw_engine::weighted::WEIGHT_MAX).contains(w) {
            return Err(msg!(
                "control.group.weight_out_of_range", upstream = p, weight = w =>
                "upstream `{upstream}` has a weight of {weight}; a weight is a whole number from \
                 1 to 100"
            ));
        }
        weights.insert(p.to_string(), *w);
    }
    // 手动选择要有一个优先使用的成员；没选就是第一个。其余策略不写这一项
    let selected = match kind {
        GroupType::Select => {
            let sel = input
                .selected
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(&providers[0])
                .to_string();
            if !providers.contains(&sel) {
                return Err(msg!(
                    "control.group.preferred_not_member", upstream = sel =>
                    "the preferred upstream `{upstream}` is not a member"
                ));
            }
            Some(sel)
        }
        _ => None,
    };
    // 按快慢、成败分只有负载均衡用得上：别的类型写了也不起作用（引擎校验也拦，
    // `engine.group_balance_not_load_balance`）
    let balance_by = input.balance_by.map(balance_by_of).unwrap_or_default();
    if kind != GroupType::LoadBalance && !balance_by.is_weights() {
        return Err(msg!(
            "control.group.balance_not_load_balance", balance_by = balance_by.slug() =>
            "balance_by `{balance_by}` applies only to a load-balance group"
        ));
    }
    Ok(Group {
        name,
        kind,
        providers: providers
            .into_iter()
            .map(|p| tw_engine::Member {
                weight: weights.get(&p).copied().unwrap_or(1),
                name: p,
            })
            .collect(),
        selected,
        balance_by,
    })
}

// ─────────────────────────────────────────────────────────── 已知模型

/// 网关知道的全部模型，以及能提供它们的上游。
///
/// **和 `/v1/models` 同一份目录**：停用的上游、启用范围外的模型都不在里面。另加
/// 别名表里的每个别名，见 [`known`]。规则条件和试算的模型建议用它。
async fn known_models(State(s): State<ControlState>) -> Json<Vec<tw_api::KnownModel>> {
    Json(known(&s.config(), &s.gateway.catalog.load()))
}

/// 目录里的真模型加上别名表里的每个别名，按名称排。
///
/// - 真模型：提供它的上游照目录，带着列出它的别名。和某个别名同名的真模型不单列 ——
///   请求这个名称按别名处理（别名优先）。
/// - 别名：**每一个都列出来**，哪家都服务不了的也列：它是配置里写下的名称，规则条件
///   要能选到它。上游是有清单、`resolve` 在这家有结果的那几家 —— 和真模型一样只算
///   有清单的；没有清单的上游照样收它的请求，只是不知道它有什么。
fn known(cfg: &tw_config::Config, catalog: &tw_engine::Catalog) -> Vec<tw_api::KnownModel> {
    let mut out: Vec<tw_api::KnownModel> = catalog
        .all()
        .into_iter()
        .filter(|id| !cfg.aliases.contains(id))
        .map(|id| tw_api::KnownModel {
            id: id.to_string(),
            providers: catalog.providers_for(id).to_vec(),
            alias: None,
            aliases: aliases_listing(cfg, id),
        })
        .collect();
    out.extend(cfg.aliases.iter().map(|a| tw_api::KnownModel {
        id: a.name.clone(),
        providers: serving_alias(cfg, catalog, &a.name),
        alias: Some(a.models.clone()),
        aliases: Vec::new(),
    }));
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// 能服务别名 `name` 的上游，按配置里的顺序：没停用、有清单，`resolve` 在这家取得到
/// 名称（清单里有，也在启用范围里）。
fn serving_alias(cfg: &tw_config::Config, catalog: &tw_engine::Catalog, name: &str) -> Vec<String> {
    cfg.providers
        .iter()
        .filter(|p| !p.disabled)
        .filter(|p| {
            tw_gateway::models::resolve(cfg, catalog, p, name)
                .is_some_and(|m| catalog.offers(&p.name, &m) == Some(true))
        })
        .map(|p| p.name.clone())
        .collect()
}

/// 列出了模型名 `model` 的别名，按别名表的顺序。
pub(crate) fn aliases_listing(cfg: &tw_config::Config, model: &str) -> Vec<String> {
    cfg.aliases
        .iter()
        .filter(|a| a.models.iter().any(|m| m == model))
        .map(|a| a.name.clone())
        .collect()
}

// ─────────────────────────────────────────────────────────── 视图

/// 概览里的一条规则。**全文**：编辑对话框靠它回填。
pub(crate) fn rule_view(r: &Rule, n: tw_engine::RuleNotes) -> tw_api::RuleView {
    tw_api::RuleView {
        name: r.name.clone(),
        conditions: crate::describe_when(&r.when),
        to: r.to.as_ref().map(target_view),
        deny: r.deny.clone(),
        set: r
            .set
            .as_ref()
            .filter(|s| !s.is_empty())
            .map(|s| tw_api::RuleRewrite {
                model: s.model.clone(),
                max_tokens: s.max_tokens,
                thinking: s.thinking,
            }),
        catch_all: n.catch_all,
        phase_two: n.phase_two,
        shadowed: n.shadowed,
    }
}

// ─────────────────────────────────────────────────────────── 共用

fn reserved(name: &str, what: &str) -> Result<(), Msg> {
    if name.starts_with(tw_engine::RESERVED_PREFIX) {
        return Err(msg!(
            "control.name_reserved", kind = what, prefix = tw_engine::RESERVED_PREFIX =>
            "a {kind} name cannot start with {prefix}; those are reserved for built-ins"
        ));
    }
    Ok(())
}

fn not_found(what: &'static str, name: &str) -> ApplyError {
    ApplyError::Edit(EditError::NotFound {
        what,
        name: name.to_string(),
    })
}

fn name_taken(what: &'static str, name: &str) -> ApplyError {
    ApplyError::Edit(EditError::NameTaken {
        what,
        name: name.to_string(),
    })
}

#[cfg(test)]
mod targets {
    use super::*;

    fn cfg() -> tw_config::Config {
        tw_config::try_parse(
            "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: c\n    key: tw-k\nproviders:\n  - name: a\n    base_url: https://x\n    key: k\n  - name: b\n    base_url: https://y\n    key: k\n",
        )
        .unwrap()
    }

    fn input(to: Option<tw_api::RuleTarget>) -> tw_api::RuleInput {
        tw_api::RuleInput {
            name: "r".into(),
            conditions: vec![],
            to,
            deny: None,
            set: None,
        }
    }

    fn pin(provider: &str, model: &str) -> tw_api::PinnedModel {
        tw_api::PinnedModel {
            provider: provider.into(),
            model: model.into(),
        }
    }

    /// 指定模型交进来、写进配置、再读成视图，还是那个列表、那个顺序。以前视图把它说成
    /// 没有去向，界面原样交回来就把它丢了
    #[test]
    fn a_pinned_target_comes_back_from_the_view_as_it_went_in() {
        let c = cfg();
        let to = tw_api::RuleTarget::Models(vec![pin("b", "m-2"), pin("a", " m-1 ")]);
        let rule = to_rule(&input(Some(to)), &c).unwrap();
        assert_eq!(
            rule.to,
            Some(tw_engine::Target::Models(vec![
                tw_engine::Pinned {
                    provider: "b".into(),
                    model: "m-2".into(),
                },
                tw_engine::Pinned {
                    provider: "a".into(),
                    model: "m-1".into(),
                },
            ]))
        );
        let view = rule_view(&rule, Default::default());
        assert_eq!(
            view.to,
            Some(tw_api::RuleTarget::Models(vec![
                pin("b", "m-2"),
                pin("a", "m-1")
            ]))
        );
        let again = tw_api::RuleInput {
            name: view.name.clone(),
            conditions: view.conditions.clone(),
            to: view.to.clone(),
            deny: view.deny.clone(),
            set: view.set.clone(),
        };
        assert_eq!(to_rule(&again, &c).unwrap().to, rule.to);
    }

    #[test]
    fn a_named_target_is_trimmed_and_a_blank_one_is_none() {
        let c = cfg();
        let rule = to_rule(&input(Some(" a ".into())), &c).unwrap();
        assert_eq!(rule.to, Some("a".into()));
        assert_eq!(
            rule_view(&rule, Default::default()).to,
            Some(tw_api::RuleTarget::from("a"))
        );
        let mut blank = input(Some("  ".into()));
        blank.deny = Some("不行".into());
        assert_eq!(to_rule(&blank, &c).unwrap().to, None);
    }

    /// 空列表不悄悄当成没有去向：交给引擎，它说出是哪条规则
    #[test]
    fn an_empty_pinned_list_is_refused_by_the_engine() {
        let c = cfg();
        let rule = to_rule(&input(Some(tw_api::RuleTarget::Models(vec![]))), &c).unwrap();
        let err = c.engine().check_rules(&[rule]).unwrap_err().msg();
        assert_eq!(err.code, "engine.pinned_empty");
        assert_eq!(err.arg("rule"), "r");
    }
}

#[cfg(test)]
mod msg_codes {
    use super::*;

    fn cfg() -> tw_config::Config {
        tw_config::try_parse(
            "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: c\n    key: tw-k\nproviders:\n  - name: a\n    base_url: https://x\n    key: k\n",
        )
        .unwrap()
    }

    fn rule(conds: &[(&str, &[&str])]) -> tw_api::RuleInput {
        tw_api::RuleInput {
            name: "长上下文".into(),
            conditions: conds
                .iter()
                .map(|(f, vs)| tw_api::ConditionView {
                    field: tw_api::ConditionField::from_slug(f).unwrap(),
                    values: vs.iter().map(|v| v.to_string()).collect(),
                })
                .collect(),
            to: Some("a".into()),
            deny: None,
            set: None,
        }
    }

    /// **每一句都有自己的码，而且带着规则名。**以前这些是拼好的英文，
    /// 经 `invalid` 塞进 `control.config_rejected` 的 `{detail}`
    #[test]
    fn a_rule_that_is_written_wrongly_says_so_with_its_own_code() {
        let c = cfg();
        let code = |r: tw_api::RuleInput| {
            let m = to_rule(&r, &c).unwrap_err();
            assert_eq!(m.arg("rule"), "长上下文", "{m:?}");
            assert!(!m.text.is_empty(), "{m:?}");
            m.code
        };
        assert_eq!(
            code(rule(&[("model", &[])])),
            "control.rule.condition_no_value"
        );
        assert_eq!(
            code(rule(&[("model", &["a", "b"])])),
            "control.rule.condition_one_value"
        );
        assert_eq!(
            code(rule(&[("model", &["a"]), ("model", &["b"])])),
            "control.rule.condition_twice"
        );
        assert_eq!(
            code(rule(&[("cache", &["yes"])])),
            "control.rule.condition_not_bool"
        );
        assert_eq!(
            code(rule(&[("client", &["nobody"])])),
            "control.rule.no_such_key"
        );
        assert_eq!(
            code(rule(&[("dialect", &["cobol"])])),
            "control.rule.unknown_dialect"
        );
        assert_eq!(
            code(rule(&[("intent", &["chat"])])),
            "control.rule.no_such_probe_class"
        );
        assert_eq!(
            code(rule(&[("provider_would_be", &["b"])])),
            "control.rule.no_such_upstream"
        );
        // 比较式写错是引擎那一句，**码不变，多带一个规则名**
        assert_eq!(
            code(rule(&[("input_tokens", &["200k"])])),
            "engine.compare.no_operator"
        );
        let mut r = rule(&[]);
        r.deny = Some("no".into());
        assert_eq!(code(r), "control.rule.forward_and_deny");
    }

    #[test]
    fn a_group_that_is_written_wrongly_says_so_with_its_own_code() {
        let c = cfg();
        let g = |name: &str, kind: &str, providers: &[&str]| tw_api::GroupInput {
            name: name.into(),
            kind: tw_api::GroupKind::from_slug(kind).unwrap(),
            providers: providers.iter().map(|p| p.to_string()).collect(),
            selected: None,
            weights: None,
            balance_by: None,
        };
        let code = |i: tw_api::GroupInput| to_group(&i, &c).unwrap_err().code;
        assert_eq!(
            code(g("a", "fallback", &["a"])),
            "control.group.name_is_upstream"
        );
        assert_eq!(code(g("__x", "fallback", &["a"])), "control.name_reserved");
        assert_eq!(
            code(g("g", "fallback", &["b"])),
            "control.group.no_such_upstream"
        );
        assert_eq!(
            code(g("g", "fallback", &["a", "a"])),
            "control.group.upstream_twice"
        );
        assert_eq!(code(g("g", "fallback", &[])), "control.group.empty");
        // 按快慢、成败分只给负载均衡；写明默认值的照收
        for kind in ["fallback", "select", "url-test", "cheapest"] {
            let mut i = g("g", kind, &["a"]);
            i.balance_by = Some(tw_api::BalanceBy::Latency);
            assert_eq!(code(i), "control.group.balance_not_load_balance", "{kind}");
            let mut i = g("g", kind, &["a"]);
            i.balance_by = Some(tw_api::BalanceBy::Weights);
            assert!(to_group(&i, &c).is_ok(), "{kind}");
        }
        let mut i = g("g", "load-balance", &["a"]);
        i.balance_by = Some(tw_api::BalanceBy::LatencyHealth);
        assert_eq!(
            to_group(&i, &c).unwrap().balance_by,
            BalanceBy::LatencyHealth
        );
        let weighted = |kind: &str, w: &[(&str, u32)]| tw_api::GroupInput {
            weights: Some(w.iter().map(|(p, w)| (p.to_string(), *w)).collect()),
            ..g("g", kind, &["a"])
        };
        assert_eq!(
            code(weighted("load-balance", &[("b", 2)])),
            "control.group.weight_not_member"
        );
        assert_eq!(
            code(weighted("fallback", &[("a", 2)])),
            "control.group.weight_not_load_balance"
        );
        for w in [0, 101] {
            assert_eq!(
                code(weighted("load-balance", &[("a", w)])),
                "control.group.weight_out_of_range"
            );
        }
    }

    /// 权重写进成员：`load-balance` 给了的照给的，没给的是 1；别的类型给 1 等于没给
    #[test]
    fn weights_become_the_members_weights() {
        let mut c = cfg();
        c.providers.push(tw_config::Provider {
            name: "b".into(),
            ..c.providers[0].clone()
        });
        let input = |kind: &str, w: Option<&[(&str, u32)]>| tw_api::GroupInput {
            name: "g".into(),
            kind: tw_api::GroupKind::from_slug(kind).unwrap(),
            providers: vec!["a".into(), "b".into()],
            selected: None,
            weights: w.map(|w| w.iter().map(|(p, w)| (p.to_string(), *w)).collect()),
            balance_by: None,
        };
        let g = to_group(&input("load-balance", Some(&[("b", 7)])), &c).unwrap();
        assert_eq!((g.weight("a"), g.weight("b")), (1, 7));
        assert_eq!(
            group_weights(&g),
            [("a".to_string(), 1), ("b".to_string(), 7)]
                .into_iter()
                .collect()
        );
        let g = to_group(&input("load-balance", None), &c).unwrap();
        assert!(g.providers.iter().all(|m| m.weight == 1));
        let g = to_group(&input("fallback", Some(&[("a", 1)])), &c).unwrap();
        assert!(g.providers.iter().all(|m| m.weight == 1));
        assert!(group_weights(&g).is_empty(), "别的类型不给权重");
    }
}
