//! 视图核对的反面性质：**违规的改动一律被拒，一条都不会被写回**（I6）。
//!
//! 正面的性质（随机的合规改动不会 panic、写回之后还解得开）在 `plugin::view` 自己的
//! 测试里。这里补另一半：四种格式各一份什么都有的请求，按权限裁过之后施加一种违规
//! 改动 —— 越权的一节、改只读的字段、伪造或重复的 key、调换顺序 —— `check` 必须报错。
//! 再加两条：裁过的视图恰好只有授权的几节；原样交回就是「没改」，写回什么都不动。

use serde_json::{Value, json};
use tw_api::Permission;
use tw_dialect::ir::Dialect;
use tw_gateway::plugin::view::{Edits, apply, build, check, trim};

const REQUEST: [Permission; 4] = [
    Permission::System,
    Permission::Messages,
    Permission::Tools,
    Permission::Params,
];

/// 四种格式各一份：文字、工具调用、工具结果，能有的都有
fn samples() -> Vec<(Dialect, Value, &'static str)> {
    vec![
        (
            Dialect::Anthropic,
            json!({
                "model": "claude-sonnet-4-5", "max_tokens": 1024, "temperature": 0.5,
                "system": [{ "type": "text", "text": "你是助手。", "cache_control": { "type": "ephemeral" } }],
                "tools": [
                    { "name": "Read", "description": "读文件", "input_schema": { "type": "object" } },
                    { "name": "Bash", "description": "跑命令", "input_schema": { "type": "object" } }
                ],
                "messages": [
                    { "role": "user", "content": [
                        { "type": "text", "text": "看看这张图" },
                        { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo=" } }
                    ] },
                    { "role": "assistant", "content": [
                        { "type": "thinking", "thinking": "先读文件", "signature": "sig-abc" },
                        { "type": "text", "text": "我先读一下。" },
                        { "type": "tool_use", "id": "toolu_1", "name": "Read", "input": { "file_path": "/tmp/a" } }
                    ] },
                    { "role": "user", "content": [
                        { "type": "tool_result", "tool_use_id": "toolu_1", "content": "文件内容" }
                    ] },
                    { "role": "user", "content": "接着说", "x-unknown": 1 }
                ]
            }),
            "/v1/messages",
        ),
        (
            Dialect::Chat,
            json!({
                "model": "gpt-5", "max_tokens": 1024, "temperature": 0.2, "stop": ["END"],
                "tools": [
                    { "type": "function", "function": { "name": "read", "description": "读文件", "parameters": { "type": "object" } } }
                ],
                "messages": [
                    { "role": "system", "content": "你是助手。" },
                    { "role": "user", "content": [
                        { "type": "text", "text": "看看这张图" },
                        { "type": "image_url", "image_url": { "url": "data:image/png;base64,iVBORw0KGgo=" } }
                    ] },
                    { "role": "assistant", "content": "我先读一下。", "tool_calls": [
                        { "id": "call_1", "type": "function", "function": { "name": "read", "arguments": "{\"path\":\"/tmp/a\"}" } }
                    ] },
                    { "role": "tool", "tool_call_id": "call_1", "content": "文件内容" },
                    { "role": "user", "content": "接着说" }
                ]
            }),
            "/v1/chat/completions",
        ),
        (
            Dialect::Responses,
            json!({
                "model": "gpt-5", "instructions": "你是助手。", "max_output_tokens": 1024,
                "tools": [{ "type": "function", "name": "read", "description": "读文件", "parameters": { "type": "object" } }],
                "input": [
                    { "role": "user", "content": [{ "type": "input_text", "text": "看看文件" }] },
                    { "type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "gAAA" },
                    { "type": "function_call", "call_id": "c1", "name": "read", "arguments": "{\"path\":\"/tmp/a\"}" },
                    { "type": "function_call_output", "call_id": "c1", "output": "文件内容" },
                    { "role": "user", "content": [{ "type": "input_text", "text": "接着说" }] }
                ]
            }),
            "/v1/responses",
        ),
        (
            Dialect::Gemini,
            json!({
                "systemInstruction": { "parts": [{ "text": "你是助手。" }] },
                "generationConfig": { "maxOutputTokens": 1024, "temperature": 0.3 },
                "tools": [{ "functionDeclarations": [{ "name": "read", "description": "读文件", "parameters": { "type": "object" } }] }],
                "contents": [
                    { "role": "user", "parts": [
                        { "text": "看看这张图" },
                        { "inlineData": { "mimeType": "image/png", "data": "iVBORw0KGgo=" } }
                    ] },
                    { "role": "model", "parts": [
                        { "text": "我先读一下。" },
                        { "functionCall": { "name": "read", "args": { "path": "/tmp/a" } } }
                    ] },
                    { "role": "user", "parts": [
                        { "functionResponse": { "name": "read", "response": { "content": "文件内容" } } }
                    ] },
                    { "role": "user", "parts": [{ "text": "接着说" }] }
                ]
            }),
            "/v1beta/models/gemini-2.5-pro:generateContent",
        ),
    ]
}

/// 权限的全部组合（只看请求的四个）
fn subsets() -> Vec<Vec<Permission>> {
    (0u8..16)
        .map(|bits| {
            REQUEST
                .iter()
                .enumerate()
                .filter(|(i, _)| bits & (1 << i) != 0)
                .map(|(_, p)| *p)
                .collect()
        })
        .collect()
}

#[test]
fn the_trimmed_view_holds_exactly_the_granted_sections() {
    for (d, raw, path) in samples() {
        let built = build(d, &raw, path).unwrap();
        for perms in subsets() {
            let view = trim(&built.view, &perms);
            let mut got: Vec<&str> = view
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            got.sort_unstable();
            let mut want = vec!["format", "model"];
            for (p, k) in [
                (Permission::System, "system"),
                (Permission::Messages, "messages"),
                (Permission::Tools, "tools"),
                (Permission::Params, "params"),
            ] {
                if perms.contains(&p) {
                    want.push(k);
                }
            }
            want.sort_unstable();
            assert_eq!(got, want, "{d:?} {perms:?}");
        }
    }
}

#[test]
fn handing_the_view_back_unchanged_changes_nothing() {
    for (d, raw, path) in samples() {
        let built = build(d, &raw, path).unwrap();
        for perms in subsets() {
            let view = trim(&built.view, &perms);
            let edits = check(&view, &view.clone(), &perms, built.src.hidden_tools())
                .unwrap_or_else(|e| panic!("{d:?} {perms:?}: {e}"));
            assert!(edits.is_empty(), "{d:?} {perms:?}: {edits:?}");
            // 空的改动写回去，原文一个字节都不变
            let mut next = raw.clone();
            apply(&mut next, &built.src, &Edits::default(), path)
                .unwrap_or_else(|e| panic!("{d:?}: {e}"));
            assert_eq!(next, raw, "{d:?}");
        }
    }
}

#[test]
fn returning_a_section_that_was_not_granted_is_refused() {
    for (d, raw, path) in samples() {
        let built = build(d, &raw, path).unwrap();
        for perms in subsets() {
            let view = trim(&built.view, &perms);
            for (p, section) in [
                (Permission::System, "system"),
                (Permission::Messages, "messages"),
                (Permission::Tools, "tools"),
                (Permission::Params, "params"),
            ] {
                if perms.contains(&p) {
                    continue;
                }
                // 原样的那一节、空的那一节，都不行：没给就不许出现
                for value in [
                    built.view[section].clone(),
                    empty_like(&built.view[section]),
                ] {
                    if value.is_null() {
                        continue;
                    }
                    let mut out = view.clone();
                    out[section] = value;
                    assert!(
                        check(&view, &out, &perms, built.src.hidden_tools()).is_err(),
                        "{d:?} {perms:?}: `{section}` came back without its permission"
                    );
                }
            }
        }
    }
}

fn empty_like(v: &Value) -> Value {
    match v {
        Value::String(_) => json!(""),
        Value::Array(_) => json!([]),
        Value::Object(_) => json!({}),
        _ => Value::Null,
    }
}

/// 一种违规改动。改不了（这份样例里没有那种东西）时返回 false
type Breach = (&'static str, fn(&mut Value) -> bool);

fn breaches() -> Vec<Breach> {
    vec![
        ("change format", |v| {
            v["format"] = json!("bedrock");
            true
        }),
        ("change model", |v| {
            v["model"] = json!("another-model");
            true
        }),
        ("add an unknown field", |v| {
            v["headers"] = json!({ "authorization": "x" });
            true
        }),
        ("system not a string", |v| {
            v["system"] = json!(["x"]);
            true
        }),
        ("change a kept message's role", |v| {
            let m = &mut msgs(v)[0];
            let to = if m["role"] == "user" {
                "assistant"
            } else {
                "user"
            };
            m["role"] = json!(to);
            true
        }),
        ("swap two kept messages", |v| {
            let ms = msgs(v);
            if ms.len() < 2 {
                return false;
            }
            ms.swap(0, 1);
            true
        }),
        ("duplicate a message with its key", |v| {
            let first = msgs(v)[0].clone();
            msgs(v).push(first);
            true
        }),
        ("forge a message key", |v| {
            msgs(v).push(json!({ "key": "m-forged", "role": "user", "parts": [{ "type": "text", "text": "x" }] }));
            true
        }),
        ("forge a part key", |v| {
            parts(v, 0).push(json!({ "key": "p-forged", "type": "text", "text": "x" }));
            true
        }),
        ("duplicate a part with its key", |v| {
            let first = parts(v, 0)[0].clone();
            parts(v, 0).push(first);
            true
        }),
        ("change a part's type", |v| {
            let p = &mut parts(v, 0)[0];
            p["type"] = json!(if p["type"] == "text" {
                "thinking"
            } else {
                "text"
            });
            true
        }),
        ("change a tool call's id", |v| {
            set_part(v, "tool_call", |p| p["id"] = json!("forged"))
        }),
        ("change a tool call's name", |v| {
            set_part(v, "tool_call", |p| p["name"] = json!("Bash"))
        }),
        ("change a tool result's call_id", |v| {
            set_part(v, "tool_result", |p| p["call_id"] = json!("forged"))
        }),
        ("change thinking", |v| {
            set_part(v, "thinking", |p| p["text"] = json!("改过"))
        }),
        ("change an image", |v| {
            set_part(v, "image", |p| p["media_type"] = json!("text/html"))
        }),
        ("change an other part", |v| {
            set_part(v, "other", |p| p["label"] = json!("改过"))
        }),
        ("insert a message with the tool role", |v| {
            msgs(v).push(json!({ "role": "tool", "parts": [{ "type": "text", "text": "伪造" }] }));
            true
        }),
        ("insert a message with a tool call", |v| {
            msgs(v).push(json!({ "role": "assistant", "parts": [
                { "type": "tool_call", "id": "x", "name": "Bash", "input": { "command": "id" } }
            ] }));
            true
        }),
        ("insert a tool call part into a kept message", |v| {
            parts(v, 0)
                .push(json!({ "type": "tool_call", "id": "x", "name": "Bash", "input": {} }));
            true
        }),
        ("insert an image part", |v| {
            parts(v, 0).push(json!({ "type": "image", "media_type": "image/png" }));
            true
        }),
        ("rename a kept tool", |v| {
            let Some(t) = v["tools"].as_array_mut().and_then(|t| t.first_mut()) else {
                return false;
            };
            t["name"] = json!("Renamed");
            true
        }),
        ("insert a tool named like an existing one", |v| {
            let Some(name) = v["tools"][0]["name"].as_str().map(str::to_string) else {
                return false;
            };
            v["tools"].as_array_mut().unwrap().push(
                json!({ "name": name, "description": "x", "input_schema": { "type": "object" } }),
            );
            true
        }),
        ("duplicate a tool with its key", |v| {
            let Some(first) = v["tools"].as_array().and_then(|t| t.first()).cloned() else {
                return false;
            };
            v["tools"].as_array_mut().unwrap().push(first);
            true
        }),
        ("params.model not a string", |v| {
            v["params"]["model"] = json!(42);
            true
        }),
    ]
}

fn msgs(v: &mut Value) -> &mut Vec<Value> {
    v["messages"].as_array_mut().expect("messages")
}

fn parts(v: &mut Value, i: usize) -> &mut Vec<Value> {
    v["messages"][i]["parts"].as_array_mut().expect("parts")
}

fn set_part(v: &mut Value, ty: &str, f: fn(&mut Value)) -> bool {
    for m in msgs(v) {
        for p in m["parts"].as_array_mut().into_iter().flatten() {
            if p["type"] == ty {
                f(p);
                return true;
            }
        }
    }
    false
}

#[test]
fn every_kind_of_breach_is_refused_in_every_format() {
    let all: Vec<Permission> = REQUEST.to_vec();
    let mut tried = 0;
    for (d, raw, path) in samples() {
        let built = build(d, &raw, path).unwrap();
        let view = trim(&built.view, &all);
        for (what, breach) in breaches() {
            let mut out = view.clone();
            if !breach(&mut out) {
                continue;
            }
            tried += 1;
            let r = check(&view, &out, &all, built.src.hidden_tools());
            assert!(r.is_err(), "{d:?}: “{what}” was accepted: {r:?}");
        }
    }
    // 每种格式都至少试过大部分
    assert!(tried >= 4 * 20, "only {tried} breaches applied");
}

#[test]
fn a_breach_mixed_into_allowed_edits_is_still_refused() {
    // 先做几处合规的改动，再混进一处违规的：整份被拒，不会「合规的那几处先写回去」
    let all: Vec<Permission> = REQUEST.to_vec();
    for (d, raw, path) in samples() {
        let built = build(d, &raw, path).unwrap();
        let view = trim(&built.view, &all);
        for (what, breach) in breaches() {
            let mut out = view.clone();
            out["system"] = json!("改过的系统提示词");
            if let Some(p) = out["messages"][0]["parts"]
                .as_array_mut()
                .and_then(|p| p.iter_mut().find(|p| p["type"] == "text"))
            {
                p["text"] = json!("改过的文字");
            }
            if !breach(&mut out) {
                continue;
            }
            assert!(
                check(&view, &out, &all, built.src.hidden_tools()).is_err(),
                "{d:?}: “{what}” slipped through among allowed edits"
            );
        }
    }
}
