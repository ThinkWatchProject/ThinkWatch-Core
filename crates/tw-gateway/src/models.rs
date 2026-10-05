//! 每个上游提供哪些模型：向上游要来的、配置里手写的，以及它们是什么时候、
//! 怎么来的。
//!
//! # 两份东西
//!
//! - **目录（[`Directory`]）**记每个上游的原始答案：列出了什么、上次什么时候
//!   问的、没问到的话为什么。它跨重载存活。
//! - **汇总（`tw_engine::Catalog`）**从目录和当前配置推出来：停用的上游不算，
//!   启用范围（`models_only`）之外的模型不算。`/v1/models`、准入、挑候选
//!   都只看它。
//!
//! 配置一变，汇总马上按新配置重算；只有地址、凭据、协议、代理变了的上游
//! 才需要重新去问，其余的答案原样留着。以前改一次配置，汇总就换回只含手写
//! 清单的那份：已经问到的模型全部消失，新加的上游要等到第二天的定时刷新。
//!
//! **向上游问是后台的事。**探测零成本，但要打网络：它不挡启动，`/v1/models`
//! 也不现问 —— 每次有人列模型就去打一遍上游，会把一个本该零成本的端点变成
//! 一串网络往返。
//!
//! **问的开始和结束都报 `models_changed`。**后台的问不挂在任何一次调用上，
//! 界面没有别的办法知道它问完了：以前启动那一刻读到的「还没问」会一直挂在
//! 上游页上，直到别的什么事让它重读一次概览。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use crate::probe::ModelList;
use crate::server::now_ms;
use crate::state::{AppState, Runtime};
use tw_types::msg;

/// 多久向每个上游重新问一次。
const REFRESH_EVERY: Duration = Duration::from_secs(24 * 3600);
/// 没问到（连不上、密钥被拒）之后多久再试。
const RETRY_AFTER: Duration = Duration::from_secs(3600);
/// 页面打开时补问：同一家至少隔这么久。**来回切页面不该每次都打一遍网络**
const RECHECK_GAP: Duration = Duration::from_secs(60);
/// 多久看一次有没有到时间的。看一次只是比较时间，不联网。
///
/// **不是睡满一天再醒。**笔记本合盖时单调时钟不走，睡「24 小时」可能睡上
/// 好几天；按墙上时间一轮轮比，开盖之后下一轮就会去问。
const TICK: Duration = Duration::from_secs(10 * 60);

/// 一个上游的模型清单从哪儿来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// 上游自己列出来的
    Discovered,
    /// 上游没给出清单，用配置里手写的 `models`
    Manual,
    /// 都没有。**不知道它有什么**，不是它什么都没有
    None,
}

impl Source {
    pub fn slug(&self) -> &'static str {
        match self {
            Source::Discovered => "discovered",
            Source::Manual => "manual",
            Source::None => "none",
        }
    }
}

/// 最近一次向上游问的结果。**和 [`Source`] 不是一回事**：没问到时清单可能
/// 来自手写的兜底（`Manual`），而界面要说的是「问了，没问到，为什么」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// 还没问过：刚启动、刚加的、刚改了地址或凭据。停用的上游一直是这样 ——
    /// 不去问它
    Pending,
    /// 上游列出了清单
    Listed,
    /// 问到了，但上游没给出清单：没有这个接口、格式认不出、空的。**再问多半
    /// 还是这样**，该做的是填手动清单
    NoList,
    /// 没问到：连不上、密钥被拒、取不到密钥。**修好连接再问就行**
    Failed,
}

impl Status {
    pub fn slug(&self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Listed => "listed",
            Status::NoList => "no_list",
            Status::Failed => "failed",
        }
    }
}

/// 一个上游的模型清单。
#[derive(Debug, Clone)]
pub struct Listing {
    pub source: Source,
    pub status: Status,
    /// 正在向上游问。**和 `status` 同时成立**：上一次的答案照常可用，
    /// 新答案回来之前不作废
    pub fetching: bool,
    /// 清单本身。**还没按启用范围过滤**
    pub models: Vec<String>,
    /// 最近一次向上游问的时间。还没问过是空
    pub checked_at_ms: Option<u64>,
    /// 没从上游拿到清单的原因
    pub error: Option<tw_types::Msg>,
}

/// 最近一次向上游问的结果。
#[derive(Debug, Clone)]
enum Answer {
    /// 还没问过
    Pending,
    /// 列出来了
    Listed(Vec<String>),
    /// 问到了，但上游没给出清单：没有这个接口、格式认不出、空的
    NoList(tw_types::Msg),
    /// 没问到：连不上、密钥被拒、取不到密钥
    Failed(tw_types::Msg),
}

#[derive(Debug, Clone)]
struct Entry {
    /// 问的时候这家长什么样。地址、凭据、协议、代理变了，答案就作废
    identity: String,
    answer: Answer,
    checked_at_ms: Option<u64>,
    /// 正在问的次数。**是计数不是开关**：后台那一轮和用户点的刷新可能同时
    /// 在问同一家，先回来的那个不该把另一个还在问的状态抹掉
    in_flight: u32,
}

#[derive(Debug, Default)]
pub struct Directory {
    entries: Mutex<HashMap<String, Entry>>,
    /// 有上游等着去问
    wake: tokio::sync::Notify,
}

/// 决定答案还作不作数的那几样。
///
/// **OAuth 只看账号，不看 token**：refresh token 轮换一次就重问一遍，问到的
/// 是同一份清单。
fn identity(cfg: &tw_config::Config, p: &tw_config::Provider) -> String {
    let key = p.credential_identity();
    let raw = format!(
        "{}|{key}|{:?}|{}",
        p.base_url,
        p.effective_protocol(),
        crate::outbound::proxy_shape(cfg, p)
    );
    // **存指纹，不存原文** —— 这张表不该多出一份密钥
    blake3::hash(raw.as_bytes()).to_hex().to_string()
}

impl Directory {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 配置换了之后对一遍：删掉的上游忘掉，新加的、身份变了的等着去问。
    /// 返回有没有要去问的。
    pub(crate) fn reconcile(&self, cfg: &tw_config::Config) -> bool {
        let mut entries = self.lock();
        entries.retain(|name, _| cfg.providers.iter().any(|p| &p.name == name));
        let mut pending = false;
        for p in &cfg.providers {
            let id = identity(cfg, p);
            if entries.get(&p.name).is_some_and(|e| e.identity == id) {
                continue;
            }
            entries.insert(
                p.name.clone(),
                Entry {
                    identity: id,
                    answer: Answer::Pending,
                    checked_at_ms: None,
                    in_flight: 0,
                },
            );
            pending = true;
        }
        pending
    }

    pub(crate) fn wake(&self) {
        self.wake.notify_one();
    }

    /// 这个上游的模型清单。
    pub fn listing(&self, p: &tw_config::Provider) -> Listing {
        let entry = self.lock().get(&p.name).cloned();
        listing_of(entry.as_ref(), p)
    }

    /// 按目录和配置推出汇总，并换进 `into`。
    ///
    /// **推算和换入在同一把锁里，配置在锁里现取。**否则一次探测回来和一次
    /// 配置重载交错时，后换入的可能是按旧配置推出来的那份。
    pub(crate) fn publish(
        &self,
        config: impl Fn() -> std::sync::Arc<tw_config::Config>,
        into: &arc_swap::ArcSwap<tw_engine::Catalog>,
    ) {
        let entries = self.lock();
        let cfg = config();
        let sources: Vec<tw_engine::ProviderModels> = cfg
            .providers
            .iter()
            // 停用的上游不提供任何东西
            .filter(|p| !p.disabled)
            .map(|p| {
                let l = listing_of(entries.get(&p.name), p);
                tw_engine::ProviderModels {
                    provider: p.name.clone(),
                    protocol: p
                        .effective_protocol()
                        // 猜不出协议时按 Anthropic 算 —— 和放凭据时的默认一致
                        // （`tw_config::auth_header`）。两处不一致会让「列出来
                        // 了但发过去 401」变成可能。
                        .unwrap_or(tw_config::Protocol::Anthropic)
                        .slug()
                        .to_string(),
                    known: l.source != Source::None,
                    models: l.models.into_iter().filter(|m| p.uses_model(m)).collect(),
                }
            })
            .collect();
        // 别名跟着同一份配置登记：列表、准入看到的别名表和挑候选时的是同一张
        let catalog = tw_engine::Catalog::build(&sources).with_aliases(
            cfg.aliases
                .iter()
                .map(|a| (a.name.as_str(), a.models.as_slice())),
        );
        into.store(std::sync::Arc::new(catalog));
    }

    /// 到时间该去问的上游。
    fn due(&self, cfg: &tw_config::Config, now: u64) -> Vec<String> {
        let entries = self.lock();
        cfg.providers
            .iter()
            // 停用的上游用不着：打开它时身份不变，也会在到时间时补上
            .filter(|p| !p.disabled)
            .filter(|p| match entries.get(&p.name) {
                None => true,
                // 正在问的不再排一次
                Some(e) if e.in_flight > 0 => false,
                Some(e) => match (&e.answer, e.checked_at_ms) {
                    (Answer::Pending, _) | (_, None) => true,
                    (Answer::Failed(_), Some(at)) => {
                        now.saturating_sub(at) >= RETRY_AFTER.as_millis() as u64
                    }
                    (_, Some(at)) => now.saturating_sub(at) >= REFRESH_EVERY.as_millis() as u64,
                },
            })
            .map(|p| p.name.clone())
            .collect()
    }

    /// 页面打开时该补问的上游：没问过的、没问到的、过期的。
    ///
    /// **刚问过的不问**（[`RECHECK_GAP`]），**正在问的不问**，**上游本来就
    /// 不给清单的不问** —— 除非过期了，再问多半还是一样。停用的不问。
    fn stale(&self, cfg: &tw_config::Config, now: u64) -> Vec<String> {
        let entries = self.lock();
        cfg.providers
            .iter()
            .filter(|p| !p.disabled)
            .filter(|p| {
                let Some(e) = entries.get(&p.name) else {
                    return true;
                };
                if e.in_flight > 0 {
                    return false;
                }
                let Some(at) = e.checked_at_ms else {
                    return true;
                };
                let age = now.saturating_sub(at);
                if age < RECHECK_GAP.as_millis() as u64 {
                    return false;
                }
                match e.answer {
                    Answer::Pending | Answer::Failed(_) => true,
                    Answer::Listed(_) | Answer::NoList(_) => {
                        age >= REFRESH_EVERY.as_millis() as u64
                    }
                }
            })
            .map(|p| p.name.clone())
            .collect()
    }

    /// 开始问一家。**配置还没对过这一家时补一条**（比如刚加上、重载还没轮到）
    /// —— 否则问回来的答案没有地方记。
    fn begin(&self, name: &str, identity: &str) {
        let mut entries = self.lock();
        let e = entries.entry(name.to_string()).or_insert_with(|| Entry {
            identity: identity.to_string(),
            answer: Answer::Pending,
            checked_at_ms: None,
            in_flight: 0,
        });
        if e.identity == identity {
            e.in_flight += 1;
        }
    }

    /// 记下一次答案。**问的时候那家的身份已经变了的话，这个答案作废** ——
    /// 配置重载已经把它排进了下一轮。
    fn record(&self, name: &str, identity: &str, answer: Answer, at: u64) {
        let mut entries = self.lock();
        if let Some(e) = entries.get_mut(name)
            && e.identity == identity
        {
            e.answer = answer;
            e.checked_at_ms = Some(at);
            e.in_flight = e.in_flight.saturating_sub(1);
        }
    }

    /// 开始问之后又没问（问之前配置变了）：只撤掉在途，不动答案。
    fn abandon(&self, name: &str, identity: &str) {
        let mut entries = self.lock();
        if let Some(e) = entries.get_mut(name)
            && e.identity == identity
        {
            e.in_flight = e.in_flight.saturating_sub(1);
        }
    }
}

fn listing_of(entry: Option<&Entry>, p: &tw_config::Provider) -> Listing {
    let checked_at_ms = entry.and_then(|e| e.checked_at_ms);
    let fetching = entry.is_some_and(|e| e.in_flight > 0);
    let error = entry.and_then(|e| match &e.answer {
        Answer::NoList(why) | Answer::Failed(why) => Some(why.clone()),
        Answer::Pending | Answer::Listed(_) => None,
    });
    let status = match entry.map(|e| &e.answer) {
        None | Some(Answer::Pending) => Status::Pending,
        Some(Answer::Listed(_)) => Status::Listed,
        Some(Answer::NoList(_)) => Status::NoList,
        Some(Answer::Failed(_)) => Status::Failed,
    };
    match entry.map(|e| &e.answer) {
        Some(Answer::Listed(models)) => Listing {
            source: Source::Discovered,
            status,
            fetching,
            models: models.clone(),
            checked_at_ms,
            error: None,
        },
        // 问不到就用手写的兜底。**两者不合并** —— 合并的话，用户删掉一个
        // 上游不再提供的模型时会发现它删不掉
        _ if !p.models.is_empty() => Listing {
            source: Source::Manual,
            status,
            fetching,
            models: p.models.clone(),
            checked_at_ms,
            error,
        },
        _ => Listing {
            source: Source::None,
            status,
            fetching,
            models: Vec::new(),
            checked_at_ms,
            error,
        },
    }
}

/// 问一家上游有哪些模型。
async fn ask(state: &AppState, rt: &Runtime, p: &tw_config::Provider) -> Answer {
    // 用这一家自己的 client：它带着该走的代理，换 OAuth token 也要走那条路
    let http = rt.clients.get(&p.name).unwrap_or(&state.http);
    let headers = match state.headers_for(p, http).await {
        Ok(h) => h,
        Err(e) => {
            return Answer::Failed(crate::state::credential_failed(e, &p.name));
        }
    };
    let r = if p.is_bedrock() {
        let credentials = match p.aws_credentials() {
            Ok(c) => c,
            Err(e) => return Answer::Failed(crate::state::credential_failed(e.msg(), &p.name)),
        };
        crate::probe::probe_bedrock(http, p, &headers, credentials.as_ref()).await
    } else {
        crate::probe::probe(http, &p.base_url, &headers, p.effective_protocol()).await
    };
    if !r.ok {
        return Answer::Failed(
            r.error
                .unwrap_or_else(|| msg!("gw.models.check_failed" => "The check failed.")),
        );
    }
    match r.models {
        ModelList::Listed { models } => Answer::Listed(models),
        // 探测说了为什么列不出（Bedrock 没有列模型的权限）就用它的话
        ModelList::NotImplemented { status } => Answer::NoList(r.error.unwrap_or_else(|| {
            msg!(
                "gw.models.no_endpoint", status = status =>
                "The upstream has no model-list endpoint (HTTP {status})."
            )
        })),
        ModelList::Unrecognized { .. } => Answer::NoList(msg!(
            "gw.models.unrecognized" =>
            "The model list the upstream returned is in an unrecognized format."
        )),
        ModelList::Empty => Answer::NoList(msg!(
            "gw.models.empty" => "The model list the upstream returned is empty."
        )),
    }
}

/// 正在问的一家：名字，和开始问时它的身份。
struct Asking {
    name: String,
    identity: String,
}

/// 告诉界面这一家的清单变了（开始问了，或者问完了）。
fn changed(state: &AppState, provider: &str) {
    let id = state.bus.next_id();
    state.bus.emit(tw_api::Event::ModelsChanged {
        id,
        provider: provider.to_string(),
        at_ms: now_ms(),
    });
}

/// 把这几家标成正在问。**同步做完再返回** —— 调用方紧接着读概览时，
/// 读到的就已经是「正在获取」。
fn start(state: &AppState, names: &[String]) -> Vec<Asking> {
    let cfg = state.config();
    names
        .iter()
        .filter_map(|name| {
            let p = cfg.providers.iter().find(|p| &p.name == name)?;
            let identity = identity(&cfg, p);
            state.models.begin(name, &identity);
            changed(state, name);
            Some(Asking {
                name: name.clone(),
                identity,
            })
        })
        .collect()
}

/// 去问，记下答案，重算汇总。**几家一起问** —— 探测互不干扰，而一家
/// 连不上要等满超时，挨个问会让排在后面的都跟着等。
async fn finish(state: &AppState, asking: Vec<Asking>) {
    let rt = state.runtime();
    let asks = asking.iter().map(|a| {
        let rt = &rt;
        async move {
            let answer = match rt.config.providers.iter().find(|p| p.name == a.name) {
                Some(p) if identity(&rt.config, p) == a.identity => Some(ask(state, rt, p).await),
                // 开始问之后配置又变了：这一问作废，重载已经把它排进下一轮
                _ => None,
            };
            (a, answer)
        }
    });
    for (a, answer) in futures::future::join_all(asks).await {
        let Some(answer) = answer else {
            state.models.abandon(&a.name, &a.identity);
            continue;
        };
        match &answer {
            Answer::Listed(models) => {
                tracing::debug!(provider = %a.name, models = models.len(), "fetched the model list")
            }
            Answer::NoList(why) | Answer::Failed(why) => {
                tracing::info!(provider = %a.name, "the model list could not be fetched: {why}")
            }
            Answer::Pending => {}
        }
        state.models.record(&a.name, &a.identity, answer, now_ms());
    }
    // 先重算汇总再报：界面收到事件去读概览时，数目已经是新的
    state.publish_catalog();
    for a in &asking {
        changed(state, &a.name);
    }
}

async fn refresh(state: &AppState, names: &[String]) {
    let asking = start(state, names);
    finish(state, asking).await;
}

/// 页面打开时补问：没问过的、没问到的、过期的（见 [`Directory::stale`]）。
///
/// **不等结果**：返回的是开始问的那几家，答案随 `models_changed` 一家一家地到。
/// 等齐了再回的话，一家连不上就让整页等满超时。
pub fn refresh_stale(state: &AppState) -> Vec<String> {
    let names = state.models.stale(&state.config(), now_ms());
    if names.is_empty() {
        return names;
    }
    let asking = start(state, &names);
    let state = state.clone();
    tokio::spawn(async move { finish(&state, asking).await });
    names
}

/// 马上向所有上游问一遍。
pub async fn refresh_all(state: &AppState) {
    let names: Vec<String> = state
        .config()
        .providers
        .iter()
        .map(|p| p.name.clone())
        .collect();
    refresh(state, &names).await;
}

/// 马上问这一家，返回它现在的清单。**停用了也问** —— 这是用户点的。
pub async fn refresh_one(state: &AppState, name: &str) -> Option<Listing> {
    refresh(state, &[name.to_string()]).await;
    let cfg = state.config();
    let p = cfg.providers.iter().find(|p| p.name == name)?;
    Some(state.models.listing(p))
}

/// 后台：启动时问一遍，之后每天一遍；配置里新加、改了的上游随时补问。
///
/// **`url-test` 的垫底跟着它一起跑**：两者都是「启动时打一次网络、之后靠
/// 真实流量」，而且都不该挡住启动。
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut seeded_at: Option<u64> = None;
        loop {
            let now = now_ms();
            if seeded_at.is_none_or(|t| now.saturating_sub(t) >= REFRESH_EVERY.as_millis() as u64) {
                crate::latency::seed_url_test(&state).await;
                seeded_at = Some(now);
            }
            let due = state.models.due(&state.config(), now);
            if !due.is_empty() {
                refresh(&state, &due).await;
            }
            tokio::select! {
                _ = tokio::time::sleep(TICK) => {}
                _ = state.models.wake.notified() => {}
            }
        }
    });
}

/// 为什么跳过一个候选上游。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// 停用了
    Disabled,
    /// 这个模型不在它的启用范围里
    OutOfScope,
    /// 它的模型清单里没有这个模型
    NotOffered,
    /// 发给它的名字这把密钥不让用（`allow`）：指定模型、阶段二改的名字一家一个，
    /// 准入放行了别的那几家，故障转移也到不了这一家
    NotAllowed,
}

impl Skip {
    pub fn slug(&self) -> &'static str {
        match self {
            Skip::Disabled => "disabled",
            Skip::OutOfScope => "out_of_scope",
            Skip::NotOffered => "not_offered",
            Skip::NotAllowed => "not_allowed",
        }
    }
    /// 进给客户端的那句话里。**界面不用它** —— 界面按 `slug` 自己说。
    pub fn label(&self) -> &'static str {
        match self {
            Skip::Disabled => "disabled",
            Skip::OutOfScope => "out of scope",
            Skip::NotOffered => "does not offer this model",
            Skip::NotAllowed => "not allowed for this key",
        }
    }
}

/// 路由选出的候选里，哪些能服务这个请求。
#[derive(Debug, Clone, Default)]
pub struct Serving {
    pub usable: Vec<String>,
    pub skipped: Vec<(String, Skip)>,
    /// 每个候选要的模型，给 [`Self::explain`] 那句话用
    asked: Vec<(String, String)>,
}

impl Serving {
    /// 一个都不剩时给客户端的那句话。`model` 是阶段一之后要的模型；哪一家要的
    /// 不是它（阶段二按上游改写过），就在那一家后面写出它要的那个。
    pub fn explain(&self, model: &str) -> crate::GatewayError {
        let why = self
            .skipped
            .iter()
            .map(
                |(p, s)| match self.asked.iter().find(|(c, _)| c == p).map(|(_, m)| m) {
                    Some(m) if !m.is_empty() && m != model => format!("{p} {} ({m})", s.label()),
                    _ => format!("{p} {}", s.label()),
                },
            )
            .collect::<Vec<_>>()
            .join("; ");
        // 全是停用：请求没问题，是配置里能用的上游都关掉了
        if self.skipped.iter().all(|(_, s)| *s == Skip::Disabled) {
            crate::GatewayError::config(msg!(
                "gw.route.all_selected_disabled", detail = why =>
                "Every upstream the route selected is disabled: {detail}"
            ))
        } else {
            crate::GatewayError::new(
                crate::error::Source::Request,
                msg!(
                    "gw.model.no_upstream_available", model = model, detail = why =>
                    "No upstream is available to serve model {model}: {detail}"
                ),
            )
        }
    }
}

/// 这家能不能服务这个模型名：不在启用范围里、清单里没有，都不能。**不看
/// 停用** —— 停用是路由的事，测速一家停用的上游是合理的。
///
/// **`model` 是发出去的名字，原样看，不经过别名表。**客户端说的名称（可能是别名）
/// 先用 [`resolve`] 对到这家，或者直接用 [`sent_to`]。
pub fn fit(catalog: &tw_engine::Catalog, p: &tw_config::Provider, model: &str) -> Option<Skip> {
    if !p.uses_model(model) {
        Some(Skip::OutOfScope)
    } else if catalog.offers(&p.name, model) == Some(false) {
        Some(Skip::NotOffered)
    } else {
        None
    }
}

/// 密钥 `key` 的模型范围（`allow`）。没有这把密钥、或者它没写 `allow` 时是 `None`：什么都放行。
pub fn key_allow<'a>(cfg: &'a tw_config::Config, key: &str) -> Option<&'a [String]> {
    cfg.clients
        .iter()
        .find(|c| c.name == key)
        .and_then(|c| c.allow.as_deref())
}

/// 密钥的 `allow` 放不放行要发给一家的名字 `model`。`allow` 是 `None`（不写）时什么都放行。
///
/// - **客户端那一侧的名称**（客户端写的、阶段一改写的、插件改的，可能是别名）按目录的规矩
///   看（[`tw_engine::Catalog::allows`]）：写上游的模型名也放行列了它的别名，和准入、
///   `/v1/models` 是同一套。
/// - **原样发出的名字**（`as_written`：指定模型、阶段二改的）按这个名字本身对 glob：它不经过
///   别名表，也就没有别名可继承。
///
/// **每个候选各看各的**：指定模型、阶段二改的名字一家一个，准入只要有一家过得去就放行，
/// 剩下那几家要在这里拦住 —— 不然故障转移会把请求带到密钥不让用的模型上。
pub fn allowed(
    catalog: &tw_engine::Catalog,
    allow: Option<&[String]>,
    model: &str,
    as_written: bool,
) -> bool {
    allow.is_none_or(|patterns| {
        if as_written {
            patterns
                .iter()
                .any(|p| tw_engine::rule::glob_match(p, model))
        } else {
            catalog.allows(model, patterns)
        }
    })
}

/// 发给上游 `p` 时，客户端说的 `name` 叫什么。`None` = 这家服务不了这个别名。
///
/// - `name` 是别名：按列表顺序取第一个**这家能服务**的名称 —— 在它的启用范围里
///   （`models_only`），清单里也没说没有（没有清单的上游当作有，取第一个在启用
///   范围里的）。一个都没有就是 `None`。
/// - 不是别名：就是 `name` 本身。这家有没有它不在这里判断，那是 [`fit`] 的事。
///
/// **别名优先**：`name` 是别名时，这家恰好有一个同名的真模型、却没列进别名，
/// 也不发这个名字给它。
///
/// **不看停用**，和 [`fit`] 一样：停用是路由的事。指定模型（规则的 `to` 列出的
/// 「上游 + 模型」）不经过这里，那个名字原样发出。
pub fn resolve(
    cfg: &tw_config::Config,
    catalog: &tw_engine::Catalog,
    p: &tw_config::Provider,
    name: &str,
) -> Option<String> {
    let Some(alias) = cfg.aliases.find(name) else {
        return Some(name.to_string());
    };
    alias
        .models
        .iter()
        .find(|m| p.uses_model(m) && catalog.offers(&p.name, m) != Some(false))
        .cloned()
}

/// 发给候选 `p` 的模型名；这家服务不了时是为什么。
///
/// `asked` 是这一家被要的名称（[`tw_engine::Engine::models_asked`]），在客户端那一侧：
/// 别名还是别名名称。
///
/// - **指定模型**（[`tw_engine::Decision::pinned_model`]）：原样发出，不经过别名表，
///   就看这家有没有这个名字。
/// - 别的按 [`resolve`] 对到这家：别名取它列表里这家能服务的第一个，一个都没有就
///   服务不了 —— 都不在启用范围里算范围外，否则算清单里没有。
pub fn sent_to(
    cfg: &tw_config::Config,
    catalog: &tw_engine::Catalog,
    decision: &tw_engine::Decision,
    p: &tw_config::Provider,
    asked: &str,
) -> Result<String, Skip> {
    let sent = match decision.pinned_model(&p.name) {
        Some(m) => m.to_string(),
        None => match resolve(cfg, catalog, p, asked) {
            Some(m) => m,
            None => {
                let in_scope = cfg
                    .aliases
                    .find(asked)
                    .is_some_and(|a| a.models.iter().any(|m| p.uses_model(m)));
                return Err(if in_scope {
                    Skip::NotOffered
                } else {
                    Skip::OutOfScope
                });
            }
        },
    };
    match fit(catalog, p, &sent) {
        Some(skip) => Err(skip),
        None => Ok(sent),
    }
}

/// 在路由选出的候选里去掉服务不了这个请求的上游。
///
/// `asked` 是每个候选和它要的模型（[`tw_engine::Engine::models_asked`]）：
/// **每一家按它实际要的那个模型看**，规则改写过的就是改写后的；别名按这家能不能
/// 服务它看，指定模型按指定的那个名字看（见 [`sent_to`]）。
///
/// 密钥的 `allow`（[`key_allow`]）也一家一家地看（[`allowed`]）：指定模型按指定的名字，
/// 别的按它要的、客户端那一侧的名称。不让用的跳过（[`Skip::NotAllowed`]）。
///
/// **没有模型清单的上游不跳过**：不知道它有什么，不等于它没有。模型是空的
/// （请求体解析不了）时只看停用。
pub fn serving(
    cfg: &tw_config::Config,
    catalog: &tw_engine::Catalog,
    decision: &tw_engine::Decision,
    asked: &[(String, String)],
    allow: Option<&[String]>,
) -> Serving {
    let mut out = Serving {
        asked: asked.to_vec(),
        ..Serving::default()
    };
    for (name, model) in asked {
        let Some(p) = cfg.providers.iter().find(|p| &p.name == name) else {
            // 配置里没有这一家：留给尝试那一步报出来
            out.usable.push(name.clone());
            continue;
        };
        let pinned = decision.pinned_model(name);
        let skip = if p.disabled {
            Some(Skip::Disabled)
        } else if model.is_empty() {
            None
        } else if !allowed(catalog, allow, pinned.unwrap_or(model), pinned.is_some()) {
            Some(Skip::NotAllowed)
        } else {
            sent_to(cfg, catalog, decision, p, model).err()
        };
        match skip {
            Some(s) => out.skipped.push((name.clone(), s)),
            None => out.usable.push(name.clone()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一句原因。码不要紧，这里比的是哪一句
    fn why(text: &str) -> tw_types::Msg {
        tw_types::Msg {
            code: "test.why".into(),
            args: Default::default(),
            text: text.into(),
        }
    }

    fn provider(name: &str) -> tw_config::Provider {
        tw_config::Provider {
            name: name.into(),
            base_url: format!("https://{name}.example"),
            key: Some("sk".into()),
            ..Default::default()
        }
    }

    /// 每个候选都要同一个模型：没有规则改写时的样子
    fn asked(candidates: &[&str], model: &str) -> Vec<(String, String)> {
        candidates
            .iter()
            .map(|c| (c.to_string(), model.to_string()))
            .collect()
    }

    /// 路由的决定。`pinned` 是指定模型的那几项（上游、模型），空的就是按名称要
    fn decided(pinned: &[(&str, &str)]) -> tw_engine::Decision {
        tw_engine::Decision {
            candidates: pinned.iter().map(|(p, _)| p.to_string()).collect(),
            matched_rule: "r".into(),
            via_group: None,
            set: Default::default(),
            rewritten_by: Vec::new(),
            pinned: pinned
                .iter()
                .map(|(p, m)| tw_engine::Pinned {
                    provider: p.to_string(),
                    model: m.to_string(),
                })
                .collect(),
        }
    }

    fn cfg(providers: Vec<tw_config::Provider>) -> tw_config::Config {
        tw_config::Config {
            providers,
            ..Default::default()
        }
    }

    fn published(d: &Directory, c: &tw_config::Config) -> tw_engine::Catalog {
        let into = arc_swap::ArcSwap::from_pointee(tw_engine::Catalog::default());
        let c = std::sync::Arc::new(c.clone());
        d.publish(|| c.clone(), &into);
        (*into.load_full()).clone()
    }

    #[test]
    fn an_edit_that_does_not_change_how_to_reach_an_upstream_keeps_its_answer() {
        let d = Directory::default();
        let mut c = cfg(vec![provider("a"), provider("b")]);
        assert!(d.reconcile(&c));
        for p in &c.providers {
            d.record(
                &p.name,
                &identity(&c, p),
                Answer::Listed(vec![format!("{}-m", p.name)]),
                1,
            );
        }
        // 改计费方式、启用范围：不用重问
        c.providers[0].billing = tw_config::Billing::Free;
        c.providers[0].models_only = Some(vec!["a-*".into()]);
        assert!(!d.reconcile(&c));
        assert_eq!(d.listing(&c.providers[0]).models, ["a-m"]);
        // 改地址：重问，旧答案不再作数
        c.providers[1].base_url = "https://elsewhere.example".into();
        assert!(d.reconcile(&c));
        let l = d.listing(&c.providers[1]);
        assert_eq!(l.source, Source::None);
        assert_eq!(d.due(&c, 2), ["b"]);
        // 删掉的上游忘掉
        c.providers.remove(0);
        d.reconcile(&c);
        assert!(d.lock().get("a").is_none());
    }

    /// 别名对到每一家：按列表顺序取它能服务的第一个名称；范围外的、清单里没有的跳过；
    /// 没有清单的当作有。不是别名的就是它自己
    #[test]
    fn an_alias_resolves_to_the_first_listed_name_each_upstream_can_serve() {
        let mut c = cfg(vec![
            provider("bedrock"),
            provider("anthropic"),
            provider("relay"),
            provider("no-list"),
            provider("scoped"),
        ]);
        c.providers[4].models_only = Some(vec!["anthropic/*".into()]);
        c.aliases = serde_yaml_ng::from_str(
            "claude-sonnet-5: [claude-sonnet-5, us.anthropic.claude-sonnet-5-v1:0, anthropic/claude-sonnet-5]\n\
             gpt-x: gpt-x-2026\n",
        )
        .unwrap();
        let catalog = tw_engine::Catalog::build(&[
            listed(
                "bedrock",
                &["us.anthropic.claude-sonnet-5-v1:0", "claude-sonnet-5"],
            ),
            listed("anthropic", &["claude-sonnet-5"]),
            // 有同名的真模型，但只有它没列进别名的那个名字
            listed("relay", &["gpt-x", "anthropic/claude-sonnet-5"]),
            tw_engine::ProviderModels {
                provider: "no-list".into(),
                protocol: "anthropic".into(),
                known: false,
                models: Vec::new(),
            },
            tw_engine::ProviderModels {
                provider: "scoped".into(),
                protocol: "anthropic".into(),
                known: false,
                models: Vec::new(),
            },
        ]);
        let r = |p: usize, name: &str| resolve(&c, &catalog, &c.providers[p], name);
        // 按列表顺序：bedrock 两个都有，取排在前面的那个
        assert_eq!(r(0, "claude-sonnet-5").as_deref(), Some("claude-sonnet-5"));
        assert_eq!(r(1, "claude-sonnet-5").as_deref(), Some("claude-sonnet-5"));
        assert_eq!(
            r(2, "claude-sonnet-5").as_deref(),
            Some("anthropic/claude-sonnet-5")
        );
        // 没有清单：当作有，取第一个
        assert_eq!(r(3, "claude-sonnet-5").as_deref(), Some("claude-sonnet-5"));
        // 没有清单、有启用范围：取第一个在范围里的
        assert_eq!(
            r(4, "claude-sonnet-5").as_deref(),
            Some("anthropic/claude-sonnet-5")
        );
        // 别名优先：relay 有个叫 gpt-x 的真模型，没列进别名，不发给它
        assert_eq!(r(2, "gpt-x"), None);
        assert_eq!(r(1, "gpt-x"), None);
        // 不是别名：原样，有没有交给 `fit`
        assert_eq!(r(1, "claude-opus-5").as_deref(), Some("claude-opus-5"));
    }

    /// 别名登记进汇总；挑候选时别名按每家能不能服务它看，指定模型按写的那个名字看
    #[test]
    fn candidates_are_judged_by_the_alias_each_can_serve_and_pinned_models_as_written() {
        let d = Directory::default();
        let mut c = cfg(vec![
            provider("bedrock"),
            provider("relay"),
            provider("openai"),
            provider("no-list"),
            provider("scoped"),
        ]);
        c.providers[4].models_only = Some(vec!["gemini-*".into()]);
        c.aliases = serde_yaml_ng::from_str(
            "claude-sonnet-5: [claude-sonnet-5, us.anthropic.claude-sonnet-5-v1:0]\n\
             gpt-x: gpt-x-2026\n",
        )
        .unwrap();
        d.reconcile(&c);
        for (p, models) in [
            ("bedrock", vec!["us.anthropic.claude-sonnet-5-v1:0"]),
            // 有一个叫 gpt-x 的真模型，别名 gpt-x 没列它
            ("relay", vec!["gpt-x", "claude-sonnet-5"]),
            ("openai", vec!["gpt-x-2026"]),
            ("scoped", vec!["gemini-3"]),
        ] {
            let i = c.providers.iter().position(|x| x.name == p).unwrap();
            let models = models.into_iter().map(String::from).collect();
            d.record(p, &identity(&c, &c.providers[i]), Answer::Listed(models), 1);
        }
        d.record(
            "no-list",
            &identity(&c, &c.providers[3]),
            Answer::NoList(why("没有接口")),
            1,
        );
        let cat = published(&d, &c);
        // 别名登记在有清单、能服务它的上游名下；同名的真模型让给别名
        assert_eq!(cat.providers_for("claude-sonnet-5"), ["bedrock", "relay"]);
        assert_eq!(cat.providers_for("gpt-x"), ["openai"]);
        assert_eq!(cat.offers("relay", "gpt-x"), Some(true));

        let all = ["bedrock", "relay", "openai", "no-list", "scoped"];
        let s = serving(&c, &cat, &decided(&[]), &asked(&all, "gpt-x"), None);
        // relay 只有同名的真模型：别名优先，到不了它。没有清单的照样能
        assert_eq!(s.usable, ["openai", "no-list"]);
        assert_eq!(
            s.skipped,
            [
                ("bedrock".to_string(), Skip::NotOffered),
                ("relay".to_string(), Skip::NotOffered),
                ("scoped".to_string(), Skip::OutOfScope),
            ]
        );
        let unpinned = decided(&[]);
        let sent = |p: usize, name: &str| sent_to(&c, &cat, &unpinned, &c.providers[p], name);
        assert_eq!(
            sent(0, "claude-sonnet-5"),
            Ok("us.anthropic.claude-sonnet-5-v1:0".into())
        );
        assert_eq!(sent(1, "claude-sonnet-5"), Ok("claude-sonnet-5".into()));
        assert_eq!(sent(3, "gpt-x"), Ok("gpt-x-2026".into()));
        // 不是别名的照旧按清单看
        assert_eq!(sent(2, "gpt-x-2026"), Ok("gpt-x-2026".into()));
        assert_eq!(sent(2, "gpt-9"), Err(Skip::NotOffered));

        // 指定模型原样看，不经过别名表：relay 的 gpt-x 就是它自己的那个；bedrock 没有
        // 叫 claude-sonnet-5 的（虽然它能服务这个别名）
        let pinned = decided(&[("relay", "gpt-x"), ("bedrock", "claude-sonnet-5")]);
        let s = serving(
            &c,
            &cat,
            &pinned,
            &[
                ("relay".into(), "gpt-x".into()),
                ("bedrock".into(), "claude-sonnet-5".into()),
            ],
            None,
        );
        assert_eq!(s.usable, ["relay"]);
        assert_eq!(s.skipped, [("bedrock".to_string(), Skip::NotOffered)]);
        assert_eq!(
            sent_to(&c, &cat, &pinned, &c.providers[1], "gpt-x"),
            Ok("gpt-x".into())
        );

        // 密钥的 allow 一家一家地看。指定的名字原样对 glob：只许 gpt-x-* 的密钥，relay 的
        // gpt-x 不让用，跳过；openai 的 gpt-x-2026 照常
        let narrow = ["gpt-x-*".to_string()];
        let pinned = decided(&[("relay", "gpt-x"), ("openai", "gpt-x-2026")]);
        let both: [(String, String); 2] = [
            ("relay".into(), "gpt-x".into()),
            ("openai".into(), "gpt-x-2026".into()),
        ];
        let s = serving(&c, &cat, &pinned, &both, Some(&narrow));
        assert_eq!(s.usable, ["openai"]);
        assert_eq!(s.skipped, [("relay".to_string(), Skip::NotAllowed)]);
        // 写别名的密钥只管别名本身：指定的名字恰好和别名同名，不算
        let s = serving(&c, &cat, &pinned, &both, Some(&["gpt-x".to_string()]));
        assert_eq!(s.usable, ["relay"]);
        assert_eq!(s.skipped, [("openai".to_string(), Skip::NotAllowed)]);
        // 按名称要别名：写上游模型名的密钥继承到别名（gpt-x 列了 gpt-x-2026）
        let s = serving(
            &c,
            &cat,
            &decided(&[]),
            &asked(&["openai"], "gpt-x"),
            Some(&narrow),
        );
        assert_eq!(s.usable, ["openai"]);
        let s = serving(
            &c,
            &cat,
            &decided(&[]),
            &asked(&["openai"], "gpt-x"),
            Some(&["claude-*".to_string()]),
        );
        assert_eq!(s.skipped, [("openai".to_string(), Skip::NotAllowed)]);
        // 停用的照旧说停用：那一条更要紧
        let mut off = c.clone();
        off.providers[2].disabled = true;
        let s = serving(&off, &cat, &pinned, &both, Some(&["claude-*".to_string()]));
        assert_eq!(
            s.skipped,
            [
                ("relay".to_string(), Skip::NotAllowed),
                ("openai".to_string(), Skip::Disabled),
            ]
        );
    }

    fn listed(p: &str, models: &[&str]) -> tw_engine::ProviderModels {
        tw_engine::ProviderModels {
            provider: p.into(),
            protocol: "anthropic".into(),
            known: true,
            models: models.iter().map(|m| m.to_string()).collect(),
        }
    }

    #[test]
    fn an_answer_for_an_upstream_that_changed_meanwhile_is_dropped() {
        let d = Directory::default();
        let mut c = cfg(vec![provider("a")]);
        d.reconcile(&c);
        let before = identity(&c, &c.providers[0]);
        c.providers[0].base_url = "https://elsewhere.example".into();
        d.reconcile(&c);
        d.record("a", &before, Answer::Listed(vec!["stale".into()]), 1);
        assert_eq!(d.listing(&c.providers[0]).source, Source::None);
    }

    #[test]
    fn the_catalog_leaves_out_disabled_upstreams_and_models_out_of_scope() {
        let d = Directory::default();
        let mut c = cfg(vec![provider("a"), provider("b"), provider("c")]);
        d.reconcile(&c);
        for p in &c.providers {
            d.record(
                &p.name,
                &identity(&c, p),
                Answer::Listed(vec![format!("{}-1", p.name), format!("{}-2", p.name)]),
                1,
            );
        }
        c.providers[1].disabled = true;
        c.providers[2].models_only = Some(vec!["c-1".into()]);
        let cat = published(&d, &c);
        assert_eq!(cat.all(), ["a-1", "a-2", "c-1"]);
        let s = serving(
            &c,
            &cat,
            &decided(&[]),
            &asked(&["a", "b", "c"], "c-2"),
            None,
        );
        assert_eq!(
            s.skipped,
            [
                ("a".to_string(), Skip::NotOffered),
                ("b".to_string(), Skip::Disabled),
                ("c".to_string(), Skip::OutOfScope)
            ]
        );
        assert!(s.usable.is_empty());
    }

    #[test]
    fn an_upstream_without_a_list_is_kept_and_a_failure_retries_sooner_than_a_day() {
        let d = Directory::default();
        let c = cfg(vec![provider("a"), provider("b")]);
        d.reconcile(&c);
        d.record(
            "a",
            &identity(&c, &c.providers[0]),
            Answer::Failed(why("连不上")),
            0,
        );
        d.record(
            "b",
            &identity(&c, &c.providers[1]),
            Answer::NoList(why("没有接口")),
            0,
        );
        let cat = published(&d, &c);
        // 不知道它们有什么：不跳过
        let s = serving(&c, &cat, &decided(&[]), &asked(&["a", "b"], "m"), None);
        assert_eq!(s.usable, ["a", "b"]);
        // 失败的一小时后重问，没有接口的一天后
        let hour = RETRY_AFTER.as_millis() as u64;
        assert_eq!(d.due(&c, hour), ["a"]);
        assert_eq!(d.due(&c, REFRESH_EVERY.as_millis() as u64), ["a", "b"]);
        assert_eq!(
            d.listing(&c.providers[1])
                .error
                .as_ref()
                .map(|m| m.text.as_str()),
            Some("没有接口")
        );
    }

    #[test]
    fn a_page_visit_asks_what_is_missing_failed_or_old_and_nothing_it_just_asked() {
        let d = Directory::default();
        let names = [
            "new",
            "failed",
            "failed-now",
            "listed",
            "listed-old",
            "no-list",
            "asking",
        ];
        let mut c = cfg(names.iter().map(|n| provider(n)).collect());
        c.providers.push(tw_config::Provider {
            disabled: true,
            ..provider("off")
        });
        d.reconcile(&c);
        let day = REFRESH_EVERY.as_millis() as u64;
        let now = day * 2;
        let minute = RECHECK_GAP.as_millis() as u64;
        let id = |n: &str| identity(&c, c.providers.iter().find(|p| p.name == n).unwrap());
        d.record(
            "failed",
            &id("failed"),
            Answer::Failed(why("连不上")),
            now - minute,
        );
        d.record(
            "failed-now",
            &id("failed-now"),
            Answer::Failed(why("连不上")),
            now - 1,
        );
        d.record(
            "listed",
            &id("listed"),
            Answer::Listed(vec!["m".into()]),
            now - minute,
        );
        d.record(
            "listed-old",
            &id("listed-old"),
            Answer::Listed(vec!["m".into()]),
            now - day,
        );
        d.record(
            "no-list",
            &id("no-list"),
            Answer::NoList(why("没有接口")),
            now - minute,
        );
        d.begin("asking", &id("asking"));
        // 没问过的、失败了一阵的、过期的；刚问过的、不给清单的、正在问的、
        // 停用的都不问
        assert_eq!(d.stale(&c, now), ["new", "failed", "listed-old"]);
    }

    #[test]
    fn two_asks_at_once_keep_it_fetching_until_both_are_back() {
        let d = Directory::default();
        let c = cfg(vec![provider("a")]);
        d.reconcile(&c);
        let id = identity(&c, &c.providers[0]);
        let p = &c.providers[0];
        assert_eq!(d.listing(p).status, Status::Pending);
        assert!(!d.listing(p).fetching);
        d.begin("a", &id);
        d.begin("a", &id);
        d.record("a", &id, Answer::Failed(why("连不上")), 1);
        // 一个回来了，另一个还在问；上一次的答案照常可用
        let l = d.listing(p);
        assert!(l.fetching);
        assert_eq!(l.status, Status::Failed);
        assert_eq!(l.error.as_ref().map(|m| m.text.as_str()), Some("连不上"));
        // 正在问的不再排进后台那一轮
        assert!(d.due(&c, REFRESH_EVERY.as_millis() as u64).is_empty());
        d.record("a", &id, Answer::Listed(vec!["m".into()]), 2);
        let l = d.listing(p);
        assert!(!l.fetching);
        assert_eq!((l.status, l.source), (Status::Listed, Source::Discovered));
        assert_eq!(l.error, None);
    }

    #[test]
    fn an_ask_dropped_because_the_config_changed_leaves_nothing_fetching() {
        let d = Directory::default();
        let mut c = cfg(vec![provider("a")]);
        d.reconcile(&c);
        let before = identity(&c, &c.providers[0]);
        d.begin("a", &before);
        c.providers[0].base_url = "https://elsewhere.example".into();
        d.reconcile(&c);
        d.abandon("a", &before);
        let l = d.listing(&c.providers[0]);
        assert!(!l.fetching);
        assert_eq!(l.status, Status::Pending);
        // 还没对过配置的一家（刚加上）：开始问时补一条，答案有地方记
        let fresh = provider("fresh");
        let id = identity(&c, &fresh);
        d.begin("fresh", &id);
        d.record("fresh", &id, Answer::Listed(vec!["m".into()]), 1);
        assert_eq!(d.listing(&fresh).status, Status::Listed);
    }

    #[test]
    fn a_failure_with_a_manual_list_says_both() {
        let d = Directory::default();
        let mut c = cfg(vec![provider("a")]);
        c.providers[0].models = vec!["手写".into()];
        d.reconcile(&c);
        d.record(
            "a",
            &identity(&c, &c.providers[0]),
            Answer::Failed(why("密钥被拒")),
            1,
        );
        let l = d.listing(&c.providers[0]);
        // 清单来自手写的兜底，但「问了、没问到、为什么」一样要说
        assert_eq!((l.source, l.status), (Source::Manual, Status::Failed));
        assert_eq!(l.models, ["手写"]);
        assert_eq!(l.error.as_ref().map(|m| m.text.as_str()), Some("密钥被拒"));
    }
}
