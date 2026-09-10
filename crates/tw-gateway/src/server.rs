//! HTTP 服务：路由、身份识别、转发。

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{OriginalUri, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};

use crate::auth::key_eq;
use crate::error::GatewayError;
use crate::forward;
use crate::health::Health;

/// 256 MiB。大到能装下几张 4K 图的 base64（膨胀 33%），小到失控的
/// 客户端打不爆内存。
const MAX_BODY: usize = 256 * 1024 * 1024;

/// 所有 Client 共享的那部分设置。**只写一遍** —— 分成两处的话，走代理
/// 的那批和不走代理的那批会慢慢长出不同的超时行为。
fn base_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        // 分段超时（§4.7）。**没有整体超时** —— 一个跑了六分钟的
        // Opus 任务不该被中间层掐断，让客户端自己决定何时放弃。
        .connect_timeout(std::time::Duration::from_secs(10))
        // 响应头超时覆盖不到 DNS 和 TCP 握手，所以上面那条必须显式
        // 设置：DNS 被污染解析到黑洞 IP 时，建连会等满内核重传。
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        // HTTP/2 的死连接探测。NAT / 代理会静默丢弃空闲连接，两端都
        // 以为还活着，下一个请求要等满内核重传（分钟级）。挂 Clash /
        // Surge 的桌面用户几乎必踩，而默认是不发 PING 的。
        .http2_keep_alive_interval(std::time::Duration::from_secs(15))
        .http2_keep_alive_timeout(std::time::Duration::from_secs(15))
        .http2_keep_alive_while_idle(true)
}

/// 给一个 provider 建 Client，带上它该走的代理。
fn build_client(
    cfg: &tw_config::Config,
    p: &tw_config::Provider,
) -> Result<reqwest::Client, GatewayError> {
    let mut b = base_client_builder();
    match p.proxy.as_str() {
        tw_config::DIRECT => {
            // 强制直连，忽略一切系统设置 —— 本地 Ollama 走了代理必挂。
            b = b.no_proxy();
        }
        tw_config::SYSTEM => {
            // reqwest 默认就读系统代理，什么都不做即可。
        }
        name => {
            let proxy = cfg.proxies.iter().find(|x| x.name == name).ok_or_else(|| {
                // 用 concat! 而不是反斜杠续行：续行后面那行的缩进会
                // 原样进字符串，而错误信息里冒出一串空格没人会注意到。
                // 今天已经犯过一次了（还没有配置任何上游那条）。
                GatewayError::config(format!(
                    concat!(
                        "provider `{}` 要走代理 `{}`，但 proxies 段里没有这个名字。\n",
                        "内置的名字只有 `direct` 和 `system`。"
                    ),
                    p.name, name
                ))
            })?;
            let url = proxy
                .url()
                .map_err(|e| GatewayError::config(format!("代理 `{name}` 的密码取不到：{e}")))?;
            match reqwest::Proxy::all(&url) {
                Ok(px) => b = b.proxy(px),
                Err(e) => {
                    // **默认让请求失败，不静默改走直连**（§3.7）。静默降级
                    // 最糟的情况不是失败，是它真的连上了，而你以为自己在
                    // 走代理。
                    if p.on_proxy_fail == tw_config::OnProxyFail::Direct {
                        tracing::warn!(
                            provider = %p.name, proxy = %name,
                            "代理配不起来，按 on_proxy_fail 降级直连：{e}"
                        );
                        b = b.no_proxy();
                    } else {
                        return Err(GatewayError::config(format!(
                            "provider `{}` 的代理 `{name}` 配不起来：{e}",
                            p.name
                        )));
                    }
                }
            }
        }
    }
    b.build()
        .map_err(|e| GatewayError::config(format!("HTTP 客户端建不起来：{e}")))
}

/// 目录的来源清单：每个 provider 声明了哪些模型、说什么协议。
///
/// **单独一个函数是为了让「上游名单变没变」有一个确切的判据**。
/// 改一条路由规则不该重置模型目录（那会让 `/v1/models` 短暂地空一下），
/// 而删掉一个 provider 必须立刻反映（列表即承诺，§3.9）。
fn catalog_sources(cfg: &tw_config::Config) -> Vec<tw_engine::ProviderModels> {
    cfg.providers
        .iter()
        .map(|p| tw_engine::ProviderModels {
            provider: p.name.clone(),
            protocol: p
                .effective_protocol()
                .map(|x| format!("{x:?}"))
                // 猜不出协议时按 Anthropic 算 —— 和转发时的默认一致
                // （forward::apply_credential）。两处不一致会让「列出来了
                // 但发过去 401」变成可能。
                .unwrap_or_else(|| "Anthropic".to_string()),
            models: p.models.clone(),
        })
        .collect()
}

fn catalog_from(cfg: &tw_config::Config) -> tw_engine::Catalog {
    tw_engine::Catalog::build(&catalog_sources(cfg))
}

/// 一次配置换入时**整块换掉**的那部分。
///
/// 分成「换的」和「不换的」两堆，判据是**这个东西丢了会不会让用户感觉
/// 到**：熔断状态丢了，一家刚被熔断的上游会立刻又被试一遍；并发闸门丢
/// 了，正在排队的请求会失去它们的位置；事件流丢了，界面上的实时列表会
/// 断一次。这些都不该因为改了一条路由规则而发生。
pub struct Runtime {
    pub config: Arc<tw_config::Config>,
    /// 路由引擎。**和配置一起建，一起换** —— 分开持有会让「规则改了但
    /// 引擎还是旧的」变成可能，而那种不一致完全静默。
    pub engine: Arc<tw_engine::Engine>,
    /// **每个 provider 一个 Client**。reqwest 的代理绑在 Client 上，
    /// 不能按请求切换（§3.7）—— 而这本来也是对的：连接池按上游隔离，
    /// 一个慢上游不会占着另一个的连接。
    pub clients: std::collections::HashMap<String, reqwest::Client>,
    /// 来源白名单。空 = 全放行，而那只在 loopback 下成立（§5.4）。
    pub allow: crate::access::AllowList,
    /// 工具调用防火墙的规则（§5.2）。
    ///
    /// **和配置一起建、一起换**，而不是每个请求现读一次文件 —— 那是几十
    /// 个正则的编译，摆在数据面上就是每个请求几毫秒的白付。
    pub rules: Arc<tw_scan::rules::Rules>,
}

impl Runtime {
    /// 建一份运行时。
    ///
    /// `previous` 在时**尽量复用上一份的 Client**。每次重载都重建所有
    /// Client，等于把每个上游的连接池连同已经握好的 TLS 一起扔掉 ——
    /// 改一条路由规则不该让下一个请求多付一次完整的建连。只有代理相关
    /// 的字段变了才必须重建，因为代理是绑在 Client 上的。
    pub fn build(
        config: tw_config::Config,
        previous: Option<&Runtime>,
    ) -> Result<Self, GatewayError> {
        let mut clients = std::collections::HashMap::new();
        for p in &config.providers {
            let reusable = previous.and_then(|prev| {
                let old = prev.config.providers.iter().find(|x| x.name == p.name)?;
                if proxy_shape(&prev.config, old) == proxy_shape(&config, p) {
                    prev.clients.get(&p.name)
                } else {
                    None
                }
            });
            match reusable {
                Some(c) => clients.insert(p.name.clone(), c.clone()),
                None => clients.insert(p.name.clone(), build_client(&config, p)?),
            };
        }
        let allow = crate::access::AllowList::parse(&config.listen.gateway.effective_allow_from())
            .map_err(|e| GatewayError::config(format!("listen.gateway.allow_from：{e}")))?;
        // 规则集编译一次，跟着运行时一起换 —— 它现在住在 config.yaml 的
        // `security.scan_rules` 里，所以「改了规则」和「改了别的配置」
        // 走同一条热重载路径（§3.1、§5.3）。
        //
        // **用户写坏的那几条被跳过，其余照常工作**：一个因为配置写错就
        // 整个不工作的安全功能等于没有。但跳过要大声说出来。
        let rules = tw_scan::rules::build(&config.security.scan_rules)
            .map_err(|e| GatewayError::config(e.to_string()))?;
        for w in &rules.warnings {
            tracing::warn!("{w}");
        }
        Ok(Self {
            engine: Arc::new(config.engine()),
            config: Arc::new(config),
            clients,
            allow,
            rules: Arc::new(rules),
        })
    }
}

/// 决定一个 Client 能不能复用的那几个字段。
///
/// base_url 和 key 都**不在**里面：Client 不绑 URL，凭据是每个请求现加
/// 的。把它们算进来只会让「改个 key」白白丢掉一整个连接池。
fn proxy_shape(cfg: &tw_config::Config, p: &tw_config::Provider) -> String {
    let px = cfg
        .proxies
        .iter()
        .find(|x| x.name == p.proxy)
        .map(|x| format!("{:?}|{}|{}", x.kind, x.addr, x.auth.is_some()))
        .unwrap_or_default();
    format!("{}|{:?}|{px}", p.proxy, p.on_proxy_fail)
}

#[derive(Clone)]
pub struct AppState {
    /// 配置换入时整块换掉的那部分（§3.8 第 ⑤ 步）。
    ///
    /// **一次 `store` 就是一次生效**：正在跑的请求持有旧的 `Arc`，跑完
    /// 自然释放；新请求看到的是新的。中间没有任何一个瞬间是半新半旧的。
    rt: Arc<arc_swap::ArcSwap<Runtime>>,
    /// 并发闸门。排队不拒绝（§4.7）。
    ///
    /// **不在 Runtime 里，因为它握着正在跑的请求的通行证。**跟着配置一起
    /// 换的话，每改一次规则，队列里排着的请求就会失去位置，而已经在跑的
    /// 那些的通行证会变成孤儿 —— 于是那一瞬间的实际并发可以到上限的两倍。
    /// 只有 `limits` 真的变了才换它。
    gate: Arc<arc_swap::ArcSwap<crate::limits::Gate>>,
    /// 探测和别的杂事用的默认 Client（不走代理）
    pub http: reqwest::Client,
    /// 观测事件往这里丢。没有订阅者时是零成本的 —— 数据面不该知道有
    /// 没有人在看。**跨重载存活**：界面上的实时列表不该因为改了配置断一次。
    pub bus: tw_observe::EventBus,
    /// 上游健康。**不持久化**，但**跨重载存活** —— 一家刚被熔断的上游
    /// 不该因为你改了条规则就立刻又被试一遍（§4.2）。
    pub health: Arc<Health>,
    /// 模型目录。**列表和准入的唯一真相来源**（§3.9）。
    ///
    /// 启动时用配置里的 `models:` 填一份，L2 探测回来后原子换入 ——
    /// 探测要打网络，不能挡住启动。
    pub catalog: Arc<arc_swap::ArcSwap<tw_engine::Catalog>>,
    /// 请求体和响应体往哪儿交（§8）。
    ///
    /// **有界通道，满了就丢。**直接调用意味着文件 I/O 跑在转发那条路上
    /// —— 一次慢磁盘写就变成一次慢请求，而观测永远不该有这个权力。
    /// `None` 表示观测层没起来，那时什么都不做。
    body_sink: Arc<std::sync::Mutex<Option<crate::bodies::BodySender>>>,
    /// 每个上游最近一次报的订阅额度（§4.3.2）。
    ///
    /// **在内存里，不落库。**它是「现在还剩多少」，不是历史 —— 存一份
    /// 五分钟前的百分比，价值几乎为零，而它会让「重启之后显示的是旧
    /// 数字」变成一个要解释的问题。下一个请求回来就有新的了。
    quotas: Arc<std::sync::Mutex<std::collections::HashMap<String, crate::quota::Quota>>>,
    /// 监听地址变了。**这是「温」那一级**（§3.8 的三级热重载）——
    /// 换端口不能只换配置：监听器是启动时建的，不重建的话新端口上什么
    /// 都没有，而旧端口还在服务。那种「改了没反应」比报错难查得多。
    relisten: Arc<tokio::sync::Notify>,
}

impl AppState {
    pub fn new(config: tw_config::Config) -> Result<Self, GatewayError> {
        let http = base_client_builder()
            .build()
            .map_err(|e| GatewayError::config(format!("HTTP 客户端建不起来：{e}")))?;
        let limits = config.limits.clone();
        let catalog = catalog_from(&config);
        let rt = Runtime::build(config, None)?;
        Ok(Self {
            rt: Arc::new(arc_swap::ArcSwap::from_pointee(rt)),
            gate: Arc::new(arc_swap::ArcSwap::from_pointee(crate::limits::Gate::new(
                limits,
            ))),
            http,
            bus: tw_observe::EventBus::new(),
            health: Arc::new(Health::new()),
            catalog: Arc::new(arc_swap::ArcSwap::from_pointee(catalog)),
            body_sink: Arc::new(std::sync::Mutex::new(None)),
            quotas: Arc::new(std::sync::Mutex::new(Default::default())),
            relisten: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// 当前这一份运行时。**每个请求只取一次**，从头到尾用同一份 ——
    /// 中途重新取会让一个请求跨在两份配置上。
    pub fn runtime(&self) -> Arc<Runtime> {
        self.rt.load_full()
    }

    pub fn config(&self) -> Arc<tw_config::Config> {
        self.rt.load().config.clone()
    }

    pub fn gate(&self) -> Arc<crate::limits::Gate> {
        self.gate.load_full()
    }

    /// 接上 body 的去处。**观测层起来之后才调** —— 在那之前 body 一律
    /// 丢掉，而请求照常。
    pub fn set_body_sink(&self, tx: crate::bodies::BodySender) {
        if let Ok(mut g) = self.body_sink.lock() {
            *g = Some(tx);
        }
    }

    fn body_sink(&self) -> Option<crate::bodies::BodySender> {
        self.body_sink.lock().ok().and_then(|g| g.clone())
    }

    /// 每个上游最近一次报的订阅额度。
    pub fn quotas(&self) -> std::collections::HashMap<String, crate::quota::Quota> {
        self.quotas.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// 换一份配置进去（§3.8 的第 ④⑤ 步）。
    ///
    /// **建不起来就什么都不换。**校验已经在 `tw_config::reload` 里做过
    /// 三遍了，但运行时对象仍然可能建不起来（比如代理地址 reqwest 不认），
    /// 而那时旧配置必须原样继续服务。
    pub fn reload(&self, config: tw_config::Config) -> Result<(), GatewayError> {
        let old = self.rt.load();
        let limits_changed = old.config.limits != config.limits;
        let new_limits = config.limits.clone();
        let catalog_stale = catalog_sources(&old.config) != catalog_sources(&config);
        let next = Runtime::build(config, Some(&old))?;
        if catalog_stale {
            // 上游名单变了，目录里那些属于已删上游的模型必须立刻消失 ——
            // 不然 `/v1/models` 会继续列出一个已经不存在的东西，而
            // 「列表即承诺」（§3.9）。真正的探测在后台补。
            self.catalog.store(Arc::new(catalog_from(&next.config)));
        }
        let relisten =
            old.config.listen.gateway.socket_addr() != next.config.listen.gateway.socket_addr();
        self.rt.store(Arc::new(next));
        if relisten {
            // 只通知，不在这里重建 —— 换监听器要 await，而这个函数被
            // 文件监听那条同步路径调用。谁在监听谁去换。
            self.relisten.notify_waiters();
        }
        if limits_changed {
            self.gate
                .store(Arc::new(crate::limits::Gate::new(new_limits)));
        }
        Ok(())
    }

    /// 密钥 → 客户端名字 + 方言。
    fn identify(
        &self,
        headers: &HeaderMap,
        query: Option<&str>,
    ) -> Result<(String, crate::auth::KeyPosition), GatewayError> {
        let Some((key, position)) = crate::auth::extract_key_with_position(headers, query) else {
            return Err(GatewayError::auth(
                "请求没带网关密钥。把 config.yaml 里 clients 段的那把 key 配到客户端上。",
            ));
        };
        self.rt
            .load()
            .config
            .clients
            .iter()
            .find(|c| key_eq(&c.key, &key))
            .map(|c| (c.name.clone(), position))
            .ok_or_else(|| {
                // 不回显收到的 key，哪怕是打码的 —— 回显会让「猜密钥」
                // 这件事有了反馈信号。
                GatewayError::auth(
                    "网关密钥不认识。检查客户端配置里的 key 和 config.yaml 是否一致。",
                )
            })
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        // **和准入共用同一个函数**（§3.9）—— 列表和准入不可能不一致。
        .route("/v1/models", get(list_models))
        // M0 只有透传：任何方法、任何路径都往上游送。M1 加路由时，
        // 这里会先过规则引擎再决定送给谁。
        .fallback(any(passthrough))
        .with_state(state)
}

/// `GET /v1/models`。
///
/// 三种方言的响应结构不同，但**列表内容来自同一个函数** —— 差别只在
/// 外壳（§3.9）。
async fn list_models(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Result<Response, GatewayError> {
    let rt = state.runtime();
    if !rt.allow.allows(peer.ip()) {
        return Err(GatewayError::auth(format!(
            "{} 不在允许的来源里。",
            peer.ip()
        )));
    }
    let (client, position) = state.identify(&headers, query.as_deref())?;
    let allow = rt
        .config
        .clients
        .iter()
        .find(|c| c.name == client)
        .and_then(|c| c.allow.clone());
    let models = state
        .catalog
        .load()
        .resolve_allowed(Some(position.dialect()), allow.as_deref());

    let now = now_ms() / 1000;
    let body = match position {
        crate::auth::KeyPosition::GoogleHeader => serde_json::json!({
            "models": models.iter().map(|m| serde_json::json!({
                "name": format!("models/{m}"),
            })).collect::<Vec<_>>()
        }),
        // Anthropic 和 OpenAI 的 /v1/models 形状一样
        _ => serde_json::json!({
            "object": "list",
            "data": models.iter().map(|m| serde_json::json!({
                "id": m, "object": "model", "created": now,
            })).collect::<Vec<_>>()
        }),
    };
    Ok(axum::Json(body).into_response())
}

async fn passthrough(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    OriginalUri(uri): OriginalUri,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, GatewayError> {
    let started = std::time::Instant::now();
    // **整个请求只取一次运行时。**中途重新取会让一个请求跨在两份配置
    // 上：按新规则选了 provider，却拿旧的 Client 去发 —— 而那种不一致
    // 完全静默。
    let rt = state.runtime();
    // 来源检查在身份检查**之前**：一个不该连过来的地址，不该有机会
    // 试密钥（§5.4）。
    if !rt.allow.allows(peer.ip()) {
        return Err(GatewayError::new(
            crate::error::Source::Auth,
            format!(
                "{} 不在允许的来源里。改 listen.gateway.allow_from，或者把 bind 改回 loopback。",
                peer.ip()
            ),
        ));
    }
    let (client_name, position) = state.identify(&headers, query.as_deref())?;
    // 从这里往下，所有错误都要用客户端自己那套结构回（§4.6.1）。
    // **认证失败在这一行之前，那时方言还猜不出来** —— key 就是没认出来
    // 的，只能退回 Anthropic 形状，而那是桌面版的主用例。
    let dialect = crate::error::Dialect::from_key_position(position);
    // **在一个地方给方言，而不是在每个 return 点。**后者只要漏一处，
    // 那条路径上的客户端就会收到一个它解析不了的 body，而那个失败看
    // 起来和真实原因毫无关系。
    pipeline(
        state,
        rt,
        uri,
        query,
        headers,
        body,
        client_name,
        position,
        started,
    )
    .await
    .map_err(|e| e.in_dialect(dialect))
}

#[allow(clippy::too_many_arguments)]
async fn pipeline(
    state: AppState,
    rt: Arc<Runtime>,
    uri: axum::http::Uri,
    query: Option<String>,
    headers: HeaderMap,
    body: Bytes,
    client_name: String,
    position: crate::auth::KeyPosition,
    started: std::time::Instant,
) -> Result<Response, GatewayError> {
    forward::check_body_size(&body, MAX_BODY)?;

    // 管线第 1.3 步：客户端的自言自语（§4.8）。
    //
    // **位置在身份识别之后、模型准入和路由之前。**在准入之前不是偷懒：
    // 一个被本地应答的请求永远不会到达任何上游，而模型准入回答的是
    // 「哪些上游可以为你服务」，对它无从谈起。
    //
    // 更要紧的是**离线时也要能应答** —— sub2api 把判定放在选号之后，
    // 于是断网时健康检查照样失败，白白丢掉这个功能最有价值的场景。
    // 同样的理由让它排在「一个 provider 都没有」那一条之前：那一条也是
    // 一种「没有可用上游」，而本地应答本来就不需要上游。
    let mut intent = String::new();
    if let Some(kind) = crate::clientprobe::classify(&body, is_claude_code(&client_name, &headers))
    {
        use tw_config::ProbeAction::*;
        match kind.action(&rt.config.client_probes) {
            Intercept => {
                let id = state.bus.next_id();
                state.bus.emit(tw_api::Event::LocallyAnswered {
                    id,
                    client: client_name.clone(),
                    probe: kind.label().to_string(),
                    at_ms: now_ms(),
                });
                tracing::debug!(client = %client_name, kind = kind.label(), "本地应答");
                return Ok(local_answer(kind, &body));
            }
            // `route` 交给规则处理：打一个标记让 `when: { intent: ... }`
            // 能匹配到，然后照常往下走。
            Route => intent = kind.slug().to_string(),
            // **`passthrough` 不打标记。**打了的话，一条
            // `when: { intent: assistant_internal }` 的规则会在用户还
            // 没把那类请求配成 route 的时候就开始生效 —— 而配置文件里
            // 看不出任何线索。
            Passthrough => {}
        }
    }

    // 首次运行还没配完是正常状态，不是配置错误。这条要在路由之前挡，
    // 因为「一个 provider 都没有」时任何路由结果都是空的，而那条错误
    // 说不清下一步。
    if rt.config.providers.is_empty() {
        return Err(GatewayError::config(concat!(
            "还没有配置任何上游。打开 ThinkWatch Lite 添加第一个 provider，",
            "或者往 config.yaml 的 providers 段里写一个。"
        )));
    }

    // 管线第 2 步：路由（§4）。**规则引擎在这里** —— M0 那句「取第一个
    // provider」就是留给这一段的接缝。
    // **只解析一次。**路由要它，会话指纹也要它（§7.9），而 body 可能有
    // 几百 KB —— 解两遍是白付一份钱。
    let parsed = serde_json::from_slice::<serde_json::Value>(&body).ok();
    let facts = {
        let mut f = match &parsed {
            Some(v) => tw_engine::RequestFacts::from_anthropic_body(v),
            // body 解不开时用空的性质走兜底规则。**不要因此拒绝请求** ——
            // 我们的解析器不认识的东西，上游可能完全认识（§4.1）。
            None => tw_engine::RequestFacts::default(),
        };
        f.client = client_name.clone();
        f.intent = intent;
        f
    };
    // 管线第 1.5 步：模型准入。**和 `GET /v1/models` 共用同一个函数**
    // （§3.9）—— 列出来的一定能用，能用的一定列了出来。
    //
    // 目录空着时不拦：那说明探测还没回来或者上游都不给列表，这时候拦
    // 等于把整个网关关掉。
    {
        let catalog = state.catalog.load();
        if !catalog.is_empty() && !facts.model.is_empty() {
            let allow = rt
                .config
                .clients
                .iter()
                .find(|c| c.name == client_name)
                .and_then(|c| c.allow.clone());
            if !catalog.admits(&facts.model, Some(position.dialect()), allow.as_deref()) {
                // 错误信息要说人话 —— 而不是一个干巴巴的 permission
                // denied（§3.9、§4.6.1）。
                return Err(GatewayError::new(
                    crate::error::Source::Request,
                    format!(
                        "客户端 `{client_name}` 不允许使用 {}。它能用的模型见 GET /v1/models。",
                        facts.model
                    ),
                ));
            }
        }
    }

    let decision = match rt
        .engine
        .route(&facts)
        .map_err(|e| GatewayError::config(format!("路由失败：{e}")))?
    {
        tw_engine::Outcome::Route(d) => d,
        tw_engine::Outcome::Deny { rule, reason } => {
            // **带理由的拒绝。**一个没有理由的拒绝，和一个 bug，在用户
            // 眼里没有区别（§3.4）。
            tracing::info!(%rule, "按规则拒绝");
            return Err(GatewayError::denied(reason));
        }
    };
    // 管线第 3 步：准入。**排队而不是拒绝**（§4.7）—— 客户端收到 429
    // 通常不会优雅重试，一个本来只需要多等两秒的请求会变成一次任务中断。
    //
    // 闸门在路由**之后**取：要知道走哪个 provider 才能算 per_provider
    // 那一维。
    let client_limit = rt
        .config
        .clients
        .iter()
        .find(|c| c.name == client_name)
        .and_then(|c| c.max_concurrent);
    let _pass = state
        .gate()
        .acquire(
            decision
                .candidates
                .first()
                .map(|s| s.as_str())
                .unwrap_or("?"),
            &client_name,
            client_limit,
        )
        .await
        .map_err(|e| GatewayError::new(crate::error::Source::Overloaded, e.to_string()))?;

    // 熔断过滤。**只有一个候选时完全旁路**，全都熔断时 fail-open ——
    // 两条边界都在 `Health::filter` 里，理由写在那儿。
    let (alive, fail_open) = state.health.filter(&decision.candidates);
    if fail_open {
        tracing::warn!(
            candidates = ?decision.candidates,
            "全部候选都在熔断中，仍然照常尝试（fail-open）"
        );
    }

    let id = state.bus.next_id();
    state.bus.emit(tw_api::Event::RequestStarted {
        id,
        client: client_name.clone(),
        // **旁证，不是身份。**只用来显示和判断「接管生效了吗」，
        // 不参与鉴权、路由、配额（见 crate::hint）。
        client_hint: crate::hint::client_hint(&headers),
        // 认出「这几十个请求是同一次任务」（§7.9）。**认不出来就是
        // None** —— 硬凑一个会把互不相干的请求并成一个「会话」
        session_fp: parsed.as_ref().and_then(crate::session::fingerprint),
        provider: alive.first().map(|s| s.as_str()).unwrap_or("?").to_string(),
        model: facts.model.clone(),
        method: "POST".to_string(),
        path: uri.path().to_string(),
        at_ms: now_ms(),
    });

    // 出站密钥检测（§5.0 的观察态）。**只看，不动** —— 换成占位符是
    // 「拦截」态的事，而那要等 §5.1 那套完整的脱敏。
    //
    // 位置在这里是因为它要知道**发给了谁**：一把 key 发给官方和发给一个
    // 中转站，是完全不同的两件事，而后者才是这条防线存在的理由。
    if rt.config.security.redact.detects() {
        for f in crate::leak::scan(&body) {
            state.bus.emit(tw_api::Event::LeakSeen {
                id,
                provider: alive.first().map(|s| s.to_string()).unwrap_or_default(),
                secret: f.kind,
                masked: f.masked,
                at_ms: now_ms(),
            });
        }
    }

    // 请求体交给观测层。**这时候它已经完整在内存里了**，所以这一步
    // 除了一次 `Bytes` 的引用计数之外没有别的成本（§4.1 说过入站是要
    // 整个解析的，所以本来就在）。
    let sink = state.body_sink();
    let at_ms = now_ms() as i64;
    crate::bodies::offer(
        &sink,
        crate::bodies::BodyRecord {
            id,
            at_ms,
            kind: crate::bodies::BodyKind::Request,
            body: body.clone(),
            original_len: body.len(),
        },
    );

    // 依次尝试。**首字节之前可以透明切换**（§4.2）—— 拿到响应头之前
    // 我们还没往客户端写过任何东西，换一家客户端完全无感。
    //
    // 「尝试链」要留下来：用户能看见故障转移在替他工作，**这是信任的
    // 来源**。一个静默切换过的请求和一个一次就成的请求，在用户眼里
    // 应该是不同的。
    let mut attempts: Vec<String> = Vec::new();
    // 成功那一次的脱敏账本。**必须是成功那一次的** —— 故障转移从官方切到
    // 中转时，两次的脱敏规格不一样，拿错一本就还原不回来（§5.1）
    let mut used_ledger = tw_redact::redact::Ledger::default();
    // 每一跳的结果和耗时。**失败的原因要留着** —— 一条说「试过 A → B →
    // C」的链，和一条还说清每一跳为什么失败的链，排查价值差得远。
    let mut chain: Vec<tw_api::AttemptView> = Vec::new();
    #[allow(unused_assignments)]
    let mut hop_started = std::time::Instant::now();
    let mut last_err: Option<GatewayError> = None;
    let mut upstream = None;
    let mut used: Option<&tw_config::Provider> = None;

    for name in &alive {
        let Some(provider) = rt.config.providers.iter().find(|p| &p.name == *name) else {
            // 校验时挡过一次，能到这儿说明配置在运行中被换过。
            last_err = Some(GatewayError::config(format!(
                "规则 `{}` 选中了 `{name}`，但配置里没有这个 provider",
                decision.matched_rule
            )));
            continue;
        };
        attempts.push(provider.name.clone());
        hop_started = std::time::Instant::now();

        // 阶段二：知道走哪家了，再跑一遍含 `provider_would_be` 的规则。
        //
        // **在循环里面，因为故障转移换了 provider 之后必须重算**（§3.4）。
        // 否则「走中转的一律脱敏」这条规则，在从官方转移到中转时会漏掉
        // —— 而那正是最需要它的时刻。
        let (effective_set, guard) =
            match rt
                .engine
                .phase_two(&facts, &provider.name, &decision.set, &decision.guard)
            {
                Ok(tw_engine::Outcome2::Proceed(s, g)) => (s, g),
                Ok(tw_engine::Outcome2::Deny { rule, reason }) => {
                    tracing::info!(%rule, provider = %provider.name, "阶段二拒绝");
                    return Err(GatewayError::denied(reason));
                }
                Err(e) => return Err(GatewayError::config(format!("阶段二求值失败：{e}"))),
            };
        // 参数改写。**只在这里动 body，而且只动被点名的那几个字段** ——
        // §4.1 的出站直通说过任何 body 改写都可能是缓存杀手，所以这是
        // 一个用户显式要求的例外，不是默认行为。
        let outbound = forward::apply_set(&body, &effective_set);

        // 出站脱敏（§5.1）。**和阶段二在同一个位置，理由完全一样** ——
        // 故障转移从官方切到中转的那一刻，正是最需要它的时刻，而那时
        // 「该脱哪些」已经换了一套。
        let (outbound, ledger) =
            crate::guard::redact_outbound(rt.config.security.redact, provider, &guard, outbound);
        if !ledger.is_empty() {
            state.bus.emit(tw_api::Event::Redacted {
                id,
                provider: provider.name.clone(),
                items: ledger
                    .counts
                    .iter()
                    .map(|(k, what, n)| tw_api::RedactedItem {
                        kind: k.slug().to_string(),
                        what: what.to_string(),
                        count: *n as u64,
                    })
                    .collect(),
                at_ms: now_ms(),
            });
        }

        let key = match provider.resolved_key() {
            Ok(k) => k,
            Err(e) => {
                // 密钥取不到是这一家的问题（可能是 exec 命令挂了），
                // 换下一家是合理的。
                state.health.record_failure(&provider.name);
                chain.push(hop(&provider.name, format!("密钥取不到：{e}"), hop_started));
                last_err = Some(GatewayError::config(format!(
                    "provider `{}` 的密钥取不到：{e}",
                    provider.name
                )));
                continue;
            }
        };
        let url = forward::upstream_url(&provider.base_url, uri.path(), query.as_deref());
        let method = reqwest::Method::from_bytes(b"POST").expect("POST 是合法方法");

        tracing::debug!(
            client = %client_name,
            provider = %provider.name,
            rule = %decision.matched_rule,
            group = ?decision.via_group,
            url = %tw_secret::redact_url(&url),
            attempt = attempts.len(),
            "转发"
        );

        // 用这个 provider 自己的 Client —— 它带着该走的代理。
        let http = rt.clients.get(&provider.name).unwrap_or(&state.http);
        let mut req = http.request(method, &url);
        req = forward::forward_headers(req, &headers);
        req = forward::apply_credential(req, provider.effective_protocol(), &key);
        match req.body(outbound.clone()).send().await {
            Ok(r) if r.status().is_server_error() || r.status() == 429 => {
                // 5xx 和限流：换一家有意义，那边可能有不同的额度或地域。
                // **4xx 不换**（除了 429）—— 请求本身有问题的话，换一家
                // 也一样被拒，还会白白污染那家的健康度。
                state.health.record_failure(&provider.name);
                chain.push(hop(&provider.name, format!("{}", r.status()), hop_started));
                // **429 要保住 429。**塌成 502 的话，客户端会当成「服务器
                // 坏了」而不是「该退避了」，而它们该做的事完全不同
                // （§4.6.1）。
                last_err = Some(if r.status() == 429 {
                    GatewayError::rate_limited(format!("`{}` 限流了", provider.name))
                } else {
                    GatewayError::upstream(format!("`{}` 返回 {}", provider.name, r.status()))
                });
                continue;
            }
            Ok(r) => {
                state.health.record_success(&provider.name);
                chain.push(hop(&provider.name, "成功".to_string(), hop_started));
                upstream = Some(r);
                used = Some(provider);
                used_ledger = ledger;
                break;
            }
            Err(e) => {
                state.health.record_failure(&provider.name);
                let err = forward::map_reqwest_error(e);
                chain.push(hop(&provider.name, err.message.clone(), hop_started));
                last_err = Some(err);
                continue;
            }
        }
    }

    // 尝试链走完了，两条路都要发 —— 挂在 RequestFinished 上的话，
    // 失败那条路就没有尝试链，而那恰恰是最需要看它的时候。
    // 最终服务的那家怎么收钱。**跟着请求走，不能事后查配置** ——
    // 配置随时会被热重载，而一条三天前的记录该按它当时那家的算（§4.3.1）。
    let billing = used
        .map(|p| effective_billing(&state, p))
        .unwrap_or(tw_config::Billing::PerToken);
    state.bus.emit(tw_api::Event::RequestRouted {
        id,
        rule: decision.matched_rule.clone(),
        group: decision.via_group.clone(),
        attempts: chain,
        billing: billing.slug().to_string(),
    });

    let (Some(upstream), Some(provider)) = (upstream, used) else {
        let mut err = last_err.unwrap_or_else(|| GatewayError::config("没有可用的上游"));
        // **尝试链要进客户端看到的那条错误**，不只进我们的事件流 ——
        // 用户看的是他自己终端里的报错。一条说「试过 A → B → C 都不行」
        // 的错误，和一条只说「503」的错误，是两种产品：前者说明我们替
        // 他做了工作，后者让他以为我们什么都没干（§4.2）。
        if attempts.len() > 1 {
            err.message = format!("{}（试过：{}）", err.message, attempts.join(" → "));
        }
        // 失败也必须发事件。少了它，UI 上那一行会永远停在「进行中」——
        // 而「一直转圈」比「明确失败」更让人怀疑是我们卡住了。
        state.bus.emit(tw_api::Event::RequestFailed {
            id,
            source: "upstream".to_string(),
            message: err.message.clone(),
        });
        return Err(err);
    };
    if attempts.len() > 1 {
        tracing::info!(
            chain = %attempts.join(" → "),
            "故障转移：最终由 {} 服务",
            provider.name
        );
    }

    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    // 响应头到手就发一次。**这个事件单独存在是有意的**：流式请求从这里
    // 到结束可能还有好几分钟，UI 要能在这个点就把行画出来并标「进行中」，
    // 而不是等它结束才出现。
    state.bus.emit(tw_api::Event::RequestHeaders {
        id,
        status: status.as_u16(),
        ttfb_ms: started.elapsed().as_millis() as u64,
    });
    // 订阅额度（§4.3.2）。**零成本** —— 这些头本来就在响应里，读一下
    // 就有了。按量付费的账号没有它们，那时什么都不发。
    let quota = crate::quota::from_headers_reqwest(upstream.headers());
    if !quota.is_empty() {
        if let Ok(mut g) = state.quotas.lock() {
            g.insert(provider.name.clone(), quota.clone());
        }
        state.bus.emit(tw_api::Event::QuotaSeen {
            id,
            provider: provider.name.clone(),
            windows: quota
                .windows
                .iter()
                .map(|w| tw_api::QuotaWindow {
                    label: w.label.clone(),
                    used_percent: w.used_percent,
                    reset_in_secs: w.reset_in_secs,
                    status: w.status.clone(),
                })
                .collect(),
            at_ms: now_ms(),
        });
    }

    let mut out_headers = forward::response_headers(upstream.headers());
    // **哪一家服务的，写在头上。**§4.6.1 说上游的错误要原样透传、不加
    // `[ThinkWatch]` 前缀 —— 那确实是它说的话。可上游的 401 说的是
    // 「invalid x-api-key」，而用户手里有两把 key（网关的和上游的），
    // 他会去查错的那一把。改 body 是越界，加一个头不是。
    if let Ok(v) = axum::http::HeaderValue::from_str(&provider.name) {
        out_headers.insert("x-thinkwatch-upstream", v);
    }

    // 流式：**不缓冲**。整块缓冲会把 SSE 变成一次性交付，客户端那边
    // 看起来就是「卡住很久然后一下全出来」。
    let bus = state.bus.clone();
    let bytes_seen = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counted = {
        let bytes_seen = bytes_seen.clone();
        upstream.bytes_stream().map_ok(move |chunk| {
            bytes_seen.fetch_add(chunk.len() as u64, std::sync::atomic::Ordering::Relaxed);
            chunk
        })
    };
    // 流结束时才知道总字节数和真实耗时 —— 对一个跑了六分钟的任务，
    // 这两个数字在响应头那一刻都还不存在。
    //
    // **中途断掉不能只是让流消失。**首字节已经发出去了，状态码和响应头
    // 都改不了，而一个戛然而止的 SSE 流和一个正常结束的流在客户端看来
    // 长得一模一样 —— 用户会以为模型就答了这么多。唯一还能说话的地方
    // 是流本身，所以补一个 `event: error` 帧（§4.6.1）。
    let is_sse = out_headers
        .get(axum::http::header::CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"text/event-stream"));
    let dialect = crate::error::Dialect::from_key_position(position);
    // 回显还原（§5.1）。
    //
    // **SSE 和非流式走两套**：前者的占位符散落在几十帧里（模型按 token
    // 吐字，一个 `<<TW_SECRET_1>>` 会被切成五到八段），后者整个躺在一份
    // JSON 里。
    //
    // **没脱敏过就是个空壳**，`process` 直接把字节原样递出去 —— 绝大多数
    // 请求走的是这条路，它不该为这个功能付任何延迟。
    let mut restorer = tw_redact::sse::Body::new(&used_ledger, is_sse);
    // 工具调用防火墙（§5.2）。**只在 SSE 上跑** —— 非流式响应整个到手
    // 之后再拦已经没有意义，客户端下一步就拿到全文了。
    let inspect = rt.config.security.inspect_tools;
    let trust = crate::guard::effective_trust(provider, &decision.guard);
    // 正文里的提示注入**只对不受信任的上游查**（§5.2 末尾）：官方端点上
    // 模型讲解提示注入是完全正常的
    let mut wall = (is_sse && inspect.detects())
        .then(|| crate::toolwall::Wall::new(rt.rules.clone(), trust.blocks()));
    let wall_provider = provider.name.clone();
    let stream = async_stream::stream! {
        let mut counted = std::pin::pin!(counted);
        let mut broke: Option<GatewayError> = None;
        // **旁路嗅探，不缓冲**（§4.3）：字节照常流向客户端，同时喂它
        // 一份。上游返回的 usage 是真相，而拿不到它就只能估。
        let mut sniffer = crate::usage::Sniffer::new();
        // 响应体也攒一份，**攒到上限就停**。和 usage 嗅探走同一个循环 ——
        // 两个各自遍历一遍是白白多走一趟。
        let mut tap = crate::bodies::ResponseTap::new();
        while let Some(item) = counted.next().await {
            match item {
                Ok(chunk) => {
                    // **嗅探和留档看的是上游原话**（带占位符的那一版）：
                    // usage 数字不受影响，而请求详情里存的正是「我们发出去
                    // 的和收回来的」，把还原后的存进去会让那一页说谎。
                    sniffer.feed(&chunk);
                    tap.feed(&chunk);
                    let out = restorer.process(&chunk);
                    // **审查的是客户端将要看到的那一版**（还原之后的），
                    // 因为那才是它真正会去执行的东西
                    let mut cut: Option<(GatewayError, usize)> = None;
                    if let Some(w) = wall.as_mut() {
                        for v in w.feed(&out) {
                            // 高危 + 不受信任 + 拦截态 = 切断（§5.2）
                            let blocked = v.high && inspect.acts() && trust.blocks();
                            bus.emit(tw_api::Event::ToolCallFlagged {
                                id,
                                provider: wall_provider.clone(),
                                tool: v.tool.clone(),
                                rule: v.rule.clone(),
                                why: v.why.clone(),
                                excerpt: v.excerpt.clone(),
                                high: v.high,
                                blocked,
                                at_ms: now_ms(),
                            });
                            if blocked {
                                tracing::warn!(
                                    provider = %wall_provider, tool = %v.tool, rule = %v.rule,
                                    "切断响应流：上游返回了一个高危工具调用"
                                );
                                cut = Some((
                                    GatewayError::denied(format!(
                                        "`{}` 返回的 `{}` 调用命中「{}」（{}），已切断。这个上游标记为不受信任。",
                                        wall_provider, v.tool, v.rule, v.why
                                    )),
                                    v.safe_prefix,
                                ));
                                break;
                            }
                        }
                    }
                    if let Some((err, safe)) = cut {
                        // **命中那一帧之前的内容照常发。**模型在动手之前
                        // 通常先说了几句正常的话，一起吞掉的话用户看到的
                        // 是「什么都没发生然后报错了」。而从那一帧起一个
                        // 字节都不发 —— 「尽力阻断」的要点是客户端拼不出
                        // 完整的工具调用（§5.2）
                        let safe = safe.min(out.len());
                        if safe > 0 {
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(out[..safe].to_vec()));
                        }
                        broke = Some(err.in_dialect(dialect));
                        break;
                    }
                    if !out.is_empty() {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(out));
                    }
                }
                Err(e) => {
                    broke = Some(forward::map_reqwest_error(e).in_dialect(dialect));
                    break;
                }
            }
        }
        // 扣住的尾巴要吐出来，**在结束事件之前** —— 否则最后几个字节
        // 会掉在流的外面
        let tail = restorer.flush();
        if !tail.is_empty() {
            yield Ok::<Bytes, std::io::Error>(Bytes::from(tail));
        }
        // 这条响应长什么样（§5.2 防线三）。**只有形状，没有内容。**
        if let Some(w) = wall.as_ref() {
            let (tool_calls, flagged) = w.shape();
            if tool_calls > 0 || flagged > 0 {
                bus.emit(tw_api::Event::ResponseInspected { id, tool_calls, flagged });
            }
        }
        let (recorded, original_len) = tap.finish();
        if !recorded.is_empty() {
            crate::bodies::offer(
                &sink,
                crate::bodies::BodyRecord {
                    id,
                    at_ms,
                    kind: crate::bodies::BodyKind::Response,
                    body: recorded,
                    original_len,
                },
            );
        }
        match broke {
            None => bus.emit(tw_api::Event::RequestFinished {
                id,
                status: status.as_u16(),
                bytes: bytes_seen.load(std::sync::atomic::Ordering::Relaxed),
                duration_ms: started.elapsed().as_millis() as u64,
                usage: sniffer.finish().map(|u| tw_api::UsageView {
                    input: u.input,
                    output: u.output,
                    cache_read: u.cache_read,
                    cache_write: u.cache_write,
                    cache_1h: u.cache_1h,
                }),
            }),
            Some(err) => {
                // 少了这个事件，UI 上那一行会永远停在「进行中」——
                // 而「一直转圈」比「明确失败」更让人怀疑是我们卡住了。
                bus.emit(tw_api::Event::RequestFailed {
                    id,
                    source: "upstream".to_string(),
                    message: format!("流中断：{}", err.message),
                });
                if is_sse {
                    yield Ok(Bytes::from(err.sse_frame()));
                }
            }
        }
    };
    let mut resp = Response::new(Body::from_stream(stream));
    *resp.status_mut() = status;
    *resp.headers_mut() = out_headers;
    Ok(resp)
}

/// 去问每个上游有哪些模型，把目录换掉。
///
/// **探测是零成本的**（§4.6 的 L2），但它要打网络，所以在后台跑而不是
/// 挡住启动。配置里手写的 `models:` 是它回来之前的兜底。
///
/// 结果缓存 24 小时（§3.9）——模型列表变化不频繁，而**每次有人调
/// `/v1/models` 就去打上游，会把一个本该零成本的端点变成一次串行网络
/// 往返**。
pub async fn refresh_catalog(state: &AppState) {
    // 探测要打网络，一轮下来可能几秒。**整轮用同一份运行时** —— 中途
    // 换了配置的话，这一轮探的是旧名单，而下面换入前会再确认一次。
    let rt = state.runtime();
    let mut sources = Vec::new();
    for p in rt.config.providers.iter() {
        let protocol = p
            .effective_protocol()
            .map(|x| format!("{x:?}"))
            .unwrap_or_else(|| "Anthropic".to_string());
        let key = match p.resolved_key() {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(provider = %p.name, "密钥取不到，跳过探测：{e}");
                sources.push(tw_engine::ProviderModels {
                    provider: p.name.clone(),
                    protocol,
                    models: p.models.clone(),
                });
                continue;
            }
        };
        let http = rt.clients.get(&p.name).unwrap_or(&state.http);
        let r = crate::probe::probe(http, &p.base_url, &key, p.effective_protocol()).await;
        let discovered = match &r.models {
            crate::probe::ModelList::Listed { models } => models.clone(),
            // 探不到就用手写的兜底。**两者不合并** —— 合并的话，用户
            // 删掉一个上游不再提供的模型时会发现它删不掉。
            other => {
                if !p.models.is_empty() {
                    tracing::debug!(provider = %p.name, ?other, "用配置里手写的模型清单");
                } else {
                    tracing::info!(
                        provider = %p.name, ?other,
                        "这家没给模型列表，也没写 models: 兜底 —— 它的模型不会出现在 /v1/models 里"
                    );
                }
                p.models.clone()
            }
        };
        sources.push(tw_engine::ProviderModels {
            provider: p.name.clone(),
            protocol,
            models: discovered,
        });
    }
    let total: usize = sources.iter().map(|s| s.models.len()).sum();
    state
        .catalog
        .store(Arc::new(tw_engine::Catalog::build(&sources)));
    tracing::info!(providers = sources.len(), models = total, "模型目录已刷新");
}

/// 后台刷新循环。
pub fn spawn_catalog_refresh(state: AppState) {
    tokio::spawn(async move {
        loop {
            refresh_catalog(&state).await;
            tokio::time::sleep(std::time::Duration::from_secs(24 * 3600)).await;
        }
    });
}

/// 起服务。返回实际绑定的地址 —— 端口写 0 时调用方需要知道拿到了哪个。
pub async fn serve(state: AppState, addr: std::net::SocketAddr) -> std::io::Result<()> {
    serve_once(state, addr, std::future::pending()).await
}

/// 起服务，**并且跟着配置里的监听地址走**（§3.8 的「温」）。
///
/// 换端口时：新监听器先起来，旧的停止接受新连接并**等现有请求自然
/// 结束** —— 一个跑了六分钟的流不该因为你改了个端口而断掉。
///
/// 命令行给了 `--port` 时不要用这个：那是一个显式的覆盖，不该被配置
/// 文件推翻。
pub async fn serve_following_config(
    state: AppState,
    addr: std::net::SocketAddr,
) -> std::io::Result<()> {
    let mut next = addr;
    loop {
        let relisten = state.relisten.clone();
        // **先订阅再进循环。**`notified()` 要在可能发生通知之前建好，
        // 否则重建监听器那几毫秒里来的通知会丢，于是端口改了两次只生效
        // 一次 —— 而那种「有时候生效有时候不」最难查。
        let wait = async move {
            relisten.notified().await;
        };
        serve_once(state.clone(), next, wait).await?;
        let want = state.runtime().config.listen.gateway.socket_addr();
        if want == next {
            // 通知来了但地址没变（比如又改回去了）—— 原样重来
            continue;
        }
        tracing::info!(from = %next, to = %want, "监听地址变了，重建监听器");
        next = want;
    }
}

/// 一次监听。`until` 完成时优雅停止：不再接受新连接，已经在跑的请求
/// 自己跑完。
async fn serve_once(
    state: AppState,
    addr: std::net::SocketAddr,
    until: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let actual = listener.local_addr()?;
    if !state.runtime().allow.is_empty() {
        tracing::info!(%actual, "网关已监听（有来源白名单）");
    } else {
        tracing::info!(%actual, "网关已监听");
    }
    // `into_make_service_with_connect_info` 是拿到对端地址的唯一办法 ——
    // 少了它，来源白名单收到的永远是 unwrap 出来的默认值。
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(until)
    .await
}

/// 这家实际怎么收钱。
///
/// **配置里写了就听配置的，没写就自动判**：响应头里报过订阅额度的就是
/// 订阅型（§4.3.2）。那个信号一直在我们手上，不该变成一个用户要填的
/// 字段（§0.6）—— 而一个填错了的字段比没有更糟。
///
/// **自动判有一个已知的边界：每次进程启动之后，打给一家订阅上游的第一个
/// 请求会被按量计价。**那时我们还没见过它的额度头。之后就对了。
///
/// 没有更好的办法：额度头只在响应里，而计价发生在响应之后 —— 想在第一
/// 个请求之前知道，只能主动探测，而 §4.3.2 明确否掉了那条路（会占用户
/// 自己的配额）。在乎那一条记录的人，在配置里写一行 `billing:
/// subscription` 就没有歧义了。
fn effective_billing(state: &AppState, p: &tw_config::Provider) -> tw_config::Billing {
    if let Some(b) = p.billing {
        return b;
    }
    if state.quotas().get(&p.name).is_some_and(|q| !q.is_empty()) {
        tw_config::Billing::Subscription
    } else {
        tw_config::Billing::PerToken
    }
}

fn hop(provider: &str, outcome: String, started: std::time::Instant) -> tw_api::AttemptView {
    tw_api::AttemptView {
        provider: provider.to_string(),
        outcome,
        ms: started.elapsed().as_millis() as u64,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 这个请求是 Claude Code 发的吗。
///
/// **`max_tokens: 1` 那条判定必须同时要求它**（§4.8），否则会误伤别人
/// 真实的 `max_tokens: 1` 请求 —— 而误判的代价是用户看到一个凭空出现的
/// 假答案，且完全无从察觉。
///
/// 两个信号取或：配置里那个客户端叫什么（`twcore init` 生成的名字就是
/// `claude-code`），以及 UA。**都不是铁证**，所以这里只做「更窄」用：
/// 认不出来就不拦，那是正确的失败方向。
fn is_claude_code(client_name: &str, headers: &HeaderMap) -> bool {
    if client_name == "claude-code" {
        return true;
    }
    headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ua| ua.to_ascii_lowercase().starts_with("claude-cli/"))
}

/// 伪造一个应答。形状跟着请求走 —— 客户端按自己请求的形状去解析，
/// 回错了形状比不拦截更糟。
fn local_answer(kind: crate::clientprobe::ProbeKind, body: &Bytes) -> Response {
    if crate::clientprobe::wants_stream(body) {
        return (
            [
                (axum::http::header::CONTENT_TYPE, "text/event-stream"),
                (axum::http::header::CACHE_CONTROL, "no-cache"),
                // 让本地应答在响应里也是可见的。**对客户端逼真，对用户
                // 透明** —— 这两件事不矛盾，因为看这个头的是人。
                (
                    axum::http::HeaderName::from_static("x-thinkwatch-local"),
                    "1",
                ),
            ],
            crate::clientprobe::sse_response(kind, body),
        )
            .into_response();
    }
    (
        [(
            axum::http::HeaderName::from_static("x-thinkwatch-local"),
            "1",
        )],
        axum::Json(crate::clientprobe::json_response(kind, body)),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tw_config::{Client, Config, Listen, Provider};

    fn cfg() -> Config {
        Config {
            version: 1,
            listen: Listen::default(),
            clients: vec![Client {
                name: "default".into(),
                key: "tw-good".into(),
                ..Default::default()
            }],
            providers: vec![Provider {
                name: "r".into(),
                base_url: "https://example.invalid".into(),
                key: "sk-1".into(),
                protocol: None,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn hdr(k: &'static str, v: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert(k, axum::http::HeaderValue::from_str(v).unwrap());
        m
    }

    fn provider_with_proxy(proxy: &str) -> tw_config::Provider {
        tw_config::Provider {
            name: "p".into(),
            base_url: "https://x.com".into(),
            key: "k".into(),
            proxy: proxy.into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_default_proxy_is_direct_not_system() {
        // 显式优于隐式（§3.7）。默认跟随系统的话，用户在系统里开了全局
        // 代理，本地 Ollama 就会莫名连不上 —— 而配置文件里看不出任何线索。
        assert_eq!(tw_config::Provider::default().proxy, tw_config::DIRECT);
    }

    #[test]
    fn the_two_builtin_proxy_names_need_no_declaration() {
        let cfg = tw_config::Config::default();
        assert!(build_client(&cfg, &provider_with_proxy(tw_config::DIRECT)).is_ok());
        assert!(build_client(&cfg, &provider_with_proxy(tw_config::SYSTEM)).is_ok());
    }

    #[test]
    fn an_undeclared_proxy_name_fails_at_startup_and_lists_the_builtins() {
        // 启动时报，不要等请求进来。而且要说清有哪两个内置名字 ——
        // 用户十有八九是想写 `direct`。
        let cfg = tw_config::Config::default();
        let e = build_client(&cfg, &provider_with_proxy("airport")).unwrap_err();
        assert!(e.message.contains("airport"), "{}", e.message);
        assert!(e.message.contains("direct"), "{}", e.message);
        // 行续接留下的缩进不该进错误信息
        assert!(
            !e.message.contains("   "),
            "错误信息里有多余空格：{}",
            e.message
        );
    }

    #[test]
    fn a_declared_proxy_builds() {
        let cfg = tw_config::Config {
            proxies: vec![tw_config::Proxy {
                name: "airport".into(),
                kind: tw_config::ProxyKind::Socks5h,
                addr: "127.0.0.1:7890".into(),
                auth: None,
            }],
            ..Default::default()
        };
        assert!(build_client(&cfg, &provider_with_proxy("airport")).is_ok());
    }

    #[test]
    fn each_provider_gets_its_own_client() {
        // reqwest 的代理绑在 Client 上，不能按请求切换（§3.7）。共用一个
        // Client 的话，「官方走代理、Ollama 直连」这个最基本的需求就做
        // 不到 —— 而它恰恰是要代理这个功能的原因。
        let cfg = tw_config::Config {
            clients: vec![tw_config::Client {
                name: "c".into(),
                key: "tw-k".into(),
                ..Default::default()
            }],
            providers: vec![
                tw_config::Provider {
                    name: "a".into(),
                    base_url: "https://a.com".into(),
                    key: "k".into(),
                    ..Default::default()
                },
                tw_config::Provider {
                    name: "b".into(),
                    base_url: "https://b.com".into(),
                    key: "k".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let s = AppState::new(cfg).unwrap();
        let rt = s.runtime();
        assert_eq!(rt.clients.len(), 2);
        assert!(rt.clients.contains_key("a") && rt.clients.contains_key("b"));
    }

    #[test]
    fn a_known_key_resolves_to_its_client_name() {
        let s = AppState::new(cfg()).unwrap();
        let (name, pos) = s.identify(&hdr("x-api-key", "tw-good"), None).unwrap();
        assert_eq!(name, "default");
        // 位置带出来的方言是 §3.9 按方言过滤的依据
        assert_eq!(pos.dialect(), "Anthropic");
    }

    #[test]
    fn no_key_is_rejected_and_the_message_says_where_to_put_one() {
        let s = AppState::new(cfg()).unwrap();
        let e = s.identify(&HeaderMap::new(), None).unwrap_err();
        assert_eq!(e.source, crate::error::Source::Auth);
        assert!(e.message.contains("clients"));
    }

    #[test]
    fn an_unknown_key_is_rejected_without_echoing_it_back() {
        // 回显会给「猜密钥」这件事一个反馈信号，哪怕只是打码的。
        let s = AppState::new(cfg()).unwrap();
        let e = s.identify(&hdr("x-api-key", "tw-wrong"), None).unwrap_err();
        assert!(!e.message.contains("tw-wrong"));
        assert!(!e.message.contains("tw-wr"));
    }

    #[test]
    fn the_key_can_arrive_in_any_of_the_four_positions() {
        let s = AppState::new(cfg()).unwrap();
        assert!(s.identify(&hdr("x-api-key", "tw-good"), None).is_ok());
        assert!(s.identify(&hdr("x-goog-api-key", "tw-good"), None).is_ok());
        assert!(
            s.identify(&hdr("authorization", "Bearer tw-good"), None)
                .is_ok()
        );
        assert!(s.identify(&HeaderMap::new(), Some("key=tw-good")).is_ok());
    }
}
