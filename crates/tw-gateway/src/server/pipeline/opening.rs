//! 回答的开头：**第一段内容到达之前，上游在流里报的错误照样换下一家；一直没有内容，也换。**
//!
//! 上游回了 200、流也开了，之后第一个事件却是错误 —— Anthropic 过载时的
//! `overloaded_error`、Codex 额度用完时的 `response.failed`、Bedrock 的
//! `throttlingException`。这时客户端还什么都没收到，换一家它完全无感；等到
//! 错误帧写给了客户端，就只能让它自己重试了。
//!
//! 所以响应头到手之后先不转发：把流读到第一段内容为止，读到的字节原样留着。
//! 期间是错误就换下一家；是内容，就把留着的字节和后面的流接在一起交出去 ——
//! 客户端收到的和直接转发一个字节都不差。什么是内容、什么是开头的例行事件和心跳，
//! 和无响应超时是同一个判据（见 [`crate::pulse`]）。
//!
//! 等到无响应超时那一刻（`failover.idle_timeout_secs`，从请求发出去算起）还没有内容是
//! [`Opening::Slow`]：调用方放弃这一家、换下一家。**最后一家不在这里等**（见 `hop`）：没有
//! 下一家可换，压着它只会让客户端晚一点看到同样的东西。开头压得太多（[`HOLD_LIMIT`]）也不
//! 再压，交出去由回程照常计时。
//!
//! 整包的回答（不是流）也一样等：响应头先到、正文迟迟不来的上游，等到点了换下一家（见
//! [`first_bytes`]）。

use bytes::Bytes;
use futures::StreamExt;

use crate::error::GatewayError;
use tw_dialect::ir::Dialect;

/// 开头最多压多少字节。前导事件都很小，攒到这么多还没有内容，就不是在等开头了
pub(super) const HOLD_LIMIT: usize = 1024 * 1024;

/// 流开头的结论。
pub(super) enum Opening {
    /// 内容来了（或者流结束了、开头压得太多了）：交给客户端。读过的字节已经接回去了
    Go(reqwest::Response),
    /// 等到无响应超时那一刻还没有内容。**丢掉 `response` 就断开了和上游的连接**，上游不再
    /// 接着生成。`usage` 是开头里上游报了的用量（Anthropic 的 `message_start` 带着输入），
    /// 没报是 None
    Slow {
        response: reqwest::Response,
        usage: Option<tw_dialect::usage::Usage>,
    },
    /// 第一段内容之前上游报了错。`status` 是这个错误对应的状态码，`body` 是
    /// 上游的原话，交给 [`crate::failure::classify`] 判断换不换。
    ///
    /// **读过的字节也还在 `response` 里**：判断下来是请求本身的问题（换一家也一样
    /// 被拒）时，照常把它交给客户端
    Failed {
        status: u16,
        headers: http::HeaderMap,
        body: Bytes,
        kind: String,
        message: String,
        response: reqwest::Response,
    },
    /// 流在第一段内容之前断了
    Broken(GatewayError),
}

/// 读到第一段内容为止，最多等到 `deadline`（无响应超时的那一刻）。`dialect` 是上游说的
/// 格式，`eventstream` 表示流是 Bedrock 的二进制帧。
pub(super) async fn watch(
    r: reqwest::Response,
    dialect: Dialect,
    eventstream: bool,
    deadline: tokio::time::Instant,
) -> Opening {
    let status = r.status();
    let headers = r.headers().clone();
    let mut stream = r.bytes_stream();
    let mut held: Vec<Bytes> = Vec::new();
    let mut size = 0usize;
    let mut sse = Sse::default();
    let mut unframe = eventstream.then(tw_bedrock::eventstream::Transcoder::new);
    // 开头里上游报的用量：放弃这一家时，它可能已经按这些收了钱
    let mut usage = tw_dialect::usage::Sniffer::new();
    let mut slow = false;

    let verdict = loop {
        let next = match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(next) => next,
            Err(_) => {
                slow = true;
                break Judge::Content;
            }
        };
        let chunk = match next {
            None => break Judge::Content,
            Some(Ok(c)) => c,
            Some(Err(e)) => {
                return Opening::Broken(crate::forward::map_reqwest_error(e));
            }
        };
        size += chunk.len();
        held.push(chunk.clone());
        let text = match unframe.as_mut() {
            None => chunk.to_vec(),
            Some(t) => match t.feed(&chunk) {
                Ok(sse) => sse,
                Err(tw_bedrock::eventstream::StreamError::Upstream { kind, message }) => {
                    break bedrock_exception(kind, message);
                }
                // 帧坏了：交给转发那一边照常报
                Err(_) => break Judge::Content,
            },
        };
        usage.feed(&text);
        let judged = sse
            .feed(&text)
            .into_iter()
            .map(|(event, data)| judge(dialect, event.as_deref(), &data))
            .find(|j| !matches!(j, Judge::Preamble));
        if let Some(j) = judged {
            break j;
        }
        if size >= HOLD_LIMIT {
            break Judge::Content;
        }
    };

    // 读过的字节接回流的前面，交出去的和上游发的一个字节都不差
    let replay = futures::stream::iter(held.into_iter().map(Ok::<_, reqwest::Error>));
    let mut resp = http::Response::new(reqwest::Body::wrap_stream(replay.chain(stream)));
    *resp.status_mut() = status;
    *resp.headers_mut() = headers.clone();
    let response = reqwest::Response::from(resp);
    match verdict {
        Judge::Error {
            status,
            body,
            kind,
            message,
        } => Opening::Failed {
            status,
            headers,
            body,
            kind,
            message,
            response,
        },
        Judge::Content | Judge::Preamble if slow => Opening::Slow {
            response,
            usage: usage.finish(),
        },
        Judge::Content | Judge::Preamble => Opening::Go(response),
    }
}

/// 整包的回答（不是流）：等到正文的第一个不是空白的字节，最多等到 `deadline`（无响应超时
/// 的那一刻）。到了是 [`Opening::Go`]，读过的字节接回去；到点了还没有是 [`Opening::Slow`]。
///
/// 只发空格保活的中转站，空格不算（见 [`crate::pulse`]）。整包的回答不会在正文里报一个
/// 「开头的错误」，所以这里没有 [`Opening::Failed`]。
pub(super) async fn first_bytes(r: reqwest::Response, deadline: tokio::time::Instant) -> Opening {
    let status = r.status();
    let headers = r.headers().clone();
    let mut stream = r.bytes_stream();
    let mut held: Vec<Bytes> = Vec::new();
    let mut size = 0usize;
    let mut pulse = crate::pulse::Pulse::new(None);
    let slow = loop {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Err(_) => break true,
            Ok(None) => break false,
            Ok(Some(Err(e))) => return Opening::Broken(crate::forward::map_reqwest_error(e)),
            Ok(Some(Ok(chunk))) => {
                size += chunk.len();
                let said = pulse.feed(&chunk);
                held.push(chunk);
                if said || size >= HOLD_LIMIT {
                    break false;
                }
            }
        }
    };
    let replay = futures::stream::iter(held.into_iter().map(Ok::<_, reqwest::Error>));
    let mut resp = http::Response::new(reqwest::Body::wrap_stream(replay.chain(stream)));
    *resp.status_mut() = status;
    *resp.headers_mut() = headers;
    let response = reqwest::Response::from(resp);
    if slow {
        Opening::Slow {
            response,
            usage: None,
        }
    } else {
        Opening::Go(response)
    }
}

/// 一个事件是什么。
#[derive(Debug, PartialEq)]
enum Judge {
    /// 开头的例行事件（`message_start`、`response.created`、心跳）：接着等
    Preamble,
    /// 内容，或者认不出来的东西：**认不出来的一律当内容** —— 放行最多是没能
    /// 换一家，拦错了却会把好好的回答扔掉
    Content,
    Error {
        status: u16,
        body: Bytes,
        kind: String,
        message: String,
    },
}

/// 看一个 SSE 事件：上游报的错，内容，还是开头的例行事件和心跳（后两样的判据在
/// [`crate::pulse::advances`]，和无响应超时同一个）。
fn judge(dialect: Dialect, event: Option<&str>, data: &str) -> Judge {
    let data = data.trim();
    if data.is_empty() {
        return Judge::Preamble;
    }
    if data == "[DONE]" {
        return Judge::Content;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
        return Judge::Content;
    };
    let ty = event
        .map(str::to_string)
        .or_else(|| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
        .unwrap_or_default();
    if let Some(e) = error_in(dialect, &ty, &v, data) {
        return e;
    }
    if crate::pulse::advances(dialect, &ty, &v) {
        Judge::Content
    } else {
        Judge::Preamble
    }
}

/// 这个事件是不是上游报的错：是的话，那个错误对应的状态码和上游的原话
fn error_in(dialect: Dialect, ty: &str, v: &serde_json::Value, data: &str) -> Option<Judge> {
    match dialect {
        Dialect::Anthropic => (ty == "error").then(|| {
            let e = v.get("error").unwrap_or(v);
            let kind = text_of(e, "type");
            error(anthropic_status(&kind), data, kind, text_of(e, "message"))
        }),
        Dialect::Responses => match ty {
            "response.failed" => {
                let e = v.pointer("/response/error").unwrap_or(v);
                let kind = text_of(e, "code");
                Some(error(
                    openai_status(&kind),
                    data,
                    kind,
                    text_of(e, "message"),
                ))
            }
            "error" => {
                let e = v.get("error").unwrap_or(v);
                let kind = Some(text_of(e, "code"))
                    .filter(|c| !c.is_empty())
                    .unwrap_or_else(|| text_of(e, "type"));
                Some(error(
                    openai_status(&kind),
                    data,
                    kind,
                    text_of(e, "message"),
                ))
            }
            _ => None,
        },
        Dialect::Chat => v.get("error").map(|e| {
            let kind = Some(text_of(e, "code"))
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| text_of(e, "type"));
            let status = e
                .get("code")
                .and_then(|c| c.as_u64())
                .and_then(|c| u16::try_from(c).ok())
                .filter(|c| (400..600).contains(c))
                .unwrap_or_else(|| openai_status(&kind));
            error(status, data, kind, text_of(e, "message"))
        }),
        Dialect::Gemini => v.get("error").map(|e| {
            let status = e
                .get("code")
                .and_then(|c| c.as_u64())
                .and_then(|c| u16::try_from(c).ok())
                .unwrap_or(500);
            error(status, data, text_of(e, "status"), text_of(e, "message"))
        }),
        Dialect::Bedrock => None,
    }
}

/// 流里的一个事件（`data`）是不是上游报的错：是的话，那个错误对应的状态码和上游的原话，交给
/// [`crate::failure::classify`]。**和开头报错是同一个判据**（[`judge`]）：WebSocket 上 Responses
/// 的一轮在第一段内容之前收尾时按它给这一家记成败（见 `hop::stream_fault`）
pub(super) fn stream_error(dialect: Dialect, data: &str) -> Option<(u16, Bytes)> {
    match judge(dialect, None, data) {
        Judge::Error { status, body, .. } => Some((status, body)),
        Judge::Preamble | Judge::Content => None,
    }
}

fn error(status: u16, data: &str, kind: String, message: String) -> Judge {
    Judge::Error {
        status,
        body: Bytes::copy_from_slice(data.as_bytes()),
        kind,
        message,
    }
}

fn text_of(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Anthropic 错误类型对应的状态码（它的文档里一一对应）
fn anthropic_status(kind: &str) -> u16 {
    match kind {
        "invalid_request_error" => 400,
        "authentication_error" => 401,
        "billing_error" => 402,
        "permission_error" => 403,
        "not_found_error" => 404,
        "request_too_large" => 413,
        "rate_limit_error" => 429,
        "timeout_error" => 504,
        "overloaded_error" => 529,
        _ => 500,
    }
}

/// OpenAI 在流里报的错误码对应的状态码
fn openai_status(code: &str) -> u16 {
    match code {
        "rate_limit_exceeded" | "insufficient_quota" | "usage_limit_reached" => 429,
        "invalid_prompt" | "context_length_exceeded" | "invalid_request_error" => 400,
        "server_is_overloaded" | "slow_down" => 503,
        _ => 500,
    }
}

fn bedrock_exception(kind: String, message: String) -> Judge {
    let status = match kind.as_str() {
        "throttlingException" => 429,
        "serviceUnavailableException" => 503,
        "validationException" => 400,
        "accessDeniedException" => 403,
        "modelTimeoutException" => 504,
        _ => 500,
    };
    let body = serde_json::json!({ "message": message }).to_string();
    Judge::Error {
        status,
        body: Bytes::from(body),
        kind,
        message,
    }
}

/// 从字节里拆 SSE 事件：`(event 名, data)`。一个事件没收齐时留着等下一块。
#[derive(Default)]
struct Sse {
    buf: String,
}

impl Sse {
    fn feed(&mut self, bytes: &[u8]) -> Vec<(Option<String>, String)> {
        self.buf.push_str(&String::from_utf8_lossy(bytes));
        let mut out = Vec::new();
        loop {
            let normalized = self.buf.replace("\r\n", "\n");
            let Some(end) = normalized.find("\n\n") else {
                self.buf = normalized;
                break;
            };
            let block = normalized[..end].to_string();
            self.buf = normalized[end + 2..].to_string();
            let mut event = None;
            let mut data = Vec::new();
            for line in block.lines() {
                if let Some(v) = line.strip_prefix("event:") {
                    event = Some(v.trim().to_string());
                } else if let Some(v) = line.strip_prefix("data:") {
                    data.push(v.strip_prefix(' ').unwrap_or(v).to_string());
                }
            }
            // 只有注释（`: keep-alive`）的块什么都不是
            if event.is_some() || !data.is_empty() {
                out.push((event, data.join("\n")));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn events(text: &str) -> Vec<(Option<String>, String)> {
        Sse::default().feed(text.as_bytes())
    }

    fn first_verdict(dialect: Dialect, text: &str) -> Option<Judge> {
        events(text)
            .into_iter()
            .map(|(e, d)| judge(dialect, e.as_deref(), &d))
            .find(|j| !matches!(j, Judge::Preamble))
    }

    #[test]
    fn events_split_across_chunks_are_put_back_together() {
        let mut sse = Sse::default();
        assert!(sse.feed(b"event: ping\ndata: {\"ty").is_empty());
        let got = sse.feed(b"pe\":\"ping\"}\r\n\r\n: keep-alive\n\n");
        assert_eq!(
            got,
            vec![(Some("ping".into()), r#"{"type":"ping"}"#.into())]
        );
    }

    #[test]
    fn anthropic_overloaded_before_content_is_a_529() {
        let text = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\n\
                    event: ping\ndata: {\"type\":\"ping\"}\n\n\
                    event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
        match first_verdict(Dialect::Anthropic, text) {
            Some(Judge::Error {
                status,
                kind,
                message,
                ..
            }) => {
                assert_eq!(status, 529);
                assert_eq!(kind, "overloaded_error");
                assert_eq!(message, "Overloaded");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn anthropic_content_ends_the_wait() {
        let text = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n\
                    event: content_block_start\ndata: {\"type\":\"content_block_start\"}\n\n";
        assert_eq!(
            first_verdict(Dialect::Anthropic, text),
            Some(Judge::Content)
        );
    }

    #[test]
    fn codex_usage_limit_in_the_stream_is_a_429() {
        let text = "event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
                    event: codex.rate_limits\ndata: {\"type\":\"codex.rate_limits\"}\n\n\
                    event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"usage_limit_reached\",\"message\":\"The usage limit has been reached\"}}}\n\n";
        match first_verdict(Dialect::Responses, text) {
            Some(Judge::Error { status, kind, .. }) => {
                assert_eq!(status, 429);
                assert_eq!(kind, "usage_limit_reached");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_chat_role_chunk_is_not_content_but_text_is() {
        let role =
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n";
        assert_eq!(first_verdict(Dialect::Chat, role), None);
        let text = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n";
        assert_eq!(first_verdict(Dialect::Chat, text), Some(Judge::Content));
        let err = "data: {\"error\":{\"code\":429,\"message\":\"busy\"}}\n\n";
        assert!(matches!(
            first_verdict(Dialect::Chat, err),
            Some(Judge::Error { status: 429, .. })
        ));
    }

    #[test]
    fn a_gemini_error_carries_its_http_code() {
        let err = "data: {\"error\":{\"code\":503,\"status\":\"UNAVAILABLE\",\"message\":\"overloaded\"}}\n\n";
        assert!(matches!(
            first_verdict(Dialect::Gemini, err),
            Some(Judge::Error { status: 503, .. })
        ));
    }

    #[test]
    fn something_unrecognised_counts_as_content() {
        assert_eq!(
            first_verdict(Dialect::Anthropic, "data: not json\n\n"),
            Some(Judge::Content)
        );
    }

    async fn served(body: &'static str) -> reqwest::Response {
        let mut resp = http::Response::new(reqwest::Body::from(body));
        resp.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/event-stream"),
        );
        reqwest::Response::from(resp)
    }

    #[tokio::test]
    async fn what_was_read_is_handed_on_byte_for_byte() {
        let body = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n\
                    event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\n\
                    event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let r = served(body).await;
        match watch(r, Dialect::Anthropic, false, soon(5)).await {
            Opening::Go(r) => {
                assert_eq!(r.headers()[http::header::CONTENT_TYPE], "text/event-stream");
                assert_eq!(r.text().await.unwrap(), body);
            }
            _ => panic!("该放行"),
        }
    }

    #[tokio::test]
    async fn an_error_before_content_is_reported_not_handed_on() {
        let body = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow\"}}\n\n";
        match watch(served(body).await, Dialect::Anthropic, false, soon(5)).await {
            Opening::Failed { status, .. } => assert_eq!(status, 429),
            _ => panic!("该报失败"),
        }
    }

    fn soon(secs: u64) -> tokio::time::Instant {
        tokio::time::Instant::now() + Duration::from_secs(secs)
    }

    /// 发完开头就不说话的上游：响应头和 `head` 先到，之后流一直开着
    fn stalled(head: &'static str) -> reqwest::Response {
        let first =
            futures::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(head.as_bytes()))]);
        let body = reqwest::Body::wrap_stream(first.chain(futures::stream::pending()));
        let mut resp = http::Response::new(body);
        resp.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/event-stream"),
        );
        reqwest::Response::from(resp)
    }

    #[tokio::test]
    async fn no_content_by_the_deadline_is_slow_and_keeps_what_the_upstream_reported() {
        let head = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1200,\"cache_read_input_tokens\":800,\"output_tokens\":1}}}\n\n\
                    event: ping\ndata: {\"type\":\"ping\"}\n\n";
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        match watch(stalled(head), Dialect::Anthropic, false, deadline).await {
            Opening::Slow { usage, .. } => {
                let u = usage.expect("message_start 报了输入");
                assert_eq!((u.input, u.cache_read), (1200, 800));
            }
            _ => panic!("到点没有内容该是 Slow"),
        }
    }

    /// 整包的回答：先到的空格不算，到点了没有正文是 Slow；正文来了照原样交出去
    #[tokio::test]
    async fn a_whole_answer_waits_for_its_first_real_byte() {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        assert!(matches!(
            first_bytes(stalled("  \n"), deadline).await,
            Opening::Slow { usage: None, .. }
        ));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        match first_bytes(served("  {\"id\":1}").await, deadline).await {
            Opening::Go(r) => assert_eq!(r.text().await.unwrap(), "  {\"id\":1}"),
            _ => panic!("该放行"),
        }
        // 空的正文也放行：交出去由回程照常收尾
        assert!(matches!(
            first_bytes(served("").await, deadline).await,
            Opening::Go(_)
        ));
    }

    /// Gemini 只带用量的块不算开口（和无响应超时同一个判据）
    #[tokio::test]
    async fn a_gemini_usage_only_chunk_is_not_the_first_content() {
        let head = "data: {\"usageMetadata\":{\"promptTokenCount\":10}}\r\n\r\n";
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        assert!(matches!(
            watch(stalled(head), Dialect::Gemini, false, deadline).await,
            Opening::Slow { .. }
        ));
    }

    #[tokio::test]
    async fn a_thinking_delta_is_content_and_a_role_chunk_is_not() {
        // Chat 格式的推理字（DeepSeek、Qwen 的 `reasoning_content`）也是模型开口了
        let head = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Let me think\"}}]}\n\n";
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        assert!(matches!(
            watch(stalled(head), Dialect::Chat, false, deadline).await,
            Opening::Go(_)
        ));
        // 只来了角色的那一块不算
        let role =
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n";
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        assert!(matches!(
            watch(stalled(role), Dialect::Chat, false, deadline).await,
            Opening::Slow { usage: None, .. }
        ));
    }
}
