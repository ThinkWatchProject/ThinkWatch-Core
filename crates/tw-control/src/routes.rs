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
use axum::routing::{get, post, put};
use serde_yaml_ng::Value;
use tw_config::edit::{self, EditError};
use tw_config::history::Origin;
use tw_config::refs;
use tw_engine::rule::{OneOrMany, When};
use tw_engine::{Group, GroupType, RouteSet, Rule, SetAction};
use tw_yaml::Step;

use crate::resources::{checked_name, invalid, mapping};
use crate::{ApplyError, ControlState, Fail, apply_fail};

pub fn router() -> axum::Router<ControlState> {
    axum::Router::new()
        .route("/routes", post(create_route))
        .route("/routes/{name}", put(update_route).delete(delete_route))
        .route("/default_route", put(set_default_route))
        .route("/groups", post(create_group))
        .route("/groups/{name}", put(update_group).delete(delete_group))
        .route("/models", get(known_models))
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
            let mut out = edit::upsert(text, edit::ROUTES, None, &mapping(&set)?)?;
            if let Some(keys) = &req.keys {
                out = assign_keys(&out, cfg, None, &set.name, keys)?;
            }
            route_probes(&out, &req.route_probes)
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
            if let Some(keys) = &req.keys {
                out = assign_keys(&out, cfg, Some(&name), &set.name, keys)?;
            }
            route_probes(&out, &req.route_probes)
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
                return Err(ApplyError::InUse(format!(
                    "Route `{name}` is the default route and cannot be deleted. Make another route the \
             default first."
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
                    return Err(invalid(format!(
            "a key cannot be moved onto route `{name}`, which is being deleted"
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

/// 把这几类辅助请求设为「交给路由」。
fn route_probes(text: &str, probes: &[String]) -> Result<String, ApplyError> {
    let mut out = text.to_string();
    for p in probes {
        // 总称不是一个可以单独设置的类别
        if p == "assistant_internal" || !INTENTS.contains(&p.as_str()) {
            return Err(invalid(format!(
                "there is no auxiliary-request class `{p}`"
            )));
        }
        out = edit::set(
            &out,
            &[Step::key("client_probes"), Step::key(p.as_str())],
            Some(&Value::String("route".to_string())),
        )?;
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

fn to_route(input: &tw_api::RouteInput, cfg: &tw_config::Config) -> Result<RouteSet, String> {
    let name = checked_name(&input.name, "route").map_err(|e| e.text)?;
    reserved(&name, "route")?;
    let mut seen = HashSet::new();
    let rules = input
        .rules
        .iter()
        .map(|r| {
            let rule = to_rule(r, cfg)?;
            if !seen.insert(rule.name.clone()) {
                return Err(format!("the route has two rules named `{}`", rule.name));
            }
            Ok(rule)
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(RouteSet { name, rules })
}

/// 界面交过来的一条规则 → 配置里的规则。**写法错在哪儿，这里就说哪儿。**
pub(crate) fn to_rule(input: &tw_api::RuleInput, cfg: &tw_config::Config) -> Result<Rule, String> {
    let name = checked_name(&input.name, "rule").map_err(|e| e.text)?;
    let when = when_from(&input.conditions, cfg).map_err(|e| format!("rule `{name}`: {e}"))?;
    let to = input
        .to
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    let deny = match input.deny.as_deref().map(str::trim) {
        Some("") => return Err(format!("rule `{name}`: a denial needs a reason")),
        other => other.map(str::to_string),
    };
    if to.is_some() && deny.is_some() {
        return Err(format!("rule `{name}` cannot both forward and deny"));
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
fn when_from(conds: &[tw_api::ConditionView], cfg: &tw_config::Config) -> Result<When, String> {
    let mut w = When::default();
    let mut seen = HashSet::new();
    for c in conds {
        let field = c.field.as_str();
        if !seen.insert(field) {
            return Err(format!("condition `{field}` appears twice"));
        }
        let vals: Vec<String> = c
            .values
            .iter()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect();
        let one = || -> Result<String, String> {
            match vals.as_slice() {
                [v] => Ok(v.clone()),
                [] => Err(format!("condition `{field}` has no value")),
                _ => Err(format!("condition `{field}` takes exactly one value")),
            }
        };
        let flag = || -> Result<bool, String> {
            match one()?.as_str() {
                "true" => Ok(true),
                "false" => Ok(false),
                v => Err(format!(
                    "condition `{field}` takes true or false, and got `{v}`"
                )),
            }
        };
        let many = |allowed: &dyn Fn(&str) -> bool, what: &str| -> Result<OneOrMany, String> {
            if let Some(bad) = vals.iter().find(|v| !allowed(v.as_str())) {
                return Err(format!("there is no {what} `{bad}`"));
            }
            match vals.as_slice() {
                [] => Err(format!("condition `{field}` has no value")),
                [v] => Ok(OneOrMany::One(v.clone())),
                vs => Ok(OneOrMany::Many(vs.to_vec())),
            }
        };
        match field {
            "model" => w.model = Some(one()?),
            "client" => {
                let k = one()?;
                if !cfg.clients.iter().any(|c| c.name == k) {
                    return Err(format!("there is no gateway key `{k}`"));
                }
                w.client = Some(k);
            }
            "dialect" => {
                let d = one()?;
                if !DIALECTS.contains(&d.as_str()) {
                    return Err(format!("`{d}` is not a client format we support"));
                }
                w.dialect = Some(d);
            }
            "input_tokens" => w.input_tokens = Some(one()?),
            "max_tokens" => w.max_tokens = Some(one()?),
            "tool_count" => w.tool_count = Some(one()?),
            "cache" => w.cache = Some(flag()?),
            "tools" => w.tools = Some(flag()?),
            "image" => w.image = Some(flag()?),
            "thinking" => w.thinking = Some(flag()?),
            "stream" => w.stream = Some(flag()?),
            "intent" => {
                w.intent = Some(many(&|v| INTENTS.contains(&v), "auxiliary-request class")?)
            }
            "provider_would_be" => {
                w.provider_would_be = Some(many(
                    &|v| cfg.providers.iter().any(|p| p.name == v),
                    "upstream",
                )?)
            }
            other => return Err(format!("`{other}` is not a condition we support")),
        }
    }
    // 比较式写错了（`200k` 少了比较符）：保存之前就说
    w.validate().map_err(|e| e.to_string())?;
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
                return Err(ApplyError::InUse(format!(
                    "Group `{name}` is still referenced by {by}; drop those references before deleting it."
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
        return Err(ApplyError::InUse(
            "The built-in group of every upstream follows the upstream list; it cannot be edited \
             or deleted."
                .to_string(),
        ));
    }
    Ok(())
}

fn to_group(input: &tw_api::GroupInput, cfg: &tw_config::Config) -> Result<Group, String> {
    let name = checked_name(&input.name, "group").map_err(|e| e.text)?;
    reserved(&name, "group")?;
    if cfg.providers.iter().any(|p| p.name == name) {
        return Err(format!(
            "`{name}` is already the name of an upstream. A rule points at an upstream or a group \
             by name, so the two cannot share one."
        ));
    }
    let kind: GroupType = serde_yaml_ng::from_value(Value::String(input.kind.clone()))
        .map_err(|_| format!("`{}` is not a strategy we support", input.kind))?;
    let mut providers = Vec::new();
    for p in &input.providers {
        let p = p.trim();
        if !cfg.providers.iter().any(|x| x.name == p) {
            return Err(format!("there is no upstream `{p}`"));
        }
        if providers.iter().any(|x| x == p) {
            return Err(format!("upstream `{p}` appears twice"));
        }
        providers.push(p.to_string());
    }
    if providers.is_empty() {
        return Err("a group needs at least one upstream".to_string());
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
                return Err(format!("the preferred upstream `{sel}` is not a member"));
            }
            Some(sel)
        }
        _ => None,
    };
    Ok(Group {
        name,
        kind,
        providers,
        // 只对轮询有意义；其余策略保持默认，不往文件里写
        session_affinity: kind != GroupType::LoadBalance || input.session_affinity,
        selected,
    })
}

// ─────────────────────────────────────────────────────────── 已知模型

/// 网关知道的全部模型，以及能提供它们的上游。
///
/// **和 `/v1/models` 同一份目录**：停用的上游、启用范围外的模型都不在里面。
/// 规则条件和试算的模型建议用它。
async fn known_models(State(s): State<ControlState>) -> Json<Vec<tw_api::KnownModel>> {
    let catalog = s.gateway.catalog.load();
    Json(
        catalog
            .all()
            .into_iter()
            .map(|id| tw_api::KnownModel {
                id: id.to_string(),
                providers: catalog.providers_for(id).to_vec(),
            })
            .collect(),
    )
}

// ─────────────────────────────────────────────────────────── 视图

/// 概览里的一条规则。**全文**：编辑对话框靠它回填。
pub(crate) fn rule_view(r: &Rule, n: tw_engine::RuleNotes) -> tw_api::RuleView {
    tw_api::RuleView {
        name: r.name.clone(),
        conditions: crate::describe_when(&r.when),
        to: r.to.clone(),
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

fn reserved(name: &str, what: &str) -> Result<(), String> {
    if name.starts_with(tw_engine::RESERVED_PREFIX) {
        return Err(format!(
            "a {what} name cannot start with {}; those are reserved for built-ins",
            tw_engine::RESERVED_PREFIX
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
