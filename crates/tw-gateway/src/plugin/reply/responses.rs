//! OpenAI Responses 的回答。
//!
//! Responses 的流按输出项组织，**完整的内容在好几处重复**：一段文字的增量
//! （`output_text.delta`）之外，`output_text.done` 的 `text`、`content_part.done` 的
//! `part.text`、`output_item.done` 的整个项、最后 `response.completed` 的 `output`
//! 里都有全文 —— Codex 记历史用的是 `output_item.done`。插件改了文字，这几处都换成
//! 客户端实际收到的那一版。
//!
//! 函数调用（`function_call`、`custom_tool_call`）从 `output_item.added` 攒到
//! `output_item.done`，交给插件之后按结果写出：不变的原样，换出来的每个写成一组完整的
//! 事件（added、参数的 delta 和 done、item.done），去掉的一帧不留。后面输出项的
//! `output_index` 跟着挪，`response.completed` 的 `output` 跟着换；每一帧的
//! `sequence_number` 按实际发出去的顺序重新数。

use std::collections::HashMap;

use serde_json::{Value, json};

use super::{Call, Chain, Frame, Ids, Out, whole_text};
use crate::error::GatewayError;
use crate::plugin::view::{args_text, args_value};

pub(crate) struct Codec {
    wants_text: bool,
    wants_tools: bool,
    shift: i64,
    /// 下一个要写出去的序号。看到第一帧带序号时定下来
    seq: Option<u64>,
    /// (输出项, 内容部分) → 这一块的编号和客户端已经收到的文字
    lanes: HashMap<(u64, u64), LaneState>,
    next_lane: u64,
    /// 结束了的文字块最后的样子，`*.done` 和 `response.completed` 里用它
    finals: HashMap<(u64, u64), String>,
    tool: Option<ItemBuf>,
    deferred: Vec<Frame>,
    pub(super) requeue: Vec<Frame>,
    /// 原来的输出项序号 → 换成了什么（`None` 是没改）
    decisions: HashMap<u64, Option<Vec<Value>>>,
    ids: Ids,
}

struct LaneState {
    id: u64,
    item_id: Value,
    emitted: String,
}

struct ItemBuf {
    oi: u64,
    frames: Vec<Frame>,
    item: Value,
    args: String,
    custom: bool,
    done: Option<Value>,
}

const TOOL_EVENTS: &[&str] = &[
    "response.function_call_arguments.delta",
    "response.function_call_arguments.done",
    "response.custom_tool_call_input.delta",
    "response.custom_tool_call_input.done",
    "response.output_item.done",
];

impl Codec {
    pub(super) fn new(chain: &Chain) -> Self {
        Self {
            wants_text: chain.wants_text(),
            wants_tools: chain.wants_tools(),
            shift: 0,
            seq: None,
            lanes: HashMap::new(),
            next_lane: 0,
            finals: HashMap::new(),
            tool: None,
            deferred: Vec::new(),
            requeue: Vec::new(),
            decisions: HashMap::new(),
            ids: Ids::new(),
        }
    }

    fn shifted(&self, oi: u64) -> u64 {
        (oi as i64 + self.shift).max(0) as u64
    }

    fn keep(&self, mut f: Frame, out: &mut Vec<Out>) {
        if self.shift != 0
            && let Some(d) = f.data.as_mut()
            && let Some(oi) = d.get("output_index").and_then(Value::as_u64)
        {
            d["output_index"] = json!((oi as i64 + self.shift).max(0));
            f.dirty = true;
        }
        out.push(Out::Keep(f));
    }

    fn event(kind: &str, mut body: Value) -> Out {
        body["type"] = json!(kind);
        Out::new(Some(kind), body)
    }

    /// 按实际发出去的顺序重新数序号。**没增删帧时一帧都不改**
    pub(super) fn renumber(&mut self, out: &mut [Out]) {
        for o in out.iter_mut() {
            let d = match o {
                Out::Keep(f) => match f.data.as_mut() {
                    Some(d) if d.get("sequence_number").is_some() => {
                        let want = *self
                            .seq
                            .get_or_insert_with(|| d["sequence_number"].as_u64().unwrap_or(0));
                        if d["sequence_number"].as_u64() != Some(want) {
                            d["sequence_number"] = json!(want);
                            f.dirty = true;
                        }
                        self.seq = Some(want + 1);
                        continue;
                    }
                    _ => continue,
                },
                Out::New { data, .. } => data,
            };
            if let Some(n) = self.seq {
                d["sequence_number"] = json!(n);
                self.seq = Some(n + 1);
            }
        }
    }

    /// 关上一块文字：扣着的补成一段增量，记下它最后的样子
    async fn close_lane(
        &mut self,
        chain: &mut Chain,
        key: (u64, u64),
        out: &mut Vec<Out>,
    ) -> Result<(), GatewayError> {
        let Some(mut l) = self.lanes.remove(&key) else {
            return Ok(());
        };
        let end = chain.text_end(l.id).await?;
        if !end.is_empty() {
            out.push(Self::event(
                "response.output_text.delta",
                json!({
                    "item_id": l.item_id,
                    "output_index": self.shifted(key.0),
                    "content_index": key.1,
                    "delta": end,
                    "logprobs": [],
                }),
            ));
            l.emitted.push_str(&end);
        }
        self.finals.insert(key, l.emitted);
        Ok(())
    }

    async fn close_item(
        &mut self,
        chain: &mut Chain,
        oi: u64,
        out: &mut Vec<Out>,
    ) -> Result<(), GatewayError> {
        let mut keys: Vec<(u64, u64)> = self.lanes.keys().filter(|k| k.0 == oi).copied().collect();
        keys.sort_unstable();
        for k in keys {
            self.close_lane(chain, k, out).await?;
        }
        Ok(())
    }

    async fn close_all(
        &mut self,
        chain: &mut Chain,
        out: &mut Vec<Out>,
    ) -> Result<(), GatewayError> {
        let mut keys: Vec<(u64, u64)> = self.lanes.keys().copied().collect();
        keys.sort_unstable();
        for k in keys {
            self.close_lane(chain, k, out).await?;
        }
        Ok(())
    }

    /// 一个消息项里的文字换成客户端收到的那一版
    fn rewrite_message(&self, oi: u64, item: &mut Value) -> bool {
        let mut changed = false;
        for (ci, part) in item
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
            .enumerate()
        {
            if let Some(t) = self.finals.get(&(oi, ci as u64))
                && part.get("type").and_then(Value::as_str) == Some("output_text")
                && part.get("text").and_then(Value::as_str) != Some(t.as_str())
            {
                part["text"] = json!(t);
                changed = true;
            }
        }
        changed
    }

    fn release_tool(&mut self, out: &mut Vec<Out>) {
        if let Some(t) = self.tool.take() {
            for f in t.frames {
                self.keep(f, out);
            }
            self.requeue = std::mem::take(&mut self.deferred);
        }
    }

    pub(super) async fn frame(
        &mut self,
        chain: &mut Chain,
        mut f: Frame,
        out: &mut Vec<Out>,
    ) -> Result<(), GatewayError> {
        let kind = f.kind().to_string();
        let oi = f.u64("output_index").unwrap_or(0);
        let ci = f.u64("content_index").unwrap_or(0);
        if let Some(t) = &self.tool {
            let mine = TOOL_EVENTS.contains(&kind.as_str()) && oi == t.oi;
            if !mine {
                if matches!(
                    kind.as_str(),
                    "response.completed" | "response.incomplete" | "response.failed" | "error"
                ) {
                    self.release_tool(out);
                    self.requeue.push(f);
                } else {
                    self.deferred.push(f);
                }
                return Ok(());
            }
        }
        match kind.as_str() {
            "response.output_item.added" => {
                let item = f
                    .data
                    .as_ref()
                    .and_then(|d| d.get("item"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let t = item.get("type").and_then(Value::as_str).unwrap_or_default();
                if self.wants_tools && matches!(t, "function_call" | "custom_tool_call") {
                    let custom = t == "custom_tool_call";
                    let key = if custom { "input" } else { "arguments" };
                    self.tool = Some(ItemBuf {
                        oi,
                        args: item
                            .get(key)
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        item,
                        custom,
                        frames: vec![f],
                        done: None,
                    });
                    return Ok(());
                }
                if let Some(id) = item.get("call_id").and_then(Value::as_str) {
                    self.ids.note(id);
                }
                self.keep(f, out);
            }
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta"
                if self.tool.is_some() =>
            {
                let t = self.tool.as_mut().expect("checked");
                if let Some(p) = f
                    .data
                    .as_ref()
                    .and_then(|d| d.get("delta"))
                    .and_then(Value::as_str)
                {
                    t.args.push_str(p);
                }
                t.frames.push(f);
            }
            "response.function_call_arguments.done" | "response.custom_tool_call_input.done"
                if self.tool.is_some() =>
            {
                let t = self.tool.as_mut().expect("checked");
                let key = if t.custom { "input" } else { "arguments" };
                if let Some(all) = f
                    .data
                    .as_ref()
                    .and_then(|d| d.get(key))
                    .and_then(Value::as_str)
                {
                    t.args = all.to_string();
                }
                t.frames.push(f);
            }
            "response.output_item.done" if self.tool.as_ref().is_some_and(|t| t.oi == oi) => {
                let mut t = self.tool.take().expect("checked");
                t.done = f.data.as_ref().and_then(|d| d.get("item")).cloned();
                t.frames.push(f);
                self.settle(chain, t, out).await?;
                self.requeue = std::mem::take(&mut self.deferred);
            }
            "response.output_text.delta" if self.wants_text => {
                let text = f
                    .data
                    .as_ref()
                    .and_then(|d| d.get("delta"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if !self.lanes.contains_key(&(oi, ci)) {
                    let id = self.next_lane;
                    self.next_lane += 1;
                    let item_id = f
                        .data
                        .as_ref()
                        .and_then(|d| d.get("item_id"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    self.lanes.insert(
                        (oi, ci),
                        LaneState {
                            id,
                            item_id,
                            emitted: String::new(),
                        },
                    );
                }
                let lane = self.lanes[&(oi, ci)].id;
                let got = chain.text(lane, &text).await?;
                self.lanes
                    .get_mut(&(oi, ci))
                    .expect("inserted")
                    .emitted
                    .push_str(&got);
                if got.is_empty() && !text.is_empty() {
                    return Ok(());
                }
                if got != text {
                    if let Some(d) = f.data.as_mut() {
                        d["delta"] = json!(got);
                    }
                    f.dirty = true;
                }
                self.keep(f, out);
            }
            "response.output_text.done" => {
                self.close_lane(chain, (oi, ci), out).await?;
                if let Some(t) = self.finals.get(&(oi, ci))
                    && let Some(d) = f.data.as_mut()
                    && d.get("text").and_then(Value::as_str) != Some(t.as_str())
                {
                    d["text"] = json!(t);
                    f.dirty = true;
                }
                self.keep(f, out);
            }
            "response.content_part.done" => {
                self.close_lane(chain, (oi, ci), out).await?;
                if let Some(t) = self.finals.get(&(oi, ci))
                    && let Some(part) = f.data.as_mut().and_then(|d| d.get_mut("part"))
                    && part.get("type").and_then(Value::as_str) == Some("output_text")
                    && part.get("text").and_then(Value::as_str) != Some(t.as_str())
                {
                    part["text"] = json!(t);
                    f.dirty = true;
                }
                self.keep(f, out);
            }
            "response.output_item.done" => {
                self.close_item(chain, oi, out).await?;
                let mut item = f
                    .data
                    .as_ref()
                    .and_then(|d| d.get("item"))
                    .cloned()
                    .unwrap_or(Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("message")
                    && self.rewrite_message(oi, &mut item)
                {
                    if let Some(d) = f.data.as_mut() {
                        d["item"] = item;
                    }
                    f.dirty = true;
                }
                self.keep(f, out);
            }
            "response.completed" | "response.incomplete" => {
                self.close_all(chain, out).await?;
                if self.rewrite_output(&mut f) {
                    f.dirty = true;
                }
                self.keep(f, out);
            }
            "response.failed" | "error" => {
                self.close_all(chain, out).await?;
                self.keep(f, out);
            }
            _ => self.keep(f, out),
        }
        Ok(())
    }

    /// `response.completed` 里的 `output`：文字换成客户端收到的，函数调用按插件的结果
    fn rewrite_output(&self, f: &mut Frame) -> bool {
        let Some(output) = f
            .data
            .as_mut()
            .and_then(|d| d.pointer_mut("/response/output"))
            .and_then(Value::as_array_mut)
        else {
            return false;
        };
        let mut changed = false;
        let mut next = Vec::with_capacity(output.len());
        for (p, mut item) in std::mem::take(output).into_iter().enumerate() {
            let p = p as u64;
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    changed |= self.rewrite_message(p, &mut item);
                    next.push(item);
                }
                Some("function_call" | "custom_tool_call") => match self.decisions.get(&p) {
                    Some(Some(items)) => {
                        changed = true;
                        next.extend(items.iter().cloned());
                    }
                    _ => next.push(item),
                },
                _ => next.push(item),
            }
        }
        *output = next;
        changed
    }

    async fn settle(
        &mut self,
        chain: &mut Chain,
        t: ItemBuf,
        out: &mut Vec<Out>,
    ) -> Result<(), GatewayError> {
        let s = |v: &Value, k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let base = t.done.clone().unwrap_or_else(|| t.item.clone());
        let call_id = s(&base, "call_id");
        let call = Call {
            id: Some(call_id.clone()),
            name: s(&base, "name"),
            input: if t.custom {
                Value::String(t.args.clone())
            } else {
                args_value(&t.args)
            },
        };
        match chain.tool_call(call).await? {
            None => {
                self.ids.note(&call_id);
                self.decisions.insert(t.oi, None);
                for f in t.frames {
                    self.keep(f, out);
                }
            }
            Some(calls) => {
                let n = calls.len() as i64;
                let mut done_items = Vec::with_capacity(calls.len());
                for (k, c) in calls.into_iter().enumerate() {
                    let oi = self.shifted(t.oi) + k as u64;
                    let item_id = tw_dialect::ir::new_id("fc_");
                    let call_id = self.ids.take(c.id.as_deref(), "call_");
                    // 原来是自由格式的调用、换出来的参数还是一段原文：照旧写成自由格式
                    let custom = t.custom && c.input.is_string();
                    let (kind, field, text) = if custom {
                        ("custom_tool_call", "input", args_text(&c.input))
                    } else {
                        ("function_call", "arguments", c.input.to_string())
                    };
                    let mut item = base.clone();
                    if let Some(o) = item.as_object_mut() {
                        o.remove("arguments");
                        o.remove("input");
                    }
                    item["type"] = json!(kind);
                    item["id"] = json!(item_id);
                    item["call_id"] = json!(call_id);
                    item["name"] = json!(c.name);
                    let mut added = item.clone();
                    added[field] = json!("");
                    added["status"] = json!("in_progress");
                    item[field] = json!(text);
                    item["status"] = json!("completed");
                    out.push(Self::event(
                        "response.output_item.added",
                        json!({ "output_index": oi, "item": added }),
                    ));
                    let (delta_kind, done_kind) = if custom {
                        (
                            "response.custom_tool_call_input.delta",
                            "response.custom_tool_call_input.done",
                        )
                    } else {
                        (
                            "response.function_call_arguments.delta",
                            "response.function_call_arguments.done",
                        )
                    };
                    out.push(Self::event(
                        delta_kind,
                        json!({ "item_id": item_id, "output_index": oi, "delta": text }),
                    ));
                    out.push(Self::event(
                        done_kind,
                        json!({ "item_id": item_id, "output_index": oi, field: text }),
                    ));
                    out.push(Self::event(
                        "response.output_item.done",
                        json!({ "output_index": oi, "item": item }),
                    ));
                    done_items.push(item);
                }
                self.decisions.insert(t.oi, Some(done_items));
                self.shift += n - 1;
            }
        }
        Ok(())
    }

    pub(super) async fn finish(
        &mut self,
        chain: &mut Chain,
        out: &mut Vec<Out>,
    ) -> Result<(), GatewayError> {
        loop {
            self.release_tool(out);
            let queued = std::mem::take(&mut self.requeue);
            if queued.is_empty() {
                break;
            }
            for f in queued {
                Box::pin(self.frame(chain, f, out)).await?;
            }
        }
        self.close_all(chain, out).await
    }
}

/// 整包：`output` 里的消息项和函数调用
pub(super) async fn whole(chain: &mut Chain, v: &mut Value) -> Result<bool, GatewayError> {
    let Some(items) = v.get("output").and_then(Value::as_array).cloned() else {
        return Ok(false);
    };
    let (text, tools) = (chain.wants_text(), chain.wants_tools());
    let mut ids = Ids::new();
    for it in &items {
        if let Some(id) = it.get("call_id").and_then(Value::as_str) {
            ids.note(id);
        }
    }
    let mut out = Vec::with_capacity(items.len());
    let mut changed = false;
    let mut lane = 0u64;
    for mut it in items {
        match it.get("type").and_then(Value::as_str) {
            Some("message") if text => {
                for part in it
                    .get_mut("content")
                    .and_then(Value::as_array_mut)
                    .into_iter()
                    .flatten()
                {
                    if part.get("type").and_then(Value::as_str) != Some("output_text") {
                        continue;
                    }
                    let t = part
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let got = whole_text(chain, lane, &t).await?;
                    lane += 1;
                    if got != t {
                        part["text"] = json!(got);
                        changed = true;
                    }
                }
                out.push(it);
            }
            Some(kind @ ("function_call" | "custom_tool_call")) if tools => {
                let custom = kind == "custom_tool_call";
                let s = |k: &str| {
                    it.get(k)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                };
                let call = Call {
                    id: it
                        .get("call_id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    name: s("name"),
                    input: if custom {
                        Value::String(s("input"))
                    } else {
                        args_value(&s("arguments"))
                    },
                };
                match chain.tool_call(call).await? {
                    None => out.push(it),
                    Some(calls) => {
                        changed = true;
                        for c in calls {
                            let custom = custom && c.input.is_string();
                            let mut item = it.clone();
                            if let Some(o) = item.as_object_mut() {
                                o.remove("arguments");
                                o.remove("input");
                            }
                            item["type"] = json!(if custom {
                                "custom_tool_call"
                            } else {
                                "function_call"
                            });
                            item["id"] = json!(tw_dialect::ir::new_id("fc_"));
                            item["call_id"] = json!(ids.take(c.id.as_deref(), "call_"));
                            item["name"] = json!(c.name);
                            if custom {
                                item["input"] = json!(args_text(&c.input));
                            } else {
                                item["arguments"] = json!(c.input.to_string());
                            }
                            out.push(item);
                        }
                    }
                }
            }
            _ => out.push(it),
        }
    }
    if changed {
        v["output"] = Value::Array(out);
    }
    Ok(changed)
}
