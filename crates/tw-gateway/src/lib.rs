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
pub mod leak;
pub mod limits;
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
pub use limits::{Gate, LimitError, Limits};
pub use probe::{ModelList, ProbeResult, probe};
pub use quota::{Quota, from_headers as quota_from_headers};
pub use quote::Quote;
pub use server::{AppState, Runtime, client_for_provider, router, serve, serve_following_config};
pub use usage::{Sniffer, Usage};
