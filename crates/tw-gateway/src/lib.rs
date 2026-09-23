//! 数据面：一个请求的生命周期。
//!
//! M0 只做管线的两头 —— 身份识别（第 1 步）和转发（第 5 步）。路由、
//! 准入、计价在 M1/M3。中间缺的那几步不是「以后再插进来」，而是**现在
//! 就把接缝留对**：`forward` 已经按「选中的 provider」取参数，M1 加路由
//! 时只需要换掉挑选逻辑。

pub mod access;
pub mod auth;
pub mod bodies;
pub mod chatgpt;
pub mod client_api;
pub mod clientprobe;
pub mod ending;
pub mod error;
pub mod fixture;
pub mod forward;
pub mod guard;
pub mod health;
pub mod hint;
pub mod l1;
pub mod l3;
pub mod latency;
pub mod limits;
pub mod listen;
pub mod live;
pub mod models;
pub mod oauth;
pub mod probe;
pub mod quota;
pub mod quote;
pub mod server;
pub mod session;
pub mod toolwall;
pub mod translate;
pub mod usage;
pub mod ws;

pub use access::{AllowList, Cidr};
pub use bodies::{BodyKind, BodyRecord, BodySender};
pub use clientprobe::{ProbeKind, classify};
pub use error::GatewayError;
pub use health::Health;
pub use l1::{
    L1Result, Peer, ProxyHop, Segment, Skip, SkipReason, Stage, Step, hop_of, l1, l1_proxy,
    proxy_target,
};
pub use l3::{Estimate, L3Result};
pub use limits::Gate;
pub use listen::{Listening, bind_failure, serve_at};
pub use probe::{ModelList, ProbeResult, probe};
pub use quota::{Quota, from_headers as quota_from_headers};
pub use quote::Quote;
pub use server::{AppState, Runtime, client_for_provider, router, serve};
pub use usage::{Sniffer, Usage};

/// 请求来自谁。**如实写 ThinkWatch** —— 我们从不把自己报成别的客户端。
pub const ORIGINATOR: &str = "thinkwatch";

/// 如实说明自己是谁的 User-Agent：`thinkwatch/0.29.0 (macos; aarch64)`。
///
/// **账号类上游的接口也用它**（ChatGPT 的 Codex 后端、Z.ai 的业务接口）。对方认不认
/// 我们是对方的事；冒充它们自己的客户端不在我们的选项里。
pub fn user_agent() -> String {
    format!(
        "{ORIGINATOR}/{} ({}; {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// 一串不可猜的十六进制令牌。
///
/// Z.ai 的命令行登录拿它当轮询凭据：**它由我们生成**，服务端只是记住它，所以这一次
/// 登录的结果只有拿着它的人取得到。
pub fn opaque_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::fill(buf.as_mut_slice());
    buf.iter().fold(String::new(), |mut out, b| {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
        out
    })
}
