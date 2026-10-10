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
///
/// 24：安全日志多了内容过滤的匹配方式和解出来的隐藏内容（`security_events` 的
/// `matching`、`revealed`），结局多了「已删除」。
///
/// 25：插件在每个请求上的运行记录（`plugin_runs`）。
///
/// 26：尝试链的结果（`routing` 里的 `outcome`）没有了 `slow_start`，多了 `idle_timeout` 和
/// `aborted`。
///
/// 27：`bytes`（解码之后的回答有多少字节）换成网关和上游之间的流量 —— `sent_bytes`、
/// `received_bytes` —— 和出口 `egress`；尝试链的每一跳（`routing`）多了 `proxy`。安全日志
/// 多了细节（`security_events` 的 `session`、`sent_model`、`detail`）。
pub(crate) const SCHEMA: i64 = 27;

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

/// 数路由命中要的那几项，从路由那一列（`tw_api::RoutingView` 的 JSON）里取（见
/// [`Db::route_hits`]）。**建索引的和查询的是同一串**：表达式一字不差，SQLite 才用索引里
/// 存好的值，不回表去解 JSON。
///
/// 用 `json_extract` 不用 `->>`：索引写在库文件里，读这个库的 SQLite 都得认得它。
const ROUTE: &str = "json_extract(routing, '$.route')";
const RULE: &str = "json_extract(routing, '$.rule')";
/// 一个 JSON 数组的原文（`[]`、`["关思考"]`）
const REWRITTEN_BY: &str = "json_extract(routing, '$.rewritten_by')";
const DENIED_BY: &str = "json_extract(routing, '$.denied_by')";
/// 经过路由、路由那一列解得开的行。**解不开的不进索引**：一行坏掉的记录不该让整张表出不来，
/// 也不该让它自己写不进去
const ROUTED: &str = "local = 0 AND json_valid(routing)";

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("{path} could not be opened: {source}")]
    Open {
        path: String,
        source: rusqlite::Error,
    },
    /// 库是别的版本建的。**不迁移、也不硬读** —— 旧版本建的由 [`crate::open`]
    /// 连同正文目录整个重建，更新的版本建的原样留着。
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
    /// 客户端要的模型名。按模型的统计看的是它：客户端要了什么
    pub model: String,
    /// 发给上游的模型名：规则改写过的是改写后的那个（尝试链最后一跳的 `model`），没改写
    /// 的和 `model` 一样。**按它查价**，上游体检也拿它和回答里写的比 —— 上游收到的是它
    pub sent_model: String,
    /// 上游在回答里写的模型名，原样。回答里没写的是 None（Bedrock 的 Converse、
    /// WebSocket、没收到回答的）
    pub answered_model: Option<String>,
    pub path: String,
    /// 没走到上游就失败时是 None
    pub status: Option<u16>,
    /// 响应头到的时刻
    pub ttfb_ms: Option<i64>,
    /// 第一个 token 到的时刻。只有流式的有（`tw_api::Event::RequestFirstToken`）
    pub ttft_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    /// 生成速度，token/秒。网关在结局里算好的（`RequestFinished::tokens_per_sec`），
    /// 只有跑完的流式请求有
    pub tokens_per_sec: Option<u32>,
    /// 发给上游的请求体，每一跳加起来，按线上的样子（`RequestFinished::sent_bytes`）。
    /// 本地应答的、一跳都没发出去的是 None
    pub sent_bytes: Option<i64>,
    /// 从上游收到的响应体，每一跳加起来，解压之前（`RequestFinished::received_bytes`）
    pub received_bytes: Option<i64>,
    /// 从哪个出口出去的：代理名，直连是 None（`RequestRouted::egress`）
    pub egress: Option<String>,
    /// 上游报的输入，**不含缓存读写**：几种格式在解析时已经换算成三项互不重叠的数，
    /// 三项加起来才是上游计费的全部输入
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    /// 本地估的输入 token 数（`tw_api::Event::RequestStarted::input_estimate`）。
    /// 解不开的请求是 None
    pub input_estimate: Option<i64>,
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
    /// 请求带着的 DeepSeek Harness 会话日志有多少字节。没带是 None
    pub session_log_bytes: Option<i64>,
}

/// 一把密钥从某一刻起用了多少，按几样分开数（见 [`Db::key_usage_since`]）。
///
/// **分开的那几样就是「算不算」要看的**：数 token 的请求、没发到上游的不算进密钥的
/// 用量上限，判断在网关那边（`tw_gateway::key_limits`），和它结算一行时是同一个判断。
#[derive(Debug, Clone, PartialEq)]
pub struct KeyUsage {
    pub client: String,
    pub path: String,
    /// 这些请求可能发到了上游（[`tw_api::RoutingView::reached_upstream`]）
    pub reached: bool,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// 记下的费用合计，微分。算不出来的那几行算 0
    pub cost_micros: i64,
}

/// 装上 `tw_reached(路由那一列, 失败没有)`：这一行的请求可能发到了上游
/// （[`tw_api::RoutingView::reached_upstream`]）。**判断写在 tw-api 那一处**，记下一行时交给
/// 网关的（[`crate::Settled::reached`]）和从库里加回来的是同一个。路由那一列读不出来的当作
/// 尝试链是空的
fn register_reached(conn: &Connection) -> rusqlite::Result<()> {
    use rusqlite::functions::FunctionFlags;
    use rusqlite::types::ValueRef;
    conn.create_scalar_function(
        "tw_reached",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let failed = matches!(ctx.get_raw(1), ValueRef::Integer(n) if n != 0);
            let routing = match ctx.get_raw(0) {
                ValueRef::Text(t) => serde_json::from_slice::<tw_api::RoutingView>(t).ok(),
                _ => None,
            };
            Ok(routing.unwrap_or_default().reached_upstream(failed))
        },
    )
}

#[derive(Debug)]
pub struct Db {
    /// 上游体检的查询在 `crate::health`，和这里共用一个连接
    pub(crate) conn: Connection,
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

    /// 给同一个 crate 里另写在别处的查询用（搜索在 `crate::search`）
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
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
        // 搜索用的 SQL 函数。装在连接上，不进库文件：每次打开都要装
        crate::search::register(&conn)?;
        register_reached(&conn)?;
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
        db.indexes();
        Ok(db)
    }

    /// 表之外另加的索引，**每次打开都走一遍**（`IF NOT EXISTS`，建过了就是空操作）。
    ///
    /// **它们不算 schema，加它们不加 [`SCHEMA`]**：索引不改变一行长什么样，少了它查询照样
    /// 对、只是慢，多了它别的版本照样读写（SQLite 自己维护）。加 [`SCHEMA`] 却要清掉每个人
    /// 的请求历史 —— 为了快一点不值得。已有的库第一次打开时要建一遍（二十七万行一秒上下）。
    ///
    /// **建不成只记一行日志**：那时查询照样对，只是慢。
    fn indexes(&self) {
        // - `requests_client`：每把密钥最后一次用在什么时候，一把一把地跳着取（见
        //   `last_seen_by_client`）。**只给那一条查询用**：条件里要写上 `client > ''` 才用得上
        //   它。按时间窗、按密钥分组的那些（密钥的用量、按密钥的费用）用了它，会把整个索引按
        //   密钥扫一遍，比沿着时刻走慢得多
        // - `requests_session_at`：最近活动过的会话，按时间倒着走，只读索引（见 `sessions`）
        // - `requests_routed`：各条路由、规则命中了多少要的那几项，落库时从路由那一列里取出来
        //   存在索引里。数的时候只读索引，不再把一周几 MB 的 JSON 读出来解一遍（见
        //   `route_hits`）
        let sql = format!(
            "CREATE INDEX IF NOT EXISTS requests_client ON requests (client, at_ms)
                WHERE client > '';
             CREATE INDEX IF NOT EXISTS requests_session_at ON requests (at_ms, session)
                WHERE session IS NOT NULL AND local = 0;
             CREATE INDEX IF NOT EXISTS requests_routed
                ON requests (at_ms, {ROUTE}, {RULE}, {REWRITTEN_BY}, {DENIED_BY}, error IS NOT NULL)
                WHERE {ROUTED};"
        );
        if let Err(e) = self.conn.execute_batch(&sql) {
            tracing::warn!(
                "the request history could not be indexed, so some views load slowly: {e}"
            );
        }
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
                -- 发给上游的模型名。**和 `model` 分开存**：规则可以把请求改写成另一个
                -- 模型发出去，上游按它收钱、照它回答，而按模型的统计看的是客户端要的那个
                sent_model         TEXT    NOT NULL,
                -- 上游在回答里写的模型名，原样。**在记录的时候就定下**：它只在响应体里，
                -- 而响应体只留一段、过几天就删
                answered_model     TEXT,
                path               TEXT    NOT NULL,
                status             INTEGER,
                ttfb_ms            INTEGER,
                -- 第一个 token 到的时刻。**和响应头是两个时刻**：流式响应的响应头一般
                -- 一收到请求就回，用户等的是这一个。非流式的没有
                ttft_ms            INTEGER,
                duration_ms        INTEGER,
                -- 生成速度，token/秒。**在记录的时候就定下**：推理被隐藏时要扣掉推理
                -- token，而那只有看着流的网关知道，事后从这几列推不回来
                tokens_per_sec     INTEGER,
                -- 网关和上游之间的流量：发出去的请求体、收回来的响应体，每一跳加起来，
                -- 按线上的样子（压缩之后、解压之前）。**走代理、被计量的是这一段**，客户端
                -- 和网关之间在本机，不算
                sent_bytes         INTEGER,
                received_bytes     INTEGER,
                -- 从哪个出口出去的：代理名，直连是 NULL。按出口数流量看它
                egress             TEXT,
                input_tokens       INTEGER,
                output_tokens      INTEGER,
                cache_read_tokens  INTEGER,
                cache_write_tokens INTEGER,
                -- 本地估的输入 token 数（发给上游的那一份）。**在记录的时候就定下**：
                -- 事后要估就得把请求体再解析一遍，而请求体过几天就删
                input_estimate     INTEGER,
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
                cancelled          INTEGER NOT NULL,
                -- 请求带着 DeepSeek Harness 的会话日志：它的字节数。整段对话都在里面，
                -- 界面要说得出哪些请求带着它
                session_log_bytes  INTEGER
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
                count      INTEGER NOT NULL,
                -- 内容过滤：规则怎么认（contains / regex / codepoints）。别的防护是 NULL
                matching   TEXT,
                -- 内容过滤的码位规则命中标签字符时解出来的原文。别的时候是 NULL
                revealed   TEXT,
                -- 请求属于哪一次会话，记下时知道的。请求那一行在的话以那一行为准
                session    TEXT,
                -- 记下时已经知道的、发给上游的模型名（工具调用审查：回答已经在路上了）。
                -- 请求那一行在的话以那一行为准
                sent_model TEXT,
                -- 细节，JSON（`tw_api::SecurityHitDetail`）：每一处在哪儿、打过码的前后文、
                -- 当时的规则、具体做了什么。**命中那一刻定下**：规则之后改了，这一条不变。
                -- 存在这一行上，和请求记录一起过期
                detail     TEXT    NOT NULL
             );
             CREATE INDEX security_events_at ON security_events (at_ms DESC);
             CREATE INDEX security_events_request ON security_events (request_id);
             -- 脚本插件在请求上的每一次运行：请求钩子一次一行，回答钩子一个回答一行。
             -- **跑了没改、出错跳过的也记**：一个请求经过了哪些插件，要说得全
             CREATE TABLE plugin_runs (
                request_id  INTEGER NOT NULL,
                -- 这个请求上的第几次，从 0 起，按记下的先后
                seq         INTEGER NOT NULL,
                at_ms       INTEGER NOT NULL,
                plugin_id   TEXT    NOT NULL,
                -- 当时的名字。插件之后改了名，这一行说的还是当时那个
                plugin_name TEXT    NOT NULL,
                -- request / reply
                hook        TEXT    NOT NULL,
                -- unchanged / changed / rejected / error / skipped
                outcome     TEXT    NOT NULL,
                -- 出错、拒绝的原因：正文、码、参数，和 `requests` 的三列一样
                error       TEXT,
                error_code  TEXT,
                error_args  TEXT,
                cpu_us      INTEGER NOT NULL,
                -- 细节，JSON（回答钩子改了几处之类）
                detail      TEXT,
                PRIMARY KEY (request_id, seq)
             );
             CREATE INDEX plugin_runs_at ON plugin_runs (at_ms);
             PRAGMA user_version = {SCHEMA};
             COMMIT;"
        ))?;
        Ok(())
    }

    /// 记一条。
    pub fn insert(&self, r: &RequestRow) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO requests
             (id, at_ms, client, provider, model, path, status, ttfb_ms, duration_ms, sent_bytes,
              input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
              cost_micros, cost_estimated, error, local, routing, billing, cache_saved_micros,
              client_hint, session, cancelled, price_source, translated,
              error_code, error_args, peer, key_masked, session_log_bytes,
              ttft_ms, tokens_per_sec, sent_model, answered_model, input_estimate,
              received_bytes, egress)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30,?31,?32,?33,?34,?35,?36,?37,?38)",
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
                r.sent_bytes,
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
                r.session_log_bytes,
                r.ttft_ms,
                r.tokens_per_sec,
                r.sent_model,
                r.answered_model,
                r.input_estimate,
                r.received_bytes,
                r.egress,
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

/// 一次会话汇总的那几列，和 [`session_row`] 一列对一列。`GROUP BY session` 之后用
fn session_columns() -> String {
    format!(
        "session,
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
         COALESCE(SUM({NO_USAGE}), 0)"
    )
}

/// 读出 [`session_columns`] 的一行
fn session_row(r: &rusqlite::Row) -> rusqlite::Result<SessionRow> {
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
    /// 上游回的状态码。没走到上游的是 None
    pub status: Option<u16>,
    /// 失败的原因，带着码。上游回了错误、原样交给客户端的也有（那时 `status` 是那个
    /// 状态码），见 `tw_api::TurnView::error`
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
    /// 问的是整次任务，不是它落在某个窗口里的那一段。所以留下的会话整条聚合，
    /// 按「首尾之间和窗口有没有重叠」留。
    ///
    /// **先挑会话，再聚合挑中的那几次**（[`Db::recent_sessions`]）。以前把三个月里每一行按
    /// 会话分组、排序之后再截前几百条：二十七万行要两百毫秒，而界面每结束一个请求就来要一次。
    pub fn sessions(
        &self,
        within: Option<(i64, i64)>,
        limit: usize,
    ) -> Result<Vec<SessionRow>, DbError> {
        let picked = self.recent_sessions(within, limit)?;
        if picked.is_empty() {
            return Ok(Vec::new());
        }
        // 挑中的会话号作为一个 JSON 数组交进去：几百个占位符拼不出一条好读的 SQL
        let picked = serde_json::Value::from(picked).to_string();
        let mut st = self.conn.prepare(&format!(
            "SELECT {} FROM requests
             WHERE session IN (SELECT value FROM json_each(?1)) AND local = 0
             GROUP BY session
             ORDER BY MAX(at_ms) DESC, session DESC",
            session_columns()
        ))?;
        let rows = st.query_map([picked], session_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 一次会话的汇总。没有这次会话（或者它的每一轮都是本地应答的）是 None。
    ///
    /// **按会话号直接取**，不从列表里找：以前取最近五百次会话再从里面挑，更早的会话点开是 404。
    pub fn session(&self, id: &str) -> Result<Option<SessionRow>, DbError> {
        Ok(self
            .conn
            .prepare(&format!(
                "SELECT {} FROM requests WHERE session = ?1 AND local = 0 GROUP BY session",
                session_columns()
            ))?
            .query_row([id], session_row)
            .optional()?)
    }

    /// 最近活动过的 `limit` 次会话，最后一次活动最近的在前；`within` 给了就只要首尾之间和它
    /// 重叠的。
    ///
    /// **只走索引，不按会话分组整张表**：按时间倒着走（`requests_session_at`，只有带会话、
    /// 不是本地应答的行），一次会话第一次碰到的那一行就是它最后一次活动，碰够了就停。窗口的
    /// 起点之前不必再走：最后一次活动比它早的会话不和窗口重叠。最后一次活动在窗口终点之后的，
    /// 再按会话号取它的第一轮（`requests_session`）看是不是不晚于终点。
    fn recent_sessions(
        &self,
        within: Option<(i64, i64)>,
        limit: usize,
    ) -> Result<Vec<String>, DbError> {
        let (from, to) = within.unwrap_or((i64::MIN, i64::MAX));
        let mut out = Vec::new();
        if limit == 0 {
            return Ok(out);
        }
        let mut walk = self.conn.prepare(
            "SELECT session, at_ms FROM requests
             WHERE session IS NOT NULL AND local = 0 AND at_ms >= ?1
             ORDER BY at_ms DESC, session DESC",
        )?;
        let mut first = self.conn.prepare(
            "SELECT at_ms FROM requests WHERE session = ?1 AND local = 0
             ORDER BY at_ms LIMIT 1",
        )?;
        let mut seen = std::collections::HashSet::new();
        let mut rows = walk.query([from])?;
        while let Some(r) = rows.next()? {
            let session = r.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
            if seen.contains(session) {
                continue;
            }
            let last: i64 = r.get(1)?;
            let overlaps = last <= to || first.query_row([session], |r| r.get::<_, i64>(0))? <= to;
            seen.insert(session.to_string());
            if overlaps {
                out.push(session.to_string());
                if out.len() == limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// 一次会话里的每一轮，**按时间正序** —— 曲线是从左往右画的。
    pub fn turns(&self, session: &str) -> Result<Vec<TurnRow>, DbError> {
        let mut st = self.conn.prepare(
            "SELECT id, at_ms, model, provider, input_tokens, output_tokens,
                    cache_read_tokens, cost_micros, duration_ms, error, cancelled,
                    cost_estimated, billing, error_code, error_args, status
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
                status: r.get(15)?,
                error: error_from(r)?,
                cancelled: r.get::<_, i64>(10)? != 0,
                cost_estimated: r.get::<_, i64>(11)? != 0,
                billing: slug_col(r, 12, tw_api::Billing::from_slug)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 一次会话里的每一个请求，整行。**和 [`Db::turns`] 同样的筛法、同样的顺序**：对话记录
    /// （[`crate::transcript`]）一轮对一轮地跟着会话详情走。
    pub fn session_requests(&self, session: &str) -> Result<Vec<RequestRow>, DbError> {
        let mut st = self.conn.prepare(
            "SELECT * FROM requests WHERE session = ?1 AND local = 0 ORDER BY at_ms, id",
        )?;
        let rows = st.query_map([session], row_from)?;
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
    ///
    /// **一把一把地跳着取**（`requests_client`）：取下一把比上一把大的密钥名，再取它最大的
    /// 时刻，各是一次索引查找。按密钥分组的话要把三个月里每一行过一遍，而密钥只有几把。
    /// 每个子查询都写着 `client > ''`：那个索引只收这些行，条件里没有它就用不上。
    pub fn last_seen_by_client(&self) -> Result<Vec<(String, i64)>, DbError> {
        let mut st = self.conn.prepare(
            "WITH RECURSIVE keys(client) AS (
                SELECT MIN(client) FROM requests WHERE client > ''
                UNION ALL
                SELECT (SELECT MIN(client) FROM requests WHERE client > keys.client AND client > '')
                FROM keys WHERE keys.client IS NOT NULL
             )
             SELECT client,
                    (SELECT MAX(at_ms) FROM requests
                     WHERE requests.client = keys.client AND client > '')
             FROM keys WHERE client IS NOT NULL",
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
        // 该在哪张表里补价格取决于它走的是哪家。
        //
        // **模型是查价用的那个名字**：规则改写过的，是发给上游的那个（`sent_model`）
        // —— 该补价格的是它，照着客户端要的名字补，补了也还是算不出钱
        let mut st = self.conn.prepare(&format!(
            "SELECT provider, sent_model, COUNT(*) AS n FROM requests \
             WHERE {filter} GROUP BY provider, sent_model \
             ORDER BY n DESC, provider, sent_model LIMIT 20"
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
            sent,
            received,
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
                COALESCE(SUM(cache_saved_micros), 0),
                COALESCE(SUM({NO_USAGE}), 0),
                COALESCE(SUM(sent_bytes), 0),
                COALESCE(SUM(received_bytes), 0)
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
            sent_bytes: sent,
            received_bytes: received,
            cost_micros_exact: exact,
            cost_micros_estimated: estimated,
            unpriced_requests: unpriced,
            no_usage_requests: no_usage,
            cache_saved_micros: cache_saved,
        })
    }

    /// 某段时间内每个模型第一个 token 到的时刻的分位数。
    ///
    /// **用分位数不用平均值**：AI 延迟是长尾分布，平均值会被极端
    /// 值拉偏。**样本数一起返回** —— 「800ms」是 3 个样本还是 300 个，
    /// 含义完全不同。
    ///
    /// **只有流式的有样本。**非流式的整段一起到：它的「第一个 token」就是总耗时，混进来
    /// 的话，一个跑了三十秒的长回答会让这个模型看起来要等三十秒才开口。
    pub fn latency_by_model(&self, since_ms: i64, until_ms: i64) -> Result<Vec<Latency>, DbError> {
        let mut st = self.conn.prepare(
            "SELECT model, ttft_ms FROM requests
             WHERE at_ms >= ?1 AND at_ms < ?2 AND local = 0 AND ttft_ms IS NOT NULL
             ORDER BY model, ttft_ms",
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
             (at_ms, request_id, guard, rule, custom, action, provider, client, tool, excerpt, count,
              matching, revealed, session, sent_model, detail)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
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
                e.matching.map(tw_api::ContentMatch::slug),
                e.revealed,
                e.session,
                e.sent_model,
                serde_json::to_string(&e.detail).unwrap_or_default(),
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
                tw_api::SecurityOutcome::Stripped => &mut by.stripped,
                tw_api::SecurityOutcome::Blocked => &mut by.blocked,
            };
            *slot = n;
        }
        Ok(tw_api::SecurityEventsPage {
            events,
            more,
            total: by.recorded + by.replaced + by.cut + by.stripped + by.blocked,
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

    /// 这几条请求的安全记录，按请求号分好。
    ///
    /// 搜索翻出来的那一页散在整份记录里，按请求号的范围取（[`Db::security_of_requests`]）
    /// 会把中间几万条请求的记录一起取回来，所以按号点名。
    pub fn security_of(
        &self,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, Vec<tw_api::SecurityEventView>>, DbError> {
        let mut out: std::collections::HashMap<i64, Vec<tw_api::SecurityEventView>> =
            Default::default();
        if ids.is_empty() {
            return Ok(out);
        }
        let mut st = self.conn.prepare(&format!(
            "{SECURITY_SELECT}
             WHERE e.request_id IN (SELECT value FROM json_each(?1))
             ORDER BY e.id"
        ))?;
        let ids = serde_json::to_string(ids).unwrap_or_default();
        for r in st.query_map([ids], security_view)? {
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
                COALESCE(SUM(guard = 'content'), 0),
                COALESCE(SUM(guard = 'content' AND action = 'blocked'), 0),
                COALESCE(SUM(guard = 'content' AND action = 'stripped'), 0)
             FROM security_events WHERE at_ms >= ?1 AND at_ms < ?2",
            params![since_ms, until_ms],
            |r| {
                Ok(tw_api::SecurityCounts {
                    secrets: r.get(0)?,
                    secrets_replaced: r.get(1)?,
                    tool_calls: r.get(2)?,
                    tool_calls_cut: r.get(3)?,
                    content: r.get(4)?,
                    content_blocked: r.get(5)?,
                    content_stripped: r.get(6)?,
                })
            },
        )?)
    }

    /// 按上游分的延迟分位数。样本和按模型分的一样，只有流式的。
    ///
    /// **和按模型分是两个问题。**「哪个模型慢」和「哪家上游慢」的下一步
    /// 完全不同：前者换模型，后者换上游。合成一张表的话两个问题都答不好
    /// （M3 验收里问的是后者）。
    pub fn latency_by_provider(
        &self,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Vec<Latency>, DbError> {
        self.latency_by("provider", since_ms, until_ms)
    }

    /// 按密钥分的首 token 分位数：谁在用、谁等得久。**和按上游分是两个问题** —— 一把密钥
    /// 的请求可能分在好几家上游，慢的是哪一家要看按上游分的那个。样本的规矩和按模型分的一样
    pub fn latency_by_client(&self, since_ms: i64, until_ms: i64) -> Result<Vec<Latency>, DbError> {
        self.latency_by("client", since_ms, until_ms)
    }

    /// `by` 只会是上面两个调用方给的列名，不来自外面
    fn latency_by(&self, by: &str, since_ms: i64, until_ms: i64) -> Result<Vec<Latency>, DbError> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {by}, ttft_ms FROM requests
             WHERE at_ms >= ?1 AND at_ms < ?2 AND local = 0 AND ttft_ms IS NOT NULL
               AND {by} <> ''
             ORDER BY {by}, ttft_ms"
        ))?;
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

    /// 某段时间内每个模型的生成速度中位数，token/秒。
    ///
    /// 和延迟一样用分位数、带样本数。样本是有速度的请求：跑完的流式请求（见
    /// `tokens_per_sec` 那一列）。
    pub fn token_rate_by_model(
        &self,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Vec<TokenRate>, DbError> {
        self.token_rate("model", since_ms, until_ms)
    }

    /// 按上游分的生成速度中位数。**和按模型分是两个问题**，理由同延迟。
    pub fn token_rate_by_provider(
        &self,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Vec<TokenRate>, DbError> {
        self.token_rate("provider", since_ms, until_ms)
    }

    /// `by` 只会是上面两个调用方给的列名，不来自外面
    fn token_rate(
        &self,
        by: &str,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Vec<TokenRate>, DbError> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {by}, tokens_per_sec FROM requests
             WHERE at_ms >= ?1 AND at_ms < ?2 AND local = 0 AND tokens_per_sec IS NOT NULL
               AND {by} <> ''
             ORDER BY {by}, tokens_per_sec"
        ))?;
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
            .map(|(model, xs)| TokenRate {
                p50: percentile(&xs, 50).clamp(0, i64::from(u32::MAX)) as u32,
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
                    COALESCE(SUM({NO_USAGE}), 0),
                    COALESCE(SUM(sent_bytes), 0),
                    COALESCE(SUM(received_bytes), 0),
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cache_read_tokens), 0),
                    COALESCE(SUM(cache_write_tokens), 0)
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
                sent_bytes: r.get(7)?,
                received_bytes: r.get(8)?,
                input_tokens: r.get(9)?,
                output_tokens: r.get(10)?,
                cache_read_tokens: r.get(11)?,
                cache_write_tokens: r.get(12)?,
                ttft_p50_ms: None,
                ttft_p95_ms: None,
                ttft_samples: 0,
            })
        })?;
        let mut buckets = rows.collect::<Result<Vec<_>, _>>()?;
        // 每一格的首 token 分位数：**样本和 `latency_by_model` 同一个口径**（有第一个 token 的、
        // 不是本地应答的）。分位数在 SQL 里求不出来，按格取出排好序的样本在这里求；一格的
        // 样本就是那一格里的流式请求，不多
        let mut st = self.conn.prepare(
            "SELECT ((at_ms - ?1) / ?3) AS b, ttft_ms FROM requests
             WHERE at_ms >= ?1 AND at_ms < ?2 AND local = 0 AND ttft_ms IS NOT NULL
             ORDER BY b, ttft_ms",
        )?;
        let rows = st.query_map(params![since_ms, until_ms, bucket_ms], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut by: std::collections::BTreeMap<i64, Vec<i64>> = Default::default();
        for row in rows {
            let (b, t) = row?;
            by.entry(b).or_default().push(t);
        }
        for bucket in &mut buckets {
            let b = (bucket.at_ms - since_ms) / bucket_ms;
            if let Some(xs) = by.get(&b) {
                bucket.ttft_p50_ms = Some(percentile(xs, 50));
                bucket.ttft_p95_ms = Some(percentile(xs, 95));
                bucket.ttft_samples = xs.len() as i64;
            }
        }
        Ok(buckets)
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
        let col = dim_col(dim);
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
                    COALESCE(SUM(cache_write_tokens), 0),
                    COALESCE(SUM(sent_bytes), 0),
                    COALESCE(SUM(received_bytes), 0)
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
                sent_bytes: r.get(12)?,
                received_bytes: r.get(13)?,
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
        let col = dim_col(dim);
        // 名字是空的不成一组（本地应答之外，WebSocket 的连接行不知道模型）。**出口除外**：
        // 空串是直连那一组
        let named = match dim {
            tw_api::CostDim::Egress => "1".to_string(),
            _ => format!("{col} <> ''"),
        };
        let sql = format!(
            "SELECT {col}, COUNT(*),
                    COALESCE(SUM(cost_micros), 0),
                    COALESCE(SUM({NO_PRICE}), 0),
                    COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM({NO_USAGE}), 0)
             FROM requests
             WHERE at_ms >= ?1 AND at_ms < ?2 AND local = 0 AND {named}
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

    /// 每把密钥从 `since_ms` 起用了多少：重启之后（和一期的开头变了的时候），密钥的用量
    /// 上限按它把这一天、这一周、这个月的数加回来。
    ///
    /// 网关自己答的不在里面。**按「算不算」要看的几样分组**（路径、有没有发到上游），组数和
    /// 密钥、路径的个数相当，一把密钥一个月的记录也只有几十组。有没有发到上游按路由那一列
    /// 和这一行失败没有判断（`tw_reached`，和记下这一行时交给网关的是同一个判断）
    pub fn key_usage_since(&self, since_ms: i64) -> Result<Vec<KeyUsage>, DbError> {
        let mut st = self.conn.prepare(
            "SELECT client, path, tw_reached(routing, error IS NOT NULL) AS reached,
                    COUNT(*),
                    COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cache_read_tokens), 0), COALESCE(SUM(cache_write_tokens), 0),
                    COALESCE(SUM(cost_micros), 0)
             FROM requests
             WHERE at_ms >= ?1 AND local = 0
             GROUP BY client, path, reached",
        )?;
        let rows = st.query_map(params![since_ms], |r| {
            Ok(KeyUsage {
                client: r.get(0)?,
                path: r.get(1)?,
                reached: r.get(2)?,
                requests: r.get(3)?,
                input_tokens: r.get(4)?,
                output_tokens: r.get(5)?,
                cache_read_tokens: r.get(6)?,
                cache_write_tokens: r.get(7)?,
                cost_micros: r.get(8)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// 各条路由走了多少请求、各条规则命中了多少，以及记录从哪一刻起是全的（见
    /// [`tw_api::RouteStats`]）。
    pub fn route_stats(&self, since_ms: i64, until_ms: i64) -> Result<tw_api::RouteStats, DbError> {
        Ok(tw_api::RouteStats {
            covered_since_ms: self.covered_since(since_ms, until_ms)?,
            routes: self.route_hits(since_ms, until_ms)?,
        })
    }

    /// 这段时间里记录从哪一刻起是全的（见 [`tw_api::RouteStats::covered_since_ms`]）：
    /// 问的起点和库里最老那条请求开始的时刻，取晚的那个；落在窗口外就是空。
    ///
    /// 最老的那条**不论是不是经过了路由、是不是本地应答**：哪一行都说明那时候已经在记
    /// 了，那段时间里的请求都会在库里
    pub(crate) fn covered_since(
        &self,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Option<i64>, DbError> {
        let oldest: Option<i64> =
            self.conn
                .query_row("SELECT MIN(at_ms) FROM requests", [], |r| r.get(0))?;
        Ok(oldest
            .map(|o| o.max(since_ms))
            .filter(|&from| from < until_ms))
    }

    /// 各条路由走了多少请求、各条规则命中了多少（见 [`tw_api::RouteHits`]）。
    ///
    /// **按每一行记下的路由算**，不按现在的配置推：请求走的是它那一刻的路由和
    /// 规则。一个请求算在这几条规则上，每条只算一次：决定去向的那一条、附加了
    /// 改写的每一条、选定上游之后拒绝了它的那一条。本地应答的没有路由，不算。
    ///
    /// **只读索引**（`requests_routed`）：要的那几项落库时就从路由那一列里取出来存在索引里，
    /// 路由图开着时每十秒来一次，不再把一周几 MB 的 JSON 读出来一行一行地解。
    fn route_hits(&self, since_ms: i64, until_ms: i64) -> Result<Vec<tw_api::RouteHits>, DbError> {
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
        let mut st = self.conn.prepare(&format!(
            "SELECT at_ms, error IS NOT NULL, {ROUTE}, {RULE}, {REWRITTEN_BY}, {DENIED_BY}
             FROM requests
             WHERE at_ms >= ?1 AND at_ms < ?2 AND {ROUTED}"
        ))?;
        let mut routes: Named<(Tally, Named<Tally>)> = Named::default();
        let mut rows = st.query(params![since_ms, until_ms])?;
        while let Some(r) = rows.next()? {
            let at_ms: i64 = r.get(0)?;
            let failed: bool = r.get(1)?;
            // 缺了哪一项的那一行不算。**一条坏掉的记录不该让整张表出不来**
            let (Ok(route), Ok(rule), Ok(rewritten_by), Ok(denied_by)) = (
                r.get_ref(2)?.as_str(),
                r.get_ref(3)?.as_str(),
                r.get_ref(4)?.as_str(),
                r.get_ref(5)?.as_str_or_null(),
            ) else {
                continue;
            };
            // 绝大多数请求没有附加改写，不必解
            let rewritten_by: Vec<String> = match rewritten_by {
                "[]" => Vec::new(),
                j => match serde_json::from_str(j) {
                    Ok(names) => names,
                    Err(_) => continue,
                },
            };
            let (total, rules) = routes.get(route);
            total.add(at_ms, failed);
            let decider = rules.get(rule);
            decider.add(at_ms, failed);
            decider.decided += 1;
            let mut counted = vec![rule];
            for name in rewritten_by.iter().map(String::as_str).chain(denied_by) {
                if !counted.contains(&name) {
                    rules.get(name).add(at_ms, failed);
                    counted.push(name);
                }
            }
        }
        let mut out: Vec<tw_api::RouteHits> = routes
            .items
            .into_iter()
            .map(|(route, (total, rules))| {
                let mut rules: Vec<tw_api::RuleHits> = rules
                    .items
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
        // 插件的运行记录同理
        let _ = self
            .conn
            .execute("DELETE FROM plugin_runs WHERE at_ms < ?1", [cutoff_ms]);
        Ok(self
            .conn
            .execute("DELETE FROM requests WHERE at_ms < ?1", [cutoff_ms])?)
    }
}

/// 按名字数的一组东西。**名字只在第一次见到时拷一份**：数路由命中时一周两万行，每行都按
/// 名字找一遍（见 [`Db::route_hits`]）
struct Named<T> {
    at: std::collections::HashMap<String, usize>,
    items: Vec<(String, T)>,
}

impl<T> Default for Named<T> {
    fn default() -> Self {
        Self {
            at: Default::default(),
            items: Vec::new(),
        }
    }
}

impl<T: Default> Named<T> {
    fn get(&mut self, name: &str) -> &mut T {
        let i = match self.at.get(name) {
            Some(&i) => i,
            None => {
                self.at.insert(name.to_string(), self.items.len());
                self.items.push((name.to_string(), T::default()));
                self.items.len() - 1
            }
        };
        &mut self.items[i].1
    }
}

/// 一个插件在一个请求上的一次运行，落库的样子（`plugin_runs` 一行，少了 `seq`：
/// 它在写入时按这个请求已有的行数定）。
#[derive(Debug, Clone, PartialEq)]
pub struct PluginRunRow {
    pub request_id: i64,
    pub at_ms: i64,
    pub plugin_id: String,
    pub plugin_name: String,
    pub hook: tw_api::PluginHook,
    pub outcome: tw_api::PluginOutcome,
    pub error: Option<Msg>,
    pub cpu_us: i64,
    /// JSON
    pub detail: Option<String>,
}

impl Db {
    /// 记一次插件运行。**排在这个请求已有的那些后面**：先记下的先跑
    pub fn insert_plugin_run(&self, r: &PluginRunRow) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT INTO plugin_runs
             (request_id, seq, at_ms, plugin_id, plugin_name, hook, outcome,
              error, error_code, error_args, cpu_us, detail)
             VALUES (?1,
                     (SELECT COALESCE(MAX(seq) + 1, 0) FROM plugin_runs WHERE request_id = ?1),
                     ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                r.request_id,
                r.at_ms,
                r.plugin_id,
                r.plugin_name,
                r.hook.slug(),
                r.outcome.slug(),
                r.error.as_ref().map(|e| e.text.as_str()),
                r.error.as_ref().map(|e| e.code.as_str()),
                r.error
                    .as_ref()
                    .filter(|e| !e.args.is_empty())
                    .map(|e| serde_json::to_string(&e.args).unwrap_or_default()),
                r.cpu_us,
                r.detail,
            ],
        )?;
        Ok(())
    }

    /// 一个请求上的插件运行，按记下的先后。
    pub fn plugin_runs(&self, request_id: i64) -> Result<Vec<PluginRunRow>, DbError> {
        let mut st = self
            .conn
            .prepare("SELECT * FROM plugin_runs WHERE request_id = ?1 ORDER BY seq")?;
        let rows = st.query_map([request_id], |r| {
            Ok(PluginRunRow {
                request_id: r.get("request_id")?,
                at_ms: r.get("at_ms")?,
                plugin_id: r.get("plugin_id")?,
                plugin_name: r.get("plugin_name")?,
                hook: slug_col(r, "hook", tw_api::PluginHook::from_slug)?,
                outcome: slug_col(r, "outcome", tw_api::PluginOutcome::from_slug)?,
                error: error_from(r)?,
                cpu_us: r.get("cpu_us")?,
                detail: r.get("detail")?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 请求号落在 `[from, to]` 里、被插件改过的那些（流量页的徽标）。一段历史一次取完
    pub fn changed_by_plugins_between(
        &self,
        from: i64,
        to: i64,
    ) -> Result<std::collections::HashSet<i64>, DbError> {
        let mut st = self.conn.prepare(
            "SELECT DISTINCT request_id FROM plugin_runs
             WHERE request_id >= ?1 AND request_id <= ?2 AND outcome = 'changed'",
        )?;
        let ids = st.query_map(params![from, to], |r| r.get(0))?;
        Ok(ids.collect::<Result<_, _>>()?)
    }

    /// 这几条请求里被插件改过的。**按号点名**：搜索翻出来的一页散在整份记录里
    pub fn changed_by_plugins(
        &self,
        ids: &[i64],
    ) -> Result<std::collections::HashSet<i64>, DbError> {
        if ids.is_empty() {
            return Ok(Default::default());
        }
        let mut st = self.conn.prepare(
            "SELECT DISTINCT request_id FROM plugin_runs
             WHERE request_id IN (SELECT value FROM json_each(?1)) AND outcome = 'changed'",
        )?;
        let ids = serde_json::to_string(ids).unwrap_or_default();
        let found = st.query_map([ids], |r| r.get(0))?;
        Ok(found.collect::<Result<_, _>>()?)
    }
}

/// 分组维度对应的那一列。**只有这几个固定的值** —— 维度来自查询串，拼进 SQL 的只能是这里
/// 写死的列名（见 `tw_api::CostDim`）
fn dim_col(dim: tw_api::CostDim) -> &'static str {
    match dim {
        tw_api::CostDim::Model => "model",
        tw_api::CostDim::Provider => "provider",
        tw_api::CostDim::Client => "client",
        // 直连的出口是 NULL：算成空串那一组，不丢掉
        tw_api::CostDim::Egress => "COALESCE(egress, '')",
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
pub(crate) fn percentile(sorted: &[i64], p: usize) -> i64 {
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

pub(crate) fn row_from(r: &rusqlite::Row) -> rusqlite::Result<RequestRow> {
    Ok(RequestRow {
        id: r.get("id")?,
        at_ms: r.get("at_ms")?,
        client: r.get("client")?,
        client_hint: r.get("client_hint")?,
        peer: r.get("peer")?,
        key_masked: r.get("key_masked")?,
        session_log_bytes: r.get("session_log_bytes")?,
        session: r.get("session")?,
        provider: r.get("provider")?,
        model: r.get("model")?,
        sent_model: r.get("sent_model")?,
        answered_model: r.get("answered_model")?,
        path: r.get("path")?,
        status: r.get::<_, Option<i64>>("status")?.map(|s| s as u16),
        ttfb_ms: r.get("ttfb_ms")?,
        ttft_ms: r.get("ttft_ms")?,
        duration_ms: r.get("duration_ms")?,
        tokens_per_sec: r.get("tokens_per_sec")?,
        sent_bytes: r.get("sent_bytes")?,
        received_bytes: r.get("received_bytes")?,
        egress: r.get("egress")?,
        input_tokens: r.get("input_tokens")?,
        output_tokens: r.get("output_tokens")?,
        cache_read_tokens: r.get("cache_read_tokens")?,
        cache_write_tokens: r.get("cache_write_tokens")?,
        input_estimate: r.get("input_estimate")?,
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
    /// 和上游之间的流量合计：发出去的请求体、收回来的响应体
    pub sent_bytes: i64,
    pub received_bytes: i64,
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
    /// 内容过滤：规则怎么认
    pub matching: Option<tw_api::ContentMatch>,
    /// 内容过滤的码位规则解出来的隐藏内容
    pub revealed: Option<String>,
    /// 请求属于哪一次会话，记下时知道的
    pub session: Option<String>,
    /// 记下时已经知道的、发给上游的模型名
    pub sent_model: Option<String>,
    /// 细节：每一处在哪儿、当时的规则、具体做了什么。**已打码**
    pub detail: tw_api::SecurityHitDetail,
}

/// 读安全日志时的那段 SELECT。**上游、密钥、模型、会话优先取请求那一行的。**
///
/// 发给上游的模型名也取请求那一行的，**只在它可能发到了上游时**（`tw_reached`，和密钥的
/// 用量上限同一个判断）：被拒的、被规则挡下的一个字节都没发出去，那一行记着的名字是它
/// 要发的，不是发了的。请求还没落库时退回记录自己的。
const SECURITY_SELECT: &str =
    "SELECT e.id, e.at_ms, e.request_id, e.guard, e.rule, e.custom, e.action,
        COALESCE(NULLIF(r.provider, ''), e.provider),
        COALESCE(NULLIF(r.client, ''), e.client),
        COALESCE(r.model, ''),
        e.tool, e.excerpt, e.count,
        r.client_hint, r.peer, r.key_masked,
        e.matching, e.revealed,
        COALESCE(r.session, e.session),
        CASE
            WHEN r.id IS NULL THEN e.sent_model
            WHEN tw_reached(r.routing, r.error IS NOT NULL) THEN NULLIF(r.sent_model, '')
        END,
        e.detail
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
    // 细节解不开是这一行坏了：表的样子变了要加 SCHEMA，不会读到别的样子
    let detail: String = r.get(20)?;
    let detail: tw_api::SecurityHitDetail = serde_json::from_str(&detail).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(20, rusqlite::types::Type::Text, e.into())
    })?;
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
        matching: match r.get::<_, Option<String>>(16)? {
            Some(_) => Some(slug_col(r, 16, tw_api::ContentMatch::from_slug)?),
            None => None,
        },
        revealed: r.get(17)?,
        client_hint: r.get(13)?,
        peer: r.get(14)?,
        key_masked: r.get(15)?,
        session: r.get(18)?,
        sent_model: r.get(19)?,
        direction: detail.direction,
        locations: detail.locations,
        more_locations: detail.more_locations,
        rule_snapshot: detail.rule_snapshot,
        outcome_detail: detail.outcome_detail,
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

/// 生成速度的中位数。`model` 在按上游分的那个查询里是上游名
#[derive(Debug, Clone, PartialEq)]
pub struct TokenRate {
    pub model: String,
    pub p50: u32,
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
            sent_model: "claude-sonnet-4-5".into(),
            answered_model: None,
            path: "/v1/messages".into(),
            status: Some(200),
            ttfb_ms: Some(300),
            ttft_ms: Some(800),
            duration_ms: Some(4000),
            tokens_per_sec: Some(156),
            sent_bytes: Some(4321),
            received_bytes: Some(12345),
            egress: None,
            input_tokens: Some(1000),
            output_tokens: Some(500),
            cache_read_tokens: Some(200),
            cache_write_tokens: None,
            input_estimate: None,
            cost_micros: Some(12_000),
            cost_estimated: false,
            error: None,
            local: false,
            cancelled: false,
            routing: None,
            billing: tw_api::Billing::PerToken,
            cache_saved_micros: None,
            price_source: None,
            session_log_bytes: None,
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

    /// 一条流量：发出去多少、收回来多少、从哪个出口
    fn traffic(id: i64, at_ms: i64, sent: i64, received: i64, egress: Option<&str>) -> RequestRow {
        let mut r = row(id, at_ms);
        r.sent_bytes = Some(sent);
        r.received_bytes = Some(received);
        r.egress = egress.map(str::to_string);
        r
    }

    /// 流量和出口跟着一行走：写进去、读出来一样；一跳都没发出去的是 None，不是 0
    #[test]
    fn traffic_and_egress_survive_a_round_trip() {
        let db = Db::in_memory().unwrap();
        db.insert(&traffic(1, 1000, 2048, 312, Some("机场")))
            .unwrap();
        let mut never = row(2, 1001);
        never.sent_bytes = None;
        never.received_bytes = None;
        db.insert(&never).unwrap();
        let r = db.get(1).unwrap().unwrap();
        assert_eq!(
            (r.sent_bytes, r.received_bytes, r.egress.as_deref()),
            (Some(2048), Some(312), Some("机场"))
        );
        let r = db.get(2).unwrap().unwrap();
        assert_eq!(
            (r.sent_bytes, r.received_bytes, r.egress),
            (None, None, None)
        );
    }

    /// 汇总、每一格、每一格的每一项都带着流量的合计；一跳都没发出去的那几行不算（没有流量），
    /// 本地应答的也不算
    #[test]
    fn traffic_adds_up_in_the_summary_and_in_every_bucket() {
        let db = Db::in_memory().unwrap();
        let t0 = 1_000_000_000i64;
        let hour = 3_600_000i64;
        db.insert(&traffic(1, t0 + 1, 100, 1_000, None)).unwrap();
        db.insert(&traffic(2, t0 + 2, 200, 2_000, Some("机场")))
            .unwrap();
        db.insert(&traffic(3, t0 + hour + 1, 400, 4_000, Some("机场")))
            .unwrap();
        let mut refused = row(4, t0 + 3);
        refused.sent_bytes = None;
        refused.received_bytes = None;
        db.insert(&refused).unwrap();
        let mut local = traffic(5, t0 + 4, 9_999, 9_999, None);
        local.local = true;
        db.insert(&local).unwrap();

        let s = db.summary(t0, t0 + 2 * hour).unwrap();
        assert_eq!((s.sent_bytes, s.received_bytes), (700, 7_000));

        let b = db.cost_buckets(t0, t0 + 2 * hour, hour).unwrap();
        let per: Vec<_> = b.iter().map(|b| (b.sent_bytes, b.received_bytes)).collect();
        assert_eq!(per, [(300, 3_000), (400, 4_000)]);
        // 每一格也带着四类 token：`row` 是输入 1000、输出 500、缓存读 200
        assert_eq!(
            (
                b[0].input_tokens,
                b[0].output_tokens,
                b[0].cache_read_tokens,
                b[0].cache_write_tokens
            ),
            (3_000, 1_500, 600, 0)
        );

        let by = db
            .cost_buckets_by(tw_api::CostDim::Model, t0, t0 + 2 * hour, hour)
            .unwrap();
        let per: Vec<_> = by
            .iter()
            .map(|b| (b.at_ms, b.sent_bytes, b.received_bytes))
            .collect();
        assert_eq!(per, [(t0, 300, 3_000), (t0 + hour, 400, 4_000)]);
    }

    /// 按出口分：**直连的那一组名字是空串**，不丢掉 —— 少了它各组加起来对不上总数
    #[test]
    fn grouping_by_egress_keeps_the_direct_traffic_as_an_empty_name() {
        let db = Db::in_memory().unwrap();
        let t0 = 1_000_000_000i64;
        db.insert(&traffic(1, t0 + 1, 100, 1_000, None)).unwrap();
        db.insert(&traffic(2, t0 + 2, 200, 2_000, Some("机场")))
            .unwrap();
        db.insert(&traffic(3, t0 + 3, 400, 4_000, Some("机场")))
            .unwrap();

        let mut by = db
            .cost_buckets_by(tw_api::CostDim::Egress, t0, t0 + 1000, 1000)
            .unwrap();
        by.sort_by(|a, b| a.name.cmp(&b.name));
        let per: Vec<_> = by
            .iter()
            .map(|b| (b.name.as_str(), b.requests, b.sent_bytes, b.received_bytes))
            .collect();
        assert_eq!(per, [("", 1, 100, 1_000), ("机场", 2, 600, 6_000)]);

        let g = db.cost_by(tw_api::CostDim::Egress, t0, t0 + 1000).unwrap();
        let mut names: Vec<_> = g.iter().map(|g| (g.name.as_str(), g.requests)).collect();
        names.sort();
        assert_eq!(names, [("", 1), ("机场", 2)]);
    }

    /// 每一格的首 token 分位数：**样本和 `latency_by_model` 同一个口径** —— 有第一个 token 的
    /// （流式的）才算，本地应答的不算。没有样本的格子是空的，不是 0
    #[test]
    fn every_bucket_carries_its_first_token_percentiles() {
        let db = Db::in_memory().unwrap();
        let t0 = 1_000_000_000i64;
        let hour = 3_600_000i64;
        // 第 0 格：100 到 1000 毫秒的十个流式请求，加一个非流式的
        for i in 1..=10 {
            let mut r = row(i, t0 + i);
            r.ttft_ms = Some(i * 100);
            db.insert(&r).unwrap();
        }
        let mut whole = row(11, t0 + 11);
        whole.ttft_ms = None;
        db.insert(&whole).unwrap();
        let mut local = row(12, t0 + 12);
        local.local = true;
        local.ttft_ms = Some(1);
        db.insert(&local).unwrap();
        // 第 1 格：只有非流式的
        let mut later = row(13, t0 + hour + 1);
        later.ttft_ms = None;
        db.insert(&later).unwrap();

        let b = db.cost_buckets(t0, t0 + 2 * hour, hour).unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(
            (b[0].ttft_p50_ms, b[0].ttft_p95_ms, b[0].ttft_samples),
            (Some(500), Some(1000), 10)
        );
        assert_eq!(
            (b[1].ttft_p50_ms, b[1].ttft_p95_ms, b[1].ttft_samples),
            (None, None, 0)
        );
        // 和整段的按模型分同一个口径
        let all = db.latency_by_model(t0, t0 + hour).unwrap();
        assert_eq!((all[0].p50, all[0].p95, all[0].samples), (500, 1000, 10));
    }

    /// 按密钥分的首 token 分位数：每把密钥一组，规矩和按模型分的一样
    #[test]
    fn latency_by_client_groups_by_the_key() {
        let db = Db::in_memory().unwrap();
        for i in 1..=4 {
            let mut r = row(i, 1000 + i);
            r.client = if i % 2 == 0 { "codex" } else { "claude-code" }.into();
            r.ttft_ms = Some(i * 100);
            db.insert(&r).unwrap();
        }
        let mut whole = row(5, 1005);
        whole.client = "codex".into();
        whole.ttft_ms = None;
        db.insert(&whole).unwrap();
        let lat = db.latency_by_client(0, 10_000).unwrap();
        let got: Vec<_> = lat
            .iter()
            .map(|l| (l.model.as_str(), l.p50, l.p95, l.samples))
            .collect();
        assert_eq!(got, [("claude-code", 100, 300, 2), ("codex", 200, 400, 2)]);
    }

    /// 密钥用量上限重启之后加回来的数：按密钥、路径、有没有发到上游分组，从那一刻起，网关
    /// 自己答的不算。
    #[test]
    fn key_usage_adds_up_each_key_from_a_moment_on() {
        let db = Db::in_memory().unwrap();
        let t0 = 1_000_000_000i64;
        let attempt = r#"{"route":"default","rule":"r","rewritten_by":[],
            "attempts":[{"provider":"官方","outcome":"served","status":200,"ms":5}]}"#;
        let nowhere = r#"{"route":"default","rule":"r","rewritten_by":[],"attempts":[]}"#;
        let mut early = row(1, t0 - 1);
        early.routing = Some(attempt.into());
        let mut a = row(2, t0);
        a.routing = Some(attempt.into());
        let mut b = row(3, t0 + 5);
        b.routing = Some(attempt.into());
        b.cost_micros = None;
        b.cache_write_tokens = Some(7);
        let mut refused = row(4, t0 + 6);
        refused.routing = Some(nowhere.into());
        refused.error = Some(Msg {
            code: "gw.route.denied".into(),
            args: Default::default(),
            text: "x".into(),
        });
        refused.input_tokens = None;
        refused.output_tokens = None;
        refused.cache_read_tokens = None;
        refused.cost_micros = None;
        // 上游都满着：尝试链上只有跳过的那一跳
        let mut busy = refused.clone();
        busy.id = 7;
        busy.routing = Some(
            r#"{"route":"default","rule":"r","rewritten_by":[],"attempts":[{"provider":"官方",
                "outcome":"error","error":{"code":"gw.busy_upstream","args":{},"text":"x"},
                "ms":0,"skipped":"busy"}]}"#
                .into(),
        );
        let mut local = row(5, t0 + 7);
        local.local = true;
        let mut other = row(6, t0 + 8);
        other.client = "codex".into();
        for r in [&early, &a, &b, &refused, &busy, &local, &other] {
            db.insert(r).unwrap();
        }
        let mut got = db.key_usage_since(t0).unwrap();
        got.sort_by(|x, y| (&x.client, x.reached).cmp(&(&y.client, y.reached)));
        assert_eq!(got.len(), 3, "{got:?}");
        let (refused_g, served, codex) = (&got[0], &got[1], &got[2]);
        assert_eq!(
            (served.client.as_str(), served.reached, served.requests),
            ("claude-code", true, 2),
            "早于那一刻的、本地答的都不算"
        );
        assert_eq!(
            (
                served.input_tokens,
                served.output_tokens,
                served.cache_read_tokens,
                served.cache_write_tokens,
                served.cost_micros
            ),
            (2000, 1000, 400, 7, 12_000),
            "算不出钱的那一行算 0"
        );
        assert_eq!(
            (refused_g.reached, refused_g.requests),
            (false, 2),
            "被拒的、都满着的没发到上游"
        );
        assert_eq!(
            (codex.client.as_str(), codex.reached),
            ("codex", true),
            "没有路由那一列、也没失败的：客户端在等上游时走了，可能发到了"
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

    /// 列表是最后一次活动最近的那几次会话，每一次都整条聚合 —— 挑会话时只看了最近的那几行，
    /// 聚合的却是它的每一轮。窗口按首尾和它重叠不重叠留：最后一次活动在窗口之后、开始在窗口
    /// 之内的留，整个在窗口之后、之前的不留
    #[test]
    fn the_list_is_the_most_recently_active_sessions_each_counted_whole() {
        let db = Db::in_memory().unwrap();
        // （会话，时刻）：a 最早开始、最后结束；b 整个在中间；c 在 a 的两头之间断断续续
        for (id, (session, at)) in [
            ("a", 1_000),
            ("c", 2_000),
            ("b", 3_000),
            ("b", 4_000),
            ("c", 5_000),
            ("c", 6_000),
            ("a", 9_000),
        ]
        .into_iter()
        .enumerate()
        {
            let mut r = row(id as i64 + 1, at);
            r.session = Some(session.into());
            db.insert(&r).unwrap();
        }
        // 本地应答的不算，哪怕它最新
        let mut probe = row(99, 99_000);
        probe.session = Some("b".into());
        probe.local = true;
        db.insert(&probe).unwrap();

        let ids = |got: Vec<SessionRow>| got.into_iter().map(|s| s.id).collect::<Vec<_>>();
        let two = db.sessions(None, 2).unwrap();
        assert_eq!(
            two.iter()
                .map(|s| (s.id.as_str(), s.turns))
                .collect::<Vec<_>>(),
            [("a", 2), ("c", 3)]
        );
        assert_eq!(two[0].started_ms, 1_000);
        assert_eq!(ids(db.sessions(None, 10).unwrap()), ["a", "c", "b"]);
        assert!(db.sessions(None, 0).unwrap().is_empty());

        // 窗口 [3500, 4500]：三次都和它重叠（a、c 跨过它，b 在里面）
        assert_eq!(
            ids(db.sessions(Some((3_500, 4_500)), 10).unwrap()),
            ["a", "c", "b"]
        );
        // [0, 1500]：只有 a 开始得那么早
        assert_eq!(ids(db.sessions(Some((0, 1_500)), 10).unwrap()), ["a"]);
        // [6500, 8000]：b、c 那时都结束了，a 还没结束
        assert_eq!(ids(db.sessions(Some((6_500, 8_000)), 10).unwrap()), ["a"]);
        // 窗口在所有会话之后
        assert!(db.sessions(Some((10_000, 20_000)), 10).unwrap().is_empty());
    }

    /// 会话详情按会话号直接取：多老、前面压着多少次会话都取得到。以前从最近五百次里找，
    /// 更早的点开是 404
    #[test]
    fn one_session_is_found_by_its_id_however_old() {
        let db = Db::in_memory().unwrap();
        for i in 0..600i64 {
            let mut r = row(i + 1, 1_000 + i);
            r.session = Some(format!("s{i}"));
            db.insert(&r).unwrap();
        }
        let mut second = row(1_000, 5_000);
        second.session = Some("s0".into());
        db.insert(&second).unwrap();

        let s0 = db.session("s0").unwrap().unwrap();
        assert_eq!((s0.turns, s0.started_ms, s0.ended_ms), (2, 1_000, 5_000));
        assert_eq!(db.session("s599").unwrap().unwrap().turns, 1);
        assert_eq!(db.session("nope").unwrap(), None);

        // 只有本地应答的不算一次会话，和列表一样
        let mut probe = row(2_000, 6_000);
        probe.session = Some("probes".into());
        probe.local = true;
        db.insert(&probe).unwrap();
        assert_eq!(db.session("probes").unwrap(), None);
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
        let mut r = row(1, 1_000_000);
        r.sent_model = "claude-haiku-4-5".into();
        r.answered_model = Some("claude-haiku-4-5-20251001".into());
        r.input_estimate = Some(1_234);
        db.insert(&r).unwrap();
        assert_eq!(db.get(1).unwrap().unwrap(), r);
        // 没有的照样是 None
        let mut bare = row(2, 1_000_000);
        bare.answered_model = None;
        bare.input_estimate = None;
        db.insert(&bare).unwrap();
        assert_eq!(db.get(2).unwrap().unwrap(), bare);
    }

    #[test]
    fn the_optional_fields_stay_none_rather_than_becoming_zero() {
        // **把「不知道」记成 0 是在撒谎。**一个 ttfb 为 0 的请求会把
        // 分位数拉到地板上，而一个成本为 0 的请求会让总额偏低。
        let db = Db::in_memory().unwrap();
        let mut r = row(1, 1);
        r.status = None;
        r.ttfb_ms = None;
        r.ttft_ms = None;
        r.tokens_per_sec = None;
        r.cost_micros = None;
        r.input_tokens = None;
        db.insert(&r).unwrap();
        let got = db.get(1).unwrap().unwrap();
        assert_eq!(got.status, None);
        assert_eq!(got.ttfb_ms, None);
        assert_eq!(got.ttft_ms, None);
        assert_eq!(got.tokens_per_sec, None);
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
        probe.ttft_ms = Some(0);
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
            r.ttft_ms = Some(t * 10);
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
            r.ttft_ms = Some(500);
            db.insert(&r).unwrap();
        }
        let mut slow = row(100, 1000);
        slow.ttft_ms = Some(60_000);
        db.insert(&slow).unwrap();
        let lat = db.latency_by_model(0, 10_000).unwrap();
        assert_eq!(lat[0].p50, 500, "一个慢请求不该动 p50");
    }

    #[test]
    fn latency_is_grouped_by_model_because_mixing_them_is_meaningless() {
        let db = Db::in_memory().unwrap();
        let mut a = row(1, 1000);
        a.model = "claude-opus-4".into();
        a.ttft_ms = Some(3000);
        let mut b = row(2, 1000);
        b.model = "claude-3-5-haiku".into();
        b.ttft_ms = Some(300);
        db.insert(&a).unwrap();
        db.insert(&b).unwrap();
        let lat = db.latency_by_model(0, 10_000).unwrap();
        assert_eq!(lat.len(), 2);
        let opus = lat.iter().find(|l| l.model == "claude-opus-4").unwrap();
        assert_eq!(opus.p50, 3000);
    }

    /// **延迟看第一个 token，不看响应头。**非流式的没有第一个 token，不进样本：它的
    /// 响应头要等整段生成完才到，混进来的话这个模型看起来要等几十秒才开口
    #[test]
    fn latency_is_the_first_token_and_leaves_non_streaming_requests_out() {
        let db = Db::in_memory().unwrap();
        let mut streamed = row(1, 1000);
        streamed.ttfb_ms = Some(200);
        streamed.ttft_ms = Some(1_200);
        let mut whole = row(2, 1000);
        whole.ttfb_ms = Some(30_000);
        whole.ttft_ms = None;
        whole.tokens_per_sec = None;
        db.insert(&streamed).unwrap();
        db.insert(&whole).unwrap();
        let lat = db.latency_by_model(0, 10_000).unwrap();
        assert_eq!((lat[0].p50, lat[0].samples), (1_200, 1), "{lat:?}");
        let by_provider = db.latency_by_provider(0, 10_000).unwrap();
        assert_eq!((by_provider[0].p50, by_provider[0].samples), (1_200, 1));
    }

    /// 生成速度按模型、按上游各给中位数和样本数；没有速度的请求（非流式、本地应答）
    /// 不进样本
    #[test]
    fn token_rate_is_the_median_by_model_and_by_upstream() {
        let db = Db::in_memory().unwrap();
        for (id, model, provider, rate) in [
            (1, "sonnet", "anthropic", Some(80)),
            (2, "sonnet", "anthropic", Some(90)),
            (3, "sonnet", "openrouter", Some(120)),
            (4, "haiku", "anthropic", Some(200)),
            (5, "haiku", "anthropic", None),
        ] {
            let mut r = row(id, 1000);
            r.model = model.into();
            r.provider = provider.into();
            r.tokens_per_sec = rate;
            db.insert(&r).unwrap();
        }
        let mut probe = row(6, 1000);
        probe.local = true;
        probe.tokens_per_sec = Some(9_999);
        db.insert(&probe).unwrap();

        let by_model = db.token_rate_by_model(0, 10_000).unwrap();
        assert_eq!(
            by_model,
            vec![
                TokenRate {
                    model: "haiku".into(),
                    p50: 200,
                    samples: 1
                },
                TokenRate {
                    model: "sonnet".into(),
                    p50: 90,
                    samples: 3
                },
            ]
        );
        let by_provider = db.token_rate_by_provider(0, 10_000).unwrap();
        assert_eq!(
            by_provider,
            vec![
                TokenRate {
                    model: "anthropic".into(),
                    p50: 90,
                    samples: 3
                },
                TokenRate {
                    model: "openrouter".into(),
                    p50: 120,
                    samples: 1
                },
            ]
        );
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

    fn plugin_run(request_id: i64, at_ms: i64, outcome: tw_api::PluginOutcome) -> PluginRunRow {
        PluginRunRow {
            request_id,
            at_ms,
            plugin_id: format!("p{at_ms}"),
            plugin_name: "插件".into(),
            hook: tw_api::PluginHook::Request,
            outcome,
            error: None,
            cpu_us: 5,
            detail: None,
        }
    }

    /// 一个请求上的运行按记下的先后排；出错的原因带着码和参数读回来
    #[test]
    fn plugin_runs_come_back_in_the_order_they_were_recorded() {
        let db = Db::in_memory().unwrap();
        let mut failed = plugin_run(7, 30, tw_api::PluginOutcome::Error);
        failed.hook = tw_api::PluginHook::Reply;
        failed.error = Some(Msg {
            code: "t.cpu".into(),
            args: [("ms".to_string(), "200".to_string())].into(),
            text: "over 200 ms".into(),
        });
        failed.detail = Some("{\"texts\":2}".into());
        db.insert_plugin_run(&plugin_run(7, 10, tw_api::PluginOutcome::Changed))
            .unwrap();
        db.insert_plugin_run(&plugin_run(8, 15, tw_api::PluginOutcome::Unchanged))
            .unwrap();
        db.insert_plugin_run(&failed).unwrap();
        db.insert_plugin_run(&plugin_run(7, 20, tw_api::PluginOutcome::Skipped))
            .unwrap();
        let runs = db.plugin_runs(7).unwrap();
        let ids: Vec<&str> = runs.iter().map(|r| r.plugin_id.as_str()).collect();
        assert_eq!(ids, ["p10", "p30", "p20"]);
        assert_eq!(runs[1], failed);
        assert_eq!(runs[1].error.as_ref().unwrap().arg("ms"), "200");
        assert!(db.plugin_runs(9).unwrap().is_empty());
    }

    #[test]
    fn requests_changed_by_plugins_are_found_by_range_and_by_id() {
        let db = Db::in_memory().unwrap();
        db.insert_plugin_run(&plugin_run(1, 10, tw_api::PluginOutcome::Changed))
            .unwrap();
        db.insert_plugin_run(&plugin_run(1, 11, tw_api::PluginOutcome::Error))
            .unwrap();
        db.insert_plugin_run(&plugin_run(2, 20, tw_api::PluginOutcome::Unchanged))
            .unwrap();
        db.insert_plugin_run(&plugin_run(3, 30, tw_api::PluginOutcome::Changed))
            .unwrap();
        let mut got: Vec<i64> = db
            .changed_by_plugins_between(1, 2)
            .unwrap()
            .into_iter()
            .collect();
        got.sort();
        assert_eq!(got, [1]);
        let mut got: Vec<i64> = db
            .changed_by_plugins(&[2, 3])
            .unwrap()
            .into_iter()
            .collect();
        got.sort();
        assert_eq!(got, [3]);
        assert!(db.changed_by_plugins(&[]).unwrap().is_empty());
    }

    /// 运行记录跟着请求一起过期
    #[test]
    fn plugin_runs_are_pruned_with_the_requests() {
        let db = Db::in_memory().unwrap();
        db.insert(&row(1, 100)).unwrap();
        db.insert(&row(2, 900)).unwrap();
        db.insert_plugin_run(&plugin_run(1, 100, tw_api::PluginOutcome::Changed))
            .unwrap();
        db.insert_plugin_run(&plugin_run(2, 900, tw_api::PluginOutcome::Changed))
            .unwrap();
        db.prune_before(500).unwrap();
        assert!(db.plugin_runs(1).unwrap().is_empty());
        assert_eq!(db.plugin_runs(2).unwrap().len(), 1);
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
        // 没有密钥的（本地来的、旧记录）不算一把
        let mut keyless = row(9, 900);
        keyless.client = String::new();
        db.insert(&keyless).unwrap();
        let mut got = db.last_seen_by_client().unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![("codex".to_string(), 300), ("default".to_string(), 200)]
        );
    }

    /// 另加的索引不算 schema：同一版本、还没有这些索引的库照样打开，行都在，缺的索引补上
    /// —— 加 SCHEMA 的话，每个人的请求历史都要清掉
    #[test]
    fn a_database_without_the_extra_indexes_keeps_its_rows_and_gains_them() {
        const EXTRA: [&str; 3] = ["requests_client", "requests_session_at", "requests_routed"];
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("data.db");
        {
            let db = Db::open(&path).unwrap();
            db.insert(&row(1, 100)).unwrap();
            for name in EXTRA {
                db.conn.execute(&format!("DROP INDEX {name}"), []).unwrap();
            }
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.count().unwrap(), 1);
        let names: Vec<String> = db
            .conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'requests'",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for name in EXTRA {
            assert!(names.iter().any(|n| n == name), "{name}: {names:?}");
        }
    }

    #[test]
    fn a_database_from_another_version_is_refused_rather_than_read() {
        // 不迁移，也不硬读：版本对不上就说出来，怎么处理由 `crate::open` 定
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
        r.sent_model = r.model.clone();
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

    /// 改写过模型名的请求按发出去的那个名字查价，**列出来的也得是那个名字** ——
    /// 照着客户端要的名字补价格，那几行照样算不出钱
    #[test]
    fn the_pricing_page_names_the_model_that_was_sent() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let db = Db::in_memory().unwrap();
        let arn = "arn:aws:bedrock:us-east-2:123456789012:application-inference-profile/a1b2c3";
        let mut r = unknown_model(1, now);
        r.model = "claude-sonnet-4-5".into();
        r.sent_model = arn.into();
        r.routing = Some(
            serde_json::to_string(&tw_api::RoutingView {
                attempts: vec![tw_api::AttemptView {
                    provider: "官方".into(),
                    model: Some(arn.into()),
                    outcome: tw_api::AttemptOutcome::Served,
                    status: Some(200),
                    error: None,
                    ms: 1,
                    usage: None,
                    queued_ms: None,
                    skipped: None,
                    proxy: None,
                }],
                ..Default::default()
            })
            .unwrap(),
        );
        db.insert(&r).unwrap();
        // 没改写的照旧是客户端要的名字
        db.insert(&unknown_model(2, now)).unwrap();
        let (n, models) = db.unpriced_recent(7).unwrap();
        assert_eq!(n, 2);
        let mut names: Vec<_> = models.into_iter().map(|m| m.model).collect();
        names.sort();
        assert_eq!(names, [arn, "中转站自己起的名字"]);
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
            r.sent_model = r.model.clone();
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
            matching: (guard == Guard::Content).then_some(tw_api::ContentMatch::Contains),
            revealed: None,
            session: None,
            sent_model: None,
            detail: Default::default(),
        }
    }

    /// 九条，时刻 1–9，号也是 1–9。只记录 3 条、已替换 1 条、已切断 2 条、
    /// 已删除 1 条、被拒 2 条
    fn seeded() -> Db {
        let db = Db::in_memory().unwrap();
        for (at, guard, action) in [
            (1, Guard::Redact, SecurityOutcome::Recorded),
            (2, Guard::Redact, SecurityOutcome::Replaced),
            (3, Guard::InspectTools, SecurityOutcome::Cut),
            (4, Guard::Redact, SecurityOutcome::Recorded),
            (5, Guard::Content, SecurityOutcome::Blocked),
            (6, Guard::Content, SecurityOutcome::Recorded),
            (7, Guard::InspectTools, SecurityOutcome::Cut),
            (8, Guard::Content, SecurityOutcome::Blocked),
            (9, Guard::Content, SecurityOutcome::Stripped),
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
            cut: 2,
            stripped: 1,
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
            // 删除的那条在 9，终点不含：数不到它
            (Some("content"), 0, 9),
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
                    SecurityOutcome::Stripped => counted.stripped += 1,
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

    /// 一条都没有：零条，五项都在、都是 0。
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
mod route_stats_tests {
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
                affinity: None,
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

        let got = db.route_stats(0, 1_000).unwrap().routes;
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
    /// 路由那一列解不开的那一行照样写得进去（索引不收它），数命中时不算。**一条坏掉的记录
    /// 不该让整张表出不来，也不该让它自己写不进去**
    #[test]
    fn a_row_whose_routing_does_not_parse_is_kept_and_left_out_of_the_count() {
        let db = Db::in_memory().unwrap();
        let mut broken = row(1, 100);
        broken.routing = Some("{\"route\": ".into());
        db.insert(&broken).unwrap();
        // 解得开、却缺了必有的一项的也不算
        let mut partial = row(2, 150);
        partial.routing = Some(r#"{"route":"默认"}"#.into());
        db.insert(&partial).unwrap();
        db.insert(&routed(3, 200, "默认", "兜底", &[], None))
            .unwrap();
        assert_eq!(db.count().unwrap(), 3);
        let got = db.route_stats(0, 1_000).unwrap();
        assert_eq!(got.routes.len(), 1, "{got:?}");
        assert_eq!(got.routes[0].requests, 1);
        assert_eq!(hits(&got.routes[0], "兜底").decided, 1);
    }

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

        let got = db.route_stats(100, 200).unwrap().routes;
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!((got[0].requests, got[0].last_ms), (1, 150));
        assert!(db.route_stats(1_000, 2_000).unwrap().routes.is_empty());
    }

    /// 库里一条请求都没有（刚装好、升级时刚重建）：什么都说不上，不是「整段都没命中」。
    #[test]
    fn an_empty_store_covers_nothing() {
        let db = Db::in_memory().unwrap();
        let got = db.route_stats(0, 1_000).unwrap();
        assert_eq!(got.covered_since_ms, None);
        assert!(got.routes.is_empty());
        // 线上是 null，不是省掉：界面必须想到这一种
        let v = serde_json::to_value(&got).unwrap();
        assert!(v["covered_since_ms"].is_null(), "{v}");
        assert!(v.get("covered_since_ms").is_some(), "{v}");
    }

    /// 记录比窗口短（库在窗口中间才建好，或者留的天数比窗口短）：记录从最老那条开始的
    /// 时刻起才是全的。**不论它是不是经过了路由** —— 本地应答的那一行也说明那时候
    /// 已经在记了。
    #[test]
    fn history_shorter_than_the_window_starts_at_the_oldest_request() {
        let db = Db::in_memory().unwrap();
        let mut local = row(1, 400);
        local.local = true;
        db.insert(&local).unwrap();
        db.insert(&routed(2, 600, "默认", "兜底", &[], None))
            .unwrap();

        let got = db.route_stats(0, 1_000).unwrap();
        assert_eq!(got.covered_since_ms, Some(400));
        assert_eq!(got.routes.len(), 1, "{got:?}");
        assert_eq!(got.routes[0].requests, 1);

        // 过期的记录删掉之后，最老的那条往后挪，记录的起点跟着挪
        assert_eq!(db.prune_before(500).unwrap(), 1);
        assert_eq!(
            db.route_stats(0, 1_000).unwrap().covered_since_ms,
            Some(600)
        );
    }

    /// 记录比窗口长：整段都有记录，起点就是问的起点。窗口里没有请求也一样 ——
    /// 那段时间在记，只是没有请求来。
    #[test]
    fn history_longer_than_the_window_covers_all_of_it() {
        let db = Db::in_memory().unwrap();
        db.insert(&routed(1, 50, "默认", "兜底", &[], None))
            .unwrap();
        db.insert(&routed(2, 500, "默认", "兜底", &[], None))
            .unwrap();

        let got = db.route_stats(100, 1_000).unwrap();
        assert_eq!(got.covered_since_ms, Some(100));
        assert_eq!(got.routes[0].requests, 1, "窗口外的那条不算：{got:?}");

        let quiet = db.route_stats(100, 400).unwrap();
        assert_eq!(quiet.covered_since_ms, Some(100));
        assert!(quiet.routes.is_empty(), "{quiet:?}");
    }

    /// 窗口整个在记录开始之前（或者窗口本身是空的）：这段时间没有一刻有记录。
    #[test]
    fn a_window_before_the_history_began_covers_nothing() {
        let db = Db::in_memory().unwrap();
        db.insert(&routed(1, 500, "默认", "兜底", &[], None))
            .unwrap();
        assert_eq!(db.route_stats(0, 400).unwrap().covered_since_ms, None);
        // 窗口的终点不含在内：恰好在终点开始的请求不在这段里
        assert_eq!(db.route_stats(0, 500).unwrap().covered_since_ms, None);
        assert_eq!(db.route_stats(0, 501).unwrap().covered_since_ms, Some(500));
        assert_eq!(db.route_stats(700, 700).unwrap().covered_since_ms, None);
    }
}

/// 一份和真实库一样大的请求库上，界面常问的那几条查询各花多久。
///
/// 默认不跑：要先写二十七万行。发布构建下跑（`TW_BENCH_DB` 给一个路径的话库留在那里，
/// 下次接着用）：
/// `cargo test --release -p tw-store history_cost -- --ignored --nocapture`
#[cfg(test)]
mod cost {
    use super::tests::row;
    use super::*;

    /// 九十天、每天三千条，和留得最久的记录一样长
    const ROWS: i64 = 270_000;
    const NOW: i64 = 1_790_000_000_000;
    const DAY: i64 = 86_400_000;

    /// 不引随机数的库：一个线性同余就够造数据
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    const CLIENTS: &[&str] = &["claude-code", "codex", "cursor", "opencode", "gemini", "pi"];
    const ROUTES: &[&str] = &["默认", "工作", "便宜"];
    const RULES: &[&str] = &[
        "兜底",
        "opus 走中转",
        "haiku 本地",
        "长上下文",
        "关思考",
        "拒绝 gpt-4",
    ];
    const PROVIDERS: &[&str] = &["官方", "中转 A", "中转 B", "OpenRouter", "Bedrock"];
    const MODELS: &[&str] = &[
        "claude-sonnet-4-5",
        "claude-opus-4-1",
        "gpt-5-codex",
        "gemini-2.5-pro",
    ];

    fn routing(g: &mut Lcg) -> String {
        let hops = if g.below(10) < 4 { 2 + g.below(2) } else { 1 };
        let attempts = (0..hops)
            .map(|i| {
                let last = i + 1 == hops;
                tw_api::AttemptView {
                    provider: PROVIDERS[g.below(PROVIDERS.len() as u64) as usize].into(),
                    model: (g.below(4) == 0).then(|| "claude-sonnet-4-5-20250929".into()),
                    outcome: if last {
                        tw_api::AttemptOutcome::Served
                    } else {
                        tw_api::AttemptOutcome::Error
                    },
                    status: last.then_some(200),
                    error: (!last).then(|| Msg {
                        code: "gw.upstream.timeout".into(),
                        args: Default::default(),
                        text: "The upstream answered 529 Overloaded: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"},\"request_id\":\"req_011CTxxxxxxxxxxxxxxxxxxx\"}; trying the next one."
                            .into(),
                    }),
                    ms: 200 + g.below(30_000),
                    usage: None,
                    queued_ms: None,
                    skipped: None,
                    proxy: None,
                }
            })
            .collect();
        serde_json::to_string(&tw_api::RoutingView {
            route: ROUTES[g.below(ROUTES.len() as u64) as usize].into(),
            rule: RULES[g.below(RULES.len() as u64) as usize].into(),
            group: Some("__all__".into()),
            rewritten_by: (0..g.below(3))
                .map(|_| RULES[g.below(RULES.len() as u64) as usize].to_string())
                .collect(),
            denied_by: (g.below(50) == 0).then(|| RULES[5].into()),
            affinity: Some(tw_api::AffinityView {
                held_route: g.below(2) == 0,
                stayed: None,
            }),
            attempts,
        })
        .unwrap()
    }

    /// 造一份：会话两三个交错着进行，一次几轮到几百轮；少数请求认不出会话（WebSocket）
    /// 或者是网关自己答的
    fn fill(db: &Db, rows: i64) {
        let mut g = Lcg(7);
        // （会话号，密钥，还剩几轮）
        let mut open: Vec<(String, &str, u64)> = Vec::new();
        db.conn.execute_batch("BEGIN").unwrap();
        for id in 1..=rows {
            let at = NOW - 90 * DAY + id * (90 * DAY / rows);
            let mut r = row(id, at);
            r.price_source = Some(r#"{"kind":"default","date":"2026-09-30"}"#.into());
            r.model = MODELS[g.below(MODELS.len() as u64) as usize].into();
            r.sent_model = r.model.clone();
            r.provider = PROVIDERS[g.below(PROVIDERS.len() as u64) as usize].into();
            match g.below(100) {
                0..=2 => {
                    r.local = true;
                    r.path = "count_tokens".into();
                    r.client = CLIENTS[g.below(CLIENTS.len() as u64) as usize].into();
                }
                3..=9 => {
                    r.client = CLIENTS[g.below(CLIENTS.len() as u64) as usize].into();
                    r.routing = Some(routing(&mut g));
                }
                _ => {
                    while open.len() < 3 {
                        let len = if g.below(20) == 0 {
                            300 + g.below(400)
                        } else {
                            1 + g.below(120)
                        };
                        open.push((
                            format!("{:012x}-{at}", g.next()),
                            CLIENTS[g.below(CLIENTS.len() as u64) as usize],
                            len,
                        ));
                    }
                    let i = g.below(open.len() as u64) as usize;
                    r.session = Some(open[i].0.clone());
                    r.client = open[i].1.into();
                    r.routing = Some(routing(&mut g));
                    open[i].2 -= 1;
                    if open[i].2 == 0 {
                        open.swap_remove(i);
                    }
                }
            }
            if g.below(30) == 0 {
                r.error = Some(super::tests::upstream_failed("upstream returned 529"));
            }
            db.insert(&r).unwrap();
        }
        db.conn.execute_batch("COMMIT").unwrap();
    }

    /// 以前的会话列表：整张表按会话分组，排序之后截前几条。新的那条要和它一模一样
    fn grouping_everything(db: &Db, within: Option<(i64, i64)>, limit: usize) -> Vec<SessionRow> {
        let (from, to) = within.unwrap_or((i64::MIN, i64::MAX));
        db.conn
            .prepare(&format!(
                "SELECT {} FROM requests
                 WHERE session IS NOT NULL AND local = 0
                 GROUP BY session
                 HAVING MAX(at_ms) >= ?1 AND MIN(at_ms) <= ?2
                 ORDER BY MAX(at_ms) DESC, session DESC
                 LIMIT ?3",
                session_columns()
            ))
            .unwrap()
            .query_map([from, to, limit as i64], session_row)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// 挑会话再聚合，和整张表分组再截前几条，在各种窗口、各种条数下给的是同一张表
    #[test]
    fn the_list_is_what_grouping_every_row_gives() {
        let db = Db::in_memory().unwrap();
        fill(&db, 6_000);
        for limit in [1, 7, 50, 2_000] {
            for within in [
                None,
                Some((NOW - 7 * DAY, NOW)),
                Some((NOW - 31 * DAY, NOW - 30 * DAY)),
                Some((NOW - 60 * DAY, NOW - 59 * DAY + 1)),
                Some((NOW + DAY, NOW + 2 * DAY)),
                Some((i64::MIN, NOW - 89 * DAY)),
            ] {
                assert_eq!(
                    db.sessions(within, limit).unwrap(),
                    grouping_everything(&db, within, limit),
                    "{within:?} {limit}"
                );
            }
        }
    }

    fn time<T>(what: &str, mut f: impl FnMut() -> T) -> T {
        let mut out = f();
        let mut best = std::time::Duration::MAX;
        for _ in 0..5 {
            let t0 = std::time::Instant::now();
            out = f();
            best = best.min(t0.elapsed());
        }
        eprintln!("{what:<48} {best:>12.2?}");
        out
    }

    #[test]
    #[ignore]
    fn history_cost() {
        let dir = tempfile::tempdir().unwrap();
        let path = std::env::var_os("TW_BENCH_DB")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| dir.path().join("data.db"));
        let t0 = std::time::Instant::now();
        let db = Db::open(&path).unwrap();
        eprintln!(
            "{:<48} {:>12.2?}",
            "open (builds missing indexes)",
            t0.elapsed()
        );
        if db.count().unwrap() < ROWS {
            fill(&db, ROWS);
        }
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        eprintln!(
            "{} rows, {} MB on disk",
            db.count().unwrap(),
            size / 1_000_000
        );
        for (what, within) in [
            ("sessions (no window, 200)", None),
            ("sessions (last 7 days, 200)", Some((NOW - 7 * DAY, NOW))),
            (
                "sessions (a day a month ago, 200)",
                Some((NOW - 31 * DAY, NOW - 30 * DAY)),
            ),
        ] {
            let got = time(what, || db.sessions(within, 200).unwrap());
            let grouped = time("  the same by grouping every row", || {
                grouping_everything(&db, within, 200)
            });
            assert_eq!(got, grouped, "{what}");
        }
        // 会话详情要的那一次：最老的一次会话
        let oldest = db.sessions(None, 2000).unwrap().pop().unwrap();
        let one = time("one session (oldest of 2000)", || {
            db.session(&oldest.id).unwrap()
        });
        assert_eq!(one, Some(oldest.clone()));
        time("one session's turns", || db.turns(&oldest.id).unwrap());
        time("last_seen_by_client", || db.last_seen_by_client().unwrap());
        time("route_stats (last 7 days)", || {
            db.route_stats(NOW - 7 * DAY, NOW).unwrap()
        });
    }
}
