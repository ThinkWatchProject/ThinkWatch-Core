//! 控制面：unix socket 上的 HTTP。
//!
//! **不占 TCP 端口**。理由不是省端口，是权限：一个 `0700` 的 socket 文件
//! 天然只有当前用户能连。
//!
//! 但那是文件系统给的保证，**只在这个平台上成立** —— Windows 上没有对等物，
//! 控制面在那里只能落在回环 TCP 上。所以每条连接先握手（`tw-link`，钥匙是
//! 配置里的 `listen.control.key`），这道门是另外装的一道，每一种通道都走它，
//! 见 `gate`。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::{Json, Router};
use futures::stream::Stream;
use tokio::sync::broadcast;
use tw_api::ep;
use tw_types::{Msg, msg};

use contract::RouterExt;

pub mod chatgpt;
pub mod clients;
pub mod config;
mod contract;
pub mod diagnostics;
pub mod dryrun;
mod gate;
pub mod keys;
pub mod listen;
pub mod pricing;
pub mod remote;
pub mod replay;
pub mod resources;
pub mod rotation;
pub mod routes;
pub mod security;
pub mod shutdown;
pub mod zai;
pub use config::{ApplyError, ConfigManager, resolve_path, spawn_watcher};
pub use shutdown::Shutdown;
pub use tw_observe::EventBus;

#[derive(Clone)]
pub struct ControlState {
    pub started: std::time::Instant,
    /// **数据面本身**，不是它启动时的那份配置快照。
    ///
    /// 拿快照的后果是：用户在编辑器里改完文件、网关已经按新配置在转发
    /// 了，而界面上还显示着旧的 —— 而他分不清是我们没生效还是界面没
    /// 刷新。
    pub gateway: tw_gateway::AppState,
    pub cfg: Arc<ConfigManager>,
    /// 请求历史。**可能没有** —— 磁盘起不来时观测这一层整个不在，
    /// 而那时网关照常转发，所以它是 Option 而不是必需品。
    pub store: Option<Arc<tokio::sync::Mutex<tw_store::Recorder>>>,
    /// 定期刷新默认价目表的那个任务。**价格本身不在这里**，在网关的价格簿里
    pub price_updater: Arc<pricing::Updater>,
    /// ChatGPT 账号的登录。**同一时刻只有一次**：回调端口只有一个
    pub chatgpt: Arc<chatgpt::Accounts>,
    /// Z.ai / BigModel 账号的登录。**同一时刻也只有一次** —— 理由不是端口，是
    /// 「一次登录」在界面上就是一件正在进行的事，两件同时进行没人说得清哪件成了
    pub zai: Arc<zai::Accounts>,
    /// 请网关退出的那个开关。控制面上的 `POST /shutdown` 扳它，主循环等它。
    pub shutdown: Shutdown,
    /// 远程控制端口此刻在听哪儿、谁被晾着。
    pub remote: Arc<remote::Remote>,
}

impl ControlState {
    /// 当前生效的配置。**每次现取** —— 见上面那条注释。
    pub fn config(&self) -> Arc<tw_config::Config> {
        self.gateway.config()
    }
    pub fn config_path(&self) -> &std::path::Path {
        self.cfg.path()
    }
    pub fn bus(&self) -> &EventBus {
        &self.gateway.bus
    }
    pub fn health(&self) -> &Arc<tw_gateway::Health> {
        &self.gateway.health
    }
}

pub fn router(state: ControlState) -> Router {
    Router::new()
        .at(ep::Status, status)
        .at(ep::Shutdown, ask_shutdown)
        .at(ep::Interfaces, interfaces)
        .merge(keys::router())
        .merge(listen::router())
        .at(ep::Events, events)
        .at(ep::InFlight, in_flight)
        .at(ep::Live, live)
        .at(ep::Overview, overview)
        .at(ep::L1, l1)
        .at(ep::GetConfig, get_config)
        .at(ep::PatchConfig, patch_config)
        .at(ep::PutConfig, put_config)
        .at(ep::ConfigHistory, config_history)
        .at(ep::ConfigAt, config::path_at)
        .at(ep::ConfigRollback, config_rollback)
        .at(ep::Summary, summary)
        .at(ep::CostBuckets, cost_buckets)
        .at(ep::CostBucketsBy, cost_buckets_by)
        .at(ep::CostBy, cost_by)
        .at(ep::History, history)
        .at(ep::Latency, latency)
        .at(ep::LatencyByProvider, latency_by_provider)
        .at(ep::Storage, storage)
        .at(ep::Quota, quota)
        .at(ep::SpeedQuote, speed_quote)
        .at(ep::SpeedRun, speed_run)
        .at(ep::RequestDetail, request_detail)
        // 诊断包（脱敏纪律）。**只读，不写任何文件**
        .at(ep::Diagnostics, diagnostics::bundle)
        // 把一条真实请求变成回放用例。**录制不是新功能** ——
        // 每个请求本来就在存储里
        .at(ep::Fixture, replay::fixture)
        // **报价和真跑是两个端点**：这一步花钱（和 L3 测速同一条纪律）
        .at(ep::ReplayQuote, replay::quote)
        .at(ep::ReplayRun, replay::run)
        .at(ep::Sessions, sessions)
        .at(ep::SessionDetail, session_detail)
        .at(ep::DryRun, dryrun::dry_run)
        // 为客户端发专用密钥。接管本身在桌面端做
        .at(ep::ClientKey, clients::client_key)
        .merge(resources::router())
        .merge(routes::router())
        .merge(security::router())
        .merge(pricing::router())
        .merge(chatgpt::router())
        .merge(zai::router())
        .fallback(contract::no_such_endpoint)
        .layer(axum::middleware::from_fn(contract::errors_are_messages))
        .with_state(state)
}

/// 这台机器上有哪些网卡。
///
/// 不带任何状态 —— 每次现问系统。**网卡是会变的**：插拔网线、连上另一
/// 个 Wi-Fi、起一条 VPN，清单就不一样了，缓存下来只会让选单里出现一个
/// 已经不存在的地址。
async fn interfaces() -> Json<Vec<tw_api::NicView>> {
    Json(
        tw_config::nics::by_name()
            .into_iter()
            .map(|n| tw_api::NicView {
                name: n.name,
                loopback: n.addr.is_loopback(),
                addr: n.addr.to_string(),
            })
            .collect(),
    )
}

/// 请网关退出。
///
/// 理由见 [`shutdown`] 那个模块：Windows 上没有 SIGTERM，而桌面端要在改完
/// 配置之后重启 core、在装更新之前停掉它并且等它真的退出。
async fn ask_shutdown(State(s): State<ControlState>) -> (StatusCode, Json<Msg>) {
    // 服务器上 core 归 systemd 管：从远程关掉它，就只能去服务器上重新拉起
    if remote::is_remote() {
        return remote::refused(remote::Refused::Shutdown);
    }
    tracing::info!("asked to shut down over the control plane");
    // **先把话说完再退。**立刻扳开关的话，这条响应可能还没写出去进程就没了，
    // 而客户端看到的是连接被重置 —— 和「core 崩了」长得一模一样，偏偏这是
    // 它自己要求的那一次退出。
    let sw = s.shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        sw.ask();
    });
    (
        StatusCode::ACCEPTED,
        Json(msg!("control.shutdown" => "The gateway is shutting down.")),
    )
}

async fn status(State(s): State<ControlState>) -> Json<tw_api::Status> {
    let cfg = s.config();
    let listening = s.gateway.listening();
    let rl = s.remote.listening();
    let gateway_reachable = listening
        .primary()
        .map(remote::reachable)
        .unwrap_or_default();
    let rcfg = cfg.listen.control.remote.as_ref();
    let remote_control = tw_api::RemoteControlView {
        enabled: rcfg.is_some_and(|r| r.enabled),
        addr: rl.addr.map(|a| a.to_string()),
        error: rl.error,
        allow_from: rcfg.map(|r| r.allow_from.clone()).unwrap_or_default(),
        reachable: rl.addr.map(remote::reachable).unwrap_or_default(),
    };
    Json(tw_api::Status {
        api_version: tw_api::CONTROL_API_VERSION,
        version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
        // **问监听器，不是问启动时记下的那一次。**安全模式下数据面从没起过，
        // 这里自然是 None
        gateway_addr: listening.primary().map(|a| a.to_string()),
        listen_error: listening.error,
        config_rejected: s.cfg.rejected(),
        config_path: s.config_path().display().to_string(),
        clients: cfg.clients.len(),
        providers: cfg.providers.len(),
        uptime_secs: s.started.elapsed().as_secs(),
        in_flight: s.gateway.live.count(),
        remote_control,
        gateway_reachable,
    })
}

async fn events(
    State(s): State<ControlState>,
) -> Sse<impl Stream<Item = Result<SseEvent, std::convert::Infallible>>> {
    let rx = s.bus().subscribe();
    let stream = async_stream_from(rx, s.bus().clone());
    // 心跳。UI 那边要能区分「没有请求」和「连接断了」—— 没有心跳的话
    // 一个安静的下午看起来就像挂了。
    Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
}

/// 此刻还在跑的请求：它们的开始事件，原样（见 `EventBus::in_flight`）。
///
/// **半路才开始听 `/events` 的一方先问这个。**不问的话，订阅之前就开始了
/// 的请求它一个都不知道，直到它们结束 —— 数「进行中」就会少数。
async fn in_flight(State(s): State<ControlState>) -> Json<Vec<tw_api::Event>> {
    Json(s.bus().in_flight())
}

/// 在跑的请求和最近的生成速率（见 `EventBus::live`）。菜单栏每次重收都问它，
/// 不必自己听事件去数。
async fn live(State(s): State<ControlState>) -> Json<tw_api::LiveView> {
    Json(s.bus().live())
}

/// 事件流编码成 SSE。
fn async_stream_from(
    rx: broadcast::Receiver<tw_api::Event>,
    bus: EventBus,
) -> impl Stream<Item = Result<SseEvent, std::convert::Infallible>> {
    use futures::StreamExt;
    event_stream(rx, bus).map(|ev| {
        let data = serde_json::to_string(&ev).unwrap_or_default();
        Ok(SseEvent::default().data(data))
    })
}

/// 一个订阅者收到的事件。`bus` 用来给掉队的那条标记取一个号。
fn event_stream(
    mut rx: broadcast::Receiver<tw_api::Event>,
    bus: EventBus,
) -> impl Stream<Item = tw_api::Event> {
    futures::stream::unfold((rx_state(&mut rx), bus), |(mut rx, bus)| async move {
        let ev = match rx.recv().await {
            Ok(ev) => ev,
            // 订阅者跟不上时 broadcast 会丢最老的。**继续收而不是断开**（断掉重连
            // 丢得更多），**但要告诉它丢了**：它手上只靠增量维护的状态从这一刻起
            // 不可信，得自己对一次账（见 `Event::EventsDropped`）
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(
                    dropped = n,
                    "a control-plane subscriber fell behind; some events were dropped"
                );
                tw_api::Event::EventsDropped {
                    id: bus.next_id(),
                    count: n,
                    at_ms: config::now_ms(),
                }
            }
            Err(broadcast::error::RecvError::Closed) => return None,
        };
        Some((ev, (rx, bus)))
    })
}

fn rx_state(rx: &mut broadcast::Receiver<tw_api::Event>) -> broadcast::Receiver<tw_api::Event> {
    rx.resubscribe()
}

/// 界面要显示的配置概览。**密钥只给来源，不给值。**
async fn overview(State(s): State<ControlState>) -> Json<tw_api::Overview> {
    let cfg = s.config();
    let cfg = &*cfg;
    let engine = cfg.engine();
    // 正文现在占了多少。**扫的是目录，所以放在这儿算一次**，不塞进
    // 下面那个视图表达式里
    let body_bytes_now = match &s.store {
        Some(st) => st.lock().await.blobs().total_bytes(),
        None => 0,
    };
    Json(tw_api::Overview {
        // **磁盘上那一份的版本，不是正在服务的那一份。**改配置时核对的是
        // 文件（见 `ConfigManager::write`）。两者只在文件被改坏、旧配置还在
        // 服务时不一样：这时拿文件的版本去改，得到的是「文件读不懂」，而不是
        // 一句刷新多少次都没用的「版本对不上」。读不到文件时是空串，拿它去
        // 改会得到读不到文件的那个错误。
        config_version: s.cfg.current().map(|l| l.version()).unwrap_or_default(),
        proxies: cfg
            .proxies
            .iter()
            .map(|x| tw_api::ProxyView {
                name: x.name.clone(),
                kind: x.kind.into(),
                addr: x.addr.clone(),
                // **密码不出这个函数。**它和上游的 key 是同一类东西，
                // 而这个视图会进日志、进诊断包、进用户贴出来的截图。
                has_auth: x.auth.is_some(),
                used_by: tw_config::refs::proxy_users(cfg, &x.name),
                unreachable: s.gateway.proxy_fault(&x.name),
            })
            .collect(),
        providers: cfg
            .providers
            .iter()
            .map(|p| provider_view(&s, cfg, p))
            .collect(),
        routes: engine
            .routes()
            .iter()
            .map(|set| tw_api::RouteView {
                name: set.name.clone(),
                default: set.name == engine.default_route(),
                builtin: engine.is_builtin_route(&set.name),
                has_catch_all: tw_engine::has_catch_all(&set.rules),
                clients: cfg
                    .clients
                    .iter()
                    .filter(|c| c.route.as_deref() == Some(set.name.as_str()))
                    .map(|c| c.name.clone())
                    .collect(),
                rules: set
                    .rules
                    .iter()
                    .zip(tw_engine::notes(&set.rules))
                    .map(|(r, n)| routes::rule_view(r, n))
                    .collect(),
            })
            .collect(),
        groups: engine
            .groups()
            .iter()
            .map(|g| tw_api::GroupView {
                name: g.name.clone(),
                builtin: tw_engine::is_builtin_group(&g.name),
                kind: routes::group_kind(g.kind),
                session_affinity: g.session_affinity,
                selected: g.selected.clone(),
                providers: g.providers.clone(),
                // 按这个组的配置判断：开着会话粘滞的轮询组不伤缓存
                hurts_cache: g.hurts_cache(),
            })
            .collect(),
        // 和 `GET /keys` 同一份视图：概览里少一个字段的话，两处会各自
        // 按不同的事实画同一张表。**只是密钥的值脱敏** —— 概览到处都在读，
        // 用不着它
        clients: keys::views(&s, keys::Reveal::Masked).await,
        security: tw_api::SecurityView {
            redact: cfg.security.redact.mode.into(),
            inspect_tools: cfg.security.inspect_tools.mode.into(),
            hidden_text: cfg.security.hidden_text.mode.into(),
            content: cfg.security.content.mode.into(),
            output_limit: cfg.security.output_limit.mode.into(),
        },
        default_route: engine.default_route().to_string(),
        client_probes: cfg
            .client_probes
            .all()
            .into_iter()
            .map(|(id, mode)| tw_api::ProbeView {
                id,
                mode: mode.into(),
            })
            .collect(),
        price_sheets: pricing::sheet_views(cfg),
        retention: tw_api::RetentionView {
            body_days: cfg.retention.body_days,
            row_days: cfg.retention.row_days,
            body_max_bytes: cfg.retention.body_max_bytes,
            // **现状和配置一起给。**「上限 2 GB」这个数字，用户没法
            // 判断松还是紧，除非同时看得见现在占了多少
            body_bytes_now,
        },
        listen: tw_api::ListenView {
            // **`Display` 不是 `Debug`。**`{:?}` 对 `Loopback` / `All`
            // 碰巧给出正确的小写词，对 `Addr(192.168.1.5)` 给的是
            // `addr(192.168.1.5)` —— 界面拿它去比对档位，永远不相等。
            bind: cfg.listen.gateway.bind.to_string(),
            port: cfg.listen.gateway.port,
            allow_from: cfg.listen.gateway.allow_from.clone(),
            default_allow_from: tw_config::default_allow_from(),
            exposed: cfg.listen.gateway.bind.is_exposed(),
        },
    })
}

/// 一行请求头给界面看的样子。
///
/// **不知道是不是密钥的，一律按密钥打码。**请求头名说明不了什么 ——
/// `X-Relay-Token` 和 `X-Tenant` 看不出哪个是秘密。原样给的只有两类：已知
/// 公开的头（`anthropic-version` 之类），和只由环境变量、占位符加上一个短前缀
/// 组成的值（`Bearer ${RELAY_TOKEN}`）—— 那里面没有秘密可泄。
fn header_view(h: &tw_config::Header) -> tw_api::HeaderView {
    let raw = h.value.raw();
    let masked = !(tw_secret::is_public_header(&h.name) || tw_secret::is_reference_only(raw));
    tw_api::HeaderView {
        name: h.name.clone(),
        value: if masked {
            tw_secret::mask_secret(raw)
        } else {
            raw.to_string()
        },
        masked,
    }
}

/// 一个上游给界面看的样子。**任何一个字段都不带密钥原文。**
fn provider_view(
    s: &ControlState,
    cfg: &tw_config::Config,
    p: &tw_config::Provider,
) -> tw_api::ProviderView {
    let base_url = tw_secret::redact_url(&p.base_url);
    let listing = s.gateway.models.listing(p);
    tw_api::ProviderView {
        name: p.name.clone(),
        base_url_masked: base_url != p.base_url,
        base_url,
        key: p.key.as_ref().map(|k| tw_api::SecretView {
            display: k.shown(),
            env: k.env_var().map(str::to_string),
        }),
        auth_header: p.auth_header().0.to_string(),
        headers: p.headers.iter().map(header_view).collect(),
        oauth: p.oauth.as_ref().map(|o| {
            let failure = s.gateway.oauth.failure(&p.name, o);
            tw_api::OAuthView {
                endpoint: o.endpoint.clone(),
                client_id: o.client_id.clone(),
                expires_at: o.expires_at.clone(),
                needs_login: failure.as_ref().is_some_and(|(_, relogin)| *relogin),
                failure: failure.map(|(why, _)| why),
            }
        }),
        protocol: p.effective_protocol().map(Into::into),
        protocol_explicit: p.protocol.is_some(),
        proxy: p.proxy.clone(),
        on_proxy_fail: p.on_proxy_fail.into(),
        models: p.models.clone(),
        models_only: p.models_only.clone(),
        model_source: listing.source.into(),
        model_status: listing.status.into(),
        model_fetching: listing.fetching,
        model_checked_at_ms: listing.checked_at_ms,
        model_error: listing.error,
        model_count: s.gateway.catalog.load().count_for(&p.name),
        disabled: p.disabled,
        health: match s.health().state(&p.name) {
            // 冷却到点的半开照常放行，界面上和闭合一样是「正常」
            tw_gateway::health::State::Closed | tw_gateway::health::State::HalfOpen => {
                tw_api::Health::Ok
            }
            tw_gateway::health::State::Open => tw_api::Health::Open,
        },
        auth_rejected: s.gateway.auth_rejected(&p.name),
        writeback_failed: s.gateway.writeback_failed(&p.name),
        billing: p.billing.into(),
        references: tw_config::refs::provider_refs(cfg, &p.name)
            .iter()
            .map(resources::reference_view)
            .collect(),
        pricing: p.pricing.clone(),
    }
}

/// `when` 里写了的条件，按固定顺序。
///
/// **规则列表上必须能直接读懂条件** —— 让用户去对着 YAML 猜「这条为什么
/// 没命中」，正是那类「我明明配了」问题的来源。这里给键和值，每个条件
/// 怎么称呼由界面决定。
pub(crate) fn describe_when(w: &tw_engine::rule::When) -> Vec<tw_api::ConditionView> {
    use tw_api::ConditionField;
    let mut out = Vec::new();
    let mut push = |field: ConditionField, values: Vec<String>| {
        out.push(tw_api::ConditionView { field, values })
    };
    for (field, v) in [
        (ConditionField::Model, &w.model),
        (ConditionField::Client, &w.client),
        (ConditionField::Dialect, &w.dialect),
        (ConditionField::InputTokens, &w.input_tokens),
        (ConditionField::MaxTokens, &w.max_tokens),
        (ConditionField::ToolCount, &w.tool_count),
    ] {
        if let Some(x) = v {
            push(field, vec![x.clone()]);
        }
    }
    if let Some(i) = &w.intent {
        push(ConditionField::Intent, one_or_many(i));
    }
    // **不写出来的话，一条只有 provider_would_be 的规则在界面上会显示成
    // 「兜底」**（条件为空就是兜底的标记）—— 那是个会让人查半天的假象。
    if let Some(p) = &w.provider_would_be {
        push(ConditionField::ProviderWouldBe, one_or_many(p));
    }
    for (field, v) in [
        (ConditionField::Cache, w.cache),
        (ConditionField::Tools, w.tools),
        (ConditionField::Image, w.image),
        (ConditionField::Thinking, w.thinking),
        (ConditionField::Stream, w.stream),
    ] {
        if let Some(b) = v {
            push(field, vec![b.to_string()]);
        }
    }
    out
}

fn one_or_many(v: &tw_engine::rule::OneOrMany) -> Vec<String> {
    match v {
        tw_engine::rule::OneOrMany::One(s) => vec![s.clone()],
        tw_engine::rule::OneOrMany::Many(xs) => xs.clone(),
    }
}

/// 模型清单的两份副本之间转换（core 内部一份、控制面契约一份）。
///
/// 手工转换是为了让 tw-api 不依赖 tw-gateway —— UI 和 CLI 只该依赖契约，
/// 不该被拖上整个数据面。
pub(crate) fn model_list(m: tw_gateway::ModelList) -> tw_api::ModelList {
    match m {
        tw_gateway::ModelList::Listed { models } => tw_api::ModelList::Listed { models },
        tw_gateway::ModelList::NotImplemented { status } => {
            tw_api::ModelList::NotImplemented { status }
        }
        tw_gateway::ModelList::Unrecognized { sample } => {
            tw_api::ModelList::Unrecognized { sample }
        }
        tw_gateway::ModelList::Empty => tw_api::ModelList::Empty,
    }
}

/// L1 测速。**零成本**，不发任何业务请求，用户可以随便点。
///
/// 一次可能测好几家，所以逐个测而不是并发：**并发会让每一段的耗时互相
/// 干扰**，六条线一起抢带宽测出来的 TLS 时间不是任何一条线的真实值，
/// 而这一层存在的全部意义就是那几个数字准不准。
async fn l1(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::L1Request>,
) -> Result<Json<Vec<tw_api::L1Result>>, Fail> {
    let cfg = s.config();
    let mut out = Vec::new();

    let targets: Vec<&tw_config::Provider> = match &req.provider {
        Some(n) => vec![
            cfg.providers
                .iter()
                .find(|p| p.name == *n)
                .ok_or_else(|| no_such_upstream(n))?,
        ],
        None => cfg.providers.iter().collect(),
    };
    for p in targets {
        let hop = match tw_gateway::l1::hop_for(&cfg, p) {
            Ok(h) => h,
            Err(e) => {
                out.push(tw_api::L1Result {
                    target: p.name.clone(),
                    via: Some(p.proxy.clone()),
                    ok: false,
                    segments: Vec::new(),
                    total_ms: 0,
                    skipped: Vec::new(),
                    failed: Some(l1_stage(tw_gateway::Stage {
                        step: tw_gateway::Step::Config,
                        peer: tw_gateway::Peer::Proxy,
                    })),
                    error: Some(e),
                });
                continue;
            }
        };
        let via = hop.as_ref().map(|_| p.proxy.clone());
        let r = tw_gateway::l1(&p.base_url, hop.as_ref()).await;
        out.push(l1_view(p.name.clone(), via, r));
    }
    Ok(Json(out))
}

pub(crate) fn l1_view(
    target: String,
    via: Option<String>,
    r: tw_gateway::L1Result,
) -> tw_api::L1Result {
    tw_api::L1Result {
        target,
        via,
        ok: r.ok,
        segments: r
            .segments
            .into_iter()
            .map(|x| tw_api::L1Segment {
                stage: l1_stage(x.stage),
                ms: x.ms,
            })
            .collect(),
        total_ms: r.total_ms,
        skipped: r
            .skipped
            .into_iter()
            .map(|x| tw_api::L1Skip {
                stage: l1_stage(x.stage),
                reason: x.reason,
            })
            .collect(),
        failed: r.failed.map(l1_stage),
        error: r.error,
    }
}

fn l1_stage(s: tw_gateway::Stage) -> tw_api::L1Stage {
    tw_api::L1Stage {
        step: s.step,
        peer: s.peer,
    }
}

/// 按时间分桶的花费与请求数（概览的趋势图）。
///
/// 桶宽由调用方给：同一段数据，「今天每小时」和「最近 30 天每天」要的
/// 是两种桶，而在服务端写死一种，另一种就得再加一个端点。
///
/// **空桶不补。**SQL 的 GROUP BY 只产出有数据的桶，而要画多少格只有
/// 界面知道（它知道图有多宽）。在这里补的话，一个跨度很大、桶很窄的
/// 请求会让我们凭空造出几万个零。
async fn cost_buckets(
    State(s): State<ControlState>,
    axum::extract::Query(q): axum::extract::Query<tw_api::BucketQuery>,
) -> Result<Json<Vec<tw_api::CostBucket>>, Fail> {
    let (from, to) = range(q.from_ms, q.to_ms);
    // 桶宽有下限，否则一个 `bucket_ms=1` 能让这条查询扫出几百万个分组。
    let bucket = q.bucket_ms.unwrap_or(3_600_000).max(1_000);
    let store = need_store(&s)?;
    let g = store.lock().await;
    let x = g.db().cost_buckets(from, to, bucket).map_err(records)?;
    Ok(Json(x))
}

/// 按模型或上游分组的花费（钱花在哪儿）。
async fn cost_buckets_by(
    State(s): State<ControlState>,
    axum::extract::Query(q): axum::extract::Query<tw_api::BucketGroupQuery>,
) -> Result<Json<Vec<tw_api::CostBucketGroup>>, Fail> {
    let (from, to) = range(q.from_ms, q.to_ms);
    // 桶宽有下限，和 `/summary/buckets` 同一条理由：`bucket_ms=1` 能让
    // 这条查询扫出几百万个分组，而这一条还要再乘上模型个数。
    let bucket = q.bucket_ms.unwrap_or(3_600_000).max(1_000);
    let store = need_store(&s)?;
    let g = store.lock().await;
    let x = g
        .db()
        .cost_buckets_by(q.dim, from, to, bucket)
        .map_err(records)?;
    Ok(Json(x))
}

async fn cost_by(
    State(s): State<ControlState>,
    axum::extract::Query(q): axum::extract::Query<tw_api::GroupQuery>,
) -> Result<Json<Vec<tw_api::CostGroup>>, Fail> {
    let (from, to) = range(q.from_ms, q.to_ms);
    let store = need_store(&s)?;
    let g = store.lock().await;
    let x = g.db().cost_by(q.dim, from, to).map_err(records)?;
    Ok(Json(x))
}

/// 一段时间的汇总。不给参数就是「今天」。
async fn summary(
    State(s): State<ControlState>,
    axum::extract::Query(q): axum::extract::Query<tw_api::Window>,
) -> Result<Json<tw_api::Summary>, Fail> {
    let (from, to) = range(q.from_ms, q.to_ms);
    let store = need_store(&s)?;
    let g = store.lock().await;
    let x = g.db().summary(from, to).map_err(records)?;
    Ok(Json(tw_api::Summary {
        requests: x.requests,
        failed: x.failed,
        locally_answered: x.locally_answered,
        input_tokens: x.input_tokens,
        output_tokens: x.output_tokens,
        cache_read_tokens: x.cache_read_tokens,
        cache_write_tokens: x.cache_write_tokens,
        cost_micros_exact: x.cost_micros_exact,
        cost_micros_estimated: x.cost_micros_estimated,
        unpriced_requests: x.unpriced_requests,
        no_usage_requests: x.no_usage_requests,
        cache_saved_micros: x.cache_saved_micros,
        // **和安全日志数的是同一批**：概览上点开这个数，落到的日志就是这么多条
        security: g.db().security_counts(from, to).map_err(records)?,
        pricing_date: s.gateway.pricing.load().table().date.clone(),
    }))
}

/// 最近的请求。**实时列表走内存 ring buffer，这个是给「翻历史」的**。
async fn history(
    State(s): State<ControlState>,
    axum::extract::Query(q): axum::extract::Query<tw_api::ListQuery>,
) -> Result<Json<Vec<tw_api::HistoryRow>>, Fail> {
    let store = need_store(&s)?;
    let g = store.lock().await;
    let rows = g.db().recent(within(&q), list_limit(&q)).map_err(records)?;
    // 这一段里的安全记录一次取完，按请求号挂上去。**流量页的徽标靠它**：
    // 以前徽标只来自实时事件，关窗再开就没了
    let mut security = match (
        rows.iter().map(|r| r.id).min(),
        rows.iter().map(|r| r.id).max(),
    ) {
        (Some(from), Some(to)) => g.db().security_of_requests(from, to).map_err(records)?,
        _ => Default::default(),
    };
    Ok(Json(
        rows.into_iter()
            .map(|r| {
                let sec = security.remove(&r.id).unwrap_or_default();
                history_row(r, sec)
            })
            .collect(),
    ))
}

async fn latency(
    State(s): State<ControlState>,
    axum::extract::Query(q): axum::extract::Query<tw_api::Window>,
) -> Result<Json<Vec<tw_api::LatencyView>>, Fail> {
    let (from, to) = range(q.from_ms, q.to_ms);
    let store = need_store(&s)?;
    let g = store.lock().await;
    let xs = g.db().latency_by_model(from, to).map_err(records)?;
    Ok(Json(
        xs.into_iter()
            .map(|l| tw_api::LatencyView {
                model: l.model,
                p50: l.p50,
                p95: l.p95,
                samples: l.samples,
            })
            .collect(),
    ))
}

/// L3 测速的报价。**必须先问这个，再问 run。**
///
/// 这两个端点分开不是为了好看：合成一个的话，「显示预估」和「真的花钱」
/// 之间就没有一个用户点头的位置了。
async fn speed_quote(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::SpeedRunRequest>,
) -> Result<Json<tw_api::SpeedQuote>, Fail> {
    let cfg = s.config();
    let book = s.gateway.pricing.load();
    let catalog = s.gateway.catalog.load();
    let items: Vec<(tw_gateway::Estimate, Option<tw_gateway::models::Skip>)> =
        targets(&cfg, &req.providers)?
            .into_iter()
            .map(|p| {
                // 和记账同一个口径：按这家的计费方式和价目表。**协议也要给**：
                // 输出上限报多少由它决定（见 `tw_gateway::l3::max_output_tokens`）
                let e = tw_gateway::l3::estimate(
                    &book,
                    &p.name,
                    &req.model,
                    p.billing,
                    p.effective_protocol(),
                );
                (e, tw_gateway::models::fit(&catalog, p, &req.model))
            })
            .collect();
    Ok(Json(tw_api::SpeedQuote {
        // 服务不了这个模型的那几家不会被测，也就不进合计
        total_micros: tw_gateway::quote::total(
            items
                .iter()
                .filter(|(_, skip)| skip.is_none())
                .map(|(e, _)| &e.quote),
        ),
        items: items
            .into_iter()
            .map(|(e, skip)| quote_item(e, skip))
            .collect(),
        pricing_date: book.table().date.clone(),
    }))
}

/// 真的跑。**这一步花钱。**
async fn speed_run(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::SpeedRunRequest>,
) -> Result<Json<Vec<tw_api::SpeedResult>>, Fail> {
    let cfg = s.config();
    let catalog = s.gateway.catalog.load();
    let mut out = Vec::new();
    // **逐个跑，不并发。**几家一起打，测出来的 TTFT 互相干扰，而这一层
    // 存在的全部意义就是那几个数字准不准（和 L1 同一个理由）。
    for p in targets(&cfg, &req.providers)? {
        // 服务不了这个模型的不发：报价里已经说了它不会被测，发出去只会得到
        // 一个 4xx，还可能被计费
        if tw_gateway::models::fit(&catalog, p, &req.model).is_some() {
            continue;
        }
        // OAuth 那类要联网换 token，所以走网关那条 async 的路。
        // **用这一家自己的 client** —— 换 token 要走它的代理。
        let pk_http = s.gateway.client_for(&p.name);
        let headers = match s.gateway.headers_for(p, &pk_http).await {
            Ok(h) => h,
            Err(e) => {
                out.push(tw_api::SpeedResult {
                    provider: p.name.clone(),
                    model: req.model.clone(),
                    ok: false,
                    connect_ms: 0,
                    ttft_ms: None,
                    total_ms: 0,
                    input_tokens: None,
                    output_tokens: None,
                    error: Some(tw_gateway::credential_failed(e, &p.name)),
                });
                continue;
            }
        };
        // **发请求也用这一家的 client。**以前只有换 token 走它、真正的测速
        // 请求走默认 client —— 要走代理的上游在这里连不上，而转发时它是通的
        //
        // 输出上限和报价里那个是同一个数：摆给用户看的上限和真正发出去的不一样，
        // 那份报价就不是这次消耗的报价
        let r = tw_gateway::l3::run(
            &pk_http,
            p,
            &headers,
            &req.model,
            tw_gateway::l3::max_output_tokens(
                &s.gateway.pricing.load(),
                &req.model,
                p.effective_protocol(),
            ),
        )
        .await;
        out.push(tw_api::SpeedResult {
            provider: r.provider,
            model: r.model,
            ok: r.ok,
            connect_ms: r.connect_ms,
            ttft_ms: r.ttft_ms,
            total_ms: r.total_ms,
            input_tokens: r.input_tokens,
            output_tokens: r.output_tokens,
            error: r.error,
        });
    }
    Ok(Json(out))
}

fn quote_item(
    e: tw_gateway::Estimate,
    skip: Option<tw_gateway::models::Skip>,
) -> tw_api::SpeedEstimate {
    tw_api::SpeedEstimate {
        provider: e.provider,
        model: e.model,
        input_tokens: e.input_tokens,
        max_output_tokens: e.max_output_tokens,
        cost_micros: e.quote.cost_micros,
        billing: e.quote.billing.into(),
        skipped: skip.map(Into::into),
    }
}

/// 要测的那几家。**空 = 全部**；点名了一家不存在的就是 404，不悄悄跳过。
fn targets<'a>(
    cfg: &'a tw_config::Config,
    names: &[String],
) -> Result<Vec<&'a tw_config::Provider>, Fail> {
    if names.is_empty() {
        return Ok(cfg.providers.iter().collect());
    }
    names
        .iter()
        .map(|n| {
            cfg.providers
                .iter()
                .find(|p| &p.name == n)
                .ok_or_else(|| no_such_upstream(n))
        })
        .collect()
}

/// 一条请求的全部细节，含 body。
///
/// **body 是从磁盘现读的，不进内存缓存。**详情抽屉一次只看一条，而把
/// 所有 body 缓存起来等着「万一有人点」，代价是几百 MB。
async fn request_detail(
    State(s): State<ControlState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<tw_api::RequestDetail>, Fail> {
    let store = need_store(&s)?;
    let g = store.lock().await;
    // **还在跑的也答。**记录要等结局才落库，而用户点开的往往正是那个跑了很久的
    // 请求：给到目前为止知道的，标着还在跑
    let (row, in_flight) = match g.db().get(id).map_err(records)? {
        Some(row) => (row, false),
        None => {
            let row = u64::try_from(id)
                .ok()
                .and_then(|id| g.in_flight_row(id))
                .ok_or_else(|| {
                    fail(
                        StatusCode::NOT_FOUND,
                        msg!("control.request_not_found", id = id => "There is no request {id}."),
                    )
                })?;
            (row, true)
        }
    };
    let at = row.at_ms;
    let body = |which| {
        let raw = g.blobs().get(at, id, which)?;
        let stored = raw.len();
        // **一律脱敏。**请求体里有 system prompt、工具定义、有时还有
        // 用户粘进去的密钥，而这段文字会被复制到 issue 里。
        let text = tw_secret::mask_body(&String::from_utf8_lossy(&raw));
        let original_len = g.blobs().original_len(at, id, which).unwrap_or(stored);
        Some(tw_api::BodyView {
            text,
            original_len,
            truncated: original_len > stored,
        })
    };
    let security = g
        .db()
        .security_of_requests(id, id)
        .map_err(records)?
        .remove(&id)
        .unwrap_or_default();
    let detail = tw_api::RequestDetail {
        request_body: body(tw_store::Which::Request),
        response_body: body(tw_store::Which::Response),
        row: history_row(row, security),
        in_flight,
    };
    Ok(Json(detail))
}

/// 订阅额度。**每个上游最近一次报的**。
///
/// 按量付费的账号没有这些头，那时这个列表是空的 —— 界面据此决定显示
/// 金额还是百分比，两种人格共用同一块地方。
async fn quota(State(s): State<ControlState>) -> Json<Vec<tw_api::ProviderQuota>> {
    let mut out: Vec<tw_api::ProviderQuota> = s
        .gateway
        .quotas()
        .into_iter()
        .map(|(provider, q)| tw_api::ProviderQuota {
            provider,
            windows: q
                .windows
                .into_iter()
                .map(|w| tw_api::QuotaWindow {
                    window: w.window,
                    used_percent: w.used_percent,
                    resets_at_ms: w.resets_at_ms,
                    status: w.status,
                })
                .collect(),
        })
        .collect();
    out.sort_by(|a, b| a.provider.cmp(&b.provider));
    Json(out)
}

/// 按上游分的延迟。**「哪家 TTFT 最差」问的是这个。**
///
/// 和按模型分是两个问题：前者的下一步是换上游，后者是换模型。
async fn latency_by_provider(
    State(s): State<ControlState>,
    axum::extract::Query(q): axum::extract::Query<tw_api::Window>,
) -> Result<Json<Vec<tw_api::LatencyView>>, Fail> {
    let (from, to) = range(q.from_ms, q.to_ms);
    let store = need_store(&s)?;
    let g = store.lock().await;
    let xs = g.db().latency_by_provider(from, to).map_err(records)?;
    Ok(Json(
        xs.into_iter()
            .map(|l| tw_api::LatencyView {
                model: l.model,
                p50: l.p50,
                p95: l.p95,
                samples: l.samples,
            })
            .collect(),
    ))
}

async fn storage(State(s): State<ControlState>) -> Json<tw_api::StorageStatus> {
    let Some(store) = &s.store else {
        return Json(tw_api::StorageStatus {
            recording: false,
            rows: 0,
            blob_bytes: 0,
            forwarding_affected: false,
        });
    };
    let g = store.lock().await;
    Json(tw_api::StorageStatus {
        recording: true,
        rows: g.db().count().unwrap_or(0),
        blob_bytes: g.blobs().total_bytes(),
        // **永远是 false。**观测挂了，代理照跑。哪天有人想改成
        // true，先回去读那一节。
        forwarding_affected: false,
    })
}

fn need_store(s: &ControlState) -> Result<&Arc<tokio::sync::Mutex<tw_store::Recorder>>, Fail> {
    s.store.as_ref().ok_or_else(|| {
        fail(
            StatusCode::SERVICE_UNAVAILABLE,
            msg!(
                "control.store_unavailable" =>
                "Request recording is unavailable: the database could not be opened, or the disk \
                 is failing. Forwarding is unaffected."
            ),
        )
    })
}

fn history_row(
    r: tw_store::RequestRow,
    security: Vec<tw_api::SecurityEventView>,
) -> tw_api::HistoryRow {
    tw_api::HistoryRow {
        id: r.id,
        at_ms: r.at_ms,
        client: r.client,
        provider: r.provider,
        model: r.model,
        path: r.path,
        status: r.status,
        ttfb_ms: r.ttfb_ms,
        duration_ms: r.duration_ms,
        bytes: r.bytes,
        input_tokens: r.input_tokens,
        output_tokens: r.output_tokens,
        cache_read_tokens: r.cache_read_tokens,
        cache_write_tokens: r.cache_write_tokens,
        cost_micros: r.cost_micros,
        cost_estimated: r.cost_estimated,
        error: r.error,
        local: r.local,
        cancelled: r.cancelled,
        // 解不开就当没有。**一条坏掉的路由记录不该让整条请求记录读不出来**
        // —— 那是详情页上的一栏，不是这一行存在的理由。
        routing: r
            .routing
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok()),
        billing: r.billing,
        cache_saved_micros: r.cache_saved_micros,
        // 同上：解不开就当没有
        price_source: r
            .price_source
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok()),
        translated: r
            .translated
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok()),
        session: r.session,
        client_hint: r.client_hint,
        peer: r.peer,
        key_masked: r.key_masked,
        security,
    }
}

/// 时间窗的两端。**缺省是「今天」**，理由见 [`tw_api::Window`]。
fn range(from_ms: Option<i64>, to_ms: Option<i64>) -> (i64, i64) {
    let now = now_ms();
    // 本地时区的零点。UTC 零点对一个桌面工具没有意义 —— 用户在
    // 东八区，UTC 零点是他的早上八点。
    let midnight = local_midnight_ms(now);
    (from_ms.unwrap_or(midnight), to_ms.unwrap_or(now))
}

fn local_midnight_ms(now_ms: i64) -> i64 {
    let offset = chrono::Local::now().offset().local_minus_utc() as i64 * 1000;
    let local = now_ms + offset;
    local - local.rem_euclid(86_400_000) - offset
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 列表的时间窗：**缺省是不限**，不是今天（见 [`tw_api::ListQuery`]）。
fn within(q: &tw_api::ListQuery) -> Option<(i64, i64)> {
    match (q.from_ms, q.to_ms) {
        (None, None) => None,
        (f, t) => Some((f.unwrap_or(i64::MIN), t.unwrap_or_else(now_ms))),
    }
}

fn list_limit(q: &tw_api::ListQuery) -> usize {
    q.limit.unwrap_or(200).min(2000)
}

/// 当前配置的原文。**文本模式直接显示它。**
///
/// 控制面的钥匙是打码的（`tw_api::control::KEY_MASK`）：界面用不着它，而
/// 编辑器里的原文会被复制、截图。整份写回来时带着打码，就是钥匙不动。
async fn get_config(State(s): State<ControlState>) -> Result<Json<tw_api::ConfigText>, Fail> {
    let c = s.cfg.current().map_err(unreadable_config)?;
    Ok(Json(tw_api::ConfigText {
        path: c.path.display().to_string(),
        version: c.version(),
        text: tw_config::control_key::mask(&c.text),
    }))
}

/// 按字段改配置。**409 表示「你手里那份过期了」，不是失败。**
async fn patch_config(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::ConfigPatch>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .patch(
            &req.ops,
            req.base_version.as_deref(),
            tw_config::history::Origin::Ui,
        )
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

/// 整份写回去。**文本模式走这条。**
async fn put_config(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::ConfigWrite>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .write(
            &req.text,
            Some(&req.base_version),
            tw_config::history::Origin::Ui,
        )
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn config_history(
    State(s): State<ControlState>,
) -> Result<Json<Vec<tw_api::ConfigVersion>>, Fail> {
    let all = tw_config::history::list(s.config_path()).map_err(unreadable_config)?;
    let now = s.cfg.current().map(|c| c.version()).unwrap_or_default();
    // **新的在前。**用户找的几乎总是最近那几版。
    Ok(Json(
        all.into_iter()
            .rev()
            .map(|v| tw_api::ConfigVersion {
                current: v.version == now,
                version: v.version,
                at_ms: v.at_ms,
                origin: v.origin.into(),
                bytes: v.bytes,
            })
            .collect(),
    ))
}

async fn config_rollback(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::RollbackRequest>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s.cfg.rollback(&req.version).await.map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

/// 控制面的错误响应。
///
/// **响应体是一个 JSON 的 [`Msg`]，不是一句纯文本。**上一版发的是文本，
/// 于是界面只能把它原样贴出来 —— 一个中英双语的界面里，那句话必然有
/// 一半人读不懂。现在带着码发出去，界面照码说它自己那句话。
pub(crate) type Fail = (StatusCode, Json<Msg>);

pub(crate) fn fail(code: StatusCode, detail: Msg) -> Fail {
    (code, Json(detail))
}

/// 内部错误：锁坏了、序列化不了之类 —— **正常使用碰不到的那一类**。
///
/// 界面对它们能说的只有同一句话（「网关内部出了问题」），真正有用的是
/// `detail` 里那句原话，而那句话是给看日志的人读的。
pub(crate) fn internal(e: impl std::fmt::Display) -> Fail {
    fail(
        StatusCode::INTERNAL_SERVER_ERROR,
        msg!(
            "control.internal_error", detail = e =>
            "Something went wrong inside the gateway: {detail}"
        ),
    )
}

/// 请求记录的库读不了。**和 [`internal`] 分开**：这一类用户看得见后果
/// （流量页空着），说清是记录读不出来，比一句「内部错误」有用。`detail`
/// 是 SQLite 的原话。
pub(crate) fn records(e: tw_store::db::DbError) -> Fail {
    fail(
        StatusCode::INTERNAL_SERVER_ERROR,
        msg!(
            "control.records_unreadable", detail = e =>
            "The request records could not be read: {detail}"
        ),
    )
}

/// 配置文件读不了（不存在、没权限）。那句话自己带码。
pub(crate) fn unreadable_config(e: tw_config::StoreError) -> Fail {
    fail(StatusCode::INTERNAL_SERVER_ERROR, e.msg())
}

/// 配置里没有这个名字的上游。
pub(crate) fn no_such_upstream(name: &str) -> Fail {
    fail(
        StatusCode::NOT_FOUND,
        msg!(
            "control.upstream_not_found", upstream = name =>
            "There is no upstream named `{upstream}`."
        ),
    )
}

/// 把配置改动的失败翻成 HTTP。
///
/// **`Stale` 必须是 409 而不是 400。**界面要能区分「我写错了」和「有人
/// 抢先改了」—— 后者的正确反应是刷新再合并，而不是给用户看一条错误。
pub(crate) fn apply_fail(e: ApplyError) -> Fail {
    use tw_config::edit::EditError;
    let code = match &e {
        ApplyError::Stale { .. } => StatusCode::CONFLICT,
        ApplyError::Store(tw_config::StoreError::Conflict { .. }) => StatusCode::CONFLICT,
        // 名字撞了、还有人在引用 —— 都是「现在的配置不允许」，不是请求写错了
        ApplyError::Edit(EditError::NameTaken { .. }) | ApplyError::InUse(_) => {
            StatusCode::CONFLICT
        }
        ApplyError::Edit(EditError::NotFound { .. }) => StatusCode::NOT_FOUND,
        // 不是请求写错了，是这条路上不许改
        ApplyError::ControlKeyLocked | ApplyError::RemoteControlLocked => StatusCode::FORBIDDEN,
        ApplyError::Rejected(_)
        | ApplyError::Build(_)
        | ApplyError::BadPath(_)
        | ApplyError::Invalid(_)
        | ApplyError::Edit(
            EditError::Unwritable(_) | EditError::Multiline | EditError::Nameless { .. },
        )
        | ApplyError::Edit(EditError::Yaml(_)) => StatusCode::BAD_REQUEST,
        ApplyError::Edit(EditError::Parse(_) | EditError::SelfCheck(_)) | ApplyError::Store(_) => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    fail(code, e.msg())
}

fn session_view(s: &tw_store::db::SessionRow) -> tw_api::SessionView {
    tw_api::SessionView {
        id: s.id.clone(),
        client: s.client.clone(),
        started_ms: s.started_ms as u64,
        ended_ms: s.ended_ms as u64,
        turns: s.turns as u64,
        cost_micros: s.cost_micros,
        cost_micros_estimated: s.cost_micros_estimated,
        priced_turns: s.priced_turns as u64,
        unpriced_turns: s.unpriced_turns as u64,
        no_usage_turns: s.no_usage_turns as u64,
        input_tokens: s.input_tokens,
        output_tokens: s.output_tokens,
        cache_read_tokens: s.cache_read_tokens,
        cache_write_tokens: s.cache_write_tokens,
        cache_saved_micros: s.cache_saved_micros,
        peak_input_tokens: s.peak_input_tokens,
        models: s
            .models
            .split(',')
            .filter(|x| !x.is_empty())
            .map(|x| x.to_string())
            .collect(),
        errors: s.errors as u64,
    }
}

fn turn_view(t: &tw_store::db::TurnRow) -> tw_api::TurnView {
    tw_api::TurnView {
        id: t.id,
        at_ms: t.at_ms as u64,
        model: t.model.clone(),
        provider: t.provider.clone(),
        input_tokens: t.input_tokens,
        output_tokens: t.output_tokens,
        cache_read_tokens: t.cache_read_tokens,
        cost_micros: t.cost_micros,
        duration_ms: t.duration_ms,
        error: t.error.clone(),
        cancelled: t.cancelled,
        cost_estimated: t.cost_estimated,
        billing: t.billing,
    }
}

/// 会话列表。
///
/// **观测层没起来时返回空列表，不是错误**：那时网关照常转发，
/// 界面上少一块统计，而不是弹一个错。
async fn sessions(
    State(s): State<ControlState>,
    axum::extract::Query(q): axum::extract::Query<tw_api::ListQuery>,
) -> Json<Vec<tw_api::SessionView>> {
    let Some(store) = &s.store else {
        return Json(Vec::new());
    };
    let g = store.lock().await;
    Json(
        g.db()
            .sessions(within(&q), list_limit(&q))
            .unwrap_or_default()
            .iter()
            .map(session_view)
            .collect(),
    )
}

async fn session_detail(
    State(s): State<ControlState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<tw_api::SessionDetail>, Fail> {
    let Some(store) = &s.store else {
        return Err(fail(
            StatusCode::SERVICE_UNAVAILABLE,
            msg!("control.store_off" => "Request recording is not running."),
        ));
    };
    let g = store.lock().await;
    let session = g
        .db()
        .sessions(None, 500)
        .unwrap_or_default()
        .iter()
        .find(|x| x.id == id)
        .map(session_view)
        .ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                msg!("control.session_not_found", id = id.clone() => "There is no session {id}."),
            )
        })?;
    let turns = g
        .db()
        .turns(&id)
        .unwrap_or_default()
        .iter()
        .map(turn_view)
        .collect();
    Ok(Json(tw_api::SessionDetail { session, turns }))
}

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("the control-plane socket {path} could not be started: {source}")]
    Bind {
        path: PathBuf,
        source: std::io::Error,
    },
    /// **系统级的硬上限，不是我们的规矩。**`sockaddr_un.sun_path` 在
    /// macOS 上是 104 字节、Linux 上 108 —— 超了 `bind` 会失败，而 libc
    /// 给的原话是「path must be shorter than SUN_LEN」，看不出上限是多少、
    /// 也看不出自己超了多少。
    #[error(
        "the control-plane socket path is {len} bytes, over the system limit of {max}.\n{path}\nPut the configuration in a directory with a shorter path (the default ~/.thinkwatch is well within the limit)."
    )]
    PathTooLong {
        path: PathBuf,
        len: usize,
        max: usize,
    },
    /// 端口号写不进去。**这不是小事**：写不进去，客户端就找不到控制面，
    /// 而网关本身照常在转发 —— 一个「跑着但够不着」的 core。
    #[error("the control-plane port could not be written to {path}: {source}")]
    PortFile {
        path: PathBuf,
        source: std::io::Error,
    },
    /// 这个平台上没有 unix socket。
    ///
    /// `Address::in_dir` 不会给出这一档，所以走到这里说明有人显式指定了它
    /// —— 与其静默换一种传输，不如说清楚。
    #[cfg(not(unix))]
    #[error("this platform has no unix sockets, so the control plane cannot listen on {path}")]
    NoUnixSockets { path: PathBuf },
}

/// `sockaddr_un.sun_path` 的容量。macOS 104、Linux 108，取小的那个 ——
/// 差的那 4 个字节不值得为它分平台。
const SUN_PATH_MAX: usize = 104;

/// 路径放得下吗。**在 bind 之前问**，这样错误信息能说清上限和超出量。
pub fn socket_path_fits(path: &Path) -> Result<(), ControlError> {
    // 末尾的 NUL 也占一个字节
    let len = path.as_os_str().as_encoded_bytes().len() + 1;
    if len > SUN_PATH_MAX {
        return Err(ControlError::PathTooLong {
            path: path.to_path_buf(),
            len,
            max: SUN_PATH_MAX,
        });
    }
    Ok(())
}

/// 起控制面。
///
/// 两种传输，**挑哪一种不是调用方的事**：`Address::in_dir` 按平台给出这台
/// 机器上唯一可用的那一种（见 [`tw_api::control::Address`]）。这里只负责把
/// 它听起来。
///
/// **每条连接先握手**（见 `gate`），握上了才交给 HTTP。门装在这一层，不装进
/// `router()`：`router()` 是路由表本身，十几个集成测试直接拿它跑处理函数，
/// 它们测的不是门。
///
/// 远程控制端口（`listen.control.remote`）在这里一并跟起来。**它是另开的**：
/// 绑不上、写错了都不影响本机的通道，原因记在 `Status.remote_control`。
pub async fn serve(state: ControlState, at: &tw_api::control::Address) -> Result<(), ControlError> {
    use tw_api::control::Address;
    let gate = gate::Gate::new(&state);
    let app = router(state.clone());
    tokio::spawn(remote::follow(state, app.clone(), gate.clone()));
    match at {
        #[cfg(unix)]
        Address::Socket(path) => serve_socket(app, gate, path).await,
        #[cfg(not(unix))]
        Address::Socket(path) => Err(ControlError::NoUnixSockets { path: path.clone() }),
        Address::Loopback { port_file } => serve_loopback(app, gate, port_file).await,
    }
}

/// unix socket 上的控制面。
///
/// 陈旧的 socket 文件直接删掉重建 —— 它和 lock 文件不一样，没有「另一个
/// 实例可能还在用」的歧义：单实例锁已经在上一步挡住了。
#[cfg(unix)]
async fn serve_socket(app: Router, gate: gate::Gate, path: &Path) -> Result<(), ControlError> {
    socket_path_fits(path)?;
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let listener = tokio::net::UnixListener::bind(path).map_err(|source| ControlError::Bind {
        path: path.to_path_buf(),
        source,
    })?;
    {
        use std::os::unix::fs::PermissionsExt;
        // 0700：只有当前用户能连。握手那道门在它之外，不是替代它 ——
        // 两道都在，而只有这一道是平台给的。
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    tracing::info!(path = %path.display(), "the control plane is listening");
    loop {
        match listener.accept().await {
            Ok((stream, _)) => hand_off(stream, app.clone(), gate.clone(), None),
            Err(e) => tracing::warn!("the control plane could not accept a connection: {e}"),
        }
    }
}

/// 回环 TCP 上的控制面。Windows 只有这一档。
///
/// **绑 0 号端口**，让系统挑一个空闲的，再把号码写进那个文件。固定端口会和
/// 别的软件撞，而撞上的表现是 core 起不来；它也省了想连进来的人一步。
///
/// 端口文件**先绑后写**：写完才说得出真实的号码，而反过来（先写一个想要的
/// 号再去绑）会在绑失败时留下一个指向别人的文件。
///
/// 这一档**挡不住同机的任何进程**，也问不出对端是谁 —— 门全在握手上，
/// 见 `gate`。
async fn serve_loopback(
    app: Router,
    gate: gate::Gate,
    port_file: &Path,
) -> Result<(), ControlError> {
    use std::net::Ipv4Addr;
    if let Some(dir) = port_file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|source| ControlError::Bind {
            path: port_file.to_path_buf(),
            source,
        })?;
    let port = listener
        .local_addr()
        .map_err(|source| ControlError::Bind {
            path: port_file.to_path_buf(),
            source,
        })?
        .port();
    std::fs::write(port_file, port.to_string()).map_err(|source| ControlError::PortFile {
        path: port_file.to_path_buf(),
        source,
    })?;
    tracing::info!(port, "the control plane is listening on loopback");
    loop {
        match listener.accept().await {
            Ok((stream, _)) => hand_off(stream, app.clone(), gate.clone(), None),
            Err(e) => tracing::warn!("the control plane could not accept a connection: {e}"),
        }
    }
}

/// 一条连接：先握手，握上了交给 hyper。
///
/// **每种传输共用**：它们的差别只在怎么拿到这个流，拿到之后的每一件事
/// （握手、协议协商、错误怎么记）都该一模一样 —— 写两遍就是两遍会漂。
/// 远程端口接进来的连接也走这里，多带一个 [`RemoteConn`]。
fn hand_off<S>(stream: S, app: Router, gate: gate::Gate, remote: Option<RemoteConn>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    tokio::spawn(async move {
        let link = match gate.admit(stream).await {
            Ok(l) => l,
            Err(e) => {
                // 远程来源的失败要记账（节流）。版本不一致不算：钥匙是对的
                if let Some(r) = remote
                    && !matches!(e, tw_link::LinkError::VersionMismatch { .. })
                {
                    (r.on_failure)();
                }
                return;
            }
        };
        let io = hyper_util::rt::TokioIo::new(link.stream);
        let svc = hyper::service::service_fn(move |req| {
            use tower::ServiceExt;
            app.clone().oneshot(req)
        });
        let Some(mut r) = remote else {
            let builder =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
            let conn = builder.serve_connection(io, svc);
            tokio::select! {
                r = conn => {
                    if let Err(e) = r {
                        tracing::debug!("a control-plane connection ended: {e}");
                    }
                }
                // 钥匙换了：用旧钥匙进来的这一条断开（理由见 `gate`）。连接直接丢掉，
                // 事件流也跟着断，对面重连时按新钥匙握手
                _ = gate.key_changed_from(&link.key) => {
                    tracing::info!("the control key changed; closing a connection made with the previous one");
                }
            }
            return;
        };
        // 远程连接**只说 HTTP/1.1**：请求在这条连接自己的任务里处理，「这是远程」
        // 的标记（`remote::is_remote`）一路都在。HTTP/2 会把每个请求派到别的任务上，
        // 标记就丢了 —— 丢了的样子是远程连接能关掉 core
        let conn = hyper::server::conn::http1::Builder::new().serve_connection(io, svc);
        remote::as_remote(async {
            tokio::select! {
                r = conn => {
                    if let Err(e) = r {
                        tracing::debug!("a remote control connection ended: {e}");
                    }
                }
                _ = gate.key_changed_from(&link.key) => {
                    tracing::info!("the control key changed; closing a remote connection made with the previous one");
                }
                // 远程端口关了或换了地址：从它进来的一起断开
                _ = r.closed.changed() => {
                    tracing::info!("the remote control port closed; closing a connection made through it");
                }
            }
        })
        .await;
    });
}

/// 从远程端口进来的一条连接多带的东西。
pub(crate) struct RemoteConn {
    /// 握手没成：记一笔（节流）
    pub(crate) on_failure: Box<dyn FnOnce() + Send>,
    /// 远程端口停了，这条也断
    pub(crate) closed: tokio::sync::watch::Receiver<()>,
    /// 占着一个并发名额，连接结束时还回去
    pub(crate) _slot: tokio::sync::OwnedSemaphorePermit,
}

/// 在起任何东西之前问一句：这个地址听得起来吗。
///
/// **不是等到 bind 的那一刻才发现。**那时网关已经在监听、客户端可能已经
/// 连上来了，而这条错误当时只会进日志。
pub fn endpoint_usable(at: &tw_api::control::Address) -> Result<(), ControlError> {
    use tw_api::control::Address;
    match at {
        Address::Socket(p) => socket_path_fits(p),
        // 回环那一档没有等价的前置条件：端口由系统挑，挑不出来时 bind 自己
        // 会说，而那句话已经够清楚了。
        Address::Loopback { .. } => Ok(()),
    }
}

#[cfg(test)]
mod socket_path_tests {
    use super::*;

    #[test]
    fn a_path_that_does_not_fit_says_the_limit_and_by_how_much() {
        // libc 的原话是「path must be shorter than SUN_LEN」—— 看不出
        // 上限是多少，也看不出自己超了多少。两个数字都得说。
        let long = PathBuf::from("/tmp")
            .join("x".repeat(200))
            .join("twcore.sock");
        let e = socket_path_fits(&long).unwrap_err();
        let m = e.to_string();
        assert!(m.contains("104"), "{m}");
        assert!(
            m.contains(&format!("{}", long.as_os_str().len() + 1)),
            "{m}"
        );
        // 还要说怎么办
        assert!(m.contains(".thinkwatch"), "{m}");
    }

    #[test]
    fn the_default_path_fits_with_room_to_spare() {
        // 这条不是形式主义：如果哪天默认目录变深了，它会立刻响。
        let p = tw_api::data::dir().join("twcore.sock");
        socket_path_fits(&p).unwrap();
    }

    #[test]
    fn the_boundary_is_the_nul_terminator_not_the_byte_count() {
        // 正好 104 字节的路径**放不下** —— 结尾的 NUL 也要占一个。
        // 差这一个字节的话，失败会推迟到 bind，而那时的报错完全不同。
        let base = "/tmp/";
        let exact = PathBuf::from(format!("{base}{}", "a".repeat(104 - base.len())));
        assert_eq!(exact.as_os_str().len(), 104);
        assert!(socket_path_fits(&exact).is_err());
        let one_less = PathBuf::from(format!("{base}{}", "a".repeat(103 - base.len())));
        assert!(socket_path_fits(&one_less).is_ok());
    }
}

#[cfg(test)]
mod describe_tests {
    use super::*;

    /// **每加一个 `when` 字段就必须在这里列出来。**
    ///
    /// 漏掉的后果不是「少显示一个条件」，而是条件列表变空 —— 而空列表
    /// 在界面上正是「兜底」的标记。于是一条精确规则会显示成兜底，用户
    /// 照着界面查半天。这条测试就是为了让漏掉这件事立刻响。
    #[test]
    fn every_when_field_gets_a_line() {
        let full: tw_engine::rule::When = serde_yaml_ng::from_str(
            "{ model: a*, client: c, dialect: anthropic, input_tokens: '>1k',
               max_tokens: '<4k', tool_count: '>2', cache: true, tools: false,
               image: true, thinking: false, stream: true,
               intent: titling, provider_would_be: [a, b] }",
        )
        .unwrap();
        let fields = match serde_json::to_value(&full).unwrap() {
            serde_json::Value::Object(m) => m,
            other => panic!("{other:?}"),
        };
        let lines = describe_when(&full);
        assert_eq!(lines.len(), fields.len(), "有 when 字段没列出来：{lines:?}");
        // 界面按 `field` 取名称，所以它必须就是配置里的那个键
        for l in &lines {
            assert!(fields.contains_key(l.field.slug()), "{l:?}");
        }
    }

    #[test]
    fn a_phase_two_only_rule_is_not_shown_as_a_catch_all() {
        let w: tw_engine::rule::When =
            serde_yaml_ng::from_str("{ provider_would_be: relay }").unwrap();
        let lines = describe_when(&w);
        assert!(!lines.is_empty(), "空的条件列表在界面上就是「兜底」");
        assert_eq!(
            lines[0].field,
            tw_api::ConditionField::ProviderWouldBe,
            "{lines:?}"
        );
        assert_eq!(lines[0].values, ["relay"], "{lines:?}");
    }
}

#[cfg(test)]
mod event_stream_tests {
    use super::*;
    use futures::StreamExt;

    /// 订阅者跟不上时，**丢了要说**。以前只记一行日志，而界面上「进行中」的
    /// 计数和每一行的状态都靠增量维护 —— 丢一个结局，那一行就永远在跑。
    #[tokio::test]
    async fn a_subscriber_that_falls_behind_is_told_how_many_events_it_lost() {
        let bus = EventBus::new();
        let stream = event_stream(bus.subscribe(), bus.clone());
        let mut stream = Box::pin(stream);
        // 比缓冲多塞 10 条，一条都还没读
        let n = 1024 + 10;
        for i in 0..n {
            bus.emit(tw_api::Event::HealthChanged {
                id: i,
                provider: "p".into(),
                state: tw_api::BreakerState::Open,
                at_ms: 0,
            });
        }
        match stream.next().await {
            Some(tw_api::Event::EventsDropped { count, .. }) => assert_eq!(count, 10),
            other => panic!("该先说丢了几条，实际 {other:?}"),
        }
        // 然后接着收留下来的：最老的丢掉了，从第 10 条开始
        match stream.next().await {
            Some(tw_api::Event::HealthChanged { id, .. }) => assert_eq!(id, 10),
            other => panic!("该接着收留下来的，实际 {other:?}"),
        }
    }
}
