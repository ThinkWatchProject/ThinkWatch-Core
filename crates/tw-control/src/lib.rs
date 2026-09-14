//! 控制面：unix socket 上的 HTTP。
//!
//! **不占 TCP 端口**。理由不是省端口，是权限：一个
//! `0700` 的 socket 文件天然只有当前用户能连，不需要再发明一套 token。
//! 只有用户显式开启远程访问时才监听 TCP，那时才需要 token。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::Stream;
use tokio::sync::broadcast;

pub mod clients;
pub mod config;
pub mod diagnostics;
pub mod dryrun;
pub mod nics;
pub mod replay;
pub mod rotation;
pub mod scan;
pub use config::{ApplyError, ConfigManager, resolve_path, spawn_watcher};
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
    pub gateway_addr: Option<String>,
    /// 请求历史。**可能没有** —— 磁盘起不来时观测这一层整个不在，
    /// 而那时网关照常转发，所以它是 Option 而不是必需品。
    pub store: Option<Arc<tokio::sync::Mutex<tw_store::Recorder>>>,
    /// 用户的 home。接管要顺着它去找各客户端的配置。
    ///
    /// **是个字段，不是每次现读 `$HOME`。**进程级的环境变量是全局可变
    /// 状态：测试里改一次，同进程里并行跑的另一个测试就会去读一个它
    /// 没想到的目录 —— 而这个模块写的是用户其他软件的配置文件。
    pub home: std::path::PathBuf,
}

/// `$HOME`。取不到时给一个空路径，而不是 `/` —— 空路径会让后续的
/// 「文件不存在」自然发生，`/` 则会让我们去翻系统根目录。
pub fn home_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
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
    /// 探测复用数据面的 HTTP 客户端 —— 同一套超时、同一套代理设置。
    /// 另起一个会让「探测通了但实际请求不通」变成可能。
    pub fn http(&self) -> &reqwest::Client {
        &self.gateway.http
    }
    pub fn health(&self) -> &Arc<tw_gateway::Health> {
        &self.gateway.health
    }
}

pub fn router(state: ControlState) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/interfaces", get(interfaces))
        .route("/events", get(events))
        .route("/overview", get(overview))
        .route("/probe", post(probe))
        .route("/l1", post(l1))
        .route(
            "/config",
            get(get_config).patch(patch_config).put(put_config),
        )
        .route("/config/history", get(config_history))
        .route("/config/at", get(config::path_at))
        .route("/pricing", get(pricing_get).put(pricing_put))
        // 「检查价格更新」三步走。**三个端点，不是一个**
        // —— 一个端点意味着「检查」和「写入」是同一次调用，而那正是
        // 「静默下载」的定义
        .route("/pricing/update/offer", post(update_offer))
        .route("/pricing/update/fetch", post(update_fetch))
        .route("/pricing/update/apply", post(update_apply))
        .route("/config/rollback", post(config_rollback))
        .route("/summary", get(summary))
        .route("/summary/buckets", get(cost_buckets))
        .route("/summary/by", get(cost_by))
        .route("/history", get(history))
        .route("/latency", get(latency))
        .route("/latency/provider", get(latency_by_provider))
        .route("/storage", get(storage))
        .route("/quota", get(quota))
        .route("/leaks", get(leaks))
        .route("/speed/quote", post(speed_quote))
        .route("/speed/run", post(speed_run))
        .route("/request/{id}", get(request_detail))
        .route("/setup", post(setup))
        // 接管：**plan 和 adopt 是两个端点**，中间夹一次人的确认
        .route("/baseline", get(baseline))
        // 诊断包（脱敏纪律）。**只读，不写任何文件**
        .route("/diagnostics", get(diagnostics::bundle))
        // **报价和真跑是两个端点**：这一步花钱（和 L3 测速同一条纪律）
        // 把一条真实请求变成回放用例。**录制不是新功能** ——
        // 每个请求本来就在存储里
        .route("/request/{id}/fixture", get(replay::fixture))
        .route("/replay/quote", post(replay::quote))
        .route("/replay/run", post(replay::run))
        .route("/sessions", get(sessions))
        .route("/sessions/{id}", get(session_detail))
        .route("/dryrun", post(dryrun::dry_run))
        // **每次现扫，什么都不存**
        .route("/scan", get(scan::scan))
        .route("/clients", get(clients::list))
        .route("/clients/plan", post(clients::plan_adopt))
        .route("/clients/adopt", post(clients::adopt))
        .route("/clients/{id}/restore/plan", get(clients::plan_restore))
        .route("/clients/{id}/restore", post(clients::restore))
        .route("/clients/{id}/why", get(clients::why))
        // 矩阵上点一下。**plan 和 apply 同样是两步**
        .route("/mcp/targets", get(clients::mcp_targets))
        .route("/mcp/plan", post(clients::mcp_plan_op))
        .route("/mcp/apply", post(clients::mcp_apply))
        .with_state(state)
}

/// 这台机器上有哪些网卡。
///
/// 不带任何状态 —— 每次现问系统。**网卡是会变的**：插拔网线、连上另一
/// 个 Wi-Fi、起一条 VPN，清单就不一样了，缓存下来只会让选单里出现一个
/// 已经不存在的地址。
async fn interfaces() -> Json<Vec<tw_api::NicView>> {
    Json(
        crate::nics::list()
            .into_iter()
            .map(|n| tw_api::NicView {
                name: n.name,
                loopback: n.addr.is_loopback(),
                addr: n.addr.to_string(),
            })
            .collect(),
    )
}

async fn status(State(s): State<ControlState>) -> Json<tw_api::Status> {
    let cfg = s.config();
    Json(tw_api::Status {
        api_version: tw_api::CONTROL_API_VERSION,
        version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
        gateway_addr: s.gateway_addr.clone(),
        config_path: s.config_path().display().to_string(),
        clients: cfg.clients.len(),
        providers: cfg.providers.len(),
        uptime_secs: s.started.elapsed().as_secs(),
    })
}

async fn events(
    State(s): State<ControlState>,
) -> Sse<impl Stream<Item = Result<SseEvent, std::convert::Infallible>>> {
    let rx = s.bus().subscribe();
    let stream = async_stream_from(rx);
    // 心跳。UI 那边要能区分「没有请求」和「连接断了」—— 没有心跳的话
    // 一个安静的下午看起来就像挂了。
    Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
}

fn async_stream_from(
    mut rx: broadcast::Receiver<tw_api::Event>,
) -> impl Stream<Item = Result<SseEvent, std::convert::Infallible>> {
    futures::stream::unfold(rx_state(&mut rx), |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let data = serde_json::to_string(&ev).unwrap_or_default();
                    return Some((Ok(SseEvent::default().data(data)), rx));
                }
                // 订阅者跟不上时 broadcast 会丢最老的。**继续收而不是断开** ——
                // UI 少几行实时日志无所谓，断掉重连才是真的难受。
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped = n, "控制面订阅者跟不上，丢了一些事件");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
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
    Json(tw_api::Overview {
        proxies: cfg.proxies.iter().map(|x| x.name.clone()).collect(),
        providers: cfg
            .providers
            .iter()
            .map(|p| tw_api::ProviderView {
                name: p.name.clone(),
                base_url: tw_secret::redact_url(&p.base_url),
                key_source: p.key.describe(),
                protocol: p.effective_protocol().map(|x| format!("{x:?}")),
                proxy: p.proxy.clone(),
                health: match s.health().state(&p.name) {
                    tw_gateway::health::State::Closed => "ok".into(),
                    tw_gateway::health::State::Open => "open".into(),
                },
                billing: p.billing.map(|b| b.slug().to_string()),
                // **给判完的结果，不是配置里那个 Option。**界面要显示的是
                // 「这家现在算不算受信任」，而那件事在没写的时候由
                // base_url 决定
                trust: tw_gateway::guard::effective_trust(p, &tw_engine::Guard::default())
                    .label()
                    .to_string(),
                trust_explicit: p.trust.is_some(),
                // 和 trust 同一个理由：给判完的结果。不写的时候官方端点是
                // 空的、其余是那四类默认，而用户要看的是「这家实际脱哪几
                // 类」
                redact: p
                    .effective_redact()
                    .iter()
                    .map(|k| k.slug().to_string())
                    .collect(),
                redact_explicit: p.redact.is_some(),
            })
            .collect(),
        routes: engine
            .routes()
            .iter()
            .map(|set| tw_api::RouteView {
                name: set.name.clone(),
                default: set.default,
                clients: cfg
                    .clients
                    .iter()
                    .filter(|c| c.routes.contains(&set.name))
                    .map(|c| c.name.clone())
                    .collect(),
                rules: set
                    .rules
                    .iter()
                    .map(|r| tw_api::RuleView {
                        name: r.name.clone(),
                        // 阶段二的规则没有去向 —— 它们只改参数或拒绝
                        to: r.to.clone().unwrap_or_else(|| {
                            if r.deny.is_some() {
                                "拒绝".into()
                            } else {
                                "（只改参数）".into()
                            }
                        }),
                        conditions: describe_when(&r.when),
                    })
                    .collect(),
            })
            .collect(),
        groups: engine
            .groups()
            .iter()
            .map(|g| tw_api::GroupView {
                name: g.name.clone(),
                kind: g.kind.label().to_string(),
                session_affinity: g.session_affinity,
                selected: g.selected.clone(),
                providers: g.providers.clone(),
                hurts_cache: g.kind.hurts_cache(),
            })
            .collect(),
        clients: cfg
            .clients
            .iter()
            .map(|c| tw_api::ClientView {
                name: c.name.clone(),
                key: tw_secret::mask_secret(&c.key),
                max_concurrent: c.max_concurrent,
            })
            .collect(),
        security: tw_api::SecurityView {
            redact: cfg.security.redact.slug().to_string(),
            inspect_tools: cfg.security.inspect_tools.slug().to_string(),
            scan_configs: cfg.security.scan_configs.slug().to_string(),
            scan_rules_added: cfg.security.scan_rules.add.len(),
            scan_rules_disabled: cfg.security.scan_rules.disable.len(),
        },
        listen: tw_api::ListenView {
            bind: format!("{:?}", cfg.listen.gateway.bind).to_lowercase(),
            port: cfg.listen.gateway.port,
            allow_from: cfg.listen.gateway.effective_allow_from(),
            exposed: cfg.listen.gateway.bind.is_exposed(),
        },
    })
}

/// 把 `when` 写成人话。
///
/// **规则列表上必须能直接读懂条件** —— 让用户去对着 YAML 猜「这条为什么
/// 没命中」，正是那类「我明明配了」问题的来源。
fn describe_when(w: &tw_engine::rule::When) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(m) = &w.model {
        out.push(format!("模型 {m}"));
    }
    if let Some(c) = &w.client {
        out.push(format!("客户端 {c}"));
    }
    if let Some(d) = &w.dialect {
        out.push(format!("方言 {d}"));
    }
    for (label, v) in [
        ("输入 token", &w.input_tokens),
        ("max_tokens", &w.max_tokens),
        ("工具数", &w.tool_count),
    ] {
        if let Some(x) = v {
            out.push(format!("{label} {x}"));
        }
    }
    if let Some(i) = &w.intent {
        out.push(format!("客户端辅助请求 {}", join_one_or_many(i)));
    }
    // **不写出来的话，一条只有 provider_would_be 的规则在界面上会显示成
    // 「兜底」**（条件为空就是兜底的标记）—— 那是个会让人查半天的假象。
    if let Some(p) = &w.provider_would_be {
        out.push(format!("将要走 {}", join_one_or_many(p)));
    }
    for (label, v) in [
        ("带缓存", w.cache),
        ("带工具", w.tools),
        ("带图片", w.image),
        ("扩展思考", w.thinking),
        ("流式", w.stream),
    ] {
        if let Some(b) = v {
            out.push(if b {
                label.to_string()
            } else {
                format!("不{label}")
            });
        }
    }
    out
}

fn join_one_or_many(v: &tw_engine::rule::OneOrMany) -> String {
    match v {
        tw_engine::rule::OneOrMany::One(s) => s.clone(),
        tw_engine::rule::OneOrMany::Many(xs) => xs.join(" 或 "),
    }
}

/// 探一个上游。零成本，用户可以随便点。
async fn probe(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::ProbeRequest>,
) -> Json<tw_api::ProbeResponse> {
    let r = tw_gateway::probe(s.http(), &req.base_url, &req.key, None).await;
    // 两边的枚举是同一份契约的两个副本（core 内部一份、控制面契约一份）。
    // 手工转换是为了让 tw-api 不依赖 tw-gateway —— UI 和 CLI 只该依赖
    // 契约，不该被拖上整个数据面。
    let models = match r.models {
        tw_gateway::ModelList::Listed { models } => tw_api::ModelList::Listed { models },
        tw_gateway::ModelList::NotImplemented { status } => {
            tw_api::ModelList::NotImplemented { status }
        }
        tw_gateway::ModelList::Unrecognized { sample } => {
            tw_api::ModelList::Unrecognized { sample }
        }
        tw_gateway::ModelList::Empty => tw_api::ModelList::Empty,
    };
    Json(tw_api::ProbeResponse {
        ok: r.ok,
        protocol: r.protocol,
        latency_ms: r.latency_ms,
        models,
        error: r.error,
    })
}

/// L1 测速。**零成本**，不发任何业务请求，用户可以随便点。
///
/// 一次可能测好几家，所以逐个测而不是并发：**并发会让每一段的耗时互相
/// 干扰**，六条线一起抢带宽测出来的 TLS 时间不是任何一条线的真实值，
/// 而这一层存在的全部意义就是那几个数字准不准。
async fn l1(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::L1Request>,
) -> Result<Json<Vec<tw_api::L1Result>>, (StatusCode, String)> {
    let cfg = s.config();
    let mut out = Vec::new();

    // 只测代理本身。代理影响的是网络层，测到 L1 就够了。
    if let Some(name) = &req.proxy {
        let p = cfg
            .proxies
            .iter()
            .find(|x| x.name == *name)
            .ok_or_else(|| (StatusCode::NOT_FOUND, format!("没有叫 `{name}` 的代理")))?;
        let mut r = tw_gateway::l1_tcp(&p.addr).await;
        r.notes
            .push("只测到代理这一跳的 TCP 握手。代理影响的是网络层，再往上就该测上游了。".into());
        out.push(view(format!("代理 {}", p.name), None, r));
        return Ok(Json(out));
    }

    // 还没保存时的临时地址（首次配置那一步）。
    if let Some(url) = &req.base_url {
        let r = tw_gateway::l1(url, None).await;
        out.push(view(url.clone(), None, r));
        return Ok(Json(out));
    }

    let targets: Vec<&tw_config::Provider> = match &req.provider {
        Some(n) => vec![
            cfg.providers
                .iter()
                .find(|p| p.name == *n)
                .ok_or_else(|| (StatusCode::NOT_FOUND, format!("没有叫 `{n}` 的上游")))?,
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
                    notes: Vec::new(),
                    error: Some(e),
                });
                continue;
            }
        };
        let via = hop.as_ref().map(|_| p.proxy.clone());
        let r = tw_gateway::l1(&p.base_url, hop.as_ref()).await;
        out.push(view(p.name.clone(), via, r));
    }
    Ok(Json(out))
}

fn view(target: String, via: Option<String>, r: tw_gateway::L1Result) -> tw_api::L1Result {
    tw_api::L1Result {
        target,
        via,
        ok: r.ok,
        segments: r
            .segments
            .into_iter()
            .map(|x| tw_api::L1Segment {
                name: x.name,
                ms: x.ms,
            })
            .collect(),
        total_ms: r.total_ms,
        notes: r.notes,
        error: r.error,
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
    axum::extract::Query(q): axum::extract::Query<BucketQuery>,
) -> Result<Json<Vec<tw_api::CostBucket>>, Fail> {
    let (from, to) = q.range();
    // 桶宽有下限，否则一个 `bucket_ms=1` 能让这条查询扫出几百万个分组。
    let bucket = q.bucket_ms.unwrap_or(3_600_000).max(1_000);
    let store = need_store(&s)?;
    let g = store.lock().await;
    let x = g
        .db()
        .cost_buckets(from, to, bucket)
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(x))
}

/// 按模型或上游分组的花费（钱花在哪儿）。
async fn cost_by(
    State(s): State<ControlState>,
    axum::extract::Query(q): axum::extract::Query<GroupQuery>,
) -> Result<Json<Vec<tw_api::CostGroup>>, Fail> {
    let (from, to) = q.range();
    let store = need_store(&s)?;
    let g = store.lock().await;
    let x = g
        .db()
        .cost_by(q.dim, from, to)
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(x))
}

/// 一段时间的汇总。不给参数就是「今天」。
async fn summary(
    State(s): State<ControlState>,
    axum::extract::Query(q): axum::extract::Query<Window>,
) -> Result<Json<tw_api::Summary>, Fail> {
    let (from, to) = q.range();
    let store = need_store(&s)?;
    let g = store.lock().await;
    let x = g
        .db()
        .summary(from, to)
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
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
        subscription_requests: x.subscription_requests,
        subscription_tokens: x.subscription_tokens,
        cache_saved_micros: x.cache_saved_micros,
        pricing_date: tw_pricing::SNAPSHOT_DATE.to_string(),
    }))
}

/// 最近的请求。**实时列表走内存 ring buffer，这个是给「翻历史」的**。
async fn history(
    State(s): State<ControlState>,
    axum::extract::Query(q): axum::extract::Query<Limit>,
) -> Result<Json<Vec<tw_api::HistoryRow>>, Fail> {
    let store = need_store(&s)?;
    let g = store.lock().await;
    let rows = g
        .db()
        .recent(q.limit.unwrap_or(200).min(2000))
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(rows.into_iter().map(history_row).collect()))
}

async fn latency(
    State(s): State<ControlState>,
    axum::extract::Query(q): axum::extract::Query<Window>,
) -> Result<Json<Vec<tw_api::LatencyView>>, Fail> {
    let (from, to) = q.range();
    let store = need_store(&s)?;
    let g = store.lock().await;
    let xs = g
        .db()
        .latency_by_model(from, to)
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
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
    let prices = prices(&s);
    let items: Vec<tw_gateway::Estimate> = targets(&cfg, req.provider.as_deref())?
        .iter()
        .map(|p| {
            tw_gateway::l3::estimate(
                &prices,
                &p.name,
                &req.model,
                // 订阅型上游的判据：最近一次响应里报过额度。
                // **不是一个用户要填的字段** —— 那个数字一直在我们手上。
                s.gateway
                    .quotas()
                    .get(&p.name)
                    .is_some_and(|q| !q.is_empty()),
            )
        })
        .collect();
    Ok(Json(tw_api::SpeedQuote {
        total_micros: tw_gateway::l3::total_micros(&items),
        items: items.into_iter().map(quote_item).collect(),
        pricing_date: tw_pricing::SNAPSHOT_DATE.to_string(),
    }))
}

/// 真的跑。**这一步花钱。**
async fn speed_run(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::SpeedRunRequest>,
) -> Result<Json<Vec<tw_api::SpeedResult>>, Fail> {
    let cfg = s.config();
    let mut out = Vec::new();
    // **逐个跑，不并发。**几家一起打，测出来的 TTFT 互相干扰，而这一层
    // 存在的全部意义就是那几个数字准不准（和 L1 同一个理由）。
    for p in targets(&cfg, req.provider.as_deref())? {
        // OAuth 那类要联网换 token，所以走网关那条 async 的路。
        // **用这一家自己的 client** —— 换 token 要走它的代理。
        let pk_http = s.gateway.client_for(&p.name);
        let key = match s.gateway.key_for(p, &pk_http).await {
            Ok(k) => k,
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
                    error: Some(format!("密钥取不到：{e}")),
                });
                continue;
            }
        };
        let r = tw_gateway::l3::run(
            s.http(),
            &p.base_url,
            &key,
            p.effective_protocol(),
            &p.name,
            &req.model,
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

fn quote_item(e: tw_gateway::Estimate) -> tw_api::SpeedEstimate {
    tw_api::SpeedEstimate {
        provider: e.provider,
        model: e.model,
        input_tokens: e.input_tokens,
        max_output_tokens: e.max_output_tokens,
        cost_micros: e.cost_micros,
        note: e.note,
    }
}

/// 第一步：**只问「要访问什么、多大」，一个字节都不下载。**
///
/// 零上传的承诺同时意味着**零静默下载** —— 而这条承诺里最容易被
/// 省掉的一半，正是「先告诉你要连哪儿」。
async fn update_offer(State(s): State<ControlState>) -> Result<Json<tw_api::UpdateOffer>, Fail> {
    let r = s
        .http()
        .head(tw_pricing::UPDATE_URL)
        .send()
        .await
        .map_err(|e| fail(StatusCode::BAD_GATEWAY, format!("连不上：{e}")))?;
    Ok(Json(tw_api::UpdateOffer {
        url: tw_pricing::UPDATE_URL.to_string(),
        bytes: r.content_length(),
        current_date: tw_pricing::SNAPSHOT_DATE.to_string(),
    }))
}

/// 第二步：下载、解析、**给 diff，但不写盘**。
async fn update_fetch(State(s): State<ControlState>) -> Result<Json<tw_api::UpdatePreview>, Fail> {
    let raw = s
        .http()
        .get(tw_pricing::UPDATE_URL)
        .send()
        .await
        .map_err(|e| fail(StatusCode::BAD_GATEWAY, format!("下载失败：{e}")))?
        .bytes()
        .await
        .map_err(|e| fail(StatusCode::BAD_GATEWAY, format!("下载失败：{e}")))?;
    let table = tw_pricing::parse_upstream(&raw)
        .map_err(|e| fail(StatusCode::BAD_GATEWAY, e.to_string()))?;
    let cur = prices(&s);
    let changes = tw_pricing::diff(cur.table(), &table);
    let token = blake3::hash(&raw).to_hex()[..16].to_string();
    // **存在磁盘上，不存在内存里。**第三步可能几分钟之后才来（用户在
    // 看 diff），而这中间进程完全可能重启
    let path = pending_path(&s);
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    std::fs::write(&path, &raw)
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("存不下来：{e}")))?;
    Ok(Json(tw_api::UpdatePreview {
        models: table.len(),
        changes: changes
            .into_iter()
            // 三千多个模型全列出来没人看得完。**改价的排在前面**
            // （见 `tw_pricing::diff`），所以截断截掉的是新增那一批
            .take(200)
            .map(|c| tw_api::PriceChangeView {
                model: c.model,
                old_input: c.old_input,
                new_input: c.new_input,
                old_output: c.old_output,
                new_output: c.new_output,
            })
            .collect(),
        token,
    }))
}

/// 第三步：**确认之后才写**。
async fn update_apply(
    State(s): State<ControlState>,
    Json(req): Json<ApplyReq>,
) -> Result<Json<tw_api::PricingView>, Fail> {
    let path = pending_path(&s);
    let raw = std::fs::read(&path).map_err(|_| {
        fail(
            StatusCode::BAD_REQUEST,
            "没有待确认的更新 —— 先点「检查更新」".to_string(),
        )
    })?;
    // **指纹要对上。**不对上的话，「确认写入」写的可能是另一次下载的
    // 结果 —— 用户看的 diff 和落盘的东西不是一回事
    let token = blake3::hash(&raw).to_hex()[..16].to_string();
    if token != req.token {
        return Err(fail(
            StatusCode::CONFLICT,
            "这份更新和你看到的 diff 对不上，重新检查一次".to_string(),
        ));
    }
    let dir = s
        .config_path()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    // 落到数据目录，加载时优先于内置快照
    std::fs::rename(&path, dir.join("model_prices.json"))
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("写不进去：{e}")))?;
    pricing_get(State(s)).await
}

#[derive(serde::Deserialize)]
struct ApplyReq {
    token: String,
}

/// 下载完还没确认的那份放哪儿。
fn pending_path(s: &ControlState) -> std::path::PathBuf {
    s.config_path()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default()
        .join("model_prices.pending.json")
}

/// 用户自己写的那份价格（第三层）。
///
/// **这一页存在的理由是「有 N 条请求算不出钱」** —— 用户不会主动想起
/// 要配价格（高级功能的触发条件要绑在「这个问题存不存在」上）。
async fn pricing_get(State(s): State<ControlState>) -> Result<Json<tw_api::PricingView>, Fail> {
    let p = prices(&s);
    let rows = p
        .overrides_list()
        .into_iter()
        .map(|(provider, model, mp)| tw_api::PriceRow {
            overrides_builtin: p.builtin_has(&model),
            provider,
            // **每百万 token 的美元** —— 和厂商定价页上印的一样
            input: mp.input * 1_000_000.0,
            output: mp.output * 1_000_000.0,
            model,
        })
        .collect();
    // 算不出价钱的那些。**拿不到存储就是 0** —— 观测层起不来时网关
    // 照常转发，这一页也该照常打开
    let (unpriced_recent, unpriced_models) = match &s.store {
        Some(st) => {
            let g = st.lock().await;
            g.db().unpriced_recent(7).unwrap_or_else(|e| {
                tracing::debug!("算不出价钱的统计取不到：{e}");
                (0, Vec::new())
            })
        }
        None => (0, Vec::new()),
    };
    Ok(Json(tw_api::PricingView {
        rows,
        // **这份表自己的日期，不是那个常量。**用户点过更新之后还显示
        // 内置快照的日期，等于把「你的价目表是哪天的」这个问题答错了
        // —— 而成本旁边标它的全部意义就是回答那个问题
        snapshot_date: p.snapshot_date.clone(),
        unpriced_recent,
        unpriced_models,
    }))
}

/// 整份写回去。**和配置文件同一条纪律**：先在内存里验一遍，再原子写。
async fn pricing_put(
    State(s): State<ControlState>,
    Json(rows): Json<Vec<tw_api::PriceRow>>,
) -> Result<Json<tw_api::PricingView>, Fail> {
    use std::collections::HashMap;
    let mut models: HashMap<String, Option<tw_pricing::ModelPrice>> = HashMap::new();
    let mut providers: HashMap<String, HashMap<String, tw_pricing::ModelPrice>> = HashMap::new();
    for r in &rows {
        if r.model.trim().is_empty() {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "有一行没填模型名".to_string(),
            ));
        }
        // **负价不是「便宜」，是写错了。**让它进去的话，`cheapest` 会
        // 永远选中那一家，而成本栏会往下走
        if r.input < 0.0 || r.output < 0.0 {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                format!("`{}` 的单价是负数", r.model),
            ));
        }
        let mp = tw_pricing::ModelPrice {
            input: r.input / 1_000_000.0,
            output: r.output / 1_000_000.0,
            cache_read: None,
            cache_write_5m: None,
            cache_write_1h: None,
            input_above_200k: None,
            output_above_200k: None,
            max_input_tokens: None,
        };
        match r.provider.as_ref().filter(|p| !p.trim().is_empty()) {
            Some(p) => {
                providers
                    .entry(p.clone())
                    .or_default()
                    .insert(r.model.clone(), mp);
            }
            None => {
                models.insert(r.model.clone(), Some(mp));
            }
        }
    }
    let text = serde_yaml_ng::to_string(&tw_pricing::Overrides { models, providers })
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let head = "# ThinkWatch 的用户价格覆盖（第三层）。\n\
                # 单位是**每 token** 的美元 —— 界面上填的是每百万，这里换算过了。\n\
                # 这个文件是界面写的，但手改也没问题：它只是一份普通 YAML。\n";
    let dir = s
        .config_path()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    tw_config::store::write_atomic(&dir.join("pricing.yaml"), &format!("{head}{text}"))
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    pricing_get(State(s)).await
}

/// 价目表。**每次现建** —— 用户可能刚改过 pricing.yaml，而报价这件事
/// 一年也点不了几次。
pub(crate) fn prices(s: &ControlState) -> tw_pricing::Prices {
    let dir = s
        .config_path()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    tw_pricing::Prices::builtin()
        // 用户点过「更新」的话，用拉回来的那份（第二层）。
        // **读不了就退回内置那份** —— 一个损坏的更新文件不该让成本栏
        // 整个变成「未知」
        .map(|mut p| {
            let f = dir.join("model_prices.json");
            match std::fs::read(&f).ok().and_then(|raw| {
                // **要给一个人看得懂的日期。**成本旁边标的是「这份
                // 价目表是哪天的」，而「第 20800 天」回答不了那个问题
                let date = std::fs::metadata(&f)
                    .and_then(|m| m.modified())
                    .ok()
                    .map(|t| {
                        let dt: chrono::DateTime<chrono::Local> = t.into();
                        dt.format("%Y-%m-%d").to_string()
                    })
                    .unwrap_or_else(|| "用户更新的".to_string());
                tw_pricing::parse_upstream(&raw).ok().map(|t| (t, date))
            }) {
                Some((t, date)) => {
                    p.replace_table(t, date);
                    p
                }
                None => p,
            }
        })
        .and_then(|p| p.with_overrides(&dir.join("pricing.yaml")))
        .unwrap_or_else(|e| {
            tracing::warn!("价目表读不了，测速报价会说「价格未知」：{e}");
            // 建不出来时给一个空表 —— 那时每一项都是「价格未知」，
            // 而那正是诚实的答案
            tw_pricing::Prices::empty()
        })
}

fn targets<'a>(
    cfg: &'a tw_config::Config,
    provider: Option<&str>,
) -> Result<Vec<&'a tw_config::Provider>, Fail> {
    match provider {
        Some(n) => Ok(vec![
            cfg.providers
                .iter()
                .find(|p| p.name == n)
                .ok_or_else(|| (StatusCode::NOT_FOUND, format!("没有叫 `{n}` 的上游")))?,
        ]),
        None => Ok(cfg.providers.iter().collect()),
    }
}

/// 出站密钥检测攒下的证据。
///
/// **默认看过去 7 天** —— 那正是「跑上一周」之后那句话的时间尺度。
async fn leaks(
    State(s): State<ControlState>,
    axum::extract::Query(q): axum::extract::Query<Days>,
) -> Result<Json<Vec<tw_api::LeakGroup>>, Fail> {
    let since = now_ms() - (q.days.unwrap_or(7) as i64) * 86_400_000;
    let store = need_store(&s)?;
    let g = store.lock().await;
    let xs = g
        .db()
        .leak_summary(since)
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(
        xs.into_iter()
            .map(|l| tw_api::LeakGroup {
                provider: l.provider,
                kind: l.kind,
                requests: l.requests,
                last_at_ms: l.last_at_ms,
                masked: l.masked,
            })
            .collect(),
    ))
}

#[derive(Debug, serde::Deserialize)]
struct Days {
    days: Option<u32>,
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
    let row = g
        .db()
        .get(id)
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("没有第 {id} 号请求")))?;
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
    let detail = tw_api::RequestDetail {
        request_body: body(tw_store::Which::Request),
        response_body: body(tw_store::Which::Response),
        row: history_row(row),
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
                    label: w.label,
                    used_percent: w.used_percent,
                    reset_in_secs: w.reset_in_secs,
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
    axum::extract::Query(q): axum::extract::Query<Window>,
) -> Result<Json<Vec<tw_api::LatencyView>>, Fail> {
    let (from, to) = q.range();
    let store = need_store(&s)?;
    let g = store.lock().await;
    let xs = g
        .db()
        .latency_by_provider(from, to)
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
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
            level: "没有起来（历史记录不可用，转发不受影响）".into(),
            rows: 0,
            blob_bytes: 0,
            forwarding_affected: false,
        });
    };
    let g = store.lock().await;
    Json(tw_api::StorageStatus {
        level: g.level().label().to_string(),
        rows: g.db().count().unwrap_or(0),
        blob_bytes: g.blobs().total_bytes(),
        // **永远是 false。**观测挂了，代理照跑。哪天有人想改成
        // true，先回去读那一节。
        forwarding_affected: false,
    })
}

fn need_store(s: &ControlState) -> Result<&Arc<tokio::sync::Mutex<tw_store::Recorder>>, Fail> {
    s.store.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "历史记录这一层没起来（磁盘或数据库有问题）。转发不受影响。".to_string(),
        )
    })
}

fn history_row(r: tw_store::RequestRow) -> tw_api::HistoryRow {
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
        // 解不开就当没有。**一条坏掉的路由记录不该让整条请求记录读不出来**
        // —— 那是详情页上的一栏，不是这一行存在的理由。
        routing: r
            .routing
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok()),
        billing: r.billing,
        cache_saved_micros: r.cache_saved_micros,
    }
}

/// 时间窗。**默认是「今天」而不是「最近 24 小时」** —— 用户问的是
/// 「今天花了多少」，那是个从零点算起的问题。
#[derive(Debug, serde::Deserialize)]
struct Window {
    from_ms: Option<i64>,
    to_ms: Option<i64>,
}

/// **不能用 `#[serde(flatten)]` 摊平 `Window`。**
///
/// flatten 会让 serde 走 `deserialize_any`，而 query string 里一切都是
/// 字符串 —— 于是 `from_ms=123` 被当成字符串喂给 `i64`，整个请求 400：
/// `invalid type: string "123", expected i64`。
///
/// 表现很坏：不带参数时一切正常，只有在**同时带上时间窗**的时候才失败，
/// 而界面恰恰总是带着它调。单元测试看不见这件事 —— 它只在真的经过一次
/// query string 解析时才发生。
#[derive(serde::Deserialize)]
struct BucketQuery {
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    bucket_ms: Option<i64>,
}

impl BucketQuery {
    fn range(&self) -> (i64, i64) {
        Window {
            from_ms: self.from_ms,
            to_ms: self.to_ms,
        }
        .range()
    }
}

#[derive(serde::Deserialize)]
struct GroupQuery {
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    /// **是枚举不是字符串。**它会决定 SQL 里的列名，用字符串就是一个
    /// 注入口；写错的值在这里被 serde 直接拒掉，而不是拼进查询。
    dim: tw_api::CostDim,
}

impl GroupQuery {
    fn range(&self) -> (i64, i64) {
        Window {
            from_ms: self.from_ms,
            to_ms: self.to_ms,
        }
        .range()
    }
}

impl Window {
    fn range(&self) -> (i64, i64) {
        let now = now_ms();
        // 本地时区的零点。UTC 零点对一个桌面工具没有意义 —— 用户在
        // 东八区，UTC 零点是他的早上八点。
        let midnight = local_midnight_ms(now);
        (self.from_ms.unwrap_or(midnight), self.to_ms.unwrap_or(now))
    }
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

#[derive(Debug, serde::Deserialize)]
struct Limit {
    limit: Option<usize>,
}

/// 当前配置的原文。**文本模式直接显示它。**
async fn get_config(State(s): State<ControlState>) -> Result<Json<tw_api::ConfigText>, Fail> {
    let c = s
        .cfg
        .current()
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(tw_api::ConfigText {
        path: c.path.display().to_string(),
        version: c.version(),
        text: c.text,
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
    let all = tw_config::history::list(s.config_path())
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let now = s.cfg.current().map(|c| c.version()).unwrap_or_default();
    // **新的在前。**用户找的几乎总是最近那几版。
    Ok(Json(
        all.into_iter()
            .rev()
            .map(|v| tw_api::ConfigVersion {
                current: v.version == now,
                version: v.version,
                at_ms: v.at_ms,
                origin: v.origin.label().to_string(),
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

type Fail = (StatusCode, String);

fn fail(code: StatusCode, e: impl std::fmt::Display) -> Fail {
    (code, e.to_string())
}

/// 把配置改动的失败翻成 HTTP。
///
/// **`Stale` 必须是 409 而不是 400。**界面要能区分「我写错了」和「有人
/// 抢先改了」—— 后者的正确反应是刷新再合并，而不是给用户看一条错误。
fn apply_fail(e: ApplyError) -> Fail {
    let code = match &e {
        ApplyError::Stale { .. } => StatusCode::CONFLICT,
        ApplyError::Store(tw_config::StoreError::Conflict { .. }) => StatusCode::CONFLICT,
        ApplyError::Rejected(_) | ApplyError::Build(_) | ApplyError::BadPath(_) => {
            StatusCode::BAD_REQUEST
        }
        ApplyError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (code, e.to_string())
}

/// 首次运行：写下第一个上游。
///
/// **整文件生成**，不走最小替换 —— 那是两套机制（第 1 步）。
/// 只在还没有 provider 时可用，之后改配置归 M2 的双向同步管。
/// 最近这一段有多长。
const RECENT_HOURS: u32 = 24;
/// 拿来当基线的那一段有多长。
const BASELINE_DAYS: u32 = 30;

/// 每个上游最近是不是变了（防线三）。
///
/// **两段时间不重叠**：基线是「最近这一段之前的那 30 天」，不含最近的
/// 那 24 小时。重叠的话，一次异常会同时抬高两边，把自己的信号冲淡。
async fn baseline(State(s): State<ControlState>) -> Json<tw_api::BaselineResponse> {
    let mut out = tw_api::BaselineResponse {
        recent_hours: RECENT_HOURS,
        baseline_days: BASELINE_DAYS,
        providers: Vec::new(),
        unavailable: s.store.is_none(),
    };
    let Some(store) = &s.store else {
        return Json(out);
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let recent_from = now - (RECENT_HOURS as i64) * 3_600_000;
    let base_from = recent_from - (BASELINE_DAYS as i64) * 86_400_000;

    let g = store.lock().await;
    for provider in g.db().providers_seen().unwrap_or_default() {
        let Ok(recent) = g.db().shape_of(&provider, recent_from, now) else {
            continue;
        };
        let Ok(base) = g.db().shape_of(&provider, base_from, recent_from) else {
            continue;
        };
        out.providers.push(tw_api::ProviderBaseline {
            recent_total: recent.total,
            baseline_total: base.total,
            recent_inspected: recent.inspected,
            baseline_inspected: base.inspected,
            drifts: tw_store::drift::compare(&recent, &base)
                .into_iter()
                .map(|d| tw_api::DriftView {
                    metric: d.metric.to_string(),
                    label: d.label.to_string(),
                    recent: d.recent,
                    baseline: d.baseline,
                    recent_n: d.recent_n,
                    baseline_n: d.baseline_n,
                    notable: d.notable,
                })
                .collect(),
            provider,
        });
    }
    Json(out)
}

fn session_view(s: &tw_store::db::SessionRow) -> tw_api::SessionView {
    tw_api::SessionView {
        id: s.id.clone(),
        client: s.client.clone(),
        started_ms: s.started_ms as u64,
        ended_ms: s.ended_ms as u64,
        turns: s.turns as u64,
        cost_micros: s.cost_micros,
        unpriced_turns: s.unpriced_turns as u64,
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
    }
}

/// 会话列表。
///
/// **观测层没起来时返回空列表，不是错误**：那时网关照常转发，
/// 界面上少一块统计，而不是弹一个错。
async fn sessions(State(s): State<ControlState>) -> Json<Vec<tw_api::SessionView>> {
    let Some(store) = &s.store else {
        return Json(Vec::new());
    };
    let g = store.lock().await;
    Json(
        g.db()
            .sessions(200)
            .unwrap_or_default()
            .iter()
            .map(session_view)
            .collect(),
    )
}

async fn session_detail(
    State(s): State<ControlState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<tw_api::SessionDetail>, (StatusCode, String)> {
    let Some(store) = &s.store else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "观测层没有启动".into()));
    };
    let g = store.lock().await;
    let session = g
        .db()
        .sessions(500)
        .unwrap_or_default()
        .iter()
        .find(|x| x.id == id)
        .map(session_view)
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("没有叫 `{id}` 的会话")))?;
    let turns = g
        .db()
        .turns(&id)
        .unwrap_or_default()
        .iter()
        .map(turn_view)
        .collect();
    Ok(Json(tw_api::SessionDetail { session, turns }))
}

async fn setup(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::SetupRequest>,
) -> Result<Json<tw_api::SetupResponse>, (StatusCode, String)> {
    // **从磁盘重新读，不看 `s.config`。** 那是启动时的快照，而这个端点
    // 自己就会改磁盘 —— 用快照做守卫，第二次调用会因为看到一份过期的
    // 「零 provider」而通过，然后把刚写好的配置整个覆盖掉。
    //
    // 热重载（M2）之后快照会跟着磁盘走，但那时这条守卫也不该改回去：
    // 「会不会覆盖用户的文件」这种判断，就该问文件本身。
    let current = tw_config::load(s.config_path()).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("读配置失败：{e}"),
        )
    })?;
    if !current.providers.is_empty() {
        // 拒绝而不是覆盖。这个端点存在的前提是「还没有配置」，一旦有了
        // 配置，整文件重写会把用户的注释和格式全抹掉。
        return Err((
            StatusCode::CONFLICT,
            "已经配过上游了。改配置请直接编辑 config.yaml，或者等界面上的配置页。".to_string(),
        ));
    }
    let mut cfg = current;
    cfg.providers.push(tw_config::Provider {
        name: req.name.clone(),
        base_url: req.base_url.clone(),
        key: tw_config::Secret::Literal(req.key.clone()),
        // 猜得出来就不写进文件 —— 少一行是一行。
        ..Default::default()
    });
    tw_config::validate(&cfg).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let text = serde_yaml_ng::to_string(&cfg).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("序列化失败：{e}"),
        )
    })?;
    // **走同一扇门。**直接写文件的话，这次写会被自己的监听当成外部改动
    // （多一次无谓的重载），而且不进历史 —— 于是「刚配好就配错了」没有
    // 退路可回。
    s.cfg
        .write(&text, None, tw_config::history::Origin::Ui)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;

    let gateway_key = cfg
        .clients
        .first()
        .map(|c| c.key.clone())
        .unwrap_or_default();
    Ok(Json(tw_api::SetupResponse {
        gateway_key,
        gateway_addr: s.gateway_addr.clone().unwrap_or_default(),
        config_path: s.config_path().display().to_string(),
    }))
}

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("控制面 socket {path} 起不来：{source}")]
    Bind {
        path: PathBuf,
        source: std::io::Error,
    },
    /// **系统级的硬上限，不是我们的规矩。**`sockaddr_un.sun_path` 在
    /// macOS 上是 104 字节、Linux 上 108 —— 超了 `bind` 会失败，而 libc
    /// 给的原话是「path must be shorter than SUN_LEN」，看不出上限是多少、
    /// 也看不出自己超了多少。
    #[error(
        "控制面 socket 的路径太长：{len} 字节，系统上限是 {max}。\n{path}\n把配置放到一个短一点的目录下（默认的 ~/.thinkwatch 不会有这个问题）。"
    )]
    PathTooLong {
        path: PathBuf,
        len: usize,
        max: usize,
    },
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

/// 在 unix socket 上起控制面。
///
/// 陈旧的 socket 文件直接删掉重建 —— 它和 lock 文件不一样，没有「另一个
/// 实例可能还在用」的歧义：单实例锁已经在上一步挡住了。
pub async fn serve_unix(state: ControlState, path: &Path) -> Result<(), ControlError> {
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 0700：只有当前用户能连。这就是不需要 token 的原因。
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    tracing::info!(path = %path.display(), "控制面已监听");

    let app = router(state);
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("控制面 accept 失败：{e}");
                continue;
            }
        };
        let svc = app.clone();
        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let svc = hyper::service::service_fn(move |req| {
                use tower::ServiceExt;
                svc.clone().oneshot(req)
            });
            if let Err(e) =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(io, svc)
                    .await
            {
                tracing::debug!("控制面连接结束：{e}");
            }
        });
    }
}

/// 默认 socket 路径。和配置放一起，这样「一个目录装下全部状态」这条成立。
pub fn default_socket_path() -> PathBuf {
    tw_config::default_dir().join("twcore.sock")
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
        let p = tw_config::default_dir().join("twcore.sock");
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

    /// **每加一个 `when` 字段就必须在这里加一句人话。**
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
            serde_json::Value::Object(m) => m.len(),
            other => panic!("{other:?}"),
        };
        let lines = describe_when(&full);
        assert_eq!(
            lines.len(),
            fields,
            "有 when 字段没被翻成人话。已翻的：{lines:?}"
        );
    }

    #[test]
    fn a_phase_two_only_rule_is_not_shown_as_a_catch_all() {
        let w: tw_engine::rule::When =
            serde_yaml_ng::from_str("{ provider_would_be: relay }").unwrap();
        let lines = describe_when(&w);
        assert!(!lines.is_empty(), "空的条件列表在界面上就是「兜底」");
        assert!(lines[0].contains("relay"), "{lines:?}");
    }
}
