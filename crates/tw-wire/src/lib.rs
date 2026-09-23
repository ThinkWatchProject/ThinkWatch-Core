//! 响应流过时顺带做的两件事。
//!
//! 两者共享同一条纪律：**不缓冲**。响应是流着出去的，为了看一眼它而把
//! 它整块攒下来，会把 SSE 变成一次性交付 —— 客户端看到的是「卡很久，然后
//! 一下全出来」。所以这里的东西都是旁路的：字节照常流向客户端，同时喂
//! 它们一份。
//!
//! [`usage`] 嗅出上游报的用量，[`bodies`] 留档请求和响应的正文。
//!
//! 这一层刻意零业务依赖 —— 它不认识配置从哪来、数据存哪里、谁在调用，
//! 所以企业版和桌面版都能直接用。

pub mod bodies;
pub mod usage;

pub use bodies::{BodyKind, BodyRecord, BodySender, ResponseTap};
pub use usage::{Sniffer, Usage};
