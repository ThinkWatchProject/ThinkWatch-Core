//! 各家 provider 的实现，外加一个 dyn-兼容的包装。

pub mod anthropic;
pub mod azure_openai;
pub mod bedrock;
pub mod custom;
pub mod google;
pub mod openai;
pub mod openai_responses;
pub mod protocol;
