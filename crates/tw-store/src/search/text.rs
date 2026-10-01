//! 从存下来的正文里拆出要找的那两段文字：请求里新的那一轮、回答。
//!
//! **怎么读四种格式不在这里写**：请求按 tw-dialect 解成中间表示，回答按它的整包解码和
//! 流读法读 —— 网关转换格式用的就是这几样。这里只决定读哪一段。

use serde_json::Value;
use tw_dialect::ir::{Block, Delta, Dialect, Event, Part, Role};

/// 这个路径是不是生成回答的那个调用；是的话，客户端说的是哪种格式。
///
/// **和网关认的是同一批路径**（`tw_gateway::client_api::ClientApi` 的 `of_path` 加
/// `generates`，tw-control 的测试逐条核对两边）。只读生成回答的那些：数 token 的请求带着
/// 和下一个请求一样的整段对话，读它只会给同一轮话多出一条重复的命中。
pub fn client_dialect(path: &str) -> Option<Dialect> {
    let p = path.trim_end_matches('/');
    // 有的客户端把 base_url 写成带 `/v1` 的，有的不带
    match p.strip_prefix("/v1").unwrap_or(p) {
        "/messages" => return Some(Dialect::Anthropic),
        "/chat/completions" => return Some(Dialect::Chat),
        "/responses" => return Some(Dialect::Responses),
        _ => {}
    }
    if p == "/backend-api/codex/responses" {
        return Some(Dialect::Responses);
    }
    (p.contains("/models/")
        && (p.ends_with(":generateContent") || p.ends_with(":streamGenerateContent")))
    .then_some(Dialect::Gemini)
}

/// 契约里的格式名换成 tw-dialect 的。
pub fn dialect_of(d: tw_api::Dialect) -> Dialect {
    match d {
        tw_api::Dialect::Anthropic => Dialect::Anthropic,
        tw_api::Dialect::OpenaiChat => Dialect::Chat,
        tw_api::Dialect::OpenaiResponses => Dialect::Responses,
        tw_api::Dialect::Gemini => Dialect::Gemini,
        tw_api::Dialect::Bedrock => Dialect::Bedrock,
    }
}

/// 请求里新的那一轮：对话末尾连着的那几条用户消息里的文字和工具结果，一段一行。
///
/// **只读这一轮。**Claude Code 这类客户端每个请求都把整段对话重发一遍：按整个请求体找的
/// 话，第三轮里说过的一句话会让之后每一个请求都对上。每个请求里新的只有最后这一轮 ——
/// 用户刚说的话，或者刚跑完的工具交回来的结果。
///
/// 解不开（截断了的、不是 JSON 的）就是 `None`。
pub fn user_turn(body: &[u8], path: &str) -> Option<String> {
    let dialect = client_dialect(path)?;
    let mut v: Value = serde_json::from_slice(body).ok()?;
    trim_for_reading(&mut v, dialect);
    let decoded = tw_dialect::convert::decode(dialect, &v, path, None).ok()?;
    let msgs = &decoded.request.messages;
    let last = msgs.iter().rposition(|m| m.role == Role::User)?;
    let first = msgs[..last]
        .iter()
        .rposition(|m| m.role != Role::User)
        .map_or(0, |i| i + 1);
    let mut out = String::new();
    for m in &msgs[first..=last] {
        for p in &m.parts {
            match p {
                Part::Text(t) => line(&mut out, t),
                Part::ToolResult(r) => line(&mut out, &r.text()),
                _ => {}
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

/// 解码之前去掉读文字用不着、又会让解码白费力气或者干脆拒绝的东西。
///
/// - **服务端状态的引用**（Responses 的 `previous_response_id`、`compaction` 这些，Gemini 的
///   `cachedContent`）：tw-dialect 见到它们就拒绝转换 —— 转给别家的上游会丢掉前文。可这里
///   只读请求里写着的字，那部分前文本来就不在请求里，不该因此连最后一轮也读不到。
/// - **工具定义**：Claude Code 每个请求带着几十 KB，读文字用不着。
/// - **最后一条助手消息之前的对话**：新的那一轮一定在它之后。长对话的请求体大头就在这里，
///   解码会把每张截图的 base64 都复制一遍。只认确定是助手说的（`role` 写着 `assistant` /
///   `model`）：剩下的交给解码去分，拿不准的留着不会读错，只是多读几条。
fn trim_for_reading(v: &mut Value, dialect: Dialect) {
    let Some(o) = v.as_object_mut() else { return };
    o.remove("tools");
    let turns = match dialect {
        Dialect::Responses => {
            for k in [
                "previous_response_id",
                "conversation",
                "prompt",
                "background",
            ] {
                o.remove(k);
            }
            "input"
        }
        Dialect::Gemini => {
            o.remove("cachedContent");
            o.remove("cached_content");
            "contents"
        }
        _ => "messages",
    };
    let Some(items) = o.get_mut(turns).and_then(Value::as_array_mut) else {
        return;
    };
    let assistant = |m: &Value| {
        matches!(
            m.get("role").and_then(Value::as_str),
            Some("assistant" | "model")
        )
    };
    // 末尾的助手消息是预填（prefill，替模型开个头）：新的那一轮在它前面，从它前面找
    let end = items
        .iter()
        .rposition(|m| !assistant(m))
        .map_or(0, |i| i + 1);
    if let Some(i) = items[..end].iter().rposition(assistant) {
        items.drain(..=i);
    }
    if dialect == Dialect::Responses {
        items.retain(|it| {
            !matches!(
                it.get("type").and_then(Value::as_str),
                Some("item_reference" | "compaction")
            )
        });
    }
}

/// 回答的文字，一个文字块一行。推理和工具调用不算 —— 它们不是写给人看的那部分。
///
/// `body` 是上游的原话（存的就是它）：整包的 JSON、SSE 的流（Bedrock 的二进制帧在网关
/// 进门时已经转成了 SSE），或者 Gemini 不带 `alt=sse` 时那个逐步写出的 JSON 数组。按
/// 开头的字节分辨，不看请求要没要流：上游只给流、客户端要整包的时候，存下来的是流。
/// `upstream` 是回答它的那一家说的格式。
pub fn answer(body: &[u8], upstream: Dialect) -> Option<String> {
    let out = match body.iter().find(|b| !b.is_ascii_whitespace())? {
        b'{' => {
            let v: Value = serde_json::from_slice(body).ok()?;
            let r = match upstream {
                Dialect::Anthropic => tw_dialect::anthropic::decode_response(&v),
                Dialect::Chat => tw_dialect::chat::decode_response(&v),
                Dialect::Responses => tw_dialect::responses::decode_response(&v),
                Dialect::Gemini => tw_dialect::gemini::decode_response(&v),
                Dialect::Bedrock => tw_dialect::bedrock::decode_response(&v),
            };
            let mut out = String::new();
            for b in &r.blocks {
                if let Block::Text(t) = b {
                    line(&mut out, t);
                }
            }
            out
        }
        b'[' if upstream == Dialect::Gemini => {
            let v: Value = serde_json::from_slice(body).ok()?;
            let mut p = tw_dialect::gemini::stream::Parser::default();
            let mut events = Vec::new();
            for chunk in v.as_array()? {
                p.chunk(chunk, &mut events);
            }
            p.finish(&mut events);
            text_of(events)
        }
        _ => {
            // **一小块一小块地喂。**拆帧器从缓冲区头上一帧一帧地取，整份 256 KB 一次喂进去
            // 的话，每取一帧都要把后面的字节往前挪一遍
            let mut r = tw_dialect::convert::Reader::new(upstream);
            let mut events = Vec::new();
            for piece in body.chunks(16 * 1024) {
                events.extend(r.feed(piece));
            }
            events.extend(r.finish());
            text_of(events)
        }
    };
    (!out.is_empty()).then_some(out)
}

/// 流里的文字增量接起来。换了一个文字块就另起一行
fn text_of(events: Vec<Event>) -> String {
    let mut out = String::new();
    let mut block = None;
    for e in events {
        if let Event::Delta {
            index,
            delta: Delta::Text(t),
        } = e
        {
            if block != Some(index) && !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            block = Some(index);
            out.push_str(&t);
        }
    }
    out
}

fn line(out: &mut String, t: &str) {
    if t.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(t);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn turn(body: Value, path: &str) -> Option<String> {
        user_turn(body.to_string().as_bytes(), path)
    }

    #[test]
    fn the_same_paths_as_the_gateway_are_generation_calls() {
        assert_eq!(client_dialect("/v1/messages"), Some(Dialect::Anthropic));
        assert_eq!(client_dialect("/messages/"), Some(Dialect::Anthropic));
        assert_eq!(client_dialect("/v1/chat/completions"), Some(Dialect::Chat));
        assert_eq!(client_dialect("/v1/responses"), Some(Dialect::Responses));
        assert_eq!(
            client_dialect("/backend-api/codex/responses"),
            Some(Dialect::Responses)
        );
        assert_eq!(
            client_dialect("/v1beta/models/gemini-2.5-pro:streamGenerateContent"),
            Some(Dialect::Gemini)
        );
        for p in [
            "/v1/messages/count_tokens",
            "/v1/embeddings",
            "/v1beta/models/gemini-2.5-pro:countTokens",
            "/v1/models",
        ] {
            assert_eq!(client_dialect(p), None, "{p}");
        }
    }

    /// 长对话里只读最后一轮：前面说过的话不算这个请求的
    #[test]
    fn only_the_last_user_turn_of_a_long_conversation_is_read() {
        let mut messages = vec![json!({"role": "user", "content": "第一轮：帮我找 OLD-WORD"})];
        for i in 0..30 {
            messages.push(json!({"role": "assistant", "content": [
                {"type": "text", "text": format!("回复 {i}")},
                {"type": "tool_use", "id": format!("t{i}"), "name": "Read", "input": {"path": "a.rs"}}
            ]}));
            messages.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": format!("t{i}"), "content": format!("文件内容 {i}")}
            ]}));
        }
        messages.push(json!({"role": "assistant", "content": "好的"}));
        messages.push(json!({"role": "user", "content": [
            {"type": "text", "text": "<system-reminder>提醒</system-reminder>"},
            {"type": "text", "text": "最后一句 NEW-WORD"}
        ]}));
        let body = json!({"model": "claude-sonnet-4-5", "system": "你是助手", "messages": messages,
            "tools": [{"name": "Read", "input_schema": {"type": "object"}}]});
        let t = turn(body, "/v1/messages").unwrap();
        assert_eq!(
            t,
            "<system-reminder>提醒</system-reminder>\n最后一句 NEW-WORD"
        );
        assert!(!t.contains("OLD-WORD"));
    }

    /// 工具跑完交回来的结果就是这一轮新的东西
    #[test]
    fn tool_results_are_part_of_the_turn_in_every_format() {
        let anthropic = json!({"messages": [
            {"role": "user", "content": "跑一下"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "a", "name": "Bash", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a",
                "content": [{"type": "text", "text": "exit 0: 编译通过"}]}]}
        ]});
        assert_eq!(turn(anthropic, "/v1/messages").unwrap(), "exit 0: 编译通过");

        // Chat 的工具结果是 `tool` 角色，几条连着的都算这一轮
        let chat = json!({"messages": [
            {"role": "system", "content": "sys"},
            {"role": "user", "content": "跑一下"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "a", "type": "function", "function": {"name": "ls", "arguments": "{}"}},
                {"id": "b", "type": "function", "function": {"name": "pwd", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "a", "content": "README.md"},
            {"role": "tool", "tool_call_id": "b", "content": "/home/me"}
        ]});
        assert_eq!(
            turn(chat, "/v1/chat/completions").unwrap(),
            "README.md\n/home/me"
        );

        let responses = json!({"instructions": "You are Codex", "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "跑一下"}]},
            {"type": "reasoning", "summary": [], "encrypted_content": "gAAA"},
            {"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": "cargo build 成功"}
        ]});
        assert_eq!(
            turn(responses, "/backend-api/codex/responses").unwrap(),
            "cargo build 成功"
        );

        let gemini = json!({"contents": [
            {"role": "user", "parts": [{"text": "跑一下"}]},
            {"role": "model", "parts": [{"functionCall": {"name": "run", "args": {}}}]},
            {"role": "user", "parts": [{"functionResponse": {"name": "run", "response": {"output": "完成"}}}]}
        ]});
        assert_eq!(
            turn(
                gemini,
                "/v1beta/models/gemini-2.5-pro:streamGenerateContent"
            )
            .unwrap(),
            "完成"
        );
    }

    /// 末尾替模型开了个头（预填）：新的那一轮是它前面那条用户消息
    #[test]
    fn a_prefilled_answer_does_not_hide_the_turn_before_it() {
        let body = json!({"messages": [
            {"role": "user", "content": "旧问题"},
            {"role": "assistant", "content": "旧回答"},
            {"role": "user", "content": "给我一段 JSON"},
            {"role": "assistant", "content": "{"}
        ]});
        assert_eq!(turn(body, "/v1/messages").unwrap(), "给我一段 JSON");
        let only = json!({"messages": [{"role": "assistant", "content": "{"}]});
        assert_eq!(turn(only, "/v1/messages"), None);
    }

    /// Responses 的 `input` 也可以直接是一句话
    #[test]
    fn a_responses_input_can_be_one_string() {
        let body = json!({"model": "gpt-5.5", "input": "一句话"});
        assert_eq!(turn(body, "/v1/responses").unwrap(), "一句话");
    }

    /// 带着服务端状态的请求转不了别家，可它写着的字照样读得到
    #[test]
    fn server_side_state_does_not_stop_the_turn_from_being_read() {
        let body = json!({"previous_response_id": "resp_1", "background": true, "input": [
            {"type": "compaction", "encrypted_content": "gAAA"},
            {"type": "item_reference", "id": "msg_1"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "接着来"}]}
        ]});
        assert_eq!(turn(body, "/v1/responses").unwrap(), "接着来");
        let gemini = json!({"cachedContent": "cachedContents/abc",
            "contents": [{"role": "user", "parts": [{"text": "缓存之后"}]}]});
        assert_eq!(
            turn(gemini, "/v1beta/models/gemini-2.5-pro:generateContent").unwrap(),
            "缓存之后"
        );
    }

    /// Python 客户端把 ASCII 以外的字全写成 `\uXXXX`：按 JSON 解开之后就是原字
    #[test]
    fn escaped_text_is_read_as_the_characters_it_stands_for() {
        let body = b"{\"messages\":[{\"role\":\"user\",\"content\":\"\x5cu8bf7\x5cu5e2e\x5cu6211\x5cu770b\x5cu770b\"}]}";
        assert_eq!(user_turn(body, "/v1/messages").unwrap(), "请帮我看看");
    }

    #[test]
    fn a_body_that_cannot_be_read_gives_nothing() {
        assert_eq!(
            user_turn(b"{\"messages\":[{\"role\":\"user\"", "/v1/messages"),
            None
        );
        assert_eq!(user_turn(b"{\"messages\":[]}", "/v1/messages"), None);
        assert_eq!(user_turn(b"{}", "/v1/messages/count_tokens"), None);
    }

    fn sse(frames: &[(&str, Value)]) -> Vec<u8> {
        let mut s = String::new();
        for (event, data) in frames {
            if !event.is_empty() {
                s.push_str(&format!("event: {event}\n"));
            }
            s.push_str(&format!("data: {data}\n\n"));
        }
        s.into_bytes()
    }

    #[test]
    fn the_answer_of_an_anthropic_stream_is_its_text_deltas() {
        let body = sse(&[
            (
                "message_start",
                json!({"type": "message_start", "message": {"id": "m", "model": "x", "usage": {"input_tokens": 3}}}),
            ),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "推理不算"}}),
            ),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "答案是"}}),
            ),
            ("ping", json!({"type": "ping"})),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "四十二"}}),
            ),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "t", "name": "Write", "input": {}}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"x\":1}"}}),
            ),
            (
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 9}}),
            ),
        ]);
        assert_eq!(answer(&body, Dialect::Anthropic).unwrap(), "答案是四十二");
    }

    #[test]
    fn the_answer_is_read_from_streams_of_every_format() {
        let chat = sse(&[
            (
                "",
                json!({"choices": [{"index": 0, "delta": {"role": "assistant", "content": "你"}}]}),
            ),
            (
                "",
                json!({"choices": [{"index": 0, "delta": {"content": "好"}}]}),
            ),
            (
                "",
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
            ),
        ]);
        let mut chat = chat;
        chat.extend_from_slice(b"data: [DONE]\n\n");
        assert_eq!(answer(&chat, Dialect::Chat).unwrap(), "你好");

        let responses = sse(&[
            (
                "response.created",
                json!({"type": "response.created", "response": {"id": "r"}}),
            ),
            (
                "response.output_item.added",
                json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message"}}),
            ),
            (
                "response.output_text.delta",
                json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": "构建"}),
            ),
            (
                "response.output_text.delta",
                json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": "成功"}),
            ),
            (
                "response.completed",
                json!({"type": "response.completed", "response": {"usage": {"input_tokens": 1, "output_tokens": 2}}}),
            ),
        ]);
        assert_eq!(answer(&responses, Dialect::Responses).unwrap(), "构建成功");

        let gemini = sse(&[
            (
                "",
                json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "晴"}]}}]}),
            ),
            (
                "",
                json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "天"}]}, "finishReason": "STOP"}]}),
            ),
        ]);
        assert_eq!(answer(&gemini, Dialect::Gemini).unwrap(), "晴天");

        // 不带 alt=sse 的 Gemini 流是一个 JSON 数组
        let array = json!([
            {"candidates": [{"content": {"role": "model", "parts": [{"text": "数"}]}}]},
            {"candidates": [{"content": {"role": "model", "parts": [{"text": "组"}]}}]}
        ]);
        assert_eq!(
            answer(array.to_string().as_bytes(), Dialect::Gemini).unwrap(),
            "数组"
        );

        // Bedrock 的二进制帧在网关进门时转成了 SSE：事件名进 `event:`
        let bedrock = sse(&[
            ("messageStart", json!({"role": "assistant"})),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex": 0, "delta": {"text": "基岩"}}),
            ),
            ("contentBlockStop", json!({"contentBlockIndex": 0})),
            ("messageStop", json!({"stopReason": "end_turn"})),
        ]);
        assert_eq!(answer(&bedrock, Dialect::Bedrock).unwrap(), "基岩");
    }

    #[test]
    fn the_answer_is_read_from_whole_responses_of_every_format() {
        let cases = [
            (
                Dialect::Anthropic,
                json!({"content": [
                {"type": "thinking", "thinking": "不算"},
                {"type": "text", "text": "整包"},
                {"type": "tool_use", "id": "t", "name": "x", "input": {}}]}),
            ),
            (
                Dialect::Chat,
                json!({"choices": [{"message": {"role": "assistant", "content": "整包"}}]}),
            ),
            (
                Dialect::Responses,
                json!({"output": [{"type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "整包"}]}]}),
            ),
            (
                Dialect::Gemini,
                json!({"candidates": [{"content": {"parts": [{"text": "整包"}]}}]}),
            ),
            (
                Dialect::Bedrock,
                json!({"output": {"message": {"role": "assistant",
                "content": [{"text": "整包"}]}}, "stopReason": "end_turn"}),
            ),
        ];
        for (d, v) in cases {
            assert_eq!(
                answer(v.to_string().as_bytes(), d).as_deref(),
                Some("整包"),
                "{d:?}"
            );
        }
    }

    /// 流里的 `\u` 转义也一样解开
    #[test]
    fn escaped_text_in_a_stream_is_read_as_characters() {
        let body = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\\u4f60\\u597d\"}}]}\n\ndata: [DONE]\n\n";
        assert_eq!(answer(body, Dialect::Chat).unwrap(), "你好");
    }

    /// 两个文字块之间另起一行：一个词不会被拼在两块的接缝上
    #[test]
    fn separate_text_blocks_go_on_separate_lines() {
        let body = sse(&[
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "前一块"}}),
            ),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 2, "content_block": {"type": "text", "text": ""}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 2, "delta": {"type": "text_delta", "text": "后一块"}}),
            ),
        ]);
        assert_eq!(answer(&body, Dialect::Anthropic).unwrap(), "前一块\n后一块");
    }

    #[test]
    fn an_answer_without_text_gives_nothing() {
        assert_eq!(answer(b"", Dialect::Anthropic), None);
        assert_eq!(
            answer(b"{\"error\":{\"message\":\"x\"}}", Dialect::Anthropic),
            None
        );
        assert_eq!(answer(b"{\"choices\":[", Dialect::Chat), None);
    }
}
