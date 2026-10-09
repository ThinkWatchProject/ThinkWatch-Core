//! 四种接口格式之间的转换：Anthropic Messages、OpenAI Chat Completions、
//! OpenAI Responses、Gemini generateContent。
//!
//! 入口在 [`convert`]：[`convert::prepare`] 改写请求，它返回的 [`convert::Session`]
//! 改写响应、流和错误。每种格式各一个模块，分请求、整包响应、流三部分，全部经过
//! [`ir`] 里的中间表示。这个 crate 只依赖 serde，不碰网络，企业版网关也可以直接用。
//!
//! 同样两边都用的还有：从响应里旁路嗅出用量（[`usage`]，换算和转换共用各家的
//! `usage()`），拼上游地址（[`url`]），去掉 DeepSeek Harness 只发给 DeepSeek 的
//! 扩展（[`harness`]），在原文上找调用方的正文（[`caller`]，内容过滤读它、删它），
//! 读写各格式里名字不同的请求参数（[`params`]），以及 Codex 的远程压缩转给别家时怎么做
//! （[`compaction`]）。

pub mod anthropic;
pub mod bedrock;
pub mod caller;
pub mod chat;
pub mod compaction;
pub mod convert;
pub mod frame;
pub mod gemini;
pub mod harness;
pub mod ir;
pub mod official;
pub mod params;
pub mod responses;
pub mod think;
pub mod url;
pub mod usage;
