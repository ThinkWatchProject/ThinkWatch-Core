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
use axum::routing::{get, post, put};
use serde_yaml_ng::Value;
use tw_config::edit::{self, EditError};
use tw_config::history::Origin;
use tw_config::refs::{self, ProviderRef};

use crate::{ApplyError, ControlState, Fail, apply_fail, fail};

pub fn router() -> axum::Router<ControlState> {
    axum::Router::new()
        .route("/providers", post(create_provider))
        .route("/providers/test", post(test_provider))
        .route("/providers/preview", post(preview_provider))
        .route(
            "/providers/{name}",
            put(update_provider).delete(delete_provider),
        )
        .route("/providers/{name}/models", get(provider_models))
        .route("/providers/{name}/models/refresh", post(refresh_models))
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
    // 删掉之后这一家的 client 就不在了，先拿着：吊销也要走它的出站设置
    let http = s.gateway.client_for(&name);
    let mut login = None;
    let version = s
        .cfg
        .transform(q.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let used = refs::provider_refs(cfg, &name);
            if !used.is_empty() {
                return Err(ApplyError::InUse(format!(
                    "上游「{name}」仍被{}引用，请先解除引用再删除",
                    describe(&used)
                )));
            }
            login = chatgpt_login(cfg, &name, &s.chatgpt.endpoints.token);
            Ok(edit::remove(text, edit::PROVIDERS, &name)?)
        })
        .await
        .map_err(apply_fail)?;
    if let Some(refresh) = login {
        let endpoint = s.chatgpt.endpoints.revoke.clone();
        tokio::spawn(async move {
            match tw_gateway::chatgpt::revoke(&http, &endpoint, &refresh).await {
                Ok(()) => tracing::info!(provider = %name, "已吊销 ChatGPT 登录凭据"),
                Err(why) => tracing::warn!(provider = %name, "未能吊销 ChatGPT 登录凭据：{why}"),
            }
        });
    }
    Ok(Json(tw_api::ConfigWritten { version }))
}

/// 删掉这个上游时要吊销的 ChatGPT refresh token。
///
/// **只吊销登录得来的**（发放它的正是这个 token 端点），而且别的上游没有在用同一个 ——
/// 复制出来的上游共用一份登录，吊销会让留下的那个也失效。
fn chatgpt_login(cfg: &tw_config::Config, name: &str, token_endpoint: &str) -> Option<String> {
    let p = cfg.providers.iter().find(|p| p.name == name)?;
    let o = p.oauth.as_ref()?;
    let ours = p.effective_protocol() == Some(tw_config::Protocol::Chatgpt)
        && o.endpoint == token_endpoint
        && o.client_id.as_deref() == Some(tw_config::chatgpt::CLIENT_ID);
    let shared = cfg
        .providers
        .iter()
        .any(|q| q.name != name && q.oauth.as_ref().is_some_and(|x| x.refresh == o.refresh));
    (ours && !shared).then(|| o.refresh.clone())
}

/// 按接口地址自动识别会得到什么：协议、是不是官方端点、默认脱敏哪几类。
///
/// **不联网，只看地址。**编辑对话框在用户输入地址时调它，好让「自动识别」
/// 这个选项说清楚它此刻会选成什么 —— 这套判断只在 core 里写一遍。
async fn preview_provider(
    Json(req): Json<tw_api::ProviderPreviewRequest>,
) -> Result<Json<tw_api::ProviderPreview>, Fail> {
    let p = tw_config::Provider {
        base_url: req.base_url.trim().to_string(),
        ..Default::default()
    };
    // 选定了协议，密钥就按选定的协议放；「自动识别」那一项要说的仍是按地址推断的结果
    let chosen = tw_config::Provider {
        protocol: req
            .protocol
            .as_deref()
            .map(|v| slug("接口协议", v))
            .transpose()
            .map_err(|e| fail(StatusCode::BAD_REQUEST, e))?,
        ..p.clone()
    };
    Ok(Json(tw_api::ProviderPreview {
        protocol: p.effective_protocol().map(|x| x.slug().to_string()),
        official: p.is_official_endpoint(),
        redact: p
            .effective_redact()
            .iter()
            .map(|k| k.slug().to_string())
            .collect(),
        auth_header: chosen.auth_header().0.to_string(),
    }))
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
                .ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("未找到名为「{n}」的上游")))?,
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
    // OAuth 的 token：新填的那份只能用现成的 access token；沿用原来那份时走网关，
    // **按原来的名字换** —— 缓存和轮换写回都认名字，而表单里可能刚改了名
    let token = match (&req.provider.oauth, existing) {
        (tw_api::OAuthChange::Set { access, .. }, _) => {
            // **不拿 refresh token 去换。**服务端可能当场作废旧的那把，而
            // 新换到的那把还没有地方写 —— 检测一次就把凭据弄坏了
            match access.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
                Some(a) => Some(a.to_string()),
                None => {
                    return Ok(Json(failed(
                        "OAuth 凭据需要保存后才能检测。如需立即检测，请同时填写 access token"
                            .to_string(),
                    )));
                }
            }
        }
        (tw_api::OAuthChange::Keep, Some(e)) if e.oauth.is_some() && p.oauth.is_some() => {
            match s.gateway.oauth_token_for(e, &http).await {
                Ok(t) => Some(t),
                Err(e) => return Ok(Json(failed(e))),
            }
        }
        _ => None,
    };
    let headers = match p.outbound_headers(token.as_deref(), None) {
        Ok(h) => h,
        Err(e) => return Ok(Json(failed(e.to_string()))),
    };
    let r = tw_gateway::probe(&http, &p.base_url, &headers, protocol).await;
    Ok(Json(tw_api::ProviderTestResult {
        ok: r.ok,
        protocol: protocol.map(|x| x.slug().to_string()),
        latency_ms: r.latency_ms,
        models: crate::model_list(r.models),
        via,
        error: r.error,
    }))
}

/// 一个上游的模型清单：每个模型在不在启用范围里、上下文窗口多大、按它选的
/// 价目表怎么计价。
async fn provider_models(
    State(s): State<ControlState>,
    Path(name): Path<String>,
) -> Result<Json<tw_api::ProviderModelsView>, Fail> {
    let cfg = s.config();
    let p = cfg
        .providers
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("未找到名为「{name}」的上游")))?;
    Ok(Json(models_view(&s, p, s.gateway.models.listing(p))))
}

/// 马上向上游重新获取模型清单。
async fn refresh_models(
    State(s): State<ControlState>,
    Path(name): Path<String>,
) -> Result<Json<tw_api::ProviderModelsView>, Fail> {
    let listing = tw_gateway::models::refresh_one(&s.gateway, &name)
        .await
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("未找到名为「{name}」的上游")))?;
    let cfg = s.config();
    let p = cfg
        .providers
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("未找到名为「{name}」的上游")))?;
    Ok(Json(models_view(&s, p, listing)))
}

fn models_view(
    s: &ControlState,
    p: &tw_config::Provider,
    listing: tw_gateway::models::Listing,
) -> tw_api::ProviderModelsView {
    let book = s.gateway.pricing.load();
    let date = &book.table().date;
    tw_api::ProviderModelsView {
        provider: p.name.clone(),
        source: listing.source.slug().to_string(),
        checked_at_ms: listing.checked_at_ms,
        error: listing.error,
        models: listing
            .models
            .into_iter()
            .map(|id| {
                let r = book.resolve_for(&p.name, &id);
                tw_api::ModelRow {
                    enabled: p.uses_model(&id),
                    context_window: r.as_ref().and_then(|r| r.price.max_input_tokens),
                    price: r.as_ref().map(|r| {
                        crate::pricing::price_fields(&tw_pricing::PerMillion::of(&r.price))
                    }),
                    price_source: r.as_ref().map(|r| tw_store::price_source(&r.source, date)),
                    estimated: r.as_ref().is_some_and(|r| r.cross_platform),
                    id,
                }
            })
            .collect(),
    }
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
        (tw_api::SecretChange::Keep, Some(e)) => e.key.clone(),
        (tw_api::SecretChange::Keep, None) | (tw_api::SecretChange::None, _) => None,
        (tw_api::SecretChange::Set { value }, _) => {
            let v = value.trim();
            if v.is_empty() {
                return Err("API 密钥不能为空".to_string());
            }
            Some(tw_config::Secret::new(v))
        }
    };
    let headers = input
        .headers
        .iter()
        .map(|h| {
            let name = h.name.trim().to_string();
            let value = match &h.value {
                Some(v) => tw_config::Secret::new(v.trim()),
                // 沿用原值：界面拿不到打码之前的值，这一行不动就不传
                None => existing
                    .and_then(|e| e.headers.get(&name))
                    .map(|x| x.value.clone())
                    .ok_or_else(|| format!("请求头「{name}」缺少值"))?,
            };
            Ok(tw_config::Header { name, value })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let oauth = match (&input.oauth, existing) {
        (tw_api::OAuthChange::Keep, Some(e)) => e.oauth.clone(),
        (tw_api::OAuthChange::Keep, None) | (tw_api::OAuthChange::None, _) => None,
        (
            tw_api::OAuthChange::Set {
                refresh,
                endpoint,
                client_id,
                client_secret,
                access,
            },
            _,
        ) => {
            let nonempty = |v: &Option<String>| {
                v.as_deref()
                    .map(str::trim)
                    .filter(|x| !x.is_empty())
                    .map(str::to_string)
            };
            Some(tw_config::OAuth {
                access: nonempty(access),
                expires_at: None,
                refresh: refresh.trim().to_string(),
                endpoint: endpoint.trim().to_string(),
                client_id: nonempty(client_id),
                client_secret: nonempty(client_secret),
                // 沿用原来写的提前量：界面不编辑它
                refresh_before: existing
                    .and_then(|e| e.oauth.as_ref())
                    .and_then(|o| o.refresh_before.clone()),
            })
        }
    };
    let provider = tw_config::Provider {
        name,
        base_url,
        key,
        headers: tw_config::Headers::new(headers),
        oauth,
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
        models_only: input
            .models_only
            .as_ref()
            .map(|ms| ms.iter().map(|m| m.trim().to_string()).collect::<Vec<_>>()),
        pricing: input.pricing.clone(),
        disabled: input.disabled,
    };
    // **保存和检测之前就说清楚凭据写法哪儿不对**，而不是等整份配置校验时
    // 报一条指着 YAML 的错误
    provider.check_credential().map_err(|e| e.to_string())?;
    Ok(provider)
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
                    "代理「{name}」仍被上游{}使用，请先解除关联再删除",
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
                .ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("未找到名为「{n}」的代理")))?,
        ),
        None => None,
    };
    let px = to_proxy(&req.proxy, existing).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    let hop = tw_gateway::hop_of(&px).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    // 握手的目标：正在编辑的那个代理原来服务的上游
    let (host, port) = tw_gateway::proxy_target(&cfg, req.current.as_deref().unwrap_or(&px.name));
    let r = tw_gateway::l1_proxy(&hop, &host, port).await;
    Ok(Json(crate::l1_view(px.name.clone(), None, r)))
}

fn to_proxy(
    input: &tw_api::ProxyInput,
    existing: Option<&tw_config::Proxy>,
) -> Result<tw_config::Proxy, String> {
    let name = checked_name(&input.name, "代理")?;
    if matches!(name.as_str(), tw_config::DIRECT | tw_config::SYSTEM) {
        return Err(format!("「{name}」是内置选项的名称，请使用其他名称"));
    }
    let addr = input.addr.trim().to_string();
    let port_ok = addr
        .rsplit_once(':')
        .is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok_and(|p| p > 0));
    if !port_ok {
        return Err(format!("代理地址「{addr}」应写成 主机:端口 的形式"));
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
                return Err("密码中不能包含 ${".to_string());
            }
            Some(tw_config::proxy::ProxyAuth {
                user: user.to_string(),
                pass: tw_config::Secret::new(pass.clone()),
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
        return Err(format!("{what}名称首尾不能包含空白"));
    }
    Ok(raw.to_string())
}

/// 界面上的一个选项值 → 配置里的枚举。**和 YAML 里写的词是同一套**。
fn slug<T: serde::de::DeserializeOwned>(what: &str, v: &str) -> Result<T, String> {
    serde_yaml_ng::from_value(Value::String(v.to_string()))
        .map_err(|_| format!("{what}「{v}」不受支持"))
}

pub(crate) fn mapping<T: serde::Serialize>(v: &T) -> Result<serde_yaml_ng::Mapping, ApplyError> {
    match serde_yaml_ng::to_value(v) {
        Ok(Value::Mapping(m)) => Ok(m),
        Ok(_) => Err(invalid("无法序列化为映射".to_string())),
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
