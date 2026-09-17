//! Anthropic Messages 的流 ⇄ 中间表示的事件。
//!
//! Anthropic 的流本来就是按块组织的（`content_block_start` / `_delta` / `_stop`），
//! 和中间表示几乎一一对应。写出时的难点在用量：输入 token 在开头的 `message_start`，
//! 而别家的用量在**结尾**才给。等到结尾再发 `message_start` 就成了整块缓冲，所以
//! `message_start` 先写 0，真数字在结尾的 `message_delta` 里给 —— Anthropic 自己的流
//! 也在 `message_delta` 里更新用量，客户端本来就看那一处。

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::response::{message_id, stop_reason, stop_reason_str, usage, usage_json};
use crate::convert::Session;
use crate::frame::{self, Frame};
use crate::ir::*;

// ───────────────────────────────────────────────────────── 解析

#[derive(Debug, Default)]
pub struct Parser {
    usage: Usage,
    /// 服务端工具的块：它们的增量和结束都不往下传
    skipped: HashSet<usize>,
}

impl Parser {
    pub fn frame(&mut self, f: &Frame, out: &mut Vec<Event>) {
        let Ok(v) = serde_json::from_str::<Value>(&f.data) else {
            return;
        };
        let kind = str_of(&v, "type").or(f.event.as_deref()).unwrap_or("");
        let index = u64_of(&v, "index").unwrap_or(0) as usize;
        match kind {
            "message_start" => {
                let m = v.get("message").unwrap_or(&Value::Null);
                out.push(Event::Start {
                    id: str_of(m, "id").map(str::to_string),
                    model: str_of(m, "model").map(str::to_string),
                });
                if let Some(u) = m.get("usage") {
                    self.usage.merge(&usage(u));
                    out.push(Event::Usage(self.usage));
                }
            }
            "content_block_start" => {
                let b = v.get("content_block").unwrap_or(&Value::Null);
                let kind = match str_of(b, "type") {
                    Some("text") => BlockKind::Text,
                    Some("thinking") | Some("redacted_thinking") => BlockKind::Thinking,
                    Some("tool_use") => BlockKind::ToolCall {
                        id: str_of(b, "id").unwrap_or_default().to_string(),
                        name: str_of(b, "name").unwrap_or_default().to_string(),
                    },
                    _ => {
                        self.skipped.insert(index);
                        return;
                    }
                };
                out.push(Event::BlockStart { index, kind });
                // 有的兼容实现把内容直接放在开始帧里，不再发增量
                let first = match str_of(b, "type") {
                    Some("text") => str_of(b, "text")
                        .filter(|t| !t.is_empty())
                        .map(|t| Delta::Text(t.to_string())),
                    Some("thinking") => str_of(b, "thinking")
                        .filter(|t| !t.is_empty())
                        .map(|t| Delta::Thinking(t.to_string())),
                    Some("redacted_thinking") => Some(Delta::Signature(Signature {
                        vendor: Vendor::Anthropic,
                        value: str_of(b, "data").unwrap_or_default().to_string(),
                        redacted: true,
                    })),
                    Some("tool_use") => b
                        .get("input")
                        .filter(|i| i.as_object().is_some_and(|o| !o.is_empty()))
                        .map(|i| Delta::ToolInput(i.to_string())),
                    _ => None,
                };
                if let Some(delta) = first {
                    out.push(Event::Delta { index, delta });
                }
            }
            "content_block_delta" if !self.skipped.contains(&index) => {
                let d = v.get("delta").unwrap_or(&Value::Null);
                let delta = match str_of(d, "type") {
                    Some("text_delta") => {
                        Delta::Text(str_of(d, "text").unwrap_or_default().to_string())
                    }
                    Some("thinking_delta") => {
                        Delta::Thinking(str_of(d, "thinking").unwrap_or_default().to_string())
                    }
                    Some("signature_delta") => match str_of(d, "signature")
                        .and_then(|s| Signature::read(s, Vendor::Anthropic))
                    {
                        Some(s) => Delta::Signature(s),
                        None => return,
                    },
                    Some("input_json_delta") => {
                        Delta::ToolInput(str_of(d, "partial_json").unwrap_or_default().to_string())
                    }
                    _ => return,
                };
                out.push(Event::Delta { index, delta });
            }
            "content_block_stop" if !self.skipped.contains(&index) => {
                out.push(Event::BlockStop { index });
            }
            "message_delta" => {
                if let Some(s) = v.get("delta").and_then(|d| str_of(d, "stop_reason")) {
                    let reason = match stop_reason(s) {
                        StopReason::StopSequence(_) => StopReason::StopSequence(
                            v.get("delta")
                                .and_then(|d| str_of(d, "stop_sequence"))
                                .map(str::to_string),
                        ),
                        other => other,
                    };
                    out.push(Event::Stop(reason));
                }
                if let Some(u) = v.get("usage") {
                    self.usage.merge(&usage(u));
                    out.push(Event::Usage(self.usage));
                }
            }
            "error" => out.push(Event::Error {
                message: super::response::error_message(&v).unwrap_or_else(|| f.data.clone()),
            }),
            _ => {}
        }
    }
}

// ───────────────────────────────────────────────────────── 写出

#[derive(Debug)]
pub struct Writer {
    model: String,
    started: bool,
    /// 中间表示的块 → (我们写出的序号, 是不是思考块, 有没有写过签名)
    blocks: HashMap<usize, (usize, bool, bool)>,
    open: Vec<usize>,
    next: usize,
    usage: Usage,
    stop: Option<StopReason>,
    failed: bool,
}

impl Writer {
    pub fn new(s: &Session) -> Writer {
        Writer {
            model: s.model.clone(),
            started: false,
            blocks: HashMap::new(),
            open: Vec::new(),
            next: 0,
            usage: Usage::default(),
            stop: None,
            failed: false,
        }
    }

    fn start(&mut self, id: Option<&str>, model: Option<&str>, out: &mut String) {
        if self.started {
            return;
        }
        self.started = true;
        if let Some(m) = model {
            self.model = m.to_string();
        }
        out.push_str(&frame::named(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": message_id(id),
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    // 真数字在结尾的 message_delta 里，见文件头
                    "usage": usage_json(&self.usage),
                }
            }),
        ));
    }

    pub fn event(&mut self, e: &Event) -> String {
        let mut out = String::new();
        if self.failed {
            return out;
        }
        match e {
            Event::Start { id, model } => self.start(id.as_deref(), model.as_deref(), &mut out),
            Event::BlockStart { index, kind } => {
                self.start(None, None, &mut out);
                let n = self.next;
                self.next += 1;
                let (block, thinking) = match kind {
                    BlockKind::Text => (json!({ "type": "text", "text": "" }), false),
                    BlockKind::Thinking => (
                        json!({ "type": "thinking", "thinking": "", "signature": "" }),
                        true,
                    ),
                    BlockKind::ToolCall { id, name } => (
                        json!({ "type": "tool_use", "id": id, "name": name, "input": {} }),
                        false,
                    ),
                };
                self.blocks.insert(*index, (n, thinking, false));
                self.open.push(*index);
                out.push_str(&frame::named(
                    "content_block_start",
                    &json!({ "type": "content_block_start", "index": n, "content_block": block }),
                ));
            }
            Event::Delta { index, delta } => {
                let Some((n, _, signed)) = self.blocks.get_mut(index) else {
                    return out;
                };
                let d = match delta {
                    Delta::Text(t) => json!({ "type": "text_delta", "text": t }),
                    Delta::Thinking(t) => json!({ "type": "thinking_delta", "thinking": t }),
                    Delta::Signature(s) => {
                        *signed = true;
                        json!({ "type": "signature_delta", "signature": s.carried_in(Vendor::Anthropic) })
                    }
                    Delta::ToolInput(p) => json!({ "type": "input_json_delta", "partial_json": p }),
                };
                out.push_str(&frame::named(
                    "content_block_delta",
                    &json!({ "type": "content_block_delta", "index": *n, "delta": d }),
                ));
            }
            Event::BlockStop { index } => self.stop_block(*index, &mut out),
            Event::Usage(u) => self.usage.merge(u),
            Event::Stop(s) => self.stop = Some(s.clone()),
            Event::Error { message } => {
                self.failed = true;
                out.push_str(&frame::named(
                    "error",
                    &json!({ "type": "error", "error": { "type": "api_error", "message": message } }),
                ));
            }
        }
        out
    }

    fn stop_block(&mut self, index: usize, out: &mut String) {
        let Some(pos) = self.open.iter().position(|i| *i == index) else {
            return;
        };
        self.open.remove(pos);
        let Some((n, thinking, signed)) = self.blocks.get(&index).copied() else {
            return;
        };
        // 没有签名的推理（DeepSeek 这类）也要写一个签名：客户端带回来时认得出
        if thinking && !signed {
            out.push_str(&frame::named(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": n,
                    "delta": { "type": "signature_delta", "signature": unsigned_marker() },
                }),
            ));
        }
        out.push_str(&frame::named(
            "content_block_stop",
            &json!({ "type": "content_block_stop", "index": n }),
        ));
    }

    /// 流结束。**幂等**，连接关闭和上游的结束帧都可能触发
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if self.failed {
            return out;
        }
        self.failed = true;
        self.start(None, None, &mut out);
        for index in self.open.clone() {
            self.stop_block(index, &mut out);
        }
        let stop_sequence = match &self.stop {
            Some(StopReason::StopSequence(Some(s))) => json!(s),
            _ => Value::Null,
        };
        out.push_str(&frame::named(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": self.stop.as_ref().map(stop_reason_str).unwrap_or(Value::Null),
                    "stop_sequence": stop_sequence,
                },
                "usage": usage_json(&self.usage),
            }),
        ));
        out.push_str(&frame::named(
            "message_stop",
            &json!({ "type": "message_stop" }),
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Decoder;

    pub(crate) fn frames(s: &str) -> Vec<(String, Value)> {
        let mut d = Decoder::default();
        d.feed(s.as_bytes())
            .into_iter()
            .map(|f| {
                (
                    f.event.unwrap_or_default(),
                    serde_json::from_str(&f.data).unwrap(),
                )
            })
            .collect()
    }

    const UPSTREAM: &str = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-opus-4-7\",\"usage\":{\"input_tokens\":12,\"cache_read_input_tokens\":900,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"想\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"s\",\"name\":\"web_search\",\"input\":{}}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Read\",\"input\":{}}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"a\\\"}\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":40}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );

    fn parse(s: &str) -> Vec<Event> {
        let mut d = Decoder::default();
        let mut p = Parser::default();
        let mut out = Vec::new();
        for f in d.feed(s.as_bytes()) {
            p.frame(&f, &mut out);
        }
        out
    }

    #[test]
    fn the_upstream_stream_becomes_blocks_and_server_tools_disappear() {
        let ev = parse(UPSTREAM);
        let starts: Vec<&BlockKind> = ev
            .iter()
            .filter_map(|e| match e {
                Event::BlockStart { kind, .. } => Some(kind),
                _ => None,
            })
            .collect();
        assert_eq!(starts.len(), 2, "{ev:#?}");
        assert!(ev.contains(&Event::Delta {
            index: 0,
            delta: Delta::Signature(Signature::new(Vendor::Anthropic, "sig"))
        }));
        assert!(ev.contains(&Event::Stop(StopReason::ToolUse)));
        let Some(Event::Usage(u)) = ev.iter().rev().find(|e| matches!(e, Event::Usage(_))) else {
            panic!()
        };
        assert_eq!((u.input, u.cache_read, u.output), (12, 900, 40));
    }

    #[test]
    fn writing_the_events_back_gives_a_complete_envelope() {
        let s = Session::for_test(Dialect::Anthropic, Dialect::Anthropic);
        let mut w = Writer::new(&s);
        let mut out = String::new();
        for e in parse(UPSTREAM) {
            out.push_str(&w.event(&e));
        }
        out.push_str(&w.finish());
        assert!(w.finish().is_empty(), "finish 要幂等");
        let f = frames(&out);
        let kinds: Vec<&str> = f.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        // 跳过服务端工具之后序号是连续的
        assert_eq!(f[5].1["index"], 1);
        assert_eq!(f[9].1["delta"]["stop_reason"], "tool_use");
        assert_eq!(f[9].1["usage"]["cache_read_input_tokens"], 900);
        assert_eq!(f[9].1["usage"]["output_tokens"], 40);
    }

    #[test]
    fn unsigned_thinking_gets_the_marker_before_it_closes() {
        let s = Session::for_test(Dialect::Anthropic, Dialect::Chat);
        let mut w = Writer::new(&s);
        let mut out = String::new();
        for e in [
            Event::BlockStart {
                index: 7,
                kind: BlockKind::Thinking,
            },
            Event::Delta {
                index: 7,
                delta: Delta::Thinking("嗯".into()),
            },
            Event::BlockStop { index: 7 },
        ] {
            out.push_str(&w.event(&e));
        }
        let f = frames(&out);
        assert_eq!(f[0].0, "message_start");
        assert_eq!(f[3].1["delta"]["signature"], "tw1.n.");
    }

    #[test]
    fn a_stream_that_dies_early_still_closes_what_it_opened() {
        let s = Session::for_test(Dialect::Anthropic, Dialect::Chat);
        let mut w = Writer::new(&s);
        let mut out = w.event(&Event::BlockStart {
            index: 0,
            kind: BlockKind::Text,
        });
        out.push_str(&w.finish());
        let kinds: Vec<String> = frames(&out).into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            kinds,
            [
                "message_start",
                "content_block_start",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
    }

    #[test]
    fn an_upstream_error_ends_the_stream_with_an_error_event() {
        let s = Session::for_test(Dialect::Anthropic, Dialect::Responses);
        let mut w = Writer::new(&s);
        let out = w.event(&Event::Error {
            message: "上游过载".into(),
        });
        assert_eq!(frames(&out)[0].1["error"]["message"], "上游过载");
        assert!(w.finish().is_empty());
    }
}
