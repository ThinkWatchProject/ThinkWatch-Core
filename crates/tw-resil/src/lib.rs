//! 重试与熔断。
//!
//! 桌面版的熔断参数比企业版克制得多（只留连续失败
//! 阈值和冷却时长两个旋钮），但状态机本身是同一套。

pub mod cb_registry;
pub mod metrics_labels;
pub mod retry;

pub use cb_registry::{CbState, record_cb_with_kind};
pub use retry::*;
