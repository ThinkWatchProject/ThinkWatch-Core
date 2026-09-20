//! ChatGPT 账号上游：Codex 后端的接法，和登录要用的 OpenAI OAuth 细节。
//!
//! # 如实说明身份
//!
//! 发给 Codex 后端的 `originator` 和 User-Agent 是 ThinkWatch 自己的，系统提示词是客户端
//! 自己的，不冒充 Codex，也不做指纹。唯一借用的是 OAuth 客户端 ID（见
//! [`tw_config::chatgpt::CLIENT_ID`]）。2026-09-18 用 Plus 账号实测过，后端照常服务。
//!
//! # 和 OpenAI Responses 不一样的地方
//!
//! 协议的出处是 OpenAI 开源的 Codex（Apache-2.0），下面几条都实测核对过：
//!
//! - **只接受流式**：`stream: false` 回 400。客户端要整包时，由网关收齐流再回
//! - **不认 `max_output_tokens`**：回 400。发出去之前删掉，记进丢弃的字段
//! - **流式响应没有 Content-Type**：按协议认定是 SSE，否则转换、回显还原和工具调用
//!   审查都会走整包那条路
//! - 模型清单在 `/models`，必须带 `client_version`，否则回 400
//! - 额度在响应头 `x-codex-{primary,secondary}-*` 里，窗口多长看 `-window-minutes`

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tw_dialect::ir::Dialect;

pub use tw_config::chatgpt::{ACCOUNT_HEADER, BASE_URL, CLIENT_ID, TOKEN_ENDPOINT};

/// OpenAI 的登录页和 token 端点所在
pub const ISSUER: &str = "https://auth.openai.com";
/// 登出时吊销 refresh token
pub const REVOKE_ENDPOINT: &str = "https://auth.openai.com/oauth/revoke";
/// 请求来自谁。**如实写 ThinkWatch**
pub const ORIGINATOR: &str = "thinkwatch";
/// 模型清单要的 `client_version`。
///
/// 后端不带这个参数就回 400。ThinkWatch 不实现任何 Codex 客户端版本的特性，所以如实报
/// `0.0.0`；实测它照样返回全部模型。
pub const CLIENT_VERSION: &str = "0.0.0";
/// 授权完成后浏览器跳回本机的端口。**只有这两个能用**：它们是登记在 Codex 客户端上的回调
/// 地址，换别的端口，登录页会拒绝跳转
pub const CALLBACK_PORTS: [u16; 2] = [1455, 1457];
/// 要的权限。`offline_access` 才会给 refresh token
pub const SCOPE: &str = "openid profile email offline_access";

const RESIDENCY_HEADER: &str = "x-openai-internal-codex-residency";

/// 发给 Codex 后端的 User-Agent：`thinkwatch/0.5.0 (macos; aarch64)`
pub fn user_agent() -> String {
    format!(
        "{ORIGINATOR}/{} ({}; {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

// ---------------------------------------------------------------- 请求

/// 客户端本来就说 Responses、这一跳直通时，把请求体改成 Codex 后端接受的样子。
///
/// 返回改好的请求体和删掉的字段。**只动这三个字段**，其余原样留着：Codex CLI 这类客户端
/// 带着自定义工具和加密的推理条目，经过中间表示转一遍会丢东西。请求体不是 JSON 对象时
/// 原样返回，由后端说它哪里不对。
pub fn shape_passthrough(body: &Bytes) -> (Bytes, Vec<String>) {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return (body.clone(), Vec::new());
    };
    let Some(obj) = v.as_object_mut() else {
        return (body.clone(), Vec::new());
    };
    let mut dropped = Vec::new();
    if obj.remove("max_output_tokens").is_some() {
        dropped.push("max_output_tokens".to_string());
    }
    obj.insert("stream".into(), Value::Bool(true));
    obj.insert("store".into(), Value::Bool(false));
    match serde_json::to_vec(&v) {
        Ok(b) => (Bytes::from(b), dropped),
        Err(_) => (body.clone(), Vec::new()),
    }
}

/// 转换过来的请求：先从中间表示里去掉输出上限。返回客户端那边这个字段叫什么，没设就是
/// `None`。**在编码之前做**，转换器报的丢弃字段和这里说的是同一种写法。
pub fn drop_output_limit(request: &mut tw_dialect::ir::Request, client: Dialect) -> Option<String> {
    request.max_tokens.take()?;
    Some(
        match client {
            Dialect::Anthropic | Dialect::Chat => "max_tokens",
            Dialect::Responses => "max_output_tokens",
            Dialect::Gemini => "generationConfig.maxOutputTokens",
        }
        .to_string(),
    )
}

/// 编码好的请求体改成流式。**客户端要不要流，会话里记着**；这里改的只是发给后端的那一份
pub fn force_stream(body: Vec<u8>) -> Vec<u8> {
    let Ok(mut v) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    let Some(obj) = v.as_object_mut() else {
        return body;
    };
    obj.insert("stream".into(), Value::Bool(true));
    serde_json::to_vec(&v).unwrap_or(body)
}

/// 发给 Codex 后端的身份头，加在配置的请求头之后。
///
/// - `session-id`：客户端带着会话 ID 就沿用（Codex CLI 会带），同一段对话落在后端同一处，
///   prompt cache 才命中得上；没有就每个请求新起一个
/// - 数据驻留：从 access token 里读，账号要求时才带
pub fn identity_headers(
    client: &axum::http::HeaderMap,
    upstream: &[(String, String)],
) -> Vec<(String, String)> {
    let session = ["session-id", "session_id"]
        .iter()
        .find_map(|k| client.get(*k))
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mut out = vec![
        ("originator".to_string(), ORIGINATOR.to_string()),
        ("user-agent".to_string(), user_agent()),
        ("session-id".to_string(), session),
        ("accept".to_string(), "text/event-stream".to_string()),
    ];
    let residency = upstream
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .and_then(|(_, v)| v.strip_prefix("Bearer "))
        .and_then(|t| account(t).residency);
    if let Some(r) = residency {
        out.push((RESIDENCY_HEADER.to_string(), r));
    }
    out
}

/// 客户端带来的这个请求头要不要发给 Codex 后端。说明来源的头由网关自己填，`accept` 也是
pub fn keeps_client_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower != "accept" && !tw_config::chatgpt::IDENTITY_HEADERS.contains(&lower.as_str())
}

// ---------------------------------------------------------------- 账户

/// 令牌里和账户有关的几项。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Account {
    pub account_id: Option<String>,
    /// `plus` / `pro` / `team` …
    pub plan: Option<String>,
    /// 数据驻留。`no_constraint` 当成没有
    pub residency: Option<String>,
}

/// 从 id_token 或 access token 里读账户信息。
///
/// **不验签。**令牌是我们自己经 TLS 从 OpenAI 换来的，读它只是为了拿要随请求带上的值，
/// 不拿它做任何授权判断。
pub fn account(jwt: &str) -> Account {
    let claims = jwt
        .split('.')
        .nth(1)
        .and_then(|p| URL_SAFE_NO_PAD.decode(p.trim_end_matches('=')).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or(Value::Null);
    let auth = &claims["https://api.openai.com/auth"];
    let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_string);
    Account {
        account_id: text(&auth["chatgpt_account_id"])
            .or_else(|| text(&claims["chatgpt_account_id"])),
        plan: text(&auth["chatgpt_plan_type"]),
        residency: text(&auth["chatgpt_compute_residency"])
            .or_else(|| text(&claims["chatgpt_compute_residency"]))
            .filter(|r| r != "no_constraint"),
    }
}

// ---------------------------------------------------------------- 模型与额度

/// 模型清单的地址
pub fn models_url(base_url: &str) -> String {
    crate::forward::upstream_url(
        base_url,
        "/models",
        Some(&format!("client_version={CLIENT_VERSION}")),
    )
}

/// 从模型清单里挑出能用的模型。
///
/// `visibility` 是 `hide` 的不列（后端内部用的，比如代码审查专用模型），`supported_in_api`
/// 明确为 false 的也不列。认不出形状是 `None`。
pub fn parse_models(v: &Value) -> Option<Vec<String>> {
    let list = v.get("models")?.as_array()?;
    Some(
        list.iter()
            .filter(|m| m.get("visibility").and_then(|x| x.as_str()) != Some("hide"))
            .filter(|m| m.get("supported_in_api").and_then(|x| x.as_bool()) != Some(false))
            .filter_map(|m| m.get("slug").and_then(|x| x.as_str()))
            .map(str::to_string)
            .collect(),
    )
}

/// ChatGPT 网页后端的接口地址（额度、重置卡）。它们和 Codex 后端同一台，路径在 `/wham` 下
pub fn wham_url(base_url: &str, path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let root = base.strip_suffix("/codex").unwrap_or(base);
    format!("{root}/wham/{}", path.trim_start_matches('/'))
}

// ---------------------------------------------------------------- 登录

/// PKCE 的一对值。`verifier` 留在本机，`challenge` 放进授权地址
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn new() -> Self {
        let mut bytes = [0u8; 64];
        rand::fill(&mut bytes);
        let verifier = URL_SAFE_NO_PAD.encode(bytes);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self {
            verifier,
            challenge,
        }
    }
}

impl Default for Pkce {
    fn default() -> Self {
        Self::new()
    }
}

/// 防跨站请求伪造的 `state`：回调带回来的必须和发出去的一样
pub fn new_state() -> String {
    let mut bytes = [0u8; 24];
    rand::fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// 授权完成后浏览器跳回来的地址
pub fn redirect_uri(port: u16) -> String {
    format!("http://localhost:{port}/auth/callback")
}

/// 登录页的地址。`issuer` 平时是 [`ISSUER`]，测试里换成假服务器
pub fn authorize_url(issuer: &str, redirect_uri: &str, pkce: &Pkce, state: &str) -> String {
    let mut url = reqwest::Url::parse(&format!("{}/oauth/authorize", issuer.trim_end_matches('/')))
        .unwrap_or_else(|_| {
            reqwest::Url::parse(&format!("{ISSUER}/oauth/authorize")).expect("built from constants")
        });
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("state", state)
        .append_pair("originator", ORIGINATOR);
    url.to_string()
}

/// 设备码：在这台机器上换一个一次性码。**不是所有账号都开放**，没开放时这个接口回 404
pub fn device_code_url(issuer: &str) -> String {
    format!(
        "{}/api/accounts/deviceauth/usercode",
        issuer.trim_end_matches('/')
    )
}

/// 设备码：问一次「批准了没有」
pub fn device_token_url(issuer: &str) -> String {
    format!(
        "{}/api/accounts/deviceauth/token",
        issuer.trim_end_matches('/')
    )
}

/// 设备码：让用户在另一台设备上打开、输码的那一页
pub fn device_page_url(issuer: &str) -> String {
    format!("{}/codex/device", issuer.trim_end_matches('/'))
}

/// 设备码换令牌时要报的回调地址。**这一步没有浏览器跳转**，但它必须和授权时的一致
pub fn device_redirect_uri(issuer: &str) -> String {
    format!("{}/deviceauth/callback", issuer.trim_end_matches('/'))
}

/// 登录换回来的令牌。refresh token、access token 和过期时间写进配置，id_token 只用来读账户信息
pub struct Tokens {
    pub id_token: String,
    pub access: String,
    pub refresh: String,
    pub expires_in: Option<u64>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 令牌不进日志，哪怕是调试输出
        f.debug_struct("Tokens")
            .field("expires_in", &self.expires_in)
            .finish_non_exhaustive()
    }
}

/// 用授权码换令牌。
pub async fn exchange_code(
    http: &reqwest::Client,
    token_endpoint: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<Tokens, String> {
    let resp = http
        .post(token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|e| {
            format!(
                "{} could not be reached: {e}",
                tw_secret::redact_url(token_endpoint)
            )
        })?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if status >= 400 {
        let body = tw_secret::mask_body(
            &text
                .replace(code, "<omitted>")
                .replace(verifier, "<omitted>"),
        );
        return Err(format!(
            "the token endpoint answered {status}: {}",
            body.chars().take(400).collect::<String>()
        ));
    }
    let v: Value = serde_json::from_str(&text)
        .map_err(|_| "the token endpoint's response is not JSON".to_string())?;
    let field = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Ok(Tokens {
        id_token: field("id_token").ok_or("the token endpoint's response has no id_token")?,
        access: field("access_token").ok_or("the token endpoint's response has no access_token")?,
        refresh: field("refresh_token")
            .ok_or("the token endpoint's response has no refresh_token")?,
        expires_in: v.get("expires_in").and_then(|x| x.as_u64()),
    })
}

/// 吊销 refresh token。**尽力而为**：失败也不挡住删除上游
pub async fn revoke(
    http: &reqwest::Client,
    revoke_endpoint: &str,
    refresh: &str,
) -> Result<(), String> {
    let resp = http
        .post(revoke_endpoint)
        .json(&serde_json::json!({
            "token": refresh,
            "token_type_hint": "refresh_token",
            "client_id": CLIENT_ID,
        }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| {
            format!(
                "{} could not be reached: {e}",
                tw_secret::redact_url(revoke_endpoint)
            )
        })?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!(
            "the revoke endpoint answered {}",
            resp.status().as_u16()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(claims: Value) -> String {
        format!(
            "eyJhbGciOiJub25lIn0.{}.sig",
            URL_SAFE_NO_PAD.encode(claims.to_string())
        )
    }

    #[test]
    fn a_passthrough_request_keeps_everything_but_the_three_fields() {
        let body = Bytes::from(
            serde_json::json!({
                "model": "gpt-5.5",
                "input": [{"type": "reasoning", "encrypted_content": "enc"}],
                "tools": [{"type": "custom", "name": "apply_patch"}],
                "max_output_tokens": 256,
                "stream": false,
                "store": true,
                "prompt_cache_key": "conv-1"
            })
            .to_string(),
        );
        let (out, dropped) = shape_passthrough(&body);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(dropped, vec!["max_output_tokens"]);
        assert_eq!(v["stream"], true);
        assert_eq!(v["store"], false);
        assert!(v.get("max_output_tokens").is_none());
        assert_eq!(v["input"][0]["encrypted_content"], "enc");
        assert_eq!(v["tools"][0]["type"], "custom");
        assert_eq!(v["prompt_cache_key"], "conv-1");

        // 不是 JSON 就原样交给后端，由它说哪里不对
        let raw = Bytes::from_static(b"not json");
        assert_eq!(shape_passthrough(&raw), (raw.clone(), Vec::new()));
    }

    #[test]
    fn the_dropped_limit_is_named_the_way_the_client_wrote_it() {
        let mut r = tw_dialect::ir::Request {
            max_tokens: Some(1000),
            ..Default::default()
        };
        assert_eq!(
            drop_output_limit(&mut r, Dialect::Gemini).as_deref(),
            Some("generationConfig.maxOutputTokens")
        );
        assert_eq!(r.max_tokens, None);
        assert_eq!(drop_output_limit(&mut r, Dialect::Anthropic), None);
    }

    #[test]
    fn identity_is_ours_and_the_session_follows_the_client() {
        let mut client = axum::http::HeaderMap::new();
        client.insert("session_id", "conv-42".parse().unwrap());
        let token = jwt(serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_compute_residency": "eu"}
        }));
        let upstream = vec![("authorization".to_string(), format!("Bearer {token}"))];
        let h = identity_headers(&client, &upstream);
        let get = |k: &str| h.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("originator"), Some("thinkwatch"));
        assert!(get("user-agent").unwrap().starts_with("thinkwatch/"));
        assert_eq!(get("session-id"), Some("conv-42"));
        assert_eq!(get(RESIDENCY_HEADER), Some("eu"));

        // 没有会话 ID 就新起一个；不要求驻留就不带那个头
        let plain = identity_headers(&axum::http::HeaderMap::new(), &[]);
        assert!(
            plain
                .iter()
                .any(|(n, v)| n == "session-id" && !v.is_empty())
        );
        assert!(!plain.iter().any(|(n, _)| n == RESIDENCY_HEADER));

        // 客户端报的来源不转发：请求经过的是 ThinkWatch
        assert!(!keeps_client_header("Originator"));
        assert!(!keeps_client_header("user-agent"));
        assert!(!keeps_client_header("accept"));
        assert!(keeps_client_header("x-codex-turn-state"));
    }

    #[test]
    fn account_fields_come_from_the_auth_claims() {
        let id = jwt(serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acc-1",
                "chatgpt_plan_type": "plus",
                "chatgpt_compute_residency": "no_constraint"
            }
        }));
        assert_eq!(
            account(&id),
            Account {
                account_id: Some("acc-1".into()),
                plan: Some("plus".into()),
                residency: None,
            }
        );
        assert_eq!(account("not-a-jwt"), Account::default());
    }

    #[test]
    fn models_hidden_by_the_backend_are_not_listed() {
        let v = serde_json::json!({"models": [
            {"slug": "gpt-6-astra", "visibility": "list", "supported_in_api": true},
            {"slug": "codex-auto-review", "visibility": "hide", "supported_in_api": true},
            {"slug": "gpt-5.5", "visibility": "list"},
            {"slug": "legacy", "visibility": "list", "supported_in_api": false}
        ]});
        assert_eq!(
            parse_models(&v),
            Some(vec!["gpt-6-astra".to_string(), "gpt-5.5".to_string()])
        );
        assert_eq!(parse_models(&serde_json::json!({"data": []})), None);
        assert_eq!(
            models_url(BASE_URL),
            "https://chatgpt.com/backend-api/codex/models?client_version=0.0.0"
        );
    }

    #[test]
    fn wham_lives_next_to_codex() {
        assert_eq!(
            wham_url(BASE_URL, "usage"),
            "https://chatgpt.com/backend-api/wham/usage"
        );
        assert_eq!(
            wham_url(
                "http://127.0.0.1:9/backend-api/codex/",
                "/rate-limit-reset-credits"
            ),
            "http://127.0.0.1:9/backend-api/wham/rate-limit-reset-credits"
        );
    }

    #[test]
    fn the_authorize_url_says_who_is_asking_and_carries_the_challenge() {
        let pkce = Pkce::new();
        assert!(pkce.verifier.len() >= 43 && pkce.verifier.len() <= 128);
        assert_ne!(pkce.verifier, Pkce::new().verifier);
        let url = authorize_url(ISSUER, &redirect_uri(1455), &pkce, "st-1");
        let parsed = reqwest::Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(parsed.host_str(), Some("auth.openai.com"));
        assert_eq!(q["client_id"], CLIENT_ID);
        assert_eq!(q["redirect_uri"], "http://localhost:1455/auth/callback");
        assert_eq!(q["code_challenge"], pkce.challenge);
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["state"], "st-1");
        assert_eq!(q["originator"], "thinkwatch");
        assert!(q["scope"].contains("offline_access"));
    }

    #[test]
    fn tokens_never_show_up_in_debug_output() {
        let t = Tokens {
            id_token: "id-secret".into(),
            access: "at-secret".into(),
            refresh: "rt-secret".into(),
            expires_in: Some(10),
        };
        let s = format!("{t:?}");
        assert!(!s.contains("secret"), "{s}");
    }
}
