//! Gemini 的流 ⇄ 中间表示的事件。
//!
//! Gemini 的每一帧都是一个完整的响应对象，文字是增量，**函数调用整个出现在一帧里**。
//! 写给 Gemini 客户端时也要这样：工具参数攒到块结束再整个写出。
//!
//! 客户端请求带 `alt=sse` 时流是 SSE；不带时是一个逐步写出的 JSON 数组。

use std::collections::HashMap;

use serde_json::{Value, json};

use super::request::field;
use super::response::{finish_reason, stop_reason, usage, usage_json};
use crate::convert::Session;
use crate::frame::{self, Frame};
use crate::ir::*;

// ───────────────────────────────────────────────────────── 解析

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Open {
    Text,
    Thinking,
}

#[derive(Debug, Default)]
pub struct Parser {
    started: bool,
    open: Option<(usize, Open)>,
    next: usize,
    usage: Usage,
    tool_call: bool,
}

impl Parser {
    pub fn frame(&mut self, f: &Frame, out: &mut Vec<Event>) {
        let Ok(v) = serde_json::from_str::<Value>(&f.data) else {
            return;
        };
        self.chunk(&v, out);
    }

    pub fn chunk(&mut self, v: &Value, out: &mut Vec<Event>) {
        // `"error": null` 不是错误
        if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
            out.push(Event::Error {
                message: str_of(e, "message")
                    .unwrap_or("the upstream returned an error")
                    .to_string(),
            });
            return;
        }
        if !self.started {
            self.started = true;
            out.push(Event::Start {
                id: field(v, "responseId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                model: field(v, "modelVersion")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
        let candidate = v.get("candidates").and_then(|c| c.get(0));
        let parts = candidate
            .and_then(|c| c.get("content"))
            .map(|c| arr_of(c, "parts"))
            .unwrap_or(&[]);
        for p in parts {
            if let Some(t) = field(p, "text").and_then(Value::as_str) {
                let thought = p.get("thought").and_then(Value::as_bool) == Some(true);
                let index = self.ensure(if thought { Open::Thinking } else { Open::Text }, out);
                if !t.is_empty() {
                    out.push(Event::Delta {
                        index,
                        delta: if thought {
                            Delta::Thinking(t.to_string())
                        } else {
                            Delta::Text(t.to_string())
                        },
                    });
                }
                if thought
                    && let Some(sig) = field(p, "thoughtSignature")
                        .and_then(Value::as_str)
                        .and_then(|s| Signature::read(s, Vendor::Google))
                {
                    out.push(Event::Delta {
                        index,
                        delta: Delta::Signature(sig),
                    });
                }
            } else if let Some(call) = field(p, "functionCall") {
                self.close(out);
                // 这一格上的签名记成紧挨在前面的空思考块（见 `request::call_signature`）
                if let Some(sig) = field(p, "thoughtSignature")
                    .and_then(Value::as_str)
                    .and_then(|s| Signature::read(s, Vendor::Google))
                {
                    let index = self.next;
                    self.next += 1;
                    out.push(Event::BlockStart {
                        index,
                        kind: BlockKind::Thinking,
                    });
                    out.push(Event::Delta {
                        index,
                        delta: Delta::Signature(sig),
                    });
                    out.push(Event::BlockStop { index });
                }
                let index = self.next;
                self.next += 1;
                self.tool_call = true;
                out.push(Event::BlockStart {
                    index,
                    kind: BlockKind::ToolCall {
                        id: str_of(call, "id")
                            .map(str::to_string)
                            .unwrap_or_else(|| new_id("call_")),
                        name: str_of(call, "name").unwrap_or_default().to_string(),
                    },
                });
                out.push(Event::Delta {
                    index,
                    delta: Delta::ToolInput(
                        call.get("args")
                            .cloned()
                            .unwrap_or_else(|| json!({}))
                            .to_string(),
                    ),
                });
                out.push(Event::BlockStop { index });
            }
        }
        if let Some(u) = field(v, "usageMetadata") {
            self.usage.merge(&usage(u));
            out.push(Event::Usage(self.usage));
        }
        if let Some(s) = candidate
            .and_then(|c| field(c, "finishReason"))
            .and_then(Value::as_str)
        {
            out.push(Event::Stop(stop_reason(s, self.tool_call)));
        }
    }

    fn ensure(&mut self, want: Open, out: &mut Vec<Event>) -> usize {
        if let Some((i, o)) = self.open
            && o == want
        {
            return i;
        }
        self.close(out);
        let index = self.next;
        self.next += 1;
        out.push(Event::BlockStart {
            index,
            kind: match want {
                Open::Text => BlockKind::Text,
                Open::Thinking => BlockKind::Thinking,
            },
        });
        self.open = Some((index, want));
        index
    }

    fn close(&mut self, out: &mut Vec<Event>) {
        if let Some((index, _)) = self.open.take() {
            out.push(Event::BlockStop { index });
        }
    }

    pub fn finish(&mut self, out: &mut Vec<Event>) {
        self.close(out);
    }
}

// ───────────────────────────────────────────────────────── 写出

#[derive(Debug)]
pub struct Writer {
    sse: bool,
    wrote_any: bool,
    id: String,
    model: String,
    /// 工具块 → (id, 名字, 攒着的参数)
    tools: HashMap<usize, (String, String, String)>,
    /// 思考块最后一段还没写出的文字：块结束时和签名写在同一格里，所以每段都压一段
    /// 再写，只晚一段
    thoughts: HashMap<usize, String>,
    /// 思考块的签名，块结束时写出
    signatures: HashMap<usize, Signature>,
    /// 没有文字的思考块只是签名（functionCall 的签名、改写过的思考）：等着写到下一个
    /// functionCall 那一格上；下一格不是 functionCall 就单独写一格
    pending: Vec<Signature>,
    usage: Option<Usage>,
    stop: Option<StopReason>,
    done: bool,
}

impl Writer {
    pub fn new(s: &Session) -> Writer {
        Writer {
            sse: s.shape.gemini_sse,
            wrote_any: false,
            id: new_id(""),
            model: s.model.clone(),
            tools: HashMap::new(),
            thoughts: HashMap::new(),
            signatures: HashMap::new(),
            pending: Vec::new(),
            usage: None,
            stop: None,
            done: false,
        }
    }

    fn write(&mut self, v: &Value, out: &mut String) {
        if self.sse {
            out.push_str(&frame::data(v));
        } else {
            out.push_str(if self.wrote_any { ",\r\n" } else { "[" });
            out.push_str(&v.to_string());
        }
        self.wrote_any = true;
    }

    /// 等着放到 functionCall 上的签名各自写成一格空思考
    fn flush_pending(&mut self, parts: &mut Vec<Value>) {
        for s in self.pending.drain(..) {
            parts.push(json!({
                "text": "",
                "thought": true,
                "thoughtSignature": s.carried_in(Vendor::Google),
            }));
        }
    }

    fn chunk(&mut self, mut parts: Vec<Value>, out: &mut String) {
        if !self.pending.is_empty() {
            let mut all = Vec::new();
            self.flush_pending(&mut all);
            all.append(&mut parts);
            parts = all;
        }
        let v = json!({
            "candidates": [{ "content": { "role": "model", "parts": parts }, "index": 0 }],
            "modelVersion": self.model,
            "responseId": self.id,
        });
        self.write(&v, out);
    }

    pub fn event(&mut self, e: &Event) -> String {
        let mut out = String::new();
        if self.done {
            return out;
        }
        match e {
            Event::Start { id, model } => {
                if let Some(i) = id {
                    self.id = i.clone();
                }
                if let Some(m) = model {
                    self.model = m.clone();
                }
            }
            Event::BlockStart {
                index,
                kind: BlockKind::ToolCall { id, name },
            } => {
                self.tools
                    .insert(*index, (id.clone(), name.clone(), String::new()));
            }
            Event::BlockStart { .. } => {}
            Event::Delta { index, delta } => match delta {
                Delta::Text(t) => self.chunk(vec![json!({ "text": t })], &mut out),
                Delta::Thinking(t) if !t.is_empty() => {
                    if let Some(prev) = self.thoughts.insert(*index, t.clone()) {
                        self.chunk(vec![json!({ "text": prev, "thought": true })], &mut out);
                    }
                }
                Delta::Thinking(_) => {}
                Delta::Signature(s) => {
                    self.signatures.insert(*index, s.clone());
                }
                Delta::ToolInput(p) => {
                    if let Some((_, _, args)) = self.tools.get_mut(index) {
                        args.push_str(p);
                    }
                }
            },
            Event::BlockStop { index } => {
                if let Some((id, name, args)) = self.tools.remove(index) {
                    let call = ToolCall {
                        id,
                        name,
                        input: ToolInput::from_json_text(&args),
                    };
                    // 一格只放一个签名：攒了几个时前面的各自写一格
                    let sig = self.pending.pop();
                    let mut parts = Vec::new();
                    self.flush_pending(&mut parts);
                    parts.push(super::response::call_part(&call, sig.as_ref()));
                    self.chunk(parts, &mut out);
                } else {
                    match (self.thoughts.remove(index), self.signatures.remove(index)) {
                        (Some(text), sig) => {
                            let mut p = json!({ "text": text, "thought": true });
                            if let Some(s) = sig {
                                p["thoughtSignature"] = json!(s.carried_in(Vendor::Google));
                            }
                            self.chunk(vec![p], &mut out);
                        }
                        (None, Some(s)) => self.pending.push(s),
                        (None, None) => {}
                    }
                }
            }
            Event::Usage(u) => self.usage.get_or_insert_default().merge(u),
            Event::Stop(s) => self.stop = Some(s.clone()),
            Event::Error { message } => {
                self.write(&super::response::error_body(500, message), &mut out);
                self.close(&mut out);
            }
        }
        out
    }

    fn close(&mut self, out: &mut String) {
        self.done = true;
        if !self.sse {
            out.push_str(if self.wrote_any { "]" } else { "[]" });
        }
    }

    /// 流结束。**幂等**
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if self.done {
            return out;
        }
        let mut parts = Vec::new();
        self.flush_pending(&mut parts);
        // 没等到块结束的思考文字也不能丢
        let mut open: Vec<(usize, String)> = self.thoughts.drain().collect();
        open.sort_by_key(|(i, _)| *i);
        for (index, text) in open {
            let mut p = json!({ "text": text, "thought": true });
            if let Some(s) = self.signatures.remove(&index) {
                p["thoughtSignature"] = json!(s.carried_in(Vendor::Google));
            }
            parts.push(p);
        }
        let mut v = json!({
            "candidates": [{
                "content": { "role": "model", "parts": parts },
                "finishReason": finish_reason(self.stop.as_ref().unwrap_or(&StopReason::EndTurn)),
                "index": 0,
            }],
            "modelVersion": self.model,
            "responseId": self.id,
        });
        if let Some(u) = &self.usage {
            v["usageMetadata"] = usage_json(u);
        }
        self.write(&v, &mut out);
        self.close(&mut out);
        out
    }
}

/// 流里的一个错误帧（`alt=sse` 的写法）
pub(crate) fn error_frame(status: u16, message: &str) -> String {
    frame::data(&super::response::error_body(status, message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Decoder;

    #[test]
    fn a_gemini_stream_splits_into_thinking_text_and_a_whole_call() {
        let s = concat!(
            "data: {\"responseId\":\"r1\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"想\",\"thought\":true}]}}]}\r\n\r\n",
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"我\"}]}}]}\r\n\r\n",
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"看看\"}]}}]}\r\n\r\n",
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"ls\",\"args\":{\"p\":\".\"}},\"thoughtSignature\":\"CiQB\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":5,\"thoughtsTokenCount\":3}}\r\n\r\n",
        );
        let mut d = Decoder::default();
        let mut p = Parser::default();
        let mut ev = Vec::new();
        for f in d.feed(s.as_bytes()) {
            p.frame(&f, &mut ev);
        }
        p.finish(&mut ev);
        let kinds: Vec<&BlockKind> = ev
            .iter()
            .filter_map(|e| match e {
                Event::BlockStart { kind, .. } => Some(kind),
                _ => None,
            })
            .collect();
        assert_eq!(kinds.len(), 4);
        assert_eq!(
            kinds[2],
            &BlockKind::Thinking,
            "functionCall 上的签名是紧挨在调用前面的一个空思考块"
        );
        assert!(matches!(kinds[3], BlockKind::ToolCall { name, .. } if name == "ls"));
        assert!(ev.contains(&Event::Delta {
            index: 2,
            delta: Delta::Signature(Signature::new(Vendor::Google, "CiQB")),
        }));
        let text: String = ev
            .iter()
            .filter_map(|e| match e {
                Event::Delta {
                    delta: Delta::Text(t),
                    ..
                } => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "我看看");
        assert!(ev.contains(&Event::Stop(StopReason::ToolUse)));
        assert!(ev.contains(&Event::Usage(Usage {
            input: 10,
            output: 8,
            reasoning: 3,
            ..Default::default()
        })));
    }

    fn events() -> Vec<Event> {
        vec![
            Event::Start {
                id: Some("msg_1".into()),
                model: Some("claude-opus-4-7".into()),
            },
            Event::BlockStart {
                index: 0,
                kind: BlockKind::Text,
            },
            Event::Delta {
                index: 0,
                delta: Delta::Text("好".into()),
            },
            Event::BlockStop { index: 0 },
            Event::BlockStart {
                index: 1,
                kind: BlockKind::ToolCall {
                    id: "toolu_1".into(),
                    name: "ls".into(),
                },
            },
            Event::Delta {
                index: 1,
                delta: Delta::ToolInput("{\"p\":".into()),
            },
            Event::Delta {
                index: 1,
                delta: Delta::ToolInput("\".\"}".into()),
            },
            Event::BlockStop { index: 1 },
            Event::Stop(StopReason::ToolUse),
            Event::Usage(Usage {
                input: 4,
                output: 6,
                ..Default::default()
            }),
        ]
    }

    fn write(sse: bool) -> String {
        let mut s = Session::for_test(Dialect::Gemini, Dialect::Anthropic);
        s.shape.gemini_sse = sse;
        let mut w = Writer::new(&s);
        let mut out = String::new();
        for e in events() {
            out.push_str(&w.event(&e));
        }
        out.push_str(&w.finish());
        assert!(w.finish().is_empty());
        out
    }

    #[test]
    fn written_as_sse_the_call_arrives_whole() {
        let out = write(true);
        let mut d = Decoder::default();
        let chunks: Vec<Value> = d
            .feed(out.as_bytes())
            .into_iter()
            .map(|f| serde_json::from_str(&f.data).unwrap())
            .collect();
        assert_eq!(chunks.len(), 3);
        assert_eq!(
            chunks[1]["candidates"][0]["content"]["parts"][0]["functionCall"]["args"]["p"],
            "."
        );
        assert_eq!(chunks[2]["candidates"][0]["finishReason"], "STOP");
        assert_eq!(chunks[2]["usageMetadata"]["totalTokenCount"], 10);
        assert_eq!(chunks[0]["modelVersion"], "claude-opus-4-7");
    }

    fn chunks_of(events: &[Event]) -> Vec<Value> {
        let s = Session::for_test(Dialect::Gemini, Dialect::Anthropic);
        let mut w = Writer::new(&s);
        let mut out = String::new();
        for e in events {
            out.push_str(&w.event(e));
        }
        out.push_str(&w.finish());
        let mut d = Decoder::default();
        d.feed(out.as_bytes())
            .into_iter()
            .map(|f| serde_json::from_str(&f.data).unwrap())
            .collect()
    }

    fn parts_of(chunks: &[Value]) -> Vec<Value> {
        chunks
            .iter()
            .flat_map(|c| {
                c["candidates"][0]["content"]["parts"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    }

    /// 思考文字一段一段写，签名和最后一段写在同一格里，不单独占一格。
    #[test]
    fn a_thoughts_signature_is_written_in_the_same_part_as_its_last_text() {
        let chunks = chunks_of(&[
            Event::BlockStart {
                index: 0,
                kind: BlockKind::Thinking,
            },
            Event::Delta {
                index: 0,
                delta: Delta::Thinking("先".into()),
            },
            Event::Delta {
                index: 0,
                delta: Delta::Thinking("想想".into()),
            },
            Event::Delta {
                index: 0,
                delta: Delta::Signature(Signature::new(Vendor::Anthropic, "sig")),
            },
            Event::BlockStop { index: 0 },
            Event::BlockStart {
                index: 1,
                kind: BlockKind::Text,
            },
            Event::Delta {
                index: 1,
                delta: Delta::Text("好".into()),
            },
            Event::BlockStop { index: 1 },
            Event::Stop(StopReason::EndTurn),
        ]);
        let parts = parts_of(&chunks);
        assert_eq!(
            parts,
            json!([
                {"text": "先", "thought": true},
                {"text": "想想", "thought": true, "thoughtSignature": "tw1.a.sig"},
                {"text": "好"},
            ])
            .as_array()
            .unwrap()
            .as_slice(),
            "{chunks:?}"
        );
    }

    /// 没有文字的签名块跟着工具调用：签名写在 functionCall 那一格上。
    #[test]
    fn a_calls_signature_is_written_on_the_function_call_part() {
        let chunks = chunks_of(&[
            Event::BlockStart {
                index: 0,
                kind: BlockKind::Thinking,
            },
            Event::Delta {
                index: 0,
                delta: Delta::Thinking(String::new()),
            },
            Event::Delta {
                index: 0,
                delta: Delta::Signature(Signature::new(Vendor::Google, "CiQB")),
            },
            Event::BlockStop { index: 0 },
            Event::BlockStart {
                index: 1,
                kind: BlockKind::ToolCall {
                    id: "call_1".into(),
                    name: "ls".into(),
                },
            },
            Event::Delta {
                index: 1,
                delta: Delta::ToolInput("{\"p\":\".\"}".into()),
            },
            Event::BlockStop { index: 1 },
            Event::Stop(StopReason::ToolUse),
        ]);
        let parts = parts_of(&chunks);
        assert_eq!(parts.len(), 1, "{chunks:?}");
        assert_eq!(parts[0]["functionCall"]["name"], "ls");
        assert_eq!(parts[0]["thoughtSignature"], "CiQB");
    }

    /// 签名块后面不是工具调用：单独写一格，不能丢。
    #[test]
    fn a_signature_without_a_call_after_it_still_gets_a_part() {
        let chunks = chunks_of(&[
            Event::BlockStart {
                index: 0,
                kind: BlockKind::Thinking,
            },
            Event::Delta {
                index: 0,
                delta: Delta::Signature(Signature {
                    vendor: Vendor::Anthropic,
                    value: "data".into(),
                    redacted: true,
                }),
            },
            Event::BlockStop { index: 0 },
            Event::BlockStart {
                index: 1,
                kind: BlockKind::Text,
            },
            Event::Delta {
                index: 1,
                delta: Delta::Text("好".into()),
            },
            Event::BlockStop { index: 1 },
        ]);
        assert_eq!(
            parts_of(&chunks),
            json!([
                {"text": "", "thought": true, "thoughtSignature": "tw1.ar.data"},
                {"text": "好"},
            ])
            .as_array()
            .unwrap()
            .as_slice()
        );
        // 流结束时还挂着的也写出来
        let chunks = chunks_of(&[
            Event::BlockStart {
                index: 0,
                kind: BlockKind::Thinking,
            },
            Event::Delta {
                index: 0,
                delta: Delta::Signature(Signature::new(Vendor::Google, "CiQB")),
            },
            Event::BlockStop { index: 0 },
        ]);
        assert_eq!(parts_of(&chunks)[0]["thoughtSignature"], "CiQB");
    }

    #[test]
    fn without_alt_sse_the_stream_is_one_json_array() {
        let out = write(false);
        let v: Vec<Value> = serde_json::from_str(&out).unwrap();
        assert_eq!(v.len(), 3);
    }
}
