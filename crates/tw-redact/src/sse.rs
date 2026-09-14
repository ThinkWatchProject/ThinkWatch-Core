//! 在 SSE 流里把占位符换回来。
//!
//! # 为什么不能在原始字节上做
//!
//! 第一版是在字节流上找占位符的，写完一测就发现它**根本不可能对**：
//!
//! ```text
//! data: {"delta":{"text":"<<"}}
//! data: {"delta":{"text":"TW_SEC"}}
//! data: {"delta":{"text":"RET_1>>"}}
//! ```
//!
//! 占位符在**解码后的正文里**是连续的，在线路上不是 —— 中间隔着
//! `"}}\n\ndata: {"delta":{"text":"`。模型按 token 吐字，一个
//! `<<TW_SECRET_1>>` 会被切成五到八段，这不是边角情况，是常态。
//!
//! 企业版的 `PiiStreamRestorer` 之所以没这个问题，是因为它拿到的本来就是
//! 解码后的 content —— 那个网关无论如何都要做协议转换。我们是直通的，
//! 所以这一层得自己拆帧。
//!
//! # 做法
//!
//! 拆出每一帧，找到装正文的那个字段，解码 → 喂给
//! [`crate::stream::Restorer`] → 把它吐出来的那段写回去。扣住的尾巴跨帧
//! 攒着，下一帧的正文接在它后面。
//!
//! **不认识的帧原样转发。**心跳、`ping`、我们没见过的事件类型 —— 这条
//! 路上任何「看不懂就重写一下」的行为都是在拿用户的流冒险。

use serde_json::Value;

use crate::redact::Ledger;
use crate::stream::Restorer;

/// 正文可能挂在哪几个字段上。**按方言列，不是猜。**
///
/// `partial_json` 那一条尤其要紧：它是工具调用参数的分片。用户说
/// 「把我的 key 写进 .env」时，占位符会从这里流过去 —— 不还原的话，
/// 客户端真的会把 `<<TW_SECRET_1>>` 写进文件。
const TEXT_PATHS: &[&[&str]] = &[
    // Anthropic
    &["delta", "text"],
    &["delta", "partial_json"],
    &["content_block", "text"],
    // OpenAI chat
    &["choices", "0", "delta", "content"],
    // Gemini
    &["candidates", "0", "content", "parts", "0", "text"],
];

fn get_mut<'a>(v: &'a mut Value, path: &[&str]) -> Option<&'a mut Value> {
    let mut cur = v;
    for k in path {
        cur = match cur {
            Value::Object(m) => m.get_mut(*k)?,
            Value::Array(a) => a.get_mut(k.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// 一条 SSE 流上的还原器。
pub struct SseRestorer {
    inner: Restorer,
    /// 还没收齐的那一帧
    partial: Vec<u8>,
    /// 最后见过的 `index`，补帧时要带上
    last_index: Option<u64>,
    /// 补出来的那一帧是文本还是工具参数
    last_delta_type: Option<String>,
}

impl SseRestorer {
    pub fn new(ledger: &Ledger) -> Self {
        Self {
            inner: Restorer::new(ledger),
            partial: Vec::new(),
            last_index: None,
            last_delta_type: None,
        }
    }
    pub fn is_noop(&self) -> bool {
        self.inner.is_noop()
    }

    pub fn process(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.is_noop() {
            return chunk.to_vec();
        }
        self.partial.extend_from_slice(chunk);
        let mut out = Vec::new();
        // 帧之间用空行分隔。**收不齐就等下一块** —— 半帧转发出去，
        // 客户端那边会当成一帧解析失败
        while let Some(end) = find_frame_end(&self.partial) {
            let frame: Vec<u8> = self.partial.drain(..end).collect();
            out.extend_from_slice(&self.rewrite(&frame));
        }
        out
    }

    /// 流结束了。
    ///
    /// **扣住的尾巴要补一帧发出去。**丢掉它就是丢掉模型说过的字，而
    /// 那比多一帧难看得多。
    pub fn flush(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.partial.is_empty() {
            let frame: Vec<u8> = std::mem::take(&mut self.partial);
            out.extend_from_slice(&self.rewrite(&frame));
        }
        let tail = self.inner.flush();
        if !tail.is_empty() {
            out.extend_from_slice(self.synth(&tail).as_bytes());
        }
        out
    }

    /// 补一帧，把扣住的尾巴装进去。
    fn synth(&self, text: &str) -> String {
        let index = self.last_index.unwrap_or(0);
        let dt = self.last_delta_type.as_deref().unwrap_or("text_delta");
        let field = if dt == "input_json_delta" {
            "partial_json"
        } else {
            "text"
        };
        format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":{index},\"delta\":{{\"type\":\"{dt}\",\"{field}\":{}}}}}\n\n",
            serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())
        )
    }

    /// 改写一帧。**看不懂就原样转发。**
    fn rewrite(&mut self, frame: &[u8]) -> Vec<u8> {
        let Ok(text) = std::str::from_utf8(frame) else {
            return frame.to_vec();
        };
        let mut out = String::with_capacity(text.len());
        let mut touched = false;
        for line in text.split_inclusive('\n') {
            let Some(payload) = line.strip_prefix("data: ") else {
                out.push_str(line);
                continue;
            };
            let trimmed = payload.trim_end_matches(['\n', '\r']);
            let Ok(mut v) = serde_json::from_str::<Value>(trimmed) else {
                out.push_str(line);
                continue;
            };
            // 记住 index 和 delta 类型，补帧时要用
            if let Some(i) = v.get("index").and_then(|x| x.as_u64()) {
                self.last_index = Some(i);
            }
            if let Some(t) = v
                .get("delta")
                .and_then(|d| d.get("type"))
                .and_then(|x| x.as_str())
            {
                self.last_delta_type = Some(t.to_string());
            }
            let mut hit = false;
            for path in TEXT_PATHS {
                let Some(slot) = get_mut(&mut v, path) else {
                    continue;
                };
                let Some(s) = slot.as_str() else { continue };
                *slot = Value::String(self.inner.process(s));
                hit = true;
                break;
            }
            if !hit {
                out.push_str(line);
                continue;
            }
            touched = true;
            out.push_str("data: ");
            out.push_str(&serde_json::to_string(&v).unwrap_or_else(|_| trimmed.to_string()));
            // 把 trim 掉的换行原样接回去
            out.push_str(&payload[trimmed.len()..]);
        }
        if touched {
            out.into_bytes()
        } else {
            frame.to_vec()
        }
    }
}

/// 一帧到哪儿结束（含分隔的空行）。
fn find_frame_end(buf: &[u8]) -> Option<usize> {
    // `\n\n` 或者 `\r\n\r\n`
    buf.windows(2)
        .position(|w| w == b"\n\n")
        .map(|i| i + 2)
        .or_else(|| buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4))
}

/// 一条响应流上的还原器，按内容类型分派。
///
/// **SSE 和非流式是两件不同的事**：前者的占位符散落在几十帧里，后者
/// 整个躺在一份 JSON 里。合成一个「通用」的实现只会让两边都做不对 ——
/// 第一版就是那么写的，SSE 那半从来没还原成功过。
pub enum Body {
    Sse(SseRestorer),
    Whole(crate::stream::ByteRestorer),
}

impl Body {
    pub fn new(ledger: &Ledger, is_sse: bool) -> Self {
        if is_sse {
            Body::Sse(SseRestorer::new(ledger))
        } else {
            Body::Whole(crate::stream::ByteRestorer::new(ledger))
        }
    }
    pub fn is_noop(&self) -> bool {
        match self {
            Body::Sse(r) => r.is_noop(),
            Body::Whole(r) => r.is_noop(),
        }
    }
    pub fn process(&mut self, chunk: &[u8]) -> Vec<u8> {
        match self {
            Body::Sse(r) => r.process(chunk),
            Body::Whole(r) => r.process(chunk),
        }
    }
    pub fn flush(&mut self) -> Vec<u8> {
        match self {
            Body::Sse(r) => r.flush(),
            Body::Whole(r) => r.flush(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redact::redact;
    use crate::rules::Kind;

    const KEY: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn ledger() -> Ledger {
        redact(&format!("k={KEY}"), &[Kind::ApiKeys]).ledger
    }

    fn delta(text: &str) -> String {
        format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{}}}}}\n\n",
            serde_json::to_string(text).unwrap()
        )
    }

    /// 把帧喂进去，返回客户端最终看到的正文（把所有 text 拼起来）。
    fn text_out(frames: &[String]) -> String {
        let l = ledger();
        let mut r = SseRestorer::new(&l);
        let mut raw = Vec::new();
        for f in frames {
            raw.extend_from_slice(&r.process(f.as_bytes()));
        }
        raw.extend_from_slice(&r.flush());
        let s = String::from_utf8(raw).unwrap();
        s.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|d| serde_json::from_str::<Value>(d).ok())
            .filter_map(|v| {
                v["delta"]["text"]
                    .as_str()
                    .or_else(|| v["delta"]["partial_json"].as_str())
                    .map(|s| s.to_string())
            })
            .collect()
    }

    #[test]
    fn a_placeholder_spread_over_one_character_per_frame_comes_back_whole() {
        // **这是第一版当场失败的那个用例。**模型按 token 吐字，一个
        // `<<TW_SECRET_1>>` 会被切成五到八段 —— 不是边角情况，是常态。
        let frames: Vec<String> = "你的 key 是 <<TW_SECRET_1>> 对吗"
            .chars()
            .map(|c| delta(&c.to_string()))
            .collect();
        assert_eq!(text_out(&frames), format!("你的 key 是 {KEY} 对吗"));
    }

    #[test]
    fn a_realistic_token_split_comes_back_whole() {
        let frames: Vec<String> = [
            "你的 key 是 <<",
            "TW_",
            "SECRET",
            "_1",
            ">>",
            "，看起来没问题",
        ]
        .iter()
        .map(|s| delta(s))
        .collect();
        assert_eq!(
            text_out(&frames),
            format!("你的 key 是 {KEY}，看起来没问题")
        );
    }

    #[test]
    fn a_tool_call_argument_is_restored_too() {
        // **用户说「把我的 key 写进 .env」的时候，占位符从这里流过去。**
        // 不还原的话，客户端真的会把 `<<TW_SECRET_1>>` 写进文件。
        let f = |s: &str| {
            format!(
                "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{}}}}}\n\n",
                serde_json::to_string(s).unwrap()
            )
        };
        let frames: Vec<String> = [r#"{"content":"KEY=<<TW"#, "_SECRET_1", r#">>"}"#]
            .iter()
            .map(|s| f(s))
            .collect();
        assert_eq!(text_out(&frames), format!(r#"{{"content":"KEY={KEY}"}}"#));
    }

    #[test]
    fn frames_that_arrive_in_pieces_are_not_forwarded_half_written() {
        // 半帧转发出去，客户端那边会当成一帧解析失败。
        let l = ledger();
        let mut r = SseRestorer::new(&l);
        let whole = delta("<<TW_SECRET_1>>");
        let bytes = whole.as_bytes();
        let mid = bytes.len() / 2;
        let first = r.process(&bytes[..mid]);
        assert!(
            first.is_empty(),
            "半帧就发出去了：{:?}",
            String::from_utf8_lossy(&first)
        );
        let rest = r.process(&bytes[mid..]);
        assert!(String::from_utf8_lossy(&rest).contains(KEY));
    }

    #[test]
    fn frames_we_do_not_understand_pass_through_byte_for_byte() {
        // 心跳、ping、没见过的事件类型 —— 「看不懂就重写一下」是在拿
        // 用户的流冒险。
        let l = ledger();
        let mut r = SseRestorer::new(&l);
        for raw in [
            ": 心跳\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\"}}\n\n",
            "data: [DONE]\n\n",
        ] {
            assert_eq!(
                String::from_utf8(r.process(raw.as_bytes())).unwrap(),
                raw,
                "改写了不该改的帧"
            );
        }
    }

    #[test]
    fn a_stream_that_ends_mid_placeholder_still_delivers_the_text() {
        // 丢掉它就是丢掉模型说过的字。补一帧比丢字难看得多，但正确。
        let frames = vec![delta("结果是 <<TW_SECRET_")];
        assert_eq!(text_out(&frames), "结果是 <<TW_SECRET_");
    }

    #[test]
    fn ordinary_text_flows_without_being_held_back() {
        // 一帧进一帧出。没有占位符嫌疑的正文不该在我们这儿多待哪怕
        // 一帧 —— 那是用户能直接看到的卡顿。
        let l = ledger();
        let mut r = SseRestorer::new(&l);
        for s in ["第一段", "第二段", "std::cout << x"] {
            let out = String::from_utf8(r.process(delta(s).as_bytes())).unwrap();
            assert!(out.contains(s), "「{s}」被扣住了：{out}");
        }
    }

    #[test]
    fn a_stream_with_nothing_to_restore_is_copied_straight_through() {
        // 绝大多数请求走这条路，它不该为这个功能付任何代价。
        let mut r = SseRestorer::new(&Ledger::default());
        assert!(r.is_noop());
        let raw = delta("<<TW_SECRET_1>>");
        assert_eq!(String::from_utf8(r.process(raw.as_bytes())).unwrap(), raw);
    }

    #[test]
    fn the_openai_shape_works_too() {
        let f = |s: &str| {
            format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\n",
                serde_json::to_string(s).unwrap()
            )
        };
        let l = ledger();
        let mut r = SseRestorer::new(&l);
        let mut out = Vec::new();
        for s in ["key 是 <<TW", "_SECRET_1>>"] {
            out.extend_from_slice(&r.process(f(s).as_bytes()));
        }
        out.extend_from_slice(&r.flush());
        assert!(String::from_utf8(out).unwrap().contains(KEY));
    }
}
