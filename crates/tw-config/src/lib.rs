//! 配置：schema、加载、校验、初始生成。
//!
//! 只有一份文件，密钥明文写在里面。
//! M0 只做只读加载；双向同步是 M2 的事。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
pub use tw_types::Limits;

pub mod edit;
pub mod history;
mod init;
mod probes;
pub mod proxy;
pub mod refs;
pub mod reload;
mod security;
pub mod store;
mod validate;
pub mod watch;

pub use init::{generate_initial, generate_key};
pub use proxy::{DIRECT, OnProxyFail, Proxy, ProxyKind, SYSTEM};
pub use validate::ValidationError;

/// 配置 schema 的版本。和应用的 CalVer 是两回事 —— 这个只决定要不要
/// 跑迁移。
pub const SCHEMA_VERSION: u32 = 1;

pub const DEFAULT_GATEWAY_PORT: u16 = 8788;

/// **写错的字段名是错误，不是空操作。**
///
/// 这条是烟测里换来的：`listen: { addr: 127.0.0.1:18830 }`（正确写法是
/// `listen.gateway.port`）被静默丢掉，网关照常起在默认端口 8788，日志
/// 里一个字都没有 —— 用户会以为是网关坏了，而不是自己写错了一个词。
/// 密钥、上游地址、并发上限，每一个都有同样的失败模式。
///
/// 代价是旧版二进制读不了新版配置，但 `version` 字段本来就是干这个的，
/// 而且桌面版的 core 和 UI 是一起发的。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    /// **默认值不写进文件。**第一天的配置只有六行，而每加一个
    /// 带默认值的字段就会往文件里多堆几行 —— 用户打开配置看到一屏自己
    /// 没配过的东西，就分不清哪些是他的决定、哪些只是默认。
    #[serde(default, skip_serializing_if = "is_default")]
    pub listen: Listen,
    /// 客户端身份。密钥即身份 —— 不是「先认证再看是谁」，
    /// 而是「这把钥匙就是这个人」。
    ///
    /// **不写这一段不是语法错误。**serde 的「missing field `clients`」
    /// 说不出下一步做什么，而 `validate` 那句「首次启动本应自动生成
    /// 一把」能。缺字段的判断交给它。
    #[serde(default)]
    pub clients: Vec<Client>,
    /// **同样可以整段不写。**第一天的配置只有六行，而零 provider
    /// 是一个合法状态（首次运行就是它）—— 逼用户写一行 `providers: []`
    /// 只是为了让解析器高兴。
    #[serde(default)]
    pub providers: Vec<Provider>,
    /// 策略组。不写就没有 —— 层 0（只配 provider）是完全合法的配置，
    /// 引擎内部会把它展开。
    /// 出站代理。声明一次到处引用。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proxies: Vec<Proxy>,
    /// 价目表：默认价目表要不要定期刷新，以及用户自己的价目表。
    #[serde(default, skip_serializing_if = "is_default")]
    pub pricing: tw_pricing::PricingConfig,
    /// 客户端自己发的辅助请求怎么处理。默认只拦 A 类。
    #[serde(default, skip_serializing_if = "is_default")]
    pub client_probes: ClientProbes,
    /// 三道防线。**出厂时都停在「观察」** —— 只记录，不改变
    /// 任何行为。
    #[serde(default, skip_serializing_if = "is_default")]
    pub security: Security,
    /// 并发上限。不写就是默认值。
    #[serde(default, skip_serializing_if = "is_default")]
    pub limits: Limits,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<tw_engine::Group>,
    /// 路由。一条路由是一组按顺序求值的规则。
    ///
    /// **不写就是「按声明顺序故障转移」** —— 那条默认路由由引擎合成，
    /// 不写进文件（`generate_initial` 的六行里没有它）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<tw_engine::RouteSet>,
    /// 没绑路由的密钥走哪条。不写就是叫「默认」的那条。
    ///
    /// **它是顶层的一个名字，不是某条路由身上的标志** —— 结构上就唯一，
    /// 不需要一条「最多一条默认」的校验去维持。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_route: Option<String>,
}

/// 便于构造，**不代表一份可用的配置** —— `providers` 和 `clients` 都是
/// 空的，得自己填。加这个是因为每给 `Config` 添一个字段，所有构造点都
/// 要改一遍，而那些改动全是噪音。
impl Default for Config {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            listen: Listen::default(),
            clients: Vec::new(),
            providers: Vec::new(),
            proxies: Vec::new(),
            pricing: tw_pricing::PricingConfig::default(),
            client_probes: ClientProbes::default(),
            security: Security::default(),
            limits: Limits::default(),
            groups: Vec::new(),
            routes: Vec::new(),
            default_route: None,
        }
    }
}

/// 同上：加 Default 是为了让新字段不再逼着所有构造点跟着改。
impl Default for Client {
    fn default() -> Self {
        Self {
            name: String::new(),
            key: String::new(),
            max_concurrent: None,
            allow: None,
            route: None,
        }
    }
}

/// 同上。`name` / `base_url` / `key` 空着的 provider 过不了校验。
impl Default for Provider {
    fn default() -> Self {
        Self {
            name: String::new(),
            base_url: String::new(),
            key: Secret::Literal(String::new()),
            protocol: None,
            models: Vec::new(),
            billing: None,
            redact: None,
            trust: None,
            proxy: default_proxy(),
            on_proxy_fail: OnProxyFail::default(),
            pricing: None,
        }
    }
}

impl Config {
    /// 哪个上游选了哪张价目表。**没选的不在里面** —— 它们按默认价目表。
    pub fn price_assign(&self) -> Vec<(String, String)> {
        self.providers
            .iter()
            .filter_map(|p| p.pricing.clone().map(|s| (p.name.clone(), s)))
            .collect()
    }

    /// 按配置建一个路由引擎。
    pub fn engine(&self) -> tw_engine::Engine {
        tw_engine::Engine::new(
            self.providers.iter().map(|p| p.name.clone()).collect(),
            self.groups.clone(),
            self.routes.clone(),
            self.default_route.clone(),
            // 密钥 → 它绑的那条路由。**引擎不认识密钥这个概念** ——
            // 它只需要「这个名字走哪条路由」，所以映射在这里拍平，
            // 而不是把整个 `clients` 交进去。
            self.clients
                .iter()
                .filter_map(|c| c.route.clone().map(|r| (c.name.clone(), r)))
                .collect(),
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listen {
    #[serde(default)]
    pub gateway: GatewayListen,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayListen {
    /// `loopback` | `all` | 一张网卡的 IP。默认 loopback —— 学 Surge，
    /// 但默认值要保守。
    #[serde(default)]
    pub bind: Bind,
    #[serde(default = "default_port")]
    pub port: u16,
    /// 只在绑到本机之外时生效的来源白名单（CIDR）。
    #[serde(default)]
    pub allow_from: Vec<String>,
}

fn default_port() -> u16 {
    DEFAULT_GATEWAY_PORT
}

impl GatewayListen {
    /// 实际要监听的地址。
    ///
    /// **一个函数，不是两处各拼一遍。**bind 和 port 分开写的地方多了，
    /// 迟早有一处忘了跟着改 —— 而它的表现是「监听在了一个谁也没想到的
    /// 地址上」。
    pub fn socket_addr(&self) -> std::net::SocketAddr {
        std::net::SocketAddr::new(self.bind.addr(), self.port)
    }
}

impl Default for GatewayListen {
    fn default() -> Self {
        Self {
            bind: Bind::default(),
            port: DEFAULT_GATEWAY_PORT,
            allow_from: Vec::new(),
        }
    }
}

/// 绑在哪张网卡上。
///
/// 三种写法，对应三个真实的选择：
///
/// ```yaml
/// bind: loopback      # 127.0.0.1，只有本机
/// bind: 192.168.1.5   # 某一张具体的网卡
/// bind: all           # 0.0.0.0，所有网卡
/// ```
///
/// **以前这里有个 `lan`，它是假的。**`lan` 和 `all` 绑的是同一个地址
/// `0.0.0.0`，区别只在 `allow_from` 的默认值 —— 也就是说它是个白名单
/// 概念，伪装成了网卡选择。用户在界面上选「局域网」，以为网关只在局域
/// 网那张网卡上监听，实际上它在**所有**网卡上监听，包括公网那张。
/// 现在要真的只在局域网网卡上听，就写那张网卡的地址。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Bind {
    #[default]
    Loopback,
    All,
    /// 一张具体的网卡。**地址会变** —— DHCP 续租、换网络都可能让它失效，
    /// 那时网关起不来。这是选它要接受的代价，界面上必须说。
    Addr(std::net::IpAddr),
}

impl Bind {
    pub fn addr(&self) -> std::net::IpAddr {
        match self {
            Bind::Loopback => std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            Bind::All => std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            Bind::Addr(a) => *a,
        }
    }

    /// 本机之外连得上吗。
    ///
    /// **这个判断决定了两件强制行为**：密钥校验不可关闭，
    /// 以及 `allow_from` 为空时自动填私网段。
    ///
    /// 判的是地址本身而不是枚举变体 —— `bind: 127.0.0.1` 写成具体地址
    /// 的时候，它和 `loopback` 是同一件事，不该因为换了个写法就被当成
    /// 暴露在外。
    pub fn is_exposed(&self) -> bool {
        !self.addr().is_loopback()
    }
}

impl Bind {
    /// 给界面显示的地址字符串。`loopback`/`all` 展开成真实地址，
    /// 具体网卡就是它自己。
    pub fn socket_string(&self) -> String {
        self.addr().to_string()
    }
}

impl std::fmt::Display for Bind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Bind::Loopback => f.write_str("loopback"),
            Bind::All => f.write_str("all"),
            Bind::Addr(a) => write!(f, "{a}"),
        }
    }
}

impl Serialize for Bind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Bind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        match raw.as_str() {
            "loopback" => Ok(Bind::Loopback),
            "all" => Ok(Bind::All),
            other => other.parse().map(Bind::Addr).map_err(|_| {
                // **说清楚三种合法写法。**「invalid value」对着一个
                // 手写配置文件的人什么都没说，而这个字段写错的后果是
                // 整份配置加载失败、网关起不来。
                serde::de::Error::custom(format!(
                    "`bind` 只能是 `loopback`、`all`，或者一张网卡的 IP（比如 192.168.1.5）。\
                     收到的是 `{other}`"
                ))
            }),
        }
    }
}

impl GatewayListen {
    /// 实际生效的来源白名单。
    ///
    /// **`lan` / `all` 且用户没写白名单时，默认填私网段** ——
    /// 而不是放行所有。想放开得手动写 `0.0.0.0/0`，那时他至少知道自己
    /// 做了什么。
    pub fn effective_allow_from(&self) -> Vec<String> {
        if !self.bind.is_exposed() || !self.allow_from.is_empty() {
            return self.allow_from.clone();
        }
        tw_types::PRIVATE_RANGES
            .iter()
            .map(|s| s.to_string())
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Client {
    pub name: String,
    /// 这个客户端自己的并发上限。**监听局域网时是刚需** ——
    /// 某台机器上的失控脚本不该能占满全部并发。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<usize>,
    /// 这个客户端能看到哪些模型。
    ///
    /// **三种状态语义分明**，因为 one-api 和 new-api 在这里正好相反：
    ///
    /// - 不写（`None`）→ 只按方言过滤。默认
    /// - 写非空 → 在方言过滤基础上再按 glob 保留
    /// - 写 `[]` → **一个都不给**。「临时禁用这个客户端」的正当用法
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    /// 这把密钥走哪条路由。**一把密钥一条路由。**
    ///
    /// 不写就走默认路由（顶层的 `default_route`）。一条路由可以绑给
    /// 多把密钥，规则只有一份。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    /// 网关密钥。`tw-` 前缀是刻意的：用户在客户端配置里看到它时，
    /// 一眼就知道这不是某个上游的真 key。
    pub key: String,
}

/// 密钥怎么来。
///
/// 两种形态：绝大多数人写一个字符串就完了（明确说了密钥就明文写在
/// 配置里，不做 keychain），字符串里可以带 `${ENV}`；另一种是 OAuth，
/// 带自动刷新。
///
/// serde 的 untagged 让第一种是裸字符串 —— 配置文件里看不出 `${ENV}`
/// 和明文的区别，也不该看出。
///
/// **没有「跑一条命令拿密钥」这一类，而且不会有**：配置文件
/// 不该能执行程序 —— 「配置被同步、被分享、被 AI 改」都是目标
/// 场景，那时抄一份配置就等于跑一段代码。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Secret {
    /// 明文，或含 `${ENV}` 的字符串
    Literal(String),
    /// OAuth，带自动刷新。
    OAuth { oauth: OAuth },
    /// 什么形状都没匹配上。**存在的唯一理由是报一句人话。**
    ///
    /// serde 的 untagged 在全部变体都不匹配时只会说
    /// 「data did not match any variant of untagged enum Secret」——
    /// 而这是配置里最重要的那个字段，那句话对着它等于什么都没说。
    /// 更糟的是它把**每一种**写错都塌成同一句：`oauth` 少写一个必填
    /// 字段和随手打错一个键名，报出来一模一样。
    ///
    /// 有了这个兜底，`validate` 那一关才有机会拿着真实的值去说清楚
    /// 到底哪儿不对。**注意它必须排在最后** —— untagged 是按顺序试的。
    #[serde(skip_serializing)]
    Unknown(serde_yaml_ng::Value),
}

/// OAuth 凭据（第 3 类）。
///
/// # 轮换
///
/// 很多 OAuth2 服务器每次刷新都换发一个新的 refresh token 并作废旧的。
/// 我们**把新的写回 config.yaml** —— 那是一次用户没要求的写入，
/// 但不写回更糟：**换发新的那一刻旧的已经在服务端作废了**，不写回等于
/// 让配置文件从那一秒起就是坏的，只是症状延迟到下一次重启。
///
/// 写回只动那一个标量（span 补丁），而且不进配置历史 —— 回滚到一次
/// 轮换之前拿到的是一个作废的 token，那不是可以退回去的状态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuth {
    /// 现成的 access token。**可选** —— 不写就启动后立刻用 refresh 换一个
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<String>,
    pub refresh: String,
    /// token 端点
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// 提前多久去刷。**默认 5 分钟** —— 避免边界上打到一个刚过期的
    /// token，而那个失败看起来是「上游偶尔 401」
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_before: Option<String>,
}

impl OAuth {
    /// 提前量。写坏了按默认走 —— **一个写错的提前量不该让上游整个不可用**。
    pub fn refresh_before(&self) -> std::time::Duration {
        const DEFAULT: u64 = 300;
        let secs = self
            .refresh_before
            .as_deref()
            .and_then(parse_duration_secs)
            .unwrap_or(DEFAULT);
        std::time::Duration::from_secs(secs)
    }
}

/// `30s` / `5m` / `1h`。不带单位按秒。
///
/// 不发明一套时长语言，只认这三个后缀 —— 示例写的就是 `5m`。
pub fn parse_duration_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last()? {
        's' => (&s[..s.len() - 1], 1),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        _ => (s, 1),
    };
    num.trim().parse::<u64>().ok().map(|n| n * mult)
}

impl Secret {
    /// 拿到真正的密钥。
    ///
    /// **同步**：展开 `${ENV}` 就到头了。OAuth 那一类要联网换 token，
    /// 走不了这条路 —— 它返回 `NeedsRefresh`，由网关那边的异步路径处理。
    pub fn resolve(&self) -> Result<String, SecretResolveError> {
        match self {
            Secret::Literal(s) => Ok(tw_secret::expand_from_env(s)?),
            // **OAuth 走不了同步这条路**：换 token 是一次网络往返。
            // 调用方要么走异步那条（网关），要么把这一句原样说给用户听
            // （`twcore check`）—— 都比在这里编一个值好
            Secret::OAuth { .. } => Err(SecretResolveError::NeedsRefresh),
            // 到不了这儿：校验那一关先把整份配置拒了
            Secret::Unknown(_) => Err(SecretResolveError::Unreadable),
        }
    }

    /// 是不是 OAuth。调用方据此决定走异步那条路。
    pub fn is_oauth(&self) -> bool {
        matches!(self, Secret::OAuth { .. })
    }

    pub fn oauth(&self) -> Option<&OAuth> {
        match self {
            Secret::OAuth { oauth } => Some(oauth),
            _ => None,
        }
    }

    /// 给人看的形态，**永远不含真实密钥**。
    pub fn describe(&self) -> String {
        match self {
            Secret::Literal(s) if s.contains("${") => format!("环境变量 {s}"),
            Secret::Literal(s) => tw_secret::mask_secret(s),
            // **不回显任何一段 token** —— refresh token 比 access token
            // 更值钱，它换得出无数个 access
            Secret::OAuth { oauth } => format!("OAuth（{}）", oauth.endpoint),
            Secret::Unknown(_) => "（这个 key 读不懂）".to_string(),
        }
    }

    pub(crate) fn is_blank(&self) -> bool {
        match self {
            Secret::Literal(s) => s.trim().is_empty(),
            Secret::OAuth { oauth } => {
                oauth.refresh.trim().is_empty() || oauth.endpoint.trim().is_empty()
            }
            // 「读不懂」是另一回事，由 `BadKeyShape` 单独报
            Secret::Unknown(_) => false,
        }
    }
}

/// 裸字符串是绝大多数人的写法，所以让它在 Rust 侧也是最省事的那个。
impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Secret::Literal(s.to_string())
    }
}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Secret::Literal(s)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SecretResolveError {
    #[error(transparent)]
    Env(#[from] tw_secret::SecretError),
    /// OAuth 凭据要去换 token，那是一次网络往返，同步这条路走不了。
    #[error("这是一个 OAuth 凭据，要联网换 token —— 网关起来之后才会去换")]
    NeedsRefresh,
    #[error("这个 key 的写法读不懂")]
    Unreadable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    /// 明文、`${ENV}`、或 `{ oauth: {...} }`。**不做 keychain。**
    pub key: Secret,
    /// 不写就从 base_url 猜（最小配置）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
    /// 代理名，或内置的 `direct` / `system`。
    ///
    /// **默认 `direct` 而不是 `system`**：显式优于隐式。默认跟随系统的
    /// 话，用户在系统里开了全局代理，本地 Ollama 就会莫名连不上，而
    /// 配置文件里看不出任何线索。
    /// 探测不到时的兜底清单。
    ///
    /// 有些中转站没实现 `/v1/models`。**这是 provider 级的「这家有什么」，
    /// 不是全局的「我们对外暴露什么」** —— 那个由汇总推导出来。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// 这家怎么收钱。
    ///
    /// **不写就自动判**：响应头里报过订阅额度的就是订阅型。
    /// 那个信号一直在我们手上，不该变成一个用户要填的字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing: Option<Billing>,
    /// 发给这家之前，把哪几类东西换成占位符。
    ///
    /// **不写就按 base_url 判**：官方端点不脱，其余脱默认那几类。理由很
    /// 实在 —— 你让 Claude Code 调试一个 `.env` 问题，它得真看见里面的值
    /// 才帮得上忙；对官方端点脱敏，等于为了防一个你本来就信任的对象而
    /// 自废武功。而中转站是完整的中间人。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redact: Option<Vec<tw_redact::rules::Kind>>,
    /// 这家可不可信。**不写就按 base_url 判。**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<Trust>,
    #[serde(default = "default_proxy", skip_serializing_if = "is_direct")]
    pub proxy: String,
    #[serde(default, skip_serializing_if = "is_default_on_proxy_fail")]
    pub on_proxy_fail: OnProxyFail,
    /// 按哪张价目表计价。不写就是默认价目表。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<String>,
}

/// 这家上游可不可信。
///
/// **默认值落在保守那一侧**（「零值 = 安全」）：没判出来就是
/// 不受信任。中转站是完整的中间人 —— 它不只能看你的请求，还能改你收到的
/// 响应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trust {
    /// 厂商官方端点。工具调用只记录，不拦
    Official,
    /// 高危拦截，中危告警
    #[default]
    Untrusted,
}

impl Trust {
    /// 写进 YAML 的那个词。
    pub fn slug(&self) -> &'static str {
        match self {
            Trust::Official => "official",
            Trust::Untrusted => "untrusted",
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Trust::Official => "官方",
            Trust::Untrusted => "不受信任",
        }
    }
    /// 命中高危时要不要真的切断。
    pub fn blocks(&self) -> bool {
        matches!(self, Trust::Untrusted)
    }
}

/// 上游怎么收钱。
///
/// **接入订阅型网关之后，「按价目表乘 token 数」这个假设就不成立了** ——
/// 订阅制的边际成本是零，按 API 价目表算出来的数字是纯虚构的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Billing {
    /// 按价目表计价。默认
    #[default]
    PerToken,
    /// 订阅制，边际成本为零。
    ///
    /// **成本栏显示「订阅」而不是 `$0.00`** —— 后者看起来像一个算出来的
    /// 结果，会让人误以为这次调用真的免费；「订阅」表达的是「这笔账不在
    /// 这个维度上」。
    Subscription,
    /// 上游价格未知。成本栏标「未知」，**不参与合计**
    Unknown,
}

impl Billing {
    /// 这次调用该不该进金额合计。
    ///
    /// **宁可显示「不知道」，也不显示一个编出来的精确数字**。
    pub fn counts_toward_money(&self) -> bool {
        matches!(self, Billing::PerToken)
    }
    pub fn label(&self) -> &'static str {
        match self {
            Billing::PerToken => "按量",
            Billing::Subscription => "订阅",
            Billing::Unknown => "未知",
        }
    }
    pub fn slug(&self) -> &'static str {
        match self {
            Billing::PerToken => "per-token",
            Billing::Subscription => "subscription",
            Billing::Unknown => "unknown",
        }
    }
    pub fn parse(s: &str) -> Billing {
        match s {
            "subscription" => Billing::Subscription,
            "unknown" => Billing::Unknown,
            _ => Billing::PerToken,
        }
    }
}

/// 和默认值相等吗。用于 `skip_serializing_if`。
fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

fn default_proxy() -> String {
    DIRECT.to_string()
}

fn is_direct(v: &str) -> bool {
    v == DIRECT
}

fn is_default_on_proxy_fail(v: &OnProxyFail) -> bool {
    *v == OnProxyFail::default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    Anthropic,
    OpenaiChat,
    OpenaiResponses,
    Gemini,
}

impl Protocol {
    /// 写进 YAML 的那个词。
    pub fn slug(&self) -> &'static str {
        match self {
            Protocol::Anthropic => "anthropic",
            Protocol::OpenaiChat => "openai-chat",
            Protocol::OpenaiResponses => "openai-responses",
            Protocol::Gemini => "gemini",
        }
    }
}

impl Provider {
    /// 从 base_url 猜协议。猜不出来返回 None —— **不猜一个默认值**，
    /// 因为猜错的表现是「请求发出去了但上游 400」，比直接说不知道难查。
    pub fn guess_protocol(base_url: &str) -> Option<Protocol> {
        let h = base_url.to_ascii_lowercase();
        if h.contains("api.anthropic.com") {
            Some(Protocol::Anthropic)
        } else if h.contains("generativelanguage.googleapis.com") {
            Some(Protocol::Gemini)
        } else if h.contains("api.openai.com") {
            Some(Protocol::OpenaiChat)
        } else {
            None
        }
    }

    pub fn effective_protocol(&self) -> Option<Protocol> {
        self.protocol
            .or_else(|| Self::guess_protocol(&self.base_url))
    }

    /// 这是不是一个厂商官方的端点。
    ///
    /// **判据只有域名。**「我信任这一家」是个安全判断，而域名是这里唯一
    /// 不可伪装的东西 —— 一个中转站可以把自己叫做 `anthropic-official`，
    /// 但它没法让自己的 base_url 变成 `api.anthropic.com`。
    pub fn is_official_endpoint(&self) -> bool {
        const OFFICIAL: &[&str] = &[
            "api.anthropic.com",
            "api.openai.com",
            "generativelanguage.googleapis.com",
            "api.x.ai",
            "api.deepseek.com",
            "api.moonshot.cn",
            "open.bigmodel.cn",
            "dashscope.aliyuncs.com",
        ];
        let h = self.base_url.to_ascii_lowercase();
        // **要在 host 上比，不能只看包含。**`https://evil.com/api.anthropic.com/`
        // 里也「含有」那个域名
        let host = h
            .split("://")
            .nth(1)
            .unwrap_or(&h)
            .split('/')
            .next()
            .unwrap_or("")
            .split('@')
            .next_back()
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("");
        OFFICIAL.contains(&host) || host.ends_with(".amazonaws.com")
    }

    /// 发给这家要脱哪几类。
    pub fn effective_redact(&self) -> Vec<tw_redact::rules::Kind> {
        use tw_redact::rules::Kind;
        if let Some(k) = &self.redact {
            return k.clone();
        }
        if self.is_official_endpoint() {
            // 官方端点不脱。**为了防一个你本来就信任的对象而自废武功，
            // 是这一节最要避免的事**
            return Vec::new();
        }
        // 默认那几类**不含 `internal`**：RFC1918 地址在代码和文档里到处
        // 都是，而它的危害远小于一把 key。想脱的人自己开
        vec![
            Kind::ApiKeys,
            Kind::PrivateKeys,
            Kind::Jwt,
            Kind::ConnStrings,
        ]
    }

    /// 这家可不可信。
    pub fn effective_trust(&self) -> Trust {
        self.trust.unwrap_or(if self.is_official_endpoint() {
            Trust::Official
        } else {
            Trust::Untrusted
        })
    }

    /// 拿到真正的 key（展开 `${ENV}`）。
    pub fn resolved_key(&self) -> Result<String, SecretResolveError> {
        self.key.resolve()
    }
}

/// 把 token 端点换发的新 refresh token 写回 config.yaml 的**那一个标量**。
///
/// # 为什么要写回
///
/// **服务器换发新 refresh token 的那一刻，旧的已经在服务端作废了。**
/// 所以「不写回」不是保守选项 —— 它保证了配置文件从那一秒起就是坏的，
/// 只是症状延迟到下一次重启（表现是这家上游突然全是 401，而那时没人
/// 会想到是几天前的一次轮换）。写回才是安全的那一边。
///
/// # 为什么是 span 补丁而不是 serde 往返
///
/// 往返会把用户的注释、空行、字段顺序全洗掉 —— 而这是**用户没要求的
/// 一次写入**，它必须只动它该动的那 40 个字符。cc-switch 那 147 个
/// commit 的白名单教训在这里同样成立：我们要写什么是清楚的，
/// 「要保留什么」永远数不完。
///
/// 找不到那个字段就报错，**不追加**：追加意味着我们猜错了结构，而在
/// 一个装着明文密钥的文件里猜结构是不能接受的。
pub fn patch_oauth_refresh(
    text: &str,
    provider: &str,
    new_refresh: &str,
) -> Result<String, RotateError> {
    // 名字对应第几个 provider —— 从**文本本身**数，不从解析后的结构数。
    // 两者理论上一致，但真正要动的是文本里的那个位置。
    let idx = provider_index(text, provider).ok_or_else(|| RotateError::NoProvider {
        provider: provider.to_string(),
    })?;
    let path = tw_yaml::path!["providers", idx, "key", "oauth", "refresh"];
    // 先确认它在那儿。`set` 对不存在的路径行为是另一回事，而这里
    // 「不在那儿」本身就是「别写」的理由
    tw_yaml::find(text, &path).map_err(|source| RotateError::Shape {
        provider: provider.to_string(),
        source,
    })?;
    let out = tw_yaml::set(text, &path, &tw_yaml::Scalar::s(new_refresh)).map_err(|source| {
        RotateError::Shape {
            provider: provider.to_string(),
            source,
        }
    })?;
    // **写之前先自己读一遍。**patch 出来的东西必须还是一份能加载的配置，
    // 而且那个字段真的变成了新值 —— 否则我们会把一份坏配置留在盘上，
    // 而用户下一次启动才撞上它（「先校验再写」同一条）。
    let re = try_parse(&out).map_err(|r| RotateError::Broke {
        provider: provider.to_string(),
        why: r.message,
    })?;
    let ok = re
        .providers
        .iter()
        .find(|p| p.name == provider)
        .and_then(|p| p.key.oauth())
        .is_some_and(|o| o.refresh == new_refresh);
    if !ok {
        return Err(RotateError::Broke {
            provider: provider.to_string(),
            why: "补丁写完之后读回来，那个字段不是新值".into(),
        });
    }
    Ok(out)
}

/// `providers` 里第几个叫这个名字。
fn provider_index(text: &str, provider: &str) -> Option<usize> {
    let cfg: Config = serde_yaml_ng::from_str(text).ok()?;
    cfg.providers.iter().position(|p| p.name == provider)
}

#[derive(Debug, thiserror::Error)]
pub enum RotateError {
    #[error("配置里没有叫 `{provider}` 的上游了")]
    NoProvider { provider: String },
    /// **说清「形状不对」而不是「写失败」。**用户可能把凭据写成了
    /// 别的形状（锚点、块标量），那时正确的动作是他自己去改，
    /// 而不是让我们猜。
    #[error("`{provider}` 的 key.oauth.refresh 不在预期的位置上：{source}")]
    Shape {
        provider: String,
        source: tw_yaml::PatchError,
    },
    #[error("给 `{provider}` 打完补丁之后配置读不回来了，没有写盘：{why}")]
    Broke { provider: String, why: String },
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("读 {path} 失败：{source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// 语法错和字段名写错都走这里。**说「不是合法的 YAML」是错的** ——
    /// 一份语法完美、只是把 `port` 写成 `prot` 的文件也会到这儿，而那句
    /// 话会让用户去找一个根本不存在的语法错误。serde 自己的
    /// 「unknown field `prot`, expected one of ...」比我们能补的任何话都准。
    #[error("{path} 读不通：{source}")]
    Parse {
        path: PathBuf,
        source: serde_yaml_ng::Error,
    },
    #[error("{0}")]
    Invalid(#[from] ValidationError),
}

/// 从磁盘读一份配置并校验。
pub fn load(path: &Path) -> Result<Config, LoadError> {
    let text = std::fs::read_to_string(path).map_err(|source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let cfg: Config = serde_yaml_ng::from_str(&text).map_err(|source| LoadError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    validate::validate(&cfg)?;
    Ok(cfg)
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("写 {path} 失败：{source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("序列化失败：{0}")]
    Serialize(#[from] serde_yaml_ng::Error),
}

/// 整文件写下一份配置，权限 `0600`。
///
/// **这不是最小替换**。那一套是「改一个标量、其余字节原样不动」，
/// 用于日常改配置；这一套是从无到有生成，用于首次运行。两者混用会让
/// 「注释和格式原样保留」那条承诺失效。
///
/// 权限不能靠 umask 的运气：这个文件里有明文密钥。
pub fn write(path: &Path, cfg: &Config) -> Result<(), WriteError> {
    let text = serde_yaml_ng::to_string(cfg)?;
    // **权限、原子写、目录创建都在 store 里。**两处各写一遍就是两处会
    // 漂移，而漂移的那一处大概率是漏了 0600 的那处 —— 这个坑今天已经
    // 踩过一次了。
    store::write_atomic(path, &text).map_err(|e| match e {
        store::StoreError::Io { path, source } => WriteError::Io { path, source },
        other => WriteError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other(other.to_string()),
        },
    })?;
    Ok(())
}

pub use probes::{ClientProbes, ProbeAction};
pub use reload::{Rejected, Stage, try_parse};
pub use security::{Mode as SecurityMode, ScanRule, ScanRules, Security};
// Billing 在本文件里定义，这里不必再导出
pub use store::{Fingerprint, Loaded, StoreError, version_of};
pub use validate::validate;

/// 默认配置目录：`~/.thinkwatch`。
pub fn default_dir() -> PathBuf {
    std::env::var_os("THINKWATCH_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".thinkwatch")))
        .unwrap_or_else(|| PathBuf::from(".thinkwatch"))
}

pub fn default_path() -> PathBuf {
    default_dir().join("config.yaml")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
version: 1
clients:
  - { name: default, key: tw-a3f9c8d1e5b2 }
providers:
  - { name: my-relay, base_url: https://api.example.com, key: sk-xxx }
"#;

    #[test]
    fn the_six_line_config_actually_parses() {
        // 「第一天的配置是六行」是对用户的承诺。如果这个测试挂了，那句话
        // 就是假的。
        let cfg: Config = serde_yaml_ng::from_str(MINIMAL).unwrap();
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.clients.len(), 1);
        assert_eq!(cfg.providers[0].name, "my-relay");
        // 什么都没写的段落必须有能用的默认值
        assert_eq!(cfg.listen.gateway.bind, Bind::Loopback);
        assert_eq!(cfg.listen.gateway.port, DEFAULT_GATEWAY_PORT);
        assert!(cfg.listen.gateway.allow_from.is_empty());
    }

    #[test]
    fn protocol_is_guessed_from_the_url_or_left_unknown() {
        assert_eq!(
            Provider::guess_protocol("https://api.anthropic.com"),
            Some(Protocol::Anthropic)
        );
        assert_eq!(
            Provider::guess_protocol("https://generativelanguage.googleapis.com/v1beta"),
            Some(Protocol::Gemini)
        );
        // 中转站猜不出来 —— 返回 None 而不是编一个默认值。
        assert_eq!(
            Provider::guess_protocol("https://relay.example.cn/v1"),
            None
        );
    }

    #[test]
    fn explicit_protocol_wins_over_the_guess() {
        let p = Provider {
            name: "x".into(),
            base_url: "https://api.anthropic.com".into(),
            key: Secret::Literal("k".into()),
            protocol: Some(Protocol::OpenaiChat),
            ..Default::default()
        };
        assert_eq!(p.effective_protocol(), Some(Protocol::OpenaiChat));
    }

    #[test]
    fn bind_defaults_to_loopback_not_all_interfaces() {
        // 默认监听 0.0.0.0 会把网关暴露给整个局域网，而用户不会知道。
        assert_eq!(Bind::default().addr().to_string(), "127.0.0.1");
        assert_eq!(Bind::All.addr().to_string(), "0.0.0.0");
        assert!(!Bind::default().is_exposed());
        assert!(Bind::All.is_exposed());
    }

    #[test]
    fn bind_accepts_a_concrete_interface_address() {
        // 这是 `lan` 被换掉的理由：想「只在局域网那张网卡上听」，
        // 以前只能写 `lan`，而它绑的是 0.0.0.0 —— 所有网卡，包括公网那张。
        let b: Bind = serde_yaml_ng::from_str("192.168.1.5").unwrap();
        assert_eq!(b, Bind::Addr("192.168.1.5".parse().unwrap()));
        assert_eq!(b.socket_string(), "192.168.1.5");
        assert!(b.is_exposed());
    }

    #[test]
    fn writing_the_loopback_address_out_longhand_is_not_exposure() {
        // `is_exposed` 判的是地址，不是枚举变体 —— 换个写法不该让
        // 密钥校验被强制、白名单被自动填上。
        let b: Bind = serde_yaml_ng::from_str("127.0.0.1").unwrap();
        assert!(!b.is_exposed());
    }

    #[test]
    fn every_bind_form_round_trips_through_yaml() {
        for raw in ["loopback", "all", "192.168.1.5", "::1"] {
            let b: Bind = serde_yaml_ng::from_str(raw).unwrap();
            let back = serde_yaml_ng::to_string(&b).unwrap();
            assert_eq!(back.trim(), raw, "{raw} 写回来变了样");
        }
    }

    #[test]
    fn a_bind_typo_says_what_the_three_forms_are() {
        // 这个字段写错的后果是整份配置加载失败、网关起不来 ——
        // 那条错误必须自带答案。
        let e = serde_yaml_ng::from_str::<Bind>("lan")
            .unwrap_err()
            .to_string();
        assert!(e.contains("loopback"), "{e}");
        assert!(e.contains("all"), "{e}");
        assert!(e.contains("192.168.1.5"), "{e}");
    }

    #[test]
    fn a_bare_string_key_still_parses_the_way_it_always_did() {
        // untagged 的第一条：配置文件里绝大多数人写的还是一个裸字符串，
        // 而且不该看出这里有个枚举。
        let cfg: Config = serde_yaml_ng::from_str(MINIMAL).unwrap();
        assert!(matches!(cfg.providers[0].key, Secret::Literal(ref s) if s == "sk-xxx"));
    }

    #[test]
    fn describe_never_leaks_a_literal_key() {
        // 这个方法会出现在 UI、日志、错误信息里。
        let s = Secret::Literal("sk-ant-api03-verysecretvalue".into());
        let d = s.describe();
        assert!(!d.contains("verysecret"), "{d}");
        assert!(d.contains('…'), "{d}");
    }

    #[test]
    fn describe_shows_the_env_var_name_not_its_value() {
        // 变量名不是秘密，而它恰恰是用户排查时要看的东西。
        assert_eq!(
            Secret::Literal("${MY_KEY}".into()).describe(),
            "环境变量 ${MY_KEY}"
        );
    }

    #[test]
    fn env_interpolation_reaches_the_key() {
        unsafe { std::env::set_var("TW_TEST_KEY", "sk-from-env") };
        let p = Provider {
            name: "x".into(),
            base_url: "https://x".into(),
            key: Secret::Literal("${TW_TEST_KEY}".into()),
            ..Default::default()
        };
        assert_eq!(p.resolved_key().unwrap(), "sk-from-env");
    }
}

#[cfg(test)]
mod strictness_tests {
    use super::*;

    /// 烟测里换来的：`listen: { addr: ... }` 被静默丢掉，网关起在默认
    /// 端口，日志里一个字都没有。**写错的字段名必须是错误。**
    #[test]
    fn a_misspelled_field_is_an_error_that_names_the_field() {
        let e = serde_yaml_ng::from_str::<Config>(
            "version: 1\nlisten:\n  addr: 127.0.0.1:18830\nclients: []\nproviders: []\n",
        )
        .unwrap_err();
        let m = e.to_string();
        assert!(m.contains("addr"), "错误信息里得有那个写错的词：{m}");
        assert!(m.contains("gateway"), "还得说对的写法是什么：{m}");
    }

    #[test]
    fn a_misspelled_provider_field_is_an_error_too() {
        // `base_ur` 静默丢掉的话，剩下的是一个没有地址的上游。
        let e = serde_yaml_ng::from_str::<Config>(
            "version: 1\nproviders:\n  - name: a\n    base_ur: http://x\n    key: k\n",
        )
        .unwrap_err();
        assert!(e.to_string().contains("base_ur"), "{e}");
    }

    #[test]
    fn the_parse_error_does_not_claim_the_yaml_is_malformed() {
        // 那句话会让用户去找一个不存在的语法错误。
        let e = LoadError::Parse {
            path: PathBuf::from("config.yaml"),
            source: serde_yaml_ng::from_str::<Config>("version: 1\nnope: 1\n").unwrap_err(),
        };
        let m = e.to_string();
        assert!(!m.contains("不是合法的 YAML"), "{m}");
        assert!(m.contains("nope"), "{m}");
    }

    #[test]
    fn an_official_endpoint_is_recognised_by_host_not_by_name() {
        // **一个中转站可以把自己叫做 `anthropic-official`，但它没法让
        // 自己的 base_url 变成 `api.anthropic.com`。**「我信任这一家」是
        // 个安全判断，域名是这里唯一不可伪装的东西。
        let p = |name: &str, url: &str| Provider {
            name: name.into(),
            base_url: url.into(),
            ..Default::default()
        };
        assert!(p("随便叫", "https://api.anthropic.com").is_official_endpoint());
        assert!(p("x", "https://api.anthropic.com/v1/").is_official_endpoint());
        assert!(p("x", "https://bedrock-runtime.us-east-1.amazonaws.com").is_official_endpoint());

        // 名字骗不了人
        assert!(!p("anthropic-official", "https://relay.example.com").is_official_endpoint());
        // 把官方域名塞进路径里也骗不了
        assert!(!p("x", "https://evil.com/api.anthropic.com/v1").is_official_endpoint());
        // 塞进子域名也不行
        assert!(!p("x", "https://api.anthropic.com.evil.com").is_official_endpoint());
        // 塞进 userinfo 里同样不行
        assert!(!p("x", "https://api.anthropic.com@evil.com/v1").is_official_endpoint());
    }

    #[test]
    fn the_official_endpoint_is_not_redacted_and_a_relay_is() {
        // 核心分歧点：**走官方端点不该脱敏，走中转站才脱。**
        // 你让 Claude Code 调试一个 .env 问题，它得真看见里面的值。
        let official = Provider {
            base_url: "https://api.anthropic.com".into(),
            ..Default::default()
        };
        assert!(official.effective_redact().is_empty());
        assert_eq!(official.effective_trust(), Trust::Official);

        let relay = Provider {
            base_url: "https://relay.example.com".into(),
            ..Default::default()
        };
        assert!(!relay.effective_redact().is_empty());
        assert_eq!(relay.effective_trust(), Trust::Untrusted);
        // 默认不含 internal —— RFC1918 地址在代码和文档里到处都是
        assert!(
            !relay
                .effective_redact()
                .contains(&tw_redact::rules::Kind::Internal)
        );
        assert!(
            relay
                .effective_redact()
                .contains(&tw_redact::rules::Kind::ApiKeys)
        );
    }

    #[test]
    fn writing_it_down_explicitly_wins_over_the_guess() {
        let p = Provider {
            base_url: "https://api.anthropic.com".into(),
            redact: Some(vec![tw_redact::rules::Kind::Internal]),
            trust: Some(Trust::Untrusted),
            ..Default::default()
        };
        assert_eq!(p.effective_redact(), vec![tw_redact::rules::Kind::Internal]);
        assert_eq!(p.effective_trust(), Trust::Untrusted);
        // 显式写空列表 = 明确不脱，不是「没写」
        let q = Provider {
            base_url: "https://relay.example.com".into(),
            redact: Some(vec![]),
            ..Default::default()
        };
        assert!(q.effective_redact().is_empty());
    }

    #[test]
    fn an_unknown_upstream_defaults_to_the_safe_side() {
        // 布尔开关要命名成「零值 = 安全」。判不出来就是不受信任。
        assert_eq!(Trust::default(), Trust::Untrusted);
        assert!(Trust::default().blocks());
        assert!(!Trust::Official.blocks());
    }
}
