//! AWS eventstream → SSE。
//!
//! Bedrock 的 ConverseStream 不是 SSE，是 AWS eventstream 的二进制帧：
//! 每帧一段前导（总长、头长、前导 CRC）、一串带类型的头、载荷、整帧 CRC。
//! 别的格式的流都是 SSE，下游的一切 —— 格式转换、用量嗅探、流的组装 —— 都按 SSE
//! 读。所以**在字节进门的地方就把它转成 SSE**，后面不需要知道 Bedrock 有什么不同。
//!
//! 转出来的形状是 `tw_dialect::bedrock::stream` 读的那种：`:event-type` 头进
//! `event:`，载荷 JSON 进 `data:`。
//!
//! **帧可能被切在任意一个字节上**，没收齐的那部分留到下一块。CRC 由
//! `aws-smithy-eventstream` 校验 —— 校验不过就是坏了，不要猜着往下读。

use std::fmt;

use aws_smithy_eventstream::frame::{DecodedFrame, MessageFrameDecoder};
use bytes::{Buf, BytesMut};

/// ConverseStream 回答的 `content-type`。
pub const CONTENT_TYPE: &str = "application/vnd.amazon.eventstream";

/// 流里出的错。
///
/// **这一层不认识调用方的错误体系**，理由同 [`crate::SignError`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamError {
    /// 帧坏了：长度不对、CRC 不对
    Malformed(String),
    /// 上游在流里报了错（`:message-type` 是 `exception` 或 `error`），比如半路被
    /// 限流。`kind` 是 AWS 的异常名，比如 `throttlingException`
    Upstream { kind: String, message: String },
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamError::Malformed(m) => write!(f, "malformed AWS eventstream frame: {m}"),
            StreamError::Upstream { kind, message } => write!(f, "{kind}: {message}"),
        }
    }
}

impl std::error::Error for StreamError {}

/// 一边收一边转。
#[derive(Debug, Default)]
pub struct Transcoder {
    buf: BytesMut,
    decoder: MessageFrameDecoder,
}

impl Transcoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一块，返回这一块收齐了的所有帧，已经写成 SSE。
    ///
    /// 出错之后这个转码器就不该再用了：帧的边界已经对不上。
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<u8>, StreamError> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            let before = self.buf.remaining();
            let frame = self
                .decoder
                .decode_frame(&mut self.buf)
                .map_err(|e| StreamError::Malformed(e.to_string()))?;
            match frame {
                DecodedFrame::Complete(message) => {
                    let header = |name: &str| {
                        message
                            .headers()
                            .iter()
                            .find(|h| h.name().as_str() == name)
                            .and_then(|h| h.value().as_string().ok())
                            .map(|s| s.as_str().to_string())
                    };
                    let payload = String::from_utf8_lossy(message.payload());
                    match header(":message-type").as_deref() {
                        Some("event") | None => {
                            let event = header(":event-type").unwrap_or_default();
                            write_frame(&mut out, &event, &payload);
                        }
                        Some("exception") => {
                            return Err(StreamError::Upstream {
                                kind: header(":exception-type").unwrap_or_default(),
                                message: crate::error::message_in(payload.as_bytes())
                                    .unwrap_or_else(|| payload.to_string()),
                            });
                        }
                        Some(other) => {
                            return Err(StreamError::Upstream {
                                kind: header(":error-code").unwrap_or_else(|| other.to_string()),
                                message: header(":error-message").unwrap_or_default(),
                            });
                        }
                    }
                }
                // 前导读走了但整帧没收齐时，缓冲区也会变短 —— 只有什么都没读走，
                // 才是真的要等下一块
                DecodedFrame::Incomplete if self.buf.remaining() == before => return Ok(out),
                DecodedFrame::Incomplete => {}
            }
        }
    }
}

/// 写一帧 SSE。载荷按行拆成多个 `data:` —— SSE 读回来会用换行拼上，多行 JSON
/// 原样还原。
fn write_frame(out: &mut Vec<u8>, event: &str, payload: &str) {
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(event.as_bytes());
    out.push(b'\n');
    for line in payload.split('\n') {
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    out.push(b'\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_smithy_eventstream::frame::write_message_to;
    use aws_smithy_types::event_stream::{Header, HeaderValue, Message};

    fn event(kind: &str, payload: &str) -> Vec<u8> {
        let m = Message::new(payload.as_bytes().to_vec())
            .add_header(Header::new(
                ":message-type",
                HeaderValue::String("event".into()),
            ))
            .add_header(Header::new(
                ":event-type",
                HeaderValue::String(kind.to_string().into()),
            ))
            .add_header(Header::new(
                ":content-type",
                HeaderValue::String("application/json".into()),
            ));
        let mut out = Vec::new();
        write_message_to(&m, &mut out).unwrap();
        out
    }

    fn exception(kind: &str, payload: &str) -> Vec<u8> {
        let m = Message::new(payload.as_bytes().to_vec())
            .add_header(Header::new(
                ":message-type",
                HeaderValue::String("exception".into()),
            ))
            .add_header(Header::new(
                ":exception-type",
                HeaderValue::String(kind.to_string().into()),
            ));
        let mut out = Vec::new();
        write_message_to(&m, &mut out).unwrap();
        out
    }

    #[test]
    fn each_frame_becomes_one_sse_event() {
        let mut wire = event("messageStart", r#"{"role":"assistant"}"#);
        wire.extend(event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"晴"}}"#,
        ));
        let sse = Transcoder::new().feed(&wire).unwrap();
        assert_eq!(
            String::from_utf8(sse).unwrap(),
            "event: messageStart\ndata: {\"role\":\"assistant\"}\n\n\
             event: contentBlockDelta\ndata: {\"contentBlockIndex\":0,\"delta\":{\"text\":\"晴\"}}\n\n"
        );
    }

    #[test]
    fn a_frame_cut_on_any_byte_comes_out_whole() {
        let mut wire = event("messageStart", r#"{"role":"assistant"}"#);
        wire.extend(event(
            "metadata",
            r#"{"usage":{"inputTokens":3,"outputTokens":5}}"#,
        ));
        let whole = Transcoder::new().feed(&wire).unwrap();
        // 一个字节一个字节地喂：前导、头、载荷、CRC 都会被切开
        let mut t = Transcoder::new();
        let mut pieced = Vec::new();
        for b in &wire {
            pieced.extend(t.feed(std::slice::from_ref(b)).unwrap());
        }
        assert_eq!(pieced, whole);
    }

    #[test]
    fn an_exception_in_the_stream_is_an_error_not_an_event() {
        let mut wire = event("messageStart", r#"{"role":"assistant"}"#);
        wire.extend(exception(
            "throttlingException",
            r#"{"message":"Too many requests"}"#,
        ));
        assert_eq!(
            Transcoder::new().feed(&wire),
            Err(StreamError::Upstream {
                kind: "throttlingException".into(),
                message: "Too many requests".into(),
            })
        );
    }

    #[test]
    fn an_exception_spelled_with_a_capital_message_keeps_its_text() {
        let wire = exception(
            "validationException",
            r#"{"Message":"Malformed input request"}"#,
        );
        assert_eq!(
            Transcoder::new().feed(&wire),
            Err(StreamError::Upstream {
                kind: "validationException".into(),
                message: "Malformed input request".into(),
            })
        );
    }

    #[test]
    fn a_corrupted_frame_is_refused() {
        let mut wire = event("messageStart", r#"{"role":"assistant"}"#);
        let n = wire.len();
        wire[n - 6] ^= 0xff; // 改一个载荷字节，整帧 CRC 就对不上
        assert!(matches!(
            Transcoder::new().feed(&wire),
            Err(StreamError::Malformed(_))
        ));
    }
}
