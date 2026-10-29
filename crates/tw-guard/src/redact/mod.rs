//! 出站脱敏：把敏感值换成占位符，响应回显时再换回来。
//!
//! 一个请求体怎么走（先看一遍、编号、每一跳换）在 [`flow`]，两个网关共用。

mod anchor;
pub mod cache;
mod common;
mod engine;
pub mod exceptions;
pub mod flow;
mod nest;
mod personal;
pub mod replace;
pub mod rules;
mod secrets;
pub mod sse;
pub mod stream;
