//! Gemini generateContent 的请求视图。
//!
//! Gemini 的 REST 接口是 proto3 JSON：驼峰和下划线两种写法都收。读的时候两种都认，
//! **写回原来那个字段**（原来没有的写驼峰）。
//!
//! - 模型写在路径里（`/v1beta/models/{model}:generateContent`）：换模型就是换路径。
//! - `system` 是 `systemInstruction` 的文字部分，部分之间空一行。
//! - 每个 `contents` 一条消息：`model` 是 `assistant`；只装函数结果的 user 是 `tool`。
//!   `contents` 里没有 system 角色，新加 system 消息要改 `system`。
//! - 工具是 `functionDeclarations` 里的每一个；`googleSearch` 这些看不见、不动。
//! - 参数：`model`、`generationConfig` 里的 `maxOutputTokens`、`temperature`、
//!   `topP`、`stopSequences`。

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::*;

pub struct Src {
    system: System,
    /// 每条消息每个部分在 `parts` 里的下标就是它在视图里的序号，不另记
    messages: usize,
    /// 视图里第 k 个工具：(在 `tools` 里的下标, 声明数组的字段名, 在声明数组里的下标)
    tools: Vec<(usize, String, usize)>,
    pub hidden_tools: Vec<String>,
}

enum System {
    None,
    /// 一个字符串，字段名
    String(String),
    /// 一个 Content：字段名，有字的文字部分的下标
    Parts(String, Vec<usize>),
}

/// 驼峰的那个字段，取不到再试下划线写法。返回值和实际的字段名
fn field<'a>(v: &'a Value, camel: &str) -> Option<(&'a Value, String)> {
    if let Some(x) = v.get(camel) {
        return Some((x, camel.to_string()));
    }
    let snake = snake(camel);
    v.get(&snake).map(|x| (x, snake))
}

fn snake(camel: &str) -> String {
    let mut out = String::with_capacity(camel.len() + 4);
    for c in camel.chars() {
        if c.is_ascii_uppercase() {
            out.push('_');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// 原来有这个字段就用原来的写法，没有就写驼峰
fn name_in(v: &Value, camel: &str) -> String {
    field(v, camel).map_or_else(|| camel.to_string(), |(_, k)| k)
}

pub(super) fn fstr<'a>(v: &'a Value, camel: &str) -> Option<&'a str> {
    field(v, camel).and_then(|(x, _)| x.as_str())
}

/// `/v1beta/models/gemini-2.5-pro:generateContent` 里的模型
pub(super) fn path_model(path: &str) -> Option<&str> {
    let (_, rest) = path.split_once("/models/")?;
    let (model, _) = rest.rsplit_once(':')?;
    Some(model)
}

/// 函数结果写成文字：只有一个 output / result / content / error 字符串时取它本身
fn response_text(v: &Value) -> String {
    if let Some(s) = single_text(v) {
        return s.1.to_string();
    }
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn single_text(v: &Value) -> Option<(&str, &str)> {
    let o = v.as_object()?;
    if o.len() != 1 {
        return None;
    }
    ["output", "result", "content", "error"]
        .iter()
        .find_map(|k| o.get(*k).and_then(Value::as_str).map(|s| (*k, s)))
}

pub fn build(raw: &Value, path: &str) -> Result<Built, String> {
    let model = path_model(path)
        .ok_or_else(|| format!("the path {path} does not say which Gemini model to call"))?
        .to_string();
    let mut view = Map::new();
    view.insert("format".into(), json!("gemini"));
    view.insert("model".into(), json!(model));

    let (system_text, system) = match field(raw, "systemInstruction") {
        Some((Value::String(s), k)) => (s.clone(), System::String(k)),
        Some((sys @ Value::Object(_), k)) => {
            let parts = sys
                .get("parts")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let idx: Vec<usize> = parts
                .iter()
                .enumerate()
                .filter(|(_, p)| fstr(p, "text").is_some_and(|t| !t.is_empty()))
                .map(|(i, _)| i)
                .collect();
            let text = idx
                .iter()
                .filter_map(|&i| fstr(&parts[i], "text"))
                .collect::<Vec<_>>()
                .join("\n\n");
            (text, System::Parts(k, idx))
        }
        _ => (String::new(), System::None),
    };
    view.insert("system".into(), json!(system_text));

    let mut messages = Vec::new();
    let contents = raw
        .get("contents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // 没有 id 的调用按名字排队，结果按名字依次认领（和转换时一样）
    let mut pending: HashMap<String, Vec<String>> = HashMap::new();
    for (i, c) in contents.iter().enumerate() {
        let parts = c
            .get("parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::with_capacity(parts.len());
        for (j, p) in parts.iter().enumerate() {
            out.push(part(&part_key(i, j), p, i, j, &mut pending));
        }
        let only_results =
            !parts.is_empty() && parts.iter().all(|p| field(p, "functionResponse").is_some());
        let role = match c.get("role").and_then(Value::as_str) {
            Some("model") => Role::Assistant,
            _ if only_results => Role::Tool,
            _ => Role::User,
        };
        messages.push(json!({ "key": msg_key(i), "role": role.slug(), "parts": out }));
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
        let Some(o) = t.as_object() else { continue };
        for (k, v) in o {
            if k == "functionDeclarations" || k == "function_declarations" {
                for (d, decl) in v.as_array().into_iter().flatten().enumerate() {
                    let schema = field(decl, "parametersJsonSchema")
                        .or_else(|| decl.get("parameters").map(|p| (p, "parameters".into())))
                        .map(|(s, _)| s.clone())
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
                    tools.push(json!({
                        "key": tool_key(src_tools.len()),
                        "name": fstr(decl, "name").unwrap_or_default(),
                        "description": fstr(decl, "description").unwrap_or_default(),
                        "input_schema": schema,
                    }));
                    src_tools.push((i, k.clone(), d));
                }
            } else {
                hidden_tools.push(k.clone());
            }
        }
    }
    view.insert("tools".into(), Value::Array(tools));

    let mut params = Map::new();
    params.insert("model".into(), json!(model));
    if let Some((g, _)) = field(raw, "generationConfig") {
        if let Some(n) = field(g, "maxOutputTokens").and_then(|(x, _)| x.as_u64()) {
            params.insert("max_tokens".into(), json!(n));
        }
        if let Some((x, _)) = field(g, "temperature").filter(|(x, _)| x.is_number()) {
            params.insert("temperature".into(), x.clone());
        }
        if let Some((x, _)) = field(g, "topP").filter(|(x, _)| x.is_number()) {
            params.insert("top_p".into(), x.clone());
        }
        if let Some(a) = field(g, "stopSequences").and_then(|(x, _)| x.as_array()) {
            params.insert(
                "stop".into(),
                Value::Array(a.iter().filter(|s| s.is_string()).cloned().collect()),
            );
        }
    }
    view.insert("params".into(), Value::Object(params));

    Ok(Built {
        view: Value::Object(view),
        src: super::Src::Gemini(Src {
            system,
            messages: contents.len(),
            tools: src_tools,
            hidden_tools,
        }),
    })
}

fn part(
    key: &str,
    p: &Value,
    i: usize,
    j: usize,
    pending: &mut HashMap<String, Vec<String>>,
) -> Value {
    if let Some(t) = fstr(p, "text") {
        return if p.get("thought").and_then(Value::as_bool) == Some(true) {
            part_thinking(key, t)
        } else {
            part_text(key, t)
        };
    }
    if let Some((blob, _)) = field(p, "inlineData") {
        let mime = fstr(blob, "mimeType").unwrap_or_default();
        return if mime.starts_with("image/") {
            part_image(key, Some(mime))
        } else {
            part_other(key, "inlineData")
        };
    }
    if let Some((call, _)) = field(p, "functionCall") {
        let name = fstr(call, "name").unwrap_or_default().to_string();
        let id = fstr(call, "id")
            .map(str::to_string)
            .unwrap_or_else(|| format!("call_{i}_{j}"));
        pending.entry(name.clone()).or_default().push(id.clone());
        return part_call(
            key,
            &id,
            &name,
            field(call, "args").map_or_else(|| json!({}), |(a, _)| a.clone()),
        );
    }
    if let Some((resp, _)) = field(p, "functionResponse") {
        let name = fstr(resp, "name").unwrap_or_default();
        let id = match fstr(resp, "id") {
            Some(id) => id.to_string(),
            None => pending
                .get_mut(name)
                .filter(|q| !q.is_empty())
                .map(|q| q.remove(0))
                .unwrap_or_else(|| format!("call_{i}_{j}")),
        };
        let body = resp.get("response").unwrap_or(&Value::Null);
        return part_result(
            key,
            &id,
            &response_text(body),
            body.get("error").is_some_and(|e| !e.is_null()) && body.get("output").is_none(),
        );
    }
    let label = p
        .as_object()
        .and_then(|o| {
            o.keys().find(|k| {
                !matches!(
                    k.as_str(),
                    "thought" | "thoughtSignature" | "thought_signature"
                )
            })
        })
        .map_or("unknown", String::as_str);
    part_other(key, label)
}

pub fn apply(
    raw: &mut Value,
    src: &Src,
    edits: &Edits,
    path: &str,
) -> Result<Option<String>, EditError> {
    let Some(obj) = raw.as_object_mut() else {
        return Err(bad("the request body is not a JSON object"));
    };
    if let Some(new) = &edits.system {
        apply_system(obj, &src.system, new);
    }
    if let Some(medits) = &edits.messages {
        let contents = obj
            .get("contents")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        debug_assert_eq!(contents.len(), src.messages);
        let mut out = Vec::with_capacity(medits.len());
        for e in medits {
            match e {
                MsgEdit::Keep { from, parts: None } => out.push(contents[*from].clone()),
                MsgEdit::Keep {
                    from,
                    parts: Some(parts),
                } => out.push(rebuild(&contents[*from], parts)?),
                MsgEdit::Insert { role, texts } => {
                    let role = match role {
                        Role::User => "user",
                        Role::Assistant => "model",
                        _ => {
                            return Err(bad(
                                "Gemini has no system role inside `contents`; change `system` instead",
                            ));
                        }
                    };
                    out.push(json!({
                        "role": role,
                        "parts": texts.iter().map(|t| json!({ "text": t })).collect::<Vec<_>>(),
                    }));
                }
            }
        }
        obj.insert("contents".into(), Value::Array(out));
    }
    if let Some(tedits) = &edits.tools {
        apply_tools(obj, src, tedits);
    }
    let mut new_path = None;
    if let Some(p) = &edits.params {
        if let Some(m) = &p.model {
            new_path = Some(crate::forward::gemini_path_with_model(path, m));
        }
        let raw_obj = Value::Object(obj.clone());
        let gkey = name_in(&raw_obj, "generationConfig");
        let any = p.max_tokens.is_some()
            || p.temperature.is_some()
            || p.top_p.is_some()
            || p.stop.is_some();
        if any {
            let g = obj.entry(gkey).or_insert_with(|| json!({}));
            if !g.is_object() {
                *g = json!({});
            }
            let gv = g.clone();
            let go = g.as_object_mut().expect("just made it an object");
            anthropic::set_opt(
                go,
                &name_in(&gv, "maxOutputTokens"),
                p.max_tokens.map(|o| o.map(Value::from)),
            );
            anthropic::set_opt(
                go,
                &name_in(&gv, "temperature"),
                p.temperature.map(|o| o.map(Value::from)),
            );
            anthropic::set_opt(
                go,
                &name_in(&gv, "topP"),
                p.top_p.map(|o| o.map(Value::from)),
            );
            anthropic::set_opt(
                go,
                &name_in(&gv, "stopSequences"),
                p.stop.clone().map(|o| o.map(|s| json!(s))),
            );
        }
    }
    Ok(new_path)
}

fn apply_system(obj: &mut Map<String, Value>, src: &System, new: &str) {
    match src {
        System::None => {
            if !new.is_empty() {
                obj.insert(
                    "systemInstruction".into(),
                    json!({ "parts": [{ "text": new }] }),
                );
            }
        }
        System::String(k) => {
            if new.is_empty() {
                obj.remove(k);
            } else {
                obj.insert(k.clone(), json!(new));
            }
        }
        System::Parts(k, idx) => {
            let mut sys = obj.get(k).cloned().unwrap_or_else(|| json!({}));
            let parts = sys
                .get("parts")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let segs: Vec<&str> = idx
                .iter()
                .filter_map(|&i| fstr(&parts[i], "text"))
                .collect();
            let out = segment_diff(&segs, "\n\n", new)
                .into_iter()
                .filter_map(|s| match s {
                    Seg::Keep(n) => Some(Ok((n, parts[idx[n]].clone()))),
                    Seg::Replace(_, t) if t.is_empty() => None,
                    Seg::Replace(n, t) => {
                        let mut p = parts[idx[n]].clone();
                        let key = name_in(&p, "text");
                        p[key] = json!(t);
                        Some(Ok((n, p)))
                    }
                    Seg::Insert(t) if t.is_empty() => None,
                    Seg::Insert(t) => Some(Err(json!({ "text": t }))),
                })
                .collect();
            let merged = merge(&parts, idx, out);
            if merged.is_empty() {
                obj.remove(k);
            } else {
                sys["parts"] = Value::Array(merged);
                obj.insert(k.clone(), sys);
            }
        }
    }
}

fn rebuild(content: &Value, parts: &[PartEdit]) -> Result<Value, EditError> {
    let mut content = content.clone();
    let orig = content
        .get("parts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::with_capacity(parts.len());
    for p in parts {
        match p {
            PartEdit::Insert(t) => out.push(json!({ "text": t })),
            PartEdit::Keep { from, change } => {
                let mut x = orig[*from].clone();
                match change {
                    Some(Change::Text(t)) => {
                        let key = name_in(&x, "text");
                        x[key] = json!(t);
                    }
                    Some(Change::Input(v)) => {
                        if !v.is_object() {
                            return Err(bad(
                                "the arguments of a Gemini function call must be an object",
                            ));
                        }
                        let key = name_in(&x, "functionCall");
                        x[&key]["args"] = v.clone();
                    }
                    Some(Change::Result(t)) => {
                        let key = name_in(&x, "functionResponse");
                        let body = x[&key].get("response").cloned().unwrap_or(Value::Null);
                        let next = match single_text(&body) {
                            Some((field, _)) => json!({ field: t }),
                            None => match serde_json::from_str::<Value>(t) {
                                Ok(v @ Value::Object(_)) => v,
                                _ => json!({ "output": t }),
                            },
                        };
                        x[&key]["response"] = next;
                    }
                    None => {}
                }
                out.push(x);
            }
        }
    }
    content["parts"] = Value::Array(out);
    Ok(content)
}

fn apply_tools(obj: &mut Map<String, Value>, src: &Src, edits: &[ToolEdit]) {
    let mut tools = obj
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // 每个声明数组：(工具下标, 字段名) → 新的声明
    let mut arrays: Vec<(usize, String)> = Vec::new();
    for (i, k, _) in &src.tools {
        if !arrays.iter().any(|(a, b)| a == i && b == k) {
            arrays.push((*i, k.clone()));
        }
    }
    let mut rebuilt: HashMap<(usize, String), Vec<Value>> =
        arrays.iter().map(|a| (a.clone(), Vec::new())).collect();
    let mut inserted = Vec::new();
    for e in edits {
        match e {
            ToolEdit::Keep {
                from,
                description,
                schema,
            } => {
                let (i, k, d) = &src.tools[*from];
                let mut decl = tools[*i][k][*d].clone();
                match description.as_deref() {
                    Some("") => {
                        if let Some(o) = decl.as_object_mut() {
                            o.remove("description");
                        }
                    }
                    Some(text) => decl["description"] = json!(text),
                    None => {}
                }
                if let Some(s) = schema {
                    let key = if field(&decl, "parametersJsonSchema").is_some() {
                        name_in(&decl, "parametersJsonSchema")
                    } else if decl.get("parameters").is_some() {
                        "parameters".to_string()
                    } else {
                        "parametersJsonSchema".to_string()
                    };
                    decl[key] = s.clone();
                }
                if let Some(v) = rebuilt.get_mut(&(*i, k.clone())) {
                    v.push(decl);
                }
            }
            ToolEdit::Insert {
                name,
                description,
                schema,
            } => {
                let mut d = Map::new();
                d.insert("name".into(), json!(name));
                if !description.is_empty() {
                    d.insert("description".into(), json!(description));
                }
                d.insert("parametersJsonSchema".into(), schema.clone());
                inserted.push(Value::Object(d));
            }
        }
    }
    // 新加的放进最后一个声明数组；一个都没有就新起一个工具
    if !inserted.is_empty() {
        match arrays.last() {
            Some(last) => {
                if let Some(v) = rebuilt.get_mut(last) {
                    v.extend(inserted);
                }
            }
            None => tools.push(json!({ "functionDeclarations": inserted })),
        }
    }
    for ((i, k), decls) in rebuilt {
        tools[i][&k] = Value::Array(decls);
    }
    // 声明删空了、又没有别的东西的工具整个去掉
    tools.retain(|t| {
        t.as_object().is_none_or(|o| {
            !(o.len() == 1
                && o.values()
                    .next()
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
                && o.keys()
                    .next()
                    .is_some_and(|k| k == "functionDeclarations" || k == "function_declarations"))
        })
    });
    if tools.is_empty() {
        obj.remove("tools");
    } else {
        obj.insert("tools".into(), Value::Array(tools));
    }
}
