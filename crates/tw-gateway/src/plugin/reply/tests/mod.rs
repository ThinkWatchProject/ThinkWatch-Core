//! 回答钩子：四种格式的流和整包，按帧看改了什么、没改什么。

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tw_api::{Permission, ReplyMode};
use tw_dialect::ir::Dialect;

use super::*;
use crate::plugin::host::ToolCallOutcome;
use crate::plugin::host::double::{Closures, Double};

const KEY: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";

fn state() -> crate::AppState {
    crate::AppState::new(tw_config::Config {
        clients: vec![tw_config::Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        ..Default::default()
    })
    .unwrap()
}

fn set_of(doubles: Vec<Double>) -> PluginSet {
    PluginSet::new(
        doubles
            .into_iter()
            .enumerate()
            .map(|(i, d)| Arc::new(crate::plugin::host::double::active(&format!("p{i}"), d)))
            .collect(),
    )
}

/// 一条插件链。请求体里认得出的密钥记进账（回答里出现时插件看到的是占位符）
async fn chain_with(
    state: &crate::AppState,
    set: &PluginSet,
    dialect: Dialect,
    request: &Value,
) -> Chain {
    let mut bridge = Bridge::new(Arc::new(tw_guard::redact::rules::RuleSet::defaults()));
    bridge.learn(request.to_string().as_bytes());
    Chain::start(
        state,
        set,
        bridge,
        &ReplyCtx {
            dialect,
            client: None,
            model: "m",
            upstream: "u",
            request_id: 1,
        },
    )
    .await
    .unwrap()
    .expect("a plugin is in scope")
}

async fn chain_of(dialect: Dialect, doubles: Vec<Double>, request: &Value) -> Chain {
    chain_with(&state(), &set_of(doubles), dialect, request).await
}

fn upper() -> Double {
    Double::new("upper")
        .permit(&[Permission::ReplyText])
        .on_text(|t| Some(t.to_uppercase()))
}

/// 流式：每段都先扣着，块结束时整段大写交出来
fn hold_until_end() -> Double {
    Double::new("hold")
        .permit(&[Permission::ReplyText])
        .mode(ReplyMode::Stream)
        .on_reply(true, true, false, |_| {
            let buf = Arc::new(Mutex::new(String::new()));
            let (a, b) = (buf.clone(), buf);
            Ok(Box::new(Closures {
                text: Box::new(move |t| {
                    a.lock().unwrap().push_str(t);
                    Invocation::ok(Some(String::new()))
                }),
                end: Box::new(move || {
                    let s = std::mem::take(&mut *b.lock().unwrap());
                    Invocation::ok(Some(s.to_uppercase()))
                }),
                tool: Box::new(|_| Invocation::ok(ToolCallOutcome::Unchanged)),
            }))
        })
}

fn tools(f: impl Fn(Value) -> ToolCallOutcome + Send + Sync + 'static) -> Double {
    Double::new("tools")
        .permit(&[Permission::ReplyToolCalls])
        .on_tool_call(f)
}

/// 整条流按 `step` 个字节一块喂进去
async fn run(s: &mut Stream, input: &str, step: usize) -> (String, Option<GatewayError>) {
    let mut out = Vec::new();
    for c in input.as_bytes().chunks(step.max(1)) {
        let (o, e) = s.feed(c).await;
        out.extend(o);
        if e.is_some() {
            return (String::from_utf8(out).unwrap(), e);
        }
    }
    let (o, e) = s.finish(false).await;
    out.extend(o);
    (String::from_utf8(out).unwrap(), e)
}

fn frames(s: &str) -> Vec<(String, Value)> {
    let mut d = tw_dialect::frame::Decoder::default();
    let mut f = d.feed(s.as_bytes());
    f.extend(d.flush());
    f.into_iter()
        .map(|f| {
            (
                f.event.unwrap_or_default(),
                serde_json::from_str(&f.data).unwrap_or(Value::String(f.data)),
            )
        })
        .collect()
}

fn ev(kind: &str, v: Value) -> String {
    format!("event: {kind}\ndata: {v}\n\n")
}

fn data(v: Value) -> String {
    format!("data: {v}\n\n")
}

// ───────────────────────────────────────────────────────── Anthropic

fn anthropic_stream() -> String {
    [
        ev("message_start", json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"m","content":[],"usage":{"input_tokens":3,"output_tokens":1}}})),
        ev("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}})),
        ev("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}})),
        ev("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}})),
        ev("content_block_stop", json!({"type":"content_block_stop","index":0})),
        ev("content_block_start", json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}})),
        ev("content_block_delta", json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hel"}})),
        "event: ping\ndata: {\"type\": \"ping\"}\n\n".to_string(),
        ev("content_block_delta", json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"lo 世界"}})),
        ev("content_block_stop", json!({"type":"content_block_stop","index":1})),
        ev("content_block_start", json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}})),
        ev("content_block_delta", json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}})),
        ev("content_block_delta", json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"ls\"}"}})),
        ev("content_block_stop", json!({"type":"content_block_stop","index":2})),
        ev("content_block_start", json!({"type":"content_block_start","index":3,"content_block":{"type":"text","text":""}})),
        ev("content_block_delta", json!({"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"bye"}})),
        ev("content_block_stop", json!({"type":"content_block_stop","index":3})),
        ev("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":9}})),
        ev("message_stop", json!({"type":"message_stop"})),
    ]
    .concat()
}

/// Anthropic 的输出按块收起来：(序号, 类型, 文字或参数)
fn anthropic_blocks(out: &str) -> Vec<(u64, String, String)> {
    let mut blocks: Vec<(u64, String, String)> = Vec::new();
    for (_, v) in frames(out) {
        match v["type"].as_str() {
            Some("content_block_start") => blocks.push((
                v["index"].as_u64().unwrap(),
                v["content_block"]["type"].as_str().unwrap().to_string(),
                v["content_block"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )),
            Some("content_block_delta") => {
                let i = v["index"].as_u64().unwrap();
                let b = blocks
                    .iter_mut()
                    .rev()
                    .find(|b| b.0 == i)
                    .expect("a delta for an open block");
                for k in ["text", "partial_json", "thinking"] {
                    if let Some(s) = v["delta"][k].as_str() {
                        b.2.push_str(s);
                    }
                }
            }
            _ => {}
        }
    }
    blocks
}

#[tokio::test]
async fn anthropic_text_blocks_are_rewritten_whole_and_everything_else_is_untouched() {
    let input = anthropic_stream();
    let mut s = Stream::new(
        chain_of(Dialect::Anthropic, vec![upper()], &json!({})).await,
        Framing::Sse,
    );
    let (out, err) = run(&mut s, &input, 4096).await;
    assert!(err.is_none());
    let blocks = anthropic_blocks(&out);
    assert_eq!(blocks[0], (0, "thinking".into(), "hmm".into()));
    assert_eq!(blocks[1], (1, "text".into(), "HELLO 世界".into()));
    assert_eq!(blocks[2].1, "tool_use");
    assert_eq!(blocks[3], (3, "text".into(), "BYE".into()));
    // 推理块、工具调用、心跳、结尾那几帧一个字节都没动
    let input_frames: Vec<&str> = input.split_inclusive("\n\n").collect();
    for (n, f) in input_frames.iter().enumerate() {
        if f.contains("thinking")
            || f.contains("tool_use")
            || f.contains("input_json")
            || f.contains("ping")
            || f.contains("message_")
        {
            assert!(out.contains(f), "frame {n} is gone: {f}");
        }
    }
}

#[tokio::test]
async fn the_same_stream_cut_at_any_byte_comes_out_the_same() {
    let input = anthropic_stream();
    let mut whole = Stream::new(
        chain_of(Dialect::Anthropic, vec![upper()], &json!({})).await,
        Framing::Sse,
    );
    let (want, _) = run(&mut whole, &input, input.len()).await;
    for step in [1, 2, 3, 7, 64] {
        let mut s = Stream::new(
            chain_of(Dialect::Anthropic, vec![upper()], &json!({})).await,
            Framing::Sse,
        );
        let (got, _) = run(&mut s, &input, step).await;
        assert_eq!(
            anthropic_blocks(&got),
            anthropic_blocks(&want),
            "step {step}"
        );
    }
}

#[tokio::test]
async fn a_plugin_that_changes_nothing_changes_nothing() {
    let input = anthropic_stream();
    // 只看工具调用、都不改：一个字节都不动
    let mut s = Stream::new(
        chain_of(
            Dialect::Anthropic,
            vec![tools(|_| ToolCallOutcome::Unchanged)],
            &json!({}),
        )
        .await,
        Framing::Sse,
    );
    let (out, _) = run(&mut s, &input, 5).await;
    assert_eq!(out, input);
    // 整块模式不改文字：块还是那些块、字还是那些字（整块交出来，增量并成了一段）
    let same = Double::new("same")
        .permit(&[Permission::ReplyText])
        .on_text(|_| None);
    let mut s = Stream::new(
        chain_of(Dialect::Anthropic, vec![same], &json!({})).await,
        Framing::Sse,
    );
    let (out, _) = run(&mut s, &input, 5).await;
    assert_eq!(anthropic_blocks(&out), anthropic_blocks(&input));
}

#[tokio::test]
async fn stream_mode_holds_back_and_flushes_at_the_end_of_the_block() {
    let input = anthropic_stream();
    let mut s = Stream::new(
        chain_of(Dialect::Anthropic, vec![hold_until_end()], &json!({})).await,
        Framing::Sse,
    );
    let (out, _) = run(&mut s, &input, 4096).await;
    let blocks = anthropic_blocks(&out);
    assert_eq!(blocks[1].2, "HELLO 世界");
    assert_eq!(blocks[3].2, "BYE");
    // 扣着的那几段没有发出去：每块只有补出来的那一段
    let text_deltas = frames(&out)
        .iter()
        .filter(|(_, v)| v["delta"]["type"] == "text_delta")
        .count();
    assert_eq!(text_deltas, 2, "{out}");
}

#[tokio::test]
async fn a_replaced_tool_call_is_written_whole_and_later_blocks_move_up() {
    let input = anthropic_stream();
    let two = tools(|call| {
        assert_eq!(call["name"], "Bash");
        assert_eq!(call["input"], json!({ "command": "ls" }));
        ToolCallOutcome::Replace(vec![
            json!({ "name": "Read", "input": { "path": "a" } }),
            json!({ "id": "keep-me", "name": "Read", "input": { "path": "b" } }),
        ])
    });
    let mut s = Stream::new(
        chain_of(Dialect::Anthropic, vec![two], &json!({})).await,
        Framing::Sse,
    );
    let (out, _) = run(&mut s, &input, 4096).await;
    let blocks = anthropic_blocks(&out);
    let idx: Vec<u64> = blocks.iter().map(|b| b.0).collect();
    assert_eq!(idx, [0, 1, 2, 3, 4]);
    assert_eq!(
        (blocks[2].1.as_str(), blocks[2].2.as_str()),
        ("tool_use", "Read{\"path\":\"a\"}")
    );
    assert_eq!(blocks[3].2, "Read{\"path\":\"b\"}");
    assert_eq!(blocks[4], (4, "text".into(), "bye".into()));
    assert!(out.contains("\"id\":\"keep-me\""), "{out}");
    // 序号挪过之后每一帧都对得上
    let stops: Vec<u64> = frames(&out)
        .iter()
        .filter(|(_, v)| v["type"] == "content_block_stop")
        .map(|(_, v)| v["index"].as_u64().unwrap())
        .collect();
    assert_eq!(stops, [0, 1, 2, 3, 4]);
}

#[tokio::test]
async fn dropping_every_tool_call_ends_the_turn_instead_of_waiting_for_results() {
    let input = anthropic_stream();
    // 交回 null 和交回空数组是同一个意思
    for drop in [
        tools(|_| ToolCallOutcome::Drop),
        tools(|_| ToolCallOutcome::Replace(Vec::new())),
    ] {
        let mut s = Stream::new(
            chain_of(Dialect::Anthropic, vec![drop], &json!({})).await,
            Framing::Sse,
        );
        let (out, _) = run(&mut s, &input, 4096).await;
        let blocks = anthropic_blocks(&out);
        let idx: Vec<u64> = blocks.iter().map(|b| b.0).collect();
        assert_eq!(idx, [0, 1, 2]);
        assert_eq!(blocks[2], (2, "text".into(), "bye".into()));
        let stop = frames(&out)
            .into_iter()
            .find(|(_, v)| v["type"] == "message_delta")
            .unwrap();
        assert_eq!(stop.1["delta"]["stop_reason"], "end_turn");
    }
}

/// 进出一趟 JavaScript 的调用：`2.0` 回来是 `2`，超过 2^53 的整数丢了精度 —— 这还是
/// 原来那个调用，不能算改过
#[test]
fn a_call_that_went_through_javascript_unchanged_is_the_same_call() {
    let given = json!({"id": "t1", "name": "Read",
                       "input": {"limit": 2.0, "seed": 12345678901234567890u64}});
    let back = json!({"id": "t1", "name": "Read",
                      "input": {"seed": 12345678901234567000u64, "limit": 2}});
    assert!(same_call(&back, &given));
    let other = json!({"id": "t1", "name": "Read", "input": {"limit": 3, "seed": 1}});
    assert!(!same_call(&other, &given));
}

#[tokio::test]
async fn plugins_chain_and_each_sees_the_previous_ones_output() {
    let input = anthropic_stream();
    let exclaim = Double::new("exclaim")
        .permit(&[Permission::ReplyText])
        .on_text(|t| Some(format!("{t}!")));
    let mut s = Stream::new(
        chain_of(Dialect::Anthropic, vec![upper(), exclaim], &json!({})).await,
        Framing::Sse,
    );
    let (out, _) = run(&mut s, &input, 4096).await;
    assert_eq!(anthropic_blocks(&out)[1].2, "HELLO 世界!");
}

#[tokio::test]
async fn reply_text_carries_placeholders_into_the_plugin_and_real_values_out() {
    let request = json!({ "messages": [{ "role": "user", "content": format!("key {KEY}") }] });
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let s2 = seen.clone();
    let spy = Double::new("spy")
        .permit(&[Permission::ReplyText])
        .mode(ReplyMode::Stream)
        .on_reply(true, false, false, move |_| {
            let s3 = s2.clone();
            Ok(Box::new(Closures {
                text: Box::new(move |t| {
                    s3.lock().unwrap().push(t.to_string());
                    Invocation::ok(Some(format!("[{t}]")))
                }),
                end: Box::new(|| Invocation::ok(None)),
                tool: Box::new(|_| Invocation::ok(ToolCallOutcome::Unchanged)),
            }))
        });
    // 上游流里的真值（还原之后的样子），被切在两段中间
    let input = [
        ev("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
        ev("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text": format!("use {}", &KEY[..12])}})),
        ev("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text": format!("{} now", &KEY[12..])}})),
        ev("content_block_stop", json!({"type":"content_block_stop","index":0})),
    ]
    .concat();
    let mut s = Stream::new(
        chain_of(Dialect::Anthropic, vec![spy], &request).await,
        Framing::Sse,
    );
    let (out, _) = run(&mut s, &input, 4096).await;
    let seen = seen.lock().unwrap().clone();
    assert!(seen.iter().all(|t| !t.contains(&KEY[..12])), "{seen:?}");
    assert!(seen.concat().contains("<<TW_SECRET_1>>"), "{seen:?}");
    let text: String = anthropic_blocks(&out)[0].2.clone();
    assert!(text.contains(KEY), "the key did not come back: {text}");
}

#[tokio::test]
async fn a_failing_plugin_cuts_under_reject_and_steps_aside_under_skip() {
    let input = anthropic_stream();
    let boom = || {
        Double::new("boom")
            .permit(&[Permission::ReplyText])
            .on_reply(true, false, false, |_| {
                Ok(Box::new(Closures {
                    text: Box::new(|_| Invocation::err(RunError::CpuLimit)),
                    end: Box::new(|| Invocation::ok(None)),
                    tool: Box::new(|_| Invocation::ok(ToolCallOutcome::Unchanged)),
                }))
            })
    };
    let mut s = Stream::new(
        chain_of(Dialect::Anthropic, vec![boom()], &json!({})).await,
        Framing::Sse,
    );
    let (out, err) = run(&mut s, &input, 4096).await;
    let err = err.expect("rejects");
    assert_eq!(err.detail.code, "gw.plugin.reply_failed");
    // 出错之前的那几帧照发，文字一个字都没漏出去
    assert!(out.contains("message_start"));
    assert!(!out.contains("text_delta"), "{out}");

    // 跳过：这个插件拿掉，文字原样
    let mut chain = chain_of(Dialect::Anthropic, vec![boom(), upper()], &json!({})).await;
    chain.stages[0].on_error = OnError::Skip;
    let mut s = Stream::new(chain, Framing::Sse);
    let (out, err) = run(&mut s, &input, 4096).await;
    assert!(err.is_none());
    assert_eq!(anthropic_blocks(&out)[1].2, "HELLO 世界");
}

// ───────────────────────────────────────────────────────── Chat

fn chat_stream() -> String {
    let chunk = |delta: Value, finish: Value| {
        data(
            json!({"id":"c1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":delta,"finish_reason":finish}]}),
        )
    };
    [
        chunk(json!({"role":"assistant","content":""}), Value::Null),
        chunk(json!({"content":"hel"}), Value::Null),
        chunk(json!({"content":"lo"}), Value::Null),
        chunk(json!({"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":""}}]}), Value::Null),
        chunk(json!({"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":"}}]}), Value::Null),
        chunk(json!({"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]}), Value::Null),
        chunk(json!({"tool_calls":[{"index":1,"id":"call_2","type":"function","function":{"name":"read","arguments":"{\"p\":1}"}}]}), Value::Null),
        chunk(json!({}), json!("tool_calls")),
        data(json!({"id":"c1","object":"chat.completion.chunk","created":1,"model":"m","choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2}})),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat()
}

/// Chat 的输出：(正文, [(序号, id, 名字, 参数)], finish_reason)
/// (序号, id, 名字, 参数)
type ChatCall = (u64, String, String, String);

fn chat_reading(out: &str) -> (String, Vec<ChatCall>, String) {
    let mut text = String::new();
    let mut calls: Vec<ChatCall> = Vec::new();
    let mut finish = String::new();
    for (_, v) in frames(out) {
        let Some(c) = v["choices"].get(0) else {
            continue;
        };
        if let Some(t) = c["delta"]["content"].as_str() {
            text.push_str(t);
        }
        for e in c["delta"]["tool_calls"].as_array().into_iter().flatten() {
            let i = e["index"].as_u64().unwrap();
            match calls.iter_mut().find(|x| x.0 == i) {
                Some(x) => {
                    x.3.push_str(e["function"]["arguments"].as_str().unwrap_or_default())
                }
                None => calls.push((
                    i,
                    e["id"].as_str().unwrap_or_default().into(),
                    e["function"]["name"].as_str().unwrap_or_default().into(),
                    e["function"]["arguments"]
                        .as_str()
                        .unwrap_or_default()
                        .into(),
                )),
            }
        }
        if let Some(f) = c["finish_reason"].as_str() {
            finish = f.into();
        }
    }
    (text, calls, finish)
}

#[tokio::test]
async fn chat_text_and_tool_calls_come_out_in_chat_shape() {
    let input = chat_stream();
    let first_only = tools(|call| {
        if call["name"] == "shell" {
            ToolCallOutcome::Replace(vec![json!({ "name": "shell", "input": { "cmd": "pwd" } })])
        } else {
            ToolCallOutcome::Drop
        }
    });
    let mut s = Stream::new(
        chain_of(Dialect::Chat, vec![upper(), first_only], &json!({})).await,
        Framing::Sse,
    );
    let (out, err) = run(&mut s, &input, 9).await;
    assert!(err.is_none());
    let (text, calls, finish) = chat_reading(&out);
    assert_eq!(text, "HELLO");
    assert_eq!(calls.len(), 1, "{out}");
    assert_eq!(
        (calls[0].0, calls[0].2.as_str(), calls[0].3.as_str()),
        (0, "shell", "{\"cmd\":\"pwd\"}")
    );
    assert_eq!(finish, "tool_calls");
    assert!(out.ends_with("data: [DONE]\n\n"));
    assert!(out.contains("\"usage\""));

    let mut s = Stream::new(
        chain_of(
            Dialect::Chat,
            vec![tools(|_| ToolCallOutcome::Drop)],
            &json!({}),
        )
        .await,
        Framing::Sse,
    );
    let (out, _) = run(&mut s, &input, 4096).await;
    let (text, calls, finish) = chat_reading(&out);
    assert_eq!(text, "hello");
    assert!(calls.is_empty());
    assert_eq!(finish, "stop");
}

#[tokio::test]
async fn chat_calls_that_pass_untouched_keep_their_ids_and_order() {
    let input = chat_stream();
    let mut s = Stream::new(
        chain_of(
            Dialect::Chat,
            vec![tools(|_| ToolCallOutcome::Unchanged)],
            &json!({}),
        )
        .await,
        Framing::Sse,
    );
    let (out, _) = run(&mut s, &input, 4096).await;
    let (_, calls, finish) = chat_reading(&out);
    assert_eq!(
        calls,
        [
            (
                0,
                "call_1".into(),
                "shell".into(),
                "{\"cmd\":\"ls\"}".into()
            ),
            (1, "call_2".into(), "read".into(), "{\"p\":1}".into())
        ]
    );
    assert_eq!(finish, "tool_calls");
}

// ───────────────────────────────────────────────────────── Responses

fn responses_stream() -> String {
    let e = |kind: &str, seq: u64, mut v: Value| {
        v["type"] = json!(kind);
        v["sequence_number"] = json!(seq);
        ev(kind, v)
    };
    [
        e("response.created", 0, json!({"response":{"id":"resp_1","status":"in_progress","output":[]}})),
        e("response.output_item.added", 1, json!({"output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","content":[]}})),
        e("response.content_part.added", 2, json!({"output_index":0,"content_index":0,"item_id":"msg_1","part":{"type":"output_text","text":""}})),
        e("response.output_text.delta", 3, json!({"output_index":0,"content_index":0,"item_id":"msg_1","delta":"hel"})),
        e("response.output_text.delta", 4, json!({"output_index":0,"content_index":0,"item_id":"msg_1","delta":"lo"})),
        e("response.output_text.done", 5, json!({"output_index":0,"content_index":0,"item_id":"msg_1","text":"hello"})),
        e("response.content_part.done", 6, json!({"output_index":0,"content_index":0,"item_id":"msg_1","part":{"type":"output_text","text":"hello"}})),
        e("response.output_item.done", 7, json!({"output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","content":[{"type":"output_text","text":"hello"}]}})),
        e("response.output_item.added", 8, json!({"output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"shell","arguments":""}})),
        e("response.function_call_arguments.delta", 9, json!({"output_index":1,"item_id":"fc_1","delta":"{\"command\":[\"ls\"]}"})),
        e("response.function_call_arguments.done", 10, json!({"output_index":1,"item_id":"fc_1","arguments":"{\"command\":[\"ls\"]}"})),
        e("response.output_item.done", 11, json!({"output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"shell","arguments":"{\"command\":[\"ls\"]}","status":"completed"}})),
        e("response.output_item.added", 12, json!({"output_index":2,"item":{"type":"message","id":"msg_2","role":"assistant","content":[]}})),
        e("response.output_text.delta", 13, json!({"output_index":2,"content_index":0,"item_id":"msg_2","delta":"bye"})),
        e("response.output_text.done", 14, json!({"output_index":2,"content_index":0,"item_id":"msg_2","text":"bye"})),
        e("response.output_item.done", 15, json!({"output_index":2,"item":{"type":"message","id":"msg_2","role":"assistant","content":[{"type":"output_text","text":"bye"}]}})),
        e("response.completed", 16, json!({"response":{"id":"resp_1","status":"completed","output":[
            {"type":"message","id":"msg_1","role":"assistant","content":[{"type":"output_text","text":"hello"}]},
            {"type":"function_call","id":"fc_1","call_id":"call_1","name":"shell","arguments":"{\"command\":[\"ls\"]}","status":"completed"},
            {"type":"message","id":"msg_2","role":"assistant","content":[{"type":"output_text","text":"bye"}]}
        ],"usage":{"input_tokens":1,"output_tokens":1}}})),
    ]
    .concat()
}

#[tokio::test]
async fn responses_text_is_rewritten_everywhere_the_full_text_repeats() {
    let input = responses_stream();
    let mut s = Stream::new(
        chain_of(Dialect::Responses, vec![upper()], &json!({})).await,
        Framing::Sse,
    );
    let (out, _) = run(&mut s, &input, 13).await;
    let f = frames(&out);
    let deltas: String = f
        .iter()
        .filter(|(_, v)| v["type"] == "response.output_text.delta")
        .map(|(_, v)| v["delta"].as_str().unwrap())
        .collect();
    assert_eq!(deltas, "HELLOBYE");
    let done = f
        .iter()
        .find(|(_, v)| v["type"] == "response.output_text.done")
        .unwrap();
    assert_eq!(done.1["text"], "HELLO");
    let part = f
        .iter()
        .find(|(_, v)| v["type"] == "response.content_part.done")
        .unwrap();
    assert_eq!(part.1["part"]["text"], "HELLO");
    let item = f
        .iter()
        .find(|(_, v)| v["type"] == "response.output_item.done")
        .unwrap();
    assert_eq!(item.1["item"]["content"][0]["text"], "HELLO");
    let completed = f
        .iter()
        .find(|(_, v)| v["type"] == "response.completed")
        .unwrap();
    assert_eq!(
        completed.1["response"]["output"][0]["content"][0]["text"],
        "HELLO"
    );
    assert_eq!(
        completed.1["response"]["output"][2]["content"][0]["text"],
        "BYE"
    );
    // 序号连续
    let seqs: Vec<u64> = f
        .iter()
        .filter_map(|(_, v)| v["sequence_number"].as_u64())
        .collect();
    assert_eq!(seqs, (0..seqs.len() as u64).collect::<Vec<_>>());
}

#[tokio::test]
async fn responses_tool_calls_are_replaced_with_full_events_and_indexes_follow() {
    let input = responses_stream();
    let two = tools(|_| {
        ToolCallOutcome::Replace(vec![
            json!({ "name": "shell", "input": { "command": ["pwd"] } }),
            json!({ "name": "shell", "input": { "command": ["ls", "-a"] } }),
        ])
    });
    let mut s = Stream::new(
        chain_of(Dialect::Responses, vec![two], &json!({})).await,
        Framing::Sse,
    );
    let (out, _) = run(&mut s, &input, 4096).await;
    let f = frames(&out);
    let dones: Vec<&Value> = f
        .iter()
        .filter(|(_, v)| v["type"] == "response.output_item.done")
        .map(|(_, v)| v)
        .collect();
    let oi: Vec<u64> = dones
        .iter()
        .map(|v| v["output_index"].as_u64().unwrap())
        .collect();
    assert_eq!(oi, [0, 1, 2, 3]);
    assert_eq!(dones[1]["item"]["arguments"], "{\"command\":[\"pwd\"]}");
    assert_eq!(
        dones[2]["item"]["arguments"],
        "{\"command\":[\"ls\",\"-a\"]}"
    );
    assert_ne!(dones[1]["item"]["call_id"], dones[2]["item"]["call_id"]);
    let completed = &f
        .iter()
        .find(|(_, v)| v["type"] == "response.completed")
        .unwrap()
        .1;
    let output = completed["response"]["output"].as_array().unwrap();
    assert_eq!(output.len(), 4);
    assert_eq!(output[1], dones[1]["item"]);
    let seqs: Vec<u64> = f
        .iter()
        .filter_map(|(_, v)| v["sequence_number"].as_u64())
        .collect();
    assert_eq!(seqs, (0..seqs.len() as u64).collect::<Vec<_>>());
    // 去掉：后面的消息项挪上来，completed 里也没有它
    let mut s = Stream::new(
        chain_of(
            Dialect::Responses,
            vec![tools(|_| ToolCallOutcome::Drop)],
            &json!({}),
        )
        .await,
        Framing::Sse,
    );
    let (out, _) = run(&mut s, &input, 4096).await;
    let f = frames(&out);
    assert!(!out.contains("function_call"), "{out}");
    let last = f
        .iter()
        .rfind(|(_, v)| v["type"] == "response.output_item.done")
        .unwrap();
    assert_eq!(last.1["output_index"], 1);
    let completed = &f
        .iter()
        .find(|(_, v)| v["type"] == "response.completed")
        .unwrap()
        .1;
    assert_eq!(completed["response"]["output"].as_array().unwrap().len(), 2);
}

// ───────────────────────────────────────────────────────── Gemini

fn gemini_chunks() -> Vec<Value> {
    vec![
        json!({"candidates":[{"content":{"role":"model","parts":[{"text":"thinking","thought":true}]}}],"modelVersion":"g","responseId":"r"}),
        json!({"candidates":[{"content":{"role":"model","parts":[{"text":"hel"}]}}],"modelVersion":"g","responseId":"r"}),
        json!({"candidates":[{"content":{"role":"model","parts":[{"text":"lo"},{"functionCall":{"name":"ls","args":{"dir":"."}},"thoughtSignature":"c2ln"}]}}],"modelVersion":"g","responseId":"r"}),
        json!({"candidates":[{"content":{"role":"model","parts":[{"text":"bye"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1},"modelVersion":"g","responseId":"r"}),
    ]
}

/// Gemini 的输出：(正文, 调用, 推理签名)
fn gemini_reading(chunks: &[Value]) -> (String, Vec<Value>, Vec<String>) {
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut sigs = Vec::new();
    for c in chunks {
        for p in c["candidates"][0]["content"]["parts"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if p["thought"] == true {
                continue;
            }
            if let Some(t) = p["text"].as_str() {
                text.push_str(t);
            }
            if p.get("functionCall").is_some() {
                calls.push(p["functionCall"].clone());
            }
            if let Some(s) = p["thoughtSignature"].as_str() {
                sigs.push(s.to_string());
            }
        }
    }
    (text, calls, sigs)
}

#[tokio::test]
async fn gemini_sse_and_json_arrays_get_the_same_rewrite() {
    let chunks = gemini_chunks();
    let sse: String = chunks.iter().map(|c| data(c.clone())).collect();
    let array = format!(
        "[{}]",
        chunks
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(",\r\n")
    );
    let two = || {
        tools(|_| {
            ToolCallOutcome::Replace(vec![
                json!({ "name": "ls", "input": { "dir": "/" } }),
                json!({ "name": "cat", "input": { "file": "a" } }),
            ])
        })
    };
    let mut s = Stream::new(
        chain_of(Dialect::Gemini, vec![upper(), two()], &json!({})).await,
        Framing::Sse,
    );
    let (out, _) = run(&mut s, &sse, 11).await;
    let got: Vec<Value> = frames(&out).into_iter().map(|(_, v)| v).collect();
    let (text, calls, sigs) = gemini_reading(&got);
    assert_eq!(text, "HELLOBYE");
    assert_eq!(
        calls,
        [
            json!({"name":"ls","args":{"dir":"/"}}),
            json!({"name":"cat","args":{"file":"a"}})
        ]
    );
    assert_eq!(sigs, ["c2ln"]);

    let mut s = Stream::new(
        chain_of(Dialect::Gemini, vec![upper(), two()], &json!({})).await,
        Framing::JsonArray,
    );
    let (out, _) = run(&mut s, &array, 7).await;
    let got: Vec<Value> = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    let (text, calls, sigs) = gemini_reading(&got);
    assert_eq!(text, "HELLOBYE");
    assert_eq!(calls.len(), 2);
    assert_eq!(sigs, ["c2ln"]);
}

#[tokio::test]
async fn a_cut_json_array_is_closed_into_a_valid_array() {
    let chunks = gemini_chunks();
    let array = format!(
        "[{}",
        chunks[..2]
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(",\r\n")
    );
    let mut s = Stream::new(
        chain_of(Dialect::Gemini, vec![upper()], &json!({})).await,
        Framing::JsonArray,
    );
    let (mut out, _) = s.feed(array.as_bytes()).await;
    // 中途被切断：补的错误收尾按这一层发过的接上
    out.extend(s.tail(b",\r\n{\"error\":{\"code\":403,\"message\":\"cut\"}}]"));
    let got: Vec<Value> = serde_json::from_slice(&out)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out)));
    assert_eq!(got.last().unwrap()["error"]["message"], "cut");
}

// ───────────────────────────────────────────────────────── 整包

#[tokio::test]
async fn whole_bodies_get_block_semantics_in_every_format() {
    let cases = [
        (
            Dialect::Anthropic,
            json!({"id":"m","type":"message","role":"assistant","content":[
                {"type":"thinking","thinking":"t","signature":"s"},
                {"type":"text","text":"hello"},
                {"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}
            ],"stop_reason":"tool_use"}),
        ),
        (
            Dialect::Chat,
            json!({"id":"c","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"hello",
                "tool_calls":[{"id":"call_1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"ls\"}"}}]},
                "finish_reason":"tool_calls"}]}),
        ),
        (
            Dialect::Responses,
            json!({"id":"r","object":"response","status":"completed","output":[
                {"type":"message","id":"msg","role":"assistant","content":[{"type":"output_text","text":"hello"}]},
                {"type":"function_call","id":"fc","call_id":"call_1","name":"Bash","arguments":"{\"command\":\"ls\"}"}
            ]}),
        ),
        (
            Dialect::Gemini,
            json!({"candidates":[{"content":{"role":"model","parts":[
                {"text":"hello"},
                {"functionCall":{"name":"Bash","args":{"command":"ls"}},"thoughtSignature":"c2ln"}
            ]},"finishReason":"STOP"}]}),
        ),
    ];
    for (d, body) in cases {
        let drop_bash = tools(|c| {
            assert_eq!(c["input"], json!({ "command": "ls" }));
            ToolCallOutcome::Drop
        });
        let mut chain = chain_of(d, vec![hold_until_end(), drop_bash], &json!({})).await;
        let out = whole(&mut chain, body.to_string().as_bytes())
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let text = v.to_string();
        assert!(text.contains("HELLO"), "{d:?}: {text}");
        assert!(!text.contains("Bash"), "{d:?}: {text}");
        match d {
            Dialect::Anthropic => {
                assert_eq!(v["stop_reason"], "end_turn");
                assert_eq!(v["content"][0]["signature"], "s");
            }
            Dialect::Chat => assert_eq!(v["choices"][0]["finish_reason"], "stop"),
            _ => {}
        }
        // 什么都没改的整包原样返回
        let mut chain = chain_of(d, vec![tools(|_| ToolCallOutcome::Unchanged)], &json!({})).await;
        let raw = body.to_string();
        assert_eq!(
            whole(&mut chain, raw.as_bytes()).await.unwrap(),
            raw.as_bytes()
        );
    }
}

#[tokio::test]
async fn reply_runs_are_counted_on_the_plugins() {
    let state = state();
    let set = set_of(vec![upper(), tools(|_| ToolCallOutcome::Drop)]);
    let chain = chain_with(&state, &set, Dialect::Anthropic, &json!({})).await;
    let mut s = Stream::new(chain, Framing::Sse);
    let _ = run(&mut s, &anthropic_stream(), 4096).await;
    drop(s);
    // 一个回答一次：两个插件各记一次，都改了东西
    for a in set.all() {
        let v = a.stats.view();
        assert_eq!((v.calls, v.changed, v.errors), (1, 1, 0), "{}", a.id);
    }
}
