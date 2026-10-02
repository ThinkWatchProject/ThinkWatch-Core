//! 四种客户端格式：按格式写请求，按格式读回答。
//!
//! 假上游说的是 Anthropic，所以 Chat、Responses、Gemini 的客户端都要经过网关的格式转换：
//! 请求钩子改的是客户端那一份（再转给上游），回答钩子看的是转回客户端格式之后的那一份。

use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fmt {
    Anthropic,
    Chat,
    Responses,
    Gemini,
}

pub const FORMATS: [Fmt; 4] = [Fmt::Anthropic, Fmt::Chat, Fmt::Responses, Fmt::Gemini];

/// 对话里的一步
#[derive(Clone, Debug)]
pub enum Turn {
    User(String),
    /// 助手的一段话
    Assistant(String),
    /// 助手调了一个工具
    Call {
        id: String,
        name: String,
        input: Value,
    },
    /// 那个工具的结果
    Result {
        id: String,
        name: String,
        text: String,
    },
}

impl Fmt {
    pub fn path(self, model: &str, stream: bool) -> String {
        match self {
            Fmt::Anthropic => "/v1/messages".into(),
            Fmt::Chat => "/v1/chat/completions".into(),
            Fmt::Responses => "/v1/responses".into(),
            Fmt::Gemini if stream => {
                format!("/v1beta/models/{model}:streamGenerateContent?alt=sse")
            }
            Fmt::Gemini => format!("/v1beta/models/{model}:generateContent"),
        }
    }

    pub fn request(self, model: &str, system: &str, turns: &[Turn], stream: bool) -> Value {
        match self {
            Fmt::Anthropic => {
                let messages: Vec<Value> = turns
                    .iter()
                    .map(|t| match t {
                        Turn::User(s) => json!({ "role": "user", "content": s }),
                        Turn::Assistant(s) => json!({ "role": "assistant", "content": s }),
                        Turn::Call { id, name, input } => json!({ "role": "assistant", "content": [
                            { "type": "tool_use", "id": id, "name": name, "input": input }
                        ] }),
                        Turn::Result { id, text, .. } => json!({ "role": "user", "content": [
                            { "type": "tool_result", "tool_use_id": id, "content": text }
                        ] }),
                    })
                    .collect();
                json!({ "model": model, "max_tokens": 256, "stream": stream, "system": system, "messages": messages })
            }
            Fmt::Chat => {
                let mut messages = vec![json!({ "role": "system", "content": system })];
                messages.extend(turns.iter().map(|t| match t {
                    Turn::User(s) => json!({ "role": "user", "content": s }),
                    Turn::Assistant(s) => json!({ "role": "assistant", "content": s }),
                    Turn::Call { id, name, input } => json!({ "role": "assistant", "content": null, "tool_calls": [
                        { "id": id, "type": "function", "function": { "name": name, "arguments": input.to_string() } }
                    ] }),
                    Turn::Result { id, text, .. } => {
                        json!({ "role": "tool", "tool_call_id": id, "content": text })
                    }
                }));
                json!({ "model": model, "stream": stream, "messages": messages })
            }
            Fmt::Responses => {
                let input: Vec<Value> = turns
                    .iter()
                    .map(|t| match t {
                        Turn::User(s) => json!({ "role": "user", "content": [{ "type": "input_text", "text": s }] }),
                        Turn::Assistant(s) => json!({ "role": "assistant", "content": [{ "type": "output_text", "text": s }] }),
                        Turn::Call { id, name, input } => json!({ "type": "function_call", "call_id": id, "name": name, "arguments": input.to_string() }),
                        Turn::Result { id, text, .. } => json!({ "type": "function_call_output", "call_id": id, "output": text }),
                    })
                    .collect();
                json!({ "model": model, "stream": stream, "instructions": system, "input": input })
            }
            Fmt::Gemini => {
                let contents: Vec<Value> = turns
                    .iter()
                    .map(|t| match t {
                        Turn::User(s) => json!({ "role": "user", "parts": [{ "text": s }] }),
                        Turn::Assistant(s) => json!({ "role": "model", "parts": [{ "text": s }] }),
                        Turn::Call { name, input, .. } => json!({ "role": "model", "parts": [{ "functionCall": { "name": name, "args": input } }] }),
                        Turn::Result { name, text, .. } => json!({ "role": "user", "parts": [{ "functionResponse": { "name": name, "response": { "content": text } } }] }),
                    })
                    .collect();
                json!({ "systemInstruction": { "parts": [{ "text": system }] }, "contents": contents })
            }
        }
    }

    /// 客户端收到的全部文字
    pub fn text(self, body: &str, stream: bool) -> String {
        let frames = data_frames(body);
        match (self, stream) {
            (Fmt::Anthropic, true) => frames
                .iter()
                .filter_map(|v| v["delta"]["text"].as_str())
                .collect(),
            (Fmt::Anthropic, false) => one(body)["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|b| b["text"].as_str())
                .collect(),
            (Fmt::Chat, true) => frames
                .iter()
                .filter_map(|v| v["choices"][0]["delta"]["content"].as_str())
                .collect(),
            (Fmt::Chat, false) => one(body)["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            (Fmt::Responses, true) => frames
                .iter()
                .filter(|v| v["type"] == "response.output_text.delta")
                .filter_map(|v| v["delta"].as_str())
                .collect(),
            (Fmt::Responses, false) => one(body)["output"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|o| o["type"] == "message")
                .flat_map(|o| o["content"].as_array().cloned().unwrap_or_default())
                .filter_map(|c| c["text"].as_str().map(str::to_string))
                .collect(),
            (Fmt::Gemini, true) => frames.iter().map(gemini_text).collect(),
            (Fmt::Gemini, false) => gemini_text(&one(body)),
        }
    }

    /// 客户端收到的工具调用：`(名字, 参数)`
    pub fn calls(self, body: &str, stream: bool) -> Vec<(String, Value)> {
        let frames = data_frames(body);
        match (self, stream) {
            (Fmt::Anthropic, true) => {
                let mut out: Vec<(u64, String, String)> = Vec::new();
                for v in &frames {
                    if v["type"] == "content_block_start"
                        && v["content_block"]["type"] == "tool_use"
                    {
                        out.push((
                            v["index"].as_u64().unwrap(),
                            v["content_block"]["name"].as_str().unwrap().to_string(),
                            String::new(),
                        ));
                    }
                    if let Some(part) = v["delta"]["partial_json"].as_str()
                        && let Some(c) = out.iter_mut().find(|c| Some(c.0) == v["index"].as_u64())
                    {
                        c.2.push_str(part);
                    }
                }
                out.into_iter().map(|(_, n, a)| (n, args(&a))).collect()
            }
            (Fmt::Anthropic, false) => one(body)["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "tool_use")
                .map(|b| (b["name"].as_str().unwrap().to_string(), b["input"].clone()))
                .collect(),
            (Fmt::Chat, true) => {
                let mut out: Vec<(u64, String, String)> = Vec::new();
                for v in &frames {
                    for c in v["choices"][0]["delta"]["tool_calls"]
                        .as_array()
                        .into_iter()
                        .flatten()
                    {
                        let i = c["index"].as_u64().unwrap_or(0);
                        if !out.iter().any(|o| o.0 == i) {
                            out.push((i, String::new(), String::new()));
                        }
                        let o = out.iter_mut().find(|o| o.0 == i).unwrap();
                        if let Some(n) = c["function"]["name"].as_str() {
                            o.1.push_str(n);
                        }
                        if let Some(a) = c["function"]["arguments"].as_str() {
                            o.2.push_str(a);
                        }
                    }
                }
                out.into_iter().map(|(_, n, a)| (n, args(&a))).collect()
            }
            (Fmt::Chat, false) => one(body)["choices"][0]["message"]["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| {
                    (
                        c["function"]["name"].as_str().unwrap().to_string(),
                        args(c["function"]["arguments"].as_str().unwrap()),
                    )
                })
                .collect(),
            (Fmt::Responses, true) => frames
                .iter()
                .filter(|v| {
                    v["type"] == "response.output_item.done" && v["item"]["type"] == "function_call"
                })
                .map(|v| {
                    (
                        v["item"]["name"].as_str().unwrap().to_string(),
                        args(v["item"]["arguments"].as_str().unwrap()),
                    )
                })
                .collect(),
            (Fmt::Responses, false) => one(body)["output"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|o| o["type"] == "function_call")
                .map(|o| {
                    (
                        o["name"].as_str().unwrap().to_string(),
                        args(o["arguments"].as_str().unwrap()),
                    )
                })
                .collect(),
            (Fmt::Gemini, true) => frames.iter().flat_map(gemini_calls).collect(),
            (Fmt::Gemini, false) => gemini_calls(&one(body)),
        }
    }
}

fn one(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"))
}

fn args(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|e| panic!("tool arguments are not JSON ({e}): {s}"))
}

fn data_frames(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .collect()
}

fn gemini_text(v: &Value) -> String {
    v["candidates"][0]["content"]["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["text"].as_str())
        .collect()
}

fn gemini_calls(v: &Value) -> Vec<(String, Value)> {
    v["candidates"][0]["content"]["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p.get("functionCall"))
        .map(|c| (c["name"].as_str().unwrap().to_string(), c["args"].clone()))
        .collect()
}

// ── 上游（Anthropic）收到的那一份 ────────────────────────────────

/// 系统提示词：字符串，或者几块文字连起来
pub fn sent_system(body: &Value) -> String {
    match &body["system"] {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// 全部消息里的文字（含工具结果），连起来
pub fn sent_texts(body: &Value) -> String {
    let mut out = String::new();
    for m in body["messages"].as_array().into_iter().flatten() {
        match &m["content"] {
            Value::String(s) => out.push_str(s),
            Value::Array(blocks) => {
                for b in blocks {
                    if let Some(t) = b["text"].as_str() {
                        out.push_str(t);
                    }
                    match &b["content"] {
                        Value::String(s) => out.push_str(s),
                        Value::Array(inner) => {
                            for i in inner {
                                if let Some(t) = i["text"].as_str() {
                                    out.push_str(t);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        out.push('\n');
    }
    out
}

/// 历史里每个工具调用的参数
pub fn sent_tool_inputs(body: &Value) -> Vec<Value> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter(|b| b["type"] == "tool_use")
        .map(|b| b["input"].clone())
        .collect()
}
