//! Bedrock ConverseStream 流 ⇄ 事件。
//!
//! # 帧的边界在哪
//!
//! Converse 的流是 AWS eventstream 的二进制帧，不是 SSE。**拆帧和封帧都不在这里** ——
//! 那是传输层的事，和 SSE 平级:
//!
//! - 读:传输层拆出每个事件，交成一个 [`Frame`]，`:event-type` 头进 `event`，载荷 JSON 进 `data`
//! - 写:这里照常吐 SSE 帧文本(和另外四种方言一致)，传输层要发给 Bedrock 客户端时再封成二进制帧
//!
//! 这样这个 crate 仍然只依赖 serde,而"帧里装什么"和"帧怎么封"各归各的。
//!
//! # 和 Anthropic 同构
//!
//! 事件序列是一样的形状:`messageStart` / `contentBlockStart` / `contentBlockDelta` /
//! `contentBlockStop` / `messageStop` / `metadata`。一处差别:**文本块不发
//! `contentBlockStart`**，直接就是 delta，所以读的时候要替它补一个块开始。

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::convert::Session;
use crate::frame::{self, Frame};
use crate::ir::*;

use super::response::{stop_reason, stop_reason_str, usage, usage_json};

// ───────────────────────────────────────────────────────── 读

#[derive(Default)]
pub struct Parser {
    /// 已经报过 `BlockStart` 的块。**文本块没有 `contentBlockStart`**，
    /// 要在第一个 delta 到达时替它补一个
    open: HashMap<usize, ()>,
}

impl Parser {
    pub fn frame(&mut self, f: &Frame, out: &mut Vec<Event>) {
        let Ok(v) = serde_json::from_str::<Value>(&f.data) else {
            return;
        };
        // 传输层把 `:event-type` 放进 event。没有的话退回看载荷的形状
        let kind = f.event.as_deref().unwrap_or_else(|| shape_of(&v));
        let index = |v: &Value| {
            v.get("contentBlockIndex")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize
        };

        match kind {
            "messageStart" => out.push(Event::Start {
                id: None,
                model: None,
            }),

            "contentBlockStart" => {
                let i = index(&v);
                if let Some(t) = v.get("start").and_then(|s| s.get("toolUse")) {
                    self.open.insert(i, ());
                    out.push(Event::BlockStart {
                        index: i,
                        kind: BlockKind::ToolCall {
                            id: t
                                .get("toolUseId")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| new_id("tooluse_")),
                            name: t
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        },
                    });
                }
            }

            "contentBlockDelta" => {
                let i = index(&v);
                let Some(d) = v.get("delta") else { return };

                if let Some(t) = d.get("text").and_then(Value::as_str) {
                    self.begin(i, BlockKind::Text, out);
                    out.push(Event::Delta {
                        index: i,
                        delta: Delta::Text(t.to_string()),
                    });
                } else if let Some(t) = d.get("toolUse").and_then(|u| u.get("input")) {
                    // 工具参数是 JSON 文本的片段，不是对象
                    let piece = t
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| t.to_string());
                    out.push(Event::Delta {
                        index: i,
                        delta: Delta::ToolInput(piece),
                    });
                } else if let Some(r) = d.get("reasoningContent") {
                    if let Some(t) = r.get("text").and_then(Value::as_str) {
                        self.begin(i, BlockKind::Thinking, out);
                        out.push(Event::Delta {
                            index: i,
                            delta: Delta::Thinking(t.to_string()),
                        });
                    }
                    // 签名单独来一帧，收尾时才到
                    if let Some(s) = r.get("signature").and_then(Value::as_str) {
                        self.begin(i, BlockKind::Thinking, out);
                        out.push(Event::Delta {
                            index: i,
                            delta: Delta::Signature(Signature {
                                vendor: Vendor::Anthropic,
                                value: s.to_string(),
                                redacted: false,
                            }),
                        });
                    }
                    if let Some(s) = r.get("redactedContent").and_then(Value::as_str) {
                        self.begin(i, BlockKind::Thinking, out);
                        out.push(Event::Delta {
                            index: i,
                            delta: Delta::Signature(Signature {
                                vendor: Vendor::Anthropic,
                                value: s.to_string(),
                                redacted: true,
                            }),
                        });
                    }
                }
            }

            "contentBlockStop" => {
                let i = index(&v);
                if self.open.remove(&i).is_some() {
                    out.push(Event::BlockStop { index: i });
                }
            }

            "messageStop" => {
                if let Some(s) = v.get("stopReason").and_then(Value::as_str) {
                    out.push(Event::Stop(stop_reason(s)));
                }
            }

            "metadata" => {
                if let Some(u) = v.get("usage") {
                    out.push(Event::Usage(usage(u)));
                }
            }

            // 流中途的异常事件。**它们是流的一部分，不是 HTTP 错误** ——
            // 头已经发出去了，出错只能在流里说
            "internalServerException"
            | "modelStreamErrorException"
            | "validationException"
            | "throttlingException"
            | "serviceUnavailableException" => {
                out.push(Event::Error {
                    message: v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("The upstream ended the stream with an error.")
                        .to_string(),
                });
            }

            _ => {}
        }
    }

    /// 补一个块开始。文本和推理块在 Converse 里没有 `contentBlockStart`
    fn begin(&mut self, index: usize, kind: BlockKind, out: &mut Vec<Event>) {
        if self.open.insert(index, ()).is_none() {
            out.push(Event::BlockStart { index, kind });
        }
    }

    pub fn finish(&mut self, out: &mut Vec<Event>) {
        let mut open: Vec<usize> = self.open.keys().copied().collect();
        open.sort_unstable();
        for i in open {
            out.push(Event::BlockStop { index: i });
        }
        self.open.clear();
    }
}

/// 传输层没给 `:event-type` 时，按载荷的形状认。
fn shape_of(v: &Value) -> &'static str {
    for (key, name) in [
        ("start", "contentBlockStart"),
        ("delta", "contentBlockDelta"),
        ("stopReason", "messageStop"),
        ("usage", "metadata"),
        ("role", "messageStart"),
    ] {
        if v.get(key).is_some() {
            return name;
        }
    }
    if v.get("contentBlockIndex").is_some() {
        return "contentBlockStop";
    }
    ""
}

// ───────────────────────────────────────────────────────── 写

pub struct Writer {
    usage: Usage,
    stop: Option<StopReason>,
    started: bool,
}

impl Writer {
    pub fn new(_s: &Session) -> Writer {
        Writer {
            usage: Usage::default(),
            stop: None,
            started: false,
        }
    }

    pub fn event(&mut self, e: &Event) -> String {
        match e {
            Event::Start { .. } => {
                self.started = true;
                frame::named("messageStart", &json!({ "role": "assistant" }))
            }
            Event::BlockStart { index, kind } => match kind {
                BlockKind::ToolCall { id, name } => frame::named(
                    "contentBlockStart",
                    &json!({
                        "contentBlockIndex": index,
                        "start": { "toolUse": { "toolUseId": id, "name": name } },
                    }),
                ),
                // 文本和推理块在 Converse 里不发块开始
                _ => String::new(),
            },
            Event::Delta { index, delta } => {
                let d = match delta {
                    Delta::Text(t) => json!({ "text": t }),
                    Delta::Thinking(t) => json!({ "reasoningContent": { "text": t } }),
                    Delta::Signature(s) if s.redacted => {
                        json!({ "reasoningContent": { "redactedContent": s.value } })
                    }
                    Delta::Signature(s) => {
                        json!({ "reasoningContent": { "signature": s.value } })
                    }
                    Delta::ToolInput(t) => json!({ "toolUse": { "input": t } }),
                };
                frame::named(
                    "contentBlockDelta",
                    &json!({ "contentBlockIndex": index, "delta": d }),
                )
            }
            Event::BlockStop { index } => {
                frame::named("contentBlockStop", &json!({ "contentBlockIndex": index }))
            }
            // 用量和停止原因攒到收尾:Converse 把它们放在最后两个事件里
            Event::Usage(u) => {
                self.usage.merge(u);
                String::new()
            }
            Event::Stop(s) => {
                self.stop = Some(s.clone());
                String::new()
            }
            Event::Error { message } => {
                frame::named("modelStreamErrorException", &json!({ "message": message }))
            }
        }
    }

    pub fn finish(&mut self) -> String {
        if !self.started {
            return String::new();
        }
        let mut out = frame::named(
            "messageStop",
            &json!({
                "stopReason": stop_reason_str(self.stop.as_ref().unwrap_or(&StopReason::EndTurn)),
            }),
        );
        out.push_str(&frame::named(
            "metadata",
            &json!({ "usage": usage_json(&self.usage) }),
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(frames: &[(&str, Value)]) -> Vec<Event> {
        let mut p = Parser::default();
        let mut out = Vec::new();
        for (event, data) in frames {
            p.frame(
                &Frame {
                    event: Some((*event).to_string()),
                    data: data.to_string(),
                },
                &mut out,
            );
        }
        p.finish(&mut out);
        out
    }

    #[test]
    fn a_text_block_gets_the_start_converse_never_sends() {
        // Converse 只给工具块发 contentBlockStart。文本直接就是 delta，
        // 而中间表示要求块先开始
        let events = feed(&[(
            "contentBlockDelta",
            json!({"contentBlockIndex": 0, "delta": {"text": "你好"}}),
        )]);
        assert!(
            matches!(
                events.first(),
                Some(Event::BlockStart {
                    index: 0,
                    kind: BlockKind::Text
                })
            ),
            "第一个事件应该是补出来的块开始：{events:?}"
        );
    }

    #[test]
    fn a_text_block_only_starts_once() {
        let events = feed(&[
            (
                "contentBlockDelta",
                json!({"contentBlockIndex": 0, "delta": {"text": "你"}}),
            ),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex": 0, "delta": {"text": "好"}}),
            ),
        ]);
        let starts = events
            .iter()
            .filter(|e| matches!(e, Event::BlockStart { .. }))
            .count();
        assert_eq!(starts, 1, "{events:?}");
    }

    #[test]
    fn a_tool_call_keeps_its_id_and_name_from_the_start_event() {
        let events = feed(&[
            (
                "contentBlockStart",
                json!({"contentBlockIndex": 1, "start": {"toolUse": {"toolUseId": "tu_1", "name": "get_weather"}}}),
            ),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": "{\"city\":"}}}),
            ),
        ]);
        assert!(
            matches!(
                &events[0],
                Event::BlockStart { index: 1, kind: BlockKind::ToolCall { id, name } }
                    if id == "tu_1" && name == "get_weather"
            ),
            "{events:?}"
        );
        assert!(
            matches!(
                &events[1],
                Event::Delta { index: 1, delta: Delta::ToolInput(s) } if s == "{\"city\":"
            ),
            "{events:?}"
        );
    }

    #[test]
    fn the_metadata_frame_is_where_usage_comes_from() {
        let events = feed(&[(
            "metadata",
            json!({"usage": {"inputTokens": 60, "cacheReadInputTokens": 40, "outputTokens": 20}}),
        )]);
        let Some(Event::Usage(u)) = events.first() else {
            panic!("没有用量：{events:?}");
        };
        assert_eq!((u.input, u.cache_read, u.output), (60, 40, 20));
    }

    #[test]
    fn an_exception_frame_becomes_an_error_event_not_silence() {
        // 头已经发出去了，出错只能在流里说。吞掉它客户端就只看到流突然停了
        let events = feed(&[(
            "throttlingException",
            json!({"message": "Too many requests"}),
        )]);
        assert!(
            matches!(events.first(), Some(Event::Error { message }) if message == "Too many requests"),
            "{events:?}"
        );
    }

    #[test]
    fn an_unfinished_block_is_closed_when_the_stream_ends() {
        let events = feed(&[(
            "contentBlockDelta",
            json!({"contentBlockIndex": 0, "delta": {"text": "半句"}}),
        )]);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::BlockStop { index: 0 })),
            "流结束时没收尾：{events:?}"
        );
    }
}
