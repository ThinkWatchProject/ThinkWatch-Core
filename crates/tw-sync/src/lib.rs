//! 多设备配置同步。
//!
//! 多台设备（本机 Core、远程 core）通过用户自选的同步位置共用一份配置：
//!
//! - **同步位置只追加新版本、不覆盖已有文件**（[`layout`]）。不依赖存储服务的条件写入，文件夹、
//!   WebDAV、S3 是同一套机制（[`store`]）；
//! - **按对象三方合并**，以共同的上一版本为基准（[`merge`]）。不同设备改不同对象、同一对象的
//!   不同部分时自动合并，只有同一处被改成不同内容才是冲突；
//! - 内容可选加密：一把随机的版本密钥，以同步口令加密后存在同步位置上（[`keyring`]）；
//! - 每台设备在同步位置上有一份设备记录（[`device`]）。
//!
//! 这里不碰控制面：调度、写配置、事件在 `tw-control` 的 `sync` 模块。

pub mod device;
pub mod engine;
pub mod keyring;
pub mod layout;
pub mod local;
pub mod merge;
pub mod order;
pub mod record;
pub mod store;

pub use store::{AnyStore, Entry, Store, StoreError};
