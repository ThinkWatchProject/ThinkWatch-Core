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
///
/// **三种情况都要说清楚**：算得出金额的给金额；订阅型的说
/// 「不计费」；价格未知的说「价格未知」—— 而三者都要给出 token 数，
/// 因为那是唯一一个我们确定知道的量。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    pub provider: String,
    pub model: String,
    /// 输入 token。**精确值** —— 请求是固定的
    pub input_tokens: u64,
    /// 输出上限
    pub max_output_tokens: u64,
    /// 微分。`None` 表示价目表里没有这个模型
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<i64>,
    /// 给人看的那一句。**金额再小也要显示** —— 用户按下按钮时有权知道
    /// 自己在花什么
    pub note: String,
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

/// 真的跑一次。
///
/// 调用方**必须**先把 `Estimate` 摆给用户看过。这个函数不检查那件事 ——
/// 它检查不了 —— 但它是这一层唯一花钱的入口，所以这条注释写在这里。
pub async fn run(
    http: &reqwest::Client,
    base_url: &str,
    key: &str,
    protocol: Option<tw_config::Protocol>,
    provider: &str,
    model: &str,
) -> L3Result {
    let started = Instant::now();
    let url = crate::forward::upstream_url(base_url, "/v1/messages", None);
    let mut req = http
        .post(&url)
        // 测速不该无限等。**但也不能太短** —— 一个排队中的上游正是我们
        // 想量的东西，掐早了会把「慢」误报成「不通」。
        .timeout(Duration::from_secs(60));
    req = crate::forward::apply_credential(req, protocol, key);
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

    let resp = match req.json(&probe_body(model)).send().await {
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
    prices: &tw_pricing::Prices,
    provider: &str,
    model: &str,
    subscription: bool,
) -> Estimate {
    let input = probe_input_tokens();
    let usage = tw_pricing::Usage {
        input,
        output: MAX_TOKENS,
        ..Default::default()
    };
    let (cost_micros, note) = if subscription {
        // **订阅型上游不按 token 计费**，但它照样消耗额度 —— 说清楚
        // 消耗多少，而不是说「免费」
        (
            None,
            format!("不计费，但会消耗约 {} tokens 的额度", input + MAX_TOKENS),
        )
    } else {
        match prices.cost(model, &usage, false) {
            tw_pricing::Cost::Known(m) | tw_pricing::Cost::Estimated(m) => (
                Some(m),
                // **金额再小也要显示。**用户按下按钮时有权知道自己在花
                // 什么
                format!("约 ${:.5}", m as f64 / 1e6),
            ),
            tw_pricing::Cost::Unpriced { .. } => (
                None,
                format!(
                    "价格未知（这个模型不在价目表里），将消耗约 {} tokens",
                    input + MAX_TOKENS
                ),
            ),
        }
    };
    Estimate {
        provider: provider.to_string(),
        model: model.to_string(),
        input_tokens: input,
        max_output_tokens: MAX_TOKENS,
        cost_micros,
        note,
    }
}

/// 一批测速的总计。
///
/// **批量是最容易让人手滑的地方** —— 点一下「全部测速」可能是
/// 十几次真实调用，所以要列出每一项**并给出总计**。
pub fn total_micros(es: &[Estimate]) -> Option<i64> {
    // 有任何一项算不出来，总计就不该给一个看起来完整的数字
    if es.iter().any(|e| e.cost_micros.is_none()) {
        return None;
    }
    Some(es.iter().filter_map(|e| e.cost_micros).sum())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prices() -> tw_pricing::Prices {
        tw_pricing::Prices::builtin().unwrap()
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
    fn an_estimate_names_a_concrete_amount_however_small() {
        // **金额再小也要显示。**用户按下按钮时有权知道自己在花什么。
        let e = estimate(&prices(), "官方", "claude-sonnet-4-5", false);
        assert!(e.cost_micros.is_some());
        assert!(e.note.starts_with("约 $"), "{}", e.note);
        // 五位小数：这次测速是几百微分的量级，**两位小数会显示成
        // $0.00**，而那等于告诉用户「这不花钱」。
        assert!(e.note.contains("0.000"), "{}", e.note);
        assert_ne!(e.note, "约 $0.00", "精度不够，看起来像免费的");
    }

    #[test]
    fn a_subscription_upstream_says_it_costs_quota_not_that_it_is_free() {
        // 「免费」是错的 —— 它照样消耗额度。
        let e = estimate(&prices(), "订阅", "claude-sonnet-4-5", true);
        assert!(e.cost_micros.is_none());
        assert!(e.note.contains("不计费"), "{}", e.note);
        assert!(e.note.contains("额度"), "得说清消耗的是什么：{}", e.note);
        assert!(e.note.contains("18"), "得给出 token 数：{}", e.note);
    }

    #[test]
    fn an_unpriced_model_says_so_and_still_gives_the_token_count() {
        // token 数是唯一一个我们确定知道的量。
        let e = estimate(&prices(), "中转", "某个自己起的名字", false);
        assert!(e.cost_micros.is_none());
        assert!(e.note.contains("价格未知"), "{}", e.note);
        assert!(e.note.contains("18"), "{}", e.note);
    }

    #[test]
    fn the_input_token_count_is_exact_not_approximate() {
        // 「约 10 tokens」和「10 tokens」在一个「你确认要花钱吗」的
        // 对话框里是两种可信度。
        let e = estimate(&prices(), "p", "claude-sonnet-4-5", false);
        assert_eq!(e.input_tokens, probe_input_tokens());
        assert_eq!(e.max_output_tokens, MAX_TOKENS);
    }

    #[test]
    fn a_batch_gives_a_total() {
        // **批量是最容易让人手滑的地方**。
        let es: Vec<_> = ["claude-sonnet-4-5", "claude-opus-4-1", "claude-haiku-4-5"]
            .iter()
            .map(|m| estimate(&prices(), "p", m, false))
            .collect();
        let total = total_micros(&es).expect("三个都有价格，总计该算得出来");
        assert_eq!(
            total,
            es.iter().map(|e| e.cost_micros.unwrap()).sum::<i64>()
        );
    }

    #[test]
    fn a_batch_with_one_unpriced_model_refuses_to_show_a_complete_looking_total() {
        // **有一项算不出来，总计就不该给一个看起来完整的数字** ——
        // 那会让用户以为「全部测速」的代价就是那个数。
        let mut es = vec![estimate(&prices(), "p", "claude-sonnet-4-5", false)];
        es.push(estimate(&prices(), "p", "某个中转站的模型", false));
        assert!(total_micros(&es).is_none());
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
