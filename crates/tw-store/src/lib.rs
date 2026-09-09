//! 请求历史与运行时状态（DESIGN.md §8）。
//!
//! **metadata 进 SQLite，body 进文件系统。**这个分离是刻意的：
//!
//! - metadata 每条约 500 字节，10 万请求 50 MB，SQLite 轻松处理，而且
//!   支持复杂查询；
//! - body 可能几百 KB（长上下文），塞进 SQLite 会让库膨胀到 GB 级 ——
//!   VACUUM 慢、备份慢、回收难。存文件系统按天分目录，过期直接删掉整个
//!   目录，回收成本 O(1)。

pub mod blobs;
pub mod db;
pub mod disk;
pub mod recorder;
pub mod task;

pub use blobs::{Blobs, Which};
pub use db::{Db, DbError, Latency, RequestRow, Summary};
pub use disk::{DiskLevel, free_bytes, level_for};
pub use recorder::Recorder;
pub use task::StoredBody;
