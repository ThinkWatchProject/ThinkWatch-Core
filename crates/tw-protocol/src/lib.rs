//! SSE 分帧与方言间的内容提取。
//!
//! 注意这里**不含** `streaming.rs`。DESIGN.md §9.0.1 原本把它算进来
//! （sse_parser + streaming + transform = 900 行），但实测它依赖 axum
//! 的 SSE 响应类型、metrics、以及企业版的 PII 流式还原器 ——
//! **那是 HTTP 层的东西，不是协议层的**。把它搬进来会让共用层背上
//! axum，而桌面版的数据面会自己写响应封装。

pub mod sse;
pub mod transform;

pub use sse::{SseEvent, SseStreamExt};
