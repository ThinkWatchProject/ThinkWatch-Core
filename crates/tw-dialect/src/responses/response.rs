//! OpenAI Responses 整包响应 ⇄ 中间表示。

use serde_json::{Value, json};

use crate::convert::Session;
use crate::ir::*;

/// Responses 的 `input_tokens` **包含**缓存读写，中间表示的 `input` 不含
pub fn usage(u: &Value) -> Usage {
    let n = |k: &str| u64_of(u, k).unwrap_or(0);
    let details = u.get("input_tokens_details").unwrap_or(&Value::Null);
    let cache_read = u64_of(details, "cached_tokens").unwrap_or(0);
    let cache_write = u64_of(details, "cache_write_tokens").unwrap_or(0);
    Usage {
        input: n("input_tokens").saturating_sub(cache_read + cache_write),
        cache_read,
        cache_write,
        cache_1h: false,
        output: n("output_tokens"),
        reasoning: u
            .get("output_tokens_details")
            .and_then(|d| u64_of(d, "reasoning_tokens"))
            .unwrap_or(0),
    }
}

pub fn usage_json(u: &Usage) -> Value {
    json!({
        "input_tokens": u.prompt_total(),
        "input_tokens_details": { "cached_tokens": u.cache_read, "cache_write_tokens": u.cache_write },
        "output_tokens": u.output,
        "output_tokens_details": { "reasoning_tokens": u.reasoning },
        "total_tokens": u.prompt_total() + u.output,
    })
}

/// 未完成的原因
pub fn incomplete(reason: Option<&str>) -> StopReason {
    match reason {
        Some("max_output_tokens") => StopReason::MaxTokens,
        Some("content_filter") => StopReason::ContentFilter,
        Some(other) => StopReason::Other(other.to_string()),
        None => StopReason::Other("incomplete".into()),
    }
}

/// 中间表示的结束原因 → Responses 的状态和未完成原因
pub fn status(stop: Option<&StopReason>) -> (&'static str, Value) {
    match stop {
        Some(StopReason::MaxTokens | StopReason::ContextWindow) => {
            ("incomplete", json!({ "reason": "max_output_tokens" }))
        }
        Some(StopReason::ContentFilter | StopReason::Refusal) => {
            ("incomplete", json!({ "reason": "content_filter" }))
        }
        _ => ("completed", Value::Null),
    }
}

/// 推理项的签名。OpenAI 自己的带着项 id
pub(crate) fn reasoning_signature(item: &Value) -> Option<Signature> {
    let enc = str_of(item, "encrypted_content").filter(|e| !e.is_empty())?;
    if enc.starts_with(CARRIED) {
        return Signature::read(enc, Vendor::OpenAi);
    }
    let id = str_of(item, "id").unwrap_or_default();
    Some(Signature::new(Vendor::OpenAi, format!("{id}:{enc}")))
}

pub(crate) fn reasoning_text(item: &Value) -> String {
    let texts = |key: &str| {
        arr_of(item, key)
            .iter()
            .filter_map(|x| str_of(x, "text"))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    match texts("content") {
        t if t.is_empty() => texts("summary"),
        t => t,
    }
}

/// 上游 Responses 的整包响应 → 中间表示。
pub fn decode_response(v: &Value) -> Response {
    let mut blocks = Vec::new();
    for item in arr_of(v, "output") {
        match str_of(item, "type") {
            Some("message") => {
                for c in arr_of(item, "content") {
                    if let Some(t) = str_of(c, "text").or_else(|| str_of(c, "refusal")) {
                        blocks.push(Block::Text(t.to_string()));
                    }
                }
            }
            Some("reasoning") => blocks.push(Block::Thinking(Thinking {
                text: reasoning_text(item),
                signature: reasoning_signature(item),
            })),
            Some("function_call") => blocks.push(Block::ToolCall(ToolCall {
                id: str_of(item, "call_id").unwrap_or_default().to_string(),
                name: str_of(item, "name").unwrap_or_default().to_string(),
                input: ToolInput::from_json_text(str_of(item, "arguments").unwrap_or_default()),
            })),
            Some("custom_tool_call") => blocks.push(Block::ToolCall(ToolCall {
                id: str_of(item, "call_id").unwrap_or_default().to_string(),
                name: str_of(item, "name").unwrap_or_default().to_string(),
                input: ToolInput::Text(str_of(item, "input").unwrap_or_default().to_string()),
            })),
            _ => {}
        }
    }
    let tool_call = blocks.iter().any(|b| matches!(b, Block::ToolCall(_)));
    let stop = match str_of(v, "status") {
        Some("incomplete") => Some(incomplete(
            v.get("incomplete_details")
                .and_then(|d| str_of(d, "reason")),
        )),
        _ if tool_call => Some(StopReason::ToolUse),
        Some("completed") => Some(StopReason::EndTurn),
        _ => None,
    };
    Response {
        id: str_of(v, "id").map(str::to_string),
        model: str_of(v, "model").map(str::to_string),
        blocks,
        stop,
        usage: v.get("usage").filter(|u| !u.is_null()).map(usage),
    }
}

/// 一个块写成一个输出项。`done`：写完整的项；否则写刚开始的空项
pub(crate) fn item(b: &Block, id: &str, done: bool, s: &Session) -> Value {
    let status = if done { "completed" } else { "in_progress" };
    match b {
        Block::Text(t) => json!({
            "id": id,
            "type": "message",
            "status": status,
            "role": "assistant",
            "content": if done {
                json!([{ "type": "output_text", "text": t, "annotations": [] }])
            } else {
                json!([])
            },
        }),
        Block::Thinking(th) => {
            let summary = if th.text.is_empty() || !done {
                json!([])
            } else {
                json!([{ "type": "summary_text", "text": th.text }])
            };
            let mut o = json!({ "id": id, "type": "reasoning", "summary": summary });
            if done {
                o["encrypted_content"] = json!(match &th.signature {
                    Some(sig) if sig.vendor == Vendor::OpenAi && !sig.redacted => sig
                        .value
                        .split_once(':')
                        .map(|(_, enc)| enc.to_string())
                        .unwrap_or_else(|| sig.value.clone()),
                    Some(sig) => sig.carried_in(Vendor::OpenAi),
                    None => unsigned_marker(),
                });
            }
            o
        }
        Block::ToolCall(c) => {
            let (name, namespace) = match s.namespaced(&c.name) {
                Some((ns, n)) => (n.as_str(), Some(ns.as_str())),
                None => (c.name.as_str(), None),
            };
            let mut o = match &c.input {
                ToolInput::Text(t) if s.is_freeform(&c.name) => json!({
                    "id": id,
                    "type": "custom_tool_call",
                    "call_id": c.id,
                    "name": name,
                    "input": if done { t.as_str() } else { "" },
                }),
                input => json!({
                    "id": id,
                    "type": "function_call",
                    "call_id": c.id,
                    "name": name,
                    "arguments": if done { input.to_json_text() } else { String::new() },
                    "status": status,
                }),
            };
            if let Some(ns) = namespace {
                o["namespace"] = json!(ns);
            }
            o
        }
    }
}

pub(crate) fn item_id(b: &Block, s: &Session) -> String {
    new_id(match b {
        Block::Text(_) => "msg_",
        Block::Thinking(_) => "rs_",
        Block::ToolCall(c) if s.is_freeform(&c.name) => "ctc_",
        Block::ToolCall(_) => "fc_",
    })
}

/// 响应对象的外壳
pub(crate) fn envelope(id: &str, created: u64, model: &str, status: &str) -> Value {
    json!({
        "id": id,
        "object": "response",
        "created_at": created,
        "status": status,
        "error": null,
        "incomplete_details": null,
        "instructions": null,
        "model": model,
        "output": [],
        "parallel_tool_calls": true,
        "store": false,
        "tool_choice": "auto",
        "tools": [],
        "usage": null,
    })
}

pub(crate) fn response_id(upstream: Option<&str>) -> String {
    match upstream {
        Some(id) if id.starts_with("resp_") => id.to_string(),
        _ => new_id("resp_"),
    }
}

/// 中间表示 → 给 Responses 客户端的整包响应。
pub fn encode_response(r: &Response, s: &Session) -> Value {
    let (state, details) = status(r.stop.as_ref());
    let mut out = envelope(
        &response_id(r.id.as_deref()),
        unix_secs(),
        r.model.as_deref().unwrap_or(&s.model),
        state,
    );
    out["output"] = Value::Array(
        r.blocks
            .iter()
            .map(|b| item(b, &item_id(b, s), true, s))
            .collect(),
    );
    out["incomplete_details"] = details;
    if let Some(u) = &r.usage {
        out["usage"] = usage_json(u);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_codex_backend_response_decodes_reasoning_and_calls() {
        let r = decode_response(&json!({
            "id": "resp_1", "model": "gpt-5.1-codex", "status": "completed",
            "output": [
                {"type": "reasoning", "id": "rs_9", "summary": [{"type": "summary_text", "text": "看"}], "encrypted_content": "gAAA"},
                {"type": "message", "id": "msg_1", "role": "assistant", "content": [{"type": "output_text", "text": "改好了"}]},
                {"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_9", "name": "apply_patch", "input": "*** Begin"}
            ],
            "usage": {"input_tokens": 5000, "input_tokens_details": {"cached_tokens": 4000, "cache_write_tokens": 0},
                      "output_tokens": 300, "output_tokens_details": {"reasoning_tokens": 200}, "total_tokens": 5300}
        }));
        assert_eq!(r.blocks.len(), 3);
        assert!(
            matches!(&r.blocks[0], Block::Thinking(t) if t.signature == Some(Signature::new(Vendor::OpenAi, "rs_9:gAAA")))
        );
        assert_eq!(r.stop, Some(StopReason::ToolUse));
        let u = r.usage.unwrap();
        assert_eq!(
            (u.input, u.cache_read, u.output, u.reasoning),
            (1000, 4000, 300, 200)
        );
    }

    #[test]
    fn running_out_of_tokens_is_incomplete() {
        let r = decode_response(
            &json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, "output": []}),
        );
        assert_eq!(r.stop, Some(StopReason::MaxTokens));
        let v = encode_response(
            &r,
            &Session::for_test(Dialect::Responses, Dialect::Anthropic),
        );
        assert_eq!(v["status"], "incomplete");
        assert_eq!(v["incomplete_details"]["reason"], "max_output_tokens");
    }

    #[test]
    fn items_for_a_codex_client_use_its_own_tool_kinds_and_namespaces() {
        let mut s = Session::for_test(Dialect::Responses, Dialect::Anthropic);
        s.freeform.insert("apply_patch".into());
        s.shape
            .namespaced
            .insert("mcp_fs__read".into(), ("mcp_fs".into(), "read".into()));
        let r = Response {
            blocks: vec![
                Block::Thinking(Thinking {
                    text: "想".into(),
                    signature: Some(Signature::new(Vendor::Anthropic, "sig")),
                }),
                Block::ToolCall(ToolCall {
                    id: "toolu_1".into(),
                    name: "apply_patch".into(),
                    input: ToolInput::Text("*** Begin".into()),
                }),
                Block::ToolCall(ToolCall {
                    id: "toolu_2".into(),
                    name: "mcp_fs__read".into(),
                    input: ToolInput::Json(json!({"path": "a"})),
                }),
            ],
            stop: Some(StopReason::ToolUse),
            usage: Some(Usage {
                input: 10,
                cache_read: 90,
                output: 5,
                ..Default::default()
            }),
            ..Default::default()
        };
        let v = encode_response(&r, &s);
        let out = v["output"].as_array().unwrap();
        assert_eq!(out[0]["encrypted_content"], "tw1.a.sig");
        assert_eq!(out[0]["summary"][0]["text"], "想");
        assert_eq!(out[1]["type"], "custom_tool_call");
        assert_eq!(out[1]["input"], "*** Begin");
        assert_eq!(out[2]["name"], "read");
        assert_eq!(out[2]["namespace"], "mcp_fs");
        assert_eq!(out[2]["arguments"], "{\"path\":\"a\"}");
        assert_eq!(v["status"], "completed");
        assert_eq!(v["usage"]["input_tokens"], 100);
        assert_eq!(v["usage"]["input_tokens_details"]["cached_tokens"], 90);
    }
}
