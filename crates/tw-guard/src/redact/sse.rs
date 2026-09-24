//! 在流里把占位符换回来。
//!
//! # 为什么不能在原始字节上做
//!
//! ```text
//! data: {"delta":{"text":"<<"}}
//! data: {"delta":{"text":"TW_SEC"}}
//! data: {"delta":{"text":"RET_1>>"}}
//! ```
//!
//! 占位符在**解码后的正文里**是连续的，在线路上不是 —— 中间隔着
//! `"}}\n\ndata: {"delta":{"text":"`。模型按 token 吐字，一个占位符会被切成
//! 五到八段，这不是边角情况，是常态。所以要拆出每一帧，找到装正文的那个字段，
//! 解码之后再还原。
//!
//! # 按格式、按路
//!
//! **一条流里不止一路正文。**Anthropic 一个内容块一路，工具参数又是一路；
//! Chat 的正文和每个工具调用的参数各是一路。扣住的尾巴只能接回它自己那一路：
//! 正文块末尾扣住的 `<<TW_`，要是被接到下一个工具参数分片的开头，工具参数就
//! 坏了。
//!
//! 一路结束的时候（内容块的 `content_block_stop`、Chat 的 `finish_reason`、
//! Responses 的 `*.done`），扣住的尾巴要**在那一帧之前**补成一帧发出去，形状
//! 照着那一路自己的格式 —— 补一帧 Anthropic 的 delta 给 Chat 客户端，客户端
//! 不认识它，那几个字就丢了。
//!
//! **工具参数也要还原。**用户说「把我的 key 写进 .env」时，占位符会从工具参数
//! 流过去 —— 不还原的话，客户端真的会把占位符写进文件。
//!
//! **思考过程不碰。**Anthropic 的 thinking 带签名，改一个字，下一轮请求就会
//! 被上游拒绝。
//!
//! 两层：[`FrameRestorer`] 在解析好的一帧上干活，给已经自己拆好帧的调用方用；
//! [`SseRestorer`] 在字节流上拆帧，**看不懂的帧原样转发** —— 心跳、`ping`、
//! 没见过的事件类型，这条路上任何「看不懂就重写一下」都是在拿用户的流冒险。

use std::collections::BTreeMap;

use serde_json::{Value, json};
use tw_dialect::ir::Dialect;

use crate::redact::replace::Ledger;
use crate::redact::stream::Restorer;

/// 这几个键下面的东西不还原。思考过程和它的签名（改了下一轮就被拒），以及
/// 装 base64 的字段（里面一段数字恰好像占位符的机会不大，但换了的话坏的是
/// 一张图）。
const UNTOUCHED: &[&str] = &[
    "thinking",
    "signature",
    "thoughtSignature",
    "encrypted_content",
    "data",
    "bytes",
];

/// 流里的一路正文。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Lane {
    /// 正文。Anthropic 是内容块的 index，Responses 是 `output_index` 和
    /// `content_index` 拼起来的，Chat 和 Gemini 只有一路
    Text(u64),
    /// 工具参数。Anthropic 是内容块的 index，Chat 是工具调用的 index，
    /// Responses 是 `output_index`
    Args(u64),
}

impl Lane {
    fn output_index(self) -> u64 {
        match self {
            Lane::Text(k) => k >> 32,
            Lane::Args(k) => k,
        }
    }
}

/// 一帧里的一处正文：哪一路、在哪个字段。
struct Field {
    lane: Lane,
    path: Vec<Seg>,
}

#[derive(Debug, Clone)]
enum Seg {
    Key(&'static str),
    Idx(usize),
}

fn at<'a>(v: &'a mut Value, path: &[Seg]) -> Option<&'a mut String> {
    let mut cur = v;
    for s in path {
        cur = match (s, cur) {
            (Seg::Key(k), Value::Object(m)) => m.get_mut(*k)?,
            (Seg::Idx(i), Value::Array(a)) => a.get_mut(*i)?,
            _ => return None,
        };
    }
    match cur {
        Value::String(s) => Some(s),
        _ => None,
    }
}

/// 哪几路在这一帧结束。
enum Close {
    None,
    Lanes(Vec<Lane>),
    /// Responses 的 `output_item.done`：这一项的所有路
    Output(u64),
    All,
}

/// 补出来的一帧，要**先于**它前面那一帧发出去。
#[derive(Debug, Clone, PartialEq)]
pub struct Synth {
    pub event: Option<String>,
    pub data: Value,
}

/// 一帧改写的结果。
#[derive(Debug, Default)]
pub struct Rewritten {
    /// 这一帧本身改了没有。没改的话调用方可以原样转发它的字节
    pub changed: bool,
    /// 要先于这一帧发出去的补帧
    pub before: Vec<Synth>,
}

struct Open {
    restorer: Restorer,
    /// 这一路最近的一帧。补帧时从它身上抄 id、index 这些
    last: Value,
}

/// 一条流上、按帧的还原器。
pub struct FrameRestorer {
    dialect: Dialect,
    ledger: Ledger,
    oneshot: Restorer,
    lanes: BTreeMap<Lane, Open>,
}

impl FrameRestorer {
    /// `dialect` 是**这条流**的格式：还原放在转换之前就是上游的格式，放在转换
    /// 之后就是客户端的格式。
    pub fn new(ledger: &Ledger, dialect: Dialect) -> Self {
        Self {
            dialect,
            ledger: ledger.clone(),
            oneshot: Restorer::new(ledger),
            lanes: BTreeMap::new(),
        }
    }

    /// 没东西要还原。**调用方据此整条短路。**
    pub fn is_noop(&self) -> bool {
        self.ledger.is_empty()
    }

    /// 改写一帧。帧本身就地改，返回要先于它发出去的补帧。
    pub fn frame(&mut self, v: &mut Value) -> Rewritten {
        let mut out = Rewritten::default();
        if self.is_noop() {
            return out;
        }
        // 完整的占位符，不管在哪个字段里：`output_text.done` 带着整段正文、
        // Gemini 的函数调用参数整个在一帧里
        out.changed |= walk(v, &self.oneshot);
        // 流着的正文，按路扣住半截的
        let fields = fields(self.dialect, v);
        for f in &fields {
            let open = self.lanes.entry(f.lane).or_insert_with(|| Open {
                restorer: Restorer::new(&self.ledger),
                last: Value::Null,
            });
            open.last = v.clone();
            if let Some(s) = at(v, &f.path) {
                let next = open.restorer.process(s);
                out.changed |= next != *s;
                *s = next;
            }
        }
        // 这一帧结束了哪几路：扣住的尾巴在它之前补出来。那一路的正文就在
        // 这一帧里的话（Gemini 最后一帧常常既有正文又有 finishReason），直接
        // 接在那个字段后面
        let closing: Vec<Lane> = match close(self.dialect, v) {
            Close::None => Vec::new(),
            Close::Lanes(l) => l,
            Close::Output(oi) => self
                .lanes
                .keys()
                .copied()
                .filter(|l| l.output_index() == oi)
                .collect(),
            Close::All => self.lanes.keys().copied().collect(),
        };
        for lane in closing {
            let Some(mut open) = self.lanes.remove(&lane) else {
                continue;
            };
            let tail = open.restorer.flush();
            if tail.is_empty() {
                continue;
            }
            match fields.iter().rev().find(|f| f.lane == lane) {
                Some(f) => {
                    if let Some(s) = at(v, &f.path) {
                        s.push_str(&tail);
                        out.changed = true;
                    }
                }
                None => out.before.push(synth(self.dialect, lane, &open.last, tail)),
            }
        }
        out
    }

    /// 碰到不是 JSON 的帧（`[DONE]`），或者流结束了：所有扣住的尾巴补成帧。
    ///
    /// **丢掉它就是丢掉模型说过的字**，而那比多一帧难看得多。
    pub fn drain(&mut self) -> Vec<Synth> {
        let lanes = std::mem::take(&mut self.lanes);
        lanes
            .into_iter()
            .filter_map(|(lane, mut open)| {
                let tail = open.restorer.flush();
                (!tail.is_empty()).then(|| synth(self.dialect, lane, &open.last, tail))
            })
            .collect()
    }
}

/// 在一帧的每个字符串上做一次性还原，跳过 [`UNTOUCHED`] 下面的。返回改了没有。
fn walk(v: &mut Value, r: &Restorer) -> bool {
    match v {
        Value::String(s) => {
            let next = r.oneshot(s);
            let changed = next != *s;
            *s = next;
            changed
        }
        Value::Array(items) => items.iter_mut().fold(false, |c, i| walk(i, r) | c),
        Value::Object(m) => m
            .iter_mut()
            .filter(|(k, _)| !UNTOUCHED.contains(&k.as_str()))
            .fold(false, |c, (_, i)| walk(i, r) | c),
        _ => false,
    }
}

fn kind(v: &Value) -> Option<&str> {
    v.get("type").and_then(Value::as_str)
}

fn u(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

/// 一帧里流着的正文在哪几个字段。
fn fields(d: Dialect, v: &Value) -> Vec<Field> {
    use Seg::{Idx, Key};
    let mut out = Vec::new();
    match d {
        Dialect::Anthropic => {
            if kind(v) == Some("content_block_delta") {
                let i = u(v, "index");
                match v.pointer("/delta/type").and_then(Value::as_str) {
                    Some("text_delta") => out.push(Field {
                        lane: Lane::Text(i),
                        path: vec![Key("delta"), Key("text")],
                    }),
                    Some("input_json_delta") => out.push(Field {
                        lane: Lane::Args(i),
                        path: vec![Key("delta"), Key("partial_json")],
                    }),
                    _ => {}
                }
            }
        }
        Dialect::Chat => {
            let Some(delta) = v.pointer("/choices/0/delta") else {
                return out;
            };
            if delta.get("content").is_some_and(Value::is_string) {
                out.push(Field {
                    lane: Lane::Text(0),
                    path: vec![Key("choices"), Idx(0), Key("delta"), Key("content")],
                });
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for (k, c) in calls.iter().enumerate() {
                    if c.pointer("/function/arguments")
                        .is_some_and(Value::is_string)
                    {
                        let i = c.get("index").and_then(Value::as_u64).unwrap_or(k as u64);
                        out.push(Field {
                            lane: Lane::Args(i),
                            path: vec![
                                Key("choices"),
                                Idx(0),
                                Key("delta"),
                                Key("tool_calls"),
                                Idx(k),
                                Key("function"),
                                Key("arguments"),
                            ],
                        });
                    }
                }
            }
        }
        Dialect::Responses => match kind(v) {
            Some("response.output_text.delta") => out.push(Field {
                lane: Lane::Text(u(v, "output_index") << 32 | u(v, "content_index")),
                path: vec![Key("delta")],
            }),
            Some("response.function_call_arguments.delta") => out.push(Field {
                lane: Lane::Args(u(v, "output_index")),
                path: vec![Key("delta")],
            }),
            _ => {}
        },
        Dialect::Gemini => {
            let Some(parts) = v
                .pointer("/candidates/0/content/parts")
                .and_then(Value::as_array)
            else {
                return out;
            };
            for (j, p) in parts.iter().enumerate() {
                let thought = p.get("thought").and_then(Value::as_bool) == Some(true);
                if !thought && p.get("text").is_some_and(Value::is_string) {
                    out.push(Field {
                        lane: Lane::Text(0),
                        path: vec![
                            Key("candidates"),
                            Idx(0),
                            Key("content"),
                            Key("parts"),
                            Idx(j),
                            Key("text"),
                        ],
                    });
                }
            }
        }
        // 从来不是任何一边要还原的流：客户端不说它，桌面版也不接它。
        // 完整的占位符照样由 `walk` 还原
        Dialect::Bedrock => {}
    }
    out
}

fn close(d: Dialect, v: &Value) -> Close {
    match d {
        Dialect::Anthropic => match kind(v) {
            Some("content_block_stop") => {
                let i = u(v, "index");
                Close::Lanes(vec![Lane::Text(i), Lane::Args(i)])
            }
            Some("message_stop") | Some("error") => Close::All,
            _ => Close::None,
        },
        Dialect::Chat => {
            let done = v
                .pointer("/choices/0/finish_reason")
                .is_some_and(|f| !f.is_null());
            if done { Close::All } else { Close::None }
        }
        Dialect::Responses => match kind(v) {
            Some("response.output_text.done") => Close::Lanes(vec![Lane::Text(
                u(v, "output_index") << 32 | u(v, "content_index"),
            )]),
            Some("response.function_call_arguments.done") => {
                Close::Lanes(vec![Lane::Args(u(v, "output_index"))])
            }
            Some("response.output_item.done") => Close::Output(u(v, "output_index")),
            Some("response.completed")
            | Some("response.incomplete")
            | Some("response.failed")
            | Some("error") => Close::All,
            _ => Close::None,
        },
        Dialect::Gemini => {
            let done = v
                .pointer("/candidates/0/finishReason")
                .is_some_and(|f| !f.is_null());
            if done { Close::All } else { Close::None }
        }
        Dialect::Bedrock => Close::None,
    }
}

/// 一路扣住的尾巴补成一帧，形状照着那一路自己的格式。
fn synth(d: Dialect, lane: Lane, last: &Value, tail: String) -> Synth {
    let copy = |keys: &[&str]| {
        let mut m = serde_json::Map::new();
        for k in keys {
            if let Some(x) = last.get(*k) {
                m.insert(k.to_string(), x.clone());
            }
        }
        m
    };
    match (d, lane) {
        (Dialect::Anthropic, Lane::Text(i)) => Synth {
            event: Some("content_block_delta".into()),
            data: json!({
                "type": "content_block_delta",
                "index": i,
                "delta": { "type": "text_delta", "text": tail },
            }),
        },
        (Dialect::Anthropic, Lane::Args(i)) => Synth {
            event: Some("content_block_delta".into()),
            data: json!({
                "type": "content_block_delta",
                "index": i,
                "delta": { "type": "input_json_delta", "partial_json": tail },
            }),
        },
        (Dialect::Chat, lane) => {
            let delta = match lane {
                Lane::Text(_) => json!({ "content": tail }),
                Lane::Args(i) => json!({
                    "tool_calls": [{ "index": i, "function": { "arguments": tail } }],
                }),
            };
            let mut m = copy(&["id", "object", "created", "model", "system_fingerprint"]);
            m.insert(
                "choices".into(),
                json!([{ "index": 0, "delta": delta, "finish_reason": null }]),
            );
            Synth {
                event: None,
                data: Value::Object(m),
            }
        }
        (Dialect::Responses, Lane::Text(k)) => Synth {
            event: Some("response.output_text.delta".into()),
            data: json!({
                "type": "response.output_text.delta",
                "item_id": last.get("item_id").cloned().unwrap_or(Value::Null),
                "output_index": k >> 32,
                "content_index": k & 0xffff_ffff,
                "delta": tail,
            }),
        },
        (Dialect::Responses, Lane::Args(oi)) => Synth {
            event: Some("response.function_call_arguments.delta".into()),
            data: json!({
                "type": "response.function_call_arguments.delta",
                "item_id": last.get("item_id").cloned().unwrap_or(Value::Null),
                "output_index": oi,
                "delta": tail,
            }),
        },
        (Dialect::Gemini, _) => {
            let mut m = copy(&["modelVersion", "responseId"]);
            m.insert(
                "candidates".into(),
                json!([{ "content": { "role": "model", "parts": [{ "text": tail }] }, "index": 0 }]),
            );
            Synth {
                event: None,
                data: Value::Object(m),
            }
        }
        // `fields` 从不给 Bedrock 开路，走不到这里
        (Dialect::Bedrock, _) => Synth {
            event: None,
            data: Value::String(tail),
        },
    }
}

/// 补帧写成 SSE。
pub fn write(s: &Synth) -> String {
    let data = serde_json::to_string(&s.data).unwrap_or_default();
    match &s.event {
        Some(e) => format!("event: {e}\ndata: {data}\n\n"),
        None => format!("data: {data}\n\n"),
    }
}

/// 一条 SSE 字节流上的还原器。
pub struct SseRestorer {
    frames: FrameRestorer,
    /// 还没收齐的那一帧
    partial: Vec<u8>,
}

impl SseRestorer {
    pub fn new(ledger: &Ledger, dialect: Dialect) -> Self {
        Self {
            frames: FrameRestorer::new(ledger, dialect),
            partial: Vec::new(),
        }
    }
    pub fn is_noop(&self) -> bool {
        self.frames.is_noop()
    }

    pub fn process(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.is_noop() {
            return chunk.to_vec();
        }
        self.partial.extend_from_slice(chunk);
        let mut out = Vec::new();
        // 帧之间用空行分隔。**收不齐就等下一块** —— 半帧转发出去，
        // 客户端那边会当成一帧解析失败
        while let Some((n, sep)) = tw_dialect::frame::frame_end(&self.partial) {
            let frame: Vec<u8> = self.partial.drain(..n + sep).collect();
            out.extend_from_slice(&self.rewrite(&frame));
        }
        out
    }

    /// 流结束了：没收尾的最后一帧照样改写，扣住的尾巴补成帧。
    pub fn flush(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.partial.is_empty() {
            let frame: Vec<u8> = std::mem::take(&mut self.partial);
            out.extend_from_slice(&self.rewrite(&frame));
        }
        for s in self.frames.drain() {
            out.extend_from_slice(write(&s).as_bytes());
        }
        out
    }

    /// 改写一帧。**看不懂就原样转发。**
    fn rewrite(&mut self, frame: &[u8]) -> Vec<u8> {
        let Ok(text) = std::str::from_utf8(frame) else {
            return frame.to_vec();
        };
        // 多行 `data:` 按规范用换行连起来
        if !text
            .lines()
            .any(|l| tw_dialect::frame::data_of(l).is_some())
        {
            return frame.to_vec();
        }
        let data = tw_dialect::frame::parse(frame)
            .map(|f| f.data)
            .unwrap_or_default();
        let Ok(mut v) = serde_json::from_str::<Value>(&data) else {
            // `[DONE]` 这类：流就要结束了，扣住的尾巴得赶在它前面
            let mut out: Vec<u8> = self
                .frames
                .drain()
                .iter()
                .flat_map(|s| write(s).into_bytes())
                .collect();
            out.extend_from_slice(frame);
            return out;
        };
        let r = self.frames.frame(&mut v);
        let mut out: Vec<u8> = r
            .before
            .iter()
            .flat_map(|s| write(s).into_bytes())
            .collect();
        if !r.changed {
            out.extend_from_slice(frame);
            return out;
        }
        // 只换 `data:` 那一行，别的行（`event:`、`id:`、注释）原样留着
        let json = serde_json::to_string(&v).unwrap_or_default();
        let mut wrote = false;
        for line in text.split_inclusive('\n') {
            if tw_dialect::frame::data_of(line.trim_end_matches(['\n', '\r'])).is_some() {
                if !wrote {
                    out.extend_from_slice(b"data: ");
                    out.extend_from_slice(json.as_bytes());
                    // 把这一行原来的换行接回去
                    out.extend_from_slice(
                        &line.as_bytes()[line.trim_end_matches(['\n', '\r']).len()..],
                    );
                    wrote = true;
                }
                continue;
            }
            out.extend_from_slice(line.as_bytes());
        }
        out
    }
}

/// 一条响应流上的还原器，按内容类型分派。
///
/// **SSE 和非流式是两件不同的事**：前者的占位符散落在几十帧里，后者
/// 整个躺在一份 JSON 里。合成一个「通用」的实现只会让两边都做不对 ——
/// 第一版就是那么写的，SSE 那半从来没还原成功过。
pub enum Body {
    Sse(Box<SseRestorer>),
    Whole(crate::redact::stream::ByteRestorer),
}

impl Body {
    pub fn new(ledger: &Ledger, is_sse: bool, dialect: Dialect) -> Self {
        if is_sse {
            Body::Sse(Box::new(SseRestorer::new(ledger, dialect)))
        } else {
            Body::Whole(crate::redact::stream::ByteRestorer::new(ledger))
        }
    }
    pub fn is_noop(&self) -> bool {
        match self {
            Body::Sse(r) => r.is_noop(),
            Body::Whole(r) => r.is_noop(),
        }
    }
    pub fn process(&mut self, chunk: &[u8]) -> Vec<u8> {
        match self {
            Body::Sse(r) => r.process(chunk),
            Body::Whole(r) => r.process(chunk),
        }
    }
    pub fn flush(&mut self) -> Vec<u8> {
        match self {
            Body::Sse(r) => r.flush(),
            Body::Whole(r) => r.flush(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redact::replace::{Scheme, redact};
    use crate::redact::rules::RuleSet;

    const KEY: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn ledger() -> Ledger {
        redact(
            &format!("k={KEY}"),
            &RuleSet::only(&["anthropic-api-key"]),
            Ledger::new(Scheme::SECRET),
        )
        .ledger
    }

    fn named(event: &str, v: Value) -> String {
        format!("event: {event}\ndata: {v}\n\n")
    }

    fn data(v: Value) -> String {
        format!("data: {v}\n\n")
    }

    fn delta(text: &str) -> String {
        delta_at(0, text)
    }

    fn delta_at(i: u64, text: &str) -> String {
        named(
            "content_block_delta",
            json!({"type":"content_block_delta","index":i,"delta":{"type":"text_delta","text":text}}),
        )
    }

    fn args_at(i: u64, s: &str) -> String {
        named(
            "content_block_delta",
            json!({"type":"content_block_delta","index":i,"delta":{"type":"input_json_delta","partial_json":s}}),
        )
    }

    fn stop(i: u64) -> String {
        named(
            "content_block_stop",
            json!({"type":"content_block_stop","index":i}),
        )
    }

    fn run(d: Dialect, frames: &[String]) -> String {
        let l = ledger();
        let mut r = SseRestorer::new(&l, d);
        let mut raw = Vec::new();
        for f in frames {
            raw.extend_from_slice(&r.process(f.as_bytes()));
        }
        raw.extend_from_slice(&r.flush());
        String::from_utf8(raw).unwrap()
    }

    fn values(out: &str) -> Vec<Value> {
        out.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|d| serde_json::from_str::<Value>(d).ok())
            .collect()
    }

    /// Anthropic：按内容块拼出 (index, 正文, 参数)
    fn blocks(out: &str) -> BTreeMap<u64, (String, String)> {
        let mut m: BTreeMap<u64, (String, String)> = BTreeMap::new();
        for v in values(out) {
            if v["type"] != "content_block_delta" {
                continue;
            }
            let e = m.entry(v["index"].as_u64().unwrap()).or_default();
            if let Some(s) = v["delta"]["text"].as_str() {
                e.0.push_str(s);
            }
            if let Some(s) = v["delta"]["partial_json"].as_str() {
                e.1.push_str(s);
            }
        }
        m
    }

    #[test]
    fn a_placeholder_spread_over_one_character_per_frame_comes_back_whole() {
        // **这是第一版当场失败的那个用例。**模型按 token 吐字，一个
        // 占位符会被切成五到八段 —— 不是边角情况，是常态。
        let frames: Vec<String> = "你的 key 是 <<TW_SECRET_1>> 对吗"
            .chars()
            .map(|c| delta(&c.to_string()))
            .collect();
        let out = run(Dialect::Anthropic, &frames);
        assert_eq!(blocks(&out)[&0].0, format!("你的 key 是 {KEY} 对吗"));
    }

    #[test]
    fn a_tool_call_argument_is_restored_too() {
        // **用户说「把我的 key 写进 .env」的时候，占位符从这里流过去。**
        // 不还原的话，客户端真的会把占位符写进文件。
        let frames: Vec<String> = [r#"{"content":"KEY=<<TW"#, "_SECRET_1", r#">>"}"#]
            .iter()
            .map(|s| args_at(1, s))
            .collect();
        let out = run(Dialect::Anthropic, &frames);
        assert_eq!(blocks(&out)[&1].1, format!(r#"{{"content":"KEY={KEY}"}}"#));
    }

    #[test]
    fn a_tail_held_at_the_end_of_a_block_stays_in_that_block() {
        // 正文块末尾扣住的 `<<TW_SEC`，以前会被接到下一个工具参数分片的开头 ——
        // 工具参数就坏了
        let frames = vec![
            delta_at(0, "好的 <<TW_SEC"),
            stop(0),
            args_at(1, r#"{"path":".env"}"#),
            stop(1),
        ];
        let out = run(Dialect::Anthropic, &frames);
        let b = blocks(&out);
        assert_eq!(b[&0].0, "好的 <<TW_SEC");
        assert_eq!(b[&1].1, r#"{"path":".env"}"#);
        // 而且补在 stop 之前
        let tail = out.find("<<TW_SEC").unwrap();
        assert!(tail < out.find("content_block_stop").unwrap(), "{out}");
    }

    #[test]
    fn frames_that_arrive_in_pieces_are_not_forwarded_half_written() {
        // 半帧转发出去，客户端那边会当成一帧解析失败。
        let l = ledger();
        let mut r = SseRestorer::new(&l, Dialect::Anthropic);
        let whole = delta("<<TW_SECRET_1>>");
        let bytes = whole.as_bytes();
        let mid = bytes.len() / 2;
        assert!(r.process(&bytes[..mid]).is_empty(), "半帧就发出去了");
        assert!(String::from_utf8_lossy(&r.process(&bytes[mid..])).contains(KEY));
    }

    #[test]
    fn frames_we_do_not_understand_pass_through_byte_for_byte() {
        // 心跳、ping、没见过的事件类型 —— 「看不懂就重写一下」是在拿
        // 用户的流冒险。
        let l = ledger();
        let mut r = SseRestorer::new(&l, Dialect::Anthropic);
        for raw in [
            ": 心跳\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: message_start\r\ndata:{\"type\":\"message_start\",\"message\":{\"id\":\"m\"}}\r\n\r\n",
            "data: [DONE]\n\n",
        ] {
            assert_eq!(
                String::from_utf8(r.process(raw.as_bytes())).unwrap(),
                raw,
                "改写了不该改的帧"
            );
        }
    }

    #[test]
    fn a_stream_that_ends_mid_placeholder_still_delivers_the_text() {
        // 丢掉它就是丢掉模型说过的字。补一帧比丢字难看得多，但正确。
        let out = run(Dialect::Anthropic, &[delta("结果是 <<TW_SECRET_")]);
        assert_eq!(blocks(&out)[&0].0, "结果是 <<TW_SECRET_");
    }

    #[test]
    fn ordinary_text_flows_without_being_held_back() {
        // 一帧进一帧出。没有占位符嫌疑的正文不该在我们这儿多待哪怕一帧。
        let l = ledger();
        let mut r = SseRestorer::new(&l, Dialect::Anthropic);
        for s in ["第一段", "第二段", "std::cout << x"] {
            let out = String::from_utf8(r.process(delta(s).as_bytes())).unwrap();
            assert!(out.contains(s), "「{s}」被扣住了：{out}");
        }
    }

    #[test]
    fn a_stream_with_nothing_to_restore_is_copied_straight_through() {
        // 绝大多数请求走这条路，它不该为这个功能付任何代价。
        let mut r = SseRestorer::new(&Ledger::new(Scheme::SECRET), Dialect::Anthropic);
        assert!(r.is_noop());
        let raw = delta("<<TW_SECRET_1>>");
        assert_eq!(String::from_utf8(r.process(raw.as_bytes())).unwrap(), raw);
    }

    #[test]
    fn thinking_is_left_exactly_as_the_upstream_signed_it() {
        // 改一个字，下一轮请求就被上游拒绝
        let raw = named(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"<<TW_SECRET_1>>"}}),
        );
        assert_eq!(run(Dialect::Anthropic, std::slice::from_ref(&raw)), raw);
    }

    fn chat(delta: Value, finish: Value) -> String {
        data(json!({
            "id":"c1","object":"chat.completion.chunk","created":1,"model":"m",
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}],
        }))
    }

    #[test]
    fn a_chat_stream_gets_its_text_and_its_tool_arguments_back() {
        let frames = vec![
            chat(json!({"content":"key 是 <<TW"}), Value::Null),
            chat(json!({"content":"_SECRET_1>>"}), Value::Null),
            chat(
                json!({"tool_calls":[{"index":0,"id":"t","function":{"name":"write","arguments":"{\"v\":\"<<TW_SE"}}]}),
                Value::Null,
            ),
            chat(
                json!({"tool_calls":[{"index":0,"function":{"arguments":"CRET_1>>\"}"}}]}),
                Value::Null,
            ),
            chat(json!({}), json!("tool_calls")),
            "data: [DONE]\n\n".to_string(),
        ];
        let out = run(Dialect::Chat, &frames);
        let vs = values(&out);
        let text: String = vs
            .iter()
            .filter_map(|v| v["choices"][0]["delta"]["content"].as_str())
            .collect();
        let args: String = vs
            .iter()
            .filter_map(|v| {
                v["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str()
            })
            .collect();
        assert_eq!(text, format!("key 是 {KEY}"));
        assert_eq!(args, format!(r#"{{"v":"{KEY}"}}"#));
    }

    #[test]
    fn a_chat_tail_is_written_as_a_chat_chunk() {
        // 以前补的一律是 Anthropic 的 delta —— Chat 客户端不认识它，那几个字就丢了
        let frames = vec![
            chat(json!({"content":"结果是 <<TW_SEC"}), Value::Null),
            chat(json!({}), json!("stop")),
        ];
        let out = run(Dialect::Chat, &frames);
        let vs = values(&out);
        let text: String = vs
            .iter()
            .filter_map(|v| v["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert_eq!(text, "结果是 <<TW_SEC");
        assert!(!out.contains("content_block_delta"), "{out}");
        // 补的那一帧带着这条流自己的 id 和模型名，而且在结束那一帧之前
        let synth = vs
            .iter()
            .position(|v| v["choices"][0]["delta"]["content"] == "<<TW_SEC");
        let finish = vs
            .iter()
            .position(|v| v["choices"][0]["finish_reason"] == "stop");
        assert!(synth.unwrap() < finish.unwrap(), "{out}");
        assert_eq!(vs[synth.unwrap()]["id"], "c1");
    }

    #[test]
    fn a_responses_stream_gets_text_arguments_and_the_done_frames_back() {
        let ev = |t: &str, v: Value| named(t, v);
        let frames = vec![
            ev(
                "response.output_text.delta",
                json!({"type":"response.output_text.delta","item_id":"m1","output_index":0,"content_index":0,"delta":"是 <<TW_SE"}),
            ),
            ev(
                "response.output_text.delta",
                json!({"type":"response.output_text.delta","item_id":"m1","output_index":0,"content_index":0,"delta":"CRET_1>> 吗"}),
            ),
            ev(
                "response.output_text.done",
                json!({"type":"response.output_text.done","item_id":"m1","output_index":0,"content_index":0,"text":"是 <<TW_SECRET_1>> 吗"}),
            ),
            ev(
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","item_id":"f1","output_index":1,"delta":"{\"k\":\"<<TW_SECRET_1"}),
            ),
            ev(
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","item_id":"f1","output_index":1,"delta":">>\"}"}),
            ),
            ev(
                "response.function_call_arguments.done",
                json!({"type":"response.function_call_arguments.done","item_id":"f1","output_index":1,"arguments":"{\"k\":\"<<TW_SECRET_1>>\"}"}),
            ),
        ];
        let out = run(Dialect::Responses, &frames);
        let vs = values(&out);
        let joined = |t: &str| -> String {
            vs.iter()
                .filter(|v| v["type"] == t)
                .filter_map(|v| v["delta"].as_str())
                .collect()
        };
        assert_eq!(joined("response.output_text.delta"), format!("是 {KEY} 吗"));
        assert_eq!(
            joined("response.function_call_arguments.delta"),
            format!(r#"{{"k":"{KEY}"}}"#)
        );
        assert!(!out.contains("<<TW_SECRET_1>>"), "{out}");
    }

    #[test]
    fn a_gemini_stream_whose_last_frame_carries_text_and_the_finish() {
        let frame = |text: &str, finish: Option<&str>| {
            let mut c = json!({"content":{"role":"model","parts":[{"text":text}]},"index":0});
            if let Some(f) = finish {
                c["finishReason"] = json!(f);
            }
            data(json!({"candidates":[c],"modelVersion":"g"}))
        };
        let frames = vec![
            frame("key: <<TW_SEC", None),
            frame("RET_1>> 结束 <<TW", Some("STOP")),
        ];
        let out = run(Dialect::Gemini, &frames);
        let text: String = values(&out)
            .iter()
            .filter_map(|v| v["candidates"][0]["content"]["parts"][0]["text"].as_str())
            .collect();
        assert_eq!(text, format!("key: {KEY} 结束 <<TW"));
    }

    #[test]
    fn a_gemini_function_call_is_restored_whole() {
        let raw = data(json!({"candidates":[{"content":{"role":"model","parts":[
            {"functionCall":{"name":"write","args":{"v":"<<TW_SECRET_1>>"}}}
        ]},"index":0}]}));
        let out = run(Dialect::Gemini, &[raw]);
        assert!(out.contains(KEY), "{out}");
    }

    #[test]
    fn what_gets_replaced_never_carries_a_quote_so_restoring_keeps_json_valid() {
        // 整包响应原样放回、不转义，靠的就是这一条：一条跨过引号的自定义规则，
        // 换下来的那段止于引号之前
        let rules = RuleSet::none().with_custom("q", r#"id=\S+"#).unwrap();
        let body = r#"{"text":"id=abc\"x"}"#;
        let r = redact(body, &rules, Ledger::new(Scheme::SECRET));
        assert!(
            r.ledger
                .table()
                .values()
                .all(|v| !v.contains('"') && !v.contains('\\'))
        );
        let mut b = Body::new(&r.ledger, false, Dialect::Chat);
        let mut out = b.process(r.text.as_bytes());
        out.extend(b.flush());
        assert_eq!(String::from_utf8(out).unwrap(), body);
    }
}
