//! 回答在不在往前走：上游来的一段是真内容，还是心跳。
//!
//! 无响应超时（`failover.idle_timeout_secs`）只认这个：每来一段真内容重新计时，心跳不算。
//! 心跳也算的话，一家只发心跳、永远不出内容的上游会一直挂着，客户端跟着干等。流开头的等待
//! （`server::pipeline::opening`）用的是同一个判据：开头的例行事件和心跳之后，第一段真内容
//! 才算上游开口了。
//!
//! **心跳**（不算）：
//!
//! - SSE 的注释行（`: keep-alive`、`: OPENROUTER PROCESSING`）和空的 `data:`；
//! - 叫心跳的事件，哪种格式都一样：`ping`、`keepalive`、`heartbeat`；
//! - Anthropic：`message_start`（收到请求就发的开场）、`ping`；
//! - Responses：`response.created`、`response.in_progress`、`response.queued`，和 Codex 后端
//!   开头报额度的 `codex.*`；
//! - Chat Completions：只有角色、内容为空的块，`choices` 为空又没有用量的块；
//! - Gemini：候选里一段内容都没有、也没有结束原因的块（只带 `usageMetadata` 的那种）；
//! - Bedrock：`messageStart`。
//!
//! **真内容**（重新计时）：正文、推理（字和摘要）、工具调用（开头和参数）、拒答、结束原因、
//! 收尾的用量、`[DONE]`、上游在流里报的错，以及**认不出来的一切** —— 认错了往宽里错：把
//! 心跳当内容，最多是一家坏了的上游多挂一阵；把内容当心跳，会把一个好好在答的回答掐断。
//!
//! 不是流的回答（整包的 JSON）：有一个不是空白的字节就算 —— 有的中转站在整包生成完之前
//! 隔一阵发一个空格保活。

use tw_dialect::ir::Dialect;

/// 一个事件攒到这么大还没收齐，就当它是内容、不再攒：一个坏了的上游不能让这里无限长
const MAX_EVENT: usize = 1024 * 1024;

/// 哪种格式都算心跳的事件名
const HEARTBEATS: &[&str] = &["ping", "keepalive", "keep-alive", "keep_alive", "heartbeat"];

/// 一个解析好的事件（`ty` 是 `event:` 行写的名字，没有就是 JSON 里的 `type`）是不是让回答
/// 往前走了一步。上游报的错也算（流跟着就结束了）
pub(crate) fn advances(dialect: Dialect, ty: &str, v: &serde_json::Value) -> bool {
    if HEARTBEATS.contains(&ty) {
        return false;
    }
    match dialect {
        Dialect::Anthropic => ty != "message_start",
        Dialect::Responses => {
            !matches!(
                ty,
                "response.created" | "response.in_progress" | "response.queued"
            ) && !ty.starts_with("codex.")
        }
        Dialect::Chat => chat_advances(v),
        Dialect::Gemini => gemini_advances(v),
        Dialect::Bedrock => ty != "messageStart",
    }
}

/// Chat Completions 的一块：有错误、有内容、有结束原因，或者是收尾的那块用量
fn chat_advances(v: &serde_json::Value) -> bool {
    if v.get("error").is_some_and(|e| !e.is_null()) {
        return true;
    }
    let Some(choices) = v.get("choices").and_then(|c| c.as_array()) else {
        // 没有 `choices`：只有用量的收尾块算，别的认不出来，也算
        return true;
    };
    if choices.is_empty() {
        return v.get("usage").is_some_and(|u| !u.is_null());
    }
    choices.iter().any(|c| {
        let finished = c.get("finish_reason").is_some_and(|f| !f.is_null());
        let said = c.get("delta").is_some_and(|d| {
            [
                "content",
                "reasoning_content",
                "reasoning",
                "tool_calls",
                "function_call",
                "refusal",
                "audio",
            ]
            .iter()
            .any(|k| d.get(*k).is_some_and(filled))
        });
        finished || said
    })
}

/// Gemini 的一块：有错误、被拦下，或者某个候选有内容、有结束原因
fn gemini_advances(v: &serde_json::Value) -> bool {
    if v.get("error").is_some_and(|e| !e.is_null()) || v.get("promptFeedback").is_some() {
        return true;
    }
    let Some(candidates) = v.get("candidates").and_then(|c| c.as_array()) else {
        // 只带用量、模型版本的块不是内容；别的认不出来的算
        return !v.as_object().is_some_and(|o| {
            o.keys()
                .all(|k| matches!(k.as_str(), "usageMetadata" | "modelVersion" | "responseId"))
        });
    };
    candidates.iter().any(|c| {
        let finished = c.get("finishReason").is_some_and(|f| !f.is_null());
        let parts = c
            .pointer("/content/parts")
            .and_then(|p| p.as_array())
            .is_some_and(|parts| {
                parts.iter().any(|p| {
                    p.as_object().is_some_and(|o| {
                        o.iter().any(|(k, x)| k.as_str() != "thought" && filled(x))
                    })
                })
            });
        finished || parts
    })
}

/// 有东西：不是 null、空串、空数组、空对象
fn filled(x: &serde_json::Value) -> bool {
    match x {
        serde_json::Value::Null => false,
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
        _ => true,
    }
}

/// 一个 SSE 事件（`event:` 行的名字、拼好的 `data:`）是不是真内容。
pub(crate) fn event_advances(dialect: Dialect, event: Option<&str>, data: &str) -> bool {
    // 带名字的格式先看名字：不用为每一个 token 解析一遍 JSON
    if let Some(name) = event.map(str::trim).filter(|n| !n.is_empty()) {
        if HEARTBEATS.contains(&name) {
            return false;
        }
        if matches!(
            dialect,
            Dialect::Anthropic | Dialect::Responses | Dialect::Bedrock
        ) {
            return advances(dialect, name, &serde_json::Value::Null);
        }
    }
    let data = data.trim();
    if data.is_empty() {
        return false;
    }
    if data == "[DONE]" {
        return true;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
        return true;
    };
    let ty = event
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .or_else(|| v.get("type").and_then(|t| t.as_str()))
        .unwrap_or_default()
        .to_string();
    advances(dialect, &ty, &v)
}

/// 跟着一条回答，一块一块地看有没有真内容。
pub(crate) struct Pulse {
    /// 上游说的是 SSE 时是它的格式；整包的回答是 None
    dialect: Option<Dialect>,
    /// 还没收到换行的那半行
    line: Vec<u8>,
    /// 这个事件的名字，和攒着的 `data:`
    event: Option<String>,
    data: String,
}

impl Pulse {
    /// `dialect`：上游的回答是这种格式的 SSE（Bedrock 的二进制帧先转成 SSE 再喂）。整包的
    /// 回答给 None
    pub(crate) fn new(dialect: Option<Dialect>) -> Self {
        Self {
            dialect,
            line: Vec::new(),
            event: None,
            data: String::new(),
        }
    }

    /// 这一块里有没有真内容。**一块里有一个就够了**：后面的事件只拆不判，下一块从头再判
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> bool {
        let Some(dialect) = self.dialect else {
            return chunk.iter().any(|b| !b.is_ascii_whitespace());
        };
        let mut found = false;
        let mut rest = chunk;
        while let Some(nl) = memchr::memchr(b'\n', rest) {
            let (head, tail) = rest.split_at(nl);
            rest = &tail[1..];
            let line = if self.line.is_empty() {
                head
            } else {
                self.line.extend_from_slice(head);
                self.line.as_slice()
            };
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let ended = line.is_empty();
            if !ended {
                Self::field(&mut self.event, &mut self.data, line);
            }
            self.line.clear();
            if ended {
                let (event, data) = (self.event.take(), std::mem::take(&mut self.data));
                if !found && (event.is_some() || !data.is_empty()) {
                    found = event_advances(dialect, event.as_deref(), &data);
                }
            }
        }
        self.line.extend_from_slice(rest);
        if self.line.len() + self.data.len() > MAX_EVENT {
            self.line.clear();
            self.event = None;
            self.data.clear();
            return true;
        }
        found
    }

    /// 一行 SSE：`event:` 记名字，`data:` 攒起来，注释和别的字段不管
    fn field(event: &mut Option<String>, data: &mut String, line: &[u8]) {
        if let Some(v) = line.strip_prefix(b"event:") {
            *event = Some(String::from_utf8_lossy(v).trim().to_string());
        } else if let Some(v) = line.strip_prefix(b"data:") {
            let v = v.strip_prefix(b" ").unwrap_or(v);
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(&String::from_utf8_lossy(v));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一段 SSE 整个喂进去，有没有真内容
    fn sse(dialect: Dialect, text: &str) -> bool {
        Pulse::new(Some(dialect)).feed(text.as_bytes())
    }

    #[test]
    fn anthropic_pings_and_the_opening_are_not_content() {
        let d = Dialect::Anthropic;
        assert!(!sse(d, ": keep-alive\n\n"));
        assert!(!sse(d, "event: ping\ndata: {\"type\":\"ping\"}\n\n"));
        assert!(!sse(
            d,
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\n"
        ));
        // 不带 `event:` 行、只在 JSON 里写类型的也认得
        assert!(!sse(d, "data: {\"type\":\"ping\"}\n\n"));
        for text in [
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Hm\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n\n",
        ] {
            assert!(sse(d, text), "{text}");
        }
    }

    #[test]
    fn responses_progress_events_are_content_and_the_status_ones_are_not() {
        let d = Dialect::Responses;
        for quiet in [
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
            "event: response.in_progress\ndata: {\"type\":\"response.in_progress\"}\n\n",
            "event: response.queued\ndata: {\"type\":\"response.queued\"}\n\n",
            "event: codex.rate_limits\ndata: {\"type\":\"codex.rate_limits\"}\n\n",
            "data: {\"type\":\"keepalive\"}\n\n",
        ] {
            assert!(!sse(d, quiet), "{quiet}");
        }
        for said in [
            "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"reasoning\"}}\n\n",
            "event: response.reasoning_summary_text.delta\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"Thinking\"}\n\n",
            "event: response.function_call_arguments.delta\ndata: {\"delta\":\"{\"}\n\n",
            "event: response.output_text.delta\ndata: {\"delta\":\"Hi\"}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n",
            "event: response.failed\ndata: {\"type\":\"response.failed\"}\n\n",
        ] {
            assert!(sse(d, said), "{said}");
        }
    }

    #[test]
    fn chat_role_only_and_empty_chunks_are_not_content() {
        let d = Dialect::Chat;
        for quiet in [
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":null,\"tool_calls\":[]}}]}\n\n",
            "data: {\"choices\":[]}\n\n",
            "data: \n\n",
            ": OPENROUTER PROCESSING\n\n",
        ] {
            assert!(!sse(d, quiet), "{quiet}");
        }
        for said in [
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Let me think\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning\":\"Let me think\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3}}\n\n",
            "data: {\"error\":{\"message\":\"busy\"}}\n\n",
            "data: [DONE]\n\n",
            "data: not json\n\n",
        ] {
            assert!(sse(d, said), "{said}");
        }
    }

    #[test]
    fn gemini_chunks_without_parts_are_not_content() {
        let d = Dialect::Gemini;
        for quiet in [
            "data: {\"usageMetadata\":{\"promptTokenCount\":10},\"modelVersion\":\"gemini-3-pro\"}\r\n\r\n",
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"\"}]}}]}\r\n\r\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"\",\"thought\":true}]}}]}\r\n\r\n",
        ] {
            assert!(!sse(d, quiet), "{quiet}");
        }
        for said in [
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hi\"}]}}]}\r\n\r\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hm\",\"thought\":true}]}}]}\r\n\r\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"ls\"}}]}}]}\r\n\r\n",
            "data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\r\n\r\n",
            "data: {\"promptFeedback\":{\"blockReason\":\"SAFETY\"}}\r\n\r\n",
        ] {
            assert!(sse(d, said), "{said}");
        }
    }

    #[test]
    fn bedrock_message_start_is_not_content() {
        let d = Dialect::Bedrock;
        assert!(!sse(
            d,
            "event: messageStart\ndata: {\"role\":\"assistant\"}\n\n"
        ));
        assert!(sse(
            d,
            "event: contentBlockDelta\ndata: {\"delta\":{\"text\":\"Hi\"}}\n\n"
        ));
    }

    #[test]
    fn an_event_split_across_chunks_counts_when_it_is_complete() {
        let mut p = Pulse::new(Some(Dialect::Anthropic));
        assert!(!p.feed(b"event: content_block_del"));
        assert!(!p.feed(b"ta\r\ndata: {\"type\":\"content_block_delta\"}\r"));
        assert!(p.feed(b"\n\r\n"));
        // 一块里先是心跳、后是内容：算
        assert!(p.feed(b"event: ping\ndata: {}\n\nevent: content_block_delta\ndata: {}\n\n"));
        // 只有心跳：不算
        assert!(!p.feed(b"event: ping\ndata: {}\n\n: keep-alive\n\n"));
    }

    #[test]
    fn a_whole_body_counts_any_byte_that_is_not_whitespace() {
        let mut p = Pulse::new(None);
        assert!(!p.feed(b"  \n\t"));
        assert!(p.feed(b" {\"id\":"));
    }

    #[test]
    fn an_event_that_never_ends_is_taken_as_content_and_dropped() {
        let mut p = Pulse::new(Some(Dialect::Chat));
        let big = vec![b'x'; MAX_EVENT + 1];
        let mut first = b"data: ".to_vec();
        first.extend_from_slice(&big);
        assert!(p.feed(&first));
        assert!(!p.feed(b"\n\n"), "攒的已经扔掉了");
    }
}
