//! 上游凭据：密钥、请求头、OAuth。
//!
//! # 三样东西，各管一件事
//!
//! - **`key`**：一把 API 密钥。放进哪个请求头由接口协议决定 —— Anthropic 是
//!   `x-api-key`，OpenAI 是 `Authorization: Bearer`，Gemini 是 `x-goog-api-key`。
//!   绝大多数上游只要这一行，写配置的人不该为此去查每家把密钥放在哪儿。
//! - **`headers`**：其余要发的请求头，或者完全自己决定凭据怎么发 —— 中转站自己的
//!   鉴权头、`anthropic-version`、组织 ID。以前只能填一把密钥，一个要求
//!   `X-Relay-Token` 的中转站就接不进来。
//! - **`oauth`**：access token 由 refresh token 换发。token 默认放进协议的鉴权头，
//!   也可以在 `headers` 里用 `{{access_token}}` 指定放在哪儿、怎么拼。
//!
//! 值里可以写 `${ENV}` 从环境变量读。
//!
//! **没有「跑一条命令拿密钥」这一类，而且不会有**：配置文件不该能执行程序 ——
//! 「配置被同步、被分享、被 AI 改」都是目标场景，那时抄一份配置就等于跑一段代码。

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Protocol, Provider};
use tw_types::{Msg, msg};

/// `headers` 里换成 OAuth access token 的占位符。
pub const ACCESS_TOKEN: &str = "{{access_token}}";

/// 一个请求头最多几行、名字和值最长多少。**上限是给写错兜底的** —— 一个把整段
/// PEM 贴进请求头的配置，该在加载时被拦下，而不是在上游那边变成一个 431。
pub const MAX_HEADERS: usize = 32;
pub const MAX_HEADER_NAME: usize = 128;
pub const MAX_HEADER_VALUE: usize = 4096;

/// 这些头由 HTTP 本身或网关管，**配置里写了只会把转发弄坏**：`host` 写错整个请求
/// 发错地方，`content-length` 写错体被截断，`connection` 一类是逐跳的，
/// `proxy-authorization` 属于代理那一段。
const RESERVED: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "upgrade",
    "proxy-connection",
    "proxy-authorization",
];

/// 可以带 `${ENV}` 的字符串：密钥、代理密码、请求头的值。
///
/// 明文写在配置里（不做 keychain）。配置文件里看不出 `${ENV}` 和明文的区别，
/// 也不该看出 —— 两种写法对读的人是一回事。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// 写在配置里的原文。**可能是明文密钥** —— 给要写回配置的调用方，和编辑
    /// 对话框要回填的上游视图。
    pub fn raw(&self) -> &str {
        &self.0
    }

    /// 展开 `${ENV}` 之后的值。
    pub fn resolve(&self) -> Result<String, SecretResolveError> {
        Ok(tw_secret::expand_from_env(&self.0)?)
    }

    /// 命令行和诊断包里的说法，**永远不含真实密钥**。
    pub fn describe(&self) -> String {
        if self.0.contains("${") {
            format!("environment variable {}", self.0)
        } else {
            tw_secret::mask_secret(&self.0)
        }
    }

    pub fn is_blank(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl Serialize for Secret {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = Secret;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a string, which may contain ${VAR}")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Secret, E> {
                Ok(Secret(v.to_string()))
            }
            // 纯数字的密钥（有些自建服务就是一串数字）不该因为 YAML 把它读成整数而失败
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Secret, E> {
                Ok(Secret(v.to_string()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Secret, E> {
                Ok(Secret(v.to_string()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Secret, E> {
                Ok(Secret(v.to_string()))
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Secret, E> {
                Ok(Secret(v.to_string()))
            }
        }
        d.deserialize_any(V)
    }
}

/// `${ENV}` 展开不了。**英文只写一遍**：`Display` 就是 [`SecretResolveError::msg`] 的原句。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretResolveError {
    #[error("{}", self.msg())]
    MissingEnv(String),
    #[error("{}", self.msg())]
    Unterminated { pos: usize },
    #[error("{}", self.msg())]
    EmptyName,
}

impl From<tw_secret::SecretError> for SecretResolveError {
    fn from(e: tw_secret::SecretError) -> Self {
        use tw_secret::SecretError;
        match e {
            SecretError::MissingEnv(v) => Self::MissingEnv(v),
            SecretError::Unterminated { pos } => Self::Unterminated { pos },
            SecretError::EmptyName => Self::EmptyName,
        }
    }
}

impl SecretResolveError {
    /// 给人看的那句话，带码。
    pub fn msg(&self) -> Msg {
        match self {
            Self::MissingEnv(var) => msg!(
                "config.secret.env_missing", var = var =>
                "the environment variable {var} is not set"
            ),
            Self::Unterminated { pos } => msg!(
                "config.secret.unterminated", pos = pos =>
                "the ${{...}} at character {pos} is not closed"
            ),
            Self::EmptyName => msg!(
                "config.secret.empty_name" => "the variable name is empty: ${{}}"
            ),
        }
    }
}

/// 一行请求头。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub name: String,
    pub value: Secret,
}

/// 要发给上游的请求头，**按写的顺序**。
///
/// 配置里是一个映射（`名字: 值`），读起来和 HTTP 报文一个样子。不用 `HashMap`
/// 是因为顺序要保住 —— 往返一次把用户写的顺序打乱，是一次他没要求的改动。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers(Vec<Header>);

impl Headers {
    pub fn new(headers: Vec<Header>) -> Self {
        Self(headers)
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Header> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// 按名字找，**不分大小写** —— HTTP 头名本来就不分。
    pub fn get(&self, name: &str) -> Option<&Header> {
        self.0.iter().find(|h| h.name.eq_ignore_ascii_case(name))
    }

    /// 有没有哪个值用到了这个占位符。
    pub fn uses(&self, placeholder: &str) -> bool {
        self.0.iter().any(|h| h.value.raw().contains(placeholder))
    }
}

impl Serialize for Headers {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for h in &self.0 {
            m.serialize_entry(&h.name, &h.value)?;
        }
        m.end()
    }
}

impl<'de> Deserialize<'de> for Headers {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Headers;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a mapping of header name to value")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut m: A,
            ) -> Result<Headers, A::Error> {
                let mut out: Vec<Header> = Vec::new();
                while let Some((name, value)) = m.next_entry::<String, Secret>()? {
                    out.push(Header { name, value });
                }
                Ok(Headers(out))
            }
        }
        d.deserialize_map(V)
    }
}

/// 协议的鉴权头：名字，和值前面要拼的东西。
///
/// **猜不出协议时按 Anthropic 放**：桌面版的主用例是 Claude Code，而中转站绝大多数
/// 说的是 Anthropic 方言。模型目录那边按同一个默认算协议，两处不一致会让
/// 「列出来了但发过去 401」变成可能。
pub fn auth_header(protocol: Option<Protocol>) -> (&'static str, &'static str) {
    match protocol {
        Some(Protocol::Gemini) => ("x-goog-api-key", ""),
        Some(Protocol::OpenaiChat) | Some(Protocol::OpenaiResponses) | Some(Protocol::Chatgpt) => {
            ("authorization", "Bearer ")
        }
        Some(Protocol::Anthropic) | None => ("x-api-key", ""),
    }
}

/// 凭据写法上的问题。**每一条都指到具体的那一行**，而不是「凭据无效」。
///
/// **英文只写一遍**：`Display` 就是 [`CredentialError::msg`] 的原句，界面拿码去翻。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialError {
    #[error("{}", self.msg())]
    EmptyKey,
    #[error("{}", self.msg())]
    KeyAndOauth,
    #[error("{}", self.msg())]
    EmptyOauth,
    #[error("{}", self.msg())]
    ClaudeSubscription,
    #[error("{}", self.msg())]
    GoogleSubscription,
    #[error("{}", self.msg())]
    ChatgptWithoutLogin,
    #[error("{}", self.msg())]
    IdentityHeader(String),
    #[error("{}", self.msg())]
    TooManyHeaders,
    #[error("{}", self.msg())]
    BadHeaderName(String),
    #[error("{}", self.msg())]
    ReservedHeader(String),
    #[error("{}", self.msg())]
    DuplicateHeader(String),
    #[error("{}", self.msg())]
    BadHeaderValue(String),
    #[error("{}", self.msg())]
    UnknownPlaceholder { name: String, placeholder: String },
    #[error("{}", self.msg())]
    TokenWithoutOauth(String),
    #[error("{}", self.msg())]
    KeyAndAuthHeader(String),
    #[error("{}", self.msg())]
    OauthAndAuthHeader(String),
    #[error("{}", self.msg())]
    NoToken,
    /// 环境变量没设之类
    #[error("{}", self.msg())]
    Env(SecretResolveError),
}

impl CredentialError {
    /// 给人看的那句话，带码。
    pub fn msg(&self) -> Msg {
        use CredentialError::*;
        match self {
            EmptyKey => msg!("config.credential.empty_key" => "the API key is empty"),
            KeyAndOauth => msg!(
                "config.credential.key_and_oauth" =>
                "key and oauth are alternatives; fill in one of them"
            ),
            EmptyOauth => msg!(
                "config.credential.empty_oauth" =>
                "neither refresh nor endpoint of oauth can be empty"
            ),
            ClaudeSubscription => msg!(
                "config.credential.claude_subscription" =>
                "a Claude subscription sign-in is not supported here; use an Anthropic API key"
            ),
            GoogleSubscription => msg!(
                "config.credential.google_subscription" =>
                "the Google sign-in of Gemini CLI is not supported here; use a Gemini API key"
            ),
            ChatgptWithoutLogin => msg!(
                "config.credential.chatgpt_without_login" =>
                "a ChatGPT account upstream takes only the credential obtained by signing in"
            ),
            IdentityHeader(h) => msg!(
                "config.credential.identity_header", header = h =>
                "the `{header}` header says where a request came from; the gateway sends it \
                 truthfully and it cannot be set in the configuration"
            ),
            TooManyHeaders => msg!(
                "config.credential.too_many_headers", max = MAX_HEADERS =>
                "there can be at most {max} headers"
            ),
            BadHeaderName(h) => msg!(
                "config.credential.bad_header_name", header = h, max = MAX_HEADER_NAME =>
                "the header name `{header}` is not valid: letters, digits and - _ . ~ only, and at \
                 most {max} characters"
            ),
            ReservedHeader(h) => msg!(
                "config.credential.reserved_header", header = h =>
                "the `{header}` header is the gateway's to manage and cannot be set in the \
                 configuration"
            ),
            DuplicateHeader(h) => msg!(
                "config.credential.duplicate_header", header = h =>
                "the `{header}` header appears twice (header names are case-insensitive)"
            ),
            BadHeaderValue(h) => msg!(
                "config.credential.bad_header_value", header = h, max = MAX_HEADER_VALUE =>
                "the value of the `{header}` header cannot contain a newline and is at most {max} \
                 characters"
            ),
            UnknownPlaceholder { name, placeholder } => msg!(
                "config.credential.unrecognized_placeholder", header = name, placeholder = placeholder =>
                "{placeholder} in the `{header}` header is not recognized; only {{{{access_token}}}} is"
            ),
            TokenWithoutOauth(h) => msg!(
                "config.credential.token_without_oauth", header = h =>
                "the `{header}` header uses {{{{access_token}}}}, and this upstream has no oauth \
                 configured"
            ),
            KeyAndAuthHeader(h) => msg!(
                "config.credential.key_and_auth_header", header = h =>
                "a key is filled in, so the API key goes out in the `{header}` header; `{header}` \
                 cannot also be set among the headers"
            ),
            OauthAndAuthHeader(h) => msg!(
                "config.credential.oauth_and_auth_header", header = h =>
                "with oauth configured, the token goes out in the `{header}` header by default. To \
                 set that header yourself, mark where the token goes with {{{{access_token}}}}"
            ),
            NoToken => msg!(
                "config.credential.no_token" =>
                "an OAuth access token could not be obtained"
            ),
            // 只在转发时出现（展开 `${VAR}`），写法检查碰不到它
            Env(e) => e.msg(),
        }
    }
}

impl From<SecretResolveError> for CredentialError {
    fn from(e: SecretResolveError) -> Self {
        CredentialError::Env(e)
    }
}

pub(crate) fn host_of(url: &str) -> String {
    let h = url.trim().to_ascii_lowercase();
    h.split("://")
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
        .unwrap_or("")
        .to_string()
}

fn is_or_under(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// 值里出现的全部 `{{…}}`。
fn placeholders(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find("{{") {
        match rest[start..].find("}}") {
            Some(len) => {
                out.push(&rest[start..start + len + 2]);
                rest = &rest[start + len + 2..];
            }
            None => break,
        }
    }
    out
}

impl Provider {
    /// 这家的鉴权头：名字和值的前缀。
    pub fn auth_header(&self) -> (&'static str, &'static str) {
        auth_header(self.effective_protocol())
    }

    /// 凭据写法对不对。**不联网，不读环境变量** —— 那是转发时的事。
    pub fn check_credential(&self) -> Result<(), CredentialError> {
        if let Some(k) = &self.key
            && k.is_blank()
        {
            return Err(CredentialError::EmptyKey);
        }
        if let Some(o) = &self.oauth {
            if self.key.is_some() {
                return Err(CredentialError::KeyAndOauth);
            }
            if o.refresh.trim().is_empty() || o.endpoint.trim().is_empty() {
                return Err(CredentialError::EmptyOauth);
            }
            // **订阅账号的令牌不接。**Anthropic 规定 Claude 订阅的 OAuth 令牌不得由
            // 第三方产品收集、保存或转接；Gemini CLI 的条款把借用它的 Google 登录访问
            // 后端列为违规。能接的只是 OAuth2 保护的自建或企业服务
            let base = host_of(&self.base_url);
            let token = host_of(&o.endpoint);
            let claude = |h: &str| {
                ["anthropic.com", "claude.ai", "claude.com"]
                    .iter()
                    .any(|d| is_or_under(h, d))
            };
            if claude(&base) || claude(&token) {
                return Err(CredentialError::ClaudeSubscription);
            }
            if is_or_under(&base, "cloudcode-pa.googleapis.com") {
                return Err(CredentialError::GoogleSubscription);
            }
        }
        // **Codex 后端只认登录拿到的凭据**，API 密钥发过去只会是一个 401
        let chatgpt = self.effective_protocol() == Some(Protocol::Chatgpt);
        if chatgpt && crate::chatgpt::is_backend(&self.base_url) && self.oauth.is_none() {
            return Err(CredentialError::ChatgptWithoutLogin);
        }
        if self.headers.len() > MAX_HEADERS {
            return Err(CredentialError::TooManyHeaders);
        }
        let (auth_name, _) = self.auth_header();
        let mut seen: Vec<String> = Vec::new();
        for h in self.headers.iter() {
            let name = h.name.trim();
            let valid = !name.is_empty()
                && name.len() <= MAX_HEADER_NAME
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.~".contains(&b));
            if !valid {
                return Err(CredentialError::BadHeaderName(h.name.clone()));
            }
            let lower = name.to_ascii_lowercase();
            if RESERVED.contains(&lower.as_str()) {
                return Err(CredentialError::ReservedHeader(h.name.clone()));
            }
            if chatgpt && crate::chatgpt::IDENTITY_HEADERS.contains(&lower.as_str()) {
                return Err(CredentialError::IdentityHeader(h.name.clone()));
            }
            if seen.contains(&lower) {
                return Err(CredentialError::DuplicateHeader(h.name.clone()));
            }
            seen.push(lower.clone());
            let raw = h.value.raw();
            if raw.len() > MAX_HEADER_VALUE || raw.contains(['\r', '\n']) {
                return Err(CredentialError::BadHeaderValue(h.name.clone()));
            }
            for p in placeholders(raw) {
                if p != ACCESS_TOKEN {
                    return Err(CredentialError::UnknownPlaceholder {
                        name: h.name.clone(),
                        placeholder: p.to_string(),
                    });
                }
            }
            if raw.contains(ACCESS_TOKEN) && self.oauth.is_none() {
                return Err(CredentialError::TokenWithoutOauth(h.name.clone()));
            }
            if lower == auth_name {
                if self.key.is_some() {
                    return Err(CredentialError::KeyAndAuthHeader(h.name.clone()));
                }
                if self.oauth.is_some() && !self.headers.uses(ACCESS_TOKEN) {
                    return Err(CredentialError::OauthAndAuthHeader(h.name.clone()));
                }
            }
        }
        Ok(())
    }

    /// 要发给这家的全部请求头，值已经展开。
    ///
    /// `access_token`：配了 oauth 时由调用方换好传进来（换 token 要联网，这里是同步的）。
    pub fn outbound_headers(
        &self,
        access_token: Option<&str>,
    ) -> Result<Vec<(String, String)>, CredentialError> {
        let (auth_name, prefix) = self.auth_header();
        let mut out = Vec::with_capacity(self.headers.len() + 1);
        if let Some(k) = &self.key {
            out.push((auth_name.to_string(), format!("{prefix}{}", k.resolve()?)));
        }
        if self.oauth.is_some() && !self.headers.uses(ACCESS_TOKEN) {
            let t = access_token.ok_or(CredentialError::NoToken)?;
            out.push((auth_name.to_string(), format!("{prefix}{t}")));
        }
        for h in self.headers.iter() {
            let mut v = h.value.resolve()?;
            if v.contains(ACCESS_TOKEN) {
                v = v.replace(ACCESS_TOKEN, access_token.ok_or(CredentialError::NoToken)?);
            }
            out.push((h.name.trim().to_string(), v));
        }
        Ok(out)
    }

    /// 凭据的身份：地址或凭据变了，之前问到的模型清单就不再作数。
    ///
    /// **OAuth 只看账号，不看 token**：refresh token 轮换一次就重问一遍，问到的是同一份清单。
    pub fn credential_identity(&self) -> String {
        let mut s = String::new();
        if let Some(k) = &self.key {
            s.push_str(k.raw());
        }
        if let Some(o) = &self.oauth {
            s.push_str(&format!(
                "oauth|{}|{}",
                o.endpoint,
                o.client_id.as_deref().unwrap_or_default()
            ));
        }
        for h in self.headers.iter() {
            s.push('|');
            s.push_str(&h.name.to_ascii_lowercase());
            s.push('=');
            s.push_str(h.value.raw());
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::OAuth;

    fn p(yaml: &str) -> Provider {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    fn oauth() -> OAuth {
        OAuth {
            access: None,
            expires_at: None,
            refresh: "r".into(),
            endpoint: "https://auth.example.com/token".into(),
            client_id: None,
            client_secret: None,
            refresh_before: None,
        }
    }

    #[test]
    fn a_key_goes_into_the_header_its_protocol_expects() {
        let anthropic = p("name: a\nbase_url: https://api.anthropic.com\nkey: sk-a\n");
        assert_eq!(
            anthropic.outbound_headers(None).unwrap(),
            vec![("x-api-key".to_string(), "sk-a".to_string())]
        );
        let openai = p("name: o\nbase_url: https://api.openai.com\nkey: sk-o\n");
        assert_eq!(
            openai.outbound_headers(None).unwrap(),
            vec![("authorization".to_string(), "Bearer sk-o".to_string())]
        );
        let gemini = p("name: g\nbase_url: https://generativelanguage.googleapis.com\nkey: g-k\n");
        assert_eq!(
            gemini.outbound_headers(None).unwrap(),
            vec![("x-goog-api-key".to_string(), "g-k".to_string())]
        );
    }

    #[test]
    fn headers_keep_their_order_and_come_after_the_key() {
        let x = p(
            "name: r\nbase_url: https://relay.example\nkey: sk-r\nheaders:\n  anthropic-version: 2023-06-01\n  X-Tenant: team-a\n",
        );
        assert_eq!(
            x.outbound_headers(None).unwrap(),
            vec![
                ("x-api-key".to_string(), "sk-r".to_string()),
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
                ("X-Tenant".to_string(), "team-a".to_string()),
            ]
        );
        // 往返一次顺序不变
        let back = serde_yaml_ng::to_string(&x.headers).unwrap();
        assert!(back.find("anthropic-version").unwrap() < back.find("X-Tenant").unwrap());
    }

    #[test]
    fn a_relay_can_take_its_credential_in_any_header() {
        let x = p("name: r\nbase_url: https://relay.example\nheaders:\n  X-Relay-Token: t-1\n");
        x.check_credential().unwrap();
        assert_eq!(
            x.outbound_headers(None).unwrap(),
            vec![("X-Relay-Token".to_string(), "t-1".to_string())]
        );
    }

    #[test]
    fn an_oauth_token_goes_into_the_auth_header_unless_a_header_says_where() {
        let mut x = p("name: r\nbase_url: https://relay.example\nprotocol: openai-chat\n");
        x.oauth = Some(oauth());
        x.check_credential().unwrap();
        assert_eq!(
            x.outbound_headers(Some("at-1")).unwrap(),
            vec![("authorization".to_string(), "Bearer at-1".to_string())]
        );
        x.headers = Headers::new(vec![Header {
            name: "X-Token".into(),
            value: Secret::new("Token {{access_token}}"),
        }]);
        x.check_credential().unwrap();
        assert_eq!(
            x.outbound_headers(Some("at-2")).unwrap(),
            vec![("X-Token".to_string(), "Token at-2".to_string())]
        );
    }

    #[test]
    fn writing_the_auth_header_twice_is_refused() {
        let x = p(
            "name: r\nbase_url: https://api.anthropic.com\nkey: sk-a\nheaders:\n  X-Api-Key: sk-b\n",
        );
        assert_eq!(
            x.check_credential(),
            Err(CredentialError::KeyAndAuthHeader("X-Api-Key".into()))
        );
    }

    #[test]
    fn headers_that_would_break_forwarding_are_refused() {
        for (h, want) in [
            ("Host: x", CredentialError::ReservedHeader("Host".into())),
            (
                "content-length: 3",
                CredentialError::ReservedHeader("content-length".into()),
            ),
            (
                "\"bad name\": x",
                CredentialError::BadHeaderName("bad name".into()),
            ),
        ] {
            let x = p(&format!(
                "name: r\nbase_url: https://relay.example\nheaders:\n  {h}\n"
            ));
            assert_eq!(x.check_credential(), Err(want), "{h}");
        }
        let dup = p("name: r\nbase_url: https://relay.example\nheaders:\n  X-A: 1\n  x-a: 2\n");
        assert_eq!(
            dup.check_credential(),
            Err(CredentialError::DuplicateHeader("x-a".into()))
        );
    }

    #[test]
    fn placeholders_are_checked() {
        let unknown =
            p("name: r\nbase_url: https://relay.example\nheaders:\n  X-A: \"{{user}}\"\n");
        assert!(matches!(
            unknown.check_credential(),
            Err(CredentialError::UnknownPlaceholder { .. })
        ));
        // 按调用方填网关密钥名的 {{client}} 已经不支持了
        let client =
            p("name: r\nbase_url: https://relay.example\nheaders:\n  X-A: \"{{client}}\"\n");
        assert_eq!(
            client.check_credential(),
            Err(CredentialError::UnknownPlaceholder {
                name: "X-A".into(),
                placeholder: "{{client}}".into(),
            })
        );
        let no_oauth =
            p("name: r\nbase_url: https://relay.example\nheaders:\n  X-A: \"{{access_token}}\"\n");
        assert_eq!(
            no_oauth.check_credential(),
            Err(CredentialError::TokenWithoutOauth("X-A".into()))
        );
    }

    #[test]
    fn subscription_logins_cannot_be_configured_as_oauth() {
        let mut claude = p("name: c\nbase_url: https://api.anthropic.com\n");
        claude.oauth = Some(oauth());
        assert_eq!(
            claude.check_credential(),
            Err(CredentialError::ClaudeSubscription)
        );

        // 地址是中转站，但 token 从 Claude 的授权端点换来 —— 一样不行
        let mut via_relay = p("name: r\nbase_url: https://relay.example\n");
        via_relay.oauth = Some(OAuth {
            endpoint: "https://console.anthropic.com/v1/oauth/token".into(),
            ..oauth()
        });
        assert_eq!(
            via_relay.check_credential(),
            Err(CredentialError::ClaudeSubscription)
        );

        let mut gemini_cli = p("name: g\nbase_url: https://cloudcode-pa.googleapis.com\n");
        gemini_cli.oauth = Some(oauth());
        assert_eq!(
            gemini_cli.check_credential(),
            Err(CredentialError::GoogleSubscription)
        );

        // `evil.com/api.anthropic.com` 不是 Anthropic，也不该被当成它拦下或放过
        let mut lookalike = p("name: l\nbase_url: https://notanthropic.com/api.anthropic.com\n");
        lookalike.oauth = Some(oauth());
        lookalike.check_credential().unwrap();
    }

    #[test]
    fn a_chatgpt_account_takes_only_a_login_and_its_identity_is_not_configurable() {
        let bare = p("name: c\nbase_url: https://chatgpt.com/backend-api/codex\n");
        assert_eq!(bare.effective_protocol(), Some(Protocol::Chatgpt));
        assert_eq!(
            bare.check_credential(),
            Err(CredentialError::ChatgptWithoutLogin)
        );
        let key = p("name: c\nbase_url: https://chatgpt.com/backend-api/codex\nkey: sk-x\n");
        assert_eq!(
            key.check_credential(),
            Err(CredentialError::ChatgptWithoutLogin)
        );

        let mut login = p(
            "name: c\nbase_url: https://chatgpt.com/backend-api/codex\nheaders:\n  ChatGPT-Account-Id: acc-1\n",
        );
        login.oauth = Some(OAuth {
            endpoint: crate::chatgpt::TOKEN_ENDPOINT.into(),
            client_id: Some(crate::chatgpt::CLIENT_ID.into()),
            ..oauth()
        });
        login.check_credential().unwrap();
        assert_eq!(
            login.outbound_headers(Some("at")).unwrap(),
            vec![
                ("authorization".to_string(), "Bearer at".to_string()),
                ("ChatGPT-Account-Id".to_string(), "acc-1".to_string()),
            ]
        );

        // 一行配置就能冒充 Codex 的话，「如实说明身份」就只是个默认值
        for name in [
            "originator",
            "User-Agent",
            "session-id",
            "session_id",
            "version",
        ] {
            let mut forged = login.clone();
            forged.headers = p(&format!(
                "name: c\nbase_url: https://x\nheaders:\n  {name}: codex_cli_rs\n"
            ))
            .headers;
            assert_eq!(
                forged.check_credential(),
                Err(CredentialError::IdentityHeader(name.to_string())),
                "{name}"
            );
        }
    }

    #[test]
    fn a_numeric_key_is_still_a_key() {
        let x = p("name: a\nbase_url: https://x\nkey: 12345678\n");
        assert_eq!(x.key.unwrap().raw(), "12345678");
    }

    #[test]
    fn env_vars_expand_inside_header_values() {
        // SAFETY: 测试里设一个只有这个测试读的变量
        unsafe { std::env::set_var("TW_TEST_RELAY_TOKEN", "from-env") };
        let x = p(
            "name: r\nbase_url: https://relay.example\nheaders:\n  Authorization: Token ${TW_TEST_RELAY_TOKEN}\n",
        );
        assert_eq!(
            x.outbound_headers(None).unwrap(),
            vec![("Authorization".to_string(), "Token from-env".to_string())]
        );
    }
}
