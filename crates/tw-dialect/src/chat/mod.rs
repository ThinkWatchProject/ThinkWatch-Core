//! OpenAI Chat Completions（`/v1/chat/completions`）。
//!
//! 除了 OpenAI 官方，DeepSeek、Kimi、通义、Ollama、vLLM 这些都说这种格式，而且各有
//! 扩展。**认得出的扩展照样接**（推理内容的 `reasoning_content`），编码时只写各家
//! 都认的字段。

pub mod request;
pub mod response;
pub mod stream;

pub use request::{decode_request, encode_request};
pub use response::{decode_response, encode_response};
