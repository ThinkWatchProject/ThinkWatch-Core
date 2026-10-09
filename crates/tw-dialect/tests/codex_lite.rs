//! Codex 的 Responses Lite 请求转给别家格式的上游，走一个来回。
//!
//! Responses Lite（Codex 的 `use_responses_lite`）的请求顶层没有 `tools`：所有工具装在
//! `input` 开头的 `additional_tools` 项里，函数和自由格式工具又装在 `functions` 这个
//! namespace 里（`codex-rs/core/src/client.rs` 的 `build_responses_request`、
//! `codex-rs/tools/src/tool_spec.rs` 的 `create_tools_json_for_responses_lite`）。这一项
//! 以前整个被丢掉：转给别家的请求一个工具都没有，Codex 回答「没有挂载读写和执行命令的工具」。
//!
//! 请求照 Codex 的 serde 类型写（`codex-rs/protocol/src/models.rs` 的 `ResponseItem`）。
//! 检查：
//!
//! - 发给 Anthropic、Chat、Gemini、Bedrock 的请求里工具一个不少，历史里的调用和结果成对
//! - 上游调用这些工具时，Codex 收到的调用用的是它自己的写法：namespace 加名字、
//!   `function_call` 还是 `custom_tool_call`、工具搜索是 `tool_search_call` —— 整包、流式、
//!   「上游给流、客户端要整包」三条路都一样
//! - 同格式直通一个字节都不改

// 整个请求写成一个 `json!`，嵌套得深
#![recursion_limit = "512"]

use serde_json::{Value, json};
use tw_dialect::convert::{Session, decode, strip_carried};
use tw_dialect::frame::Decoder;
use tw_dialect::ir::*;
use tw_dialect::{anthropic, bedrock, chat, gemini};

const UPSTREAMS: [Dialect; 4] = [
    Dialect::Anthropic,
    Dialect::Chat,
    Dialect::Gemini,
    Dialect::Bedrock,
];

const PATCH: &str = "*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch\n";

/// Codex 0.1xx 用 Responses Lite 时发的那种请求：第二轮，带着上一轮的命令调用、一次老式的
/// `local_shell_call`、一次工具搜索和对话中途改过的推理强度
fn lite_request() -> Value {
    json!({
        "model": "gpt-5.4",
        "stream": true,
        "input": [
            {"id": "at_0b9e", "type": "additional_tools", "role": "developer", "tools": [
                {"type": "namespace", "name": "functions", "description": "", "tools": [
                    {"type": "function", "name": "exec_command", "description": "Runs a command in a PTY.", "strict": false,
                     "parameters": {"type": "object", "properties": {
                         "cmd": {"type": "string", "description": "Shell command to execute."},
                         "workdir": {"type": "string"}
                     }, "required": ["cmd"], "additionalProperties": false}},
                    {"type": "function", "name": "write_stdin", "description": "Writes characters to an existing session.", "strict": false,
                     "parameters": {"type": "object", "properties": {"session_id": {"type": "number"}, "chars": {"type": "string"}},
                                    "required": ["session_id"], "additionalProperties": false}},
                    {"type": "custom", "name": "apply_patch", "description": "Use the `apply_patch` tool to edit files.",
                     "format": {"type": "grammar", "syntax": "lark", "definition": "start: begin_patch hunk+ end_patch\nbegin_patch: \"*** Begin Patch\" LF"}},
                    {"type": "function", "name": "update_plan", "description": "Updates the task plan.", "strict": false,
                     "parameters": {"type": "object", "properties": {"plan": {"type": "array", "items": {"type": "object"}}},
                                    "required": ["plan"], "additionalProperties": false}}
                ]},
                {"type": "namespace", "name": "mcp__codex_apps__calendar", "description": "Plan events.", "tools": [
                    {"type": "function", "name": "_create_event", "description": "Create a calendar event.", "strict": false,
                     "parameters": {"type": "object", "properties": {"title": {"type": "string"}}, "required": ["title"]}}
                ]},
                {"type": "namespace", "name": "web", "description": "Tools in the web namespace.", "tools": [
                    {"type": "function", "name": "run", "description": "Search the web.", "strict": false,
                     "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}}
                ]},
                {"type": "tool_search", "execution": "client", "description": "Search deferred tools.",
                 "parameters": {"type": "object", "properties": {
                     "query": {"type": "string", "description": "Search query for deferred tools."},
                     "limit": {"type": "number", "description": "Maximum number of tools to return. Defaults to 8."}
                 }, "required": ["query"], "additionalProperties": false}}
            ]},
            {"id": "msg_5c1d", "type": "message", "role": "developer",
             "content": [{"type": "input_text", "text": "You are Codex, a coding agent."}],
             "internal_chat_message_metadata_passthrough": {"content_item_kinds": ["model.base_instructions"]}},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Fix the failing test."}]},
            {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "**Running the tests**"}], "encrypted_content": "gAAAAB"},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Running the tests first."}], "phase": "commentary"},
            {"type": "function_call", "name": "exec_command", "namespace": "functions", "arguments": "{\"cmd\":\"cargo test\"}", "call_id": "call_1"},
            {"type": "function_call_output", "call_id": "call_1", "output": "test result: FAILED"},
            {"type": "custom_tool_call", "status": "completed", "call_id": "call_2", "name": "apply_patch", "namespace": "functions", "input": PATCH},
            {"type": "custom_tool_call_output", "call_id": "call_2", "output": "Success. Updated the following files:\nA a.txt"},
            {"type": "local_shell_call", "id": "lsh_1", "call_id": "call_3", "status": "completed",
             "action": {"type": "exec", "command": ["cargo", "test"], "timeout_ms": 60000, "working_directory": "/repo", "env": null, "user": null}},
            {"type": "function_call_output", "call_id": "call_3", "output": "test result: ok"},
            {"type": "tool_search_call", "call_id": "call_4", "execution": "client", "status": "completed", "arguments": {"query": "drive files", "limit": 8}},
            {"type": "tool_search_output", "call_id": "call_4", "status": "completed", "execution": "client", "tools": [
                {"type": "namespace", "name": "mcp__codex_apps__drive", "description": "Files in Drive.", "tools": [
                    {"type": "function", "name": "_search", "description": "Search files.", "strict": false, "defer_loading": true,
                     "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}}
                ]}
            ]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "The test passes now."}], "phase": "final_answer"},
            {"type": "configuration_update", "reasoning": {"effort": "high"}},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Now add a calendar event for the release."}]}
        ],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "reasoning": {"effort": "medium", "summary": "auto", "context": "all_turns"},
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "019a5e0c-3d1b-7f00-8000-000000000001",
        "text": {"verbosity": "low"},
        "client_metadata": {"x-codex-turn-metadata": "{\"turn_id\":\"t1\"}"}
    })
}

/// 每个工具在别家那边叫什么
const TOOLS: [&str; 8] = [
    "exec_command",
    "write_stdin",
    "apply_patch",
    "update_plan",
    "mcp__codex_apps__calendar_create_event",
    "web__run",
    "tool_search",
    "mcp__codex_apps__drive_search",
];

fn target(d: Dialect) -> Target {
    Target {
        dialect: d,
        official: false,
        default_max_tokens: 8192,
    }
}

/// 网关的做法：解码一次，改成上游的模型名，再按上游的格式编码
fn prepared(upstream: Dialect, model: &str) -> tw_dialect::convert::Prepared {
    let mut d = decode(Dialect::Responses, &lite_request(), "/v1/responses", None).unwrap();
    d.request.model = model.to_string();
    d.encode(&target(upstream))
}

/// 用上游格式自己的解码器把发出去的请求读回来
fn read_back(upstream: Dialect, body: &[u8]) -> Request {
    let v: Value = serde_json::from_slice(body).unwrap();
    let mut d = Dropped::new(upstream);
    let mut shape = ClientShape::default();
    match upstream {
        Dialect::Anthropic => anthropic::decode_request(&v, &mut d),
        Dialect::Chat => chat::decode_request(&v, &mut d, &mut shape),
        Dialect::Gemini => gemini::decode_request(&v, "m", true, &mut d),
        Dialect::Bedrock => bedrock::decode_request(&v, "m", true, &mut d),
        Dialect::Responses => unreachable!(),
    }
    .unwrap()
}

#[test]
fn every_tool_codex_declares_in_input_reaches_the_upstream() {
    for upstream in UPSTREAMS {
        let model = match upstream {
            Dialect::Anthropic => "claude-opus-4-7",
            Dialect::Chat => "deepseek-chat",
            Dialect::Gemini => "gemini-2.5-pro",
            _ => "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        };
        let p = prepared(upstream, model);
        // 丢的只是别家确实没有的：OpenAI 加密的推理（只有 OpenAI 读得懂）、apply_patch 的
        // 语法约束（只有 OpenAI 执行）、回答写多写少，Gemini 和 Bedrock 还有「一次只调一个
        // 工具」。工具声明一项不丢
        let mut want = vec!["input.reasoning", "tools.custom.format"];
        if matches!(upstream, Dialect::Gemini | Dialect::Bedrock) {
            want.push("parallel_tool_calls");
        }
        want.push("text.verbosity");
        assert_eq!(p.dropped, want, "{upstream:?}");

        let r = read_back(upstream, &p.body);
        let names: Vec<&str> = r.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, TOOLS, "{upstream:?}");
        // 自由格式的 apply_patch 在只有 JSON 工具的格式里声明成 `{"input": string}`
        let patch = r.tools.iter().find(|t| t.name == "apply_patch").unwrap();
        assert!(
            matches!(&patch.kind, ToolKind::Function { schema, .. } if schema["properties"]["input"]["type"] == "string"),
            "{upstream:?}: {patch:?}"
        );
        // 系统提示：基础指令、MCP 服务器的说明
        let system = r.system.join("\n");
        assert!(system.contains("You are Codex"), "{upstream:?}");
        assert!(system.contains("Plan events."), "{upstream:?}");
        assert!(system.contains("Files in Drive."), "{upstream:?}");

        // 历史里的四次调用（含老式的 local_shell_call 和工具搜索）都在，每次都有结果
        let calls: Vec<&str> = r
            .messages
            .iter()
            .flat_map(|m| &m.parts)
            .filter_map(|p| match p {
                Part::ToolCall(c) => Some(c.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            calls,
            ["exec_command", "apply_patch", "local_shell", "tool_search"],
            "{upstream:?}"
        );
        let results: Vec<String> = r
            .messages
            .iter()
            .flat_map(|m| &m.parts)
            .filter_map(|p| match p {
                Part::ToolResult(res) => Some(res.text()),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 4, "{upstream:?}: {results:?}");
        assert!(results[2].contains("test result: ok"), "{upstream:?}");
        assert!(
            results[3].contains("mcp__codex_apps__drive_search"),
            "{upstream:?}"
        );
    }
}

#[test]
fn the_effort_changed_mid_conversation_is_the_one_sent() {
    // 顶层写的是 medium（Codex 为了保住提示缓存，一直写开头那一档），对话中途改成了 high
    let p = prepared(Dialect::Chat, "gpt-5.4");
    let v: Value = serde_json::from_slice(&p.body).unwrap();
    assert_eq!(v["reasoning_effort"], "high");
    // GPT-5 认 verbosity：照写，不算丢
    assert_eq!(v["verbosity"], "low");
    assert_eq!(p.dropped, ["input.reasoning", "tools.custom.format"]);
    let p = prepared(Dialect::Chat, "deepseek-chat");
    let v: Value = serde_json::from_slice(&p.body).unwrap();
    assert!(v.get("verbosity").is_none());
    assert_eq!(
        p.dropped,
        ["input.reasoning", "tools.custom.format", "text.verbosity"]
    );
}

#[test]
fn passing_straight_through_to_openai_changes_nothing() {
    let body = lite_request().to_string();
    assert_eq!(strip_carried(Dialect::Responses, body.as_bytes()), None);
}

// ───────────────────────────────────────────────────────── 上游调用工具

fn named(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn data(v: Value) -> String {
    format!("data: {v}\n\n")
}

/// 上游的整包回答：一段话，再调用四种工具
fn upstream_response(d: Dialect) -> Value {
    let wrapped = json!({ "input": PATCH });
    match d {
        Dialect::Anthropic => json!({
            "id": "msg_up", "type": "message", "role": "assistant", "model": "claude-opus-4-7",
            "content": [
                {"type": "text", "text": "On it."},
                {"type": "tool_use", "id": "toolu_1", "name": "exec_command", "input": {"cmd": "cargo test"}},
                {"type": "tool_use", "id": "toolu_2", "name": "apply_patch", "input": wrapped},
                {"type": "tool_use", "id": "toolu_3", "name": "mcp__codex_apps__calendar_create_event", "input": {"title": "release"}},
                {"type": "tool_use", "id": "toolu_4", "name": "tool_search", "input": {"query": "drive"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        }),
        Dialect::Chat => json!({
            "id": "chatcmpl-up", "object": "chat.completion", "model": "deepseek-chat",
            "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": "On it.",
                "tool_calls": [
                    {"id": "call_a", "type": "function", "function": {"name": "exec_command", "arguments": "{\"cmd\":\"cargo test\"}"}},
                    {"id": "call_b", "type": "function", "function": {"name": "apply_patch", "arguments": wrapped.to_string()}},
                    {"id": "call_c", "type": "function", "function": {"name": "mcp__codex_apps__calendar_create_event", "arguments": "{\"title\":\"release\"}"}},
                    {"id": "call_d", "type": "function", "function": {"name": "tool_search", "arguments": "{\"query\":\"drive\"}"}}
                ]
            }}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        }),
        Dialect::Gemini => json!({
            "candidates": [{"content": {"role": "model", "parts": [
                {"text": "On it."},
                {"functionCall": {"name": "exec_command", "args": {"cmd": "cargo test"}}},
                {"functionCall": {"name": "apply_patch", "args": wrapped}},
                {"functionCall": {"name": "mcp__codex_apps__calendar_create_event", "args": {"title": "release"}}},
                {"functionCall": {"name": "tool_search", "args": {"query": "drive"}}}
            ]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5},
            "modelVersion": "gemini-2.5-pro"
        }),
        Dialect::Bedrock => json!({
            "output": {"message": {"role": "assistant", "content": [
                {"text": "On it."},
                {"toolUse": {"toolUseId": "tooluse_1", "name": "exec_command", "input": {"cmd": "cargo test"}}},
                {"toolUse": {"toolUseId": "tooluse_2", "name": "apply_patch", "input": wrapped}},
                {"toolUse": {"toolUseId": "tooluse_3", "name": "mcp__codex_apps__calendar_create_event", "input": {"title": "release"}}},
                {"toolUse": {"toolUseId": "tooluse_4", "name": "tool_search", "input": {"query": "drive"}}}
            ]}},
            "stopReason": "tool_use",
            "usage": {"inputTokens": 10, "outputTokens": 5, "totalTokens": 15}
        }),
        Dialect::Responses => unreachable!(),
    }
}

/// 同样的回答写成上游的流，参数分几片发
fn upstream_stream(d: Dialect) -> String {
    let wrapped = json!({ "input": PATCH }).to_string();
    let (w1, w2) = wrapped.split_at(12);
    let calls: [(&str, &str, &str); 4] = [
        ("exec_command", "{\"cmd\":", "\"cargo test\"}"),
        ("apply_patch", w1, w2),
        (
            "mcp__codex_apps__calendar_create_event",
            "{\"title\":",
            "\"release\"}",
        ),
        ("tool_search", "{\"query\":", "\"drive\"}"),
    ];
    match d {
        Dialect::Anthropic => {
            let mut s = vec![
                named(
                    "message_start",
                    json!({"type": "message_start", "message": {"id": "msg_up", "model": "claude-opus-4-7", "usage": {"input_tokens": 10, "output_tokens": 1}}}),
                ),
                named(
                    "content_block_start",
                    json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                ),
                named(
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "On it."}}),
                ),
                named(
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": 0}),
                ),
            ];
            for (i, (name, a, b)) in calls.iter().enumerate() {
                let index = i + 1;
                s.push(named("content_block_start", json!({"type": "content_block_start", "index": index, "content_block": {"type": "tool_use", "id": format!("toolu_{index}"), "name": name, "input": {}}})));
                for part in [a, b] {
                    s.push(named("content_block_delta", json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": part}})));
                }
                s.push(named(
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": index}),
                ));
            }
            s.push(named("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 5}})));
            s.push(named("message_stop", json!({"type": "message_stop"})));
            s.concat()
        }
        Dialect::Chat => {
            let chunk = |delta: Value, finish: Value| {
                data(
                    json!({"id": "chatcmpl-up", "object": "chat.completion.chunk", "model": "deepseek-chat",
                    "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}),
                )
            };
            let mut s = vec![
                chunk(json!({"role": "assistant", "content": ""}), Value::Null),
                chunk(json!({"content": "On it."}), Value::Null),
            ];
            for (i, (name, a, b)) in calls.iter().enumerate() {
                s.push(chunk(json!({"tool_calls": [{"index": i, "id": format!("call_{i}"), "type": "function", "function": {"name": name, "arguments": a}}]}), Value::Null));
                s.push(chunk(
                    json!({"tool_calls": [{"index": i, "function": {"arguments": b}}]}),
                    Value::Null,
                ));
            }
            s.push(chunk(json!({}), json!("tool_calls")));
            s.push(data(json!({"id": "chatcmpl-up", "object": "chat.completion.chunk", "model": "deepseek-chat", "choices": [],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5}})));
            s.push("data: [DONE]\n\n".to_string());
            s.concat()
        }
        // Gemini 的函数调用不分片：一次给完整的参数
        Dialect::Gemini => {
            let v = upstream_response(Dialect::Gemini);
            let parts = v["candidates"][0]["content"]["parts"].as_array().unwrap();
            let mut s: Vec<String> = parts[..parts.len() - 1]
                .iter()
                .map(|p| data(json!({"candidates": [{"content": {"role": "model", "parts": [p]}}], "modelVersion": "gemini-2.5-pro"})))
                .collect();
            s.push(data(json!({"candidates": [{"content": {"role": "model", "parts": [parts.last().unwrap()]}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5}})));
            s.concat()
        }
        Dialect::Bedrock => {
            let mut s = vec![
                named("messageStart", json!({"role": "assistant"})),
                named(
                    "contentBlockDelta",
                    json!({"contentBlockIndex": 0, "delta": {"text": "On it."}}),
                ),
                named("contentBlockStop", json!({"contentBlockIndex": 0})),
            ];
            for (i, (name, a, b)) in calls.iter().enumerate() {
                let index = i + 1;
                s.push(named("contentBlockStart", json!({"contentBlockIndex": index, "start": {"toolUse": {"toolUseId": format!("tooluse_{index}"), "name": name}}})));
                for part in [a, b] {
                    s.push(named(
                        "contentBlockDelta",
                        json!({"contentBlockIndex": index, "delta": {"toolUse": {"input": part}}}),
                    ));
                }
                s.push(named(
                    "contentBlockStop",
                    json!({"contentBlockIndex": index}),
                ));
            }
            s.push(named("messageStop", json!({"stopReason": "tool_use"})));
            s.push(named(
                "metadata",
                json!({"usage": {"inputTokens": 10, "outputTokens": 5, "totalTokens": 15}}),
            ));
            s.concat()
        }
        Dialect::Responses => unreachable!(),
    }
}

/// Codex 的 `ResponseItem` 里这几种调用必须有的字段（`codex-rs/protocol/src/models.rs`）。
/// 少一个，Codex 解析 `output_item` 失败，这次调用就像没发生过
fn assert_codex_can_read(item: &Value, at: &str) {
    let has_str = |k: &str| item[k].is_string();
    match item["type"].as_str() {
        Some("function_call") => assert!(
            has_str("name") && has_str("arguments") && has_str("call_id"),
            "{at}: {item}"
        ),
        Some("custom_tool_call") => assert!(
            has_str("name") && has_str("input") && has_str("call_id"),
            "{at}: {item}"
        ),
        Some("tool_search_call") => assert!(
            has_str("execution") && !item["arguments"].is_null() && has_str("call_id"),
            "{at}: {item}"
        ),
        Some("message" | "reasoning") => {}
        other => panic!("{at}: unexpected item {other:?}: {item}"),
    }
}

/// 调用写回 Codex 的样子：(类型, namespace, 名字, 参数)
fn calls_of(items: &[Value], at: &str) -> Vec<(String, Option<String>, String, Value)> {
    items
        .iter()
        .inspect(|i| assert_codex_can_read(i, at))
        .filter(|i| i["type"] != "message" && i["type"] != "reasoning")
        .map(|i| {
            let kind = i["type"].as_str().unwrap().to_string();
            let args = match kind.as_str() {
                "function_call" => serde_json::from_str(i["arguments"].as_str().unwrap()).unwrap(),
                "custom_tool_call" => i["input"].clone(),
                _ => i["arguments"].clone(),
            };
            (
                kind,
                i["namespace"].as_str().map(str::to_string),
                i["name"].as_str().unwrap_or_default().to_string(),
                args,
            )
        })
        .collect()
}

fn check(items: &[Value], at: &str) {
    let ns = |s: &str| Some(s.to_string());
    assert_eq!(
        calls_of(items, at),
        [
            (
                "function_call".to_string(),
                ns("functions"),
                "exec_command".to_string(),
                json!({"cmd": "cargo test"})
            ),
            (
                "custom_tool_call".to_string(),
                ns("functions"),
                "apply_patch".to_string(),
                json!(PATCH)
            ),
            (
                "function_call".to_string(),
                ns("mcp__codex_apps__calendar"),
                "_create_event".to_string(),
                json!({"title": "release"})
            ),
            (
                "tool_search_call".to_string(),
                None,
                String::new(),
                json!({"query": "drive"})
            ),
        ],
        "{at}"
    );
    let search = items
        .iter()
        .find(|i| i["type"] == "tool_search_call")
        .unwrap();
    assert_eq!(search["execution"], "client", "{at}");
    assert!(
        search["id"].as_str().unwrap().starts_with("tsc_"),
        "{at}: {search}"
    );
}

fn session(upstream: Dialect, stream: bool) -> Session {
    let mut body = lite_request();
    body["stream"] = json!(stream);
    let mut d = decode(Dialect::Responses, &body, "/v1/responses", None).unwrap();
    d.request.model = "up-model".into();
    d.encode(&target(upstream)).session
}

#[test]
fn a_whole_answer_calls_the_tools_the_way_codex_names_them() {
    for upstream in UPSTREAMS {
        let s = session(upstream, false);
        let out = s
            .response(upstream_response(upstream).to_string().as_bytes())
            .unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        check(
            v["output"].as_array().unwrap(),
            &format!("{upstream:?} 整包"),
        );
        assert_eq!(v["status"], "completed");
    }
}

#[test]
fn a_streamed_answer_calls_the_tools_the_way_codex_names_them() {
    for upstream in UPSTREAMS {
        let at = format!("{upstream:?} 流式");
        let s = session(upstream, true);
        let mut c = s.stream();
        let mut out = Vec::new();
        for piece in upstream_stream(upstream).as_bytes().chunks(7) {
            out.extend(c.process(piece));
        }
        out.extend(c.finish());
        let mut dec = Decoder::default();
        let frames = dec.feed(&out);
        let events: Vec<(String, Value)> = frames
            .iter()
            .map(|f| {
                (
                    f.event.clone().unwrap_or_default(),
                    serde_json::from_str(&f.data).unwrap(),
                )
            })
            .collect();
        // Codex 记历史用的是 output_item.done 里的完整项
        let done: Vec<Value> = events
            .iter()
            .filter(|(k, _)| k == "response.output_item.done")
            .map(|(_, v)| v["item"].clone())
            .collect();
        check(&done, &at);
        // 刚开始的项也要读得懂：Codex 用它显示「正在调用」
        for (_, v) in events
            .iter()
            .filter(|(k, _)| k == "response.output_item.added")
        {
            assert_codex_can_read(&v["item"], &at);
        }
        // 工具搜索没有参数的增量事件
        let search_id = done
            .iter()
            .find(|i| i["type"] == "tool_search_call")
            .unwrap()["id"]
            .clone();
        assert!(
            !events
                .iter()
                .any(|(k, v)| k.ends_with(".delta") && v["item_id"] == search_id),
            "{at}"
        );
        // 自由格式工具的原文完整地出现在它自己的事件里
        assert!(
            events
                .iter()
                .any(|(k, v)| k == "response.custom_tool_call_input.done" && v["input"] == PATCH),
            "{at}"
        );
        let (last, body) = events.last().unwrap();
        assert_eq!(last, "response.completed", "{at}");
        check(body["response"]["output"].as_array().unwrap(), &at);
    }
}

#[test]
fn a_stream_collected_into_a_whole_answer_calls_the_tools_the_same_way() {
    for upstream in UPSTREAMS {
        let s = session(upstream, false);
        let mut c = s.collector();
        for piece in upstream_stream(upstream).as_bytes().chunks(7) {
            c.process(piece);
        }
        let v: Value = serde_json::from_slice(&c.finish().unwrap()).unwrap();
        check(
            v["output"].as_array().unwrap(),
            &format!("{upstream:?} 收集"),
        );
    }
}
