//! 流式回答迟迟没有内容时，**响应头先交给客户端**，故障转移在响应体里接着做。
//!
//! 第一段内容到达之前，回答是压着的（见 [`super::opening`]）：那时还能换一家上游，上游开头
//! 报的错也还能换成一个像样的状态码。可压得太久，客户端自己的计时先到了 —— 等响应头等
//! 不到，它就放弃了，网关的故障转移根本轮不到出手（无响应超时默认 300 秒）。
//!
//! 所以压着的时间有上限：[`crate::OPENING_HOLD`]（15 秒，不可配置）。到点了还没有回答，就
//! 先交出 `200` 和流的响应头，之后每隔 [`crate::KEEPALIVE_EVERY`] 发一行 SSE 注释
//! （`: keep-alive`）—— 每一种 SSE 客户端都跳过注释，它不是内容。上游的事件照旧压着，直到
//! 第一段真内容：在那之前这个请求照样能因为无响应超时、开头报错、流断了换下一家，被放弃的
//! 那一家一个字节都没交出去，下一家的流在同一个 `200` 底下从头开始。
//!
//! - **保活不算内容**：它是网关自己发的，不经过上游那条计时（见 [`super::idle`]）。
//! - **候选都失败了**：状态码已经交出去了，按客户端的格式在流里报那个错 —— 和回答到一半
//!   断了是同一种错误帧。
//! - **Gemini 的客户端不发保活**：它官方的 Python SDK 把注释行当成一段 JSON 去解析，直接
//!   报错。响应头照样先交，流里等到回答为止。
//! - 只管客户端要 SSE 流的生成请求。整包的请求、Gemini 不带 `alt=sse` 的那种（一个 JSON
//!   数组）照旧压到回答为止。
//!
//! 在等的时候被手动中止（见 [`crate::abort`]）：丢掉还在试的那一跳，流里报手动中止。

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;

use super::Inbound;
use crate::error::GatewayError;
use crate::state::AppState;
use tw_dialect::ir::Dialect;

/// 保活那一行：一行 SSE 注释
const KEEPALIVE: &[u8] = b": keep-alive\n\n";

/// 先交响应头的话怎么交。
#[derive(Debug, Clone, Copy)]
pub(super) struct Early {
    /// 压着最多多久（[`crate::OPENING_HOLD`]，测试调短）
    hold: Duration,
    /// 隔多久发一行保活。Gemini 的客户端不发：None
    keepalive: Option<Duration>,
}

/// 这个请求压不住时先不先交响应头：客户端要的是生成回答的 SSE 流才交。
pub(super) fn early(
    state: &AppState,
    req: &Inbound,
    reading: &crate::client_api::Reading,
) -> Option<Early> {
    let streams = reading.generates && matches!(&reading.decoded, Some(Ok(d)) if d.request.stream);
    if !streams {
        return None;
    }
    let gemini = req.dialect == Dialect::Gemini;
    // Gemini 的客户端不带 `alt=sse` 时收的是一个 JSON 数组，不是 SSE
    let sse = !gemini
        || req
            .query
            .as_deref()
            .is_some_and(|q| q.split('&').any(|kv| kv == "alt=sse"));
    sse.then_some(Early {
        hold: state.opening_hold,
        keepalive: (!gemini).then_some(state.keepalive_every),
    })
}

type Running = Pin<Box<dyn Future<Output = Result<Response, GatewayError>> + Send>>;

/// 等 `run`（试上游、交回答的那一段，见 `super::answer`）最多 `early.hold`：等到了就照常交出去；
/// 等不到就先交 `200` 和流的响应头，`run` 挪进响应体里接着跑（见 [`committed`]）。`dialect` 是
/// 客户端的格式：之后的错误按它写。
pub(super) async fn hold(
    run: impl Future<Output = Result<Response, GatewayError>> + Send + 'static,
    dialect: Dialect,
    early: Early,
    abort: crate::abort::Switch,
) -> Result<Response, GatewayError> {
    let mut run: Running = Box::pin(run);
    let by = tokio::time::Instant::now() + early.hold;
    tokio::select! {
        biased;
        r = &mut run => return r,
        _ = tokio::time::sleep_until(by) => {}
    }
    Ok(committed(run, dialect, early.keepalive, abort))
}

/// 已经交出 `200` 的响应：先发保活，`run` 有了结论再接上 —— 一家接下了，就是它的回答；
/// 都失败了，按客户端的格式报错收尾。
fn committed(
    run: Running,
    dialect: Dialect,
    keepalive: Option<Duration>,
    abort: crate::abort::Switch,
) -> Response {
    let stream = async_stream::stream! {
        let mut run = run;
        // 第一行保活跟着响应头一起走
        let mut next = tokio::time::Instant::now();
        let every = keepalive.unwrap_or(Duration::MAX);
        let got = loop {
            tokio::select! {
                biased;
                r = &mut run => break Some(r),
                // 停在看不着开关的地方时由这里丢掉它：结局在它手上，按手动中止报（见 `Ending`）
                _ = abort.wait() => break None,
                _ = tokio::time::sleep_until(next), if keepalive.is_some() => {
                    next = tokio::time::Instant::now() + every;
                    yield Ok::<Bytes, std::io::Error>(Bytes::from_static(KEEPALIVE));
                }
            }
        };
        match got {
            None => {
                drop(run);
                yield Ok(Bytes::from(GatewayError::aborted().in_dialect(dialect).sse_frame()));
            }
            // 结局已经在 `answer` 里报过了
            Some(Err(e)) => yield Ok(Bytes::from(e.in_dialect(dialect).sse_frame())),
            Some(Ok(resp)) if resp.status().is_success() => {
                let mut body = resp.into_body().into_data_stream();
                while let Some(chunk) = body.next().await {
                    match chunk {
                        Ok(b) => yield Ok(b),
                        Err(e) => {
                            yield Err(std::io::Error::other(e));
                            break;
                        }
                    }
                }
            }
            // 最后一家回了错误、原样交出的那种：状态码交不出去了，把它说的话写成流里的错误
            Some(Ok(resp)) => {
                let status = resp.status().as_u16();
                let body = axum::body::to_bytes(resp.into_body(), crate::failure::BODY_PEEK * 4)
                    .await
                    .unwrap_or_default();
                yield Ok(Bytes::from(said(dialect, status, &body)));
            }
        }
    };
    let mut resp = Response::new(Body::from_stream(stream));
    let h = resp.headers_mut();
    h.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/event-stream"),
    );
    h.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-cache"),
    );
    resp
}

/// 一个错误响应（`status`，正文 `body`，客户端那种格式）写成流里的一帧错误。上游的原话照说，
/// 不加前缀：那是它说的
fn said(dialect: Dialect, status: u16, body: &[u8]) -> String {
    let text = tw_dialect::convert::error_message(dialect, body)
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string());
    tw_dialect::convert::error_frame(dialect, status, &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_answer_becomes_an_error_event_in_the_clients_format() {
        let body = br#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long"}}"#;
        let f = said(Dialect::Anthropic, 400, body);
        assert!(f.starts_with("event: error\n"), "{f}");
        assert!(f.contains("prompt is too long"), "{f}");
        assert!(f.contains("invalid_request_error"), "{f}");
        let f = said(Dialect::Responses, 400, br#"{"error":{"message":"bad"}}"#);
        assert!(f.starts_with("event: response.failed\n"), "{f}");
    }
}
