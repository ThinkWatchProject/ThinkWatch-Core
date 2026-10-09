//! 对照：[`tw_dialect::caller`] 在原文上认的调用方正文，和解码成中间表示时用户消息里的
//! 文字是同一批字符串。
//!
//! 每种格式一份把各处都写到的请求，往每个字符串里放一个记号：`«u…»` 是调用方打的字，
//! `«t…»` 是工具结果，`«s…»` 是系统提示，`«a…»` 是模型的话，`«x…»` 是别的（工具定义、
//! 图片数据、工具调用的参数、推理）。两边各找一遍记号，找到的必须一样多、一样在不在工具
//! 结果里，而且正好是 `u` 和 `t` 那几个。再把原文里调用方正文的记号全删掉、重新解码：
//! 中间表示里调用方的话一个记号都不剩，系统提示和模型的话原样都在 —— 删也删在同一批
//! 字符串上。
//!
//! 一处说明：中间表示把 Gemini 的函数结果、Bedrock 的 `json` 结果整个写成 JSON 文字，
//! 键名也在里面；这里认的是其中的字符串。记号不放在键名里。

use std::collections::BTreeSet;

use serde_json::{Value, json};
use tw_dialect::caller;
use tw_dialect::convert::decode;
use tw_dialect::ir::{Dialect, Part, Role};

type Marks = BTreeSet<(String, bool)>;

fn marks_in(text: &str, in_tool_result: bool, out: &mut Marks) {
    let mut rest = text;
    while let Some(i) = rest.find('«') {
        let tail = &rest[i..];
        let Some(j) = tail.find('»') else { break };
        out.insert((tail[..j + '»'.len_utf8()].to_string(), in_tool_result));
        rest = &tail[j..];
    }
}

fn path(d: Dialect) -> &'static str {
    match d {
        Dialect::Gemini => "/v1beta/models/m:generateContent",
        Dialect::Bedrock => "/model/m/converse",
        _ => "/",
    }
}

fn parts(ps: &[Part], in_tool_result: bool, out: &mut Marks) {
    for p in ps {
        match p {
            Part::Text(t) => marks_in(t, in_tool_result, out),
            Part::ToolResult(r) => parts(&r.content, true, out),
            _ => {}
        }
    }
}

/// 中间表示里用户消息的文字（连同工具结果）带着的记号
fn decoded(d: Dialect, v: &Value) -> Marks {
    let r = decode(d, v, path(d), None).expect("decodes").request;
    let mut out = Marks::new();
    for m in r.messages.iter().filter(|m| m.role == Role::User) {
        parts(&m.parts, false, &mut out);
    }
    out
}

/// 中间表示里系统提示和模型的话带着的记号
fn not_callers(d: Dialect, v: &Value) -> BTreeSet<String> {
    let r = decode(d, v, path(d), None).expect("decodes").request;
    let mut out = Marks::new();
    for s in &r.system {
        marks_in(s, false, &mut out);
    }
    for m in r.messages.iter().filter(|m| m.role == Role::Assistant) {
        for p in &m.parts {
            match p {
                Part::Text(t) => marks_in(t, false, &mut out),
                Part::Thinking(t) => marks_in(&t.text, false, &mut out),
                _ => {}
            }
        }
    }
    out.into_iter().map(|(m, _)| m).collect()
}

/// 原文上认出的调用方正文带着的记号
fn walked(d: Dialect, v: &Value) -> Marks {
    let mut out = Marks::new();
    for (t, in_tool_result) in caller::texts(d, v) {
        marks_in(t, in_tool_result, &mut out);
    }
    out
}

fn expect(users: &[&str], tools: &[&str]) -> Marks {
    users
        .iter()
        .map(|m| (format!("«{m}»"), false))
        .chain(tools.iter().map(|m| (format!("«{m}»"), true)))
        .collect()
}

/// 两边一样，正好是这些；删掉之后中间表示里调用方的记号全没了，别的都在
fn check(d: Dialect, v: Value, users: &[&str], tools: &[&str]) {
    let want = expect(users, tools);
    assert_eq!(decoded(d, &v), want, "{d:?}: 中间表示");
    assert_eq!(walked(d, &v), want, "{d:?}: 原文");

    let kept = not_callers(d, &v);
    assert!(!kept.is_empty(), "{d:?}: 例子里该有系统提示或模型的话");
    let mut stripped = v.clone();
    caller::rewrite(d, &mut stripped, |s, _| {
        while let (Some(i), Some(j)) = (s.find('«'), s.find('»')) {
            s.replace_range(i..j + '»'.len_utf8(), "");
        }
    });
    assert!(decoded(d, &stripped).is_empty(), "{d:?}: 删完还有");
    assert_eq!(not_callers(d, &stripped), kept, "{d:?}: 删到了别处");
    // 别的地方（工具定义、图片、调用参数）也原样在
    let others = |v: &Value| v.to_string().matches("«x").count();
    assert_eq!(others(&stripped), others(&v), "{d:?}");
}

#[test]
fn anthropic_messages() {
    let v = json!({
        "model": "m",
        "system": [{"type": "text", "text": "«s1»"}],
        "tools": [{"name": "f", "description": "«x1»", "input_schema": {"type": "object"}}],
        "messages": [
            {"role": "user", "content": "«u1» plain"},
            {"role": "system", "content": "«s2» a system turn"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "«a1»"},
                {"type": "thinking", "thinking": "«a2»", "signature": "sig"},
                {"type": "tool_use", "id": "t1", "name": "f", "input": {"q": "«x2»"}},
            ]},
            {"role": "user", "content": [
                {"type": "text", "text": "«u2»"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "«x3»"}},
                {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "«u3»"}},
                {"type": "document", "source": {"type": "content", "content": [{"type": "text", "text": "«u4»"}]}},
                {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "«x4»"}},
                {"type": "search_result", "title": "«u5»", "source": "«u6»",
                    "content": [{"type": "text", "text": "«u7»"}]},
                {"type": "tool_result", "tool_use_id": "t1", "content": "«t1»"},
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "text", "text": "«t2»"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "«x5»"}},
                    {"type": "document", "source": {"type": "text", "data": "«t3»"}},
                    {"type": "search_result", "title": "«t4»", "source": "«t5»", "content": "«t6»"},
                ]},
            ]},
            {"content": "«u8» a turn without a role"},
        ]
    });
    check(
        Dialect::Anthropic,
        v,
        &["u1", "u2", "u3", "u4", "u5", "u6", "u7", "u8"],
        &["t1", "t2", "t3", "t4", "t5", "t6"],
    );
}

#[test]
fn chat_completions() {
    let v = json!({
        "model": "m",
        "messages": [
            {"role": "system", "content": "«s1»"},
            {"role": "developer", "content": [{"type": "text", "text": "«s2»"}]},
            {"role": "user", "content": "«u1»"},
            {"role": "user", "content": [
                {"type": "text", "text": "«u2»"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,«x1»"}},
                {"type": "file", "file": {"file_data": "«x2»", "filename": "a.pdf"}},
            ]},
            {"role": "assistant", "content": "«a1»", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{\"q\":\"«x3»\"}"}},
            ]},
            {"role": "tool", "tool_call_id": "c1", "content": "«t1»"},
            {"role": "tool", "tool_call_id": "c1", "content": [
                {"type": "text", "text": "«t2»"},
                {"type": "text", "text": "«t3»"},
            ]},
            {"role": "function", "content": "«x4»"},
        ],
        "tools": [{"type": "function", "function": {"name": "f", "description": "«x5»", "parameters": {}}}],
    });
    check(Dialect::Chat, v, &["u1", "u2"], &["t1", "t2", "t3"]);
}

#[test]
fn responses() {
    let v = json!({
        "model": "m",
        "instructions": "«s1»",
        "input": [
            {"type": "message", "role": "developer", "content": "«s2»"},
            {"role": "system", "content": [{"type": "input_text", "text": "«s3»"}]},
            {"role": "user", "content": "«u1»"},
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "«u2»"},
                {"type": "input_image", "image_url": "data:image/png;base64,«x1»"},
                {"type": "refusal", "refusal": "«u3»"},
                {"type": "output_text", "text": "«u4»"},
            ]},
            {"content": "«u5» neither a type nor a role"},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "«a1»"}]},
            {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{\"q\":\"«x2»\"}"},
            {"type": "function_call_output", "call_id": "c1", "output": "«t1»"},
            {"type": "custom_tool_call", "call_id": "c2", "name": "g", "input": "«x3»"},
            {"type": "custom_tool_call_output", "call_id": "c2", "output": [
                {"type": "input_text", "text": "«t2»"},
                {"type": "input_image", "image_url": "data:image/png;base64,«x4»"},
            ]},
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "«a2»"}]},
            // Codex 的 Responses Lite：工具声明在 input 里，namespace 的说明进系统提示
            {"type": "additional_tools", "role": "developer", "tools": [
                {"type": "namespace", "name": "mcp__x", "description": "«s4»", "tools": [
                    {"type": "function", "name": "h", "description": "«x6»", "parameters": {}},
                ]},
                {"type": "tool_search", "execution": "client", "description": "«x7»", "parameters": {}},
            ]},
            {"type": "local_shell_call", "call_id": "c3", "status": "completed",
                "action": {"type": "exec", "command": ["echo", "«x8»"]}},
            {"type": "function_call_output", "call_id": "c3", "output": "«t3»"},
            {"type": "tool_search_call", "call_id": "c4", "execution": "client", "arguments": {"query": "«x9»"}},
            {"type": "tool_search_output", "call_id": "c4", "status": "completed", "execution": "client", "tools": [
                {"type": "function", "name": "k", "description": "«x10»", "parameters": {}},
            ]},
            // 别的代理发来的话：读得懂的那段是调用方的话
            {"type": "agent_message", "author": "/root", "recipient": "/root/w", "content": [
                {"type": "input_text", "text": "«u6»"},
                {"type": "encrypted_content", "encrypted_content": "«x11»"},
            ]},
            {"type": "configuration_update", "reasoning": {"effort": "high"}},
        ],
        "tools": [
            {"type": "function", "name": "f", "description": "«x5»", "parameters": {}},
            {"type": "custom", "name": "g"},
        ],
    });
    check(
        Dialect::Responses,
        v,
        &["u1", "u2", "u3", "u4", "u5", "u6"],
        &["t1", "t2", "t3"],
    );
}

#[test]
fn responses_with_a_plain_string_input() {
    let v = json!({"model": "m", "instructions": "«s1»", "input": "«u1»"});
    check(Dialect::Responses, v, &["u1"], &[]);
}

#[test]
fn gemini() {
    let v = json!({
        "systemInstruction": {"parts": [{"text": "«s1»"}]},
        "contents": [
            {"role": "user", "parts": [
                {"text": "«u1»"},
                {"inlineData": {"mimeType": "image/png", "data": "«x1»"}},
            ]},
            {"role": "model", "parts": [
                {"text": "«a1»"},
                {"text": "«a2»", "thought": true},
                {"functionCall": {"name": "f", "args": {"q": "«x2»"}}},
            ]},
            {"role": "user", "parts": [{"functionResponse": {"name": "f", "response": {"output": "«t1»"}}}]},
            {"role": "function", "parts": [{"function_response": {"name": "f",
                "response": {"rows": [{"name": "«t2»"}, {"name": "«t3»"}], "n": 3}}}]},
            {"parts": [
                {"text": "«u2» a turn without a role"},
                {"text": "«x3» reasoning in a user turn is not the caller's text", "thought": true},
            ]},
            {"role": "user", "parts": [{"functionResponse": {"name": "f", "response": "«t4»"}}]},
        ],
        "tools": [{"functionDeclarations": [{"name": "f", "description": "«x4»"}]}],
    });
    check(Dialect::Gemini, v, &["u1", "u2"], &["t1", "t2", "t3", "t4"]);
}

#[test]
fn bedrock_converse() {
    let v = json!({
        "system": [{"text": "«s1»"}, {"cachePoint": {"type": "default"}}],
        "messages": [
            {"role": "user", "content": [
                {"text": "«u1»"},
                {"image": {"format": "png", "source": {"bytes": "«x1»"}}},
                {"document": {"format": "txt", "name": "d", "source": {"text": "«u2»"}}},
                {"document": {"format": "pdf", "name": "p", "source": {"bytes": "«x2»"}}},
                {"cachePoint": {"type": "default"}},
            ]},
            {"role": "assistant", "content": [
                {"text": "«a1»"},
                {"toolUse": {"toolUseId": "t1", "name": "f", "input": {"q": "«x3»"}}},
                {"reasoningContent": {"reasoningText": {"text": "«a2»"}}},
            ]},
            {"role": "user", "content": [{"toolResult": {"toolUseId": "t1", "content": [
                {"text": "«t1»"},
                {"json": {"rows": ["«t2»"]}},
                {"image": {"format": "png", "source": {"bytes": "«x4»"}}},
            ]}}]},
        ],
        "toolConfig": {"tools": [{"toolSpec": {"name": "f", "description": "«x5»",
            "inputSchema": {"json": {}}}}]},
    });
    check(Dialect::Bedrock, v, &["u1", "u2"], &["t1", "t2"]);
}

/// 原文里写成转义的字（`«`、代理对写的标签字符）解析之后就是那个字：两边看到的
/// 都是模型会读到的字，不是转义序列
#[test]
fn escaped_characters_are_read_as_the_characters_they_stand_for() {
    // 反斜杠由 `char::from(92)` 拼：直接写出来的转义，经过某些编辑工具会变成真字符
    let b = char::from(92);
    let raw = format!(
        r#"{{"model":"m","messages":[{{"role":"user","content":"{b}u00abu1{b}u00bb hidden:{b}udb40{b}udc49"}}]}}"#
    );
    assert!(raw.is_ascii(), "原文里只有转义，没有真字符");
    let v: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(walked(Dialect::Anthropic, &v), expect(&["u1"], &[]));
    let texts = caller::texts(Dialect::Anthropic, &v);
    assert!(texts[0].0.ends_with('\u{E0049}'), "{:?}", texts[0].0);
}
