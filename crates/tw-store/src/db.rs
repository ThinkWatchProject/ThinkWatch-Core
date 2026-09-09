//! 请求 metadata 的库（DESIGN.md §8）。
//!
//! 三条贯穿这个文件的规矩：
//!
//! **一、写入永远不能挡住转发。**这一层的每一个错误都只记一行日志，
//! 不往上抛到数据面。观测挂了，代理照跑（§4.7）。
//!
//! **二、schema 版本用 `PRAGMA user_version`。**「库比程序新」要能识别
//! 出来并给一句人话，而不是在某个 `SELECT` 上以「no such column」告终
//! （§9.7）。
//!
//! **三、成本三态。**没有价格的模型不能记成 0 —— 那是在撒谎，而一个会
//! 撒谎的成本面板不如没有（§4.3）。

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

/// 当前 schema 版本。**加字段就加一，并在 `migrate` 里补一步。**
const SCHEMA: i64 = 2;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("打不开 {path}：{source}")]
    Open {
        path: String,
        source: rusqlite::Error,
    },
    /// 库是更新版本的程序建的。**只能读不能写** —— 硬写会让那个版本的
    /// 数据变成半新半旧，而用户回到新版本时已经修不回来了。
    #[error(
        "数据库的 schema 版本是 {found}，这个版本的 twcore 只认到 {supported}。请升级应用；实在要用旧版的话，把 {path} 挪走会重新建一个空库（历史记录会看不见，但不会丢）。"
    )]
    TooNew {
        found: i64,
        supported: i64,
        path: String,
    },
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

/// 一次请求落库的样子。
///
/// **和 `tw_api::Event` 不是同一个东西**：那边是「刚刚发生了什么」，
/// 这边是「一次请求最终长什么样」—— 由四类事件缝出来的那一行。
#[derive(Debug, Clone, PartialEq)]
pub struct RequestRow {
    pub id: i64,
    pub at_ms: i64,
    pub client: String,
    pub provider: String,
    pub model: String,
    pub path: String,
    /// 没走到上游就失败时是 None
    pub status: Option<u16>,
    pub ttfb_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    pub bytes: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    /// 成本，单位是**微分**（百万分之一美元）。
    ///
    /// 用整数不用浮点：金额相加是这个字段唯一的用途，而浮点相加一万次
    /// 之后的尾差会让「今日花费」和「逐条相加」对不上 —— 那种对不上没
    /// 有任何办法解释给用户听。
    pub cost_micros: Option<i64>,
    /// 成本是估的还是上游给的。**估算值不能混进精确数字里**（§4.3）
    pub cost_estimated: bool,
    pub error: Option<String>,
    /// 客户端的辅助请求被本地应答了（§4.8）。**不进成本和延迟统计**
    pub local: bool,
}

#[derive(Debug)]
pub struct Db {
    conn: Connection,
}

impl Db {
    /// 打开（或新建）。
    pub fn open(path: &Path) -> Result<Self, DbError> {
        if let Some(d) = path.parent()
            && !d.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(d);
        }
        let conn = Connection::open(path).map_err(|source| DbError::Open {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_conn(conn, &path.display().to_string())
    }

    /// 内存库。测试用，也是「磁盘写不了」时的退路。
    pub fn in_memory() -> Result<Self, DbError> {
        Self::from_conn(Connection::open_in_memory()?, ":memory:")
    }

    fn from_conn(conn: Connection, path: &str) -> Result<Self, DbError> {
        // WAL：读不挡写。**界面在查历史的同时数据面在写** —— 默认的
        // rollback journal 下那是互相阻塞的。
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // NORMAL 而不是 FULL：崩溃时最多丢最近几条观测记录，换来的是
        // 每次写少一次 fsync。**观测数据不值得为它付 fsync 的代价** ——
        // 而配置文件那边是原子写加 fsync，因为那份丢不起。
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let found: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if found > SCHEMA {
            return Err(DbError::TooNew {
                found,
                supported: SCHEMA,
                path: path.to_string(),
            });
        }
        let db = Self { conn };
        db.migrate(found)?;
        Ok(db)
    }

    /// 从 `from` 版本升到当前。**每一步都是独立的、只往前的。**
    fn migrate(&self, from: i64) -> Result<(), DbError> {
        if from < 1 {
            self.conn.execute_batch(
                "CREATE TABLE requests (
                    id                INTEGER PRIMARY KEY,
                    at_ms             INTEGER NOT NULL,
                    client            TEXT    NOT NULL,
                    provider          TEXT    NOT NULL,
                    model             TEXT    NOT NULL DEFAULT '',
                    path              TEXT    NOT NULL,
                    status            INTEGER,
                    ttfb_ms           INTEGER,
                    duration_ms       INTEGER,
                    bytes             INTEGER,
                    input_tokens      INTEGER,
                    output_tokens     INTEGER,
                    cache_read_tokens  INTEGER,
                    cache_write_tokens INTEGER,
                    cost_micros       INTEGER,
                    cost_estimated    INTEGER NOT NULL DEFAULT 0,
                    error             TEXT,
                    local             INTEGER NOT NULL DEFAULT 0
                 );
                 -- 几乎所有查询都是「最近的 N 条」或者「某段时间内的」，
                 -- 所以时间是唯一必需的索引。按 provider / model 过滤是
                 -- 在那之上再筛，数据量小得不值得再建索引。
                 CREATE INDEX requests_at ON requests (at_ms DESC);",
            )?;
        }
        if from < 2 {
            // 出站密钥检测的发现（§5.0 的观察态）。
            //
            // **单独一张表，不是 requests 上的一列。**一次请求可能同时
            // 带出好几种凭据，而「过去 7 天有 3 个请求把 key 发给了
            // relay-cn」这句话要按 (provider, kind) 分组数。
            self.conn.execute_batch(
                "CREATE TABLE leaks (
                    id        INTEGER PRIMARY KEY AUTOINCREMENT,
                    at_ms     INTEGER NOT NULL,
                    request_id INTEGER NOT NULL,
                    provider  TEXT NOT NULL,
                    kind      TEXT NOT NULL,
                    -- **已打码。**存原文等于把泄漏搬了个家
                    masked    TEXT NOT NULL
                 );
                 CREATE INDEX leaks_at ON leaks (at_ms DESC);",
            )?;
        }
        self.conn.pragma_update(None, "user_version", SCHEMA)?;
        Ok(())
    }

    /// 记一条。
    pub fn insert(&self, r: &RequestRow) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO requests
             (id, at_ms, client, provider, model, path, status, ttfb_ms, duration_ms, bytes,
              input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
              cost_micros, cost_estimated, error, local)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            params![
                r.id,
                r.at_ms,
                r.client,
                r.provider,
                r.model,
                r.path,
                r.status,
                r.ttfb_ms,
                r.duration_ms,
                r.bytes,
                r.input_tokens,
                r.output_tokens,
                r.cache_read_tokens,
                r.cache_write_tokens,
                r.cost_micros,
                r.cost_estimated as i64,
                r.error,
                r.local as i64,
            ],
        )?;
        Ok(())
    }

    /// 最近 N 条，新的在前。
    pub fn recent(&self, limit: usize) -> Result<Vec<RequestRow>, DbError> {
        let mut st = self
            .conn
            .prepare("SELECT * FROM requests ORDER BY at_ms DESC, id DESC LIMIT ?1")?;
        let rows = st.query_map([limit as i64], row_from)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn get(&self, id: i64) -> Result<Option<RequestRow>, DbError> {
        Ok(self
            .conn
            .prepare("SELECT * FROM requests WHERE id = ?1")?
            .query_row([id], row_from)
            .optional()?)
    }

    pub fn count(&self) -> Result<i64, DbError> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))?)
    }

    /// 一段时间内的汇总。Dashboard 和「昨天花了多少钱」都问它。
    pub fn summary(&self, since_ms: i64, until_ms: i64) -> Result<Summary, DbError> {
        // **本地应答不算。**成本 0、延迟 0 的东西混进来，会让「平均延迟」
        // 和「请求数」这两个数字都失去意义（§4.8）。
        let (requests, failed, in_tok, out_tok, cache_r, cache_w, exact, estimated, unpriced): (
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
        ) = self.conn.query_row(
            "SELECT
                COUNT(*),
                COALESCE(SUM(error IS NOT NULL), 0),
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_write_tokens), 0),
                COALESCE(SUM(CASE WHEN cost_estimated = 0 THEN cost_micros ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN cost_estimated = 1 THEN cost_micros ELSE 0 END), 0),
                COALESCE(SUM(cost_micros IS NULL), 0)
             FROM requests
             WHERE at_ms >= ?1 AND at_ms < ?2 AND local = 0",
            params![since_ms, until_ms],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            },
        )?;
        let local: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM requests WHERE at_ms >= ?1 AND at_ms < ?2 AND local = 1",
            params![since_ms, until_ms],
            |r| r.get(0),
        )?;
        Ok(Summary {
            requests,
            failed,
            locally_answered: local,
            input_tokens: in_tok,
            output_tokens: out_tok,
            cache_read_tokens: cache_r,
            cache_write_tokens: cache_w,
            cost_micros_exact: exact,
            cost_micros_estimated: estimated,
            unpriced_requests: unpriced,
        })
    }

    /// 某段时间内每个模型的延迟分位数。
    ///
    /// **用分位数不用平均值**（§4.6）：AI 延迟是长尾分布，平均值会被极端
    /// 值拉偏。**样本数一起返回** —— 「800ms」是 3 个样本还是 300 个，
    /// 含义完全不同。
    pub fn latency_by_model(&self, since_ms: i64, until_ms: i64) -> Result<Vec<Latency>, DbError> {
        let mut st = self.conn.prepare(
            "SELECT model, ttfb_ms FROM requests
             WHERE at_ms >= ?1 AND at_ms < ?2 AND local = 0 AND ttfb_ms IS NOT NULL
             ORDER BY model, ttfb_ms",
        )?;
        let rows = st.query_map(params![since_ms, until_ms], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut by: std::collections::BTreeMap<String, Vec<i64>> = Default::default();
        for row in rows {
            let (m, t) = row?;
            by.entry(m).or_default().push(t);
        }
        Ok(by
            .into_iter()
            .map(|(model, xs)| Latency {
                p50: percentile(&xs, 50),
                p95: percentile(&xs, 95),
                samples: xs.len(),
                model,
            })
            .collect())
    }

    /// 记一次出站密钥发现（§5.0 的观察态）。
    pub fn insert_leak(&self, l: &Leak) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT INTO leaks (at_ms, request_id, provider, kind, masked)
             VALUES (?1,?2,?3,?4,?5)",
            params![l.at_ms, l.request_id, l.provider, l.kind, l.masked],
        )?;
        Ok(())
    }

    /// 一段时间里发生过什么。
    ///
    /// **按 (上游, 种类) 分组** —— 「有 3 个请求把你的 API key 发给了
    /// relay-cn」这句话就是这么数出来的（§5.0）。
    pub fn leak_summary(&self, since_ms: i64) -> Result<Vec<LeakGroup>, DbError> {
        let mut st = self.conn.prepare(
            "SELECT provider, kind, COUNT(DISTINCT request_id), MAX(at_ms),
                    GROUP_CONCAT(DISTINCT masked)
             FROM leaks WHERE at_ms >= ?1
             GROUP BY provider, kind
             ORDER BY COUNT(DISTINCT request_id) DESC",
        )?;
        let rows = st.query_map([since_ms], |r| {
            Ok(LeakGroup {
                provider: r.get(0)?,
                kind: r.get(1)?,
                requests: r.get(2)?,
                last_at_ms: r.get(3)?,
                masked: r
                    .get::<_, Option<String>>(4)?
                    .unwrap_or_default()
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect(),
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 删掉太老的 metadata。返回删了几条。
    pub fn prune_before(&self, cutoff_ms: i64) -> Result<usize, DbError> {
        // 发现记录跟着请求一起过期 —— 留着一条指向不存在的请求的发现，
        // 用户点「看是哪几个请求」会落空
        let _ = self
            .conn
            .execute("DELETE FROM leaks WHERE at_ms < ?1", [cutoff_ms]);
        Ok(self
            .conn
            .execute("DELETE FROM requests WHERE at_ms < ?1", [cutoff_ms])?)
    }
}

/// 排好序的样本里的第 p 百分位，**最近秩法**。
///
/// 不做线性插值：延迟本来就是毫秒粒度的整数，插出一个「843.7ms」只是
/// 假装精确。而最近秩法有一条更实际的好处 —— **返回的永远是一个真实
/// 发生过的值**，用户能在请求列表里找到它。
///
/// 两个样本的 p95 应该是较大那个：`ceil(0.95 × 2) = 2`。用
/// `(n-1)·p/100` 会给出较小那个，于是小样本下的 p95 永远偏乐观。
fn percentile(sorted: &[i64], p: usize) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let n = sorted.len();
    let rank = (p * n).div_ceil(100);
    sorted[rank.saturating_sub(1).min(n - 1)]
}

fn row_from(r: &rusqlite::Row) -> rusqlite::Result<RequestRow> {
    Ok(RequestRow {
        id: r.get("id")?,
        at_ms: r.get("at_ms")?,
        client: r.get("client")?,
        provider: r.get("provider")?,
        model: r.get("model")?,
        path: r.get("path")?,
        status: r.get::<_, Option<i64>>("status")?.map(|s| s as u16),
        ttfb_ms: r.get("ttfb_ms")?,
        duration_ms: r.get("duration_ms")?,
        bytes: r.get("bytes")?,
        input_tokens: r.get("input_tokens")?,
        output_tokens: r.get("output_tokens")?,
        cache_read_tokens: r.get("cache_read_tokens")?,
        cache_write_tokens: r.get("cache_write_tokens")?,
        cost_micros: r.get("cost_micros")?,
        cost_estimated: r.get::<_, i64>("cost_estimated")? != 0,
        error: r.get("error")?,
        local: r.get::<_, i64>("local")? != 0,
    })
}

/// 一段时间的汇总。
///
/// **实测和估算分开。**「今日 $12.40 实测 + ~$0.80 估算」比一个混在一起
/// 的 $13.20 诚实得多 —— 后者看起来是个确定的数字（§4.3）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Summary {
    pub requests: i64,
    pub failed: i64,
    /// 本地应答的次数。**是个正向数字**，单独显示（§4.8）
    pub locally_answered: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_micros_exact: i64,
    pub cost_micros_estimated: i64,
    /// 有多少条请求**根本没有价格**（模型不在价目表里）。
    ///
    /// 这是成本三态的第三态。把它们当成 0 会让总额悄悄偏低，而用户没有
    /// 任何线索知道少算了什么（§4.3）。
    pub unpriced_requests: i64,
}

/// 一次出站密钥发现。
#[derive(Debug, Clone, PartialEq)]
pub struct Leak {
    pub at_ms: i64,
    pub request_id: i64,
    pub provider: String,
    pub kind: String,
    /// **已打码。**存原文等于把泄漏搬了个家
    pub masked: String,
}

/// 「过去 7 天，有 3 个请求把你的 API key 发给了 relay-cn」（§5.0）。
#[derive(Debug, Clone, PartialEq)]
pub struct LeakGroup {
    pub provider: String,
    pub kind: String,
    pub requests: i64,
    pub last_at_ms: i64,
    pub masked: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Latency {
    pub model: String,
    pub p50: i64,
    pub p95: i64,
    /// **样本数要一起给。**「800ms」是 3 个样本还是 300 个，含义完全不同
    pub samples: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, at_ms: i64) -> RequestRow {
        RequestRow {
            id,
            at_ms,
            client: "claude-code".into(),
            provider: "官方".into(),
            model: "claude-sonnet-4-5".into(),
            path: "/v1/messages".into(),
            status: Some(200),
            ttfb_ms: Some(800),
            duration_ms: Some(4000),
            bytes: Some(12345),
            input_tokens: Some(1000),
            output_tokens: Some(500),
            cache_read_tokens: Some(200),
            cache_write_tokens: None,
            cost_micros: Some(12_000),
            cost_estimated: false,
            error: None,
            local: false,
        }
    }

    #[test]
    fn a_row_survives_a_round_trip_with_every_field_intact() {
        // 漏掉一个字段的表现是「详情页上少一个数」，而那种缺失在肉眼
        // 检查里几乎发现不了。
        let db = Db::in_memory().unwrap();
        let r = row(1, 1_000_000);
        db.insert(&r).unwrap();
        assert_eq!(db.get(1).unwrap().unwrap(), r);
    }

    #[test]
    fn the_optional_fields_stay_none_rather_than_becoming_zero() {
        // **把「不知道」记成 0 是在撒谎。**一个 ttfb 为 0 的请求会把
        // 分位数拉到地板上，而一个成本为 0 的请求会让总额偏低。
        let db = Db::in_memory().unwrap();
        let mut r = row(1, 1);
        r.status = None;
        r.ttfb_ms = None;
        r.cost_micros = None;
        r.input_tokens = None;
        db.insert(&r).unwrap();
        let got = db.get(1).unwrap().unwrap();
        assert_eq!(got.status, None);
        assert_eq!(got.ttfb_ms, None);
        assert_eq!(got.cost_micros, None);
        assert_eq!(got.input_tokens, None);
    }

    #[test]
    fn recent_gives_the_newest_first() {
        let db = Db::in_memory().unwrap();
        for i in 1..=5 {
            db.insert(&row(i, i * 1000)).unwrap();
        }
        let got: Vec<i64> = db.recent(3).unwrap().iter().map(|r| r.id).collect();
        assert_eq!(got, vec![5, 4, 3]);
    }

    #[test]
    fn the_summary_keeps_measured_and_estimated_costs_apart() {
        // 「今日 $12.40 实测 + ~$0.80 估算」比一个混在一起的 $13.20
        // 诚实得多 —— 后者看起来是个确定的数字（§4.3）。
        let db = Db::in_memory().unwrap();
        let mut a = row(1, 100);
        a.cost_micros = Some(1000);
        a.cost_estimated = false;
        let mut b = row(2, 200);
        b.cost_micros = Some(500);
        b.cost_estimated = true;
        db.insert(&a).unwrap();
        db.insert(&b).unwrap();
        let s = db.summary(0, 1000).unwrap();
        assert_eq!(s.cost_micros_exact, 1000);
        assert_eq!(s.cost_micros_estimated, 500);
    }

    #[test]
    fn a_request_with_no_price_at_all_is_counted_separately_not_as_zero() {
        // **成本三态的第三态。**当成 0 会让总额悄悄偏低，而用户没有任何
        // 线索知道少算了什么。
        let db = Db::in_memory().unwrap();
        let mut a = row(1, 100);
        a.cost_micros = Some(1000);
        let mut b = row(2, 200);
        b.cost_micros = None;
        b.model = "某个中转站自己的模型".into();
        db.insert(&a).unwrap();
        db.insert(&b).unwrap();
        let s = db.summary(0, 1000).unwrap();
        assert_eq!(s.cost_micros_exact, 1000);
        assert_eq!(s.unpriced_requests, 1, "没价格的那条要能被数出来");
        assert_eq!(s.requests, 2);
    }

    #[test]
    fn locally_answered_requests_are_counted_but_not_averaged_in() {
        // 成本 0、延迟 0 的东西混进来，会让「平均延迟」和「请求数」
        // 这两个数字都失去意义（§4.8）。
        let db = Db::in_memory().unwrap();
        db.insert(&row(1, 100)).unwrap();
        let mut probe = row(2, 200);
        probe.local = true;
        probe.ttfb_ms = Some(0);
        probe.cost_micros = Some(0);
        probe.input_tokens = Some(0);
        db.insert(&probe).unwrap();

        let s = db.summary(0, 1000).unwrap();
        assert_eq!(s.requests, 1, "本地应答混进了请求总数");
        assert_eq!(s.locally_answered, 1, "本地应答该单独有个数");
        assert_eq!(s.input_tokens, 1000, "本地应答的 0 token 混进了汇总");

        let lat = db.latency_by_model(0, 1000).unwrap();
        assert_eq!(lat.len(), 1);
        assert_eq!(lat[0].p50, 800, "本地应答的 0ms 把分位数拉到地板上了");
    }

    #[test]
    fn latency_reports_percentiles_and_the_sample_count() {
        // 「800ms」是 3 个样本还是 300 个，含义完全不同（§4.6）。
        let db = Db::in_memory().unwrap();
        for (i, t) in (1..=100).enumerate() {
            let mut r = row(i as i64 + 1, 1000);
            r.ttfb_ms = Some(t * 10);
            db.insert(&r).unwrap();
        }
        let lat = db.latency_by_model(0, 10_000).unwrap();
        assert_eq!(lat.len(), 1);
        assert_eq!(lat[0].samples, 100);
        assert_eq!(lat[0].p50, 500);
        assert_eq!(lat[0].p95, 950);
        assert!(lat[0].p95 > lat[0].p50, "p95 该比 p50 大");
    }

    #[test]
    fn one_slow_request_moves_p95_but_not_p50() {
        // **这就是不用平均值的理由。**一个 60 秒的长任务会把平均值拉到
        // 没法看，而 p50 说的仍然是「通常多快」。
        let db = Db::in_memory().unwrap();
        for i in 1..=99 {
            let mut r = row(i, 1000);
            r.ttfb_ms = Some(500);
            db.insert(&r).unwrap();
        }
        let mut slow = row(100, 1000);
        slow.ttfb_ms = Some(60_000);
        db.insert(&slow).unwrap();
        let lat = db.latency_by_model(0, 10_000).unwrap();
        assert_eq!(lat[0].p50, 500, "一个慢请求不该动 p50");
    }

    #[test]
    fn latency_is_grouped_by_model_because_mixing_them_is_meaningless() {
        let db = Db::in_memory().unwrap();
        let mut a = row(1, 1000);
        a.model = "claude-opus-4".into();
        a.ttfb_ms = Some(3000);
        let mut b = row(2, 1000);
        b.model = "claude-3-5-haiku".into();
        b.ttfb_ms = Some(300);
        db.insert(&a).unwrap();
        db.insert(&b).unwrap();
        let lat = db.latency_by_model(0, 10_000).unwrap();
        assert_eq!(lat.len(), 2);
        let opus = lat.iter().find(|l| l.model == "claude-opus-4").unwrap();
        assert_eq!(opus.p50, 3000);
    }

    #[test]
    fn a_time_window_excludes_what_is_outside_it() {
        let db = Db::in_memory().unwrap();
        db.insert(&row(1, 100)).unwrap();
        db.insert(&row(2, 500)).unwrap();
        db.insert(&row(3, 900)).unwrap();
        assert_eq!(db.summary(200, 800).unwrap().requests, 1);
        // 左闭右开 —— 「今天」和「昨天」不能都算上零点那一条
        assert_eq!(db.summary(500, 900).unwrap().requests, 1);
    }

    #[test]
    fn an_empty_window_gives_zeroes_not_an_error() {
        // Dashboard 第一次打开时就是这个状态。
        let db = Db::in_memory().unwrap();
        let s = db.summary(0, 1).unwrap();
        assert_eq!(s, Summary::default());
        assert!(db.latency_by_model(0, 1).unwrap().is_empty());
    }

    #[test]
    fn pruning_removes_only_what_is_older_than_the_cutoff() {
        let db = Db::in_memory().unwrap();
        for i in 1..=10 {
            db.insert(&row(i, i * 100)).unwrap();
        }
        assert_eq!(db.prune_before(500).unwrap(), 4);
        assert_eq!(db.count().unwrap(), 6);
    }

    #[test]
    fn a_database_from_a_newer_version_says_so_instead_of_failing_weirdly() {
        // **「schema 太新」要能识别出来并给一句人话**，而不是在某个
        // SELECT 上以「no such column」告终（§9.7）。
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("data.db");
        {
            let db = Db::open(&p).unwrap();
            db.conn.pragma_update(None, "user_version", 999).unwrap();
        }
        let e = Db::open(&p).unwrap_err();
        assert!(matches!(e, DbError::TooNew { .. }), "{e:?}");
        let m = e.to_string();
        assert!(m.contains("升级"), "{m}");
        assert!(m.contains("不会丢"), "得说清历史记录的下场：{m}");
    }

    #[test]
    fn opening_the_same_database_twice_is_idempotent() {
        // 迁移跑两遍不该出错 —— 每次启动都会跑一次。
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("data.db");
        {
            let db = Db::open(&p).unwrap();
            db.insert(&row(1, 100)).unwrap();
        }
        let db = Db::open(&p).unwrap();
        assert_eq!(db.count().unwrap(), 1);
    }

    #[test]
    fn the_same_id_written_twice_updates_rather_than_duplicating() {
        // 一次请求会由四类事件缝出来，中途可能写好几次。
        let db = Db::in_memory().unwrap();
        db.insert(&row(1, 100)).unwrap();
        let mut later = row(1, 100);
        later.duration_ms = Some(9999);
        db.insert(&later).unwrap();
        assert_eq!(db.count().unwrap(), 1);
        assert_eq!(db.get(1).unwrap().unwrap().duration_ms, Some(9999));
    }

    #[test]
    fn percentiles_of_tiny_samples_do_not_panic() {
        assert_eq!(percentile(&[], 50), 0);
        assert_eq!(percentile(&[7], 50), 7);
        assert_eq!(percentile(&[7], 95), 7);
        assert_eq!(percentile(&[1, 2], 95), 2);
    }
}
