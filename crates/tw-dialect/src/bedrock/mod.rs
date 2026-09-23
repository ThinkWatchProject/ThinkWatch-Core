//! Bedrock Converse（`/model/{id}/converse`）。
//!
//! **和另外四种的一处根本差别:它的流不是 SSE**，是 AWS eventstream 的二进制帧
//! （前导长度 + 头部 + 载荷 + CRC）。拆帧不在这个 crate 里 —— 那是传输层的事，
//! 和 SSE 平级；这里只管帧里装什么。传输层把每个事件交成一个 [`crate::frame::Frame`]：
//! `:event-type` 头进 `event`，载荷 JSON 进 `data`。

pub mod request;
pub mod response;
pub mod stream;

pub use request::{decode_request, encode_request};
pub use response::{decode_response, encode_response};
