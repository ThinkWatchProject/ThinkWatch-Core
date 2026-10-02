//! Anthropic Messages 的回答：按内容块改。
//!
//! - 文字块（`content_block_start` 的 `text`、`text_delta`）交给插件；块结束
//!   （`content_block_stop`）时补上插件扣着的，赶在结束帧之前。
//! - `tool_use` 块从开始帧攒到结束帧，参数拼完整了再交；换出来的几个写成完整的
//!   `tool_use` 块（开始、一段参数、结束），**后面所有块的 `index` 跟着挪**，客户端按
//!   序号把块拼进数组，序号有空洞或者重复它就拼错了。攒着的时候后面来的帧先放着，
//!   这个调用落定了再接着处理。
//! - 工具调用全被去掉了，`stop_reason` 就不能还是 `tool_use`：改成 `end_turn`。
//! - 推理块、服务端工具块原样。

use std::collections::HashSet;

use serde_json::{Value, json};

use super::{Call, Chain, Frame, Ids, Out, whole_text};
use crate::error::GatewayError;

pub(crate) struct Codec {
    wants_text: bool,
    wants_tools: bool,
    /// 开着的文字块（原来的序号）
    lanes: HashSet<u64>,
    tool: Option<ToolBuf>,
    /// 攒着工具调用时后面来的帧
    deferred: Vec<Frame>,
    pub(super) requeue: Vec<Frame>,
    /// 原来的序号加上它就是写出去的序号
    shift: i64,
    tools_seen: u64,
    tools_emitted: u64,
    ids: Ids,
}

struct ToolBuf {
    index: u64,
    frames: Vec<Frame>,
    id: String,
    name: String,
    json: String,
}

impl Codec {
    pub(super) fn new(chain: &Chain) -> Self {
        Self {
            wants_text: chain.wants_text(),
            wants_tools: chain.wants_tools(),
            lanes: HashSet::new(),
            tool: None,
            deferred: Vec::new(),
            requeue: Vec::new(),
            shift: 0,
            tools_seen: 0,
            tools_emitted: 0,
            ids: Ids::new(),
        }
    }

    fn shifted(&self, index: u64) -> u64 {
        (index as i64 + self.shift).max(0) as u64
    }

    /// 带 `index` 的帧按挪过的序号写
    fn renumber(&self, f: &mut Frame) {
        if self.shift == 0 {
            return;
        }
        if let Some(d) = f.data.as_mut()
            && matches!(
                d.get("type").and_then(Value::as_str),
                Some("content_block_start" | "content_block_delta" | "content_block_stop")
            )
            && let Some(i) = d.get("index").and_then(Value::as_u64)
        {
            d["index"] = json!((i as i64 + self.shift).max(0));
            f.dirty = true;
        }
    }

    fn keep(&self, mut f: Frame, out: &mut Vec<Out>) {
        self.renumber(&mut f);
        out.push(Out::Keep(f));
    }

    fn delta(&self, index: u64, text: String) -> Out {
        Out::new(
            Some("content_block_delta"),
            json!({
                "type": "content_block_delta",
                "index": self.shifted(index),
                "delta": { "type": "text_delta", "text": text },
            }),
        )
    }

    /// 关上开着的文字块：插件扣着的补出来
    async fn close_lanes(
        &mut self,
        chain: &mut Chain,
        out: &mut Vec<Out>,
    ) -> Result<(), GatewayError> {
        let mut open: Vec<u64> = self.lanes.drain().collect();
        open.sort_unstable();
        for i in open {
            let end = chain.text_end(i).await?;
            if !end.is_empty() {
                out.push(self.delta(i, end));
            }
        }
        Ok(())
    }

    /// 攒着的工具调用原样发出去（流断了、没等到它的结束帧）
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
        let index = f.u64("index").unwrap_or(0);
        if let Some(t) = &self.tool {
            let mine = matches!(kind.as_str(), "content_block_delta" | "content_block_stop")
                && index == t.index;
            if !mine {
                if matches!(kind.as_str(), "message_delta" | "message_stop" | "error") {
                    // 流要结束了，这个调用等不到它的结束帧：原样放出去，攒着的帧和这一帧
                    // 接着处理
                    self.release_tool(out);
                    self.requeue.push(f);
                } else {
                    self.deferred.push(f);
                }
                return Ok(());
            }
        }
        match kind.as_str() {
            "content_block_start" => {
                let block = f
                    .data
                    .as_ref()
                    .and_then(|d| d.get("content_block"))
                    .cloned()
                    .unwrap_or(Value::Null);
                match block.get("type").and_then(Value::as_str) {
                    Some("text") if self.wants_text => {
                        self.lanes.insert(index);
                        let first = block.get("text").and_then(Value::as_str).unwrap_or("");
                        if !first.is_empty() {
                            let got = chain.text(index, first).await?;
                            if got != first {
                                if let Some(d) = f.data.as_mut() {
                                    d["content_block"]["text"] = json!(got);
                                }
                                f.dirty = true;
                            }
                        }
                    }
                    Some("tool_use") if self.wants_tools => {
                        let s = |k: &str| {
                            block
                                .get(k)
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string()
                        };
                        let first = block
                            .get("input")
                            .filter(|i| i.as_object().is_some_and(|o| !o.is_empty()))
                            .map(Value::to_string)
                            .unwrap_or_default();
                        self.tool = Some(ToolBuf {
                            index,
                            frames: vec![f],
                            id: s("id"),
                            name: s("name"),
                            json: first,
                        });
                        return Ok(());
                    }
                    Some("tool_use") => {
                        if let Some(id) = block.get("id").and_then(Value::as_str) {
                            self.ids.note(id);
                        }
                    }
                    _ => {}
                }
                self.keep(f, out);
            }
            "content_block_delta" => {
                if let Some(t) = &mut self.tool
                    && t.index == index
                {
                    if let Some(p) = f
                        .data
                        .as_ref()
                        .and_then(|d| d.pointer("/delta/partial_json"))
                        .and_then(Value::as_str)
                    {
                        t.json.push_str(p);
                    }
                    t.frames.push(f);
                    return Ok(());
                }
                let text = f
                    .data
                    .as_ref()
                    .filter(|d| {
                        d.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta")
                    })
                    .and_then(|d| d.pointer("/delta/text"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let (true, Some(text)) = (self.lanes.contains(&index), text) {
                    let got = chain.text(index, &text).await?;
                    if got.is_empty() {
                        return Ok(());
                    }
                    if got != text {
                        if let Some(d) = f.data.as_mut() {
                            d["delta"]["text"] = json!(got);
                        }
                        f.dirty = true;
                    }
                }
                self.keep(f, out);
            }
            "content_block_stop" => {
                if self.tool.as_ref().is_some_and(|t| t.index == index) {
                    let mut t = self.tool.take().expect("checked");
                    t.frames.push(f);
                    self.settle(chain, t, out).await?;
                    self.requeue = std::mem::take(&mut self.deferred);
                    return Ok(());
                }
                if self.lanes.remove(&index) {
                    let end = chain.text_end(index).await?;
                    if !end.is_empty() {
                        out.push(self.delta(index, end));
                    }
                }
                self.keep(f, out);
            }
            "message_delta" => {
                self.close_lanes(chain, out).await?;
                if self.tools_seen > 0
                    && self.tools_emitted == 0
                    && let Some(d) = f.data.as_mut()
                    && d.pointer("/delta/stop_reason").and_then(Value::as_str) == Some("tool_use")
                {
                    d["delta"]["stop_reason"] = json!("end_turn");
                    f.dirty = true;
                }
                self.keep(f, out);
            }
            "message_stop" | "error" => {
                self.close_lanes(chain, out).await?;
                self.keep(f, out);
            }
            _ => self.keep(f, out),
        }
        Ok(())
    }

    /// 一个收齐了的工具调用交给插件，按结果写出去
    async fn settle(
        &mut self,
        chain: &mut Chain,
        t: ToolBuf,
        out: &mut Vec<Out>,
    ) -> Result<(), GatewayError> {
        self.tools_seen += 1;
        let call = Call {
            id: Some(t.id.clone()),
            name: t.name.clone(),
            input: super::super::view::args_value(&t.json),
        };
        match chain.tool_call(call).await? {
            None => {
                self.ids.note(&t.id);
                self.tools_emitted += 1;
                for f in t.frames {
                    self.keep(f, out);
                }
            }
            Some(calls) => {
                let n = calls.len() as i64;
                for (k, c) in calls.into_iter().enumerate() {
                    let index = self.shifted(t.index) + k as u64;
                    let id = self.ids.take(c.id.as_deref(), "toolu_");
                    out.push(Out::new(
                        Some("content_block_start"),
                        json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": { "type": "tool_use", "id": id, "name": c.name, "input": {} },
                        }),
                    ));
                    out.push(Out::new(
                        Some("content_block_delta"),
                        json!({
                            "type": "content_block_delta",
                            "index": index,
                            "delta": { "type": "input_json_delta", "partial_json": c.input.to_string() },
                        }),
                    ));
                    out.push(Out::new(
                        Some("content_block_stop"),
                        json!({ "type": "content_block_stop", "index": index }),
                    ));
                }
                self.tools_emitted += n as u64;
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
        self.close_lanes(chain, out).await
    }
}

/// 整包：`content` 里的文字块和 `tool_use` 块
pub(super) async fn whole(chain: &mut Chain, v: &mut Value) -> Result<bool, GatewayError> {
    let Some(blocks) = v.get("content").and_then(Value::as_array).cloned() else {
        return Ok(false);
    };
    let (text, tools) = (chain.wants_text(), chain.wants_tools());
    let mut ids = Ids::new();
    for b in &blocks {
        if let Some(id) = b.get("id").and_then(Value::as_str) {
            ids.note(id);
        }
    }
    let mut out = Vec::with_capacity(blocks.len());
    let mut changed = false;
    let (mut seen, mut emitted) = (0u64, 0u64);
    for (lane, mut b) in blocks.into_iter().enumerate() {
        match b.get("type").and_then(Value::as_str) {
            Some("text") if text => {
                let t = b
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let got = whole_text(chain, lane as u64, &t).await?;
                if got != t {
                    b["text"] = json!(got);
                    changed = true;
                }
                out.push(b);
            }
            Some("tool_use") if tools => {
                seen += 1;
                let call = Call {
                    id: b.get("id").and_then(Value::as_str).map(str::to_string),
                    name: b
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input: b.get("input").cloned().unwrap_or_else(|| json!({})),
                };
                match chain.tool_call(call).await? {
                    None => {
                        emitted += 1;
                        out.push(b);
                    }
                    Some(calls) => {
                        changed = true;
                        for c in calls {
                            emitted += 1;
                            let id = ids.take(c.id.as_deref(), "toolu_");
                            out.push(json!({ "type": "tool_use", "id": id, "name": c.name, "input": c.input }));
                        }
                    }
                }
            }
            _ => out.push(b),
        }
    }
    if changed {
        v["content"] = Value::Array(out);
        if seen > 0
            && emitted == 0
            && v.get("stop_reason").and_then(Value::as_str) == Some("tool_use")
        {
            v["stop_reason"] = json!("end_turn");
        }
    }
    Ok(changed)
}
