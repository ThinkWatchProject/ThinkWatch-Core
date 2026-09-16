//! 请求 metadata 的库。
//!
//! 三条贯穿这个文件的规矩：
//!
//! **一、写入永远不能挡住转发。**这一层的每一个错误都只记一行日志，
//! 不往上抛到数据面。观测挂了，代理照跑。
//!
//! **二、schema 版本用 `PRAGMA user_version`。**「库比程序新」要能识别
//! 出来并给一句人话，而不是在某个 `SELECT` 上以「no such column」告终。
//!
//! **三、成本三态。**没有价格的模型不能记成 0 —— 那是在撒谎，而一个会
//! 撒谎的成本面板不如没有。

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

/// 当前 schema 版本。**加字段就加一，并在 `migrate` 里补一步。**
const SCHEMA: i64 = 10;

/// 这一行算不出钱，**因为价目表里没有这个模型**：用量是有的，缺的是单价。
///
/// 价格页上那句「给这个模型配一个价格」能解决的只有这一种。按 token 计费
/// 的才算 —— 订阅制的那一行不是没有价格，是这笔账不在金额这个维度上。
///
/// **每一处数「没有价格」的都用它。**以前各写各的：汇总只看 `cost_micros
/// IS NULL AND billing = 'per-token'`，于是每一条失败都被数成「模型不在价目
/// 表里」；时间桶、分组和会话只看 `cost_micros IS NULL`，连订阅制也数了进去。
const NO_PRICE: &str = "(cost_micros IS NULL AND input_tokens IS NOT NULL \
                         AND billing IN ('', 'per-token'))";

/// 这一行算不出钱，**因为没有拿到用量**：上游没报，或者连接在它报之前就
/// 结束了（客户端取消、WebSocket 会话）。配价格解决不了它，而它多半花了钱，
/// 合计里缺着它 —— 所以要单独数出来，不能混进「没有价格」，也不能不数。
///
/// 只数上游确实接下了的：成功的响应（含 WebSocket 的 101）和客户端取消的。
/// 失败的不数 —— 响应开始之前的失败不计费，断在中间的会带着用量，归到上面
/// 那种；上游回了 4xx 的也不数，那种响应不计费。
const NO_USAGE: &str = "(cost_micros IS NULL AND input_tokens IS NULL AND error IS NULL \
                         AND billing IN ('', 'per-token') AND (cancelled = 1 OR status < 300))";

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
    /// 请求头透出来的旁证。**可以伪造** —— 只用来显示和判断接管有没有
    /// 生效，从不参与鉴权、路由或配额
    pub client_hint: Option<String>,
    /// 这条属于哪一次任务。**指纹 + 起始时刻**，老记录是 None
    pub session: Option<String>,
    /// 响应里有几个工具调用。`None` = 那次没开入站审查，**不是 0**
    /// —— 「没数过」和「数了是零」在画像里是完全不同的两件事
    pub tool_calls: Option<i64>,
    /// 命中了几条危险规则
    pub flagged: Option<i64>,
    /// 出站脱敏换掉了几处。`None` = 那次没开脱敏，**不是 0**
    pub redacted: Option<i64>,
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
    /// 成本是估的还是上游给的。**估算值不能混进精确数字里**
    pub cost_estimated: bool,
    pub error: Option<String>,
    /// 客户端的辅助请求被本地应答了。**不进成本和延迟统计**
    pub local: bool,
    /// 客户端没等到响应结束就走了。**和 `error` 是两件事**：它不算失败，
    /// 用量只算到断开那一刻，所以金额是估算
    pub cancelled: bool,
    /// 路由决策与尝试链，JSON。老记录是 None
    pub routing: Option<String>,
    /// 服务它的那家怎么收钱：`per-token` / `subscription` / `unknown`
    pub billing: String,
    /// 缓存命中省下了多少微分。`None` = 算不出来
    pub cache_saved_micros: Option<i64>,
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
        // **0600。**这个库里有每一条请求的模型、上游、token 数和花费 ——
        // 同一台机器上的别的用户不该能读走一份你的使用记录（那条
        // 「权限就是认证」的同一个道理）。WAL 模式还会带出两个兄弟文件，
        // 一起收。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for suffix in ["", "-wal", "-shm"] {
                let p = if suffix.is_empty() {
                    path.to_path_buf()
                } else {
                    std::path::PathBuf::from(format!("{}{suffix}", path.display()))
                };
                if p.exists() {
                    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
                }
            }
        }
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
            // 出站密钥检测的发现（观察态）。
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
        if from < 3 {
            // 路由决策与尝试链。
            //
            // **一列 JSON，不是一张表。**它是一条请求的固有事实，永远
            // 跟着那一行一起取，从来不跨行查 —— 拆出去只会多一次 join。
            self.conn
                .execute_batch("ALTER TABLE requests ADD COLUMN routing TEXT;")?;
        }
        if from < 4 {
            // 服务它的那家怎么收钱。
            //
            // **存在行上，不是事后查配置。**配置随时会被热重载，而一条
            // 三天前的记录该按它当时那家的计费方式算 —— 否则今天把一家
            // 改成订阅型，昨天的账就跟着变了。
            self.conn.execute_batch(
                "ALTER TABLE requests ADD COLUMN billing TEXT NOT NULL DEFAULT '';",
            )?;
        }
        if from < 5 {
            // 缓存命中省下了多少。
            //
            // **在记录的时候算，不在查询的时候算。**查询时算意味着要把
            // 价目表带进 SQL，而价目表会变 —— 那样「上周省了多少」会
            // 随着一次价格更新悄悄改变。
            self.conn
                .execute_batch("ALTER TABLE requests ADD COLUMN cache_saved_micros INTEGER;")?;
        }
        if from < 6 {
            // 「这条是哪个客户端发的」的旁证（观察窗口）。
            //
            // **和 `client` 分开两列，不是覆盖它。**一个不可伪造、一个
            // 可以伪造，混成一列之后就再也分不清某一行的可信度了。
            self.conn
                .execute_batch("ALTER TABLE requests ADD COLUMN client_hint TEXT;")?;
        }
        if from < 7 {
            // 会话聚合。**孤立地看单个请求看不出任何有用的东西**
            // —— Claude Code 的一次任务是几十到上百个请求。
            self.conn
                .execute_batch("ALTER TABLE requests ADD COLUMN session TEXT;")?;
            // 会话视图永远是「按会话分组、按时间倒序」，这个索引正好
            self.conn
                .execute_batch("CREATE INDEX requests_session ON requests (session, at_ms);")?;
        }
        if from < 8 {
            // 上游行为画像（防线三）。**「没数过」和「数了是零」
            // 要分得开**，所以是可空列而不是默认 0 —— 后者会让关掉审查
            // 的那段时间在画像里变成「一个工具调用都没有」，而那是假的。
            self.conn.execute_batch(
                "ALTER TABLE requests ADD COLUMN tool_calls INTEGER;
                 ALTER TABLE requests ADD COLUMN flagged INTEGER;",
            )?;
        }
        if from < 9 {
            // 出站脱敏换掉了几处（防线一的拦截档）。
            //
            // **不记的话，切到「拦截」之后面板反而没数字了** —— 观察档
            // 产出的是「检测到的外泄」，而拦截档把它们就地换掉了，于是
            // 那条证据链在防护最强的时候断掉，读起来像「什么都没发生」。
            //
            // 和 `tool_calls` 同一条理由用可空列：「没开脱敏」和
            // 「开了但这次没换」是两件事。
            self.conn
                .execute_batch("ALTER TABLE requests ADD COLUMN redacted INTEGER;")?;
        }
        if from < 10 {
            // 客户端没等到响应结束就走了的那些（Claude Code 里按 Esc）。
            //
            // **不能写进 `error`。**那一列决定失败数、失败率，还有上游行为
            // 画像里的错误率 —— 用户按了 Esc，不该让上游看起来在出错。可它
            // 也不是正常结束：用量只算到断开那一刻，界面要能把这一点说出来。
            //
            // 老记录是 0。那时候这类请求根本不落库，所以没有一条老记录
            // 该是 1。
            self.conn.execute_batch(
                "ALTER TABLE requests ADD COLUMN cancelled INTEGER NOT NULL DEFAULT 0;",
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
              cost_micros, cost_estimated, error, local, routing, billing, cache_saved_micros,
              client_hint, session, tool_calls, flagged, redacted, cancelled)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27)",
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
                r.routing,
                r.billing,
                r.cache_saved_micros,
                r.client_hint,
                r.session,
                r.tool_calls,
                r.flagged,
                r.redacted,
                r.cancelled as i64,
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
}

/// 一次任务的汇总。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    pub id: String,
    pub client: String,
    pub started_ms: i64,
    pub ended_ms: i64,
    pub turns: i64,
    /// 有价格的那些轮次加起来。**单位是微分**
    pub cost_micros: i64,
    /// 其中估算的那部分（客户端取消、断在中间、跨平台借来的价格）。
    /// **估算不能冒充实测** —— 合计里有它，界面上就得标出来
    pub cost_micros_estimated: i64,
    /// 算出了价格的轮数
    pub priced_turns: i64,
    /// **没有价格的轮数**（见 `NO_PRICE`）。三态成本的第三态在会话这一层
    /// 的样子：「$1.23」和「$1.23，另有 4 轮没有价格」是两个不同的结论
    pub unpriced_turns: i64,
    /// 没有拿到用量、所以算不出钱的轮数（见 `NO_USAGE`）
    pub no_usage_turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cache_saved_micros: i64,
    /// 上下文的峰值。**一眼看出哪次任务的上下文失控了**
    pub peak_input_tokens: i64,
    pub models: String,
    pub errors: i64,
}

/// 会话里的一轮。上下文增长曲线和成本瀑布画的就是它。
#[derive(Debug, Clone, PartialEq)]
pub struct TurnRow {
    pub id: i64,
    pub at_ms: i64,
    pub model: String,
    pub provider: String,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cost_micros: Option<i64>,
    pub duration_ms: Option<i64>,
    pub error: Option<String>,
    pub cancelled: bool,
    /// 这一轮的金额是估算。**瀑布图上要带记号**
    pub cost_estimated: bool,
}

impl Db {
    /// 按会话聚合，最近的在前。
    ///
    /// **本地应答的那些不算轮次**：它们没经过上游，把它们算进
    /// 「这次任务跑了多少轮」会让每个数字都偏大一点，而偏得毫无规律。
    pub fn sessions(&self, limit: usize) -> Result<Vec<SessionRow>, DbError> {
        let mut st = self.conn.prepare(&format!(
            "SELECT session,
                    client,
                    MIN(at_ms), MAX(at_ms), COUNT(*),
                    COALESCE(SUM(cost_micros), 0),
                    COALESCE(SUM({NO_PRICE}), 0),
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cache_read_tokens), 0),
                    COALESCE(SUM(cache_write_tokens), 0),
                    COALESCE(SUM(cache_saved_micros), 0),
                    COALESCE(MAX(input_tokens), 0),
                    GROUP_CONCAT(DISTINCT model),
                    SUM(CASE WHEN error IS NOT NULL THEN 1 ELSE 0 END),
                    COALESCE(SUM(CASE WHEN cost_estimated = 1 THEN cost_micros ELSE 0 END), 0),
                    COUNT(cost_micros),
                    COALESCE(SUM({NO_USAGE}), 0)
             FROM requests
             WHERE session IS NOT NULL AND local = 0
             GROUP BY session
             ORDER BY MAX(at_ms) DESC
             LIMIT ?1"
        ))?;
        let rows = st.query_map([limit as i64], |r| {
            Ok(SessionRow {
                id: r.get(0)?,
                client: r.get(1)?,
                started_ms: r.get(2)?,
                ended_ms: r.get(3)?,
                turns: r.get(4)?,
                cost_micros: r.get(5)?,
                unpriced_turns: r.get(6)?,
                input_tokens: r.get(7)?,
                output_tokens: r.get(8)?,
                cache_read_tokens: r.get(9)?,
                cache_write_tokens: r.get(10)?,
                cache_saved_micros: r.get(11)?,
                peak_input_tokens: r.get(12)?,
                models: r.get::<_, Option<String>>(13)?.unwrap_or_default(),
                errors: r.get(14)?,
                cost_micros_estimated: r.get(15)?,
                priced_turns: r.get(16)?,
                no_usage_turns: r.get(17)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 一次会话里的每一轮，**按时间正序** —— 曲线是从左往右画的。
    pub fn turns(&self, session: &str) -> Result<Vec<TurnRow>, DbError> {
        let mut st = self.conn.prepare(
            "SELECT id, at_ms, model, provider, input_tokens, output_tokens,
                    cache_read_tokens, cost_micros, duration_ms, error, cancelled,
                    cost_estimated
             FROM requests WHERE session = ?1 AND local = 0 ORDER BY at_ms, id",
        )?;
        let rows = st.query_map([session], |r| {
            Ok(TurnRow {
                id: r.get(0)?,
                at_ms: r.get(1)?,
                model: r.get(2)?,
                provider: r.get(3)?,
                input_tokens: r.get(4)?,
                output_tokens: r.get(5)?,
                cache_read_tokens: r.get(6)?,
                cost_micros: r.get(7)?,
                duration_ms: r.get(8)?,
                error: r.get(9)?,
                cancelled: r.get::<_, i64>(10)? != 0,
                cost_estimated: r.get::<_, i64>(11)? != 0,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

/// 一个上游在某段时间里的行为画像（防线三）。
#[derive(Debug, Clone, PartialEq)]
pub struct Shape {
    /// 这段时间里数过形状的请求有多少条。
    ///
    /// **不是「有多少条请求」** —— 关掉入站审查的那段时间没有数过，
    /// 那些条不该进画像
    pub inspected: i64,
    /// 其中有工具调用的
    pub with_tools: i64,
    /// 其中命中过危险规则的
    pub with_flags: i64,
    /// 响应字节数的中位数。**中位数不是平均数** —— 一次超长响应会把
    /// 平均数拉走，而画像要说的是「平常什么样」
    pub median_bytes: i64,
    pub errors: i64,
    /// 这段时间里的总请求数（含没数过形状的）
    pub total: i64,
}

impl Shape {
    pub fn tool_rate(&self) -> Option<f64> {
        (self.inspected > 0).then(|| self.with_tools as f64 / self.inspected as f64)
    }
    pub fn flag_rate(&self) -> Option<f64> {
        (self.inspected > 0).then(|| self.with_flags as f64 / self.inspected as f64)
    }
    pub fn error_rate(&self) -> Option<f64> {
        (self.total > 0).then(|| self.errors as f64 / self.total as f64)
    }
}

impl Db {
    /// 一个上游在 `[from_ms, to_ms)` 里的行为画像。
    ///
    /// **本地应答的不算**：它们没经过上游，混进来会稀释每一个
    /// 比率，而且稀释的幅度随用户开了几个客户端而变 —— 那种噪声没法解释。
    pub fn shape_of(&self, provider: &str, from_ms: i64, to_ms: i64) -> Result<Shape, DbError> {
        let mut st = self.conn.prepare(
            "SELECT COUNT(*),
                    SUM(CASE WHEN tool_calls IS NOT NULL THEN 1 ELSE 0 END),
                    SUM(CASE WHEN tool_calls > 0 THEN 1 ELSE 0 END),
                    SUM(CASE WHEN flagged > 0 THEN 1 ELSE 0 END),
                    SUM(CASE WHEN error IS NOT NULL THEN 1 ELSE 0 END)
             FROM requests
             WHERE provider = ?1 AND local = 0 AND at_ms >= ?2 AND at_ms < ?3",
        )?;
        let (total, inspected, with_tools, with_flags, errors) =
            st.query_row(rusqlite::params![provider, from_ms, to_ms], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                ))
            })?;
        // 中位数走 nearest-rank，和分位数同一套算法 ——
        // 两处用不同的定义会让同一份数据在两个页面上对不上
        let median_bytes = self
            .conn
            .prepare(
                "SELECT bytes FROM requests
                 WHERE provider = ?1 AND local = 0 AND bytes IS NOT NULL
                   AND at_ms >= ?2 AND at_ms < ?3
                 ORDER BY bytes LIMIT 1 OFFSET (
                     SELECT MAX(0, (COUNT(*) - 1) / 2) FROM requests
                     WHERE provider = ?1 AND local = 0 AND bytes IS NOT NULL
                       AND at_ms >= ?2 AND at_ms < ?3)",
            )?
            .query_row(rusqlite::params![provider, from_ms, to_ms], |r| r.get(0))
            .unwrap_or(0);
        Ok(Shape {
            inspected,
            with_tools,
            with_flags,
            median_bytes,
            errors,
            total,
        })
    }

    /// 历史上出现过的所有上游名字。
    pub fn providers_seen(&self) -> Result<Vec<String>, DbError> {
        let mut st = self.conn.prepare(
            "SELECT DISTINCT provider FROM requests WHERE provider != '' ORDER BY provider",
        )?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 每个客户端旁证最后一次出现是什么时候。**接管的观察窗口靠它**：
    /// 我们改了一个文件，但那个文件有没有被读到，只有请求能
    /// 证明。
    pub fn last_seen_by_hint(&self) -> Result<Vec<(String, i64)>, DbError> {
        let mut st = self.conn.prepare(
            "SELECT client_hint, MAX(at_ms) FROM requests
             WHERE client_hint IS NOT NULL GROUP BY client_hint",
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
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
    /// 最近这些天里**算不出价钱**的请求数，和它们用的模型。
    ///
    /// **这是价格页存在的理由。**用户不会主动想起要配价格 —— 只有
    /// 「有 37 条请求算不出钱，用的是这两个模型」这种具体证据才会
    /// （高级功能的触发条件要绑在「这个问题存不存在」上）。
    pub fn unpriced_recent(&self, days: i64) -> Result<(i64, Vec<String>), DbError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let since = now - days * 24 * 3600 * 1000;
        // **只数配一个价格就能解决的那些**（`NO_PRICE`）。本地应答不算（它
        // 本来就没有成本），订阅制不算（它的成本不在这个维度上）；失败的、
        // 没有用量的也不算 —— 给那个模型配价格，那几行照样算不出钱，而这一页
        // 让人去配的正是价格。
        let mut st = self.conn.prepare(&format!(
            "SELECT model, COUNT(*) FROM requests \
             WHERE at_ms >= ?1 AND local = 0 AND {NO_PRICE} \
               AND model IS NOT NULL AND model <> '' \
             GROUP BY model ORDER BY COUNT(*) DESC LIMIT 20"
        ))?;
        let rows = st.query_map([since], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut total = 0i64;
        let mut models = Vec::new();
        for row in rows {
            let (m, n) = row?;
            total += n;
            models.push(m);
        }
        Ok((total, models))
    }

    pub fn summary(&self, since_ms: i64, until_ms: i64) -> Result<Summary, DbError> {
        // **本地应答不算。**成本 0、延迟 0 的东西混进来，会让「平均延迟」
        // 和「请求数」这两个数字都失去意义。
        #[allow(clippy::type_complexity)]
        let (
            requests,
            failed,
            in_tok,
            out_tok,
            cache_r,
            cache_w,
            exact,
            estimated,
            unpriced,
            sub_reqs,
            sub_tokens,
            cache_saved,
            flagged_requests,
            redacted_requests,
            no_usage,
        ): (
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
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
            &format!(
                "SELECT
                COUNT(*),
                COALESCE(SUM(error IS NOT NULL), 0),
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_write_tokens), 0),
                COALESCE(SUM(CASE WHEN cost_estimated = 0 THEN cost_micros ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN cost_estimated = 1 THEN cost_micros ELSE 0 END), 0),
                COALESCE(SUM({NO_PRICE}), 0),
                COALESCE(SUM(billing = 'subscription'), 0),
                COALESCE(SUM(CASE WHEN billing = 'subscription'
                                  THEN COALESCE(input_tokens,0) + COALESCE(output_tokens,0)
                                  ELSE 0 END), 0),
                COALESCE(SUM(cache_saved_micros), 0),
                COALESCE(SUM(flagged > 0), 0),
                COALESCE(SUM(redacted > 0), 0),
                COALESCE(SUM({NO_USAGE}), 0)
             FROM requests
             WHERE at_ms >= ?1 AND at_ms < ?2 AND local = 0"
            ),
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
                    r.get(9)?,
                    r.get(10)?,
                    r.get(11)?,
                    r.get(12)?,
                    r.get(13)?,
                    r.get(14)?,
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
            no_usage_requests: no_usage,
            subscription_requests: sub_reqs,
            subscription_tokens: sub_tokens,
            flagged_requests,
            redacted_requests,
            cache_saved_micros: cache_saved,
        })
    }

    /// 某段时间内每个模型的延迟分位数。
    ///
    /// **用分位数不用平均值**：AI 延迟是长尾分布，平均值会被极端
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

    /// 记一次出站密钥发现（观察态）。
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
    /// relay-cn」这句话就是这么数出来的。
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

    /// 按上游分的延迟分位数。
    ///
    /// **和按模型分是两个问题。**「哪个模型慢」和「哪家上游慢」的下一步
    /// 完全不同：前者换模型，后者换上游。合成一张表的话两个问题都答不好
    /// （M3 验收里问的是后者）。
    pub fn latency_by_provider(
        &self,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Vec<Latency>, DbError> {
        let mut st = self.conn.prepare(
            "SELECT provider, ttfb_ms FROM requests
             WHERE at_ms >= ?1 AND at_ms < ?2 AND local = 0 AND ttfb_ms IS NOT NULL
               AND provider <> ''
             ORDER BY provider, ttfb_ms",
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

    /// 按时间分桶的花费与请求数（概览的趋势图）。
    ///
    /// **桶宽由调用方给，不在这里猜。**同一段数据，看「今天每小时」和
    /// 「最近 30 天每天」要的是两种桶，而在 SQL 里写死一种，另一种就得
    /// 再写一个查询。
    ///
    /// 成本三态在这里保持分开：实测的、估算的、以及**根本没有
    /// 价格的那几条的条数**。把第三种当成 0 加进柱子里，图上那根柱子
    /// 就是偏低的，而看图的人没有任何线索知道少算了什么。
    pub fn cost_buckets(
        &self,
        since_ms: i64,
        until_ms: i64,
        bucket_ms: i64,
    ) -> Result<Vec<tw_api::CostBucket>, DbError> {
        if bucket_ms <= 0 {
            return Ok(Vec::new());
        }
        let mut st = self.conn.prepare(&format!(
            "SELECT ((at_ms - ?1) / ?3) AS b,
                    COUNT(*),
                    SUM(CASE WHEN error IS NOT NULL THEN 1 ELSE 0 END),
                    COALESCE(SUM(CASE WHEN cost_estimated = 0 THEN cost_micros ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN cost_estimated = 1 THEN cost_micros ELSE 0 END), 0),
                    COALESCE(SUM({NO_PRICE}), 0),
                    COALESCE(SUM({NO_USAGE}), 0)
             FROM requests
             WHERE at_ms >= ?1 AND at_ms < ?2 AND local = 0
             GROUP BY b ORDER BY b"
        ))?;
        let rows = st.query_map(params![since_ms, until_ms, bucket_ms], |r| {
            Ok(tw_api::CostBucket {
                at_ms: since_ms + r.get::<_, i64>(0)? * bucket_ms,
                requests: r.get(1)?,
                failed: r.get(2)?,
                cost_micros_exact: r.get(3)?,
                cost_micros_estimated: r.get(4)?,
                unpriced_requests: r.get(5)?,
                no_usage_requests: r.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// 按某个维度分组的花费（钱花在哪儿）。
    ///
    /// `dim` 只接受固定的两个值 —— **不是把列名拼进 SQL**。这个参数最终
    /// 来自控制面的 query string，拼进去就是一个注入口，而它省下的那点
    /// 代码完全不值。
    /// 每个时间桶里，按模型（或上游）分开的那部分。
    ///
    /// **和 `cost_buckets` 是两个查询。**前者回答「这段时间的形状」，
    /// 这个多回答一句「每一段里是谁花的」—— 趋势图按模型分层之后，
    /// 两个问题只用看一次。
    ///
    /// 桶边界的算法和 `cost_buckets` 完全一样（相对 `since_ms` 数），
    /// 两边必须一致：界面是按同一个起点补空桶的。
    pub fn cost_buckets_by(
        &self,
        dim: tw_api::CostDim,
        since_ms: i64,
        until_ms: i64,
        bucket_ms: i64,
    ) -> Result<Vec<tw_api::CostBucketGroup>, DbError> {
        if bucket_ms <= 0 {
            return Ok(Vec::new());
        }
        let col = match dim {
            tw_api::CostDim::Model => "model",
            tw_api::CostDim::Provider => "provider",
        };
        let sql = format!(
            "SELECT ((at_ms - ?1) / ?3) AS b, {col},
                    COUNT(*),
                    SUM(CASE WHEN error IS NOT NULL THEN 1 ELSE 0 END),
                    COALESCE(SUM(CASE WHEN cost_estimated = 0 THEN cost_micros ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN cost_estimated = 1 THEN cost_micros ELSE 0 END), 0),
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cache_read_tokens), 0),
                    COALESCE(SUM(cache_write_tokens), 0)
             FROM requests
             WHERE at_ms >= ?1 AND at_ms < ?2 AND local = 0
             GROUP BY b, {col} ORDER BY b"
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(params![since_ms, until_ms, bucket_ms], |r| {
            Ok(tw_api::CostBucketGroup {
                at_ms: since_ms + r.get::<_, i64>(0)? * bucket_ms,
                name: r.get(1)?,
                requests: r.get(2)?,
                failed: r.get(3)?,
                cost_micros_exact: r.get(4)?,
                cost_micros_estimated: r.get(5)?,
                input_tokens: r.get(6)?,
                output_tokens: r.get(7)?,
                cache_read_tokens: r.get(8)?,
                cache_write_tokens: r.get(9)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn cost_by(
        &self,
        dim: tw_api::CostDim,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Vec<tw_api::CostGroup>, DbError> {
        let col = match dim {
            tw_api::CostDim::Model => "model",
            tw_api::CostDim::Provider => "provider",
        };
        let sql = format!(
            "SELECT {col}, COUNT(*),
                    COALESCE(SUM(cost_micros), 0),
                    COALESCE(SUM({NO_PRICE}), 0),
                    COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM({NO_USAGE}), 0)
             FROM requests
             WHERE at_ms >= ?1 AND at_ms < ?2 AND local = 0 AND {col} <> ''
             GROUP BY {col} ORDER BY 3 DESC"
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(params![since_ms, until_ms], |r| {
            Ok(tw_api::CostGroup {
                name: r.get(0)?,
                requests: r.get(1)?,
                cost_micros: r.get(2)?,
                unpriced_requests: r.get(3)?,
                input_tokens: r.get(4)?,
                output_tokens: r.get(5)?,
                no_usage_requests: r.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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
        client_hint: r.get("client_hint")?,
        session: r.get("session")?,
        tool_calls: r.get("tool_calls")?,
        flagged: r.get("flagged")?,
        redacted: r.get("redacted")?,
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
        cancelled: r.get::<_, i64>("cancelled")? != 0,
        routing: r.get("routing")?,
        billing: r.get("billing")?,
        cache_saved_micros: r.get("cache_saved_micros")?,
    })
}

/// 一段时间的汇总。
///
/// **实测和估算分开。**「今日 $12.40 实测 + ~$0.80 估算」比一个混在一起
/// 的 $13.20 诚实得多 —— 后者看起来是个确定的数字。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Summary {
    pub requests: i64,
    pub failed: i64,
    /// 本地应答的次数。**是个正向数字**，单独显示
    pub locally_answered: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_micros_exact: i64,
    pub cost_micros_estimated: i64,
    /// 有多少条请求**根本没有价格**（模型不在价目表里，见 `NO_PRICE`）。
    ///
    /// 这是成本三态的第三态。把它们当成 0 会让总额悄悄偏低，而用户没有
    /// 任何线索知道少算了什么。**订阅型的不算在这里** —— 那不是
    /// 「不知道价格」，是「这笔账不在这个维度上」。
    pub unpriced_requests: i64,
    /// 有多少条请求**没有拿到用量**，所以同样算不出钱（见 `NO_USAGE`）。
    /// 和上面那个分开数：两者都让总额偏低，但只有上面那个是配一个价格
    /// 就能解决的
    pub no_usage_requests: i64,
    /// 走订阅型上游的请求数。**不参与金额合计**
    pub subscription_requests: i64,
    /// 那些请求用掉的 token。**它才是订阅用户该看的量**
    pub subscription_tokens: i64,
    /// 本区间有多少个请求带回了可疑工具调用（防线三）
    pub flagged_requests: i64,
    /// 本区间有多少个请求在出站时被脱敏换过内容（防线一的拦截档）
    pub redacted_requests: i64,
    /// 缓存命中一共省下了多少微分
    pub cache_saved_micros: i64,
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

/// 「过去 7 天，有 3 个请求把你的 API key 发给了 relay-cn」。
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

    pub(super) fn row(id: i64, at_ms: i64) -> RequestRow {
        RequestRow {
            client_hint: None,
            session: None,
            tool_calls: None,
            flagged: None,
            redacted: None,
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
            cancelled: false,
            routing: None,
            billing: "per-token".into(),
            cache_saved_micros: None,
        }
    }

    /// 分桶的边界。
    ///
    /// **空桶要有,不能跳过。**没有请求的那一小时在图上是一根零高度的
    /// 柱子,不是「那一格不存在」—— 跳过的话,一天里的空档会被两边的
    /// 柱子挤没,图上看起来就是连续在用。
    ///
    /// 这条现在是**已知的不满足**:SQL 的 GROUP BY 只会产出有数据的桶。
    /// 补空桶放在调用方做,因为只有它知道要画多少格。写在这里是为了
    /// 让下一个人不要以为这个函数会给一个稠密的序列。
    #[test]
    fn cost_buckets_group_by_the_given_width() {
        let db = Db::in_memory().unwrap();
        let t0 = 1_000_000_000i64;
        let hour = 3_600_000i64;
        // 第 0 桶两条,第 2 桶一条,第 1 桶空着
        for (i, at) in [t0 + 10, t0 + 20, t0 + 2 * hour + 5].iter().enumerate() {
            let mut r = row(i as i64 + 1, *at);
            r.cost_micros = Some(1_000);
            db.insert(&r).unwrap();
        }
        let b = db.cost_buckets(t0, t0 + 3 * hour, hour).unwrap();
        assert_eq!(b.len(), 2, "只产出有数据的桶,空桶由调用方补");
        assert_eq!(
            b[0].at_ms, t0,
            "桶的起点要对齐到 since,不是第一条记录的时间"
        );
        assert_eq!(b[0].requests, 2);
        assert_eq!(b[0].cost_micros_exact, 2_000);
        assert_eq!(b[1].at_ms, t0 + 2 * hour);
    }

    /// 成本三态在桶里也要分开。
    ///
    /// 把「没有价格」当成 0 加进柱子,那根柱子就是偏低的,而看图的人
    /// 没有任何线索知道少算了什么。

    #[test]
    fn buckets_by_model_line_up_with_the_plain_buckets() {
        // **两条查询的桶边界必须完全一样。**界面是按同一个起点补空桶的，
        // 差一格就是「有数据的那一格被画在了没数据的位置上」。
        let db = Db::in_memory().unwrap();
        let t0 = 1_000_000_000i64;
        let hour = 3_600_000i64;
        for (i, (at, model)) in [
            (t0 + 10, "opus"),
            (t0 + 20, "sonnet"),
            (t0 + 2 * hour + 5, "opus"),
        ]
        .iter()
        .enumerate()
        {
            let mut r = row(i as i64 + 1, *at);
            r.model = (*model).into();
            r.cost_micros = Some(1_000);
            db.insert(&r).unwrap();
        }
        let plain = db.cost_buckets(t0, t0 + 3 * hour, hour).unwrap();
        let by = db
            .cost_buckets_by(tw_api::CostDim::Model, t0, t0 + 3 * hour, hour)
            .unwrap();
        // 第 0 桶两个模型各一条，第 2 桶一条
        assert_eq!(by.len(), 3, "两个模型在第 0 桶要分成两行：{by:?}");
        let sum: i64 = by.iter().map(|b| b.cost_micros_exact).sum();
        let plain_sum: i64 = plain.iter().map(|b| b.cost_micros_exact).sum();
        assert_eq!(sum, plain_sum, "分组之后总额要和不分组的一致");
        for b in &by {
            assert!(
                plain.iter().any(|p| p.at_ms == b.at_ms),
                "{} 这一格在不分组的结果里没有对应",
                b.at_ms
            );
        }
    }

    #[test]
    fn a_bucket_keeps_the_three_cost_states_apart() {
        let db = Db::in_memory().unwrap();
        let t0 = 1_000_000_000i64;
        let mut exact = row(1, t0 + 1);
        exact.cost_micros = Some(5_000);
        exact.cost_estimated = false;
        let mut est = row(2, t0 + 2);
        est.cost_micros = Some(3_000);
        est.cost_estimated = true;
        let mut none = row(3, t0 + 3);
        none.cost_micros = None;
        for r in [&exact, &est, &none] {
            db.insert(r).unwrap();
        }
        let b = db.cost_buckets(t0, t0 + 1000, 1000).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].cost_micros_exact, 5_000);
        assert_eq!(b[0].cost_micros_estimated, 3_000);
        assert_eq!(b[0].unpriced_requests, 1, "没有价格的要单独数,不能当成 0");
    }

    /// 分组维度只能是那两个,而且是按花费倒序 —— 「钱花在哪儿」这张图
    /// 第一眼要看到的就是最大的那一项。
    #[test]
    fn cost_by_groups_and_sorts_by_spend() {
        let db = Db::in_memory().unwrap();
        let t0 = 1_000_000_000i64;
        let mut cheap = row(1, t0 + 1);
        cheap.model = "haiku".into();
        cheap.cost_micros = Some(100);
        let mut dear = row(2, t0 + 2);
        dear.model = "opus".into();
        dear.cost_micros = Some(9_000);
        for r in [&cheap, &dear] {
            db.insert(r).unwrap();
        }
        let g = db.cost_by(tw_api::CostDim::Model, t0, t0 + 1000).unwrap();
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].name, "opus", "贵的排前面");
        assert_eq!(g[0].cost_micros, 9_000);
    }

    /// 本地应答不进任何聚合。成本 0、延迟 0 的东西混进来,
    /// 会让图上每一格都被稀释。
    #[test]
    fn local_answers_stay_out_of_the_aggregates() {
        let db = Db::in_memory().unwrap();
        let t0 = 1_000_000_000i64;
        let mut r = row(1, t0 + 1);
        r.local = true;
        r.cost_micros = None;
        db.insert(&r).unwrap();
        assert!(db.cost_buckets(t0, t0 + 1000, 1000).unwrap().is_empty());
        assert!(
            db.cost_by(tw_api::CostDim::Model, t0, t0 + 1000)
                .unwrap()
                .is_empty()
        );
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
        // 诚实得多 —— 后者看起来是个确定的数字。
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
        // 这两个数字都失去意义。
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
        // 「800ms」是 3 个样本还是 300 个，含义完全不同。
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
    fn a_session_aggregates_its_turns_and_keeps_the_unpriced_ones_visible() {
        // **「$1.23」和「$1.23，另有 4 轮没有价格」是两个不同的结论。**
        // 把没价格的当成 0 加进去，得到的是一个会撒谎的账。
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(&d.path().join("data.db")).unwrap();
        for (i, (at, cost, input)) in [
            (100, Some(1000), Some(1_000)),
            (200, Some(2000), Some(50_000)),
            (300, None, Some(120_000)),
        ]
        .into_iter()
        .enumerate()
        {
            let mut r = row(i as i64 + 1, at);
            r.session = Some("s1".into());
            r.cost_micros = cost;
            r.input_tokens = input;
            db.insert(&r).unwrap();
        }
        let s = &db.sessions(10).unwrap()[0];
        assert_eq!(s.turns, 3);
        assert_eq!(s.cost_micros, 3000);
        assert_eq!(s.unpriced_turns, 1, "没价格的那轮得单独说");
        // 一眼看出哪次任务的上下文失控了
        assert_eq!(s.peak_input_tokens, 120_000);
        assert_eq!(s.started_ms, 100);
        assert_eq!(s.ended_ms, 300);
    }

    #[test]
    fn locally_answered_probes_do_not_count_as_turns() {
        // 它们没经过上游。算进「这次任务跑了多少轮」会让每个数字都
        // 偏大一点，而偏得毫无规律。
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(&d.path().join("data.db")).unwrap();
        let mut a = row(1, 100);
        a.session = Some("s1".into());
        db.insert(&a).unwrap();
        let mut b = row(2, 200);
        b.session = Some("s1".into());
        b.local = true;
        db.insert(&b).unwrap();
        assert_eq!(db.sessions(10).unwrap()[0].turns, 1);
        assert_eq!(db.turns("s1").unwrap().len(), 1);
    }

    #[test]
    fn turns_come_back_in_time_order_because_the_curve_is_drawn_left_to_right() {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(&d.path().join("data.db")).unwrap();
        for (i, at) in [300, 100, 200].into_iter().enumerate() {
            let mut r = row(i as i64 + 1, at);
            r.session = Some("s1".into());
            db.insert(&r).unwrap();
        }
        let ts: Vec<_> = db.turns("s1").unwrap().iter().map(|t| t.at_ms).collect();
        assert_eq!(ts, [100, 200, 300]);
    }

    #[test]
    fn requests_without_a_session_are_left_out_rather_than_lumped_together() {
        // 认不出会话的请求并成一个「会话」，比没有会话视图更糟。
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(&d.path().join("data.db")).unwrap();
        db.insert(&row(1, 100)).unwrap();
        db.insert(&row(2, 200)).unwrap();
        assert!(db.sessions(10).unwrap().is_empty());
    }

    #[test]
    fn a_profile_counts_only_what_it_actually_measured() {
        // **「没数过」和「数了是零」是两件事。**关掉入站审查的那段时间
        // 没有数过形状，那些条混进画像会变成「一个工具调用都没有」——
        // 而那是假的。
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(&d.path().join("data.db")).unwrap();
        let mk = |i: i64, at: i64, tools: Option<i64>| {
            let mut r = row(i, at);
            r.provider = "relay".into();
            r.tool_calls = tools;
            r.flagged = tools.map(|_| 0);
            db.insert(&r).unwrap();
        };
        mk(1, 100, Some(2));
        mk(2, 200, Some(0));
        mk(3, 300, None); // 那时候审查是关的
        let s = db.shape_of("relay", 0, 1000).unwrap();
        assert_eq!(s.total, 3);
        assert_eq!(s.inspected, 2, "把没数过的也算进去了");
        assert_eq!(s.with_tools, 1);
        assert_eq!(s.tool_rate(), Some(0.5), "分母该是数过的那些");
    }

    #[test]
    fn locally_answered_requests_stay_out_of_the_profile() {
        // 它们没经过上游，混进来会稀释每一个比率，而稀释的幅度随用户
        // 开了几个客户端而变 —— 那种噪声没法解释。
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(&d.path().join("data.db")).unwrap();
        let mut r = row(1, 100);
        r.provider = "relay".into();
        r.tool_calls = Some(1);
        db.insert(&r).unwrap();
        let mut l = row(2, 200);
        l.provider = "relay".into();
        l.local = true;
        l.tool_calls = Some(0);
        db.insert(&l).unwrap();
        let s = db.shape_of("relay", 0, 1000).unwrap();
        assert_eq!(s.inspected, 1);
        assert_eq!(s.tool_rate(), Some(1.0));
    }

    #[test]
    fn a_window_with_nothing_in_it_reports_none_not_zero() {
        // 「这段时间里没有数据」和「这段时间里比率是 0」是两个结论。
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(&d.path().join("data.db")).unwrap();
        let s = db.shape_of("relay", 0, 1000).unwrap();
        assert_eq!(s.tool_rate(), None);
        assert_eq!(s.error_rate(), None);
    }

    #[test]
    fn the_median_is_a_median_not_a_mean() {
        // 一次超长响应会把平均数拉走，而画像要说的是「平常什么样」。
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(&d.path().join("data.db")).unwrap();
        for (i, b) in [100i64, 110, 120, 130, 1_000_000].into_iter().enumerate() {
            let mut r = row(i as i64 + 1, (i as i64 + 1) * 10);
            r.provider = "relay".into();
            r.bytes = Some(b);
            db.insert(&r).unwrap();
        }
        assert_eq!(db.shape_of("relay", 0, 1000).unwrap().median_bytes, 120);
    }

    #[test]
    fn providers_are_listed_from_what_actually_happened() {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(&d.path().join("data.db")).unwrap();
        for (i, p) in ["relay", "官方", "relay"].into_iter().enumerate() {
            let mut r = row(i as i64 + 1, 100);
            r.provider = p.into();
            db.insert(&r).unwrap();
        }
        assert_eq!(
            db.providers_seen().unwrap(),
            vec!["relay".to_string(), "官方".to_string()]
        );
    }

    #[test]
    fn the_observation_window_can_ask_when_a_client_was_last_seen() {
        // 我们改了一个文件，但那个文件有没有被读到，只有请求能证明。
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(&d.path().join("data.db")).unwrap();
        for (i, (hint, at)) in [
            (Some("codex"), 100),
            (Some("codex"), 300),
            (Some("claude-code"), 200),
            (None, 400),
        ]
        .into_iter()
        .enumerate()
        {
            let mut r = row(i as i64 + 1, at);
            r.client_hint = hint.map(|s| s.to_string());
            db.insert(&r).unwrap();
        }
        let mut got = db.last_seen_by_hint().unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![("claude-code".to_string(), 200), ("codex".to_string(), 300)]
        );
    }

    #[test]
    fn an_older_database_gains_the_new_column_without_losing_a_row() {
        // 升级时最要紧的一条：**老记录一条都不能少**。用户装新版之前
        // 那三个月的账，比这个新字段重要得多。
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("data.db");
        {
            let db = Db::open(&p).unwrap();
            db.insert(&row(1, 100)).unwrap();
            db.insert(&row(2, 200)).unwrap();
            // 装作是老版本建的库：把这一版之后加的列全撤掉。
            //
            // **每加一列都要在这里补一行。**忘了补的话，这个测试会以
            // 「duplicate column」失败 —— 那正是我们要的：它逼着人来
            // 看一眼迁移，而不是悄悄绕过去。
            db.conn
                .execute_batch(
                    "DROP INDEX requests_session;
                     ALTER TABLE requests DROP COLUMN cancelled;
                     ALTER TABLE requests DROP COLUMN redacted;
                     ALTER TABLE requests DROP COLUMN flagged;
                     ALTER TABLE requests DROP COLUMN tool_calls;
                     ALTER TABLE requests DROP COLUMN session;
                     ALTER TABLE requests DROP COLUMN client_hint;",
                )
                .unwrap();
            db.conn.pragma_update(None, "user_version", 5).unwrap();
        }
        let db = Db::open(&p).unwrap();
        assert_eq!(db.count().unwrap(), 2, "迁移把老记录弄丢了");
        let got = db.recent(10).unwrap();
        // 老记录没有旁证，那就是 None —— 不是空字符串
        assert!(got.iter().all(|r| r.client_hint.is_none()));
        // 升级之前取消的请求根本不落库，所以老记录一条都不该是「已取消」
        assert!(got.iter().all(|r| !r.cancelled));
        let mut fresh = row(3, 300);
        fresh.client_hint = Some("codex".into());
        fresh.session = Some("abc-100".into());
        db.insert(&fresh).unwrap();
        let back = &db.recent(1).unwrap()[0];
        assert_eq!(back.client_hint.as_deref(), Some("codex"));
        assert_eq!(back.session.as_deref(), Some("abc-100"));
    }

    #[test]
    fn a_database_from_a_newer_version_says_so_instead_of_failing_weirdly() {
        // **「schema 太新」要能识别出来并给一句人话**，而不是在某个
        // SELECT 上以「no such column」告终。
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

#[cfg(all(test, unix))]
mod permission_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// 这个库里有每一条请求的模型、上游、token 数和花费 —— 同一台机器
    /// 上的别的用户不该能读走一份你的使用记录。
    ///
    /// **WAL 模式会带出 `-wal` 和 `-shm` 两个兄弟文件**，而未提交的数据
    /// 就在 `-wal` 里。只收主文件等于没收。
    #[test]
    fn the_database_and_its_wal_siblings_are_not_world_readable() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("data.db");
        {
            let db = Db::open(&p).unwrap();
            db.insert(&super::tests::row(1, 100)).unwrap();
        }
        // 重新打开一次，让权限那一步也覆盖到 WAL（它是第一次写才出现的）
        let _db = Db::open(&p).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let f = std::path::PathBuf::from(format!("{}{suffix}", p.display()));
            if !f.exists() {
                continue;
            }
            let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} 的权限是 {mode:o}", f.display());
        }
    }
}

#[cfg(test)]
mod cost_state_tests {
    use super::tests::row;
    use super::*;

    /// 响应头之前就失败了：没有状态码、没有用量、没有金额。
    fn failed_before_usage(id: i64, at: i64) -> RequestRow {
        let mut r = row(id, at);
        r.error = Some("`up` 返回 502 Bad Gateway".into());
        r.status = None;
        r.input_tokens = None;
        r.output_tokens = None;
        r.cache_read_tokens = None;
        r.cost_micros = None;
        r
    }

    /// 上游回了一个正常的响应，但没有报用量。
    fn without_usage(id: i64, at: i64) -> RequestRow {
        let mut r = row(id, at);
        r.input_tokens = None;
        r.output_tokens = None;
        r.cache_read_tokens = None;
        r.cost_micros = None;
        r
    }

    /// 用量是有的，价目表里没有这个模型。
    fn unknown_model(id: i64, at: i64) -> RequestRow {
        let mut r = row(id, at);
        r.model = "中转站自己起的名字".into();
        r.cost_micros = None;
        r
    }

    /// **这条是「没有价格」被重新定义的理由。**一条失败的请求没有用量，
    /// 给它的模型配价格也算不出钱 —— 可以前每一条失败都被数成了「模型不在
    /// 价目表里」，概览上那句提示在失败多的那天格外响。
    #[test]
    fn a_failure_is_not_counted_as_a_model_without_a_price() {
        let db = Db::in_memory().unwrap();
        db.insert(&failed_before_usage(1, 100)).unwrap();
        let s = db.summary(0, 1000).unwrap();
        assert_eq!(s.failed, 1);
        assert_eq!(s.unpriced_requests, 0, "一条失败被数成了「没有价格」");
        assert_eq!(
            s.no_usage_requests, 0,
            "响应开始之前的失败不计费，钱没有缺着"
        );
    }

    /// 断在中间、带着用量的失败，模型又没有价格：**那一行的钱确实缺着**，
    /// 而且配一个价格就能补上。
    #[test]
    fn a_failure_with_usage_and_an_unknown_model_is_still_unpriced() {
        let db = Db::in_memory().unwrap();
        let mut r = unknown_model(1, 100);
        r.error = Some("流中断：上游断开了".into());
        db.insert(&r).unwrap();
        assert_eq!(db.summary(0, 1000).unwrap().unpriced_requests, 1);
    }

    /// 没有用量的那几种：**钱缺着，但缺的不是价格**。
    #[test]
    fn a_response_without_usage_is_counted_as_no_usage_not_as_no_price() {
        let db = Db::in_memory().unwrap();
        // 上游没报用量的成功响应
        db.insert(&without_usage(1, 100)).unwrap();
        // 响应头之前就被客户端取消的
        let mut early = without_usage(2, 200);
        early.status = None;
        early.cancelled = true;
        db.insert(&early).unwrap();
        // 一次 WebSocket 会话
        let mut ws = without_usage(3, 300);
        ws.status = Some(101);
        db.insert(&ws).unwrap();
        // 上游回了 400 —— 那种响应不计费，**两种都不是**
        let mut rejected = without_usage(4, 400);
        rejected.status = Some(400);
        db.insert(&rejected).unwrap();

        let s = db.summary(0, 1000).unwrap();
        assert_eq!(s.no_usage_requests, 3);
        assert_eq!(s.unpriced_requests, 0, "没有用量的被说成了「模型没有价格」");
        let b = db.cost_buckets(0, 1000, 1000).unwrap();
        assert_eq!((b[0].unpriced_requests, b[0].no_usage_requests), (0, 3));
        let g = db.cost_by(tw_api::CostDim::Model, 0, 1000).unwrap();
        assert_eq!((g[0].unpriced_requests, g[0].no_usage_requests), (0, 3));
    }

    /// 订阅制的那一行**哪一种都不是**：这笔账不在金额这个维度上。汇总早就
    /// 这么算了，而时间桶、分组和会话以前把它数成了「没有价格」。
    #[test]
    fn a_subscription_row_is_neither_anywhere() {
        let db = Db::in_memory().unwrap();
        let mut r = row(1, 100);
        r.billing = "subscription".into();
        r.cost_micros = None;
        r.session = Some("s1".into());
        db.insert(&r).unwrap();

        let b = db.cost_buckets(0, 1000, 1000).unwrap();
        assert_eq!((b[0].unpriced_requests, b[0].no_usage_requests), (0, 0));
        let g = db.cost_by(tw_api::CostDim::Model, 0, 1000).unwrap();
        assert_eq!((g[0].unpriced_requests, g[0].no_usage_requests), (0, 0));
        let s = &db.sessions(10).unwrap()[0];
        assert_eq!((s.unpriced_turns, s.no_usage_turns), (0, 0));
        assert_eq!(s.priced_turns, 0, "订阅制那一轮没有价格可言");
    }

    /// 价格页只列**配一个价格就能解决**的模型。列出一个失败了的、或者
    /// 上游没报用量的模型，用户照做了，那几行也还是算不出钱。
    #[test]
    fn the_pricing_page_lists_only_models_a_price_would_fix() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let db = Db::in_memory().unwrap();
        let mut failed = failed_before_usage(1, now);
        failed.model = "失败的那个".into();
        db.insert(&failed).unwrap();
        let mut silent = without_usage(2, now);
        silent.model = "不报用量的那个".into();
        db.insert(&silent).unwrap();
        db.insert(&unknown_model(3, now)).unwrap();

        let (n, models) = db.unpriced_recent(7).unwrap();
        assert_eq!(n, 1);
        assert_eq!(models, vec!["中转站自己起的名字".to_string()]);
    }

    /// 会话的合计里有估算，**就得说出来**；每一轮也带着自己的记号。
    #[test]
    fn a_session_says_how_much_of_its_cost_is_estimated() {
        let db = Db::in_memory().unwrap();
        let mut exact = row(1, 100);
        exact.cost_micros = Some(1_000);
        let mut estimated = row(2, 200);
        estimated.cost_micros = Some(300);
        estimated.cost_estimated = true;
        estimated.cancelled = true;
        let silent = without_usage(3, 300);
        let unknown = unknown_model(4, 400);
        for mut r in [exact, estimated, silent, unknown] {
            r.session = Some("s1".into());
            db.insert(&r).unwrap();
        }

        let s = &db.sessions(10).unwrap()[0];
        assert_eq!(s.cost_micros, 1_300);
        assert_eq!(s.cost_micros_estimated, 300, "合计里的估算部分没有单独说");
        assert_eq!(s.priced_turns, 2);
        assert_eq!(s.unpriced_turns, 1);
        assert_eq!(s.no_usage_turns, 1);
        let turns = db.turns("s1").unwrap();
        assert_eq!(
            turns.iter().map(|t| t.cost_estimated).collect::<Vec<_>>(),
            vec![false, true, false, false]
        );
    }
}
