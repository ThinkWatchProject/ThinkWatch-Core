//! 网关密钥：增删改、更换，以及「哪一把是默认的」。
//!
//! # 为什么密钥也要有自己的资源接口
//!
//! 在此之前密钥是唯一一段只能靠通用配置补丁改的东西 —— 界面自己拼
//! `name: x\nkey: tw-…` 那两行 YAML，自己判断能不能删。于是几条本该由 core
//! 保证的规则散在界面里，或者根本没人保证：
//!
//! - **默认密钥删不得。**删掉之后，手动配置的客户端会在某个说不清的时刻断掉。
//! - **正被接管的客户端的密钥删不得。**那个客户端配置里写着它的值，删掉的下一个
//!   请求就是 401，而用户刚做的动作是「删一把看起来没用的钥匙」。
//! - **改名要带着引用一起改。**规则里的 `client` 是精确匹配一个密钥名。
//!
//! # 更换密钥为什么要 core 做
//!
//! 换掉一把被接管的客户端在用的密钥，是两件必须一起成的事：改我们的配置，
//! 和改它的配置。拆成两个接口由界面串的话，中间失败留下的是「界面说换好了、
//! 那个客户端连不上」—— 而用户此刻正相信自己刚刚修好了一个安全问题。

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use serde_yaml_ng::Value;
use tw_adopt::{detect, plan};
use tw_config::edit;
use tw_config::history::Origin;
use tw_config::refs;
use tw_yaml::Step;

use crate::{ApplyError, ControlState, Fail, apply_fail, fail};
use tw_types::msg;

pub fn router() -> axum::Router<ControlState> {
    axum::Router::new()
        .route("/keys", get(list).post(create))
        .route("/keys/{name}", put(update).delete(delete_key))
        .route("/keys/{name}/value", get(value))
        .route("/keys/{name}/rotate", post(rotate))
        .route("/default_key", put(set_default))
}

/// 配置里密钥那一段。**没有 `edit::Section` 常量**：它在 `tw-config` 里
/// 按用途分段，而密钥这一段之前没人按资源改过
pub(crate) const CLIENTS: edit::Section = edit::Section {
    path: &["clients"],
    what: "gateway key",
};

fn not_found(name: &str) -> ApplyError {
    crate::resources::not_found("gateway key", name)
}

// ---------------------------------------------------------------- 读

/// 密钥的值给不给明文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reveal {
    /// 密钥页：值原样显示，旁边一个复制按钮
    Plain,
    /// 概览：到处都在读，用不着密钥的值
    Masked,
}

/// **密钥页给明文。**这把钥匙只能连本机这个网关，它的作用就是被复制进
/// 客户端配置 —— 只给头尾几位的话，用户要么点「复制」贴出来看一眼，要么
/// 去翻 config.yaml，两条路都比直接显示更糟。上游的凭据不走这里，照旧脱敏。
async fn list(State(s): State<ControlState>) -> Json<Vec<tw_api::ClientView>> {
    Json(views(&s, Reveal::Plain).await)
}

pub async fn views(s: &ControlState, reveal: Reveal) -> Vec<tw_api::ClientView> {
    let cfg = s.config();
    let default = cfg.default_client().map(|c| c.name.clone());
    // **按密钥算最后一次使用，不按客户端自报的标识** —— 后者可以伪造，
    // 而「这把钥匙还有没有人在用」要的正是一个不能伪造的答案
    let seen: Vec<(String, i64)> = match &s.store {
        Some(st) => st
            .lock()
            .await
            .db()
            .last_seen_by_client()
            .unwrap_or_default(),
        None => Vec::new(),
    };
    cfg.clients
        .iter()
        .map(|c| tw_api::ClientView {
            name: c.name.clone(),
            key: match reveal {
                Reveal::Plain => c.key.clone(),
                Reveal::Masked => tw_secret::mask_secret(&c.key),
            },
            max_concurrent: c.max_concurrent,
            route: c.route.clone(),
            allow: c.allow.clone(),
            client: c.client.clone(),
            disabled: c.disabled,
            default: default.as_deref() == Some(c.name.as_str()),
            last_seen_ms: seen
                .iter()
                .find(|(k, _)| *k == c.name)
                .map(|(_, at)| *at as u64),
        })
        .collect()
}

/// 一把的明文。「复制」走这里：它要的是此刻配置里的那个值，不是界面手里
/// 可能已经过期的那份列表。
async fn value(
    State(s): State<ControlState>,
    Path(name): Path<String>,
) -> Result<Json<tw_api::KeyValue>, Fail> {
    let cfg = s.config();
    let c = cfg
        .clients
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                msg!("control.key_not_found", key = name.clone() => "There is no gateway key named `{key}`."),
            )
        })?;
    Ok(Json(tw_api::KeyValue {
        name: c.name.clone(),
        key: c.key.clone(),
    }))
}

// ---------------------------------------------------------------- 写

async fn create(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::KeySave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let c = to_client(&req.key, None, cfg)?;
            Ok(edit::upsert(
                text,
                CLIENTS,
                None,
                &crate::resources::mapping(&c)?,
            )?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn update(
    State(s): State<ControlState>,
    Path(name): Path<String>,
    Json(req): Json<tw_api::KeySave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let existing = cfg
                .clients
                .iter()
                .find(|c| c.name == name)
                .ok_or_else(|| not_found(&name))?;
            let c = to_client(&req.key, Some(existing), cfg)?;
            // 默认密钥停用 = 把所有手动配置的客户端一起关掉，而它们不在这一页上
            if c.disabled && cfg.default_client().map(|d| d.name.as_str()) == Some(name.as_str()) {
                return Err(ApplyError::InUse(
                    "The default key cannot be disabled. Every client that has not been pointed \
                     at the gateway explicitly uses it, and disabling it would break all of them."
                        .to_string(),
                ));
            }
            let mut out =
                edit::upsert(text, CLIENTS, Some(&name), &crate::resources::mapping(&c)?)?;
            if c.name != name {
                // **和那一项在同一个版本里改**：分两次写的话，中间那一版里
                // 规则指向一个不存在的密钥名
                out = refs::rename_client(&out, cfg, &name, &c.name)?;
            }
            Ok(out)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn delete_key(
    State(s): State<ControlState>,
    Path(name): Path<String>,
    Query(q): Query<tw_api::BaseVersion>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    // 接管状态在对方配置旁边的记录里，读它要走文件系统 —— 在改配置之前问，
    // 拿到的是「此刻」的答案
    let adopted = adopted_client(&s, &name);
    let version = s
        .cfg
        .transform(q.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let c = cfg
                .clients
                .iter()
                .find(|c| c.name == name)
                .ok_or_else(|| not_found(&name))?;
            if cfg.default_client().map(|d| d.name.as_str()) == Some(name.as_str()) {
                return Err(ApplyError::InUse(
                    "The default key cannot be deleted. Every client that has not been pointed at \
                     the gateway explicitly uses it, and they could not connect without it."
                        .to_string(),
                ));
            }
            if let Some(label) = &adopted {
                return Err(ApplyError::InUse(format!(
                    "{label} is pointed at the gateway and has this key in its configuration. Restore \
                 it before deleting the key."
                )));
            }
            let used = refs::client_refs(cfg, &c.name);
            if !used.is_empty() {
                return Err(ApplyError::InUse(format!(
                    "Gateway key `{name}` is still referenced by {}; drop those references before \
                     deleting it.",
                    used.join(", ")
                )));
            }
            Ok(edit::remove(text, CLIENTS, &name)?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn set_default(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::DefaultKeySave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let c = cfg
                .clients
                .iter()
                .find(|c| c.name == req.name)
                .ok_or_else(|| not_found(&req.name))?;
            if c.disabled {
                return Err(ApplyError::InUse(format!(
                    "Gateway key `{}` is disabled, so it cannot be the default key.",
                    req.name
                )));
            }
            // 默认值不写进文件
            let value =
                (req.name != tw_config::DEFAULT_KEY).then(|| Value::String(req.name.clone()));
            Ok(edit::set(
                text,
                &[Step::key("default_key")],
                value.as_ref(),
            )?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

// ---------------------------------------------------------------- 更换

/// 换一把新的，并且**把新值同步给正在用它的那个客户端**。
///
/// 顺序是刻意的：先改我们的配置，再改对方的。反过来的话，中间那一刻对方
/// 配置里写着一把网关还不认识的钥匙。而按这个顺序，中间那一刻对方用的是
/// 一把刚作废的钥匙 —— 同样是坏的，但**它是在用户刚按下「更换」的那一秒**，
/// 界面正看着结果，而不是几小时后。
async fn rotate(
    State(s): State<ControlState>,
    Path(name): Path<String>,
    Json(req): Json<tw_api::KeyRotate>,
) -> Result<Json<tw_api::KeyRotated>, Fail> {
    let fresh = tw_config::generate_key();
    let owner = {
        let cfg = s.config();
        let c = cfg
            .clients
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                msg!("control.key_not_found", key = name.clone() => "There is no gateway key named `{key}`."),
            )
        })?;
        c.client.clone()
    };
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let i = cfg
                .clients
                .iter()
                .position(|c| c.name == name)
                .ok_or_else(|| not_found(&name))?;
            Ok(edit::set(
                text,
                &[Step::key("clients"), Step::Index(i), Step::key("key")],
                Some(&Value::String(fresh.clone())),
            )?)
        })
        .await
        .map_err(apply_fail)?;

    // 只有被接管的客户端要同步：没接管的那些，密钥根本没写进它们的配置
    let mut synced = Vec::new();
    let mut failed = Vec::new();
    if let Some(id) = owner
        && let Some(c) = tw_adopt::clients::adoptable()
            .into_iter()
            .find(|c| c.id == id)
        && detect::detect_one(&c, &s.home).adopted_at_ms.is_some()
    {
        let gw = tw_adopt::clients::Gateway {
            base: format!("http://127.0.0.1:{}", s.config().listen.gateway.port),
            key: Some(fresh.clone()),
        };
        match plan::plan_adopt(&c, &s.home, &gw)
            .and_then(|p| plan::apply(&c, &p, &tw_adopt::foreign::backup_root()))
        {
            Ok(a) => synced.push(tw_api::KeySynced {
                client: c.id.to_string(),
                name: c.name.to_string(),
                takes_effect: c.takes_effect.slug().to_string(),
                backup: a.backup.display().to_string(),
            }),
            // **密钥已经换了**，这一条不能把整次更换报成失败 —— 那会让用户
            // 以为旧密钥还能用
            Err(e) => failed.push(tw_api::KeySyncFailed {
                client: c.id.to_string(),
                name: c.name.to_string(),
                error: e.to_string(),
            }),
        }
    }
    Ok(Json(tw_api::KeyRotated {
        version,
        key: fresh,
        synced,
        failed,
    }))
}

/// 这把密钥是为某个客户端生成的，而那个客户端此刻正被接管着吗。
/// 返回它在界面上的名字。
fn adopted_client(s: &ControlState, key: &str) -> Option<String> {
    let cfg = s.config();
    let id = cfg.clients.iter().find(|c| c.name == key)?.client.clone()?;
    let c = tw_adopt::clients::adoptable()
        .into_iter()
        .find(|c| c.id == id)?;
    detect::detect_one(&c, &s.home)
        .adopted_at_ms
        .map(|_| c.name.to_string())
}

// ---------------------------------------------------------------- 表单 → 配置

fn to_client(
    input: &tw_api::KeyInput,
    existing: Option<&tw_config::Client>,
    cfg: &tw_config::Config,
) -> Result<tw_config::Client, ApplyError> {
    let name = crate::resources::checked_name(&input.name, "gateway key")
        .map_err(|e| crate::resources::invalid(e.text))?;
    if let Some(r) = input
        .route
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        && cfg.engine().rules_of(r).is_none()
    {
        return Err(crate::resources::not_found("route", r));
    }
    Ok(tw_config::Client {
        name,
        // 新建时由 core 生成：字母表和长度是安全决定，界面不该有第二份
        key: existing
            .map(|c| c.key.clone())
            .unwrap_or_else(tw_config::generate_key),
        max_concurrent: input.max_concurrent.filter(|n| *n > 0),
        allow: input.allow.clone(),
        route: input
            .route
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(str::to_string),
        // 绑定由接管写，用户改不了 —— 它记的是「这把钥匙是为谁生成的」
        client: existing.and_then(|c| c.client.clone()),
        disabled: input.disabled,
    })
}
