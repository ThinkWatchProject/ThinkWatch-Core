//! OpenAI Chat Completions 的流 ⇄ 中间表示的事件。
//!
//! Chat 的流只有一种帧（`choices[].delta`），**没有块边界**：文本、推理和工具调用
//! 混在同一串增量里。解析时自己判断边界：换了一种内容就结束上一块。工具调用按
//! `index` 区分，OpenAI 总是一个调用发完再发下一个，所以新调用开始时结束上一个。

use std::collections::HashMap;

use serde_json::{Value, json};

use super::response::{completion_id, finish_reason, stop_reason, usage, usage_json};
use crate::convert::Session;
use crate::frame::{self, Frame};
use crate::ir::*;

// ───────────────────────────────────────────────────────── 解析

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Open {
    Text,
    Thinking,
    /// OpenAI 的工具调用序号
    Tool(u64),
}

#[derive(Debug, Default)]
pub struct Parser {
    started: bool,
    open: Option<(usize, Open)>,
    /// 已经开过的工具调用：OpenAI 的序号 → 块
    tools: HashMap<u64, usize>,
    next: usize,
    usage: Usage,
}

impl Parser {
    pub fn frame(&mut self, f: &Frame, out: &mut Vec<Event>) {
        let data = f.data.trim();
        if data == "[DONE]" {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return;
        };
        if let Some(e) = v.get("error") {
            out.push(Event::Error {
                message: str_of(e, "message").unwrap_or(data).to_string(),
            });
            return;
        }
        if !self.started {
            self.started = true;
            out.push(Event::Start {
                id: str_of(&v, "id").map(str::to_string),
                model: str_of(&v, "model").map(str::to_string),
            });
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            self.usage.merge(&usage(u));
            out.push(Event::Usage(self.usage));
        }
        let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else {
            return;
        };
        let delta = choice.get("delta").unwrap_or(&Value::Null);

        // DeepSeek 叫 reasoning_content，OpenRouter、vLLM 叫 reasoning
        if let Some(t) = str_of(delta, "reasoning_content")
            .or_else(|| str_of(delta, "reasoning"))
            .filter(|t| !t.is_empty())
        {
            let index = self.ensure(Open::Thinking, None, out);
            out.push(Event::Delta {
                index,
                delta: Delta::Thinking(t.to_string()),
            });
        }
        for key in ["content", "refusal"] {
            if let Some(t) = str_of(delta, key).filter(|t| !t.is_empty()) {
                let index = self.ensure(Open::Text, None, out);
                out.push(Event::Delta {
                    index,
                    delta: Delta::Text(t.to_string()),
                });
            }
        }
        for c in arr_of(delta, "tool_calls") {
            let k = u64_of(c, "index").unwrap_or(0);
            let f = c.get("function").unwrap_or(&Value::Null);
            let index = match self.tools.get(&k) {
                Some(&i) if self.open.map(|(o, _)| o) == Some(i) => i,
                // 已经结束的调用又来了片段：没有办法重新打开，丢掉
                Some(_) => continue,
                None => {
                    let i = self.ensure(
                        Open::Tool(k),
                        Some(BlockKind::ToolCall {
                            id: str_of(c, "id")
                                .map(str::to_string)
                                .unwrap_or_else(|| new_id("call_")),
                            name: str_of(f, "name").unwrap_or_default().to_string(),
                        }),
                        out,
                    );
                    self.tools.insert(k, i);
                    i
                }
            };
            if let Some(args) = str_of(f, "arguments").filter(|a| !a.is_empty()) {
                out.push(Event::Delta {
                    index,
                    delta: Delta::ToolInput(args.to_string()),
                });
            }
        }
        if let Some(s) = str_of(choice, "finish_reason") {
            out.push(Event::Stop(stop_reason(s)));
        }
    }

    /// 需要的那一块开着就用它，否则结束当前块、开一块新的
    fn ensure(&mut self, want: Open, kind: Option<BlockKind>, out: &mut Vec<Event>) -> usize {
        if let Some((i, o)) = self.open
            && o == want
        {
            return i;
        }
        self.close(out);
        let index = self.next;
        self.next += 1;
        let kind = kind.unwrap_or(match want {
            Open::Text => BlockKind::Text,
            _ => BlockKind::Thinking,
        });
        out.push(Event::BlockStart { index, kind });
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
    id: String,
    created: u64,
    model: String,
    include_usage: bool,
    started: bool,
    /// 中间表示的工具块 → Chat 的工具调用序号
    tools: HashMap<usize, usize>,
    usage: Option<Usage>,
    stop: Option<StopReason>,
    done: bool,
}

impl Writer {
    pub fn new(s: &Session) -> Writer {
        Writer {
            id: completion_id(None),
            created: unix_secs(),
            model: s.model.clone(),
            include_usage: s.shape.include_usage,
            started: false,
            tools: HashMap::new(),
            usage: None,
            stop: None,
            done: false,
        }
    }

    fn chunk(&self, delta: Value, finish: Value) -> String {
        frame::data(&json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish, "logprobs": null }],
        }))
    }

    fn start(&mut self, out: &mut String) {
        if !self.started {
            self.started = true;
            out.push_str(&self.chunk(json!({ "role": "assistant", "content": "" }), Value::Null));
        }
    }

    pub fn event(&mut self, e: &Event) -> String {
        let mut out = String::new();
        if self.done {
            return out;
        }
        match e {
            Event::Start { id, model } => {
                if !self.started {
                    self.id = completion_id(id.as_deref());
                    if let Some(m) = model {
                        self.model = m.clone();
                    }
                }
                self.start(&mut out);
            }
            Event::BlockStart { index, kind } => {
                self.start(&mut out);
                if let BlockKind::ToolCall { id, name } = kind {
                    let n = self.tools.len();
                    self.tools.insert(*index, n);
                    out.push_str(&self.chunk(
                        json!({ "tool_calls": [{
                            "index": n, "id": id, "type": "function",
                            "function": { "name": name, "arguments": "" },
                        }] }),
                        Value::Null,
                    ));
                }
            }
            Event::Delta { index, delta } => {
                self.start(&mut out);
                let d = match delta {
                    Delta::Text(t) => json!({ "content": t }),
                    Delta::Thinking(t) => json!({ "reasoning_content": t }),
                    Delta::Signature(_) => return out,
                    Delta::ToolInput(p) => match self.tools.get(index) {
                        Some(n) => {
                            json!({ "tool_calls": [{ "index": n, "function": { "arguments": p } }] })
                        }
                        None => return out,
                    },
                };
                out.push_str(&self.chunk(d, Value::Null));
            }
            Event::BlockStop { .. } => {}
            Event::Usage(u) => self.usage.get_or_insert_default().merge(u),
            Event::Stop(s) => self.stop = Some(s.clone()),
            Event::Error { message } => {
                self.done = true;
                out.push_str(&frame::data(&json!({
                    "error": { "message": message, "type": "server_error", "param": null, "code": null },
                })));
            }
        }
        out
    }

    /// 流结束。**幂等**
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if self.done {
            return out;
        }
        self.done = true;
        self.start(&mut out);
        let reason = finish_reason(
            self.stop.as_ref().unwrap_or(&StopReason::EndTurn),
            !self.tools.is_empty(),
        );
        out.push_str(&self.chunk(json!({}), json!(reason)));
        // 客户端没要用量块就不发：有的客户端见到空的 choices 会出错
        if self.include_usage {
            out.push_str(&frame::data(&json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.model,
                "choices": [],
                "usage": usage_json(&self.usage.unwrap_or_default()),
            })));
        }
        out.push_str("data: [DONE]\n\n");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Decoder;

    fn parse(chunks: &[&str]) -> Vec<Event> {
        let mut d = Decoder::default();
        let mut p = Parser::default();
        let mut out = Vec::new();
        for c in chunks {
            for f in d.feed(c.as_bytes()) {
                p.frame(&f, &mut out);
            }
        }
        p.finish(&mut out);
        out
    }

    #[test]
    fn reasoning_text_and_two_tool_calls_become_four_blocks() {
        let ev = parse(&[
            "data: {\"id\":\"chatcmpl-1\",\"model\":\"deepseek-reasoner\",\"choices\":[{\"delta\":{\"reasoning_content\":\"想\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"好\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"A\",\"arguments\":\"{\\\"x\\\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":1}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"function\":{\"name\":\"B\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":9,\"prompt_tokens_details\":{\"cached_tokens\":60}}}\n\n",
            "data: [DONE]\n\n",
        ]);
        let starts: Vec<&BlockKind> = ev
            .iter()
            .filter_map(|e| match e {
                Event::BlockStart { kind, .. } => Some(kind),
                _ => None,
            })
            .collect();
        assert_eq!(starts.len(), 4, "{ev:#?}");
        assert_eq!(
            starts[3],
            &BlockKind::ToolCall {
                id: "call_b".into(),
                name: "B".into()
            }
        );
        let stops = ev
            .iter()
            .filter(|e| matches!(e, Event::BlockStop { .. }))
            .count();
        assert_eq!(stops, 4);
        assert!(ev.contains(&Event::Stop(StopReason::ToolUse)));
        assert!(ev.contains(&Event::Usage(Usage {
            input: 40,
            cache_read: 60,
            output: 9,
            ..Default::default()
        })));
    }

    fn write(events: &[Event], include_usage: bool) -> Vec<Value> {
        let mut s = Session::for_test(Dialect::Chat, Dialect::Anthropic);
        s.shape.include_usage = include_usage;
        let mut w = Writer::new(&s);
        let mut out = String::new();
        for e in events {
            out.push_str(&w.event(e));
        }
        out.push_str(&w.finish());
        assert!(out.ends_with("data: [DONE]\n\n"), "{out}");
        let mut d = Decoder::default();
        d.feed(out.as_bytes())
            .into_iter()
            .filter(|f| f.data != "[DONE]")
            .map(|f| serde_json::from_str(&f.data).unwrap())
            .collect()
    }

    #[test]
    fn a_tool_call_is_written_as_an_indexed_delta_and_finishes_as_tool_calls() {
        let chunks = write(
            &[
                Event::Start {
                    id: Some("msg_1".into()),
                    model: Some("claude-opus-4-7".into()),
                },
                Event::BlockStart {
                    index: 3,
                    kind: BlockKind::Thinking,
                },
                Event::Delta {
                    index: 3,
                    delta: Delta::Thinking("嗯".into()),
                },
                Event::BlockStop { index: 3 },
                Event::BlockStart {
                    index: 5,
                    kind: BlockKind::ToolCall {
                        id: "toolu_1".into(),
                        name: "Read".into(),
                    },
                },
                Event::Delta {
                    index: 5,
                    delta: Delta::ToolInput("{\"p\":1}".into()),
                },
                Event::BlockStop { index: 5 },
                Event::Stop(StopReason::ToolUse),
                Event::Usage(Usage {
                    input: 10,
                    cache_read: 90,
                    output: 5,
                    ..Default::default()
                }),
            ],
            true,
        );
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        assert!(chunks[0]["id"].as_str().unwrap().starts_with("chatcmpl-"));
        assert_eq!(chunks[1]["choices"][0]["delta"]["reasoning_content"], "嗯");
        assert_eq!(
            chunks[2]["choices"][0]["delta"]["tool_calls"][0]["id"],
            "toolu_1"
        );
        assert_eq!(
            chunks[3]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"p\":1}"
        );
        assert_eq!(chunks[4]["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(chunks[5]["usage"]["prompt_tokens"], 100);
        assert_eq!(
            chunks[5]["usage"]["prompt_tokens_details"]["cached_tokens"],
            90
        );
    }

    #[test]
    fn no_usage_chunk_unless_the_client_asked_for_one() {
        let chunks = write(&[Event::Stop(StopReason::MaxTokens)], false);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[1]["choices"][0]["finish_reason"], "length");
    }
}
