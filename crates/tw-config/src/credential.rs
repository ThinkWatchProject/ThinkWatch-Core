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
//! 值里可以写 `${ENV}` 从环境变量读；`{{client}}` 换成发起这次请求的那把网关
//! 密钥的名字，给按调用方记账的中转站用。
//!
//! **没有「跑一条命令拿密钥」这一类，而且不会有**：配置文件不该能执行程序 ——
//! 「配置被同步、被分享、被 AI 改」都是目标场景，那时抄一份配置就等于跑一段代码。

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Protocol, Provider};

/// `headers` 里换成 OAuth access token 的占位符。
pub const ACCESS_TOKEN: &str = "{{access_token}}";
/// `headers` 里换成网关密钥名字的占位符。
pub const CLIENT: &str = "{{client}}";

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

    /// 写在配置里的原文。**可能是明文密钥** —— 只给要写回配置的调用方用。
    pub fn raw(&self) -> &str {
        &self.0
    }

    /// 展开 `${ENV}` 之后的值。
    pub fn resolve(&self) -> Result<String, SecretResolveError> {
        Ok(tw_secret::expand_from_env(&self.0)?)
    }

    /// 给界面的形态，**永远不含真实密钥**：带 `${NAME}` 的原样给（写的是
    /// 从哪个环境变量读），其余打码。怎么称呼它由界面决定。
    pub fn shown(&self) -> String {
        if self.0.contains("${") {
            self.0.clone()
        } else {
            tw_secret::mask_secret(&self.0)
        }
    }

    /// 命令行和诊断包里的说法，**永远不含真实密钥**。
    pub fn describe(&self) -> String {
        if self.0.contains("${") {
            format!("环境变量 {}", self.0)
        } else {
            tw_secret::mask_secret(&self.0)
        }
    }

    /// 整个值恰好是一个 `${VAR}` 时，那个变量名。
    pub fn env_var(&self) -> Option<&str> {
        let inner = self.0.trim().strip_prefix("${")?.strip_suffix('}')?;
        let ok = !inner.is_empty()
            && !inner.starts_with(|c: char| c.is_ascii_digit())
            && inner.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        ok.then_some(inner)
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
            // **说清楚该怎么写。**serde 默认只会说「expected a string」，而写错成
            // `key: { oauth: ... }` 的人要知道的是 OAuth 放在哪儿
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("字符串（可以包含 ${VAR}）。OAuth 凭据应写在上游的 oauth 字段中")
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

#[derive(Debug, thiserror::Error)]
pub enum SecretResolveError {
    #[error(transparent)]
    Env(#[from] tw_secret::SecretError),
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
                f.write_str("由「请求头名: 值」组成的映射")
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
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialError {
    #[error("API 密钥为空")]
    EmptyKey,
    #[error("key 和 oauth 只能填写其中一项")]
    KeyAndOauth,
    #[error("oauth 的 refresh 和 endpoint 都不能为空")]
    EmptyOauth,
    #[error("不支持接入 Claude 订阅账号的登录凭据，请改用 Anthropic API 密钥")]
    ClaudeSubscription,
    #[error("不支持接入 Gemini CLI 的 Google 登录凭据，请改用 Gemini API 密钥")]
    GoogleSubscription,
    #[error("ChatGPT 账号上游只能使用登录获得的凭据")]
    ChatgptWithoutLogin,
    #[error("请求头「{0}」说明请求的来源，由网关如实发送，不能在配置中设置")]
    IdentityHeader(String),
    #[error("请求头不能超过 {MAX_HEADERS} 个")]
    TooManyHeaders,
    #[error(
        "请求头名「{0}」不合法：只能由字母、数字和 - _ . ~ 组成，长度不超过 {MAX_HEADER_NAME} 个字符"
    )]
    BadHeaderName(String),
    #[error("请求头「{0}」由网关管理，不能在配置中设置")]
    ReservedHeader(String),
    #[error("请求头「{0}」重复（请求头名不区分大小写）")]
    DuplicateHeader(String),
    #[error("请求头「{0}」的值不能包含换行，长度不超过 {MAX_HEADER_VALUE} 个字符")]
    BadHeaderValue(String),
    #[error(
        "请求头「{name}」中的 {placeholder} 无法识别，仅支持 {{{{access_token}}}} 和 {{{{client}}}}"
    )]
    UnknownPlaceholder { name: String, placeholder: String },
    #[error("请求头「{0}」使用了 {{{{access_token}}}}，但该上游未配置 oauth")]
    TokenWithoutOauth(String),
    #[error("已填写 key，API 密钥将通过请求头「{0}」发送，请求头中不能再设置「{0}」")]
    KeyAndAuthHeader(String),
    #[error(
        "配置 oauth 后，token 默认通过请求头「{0}」发送。如需自行设置该请求头，请在值中用 {{{{access_token}}}} 标明 token 的位置"
    )]
    OauthAndAuthHeader(String),
    #[error("无法获取 OAuth access token")]
    NoToken,
    /// 环境变量没设之类。存成文字：这个错误要能比较，而底下那个类型不能
    #[error("{0}")]
    Env(String),
}

impl From<SecretResolveError> for CredentialError {
    fn from(e: SecretResolveError) -> Self {
        CredentialError::Env(e.to_string())
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
                if p != ACCESS_TOKEN && p != CLIENT {
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
    /// `client`：发起请求的那把网关密钥的名字，没有就换成空串。
    pub fn outbound_headers(
        &self,
        access_token: Option<&str>,
        client: Option<&str>,
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
            if v.contains(CLIENT) {
                v = v.replace(CLIENT, client.unwrap_or_default());
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
            anthropic.outbound_headers(None, None).unwrap(),
            vec![("x-api-key".to_string(), "sk-a".to_string())]
        );
        let openai = p("name: o\nbase_url: https://api.openai.com\nkey: sk-o\n");
        assert_eq!(
            openai.outbound_headers(None, None).unwrap(),
            vec![("authorization".to_string(), "Bearer sk-o".to_string())]
        );
        let gemini = p("name: g\nbase_url: https://generativelanguage.googleapis.com\nkey: g-k\n");
        assert_eq!(
            gemini.outbound_headers(None, None).unwrap(),
            vec![("x-goog-api-key".to_string(), "g-k".to_string())]
        );
    }

    #[test]
    fn headers_keep_their_order_and_come_after_the_key() {
        let x = p(
            "name: r\nbase_url: https://relay.example\nkey: sk-r\nheaders:\n  anthropic-version: 2023-06-01\n  X-Tenant: team-{{client}}\n",
        );
        assert_eq!(
            x.outbound_headers(None, Some("laptop")).unwrap(),
            vec![
                ("x-api-key".to_string(), "sk-r".to_string()),
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
                ("X-Tenant".to_string(), "team-laptop".to_string()),
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
            x.outbound_headers(None, None).unwrap(),
            vec![("X-Relay-Token".to_string(), "t-1".to_string())]
        );
    }

    #[test]
    fn an_oauth_token_goes_into_the_auth_header_unless_a_header_says_where() {
        let mut x = p("name: r\nbase_url: https://relay.example\nprotocol: openai-chat\n");
        x.oauth = Some(oauth());
        x.check_credential().unwrap();
        assert_eq!(
            x.outbound_headers(Some("at-1"), None).unwrap(),
            vec![("authorization".to_string(), "Bearer at-1".to_string())]
        );
        x.headers = Headers::new(vec![Header {
            name: "X-Token".into(),
            value: Secret::new("Token {{access_token}}"),
        }]);
        x.check_credential().unwrap();
        assert_eq!(
            x.outbound_headers(Some("at-2"), None).unwrap(),
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
            login.outbound_headers(Some("at"), None).unwrap(),
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
    fn a_key_written_as_a_mapping_says_where_oauth_goes() {
        let e = serde_yaml_ng::from_str::<Provider>(
            "name: a\nbase_url: https://x\nkey:\n  oauth:\n    refresh: r\n",
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("oauth"), "{e}");
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
            x.outbound_headers(None, None).unwrap(),
            vec![("Authorization".to_string(), "Token from-env".to_string())]
        );
    }

    #[test]
    fn the_env_var_name_is_recognised_only_when_it_is_the_whole_value() {
        assert_eq!(Secret::new("${RELAY_KEY}").env_var(), Some("RELAY_KEY"));
        assert_eq!(Secret::new("Bearer ${RELAY_KEY}").env_var(), None);
        assert_eq!(Secret::new("sk-plain").env_var(), None);
    }
}
