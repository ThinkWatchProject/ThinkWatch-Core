//! 配置：schema、加载、校验、初始生成。
//!
//! 只有一份文件，密钥明文写在里面。
//! M0 只做只读加载；双向同步是 M2 的事。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod chatgpt;
pub mod control_key;
pub mod credential;
pub mod edit;
pub mod history;
mod init;
pub mod nics;
pub mod private_dir;
mod probes;
pub mod proxy;
pub mod refs;
pub mod reload;
pub mod remote;
mod retention;
mod security;
pub mod store;
mod validate;
pub mod watch;
mod wire;

pub use credential::{CredentialError, Header, Headers, Secret, SecretResolveError, auth_header};
pub use init::{generate_control_key, generate_initial, generate_key};
pub use proxy::{DIRECT, OnProxyFail, Proxy, ProxyKind, SYSTEM};
pub use validate::ValidationError;

/// 配置文件格式的版本，和应用的 CalVer 是两回事。**不迁移**：格式变了就
/// 直接改，旧写法读不进来时报的是那个字段本身的错。它只挡住一种情况 ——
/// 更新版本的程序写的文件交给了旧程序。
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
    /// 两项防护：出站脱敏、工具调用审查。**出厂时都停在「观察」** ——
    /// 只记录，不改变任何行为。对所有上游一视同仁。
    #[serde(default, skip_serializing_if = "is_default")]
    pub security: Security,
    /// 日志留多久。不写就是默认值。
    #[serde(default, skip_serializing_if = "is_default")]
    pub retention: Retention,
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
    /// 没有为自己生成密钥的那些客户端用哪一把。
    ///
    /// **它是一个身份，不是「列表里的第一个」。**接管时不指定用哪把密钥，
    /// 落到的就是它；界面上它删不掉 —— 删了之后手动配置的客户端会在某个
    /// 说不清的时刻断掉。不写就是名字叫 `default` 的那把。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_key: Option<String>,
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
            retention: Retention::default(),
            groups: Vec::new(),
            routes: Vec::new(),
            default_route: None,
            default_key: None,
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
            client: None,
            disabled: false,
        }
    }
}

/// 同上。`name` / `base_url` 空着的 provider 过不了校验。
impl Default for Provider {
    fn default() -> Self {
        Self {
            name: String::new(),
            base_url: String::new(),
            key: None,
            headers: Headers::default(),
            oauth: None,
            protocol: None,
            proxy: default_proxy(),
            on_proxy_fail: OnProxyFail::default(),
            models: Vec::new(),
            models_only: None,
            billing: Billing::PerToken,
            pricing: None,
            disabled: false,
        }
    }
}

/// 名字叫这个的密钥就是默认那把 —— 前提是 `default_key` 没有明写别的
pub const DEFAULT_KEY: &str = "default";

impl Config {
    /// 没有为自己生成密钥的客户端用哪一把。
    ///
    /// 按顺序：`default_key` 指名的那把 → 名字叫 `default` 的 → 第一把。
    /// **最后这一条只是不让它返回空**：配置是首次启动生成的，那时就有一把
    /// 叫 `default` 的；能落到「第一把」的只有手写配置删掉了它的情形。
    pub fn default_client(&self) -> Option<&Client> {
        let named = self
            .default_key
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty());
        if let Some(n) = named
            && let Some(c) = self.clients.iter().find(|c| c.name == n)
        {
            return Some(c);
        }
        self.clients
            .iter()
            .find(|c| c.name == DEFAULT_KEY)
            .or_else(|| self.clients.first())
    }

    /// 为这个客户端留着的那把钥匙。**取消接管之后它还在**
    pub fn client_key(&self, client: &str) -> Option<&Client> {
        self.clients
            .iter()
            .find(|c| c.client.as_deref() == Some(client))
    }

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
    #[serde(default, skip_serializing_if = "is_default")]
    pub gateway: GatewayListen,
    /// 控制面。**钥匙在这里**，而钥匙每份配置都有 —— 所以 `listen` 这一段
    /// 从第一天起就在文件里，里面只有这一行。
    #[serde(default)]
    pub control: ControlListen,
}

/// 控制面怎么进。
///
/// 本机的通道（unix socket、Windows 的回环端口）不用配：它在哪儿由平台
/// 决定（`tw_api::control::Address`），这里只有进门的钥匙。
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlListen {
    /// 64 个十六进制字符。**每一种通道共用这一把**：握手（`tw-link`）拿它当
    /// PSK，不知道它的一方连第一条握手消息都写不对。
    ///
    /// `twcore serve` 在控制面起来之前保证它在（没有就生成、只补这一行）；
    /// 界面上看到的是打码的（`tw_api::control::KEY_MASK`），也改不了它 ——
    /// 只有 `twcore control-key --rotate` 和直接改文件能换。
    ///
    /// 这里是一串文字而不是 [`tw_api::control::ControlKey`]：写错了的时候，
    /// 校验要能说一句带码的话（[`ValidationError::ControlKeyInvalid`]），而
    /// 不是一句解析器的原话。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// 远程控制端口：给另一台机器上的桌面端用。**另开的**一个网络端口，本机的
    /// 通道（socket、Windows 的回环端口）照旧在；钥匙还是上面这一把。
    ///
    /// 不写，或者 `enabled: false`，就不听。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteListen>,
}

/// `listen.control.remote`。
///
/// ```yaml
/// remote:
///   enabled: true
///   bind: all               # 和网关同样的写法：loopback / all / 网卡名 / 地址
///   port: 23483             # 写进配置时随机生成
///   allow_from: [192.168.1.0/24]
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteListen {
    #[serde(default)]
    pub enabled: bool,
    /// 默认 `all`：这个端口存在的理由就是让别的机器连进来，只听回环等于没开。
    #[serde(default = "bind_all")]
    pub bind: Bind,
    /// **没有默认值**，写进配置时随机挑一个（[`generate_remote_port`]）：一个
    /// 人人都知道的固定端口只会招来更多扫描，而它也省不了谁一步 —— 连接时
    /// 反正要从服务器上抄钥匙，端口跟着一起抄。
    pub port: u16,
    /// 和网关一样：不写是私网段（[`default_allow_from`]），写成空列表就是只有
    /// 本机。名单之外的来源 accept 之后立刻关掉，一个字节都不回。
    #[serde(default = "default_allow_from")]
    pub allow_from: Vec<String>,
}

fn bind_all() -> Bind {
    Bind::All
}

/// 远程控制端口从哪一段里挑：不需要特权，也躲开系统分给临时连接的那一段
/// （Linux 默认 32768 起，Windows 49152 起）。
pub const REMOTE_PORT_RANGE: std::ops::RangeInclusive<u16> = 20000..=32000;

/// 随机挑一个远程控制端口，**避开网关的端口**。
pub fn generate_remote_port(gateway_port: u16) -> u16 {
    loop {
        let p = rand::random_range(REMOTE_PORT_RANGE);
        if p != gateway_port {
            return p;
        }
    }
}

/// **钥匙不打印。**`Config` 会整个落进 Debug 输出，而日志是会被贴进 issue 的。
impl std::fmt::Debug for ControlListen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlListen")
            .field("key", &self.key.as_ref().map(|_| "<redacted>"))
            .field("remote", &self.remote)
            .finish()
    }
}

impl ControlListen {
    /// 此刻的钥匙。校验过的配置里一定有；没有或写坏了是 `None`。
    pub fn key(&self) -> Option<tw_api::control::ControlKey> {
        tw_api::control::ControlKey::parse(self.key.as_deref()?).ok()
    }
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
    /// 除本机之外，哪些来源可以连（CIDR，或者单个地址）。**本机永远放行。**
    ///
    /// 不写就是私网段（[`default_allow_from`]），写成空列表就是只有本机。
    /// **没有「空 = 什么」的特例**：空列表若意味着「按私网段放行」，界面上
    /// 是一个空格子、实际放行的是四个网段，删掉最后一条反而把默认名单请了
    /// 回来。
    #[serde(default = "default_allow_from")]
    pub allow_from: Vec<String>,
}

/// 放行网段的默认名单：RFC1918 的三个私网段和 IPv6 的唯一本地地址段。
///
/// **不含回环。**本机永远放行（见 tw-gateway 的 `AllowList::allows`），写进
/// 名单只会在界面上多出两条删了也不起作用的条目。
///
/// 配置层用它填默认值，控制面把它交给界面「恢复默认」。
pub const PRIVATE_RANGES: &[&str] = &["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7"];

/// 放行网段的默认名单：私网段。**不是放行所有** —— 想放开得手动写
/// `0.0.0.0/0`，那时他至少知道自己做了什么。
pub fn default_allow_from() -> Vec<String> {
    PRIVATE_RANGES.iter().map(|s| s.to_string()).collect()
}

fn default_port() -> u16 {
    DEFAULT_GATEWAY_PORT
}

impl GatewayListen {
    /// 实际要监听的地址，**配置里写的那个排在最后**。
    ///
    /// **一个函数，不是几处各拼一遍。**bind 和 port 分开写的地方多了，
    /// 迟早有一处忘了跟着改 —— 而它的表现是「监听在了一个谁也没想到的
    /// 地址上」。
    ///
    /// **绑一张具体的网卡时，本机回环也要听。**只绑 `192.168.1.5` 的话，
    /// 连 `127.0.0.1` 的请求一律被拒 —— 而接管时写进客户端配置的正是
    /// `127.0.0.1`。用户选「局域网」是想让别的设备也能连，不是想让这台
    /// 电脑上的客户端全部断线。`0.0.0.0` 本来就包含回环，不用另加。
    pub fn addrs(&self) -> Result<Vec<std::net::SocketAddr>, BindError> {
        let ip = self.bind.resolve()?;
        let at = |ip| std::net::SocketAddr::new(ip, self.port);
        if ip.is_loopback() || ip.is_unspecified() {
            return Ok(vec![at(ip)]);
        }
        Ok(vec![
            at(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            at(ip),
        ])
    }
}

impl Default for GatewayListen {
    fn default() -> Self {
        Self {
            bind: Bind::default(),
            port: DEFAULT_GATEWAY_PORT,
            allow_from: default_allow_from(),
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
/// **只在局域网那张网卡上听，就写那张网卡的名字**（`en0`）。白名单只按
/// 来源地址放行，它替代不了「根本不在别的网卡上监听」。
///
/// 写名字而不是写地址，是因为地址会变：DHCP 续租、换个 Wi-Fi，
/// `192.168.1.5` 就不在了，网关起不来，而系统给的错误只有一句
/// 「Can't assign requested address」。名字不会变，网关每隔几秒现问系统它
/// 当下是哪个地址，变了就换过去；网卡暂时不在时先只听回环，等它出现。**写死的地址仍然收** —— 有人就是要钉住那一个。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Bind {
    #[default]
    Loopback,
    All,
    /// 一张具体的网卡，按名字。启动时解析。
    Nic(String),
    /// 一个写死的地址。**它会随网络变化失效**，而 [`Bind::Nic`] 不会。
    Addr(std::net::IpAddr),
}

/// `bind` 解析不出一个能监听的地址。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindError {
    /// **把机器上真有哪些网卡一并说出来。**名字是在加载配置时按形状收下
    /// 的（那时那张网卡可能正没插线），所以拼错要到这一刻才发现 ——
    /// 那就让这一刻的这句话直接给出答案，而不是让人再去跑一次 ifconfig。
    #[error("this machine has no interface named `{name}`; it has {}", available.join(", "))]
    NoSuchNic {
        name: String,
        available: Vec<String>,
    },
    /// 网卡在，但此刻用不了：网线拔了、Wi-Fi 没连上、还没拿到 DHCP。
    ///
    /// **不说「没有地址」。**没连上的网卡常常还挂着地址（静态配的、或者
    /// 一个没有容器接着的 `docker0`），[`nics::list`] 按「连着没有」把它们
    /// 筛掉了 —— 这时说「没有地址」是错的，而用户要做的事两种情况一样。
    #[error("interface `{name}` is not connected right now")]
    NicOffline { name: String },
}

impl Bind {
    /// 要监听的那个地址。**网卡名要问系统**，所以这一步可能失败。
    ///
    /// 每次都现问，不缓存：换了网络，下一次问拿到的就该是新地址（网关的
    /// 监听那一边每隔几秒问一次，见 `tw_gateway::serve_at`）。
    pub fn resolve(&self) -> Result<std::net::IpAddr, BindError> {
        match self {
            Bind::Loopback => Ok(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            Bind::All => Ok(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
            Bind::Addr(a) => Ok(*a),
            // 一张网卡有好几个地址时用哪一个，由 `nics::by_name` 说了算 ——
            // 界面上的选单列的也是它，选单上写的就是真要监听的那个
            Bind::Nic(name) => match nics::by_name().into_iter().find(|n| n.name == *name) {
                Some(n) => Ok(n.addr),
                None if nics::exists(name) => Err(BindError::NicOffline { name: name.clone() }),
                None => Err(BindError::NoSuchNic {
                    name: name.clone(),
                    available: nics::by_name().into_iter().map(|n| n.name).collect(),
                }),
            },
        }
    }

    /// 本机之外连得上吗。
    ///
    /// **这个判断决定了一件强制行为**：密钥校验不可关闭。
    ///
    /// 写死地址的那一档判的是地址本身而不是枚举变体 —— `bind: 127.0.0.1`
    /// 和 `loopback` 是同一件事，不该因为换了个写法就被当成暴露在外。
    ///
    /// **网卡名一律算暴露，不去解析。**这个判断要在任何时候都答得出，
    /// 包括那张网卡当下没有地址的时候；而它决定的是密钥强制 —— 答不上来
    /// 时错在保守那一侧，比为一个判断去做系统调用好。
    /// 代价是 `bind: lo0` 会被当成暴露，那没有坏处。
    pub fn is_exposed(&self) -> bool {
        match self {
            Bind::Loopback => false,
            Bind::All | Bind::Nic(_) => true,
            Bind::Addr(a) => !a.is_loopback(),
        }
    }
}

impl Bind {
    /// 给界面显示的地址字符串。`loopback`/`all` 展开成真实地址，写死的
    /// 地址就是它自己，网卡名解析得出来就给地址、解析不出来就给名字。
    pub fn socket_string(&self) -> String {
        match self.resolve() {
            Ok(a) => a.to_string(),
            Err(_) => self.to_string(),
        }
    }
}

impl std::fmt::Display for Bind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Bind::Loopback => f.write_str("loopback"),
            Bind::All => f.write_str("all"),
            Bind::Nic(n) => f.write_str(n),
            Bind::Addr(a) => write!(f, "{a}"),
        }
    }
}

/// 看起来像不像一个网卡名。
///
/// **只判形状，不问系统。**它拦不住把 `loopback` 拼成 `lookback` —— 那照样
/// 是个合法形状，会被当成网卡名收下，到启动时才发现。所以
/// [`BindError::NoSuchNic`] 要把真有哪些网卡说出来。
fn looks_like_nic(s: &str) -> bool {
    nic_shape(cfg!(windows), s)
}

/// 两个平台的网卡名**不是同一种东西**，所以形状各判各的。
///
/// - unix：内核给的短名。Linux 的 `IFNAMSIZ` 是 16，BSD 一系更短；字母数字加
///   `.` `-` `_`（`en0`、`utun3`、`br-a1b2`、`enp0s31f6`）。
/// - Windows：我们用的是网卡的显示名（见 `nics.rs`，那里特意没用 GUID），
///   也就是用户在「网络连接」里看到、能改的那个：`以太网`、`WLAN 2`、
///   `vEthernet (Default Switch)`。中文、空格、括号都是常态，按 unix 那套
///   判，这台机器上的每一张网卡都选不了。这边只挡明显不是名字的：空的、
///   首尾带空白的、带控制字符的、长得离谱的（系统上限是 256 个字符）。
///
/// **是个参数，不是就地一个 `cfg!`**：两套规则在任何一台机器上都测得到。
fn nic_shape(windows: bool, s: &str) -> bool {
    if windows {
        !s.is_empty()
            && s.chars().count() <= 256
            && s.trim() == s
            && !s.chars().any(char::is_control)
    } else {
        !s.is_empty()
            && s.len() < 16
            && s.starts_with(|c: char| c.is_ascii_alphabetic())
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
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
            other => {
                if let Ok(a) = other.parse() {
                    return Ok(Bind::Addr(a));
                }
                // **网卡名不在这里核实存不存在。**写配置的时候那张网卡
                // 可能正好没插线；真要监听的那一刻才问系统，那时给的是
                // 「没有叫 en9 的网卡」，比一句解析失败准确得多。
                //
                // 但形状要核：拼错成 `lookback` 的话，当成网卡名收下就
                // 成了一个到启动才炸的错误，而它本来可以在这里就说清楚。
                if looks_like_nic(other) {
                    return Ok(Bind::Nic(other.to_string()));
                }
                // **说清楚四种合法写法。**「invalid value」对着一个
                // 手写配置文件的人什么都没说，而这个字段写错的后果是
                // 整份配置加载失败、网关起不来。
                Err(serde::de::Error::custom(format!(
                    "bind takes loopback, all, the name of an interface such as en0, \
                     or an address such as 192.168.1.5; it reads {other}"
                )))
            }
        }
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
    /// 这把钥匙是为哪个客户端生成的（`claude-code` / `codex` …）。
    ///
    /// **写下来，而不是靠名字相同去猜。**接管时把密钥的值写进那个客户端
    /// 的配置文件，之后两边就没有别的联系了 —— 靠名字的话，用户改一次名
    /// 就对不上，自己建一把重名的又会被当成它的。
    ///
    /// 取消接管**不清空它**：那把钥匙仍然是为这个客户端留着的，下次接管
    /// 直接接着用，不必让用户再配一遍。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// 停用之后，用这把钥匙的请求一律拒绝。
    ///
    /// **和「一个模型都不给」不是一回事。**后者是模型范围为空，用户读到的
    /// 是「配错了」；而「临时停掉这个客户端」是一个正当的、要能一眼看出来
    /// 也能一键恢复的状态。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
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
/// 写回只动这几个标量（span 补丁），而且不进配置历史 —— 回滚到一次
/// 轮换之前拿到的是一个作废的 token，那不是可以退回去的状态。
///
/// # access token 也写在这里
///
/// 每次刷新之后，网关把新的 access token 和过期时间一起写回来（2026-09-18 定）。
/// 重启之后手里那个没过期就直接用：不用先换一次 token，也就不会每次重启都轮换
/// 一次 refresh token、写一次文件；启动时 token 端点一时连不上，请求也照样能发。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuth {
    /// access token。**可选** —— 不写就在第一次用到时拿 refresh 换一个，换来的写回这里
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<String>,
    /// `access` 什么时候过期，RFC 3339（UTC）。不知道的话就一直用到上游回 401
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    /// API 密钥，按接口协议放进它认的请求头。可以写 `${ENV}`。**不做 keychain。**
    ///
    /// 不需要密钥的上游（本地 Ollama）不写；密钥要放在别的头里时写在 `headers`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<Secret>,
    /// 其余要发的请求头，或者自己决定凭据怎么发。见 [`credential`]。
    #[serde(default, skip_serializing_if = "Headers::is_empty")]
    pub headers: Headers,
    /// OAuth：access token 由 refresh token 换发，默认放进协议的鉴权头。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuth>,
    /// 不写就从 base_url 猜（最小配置）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
    /// 代理名，或内置的 `direct` / `system`。
    ///
    /// **默认 `direct` 而不是 `system`**：显式优于隐式。默认跟随系统的
    /// 话，用户在系统里开了全局代理，本地 Ollama 就会莫名连不上，而
    /// 配置文件里看不出任何线索。
    #[serde(default = "default_proxy", skip_serializing_if = "is_direct")]
    pub proxy: String,
    #[serde(default, skip_serializing_if = "is_default_on_proxy_fail")]
    pub on_proxy_fail: OnProxyFail,
    /// 探测不到时的兜底清单。
    ///
    /// 有些中转站没实现 `/v1/models`。**这是 provider 级的「这家有什么」，
    /// 不是全局的「我们对外暴露什么」** —— 那个由汇总推导出来。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// 只用这家的这些模型：模型 ID 或 glob。不写就是它提供的全部。
    ///
    /// **和 `models` 说的不是一件事**：`models` 说这家有什么，这里说我们
    /// 用它的哪些。范围外的模型不出现在 `/v1/models` 里，路由也不会把
    /// 它们的请求交给这家。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_only: Option<Vec<String>>,
    /// 这家怎么收钱：按量计费（默认，按价目表算）或不计费。
    ///
    /// **订阅账号也按量计费。**费用一律是「用量 × 这家所选价目表里该模型的
    /// 单价」，订阅账号算出来的就是按 API 价格折算的费用；额度另从响应头读，
    /// 和计费无关。想让它的费用记 0，写 `free`。
    #[serde(default, skip_serializing_if = "is_default")]
    pub billing: Billing,
    /// 按哪张价目表计价。不写就是默认价目表。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<String>,
    /// 停用。**配置原样留着**：不参与路由，它的模型也不出现在
    /// `/v1/models` 里。要暂时不用一家上游时，比删掉再重新填一遍凭据好。
    #[serde(default, skip_serializing_if = "is_default")]
    pub disabled: bool,
}

impl Provider {
    /// 这家的这个模型在不在启用范围里（`models_only`）。**不管这家到底
    /// 有没有这个模型** —— 那要看模型目录。
    pub fn uses_model(&self, model: &str) -> bool {
        match &self.models_only {
            None => true,
            Some(patterns) => patterns
                .iter()
                .any(|p| tw_engine::rule::glob_match(p, model)),
        }
    }
}

/// 上游怎么收钱。
///
/// **只有两档。**费用只取决于用量和价目表：按量计费的按价目表算，不计费的
/// 记 $0。上游是不是订阅账号不影响费用 —— 那件事由它报的额度说。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Billing {
    /// 按价目表计价。默认
    #[default]
    PerToken,
    /// 不计费：本地模型、免费额度。**费用记 $0，是一个确定的数**
    Free,
}

impl Billing {
    pub fn label(&self) -> &'static str {
        match self {
            Billing::PerToken => "per token",
            Billing::Free => "free",
        }
    }
    pub fn slug(&self) -> &'static str {
        match self {
            Billing::PerToken => "per-token",
            Billing::Free => "free",
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
    /// ChatGPT 账号：Codex 后端。说的是 OpenAI Responses 格式，但只接受流式、
    /// 不认 `max_output_tokens`、身份头由网关填（见 [`chatgpt`]）
    Chatgpt,
}

impl Protocol {
    /// 写进 YAML 的那个词。
    pub fn slug(&self) -> &'static str {
        match self {
            Protocol::Anthropic => "anthropic",
            Protocol::OpenaiChat => "openai-chat",
            Protocol::OpenaiResponses => "openai-responses",
            Protocol::Gemini => "gemini",
            Protocol::Chatgpt => "chatgpt",
        }
    }
}

impl Provider {
    /// 从 base_url 猜协议。猜不出来返回 None —— **不猜一个默认值**，
    /// 因为猜错的表现是「请求发出去了但上游 400」，比直接说不知道难查。
    pub fn guess_protocol(base_url: &str) -> Option<Protocol> {
        if chatgpt::is_backend(base_url) {
            return Some(Protocol::Chatgpt);
        }
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
}

/// 刷新之后要写回配置的值。
#[derive(Debug, Clone, Copy)]
pub struct RenewedTokens<'a> {
    pub access: &'a str,
    /// RFC 3339（UTC）。服务器没说有效期就是 `None`，那时删掉配置里旧的过期时间
    pub expires_at: Option<&'a str>,
    /// 服务器换发了新的 refresh token 才有
    pub refresh: Option<&'a str>,
}

/// 把刷新换回来的 token 写回 config.yaml：access token、过期时间，以及换发的
/// refresh token（有的话）。
///
/// # 为什么要写回
///
/// **服务器换发新 refresh token 的那一刻，旧的已经在服务端作废了。**
/// 所以「不写回」不是保守选项 —— 它保证了配置文件从那一秒起就是坏的，
/// 只是症状延迟到下一次重启（表现是这家上游突然全是 401，而那时没人
/// 会想到是几天前的一次轮换）。写回才是安全的那一边。access token 写回的理由
/// 见 [`OAuth`]。
///
/// # 为什么是 span 补丁而不是 serde 往返
///
/// 往返会把用户的注释、空行、字段顺序全洗掉 —— 而这是**用户没要求的
/// 一次写入**，它必须只动它该动的那几个字段。cc-switch 那 147 个
/// commit 的白名单教训在这里同样成立：我们要写什么是清楚的，
/// 「要保留什么」永远数不完。
///
/// `oauth.refresh` 找不到就报错，**不追加**：那说明我们猜错了结构，而在一个装着
/// 明文密钥的文件里猜结构是不能接受的。`access` 和 `expires_at` 是可选字段，
/// 没写过就加在 `oauth` 下面。
pub fn patch_oauth_tokens(
    text: &str,
    provider: &str,
    tokens: RenewedTokens<'_>,
) -> Result<String, RotateError> {
    // 名字对应第几个 provider —— 从**文本本身**数，不从解析后的结构数。
    // 两者理论上一致，但真正要动的是文本里的那个位置。
    let idx = provider_index(text, provider).ok_or_else(|| RotateError::NoProvider {
        provider: provider.to_string(),
    })?;
    let shape = |source| RotateError::Shape {
        provider: provider.to_string(),
        source,
    };
    let refresh_path = tw_yaml::path!["providers", idx, "oauth", "refresh"];
    // 先确认它在那儿：不在那儿本身就是「别写」的理由
    tw_yaml::find(text, &refresh_path).map_err(shape)?;
    let mut out = text.to_string();
    if let Some(r) = tokens.refresh {
        out = tw_yaml::set(&out, &refresh_path, &tw_yaml::Scalar::s(r)).map_err(shape)?;
    }
    let access_path = tw_yaml::path!["providers", idx, "oauth", "access"];
    out = tw_yaml::insert(&out, &access_path, &tw_yaml::Scalar::s(tokens.access)).map_err(shape)?;
    let expires_path = tw_yaml::path!["providers", idx, "oauth", "expires_at"];
    out = match tokens.expires_at {
        Some(at) => tw_yaml::insert(&out, &expires_path, &tw_yaml::Scalar::s(at)).map_err(shape)?,
        // 新 token 的有效期不知道：旧的过期时间留着只会让它被当成旧 token 的寿命
        None if tw_yaml::find(&out, &expires_path).is_ok() => {
            tw_yaml::remove_key(&out, &expires_path).map_err(shape)?
        }
        None => out,
    };
    // **写之前先自己读一遍。**patch 出来的东西必须还是一份能加载的配置，
    // 而且那几个字段真的变成了新值 —— 否则我们会把一份坏配置留在盘上，
    // 而用户下一次启动才撞上它（「先校验再写」同一条）。
    let re = try_parse(&out).map_err(|r| RotateError::Broke {
        provider: provider.to_string(),
        why: r.msg(),
    })?;
    let ok = re
        .providers
        .iter()
        .find(|p| p.name == provider)
        .and_then(|p| p.oauth.as_ref())
        .is_some_and(|o| {
            o.access.as_deref() == Some(tokens.access)
                && o.expires_at.as_deref() == tokens.expires_at
                && tokens.refresh.is_none_or(|r| o.refresh == r)
        });
    if !ok {
        return Err(RotateError::Broke {
            provider: provider.to_string(),
            why: tw_types::msg!(
                "config.rotate.read_back_differs" =>
                "the token read back after writing is not the new one"
            ),
        });
    }
    Ok(out)
}

/// `providers` 里第几个叫这个名字。
fn provider_index(text: &str, provider: &str) -> Option<usize> {
    let cfg: Config = serde_yaml_ng::from_str(text).ok()?;
    cfg.providers.iter().position(|p| p.name == provider)
}

/// 轮换之后写回凭据没成。**英文只写一遍**：`Display` 就是 [`RotateError::msg`] 的原句。
#[derive(Debug, thiserror::Error)]
pub enum RotateError {
    #[error("{}", self.msg())]
    NoProvider { provider: String },
    /// **说清「形状不对」而不是「写失败」。**用户可能把凭据写成了
    /// 别的形状（锚点、块标量），那时正确的动作是他自己去改，
    /// 而不是让我们猜。
    #[error("{}", self.msg())]
    Shape {
        provider: String,
        #[source]
        source: tw_yaml::PatchError,
    },
    #[error("{}", self.msg())]
    Broke {
        provider: String,
        why: tw_types::Msg,
    },
}

impl RotateError {
    /// 给人看的那句话，带码。原因自己带码，放进「哪个上游」这个场合
    pub fn msg(&self) -> tw_types::Msg {
        match self {
            RotateError::NoProvider { provider } => tw_types::msg!(
                "config.rotate.no_provider", provider = provider =>
                "the configuration no longer has an upstream `{provider}`"
            ),
            RotateError::Shape { provider, source } => source.msg().in_context(
                "provider",
                provider,
                &format!("oauth.refresh of upstream `{provider}` could not be located"),
            ),
            RotateError::Broke { provider, why } => why.clone().in_context(
                "provider",
                provider,
                &format!(
                    "after writing the new credential for upstream `{provider}` the \
                     configuration could not be read, so nothing was written"
                ),
            ),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("{path} could not be read: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// 语法错和字段名写错都走这里。**说「不是合法的 YAML」是错的** ——
    /// 一份语法完美、只是把 `port` 写成 `prot` 的文件也会到这儿，而那句
    /// 话会让用户去找一个根本不存在的语法错误。serde 自己的
    /// 「unknown field `prot`, expected one of ...」比我们能补的任何话都准。
    #[error("{path} could not be parsed: {source}")]
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
    #[error("{path} could not be written: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("serialization failed: {0}")]
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
pub use retention::Retention;
pub use security::{
    ContentAction, ContentMatch, ContentPolicy, CustomContentRule, CustomRedactRule,
    CustomToolRule, DEFAULT_MAX_CHARS, HiddenPolicy, MAX_CHARS_CEILING, Mode as SecurityMode,
    OutputLimitPolicy, RedactPolicy, Security, ToolAction, ToolPolicy,
};
// Billing 在本文件里定义，这里不必再导出
pub use store::{Fingerprint, Loaded, StoreError, version_of};
pub use validate::validate;

pub fn default_path() -> PathBuf {
    tw_api::data::dir().join("config.yaml")
}

#[cfg(test)]
mod tests {

    /// 保留期出厂不写进文件，写了要认得出来。
    ///
    /// **默认值不进文件**是这份配置的一条规矩：用户打开 config.yaml
    /// 看到一屏自己没配过的东西，就分不清哪些是他的决定。
    #[test]
    fn retention_defaults_stay_out_of_the_file_but_a_choice_does_not() {
        let cfg = Config::default();
        let out = serde_yaml_ng::to_string(&cfg).unwrap();
        assert!(!out.contains("retention"), "默认值被写进文件了：{out}");

        let text = "version: 1\nretention:\n  body_days: 3\n";
        let back: Config = serde_yaml_ng::from_str(text).unwrap();
        assert_eq!(back.retention.body_days, 3);
        // 没写的那两个仍然是默认值，不是 0 —— 0 会让 gc 把一切都删掉
        assert_eq!(back.retention.row_days, 90);
        assert_eq!(back.retention.body_max_bytes, 2 * 1024 * 1024 * 1024);
        assert!(
            serde_yaml_ng::to_string(&back)
                .unwrap()
                .contains("body_days: 3")
        );
    }

    /// 写错字段名要报错，不能悄悄忽略。
    #[test]
    fn a_typo_in_retention_is_rejected_rather_than_ignored() {
        // 悄悄忽略的话，用户改了个期限，界面说写成功了，什么都没发生
        let text = "version: 1\nretention:\n  body_dayz: 3\n";
        assert!(serde_yaml_ng::from_str::<Config>(text).is_err());
    }
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
        assert_eq!(cfg.listen.gateway.allow_from, default_allow_from());
    }

    #[test]
    fn billing_is_per_token_or_free_and_nothing_else() {
        let parse = |billing: &str| {
            serde_yaml_ng::from_str::<Config>(&format!(
                "version: 1\nproviders:\n  - {{ name: p, base_url: https://api.example.com{billing} }}\n"
            ))
        };
        // 不写就是按量计费，写回去也不多出这一行
        let unset = parse("").unwrap();
        assert_eq!(unset.providers[0].billing, Billing::PerToken);
        assert!(
            !serde_yaml_ng::to_string(&unset)
                .unwrap()
                .contains("billing")
        );
        assert_eq!(
            parse(", billing: free").unwrap().providers[0].billing,
            Billing::Free
        );
        // 订阅账号也按价目表算费用，「订阅」和「未知」不再是计费方式
        for gone in ["subscription", "unknown"] {
            assert!(
                parse(&format!(", billing: {gone}")).is_err(),
                "{gone} 还被接受"
            );
        }
    }

    #[test]
    fn the_allow_list_is_exactly_what_is_written() {
        // 不写是私网段；写成空的就是空的（只有本机），不会被悄悄填回默认名单
        let parse = |text: &str| serde_yaml_ng::from_str::<Config>(text).unwrap();
        let unset = parse("version: 1\nlisten:\n  gateway:\n    bind: all\n");
        assert_eq!(unset.listen.gateway.allow_from, default_allow_from());
        assert!(
            !unset
                .listen
                .gateway
                .allow_from
                .iter()
                .any(|r| r == "0.0.0.0/0"),
            "默认名单不是放行所有"
        );
        let empty = parse("version: 1\nlisten:\n  gateway:\n    bind: all\n    allow_from: []\n");
        assert!(empty.listen.gateway.allow_from.is_empty());
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
        assert_eq!(
            Provider::guess_protocol("https://chatgpt.com/backend-api/codex"),
            Some(Protocol::Chatgpt)
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
            key: Some(Secret::new("k")),
            protocol: Some(Protocol::OpenaiChat),
            ..Default::default()
        };
        assert_eq!(p.effective_protocol(), Some(Protocol::OpenaiChat));
    }

    #[test]
    fn bind_defaults_to_loopback_not_all_interfaces() {
        // 默认监听 0.0.0.0 会把网关暴露给整个局域网，而用户不会知道。
        assert_eq!(Bind::default().resolve().unwrap().to_string(), "127.0.0.1");
        assert_eq!(Bind::All.resolve().unwrap().to_string(), "0.0.0.0");
        assert!(!Bind::default().is_exposed());
        assert!(Bind::All.is_exposed());
    }

    #[test]
    fn bind_accepts_a_concrete_interface_address() {
        // 想「只在局域网那张网卡上听」，就写那张网卡的地址或名字 ——
        // 绑 0.0.0.0 会连公网那张一起听。
        let b: Bind = serde_yaml_ng::from_str("192.168.1.5").unwrap();
        assert_eq!(b, Bind::Addr("192.168.1.5".parse().unwrap()));
        assert_eq!(b.socket_string(), "192.168.1.5");
        assert!(b.is_exposed());
    }

    #[test]
    fn a_concrete_interface_is_listened_on_alongside_loopback() {
        // 只绑网卡地址的话，接管时写进客户端的 127.0.0.1 就连不上了
        let listen = |bind: &str| GatewayListen {
            bind: serde_yaml_ng::from_str(bind).unwrap(),
            port: 18790,
            allow_from: Vec::new(),
        };
        let addrs = |bind: &str| -> Vec<String> {
            listen(bind)
                .addrs()
                .unwrap()
                .iter()
                .map(|a| a.to_string())
                .collect()
        };
        assert_eq!(
            addrs("192.168.1.5"),
            ["127.0.0.1:18790", "192.168.1.5:18790"]
        );
        // 这几种本来就连得上回环，不重复监听
        assert_eq!(addrs("loopback"), ["127.0.0.1:18790"]);
        assert_eq!(addrs("127.0.0.1"), ["127.0.0.1:18790"]);
        assert_eq!(addrs("all"), ["0.0.0.0:18790"]);
    }

    #[test]
    fn writing_the_loopback_address_out_longhand_is_not_exposure() {
        // `is_exposed` 判的是地址，不是枚举变体 —— 换个写法不该让
        // 密钥校验被强制、白名单被自动填上。
        let b: Bind = serde_yaml_ng::from_str("127.0.0.1").unwrap();
        assert!(!b.is_exposed());
    }

    /// 写名字而不是写地址，图的就是它熬得过换网络：地址会变，`en0` 不变。
    #[test]
    fn bind_takes_an_interface_by_name() {
        let b: Bind = serde_yaml_ng::from_str("en0").unwrap();
        assert_eq!(b, Bind::Nic("en0".into()));
        // **不去解析就要算暴露。**这个判断决定密钥强制和白名单默认值，
        // 而那张网卡此刻可能没有地址 —— 答不上来时错在保守那一侧
        assert!(b.is_exposed());
    }

    /// 名字要在启动时解析成当下的地址。这台机器上一定有回环，拿它当样本。
    #[test]
    fn a_name_resolves_to_whatever_address_that_interface_has_now() {
        let lo = nics::list()
            .into_iter()
            .find(|n| n.addr.is_loopback())
            .expect("一台机器不可能没有回环网卡");
        let b = Bind::Nic(lo.name.clone());
        assert_eq!(b.resolve().unwrap(), lo.addr);
    }

    #[test]
    fn a_name_that_is_not_here_says_what_is() {
        let e = Bind::Nic("zzz0".into()).resolve().unwrap_err().to_string();
        assert!(e.contains("zzz0"), "{e}");
        // 拼错要到这一刻才发现，所以这一刻得给出答案，而不是让人去跑 ifconfig
        let lo = nics::list().into_iter().next().expect("至少有一张网卡");
        assert!(e.contains(&lo.name), "没把机器上真有的网卡说出来：{e}");
    }

    /// Windows 上的网卡名是显示名：中文、空格、括号都是常态。按 unix 那套
    /// 判的话，一台中文 Windows 上的每一张网卡都选不了 —— 真机上就是这么发现的。
    #[test]
    fn windows_interface_names_are_display_names() {
        for name in ["以太网", "WLAN 2", "vEthernet (Default Switch)", "Wi-Fi"] {
            assert!(nic_shape(true, name), "{name}");
        }
        for bad in ["", " 以太网", "以太网 ", "a\nb"] {
            assert!(!nic_shape(true, bad), "{bad:?}");
        }
        // unix 那边还是原来那套：内核给的短名
        for name in ["en0", "utun3", "br-a1b2", "enp0s31f6"] {
            assert!(nic_shape(false, name), "{name}");
        }
        for bad in [
            "以太网",
            "WLAN 2",
            "vEthernet (Default Switch)",
            "0en",
            "abcdefghijklmnop",
        ] {
            assert!(!nic_shape(false, bad), "{bad}");
        }
    }

    /// 这样的名字写进配置、再读回来还是它自己 —— 空格、括号、中文都要
    /// 过得了 YAML 那一关。写配置的是 `edit::set`（界面保存走的那一条）。
    /// 写和读在哪都测；读成网卡名只在 Windows 上成立。
    #[test]
    fn a_windows_interface_name_survives_the_config_file() {
        let at = [
            tw_yaml::Step::key("listen"),
            tw_yaml::Step::key("gateway"),
            tw_yaml::Step::key("bind"),
        ];
        for name in ["以太网", "vEthernet (Default Switch)", "WLAN 2"] {
            let text = edit::set(
                "version: 1\n",
                &at,
                Some(&serde_yaml_ng::Value::String(name.to_string())),
            )
            .unwrap();
            let cfg: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text).unwrap();
            let raw = cfg["listen"]["gateway"]["bind"]
                .as_str()
                .unwrap()
                .to_string();
            assert_eq!(raw, name, "写进去再读回来变了样：{text}");
            if cfg!(windows) {
                let b: Bind =
                    serde_yaml_ng::from_value(cfg["listen"]["gateway"]["bind"].clone()).unwrap();
                assert_eq!(b, Bind::Nic(name.to_string()), "{text}");
            }
        }
    }

    #[test]
    fn every_bind_form_round_trips_through_yaml() {
        for raw in ["loopback", "all", "en0", "192.168.1.5", "::1"] {
            let b: Bind = serde_yaml_ng::from_str(raw).unwrap();
            let back = serde_yaml_ng::to_string(&b).unwrap();
            assert_eq!(back.trim(), raw, "{raw} 写回来变了样");
        }
    }

    #[test]
    fn a_bind_typo_says_what_the_forms_are() {
        // 这个字段写错的后果是整份配置加载失败、网关起不来 ——
        // 那条错误必须自带答案。**开头带空格的在两个平台上都不是网卡名**，
        // 所以它走不到「当成网卡名收下」那条路上（Windows 的网卡名什么字符
        // 都可能有，`@` 挡不住它）
        let e = serde_yaml_ng::from_str::<Bind>("\" 192.168.1.5@wifi\"")
            .unwrap_err()
            .to_string();
        assert!(e.contains("loopback"), "{e}");
        assert!(e.contains("all"), "{e}");
        assert!(e.contains("en0"), "{e}");
        assert!(e.contains("192.168.1.5"), "{e}");
        assert!(
            e.contains(" 192.168.1.5@wifi"),
            "没把写错的那个词说出来：{e}"
        );
    }

    #[test]
    fn a_key_is_a_bare_string() {
        // 配置文件里绝大多数人写的就是一个裸字符串
        let cfg: Config = serde_yaml_ng::from_str(MINIMAL).unwrap();
        assert_eq!(
            cfg.providers[0].key.as_ref().map(Secret::raw),
            Some("sk-xxx")
        );
    }

    #[test]
    fn describe_never_leaks_a_literal_key() {
        // 这个方法会出现在 UI、日志、错误信息里。
        let s = Secret::new("sk-ant-api03-verysecretvalue");
        let d = s.describe();
        assert!(!d.contains("verysecret"), "{d}");
        assert!(d.contains('…'), "{d}");
    }

    #[test]
    fn describe_shows_the_env_var_name_not_its_value() {
        // 变量名不是秘密，而它恰恰是用户排查时要看的东西。
        assert_eq!(
            Secret::new("${MY_KEY}").describe(),
            "environment variable ${MY_KEY}"
        );
    }

    #[test]
    fn env_interpolation_reaches_the_key() {
        unsafe { std::env::set_var("TW_TEST_KEY", "sk-from-env") };
        let p = Provider {
            name: "x".into(),
            base_url: "https://x".into(),
            key: Some(Secret::new("${TW_TEST_KEY}")),
            ..Default::default()
        };
        assert_eq!(
            p.outbound_headers(None).unwrap(),
            vec![("x-api-key".to_string(), "sk-from-env".to_string())]
        );
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
}
