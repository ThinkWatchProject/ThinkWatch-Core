//! 用量历史：按 UTC 整点累加的用量，概览上「最近一年」那张热力图用（`GET /summary/history`）。
//!
//! **存在数据目录里另一个库文件 `usage.db`**，和请求库（`data.db`）同目录、同样 0600。请求库的
//! [`crate::db::SCHEMA`] 一变就整个重建、记录也只留保留期那么多天；一年的用量得跨过这两样
//! 留下来。所以它有自己的版本号（[`SCHEMA`]），只在它自己的表变了时才重建，也不随请求记录的
//! 保留期删除。一个小时一行，一年不到九千行。
//!
//! **ATTACH 在请求库的同一条连接上**（库名 `usage`）：结局那一行写进 `requests` 之后，直接从
//! 那一行 `INSERT … SELECT` 加进它的整点。取数和 `/summary/buckets` 是同一串 SQL
//! （[`crate::db::TALLY`]），口径不在 Rust 里另抄一份；新建时从请求库补历史也是同一个 SELECT。
//!
//! **每个请求只加一次**：只在结局（`Recorder::settle`，每个请求只走一次）那一行写进去之后加。
//! 本地应答、网关自己估 token 数的（`local`）不加 —— 和汇总一样不算它们。
//!
//! **写失败只记日志**（存储层的第一条规矩）：少加一个请求，转发照跑。

use std::path::Path;
use std::sync::LazyLock;

use rusqlite::{Connection, OptionalExtension, params};

use crate::db::{Db, DbError, TALLY, cost_bucket};

/// 数据目录里用量历史的文件名
pub(crate) const FILE: &str = "usage.db";

/// 用量历史的版本。**只在它自己的表变了时加一**，和请求库的 [`crate::db::SCHEMA`] 无关。
/// 版本旧的整个重建（从请求库现有的行补），更新的版本建的原样留着
pub(crate) const SCHEMA: i64 = 1;

const HOUR_MS: i64 = 3_600_000;

/// 一个整点那一行的十二项，顺序同 [`TALLY`]
const COLUMNS: [&str; 12] = [
    "requests",
    "failed",
    "cost_micros_exact",
    "cost_micros_estimated",
    "unpriced_requests",
    "no_usage_requests",
    "sent_bytes",
    "received_bytes",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
];

/// 这一次用量历史用不用得了。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Usage {
    Ready,
    /// 用不了：更新的版本建的、打不开。请求记录照常，问历史的那个查询报错，原因是这一句
    Unavailable(String),
}

/// 把用量历史 ATTACH 到请求库的连接上，库名 `usage`。`path` 是 None 时是内存库。
///
/// **出什么错都不让请求库打不开**：记成用不了，记一行日志。
pub(crate) fn attach(conn: &Connection, path: Option<&Path>) -> Usage {
    match try_attach(conn, path) {
        Ok(u) => u,
        Err(e) => {
            let _ = conn.execute_batch("DETACH DATABASE usage");
            tracing::warn!(
                "the usage history could not be opened, so it is unavailable this run; requests are still recorded: {e}"
            );
            Usage::Unavailable(e.to_string())
        }
    }
}

fn try_attach(conn: &Connection, path: Option<&Path>) -> rusqlite::Result<Usage> {
    let file = match path {
        Some(p) => {
            crate::db::create_private(p);
            p.to_string_lossy().into_owned()
        }
        None => ":memory:".to_string(),
    };
    conn.execute("ATTACH DATABASE ?1 AS usage", [&file])?;
    let found: i64 = conn.pragma_query_value(Some("usage"), "user_version", |r| r.get(0))?;
    // **更新的版本建的不动它**：装回了旧版、或者另一份更新过的桌面端用过这个目录。重建的话，
    // 换回新版时那一年的用量就没了；留着，新版下次打开原样读
    if found > SCHEMA {
        conn.execute_batch("DETACH DATABASE usage")?;
        tracing::warn!(
            found,
            supported = SCHEMA,
            "the usage history was written by a newer version; leaving it alone, so it is unavailable this run"
        );
        return Ok(Usage::Unavailable(format!(
            "it was written by a newer version (schema {found}; this twcore reads {SCHEMA})"
        )));
    }
    // 和请求库同样的理由：读不挡写，观测数据不值得每次写都 fsync
    conn.pragma_update(Some("usage"), "journal_mode", "WAL")?;
    conn.pragma_update(Some("usage"), "synchronous", "NORMAL")?;
    if let Some(p) = path {
        crate::db::restrict(p);
    }
    if found != SCHEMA {
        create(conn)?;
    }
    Ok(Usage::Ready)
}

/// 建表，**从请求库现有的行补一遍**：升级的用户立刻有保留期内的历史。之后不再补。
///
/// 「从什么时候起有记录」记成这时请求库里最老一行的时刻（本地应答也算：那时候已经在记了）；
/// 请求库是空的就记成这一刻。
///
/// 放在一个事务里：建到一半断掉的话，下次打开看到的仍然是旧版本（或者 0），重来一遍，而不是
/// 一个补了一半、版本号却对得上的库。**里面原有的表整个丢掉**：旧版本建的，样子不对
fn create(conn: &Connection) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    let tables: Vec<String> = tx
        .prepare(
            "SELECT name FROM usage.sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for t in tables {
        tx.execute_batch(&format!("DROP TABLE usage.\"{}\"", t.replace('"', "\"\"")))?;
    }
    let columns: String = COLUMNS
        .iter()
        .map(|c| format!(",\n                {c} INTEGER NOT NULL"))
        .collect();
    tx.execute_batch(&format!(
        "-- 一个 UTC 整点一行，这个小时里结束的请求按开始的时刻加进来。**没有用量的小时没有行**
         CREATE TABLE usage.hours (
            hour_ms INTEGER PRIMARY KEY{columns}
         );
         -- 只有一行：从什么时候起有记录。这一刻之前是「没有记录」，不是「没有用量」
         CREATE TABLE usage.meta (
            id       INTEGER PRIMARY KEY CHECK (id = 1),
            since_ms INTEGER NOT NULL
         );
         PRAGMA usage.user_version = {SCHEMA};"
    ))?;
    tx.execute(
        "INSERT INTO usage.meta (id, since_ms) SELECT 1, COALESCE(MIN(at_ms), ?1) FROM main.requests",
        [now_ms()],
    )?;
    let n = tx.execute(&ADD_ALL, [])?;
    tx.commit()?;
    if n > 0 {
        tracing::info!(
            hours = n,
            "started the usage history from the request records already kept"
        );
    }
    Ok(())
}

/// 把请求库里的行按 UTC 整点合计、加进用量历史。`{filter}` 是选哪些行，本地应答一律不算。
///
/// 按 `GROUP BY` 出行：一行都没选中（本地应答、没有这一行）时什么都不插，不会插一个全是 0
/// 的整点。整点用欧几里得取余对齐，负的时刻也落在它之前的那个整点上
fn add_sql(filter: &str) -> String {
    let names = COLUMNS.join(", ");
    let set = COLUMNS
        .iter()
        .map(|c| format!("{c} = {c} + excluded.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO usage.hours (hour_ms, {names})
         SELECT at_ms - ((at_ms % {HOUR_MS}) + {HOUR_MS}) % {HOUR_MS} AS h, {tally}
         FROM main.requests
         WHERE local = 0 AND {filter}
         GROUP BY h
         ON CONFLICT (hour_ms) DO UPDATE SET {set}",
        tally = *TALLY
    )
}

/// 结局那一行（`?1` 是它的号）
static ADD_ONE: LazyLock<String> = LazyLock::new(|| add_sql("id = ?1"));
/// 新建时补历史：请求库里的每一行
static ADD_ALL: LazyLock<String> = LazyLock::new(|| add_sql("1"));

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Db {
    /// 一个请求的结局那一行刚写进请求库：把它加进它开始的那个整点。
    ///
    /// **只在结局那一次调**（`Recorder::settle`）：同一个请求调两次就加了两次。本地应答、
    /// `local` 的行在 SQL 里就排除了，调了也不加。用量历史这一次用不了的，什么都不做
    pub(crate) fn add_to_usage(&self, id: i64) -> Result<(), DbError> {
        if self.usage != Usage::Ready {
            return Ok(());
        }
        self.conn.prepare_cached(&ADD_ONE)?.execute([id])?;
        Ok(())
    }

    /// `[since_ms, until_ms)` 里的整点按 `(hour_ms - since_ms) / bucket_ms` 分格合计（格子的起点是
    /// `since_ms + k × bucket_ms`，同 [`Db::cost_buckets`]）。**稀疏**：没有用量的格子不在里面。
    /// 首 token 的三项不记（None、None、0）。
    ///
    /// 用量历史这一次用不了（更新的版本建的、打不开）是 [`DbError::UsageUnavailable`]
    pub fn usage_history(
        &self,
        since_ms: i64,
        until_ms: i64,
        bucket_ms: i64,
    ) -> Result<tw_api::UsageHistory, DbError> {
        if let Usage::Unavailable(why) = &self.usage {
            return Err(DbError::UsageUnavailable(why.clone()));
        }
        let since: Option<i64> = self
            .conn
            .query_row("SELECT since_ms FROM usage.meta", [], |r| r.get(0))
            .optional()?;
        if bucket_ms <= 0 {
            return Ok(tw_api::UsageHistory {
                since_ms: since,
                buckets: Vec::new(),
            });
        }
        let sums: String = COLUMNS.iter().map(|c| format!(", SUM({c})")).collect();
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT ((hour_ms - ?1) / ?3) AS b{sums}
             FROM usage.hours
             WHERE hour_ms >= ?1 AND hour_ms < ?2
             GROUP BY b ORDER BY b"
        ))?;
        let buckets = st
            .query_map(params![since_ms, until_ms, bucket_ms], |r| {
                cost_bucket(since_ms + r.get::<_, i64>(0)? * bucket_ms, r)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(tw_api::UsageHistory {
            since_ms: since,
            buckets,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::row;

    const T0: i64 = 1_700_000_000_000 - 1_700_000_000_000 % HOUR_MS;

    /// 只读用量历史这一个查询，一格一小时，从 `T0` 起十天
    fn hours(db: &Db) -> Vec<tw_api::CostBucket> {
        db.usage_history(T0, T0 + 240 * HOUR_MS, HOUR_MS)
            .unwrap()
            .buckets
    }

    fn settle(db: &Db, r: &crate::RequestRow) {
        db.insert(r).unwrap();
        db.add_to_usage(r.id).unwrap();
    }

    /// 历史里的那一格和 `/summary/buckets` 同一段时间的那一格**每一项都一样**：口径是同一串
    /// SQL，这里看它们真的没有走开
    #[test]
    fn an_hour_adds_up_exactly_like_the_buckets_of_the_request_records() {
        let db = Db::in_memory().unwrap();
        let mut exact = row(1, T0 + 10);
        exact.cost_micros = Some(5_000);
        let mut est = row(2, T0 + 20);
        est.cost_micros = Some(3_000);
        est.cost_estimated = true;
        let mut unpriced = row(3, T0 + 30);
        unpriced.cost_micros = None;
        let mut no_usage = row(4, T0 + 40);
        no_usage.cost_micros = None;
        no_usage.input_tokens = None;
        no_usage.output_tokens = None;
        no_usage.cache_read_tokens = None;
        let mut failed = row(5, T0 + 50);
        failed.error = Some(crate::db::tests::upstream_failed("broke"));
        failed.cost_estimated = true;
        failed.cache_write_tokens = Some(77);
        for r in [&exact, &est, &unpriced, &no_usage, &failed] {
            settle(&db, r);
        }
        let got = hours(&db);
        // 首 token 的三项历史里不记
        let want: Vec<_> = db
            .cost_buckets(T0, T0 + 240 * HOUR_MS, HOUR_MS)
            .unwrap()
            .into_iter()
            .map(|b| tw_api::CostBucket {
                ttft_p50_ms: None,
                ttft_p95_ms: None,
                ttft_samples: 0,
                ..b
            })
            .collect();
        assert_eq!(got.len(), 1);
        assert_eq!(format!("{got:?}"), format!("{want:?}"));
        let b = &got[0];
        assert_eq!(b.requests, 5);
        assert_eq!(b.failed, 1);
        assert_eq!(b.cost_micros_exact, 5_000);
        assert_eq!(b.cost_micros_estimated, 3_000 + 12_000);
        assert_eq!(b.unpriced_requests, 1, "没有价格的单独数，不当成 0");
        assert_eq!(b.no_usage_requests, 1, "没有用量的单独数");
        assert_eq!(b.input_tokens, 4_000);
        assert_eq!(b.cache_write_tokens, 77);
        assert_eq!(b.sent_bytes, 5 * 4321);
        assert_eq!(
            (b.ttft_p50_ms, b.ttft_p95_ms, b.ttft_samples),
            (None, None, 0)
        );
        let s = db.summary(T0, T0 + HOUR_MS).unwrap();
        assert_eq!(
            (
                s.requests,
                s.failed,
                s.cost_micros_exact,
                s.cost_micros_estimated
            ),
            (
                b.requests,
                b.failed,
                b.cost_micros_exact,
                b.cost_micros_estimated
            )
        );
        assert_eq!(
            (s.unpriced_requests, s.no_usage_requests),
            (b.unpriced_requests, b.no_usage_requests)
        );
    }

    /// 费用三态各自累加：同一个整点里先后结束的请求，三项各加各的
    #[test]
    fn each_cost_state_accumulates_on_its_own() {
        let db = Db::in_memory().unwrap();
        for id in 1..=3 {
            let mut r = row(id, T0 + id);
            r.cost_micros = Some(1_000 * id);
            settle(&db, &r);
            let mut e = row(10 + id, T0 + 10 + id);
            e.cost_micros = Some(100 * id);
            e.cost_estimated = true;
            settle(&db, &e);
            let mut u = row(20 + id, T0 + 20 + id);
            u.cost_micros = None;
            settle(&db, &u);
        }
        let b = &hours(&db)[0];
        assert_eq!(b.cost_micros_exact, 6_000);
        assert_eq!(b.cost_micros_estimated, 600);
        assert_eq!(b.unpriced_requests, 3);
        assert_eq!(b.requests, 9);
    }

    /// 结局之前那一行可能已经写过（`INSERT OR REPLACE`）：**加只看结局那一次**，写几次行都一样
    #[test]
    fn a_request_written_twice_is_added_once() {
        let db = Db::in_memory().unwrap();
        let mut started = row(1, T0 + 5);
        started.cost_micros = None;
        started.input_tokens = None;
        db.insert(&started).unwrap();
        settle(&db, &row(1, T0 + 5));
        let got = hours(&db);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].requests, 1);
        assert_eq!(got[0].input_tokens, 1_000);
    }

    /// 本地应答、网关估 token 数的（都是 `local`）不加：和汇总一样不算
    #[test]
    fn local_rows_are_not_added() {
        let db = Db::in_memory().unwrap();
        let mut local = row(1, T0 + 5);
        local.local = true;
        local.cost_micros = Some(0);
        settle(&db, &local);
        assert!(hours(&db).is_empty(), "本地应答加进去了");
        settle(&db, &row(2, T0 + 6));
        assert_eq!(hours(&db)[0].requests, 1);
    }

    /// 分格：整点按 `(hour_ms - from) / bucket` 落格，格子起点是 `from + k × bucket`；跨过整点
    /// 的两个请求进两个整点，同一格里合计；`to` 截在整点上，那个整点不算
    #[test]
    fn hours_fall_into_buckets_counted_from_the_start() {
        let db = Db::in_memory().unwrap();
        let day = 24 * HOUR_MS;
        // 第 0 天的最后一小时、第 1 天的第一小时、第 1 天的中间、第 3 天
        for (id, at) in [
            (1, T0 + day - 1),
            (2, T0 + day),
            (3, T0 + day + 5 * HOUR_MS + 7),
            (4, T0 + 3 * day + 1),
        ] {
            settle(&db, &row(id, at));
        }
        let h = db.usage_history(T0, T0 + 10 * day, day).unwrap().buckets;
        let got: Vec<(i64, i64)> = h.iter().map(|b| (b.at_ms, b.requests)).collect();
        assert_eq!(
            got,
            vec![(T0, 1), (T0 + day, 2), (T0 + 3 * day, 1)],
            "稀疏，空格不补"
        );
        // 起点不在整点上：落格照 `from` 数，格子起点是 `from + k × day`。第 1 天零点那个整点
        // 离 `from` 不到一天，和前一个整点同在第 0 格
        let from = T0 + 30 * 60_000;
        let h = db.usage_history(from, T0 + 10 * day, day).unwrap().buckets;
        let got: Vec<(i64, i64)> = h.iter().map(|b| (b.at_ms, b.requests)).collect();
        assert_eq!(got, vec![(from, 2), (from + day, 1), (from + 2 * day, 1)]);
        // `to` 截在第 1 天零点：那个整点不算，前一个整点算
        let h = db.usage_history(T0, T0 + day, HOUR_MS).unwrap().buckets;
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].at_ms, T0 + day - HOUR_MS);
        // 每小时一格时，整点就是请求开始的那个小时
        let h = hours(&db);
        assert_eq!(h[2].at_ms, T0 + day + 5 * HOUR_MS);
    }

    /// 新建用量历史时从请求库现有的行补一遍，「从什么时候起有记录」是最老那一行（本地应答也算）
    #[test]
    fn a_new_usage_history_starts_from_the_rows_already_kept() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("data.db");
        {
            let db = Db::open(&path).unwrap();
            let mut local = row(1, T0 - 7);
            local.local = true;
            db.insert(&local).unwrap();
            db.insert(&row(2, T0 + 1)).unwrap();
            db.insert(&row(3, T0 + 2 * HOUR_MS)).unwrap();
        }
        std::fs::remove_file(d.path().join(FILE)).unwrap();
        for s in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(d.path().join(format!("{FILE}{s}")));
        }
        let db = Db::open(&path).unwrap();
        let h = db
            .usage_history(T0 - HOUR_MS, T0 + 10 * HOUR_MS, HOUR_MS)
            .unwrap();
        assert_eq!(h.since_ms, Some(T0 - 7), "最老那一行是本地应答，也算");
        let got: Vec<(i64, i64)> = h.buckets.iter().map(|b| (b.at_ms, b.requests)).collect();
        assert_eq!(got, vec![(T0, 1), (T0 + 2 * HOUR_MS, 1)]);
        drop(db);
        // **之后不再补**：再打开一次，数不翻倍
        let db = Db::open(&path).unwrap();
        assert_eq!(hours(&db).iter().map(|b| b.requests).sum::<i64>(), 2);
    }

    /// 请求库是空的：「从什么时候起有记录」是建库那一刻
    #[test]
    fn an_empty_request_store_starts_the_history_now() {
        let before = now_ms();
        let db = Db::in_memory().unwrap();
        let after = now_ms();
        let h = db.usage_history(0, i64::MAX, HOUR_MS).unwrap();
        let since = h.since_ms.unwrap();
        assert!(before <= since && since <= after, "{since}");
        assert!(h.buckets.is_empty());
    }

    /// 请求库因为版本旧被整个重建（[`crate::open`]），**用量历史还在**：「从什么时候起」不变，
    /// 也不再从新的空库补
    #[test]
    fn the_history_outlives_a_rebuilt_request_store() {
        let d = tempfile::tempdir().unwrap();
        let since = {
            let (db, _) = crate::open(d.path()).unwrap();
            settle(&db, &row(1, T0 + 1));
            settle(&db, &row(2, T0 + HOUR_MS + 1));
            db.usage_history(0, i64::MAX, HOUR_MS).unwrap().since_ms
        };
        assert!(since.is_some());
        {
            let conn = Connection::open(d.path().join("data.db")).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }
        let (db, _) = crate::open(d.path()).unwrap();
        assert_eq!(db.count().unwrap(), 0, "请求库没有重建");
        let h = db.usage_history(0, i64::MAX, HOUR_MS).unwrap();
        assert_eq!(h.since_ms, since, "「从什么时候起」变了");
        assert_eq!(h.buckets.iter().map(|b| b.requests).sum::<i64>(), 2);
        // 重建之后接着加
        settle(&db, &row(1, T0 + 2));
        assert_eq!(hours(&db)[0].requests, 2);
    }

    /// 请求记录过了保留期被删，用量历史不跟着删
    #[test]
    fn pruning_the_request_records_keeps_the_history() {
        let db = Db::in_memory().unwrap();
        settle(&db, &row(1, T0 + 1));
        assert_eq!(db.prune_before(T0 + HOUR_MS).unwrap(), 1);
        assert_eq!(hours(&db)[0].requests, 1);
    }

    /// 更新的版本建的用量历史**不动它**：请求库照常打开、照常记，历史这一项报错；文件原样留着
    #[test]
    fn a_usage_history_from_a_newer_version_is_left_alone() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("data.db");
        {
            let db = Db::open(&path).unwrap();
            settle(&db, &row(1, T0 + 1));
        }
        let usage = d.path().join(FILE);
        {
            let conn = Connection::open(&usage).unwrap();
            conn.pragma_update(None, "user_version", SCHEMA + 1)
                .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let e = db.usage_history(0, i64::MAX, HOUR_MS).unwrap_err();
        assert!(matches!(e, DbError::UsageUnavailable(_)), "{e:?}");
        // 请求照记，加进历史那一步什么都不做
        settle(&db, &row(2, T0 + 2));
        assert_eq!(db.count().unwrap(), 2);
        drop(db);
        let conn = Connection::open(&usage).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA + 1, "版本号被改了");
        let kept: i64 = conn
            .query_row("SELECT SUM(requests) FROM hours", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, 1, "里面的行被动了");
    }

    /// 旧版本建的用量历史整个重建：原来的表丢掉，从请求库现有的行补
    #[test]
    fn a_usage_history_from_an_older_version_starts_over_from_the_records() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("data.db");
        {
            let db = Db::open(&path).unwrap();
            db.insert(&row(1, T0 + 1)).unwrap();
        }
        {
            // 同名的表、不一样的样子
            let conn = Connection::open(d.path().join(FILE)).unwrap();
            conn.execute_batch(
                "DROP TABLE hours; DROP TABLE meta;
                 CREATE TABLE hours (day INTEGER PRIMARY KEY, n INTEGER);
                 INSERT INTO hours VALUES (1, 99);",
            )
            .unwrap();
            conn.pragma_update(None, "user_version", SCHEMA - 1)
                .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let h = db.usage_history(0, i64::MAX, HOUR_MS).unwrap();
        assert_eq!(h.since_ms, Some(T0 + 1));
        assert_eq!(h.buckets.iter().map(|b| b.requests).sum::<i64>(), 1);
    }

    /// 用量历史是一个坏文件：请求库照常打开，历史这一项报错
    #[test]
    fn a_broken_usage_history_does_not_keep_the_request_store_shut() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(FILE),
            b"not a database at all, just text ......",
        )
        .unwrap();
        let db = Db::open(&d.path().join("data.db")).unwrap();
        settle(&db, &row(1, T0));
        assert_eq!(db.count().unwrap(), 1);
        assert!(matches!(
            db.usage_history(0, i64::MAX, HOUR_MS),
            Err(DbError::UsageUnavailable(_))
        ));
    }
}
