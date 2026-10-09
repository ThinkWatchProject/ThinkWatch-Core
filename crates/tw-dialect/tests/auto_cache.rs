//! 转给 Claude（Anthropic 格式、Bedrock 上的 Claude）时，客户端没标的提示缓存断点替它标上。
//!
//! Codex 这类说 OpenAI 格式的客户端没有断点的写法：OpenAI 自己缓存开头相同的部分。转给
//! Anthropic 时不标，就一点都不缓存，每一轮整段对话全价重算。标在工具末尾、系统提示末尾、
//! 最后两条用户消息末尾：倒数第二条正是上一轮的最后一个断点，这一轮从那里读回上一轮写的
//! 缓存。客户端自己标了的（Claude Code）原样不动。
//!
//! 请求照 Codex 的 serde 类型写（`codex-rs/protocol/src/models.rs` 的 `ResponseItem`）。

// 整个请求写成一个 `json!`，嵌套得深
#![recursion_limit = "512"]

use serde_json::{Value, json};
use tw_dialect::convert::{Prepared, decode, prepare};
use tw_dialect::ir::*;

fn tools() -> Value {
    json!({"id": "at_1", "type": "additional_tools", "role": "developer", "tools": [
        {"type": "namespace", "name": "functions", "description": "", "tools": [
            {"type": "function", "name": "exec_command", "description": "Runs a command.", "strict": false,
             "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]}},
            {"type": "custom", "name": "apply_patch", "description": "Edit files.",
             "format": {"type": "grammar", "syntax": "lark", "definition": "start: begin_patch hunk+ end_patch"}}
        ]},
        {"type": "namespace", "name": "mcp__codex_apps__calendar", "description": "Plan events.", "tools": [
            {"type": "function", "name": "_create_event", "description": "Create an event.", "strict": false,
             "parameters": {"type": "object", "properties": {"title": {"type": "string"}}}}
        ]}
    ]})
}

/// Codex 一轮里接连的三个请求：每一步多一次工具调用和它的结果，最后用户接着说
fn turns() -> [Vec<Value>; 3] {
    let first = vec![
        tools(),
        json!({"id": "msg_b", "type": "message", "role": "developer",
               "content": [{"type": "input_text", "text": "You are Codex, a coding agent."}]}),
        json!({"type": "message", "role": "user",
               "content": [{"type": "input_text", "text": "<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>"}]}),
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Fix the failing parser test."}]}),
        json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Running the tests."}], "phase": "commentary"}),
        json!({"type": "function_call", "name": "exec_command", "namespace": "functions", "arguments": "{\"cmd\":\"cargo test\"}", "call_id": "call_1"}),
        json!({"type": "function_call_output", "call_id": "call_1", "output": "parser::tests::nested FAILED"}),
    ];
    let mut second = first.clone();
    second.extend([
        json!({"type": "custom_tool_call", "call_id": "call_2", "name": "apply_patch", "namespace": "functions",
               "input": "*** Begin Patch\n*** Update File: src/parser.rs\n*** End Patch\n"}),
        json!({"type": "custom_tool_call_output", "call_id": "call_2", "output": "Success."}),
    ]);
    let mut third = second.clone();
    third.extend([
        json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Fixed; the tests pass."}], "phase": "final_answer"}),
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Now update the docs."}]}),
    ]);
    [first, second, third]
}

fn request(input: Vec<Value>) -> Value {
    json!({
        "model": "gpt-5.4", "stream": true, "input": input,
        "tool_choice": "auto", "parallel_tool_calls": false,
        "reasoning": {"effort": "medium", "summary": "auto"},
        "store": false, "include": ["reasoning.encrypted_content"]
    })
}

fn target(d: Dialect) -> Target {
    Target {
        dialect: d,
        official: false,
        default_max_tokens: 8192,
    }
}

/// 网关的做法：解码一次，改成上游的模型名，再按上游的格式编码
fn codex(input: Vec<Value>, upstream: Dialect, model: &str) -> Value {
    let mut d = decode(Dialect::Responses, &request(input), "/v1/responses", None).unwrap();
    d.request.model = model.to_string();
    let p: Prepared = d.encode(&target(upstream));
    serde_json::from_slice(&p.body).unwrap()
}

fn claude(input: Vec<Value>) -> Value {
    codex(input, Dialect::Anthropic, "claude-opus-4-7")
}

/// 标了 `cache_control` 的块在哪：("tools" | "system" | "messages", 第几条, 第几块)
fn marks(v: &Value) -> Vec<(&'static str, usize, usize)> {
    let mut out = Vec::new();
    for (key, list) in [("tools", &v["tools"]), ("system", &v["system"])] {
        for (i, b) in list.as_array().into_iter().flatten().enumerate() {
            if b.get("cache_control").is_some() {
                assert_eq!(b["cache_control"], json!({"type": "ephemeral"}), "{b}");
                out.push((key, i, 0));
            }
        }
    }
    for (i, m) in v["messages"].as_array().unwrap().iter().enumerate() {
        for (j, b) in m["content"].as_array().into_iter().flatten().enumerate() {
            if b.get("cache_control").is_some() {
                assert_eq!(b["cache_control"], json!({"type": "ephemeral"}), "{b}");
                out.push(("messages", i, j));
            }
        }
    }
    out
}

fn without_marks(mut v: Value) -> Value {
    fn strip(v: &mut Value) {
        match v {
            Value::Object(o) => {
                o.remove("cache_control");
                o.values_mut().for_each(strip);
            }
            Value::Array(a) => a.iter_mut().for_each(strip),
            _ => {}
        }
    }
    strip(&mut v);
    v
}

/// 一个请求从开头到 (第几条消息, 第几块) 为止的那段，按 Anthropic 算缓存的顺序：工具、
/// 系统提示、消息
fn prefix_to(v: &Value, message: usize, block: usize) -> String {
    let mut messages: Vec<Value> = v["messages"].as_array().unwrap()[..=message].to_vec();
    let last = messages.last_mut().unwrap();
    let content = last["content"].as_array().unwrap()[..=block].to_vec();
    last["content"] = Value::Array(content);
    json!([v["tools"], v["system"], messages]).to_string()
}

#[test]
fn a_codex_request_for_claude_gets_the_four_usual_breakpoints() {
    let [_, second, _] = turns();
    let v = claude(second);
    let roles: Vec<&str> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant", "user"]);
    assert_eq!(
        marks(&v),
        [
            // 工具末尾
            ("tools", 2, 0),
            // 系统提示末尾：基础指令之后还有 MCP namespace 的说明
            ("system", 1, 0),
            // 上一轮的最后一条：exec_command 的结果
            ("messages", 2, 0),
            // 这一轮的最后一条：apply_patch 的结果
            ("messages", 4, 0),
        ]
    );
    assert_eq!(v.to_string().matches("cache_control").count(), 4);
}

#[test]
fn each_turn_reads_back_what_the_turn_before_wrote() {
    let [first, second, third] = turns();
    let bodies = [claude(first), claude(second), claude(third)];
    for pair in bodies.windows(2) {
        let (earlier, later) = (&pair[0], &pair[1]);
        // 前一个请求的最后一个断点
        let &(_, i, j) = marks(earlier).last().unwrap();
        // 后一个请求在同一块上也有一个：从这里读回前一个写进去的缓存
        assert!(
            marks(later).contains(&("messages", i, j)),
            "{:?} vs {:?}",
            marks(earlier),
            marks(later)
        );
        // 到那一块为止一个字节都不差（断点本身不算内容：前一个请求标在更早位置的那个，
        // 这一轮挪走了 —— Anthropic 文档里多轮对话就是这么标的）
        assert_eq!(
            prefix_to(&without_marks(earlier.clone()), i, j),
            prefix_to(&without_marks(later.clone()), i, j)
        );
        // 工具和系统提示连断点都一样
        assert_eq!(earlier["tools"], later["tools"]);
        assert_eq!(earlier["system"], later["system"]);
    }
}

#[test]
fn a_client_that_marks_its_own_breakpoints_keeps_exactly_those() {
    // Claude Code 的请求：自己标了系统提示和最后一条消息
    let claude_code = json!({
        "model": "claude-opus-4-7", "max_tokens": 1024,
        "system": [{"type": "text", "text": "You are Claude Code.", "cache_control": {"type": "ephemeral"}}],
        "tools": [{"name": "Read", "input_schema": {"type": "object"}}],
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": [{"type": "text", "text": "read a.rs", "cache_control": {"type": "ephemeral", "ttl": "1h"}}]}
        ]
    })
    .to_string();
    // 转给 Anthropic 格式的另一种写法（经过中间表示）
    let p = prepare(
        Dialect::Anthropic,
        claude_code.as_bytes(),
        "/v1/messages",
        None,
        &target(Dialect::Anthropic),
    )
    .unwrap();
    let v: Value = serde_json::from_slice(&p.body).unwrap();
    assert_eq!(marks_any(&v), 2, "{v}");
    assert!(v["tools"][0].get("cache_control").is_none());
    assert_eq!(v["messages"][2]["content"][0]["cache_control"]["ttl"], "1h");
    // 转给 Bedrock 上的 Claude
    let mut d = decode(
        Dialect::Anthropic,
        &serde_json::from_str(&claude_code).unwrap(),
        "/v1/messages",
        None,
    )
    .unwrap();
    d.request.model = "us.anthropic.claude-sonnet-4-5-20250929-v1:0".into();
    let v: Value = serde_json::from_slice(&d.encode(&target(Dialect::Bedrock)).body).unwrap();
    assert_eq!(v.to_string().matches("cachePoint").count(), 2, "{v}");
}

fn marks_any(v: &Value) -> usize {
    v.to_string().matches("cache_control").count()
}

#[test]
fn a_short_conversation_gets_only_the_breakpoints_it_has_room_for() {
    // 没有工具、没有系统提示、只有一条用户消息：只标那一条
    let chat = json!({"model": "claude-opus-4-7", "messages": [{"role": "user", "content": "hi"}]})
        .to_string();
    let p = prepare(
        Dialect::Chat,
        chat.as_bytes(),
        "/v1/chat/completions",
        None,
        &target(Dialect::Anthropic),
    )
    .unwrap();
    let v: Value = serde_json::from_slice(&p.body).unwrap();
    assert_eq!(marks(&v), [("messages", 0, 0)]);
    // 不认断点的格式什么都不加
    for d in [Dialect::Chat, Dialect::Gemini, Dialect::Responses] {
        let [_, second, _] = turns();
        let v = codex(second, d, "m");
        assert!(!v.to_string().contains("cache_control"), "{d:?}");
        assert!(!v.to_string().contains("cachePoint"), "{d:?}");
    }
}

#[test]
fn claude_on_bedrock_gets_the_same_four_cache_points() {
    let [_, second, _] = turns();
    let v = codex(
        second.clone(),
        Dialect::Bedrock,
        "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
    );
    let point = json!({"cachePoint": {"type": "default"}});
    let tools = v["toolConfig"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 4, "{v}");
    assert_eq!(tools[3], point);
    assert_eq!(v["system"].as_array().unwrap().last(), Some(&point));
    let messages = v["messages"].as_array().unwrap();
    for i in [2, 4] {
        assert_eq!(
            messages[i]["content"].as_array().unwrap().last(),
            Some(&point),
            "{i}"
        );
    }
    assert_eq!(v.to_string().matches("cachePoint").count(), 4);
    // 别家的模型不自动标：不认 cachePoint 的会拒掉整个请求。看不出背后是谁的 ARN 也不标
    for model in [
        "meta.llama3-70b-instruct-v1:0",
        "amazon.nova-pro-v1:0",
        "arn:aws:bedrock:us-east-1:123:application-inference-profile/x",
    ] {
        let v = codex(second.clone(), Dialect::Bedrock, model);
        assert!(!v.to_string().contains("cachePoint"), "{model}");
    }
}
