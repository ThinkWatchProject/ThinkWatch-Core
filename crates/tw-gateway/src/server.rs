//! HTTP 服务：路由、身份识别、转发。

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{OriginalUri, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use bytes::Bytes;
use futures::StreamExt;

use crate::auth::key_eq;
use crate::error::GatewayError;
use crate::forward;
use crate::health::Health;
use tw_types::msg;

/// 256 MiB。大到能装下几张 4K 图的 base64（膨胀 33%），小到失控的
/// 客户端打不爆内存。
const MAX_BODY: usize = 256 * 1024 * 1024;

/// 所有 Client 共享的那部分设置。**只写一遍** —— 分成两处的话，走代理
/// 的那批和不走代理的那批会慢慢长出不同的超时行为。
fn base_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        // 分段超时。**没有整体超时** —— 一个跑了六分钟的
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
/// 按这家上游的出站设置建一个 HTTP 客户端。
///
/// **公开是给控制面检测一个还没保存的上游用的** —— 检测必须和转发走同一条
/// 出站路径，否则「检测通了、转发不通」会成为可能。
pub fn client_for_provider(
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
                GatewayError::config(msg!(
                    "gw.config.proxy_undefined", upstream = p.name.clone(), proxy = name =>
                    "Upstream `{upstream}` uses proxy `{proxy}`, which is not defined under \
                     `proxies`; the only built-in choices are direct and system."
                ))
            })?;
            let url = proxy.url().map_err(|e| {
                GatewayError::config(msg!(
                    "gw.config.proxy_password", proxy = name, detail = e =>
                    "The password for proxy `{proxy}` could not be read: {detail}"
                ))
            })?;
            match reqwest::Proxy::all(&url) {
                Ok(px) => b = b.proxy(px),
                Err(e) => {
                    // **默认让请求失败，不静默改走直连**。静默降级
                    // 最糟的情况不是失败，是它真的连上了，而你以为自己在
                    // 走代理。
                    if p.on_proxy_fail == tw_config::OnProxyFail::Direct {
                        tracing::warn!(
                            provider = %p.name, proxy = %name,
                            "proxy unusable, going direct as on_proxy_fail says: {e}"
                        );
                        b = b.no_proxy();
                    } else {
                        return Err(GatewayError::config(msg!(
                            "gw.config.proxy_unusable", upstream = p.name.clone(), proxy = name, detail = e =>
                            "Proxy `{proxy}`, used by upstream `{upstream}`, is unusable: {detail}"
                        )));
                    }
                }
            }
        }
    }
    b.build().map_err(|e| {
        GatewayError::config(msg!(
            "gw.config.http_client", detail = e => "The HTTP client could not be created: {detail}"
        ))
    })
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
    /// 不能按请求切换 —— 而这本来也是对的：连接池按上游隔离，
    /// 一个慢上游不会占着另一个的连接。
    pub clients: std::collections::HashMap<String, reqwest::Client>,
    /// 来源白名单。空 = 全放行，而那只在 loopback 下成立。
    pub allow: crate::access::AllowList,
    /// 出站脱敏的规则。
    ///
    /// **和配置一起建、一起换**，而不是每个请求现编一次 —— 自定义规则是
    /// 正则，摆在数据面上现编就是每个请求白付一次编译。
    pub redact: Arc<tw_redact::rules::RuleSet>,
    /// 工具调用审查的规则。同上。
    pub tools: Arc<tw_scan::rules::Rules>,
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
                None => clients.insert(p.name.clone(), client_for_provider(&config, p)?),
            };
        }
        let allow = crate::access::AllowList::parse(&config.listen.gateway.effective_allow_from())
            .map_err(|e| {
                GatewayError::config(msg!(
                    "gw.config.allow_from", detail = e => "listen.gateway.allow_from: {detail}"
                ))
            })?;
        // 两项防护的规则编译一次，跟着运行时一起换 —— 它们住在
        // config.yaml 的 `security` 里，所以「改了规则」和「改了别的配置」
        // 走同一条热重载路径。
        //
        // 自定义规则的正则在配置校验时已经编过一次，这里再失败只可能是有人
        // 绕过了校验，照样拒绝这份配置。认不出的内置规则 id 不拒绝：一条
        // 内置规则将来可能改名，用户停用过它的那一行不该让整份配置失效
        let sec = &config.security;
        let (redact, unknown) = tw_redact::rules::RuleSet::build(
            &sec.redact.enable,
            &sec.redact.disable,
            sec.redact.active_custom(),
        )
        .map_err(|e| {
            GatewayError::config(msg!("gw.config.security_rules", detail = e => "{detail}"))
        })?;
        let tools = tw_scan::rules::tool_rules(&sec.inspect_tools).map_err(|e| {
            GatewayError::config(msg!("gw.config.security_rules", detail = e => "{detail}"))
        })?;
        for id in unknown.iter().chain(&tools.warnings) {
            tracing::warn!(rule = %id, "`{id}` is not the id of a built-in rule, so turning it on or off did nothing");
        }
        Ok(Self {
            engine: Arc::new(config.engine()),
            config: Arc::new(config),
            clients,
            allow,
            redact: Arc::new(redact),
            tools: Arc::new(tools),
        })
    }
}

/// 决定一个 Client 能不能复用的那几个字段。
///
/// base_url 和 key 都**不在**里面：Client 不绑 URL，凭据是每个请求现加
/// 的。把它们算进来只会让「改个 key」白白丢掉一整个连接池。
pub(crate) fn proxy_shape(cfg: &tw_config::Config, p: &tw_config::Provider) -> String {
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
    /// 配置换入时整块换掉的那部分（第 ⑤ 步）。
    ///
    /// **一次 `store` 就是一次生效**：正在跑的请求持有旧的 `Arc`，跑完
    /// 自然释放；新请求看到的是新的。中间没有任何一个瞬间是半新半旧的。
    rt: Arc<arc_swap::ArcSwap<Runtime>>,
    /// 并发闸门。排队不拒绝。
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
    /// 不该因为你改了条规则就立刻又被试一遍。
    pub health: Arc<Health>,
    /// 模型汇总。**列表、准入和挑候选的唯一真相来源**。
    ///
    /// 从 `models` 和当前配置推出来（见 [`crate::models`]）：启动时只有手写
    /// 的清单，向上游问到之后、配置换了之后都会重算。
    pub catalog: Arc<arc_swap::ArcSwap<tw_engine::Catalog>>,
    /// 每个上游的模型清单，以及是什么时候、怎么来的。**跨重载存活**
    pub models: Arc<crate::models::Directory>,
    /// 请求体和响应体往哪儿交。
    ///
    /// **有界通道，满了就丢。**直接调用意味着文件 I/O 跑在转发那条路上
    /// —— 一次慢磁盘写就变成一次慢请求，而观测永远不该有这个权力。
    /// `None` 表示观测层没起来，那时什么都不做。
    body_sink: Arc<std::sync::Mutex<Option<crate::bodies::BodySender>>>,
    /// 每个上游最近一次报的订阅额度。
    ///
    /// **在内存里，不落库。**它是「现在还剩多少」，不是历史 —— 存一份
    /// 五分钟前的百分比，价值几乎为零，而它会让「重启之后显示的是旧
    /// 数字」变成一个要解释的问题。下一个请求回来就有新的了。
    quotas: Arc<std::sync::Mutex<std::collections::HashMap<String, crate::quota::Quota>>>,
    /// 监听地址变了。**这是「温」那一级**（三级热重载） ——
    /// 换端口不能只换配置：监听器是启动时建的，不重建的话新端口上什么
    /// 都没有，而旧端口还在服务。那种「改了没反应」比报错难查得多。
    relisten: Arc<tokio::sync::Notify>,
    /// OAuth 的 access token。
    ///
    /// **在内存里，跨重载存活。**access token 是派生状态 —— 不是用户输入
    /// 的，会过期，丢了重换一个就行。落盘只多一处密钥副本，换不到
    /// 任何东西；而跟着配置一起丢掉的话，改一条限流规则会让所有 OAuth
    /// 上游各自多打一次往返。用户真的改了 refresh token 时，缓存自己认
    /// 得出来（指纹对不上就重换）。
    pub oauth: Arc<crate::oauth::Cache>,
    /// 每家的典型首字节时间。`url-test` 策略靠它排序。
    ///
    /// **跨重载存活**：改一条规则不该让所有上游回到「没测过」。
    pub latency: Arc<crate::latency::Latency>,
    /// 价格簿：默认价目表 + 自定义价目表 + 哪个上游用哪张。
    ///
    /// **全进程只有这一份。**`cheapest` 排序、记账、测速报价、回放都从这
    /// 里取 —— 以前记账那一层攥着启动时的一份副本，改了价格要重启才生效。
    /// 配置重载时换掉自定义价目表，刷新默认价目表时换掉底表。
    pub pricing: tw_pricing::Shared,
    /// 刷新换回来的 token 往哪儿交（写回 config.yaml）。
    ///
    /// **和 body 那条路同一个形状**：数据面只管交出去，写文件是控制面的
    /// 事 —— 那里才有历史快照、乐观并发和防回环。`None` 表示控制面没
    /// 起来（比如测试里直接建的 AppState），那时轮换只报不写。
    renewal_sink: Arc<std::sync::Mutex<Option<crate::oauth::RenewalSender>>>,
    /// 已经报过「写回成功」的上游。
    ///
    /// **只压成功的那句，失败的每次都说。**会轮换的服务器每小时换一次，
    /// 而「已经帮你写回去了」这句话说一次就够 —— 通知的代价是用户学会
    /// 忽略通知，包括那些真该看的。失败不一样：它要一直挂着，
    /// 而且从成功变成失败是**状态变了**，必须重新说。
    rotation_told: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// 已经报过「凭据失效」的上游。**只在失效的那一刻报一次**，恢复之后清掉
    expired_told: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// 已经报过「用完」的额度窗口：(上游, 窗口)。**窗口恢复之后清掉**，再用完会重新报
    exhausted: Arc<std::sync::Mutex<std::collections::HashSet<(String, String)>>>,
    /// 凭据正被上游拒绝的那几家。**进入和恢复各报一次**
    rejected: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// 每个代理最近一次检查的结果。见 [`AppState::check_proxy`]
    proxies: Arc<std::sync::Mutex<std::collections::HashMap<String, ProxyState>>>,
    /// 正在服务中的请求数。见 [`crate::live`]。
    pub live: crate::live::Live,
}

/// 这个出站设置是配置里定义的代理吗。`direct` 和 `system` 不是：
/// 前者没有代理，后者的地址在 reqwest 建连时才去环境里查，我们没有它可以握手
fn named_proxy(proxy: &str) -> bool {
    !proxy.is_empty() && proxy != tw_config::DIRECT && proxy != tw_config::SYSTEM
}

/// 一个代理最近一次检查的结果。
///
/// **只在转发失败之后才检查**，而且同一个代理隔一会儿才检一次 —— 一条打不通的
/// 链路上每个请求都去检一遍，等于把一次故障放大成一串握手。
struct ProxyState {
    reachable: bool,
    checked_at: std::time::Instant,
    /// 正在检查。并发的失败只触发一次
    checking: bool,
}

/// 同一个代理两次检查之间至少隔多久
const PROXY_RECHECK: std::time::Duration = std::time::Duration::from_secs(30);

impl AppState {
    pub fn new(config: tw_config::Config) -> Result<Self, GatewayError> {
        let http = base_client_builder().build().map_err(|e| {
            GatewayError::config(msg!(
                "gw.config.http_client", detail = e => "The HTTP client could not be created: {detail}"
            ))
        })?;
        let limits = config.limits.clone();
        let pricing_config = config.pricing.clone();
        let price_assign = config.price_assign();
        let models = Arc::new(crate::models::Directory::default());
        models.reconcile(&config);
        let rt = Runtime::build(config, None)?;
        let state = Self {
            rt: Arc::new(arc_swap::ArcSwap::from_pointee(rt)),
            gate: Arc::new(arc_swap::ArcSwap::from_pointee(crate::limits::Gate::new(
                limits,
            ))),
            http,
            bus: tw_observe::EventBus::new(),
            health: Arc::new(Health::new()),
            catalog: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
            models,
            body_sink: Arc::new(std::sync::Mutex::new(None)),
            quotas: Arc::new(std::sync::Mutex::new(Default::default())),
            relisten: Arc::new(tokio::sync::Notify::new()),
            oauth: Arc::new(crate::oauth::Cache::new()),
            latency: Arc::new(crate::latency::Latency::new()),
            pricing: tw_pricing::shared(tw_pricing::PriceBook::new(
                Arc::new(tw_pricing::Table::builtin().unwrap_or_else(|e| {
                    // **加载不了不能挡住启动**：那时成本显示「未知」，
                    // 而转发照常
                    tracing::warn!("the built-in price sheet failed to load; every request is recorded with an unknown cost: {e}");
                    tw_pricing::Table::empty()
                })),
                pricing_config,
                price_assign,
            )),
            renewal_sink: Arc::new(std::sync::Mutex::new(None)),
            rotation_told: Arc::new(std::sync::Mutex::new(Default::default())),
            expired_told: Arc::new(std::sync::Mutex::new(Default::default())),
            exhausted: Arc::new(std::sync::Mutex::new(Default::default())),
            rejected: Arc::new(std::sync::Mutex::new(Default::default())),
            proxies: Arc::new(std::sync::Mutex::new(Default::default())),
            live: crate::live::Live::default(),
        };
        // 手写的清单马上可用；向上游问是后台的事，不挡启动
        state.publish_catalog();
        Ok(state)
    }

    /// 按目录和当前配置重算模型汇总。
    pub(crate) fn publish_catalog(&self) {
        self.models.publish(|| self.config(), &self.catalog);
    }

    /// 要发给这一家的请求头，凭据在里面。**OAuth 那一类要联网换 token，所以这条路是
    /// async 的**；密钥和 `${ENV}` 走同步那条，零额外成本。
    ///
    /// `http` 必须是**这一家自己的** client：换 token 要走它该走的代理。
    /// 用一个干净的 client 去换，代理后面的用户会得到一个
    /// 「数据面通、刷新不通」的组合 —— 而那个症状看起来完全不像凭据问题。
    ///
    /// `client`：发起请求的网关密钥名，填 `{{client}}` 用。后台的探测没有这个人。
    pub async fn headers_for(
        &self,
        p: &tw_config::Provider,
        http: &reqwest::Client,
        client: Option<&str>,
    ) -> Result<Vec<(String, String)>, String> {
        let token = match &p.oauth {
            Some(o) => Some(self.oauth_token(p, o, http).await?),
            None => None,
        };
        p.outbound_headers(token.as_deref(), client)
            .map_err(|e| e.to_string())
    }

    /// 这一家的 OAuth access token。没配 oauth 是错误。
    ///
    /// 检测一个沿用原凭据的上游时用：**按原来那一家的名字换**，缓存和轮换写回都认名字。
    pub async fn oauth_token_for(
        &self,
        p: &tw_config::Provider,
        http: &reqwest::Client,
    ) -> Result<String, String> {
        let o = p
            .oauth
            .as_ref()
            .ok_or_else(|| format!("upstream `{}` has no OAuth configured", p.name))?;
        self.oauth_token(p, o, http).await
    }

    /// 上游回了 401 之后换一个 access token，重新生成请求头。
    ///
    /// **只该调一次**（由调用方保证）：换回来的 token 还是 401，说明问题不在 token。
    /// `sent_at` 是被拒的那个请求发出去的时刻 —— 那之后已经有人换过的话，直接用新的。
    pub async fn headers_after_401(
        &self,
        p: &tw_config::Provider,
        http: &reqwest::Client,
        client: Option<&str>,
        sent_at: std::time::Instant,
    ) -> Result<Vec<(String, String)>, String> {
        let o = p
            .oauth
            .as_ref()
            .ok_or_else(|| format!("upstream `{}` has no OAuth configured", p.name))?;
        let got = self
            .oauth
            .invalidate_and_refresh(&p.name, o, http, sent_at)
            .await;
        let token = self.settle_token(p, got)?;
        p.outbound_headers(Some(&token), client)
            .map_err(|e| e.to_string())
    }

    /// 换一个 access token，服务器换发了新的 refresh token 就交给控制面写回。
    async fn oauth_token(
        &self,
        p: &tw_config::Provider,
        o: &tw_config::OAuth,
        http: &reqwest::Client,
    ) -> Result<String, String> {
        let got = self.oauth.token(&p.name, o, http).await;
        self.settle_token(p, got)
    }

    /// 换 token 的结果：失效了要说，轮换了要写回。
    fn settle_token(
        &self,
        p: &tw_config::Provider,
        got: Result<(String, Option<crate::oauth::Renewed>), crate::oauth::OauthError>,
    ) -> Result<String, String> {
        let (token, renewed) = match got {
            Ok(t) => {
                if let Ok(mut told) = self.expired_told.lock() {
                    told.remove(&p.name);
                }
                t
            }
            Err(e) => {
                if e.needs_login() {
                    self.report_expired(&p.name, &e);
                }
                return Err(e.to_string());
            }
        };
        if let Some(r) = renewed {
            let rotated = r.refresh.is_some();
            let sink = self.renewal_sink.lock().ok().and_then(|g| g.clone());
            let lost = match sink {
                // **交出去就不管了。**写文件、存历史、防回环都在控制面，
                // 而这里是转发路径 —— 它不能等一次磁盘写。
                // 通道满 = 前一次还没写完
                Some(tx) => tx
                    .try_send(r)
                    .err()
                    .map(|_| "the write-back queue is full, so this rotation was not written back"),
                // 控制面没起来：**只报不写**，而且要说清没写
                None => Some(
                    "the gateway is running on its own, with no configuration manager, so this \
                     rotation was not written back",
                ),
            };
            // **换发的 refresh token 没写回才要说** —— 丢掉的是一份还没落盘、旧的已经作废的
            // 凭据。access token 没写回不要紧：下次启动拿 refresh token 再换一个就是
            if let (Some(why), true) = (lost, rotated) {
                self.report_rotation(&p.name, false, why);
            }
        }
        Ok(token)
    }

    /// 上游对凭据的态度变了没有。**进入被拒和恢复各报一次**。
    ///
    /// 熔断器看不见这件事：4xx 不算失败（换一家也一样被拒），所以一个凭据坏掉的
    /// 上游永远不会被熔断，也就永远不会有 `HealthChanged`。而这件事要用户去改配置。
    pub(crate) fn note_auth(&self, provider: &str, status: u16) {
        let rejected = status == 401 || status == 403;
        // 别的失败（500、429、超时）什么都不说明：凭据可能好好的
        if !rejected && !(200..300).contains(&status) {
            return;
        }
        let changed = self
            .rejected
            .lock()
            .map(|mut g| {
                if rejected {
                    g.insert(provider.to_string())
                } else {
                    g.remove(provider)
                }
            })
            .unwrap_or(false);
        if !changed {
            return;
        }
        if rejected {
            tracing::warn!(provider, status, "the upstream rejected the credential");
        }
        self.bus.emit(tw_api::Event::AuthChanged {
            id: self.bus.next_id(),
            provider: provider.to_string(),
            state: if rejected { "rejected" } else { "accepted" }.into(),
            status: rejected.then_some(status),
            at_ms: now_ms(),
        });
    }

    /// 经这个代理的请求成功了：它之前要是被判成不通，现在说一声通了。
    pub(crate) fn note_proxy_ok(&self, proxy: &str) {
        if !named_proxy(proxy) {
            return;
        }
        let recovered = self
            .proxies
            .lock()
            .map(|mut g| match g.get_mut(proxy) {
                Some(st) if !st.reachable => {
                    st.reachable = true;
                    st.checked_at = std::time::Instant::now();
                    true
                }
                _ => false,
            })
            .unwrap_or(false);
        if recovered {
            self.bus.emit(tw_api::Event::ProxyChanged {
                id: self.bus.next_id(),
                proxy: proxy.to_string(),
                state: "reachable".into(),
                failed: None,
                detail: None,
                at_ms: now_ms(),
            });
        }
    }

    /// 经这个代理的请求连不上了：检一次代理本身。
    ///
    /// **不做定时探测**，只在转发失败之后顺手检一次 —— 没有它，代理挂掉在界面上
    /// 看起来是「好几家上游同时不通」，而那两件事要做的处理完全不同。
    pub(crate) fn check_proxy(&self, proxy: &str) {
        if !named_proxy(proxy) {
            return;
        }
        let due = self
            .proxies
            .lock()
            .map(|mut g| {
                let st = g.entry(proxy.to_string()).or_insert(ProxyState {
                    reachable: true,
                    // 第一次就该检：把时间放到足够早
                    checked_at: std::time::Instant::now() - PROXY_RECHECK,
                    checking: false,
                });
                let due = !st.checking && st.checked_at.elapsed() >= PROXY_RECHECK;
                if due {
                    st.checking = true;
                }
                due
            })
            .unwrap_or(false);
        if !due {
            return;
        }
        let state = self.clone();
        let name = proxy.to_string();
        tokio::spawn(async move {
            let cfg = state.config();
            // 卡在哪一步 + 为什么。**两个都要**：一句「TCP 握手失败」说不出
            // 是地址错了还是代理没起来。两样分开发，界面自己组句
            let result: Option<(Option<crate::l1::Stage>, tw_types::Msg)> =
                match cfg.proxies.iter().find(|p| p.name == name) {
                    Some(px) => match crate::l1::hop_of(px) {
                        Ok(hop) => {
                            let (host, port) = crate::l1::proxy_target(&cfg, &name);
                            let r = crate::l1::l1_proxy(&hop, &host, port).await;
                            if r.ok {
                                None
                            } else {
                                Some((
                                    r.failed,
                                    r.error.unwrap_or_else(|| {
                                        tw_types::msg!(
                                            "l1.unreachable" => "The proxy could not be reached."
                                        )
                                    }),
                                ))
                            }
                        }
                        Err(e) => Some((
                            Some(crate::l1::Stage {
                                step: crate::l1::Step::Config,
                                peer: crate::l1::Peer::Proxy,
                            }),
                            e,
                        )),
                    },
                    // 配置刚好在这中间改了，代理没了：不报
                    None => None,
                };
            let changed = state
                .proxies
                .lock()
                .map(|mut g| match g.get_mut(&name) {
                    Some(st) => {
                        st.checking = false;
                        st.checked_at = std::time::Instant::now();
                        let reachable = result.is_none();
                        let changed = st.reachable != reachable;
                        st.reachable = reachable;
                        changed
                    }
                    None => false,
                })
                .unwrap_or(false);
            if !changed {
                return;
            }
            match &result {
                Some((_, why)) => tracing::warn!(proxy = %name, "proxy is unreachable: {why}"),
                None => tracing::info!(proxy = %name, "proxy is reachable again"),
            }
            let (failed, detail) = match result {
                Some((stage, why)) => (
                    stage.map(|s| tw_api::L1Stage {
                        step: s.step.slug().into(),
                        peer: s.peer.slug().into(),
                    }),
                    Some(why),
                ),
                None => (None, None),
            };
            state.bus.emit(tw_api::Event::ProxyChanged {
                id: state.bus.next_id(),
                proxy: name,
                state: if detail.is_some() {
                    "unreachable"
                } else {
                    "reachable"
                }
                .into(),
                failed,
                detail,
                at_ms: now_ms(),
            });
        });
    }

    /// 凭据失效报给界面。**同一家只报一次**，直到它恢复。
    fn report_expired(&self, provider: &str, e: &crate::oauth::OauthError) {
        let first = self
            .expired_told
            .lock()
            .map(|mut g| g.insert(provider.to_string()))
            .unwrap_or(false);
        if !first {
            return;
        }
        tracing::warn!(
            provider,
            "the OAuth credential has expired and needs a new sign-in: {e}"
        );
        self.bus.emit(tw_api::Event::CredentialExpired {
            id: self.bus.next_id(),
            provider: provider.to_string(),
            detail: e.to_string(),
            at_ms: now_ms(),
        });
    }

    /// 读响应头里的订阅额度：存下来，报给界面，用完的窗口单独报一次。
    ///
    /// **429 的那一跳也要读**：额度用完时上游回的正是 429，只读成功那一跳的话，
    /// 「用完了」这件事永远看不到。
    pub(crate) fn note_quota(&self, id: u64, provider: &str, headers: &reqwest::header::HeaderMap) {
        self.record_quota(id, provider, crate::quota::from_headers_reqwest(headers));
    }

    /// 记下一份额度。**账号接口问来的也走这里**：额度只在内存里，冷启动之后要等第一次
    /// 请求才有，而界面一打开就该看得见
    pub fn record_quota(&self, id: u64, provider: &str, quota: crate::quota::Quota) {
        if quota.is_empty() {
            return;
        }
        if let Ok(mut g) = self.quotas.lock() {
            g.insert(provider.to_string(), quota.clone());
        }
        self.bus.emit(tw_api::Event::QuotaSeen {
            id,
            provider: provider.to_string(),
            windows: quota
                .windows
                .iter()
                .map(|w| tw_api::QuotaWindow {
                    window: w.window.clone(),
                    used_percent: w.used_percent,
                    reset_in_secs: w.reset_in_secs,
                    status: w.status.clone(),
                })
                .collect(),
            at_ms: now_ms(),
        });
        for w in &quota.windows {
            let key = (provider.to_string(), w.window.clone());
            let changed = self
                .exhausted
                .lock()
                .map(|mut g| {
                    if w.rejected() {
                        g.insert(key)
                    } else {
                        g.remove(&key);
                        false
                    }
                })
                .unwrap_or(false);
            if changed {
                tracing::warn!(provider, window = %w.window, "the subscription quota is used up");
                self.bus.emit(tw_api::Event::QuotaExhausted {
                    id: self.bus.next_id(),
                    provider: provider.to_string(),
                    window: w.window.clone(),
                    reset_in_secs: w.reset_in_secs,
                    at_ms: now_ms(),
                });
            }
        }
    }

    /// 凭据轮换的结果报给界面。
    ///
    /// **写成功也要报一次。**用户的 config.yaml 被我们改了 —— 哪怕改得
    /// 完全正确，不说一声也是不对的：他的编辑器会弹「文件已在磁盘上更改」，
    /// 而那时他应该已经知道原因。
    pub fn report_rotation(&self, provider: &str, persisted: bool, detail: &str) {
        {
            let mut told = self.rotation_told.lock().expect("lock not poisoned");
            if persisted {
                if !told.insert(provider.to_string()) {
                    // 这家的「已经帮你写回去了」说过了
                    tracing::debug!(
                        provider,
                        "the credential rotated again and was written back"
                    );
                    return;
                }
            } else {
                // 从「写得进去」变成「写不进去」是状态变了 —— 下次写成功
                // 的时候要重新说一句，否则用户不知道问题已经解决
                told.remove(provider);
            }
        }
        if persisted {
            tracing::info!(
                provider,
                "the token endpoint issued a new refresh token; it was written back to config.yaml"
            );
        } else {
            tracing::warn!(
                provider,
                detail,
                "the token endpoint issued a new refresh token and it could not be written back to \
                 config.yaml; this has to be dealt with before a restart, or every request to this \
                 upstream will come back 401"
            );
        }
        self.bus.emit(tw_api::Event::CredentialRotated {
            id: self.bus.next_id(),
            provider: provider.to_string(),
            persisted,
            detail: detail.to_string(),
            at_ms: now_ms(),
        });
    }

    /// 这一家该用的 HTTP client（带着它该走的代理）。
    ///
    /// **给控制面用。**数据面自己整轮持着同一份 `Runtime`，直接从那里
    /// 取 —— 走这里会重新 `load` 一次，于是一个请求可能跨在两份配置上。
    pub fn client_for(&self, name: &str) -> reqwest::Client {
        self.runtime()
            .clients
            .get(name)
            .cloned()
            .unwrap_or_else(|| self.http.clone())
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

    /// 换一份默认价目表进来（启动时读到的上次刷新结果，或者刚刷新的）。
    pub fn set_price_table(&self, table: tw_pricing::Table) {
        let table = Arc::new(table);
        // **rcu，不是 load 再 store。**刷新和配置重载可能同时发生，后者
        // 换的是自定义价目表 —— 先读后写会把对方刚换进去的那一半覆盖掉
        self.pricing.rcu(|book| book.with_table(table.clone()));
    }

    /// 这家实际怎么收钱。
    ///
    /// **配置里写了就听配置的，没写就自动判**：响应头里报过订阅额度的就是
    /// 订阅型。那个信号一直在我们手上，不该变成一个用户要填的
    /// 字段 —— 而一个填错了的字段比没有更糟。
    ///
    /// **自动判有一个已知的边界：每次进程启动之后，打给一家订阅上游的第一个
    /// 请求会被按量计价。**那时我们还没见过它的额度头。之后就对了。
    ///
    /// 没有更好的办法：额度头只在响应里，而计价发生在响应之后 —— 想在第一
    /// 个请求之前知道，只能主动探测，而那条路是明确否掉的（会占用户
    /// 自己的配额）。在乎那一条记录的人，在配置里写一行 `billing:
    /// subscription` 就没有歧义了。
    pub fn billing_of(&self, p: &tw_config::Provider) -> tw_config::Billing {
        if let Some(b) = p.billing {
            return b;
        }
        let reported = self
            .quotas
            .lock()
            .map(|g| g.get(&p.name).is_some_and(|q| !q.is_empty()))
            .unwrap_or(false);
        if reported {
            tw_config::Billing::Subscription
        } else {
            tw_config::Billing::PerToken
        }
    }

    /// `cheapest` 排序用的单价：每家跑这个模型的 (输入, 输出)，微分/百万 token。
    ///
    /// **路由和预演共用这一个** —— 各写一份的话，预演说会选 A，实际选的是 B。
    pub fn unit_prices(
        &self,
        providers: &[tw_config::Provider],
        candidates: &[String],
        model: &str,
    ) -> std::collections::HashMap<String, (i64, i64)> {
        let book = self.pricing.load();
        candidates
            .iter()
            .filter_map(|name| {
                let p = providers.iter().find(|p| &p.name == name)?;
                // **订阅制和不计费的边际成本是零，它们就是最便宜的**。
                // 而「价格未知」不是「免费」 —— 它不在这张表里，排到最后去
                match p.billing {
                    Some(tw_config::Billing::Subscription | tw_config::Billing::Free) => {
                        Some((name.clone(), (0, 0)))
                    }
                    Some(tw_config::Billing::Unknown) => None,
                    _ => book.unit_micros(name, model).map(|u| (name.clone(), u)),
                }
            })
            .collect()
    }

    /// 接上写回的去处。**控制面起来之后才调** —— 在那之前刷新只报不写。
    pub fn set_renewal_sink(&self, tx: crate::oauth::RenewalSender) {
        if let Ok(mut g) = self.renewal_sink.lock() {
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

    /// 换一份配置进去（第 ④⑤ 步）。
    ///
    /// **建不起来就什么都不换。**校验已经在 `tw_config::reload` 里做过
    /// 三遍了，但运行时对象仍然可能建不起来（比如代理地址 reqwest 不认），
    /// 而那时旧配置必须原样继续服务。
    pub fn reload(&self, config: tw_config::Config) -> Result<(), GatewayError> {
        let old = self.rt.load();
        let limits_changed = old.config.limits != config.limits;
        let new_limits = config.limits.clone();
        let next = Runtime::build(config, Some(&old))?;
        let relisten =
            old.config.listen.gateway.socket_addr() != next.config.listen.gateway.socket_addr();
        // 自定义价目表和上游的选择跟着配置走，默认价目表不变
        let (sheets, assign) = (next.config.pricing.clone(), next.config.price_assign());
        self.pricing
            .rcu(|book| book.with_config(sheets.clone(), assign.clone()));
        self.rt.store(Arc::new(next));
        // 模型汇总马上按新配置重算：删掉、停用的上游的模型必须立刻消失（列表
        // 即承诺），改了范围的立刻生效。新加的、地址凭据变了的在后台补问
        if self.models.reconcile(&self.config()) {
            self.models.wake();
        }
        self.publish_catalog();
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
            return Err(GatewayError::auth(msg!(
                "gw.auth.no_key" =>
                "The request carried no gateway key. Configure the client with one of the gateway \
                 keys under `clients` in config.yaml."
            )));
        };
        let rt = self.rt.load();
        let found = rt.config.clients.iter().find(|c| key_eq(&c.key, &key));
        match found {
            // **停用的密钥要说清楚是停用了。**这一条和「密钥无效」不同：
            // 用户是自己停的，而把它说成无效会让他去查客户端配置 —— 那里
            // 什么问题都没有
            Some(c) if c.disabled => Err(GatewayError::auth(msg!(
                "gw.auth.key_disabled", key = c.name.clone() =>
                "Gateway key `{key}` is disabled. Enable it on the app's keys page to use it again."
            ))),
            Some(c) => Ok((c.name.clone(), position)),
            // 不回显收到的 key，哪怕是打码的 —— 回显会让「猜密钥」
            // 这件事有了反馈信号。
            None => Err(GatewayError::auth(msg!(
                "gw.auth.key_invalid" =>
                "The gateway key is not valid. Check that the key in the client's configuration \
                 matches the one in config.yaml."
            ))),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        // **和准入共用同一个函数** —— 列表和准入不可能不一致。
        .route("/v1/models", get(list_models))
        // **单点查询要走同一道准入**。不接这条的话它掉进
        // fallback 直接透传上游 —— 一个被 `allow` 限制成只能用便宜
        // 模型的 client，`GET /v1/models/claude-opus-4` 照样拿 200。
        // 这个洞在别的项目里点过名（「都没做过滤」），而我们自己
        // 也漏了。**同一个 `admits` 函数，列表和单点不可能不一致。**
        //
        // **只截 GET。**Gemini 把调用写成 `POST /v1beta/models/{model}:generateContent`，
        // 和单点查询是同一个路径模式 —— 没有这个 `fallback`，那些请求会被这条只认
        // GET 的路由拒成 405，永远到不了透传
        .route("/v1/models/{model}", get(get_model).fallback(passthrough))
        // Gemini 方言的路径。它的客户端问的是 `/v1beta/models` 和 `/v1beta/models/x`
        .route("/v1beta/models", get(list_models))
        .route(
            "/v1beta/models/{model}",
            get(get_model).fallback(passthrough),
        )
        // M0 只有透传：任何方法、任何路径都往上游送。M1 加路由时，
        // 这里会先过规则引擎再决定送给谁。
        .fallback(any(passthrough))
        .with_state(state)
}

/// 接管一次 WebSocket 升级。
///
/// 路由照走一遍 —— **一次升级也是一次请求**，`deny` 规则、熔断对它一样
/// 有效。之后把连接交给 [`crate::ws::proxy`]，那里会在
/// 每一帧上重新点一遍管线的保护。
#[allow(clippy::too_many_arguments)]
async fn ws_upgrade(
    state: AppState,
    rt: Arc<Runtime>,
    ws: axum::extract::WebSocketUpgrade,
    client_name: String,
    uri: axum::http::Uri,
    query: Option<String>,
    headers: HeaderMap,
    started: std::time::Instant,
    live: crate::live::Pass,
) -> Result<Response, GatewayError> {
    // 升级请求没有体，所以性质里只有客户端名字 —— 按模型路由的规则
    // 对它不适用，而那是对的：这条连接上会跑什么模型，现在还不知道
    let facts = tw_engine::RequestFacts {
        client: client_name.clone(),
        ..Default::default()
    };
    let decision = match rt.engine.route(&facts).map_err(|e| {
        GatewayError::config(msg!("gw.route.failed", detail = e => "Routing failed: {detail}"))
    })? {
        tw_engine::Outcome::Route(d) => d,
        tw_engine::Outcome::Deny { rule, reason } => {
            tracing::info!(%rule, "a rule denied the WebSocket upgrade");
            return Err(GatewayError::denied(msg!(
                "gw.route.denied", rule = rule, reason = reason =>
                "Rule `{rule}` denied this request: {reason}"
            )));
        }
    };
    let (alive, _) = state.health.filter(&decision.candidates);
    let Some(name) = alive.first().map(|s| s.to_string()) else {
        return Err(GatewayError::config(msg!(
            "gw.route.no_upstream_alive" => "No upstream is available."
        )));
    };
    let Some(provider) = rt.config.providers.iter().find(|p| p.name == name) else {
        return Err(GatewayError::config(msg!(
            "gw.route.upstream_missing", upstream = name.clone() =>
            "`{upstream}` is not in the configuration."
        )));
    };
    // **走代理的上游不代理 WS**，而且要明说。悄悄绕过用户配的代理，
    // 等于把他以为在代理后面的流量直接发出去
    if provider.proxy != tw_config::DIRECT {
        return Err(GatewayError::config(msg!(
            "gw.ws.proxy_unsupported", upstream = name.clone(), proxy = provider.proxy.clone() =>
            "Upstream `{upstream}` goes through proxy `{proxy}`. WebSocket connections are not \
             forwarded through a proxy yet; only directly connected upstreams are."
        )));
    }
    let http = rt.clients.get(&name).unwrap_or(&state.http);
    let upstream_headers = state
        .headers_for(provider, http, Some(&client_name))
        .await
        .map_err(|e| {
            GatewayError::config(msg!(
                "gw.credentials.failed", upstream = name.clone(), detail = e =>
                "The credential for upstream `{upstream}` could not be obtained: {detail}"
            ))
        })?;
    let url = crate::ws::upstream_url(&provider.base_url, uri.path(), query.as_deref());
    let id = state.bus.next_id();
    state.bus.emit(tw_api::Event::RequestStarted {
        id,
        client: client_name,
        client_hint: crate::hint::client_hint(&headers),
        session_fp: None,
        provider: name.clone(),
        model: String::new(),
        method: "WS".to_string(),
        path: uri.path().to_string(),
        at_ms: now_ms(),
    });
    // 这条连接怎么断的，就是这个请求的结局。**跟着连接走**：升级没完成
    // 就被丢掉的 —— 客户端没等到 101 就走了 —— 由 Drop 报成取消。WS 帧
    // 不留档，所以没有 body 的去处
    let ending = crate::ending::Ending::new(
        state.bus.clone(),
        id,
        String::new(),
        started,
        now_ms() as i64,
        None,
    );
    let provider = provider.clone();
    let rules = crate::ws::Rules {
        redact_mode: rt.config.security.redact.mode,
        redact: rt.redact.clone(),
        inspect_mode: rt.config.security.inspect_tools.mode,
        tools: rt.tools.clone(),
    };
    Ok(ws.on_upgrade(move |sock| async move {
        // 一条 WS 连接活多久，这个请求就算在服务中多久
        let _live = live;
        let mut ending = ending;
        ending.responded(101);
        crate::ws::proxy(
            state,
            sock,
            url,
            upstream_headers,
            provider,
            rules,
            id,
            ending,
        )
        .await;
    }))
}

/// 列模型、查单个模型时，客户端是哪一种。
///
/// 这两个请求没有体，路径也分不出 Anthropic 和 OpenAI（都是 `/v1/models`），
/// 只能看请求头：
///
/// - `/v1beta` 路径、或者把密钥放在 Google 位置上的是 Gemini
/// - 带 `anthropic-version`、或者把密钥放在 `x-api-key` 的是 Anthropic ——
///   两个信号**任一个**就算：Claude Code 用 `ANTHROPIC_AUTH_TOKEN` 时密钥走
///   Bearer，但版本头照带；手写的脚本常常只放 `x-api-key`
/// - 其余按 OpenAI 算
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListingShape {
    Anthropic,
    Openai,
    Gemini,
}

impl ListingShape {
    fn of(path: &str, headers: &HeaderMap, position: crate::auth::KeyPosition) -> Self {
        if path.starts_with("/v1beta") || position == crate::auth::KeyPosition::GoogleHeader {
            ListingShape::Gemini
        } else if headers.contains_key("anthropic-version")
            || position == crate::auth::KeyPosition::AnthropicHeader
        {
            ListingShape::Anthropic
        } else {
            ListingShape::Openai
        }
    }

    /// 这种客户端能用哪些协议的上游的模型。和请求那条路用同一张表：列出来的是
    /// 用来生成回答的模型，四种格式互相转换，所以谁都能用
    fn protocols(&self) -> Vec<&'static str> {
        use crate::client_api::{ClientApi, slugs};
        let api = match self {
            ListingShape::Anthropic => ClientApi::AnthropicMessages,
            ListingShape::Openai => ClientApi::OpenaiChat,
            ListingShape::Gemini => ClientApi::Gemini,
        };
        slugs(api.servable_by(true))
    }
}

/// `GET /v1/models`。
///
/// 三种方言的响应结构不同，但**列表内容来自同一个函数** —— 差别只在
/// 外壳。
async fn list_models(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    OriginalUri(uri): OriginalUri,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Result<Response, GatewayError> {
    let rt = state.runtime();
    if !rt.allow.allows(peer.ip()) {
        return Err(GatewayError::auth(msg!(
            "gw.auth.source_not_allowed", peer = peer.ip() =>
            "{peer} is not among the allowed source addresses."
        )));
    }
    let (client, position) = state.identify(&headers, query.as_deref())?;
    let allow = rt
        .config
        .clients
        .iter()
        .find(|c| c.name == client)
        .and_then(|c| c.allow.clone());
    let shape = ListingShape::of(uri.path(), &headers, position);
    let models = state
        .catalog
        .load()
        .resolve_allowed(Some(&shape.protocols()), allow.as_deref());

    let now = now_ms() / 1000;
    let body = match shape {
        ListingShape::Gemini => serde_json::json!({
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

/// `GET /v1/models/:model`。
///
/// **不许可就当它不存在（404），不是 403。**回 403 等于告诉对方
/// 「这个模型在，只是你不能用」—— 而列表里根本没列它，两处说法不一致
/// 本身就是一条信息。
async fn get_model(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    axum::extract::Path(model): axum::extract::Path<String>,
    OriginalUri(uri): OriginalUri,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Result<Response, GatewayError> {
    let rt = state.runtime();
    if !rt.allow.allows(peer.ip()) {
        return Err(GatewayError::auth(msg!(
            "gw.auth.source_not_allowed", peer = peer.ip() =>
            "{peer} is not among the allowed source addresses."
        )));
    }
    let (client, position) = state.identify(&headers, query.as_deref())?;
    let allow = rt
        .config
        .clients
        .iter()
        .find(|c| c.name == client)
        .and_then(|c| c.allow.clone());
    let catalog = state.catalog.load();
    // 目录空着时不拦 —— 那说明探测还没回来或者上游都不给列表，这时候
    // 拦等于把整个网关关掉（和请求那条路同一个判断）
    let shape = ListingShape::of(uri.path(), &headers, position);
    if !catalog.is_empty() && !catalog.admits(&model, Some(&shape.protocols()), allow.as_deref()) {
        return Err(GatewayError::new(
            crate::error::Source::Request,
            msg!(
                "gw.model.unknown", model = model.clone() =>
                "There is no model {model}. GET /v1/models lists the models that are available."
            ),
        ));
    }
    let now = now_ms() / 1000;
    let body = match shape {
        ListingShape::Gemini => {
            serde_json::json!({ "name": format!("models/{model}") })
        }
        _ => serde_json::json!({ "id": model, "object": "model", "created": now }),
    };
    Ok(axum::Json(body).into_response())
}

async fn passthrough(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    OriginalUri(uri): OriginalUri,
    RawQuery(query): RawQuery,
    // **必须排在 `body` 前面。**提取器按顺序跑，而 `Bytes` 会把体吃掉
    // —— 一次升级要的是那条连接本身，体被读走之后就没得升了
    crate::ws::MaybeUpgrade(upgrade): crate::ws::MaybeUpgrade,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, GatewayError> {
    let started = std::time::Instant::now();
    // 从这一刻起它就算「在服务中」。**排队等并发名额的也算** —— 那时客户
    // 端的连接已经开着在等了，这时候重启网关一样会让它失败。
    let live = state.live.enter();
    // **整个请求只取一次运行时。**中途重新取会让一个请求跨在两份配置
    // 上：按新规则选了 provider，却拿旧的 Client 去发 —— 而那种不一致
    // 完全静默。
    let rt = state.runtime();
    // 来源检查在身份检查**之前**：一个不该连过来的地址，不该有机会
    // 试密钥。
    if !rt.allow.allows(peer.ip()) {
        return Err(GatewayError::new(
            crate::error::Source::Auth,
            msg!(
                "gw.auth.source_not_allowed_hint", peer = peer.ip() =>
                "{peer} is not among the allowed source addresses. Change \
                 listen.gateway.allow_from, or set bind to loopback."
            ),
        ));
    }
    let (client_name, position) = state.identify(&headers, query.as_deref())?;
    // WebSocket 升级。**在鉴权之后、解体之前分叉** —— 鉴权
    // 在前是因为一个不该连过来的地址不该有机会升级；解体之前是因为
    // 升级要的是那条连接，而 `Bytes` 会把它读干净。
    if let Some(ws) = upgrade.filter(|_| crate::ws::is_upgrade(&headers)) {
        return ws_upgrade(
            state,
            rt,
            ws,
            client_name,
            uri,
            query,
            headers,
            started,
            live,
        )
        .await;
    }
    // 客户端调的是哪种 API：**看路径**（见 `client_api`）。认不出的路径照旧
    // 直通，出错时的格式退回按密钥位置猜。
    let api = crate::client_api::ClientApi::of_path(uri.path());
    // 从这里往下，所有错误都要用客户端自己那套结构回。
    // **认证失败在这一行之前，那时方言还猜不出来** —— key 就是没认出来
    // 的，只能退回 Anthropic 形状，而那是桌面版的主用例。
    let dialect = api
        .map(|a| a.error_dialect())
        .unwrap_or_else(|| crate::error::Dialect::from_key_position(position));
    //
    // 这个请求的结局（见 `crate::ending`）。**管线发出开始事件时把它放
    // 进来。**
    //
    // 它待在这一层而不是管线里面，是因为只有这里分得清两件事：管线**返回
    // 了**一个错误，和管线**被丢掉了**。前者是失败，按返回的错误报；后者
    // 是客户端在响应头到达之前就走了 —— hyper 丢掉整个 handler，这个变量
    // 跟着被丢掉，由 Drop 报成取消。
    //
    // 交给管线里每一条 `return Err` 各自去报的话，漏掉一条的后果不是没报，
    // 而是被 Drop 报成「客户端取消」—— 一次策略拒绝会记到客户端头上。
    let mut ending: Option<crate::ending::Ending> = None;
    let result = pipeline(
        state,
        rt,
        uri,
        query,
        headers,
        body,
        client_name,
        api,
        dialect,
        started,
        live,
        &mut ending,
    )
    .await;
    if let Some(end) = ending.take() {
        match &result {
            Err(e) => end.failed(e.source.slug(), e.detail.clone()),
            // 成功的路径都把结局交给了响应体，**走到这里是漏交了**。那也只能
            // 按拿到的状态码报结束 —— 不能让它掉在地上，被记成一次取消
            Ok(resp) => end.finished(resp.status().as_u16()),
        }
    }
    // **在一个地方给方言，而不是在每个 return 点。**后者只要漏一处，
    // 那条路径上的客户端就会收到一个它解析不了的 body，而那个失败看
    // 起来和真实原因毫无关系。
    result.map_err(|e| e.in_dialect(dialect))
}

/// 熔断状态变了就报一条，没变什么都不做。
///
/// **用自己的 id，不用这次请求的。**挂上请求的 id 会让存储层把它当成
/// 那次请求的一部分 —— 而熔断说的是「这家上游现在什么情况」，和触发它
/// 的那一次请求已经没关系了。
///
/// 开的时候顺手排一个定时器：**冷却到点本身就是一次状态变化**，而它
/// 不由任何调用触发。不报的话，界面会一直显示「熔断中」，直到碰巧又有
/// 一个请求打到这家为止。
///
/// 定时器会**自己续期**，因为熔断期间的每一次失败都会把冷却顶到更晚
/// （全都熔断时我们是放行的，所以那些失败照样打得到这家）。只睡一次
/// 就下结论的话，它醒来时看到的是一段还没走完的冷却 —— 然后闭嘴，而
/// 真正到点的那一刻再也没有人报。
fn note_health(
    bus: &tw_observe::EventBus,
    health: &Arc<Health>,
    provider: &str,
    change: Option<crate::health::State>,
) {
    let Some(next) = change else { return };
    let say = |bus: &tw_observe::EventBus, name: String, open: bool| {
        let id = bus.next_id();
        bus.emit(tw_api::Event::HealthChanged {
            id,
            provider: name,
            state: if open { "open" } else { "closed" }.into(),
            at_ms: now_ms(),
        });
    };
    let open = next == crate::health::State::Open;
    say(bus, provider.to_string(), open);
    if !open {
        return;
    }
    let (bus, health, name) = (bus.clone(), health.clone(), provider.to_string());
    tokio::spawn(async move {
        let mut left = crate::health::COOLDOWN;
        loop {
            tokio::time::sleep(left).await;
            match health.cooldown_left(&name) {
                // 中途成功过了 —— `record_success` 已经报过恢复
                None => return,
                Some(d) if d.is_zero() => return say(&bus, name, false),
                // 又失败了一次，冷却被顶后。接着等，别现在就说它好了
                Some(d) => left = d,
            }
        }
    });
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
    api: Option<crate::client_api::ClientApi>,
    dialect: crate::error::Dialect,
    started: std::time::Instant,
    live: crate::live::Pass,
    ending: &mut Option<crate::ending::Ending>,
) -> Result<Response, GatewayError> {
    forward::check_body_size(&body, MAX_BODY)?;

    // 管线第 1.3 步：客户端的自言自语。
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
                    probe: kind.slug().to_string(),
                    at_ms: now_ms(),
                });
                tracing::debug!(client = %client_name, kind = kind.slug(), "answered locally");
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
        return Err(GatewayError::config(msg!(
            "gw.config.no_upstreams" =>
            "No upstream is configured yet. Add one in ThinkWatch Lite, or under `providers` in \
             config.yaml."
        )));
    }

    // 管线第 2 步：路由。**规则引擎在这里** —— M0 那句「取第一个
    // provider」就是留给这一段的接缝。
    // **只解析一次。**路由要它，会话指纹也要它，而 body 可能有
    // 几百 KB —— 解两遍是白付一份钱。
    let parsed = serde_json::from_slice::<serde_json::Value>(&body).ok();
    // 生成回答的请求解码成中间表示，**四种格式的客户端读出同一份路由事实**。
    // body 解不开时用空的性质走兜底规则。**不要因此拒绝请求** —— 我们的解析器
    // 不认识的东西，上游可能完全认识（只有需要转换时才用得上解码结果）
    let reading = crate::client_api::read(uri.path(), query.as_deref(), parsed.as_ref());
    let generates = reading.generates;
    let decoded = reading.decoded;
    let facts = {
        let mut f = reading.facts;
        f.client = client_name.clone();
        f.intent = intent;
        f
    };
    // 管线第 1.5 步：模型准入。**和 `GET /v1/models` 共用同一个函数**
    // —— 列出来的一定能用，能用的一定列了出来。
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
            let servable = api.map(|a| crate::client_api::slugs(a.servable_by(generates)));
            if !catalog.admits(&facts.model, servable.as_deref(), allow.as_deref()) {
                // 错误信息要说清是哪一种：没有上游提供它，和这个客户端不让用它，
                // 该去改的地方不一样
                let why = if catalog.providers_for(&facts.model).is_empty() {
                    msg!(
                        "gw.model.no_upstream", model = facts.model.clone() =>
                        "No upstream serves model {model}. GET /v1/models lists the models that \
                         are available."
                    )
                } else {
                    msg!(
                        "gw.model.not_allowed", key = client_name.clone(), model = facts.model.clone() =>
                        "Gateway key `{key}` may not use model {model}. GET /v1/models lists the \
                         models that are available."
                    )
                };
                return Err(GatewayError::new(crate::error::Source::Request, why));
            }
        }
    }

    let mut decision = match rt.engine.route(&facts).map_err(|e| {
        GatewayError::config(msg!("gw.route.failed", detail = e => "Routing failed: {detail}"))
    })? {
        tw_engine::Outcome::Route(d) => d,
        tw_engine::Outcome::Deny { rule, reason } => {
            // **带理由的拒绝。**一个没有理由的拒绝，和一个 bug，在用户
            // 眼里没有区别。
            tracing::info!(%rule, "a rule denied the request");
            return Err(GatewayError::denied(msg!(
                "gw.route.denied", rule = rule, reason = reason =>
                "Rule `{rule}` denied this request: {reason}"
            )));
        }
    };
    // 去掉服务不了这个请求的候选：停用的、范围外的、清单里没有这个模型的。
    // **在排序之前** —— `cheapest` 和 `url-test` 要在能服务的上游里挑。
    //
    // 不跳过的话，一家没有这个模型的上游排在前面，它回的 404 不触发故障
    // 转移，请求就在一家能服务它的上游旁边失败了。
    let serving = crate::models::serving(
        &rt.config,
        &state.catalog.load(),
        &decision.candidates,
        &facts.model,
    );
    if serving.usable.is_empty() {
        return Err(serving.explain(&facts.model));
    }
    if !serving.skipped.is_empty() {
        tracing::debug!(skipped = ?serving.skipped, model = %facts.model, "skipping the candidates that cannot serve this request");
    }
    decision.candidates = serving.usable;
    // 策略组排序。**引擎给的是集合，顺序在这儿定** ——
    // 因为 `load-balance` / `url-test` / `cheapest` 都要运行时的数字，
    // 而路由决策本身必须是纯的、可试算的。
    //
    // `fallback` 和 `select` 走不到这里面 —— 那是绝大多数人的配置，
    // 它们连一个 HashMap 都不用建。
    if let Some(gname) = decision.via_group.clone()
        && let Some(kind) = rt
            .engine
            .groups()
            .iter()
            .find(|g| g.name == gname)
            .map(|g| g.kind)
        && kind.needs_runtime()
    {
        let session = parsed.as_ref().and_then(crate::session::fingerprint);
        let facts_rt = tw_engine::Facts {
            seq: state.bus.peek_id(),
            ttfb_ms: match kind {
                tw_engine::GroupType::UrlTest => state.latency.snapshot(&decision.candidates),
                _ => Default::default(),
            },
            price: match kind {
                tw_engine::GroupType::Cheapest => {
                    state.unit_prices(&rt.config.providers, &decision.candidates, &facts.model)
                }
                _ => Default::default(),
            },
            session,
        };
        decision.candidates = rt
            .engine
            .order(Some(&gname), &decision.candidates, &facts_rt);
    }

    // 管线第 3 步：准入。**排队而不是拒绝** —— 客户端收到 429
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
        .map_err(|e| {
            GatewayError::new(
                crate::error::Source::Overloaded,
                msg!("gw.overloaded", detail = e => "{detail}"),
            )
        })?;

    // 熔断过滤。**只有一个候选时完全旁路**，全都熔断时 fail-open ——
    // 两条边界都在 `Health::filter` 里，理由写在那儿。
    let (alive, fail_open) = state.health.filter(&decision.candidates);
    if fail_open {
        tracing::warn!(
            candidates = ?decision.candidates,
            "every candidate upstream is open-circuited; trying them anyway (fail-open)"
        );
    }

    let id = state.bus.next_id();
    state.bus.emit(tw_api::Event::RequestStarted {
        id,
        client: client_name.clone(),
        // **旁证，不是身份。**只用来显示和判断「接管生效了吗」，
        // 不参与鉴权、路由、配额（见 crate::hint）。
        client_hint: crate::hint::client_hint(&headers),
        // 认出「这几十个请求是同一次任务」。**认不出来就是
        // None** —— 硬凑一个会把互不相干的请求并成一个「会话」
        session_fp: parsed.as_ref().and_then(crate::session::fingerprint),
        provider: alive.first().map(|s| s.as_str()).unwrap_or("?").to_string(),
        model: facts.model.clone(),
        method: "POST".to_string(),
        path: uri.path().to_string(),
        at_ms: now_ms(),
    });
    // **发了开始，就欠一个结局。**从这一行起，这里返回的错误由调用方报成
    // 失败，这个 future 被丢掉由 Drop 报成取消（见 `passthrough`）。
    let sink = state.body_sink();
    let at_ms = now_ms() as i64;
    *ending = Some(crate::ending::Ending::new(
        state.bus.clone(),
        id,
        facts.model.clone(),
        started,
        at_ms,
        sink.clone(),
    ));

    // 出站脱敏：按全局的规则看一遍客户端发来的原文。**观察档和拦截档报的
    // 是同一条记录**，差别只在换没换 —— 真正的替换在每一跳发出去之前做，
    // 那一跳的请求体可能是转换过格式的。
    let redact_mode = rt.config.security.redact.mode;
    let found = crate::guard::find(redact_mode, &rt.redact, &body);
    if !found.is_empty() {
        state.bus.emit(tw_api::Event::SecretsFound {
            id,
            provider: alive.first().map(|s| s.to_string()).unwrap_or_default(),
            replaced: redact_mode.acts(),
            items: crate::guard::items(&found),
            at_ms: now_ms(),
        });
    }

    // 请求体交给观测层。**这时候它已经完整在内存里了**，所以这一步
    // 除了一次 `Bytes` 的引用计数之外没有别的成本（说过入站是要
    // 整个解析的，所以本来就在）。
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

    // 依次尝试。**首字节之前可以透明切换** —— 拿到响应头之前
    // 我们还没往客户端写过任何东西，换一家客户端完全无感。
    //
    // 「尝试链」要留下来：用户能看见故障转移在替他工作，**这是信任的
    // 来源**。一个静默切换过的请求和一个一次就成的请求，在用户眼里
    // 应该是不同的。
    let mut attempts: Vec<String> = Vec::new();
    // 成功那一次的脱敏账本。**必须是成功那一次的** —— 每一跳发出去的体可能
    // 转换过格式，占位符按那一份的顺序编号
    let mut used_ledger = tw_redact::redact::Ledger::default();
    // 成功那一跳的转换。**必须是成功那一次的** —— 故障转移从 Anthropic 上游
    // 切到 OpenAI 上游时，两跳转成的格式不一样；直通时是 None
    let mut used_session: Option<tw_dialect::convert::Session> = None;
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
            last_err = Some(GatewayError::config(msg!(
                "gw.route.selected_upstream_missing",
                rule = decision.matched_rule.clone(), upstream = name =>
                "Rule `{rule}` selected upstream `{upstream}`, which is not in the configuration."
            )));
            continue;
        };
        attempts.push(provider.name.clone());
        hop_started = std::time::Instant::now();

        // 生成回答以外的接口（计 token、嵌入……）没有别的格式可以转换，只能交给同格式
        // 的上游。**不发出去**：打到别家的同名路径上，好的情况是 404，坏的情况是被
        // 当成另一个接口执行
        if !generates
            && let (Some(a), Some(p)) = (api, provider.effective_protocol())
            && a.protocol() != p
        {
            let why = format!(
                "{} can only be served by a {} upstream",
                uri.path(),
                a.slug()
            );
            chain.push(hop_failed(&provider.name, why.clone(), hop_started));
            last_err = Some(GatewayError::new(
                crate::error::Source::Request,
                msg!(
                    "gw.route.protocol_mismatch",
                    path = uri.path(), wanted = a.slug(),
                    upstream = provider.name.clone(), got = p.slug() =>
                    "{path} can only be served by a {wanted} upstream, and `{upstream}` is {got}."
                ),
            ));
            continue;
        }

        // 阶段二：知道走哪家了，再跑一遍含 `provider_would_be` 的规则。
        //
        // **在循环里面，因为故障转移换了 provider 之后必须重算**。
        // 否则「走中转的一律脱敏」这条规则，在从官方转移到中转时会漏掉
        // —— 而那正是最需要它的时刻。
        let effective_set = match rt.engine.phase_two(&facts, &provider.name, &decision.set) {
            Ok(tw_engine::Outcome2::Proceed(s)) => s,
            Ok(tw_engine::Outcome2::Deny { rule, reason }) => {
                tracing::info!(%rule, provider = %provider.name, "a phase-two rule denied the request");
                return Err(GatewayError::denied(msg!(
                    "gw.route.denied", rule = rule, reason = reason =>
                    "Rule `{rule}` denied this request: {reason}"
                )));
            }
            Err(e) => {
                return Err(GatewayError::config(msg!(
                    "gw.route.rule_failed", detail = e => "A rule could not be evaluated: {detail}"
                )));
            }
        };
        // 方言互转。**同格式时是 None，这一整段零成本**
        let client_dialect = api.map(|a| a.dialect());
        let target = crate::translate::plan(api, generates, provider.effective_protocol());
        // ChatGPT 账号（Codex 后端）：只收流式、不认输出上限、身份头由网关填
        let chatgpt =
            generates && provider.effective_protocol() == Some(tw_config::Protocol::Chatgpt);
        let mut path = uri.path().to_string();
        let mut upstream_query = query.clone();
        let mut prepared: Option<tw_dialect::convert::Prepared> = None;
        // 直通到 Codex 后端、客户端却要整包时，收齐流要用的会话
        let mut collect_session: Option<tw_dialect::convert::Session> = None;
        let outbound = match target {
            None => {
                // 参数改写。**只在这里动 body，而且只动被点名的那几个字段** ——
                // 出站直通说过任何 body 改写都可能是缓存杀手，所以这是
                // 一个用户显式要求的例外，不是默认行为。
                let out = forward::apply_set(&body, &effective_set, client_dialect);
                if let (Some(tw_dialect::ir::Dialect::Gemini), Some(m)) =
                    (client_dialect, &effective_set.model)
                {
                    path = forward::gemini_path_with_model(&path, m);
                }
                // 对话之前被转换过、这一跳直通时，去掉客户端带回来的转换签名：
                // 这个上游不认，整个请求会被拒
                let out = match client_dialect
                    .filter(|_| generates)
                    .and_then(|d| tw_dialect::convert::strip_carried(d, &out))
                {
                    Some(b) => Bytes::from(b),
                    None => out,
                };
                if chatgpt {
                    // **只动 Codex 后端不认的那几个字段**，其余原样发（见 `chatgpt` 模块）
                    let (shaped, dropped) = crate::chatgpt::shape_passthrough(&out);
                    if !dropped.is_empty() {
                        let responses = tw_dialect::ir::Dialect::Responses.slug();
                        state.bus.emit(tw_api::Event::Translated {
                            id,
                            provider: provider.name.clone(),
                            from: responses.into(),
                            to: responses.into(),
                            dropped,
                            at_ms: now_ms(),
                        });
                    }
                    // 客户端要整包，后端只给流：由网关收齐。收齐要知道客户端的格式，所以要一个会话
                    if let Some(Ok(d)) = &decoded
                        && !d.request.stream
                    {
                        collect_session = Some(
                            d.clone()
                                .encode(&tw_dialect::ir::Target {
                                    dialect: tw_dialect::ir::Dialect::Responses,
                                    official: provider.is_official_endpoint(),
                                    default_max_tokens: 0,
                                })
                                .session,
                        );
                    }
                    shaped
                } else {
                    out
                }
            }
            Some(dialect) => {
                let d = match &decoded {
                    Some(Ok(d)) => d,
                    other => {
                        // 转换不了就换下一家：同格式的上游可能还在后面
                        let why = match other {
                            Some(Err(rej)) => rej.0.clone(),
                            _ => "the request body is not valid JSON".to_string(),
                        };
                        chain.push(hop_failed(
                            &provider.name,
                            format!("could not be converted to {}: {why}", dialect.slug()),
                            hop_started,
                        ));
                        last_err = Some(GatewayError::new(
                            crate::error::Source::Request,
                            msg!(
                                "gw.convert.failed", upstream = provider.name.clone(), detail = why =>
                                "The request could not be converted to the format upstream \
                                 `{upstream}` speaks: {detail}"
                            ),
                        ));
                        continue;
                    }
                };
                let mut d = d.clone();
                crate::translate::apply_set(&mut d.request, &effective_set);
                // Codex 后端不认输出上限：带着它发过去是一个 400
                let limit = if chatgpt {
                    crate::chatgpt::drop_output_limit(&mut d.request, d.client)
                } else {
                    None
                };
                let p = d.encode(&tw_dialect::ir::Target {
                    dialect,
                    official: provider.is_official_endpoint(),
                    default_max_tokens: crate::translate::default_max_tokens(
                        &state.pricing.load(),
                        &d.request.model,
                    ),
                });
                // **转换了就要说一声，丢了字段更要说。**用户会发现「扩展思考开了
                // 却没生效」而完全不知道从哪儿查起
                let mut dropped = p.dropped.clone();
                dropped.extend(limit);
                state.bus.emit(tw_api::Event::Translated {
                    id,
                    provider: provider.name.clone(),
                    from: d.client.slug().into(),
                    to: dialect.slug().into(),
                    dropped,
                    at_ms: now_ms(),
                });
                path = p.path.clone();
                upstream_query = p.query.clone();
                // **客户端要不要流由会话记着**，发给 Codex 后端的这一份一律是流式
                let body = if chatgpt {
                    Bytes::from(crate::chatgpt::force_stream(p.body.clone()))
                } else {
                    Bytes::from(p.body.clone())
                };
                prepared = Some(p);
                body
            }
        };

        if chatgpt {
            // Codex 后端的生成接口是 `{base}/responses`，不在 `/v1` 下
            path = "/responses".to_string();
            upstream_query = None;
        }

        // 出站脱敏的拦截档：换掉**这一跳真正发出去的那一份**（可能转换过
        // 格式）。规则是全局的，每一跳换掉的是同一批东西
        let (outbound, ledger) = crate::guard::replace(redact_mode, &rt.redact, outbound);

        // 用这个 provider 自己的 Client —— 它带着该走的代理。**在取密钥
        // 之前拿到**：OAuth 换 token 也要走这条代理。
        let http = rt.clients.get(&provider.name).unwrap_or(&state.http);
        let upstream_headers = match state.headers_for(provider, http, Some(&client_name)).await {
            Ok(h) => h,
            Err(e) => {
                // 密钥取不到是这一家的问题（环境变量没设、token 端点
                // 连不上），换下一家是合理的 —— 而且**必须**换：不换的话
                // 一家 OAuth 上游的 token 端点抽风会让整个网关不可用。
                note_health(
                    &state.bus,
                    &state.health,
                    &provider.name,
                    state.health.record_failure(&provider.name),
                );
                chain.push(hop_failed(
                    &provider.name,
                    format!("the credential could not be obtained: {e}"),
                    hop_started,
                ));
                last_err = Some(GatewayError::config(msg!(
                    "gw.credentials.failed", upstream = provider.name.clone(), detail = e =>
                    "The credential for upstream `{upstream}` could not be obtained: {detail}"
                )));
                continue;
            }
        };
        let url = forward::upstream_url(&provider.base_url, &path, upstream_query.as_deref());
        let method = reqwest::Method::from_bytes(b"POST").expect("POST is a valid method");

        tracing::debug!(
            client = %client_name,
            provider = %provider.name,
            rule = %decision.matched_rule,
            group = ?decision.via_group,
            url = %tw_secret::redact_url(&url),
            attempt = attempts.len(),
            "forwarding"
        );

        let required = target
            .map(crate::translate::required_headers)
            .unwrap_or_default();
        let build = |upstream_headers: &[(String, String)]| {
            let mut req = http.request(method.clone(), &url);
            req = forward::forward_headers_filtered(req, &headers, |n| {
                let own = match (target, client_dialect) {
                    (Some(_), Some(c)) => !crate::translate::keeps_header(c, n),
                    _ => false,
                };
                // 请求来自谁由网关如实填写：客户端报的来源（比如 Codex CLI 的 originator）
                // 不转发，请求经过的是 ThinkWatch
                let identity = chatgpt && !crate::chatgpt::keeps_client_header(n);
                !own && !identity
                    && !required.iter().any(|(k, _)| k.eq_ignore_ascii_case(n))
                    && !forward::overridden(upstream_headers, n)
            });
            // 目标格式必需的头（Anthropic 的 anthropic-version）。上游配置里写了同名头时以配置为准
            for (k, v) in required {
                if !forward::overridden(upstream_headers, k) {
                    req = req.header(*k, *v);
                }
            }
            req = forward::apply_headers(req, upstream_headers);
            if chatgpt {
                req = forward::apply_headers(
                    req,
                    &crate::chatgpt::identity_headers(&headers, upstream_headers),
                );
            }
            req
        };
        let sent_at = std::time::Instant::now();
        let mut sent = build(&upstream_headers).body(outbound.clone()).send().await;
        // **OAuth 上游回 401：换一个 access token 再发一次，只一次。**token 可能在别处被
        // 吊销了、提前失效了；不重试的话，这个请求连同之后每一个请求都会原样失败，直到
        // 缓存里那个 token 按时间过期。换回来的还是 401，说明问题不在 token
        if provider.oauth.is_some() && matches!(&sent, Ok(r) if r.status() == 401) {
            match state
                .headers_after_401(provider, http, Some(&client_name), sent_at)
                .await
            {
                // 换回来的还是同一个（刚换过不久）：再发一次也是 401
                Ok(fresh) if fresh != upstream_headers => {
                    sent = build(&fresh).body(outbound.clone()).send().await;
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(provider = %provider.name, "the upstream answered 401 and the token could not be renewed: {e}")
                }
            }
        }
        match sent {
            Ok(r) if r.status().is_server_error() || r.status() == 429 => {
                // 额度用完时上游回的正是 429，这一跳的额度头也要读
                state.note_quota(id, &provider.name, r.headers());
                // 上游回了话，说明代理是通的
                state.note_proxy_ok(&provider.proxy);
                // 5xx 和限流：换一家有意义，那边可能有不同的额度或地域。
                // **4xx 不换**（除了 429）—— 请求本身有问题的话，换一家
                // 也一样被拒，还会白白污染那家的健康度。
                note_health(
                    &state.bus,
                    &state.health,
                    &provider.name,
                    state.health.record_failure(&provider.name),
                );
                chain.push(hop(
                    &provider.name,
                    "status",
                    r.status().as_u16(),
                    hop_started,
                ));
                // **429 要保住 429。**塌成 502 的话，客户端会当成「服务器
                // 坏了」而不是「该退避了」，而它们该做的事完全不同。
                last_err = Some(if r.status() == 429 {
                    GatewayError::rate_limited(msg!(
                        "gw.upstream.rate_limited", upstream = provider.name.clone() =>
                        "Upstream `{upstream}` rate-limited the request."
                    ))
                } else {
                    GatewayError::upstream(msg!(
                        "gw.upstream.status", upstream = provider.name.clone(), status = r.status().as_u16() =>
                        "Upstream `{upstream}` answered {status}."
                    ))
                });
                continue;
            }
            Ok(r) => {
                note_health(
                    &state.bus,
                    &state.health,
                    &provider.name,
                    state.health.record_success(&provider.name),
                );
                chain.push(hop(
                    &provider.name,
                    "served",
                    r.status().as_u16(),
                    hop_started,
                ));
                upstream = Some(r);
                used = Some(provider);
                used_ledger = ledger;
                used_session = prepared.map(|p| p.session).or(collect_session);
                break;
            }
            Err(e) => {
                note_health(
                    &state.bus,
                    &state.health,
                    &provider.name,
                    state.health.record_failure(&provider.name),
                );
                // 连不上的可能是代理而不是上游 —— 检一次那个代理，说清是哪一件事
                state.check_proxy(&provider.proxy);
                let err = forward::map_reqwest_error(e);
                chain.push(hop_failed(
                    &provider.name,
                    err.message().to_string(),
                    hop_started,
                ));
                last_err = Some(err);
                continue;
            }
        }
    }

    // 尝试链走完了，两条路都要发 —— 挂在 RequestFinished 上的话，
    // 失败那条路就没有尝试链，而那恰恰是最需要看它的时候。
    // 最终服务的那家怎么收钱。**跟着请求走，不能事后查配置** ——
    // 配置随时会被热重载，而一条三天前的记录该按它当时那家的算。
    let billing = used
        .map(|p| state.billing_of(p))
        .unwrap_or(tw_config::Billing::PerToken);
    state.bus.emit(tw_api::Event::RequestRouted {
        id,
        rule: decision.matched_rule.clone(),
        group: decision.via_group.clone(),
        attempts: chain,
        billing: billing.slug().to_string(),
    });

    let (Some(upstream), Some(provider)) = (upstream, used) else {
        let mut err = last_err.unwrap_or_else(|| {
            GatewayError::config(msg!("gw.route.no_upstream_alive" => "No upstream is available."))
        });
        // **尝试链要进客户端看到的那条错误**，不只进我们的事件流 ——
        // 用户看的是他自己终端里的报错。一条说「试过 A → B → C 都不行」
        // 的错误，和一条只说「503」的错误，是两种产品：前者说明我们替
        // 他做了工作，后者让他以为我们什么都没干。
        if attempts.len() > 1 {
            err = err.with_attempts(&attempts);
        }
        // 失败也必须有结局。少了它，UI 上那一行会永远停在「进行中」——
        // 而「一直转圈」比「明确失败」更让人怀疑是我们卡住了。它由
        // `passthrough` 按这里返回的错误报出去，带着上面那串尝试链，
        // `source` 也是这个错误自己的（被限流的就是 `rate_limited`）。
        return Err(err);
    };
    if attempts.len() > 1 {
        tracing::info!(
            chain = %attempts.join(" → "),
            "failed over; {} answered in the end",
            provider.name
        );
    }

    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    // 响应头到手就发一次。**这个事件单独存在是有意的**：流式请求从这里
    // 到结束可能还有好几分钟，UI 要能在这个点就把行画出来并标「进行中」，
    // 而不是等它结束才出现。
    let ttfb_ms = started.elapsed().as_millis() as u64;
    // `url-test` 的判据。**只记成功的那些** —— 一个 500 在
    // 十毫秒内返回，会让最坏的上游看起来最快
    if status.is_success() {
        state
            .latency
            .record(&provider.name, ttfb_ms.min(u32::MAX as u64) as u32);
    }
    state.bus.emit(tw_api::Event::RequestHeaders {
        id,
        status: status.as_u16(),
        ttfb_ms,
    });
    // 订阅额度。**零成本** —— 这些头本来就在响应里，读一下
    // 就有了。按量付费的账号没有它们，那时什么都不发。
    state.note_quota(id, &provider.name, upstream.headers());
    // 上游收不收我们的凭据、经过的代理通不通：**都是状态变化，各只报一次**
    state.note_auth(&provider.name, status.as_u16());
    state.note_proxy_ok(&provider.proxy);

    let mut out_headers = forward::response_headers(upstream.headers());
    // **哪一家服务的，写在头上。**错误契约要求上游的错误原样透传、不加
    // `[ThinkWatch]` 前缀 —— 那确实是它说的话。可上游的 401 说的是
    // 「invalid x-api-key」，而用户手里有两把 key（网关的和上游的），
    // 他会去查错的那一把。改 body 是越界，加一个头不是。
    if let Ok(v) = axum::http::HeaderValue::from_str(&provider.name) {
        out_headers.insert("x-thinkwatch-upstream", v);
    }

    // 流式：**不缓冲**。整块缓冲会把 SSE 变成一次性交付，客户端那边
    // 看起来就是「卡住很久然后一下全出来」。
    let bus = state.bus.clone();
    // 流结束时才知道总字节数和真实耗时 —— 对一个跑了六分钟的任务，
    // 这两个数字在响应头那一刻都还不存在。
    //
    // **结局跟着流走，而不是只写在流的末尾。**客户端中途走掉时，末尾的
    // 代码一行都不会执行，而上游已经为这次请求计了费（见 `crate::ending`）。
    let mut ending = ending
        .take()
        .expect("written when the start event was emitted");
    ending.responded(status.as_u16());
    let chunks = upstream.bytes_stream();
    // **中途断掉不能只是让流消失。**首字节已经发出去了，状态码和响应头
    // 都改不了，而一个戛然而止的 SSE 流和一个正常结束的流在客户端看来
    // 长得一模一样 —— 用户会以为模型就答了这么多。唯一还能说话的地方
    // 是流本身，所以补一个 `event: error` 帧。
    //
    // **Codex 后端的流式响应没有 Content-Type**（实测）。发给它的请求一定是流式的，所以
    // 成功的响应就是 SSE；不补上的话，转换、回显还原和工具调用审查都会当成整包处理。
    if generates
        && provider.effective_protocol() == Some(tw_config::Protocol::Chatgpt)
        && status.is_success()
        && !out_headers.contains_key(axum::http::header::CONTENT_TYPE)
    {
        out_headers.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/event-stream"),
        );
    }
    let is_sse = out_headers
        .get(axum::http::header::CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"text/event-stream"));
    // 转换的回程。**流式成功的边收边转**；整包、以及上游返回的错误，整个到手再转 ——
    // 上游的错误体要换成客户端认得的错误格式，否则客户端连原因都解析不出来
    let session = used_session;
    // 客户端要整包、上游给的是流：**收齐之后写一个客户端格式的整包。**以前这种情况把转换
    // 出来的流标成 `application/json` 发过去，客户端解析不了
    let collect = session.as_ref().is_some_and(|s| !s.stream) && is_sse && status.is_success();
    let convert_stream = session.is_some() && is_sse && status.is_success() && !collect;
    let convert_whole = session.is_some() && !convert_stream && !collect;
    if let Some(s) = &session {
        // 客户端要流而上游给了整包时，整包会被写成客户端格式的流
        let writes_stream = convert_stream || (status.is_success() && s.stream);
        let ct = if writes_stream {
            s.content_type()
        } else {
            "application/json"
        };
        out_headers.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static(ct),
        );
    }
    // 回显还原。
    //
    // **SSE 和非流式走两套**：前者的占位符散落在几十帧里（模型按 token
    // 吐字，一个 `<<TW_SECRET_1>>` 会被切成五到八段），后者整个躺在一份
    // JSON 里。
    //
    // **没脱敏过就是个空壳**，`process` 直接把字节原样递出去 —— 绝大多数
    // 请求走的是这条路，它不该为这个功能付任何延迟。
    let mut restorer = tw_redact::sse::Body::new(&used_ledger, is_sse);
    // 流式转换器。**同格式时是 None，整段零成本。**
    //
    // 位置在还原**之后**：占位符是我们在出站时塞进去的，先换回真值再
    // 转换，转换器看到的就和上游原话一样了。
    let mut back = if convert_stream {
        session.as_ref().map(|s| s.stream())
    } else {
        None
    };
    // 客户端要整包、上游给流时的收集器
    let mut collector = if collect {
        session.as_ref().map(|s| s.collector())
    } else {
        None
    };
    // 客户端收到的是不是 SSE。Gemini 客户端不带 `alt=sse` 时是一个 JSON 数组
    let client_sse = session
        .as_ref()
        .map_or(is_sse, |s| convert_stream && s.client_sse());
    // 客户端收到的是不是那个 JSON 数组流。**它也是边收边发的**：以前工具调用审查
    // 只认 SSE，不带 `alt=sse` 的 Gemini 客户端收到的工具调用一个都没查过
    let client_json_stream = status.is_success()
        && match &session {
            Some(s) => convert_stream && !s.client_sse(),
            None => {
                !is_sse
                    && api == Some(crate::client_api::ClientApi::Gemini)
                    && uri
                        .path()
                        .trim_end_matches('/')
                        .ends_with(":streamGenerateContent")
            }
        };
    // 工具调用防火墙。
    //
    // **流式和非流式走两套，但两套都跑。**流式是边流边扫、命中就切，
    // 立足点是「不完整的工具调用执行不了」；非流式没有这个立足点，
    // 却有一个更强的条件 —— 整份 body 到手时一个字节都还没发出去，
    // 所以整份看完再决定发不发。
    //
    // 以前非流式这一支根本不建审查器。代价有两条：观察档对非流式
    // 客户端一条都不记（而界面上写的是「照常检测、照常记录」），
    // 拦截档更是被整个绕过去。
    // **对所有上游一样**：切不切只看档位和规则的处置，不看上游是不是官方的
    let inspect = rt.config.security.inspect_tools.mode;
    let whole_body = !client_sse && !client_json_stream;
    let mut wall = if !inspect.detects() {
        None
    } else if client_sse {
        Some(crate::toolwall::Wall::new(rt.tools.clone()))
    } else if client_json_stream {
        Some(crate::toolwall::Wall::json_array(rt.tools.clone()))
    } else {
        Some(crate::toolwall::Wall::json_body(rt.tools.clone()))
    };
    /*
      非流式要拦得住，body 就不能边收边发 —— 发出去了就收不回来。

      **这不是把流变成一次性交付**：客户端要的本来就是一整份 JSON，
      它无论如何都得等完整 —— 它那边的 HTTP 栈同样要收齐才交给调用者。
      整包转换（`convert_whole`）和整包收集（`collect`）本来就在攒，
      只有剩下那条直通的路需要这一下。

      **不设大小上限。**设了就是一条绕过去的路：往响应里塞几 MB 无害
      内容把体积顶过阈值，后面的工具调用就再也不会被看到了。而一次
      非流式回答的体积由 `max_tokens` 封顶，十几万输出 token 也就几百
      KB —— 真正的风险不在这儿。整包转换那条路本来也是不封顶的。
    */
    let hold = wall.is_some() && whole_body && !convert_whole && !collect;
    let wall_provider = provider.name.clone();
    let stream = async_stream::stream! {
        // **通行证跟着响应体走。**这个流被丢掉的时候它才还回去：正常
        // 发完是一种，客户端中途断开、hyper 丢掉响应体是另一种 —— 两种
        // 都算这个请求结束了。
        let _live = live;
        // 结局也一样：流被丢掉的时候，它替流报「客户端取消」。
        let mut ending = ending;
        let mut chunks = std::pin::pin!(chunks);
        let mut broke: Option<GatewayError> = None;
        let mut whole: Vec<u8> = Vec::new();
        while let Some(item) = chunks.next().await {
            match item {
                Ok(chunk) => {
                    // **旁路嗅探和留档，不缓冲**：字节照常流向客户端，同时
                    // 喂它一份。上游返回的 usage 是真相，而拿不到它就只能估。
                    //
                    // 看的是上游原话（带占位符的那一版）：usage 数字不受
                    // 影响，而请求详情里存的正是「我们发出去的和收回来的」，
                    // 把还原后的存进去会让那一页说谎。
                    ending.feed(&chunk);
                    let out = restorer.process(&chunk);
                    // 翻译在还原之后、审查之前：**审查看的必须是客户端
                    // 将要拿到的那一版**，而那一版是翻译过的
                    let out = match back.as_mut() {
                        Some(c) => c.process(&out),
                        None => out,
                    };
                    // 整包那一条整个攒起来，最后转一次。**这不是缓冲流** ——
                    // 整包响应本来就是一整个 body，客户端无论如何都要等它完整
                    // （说的是别把 SSE 变成一次性交付，这里没有 SSE）
                    if convert_whole || hold {
                        whole.extend_from_slice(&out);
                        continue;
                    }
                    if let Some(c) = collector.as_mut() {
                        c.process(&out);
                        continue;
                    }
                    // **审查的是客户端将要看到的那一版**（还原之后的），
                    // 因为那才是它真正会去执行的东西
                    let mut cut: Option<(GatewayError, usize)> = None;
                    if let Some(w) = wall.as_mut() {
                        for v in w.feed(&out) {
                            // 规则是切断 + 拦截档 = 切断
                            let blocked = v.cut && inspect.acts();
                            bus.emit(flagged(id, &wall_provider, &v, blocked));
                            if blocked {
                                tracing::warn!(
                                    provider = %wall_provider, tool = %v.tool, rule = %v.rule,
                                    "cut the response stream: the upstream returned a dangerous tool call"
                                );
                                cut = Some((
                                    GatewayError::denied(msg!(
                                        "gw.toolcall.cut",
                                        upstream = wall_provider.clone(), tool = v.tool.clone(),
                                        rule = v.rule.clone(), why = v.why.clone() =>
                                        "The {tool} call returned by upstream `{upstream}` \
                                         matched rule `{rule}` ({why}), so the response was cut off."
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
                        // 完整的工具调用
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
        let tail = match (&session, back.as_mut()) {
            (Some(s), None) if convert_whole => {
                if broke.is_some() {
                    // 半截的整包转不出任何有意义的东西，由下面的错误收尾
                    Vec::new()
                } else {
                    whole.extend_from_slice(&tail);
                    if !status.is_success() {
                        s.error(status.as_u16(), &whole)
                    } else if s.stream {
                        s.stream_from_whole(&whole).unwrap_or_else(|| whole.clone())
                    } else {
                        // 转不动就原样交给客户端 —— 那是上游的原话，比我们编的任何
                        // 东西都有用
                        s.response(&whole).unwrap_or_else(|| whole.clone())
                    }
                }
            }
            (Some(s), None) if collect => match collector.take() {
                Some(mut c) if broke.is_none() => {
                    c.process(&tail);
                    match c.finish() {
                        Ok(body) => body,
                        // 上游在流里报了错：按客户端的格式说出来
                        Err(why) => tw_dialect::convert::error_body(s.client, 502, &why),
                    }
                }
                // 半截的流收不出完整的回答，由下面的错误收尾
                _ => Vec::new(),
            },
            // 攒着等整份看完的那条直通路：尾巴接上，整份交给下面
            _ if hold => {
                if broke.is_some() {
                    // 半截的整包交不出去，由下面的错误收尾
                    Vec::new()
                } else {
                    whole.extend_from_slice(&tail);
                    std::mem::take(&mut whole)
                }
            }
            (_, Some(c)) => {
                let mut t = c.process(&tail);
                // **收尾必须补上**：客户端等着结束帧（Anthropic 的 message_stop、
                // Chat 的 [DONE]），少了会一直等。中途断了的由下面按错误收尾
                if broke.is_none() {
                    t.extend(c.finish());
                }
                t
            }
            _ => tail,
        };
        /*
          非流式：**整份到手了才看得见工具调用，而它一个字节都还没发出去。**

          流式那条路只能「尽力阻断」—— 首字节早发了，能做的是从命中的
          那一帧起不再发。这里不一样：要么整份发出去，要么一份都不发，
          所以拦得干净。代价是状态码已经随响应头走了，改不动 —— body
          里换成错误体，和 `sse_frame` 在流上扮演的是同一个角色。
        */
        let mut denied: Option<GatewayError> = None;
        if whole_body
            && broke.is_none()
            && status.is_success()
            && let Some(w) = wall.as_mut()
        {
            for v in w.whole(&tail) {
                let blocked = v.cut && inspect.acts();
                bus.emit(flagged(id, &wall_provider, &v, blocked));
                if blocked {
                    tracing::warn!(
                        provider = %wall_provider, tool = %v.tool, rule = %v.rule,
                        "withheld the response: the upstream returned a dangerous tool call"
                    );
                    denied = Some(GatewayError::denied(msg!(
                        "gw.toolcall.blocked",
                        upstream = wall_provider.clone(), tool = v.tool.clone(),
                        rule = v.rule.clone(), why = v.why.clone() =>
                        "The {tool} call returned by upstream `{upstream}` matched rule \
                         `{rule}` ({why}), so the response was withheld."
                    )));
                    break;
                }
            }
        }
        if denied.is_none() && !tail.is_empty() {
            yield Ok::<Bytes, std::io::Error>(Bytes::from(tail));
        }
        // 扣下整份 body 和流断在半路，对结局来说是同一件事
        if let Some(err) = denied {
            broke = Some(err.in_dialect(dialect));
        }
        // 响应体留档和结束事件都在 `ending` 里：三种结局要交出去的是同一份
        // 东西，分开写就会有一种漏掉
        match broke {
            None => ending.finished(status.as_u16()),
            Some(err) => {
                // 少了这个事件，UI 上那一行会永远停在「进行中」——
                // 而「一直转圈」比「明确失败」更让人怀疑是我们卡住了。
                //
                // **先报再发错误帧**：客户端恰好在最后这一帧上走掉的话，
                // 结局已经报过了，不会再被记成一次取消。
                //
                // `source` 用这个错误自己的：上游断了是 `upstream`，被
                // 防火墙切断是 `denied` —— 后者不是上游坏了，是策略拦的。
                // 码保持不变，只在句子前面点明它断在流里 —— 界面认的是码
                let mut why = err.detail.clone();
                why.text = format!("the response stream broke: {}", why.text);
                ending.failed(err.source.slug(), why);
                if let Some(c) = back.as_mut() {
                    // 转换过的流按客户端的格式收尾
                    yield Ok(Bytes::from(c.fail(&format!("[ThinkWatch] {}", err.message()))));
                } else if let (true, Some(s)) = (collect, session.as_ref()) {
                    // 要收齐的整包一个字节都还没发：按客户端的格式回一个错误体
                    yield Ok(Bytes::from(tw_dialect::convert::error_body(
                        s.client,
                        502,
                        &format!("[ThinkWatch] {}", err.message()),
                    )));
                } else if is_sse && session.is_none() {
                    yield Ok(Bytes::from(err.sse_frame()));
                } else if hold {
                    // 整份攒着的那条路：body 还没发，整个换成错误体
                    yield Ok(Bytes::from(err.body_bytes()));
                }
            }
        }
    };
    let mut resp = Response::new(Body::from_stream(stream));
    *resp.status_mut() = status;
    *resp.headers_mut() = out_headers;
    Ok(resp)
}

/// 给 `url-test` 组的成员垫一个底（样本不够时用零成本的 L1 补）。
///
/// **只测 `url-test` 组里的那些，启动时和之后每天各测一次**（跟着模型清单
/// 的刷新一起跑，见 [`crate::models::spawn`]）。
/// 没有这一步的话，`url-test` 在攒够真实样本之前完全等同于 `fallback`
/// —— 用户配了「选最快的」，而头几十个请求全落在配置里排第一那家。
///
/// L1 是握手计时，不发一个 API 请求、不花一分钱；也**不是定期
/// 跑的** —— 真实流量一到就该由它说了算。
pub async fn seed_latency(state: &AppState) {
    let rt = state.runtime();
    let mut want: Vec<String> = Vec::new();
    for g in rt.engine.groups() {
        if g.kind == tw_engine::GroupType::UrlTest {
            want.extend(g.providers.iter().cloned());
        }
    }
    want.sort();
    want.dedup();
    if want.is_empty() {
        return;
    }
    for name in want {
        // 停用的不参与路由，用不着垫
        let Some(p) = rt
            .config
            .providers
            .iter()
            .find(|p| p.name == name && !p.disabled)
        else {
            continue;
        };
        // 走代理的那家要测它真正会走的那条路。`system` 测不了，
        // 那时不垫底 —— 假装直连测一遍给的数字，测的根本不是那条路
        let hop = match crate::l1::hop_for(&rt.config, p) {
            Ok(h) => h,
            Err(why) => {
                tracing::debug!(provider = %name, %why, "this upstream cannot be link-tested, so no latency sample is seeded");
                continue;
            }
        };
        let r = crate::l1::l1(&p.base_url, hop.as_ref()).await;
        if r.ok {
            tracing::debug!(provider = %name, ms = r.total_ms, "seeding a latency sample from the link test");
            state
                .latency
                .seed(&name, r.total_ms.min(u32::MAX as u64) as u32);
        } else {
            // **连都连不上的那家不垫。**它会因为「没样本」排在最后，
            // 而那正是对的
            tracing::debug!(provider = %name, "the link test could not connect, so no latency sample is seeded");
        }
    }
}

/// 起服务。返回实际绑定的地址 —— 端口写 0 时调用方需要知道拿到了哪个。
pub async fn serve(state: AppState, addr: std::net::SocketAddr) -> std::io::Result<()> {
    serve_once(state, addr, std::future::pending()).await
}

/// 起服务，**并且跟着配置里的监听地址走**（「温」）。
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
        let want = match state.runtime().config.listen.gateway.socket_addr() {
            Ok(a) => a,
            // **新配置的地址算不出来就守住旧的。**`bind` 指着一张刚被拔掉
            // 的网卡时，正确的动作不是把一个正在工作的监听器拆掉 —— 那会
            // 让所有客户端立刻断线，而它们本来好好的。
            Err(e) => {
                tracing::error!(%e, keeping = %next, "the new listen address cannot be resolved");
                continue;
            }
        };
        if want == next {
            // 通知来了但地址没变（比如又改回去了）—— 原样重来
            continue;
        }
        tracing::info!(from = %next, to = %want, "the listen address changed; rebuilding the listener");
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
        tracing::info!(%actual, "the gateway is listening (source allow-list in effect)");
    } else {
        tracing::info!(%actual, "the gateway is listening");
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

/// 上游回了话的一跳：`served` 或者 `status`。
fn hop(
    provider: &str,
    outcome: &str,
    status: u16,
    started: std::time::Instant,
) -> tw_api::AttemptView {
    tw_api::AttemptView {
        provider: provider.to_string(),
        outcome: outcome.to_string(),
        status: Some(status),
        error: None,
        ms: started.elapsed().as_millis() as u64,
    }
}

/// 没有收到响应的一跳。
fn hop_failed(provider: &str, error: String, started: std::time::Instant) -> tw_api::AttemptView {
    tw_api::AttemptView {
        provider: provider.to_string(),
        outcome: "error".to_string(),
        status: None,
        error: Some(error),
        ms: started.elapsed().as_millis() as u64,
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 这个请求是 Claude Code 发的吗。
///
/// **`max_tokens: 1` 那条判定必须同时要求它**，否则会误伤别人
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

/// 一次工具调用命中写成事件。流式、整包、WebSocket 三条路共用 —— 字段写漏
/// 一个，就有一条路上的日志说不清是哪条规则。
pub(crate) fn flagged(
    id: u64,
    provider: &str,
    v: &crate::toolwall::Verdict,
    blocked: bool,
) -> tw_api::Event {
    tw_api::Event::ToolCallFlagged {
        id,
        provider: provider.to_string(),
        tool: v.tool.clone(),
        rule: v.rule.clone(),
        custom: v.custom,
        why: v.why.clone(),
        excerpt: v.excerpt.clone(),
        action: if v.cut { "cut" } else { "record" }.to_string(),
        blocked,
        at_ms: now_ms(),
    }
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
                key: Some("sk-1".into()),
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
            key: Some("k".into()),
            proxy: proxy.into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_default_proxy_is_direct_not_system() {
        // 显式优于隐式。默认跟随系统的话，用户在系统里开了全局
        // 代理，本地 Ollama 就会莫名连不上 —— 而配置文件里看不出任何线索。
        assert_eq!(tw_config::Provider::default().proxy, tw_config::DIRECT);
    }

    #[test]
    fn the_two_builtin_proxy_names_need_no_declaration() {
        let cfg = tw_config::Config::default();
        assert!(client_for_provider(&cfg, &provider_with_proxy(tw_config::DIRECT)).is_ok());
        assert!(client_for_provider(&cfg, &provider_with_proxy(tw_config::SYSTEM)).is_ok());
    }

    #[test]
    fn an_undeclared_proxy_name_fails_at_startup_and_lists_the_builtins() {
        // 启动时报，不要等请求进来。而且要说清有哪两个内置名字 ——
        // 用户十有八九是想写 `direct`。
        let cfg = tw_config::Config::default();
        let e = client_for_provider(&cfg, &provider_with_proxy("airport")).unwrap_err();
        assert_eq!(e.detail.code, "gw.config.proxy_undefined");
        assert_eq!(e.detail.arg("proxy"), "airport");
        assert!(e.message().contains("direct"), "{}", e.message());
        // 行续接留下的缩进不该进错误信息
        assert!(
            !e.message().contains("   "),
            "错误信息里有多余空格：{}",
            e.message()
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
        assert!(client_for_provider(&cfg, &provider_with_proxy("airport")).is_ok());
    }

    #[test]
    fn each_provider_gets_its_own_client() {
        // reqwest 的代理绑在 Client 上，不能按请求切换。共用一个
        // Client 的话，「这家走代理、Ollama 直连」这个最基本的需求就做
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
                    key: Some("k".into()),
                    ..Default::default()
                },
                tw_config::Provider {
                    name: "b".into(),
                    base_url: "https://b.com".into(),
                    key: Some("k".into()),
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
        assert_eq!(pos, crate::auth::KeyPosition::AnthropicHeader);
    }

    #[test]
    fn no_key_is_rejected_and_the_message_says_where_to_put_one() {
        let s = AppState::new(cfg()).unwrap();
        let e = s.identify(&HeaderMap::new(), None).unwrap_err();
        assert_eq!(e.source, crate::error::Source::Auth);
        assert_eq!(e.detail.code, "gw.auth.no_key");
        assert!(e.message().contains("clients"));
    }

    #[test]
    fn an_unknown_key_is_rejected_without_echoing_it_back() {
        // 回显会给「猜密钥」这件事一个反馈信号，哪怕只是打码的。
        let s = AppState::new(cfg()).unwrap();
        let e = s.identify(&hdr("x-api-key", "tw-wrong"), None).unwrap_err();
        assert!(!e.message().contains("tw-wrong"));
        assert!(!e.message().contains("tw-wr"));
        // 参数里也不许有 —— 界面拿的是参数，不是那句话
        assert!(!e.detail.args.values().any(|v| v.contains("tw-wr")));
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
