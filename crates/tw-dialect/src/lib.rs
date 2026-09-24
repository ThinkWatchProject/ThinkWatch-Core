//! 四种接口格式之间的转换：Anthropic Messages、OpenAI Chat Completions、
//! OpenAI Responses、Gemini generateContent。
//!
//! 入口在 [`convert`]：[`convert::prepare`] 改写请求，它返回的 [`convert::Session`]
//! 改写响应、流和错误。每种格式各一个模块，分请求、整包响应、流三部分，全部经过
//! [`ir`] 里的中间表示。这个 crate 只依赖 serde，不碰网络，企业版网关也可以直接用。
//!
//! 同样两边都用的还有：从响应里旁路嗅出用量（[`usage`]，换算和转换共用各家的
//! `usage()`），以及拼上游地址（[`url`]）。

pub mod anthropic;
pub mod bedrock;
pub mod chat;
pub mod convert;
pub mod frame;
pub mod gemini;
pub mod ir;
pub mod official;
pub mod responses;
pub mod think;
pub mod url;
pub mod usage;
