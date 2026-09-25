//! ChatGPT 账号：登录、登的是谁、用量、额度重置卡。
//!
//! # 登录
//!
//! 两条路，都是 OAuth（PKCE），最后都落到同一处：用授权码换 token，把上游写进
//! config.yaml —— refresh token、access token、过期时间和账户 ID 都在配置里，和别的上游
//! 一起管理。
//!
//! - **在这台机器上开浏览器**（`browser`）。**回调只能回本机的 1455 或 1457 端口**：那是
//!   登记在 Codex 客户端上的回调地址，OpenAI 不开放第三方登记自己的地址。core 在其中一个
//!   端口上等回调，桌面版只负责打开浏览器。
//! - **去另一台设备上输码**（`device`）。这台机器上没有浏览器、或者浏览器登录不了的时候
//!   用。core 换一个一次性码，之后自己去问「批准了没有」。**不是所有账号都开放这条路**，
//!   服务端没开时换码直接 404，只能改用浏览器登录。
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
use axum::routing::get;
use serde_json::Value;
use tw_config::Protocol;
use tw_config::history::Origin;
use tw_gateway::chatgpt;

use crate::contract::RouterExt;
use crate::{ControlState, Fail, fail};
use tw_api::{LoginStatus, ep};
use tw_types::{Msg, msg};

/// 登录要在多久之内完成。和 Codex 一样是 15 分钟
const LOGIN_TTL: Duration = Duration::from_secs(15 * 60);
/// 设备码最快多久问一次「批准了没有」。服务端给的间隔比它还短就按它来 —— 问得再勤，
/// 除了被限流没有别的效果
const DEVICE_POLL_MIN: Duration = Duration::from_secs(3);
/// 不指定名字时，上游叫这个
const DEFAULT_NAME: &str = "chatgpt";
/// 调 ChatGPT 后端接口（用量、重置卡）的超时
const API_TIMEOUT: Duration = Duration::from_secs(15);

pub fn router() -> axum::Router<ControlState> {
    axum::Router::new()
        .at(ep::StartChatgptLogin, start)
        .at(ep::ChatgptLoginStatus, status)
        .at(ep::CancelChatgptLogin, cancel)
        .at(ep::ChatgptUsage, usage)
        .at(ep::ChatgptResets, list_resets)
        .at(ep::UseChatgptReset, use_reset)
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
        if self.status.status == LoginStatus::Pending {
            announce(s, &self.status.id, LoginStatus::Cancelled, None, None);
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

/// 在哪台设备上授权
enum Mode {
    /// 在这台机器上开浏览器
    Browser,
    /// 去另一台设备上输码
    Device,
}

/// 这次要登成什么样。两条登录路共用的那几项参数
struct Want {
    mode: Mode,
    /// 登录后写进配置的上游名
    name: String,
    /// 换 token 走哪条路
    proxy: String,
    /// 浏览器登录完成后跳回应用。设备码登录没有这一步
    return_to: Option<String>,
}

impl Want {
    /// 读一遍参数，有问题就直接回错
    fn read(s: &ControlState, req: &tw_api::ChatgptLoginStart) -> Result<Self, Fail> {
        let given = |v: &Option<String>| {
            v.as_deref()
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(str::to_string)
        };
        let mode = match req.mode {
            None | Some(tw_api::ChatgptLoginMode::Browser) => Mode::Browser,
            Some(tw_api::ChatgptLoginMode::Device) => Mode::Device,
        };
        let name = given(&req.name).unwrap_or_else(|| DEFAULT_NAME.to_string());
        let proxy = given(&req.proxy).unwrap_or_else(|| tw_config::DIRECT.to_string());
        let cfg = s.config();
        if proxy != tw_config::DIRECT
            && proxy != tw_config::SYSTEM
            && !cfg.proxies.iter().any(|p| p.name == proxy)
        {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                msg!("control.proxy_not_found", proxy = proxy => "There is no proxy named `{proxy}`."),
            ));
        }
        if let Some(p) = cfg.providers.iter().find(|p| p.name == name)
            && p.effective_protocol() != Some(Protocol::Chatgpt)
        {
            return Err(fail(
                StatusCode::CONFLICT,
                msg!(
                    "control.name_taken_not_chatgpt", name = &name =>
                    "There is already an upstream named `{name}`, and it is not a ChatGPT account. \
                     Use a different name."
                ),
            ));
        }
        let return_to = match given(&req.return_to) {
            Some(v) if app_link(&v) => Some(v),
            Some(_) => {
                return Err(fail(
                    StatusCode::BAD_REQUEST,
                    msg!(
                        "control.redirect_must_be_app_scheme" =>
                        "The address to return to after signing in has to use the app's own scheme, not a \
                         web address."
                    ),
                ));
            }
            None => None,
        };
        Ok(Self {
            mode,
            name,
            proxy,
            return_to,
        })
    }
}

async fn start(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::ChatgptLoginStart>,
) -> Result<Json<tw_api::ChatgptLogin>, Fail> {
    let want = Want::read(&s, &req)?;
    // 还没完成的上一次登录作废：回调端口要让出来
    let prev = s.chatgpt.current.lock().ok().and_then(|mut g| g.take());
    if let Some(prev) = prev {
        prev.stop(&s).await;
    }
    let (current, login) = match want.mode {
        Mode::Browser => start_browser(&s, want).await?,
        Mode::Device => start_device(&s, want).await?,
    };
    // 两个登录同时开始时，后到的算数
    let raced = match s.chatgpt.current.lock() {
        Ok(mut g) => g.replace(current),
        Err(_) => Some(current),
    };
    if let Some(prev) = raced {
        prev.stop(&s).await;
    }
    Ok(Json(login))
}

/// 在这台机器上开浏览器：占住一个回调端口，等浏览器带着授权码回来
async fn start_browser(
    s: &ControlState,
    want: Want,
) -> Result<(Current, tw_api::ChatgptLogin), Fail> {
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
                .join(", ");
            fail(
                StatusCode::CONFLICT,
                msg!(
                "control.callback_ports_busy", ports = ports =>
                "Every sign-in callback port ({ports}) is taken. If Codex is signing in, finish or \
                 close it first."
            ),
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
        name: want.name,
        proxy: want.proxy,
        return_to: want.return_to,
        redirect_uri,
        verifier: pkce.verifier,
        state,
        outcome: tokio::sync::Mutex::new(None),
        settled: AtomicBool::new(false),
        done: tokio::sync::Notify::new(),
    });
    Ok((
        Current {
            status: pending(&id),
            task: tokio::spawn(serve_callback(flow, listener)),
        },
        tw_api::ChatgptLogin {
            id,
            authorize_url: Some(authorize_url),
            user_code: None,
            verification_url: None,
            expires_in_secs: LOGIN_TTL.as_secs(),
        },
    ))
}

/// 去另一台设备上输码：先换一个一次性码，再自己去问「批准了没有」。
///
/// **PKCE 的两段都在服务端**：challenge 是它生成的，批准之后连 verifier 一起给我们，
/// 我们只负责原样带进换 token 那一步。
async fn start_device(
    s: &ControlState,
    want: Want,
) -> Result<(Current, tw_api::ChatgptLogin), Fail> {
    let issuer = s.chatgpt.endpoints.issuer.clone();
    let http =
        client_for(s, &want.name, &want.proxy).map_err(|e| fail(StatusCode::BAD_GATEWAY, e))?;
    let asked = serde_json::json!({ "client_id": chatgpt::CLIENT_ID });
    let resp = send(
        &http,
        reqwest::Method::POST,
        &chatgpt::device_code_url(&issuer),
        &[],
        Some(&asked),
    )
    .await?;
    let status = resp.status();
    if status == StatusCode::NOT_FOUND {
        return Err(fail(
            StatusCode::CONFLICT,
            msg!(
                "control.device_code_unavailable" =>
                "This account cannot sign in with a device code yet. Sign in on this computer \
                 instead."
            ),
        ));
    }
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(fail(
            StatusCode::BAD_GATEWAY,
            msg!(
                "control.device_code_failed", status = status.as_u16(), detail = brief(&text) =>
                "Asking for a device code answered {status}: {detail}"
            ),
        ));
    }
    let code: DeviceCode = serde_json::from_str(&text).map_err(|e| {
        fail(
            StatusCode::BAD_GATEWAY,
            msg!(
                "control.device_code_unreadable", detail = e =>
                "The answer to the device code request could not be read: {detail}"
            ),
        )
    })?;
    let id = chatgpt::new_state();
    let user_code = code.user_code.clone();
    Ok((
        Current {
            status: pending(&id),
            task: tokio::spawn(await_device(s.clone(), id.clone(), want, http, code)),
        },
        tw_api::ChatgptLogin {
            id,
            authorize_url: None,
            user_code: Some(user_code),
            verification_url: Some(chatgpt::device_page_url(&issuer)),
            expires_in_secs: LOGIN_TTL.as_secs(),
        },
    ))
}

fn pending(id: &str) -> tw_api::ChatgptLoginStatus {
    tw_api::ChatgptLoginStatus {
        id: id.to_string(),
        status: LoginStatus::Pending,
        provider: None,
        plan: None,
        error: None,
    }
}

/// 换来的设备码
#[derive(Debug, Clone, serde::Deserialize)]
struct DeviceCode {
    /// 问「批准了没有」时要带上它
    device_auth_id: String,
    /// 给用户看、让他输进去的码
    user_code: String,
    /// 服务端让我们隔多少秒问一次
    #[serde(default)]
    interval: u64,
}

/// 用户批准之后拿到的东西。verifier 是服务端生成的，原样带进换 token
#[derive(Debug, serde::Deserialize)]
struct Approved {
    authorization_code: String,
    code_verifier: String,
}

/// 等用户在另一台设备上批准。**403 和 404 都是「还没批」**，一直问到 15 分钟为止
async fn await_device(
    s: ControlState,
    id: String,
    want: Want,
    http: reqwest::Client,
    code: DeviceCode,
) {
    let issuer = s.chatgpt.endpoints.issuer.clone();
    let url = chatgpt::device_token_url(&issuer);
    let asked = serde_json::json!({
        "device_auth_id": code.device_auth_id,
        "user_code": code.user_code,
    });
    let every = Duration::from_secs(code.interval).max(DEVICE_POLL_MIN);
    let deadline = tokio::time::Instant::now() + LOGIN_TTL;
    while tokio::time::Instant::now() < deadline {
        let resp = match send(&http, reqwest::Method::POST, &url, &[], Some(&asked)).await {
            Ok(r) => r,
            Err((_, why)) => return settle(&s, &id, Err(why.0)),
        };
        let status = resp.status();
        // 还没批：接着等
        if status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND {
            tokio::time::sleep(every).await;
            continue;
        }
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return settle(
                &s,
                &id,
                Err(msg!(
                    "control.chatgpt_login.device_status",
                    status = status.as_u16(), body = brief(&text) =>
                    "Signing in answered {status}: {body}"
                )),
            );
        }
        let done = match serde_json::from_str::<Approved>(&text) {
            Ok(a) => {
                exchange_and_save(
                    &s,
                    &want.name,
                    &want.proxy,
                    &a.authorization_code,
                    &a.code_verifier,
                    &chatgpt::device_redirect_uri(&issuer),
                )
                .await
            }
            Err(e) => Err(msg!(
                "control.chatgpt_login.approval_unreadable", detail = e =>
                "The approval response could not be read: {detail}"
            )),
        };
        return settle(&s, &id, done);
    }
    s.chatgpt.set_status(tw_api::ChatgptLoginStatus {
        id: id.clone(),
        status: LoginStatus::Expired,
        provider: None,
        plan: None,
        error: Some(not_in_time()),
    });
    announce(&s, &id, LoginStatus::Expired, None, None);
}

/// 十五分钟里没有完成授权
fn not_in_time() -> Msg {
    msg!(
        "control.chatgpt_login.expired" =>
        "The authorization was not completed within 15 minutes."
    )
}

/// 记下登录的结果，并告诉界面一声
fn settle(s: &ControlState, id: &str, done: Result<(String, Option<tw_api::ChatgptPlan>), Msg>) {
    let status = match done {
        Ok((provider, plan)) => tw_api::ChatgptLoginStatus {
            id: id.to_string(),
            status: LoginStatus::Done,
            provider: Some(provider),
            plan,
            error: None,
        },
        Err(why) => {
            tracing::warn!("the ChatGPT sign-in did not finish: {why}");
            tw_api::ChatgptLoginStatus {
                id: id.to_string(),
                status: LoginStatus::Failed,
                provider: None,
                plan: None,
                error: Some(why),
            }
        }
    };
    s.chatgpt.set_status(status.clone());
    announce(s, id, status.status, status.provider, status.error);
}

/// 出错时给用户看的那一小段响应
fn brief(text: &str) -> String {
    tw_secret::mask_body(text).chars().take(200).collect()
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
                msg!(
                    "control.signin_gone" =>
                    "There is no such sign-in, or a newer one replaced it."
                ),
            )
        })
}

async fn cancel(
    State(s): State<ControlState>,
    Path(id): Path<String>,
) -> Result<Json<tw_api::ChatgptLoginStatus>, Fail> {
    let mut status = {
        let g = s.chatgpt.current.lock().map_err(crate::internal)?;
        let c = g.as_ref().filter(|c| c.status.id == id).ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                msg!(
                    "control.signin_gone" =>
                    "There is no such sign-in, or a newer one replaced it."
                ),
            )
        })?;
        if c.status.status != LoginStatus::Pending {
            return Ok(Json(c.status.clone()));
        }
        // 不用等它停下：端口晚一点让出来也没关系，下一次登录会等
        c.task.abort();
        c.status.clone()
    };
    status.status = LoginStatus::Cancelled;
    s.chatgpt.set_status(status.clone());
    announce(&s, &id, LoginStatus::Cancelled, None, None);
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
            status: LoginStatus::Expired,
            provider: None,
            plan: None,
            error: Some(not_in_time()),
        });
        announce(&flow.s, &flow.id, LoginStatus::Expired, None, None);
    }
}

async fn callback(
    State(flow): State<Arc<Flow>>,
    Query(q): Query<HashMap<String, String>>,
) -> Html<String> {
    // **state 对不上就当没来过**：可能是别的网页在试探这个端口，不能让它终止这次登录
    if q.get("state") != Some(&flow.state) {
        return Html(page(
            "This sign-in link has expired",
            "Go back to ThinkWatch and start again.",
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
            Err(msg!(
                "control.chatgpt_login.page_error",
                detail = detail.chars().take(200).collect::<String>() =>
                "The authorization page returned an error: {detail}"
            ))
        }
        (None, Some(code)) => {
            exchange_and_save(
                &flow.s,
                &flow.name,
                &flow.proxy,
                code,
                &flow.verifier,
                &flow.redirect_uri,
            )
            .await
        }
        (None, None) => Err(msg!(
            "control.chatgpt_login.no_code" =>
            "The callback carried no authorization code."
        )),
    };
    let html = match &result {
        Ok(_) => page(
            "Signed in to ChatGPT",
            "This page can be closed.",
            flow.return_to.as_deref(),
        ),
        Err(why) => page(
            "The ChatGPT sign-in did not finish",
            &format!("{} Go back to ThinkWatch and start again.", why.text),
            flow.return_to.as_deref(),
        ),
    };
    settle(&flow.s, &flow.id, result);
    *outcome = Some(html.clone());
    flow.settled.store(true, Ordering::SeqCst);
    // 这一页先发出去，服务再停
    flow.done.notify_one();
    Html(html)
}

/// 用授权码换 token，写进配置。返回上游名和套餐。
async fn exchange_and_save(
    s: &ControlState,
    name: &str,
    proxy: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<(String, Option<tw_api::ChatgptPlan>), Msg> {
    let endpoints = &s.chatgpt.endpoints;
    let http = client_for(s, name, proxy)?;
    let tokens =
        chatgpt::exchange_code(&http, &endpoints.token, code, verifier, redirect_uri).await?;
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
    let name = name.to_string();
    let proxy = proxy.to_string();
    let backend = endpoints.backend.clone();
    let account_id = account.account_id.clone();
    s.cfg
        .transform(None, Origin::Ui, |text, cfg| {
            let existing = cfg.providers.iter().find(|p| p.name == name);
            if let Some(e) = existing
                && e.effective_protocol() != Some(Protocol::Chatgpt)
            {
                return Err(crate::resources::invalid(msg!(
                    "control.name_taken_not_chatgpt", name = &name =>
                    "There is already an upstream named `{name}`, and it is not a ChatGPT account. \
                     Use a different name."
                )));
            }
            let mut p = existing.cloned().unwrap_or_else(|| tw_config::Provider {
                name: name.clone(),
                base_url: backend.clone(),
                protocol: Some(Protocol::Chatgpt),
                proxy: proxy.clone(),
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
                .map_err(|e| crate::resources::invalid(e.msg()))?;
            let current = existing.map(|_| name.as_str());
            Ok(tw_config::edit::upsert(
                text,
                tw_config::edit::PROVIDERS,
                current,
                &crate::resources::mapping(&p)?,
            )?)
        })
        .await
        .map_err(|e| e.msg())?;
    // 模型清单现在就问一次：不然要等到下一轮定时刷新，新上游的模型才出现
    let gateway = s.gateway.clone();
    let provider = name.clone();
    tokio::spawn(async move {
        tw_gateway::models::refresh_one(&gateway, &provider).await;
    });
    Ok((name, account.plan))
}

/// 这个上游登的是哪个账号：从它在用的 access token 里读（见 [`tw_api::AccountView`]）。
///
/// **不联网。**界面每画一次上游列表都要它；为了给一行上游标上账号去问一次后端，就是
/// 每打开一次上游页、每个账号一次真实调用
pub(crate) fn account_view(
    s: &ControlState,
    p: &tw_config::Provider,
    o: &tw_config::OAuth,
) -> Option<tw_api::AccountView> {
    if p.effective_protocol() != Some(Protocol::Chatgpt) {
        return None;
    }
    let token = s.gateway.oauth.access_in_use(&p.name, o)?;
    chatgpt::account(&token).view()
}

/// 按出站方式建一个客户端。**要代理才能访问 OpenAI 的用户，直连会卡在换 token 这一步**
fn client_for(s: &ControlState, name: &str, proxy: &str) -> Result<reqwest::Client, Msg> {
    let route = tw_config::Provider {
        name: name.to_string(),
        base_url: s.chatgpt.endpoints.backend.clone(),
        proxy: proxy.to_string(),
        ..Default::default()
    };
    // 代理没定义、密码读不到这些，数据面那句自己带码
    tw_gateway::client_for_provider(&s.config(), &route).map_err(|e| e.detail)
}

fn announce(
    s: &ControlState,
    login: &str,
    status: LoginStatus,
    provider: Option<String>,
    error: Option<Msg>,
) {
    s.bus().emit(tw_api::Event::LoginFinished {
        id: s.bus().next_id(),
        login: login.to_string(),
        status,
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
                format!(r#"<p><a href="{u}">Back to ThinkWatch</a></p>"#),
            )
        }
        None => (String::new(), String::new()),
    };
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>ThinkWatch</title>{redirect}<style>body{{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;font:15px -apple-system,BlinkMacSystemFont,"Segoe UI","PingFang SC","Microsoft YaHei",sans-serif;background:#f5f5f7;color:#1d1d1f}}main{{max-width:420px;padding:32px;text-align:center}}h1{{margin:0 0 8px;font-size:20px;font-weight:600}}p{{margin:8px 0 0;color:#6e6e73}}a{{color:#0066cc}}@media (prefers-color-scheme:dark){{body{{background:#1d1d1f;color:#f5f5f7}}p{{color:#a1a1a6}}a{{color:#2997ff}}}}</style></head><body><main><h1>{}</h1><p>{}</p>{link}</main></body></html>"#,
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
        .ok_or_else(|| crate::no_such_upstream(name))?;
    if p.effective_protocol() != Some(Protocol::Chatgpt) {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            msg!(
                "control.not_a_chatgpt_account", upstream = name =>
                "Upstream `{upstream}` is not a ChatGPT account."
            ),
        ));
    }
    let http = s.gateway.client_for(name);
    let url = chatgpt::wham_url(&p.base_url, path);
    let no_credential = |e: Msg| {
        fail(
            StatusCode::BAD_GATEWAY,
            tw_gateway::credential_failed(e, name),
        )
    };
    let headers = s
        .gateway
        .headers_for(p, &http)
        .await
        .map_err(no_credential)?;
    let sent_at = std::time::Instant::now();
    let mut resp = send(&http, method.clone(), &url, &headers, body).await?;
    if resp.status() == StatusCode::UNAUTHORIZED
        && let Ok(fresh) = s.gateway.headers_after_401(p, &http, sent_at).await
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
            msg!(
                "control.chatgpt_backend_status", status = status.as_u16(), detail = body =>
                "The ChatGPT backend answered {status}: {detail}"
            ),
        ));
    }
    serde_json::from_str(&text)
        .map_err(|_| {
            fail(
                StatusCode::BAD_GATEWAY,
                msg!("control.chatgpt_backend_not_json" => "The ChatGPT backend's response is not JSON."),
            )
        })
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
        let why = tw_gateway::forward::map_reqwest_error(e);
        // 连不上的那句数据面自己带码，这里只补上「连的是 ChatGPT 后端」
        fail(
            StatusCode::BAD_GATEWAY,
            why.detail.in_context(
                "backend",
                "chatgpt",
                "The ChatGPT backend could not be reached",
            ),
        )
    })
}

async fn usage(
    State(s): State<ControlState>,
    Path(name): Path<String>,
) -> Result<Json<tw_api::ChatgptUsage>, Fail> {
    let v = account_call(&s, &name, reqwest::Method::GET, "usage", None).await?;
    let usage = parse_usage(&v, now_ms());
    // 问来的额度和响应头里读到的一样记下来：**额度只在内存里**，冷启动之后要等这个账号
    // 第一次有请求才会有，而界面一打开就该看得见
    s.gateway.record_quota(
        s.bus().next_id(),
        &name,
        tw_gateway::quota::Quota {
            windows: usage
                .windows
                .iter()
                .map(|w| tw_gateway::quota::Window {
                    window: w.window.clone(),
                    used_percent: w.used_percent,
                    resets_at_ms: w.resets_at_ms,
                    status: w.status.clone(),
                })
                .collect(),
        },
    );
    Ok(Json(usage))
}

/// `wham/usage` 的回答。**只取额度和重置卡**：同一份回答里的邮箱、套餐由上游视图从
/// 凭据里读（[`account_view`]），用户 ID 和账户 ID 不往外带 —— 界面用不上，而它们一旦
/// 出去就会进日志
///
/// `now_ms`：收到回答的时刻。「还有多少秒重置」要靠它换成时刻
fn parse_usage(v: &Value, now_ms: u64) -> tw_api::ChatgptUsage {
    let window = |w: &Value| {
        let secs = w.get("limit_window_seconds")?.as_u64().filter(|s| *s > 0)?;
        let used = w.get("used_percent")?.as_f64()?.clamp(0.0, 100.0);
        Some(tw_api::QuotaWindow {
            window: tw_gateway::quota::codex_window(secs / 60),
            used_percent: used,
            resets_at_ms: w
                .get("reset_after_seconds")
                .and_then(|x| x.as_u64())
                .map(|secs| now_ms.saturating_add(secs.saturating_mul(1000))),
            status: (used >= 100.0).then(|| "rejected".to_string()),
        })
    };
    let limits = &v["rate_limit"];
    tw_api::ChatgptUsage {
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
        .map_err(|e| {
            fail(
                StatusCode::BAD_GATEWAY,
                msg!("control.reset_cards_unreadable", detail = e => "The list of reset cards could not be read: {detail}"),
            )
        })
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
            msg!(
                "control.bad_idempotency_key" =>
                "An idempotency key cannot be empty, and cannot be longer than 128 characters."
            ),
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
        .ok_or_else(|| {
            fail(
                StatusCode::BAD_GATEWAY,
                msg!("control.reset_card_result_unreadable" => "The result of using the reset card could not be read."),
            )
        })?
        .to_ascii_lowercase();
    tracing::info!(provider = %name, %code, "asked to use a quota reset card");
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
    fn usage_keeps_the_limits_and_nothing_about_who() {
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
        let u = parse_usage(&v, 1_758_000_000_000);
        assert_eq!(u.windows.len(), 1);
        assert_eq!(u.windows[0].window, "weekly");
        assert_eq!(u.windows[0].used_percent, 21.0);
        // 「还有 410907 秒」换成了时刻：收到回答那一刻往后数
        assert_eq!(
            u.windows[0].resets_at_ms,
            Some(1_758_000_000_000 + 410_907_000)
        );
        assert_eq!(u.reset_credits, Some(2));
        // 登的是谁在上游视图里（从凭据读）；用户 ID 更不往外带
        let json = serde_json::to_string(&u).unwrap();
        for who in ["someone@example.com", "user-1", "plus"] {
            assert!(!json.contains(who), "{json}");
        }
    }
}
