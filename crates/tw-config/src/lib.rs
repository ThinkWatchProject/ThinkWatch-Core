//! 配置：schema、加载、校验、初始生成。
//!
//! 只有一份文件（DESIGN.md §3.1），密钥明文写在里面（§3.2）。
//! M0 只做只读加载；双向同步是 M2 的事。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

mod init;
mod validate;

pub use init::{generate_initial, generate_key, generate_with_provider};
pub use validate::ValidationError;

/// 配置 schema 的版本。和应用的 CalVer 是两回事 —— 这个只决定要不要
/// 跑迁移（§9.6）。
pub const SCHEMA_VERSION: u32 = 1;

pub const DEFAULT_GATEWAY_PORT: u16 = 8788;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub listen: Listen,
    /// 客户端身份。密钥即身份（§3.3.1）—— 不是「先认证再看是谁」，
    /// 而是「这把钥匙就是这个人」。
    pub clients: Vec<Client>,
    pub providers: Vec<Provider>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Listen {
    #[serde(default)]
    pub gateway: GatewayListen,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Client {
    pub name: String,
    /// 网关密钥。`tw-` 前缀是刻意的：用户在客户端配置里看到它时，
    /// 一眼就知道这不是某个上游的真 key（§5.4）。
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    /// 明文，或 `${ENV}`。见 §3.2 —— 不做 keychain。
    pub key: String,
    /// 不写就从 base_url 猜（§3.3 的最小配置）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
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

    /// 展开 `${ENV}` 之后的 key。
    pub fn resolved_key(&self) -> Result<String, tw_secret::SecretError> {
        tw_secret::expand_from_env(&self.key)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("读 {path} 失败：{source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} 不是合法的 YAML：{source}")]
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
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|source| WriteError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
    }
    let text = serde_yaml_ng::to_string(cfg)?;
    // 先写临时文件再 rename —— 中断的写不该留下半份配置。备份必须原子
    // 发布，配置本身更是（§9.7）。
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, &text).map_err(|source| WriteError::Io {
        path: tmp.clone(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|source| WriteError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

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
            key: "k".into(),
            protocol: Some(Protocol::OpenaiChat),
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
    fn env_interpolation_reaches_the_key() {
        unsafe { std::env::set_var("TW_TEST_KEY", "sk-from-env") };
        let p = Provider {
            name: "x".into(),
            base_url: "https://x".into(),
            key: "${TW_TEST_KEY}".into(),
            protocol: None,
        };
        assert_eq!(p.resolved_key().unwrap(), "sk-from-env");
    }
}
