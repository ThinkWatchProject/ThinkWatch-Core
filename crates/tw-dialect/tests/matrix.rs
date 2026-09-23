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
