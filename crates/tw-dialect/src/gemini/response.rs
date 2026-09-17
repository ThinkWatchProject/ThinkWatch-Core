//! Gemini generateContent 整包响应 ⇄ 中间表示。

use serde_json::{Value, json};

use super::request::field;
use crate::convert::Session;
use crate::ir::*;

pub fn stop_reason(s: &str, tool_call: bool) -> StopReason {
    match s {
        "STOP" if tool_call => StopReason::ToolUse,
        "STOP" => StopReason::EndTurn,
        "MAX_TOKENS" => StopReason::MaxTokens,
        "SAFETY"
        | "RECITATION"
        | "BLOCKLIST"
        | "PROHIBITED_CONTENT"
        | "SPII"
        | "IMAGE_SAFETY"
        | "IMAGE_PROHIBITED_CONTENT"
        | "IMAGE_RECITATION" => StopReason::ContentFilter,
        other => StopReason::Other(other.to_string()),
    }
}

pub fn finish_reason(s: &StopReason) -> &'static str {
    match s {
        StopReason::MaxTokens | StopReason::ContextWindow => "MAX_TOKENS",
        StopReason::ContentFilter | StopReason::Refusal => "SAFETY",
        StopReason::Other(_) => "OTHER",
        _ => "STOP",
    }
}

/// Gemini 的 `promptTokenCount` **包含**缓存命中，`candidatesTokenCount` **不含**思考
pub fn usage(u: &Value) -> Usage {
    let n = |k: &str| field(u, k).and_then(Value::as_u64).unwrap_or(0);
    let cached = n("cachedContentTokenCount");
    let thoughts = n("thoughtsTokenCount");
    Usage {
        input: (n("promptTokenCount") + n("toolUsePromptTokenCount")).saturating_sub(cached),
        cache_read: cached,
        cache_write: 0,
        output: n("candidatesTokenCount") + thoughts,
        reasoning: thoughts,
    }
}

pub fn usage_json(u: &Usage) -> Value {
    let mut v = json!({
        "promptTokenCount": u.prompt_total(),
        "candidatesTokenCount": u.output.saturating_sub(u.reasoning),
        "totalTokenCount": u.prompt_total() + u.output,
    });
    if u.cache_read > 0 {
        v["cachedContentTokenCount"] = json!(u.cache_read);
    }
    if u.reasoning > 0 {
        v["thoughtsTokenCount"] = json!(u.reasoning);
    }
    v
}

/// 上游 Gemini 的整包响应 → 中间表示。
pub fn decode_response(v: &Value) -> Response {
    let candidate = v
        .get("candidates")
        .and_then(|c| c.get(0))
        .unwrap_or(&Value::Null);
    let mut blocks = Vec::new();
    for p in candidate
        .get("content")
        .map(|c| arr_of(c, "parts"))
        .unwrap_or(&[])
    {
        if let Some(t) = field(p, "text").and_then(Value::as_str) {
            if p.get("thought").and_then(Value::as_bool) == Some(true) {
                blocks.push(Block::Thinking(Thinking {
                    text: t.to_string(),
                    signature: field(p, "thoughtSignature")
                        .and_then(Value::as_str)
                        .and_then(|s| Signature::read(s, Vendor::Google)),
                }));
            } else if !t.is_empty() {
                blocks.push(Block::Text(t.to_string()));
            }
        } else if let Some(call) = field(p, "functionCall") {
            blocks.push(Block::ToolCall(ToolCall {
                id: field(call, "id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| new_id("call_")),
                name: str_of(call, "name").unwrap_or_default().to_string(),
                input: ToolInput::Json(call.get("args").cloned().unwrap_or_else(|| json!({}))),
            }));
        }
    }
    let tool_call = blocks.iter().any(|b| matches!(b, Block::ToolCall(_)));
    let stop = match field(candidate, "finishReason").and_then(Value::as_str) {
        Some(s) => Some(stop_reason(s, tool_call)),
        // 整个提示被拦下时没有候选，原因在 promptFeedback
        None if field(v, "promptFeedback")
            .and_then(|f| field(f, "blockReason"))
            .is_some() =>
        {
            Some(StopReason::ContentFilter)
        }
        None => None,
    };
    Response {
        id: field(v, "responseId")
            .and_then(Value::as_str)
            .map(str::to_string),
        model: field(v, "modelVersion")
            .and_then(Value::as_str)
            .map(str::to_string),
        blocks,
        stop,
        usage: field(v, "usageMetadata").map(usage),
    }
}

pub(crate) fn part(b: &Block) -> Option<Value> {
    Some(match b {
        Block::Text(t) => json!({ "text": t }),
        Block::Thinking(th) => {
            if th.text.is_empty() && th.signature.is_none() {
                return None;
            }
            let mut p = json!({ "text": th.text, "thought": true });
            if let Some(s) = &th.signature {
                p["thoughtSignature"] = json!(s.carried_in(Vendor::Google));
            }
            p
        }
        Block::ToolCall(c) => json!({
            "functionCall": { "id": c.id, "name": c.name, "args": c.input.to_object() },
        }),
    })
}

/// 中间表示 → 给 Gemini 客户端的整包响应。
pub fn encode_response(r: &Response, s: &Session) -> Value {
    let parts: Vec<Value> = r.blocks.iter().filter_map(part).collect();
    let mut out = json!({
        "candidates": [{
            "content": { "role": "model", "parts": parts },
            "finishReason": finish_reason(r.stop.as_ref().unwrap_or(&StopReason::EndTurn)),
            "index": 0,
        }],
        "modelVersion": r.model.as_deref().unwrap_or(&s.model),
        "responseId": r.id.clone().unwrap_or_else(|| new_id("")),
    });
    if let Some(u) = &r.usage {
        out["usageMetadata"] = usage_json(u);
    }
    out
}

/// 给 Gemini 客户端的错误体。
pub fn error_body(status: u16, message: &str) -> Value {
    let state = match status {
        400 | 422 => "INVALID_ARGUMENT",
        401 => "UNAUTHENTICATED",
        403 => "PERMISSION_DENIED",
        404 => "NOT_FOUND",
        429 => "RESOURCE_EXHAUSTED",
        503 | 529 => "UNAVAILABLE",
        504 => "DEADLINE_EXCEEDED",
        _ => "INTERNAL",
    };
    json!({ "error": { "code": status, "message": message, "status": state } })
}

/// 从 Gemini 的错误体里取说明。Gemini 的错误有时是一个只含一项的数组
pub fn error_message(v: &Value) -> Option<String> {
    let v = v.get(0).unwrap_or(v);
    v.get("error")
        .and_then(|e| str_of(e, "message"))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thoughts_are_output_and_cached_tokens_are_input() {
        let u = usage(&json!({
            "promptTokenCount": 1000, "cachedContentTokenCount": 600,
            "candidatesTokenCount": 50, "thoughtsTokenCount": 200, "toolUsePromptTokenCount": 10
        }));
        assert_eq!(
            (u.input, u.cache_read, u.output, u.reasoning),
            (410, 600, 250, 200)
        );
        let back = usage_json(&u);
        assert_eq!(back["promptTokenCount"], 1010);
        assert_eq!(back["candidatesTokenCount"], 50);
    }

    #[test]
    fn a_stop_with_a_function_call_is_a_tool_use() {
        let r = decode_response(&json!({
            "responseId": "r1", "modelVersion": "gemini-2.5-pro",
            "candidates": [{"content": {"role": "model", "parts": [
                {"text": "想", "thought": true},
                {"functionCall": {"name": "ls", "args": {"p": "."}}, "thoughtSignature": "CiQB"}
            ]}, "finishReason": "STOP"}]
        }));
        assert_eq!(r.stop, Some(StopReason::ToolUse));
        assert!(matches!(&r.blocks[1], Block::ToolCall(c) if c.id.starts_with("call_")));
    }

    #[test]
    fn a_blocked_prompt_is_a_content_filter_stop() {
        let r = decode_response(&json!({"promptFeedback": {"blockReason": "SAFETY"}}));
        assert_eq!(r.stop, Some(StopReason::ContentFilter));
        assert!(r.blocks.is_empty());
    }

    #[test]
    fn a_response_for_a_gemini_client_has_camel_case_usage() {
        let r = Response {
            blocks: vec![Block::Text("hi".into())],
            stop: Some(StopReason::MaxTokens),
            usage: Some(Usage {
                input: 5,
                output: 3,
                ..Default::default()
            }),
            ..Default::default()
        };
        let v = encode_response(&r, &Session::for_test(Dialect::Gemini, Dialect::Chat));
        assert_eq!(v["candidates"][0]["finishReason"], "MAX_TOKENS");
        assert_eq!(v["candidates"][0]["content"]["parts"][0]["text"], "hi");
        assert_eq!(v["usageMetadata"]["totalTokenCount"], 8);
    }
}
