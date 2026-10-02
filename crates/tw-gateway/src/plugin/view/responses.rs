//! OpenAI Responses 的请求视图。
//!
//! - `system` 是 `instructions`。
//! - `input` 是字符串时是一条 user 消息；是数组时**每一项一条消息**：消息项按它的角色
//!   （system / developer 是 `system`），函数调用和推理是 `assistant`，函数结果是
//!   `tool`。认不出的项（`item_reference`、托管工具的调用……）是一个只读的部分。
//! - 工具只列函数工具；自定义、namespace、托管工具看不见、不动。新加的工具写
//!   `"strict": false` —— Responses 的函数工具默认是严格模式，一个随手写的 schema
//!   在严格模式下会被拒。
//! - 参数：`model`、`max_output_tokens`、`temperature`、`top_p`。Responses 没有 `stop`。

use serde_json::{Map, Value, json};

use super::*;

pub struct Src {
    /// `input` 是一个字符串
    input_string: bool,
    messages: Vec<Msg>,
    tools: Vec<usize>,
    pub hidden_tools: Vec<String>,
}

struct Msg {
    kind: Kind,
    /// 消息项：内容是字符串
    string: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `input` 本身是字符串的那一条
    InputString,
    Message,
    /// `function_call`
    Call,
    /// `custom_tool_call`
    Custom,
    /// `function_call_output` / `custom_tool_call_output`
    Output,
    /// 推理和认不出的项：只读
    Fixed,
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

pub fn build(raw: &Value) -> Built {
    let mut view = Map::new();
    view.insert("format".into(), json!("openai_responses"));
    let model = raw.get("model").and_then(Value::as_str).unwrap_or_default();
    view.insert("model".into(), json!(model));
    view.insert(
        "system".into(),
        json!(
            raw.get("instructions")
                .and_then(Value::as_str)
                .unwrap_or_default()
        ),
    );

    let mut messages = Vec::new();
    let mut src_messages = Vec::new();
    let input_string = matches!(raw.get("input"), Some(Value::String(_)));
    match raw.get("input") {
        Some(Value::String(s)) => {
            messages.push(json!({
                "key": msg_key(0),
                "role": "user",
                "parts": [part_text(&part_key(0, 0), s)],
            }));
            src_messages.push(Msg {
                kind: Kind::InputString,
                string: true,
            });
        }
        Some(Value::Array(items)) => {
            for (k, item) in items.iter().enumerate() {
                let (role, kind, string, parts) = item_view(k, item);
                messages.push(json!({ "key": msg_key(k), "role": role.slug(), "parts": parts }));
                src_messages.push(Msg { kind, string });
            }
        }
        _ => {}
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
        let name = t.get("name").and_then(Value::as_str).unwrap_or_default();
        if kind == "function" {
            tools.push(json!({
                "key": tool_key(src_tools.len()),
                "name": name,
                "description": t.get("description").and_then(Value::as_str).unwrap_or_default(),
                "input_schema": t
                    .get("parameters")
                    .filter(|p| p.is_object())
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
            }));
            src_tools.push(i);
        } else {
            hidden_tools.push(if name.is_empty() { kind } else { name }.to_string());
            // namespace 里的工具展开之后也占着名字
            for inner in t
                .get("tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(n) = inner.get("name").and_then(Value::as_str) {
                    hidden_tools.push(n.to_string());
                }
            }
        }
    }
    view.insert("tools".into(), Value::Array(tools));

    let mut params = Map::new();
    params.insert("model".into(), json!(model));
    if let Some(n) = raw.get("max_output_tokens").and_then(Value::as_u64) {
        params.insert("max_tokens".into(), json!(n));
    }
    for k in ["temperature", "top_p"] {
        if let Some(x) = raw.get(k).filter(|x| x.is_number()) {
            params.insert(k.into(), x.clone());
        }
    }
    view.insert("params".into(), Value::Object(params));

    Built {
        view: Value::Object(view),
        src: super::Src::Responses(Src {
            input_string,
            messages: src_messages,
            tools: src_tools,
            hidden_tools,
        }),
    }
}

/// 一个输入项 → (角色, 种类, 内容是不是字符串, 部分)
fn item_view(k: usize, item: &Value) -> (Role, Kind, bool, Vec<Value>) {
    let s = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or_default();
    let key = |j: usize| part_key(k, j);
    match item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message")
    {
        "message" => {
            let role = match s("role") {
                "assistant" => Role::Assistant,
                "system" | "developer" => Role::System,
                _ => Role::User,
            };
            let mut parts = Vec::new();
            let string = matches!(item.get("content"), Some(Value::String(_)));
            match item.get("content") {
                Some(Value::String(t)) => parts.push(part_text(&key(0), t)),
                Some(Value::Array(items)) => {
                    for (j, p) in items.iter().enumerate() {
                        parts.push(content_part(&key(j), p));
                    }
                }
                _ => {}
            }
            (role, Kind::Message, string, parts)
        }
        "function_call" => (
            Role::Assistant,
            Kind::Call,
            false,
            vec![part_call(
                &key(0),
                s("call_id"),
                s("name"),
                args_value(s("arguments")),
            )],
        ),
        "custom_tool_call" => (
            Role::Assistant,
            Kind::Custom,
            false,
            vec![part_call(
                &key(0),
                s("call_id"),
                s("name"),
                json!(s("input")),
            )],
        ),
        "function_call_output" | "custom_tool_call_output" => (
            Role::Tool,
            Kind::Output,
            false,
            vec![part_result(
                &key(0),
                s("call_id"),
                &text_of(item.get("output")),
                false,
            )],
        ),
        "reasoning" => {
            let texts = |field: &str| {
                item.get(field)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|x| x.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n\n")
            };
            let text = match texts("content") {
                t if t.is_empty() => texts("summary"),
                t => t,
            };
            (
                Role::Assistant,
                Kind::Fixed,
                false,
                vec![part_thinking(&key(0), &text)],
            )
        }
        other => {
            let role = if other.ends_with("_output") {
                Role::Tool
            } else {
                Role::Assistant
            };
            (role, Kind::Fixed, false, vec![part_other(&key(0), other)])
        }
    }
}

fn content_part(key: &str, p: &Value) -> Value {
    match p.get("type").and_then(Value::as_str).unwrap_or_default() {
        "input_text" | "output_text" | "text" => part_text(
            key,
            p.get("text").and_then(Value::as_str).unwrap_or_default(),
        ),
        "refusal" => part_text(
            key,
            p.get("refusal").and_then(Value::as_str).unwrap_or_default(),
        ),
        "input_image" => part_image(
            key,
            p.get("image_url")
                .and_then(Value::as_str)
                .and_then(data_uri_mime),
        ),
        other => part_other(key, if other.is_empty() { "unknown" } else { other }),
    }
}

/// 一段文字写成某个角色的内容部分：助手说的是 `output_text`，别的是 `input_text`
fn text_part(role: Role, t: &str) -> Value {
    let kind = if role == Role::Assistant {
        "output_text"
    } else {
        "input_text"
    };
    json!({ "type": kind, "text": t })
}

fn new_message(role: Role, texts: &[String]) -> Value {
    let wire = match role {
        Role::Assistant => "assistant",
        // 中途的系统消息写成 developer：两者同义，新模型和 Codex 后端认的是它
        Role::System => "developer",
        _ => "user",
    };
    json!({
        "type": "message",
        "role": wire,
        "content": texts.iter().map(|t| text_part(role, t)).collect::<Vec<_>>(),
    })
}

pub fn apply(raw: &mut Value, src: &Src, edits: &Edits) -> Result<(), EditError> {
    let Some(obj) = raw.as_object_mut() else {
        return Err(bad("the request body is not a JSON object"));
    };
    if let Some(new) = &edits.system {
        if new.is_empty() {
            obj.remove("instructions");
        } else {
            obj.insert("instructions".into(), json!(new));
        }
    }
    if let Some(medits) = &edits.messages {
        let items: Vec<Value> = if src.input_string {
            let s = obj.get("input").and_then(Value::as_str).unwrap_or_default();
            vec![new_message(Role::User, &[s.to_string()])]
        } else {
            obj.get("input")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let mut out = Vec::with_capacity(medits.len());
        for e in medits {
            match e {
                MsgEdit::Keep { from, parts: None } => out.push(items[*from].clone()),
                MsgEdit::Keep {
                    from,
                    parts: Some(parts),
                } => out.push(rebuild(&items[*from], &src.messages[*from], parts)?),
                MsgEdit::Insert { role, texts } => out.push(new_message(*role, texts)),
            }
        }
        obj.insert("input".into(), Value::Array(out));
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
                            o.insert("parameters".into(), s.clone());
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
                    t.insert("type".into(), json!("function"));
                    t.insert("name".into(), json!(name));
                    if !description.is_empty() {
                        t.insert("description".into(), json!(description));
                    }
                    t.insert("parameters".into(), schema.clone());
                    t.insert("strict".into(), json!(false));
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
        if matches!(p.stop, Some(Some(_))) {
            return Err(bad("OpenAI Responses requests have no stop sequences"));
        }
        if let Some(m) = &p.model {
            obj.insert("model".into(), json!(m));
        }
        anthropic::set_opt(
            obj,
            "max_output_tokens",
            p.max_tokens.map(|o| o.map(Value::from)),
        );
        anthropic::set_opt(
            obj,
            "temperature",
            p.temperature.map(|o| o.map(Value::from)),
        );
        anthropic::set_opt(obj, "top_p", p.top_p.map(|o| o.map(Value::from)));
    }
    Ok(())
}

fn rebuild(item: &Value, src: &Msg, parts: &[PartEdit]) -> Result<Value, EditError> {
    let mut item = item.clone();
    let whole = |what: &str| {
        bad(format!(
            "{what} is a single item: edit it in place or delete the whole message; parts \
             cannot be added to it or removed from it"
        ))
    };
    match src.kind {
        Kind::Call | Kind::Custom => {
            let [PartEdit::Keep { change, .. }] = parts else {
                return Err(whole("a function call"));
            };
            if let Some(Change::Input(v)) = change {
                let field = if src.kind == Kind::Call {
                    "arguments"
                } else {
                    "input"
                };
                item[field] = json!(args_text(v));
            }
            Ok(item)
        }
        Kind::Output => {
            let [PartEdit::Keep { change, .. }] = parts else {
                return Err(whole("a function call output"));
            };
            if let Some(Change::Result(t)) = change {
                item["output"] = json!(t);
            }
            Ok(item)
        }
        Kind::Fixed => {
            let [PartEdit::Keep { .. }] = parts else {
                return Err(whole("this item"));
            };
            Ok(item)
        }
        Kind::InputString | Kind::Message => {
            let role = match item.get("role").and_then(Value::as_str) {
                Some("assistant") => Role::Assistant,
                _ => Role::User,
            };
            if src.string {
                let orig = match src.kind {
                    // 字符串的 `input` 已经被写成了一条带一个文字部分的消息
                    Kind::InputString => item["content"][0]["text"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    _ => item
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                };
                if let ([PartEdit::Keep { change, .. }], Kind::Message) = (parts, src.kind) {
                    if let Some(Change::Text(t)) = change {
                        item["content"] = json!(t);
                    }
                    return Ok(item);
                }
                let content: Vec<Value> = parts
                    .iter()
                    .map(|p| match p {
                        PartEdit::Keep { change, .. } => match change {
                            Some(Change::Text(t)) => text_part(role, t),
                            _ => text_part(role, &orig),
                        },
                        PartEdit::Insert(t) => text_part(role, t),
                    })
                    .collect();
                item["content"] = Value::Array(content);
                return Ok(item);
            }
            let orig = item
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let content: Vec<Value> = parts
                .iter()
                .map(|p| match p {
                    PartEdit::Keep { from, change } => {
                        let mut x = orig[*from].clone();
                        if let Some(Change::Text(t)) = change {
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
                    PartEdit::Insert(t) => text_part(role, t),
                })
                .collect();
            item["content"] = Value::Array(content);
            Ok(item)
        }
    }
}
