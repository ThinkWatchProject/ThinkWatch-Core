//! 视图：读、核对、写回。每种格式一组，外加随机改动的性质测试。

use serde_json::{Value, json};
use tw_api::Permission;
use tw_dialect::ir::Dialect;

use super::*;

fn all() -> Vec<Permission> {
    vec![
        Permission::System,
        Permission::Messages,
        Permission::Tools,
        Permission::Params,
    ]
}

/// 读成视图、让 `f` 改、核对、写回。返回写回之后的原文和新的路径
fn edit(
    d: Dialect,
    raw: &Value,
    path: &str,
    f: impl FnOnce(&mut Value),
) -> Result<(Value, Option<String>), EditError> {
    let built = build(d, raw, path).expect("builds");
    let input = trim(&built.view, &all());
    let mut out = input.clone();
    f(&mut out);
    let edits = check(&input, &out, &all(), built.src.hidden_tools())?;
    let mut next = raw.clone();
    let p = apply(&mut next, &built.src, &edits, path)?;
    Ok((next, p))
}

fn view_of(d: Dialect, raw: &Value, path: &str) -> Value {
    build(d, raw, path).expect("builds").view
}

/// 写回之后的请求，核心自己的解码器照样解得开
fn decodes(d: Dialect, raw: &Value, path: &str) {
    let query = (d == Dialect::Gemini).then_some("alt=sse");
    tw_dialect::convert::decode(d, raw, path, query)
        .unwrap_or_else(|e| panic!("does not decode: {e}\n{raw:#}"));
}

fn msgs(v: &mut Value) -> &mut Vec<Value> {
    v["messages"].as_array_mut().unwrap()
}

// ───────────────────────────────────────────────────────── Anthropic

fn anthropic() -> Value {
    json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 32000,
        "temperature": 1,
        "stream": true,
        "system": [
            { "type": "text", "text": "x-anthropic-billing-header: cc_version=2.1" },
            { "type": "text", "text": "You are Claude Code.", "cache_control": { "type": "ephemeral" } }
        ],
        "messages": [
            { "role": "user", "content": [
                { "type": "text", "text": "<system-reminder>ctx</system-reminder>" },
                { "type": "text", "text": "read a.txt", "cache_control": { "type": "ephemeral" } }
            ]},
            { "role": "assistant", "content": [
                { "type": "thinking", "thinking": "let me read", "signature": "sig-abc" },
                { "type": "text", "text": "Reading." },
                { "type": "tool_use", "id": "toolu_1", "name": "Read", "input": { "file_path": "/a.txt" } }
            ]},
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "toolu_1", "content": "hello" }
            ]},
            { "role": "user", "content": [
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo=" } },
                { "type": "text", "text": "what is this" }
            ]},
            { "role": "user", "content": "thanks" }
        ],
        "tools": [
            { "name": "Read", "description": "Read a file", "input_schema": { "type": "object", "properties": { "file_path": { "type": "string" } } } },
            { "type": "web_search_20250305", "name": "web_search", "max_uses": 5 },
            { "name": "Bash", "description": "Run a command", "input_schema": { "type": "object" }, "cache_control": { "type": "ephemeral" } }
        ],
        "metadata": { "user_id": "u-1" }
    })
}

const MESSAGES: &str = "/v1/messages";

#[test]
fn an_anthropic_request_reads_as_the_contract_says() {
    let v = view_of(Dialect::Anthropic, &anthropic(), MESSAGES);
    assert_eq!(v["format"], "anthropic");
    assert_eq!(v["model"], "claude-sonnet-4-5");
    assert_eq!(
        v["system"],
        "x-anthropic-billing-header: cc_version=2.1\n\nYou are Claude Code."
    );
    let roles: Vec<&str> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "tool", "user", "user"]);
    assert_eq!(
        v["messages"][1]["parts"][0],
        json!({ "key": "m1.p0", "type": "thinking", "text": "let me read" })
    );
    assert_eq!(
        v["messages"][1]["parts"][2],
        json!({ "key": "m1.p2", "type": "tool_call", "id": "toolu_1", "name": "Read", "input": { "file_path": "/a.txt" } })
    );
    assert_eq!(
        v["messages"][2]["parts"][0],
        json!({ "key": "m2.p0", "type": "tool_result", "call_id": "toolu_1", "text": "hello", "is_error": false })
    );
    // 图片只说是什么类型，不带数据
    assert_eq!(
        v["messages"][3]["parts"][0],
        json!({ "key": "m3.p0", "type": "image", "media_type": "image/png" })
    );
    assert_eq!(v["messages"][4]["parts"][0]["text"], "thanks");
    // 服务端工具看不见
    let names: Vec<&str> = v["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Read", "Bash"]);
    assert_eq!(
        v["params"],
        json!({ "model": "claude-sonnet-4-5", "max_tokens": 32000, "temperature": 1 })
    );
}

#[test]
fn returning_the_view_untouched_changes_nothing() {
    for (d, raw, path) in samples() {
        let built = build(d, &raw, &path).unwrap();
        let input = trim(&built.view, &all());
        let edits = check(&input, &input, &all(), built.src.hidden_tools()).unwrap();
        assert!(edits.is_empty(), "{d:?}: {edits:?}");
    }
}

/// 一个值进出一趟 JavaScript 之后的样子：数字都是双精度浮点，整数值写成不带小数点的
fn through_js(v: &Value) -> Value {
    match v {
        Value::Number(n) => {
            let f = n.as_f64().unwrap();
            if f.fract() == 0.0 && f.abs() < 1e21 {
                serde_json::from_str(&format!("{f:.0}")).unwrap()
            } else {
                json!(f)
            }
        }
        Value::Array(a) => Value::Array(a.iter().map(through_js).collect()),
        Value::Object(o) => {
            Value::Object(o.iter().map(|(k, v)| (k.clone(), through_js(v))).collect())
        }
        other => other.clone(),
    }
}

/// 插件没碰的数字回来变了写法（`2.0` → `2`、大整数丢了精度）不算改：只读的不报越权，
/// 能改的也不写回 —— 写回就丢了原来的写法，工具定义还会让缓存失效
#[test]
fn numbers_that_went_through_javascript_are_not_changes() {
    let mut raw = anthropic();
    raw["temperature"] = json!(1.0);
    raw["messages"][1]["content"][2]["input"] =
        json!({ "file_path": "/a.txt", "limit": 2.0, "seed": 12345678901234567890u64 });
    raw["tools"][0]["input_schema"]["properties"]["limit"] =
        json!({ "type": "number", "maximum": 2.0 });
    let built = build(Dialect::Anthropic, &raw, MESSAGES).unwrap();
    let input = trim(&built.view, &all());
    let back = through_js(&input);
    assert_ne!(
        back, input,
        "the round trip changed nothing, so this proves nothing"
    );
    let edits = check(&input, &back, &all(), built.src.hidden_tools()).unwrap();
    assert!(edits.is_empty(), "{edits:?}");

    // 只改了系统提示：别的照原样写回，一个字节都不动
    let mut sys = back.clone();
    sys["system"] = json!(format!(
        "{} Today is Friday.",
        sys["system"].as_str().unwrap()
    ));
    let edits = check(&input, &sys, &all(), built.src.hidden_tools()).unwrap();
    let mut next = raw.clone();
    apply(&mut next, &built.src, &edits, MESSAGES).unwrap();
    assert_ne!(next["system"], raw["system"]);
    for k in ["messages", "tools", "temperature"] {
        assert_eq!(next[k].to_string(), raw[k].to_string(), "{k}");
    }
}

#[test]
fn appending_to_the_anthropic_system_prompt_adds_a_block_after_the_cached_one() {
    let (raw, _) = edit(Dialect::Anthropic, &anthropic(), MESSAGES, |v| {
        let s = v["system"].as_str().unwrap().to_string();
        v["system"] = json!(format!("{s}\n\nToday is 2026-10-02."));
    })
    .unwrap();
    let sys = raw["system"].as_array().unwrap();
    assert_eq!(sys.len(), 3);
    assert_eq!(sys[1]["cache_control"], json!({ "type": "ephemeral" }));
    assert_eq!(
        sys[2],
        json!({ "type": "text", "text": "Today is 2026-10-02." })
    );
    // 别的一个字节都没动
    let mut rest = raw.clone();
    let mut orig = anthropic();
    rest.as_object_mut().unwrap().remove("system");
    orig.as_object_mut().unwrap().remove("system");
    assert_eq!(rest, orig);
    decodes(Dialect::Anthropic, &raw, MESSAGES);
}

#[test]
fn an_anthropic_system_string_and_its_removal() {
    let mut raw = anthropic();
    raw["system"] = json!("short");
    let (out, _) = edit(Dialect::Anthropic, &raw, MESSAGES, |v| {
        v["system"] = json!("longer")
    })
    .unwrap();
    assert_eq!(out["system"], "longer");
    let (out, _) = edit(Dialect::Anthropic, &raw, MESSAGES, |v| {
        v["system"] = json!("")
    })
    .unwrap();
    assert!(out.get("system").is_none());
    raw.as_object_mut().unwrap().remove("system");
    let (out, _) = edit(Dialect::Anthropic, &raw, MESSAGES, |v| {
        v["system"] = json!("new")
    })
    .unwrap();
    assert_eq!(out["system"], "new");
}

#[test]
fn editing_one_anthropic_part_touches_only_that_part() {
    let (raw, _) = edit(Dialect::Anthropic, &anthropic(), MESSAGES, |v| {
        msgs(v)[4]["parts"][0]["text"] = json!("thanks!");
        msgs(v)[1]["parts"][2]["input"] = json!({ "file_path": "/b.txt" });
        msgs(v)[2]["parts"][0]["text"] = json!("HELLO");
    })
    .unwrap();
    // 字符串内容还是字符串
    assert_eq!(raw["messages"][4]["content"], "thanks!");
    assert_eq!(
        raw["messages"][1]["content"][2]["input"],
        json!({ "file_path": "/b.txt" })
    );
    // 推理和签名原样
    assert_eq!(
        raw["messages"][1]["content"][0],
        anthropic()["messages"][1]["content"][0]
    );
    assert_eq!(raw["messages"][2]["content"][0]["content"], "HELLO");
    decodes(Dialect::Anthropic, &raw, MESSAGES);
}

#[test]
fn inserting_text_parts_and_messages_in_anthropic() {
    let (raw, _) = edit(Dialect::Anthropic, &anthropic(), MESSAGES, |v| {
        msgs(v)[0]["parts"]
            .as_array_mut()
            .unwrap()
            .insert(1, json!({ "type": "text", "text": "插进来的" }));
        msgs(v)[4]["parts"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "type": "text", "text": "and more" }));
        msgs(v).insert(
            0,
            json!({ "role": "user", "parts": [{ "type": "text", "text": "前情提要" }] }),
        );
        msgs(v).insert(
            1,
            json!({ "role": "assistant", "parts": [{ "type": "text", "text": "好" }, { "type": "text", "text": "继续" }] }),
        );
    })
    .unwrap();
    let m = raw["messages"].as_array().unwrap();
    assert_eq!(m.len(), 7);
    assert_eq!(m[0], json!({ "role": "user", "content": "前情提要" }));
    assert_eq!(
        m[1]["content"][1],
        json!({ "type": "text", "text": "继续" })
    );
    // 原来那块上的缓存断点还在它身上
    assert_eq!(m[2]["content"][1]["text"], "插进来的");
    assert_eq!(
        m[2]["content"][2]["cache_control"],
        json!({ "type": "ephemeral" })
    );
    // 字符串内容加了一段之后变成文字块
    assert_eq!(
        m[6]["content"],
        json!([{ "type": "text", "text": "thanks" }, { "type": "text", "text": "and more" }])
    );
    decodes(Dialect::Anthropic, &raw, MESSAGES);
}

#[test]
fn deleting_anthropic_messages_parts_and_tools() {
    let (raw, _) = edit(Dialect::Anthropic, &anthropic(), MESSAGES, |v| {
        msgs(v).remove(3);
        msgs(v)[0]["parts"].as_array_mut().unwrap().remove(0);
        v["tools"].as_array_mut().unwrap().remove(0);
    })
    .unwrap();
    assert_eq!(raw["messages"].as_array().unwrap().len(), 4);
    assert_eq!(raw["messages"][0]["content"].as_array().unwrap().len(), 1);
    // 看不见的服务端工具留着
    let names: Vec<&str> = raw["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["web_search", "Bash"]);
    decodes(Dialect::Anthropic, &raw, MESSAGES);
}

#[test]
fn anthropic_tools_and_params_are_edited_in_place() {
    let (raw, _) = edit(Dialect::Anthropic, &anthropic(), MESSAGES, |v| {
        v["tools"][1]["description"] = json!("Run a shell command");
        v["tools"][0]["input_schema"] = json!({ "type": "object", "properties": {} });
        v["tools"].as_array_mut().unwrap().push(
            json!({ "name": "Now", "description": "", "input_schema": { "type": "object" } }),
        );
        v["params"]["model"] = json!("claude-opus-4-5");
        v["params"].as_object_mut().unwrap().remove("temperature");
        v["params"]["stop"] = json!(["END"]);
        v["params"]["max_tokens"] = json!(1024);
    })
    .unwrap();
    assert_eq!(raw["tools"][2]["description"], "Run a shell command");
    assert_eq!(
        raw["tools"][2]["cache_control"],
        json!({ "type": "ephemeral" })
    );
    assert_eq!(
        raw["tools"][3],
        json!({ "name": "Now", "input_schema": { "type": "object" } })
    );
    assert_eq!(raw["model"], "claude-opus-4-5");
    assert!(raw.get("temperature").is_none());
    assert_eq!(raw["stop_sequences"], json!(["END"]));
    assert_eq!(raw["max_tokens"], 1024);
    decodes(Dialect::Anthropic, &raw, MESSAGES);
}

#[test]
fn rule_breaking_output_is_refused_with_the_right_kind() {
    use EditError::*;
    let run = |f: &dyn Fn(&mut Value)| {
        edit(Dialect::Anthropic, &anthropic(), MESSAGES, |v| f(v)).map(|_| ())
    };
    let kind = |r: Result<(), EditError>| match r {
        Err(PermissionViolation(_)) => "permission",
        Err(BadOutput(_)) => "bad",
        Ok(()) => "ok",
    };
    // 只读的
    assert_eq!(
        kind(run(&|v| v["messages"][1]["parts"][0]["text"] = json!("x"))),
        "permission"
    );
    assert_eq!(
        kind(run(
            &|v| v["messages"][1]["parts"][2]["name"] = json!("Bash")
        )),
        "permission"
    );
    assert_eq!(
        kind(run(&|v| v["messages"][1]["parts"][2]["id"] = json!("other"))),
        "permission"
    );
    assert_eq!(
        kind(run(
            &|v| v["messages"][2]["parts"][0]["call_id"] = json!("x")
        )),
        "permission"
    );
    assert_eq!(
        kind(run(
            &|v| v["messages"][2]["parts"][0]["is_error"] = json!(true)
        )),
        "permission"
    );
    assert_eq!(
        kind(run(
            &|v| v["messages"][3]["parts"][0]["media_type"] = json!("image/gif")
        )),
        "permission"
    );
    assert_eq!(
        kind(run(&|v| v["messages"][0]["role"] = json!("assistant"))),
        "permission"
    );
    assert_eq!(
        kind(run(
            &|v| v["messages"][0]["parts"][0]["type"] = json!("thinking")
        )),
        "permission"
    );
    assert_eq!(
        kind(run(&|v| v["tools"][0]["name"] = json!("Reader"))),
        "permission"
    );
    assert_eq!(kind(run(&|v| v["model"] = json!("other"))), "permission");
    assert_eq!(kind(run(&|v| v["format"] = json!("gemini"))), "permission");
    // key 的规矩
    assert_eq!(kind(run(&|v| v["messages"][0]["key"] = json!("m9"))), "bad");
    assert_eq!(kind(run(&|v| v["messages"][1]["key"] = json!("m0"))), "bad");
    assert_eq!(
        kind(run(&|v| {
            let m = msgs(v).remove(0);
            msgs(v).push(m);
        })),
        "bad"
    );
    assert_eq!(
        kind(run(
            &|v| v["messages"][4]["parts"][0]["key"] = json!("m0.p0")
        )),
        "bad"
    );
    assert_eq!(
        kind(run(&|v| {
            let p = v["messages"][0]["parts"][1].clone();
            v["messages"][4]["parts"].as_array_mut().unwrap().push(p);
        })),
        "bad"
    );
    assert_eq!(
        kind(run(&|v| {
            let ps = v["messages"][0]["parts"].as_array_mut().unwrap();
            ps.swap(0, 1);
        })),
        "bad"
    );
    // 新加的只能是文字
    assert_eq!(
        kind(run(&|v| msgs(v).push(
            json!({ "role": "user", "parts": [{ "type": "image", "media_type": null }] })
        ))),
        "bad"
    );
    assert_eq!(
        kind(run(&|v| msgs(v).push(
            json!({ "role": "tool", "parts": [{ "type": "text", "text": "x" }] })
        ))),
        "bad"
    );
    assert_eq!(
        kind(run(
            &|v| msgs(v).push(json!({ "role": "user", "parts": [] }))
        )),
        "bad"
    );
    assert_eq!(
        kind(run(&|v| v["messages"][0]["parts"]
            .as_array_mut()
            .unwrap()
            .push(
                json!({ "type": "tool_call", "id": "x", "name": "y", "input": {} })
            ))),
        "bad"
    );
    // Anthropic 的消息里没有 system 角色
    assert_eq!(
        kind(run(&|v| msgs(v).push(
            json!({ "role": "system", "parts": [{ "type": "text", "text": "x" }] })
        ))),
        "bad"
    );
    // 工具：重名（连看不见的服务端工具一起算）
    assert_eq!(
        kind(run(&|v| v["tools"].as_array_mut().unwrap().push(
            json!({ "name": "web_search", "description": "", "input_schema": {} })
        ))),
        "bad"
    );
    assert_eq!(
        kind(run(&|v| v["tools"].as_array_mut().unwrap().push(
            json!({ "name": "Read", "description": "", "input_schema": {} })
        ))),
        "bad"
    );
    assert_eq!(
        kind(run(
            &|v| v["tools"][0]["input_schema"] = json!("not an object")
        )),
        "bad"
    );
    // 参数的类型
    assert_eq!(kind(run(&|v| v["params"]["max_tokens"] = json!(0))), "bad");
    assert_eq!(
        kind(run(&|v| v["params"]["temperature"] = json!("hot"))),
        "bad"
    );
    assert_eq!(kind(run(&|v| v["params"]["stop"] = json!("END"))), "bad");
    assert_eq!(
        kind(run(&|v| {
            v["params"].as_object_mut().unwrap().remove("model");
        })),
        "bad"
    );
    // 不认识的字段
    assert_eq!(kind(run(&|v| v["extra"] = json!(1))), "bad");
    assert_eq!(
        kind(run(
            &|v| v["messages"][0]["parts"][0]["cache_control"] = json!({})
        )),
        "bad"
    );
    // Anthropic 的工具参数是对象
    assert_eq!(
        kind(run(&|v| v["messages"][1]["parts"][2]["input"] = json!([1]))),
        "bad"
    );
}

#[test]
fn sections_that_were_not_granted_are_not_seen_and_may_not_come_back() {
    let raw = anthropic();
    let built = build(Dialect::Anthropic, &raw, MESSAGES).unwrap();
    let only = vec![Permission::System];
    let input = trim(&built.view, &only);
    let keys: Vec<&String> = input.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["format", "model", "system"]);
    let mut out = input.clone();
    out["messages"] = built.view["messages"].clone();
    assert!(matches!(
        check(&input, &out, &only, &[]),
        Err(EditError::PermissionViolation(_))
    ));
    let mut out = input.clone();
    out["params"] = json!({ "model": "x" });
    assert!(matches!(
        check(&input, &out, &only, &[]),
        Err(EditError::PermissionViolation(_))
    ));
}

// ───────────────────────────────────────────────────────── Chat

fn chat() -> Value {
    json!({
        "model": "gpt-5",
        "max_completion_tokens": 4096,
        "stream": true,
        "stream_options": { "include_usage": true },
        "stop": "END",
        "messages": [
            { "role": "system", "content": "You are helpful." },
            { "role": "developer", "content": [{ "type": "text", "text": "Be brief." }] },
            { "role": "user", "content": [
                { "type": "text", "text": "what is in the image" },
                { "type": "image_url", "image_url": { "url": "data:image/jpeg;base64,/9j/4AAQ" } }
            ]},
            { "role": "assistant", "reasoning_content": "thinking…", "content": null, "tool_calls": [
                { "id": "call_1", "type": "function", "function": { "name": "lookup", "arguments": "{\"q\":\"cat\"}" } }
            ]},
            { "role": "tool", "tool_call_id": "call_1", "content": "a cat" },
            { "role": "system", "content": "mid-conversation note" },
            { "role": "user", "content": "thanks" }
        ],
        "tools": [
            { "type": "function", "function": { "name": "lookup", "description": "Look it up", "parameters": { "type": "object", "properties": { "q": { "type": "string" } } } } },
            { "type": "custom", "custom": { "name": "apply_patch" } }
        ]
    })
}

const CHAT: &str = "/v1/chat/completions";

#[test]
fn a_chat_request_reads_as_the_contract_says() {
    let v = view_of(Dialect::Chat, &chat(), CHAT);
    assert_eq!(v["format"], "openai_chat");
    assert_eq!(v["system"], "You are helpful.\n\nBe brief.");
    let roles: Vec<&str> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "tool", "system", "user"]);
    assert_eq!(v["messages"][0]["parts"][1]["media_type"], "image/jpeg");
    assert_eq!(v["messages"][1]["parts"][0]["type"], "thinking");
    assert_eq!(v["messages"][1]["parts"][1]["input"], json!({ "q": "cat" }));
    assert_eq!(v["messages"][2]["parts"][0]["call_id"], "call_1");
    assert_eq!(v["tools"].as_array().unwrap().len(), 1);
    assert_eq!(
        v["params"],
        json!({ "model": "gpt-5", "max_tokens": 4096, "stop": ["END"] })
    );
}

#[test]
fn chat_system_edits_land_on_the_leading_messages() {
    let (raw, _) = edit(Dialect::Chat, &chat(), CHAT, |v| {
        v["system"] = json!("You are helpful.\n\nBe brief.\n\nToday is Friday.");
    })
    .unwrap();
    let m = raw["messages"].as_array().unwrap();
    assert_eq!(m.len(), 8);
    // 新加的一段跟着最后一条的角色
    assert_eq!(
        m[2],
        json!({ "role": "developer", "content": "Today is Friday." })
    );
    assert_eq!(m[3]["role"], "user");
    let (raw, _) = edit(Dialect::Chat, &chat(), CHAT, |v| {
        v["system"] = json!("You are terse.\n\nBe brief.");
    })
    .unwrap();
    assert_eq!(
        raw["messages"][0],
        json!({ "role": "system", "content": "You are terse." })
    );
    assert_eq!(raw["messages"][1], chat()["messages"][1]);
    let (raw, _) = edit(Dialect::Chat, &chat(), CHAT, |v| v["system"] = json!("")).unwrap();
    assert_eq!(raw["messages"][0]["role"], "user");
    decodes(Dialect::Chat, &raw, CHAT);
}

#[test]
fn chat_parts_tool_calls_and_results_are_written_back_in_their_own_fields() {
    let (raw, _) = edit(Dialect::Chat, &chat(), CHAT, |v| {
        msgs(v)[1]["parts"][1]["input"] = json!({ "q": "dog" });
        msgs(v)[1]["parts"]
            .as_array_mut()
            .unwrap()
            .insert(1, json!({ "type": "text", "text": "Let me look." }));
        msgs(v)[2]["parts"][0]["text"] = json!("a dog");
        msgs(v)[4]["parts"][0]["text"] = json!("thank you");
        msgs(v)
            .push(json!({ "role": "system", "parts": [{ "type": "text", "text": "late note" }] }));
    })
    .unwrap();
    let m = raw["messages"].as_array().unwrap();
    assert_eq!(
        m[3]["tool_calls"][0]["function"]["arguments"],
        "{\"q\":\"dog\"}"
    );
    assert_eq!(m[3]["reasoning_content"], "thinking…");
    assert_eq!(m[3]["content"], json!("Let me look."));
    assert_eq!(m[4]["content"], "a dog");
    assert_eq!(m[6]["content"], "thank you");
    assert_eq!(m[7], json!({ "role": "system", "content": "late note" }));
    decodes(Dialect::Chat, &raw, CHAT);
}

#[test]
fn chat_deletions_and_format_rules() {
    let (raw, _) = edit(Dialect::Chat, &chat(), CHAT, |v| {
        // 删掉工具调用：消息里的 tool_calls 一起去掉
        msgs(v)[1]["parts"].as_array_mut().unwrap().remove(1);
        msgs(v).remove(2);
    })
    .unwrap();
    assert!(raw["messages"][3].get("tool_calls").is_none());
    assert_eq!(raw["messages"][3]["content"], "");
    decodes(Dialect::Chat, &raw, CHAT);
    // tool 消息就是它的结果：只能整条删
    let r = edit(Dialect::Chat, &chat(), CHAT, |v| {
        msgs(v)[2]["parts"].as_array_mut().unwrap().clear();
    });
    assert!(matches!(r, Err(EditError::BadOutput(_))), "{r:?}");
    // 参数：stop 原来是字符串，改了还是字符串；输出上限写回原来的字段
    let (raw, _) = edit(Dialect::Chat, &chat(), CHAT, |v| {
        v["params"]["stop"] = json!(["DONE"]);
        v["params"]["max_tokens"] = json!(100);
        v["params"]["temperature"] = json!(0.2);
    })
    .unwrap();
    assert_eq!(raw["stop"], "DONE");
    assert_eq!(raw["max_completion_tokens"], 100);
    assert!(raw.get("max_tokens").is_none());
    assert_eq!(raw["temperature"], 0.2);
    // 工具：新加的写成函数工具，看不见的自定义工具留着
    let (raw, _) = edit(Dialect::Chat, &chat(), CHAT, |v| {
        v["tools"].as_array_mut().unwrap().clear();
        v["tools"].as_array_mut().unwrap().push(
            json!({ "name": "now", "description": "Current time", "input_schema": { "type": "object", "properties": {} } }),
        );
    })
    .unwrap();
    assert_eq!(raw["tools"][0]["type"], "custom");
    assert_eq!(raw["tools"][1]["function"]["name"], "now");
    decodes(Dialect::Chat, &raw, CHAT);
}

// ───────────────────────────────────────────────────────── Responses

fn responses() -> Value {
    json!({
        "model": "gpt-5.1-codex",
        "instructions": "You are Codex.",
        "max_output_tokens": 8000,
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "input": [
            { "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": "<permissions>…</permissions>" }] },
            { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "list files" }] },
            { "type": "reasoning", "id": "rs_1", "summary": [{ "type": "summary_text", "text": "need ls" }], "encrypted_content": "gAAAA" },
            { "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "shell", "arguments": "{\"command\":[\"ls\"]}" },
            { "type": "function_call_output", "call_id": "call_1", "output": "a.txt\nb.txt" },
            { "type": "custom_tool_call", "call_id": "call_2", "name": "apply_patch", "input": "*** Begin Patch" },
            { "type": "custom_tool_call_output", "call_id": "call_2", "output": "done" },
            { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "Done." }] }
        ],
        "tools": [
            { "type": "function", "name": "shell", "description": "Run", "parameters": { "type": "object", "properties": { "command": { "type": "array" } } }, "strict": false },
            { "type": "custom", "name": "apply_patch", "description": "Patch" },
            { "type": "web_search" }
        ],
        "reasoning": { "effort": "medium", "summary": "auto" }
    })
}

const RESPONSES: &str = "/v1/responses";

#[test]
fn a_responses_request_reads_one_message_per_item() {
    let v = view_of(Dialect::Responses, &responses(), RESPONSES);
    assert_eq!(v["system"], "You are Codex.");
    let roles: Vec<&str> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        [
            "system",
            "user",
            "assistant",
            "assistant",
            "tool",
            "assistant",
            "tool",
            "assistant"
        ]
    );
    assert_eq!(v["messages"][2]["parts"][0]["type"], "thinking");
    assert_eq!(
        v["messages"][3]["parts"][0]["input"],
        json!({ "command": ["ls"] })
    );
    assert_eq!(v["messages"][5]["parts"][0]["input"], "*** Begin Patch");
    assert_eq!(v["messages"][4]["parts"][0]["text"], "a.txt\nb.txt");
    assert_eq!(v["tools"].as_array().unwrap().len(), 1);
    assert_eq!(
        v["params"],
        json!({ "model": "gpt-5.1-codex", "max_tokens": 8000 })
    );
}

#[test]
fn responses_edits_go_back_into_their_items() {
    let (raw, _) = edit(Dialect::Responses, &responses(), RESPONSES, |v| {
        v["system"] = json!("You are Codex. Today is Friday.");
        msgs(v)[1]["parts"][0]["text"] = json!("list all files");
        msgs(v)[3]["parts"][0]["input"] = json!({ "command": ["ls", "-la"] });
        msgs(v)[4]["parts"][0]["text"] = json!("a.txt");
        msgs(v)[5]["parts"][0]["input"] = json!("*** Begin Patch\n*** End Patch");
        msgs(v).insert(2, json!({ "role": "system", "parts": [{ "type": "text", "text": "注意安全" }] }));
        msgs(v)[8]["parts"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "type": "text", "text": "Anything else?" }));
        v["tools"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "name": "now", "description": "", "input_schema": { "type": "object", "properties": {} } }));
        v["params"]["temperature"] = json!(0.5);
    })
    .unwrap();
    assert_eq!(raw["instructions"], "You are Codex. Today is Friday.");
    let input = raw["input"].as_array().unwrap();
    assert_eq!(input[1]["content"][0]["text"], "list all files");
    assert_eq!(
        input[2],
        json!({ "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": "注意安全" }] })
    );
    // 推理项原样（加密内容还在）
    assert_eq!(input[3], responses()["input"][2]);
    assert_eq!(input[4]["arguments"], "{\"command\":[\"ls\",\"-la\"]}");
    assert_eq!(input[5]["output"], "a.txt");
    assert_eq!(input[6]["input"], "*** Begin Patch\n*** End Patch");
    assert_eq!(
        input[8]["content"][1],
        json!({ "type": "output_text", "text": "Anything else?" })
    );
    let tools = raw["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 4);
    assert_eq!(tools[3]["strict"], false);
    assert_eq!(raw["temperature"], 0.5);
    decodes(Dialect::Responses, &raw, RESPONSES);
}

#[test]
fn a_string_input_becomes_items_only_when_messages_change() {
    let raw = json!({ "model": "gpt-5", "input": "hi" });
    let v = view_of(Dialect::Responses, &raw, RESPONSES);
    assert_eq!(v["messages"][0]["parts"][0]["text"], "hi");
    let (out, _) = edit(Dialect::Responses, &raw, RESPONSES, |v| {
        v["system"] = json!("be nice");
    })
    .unwrap();
    assert_eq!(out["input"], "hi");
    let (out, _) = edit(Dialect::Responses, &raw, RESPONSES, |v| {
        msgs(v)[0]["parts"][0]["text"] = json!("hello");
        msgs(v).push(json!({ "role": "user", "parts": [{ "type": "text", "text": "again" }] }));
    })
    .unwrap();
    assert_eq!(
        out["input"],
        json!([
            { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "hello" }] },
            { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "again" }] }
        ])
    );
    decodes(Dialect::Responses, &out, RESPONSES);
}

#[test]
fn responses_has_no_stop_and_items_cannot_grow_parts() {
    let r = edit(Dialect::Responses, &responses(), RESPONSES, |v| {
        v["params"]["stop"] = json!(["x"]);
    });
    assert!(matches!(r, Err(EditError::BadOutput(_))), "{r:?}");
    let r = edit(Dialect::Responses, &responses(), RESPONSES, |v| {
        msgs(v)[3]["parts"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "type": "text", "text": "x" }));
    });
    assert!(matches!(r, Err(EditError::BadOutput(_))), "{r:?}");
}

// ───────────────────────────────────────────────────────── Gemini

fn gemini() -> Value {
    json!({
        "systemInstruction": { "parts": [{ "text": "You are Gemini CLI." }, { "text": "Be careful." }] },
        "contents": [
            { "role": "user", "parts": [{ "text": "read a.txt" }] },
            { "role": "model", "parts": [
                { "text": "planning", "thought": true, "thoughtSignature": "c2ln" },
                { "functionCall": { "name": "read_file", "args": { "path": "a.txt" } }, "thoughtSignature": "c2ln" }
            ]},
            { "role": "user", "parts": [{ "functionResponse": { "name": "read_file", "response": { "output": "hello" } } }] },
            { "role": "user", "parts": [
                { "inlineData": { "mimeType": "image/png", "data": "iVBORw0KGgo=" } },
                { "text": "and this?" }
            ]}
        ],
        "tools": [
            { "functionDeclarations": [
                { "name": "read_file", "description": "Read", "parametersJsonSchema": { "type": "object", "properties": { "path": { "type": "string" } } } },
                { "name": "ls", "description": "List", "parameters": { "type": "OBJECT" } }
            ]},
            { "googleSearch": {} }
        ],
        "generation_config": { "temperature": 0.7, "max_output_tokens": 2048, "thinkingConfig": { "includeThoughts": true } }
    })
}

const GEMINI: &str = "/v1beta/models/gemini-2.5-pro:streamGenerateContent";

#[test]
fn a_gemini_request_reads_its_model_from_the_path() {
    let v = view_of(Dialect::Gemini, &gemini(), GEMINI);
    assert_eq!(v["model"], "gemini-2.5-pro");
    assert_eq!(v["system"], "You are Gemini CLI.\n\nBe careful.");
    let roles: Vec<&str> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "tool", "user"]);
    assert_eq!(v["messages"][1]["parts"][1]["id"], "call_1_1");
    assert_eq!(v["messages"][2]["parts"][0]["call_id"], "call_1_1");
    assert_eq!(v["messages"][2]["parts"][0]["text"], "hello");
    assert_eq!(v["messages"][3]["parts"][0]["media_type"], "image/png");
    assert_eq!(v["tools"].as_array().unwrap().len(), 2);
    assert_eq!(
        v["params"],
        json!({ "model": "gemini-2.5-pro", "max_tokens": 2048, "temperature": 0.7 })
    );
}

#[test]
fn gemini_edits_keep_signatures_and_the_field_spelling() {
    let (raw, path) = edit(Dialect::Gemini, &gemini(), GEMINI, |v| {
        v["system"] = json!("You are Gemini CLI.\n\nBe very careful.");
        msgs(v)[1]["parts"][1]["input"] = json!({ "path": "b.txt" });
        msgs(v)[2]["parts"][0]["text"] = json!("HELLO");
        msgs(v)[3]["parts"][1]["text"] = json!("and that?");
        msgs(v).push(json!({ "role": "assistant", "parts": [{ "type": "text", "text": "ok" }] }));
        v["tools"][1]["input_schema"] = json!({ "type": "object", "properties": {} });
        v["tools"].as_array_mut().unwrap().push(
            json!({ "name": "now", "description": "", "input_schema": { "type": "object" } }),
        );
        v["params"]["model"] = json!("gemini-2.5-flash");
        v["params"]["max_tokens"] = json!(512);
        v["params"]["stop"] = json!(["END"]);
    })
    .unwrap();
    assert_eq!(
        path.as_deref(),
        Some("/v1beta/models/gemini-2.5-flash:streamGenerateContent")
    );
    assert_eq!(
        raw["systemInstruction"]["parts"][1]["text"],
        "Be very careful."
    );
    let c = raw["contents"].as_array().unwrap();
    assert_eq!(
        c[1]["parts"][1]["functionCall"]["args"],
        json!({ "path": "b.txt" })
    );
    assert_eq!(c[1]["parts"][1]["thoughtSignature"], "c2ln");
    assert_eq!(c[1]["parts"][0], gemini()["contents"][1]["parts"][0]);
    assert_eq!(
        c[2]["parts"][0]["functionResponse"]["response"],
        json!({ "output": "HELLO" })
    );
    assert_eq!(
        c[4],
        json!({ "role": "model", "parts": [{ "text": "ok" }] })
    );
    // 下划线写法的字段写回原来的那个
    assert_eq!(raw["generation_config"]["max_output_tokens"], 512);
    assert_eq!(raw["generation_config"]["stopSequences"], json!(["END"]));
    assert!(raw.get("generationConfig").is_none());
    let decls = raw["tools"][0]["functionDeclarations"].as_array().unwrap();
    assert_eq!(
        decls[1]["parameters"],
        json!({ "type": "object", "properties": {} })
    );
    assert_eq!(decls[2]["name"], "now");
    assert_eq!(raw["tools"][1], json!({ "googleSearch": {} }));
    decodes(Dialect::Gemini, &raw, path.as_deref().unwrap());
}

#[test]
fn gemini_has_no_system_role_in_contents() {
    let r = edit(Dialect::Gemini, &gemini(), GEMINI, |v| {
        msgs(v).push(json!({ "role": "system", "parts": [{ "type": "text", "text": "x" }] }));
    });
    assert!(matches!(r, Err(EditError::BadOutput(_))), "{r:?}");
    let r = edit(Dialect::Gemini, &gemini(), GEMINI, |v| {
        msgs(v)[1]["parts"][1]["input"] = json!("text");
    });
    assert!(matches!(r, Err(EditError::BadOutput(_))), "{r:?}");
}

#[test]
fn deleting_every_gemini_declaration_drops_that_tool_but_keeps_search() {
    let (raw, _) = edit(Dialect::Gemini, &gemini(), GEMINI, |v| {
        v["tools"].as_array_mut().unwrap().clear();
    })
    .unwrap();
    assert_eq!(raw["tools"], json!([{ "googleSearch": {} }]));
}

// ───────────────────────────────────────────────────────── 性质

/// 四种格式各一份有代表性的请求
fn samples() -> Vec<(Dialect, Value, String)> {
    vec![
        (Dialect::Anthropic, anthropic(), MESSAGES.to_string()),
        (Dialect::Chat, chat(), CHAT.to_string()),
        (Dialect::Responses, responses(), RESPONSES.to_string()),
        (Dialect::Gemini, gemini(), GEMINI.to_string()),
    ]
}

/// 一个够用的伪随机数：测试要能复现，不引新的依赖
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
    fn text(&mut self) -> String {
        const WORDS: &[&str] = &["日期", "hello", "\n\n", "<<", "}", "\"q\"", " ", "改", "x"];
        (0..1 + self.below(4))
            .map(|_| WORDS[self.below(WORDS.len())])
            .collect()
    }
}

/// 一次合规矩的随机改动：删、加、改能改的字段。格式自己的限制（Anthropic 和 Gemini
/// 不能加 system 消息之类）可能让写回报错，那也必须是报错而不是 panic
fn random_allowed_edit(rng: &mut Rng, v: &mut Value) {
    if rng.chance(30) {
        let s = v["system"].as_str().unwrap_or_default().to_string();
        v["system"] = json!(match rng.below(4) {
            0 => format!("{s}\n\n{}", rng.text()),
            1 => format!("{}\n\n{s}", rng.text()),
            2 => String::new(),
            _ => rng.text(),
        });
    }
    if let Some(ms) = v["messages"].as_array_mut() {
        for _ in 0..rng.below(3) {
            if !ms.is_empty() && rng.chance(40) {
                let i = rng.below(ms.len());
                ms.remove(i);
            }
        }
        for m in ms.iter_mut() {
            let Some(ps) = m["parts"].as_array_mut() else {
                continue;
            };
            for p in ps.iter_mut() {
                match p["type"].as_str() {
                    Some("text") if rng.chance(30) => p["text"] = json!(rng.text()),
                    Some("tool_call") if rng.chance(30) => {
                        p["input"] = json!({ "edited": rng.text() })
                    }
                    Some("tool_result") if rng.chance(30) => p["text"] = json!(rng.text()),
                    _ => {}
                }
            }
            if !ps.is_empty() && rng.chance(15) {
                let j = rng.below(ps.len());
                ps.remove(j);
            }
            if rng.chance(15) {
                let at = rng.below(ps.len() + 1);
                ps.insert(at, json!({ "type": "text", "text": rng.text() }));
            }
        }
        if rng.chance(30) {
            let role = ["user", "assistant", "system"][rng.below(3)];
            let at = rng.below(ms.len() + 1);
            ms.insert(
                at,
                json!({ "role": role, "parts": [{ "type": "text", "text": rng.text() }] }),
            );
        }
    }
    if let Some(ts) = v["tools"].as_array_mut() {
        if !ts.is_empty() && rng.chance(30) {
            let i = rng.below(ts.len());
            ts.remove(i);
        }
        for t in ts.iter_mut() {
            if rng.chance(30) {
                t["description"] = json!(rng.text());
            }
            if rng.chance(20) {
                t["input_schema"] =
                    json!({ "type": "object", "properties": { "a": { "type": "string" } } });
            }
        }
        if rng.chance(30) {
            let n = rng.next();
            ts.push(json!({ "name": format!("t{n}"), "description": rng.text(), "input_schema": { "type": "object" } }));
        }
    }
    if let Some(p) = v["params"].as_object_mut() {
        if rng.chance(30) {
            p.insert("model".into(), json!(format!("m-{}", rng.below(9))));
        }
        if rng.chance(30) {
            p.insert("max_tokens".into(), json!(1 + rng.below(9000)));
        }
        if rng.chance(30) {
            p.remove("temperature");
        }
        if rng.chance(20) {
            p.insert("top_p".into(), json!(0.5));
        }
    }
}

#[test]
fn random_allowed_edits_never_panic_and_the_result_still_decodes() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut applied = 0;
    for round in 0..400 {
        for (d, raw, path) in samples() {
            let built = build(d, &raw, &path).unwrap();
            let input = trim(&built.view, &all());
            let mut out = input.clone();
            random_allowed_edit(&mut rng, &mut out);
            let edits = match check(&input, &out, &all(), built.src.hidden_tools()) {
                Ok(e) => e,
                // 随机加的工具碰巧和看不见的重名之类：报错就行
                Err(_) => continue,
            };
            let mut next = raw.clone();
            match apply(&mut next, &built.src, &edits, &path) {
                Ok(p) => {
                    applied += 1;
                    let path = p.unwrap_or(path.clone());
                    decodes(d, &next, &path);
                    // 写回去的东西再读一遍还读得出来
                    build(d, &next, &path).unwrap_or_else(|e| panic!("round {round} {d:?}: {e}"));
                }
                Err(EditError::BadOutput(_)) => {}
                Err(e) => panic!("round {round} {d:?}: {e}"),
            }
        }
    }
    assert!(applied > 1000, "only {applied} edits were applied");
}

/// 什么样的返回值都不会让核对 panic：随机删字段、换类型、乱写 key
#[test]
fn random_garbage_never_panics() {
    let mut rng = Rng(42);
    let junk = |rng: &mut Rng| match rng.below(7) {
        0 => Value::Null,
        1 => json!(rng.below(5)),
        2 => json!("m0"),
        3 => json!([1, "x", null]),
        4 => json!({ "type": "text" }),
        5 => json!(true),
        _ => json!({}),
    };
    /// 往下走 `depth` 层，把碰到的那个值换成 `j`
    fn put(v: &mut Value, rng: &mut Rng, depth: usize, j: Value) {
        if depth > 0 {
            match v {
                Value::Object(m) if !m.is_empty() => {
                    let k = m.keys().nth(rng.below(m.len())).unwrap().clone();
                    return put(m.get_mut(&k).unwrap(), rng, depth - 1, j);
                }
                Value::Array(a) if !a.is_empty() => {
                    let i = rng.below(a.len());
                    return put(&mut a[i], rng, depth - 1, j);
                }
                _ => {}
            }
        }
        *v = j;
    }
    for _ in 0..2000 {
        for (d, raw, path) in samples() {
            let built = build(d, &raw, &path).unwrap();
            let input = trim(&built.view, &all());
            let mut out = input.clone();
            for _ in 0..1 + rng.below(3) {
                let depth = rng.below(5);
                let j = junk(&mut rng);
                put(&mut out, &mut rng, depth, j);
            }
            if let Ok(edits) = check(&input, &out, &all(), built.src.hidden_tools()) {
                let mut next = raw.clone();
                let _ = apply(&mut next, &built.src, &edits, &path);
            }
        }
    }
}

#[test]
fn placeholders_in_edits_are_revealed_before_write_back() {
    use crate::plugin::bridge::Bridge;
    const KEY: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";
    let mut raw = anthropic();
    raw["messages"][4]["content"] = json!(format!("my key is {KEY}"));
    let mut bridge = Bridge::new(std::sync::Arc::new(
        tw_guard::redact::rules::RuleSet::defaults(),
    ));
    bridge.learn(raw.to_string().as_bytes());
    let built = build(Dialect::Anthropic, &raw, MESSAGES).unwrap();
    let mut input = trim(&built.view, &all());
    bridge.hide_value(&mut input);
    assert!(!input.to_string().contains(KEY));
    let mut out = input.clone();
    let shown = out["messages"][4]["parts"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(shown.contains("<<TW_SECRET_1>>"), "{shown}");
    out["messages"][4]["parts"][0]["text"] = json!(format!("{shown} (rotated)"));
    let mut edits = check(&input, &out, &all(), built.src.hidden_tools()).unwrap();
    edits.reveal(&bridge);
    let mut next = raw.clone();
    apply(&mut next, &built.src, &edits, MESSAGES).unwrap();
    assert_eq!(
        next["messages"][4]["content"],
        format!("my key is {KEY} (rotated)")
    );
}
