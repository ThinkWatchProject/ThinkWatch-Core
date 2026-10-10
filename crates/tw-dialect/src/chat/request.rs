//! OpenAI Chat Completions 请求 ⇄ 中间表示。

use serde_json::{Map, Value, json};

use crate::ir::*;
use crate::think;

// ───────────────────────────────────────────────────────── 解码

/// 客户端发来的 Chat 请求 → 中间表示。
pub fn decode_request(
    v: &Value,
    dropped: &mut Dropped,
    shape: &mut ClientShape,
) -> Result<Request, Rejection> {
    if !v.is_object() {
        return Err(Rejection("The request body is not a JSON object.".into()));
    }
    let mut r = Request {
        model: str_of(v, "model").unwrap_or_default().to_string(),
        max_tokens: u64_of(v, "max_completion_tokens").or_else(|| u64_of(v, "max_tokens")),
        temperature: f64_of(v, "temperature"),
        top_p: f64_of(v, "top_p"),
        seed: v.get("seed").and_then(Value::as_i64),
        presence_penalty: f64_of(v, "presence_penalty"),
        frequency_penalty: f64_of(v, "frequency_penalty"),
        parallel_tool_calls: v.get("parallel_tool_calls").and_then(Value::as_bool),
        stream: v.get("stream").and_then(Value::as_bool).unwrap_or(false),
        ..Default::default()
    };
    shape.include_usage = v
        .get("stream_options")
        .and_then(|o| o.get("include_usage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    for m in arr_of(v, "messages") {
        let content = m.get("content").unwrap_or(&Value::Null);
        match str_of(m, "role").unwrap_or("") {
            "system" | "developer" => {
                let t = text_of(content);
                if !t.is_empty() {
                    system_turn(&mut r, t);
                }
            }
            "user" => r.messages.push(Message {
                role: Role::User,
                parts: user_parts(content, dropped),
            }),
            "assistant" => r.messages.push(Message {
                role: Role::Assistant,
                parts: assistant_parts(m, dropped),
            }),
            "tool" => r.messages.push(Message {
                role: Role::User,
                parts: vec![Part::ToolResult(ToolResult {
                    id: str_of(m, "tool_call_id").unwrap_or_default().to_string(),
                    content: match text_of(content) {
                        t if t.is_empty() => Vec::new(),
                        t => vec![Part::Text(t)],
                    },
                    is_error: false,
                })],
            }),
            other => dropped.path(format!("messages.{other}")),
        }
    }

    for t in arr_of(v, "tools") {
        match str_of(t, "type") {
            Some("function") => {
                let f = t.get("function").unwrap_or(&Value::Null);
                r.tools.push(Tool {
                    name: str_of(f, "name").unwrap_or_default().to_string(),
                    description: str_of(f, "description").map(str::to_string),
                    kind: ToolKind::Function {
                        schema: f
                            .get("parameters")
                            .cloned()
                            .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                        strict: f.get("strict").and_then(Value::as_bool),
                    },
                });
            }
            Some("custom") => {
                let c = t.get("custom").unwrap_or(&Value::Null);
                r.tools.push(Tool {
                    name: str_of(c, "name").unwrap_or_default().to_string(),
                    description: str_of(c, "description").map(str::to_string),
                    kind: ToolKind::Freeform {
                        format: c.get("format").cloned(),
                    },
                });
            }
            other => dropped.path(format!("tools.{}", other.unwrap_or("unknown"))),
        }
    }

    r.tool_choice = match v.get("tool_choice") {
        Some(Value::String(s)) => match s.as_str() {
            "auto" => Some(ToolChoice::Auto),
            "none" => Some(ToolChoice::None),
            "required" => Some(ToolChoice::Required),
            _ => None,
        },
        Some(o @ Value::Object(_)) => match str_of(o, "type") {
            Some(kind @ ("function" | "custom")) => o
                .get(kind)
                .and_then(|x| str_of(x, "name"))
                .map(|n| ToolChoice::Named(n.to_string())),
            Some("allowed_tools") => {
                dropped.path("tool_choice.allowed_tools");
                match o.get("allowed_tools").and_then(|a| str_of(a, "mode")) {
                    Some("required") => Some(ToolChoice::Required),
                    _ => Some(ToolChoice::Auto),
                }
            }
            _ => None,
        },
        _ => None,
    };

    r.stop = match v.get("stop") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };

    if let Some(e) = str_of(v, "reasoning_effort").and_then(think::parse_openai) {
        r.reasoning = Some(Reasoning {
            enabled: e.is_some(),
            effort: e,
            budget: None,
            // Chat 没有推理输出字段，但不少客户端认 `reasoning_content`，要回来给它们
            summary: e.is_some(),
        });
    }
    // DeepSeek、GLM、Kimi 的写法：`thinking.type` 开关推理，强度另写在 `reasoning_effort`。
    // 关掉时以它为准
    match v.get("thinking").and_then(|t| str_of(t, "type")) {
        Some("disabled") => {
            r.reasoning = Some(Reasoning {
                enabled: false,
                effort: None,
                budget: None,
                summary: false,
            })
        }
        Some("enabled") if r.reasoning.is_none() => {
            r.reasoning = Some(Reasoning {
                enabled: true,
                effort: None,
                budget: None,
                summary: true,
            })
        }
        _ => {}
    }

    r.format = match v.get("response_format").and_then(|f| str_of(f, "type")) {
        Some("json_object") => Some(Format::JsonObject),
        Some("json_schema") => {
            let s = v["response_format"]
                .get("json_schema")
                .unwrap_or(&Value::Null);
            Some(Format::JsonSchema {
                name: str_of(s, "name").map(str::to_string),
                schema: s.get("schema").cloned().unwrap_or(Value::Null),
                strict: s.get("strict").and_then(Value::as_bool),
            })
        }
        _ => None,
    };

    if has(v, "verbosity") {
        r.verbosity = str_of(v, "verbosity").and_then(Verbosity::parse);
        if r.verbosity.is_none() {
            dropped.path("verbosity");
        }
    }

    if u64_of(v, "n").is_some_and(|n| n > 1) {
        dropped.path("n");
    }
    if v.get("logprobs").and_then(Value::as_bool) == Some(true) {
        dropped.path("logprobs");
    }
    if v.get("modalities")
        .and_then(Value::as_array)
        .is_some_and(|m| m.iter().any(|x| x != "text"))
    {
        dropped.path("modalities");
    }
    for k in [
        "top_logprobs",
        "logit_bias",
        "prediction",
        "audio",
        "web_search_options",
        "moderation",
        "functions",
        "function_call",
    ] {
        if has(v, k) {
            dropped.path(k);
        }
    }
    Ok(r)
}

fn user_parts(content: &Value, dropped: &mut Dropped) -> Vec<Part> {
    match content {
        Value::String(s) if !s.is_empty() => vec![Part::Text(s.clone())],
        Value::Array(items) => items
            .iter()
            .filter_map(|p| match str_of(p, "type").unwrap_or("") {
                "text" => str_of(p, "text").map(|t| Part::Text(t.to_string())),
                "image_url" => p
                    .get("image_url")
                    .and_then(|i| str_of(i, "url"))
                    .map(|u| Part::Image(Media::from_uri(u))),
                "file" => {
                    let f = p.get("file").unwrap_or(&Value::Null);
                    match str_of(f, "file_data") {
                        Some(data) => Some(Part::File {
                            media: match Media::from_uri(data) {
                                m @ Media::Base64 { .. } => m,
                                // 不是 data URI 的就是裸的 base64
                                Media::Url(_) => Media::Base64 {
                                    mime: "application/pdf".into(),
                                    data: data.to_string(),
                                },
                            },
                            name: str_of(f, "filename").map(str::to_string),
                        }),
                        None => {
                            dropped.path("messages.content.file.file_id");
                            None
                        }
                    }
                }
                other => {
                    dropped.path(format!("messages.content.{other}"));
                    None
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn assistant_parts(m: &Value, dropped: &mut Dropped) -> Vec<Part> {
    let mut parts = Vec::new();
    // DeepSeek 等实现的推理字段，不在 OpenAI 的定义里。DeepSeek 叫 reasoning_content，
    // OpenRouter、vLLM 叫 reasoning
    if let Some(t) = str_of(m, "reasoning_content")
        .or_else(|| str_of(m, "reasoning"))
        .filter(|t| !t.is_empty())
    {
        parts.push(Part::Thinking(Thinking {
            text: t.to_string(),
            signature: Some(Signature::new(Vendor::Chat, "")),
        }));
    }
    match m.get("content") {
        Some(Value::String(s)) if !s.is_empty() => parts.push(Part::Text(s.clone())),
        Some(Value::Array(items)) => parts.extend(items.iter().filter_map(|p| {
            str_of(p, "text")
                .or_else(|| str_of(p, "refusal"))
                .map(|t| Part::Text(t.to_string()))
        })),
        _ => {}
    }
    for c in arr_of(m, "tool_calls") {
        let call = match str_of(c, "type") {
            Some("custom") => c.get("custom").map(|x| ToolCall {
                id: str_of(c, "id").unwrap_or_default().to_string(),
                name: str_of(x, "name").unwrap_or_default().to_string(),
                input: ToolInput::Text(str_of(x, "input").unwrap_or_default().to_string()),
            }),
            _ => c.get("function").map(|f| ToolCall {
                id: str_of(c, "id").unwrap_or_default().to_string(),
                name: str_of(f, "name").unwrap_or_default().to_string(),
                input: ToolInput::from_json_text(str_of(f, "arguments").unwrap_or_default()),
            }),
        };
        parts.extend(call.map(Part::ToolCall));
    }
    for k in ["function_call", "audio"] {
        if has(m, k) {
            dropped.path(format!("messages.{k}"));
        }
    }
    parts
}

// ───────────────────────────────────────────────────────── 编码

/// 中间表示 → 发给 Chat 上游的请求。
pub fn encode_request(r: &Request, t: &Target, dropped: &mut Dropped) -> Value {
    // 对话中途的系统消息写成带标记的用户消息（见 `fold_system_turns`）
    let folded = fold_system_turns(r);
    let r = folded.as_ref();
    let mut out = Map::new();
    out.insert("model".into(), json!(r.model));

    let mut messages = Vec::new();
    if !r.system.is_empty() {
        messages.push(json!({ "role": "system", "content": r.system.join("\n\n") }));
    }
    // 连着的几条助手消息并成一条：Responses 客户端一项一条消息，推理、文字、工具调用
    // 各在一条里，而 DeepSeek 要 reasoning_content 和 tool_calls 写在同一条助手消息上。
    // 用户那一侧不并：对话中途折成用户消息的系统提示要留在原位（见 `fold_system_turns`）
    for m in &merge_assistant_turns(&r.messages) {
        match m.role {
            Role::User | Role::System => user_messages(m, dropped, &mut messages),
            Role::Assistant => {
                if let Some(a) = assistant_message(m, dropped) {
                    messages.push(a);
                }
            }
        }
    }
    out.insert("messages".into(), Value::Array(messages));

    if !r.tools.is_empty() {
        let tools = r
            .tools
            .iter()
            .map(|tool| {
                let mut f = json!({ "name": tool.name });
                if let Some(d) = &tool.description {
                    f["description"] = json!(d);
                }
                match &tool.kind {
                    ToolKind::Function { schema, strict } => {
                        f["parameters"] = schema.clone();
                        if let Some(s) = strict {
                            f["strict"] = json!(s);
                        }
                    }
                    // 兼容实现大多不认 custom 工具，一律按「一个字符串参数的函数」声明
                    ToolKind::Freeform { format } => {
                        if format.is_some() {
                            dropped.feature(Feature::FreeformFormat);
                        }
                        f["parameters"] = freeform_schema();
                    }
                }
                json!({ "type": "function", "function": f })
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
                    ToolChoice::Named(n) => {
                        json!({ "type": "function", "function": { "name": n } })
                    }
                },
            );
        }
        if let Some(p) = r.parallel_tool_calls {
            out.insert("parallel_tool_calls".into(), json!(p));
        }
    }

    if let Some(n) = r.max_tokens {
        // OpenAI 官方的推理模型拒绝 max_tokens；兼容实现大多只认 max_tokens
        let key = if t.official {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        out.insert(key.into(), json!(n));
    }
    for (key, value) in [
        ("temperature", r.temperature),
        ("top_p", r.top_p),
        ("presence_penalty", r.presence_penalty),
        ("frequency_penalty", r.frequency_penalty),
    ] {
        if let Some(x) = value {
            out.insert(key.into(), json!(x));
        }
    }
    if let Some(s) = r.seed {
        out.insert("seed".into(), json!(s));
    }
    if !r.stop.is_empty() {
        out.insert("stop".into(), json!(r.stop));
    }
    if r.top_k.is_some() {
        dropped.feature(Feature::TopK);
    }

    match &r.reasoning {
        Some(re) if re.enabled => {
            if let Some(e) = think::effort(re) {
                out.insert("reasoning_effort".into(), json!(think::openai(e)));
            }
        }
        // 「关掉推理」在不支持推理的模型上会被拒绝，发不出去
        Some(_) => dropped.feature(Feature::Reasoning),
        None => {}
    }

    match r.verbosity {
        Some(x) if Verbosity::understood_by(&r.model) => {
            out.insert("verbosity".into(), json!(x.as_str()));
        }
        Some(_) => dropped.feature(Feature::Verbosity),
        None => {}
    }

    match &r.format {
        Some(Format::JsonObject) => {
            out.insert("response_format".into(), json!({ "type": "json_object" }));
        }
        Some(Format::JsonSchema {
            name,
            schema,
            strict,
        }) => {
            let mut s = json!({ "name": name.as_deref().unwrap_or("output"), "schema": schema });
            if let Some(x) = strict {
                s["strict"] = json!(x);
            }
            out.insert(
                "response_format".into(),
                json!({ "type": "json_schema", "json_schema": s }),
            );
        }
        None => {}
    }

    if r.stream {
        out.insert("stream".into(), json!(true));
        // **不加这一条，流里根本没有用量**，费用面板对这些请求集体失明
        out.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    Value::Object(out)
}

/// 一条 user 消息 → 零到多条：工具结果各自成一条 `role: tool`，排在前面
fn user_messages(m: &Message, dropped: &mut Dropped, out: &mut Vec<Value>) {
    let mut parts = Vec::new();
    for p in &m.parts {
        match p {
            Part::ToolResult(res) => {
                if res.has_image() {
                    dropped.feature(Feature::ToolResultImage);
                }
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": res.id,
                    "content": res.text(),
                }));
            }
            Part::Text(s) if !s.is_empty() => parts.push(json!({ "type": "text", "text": s })),
            Part::Image(media) => parts.push(json!({
                "type": "image_url",
                "image_url": { "url": media.to_uri() },
            })),
            Part::File {
                media: media @ Media::Base64 { .. },
                name,
            } => parts.push(json!({
                "type": "file",
                "file": {
                    "file_data": media.to_uri(),
                    "filename": name.as_deref().unwrap_or("file.pdf"),
                },
            })),
            Part::File {
                media: Media::Url(_),
                ..
            } => dropped.feature(Feature::MediaUrl),
            _ => {}
        }
    }
    if parts.is_empty() {
        return;
    }
    // 全是文字时写字符串：那是兼容实现最稳的形状
    let content = if parts.iter().all(|p| p["type"] == "text") {
        json!(
            parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        )
    } else {
        Value::Array(parts)
    };
    out.push(json!({ "role": "user", "content": content }));
}

/// 连着的助手消息并成一条，别的原样
fn merge_assistant_turns(messages: &[Message]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for m in messages {
        match out.last_mut() {
            Some(last) if last.role == Role::Assistant && m.role == Role::Assistant => {
                last.parts.extend(m.parts.iter().cloned())
            }
            _ => out.push(m.clone()),
        }
    }
    out
}

fn assistant_message(m: &Message, dropped: &mut Dropped) -> Option<Value> {
    let mut texts = Vec::new();
    let mut reasoning = Vec::new();
    let mut calls = Vec::new();
    for p in &m.parts {
        match p {
            Part::Text(s) if !s.is_empty() => texts.push(s.as_str()),
            Part::ToolCall(c) => calls.push(json!({
                "id": c.id,
                "type": "function",
                "function": { "name": c.name, "arguments": c.input.to_json_text() },
            })),
            // 只写回 Chat 上游自己给的推理：DeepSeek 在工具调用的那几轮要它原样带回；
            // 别家签发的写过去没有意义，而且 OpenAI 自己的接口不认 reasoning_content
            Part::Thinking(th) => match &th.signature {
                Some(s) if s.vendor == Vendor::Chat && !th.text.is_empty() => {
                    reasoning.push(th.text.as_str())
                }
                Some(s) if s.vendor == Vendor::Chat => {}
                _ => dropped.feature(Feature::ReasoningHistory),
            },
            _ => {}
        }
    }
    if texts.is_empty() && calls.is_empty() {
        return None;
    }
    let mut msg = json!({
        "role": "assistant",
        "content": if texts.is_empty() { Value::Null } else { json!(texts.join("\n")) },
    });
    if !reasoning.is_empty() {
        msg["reasoning_content"] = json!(reasoning.join("\n\n"));
    }
    if !calls.is_empty() {
        msg["tool_calls"] = Value::Array(calls);
    }
    Some(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(s: &str) -> (Request, Vec<String>, ClientShape) {
        let mut d = Dropped::new(Dialect::Chat);
        let mut shape = ClientShape::default();
        let r = decode_request(&serde_json::from_str(s).unwrap(), &mut d, &mut shape).unwrap();
        (r, d.into_vec(), shape)
    }

    fn encode(r: &Request, client: Dialect, official: bool) -> (Value, Vec<String>) {
        let mut d = Dropped::new(client);
        let t = Target {
            dialect: Dialect::Chat,
            official,
            default_max_tokens: 8192,
        };
        (encode_request(r, &t, &mut d), d.into_vec())
    }

    const CLIENT: &str = r#"{
        "model": "gpt-5",
        "messages": [
            {"role": "developer", "content": "Be brief."},
            {"role": "user", "content": [
                {"type": "text", "text": "这张图是什么？"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBOR", "detail": "high"}}
            ]},
            {"role": "assistant", "content": null, "reasoning_content": "看图",
             "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "zoom", "arguments": "{\"x\":1}"}}]},
            {"role": "tool", "tool_call_id": "call_1", "content": "放大了"}
        ],
        "tools": [{"type": "function", "function": {"name": "zoom", "parameters": {"type": "object"}}}],
        "tool_choice": {"type": "function", "function": {"name": "zoom"}},
        "max_completion_tokens": 2000,
        "stop": "END",
        "n": 2,
        "logit_bias": {"1": 2},
        "reasoning_effort": "low",
        "response_format": {"type": "json_schema", "json_schema": {"name": "a", "schema": {"type": "object"}, "strict": true}},
        "stream": true,
        "stream_options": {"include_usage": true}
    }"#;

    #[test]
    fn a_chat_request_decodes_with_its_extensions() {
        let (r, dropped, shape) = decode(CLIENT);
        assert_eq!(r.system, ["Be brief."]);
        assert!(
            matches!(&r.messages[0].parts[1], Part::Image(Media::Base64 { mime, .. }) if mime == "image/png")
        );
        assert!(matches!(&r.messages[1].parts[0], Part::Thinking(t) if t.text == "看图"));
        assert!(
            matches!(&r.messages[1].parts[1], Part::ToolCall(c) if c.input == ToolInput::Json(json!({"x": 1})))
        );
        assert!(matches!(&r.messages[2].parts[0], Part::ToolResult(t) if t.id == "call_1"));
        assert_eq!(r.tool_choice, Some(ToolChoice::Named("zoom".into())));
        assert_eq!(r.max_tokens, Some(2000));
        assert_eq!(r.stop, ["END"]);
        assert_eq!(r.reasoning.as_ref().unwrap().effort, Some(Effort::Low));
        assert!(matches!(
            r.format,
            Some(Format::JsonSchema {
                strict: Some(true),
                ..
            })
        ));
        assert!(shape.include_usage);
        assert_eq!(dropped, ["n", "logit_bias"]);
    }

    #[test]
    fn encoding_puts_tool_results_in_their_own_messages() {
        let (r, _, _) = decode(CLIENT);
        let (v, dropped) = encode(&r, Dialect::Anthropic, false);
        let m = v["messages"].as_array().unwrap();
        assert_eq!(m[0], json!({"role": "system", "content": "Be brief."}));
        assert_eq!(
            m[1]["content"][1]["image_url"]["url"],
            "data:image/png;base64,iVBOR"
        );
        assert_eq!(m[2]["tool_calls"][0]["function"]["arguments"], "{\"x\":1}");
        assert_eq!(m[2]["content"], Value::Null);
        assert_eq!(
            m[3],
            json!({"role": "tool", "tool_call_id": "call_1", "content": "放大了"})
        );
        // 兼容实现：max_tokens；Chat 上游给的推理内容原样写回
        assert_eq!(v["max_tokens"], 2000);
        assert_eq!(m[2]["reasoning_content"], "看图");
        assert!(dropped.is_empty(), "{dropped:?}");
        assert_eq!(v["stream_options"]["include_usage"], true);
        assert_eq!(v["response_format"]["json_schema"]["strict"], true);
    }

    #[test]
    fn reasoning_signed_by_another_vendor_is_not_written_back() {
        let r = Request {
            model: "gpt-5".into(),
            messages: vec![Message {
                role: Role::Assistant,
                parts: vec![
                    Part::Thinking(Thinking {
                        text: "想".into(),
                        signature: Some(Signature::new(Vendor::Anthropic, "sig")),
                    }),
                    Part::Thinking(Thinking {
                        text: "想".into(),
                        signature: None,
                    }),
                    Part::Text("答".into()),
                ],
            }],
            ..Default::default()
        };
        let (v, dropped) = encode(&r, Dialect::Anthropic, true);
        assert_eq!(v["messages"][0]["content"], "答");
        assert!(v["messages"][0].get("reasoning_content").is_none());
        assert_eq!(dropped, ["messages.content.thinking"]);
    }

    #[test]
    fn reasoning_is_read_too() {
        let (r, _, _) = decode(
            r#"{"model": "m", "messages": [{"role": "assistant", "content": "答", "reasoning": "想"}]}"#,
        );
        assert_eq!(
            r.messages[0].parts[0],
            Part::Thinking(Thinking {
                text: "想".into(),
                signature: Some(Signature::new(Vendor::Chat, "")),
            })
        );
    }

    #[test]
    fn the_official_endpoint_gets_max_completion_tokens() {
        let (r, _, _) = decode(CLIENT);
        let (v, _) = encode(&r, Dialect::Chat, true);
        assert_eq!(v["max_completion_tokens"], 2000);
        assert!(v.get("max_tokens").is_none());
    }

    #[test]
    fn a_freeform_tool_and_top_k_are_handled_honestly() {
        let r = Request {
            model: "deepseek-chat".into(),
            top_k: Some(20),
            tools: vec![Tool {
                name: "apply_patch".into(),
                description: None,
                kind: ToolKind::Freeform { format: None },
            }],
            messages: vec![Message {
                role: Role::Assistant,
                parts: vec![Part::ToolCall(ToolCall {
                    id: "c".into(),
                    name: "apply_patch".into(),
                    input: ToolInput::Text("patch".into()),
                })],
            }],
            reasoning: Some(Reasoning {
                enabled: false,
                effort: None,
                budget: None,
                summary: false,
            }),
            ..Default::default()
        };
        let (v, dropped) = encode(&r, Dialect::Responses, false);
        assert_eq!(v["tools"][0]["function"]["parameters"], freeform_schema());
        assert_eq!(
            v["messages"][0]["tool_calls"][0]["function"]["arguments"],
            "{\"input\":\"patch\"}"
        );
        assert_eq!(dropped, ["top_k", "reasoning"]);
    }

    #[test]
    fn a_budget_becomes_an_effort() {
        let r = Request {
            model: "gpt-5".into(),
            reasoning: Some(Reasoning {
                enabled: true,
                effort: None,
                budget: Some(31999),
                summary: true,
            }),
            ..Default::default()
        };
        let (v, _) = encode(&r, Dialect::Anthropic, true);
        assert_eq!(v["reasoning_effort"], "high");
    }
}
