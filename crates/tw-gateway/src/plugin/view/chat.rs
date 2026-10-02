//! OpenAI Chat Completions 的请求视图。
//!
//! - `system`：开头那几条 system / developer 消息，每条一段，段之间空一行。
//! - 其余每条消息一条：对话中途的 system / developer 是 `system`，`tool` 消息是
//!   `tool`（一个工具结果）。assistant 消息的部分依次是推理（`reasoning_content`）、
//!   正文、工具调用。
//! - 工具只列函数工具；自定义（自由格式）工具看不见、不动。
//! - 参数：`model`、`max_completion_tokens`（没有就是 `max_tokens`）、`temperature`、
//!   `top_p`、`stop`。

use serde_json::{Map, Value, json};

use super::*;

pub struct Src {
    /// 开头的 system / developer 消息有几条
    lead: usize,
    /// 其中有字的那几条的下标（系统提示的段）
    system: Vec<usize>,
    messages: Vec<Msg>,
    tools: Vec<usize>,
    pub hidden_tools: Vec<String>,
    /// 输出上限写在哪个字段
    max_key: &'static str,
    /// `stop` 原来是一个字符串
    stop_string: bool,
}

struct Msg {
    /// 在 `messages` 里的下标
    at: usize,
    kind: Kind,
    parts: Vec<At>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// 内容是字符串或者部分数组的消息（user、system、developer、assistant）
    Content,
    /// `tool` 消息：整条就是一个工具结果
    Tool,
    /// 认不出的角色：整条只读
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum At {
    ContentString,
    Content(usize),
    Reasoning,
    ToolCall(usize),
    Whole,
}

fn text_of(c: Option<&Value>) -> String {
    match c {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn is_system(m: &Value) -> bool {
    matches!(
        m.get("role").and_then(Value::as_str),
        Some("system" | "developer")
    )
}

pub fn build(raw: &Value) -> Built {
    let mut view = Map::new();
    view.insert("format".into(), json!("openai_chat"));
    let model = raw.get("model").and_then(Value::as_str).unwrap_or_default();
    view.insert("model".into(), json!(model));

    let empty = Vec::new();
    let msgs = raw
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let lead = msgs.iter().take_while(|m| is_system(m)).count();
    let system: Vec<usize> = (0..lead)
        .filter(|&i| !text_of(msgs[i].get("content")).is_empty())
        .collect();
    let system_text = system
        .iter()
        .map(|&i| text_of(msgs[i].get("content")))
        .collect::<Vec<_>>()
        .join("\n\n");
    view.insert("system".into(), json!(system_text));

    let mut messages = Vec::new();
    let mut src_messages = Vec::new();
    for (k, at) in (lead..msgs.len()).enumerate() {
        let m = &msgs[at];
        let mut parts: Vec<Value> = Vec::new();
        let mut at_list: Vec<At> = Vec::new();
        let key = |j: usize| part_key(k, j);
        let (role, kind) = match m.get("role").and_then(Value::as_str).unwrap_or_default() {
            "tool" => {
                parts.push(part_result(
                    &key(0),
                    m.get("tool_call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    &text_of(m.get("content")),
                    false,
                ));
                at_list.push(At::Whole);
                (Role::Tool, Kind::Tool)
            }
            r @ ("user" | "assistant" | "system" | "developer") => {
                let assistant = r == "assistant";
                if assistant
                    && let Some(t) = m
                        .get("reasoning_content")
                        .or_else(|| m.get("reasoning"))
                        .and_then(Value::as_str)
                {
                    parts.push(part_thinking(&key(parts.len()), t));
                    at_list.push(At::Reasoning);
                }
                match m.get("content") {
                    Some(Value::String(s)) => {
                        parts.push(part_text(&key(parts.len()), s));
                        at_list.push(At::ContentString);
                    }
                    Some(Value::Array(items)) => {
                        for (c, p) in items.iter().enumerate() {
                            parts.push(content_part(&key(parts.len()), p));
                            at_list.push(At::Content(c));
                        }
                    }
                    _ => {}
                }
                if assistant {
                    for (c, call) in m
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .enumerate()
                    {
                        parts.push(tool_call(&key(parts.len()), call));
                        at_list.push(At::ToolCall(c));
                    }
                }
                let role = match r {
                    "user" => Role::User,
                    "assistant" => Role::Assistant,
                    _ => Role::System,
                };
                (role, Kind::Content)
            }
            other => {
                parts.push(part_other(
                    &key(0),
                    if other.is_empty() { "unknown" } else { other },
                ));
                at_list.push(At::Whole);
                (Role::User, Kind::Other)
            }
        };
        messages.push(json!({ "key": msg_key(k), "role": role.slug(), "parts": parts }));
        src_messages.push(Msg {
            at,
            kind,
            parts: at_list,
        });
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
        let kind = t.get("type").and_then(Value::as_str).unwrap_or_default();
        let inner = t.get(kind).unwrap_or(&Value::Null);
        let name = inner
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind == "function" {
            tools.push(json!({
                "key": tool_key(src_tools.len()),
                "name": name,
                "description": inner.get("description").and_then(Value::as_str).unwrap_or_default(),
                "input_schema": inner
                    .get("parameters")
                    .filter(|p| p.is_object())
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
            }));
            src_tools.push(i);
        } else {
            hidden_tools.push(if name.is_empty() { kind } else { name }.to_string());
        }
    }
    view.insert("tools".into(), Value::Array(tools));

    let mut params = Map::new();
    params.insert("model".into(), json!(model));
    let max_key = if raw.get("max_completion_tokens").is_some() {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };
    if let Some(n) = raw.get(max_key).and_then(Value::as_u64) {
        params.insert("max_tokens".into(), json!(n));
    }
    for k in ["temperature", "top_p"] {
        if let Some(x) = raw.get(k).filter(|x| x.is_number()) {
            params.insert(k.into(), x.clone());
        }
    }
    let stop_string = matches!(raw.get("stop"), Some(Value::String(_)));
    match raw.get("stop") {
        Some(Value::String(s)) => {
            params.insert("stop".into(), json!([s]));
        }
        Some(Value::Array(a)) => {
            params.insert(
                "stop".into(),
                Value::Array(a.iter().filter(|s| s.is_string()).cloned().collect()),
            );
        }
        _ => {}
    }
    view.insert("params".into(), Value::Object(params));

    Built {
        view: Value::Object(view),
        src: super::Src::Chat(Src {
            lead,
            system,
            messages: src_messages,
            tools: src_tools,
            hidden_tools,
            max_key,
            stop_string,
        }),
    }
}

/// 内容数组里的一项 → 视图里的一个部分
fn content_part(key: &str, p: &Value) -> Value {
    match p.get("type").and_then(Value::as_str).unwrap_or_default() {
        "text" => part_text(
            key,
            p.get("text").and_then(Value::as_str).unwrap_or_default(),
        ),
        "refusal" => part_text(
            key,
            p.get("refusal").and_then(Value::as_str).unwrap_or_default(),
        ),
        "image_url" => part_image(
            key,
            p.get("image_url")
                .and_then(|i| i.get("url"))
                .and_then(Value::as_str)
                .and_then(data_uri_mime),
        ),
        other => part_other(key, if other.is_empty() { "unknown" } else { other }),
    }
}

fn tool_call(key: &str, c: &Value) -> Value {
    let id = c.get("id").and_then(Value::as_str).unwrap_or_default();
    if c.get("type").and_then(Value::as_str) == Some("custom") {
        let x = c.get("custom").unwrap_or(&Value::Null);
        return part_call(
            key,
            id,
            x.get("name").and_then(Value::as_str).unwrap_or_default(),
            json!(x.get("input").and_then(Value::as_str).unwrap_or_default()),
        );
    }
    let f = c.get("function").unwrap_or(&Value::Null);
    part_call(
        key,
        id,
        f.get("name").and_then(Value::as_str).unwrap_or_default(),
        args_value(
            f.get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
    )
}

pub fn apply(raw: &mut Value, src: &Src, edits: &Edits) -> Result<(), EditError> {
    let Some(obj) = raw.as_object_mut() else {
        return Err(bad("the request body is not a JSON object"));
    };
    if edits.system.is_some() || edits.messages.is_some() {
        let msgs = obj
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let lead: Vec<Value> = msgs[..src.lead].to_vec();
        let lead = match &edits.system {
            None => lead,
            Some(new) => system(&lead, &src.system, new),
        };
        let rest = match &edits.messages {
            None => msgs[src.lead..].to_vec(),
            Some(medits) => {
                let mut out = Vec::with_capacity(medits.len());
                for e in medits {
                    match e {
                        MsgEdit::Keep { from, parts } => {
                            let m = &src.messages[*from];
                            out.push(match parts {
                                None => msgs[m.at].clone(),
                                Some(p) => rebuild(&msgs[m.at], m, p)?,
                            });
                        }
                        MsgEdit::Insert { role, texts } => out.push(json!({
                            "role": role.slug(),
                            "content": text_content(texts),
                        })),
                    }
                }
                out
            }
        };
        let mut all = lead;
        all.extend(rest);
        obj.insert("messages".into(), Value::Array(all));
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
                    if let Some(f) = t.get_mut("function").and_then(Value::as_object_mut) {
                        match description.as_deref() {
                            Some("") => {
                                f.remove("description");
                            }
                            Some(d) => {
                                f.insert("description".into(), json!(d));
                            }
                            None => {}
                        }
                        if let Some(s) = schema {
                            f.insert("parameters".into(), s.clone());
                        }
                    }
                    Ok((*from, t))
                }
                ToolEdit::Insert {
                    name,
                    description,
                    schema,
                } => {
                    let mut f = Map::new();
                    f.insert("name".into(), json!(name));
                    if !description.is_empty() {
                        f.insert("description".into(), json!(description));
                    }
                    f.insert("parameters".into(), schema.clone());
                    Err(json!({ "type": "function", "function": f }))
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
        anthropic::set_opt(obj, src.max_key, p.max_tokens.map(|o| o.map(Value::from)));
        anthropic::set_opt(
            obj,
            "temperature",
            p.temperature.map(|o| o.map(Value::from)),
        );
        anthropic::set_opt(obj, "top_p", p.top_p.map(|o| o.map(Value::from)));
        anthropic::set_opt(
            obj,
            "stop",
            p.stop.clone().map(|o| {
                o.map(|s| match s.as_slice() {
                    [one] if src.stop_string => json!(one),
                    _ => json!(s),
                })
            }),
        );
    }
    Ok(())
}

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

/// 开头那几条 system 消息按段改。新加的段用最后一条的角色（developer 还是 system）
fn system(lead: &[Value], idx: &[usize], new: &str) -> Vec<Value> {
    let segs: Vec<String> = idx
        .iter()
        .map(|&i| text_of(lead[i].get("content")))
        .collect();
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    let role = lead
        .last()
        .and_then(|m| m.get("role"))
        .and_then(Value::as_str)
        .unwrap_or("system")
        .to_string();
    let out = segment_diff(&segs, "\n\n", new)
        .into_iter()
        .filter_map(|s| match s {
            Seg::Keep(k) => Some(Ok((k, lead[idx[k]].clone()))),
            Seg::Replace(_, t) if t.is_empty() => None,
            Seg::Replace(k, t) => {
                let mut m = lead[idx[k]].clone();
                m["content"] = json!(t);
                Some(Ok((k, m)))
            }
            Seg::Insert(t) if t.is_empty() => None,
            Seg::Insert(t) => Some(Err(json!({ "role": role, "content": t }))),
        })
        .collect();
    merge(lead, idx, out)
}

fn rebuild(m: &Value, src: &Msg, parts: &[PartEdit]) -> Result<Value, EditError> {
    let mut m = m.clone();
    match src.kind {
        Kind::Tool => {
            let [PartEdit::Keep { change, .. }] = parts else {
                return Err(bad(
                    "a Chat tool message is its tool result: edit the result's text, or delete \
                     the whole message",
                ));
            };
            if let Some(Change::Result(t)) = change {
                m["content"] = json!(t);
            }
            return Ok(m);
        }
        Kind::Other => {
            return Err(bad(
                "this message is read-only: keep it as it is, or delete the whole message",
            ));
        }
        Kind::Content => {}
    }
    let assistant = m.get("role").and_then(Value::as_str) == Some("assistant");
    // 正文的几项（原来的、新加的），推理和工具调用各自另算
    let mut content: Vec<Result<(At, Option<String>), String>> = Vec::new();
    let mut calls: Vec<(usize, Option<Value>)> = Vec::new();
    let mut reasoning = false;
    for p in parts {
        match p {
            PartEdit::Insert(t) => content.push(Err(t.clone())),
            PartEdit::Keep { from, change } => match src.parts[*from] {
                At::Reasoning => reasoning = true,
                a @ (At::ContentString | At::Content(_)) => content.push(Ok((
                    a,
                    match change {
                        Some(Change::Text(t)) => Some(t.clone()),
                        _ => None,
                    },
                ))),
                At::ToolCall(c) => calls.push((
                    c,
                    match change {
                        Some(Change::Input(v)) => Some(v.clone()),
                        _ => None,
                    },
                )),
                At::Whole => {}
            },
        }
    }
    let o = m.as_object_mut().expect("a message is an object");
    if !reasoning {
        o.remove("reasoning_content");
        o.remove("reasoning");
    }
    let orig_string = o.get("content").and_then(Value::as_str).map(str::to_string);
    let orig_items = o
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let new_content = match content.as_slice() {
        [] => {
            if assistant && !calls.is_empty() {
                Value::Null
            } else {
                json!("")
            }
        }
        [Ok((At::ContentString, change))] => {
            json!(change.clone().or(orig_string.clone()).unwrap_or_default())
        }
        // 原来没有部分数组（字符串或者 null）：一段新加的字还写成字符串
        [Err(t)] if orig_items.is_empty() => json!(t),
        items => Value::Array(
            items
                .iter()
                .map(|it| match it {
                    Ok((At::ContentString, change)) => json!({
                        "type": "text",
                        "text": change.clone().or(orig_string.clone()).unwrap_or_default(),
                    }),
                    Ok((At::Content(c), change)) => {
                        let mut x = orig_items[*c].clone();
                        if let Some(t) = change {
                            let field = if x.get("type").and_then(Value::as_str) == Some("refusal")
                            {
                                "refusal"
                            } else {
                                "text"
                            };
                            x[field] = json!(t);
                        }
                        x
                    }
                    Ok(_) => Value::Null,
                    Err(t) => json!({ "type": "text", "text": t }),
                })
                .collect(),
        ),
    };
    o.insert("content".into(), new_content);
    if assistant {
        let orig_calls = o
            .get("tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if calls.is_empty() {
            o.remove("tool_calls");
        } else {
            let out: Vec<Value> = calls
                .into_iter()
                .map(|(c, input)| {
                    let mut call = orig_calls[c].clone();
                    if let Some(v) = input {
                        if call.get("type").and_then(Value::as_str) == Some("custom") {
                            call["custom"]["input"] = json!(args_text(&v));
                        } else {
                            call["function"]["arguments"] = json!(args_text(&v));
                        }
                    }
                    call
                })
                .collect();
            o.insert("tool_calls".into(), Value::Array(out));
        }
    }
    Ok(m)
}
