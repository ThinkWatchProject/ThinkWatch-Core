//! Anthropic Messages 请求 ⇄ 中间表示。

use serde_json::{Map, Value, json};

use crate::ir::*;
use crate::think;

/// Anthropic 收的图片格式
const IMAGE_MIMES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];

// ───────────────────────────────────────────────────────── 解码

/// 客户端发来的 Anthropic 请求 → 中间表示。
pub fn decode_request(v: &Value, dropped: &mut Dropped) -> Result<Request, Rejection> {
    if !v.is_object() {
        return Err(Rejection("The request body is not a JSON object.".into()));
    }
    let mut r = Request {
        model: str_of(v, "model").unwrap_or_default().to_string(),
        max_tokens: u64_of(v, "max_tokens"),
        temperature: f64_of(v, "temperature"),
        top_p: f64_of(v, "top_p"),
        top_k: u64_of(v, "top_k"),
        stream: v.get("stream").and_then(Value::as_bool).unwrap_or(false),
        ..Default::default()
    };

    match v.get("system") {
        Some(Value::String(s)) if !s.is_empty() => r.system.push(s.clone()),
        Some(Value::Array(blocks)) => r.system.extend(
            blocks
                .iter()
                .filter_map(|b| str_of(b, "text"))
                .filter(|t| !t.is_empty())
                .map(str::to_string),
        ),
        _ => {}
    }

    for m in arr_of(v, "messages") {
        let role = match str_of(m, "role") {
            Some("assistant") => Role::Assistant,
            // 消息里的 system 角色：并进系统提示，位置信息丢失但内容保留
            Some("system") => {
                let t = text_of(m.get("content").unwrap_or(&Value::Null));
                if !t.is_empty() {
                    r.system.push(t);
                }
                continue;
            }
            _ => Role::User,
        };
        let parts = match m.get("content") {
            Some(Value::String(s)) if !s.is_empty() => vec![Part::Text(s.clone())],
            Some(Value::Array(blocks)) => blocks.iter().filter_map(|b| block(b, dropped)).collect(),
            _ => Vec::new(),
        };
        r.messages.push(Message { role, parts });
    }

    for t in arr_of(v, "tools") {
        match str_of(t, "type") {
            None | Some("custom") => r.tools.push(Tool {
                name: str_of(t, "name").unwrap_or_default().to_string(),
                description: str_of(t, "description").map(str::to_string),
                kind: ToolKind::Function {
                    schema: t
                        .get("input_schema")
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object" })),
                    strict: t.get("strict").and_then(Value::as_bool),
                },
            }),
            // 服务端工具（web_search_20250305、code_execution_… 这些）只有 Anthropic 能执行
            Some(other) => dropped.path(format!("tools.{other}")),
        }
    }

    if let Some(tc) = v.get("tool_choice") {
        r.tool_choice = match str_of(tc, "type") {
            Some("auto") => Some(ToolChoice::Auto),
            Some("any") => Some(ToolChoice::Required),
            Some("none") => Some(ToolChoice::None),
            Some("tool") => str_of(tc, "name").map(|n| ToolChoice::Named(n.to_string())),
            _ => None,
        };
        if let Some(d) = tc.get("disable_parallel_tool_use").and_then(Value::as_bool) {
            r.parallel_tool_calls = Some(!d);
        }
    }

    r.stop = arr_of(v, "stop_sequences")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();

    let output = v.get("output_config");
    let effort = output
        .and_then(|o| str_of(o, "effort"))
        .and_then(think::parse_anthropic);
    let summary = |t: &Value| str_of(t, "display") != Some("omitted");
    r.reasoning = match v.get("thinking") {
        Some(t) => match str_of(t, "type") {
            Some("enabled") => Some(Reasoning {
                enabled: true,
                effort,
                budget: u64_of(t, "budget_tokens"),
                summary: summary(t),
            }),
            Some("adaptive") => Some(Reasoning {
                enabled: true,
                effort,
                budget: None,
                summary: summary(t),
            }),
            Some("disabled") => Some(Reasoning {
                enabled: false,
                effort: None,
                budget: None,
                summary: false,
            }),
            _ => None,
        },
        None => effort.map(|e| Reasoning {
            enabled: true,
            effort: Some(e),
            budget: None,
            summary: false,
        }),
    };
    if let Some(f) = output.and_then(|o| o.get("format"))
        && str_of(f, "type") == Some("json_schema")
    {
        r.format = Some(Format::JsonSchema {
            name: None,
            schema: f.get("schema").cloned().unwrap_or(Value::Null),
            strict: None,
        });
    }

    for k in ["container", "mcp_servers", "context_management"] {
        if has(v, k) {
            dropped.path(k);
        }
    }
    Ok(r)
}

fn block(b: &Value, dropped: &mut Dropped) -> Option<Part> {
    match str_of(b, "type").unwrap_or("") {
        "text" => str_of(b, "text")
            .filter(|t| !t.is_empty())
            .map(|t| Part::Text(t.to_string())),
        "image" => image_source(b.get("source")?, dropped).map(Part::Image),
        "document" => document(b, dropped),
        "search_result" => {
            let title = str_of(b, "title").unwrap_or_default();
            let source = str_of(b, "source").unwrap_or_default();
            let body = text_of(b.get("content").unwrap_or(&Value::Null));
            Some(Part::Text(format!("{title}\n{source}\n{body}")))
        }
        "thinking" => Some(Part::Thinking(Thinking {
            text: str_of(b, "thinking").unwrap_or_default().to_string(),
            signature: str_of(b, "signature").and_then(|s| Signature::read(s, Vendor::Anthropic)),
        })),
        "redacted_thinking" => Some(Part::Thinking(Thinking {
            text: String::new(),
            signature: Some(Signature {
                vendor: Vendor::Anthropic,
                value: str_of(b, "data").unwrap_or_default().to_string(),
                redacted: true,
            }),
        })),
        "tool_use" => Some(Part::ToolCall(ToolCall {
            id: str_of(b, "id").unwrap_or_default().to_string(),
            name: str_of(b, "name").unwrap_or_default().to_string(),
            input: ToolInput::Json(b.get("input").cloned().unwrap_or_else(|| json!({}))),
        })),
        "tool_result" => Some(Part::ToolResult(ToolResult {
            id: str_of(b, "tool_use_id").unwrap_or_default().to_string(),
            content: result_content(b.get("content"), dropped),
            is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
        })),
        // server_tool_use、web_search_tool_result 之类：服务端工具的调用和结果
        other => {
            dropped.path(format!("messages.content.{other}"));
            None
        }
    }
}

fn image_source(src: &Value, dropped: &mut Dropped) -> Option<Media> {
    match str_of(src, "type") {
        Some("base64") => Some(Media::Base64 {
            mime: str_of(src, "media_type").unwrap_or("image/png").to_string(),
            data: str_of(src, "data").unwrap_or_default().to_string(),
        }),
        Some("url") => Some(Media::Url(str_of(src, "url")?.to_string())),
        // `file`：Anthropic Files API 里的文件，别家拿不到
        other => {
            dropped.path(format!(
                "messages.content.image.source.{}",
                other.unwrap_or("unknown")
            ));
            None
        }
    }
}

fn document(b: &Value, dropped: &mut Dropped) -> Option<Part> {
    let src = b.get("source")?;
    let name = str_of(b, "title").map(str::to_string);
    match str_of(src, "type") {
        Some("base64") => Some(Part::File {
            media: Media::Base64 {
                mime: str_of(src, "media_type")
                    .unwrap_or("application/pdf")
                    .to_string(),
                data: str_of(src, "data").unwrap_or_default().to_string(),
            },
            name,
        }),
        Some("url") => Some(Part::File {
            media: Media::Url(str_of(src, "url")?.to_string()),
            name,
        }),
        Some("text") => str_of(src, "data").map(|t| Part::Text(t.to_string())),
        Some("content") => Some(Part::Text(text_of(src.get("content")?))),
        other => {
            dropped.path(format!(
                "messages.content.document.source.{}",
                other.unwrap_or("unknown")
            ));
            None
        }
    }
}

fn result_content(c: Option<&Value>, dropped: &mut Dropped) -> Vec<Part> {
    match c {
        Some(Value::String(s)) if !s.is_empty() => vec![Part::Text(s.clone())],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| match str_of(i, "type").unwrap_or("") {
                "text" => str_of(i, "text").map(|t| Part::Text(t.to_string())),
                "image" => image_source(i.get("source")?, dropped).map(Part::Image),
                "document" | "search_result" => match block(i, dropped)? {
                    p @ Part::Text(_) => Some(p),
                    _ => {
                        dropped.path("messages.content.tool_result.content.document");
                        None
                    }
                },
                other => {
                    dropped.path(format!("messages.content.tool_result.content.{other}"));
                    None
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

// ───────────────────────────────────────────────────────── 编码

/// 中间表示 → 发给 Anthropic 上游的请求。
pub fn encode_request(r: &Request, t: &Target, dropped: &mut Dropped) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), json!(r.model));
    let max_tokens = r.max_tokens.unwrap_or(t.default_max_tokens);
    out.insert("max_tokens".into(), json!(max_tokens));
    if !r.system.is_empty() {
        out.insert(
            "system".into(),
            Value::Array(
                r.system
                    .iter()
                    .map(|s| json!({ "type": "text", "text": s }))
                    .collect(),
            ),
        );
    }
    out.insert(
        "messages".into(),
        Value::Array(messages(&r.messages, dropped)),
    );

    if !r.tools.is_empty() {
        let tools = r
            .tools
            .iter()
            .map(|tool| {
                let (schema, strict) = match &tool.kind {
                    ToolKind::Function { schema, strict } => (object_schema(schema), *strict),
                    ToolKind::Freeform { format } => {
                        if format.is_some() {
                            dropped.feature(Feature::FreeformFormat);
                        }
                        (freeform_schema(), None)
                    }
                };
                let mut o = json!({ "name": tool.name, "input_schema": schema });
                if let Some(d) = &tool.description {
                    o["description"] = json!(d);
                }
                if strict == Some(true) {
                    o["strict"] = json!(true);
                }
                o
            })
            .collect();
        out.insert("tools".into(), Value::Array(tools));
    }

    let serial = r.parallel_tool_calls == Some(false);
    let choice = match (&r.tool_choice, serial) {
        (Some(ToolChoice::None), _) => Some(json!({ "type": "none" })),
        (Some(ToolChoice::Auto), _) | (None, true) => Some(json!({ "type": "auto" })),
        (Some(ToolChoice::Required), _) => Some(json!({ "type": "any" })),
        (Some(ToolChoice::Named(n)), _) => Some(json!({ "type": "tool", "name": n })),
        (None, false) => None,
    };
    if let Some(mut c) = choice
        && !r.tools.is_empty()
    {
        if serial && c["type"] != "none" {
            c["disable_parallel_tool_use"] = json!(true);
        }
        out.insert("tool_choice".into(), c);
    }

    if !r.stop.is_empty() {
        out.insert("stop_sequences".into(), json!(r.stop));
    }

    let thinking = reasoning(r, max_tokens, t, dropped, &mut out);
    sampling(r, t, thinking, dropped, &mut out);

    match &r.format {
        Some(Format::JsonSchema { schema, .. }) => {
            let config = out.entry("output_config").or_insert_with(|| json!({}));
            config["format"] = json!({ "type": "json_schema", "schema": schema });
        }
        Some(Format::JsonObject) => dropped.feature(Feature::Format),
        None => {}
    }
    for (present, f) in [
        (r.seed.is_some(), Feature::Seed),
        (r.presence_penalty.is_some(), Feature::PresencePenalty),
        (r.frequency_penalty.is_some(), Feature::FrequencyPenalty),
    ] {
        if present {
            dropped.feature(f);
        }
    }
    if r.stream {
        out.insert("stream".into(), json!(true));
    }
    Value::Object(out)
}

/// 思考配置。返回思考有没有开 —— 开了之后采样参数另有限制。
fn reasoning(
    r: &Request,
    max_tokens: u64,
    t: &Target,
    dropped: &mut Dropped,
    out: &mut Map<String, Value>,
) -> bool {
    let Some(re) = &r.reasoning else {
        return false;
    };
    if !re.enabled {
        out.insert("thinking".into(), json!({ "type": "disabled" }));
        return false;
    }
    if think::claude_adaptive(&r.model) {
        let mut thinking = json!({ "type": "adaptive" });
        if !re.summary {
            thinking["display"] = json!("omitted");
        }
        out.insert("thinking".into(), thinking);
        if let Some(e) = think::effort(re) {
            out.insert(
                "output_config".into(),
                json!({ "effort": think::anthropic(e) }),
            );
        }
        return true;
    }
    // 手动模式：预算不少于 1024，且必须小于 max_tokens
    let budget = think::budget(re)
        .unwrap_or(think::budget_of_effort(Effort::Medium))
        .min(max_tokens.saturating_sub(1));
    // 手动模式还要求工具调用那一轮以思考块开头。那一轮来自别家时没有 Anthropic 的
    // 思考块，开着思考发过去是 400
    if budget < 1024 || continues_tool_turn_without_thinking(&r.messages) {
        dropped.feature(Feature::Reasoning);
        return false;
    }
    let mut thinking = json!({ "type": "enabled", "budget_tokens": budget });
    if t.official && !re.summary {
        thinking["display"] = json!("omitted");
    }
    out.insert("thinking".into(), thinking);
    true
}

/// 最后一条是工具结果，而发起调用的那一轮没有 Anthropic 签发的思考块
fn continues_tool_turn_without_thinking(messages: &[Message]) -> bool {
    let merged = merge_roles(messages.to_vec());
    let [.., assistant, user] = merged.as_slice() else {
        return false;
    };
    if assistant.role != Role::Assistant
        || !user.parts.iter().any(|p| matches!(p, Part::ToolResult(_)))
        || !assistant
            .parts
            .iter()
            .any(|p| matches!(p, Part::ToolCall(_)))
    {
        return false;
    }
    !matches!(
        assistant.parts.first(),
        Some(Part::Thinking(Thinking { signature: Some(s), .. })) if s.vendor == Vendor::Anthropic
    )
}

fn sampling(
    r: &Request,
    t: &Target,
    thinking: bool,
    dropped: &mut Dropped,
    out: &mut Map<String, Value>,
) {
    // 官方端点已经不接受这几个参数（temperature 只剩 1.0）；开着思考时兼容实现也
    // 要求 temperature 为 1、不许 top_k、top_p 不低于 0.95
    let strict = t.official || thinking;
    if let Some(x) = r.temperature {
        if strict && x != 1.0 {
            dropped.feature(Feature::Temperature);
        } else if !t.official {
            out.insert("temperature".into(), json!(x.clamp(0.0, 1.0)));
        }
    }
    if let Some(x) = r.top_p {
        if t.official || (thinking && x < 0.95) {
            if x < 1.0 {
                dropped.feature(Feature::TopP);
            }
        } else {
            out.insert("top_p".into(), json!(x));
        }
    }
    if let Some(x) = r.top_k {
        if strict {
            dropped.feature(Feature::TopK);
        } else {
            out.insert("top_k".into(), json!(x));
        }
    }
}

fn messages(msgs: &[Message], dropped: &mut Dropped) -> Vec<Value> {
    // 每条消息拆成「工具结果」和「其他」两组：Anthropic 要求工具结果排在 user 消息
    // 最前面，合并相邻消息时也要保持这一点
    let mut out: Vec<(Role, Vec<Value>, Vec<Value>)> = Vec::new();
    for m in msgs {
        let mut results = Vec::new();
        let mut others = Vec::new();
        for p in &m.parts {
            match p {
                Part::Text(s) if !s.is_empty() => others.push(json!({ "type": "text", "text": s })),
                Part::Text(_) => {}
                Part::Image(media) => {
                    if let Some(b) = image(media, dropped) {
                        others.push(b);
                    }
                }
                Part::File { media, name } => {
                    let source = match media {
                        Media::Base64 { mime, data } if mime == "application/pdf" => {
                            json!({ "type": "base64", "media_type": mime, "data": data })
                        }
                        Media::Url(u) => json!({ "type": "url", "url": u }),
                        Media::Base64 { .. } => {
                            dropped.feature(Feature::File);
                            continue;
                        }
                    };
                    let mut doc = json!({ "type": "document", "source": source });
                    if let Some(n) = name {
                        doc["title"] = json!(n);
                    }
                    others.push(doc);
                }
                Part::Thinking(th) => match &th.signature {
                    Some(s) if s.vendor == Vendor::Anthropic && s.redacted => {
                        others.push(json!({ "type": "redacted_thinking", "data": s.value }))
                    }
                    Some(s) if s.vendor == Vendor::Anthropic => others.push(json!({
                        "type": "thinking",
                        "thinking": th.text,
                        "signature": s.value,
                    })),
                    _ => dropped.feature(Feature::ReasoningHistory),
                },
                Part::ToolCall(c) => others.push(json!({
                    "type": "tool_use",
                    "id": c.id,
                    "name": c.name,
                    "input": c.input.to_object(),
                })),
                Part::ToolResult(res) => results.push(tool_result(res, dropped)),
            }
        }
        if results.is_empty() && others.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some((role, r, o)) if *role == m.role => {
                r.extend(results);
                o.extend(others);
            }
            _ => out.push((m.role, results, others)),
        }
    }
    out.into_iter()
        .map(|(role, results, others)| {
            json!({
                "role": if role == Role::User { "user" } else { "assistant" },
                "content": results.into_iter().chain(others).collect::<Vec<_>>(),
            })
        })
        .collect()
}

fn image(media: &Media, dropped: &mut Dropped) -> Option<Value> {
    match media {
        Media::Base64 { mime, data } if IMAGE_MIMES.contains(&mime.as_str()) => Some(json!({
            "type": "image",
            "source": { "type": "base64", "media_type": mime, "data": data },
        })),
        Media::Base64 { .. } => {
            dropped.feature(Feature::File);
            None
        }
        Media::Url(u) => Some(json!({
            "type": "image",
            "source": { "type": "url", "url": u },
        })),
    }
}

fn tool_result(res: &ToolResult, dropped: &mut Dropped) -> Value {
    let mut o = json!({ "type": "tool_result", "tool_use_id": res.id });
    if res.has_image() {
        let content: Vec<Value> = res
            .content
            .iter()
            .filter_map(|p| match p {
                Part::Text(s) if !s.is_empty() => Some(json!({ "type": "text", "text": s })),
                Part::Image(m) => image(m, dropped),
                _ => None,
            })
            .collect();
        o["content"] = Value::Array(content);
    } else {
        // 全是文字时写字符串：兼容实现对数组形式认得不全
        let text = res.text();
        if !text.is_empty() {
            o["content"] = json!(text);
        }
    }
    if res.is_error {
        o["is_error"] = json!(true);
    }
    o
}

/// Anthropic 要求 `input_schema` 是 `type: object`
fn object_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(m) if m.get("type").and_then(Value::as_str) == Some("object") => {
            schema.clone()
        }
        Value::Object(m) if !m.contains_key("type") => {
            let mut m = m.clone();
            m.insert("type".into(), json!("object"));
            Value::Object(m)
        }
        _ => json!({ "type": "object" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(s: &str) -> (Request, Vec<String>) {
        let mut d = Dropped::new(Dialect::Anthropic);
        let r = decode_request(&serde_json::from_str(s).unwrap(), &mut d).unwrap();
        (r, d.into_vec())
    }

    fn target(official: bool) -> Target {
        Target {
            dialect: Dialect::Anthropic,
            official,
            default_max_tokens: 32000,
        }
    }

    fn encode(r: &Request, client: Dialect, official: bool) -> (Value, Vec<String>) {
        let mut d = Dropped::new(client);
        let v = encode_request(r, &target(official), &mut d);
        (v, d.into_vec())
    }

    /// Claude Code 实际发出的那种请求：数组形式的 system 带 cache_control，工具调用
    /// 和结果，带签名的思考块，自适应思考加强度。
    const CLAUDE_CODE: &str = r#"{
        "model": "claude-opus-4-7",
        "max_tokens": 32000,
        "stream": true,
        "system": [
            {"type": "text", "text": "You are Claude Code."},
            {"type": "text", "text": "Project rules.", "cache_control": {"type": "ephemeral"}}
        ],
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "读一下 a.rs"}]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "先读文件。", "signature": "EqQBCkY"},
                {"type": "tool_use", "id": "toolu_01", "name": "Read", "input": {"path": "a.rs"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_01", "content": "fn main() {}"},
                {"type": "text", "text": "继续"}
            ]}
        ],
        "tools": [
            {"name": "Read", "description": "读文件", "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}},
            {"type": "web_search_20250305", "name": "web_search"}
        ],
        "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": "xhigh"},
        "metadata": {"user_id": "u"}
    }"#;

    #[test]
    fn a_claude_code_request_decodes_everything_that_has_a_counterpart() {
        let (r, dropped) = decode(CLAUDE_CODE);
        assert_eq!(r.model, "claude-opus-4-7");
        assert_eq!(r.system, ["You are Claude Code.", "Project rules."]);
        assert_eq!(r.messages.len(), 3);
        let Part::Thinking(th) = &r.messages[1].parts[0] else {
            panic!("{:?}", r.messages[1]);
        };
        assert_eq!(
            th.signature,
            Some(Signature::new(Vendor::Anthropic, "EqQBCkY"))
        );
        assert!(matches!(&r.messages[1].parts[1], Part::ToolCall(c) if c.name == "Read"));
        assert!(
            matches!(&r.messages[2].parts[0], Part::ToolResult(t) if t.text() == "fn main() {}")
        );
        assert_eq!(r.tools.len(), 1);
        assert_eq!(r.parallel_tool_calls, Some(false));
        let re = r.reasoning.as_ref().unwrap();
        assert!(re.enabled && re.summary);
        assert_eq!(re.effort, Some(Effort::XHigh));
        assert!(r.stream);
        assert_eq!(dropped, ["tools.web_search_20250305"]);
    }

    #[test]
    fn server_tool_blocks_in_history_are_reported() {
        let (r, dropped) = decode(
            r#"{"model":"m","max_tokens":1,"messages":[{"role":"assistant","content":[
                {"type":"server_tool_use","id":"s","name":"web_search","input":{}},
                {"type":"web_search_tool_result","tool_use_id":"s","content":[]},
                {"type":"text","text":"结果如下"}]}]}"#,
        );
        assert_eq!(r.messages[0].parts, [Part::Text("结果如下".into())]);
        assert_eq!(
            dropped,
            [
                "messages.content.server_tool_use",
                "messages.content.web_search_tool_result"
            ]
        );
    }

    #[test]
    fn back_to_anthropic_the_request_keeps_its_shape() {
        let (r, _) = decode(CLAUDE_CODE);
        let (v, dropped) = encode(&r, Dialect::Anthropic, true);
        assert!(dropped.is_empty(), "{dropped:?}");
        assert_eq!(v["system"][1]["text"], "Project rules.");
        assert_eq!(v["messages"][1]["content"][0]["type"], "thinking");
        assert_eq!(v["messages"][1]["content"][0]["signature"], "EqQBCkY");
        assert_eq!(v["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(v["tool_choice"]["disable_parallel_tool_use"], true);
        assert_eq!(v["thinking"]["type"], "adaptive");
        assert_eq!(v["output_config"]["effort"], "xhigh");
    }

    fn chat_like(model: &str) -> Request {
        Request {
            model: model.into(),
            messages: vec![
                Message {
                    role: Role::User,
                    parts: vec![Part::Text("hi".into())],
                },
                Message {
                    role: Role::User,
                    parts: vec![Part::Text("again".into())],
                },
            ],
            temperature: Some(0.2),
            top_p: Some(0.9),
            top_k: Some(40),
            seed: Some(7),
            reasoning: Some(Reasoning {
                enabled: true,
                effort: Some(Effort::High),
                budget: None,
                summary: false,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn the_official_endpoint_gets_no_sampling_parameters() {
        let (v, dropped) = encode(&chat_like("claude-opus-4-7"), Dialect::Chat, true);
        for k in ["temperature", "top_p", "top_k", "seed"] {
            assert!(v.get(k).is_none(), "{k}: {v}");
        }
        assert_eq!(dropped, ["temperature", "top_p", "top_k", "seed"]);
        // 相邻的 user 消息并成一条
        assert_eq!(v["messages"].as_array().unwrap().len(), 1);
        assert_eq!(v["max_tokens"], 32000);
        assert_eq!(
            v["thinking"],
            json!({"type": "adaptive", "display": "omitted"})
        );
        assert_eq!(v["output_config"]["effort"], "high");
    }

    #[test]
    fn an_older_claude_gets_a_token_budget_under_max_tokens() {
        let mut r = chat_like("claude-sonnet-4-5");
        r.max_tokens = Some(16000);
        let (v, _) = encode(&r, Dialect::Chat, true);
        assert_eq!(v["thinking"]["type"], "enabled");
        // high 折成 32000，但必须小于 max_tokens
        assert_eq!(v["thinking"]["budget_tokens"], 15999);
    }

    #[test]
    fn a_compatible_endpoint_keeps_sampling_unless_thinking_forbids_it() {
        let mut r = chat_like("deepseek-chat");
        r.reasoning = None;
        let (v, dropped) = encode(&r, Dialect::Chat, false);
        assert_eq!(v["temperature"], 0.2);
        assert_eq!(v["top_k"], 40);
        assert_eq!(dropped, ["seed"]);

        let (v, dropped) = encode(&chat_like("deepseek-chat"), Dialect::Chat, false);
        assert_eq!(v["thinking"]["type"], "enabled");
        assert!(v.get("temperature").is_none() && v.get("top_k").is_none());
        assert_eq!(dropped, ["temperature", "top_p", "top_k", "seed"]);
    }

    #[test]
    fn thinking_is_turned_off_rather_than_sent_into_a_guaranteed_400() {
        // 工具调用那一轮来自别家（没有 Anthropic 签发的思考块），手动思考模式会拒绝
        let r = Request {
            model: "claude-sonnet-4-5".into(),
            max_tokens: Some(20000),
            messages: vec![
                Message {
                    role: Role::Assistant,
                    parts: vec![
                        Part::Thinking(Thinking {
                            text: "x".into(),
                            signature: Some(Signature::new(Vendor::OpenAi, "rs_1:enc")),
                        }),
                        Part::ToolCall(ToolCall {
                            id: "call_1".into(),
                            name: "Read".into(),
                            input: ToolInput::Json(json!({})),
                        }),
                    ],
                },
                Message {
                    role: Role::User,
                    parts: vec![Part::ToolResult(ToolResult {
                        id: "call_1".into(),
                        content: vec![Part::Text("ok".into())],
                        is_error: false,
                    })],
                },
            ],
            reasoning: Some(Reasoning {
                enabled: true,
                effort: Some(Effort::Medium),
                budget: None,
                summary: true,
            }),
            ..Default::default()
        };
        let (v, dropped) = encode(&r, Dialect::Responses, true);
        assert!(v.get("thinking").is_none(), "{v}");
        assert_eq!(dropped, ["input.reasoning", "reasoning"]);
        // OpenAI 的推理内容不进 Anthropic 的历史
        assert_eq!(v["messages"][0]["content"][0]["type"], "tool_use");
    }

    #[test]
    fn a_freeform_tool_is_declared_as_an_object_with_one_string() {
        let r = Request {
            model: "m".into(),
            tools: vec![Tool {
                name: "apply_patch".into(),
                description: Some("改文件".into()),
                kind: ToolKind::Freeform {
                    format: Some(json!({"type": "grammar"})),
                },
            }],
            messages: vec![Message {
                role: Role::Assistant,
                parts: vec![Part::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "apply_patch".into(),
                    input: ToolInput::Text("*** Begin Patch".into()),
                })],
            }],
            ..Default::default()
        };
        let (v, dropped) = encode(&r, Dialect::Responses, true);
        assert_eq!(v["tools"][0]["input_schema"], freeform_schema());
        assert_eq!(
            v["messages"][0]["content"][0]["input"]["input"],
            "*** Begin Patch"
        );
        assert_eq!(dropped, ["tools.custom.format"]);
    }

    #[test]
    fn tool_results_come_first_even_after_merging() {
        let r = Request {
            model: "m".into(),
            messages: vec![
                Message {
                    role: Role::User,
                    parts: vec![Part::Text("看结果".into())],
                },
                Message {
                    role: Role::User,
                    parts: vec![Part::ToolResult(ToolResult {
                        id: "c".into(),
                        content: vec![],
                        is_error: true,
                    })],
                },
            ],
            ..Default::default()
        };
        let (v, _) = encode(&r, Dialect::Chat, false);
        let content = &v["messages"][0]["content"];
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["is_error"], true);
        assert_eq!(content[1]["type"], "text");
    }
}
