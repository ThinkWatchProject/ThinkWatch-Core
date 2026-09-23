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
//!
//! # 探测请求长什么样，由转换层说了算
//!
//! 请求体、路径和查询串都从中间表示编码出来（[`tw_dialect::convert::encode`]），
//! 和转发时走的是同一套代码：手写一份「测速专用」的请求体，迟早会和真正转发出去的
//! 那一份不一样 —— 那时「转发正常、测速失败」查起来毫无头绪。ChatGPT 账号那条另有
//! 三处不同（见 [`crate::chatgpt`]），在 [`probe_request`] 里补齐。

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tw_config::{Billing, Protocol, Provider};
use tw_dialect::ir::{self, Dialect};
use tw_types::{Msg, msg};

/// 探测请求。**固定不变** —— 变了的话，两次测速就不可比，而横向对比
/// 正是这一层存在的理由。
const PROMPT: &str = "Hi";
const MAX_TOKENS: u64 = 8;
/// 会推理的模型要留出推理的量。
///
/// **推理 token 也算输出。**按一句话的长度（8）设上限发过去，模型会把额度全花在
/// 推理上，一个可见的 token 都不吐 —— 而首 token 正是这一层要测的东西。这是上限
/// 不是预期：答一句「Hi」实际用的远不到这个数，结果里报的是真实用量。
const REASONING_MAX_TOKENS: u64 = 512;

/// 这次测速会花多少。
#[derive(Debug, Clone, PartialEq)]
pub struct Estimate {
    pub provider: String,
    pub model: String,
    /// 输入 token。**精确值** —— 请求是固定的
    pub input_tokens: u64,
    /// 输出上限。**ChatGPT 账号是空的** —— Codex 后端不接受输出上限
    pub max_output_tokens: Option<u64>,
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

/// 这次探测请求给多少输出额度。**空 = 这家不接受输出上限**。
///
/// 三种情况：
/// - ChatGPT 账号：Codex 后端不认 `max_output_tokens`，带上去是 400（见
///   [`crate::chatgpt`]）。上限报不出来，费用也就报不出来
/// - 会推理的模型：留出推理的量，见 [`REASONING_MAX_TOKENS`]
/// - 其余：一句话的长度就够
///
/// Anthropic 那边不看模型会不会推理：**要推理得在请求里写**，而探测请求不写，
/// 所以它不会花额度在推理上。
pub fn max_output_tokens(
    book: &tw_pricing::PriceBook,
    model: &str,
    protocol: Option<Protocol>,
) -> Option<u64> {
    if protocol == Some(Protocol::Chatgpt) {
        return None;
    }
    let reasons = dialect_for(protocol) != Dialect::Anthropic
        && book.table().get(model).is_some_and(|p| p.reasoning);
    Some(if reasons {
        REASONING_MAX_TOKENS
    } else {
        MAX_TOKENS
    })
}

/// 这家上游说什么方言。**猜不出协议时按 Anthropic 走** —— 和转发时
/// `credential_header` 的默认一致，两处不一致会让「转发正常、测速失败」成为可能。
fn dialect_for(protocol: Option<Protocol>) -> Dialect {
    protocol
        .map(crate::translate::dialect_of)
        .unwrap_or(Dialect::Anthropic)
}

/// 探测请求的中间表示。**四种格式共用这一份**。
fn probe(model: &str, max_output_tokens: Option<u64>) -> ir::Request {
    ir::Request {
        model: model.to_string(),
        messages: vec![ir::Message {
            role: ir::Role::User,
            parts: vec![ir::Part::Text(PROMPT.to_string())],
        }],
        max_tokens: max_output_tokens,
        // **必须流式** —— 否则测不到 TTFT
        stream: true,
        ..Default::default()
    }
}

/// 发到哪、发什么、额外带什么头。
pub struct ProbeRequest {
    pub path: String,
    pub query: Option<String>,
    pub body: Vec<u8>,
    /// 目标格式必需的头，以及 ChatGPT 账号的身份头。上游配置里写了同名头时以配置为准
    pub headers: Vec<(String, String)>,
}

/// 这家上游说什么方言，测速请求就发成什么样。
///
/// `upstream_headers` 是这家配置好的请求头（含刚取到的凭据）：ChatGPT 账号的数据
/// 驻留头要从 access token 里读，见 [`crate::chatgpt::identity_headers`]。
pub fn probe_request(
    provider: &Provider,
    model: &str,
    max_output_tokens: Option<u64>,
    upstream_headers: &[(String, String)],
) -> ProbeRequest {
    let protocol = provider.effective_protocol();
    let dialect = dialect_for(protocol);
    let prepared = tw_dialect::convert::encode(
        &probe(model, max_output_tokens),
        &ir::Target {
            dialect,
            official: provider.is_official_endpoint(),
            default_max_tokens: MAX_TOKENS,
        },
    );
    let mut headers: Vec<(String, String)> = crate::translate::required_headers(dialect)
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let (mut path, mut query) = (prepared.path, prepared.query);
    if protocol == Some(Protocol::Chatgpt) {
        // Codex 后端的生成接口是 `{base}/responses`，不在 `/v1` 下；请求来自谁由
        // 网关如实填（和转发时同一处代码）
        path = "/responses".to_string();
        query = None;
        headers.extend(crate::chatgpt::identity_headers(
            &axum::http::HeaderMap::new(),
            upstream_headers,
        ));
    }
    ProbeRequest {
        path,
        query,
        body: prepared.body,
        headers,
    }
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
    pub error: Option<Msg>,
}

/// 真的跑一次。
///
/// 调用方**必须**先把 `Estimate` 摆给用户看过。这个函数不检查那件事 ——
/// 它检查不了 —— 但它是这一层唯一花钱的入口，所以这条注释写在这里。
///
/// `http` 必须是**这家上游自己的** client：它带着该走的代理。用默认
/// client 的话，要走代理的上游在这里连不上，而转发时它是通的。
///
/// `max_output_tokens` 要和报价里那个是同一个数（见 [`max_output_tokens`]）：
/// 摆给用户看的上限和真正发出去的不一样，那份报价就不是这次消耗的报价。
pub async fn run(
    http: &reqwest::Client,
    provider: &Provider,
    headers: &[(String, String)],
    model: &str,
    max_output_tokens: Option<u64>,
) -> L3Result {
    let started = Instant::now();
    let dialect = dialect_for(provider.effective_protocol());
    let probe = probe_request(provider, model, max_output_tokens, headers);
    let url = crate::forward::upstream_url(&provider.base_url, &probe.path, probe.query.as_deref());
    let mut req = http
        .post(&url)
        // 测速不该无限等。**但也不能太短** —— 一个排队中的上游正是我们
        // 想量的东西，掐早了会把「慢」误报成「不通」。
        .timeout(Duration::from_secs(60))
        .header(axum::http::header::CONTENT_TYPE, "application/json");
    req = crate::forward::apply_headers(req, headers);
    for (name, value) in &probe.headers {
        if !crate::forward::overridden(headers, name) {
            req = req.header(name, value);
        }
    }
    let at = |started: &Instant| started.elapsed().as_millis() as u64;
    let result = |ok: bool, connect_ms: u64, error: Option<Msg>| L3Result {
        provider: provider.name.clone(),
        model: model.to_string(),
        ok,
        connect_ms,
        ttft_ms: None,
        total_ms: at(&started),
        output_tokens: None,
        input_tokens: None,
        error,
    };

    let resp = match req.body(probe.body).send().await {
        Ok(r) => r,
        Err(e) => {
            let detail = crate::forward::map_reqwest_error(e).detail;
            return result(false, at(&started), Some(detail));
        }
    };
    let connect_ms = at(&started);
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        let detail: String = body.chars().take(200).collect();
        return result(
            false,
            connect_ms,
            Some(msg!(
                "l3.refused", status = status, detail = detail =>
                "The upstream answered {status}: {detail}"
            )),
        );
    }

    // 一边流一边计时。**按上游格式解析，不在字节里找关键字**：`message_start`、
    // `response.created` 这些是上游收到请求立刻就发的，拿它们当 TTFT 会让所有上游
    // 看起来一样快 —— 而那正好抹掉了这次测速的全部信息。
    let mut reader = tw_dialect::convert::Reader::new(dialect);
    let mut stream = resp.bytes_stream();
    use futures::StreamExt;
    // **时刻在收到那一块时就记下来**，不能等流结束再算
    let mut events: Vec<(u64, ir::Event)> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        events.extend(reader.feed(&chunk).into_iter().map(|e| (at(&started), e)));
    }
    events.extend(reader.finish().into_iter().map(|e| (at(&started), e)));

    let answered = !events.is_empty();
    let mut ttft_ms = None;
    let mut usage: Option<ir::Usage> = None;
    let mut stream_error = None;
    for (ms, e) in events {
        match e {
            // 推理也算开口了：会推理的模型先吐推理 token，把它排除掉的话，测出来的
            // 是「推理完了才算的首 token」
            ir::Event::Delta { delta, .. } => {
                let said = match &delta {
                    ir::Delta::Text(t) | ir::Delta::Thinking(t) => !t.is_empty(),
                    _ => false,
                };
                if said && ttft_ms.is_none() {
                    ttft_ms = Some(ms);
                }
            }
            ir::Event::Usage(u) => usage.get_or_insert_with(Default::default).merge(&u),
            // **上游 200 之后在流里报错**：不看这一条的话，这次测速会被算成成功
            ir::Event::Error { message } => stream_error = Some(message),
            _ => {}
        }
    }

    let error = match (stream_error, answered) {
        (Some(detail), _) => Some(msg!(
            "l3.stream_failed", detail = detail =>
            "The upstream reported an error while answering: {detail}"
        )),
        // 一个事件都没读出来：上游没按流式回答，而没有流就没有首 token
        (None, false) => Some(msg!(
            "l3.not_streamed" =>
            "The upstream did not answer with a stream, so there is no first-token time."
        )),
        (None, true) => None,
    };
    L3Result {
        provider: provider.name.clone(),
        model: model.to_string(),
        ok: error.is_none(),
        connect_ms,
        ttft_ms,
        total_ms: at(&started),
        output_tokens: usage.map(|u| u.output),
        input_tokens: usage.map(|u| u.prompt_total()),
        error,
    }
}

/// 算一次测速要花多少。
pub fn estimate(
    book: &tw_pricing::PriceBook,
    provider: &str,
    model: &str,
    billing: Billing,
    protocol: Option<Protocol>,
) -> Estimate {
    let input = probe_input_tokens();
    let cap = max_output_tokens(book, model, protocol);
    let usage = tw_pricing::Usage {
        input,
        output: cap.unwrap_or_default(),
        ..Default::default()
    };
    let mut quote = crate::quote::quote(book, provider, model, &usage, billing);
    // 上限报不出来时，按量计费这次要花多少就是**不知道**，不是 0 —— 回答有多长
    // 由模型决定。不计费的那档照样是 0
    if cap.is_none() && billing == Billing::PerToken {
        quote.cost_micros = None;
    }
    Estimate {
        provider: provider.to_string(),
        model: model.to_string(),
        input_tokens: input,
        max_output_tokens: cap,
        quote,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn prices() -> tw_pricing::PriceBook {
        tw_pricing::PriceBook::builtin().unwrap()
    }

    fn provider(protocol: Option<Protocol>, base_url: &str) -> Provider {
        Provider {
            name: "p".into(),
            base_url: base_url.into(),
            protocol,
            ..Default::default()
        }
    }

    fn body_of(p: &ProbeRequest) -> Value {
        serde_json::from_slice(&p.body).unwrap()
    }

    #[test]
    fn the_probe_request_is_streaming_because_otherwise_there_is_no_ttft() {
        // **TTFT 才是这一层唯一值得测的东西**。
        for protocol in [
            Some(Protocol::Anthropic),
            Some(Protocol::OpenaiChat),
            Some(Protocol::OpenaiResponses),
            Some(Protocol::Gemini),
            Some(Protocol::Chatgpt),
            None,
        ] {
            let p = provider(protocol, "https://relay.example");
            let r = probe_request(&p, "m", Some(MAX_TOKENS), &[]);
            let v = body_of(&r);
            let streaming = v["stream"] == Value::Bool(true)
                || r.query.as_deref() == Some("alt=sse")
                || r.path.contains("streamGenerateContent");
            assert!(streaming, "{protocol:?} 的探测请求不是流式的：{v}");
        }
    }

    #[test]
    fn the_probe_request_is_fixed_so_two_runs_are_comparable() {
        // 变了的话两次测速就不可比，而横向对比正是这一层存在的理由。
        let p = provider(Some(Protocol::Anthropic), "https://relay.example");
        let one = body_of(&probe_request(&p, "a", Some(MAX_TOKENS), &[]));
        let two = body_of(&probe_request(&p, "b", Some(MAX_TOKENS), &[]));
        assert_eq!(one["messages"], two["messages"]);
    }

    #[test]
    fn every_kind_of_upstream_gets_a_probe_of_its_own_shape() {
        // 以前只写了 Anthropic 和 OpenAI Chat 两种，其余三种一律报「还不支持」，
        // 于是 ChatGPT 账号上游根本测不了
        let anthropic = probe_request(
            &provider(Some(Protocol::Anthropic), "https://api.anthropic.com"),
            "claude-sonnet-4-5",
            Some(MAX_TOKENS),
            &[],
        );
        assert_eq!(anthropic.path, "/v1/messages");
        // 没有它，官方端点回 400，而那会被报成「这家不通」
        assert!(
            anthropic
                .headers
                .iter()
                .any(|(k, _)| k == "anthropic-version")
        );

        let chat = probe_request(
            &provider(Some(Protocol::OpenaiChat), "https://relay.example"),
            "gpt-4o",
            Some(MAX_TOKENS),
            &[],
        );
        assert_eq!(chat.path, "/v1/chat/completions");
        // 不要这一项，流式响应里没有 usage，「实际消耗」一栏就是空的
        assert_eq!(body_of(&chat)["stream_options"]["include_usage"], true);
        assert!(chat.headers.is_empty(), "OpenAI 系不该带 Anthropic 的头");

        let responses = probe_request(
            &provider(Some(Protocol::OpenaiResponses), "https://relay.example"),
            "gpt-5",
            Some(MAX_TOKENS),
            &[],
        );
        assert_eq!(responses.path, "/v1/responses");
        assert_eq!(body_of(&responses)["store"], false);

        let gemini = probe_request(
            &provider(Some(Protocol::Gemini), "https://relay.example"),
            "gemini-2.5-pro",
            Some(MAX_TOKENS),
            &[],
        );
        assert_eq!(
            gemini.path,
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent"
        );
        assert_eq!(gemini.query.as_deref(), Some("alt=sse"));
    }

    #[test]
    fn a_chatgpt_account_is_probed_the_way_the_codex_backend_wants_it() {
        // 三处和 OpenAI Responses 不一样，和转发时是同一套：路径不在 `/v1` 下、
        // 不认输出上限、身份头由网关填
        let p = provider(Some(Protocol::Chatgpt), tw_config::chatgpt::BASE_URL);
        let r = probe_request(
            &p,
            "gpt-5.5",
            max_output_tokens(&prices(), "gpt-5.5", p.effective_protocol()),
            &[],
        );
        assert_eq!(r.path, "/responses");
        assert_eq!(r.query, None);
        let v = body_of(&r);
        assert!(
            v.get("max_output_tokens").is_none(),
            "Codex 后端回 400：{v}"
        );
        assert_eq!(v["stream"], true);
        assert_eq!(v["store"], false);
        let named = |n: &str| r.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(n));
        assert!(named("originator") && named("user-agent") && named("session-id"));
    }

    #[test]
    fn a_model_that_reasons_gets_room_to_reason() {
        // 推理 token 也算输出：上限按一句话给，模型一个可见的 token 都不吐
        let book = prices();
        let cap = |model, protocol| max_output_tokens(&book, model, protocol);
        assert_eq!(
            cap("gpt-5", Some(Protocol::OpenaiResponses)),
            Some(REASONING_MAX_TOKENS)
        );
        assert_eq!(cap("gpt-4o", Some(Protocol::OpenaiChat)), Some(MAX_TOKENS));
        // Anthropic 要推理得在请求里写，而探测请求不写
        assert_eq!(
            cap("claude-sonnet-4-5", Some(Protocol::Anthropic)),
            Some(MAX_TOKENS)
        );
        // 价目表里没有的模型：按不推理算
        assert_eq!(
            cap("中转站自己起的名字", Some(Protocol::OpenaiChat)),
            Some(MAX_TOKENS)
        );
        // Codex 后端不接受上限
        assert_eq!(cap("gpt-5.5", Some(Protocol::Chatgpt)), None);
    }

    #[test]
    fn an_estimate_prices_the_fixed_probe_with_an_exact_token_count() {
        // 「约 10 tokens」和「10 tokens」在一个「你确认要花钱吗」的
        // 对话框里是两种可信度。
        let e = estimate(
            &prices(),
            "官方",
            "claude-sonnet-4-5",
            Billing::PerToken,
            Some(Protocol::Anthropic),
        );
        assert_eq!(e.input_tokens, probe_input_tokens());
        assert_eq!(e.max_output_tokens, Some(MAX_TOKENS));
        assert!(e.quote.cost_micros.is_some());
        let e = estimate(
            &prices(),
            "本地",
            "claude-sonnet-4-5",
            Billing::Free,
            Some(Protocol::Anthropic),
        );
        assert_eq!(e.quote.cost_micros, Some(0));
        assert_eq!(e.input_tokens, probe_input_tokens());
    }

    #[test]
    fn an_upstream_that_takes_no_output_limit_has_no_amount_either() {
        // 回答有多长由模型决定：报一个看起来确定的数字，那是编的
        let e = estimate(
            &prices(),
            "chatgpt",
            "gpt-5.5",
            Billing::PerToken,
            Some(Protocol::Chatgpt),
        );
        assert_eq!(e.max_output_tokens, None);
        assert_eq!(e.quote.cost_micros, None);
        // 不计费的那档照样是 0
        let free = estimate(
            &prices(),
            "chatgpt",
            "gpt-5.5",
            Billing::Free,
            Some(Protocol::Chatgpt),
        );
        assert_eq!(free.quote.cost_micros, Some(0));
    }
}
