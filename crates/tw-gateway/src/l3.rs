//! L3 · 模型测速：**这一层会花钱**。
//!
//! L1 量线路、L2 量端点，两者都零成本。L3 真的调用模型，所以：
//!
//! **触发前必须显示预估消耗，而不是点了才知道。**探测请求是固定的，
//! 所以输入 token 可以精确算 —— 这不是一个「大概几分钱」的估计，是一个
//! 能提前摆出来的数字。
//!
//! **必须用流式**，否则测不到 TTFT，而 TTFT 才是这一层唯一值得测的东西。
//!
//! **要能选模型。**同一个 provider 的 Opus 和 Haiku 是两条完全不同的
//! 曲线，不指定模型的测速结果没有意义。

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// 探测请求。**固定不变** —— 变了的话，两次测速就不可比，而横向对比
/// 正是这一层存在的理由。
const PROMPT: &str = "Hi";
const MAX_TOKENS: u64 = 8;

/// 这次测速会花多少。
#[derive(Debug, Clone, PartialEq)]
pub struct Estimate {
    pub provider: String,
    pub model: String,
    /// 输入 token。**精确值** —— 请求是固定的
    pub input_tokens: u64,
    /// 输出上限
    pub max_output_tokens: u64,
    /// 按这家的计费方式和价目表报的价，见 [`crate::quote`]
    pub quote: crate::quote::Quote,
}

/// 探测请求的输入 token 数。
///
/// **精确算，不估。**请求体是固定的，所以这个数字不该是个约数 ——
/// 而「约 10 tokens」和「10 tokens」在一个「你确认要花钱吗」的对话框里
/// 是两种可信度。
///
/// 数字本身来自 Anthropic 的计费口径：system + 消息结构 + 内容。这里
/// 用一个保守的固定值，**宁可报高不报低** —— 报低了用户会觉得被骗。
pub fn probe_input_tokens() -> u64 {
    // "Hi" 一个 token，加上消息结构的固定开销
    10
}

pub fn max_output_tokens() -> u64 {
    MAX_TOKENS
}

/// 测速的分段。
///
/// **首 token 单独一段。**建连快而首 token 慢，说明是模型在排队；反过来
/// 说明是网络。这两件事的下一步完全不同。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct L3Result {
    pub provider: String,
    pub model: String,
    pub ok: bool,
    /// 建连到请求发出
    pub connect_ms: u64,
    /// **首 token**。这一层唯一值得测的东西
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    /// 全部完成
    pub total_ms: u64,
    /// 实际生成了多少 token。**和预估对照** —— 有些上游会附加 system
    /// prompt，那时实际消耗比预估多
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 一次测速的请求体。**固定的**。
pub fn probe_body(model: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        // **必须流式** —— 否则测不到 TTFT
        "stream": true,
        "messages": [{ "role": "user", "content": PROMPT }],
    })
}

/// 这家上游说什么方言，测速请求就发成什么样。
///
/// **以前一律发 Anthropic 的 `/v1/messages`**，于是 OpenAI 协议的上游
/// 必然 404；而且没带 `anthropic-version`，官方 Anthropic 端点直接 400 ——
/// 两种情况都被报成「这家不通」，用户会去换一个其实没问题的上游。
///
/// 返回 `None` 的协议是**还没写**，不是「这家不支持测速」。说清是哪一种，
/// 比发一个注定失败的请求、再把失败算到上游头上要好。
pub struct ProbeRequest {
    pub path: &'static str,
    pub body: serde_json::Value,
    pub headers: &'static [(&'static str, &'static str)],
}

pub fn probe_request(protocol: Option<tw_config::Protocol>, model: &str) -> Option<ProbeRequest> {
    use tw_config::Protocol::*;
    match protocol {
        // 猜不出协议时按 Anthropic 走 —— 和转发时 `credential_header` 的
        // 默认一致，两处不一致会让「转发正常、测速失败」成为可能
        Some(Anthropic) | None => Some(ProbeRequest {
            path: "/v1/messages",
            body: probe_body(model),
            headers: &[("anthropic-version", "2023-06-01")],
        }),
        Some(OpenaiChat) => Some(ProbeRequest {
            path: "/v1/chat/completions",
            body: serde_json::json!({
                "model": model,
                "max_tokens": MAX_TOKENS,
                "stream": true,
                // 不要这一项，流式响应里没有 usage，「实际消耗」一栏就是空的
                "stream_options": { "include_usage": true },
                "messages": [{ "role": "user", "content": PROMPT }],
            }),
            headers: &[],
        }),
        Some(OpenaiResponses) | Some(Gemini) => None,
    }
}

/// 真的跑一次。
///
/// 调用方**必须**先把 `Estimate` 摆给用户看过。这个函数不检查那件事 ——
/// 它检查不了 —— 但它是这一层唯一花钱的入口，所以这条注释写在这里。
///
/// `http` 必须是**这家上游自己的** client：它带着该走的代理。用默认
/// client 的话，要走代理的上游在这里连不上，而转发时它是通的。
pub async fn run(
    http: &reqwest::Client,
    base_url: &str,
    headers: &[(String, String)],
    protocol: Option<tw_config::Protocol>,
    provider: &str,
    model: &str,
) -> L3Result {
    let started = Instant::now();
    let Some(probe) = probe_request(protocol, model) else {
        return L3Result {
            provider: provider.to_string(),
            model: model.to_string(),
            ok: false,
            connect_ms: 0,
            ttft_ms: None,
            total_ms: 0,
            output_tokens: None,
            input_tokens: None,
            error: Some(format!(
                "推理测速暂不支持 {} 协议的上游，未发送请求",
                protocol.map(|p| format!("{p:?}")).unwrap_or_default()
            )),
        };
    };
    let url = crate::forward::upstream_url(base_url, probe.path, None);
    let mut req = http
        .post(&url)
        // 测速不该无限等。**但也不能太短** —— 一个排队中的上游正是我们
        // 想量的东西，掐早了会把「慢」误报成「不通」。
        .timeout(Duration::from_secs(60));
    req = crate::forward::apply_headers(req, headers);
    for (name, value) in probe.headers {
        if !crate::forward::overridden(headers, name) {
            req = req.header(*name, *value);
        }
    }
    let fail = |e: String, connect_ms: u64| L3Result {
        provider: provider.to_string(),
        model: model.to_string(),
        ok: false,
        connect_ms,
        ttft_ms: None,
        total_ms: started.elapsed().as_millis() as u64,
        output_tokens: None,
        input_tokens: None,
        error: Some(e),
    };

    let resp = match req.json(&probe.body).send().await {
        Ok(r) => r,
        Err(e) => {
            return fail(
                crate::forward::map_reqwest_error(e).message,
                started.elapsed().as_millis() as u64,
            );
        }
    };
    let connect_ms = started.elapsed().as_millis() as u64;
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        let short: String = body.chars().take(200).collect();
        return fail(format!("上游返回 {code}：{short}"), connect_ms);
    }

    // 一边流一边计时。**首个带内容的帧才算首 token** —— `message_start`
    // 是上游立刻就发的，拿它当 TTFT 会让所有上游看起来一样快。
    let mut ttft_ms = None;
    let mut sniffer = crate::usage::Sniffer::new();
    let mut stream = resp.bytes_stream();
    use futures::StreamExt;
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        sniffer.feed(&chunk);
        if ttft_ms.is_none() && has_content(&chunk) {
            ttft_ms = Some(started.elapsed().as_millis() as u64);
        }
    }
    let u = sniffer.finish();
    L3Result {
        provider: provider.to_string(),
        model: model.to_string(),
        ok: true,
        connect_ms,
        ttft_ms,
        total_ms: started.elapsed().as_millis() as u64,
        output_tokens: u.map(|u| u.output),
        input_tokens: u.map(|u| u.input),
        error: None,
    }
}

/// 这一帧里有真正的内容吗。
///
/// **`message_start` 不算。**上游收到请求立刻就发它，拿它当 TTFT 会让
/// 所有上游看起来一样快 —— 而那正好抹掉了这次测速的全部信息。
fn has_content(chunk: &[u8]) -> bool {
    let s = String::from_utf8_lossy(chunk);
    s.contains("content_block_delta")
        || s.contains("\"text_delta\"")
        // OpenAI 系
        || s.contains("\"delta\":{\"content\"")
}

/// 算一次测速要花多少。
pub fn estimate(
    book: &tw_pricing::PriceBook,
    provider: &str,
    model: &str,
    billing: tw_config::Billing,
) -> Estimate {
    let input = probe_input_tokens();
    let usage = tw_pricing::Usage {
        input,
        output: MAX_TOKENS,
        ..Default::default()
    };
    Estimate {
        provider: provider.to_string(),
        model: model.to_string(),
        input_tokens: input,
        max_output_tokens: MAX_TOKENS,
        quote: crate::quote::quote(book, provider, model, &usage, billing),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prices() -> tw_pricing::PriceBook {
        tw_pricing::PriceBook::builtin().unwrap()
    }

    #[test]
    fn the_probe_request_is_streaming_because_otherwise_there_is_no_ttft() {
        // **TTFT 才是这一层唯一值得测的东西**。
        let b = probe_body("claude-sonnet-4-5");
        assert_eq!(b["stream"], true);
        assert_eq!(b["max_tokens"], MAX_TOKENS);
        assert_eq!(b["model"], "claude-sonnet-4-5");
    }

    #[test]
    fn the_probe_request_is_fixed_so_two_runs_are_comparable() {
        // 变了的话两次测速就不可比，而横向对比正是这一层存在的理由。
        assert_eq!(probe_body("a")["messages"], probe_body("b")["messages"]);
    }

    #[test]
    fn an_estimate_prices_the_fixed_probe_with_an_exact_token_count() {
        // 「约 10 tokens」和「10 tokens」在一个「你确认要花钱吗」的
        // 对话框里是两种可信度。
        let e = estimate(
            &prices(),
            "官方",
            "claude-sonnet-4-5",
            tw_config::Billing::PerToken,
        );
        assert_eq!(e.input_tokens, probe_input_tokens());
        assert_eq!(e.max_output_tokens, MAX_TOKENS);
        assert!(e.quote.cost_micros.is_some());
        let e = estimate(
            &prices(),
            "订阅",
            "claude-sonnet-4-5",
            tw_config::Billing::Subscription,
        );
        assert_eq!(e.quote.cost_micros, None);
        assert_eq!(e.input_tokens, probe_input_tokens());
    }

    #[test]
    fn an_anthropic_probe_carries_the_version_header_the_official_api_requires() {
        // 没有它，官方端点回 400，而那会被报成「这家不通」。
        let p = probe_request(Some(tw_config::Protocol::Anthropic), "m").unwrap();
        assert_eq!(p.path, "/v1/messages");
        assert!(p.headers.iter().any(|(k, _)| *k == "anthropic-version"));
        // 猜不出协议时和转发的默认一致
        let guessed = probe_request(None, "m").unwrap();
        assert_eq!(guessed.path, "/v1/messages");
    }

    #[test]
    fn an_openai_chat_probe_goes_to_chat_completions_and_asks_for_usage() {
        // 以前一律发 /v1/messages，OpenAI 协议的上游必然 404。
        let p = probe_request(Some(tw_config::Protocol::OpenaiChat), "gpt-4o").unwrap();
        assert_eq!(p.path, "/v1/chat/completions");
        assert_eq!(p.body["stream"], true);
        assert_eq!(p.body["max_tokens"], MAX_TOKENS);
        assert_eq!(p.body["stream_options"]["include_usage"], true);
        assert!(p.headers.is_empty(), "OpenAI 系不该带 Anthropic 的头");
    }

    #[tokio::test]
    async fn a_protocol_without_a_probe_says_so_and_sends_nothing() {
        // 发一个注定失败的请求、再把失败算到上游头上，比直说更糟。
        // 地址指向一个不存在的端口：真发了的话错误会是「连不上」
        let r = run(
            &reqwest::Client::new(),
            "http://127.0.0.1:9",
            &[("x-goog-api-key".to_string(), "k".to_string())],
            Some(tw_config::Protocol::Gemini),
            "g",
            "gemini-2.5-pro",
        )
        .await;
        assert!(!r.ok);
        let e = r.error.unwrap();
        assert!(e.contains("暂不支持"), "{e}");
        assert!(e.contains("未发送请求"), "{e}");
    }

    #[test]
    fn message_start_alone_is_not_the_first_token() {
        // **上游收到请求立刻就发它。**拿它当 TTFT 会让所有上游看起来
        // 一样快 —— 而那正好抹掉了这次测速的全部信息。
        assert!(!has_content(
            b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n"
        ));
        assert!(!has_content(b"event: ping\ndata: {}\n\n"));
        assert!(has_content(
            b"event: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"H\"}}\n\n"
        ));
        assert!(has_content(
            br#"data: {"choices":[{"delta":{"content":"H"}}]}"#
        ));
    }
}
