//! OpenAI chat 的 SSE 流 → Anthropic 的 SSE 流。
//!
//! 这是整个互转里最难的一段，因为两边的**流形状根本不一样**：
//!
//! | | OpenAI | Anthropic |
//! |---|---|---|
//! | 帧 | 一种（`choices[].delta`） | 六种（`message_start` / `content_block_start` / `_delta` / `_stop` / `message_delta` / `message_stop`） |
//! | 内容边界 | 没有，文本和工具调用混在同一串 delta 里 | 显式的 block 开始和结束 |
//! | usage | 最后一个 chunk（还得请求方开了 `include_usage`） | `message_start` 给输入、`message_delta` 给输出 |
//! | 结束 | `data: [DONE]` | `message_stop` |
//!
//! 所以这一层是个**状态机**：它要自己判断「一个 content block 在哪儿
//! 开始、在哪儿结束」，因为源流里没有这个信息。
//!
//! # 一处必须做对的地方
//!
//! Anthropic 的输入 token 在 `message_start` 里，而 OpenAI 的 usage 在
//! **最后**。我们没法在开头就说出输入是多少 —— 硬等的话就变成整块缓冲，
//! 那是出站直通明令不许的。
//!
//! 所以 `message_start` 里的 usage 先写 0，真数字在结尾的
//! `message_delta` 里给。**客户端本来就要看那一个**（Anthropic 自己
//! 的流也是在 `message_delta` 里更新 output），所以这不是撒谎，是把
//! 一个本来就分两次给的东西按对方的节奏给。而我们自己的 usage 嗅探器
//! 认的也是同一个位置。

use serde_json::{Value, json};

/// 一条流的转换器。
pub struct Converter {
    model: String,
    /// 还没收齐的那一帧
    partial: Vec<u8>,
    started: bool,
    /// 当前开着的 block 是第几个、是什么类型
    open: Option<(u32, Kind)>,
    next_index: u32,
    /// 已经发过的工具调用：OpenAI 的 `index` → 我们分配的 block index
    tools: Vec<(u64, u32)>,
    stop_reason: Option<&'static str>,
    usage: Option<Value>,
    done: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Text,
    Tool,
}

fn frame(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

impl Converter {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            partial: Vec::new(),
            started: false,
            open: None,
            next_index: 0,
            tools: Vec::new(),
            stop_reason: None,
            usage: None,
            done: false,
        }
    }

    /// 喂一块上游字节，吐出翻译好的 Anthropic 帧。
    pub fn process(&mut self, chunk: &[u8]) -> String {
        self.partial.extend_from_slice(chunk);
        let mut out = String::new();
        while let Some(end) = find_frame_end(&self.partial) {
            let raw: Vec<u8> = self.partial.drain(..end).collect();
            let Ok(text) = std::str::from_utf8(&raw) else {
                continue;
            };
            for line in text.lines() {
                let Some(payload) = line.strip_prefix("data: ") else {
                    continue;
                };
                let payload = payload.trim();
                if payload == "[DONE]" {
                    out.push_str(&self.finish());
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(payload) else {
                    continue;
                };
                out.push_str(&self.event(&v));
            }
        }
        out
    }

    /// 流结束了。**幂等** —— `[DONE]` 和连接关闭都可能触发它。
    pub fn finish(&mut self) -> String {
        if self.done {
            return String::new();
        }
        self.done = true;
        let mut out = String::new();
        out.push_str(&self.close_block());
        if !self.started {
            // 一个字都没来就断了 —— 也要给客户端一个完整的信封，
            // 否则它会一直等
            out.push_str(&self.start_message());
        }
        let usage = self
            .usage
            .clone()
            .unwrap_or_else(|| json!({ "output_tokens": 0 }));
        out.push_str(&frame(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": { "stop_reason": self.stop_reason, "stop_sequence": null },
                "usage": usage,
            }),
        ));
        out.push_str(&frame("message_stop", &json!({ "type": "message_stop" })));
        out
    }

    fn start_message(&mut self) -> String {
        self.started = true;
        frame(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": "msg_converted",
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    // 真数字在结尾的 message_delta 里 —— 见文件头那段
                    "usage": { "input_tokens": 0, "output_tokens": 0 },
                }
            }),
        )
    }

    fn close_block(&mut self) -> String {
        match self.open.take() {
            Some((i, _)) => frame(
                "content_block_stop",
                &json!({ "type": "content_block_stop", "index": i }),
            ),
            None => String::new(),
        }
    }

    fn open_block(&mut self, kind: Kind, block: Value) -> String {
        let mut out = self.close_block();
        let index = self.next_index;
        self.next_index += 1;
        self.open = Some((index, kind));
        out.push_str(&frame(
            "content_block_start",
            &json!({ "type": "content_block_start", "index": index, "content_block": block }),
        ));
        out
    }

    fn event(&mut self, v: &Value) -> String {
        let mut out = String::new();
        if !self.started {
            out.push_str(&self.start_message());
        }
        // usage 可能出现在任何一个 chunk 上（OpenAI 通常放最后一个）
        if let Some(u) = v.get("usage")
            && !u.is_null()
        {
            self.usage = Some(crate::resp::usage(Some(u)));
        }
        let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else {
            return out;
        };
        if let Some(f) = choice.get("finish_reason").and_then(|x| x.as_str())
            && let Some(s) = crate::resp::stop_reason(Some(f))
        {
            self.stop_reason = Some(s);
        }
        let Some(delta) = choice.get("delta") else {
            return out;
        };

        // ---- 文本
        if let Some(t) = delta.get("content").and_then(|x| x.as_str())
            && !t.is_empty()
        {
            if self.open.map(|(_, k)| k) != Some(Kind::Text) {
                out.push_str(&self.open_block(Kind::Text, json!({ "type": "text", "text": "" })));
            }
            let index = self.open.map(|(i, _)| i).unwrap_or(0);
            out.push_str(&frame(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": { "type": "text_delta", "text": t },
                }),
            ));
        }

        // ---- 工具调用
        if let Some(Value::Array(calls)) = delta.get("tool_calls") {
            for c in calls {
                let oai_index = c.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
                let known = self
                    .tools
                    .iter()
                    .find(|(oi, _)| *oi == oai_index)
                    .map(|(_, bi)| *bi);
                let block_index = match known {
                    Some(i) => i,
                    None => {
                        // 新的一个工具调用：开一个 block
                        let name = c
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|x| x.as_str())
                            .unwrap_or_default();
                        let id = c.get("id").and_then(|x| x.as_str()).unwrap_or_default();
                        out.push_str(&self.open_block(
                            Kind::Tool,
                            json!({ "type": "tool_use", "id": id, "name": name, "input": {} }),
                        ));
                        let i = self.open.map(|(i, _)| i).unwrap_or(0);
                        self.tools.push((oai_index, i));
                        i
                    }
                };
                if let Some(args) = c
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|x| x.as_str())
                    && !args.is_empty()
                {
                    out.push_str(&frame(
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": block_index,
                            "delta": { "type": "input_json_delta", "partial_json": args },
                        }),
                    ));
                }
            }
        }
        out
    }
}

fn find_frame_end(buf: &[u8]) -> Option<usize> {
    buf.windows(2)
        .position(|w| w == b"\n\n")
        .map(|i| i + 2)
        .or_else(|| buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&str]) -> String {
        let mut c = Converter::new("deepseek-chat");
        let mut out = String::new();
        for x in chunks {
            out.push_str(&c.process(x.as_bytes()));
        }
        out.push_str(&c.finish());
        out
    }

    /// 把翻译出来的帧解析回结构，像客户端那样。
    fn events(s: &str) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        let mut ev = String::new();
        for line in s.lines() {
            if let Some(e) = line.strip_prefix("event: ") {
                ev = e.to_string();
            } else if let Some(d) = line.strip_prefix("data: ")
                && let Ok(v) = serde_json::from_str::<Value>(d)
            {
                out.push((ev.clone(), v));
            }
        }
        out
    }

    fn d(content: &str) -> String {
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}},\"finish_reason\":null}}]}}\n\n",
            serde_json::to_string(content).unwrap()
        )
    }

    #[test]
    fn a_plain_text_stream_gets_the_full_anthropic_envelope() {
        // Anthropic 的客户端等的是六种帧。少一种它就卡住或者报错。
        let out = run(&[
            &d("你"),
            &d("好"),
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        ]);
        let ev = events(&out);
        let kinds: Vec<&str> = ev.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ],
            "{out}"
        );
        // 文本拼得回来
        let text: String = ev
            .iter()
            .filter_map(|(_, v)| v["delta"]["text"].as_str())
            .collect();
        assert_eq!(text, "你好");
        // stop_reason 和 usage 落在 message_delta 上 —— 我们自己的嗅探器
        // 认的也是这个位置
        let md = ev.iter().find(|(k, _)| k == "message_delta").unwrap();
        assert_eq!(md.1["delta"]["stop_reason"], "end_turn");
        assert_eq!(md.1["usage"]["input_tokens"], 9);
        assert_eq!(md.1["usage"]["output_tokens"], 2);
    }

    #[test]
    fn a_tool_call_streamed_in_fragments_is_reassembled_as_one_block() {
        // OpenAI 把 arguments 分片下发，而且第一片才带 name 和 id。
        let out = run(&[
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"Read\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"p\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"/a\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        ]);
        let ev = events(&out);
        let start = ev
            .iter()
            .find(|(k, v)| k == "content_block_start" && v["content_block"]["type"] == "tool_use")
            .unwrap_or_else(|| panic!("没开 tool_use 块：{out}"));
        assert_eq!(start.1["content_block"]["name"], "Read");
        assert_eq!(start.1["content_block"]["id"], "call_1");
        // 参数拼起来是合法 JSON —— 客户端就是这么用它的
        let args: String = ev
            .iter()
            .filter_map(|(_, v)| v["delta"]["partial_json"].as_str())
            .collect();
        assert_eq!(serde_json::from_str::<Value>(&args).unwrap()["p"], "/a");
        // 结束时那个块要被关掉，否则客户端认为它还没写完
        assert!(ev.iter().any(|(k, _)| k == "content_block_stop"), "{out}");
        let md = ev.iter().find(|(k, _)| k == "message_delta").unwrap();
        assert_eq!(md.1["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn text_followed_by_a_tool_call_produces_two_blocks_with_the_right_indices() {
        // 源流里没有「块边界」这个信息，状态机得自己判断。
        let out = run(&[
            &d("我看一下。"),
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c\",\"function\":{\"name\":\"Read\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        ]);
        let ev = events(&out);
        let starts: Vec<u64> = ev
            .iter()
            .filter(|(k, _)| k == "content_block_start")
            .map(|(_, v)| v["index"].as_u64().unwrap())
            .collect();
        assert_eq!(starts, [0, 1], "{out}");
        let stops: Vec<u64> = ev
            .iter()
            .filter(|(k, _)| k == "content_block_stop")
            .map(|(_, v)| v["index"].as_u64().unwrap())
            .collect();
        // 第一个块在第二个开始前就该关掉
        assert_eq!(stops, [0, 1], "{out}");
    }

    #[test]
    fn two_parallel_tool_calls_keep_their_own_indices() {
        let out = run(&[
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"A\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"b\",\"function\":{\"name\":\"B\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        ]);
        let ev = events(&out);
        let names: Vec<&str> = ev
            .iter()
            .filter(|(k, _)| k == "content_block_start")
            .filter_map(|(_, v)| v["content_block"]["name"].as_str())
            .collect();
        assert_eq!(names, ["A", "B"], "{out}");
    }

    #[test]
    fn a_frame_split_across_chunks_is_not_lost() {
        let whole = d("你好世界");
        let bytes = whole.as_bytes();
        for cut in 1..bytes.len() {
            let mut c = Converter::new("m");
            let mut out = c.process(&bytes[..cut]);
            out.push_str(&c.process(&bytes[cut..]));
            out.push_str(&c.finish());
            let text: String = events(&out)
                .iter()
                .filter_map(|(_, v)| v["delta"]["text"].as_str())
                .collect();
            assert_eq!(text, "你好世界", "在第 {cut} 字节切开时丢了内容");
        }
    }

    #[test]
    fn a_stream_that_dies_before_saying_anything_still_gets_a_full_envelope() {
        // 一个字都没来就断了，客户端也不能一直等。
        let mut c = Converter::new("m");
        let out = c.finish();
        let kinds: Vec<String> = events(&out).iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(
            kinds,
            ["message_start", "message_delta", "message_stop"],
            "{out}"
        );
    }

    #[test]
    fn finishing_twice_does_not_emit_two_envelopes() {
        // `[DONE]` 和连接关闭都可能触发它。
        let mut c = Converter::new("m");
        let a = c.finish();
        let b = c.finish();
        assert!(!a.is_empty());
        assert!(b.is_empty());
    }

    #[test]
    fn an_unknown_finish_reason_becomes_null_not_a_guess() {
        // 客户端看到 null 至少知道「没说」，看到一个编出来的 end_turn
        // 会当成正常结束。
        let out = run(&[
            &d("x"),
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"这是什么\"}]}\n\n",
            "data: [DONE]\n\n",
        ]);
        let ev = events(&out);
        let md = ev.iter().find(|(k, _)| k == "message_delta").unwrap();
        assert_eq!(md.1["delta"]["stop_reason"], Value::Null);
    }

    #[test]
    fn a_stream_without_usage_reports_zero_rather_than_nothing() {
        // 没开 include_usage 的上游。**帧的形状不能因此缺一块** ——
        // 客户端解析的是结构，不是内容
        let out = run(&[&d("x"), "data: [DONE]\n\n"]);
        let ev = events(&out);
        let md = ev.iter().find(|(k, _)| k == "message_delta").unwrap();
        assert_eq!(md.1["usage"]["output_tokens"], 0);
    }

    #[test]
    fn the_gateways_own_sniffer_can_read_the_converted_stream() {
        // **这一条是接缝**：翻译出来的流要能被我们自己的 usage 嗅探器
        // 认出来，否则成本面板对所有互转请求集体失明。
        // 嗅探器住在 tw-gateway 里，这里只验帧的形状和位置对不对。
        let out = run(&[
            &d("x"),
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":20,\"prompt_tokens_details\":{\"cached_tokens\":60}}}\n\n",
            "data: [DONE]\n\n",
        ]);
        let ev = events(&out);
        let md = ev.iter().find(|(k, _)| k == "message_delta").unwrap();
        assert_eq!(md.1["usage"]["input_tokens"], 40, "缓存命中要减掉");
        assert_eq!(md.1["usage"]["cache_read_input_tokens"], 60);
        assert_eq!(md.1["usage"]["output_tokens"], 20);
    }
}
