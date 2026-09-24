//! 输出长度：模型这一次回答的正文不许超过多长。
//!
//! 为的是失控的回答 —— 一个在循环里打转、吐了几十万字还没停的模型，每个字都在
//! 计费，客户端那边也在一直渲染。**只数正文**（`text` 块）：思考、工具调用的参数
//! 不算，一次写大文件的工具调用是正常的。
//!
//! # 整包和流是两条路
//!
//! 整包（[`Limit::check_whole`]）：整份到手时一个字节都还没发出去，超了就整份
//! 不发，换成错误。
//!
//! 流（[`Meter`]）：边收边数，**按帧切** —— 超过的那一帧不发，之前的照发，由调用方
//! 按客户端的格式补一个错误收尾（`tw_dialect::convert::error_frame`，或者转换器的
//! `fail`）。切在帧上而不是字上：半帧 JSON 客户端解析不了，而一帧只有几个 token。
//!
//! 两条路数的都是**客户端将要收到的那一版**（转换过的就是转换之后的），所以按客户端
//! 的格式读。

use tw_dialect::convert::Reader;
use tw_dialect::ir::{Delta, Dialect, Event};

/// 按什么数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Unit {
    /// 字节。企业版一直按它数：一个中文字是三个
    Bytes,
    /// 字符（Unicode 标量）
    #[default]
    Chars,
}

/// 一个上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub max: usize,
    pub unit: Unit,
}

impl Limit {
    /// 一段正文有多长
    pub fn measure(&self, s: &str) -> usize {
        match self.unit {
            Unit::Bytes => s.len(),
            Unit::Chars => s.chars().count(),
        }
    }

    /// 一份整包响应（`client` 格式）的正文超了的话，它有多长。
    ///
    /// 读不出来（不是 JSON、是错误体）就当没超 —— 这一层只管模型的回答。
    pub fn check_whole(&self, body: &[u8], client: Dialect) -> Option<usize> {
        let n = self.measure(&assistant_text(body, client));
        (n > self.max).then_some(n)
    }
}

/// 一份整包响应里助手的正文，按 `client` 的格式读。
pub fn assistant_text(body: &[u8], client: Dialect) -> String {
    use tw_dialect::ir::Block;
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return String::new();
    };
    let r = match client {
        Dialect::Chat => tw_dialect::chat::decode_response(&v),
        Dialect::Anthropic => tw_dialect::anthropic::decode_response(&v),
        Dialect::Responses => tw_dialect::responses::decode_response(&v),
        Dialect::Gemini => tw_dialect::gemini::decode_response(&v),
        Dialect::Bedrock => tw_dialect::bedrock::decode_response(&v),
    };
    r.blocks
        .iter()
        .filter_map(|b| match b {
            Block::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

/// 流超了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trip {
    /// 数到超的那一刻正文有多长
    pub seen: usize,
    /// **这一块里前多少字节仍然该发出去**：超过的那一帧之前的完整帧。那一帧从上一块
    /// 就开始了的话是 0
    pub safe_prefix: usize,
}

/// 响应体怎么分帧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    Sse,
    /// Gemini 客户端不带 `alt=sse` 时：一个 JSON 数组，一个元素一帧
    JsonArray,
}

/// 一条流上的计数器。
pub struct Meter {
    limit: Limit,
    framing: Framing,
    reader: Reader,
    /// 没收齐的那一帧
    partial: Vec<u8>,
    seen: usize,
    tripped: bool,
}

impl Meter {
    /// SSE 流，`client` 是客户端收到的格式。
    pub fn sse(limit: Limit, client: Dialect) -> Self {
        Self {
            limit,
            framing: Framing::Sse,
            reader: Reader::new(client),
            partial: Vec::new(),
            seen: 0,
            tripped: false,
        }
    }

    /// Gemini 不带 `alt=sse` 的流：一个逐个元素下发的 JSON 数组。
    pub fn json_array(limit: Limit) -> Self {
        Self {
            framing: Framing::JsonArray,
            ..Self::sse(limit, Dialect::Gemini)
        }
    }

    /// 到目前为止数了多长
    pub fn seen(&self) -> usize {
        self.seen
    }

    /// 喂一块客户端将要收到的字节。**第一次超的那一块**返回 [`Trip`]，之后什么都
    /// 不报（照样在数）。
    ///
    /// **不改任何字节。**切不切由调用方按档位决定。
    pub fn feed(&mut self, chunk: &[u8]) -> Option<Trip> {
        let carried = self.partial.len();
        self.partial.extend_from_slice(chunk);
        let mut consumed = 0usize;
        let mut trip = None;
        while let Some(end) = self.frame_end() {
            let frame: Vec<u8> = self.partial.drain(..end).collect();
            // 这一帧在 `chunk` 里从哪儿开始（见工具调用审查里同一段的说明）
            let safe = consumed.saturating_sub(carried).min(chunk.len());
            consumed += end;
            self.seen += self.text_in(&frame);
            if !self.tripped && self.seen > self.limit.max {
                self.tripped = true;
                trip = Some(Trip {
                    seen: self.seen,
                    safe_prefix: safe,
                });
            }
        }
        trip
    }

    fn frame_end(&self) -> Option<usize> {
        match self.framing {
            Framing::Sse => tw_dialect::frame::frame_end(&self.partial).map(|(n, sep)| n + sep),
            Framing::JsonArray => crate::tools::wall::find_element_end(&self.partial),
        }
    }

    /// 一帧里的正文有多长
    fn text_in(&mut self, frame: &[u8]) -> usize {
        let events = match self.framing {
            Framing::Sse => self.reader.feed(frame),
            Framing::JsonArray => {
                let start = frame
                    .iter()
                    .position(|b| !(b.is_ascii_whitespace() || *b == b','))
                    .unwrap_or(frame.len());
                if frame.get(start) != Some(&b'{') {
                    return 0;
                }
                // 一个元素就是 SSE 里一帧的 `data`
                let mut sse = b"data: ".to_vec();
                sse.extend_from_slice(&frame[start..]);
                sse.extend_from_slice(b"\n\n");
                self.reader.feed(&sse)
            }
        };
        events
            .iter()
            .map(|e| match e {
                Event::Delta {
                    delta: Delta::Text(t),
                    ..
                } => self.limit.measure(t),
                _ => 0,
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(max: usize) -> Limit {
        Limit {
            max,
            unit: Unit::Chars,
        }
    }

    fn anthropic_delta(text: &str) -> String {
        format!(
            "event: content_block_delta\ndata: {}\n\n",
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}})
        )
    }

    fn chat_delta(text: &str) -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({"id":"c","object":"chat.completion.chunk","created":0,"model":"m",
                "choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]})
        )
    }

    #[test]
    fn a_whole_answer_within_the_limit_passes_and_one_over_it_does_not() {
        let body = serde_json::to_vec(&serde_json::json!({
            "id": "msg", "type": "message", "role": "assistant", "model": "m",
            "content": [{"type": "text", "text": "你好世界"}], "stop_reason": "end_turn"
        }))
        .unwrap();
        assert_eq!(chars(4).check_whole(&body, Dialect::Anthropic), None);
        assert_eq!(chars(3).check_whole(&body, Dialect::Anthropic), Some(4));
        // 按字节数，四个中文字是十二个
        let bytes = Limit {
            max: 11,
            unit: Unit::Bytes,
        };
        assert_eq!(bytes.check_whole(&body, Dialect::Anthropic), Some(12));
    }

    #[test]
    fn a_whole_answer_is_read_in_the_format_the_client_asked_for() {
        let body = serde_json::to_vec(&serde_json::json!({
            "id": "id", "object": "chat.completion", "created": 0, "model": "m",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "too long"},
                         "finish_reason": "stop"}]
        }))
        .unwrap();
        assert_eq!(chars(3).check_whole(&body, Dialect::Chat), Some(8));
        assert_eq!(chars(3).check_whole(b"not json", Dialect::Chat), None);
    }

    #[test]
    fn a_stream_trips_on_the_frame_that_crosses_the_limit_and_keeps_the_frames_before_it() {
        let mut m = Meter::sse(chars(5), Dialect::Anthropic);
        let first = anthropic_delta("abc");
        let second = anthropic_delta("defg");
        let chunk = format!("{first}{second}");
        let t = m.feed(chunk.as_bytes()).expect("tripped");
        assert_eq!(t.seen, 7);
        assert_eq!(t.safe_prefix, first.len(), "越界那一帧之前的要照发");
        // 之后不再报，但照样数
        assert!(m.feed(anthropic_delta("h").as_bytes()).is_none());
        assert_eq!(m.seen(), 8);
    }

    #[test]
    fn a_frame_split_across_chunks_is_counted_once_and_cut_from_its_start() {
        let whole = format!("{}{}", chat_delta("hello"), chat_delta(" world"));
        for cut in 1..whole.len() {
            let mut m = Meter::sse(chars(8), Dialect::Chat);
            let a = m.feed(&whole.as_bytes()[..cut]);
            let b = m.feed(&whole.as_bytes()[cut..]);
            let t = a.or(b).unwrap_or_else(|| panic!("在第 {cut} 字节切开没报"));
            assert_eq!(t.seen, 11, "在第 {cut} 字节切开");
            assert_eq!(m.seen(), 11);
        }
    }

    #[test]
    fn thinking_and_tool_arguments_do_not_count() {
        let mut m = Meter::sse(chars(1), Dialect::Anthropic);
        let thinking = format!(
            "event: content_block_delta\ndata: {}\n\n",
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"long thoughts"}})
        );
        let args = format!(
            "event: content_block_delta\ndata: {}\n\n",
            serde_json::json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a\"}"}})
        );
        assert!(m.feed(format!("{thinking}{args}").as_bytes()).is_none());
        assert_eq!(m.seen(), 0);
    }

    #[test]
    fn a_gemini_json_array_stream_is_counted_per_element() {
        let el = |t: &str| {
            serde_json::json!({"candidates":[{"content":{"role":"model","parts":[{"text":t}]}}]})
                .to_string()
        };
        let body = format!("[{},\n{}]", el("abcd"), el("efgh"));
        let mut m = Meter::json_array(chars(6));
        let t = m.feed(body.as_bytes()).expect("tripped");
        assert_eq!(t.seen, 8);
        assert_eq!(t.safe_prefix, 1 + el("abcd").len(), "第一个元素照发");
    }

    #[test]
    fn responses_and_gemini_sse_are_understood() {
        let r = format!(
            "event: response.output_text.delta\ndata: {}\n\n",
            serde_json::json!({"type":"response.output_text.delta","item_id":"i","output_index":0,"content_index":0,"delta":"abcdef"})
        );
        assert!(
            Meter::sse(chars(5), Dialect::Responses)
                .feed(r.as_bytes())
                .is_some()
        );
        let g = format!(
            "data: {}\n\n",
            serde_json::json!({"candidates":[{"content":{"role":"model","parts":[{"text":"abcdef"}]}}]})
        );
        assert!(
            Meter::sse(chars(5), Dialect::Gemini)
                .feed(g.as_bytes())
                .is_some()
        );
    }
}
