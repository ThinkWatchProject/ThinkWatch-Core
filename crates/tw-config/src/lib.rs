//! 配置：schema、加载、校验、初始生成。
//!
//! 只有一份文件（DESIGN.md §3.1），密钥明文写在里面（§3.2）。
//! M0 只做只读加载；双向同步是 M2 的事。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
pub use tw_types::Limits;

pub mod history;
mod init;
mod probes;
pub mod proxy;
pub mod reload;
pub mod store;
mod validate;
pub mod watch;

pub use init::{generate_initial, generate_key, generate_with_provider};
pub use proxy::{DIRECT, OnProxyFail, Proxy, ProxyKind, SYSTEM};
pub use validate::ValidationError;

/// 配置 schema 的版本。和应用的 CalVer 是两回事 —— 这个只决定要不要
/// 跑迁移（§9.6）。
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
    /// **默认值不写进文件。** §3.3 承诺第一天的配置是六行，而每加一个
    /// 带默认值的字段就会往文件里多堆几行 —— 用户打开配置看到一屏自己
    /// 没配过的东西，就分不清哪些是他的决定、哪些只是默认。
    #[serde(default, skip_serializing_if = "is_default")]
    pub listen: Listen,
    /// 客户端身份。密钥即身份（§3.3.1）—— 不是「先认证再看是谁」，
    /// 而是「这把钥匙就是这个人」。
    ///
    /// **不写这一段不是语法错误。**serde 的「missing field `clients`」
    /// 说不出下一步做什么，而 `validate` 那句「首次启动本应自动生成
    /// 一把」能。缺字段的判断交给它。
    #[serde(default)]
    pub clients: Vec<Client>,
    /// **同样可以整段不写。**§3.3 承诺第一天的配置是六行，而零 provider
    /// 是一个合法状态（首次运行就是它）—— 逼用户写一行 `providers: []`
    /// 只是为了让解析器高兴。
    #[serde(default)]
    pub providers: Vec<Provider>,
    /// 策略组。不写就没有 —— 层 0（只配 provider）是完全合法的配置，
    /// 引擎内部会把它展开（§3.4）。
    /// 出站代理。声明一次到处引用（§3.7）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proxies: Vec<Proxy>,
    /// 客户端自己发的辅助请求怎么处理（§4.8）。默认只拦 A 类。
    #[serde(default, skip_serializing_if = "is_default")]
    pub client_probes: ClientProbes,
    /// 并发上限。不写就是默认值（§4.7）。
    #[serde(default, skip_serializing_if = "is_default")]
    pub limits: Limits,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<tw_engine::Group>,
    /// 路由规则。同上，不写就是「按声明顺序故障转移」。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<tw_engine::Route>,
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
            client_probes: ClientProbes::default(),
            limits: Limits::default(),
            groups: Vec::new(),
            routes: Vec::new(),
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
            proxy: default_proxy(),
            on_proxy_fail: OnProxyFail::default(),
        }
    }
}

impl Config {
    /// 按配置建一个路由引擎。
    pub fn engine(&self) -> tw_engine::Engine {
        tw_engine::Engine::new(
            self.providers.iter().map(|p| p.name.clone()).collect(),
            self.groups.clone(),
            self.routes.clone(),
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
    /// loopback | lan | all | 具体 IP。默认 loopback —— 学 Surge，
    /// 但默认值要保守（§5.4）。
    #[serde(default)]
    pub bind: Bind,
    #[serde(default = "default_port")]
    pub port: u16,
    /// 仅 lan / all 时生效的来源白名单（CIDR）。
    #[serde(default)]
    pub allow_from: Vec<String>,
}

fn default_port() -> u16 {
    DEFAULT_GATEWAY_PORT
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Bind {
    #[default]
    Loopback,
    Lan,
    All,
}

impl Bind {
    pub fn addr(&self) -> &'static str {
        match self {
            Bind::Loopback => "127.0.0.1",
            // lan 和 all 在监听层是同一件事；区别在 allow_from 的默认
            // 值和 UI 上的措辞（§5.4）。
            Bind::Lan | Bind::All => "0.0.0.0",
        }
    }

    /// 非 loopback 吗。
    ///
    /// **这个判断决定了两件强制行为**（§5.4）：密钥校验不可关闭，
    /// 以及 `allow_from` 为空时自动填私网段。
    pub fn is_exposed(&self) -> bool {
        !matches!(self, Bind::Loopback)
    }
}

impl GatewayListen {
    /// 实际生效的来源白名单。
    ///
    /// **`lan` / `all` 且用户没写白名单时，默认填私网段**（§5.4）——
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
    /// 这个客户端自己的并发上限。**监听局域网时是刚需**（§4.7）——
    /// 某台机器上的失控脚本不该能占满全部并发。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<usize>,
    /// 这个客户端能看到哪些模型（§3.9）。
    ///
    /// **三种状态语义分明**，因为 one-api 和 new-api 在这里正好相反：
    ///
    /// - 不写（`None`）→ 只按方言过滤。默认
    /// - 写非空 → 在方言过滤基础上再按 glob 保留
    /// - 写 `[]` → **一个都不给**。「临时禁用这个客户端」的正当用法
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    /// 网关密钥。`tw-` 前缀是刻意的：用户在客户端配置里看到它时，
    /// 一眼就知道这不是某个上游的真 key（§5.4）。
    pub key: String,
}

/// 密钥怎么来。
///
/// 三种形态，**刻意按「用户会不会用到」排序**：绝大多数人写一个字符串
/// 就完了（§3.2 明确说了密钥就明文写在配置里，不做 keychain）；`${ENV}`
/// 给不想让密钥落到文件里的人；`exec` 给真的把密钥放在 1Password /
/// pass 里的人。
///
/// serde 的 untagged 让前两种都是裸字符串 —— 配置文件里看不出区别，
/// 也不该看出区别。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Secret {
    /// 明文，或含 `${ENV}` 的字符串
    Literal(String),
    /// 跑一条命令，拿 stdout。**不过 shell**，见 tw_secret::run_exec。
    Exec {
        exec: Vec<String>,
        /// 秒。不写用默认值 —— 卡住的凭据命令会让网关整个没反应。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
    },
}

impl Secret {
    /// 拿到真正的密钥。
    ///
    /// **每次调用都会重新跑 exec**。不缓存是有意的：`op read` 那类命令
    /// 背后是一个会过期的会话，缓存住会让「昨天还好好的，今天全是 401」
    /// 变得无法解释。真需要缓存时，那是一个显式的 TTL 配置，不是默认行为。
    pub fn resolve(&self) -> Result<String, SecretResolveError> {
        match self {
            Secret::Literal(s) => Ok(tw_secret::expand_from_env(s)?),
            Secret::Exec { exec, timeout_secs } => {
                let t = timeout_secs
                    .map(std::time::Duration::from_secs)
                    .unwrap_or(tw_secret::exec::DEFAULT_TIMEOUT);
                Ok(tw_secret::run_exec(exec, t)?)
            }
        }
    }

    /// 给人看的形态，**永远不含真实密钥**。
    pub fn describe(&self) -> String {
        match self {
            Secret::Literal(s) if s.contains("${") => format!("环境变量 {s}"),
            Secret::Literal(s) => tw_secret::mask_secret(s),
            Secret::Exec { exec, .. } => format!("exec: {}", exec.join(" ")),
        }
    }

    pub(crate) fn is_blank(&self) -> bool {
        match self {
            Secret::Literal(s) => s.trim().is_empty(),
            Secret::Exec { exec, .. } => exec.is_empty(),
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
    #[error(transparent)]
    Exec(#[from] tw_secret::ExecError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    /// 明文、`${ENV}`、或 `{ exec: [...] }`。见 §3.2 —— 不做 keychain。
    pub key: Secret,
    /// 不写就从 base_url 猜（§3.3 的最小配置）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
    /// 代理名，或内置的 `direct` / `system`。
    ///
    /// **默认 `direct` 而不是 `system`**：显式优于隐式。默认跟随系统的
    /// 话，用户在系统里开了全局代理，本地 Ollama 就会莫名连不上，而
    /// 配置文件里看不出任何线索。
    /// 探测不到时的兜底清单（§3.9）。
    ///
    /// 有些中转站没实现 `/v1/models`。**这是 provider 级的「这家有什么」，
    /// 不是全局的「我们对外暴露什么」** —— 那个由汇总推导出来。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    #[serde(default = "default_proxy", skip_serializing_if = "is_direct")]
    pub proxy: String,
    #[serde(default, skip_serializing_if = "is_default_on_proxy_fail")]
    pub on_proxy_fail: OnProxyFail,
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

    /// 拿到真正的 key（展开 `${ENV}` 或跑 `exec`）。
    pub fn resolved_key(&self) -> Result<String, SecretResolveError> {
        self.key.resolve()
    }
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
/// **这不是 §3.8 的最小替换**。那一套是「改一个标量、其余字节原样不动」，
/// 用于日常改配置；这一套是从无到有生成，用于首次运行。两者混用会让
/// 「注释和格式原样保留」那条承诺失效。
///
/// 权限不能靠 umask 的运气：这个文件里有明文密钥（§3.2）。
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
    fn the_six_line_config_from_the_design_doc_actually_parses() {
        // §3.3 承诺「第一天的配置是六行」。如果这个测试挂了，那句话
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
        assert_eq!(Bind::default().addr(), "127.0.0.1");
        assert_eq!(Bind::All.addr(), "0.0.0.0");
    }

    #[test]
    fn a_bare_string_key_still_parses_the_way_it_always_did() {
        // untagged 的第一条：配置文件里绝大多数人写的还是一个裸字符串，
        // 而且不该看出这里有个枚举。
        let cfg: Config = serde_yaml_ng::from_str(MINIMAL).unwrap();
        assert!(matches!(cfg.providers[0].key, Secret::Literal(ref s) if s == "sk-xxx"));
    }

    #[test]
    fn an_exec_key_parses_and_never_shows_the_secret() {
        let y = r#"
version: 1
clients:
  - { name: default, key: tw-1 }
providers:
  - name: p
    base_url: https://x.com
    key:
      exec: ["op", "read", "op://vault/anthropic/key"]
"#;
        let cfg: Config = serde_yaml_ng::from_str(y).unwrap();
        match &cfg.providers[0].key {
            Secret::Exec { exec, timeout_secs } => {
                assert_eq!(exec[0], "op");
                assert!(timeout_secs.is_none());
            }
            other => panic!("{other:?}"),
        }
        // describe 是给人看的，必须不含真实密钥 —— 这里它连密钥都还没跑
        assert!(cfg.providers[0].key.describe().starts_with("exec:"));
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
    fn an_exec_key_actually_runs() {
        let s = Secret::Exec {
            exec: vec!["echo".into(), "sk-1".into()],
            timeout_secs: None,
        };
        assert_eq!(s.resolve().unwrap(), "sk-1");
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
}
