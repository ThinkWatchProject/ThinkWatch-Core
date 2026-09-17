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
        if let Some(e) = v.get("error") {
            out.push(Event::Error {
                message: str_of(e, "message").unwrap_or("上游返回错误").to_string(),
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
    /// 思考块的签名，块结束时写出
    signatures: HashMap<usize, Signature>,
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
            signatures: HashMap::new(),
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

    fn chunk(&mut self, parts: Vec<Value>, out: &mut String) {
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
                Delta::Thinking(t) => {
                    self.chunk(vec![json!({ "text": t, "thought": true })], &mut out)
                }
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
                    let args = ToolInput::from_json_text(&args).to_object();
                    self.chunk(
                        vec![json!({ "functionCall": { "id": id, "name": name, "args": args } })],
                        &mut out,
                    );
                } else if let Some(s) = self.signatures.remove(index) {
                    self.chunk(
                        vec![json!({
                            "text": "",
                            "thought": true,
                            "thoughtSignature": s.carried_in(Vendor::Google),
                        })],
                        &mut out,
                    );
                }
            }
            Event::Usage(u) => self.usage.get_or_insert_default().merge(u),
            Event::Stop(s) => self.stop = Some(s.clone()),
            Event::Error { message } => {
                self.write(
                    &json!({ "error": { "code": 500, "message": message, "status": "INTERNAL" } }),
                    &mut out,
                );
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
        let mut v = json!({
            "candidates": [{
                "content": { "role": "model", "parts": [] },
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
        assert_eq!(kinds.len(), 3);
        assert!(matches!(kinds[2], BlockKind::ToolCall { name, .. } if name == "ls"));
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

    #[test]
    fn without_alt_sse_the_stream_is_one_json_array() {
        let out = write(false);
        let v: Vec<Value> = serde_json::from_str(&out).unwrap();
        assert_eq!(v.len(), 3);
    }
}
