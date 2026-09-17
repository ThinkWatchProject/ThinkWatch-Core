//! 方言互转在管线上的接线。
//!
//! **只在客户端格式和上游协议不同、而且调的是生成回答时才醒过来。**同格式那条路
//! 一个字节都不碰（唯一的例外是去掉转换写出去的推理签名，见
//! [`tw_dialect::convert::strip_carried`]）。转换本身在 `tw-dialect` 里，这里只管
//! 三个接缝：
//!
//! 1. **要不要转**：上游协议认不出来时不转。猜一个方向的代价是「请求被改成另一个
//!    样子然后上游 400」，比不转难查得多
//! 2. **请求头**：客户端格式专属的头（`anthropic-version`、`openai-beta`……）发给
//!    别家是噪音，有的兼容实现还会因为不认识而拒绝；目标格式必需的头要补上
//! 3. **路径和查询串**：由转换给出，客户端的不再用

use tw_config::Protocol;
use tw_dialect::ir::{Dialect, Request};

use crate::client_api::ClientApi;

pub fn dialect_of(p: Protocol) -> Dialect {
    match p {
        Protocol::Anthropic => Dialect::Anthropic,
        Protocol::OpenaiChat => Dialect::Chat,
        Protocol::OpenaiResponses => Dialect::Responses,
        Protocol::Gemini => Dialect::Gemini,
    }
}

/// 这一跳转成哪种格式。`None` 是直通。
pub fn plan(
    api: Option<ClientApi>,
    generates: bool,
    upstream: Option<Protocol>,
) -> Option<Dialect> {
    let (api, upstream) = (api?, upstream?);
    if !generates {
        return None;
    }
    let target = dialect_of(upstream);
    (target != api.dialect()).then_some(target)
}

/// 转换时，客户端发来的这个请求头还发不发给上游。
pub fn keeps_header(client: Dialect, name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let own: &[&str] = match client {
        Dialect::Anthropic => &[
            "anthropic-version",
            "anthropic-beta",
            "anthropic-dangerous-direct-browser-access",
        ],
        // Codex 带的 originator / session_id / version 说的是它和 OpenAI 之间的事
        Dialect::Chat | Dialect::Responses => &[
            "openai-beta",
            "openai-organization",
            "openai-project",
            "chatgpt-account-id",
            "originator",
            "session_id",
            "conversation_id",
            "version",
        ],
        Dialect::Gemini => &["x-goog-api-client", "x-goog-user-project"],
    };
    !own.contains(&name.as_str())
}

/// 转成这种格式时必须带的请求头（上游配置里写了同名头时以配置为准）。
pub fn required_headers(target: Dialect) -> &'static [(&'static str, &'static str)] {
    match target {
        Dialect::Anthropic => &[("anthropic-version", "2023-06-01")],
        _ => &[],
    }
}

/// 规则里的参数改写，改在中间表示上：转换出去的请求四种格式一样生效。
pub fn apply_set(r: &mut Request, set: &tw_engine::SetAction) {
    if let Some(m) = &set.model {
        r.model = m.clone();
    }
    if let Some(t) = set.max_tokens {
        r.max_tokens = Some(t);
    }
    // 和直通时一样只做「关掉」：去掉推理配置，按模型默认走
    if set.thinking == Some(false) {
        r.reasoning = None;
    }
}

/// 客户端没写最大输出、目标格式又必须写（Anthropic）时用多少。
///
/// Claude 4 系列的输出上限都不低于 32000；Anthropic 兼容接口背后的别家模型
/// （DeepSeek 这类）上限多在 8192，写大了会被拒绝。
pub fn default_max_tokens(model: &str) -> u64 {
    if model.to_ascii_lowercase().contains("claude") {
        32000
    } else {
        8192
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_format_and_non_generating_calls_are_passthrough() {
        let messages = ClientApi::of_path("/v1/messages");
        assert_eq!(plan(messages, true, Some(Protocol::Anthropic)), None);
        // 计 token 不转，即使上游是另一种格式
        assert_eq!(plan(messages, false, Some(Protocol::OpenaiChat)), None);
        // 认不出上游协议：不猜
        assert_eq!(plan(messages, true, None), None);
        // 认不出客户端 API：不猜
        assert_eq!(plan(None, true, Some(Protocol::OpenaiChat)), None);
    }

    #[test]
    fn every_other_combination_converts_to_the_upstream_format() {
        for path in [
            "/v1/messages",
            "/v1/chat/completions",
            "/v1/responses",
            "/v1beta/models/g:generateContent",
        ] {
            let api = ClientApi::of_path(path);
            for p in [
                Protocol::Anthropic,
                Protocol::OpenaiChat,
                Protocol::OpenaiResponses,
                Protocol::Gemini,
            ] {
                let want = (dialect_of(p) != api.unwrap().dialect()).then_some(dialect_of(p));
                assert_eq!(plan(api, true, Some(p)), want, "{path} → {p:?}");
            }
        }
    }

    #[test]
    fn a_clients_own_headers_stay_behind_and_anthropic_gets_its_version() {
        assert!(!keeps_header(Dialect::Anthropic, "Anthropic-Beta"));
        assert!(keeps_header(Dialect::Anthropic, "user-agent"));
        assert!(!keeps_header(Dialect::Responses, "originator"));
        assert!(!keeps_header(Dialect::Gemini, "x-goog-api-client"));
        assert_eq!(
            required_headers(Dialect::Anthropic),
            [("anthropic-version", "2023-06-01")]
        );
        assert!(required_headers(Dialect::Gemini).is_empty());
    }
}
