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
use serde_yaml_ng::Value;
use tw_config::edit::{self, EditError};
use tw_config::history::Origin;
use tw_config::refs::{self, ProviderRef};

use crate::contract::RouterExt;
use crate::{ApplyError, ControlState, Fail, apply_fail, fail};
use tw_api::ep;
use tw_types::{Msg, msg};

pub fn router() -> axum::Router<ControlState> {
    axum::Router::new()
        .at(ep::CreateProvider, create_provider)
        .at(ep::TestProvider, test_provider)
        .at(ep::PreviewProvider, preview_provider)
        .at(ep::UpdateProvider, update_provider)
        .at(ep::DeleteProvider, delete_provider)
        .at(ep::ProviderModels, provider_models)
        .at(ep::RefreshProviderModels, refresh_models)
        .at(ep::RefreshStaleModels, refresh_stale_models)
        .at(ep::CreateProxy, create_proxy)
        .at(ep::TestProxy, test_proxy)
        .at(ep::UpdateProxy, update_proxy)
        .at(ep::DeleteProxy, delete_proxy)
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
                .ok_or_else(|| not_found("upstream", &name))?;
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
                return Err(ApplyError::InUse(msg!(
                    "control.upstream_in_use", upstream = &name, refs = describe(&used) =>
                    "Upstream `{upstream}` is still referenced by {refs}; drop those references \
                     before deleting it."
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
                Ok(()) => tracing::info!(provider = %name, "the ChatGPT sign-in was revoked"),
                Err(why) => {
                    tracing::warn!(provider = %name, "the ChatGPT sign-in could not be revoked: {why}")
                }
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
            .map(|v| slug("protocol", v))
            .transpose()
            .map_err(|e| fail(StatusCode::BAD_REQUEST, e))?,
        ..p.clone()
    };
    Ok(Json(tw_api::ProviderPreview {
        protocol: p.effective_protocol().map(|x| x.slug().to_string()),
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
                .ok_or_else(|| crate::no_such_upstream(n))?,
        ),
        None => None,
    };
    let p = to_provider(&req.provider, existing).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    let protocol = p.effective_protocol();
    let via = (p.proxy != tw_config::DIRECT).then(|| p.proxy.clone());
    let failed = |error: Msg| tw_api::ProviderTestResult {
        ok: false,
        protocol: protocol.map(|x| x.slug().to_string()),
        latency_ms: 0,
        models: tw_api::ModelList::Empty,
        via: via.clone(),
        error: Some(error),
    };
    let http = match tw_gateway::client_for_provider(&cfg, &p) {
        Ok(h) => h,
        Err(e) => return Ok(Json(failed(e.detail))),
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
                    return Ok(Json(failed(msg!(
                        "control.provider_test.oauth_unsaved" =>
                        "An OAuth credential has to be saved before it can be checked. To check it \
                         now, fill in an access token as well."
                    ))));
                }
            }
        }
        (tw_api::OAuthChange::Keep, Some(e)) if e.oauth.is_some() && p.oauth.is_some() => {
            match s.gateway.oauth_token_for(e, &http).await {
                Ok(t) => Some(t),
                Err(e) => {
                    return Ok(Json(failed(msg!(
                        "control.provider_test.credentials", detail = e =>
                        "The credential could not be obtained: {detail}"
                    ))));
                }
            }
        }
        _ => None,
    };
    let headers = match p.outbound_headers(token.as_deref()) {
        Ok(h) => h,
        Err(e) => return Ok(Json(failed(e.msg()))),
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
        .ok_or_else(|| crate::no_such_upstream(&name))?;
    Ok(Json(models_view(&s, p, s.gateway.models.listing(p))))
}

/// 马上向上游重新获取模型清单。
async fn refresh_models(
    State(s): State<ControlState>,
    Path(name): Path<String>,
) -> Result<Json<tw_api::ProviderModelsView>, Fail> {
    let listing = tw_gateway::models::refresh_one(&s.gateway, &name)
        .await
        .ok_or_else(|| crate::no_such_upstream(&name))?;
    let cfg = s.config();
    let p = cfg
        .providers
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| crate::no_such_upstream(&name))?;
    Ok(Json(models_view(&s, p, listing)))
}

/// 页面打开时补问：没问过的、没问到的、过期的。**不等答案** —— 立刻回
/// 开始问的是哪几家，答案随 `models_changed` 一家一家地到。
async fn refresh_stale_models(State(s): State<ControlState>) -> Json<tw_api::ModelsRefreshing> {
    Json(tw_api::ModelsRefreshing {
        providers: tw_gateway::models::refresh_stale(&s.gateway),
    })
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
        status: crate::model_status(listing.status),
        fetching: listing.fetching,
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
) -> Result<tw_config::Provider, Msg> {
    let name = checked_name(&input.name, "upstream")?;
    let base_url = match (&input.base_url, existing) {
        (Some(u), _) => u.trim().to_string(),
        (None, Some(e)) => e.base_url.clone(),
        (None, None) => {
            return Err(msg!(
                "control.base_url_empty" => "The endpoint address cannot be empty."
            ));
        }
    };
    let key = match (&input.key, existing) {
        (tw_api::SecretChange::Keep, Some(e)) => e.key.clone(),
        (tw_api::SecretChange::Keep, None) | (tw_api::SecretChange::None, _) => None,
        (tw_api::SecretChange::Set { value }, _) => {
            let v = value.trim();
            if v.is_empty() {
                return Err(msg!("control.api_key_empty" => "The API key cannot be empty."));
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
                    .ok_or_else(|| {
                    msg!("control.header_no_value", header = name.clone() => "Header `{header}` has no value.")
                })?,
            };
            Ok(tw_config::Header { name, value })
        })
        .collect::<Result<Vec<_>, Msg>>()?;
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
            .map(|v| slug("protocol", v))
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
            .map(|v| slug("billing mode", v))
            .transpose()?
            .unwrap_or_default(),
        proxy: input.proxy.trim().to_string(),
        on_proxy_fail: slug("setting for an unusable proxy", &input.on_proxy_fail)?,
        models_only: input
            .models_only
            .as_ref()
            .map(|ms| ms.iter().map(|m| m.trim().to_string()).collect::<Vec<_>>()),
        pricing: input.pricing.clone(),
        disabled: input.disabled,
    };
    // **保存和检测之前就说清楚凭据写法哪儿不对**，而不是等整份配置校验时
    // 报一条指着 YAML 的错误
    provider.check_credential().map_err(|e| e.msg())?;
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
                .ok_or_else(|| not_found("proxy", &name))?;
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
                return Err(ApplyError::InUse(msg!(
                    "control.proxy_in_use", proxy = name, upstreams = quoted(&users) =>
                    "Proxy `{proxy}` is still used by upstream {upstreams}; unlink those before \
                     deleting it."
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
        Some(n) => Some(cfg.proxies.iter().find(|p| p.name == n).ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                msg!("control.proxy_not_found", proxy = n => "There is no proxy named `{proxy}`."),
            )
        })?),
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
) -> Result<tw_config::Proxy, Msg> {
    let name = checked_name(&input.name, "proxy")?;
    if matches!(name.as_str(), tw_config::DIRECT | tw_config::SYSTEM) {
        return Err(msg!(
            "control.name_is_builtin", name = name =>
            "`{name}` is the name of a built-in choice; use a different one."
        ));
    }
    let addr = input.addr.trim().to_string();
    let port_ok = addr
        .rsplit_once(':')
        .is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok_and(|p| p > 0));
    if !port_ok {
        return Err(msg!(
            "control.proxy_addr_form", addr = addr =>
            "The proxy address `{addr}` is written as host:port."
        ));
    }
    let auth = match &input.auth {
        // 没动认证：沿用原来那一份，**原值不经过界面**
        tw_api::ProxyAuthInput::Keep => existing.and_then(|e| e.auth.clone()),
        tw_api::ProxyAuthInput::None => None,
        tw_api::ProxyAuthInput::Set { user, pass } => {
            let user = user.trim();
            if user.is_empty() {
                return Err(msg!("control.user_empty" => "The user name cannot be empty."));
            }
            // `${` 在配置里表示「从环境变量读」
            if pass.contains("${") {
                return Err(
                    msg!("control.pass_has_expansion" => "A password cannot contain `${{`."),
                );
            }
            Some(tw_config::proxy::ProxyAuth {
                user: user.to_string(),
                pass: tw_config::Secret::new(pass.clone()),
            })
        }
    };
    Ok(tw_config::Proxy {
        name,
        kind: slug("proxy kind", &input.kind)?,
        addr,
        auth,
    })
}

// ─────────────────────────────────────────────────────────── 共用

/// 名字得有、而且不能带首尾空白。`what` 是这种资源的英文名（`upstream`、
/// `proxy`…），进句子也进 `args` —— 界面照码说话时用它挑自己的词。
pub(crate) fn checked_name(raw: &str, what: &'static str) -> Result<String, Msg> {
    if raw.trim().is_empty() {
        return Err(msg!(
            "control.name_empty", kind = what => "An {kind} needs a name."
        ));
    }
    if raw.trim() != raw {
        return Err(msg!(
            "control.name_whitespace", kind = what =>
            "An {kind} name cannot start or end with whitespace."
        ));
    }
    Ok(raw.to_string())
}

/// 界面上的一个选项值 → 配置里的枚举。**和 YAML 里写的词是同一套**。
fn slug<T: serde::de::DeserializeOwned>(what: &'static str, v: &str) -> Result<T, Msg> {
    serde_yaml_ng::from_value(Value::String(v.to_string())).map_err(|_| {
        msg!(
            "control.unsupported_value", kind = what, value = v =>
            "`{value}` is not a {kind} we support."
        )
    })
}

/// 结构 → YAML 映射。**失败是我们自己的类型写坏了**，不是用户填错了什么
pub(crate) fn mapping<T: serde::Serialize>(v: &T) -> Result<serde_yaml_ng::Mapping, ApplyError> {
    let unwritable = |d: String| ApplyError::Edit(EditError::Unwritable(d));
    match serde_yaml_ng::to_value(v) {
        Ok(Value::Mapping(m)) => Ok(m),
        Ok(_) => Err(unwritable("it does not serialize to a mapping".to_string())),
        Err(e) => Err(unwritable(e.to_string())),
    }
}

/// 交过来的东西写得不对。那句话自己带码，原样发给界面
pub(crate) fn invalid(m: Msg) -> ApplyError {
    ApplyError::Invalid(m)
}

pub(crate) fn not_found(what: &'static str, name: &str) -> ApplyError {
    ApplyError::Edit(EditError::NotFound {
        what,
        name: name.to_string(),
    })
}

/// `` `relay-hk`, `relay-sg` ``
pub(crate) fn quoted(names: &[String]) -> String {
    names
        .iter()
        .map(|n| format!("`{n}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// 「路由「默认」的规则「长上下文」、策略组「pool」」
fn describe(refs: &[ProviderRef]) -> String {
    refs.iter()
        .map(|r| match r {
            ProviderRef::RuleTarget { route, rule }
            | ProviderRef::RuleCondition { route, rule } => {
                format!("rule `{rule}` of route `{route}`")
            }
            ProviderRef::Group { group } => format!("group `{group}`"),
        })
        .collect::<Vec<_>>()
        .join(", ")
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
