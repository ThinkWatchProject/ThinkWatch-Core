//! 搜索在库里的那一半：按条件挑行，新的在前。
//!
//! **条件只写在这一处。**只按记录找时它们是 `WHERE`；按正文找时，「按记录对没对上」
//! 作为一列一起取出来，没对上的再去读正文 —— 两种找法说的是同一句 SQL，不会一边多认
//! 一个字段。

use rusqlite::functions::FunctionFlags;
use rusqlite::types::{ToSql, ValueRef};

use super::{Ask, needle};
use crate::db::{Db, DbError, RequestRow, row_from};

/// 给连接装上搜索要的 SQL 函数。每个连接打开时都装（[`Db`] 的构造里调它）。
///
/// `tw_has(字段, 小写的词)`：字段转小写之后含不含这个词，和界面上
/// `x.toLowerCase().includes(q)` 是同一个判断（见 [`needle::contains_folded`]）。
/// **不用 `LIKE`**：它只认 ASCII 的大小写（「Über」对不上「über」），而且 `%` 和 `_`
/// 是通配符，要一个一个转义；这个函数比的是字面的子串，两样问题都没有。字段是 NULL 的
/// 什么都不含。
pub(crate) fn register(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "tw_has",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let (ValueRef::Text(hay), ValueRef::Text(needle)) = (ctx.get_raw(0), ctx.get_raw(1))
            else {
                return Ok(false);
            };
            Ok(
                match (std::str::from_utf8(hay), std::str::from_utf8(needle)) {
                    (Ok(hay), Ok(needle)) => needle::contains_folded(hay, needle),
                    _ => false,
                },
            )
        },
    )
}

/// 无法计价：跑完了、报了用量，却没算出金额。
///
/// 和界面 `filterRows` 里那一条逐字对应：失败的、取消的（记录里的取消就是界面上的
/// `cancelled` 状态）、一项用量都没报的不算。**和库里别处的 `NO_PRICE` 不是一回事**：那个
/// 数的是「配一个价格就能解决的」，要求按量计费；这个是界面上那个筛选，不计费的上游记的是
/// 确定的 $0，本来就不会出现在这里。
const UNPRICED: &str = "(cost_micros IS NULL AND error IS NULL AND cancelled = 0 \
                        AND (input_tokens IS NOT NULL OR output_tokens IS NOT NULL))";

/// 一句查询和它的参数（按名字绑定）。
pub(crate) type Statement = (String, Vec<(&'static str, Box<dyn ToSql>)>);

impl Db {
    /// 按 `ask` 挑一批行，新的在前，带着「按记录对没对上搜索词」。
    ///
    /// 时刻有索引（`requests_at`），条件里时刻是一个范围，所以从新往旧沿着索引走，凑够
    /// `limit` 条就停；同一毫秒里的几条按请求号排（查询计划里那个「LAST TERM OF ORDER BY」
    /// 只在同一毫秒的几条里排，不是整表排序）。
    pub fn search_rows(&self, ask: &Ask) -> Result<Vec<(RequestRow, bool)>, DbError> {
        let (sql, args) = statement(ask);
        let mut st = self.conn().prepare(&sql)?;
        let params: Vec<(&str, &dyn ToSql)> = args.iter().map(|(k, v)| (*k, v.as_ref())).collect();
        let rows = st.query_map(params.as_slice(), |r| {
            Ok((row_from(r)?, r.get::<_, i64>("meta_hit")? != 0))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

/// [`Db::search_rows`] 跑的那一句。**只拼用得上的条件**：没给的条件不写成
/// `(? IS NULL OR …)`，那样查询计划看不出时刻是一个范围，就走不了索引。
pub(crate) fn statement(ask: &Ask) -> Statement {
    let q = ask.query;
    let mut from = q.from_ms;
    let mut upper = q.to_ms;
    let mut conds: Vec<String> = vec!["at_ms >= :from".into(), "at_ms <= :upper".into()];
    let mut args: Vec<(&'static str, Box<dyn ToSql>)> = Vec::new();
    if let Some(floor) = ask.floor {
        from = from.max(floor);
    }
    if let Some(c) = ask.before {
        upper = upper.min(c.at_ms);
        // 同一毫秒里排在它后面的（请求号更小的）还要
        conds.push("(at_ms < :before_at OR id < :before_id)".into());
        args.push((":before_at", Box::new(c.at_ms)));
        args.push((":before_id", Box::new(c.id)));
    }
    args.push((":from", Box::new(from)));
    args.push((":upper", Box::new(upper)));
    if q.failed {
        conds.push("error IS NOT NULL".into());
    }
    if q.unpriced {
        conds.push(UNPRICED.into());
    }
    if let Some(c) = &q.client {
        conds.push("client = :client".into());
        args.push((":client", Box::new(c.clone())));
    }
    if let Some(p) = &q.provider {
        // 本地应答的那一行在界面上没有上游（那一格写的是一句说明）
        conds.push("(local = 0 AND provider = :provider)".into());
        args.push((":provider", Box::new(p.clone())));
    }
    if let Some(m) = &q.model {
        conds.push("model = :model".into());
        args.push((":model", Box::new(m.clone())));
    }
    let mut flag = "1".to_string();
    if let Some(t) = &q.text {
        args.push((":q", Box::new(t.clone())));
        args.push((":local", Box::new(q.local_matches)));
        if !q.error_codes.is_empty() {
            let codes = serde_json::to_string(&q.error_codes).unwrap_or_default();
            args.push((":codes", Box::new(codes)));
        }
        let text = text_matches(!q.error_codes.is_empty());
        if ask.flag_text {
            flag = text;
        } else {
            conds.push(text);
        }
    }
    args.push((":limit", Box::new(ask.limit as i64)));
    let sql = format!(
        "SELECT *, {flag} AS meta_hit FROM requests WHERE {} \
         ORDER BY at_ms DESC, id DESC LIMIT :limit",
        conds.join(" AND ")
    );
    (sql, args)
}

/// 搜索词按记录对：**和界面 `filterRows` 的那一串 `||` 逐项对应**。
///
/// - 路径、密钥名、应用（按请求头认的）、模型、失败原因（英文原句）：转小写之后含着它；
/// - 来源地址：**原样**含着它 —— 界面那一句没有转小写（地址里没有大写字母，结果一样，
///   照抄是为了两边说的是同一件事）；
/// - 上游：界面那一格写的是什么就按什么对，本地应答的那一行写的是一句说明，由界面告诉
///   我们那句话对不对得上（`:local`），它的上游名不参与；
/// - 失败原因的码在界面交来的那张清单里（界面按码翻那句话）。
///
/// 每一项都不会是 NULL：这一串还要当作一列取出来。
fn text_matches(codes: bool) -> String {
    let mut terms = vec![
        "tw_has(path, :q)",
        "tw_has(client, :q)",
        "tw_has(client_hint, :q)",
        "IFNULL(instr(peer, :q), 0) > 0",
        "(CASE WHEN local = 1 THEN :local ELSE tw_has(provider, :q) END)",
        "tw_has(model, :q)",
        "tw_has(error, :q)",
    ];
    if codes {
        terms.push("IFNULL(error_code IN (SELECT value FROM json_each(:codes)), 0)");
    }
    format!("({})", terms.join(" OR "))
}
