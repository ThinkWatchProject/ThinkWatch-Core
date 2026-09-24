//! OpenAI Chat Completions 整包响应 ⇄ 中间表示。

use serde_json::{Value, json};

use crate::convert::Session;
use crate::ir::*;

pub fn stop_reason(s: &str) -> StopReason {
    match s {
        "stop" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "content_filter" => StopReason::ContentFilter,
        other => StopReason::Other(other.to_string()),
    }
}

/// `tool_call` 表示这次响应里有工具调用：Gemini 和 Responses 的结束原因不单独说这一点
pub fn finish_reason(s: &StopReason, tool_call: bool) -> &'static str {
    match s {
        StopReason::MaxTokens | StopReason::ContextWindow => "length",
        StopReason::ContentFilter | StopReason::Refusal => "content_filter",
        _ if tool_call => "tool_calls",
        StopReason::ToolUse => "tool_calls",
        _ => "stop",
    }
}

/// OpenAI 的 `prompt_tokens` **包含**缓存读写，中间表示的 `input` 不含
pub fn usage(u: &Value) -> Usage {
    let n = |k: &str| u64_of(u, k).unwrap_or(0);
    let details = u.get("prompt_tokens_details").unwrap_or(&Value::Null);
    let cache_read = u64_of(details, "cached_tokens").unwrap_or(0);
    let cache_write = u64_of(details, "cache_write_tokens").unwrap_or(0);
    Usage {
        input: n("prompt_tokens").saturating_sub(cache_read + cache_write),
        cache_read,
        cache_write,
        cache_1h: false,
        output: n("completion_tokens"),
        reasoning: u
            .get("completion_tokens_details")
            .and_then(|d| u64_of(d, "reasoning_tokens"))
            .unwrap_or(0),
    }
}

pub fn usage_json(u: &Usage) -> Value {
    json!({
        "prompt_tokens": u.prompt_total(),
        "completion_tokens": u.output,
        "total_tokens": u.prompt_total() + u.output,
        "prompt_tokens_details": { "cached_tokens": u.cache_read },
        "completion_tokens_details": { "reasoning_tokens": u.reasoning },
    })
}

/// 上游 Chat 的整包响应 → 中间表示。
pub fn decode_response(v: &Value) -> Response {
    let choice = v
        .get("choices")
        .and_then(|c| c.get(0))
        .unwrap_or(&Value::Null);
    let msg = choice.get("message").unwrap_or(&Value::Null);
    let mut blocks = Vec::new();
    if let Some(t) = str_of(msg, "reasoning_content").filter(|t| !t.is_empty()) {
        blocks.push(Block::Thinking(Thinking {
            text: t.to_string(),
            signature: None,
        }));
    }
    for key in ["content", "refusal"] {
        if let Some(t) = str_of(msg, key).filter(|t| !t.is_empty()) {
            blocks.push(Block::Text(t.to_string()));
        }
    }
    for c in arr_of(msg, "tool_calls") {
        let id = str_of(c, "id").unwrap_or_default().to_string();
        if let Some(x) = c.get("custom") {
            blocks.push(Block::ToolCall(ToolCall {
                id,
                name: str_of(x, "name").unwrap_or_default().to_string(),
                input: ToolInput::Text(str_of(x, "input").unwrap_or_default().to_string()),
            }));
        } else if let Some(f) = c.get("function") {
            blocks.push(Block::ToolCall(ToolCall {
                id,
                name: str_of(f, "name").unwrap_or_default().to_string(),
                input: ToolInput::from_json_text(str_of(f, "arguments").unwrap_or_default()),
            }));
        }
    }
    Response {
        id: str_of(v, "id").map(str::to_string),
        model: str_of(v, "model").map(str::to_string),
        blocks,
        stop: str_of(choice, "finish_reason").map(stop_reason),
        usage: v.get("usage").filter(|u| !u.is_null()).map(usage),
    }
}

/// 中间表示 → 给 Chat 客户端的整包响应。
pub fn encode_response(r: &Response, s: &Session) -> Value {
    let mut texts = Vec::new();
    let mut thinking = Vec::new();
    let mut calls = Vec::new();
    for b in &r.blocks {
        match b {
            Block::Text(t) => texts.push(t.as_str()),
            Block::Thinking(th) if !th.text.is_empty() => thinking.push(th.text.as_str()),
            Block::Thinking(_) => {}
            Block::ToolCall(c) => calls.push(tool_call(c, s)),
        }
    }
    let mut message = json!({
        "role": "assistant",
        "content": if texts.is_empty() { Value::Null } else { json!(texts.concat()) },
        "refusal": null,
    });
    if !thinking.is_empty() {
        message["reasoning_content"] = json!(thinking.join("\n\n"));
    }
    let has_calls = !calls.is_empty();
    if has_calls {
        message["tool_calls"] = Value::Array(calls);
    }
    let mut out = json!({
        "id": completion_id(r.id.as_deref()),
        "object": "chat.completion",
        "created": unix_secs(),
        "model": r.model.as_deref().unwrap_or(&s.model),
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason(r.stop.as_ref().unwrap_or(&StopReason::EndTurn), has_calls),
            "logprobs": null,
        }],
    });
    if let Some(u) = &r.usage {
        out["usage"] = usage_json(u);
    }
    out
}

fn tool_call(c: &ToolCall, s: &Session) -> Value {
    match &c.input {
        ToolInput::Text(t) if s.is_freeform(&c.name) => json!({
            "id": c.id,
            "type": "custom",
            "custom": { "name": c.name, "input": t },
        }),
        input => json!({
            "id": c.id,
            "type": "function",
            "function": { "name": c.name, "arguments": input.to_json_text() },
        }),
    }
}

pub(crate) fn completion_id(upstream: Option<&str>) -> String {
    match upstream {
        Some(id) if id.starts_with("chatcmpl-") => id.to_string(),
        _ => new_id("chatcmpl-"),
    }
}

/// 给 Chat 客户端的错误体。
pub fn error_body(status: u16, message: &str) -> Value {
    let kind = match status {
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        400..=499 => "invalid_request_error",
        _ => "server_error",
    };
    json!({ "error": { "message": message, "type": kind, "param": null, "code": null } })
}

/// 从 OpenAI 的错误体里取说明
pub fn error_message(v: &Value) -> Option<String> {
    v.get("error").and_then(|e| match e {
        Value::String(s) => Some(s.clone()),
        e => str_of(e, "message").map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_tokens_are_taken_out_of_the_prompt_count() {
        // 不减的话，缓存命中的部分会被按输入价再算一遍
        let u = usage(&json!({
            "prompt_tokens": 1000, "completion_tokens": 50,
            "prompt_tokens_details": {"cached_tokens": 800},
            "completion_tokens_details": {"reasoning_tokens": 30}
        }));
        assert_eq!(
            (u.input, u.cache_read, u.output, u.reasoning),
            (200, 800, 50, 30)
        );
        assert_eq!(usage_json(&u)["prompt_tokens"], 1000);
    }

    #[test]
    fn a_deepseek_response_keeps_its_reasoning() {
        let r = decode_response(&json!({
            "id": "x", "model": "deepseek-reasoner",
            "choices": [{"message": {"content": "答案", "reasoning_content": "推理",
                "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "f", "arguments": ""}}]},
                "finish_reason": "tool_calls"}]
        }));
        assert!(matches!(&r.blocks[0], Block::Thinking(t) if t.text == "推理"));
        assert!(
            matches!(&r.blocks[2], Block::ToolCall(c) if c.input == ToolInput::Json(json!({})))
        );
        assert_eq!(r.stop, Some(StopReason::ToolUse));
        assert!(r.usage.is_none());
    }

    #[test]
    fn a_gemini_style_stop_with_tool_calls_says_tool_calls() {
        let r = Response {
            blocks: vec![Block::ToolCall(ToolCall {
                id: "c".into(),
                name: "f".into(),
                input: ToolInput::Json(json!({"a": 1})),
            })],
            stop: Some(StopReason::EndTurn),
            ..Default::default()
        };
        let v = encode_response(&r, &Session::for_test(Dialect::Chat, Dialect::Gemini));
        assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(v["choices"][0]["message"]["content"], Value::Null);
        assert_eq!(
            v["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"a\":1}"
        );
        assert!(v["id"].as_str().unwrap().starts_with("chatcmpl-"));
    }
}
