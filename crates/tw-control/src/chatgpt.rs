//! ChatGPT 账号：浏览器登录、用量、额度重置卡。
//!
//! # 登录
//!
//! 在浏览器里完成 OAuth 授权（PKCE）。**回调只能回本机的 1455 或 1457 端口**：那是登记在
//! Codex 客户端上的回调地址，OpenAI 不开放第三方登记自己的地址。core 在其中一个端口上等
//! 回调，用授权码换 token，把上游写进 config.yaml —— refresh token、access token、过期时间和
//! 账户 ID 都在配置里，和别的上游一起管理。桌面版只负责打开浏览器。
//!
//! **同一时刻只有一次登录**：回调端口只有一个。新的登录开始时，还没完成的那次作废。
//!
//! # 额度重置卡
//!
//! 用掉就回不来，**只在用户明确点下去时用**。网关自己从不用，额度用完时只发通知。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::{get, post};
use serde_json::Value;
use tw_config::Protocol;
use tw_config::history::Origin;
use tw_gateway::chatgpt;

use crate::{ControlState, Fail, fail};

/// 登录要在多久之内完成。和 Codex 一样是 15 分钟
const LOGIN_TTL: Duration = Duration::from_secs(15 * 60);
/// 不指定名字时，上游叫这个
const DEFAULT_NAME: &str = "chatgpt";
/// 调 ChatGPT 后端接口（用量、重置卡）的超时
const API_TIMEOUT: Duration = Duration::from_secs(15);

pub fn router() -> axum::Router<ControlState> {
    axum::Router::new()
        .route("/chatgpt/login", post(start))
        .route("/chatgpt/login/{id}", get(status).delete(cancel))
        .route("/providers/{name}/chatgpt/usage", get(usage))
        .route(
            "/providers/{name}/chatgpt/resets",
            get(list_resets).post(use_reset),
        )
}

/// 登录要用的地址。**平时是 OpenAI 的**，测试里换成本机的假服务器
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// 登录页所在
    pub issuer: String,
    /// 换 token，也是写进上游配置的 token 端点
    pub token: String,
    /// 删除上游时吊销 refresh token
    pub revoke: String,
    /// 登录得来的上游写成这个接口地址
    pub backend: String,
    /// 依次尝试的回调端口。`0` 由系统挑一个空闲的
    pub ports: Vec<u16>,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            issuer: chatgpt::ISSUER.to_string(),
            token: chatgpt::TOKEN_ENDPOINT.to_string(),
            revoke: chatgpt::REVOKE_ENDPOINT.to_string(),
            backend: tw_config::chatgpt::BASE_URL.to_string(),
            ports: chatgpt::CALLBACK_PORTS.to_vec(),
        }
    }
}

/// 登录的状态。**进程里只有一份**（见模块文档）
#[derive(Default)]
pub struct Accounts {
    pub endpoints: Endpoints,
    current: Mutex<Option<Current>>,
}

impl Accounts {
    pub fn new(endpoints: Endpoints) -> Self {
        Self {
            endpoints,
            current: Mutex::new(None),
        }
    }

    fn set_status(&self, status: tw_api::ChatgptLoginStatus) {
        if let Ok(mut g) = self.current.lock()
            && let Some(c) = g.as_mut()
            && c.status.id == status.id
        {
            c.status = status;
        }
    }
}

struct Current {
    status: tw_api::ChatgptLoginStatus,
    /// 等回调的那个任务。取消、或者新的登录开始时停掉它
    task: tokio::task::JoinHandle<()>,
}

impl Current {
    /// 停掉等回调的任务，**等它真的停下**：端口要让给下一次登录
    async fn stop(self, s: &ControlState) {
        self.task.abort();
        let _ = self.task.await;
        if self.status.status == "pending" {
            announce(s, &self.status.id, "cancelled", None, None);
        }
    }
}

/// 一次登录要记住的东西。**PKCE 的 verifier 只在这里**，不出进程
struct Flow {
    s: ControlState,
    id: String,
    name: String,
    proxy: String,
    return_to: Option<String>,
    redirect_uri: String,
    verifier: String,
    state: String,
    /// 回调处理完之后给浏览器的那一页。**同一时刻只处理一个回调**：浏览器重试会带着
    /// 同一个授权码再来一次，那时直接给上次的结果
    outcome: tokio::sync::Mutex<Option<String>>,
    /// 回调处理完了（不论成败）
    settled: AtomicBool,
    /// 处理完之后停掉回调服务
    done: tokio::sync::Notify,
}

// ---------------------------------------------------------------- 登录

async fn start(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::ChatgptLoginStart>,
) -> Result<Json<tw_api::ChatgptLogin>, Fail> {
    let name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(DEFAULT_NAME)
        .to_string();
    let proxy = req
        .proxy
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or(tw_config::DIRECT)
        .to_string();
    let cfg = s.config();
    if proxy != tw_config::DIRECT
        && proxy != tw_config::SYSTEM
        && !cfg.proxies.iter().any(|p| p.name == proxy)
    {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            format!("代理「{proxy}」不存在"),
        ));
    }
    if let Some(p) = cfg.providers.iter().find(|p| p.name == name)
        && p.effective_protocol() != Some(Protocol::Chatgpt)
    {
        return Err(fail(
            StatusCode::CONFLICT,
            format!("已有名为「{name}」的上游，且不是 ChatGPT 账号上游，请使用其他名称"),
        ));
    }
    let return_to = match req
        .return_to
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        Some(v) if app_link(v) => Some(v.to_string()),
        Some(_) => {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "登录完成后的跳转地址只能使用应用自己的协议，不能是网页地址",
            ));
        }
        None => None,
    };

    // 还没完成的上一次登录作废：回调端口要让出来
    let prev = s.chatgpt.current.lock().ok().and_then(|mut g| g.take());
    if let Some(prev) = prev {
        prev.stop(&s).await;
    }
    let (listener, port) = bind_callback(&s.chatgpt.endpoints.ports)
        .await
        .ok_or_else(|| {
            let ports = s
                .chatgpt
                .endpoints
                .ports
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join("、");
            fail(
                StatusCode::CONFLICT,
                format!("登录回调端口 {ports} 均被占用。如果 Codex 正在登录，请先完成或关闭它"),
            )
        })?;

    let pkce = chatgpt::Pkce::new();
    let redirect_uri = chatgpt::redirect_uri(port);
    let state = chatgpt::new_state();
    let authorize_url =
        chatgpt::authorize_url(&s.chatgpt.endpoints.issuer, &redirect_uri, &pkce, &state);
    let id = chatgpt::new_state();
    let flow = Arc::new(Flow {
        s: s.clone(),
        id: id.clone(),
        name,
        proxy,
        return_to,
        redirect_uri,
        verifier: pkce.verifier,
        state,
        outcome: tokio::sync::Mutex::new(None),
        settled: AtomicBool::new(false),
        done: tokio::sync::Notify::new(),
    });
    let current = Current {
        status: tw_api::ChatgptLoginStatus {
            id: id.clone(),
            status: "pending".into(),
            provider: None,
            plan: None,
            error: None,
        },
        task: tokio::spawn(serve_callback(flow, listener)),
    };
    // 两个登录同时开始时，后到的算数
    let raced = match s.chatgpt.current.lock() {
        Ok(mut g) => g.replace(current),
        Err(_) => Some(current),
    };
    if let Some(prev) = raced {
        prev.stop(&s).await;
    }
    Ok(Json(tw_api::ChatgptLogin {
        id,
        authorize_url,
        expires_in_secs: LOGIN_TTL.as_secs(),
    }))
}

async fn status(
    State(s): State<ControlState>,
    Path(id): Path<String>,
) -> Result<Json<tw_api::ChatgptLoginStatus>, Fail> {
    s.chatgpt
        .current
        .lock()
        .ok()
        .and_then(|g| {
            g.as_ref()
                .filter(|c| c.status.id == id)
                .map(|c| c.status.clone())
        })
        .map(Json)
        .ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                "没有这次登录，或者它已被新的登录替代",
            )
        })
}

async fn cancel(
    State(s): State<ControlState>,
    Path(id): Path<String>,
) -> Result<Json<tw_api::ChatgptLoginStatus>, Fail> {
    let mut status = {
        let g = s
            .chatgpt
            .current
            .lock()
            .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        let c = g.as_ref().filter(|c| c.status.id == id).ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                "没有这次登录，或者它已被新的登录替代",
            )
        })?;
        if c.status.status != "pending" {
            return Ok(Json(c.status.clone()));
        }
        // 不用等它停下：端口晚一点让出来也没关系，下一次登录会等
        c.task.abort();
        c.status.clone()
    };
    status.status = "cancelled".into();
    s.chatgpt.set_status(status.clone());
    announce(&s, &id, "cancelled", None, None);
    Ok(Json(status))
}

/// 依次试回调端口。**刚取消的上一次登录可能还没把端口让出来**，每个端口多等几次
async fn bind_callback(ports: &[u16]) -> Option<(tokio::net::TcpListener, u16)> {
    for attempt in 0..5 {
        for port in ports {
            if let Ok(l) = tokio::net::TcpListener::bind(("127.0.0.1", *port)).await
                && let Ok(addr) = l.local_addr()
            {
                return Some((l, addr.port()));
            }
        }
        if attempt < 4 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    None
}

/// 等浏览器回调。处理完或者 15 分钟过去就停
async fn serve_callback(flow: Arc<Flow>, listener: tokio::net::TcpListener) {
    let app = axum::Router::new()
        .route("/auth/callback", get(callback))
        .with_state(flow.clone());
    let stop = flow.clone();
    // **到点也是优雅停止**：正在换 token 的那个回调要让它做完，不然换到手的凭据没写进配置就丢了
    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        tokio::select! {
            _ = stop.done.notified() => {}
            _ = tokio::time::sleep(LOGIN_TTL) => {}
        }
    });
    // 一个一直不发完请求的连接会让优雅停止等下去，再给一分钟就不等了
    let _ = tokio::time::timeout(LOGIN_TTL + Duration::from_secs(60), serve).await;
    if !flow.settled.load(Ordering::SeqCst) {
        flow.s.chatgpt.set_status(tw_api::ChatgptLoginStatus {
            id: flow.id.clone(),
            status: "expired".into(),
            provider: None,
            plan: None,
            error: Some("15 分钟内没有完成授权".into()),
        });
        announce(&flow.s, &flow.id, "expired", None, None);
    }
}

async fn callback(
    State(flow): State<Arc<Flow>>,
    Query(q): Query<HashMap<String, String>>,
) -> Html<String> {
    // **state 对不上就当没来过**：可能是别的网页在试探这个端口，不能让它终止这次登录
    if q.get("state") != Some(&flow.state) {
        return Html(page(
            "登录链接已失效",
            "请回到 ThinkWatch 重新发起登录。",
            None,
        ));
    }
    let mut outcome = flow.outcome.lock().await;
    if let Some(html) = outcome.as_ref() {
        return Html(html.clone());
    }
    let result = match (q.get("error"), q.get("code").filter(|c| !c.is_empty())) {
        (Some(e), _) => {
            let detail = q.get("error_description").unwrap_or(e);
            Err(format!(
                "授权页返回错误：{}",
                detail.chars().take(200).collect::<String>()
            ))
        }
        (None, Some(code)) => complete(&flow, code).await,
        (None, None) => Err("回调中缺少授权码".to_string()),
    };
    let html = match result {
        Ok((provider, plan)) => {
            flow.s.chatgpt.set_status(tw_api::ChatgptLoginStatus {
                id: flow.id.clone(),
                status: "done".into(),
                provider: Some(provider.clone()),
                plan,
                error: None,
            });
            announce(&flow.s, &flow.id, "done", Some(provider), None);
            page(
                "ChatGPT 登录已完成",
                "此页可以关闭。",
                flow.return_to.as_deref(),
            )
        }
        Err(why) => {
            tracing::warn!("ChatGPT 登录未完成：{why}");
            let html = page(
                "ChatGPT 登录未完成",
                &format!("{why}。请回到 ThinkWatch 重新发起登录。"),
                flow.return_to.as_deref(),
            );
            flow.s.chatgpt.set_status(tw_api::ChatgptLoginStatus {
                id: flow.id.clone(),
                status: "failed".into(),
                provider: None,
                plan: None,
                error: Some(why.clone()),
            });
            announce(&flow.s, &flow.id, "failed", None, Some(why));
            html
        }
    };
    *outcome = Some(html.clone());
    flow.settled.store(true, Ordering::SeqCst);
    // 这一页先发出去，服务再停
    flow.done.notify_one();
    Html(html)
}

/// 用授权码换 token，写进配置。返回上游名和套餐。
async fn complete(flow: &Flow, code: &str) -> Result<(String, Option<String>), String> {
    let s = &flow.s;
    let endpoints = &s.chatgpt.endpoints;
    // 换 token 走选定的出站方式：需要代理才能访问 OpenAI 的用户，直连会卡在这一步
    let route = tw_config::Provider {
        name: flow.name.clone(),
        base_url: endpoints.backend.clone(),
        proxy: flow.proxy.clone(),
        ..Default::default()
    };
    let http = tw_gateway::client_for_provider(&s.config(), &route).map_err(|e| e.message)?;
    let tokens = chatgpt::exchange_code(
        &http,
        &endpoints.token,
        code,
        &flow.verifier,
        &flow.redirect_uri,
    )
    .await?;
    let account = chatgpt::account(&tokens.id_token);
    let oauth = tw_config::OAuth {
        access: Some(tokens.access.clone()),
        expires_at: tokens.expires_in.map(|secs| {
            (chrono::Utc::now() + chrono::Duration::seconds(secs.min(i64::MAX as u64) as i64))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        }),
        refresh: tokens.refresh.clone(),
        endpoint: endpoints.token.clone(),
        client_id: Some(chatgpt::CLIENT_ID.to_string()),
        client_secret: None,
        refresh_before: None,
    };
    let name = flow.name.clone();
    let proxy = flow.proxy.clone();
    let backend = endpoints.backend.clone();
    let account_id = account.account_id.clone();
    s.cfg
        .transform(None, Origin::Ui, |text, cfg| {
            let existing = cfg.providers.iter().find(|p| p.name == name);
            if let Some(e) = existing
                && e.effective_protocol() != Some(Protocol::Chatgpt)
            {
                return Err(crate::resources::invalid(format!(
                    "已有名为「{name}」的上游，且不是 ChatGPT 账号上游"
                )));
            }
            let mut p = existing.cloned().unwrap_or_else(|| tw_config::Provider {
                name: name.clone(),
                base_url: backend.clone(),
                protocol: Some(Protocol::Chatgpt),
                proxy: proxy.clone(),
                billing: Some(tw_config::Billing::Subscription),
                ..Default::default()
            });
            // 重新登录：换掉凭据和账户 ID，其余设置（出站方式、模型范围、停用…）不动
            p.key = None;
            p.oauth = Some(oauth.clone());
            let mut headers: Vec<tw_config::Header> = p
                .headers
                .iter()
                .filter(|h| !h.name.eq_ignore_ascii_case(chatgpt::ACCOUNT_HEADER))
                .cloned()
                .collect();
            if let Some(id) = &account_id {
                headers.push(tw_config::Header {
                    name: chatgpt::ACCOUNT_HEADER.to_string(),
                    value: tw_config::Secret::new(id.clone()),
                });
            }
            p.headers = tw_config::Headers::new(headers);
            p.check_credential()
                .map_err(|e| crate::resources::invalid(e.to_string()))?;
            let current = existing.map(|_| name.as_str());
            Ok(tw_config::edit::upsert(
                text,
                tw_config::edit::PROVIDERS,
                current,
                &crate::resources::mapping(&p)?,
            )?)
        })
        .await
        .map_err(|e| format!("无法写入配置：{e}"))?;
    // 模型清单现在就问一次：不然要等到下一轮定时刷新，新上游的模型才出现
    let gateway = s.gateway.clone();
    let provider = flow.name.clone();
    tokio::spawn(async move {
        tw_gateway::models::refresh_one(&gateway, &provider).await;
    });
    Ok((flow.name.clone(), account.plan))
}

fn announce(
    s: &ControlState,
    login: &str,
    status: &str,
    provider: Option<String>,
    error: Option<String>,
) {
    s.bus().emit(tw_api::Event::LoginFinished {
        id: s.bus().next_id(),
        login: login.to_string(),
        status: status.to_string(),
        provider,
        error,
        at_ms: now_ms(),
    });
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 是不是应用自己的协议（`thinkwatch://…`）。**网页地址不行**：跳转地址是调用方给的，
/// 放行网页地址就是一个开放跳转
fn app_link(v: &str) -> bool {
    let Some((scheme, rest)) = v.split_once(':') else {
        return false;
    };
    let scheme = scheme.to_ascii_lowercase();
    let well_formed = scheme
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c));
    let web = matches!(
        scheme.as_str(),
        "http"
            | "https"
            | "javascript"
            | "data"
            | "file"
            | "blob"
            | "about"
            | "vbscript"
            | "ftp"
            | "ws"
            | "wss"
    );
    well_formed && !web && !rest.is_empty() && v.len() <= 512 && !v.chars().any(|c| c.is_control())
}

/// 回调之后浏览器里显示的那一页
fn page(title: &str, detail: &str, return_to: Option<&str>) -> String {
    let (redirect, link) = match return_to {
        Some(url) => {
            let u = escape(url);
            (
                format!(r#"<meta http-equiv="refresh" content="0;url={u}">"#),
                format!(r#"<p><a href="{u}">返回 ThinkWatch</a></p>"#),
            )
        }
        None => (String::new(), String::new()),
    };
    format!(
        r#"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>ThinkWatch</title>{redirect}<style>body{{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;font:15px -apple-system,BlinkMacSystemFont,"Segoe UI","PingFang SC","Microsoft YaHei",sans-serif;background:#f5f5f7;color:#1d1d1f}}main{{max-width:420px;padding:32px;text-align:center}}h1{{margin:0 0 8px;font-size:20px;font-weight:600}}p{{margin:8px 0 0;color:#6e6e73}}a{{color:#0066cc}}@media (prefers-color-scheme:dark){{body{{background:#1d1d1f;color:#f5f5f7}}p{{color:#a1a1a6}}a{{color:#2997ff}}}}</style></head><body><main><h1>{}</h1><p>{}</p>{link}</main></body></html>"#,
        escape(title),
        escape(detail)
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

// ---------------------------------------------------------------- 用量与重置卡

/// 调这个账号的 ChatGPT 后端接口（`wham/{path}`），读回 JSON。
///
/// **和转发一样，401 时换一个 access token 重发一次**：配置里那个可能已经在别处被吊销，
/// 不重发的话要等到下一个对话请求把它换掉，这里才恢复。用卡的请求重发也安全：401 说明
/// 没有被处理，而且带着同一个幂等键。
async fn account_call(
    s: &ControlState,
    name: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, Fail> {
    let cfg = s.config();
    let p = cfg
        .providers
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("上游「{name}」不存在")))?;
    if p.effective_protocol() != Some(Protocol::Chatgpt) {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            format!("上游「{name}」不是 ChatGPT 账号上游"),
        ));
    }
    let http = s.gateway.client_for(name);
    let url = chatgpt::wham_url(&p.base_url, path);
    let no_credential = |e: String| {
        fail(
            StatusCode::BAD_GATEWAY,
            format!("无法获取上游「{name}」的凭据：{e}"),
        )
    };
    let headers = s
        .gateway
        .headers_for(p, &http, None)
        .await
        .map_err(no_credential)?;
    let sent_at = std::time::Instant::now();
    let mut resp = send(&http, method.clone(), &url, &headers, body).await?;
    if resp.status() == StatusCode::UNAUTHORIZED
        && let Ok(fresh) = s.gateway.headers_after_401(p, &http, None, sent_at).await
        && fresh != headers
    {
        resp = send(&http, method, &url, &fresh, body).await?;
    }
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let body = tw_secret::mask_body(&text)
            .chars()
            .take(400)
            .collect::<String>();
        return Err(fail(
            StatusCode::BAD_GATEWAY,
            format!("ChatGPT 后端返回 {}：{body}", status.as_u16()),
        ));
    }
    serde_json::from_str(&text)
        .map_err(|_| fail(StatusCode::BAD_GATEWAY, "ChatGPT 后端的响应不是 JSON"))
}

/// 发一次。**这几个接口也如实报身份**；它们不是流式对话，不带会话 ID 和流式的 accept
async fn send(
    http: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    headers: &[(String, String)],
    body: Option<&Value>,
) -> Result<reqwest::Response, Fail> {
    let identity: Vec<(String, String)> =
        chatgpt::identity_headers(&axum::http::HeaderMap::new(), headers)
            .into_iter()
            .filter(|(k, _)| k != "accept" && k != "session-id")
            .collect();
    let mut req = http.request(method, url).timeout(API_TIMEOUT);
    req = tw_gateway::forward::apply_headers(req, headers);
    req = tw_gateway::forward::apply_headers(req, &identity);
    if let Some(b) = body {
        req = req.json(b);
    }
    req.send().await.map_err(|e| {
        fail(
            StatusCode::BAD_GATEWAY,
            format!(
                "无法连接 ChatGPT 后端：{}",
                tw_gateway::forward::map_reqwest_error(e).message
            ),
        )
    })
}

async fn usage(
    State(s): State<ControlState>,
    Path(name): Path<String>,
) -> Result<Json<tw_api::ChatgptUsage>, Fail> {
    let v = account_call(&s, &name, reqwest::Method::GET, "usage", None).await?;
    Ok(Json(parse_usage(&v)))
}

/// `wham/usage` 的回答。**只取额度相关的几项**：同一份回答里还有邮箱和用户 ID，不往外带
fn parse_usage(v: &Value) -> tw_api::ChatgptUsage {
    let window = |w: &Value| {
        let secs = w.get("limit_window_seconds")?.as_u64().filter(|s| *s > 0)?;
        let used = w.get("used_percent")?.as_f64()?.clamp(0.0, 100.0);
        Some(tw_api::QuotaWindow {
            window: tw_gateway::quota::codex_window(secs / 60),
            used_percent: used,
            reset_in_secs: w.get("reset_after_seconds").and_then(|x| x.as_u64()),
            status: (used >= 100.0).then(|| "rejected".to_string()),
        })
    };
    let limits = &v["rate_limit"];
    tw_api::ChatgptUsage {
        plan: v["plan_type"].as_str().map(str::to_string),
        windows: [&limits["primary_window"], &limits["secondary_window"]]
            .into_iter()
            .filter_map(window)
            .collect(),
        reset_credits: v["rate_limit_reset_credits"]["available_count"].as_i64(),
    }
}

async fn list_resets(
    State(s): State<ControlState>,
    Path(name): Path<String>,
) -> Result<Json<tw_api::ResetCredits>, Fail> {
    let v = account_call(
        &s,
        &name,
        reqwest::Method::GET,
        "rate-limit-reset-credits",
        None,
    )
    .await?;
    serde_json::from_value(v)
        .map(Json)
        .map_err(|e| fail(StatusCode::BAD_GATEWAY, format!("无法识别重置卡清单：{e}")))
}

async fn use_reset(
    State(s): State<ControlState>,
    Path(name): Path<String>,
    Json(req): Json<tw_api::ResetCreditUse>,
) -> Result<Json<tw_api::ResetCreditUsed>, Fail> {
    let key = req.idempotency_key.trim();
    if key.is_empty() || key.len() > 128 || key.chars().any(|c| c.is_control()) {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "幂等键不能为空，且不超过 128 个字符",
        ));
    }
    let mut body = serde_json::json!({ "redeem_request_id": key });
    if let Some(id) = req
        .credit_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        body["credit_id"] = Value::from(id);
    }
    let v = account_call(
        &s,
        &name,
        reqwest::Method::POST,
        "rate-limit-reset-credits/consume",
        Some(&body),
    )
    .await?;
    let code = v["code"]
        .as_str()
        .ok_or_else(|| fail(StatusCode::BAD_GATEWAY, "无法识别使用重置卡的结果"))?
        .to_ascii_lowercase();
    tracing::info!(provider = %name, %code, "已请求使用额度重置卡");
    Ok(Json(tw_api::ResetCreditUsed {
        code,
        windows_reset: v["windows_reset"].as_i64().unwrap_or(0),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_app_links_are_accepted_as_the_return_address() {
        assert!(app_link("thinkwatch://login/done"));
        assert!(app_link("thinkwatch-lite:chatgpt"));
        for bad in [
            "https://evil.example/phish",
            "http://localhost:1420/",
            "javascript:alert(1)",
            "data:text/html,hi",
            "file:///etc/passwd",
            "thinkwatch:",
            "not a url",
            "1bad://x",
        ] {
            assert!(!app_link(bad), "{bad}");
        }
    }

    #[test]
    fn the_callback_page_escapes_what_it_shows() {
        let html = page("标题", "<script>x</script>", Some("thinkwatch://a\"b"));
        assert!(!html.contains("<script>x"), "{html}");
        assert!(html.contains("thinkwatch://a&quot;b"), "{html}");
        assert!(!page("t", "d", None).contains("http-equiv"));
    }

    #[test]
    fn usage_keeps_only_the_limits() {
        let v = serde_json::json!({
            "email": "someone@example.com",
            "user_id": "user-1",
            "plan_type": "plus",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {"used_percent": 21, "limit_window_seconds": 604800, "reset_after_seconds": 410907},
                "secondary_window": null
            },
            "rate_limit_reset_credits": {"available_count": 2}
        });
        let u = parse_usage(&v);
        assert_eq!(u.plan.as_deref(), Some("plus"));
        assert_eq!(u.windows.len(), 1);
        assert_eq!(u.windows[0].window, "weekly");
        assert_eq!(u.windows[0].used_percent, 21.0);
        assert_eq!(u.windows[0].reset_in_secs, Some(410907));
        assert_eq!(u.reset_credits, Some(2));
        let json = serde_json::to_string(&u).unwrap();
        assert!(
            !json.contains("someone@example.com") && !json.contains("user-1"),
            "{json}"
        );
    }
}
