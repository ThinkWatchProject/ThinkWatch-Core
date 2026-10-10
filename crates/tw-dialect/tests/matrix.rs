//! 四种客户端 × 四种上游，每一组都走一遍完整的转换。
//!
//! 同一件事在 16 种组合里说一遍：用户问天气，上游回一段文字和一个工具调用，用了
//! 100 个输入 token（其中 40 个命中缓存）、20 个输出 token。检查客户端最终拿到的
//! 东西里这几样一样不少：
//!
//! - 发给上游的请求能被那种格式读回来，工具和用户的话都在
//! - 整包响应、流式响应、「上游给流、客户端要整包」三条路，文字、工具调用、结束原因、
//!   用量都对得上
//!
//! 客户端那一侧用各格式自己的解码器读 —— 它们按官方 SDK 的类型写成，读得回来就说明
//! 形状对。上游的流按 7 字节切开喂进去，帧被切在任何位置都不能丢内容。

use serde_json::{Value, json};
use tw_dialect::convert::{Session, prepare};
use tw_dialect::frame::Decoder;
use tw_dialect::ir::*;
use tw_dialect::{anthropic, bedrock, chat, gemini, responses};

const ALL: [Dialect; 4] = [
    Dialect::Anthropic,
    Dialect::Chat,
    Dialect::Responses,
    Dialect::Gemini,
];

fn schema() -> Value {
    json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]})
}

/// 客户端请求：(body, path, query)
fn client_request(d: Dialect, stream: bool) -> (Vec<u8>, String, Option<String>) {
    let body = match d {
        Dialect::Anthropic => json!({
            "model": "test-model", "max_tokens": 1024, "stream": stream,
            "messages": [{"role": "user", "content": "北京天气？"}],
            "tools": [{"name": "get_weather", "description": "查天气", "input_schema": schema()}],
        }),
        Dialect::Chat => json!({
            "model": "test-model", "stream": stream, "stream_options": {"include_usage": true},
            "messages": [{"role": "user", "content": "北京天气？"}],
            "tools": [{"type": "function", "function": {"name": "get_weather", "description": "查天气", "parameters": schema()}}],
        }),
        Dialect::Responses => json!({
            "model": "test-model", "stream": stream, "input": "北京天气？",
            "tools": [{"type": "function", "name": "get_weather", "description": "查天气", "parameters": schema(), "strict": false}],
        }),
        Dialect::Gemini => json!({
            "contents": [{"role": "user", "parts": [{"text": "北京天气？"}]}],
            "tools": [{"functionDeclarations": [{"name": "get_weather", "description": "查天气", "parametersJsonSchema": schema()}]}],
        }),
        Dialect::Bedrock => json!({
            "messages": [{"role": "user", "content": [{"text": "北京天气？"}]}],
            "inferenceConfig": {"maxTokens": 1024},
            "toolConfig": {"tools": [{"toolSpec": {"name": "get_weather", "description": "查天气", "inputSchema": {"json": schema()}}}]},
        }),
    };
    let (path, query) = match d {
        Dialect::Anthropic => ("/v1/messages".to_string(), None),
        Dialect::Chat => ("/v1/chat/completions".to_string(), None),
        Dialect::Responses => ("/v1/responses".to_string(), None),
        Dialect::Gemini => {
            let action = if stream {
                "streamGenerateContent"
            } else {
                "generateContent"
            };
            (
                format!("/v1beta/models/test-model:{action}"),
                stream.then(|| "alt=sse".to_string()),
            )
        }
        Dialect::Bedrock => {
            let action = if stream {
                "converse-stream"
            } else {
                "converse"
            };
            (format!("/model/test-model/{action}"), None)
        }
    };
    (body.to_string().into_bytes(), path, query)
}

fn upstream_response(d: Dialect) -> Value {
    match d {
        Dialect::Anthropic => json!({
            "id": "msg_up", "type": "message", "role": "assistant", "model": "up-model",
            "content": [
                {"type": "text", "text": "天气晴"},
                {"type": "tool_use", "id": "call_up_1", "name": "get_weather", "input": {"city": "北京"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 60, "cache_read_input_tokens": 40, "output_tokens": 20}
        }),
        Dialect::Chat => json!({
            "id": "chatcmpl-up", "object": "chat.completion", "model": "up-model",
            "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": "天气晴",
                "tool_calls": [{"id": "call_up_1", "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"北京\"}"}}]
            }}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20, "prompt_tokens_details": {"cached_tokens": 40}}
        }),
        Dialect::Responses => json!({
            "id": "resp_up", "object": "response", "status": "completed", "model": "up-model",
            "output": [
                {"type": "message", "id": "msg_1", "role": "assistant", "content": [{"type": "output_text", "text": "天气晴"}]},
                {"type": "function_call", "id": "fc_1", "call_id": "call_up_1", "name": "get_weather", "arguments": "{\"city\":\"北京\"}"}
            ],
            "usage": {"input_tokens": 100, "input_tokens_details": {"cached_tokens": 40}, "output_tokens": 20}
        }),
        Dialect::Gemini => json!({
            "candidates": [{"content": {"role": "model", "parts": [
                {"text": "天气晴"},
                {"functionCall": {"name": "get_weather", "args": {"city": "北京"}}}
            ]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 100, "cachedContentTokenCount": 40, "candidatesTokenCount": 20},
            "modelVersion": "up-model", "responseId": "r_up"
        }),
        Dialect::Bedrock => json!({
            "output": {"message": {"role": "assistant", "content": [
                {"text": "天气晴"},
                {"toolUse": {"toolUseId": "call_up_1", "name": "get_weather", "input": {"city": "北京"}}}
            ]}},
            "stopReason": "tool_use",
            "usage": {"inputTokens": 60, "cacheReadInputTokens": 40, "outputTokens": 20, "totalTokens": 120}
        }),
    }
}

fn named(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn data(v: Value) -> String {
    format!("data: {v}\n\n")
}

fn upstream_stream(d: Dialect) -> String {
    match d {
        Dialect::Anthropic => [
            named("message_start", json!({"type": "message_start", "message": {"id": "msg_up", "model": "up-model",
                "usage": {"input_tokens": 60, "cache_read_input_tokens": 40, "output_tokens": 1}}})),
            named("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})),
            named("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "天气"}})),
            named("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "晴"}})),
            named("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
            named("content_block_start", json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "call_up_1", "name": "get_weather", "input": {}}})),
            named("content_block_delta", json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"city\":"}})),
            named("content_block_delta", json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "\"北京\"}"}})),
            named("content_block_stop", json!({"type": "content_block_stop", "index": 1})),
            named("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 20}})),
            named("message_stop", json!({"type": "message_stop"})),
        ]
        .concat(),
        Dialect::Chat => {
            let chunk = |delta: Value, finish: Value| {
                data(json!({"id": "chatcmpl-up", "object": "chat.completion.chunk", "model": "up-model",
                    "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}))
            };
            [
                chunk(json!({"role": "assistant", "content": ""}), Value::Null),
                chunk(json!({"content": "天气"}), Value::Null),
                chunk(json!({"content": "晴"}), Value::Null),
                chunk(json!({"tool_calls": [{"index": 0, "id": "call_up_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":"}}]}), Value::Null),
                chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"北京\"}"}}]}), Value::Null),
                chunk(json!({}), json!("tool_calls")),
                data(json!({"id": "chatcmpl-up", "object": "chat.completion.chunk", "model": "up-model", "choices": [],
                    "usage": {"prompt_tokens": 100, "completion_tokens": 20, "prompt_tokens_details": {"cached_tokens": 40}}})),
                "data: [DONE]\n\n".to_string(),
            ]
            .concat()
        }
        Dialect::Responses => [
            named("response.created", json!({"type": "response.created", "response": {"id": "resp_up", "model": "up-model", "status": "in_progress"}})),
            named("response.output_item.added", json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}})),
            named("response.content_part.added", json!({"type": "response.content_part.added", "output_index": 0, "content_index": 0, "item_id": "msg_1", "part": {"type": "output_text", "text": ""}})),
            named("response.output_text.delta", json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "item_id": "msg_1", "delta": "天气"})),
            named("response.output_text.delta", json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "item_id": "msg_1", "delta": "晴"})),
            named("response.output_text.done", json!({"type": "response.output_text.done", "output_index": 0, "content_index": 0, "item_id": "msg_1", "text": "天气晴"})),
            named("response.output_item.done", json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": [{"type": "output_text", "text": "天气晴"}]}})),
            named("response.output_item.added", json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_up_1", "name": "get_weather", "arguments": ""}})),
            named("response.function_call_arguments.delta", json!({"type": "response.function_call_arguments.delta", "output_index": 1, "item_id": "fc_1", "delta": "{\"city\":"})),
            named("response.function_call_arguments.delta", json!({"type": "response.function_call_arguments.delta", "output_index": 1, "item_id": "fc_1", "delta": "\"北京\"}"})),
            named("response.output_item.done", json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_up_1", "name": "get_weather", "arguments": "{\"city\":\"北京\"}"}})),
            named("response.completed", json!({"type": "response.completed", "response": {"id": "resp_up", "status": "completed",
                "usage": {"input_tokens": 100, "input_tokens_details": {"cached_tokens": 40}, "output_tokens": 20}}})),
        ]
        .concat(),
        Dialect::Gemini => {
            let chunk = |parts: Value| {
                data(json!({"candidates": [{"content": {"role": "model", "parts": parts}}], "modelVersion": "up-model", "responseId": "r_up"}))
            };
            [
                chunk(json!([{"text": "天气"}])),
                chunk(json!([{"text": "晴"}])),
                data(json!({"candidates": [{"content": {"role": "model", "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "北京"}}}]}, "finishReason": "STOP"}],
                    "usageMetadata": {"promptTokenCount": 100, "cachedContentTokenCount": 40, "candidatesTokenCount": 20}})),
            ]
            .concat()
        }
        // 传输层已经把 eventstream 的二进制帧拆成了这个形状：
        // `:event-type` 头进 event，载荷进 data
        Dialect::Bedrock => [
            named("messageStart", json!({"role": "assistant"})),
            named("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"text": "天气"}})),
            named("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"text": "晴"}})),
            named("contentBlockStop", json!({"contentBlockIndex": 0})),
            named("contentBlockStart", json!({"contentBlockIndex": 1, "start": {"toolUse": {"toolUseId": "call_up_1", "name": "get_weather"}}})),
            named("contentBlockDelta", json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": "{\"city\":"}}})),
            named("contentBlockDelta", json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": "\"北京\"}"}}})),
            named("contentBlockStop", json!({"contentBlockIndex": 1})),
            named("messageStop", json!({"stopReason": "tool_use"})),
            named("metadata", json!({"usage": {"inputTokens": 60, "cacheReadInputTokens": 40, "outputTokens": 20, "totalTokens": 120}})),
        ]
        .concat(),
    }
}

fn target(d: Dialect) -> Target {
    Target {
        dialect: d,
        official: false,
        default_max_tokens: 8192,
    }
}

/// 读回：(文字, 工具调用, 结束原因, 用量)
type Seen = (
    String,
    Vec<(String, Value)>,
    Option<StopReason>,
    Option<Usage>,
);

fn seen_response(r: &Response) -> Seen {
    let mut text = String::new();
    let mut calls = Vec::new();
    for b in &r.blocks {
        match b {
            Block::Text(t) => text.push_str(t),
            Block::ToolCall(c) => calls.push((
                c.name.clone(),
                match &c.input {
                    ToolInput::Json(v) => v.clone(),
                    ToolInput::Text(t) => serde_json::from_str(t).unwrap_or(Value::Null),
                },
            )),
            Block::Thinking(_) => {}
        }
    }
    (text, calls, r.stop.clone(), r.usage)
}

fn decode_client_response(d: Dialect, body: &[u8]) -> Response {
    let v: Value =
        serde_json::from_slice(body).unwrap_or_else(|e| panic!("{d:?} 的响应不是 JSON：{e}"));
    match d {
        Dialect::Anthropic => anthropic::decode_response(&v),
        Dialect::Chat => chat::decode_response(&v),
        Dialect::Responses => responses::decode_response(&v),
        Dialect::Gemini => gemini::decode_response(&v),
        Dialect::Bedrock => bedrock::decode_response(&v),
    }
}

/// 用客户端格式的解析器读客户端收到的流，聚合成一个响应
fn decode_client_stream(d: Dialect, bytes: &[u8]) -> Response {
    let mut dec = Decoder::default();
    let mut frames = dec.feed(bytes);
    frames.extend(dec.flush());
    let mut events = Vec::new();
    match d {
        Dialect::Anthropic => {
            let mut p = anthropic::stream::Parser::default();
            frames.iter().for_each(|f| p.frame(f, &mut events));
        }
        Dialect::Chat => {
            let mut p = chat::stream::Parser::default();
            frames.iter().for_each(|f| p.frame(f, &mut events));
            p.finish(&mut events);
        }
        Dialect::Responses => {
            let mut p = responses::stream::Parser::default();
            frames.iter().for_each(|f| p.frame(f, &mut events));
            p.finish(&mut events);
        }
        Dialect::Gemini => {
            let mut p = gemini::stream::Parser::default();
            frames.iter().for_each(|f| p.frame(f, &mut events));
            p.finish(&mut events);
        }
        Dialect::Bedrock => {
            let mut p = bedrock::stream::Parser::default();
            frames.iter().for_each(|f| p.frame(f, &mut events));
            p.finish(&mut events);
        }
    }
    let mut r = Response::default();
    let mut open = std::collections::HashMap::new();
    for e in events {
        match e {
            Event::BlockStart { index, kind } => {
                open.insert(index, r.blocks.len());
                r.blocks.push(match kind {
                    BlockKind::Text => Block::Text(String::new()),
                    BlockKind::Thinking => Block::Thinking(Thinking {
                        text: String::new(),
                        signature: None,
                    }),
                    BlockKind::ToolCall { id, name } => Block::ToolCall(ToolCall {
                        id,
                        name,
                        input: ToolInput::Text(String::new()),
                    }),
                });
            }
            Event::Delta { index, delta } => match (&mut r.blocks[open[&index]], delta) {
                (Block::Text(t), Delta::Text(x)) => t.push_str(&x),
                (Block::Thinking(th), Delta::Thinking(x)) => th.text.push_str(&x),
                (Block::Thinking(th), Delta::Signature(sig)) => th.signature = Some(sig),
                (Block::ToolCall(c), Delta::ToolInput(x)) => {
                    if let ToolInput::Text(t) = &mut c.input {
                        t.push_str(&x);
                    }
                }
                _ => {}
            },
            Event::Usage(u) => r.usage.get_or_insert_default().merge(&u),
            Event::Stop(s) => r.stop = Some(s),
            Event::Error { message } => panic!("{d:?} 的流里出现错误：{message}"),
            _ => {}
        }
    }
    r
}

fn check(client: Dialect, upstream: Dialect, path: &str, seen: Seen) {
    let (text, calls, stop, usage) = seen;
    let at = format!("{client:?} ← {upstream:?}（{path}）");
    assert_eq!(text, "天气晴", "{at}");
    assert_eq!(
        calls,
        [("get_weather".to_string(), json!({"city": "北京"}))],
        "{at}"
    );
    assert_eq!(stop, Some(StopReason::ToolUse), "{at}");
    let u = usage.unwrap_or_else(|| panic!("{at}：没有用量"));
    assert_eq!((u.input, u.cache_read, u.output), (60, 40, 20), "{at}");
}

#[test]
fn the_request_reaches_every_upstream_in_its_own_shape() {
    for client in ALL {
        for upstream in ALL {
            let (body, path, query) = client_request(client, false);
            let p = prepare(client, &body, &path, query.as_deref(), &target(upstream))
                .unwrap_or_else(|e| panic!("{client:?} → {upstream:?}：{e}"));
            assert!(
                p.dropped.is_empty(),
                "{client:?} → {upstream:?}：{:?}",
                p.dropped
            );
            let v: Value = serde_json::from_slice(&p.body).unwrap();
            let mut d = Dropped::new(upstream);
            let mut shape = ClientShape::default();
            let r = match upstream {
                Dialect::Anthropic => anthropic::decode_request(&v, &mut d),
                Dialect::Chat => chat::decode_request(&v, &mut d, &mut shape),
                Dialect::Responses => responses::decode_request(&v, &mut d, &mut shape),
                Dialect::Gemini => gemini::decode_request(&v, "test-model", false, &mut d),
                Dialect::Bedrock => bedrock::decode_request(&v, "test-model", false, &mut d),
            }
            .unwrap();
            let at = format!("{client:?} → {upstream:?}");
            assert_eq!(r.tools.len(), 1, "{at}: {v}");
            assert_eq!(r.tools[0].name, "get_weather", "{at}");
            assert_eq!(
                r.tools[0].kind,
                ToolKind::Function {
                    schema: schema(),
                    strict: r.tools[0].kind.clone().strict()
                },
                "{at}"
            );
            assert_eq!(
                r.messages[0].parts,
                [Part::Text("北京天气？".into())],
                "{at}: {v}"
            );
            if upstream != Dialect::Gemini {
                assert_eq!(r.model, "test-model", "{at}");
            }
        }
    }
}

trait Strict {
    fn strict(self) -> Option<bool>;
}

impl Strict for ToolKind {
    fn strict(self) -> Option<bool> {
        match self {
            ToolKind::Function { strict, .. } => strict,
            ToolKind::Freeform { .. } => None,
        }
    }
}

fn session(client: Dialect, upstream: Dialect, stream: bool) -> Session {
    let (body, path, query) = client_request(client, stream);
    prepare(client, &body, &path, query.as_deref(), &target(upstream))
        .unwrap()
        .session
}

#[test]
fn a_whole_response_comes_back_in_every_client_shape() {
    for client in ALL {
        for upstream in ALL {
            let s = session(client, upstream, false);
            let body = upstream_response(upstream).to_string();
            let out = s.response(body.as_bytes()).unwrap();
            check(
                client,
                upstream,
                "整包",
                seen_response(&decode_client_response(client, &out)),
            );
        }
    }
}

#[test]
fn a_stream_comes_back_in_every_client_shape_even_cut_into_small_pieces() {
    for client in ALL {
        for upstream in ALL {
            let s = session(client, upstream, true);
            let mut c = s.stream();
            let mut out = Vec::new();
            for piece in upstream_stream(upstream).as_bytes().chunks(7) {
                out.extend(c.process(piece));
            }
            out.extend(c.finish());
            assert!(
                c.finish().is_empty(),
                "{client:?} ← {upstream:?}：finish 不幂等"
            );
            check(
                client,
                upstream,
                "流式",
                seen_response(&decode_client_stream(client, &out)),
            );
        }
    }
}

#[test]
fn a_stream_collected_for_a_client_that_wanted_a_whole_response() {
    for client in ALL {
        for upstream in ALL {
            let s = session(client, upstream, false);
            let mut c = s.collector();
            for piece in upstream_stream(upstream).as_bytes().chunks(7) {
                c.process(piece);
            }
            let out = c.finish().unwrap();
            check(
                client,
                upstream,
                "收集",
                seen_response(&decode_client_response(client, &out)),
            );
        }
    }
}

// ───────────────────────────────────────────────────────── 推理内容

/// 这家上游签发的推理签名长什么样。Chat 上游（DeepSeek 等）不签名，只打厂商
fn signed_by(upstream: Dialect) -> Signature {
    match upstream {
        Dialect::Anthropic | Dialect::Bedrock => Signature::new(Vendor::Anthropic, "sigA"),
        Dialect::Chat => Signature::new(Vendor::Chat, ""),
        Dialect::Responses => Signature::new(Vendor::OpenAi, "rs_1:encO"),
        Dialect::Gemini => Signature::new(Vendor::Google, "sigG"),
    }
}

/// 客户端请求：上一轮上游想了想、调了工具，工具结果回来了。推理带着 `sig`，
/// 写在客户端格式的签名字段里（别家签发的加 `tw1.` 前缀，和写给客户端时一样）。
/// Chat 客户端没有签名字段，只能带 Chat 上游自己的推理：别的签名是 `None`
fn client_request_with_thinking(d: Dialect, sig: &Signature) -> Option<Vec<u8>> {
    let call = json!({"city": "北京"});
    let body = match d {
        Dialect::Anthropic => json!({
            "model": "test-model", "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "北京天气？"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "想想", "signature": sig.carried_in(Vendor::Anthropic)},
                    {"type": "tool_use", "id": "call_1", "name": "get_weather", "input": call}
                ]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "晴"}]}
            ],
            "tools": [{"name": "get_weather", "description": "查天气", "input_schema": schema()}],
        }),
        Dialect::Chat => {
            if sig.vendor != Vendor::Chat {
                return None;
            }
            json!({
                "model": "test-model",
                "messages": [
                    {"role": "user", "content": "北京天气？"},
                    {"role": "assistant", "content": null, "reasoning_content": "想想",
                     "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": call.to_string()}}]},
                    {"role": "tool", "tool_call_id": "call_1", "content": "晴"}
                ],
                "tools": [{"type": "function", "function": {"name": "get_weather", "description": "查天气", "parameters": schema()}}],
            })
        }
        Dialect::Responses => {
            // OpenAI 自己的签名是「项 id:密文」，拆开写；别家的整个放进 encrypted_content
            let (id, enc) = match sig.vendor {
                Vendor::OpenAi => sig.value.split_once(':').unwrap(),
                _ => ("rs_tw", ""),
            };
            let enc = if enc.is_empty() {
                sig.carried_in(Vendor::OpenAi)
            } else {
                enc.to_string()
            };
            json!({
                "model": "test-model",
                "input": [
                    {"type": "message", "role": "user", "content": "北京天气？"},
                    {"type": "reasoning", "id": id, "summary": [{"type": "summary_text", "text": "想想"}], "encrypted_content": enc},
                    {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "get_weather", "arguments": call.to_string()},
                    {"type": "function_call_output", "call_id": "call_1", "output": "晴"}
                ],
                "tools": [{"type": "function", "name": "get_weather", "description": "查天气", "parameters": schema(), "strict": false}],
            })
        }
        Dialect::Gemini => json!({
            "contents": [
                {"role": "user", "parts": [{"text": "北京天气？"}]},
                {"role": "model", "parts": [
                    {"text": "想想", "thought": true, "thoughtSignature": sig.carried_in(Vendor::Google)},
                    {"functionCall": {"id": "call_1", "name": "get_weather", "args": call}}
                ]},
                {"role": "user", "parts": [{"functionResponse": {"id": "call_1", "name": "get_weather", "response": {"output": "晴"}}}]}
            ],
            "tools": [{"functionDeclarations": [{"name": "get_weather", "description": "查天气", "parametersJsonSchema": schema()}]}],
        }),
        Dialect::Bedrock => unreachable!("Bedrock is not among the client dialects here"),
    };
    Some(body.to_string().into_bytes())
}

/// 上游的整包响应：想了想，答了一句
fn upstream_response_with_thinking(d: Dialect) -> Value {
    match d {
        Dialect::Anthropic => json!({
            "id": "msg_up", "type": "message", "role": "assistant", "model": "up-model",
            "content": [
                {"type": "thinking", "thinking": "想想", "signature": "sigA"},
                {"type": "text", "text": "天气晴"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 100, "output_tokens": 20}
        }),
        Dialect::Chat => json!({
            "id": "chatcmpl-up", "object": "chat.completion", "model": "up-model",
            "choices": [{"index": 0, "finish_reason": "stop", "message": {
                "role": "assistant", "reasoning_content": "想想", "content": "天气晴"
            }}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20}
        }),
        Dialect::Responses => json!({
            "id": "resp_up", "object": "response", "status": "completed", "model": "up-model",
            "output": [
                {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "想想"}], "encrypted_content": "encO"},
                {"type": "message", "id": "msg_1", "role": "assistant", "content": [{"type": "output_text", "text": "天气晴"}]}
            ],
            "usage": {"input_tokens": 100, "output_tokens": 20}
        }),
        Dialect::Gemini => json!({
            "candidates": [{"content": {"role": "model", "parts": [
                {"text": "想想", "thought": true, "thoughtSignature": "sigG"},
                {"text": "天气晴"}
            ]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 20},
            "modelVersion": "up-model", "responseId": "r_up"
        }),
        Dialect::Bedrock => unreachable!("Bedrock is not among the upstream dialects here"),
    }
}

/// 上游的流：推理分两片，签名在推理块结束前到
fn upstream_stream_with_thinking(d: Dialect) -> String {
    match d {
        Dialect::Anthropic => [
            named("message_start", json!({"type": "message_start", "message": {"id": "msg_up", "model": "up-model",
                "usage": {"input_tokens": 100, "output_tokens": 1}}})),
            named("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}})),
            named("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "想"}})),
            named("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "想"}})),
            named("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sigA"}})),
            named("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
            named("content_block_start", json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}})),
            named("content_block_delta", json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "天气晴"}})),
            named("content_block_stop", json!({"type": "content_block_stop", "index": 1})),
            named("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 20}})),
            named("message_stop", json!({"type": "message_stop"})),
        ]
        .concat(),
        Dialect::Chat => {
            let chunk = |delta: Value, finish: Value| {
                data(json!({"id": "chatcmpl-up", "object": "chat.completion.chunk", "model": "up-model",
                    "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}))
            };
            [
                chunk(json!({"role": "assistant", "content": ""}), Value::Null),
                chunk(json!({"reasoning_content": "想"}), Value::Null),
                chunk(json!({"reasoning_content": "想"}), Value::Null),
                chunk(json!({"content": "天气晴"}), Value::Null),
                chunk(json!({}), json!("stop")),
                data(json!({"id": "chatcmpl-up", "object": "chat.completion.chunk", "model": "up-model", "choices": [],
                    "usage": {"prompt_tokens": 100, "completion_tokens": 20}})),
                "data: [DONE]\n\n".to_string(),
            ]
            .concat()
        }
        Dialect::Responses => [
            named("response.created", json!({"type": "response.created", "response": {"id": "resp_up", "model": "up-model", "status": "in_progress"}})),
            named("response.output_item.added", json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": []}})),
            named("response.reasoning_summary_text.delta", json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "summary_index": 0, "item_id": "rs_1", "delta": "想"})),
            named("response.reasoning_summary_text.delta", json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "summary_index": 0, "item_id": "rs_1", "delta": "想"})),
            named("response.output_item.done", json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "想想"}], "encrypted_content": "encO"}})),
            named("response.output_item.added", json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}})),
            named("response.content_part.added", json!({"type": "response.content_part.added", "output_index": 1, "content_index": 0, "item_id": "msg_1", "part": {"type": "output_text", "text": ""}})),
            named("response.output_text.delta", json!({"type": "response.output_text.delta", "output_index": 1, "content_index": 0, "item_id": "msg_1", "delta": "天气晴"})),
            named("response.output_text.done", json!({"type": "response.output_text.done", "output_index": 1, "content_index": 0, "item_id": "msg_1", "text": "天气晴"})),
            named("response.output_item.done", json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": [{"type": "output_text", "text": "天气晴"}]}})),
            named("response.completed", json!({"type": "response.completed", "response": {"id": "resp_up", "status": "completed",
                "usage": {"input_tokens": 100, "output_tokens": 20}}})),
        ]
        .concat(),
        Dialect::Gemini => {
            let chunk = |parts: Value| {
                data(json!({"candidates": [{"content": {"role": "model", "parts": parts}}], "modelVersion": "up-model", "responseId": "r_up"}))
            };
            [
                chunk(json!([{"text": "想", "thought": true}])),
                chunk(json!([{"text": "想", "thought": true, "thoughtSignature": "sigG"}])),
                data(json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "天气晴"}]}, "finishReason": "STOP"}],
                    "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 20}})),
            ]
            .concat()
        }
        Dialect::Bedrock => unreachable!("Bedrock is not among the upstream dialects here"),
    }
}

/// 读回的推理块：(文字, 签名)。多块拼成一块
fn seen_thinking(r: &Response) -> Option<Thinking> {
    let mut seen: Option<Thinking> = None;
    for b in &r.blocks {
        if let Block::Thinking(th) = b {
            match &mut seen {
                Some(t) => {
                    t.text.push_str(&th.text);
                    t.signature = th.signature.clone().or(t.signature.take());
                }
                None => seen = Some(th.clone()),
            }
        }
    }
    seen
}

/// 客户端读回的签名和上游签发的是同一个。Responses 给自己的推理项起新的 id，
/// 比 OpenAI 的签名时只看密文那一半
fn same_signature(seen: Option<&Signature>, want: &Signature) -> bool {
    let Some(seen) = seen else {
        return false;
    };
    if seen.vendor != want.vendor || seen.redacted != want.redacted {
        return false;
    }
    match want.vendor {
        Vendor::OpenAi => {
            seen.value.rsplit_once(':').map(|x| x.1) == want.value.rsplit_once(':').map(|x| x.1)
        }
        _ => seen.value == want.value,
    }
}

fn check_thinking(client: Dialect, upstream: Dialect, path: &str, r: &Response) {
    let at = format!("{client:?} ← {upstream:?}（{path}）");
    let th = seen_thinking(r).unwrap_or_else(|| panic!("{at}：没有推理块\n{r:#?}"));
    assert_eq!(th.text, "想想", "{at}");
    let (text, _, stop, _) = seen_response(r);
    assert_eq!(text, "天气晴", "{at}");
    assert_eq!(stop, Some(StopReason::EndTurn), "{at}");
    // Chat 客户端没有签名字段：只拿得到文字
    if client != Dialect::Chat {
        let want = signed_by(upstream);
        assert!(
            same_signature(th.signature.as_ref(), &want),
            "{at}：签名 {:?}，上游签的是 {want:?}",
            th.signature
        );
    }
}

/// 历史里的推理内容：签发它的那家上游能原样收到，别家收不到（记在丢掉的字段里）。
/// 四种客户端各带四家的签名都试一遍
#[test]
fn thinking_in_the_history_reaches_the_upstream_that_signed_it() {
    for client in ALL {
        for upstream in ALL {
            for signer in ALL {
                let sig = signed_by(signer);
                let Some(body) = client_request_with_thinking(client, &sig) else {
                    continue;
                };
                let (_, path, query) = client_request(client, false);
                let at = format!("{client:?} → {upstream:?}，{signer:?} 签的");
                let p = prepare(client, &body, &path, query.as_deref(), &target(upstream))
                    .unwrap_or_else(|e| panic!("{at}：{e}"));
                let v: Value = serde_json::from_slice(&p.body).unwrap();
                let mut d = Dropped::new(upstream);
                let mut shape = ClientShape::default();
                let r = match upstream {
                    Dialect::Anthropic => anthropic::decode_request(&v, &mut d),
                    Dialect::Chat => chat::decode_request(&v, &mut d, &mut shape),
                    Dialect::Responses => responses::decode_request(&v, &mut d, &mut shape),
                    Dialect::Gemini => gemini::decode_request(&v, "test-model", false, &mut d),
                    Dialect::Bedrock => bedrock::decode_request(&v, "test-model", false, &mut d),
                }
                .unwrap();
                // 有的格式一项一条消息：看所有助手消息里的块
                let assistant: Vec<&Part> = r
                    .messages
                    .iter()
                    .filter(|m| m.role == Role::Assistant)
                    .flat_map(|m| m.parts.iter())
                    .collect();
                let thinking: Vec<&Thinking> = assistant
                    .iter()
                    .filter_map(|p| match p {
                        Part::Thinking(t) => Some(t),
                        _ => None,
                    })
                    .collect();
                assert!(
                    assistant
                        .iter()
                        .any(|p| matches!(p, Part::ToolCall(c) if c.name == "get_weather")),
                    "{at}：工具调用丢了\n{v:#}"
                );
                // 丢掉的字段按客户端的叫法记
                let path = Feature::ReasoningHistory.path(client);
                if signer == upstream {
                    assert_eq!(thinking.len(), 1, "{at}\n{v:#}");
                    assert_eq!(thinking[0].text, "想想", "{at}");
                    assert_eq!(thinking[0].signature.as_ref(), Some(&sig), "{at}\n{v:#}");
                    assert!(
                        !p.dropped.iter().any(|d| d == path),
                        "{at}：说丢了其实没丢 {:?}",
                        p.dropped
                    );
                } else {
                    assert!(thinking.is_empty(), "{at}：别家的推理发过去了\n{v:#}");
                    assert!(
                        p.dropped.iter().any(|d| d == path),
                        "{at}：丢了没说 {:?}",
                        p.dropped
                    );
                }
            }
        }
    }
}

#[test]
fn a_whole_response_brings_its_thinking_and_signature_to_every_client() {
    for client in ALL {
        for upstream in ALL {
            let s = session(client, upstream, false);
            let body = upstream_response_with_thinking(upstream).to_string();
            let out = s.response(body.as_bytes()).unwrap();
            check_thinking(
                client,
                upstream,
                "整包",
                &decode_client_response(client, &out),
            );
        }
    }
}

#[test]
fn a_stream_brings_its_thinking_and_signature_to_every_client() {
    for client in ALL {
        for upstream in ALL {
            let s = session(client, upstream, true);
            let mut c = s.stream();
            let mut out = Vec::new();
            for piece in upstream_stream_with_thinking(upstream).as_bytes().chunks(7) {
                out.extend(c.process(piece));
            }
            out.extend(c.finish());
            check_thinking(
                client,
                upstream,
                "流式",
                &decode_client_stream(client, &out),
            );
        }
    }
}

#[test]
fn a_collected_stream_brings_its_thinking_and_signature_to_every_client() {
    for client in ALL {
        for upstream in ALL {
            let s = session(client, upstream, false);
            let mut c = s.collector();
            for piece in upstream_stream_with_thinking(upstream).as_bytes().chunks(7) {
                c.process(piece);
            }
            let out = c.finish().unwrap();
            check_thinking(
                client,
                upstream,
                "收集",
                &decode_client_response(client, &out),
            );
        }
    }
}
