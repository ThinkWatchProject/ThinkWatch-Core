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
pub mod health;
pub mod model_name;
pub mod recorder;
pub mod search;
pub mod task;
pub mod transcript;

pub use blobs::{Blobs, Which};
pub use db::{Db, DbError, Latency, PluginRunRow, RequestRow, SecurityEvent, Summary, TokenRate};
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

    /// 24 版的库有两种：安全防护合成三项的那一版（安全日志多了 `matching`、`revealed` 两列）
    /// 和脚本插件的那一版（多了 `plugin_runs`）—— 两边各自从 23 加到了 24，合在一起是 25。
    /// **哪一种都要整个重建。**照旧打开的话，少了的那张表、那两列要到第一次写的时候才出错，
    /// 而存储层的错只记一行日志：安全日志、插件的运行记录就这么悄悄丢了
    #[test]
    fn either_kind_of_version_24_database_starts_over() {
        for (kind, older) in [
            ("guard unify", "DROP TABLE plugin_runs;"),
            (
                "script plugins",
                "ALTER TABLE security_events DROP COLUMN matching;
                 ALTER TABLE security_events DROP COLUMN revealed;",
            ),
        ] {
            let d = tempfile::tempdir().unwrap();
            {
                let (db, blobs) = open(d.path()).unwrap();
                db.insert(&db::tests::row(1, 100)).unwrap();
                assert!(blobs.put(100, 1, Which::Request, b"old"));
            }
            {
                let conn = rusqlite::Connection::open(d.path().join("data.db")).unwrap();
                conn.execute_batch(older).unwrap();
                conn.pragma_update(None, "user_version", 24).unwrap();
            }
            let (db, blobs) = open(d.path()).unwrap();
            assert_eq!(db.count().unwrap(), 0, "the {kind} database was kept");
            assert!(
                blobs.get(100, 1, Which::Request).is_none(),
                "{kind}: 旧正文还在"
            );
            // 重建出来的库两边的都有
            db.insert(&db::tests::row(1, 100)).unwrap();
            db.insert_security_event(&SecurityEvent {
                at_ms: 100,
                request_id: 1,
                guard: tw_api::Guard::Content,
                rule: "unicode-tags".into(),
                custom: false,
                action: tw_api::SecurityOutcome::Stripped,
                provider: "官方".into(),
                client: "default".into(),
                tool: None,
                excerpt: "‹U+E0069 ×6›".into(),
                count: 6,
                matching: Some(tw_api::ContentMatch::Codepoints),
                revealed: Some("ignore".into()),
            })
            .unwrap();
            db.insert_plugin_run(&PluginRunRow {
                request_id: 1,
                at_ms: 100,
                plugin_id: "current-date".into(),
                plugin_name: "附加当前日期".into(),
                hook: tw_api::PluginHook::Request,
                outcome: tw_api::PluginOutcome::Changed,
                error: None,
                cpu_us: 5,
                detail: None,
            })
            .unwrap();
            assert_eq!(db.plugin_runs(1).unwrap().len(), 1, "{kind}");
            let log = db.security_of(&[1]).unwrap().remove(&1).unwrap_or_default();
            assert_eq!(log.len(), 1, "{kind}");
            assert_eq!(log[0].revealed.as_deref(), Some("ignore"), "{kind}");
        }
    }
}
