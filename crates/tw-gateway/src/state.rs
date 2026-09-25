//! 数据面的全部状态：跟着配置整块换的那份运行时，和跨重载存活的那些。

use std::sync::Arc;

use axum::http::HeaderMap;

use crate::auth::key_eq;
use crate::error::GatewayError;
use crate::health::Health;
use crate::outbound::{base_client_builder, client_for_provider, proxy_shape};
use tw_types::msg;

mod credentials;
mod glm;
pub use credentials::credential_failed;
mod upstream;

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
    pub redact: Arc<tw_guard::redact::rules::RuleSet>,
    /// 工具调用审查的规则。同上。
    pub tools: Arc<tw_guard::tools::rules::Rules>,
    /// 内容过滤的规则。同上
    pub content: Arc<tw_guard::content::Rules>,
    /// 藏匿字符查哪几种
    pub hidden: Vec<tw_guard::hidden::Kind>,
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
        let allow =
            crate::access::AllowList::parse(&config.listen.gateway.allow_from).map_err(|e| {
                GatewayError::config(msg!(
                    "gw.config.allow_from", detail = e => "listen.gateway.allow_from: {detail}"
                ))
            })?;
        // 各项防护的规则编译一次，跟着运行时一起换 —— 它们住在
        // config.yaml 的 `security` 里，所以「改了规则」和「改了别的配置」
        // 走同一条热重载路径。
        //
        // 自定义规则的正则、内置规则的 id 在配置校验时已经查过一次，这里再
        // 失败只可能是有人绕过了校验，照样拒绝这份配置
        let sec = &config.security;
        let redact = tw_guard::redact::rules::RuleSet::build(
            &sec.redact.enable,
            &sec.redact.disable,
            sec.redact.active_custom(),
        )
        .map_err(|e| {
            GatewayError::config(msg!("gw.config.security_rules", detail = e => "{detail}"))
        })?;
        let tools = sec.inspect_tools.rules().map_err(|e| {
            GatewayError::config(msg!("gw.config.security_rules", detail = e => "{detail}"))
        })?;
        let content = sec.content.rules().map_err(|e| {
            GatewayError::config(msg!("gw.config.security_rules", detail = e => "{detail}"))
        })?;
        let hidden = sec.hidden_text.kinds();
        Ok(Self {
            engine: Arc::new(config.engine()),
            config: Arc::new(config),
            clients,
            allow,
            redact: Arc::new(redact),
            tools: Arc::new(tools),
            content: Arc::new(content),
            hidden,
        })
    }
}

#[derive(Clone)]
pub struct AppState {
    /// 配置换入时整块换掉的那部分（第 ⑤ 步）。
    ///
    /// **一次 `store` 就是一次生效**：正在跑的请求持有旧的 `Arc`，跑完
    /// 自然释放；新请求看到的是新的。中间没有任何一个瞬间是半新半旧的。
    rt: Arc<arc_swap::ArcSwap<Runtime>>,
    /// 每把密钥自己的并发上限。等，不拒绝。
    ///
    /// **不在 Runtime 里，因为它握着正在跑的请求的通行证。**跟着配置一起
    /// 换的话，每改一次配置，排着的请求就会失去位置，而已经在跑的那些的
    /// 通行证会变成孤儿。上限改了由它自己在原地加减（见 `limits`）。
    pub(crate) gate: Arc<crate::limits::Gate>,
    /// 一家上游不在当前运行时里时顶上的 Client（直连，不读系统代理）
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
    /// 每个 GLM Coding Plan 上游问额度的节奏（见 [`crate::glm`]）。**跨重载存活**
    glm: Arc<crate::glm::Tracker>,
    /// 监听地址变了。**这是「温」那一级**（三级热重载） ——
    /// 换端口不能只换配置：监听器是启动时建的，不重建的话新端口上什么
    /// 都没有，而旧端口还在服务。那种「改了没反应」比报错难查得多。
    relisten: Arc<tokio::sync::Notify>,
    /// 此刻在听的地址。**跟着真实的监听器走，不是启动时记的一次** —— 见
    /// [`crate::listen`]。
    pub(crate) listening: Arc<std::sync::Mutex<crate::listen::Listening>>,
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
    /// 换发的凭据没能写回的那几家，和最后一次的原因。**是现状**：半路才连上的界面按它
    /// 补上那条提醒（`ProviderView.writeback_failed`）；写回成功就清掉
    writeback_failed: Arc<std::sync::Mutex<std::collections::HashMap<String, tw_types::Msg>>>,
    /// 已经报过「凭据失效」的上游。**只在失效的那一刻报一次**，恢复之后清掉
    expired_told: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// 已经报过「用完」的额度窗口：(上游, 窗口)。**窗口恢复之后清掉**，再用完会重新报
    exhausted: Arc<std::sync::Mutex<std::collections::HashSet<(String, String)>>>,
    /// 凭据正被上游拒绝的那几家，和它回的状态码。**进入和恢复各报一次**
    rejected: Arc<std::sync::Mutex<std::collections::HashMap<String, u16>>>,
    /// 每个代理最近一次检查的结果。见 [`AppState::check_proxy`]
    proxies: Arc<std::sync::Mutex<std::collections::HashMap<String, upstream::ProxyState>>>,
    /// 正在服务中的请求数。见 [`crate::live`]。
    pub live: crate::live::Live,
    /// 每段对话此刻归到哪一次会话（见 [`crate::session::Sessions`]）。**跨重载存活**
    pub sessions: Arc<crate::session::Sessions>,
    /// Anthropic 流里上游静默多久就补一个 `ping`（见 `relay`）。**测试会把它调短**，
    /// 否则一条心跳的测试要干等十五秒
    pub ping_every: std::time::Duration,
    /// 上游整个静默（连注释都没有）超过这么久，就不再补 `ping`（见 `relay`）。**测试会把它调短**
    pub ping_for: std::time::Duration,
}

impl AppState {
    pub fn new(config: tw_config::Config) -> Result<Self, GatewayError> {
        // **不走任何代理，连系统代理也不读** —— 和上游默认的 `direct` 一样。
        // 它只在一家上游不在当前运行时里时顶上，那时没有别的出站设置可依
        let http = base_client_builder()
            .no_proxy()
            .build()
            .map_err(|e| crate::outbound::client_error(&e))?;
        let pricing_config = config.pricing.clone();
        let price_assign = config.price_assign();
        let models = Arc::new(crate::models::Directory::default());
        models.reconcile(&config);
        let rt = Runtime::build(config, None)?;
        let state = Self {
            rt: Arc::new(arc_swap::ArcSwap::from_pointee(rt)),
            gate: Default::default(),
            http,
            bus: tw_observe::EventBus::new(),
            health: Arc::new(Health::new()),
            catalog: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
            models,
            body_sink: Arc::new(std::sync::Mutex::new(None)),
            quotas: Arc::new(std::sync::Mutex::new(Default::default())),
            glm: Default::default(),
            relisten: Arc::new(tokio::sync::Notify::new()),
            listening: Arc::new(std::sync::Mutex::new(Default::default())),
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
            writeback_failed: Arc::new(std::sync::Mutex::new(Default::default())),
            expired_told: Arc::new(std::sync::Mutex::new(Default::default())),
            exhausted: Arc::new(std::sync::Mutex::new(Default::default())),
            rejected: Arc::new(std::sync::Mutex::new(Default::default())),
            proxies: Arc::new(std::sync::Mutex::new(Default::default())),
            live: crate::live::Live::default(),
            sessions: Default::default(),
            ping_every: crate::PING_EVERY,
            ping_for: crate::PING_FOR,
        };
        // 手写的清单马上可用；向上游问是后台的事，不挡启动
        state.publish_catalog();
        Ok(state)
    }

    /// 按目录和当前配置重算模型汇总。
    pub(crate) fn publish_catalog(&self) {
        self.models.publish(|| self.config(), &self.catalog);
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

    pub(crate) fn relisten_signal(&self) -> &tokio::sync::Notify {
        &self.relisten
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
                // **不计费的就是最便宜的**；其余按它所选的价目表比，订阅账号
                // 也一样。价目表里没有这个模型的不是「免费」 —— 排到最后去
                match p.billing {
                    tw_config::Billing::Free => Some((name.clone(), (0, 0))),
                    tw_config::Billing::PerToken => {
                        book.unit_micros(name, model).map(|u| (name.clone(), u))
                    }
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

    pub(crate) fn body_sink(&self) -> Option<crate::bodies::BodySender> {
        self.body_sink.lock().ok().and_then(|g| g.clone())
    }

    /// 换一份配置进去（第 ④⑤ 步）。
    ///
    /// **建不起来就什么都不换。**校验已经在 `tw_config::reload` 里做过
    /// 三遍了，但运行时对象仍然可能建不起来（比如代理地址 reqwest 不认），
    /// 而那时旧配置必须原样继续服务。
    pub fn reload(&self, config: tw_config::Config) -> Result<(), GatewayError> {
        let old = self.rt.load();
        let next = Runtime::build(config, Some(&old))?;
        // 比的是写法不是解析出来的地址：网卡名要问系统，而那是监听那一边的事
        let (was, now) = (&old.config.listen.gateway, &next.config.listen.gateway);
        let relisten = was.bind != now.bind || was.port != now.port;
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
            self.relisten();
        }
        Ok(())
    }

    /// 密钥 → 客户端名字 + 方言。
    pub(crate) fn identify(
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
