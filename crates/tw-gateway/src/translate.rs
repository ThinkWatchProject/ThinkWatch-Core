//! 方言互转在管线上的接线（M6+）。
//!
//! **这一层只在客户端方言和上游协议不同时才醒过来。**同方言那条路
//! （九成五的场景）一个字节都不会被碰 —— 出站直通仍然成立。
//!
//! 接线本身很短，但它有三个必须做对的接缝：
//!
//! 1. **路径要跟着换。**`/v1/messages` 打到一个 OpenAI 上游上是 404。
//! 2. **头要跟着换。**`anthropic-version` 送给 OpenAI 上游没意义，
//!    而有些兼容实现会因为不认识的头直接 400。
//! 3. **响应也要换回来**，而且流式那条路要一路换到最后一帧。

use tw_config::Protocol;

use crate::client_api::ClientApi;

/// 这一次要不要翻译，往哪个方向。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Plan {
    /// 同方言，什么都不做。**九成五走这条**
    Passthrough,
    /// Anthropic 客户端 → OpenAI chat 上游
    AnthropicToOpenai,
}

/// 客户端说什么方言、上游说什么协议，决定这一次怎么走。
///
/// **认不出来就直通。**猜一个转换方向的代价是「请求被改成了另一个样子
/// 然后上游 400」，比不转难查得多 —— 而不转的表现是上游直接告诉你
/// 「我不认识这个格式」，那条错误信息本身就是线索。
pub fn plan(client: Option<ClientApi>, upstream: Option<Protocol>) -> Plan {
    match (client, upstream) {
        (Some(ClientApi::AnthropicMessages), Some(Protocol::OpenaiChat)) => Plan::AnthropicToOpenai,
        _ => Plan::Passthrough,
    }
}

impl Plan {
    pub fn active(&self) -> bool {
        !matches!(self, Plan::Passthrough)
    }
    /// 上游那边的路径。**`/v1/messages` 打到 OpenAI 上游是 404。**
    pub fn path<'a>(&self, original: &'a str) -> &'a str {
        match self {
            Plan::Passthrough => original,
            Plan::AnthropicToOpenai => "/v1/chat/completions",
        }
    }
    /// 这个请求头要不要送给上游。
    ///
    /// 翻译过去之后，方言专属的头全是噪音，而有些 OpenAI 兼容实现会
    /// 因为不认识的头直接 400。
    pub fn keeps_header(&self, name: &str) -> bool {
        match self {
            Plan::Passthrough => true,
            Plan::AnthropicToOpenai => !matches!(
                name.to_ascii_lowercase().as_str(),
                "anthropic-version"
                    | "anthropic-beta"
                    | "anthropic-dangerous-direct-browser-access"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_dialect_is_always_passthrough() {
        // **九成五走这条**，它一个字节都不该被碰。
        assert_eq!(
            plan(
                Some(ClientApi::AnthropicMessages),
                Some(Protocol::Anthropic)
            ),
            Plan::Passthrough
        );
        assert_eq!(
            plan(Some(ClientApi::OpenaiChat), Some(Protocol::OpenaiChat)),
            Plan::Passthrough
        );
        assert!(
            !plan(
                Some(ClientApi::AnthropicMessages),
                Some(Protocol::Anthropic)
            )
            .active()
        );
    }

    #[test]
    fn a_claude_client_on_an_openai_upstream_is_the_one_we_translate() {
        // 那是这个功能存在的全部理由：手上一把 DeepSeek 的 key，
        // 想让 Claude Code 用上。
        assert_eq!(
            plan(
                Some(ClientApi::AnthropicMessages),
                Some(Protocol::OpenaiChat)
            ),
            Plan::AnthropicToOpenai
        );
    }

    #[test]
    fn everything_we_have_not_built_falls_back_to_passthrough() {
        // **猜一个转换方向的代价是「请求被改成另一个样子然后上游 400」**，
        // 比不转难查得多。不转的话，上游会直接说「我不认识这个格式」——
        // 那条错误信息本身就是线索。
        for up in [
            Some(Protocol::Gemini),
            Some(Protocol::OpenaiResponses),
            None,
        ] {
            assert_eq!(
                plan(Some(ClientApi::AnthropicMessages), up),
                Plan::Passthrough,
                "{up:?}"
            );
        }
        assert_eq!(
            plan(Some(ClientApi::OpenaiChat), Some(Protocol::Anthropic)),
            Plan::Passthrough
        );
        assert_eq!(
            plan(Some(ClientApi::Gemini), Some(Protocol::OpenaiChat)),
            Plan::Passthrough
        );
    }

    #[test]
    fn the_path_follows_the_dialect() {
        // `/v1/messages` 打到 OpenAI 上游上是 404。
        assert_eq!(Plan::Passthrough.path("/v1/messages"), "/v1/messages");
        assert_eq!(
            Plan::AnthropicToOpenai.path("/v1/messages"),
            "/v1/chat/completions"
        );
    }

    #[test]
    fn dialect_specific_headers_do_not_travel_to_the_other_side() {
        // 有些 OpenAI 兼容实现会因为不认识的头直接 400。
        let p = Plan::AnthropicToOpenai;
        assert!(!p.keeps_header("anthropic-version"));
        assert!(!p.keeps_header("Anthropic-Beta"));
        assert!(p.keeps_header("content-type"));
        // 直通时一个都不剔 —— 那条路的规矩是 forward_headers 定的
        assert!(Plan::Passthrough.keeps_header("anthropic-version"));
    }
}
