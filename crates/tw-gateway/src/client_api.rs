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
use tw_dialect::convert::Decoded;
use tw_dialect::ir::{Dialect, Rejection};

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

    /// 这个路径是不是**生成回答**的那个调用。
    ///
    /// **只有生成回答能转换**：计 token（`/v1/messages/count_tokens`、`:countTokens`）、
    /// 嵌入、旧版补全、Responses 的压缩这些接口，别的格式没有对应物。以前
    /// `count_tokens` 打到 OpenAI 上游会被改写成一次真正的补全 —— 既花钱，回来的
    /// 也不是客户端要的形状。
    pub fn generates(path: &str) -> bool {
        let p = path.trim_end_matches('/');
        let tail = p.strip_prefix("/v1").unwrap_or(p);
        matches!(tail, "/messages" | "/chat/completions" | "/responses")
            || p == "/backend-api/codex/responses"
            || (p.contains("/models/")
                && (p.ends_with(":generateContent") || p.ends_with(":streamGenerateContent")))
    }

    /// 转换库里对应的格式
    pub fn dialect(&self) -> Dialect {
        match self {
            ClientApi::AnthropicMessages => Dialect::Anthropic,
            ClientApi::OpenaiChat => Dialect::Chat,
            ClientApi::OpenaiResponses => Dialect::Responses,
            ClientApi::Gemini => Dialect::Gemini,
        }
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

    /// 能服务这次调用的上游协议。
    ///
    /// **模型准入、`/v1/models` 和转发时选不选这家，用的都是这一张表** ——
    /// 列出来的模型发过去一定有人能接。生成回答四种格式互相转换，谁都能服务；
    /// 别的接口只有同格式的上游能处理（见 [`ClientApi::generates`]）。
    pub fn servable_by(&self, generates: bool) -> &'static [Protocol] {
        if generates {
            return &[
                Protocol::Anthropic,
                Protocol::OpenaiChat,
                Protocol::OpenaiResponses,
                Protocol::Gemini,
                Protocol::Chatgpt,
            ];
        }
        match self {
            ClientApi::AnthropicMessages => &[Protocol::Anthropic],
            ClientApi::OpenaiChat => &[Protocol::OpenaiChat],
            ClientApi::OpenaiResponses => &[Protocol::OpenaiResponses],
            ClientApi::Gemini => &[Protocol::Gemini],
        }
    }
}

/// 这些协议写进模型目录时的名字，给目录的过滤用。
pub fn slugs(protocols: &[Protocol]) -> Vec<&'static str> {
    protocols.iter().map(|p| p.slug()).collect()
}

/// 读一个请求：调的是哪种 API、是不是生成回答、解码成中间表示、抽出路由用的事实。
///
/// **管线和回放测试用的是同一个函数** —— 另写一份的话，回放验的是那一份，线上跑的
/// 是这一份。
#[derive(Debug)]
pub struct Reading {
    pub api: Option<ClientApi>,
    pub generates: bool,
    /// 生成回答的请求解码出来的中间表示。解码失败时是那条理由 —— **只在需要转换时
    /// 才用得上**：同格式直通时，我们解不开的东西上游可能完全认识
    pub decoded: Option<Result<Decoded, Rejection>>,
    pub facts: tw_engine::RequestFacts,
    /// DeepSeek Harness 发的请求（见 [`tw_dialect::harness`]）。要看请求头，由管线填
    pub harness: Option<tw_dialect::harness::Harness>,
}

pub fn read(path: &str, query: Option<&str>, body: Option<&serde_json::Value>) -> Reading {
    let api = ClientApi::of_path(path);
    let generates = api.is_some() && ClientApi::generates(path);
    let decoded = match (api, body) {
        (Some(a), Some(v)) if generates => {
            Some(tw_dialect::convert::decode(a.dialect(), v, path, query))
        }
        _ => None,
    };
    let mut facts = match (&decoded, body) {
        (Some(Ok(d)), Some(v)) => tw_engine::RequestFacts::from_request(&d.request, v),
        // 解不开或者不是生成回答：只取模型名，别的维度按默认走兜底规则
        (_, Some(v)) => tw_engine::RequestFacts {
            model: v
                .get("model")
                .and_then(|m| m.as_str())
                .map(str::to_string)
                .or_else(|| gemini_model(path))
                .unwrap_or_default(),
            ..Default::default()
        },
        (_, None) => tw_engine::RequestFacts {
            model: gemini_model(path).unwrap_or_default(),
            ..Default::default()
        },
    };
    facts.dialect = api.map(|a| a.slug()).unwrap_or_default().to_string();
    Reading {
        api,
        generates,
        decoded,
        facts,
        harness: None,
    }
}

/// Gemini 把模型写在路径里：`/v1beta/models/{model}:动作`
fn gemini_model(path: &str) -> Option<String> {
    let (_, rest) = path.split_once("/models/")?;
    let (model, _) = rest.rsplit_once(':')?;
    Some(model.to_string())
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
        assert_eq!(api.dialect(), Dialect::Anthropic);
        assert!(api.servable_by(true).contains(&Protocol::Anthropic));
    }

    #[test]
    fn generating_converts_to_every_protocol_but_other_calls_stay_home() {
        for (path, generates) in [
            ("/v1/messages", true),
            ("/v1/chat/completions", true),
            ("/v1/responses", true),
            ("/backend-api/codex/responses", true),
            ("/v1beta/models/gemini-2.5-pro:streamGenerateContent", true),
            ("/v1beta/models/gemini-2.5-pro:generateContent", true),
            // 计 token 转到别家会变成一次真的补全
            ("/v1/messages/count_tokens", false),
            ("/v1beta/models/gemini-2.5-pro:countTokens", false),
            ("/v1/embeddings", false),
            ("/v1/completions", false),
            ("/v1/responses/compact", false),
        ] {
            assert_eq!(ClientApi::generates(path), generates, "{path}");
        }
        assert_eq!(ClientApi::OpenaiChat.servable_by(true).len(), 5);
        assert_eq!(
            ClientApi::AnthropicMessages.servable_by(false),
            [Protocol::Anthropic]
        );
    }

    #[test]
    fn a_gemini_request_is_read_with_the_model_from_its_path() {
        let body = serde_json::json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let r = read(
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent",
            Some("alt=sse"),
            Some(&body),
        );
        assert!(r.generates);
        assert_eq!(r.facts.model, "gemini-2.5-pro");
        assert!(r.facts.stream);
        assert_eq!(r.facts.dialect, "gemini");
        let r = read(
            "/v1beta/models/gemini-2.5-pro:countTokens",
            None,
            Some(&body),
        );
        assert!(!r.generates && r.decoded.is_none());
        assert_eq!(r.facts.model, "gemini-2.5-pro");
    }
}
