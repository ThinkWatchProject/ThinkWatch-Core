//! 客户端调的是哪一种 API。
//!
//! # 看路径，不看密钥放在哪儿
//!
//! 以前按「网关密钥放在哪个请求头」猜：`x-api-key` 是 Anthropic、`Bearer` 是
//! OpenAI。理由是那个位置由 SDK 决定，而路径可能被中间层改写。
//!
//! **这个理由在最主要的那个客户端上就不成立。**Claude Code 用
//! `ANTHROPIC_AUTH_TOKEN` 配密钥时发的是 `Authorization: Bearer` —— 一键接管写的
//! 正是这个变量。于是被接管的 Claude Code 被当成 OpenAI 客户端：模型准入只放行
//! OpenAI 协议的上游，请求 Claude 模型被拒，出错时回的也是 OpenAI 的错误格式。
//!
//! 路径才是 API 本身：`/v1/messages` 就是 Anthropic Messages，不管密钥放在哪儿。
//! 认不出的路径（`None`）照旧直通，出错时的格式退回按密钥位置猜。

use tw_config::Protocol;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientApi {
    /// `/v1/messages`
    AnthropicMessages,
    /// `/v1/chat/completions`、`/v1/completions`、`/v1/embeddings`
    OpenaiChat,
    /// `/v1/responses`、Codex 的 `/backend-api/codex/responses`
    OpenaiResponses,
    /// `/v1beta/models/{model}:generateContent` 一类
    Gemini,
}

/// Gemini 在模型名后面用 `:动作` 表示调什么。
const GEMINI_ACTIONS: &[&str] = &[
    ":generateContent",
    ":streamGenerateContent",
    ":countTokens",
    ":embedContent",
    ":batchEmbedContents",
];

impl ClientApi {
    /// 按请求路径认。**认不出就是 `None`**，不猜 —— 猜错一个方向的代价是
    /// 请求被按另一种格式处理。
    pub fn of_path(path: &str) -> Option<ClientApi> {
        let p = path.trim_end_matches('/');
        // 有的客户端把 base_url 写成带 `/v1` 的，有的不带 —— 两种都认
        let tail = p.strip_prefix("/v1").unwrap_or(p);
        if tail == "/messages" || tail == "/messages/count_tokens" || tail == "/complete" {
            return Some(ClientApi::AnthropicMessages);
        }
        if tail == "/chat/completions" || tail == "/completions" || tail == "/embeddings" {
            return Some(ClientApi::OpenaiChat);
        }
        if tail == "/responses"
            || tail.starts_with("/responses/")
            || p == "/backend-api/codex/responses"
            || p.starts_with("/backend-api/codex/responses/")
        {
            return Some(ClientApi::OpenaiResponses);
        }
        if p.contains("/models/") && GEMINI_ACTIONS.iter().any(|a| p.ends_with(a)) {
            return Some(ClientApi::Gemini);
        }
        None
    }

    /// 和它同格式的上游协议。
    pub fn protocol(&self) -> Protocol {
        match self {
            ClientApi::AnthropicMessages => Protocol::Anthropic,
            ClientApi::OpenaiChat => Protocol::OpenaiChat,
            ClientApi::OpenaiResponses => Protocol::OpenaiResponses,
            ClientApi::Gemini => Protocol::Gemini,
        }
    }

    /// 路由规则里 `when.dialect` 写的词，和上游协议的写法一致。
    pub fn slug(&self) -> &'static str {
        self.protocol().slug()
    }

    /// 能服务这种请求的上游协议：同格式的直通，加上有现成转换的。
    ///
    /// **模型准入、`/v1/models` 和转发时选不选这家，用的都是这一张表** ——
    /// 列出来的模型发过去一定有人能接。
    pub fn servable_by(&self) -> &'static [Protocol] {
        match self {
            // 手上一把 DeepSeek / Kimi 的 key 想让 Claude Code 用上：见 `translate`
            ClientApi::AnthropicMessages => &[Protocol::Anthropic, Protocol::OpenaiChat],
            ClientApi::OpenaiChat => &[Protocol::OpenaiChat],
            ClientApi::OpenaiResponses => &[Protocol::OpenaiResponses],
            ClientApi::Gemini => &[Protocol::Gemini],
        }
    }

    /// 出错时用哪种格式回。
    pub fn error_dialect(&self) -> crate::error::Dialect {
        match self {
            ClientApi::AnthropicMessages => crate::error::Dialect::Anthropic,
            ClientApi::OpenaiChat | ClientApi::OpenaiResponses => crate::error::Dialect::Openai,
            ClientApi::Gemini => crate::error::Dialect::Gemini,
        }
    }
}

/// 这些协议写进模型目录时的名字，给目录的过滤用。
pub fn slugs(protocols: &[Protocol]) -> Vec<&'static str> {
    protocols.iter().map(|p| p.slug()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_api_is_recognised_by_its_path() {
        for (path, want) in [
            ("/v1/messages", ClientApi::AnthropicMessages),
            ("/v1/messages/count_tokens", ClientApi::AnthropicMessages),
            ("/messages", ClientApi::AnthropicMessages),
            ("/v1/chat/completions", ClientApi::OpenaiChat),
            ("/chat/completions", ClientApi::OpenaiChat),
            ("/v1/embeddings", ClientApi::OpenaiChat),
            ("/v1/responses", ClientApi::OpenaiResponses),
            ("/v1/responses/compact", ClientApi::OpenaiResponses),
            ("/backend-api/codex/responses", ClientApi::OpenaiResponses),
            (
                "/v1beta/models/gemini-2.5-pro:generateContent",
                ClientApi::Gemini,
            ),
            (
                "/v1beta/models/gemini-2.5-pro:streamGenerateContent",
                ClientApi::Gemini,
            ),
            ("/v1/models/gemini-2.5-flash:countTokens", ClientApi::Gemini),
        ] {
            assert_eq!(ClientApi::of_path(path), Some(want), "{path}");
        }
    }

    #[test]
    fn a_path_we_do_not_know_is_not_guessed() {
        for path in ["/v1/models", "/v1/files", "/healthz", "/v1/messagesx", "/"] {
            assert_eq!(ClientApi::of_path(path), None, "{path}");
        }
    }

    #[test]
    fn claude_code_is_an_anthropic_client_wherever_it_puts_the_key() {
        // 这正是改成看路径的原因：`ANTHROPIC_AUTH_TOKEN` 发的是 Bearer
        let api = ClientApi::of_path("/v1/messages").unwrap();
        assert_eq!(api.error_dialect(), crate::error::Dialect::Anthropic);
        assert!(api.servable_by().contains(&Protocol::Anthropic));
    }

    #[test]
    fn an_anthropic_client_can_be_served_by_an_openai_chat_upstream_but_not_the_reverse() {
        assert!(
            ClientApi::AnthropicMessages
                .servable_by()
                .contains(&Protocol::OpenaiChat)
        );
        assert!(
            !ClientApi::OpenaiChat
                .servable_by()
                .contains(&Protocol::Anthropic)
        );
    }
}
