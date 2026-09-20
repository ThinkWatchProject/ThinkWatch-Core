//! OpenAI Responses 的流 ⇄ 中间表示的事件。
//!
//! Responses 的流按**输出项**组织：每个项有 `output_item.added` 和 `output_item.done`，
//! 中间是增量。**客户端认的是 `output_item.done` 里的完整项** —— Codex 用它记历史，
//! 增量只用来显示。所以写出时每个项结束都要带上完整内容，推理项还要带上
//! `encrypted_content`，否则下一轮带不回来。

use std::collections::HashMap;

use serde_json::{Value, json};

use super::response::{
    envelope, incomplete, item, item_id, reasoning_signature, reasoning_text, response_id, status,
    usage, usage_json,
};
use crate::convert::Session;
use crate::frame::{self, Frame};
use crate::ir::*;

// ───────────────────────────────────────────────────────── 解析

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    Message,
    Reasoning,
    Call,
}

#[derive(Debug)]
struct Item {
    kind: ItemKind,
    /// 推理项和调用项对应的块。消息项的文字按内容部分另外开块
    block: Option<usize>,
    got_delta: bool,
    summary_index: Option<u64>,
}

#[derive(Debug, Default)]
pub struct Parser {
    started: bool,
    usage: Usage,
    items: HashMap<u64, Item>,
    /// (输出项序号, 内容部分序号) → (块, 结束了没有)
    texts: HashMap<(u64, u64), (usize, bool)>,
    next: usize,
    tool_call: bool,
}

impl Parser {
    fn open(&mut self, kind: BlockKind, out: &mut Vec<Event>) -> usize {
        let index = self.next;
        self.next += 1;
        out.push(Event::BlockStart { index, kind });
        index
    }

    fn text_block(&mut self, oi: u64, ci: u64, out: &mut Vec<Event>) -> Option<usize> {
        match self.texts.get(&(oi, ci)) {
            Some((i, false)) => Some(*i),
            Some((_, true)) => None,
            None => {
                let i = self.open(BlockKind::Text, out);
                self.texts.insert((oi, ci), (i, false));
                Some(i)
            }
        }
    }

    fn close_text(&mut self, oi: u64, ci: u64, out: &mut Vec<Event>) {
        if let Some((i, closed)) = self.texts.get_mut(&(oi, ci))
            && !*closed
        {
            *closed = true;
            out.push(Event::BlockStop { index: *i });
        }
    }

    pub fn frame(&mut self, f: &Frame, out: &mut Vec<Event>) {
        let Ok(v) = serde_json::from_str::<Value>(&f.data) else {
            return;
        };
        let kind = str_of(&v, "type").or(f.event.as_deref()).unwrap_or("");
        let oi = u64_of(&v, "output_index").unwrap_or(0);
        let ci = u64_of(&v, "content_index").unwrap_or(0);
        let delta = str_of(&v, "delta").unwrap_or_default();
        match kind {
            "response.created" | "response.in_progress" | "response.queued" => {
                if !self.started {
                    self.started = true;
                    let r = v.get("response").unwrap_or(&Value::Null);
                    out.push(Event::Start {
                        id: str_of(r, "id").map(str::to_string),
                        model: str_of(r, "model").map(str::to_string),
                    });
                }
            }
            "response.output_item.added" => {
                let it = v.get("item").unwrap_or(&Value::Null);
                let item = match str_of(it, "type") {
                    Some("message") => Item {
                        kind: ItemKind::Message,
                        block: None,
                        got_delta: false,
                        summary_index: None,
                    },
                    Some("reasoning") => Item {
                        kind: ItemKind::Reasoning,
                        block: Some(self.open(BlockKind::Thinking, out)),
                        got_delta: false,
                        summary_index: None,
                    },
                    Some(t @ ("function_call" | "custom_tool_call")) => {
                        let block = self.open(
                            BlockKind::ToolCall {
                                id: str_of(it, "call_id").unwrap_or_default().to_string(),
                                name: str_of(it, "name").unwrap_or_default().to_string(),
                            },
                            out,
                        );
                        let key = if t == "function_call" {
                            "arguments"
                        } else {
                            "input"
                        };
                        let first = str_of(it, key).unwrap_or_default();
                        if !first.is_empty() {
                            out.push(Event::Delta {
                                index: block,
                                delta: Delta::ToolInput(first.to_string()),
                            });
                        }
                        Item {
                            kind: ItemKind::Call,
                            block: Some(block),
                            got_delta: !first.is_empty(),
                            summary_index: None,
                        }
                    }
                    _ => return,
                };
                self.items.insert(oi, item);
            }
            "response.content_part.added" => {
                let part = str_of(v.get("part").unwrap_or(&Value::Null), "type");
                if matches!(part, Some("output_text" | "refusal")) {
                    self.text_block(oi, ci, out);
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some(index) = self.text_block(oi, ci, out)
                    && !delta.is_empty()
                {
                    out.push(Event::Delta {
                        index,
                        delta: Delta::Text(delta.to_string()),
                    });
                }
            }
            "response.output_text.done"
            | "response.refusal.done"
            | "response.content_part.done" => {
                self.close_text(oi, ci, out);
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let summary_index = u64_of(&v, "summary_index");
                let Some(it) = self.items.get_mut(&oi) else {
                    return;
                };
                let Some(index) = it.block.filter(|_| it.kind == ItemKind::Reasoning) else {
                    return;
                };
                // 多段摘要之间空一行
                if summary_index.is_some()
                    && it.summary_index.is_some()
                    && summary_index != it.summary_index
                {
                    out.push(Event::Delta {
                        index,
                        delta: Delta::Thinking("\n\n".into()),
                    });
                }
                if summary_index.is_some() {
                    it.summary_index = summary_index;
                }
                it.got_delta = true;
                out.push(Event::Delta {
                    index,
                    delta: Delta::Thinking(delta.to_string()),
                });
            }
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                if let Some(it) = self.items.get_mut(&oi)
                    && let Some(index) = it.block
                    && it.kind == ItemKind::Call
                {
                    it.got_delta = true;
                    out.push(Event::Delta {
                        index,
                        delta: Delta::ToolInput(delta.to_string()),
                    });
                }
            }
            "response.output_item.done" => {
                let done = v.get("item").unwrap_or(&Value::Null);
                let Some(it) = self.items.remove(&oi) else {
                    return;
                };
                match it.kind {
                    ItemKind::Message => {
                        let open: Vec<u64> = self
                            .texts
                            .iter()
                            .filter(|((o, _), (_, closed))| *o == oi && !closed)
                            .map(|((_, c), _)| *c)
                            .collect();
                        let seen = self.texts.keys().any(|(o, _)| *o == oi);
                        for c in open {
                            self.close_text(oi, c, out);
                        }
                        // 没有增量、只在完成时给了全文的实现
                        if !seen {
                            for (c, part) in arr_of(done, "content").iter().enumerate() {
                                if let Some(t) =
                                    str_of(part, "text").or_else(|| str_of(part, "refusal"))
                                    && let Some(index) = self.text_block(oi, c as u64, out)
                                {
                                    out.push(Event::Delta {
                                        index,
                                        delta: Delta::Text(t.to_string()),
                                    });
                                    self.close_text(oi, c as u64, out);
                                }
                            }
                        }
                    }
                    ItemKind::Reasoning => {
                        let index = it.block.unwrap_or_default();
                        let text = reasoning_text(done);
                        if !it.got_delta && !text.is_empty() {
                            out.push(Event::Delta {
                                index,
                                delta: Delta::Thinking(text),
                            });
                        }
                        if let Some(sig) = reasoning_signature(done) {
                            out.push(Event::Delta {
                                index,
                                delta: Delta::Signature(sig),
                            });
                        }
                        out.push(Event::BlockStop { index });
                    }
                    ItemKind::Call => {
                        let index = it.block.unwrap_or_default();
                        self.tool_call = true;
                        if !it.got_delta {
                            let whole = str_of(done, "arguments")
                                .or_else(|| str_of(done, "input"))
                                .unwrap_or_default();
                            if !whole.is_empty() {
                                out.push(Event::Delta {
                                    index,
                                    delta: Delta::ToolInput(whole.to_string()),
                                });
                            }
                        }
                        out.push(Event::BlockStop { index });
                    }
                }
            }
            "response.completed" | "response.incomplete" => {
                let r = v.get("response").unwrap_or(&Value::Null);
                if let Some(u) = r.get("usage").filter(|u| !u.is_null()) {
                    self.usage.merge(&usage(u));
                    out.push(Event::Usage(self.usage));
                }
                out.push(Event::Stop(if kind == "response.incomplete" {
                    incomplete(
                        r.get("incomplete_details")
                            .and_then(|d| str_of(d, "reason")),
                    )
                } else if self.tool_call {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                }));
            }
            "response.failed" => out.push(Event::Error {
                message: v
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .and_then(|e| str_of(e, "message"))
                    .unwrap_or("the upstream returned response.failed")
                    .to_string(),
            }),
            "error" => out.push(Event::Error {
                message: str_of(&v, "message").unwrap_or(&f.data).to_string(),
            }),
            _ => {}
        }
    }

    /// 流断了：把开着的块关上
    pub fn finish(&mut self, out: &mut Vec<Event>) {
        let mut open: Vec<usize> = self.items.drain().filter_map(|(_, it)| it.block).collect();
        open.extend(
            self.texts
                .values()
                .filter(|(_, closed)| !closed)
                .map(|(i, _)| *i),
        );
        self.texts.clear();
        open.sort_unstable();
        for index in open {
            out.push(Event::BlockStop { index });
        }
    }
}

// ───────────────────────────────────────────────────────── 写出

#[derive(Debug)]
struct Out {
    output_index: usize,
    id: String,
    block: Block,
    /// 推理摘要那一段开过没有
    part_open: bool,
    done: bool,
}

#[derive(Debug)]
pub struct Writer {
    session: Session,
    seq: u64,
    id: String,
    created: u64,
    model: String,
    started: bool,
    finished: bool,
    items: HashMap<usize, Out>,
    order: Vec<usize>,
    usage: Option<Usage>,
    stop: Option<StopReason>,
}

impl Writer {
    pub fn new(s: &Session) -> Writer {
        Writer {
            session: s.clone(),
            seq: 0,
            id: response_id(None),
            created: unix_secs(),
            model: s.model.clone(),
            started: false,
            finished: false,
            items: HashMap::new(),
            order: Vec::new(),
            usage: None,
            stop: None,
        }
    }

    fn emit(&mut self, kind: &str, mut body: Value, out: &mut String) {
        body["type"] = json!(kind);
        body["sequence_number"] = json!(self.seq);
        self.seq += 1;
        out.push_str(&frame::named(kind, &body));
    }

    fn start(&mut self, out: &mut String) {
        if self.started {
            return;
        }
        self.started = true;
        let r = envelope(&self.id, self.created, &self.model, "in_progress");
        self.emit("response.created", json!({ "response": r }), out);
        let r = envelope(&self.id, self.created, &self.model, "in_progress");
        self.emit("response.in_progress", json!({ "response": r }), out);
    }

    pub fn event(&mut self, e: &Event) -> String {
        let mut out = String::new();
        if self.finished {
            return out;
        }
        match e {
            Event::Start { id, model } => {
                if !self.started {
                    self.id = response_id(id.as_deref());
                    if let Some(m) = model {
                        self.model = m.clone();
                    }
                }
                self.start(&mut out);
            }
            Event::BlockStart { index, kind } => {
                self.start(&mut out);
                let block = match kind {
                    BlockKind::Text => Block::Text(String::new()),
                    BlockKind::Thinking => Block::Thinking(Thinking {
                        text: String::new(),
                        signature: None,
                    }),
                    BlockKind::ToolCall { id, name } => Block::ToolCall(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        input: if self.session.is_freeform(name) {
                            ToolInput::Text(String::new())
                        } else {
                            ToolInput::Json(Value::Null)
                        },
                    }),
                };
                let o = Out {
                    output_index: self.order.len(),
                    id: item_id(&block, &self.session),
                    block,
                    part_open: false,
                    done: false,
                };
                let added = item(&o.block, &o.id, false, &self.session);
                let (oi, item_id) = (o.output_index, o.id.clone());
                let is_text = matches!(o.block, Block::Text(_));
                self.items.insert(*index, o);
                self.order.push(*index);
                self.emit(
                    "response.output_item.added",
                    json!({ "output_index": oi, "item": added }),
                    &mut out,
                );
                if is_text {
                    self.emit(
                        "response.content_part.added",
                        json!({
                            "item_id": item_id, "output_index": oi, "content_index": 0,
                            "part": { "type": "output_text", "text": "", "annotations": [] },
                        }),
                        &mut out,
                    );
                }
            }
            Event::Delta { index, delta } => self.delta(*index, delta, &mut out),
            Event::BlockStop { index } => self.stop_item(*index, &mut out),
            Event::Usage(u) => self.usage.get_or_insert_default().merge(u),
            Event::Stop(s) => self.stop = Some(s.clone()),
            Event::Error { message } => {
                self.start(&mut out);
                self.finished = true;
                let mut r = envelope(&self.id, self.created, &self.model, "failed");
                r["error"] = json!({ "code": "server_error", "message": message });
                self.emit("response.failed", json!({ "response": r }), &mut out);
            }
        }
        out
    }

    fn delta(&mut self, index: usize, delta: &Delta, out: &mut String) {
        let Some(o) = self.items.get_mut(&index) else {
            return;
        };
        if o.done {
            return;
        }
        let (oi, id) = (o.output_index, o.id.clone());
        match (&mut o.block, delta) {
            (Block::Text(t), Delta::Text(d)) => {
                t.push_str(d);
                self.emit(
                    "response.output_text.delta",
                    json!({ "item_id": id, "output_index": oi, "content_index": 0, "delta": d, "logprobs": [] }),
                    out,
                );
            }
            (Block::Thinking(th), Delta::Thinking(d)) => {
                th.text.push_str(d);
                let first = !o.part_open;
                o.part_open = true;
                if first {
                    self.emit(
                        "response.reasoning_summary_part.added",
                        json!({ "item_id": id, "output_index": oi, "summary_index": 0,
                                "part": { "type": "summary_text", "text": "" } }),
                        out,
                    );
                }
                self.emit(
                    "response.reasoning_summary_text.delta",
                    json!({ "item_id": id, "output_index": oi, "summary_index": 0, "delta": d }),
                    out,
                );
            }
            (Block::Thinking(th), Delta::Signature(s)) => th.signature = Some(s.clone()),
            (Block::ToolCall(c), Delta::ToolInput(d)) => {
                let kind = match &mut c.input {
                    ToolInput::Text(t) => {
                        t.push_str(d);
                        "response.custom_tool_call_input.delta"
                    }
                    ToolInput::Json(Value::String(t)) => {
                        t.push_str(d);
                        "response.function_call_arguments.delta"
                    }
                    input => {
                        *input = ToolInput::Json(Value::String(d.clone()));
                        "response.function_call_arguments.delta"
                    }
                };
                self.emit(
                    kind,
                    json!({ "item_id": id, "output_index": oi, "delta": d }),
                    out,
                );
            }
            _ => {}
        }
    }

    fn stop_item(&mut self, index: usize, out: &mut String) {
        let Some(o) = self.items.get_mut(&index) else {
            return;
        };
        if o.done {
            return;
        }
        o.done = true;
        // 函数参数一路是按字符串攒的，结束时解回 JSON
        if let Block::ToolCall(c) = &mut o.block
            && let ToolInput::Json(v) = &c.input
        {
            c.input = match v {
                Value::String(s) => ToolInput::from_json_text(s),
                _ => ToolInput::Json(json!({})),
            };
        }
        let (oi, id, part_open) = (o.output_index, o.id.clone(), o.part_open);
        let block = o.block.clone();
        match &block {
            Block::Text(t) => {
                self.emit(
                    "response.output_text.done",
                    json!({ "item_id": id, "output_index": oi, "content_index": 0, "text": t, "logprobs": [] }),
                    out,
                );
                self.emit(
                    "response.content_part.done",
                    json!({ "item_id": id, "output_index": oi, "content_index": 0,
                            "part": { "type": "output_text", "text": t, "annotations": [] } }),
                    out,
                );
            }
            Block::Thinking(th) if part_open => {
                self.emit(
                    "response.reasoning_summary_text.done",
                    json!({ "item_id": id, "output_index": oi, "summary_index": 0, "text": th.text }),
                    out,
                );
                self.emit(
                    "response.reasoning_summary_part.done",
                    json!({ "item_id": id, "output_index": oi, "summary_index": 0,
                            "part": { "type": "summary_text", "text": th.text } }),
                    out,
                );
            }
            Block::Thinking(_) => {}
            Block::ToolCall(c) => match &c.input {
                ToolInput::Text(t) => self.emit(
                    "response.custom_tool_call_input.done",
                    json!({ "item_id": id, "output_index": oi, "input": t }),
                    out,
                ),
                input => self.emit(
                    "response.function_call_arguments.done",
                    json!({ "item_id": id, "output_index": oi, "arguments": input.to_json_text() }),
                    out,
                ),
            },
        }
        let done = item(&block, &id, true, &self.session);
        self.emit(
            "response.output_item.done",
            json!({ "output_index": oi, "item": done }),
            out,
        );
    }

    /// 流结束。**幂等**
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if self.finished {
            return out;
        }
        self.start(&mut out);
        self.finished = true;
        for index in self.order.clone() {
            self.stop_item(index, &mut out);
        }
        let (state, details) = status(self.stop.as_ref());
        let mut r = envelope(&self.id, self.created, &self.model, state);
        r["incomplete_details"] = details;
        r["output"] = Value::Array(
            self.order
                .iter()
                .filter_map(|i| self.items.get(i))
                .map(|o| item(&o.block, &o.id, true, &self.session))
                .collect(),
        );
        if let Some(u) = &self.usage {
            r["usage"] = usage_json(u);
        }
        let kind = if state == "incomplete" {
            "response.incomplete"
        } else {
            "response.completed"
        };
        self.emit(kind, json!({ "response": r }), &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Decoder;

    fn parse(s: &str) -> Vec<Event> {
        let mut d = Decoder::default();
        let mut p = Parser::default();
        let mut out = Vec::new();
        for f in d.feed(s.as_bytes()) {
            p.frame(&f, &mut out);
        }
        p.finish(&mut out);
        out
    }

    fn ev(kind: &str, body: Value) -> String {
        let mut b = body;
        b["type"] = json!(kind);
        frame::named(kind, &b)
    }

    #[test]
    fn a_codex_backend_stream_becomes_blocks() {
        let s = [
            ev("response.created", json!({"response": {"id": "resp_1", "model": "gpt-5.1-codex"}})),
            ev("response.output_item.added", json!({"output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": []}})),
            ev("response.reasoning_summary_text.delta", json!({"output_index": 0, "summary_index": 0, "delta": "一"})),
            ev("response.reasoning_summary_text.delta", json!({"output_index": 0, "summary_index": 1, "delta": "二"})),
            ev("response.output_item.done", json!({"output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "encrypted_content": "gAAA"}})),
            ev("response.output_item.added", json!({"output_index": 1, "item": {"type": "message", "id": "msg_1"}})),
            ev("response.content_part.added", json!({"output_index": 1, "content_index": 0, "part": {"type": "output_text", "text": ""}})),
            ev("response.output_text.delta", json!({"output_index": 1, "content_index": 0, "delta": "好"})),
            ev("response.output_text.done", json!({"output_index": 1, "content_index": 0, "text": "好"})),
            ev("response.output_item.done", json!({"output_index": 1, "item": {"type": "message"}})),
            ev("response.output_item.added", json!({"output_index": 2, "item": {"type": "function_call", "call_id": "call_1", "name": "shell", "arguments": ""}})),
            ev("response.function_call_arguments.delta", json!({"output_index": 2, "delta": "{\"command\":"})),
            ev("response.function_call_arguments.delta", json!({"output_index": 2, "delta": "[\"ls\"]}"})),
            ev("response.output_item.done", json!({"output_index": 2, "item": {"type": "function_call", "arguments": "{\"command\":[\"ls\"]}"}})),
            ev("response.completed", json!({"response": {"usage": {"input_tokens": 100, "input_tokens_details": {"cached_tokens": 80}, "output_tokens": 20}}})),
        ]
        .concat();
        let events = parse(&s);
        let thinking: String = events
            .iter()
            .filter_map(|e| match e {
                Event::Delta {
                    delta: Delta::Thinking(t),
                    ..
                } => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, "一\n\n二");
        assert!(events.contains(&Event::Delta {
            index: 0,
            delta: Delta::Signature(Signature::new(Vendor::OpenAi, "rs_1:gAAA"))
        }));
        let args: String = events
            .iter()
            .filter_map(|e| match e {
                Event::Delta {
                    delta: Delta::ToolInput(t),
                    ..
                } => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(args, "{\"command\":[\"ls\"]}", "完成时不能再把全文追加一遍");
        let starts = events
            .iter()
            .filter(|e| matches!(e, Event::BlockStart { .. }))
            .count();
        let stops = events
            .iter()
            .filter(|e| matches!(e, Event::BlockStop { .. }))
            .count();
        assert_eq!((starts, stops), (3, 3));
        assert!(events.contains(&Event::Stop(StopReason::ToolUse)));
        assert!(events.contains(&Event::Usage(Usage {
            input: 20,
            cache_read: 80,
            output: 20,
            ..Default::default()
        })));
    }

    fn written(events: &[Event], s: &Session) -> Vec<(String, Value)> {
        let mut w = Writer::new(s);
        let mut out = String::new();
        for e in events {
            out.push_str(&w.event(e));
        }
        out.push_str(&w.finish());
        assert!(w.finish().is_empty());
        let mut d = Decoder::default();
        d.feed(out.as_bytes())
            .into_iter()
            .map(|f| (f.event.unwrap(), serde_json::from_str(&f.data).unwrap()))
            .collect()
    }

    #[test]
    fn a_claude_stream_written_for_codex_carries_complete_items() {
        let mut s = Session::for_test(Dialect::Responses, Dialect::Anthropic);
        s.freeform.insert("apply_patch".into());
        let events = [
            Event::Start {
                id: Some("msg_1".into()),
                model: Some("claude-opus-4-7".into()),
            },
            Event::BlockStart {
                index: 0,
                kind: BlockKind::Thinking,
            },
            Event::Delta {
                index: 0,
                delta: Delta::Thinking("想".into()),
            },
            Event::Delta {
                index: 0,
                delta: Delta::Signature(Signature::new(Vendor::Anthropic, "sig")),
            },
            Event::BlockStop { index: 0 },
            Event::BlockStart {
                index: 1,
                kind: BlockKind::ToolCall {
                    id: "toolu_1".into(),
                    name: "apply_patch".into(),
                },
            },
            Event::Delta {
                index: 1,
                delta: Delta::ToolInput("*** Begin".into()),
            },
            Event::BlockStop { index: 1 },
            Event::BlockStart {
                index: 2,
                kind: BlockKind::ToolCall {
                    id: "toolu_2".into(),
                    name: "shell".into(),
                },
            },
            Event::Delta {
                index: 2,
                delta: Delta::ToolInput("{\"command\":".into()),
            },
            Event::Delta {
                index: 2,
                delta: Delta::ToolInput("[\"ls\"]}".into()),
            },
            Event::BlockStop { index: 2 },
            Event::Stop(StopReason::ToolUse),
            Event::Usage(Usage {
                input: 10,
                cache_read: 5,
                output: 7,
                ..Default::default()
            }),
        ];
        let f = written(&events, &s);
        let seq: Vec<u64> = f
            .iter()
            .map(|(_, v)| v["sequence_number"].as_u64().unwrap())
            .collect();
        assert_eq!(seq, (0..f.len() as u64).collect::<Vec<_>>());
        let done: Vec<&Value> = f
            .iter()
            .filter(|(k, _)| k == "response.output_item.done")
            .map(|(_, v)| &v["item"])
            .collect();
        assert_eq!(done.len(), 3);
        assert_eq!(done[0]["encrypted_content"], "tw1.a.sig");
        assert_eq!(done[0]["summary"][0]["text"], "想");
        assert_eq!(done[1]["type"], "custom_tool_call");
        assert_eq!(done[1]["input"], "*** Begin");
        assert_eq!(done[2]["type"], "function_call");
        assert_eq!(done[2]["arguments"], "{\"command\":[\"ls\"]}");
        let (last, body) = f.last().unwrap();
        assert_eq!(last, "response.completed");
        assert_eq!(body["response"]["output"].as_array().unwrap().len(), 3);
        assert_eq!(body["response"]["usage"]["input_tokens"], 15);
        assert_eq!(body["response"]["model"], "claude-opus-4-7");
        assert!(
            body["response"]["id"]
                .as_str()
                .unwrap()
                .starts_with("resp_")
        );
    }

    #[test]
    fn running_out_of_tokens_ends_with_response_incomplete() {
        let s = Session::for_test(Dialect::Responses, Dialect::Chat);
        let f = written(
            &[
                Event::BlockStart {
                    index: 0,
                    kind: BlockKind::Text,
                },
                Event::Delta {
                    index: 0,
                    delta: Delta::Text("写到一半".into()),
                },
                Event::Stop(StopReason::MaxTokens),
            ],
            &s,
        );
        let kinds: Vec<&str> = f.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.incomplete"
            ]
        );
        assert_eq!(
            f[8].1["response"]["incomplete_details"]["reason"],
            "max_output_tokens"
        );
    }

    #[test]
    fn an_error_becomes_response_failed() {
        let s = Session::for_test(Dialect::Responses, Dialect::Gemini);
        let f = written(
            &[Event::Error {
                message: "配额用尽".into(),
            }],
            &s,
        );
        let (k, v) = f.last().unwrap();
        assert_eq!(k, "response.failed");
        assert_eq!(v["response"]["error"]["message"], "配额用尽");
    }
}
