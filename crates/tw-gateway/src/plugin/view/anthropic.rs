//! Anthropic Messages 的请求视图。
//!
//! - `system`：字符串，或者文字块拼起来（块之间空一行）。改过的段落回原来的块上，
//!   块上的 `cache_control` 留着（见 [`super::segments`]）。
//! - 每条消息一条；内容是字符串的算一个文字部分，是数组的每一块一个部分。只装
//!   `tool_result` 的 user 消息角色是 `tool`；DeepSeek Harness 放在消息里的
//!   `system` 角色就是 `system`。
//! - 工具只列函数工具（没写 `type` 或者 `custom`）；服务端工具看不见、不动。
//! - 参数：`model`、`max_tokens`、`temperature`、`top_p`、`stop_sequences`。

use serde_json::{Map, Value, json};

use super::*;

/// 写回要用的位置。
pub struct Src {
    system: System,
    messages: Vec<Msg>,
    /// 视图里第 k 个工具在 `tools` 里的下标
    tools: Vec<usize>,
    pub hidden_tools: Vec<String>,
}

enum System {
    None,
    String,
    /// 有字的文字块的下标
    Blocks(Vec<usize>),
}

struct Msg {
    /// 内容是一个字符串（那就只有一个部分）
    string: bool,
}

fn content_text(c: Option<&Value>) -> (String, Vec<usize>) {
    match c {
        Some(Value::String(s)) => (s.clone(), Vec::new()),
        Some(Value::Array(blocks)) => {
            let idx: Vec<usize> = blocks
                .iter()
                .enumerate()
                .filter(|(_, b)| {
                    b.get("type").and_then(Value::as_str) == Some("text")
                        && b.get("text")
                            .and_then(Value::as_str)
                            .is_some_and(|t| !t.is_empty())
                })
                .map(|(i, _)| i)
                .collect();
            let text = idx
                .iter()
                .filter_map(|&i| blocks[i].get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (text, idx)
        }
        _ => (String::new(), Vec::new()),
    }
}

pub fn build(raw: &Value) -> Built {
    let mut view = Map::new();
    view.insert("format".into(), json!("anthropic"));
    let model = raw.get("model").and_then(Value::as_str).unwrap_or_default();
    view.insert("model".into(), json!(model));

    let (system, src_system) = match raw.get("system") {
        Some(Value::String(s)) => (s.clone(), System::String),
        Some(Value::Array(blocks)) => {
            let idx: Vec<usize> = blocks
                .iter()
                .enumerate()
                .filter(|(_, b)| {
                    b.get("type").and_then(Value::as_str) == Some("text")
                        && b.get("text")
                            .and_then(Value::as_str)
                            .is_some_and(|t| !t.is_empty())
                })
                .map(|(i, _)| i)
                .collect();
            let text = idx
                .iter()
                .filter_map(|&i| blocks[i].get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n\n");
            (text, System::Blocks(idx))
        }
        _ => (String::new(), System::None),
    };
    view.insert("system".into(), json!(system));

    let mut messages = Vec::new();
    let mut src_messages = Vec::new();
    for (i, m) in raw
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let key = msg_key(i);
        let content = m.get("content");
        let mut parts = Vec::new();
        let string = matches!(content, Some(Value::String(_)));
        match content {
            Some(Value::String(s)) => parts.push(part_text(&part_key(i, 0), s)),
            Some(Value::Array(blocks)) => {
                for (j, b) in blocks.iter().enumerate() {
                    parts.push(block(&part_key(i, j), b));
                }
            }
            _ => {}
        }
        let only_results = matches!(content, Some(Value::Array(b))
            if !b.is_empty() && b.iter().all(|b| b.get("type").and_then(Value::as_str) == Some("tool_result")));
        let role = match m.get("role").and_then(Value::as_str) {
            Some("assistant") => Role::Assistant,
            Some("system") => Role::System,
            _ if only_results => Role::Tool,
            _ => Role::User,
        };
        messages.push(json!({ "key": key, "role": role.slug(), "parts": parts }));
        src_messages.push(Msg { string });
    }
    view.insert("messages".into(), Value::Array(messages));

    let mut tools = Vec::new();
    let mut src_tools = Vec::new();
    let mut hidden_tools = Vec::new();
    for (i, t) in raw
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let name = t.get("name").and_then(Value::as_str).unwrap_or_default();
        match t.get("type").and_then(Value::as_str) {
            None | Some("custom") => {
                tools.push(json!({
                    "key": tool_key(src_tools.len()),
                    "name": name,
                    "description": t.get("description").and_then(Value::as_str).unwrap_or_default(),
                    "input_schema": t.get("input_schema").cloned().unwrap_or_else(|| json!({ "type": "object" })),
                }));
                src_tools.push(i);
            }
            Some(other) => {
                hidden_tools.push(if name.is_empty() { other } else { name }.to_string())
            }
        }
    }
    view.insert("tools".into(), Value::Array(tools));

    let mut params = Map::new();
    params.insert("model".into(), json!(model));
    if let Some(n) = raw.get("max_tokens").and_then(Value::as_u64) {
        params.insert("max_tokens".into(), json!(n));
    }
    for k in ["temperature", "top_p"] {
        if let Some(x) = raw.get(k).filter(|x| x.is_number()) {
            params.insert(k.into(), x.clone());
        }
    }
    if let Some(stop) = raw.get("stop_sequences").and_then(Value::as_array) {
        params.insert(
            "stop".into(),
            Value::Array(stop.iter().filter(|s| s.is_string()).cloned().collect()),
        );
    }
    view.insert("params".into(), Value::Object(params));

    Built {
        view: Value::Object(view),
        src: super::Src::Anthropic(Src {
            system: src_system,
            messages: src_messages,
            tools: src_tools,
            hidden_tools,
        }),
    }
}

/// 内容数组里的一块 → 视图里的一个部分
fn block(key: &str, b: &Value) -> Value {
    let s = |k: &str| b.get(k).and_then(Value::as_str).unwrap_or_default();
    match b.get("type").and_then(Value::as_str).unwrap_or_default() {
        "text" => part_text(key, s("text")),
        "thinking" => part_thinking(key, s("thinking")),
        "redacted_thinking" => part_thinking(key, ""),
        "tool_use" => part_call(
            key,
            s("id"),
            s("name"),
            b.get("input").cloned().unwrap_or_else(|| json!({})),
        ),
        "tool_result" => part_result(
            key,
            s("tool_use_id"),
            &content_text(b.get("content")).0,
            b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
        ),
        "image" => {
            let src = b.get("source");
            let media = src
                .filter(|s| s.get("type").and_then(Value::as_str) == Some("base64"))
                .and_then(|s| s.get("media_type"))
                .and_then(Value::as_str);
            part_image(key, media)
        }
        other => part_other(key, if other.is_empty() { "unknown" } else { other }),
    }
}

/// 几段文字写成内容：一段就是字符串，几段是文字块
fn text_content(texts: &[String]) -> Value {
    if texts.len() == 1 {
        json!(texts[0])
    } else {
        Value::Array(
            texts
                .iter()
                .map(|t| json!({ "type": "text", "text": t }))
                .collect(),
        )
    }
}

pub fn apply(raw: &mut Value, src: &Src, edits: &Edits) -> Result<(), EditError> {
    let Some(obj) = raw.as_object_mut() else {
        return Err(bad("the request body is not a JSON object"));
    };
    if let Some(new) = &edits.system {
        apply_system(obj, &src.system, new);
    }
    if let Some(medits) = &edits.messages {
        let msgs = obj
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::with_capacity(medits.len());
        for e in medits {
            match e {
                MsgEdit::Keep { from, parts: None } => out.push(msgs[*from].clone()),
                MsgEdit::Keep {
                    from,
                    parts: Some(parts),
                } => out.push(rebuild(&msgs[*from], &src.messages[*from], parts)?),
                MsgEdit::Insert { role, texts } => match role {
                    Role::User | Role::Assistant => out.push(json!({
                        "role": role.slug(),
                        "content": text_content(texts),
                    })),
                    _ => {
                        return Err(bad(
                            "Anthropic Messages has no system messages inside `messages`; \
                             change `system` instead",
                        ));
                    }
                },
            }
        }
        obj.insert("messages".into(), Value::Array(out));
    }
    if let Some(tedits) = &edits.tools {
        let tools = obj
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let out = tedits
            .iter()
            .map(|e| match e {
                ToolEdit::Keep {
                    from,
                    description,
                    schema,
                } => {
                    let mut t = tools[src.tools[*from]].clone();
                    if let Some(o) = t.as_object_mut() {
                        match description.as_deref() {
                            Some("") => {
                                o.remove("description");
                            }
                            Some(d) => {
                                o.insert("description".into(), json!(d));
                            }
                            None => {}
                        }
                        if let Some(s) = schema {
                            o.insert("input_schema".into(), s.clone());
                        }
                    }
                    Ok((*from, t))
                }
                ToolEdit::Insert {
                    name,
                    description,
                    schema,
                } => {
                    let mut t = Map::new();
                    t.insert("name".into(), json!(name));
                    if !description.is_empty() {
                        t.insert("description".into(), json!(description));
                    }
                    t.insert("input_schema".into(), schema.clone());
                    Err(Value::Object(t))
                }
            })
            .collect();
        let merged = merge(&tools, &src.tools, out);
        if merged.is_empty() {
            obj.remove("tools");
        } else {
            obj.insert("tools".into(), Value::Array(merged));
        }
    }
    if let Some(p) = &edits.params {
        if let Some(m) = &p.model {
            obj.insert("model".into(), json!(m));
        }
        set_opt(obj, "max_tokens", p.max_tokens.map(|o| o.map(Value::from)));
        set_opt(
            obj,
            "temperature",
            p.temperature.map(|o| o.map(Value::from)),
        );
        set_opt(obj, "top_p", p.top_p.map(|o| o.map(Value::from)));
        set_opt(
            obj,
            "stop_sequences",
            p.stop.clone().map(|o| o.map(|s| json!(s))),
        );
    }
    Ok(())
}

/// 外层 `None` 不动，`Some(None)` 去掉，`Some(Some(v))` 写上
pub(crate) fn set_opt(obj: &mut Map<String, Value>, key: &str, change: Option<Option<Value>>) {
    match change {
        None => {}
        Some(None) => {
            obj.remove(key);
        }
        Some(Some(v)) => {
            obj.insert(key.to_string(), v);
        }
    }
}

fn apply_system(obj: &mut Map<String, Value>, src: &System, new: &str) {
    match src {
        System::None | System::String => {
            if new.is_empty() {
                obj.remove("system");
            } else {
                obj.insert("system".into(), json!(new));
            }
        }
        System::Blocks(idx) => {
            let blocks = obj
                .get("system")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let segs: Vec<&str> = idx
                .iter()
                .filter_map(|&i| blocks[i].get("text").and_then(Value::as_str))
                .collect();
            let out = segment_diff(&segs, "\n\n", new)
                .into_iter()
                .filter_map(|s| match s {
                    Seg::Keep(k) => Some(Ok((k, blocks[idx[k]].clone()))),
                    // 空的文字块 Anthropic 不收：换成空的就是去掉
                    Seg::Replace(_, t) if t.is_empty() => None,
                    Seg::Replace(k, t) => {
                        let mut b = blocks[idx[k]].clone();
                        b["text"] = json!(t);
                        Some(Ok((k, b)))
                    }
                    Seg::Insert(t) if t.is_empty() => None,
                    Seg::Insert(t) => Some(Err(json!({ "type": "text", "text": t }))),
                })
                .collect();
            let merged = merge(&blocks, idx, out);
            if merged.is_empty() {
                obj.remove("system");
            } else {
                obj.insert("system".into(), Value::Array(merged));
            }
        }
    }
}

fn rebuild(m: &Value, src: &Msg, parts: &[PartEdit]) -> Result<Value, EditError> {
    let mut m = m.clone();
    if src.string {
        let orig = m
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // 只改了这一段字：还是字符串
        if let [PartEdit::Keep { change, .. }] = parts {
            if let Some(Change::Text(t)) = change {
                m["content"] = json!(t);
            }
            return Ok(m);
        }
        let blocks: Vec<Value> = parts
            .iter()
            .map(|p| match p {
                PartEdit::Keep { change, .. } => {
                    let t = match change {
                        Some(Change::Text(t)) => t.clone(),
                        _ => orig.clone(),
                    };
                    json!({ "type": "text", "text": t })
                }
                PartEdit::Insert(t) => json!({ "type": "text", "text": t }),
            })
            .collect();
        m["content"] = Value::Array(blocks);
        return Ok(m);
    }
    let blocks = m
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::with_capacity(parts.len());
    for p in parts {
        match p {
            PartEdit::Keep { from, change } => {
                let mut b = blocks[*from].clone();
                match change {
                    Some(Change::Text(t)) => b["text"] = json!(t),
                    Some(Change::Input(v)) => {
                        if !v.is_object() {
                            return Err(bad(
                                "the input of an Anthropic tool_use block must be an object",
                            ));
                        }
                        b["input"] = v.clone();
                    }
                    Some(Change::Result(t)) => set_result(&mut b, t),
                    None => {}
                }
                out.push(b);
            }
            PartEdit::Insert(t) => out.push(json!({ "type": "text", "text": t })),
        }
    }
    m["content"] = Value::Array(out);
    Ok(m)
}

/// 工具结果的文字写回去：字符串就换掉；数组里的文字块按段对回去，图片留在原位
fn set_result(b: &mut Value, t: &str) {
    match b.get("content") {
        Some(Value::Array(items)) => {
            let items = items.clone();
            let (_, idx) = content_text(Some(&Value::Array(items.clone())));
            let segs: Vec<&str> = idx
                .iter()
                .filter_map(|&i| items[i].get("text").and_then(Value::as_str))
                .collect();
            let out = segment_diff(&segs, "\n", t)
                .into_iter()
                .filter_map(|s| match s {
                    Seg::Keep(k) => Some(Ok((k, items[idx[k]].clone()))),
                    Seg::Replace(_, t) if t.is_empty() => None,
                    Seg::Replace(k, t) => {
                        let mut x = items[idx[k]].clone();
                        x["text"] = json!(t);
                        Some(Ok((k, x)))
                    }
                    Seg::Insert(t) if t.is_empty() => None,
                    Seg::Insert(t) => Some(Err(json!({ "type": "text", "text": t }))),
                })
                .collect();
            b["content"] = Value::Array(merge(&items, &idx, out));
        }
        _ => b["content"] = json!(t),
    }
}
