//! OpenAI Responses 请求 ⇄ 中间表示。

use serde_json::{Map, Value, json};

use crate::ir::*;
use crate::think;

// ───────────────────────────────────────────────────────── 解码

/// 客户端发来的 Responses 请求 → 中间表示。
///
/// **依赖 OpenAI 服务端状态的请求直接拒绝**（`previous_response_id`、`conversation`、
/// 引用服务端内容的输入项）：对话内容不在请求里，转给别家只会得到一个忘了前文的
/// 回答，而且看起来一切正常。
pub fn decode_request(
    v: &Value,
    dropped: &mut Dropped,
    shape: &mut ClientShape,
) -> Result<Request, Rejection> {
    if !v.is_object() {
        return Err(Rejection("请求体不是 JSON 对象。".into()));
    }
    for (key, what) in [
        ("previous_response_id", "对话内容保存在 OpenAI 服务端"),
        ("conversation", "对话内容保存在 OpenAI 服务端"),
        ("prompt", "提示模板保存在 OpenAI 服务端"),
    ] {
        if has(v, key) {
            return Err(Rejection(format!(
                "请求使用了 {key}，{what}，无法转换到其他格式的上游。"
            )));
        }
    }
    if v.get("background").and_then(Value::as_bool) == Some(true) {
        return Err(Rejection(
            "请求使用了 background，后台运行只有 OpenAI 服务端支持，无法转换到其他格式的上游。"
                .into(),
        ));
    }

    let mut r = Request {
        model: str_of(v, "model").unwrap_or_default().to_string(),
        max_tokens: u64_of(v, "max_output_tokens"),
        temperature: f64_of(v, "temperature"),
        top_p: f64_of(v, "top_p"),
        parallel_tool_calls: v.get("parallel_tool_calls").and_then(Value::as_bool),
        stream: v.get("stream").and_then(Value::as_bool).unwrap_or(false),
        ..Default::default()
    };
    if let Some(i) = str_of(v, "instructions").filter(|i| !i.is_empty()) {
        r.system.push(i.to_string());
    }

    for t in arr_of(v, "tools") {
        decode_tool(t, None, dropped, shape, &mut r.tools);
    }

    match v.get("input") {
        Some(Value::String(s)) if !s.is_empty() => r.messages.push(Message {
            role: Role::User,
            parts: vec![Part::Text(s.clone())],
        }),
        Some(Value::Array(items)) => {
            for item in items {
                decode_item(item, dropped, &mut r)?;
            }
        }
        _ => {}
    }

    r.tool_choice = match v.get("tool_choice") {
        Some(Value::String(s)) => match s.as_str() {
            "auto" => Some(ToolChoice::Auto),
            "none" => Some(ToolChoice::None),
            "required" => Some(ToolChoice::Required),
            _ => None,
        },
        Some(o @ Value::Object(_)) => match str_of(o, "type") {
            Some("function" | "custom") => {
                str_of(o, "name").map(|n| ToolChoice::Named(n.to_string()))
            }
            Some("allowed_tools") => {
                dropped.path("tool_choice.allowed_tools");
                match str_of(o, "mode") {
                    Some("required") => Some(ToolChoice::Required),
                    _ => Some(ToolChoice::Auto),
                }
            }
            other => {
                dropped.path(format!("tool_choice.{}", other.unwrap_or("unknown")));
                None
            }
        },
        _ => None,
    };

    if let Some(re) = v.get("reasoning").filter(|x| x.is_object()) {
        let effort = str_of(re, "effort").and_then(think::parse_openai);
        r.reasoning = Some(Reasoning {
            enabled: effort != Some(None),
            effort: effort.flatten(),
            budget: None,
            summary: has(re, "summary") || has(re, "generate_summary"),
        });
    }

    if let Some(text) = v.get("text") {
        r.format = match text.get("format").and_then(|f| str_of(f, "type")) {
            Some("json_object") => Some(Format::JsonObject),
            Some("json_schema") => {
                let f = &text["format"];
                Some(Format::JsonSchema {
                    name: str_of(f, "name").map(str::to_string),
                    schema: f.get("schema").cloned().unwrap_or(Value::Null),
                    strict: f.get("strict").and_then(Value::as_bool),
                })
            }
            _ => None,
        };
        if has(text, "verbosity") {
            dropped.path("text.verbosity");
        }
    }

    for k in [
        "top_logprobs",
        "max_tool_calls",
        "moderation",
        "context_management",
    ] {
        if has(v, k) {
            dropped.path(k);
        }
    }
    Ok(r)
}

/// namespace 里的工具展开成 `namespace__名字`，写响应时再拆回来
fn flat_name(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(ns) if !ns.is_empty() => format!("{ns}__{name}"),
        _ => name.to_string(),
    }
}

fn decode_tool(
    t: &Value,
    namespace: Option<&str>,
    dropped: &mut Dropped,
    shape: &mut ClientShape,
    tools: &mut Vec<Tool>,
) {
    let name = str_of(t, "name").unwrap_or_default();
    let flat = flat_name(namespace, name);
    let kind = match str_of(t, "type") {
        Some("function") => ToolKind::Function {
            schema: t
                .get("parameters")
                .filter(|p| !p.is_null())
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
            strict: t.get("strict").and_then(Value::as_bool),
        },
        Some("custom") => ToolKind::Freeform {
            format: t.get("format").cloned(),
        },
        Some("namespace") if namespace.is_none() => {
            for inner in arr_of(t, "tools") {
                decode_tool(inner, Some(name), dropped, shape, tools);
            }
            return;
        }
        // 托管工具（web_search、file_search、shell、apply_patch、mcp……）只有 OpenAI 能执行
        other => {
            dropped.path(format!("tools.{}", other.unwrap_or("unknown")));
            return;
        }
    };
    if let Some(ns) = namespace {
        shape
            .namespaced
            .insert(flat.clone(), (ns.to_string(), name.to_string()));
    }
    tools.push(Tool {
        name: flat,
        description: str_of(t, "description").map(str::to_string),
        kind,
    });
}

fn decode_item(item: &Value, dropped: &mut Dropped, r: &mut Request) -> Result<(), Rejection> {
    let kind = str_of(item, "type").unwrap_or("message");
    let push = |r: &mut Request, role, part| {
        r.messages.push(Message {
            role,
            parts: vec![part],
        })
    };
    match kind {
        "message" => {
            let content = item.get("content").unwrap_or(&Value::Null);
            match str_of(item, "role").unwrap_or("user") {
                "system" | "developer" => {
                    let t = text_of(content);
                    if !t.is_empty() {
                        r.system.push(t);
                    }
                }
                role => {
                    let role = if role == "assistant" {
                        Role::Assistant
                    } else {
                        Role::User
                    };
                    let parts = content_parts(content, "input.content", dropped);
                    r.messages.push(Message { role, parts });
                }
            }
        }
        "function_call" | "custom_tool_call" => {
            let name = flat_name(
                str_of(item, "namespace"),
                str_of(item, "name").unwrap_or_default(),
            );
            let input = if kind == "custom_tool_call" {
                ToolInput::Text(str_of(item, "input").unwrap_or_default().to_string())
            } else {
                ToolInput::from_json_text(str_of(item, "arguments").unwrap_or_default())
            };
            push(
                r,
                Role::Assistant,
                Part::ToolCall(ToolCall {
                    id: str_of(item, "call_id").unwrap_or_default().to_string(),
                    name,
                    input,
                }),
            );
        }
        "function_call_output" | "custom_tool_call_output" => {
            let content = match item.get("output") {
                Some(Value::String(s)) if !s.is_empty() => vec![Part::Text(s.clone())],
                Some(o @ Value::Array(_)) => {
                    content_parts(o, &format!("input.{kind}.output"), dropped)
                        .into_iter()
                        .filter(|p| matches!(p, Part::Text(_) | Part::Image(_)))
                        .collect()
                }
                _ => Vec::new(),
            };
            push(
                r,
                Role::User,
                Part::ToolResult(ToolResult {
                    id: str_of(item, "call_id").unwrap_or_default().to_string(),
                    content,
                    is_error: false,
                }),
            );
        }
        "reasoning" => {
            let texts = |key: &str| {
                arr_of(item, key)
                    .iter()
                    .filter_map(|x| str_of(x, "text"))
                    .collect::<Vec<_>>()
                    .join("\n\n")
            };
            let text = match texts("content") {
                t if t.is_empty() => texts("summary"),
                t => t,
            };
            let signature = str_of(item, "encrypted_content")
                .filter(|e| !e.is_empty())
                .and_then(|enc| {
                    if enc.starts_with(CARRIED) {
                        Signature::read(enc, Vendor::OpenAi)
                    } else {
                        let id = str_of(item, "id").unwrap_or_default();
                        Some(Signature::new(Vendor::OpenAi, format!("{id}:{enc}")))
                    }
                });
            push(
                r,
                Role::Assistant,
                Part::Thinking(Thinking { text, signature }),
            );
        }
        "item_reference" => {
            return Err(Rejection(
                "input 中的 item_reference 引用了 OpenAI 服务端保存的内容，无法转换到其他格式的上游。"
                    .into(),
            ));
        }
        "compaction" => {
            return Err(Rejection(
                "input 中的 compaction 是 OpenAI 压缩后的加密对话，只有 OpenAI 能读取，无法转换到其他格式的上游。"
                    .into(),
            ));
        }
        other => dropped.path(format!("input.{other}")),
    }
    Ok(())
}

fn content_parts(content: &Value, prefix: &str, dropped: &mut Dropped) -> Vec<Part> {
    match content {
        Value::String(s) if !s.is_empty() => vec![Part::Text(s.clone())],
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match str_of(p, "type").unwrap_or("") {
                "input_text" | "output_text" => {
                    str_of(p, "text").map(|t| Part::Text(t.to_string()))
                }
                "refusal" => str_of(p, "refusal").map(|t| Part::Text(t.to_string())),
                "input_image" => match str_of(p, "image_url") {
                    Some(u) => Some(Part::Image(Media::from_uri(u))),
                    None => {
                        dropped.path(format!("{prefix}.input_image.file_id"));
                        None
                    }
                },
                "input_file" => {
                    let name = str_of(p, "filename").map(str::to_string);
                    if let Some(data) = str_of(p, "file_data") {
                        Some(Part::File {
                            media: match Media::from_uri(data) {
                                m @ Media::Base64 { .. } => m,
                                Media::Url(_) => Media::Base64 {
                                    mime: "application/pdf".into(),
                                    data: data.to_string(),
                                },
                            },
                            name,
                        })
                    } else if let Some(u) = str_of(p, "file_url") {
                        Some(Part::File {
                            media: Media::Url(u.to_string()),
                            name,
                        })
                    } else {
                        dropped.path(format!("{prefix}.input_file.file_id"));
                        None
                    }
                }
                other => {
                    dropped.path(format!("{prefix}.{other}"));
                    None
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

// ───────────────────────────────────────────────────────── 编码

/// 中间表示 → 发给 Responses 上游的请求。
pub fn encode_request(r: &Request, _t: &Target, dropped: &mut Dropped) -> Value {
    let freeform = |name: &str| {
        r.tools
            .iter()
            .any(|t| t.name == name && matches!(t.kind, ToolKind::Freeform { .. }))
    };
    let mut out = Map::new();
    out.insert("model".into(), json!(r.model));
    if !r.system.is_empty() {
        out.insert("instructions".into(), json!(r.system.join("\n\n")));
    }

    let mut input = Vec::new();
    for m in &r.messages {
        match m.role {
            Role::User => {
                let mut content = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::ToolResult(res) => {
                            let kind = if freeform_result(r, &res.id) {
                                "custom_tool_call_output"
                            } else {
                                "function_call_output"
                            };
                            input.push(json!({
                                "type": kind,
                                "call_id": res.id,
                                "output": result_output(res),
                            }));
                        }
                        Part::Text(t) if !t.is_empty() => {
                            content.push(json!({ "type": "input_text", "text": t }))
                        }
                        Part::Image(media) => content.push(json!({
                            "type": "input_image",
                            "image_url": media.to_uri(),
                            "detail": "auto",
                        })),
                        Part::File { media, name } => content.push(match media {
                            Media::Base64 { .. } => json!({
                                "type": "input_file",
                                "file_data": media.to_uri(),
                                "filename": name.as_deref().unwrap_or("file.pdf"),
                            }),
                            Media::Url(u) => json!({ "type": "input_file", "file_url": u }),
                        }),
                        _ => {}
                    }
                }
                if !content.is_empty() {
                    input.push(json!({ "type": "message", "role": "user", "content": content }));
                }
            }
            Role::Assistant => {
                let mut text = String::new();
                let flush = |text: &mut String, input: &mut Vec<Value>| {
                    if !text.is_empty() {
                        input.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": std::mem::take(text),
                        }));
                    }
                };
                for p in &m.parts {
                    match p {
                        Part::Text(t) => text.push_str(t),
                        Part::Thinking(th) => match th.signature.as_ref() {
                            Some(s) if s.vendor == Vendor::OpenAi => {
                                flush(&mut text, &mut input);
                                let (id, enc) = s.value.split_once(':').unwrap_or(("", &s.value));
                                let summary: Vec<Value> = if th.text.is_empty() {
                                    Vec::new()
                                } else {
                                    vec![json!({ "type": "summary_text", "text": th.text })]
                                };
                                input.push(json!({
                                    "type": "reasoning",
                                    "id": id,
                                    "summary": summary,
                                    "encrypted_content": enc,
                                }));
                            }
                            _ => dropped.feature(Feature::ReasoningHistory),
                        },
                        Part::ToolCall(c) => {
                            flush(&mut text, &mut input);
                            input.push(match &c.input {
                                ToolInput::Text(t) if freeform(&c.name) => json!({
                                    "type": "custom_tool_call",
                                    "call_id": c.id,
                                    "name": c.name,
                                    "input": t,
                                }),
                                other => json!({
                                    "type": "function_call",
                                    "call_id": c.id,
                                    "name": c.name,
                                    "arguments": other.to_json_text(),
                                }),
                            });
                        }
                        _ => {}
                    }
                }
                flush(&mut text, &mut input);
            }
        }
    }
    out.insert("input".into(), Value::Array(input));

    if !r.tools.is_empty() {
        let tools = r
            .tools
            .iter()
            .map(|tool| {
                let mut o = match &tool.kind {
                    // **strict 要明确写 false**：Responses 默认按严格模式校验 schema，
                    // 别家客户端的工具定义几乎都过不了
                    ToolKind::Function { schema, strict } => json!({
                        "type": "function",
                        "name": tool.name,
                        "parameters": schema,
                        "strict": strict.unwrap_or(false),
                    }),
                    ToolKind::Freeform { format } => {
                        let mut o = json!({ "type": "custom", "name": tool.name });
                        if let Some(f) = format {
                            o["format"] = f.clone();
                        }
                        o
                    }
                };
                if let Some(d) = &tool.description {
                    o["description"] = json!(d);
                }
                o
            })
            .collect();
        out.insert("tools".into(), Value::Array(tools));
        if let Some(c) = &r.tool_choice {
            out.insert(
                "tool_choice".into(),
                match c {
                    ToolChoice::Auto => json!("auto"),
                    ToolChoice::None => json!("none"),
                    ToolChoice::Required => json!("required"),
                    ToolChoice::Named(n) if freeform(n) => json!({ "type": "custom", "name": n }),
                    ToolChoice::Named(n) => json!({ "type": "function", "name": n }),
                },
            );
        }
        if let Some(p) = r.parallel_tool_calls {
            out.insert("parallel_tool_calls".into(), json!(p));
        }
    }

    if let Some(n) = r.max_tokens {
        out.insert("max_output_tokens".into(), json!(n));
    }
    if let Some(x) = r.temperature {
        out.insert("temperature".into(), json!(x));
    }
    if let Some(x) = r.top_p {
        out.insert("top_p".into(), json!(x));
    }
    for (present, f) in [
        (r.top_k.is_some(), Feature::TopK),
        (!r.stop.is_empty(), Feature::Stop),
        (r.seed.is_some(), Feature::Seed),
        (r.presence_penalty.is_some(), Feature::PresencePenalty),
        (r.frequency_penalty.is_some(), Feature::FrequencyPenalty),
    ] {
        if present {
            dropped.feature(f);
        }
    }

    match &r.reasoning {
        Some(re) if re.enabled => {
            let mut o = Map::new();
            if let Some(e) = think::effort(re) {
                o.insert("effort".into(), json!(think::openai(e)));
            }
            if re.summary {
                o.insert("summary".into(), json!("auto"));
            }
            if !o.is_empty() {
                out.insert("reasoning".into(), Value::Object(o));
            }
            // 推理内容加密带回来，下一轮才能接上
            out.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }
        Some(_) => dropped.feature(Feature::Reasoning),
        None => {}
    }

    match &r.format {
        Some(Format::JsonObject) => {
            out.insert(
                "text".into(),
                json!({ "format": { "type": "json_object" } }),
            );
        }
        Some(Format::JsonSchema {
            name,
            schema,
            strict,
        }) => {
            let mut f = json!({
                "type": "json_schema",
                "name": name.as_deref().unwrap_or("output"),
                "schema": schema,
            });
            if let Some(s) = strict {
                f["strict"] = json!(s);
            }
            out.insert("text".into(), json!({ "format": f }));
        }
        None => {}
    }

    // 不让 OpenAI 保存这次对话：转换过来的请求本来就带着完整的上下文
    out.insert("store".into(), json!(false));
    if r.stream {
        out.insert("stream".into(), json!(true));
    }
    Value::Object(out)
}

/// 这个工具结果对应的调用是自由格式工具发起的
fn freeform_result(r: &Request, call_id: &str) -> bool {
    r.messages.iter().flat_map(|m| &m.parts).any(|p| {
        matches!(p, Part::ToolCall(c) if c.id == call_id && matches!(c.input, ToolInput::Text(_))
            && r.tools.iter().any(|t| t.name == c.name && matches!(t.kind, ToolKind::Freeform { .. })))
    })
}

fn result_output(res: &ToolResult) -> Value {
    if !res.has_image() {
        return json!(res.text());
    }
    Value::Array(
        res.content
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(json!({ "type": "input_text", "text": t })),
                Part::Image(m) => Some(
                    json!({ "type": "input_image", "image_url": m.to_uri(), "detail": "auto" }),
                ),
                _ => None,
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(s: &str) -> Result<(Request, Vec<String>, ClientShape), Rejection> {
        let mut d = Dropped::new(Dialect::Responses);
        let mut shape = ClientShape::default();
        let r = decode_request(&serde_json::from_str(s).unwrap(), &mut d, &mut shape)?;
        Ok((r, d.into_vec(), shape))
    }

    fn encode(r: &Request, client: Dialect) -> (Value, Vec<String>) {
        let mut d = Dropped::new(client);
        let t = Target {
            dialect: Dialect::Responses,
            official: true,
            default_max_tokens: 8192,
        };
        (encode_request(r, &t, &mut d), d.into_vec())
    }

    /// Codex CLI 发的那种请求：instructions、developer 消息、函数工具和自由格式的
    /// apply_patch、加密的推理项、工具调用和结果。
    const CODEX: &str = r#"{
        "model": "gpt-5.1-codex",
        "instructions": "You are Codex.",
        "input": [
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "sandbox: workspace-write"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "修一下 bug"}]},
            {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "先看代码"}], "encrypted_content": "gAAAAB"},
            {"type": "function_call", "call_id": "call_1", "name": "shell", "arguments": "{\"command\":[\"ls\"]}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "src"},
            {"type": "custom_tool_call", "call_id": "call_2", "name": "apply_patch", "input": "*** Begin Patch"},
            {"type": "custom_tool_call_output", "call_id": "call_2", "output": "Done"},
            {"type": "web_search_call", "id": "ws_1", "status": "completed"}
        ],
        "tools": [
            {"type": "function", "name": "shell", "parameters": {"type": "object"}, "strict": false},
            {"type": "custom", "name": "apply_patch", "format": {"type": "grammar", "syntax": "lark", "definition": "start: x"}},
            {"type": "namespace", "name": "mcp_fs", "description": "files", "tools": [
                {"type": "function", "name": "read", "parameters": {"type": "object"}}
            ]},
            {"type": "web_search"}
        ],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "reasoning": {"effort": "high", "summary": "auto"},
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "k"
    }"#;

    #[test]
    fn a_codex_request_decodes_into_turns_tools_and_reasoning() {
        let (r, dropped, shape) = decode(CODEX).unwrap();
        assert_eq!(r.system, ["You are Codex.", "sandbox: workspace-write"]);
        let Part::Thinking(th) = &r.messages[1].parts[0] else {
            panic!("{:?}", r.messages[1]);
        };
        assert_eq!(th.text, "先看代码");
        assert_eq!(
            th.signature,
            Some(Signature::new(Vendor::OpenAi, "rs_1:gAAAAB"))
        );
        assert!(
            matches!(&r.messages[4].parts[0], Part::ToolCall(c) if c.input == ToolInput::Text("*** Begin Patch".into()))
        );
        let names: Vec<&str> = r.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["shell", "apply_patch", "mcp_fs__read"]);
        assert_eq!(
            shape.namespaced.get("mcp_fs__read"),
            Some(&("mcp_fs".to_string(), "read".to_string()))
        );
        let re = r.reasoning.unwrap();
        assert_eq!(re.effort, Some(Effort::High));
        assert!(re.summary);
        assert_eq!(dropped, ["tools.web_search", "input.web_search_call"]);
    }

    #[test]
    fn a_request_that_depends_on_server_state_is_refused() {
        for body in [
            r#"{"model":"m","previous_response_id":"resp_1","input":"hi"}"#,
            r#"{"model":"m","input":[{"type":"item_reference","id":"msg_1"}]}"#,
            r#"{"model":"m","input":[{"type":"compaction","encrypted_content":"x"}]}"#,
            r#"{"model":"m","background":true,"input":"hi"}"#,
        ] {
            let e = decode(body).unwrap_err();
            assert!(e.0.contains("无法转换"), "{body}: {e}");
        }
        // null 不算用了
        assert!(decode(r#"{"model":"m","previous_response_id":null,"input":"hi"}"#).is_ok());
    }

    #[test]
    fn back_to_responses_the_items_keep_their_kinds() {
        let (r, _, _) = decode(CODEX).unwrap();
        let (v, dropped) = encode(&r, Dialect::Responses);
        assert!(dropped.is_empty(), "{dropped:?}");
        let items = v["input"].as_array().unwrap();
        let kinds: Vec<&str> = items.iter().map(|i| i["type"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            [
                "message",
                "reasoning",
                "function_call",
                "function_call_output",
                "custom_tool_call",
                "custom_tool_call_output"
            ]
        );
        assert_eq!(items[1]["id"], "rs_1");
        assert_eq!(items[1]["encrypted_content"], "gAAAAB");
        assert_eq!(
            v["instructions"],
            "You are Codex.\n\nsandbox: workspace-write"
        );
        assert_eq!(v["tools"][0]["strict"], false);
        assert_eq!(v["tools"][1]["type"], "custom");
        assert_eq!(v["reasoning"], json!({"effort": "high", "summary": "auto"}));
        assert_eq!(v["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(v["store"], false);
    }

    #[test]
    fn a_claude_conversation_goes_out_as_items_and_foreign_thinking_is_reported() {
        let r = Request {
            model: "gpt-5".into(),
            system: vec!["sys".into()],
            max_tokens: Some(1000),
            stop: vec!["END".into()],
            top_k: Some(5),
            messages: vec![
                Message {
                    role: Role::User,
                    parts: vec![Part::Text("hi".into())],
                },
                Message {
                    role: Role::Assistant,
                    parts: vec![
                        Part::Thinking(Thinking {
                            text: "t".into(),
                            signature: Some(Signature::new(Vendor::Anthropic, "sig")),
                        }),
                        Part::Text("我看看".into()),
                        Part::ToolCall(ToolCall {
                            id: "toolu_1".into(),
                            name: "Read".into(),
                            input: ToolInput::Json(json!({"p": 1})),
                        }),
                    ],
                },
                Message {
                    role: Role::User,
                    parts: vec![Part::ToolResult(ToolResult {
                        id: "toolu_1".into(),
                        content: vec![Part::Text("内容".into())],
                        is_error: false,
                    })],
                },
            ],
            tools: vec![Tool {
                name: "Read".into(),
                description: Some("读".into()),
                kind: ToolKind::Function {
                    schema: json!({"type": "object"}),
                    strict: None,
                },
            }],
            reasoning: Some(Reasoning {
                enabled: true,
                effort: None,
                budget: Some(10000),
                summary: true,
            }),
            ..Default::default()
        };
        let (v, dropped) = encode(&r, Dialect::Anthropic);
        let items = v["input"].as_array().unwrap();
        assert_eq!(
            items[1],
            json!({"type": "message", "role": "assistant", "content": "我看看"})
        );
        assert_eq!(items[2]["arguments"], "{\"p\":1}");
        assert_eq!(
            items[3],
            json!({"type": "function_call_output", "call_id": "toolu_1", "output": "内容"})
        );
        assert_eq!(v["max_output_tokens"], 1000);
        assert_eq!(v["reasoning"]["effort"], "medium");
        assert_eq!(
            dropped,
            ["messages.content.thinking", "top_k", "stop_sequences"]
        );
    }
}
