//! 请求 metadata 的库。
//!
//! 三条贯穿这个文件的规矩：
//!
//! **一、写入永远不能挡住转发。**这一层的每一个错误都只记一行日志，
//! 不往上抛到数据面。观测挂了，代理照跑。
//!
//! **二、schema 版本用 `PRAGMA user_version`，不迁移。**版本对不上的库
//! 不去猜它长什么样：连同正文目录整个重建（见 [`crate::open`]），而不是在
//! 某个 `SELECT` 上以「no such column」告终。
//!
//! **三、成本三态。**没有价格的模型不能记成 0 —— 那是在撒谎，而一个会
//! 撒谎的成本面板不如没有。

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use tw_api::Msg;

/// 当前 schema 版本。**表的样子一变就加一，改 [`Db::create`] 里那一份。**
/// 不写迁移：项目还没有存量用户，版本对不上的库整个重建。
///
/// **一列 JSON 的样子变了也算**（比如 `routing` 多了必有的字段）：旧的那些行
/// 读出来是坏的，而读的一方会把「解不开」当成「没有」。
const SCHEMA: i64 = 20;

/// 这一行算不出钱，**因为价目表里没有这个模型**：用量是有的，缺的是单价。
///
/// 价格页上那句「给这个模型配一个价格」能解决的只有这一种。按量计费的才算
/// —— 不计费的那一行记的是确定的 $0，不会缺价格。
///
/// **每一处数「没有价格」的都用它。**以前各写各的：汇总只看 `cost_micros
/// IS NULL AND billing = 'per-token'`，于是每一条失败都被数成「模型不在价目
/// 表里」；时间桶、分组和会话只看 `cost_micros IS NULL`，数进了不该数的行。
const NO_PRICE: &str = "(cost_micros IS NULL AND input_tokens IS NOT NULL \
                         AND billing = 'per-token')";

/// 这一行算不出钱，**因为没有拿到用量**：上游没报，或者连接在它报之前就
/// 结束了（客户端取消、WebSocket 会话）。配价格解决不了它，而它多半花了钱，
/// 合计里缺着它 —— 所以要单独数出来，不能混进「没有价格」，也不能不数。
///
/// 只数上游确实接下了的：成功的响应（含 WebSocket 的 101）和客户端取消的。
/// 失败的不数 —— 响应开始之前的失败不计费，断在中间的会带着用量，归到上面
/// 那种；上游回了 4xx 的也不数，那种响应不计费。
const NO_USAGE: &str = "(cost_micros IS NULL AND input_tokens IS NULL AND error IS NULL \
                         AND billing = 'per-token' AND (cancelled = 1 OR status < 300))";

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("{path} could not be opened: {source}")]
    Open {
        path: String,
        source: rusqlite::Error,
    },
    /// 库是别的版本建的。**不迁移、也不硬读** —— [`crate::open`] 连同正文
    /// 目录整个重建。
    #[error("the database is schema version {found}; this twcore reads only {supported}")]
    OtherVersion { found: i64, supported: i64 },
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
    /// 非本机来的请求的来源地址（这条连接对面的地址）。本机来的是 None
    pub peer: Option<String>,
    /// 请求带的那把网关密钥打码后的样子，请求那一刻的
    pub key_masked: Option<String>,
    /// 这条属于哪一次任务。**指纹 + 起始时刻**。认不出会话的（WebSocket、
    /// 本地应答）是 None
    pub session: Option<String>,
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
    /// 失败的原因，带着码
    pub error: Option<Msg>,
    /// 客户端的辅助请求被本地应答了。**不进成本和延迟统计**
    pub local: bool,
    /// 客户端没等到响应结束就走了。**和 `error` 是两件事**：它不算失败，
    /// 用量只算到断开那一刻，所以金额是估算
    pub cancelled: bool,
    /// 路由决策与尝试链，JSON（`tw_api::RoutingView`）。本地应答的是 None：它们
    /// 没到规则那一层。路由还没报出结论请求就结束了的（上游应答之前客户端就走
    /// 了）也有，是开始时就知道的那几项，尝试链是空的
    pub routing: Option<String>,
    /// 服务它的那家怎么收钱：`per-token` / `free`。本地应答是 `free`
    pub billing: tw_api::Billing,
    /// 缓存命中省下了多少微分。`None` = 算不出来
    pub cache_saved_micros: Option<i64>,
    /// 金额按什么价格算的，JSON（`tw_api::PriceSourceView`）。没算出金额的
    /// 是 None
    pub price_source: Option<String>,
    /// 服务它的那一跳做过的格式转换，JSON。直通的是 None
    pub translated: Option<String>,
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
        // **先把空文件以 0600 建出来，再交给 SQLite。**让 SQLite 自己建的话，
        // 它按 umask 给 0644，下面的 `chmod` 之前别的用户已经能打开它。
        // `-wal`、`-shm` 由 SQLite 照主库的权限位建，主库对了它们就对了
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(path);
        }
        let conn = Connection::open(path).map_err(|source| DbError::Open {
            path: path.display().to_string(),
            source,
        })?;
        // **0600。**这个库里有每一条请求的模型、上游、token 数和花费 ——
        // 同一台机器上的别的用户不该能读走一份你的使用记录（那条
        // 「权限就是认证」的同一个道理）。新库上面已经建对了，这里收的是
        // 已经存在的库；WAL 模式还会带出两个兄弟文件，一起收。
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
        Self::from_conn(conn)
    }

    /// 内存库。测试用，也是「磁盘写不了」时的退路。
    pub fn in_memory() -> Result<Self, DbError> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self, DbError> {
        // WAL：读不挡写。**界面在查历史的同时数据面在写** —— 默认的
        // rollback journal 下那是互相阻塞的。
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // NORMAL 而不是 FULL：崩溃时最多丢最近几条观测记录，换来的是
        // 每次写少一次 fsync。**观测数据不值得为它付 fsync 的代价** ——
        // 而配置文件那边是原子写加 fsync，因为那份丢不起。
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let found: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if found != 0 && found != SCHEMA {
            return Err(DbError::OtherVersion {
                found,
                supported: SCHEMA,
            });
        }
        let db = Self { conn };
        if found == 0 {
            db.create()?;
        }
        Ok(db)
    }

    /// 建表。**只有这一份，没有迁移**：表的样子一变，改这里、[`SCHEMA`] 加一，
    /// 版本对不上的旧库整个重建。
    ///
    /// 放在一个事务里：建到一半断掉的话，下次打开看到的仍然是版本 0 的空库，
    /// 而不是一个缺了几张表、版本号却对得上的库。
    fn create(&self) -> Result<(), DbError> {
        self.conn.execute_batch(&format!(
            "BEGIN;
             CREATE TABLE requests (
                id                 INTEGER PRIMARY KEY,
                at_ms              INTEGER NOT NULL,
                client             TEXT    NOT NULL,
                -- 请求头透出来的旁证。**和 `client` 分开两列**：一个不可伪造、
                -- 一个可以伪造，混成一列之后就再也分不清某一行的可信度了
                client_hint        TEXT,
                -- 非本机来的请求的来源地址。**网关开给局域网之后**，几台机器共用
                -- 一把密钥时只有它分得开是谁发的；本机来的留空
                peer               TEXT,
                -- 用的是哪把密钥：打码后的样子。名字会改、密钥会换，只记名字的
                -- 话，更换之后就对不上是哪一把了
                key_masked         TEXT,
                -- 属于哪一次任务。认不出会话的请求（WebSocket、本地应答）没有
                session            TEXT,
                provider           TEXT    NOT NULL,
                model              TEXT    NOT NULL,
                path               TEXT    NOT NULL,
                status             INTEGER,
                ttfb_ms            INTEGER,
                duration_ms        INTEGER,
                bytes              INTEGER,
                input_tokens       INTEGER,
                output_tokens      INTEGER,
                cache_read_tokens  INTEGER,
                cache_write_tokens INTEGER,
                cost_micros        INTEGER,
                cost_estimated     INTEGER NOT NULL,
                -- 缓存命中省下了多少。**在记录的时候算**：查询时算要把价目表
                -- 带进 SQL，而价目表会变，「上周省了多少」会跟着悄悄改变
                cache_saved_micros INTEGER,
                -- 金额按什么价格算的。**记在行上**：事后按现在的配置去推当时
                -- 用的是哪个价，推出来的是错的
                price_source       TEXT,
                -- 服务它的那家怎么收钱。**存在行上**：今天把一家改成不计费，
                -- 昨天的账不该跟着变
                billing            TEXT    NOT NULL,
                -- 路由决策与尝试链，一列 JSON：走的路由、命中的规则、试过的上游。
                -- 详情跟着那一行一起取；各条规则命中了多少也从这里数
                routing            TEXT,
                -- 客户端和上游说不同的格式时做过的转换，以及被丢掉的字段
                translated         TEXT,
                -- 失败的原因：正文、码、参数。**码要存**：只存正文的话，翻
                -- 历史时它永远是英文
                error              TEXT,
                error_code         TEXT,
                error_args         TEXT,
                local              INTEGER NOT NULL,
                -- 客户端没等到响应结束就走了。**不能写进 `error`**：它不算失败
                cancelled          INTEGER NOT NULL
             );
             -- 几乎所有查询都是「最近的 N 条」或者「某段时间内的」
             CREATE INDEX requests_at ON requests (at_ms DESC);
             -- 会话视图永远是「按会话分组、按时间倒序」
             CREATE INDEX requests_session ON requests (session, at_ms);
             -- 安全日志。**一条是一次命中**：出站脱敏是「一个请求里的一个值」，
             -- 工具调用审查是「一个工具调用命中一条规则」
             CREATE TABLE security_events (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                at_ms      INTEGER NOT NULL,
                request_id INTEGER NOT NULL,
                guard      TEXT    NOT NULL,
                rule       TEXT    NOT NULL,
                custom     INTEGER NOT NULL,
                action     TEXT    NOT NULL,
                provider   TEXT    NOT NULL,
                client     TEXT    NOT NULL,
                tool       TEXT,
                -- **已打码或已截断。**存原文等于把泄漏搬了个家
                excerpt    TEXT    NOT NULL,
                count      INTEGER NOT NULL
             );
             CREATE INDEX security_events_at ON security_events (at_ms DESC);
             CREATE INDEX security_events_request ON security_events (request_id);
             PRAGMA user_version = {SCHEMA};
             COMMIT;"
        ))?;
        Ok(())
    }

    /// 记一条。
    pub fn insert(&self, r: &RequestRow) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO requests
             (id, at_ms, client, provider, model, path, status, ttfb_ms, duration_ms, bytes,
              input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
              cost_micros, cost_estimated, error, local, routing, billing, cache_saved_micros,
              client_hint, session, cancelled, price_source, translated,
              error_code, error_args, peer, key_masked)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30)",
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
                r.error.as_ref().map(|e| e.text.as_str()),
                r.local as i64,
                r.routing,
                r.billing.slug(),
                r.cache_saved_micros,
                r.client_hint,
                r.session,
                r.cancelled as i64,
                r.price_source,
                r.translated,
                r.error.as_ref().map(|e| e.code.as_str()),
                r.error
                    .as_ref()
                    .filter(|e| !e.args.is_empty())
                    .map(|e| serde_json::to_string(&e.args).unwrap_or_default()),
                r.peer,
                r.key_masked,
            ],
        )?;
        Ok(())
    }

    /// 已经用掉的最大请求号。库是空的时候是 0。
    ///
    /// **重启之后请求号要接着往下发。**号是进程里的一个计数器，每次
    /// 起来都从 1 开始；而写库走的是 `INSERT OR REPLACE`（一个请求要
    /// 写两次：开始一次、结束一次，见 `recorder`）。两件事凑在一起，
    /// 重启后的第一条请求就顶掉了历史上的第 1 条，第二条顶掉第 2 条
    /// —— 最老的记录一条一条地无声消失，而应用每更新一次就重启一次。
    pub fn last_request_id(&self) -> Result<u64, DbError> {
        // 空表时 MAX(id) 是 NULL，用 COALESCE 收掉
        let id: i64 =
            self.conn
                .query_row("SELECT COALESCE(MAX(id), 0) FROM requests", [], |r| {
                    r.get(0)
                })?;
        Ok(id.max(0) as u64)
    }

    /// 最近 N 条，新的在前。`within` 给了就只看那段时间。
    ///
    /// **不给时间窗不等于「今天」。**别的端点是在做聚合，「这段时间花了
    /// 多少」必须有个默认的段，而那个段该是今天；这个端点给的是一张
    /// 列表，「最近 N 条」本身就是一个完整的回答。默认成今天的话，
    /// 过了零点这张表会空掉，而那时用户什么都没做。
    pub fn recent(
        &self,
        within: Option<(i64, i64)>,
        limit: usize,
    ) -> Result<Vec<RequestRow>, DbError> {
        let (from, to) = within.unwrap_or((i64::MIN, i64::MAX));
        let mut st = self.conn.prepare(
            "SELECT * FROM requests
             WHERE at_ms >= ?1 AND at_ms <= ?2
             ORDER BY at_ms DESC, id DESC LIMIT ?3",
        )?;
        let rows = st.query_map([from, to, limit as i64], row_from)?;
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
    /// 失败的原因，带着码
    pub error: Option<Msg>,
    pub cancelled: bool,
    /// 这一轮的金额是估算。**瀑布图上要带记号**
    pub cost_estimated: bool,
    /// 服务它的那家怎么收钱（见 `RequestRow::billing`）
    pub billing: tw_api::Billing,
}

impl Db {
    /// 按会话聚合，最近的在前。
    ///
    /// **本地应答的那些不算轮次**：它们没经过上游，把它们算进
    /// 「这次任务跑了多少轮」会让每个数字都偏大一点，而偏得毫无规律。
    /// `within` 给了就只看在那段时间里活动过的会话。
    ///
    /// **筛的是会话，不是轮次。**把轮次按时间筛掉再聚合的话，一次跨过
    /// 窗口边界的任务会少算几轮、少算一截钱 —— 而「那次重构花了多少」
    /// 问的是整次任务，不是它落在某个窗口里的那一段。所以先整体聚合，
    /// 再按「有没有任何一轮落在窗口里」留下整条。
    pub fn sessions(
        &self,
        within: Option<(i64, i64)>,
        limit: usize,
    ) -> Result<Vec<SessionRow>, DbError> {
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
             HAVING MAX(at_ms) >= ?1 AND MIN(at_ms) <= ?2
             ORDER BY MAX(at_ms) DESC
             LIMIT ?3"
        ))?;
        let (from, to) = within.unwrap_or((i64::MIN, i64::MAX));
        let rows = st.query_map([from, to, limit as i64], |r| {
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
                    cost_estimated, billing, error_code, error_args
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
                error: error_from(r)?,
                cancelled: r.get::<_, i64>(10)? != 0,
                cost_estimated: r.get::<_, i64>(11)? != 0,
                billing: slug_col(r, 12, tw_api::Billing::from_slug)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

impl Db {
    /// 每把网关密钥最后一次被用在什么时候。
    ///
    /// **接管的观察窗口也靠它**：我们改了一个文件，但那个文件有没有被读到，
    /// 只有带着为那个客户端生成的密钥的请求能证明。按请求头里自报的客户端
    /// 标识分组的话，那个标识谁都能写 —— 「这把钥匙还有没有人在用」「接好了
    /// 没有」都只能按密钥问。
    pub fn last_seen_by_client(&self) -> Result<Vec<(String, i64)>, DbError> {
        let mut st = self.conn.prepare(
            "SELECT client, MAX(at_ms) FROM requests
             WHERE client <> '' GROUP BY client",
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
    pub fn unpriced_recent(&self, days: i64) -> Result<(i64, Vec<tw_api::UnpricedModel>), DbError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let since = now - days * 24 * 3600 * 1000;
        // **只数配一个价格就能解决的那些**（`NO_PRICE`）。本地应答和不计费的
        // 不算（它们记的是确定的 $0）；失败的、没有用量的也不算 —— 给那个模型配价格，那几行照样算不出钱，而这一页
        // 让人去配的正是价格。
        let filter = format!(
            "at_ms >= ?1 AND local = 0 AND {NO_PRICE} AND model IS NOT NULL AND model <> ''"
        );
        // 总数单独数：下面那个列表有上限，拿它加出来的总数会偏低
        let total: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM requests WHERE {filter}"),
            [since],
            |r| r.get(0),
        )?;
        // **按 (上游, 模型) 分。**同一个模型在不同上游按不同的价目表计价，
        // 该在哪张表里补价格取决于它走的是哪家
        let mut st = self.conn.prepare(&format!(
            "SELECT provider, model, COUNT(*) AS n FROM requests WHERE {filter} \
             GROUP BY provider, model ORDER BY n DESC, provider, model LIMIT 20"
        ))?;
        let rows = st.query_map([since], |r| {
            Ok(tw_api::UnpricedModel {
                provider: r.get(0)?,
                model: r.get(1)?,
                requests: r.get(2)?,
            })
        })?;
        Ok((total, rows.collect::<Result<_, _>>()?))
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
            cache_saved,
            no_usage,
        ): (i64, i64, i64, i64, i64, i64, i64, i64, i64, i64, i64) = self.conn.query_row(
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
                COALESCE(SUM(cache_saved_micros), 0),
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

    /// 记一条安全日志。
    pub fn insert_security_event(&self, e: &SecurityEvent) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT INTO security_events
             (at_ms, request_id, guard, rule, custom, action, provider, client, tool, excerpt, count)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                e.at_ms,
                e.request_id,
                e.guard.slug(),
                e.rule,
                e.custom as i64,
                e.action.slug(),
                e.provider,
                e.client,
                e.tool,
                e.excerpt,
                e.count,
            ],
        )?;
        Ok(())
    }

    /// 安全日志的一页，按时间倒序，连同这一段一共几条、各做了什么。
    ///
    /// 上游、密钥和模型**尽量取请求那一行的**：记录发生在请求发出之前，那时
    /// 知道的只是首选的上游，而故障转移之后真正服务它的是另一家。请求还没
    /// 落库时退回记录自己的。
    ///
    /// **总数和这一页用同一句筛选**（[`SECURITY_FILTER`]）：页头的「一共几条」
    /// 和往下翻能翻出来的是同一批。`before_id` 只管翻到哪儿，不进总数。
    pub fn security_events(
        &self,
        guard: Option<&str>,
        since_ms: i64,
        until_ms: i64,
        before_id: Option<i64>,
        limit: usize,
    ) -> Result<tw_api::SecurityEventsPage, DbError> {
        let mut st = self.conn.prepare(&format!(
            "{SECURITY_SELECT}
             WHERE {SECURITY_FILTER}
               AND (?4 IS NULL OR e.id < ?4)
             ORDER BY e.id DESC
             LIMIT ?5"
        ))?;
        let rows = st.query_map(
            params![since_ms, until_ms, guard, before_id, limit as i64 + 1],
            security_view,
        )?;
        let mut events = rows.collect::<Result<Vec<_>, _>>()?;
        let more = events.len() > limit;
        events.truncate(limit);

        let mut st = self.conn.prepare(&format!(
            "SELECT e.action, COUNT(*) FROM security_events e
             WHERE {SECURITY_FILTER}
             GROUP BY e.action"
        ))?;
        let rows = st.query_map(params![since_ms, until_ms, guard], |r| {
            Ok((
                slug_col(r, 0, tw_api::SecurityOutcome::from_slug)?,
                r.get::<_, i64>(1)?,
            ))
        })?;
        let mut by = tw_api::SecurityOutcomeCounts::default();
        for row in rows {
            let (outcome, n) = row?;
            // 逐个列出来：多一种做法时这里编译不过，而不是悄悄少数一种
            let slot = match outcome {
                tw_api::SecurityOutcome::Recorded => &mut by.recorded,
                tw_api::SecurityOutcome::Replaced => &mut by.replaced,
                tw_api::SecurityOutcome::Cut => &mut by.cut,
                tw_api::SecurityOutcome::Blocked => &mut by.blocked,
            };
            *slot = n;
        }
        Ok(tw_api::SecurityEventsPage {
            events,
            more,
            total: by.recorded + by.replaced + by.cut + by.blocked,
            by_outcome: by,
        })
    }

    /// 请求号落在 `[from, to]` 里的那些请求的安全记录，按请求号分好。
    ///
    /// 翻历史时一次取一整段：一段历史里有记录的请求很少，逐条去问是
    /// 几千次白查。
    pub fn security_of_requests(
        &self,
        from: i64,
        to: i64,
    ) -> Result<std::collections::HashMap<i64, Vec<tw_api::SecurityEventView>>, DbError> {
        let mut st = self.conn.prepare(&format!(
            "{SECURITY_SELECT}
             WHERE e.request_id >= ?1 AND e.request_id <= ?2
             ORDER BY e.id"
        ))?;
        let rows = st.query_map(params![from, to], security_view)?;
        let mut out: std::collections::HashMap<i64, Vec<tw_api::SecurityEventView>> =
            Default::default();
        for r in rows {
            let r = r?;
            out.entry(r.request_id).or_default().push(r);
        }
        Ok(out)
    }

    /// 一段时间里各项防护各留下了几条记录。**和日志数的是同一批。**
    pub fn security_counts(
        &self,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<tw_api::SecurityCounts, DbError> {
        Ok(self.conn.query_row(
            "SELECT
                COALESCE(SUM(guard = 'redact'), 0),
                COALESCE(SUM(guard = 'redact' AND action = 'replaced'), 0),
                COALESCE(SUM(guard = 'inspect_tools'), 0),
                COALESCE(SUM(guard = 'inspect_tools' AND action = 'cut'), 0),
                COALESCE(SUM(guard = 'hidden_text'), 0),
                COALESCE(SUM(guard = 'hidden_text' AND action = 'blocked'), 0),
                COALESCE(SUM(guard = 'content'), 0),
                COALESCE(SUM(guard = 'content' AND action = 'blocked'), 0),
                COALESCE(SUM(guard = 'output_limit'), 0),
                COALESCE(SUM(guard = 'output_limit' AND action = 'cut'), 0)
             FROM security_events WHERE at_ms >= ?1 AND at_ms < ?2",
            params![since_ms, until_ms],
            |r| {
                Ok(tw_api::SecurityCounts {
                    secrets: r.get(0)?,
                    secrets_replaced: r.get(1)?,
                    tool_calls: r.get(2)?,
                    tool_calls_cut: r.get(3)?,
                    hidden_text: r.get(4)?,
                    hidden_text_blocked: r.get(5)?,
                    content: r.get(6)?,
                    content_blocked: r.get(7)?,
                    output_limit: r.get(8)?,
                    output_limit_cut: r.get(9)?,
                })
            },
        )?)
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
            tw_api::CostDim::Client => "client",
        };
        // 缺着钱的两种和 `cost_buckets` 用同一对条件：每一格里各项加起来，
        // 就是那一格自己的数
        let sql = format!(
            "SELECT ((at_ms - ?1) / ?3) AS b, {col},
                    COUNT(*),
                    SUM(CASE WHEN error IS NOT NULL THEN 1 ELSE 0 END),
                    COALESCE(SUM(CASE WHEN cost_estimated = 0 THEN cost_micros ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN cost_estimated = 1 THEN cost_micros ELSE 0 END), 0),
                    COALESCE(SUM({NO_PRICE}), 0),
                    COALESCE(SUM({NO_USAGE}), 0),
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
                unpriced_requests: r.get(6)?,
                no_usage_requests: r.get(7)?,
                input_tokens: r.get(8)?,
                output_tokens: r.get(9)?,
                cache_read_tokens: r.get(10)?,
                cache_write_tokens: r.get(11)?,
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
            tw_api::CostDim::Client => "client",
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

    /// 各条路由走了多少请求、各条规则命中了多少（见 [`tw_api::RouteHits`]）。
    ///
    /// **按每一行记下的路由算**，不按现在的配置推：请求走的是它那一刻的路由和
    /// 规则。一个请求算在这几条规则上，每条只算一次：决定去向的那一条、附加了
    /// 改写的每一条、选定上游之后拒绝了它的那一条。本地应答的没有路由，不算。
    pub fn route_hits(
        &self,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Vec<tw_api::RouteHits>, DbError> {
        /// 数命中要的那几项。尝试链不用解
        #[derive(serde::Deserialize)]
        struct Routed {
            route: String,
            rule: String,
            rewritten_by: Vec<String>,
            denied_by: Option<String>,
        }
        #[derive(Default)]
        struct Tally {
            requests: i64,
            failed: i64,
            last_ms: i64,
            decided: i64,
        }
        impl Tally {
            fn add(&mut self, at_ms: i64, failed: bool) {
                self.requests += 1;
                self.failed += failed as i64;
                self.last_ms = self.last_ms.max(at_ms);
            }
        }
        let mut st = self.conn.prepare(
            "SELECT at_ms, error IS NOT NULL, routing FROM requests
             WHERE at_ms >= ?1 AND at_ms < ?2 AND local = 0 AND routing IS NOT NULL",
        )?;
        let rows = st.query_map(params![since_ms, until_ms], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, bool>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut routes: std::collections::BTreeMap<
            String,
            (Tally, std::collections::BTreeMap<String, Tally>),
        > = Default::default();
        for row in rows {
            let (at_ms, failed, json) = row?;
            // 解不开的那一行不算。**一条坏掉的记录不该让整张表出不来**
            let Ok(r) = serde_json::from_str::<Routed>(&json) else {
                continue;
            };
            let (total, rules) = routes.entry(r.route).or_default();
            total.add(at_ms, failed);
            let decider = rules.entry(r.rule.clone()).or_default();
            decider.add(at_ms, failed);
            decider.decided += 1;
            let mut counted = vec![r.rule];
            for name in r.rewritten_by.into_iter().chain(r.denied_by) {
                if !counted.contains(&name) {
                    rules.entry(name.clone()).or_default().add(at_ms, failed);
                    counted.push(name);
                }
            }
        }
        let mut out: Vec<tw_api::RouteHits> = routes
            .into_iter()
            .map(|(route, (total, rules))| {
                let mut rules: Vec<tw_api::RuleHits> = rules
                    .into_iter()
                    .map(|(rule, t)| tw_api::RuleHits {
                        rule,
                        decided: t.decided,
                        requests: t.requests,
                        failed: t.failed,
                        last_ms: t.last_ms,
                    })
                    .collect();
                rules.sort_by(|a, b| b.requests.cmp(&a.requests).then(a.rule.cmp(&b.rule)));
                tw_api::RouteHits {
                    route,
                    requests: total.requests,
                    failed: total.failed,
                    last_ms: total.last_ms,
                    rules,
                }
            })
            .collect();
        out.sort_by(|a, b| b.requests.cmp(&a.requests).then(a.route.cmp(&b.route)));
        Ok(out)
    }

    /// 删掉太老的 metadata。返回删了几条。
    pub fn prune_before(&self, cutoff_ms: i64) -> Result<usize, DbError> {
        // 安全日志跟着请求一起过期 —— 留着一条指向不存在的请求的记录，
        // 用户点「查看请求」会落空
        let _ = self
            .conn
            .execute("DELETE FROM security_events WHERE at_ms < ?1", [cutoff_ms]);
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

/// 把三列拼回一条 [`Msg`]。有正文就有码；没有参数的是 NULL。
fn error_from(r: &rusqlite::Row) -> rusqlite::Result<Option<Msg>> {
    let Some(text) = r.get::<_, Option<String>>("error")? else {
        return Ok(None);
    };
    let code = r.get::<_, String>("error_code")?;
    let args = r
        .get::<_, Option<String>>("error_args")?
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    Ok(Some(Msg { code, args, text }))
}

fn row_from(r: &rusqlite::Row) -> rusqlite::Result<RequestRow> {
    Ok(RequestRow {
        id: r.get("id")?,
        at_ms: r.get("at_ms")?,
        client: r.get("client")?,
        client_hint: r.get("client_hint")?,
        peer: r.get("peer")?,
        key_masked: r.get("key_masked")?,
        session: r.get("session")?,
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
        error: error_from(r)?,
        local: r.get::<_, i64>("local")? != 0,
        cancelled: r.get::<_, i64>("cancelled")? != 0,
        routing: r.get("routing")?,
        billing: slug_col(r, "billing", tw_api::Billing::from_slug)?,
        cache_saved_micros: r.get("cache_saved_micros")?,
        price_source: r.get("price_source")?,
        translated: r.get("translated")?,
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
    /// 任何线索知道少算了什么。
    pub unpriced_requests: i64,
    /// 有多少条请求**没有拿到用量**，所以同样算不出钱（见 `NO_USAGE`）。
    /// 和上面那个分开数：两者都让总额偏低，但只有上面那个是配一个价格
    /// 就能解决的
    pub no_usage_requests: i64,
    /// 缓存命中一共省下了多少微分
    pub cache_saved_micros: i64,
}

/// 安全日志的一条（写入用）。
#[derive(Debug, Clone, PartialEq)]
pub struct SecurityEvent {
    pub at_ms: i64,
    pub request_id: i64,
    pub guard: tw_api::Guard,
    /// 内置规则的 id，或者自定义规则的名字
    pub rule: String,
    pub custom: bool,
    pub action: tw_api::SecurityOutcome,
    pub provider: String,
    pub client: String,
    pub tool: Option<String>,
    /// **已打码或已截断**
    pub excerpt: String,
    pub count: i64,
}

/// 读安全日志时的那段 SELECT。**上游、密钥、模型优先取请求那一行的。**
const SECURITY_SELECT: &str =
    "SELECT e.id, e.at_ms, e.request_id, e.guard, e.rule, e.custom, e.action,
        COALESCE(NULLIF(r.provider, ''), e.provider),
        COALESCE(NULLIF(r.client, ''), e.client),
        COALESCE(r.model, ''),
        e.tool, e.excerpt, e.count,
        r.client_hint, r.peer, r.key_masked
     FROM security_events e LEFT JOIN requests r ON r.id = e.request_id";

/// 安全日志按什么筛：`?1`–`?2` 这一段时间，`?3` 这一项（NULL 是全部）。
///
/// **读一页和数总数用的是这一句**，两句 SQL 各写一份条件的话，迟早有一边多
/// 一个少一个，页头的数就和列表对不上了。只看 `e` 的列：连上请求那一行不会
/// 多出或少掉一条记录。
const SECURITY_FILTER: &str = "e.at_ms >= ?1 AND e.at_ms < ?2 AND (?3 IS NULL OR e.guard = ?3)";

/// 存成词的一列读回契约里的枚举。不认得的词是这一行坏了
fn slug_col<T, I: rusqlite::RowIndex>(
    r: &rusqlite::Row,
    i: I,
    from_slug: fn(&str) -> Option<T>,
) -> rusqlite::Result<T> {
    let w: String = r.get(i)?;
    from_slug(&w).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("`{w}` is not one of the stored words").into(),
        )
    })
}

fn security_view(r: &rusqlite::Row) -> rusqlite::Result<tw_api::SecurityEventView> {
    Ok(tw_api::SecurityEventView {
        id: r.get(0)?,
        at_ms: r.get(1)?,
        request_id: r.get(2)?,
        guard: slug_col(r, 3, tw_api::Guard::from_slug)?,
        rule: r.get(4)?,
        custom: r.get::<_, i64>(5)? != 0,
        action: slug_col(r, 6, tw_api::SecurityOutcome::from_slug)?,
        provider: r.get(7)?,
        client: r.get(8)?,
        model: r.get(9)?,
        tool: r.get(10)?,
        excerpt: r.get(11)?,
        count: r.get(12)?,
        client_hint: r.get(13)?,
        peer: r.get(14)?,
        key_masked: r.get(15)?,
    })
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
pub(crate) mod tests {
    use super::*;

    /// 一条失败原因。码随便取一个，这里测的不是它
    pub(crate) fn upstream_failed(text: &str) -> Msg {
        Msg {
            code: "t.upstream_failed".into(),
            args: Default::default(),
            text: text.into(),
        }
    }

    pub(crate) fn row(id: i64, at_ms: i64) -> RequestRow {
        RequestRow {
            key_masked: None,
            peer: None,
            client_hint: None,
            session: None,
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
            billing: tw_api::Billing::PerToken,
            cache_saved_micros: None,
            price_source: None,
            translated: None,
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
    fn a_window_narrows_the_history_and_no_window_means_the_most_recent() {
        let db = Db::in_memory().unwrap();
        for (id, at) in [(1, 1_000), (2, 5_000), (3, 9_000)] {
            db.insert(&row(id, at)).unwrap();
        }
        let ids = |w| {
            let mut v: Vec<i64> = db.recent(w, 10).unwrap().iter().map(|r| r.id).collect();
            v.sort_unstable();
            v
        };
        // **不给窗口不是「今天」。**列表的默认答案是「最近 N 条」
        assert_eq!(ids(None), vec![1, 2, 3]);
        assert_eq!(ids(Some((4_000, 6_000))), vec![2]);
        // 两端都是闭区间 —— 边界上那一条属于窗口里
        assert_eq!(ids(Some((5_000, 9_000))), vec![2, 3]);
        assert!(ids(Some((20_000, 30_000))).is_empty());
    }

    #[test]
    fn a_session_that_straddles_the_window_is_kept_whole() {
        /*
          **筛的是会话，不是轮次。**把轮次先按时间筛掉再聚合的话，一次
          跨过边界的任务会少算几轮、少算一截钱 —— 而「那次重构花了多少」
          问的是整次任务，不是它落在某个窗口里的那一段。
        */
        let db = Db::in_memory().unwrap();
        for (id, at) in [(1, 1_000), (2, 5_000), (3, 9_000)] {
            let mut r = row(id, at);
            r.session = Some("s".into());
            db.insert(&r).unwrap();
        }
        // 窗口只盖住中间那一轮，整条会话照样在，而且三轮都算上了
        let got = db.sessions(Some((4_000, 6_000)), 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].turns, 3, "跨边界的会话被截断了");
        assert_eq!(got[0].started_ms, 1_000);
        assert_eq!(got[0].ended_ms, 9_000);

        // 完全错开的窗口才筛得掉它
        assert!(db.sessions(Some((20_000, 30_000)), 10).unwrap().is_empty());
    }

    #[test]
    fn the_last_request_id_is_where_the_next_run_has_to_start() {
        let db = Db::in_memory().unwrap();
        assert_eq!(db.last_request_id().unwrap(), 0, "空库不该报一个假的号");
        db.insert(&row(1, 1)).unwrap();
        db.insert(&row(7, 2)).unwrap();
        db.insert(&row(3, 3)).unwrap();
        // **最大的那个，不是最后写进去的那个。**乱序写入照样成立
        assert_eq!(db.last_request_id().unwrap(), 7);
    }

    #[test]
    fn reusing_a_request_id_overwrites_the_older_record() {
        /*
          这一条钉住的是**问题本身**，不是修法：写库走的是
          `INSERT OR REPLACE`（一个请求要写两次，见 `recorder`），所以
          重号就是覆盖。计数器每次起来都从 1 开始，两件事凑在一起，
          重启后的第一条请求会顶掉历史上的第 1 条。

          `last_request_id` 存在的全部理由就是让这件事不发生。
        */
        let db = Db::in_memory().unwrap();
        let old = row(1, 1_000);
        db.insert(&old).unwrap();
        let mut newer = row(1, 9_000);
        newer.model = "qwen3:8b".into();
        db.insert(&newer).unwrap();
        assert_eq!(db.recent(None, 10).unwrap().len(), 1, "老的那条没了");
        assert_eq!(db.get(1).unwrap().unwrap().model, "qwen3:8b");
        // 从库里问一次号就够躲开：下一条该用 2
        assert_eq!(db.last_request_id().unwrap(), 1);
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
        let got: Vec<i64> = db.recent(None, 3).unwrap().iter().map(|r| r.id).collect();
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
        let s = &db.sessions(None, 10).unwrap()[0];
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
        assert_eq!(db.sessions(None, 10).unwrap()[0].turns, 1);
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
        assert!(db.sessions(None, 10).unwrap().is_empty());
    }

    #[test]
    fn the_observation_window_asks_by_key_not_by_what_the_headers_claim() {
        // 我们改了一个文件，但那个文件有没有被读到，只有请求能证明 —— 而且是
        // 带着那把密钥的请求。自报的标识不算数
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(&d.path().join("data.db")).unwrap();
        for (i, (client, hint, at)) in [
            ("codex", Some("codex"), 100),
            ("codex", None, 300),
            ("default", Some("claude-code"), 200),
        ]
        .into_iter()
        .enumerate()
        {
            let mut r = row(i as i64 + 1, at);
            r.client = client.to_string();
            r.client_hint = hint.map(|s| s.to_string());
            db.insert(&r).unwrap();
        }
        let mut got = db.last_seen_by_client().unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![("codex".to_string(), 300), ("default".to_string(), 200)]
        );
    }

    #[test]
    fn a_database_from_another_version_is_refused_rather_than_read() {
        // 不迁移，也不硬读：版本对不上就说出来，由 `crate::open` 整个重建
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("data.db");
        for found in [SCHEMA - 1, SCHEMA + 1] {
            {
                let db = Db::open(&p).unwrap();
                db.conn.pragma_update(None, "user_version", found).unwrap();
            }
            let e = Db::open(&p).unwrap_err();
            assert!(
                matches!(e, DbError::OtherVersion { found: f, .. } if f == found),
                "{e:?}"
            );
            std::fs::remove_file(&p).unwrap();
        }
    }

    #[test]
    fn opening_the_same_database_twice_is_idempotent() {
        // 每次启动都会打开一次，建过的表不该再建。
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
    use super::tests::{row, upstream_failed};
    use super::*;

    /// 响应头之前就失败了：没有状态码、没有用量、没有金额。
    fn failed_before_usage(id: i64, at: i64) -> RequestRow {
        let mut r = row(id, at);
        r.error = Some(upstream_failed("`up` 返回 502 Bad Gateway"));
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
        r.error = Some(upstream_failed("流中断：上游断开了"));
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
        let bg = db
            .cost_buckets_by(tw_api::CostDim::Model, 0, 1000, 1000)
            .unwrap();
        assert_eq!((bg[0].unpriced_requests, bg[0].no_usage_requests), (0, 3));
    }

    /// **钱缺着的只有按量计费的那些。**不计费的，没有用量也好、模型不在价目表
    /// 里也好，哪一种都不是：这笔账本来就不按价目表算。
    #[test]
    fn only_a_per_token_row_can_be_missing_its_money() {
        let db = Db::in_memory().unwrap();
        for (i, billing) in [tw_api::Billing::PerToken, tw_api::Billing::Free]
            .into_iter()
            .enumerate()
        {
            let id = i as i64 * 2 + 1;
            let mut no_usage = without_usage(id, 100);
            no_usage.billing = billing;
            db.insert(&no_usage).unwrap();
            let mut no_price = unknown_model(id + 1, 100);
            no_price.billing = billing;
            db.insert(&no_price).unwrap();
        }

        let s = db.summary(0, 1000).unwrap();
        assert_eq!((s.unpriced_requests, s.no_usage_requests), (1, 1));
        let b = db.cost_buckets(0, 1000, 1000).unwrap();
        assert_eq!((b[0].unpriced_requests, b[0].no_usage_requests), (1, 1));
        let g = db.cost_by(tw_api::CostDim::Model, 0, 1000).unwrap();
        let total = |f: fn(&tw_api::CostGroup) -> i64| g.iter().map(f).sum::<i64>();
        assert_eq!(
            (
                total(|g| g.unpriced_requests),
                total(|g| g.no_usage_requests)
            ),
            (1, 1)
        );
        let bg = db
            .cost_buckets_by(tw_api::CostDim::Model, 0, 1000, 1000)
            .unwrap();
        assert_eq!(
            (
                bg.iter().map(|g| g.unpriced_requests).sum::<i64>(),
                bg.iter().map(|g| g.no_usage_requests).sum::<i64>()
            ),
            (1, 1)
        );
        assert_eq!(db.unpriced_recent(365_000).unwrap().0, 1);
    }

    /// 分组的每一格**自己说缺着多少钱**，三个维度都是。
    ///
    /// 概览按模型分层的图上，一个模型那一格的金额是 0：没有价格、没有用量、
    /// 还是确实不花钱，是三句不同的话。只有整格的数的话，说得出这一格缺着
    /// 钱，说不出缺在哪个模型上。
    #[test]
    fn every_group_in_a_bucket_says_how_much_of_its_money_is_missing() {
        use std::collections::BTreeMap;
        use tw_api::CostDim;

        let db = Db::in_memory().unwrap();
        let hour = 3_600_000i64;
        let t0 = 1_000_000_000i64;
        // (模型, 上游, 密钥) 每一行各不相同地交叉开，三个维度分出来的组都不一样
        let set = |mut r: RequestRow, model: &str, provider: &str, client: &str| {
            r.model = model.into();
            r.provider = provider.into();
            r.client = client.into();
            r
        };
        let mut free = unknown_model(7, t0 + hour + 3);
        free.billing = tw_api::Billing::Free;
        for r in [
            // 第 0 格
            set(row(1, t0 + 1), "opus", "官方", "alice"),
            set(unknown_model(2, t0 + 2), "自起名", "中转", "alice"),
            set(without_usage(3, t0 + 3), "opus", "中转", "bob"),
            set(failed_before_usage(4, t0 + 4), "opus", "官方", "bob"),
            // 第 1 格
            set(unknown_model(5, t0 + hour + 1), "自起名", "官方", "bob"),
            set(without_usage(6, t0 + hour + 2), "opus", "官方", "alice"),
            // 不计费的：模型不在价目表里也不缺钱，这一组是真的 $0
            set(free, "自起名", "中转", "bob"),
        ] {
            db.insert(&r).unwrap();
        }

        let missing = |dim| {
            db.cost_buckets_by(dim, t0, t0 + 2 * hour, hour)
                .unwrap()
                .into_iter()
                .map(|g| {
                    (
                        ((g.at_ms - t0) / hour, g.name),
                        (g.unpriced_requests, g.no_usage_requests),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        let want = |xs: [(i64, &str, (i64, i64)); 4]| {
            xs.into_iter()
                .map(|(b, name, n)| ((b, name.to_string()), n))
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(
            missing(CostDim::Model),
            want([
                (0, "opus", (0, 1)),
                (0, "自起名", (1, 0)),
                (1, "opus", (0, 1)),
                (1, "自起名", (1, 0)),
            ])
        );
        assert_eq!(
            missing(CostDim::Provider),
            want([
                (0, "官方", (0, 0)),
                (0, "中转", (1, 1)),
                (1, "官方", (1, 1)),
                (1, "中转", (0, 0)),
            ])
        );
        assert_eq!(
            missing(CostDim::Client),
            want([
                (0, "alice", (1, 0)),
                (0, "bob", (0, 1)),
                (1, "alice", (0, 1)),
                (1, "bob", (1, 0)),
            ])
        );

        // 每一格里各组加起来，就是不分组那一格自己的数
        let plain = db.cost_buckets(t0, t0 + 2 * hour, hour).unwrap();
        assert_eq!(plain.len(), 2);
        for dim in [CostDim::Model, CostDim::Provider, CostDim::Client] {
            let by = db.cost_buckets_by(dim, t0, t0 + 2 * hour, hour).unwrap();
            for b in &plain {
                let here = by.iter().filter(|g| g.at_ms == b.at_ms);
                let sum = here.fold((0, 0), |(p, u), g| {
                    (p + g.unpriced_requests, u + g.no_usage_requests)
                });
                assert_eq!(
                    sum,
                    (b.unpriced_requests, b.no_usage_requests),
                    "{dim:?} 在 {} 这一格加起来和不分组的对不上",
                    b.at_ms
                );
            }
        }

        // 一条都没有的一段：没有组，不是一组零
        assert!(
            db.cost_buckets_by(CostDim::Model, t0 + 5 * hour, t0 + 6 * hour, hour)
                .unwrap()
                .is_empty()
        );
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
        assert_eq!(
            models,
            vec![tw_api::UnpricedModel {
                provider: "官方".into(),
                model: "中转站自己起的名字".into(),
                requests: 1,
            }]
        );
    }

    /// 列表有上限，总数没有。
    #[test]
    fn the_unpriced_total_counts_every_request_not_just_the_listed_models() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let db = Db::in_memory().unwrap();
        for i in 0..25 {
            let mut r = unknown_model(i + 1, now);
            r.model = format!("m{i}");
            db.insert(&r).unwrap();
        }
        let (n, models) = db.unpriced_recent(7).unwrap();
        assert_eq!(models.len(), 20);
        assert_eq!(n, 25);
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

        let s = &db.sessions(None, 10).unwrap()[0];
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

#[cfg(test)]
mod security_log_tests {
    use super::*;
    use tw_api::{Guard, SecurityOutcome, SecurityOutcomeCounts};

    fn event(at_ms: i64, guard: Guard, action: SecurityOutcome) -> SecurityEvent {
        SecurityEvent {
            at_ms,
            request_id: at_ms,
            guard,
            rule: "r".into(),
            custom: false,
            action,
            provider: "官方".into(),
            client: "default".into(),
            tool: None,
            excerpt: "…".into(),
            count: 1,
        }
    }

    /// 九条，时刻 1–9，号也是 1–9。只记录 3 条、已替换 1 条、已切断 3 条、
    /// 被拒 2 条
    fn seeded() -> Db {
        let db = Db::in_memory().unwrap();
        for (at, guard, action) in [
            (1, Guard::Redact, SecurityOutcome::Recorded),
            (2, Guard::Redact, SecurityOutcome::Replaced),
            (3, Guard::InspectTools, SecurityOutcome::Cut),
            (4, Guard::Redact, SecurityOutcome::Recorded),
            (5, Guard::HiddenText, SecurityOutcome::Blocked),
            (6, Guard::Content, SecurityOutcome::Recorded),
            (7, Guard::InspectTools, SecurityOutcome::Cut),
            (8, Guard::Content, SecurityOutcome::Blocked),
            (9, Guard::OutputLimit, SecurityOutcome::Cut),
        ] {
            db.insert_security_event(&event(at, guard, action)).unwrap();
        }
        db
    }

    fn ids(p: &tw_api::SecurityEventsPage) -> Vec<i64> {
        p.events.iter().map(|e| e.id).collect()
    }

    /// 页头的数是**整段的**。以前只能拿读到的条数去数，读满一页就只能写
    /// 「100+」。往下翻页也不变：`before` 是翻到哪儿，不是筛选。
    #[test]
    fn the_total_counts_the_whole_window_not_just_the_page() {
        let db = seeded();
        let all = SecurityOutcomeCounts {
            recorded: 3,
            replaced: 1,
            cut: 3,
            blocked: 2,
        };

        let p = db.security_events(None, 0, i64::MAX, None, 4).unwrap();
        assert_eq!(ids(&p), vec![9, 8, 7, 6]);
        assert!(p.more);
        assert_eq!(p.total, 9, "数的是这一页，不是这一段");
        assert_eq!(p.by_outcome, all);

        let p = db.security_events(None, 0, i64::MAX, Some(6), 4).unwrap();
        assert_eq!(ids(&p), vec![5, 4, 3, 2]);
        assert!(p.more);
        assert_eq!((p.total, &p.by_outcome), (9, &all), "翻到第二页，总数变了");

        let p = db.security_events(None, 0, i64::MAX, Some(2), 4).unwrap();
        assert_eq!(ids(&p), vec![1]);
        assert!(!p.more);
        assert_eq!((p.total, &p.by_outcome), (9, &all));
    }

    /// **总数就是一页一页翻到底翻得出来的那些**，按哪一项、哪一段筛都一样：
    /// 两句 SQL 用的是同一句筛选。
    #[test]
    fn the_total_is_what_paging_to_the_end_yields_under_every_filter() {
        let db = seeded();
        for (guard, since, until) in [
            (None, 0, i64::MAX),
            (Some("redact"), 0, i64::MAX),
            (None, 3, 7),
            (Some("inspect_tools"), 0, 8),
            (Some("content"), 7, 100),
            // 输出长度那条在 9，终点不含：一条都没有
            (Some("output_limit"), 0, 9),
        ] {
            let first = db.security_events(guard, since, until, None, 2).unwrap();
            let mut seen = first.events.clone();
            let mut last = first.clone();
            while last.more {
                let before = last.events.last().unwrap().id;
                last = db
                    .security_events(guard, since, until, Some(before), 2)
                    .unwrap();
                assert_eq!(
                    (last.total, &last.by_outcome),
                    (first.total, &first.by_outcome),
                    "{guard:?} {since}..{until}：翻页之后总数变了"
                );
                seen.extend(last.events.iter().cloned());
            }
            let mut counted = SecurityOutcomeCounts::default();
            for e in &seen {
                match e.action {
                    SecurityOutcome::Recorded => counted.recorded += 1,
                    SecurityOutcome::Replaced => counted.replaced += 1,
                    SecurityOutcome::Cut => counted.cut += 1,
                    SecurityOutcome::Blocked => counted.blocked += 1,
                }
            }
            assert_eq!(
                first.total,
                seen.len() as i64,
                "{guard:?} {since}..{until}：总数和翻得出来的条数对不上"
            );
            assert_eq!(first.by_outcome, counted, "{guard:?} {since}..{until}");
        }
    }

    /// 一条都没有：零条，四项都在、都是 0。
    #[test]
    fn an_empty_window_counts_zero_of_everything() {
        for db in [Db::in_memory().unwrap(), seeded()] {
            let p = db.security_events(None, 100, 200, None, 10).unwrap();
            assert!(p.events.is_empty());
            assert!(!p.more);
            assert_eq!(p.total, 0);
            assert_eq!(p.by_outcome, SecurityOutcomeCounts::default());
        }
    }
}

#[cfg(test)]
mod route_hits_tests {
    use super::tests::{row, upstream_failed};
    use super::*;

    /// 一个经过路由的请求落库的样子。
    fn routed(
        id: i64,
        at_ms: i64,
        route: &str,
        rule: &str,
        rewritten_by: &[&str],
        denied_by: Option<&str>,
    ) -> RequestRow {
        let mut r = row(id, at_ms);
        r.routing = Some(
            serde_json::to_string(&tw_api::RoutingView {
                route: route.into(),
                rule: rule.into(),
                group: None,
                rewritten_by: rewritten_by.iter().map(|s| s.to_string()).collect(),
                denied_by: denied_by.map(str::to_string),
                attempts: vec![],
            })
            .unwrap(),
        );
        r
    }

    fn hits<'a>(route: &'a tw_api::RouteHits, rule: &str) -> &'a tw_api::RuleHits {
        route
            .rules
            .iter()
            .find(|h| h.rule == rule)
            .unwrap_or_else(|| panic!("`{rule}` 不在 {route:?} 里"))
    }

    /// 每条路由走了多少、每条规则命中了多少、多少失败了、最后一次是什么时候。
    ///
    /// **一个请求算在它命中的每条规则上，每条只算一次**：决定去向的、附加了改写
    /// 的、选定上游之后拒绝了它的。被规则拒绝的也算命中 —— 它们是失败。
    #[test]
    fn each_route_and_rule_counts_the_requests_it_matched() {
        let db = Db::in_memory().unwrap();
        // 「工作」：两个走官方，其中一个还被关了思考、上游失败了
        db.insert(&routed(1, 100, "工作", "opus 走官方", &[], None))
            .unwrap();
        let mut failed = routed(2, 300, "工作", "opus 走官方", &["关掉思考"], None);
        failed.error = Some(upstream_failed("503"));
        db.insert(&failed).unwrap();
        // 决定去向的那条自己也带着改写：只算一次
        db.insert(&routed(3, 200, "工作", "兜底", &["兜底"], None))
            .unwrap();
        // 规则拒绝了（第一阶段）：命中了，失败了
        let mut denied = routed(4, 400, "工作", "不许用 haiku", &[], None);
        denied.error = Some(upstream_failed("denied"));
        db.insert(&denied).unwrap();
        // 选定上游之后被拒（第二阶段）：两条规则都命中了
        let mut late = routed(5, 500, "默认", "兜底", &[], Some("中转不收密钥"));
        late.error = Some(upstream_failed("denied"));
        db.insert(&late).unwrap();

        let got = db.route_hits(0, 1_000).unwrap();
        assert_eq!(
            got.iter().map(|r| r.route.as_str()).collect::<Vec<_>>(),
            ["工作", "默认"],
            "走得多的在前"
        );
        let work = &got[0];
        assert_eq!((work.requests, work.failed, work.last_ms), (4, 2, 400));
        let opus = hits(work, "opus 走官方");
        assert_eq!(
            (opus.decided, opus.requests, opus.failed, opus.last_ms),
            (2, 2, 1, 300)
        );
        let thinking = hits(work, "关掉思考");
        assert_eq!(
            (thinking.decided, thinking.requests, thinking.failed),
            (0, 1, 1),
            "只附加改写的规则也有命中，只是没决定去向"
        );
        let catch_all = hits(work, "兜底");
        assert_eq!((catch_all.decided, catch_all.requests), (1, 1));
        let deny = hits(work, "不许用 haiku");
        assert_eq!((deny.decided, deny.requests, deny.failed), (1, 1, 1));
        assert_eq!(
            work.rules
                .iter()
                .map(|h| h.rule.as_str())
                .collect::<Vec<_>>(),
            ["opus 走官方", "不许用 haiku", "兜底", "关掉思考"],
            "命中多的在前，一样多按名字"
        );

        let default = &got[1];
        assert_eq!((default.requests, default.failed), (1, 1));
        assert_eq!(hits(default, "兜底").decided, 1);
        let late_deny = hits(default, "中转不收密钥");
        assert_eq!(
            (late_deny.decided, late_deny.requests, late_deny.failed),
            (0, 1, 1)
        );
    }

    /// 窗口外的、本地应答的、没有路由的不算；**一条解不开的记录不挡住别的**
    #[test]
    fn only_routed_requests_inside_the_window_count() {
        let db = Db::in_memory().unwrap();
        db.insert(&routed(1, 50, "默认", "兜底", &[], None))
            .unwrap();
        db.insert(&routed(2, 150, "默认", "兜底", &[], None))
            .unwrap();
        db.insert(&routed(3, 250, "默认", "兜底", &[], None))
            .unwrap();
        let mut local = routed(4, 150, "默认", "兜底", &[], None);
        local.local = true;
        db.insert(&local).unwrap();
        db.insert(&row(5, 150)).unwrap();
        let mut broken = row(6, 150);
        broken.routing = Some("{\"rule\":".into());
        db.insert(&broken).unwrap();

        let got = db.route_hits(100, 200).unwrap();
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!((got[0].requests, got[0].last_ms), (1, 150));
        assert!(db.route_hits(1_000, 2_000).unwrap().is_empty());
    }
}
