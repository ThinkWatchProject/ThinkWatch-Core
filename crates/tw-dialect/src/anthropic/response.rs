//! Anthropic Messages 整包响应 ⇄ 中间表示。

use serde_json::{Value, json};

use crate::convert::Session;
use crate::ir::*;

pub fn stop_reason(s: &str) -> StopReason {
    match s {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence(None),
        "tool_use" => StopReason::ToolUse,
        "refusal" => StopReason::Refusal,
        "model_context_window_exceeded" => StopReason::ContextWindow,
        "pause_turn" => StopReason::Paused,
        other => StopReason::Other(other.to_string()),
    }
}

/// **这个映射错了是真事故**：客户端看到 `tool_use` 会去执行工具，看到 `max_tokens`
/// 可能会续写。没有对应物的写 null —— 客户端看到 null 知道「没说」，看到一个编出来的
/// `end_turn` 会当成正常结束。
pub fn stop_reason_str(s: &StopReason) -> Value {
    json!(match s {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::StopSequence(_) => "stop_sequence",
        StopReason::ToolUse => "tool_use",
        StopReason::ContentFilter | StopReason::Refusal => "refusal",
        StopReason::ContextWindow => "model_context_window_exceeded",
        StopReason::Paused => "pause_turn",
        StopReason::Other(_) => return Value::Null,
    })
}

pub fn usage(u: &Value) -> Usage {
    let n = |k: &str| u64_of(u, k).unwrap_or(0);
    // 1 小时缓存写。细分在 `cache_creation` 里；有它就整笔按 1 小时算
    let one_hour = u
        .get("cache_creation")
        .and_then(|d| u64_of(d, "ephemeral_1h_input_tokens"))
        .unwrap_or(0);
    Usage {
        input: n("input_tokens"),
        cache_read: n("cache_read_input_tokens"),
        cache_write: n("cache_creation_input_tokens").max(one_hour),
        cache_1h: one_hour > 0,
        output: n("output_tokens"),
        reasoning: u
            .get("output_tokens_details")
            .and_then(|d| u64_of(d, "thinking_tokens"))
            .unwrap_or(0),
    }
}

pub fn usage_json(u: &Usage) -> Value {
    let mut v = json!({
        "input_tokens": u.input,
        "output_tokens": u.output,
        "cache_read_input_tokens": u.cache_read,
        "cache_creation_input_tokens": u.cache_write,
    });
    if u.cache_1h {
        v["cache_creation"] = json!({
            "ephemeral_5m_input_tokens": 0,
            "ephemeral_1h_input_tokens": u.cache_write,
        });
    }
    v
}

/// 上游 Anthropic 的整包响应 → 中间表示。
pub fn decode_response(v: &Value) -> Response {
    let blocks = arr_of(v, "content")
        .iter()
        .filter_map(|b| match str_of(b, "type")? {
            "text" => Some(Block::Text(str_of(b, "text")?.to_string())),
            "thinking" => Some(Block::Thinking(Thinking {
                text: str_of(b, "thinking").unwrap_or_default().to_string(),
                signature: str_of(b, "signature")
                    .and_then(|s| Signature::read(s, Vendor::Anthropic)),
            })),
            "redacted_thinking" => Some(Block::Thinking(Thinking {
                text: String::new(),
                signature: Some(Signature {
                    vendor: Vendor::Anthropic,
                    value: str_of(b, "data").unwrap_or_default().to_string(),
                    redacted: true,
                }),
            })),
            "tool_use" => Some(Block::ToolCall(ToolCall {
                id: str_of(b, "id").unwrap_or_default().to_string(),
                name: str_of(b, "name").unwrap_or_default().to_string(),
                input: ToolInput::Json(b.get("input").cloned().unwrap_or_else(|| json!({}))),
            })),
            // 服务端工具的调用和结果：执行已经在 Anthropic 那边完成，结论在文字里
            _ => None,
        })
        .collect();
    Response {
        id: str_of(v, "id").map(str::to_string),
        model: str_of(v, "model").map(str::to_string),
        blocks,
        stop: str_of(v, "stop_reason").map(|s| match stop_reason(s) {
            StopReason::StopSequence(_) => {
                StopReason::StopSequence(str_of(v, "stop_sequence").map(str::to_string))
            }
            other => other,
        }),
        usage: v.get("usage").map(usage),
    }
}

/// 中间表示 → 给 Anthropic 客户端的整包响应。
pub fn encode_response(r: &Response, s: &Session) -> Value {
    let content: Vec<Value> = r
        .blocks
        .iter()
        .map(|b| match b {
            Block::Text(t) => json!({ "type": "text", "text": t }),
            Block::Thinking(th) => thinking_block(th),
            Block::ToolCall(c) => json!({
                "type": "tool_use",
                "id": c.id,
                "name": c.name,
                "input": c.input.to_object(),
            }),
        })
        .collect();
    let stop_sequence = match &r.stop {
        Some(StopReason::StopSequence(Some(seq))) => json!(seq),
        _ => Value::Null,
    };
    json!({
        "id": message_id(r.id.as_deref()),
        "type": "message",
        "role": "assistant",
        "model": r.model.as_deref().unwrap_or(&s.model),
        "content": content,
        "stop_reason": r.stop.as_ref().map(stop_reason_str).unwrap_or(Value::Null),
        "stop_sequence": stop_sequence,
        "usage": usage_json(&r.usage.unwrap_or_default()),
    })
}

pub(crate) fn thinking_block(th: &Thinking) -> Value {
    json!({
        "type": "thinking",
        "thinking": th.text,
        "signature": th
            .signature
            .as_ref()
            .map(|s| s.carried_in(Vendor::Anthropic))
            .unwrap_or_else(unsigned_marker),
    })
}

/// 上游的 id 不是 `msg_` 开头就换一个：有的客户端按前缀认
pub(crate) fn message_id(upstream: Option<&str>) -> String {
    match upstream {
        Some(id) if id.starts_with("msg_") => id.to_string(),
        _ => new_id("msg_"),
    }
}

/// 给 Anthropic 客户端的错误体。
pub fn error_body(status: u16, message: &str) -> Value {
    let kind = match status {
        400 | 422 => "invalid_request_error",
        401 => "authentication_error",
        402 => "billing_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        504 => "timeout_error",
        529 | 503 => "overloaded_error",
        _ => "api_error",
    };
    json!({ "type": "error", "error": { "type": kind, "message": message } })
}

/// 从 Anthropic 的错误体里取说明
pub fn error_message(v: &Value) -> Option<String> {
    v.get("error")
        .and_then(|e| str_of(e, "message"))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_one_hour_cache_tier_survives_a_round_trip() {
        let u = usage(
            &json!({"input_tokens": 10, "cache_creation_input_tokens": 2000,
            "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 2000}}),
        );
        assert!(u.cache_1h);
        assert_eq!(u.cache_write, 2000);
        assert_eq!(usage(&usage_json(&u)), u);
        // 5 分钟档不算
        let u = usage(&json!({"cache_creation_input_tokens": 2000,
            "cache_creation": {"ephemeral_5m_input_tokens": 2000, "ephemeral_1h_input_tokens": 0}}));
        assert!(!u.cache_1h);
        assert!(usage_json(&u).get("cache_creation").is_none());
    }
    use crate::convert::Session;

    #[test]
    fn a_response_with_thinking_and_a_tool_call_decodes_in_order() {
        let r = decode_response(&json!({
            "id": "msg_1", "model": "claude-opus-4-7",
            "content": [
                {"type": "thinking", "thinking": "想一下", "signature": "sig"},
                {"type": "text", "text": "我读一下文件。"},
                {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"path": "a"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 20, "cache_read_input_tokens": 300,
                      "output_tokens_details": {"thinking_tokens": 12}}
        }));
        assert_eq!(r.blocks.len(), 3);
        assert_eq!(r.stop, Some(StopReason::ToolUse));
        let u = r.usage.unwrap();
        assert_eq!(
            (u.input, u.cache_read, u.output, u.reasoning),
            (10, 300, 20, 12)
        );
    }

    #[test]
    fn foreign_thinking_goes_out_with_a_carried_signature() {
        let r = Response {
            blocks: vec![
                Block::Thinking(Thinking {
                    text: "summary".into(),
                    signature: Some(Signature::new(Vendor::OpenAi, "rs_1:enc")),
                }),
                Block::Thinking(Thinking {
                    text: "deepseek".into(),
                    signature: None,
                }),
            ],
            stop: Some(StopReason::Other("weird".into())),
            ..Default::default()
        };
        let v = encode_response(
            &r,
            &Session::for_test(Dialect::Anthropic, Dialect::Responses),
        );
        assert_eq!(v["content"][0]["signature"], "tw1.o.rs_1:enc");
        assert_eq!(v["content"][1]["signature"], "tw1.n.");
        assert_eq!(v["stop_reason"], Value::Null);
        assert!(v["id"].as_str().unwrap().starts_with("msg_"));
        assert_eq!(v["model"], "m");
    }

    #[test]
    fn the_stop_sequence_that_matched_is_kept() {
        let r = decode_response(
            &json!({"content": [], "stop_reason": "stop_sequence", "stop_sequence": "END"}),
        );
        assert_eq!(r.stop, Some(StopReason::StopSequence(Some("END".into()))));
        let v = encode_response(
            &r,
            &Session::for_test(Dialect::Anthropic, Dialect::Anthropic),
        );
        assert_eq!(v["stop_sequence"], "END");
    }
}
