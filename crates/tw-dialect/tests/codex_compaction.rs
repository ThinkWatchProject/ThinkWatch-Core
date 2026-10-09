//! Codex 压缩前文、对话中途的 developer 消息，转给别家格式的上游时。
//!
//! **压缩**：上游是 OpenAI 时 Codex 走远程压缩（`codex-rs/core/src/compact_remote_v2.rs`）：
//! 历史末尾加一项 `compaction_trigger` 发一个流式请求，收流时要求 `output_item.done` 里恰好
//! 一个 `compaction` 项、还要有 `response.completed`；之后的请求把这个项原样带回来。这里照这个
//! 约定走一个来回：上游写摘要，Codex 收到一个 `compaction` 项，下一轮带回来时上游读到的是摘要。
//!
//! **对话中途的 developer 消息**：留在原位，系统提示不跟着变 —— 前后两个请求的开头一样，
//! 从系统提示算起的提示缓存才接得上。
//!
//! 请求照 Codex 的 serde 类型写（`codex-rs/protocol/src/models.rs` 的 `ResponseItem`）。

// 整个请求写成一个 `json!`，嵌套得深
#![recursion_limit = "512"]

use serde_json::{Value, json};
use tw_dialect::compaction;
use tw_dialect::convert::{Prepared, Session, decode, prepare, strip_carried};
use tw_dialect::frame::Decoder;
use tw_dialect::ir::*;

const UPSTREAMS: [Dialect; 4] = [
    Dialect::Anthropic,
    Dialect::Chat,
    Dialect::Gemini,
    Dialect::Bedrock,
];

const SUMMARY: &str = "The user wants the failing parser test fixed.\n\n\
    - Ran `cargo test`: `parser::tests::nested` failed (unclosed bracket).\n\
    - Patched src/parser.rs to close nested brackets; tests now pass.\n\
    - Next: update docs/syntax.md to describe nesting.";

fn tools() -> Value {
    json!({"id": "at_1", "type": "additional_tools", "role": "developer", "tools": [
        {"type": "namespace", "name": "functions", "description": "", "tools": [
            {"type": "function", "name": "exec_command", "description": "Runs a command.", "strict": false,
             "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]}},
            {"type": "custom", "name": "apply_patch", "description": "Edit files.",
             "format": {"type": "grammar", "syntax": "lark", "definition": "start: begin_patch hunk+ end_patch"}}
        ]}
    ]})
}

/// 一轮做完的对话：开头的工具声明和 developer 消息，中途 Codex 插了一条 developer 消息
fn history() -> Vec<Value> {
    vec![
        tools(),
        json!({"id": "msg_b", "type": "message", "role": "developer",
               "content": [{"type": "input_text", "text": "You are Codex, a coding agent."}]}),
        json!({"type": "message", "role": "developer",
               "content": [{"type": "input_text", "text": "<permissions instructions>sandbox: workspace-write</permissions instructions>"}]}),
        json!({"type": "message", "role": "user",
               "content": [{"type": "input_text", "text": "<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>"}]}),
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Fix the failing parser test."}]}),
        json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Running the tests."}], "phase": "commentary"}),
        json!({"type": "function_call", "name": "exec_command", "namespace": "functions", "arguments": "{\"cmd\":\"cargo test\"}", "call_id": "call_1"}),
        json!({"type": "function_call_output", "call_id": "call_1", "output": "parser::tests::nested FAILED"}),
        json!({"type": "message", "role": "developer",
               "content": [{"type": "input_text", "text": "<collaboration_mode>Default</collaboration_mode>"}]}),
        json!({"type": "custom_tool_call", "call_id": "call_2", "name": "apply_patch", "namespace": "functions",
               "input": "*** Begin Patch\n*** Update File: src/parser.rs\n*** End Patch\n"}),
        json!({"type": "custom_tool_call_output", "call_id": "call_2", "output": "Success."}),
        json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Fixed; the tests pass."}], "phase": "final_answer"}),
    ]
}

fn request(input: Vec<Value>) -> Value {
    json!({
        "model": "gpt-5.4",
        "stream": true,
        "input": input,
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "reasoning": {"effort": "medium", "summary": "auto", "context": "all_turns"},
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "019a"
    })
}

/// Codex 要压缩时发的：历史末尾加 `compaction_trigger`
fn compaction_request() -> Value {
    let mut input = history();
    input.push(json!({"type": "compaction_trigger"}));
    request(input)
}

fn target(d: Dialect) -> Target {
    Target {
        dialect: d,
        official: false,
        default_max_tokens: 8192,
    }
}

fn model(d: Dialect) -> &'static str {
    match d {
        Dialect::Anthropic => "claude-opus-4-7",
        Dialect::Chat => "deepseek-chat",
        Dialect::Gemini => "gemini-2.5-pro",
        _ => "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
    }
}

/// 网关的做法：解码一次，改成上游的模型名，再按上游的格式编码
fn prepared(body: &Value, upstream: Dialect) -> Prepared {
    let mut d = decode(Dialect::Responses, body, "/v1/responses", None).unwrap();
    d.request.model = model(upstream).to_string();
    d.encode(&target(upstream))
}

fn body_of(p: &Prepared) -> Value {
    serde_json::from_slice(&p.body).unwrap()
}

/// 发给上游的请求里：系统提示、工具、对话
fn parts_of(d: Dialect, v: &Value) -> (Value, Value, Vec<Value>) {
    match d {
        Dialect::Anthropic | Dialect::Bedrock => (
            v["system"].clone(),
            v.get("tools")
                .or_else(|| v.get("toolConfig"))
                .cloned()
                .unwrap(),
            v["messages"].as_array().unwrap().clone(),
        ),
        Dialect::Chat => {
            let messages = v["messages"].as_array().unwrap();
            assert_eq!(messages[0]["role"], "system");
            (
                messages[0].clone(),
                v["tools"].clone(),
                messages[1..].to_vec(),
            )
        }
        Dialect::Gemini => (
            v["systemInstruction"].clone(),
            v["tools"].clone(),
            v["contents"].as_array().unwrap().clone(),
        ),
        Dialect::Responses => unreachable!(),
    }
}

// ───────────────────────────────────────────────────────── 对话中途的 developer 消息

#[test]
fn a_developer_message_mid_conversation_leaves_the_prefix_alone() {
    // 这一轮：历史 + 新的一句
    let mut first = history();
    first.push(json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Now the docs."}]}));
    // 下一轮：上游答了一句，Codex 又插了一条 developer 消息，用户接着说
    let mut second = first.clone();
    second.extend([
        json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Which file?"}]}),
        json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "<permissions instructions>approval: never</permissions instructions>"}]}),
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "docs/syntax.md"}]}),
    ]);
    for upstream in UPSTREAMS {
        let a = body_of(&prepared(&request(first.clone()), upstream));
        let b = body_of(&prepared(&request(second.clone()), upstream));
        let (sys_a, tools_a, msgs_a) = parts_of(upstream, &a);
        let (sys_b, tools_b, msgs_b) = parts_of(upstream, &b);
        // 系统提示和工具一个字都没变：开头那几条 developer 消息在里面，中途的不在
        assert_eq!(sys_a, sys_b, "{upstream:?}");
        assert_eq!(tools_a, tools_b, "{upstream:?}");
        let sys = sys_b.to_string();
        assert!(sys.contains("You are Codex"), "{upstream:?}: {sys}");
        assert!(
            sys.contains("sandbox: workspace-write"),
            "{upstream:?}: {sys}"
        );
        assert!(!sys.contains("collaboration_mode"), "{upstream:?}: {sys}");
        assert!(!sys.contains("approval: never"), "{upstream:?}: {sys}");
        // 前一个请求的对话是后一个的开头
        assert_eq!(msgs_a[..], msgs_b[..msgs_a.len()], "{upstream:?}");
        // 中途的 developer 消息在它原来的位置，标明是系统说的
        let later = Value::Array(msgs_b[msgs_a.len()..].to_vec()).to_string();
        assert!(
            later.contains(
                "<system-reminder>\\n<permissions instructions>approval: never</permissions instructions>\\n</system-reminder>"
            ),
            "{upstream:?}: {later}"
        );
        let earlier = Value::Array(msgs_a.clone()).to_string();
        assert!(
            earlier.contains("<system-reminder>\\n<collaboration_mode>"),
            "{upstream:?}: {earlier}"
        );
    }
}

#[test]
fn a_mid_conversation_system_message_stays_a_developer_message_for_responses() {
    // Chat 客户端对话中途的 system 消息，转给 Responses 的上游：那边有原生的写法
    let body = json!({"model": "gpt-5.4", "messages": [
        {"role": "system", "content": "Be brief."},
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": "hello"},
        {"role": "system", "content": "The user is on mobile now."},
        {"role": "user", "content": "and?"}
    ]})
    .to_string();
    let p = prepare(
        Dialect::Chat,
        body.as_bytes(),
        "/v1/chat/completions",
        None,
        &target(Dialect::Responses),
    )
    .unwrap();
    let v = body_of(&p);
    assert_eq!(v["instructions"], "Be brief.");
    let roles: Vec<&str> = v["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "developer", "user"]);
    assert_eq!(
        v["input"][2]["content"][0]["text"],
        "The user is on mobile now."
    );
}

// ───────────────────────────────────────────────────────── 压缩

#[test]
fn a_compaction_request_asks_for_a_summary_on_the_same_prefix() {
    // 压缩请求和同一段历史上的普通一轮：系统提示、工具、对话都一样，只有最后一条不同
    let mut normal = history();
    normal.push(json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Next?"}]}));
    for upstream in UPSTREAMS {
        let p = prepared(&compaction_request(), upstream);
        assert!(p.session.is_compaction(), "{upstream:?}");
        assert!(
            !p.dropped.iter().any(|d| d.contains("compaction")),
            "{upstream:?}: {:?}",
            p.dropped
        );
        let c = body_of(&p);
        let n = body_of(&prepared(&request(normal.clone()), upstream));
        let (sys_c, tools_c, msgs_c) = parts_of(upstream, &c);
        let (sys_n, tools_n, msgs_n) = parts_of(upstream, &n);
        assert_eq!(sys_c, sys_n, "{upstream:?}");
        assert_eq!(tools_c, tools_n, "{upstream:?}");
        assert_eq!(
            msgs_c[..msgs_c.len() - 1],
            msgs_n[..msgs_n.len() - 1],
            "{upstream:?}"
        );
        // 最后一条是请上游写摘要；工具选择没动（动了 Anthropic 整段对话的缓存就作废）
        let last = msgs_c.last().unwrap().to_string();
        assert!(
            last.contains("Write that summary now"),
            "{upstream:?}: {last}"
        );
        assert_eq!(c.get("tool_choice"), n.get("tool_choice"), "{upstream:?}");
    }
}

fn named(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn data(v: Value) -> String {
    format!("data: {v}\n\n")
}

/// 上游写的摘要，按它的格式分很多片流回来。Anthropic 先想一想
fn summary_stream(d: Dialect) -> String {
    let pieces: Vec<String> = SUMMARY.split_inclusive(' ').map(str::to_string).collect();
    match d {
        Dialect::Anthropic => {
            let mut s = vec![
                named(
                    "message_start",
                    json!({"type": "message_start", "message": {"id": "msg_up", "model": "claude-opus-4-7",
                    "usage": {"input_tokens": 1200, "cache_read_input_tokens": 9000, "output_tokens": 1}}}),
                ),
                named(
                    "content_block_start",
                    json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
                ),
                named(
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Let me recall the work."}}),
                ),
                named(
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
                ),
                named(
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": 0}),
                ),
                named(
                    "content_block_start",
                    json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
                ),
            ];
            for p in &pieces {
                s.push(named("content_block_delta", json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": p}})));
            }
            s.push(named(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 1}),
            ));
            s.push(named("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 300}})));
            s.push(named("message_stop", json!({"type": "message_stop"})));
            s.concat()
        }
        Dialect::Chat => {
            let chunk = |delta: Value, finish: Value| {
                data(
                    json!({"id": "c", "object": "chat.completion.chunk", "model": "deepseek-chat",
                    "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}),
                )
            };
            let mut s = vec![chunk(
                json!({"role": "assistant", "content": ""}),
                Value::Null,
            )];
            for p in &pieces {
                s.push(chunk(json!({"content": p}), Value::Null));
            }
            s.push(chunk(json!({}), json!("stop")));
            s.push(data(json!({"id": "c", "object": "chat.completion.chunk", "model": "deepseek-chat", "choices": [],
                "usage": {"prompt_tokens": 10200, "completion_tokens": 300, "prompt_tokens_details": {"cached_tokens": 9000}}})));
            s.push("data: [DONE]\n\n".to_string());
            s.concat()
        }
        Dialect::Gemini => {
            let mut s: Vec<String> = pieces
                .iter()
                .map(|p| data(json!({"candidates": [{"content": {"role": "model", "parts": [{"text": p}]}}], "modelVersion": "gemini-2.5-pro"})))
                .collect();
            s.push(data(json!({"candidates": [{"content": {"role": "model", "parts": []}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 10200, "cachedContentTokenCount": 9000, "candidatesTokenCount": 300}})));
            s.concat()
        }
        Dialect::Bedrock => {
            let mut s = vec![named("messageStart", json!({"role": "assistant"}))];
            for p in &pieces {
                s.push(named(
                    "contentBlockDelta",
                    json!({"contentBlockIndex": 0, "delta": {"text": p}}),
                ));
            }
            s.push(named("contentBlockStop", json!({"contentBlockIndex": 0})));
            s.push(named("messageStop", json!({"stopReason": "end_turn"})));
            s.push(named("metadata", json!({"usage": {"inputTokens": 1200, "cacheReadInputTokens": 9000, "outputTokens": 300, "totalTokens": 10500}})));
            s.concat()
        }
        Dialect::Responses => unreachable!(),
    }
}

fn session(upstream: Dialect) -> Session {
    prepared(&compaction_request(), upstream).session
}

fn stream_events(s: &Session, upstream_bytes: &str) -> Vec<(String, Value)> {
    let mut c = s.stream();
    let mut out = Vec::new();
    for piece in upstream_bytes.as_bytes().chunks(7) {
        out.extend(c.process(piece));
    }
    out.extend(c.finish());
    let mut dec = Decoder::default();
    dec.feed(&out)
        .into_iter()
        .map(|f| (f.event.unwrap(), serde_json::from_str(&f.data).unwrap()))
        .collect()
}

/// Codex 收压缩的流时认的（`collect_compaction_output`）：`output_item.done` 里恰好一个
/// `compaction` 项，有 `response.completed`。返回那个项
fn as_codex_reads_it(events: &[(String, Value)], at: &str) -> Value {
    let done: Vec<&Value> = events
        .iter()
        .filter(|(k, _)| k == "response.output_item.done")
        .map(|(_, v)| &v["item"])
        .collect();
    let compactions: Vec<&&Value> = done.iter().filter(|i| i["type"] == "compaction").collect();
    assert_eq!(compactions.len(), 1, "{at}: {done:?}");
    // 只有这一个项：摘要的文字、思考都不另外往外写
    assert_eq!(done.len(), 1, "{at}: {done:?}");
    let completed = events
        .iter()
        .find(|(k, _)| k == "response.completed")
        .unwrap_or_else(|| panic!("{at}: no response.completed"));
    assert!(
        completed.1["response"]["id"].is_string(),
        "{at}: {completed:?}"
    );
    let item = (*compactions[0]).clone();
    // Codex 的 `ResponseItem::Compaction` 要的字段
    assert!(item["encrypted_content"].is_string(), "{at}: {item}");
    item
}

#[test]
fn the_summary_comes_back_to_codex_as_exactly_one_compaction_item() {
    for upstream in UPSTREAMS {
        let at = format!("{upstream:?}");
        let events = stream_events(&session(upstream), &summary_stream(upstream));
        let item = as_codex_reads_it(&events, &at);
        let enc = item["encrypted_content"].as_str().unwrap();
        assert!(enc.starts_with("tw1.c."), "{at}: {enc}");
        assert_eq!(compaction::read(enc).as_deref(), Some(SUMMARY), "{at}");
        // 写得久也不会被 Codex 当成断了：中间报着「还在写」
        assert!(
            events.iter().any(|(k, _)| k == "response.in_progress"),
            "{at}"
        );
        // 用量照常交回，Codex 记账用
        let (_, done) = events.last().unwrap();
        assert!(
            done["response"]["usage"]["output_tokens"].as_u64() > Some(0),
            "{at}: {done}"
        );
        assert_eq!(done["response"]["output"][0], item, "{at}");
    }
}

#[test]
fn a_whole_or_collected_answer_is_one_compaction_item_too() {
    for upstream in UPSTREAMS {
        // 上游给流、客户端要整包
        let mut c = session(upstream).collector();
        c.process(summary_stream(upstream).as_bytes());
        let v: Value = serde_json::from_slice(&c.finish().unwrap()).unwrap();
        let output = v["output"].as_array().unwrap();
        assert_eq!(output.len(), 1, "{upstream:?}: {v}");
        assert_eq!(output[0]["type"], "compaction");
        assert_eq!(
            compaction::read(output[0]["encrypted_content"].as_str().unwrap()).as_deref(),
            Some(SUMMARY),
            "{upstream:?}"
        );
        assert_eq!(v["status"], "completed");
    }
}

#[test]
fn an_answer_without_a_summary_fails_instead_of_wiping_the_conversation() {
    // 上游没听话，调了个工具、一个字没写：交回空摘要的话 Codex 会把前文全扔掉
    let s = session(Dialect::Anthropic);
    let up = [
        named("message_start", json!({"type": "message_start", "message": {"id": "m", "model": "claude-opus-4-7", "usage": {"input_tokens": 10, "output_tokens": 1}}})),
        named("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t", "name": "exec_command", "input": {}}})),
        named("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"cmd\":\"ls\"}"}})),
        named("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
        named("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 5}})),
        named("message_stop", json!({"type": "message_stop"})),
    ]
    .concat();
    let events = stream_events(&s, &up);
    assert!(
        !events
            .iter()
            .any(|(k, _)| k.starts_with("response.output_item")),
        "{events:?}"
    );
    let (last, v) = events.last().unwrap();
    assert_eq!(last, "response.failed");
    assert!(
        v["response"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("without writing a summary")
    );
    let whole = s
        .response(
            json!({"id": "m", "type": "message", "role": "assistant", "content": [],
                   "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}})
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    let v: Value = serde_json::from_slice(&whole).unwrap();
    assert_eq!(v["status"], "failed");
    assert_eq!(v["output"], json!([]));
}

/// 压缩之后 Codex 的历史：保留下来的用户消息、压缩项、重新写的上下文，接着是新的一轮
/// （`build_v2_compacted_history`、`build_compaction_replacement_history`）
fn after_compaction(item: Value) -> Value {
    request(vec![
        tools(),
        json!({"id": "msg_b", "type": "message", "role": "developer",
               "content": [{"type": "input_text", "text": "You are Codex, a coding agent."}]}),
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Fix the failing parser test."}]}),
        item,
        json!({"type": "message", "role": "developer",
               "content": [{"type": "input_text", "text": "<permissions instructions>sandbox: workspace-write</permissions instructions>"}]}),
        json!({"type": "message", "role": "user",
               "content": [{"type": "input_text", "text": "<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>"}]}),
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Now update the docs."}]}),
    ])
}

#[test]
fn the_next_request_reads_the_summary_back_in_its_place() {
    let item = as_codex_reads_it(
        &stream_events(
            &session(Dialect::Anthropic),
            &summary_stream(Dialect::Anthropic),
        ),
        "Anthropic",
    );
    for upstream in UPSTREAMS {
        let p = prepared(&after_compaction(item.clone()), upstream);
        assert!(!p.session.is_compaction());
        let sent = String::from_utf8(p.body.clone()).unwrap();
        assert!(!sent.contains("tw1.c."), "{upstream:?}: {sent}");
        let (sys, _, msgs) = parts_of(upstream, &body_of(&p));
        // 摘要不进系统提示，在对话里它原来的位置：保留的那句话之后、新的一轮之前
        assert!(
            !sys.to_string().contains("compacted"),
            "{upstream:?}: {sys}"
        );
        let convo = Value::Array(msgs).to_string();
        let (summary_at, kept_at, new_at) = (
            convo.find("parser test fixed").unwrap(),
            convo.find("Fix the failing parser test.").unwrap(),
            convo.find("Now update the docs.").unwrap(),
        );
        assert!(kept_at < summary_at && summary_at < new_at, "{upstream:?}");
        assert!(
            convo.contains(
                "<system-reminder>\\nThe earlier part of this conversation was compacted"
            ),
            "{upstream:?}: {convo}"
        );
    }

    // OpenAI 自己的压缩项照旧读不了
    let theirs = after_compaction(json!({"type": "compaction", "encrypted_content": "gAAAAABpZx"}));
    let e = decode(Dialect::Responses, &theirs, "/v1/responses", None).unwrap_err();
    assert!(e.0.contains("only OpenAI can read"), "{e}");
}

#[test]
fn passing_straight_through_to_openai_turns_our_summary_back_into_a_message() {
    let item = json!({"id": "cmp_1", "type": "compaction", "encrypted_content": compaction::carry(SUMMARY)});
    let body = after_compaction(item).to_string();
    let out: Value =
        serde_json::from_slice(&strip_carried(Dialect::Responses, body.as_bytes()).unwrap())
            .unwrap();
    let before: Value = serde_json::from_str(&body).unwrap();
    let (a, b) = (
        before["input"].as_array().unwrap(),
        out["input"].as_array().unwrap(),
    );
    assert_eq!(a.len(), b.len());
    // 只换了那一项，原位换成写着摘要的 developer 消息
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        if i == 3 {
            assert_eq!(y["type"], "message");
            assert_eq!(y["role"], "developer");
            assert_eq!(
                y["content"][0]["text"],
                compaction::restored(SUMMARY).as_str()
            );
        } else {
            assert_eq!(x, y);
        }
    }
    // 没有转换写出去的东西：一个字节都不改
    assert_eq!(
        strip_carried(
            Dialect::Responses,
            compaction_request().to_string().as_bytes()
        ),
        None
    );
}
