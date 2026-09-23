//! Z.ai / BigModel 账号：登录换一把 API key。
//!
//! # 这条路为什么比 ChatGPT 那条短
//!
//! **登录的终点是账号下一把普通 API key**，不是一对会过期的 OAuth 令牌。Coding Plan
//! 的额度跟着 key 走 —— Z.ai 自己的客户端就是这么用的：套餐的每个请求只带这把 key，
//! 没有第二种凭据。所以写进 config.yaml 的是 `key`，不是 `oauth`：不需要刷新、不需要
//! refresh token 轮换，[`tw_gateway::oauth`] 那一层完全不参与。
//!
//! # 授权页上显示的是 Z.ai 自己的客户端
//!
//! Z.ai 不开放第三方注册 OAuth 应用，授权地址由它的平台接口下发、`client_id` 是它自己
//! 客户端的 —— 这一点和 ChatGPT 账号同源，界面上必须说明，不能让用户以为他授权的是
//! ThinkWatch。**我们能控制的那一半如实写**：请求一律带 ThinkWatch 的 User-Agent
//! （[`tw_gateway::user_agent`]），轮询凭据由我们自己生成，登录接口的请求里没有任何
//! 「我是谁」的字段可填，所以也没有一处需要冒充。
//!
//! # 授权完成后浏览器停在对方的页面上
//!
//! 回调回的是它平台自己的地址，不是本机 —— 所以没有回调服务、没有本机端口、也没有
//! 「跳回应用」。登录成了没成，只有轮询知道。
//!
//! # 它的业务接口用 HTTP 200 包业务错误码
//!
//! `{"code":401,"msg":"token expired or incorrect"}` 会带着 200 回来（实测）。只看状态
//! 码会把失败当成功，然后在后面某一步拿着一个空值报一句莫名其妙的错 —— 所以这里每一
//! 次调用都要看回包里的 `code`（见 [`data`]）。

use std::sync::Mutex;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde_json::{Value, json};
use tw_config::Protocol;
use tw_config::history::Origin;

use crate::{ControlState, Fail, fail};
use tw_types::{Msg, msg};

/// 登录要在多久之内完成。平台给的期限更短就按它的
const LOGIN_TTL: Duration = Duration::from_secs(15 * 60);
/// 最快多久问一次「授权了没有」。平台给的间隔比它还短时按它来 —— 问得再勤，除了
/// 被限流没有别的效果
const POLL_MIN: Duration = Duration::from_secs(2);
/// 调它们的接口的超时
const API_TIMEOUT: Duration = Duration::from_secs(15);
/// 我们在用户账号里建的那把 key 叫这个名字。
///
/// **不复用 Z.ai 客户端那把。**一把 key 是谁建的、能不能删，用户要在他自己的控制台
/// 上看得出来；借用别人的名字会让他删错东西。
const KEY_NAME: &str = "thinkwatch";
/// 建 key 要指定机构和项目。名字里有这两个词的是它们给新账号的默认项，认不出来就用
/// 第一个 —— 和 Z.ai 客户端的选法一致
const DEFAULT_ORG: &str = "默认机构";
const DEFAULT_PROJECT: &str = "默认项目";

pub fn router() -> axum::Router<ControlState> {
    axum::Router::new()
        .route("/zai/login", post(start))
        .route("/zai/login/{id}", get(status).delete(cancel))
}

// ---------------------------------------------------------------- 哪一家

/// 登哪一家。两家的登录流程一模一样，**换 key 那一步的鉴权方式不一样**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// api.z.ai
    Zai,
    /// open.bigmodel.cn
    Bigmodel,
}

impl Family {
    fn parse(v: &str) -> Option<Self> {
        match v {
            "zai" => Some(Self::Zai),
            "bigmodel" => Some(Self::Bigmodel),
            _ => None,
        }
    }
    pub fn slug(&self) -> &'static str {
        match self {
            Self::Zai => "zai",
            Self::Bigmodel => "bigmodel",
        }
    }
    /// 不指定名字时，上游叫这个
    fn default_name(&self) -> &'static str {
        self.slug()
    }
}

/// 登录要用的地址。**平时是它们的**，测试里换成本机的假服务器
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// 平台的命令行登录接口所在，形如 `https://zcode.z.ai/api/v1`
    pub platform: String,
    /// Z.ai 的业务接口：换平台令牌、建 key
    pub zai_business: String,
    /// BigModel 的业务接口
    pub bigmodel_business: String,
    /// 登录成功后写进上游的接口地址
    pub zai_upstream: String,
    pub bigmodel_upstream: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            platform: "https://zcode.z.ai/api/v1".to_string(),
            zai_business: "https://api.z.ai".to_string(),
            bigmodel_business: "https://bigmodel.cn".to_string(),
            // **不带 `/v1`**：网关把客户端请求的路径原样接在后面
            zai_upstream: "https://api.z.ai/api/anthropic".to_string(),
            bigmodel_upstream: "https://open.bigmodel.cn/api/anthropic".to_string(),
        }
    }
}

impl Endpoints {
    fn business(&self, family: Family) -> &str {
        match family {
            Family::Zai => &self.zai_business,
            Family::Bigmodel => &self.bigmodel_business,
        }
    }
    fn upstream(&self, family: Family) -> &str {
        match family {
            Family::Zai => &self.zai_upstream,
            Family::Bigmodel => &self.bigmodel_upstream,
        }
    }
}

/// 登录的状态。**进程里只有一份**：同一时刻只许一次登录，新的开始时把没完成的作废
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

    fn set_status(&self, status: tw_api::ZaiLoginStatus) {
        if let Ok(mut g) = self.current.lock()
            && let Some(c) = g.as_mut()
            && c.status.id == status.id
        {
            c.status = status;
        }
    }
}

struct Current {
    status: tw_api::ZaiLoginStatus,
    /// 等授权的那个任务。取消、或者新的登录开始时停掉它
    task: tokio::task::JoinHandle<()>,
}

impl Current {
    /// 停掉等授权的那个任务。**不用等它真的停下**：这条路上没有本机端口要让给下一次
    /// 登录，而在等的那一步只是一个带超时的 GET
    fn stop(self, s: &ControlState) {
        self.task.abort();
        if self.status.status == "pending" {
            announce(s, &self.status.id, "cancelled", None, None);
        }
    }
}

// ---------------------------------------------------------------- 登录

/// 这次要登成什么样。
struct Want {
    family: Family,
    /// 登录后写进配置的上游名
    name: String,
    /// 调它们的接口走哪条路
    proxy: String,
}

impl Want {
    /// 读一遍参数，有问题就直接回错
    fn read(s: &ControlState, req: &tw_api::ZaiLoginStart) -> Result<Self, Fail> {
        let given = |v: &Option<String>| {
            v.as_deref()
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(str::to_string)
        };
        let family = match given(&req.family) {
            None => Family::Zai,
            Some(v) => Family::parse(&v).ok_or_else(|| {
                fail(
                    StatusCode::BAD_REQUEST,
                    msg!("control.unknown_account_family", family = v => "`{family}` is not an account we can sign in to."),
                )
            })?,
        };
        let name = given(&req.name).unwrap_or_else(|| family.default_name().to_string());
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
        // **同名的上游必须是同一家的账号**，否则重新登录会把一条别的上游的密钥换掉。
        // 认哪一家只能看接口地址：这类上游的协议是通用的 `anthropic`，分不出是谁
        let upstream = s.zai.endpoints.upstream(family);
        if let Some(p) = cfg.providers.iter().find(|p| p.name == name)
            && p.base_url.trim_end_matches('/') != upstream.trim_end_matches('/')
        {
            return Err(fail(
                StatusCode::CONFLICT,
                msg!(
                    "control.name_taken_not_zai", name = &name =>
                    "There is already an upstream named `{name}`, and it is not an account of this \
                     service. Use a different name."
                ),
            ));
        }
        Ok(Self {
            family,
            name,
            proxy,
        })
    }
}

/// 平台下发的这一次登录。
struct Flow {
    id: String,
    /// 轮询要带上它
    poll_token: String,
    flow_id: String,
    authorize_url: String,
    every: Duration,
    /// 到这个时刻还没授权就作废
    deadline: tokio::time::Instant,
}

async fn start(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::ZaiLoginStart>,
) -> Result<Json<tw_api::ZaiLogin>, Fail> {
    let want = Want::read(&s, &req)?;
    // 还没完成的上一次登录作废
    let prev = s.zai.current.lock().ok().and_then(|mut g| g.take());
    if let Some(prev) = prev {
        prev.stop(&s);
    }
    let http = client_for(&s, &want)?;
    let flow = init(&s, &want, &http).await?;
    let login = tw_api::ZaiLogin {
        id: flow.id.clone(),
        authorize_url: flow.authorize_url.clone(),
        expires_in_secs: flow
            .deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .as_secs(),
    };
    let current = Current {
        status: pending(&flow.id),
        task: tokio::spawn(await_login(s.clone(), want, http, flow)),
    };
    // 两个登录同时开始时，后到的算数
    let raced = match s.zai.current.lock() {
        Ok(mut g) => g.replace(current),
        Err(_) => Some(current),
    };
    if let Some(prev) = raced {
        prev.stop(&s);
    }
    Ok(Json(login))
}

/// 问平台要一个授权地址。
///
/// 轮询凭据是**我们自己生成的**：平台只是记住它，所以这一次登录的结果只有拿着它的人
/// 取得到。回包里也有一个 `poll_token`，那是它的回显，我们不用。
async fn init(s: &ControlState, want: &Want, http: &reqwest::Client) -> Result<Flow, Fail> {
    let poll_token = tw_gateway::opaque_token(32);
    let url = format!(
        "{}/oauth/cli/init",
        s.zai.endpoints.platform.trim_end_matches('/')
    );
    let d = data(
        http,
        reqwest::Method::POST,
        &url,
        "asking for an authorization address",
        Some(&format!("Bearer {poll_token}")),
        Some(json!({ "provider": want.family.slug() })),
    )
    .await
    .map_err(|e| fail(StatusCode::BAD_GATEWAY, e.msg()))?;
    let str_of = |k: &str| {
        d.get(k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let bad = |what: &str| {
        fail(
            StatusCode::BAD_GATEWAY,
            msg!(
                "control.signin_response_unusable", detail = what.to_string() =>
                "The sign-in could not be started: {detail}"
            ),
        )
    };
    let flow_id = str_of("flow_id").ok_or_else(|| bad("the response has no flow id"))?;
    let authorize_url =
        str_of("authorize_url").ok_or_else(|| bad("the response has no authorization address"))?;
    // **交给浏览器打开的地址必须是 https。**它是对方给的，一个 `file://` 或者 `javascript:`
    // 就够我们替他打开一个不该打开的东西
    if !authorize_url.starts_with("https://") {
        return Err(bad("the authorization address is not https"));
    }
    let every = d
        .get("poll_interval_sec")
        .and_then(|v| v.as_u64())
        .map(Duration::from_secs)
        .unwrap_or(POLL_MIN)
        .max(POLL_MIN);
    // 平台给的是绝对时刻（秒）。给得比我们的上限还长时按我们的来
    let ttl = d
        .get("expires_at")
        .and_then(|v| v.as_u64())
        .map(|at| Duration::from_secs(at.saturating_sub(now_secs())))
        .filter(|d| !d.is_zero())
        .map_or(LOGIN_TTL, |d| d.min(LOGIN_TTL));
    Ok(Flow {
        id: tw_gateway::opaque_token(16),
        poll_token,
        flow_id,
        authorize_url,
        every,
        deadline: tokio::time::Instant::now() + ttl,
    })
}

/// 等用户在浏览器里授权完。**`pending` 一直等，`failed` 立刻停**
async fn await_login(s: ControlState, want: Want, http: reqwest::Client, flow: Flow) {
    let url = format!(
        "{}/oauth/cli/poll/{}",
        s.zai.endpoints.platform.trim_end_matches('/'),
        flow.flow_id
    );
    let auth = format!("Bearer {}", flow.poll_token);
    while tokio::time::Instant::now() < flow.deadline {
        let d = match data(
            &http,
            reqwest::Method::GET,
            &url,
            "waiting for the authorization",
            Some(&auth),
            None,
        )
        .await
        {
            Ok(d) => d,
            Err(why) => return settle(&s, &flow.id, Err(why.into())),
        };
        match d.get("status").and_then(|v| v.as_str()) {
            Some("pending") => {
                tokio::time::sleep(flow.every).await;
                continue;
            }
            Some("ready") => {
                let done = finish(&s, &want, &http, &d).await;
                return settle(&s, &flow.id, done);
            }
            Some("failed") => {
                return settle(&s, &flow.id, Err("the authorization was refused".into()));
            }
            other => {
                return settle(
                    &s,
                    &flow.id,
                    Err(format!(
                        "the authorization is in a state we do not know: {}",
                        other.unwrap_or("none")
                    )),
                );
            }
        }
    }
    s.zai.set_status(tw_api::ZaiLoginStatus {
        id: flow.id.clone(),
        status: "expired".into(),
        provider: None,
        account: None,
        error: Some("the authorization was not completed in time".into()),
    });
    announce(&s, &flow.id, "expired", None, None);
}

/// 授权完了：换一把 key，写进配置。
async fn finish(
    s: &ControlState,
    want: &Want,
    http: &reqwest::Client,
    ready: &Value,
) -> Result<(String, Option<String>), String> {
    // 这一家的令牌挂在以它自己命名的那个键下：`data.zai.access_token`
    let access = ready
        .get(want.family.slug())
        .and_then(|v| v.get("access_token").or_else(|| v.get("accessToken")))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or("the authorization came back without an access token")?;
    let account = ready
        .get("user")
        .and_then(|u| {
            ["email", "name"]
                .iter()
                .find_map(|k| u.get(*k).and_then(|v| v.as_str()))
        })
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    let key = mint_key(http, &s.zai.endpoints, want.family, access).await?;
    let name = save(s, want, key).await?;
    Ok((name, account))
}

// ---------------------------------------------------------------- 换一把 key

/// 从账号里取一把可用的 API key。
///
/// **这一步会在用户的账号里建东西**：名字是 [`KEY_NAME`] 的那把 key，已经有就复用。
/// 套餐额度跟着账号的 key 走，所以拿到 key 才算登录完 —— 界面上必须在登录之前就把
/// 这件事说清楚。
async fn mint_key(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    family: Family,
    access: &str,
) -> Result<String, String> {
    let host = endpoints.business(family).trim_end_matches('/');
    let auth = match family {
        // Z.ai 的业务接口只认它自己换发的平台令牌，OAuth 那个 access token 在这儿用不了
        Family::Zai => {
            let d = data(
                http,
                reqwest::Method::POST,
                &format!("{host}/api/auth/z/login"),
                "exchanging the sign-in for an account token",
                None,
                Some(json!({ "token": access })),
            )
            .await?;
            let token = d
                .get("access_token")
                .or_else(|| d.get("accessToken"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .ok_or("the account token response has no token in it")?;
            format!("Bearer {token}")
        }
        // BigModel 的业务接口直接认 OAuth 的 access token，**而且不带 `Bearer ` 前缀**
        Family::Bigmodel => access.to_string(),
    };
    let info = data(
        http,
        reqwest::Method::GET,
        &format!("{host}/api/biz/customer/getCustomerInfo"),
        "reading the account",
        Some(&auth),
        None,
    )
    .await?;
    let (org, project) = pick_place(&info)
        .ok_or("the account has no organization and project an API key could go into")?;
    let keys = format!("{host}/api/biz/v1/organization/{org}/projects/{project}/api_keys");
    let listed = data(
        http,
        reqwest::Method::GET,
        &keys,
        "listing the account's API keys",
        Some(&auth),
        None,
    )
    .await?;
    let mine = listed
        .as_array()
        .into_iter()
        .flatten()
        .find(|k| k.get("name").and_then(|v| v.as_str()) == Some(KEY_NAME))
        .and_then(|k| k.get("apiKey"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let id = match mine {
        Some(id) => id,
        None => data(
            http,
            reqwest::Method::POST,
            &keys,
            "creating an API key",
            Some(&auth),
            Some(json!({ "name": KEY_NAME })),
        )
        .await?
        .get("apiKey")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or("the new API key came back without an id")?,
    };
    // **id 要接进 URL 的路径里。**对方给什么我们就接什么的话，一个带 `/` 或 `?` 的值
    // 就能把请求发到另一个接口上
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c))
    {
        return Err("the API key's id has characters we do not expect".into());
    }
    let secret = data(
        http,
        reqwest::Method::GET,
        &format!("{keys}/copy/{id}"),
        "reading the API key",
        Some(&auth),
        None,
    )
    .await?
    .get("secretKey")
    .and_then(|v| v.as_str())
    .map(str::trim)
    .filter(|v| !v.is_empty())
    .map(str::to_string);
    match (family, secret) {
        (_, Some(secret)) => Ok(format!("{id}.{secret}")),
        // Z.ai 的密钥是 `id.secret` 两段，少一段用不了 —— 宁可现在就说，不要写一把
        // 发出去就是 401 的密钥进配置
        (Family::Zai, None) => Err("the API key came back without its secret half".into()),
        (Family::Bigmodel, None) => Ok(id),
    }
}

/// key 建在哪个机构的哪个项目下。名字认不出来就用第一个
fn pick_place(info: &Value) -> Option<(String, String)> {
    let orgs = info.get("organizations")?.as_array()?;
    let named = |list: &[Value], key: &str, want: &str| -> Option<Value> {
        list.iter()
            .find(|o| {
                o.get(key)
                    .and_then(|v| v.as_str())
                    .is_some_and(|n| n.contains(want))
            })
            .or_else(|| list.first())
            .cloned()
    };
    let org = named(orgs, "organizationName", DEFAULT_ORG)?;
    let projects = org.get("projects")?.as_array()?.clone();
    let project = named(&projects, "projectName", DEFAULT_PROJECT)?;
    let id = |v: &Value, k: &str| {
        v.get(k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    Some((id(&org, "organizationId")?, id(&project, "projectId")?))
}

/// 把登录得来的密钥写进 config.yaml。
///
/// **重新登录只换密钥**，出站方式、模型范围、停用状态这些都不动 —— 用户在上游页上
/// 调过的东西不该因为换一次密钥就回到默认值。
async fn save(s: &ControlState, want: &Want, key: String) -> Result<String, String> {
    let name = want.name.clone();
    let proxy = want.proxy.clone();
    let upstream = s.zai.endpoints.upstream(want.family).to_string();
    s.cfg
        .transform(None, Origin::Ui, |text, cfg| {
            let existing = cfg.providers.iter().find(|p| p.name == name);
            if let Some(e) = existing
                && e.base_url.trim_end_matches('/') != upstream.trim_end_matches('/')
            {
                return Err(crate::resources::invalid(msg!(
                    "control.name_taken_not_zai", name = &name =>
                    "There is already an upstream named `{name}`, and it is not an account of this \
                     service. Use a different name."
                )));
            }
            let mut p = existing.cloned().unwrap_or_else(|| tw_config::Provider {
                name: name.clone(),
                base_url: upstream.clone(),
                // 它的接口是 Anthropic 的 messages 协议。**写出来而不是让它去猜**：
                // 这个域名不在猜得出来的那几个里
                protocol: Some(Protocol::Anthropic),
                proxy: proxy.clone(),
                ..Default::default()
            });
            p.key = Some(tw_config::Secret::new(key.clone()));
            // 这类上游从不用 OAuth。上一次登录留下的（不该有）也一并清掉
            p.oauth = None;
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
        .map_err(|e| format!("the configuration could not be written: {e}"))?;
    // 模型清单现在就问一次：不然要等到下一轮定时刷新，新上游的模型才出现
    let gateway = s.gateway.clone();
    let provider = name.clone();
    tokio::spawn(async move {
        tw_gateway::models::refresh_one(&gateway, &provider).await;
    });
    Ok(name)
}

// ---------------------------------------------------------------- 状态

fn pending(id: &str) -> tw_api::ZaiLoginStatus {
    tw_api::ZaiLoginStatus {
        id: id.to_string(),
        status: "pending".into(),
        provider: None,
        account: None,
        error: None,
    }
}

/// 记下登录的结果，并告诉界面一声
fn settle(s: &ControlState, id: &str, done: Result<(String, Option<String>), String>) {
    let status = match done {
        Ok((provider, account)) => tw_api::ZaiLoginStatus {
            id: id.to_string(),
            status: "done".into(),
            provider: Some(provider),
            account,
            error: None,
        },
        Err(why) => {
            tracing::warn!("the sign-in did not finish: {why}");
            tw_api::ZaiLoginStatus {
                id: id.to_string(),
                status: "failed".into(),
                provider: None,
                account: None,
                error: Some(why),
            }
        }
    };
    s.zai.set_status(status.clone());
    announce(s, id, &status.status, status.provider, status.error);
}

async fn status(
    State(s): State<ControlState>,
    Path(id): Path<String>,
) -> Result<Json<tw_api::ZaiLoginStatus>, Fail> {
    s.zai
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
) -> Result<Json<tw_api::ZaiLoginStatus>, Fail> {
    let mut status = {
        let g = s.zai.current.lock().map_err(crate::internal)?;
        let c = g.as_ref().filter(|c| c.status.id == id).ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                msg!(
                    "control.signin_gone" =>
                    "There is no such sign-in, or a newer one replaced it."
                ),
            )
        })?;
        if c.status.status != "pending" {
            return Ok(Json(c.status.clone()));
        }
        c.task.abort();
        c.status.clone()
    };
    status.status = "cancelled".into();
    s.zai.set_status(status.clone());
    announce(&s, &id, "cancelled", None, None);
    Ok(Json(status))
}

// ---------------------------------------------------------------- 杂项

/// 调一次它们的接口，**把回包里的业务错误码当失败**。
///
/// 它们会用 HTTP 200 包一个 `{"code":401,"msg":"…"}` 回来。只看状态码的话，失败会被
/// 当成成功，然后在后面某一步拿着一个空值报一句和真实原因无关的错。
///
/// 出错的话里只有**这一步在做什么**，没有地址：建 key 那几个地址的路径里带着密钥的
/// id，而错误会进日志、也会显示给用户。
/// 调一次账号平台的接口没成。
///
/// **结构留着，句子等到要说的时候再拼。**登录流程里的那些失败最后是登录
/// 状态里的一行字（`to_string`，带着「在做哪一步」）；开始登录那一步是一条
/// 控制面的错误，要带码（[`CallError::msg`]）。先拼成英文再往外发的话，
/// 后一种就只剩把整句塞进 `{detail}` 一条路。
#[derive(Debug)]
struct CallError {
    /// 在做哪一步，英文词组，只进 `Display`
    doing: String,
    why: CallFailure,
}

#[derive(Debug)]
enum CallFailure {
    /// 连不上、超时。数据面那句自己带码
    Unreachable(Msg),
    Status {
        status: u16,
        body: String,
    },
    NotJson {
        body: String,
    },
    Refused {
        why: String,
    },
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let doing = &self.doing;
        match &self.why {
            CallFailure::Unreachable(m) => write!(f, "{doing} failed: {m}"),
            CallFailure::Status { status, body } => write!(f, "{doing} answered {status}: {body}"),
            CallFailure::NotJson { body } => write!(f, "{doing} did not answer JSON: {body}"),
            CallFailure::Refused { why } => write!(f, "{doing} was refused: {why}"),
        }
    }
}

impl From<CallError> for String {
    fn from(e: CallError) -> Self {
        e.to_string()
    }
}

impl CallError {
    /// 带码的说法。**不说在做哪一步** —— 那是一个英文词组，而用得上这句话的
    /// 只有开始登录那一处，界面自己知道那是哪一步。
    fn msg(&self) -> Msg {
        match &self.why {
            CallFailure::Unreachable(m) => m.clone(),
            CallFailure::Status { status, body } => msg!(
                "control.account_service_status", status = status, detail = body =>
                "The account service answered {status}: {detail}"
            ),
            CallFailure::NotJson { body } => msg!(
                "control.account_service_not_json", detail = body =>
                "The account service did not answer JSON: {detail}"
            ),
            CallFailure::Refused { why } => msg!(
                "control.account_service_refused", why = why =>
                "The account service refused the request: {why}"
            ),
        }
    }
}

async fn data(
    http: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    doing: &str,
    auth: Option<&str>,
    body: Option<Value>,
) -> Result<Value, CallError> {
    let fail = |why| CallError {
        doing: doing.to_string(),
        why,
    };
    let mut req = http
        .request(method, url)
        .timeout(API_TIMEOUT)
        // 如实说自己是谁
        .header("user-agent", tw_gateway::user_agent());
    if let Some(auth) = auth {
        req = req.header("authorization", auth);
    }
    if let Some(body) = body {
        req = req.json(&body);
    }
    let resp = req.send().await.map_err(|e| {
        fail(CallFailure::Unreachable(
            tw_gateway::forward::map_reqwest_error(e).detail,
        ))
    })?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(fail(CallFailure::Status {
            status: status.as_u16(),
            body: brief(&text),
        }));
    }
    let v: Value = serde_json::from_str(&text)
        .map_err(|_| fail(CallFailure::NotJson { body: brief(&text) }))?;
    if !business_ok(v.get("code")) {
        let why = v
            .get("msg")
            .and_then(|m| m.as_str())
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or("no reason given");
        return Err(fail(CallFailure::Refused {
            why: why.to_string(),
        }));
    }
    Ok(v.get("data").cloned().unwrap_or(Value::Null))
}

/// 回包里的业务码算不算成功。**没有这一项也算** —— 有些接口成功时就不带它
fn business_ok(code: Option<&Value>) -> bool {
    match code {
        None | Some(Value::Null) => true,
        Some(Value::Number(n)) => matches!(n.as_i64(), Some(0) | Some(200)),
        Some(Value::String(s)) => s == "0" || s == "200",
        _ => false,
    }
}

/// 出错时给用户看的那一小段响应
fn brief(text: &str) -> String {
    tw_secret::mask_body(text).chars().take(200).collect()
}

/// 按出站方式建一个客户端。**要代理才能访问它们的用户，直连会卡在第一步**
fn client_for(s: &ControlState, want: &Want) -> Result<reqwest::Client, Fail> {
    let route = tw_config::Provider {
        name: want.name.clone(),
        base_url: s.zai.endpoints.platform.clone(),
        proxy: want.proxy.clone(),
        ..Default::default()
    };
    // 代理没定义、密码读不到这些，数据面那句自己带码
    tw_gateway::client_for_provider(&s.config(), &route)
        .map_err(|e| fail(StatusCode::BAD_GATEWAY, e.detail))
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

fn now_secs() -> u64 {
    now_ms() / 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_business_error_code_inside_a_200_is_a_failure() {
        assert!(business_ok(None));
        assert!(business_ok(Some(&json!(0))));
        assert!(business_ok(Some(&json!(200))));
        assert!(business_ok(Some(&json!("0"))));
        assert!(!business_ok(Some(&json!(401))));
        assert!(!business_ok(Some(&json!(3001))));
        assert!(!business_ok(Some(&json!("3001"))));
    }

    #[test]
    fn the_default_organization_and_project_win_over_the_first_one() {
        let info = json!({"organizations": [
            {"organizationId": "o1", "organizationName": "别的机构", "projects": [
                {"projectId": "p1", "projectName": "别的项目"}
            ]},
            {"organizationId": "o2", "organizationName": "我的默认机构", "projects": [
                {"projectId": "p2", "projectName": "别的项目"},
                {"projectId": "p3", "projectName": "默认项目"}
            ]}
        ]});
        assert_eq!(
            pick_place(&info),
            Some(("o2".to_string(), "p3".to_string()))
        );
    }

    #[test]
    fn the_first_organization_and_project_are_the_fallback() {
        let info = json!({"organizations": [
            {"organizationId": "o1", "organizationName": "Acme", "projects": [
                {"projectId": "p1", "projectName": "Playground"}
            ]}
        ]});
        assert_eq!(
            pick_place(&info),
            Some(("o1".to_string(), "p1".to_string()))
        );
        assert_eq!(pick_place(&json!({"organizations": []})), None);
    }
}

#[cfg(test)]
mod call_error_codes {
    use super::*;

    /// 开始登录那一步的失败带码发出去；登录状态里那一行照旧说在做哪一步。
    #[test]
    fn a_failed_call_has_a_code_and_still_says_what_it_was_doing() {
        let e = |why| CallError {
            doing: "asking for an authorization address".into(),
            why,
        };
        let unreachable = msg!("gw.upstream.timeout" => "timed out");
        let all = [
            (
                e(CallFailure::Unreachable(unreachable.clone())),
                "gw.upstream.timeout",
            ),
            (
                e(CallFailure::Status {
                    status: 500,
                    body: "x".into(),
                }),
                "control.account_service_status",
            ),
            (
                e(CallFailure::NotJson { body: "x".into() }),
                "control.account_service_not_json",
            ),
            (
                e(CallFailure::Refused { why: "x".into() }),
                "control.account_service_refused",
            ),
        ];
        for (err, code) in all {
            assert_eq!(err.msg().code, code);
            assert!(!err.msg().text.is_empty());
            let line: String = err.into();
            assert!(
                line.starts_with("asking for an authorization address"),
                "{line}"
            );
        }
    }
}
