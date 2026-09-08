//! HTTP 服务：路由、身份识别、转发。

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{OriginalUri, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{any, get};
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};

use crate::auth::{extract_key, key_eq};
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

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<tw_config::Config>,
    /// 路由引擎。**和配置一起建，一起换** —— 分开持有会让「规则改了但
    /// 引擎还是旧的」变成可能，而那种不一致完全静默。
    pub engine: Arc<tw_engine::Engine>,
    /// **每个 provider 一个 Client**。reqwest 的代理绑在 Client 上，
    /// 不能按请求切换（§3.7）—— 而这本来也是对的：连接池按上游隔离，
    /// 一个慢上游不会占着另一个的连接。
    pub clients: Arc<std::collections::HashMap<String, reqwest::Client>>,
    /// 探测和别的杂事用的默认 Client（不走代理）
    pub http: reqwest::Client,
    /// 观测事件往这里丢。没有订阅者时是零成本的 —— 数据面不该知道有
    /// 没有人在看。
    pub bus: tw_observe::EventBus,
    /// 上游健康。**不持久化** —— 重启后重置为未知（§4.2）。
    pub health: Arc<Health>,
    /// 并发闸门。排队不拒绝（§4.7）。
    pub gate: Arc<crate::limits::Gate>,
    /// 来源白名单。空 = 全放行，而那只在 loopback 下成立（§5.4）。
    pub allow: Arc<crate::access::AllowList>,
}

impl AppState {
    pub fn new(config: tw_config::Config) -> Result<Self, GatewayError> {
        let http = base_client_builder()
            .build()
            .map_err(|e| GatewayError::config(format!("HTTP 客户端建不起来：{e}")))?;
        let mut clients = std::collections::HashMap::new();
        for p in &config.providers {
            clients.insert(p.name.clone(), build_client(&config, p)?);
        }
        let limits = config.limits.clone();
        let allow = crate::access::AllowList::parse(&config.listen.gateway.effective_allow_from())
            .map_err(|e| GatewayError::config(format!("listen.gateway.allow_from：{e}")))?;
        let engine = Arc::new(config.engine());
        Ok(Self {
            clients: Arc::new(clients),
            engine,
            config: Arc::new(config),
            http,
            bus: tw_observe::EventBus::new(),
            health: Arc::new(Health::new()),
            gate: Arc::new(crate::limits::Gate::new(limits)),
            allow: Arc::new(allow),
        })
    }

    /// 密钥 → 客户端名字。
    fn identify(&self, headers: &HeaderMap, query: Option<&str>) -> Result<String, GatewayError> {
        let Some(key) = extract_key(headers, query) else {
            return Err(GatewayError::auth(
                "请求没带网关密钥。把 config.yaml 里 clients 段的那把 key 配到客户端上。",
            ));
        };
        self.config
            .clients
            .iter()
            .find(|c| key_eq(&c.key, &key))
            .map(|c| c.name.clone())
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
        // M0 只有透传：任何方法、任何路径都往上游送。M1 加路由时，
        // 这里会先过规则引擎再决定送给谁。
        .fallback(any(passthrough))
        .with_state(state)
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
    // 来源检查在身份检查**之前**：一个不该连过来的地址，不该有机会
    // 试密钥（§5.4）。
    if !state.allow.allows(peer.ip()) {
        return Err(GatewayError::new(
            crate::error::Source::Auth,
            format!(
                "{} 不在允许的来源里。改 listen.gateway.allow_from，或者把 bind 改回 loopback。",
                peer.ip()
            ),
        ));
    }
    let client_name = state.identify(&headers, query.as_deref())?;
    forward::check_body_size(&body, MAX_BODY)?;

    // 首次运行还没配完是正常状态，不是配置错误。这条要在路由之前挡，
    // 因为「一个 provider 都没有」时任何路由结果都是空的，而那条错误
    // 说不清下一步。
    if state.config.providers.is_empty() {
        return Err(GatewayError::config(concat!(
            "还没有配置任何上游。打开 ThinkWatch Lite 添加第一个 provider，",
            "或者往 config.yaml 的 providers 段里写一个。"
        )));
    }

    // 管线第 2 步：路由（§4）。**规则引擎在这里** —— M0 那句「取第一个
    // provider」就是留给这一段的接缝。
    let facts = {
        let mut f = match serde_json::from_slice::<serde_json::Value>(&body) {
            Ok(v) => tw_engine::RequestFacts::from_anthropic_body(&v),
            // body 解不开时用空的性质走兜底规则。**不要因此拒绝请求** ——
            // 我们的解析器不认识的东西，上游可能完全认识（§4.1）。
            Err(_) => tw_engine::RequestFacts::default(),
        };
        f.client = client_name.clone();
        f
    };
    let decision = state
        .engine
        .route(&facts)
        .map_err(|e| GatewayError::config(format!("路由失败：{e}")))?;
    // 管线第 3 步：准入。**排队而不是拒绝**（§4.7）—— 客户端收到 429
    // 通常不会优雅重试，一个本来只需要多等两秒的请求会变成一次任务中断。
    //
    // 闸门在路由**之后**取：要知道走哪个 provider 才能算 per_provider
    // 那一维。
    let client_limit = state
        .config
        .clients
        .iter()
        .find(|c| c.name == client_name)
        .and_then(|c| c.max_concurrent);
    let _pass = state
        .gate
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
        provider: alive.first().map(|s| s.as_str()).unwrap_or("?").to_string(),
        method: "POST".to_string(),
        path: uri.path().to_string(),
        at_ms: now_ms(),
    });

    // 依次尝试。**首字节之前可以透明切换**（§4.2）—— 拿到响应头之前
    // 我们还没往客户端写过任何东西，换一家客户端完全无感。
    //
    // 「尝试链」要留下来：用户能看见故障转移在替他工作，**这是信任的
    // 来源**。一个静默切换过的请求和一个一次就成的请求，在用户眼里
    // 应该是不同的。
    let mut attempts: Vec<String> = Vec::new();
    let mut last_err: Option<GatewayError> = None;
    let mut upstream = None;
    let mut used: Option<&tw_config::Provider> = None;

    for name in &alive {
        let Some(provider) = state.config.providers.iter().find(|p| &p.name == *name) else {
            // 校验时挡过一次，能到这儿说明配置在运行中被换过。
            last_err = Some(GatewayError::config(format!(
                "规则 `{}` 选中了 `{name}`，但配置里没有这个 provider",
                decision.matched_rule
            )));
            continue;
        };
        attempts.push(provider.name.clone());

        let key = match provider.resolved_key() {
            Ok(k) => k,
            Err(e) => {
                // 密钥取不到是这一家的问题（可能是 exec 命令挂了），
                // 换下一家是合理的。
                state.health.record_failure(&provider.name);
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
        let http = state.clients.get(&provider.name).unwrap_or(&state.http);
        let mut req = http.request(method, &url);
        req = forward::forward_headers(req, &headers);
        req = forward::apply_credential(req, provider.effective_protocol(), &key);
        match req.body(body.clone()).send().await {
            Ok(r) if r.status().is_server_error() || r.status() == 429 => {
                // 5xx 和限流：换一家有意义，那边可能有不同的额度或地域。
                // **4xx 不换**（除了 429）—— 请求本身有问题的话，换一家
                // 也一样被拒，还会白白污染那家的健康度。
                state.health.record_failure(&provider.name);
                last_err = Some(GatewayError::upstream(format!(
                    "`{}` 返回 {}",
                    provider.name,
                    r.status()
                )));
                continue;
            }
            Ok(r) => {
                state.health.record_success(&provider.name);
                upstream = Some(r);
                used = Some(provider);
                break;
            }
            Err(e) => {
                state.health.record_failure(&provider.name);
                last_err = Some(forward::map_reqwest_error(e));
                continue;
            }
        }
    }

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
    let out_headers = forward::response_headers(upstream.headers());

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
    let stream = counted
        .map_err(std::io::Error::other)
        .chain(futures::stream::once(async move {
            bus.emit(tw_api::Event::RequestFinished {
                id,
                status: status.as_u16(),
                bytes: bytes_seen.load(std::sync::atomic::Ordering::Relaxed),
                duration_ms: started.elapsed().as_millis() as u64,
            });
            Ok(Bytes::new())
        }));
    let mut resp = Response::new(Body::from_stream(stream));
    *resp.status_mut() = status;
    *resp.headers_mut() = out_headers;
    Ok(resp)
}

/// 起服务。返回实际绑定的地址 —— 端口写 0 时调用方需要知道拿到了哪个。
pub async fn serve(state: AppState, addr: std::net::SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let actual = listener.local_addr()?;
    if !state.allow.is_empty() {
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
    .await
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
        assert_eq!(s.clients.len(), 2);
        assert!(s.clients.contains_key("a") && s.clients.contains_key("b"));
    }

    #[test]
    fn a_known_key_resolves_to_its_client_name() {
        let s = AppState::new(cfg()).unwrap();
        assert_eq!(
            s.identify(&hdr("x-api-key", "tw-good"), None).unwrap(),
            "default"
        );
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
