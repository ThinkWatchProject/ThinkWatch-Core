//! Amazon Bedrock 在线路上要的东西，两个网关共用。
//!
//! - [`endpoint`]：区域、runtime 和控制面的地址、模型 id 怎么写进路径
//! - [`beta`]：客户端的 `anthropic-beta` 里哪些能带给 Bedrock 上的 Claude
//! - [`sign`]：SigV4 签名，以及「带 API Key 就不签」这条规则
//! - [`eventstream`]：ConverseStream 的二进制帧 → SSE
//! - [`catalog`]：一个区域能路由到哪些模型
//! - [`error`]：AWS 的错误里能拿出来什么、能给谁看
//!
//! Converse 和别的格式之间怎么转不在这里 —— 那是 `tw_dialect::bedrock`。
//! 凭证从哪儿来也不在这里：配置里的访问密钥、EC2 实例角色，都由调用方取到之后
//! 交进来。这里只放两边都一样的部分，**一份实现**。
//!
//! 这是第一层：只依赖第三方 crate 和第一层的彼此（`tw-dialect` 的
//! `layer_one_depends_only_on_itself` 守着）。

pub mod beta;
pub mod catalog;
pub mod endpoint;
pub mod error;
pub mod eventstream;
pub mod sign;

pub use sign::{Credentials, SignError, carries_api_key};
