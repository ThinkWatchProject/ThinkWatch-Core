//! Gemini generateContent（`/v1beta/models/{model}:generateContent`）。

pub mod request;
pub mod response;
pub mod stream;

pub use request::{decode_request, encode_request};
pub use response::{decode_response, encode_response};
