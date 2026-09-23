//! Bedrock Converse 响应 ⇄ 中间表示。

use serde_json::{Value, json};

use crate::convert::Session;
use crate::ir::*;

/// Converse 的 `stopReason` → 中间表示。
///
/// 九个值里有三个是别家没有的失败态（`malformed_*`、`guardrail_intervened`）。
/// **它们不是「说完了」** —— 记成 `Other` 保留原文，比硬塞进 `EndTurn` 诚实:
/// 客户端看到 `end_turn` 会以为回答是完整的
pub fn stop_reason(s: &str) -> StopReason {
    match s {
        "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence(None),
        "content_filtered" | "guardrail_intervened" => StopReason::ContentFilter,
        "model_context_window_exceeded" => StopReason::ContextWindow,
        other => StopReason::Other(other.to_string()),
    }
}

/// 中间表示 → Converse 的 `stopReason`。
pub fn stop_reason_str(s: &StopReason) -> &str {
    match s {
        StopReason::EndTurn | StopReason::Paused => "end_turn",
        StopReason::ToolUse => "tool_use",
        StopReason::MaxTokens => "max_tokens",
        StopReason::StopSequence(_) => "stop_sequence",
        StopReason::ContentFilter | StopReason::Refusal => "content_filtered",
        StopReason::ContextWindow => "model_context_window_exceeded",
        StopReason::Other(s) => s,
    }
}

/// Converse 的 `usage` → 中间表示。
///
/// **`inputTokens` 含不含缓存,AWS 没有明说**，但 Bedrock 上跑的是各家原厂模型，
/// 而 Anthropic 的语义是不含。按不含读:读错的话缓存那部分会被少算，而按含读
/// 再减一遍，读错时会把正常输入减成负数
pub fn usage(u: &Value) -> Usage {
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input: n("inputTokens"),
        cache_read: n("cacheReadInputTokens"),
        cache_write: n("cacheWriteInputTokens"),
        output: n("outputTokens"),
        reasoning: 0,
    }
}

/// 中间表示 → Converse 的 `usage`。
pub fn usage_json(u: &Usage) -> Value {
    let mut out = json!({
        "inputTokens": u.input,
        "outputTokens": u.output,
        "totalTokens": u.prompt_total() + u.output,
    });
    if u.cache_read > 0 {
        out["cacheReadInputTokens"] = json!(u.cache_read);
    }
    if u.cache_write > 0 {
        out["cacheWriteInputTokens"] = json!(u.cache_write);
    }
    out
}

/// 一个内容块 → 中间表示。
pub(crate) fn block(b: &Value) -> Option<Block> {
    if let Some(t) = b.get("text").and_then(Value::as_str) {
        return (!t.is_empty()).then(|| Block::Text(t.to_string()));
    }
    if let Some(r) = b.get("reasoningContent") {
        if let Some(rt) = r.get("reasoningText") {
            return Some(Block::Thinking(Thinking {
                text: rt
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                signature: rt
                    .get("signature")
                    .and_then(Value::as_str)
                    .and_then(|s| Signature::read(s, Vendor::Anthropic)),
            }));
        }
        // 打码过的推理:内容看不到，但签名要带回去，否则下一轮会被拒
        if let Some(red) = r.get("redactedContent").and_then(Value::as_str) {
            return Some(Block::Thinking(Thinking {
                text: String::new(),
                signature: Some(Signature {
                    vendor: Vendor::Anthropic,
                    value: red.to_string(),
                    redacted: true,
                }),
            }));
        }
        return None;
    }
    if let Some(u) = b.get("toolUse") {
        return Some(Block::ToolCall(ToolCall {
            id: u
                .get("toolUseId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| new_id("tooluse_")),
            name: u
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            input: ToolInput::Json(u.get("input").cloned().unwrap_or_else(|| json!({}))),
        }));
    }
    None
}

pub fn decode_response(v: &Value) -> Response {
    let message = v
        .get("output")
        .and_then(|o| o.get("message"))
        .unwrap_or(&Value::Null);

    let blocks: Vec<Block> = message
        .get("content")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(block).collect())
        .unwrap_or_default();

    Response {
        // Converse 不发响应 id。调用方按客户端的格式补一个
        id: None,
        model: None,
        blocks,
        stop: v.get("stopReason").and_then(Value::as_str).map(stop_reason),
        usage: v.get("usage").map(usage),
    }
}

pub fn encode_response(r: &Response, _s: &Session) -> Value {
    let mut content = Vec::new();
    for b in &r.blocks {
        match b {
            Block::Text(t) if !t.is_empty() => content.push(json!({ "text": t })),
            Block::Text(_) => {}
            Block::Thinking(th) => match &th.signature {
                Some(s) if s.redacted => {
                    content.push(json!({ "reasoningContent": { "redactedContent": s.value } }))
                }
                sig => content.push(json!({
                    "reasoningContent": {
                        "reasoningText": {
                            "text": th.text,
                            "signature": sig.as_ref().map(|s| s.value.clone()).unwrap_or_default(),
                        },
                    },
                })),
            },
            Block::ToolCall(c) => content.push(json!({
                "toolUse": { "toolUseId": c.id, "name": c.name, "input": c.input.to_object() },
            })),
        }
    }

    let mut out = json!({
        "output": { "message": { "role": "assistant", "content": content } },
        "stopReason": stop_reason_str(r.stop.as_ref().unwrap_or(&StopReason::EndTurn)),
    });
    out["usage"] = usage_json(&r.usage.unwrap_or_default());
    out
}

/// Converse 的错误体。状态码走 HTTP，正文只有一句话。
pub fn error_body(_status: u16, message: &str) -> Value {
    json!({ "message": message })
}

pub fn error_message(v: &Value) -> Option<String> {
    v.get("message").and_then(Value::as_str).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_stop_reason_is_not_flattened_into_end_turn() {
        // Converse 有三个别家没有的失败态。说成 end_turn 会让客户端
        // 以为回答是完整的
        for raw in [
            "malformed_model_output",
            "malformed_tool_use",
            "guardrail_intervened",
        ] {
            let s = stop_reason(raw);
            assert_ne!(s, StopReason::EndTurn, "{raw} 被当成了正常收尾");
        }
        assert_eq!(
            stop_reason("malformed_tool_use"),
            StopReason::Other("malformed_tool_use".into()),
            "原文要保留下来"
        );
        // 这个有对应物
        assert_eq!(
            stop_reason("guardrail_intervened"),
            StopReason::ContentFilter
        );
    }

    #[test]
    fn the_context_window_reason_has_a_home() {
        assert_eq!(
            stop_reason("model_context_window_exceeded"),
            StopReason::ContextWindow
        );
    }

    #[test]
    fn cache_tokens_are_kept_apart_from_plain_input() {
        let u = usage(&json!({
            "inputTokens": 60, "cacheReadInputTokens": 40,
            "cacheWriteInputTokens": 10, "outputTokens": 20, "totalTokens": 130
        }));
        assert_eq!(u.input, 60);
        assert_eq!(u.cache_read, 40);
        assert_eq!(u.cache_write, 10);
        assert_eq!(u.output, 20);
        // 三者不重叠，加起来才是全部输入
        assert_eq!(u.prompt_total(), 110);
    }

    #[test]
    fn a_response_round_trips_through_converse() {
        let before = Response {
            id: None,
            model: None,
            blocks: vec![
                Block::Text("天气晴".into()),
                Block::ToolCall(ToolCall {
                    id: "tu_1".into(),
                    name: "get_weather".into(),
                    input: ToolInput::Json(json!({"city": "北京"})),
                }),
            ],
            stop: Some(StopReason::ToolUse),
            usage: Some(Usage {
                input: 60,
                cache_read: 40,
                cache_write: 0,
                output: 20,
                reasoning: 0,
            }),
        };
        let s = Session::for_test(Dialect::Bedrock, Dialect::Bedrock);
        let after = decode_response(&encode_response(&before, &s));

        assert_eq!(after.blocks, before.blocks);
        assert_eq!(after.stop, before.stop);
        assert_eq!(after.usage, before.usage);
    }
}
