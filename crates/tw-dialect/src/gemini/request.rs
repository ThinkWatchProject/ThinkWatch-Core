//! Gemini generateContent 请求 ⇄ 中间表示。
//!
//! Gemini 的 REST 接口是 proto3 JSON：字段名是驼峰，但下划线写法也被接受，所以解码
//! 时两种都认；编码一律写驼峰。

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::ir::*;
use crate::think;

/// 从别家历史转过来的工具调用没有 Gemini 的思考签名。Gemini 要求每一轮第一个
/// functionCall 带签名，没有时用 Google 为迁移历史提供的这个值（gemini-cli 自己的
/// 历史修复也用它）
pub const SYNTHETIC_SIGNATURE: &str = "skip_thought_signature_validator";

/// 按驼峰名取字段，取不到再试下划线写法
pub(crate) fn field<'a>(v: &'a Value, camel: &str) -> Option<&'a Value> {
    v.get(camel).or_else(|| {
        let mut snake = String::with_capacity(camel.len() + 4);
        for c in camel.chars() {
            if c.is_ascii_uppercase() {
                snake.push('_');
                snake.push(c.to_ascii_lowercase());
            } else {
                snake.push(c);
            }
        }
        v.get(snake)
    })
}

fn fstr<'a>(v: &'a Value, camel: &str) -> Option<&'a str> {
    field(v, camel).and_then(Value::as_str)
}

// ───────────────────────────────────────────────────────── 解码

/// 客户端发来的 Gemini 请求 → 中间表示。模型和是否流式写在路径里，由调用方给。
pub fn decode_request(
    v: &Value,
    model: &str,
    stream: bool,
    dropped: &mut Dropped,
) -> Result<Request, Rejection> {
    if !v.is_object() {
        return Err(Rejection("The request body is not a JSON object.".into()));
    }
    if field(v, "cachedContent").is_some_and(|c| !c.is_null()) {
        return Err(Rejection(
            "The request uses cachedContent, which lives on Google's servers, so it cannot be converted for an upstream of another format."
                .into(),
        ));
    }
    let mut r = Request {
        model: model.to_string(),
        stream,
        ..Default::default()
    };

    if let Some(sys) = field(v, "systemInstruction") {
        let t = match sys {
            Value::String(s) => s.clone(),
            _ => text_of(sys.get("parts").unwrap_or(&Value::Null)),
        };
        if !t.is_empty() {
            r.system.push(t);
        }
    }

    // 没有 id 的调用按名字排队，结果按名字依次认领
    let mut pending: HashMap<String, Vec<String>> = HashMap::new();
    for (ci, content) in arr_of(v, "contents").iter().enumerate() {
        let role = match str_of(content, "role") {
            Some("model") => Role::Assistant,
            _ => Role::User,
        };
        let mut parts = Vec::new();
        for (pi, p) in arr_of(content, "parts").iter().enumerate() {
            if let Some(t) = fstr(p, "text") {
                if p.get("thought").and_then(Value::as_bool) == Some(true) {
                    parts.push(Part::Thinking(Thinking {
                        text: t.to_string(),
                        signature: fstr(p, "thoughtSignature")
                            .and_then(|s| Signature::read(s, Vendor::Google)),
                    }));
                } else if !t.is_empty() {
                    parts.push(Part::Text(t.to_string()));
                }
            } else if let Some(blob) = field(p, "inlineData") {
                let mime = fstr(blob, "mimeType").unwrap_or_default().to_string();
                let data = fstr(blob, "data").unwrap_or_default().to_string();
                if mime.starts_with("image/") {
                    parts.push(Part::Image(Media::Base64 { mime, data }));
                } else if mime == "application/pdf" {
                    parts.push(Part::File {
                        media: Media::Base64 { mime, data },
                        name: None,
                    });
                } else {
                    dropped.path("contents.parts.inlineData");
                }
            } else if let Some(call) = field(p, "functionCall") {
                let name = fstr(call, "name").unwrap_or_default().to_string();
                let id = fstr(call, "id")
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call_{ci}_{pi}"));
                pending.entry(name.clone()).or_default().push(id.clone());
                parts.push(Part::ToolCall(ToolCall {
                    id,
                    name,
                    input: ToolInput::Json(
                        field(call, "args").cloned().unwrap_or_else(|| json!({})),
                    ),
                }));
            } else if let Some(resp) = field(p, "functionResponse") {
                let name = fstr(resp, "name").unwrap_or_default();
                let id = match fstr(resp, "id") {
                    Some(id) => id.to_string(),
                    None => pending
                        .get_mut(name)
                        .filter(|q| !q.is_empty())
                        .map(|q| q.remove(0))
                        .unwrap_or_else(|| format!("call_{ci}_{pi}")),
                };
                let body = resp.get("response").unwrap_or(&Value::Null);
                parts.push(Part::ToolResult(ToolResult {
                    id,
                    content: vec![Part::Text(response_text(body))],
                    is_error: body.get("error").is_some() && body.get("output").is_none(),
                }));
            } else if let Some(key) = ["fileData", "executableCode", "codeExecutionResult"]
                .into_iter()
                .find(|k| field(p, k).is_some())
            {
                dropped.path(format!("contents.parts.{key}"));
            }
        }
        r.messages.push(Message { role, parts });
    }

    for tool in arr_of(v, "tools") {
        let Some(obj) = tool.as_object() else {
            continue;
        };
        for (key, value) in obj {
            if key == "functionDeclarations" || key == "function_declarations" {
                for d in value.as_array().map(Vec::as_slice).unwrap_or(&[]) {
                    let schema = field(d, "parametersJsonSchema")
                        .cloned()
                        .or_else(|| d.get("parameters").map(openapi_to_json_schema))
                        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
                    r.tools.push(Tool {
                        name: fstr(d, "name").unwrap_or_default().to_string(),
                        description: fstr(d, "description").map(str::to_string),
                        kind: ToolKind::Function {
                            schema,
                            strict: None,
                        },
                    });
                }
            } else {
                dropped.path(format!("tools.{key}"));
            }
        }
    }

    if let Some(fc) = field(v, "toolConfig").and_then(|t| field(t, "functionCallingConfig")) {
        let allowed: Vec<&str> = field(fc, "allowedFunctionNames")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        r.tool_choice = match fstr(fc, "mode") {
            Some("AUTO") => Some(ToolChoice::Auto),
            Some("NONE") => Some(ToolChoice::None),
            Some("ANY") if allowed.len() == 1 => Some(ToolChoice::Named(allowed[0].to_string())),
            Some("ANY") => {
                if !allowed.is_empty() {
                    dropped.path("toolConfig.functionCallingConfig.allowedFunctionNames");
                }
                Some(ToolChoice::Required)
            }
            Some("VALIDATED") => {
                dropped.path("toolConfig.functionCallingConfig.mode");
                Some(ToolChoice::Auto)
            }
            _ => None,
        };
    }

    if let Some(g) = field(v, "generationConfig") {
        let num = |k: &str| field(g, k).and_then(Value::as_f64);
        r.max_tokens = field(g, "maxOutputTokens").and_then(Value::as_u64);
        r.temperature = num("temperature");
        r.top_p = num("topP");
        r.top_k = num("topK").map(|k| k as u64);
        r.seed = field(g, "seed").and_then(Value::as_i64);
        r.presence_penalty = num("presencePenalty");
        r.frequency_penalty = num("frequencyPenalty");
        r.stop = field(g, "stopSequences")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if fstr(g, "responseMimeType") == Some("application/json") {
            r.format = Some(match field(g, "responseJsonSchema") {
                Some(s) => Format::JsonSchema {
                    name: None,
                    schema: s.clone(),
                    strict: None,
                },
                None => match field(g, "responseSchema") {
                    Some(s) => Format::JsonSchema {
                        name: None,
                        schema: openapi_to_json_schema(s),
                        strict: None,
                    },
                    None => Format::JsonObject,
                },
            });
        }
        if let Some(t) = field(g, "thinkingConfig") {
            let budget = field(t, "thinkingBudget").and_then(Value::as_i64);
            r.reasoning = Some(Reasoning {
                enabled: budget != Some(0),
                effort: fstr(t, "thinkingLevel").and_then(think::parse_gemini_level),
                budget: budget.filter(|b| *b > 0).map(|b| b as u64),
                summary: field(t, "includeThoughts").and_then(Value::as_bool) == Some(true),
            });
        }
        if field(g, "candidateCount")
            .and_then(Value::as_u64)
            .is_some_and(|n| n > 1)
        {
            dropped.path("generationConfig.candidateCount");
        }
        if field(g, "responseModalities")
            .and_then(Value::as_array)
            .is_some_and(|m| m.iter().any(|x| x != "TEXT"))
        {
            dropped.path("generationConfig.responseModalities");
        }
        for k in [
            "responseLogprobs",
            "logprobs",
            "speechConfig",
            "mediaResolution",
        ] {
            if field(g, k).is_some_and(|x| !x.is_null() && x != &Value::Bool(false)) {
                dropped.path(format!("generationConfig.{k}"));
            }
        }
    }

    if field(v, "safetySettings").is_some_and(|s| s.as_array().is_some_and(|a| !a.is_empty())) {
        dropped.path("safetySettings");
    }
    Ok(r)
}

/// 函数结果写成文字：只有一个 output / result / content 字符串时取它本身
fn response_text(v: &Value) -> String {
    if let Some(o) = v.as_object()
        && o.len() == 1
        && let Some(s) = ["output", "result", "content", "error"]
            .iter()
            .find_map(|k| o.get(*k).and_then(Value::as_str))
    {
        return s.to_string();
    }
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Gemini 旧式 `parameters` 用的 OpenAPI 子集，类型名是大写的
fn openapi_to_json_schema(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| {
                    let x = match (k.as_str(), x) {
                        ("type", Value::String(t)) => json!(t.to_ascii_lowercase()),
                        _ => openapi_to_json_schema(x),
                    };
                    (k.clone(), x)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(openapi_to_json_schema).collect()),
        other => other.clone(),
    }
}

// ───────────────────────────────────────────────────────── 编码

/// 中间表示 → 发给 Gemini 上游的请求体。路径由调用方按模型和是否流式拼。
pub fn encode_request(r: &Request, _t: &Target, dropped: &mut Dropped) -> Value {
    let mut out = Map::new();
    if !r.system.is_empty() {
        out.insert(
            "systemInstruction".into(),
            json!({ "parts": [{ "text": r.system.join("\n\n") }] }),
        );
    }

    let names: HashMap<&str, &str> = r
        .messages
        .iter()
        .flat_map(|m| &m.parts)
        .filter_map(|p| match p {
            Part::ToolCall(c) => Some((c.id.as_str(), c.name.as_str())),
            _ => None,
        })
        .collect();

    let mut contents = Vec::new();
    for m in merge_roles(r.messages.clone()) {
        let mut parts = Vec::new();
        for p in &m.parts {
            match p {
                Part::Text(t) if !t.is_empty() => parts.push(json!({ "text": t })),
                Part::Text(_) => {}
                Part::Image(media) | Part::File { media, .. } => match media {
                    Media::Base64 { mime, data } => {
                        parts.push(json!({ "inlineData": { "mimeType": mime, "data": data } }))
                    }
                    Media::Url(_) => dropped.feature(Feature::MediaUrl),
                },
                Part::Thinking(th) => match &th.signature {
                    Some(s) if s.vendor == Vendor::Google => parts.push(json!({
                        "text": th.text,
                        "thought": true,
                        "thoughtSignature": s.value,
                    })),
                    _ => dropped.feature(Feature::ReasoningHistory),
                },
                Part::ToolCall(c) => parts.push(json!({
                    "functionCall": { "id": c.id, "name": c.name, "args": c.input.to_object() },
                })),
                Part::ToolResult(res) => {
                    if res.has_image() {
                        dropped.feature(Feature::ToolResultImage);
                    }
                    let key = if res.is_error { "error" } else { "output" };
                    parts.push(json!({
                        "functionResponse": {
                            "id": res.id,
                            "name": names.get(res.id.as_str()).copied().unwrap_or_default(),
                            "response": { key: res.text() },
                        },
                    }));
                }
            }
        }
        if parts.is_empty() {
            continue;
        }
        if m.role == Role::Assistant
            && let Some(first) = parts.iter_mut().find(|p| p.get("functionCall").is_some())
            && first.get("thoughtSignature").is_none()
        {
            first["thoughtSignature"] = json!(SYNTHETIC_SIGNATURE);
        }
        contents.push(json!({
            "role": if m.role == Role::User { "user" } else { "model" },
            "parts": parts,
        }));
    }
    out.insert("contents".into(), Value::Array(contents));

    if !r.tools.is_empty() {
        let decls: Vec<Value> = r
            .tools
            .iter()
            .map(|tool| {
                let schema = match &tool.kind {
                    ToolKind::Function { schema, .. } => schema.clone(),
                    ToolKind::Freeform { format } => {
                        if format.is_some() {
                            dropped.feature(Feature::FreeformFormat);
                        }
                        freeform_schema()
                    }
                };
                let mut d = json!({ "name": tool.name, "parametersJsonSchema": schema });
                if let Some(desc) = &tool.description {
                    d["description"] = json!(desc);
                }
                d
            })
            .collect();
        out.insert("tools".into(), json!([{ "functionDeclarations": decls }]));
        if let Some(c) = &r.tool_choice {
            let config = match c {
                ToolChoice::Auto => json!({ "mode": "AUTO" }),
                ToolChoice::None => json!({ "mode": "NONE" }),
                ToolChoice::Required => json!({ "mode": "ANY" }),
                ToolChoice::Named(n) => json!({ "mode": "ANY", "allowedFunctionNames": [n] }),
            };
            out.insert(
                "toolConfig".into(),
                json!({ "functionCallingConfig": config }),
            );
        }
        if r.parallel_tool_calls == Some(false) {
            dropped.feature(Feature::ParallelToolCalls);
        }
    }

    let mut g = Map::new();
    if let Some(n) = r.max_tokens {
        g.insert("maxOutputTokens".into(), json!(n));
    }
    for (key, value) in [
        ("temperature", r.temperature),
        ("topP", r.top_p),
        ("presencePenalty", r.presence_penalty),
        ("frequencyPenalty", r.frequency_penalty),
    ] {
        if let Some(x) = value {
            g.insert(key.into(), json!(x));
        }
    }
    if let Some(k) = r.top_k {
        g.insert("topK".into(), json!(k));
    }
    if let Some(s) = r.seed {
        g.insert("seed".into(), json!(s));
    }
    if !r.stop.is_empty() {
        g.insert("stopSequences".into(), json!(r.stop));
    }
    match &r.format {
        Some(Format::JsonObject) => {
            g.insert("responseMimeType".into(), json!("application/json"));
        }
        Some(Format::JsonSchema { schema, .. }) => {
            g.insert("responseMimeType".into(), json!("application/json"));
            g.insert("responseJsonSchema".into(), schema.clone());
        }
        None => {}
    }
    match &r.reasoning {
        Some(re) if re.enabled => {
            let mut t = Map::new();
            if think::gemini_uses_level(&r.model) {
                if let Some(e) = think::effort(re) {
                    t.insert("thinkingLevel".into(), json!(think::gemini_level(e)));
                }
            } else {
                // -1 是让模型自己决定
                let budget = think::budget(re).map(|b| b as i64).unwrap_or(-1);
                t.insert("thinkingBudget".into(), json!(budget));
            }
            if re.summary {
                t.insert("includeThoughts".into(), json!(true));
            }
            g.insert("thinkingConfig".into(), Value::Object(t));
        }
        // 只有 Flash 系列能关掉思考，Pro 和 Gemini 3 写 0 是 400
        Some(_) if r.model.contains("flash") && !think::gemini_uses_level(&r.model) => {
            g.insert("thinkingConfig".into(), json!({ "thinkingBudget": 0 }));
        }
        Some(_) => dropped.feature(Feature::Reasoning),
        None => {}
    }
    if !g.is_empty() {
        out.insert("generationConfig".into(), Value::Object(g));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(s: &str) -> (Request, Vec<String>) {
        let mut d = Dropped::new(Dialect::Gemini);
        let r = decode_request(
            &serde_json::from_str(s).unwrap(),
            "gemini-2.5-pro",
            true,
            &mut d,
        )
        .unwrap();
        (r, d.into_vec())
    }

    fn encode(r: &Request, client: Dialect) -> (Value, Vec<String>) {
        let mut d = Dropped::new(client);
        let t = Target {
            dialect: Dialect::Gemini,
            official: true,
            default_max_tokens: 8192,
        };
        (encode_request(r, &t, &mut d), d.into_vec())
    }

    /// Gemini CLI 那种请求：系统指令、工具调用和结果（没有 id）、思考配置。
    const GEMINI_CLI: &str = r#"{
        "systemInstruction": {"parts": [{"text": "You are Gemini CLI."}]},
        "contents": [
            {"role": "user", "parts": [{"text": "列一下文件"}]},
            {"role": "model", "parts": [
                {"text": "想想", "thought": true, "thoughtSignature": "CiQB"},
                {"functionCall": {"name": "list_directory", "args": {"path": "."}}}
            ]},
            {"role": "user", "parts": [{"functionResponse": {"name": "list_directory", "response": {"output": "a.rs"}}}]}
        ],
        "tools": [{"functionDeclarations": [
            {"name": "list_directory", "description": "ls", "parameters": {"type": "OBJECT", "properties": {"path": {"type": "STRING"}}}}
        ]}, {"googleSearch": {}}],
        "toolConfig": {"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["list_directory"]}},
        "generationConfig": {"temperature": 0, "topK": 40, "maxOutputTokens": 65536,
            "thinkingConfig": {"thinkingBudget": 8192, "includeThoughts": true}},
        "safetySettings": [{"category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_NONE"}]
    }"#;

    #[test]
    fn a_gemini_cli_request_decodes_and_calls_get_ids() {
        let (r, dropped) = decode(GEMINI_CLI);
        assert_eq!(r.model, "gemini-2.5-pro");
        assert!(r.stream);
        assert_eq!(r.system, ["You are Gemini CLI."]);
        let Part::ToolCall(call) = &r.messages[1].parts[1] else {
            panic!("{:?}", r.messages[1]);
        };
        let Part::ToolResult(res) = &r.messages[2].parts[0] else {
            panic!()
        };
        assert_eq!(res.id, call.id, "没有 id 的结果按名字认领调用");
        assert_eq!(res.text(), "a.rs");
        assert_eq!(
            r.tools[0].kind,
            ToolKind::Function {
                schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
                strict: None
            }
        );
        assert_eq!(
            r.tool_choice,
            Some(ToolChoice::Named("list_directory".into()))
        );
        assert_eq!(r.top_k, Some(40));
        let re = r.reasoning.unwrap();
        assert_eq!(
            (re.enabled, re.budget, re.summary),
            (true, Some(8192), true)
        );
        assert_eq!(dropped, ["tools.googleSearch", "safetySettings"]);
    }

    #[test]
    fn cached_content_is_refused() {
        let mut d = Dropped::new(Dialect::Gemini);
        let e = decode_request(
            &json!({"cachedContent": "cachedContents/1", "contents": []}),
            "m",
            false,
            &mut d,
        )
        .unwrap_err();
        assert!(e.0.contains("cachedContent"));
    }

    #[test]
    fn a_foreign_tool_loop_gets_names_back_and_a_synthetic_signature() {
        let r = Request {
            model: "gemini-3-pro-preview".into(),
            messages: vec![
                Message {
                    role: Role::User,
                    parts: vec![Part::Text("读 a".into())],
                },
                Message {
                    role: Role::Assistant,
                    parts: vec![
                        Part::Thinking(Thinking {
                            text: "t".into(),
                            signature: Some(Signature::new(Vendor::Anthropic, "sig")),
                        }),
                        Part::Text("好".into()),
                        Part::ToolCall(ToolCall {
                            id: "toolu_1".into(),
                            name: "Read".into(),
                            input: ToolInput::Json(json!({"p": "a"})),
                        }),
                    ],
                },
                Message {
                    role: Role::User,
                    parts: vec![Part::ToolResult(ToolResult {
                        id: "toolu_1".into(),
                        content: vec![Part::Text("内容".into())],
                        is_error: true,
                    })],
                },
            ],
            reasoning: Some(Reasoning {
                enabled: true,
                effort: None,
                budget: Some(4000),
                summary: false,
            }),
            parallel_tool_calls: Some(false),
            tools: vec![Tool {
                name: "Read".into(),
                description: None,
                kind: ToolKind::Function {
                    schema: json!({"type": "object"}),
                    strict: None,
                },
            }],
            ..Default::default()
        };
        let (v, dropped) = encode(&r, Dialect::Anthropic);
        let model = &v["contents"][1];
        assert_eq!(model["role"], "model");
        assert_eq!(model["parts"][1]["functionCall"]["name"], "Read");
        assert_eq!(model["parts"][1]["thoughtSignature"], SYNTHETIC_SIGNATURE);
        let result = &v["contents"][2]["parts"][0]["functionResponse"];
        assert_eq!(result["name"], "Read");
        assert_eq!(result["response"]["error"], "内容");
        assert_eq!(
            v["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["type"],
            "object"
        );
        // Gemini 3 用等级
        assert_eq!(
            v["generationConfig"]["thinkingConfig"],
            json!({"thinkingLevel": "LOW"})
        );
        assert_eq!(
            dropped,
            [
                "messages.content.thinking",
                "tool_choice.disable_parallel_tool_use"
            ]
        );
    }

    #[test]
    fn thinking_can_only_be_turned_off_on_flash() {
        let mut r = Request {
            model: "gemini-2.5-flash".into(),
            reasoning: Some(Reasoning {
                enabled: false,
                effort: None,
                budget: None,
                summary: false,
            }),
            ..Default::default()
        };
        let (v, dropped) = encode(&r, Dialect::Anthropic);
        assert_eq!(v["generationConfig"]["thinkingConfig"]["thinkingBudget"], 0);
        assert!(dropped.is_empty());
        r.model = "gemini-2.5-pro".into();
        let (v, dropped) = encode(&r, Dialect::Anthropic);
        assert!(v.get("generationConfig").is_none());
        assert_eq!(dropped, ["thinking"]);
    }

    #[test]
    fn snake_case_fields_are_understood_too() {
        let mut d = Dropped::new(Dialect::Gemini);
        let r = decode_request(
            &json!({
                "system_instruction": {"parts": [{"text": "s"}]},
                "contents": [{"role": "user", "parts": [{"inline_data": {"mime_type": "image/png", "data": "AA"}}]}],
                "generation_config": {"max_output_tokens": 10}
            }),
            "m",
            false,
            &mut d,
        )
        .unwrap();
        assert_eq!(r.system, ["s"]);
        assert!(matches!(&r.messages[0].parts[0], Part::Image(_)));
        assert_eq!(r.max_tokens, Some(10));
    }
}
