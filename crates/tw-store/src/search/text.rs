//! 从存下来的正文里拆出要找的那两段文字：请求里新的那一轮、回答（文字和工具调用）。
//!
//! **怎么读四种格式不在这里写**：请求按 tw-dialect 解成中间表示，回答按它的整包解码和
//! 流读法读 —— 网关转换格式用的就是这几样。这里只决定读哪一段。

use std::collections::HashMap;

use serde_json::Value;
use tw_dialect::ir::{Block, BlockKind, Delta, Dialect, Event, Part, Role, ToolInput};

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
                // `compaction_trigger`：解码会在末尾加一条请上游写摘要的话，那不是用户说的
                Some("item_reference" | "compaction" | "compaction_trigger")
            )
        });
    }
}

/// 回答：文字和工具调用，按出现的先后一块一行。推理不算 —— 那是模型想的，不是它说的、
/// 做的。
///
/// **工具调用要算。**编程客户端那边，模型真正做的事就是工具调用：一次 Edit 带着它写的
/// 代码，一次 Bash 带着它跑的命令，「哪一次跑了 npm install」要找得到。一个调用一行：名字、
/// 一个空格、参数。函数工具的参数是 JSON —— 流里是模型写出来的原样（同一块的片段接在
/// 一起）；整包的回答里它已经是一个 JSON 对象，按紧凑的 JSON 写出（键按字母排）。自由格式
/// 的工具（Codex 的 `apply_patch`）是它的原文。
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
                match b {
                    Block::Text(t) => line(&mut out, t),
                    Block::ToolCall(c) => line(&mut out, &call(&c.name, &input_text(&c.input))),
                    Block::Thinking(_) => {}
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
            // **一小块一小块地喂。**拆帧器从缓冲区头上一帧一帧地取，整份（最多 4 MB）一次喂
            // 进去的话，每取一帧都要把后面的字节往前挪一遍
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

/// 整包回答里一个工具调用的参数写成文字。
fn input_text(input: &ToolInput) -> String {
    match input {
        // 不是合法 JSON 的参数，tw-dialect 原样留成一个字符串（见 `ToolInput::from_json_text`）：
        // 照原样交出去，不再给它套一层引号和转义
        ToolInput::Json(Value::String(s)) | ToolInput::Text(s) => s.clone(),
        ToolInput::Json(v) => v.to_string(),
    }
}

/// 一个工具调用的那一行：名字，有参数的话一个空格接着参数
fn call(name: &str, input: &str) -> String {
    match (name.is_empty(), input.is_empty()) {
        (_, true) => name.to_string(),
        (true, false) => input.to_string(),
        (false, false) => format!("{name} {input}"),
    }
}

/// 流里的事件拼回一块一块：文字块接起它的文字增量，工具调用接起它的参数片段。按块第一次
/// 出现的先后排，一块一行
fn text_of(events: Vec<Event>) -> String {
    enum Piece {
        Text(String),
        Call { name: String, input: String },
    }
    let mut pieces: Vec<Piece> = Vec::new();
    // 块号 → 它在 `pieces` 里的位置
    let mut at: HashMap<usize, usize> = HashMap::new();
    for e in events {
        match e {
            Event::BlockStart {
                index,
                kind: BlockKind::ToolCall { name, .. },
            } => {
                at.insert(index, pieces.len());
                pieces.push(Piece::Call {
                    name,
                    input: String::new(),
                });
            }
            Event::Delta {
                index,
                delta: Delta::Text(t),
            } => match at.get(&index).and_then(|&i| pieces.get_mut(i)) {
                Some(Piece::Text(s)) => s.push_str(&t),
                // 文字块不一定先报开始（Bedrock 的就没有）：第一段增量到了就算开了一块
                _ => {
                    at.insert(index, pieces.len());
                    pieces.push(Piece::Text(t));
                }
            },
            Event::Delta {
                index,
                delta: Delta::ToolInput(p),
            } => match at.get(&index).and_then(|&i| pieces.get_mut(i)) {
                Some(Piece::Call { input, .. }) => input.push_str(&p),
                _ => {
                    at.insert(index, pieces.len());
                    pieces.push(Piece::Call {
                        name: String::new(),
                        input: p,
                    });
                }
            },
            _ => {}
        }
    }
    let mut out = String::new();
    for p in pieces {
        match p {
            Piece::Text(t) => line(&mut out, &t),
            Piece::Call { name, input } => line(&mut out, &call(&name, &input)),
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

    /// Codex 要压缩前文的请求：末尾的 `compaction_trigger` 在转换时变成一句请上游写摘要的话，
    /// 那不是用户说的。这一轮读到的是用户最后说的那句
    #[test]
    fn a_compaction_request_reads_the_users_last_words_not_the_gateways() {
        let body = json!({"input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "改完了吗"}]},
            {"type": "compaction_trigger"}
        ]});
        assert_eq!(turn(body, "/v1/responses").unwrap(), "改完了吗");
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

    /// 文字和工具调用按出现的先后一块一行，推理不算。工具调用是名字加参数：流里的参数
    /// 片段原样接起来；开始帧里就给全了的（不再发片段）也算
    #[test]
    fn the_answer_of_an_anthropic_stream_is_its_text_and_tool_calls() {
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
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "我来装"}}),
            ),
            ("ping", json!({"type": "ping"})),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "依赖"}}),
            ),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "t", "name": "Bash", "input": {}}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"command\": \"npm ins"}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "tall\", \"description\": \"装依赖\"}"}}),
            ),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 3, "content_block": {"type": "tool_use", "id": "u", "name": "Read", "input": {"path": "a.rs"}}}),
            ),
            (
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 9}}),
            ),
        ]);
        assert_eq!(
            answer(&body, Dialect::Anthropic).unwrap(),
            "我来装依赖\nBash {\"command\": \"npm install\", \"description\": \"装依赖\"}\nRead {\"path\":\"a.rs\"}"
        );
    }

    #[test]
    fn the_answer_is_read_from_streams_of_every_format() {
        let mut chat = sse(&[
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
                json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_1",
                    "type": "function", "function": {"name": "Bash", "arguments": ""}}]}}]}),
            ),
            (
                "",
                json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0,
                    "function": {"arguments": "{\"command\":"}}]}}]}),
            ),
            (
                "",
                json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0,
                    "function": {"arguments": "\"npm install\"}"}}]}}]}),
            ),
            (
                "",
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
            ),
        ]);
        chat.extend_from_slice(b"data: [DONE]\n\n");
        assert_eq!(
            answer(&chat, Dialect::Chat).unwrap(),
            "你好\nBash {\"command\":\"npm install\"}"
        );

        // Codex：函数工具的参数是 JSON 片段，自由格式的 apply_patch 是原文片段
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
                "response.output_item.added",
                json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "reasoning"}}),
            ),
            (
                "response.reasoning_summary_text.delta",
                json!({"type": "response.reasoning_summary_text.delta", "output_index": 1, "delta": "推理不算"}),
            ),
            (
                "response.output_item.added",
                json!({"type": "response.output_item.added", "output_index": 2,
                    "item": {"type": "function_call", "call_id": "c1", "name": "shell", "arguments": ""}}),
            ),
            (
                "response.function_call_arguments.delta",
                json!({"type": "response.function_call_arguments.delta", "output_index": 2, "delta": "{\"command\":[\"npm\","}),
            ),
            (
                "response.function_call_arguments.delta",
                json!({"type": "response.function_call_arguments.delta", "output_index": 2, "delta": "\"install\"]}"}),
            ),
            (
                "response.output_item.done",
                json!({"type": "response.output_item.done", "output_index": 2,
                    "item": {"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{\"command\":[\"npm\",\"install\"]}"}}),
            ),
            (
                "response.output_item.added",
                json!({"type": "response.output_item.added", "output_index": 3,
                    "item": {"type": "custom_tool_call", "call_id": "c2", "name": "apply_patch", "input": ""}}),
            ),
            (
                "response.custom_tool_call_input.delta",
                json!({"type": "response.custom_tool_call_input.delta", "output_index": 3, "delta": "*** Begin Patch\n"}),
            ),
            (
                "response.custom_tool_call_input.delta",
                json!({"type": "response.custom_tool_call_input.delta", "output_index": 3, "delta": "*** End Patch"}),
            ),
            (
                "response.completed",
                json!({"type": "response.completed", "response": {"usage": {"input_tokens": 1, "output_tokens": 2}}}),
            ),
        ]);
        assert_eq!(
            answer(&responses, Dialect::Responses).unwrap(),
            "构建成功\nshell {\"command\":[\"npm\",\"install\"]}\napply_patch *** Begin Patch\n*** End Patch"
        );

        let gemini = sse(&[
            (
                "",
                json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "晴"}]}}]}),
            ),
            (
                "",
                json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "天"},
                    {"functionCall": {"name": "run_shell_command", "args": {"command": "npm install"}}}]},
                    "finishReason": "STOP"}]}),
            ),
        ]);
        assert_eq!(
            answer(&gemini, Dialect::Gemini).unwrap(),
            "晴天\nrun_shell_command {\"command\":\"npm install\"}"
        );

        // 不带 alt=sse 的 Gemini 流是一个 JSON 数组
        let array = json!([
            {"candidates": [{"content": {"role": "model", "parts": [{"text": "数"}]}}]},
            {"candidates": [{"content": {"role": "model", "parts": [{"text": "组"},
                {"functionCall": {"name": "run_shell_command", "args": {"command": "npm install"}}}]}}]}
        ]);
        assert_eq!(
            answer(array.to_string().as_bytes(), Dialect::Gemini).unwrap(),
            "数组\nrun_shell_command {\"command\":\"npm install\"}"
        );

        // Bedrock 的二进制帧在网关进门时转成了 SSE：事件名进 `event:`
        let bedrock = sse(&[
            ("messageStart", json!({"role": "assistant"})),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex": 0, "delta": {"text": "基岩"}}),
            ),
            ("contentBlockStop", json!({"contentBlockIndex": 0})),
            (
                "contentBlockStart",
                json!({"contentBlockIndex": 1, "start": {"toolUse": {"toolUseId": "t1", "name": "Bash"}}}),
            ),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": "{\"command\":"}}}),
            ),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": "\"npm install\"}"}}}),
            ),
            ("contentBlockStop", json!({"contentBlockIndex": 1})),
            ("messageStop", json!({"stopReason": "tool_use"})),
        ]);
        assert_eq!(
            answer(&bedrock, Dialect::Bedrock).unwrap(),
            "基岩\nBash {\"command\":\"npm install\"}"
        );
    }

    /// 整包的回答：工具调用的参数已经是 JSON 对象，按紧凑的 JSON 写出；自由格式的是原文
    #[test]
    fn the_answer_is_read_from_whole_responses_of_every_format() {
        let bash = "Bash {\"command\":\"npm install\"}";
        let cases = [
            (
                Dialect::Anthropic,
                json!({"content": [
                    {"type": "thinking", "thinking": "推理不算"},
                    {"type": "text", "text": "整包"},
                    {"type": "tool_use", "id": "t", "name": "Bash",
                        "input": {"command": "npm install", "description": "装依赖"}}]}),
                "整包\nBash {\"command\":\"npm install\",\"description\":\"装依赖\"}".to_string(),
            ),
            (
                Dialect::Chat,
                json!({"choices": [{"message": {"role": "assistant", "content": "整包",
                    "reasoning_content": "推理不算",
                    "tool_calls": [{"id": "c", "type": "function",
                        "function": {"name": "Bash", "arguments": "{\"command\": \"npm install\"}"}}]}}]}),
                format!("整包\n{bash}"),
            ),
            (
                Dialect::Responses,
                json!({"output": [
                    {"type": "reasoning", "summary": [{"type": "summary_text", "text": "推理不算"}]},
                    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "整包"}]},
                    {"type": "function_call", "call_id": "c1", "name": "Bash", "arguments": "{\"command\":\"npm install\"}"},
                    {"type": "custom_tool_call", "call_id": "c2", "name": "apply_patch", "input": "*** Begin Patch"}]}),
                format!("整包\n{bash}\napply_patch *** Begin Patch"),
            ),
            (
                Dialect::Gemini,
                json!({"candidates": [{"content": {"parts": [
                    {"text": "推理不算", "thought": true},
                    {"text": "整包"},
                    {"functionCall": {"name": "Bash", "args": {"command": "npm install"}}}]}}]}),
                format!("整包\n{bash}"),
            ),
            (
                Dialect::Bedrock,
                json!({"output": {"message": {"role": "assistant", "content": [
                    {"reasoningContent": {"reasoningText": {"text": "推理不算"}}},
                    {"text": "整包"},
                    {"toolUse": {"toolUseId": "t", "name": "Bash", "input": {"command": "npm install"}}}]}},
                    "stopReason": "tool_use"}),
                format!("整包\n{bash}"),
            ),
        ];
        for (d, v, want) in cases {
            assert_eq!(
                answer(v.to_string().as_bytes(), d).as_deref(),
                Some(want.as_str()),
                "{d:?}"
            );
        }
    }

    /// 不是合法 JSON 的参数照模型写的原样，不套引号和转义；只有工具调用的回答也有字
    #[test]
    fn tool_arguments_that_are_not_json_stay_as_written() {
        let v = json!({"choices": [{"message": {"role": "assistant", "content": null,
            "tool_calls": [{"id": "c", "type": "function",
                "function": {"name": "Bash", "arguments": "npm install --save"}}]}}]});
        assert_eq!(
            answer(v.to_string().as_bytes(), Dialect::Chat).as_deref(),
            Some("Bash npm install --save")
        );
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
