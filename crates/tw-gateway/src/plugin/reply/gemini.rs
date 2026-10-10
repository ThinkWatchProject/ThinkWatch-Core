//! Gemini 的回答。
//!
//! 每一帧都是一个完整的响应对象：文字部分是增量，函数调用整个在一个部分里。正文是
//! 一路：碰到推理部分、函数调用或者 `finishReason` 就是这一块结束了，插件扣着的补成
//! 一个文字部分。函数调用不用攒，那一个部分就是完整的调用；换出来的几个各写成一个
//! `functionCall` 部分，**推理签名（`thoughtSignature`）留在第一个上**，去掉的那个的
//! 签名挪给同一帧里下一个调用 —— Gemini 下一轮要看到它。
//!
//! 一帧里的部分全被扣下了：没有结束原因就整帧不发，有的话留一个空的文字部分。
//! 不带 `alt=sse` 的客户端收到的是 JSON 数组，拆帧、拼数组在上一层。

use serde_json::{Map, Value, json};

use super::{Call, Chain, Frame, Ids, Out, whole_text};
use crate::error::GatewayError;

const LANE: u64 = 0;

pub(crate) struct Codec {
    wants_text: bool,
    wants_tools: bool,
    lane_open: bool,
    /// 最近一帧的外壳（`modelVersion`、`responseId`），流结束时补帧抄它
    envelope: Map<String, Value>,
    ids: Ids,
}

/// 驼峰或者下划线写法的字段
fn field<'a>(v: &'a Value, camel: &str, snake: &str) -> Option<&'a Value> {
    v.get(camel).or_else(|| v.get(snake))
}

impl Codec {
    pub(super) fn new(chain: &Chain) -> Self {
        Self {
            wants_text: chain.wants_text(),
            wants_tools: chain.wants_tools(),
            lane_open: false,
            envelope: Map::new(),
            ids: Ids::new(),
        }
    }

    async fn end(&mut self, chain: &mut Chain) -> Result<String, GatewayError> {
        if !self.lane_open {
            return Ok(String::new());
        }
        self.lane_open = false;
        chain.text_end(LANE).await
    }

    fn chunk(&self, text: String) -> Out {
        let mut m = self.envelope.clone();
        m.insert(
            "candidates".into(),
            json!([{ "content": { "role": "model", "parts": [{ "text": text }] }, "index": 0 }]),
        );
        Out::new(None, Value::Object(m))
    }

    pub(super) async fn frame(
        &mut self,
        chain: &mut Chain,
        mut f: Frame,
        out: &mut Vec<Out>,
    ) -> Result<(), GatewayError> {
        let Some(data) = f.data.as_mut() else {
            // JSON 数组的 `]`、心跳：之前扣着的先补出来
            if f.raw == b"]" {
                let end = self.end(chain).await?;
                if !end.is_empty() {
                    out.push(self.chunk(end));
                }
            }
            out.push(Out::Keep(f));
            return Ok(());
        };
        for k in ["modelVersion", "responseId", "model_version", "response_id"] {
            if let Some(v) = data.get(k) {
                self.envelope.insert(k.into(), v.clone());
            }
        }
        if data.get("error").is_some_and(|e| !e.is_null()) {
            let end = self.end(chain).await?;
            if !end.is_empty() {
                out.push(self.chunk(end));
            }
            out.push(Out::Keep(f));
            return Ok(());
        }
        let Some(cand) = data
            .get_mut("candidates")
            .and_then(Value::as_array_mut)
            .and_then(|c| c.get_mut(0))
        else {
            out.push(Out::Keep(f));
            return Ok(());
        };
        let finished = field(cand, "finishReason", "finish_reason").is_some_and(|r| !r.is_null());
        let parts = cand
            .pointer("/content/parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut next: Vec<Value> = Vec::with_capacity(parts.len());
        let mut changed = false;
        // 去掉的调用身上的签名，交给下一个调用
        let mut carry: Option<(String, Value)> = None;
        for mut p in parts {
            let text = p.get("text").and_then(Value::as_str).map(str::to_string);
            let thought = p.get("thought").and_then(Value::as_bool) == Some(true);
            if let Some(t) = text {
                if thought || !self.wants_text {
                    if thought && self.lane_open {
                        let end = self.end(chain).await?;
                        if !end.is_empty() {
                            next.push(json!({ "text": end }));
                            changed = true;
                        }
                    }
                    next.push(p);
                    continue;
                }
                self.lane_open = true;
                let got = chain.text(LANE, &t).await?;
                if got == t {
                    next.push(p);
                    continue;
                }
                changed = true;
                let others = p.as_object().is_some_and(|o| o.len() > 1);
                if got.is_empty() && !others {
                    continue;
                }
                p["text"] = json!(got);
                next.push(p);
                continue;
            }
            let call = field(&p, "functionCall", "function_call").cloned();
            let Some(fc) = call else {
                next.push(p);
                continue;
            };
            let end = self.end(chain).await?;
            if !end.is_empty() {
                next.push(json!({ "text": end }));
                changed = true;
            }
            if !self.wants_tools {
                next.push(p);
                continue;
            }
            let had_id = fc.get("id").and_then(Value::as_str);
            if let Some(id) = had_id {
                self.ids.note(id);
            }
            let name = fc
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let call = Call {
                id: had_id
                    .map(str::to_string)
                    .or_else(|| Some(tw_dialect::ir::new_id("call_"))),
                name,
                input: fc.get("args").cloned().unwrap_or_else(|| json!({})),
            };
            let signature = ["thoughtSignature", "thought_signature"]
                .iter()
                .find_map(|k| p.get(*k).map(|v| (k.to_string(), v.clone())));
            match chain.tool_call(call).await? {
                None => {
                    if let (Some((k, v)), None) = (carry.take(), &signature)
                        && let Some(o) = p.as_object_mut()
                    {
                        o.insert(k, v);
                    }
                    next.push(p);
                }
                Some(calls) => {
                    changed = true;
                    if calls.is_empty() {
                        if carry.is_none() {
                            carry = signature;
                        }
                        continue;
                    }
                    for (k, c) in calls.into_iter().enumerate() {
                        let mut call = json!({ "name": c.name, "args": c.input });
                        if had_id.is_some() {
                            call["id"] = json!(self.ids.take(c.id.as_deref(), "call_"));
                        }
                        let mut part = if k == 0 {
                            // 第一个接着用原来那个部分上的别的字段（签名）
                            let mut first = p.clone();
                            if let Some(o) = first.as_object_mut() {
                                o.remove("functionCall");
                                o.remove("function_call");
                            }
                            first
                        } else {
                            json!({})
                        };
                        part["functionCall"] = call;
                        if k == 0
                            && signature.is_none()
                            && let Some((sk, sv)) = carry.take()
                        {
                            part[sk] = sv;
                        }
                        next.push(part);
                    }
                }
            }
        }
        if finished && self.lane_open {
            let end = self.end(chain).await?;
            if !end.is_empty() {
                changed = true;
                match next.last_mut() {
                    Some(last)
                        if last.get("text").is_some()
                            && last.get("thought").and_then(Value::as_bool) != Some(true) =>
                    {
                        let t = last["text"].as_str().unwrap_or_default().to_string();
                        last["text"] = json!(format!("{t}{end}"));
                    }
                    _ => next.push(json!({ "text": end })),
                }
            }
        }
        if !changed {
            out.push(Out::Keep(f));
            return Ok(());
        }
        if next.is_empty() {
            if !finished {
                // 这一帧里的字都扣着：不发
                return Ok(());
            }
            next.push(json!({ "text": "" }));
        }
        if let Some(c) = cand.get_mut("content") {
            c["parts"] = Value::Array(next);
        }
        f.dirty = true;
        out.push(Out::Keep(f));
        Ok(())
    }

    pub(super) async fn finish(
        &mut self,
        chain: &mut Chain,
        out: &mut Vec<Out>,
    ) -> Result<(), GatewayError> {
        let end = self.end(chain).await?;
        if !end.is_empty() {
            out.push(self.chunk(end));
        }
        Ok(())
    }
}

/// 整包：`candidates[0].content.parts` 里的文字和函数调用。每个文字部分算一块
pub(super) async fn whole(chain: &mut Chain, v: &mut Value) -> Result<bool, GatewayError> {
    let Some(parts) = v
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .cloned()
    else {
        return Ok(false);
    };
    let (text, tools) = (chain.wants_text(), chain.wants_tools());
    let mut ids = Ids::new();
    let mut out = Vec::with_capacity(parts.len());
    let mut changed = false;
    for (lane, mut p) in parts.into_iter().enumerate() {
        let thought = p.get("thought").and_then(Value::as_bool) == Some(true);
        if let Some(t) = p.get("text").and_then(Value::as_str).map(str::to_string) {
            if text && !thought {
                let got = whole_text(chain, lane as u64, &t).await?;
                if got != t {
                    p["text"] = json!(got);
                    changed = true;
                }
            }
            out.push(p);
            continue;
        }
        let Some(fc) = field(&p, "functionCall", "function_call")
            .cloned()
            .filter(|_| tools)
        else {
            out.push(p);
            continue;
        };
        let had_id = fc.get("id").and_then(Value::as_str);
        let call = Call {
            id: had_id.map(str::to_string),
            name: fc
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            input: fc.get("args").cloned().unwrap_or_else(|| json!({})),
        };
        match chain.tool_call(call).await? {
            None => out.push(p),
            Some(calls) => {
                changed = true;
                for (k, c) in calls.into_iter().enumerate() {
                    let mut call = json!({ "name": c.name, "args": c.input });
                    if had_id.is_some() {
                        call["id"] = json!(ids.take(c.id.as_deref(), "call_"));
                    }
                    let mut part = if k == 0 {
                        let mut first = p.clone();
                        if let Some(o) = first.as_object_mut() {
                            o.remove("functionCall");
                            o.remove("function_call");
                        }
                        first
                    } else {
                        json!({})
                    };
                    part["functionCall"] = call;
                    out.push(part);
                }
            }
        }
    }
    if changed && let Some(c) = v.pointer_mut("/candidates/0/content") {
        c["parts"] = Value::Array(out);
    }
    Ok(changed)
}
