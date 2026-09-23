//! Bedrock Converse 请求 ⇄ 中间表示。
//!
//! 模型名在路径里（`/model/{id}/converse`），不在请求体里 —— 和 Gemini 一样。
//!
//! `inferenceConfig` 只有四个旋钮:`maxTokens`、`temperature`、`topP`、
//! `stopSequences`。**别的一概走 `additionalModelRequestFields`** —— 那是个
//! 原样透传给模型的口袋，Converse 自己不解释里面装的东西。`top_k` 就走它。

use serde_json::{Map, Value, json};

use crate::ir::*;

/// 图片的 MIME → Converse 的 `format`。
///
/// **Converse 只认这四种**，而且写的是短名不是 MIME。认不出来的丢掉 —— 拿一个
/// 猜的格式去发，模型收到的是一张解不开的图
fn image_format(mime: &str) -> Option<&'static str> {
    match mime.rsplit('/').next()? {
        "png" => Some("png"),
        "jpeg" | "jpg" => Some("jpeg"),
        "gif" => Some("gif"),
        "webp" => Some("webp"),
        _ => None,
    }
}

/// 文件的 MIME 或扩展名 → `document.format`。
fn document_format(mime: &str, name: Option<&str>) -> Option<&'static str> {
    let by_ext = name
        .and_then(|n| n.rsplit('.').next())
        .map(str::to_ascii_lowercase);
    let candidate = match mime {
        "application/pdf" => "pdf",
        "text/csv" => "csv",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "text/html" => "html",
        "text/plain" => "txt",
        "text/markdown" => "md",
        _ => by_ext.as_deref().unwrap_or_default(),
    };
    match candidate {
        "pdf" => Some("pdf"),
        "csv" => Some("csv"),
        "doc" => Some("doc"),
        "docx" => Some("docx"),
        "xls" => Some("xls"),
        "xlsx" => Some("xlsx"),
        "html" | "htm" => Some("html"),
        "txt" | "text" => Some("txt"),
        "md" | "markdown" => Some("md"),
        _ => None,
    }
}

/// Converse 要求文档有名字，而且同一个请求里不能重名。
fn document_name(name: Option<&str>, index: usize) -> String {
    match name {
        Some(n) if !n.trim().is_empty() => n.to_string(),
        _ => format!("document-{index}"),
    }
}

// ───────────────────────────────────────────── 中间表示 → Converse

pub fn encode_request(r: &Request, t: &Target, dropped: &mut Dropped) -> Value {
    let mut out = Map::new();

    if !r.system.is_empty() {
        let blocks: Vec<Value> = r.system.iter().map(|s| json!({ "text": s })).collect();
        out.insert("system".into(), Value::Array(blocks));
    }

    let mut messages = Vec::new();
    let mut doc_index = 0usize;
    for m in merge_roles(r.messages.clone()) {
        let mut content = Vec::new();
        for p in &m.parts {
            match p {
                Part::Text(text) if !text.is_empty() => content.push(json!({ "text": text })),
                Part::Text(_) => {}
                Part::Image(media) => match media {
                    Media::Base64 { mime, data } => match image_format(mime) {
                        Some(format) => content.push(json!({
                            "image": { "format": format, "source": { "bytes": data } },
                        })),
                        None => dropped.path("messages.content.image"),
                    },
                    // Converse 只收字节和 S3，没有取 URL 这一说
                    Media::Url(_) => dropped.feature(Feature::MediaUrl),
                },
                Part::File { media, name } => match media {
                    Media::Base64 { mime, data } => match document_format(mime, name.as_deref()) {
                        Some(format) => {
                            content.push(json!({
                                "document": {
                                    "format": format,
                                    "name": document_name(name.as_deref(), doc_index),
                                    "source": { "bytes": data },
                                },
                            }));
                            doc_index += 1;
                        }
                        None => dropped.feature(Feature::File),
                    },
                    Media::Url(_) => dropped.feature(Feature::MediaUrl),
                },
                Part::Thinking(th) => match &th.signature {
                    // 签名是 Bedrock 上的 Anthropic 模型签的，带回去才算数
                    Some(s) if s.vendor == Vendor::Anthropic && !s.redacted => {
                        content.push(json!({
                            "reasoningContent": {
                                "reasoningText": { "text": th.text, "signature": s.value },
                            },
                        }))
                    }
                    Some(s) if s.redacted => content.push(json!({
                        "reasoningContent": { "redactedContent": s.value },
                    })),
                    _ => dropped.feature(Feature::ReasoningHistory),
                },
                Part::ToolCall(c) => content.push(json!({
                    "toolUse": {
                        "toolUseId": c.id,
                        "name": c.name,
                        "input": c.input.to_object(),
                    },
                })),
                Part::ToolResult(res) => {
                    let mut blocks = Vec::new();
                    for part in &res.content {
                        match part {
                            Part::Text(t) if !t.is_empty() => blocks.push(json!({ "text": t })),
                            Part::Text(_) => {}
                            // toolResult 的内容块里图片是一等公民，不用丢
                            Part::Image(Media::Base64 { mime, data }) => match image_format(mime) {
                                Some(format) => blocks.push(json!({
                                    "image": { "format": format, "source": { "bytes": data } },
                                })),
                                None => dropped.feature(Feature::ToolResultImage),
                            },
                            Part::Image(Media::Url(_)) => dropped.feature(Feature::MediaUrl),
                            _ => {}
                        }
                    }
                    if blocks.is_empty() {
                        blocks.push(json!({ "text": res.text() }));
                    }
                    content.push(json!({
                        "toolResult": {
                            "toolUseId": res.id,
                            "content": blocks,
                            "status": if res.is_error { "error" } else { "success" },
                        },
                    }));
                }
            }
        }
        if content.is_empty() {
            continue;
        }
        messages.push(json!({
            "role": if m.role == Role::User { "user" } else { "assistant" },
            "content": content,
        }));
    }
    out.insert("messages".into(), Value::Array(messages));

    // ── inferenceConfig：只有这四个 ────────────────────────────
    let mut cfg = Map::new();
    cfg.insert(
        "maxTokens".into(),
        json!(r.max_tokens.unwrap_or(t.default_max_tokens)),
    );
    if let Some(v) = r.temperature {
        cfg.insert("temperature".into(), json!(v));
    }
    if let Some(v) = r.top_p {
        cfg.insert("topP".into(), json!(v));
    }
    if !r.stop.is_empty() {
        cfg.insert("stopSequences".into(), json!(r.stop));
    }
    out.insert("inferenceConfig".into(), Value::Object(cfg));

    // ── additionalModelRequestFields：Converse 不解释的都塞这儿 ──
    let mut extra = Map::new();
    if let Some(k) = r.top_k {
        extra.insert("top_k".into(), json!(k));
    }
    if !extra.is_empty() {
        out.insert("additionalModelRequestFields".into(), Value::Object(extra));
    }

    if !r.tools.is_empty() {
        let tools: Vec<Value> = r
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
                let mut spec = json!({ "name": tool.name, "inputSchema": { "json": schema } });
                if let Some(desc) = &tool.description {
                    spec["description"] = json!(desc);
                }
                json!({ "toolSpec": spec })
            })
            .collect();

        let mut config = json!({ "tools": tools });
        match &r.tool_choice {
            Some(ToolChoice::Auto) => config["toolChoice"] = json!({ "auto": {} }),
            Some(ToolChoice::Required) => config["toolChoice"] = json!({ "any": {} }),
            Some(ToolChoice::Named(name)) => {
                config["toolChoice"] = json!({ "tool": { "name": name } })
            }
            // Converse 没有「一个都不要用」：不给 toolConfig 就是不用工具，
            // 但那样工具定义也跟着没了。**宁可让模型看见工具**
            Some(ToolChoice::None) => dropped.path("tool_choice"),
            None => {}
        }
        out.insert("toolConfig".into(), config);
    } else if r.tool_choice.is_some() {
        dropped.path("tool_choice");
    }

    // Converse 自己没有的旋钮
    if r.seed.is_some() {
        dropped.feature(Feature::Seed);
    }
    if r.presence_penalty.is_some() {
        dropped.feature(Feature::PresencePenalty);
    }
    if r.frequency_penalty.is_some() {
        dropped.feature(Feature::FrequencyPenalty);
    }
    if r.parallel_tool_calls.is_some() {
        dropped.feature(Feature::ParallelToolCalls);
    }
    if r.reasoning.is_some() {
        dropped.feature(Feature::Reasoning);
    }
    if r.format.is_some() {
        dropped.feature(Feature::Format);
    }

    Value::Object(out)
}

// ───────────────────────────────────────────── Converse → 中间表示

/// 一个内容块 → 中间表示的若干 [`Part`]。
fn part(b: &Value, dropped: &mut Dropped) -> Option<Part> {
    if let Some(t) = b.get("text").and_then(Value::as_str) {
        return Some(Part::Text(t.to_string()));
    }
    if let Some(img) = b.get("image") {
        let format = img.get("format").and_then(Value::as_str).unwrap_or("png");
        let source = img.get("source")?;
        if let Some(data) = source.get("bytes").and_then(Value::as_str) {
            return Some(Part::Image(Media::Base64 {
                mime: format!("image/{format}"),
                data: data.to_string(),
            }));
        }
        // S3 要拿 AWS 凭据去取，这一层没有，也不该有
        dropped.path("messages.content.image.source.s3Location");
        return None;
    }
    if let Some(doc) = b.get("document") {
        let source = doc.get("source")?;
        if let Some(data) = source.get("bytes").and_then(Value::as_str) {
            let format = doc.get("format").and_then(Value::as_str).unwrap_or("txt");
            return Some(Part::File {
                media: Media::Base64 {
                    mime: mime_of_document(format).to_string(),
                    data: data.to_string(),
                },
                name: doc.get("name").and_then(Value::as_str).map(str::to_string),
            });
        }
        if let Some(text) = source.get("text").and_then(Value::as_str) {
            return Some(Part::Text(text.to_string()));
        }
        dropped.path("messages.content.document.source");
        return None;
    }
    if let Some(r) = b.get("reasoningContent") {
        if let Some(rt) = r.get("reasoningText") {
            return Some(Part::Thinking(Thinking {
                text: rt
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                signature: rt
                    .get("signature")
                    .and_then(Value::as_str)
                    .and_then(|s| Signature::read(s, Vendor::Anthropic)),
            }));
        }
        if let Some(red) = r.get("redactedContent").and_then(Value::as_str) {
            return Some(Part::Thinking(Thinking {
                text: String::new(),
                signature: Some(Signature {
                    vendor: Vendor::Anthropic,
                    value: red.to_string(),
                    redacted: true,
                }),
            }));
        }
        return None;
    }
    if let Some(u) = b.get("toolUse") {
        return Some(Part::ToolCall(ToolCall {
            id: u
                .get("toolUseId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| new_id("tooluse_")),
            name: u
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            input: ToolInput::Json(u.get("input").cloned().unwrap_or_else(|| json!({}))),
        }));
    }
    if let Some(res) = b.get("toolResult") {
        let mut content = Vec::new();
        for c in res
            .get("content")
            .and_then(Value::as_array)
            .unwrap_or(&vec![])
        {
            if let Some(t) = c.get("text").and_then(Value::as_str) {
                content.push(Part::Text(t.to_string()));
            } else if let Some(j) = c.get("json") {
                content.push(Part::Text(j.to_string()));
            } else if let Some(img) = c.get("image")
                && let Some(data) = img
                    .get("source")
                    .and_then(|s| s.get("bytes"))
                    .and_then(Value::as_str)
            {
                let format = img.get("format").and_then(Value::as_str).unwrap_or("png");
                content.push(Part::Image(Media::Base64 {
                    mime: format!("image/{format}"),
                    data: data.to_string(),
                }));
            }
        }
        return Some(Part::ToolResult(ToolResult {
            id: res
                .get("toolUseId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            content,
            is_error: res.get("status").and_then(Value::as_str) == Some("error"),
        }));
    }
    // guardContent、cachePoint、citationsContent、video、audio、searchResult:
    // 别的格式没有对应物
    for key in [
        "guardContent",
        "cachePoint",
        "citationsContent",
        "video",
        "audio",
        "searchResult",
    ] {
        if b.get(key).is_some() {
            dropped.path(format!("messages.content.{key}"));
            return None;
        }
    }
    None
}

/// Converse 的 document `format` → MIME。
fn mime_of_document(format: &str) -> &'static str {
    match format {
        "pdf" => "application/pdf",
        "csv" => "text/csv",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "html" => "text/html",
        "md" => "text/markdown",
        _ => "text/plain",
    }
}

/// `model` 是从路径上取来的 —— Converse 的请求体里没有它。
pub fn decode_request(
    v: &Value,
    model: &str,
    stream: bool,
    dropped: &mut Dropped,
) -> Result<Request, Rejection> {
    let mut r = Request {
        model: model.to_string(),
        stream,
        ..Default::default()
    };

    for s in v.get("system").and_then(Value::as_array).unwrap_or(&vec![]) {
        if let Some(t) = s.get("text").and_then(Value::as_str) {
            r.system.push(t.to_string());
        } else if s.get("guardContent").is_some() {
            dropped.path("system.guardContent");
        } else if s.get("cachePoint").is_some() {
            dropped.path("system.cachePoint");
        }
    }

    for m in v
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&vec![])
    {
        let role = match m.get("role").and_then(Value::as_str) {
            Some("assistant") => Role::Assistant,
            _ => Role::User,
        };
        let parts: Vec<Part> = m
            .get("content")
            .and_then(Value::as_array)
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|b| part(b, dropped))
            .collect();
        if !parts.is_empty() {
            r.messages.push(Message { role, parts });
        }
    }

    if let Some(cfg) = v.get("inferenceConfig") {
        r.max_tokens = cfg.get("maxTokens").and_then(Value::as_u64);
        r.temperature = cfg.get("temperature").and_then(Value::as_f64);
        r.top_p = cfg.get("topP").and_then(Value::as_f64);
        if let Some(stops) = cfg.get("stopSequences").and_then(Value::as_array) {
            r.stop = stops
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
        }
    }

    // `top_k` 是我们自己编码时放进去的，读回来认它
    if let Some(k) = v
        .get("additionalModelRequestFields")
        .and_then(|e| e.get("top_k"))
        .and_then(Value::as_u64)
    {
        r.top_k = Some(k);
    }

    if let Some(cfg) = v.get("toolConfig") {
        for t in cfg
            .get("tools")
            .and_then(Value::as_array)
            .unwrap_or(&vec![])
        {
            let Some(spec) = t.get("toolSpec") else {
                for key in ["systemTool", "cachePoint"] {
                    if t.get(key).is_some() {
                        dropped.path(format!("toolConfig.tools.{key}"));
                    }
                }
                continue;
            };
            r.tools.push(Tool {
                name: spec
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                description: spec
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                kind: ToolKind::Function {
                    schema: spec
                        .get("inputSchema")
                        .and_then(|s| s.get("json"))
                        .cloned()
                        .unwrap_or_else(|| json!({})),
                    strict: spec.get("strict").and_then(Value::as_bool),
                },
            });
        }
        if let Some(choice) = cfg.get("toolChoice") {
            r.tool_choice = if choice.get("any").is_some() {
                Some(ToolChoice::Required)
            } else if let Some(t) = choice.get("tool") {
                t.get("name")
                    .and_then(Value::as_str)
                    .map(|n| ToolChoice::Named(n.to_string()))
            } else {
                Some(ToolChoice::Auto)
            };
        }
    }

    for key in ["guardrailConfig", "promptVariables", "outputConfig"] {
        if v.get(key).is_some() {
            dropped.path(key);
        }
    }

    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target {
            dialect: Dialect::Bedrock,
            official: true,
            default_max_tokens: 4096,
        }
    }

    fn encode(r: &Request) -> (Value, Vec<String>) {
        let mut d = Dropped::new(Dialect::Chat);
        let v = encode_request(r, &target(), &mut d);
        (v, d.into_vec())
    }

    fn user(parts: Vec<Part>) -> Request {
        Request {
            model: "m".into(),
            messages: vec![Message {
                role: Role::User,
                parts,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn an_image_becomes_bytes_under_the_format_converse_names() {
        let (v, dropped) = encode(&user(vec![Part::Image(Media::Base64 {
            mime: "image/jpeg".into(),
            data: "AAAA".into(),
        })]));
        let img = &v["messages"][0]["content"][0]["image"];
        // Converse 要短名，不是 MIME
        assert_eq!(img["format"], "jpeg");
        assert_eq!(img["source"]["bytes"], "AAAA");
        assert!(dropped.is_empty());
    }

    #[test]
    fn an_image_converse_has_no_name_for_is_dropped_rather_than_guessed() {
        // 猜一个格式发出去，模型收到的是一张解不开的图
        let (v, dropped) = encode(&user(vec![Part::Image(Media::Base64 {
            mime: "image/bmp".into(),
            data: "AAAA".into(),
        })]));
        assert!(v["messages"].as_array().unwrap().is_empty());
        assert_eq!(dropped, ["messages.content.image"]);
    }

    #[test]
    fn an_image_given_as_a_url_is_reported_because_converse_only_takes_bytes() {
        let (_, dropped) = encode(&user(vec![Part::Image(Media::Url(
            "https://example.com/a.png".into(),
        ))]));
        assert_eq!(dropped, ["messages.content.image_url"]);
    }

    #[test]
    fn a_file_carries_a_name_because_converse_requires_one() {
        let (v, _) = encode(&user(vec![Part::File {
            media: Media::Base64 {
                mime: "application/pdf".into(),
                data: "AAAA".into(),
            },
            name: None,
        }]));
        let doc = &v["messages"][0]["content"][0]["document"];
        assert_eq!(doc["format"], "pdf");
        assert!(
            doc["name"].as_str().is_some_and(|n| !n.is_empty()),
            "没名字的文档要替它起一个：{doc}"
        );
    }

    #[test]
    fn top_k_goes_into_the_passthrough_pocket_because_converse_has_no_field_for_it() {
        let r = Request {
            model: "m".into(),
            top_k: Some(200),
            ..Default::default()
        };
        let (v, dropped) = encode(&r);
        assert_eq!(v["additionalModelRequestFields"]["top_k"], 200);
        assert!(v["inferenceConfig"].get("topK").is_none());
        assert!(dropped.is_empty(), "它没有被丢掉：{dropped:?}");
    }

    #[test]
    fn max_tokens_is_always_written_because_converse_wants_a_number() {
        let (v, _) = encode(&Request {
            model: "m".into(),
            ..Default::default()
        });
        assert_eq!(v["inferenceConfig"]["maxTokens"], 4096);
    }

    #[test]
    fn the_three_tool_choices_converse_has() {
        let tool = Tool {
            name: "t".into(),
            description: None,
            kind: ToolKind::Function {
                schema: json!({}),
                strict: None,
            },
        };
        let with = |c: ToolChoice| {
            let r = Request {
                model: "m".into(),
                tools: vec![tool.clone()],
                tool_choice: Some(c),
                ..Default::default()
            };
            encode(&r)
        };
        assert!(with(ToolChoice::Auto).0["toolConfig"]["toolChoice"]["auto"].is_object());
        assert!(with(ToolChoice::Required).0["toolConfig"]["toolChoice"]["any"].is_object());
        assert_eq!(
            with(ToolChoice::Named("t".into())).0["toolConfig"]["toolChoice"]["tool"]["name"],
            "t"
        );
    }

    #[test]
    fn asking_for_no_tools_is_reported_because_converse_cannot_say_it() {
        // 不给 toolConfig 就是不用工具，但那样工具定义也跟着没了 ——
        // 宁可让模型看见工具，把这件事记下来
        let (v, dropped) = encode(&Request {
            model: "m".into(),
            tools: vec![Tool {
                name: "t".into(),
                description: None,
                kind: ToolKind::Function {
                    schema: json!({}),
                    strict: None,
                },
            }],
            tool_choice: Some(ToolChoice::None),
            ..Default::default()
        });
        assert!(
            v["toolConfig"]["tools"]
                .as_array()
                .is_some_and(|a| a.len() == 1)
        );
        assert_eq!(dropped, ["tool_choice"]);
    }

    #[test]
    fn a_thinking_block_keeps_its_signature_so_the_next_turn_is_accepted() {
        let (v, dropped) = encode(&user(vec![Part::Thinking(Thinking {
            text: "想".into(),
            signature: Some(Signature {
                vendor: Vendor::Anthropic,
                value: "sig".into(),
                redacted: false,
            }),
        })]));
        let rc = &v["messages"][0]["content"][0]["reasoningContent"];
        assert_eq!(rc["reasoningText"]["text"], "想");
        assert_eq!(rc["reasoningText"]["signature"], "sig");
        assert!(dropped.is_empty());
    }

    #[test]
    fn a_thinking_block_signed_by_someone_else_is_reported_not_forwarded() {
        // 别家的签名在 Bedrock 上验不过，带过去会被整个请求拒掉
        let (_, dropped) = encode(&user(vec![Part::Thinking(Thinking {
            text: "想".into(),
            signature: Some(Signature {
                vendor: Vendor::Google,
                value: "sig".into(),
                redacted: false,
            }),
        })]));
        assert_eq!(dropped, ["messages.reasoning_content"]);
    }

    #[test]
    fn a_tool_result_keeps_its_image_because_converse_takes_one() {
        let (v, dropped) = encode(&user(vec![Part::ToolResult(ToolResult {
            id: "tu_1".into(),
            content: vec![
                Part::Text("看这个".into()),
                Part::Image(Media::Base64 {
                    mime: "image/png".into(),
                    data: "AAAA".into(),
                }),
            ],
            is_error: false,
        })]));
        let res = &v["messages"][0]["content"][0]["toolResult"];
        assert_eq!(res["toolUseId"], "tu_1");
        assert_eq!(res["status"], "success");
        assert_eq!(res["content"][0]["text"], "看这个");
        assert_eq!(res["content"][1]["image"]["format"], "png");
        assert!(dropped.is_empty());
    }

    #[test]
    fn a_request_round_trips_through_converse() {
        let before = Request {
            model: "m".into(),
            system: vec!["你是助手".into()],
            messages: vec![Message {
                role: Role::User,
                parts: vec![Part::Text("你好".into())],
            }],
            max_tokens: Some(100),
            temperature: Some(0.5),
            top_p: Some(0.9),
            stop: vec!["END".into()],
            ..Default::default()
        };
        let (v, _) = encode(&before);
        let mut d = Dropped::new(Dialect::Bedrock);
        let after = decode_request(&v, "m", false, &mut d).unwrap();

        assert_eq!(after.system, before.system);
        assert_eq!(after.messages, before.messages);
        assert_eq!(after.max_tokens, before.max_tokens);
        assert_eq!(after.temperature, before.temperature);
        assert_eq!(after.top_p, before.top_p);
        assert_eq!(after.stop, before.stop);
    }
}
