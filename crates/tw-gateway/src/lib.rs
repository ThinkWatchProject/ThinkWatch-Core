//! 数据面：一个请求的生命周期。
//!
//! M0 只做管线的两头 —— 身份识别（第 1 步）和转发（第 5 步）。路由、
//! 准入、计价在 M1/M3。中间缺的那几步不是「以后再插进来」，而是**现在
//! 就把接缝留对**：`forward` 已经按「选中的 provider」取参数，M1 加路由
//! 时只需要换掉挑选逻辑。

pub mod auth;
pub mod error;
pub mod forward;
pub mod probe;
pub mod server;

pub use error::GatewayError;
pub use probe::{ProbeResult, probe};
pub use server::{AppState, router, serve};
