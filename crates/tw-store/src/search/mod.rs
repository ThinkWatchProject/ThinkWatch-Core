//! 在整份请求记录里找（`POST /history/search`）。
//!
//! 流量页以前把最近两千条读进来再筛，可记录留着三个月 —— 再往前的，筛选框永远看不见。
//! 这里把那个筛选搬进库里，一页一页往回翻。
//!
//! **两种找法。**只按记录（路径、密钥、上游、模型、失败原因……）找，整个交给 SQL，三个月
//! 十万条也是一次走索引的扫描。按正文找要读盘：一份请求体动辄几百 KB，只能一份一份地读，
//! 所以每次只读一定的量（[`Budget`]），读完就把找到的先交回去，界面要再往前就再问一次。
//! 正文只留几天（`retention.body_days`），比它早的照样按记录对。

mod needle;
mod sql;
pub mod text;

pub(crate) use sql::register;

use std::time::{Duration, Instant};

use tw_api::{ContentHit, ContentSide, HistoryCursor, SearchStop};

use needle::{Excerpt, Needle, fold};

use crate::blobs::{Blobs, Which};
use crate::db::{DbError, RequestRow};

/// 一页缺省几条。
pub const DEFAULT_LIMIT: usize = 100;
/// 一页最多几条。**结果要整页交给界面**，五百行已经是一屏看不完的量
pub const MAX_LIMIT: usize = 500;

/// 按正文找时，一次从库里取多少行来看。每取一批锁一次库，看正文的时候不锁 ——
/// 正在落库的请求不必等这边读盘
const BATCH: usize = 64;

/// 一次搜索的条件，已经按界面的规则整理过：搜索词去掉首尾空白、转了小写，空串的
/// 条件当没给。
#[derive(Debug, Clone)]
pub struct Query {
    pub(crate) text: Option<String>,
    pub(crate) failed: bool,
    pub(crate) unpriced: bool,
    pub(crate) client: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) error_codes: Vec<String>,
    pub(crate) local_matches: bool,
    pub(crate) content: bool,
    pub(crate) from_ms: i64,
    pub(crate) to_ms: i64,
    pub(crate) before: Option<HistoryCursor>,
    pub(crate) limit: usize,
}

impl Query {
    pub fn new(q: &tw_api::HistorySearchQuery) -> Query {
        // 界面上「空串 = 不限」
        let exact = |s: &Option<String>| s.clone().filter(|s| !s.is_empty());
        Query {
            text: fold(&q.q),
            failed: q.failed,
            unpriced: q.unpriced,
            client: exact(&q.client),
            provider: exact(&q.provider),
            model: exact(&q.model),
            error_codes: q.error_codes.clone(),
            local_matches: q.local_matches,
            content: q.content,
            from_ms: q.from_ms.unwrap_or(i64::MIN),
            to_ms: q.to_ms.unwrap_or(i64::MAX),
            before: q.before,
            limit: q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
        }
    }

    /// 这次要读正文吗：要了，而且有搜索词
    fn reads_bodies(&self) -> bool {
        self.content && self.text.is_some()
    }
}

/// 向库要一批行：从哪儿往前、最早到哪一刻、最多几条。
#[derive(Debug, Clone, Copy)]
pub struct Ask<'a> {
    pub query: &'a Query,
    /// 只要这一条之前的
    pub before: Option<HistoryCursor>,
    /// 只要这一刻及以后的（按正文找时，正文最早留到的那一刻）
    pub floor: Option<i64>,
    /// `true`：搜索词不进 `WHERE`，「按记录对上了没有」跟着行一起交回来；`false`：只要
    /// 按记录就对上了的
    pub flag_text: bool,
    pub limit: usize,
}

/// 按正文找时，一次最多做多少事。两样先到哪样算哪样。
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// 最多读几条请求的正文
    pub bodies: usize,
    /// 最多花多久
    pub time: Duration,
}

impl Budget {
    /// 界面上一次搜索的量。
    ///
    /// **条数是常态下的那道线**：一条请求的正文平均几百 KB，读盘、粗筛、解开最后一轮和
    /// 回答，实测（发布构建、Claude Code 那样的长对话）一条半毫秒到一毫秒多，五百条大约
    /// 半秒 —— 结果先回来，界面说「已找到某一刻，再往前找」。**时间是兜底**：正文的开销
    /// 差得很远（一段长对话的请求体几 MB，光解析就几十毫秒，盘慢的时候更久），一点五秒
    /// 是「等一下」还不算「卡住了」的那条线。
    pub const DEFAULT: Budget = Budget {
        bodies: 500,
        time: Duration::from_millis(1500),
    };
}

/// 一次搜索的结果，还没换成界面的样子（行要在 tw-control 里配上安全记录）。
#[derive(Debug, Clone)]
pub struct Outcome {
    pub rows: Vec<RequestRow>,
    pub hits: Vec<ContentHit>,
    pub next: Option<HistoryCursor>,
    pub bodies_since_ms: Option<i64>,
    pub stopped: SearchStop,
}

/// 找一页。`fetch` 向库要一批行（[`crate::Db::search_rows`]）：放在外面是因为库在调用方
/// 的锁里，每批只锁一下；读正文不需要锁。
pub fn run<F>(q: &Query, blobs: &Blobs, budget: Budget, mut fetch: F) -> Result<Outcome, DbError>
where
    F: FnMut(&Ask) -> Result<Vec<(RequestRow, bool)>, DbError>,
{
    let started = Instant::now();
    let mut out = Outcome {
        rows: Vec::new(),
        hits: Vec::new(),
        next: None,
        bodies_since_ms: None,
        stopped: SearchStop::End,
    };
    let mut cursor = q.before;

    // 正文还在盘上的那一段：一条一条看
    if let (true, Some(text), Some(floor)) = (q.reads_bodies(), &q.text, blobs.oldest_ms()) {
        out.bodies_since_ms = Some(floor);
        let needle = Needle::new(text.clone());
        let mut read = 0;
        loop {
            let batch = fetch(&Ask {
                query: q,
                before: cursor,
                floor: Some(floor),
                flag_text: true,
                limit: BATCH,
            })?;
            let n = batch.len();
            for (row, by_record) in batch {
                cursor = Some(HistoryCursor {
                    at_ms: row.at_ms,
                    id: row.id,
                });
                if by_record {
                    out.rows.push(row);
                } else {
                    match look(&row, blobs, &needle) {
                        Look::NoBodies => {}
                        Look::Read(None) => read += 1,
                        Look::Read(Some((side, e))) => {
                            read += 1;
                            out.hits.push(ContentHit {
                                id: row.id,
                                side,
                                before: e.before,
                                matched: e.matched,
                                after: e.after,
                            });
                            out.rows.push(row);
                        }
                    }
                }
                if out.rows.len() >= q.limit {
                    out.next = cursor;
                    out.stopped = SearchStop::Full;
                    return Ok(out);
                }
                if read >= budget.bodies || started.elapsed() >= budget.time {
                    out.next = cursor;
                    out.stopped = SearchStop::Budget;
                    return Ok(out);
                }
            }
            if n < BATCH {
                break;
            }
        }
    }

    // 剩下的（或者全部，没按正文找时）只按记录对：一句 SQL。多要一条，看后面还有没有
    let want = q.limit - out.rows.len();
    let mut rest = fetch(&Ask {
        query: q,
        before: cursor,
        floor: None,
        flag_text: false,
        limit: want + 1,
    })?;
    if rest.len() > want {
        rest.truncate(want);
        out.next = rest.last().map(|(r, _)| HistoryCursor {
            at_ms: r.at_ms,
            id: r.id,
        });
        out.stopped = SearchStop::Full;
    }
    out.rows.extend(rest.into_iter().map(|(r, _)| r));
    Ok(out)
}

/// 看一条请求的正文的结果。
enum Look {
    /// 没有可读的正文：清掉了、没存下来，或者不是生成回答的调用。不算进读了几份
    NoBodies,
    /// 读了。对上的话，在哪一边、前后是什么
    Read(Option<(ContentSide, Excerpt)>),
}

/// 先看请求里新的那一轮，再看回答：一轮话里先有问、后有答。
///
/// 原始字节上先粗筛（[`Needle::might_be_in`]）：绝大多数请求体里根本没有这个词，不必解析。
/// 流式的回答不能这样筛 —— 一个词可能被切在两帧里。
fn look(row: &RequestRow, blobs: &Blobs, needle: &Needle) -> Look {
    let Some(client) = text::client_dialect(&row.path) else {
        return Look::NoBodies;
    };
    let request = blobs.get(row.at_ms, row.id, Which::Request);
    if let Some(body) = &request
        && needle.might_be_in(body)
        && let Some(turn) = text::user_turn(body, &row.path)
        && let Some(e) = needle.excerpt(&turn)
    {
        return Look::Read(Some((ContentSide::Request, e)));
    }
    let Some(body) = blobs.get(row.at_ms, row.id, Which::Response) else {
        return match request {
            Some(_) => Look::Read(None),
            None => Look::NoBodies,
        };
    };
    // 回答它的那一家说的格式：转换过的记在行上，直通的就是客户端那一种
    let upstream = row
        .translated
        .as_deref()
        .and_then(|j| serde_json::from_str::<tw_api::TranslatedView>(j).ok())
        .map_or(client, |t| text::dialect_of(t.to));
    let whole = body.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{');
    let hit = (!whole || needle.might_be_in(&body))
        .then(|| text::answer(&body, upstream))
        .flatten()
        .and_then(|a| needle.excerpt(&a));
    Look::Read(hit.map(|e| (ContentSide::Answer, e)))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use tw_api::Msg;

    use super::*;
    use crate::db::Db;
    use crate::db::tests::row;

    const DAY: i64 = 86_400_000;
    /// 测试里「今天」：正文目录按这一天建
    const NOW: i64 = 1_790_000_000_000;

    fn ask(q: tw_api::HistorySearchQuery) -> Query {
        Query::new(&q)
    }

    fn search(db: &Db, blobs: &Blobs, q: &Query, budget: Budget) -> Outcome {
        run(q, blobs, budget, |a| db.search_rows(a)).unwrap()
    }

    /// 只按记录找，一页拿完
    fn ids(db: &Db, q: tw_api::HistorySearchQuery) -> Vec<i64> {
        let d = tempfile::tempdir().unwrap();
        let blobs = Blobs::new(d.path().join("blobs"));
        let q = Query::new(&tw_api::HistorySearchQuery {
            limit: Some(MAX_LIMIT),
            ..q
        });
        search(db, &blobs, &q, Budget::DEFAULT)
            .rows
            .iter()
            .map(|r| r.id)
            .collect()
    }

    fn err(code: &str, text: &str) -> Msg {
        Msg {
            code: code.into(),
            args: BTreeMap::new(),
            text: text.into(),
        }
    }

    // ───────────────────────────────────────────────── 和界面的筛选一模一样

    /// 界面那一边：桌面端 `useRequests.ts` 把一条 `HistoryRow` 变成表格的一行，
    /// `requestTable.ts` 的 `filterRows` 筛它。**逐句照抄**，测试拿它当标准答案。
    mod ui {
        use crate::db::RequestRow;

        /// 本地应答那一格写的话（中文界面）
        pub const ANSWERED_LOCALLY: &str = "本地应答";

        pub struct Filter<'a> {
            pub q: &'a str,
            pub failed_only: bool,
            pub unpriced_only: bool,
            pub client: &'a str,
            pub provider: &'a str,
            pub model: &'a str,
        }

        /// `coreText(r.error)`：英文界面上就是那句英文
        fn core_text(r: &RequestRow) -> String {
            r.error.as_ref().map(|e| e.text.clone()).unwrap_or_default()
        }

        pub fn keeps(r: &RequestRow, f: &Filter) -> bool {
            // mergeHistory：state、provider（本地应答没有上游）、model（空串当没有）
            let state = if r.error.is_some() {
                "failed"
            } else if r.cancelled {
                "cancelled"
            } else {
                "done"
            };
            let provider = if r.local { "" } else { r.provider.as_str() };
            let model = (!r.model.is_empty()).then_some(r.model.as_str());
            // filterRows
            let q = super::js_trim(f.q).to_lowercase();
            if f.failed_only && state != "failed" {
                return false;
            }
            if f.unpriced_only
                && (r.cost_micros.is_some()
                    || state != "done"
                    || (r.input_tokens.is_none() && r.output_tokens.is_none()))
            {
                return false;
            }
            if !f.client.is_empty() && r.client != f.client {
                return false;
            }
            if !f.provider.is_empty() && provider != f.provider {
                return false;
            }
            if !f.model.is_empty() && model != Some(f.model) {
                return false;
            }
            if q.is_empty() {
                return true;
            }
            let upstream = if r.local { ANSWERED_LOCALLY } else { provider };
            r.path.to_lowercase().contains(&q)
                || r.client.to_lowercase().contains(&q)
                || r.client_hint
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(&q)
                || r.peer.as_deref().unwrap_or("").contains(&q)
                || upstream.to_lowercase().contains(&q)
                || model.unwrap_or("").to_lowercase().contains(&q)
                || core_text(r).to_lowercase().contains(&q)
        }
    }

    use super::needle::js_trim;

    /// 各种样子的行：失败的、取消的、没报用量的、无法计价的、本地应答的（含网关替上游估
    /// token 数的那种：它有上游名，界面上却没有）、局域网来的、名字里有外文大写字母的……
    fn variety() -> (Db, Vec<RequestRow>) {
        let db = Db::in_memory().unwrap();
        let mut rows = Vec::new();
        let clients = ["claude-code", "Max's Mac", "Übersetzer", "50%off", "a_b"];
        let providers = ["官方", "relay-cn", "Яндекс"];
        let models = ["claude-sonnet-4-5", "gpt-5.5", "gpt-5.5-codex", ""];
        let paths = [
            "/v1/messages",
            "/v1/chat/completions",
            "/v1/messages/count_tokens",
        ];
        for i in 0..120i64 {
            let mut r = row(i + 1, NOW - i * 1000);
            r.client = clients[i as usize % clients.len()].into();
            r.provider = providers[i as usize % providers.len()].into();
            r.model = models[i as usize % models.len()].into();
            r.path = paths[i as usize % paths.len()].into();
            r.client_hint = match i % 4 {
                0 => Some("claude-code".into()),
                1 => Some("codex".into()),
                _ => None,
            };
            r.peer = match i % 5 {
                0 => Some("192.168.1.23".into()),
                1 => Some("fe80::1".into()),
                _ => None,
            };
            match i % 7 {
                0 => {
                    r.error = Some(err(
                        "gw.upstream.timeout",
                        "Upstream `relay-cn` did not answer within 600s",
                    ))
                }
                1 => r.error = Some(err("gw.denied.rule", "Rule `夜间` denied the request")),
                2 => r.cancelled = true,
                3 => r.cost_micros = None,
                4 => {
                    r.cost_micros = None;
                    r.input_tokens = None;
                    r.output_tokens = None;
                }
                5 => {
                    r.cost_micros = None;
                    r.input_tokens = None;
                }
                _ => {}
            }
            if i % 11 == 0 {
                r.local = true;
                r.error = None;
                r.cost_micros = Some(0);
            }
            if i % 22 == 0 {
                r.provider = String::new();
                r.model = String::new();
                r.path = "health_check".into();
            }
            db.insert(&r).unwrap();
            rows.push(r);
        }
        (db, rows)
    }

    /// **和界面的筛选一模一样**：每一种筛选、每一个字段上的搜索词，库里找出来的和
    /// `filterRows` 留下的是同一批，顺序也一样（新的在前）。
    #[test]
    fn searching_the_records_keeps_exactly_what_the_traffic_filter_keeps() {
        let (db, rows) = variety();
        let queries = [
            "",
            "   ",
            "claude",
            "CLAUDE",
            " sonnet ",
            "gpt-5.5",
            "über",
            "ÜBERSETZER",
            "яндекс",
            "官方",
            "本地",
            "192.168",
            "FE80",
            "codex",
            "max's",
            "count_tokens",
            "health",
            "600s",
            "夜间",
            "%",
            "_",
            "50%",
            "a_b",
            "nothing-like-this",
        ];
        let filters = [
            (false, false, "", "", ""),
            (true, false, "", "", ""),
            (false, true, "", "", ""),
            (false, false, "Übersetzer", "", ""),
            (false, false, "", "relay-cn", ""),
            (false, false, "", "官方", ""),
            (false, false, "", "", "gpt-5.5"),
            (false, false, "", "", ""),
            (true, false, "claude-code", "Яндекс", ""),
            (false, true, "", "", "claude-sonnet-4-5"),
        ];
        let mut checked = 0;
        for q in queries {
            for (failed, unpriced, client, provider, model) in filters {
                let f = ui::Filter {
                    q,
                    failed_only: failed,
                    unpriced_only: unpriced,
                    client,
                    provider,
                    model,
                };
                let want: Vec<i64> = rows
                    .iter()
                    .filter(|r| ui::keeps(r, &f))
                    .map(|r| r.id)
                    .collect();
                let opt = |s: &str| (!s.is_empty()).then(|| s.to_string());
                let got = ids(
                    &db,
                    tw_api::HistorySearchQuery {
                        q: q.into(),
                        failed,
                        unpriced,
                        client: opt(client),
                        provider: opt(provider),
                        model: opt(model),
                        local_matches: ui::ANSWERED_LOCALLY.contains(&q.trim().to_lowercase()),
                        ..Default::default()
                    },
                );
                assert_eq!(
                    got, want,
                    "q={q:?} failed={failed} unpriced={unpriced} client={client:?} provider={provider:?} model={model:?}"
                );
                checked += usize::from(!want.is_empty());
            }
        }
        assert!(checked > 100, "大多数组合都该筛出点什么：{checked}");
    }

    /// 失败原因按界面上那句话对：中文界面把译文里含着搜索词的码交过来。英文原句里没有
    /// 这个词，照样对上
    #[test]
    fn an_error_is_found_by_the_words_of_its_translation() {
        let (db, rows) = variety();
        let q = |codes: Vec<&str>| tw_api::HistorySearchQuery {
            q: "超时".into(),
            error_codes: codes.into_iter().map(String::from).collect(),
            ..Default::default()
        };
        assert!(ids(&db, q(vec![])).is_empty(), "英文原句里没有「超时」");
        let got = ids(&db, q(vec!["gw.upstream.timeout", "gw.never.seen"]));
        let want: Vec<i64> = rows
            .iter()
            .filter(|r| {
                r.error
                    .as_ref()
                    .is_some_and(|e| e.code == "gw.upstream.timeout")
            })
            .map(|r| r.id)
            .collect();
        assert!(!want.is_empty());
        assert_eq!(got, want);
    }

    /// 本地应答的那一格写的是界面自己的一句话：那句话对得上时，本地应答的都算；它们的
    /// 上游名（网关替上游估 token 数的那种有）不参与
    #[test]
    fn local_answers_are_found_by_the_sentence_the_ui_shows_for_them() {
        let (db, rows) = variety();
        let local: Vec<i64> = rows.iter().filter(|r| r.local).map(|r| r.id).collect();
        assert!(local.len() > 3);
        let got = ids(
            &db,
            tw_api::HistorySearchQuery {
                q: "本地".into(),
                local_matches: true,
                ..Default::default()
            },
        );
        assert_eq!(got, local);
        let got = ids(
            &db,
            tw_api::HistorySearchQuery {
                q: "官方".into(),
                ..Default::default()
            },
        );
        assert!(got.iter().all(|id| !local.contains(id)), "{got:?}");
    }

    /// `%` 和 `_` 是字面的字，不是通配符
    #[test]
    fn percent_and_underscore_in_the_query_are_taken_literally() {
        let db = Db::in_memory().unwrap();
        for (id, client) in [
            (1, "50%off"),
            (2, "500ff"),
            (3, "a_b"),
            (4, "axb"),
            (5, "a%b"),
        ] {
            let mut r = row(id, NOW - id);
            r.client = client.into();
            r.model = "m".into();
            r.path = "/p".into();
            r.provider = "x".into();
            db.insert(&r).unwrap();
        }
        let q = |s: &str| {
            ids(
                &db,
                tw_api::HistorySearchQuery {
                    q: s.into(),
                    ..Default::default()
                },
            )
        };
        assert_eq!(q("50%"), [1]);
        assert_eq!(q("a_b"), [3]);
        assert_eq!(q("%"), [1, 5]);
        assert_eq!(q("_"), [3]);
        assert_eq!(q("\\"), Vec::<i64>::new());
    }

    // ───────────────────────────────────────────────── 翻页

    /// 同一毫秒里开始的几条落在页缝上：一条不漏、一条不重，顺序和 `GET /history` 一样
    #[test]
    fn paging_back_through_rows_that_share_a_timestamp_misses_and_repeats_nothing() {
        let db = Db::in_memory().unwrap();
        let mut want = Vec::new();
        // 三个时刻，每个时刻七条
        for (k, at) in [NOW, NOW - 10, NOW - 20].into_iter().enumerate() {
            for j in 0..7 {
                let id = (k * 7 + j) as i64 + 1;
                db.insert(&row(id, at)).unwrap();
                want.push((at, id));
            }
        }
        want.sort_by(|a, b| b.cmp(a));
        let want: Vec<i64> = want.into_iter().map(|(_, id)| id).collect();
        let d = tempfile::tempdir().unwrap();
        let blobs = Blobs::new(d.path().join("blobs"));
        for size in [1, 2, 3, 5, 7, 20] {
            let mut got = Vec::new();
            let mut before = None;
            let mut pages = 0;
            loop {
                let q = ask(tw_api::HistorySearchQuery {
                    before,
                    limit: Some(size),
                    ..Default::default()
                });
                let page = search(&db, &blobs, &q, Budget::DEFAULT);
                pages += 1;
                assert!(page.rows.len() <= size);
                got.extend(page.rows.iter().map(|r| r.id));
                match page.next {
                    Some(c) => {
                        assert_eq!(page.stopped, SearchStop::Full);
                        let last = page.rows.last().unwrap();
                        assert_eq!((c.at_ms, c.id), (last.at_ms, last.id));
                        before = Some(c);
                    }
                    None => {
                        assert_eq!(page.stopped, SearchStop::End);
                        break;
                    }
                }
                assert!(pages < 100);
            }
            assert_eq!(got, want, "一页 {size} 条");
            assert_eq!(pages, want.len().div_ceil(size), "一页 {size} 条");
        }
        // 和 `GET /history` 同一个顺序
        let recent: Vec<i64> = db.recent(None, 100).unwrap().iter().map(|r| r.id).collect();
        assert_eq!(recent, want);
    }

    #[test]
    fn a_time_window_bounds_the_search_on_both_ends() {
        let db = Db::in_memory().unwrap();
        for i in 0..10 {
            db.insert(&row(i + 1, NOW + i * 100)).unwrap();
        }
        let got = ids(
            &db,
            tw_api::HistorySearchQuery {
                from_ms: Some(NOW + 200),
                to_ms: Some(NOW + 500),
                ..Default::default()
            },
        );
        assert_eq!(got, [6, 5, 4, 3]);
    }

    #[test]
    fn a_page_holds_a_hundred_by_default_and_at_most_five_hundred() {
        let db = Db::in_memory().unwrap();
        for i in 0..600 {
            db.insert(&row(i + 1, NOW - i)).unwrap();
        }
        let d = tempfile::tempdir().unwrap();
        let blobs = Blobs::new(d.path().join("blobs"));
        let page = |limit| {
            search(
                &db,
                &blobs,
                &ask(tw_api::HistorySearchQuery {
                    limit,
                    ..Default::default()
                }),
                Budget::DEFAULT,
            )
        };
        let p = page(None);
        assert_eq!(p.rows.len(), DEFAULT_LIMIT);
        assert_eq!(p.stopped, SearchStop::Full);
        assert_eq!(p.next.unwrap().id, 100);
        assert_eq!(page(Some(10_000)).rows.len(), MAX_LIMIT);
        assert_eq!(page(Some(0)).rows.len(), 1);
        // 正好取完的那一页说找完了，不留一个空的下一页
        let p = search(
            &db,
            &blobs,
            &ask(tw_api::HistorySearchQuery {
                before: Some(HistoryCursor {
                    at_ms: NOW - 499,
                    id: 500,
                }),
                limit: Some(100),
                ..Default::default()
            }),
            Budget::DEFAULT,
        );
        assert_eq!(
            (p.rows.len(), p.stopped, p.next),
            (100, SearchStop::End, None)
        );
    }

    /// 三个月十万条记录，只按记录找、一条都对不上的时候，也是一次沿着索引走到头。
    ///
    /// **门槛放得很宽**：测试是调试构建、CI 的机器也慢，挡的是「一条一条拿回 Rust 里比」
    /// 那种量级的退化。实测的数字见 PR（发布构建下十万条一条都对不上约几十毫秒）。
    #[test]
    fn searching_a_hundred_thousand_rows_stays_fast() {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(&d.path().join("data.db")).unwrap();
        hundred_thousand(&db);
        let blobs = Blobs::new(d.path().join("blobs"));
        let q = ask(tw_api::HistorySearchQuery {
            q: "没有这个词".into(),
            ..Default::default()
        });
        let t = Instant::now();
        let p = search(&db, &blobs, &q, Budget::DEFAULT);
        let none = t.elapsed();
        assert!(p.rows.is_empty() && p.stopped == SearchStop::End);
        let q = ask(tw_api::HistorySearchQuery {
            q: "Overloaded".into(),
            ..Default::default()
        });
        let t = Instant::now();
        let p = search(&db, &blobs, &q, Budget::DEFAULT);
        let page = t.elapsed();
        assert_eq!(p.rows.len(), DEFAULT_LIMIT);
        eprintln!("100k rows: no match {none:?}, first page {page:?}");
        assert!(none < Duration::from_secs(10), "{none:?}");
        assert!(page < Duration::from_secs(2), "{page:?}");
    }

    /// 十万条，一分钟一条往回排：三种密钥（一种名字带中文）、每十三条一条失败
    fn hundred_thousand(db: &Db) {
        db.conn()
        .execute(
            "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < 99999)
             INSERT INTO requests (id, at_ms, client, client_hint, provider, model, path, status,
                cost_estimated, billing, local, cancelled, input_tokens, output_tokens, cost_micros,
                error, error_code)
             SELECT i + 1, ?1 - i * 60000,
                CASE i % 3 WHEN 0 THEN 'claude-code' WHEN 1 THEN 'codex' ELSE 'Cursor 编辑器' END,
                CASE i % 2 WHEN 0 THEN 'claude-code' END,
                '官方', 'claude-sonnet-4-5', '/v1/messages', 200, 0, 'per-token', 0, 0, 1000, 200, 300,
                CASE WHEN i % 13 = 0 THEN 'Upstream `官方` answered HTTP 529: overloaded' END,
                CASE WHEN i % 13 = 0 THEN 'gw.upstream.http' END
             FROM n",
            [NOW],
        )
        .unwrap();
    }

    /// 查询计划：每一种问法都沿着时刻的索引走，不整表扫、不整表排序
    #[test]
    fn the_search_walks_the_time_index() {
        let db = Db::in_memory().unwrap();
        let full = ask(tw_api::HistorySearchQuery {
            q: "x".into(),
            failed: true,
            unpriced: true,
            client: Some("c".into()),
            provider: Some("p".into()),
            model: Some("m".into()),
            error_codes: vec!["a.b".into()],
            from_ms: Some(1),
            to_ms: Some(9),
            ..Default::default()
        });
        let bare = ask(Default::default());
        let before = Some(HistoryCursor { at_ms: 5, id: 3 });
        for (q, before, floor, flag_text) in [
            (&bare, None, None, false),
            (&full, before, None, false),
            (&full, before, Some(2), true),
            (&bare, before, Some(2), true),
        ] {
            let (sql, args) = super::sql::statement(&Ask {
                query: q,
                before,
                floor,
                flag_text,
                limit: 101,
            });
            let params: Vec<(&str, &dyn rusqlite::ToSql)> =
                args.iter().map(|(k, v)| (*k, v.as_ref())).collect();
            let plan: Vec<String> = db
                .conn()
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map(params.as_slice(), |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            let plan = plan.join(" / ");
            assert!(plan.contains("USING INDEX requests_at"), "{plan}\n{sql}");
            assert!(!plan.contains("SCAN requests"), "{plan}\n{sql}");
            // 同一毫秒里的几条按请求号排，不是把结果整个排一遍
            assert!(
                !plan.contains("USE TEMP B-TREE FOR ORDER BY"),
                "{plan}\n{sql}"
            );
            // 真跑一遍：每个按名字绑定的参数都用得上
            db.search_rows(&Ask {
                query: q,
                before,
                floor,
                flag_text,
                limit: 101,
            })
            .unwrap();
        }
    }

    // ───────────────────────────────────────────────── 按正文找

    /// 一份测试用的盘：一个请求库，一个正文目录
    struct Disk {
        _dir: tempfile::TempDir,
        db: Db,
        blobs: Blobs,
    }

    fn disk() -> Disk {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::new(dir.path().join("blobs"));
        Disk {
            db: Db::in_memory().unwrap(),
            blobs,
            _dir: dir,
        }
    }

    impl Disk {
        /// 一条带着正文的请求。`response` 空的是没存下回答
        fn put(&self, r: &RequestRow, request: &[u8], response: &[u8]) {
            self.db.insert(r).unwrap();
            if !request.is_empty() {
                assert!(self.blobs.put(r.at_ms, r.id, Which::Request, request));
            }
            if !response.is_empty() {
                assert!(self.blobs.put(r.at_ms, r.id, Which::Response, response));
            }
        }

        fn find(&self, q: &str) -> Outcome {
            self.find_with(q, Budget::DEFAULT, None)
        }

        fn find_with(&self, q: &str, budget: Budget, before: Option<HistoryCursor>) -> Outcome {
            let q = ask(tw_api::HistorySearchQuery {
                q: q.into(),
                content: true,
                before,
                ..Default::default()
            });
            search(&self.db, &self.blobs, &q, budget)
        }
    }

    fn at(id: i64, at_ms: i64, path: &str) -> RequestRow {
        let mut r = row(id, at_ms);
        r.path = path.into();
        r
    }

    fn sse(frames: &[(&str, serde_json::Value)]) -> Vec<u8> {
        let mut s = String::new();
        for (event, data) in frames {
            if !event.is_empty() {
                s.push_str(&format!("event: {event}\n"));
            }
            s.push_str(&format!("data: {data}\n\n"));
        }
        s.into_bytes()
    }

    fn hit_of(o: &Outcome, id: i64) -> &ContentHit {
        o.hits
            .iter()
            .find(|h| h.id == id)
            .unwrap_or_else(|| panic!("{id} 没有按正文对上：{:?}", o.hits))
    }

    /// 四种格式，流式和整包，问在请求里、答在回答里，都找得到；`\u` 转义的中文按中文找
    #[test]
    fn content_is_found_in_every_format_streamed_or_whole() {
        let d = disk();
        let user =
            |text: &str| json!({"model": "m", "messages": [{"role": "user", "content": text}]});
        // Anthropic，流式回答
        d.put(
        &at(1, NOW - 1, "/v1/messages"),
        user("你好").to_string().as_bytes(),
        &sse(&[
            ("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})),
            ("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "答案藏在 ZEBRA"}})),
            ("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "-STRIPE 里"}})),
        ]),
    );
        // Chat，整包回答
        d.put(
        &at(2, NOW - 2, "/v1/chat/completions"),
        user("问").to_string().as_bytes(),
        json!({"choices": [{"message": {"role": "assistant", "content": "整包里也有 zebra-stripe"}}]})
            .to_string()
            .as_bytes(),
    );
        // Responses，问题里有（Python 把中文全写成 \u 转义）
        d.put(
        &at(3, NOW - 3, "/v1/responses"),
        br#"{"model":"gpt-5.5","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"\u6591\u9a6c\u7eb9 zebra-stripe \u5728\u54ea"}]}]}"#,
        b"",
    );
        // Gemini，流式回答里是 \u 转义的中文
        d.put(
        &at(4, NOW - 4, "/v1beta/models/gemini-2.5-pro:streamGenerateContent"),
        json!({"contents": [{"role": "user", "parts": [{"text": "问"}]}]}).to_string().as_bytes(),
        b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"\\u6591\\u9a6c\"}]}}]}\n\ndata: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"\\u7eb9\"}]}}]}\n\n",
    );
        // 一条什么都对不上的
        d.put(
        &at(5, NOW - 5, "/v1/messages"),
        user("无关").to_string().as_bytes(),
        &sse(&[("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "无关"}}))]),
    );

        let o = d.find("Zebra-Stripe");
        assert_eq!(o.rows.iter().map(|r| r.id).collect::<Vec<_>>(), [1, 2, 3]);
        // 流里切成两帧的那个词照样对上
        let h = hit_of(&o, 1);
        assert_eq!(h.side, ContentSide::Answer);
        assert_eq!(
            (h.before.as_str(), h.matched.as_str(), h.after.as_str()),
            ("答案藏在 ", "ZEBRA-STRIPE", " 里")
        );
        assert_eq!(hit_of(&o, 2).side, ContentSide::Answer);
        let h = hit_of(&o, 3);
        assert_eq!(h.side, ContentSide::Request);
        assert_eq!(h.before, "斑马纹 ");

        let o = d.find("斑马纹");
        assert_eq!(o.rows.iter().map(|r| r.id).collect::<Vec<_>>(), [3, 4]);
        assert_eq!(hit_of(&o, 4).side, ContentSide::Answer);
        assert_eq!(hit_of(&o, 4).matched, "斑马纹");
        assert_eq!(o.stopped, SearchStop::End);
        assert_eq!(o.bodies_since_ms, Some(NOW - NOW.rem_euclid(DAY)));
    }

    /// 一段长对话：早先一轮里说过的话，不让之后每个请求都对上；说在最后一轮里的才算
    #[test]
    fn only_the_last_user_turn_of_a_long_conversation_counts() {
        let d = disk();
        let mut messages = vec![json!({"role": "user", "content": "第一轮就提到了 KIWI-42"})];
        for id in 1..=5i64 {
            let mut turn = messages.clone();
            turn.push(json!({"role": "user", "content": format!("第 {id} 轮的新问题")}));
            d.put(
                &at(id, NOW + id, "/v1/messages"),
                json!({"messages": turn}).to_string().as_bytes(),
                b"",
            );
            messages.push(json!({"role": "user", "content": format!("第 {id} 轮的新问题")}));
            messages.push(json!({"role": "assistant", "content": format!("第 {id} 轮的回答")}));
        }
        // 第一个请求：那句话就在它的最后一轮里（两条连着的用户消息都算这一轮）
        let o = d.find("kiwi-42");
        assert_eq!(o.rows.iter().map(|r| r.id).collect::<Vec<_>>(), [1]);
        let o = d.find("第 4 轮的新问题");
        assert_eq!(o.rows.iter().map(|r| r.id).collect::<Vec<_>>(), [4]);
    }

    /// 转换过格式的请求：回答是上游的原话，按上游的格式读
    #[test]
    fn a_translated_answer_is_read_in_the_upstream_format() {
        let d = disk();
        let mut r = at(1, NOW, "/v1/messages");
        r.translated = Some(
            serde_json::to_string(&tw_api::TranslatedView {
                from: tw_api::Dialect::Anthropic,
                to: tw_api::Dialect::OpenaiChat,
                dropped: vec![],
            })
            .unwrap(),
        );
        let mut body = sse(&[
            (
                "",
                json!({"choices": [{"index": 0, "delta": {"content": "来自 Chat 的"}}]}),
            ),
            (
                "",
                json!({"choices": [{"index": 0, "delta": {"content": "回答"}}]}),
            ),
        ]);
        body.extend_from_slice(b"data: [DONE]\n\n");
        d.put(
            &r,
            json!({"messages": [{"role": "user", "content": "问"}]})
                .to_string()
                .as_bytes(),
            &body,
        );
        let o = d.find("chat 的回答");
        assert_eq!(hit_of(&o, 1).side, ContentSide::Answer);
    }

    /// 编程客户端那边，模型做的事是工具调用：「哪一次跑了 npm install」要找得到 ——
    /// 流里的参数片段接起来找，整包回答里的参数（Chat 的写成 JSON 字符串里的 JSON）也找
    #[test]
    fn the_request_where_the_model_ran_a_command_is_found_by_its_tool_call() {
        let d = disk();
        let question = json!({"messages": [{"role": "user", "content": "装一下依赖"}]}).to_string();
        d.put(
            &at(1, NOW, "/v1/messages"),
            question.as_bytes(),
            &sse(&[
                ("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})),
                ("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "好的"}})),
                ("content_block_start", json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "t", "name": "Bash", "input": {}}})),
                ("content_block_delta", json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"command\": \"npm ins"}})),
                ("content_block_delta", json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "tall\"}"}})),
            ]),
        );
        d.put(
            &at(2, NOW - 1, "/v1/chat/completions"),
            question.as_bytes(),
            json!({"choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c", "type": "function", "function": {"name": "shell", "arguments": "{\"cmd\": \"npm install\"}"}}]}}]})
            .to_string()
            .as_bytes(),
        );
        // 只想了想、没有动手的那一条不算
        d.put(
            &at(3, NOW - 2, "/v1/messages"),
            question.as_bytes(),
            &sse(&[
                ("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}})),
                ("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "也许该跑 npm install"}})),
            ]),
        );
        let o = d.find("NPM install");
        assert_eq!(o.rows.iter().map(|r| r.id).collect::<Vec<_>>(), [1, 2]);
        let h = hit_of(&o, 1);
        assert_eq!(h.side, ContentSide::Answer);
        assert_eq!(
            (h.before.as_str(), h.matched.as_str(), h.after.as_str()),
            ("好的 Bash {\"command\": \"", "npm install", "\"}")
        );
        let h = hit_of(&o, 2);
        assert_eq!(
            (h.side, h.before.as_str()),
            (ContentSide::Answer, "shell {\"cmd\":\"")
        );
    }

    /// 工具调用的参数里带着密钥（命令里 export 了一把）：摘录照样打码，只在密钥里的词不算
    #[test]
    fn a_secret_in_a_tool_call_is_masked_in_the_excerpt() {
        let d = disk();
        let key = "sk-proj-TOOLKEYAAAAAAAAAAAAAAAAAAAAAAAA";
        d.put(
            &at(1, NOW, "/v1/messages"),
            json!({"messages": [{"role": "user", "content": "部署"}]}).to_string().as_bytes(),
            &sse(&[
                ("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t", "name": "Bash", "input": {}}})),
                ("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta",
                    "partial_json": format!("{{\"command\": \"OPENAI_API_KEY={key} npm run deploy\"}}")}})),
            ]),
        );
        let o = d.find("npm run deploy");
        let h = hit_of(&o, 1);
        assert!(!h.before.contains("TOOLKEYAAAA"), "{h:?}");
        assert!(h.before.contains("OPENAI_API_KEY=sk-pr…"), "{h:?}");
        assert!(d.find("toolkeyaaaa").rows.is_empty());
    }

    /// 问和答里都有：报问里那一处
    #[test]
    fn a_hit_in_the_request_comes_before_one_in_the_answer() {
        let d = disk();
        d.put(
            &at(1, NOW, "/v1/chat/completions"),
            json!({"messages": [{"role": "user", "content": "什么是 rust"}]})
                .to_string()
                .as_bytes(),
            json!({"choices": [{"message": {"content": "Rust 是一门语言"}}]})
                .to_string()
                .as_bytes(),
        );
        let o = d.find("rust");
        let h = hit_of(&o, 1);
        assert_eq!(
            (h.side, h.before.as_str()),
            (ContentSide::Request, "什么是 ")
        );
    }

    /// 按记录就对上的不去读正文，也不交摘录；按正文对上的才有
    #[test]
    fn a_row_found_by_its_record_is_not_read_for_content() {
        let d = disk();
        let mut r = at(1, NOW, "/v1/messages");
        r.client = "needle-client".into();
        d.put(
            &r,
            json!({"messages": [{"role": "user", "content": "needle 也在正文里"}]})
                .to_string()
                .as_bytes(),
            b"",
        );
        let o = d.find("needle");
        assert_eq!(o.rows.len(), 1);
        assert!(o.hits.is_empty(), "{:?}", o.hits);
    }

    /// 筛选条件对按正文找的那一段一样管用
    #[test]
    fn filters_apply_to_content_hits_too() {
        let d = disk();
        let mut failed = at(1, NOW, "/v1/messages");
        failed.error = Some(err("gw.x", "boom"));
        let ok = at(2, NOW - 1, "/v1/messages");
        for r in [&failed, &ok] {
            d.put(
                r,
                json!({"messages": [{"role": "user", "content": "共同的 PHRASE"}]})
                    .to_string()
                    .as_bytes(),
                b"",
            );
        }
        let q = ask(tw_api::HistorySearchQuery {
            q: "phrase".into(),
            content: true,
            failed: true,
            ..Default::default()
        });
        let o = search(&d.db, &d.blobs, &q, Budget::DEFAULT);
        assert_eq!(o.rows.iter().map(|r| r.id).collect::<Vec<_>>(), [1]);
    }

    /// 一次只读这么多份正文：读够了先交回来，`next` 说从哪儿接着找；接着找下去，一条
    /// 不漏地找到头
    #[test]
    fn content_search_stops_at_its_budget_and_picks_up_where_it_left_off() {
        let d = disk();
        for id in 1..=10i64 {
            let text = if id == 2 || id == 9 {
                "目标 OKAPI 在这"
            } else {
                "没有"
            };
            d.put(
                &at(id, NOW + id, "/v1/messages"),
                json!({"messages": [{"role": "user", "content": text}]})
                    .to_string()
                    .as_bytes(),
                b"",
            );
        }
        let budget = Budget {
            bodies: 3,
            time: Duration::from_secs(60),
        };
        let mut found = Vec::new();
        let mut before = None;
        let mut calls = Vec::new();
        loop {
            let o = d.find_with("okapi", budget, before);
            found.extend(o.rows.iter().map(|r| r.id));
            calls.push((o.stopped, o.next.map(|c| c.id)));
            match o.next {
                Some(c) => before = Some(c),
                None => break,
            }
        }
        assert_eq!(found, [9, 2]);
        assert_eq!(
            calls,
            [
                (SearchStop::Budget, Some(8)),
                (SearchStop::Budget, Some(5)),
                (SearchStop::Budget, Some(2)),
                (SearchStop::End, None),
            ]
        );
        // 时间用完也一样停
        let o = d.find_with(
            "okapi",
            Budget {
                bodies: 100,
                time: Duration::ZERO,
            },
            None,
        );
        assert_eq!(
            (o.stopped, o.next.map(|c| c.id)),
            (SearchStop::Budget, Some(10))
        );
    }

    /// 凑够一页就停，`next` 指着这一页最后一条
    #[test]
    fn a_full_page_of_content_hits_stops_there() {
        let d = disk();
        for id in 1..=5i64 {
            d.put(
                &at(id, NOW + id, "/v1/messages"),
                json!({"messages": [{"role": "user", "content": "每条都有 LYNX"}]})
                    .to_string()
                    .as_bytes(),
                b"",
            );
        }
        let q = ask(tw_api::HistorySearchQuery {
            q: "lynx".into(),
            content: true,
            limit: Some(2),
            ..Default::default()
        });
        let o = search(&d.db, &d.blobs, &q, Budget::DEFAULT);
        assert_eq!(o.rows.iter().map(|r| r.id).collect::<Vec<_>>(), [5, 4]);
        assert_eq!(
            (o.stopped, o.next.map(|c| c.id)),
            (SearchStop::Full, Some(4))
        );
    }

    /// 正文清掉了的那些（比盘上最老的一天还早）不去盘上问，但按记录照样找得到；
    /// 只在正文里有的词找不到它们
    #[test]
    fn rows_whose_bodies_were_pruned_are_still_found_by_their_record() {
        let d = disk();
        // 今天的两条有正文
        d.put(
            &at(10, NOW, "/v1/messages"),
            json!({"messages": [{"role": "user", "content": "正文里的 MARMOT"}]})
                .to_string()
                .as_bytes(),
            b"",
        );
        d.put(&at(9, NOW - 1, "/v1/messages"), b"{}", b"");
        // 十天前的：正文早清了
        for id in 1..=3i64 {
            let mut r = at(id, NOW - 10 * DAY - id, "/v1/messages");
            if id == 2 {
                r.client = "marmot-laptop".into();
            }
            d.db.insert(&r).unwrap();
        }
        let o = d.find("marmot");
        assert_eq!(o.rows.iter().map(|r| r.id).collect::<Vec<_>>(), [10, 2]);
        assert_eq!(o.hits.len(), 1);
        assert_eq!(o.bodies_since_ms, Some(NOW - NOW.rem_euclid(DAY)));
        assert_eq!(o.stopped, SearchStop::End);
        // 正文那一段之外不计入读了几份：读的量只花在有正文的那几条上
        let o = d.find_with(
            "marmot",
            Budget {
                bodies: 2,
                time: Duration::from_secs(60),
            },
            None,
        );
        assert_eq!(o.rows.iter().map(|r| r.id).collect::<Vec<_>>(), [10]);
        assert_eq!(
            (o.stopped, o.next.map(|c| c.id)),
            (SearchStop::Budget, Some(9))
        );
        let o = d.find_with("marmot", Budget::DEFAULT, o.next);
        assert_eq!(o.rows.iter().map(|r| r.id).collect::<Vec<_>>(), [2]);
    }

    /// 一份正文都没有的盘：按正文找就是按记录找，不报正文留到哪一刻
    #[test]
    fn without_any_bodies_content_search_is_a_record_search() {
        let d = disk();
        let mut r = at(1, NOW, "/v1/messages");
        r.model = "walrus-1".into();
        d.db.insert(&r).unwrap();
        let o = d.find("walrus");
        assert_eq!(o.rows.len(), 1);
        assert_eq!(o.bodies_since_ms, None);
        // 没有搜索词时不读正文
        let q = ask(tw_api::HistorySearchQuery {
            content: true,
            ..Default::default()
        });
        assert_eq!(
            search(&d.db, &d.blobs, &q, Budget::DEFAULT).bodies_since_ms,
            None
        );
    }

    /// 数 token 的请求和下一个请求带着同一段对话：不读它，免得一句话对上两条
    #[test]
    fn token_counts_are_not_read_for_content() {
        let d = disk();
        let body = json!({"messages": [{"role": "user", "content": "同一句 BEAVER"}]}).to_string();
        d.put(
            &at(1, NOW, "/v1/messages/count_tokens"),
            body.as_bytes(),
            b"",
        );
        d.put(&at(2, NOW - 1, "/v1/messages"), body.as_bytes(), b"");
        let o = d.find("beaver");
        assert_eq!(o.rows.iter().map(|r| r.id).collect::<Vec<_>>(), [2]);
    }

    /// 摘录和详情抽屉一样打码；只在密钥里出现的词不算对上
    #[test]
    fn content_hits_never_carry_a_secret() {
        let d = disk();
        let key = "sk-ant-api03-SEARCHKEYAAAAAAAAAAAAAAAAAAAAAA";
        d.put(
            &at(1, NOW, "/v1/messages"),
            json!({"messages": [{"role": "user", "content": format!("我的 key 是 {key}")}]})
                .to_string()
                .as_bytes(),
            b"",
        );
        let o = d.find("我的 key");
        let h = hit_of(&o, 1);
        assert!(!h.after.contains("SEARCHKEYAAAA"), "{h:?}");
        assert!(d.find("searchkeyaaaa").rows.is_empty());
    }

    #[test]
    fn security_records_come_back_for_exactly_the_requests_asked_for() {
        let db = Db::in_memory().unwrap();
        for (id, req) in [(1, 10), (2, 20), (3, 10), (4, 30)] {
            db.insert_security_event(&crate::db::SecurityEvent {
                at_ms: NOW + id,
                request_id: req,
                guard: tw_api::Guard::Redact,
                rule: "anthropic".into(),
                custom: false,
                action: tw_api::SecurityOutcome::Replaced,
                provider: "官方".into(),
                client: "me".into(),
                tool: None,
                excerpt: "sk-an…".into(),
                count: 1,
            })
            .unwrap();
        }
        let got = db.security_of(&[10, 30, 99]).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[&10].len(), 2);
        assert_eq!(got[&30][0].id, 4);
        assert!(db.security_of(&[]).unwrap().is_empty());
    }

    // ───────────────────────────────────────────────── 量一量

    /// 按正文找要花多少：照 Claude Code 的样子造几段长对话（每轮带着整段历史、工具结果
    /// 几 KB 到十几 KB、流式回答里有推理和工具调用），量一条请求的正文平均要多久。
    ///
    /// 默认不跑：要写一两百 MB 的正文。发布构建下跑：
    /// `cargo test --release -p tw-store content_search_cost -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn content_search_cost() {
        let d = disk();
        let system = "You are Claude Code. ".repeat(1000);
        let tools: Vec<serde_json::Value> = (0..40)
            .map(|i| {
                json!({"name": format!("Tool{i}"), "description": "x".repeat(600),
            "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}})
            })
            .collect();
        let mut id = 0;
        let mut bytes = 0usize;
        for s in 0..3 {
            let mut messages = vec![
                json!({"role": "user", "content": format!("会话 {s} 的第一句：请重构 FIRST-TURN-{s}")}),
            ];
            for turn in 0..100 {
                id += 1;
                let body = json!({"model": "claude-sonnet-4-5", "system": system, "tools": tools,
                "messages": messages, "max_tokens": 32000, "stream": true})
                .to_string();
                let mut frames = vec![(
                    "message_start",
                    json!({"type": "message_start", "message": {"id": "m", "model": "claude-sonnet-4-5", "usage": {"input_tokens": 10}}}),
                )];
                frames.push(("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}})));
                for k in 0..200 {
                    frames.push(("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": format!("想一想第 {k} 步 ")}})));
                }
                frames.push(("content_block_start", json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}})));
                for k in 0..60 {
                    frames.push(("content_block_delta", json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": format!("我来改第 {k} 处，")}})));
                }
                frames.push(("content_block_start", json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "t", "name": "Edit", "input": {}}})));
                for _ in 0..120 {
                    frames.push(("content_block_delta", json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "fn main() { println!(\\\"hi\\\"); }"}})));
                }
                frames.push(("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 900}})));
                let answer = sse(&frames);
                bytes += body.len() + answer.len();
                d.put(&at(id, NOW + id, "/v1/messages"), body.as_bytes(), &answer);
                messages.push(json!({"role": "assistant", "content": [
                {"type": "text", "text": format!("第 {turn} 轮我来改")},
                {"type": "tool_use", "id": format!("t{turn}"), "name": "Edit", "input": {"code": "y".repeat(1500)}}]}));
                messages.push(json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": format!("t{turn}"),
                "content": format!("{}\n第 {turn} 轮的结果", "文件内容 let x = 1;\n".repeat(300))}]}));
            }
        }
        eprintln!("{id} requests, {} MB of bodies", bytes / 1_000_000);
        let all = Budget {
            bodies: 100_000,
            time: Duration::from_secs(600),
        };
        for (what, q) in [
            ("an ASCII word that is nowhere", "zzqx-never"),
            ("a Chinese word that is nowhere", "从未出现过"),
            (
                "a word from each session's first turn (in every body)",
                "first-turn-1",
            ),
            ("a word in most answers", "我来改第 3 处"),
        ] {
            let t = Instant::now();
            let o = d.find_with(q, all, None);
            let took = t.elapsed();
            eprintln!(
                "{what}: {} rows in {took:?} ({:?} per request), {} hits",
                id,
                took / id as u32,
                o.hits.len()
            );
        }
        let t = Instant::now();
        let o = d.find_with("zzqx-never", Budget::DEFAULT, None);
        eprintln!(
            "default budget: stopped {:?} after {:?} at request {:?}",
            o.stopped,
            t.elapsed(),
            o.next.map(|c| c.id)
        );
    }
}
