//! OpenAI Chat Completions 的回答。
//!
//! Chat 的流没有块边界：每个选项（`n` > 1 时有好几个，帧里按选项的 `index` 分）的正文
//! 是 `delta.content` 一路；换成推理、开始工具调用、`finish_reason`、`[DONE]` 都算这一块
//! 结束。工具调用按 `index` 分片，OpenAI 一个发完再发下一个，所以换了 `index` 或者到了
//! `finish_reason` 就是上一个收齐了。
//!
//! **每个选项各算各的**：正文一个选项一块（块号就是选项的 `index`），工具调用各攒各的、
//! 序号各数各的，`finish_reason` 只结束它那一个选项；`[DONE]`、错误帧、用量块把所有选项
//! 一起收尾。
//!
//! 工具调用的分片从原来的帧里摘掉，收齐之后**写成一帧完整的调用**（不变的也是：一帧
//! 和几帧拼起来是同一个调用），序号按写出去的顺序重新数。一个选项的调用全被去掉了的话，
//! 它的 `finish_reason` 从 `tool_calls` 改成 `stop`。

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::{Call, Chain, Frame, Ids, Out, whole_text};
use crate::error::GatewayError;
use crate::plugin::view::{args_text, args_value};

pub(crate) struct Codec {
    wants_text: bool,
    wants_tools: bool,
    /// 每个选项一份，按选项的 `index`
    choices: BTreeMap<u64, Choice>,
    /// 最近一帧的外壳（id、created、model……），补帧时抄它
    envelope: Map<String, Value>,
    ids: Ids,
}

/// 一个选项的正文和工具调用。正文的块号就是选项的 `index`
#[derive(Default)]
struct Choice {
    lane_open: bool,
    calls: Vec<CallBuf>,
    /// 写出去的下一个调用的序号
    next_index: u64,
    tools_seen: u64,
    tools_emitted: u64,
}

struct CallBuf {
    index: u64,
    id: String,
    custom: bool,
    name: String,
    args: String,
    /// 第一片：补帧时抄它别的字段
    first: Value,
}

/// 帧里一个选项的 `index`。没写的按它在数组里的位置
fn index_of(choice: &Value, pos: usize) -> u64 {
    choice
        .get("index")
        .and_then(Value::as_u64)
        .unwrap_or(pos as u64)
}

impl Codec {
    pub(super) fn new(chain: &Chain) -> Self {
        Self {
            wants_text: chain.wants_text(),
            wants_tools: chain.wants_tools(),
            choices: BTreeMap::new(),
            envelope: Map::new(),
            ids: Ids::new(),
        }
    }

    fn at(&mut self, k: u64) -> &mut Choice {
        self.choices.entry(k).or_default()
    }

    fn chunk(&self, k: u64, delta: Value) -> Out {
        let mut m = self.envelope.clone();
        m.insert(
            "choices".into(),
            json!([{ "index": k, "delta": delta, "finish_reason": null }]),
        );
        Out::new(None, Value::Object(m))
    }

    async fn close_lane(
        &mut self,
        chain: &mut Chain,
        k: u64,
        out: &mut Vec<Out>,
    ) -> Result<(), GatewayError> {
        let c = self.at(k);
        if !c.lane_open {
            return Ok(());
        }
        c.lane_open = false;
        let end = chain.text_end(k).await?;
        if !end.is_empty() {
            out.push(self.chunk(k, json!({ "content": end })));
        }
        Ok(())
    }

    /// 选项 `k` 攒着的调用都收齐了：交给插件，写出去
    async fn settle(
        &mut self,
        chain: &mut Chain,
        k: u64,
        out: &mut Vec<Out>,
    ) -> Result<(), GatewayError> {
        for c in std::mem::take(&mut self.at(k).calls) {
            self.at(k).tools_seen += 1;
            let input = if c.custom {
                Value::String(c.args.clone())
            } else {
                args_value(&c.args)
            };
            let call = Call {
                id: Some(c.id.clone()),
                name: c.name.clone(),
                input,
            };
            let written: Vec<(String, String, String)> = match chain.tool_call(call).await? {
                None => {
                    self.ids.note(&c.id);
                    vec![(c.id.clone(), c.name.clone(), c.args.clone())]
                }
                Some(calls) => calls
                    .into_iter()
                    .map(|n| {
                        let id = self.ids.take(n.id.as_deref(), "call_");
                        let args = if c.custom {
                            args_text(&n.input)
                        } else {
                            n.input.to_string()
                        };
                        (id, n.name, args)
                    })
                    .collect(),
            };
            for (id, name, args) in written {
                let mut entry = c.first.clone();
                let ch = self.at(k);
                entry["index"] = json!(ch.next_index);
                ch.next_index += 1;
                ch.tools_emitted += 1;
                entry["id"] = json!(id);
                if c.custom {
                    entry["type"] = json!("custom");
                    entry["custom"] = json!({ "name": name, "input": args });
                } else {
                    entry["type"] = json!("function");
                    let mut f = entry
                        .get("function")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    f.insert("name".into(), json!(name));
                    f.insert("arguments".into(), json!(args));
                    entry["function"] = Value::Object(f);
                }
                out.push(self.chunk(k, json!({ "tool_calls": [entry] })));
            }
        }
        Ok(())
    }

    /// 所有选项一起收尾：扣着的正文、攒着的调用都补出来
    async fn close_all(
        &mut self,
        chain: &mut Chain,
        out: &mut Vec<Out>,
    ) -> Result<(), GatewayError> {
        let ks: Vec<u64> = self.choices.keys().copied().collect();
        for k in ks {
            self.close_lane(chain, k, out).await?;
            self.settle(chain, k, out).await?;
        }
        Ok(())
    }

    pub(super) async fn frame(
        &mut self,
        chain: &mut Chain,
        mut f: Frame,
        out: &mut Vec<Out>,
    ) -> Result<(), GatewayError> {
        let Some(data) = f.data.as_mut() else {
            // `[DONE]`：攒着的赶在它前面补出来
            self.close_all(chain, out).await?;
            out.push(Out::Keep(f));
            return Ok(());
        };
        if data.get("error").is_some_and(|e| !e.is_null()) {
            self.close_all(chain, out).await?;
            out.push(Out::Keep(f));
            return Ok(());
        }
        if let Some(o) = data.as_object() {
            for k in ["id", "object", "created", "model", "system_fingerprint"] {
                if let Some(v) = o.get(k) {
                    self.envelope.insert(k.into(), v.clone());
                }
            }
        }
        let has_usage = data.get("usage").is_some_and(|u| !u.is_null());
        let Some(choices) = data
            .get_mut("choices")
            .and_then(Value::as_array_mut)
            .filter(|c| !c.is_empty())
        else {
            // 没有 choices 的（流末尾的用量块）：之前攒着的先补出来
            self.close_all(chain, out).await?;
            out.push(Out::Keep(f));
            return Ok(());
        };
        let mut before: Vec<Out> = Vec::new();
        let mut dirty = false;
        let mut kept = Vec::with_capacity(choices.len());
        for (pos, mut choice) in std::mem::take(choices).into_iter().enumerate() {
            let k = index_of(&choice, pos);
            let changed = self.choice(chain, k, &mut choice, &mut before).await?;
            dirty |= changed;
            // 摘空了的选项不发：没有正文、没有工具调用、没有结束原因，帧上也没有用量
            let empty = choice
                .get("delta")
                .and_then(Value::as_object)
                .is_none_or(|d| d.values().all(|v| v.is_null() || v == ""))
                && choice.get("finish_reason").is_none_or(Value::is_null)
                && !has_usage;
            if !(changed && empty) {
                kept.push(choice);
            }
        }
        out.extend(before);
        if kept.is_empty() {
            return Ok(());
        }
        *choices = kept;
        f.dirty |= dirty;
        out.push(Out::Keep(f));
        Ok(())
    }

    /// 帧里选项 `k` 的那一份。补出来的帧放进 `before`，返回这一份改没改
    async fn choice(
        &mut self,
        chain: &mut Chain,
        k: u64,
        choice: &mut Value,
        before: &mut Vec<Out>,
    ) -> Result<bool, GatewayError> {
        let mut dirty = false;
        let finish = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(str::to_string);
        let delta = choice.get_mut("delta").and_then(Value::as_object_mut);
        if let Some(delta) = delta {
            let thinking = ["reasoning_content", "reasoning"].iter().any(|k| {
                delta
                    .get(*k)
                    .and_then(Value::as_str)
                    .is_some_and(|t| !t.is_empty())
            });
            if thinking {
                self.close_lane(chain, k, before).await?;
            }
            if self.wants_text
                && let Some(text) = delta
                    .get("content")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                    .map(str::to_string)
            {
                if !self.at(k).calls.is_empty() {
                    self.settle(chain, k, before).await?;
                }
                self.at(k).lane_open = true;
                let mut got = chain.text(k, &text).await?;
                if finish.is_some() {
                    // 这一帧就是结尾：扣着的接在它自己的正文后面
                    self.at(k).lane_open = false;
                    got.push_str(&chain.text_end(k).await?);
                }
                if got != text {
                    dirty = true;
                    if got.is_empty() {
                        delta.remove("content");
                    } else {
                        delta.insert("content".into(), json!(got));
                    }
                }
            }
            if self.wants_tools
                && let Some(entries) = delta.get("tool_calls").and_then(Value::as_array).cloned()
                && !entries.is_empty()
            {
                self.close_lane(chain, k, before).await?;
                for (pos, e) in entries.iter().enumerate() {
                    let i = e.get("index").and_then(Value::as_u64).unwrap_or(pos as u64);
                    let known = self.at(k).calls.iter().any(|c| c.index == i);
                    if !known {
                        // 新的一个调用开始了：这个选项前面的都收齐了
                        if !self.at(k).calls.is_empty() {
                            self.settle(chain, k, before).await?;
                        }
                        let custom = e.get("type").and_then(Value::as_str) == Some("custom");
                        let inner = if custom {
                            e.get("custom")
                        } else {
                            e.get("function")
                        };
                        let name = inner
                            .and_then(|x| x.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let mut first = e.clone();
                        if let Some(o) = first.as_object_mut() {
                            o.remove("index");
                        }
                        self.at(k).calls.push(CallBuf {
                            index: i,
                            id: e
                                .get("id")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| tw_dialect::ir::new_id("call_")),
                            custom,
                            name,
                            args: String::new(),
                            first,
                        });
                    }
                    let c = self
                        .at(k)
                        .calls
                        .iter_mut()
                        .find(|c| c.index == i)
                        .expect("just pushed");
                    let piece = if c.custom {
                        e.pointer("/custom/input")
                    } else {
                        e.pointer("/function/arguments")
                    };
                    if let Some(p) = piece.and_then(Value::as_str) {
                        c.args.push_str(p);
                    }
                }
                delta.remove("tool_calls");
                dirty = true;
            }
        }
        if let Some(reason) = finish {
            self.close_lane(chain, k, before).await?;
            self.settle(chain, k, before).await?;
            let c = self.at(k);
            if reason == "tool_calls" && c.tools_seen > 0 && c.tools_emitted == 0 {
                choice["finish_reason"] = json!("stop");
                dirty = true;
            }
        }
        Ok(dirty)
    }

    pub(super) async fn finish(
        &mut self,
        chain: &mut Chain,
        out: &mut Vec<Out>,
    ) -> Result<(), GatewayError> {
        self.close_all(chain, out).await
    }
}

/// 整包：每个选项（`n` > 1 时有好几个）的 `message` 里的正文和工具调用
pub(super) async fn whole(chain: &mut Chain, v: &mut Value) -> Result<bool, GatewayError> {
    let Some(choices) = v.get_mut("choices").and_then(Value::as_array_mut) else {
        return Ok(false);
    };
    let mut changed = false;
    for (pos, choice) in choices.iter_mut().enumerate() {
        let k = index_of(choice, pos);
        changed |= whole_choice(chain, k, choice).await?;
    }
    Ok(changed)
}

/// 整包里的一个选项。正文的块号是选项的 `index`
async fn whole_choice(chain: &mut Chain, k: u64, choice: &mut Value) -> Result<bool, GatewayError> {
    let Some(m) = choice.get_mut("message").and_then(Value::as_object_mut) else {
        return Ok(false);
    };
    let mut changed = false;
    if chain.wants_text()
        && let Some(t) = m.get("content").and_then(Value::as_str).map(str::to_string)
        && !t.is_empty()
    {
        let got = whole_text(chain, k, &t).await?;
        if got != t {
            m.insert("content".into(), json!(got));
            changed = true;
        }
    }
    let mut all_dropped = false;
    if chain.wants_tools()
        && let Some(calls) = m.get("tool_calls").and_then(Value::as_array).cloned()
        && !calls.is_empty()
    {
        let mut ids = Ids::new();
        for c in &calls {
            if let Some(id) = c.get("id").and_then(Value::as_str) {
                ids.note(id);
            }
        }
        let mut out = Vec::with_capacity(calls.len());
        let mut touched = false;
        for c in calls {
            let custom = c.get("type").and_then(Value::as_str) == Some("custom");
            let (name, input) = if custom {
                (
                    c.pointer("/custom/name"),
                    c.pointer("/custom/input")
                        .and_then(Value::as_str)
                        .map(|s| Value::String(s.to_string())),
                )
            } else {
                (
                    c.pointer("/function/name"),
                    c.pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .map(args_value),
                )
            };
            let call = Call {
                id: c.get("id").and_then(Value::as_str).map(str::to_string),
                name: name.and_then(Value::as_str).unwrap_or_default().to_string(),
                input: input.unwrap_or_else(|| json!({})),
            };
            match chain.tool_call(call).await? {
                None => out.push(c),
                Some(new) => {
                    touched = true;
                    for n in new {
                        let id = ids.take(n.id.as_deref(), "call_");
                        out.push(if custom {
                            json!({ "id": id, "type": "custom", "custom": { "name": n.name, "input": args_text(&n.input) } })
                        } else {
                            json!({ "id": id, "type": "function", "function": { "name": n.name, "arguments": n.input.to_string() } })
                        });
                    }
                }
            }
        }
        if touched {
            changed = true;
            all_dropped = out.is_empty();
            if out.is_empty() {
                m.remove("tool_calls");
            } else {
                m.insert("tool_calls".into(), Value::Array(out));
            }
        }
    }
    if all_dropped
        && let Some(f) = choice.get_mut("finish_reason")
        && f == "tool_calls"
    {
        *f = json!("stop");
    }
    Ok(changed)
}
