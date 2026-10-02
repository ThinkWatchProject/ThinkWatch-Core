//! 调用方的正文：请求里调用方自己发来的文字 —— 用户消息，连同其中的工具结果。
//!
//! 内容过滤只看这些，删除也只动这些：系统提示是配置网关的人写的，模型自己说的话、
//! 工具定义、图片和文件都不是调用方打的字。
//!
//! # 和中间表示读的是同一批字符串
//!
//! 解码成中间表示时，用户消息里的每一段文字（[`Part::Text`]，连同工具结果里的）都来自
//! 原文里的一个或几个字符串；这里认的就是那几个，判据逐条照着各格式的解码器写。几处
//! 中间表示把几个字符串拼成一段（Anthropic 的 `search_result` 是标题、来源、正文三段，
//! Chat 的工具结果是各块的文字，Gemini 的函数结果和 Bedrock 的 `json` 结果是整个对象
//! 写成的 JSON），这里给的是拼进去的那几个字符串本身。对照测试（`tests/caller.rs`）
//! 钉着两边：四种客户端格式加 Bedrock，中间表示里调用方的每一个字在这里都找得到，
//! 系统提示和模型的话一个都不在。
//!
//! # 为什么不在中间表示上改
//!
//! 同格式直通发的是原文，不经过中间表示 —— `cache_control` 这类中间表示不装的东西
//! 就是这么保住的。要删的字必须删在原文上，所以这里在原始 JSON（[`serde_json::Value`]）
//! 上找。原文里的转义写法（`\u200b`、代理对写成的 `\udb40\udc49`）在解析成 `Value`
//! 时已经还原成字符，这里看到的就是模型会读到的那个字。
//!
//! # 给的是位置
//!
//! [`spots`] 给出每个字符串的位置（[`Spot`]）：读用 [`Spot::get`]，改用
//! [`Spot::get_mut`]。「哪些字符串是调用方的」只写这一遍，先读一遍、决定删什么、
//! 再按同一份位置改。改的只是字符串的内容，位置不会因此失效。
//!
//! [`Part::Text`]: crate::ir::Part::Text

use serde_json::Value;

use crate::ir::Dialect;

/// 从请求体的根往下走的一步。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Key(String),
    Index(usize),
}

/// 调用方正文里的一个字符串在请求体里的位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spot {
    path: Vec<Step>,
    /// 在工具结果里（工具抓回来的网页、读到的文件），而不是调用方自己打的字
    pub in_tool_result: bool,
}

impl Spot {
    /// 从根到这个字符串的路径
    pub fn path(&self) -> &[Step] {
        &self.path
    }

    /// 这个位置上的字符串。请求体换过形状、这里已经不是字符串的是 `None`
    pub fn get<'a>(&self, body: &'a Value) -> Option<&'a str> {
        let mut at = body;
        for s in &self.path {
            at = match s {
                Step::Key(k) => at.get(k.as_str())?,
                Step::Index(i) => at.get(*i)?,
            };
        }
        at.as_str()
    }

    /// 同 [`Spot::get`]，可以改
    pub fn get_mut<'a>(&self, body: &'a mut Value) -> Option<&'a mut String> {
        let mut at = body;
        for s in &self.path {
            at = match s {
                Step::Key(k) => at.get_mut(k.as_str())?,
                Step::Index(i) => at.get_mut(*i)?,
            };
        }
        match at {
            Value::String(s) => Some(s),
            _ => None,
        }
    }
}

/// 一个 `dialect` 格式的请求体里，调用方正文的每一个字符串，按在请求里出现的先后。
/// 空串不算。不是 JSON 对象的请求体没有正文可言。
pub fn spots(dialect: Dialect, body: &Value) -> Vec<Spot> {
    let mut c = Collect::default();
    if body.is_object() {
        match dialect {
            Dialect::Anthropic => anthropic(&mut c, body),
            Dialect::Chat => chat(&mut c, body),
            Dialect::Responses => responses(&mut c, body),
            Dialect::Gemini => gemini(&mut c, body),
            Dialect::Bedrock => bedrock(&mut c, body),
        }
    }
    c.out
}

/// 调用方正文的每一个字符串，连同它在不在工具结果里。
pub fn texts(dialect: Dialect, body: &Value) -> Vec<(&str, bool)> {
    spots(dialect, body)
        .iter()
        .filter_map(|s| Some((s.get(body)?, s.in_tool_result)))
        .collect()
}

/// 逐个改调用方正文的字符串。`f` 拿到字符串和它在不在工具结果里。
pub fn rewrite(dialect: Dialect, body: &mut Value, mut f: impl FnMut(&mut String, bool)) {
    for s in spots(dialect, body) {
        if let Some(t) = s.get_mut(body) {
            f(t, s.in_tool_result);
        }
    }
}

#[derive(Default)]
struct Collect {
    at: Vec<Step>,
    out: Vec<Spot>,
}

impl Collect {
    fn key(&mut self, k: &str, f: impl FnOnce(&mut Self)) {
        self.at.push(Step::Key(k.to_string()));
        f(self);
        self.at.pop();
    }

    fn index(&mut self, i: usize, f: impl FnOnce(&mut Self)) {
        self.at.push(Step::Index(i));
        f(self);
        self.at.pop();
    }

    /// 眼下这个位置上的值是一个非空字符串就记下
    fn here(&mut self, v: &Value, in_tool_result: bool) {
        if v.as_str().is_some_and(|s| !s.is_empty()) {
            self.out.push(Spot {
                path: self.at.clone(),
                in_tool_result,
            });
        }
    }

    /// `v.k` 是一个非空字符串就记下
    fn field(&mut self, v: &Value, k: &str, in_tool_result: bool) {
        if let Some(x) = v.get(k) {
            self.key(k, |c| c.here(x, in_tool_result));
        }
    }

    /// `string | [{text}]` 形状的内容：中间表示用 `text_of` 读它，数组里**每一块**的
    /// `text` 都算，不看块的类型
    fn text_of(&mut self, v: &Value, in_tool_result: bool) {
        match v {
            Value::String(_) => self.here(v, in_tool_result),
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    self.index(i, |c| c.field(item, "text", in_tool_result));
                }
            }
            _ => {}
        }
    }

    /// 一个值里的全部字符串。中间表示把它整个写成 JSON 文字的地方用（Gemini 的函数
    /// 结果、Bedrock 的 `json` 结果）：调用方的字在这些字符串里，键名和 JSON 的标点
    /// 不是谁打的字
    fn leaves(&mut self, v: &Value, in_tool_result: bool) {
        match v {
            Value::String(_) => self.here(v, in_tool_result),
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    self.index(i, |c| c.leaves(item, in_tool_result));
                }
            }
            Value::Object(m) => {
                for (k, item) in m {
                    self.key(k, |c| c.leaves(item, in_tool_result));
                }
            }
            _ => {}
        }
    }
}

fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn each<'a>(v: &'a Value, k: &str) -> impl Iterator<Item = (usize, &'a Value)> {
    v.get(k)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .enumerate()
}

// ───────────────────────────────────────────────────────── Anthropic

/// 见 `anthropic::request::decode_request`：`assistant` 之外、`system` 之外的角色
/// （没写的也算）都是用户；`system` 角色的消息并进了系统提示
fn anthropic(c: &mut Collect, v: &Value) {
    for (i, m) in each(v, "messages") {
        if matches!(str_of(m, "role"), Some("assistant" | "system")) {
            continue;
        }
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
}

/// 用户消息里的一块（`anthropic::request` 的 `block`）
fn anthropic_block(c: &mut Collect, b: &Value) {
    match str_of(b, "type").unwrap_or("") {
        "text" => c.field(b, "text", false),
        "document" => anthropic_document(c, b, false),
        "search_result" => anthropic_search_result(c, b, false),
        "tool_result" => {
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

/// `document` 块：`text` 来源的 `data`，`content` 来源的各块文字。base64 和 URL
/// 来源是文件，不是文字
fn anthropic_document(c: &mut Collect, b: &Value, in_tool_result: bool) {
    let Some(src) = b.get("source") else {
        return;
    };
    c.key("source", |c| match str_of(src, "type") {
        Some("text") => c.field(src, "data", in_tool_result),
        Some("content") => {
            if let Some(content) = src.get("content") {
                c.key("content", |c| c.text_of(content, in_tool_result));
            }
        }
        _ => {}
    });
}

/// `search_result` 块：中间表示里是「标题、来源、正文」三段拼成的一段文字
fn anthropic_search_result(c: &mut Collect, b: &Value, in_tool_result: bool) {
    c.field(b, "title", in_tool_result);
    c.field(b, "source", in_tool_result);
    if let Some(content) = b.get("content") {
        c.key("content", |c| c.text_of(content, in_tool_result));
    }
}

// ───────────────────────────────────────────────────────── Chat

/// 见 `chat::request::decode_request`：`user` 的文字块，和 `tool` 消息（工具结果）的
/// 内容
fn chat(c: &mut Collect, v: &Value) {
    for (i, m) in each(v, "messages") {
        let Some(content) = m.get("content") else {
            continue;
        };
        let role = str_of(m, "role").unwrap_or("");
        if role != "user" && role != "tool" {
            continue;
        }
        c.key("messages", |c| {
            c.index(i, |c| {
                c.key("content", |c| {
                    if role == "tool" {
                        c.text_of(content, true);
                        return;
                    }
                    match content {
                        Value::String(_) => c.here(content, false),
                        Value::Array(items) => {
                            for (j, p) in items.iter().enumerate() {
                                if str_of(p, "type") == Some("text") {
                                    c.index(j, |c| c.field(p, "text", false));
                                }
                            }
                        }
                        _ => {}
                    }
                })
            })
        });
    }
}

// ───────────────────────────────────────────────────────── Responses

/// 见 `responses::request::decode_request`：`input` 是一个字符串时整个是用户的话；
/// 是数组时看 `message`（`system`、`developer`、`assistant` 之外的角色，没写的算
/// `user`）和两种工具结果
fn responses(c: &mut Collect, v: &Value) {
    let Some(input) = v.get("input") else {
        return;
    };
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

fn responses_item(c: &mut Collect, item: &Value) {
    match str_of(item, "type").unwrap_or("message") {
        "message" => {
            if matches!(
                str_of(item, "role").unwrap_or("user"),
                "system" | "developer" | "assistant"
            ) {
                return;
            }
            if let Some(content) = item.get("content") {
                c.key("content", |c| responses_content(c, content, false));
            }
        }
        "function_call_output" | "custom_tool_call_output" => {
            if let Some(output) = item.get("output") {
                c.key("output", |c| responses_content(c, output, true));
            }
        }
        _ => {}
    }
}

/// `responses::request` 的 `content_parts`：字符串，或者各块里的 `input_text` /
/// `output_text` 的 `text`、`refusal` 的 `refusal`
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

// ───────────────────────────────────────────────────────── Gemini

/// Gemini 的字段名驼峰和下划线写法都认（`gemini::request::field`）。这个值写在哪个
/// 键下：先驼峰，再下划线
fn gemini_key(v: &Value, camel: &str) -> Option<String> {
    if v.get(camel).is_some() {
        return Some(camel.to_string());
    }
    let mut snake = String::with_capacity(camel.len() + 4);
    for ch in camel.chars() {
        if ch.is_ascii_uppercase() {
            snake.push('_');
            snake.push(ch.to_ascii_lowercase());
        } else {
            snake.push(ch);
        }
    }
    v.get(&snake).is_some().then_some(snake)
}

/// 见 `gemini::request::decode_request`：`model` 之外的角色（没写的也算）是用户。
/// 一块按 `text`、`inlineData`、`functionCall`、`functionResponse` 的先后认，`text`
/// 带 `thought: true` 的是推理
fn gemini(c: &mut Collect, v: &Value) {
    for (i, content) in each(v, "contents") {
        if str_of(content, "role") == Some("model") {
            continue;
        }
        c.key("contents", |c| {
            c.index(i, |c| {
                c.key("parts", |c| {
                    for (j, p) in each(content, "parts") {
                        c.index(j, |c| gemini_part(c, p));
                    }
                })
            })
        });
    }
}

fn gemini_part(c: &mut Collect, p: &Value) {
    if str_of(p, "text").is_some() {
        if p.get("thought").and_then(Value::as_bool) != Some(true) {
            c.field(p, "text", false);
        }
        return;
    }
    if gemini_key(p, "inlineData").is_some() || gemini_key(p, "functionCall").is_some() {
        return;
    }
    let Some(key) = gemini_key(p, "functionResponse") else {
        return;
    };
    let resp = &p[key.as_str()];
    let Some(body) = resp.get("response") else {
        return;
    };
    c.key(&key, |c| {
        c.key("response", |c| {
            // `gemini::request` 的 `response_text`：只有一个 output / result / content /
            // error 字符串时取它本身，否则整个对象写成 JSON
            if let Some(o) = body.as_object()
                && o.len() == 1
                && let Some(k) = ["output", "result", "content", "error"]
                    .into_iter()
                    .find(|k| o.get(*k).is_some_and(Value::is_string))
            {
                c.field(body, k, true);
                return;
            }
            c.leaves(body, true);
        })
    });
}

// ───────────────────────────────────────────────────────── Bedrock

/// 见 `bedrock::request::decode_request`：`assistant` 之外的角色是用户。一块按
/// `cachePoint`、`text`、`image`、`document`、`reasoningContent`、`toolUse`、
/// `toolResult` 的先后认
fn bedrock(c: &mut Collect, v: &Value) {
    for (i, m) in each(v, "messages") {
        if str_of(m, "role") == Some("assistant") {
            continue;
        }
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
}

fn bedrock_block(c: &mut Collect, b: &Value) {
    if b.get("cachePoint").is_some() {
        return;
    }
    if str_of(b, "text").is_some() {
        c.field(b, "text", false);
        return;
    }
    if b.get("image").is_some() {
        return;
    }
    if let Some(doc) = b.get("document") {
        let Some(src) = doc.get("source") else {
            return;
        };
        if str_of(src, "bytes").is_none() {
            c.key("document", |c| {
                c.key("source", |c| c.field(src, "text", false))
            });
        }
        return;
    }
    if b.get("reasoningContent").is_some() || b.get("toolUse").is_some() {
        return;
    }
    let Some(res) = b.get("toolResult") else {
        return;
    };
    c.key("toolResult", |c| {
        c.key("content", |c| {
            for (k, item) in each(res, "content") {
                c.index(k, |c| {
                    if str_of(item, "text").is_some() {
                        c.field(item, "text", true);
                    } else if let Some(j) = item.get("json") {
                        c.key("json", |c| c.leaves(j, true));
                    }
                });
            }
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_spot_reads_and_writes_the_same_string() {
        let mut v = json!({"messages": [
            {"role": "system", "content": "sys"},
            {"role": "user", "content": [{"type": "text", "text": "hello"}]},
        ]});
        let s = spots(Dialect::Chat, &v);
        assert_eq!(s.len(), 1);
        assert_eq!(
            s[0].path(),
            [
                Step::Key("messages".into()),
                Step::Index(1),
                Step::Key("content".into()),
                Step::Index(0),
                Step::Key("text".into()),
            ]
        );
        assert_eq!(s[0].get(&v), Some("hello"));
        s[0].get_mut(&mut v).unwrap().push('!');
        assert_eq!(v["messages"][1]["content"][0]["text"], "hello!");
        // 换了形状的请求体：那里已经不是字符串
        assert!(s[0].get(&json!({"messages": []})).is_none());
    }

    #[test]
    fn a_body_that_is_not_an_object_has_no_caller_text() {
        for d in [
            Dialect::Anthropic,
            Dialect::Chat,
            Dialect::Responses,
            Dialect::Gemini,
            Dialect::Bedrock,
        ] {
            assert!(spots(d, &json!(["hi"])).is_empty());
            assert!(spots(d, &json!("hi")).is_empty());
        }
    }

    #[test]
    fn rewrite_changes_only_the_callers_strings() {
        let mut v = json!({
            "system": "keep",
            "messages": [
                {"role": "user", "content": "drop"},
                {"role": "assistant", "content": "keep"},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": "drop"}]},
            ]
        });
        let mut seen = Vec::new();
        rewrite(Dialect::Anthropic, &mut v, |s, tool| {
            seen.push(tool);
            *s = s.replace("drop", "");
        });
        assert_eq!(seen, [false, true]);
        assert_eq!(v["system"], "keep");
        assert_eq!(v["messages"][0]["content"], "");
        assert_eq!(v["messages"][1]["content"], "keep");
        assert_eq!(v["messages"][2]["content"][0]["content"], "");
    }

    #[test]
    fn gemini_reads_snake_case_keys_as_the_decoder_does() {
        let v = json!({"contents": [{"role": "user", "parts": [
            {"function_response": {"name": "f", "response": {"output": "tool says"}}},
        ]}]});
        assert_eq!(texts(Dialect::Gemini, &v), [("tool says", true)]);
    }
}
