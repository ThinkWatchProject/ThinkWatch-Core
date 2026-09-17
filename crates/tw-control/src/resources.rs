//! 上游和代理：按资源增删改，以及保存之前的检测。
//!
//! # 为什么是资源而不是补丁
//!
//! 以前界面把一个上游拼成一段 YAML，交给通用的补丁接口。三个问题：
//!
//! 1. **拼字符串的一方不懂引号规则。**一个带 `#` 的代理密码会被读成注释，
//!    写坏的是用户唯一的那份配置。
//! 2. **改名没有人跟着改引用。**路由规则还写着旧名字，保存被校验拒掉；
//!    用户看到的是「改个名字都保存不了」。
//! 3. **删除没有人检查引用。**删掉一个还在被规则指向的上游，同样被拒，
//!    而拒绝的原因要用户自己去配置文件里找。
//!
//! 现在界面交过来的是结构。渲染由 serde 做、落盘由 `tw_config::edit` 做、
//! 引用由 `tw_config::refs` 管 —— 一次保存就是一个版本。

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{post, put};
use serde_yaml_ng::Value;
use tw_config::edit::{self, EditError};
use tw_config::history::Origin;
use tw_config::refs::{self, ProviderRef};

use crate::{ApplyError, ControlState, Fail, apply_fail, fail};

pub fn router() -> axum::Router<ControlState> {
    axum::Router::new()
        .route("/providers", post(create_provider))
        .route("/providers/test", post(test_provider))
        .route(
            "/providers/{name}",
            put(update_provider).delete(delete_provider),
        )
        .route("/proxies", post(create_proxy))
        .route("/proxies/test", post(test_proxy))
        .route("/proxies/{name}", put(update_proxy).delete(delete_proxy))
}

// ─────────────────────────────────────────────────────────── 上游

async fn create_provider(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::ProviderSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, _| {
            let p = to_provider(&req.provider, None).map_err(invalid)?;
            Ok(edit::upsert(text, edit::PROVIDERS, None, &mapping(&p)?)?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn update_provider(
    State(s): State<ControlState>,
    Path(name): Path<String>,
    Json(req): Json<tw_api::ProviderSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let existing = cfg
                .providers
                .iter()
                .find(|p| p.name == name)
                .ok_or_else(|| not_found("上游", &name))?;
            let p = to_provider(&req.provider, Some(existing)).map_err(invalid)?;
            let mut out = edit::upsert(text, edit::PROVIDERS, Some(&name), &mapping(&p)?)?;
            if p.name != name {
                // **和那一项在同一个版本里改** —— 分两次写的话，中间那一版
                // 的规则指向一个不存在的名字，会被校验拒掉
                out = refs::rename_provider(&out, cfg, &name, &p.name)?;
            }
            Ok(out)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn delete_provider(
    State(s): State<ControlState>,
    Path(name): Path<String>,
    Query(q): Query<tw_api::BaseVersion>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(q.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let used = refs::provider_refs(cfg, &name);
            if !used.is_empty() {
                return Err(ApplyError::InUse(format!(
                    "上游「{name}」仍被{}引用，解除引用后才能删除",
                    describe(&used)
                )));
            }
            Ok(edit::remove(text, edit::PROVIDERS, &name)?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

/// 检测一个上游，**不保存**。
///
/// 走的出站路径和转发完全一样：同一个构造 client 的函数、同一套凭据取法。
/// 否则会出现「检测通了、转发不通」。
async fn test_provider(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::ProviderTest>,
) -> Result<Json<tw_api::ProviderTestResult>, Fail> {
    let cfg = s.config();
    let existing = match req.current.as_deref() {
        Some(n) => Some(
            cfg.providers
                .iter()
                .find(|p| p.name == n)
                .ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("没有叫「{n}」的上游")))?,
        ),
        None => None,
    };
    let p = to_provider(&req.provider, existing).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    let protocol = p.effective_protocol();
    let via = (p.proxy != tw_config::DIRECT).then(|| p.proxy.clone());
    let failed = |error: String| tw_api::ProviderTestResult {
        ok: false,
        protocol: protocol.map(|x| x.slug().to_string()),
        latency_ms: 0,
        models: tw_api::ModelList::Empty,
        via: via.clone(),
        error: Some(error),
    };
    let http = match tw_gateway::client_for_provider(&cfg, &p) {
        Ok(h) => h,
        Err(e) => return Ok(Json(failed(e.message))),
    };
    let key = match (&req.provider.key, existing) {
        // 凭据没改：走网关那条路。OAuth 的缓存和轮换写回都在那儿
        (None, Some(e)) => s.gateway.key_for(e, &http).await,
        (Some(tw_api::CredentialInput::Oauth { access, .. }), _) => {
            // **不拿 refresh token 去换。**服务端可能当场作废旧的那把，而
            // 新换到的那把还没有地方写 —— 检测一次就把凭据弄坏了
            match access.as_deref().filter(|a| !a.is_empty()) {
                Some(a) => Ok(a.to_string()),
                None => Err(
                    "OAuth 凭据需要保存后才能检测。要现在检测，请同时填写 access token。"
                        .to_string(),
                ),
            }
        }
        _ => p.resolved_key().map_err(|e| e.to_string()),
    };
    let key = match key {
        Ok(k) => k,
        Err(e) => return Ok(Json(failed(e))),
    };
    let r = tw_gateway::probe(&http, &p.base_url, &key, protocol).await;
    Ok(Json(tw_api::ProviderTestResult {
        ok: r.ok,
        protocol: protocol.map(|x| x.slug().to_string()),
        latency_ms: r.latency_ms,
        models: crate::model_list(r.models),
        via,
        error: r.error,
    }))
}

fn to_provider(
    input: &tw_api::ProviderInput,
    existing: Option<&tw_config::Provider>,
) -> Result<tw_config::Provider, String> {
    let name = checked_name(&input.name, "上游")?;
    let base_url = match (&input.base_url, existing) {
        (Some(u), _) => u.trim().to_string(),
        (None, Some(e)) => e.base_url.clone(),
        (None, None) => return Err("接口地址不能为空".to_string()),
    };
    let key = match (&input.key, existing) {
        (Some(c), _) => credential(c)?,
        (None, Some(e)) => e.key.clone(),
        (None, None) => return Err("缺少凭据".to_string()),
    };
    Ok(tw_config::Provider {
        name,
        base_url,
        key,
        protocol: input
            .protocol
            .as_deref()
            .map(|v| slug("接口协议", v))
            .transpose()?,
        models: input
            .models
            .iter()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .collect(),
        billing: input
            .billing
            .as_deref()
            .map(|v| slug("计费方式", v))
            .transpose()?,
        redact: input
            .redact
            .as_ref()
            .map(|ks| {
                ks.iter()
                    .map(|k| slug("脱敏类别", k))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?,
        trust: input
            .trust
            .as_deref()
            .map(|v| slug("信任级别", v))
            .transpose()?,
        proxy: input.proxy.trim().to_string(),
        on_proxy_fail: slug("代理不可用时的处理", &input.on_proxy_fail)?,
        pricing: input.pricing.clone(),
    })
}

fn credential(c: &tw_api::CredentialInput) -> Result<tw_config::Secret, String> {
    use tw_api::CredentialInput::*;
    Ok(match c {
        Key { value } => {
            let v = value.trim();
            if v.is_empty() {
                return Err("密钥不能为空".to_string());
            }
            // `${` 在配置里表示「从环境变量读」。一把真的含 `${` 的明文密钥
            // 写进去会被当成变量展开 —— 说清楚，而不是写一个读回来就变了的值
            if v.contains("${") {
                return Err("密钥里不能出现 `${`。要从环境变量读，请选择「环境变量」".to_string());
            }
            tw_config::Secret::Literal(v.to_string())
        }
        Env { var } => {
            let v = var.trim();
            let ok = !v.is_empty()
                && !v.starts_with(|c: char| c.is_ascii_digit())
                && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !ok {
                return Err(format!(
                    "环境变量名「{v}」不合法：只能由字母、数字和下划线组成，且不以数字开头"
                ));
            }
            tw_config::Secret::Literal(format!("${{{v}}}"))
        }
        Oauth {
            refresh,
            endpoint,
            client_id,
            client_secret,
            access,
        } => {
            let nonempty = |v: &Option<String>| {
                v.as_deref()
                    .map(str::trim)
                    .filter(|x| !x.is_empty())
                    .map(str::to_string)
            };
            tw_config::Secret::OAuth {
                oauth: tw_config::OAuth {
                    access: nonempty(access),
                    refresh: refresh.trim().to_string(),
                    endpoint: endpoint.trim().to_string(),
                    client_id: nonempty(client_id),
                    client_secret: nonempty(client_secret),
                    refresh_before: None,
                },
            }
        }
    })
}

// ─────────────────────────────────────────────────────────── 代理

async fn create_proxy(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::ProxySave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, _| {
            let px = to_proxy(&req.proxy, None).map_err(invalid)?;
            Ok(edit::upsert(text, edit::PROXIES, None, &mapping(&px)?)?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn update_proxy(
    State(s): State<ControlState>,
    Path(name): Path<String>,
    Json(req): Json<tw_api::ProxySave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let existing = cfg
                .proxies
                .iter()
                .find(|p| p.name == name)
                .ok_or_else(|| not_found("代理", &name))?;
            let px = to_proxy(&req.proxy, Some(existing)).map_err(invalid)?;
            let mut out = edit::upsert(text, edit::PROXIES, Some(&name), &mapping(&px)?)?;
            if px.name != name {
                out = refs::rename_proxy(&out, cfg, &name, &px.name)?;
            }
            Ok(out)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn delete_proxy(
    State(s): State<ControlState>,
    Path(name): Path<String>,
    Query(q): Query<tw_api::BaseVersion>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(q.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let users = refs::proxy_users(cfg, &name);
            if !users.is_empty() {
                return Err(ApplyError::InUse(format!(
                    "代理「{name}」仍被上游{}使用，解除后才能删除",
                    quoted(&users)
                )));
            }
            Ok(edit::remove(text, edit::PROXIES, &name)?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

/// 检测一个代理，**不保存**：连上它，并完成握手和认证。
async fn test_proxy(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::ProxyTest>,
) -> Result<Json<tw_api::L1Result>, Fail> {
    let cfg = s.config();
    let existing = match req.current.as_deref() {
        Some(n) => Some(
            cfg.proxies
                .iter()
                .find(|p| p.name == n)
                .ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("没有叫「{n}」的代理")))?,
        ),
        None => None,
    };
    let px = to_proxy(&req.proxy, existing).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    let hop = tw_gateway::hop_of(&px).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    // 握手的目标：正在编辑的那个代理原来服务的上游
    let (host, port) = tw_gateway::proxy_target(&cfg, req.current.as_deref().unwrap_or(&px.name));
    let r = tw_gateway::l1_proxy(&hop, &host, port).await;
    Ok(Json(crate::l1_view(format!("代理 {}", px.name), None, r)))
}

fn to_proxy(
    input: &tw_api::ProxyInput,
    existing: Option<&tw_config::Proxy>,
) -> Result<tw_config::Proxy, String> {
    let name = checked_name(&input.name, "代理")?;
    if matches!(name.as_str(), tw_config::DIRECT | tw_config::SYSTEM) {
        return Err(format!("「{name}」是内置选项的名字，请换一个"));
    }
    let addr = input.addr.trim().to_string();
    let port_ok = addr
        .rsplit_once(':')
        .is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok_and(|p| p > 0));
    if !port_ok {
        return Err(format!("代理地址「{addr}」要写成 主机:端口"));
    }
    let auth = match &input.auth {
        // 没动认证：沿用原来那一份，**原值不经过界面**
        tw_api::ProxyAuthInput::Keep => existing.and_then(|e| e.auth.clone()),
        tw_api::ProxyAuthInput::None => None,
        tw_api::ProxyAuthInput::Set { user, pass } => {
            let user = user.trim();
            if user.is_empty() {
                return Err("用户名不能为空".to_string());
            }
            // `${` 在配置里表示「从环境变量读」
            if pass.contains("${") {
                return Err("密码里不能出现 `${`".to_string());
            }
            Some(tw_config::proxy::ProxyAuth {
                user: user.to_string(),
                pass: tw_config::Secret::Literal(pass.clone()),
            })
        }
    };
    Ok(tw_config::Proxy {
        name,
        kind: slug("代理类型", &input.kind)?,
        addr,
        auth,
    })
}

// ─────────────────────────────────────────────────────────── 共用

pub(crate) fn checked_name(raw: &str, what: &str) -> Result<String, String> {
    if raw.trim().is_empty() {
        return Err(format!("{what}名称不能为空"));
    }
    if raw.trim() != raw {
        return Err(format!("{what}名称首尾不能有空白"));
    }
    Ok(raw.to_string())
}

/// 界面上的一个选项值 → 配置里的枚举。**和 YAML 里写的词是同一套**。
fn slug<T: serde::de::DeserializeOwned>(what: &str, v: &str) -> Result<T, String> {
    serde_yaml_ng::from_value(Value::String(v.to_string()))
        .map_err(|_| format!("{what}「{v}」不认识"))
}

pub(crate) fn mapping<T: serde::Serialize>(v: &T) -> Result<serde_yaml_ng::Mapping, ApplyError> {
    match serde_yaml_ng::to_value(v) {
        Ok(Value::Mapping(m)) => Ok(m),
        Ok(_) => Err(invalid("写不成一个映射".to_string())),
        Err(e) => Err(invalid(e.to_string())),
    }
}

pub(crate) fn invalid(msg: String) -> ApplyError {
    ApplyError::Edit(EditError::Unwritable(msg))
}

fn not_found(what: &'static str, name: &str) -> ApplyError {
    ApplyError::Edit(EditError::NotFound {
        what,
        name: name.to_string(),
    })
}

/// 「「relay-hk」、「relay-sg」」
pub(crate) fn quoted(names: &[String]) -> String {
    names
        .iter()
        .map(|n| format!("「{n}」"))
        .collect::<Vec<_>>()
        .join("、")
}

/// 「路由「默认」的规则「长上下文」、策略组「pool」」
fn describe(refs: &[ProviderRef]) -> String {
    refs.iter()
        .map(|r| match r {
            ProviderRef::RuleTarget { route, rule }
            | ProviderRef::RuleCondition { route, rule } => {
                format!("路由「{route}」的规则「{rule}」")
            }
            ProviderRef::Group { group } => format!("策略组「{group}」"),
        })
        .collect::<Vec<_>>()
        .join("、")
}

pub(crate) fn reference_view(r: &ProviderRef) -> tw_api::ReferenceView {
    match r {
        ProviderRef::RuleTarget { route, rule } => tw_api::ReferenceView::RuleTarget {
            route: route.clone(),
            rule: rule.clone(),
        },
        ProviderRef::RuleCondition { route, rule } => tw_api::ReferenceView::RuleCondition {
            route: route.clone(),
            rule: rule.clone(),
        },
        ProviderRef::Group { group } => tw_api::ReferenceView::Group {
            group: group.clone(),
        },
    }
}
