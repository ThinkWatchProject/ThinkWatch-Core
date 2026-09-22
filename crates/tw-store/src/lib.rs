//! 请求历史与运行时状态。
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
pub mod recorder;
pub mod task;

pub use blobs::{Blobs, Which};
pub use db::{Db, DbError, Latency, RequestRow, SecurityEvent, Summary};
pub use recorder::{Recorder, price_source};
pub use task::StoredBody;

use std::path::Path;

/// 打开数据目录里的请求库（`data.db`）和正文目录（`blobs/`）。
///
/// **库是别的版本建的，就连同正文整个重建，不迁移。**项目还没有存量用户，
/// 为旧库写迁移只是负担。正文必须一起清：新库的请求号从 1 重新数，留着的
/// 旧正文会被当成新请求的正文显示出来。
pub fn open(dir: &Path) -> Result<(Db, Blobs), DbError> {
    let path = dir.join("data.db");
    let blobs = Blobs::new(dir.join("blobs"));
    match Db::open(&path) {
        Err(DbError::OtherVersion { found, supported }) => {
            tracing::warn!(
                found,
                supported,
                "the request history was written by another version; starting over with an empty one"
            );
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            }
            let _ = std::fs::remove_dir_all(blobs.root());
            Ok((Db::open(&path)?, blobs))
        }
        other => Ok((other?, blobs)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 别的版本建的库连同正文一起清掉，换一个空的 —— 留着正文的话，新库里
    /// 的 1 号请求会显示旧库 1 号请求的正文。
    #[test]
    fn a_database_from_another_version_starts_over_with_its_bodies() {
        let d = tempfile::tempdir().unwrap();
        {
            let (db, blobs) = open(d.path()).unwrap();
            db.insert(&db::tests::row(1, 100)).unwrap();
            assert!(blobs.put(100, 1, Which::Request, b"old"));
        }
        {
            let conn = rusqlite::Connection::open(d.path().join("data.db")).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }
        let (db, blobs) = open(d.path()).unwrap();
        assert_eq!(db.count().unwrap(), 0);
        assert!(blobs.get(100, 1, Which::Request).is_none(), "旧正文还在");
        // 重建之后就是当前版本，下一次打开原样读
        db.insert(&db::tests::row(1, 100)).unwrap();
        drop(db);
        assert_eq!(open(d.path()).unwrap().0.count().unwrap(), 1);
    }
}
