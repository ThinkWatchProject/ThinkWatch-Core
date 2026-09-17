//! OpenAI Responses（`/v1/responses`）。
//!
//! Codex CLI 说的就是这种格式；ChatGPT 账号的 Codex 后端也是。

pub mod request;
pub mod response;
pub mod stream;

pub use request::{decode_request, encode_request};
pub use response::{decode_response, encode_response};

/// 给 Responses 客户端的错误体，和 Chat 同形
pub use crate::chat::response::{error_body, error_message};
