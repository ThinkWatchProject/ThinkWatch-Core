//! 请求里写出来的文字：人、模型、工具写的字。出站脱敏只看这些。
//!
//! # 只认名单里的位置
//!
//! 请求体里还有协议自己的东西：条目和调用的 id、类型、角色、工具名、模型名、推理的签名和
//! 加密内容、图片、各种配置项。它们的值由协议或上游定，改一个字上游就拒收整个请求 ——
//! Codex 的推理条目 id（`rs_` 加一长串十六进制）里碰巧有 11 位连着的数字，手机号那条规则
//! 认得出，换成占位符之后这段对话每一轮都被拒。
//!
//! 所以这里**按协议的结构列出正文在哪儿**，脱敏只在这些字符串里找；名单之外的一律不看，
//! 不认识的块、不认识的字段也不看。协议新加了放正文的地方，在这里补上。
//!
//! 名单是：系统提示、各个角色的消息文字（模型以前的回答也算）、推理的文字和摘要、工具调用
//! 的参数、工具结果、工具定义的说明。**推理的文字也算**：回答里的占位符送回客户端之前换回了
//! 原值，客户端下一轮把整段历史连同推理原样发回来，不再换一次的话，真值就这样发了出去 ——
//! 签过名的推理（Anthropic 的 `thinking`）也只有换回原来的占位符，才和签名对得上。
//!
//! # 给的是原文里的位置
//!
//! 脱敏跑在请求体的 JSON 原文上（规则认得 JSON 的转义，换下来的值放回去还是合法的 JSON），
//! 所以 [`ranges`] 给的是每个正文字符串在原文里引号之间的那一段。哪些字符串算正文在解析
//! 出来的 [`Value`] 上判断（[`spots`]，和 [`crate::caller`] 一样按位置），再在原文上走一遍、
//! 按位置对上。

use std::collections::HashMap;
use std::ops::Range;

use serde_json::Value;

use crate::caller::{
    Collect, Spot, Step, anthropic_document, anthropic_search_result, each, gemini_key, str_of,
};
use crate::ir::Dialect;

/// 一个请求体是什么形状：决定正文在哪儿。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// 生成回答的请求，和同一种形状的数 token、压缩上下文
    Generate(Dialect),
    /// OpenAI 的嵌入（`/v1/embeddings`）：`input`
    Embeddings,
    /// 旧版补全（OpenAI 的 `/v1/completions`、Anthropic 的 `/v1/complete`）：`prompt`、`suffix`
    Completion,
    /// Gemini 的嵌入（`:embedContent`、`:batchEmbedContents`）
    GeminiEmbed,
    /// Realtime 连接上客户端发的一帧事件
    RealtimeEvent,
}

/// 一个 `shape` 形状的请求体里写出来的每一个字符串，按在请求里出现的先后。空串不算；
/// 不是 JSON 对象的请求体没有。
pub fn spots(shape: Shape, body: &Value) -> Vec<Spot> {
    let mut c = Collect::default();
    if body.is_object() {
        match shape {
            Shape::Generate(Dialect::Anthropic) => anthropic(&mut c, body),
            Shape::Generate(Dialect::Chat) => chat(&mut c, body),
            Shape::Generate(Dialect::Responses) => responses(&mut c, body),
            Shape::Generate(Dialect::Gemini) => gemini(&mut c, body),
            Shape::Generate(Dialect::Bedrock) => bedrock(&mut c, body),
            Shape::Embeddings => {
                if let Some(input) = body.get("input") {
                    c.key("input", |c| strings(c, input));
                }
            }
            Shape::Completion => {
                for k in ["prompt", "suffix"] {
                    if let Some(x) = body.get(k) {
                        c.key(k, |c| strings(c, x));
                    }
                }
            }
            Shape::GeminiEmbed => {
                gemini_embed(&mut c, body);
                for (i, r) in each(body, "requests") {
                    c.key("requests", |c| c.index(i, |c| gemini_embed(c, r)));
                }
            }
            Shape::RealtimeEvent => realtime(&mut c, body),
        }
    }
    c.out
}

/// 原文里每个正文字符串引号之间的那一段，按先后。不是 JSON 的是 `None`。
///
/// 字符串里原样写着控制字符（换行、制表符）的请求体也认：有的客户端这么发，上游也收。
pub fn ranges(shape: Shape, text: &str) -> Option<Vec<Range<usize>>> {
    let value = parse(text)?;
    let mut tree = Tree::default();
    for s in spots(shape, &value) {
        tree.insert(s.path());
    }
    let mut out = Vec::new();
    if !tree.is_empty() {
        walk(text, &tree, &mut out);
    }
    Some(out)
}

fn parse(text: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str(text) {
        return Some(v);
    }
    if !text.bytes().any(|b| b < 0x20) {
        return None;
    }
    // 控制字符换成空格：都是一个字节，位置不变
    let spaced: String = text
        .chars()
        .map(|c| if (c as u32) < 0x20 { ' ' } else { c })
        .collect();
    serde_json::from_str(&spaced).ok()
}

/// 正文字符串的位置，按路径排成一棵树：走原文时一层一层往下对
#[derive(Default)]
struct Tree {
    keys: HashMap<String, Tree>,
    indices: HashMap<usize, Tree>,
    /// 这个位置本身是一个正文字符串
    leaf: bool,
}

impl Tree {
    fn insert(&mut self, path: &[Step]) {
        let mut at = self;
        for s in path {
            at = match s {
                Step::Key(k) => at.keys.entry(k.clone()).or_default(),
                Step::Index(i) => at.indices.entry(*i).or_default(),
            };
        }
        at.leaf = true;
    }

    fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.indices.is_empty() && !self.leaf
    }
}

/// 原文里正在里面的一层
enum Frame<'t> {
    /// 一个对象，和眼下这个值的键（还没读到键时是 `None`）
    Object(Option<&'t Tree>, Option<String>),
    /// 一个数组，和眼下这个值的下标
    Array(Option<&'t Tree>, usize),
}

impl<'t> Frame<'t> {
    /// 眼下这个值在树上的位置。不在树上的是 `None`：它里面没有正文
    fn child(&self) -> Option<&'t Tree> {
        match self {
            Frame::Object(t, Some(k)) => t.and_then(|t| t.keys.get(k)),
            Frame::Object(_, None) => None,
            Frame::Array(t, i) => t.and_then(|t| t.indices.get(i)),
        }
    }
}

/// 走一遍 JSON 原文（已经解析过，是合规的），把树上的字符串在原文里的位置记下来。
///
/// **只扫一遍、不建树**：位置一边走一边对，一个不在树上的对象或数组里面什么都不记。
fn walk(text: &str, root: &Tree, out: &mut Vec<Range<usize>>) {
    let b = text.as_bytes();
    let mut stack: Vec<Frame<'_>> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            c @ (b'{' | b'[') => {
                let at = match stack.last() {
                    Some(f) => f.child(),
                    None => Some(root),
                };
                stack.push(if c == b'{' {
                    Frame::Object(at, None)
                } else {
                    Frame::Array(at, 0)
                });
                i += 1;
            }
            b'}' | b']' => {
                stack.pop();
                i += 1;
            }
            b',' => {
                match stack.last_mut() {
                    Some(Frame::Array(_, n)) => *n += 1,
                    Some(Frame::Object(_, k)) => *k = None,
                    None => {}
                }
                i += 1;
            }
            b'"' => {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && b[j] != b'"' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                let end = j.min(b.len());
                i = end + 1;
                match stack.last_mut() {
                    // 对象里还没读到键：这一个就是键
                    Some(Frame::Object(_, k @ None)) => *k = Some(key(&text[start - 1..i])),
                    Some(f) if f.child().is_some_and(|t| t.leaf) => out.push(start..end),
                    _ => {}
                }
            }
            _ => i += 1,
        }
    }
}

/// 原文里带引号的一个键，解开转义
fn key(quoted: &str) -> String {
    let inner = &quoted[1..quoted.len().saturating_sub(1).max(1)];
    if !inner.contains('\\') {
        return inner.to_string();
    }
    serde_json::from_str(quoted).unwrap_or_else(|_| inner.to_string())
}

/// 一个字符串，或者一组字符串里的每一个（嵌入的 `input`、补全的 `prompt`）。写成 token 的
/// 数组是数，不是文字
fn strings(c: &mut Collect, v: &Value) {
    match v {
        Value::String(_) => c.here(v, false),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                c.index(i, |c| c.here(item, false));
            }
        }
        _ => {}
    }
}

/// `v.k` 下的整个值里的全部字符串：工具调用的参数对象（键是工具定的，值是模型写的）
fn leaves_at(c: &mut Collect, v: &Value, k: &str, in_tool_result: bool) {
    if let Some(x) = v.get(k) {
        c.key(k, |c| c.leaves(x, in_tool_result));
    }
}

/// `v.k` 下面一个数组里每一项的 `field`
fn each_field(c: &mut Collect, v: &Value, k: &str, field: &str) {
    c.key(k, |c| {
        for (i, x) in each(v, k) {
            c.index(i, |c| c.field(x, field, false));
        }
    });
}

// ───────────────────────────────────────────────────────── Anthropic

/// 系统提示、每个角色的消息、工具的说明
fn anthropic(c: &mut Collect, v: &Value) {
    if let Some(sys) = v.get("system") {
        c.key("system", |c| c.text_of(sys, false));
    }
    for (i, m) in each(v, "messages") {
        let Some(content) = m.get("content") else {
            continue;
        };
        c.key("messages", |c| {
            c.index(i, |c| {
                c.key("content", |c| match content {
                    Value::String(_) => c.here(content, false),
                    Value::Array(blocks) => {
                        for (j, b) in blocks.iter().enumerate() {
                            c.index(j, |c| anthropic_block(c, b));
                        }
                    }
                    _ => {}
                })
            })
        });
    }
    each_field(c, v, "tools", "description");
}

fn anthropic_block(c: &mut Collect, b: &Value) {
    match str_of(b, "type").unwrap_or("") {
        "text" => c.field(b, "text", false),
        "thinking" => c.field(b, "thinking", false),
        "document" => anthropic_document(c, b, false),
        "search_result" => anthropic_search_result(c, b, false),
        "tool_use" | "server_tool_use" | "mcp_tool_use" => leaves_at(c, b, "input", false),
        "tool_result" | "mcp_tool_result" => {
            let Some(content) = b.get("content") else {
                return;
            };
            c.key("content", |c| match content {
                Value::String(_) => c.here(content, true),
                Value::Array(items) => {
                    for (k, item) in items.iter().enumerate() {
                        c.index(k, |c| match str_of(item, "type").unwrap_or("") {
                            "text" => c.field(item, "text", true),
                            "document" => anthropic_document(c, item, true),
                            "search_result" => anthropic_search_result(c, item, true),
                            _ => {}
                        });
                    }
                }
                _ => {}
            });
        }
        _ => {}
    }
}

// ───────────────────────────────────────────────────────── Chat

/// 每个角色的消息：文字、拒答、推理（`reasoning_content` / `reasoning`）、工具调用的参数，
/// 工具消息的内容；预测输出；工具的说明
fn chat(c: &mut Collect, v: &Value) {
    for (i, m) in each(v, "messages") {
        let tool = str_of(m, "role") == Some("tool");
        c.key("messages", |c| {
            c.index(i, |c| {
                if let Some(content) = m.get("content") {
                    c.key("content", |c| chat_content(c, content, tool));
                }
                for k in ["reasoning_content", "reasoning", "refusal"] {
                    c.field(m, k, false);
                }
                c.key("tool_calls", |c| {
                    for (j, call) in each(m, "tool_calls") {
                        c.index(j, |c| {
                            if let Some(f) = call.get("function") {
                                c.key("function", |c| c.field(f, "arguments", false));
                            }
                            if let Some(x) = call.get("custom") {
                                c.key("custom", |c| c.field(x, "input", false));
                            }
                        });
                    }
                });
                // 旧的写法，一条消息一个调用
                if let Some(f) = m.get("function_call") {
                    c.key("function_call", |c| c.field(f, "arguments", false));
                }
            })
        });
    }
    if let Some(p) = v.get("prediction")
        && let Some(content) = p.get("content")
    {
        c.key("prediction", |c| {
            c.key("content", |c| chat_content(c, content, false))
        });
    }
    c.key("tools", |c| {
        for (i, t) in each(v, "tools") {
            c.index(i, |c| {
                for k in ["function", "custom"] {
                    if let Some(f) = t.get(k) {
                        c.key(k, |c| c.field(f, "description", false));
                    }
                }
            });
        }
    });
}

/// 字符串，或者各块的 `text`、`refusal`
fn chat_content(c: &mut Collect, content: &Value, in_tool_result: bool) {
    match content {
        Value::String(_) => c.here(content, in_tool_result),
        Value::Array(parts) => {
            for (i, p) in parts.iter().enumerate() {
                c.index(i, |c| match str_of(p, "type").unwrap_or("") {
                    "text" => c.field(p, "text", in_tool_result),
                    "refusal" => c.field(p, "refusal", in_tool_result),
                    _ => {}
                });
            }
        }
        _ => {}
    }
}

// ───────────────────────────────────────────────────────── Responses

/// `instructions`、`input`（一个字符串，或者一条条的条目）、工具的说明
fn responses(c: &mut Collect, v: &Value) {
    c.field(v, "instructions", false);
    if let Some(input) = v.get("input") {
        c.key("input", |c| match input {
            Value::String(_) => c.here(input, false),
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    c.index(i, |c| responses_item(c, item));
                }
            }
            _ => {}
        });
    }
    responses_tools(c, v);
}

fn responses_item(c: &mut Collect, item: &Value) {
    match str_of(item, "type").unwrap_or("message") {
        "message" | "agent_message" => {
            if let Some(content) = item.get("content") {
                c.key("content", |c| responses_content(c, content, false));
            }
        }
        "reasoning" => {
            for k in ["summary", "content"] {
                each_field(c, item, k, "text");
            }
        }
        "function_call" | "mcp_call" => {
            c.field(item, "arguments", false);
            c.field(item, "output", true);
        }
        "custom_tool_call" => c.field(item, "input", false),
        "tool_search_call" => match item.get("arguments") {
            Some(a @ Value::Object(_)) => c.key("arguments", |c| c.leaves(a, false)),
            _ => c.field(item, "arguments", false),
        },
        "local_shell_call" => {
            let Some(action) = item.get("action") else {
                return;
            };
            c.key("action", |c| {
                c.field(action, "working_directory", false);
                for k in ["command", "env"] {
                    leaves_at(c, action, k, false);
                }
            });
        }
        "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
            if let Some(output) = item.get("output") {
                c.key("output", |c| responses_content(c, output, true));
            }
        }
        "additional_tools" | "tool_search_output" => responses_tools(c, item),
        _ => {}
    }
}

/// 字符串，或者各块里的 `input_text` / `output_text` 的 `text`、`refusal` 的 `refusal`
fn responses_content(c: &mut Collect, content: &Value, in_tool_result: bool) {
    match content {
        Value::String(_) => c.here(content, in_tool_result),
        Value::Array(parts) => {
            for (i, p) in parts.iter().enumerate() {
                c.index(i, |c| match str_of(p, "type").unwrap_or("") {
                    "input_text" | "output_text" => c.field(p, "text", in_tool_result),
                    "refusal" => c.field(p, "refusal", in_tool_result),
                    _ => {}
                });
            }
        }
        _ => {}
    }
}

/// `tools` 里每个工具的说明，namespace 的说明和它里面的每个工具
fn responses_tools(c: &mut Collect, v: &Value) {
    c.key("tools", |c| {
        for (i, t) in each(v, "tools") {
            c.index(i, |c| {
                c.field(t, "description", false);
                if str_of(t, "type") == Some("namespace") {
                    each_field(c, t, "tools", "description");
                }
            });
        }
    });
}

// ───────────────────────────────────────────────────────── Gemini

/// 系统提示、每个角色的每一块（文字和推理、函数调用的参数、函数结果、代码执行的代码和输出）、
/// 函数的说明。数 token 的请求可能把整个生成请求包在 `generateContentRequest` 里
fn gemini(c: &mut Collect, v: &Value) {
    if let Some(k) = gemini_key(v, "systemInstruction") {
        let sys = &v[k.as_str()];
        c.key(&k, |c| match sys {
            Value::String(_) => c.here(sys, false),
            _ => gemini_parts(c, sys),
        });
    }
    for (i, content) in each(v, "contents") {
        c.key("contents", |c| c.index(i, |c| gemini_parts(c, content)));
    }
    c.key("tools", |c| {
        for (i, t) in each(v, "tools") {
            let Some(k) = gemini_key(t, "functionDeclarations") else {
                continue;
            };
            c.index(i, |c| each_field(c, t, &k, "description"));
        }
    });
    if let Some(k) = gemini_key(v, "generateContentRequest") {
        let inner = &v[k.as_str()];
        c.key(&k, |c| gemini(c, inner));
    }
}

/// 一个 `Content` 的 `parts`
fn gemini_parts(c: &mut Collect, content: &Value) {
    c.key("parts", |c| {
        for (j, p) in each(content, "parts") {
            c.index(j, |c| gemini_part(c, p));
        }
    });
}

fn gemini_part(c: &mut Collect, p: &Value) {
    c.field(p, "text", false);
    for (camel, inner, tool) in [
        ("functionCall", "args", false),
        ("functionResponse", "response", true),
    ] {
        if let Some(k) = gemini_key(p, camel) {
            let x = &p[k.as_str()];
            c.key(&k, |c| leaves_at(c, x, inner, tool));
        }
    }
    for (camel, inner, tool) in [
        ("executableCode", "code", false),
        ("codeExecutionResult", "output", true),
    ] {
        if let Some(k) = gemini_key(p, camel) {
            let x = &p[k.as_str()];
            c.key(&k, |c| c.field(x, inner, tool));
        }
    }
}

/// 嵌入请求里的 `content`
fn gemini_embed(c: &mut Collect, v: &Value) {
    if let Some(content) = v.get("content") {
        c.key("content", |c| gemini_parts(c, content));
    }
}

// ───────────────────────────────────────────────────────── Bedrock

/// 系统提示、每个角色的每一块（文字、文本文档、推理的文字、工具调用的参数、工具结果）、
/// 工具的说明
fn bedrock(c: &mut Collect, v: &Value) {
    each_field(c, v, "system", "text");
    for (i, m) in each(v, "messages") {
        c.key("messages", |c| {
            c.index(i, |c| {
                c.key("content", |c| {
                    for (j, b) in each(m, "content") {
                        c.index(j, |c| bedrock_block(c, b));
                    }
                })
            })
        });
    }
    if let Some(cfg) = v.get("toolConfig") {
        c.key("toolConfig", |c| {
            c.key("tools", |c| {
                for (i, t) in each(cfg, "tools") {
                    if let Some(spec) = t.get("toolSpec") {
                        c.index(i, |c| {
                            c.key("toolSpec", |c| c.field(spec, "description", false))
                        });
                    }
                }
            })
        });
    }
}

fn bedrock_block(c: &mut Collect, b: &Value) {
    c.field(b, "text", false);
    if let Some(src) = b.get("document").and_then(|d| d.get("source")) {
        c.key("document", |c| {
            c.key("source", |c| c.field(src, "text", false))
        });
    }
    if let Some(rt) = b
        .get("reasoningContent")
        .and_then(|r| r.get("reasoningText"))
    {
        c.key("reasoningContent", |c| {
            c.key("reasoningText", |c| c.field(rt, "text", false))
        });
    }
    if let Some(u) = b.get("toolUse") {
        c.key("toolUse", |c| leaves_at(c, u, "input", false));
    }
    if let Some(res) = b.get("toolResult") {
        c.key("toolResult", |c| {
            c.key("content", |c| {
                for (k, item) in each(res, "content") {
                    c.index(k, |c| {
                        c.field(item, "text", true);
                        leaves_at(c, item, "json", true);
                    });
                }
            })
        });
    }
}

// ───────────────────────────────────────────────────────── Realtime

/// Realtime 连接上客户端的一帧：会话的指令和工具说明（`session.update`），加进对话的条目
/// （`conversation.item.create`），要回答时另给的指令和条目（`response.create`）
fn realtime(c: &mut Collect, v: &Value) {
    if let Some(s) = v.get("session") {
        c.key("session", |c| {
            c.field(s, "instructions", false);
            each_field(c, s, "tools", "description");
        });
    }
    if let Some(item) = v.get("item") {
        c.key("item", |c| realtime_item(c, item));
    }
    if let Some(r) = v.get("response") {
        c.key("response", |c| {
            c.field(r, "instructions", false);
            each_field(c, r, "tools", "description");
            c.key("input", |c| {
                for (i, item) in each(r, "input") {
                    c.index(i, |c| realtime_item(c, item));
                }
            });
        });
    }
}

fn realtime_item(c: &mut Collect, item: &Value) {
    match str_of(item, "type").unwrap_or("") {
        "message" => {
            c.key("content", |c| {
                for (i, p) in each(item, "content") {
                    c.index(i, |c| {
                        c.field(p, "text", false);
                        c.field(p, "transcript", false);
                    });
                }
            });
        }
        "function_call" => c.field(item, "arguments", false),
        "function_call_output" => c.field(item, "output", true),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 找到的那些字符串，排好序（`json!` 写出来的键按字母排，原文里的先后跟着变）
    fn found(shape: Shape, body: &Value) -> Vec<String> {
        let text = body.to_string();
        let mut got: Vec<String> = ranges(shape, &text)
            .unwrap()
            .into_iter()
            .map(|r| text[r].to_string())
            .collect();
        got.sort();
        got
    }

    fn sorted<const N: usize>(mut v: [&str; N]) -> [&str; N] {
        v.sort();
        v
    }

    /// Codex 的请求：条目 id、调用 id、类型、角色、推理的加密内容、工具名都不是正文
    #[test]
    fn responses_text_is_found_and_protocol_fields_are_not() {
        let body = json!({
            "model": "gpt-5.5",
            "instructions": "be brief",
            "previous_response_id": "resp_1",
            "input": [
                {"type": "message", "id": "msg_1", "role": "user",
                 "content": [{"type": "input_text", "text": "hello"}]},
                {"type": "reasoning", "id": "rs_0a13800138000c", "encrypted_content": "gAAAA",
                 "summary": [{"type": "summary_text", "text": "thought"}]},
                {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "shell",
                 "arguments": "{\"cmd\":\"ls\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "a.rs"},
                {"type": "custom_tool_call", "call_id": "call_2", "name": "apply_patch", "input": "*** Begin"},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "done"}]},
                {"type": "web_search_call", "id": "ws_1", "action": {"query": "q"}},
            ],
            "tools": [
                {"type": "function", "name": "shell", "description": "run a command",
                 "parameters": {"type": "object", "properties": {"cmd": {"type": "string", "description": "the command"}}}},
                {"type": "namespace", "name": "mcp", "description": "MCP tools",
                 "tools": [{"type": "function", "name": "x", "description": "inner"}]},
            ],
            "reasoning": {"effort": "high"},
        });
        assert_eq!(
            found(Shape::Generate(Dialect::Responses), &body),
            sorted([
                "be brief",
                "hello",
                "thought",
                r#"{\"cmd\":\"ls\"}"#,
                "a.rs",
                "*** Begin",
                "done",
                "run a command",
                "MCP tools",
                "inner",
            ])
        );
    }

    #[test]
    fn anthropic_covers_every_role_and_leaves_signatures_and_ids_alone() {
        let body = json!({
            "model": "claude",
            "system": [{"type": "text", "text": "sys"}],
            "messages": [
                {"role": "user", "content": "q"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                    {"type": "redacted_thinking", "data": "xxx"},
                    {"type": "text", "text": "a"},
                    {"type": "tool_use", "id": "toolu_1", "name": "f", "input": {"id": "13800138000", "path": "/x"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "r"}]},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                ]},
            ],
            "tools": [{"name": "f", "description": "does f", "input_schema": {"type": "object"}}],
            "metadata": {"user_id": "u"},
        });
        assert_eq!(
            found(Shape::Generate(Dialect::Anthropic), &body),
            sorted(["sys", "q", "hmm", "a", "13800138000", "/x", "r", "does f"])
        );
    }

    #[test]
    fn chat_covers_reasoning_tool_calls_and_tool_messages() {
        let body = json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "s"},
                {"role": "user", "content": [{"type": "text", "text": "u"}, {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA"}}]},
                {"role": "assistant", "content": null, "reasoning_content": "think",
                 "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "f", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "call_1", "content": "out"},
            ],
            "tools": [{"type": "function", "function": {"name": "f", "description": "d", "parameters": {}}}],
        });
        assert_eq!(
            found(Shape::Generate(Dialect::Chat), &body),
            sorted(["s", "u", "think", "{}", "out", "d"])
        );
    }

    #[test]
    fn gemini_reads_both_spellings_and_skips_signatures() {
        let body = json!({
            "system_instruction": {"parts": [{"text": "sys"}]},
            "contents": [
                {"role": "user", "parts": [{"text": "q"}, {"inlineData": {"mimeType": "image/png", "data": "AA"}}]},
                {"role": "model", "parts": [
                    {"text": "t", "thought": true, "thoughtSignature": "sig"},
                    {"functionCall": {"name": "f", "args": {"a": "b"}}, "thoughtSignature": "sig2"},
                ]},
                {"role": "user", "parts": [{"function_response": {"name": "f", "response": {"output": "o"}}}]},
            ],
            "tools": [{"functionDeclarations": [{"name": "f", "description": "d"}]}],
        });
        assert_eq!(
            found(Shape::Generate(Dialect::Gemini), &body),
            sorted(["sys", "q", "t", "b", "o", "d"])
        );
    }

    #[test]
    fn bedrock_covers_reasoning_tools_and_documents() {
        let body = json!({
            "system": [{"text": "sys"}],
            "messages": [
                {"role": "user", "content": [{"text": "q"}, {"document": {"name": "n", "format": "txt", "source": {"text": "doc"}}}]},
                {"role": "assistant", "content": [
                    {"reasoningContent": {"reasoningText": {"text": "hmm", "signature": "sig"}}},
                    {"toolUse": {"toolUseId": "tu_1", "name": "f", "input": {"k": "v"}}},
                ]},
                {"role": "user", "content": [{"toolResult": {"toolUseId": "tu_1", "content": [{"text": "r"}, {"json": {"x": "y"}}]}}]},
            ],
            "toolConfig": {"tools": [{"toolSpec": {"name": "f", "description": "d", "inputSchema": {"json": {}}}}]},
        });
        assert_eq!(
            found(Shape::Generate(Dialect::Bedrock), &body),
            sorted(["sys", "q", "doc", "hmm", "v", "r", "y", "d"])
        );
    }

    #[test]
    fn the_other_shapes() {
        assert_eq!(
            found(
                Shape::Embeddings,
                &json!({"model": "e", "input": ["a", "b"]})
            ),
            sorted(["a", "b"])
        );
        assert_eq!(
            found(
                Shape::Completion,
                &json!({"model": "c", "prompt": "p", "suffix": "s"})
            ),
            sorted(["p", "s"])
        );
        assert_eq!(
            found(
                Shape::GeminiEmbed,
                &json!({"requests": [{"model": "models/e", "content": {"parts": [{"text": "x"}]}}]})
            ),
            sorted(["x"])
        );
        assert_eq!(
            found(
                Shape::RealtimeEvent,
                &json!({"type": "conversation.item.create", "event_id": "evt_1",
                        "item": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}})
            ),
            sorted(["hi"])
        );
    }

    /// 原文里的位置：转义、重复的键、写成转义的键、字符串里原样的换行
    #[test]
    fn ranges_line_up_with_the_raw_text() {
        let text = "{\"input\":[{\"type\":\"message\",\"content\":\"a\\\"b\"}],\"\\u0069nstructions\":\"x\ny\",\"model\":\"m\"}";
        let r = ranges(Shape::Generate(Dialect::Responses), text).unwrap();
        let got: Vec<&str> = r.iter().map(|r| &text[r.clone()]).collect();
        assert_eq!(got, ["a\\\"b", "x\ny"]);
        assert!(ranges(Shape::Generate(Dialect::Chat), "not json").is_none());
        assert_eq!(
            ranges(Shape::Generate(Dialect::Chat), "{\"model\":\"m\"}"),
            Some(Vec::new())
        );
    }
}
