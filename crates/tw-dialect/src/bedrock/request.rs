//! Bedrock Converse 请求 ⇄ 中间表示。
//!
//! 模型名在路径里（`/model/{id}/converse`），不在请求体里 —— 和 Gemini 一样。
//!
//! `inferenceConfig` 只有四个旋钮:`maxTokens`、`temperature`、`topP`、
//! `stopSequences`。**别的一概走 `additionalModelRequestFields`** —— 那是个
//! 原样透传给模型的口袋，Converse 自己不解释里面装的东西。`top_k` 就走它。

use serde_json::{Map, Value, json};

use crate::ir::*;
use crate::think;

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

/// 这个模型 id 是不是 Bedrock 上的 Claude：基础 id（`anthropic.claude-…`）、推理
/// 配置（`us.anthropic.claude-…`）都认。应用推理配置的 ARN 看不出背后是谁，不算。
fn is_claude(model: &str) -> bool {
    model.to_ascii_lowercase().contains("anthropic.claude")
}

/// 这个模型认不认提示缓存的断点。Bedrock 上只有 Claude 和 Nova 认；别的模型收到
/// `cachePoint` 会拒掉整个请求，所以只给认得的发。应用推理配置的 ARN 看不出背后是
/// 谁 —— 会把请求路由到 ARN 上的，基本是冲着 Claude 去的，照发。
fn caches(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("anthropic.claude") || m.contains("amazon.nova") || m.starts_with("arn:")
}

/// 一个缓存断点块。`ttl` 缺省是 5 分钟
fn cache_point(ttl: CacheTtl) -> Value {
    match ttl {
        CacheTtl::Short => json!({ "cachePoint": { "type": "default" } }),
        CacheTtl::Long => json!({ "cachePoint": { "type": "default", "ttl": "1h" } }),
    }
}

pub fn encode_request(r: &Request, t: &Target, dropped: &mut Dropped) -> Value {
    let mut out = Map::new();
    // 断点放不放：模型不认就一个都不放，报出来
    let cache: &[CachePoint] = if caches(&r.model) { &r.cache } else { &[] };
    if cache.is_empty() && !r.cache.is_empty() {
        dropped.feature(Feature::Cache);
    }

    if !r.system.is_empty() {
        let mut blocks = Vec::new();
        for (i, s) in r.system.iter().enumerate() {
            blocks.push(json!({ "text": s }));
            if let Some(ttl) = CacheAfter::find(cache, CacheAfter::System(i)) {
                blocks.push(cache_point(ttl));
            }
        }
        out.insert("system".into(), Value::Array(blocks));
    }

    // 先一条一条转，断点跟在它那一块后面；再把相邻的同一方合成一条 —— Converse
    // 要求一问一答交替。先合再转的话，断点就找不到它原来的位置了
    let mut turns: Vec<(Role, Vec<Value>)> = Vec::new();
    let mut doc_index = 0usize;
    for (i, m) in r.messages.iter().enumerate() {
        let mut content = Vec::new();
        for (j, p) in m.parts.iter().enumerate() {
            let before = content.len();
            content_block(p, &mut content, &mut doc_index, dropped);
            if content.len() > before
                && let Some(ttl) = CacheAfter::find(
                    cache,
                    CacheAfter::Part {
                        message: i,
                        part: j,
                    },
                )
            {
                content.push(cache_point(ttl));
            }
        }
        if content.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some((role, c)) if *role == m.role => c.extend(content),
            _ => turns.push((m.role, content)),
        }
    }
    let messages: Vec<Value> = turns
        .into_iter()
        .map(|(role, content)| {
            json!({
                "role": if role == Role::User { "user" } else { "assistant" },
                "content": content,
            })
        })
        .collect();
    out.insert("messages".into(), Value::Array(messages));

    // ── 推理：Claude 的放进 additionalModelRequestFields，写法和 Anthropic 一样 ──
    let mut extra = Map::new();
    let max_tokens = r.max_tokens.unwrap_or(t.default_max_tokens);
    let thinking = reasoning(r, max_tokens, dropped, &mut extra);

    // ── inferenceConfig：只有这四个 ────────────────────────────
    let mut cfg = Map::new();
    cfg.insert("maxTokens".into(), json!(max_tokens));
    sampling(r, thinking, dropped, &mut cfg, &mut extra);
    if !r.stop.is_empty() {
        cfg.insert("stopSequences".into(), json!(r.stop));
    }
    out.insert("inferenceConfig".into(), Value::Object(cfg));

    // ── additionalModelRequestFields：Converse 不解释的都塞这儿 ──
    if !extra.is_empty() {
        out.insert("additionalModelRequestFields".into(), Value::Object(extra));
    }

    if !r.tools.is_empty() {
        let mut tools = Vec::new();
        for (i, tool) in r.tools.iter().enumerate() {
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
            tools.push(json!({ "toolSpec": spec }));
            if let Some(ttl) = CacheAfter::find(cache, CacheAfter::Tool(i)) {
                tools.push(cache_point(ttl));
            }
        }

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
    if r.format.is_some() {
        dropped.feature(Feature::Format);
    }
    if r.verbosity.is_some() {
        dropped.feature(Feature::Verbosity);
    }

    Value::Object(out)
}

/// 一块内容转成 Converse 的内容块，追加在 `content` 后面。转不过去的记进 `dropped`。
fn content_block(p: &Part, content: &mut Vec<Value>, doc_index: &mut usize, dropped: &mut Dropped) {
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
                            "name": document_name(name.as_deref(), *doc_index),
                            "source": { "bytes": data },
                        },
                    }));
                    *doc_index += 1;
                }
                None => dropped.feature(Feature::File),
            },
            Media::Url(_) => dropped.feature(Feature::MediaUrl),
        },
        Part::Thinking(th) => match &th.signature {
            // 签名是 Bedrock 上的 Anthropic 模型签的，带回去才算数
            Some(s) if s.vendor == Vendor::Anthropic && !s.redacted => content.push(json!({
                "reasoningContent": {
                    "reasoningText": { "text": th.text, "signature": s.value },
                },
            })),
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

/// 推理配置。返回思考有没有开 —— 开了之后采样参数另有限制。
///
/// **只有 Claude 有**，写法和 Anthropic 的 Messages 一样，放在
/// `additionalModelRequestFields` 里原样交给模型：4.6 及以后是 `adaptive` 加
/// `output_config.effort`，之前是 `enabled` 加 `budget_tokens`（见
/// [`crate::think::claude_adaptive`]）。别的模型、看不出背后是谁的 ARN，不开，报出来。
fn reasoning(
    r: &Request,
    max_tokens: u64,
    dropped: &mut Dropped,
    extra: &mut Map<String, Value>,
) -> bool {
    let Some(re) = &r.reasoning else {
        return false;
    };
    // 关掉：不写就是不开
    if !re.enabled {
        return false;
    }
    if !is_claude(&r.model) {
        dropped.feature(Feature::Reasoning);
        return false;
    }
    if think::claude_adaptive(&r.model) {
        extra.insert("thinking".into(), json!({ "type": "adaptive" }));
        if let Some(e) = think::effort(re) {
            extra.insert(
                "output_config".into(),
                json!({ "effort": think::anthropic(e) }),
            );
        }
        return true;
    }
    // 按预算：不少于 1024，且必须小于 maxTokens；工具调用那一轮得以思考块开头
    let budget = think::budget(re)
        .unwrap_or(think::budget_of_effort(Effort::Medium))
        .min(max_tokens.saturating_sub(1));
    if budget < 1024 || think::continues_tool_turn_without_thinking(&r.messages) {
        dropped.feature(Feature::Reasoning);
        return false;
    }
    extra.insert(
        "thinking".into(),
        json!({ "type": "enabled", "budget_tokens": budget }),
    );
    true
}

/// 采样参数。
///
/// Claude 在 Bedrock 上的限制和在 Anthropic 上一样：开着思考时 `temperature` 只能是
/// 1、不许 `top_k`、`top_p` 不低于 0.95；Sonnet 4.5、Haiku 4.5 这些新模型
/// `temperature` 和 `top_p` 只能二选一，两个都给就留 `temperature`。`top_k` 在
/// `inferenceConfig` 里没有位置，走 `additionalModelRequestFields`。
fn sampling(
    r: &Request,
    thinking: bool,
    dropped: &mut Dropped,
    cfg: &mut Map<String, Value>,
    extra: &mut Map<String, Value>,
) {
    let claude = is_claude(&r.model);
    if let Some(v) = r.temperature {
        if thinking && v != 1.0 {
            dropped.feature(Feature::Temperature);
        } else {
            cfg.insert("temperature".into(), json!(v));
        }
    }
    if let Some(v) = r.top_p {
        let both = claude && cfg.contains_key("temperature");
        if both || (thinking && v < 0.95) {
            dropped.feature(Feature::TopP);
        } else {
            cfg.insert("topP".into(), json!(v));
        }
    }
    if let Some(k) = r.top_k {
        if thinking {
            dropped.feature(Feature::TopK);
        } else {
            extra.insert("top_k".into(), json!(k));
        }
    }
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
    // guardContent、citationsContent、video、audio、searchResult：别的格式没有
    // 对应物。cachePoint 由调用方记成断点
    for key in [
        "guardContent",
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

/// 一个 `cachePoint` 块的 TTL。不是断点块就是 None
fn cache_point_ttl(b: &Value) -> Option<CacheTtl> {
    let c = b.get("cachePoint")?;
    Some(match c.get("ttl").and_then(Value::as_str) {
        Some("1h") => CacheTtl::Long,
        _ => CacheTtl::Short,
    })
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
        } else if let Some(ttl) = cache_point_ttl(s)
            && let Some(last) = r.system.len().checked_sub(1)
        {
            r.cache.push(CachePoint {
                after: CacheAfter::System(last),
                ttl,
            });
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
        let mut parts: Vec<Part> = Vec::new();
        for b in m
            .get("content")
            .and_then(Value::as_array)
            .unwrap_or(&vec![])
        {
            if let Some(ttl) = cache_point_ttl(b) {
                if let Some(last) = parts.len().checked_sub(1) {
                    r.cache.push(CachePoint {
                        after: CacheAfter::Part {
                            message: r.messages.len(),
                            part: last,
                        },
                        ttl,
                    });
                }
            } else if let Some(p) = part(b, dropped) {
                parts.push(p);
            }
        }
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

    // `top_k` 和 Claude 的思考配置是我们自己编码时放进去的，读回来认它们
    let extra = v.get("additionalModelRequestFields");
    if let Some(k) = extra.and_then(|e| e.get("top_k")).and_then(Value::as_u64) {
        r.top_k = Some(k);
    }
    let effort = extra
        .and_then(|e| e.get("output_config"))
        .and_then(|o| o.get("effort"))
        .and_then(Value::as_str)
        .and_then(think::parse_anthropic);
    r.reasoning = match extra
        .and_then(|e| e.get("thinking"))
        .and_then(|t| t.get("type"))
        .and_then(Value::as_str)
    {
        Some("enabled") => Some(Reasoning {
            enabled: true,
            effort,
            budget: extra
                .and_then(|e| e.pointer("/thinking/budget_tokens"))
                .and_then(Value::as_u64),
            summary: true,
        }),
        Some("adaptive") => Some(Reasoning {
            enabled: true,
            effort,
            budget: None,
            summary: true,
        }),
        Some("disabled") => Some(Reasoning {
            enabled: false,
            effort: None,
            budget: None,
            summary: false,
        }),
        _ => None,
    };

    if let Some(cfg) = v.get("toolConfig") {
        for t in cfg
            .get("tools")
            .and_then(Value::as_array)
            .unwrap_or(&vec![])
        {
            if let Some(ttl) = cache_point_ttl(t) {
                if let Some(last) = r.tools.len().checked_sub(1) {
                    r.cache.push(CachePoint {
                        after: CacheAfter::Tool(last),
                        ttl,
                    });
                }
                continue;
            }
            let Some(spec) = t.get("toolSpec") else {
                if t.get("systemTool").is_some() {
                    dropped.path("toolConfig.tools.systemTool");
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

    /// Claude Code 那样的请求：系统提示第二段、最后一个工具、最后一条用户消息标了断点，
    /// 其中一个是 1 小时
    fn claude_code_like(model: &str) -> Request {
        let mut d = Dropped::new(Dialect::Anthropic);
        let mut r = crate::anthropic::decode_request(
            &json!({
                "model": model,
                "max_tokens": 32000,
                "system": [
                    {"type": "text", "text": "You are Claude Code."},
                    {"type": "text", "text": "Project rules.", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
                ],
                "tools": [
                    {"name": "Read", "input_schema": {"type": "object"}},
                    {"name": "Edit", "input_schema": {"type": "object"}, "cache_control": {"type": "ephemeral"}}
                ],
                "messages": [
                    {"role": "user", "content": [
                        {"type": "text", "text": "读一下 a.rs"},
                        {"type": "text", "text": "谢谢", "cache_control": {"type": "ephemeral"}}
                    ]}
                ]
            }),
            &mut d,
        )
        .unwrap();
        r.model = model.into();
        r
    }

    fn with_dropped(r: &Request) -> (Value, Vec<String>) {
        let mut d = Dropped::new(Dialect::Anthropic);
        let v = encode_request(r, &target(), &mut d);
        (v, d.into_vec())
    }

    #[test]
    fn cache_points_follow_the_blocks_the_client_marked() {
        let r = claude_code_like("us.anthropic.claude-sonnet-4-5-20250929-v1:0");
        assert_eq!(r.cache.len(), 3);
        let (v, dropped) = with_dropped(&r);
        assert!(dropped.is_empty(), "{dropped:?}");

        assert_eq!(
            v["system"],
            json!([
                {"text": "You are Claude Code."},
                {"text": "Project rules."},
                {"cachePoint": {"type": "default", "ttl": "1h"}}
            ])
        );
        let tools = v["toolConfig"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[2], json!({"cachePoint": {"type": "default"}}));
        assert_eq!(
            v["messages"][0]["content"],
            json!([
                {"text": "读一下 a.rs"},
                {"text": "谢谢"},
                {"cachePoint": {"type": "default"}}
            ])
        );
    }

    #[test]
    fn a_cache_point_stays_after_its_block_when_turns_are_merged() {
        // 两条相邻的用户消息合成一条；断点标在第一条上，就还跟在它后面，不跑到末尾
        let r = Request {
            model: "anthropic.claude-3-7-sonnet-20250219-v1:0".into(),
            messages: vec![
                Message {
                    role: Role::User,
                    parts: vec![Part::Text("前面".into())],
                },
                Message {
                    role: Role::User,
                    parts: vec![Part::Text("后面".into())],
                },
            ],
            cache: vec![CachePoint {
                after: CacheAfter::Part {
                    message: 0,
                    part: 0,
                },
                ttl: CacheTtl::Short,
            }],
            ..Default::default()
        };
        let (v, _) = with_dropped(&r);
        assert_eq!(
            v["messages"],
            json!([{"role": "user", "content": [
                {"text": "前面"},
                {"cachePoint": {"type": "default"}},
                {"text": "后面"}
            ]}])
        );
    }

    #[test]
    fn a_model_that_does_not_cache_gets_no_cache_points_and_it_is_reported() {
        // 不认 cachePoint 的模型会拒掉整个请求
        let r = claude_code_like("meta.llama3-70b-instruct-v1:0");
        let (v, dropped) = with_dropped(&r);
        assert!(!v.to_string().contains("cachePoint"), "{v}");
        assert_eq!(dropped, ["cache_control"]);
    }

    #[test]
    fn nova_and_an_application_profile_arn_get_cache_points() {
        for model in [
            "us.amazon.nova-pro-v1:0",
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/a1b2",
        ] {
            let (v, dropped) = with_dropped(&claude_code_like(model));
            assert!(v.to_string().contains("cachePoint"), "{model}");
            assert!(dropped.is_empty(), "{model}: {dropped:?}");
        }
    }

    fn thinking(model: &str, effort: Option<Effort>, budget: Option<u64>) -> Request {
        Request {
            model: model.into(),
            max_tokens: Some(32000),
            messages: vec![Message {
                role: Role::User,
                parts: vec![Part::Text("想一想".into())],
            }],
            reasoning: Some(Reasoning {
                enabled: true,
                effort,
                budget,
                summary: true,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn claude_4_5_thinks_on_a_budget_and_4_6_adaptively() {
        let (v, dropped) = with_dropped(&thinking(
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            None,
            Some(10000),
        ));
        assert!(dropped.is_empty(), "{dropped:?}");
        assert_eq!(
            v["additionalModelRequestFields"],
            json!({"thinking": {"type": "enabled", "budget_tokens": 10000}})
        );

        let (v, dropped) = with_dropped(&thinking(
            "global.anthropic.claude-opus-4-6-v1",
            Some(Effort::High),
            None,
        ));
        assert!(dropped.is_empty(), "{dropped:?}");
        assert_eq!(
            v["additionalModelRequestFields"],
            json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "high"}})
        );
    }

    #[test]
    fn a_budget_stays_below_max_tokens() {
        let mut r = thinking(
            "anthropic.claude-3-7-sonnet-20250219-v1:0",
            None,
            Some(50000),
        );
        r.max_tokens = Some(8000);
        let (v, _) = with_dropped(&r);
        assert_eq!(
            v["additionalModelRequestFields"]["thinking"]["budget_tokens"],
            7999
        );
    }

    #[test]
    fn a_model_that_is_not_claude_does_not_think_and_it_is_reported() {
        let (v, dropped) = with_dropped(&thinking(
            "us.meta.llama4-maverick-17b-instruct-v1:0",
            Some(Effort::High),
            None,
        ));
        assert!(v.get("additionalModelRequestFields").is_none(), "{v}");
        assert_eq!(dropped, ["thinking"]);
    }

    #[test]
    fn budget_thinking_is_off_for_a_tool_turn_that_started_without_it() {
        // 工具调用那一轮来自别家，没有签过名的思考块：开着思考发过去是 400
        let mut r = thinking(
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            None,
            Some(4000),
        );
        r.messages = vec![
            Message {
                role: Role::User,
                parts: vec![Part::Text("读 a.rs".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![Part::ToolCall(ToolCall {
                    id: "tu_1".into(),
                    name: "Read".into(),
                    input: ToolInput::Json(json!({"path": "a.rs"})),
                })],
            },
            Message {
                role: Role::User,
                parts: vec![Part::ToolResult(ToolResult {
                    id: "tu_1".into(),
                    content: vec![Part::Text("fn main() {}".into())],
                    is_error: false,
                })],
            },
        ];
        let (v, dropped) = with_dropped(&r);
        assert!(v.get("additionalModelRequestFields").is_none(), "{v}");
        assert_eq!(dropped, ["thinking"]);
    }

    #[test]
    fn thinking_takes_away_what_claude_refuses_alongside_it() {
        let mut r = thinking(
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            None,
            Some(4000),
        );
        r.temperature = Some(0.5);
        r.top_p = Some(0.5);
        r.top_k = Some(40);
        let (v, dropped) = with_dropped(&r);
        let cfg = &v["inferenceConfig"];
        assert!(
            cfg.get("temperature").is_none() && cfg.get("topP").is_none(),
            "{cfg}"
        );
        assert!(v["additionalModelRequestFields"].get("top_k").is_none());
        assert_eq!(dropped, ["temperature", "top_p", "top_k"]);
    }

    #[test]
    fn claude_gets_temperature_or_top_p_but_not_both() {
        let both = |model: &str| Request {
            model: model.into(),
            temperature: Some(0.3),
            top_p: Some(0.8),
            ..Default::default()
        };
        let (v, dropped) = with_dropped(&both("us.anthropic.claude-haiku-4-5-20251001-v1:0"));
        assert_eq!(v["inferenceConfig"]["temperature"], 0.3);
        assert!(v["inferenceConfig"].get("topP").is_none());
        assert_eq!(dropped, ["top_p"]);

        // 别的模型两个都认
        let (v, dropped) = with_dropped(&both("meta.llama3-70b-instruct-v1:0"));
        assert_eq!(v["inferenceConfig"]["topP"], 0.8);
        assert!(dropped.is_empty());
    }

    #[test]
    fn cache_points_and_thinking_come_back_from_converse() {
        let mut before = claude_code_like("us.anthropic.claude-opus-4-6-v1");
        before.reasoning = Some(Reasoning {
            enabled: true,
            effort: Some(Effort::High),
            budget: None,
            summary: true,
        });
        let (v, _) = with_dropped(&before);
        let mut d = Dropped::new(Dialect::Bedrock);
        let after = decode_request(&v, &before.model, false, &mut d).unwrap();
        assert!(d.is_empty(), "{:?}", d.into_vec());
        assert_eq!(after.cache, before.cache);
        assert_eq!(after.reasoning, before.reasoning);
    }
}
