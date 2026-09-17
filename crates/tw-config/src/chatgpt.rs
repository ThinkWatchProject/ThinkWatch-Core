//! ChatGPT 账号：用 OpenAI 登录拿到的凭据，调 ChatGPT 订阅背后的 Codex 后端。
//!
//! 这里只放配置层要认的东西：地址、token 端点、客户端 ID、哪些请求头由网关填。
//! 登录、刷新和转发的细节在网关和控制面。

/// Codex 后端。请求发到 `{BASE_URL}/responses`，模型清单在 `{BASE_URL}/models`
pub const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// OpenAI 的 token 端点：登录换 token、之后的刷新都在这里
pub const TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";

/// OAuth 客户端 ID。
///
/// **这是 Codex 的客户端 ID。**OpenAI 没有开放第三方注册 OAuth 客户端，OpenCode 等
/// 开源客户端用的也是这一个。除了它，请求里的身份都是 ThinkWatch 自己的（见
/// [`IDENTITY_HEADERS`]）。
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// 账户 ID 所在的请求头。登录时从 id_token 读出来，写进上游的 `headers`
pub const ACCOUNT_HEADER: &str = "ChatGPT-Account-Id";

/// 说明请求来自谁的请求头。
///
/// **由网关如实填写，配置里不能写。**能写的话，一行配置就能让 ThinkWatch 冒充
/// Codex 或别的客户端 —— 而接入 ChatGPT 账号的前提是如实说明自己是谁。
pub const IDENTITY_HEADERS: &[&str] = &[
    "originator",
    "user-agent",
    "session-id",
    "session_id",
    "version",
];

/// 这个地址是不是 Codex 后端。
///
/// 按 host 和路径比，**不按包含比**：`https://relay.example/chatgpt.com/backend-api/codex`
/// 也「含有」那一段。
pub fn is_backend(base_url: &str) -> bool {
    let rest = base_url.trim().split("://").nth(1).unwrap_or("");
    let path = rest.find('/').map_or("", |i| &rest[i..]);
    crate::credential::host_of(base_url) == "chatgpt.com"
        && (path.trim_end_matches('/') == "/backend-api/codex"
            || path.starts_with("/backend-api/codex/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_codex_backend_on_chatgpt_com_counts() {
        assert!(is_backend(BASE_URL));
        assert!(is_backend("https://chatgpt.com/backend-api/codex/"));
        assert!(is_backend("HTTPS://ChatGPT.com/backend-api/codex"));
        assert!(!is_backend("https://chatgpt.com/backend-api"));
        assert!(!is_backend("https://chatgpt.com/backend-api/codexx"));
        assert!(!is_backend(
            "https://relay.example/chatgpt.com/backend-api/codex"
        ));
        assert!(!is_backend(
            "https://chatgpt.com.evil.example/backend-api/codex"
        ));
    }
}
