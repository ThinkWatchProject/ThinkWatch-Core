//! 回答：存下来的上游原话读成一块一块。
//!
//! `body` 是回答它的那一家的原话：整包的 JSON、SSE 的流（Bedrock 的二进制帧在网关进门时
//! 已经转成了 SSE），或者 Gemini 不带 `alt=sse` 时那个逐步写出的 JSON 数组。按开头的字节
//! 分辨，不看请求要没要流：上游只给流、客户端要整包的时候，存下来的是流（和按正文找的
//! `search::text::answer` 同一个分法）。
//!
//! - **整包**和请求里的助手消息是同一个形状，按 [`super::read`] 读；
//! - **流**按 tw-dialect 各家的流解析器读成事件再拼回来 —— 网关转换格式时读的就是它们。
//!   解析器只认得各家之间对应得上的块，服务端工具的块、Gemini 的代码执行这些它跳过，
//!   这里在同一帧上另外认出来，记成 `other`，位置不乱。

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use tw_api::TranscriptPart;
use tw_dialect::frame::{self, Frame};
use tw_dialect::ir::{Block, BlockKind, Delta, Dialect, Event, ToolInput};

use super::read;

/// 读出来的回答。
#[derive(Debug, Default)]
pub(super) struct Answer {
    pub(super) parts: Vec<TranscriptPart>,
    /// 认出了这家格式的回答没有。存着东西却认不出来，是 `response_unreadable`
    pub(super) recognized: bool,
}

/// 读一份回答。`freeform` 是客户端定义成自由格式的工具：别的格式的上游把它们的原文包在
/// `{"input": …}` 里，拆出来，和客户端自己记下的一样。
pub(super) fn read(body: &[u8], upstream: Dialect, freeform: &HashSet<String>) -> Answer {
    let mut a = match body.iter().find(|b| !b.is_ascii_whitespace()) {
        None => Answer::default(),
        Some(b'{') => whole(body, upstream),
        Some(b'[') if upstream == Dialect::Gemini => gemini_array(body),
        Some(_) => stream(body, upstream),
    };
    tidy(&mut a.parts, upstream, freeform);
    a
}

/// 整包的回答。JSON 都解析不了的（截断了的）什么都读不出来
fn whole(body: &[u8], upstream: Dialect) -> Answer {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return Answer::default();
    };
    let mut pieces = Vec::new();
    // 认得出是这家的回答就算读懂了，哪怕里面什么都没有（被拦下的提示、空的回答）
    let says = |key: &str, prefix: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| s.starts_with(prefix))
    };
    let recognized = match upstream {
        Dialect::Anthropic => {
            if let Some(blocks) = v.get("content").and_then(Value::as_array) {
                read::anthropic_blocks(blocks, &mut pieces);
            }
            v.get("content").is_some() || says("type", "message")
        }
        Dialect::Chat => {
            if let Some(m) = v
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
            {
                pieces = read::chat_assistant(m);
            }
            v.get("choices").is_some() || says("object", "chat.completion")
        }
        Dialect::Responses => {
            for it in v
                .get("output")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                pieces.extend(read::responses_item(it).pieces);
            }
            v.get("output").is_some() || says("object", "response")
        }
        Dialect::Gemini => {
            if let Some(parts) = v
                .get("candidates")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("content"))
                .and_then(|c| c.get("parts"))
                .and_then(Value::as_array)
            {
                read::gemini_parts(parts, &mut pieces);
            }
            ["candidates", "promptFeedback", "usageMetadata"]
                .iter()
                .any(|k| v.get(*k).is_some())
        }
        // 客户端不说 Converse，没有它的消息读法：整包交给 tw-dialect 解
        Dialect::Bedrock => {
            if v.get("output").and_then(|o| o.get("message")).is_none() {
                return Answer::default();
            }
            let parts = tw_dialect::bedrock::decode_response(&v)
                .blocks
                .into_iter()
                .map(block)
                .collect();
            return Answer {
                parts,
                recognized: true,
            };
        }
    };
    Answer {
        parts: pieces.iter().map(super::part).collect(),
        recognized,
    }
}

fn block(b: Block) -> TranscriptPart {
    match b {
        Block::Text(text) => TranscriptPart::Text { text },
        Block::Thinking(t) => TranscriptPart::Thinking { text: t.text },
        Block::ToolCall(c) => TranscriptPart::ToolCall {
            id: c.id,
            name: c.name,
            input: match c.input {
                ToolInput::Text(s) => s,
                json => json.to_json_text(),
            },
        },
    }
}

/// 流里的事件拼回一块一块。块按第一次出现的先后排
#[derive(Default)]
struct Assembly {
    parts: Vec<TranscriptPart>,
    /// 块号 → 在 `parts` 里的位置
    at: HashMap<usize, usize>,
    recognized: bool,
}

impl Assembly {
    fn other(&mut self, label: &str) {
        self.push(TranscriptPart::Other {
            label: label.to_string(),
        });
    }

    /// 解析器不认、这里另外认出来的一块。**之后的字另起一块**：解析器那边的文字块可能还
    /// 开着（Gemini 每帧都往同一块里接），接回前面那块的话，字就跑到这一块前头去了
    fn push(&mut self, part: TranscriptPart) {
        self.recognized = true;
        self.parts.push(part);
        let parts = &self.parts;
        self.at.retain(|_, &mut i| {
            !matches!(
                parts[i],
                TranscriptPart::Text { .. } | TranscriptPart::Thinking { .. }
            )
        });
    }

    fn events(&mut self, events: Vec<Event>) {
        for e in events {
            self.event(e);
        }
    }

    fn event(&mut self, e: Event) {
        self.recognized = true;
        match e {
            Event::BlockStart { index, kind } => {
                self.at.insert(index, self.parts.len());
                self.parts.push(match kind {
                    BlockKind::Text => TranscriptPart::Text {
                        text: String::new(),
                    },
                    BlockKind::Thinking => TranscriptPart::Thinking {
                        text: String::new(),
                    },
                    BlockKind::ToolCall { id, name } => TranscriptPart::ToolCall {
                        id,
                        name,
                        input: String::new(),
                    },
                });
            }
            Event::Delta { index, delta } => {
                if let Some(&i) = self.at.get(&index)
                    && append(&mut self.parts[i], &delta)
                {
                    return;
                }
                // 没报开始的块（Bedrock 的文字和推理就不报）：第一段增量到了就算开了一块
                self.at.insert(index, self.parts.len());
                self.parts.push(match delta {
                    Delta::Text(text) => TranscriptPart::Text { text },
                    Delta::Thinking(text) => TranscriptPart::Thinking { text },
                    Delta::Signature(_) => TranscriptPart::Thinking {
                        text: String::new(),
                    },
                    Delta::ToolInput(input) => TranscriptPart::ToolCall {
                        id: String::new(),
                        name: String::new(),
                        input,
                    },
                });
            }
            // 用量、结束原因、上游在流里报的错：不是回答的内容。错误在会话详情里有
            Event::Start { .. }
            | Event::BlockStop { .. }
            | Event::Usage(_)
            | Event::Stop(_)
            | Event::Error { .. } => {}
        }
    }

    fn finish(self) -> Answer {
        Answer {
            parts: self.parts,
            recognized: self.recognized,
        }
    }
}

/// 一段增量接到它那一块上。块的种类对不上的接不上
fn append(part: &mut TranscriptPart, delta: &Delta) -> bool {
    match (part, delta) {
        (TranscriptPart::Text { text }, Delta::Text(d))
        | (TranscriptPart::Thinking { text }, Delta::Thinking(d))
        | (TranscriptPart::ToolCall { input: text, .. }, Delta::ToolInput(d)) => {
            text.push_str(d);
            true
        }
        // 签名不交出去：只有签名的推理块是一块空的推理
        (TranscriptPart::Thinking { .. }, Delta::Signature(_)) => true,
        _ => false,
    }
}

/// tw-dialect 各家的流解析器
enum Parser {
    Anthropic(tw_dialect::anthropic::stream::Parser),
    Chat(tw_dialect::chat::stream::Parser),
    Responses(tw_dialect::responses::stream::Parser),
    Gemini(tw_dialect::gemini::stream::Parser),
    Bedrock(tw_dialect::bedrock::stream::Parser),
}

impl Parser {
    fn new(upstream: Dialect) -> Parser {
        match upstream {
            Dialect::Anthropic => Parser::Anthropic(Default::default()),
            Dialect::Chat => Parser::Chat(Default::default()),
            Dialect::Responses => Parser::Responses(Default::default()),
            Dialect::Gemini => Parser::Gemini(Default::default()),
            Dialect::Bedrock => Parser::Bedrock(Default::default()),
        }
    }

    fn finish(&mut self, out: &mut Vec<Event>) {
        match self {
            Parser::Anthropic(_) => {}
            Parser::Chat(p) => p.finish(out),
            Parser::Responses(p) => p.finish(out),
            Parser::Gemini(p) => p.finish(out),
            Parser::Bedrock(p) => p.finish(out),
        }
    }
}

/// SSE 的流。**截断了的读到哪儿算哪儿**：最后半帧解析不了，就停在它前面
fn stream(body: &[u8], upstream: Dialect) -> Answer {
    let mut asm = Assembly::default();
    let mut parser = Parser::new(upstream);
    let mut rest = body;
    while !rest.is_empty() {
        // 整份都在手里，不必像收流时那样攒着：按帧的边界一段一段切
        let (raw, next) = match frame::frame_end(rest) {
            Some((end, sep)) => (&rest[..end], &rest[end + sep..]),
            None => (rest, &rest[rest.len()..]),
        };
        rest = next;
        let Some(f) = frame::parse(raw) else {
            continue;
        };
        let mut events = Vec::new();
        match &mut parser {
            Parser::Anthropic(p) => {
                if let Some(label) = anthropic_other(&f) {
                    asm.other(&label);
                }
                p.frame(&f, &mut events);
            }
            Parser::Responses(p) => {
                if let Some(label) = responses_other(&f) {
                    asm.other(&label);
                }
                p.frame(&f, &mut events);
            }
            Parser::Gemini(p) => {
                // 自己解析这一帧，解析器也用这一份：不解析两遍
                if let Ok(v) = serde_json::from_str::<Value>(&f.data) {
                    gemini_chunk(&v, p, &mut asm, &mut events);
                }
            }
            Parser::Chat(p) => p.frame(&f, &mut events),
            Parser::Bedrock(p) => p.frame(&f, &mut events),
        }
        asm.events(events);
    }
    let mut events = Vec::new();
    parser.finish(&mut events);
    asm.events(events);
    asm.finish()
}

/// 解析器跳过的块（服务端工具的调用和结果、MCP……）：在它开始的那一帧上认出来
fn anthropic_other(f: &Frame) -> Option<String> {
    if !f.data.contains("\"content_block_start\"") {
        return None;
    }
    let v: Value = serde_json::from_str(&f.data).ok()?;
    let kind = v.get("content_block")?.get("type")?.as_str()?;
    (!matches!(kind, "text" | "thinking" | "redacted_thinking" | "tool_use"))
        .then(|| kind.to_string())
}

/// 解析器跳过的输出项（托管工具的调用：web_search_call、image_generation_call……）
fn responses_other(f: &Frame) -> Option<String> {
    if !f.data.contains("\"response.output_item.added\"") {
        return None;
    }
    let v: Value = serde_json::from_str(&f.data).ok()?;
    let kind = v.get("item")?.get("type")?.as_str()?;
    (!matches!(
        kind,
        "message" | "reasoning" | "function_call" | "custom_tool_call"
    ))
    .then(|| kind.to_string())
}

/// Gemini 的一帧（数组里的一个元素）。解析器只认文字、推理和函数调用；图片和代码执行
/// 在这里认，排在这一帧的文字前面
fn gemini_chunk(
    v: &Value,
    p: &mut tw_dialect::gemini::stream::Parser,
    asm: &mut Assembly,
    events: &mut Vec<Event>,
) {
    let parts = v
        .get("candidates")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut pieces = Vec::new();
    read::gemini_parts(parts, &mut pieces);
    for piece in &pieces {
        if matches!(piece, read::Piece::Image { .. } | read::Piece::Other(_)) {
            asm.push(super::part(piece));
        }
    }
    p.chunk(v, events);
}

/// 不带 `alt=sse` 的 Gemini 流：一个逐步写出的 JSON 数组，一个元素是一帧。截断了的
/// 读到最后一个完整的元素
fn gemini_array(body: &[u8]) -> Answer {
    let mut asm = Assembly::default();
    let mut parser = tw_dialect::gemini::stream::Parser::default();
    let start = body
        .iter()
        .position(|b| *b == b'[')
        .map_or(body.len(), |i| i + 1);
    let mut rest = &body[start..];
    loop {
        let skip = rest
            .iter()
            .position(|b| !(b.is_ascii_whitespace() || *b == b','))
            .unwrap_or(rest.len());
        rest = &rest[skip..];
        if rest.first().is_none_or(|b| *b == b']') {
            break;
        }
        let mut one = serde_json::Deserializer::from_slice(rest).into_iter::<Value>();
        let Some(Ok(v)) = one.next() else {
            break;
        };
        let used = one.byte_offset();
        let mut events = Vec::new();
        gemini_chunk(&v, &mut parser, &mut asm, &mut events);
        asm.events(events);
        rest = &rest[used..];
    }
    let mut events = Vec::new();
    parser.finish(&mut events);
    asm.events(events);
    asm.finish()
}

/// 收拾一下：空的文字块不算；没有参数的函数调用是 `{}`；别的格式的上游包起来的自由格式
/// 原文拆出来（和网关转换时一样，见 tw-dialect 的 `Normalizer`）
fn tidy(parts: &mut Vec<TranscriptPart>, upstream: Dialect, freeform: &HashSet<String>) {
    parts.retain(|p| !matches!(p, TranscriptPart::Text { text } if text.is_empty()));
    for p in parts {
        let TranscriptPart::ToolCall { name, input, .. } = p else {
            continue;
        };
        if freeform.contains(name.as_str()) {
            if upstream != Dialect::Responses {
                *input = unwrap_freeform(input);
            }
        } else if input.trim().is_empty() {
            *input = "{}".into();
        }
    }
}

fn unwrap_freeform(json_text: &str) -> String {
    match serde_json::from_str::<Value>(json_text) {
        Ok(Value::Object(o)) => match o.get("input") {
            Some(Value::String(s)) => s.clone(),
            _ => json_text.to_string(),
        },
        Ok(Value::String(s)) => s,
        _ => json_text.to_string(),
    }
}
