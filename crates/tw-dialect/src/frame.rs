//! SSE 的拆帧和写帧。
//!
//! 四种格式的流都是 SSE（Gemini 要带 `alt=sse`），差别只在帧里装什么。
//! **帧可能被切在任意一个字节上**，拆帧器要把没收齐的那部分留到下一块。

use serde_json::Value;

/// 一帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub event: Option<String>,
    /// 多行 `data:` 按 SSE 规范用换行连起来
    pub data: String,
}

#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Frame> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((end, sep)) = frame_end(&self.buf) {
            let raw: Vec<u8> = self.buf.drain(..end + sep).collect();
            if let Some(f) = parse(&raw[..end]) {
                out.push(f);
            }
        }
        out
    }

    /// 流结束时剩下的最后一帧。有的上游最后一帧后面不带空行
    pub fn flush(&mut self) -> Vec<Frame> {
        let raw = std::mem::take(&mut self.buf);
        parse(&raw).into_iter().collect()
    }
}

/// 找第一个帧结尾：返回帧内容的长度和分隔符的长度。
fn frame_end(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        match (buf[i], buf[i + 1]) {
            (b'\n', b'\n') => return Some((i, 2)),
            (b'\r', b'\n') if buf.get(i + 2..i + 4) == Some(b"\r\n") => return Some((i, 4)),
            _ => {}
        }
        i += 1;
    }
    None
}

fn parse(raw: &[u8]) -> Option<Frame> {
    let text = String::from_utf8_lossy(raw);
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => event = Some(value.to_string()),
            "data" => data.push(value),
            _ => {}
        }
    }
    if event.is_none() && data.is_empty() {
        return None;
    }
    Some(Frame {
        event,
        data: data.join("\n"),
    })
}

/// `event: 名字` + `data: JSON`
pub fn named(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// 只有 `data: JSON`
pub fn data(data: &Value) -> String {
    format!("data: {data}\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_cut_at_every_byte_still_comes_out_once() {
        let whole = "event: message_start\ndata: {\"a\":\"你好\"}\n\n";
        for cut in 1..whole.len() {
            let mut d = Decoder::default();
            let mut frames = d.feed(&whole.as_bytes()[..cut]);
            frames.extend(d.feed(&whole.as_bytes()[cut..]));
            assert_eq!(frames.len(), 1, "在第 {cut} 字节切开");
            assert_eq!(frames[0].event.as_deref(), Some("message_start"));
            assert_eq!(frames[0].data, "{\"a\":\"你好\"}");
        }
    }

    #[test]
    fn crlf_frames_and_comments_are_understood() {
        let mut d = Decoder::default();
        let frames = d.feed(b": keep-alive\r\n\r\ndata: {\"x\":1}\r\n\r\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "{\"x\":1}");
    }

    #[test]
    fn a_last_frame_without_a_blank_line_is_flushed_at_the_end() {
        let mut d = Decoder::default();
        assert!(d.feed(b"data: [DONE]").is_empty());
        assert_eq!(d.flush()[0].data, "[DONE]");
    }

    #[test]
    fn multi_line_data_joins_with_newlines() {
        let mut d = Decoder::default();
        let f = d.feed(b"data: a\ndata: b\n\n");
        assert_eq!(f[0].data, "a\nb");
    }
}
