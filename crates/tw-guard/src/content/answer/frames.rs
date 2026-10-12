//! 回答过滤按格式拆帧：一条回答里有哪几路正文、每一路在帧的哪个字段、一路在哪一帧结束、
//! 扣着的尾巴补成什么样的一帧。
//!
//! 看的是**客户端收到的那种格式**（转换、回答钩子之后，工具调用审查之前）。每一路正文
//! 一个 [`Lane`]：
//!
//! - Anthropic：一个内容块一路（`text_delta`），`content_block_stop` 结束；
//! - Chat：一个回答（choice）一路（`delta.content`），`finish_reason` 结束；
//! - Responses：一段正文一路（`output_text.delta`，按 `output_index` 和 `content_index`），
//!   Codex 自由格式工具的输入一路（`custom_tool_call_input.delta`），各自的 `.done` 结束；
//! - Gemini：一个候选回答一路（不是思考的 `text`），`finishReason` 结束；
//! - Bedrock Converse：一个内容块一路（`delta.text`），`contentBlockStop` 结束。
//!
//! 一路结束时扣着的尾巴：那一帧里正好有这一路的字段就接在后面，没有就按这一路自己的格式
//! 补一帧，**先于**结束的那一帧发出去（和占位符的还原同一个办法，见
//! [`crate::redact::sse`]）。工具调用、思考过程不碰。
//!
//! # 带着整段正文的帧
//!
//! Responses 在结束的时候把整段正文再说一遍：`output_text.done` 的 `text`、
//! `content_part.done` 的 `part.text`、`output_item.done` 的 `item`、`response.completed`
//! 的 `response.output`，自由格式工具的输入也一样。它们一律改成**这一路实际发出去的那一段**，
//! 和客户端拼出来的一字不差。一路正文一个增量都没来、只在这些帧里出现的，在这里查一遍（只
//! 查一遍：后面再出现的照第一遍的结果写）。
//!
//! 整份的回答（[`whole`]）是同一套：一份 JSON 里每一路正文一个 [`Lane`]，一次处理完。

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde_json::{Map, Value, json};
use tw_dialect::ir::Dialect;

use super::{Answer, Found, Lane, PLACES_MAX, Place};
use crate::content::{Action, Match, Outcome, SNIPPET_MAX};
use crate::redact::sse::{Rewritten, Synth, write};

/// 一条回答里的一路正文。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LaneKey {
    /// 正文。Anthropic、Bedrock 是内容块的 index，Chat、Gemini 是第几个回答（choice、
    /// candidate），Responses 是 `output_index` 和 `content_index` 拼起来的
    Text(u64),
    /// Responses 自由格式工具（Codex 的 `apply_patch`）的输入，`output_index`
    Input(u64),
}

impl LaneKey {
    /// Responses 的正文一路：`output_index` 和 `content_index` 拼成一个
    pub fn responses_text(output: u64, content: u64) -> Self {
        LaneKey::Text((output << 32) | (content & 0xffff_ffff))
    }

    fn output_index(self) -> u64 {
        match self {
            LaneKey::Text(k) => k >> 32,
            LaneKey::Input(k) => k,
        }
    }

    /// 这一路在整份回答里的位置，按整包的形状写：`content[0].text`（Anthropic）、
    /// `choices[0].message.content`（Chat）、`output[1].content[0].text`（Responses）、
    /// `candidates[0].content.parts`（Gemini）、`output.message.content[0].text`（Bedrock）
    pub fn path(self, d: Dialect) -> String {
        match (d, self) {
            (_, LaneKey::Input(oi)) => format!("output[{oi}].input"),
            (Dialect::Anthropic, LaneKey::Text(i)) => format!("content[{i}].text"),
            (Dialect::Chat, LaneKey::Text(i)) => format!("choices[{i}].message.content"),
            (Dialect::Responses, LaneKey::Text(k)) => {
                format!("output[{}].content[{}].text", k >> 32, k & 0xffff_ffff)
            }
            (Dialect::Gemini, LaneKey::Text(i)) => format!("candidates[{i}].content.parts"),
            (Dialect::Bedrock, LaneKey::Text(i)) => format!("output.message.content[{i}].text"),
        }
    }
}

/// 一条规则在一次回答里的命中，各路合起来。**一条规则一条**，和请求那一边一样。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerHit {
    /// 内置规则的 id，或者自定义规则的名字
    pub rule: String,
    pub name: String,
    pub custom: bool,
    /// 写的样子，原样
    pub pattern: String,
    pub matching: Match,
    pub case_sensitive: bool,
    /// 第三档下做什么
    pub action: Action,
    /// 实际做了什么：删掉、换掉，或者只记录（观察档、仅记录的规则）
    pub outcome: Outcome,
    /// 规则的替换文本（换掉时就是换成的它）
    pub replacement: String,
    /// 一共几处
    pub count: usize,
    /// 第一处前后的一小段（最多 [`SNIPPET_MAX`] 个字，截过的一头写 `…`）。**原文，没打码**
    pub snippet: String,
    /// 前 [`PLACES_MAX`] 处：哪一路（[`LaneKey::path`]）、前后的字。**原文，没打码**
    pub places: Vec<(String, Place)>,
}

/// 一次回答的命中，按规则合起来，按第一次命中的先后。
#[derive(Debug, Default)]
struct Tally {
    hits: Vec<AnswerHit>,
    /// 第几条规则 → `hits` 的下标
    at: HashMap<usize, usize>,
}

impl Tally {
    fn add(&mut self, answer: &Answer, acts: bool, path: &str, found: Vec<Found>) {
        for f in found {
            let Some(rule) = answer.rule(&f) else {
                continue;
            };
            let k = *self.at.entry(f.rule).or_insert_with(|| {
                let outcome = match rule.action() {
                    Action::Strip if acts => Outcome::Stripped,
                    Action::Replace if acts => Outcome::Replaced,
                    _ => Outcome::Recorded,
                };
                self.hits.push(AnswerHit {
                    rule: rule.id().to_string(),
                    name: rule.name().to_string(),
                    custom: rule.custom(),
                    pattern: rule.pattern().to_string(),
                    matching: rule.matching(),
                    case_sensitive: rule.case_sensitive(),
                    action: rule.action(),
                    outcome,
                    replacement: rule.replacement().to_string(),
                    count: 0,
                    snippet: String::new(),
                    places: Vec::new(),
                });
                self.hits.len() - 1
            });
            let h = &mut self.hits[k];
            h.count += 1;
            if let Some(p) = f.place {
                if h.snippet.is_empty() {
                    h.snippet = snippet(&p);
                }
                if h.places.len() < PLACES_MAX {
                    h.places.push((path.to_string(), p));
                }
            }
        }
    }
}

/// 一处命中前后的一小段：前面带十个字，整段不超过 [`SNIPPET_MAX`] 个字，截过的一头用 `…`
/// 标出来（和请求那一边的写法一样）
fn snippet(p: &Place) -> String {
    let lead: Vec<char> = p.before.chars().collect();
    let take = lead.len().min(10);
    let mut out = String::new();
    if take < lead.len() {
        out.push('…');
    }
    let body: String = lead[lead.len() - take..]
        .iter()
        .copied()
        .chain(p.matched.chars())
        .chain(p.after.chars())
        .collect();
    let n = body.chars().count();
    out.extend(body.chars().take(SNIPPET_MAX));
    if n > SNIPPET_MAX {
        out.push('…');
    }
    out
}

/// 一路正文，连同补帧要抄的几个字段和（Responses）到目前为止发出去的整段。
struct Open {
    lane: Lane,
    /// 这一路最近一帧里补帧要抄的那几个字段（id、模型名……）
    ids: Value,
    /// 发出去的整段正文。只有 Responses、而且会改字时才记（见 [`FrameFilter`]）
    out: Option<String>,
}

/// 一条流上、按帧的回答过滤。帧是解析好的 JSON，**客户端的格式**。
pub struct FrameFilter {
    dialect: Dialect,
    answer: Arc<Answer>,
    acts: bool,
    lanes: BTreeMap<LaneKey, Open>,
    /// Responses：结束了的那几路（或者只在带着整段正文的帧里出现过的）实际发出去的整段。
    /// 没改字的是 `None`：照原样
    finals: HashMap<LaneKey, Option<String>>,
    tally: Tally,
}

/// 一帧里的一处正文：哪一路、在哪个字段
struct Field {
    lane: LaneKey,
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

impl FrameFilter {
    /// `dialect` 是客户端收到的格式；`acts`：第三档（观察档是 `false`，只记录）
    pub fn new(answer: Arc<Answer>, acts: bool, dialect: Dialect) -> Self {
        FrameFilter {
            dialect,
            answer,
            acts,
            lanes: BTreeMap::new(),
            finals: HashMap::new(),
            tally: Tally::default(),
        }
    }

    /// 会改字吗（第三档、有删除或替换规则）。不会的话不用记发出去的整段
    fn edits(&self) -> bool {
        self.acts && self.answer.edits
    }

    /// 改写一帧。帧本身就地改，返回要先于它发出去的补帧。
    pub fn frame(&mut self, v: &mut Value) -> Rewritten {
        let mut out = Rewritten::default();
        let d = self.dialect;
        let keep = d == Dialect::Responses && self.edits();
        let fields = fields(d, v);
        for f in &fields {
            let ids = ids(d, v);
            let open = self.lanes.entry(f.lane).or_insert_with(|| Open {
                lane: self.answer.lane(self.acts),
                ids: Value::Null,
                out: keep.then(String::new),
            });
            open.ids = ids;
            if let Some(s) = at(v, &f.path) {
                let next = open.lane.push(s);
                if let Some(o) = open.out.as_mut() {
                    o.push_str(&next);
                }
                if next != *s {
                    out.changed = true;
                    *s = next;
                }
            }
        }
        let closing: Vec<LaneKey> = match close(d, v) {
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
            let tail = open.lane.finish();
            self.harvest(lane, &mut open.lane);
            if let Some(o) = open.out.as_mut() {
                o.push_str(&tail);
            }
            self.finals.insert(lane, open.out.take());
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
                None => out.before.push(synth(d, lane, &open.ids, tail)),
            }
        }
        if d == Dialect::Responses {
            out.changed |= self.repeat(v);
        }
        out
    }

    /// 碰到不是 JSON 的帧（`[DONE]`），或者流结束了、断了：所有扣着的尾巴补成帧。
    ///
    /// **丢掉它就是丢掉模型说过的字。**
    pub fn drain(&mut self) -> Vec<Synth> {
        let lanes = std::mem::take(&mut self.lanes);
        let mut out = Vec::new();
        for (key, mut open) in lanes {
            let tail = open.lane.finish();
            self.harvest(key, &mut open.lane);
            if let Some(o) = open.out.as_mut() {
                o.push_str(&tail);
            }
            self.finals.insert(key, open.out.take());
            if !tail.is_empty() {
                out.push(synth(self.dialect, key, &open.ids, tail));
            }
        }
        out
    }

    /// 到目前为止的命中，取走。还没结束的那几路也算上（它们之后的命中下一次取）
    pub fn take_hits(&mut self) -> Vec<AnswerHit> {
        let keys: Vec<LaneKey> = self.lanes.keys().copied().collect();
        for k in keys {
            if let Some(mut open) = self.lanes.remove(&k) {
                self.harvest(k, &mut open.lane);
                self.lanes.insert(k, open);
            }
        }
        self.tally.at.clear();
        std::mem::take(&mut self.tally.hits)
    }

    fn harvest(&mut self, key: LaneKey, lane: &mut Lane) {
        let found = lane.take_found();
        if !found.is_empty() {
            let path = key.path(self.dialect);
            self.tally.add(&self.answer, self.acts, &path, found);
        }
    }

    /// 一路实际发出去的整段：结束了的照记下的；一个增量都没来过的，在这里查一遍，记下来。
    /// 和 `given` 一样（没改字）是 `None`
    fn final_text(&mut self, key: LaneKey, given: &str) -> Option<String> {
        if let Some(open) = self.lanes.get(&key) {
            // 还没结束的一路（带着整段正文的帧先到了）：照到目前为止发出去的
            return open.out.clone().filter(|o| o != given);
        }
        let known = match self.finals.get(&key) {
            Some(f) => f.clone(),
            None => {
                let mut lane = self.answer.lane(self.acts);
                let out = lane.whole(given);
                self.harvest(key, &mut lane);
                let f = (self.edits() && out != given).then_some(out);
                self.finals.insert(key, f.clone());
                f
            }
        };
        known.filter(|o| o != given)
    }

    /// Responses 带着整段正文的帧：改成实际发出去的那一段（见模块说明）。返回改了没有
    fn repeat(&mut self, v: &mut Value) -> bool {
        let u = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        let mut changed = false;
        let mut set = |me: &mut Self, slot: Option<&mut Value>, key: LaneKey| {
            if let Some(Value::String(s)) = slot
                && let Some(t) = me.final_text(key, s)
            {
                *s = t;
                changed = true;
            }
        };
        match kind(v) {
            Some("response.output_text.done") => {
                let key = LaneKey::responses_text(u(v, "output_index"), u(v, "content_index"));
                set(self, v.get_mut("text"), key);
            }
            Some("response.custom_tool_call_input.done") => {
                let key = LaneKey::Input(u(v, "output_index"));
                set(self, v.get_mut("input"), key);
            }
            Some("response.content_part.done") => {
                let key = LaneKey::responses_text(u(v, "output_index"), u(v, "content_index"));
                if let Some(part) = v.get_mut("part")
                    && kind(part) == Some("output_text")
                {
                    set(self, part.get_mut("text"), key);
                }
            }
            Some("response.output_item.done") => {
                let oi = u(v, "output_index");
                if let Some(item) = v.get_mut("item") {
                    changed |= self.item(oi, item);
                }
            }
            Some("response.completed" | "response.incomplete" | "response.failed") => {
                if let Some(items) = v
                    .pointer_mut("/response/output")
                    .and_then(Value::as_array_mut)
                {
                    for (i, item) in items.iter_mut().enumerate() {
                        changed |= self.item(i as u64, item);
                    }
                }
            }
            _ => {}
        }
        changed
    }

    /// Responses 的一项（`output` 里的一个）：消息的每段正文、自由格式工具的输入
    fn item(&mut self, oi: u64, item: &mut Value) -> bool {
        let mut changed = false;
        match kind(item) {
            Some("message") => {
                if let Some(parts) = item.get_mut("content").and_then(Value::as_array_mut) {
                    for (ci, part) in parts.iter_mut().enumerate() {
                        if kind(part) != Some("output_text") {
                            continue;
                        }
                        let key = LaneKey::responses_text(oi, ci as u64);
                        if let Some(Value::String(s)) = part.get_mut("text")
                            && let Some(t) = self.final_text(key, s)
                        {
                            *s = t;
                            changed = true;
                        }
                    }
                }
            }
            Some("custom_tool_call") => {
                if let Some(Value::String(s)) = item.get_mut("input")
                    && let Some(t) = self.final_text(LaneKey::Input(oi), s)
                {
                    *s = t;
                    changed = true;
                }
            }
            _ => {}
        }
        changed
    }
}

fn kind(v: &Value) -> Option<&str> {
    v.get("type").and_then(Value::as_str)
}

fn u(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

/// 一帧里流着的正文在哪几个字段，按先后
fn fields(d: Dialect, v: &Value) -> Vec<Field> {
    use Seg::{Idx, Key};
    let mut out = Vec::new();
    match d {
        Dialect::Anthropic => {
            if kind(v) == Some("content_block_delta")
                && v.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta")
            {
                out.push(Field {
                    lane: LaneKey::Text(u(v, "index")),
                    path: vec![Key("delta"), Key("text")],
                });
            }
        }
        Dialect::Chat => {
            let Some(choices) = v.get("choices").and_then(Value::as_array) else {
                return out;
            };
            for (k, c) in choices.iter().enumerate() {
                if c.pointer("/delta/content").is_some_and(Value::is_string) {
                    let i = c.get("index").and_then(Value::as_u64).unwrap_or(k as u64);
                    out.push(Field {
                        lane: LaneKey::Text(i),
                        path: vec![Key("choices"), Idx(k), Key("delta"), Key("content")],
                    });
                }
            }
        }
        Dialect::Responses => match kind(v) {
            Some("response.output_text.delta") => out.push(Field {
                lane: LaneKey::responses_text(u(v, "output_index"), u(v, "content_index")),
                path: vec![Key("delta")],
            }),
            Some("response.custom_tool_call_input.delta") => out.push(Field {
                lane: LaneKey::Input(u(v, "output_index")),
                path: vec![Key("delta")],
            }),
            _ => {}
        },
        Dialect::Gemini => {
            let Some(cands) = v.get("candidates").and_then(Value::as_array) else {
                return out;
            };
            for (k, c) in cands.iter().enumerate() {
                let i = c.get("index").and_then(Value::as_u64).unwrap_or(k as u64);
                let Some(parts) = c.pointer("/content/parts").and_then(Value::as_array) else {
                    continue;
                };
                for (j, p) in parts.iter().enumerate() {
                    // 思考过程不碰：带着签名
                    let thought = p.get("thought").and_then(Value::as_bool) == Some(true);
                    if !thought && p.get("text").is_some_and(Value::is_string) {
                        out.push(Field {
                            lane: LaneKey::Text(i),
                            path: vec![
                                Key("candidates"),
                                Idx(k),
                                Key("content"),
                                Key("parts"),
                                Idx(j),
                                Key("text"),
                            ],
                        });
                    }
                }
            }
        }
        // Converse 的流在进门时已经拆成了 SSE，载荷里不带事件名，按形状认
        Dialect::Bedrock => {
            if v.pointer("/delta/text").is_some_and(Value::is_string) {
                out.push(Field {
                    lane: LaneKey::Text(u(v, "contentBlockIndex")),
                    path: vec![Key("delta"), Key("text")],
                });
            }
        }
    }
    out
}

/// 哪几路在这一帧结束
enum Close {
    None,
    Lanes(Vec<LaneKey>),
    /// Responses 的 `output_item.done`：这一项的所有路
    Output(u64),
    All,
}

fn close(d: Dialect, v: &Value) -> Close {
    match d {
        Dialect::Anthropic => match kind(v) {
            Some("content_block_stop") => Close::Lanes(vec![LaneKey::Text(u(v, "index"))]),
            Some("message_stop" | "error") => Close::All,
            _ => Close::None,
        },
        Dialect::Chat => {
            let done: Vec<LaneKey> = v
                .get("choices")
                .and_then(Value::as_array)
                .map(|cs| {
                    cs.iter()
                        .enumerate()
                        .filter(|(_, c)| c.get("finish_reason").is_some_and(|f| !f.is_null()))
                        .map(|(k, c)| {
                            LaneKey::Text(
                                c.get("index").and_then(Value::as_u64).unwrap_or(k as u64),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            if done.is_empty() {
                Close::None
            } else {
                Close::Lanes(done)
            }
        }
        Dialect::Responses => match kind(v) {
            Some("response.output_text.done" | "response.content_part.done") => {
                Close::Lanes(vec![LaneKey::responses_text(
                    u(v, "output_index"),
                    u(v, "content_index"),
                )])
            }
            Some("response.custom_tool_call_input.done") => {
                Close::Lanes(vec![LaneKey::Input(u(v, "output_index"))])
            }
            Some("response.output_item.done") => Close::Output(u(v, "output_index")),
            Some("response.completed" | "response.incomplete" | "response.failed" | "error") => {
                Close::All
            }
            _ => Close::None,
        },
        Dialect::Gemini => {
            let done: Vec<LaneKey> = v
                .get("candidates")
                .and_then(Value::as_array)
                .map(|cs| {
                    cs.iter()
                        .enumerate()
                        .filter(|(_, c)| c.get("finishReason").is_some_and(|f| !f.is_null()))
                        .map(|(k, c)| {
                            LaneKey::Text(
                                c.get("index").and_then(Value::as_u64).unwrap_or(k as u64),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            if done.is_empty() {
                Close::None
            } else {
                Close::Lanes(done)
            }
        }
        // `messageStop` 带 `stopReason`；`contentBlockStop` 只有下标，没有 `delta` 也没有 `start`
        Dialect::Bedrock => {
            if v.get("stopReason").is_some() {
                Close::All
            } else if v.get("contentBlockIndex").is_some()
                && v.get("delta").is_none()
                && v.get("start").is_none()
            {
                Close::Lanes(vec![LaneKey::Text(u(v, "contentBlockIndex"))])
            } else {
                Close::None
            }
        }
    }
}

/// 补帧要从这一路最近的一帧上抄的那几个字段
fn ids(d: Dialect, v: &Value) -> Value {
    let keys: &[&str] = match d {
        Dialect::Chat => &["id", "object", "created", "model", "system_fingerprint"],
        Dialect::Gemini => &["modelVersion", "responseId"],
        Dialect::Responses => &["item_id"],
        Dialect::Anthropic | Dialect::Bedrock => &[],
    };
    let mut m = Map::new();
    for k in keys {
        if let Some(x) = v.get(*k) {
            m.insert(k.to_string(), x.clone());
        }
    }
    Value::Object(m)
}

/// 一路扣着的尾巴补成一帧，形状照着那一路自己的格式
fn synth(d: Dialect, lane: LaneKey, ids: &Value, tail: String) -> Synth {
    let item_id = ids.get("item_id").cloned().unwrap_or(Value::Null);
    match (d, lane) {
        (_, LaneKey::Input(oi)) => Synth {
            event: Some("response.custom_tool_call_input.delta".into()),
            data: json!({
                "type": "response.custom_tool_call_input.delta",
                "item_id": item_id,
                "output_index": oi,
                "delta": tail,
            }),
        },
        (Dialect::Anthropic, LaneKey::Text(i)) => Synth {
            event: Some("content_block_delta".into()),
            data: json!({
                "type": "content_block_delta",
                "index": i,
                "delta": { "type": "text_delta", "text": tail },
            }),
        },
        (Dialect::Chat, LaneKey::Text(i)) => {
            let mut m = ids.as_object().cloned().unwrap_or_default();
            m.insert(
                "choices".into(),
                json!([{ "index": i, "delta": { "content": tail }, "finish_reason": null }]),
            );
            Synth {
                event: None,
                data: Value::Object(m),
            }
        }
        (Dialect::Responses, LaneKey::Text(k)) => Synth {
            event: Some("response.output_text.delta".into()),
            data: json!({
                "type": "response.output_text.delta",
                "item_id": item_id,
                "output_index": k >> 32,
                "content_index": k & 0xffff_ffff,
                "delta": tail,
            }),
        },
        (Dialect::Gemini, LaneKey::Text(i)) => {
            let mut m = ids.as_object().cloned().unwrap_or_default();
            m.insert(
                "candidates".into(),
                json!([{ "content": { "role": "model", "parts": [{ "text": tail }] }, "index": i }]),
            );
            Synth {
                event: None,
                data: Value::Object(m),
            }
        }
        (Dialect::Bedrock, LaneKey::Text(i)) => Synth {
            event: Some("contentBlockDelta".into()),
            data: json!({ "contentBlockIndex": i, "delta": { "text": tail } }),
        },
    }
}

// ───────────────────────────────────────────────────────── 字节流

/// 一条 SSE 字节流上的回答过滤。**看不懂的帧原样转发**：心跳、注释、没见过的事件。
pub struct SseFilter {
    frames: FrameFilter,
    /// 还没收齐的那一帧
    partial: Vec<u8>,
}

impl SseFilter {
    pub fn new(frames: FrameFilter) -> Self {
        SseFilter {
            frames,
            partial: Vec::new(),
        }
    }

    pub fn process(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.partial.extend_from_slice(chunk);
        let mut out = Vec::new();
        // **收不齐就等下一块**：半帧转发出去，客户端那边会当成一帧解析失败
        while let Some((n, sep)) = tw_dialect::frame::frame_end(&self.partial) {
            let frame: Vec<u8> = self.partial.drain(..n + sep).collect();
            out.extend_from_slice(&self.rewrite(&frame));
        }
        out
    }

    /// 流结束了（或者断了）：没收尾的最后一帧照样改写，扣着的尾巴补成帧
    pub fn flush(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.partial.is_empty() {
            let frame = std::mem::take(&mut self.partial);
            out.extend_from_slice(&self.rewrite(&frame));
        }
        for s in self.frames.drain() {
            out.extend_from_slice(write(&s).as_bytes());
        }
        out
    }

    fn rewrite(&mut self, frame: &[u8]) -> Vec<u8> {
        let Ok(text) = std::str::from_utf8(frame) else {
            return frame.to_vec();
        };
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
            // `[DONE]` 这类：流就要结束了，扣着的尾巴得赶在它前面
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
        let json = v.to_string();
        let mut wrote = false;
        for line in text.split_inclusive('\n') {
            let bare = line.trim_end_matches(['\n', '\r']);
            if tw_dialect::frame::data_of(bare).is_some() {
                if !wrote {
                    out.extend_from_slice(b"data: ");
                    out.extend_from_slice(json.as_bytes());
                    out.extend_from_slice(&line.as_bytes()[bare.len()..]);
                    wrote = true;
                }
                continue;
            }
            out.extend_from_slice(line.as_bytes());
        }
        // 没收尾的最后一帧（流结束时）补上空行
        if !out.ends_with(b"\n\n") && !out.ends_with(b"\r\n\r\n") {
            out.extend_from_slice(b"\n\n");
        }
        out
    }
}

/// Gemini 客户端不带 `alt=sse` 时的流：一个逐个元素写出的 JSON 数组，一个元素一帧。
pub struct ArrayFilter {
    frames: FrameFilter,
    partial: Vec<u8>,
    /// 发出去过元素了：再发一个（补出来的）要先写分隔符
    element: bool,
}

impl ArrayFilter {
    pub fn new(frames: FrameFilter) -> Self {
        ArrayFilter {
            frames,
            partial: Vec::new(),
            element: false,
        }
    }

    pub fn process(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.partial.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(end) = crate::tools::wall::find_element_end(&self.partial) {
            let frame: Vec<u8> = self.partial.drain(..end).collect();
            self.piece(&frame, &mut out);
        }
        out
    }

    /// 流结束了（或者断了）：没收齐的原样交出去，扣着的尾巴补成元素。**不补收尾的 `]`**：
    /// 断了的流由调用方按它发过的接好
    pub fn flush(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let rest = std::mem::take(&mut self.partial);
        if !rest.is_empty() {
            self.piece(&rest, &mut out);
        }
        let tails = self.frames.drain();
        self.synths(tails, &mut out);
        out
    }

    fn synths(&mut self, synths: Vec<Synth>, out: &mut Vec<u8>) {
        for s in synths {
            if self.element {
                out.extend_from_slice(b",\r\n");
            }
            out.extend_from_slice(s.data.to_string().as_bytes());
            self.element = true;
        }
    }

    /// 一帧：开头的 `[`、结尾的 `]`、或者一个元素（连同它前面的分隔符）
    fn piece(&mut self, raw: &[u8], out: &mut Vec<u8>) {
        let start = raw
            .iter()
            .position(|b| !(b.is_ascii_whitespace() || *b == b','))
            .unwrap_or(raw.len());
        match raw.get(start) {
            Some(b'{') => {}
            // 收尾之前把扣着的补上
            Some(b']') => {
                let tails = self.frames.drain();
                self.synths(tails, out);
                out.extend_from_slice(raw);
                return;
            }
            _ => {
                out.extend_from_slice(raw);
                return;
            }
        }
        let Ok(mut v) = serde_json::from_slice::<Value>(&raw[start..]) else {
            out.extend_from_slice(raw);
            self.element = true;
            return;
        };
        let r = self.frames.frame(&mut v);
        let had = !r.before.is_empty();
        self.synths(r.before, out);
        let seps = &raw[..start];
        // 补出来的元素插在第一个元素前面时，第一个元素前面没有逗号
        if had && !seps.contains(&b',') {
            out.extend_from_slice(b",\r\n");
        }
        out.extend_from_slice(seps);
        if r.changed {
            out.extend_from_slice(v.to_string().as_bytes());
        } else {
            out.extend_from_slice(&raw[start..]);
        }
        self.element = true;
    }
}

/// 一条回答流上的过滤，按客户端收到的样子分：SSE，或者 JSON 数组。
pub enum Body {
    Sse(SseFilter),
    JsonArray(ArrayFilter),
}

impl Body {
    /// `sse`：客户端收到的是 SSE；不是的话是 Gemini 的 JSON 数组
    pub fn new(answer: Arc<Answer>, acts: bool, dialect: Dialect, sse: bool) -> Self {
        let frames = FrameFilter::new(answer, acts, dialect);
        if sse {
            Body::Sse(SseFilter::new(frames))
        } else {
            Body::JsonArray(ArrayFilter::new(frames))
        }
    }

    pub fn process(&mut self, chunk: &[u8]) -> Vec<u8> {
        match self {
            Body::Sse(f) => f.process(chunk),
            Body::JsonArray(f) => f.process(chunk),
        }
    }

    /// 流结束了（或者断了）：扣着的补出来
    pub fn flush(&mut self) -> Vec<u8> {
        match self {
            Body::Sse(f) => f.flush(),
            Body::JsonArray(f) => f.flush(),
        }
    }

    /// 到目前为止的命中，取走
    pub fn take_hits(&mut self) -> Vec<AnswerHit> {
        match self {
            Body::Sse(f) => f.frames.take_hits(),
            Body::JsonArray(f) => f.frames.take_hits(),
        }
    }
}

// ───────────────────────────────────────────────────────── 整份

/// 一份整份的回答（客户端的格式，JSON）：每一路正文一次处理完。返回改过的回答（没改是
/// `None`）和命中。不是 JSON 的什么都不做
pub fn whole(
    answer: &Arc<Answer>,
    acts: bool,
    dialect: Dialect,
    body: &[u8],
) -> (Option<Vec<u8>>, Vec<AnswerHit>) {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return (None, Vec::new());
    };
    let spots = whole_fields(dialect, &v);
    let mut lanes: BTreeMap<LaneKey, Lane> = BTreeMap::new();
    let mut last: BTreeMap<LaneKey, usize> = BTreeMap::new();
    let mut changed = false;
    for (k, f) in spots.iter().enumerate() {
        let lane = lanes.entry(f.lane).or_insert_with(|| answer.lane(acts));
        last.insert(f.lane, k);
        if let Some(s) = at(&mut v, &f.path) {
            let next = lane.push(s);
            if next != *s {
                *s = next;
                changed = true;
            }
        }
    }
    let mut tally = Tally::default();
    for (key, mut lane) in lanes {
        let tail = lane.finish();
        if !tail.is_empty()
            && let Some(s) = last
                .get(&key)
                .and_then(|&k| spots.get(k))
                .and_then(|f| at(&mut v, &f.path))
        {
            s.push_str(&tail);
            changed = true;
        }
        let found = lane.take_found();
        tally.add(answer, acts, &key.path(dialect), found);
    }
    let body = changed.then(|| v.to_string().into_bytes());
    (body, tally.hits)
}

/// 一份整份的回答里正文在哪几个字段，按先后
fn whole_fields(d: Dialect, v: &Value) -> Vec<Field> {
    use Seg::{Idx, Key};
    let mut out = Vec::new();
    let arr = |v: &Value, k: &str| v.get(k).and_then(Value::as_array).cloned();
    match d {
        Dialect::Anthropic => {
            for (i, b) in arr(v, "content").unwrap_or_default().iter().enumerate() {
                if kind(b) == Some("text") {
                    out.push(Field {
                        lane: LaneKey::Text(i as u64),
                        path: vec![Key("content"), Idx(i), Key("text")],
                    });
                }
            }
        }
        Dialect::Chat => {
            for (k, c) in arr(v, "choices").unwrap_or_default().iter().enumerate() {
                if c.pointer("/message/content").is_some_and(Value::is_string) {
                    let i = c.get("index").and_then(Value::as_u64).unwrap_or(k as u64);
                    out.push(Field {
                        lane: LaneKey::Text(i),
                        path: vec![Key("choices"), Idx(k), Key("message"), Key("content")],
                    });
                }
            }
        }
        Dialect::Responses => {
            for (i, item) in arr(v, "output").unwrap_or_default().iter().enumerate() {
                match kind(item) {
                    Some("message") => {
                        for (ci, p) in arr(item, "content").unwrap_or_default().iter().enumerate() {
                            if kind(p) == Some("output_text") {
                                out.push(Field {
                                    lane: LaneKey::responses_text(i as u64, ci as u64),
                                    path: vec![
                                        Key("output"),
                                        Idx(i),
                                        Key("content"),
                                        Idx(ci),
                                        Key("text"),
                                    ],
                                });
                            }
                        }
                    }
                    Some("custom_tool_call") => out.push(Field {
                        lane: LaneKey::Input(i as u64),
                        path: vec![Key("output"), Idx(i), Key("input")],
                    }),
                    _ => {}
                }
            }
        }
        Dialect::Gemini => {
            for (k, c) in arr(v, "candidates").unwrap_or_default().iter().enumerate() {
                let i = c.get("index").and_then(Value::as_u64).unwrap_or(k as u64);
                let parts = c
                    .pointer("/content/parts")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for (j, p) in parts.iter().enumerate() {
                    let thought = p.get("thought").and_then(Value::as_bool) == Some(true);
                    if !thought && p.get("text").is_some_and(Value::is_string) {
                        out.push(Field {
                            lane: LaneKey::Text(i),
                            path: vec![
                                Key("candidates"),
                                Idx(k),
                                Key("content"),
                                Key("parts"),
                                Idx(j),
                                Key("text"),
                            ],
                        });
                    }
                }
            }
        }
        Dialect::Bedrock => {
            let blocks = v
                .pointer("/output/message/content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for (i, b) in blocks.iter().enumerate() {
                if b.get("text").is_some_and(Value::is_string) {
                    out.push(Field {
                        lane: LaneKey::Text(i as u64),
                        path: vec![
                            Key("output"),
                            Key("message"),
                            Key("content"),
                            Idx(i),
                            Key("text"),
                        ],
                    });
                }
            }
        }
    }
    out
}
