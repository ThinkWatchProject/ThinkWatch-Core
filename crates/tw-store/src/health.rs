//! 上游体检：一段时间里每家上游的几项事实（`GET /upstreams/health`）。
//!
//! **只摆事实和参照，不下结论**（见 [`tw_api::UpstreamHealth`]）。这里的每一个数都要
//! 说得清是从哪些请求里数出来的、数了几条：
//!
//! - 失败：口径和概览一样（`error IS NOT NULL`），客户端取消的不算失败，单独数；
//! - 模型名：回答里写了的，归一之后和发出去的比（[`crate::model_name`]）；
//! - 输入之比：上游报的输入（三项加起来）÷ 本地估算，取中位数，再拿别家服务同一个模型
//!   时的中位数作参照 —— 估算只准到两三成，参照才让这个数有意义；
//! - 缓存读：本该读得到缓存的轮次里读到了多少，同样带着别家的参照；
//! - 延迟和速度：口径和按上游分的那两个查询（`latency_by_provider`、
//!   `token_rate_by_provider`）一样。
//!
//! **一遍扫完。**几项分开查的话，同一段行要扫六遍（十万行将近半秒），而查询期间记录
//! 请求的那个任务在等同一把锁。所以窗口里的行按时间顺序读一次，每一项都在这一遍里
//! 攒；中位数的取法和延迟一样，样本排好序之后按最近秩法取（[`crate::db::percentile`]），
//! 给出的永远是一个真实发生过的值。

use std::collections::{BTreeMap, HashMap};

use rusqlite::params;

use crate::db::{Db, DbError, percentile};
use crate::model_name;

/// 本地估算至少这么多 token 的请求才进输入之比的样本。
///
/// 太短的请求里，消息格式自己的那几个 token（角色、分隔符、各家不一样的包装）就能让
/// 比值差出一截，说明不了上游报得准不准。
pub const MIN_ESTIMATE: i64 = 1_000;

/// 后一轮离前一轮开始不超过这么久，前一轮写下的缓存才当还在：Anthropic 和 OpenAI
/// 不被读取的缓存最短都只存五分钟（和 `tw_gateway::affinity` 判断缓存凉没凉是同一个数）。
///
/// 从前一轮**开始**算，不从它结束算：缓存是在上游读完输入的时候写下（或者续上）的，
/// 那一刻离开始最近。从结束算的话，一个生成了四分钟的前一轮会让一轮早就凉了的缓存
/// 看起来还热着。
pub const CACHE_WARM_MS: i64 = 5 * 60 * 1000;

/// 两轮的输入都至少这么多 token，才当可以缓存。
///
/// 几家最小可缓存的长度在 1024 到 4096 之间（不同的模型不一样）。取最大的那个：再短
/// 的那一轮读不到缓存是上游本来的规矩，数进「读不到」就是冤枉它。
pub const MIN_CACHEABLE: i64 = 4_096;

/// 对不上的模型名最多举几对
const EXAMPLES: usize = 3;

impl Db {
    /// 一段时间（`[from_ms, to_ms)`）里每家上游的体检结果。
    ///
    /// 读的行从窗口开始之前 [`CACHE_WARM_MS`] 起：那一截只用来给窗口里的第一轮找它的
    /// 前一轮，不算进任何一项。按（时刻、请求号）的顺序读 —— 和会话里「前一轮」的
    /// 先后是同一个顺序，一次会话最近的那一轮记在手上就够，不用再排一次。
    pub fn upstream_health(
        &self,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<tw_api::UpstreamHealth, DbError> {
        let mut st = self.conn.prepare(
            "SELECT at_ms, provider, sent_model, answered_model, session, cancelled,
                    error IS NOT NULL, input_tokens, COALESCE(cache_read_tokens, 0),
                    COALESCE(cache_write_tokens, 0), input_estimate, ttft_ms, tokens_per_sec
             FROM requests
             WHERE at_ms >= ?1 AND at_ms < ?2 AND local = 0
             ORDER BY at_ms, id",
        )?;
        let mut tally = Tally::default();
        let mut rows = st.query(params![from_ms.saturating_sub(CACHE_WARM_MS), to_ms])?;
        while let Some(r) = rows.next()? {
            tally.add(&Line::read(r)?, from_ms);
        }
        Ok(tw_api::UpstreamHealth {
            from_ms,
            to_ms,
            covered_since_ms: self.covered_since(from_ms, to_ms)?,
            upstreams: tally.finish(),
        })
    }
}

/// 一行里体检要的那几列。字符串借着这一行，不拷
struct Line<'a> {
    at_ms: i64,
    provider: &'a str,
    sent: &'a str,
    answered: Option<&'a str>,
    session: Option<&'a str>,
    cancelled: bool,
    failed: bool,
    /// 没命中缓存的输入。**有它就是报了用量**（落库时四项一起写）
    input: Option<i64>,
    cache_read: i64,
    cache_write: i64,
    estimate: Option<i64>,
    ttft_ms: Option<i64>,
    tokens_per_sec: Option<i64>,
}

impl<'a> Line<'a> {
    fn read(r: &'a rusqlite::Row<'a>) -> rusqlite::Result<Self> {
        Ok(Line {
            at_ms: r.get(0)?,
            provider: r.get_ref(1)?.as_str()?,
            sent: r.get_ref(2)?.as_str()?,
            answered: r.get_ref(3)?.as_str_or_null()?,
            session: r.get_ref(4)?.as_str_or_null()?,
            cancelled: r.get::<_, i64>(5)? != 0,
            failed: r.get::<_, i64>(6)? != 0,
            input: r.get(7)?,
            cache_read: r.get(8)?,
            cache_write: r.get(9)?,
            estimate: r.get(10)?,
            ttft_ms: r.get(11)?,
            tokens_per_sec: r.get(12)?,
        })
    }

    /// 上游报的全部输入：没命中缓存的 + 缓存读 + 缓存写。**几种格式落库时已经换算成
    /// 这三项互不重叠**（OpenAI 和 Gemini 的输入数原本包含缓存命中，见 `tw_dialect`
    /// 各家的 `usage()`），所以三项相加就是上游计费的那个数，不会把缓存算两遍。
    /// 没报用量的是 None
    fn reported_input(&self) -> Option<i64> {
        self.input.map(|i| i + self.cache_read + self.cache_write)
    }
}

/// 一遍扫下来攒的。上游名和模型名一段时间里只有几种，记成编号：每一行不必再分配
#[derive(Default)]
struct Tally {
    names: Names,
    /// 上游的编号 → 那一家攒的
    upstreams: BTreeMap<usize, Upstream>,
    /// 每次会话最近的一轮（报了用量的）
    sessions: HashMap<String, Turn>,
}

/// 一家上游攒的。模型都是发出去的那个名字的编号，归一在最后做：每种写法只归一一次
#[derive(Default)]
struct Upstream {
    requests: i64,
    failed: i64,
    cancelled: i64,
    /// （发出去的，回答里写的）→ 次数
    pairs: HashMap<(usize, usize), i64>,
    /// 发出去的 → 输入之比，千分数
    input: HashMap<usize, Vec<i64>>,
    /// 发出去的 → 本该读得到缓存的轮次
    cache: HashMap<usize, tw_api::CacheTally>,
    ttft_ms: Vec<i64>,
    tokens_per_sec: Vec<i64>,
}

/// 会话里报了用量的一轮：它读过输入，缓存就写下（或者续上）了。失败的、取消的也算
#[derive(Clone, Copy)]
struct Turn {
    upstream: usize,
    sent: usize,
    at_ms: i64,
    total: i64,
}

impl Tally {
    fn add(&mut self, l: &Line<'_>, from_ms: i64) {
        // 规则拒绝了、一家上游都没去的，上游名是空的：不归哪一家，也不算一轮
        if l.provider.is_empty() {
            return;
        }
        let upstream = self.names.id(l.provider);
        let sent = self.names.id(l.sent);
        let in_window = l.at_ms >= from_ms;
        // 这一轮之前，这次会话最近的那一轮。窗口之前的那一截只走到这里
        let previous = match (l.session, l.reported_input()) {
            (Some(session), Some(total)) => {
                let now = Turn {
                    upstream,
                    sent,
                    at_ms: l.at_ms,
                    total,
                };
                match self.sessions.get_mut(session) {
                    Some(t) => Some(std::mem::replace(t, now)),
                    None => {
                        self.sessions.insert(session.to_string(), now);
                        None
                    }
                }
            }
            _ => None,
        };
        if !in_window {
            return;
        }
        let u = self.upstreams.entry(upstream).or_default();
        // 延迟和速度的样本不看结局：口径和 `latency_by_provider` 那两个查询一样
        if let Some(t) = l.ttft_ms {
            u.ttft_ms.push(t);
        }
        if let Some(r) = l.tokens_per_sec {
            u.tokens_per_sec.push(r);
        }
        if l.cancelled {
            u.cancelled += 1;
            return;
        }
        u.requests += 1;
        u.failed += i64::from(l.failed);
        // 模型名：失败的也算 —— 断在半路之前，上游已经在开头写过它用的模型了
        if let Some(answered) = l.answered
            && !l.sent.is_empty()
        {
            let answered = self.names.id(answered);
            *u.pairs.entry((sent, answered)).or_default() += 1;
        }
        // 输入之比和缓存读只看成功跑完的：取消的、断在半路的，用量可能只是开头那一帧
        // 里的占位（有的中转站在 `message_start` 里报 0、到结尾才报真数）
        if l.failed {
            return;
        }
        let Some(total) = l.reported_input() else {
            return;
        };
        if let Some(estimate) = l.estimate.filter(|&e| e >= MIN_ESTIMATE) {
            // 千分数，四舍五入
            u.input
                .entry(sent)
                .or_default()
                .push((total * 1000 + estimate / 2) / estimate);
        }
        if let Some(p) = previous
            && p.upstream == upstream
            && p.sent == sent
            && l.at_ms - p.at_ms <= CACHE_WARM_MS
            && total >= MIN_CACHEABLE
            && p.total >= MIN_CACHEABLE
        {
            add(
                u.cache.entry(sent).or_default(),
                &tw_api::CacheTally {
                    turns: 1,
                    zero_read_turns: i64::from(l.cache_read == 0),
                    input_tokens: total,
                    cache_read_tokens: l.cache_read,
                },
            );
        }
    }

    /// 攒的东西变成每家一条：归一模型名、取中位数、给每个模型配上别家的参照。
    fn finish(mut self) -> Vec<tw_api::UpstreamCheckup> {
        // 归一之后的模型名 → 上游 → 那一家的样本。参照要跨上游看，所以先摊开
        let mut input: BTreeMap<String, BTreeMap<usize, Vec<i64>>> = BTreeMap::new();
        let mut cache: BTreeMap<String, BTreeMap<usize, tw_api::CacheTally>> = BTreeMap::new();
        for (&upstream, u) in &mut self.upstreams {
            for (sent, xs) in std::mem::take(&mut u.input) {
                input
                    .entry(self.names.normalized(sent))
                    .or_default()
                    .entry(upstream)
                    .or_default()
                    .extend(xs);
            }
            for (sent, t) in std::mem::take(&mut u.cache) {
                add(
                    cache
                        .entry(self.names.normalized(sent))
                        .or_default()
                        .entry(upstream)
                        .or_default(),
                    &t,
                );
            }
        }
        for upstreams in input.values_mut() {
            for xs in upstreams.values_mut() {
                xs.sort_unstable();
            }
        }
        let mut out: Vec<tw_api::UpstreamCheckup> = Vec::new();
        for (&upstream, u) in &mut self.upstreams {
            let mut all: Vec<i64> = Vec::new();
            let mut input_by_model = Vec::new();
            for (model, upstreams) in &input {
                let Some(here) = upstreams.get(&upstream) else {
                    continue;
                };
                all.extend_from_slice(here);
                let mut others: Vec<i64> = Vec::new();
                for (_, xs) in upstreams.iter().filter(|(p, _)| **p != upstream) {
                    others.extend_from_slice(xs);
                }
                others.sort_unstable();
                input_by_model.push(tw_api::InputForModel {
                    model: model.clone(),
                    here: ratio(here),
                    others: (!others.is_empty()).then(|| ratio(&others)),
                    other_upstreams: upstreams.len() as i64 - 1,
                });
            }
            input_by_model.sort_by(|a, b| {
                b.here
                    .samples
                    .cmp(&a.here.samples)
                    .then_with(|| a.model.cmp(&b.model))
            });
            all.sort_unstable();

            let mut cache_all = tw_api::CacheTally::default();
            let mut cache_by_model = Vec::new();
            for (model, upstreams) in &cache {
                let Some(here) = upstreams.get(&upstream) else {
                    continue;
                };
                add(&mut cache_all, here);
                let mut others = tw_api::CacheTally::default();
                for (_, t) in upstreams.iter().filter(|(p, _)| **p != upstream) {
                    add(&mut others, t);
                }
                let other_upstreams = upstreams.len() as i64 - 1;
                cache_by_model.push(tw_api::CacheForModel {
                    model: model.clone(),
                    here: *here,
                    others: (other_upstreams > 0).then_some(others),
                    other_upstreams,
                });
            }
            cache_by_model.sort_by(|a, b| {
                b.here
                    .turns
                    .cmp(&a.here.turns)
                    .then_with(|| a.model.cmp(&b.model))
            });

            out.push(tw_api::UpstreamCheckup {
                upstream: self.names.name(upstream).to_string(),
                requests: u.requests,
                failed: u.failed,
                cancelled: u.cancelled,
                models: models(&mut self.names, &u.pairs),
                input: tw_api::InputVsEstimate {
                    all: (!all.is_empty()).then(|| ratio(&all)),
                    by_model: input_by_model,
                },
                cache: tw_api::CacheReads {
                    all: cache_all,
                    by_model: cache_by_model,
                },
                ttft_ms: median(&mut u.ttft_ms),
                tokens_per_sec: median(&mut u.tokens_per_sec),
            });
        }
        out.sort_by(|a, b| {
            b.requests
                .cmp(&a.requests)
                .then(b.cancelled.cmp(&a.cancelled))
                .then_with(|| a.upstream.cmp(&b.upstream))
        });
        out
    }
}

/// 归一之后是同一对的那些：一共几次，各种原样的写法（发出去的、回答里写的）各几次
type Spellings = (i64, BTreeMap<(usize, usize), i64>);

/// 回答里写的模型名和发出去的对不对得上：数几次写了、几次对不上，举最常见的几对。
///
/// 归一之后是同一对的合在一起算（`claude-opus-4-1` 和 `claude-opus-4-1-20250805` 换成
/// 同一个模型，是同一件事），名字取其中最常见的那种写法
fn models(names: &mut Names, pairs: &HashMap<(usize, usize), i64>) -> tw_api::ModelConsistency {
    let mut named = 0;
    let mut differed = 0;
    let mut grouped: HashMap<(String, String), Spellings> = HashMap::new();
    for (&(sent, answered), &n) in pairs {
        named += n;
        let (s, a) = (names.normalized(sent), names.normalized(answered));
        // 归一之后有一边是空的：说不上对不对得上，不算对不上（见 `model_name::same`）
        if s.is_empty() || a.is_empty() || s == a {
            continue;
        }
        differed += n;
        let (total, spellings) = grouped.entry((s, a)).or_default();
        *total += n;
        *spellings.entry((sent, answered)).or_default() += n;
    }
    let mut examples: Vec<tw_api::ModelPair> = grouped
        .into_values()
        .map(|(count, spellings)| {
            // 最常见的那种写法；一样多时按名字取，结果是确定的
            let (sent, answered) = spellings
                .iter()
                .map(|(&(s, a), &n)| (n, names.name(s), names.name(a)))
                .max_by(|x, y| x.0.cmp(&y.0).then_with(|| (y.1, y.2).cmp(&(x.1, x.2))))
                .map(|(_, s, a)| (s.to_string(), a.to_string()))
                .unwrap_or_default();
            tw_api::ModelPair {
                sent,
                answered,
                count,
            }
        })
        .collect();
    examples.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.sent.cmp(&b.sent))
            .then_with(|| a.answered.cmp(&b.answered))
    });
    examples.truncate(EXAMPLES);
    tw_api::ModelConsistency {
        named,
        differed,
        examples,
    }
}

/// 排好序的千分数的中位数和样本数
fn ratio(sorted: &[i64]) -> tw_api::RatioView {
    tw_api::RatioView {
        median: percentile(sorted, 50) as f64 / 1000.0,
        samples: sorted.len() as i64,
    }
}

/// 中位数和样本数。一个样本都没有的是 None，不是 0
fn median(xs: &mut [i64]) -> Option<tw_api::MedianView> {
    if xs.is_empty() {
        return None;
    }
    xs.sort_unstable();
    Some(tw_api::MedianView {
        p50: percentile(xs, 50),
        samples: xs.len() as i64,
    })
}

fn add(to: &mut tw_api::CacheTally, t: &tw_api::CacheTally) {
    to.turns += t.turns;
    to.zero_read_turns += t.zero_read_turns;
    to.input_tokens += t.input_tokens;
    to.cache_read_tokens += t.cache_read_tokens;
}

/// 上游名和模型名 ↔ 编号，连同归一之后的样子（要用时才归一，每种写法一次）
#[derive(Default)]
struct Names {
    ids: HashMap<String, usize>,
    names: Vec<String>,
    normalized: HashMap<usize, String>,
}

impl Names {
    /// 已经见过的只查一次表，不分配
    fn id(&mut self, name: &str) -> usize {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = self.names.len();
        self.names.push(name.to_string());
        self.ids.insert(name.to_string(), id);
        id
    }

    fn name(&self, id: usize) -> &str {
        &self.names[id]
    }

    fn normalized(&mut self, id: usize) -> String {
        let name = &self.names[id];
        self.normalized
            .entry(id)
            .or_insert_with(|| model_name::normalize(name))
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::RequestRow;
    use crate::db::tests::{row, upstream_failed};

    const T0: i64 = 1_000_000_000;
    const MIN: i64 = 60_000;

    fn on(id: i64, at_ms: i64, provider: &str, sent: &str) -> RequestRow {
        let mut r = row(id, at_ms);
        r.provider = provider.into();
        r.model = sent.into();
        r.sent_model = sent.into();
        r
    }

    fn checkup<'a>(h: &'a tw_api::UpstreamHealth, name: &str) -> &'a tw_api::UpstreamCheckup {
        h.upstreams
            .iter()
            .find(|u| u.upstream == name)
            .unwrap_or_else(|| panic!("体检里没有 `{name}`：{h:?}"))
    }

    fn health(rows: &[RequestRow]) -> tw_api::UpstreamHealth {
        let db = Db::in_memory().unwrap();
        for r in rows {
            db.insert(r).unwrap();
        }
        db.upstream_health(T0, T0 + 24 * 60 * MIN).unwrap()
    }

    /// 一条请求都没有：没有上游，不是一组零；记录从哪一刻起是全的也说不上
    #[test]
    fn an_empty_window_has_no_upstreams() {
        let h = health(&[]);
        assert!(h.upstreams.is_empty(), "{h:?}");
        assert_eq!(h.covered_since_ms, None);
        assert_eq!((h.from_ms, h.to_ms), (T0, T0 + 24 * 60 * MIN));
        // 窗口外的请求不算
        let db = Db::in_memory().unwrap();
        db.insert(&on(1, T0 - 1, "中转", "claude-sonnet-4-5"))
            .unwrap();
        let h = db.upstream_health(T0, T0 + MIN).unwrap();
        assert!(h.upstreams.is_empty(), "{h:?}");
        assert_eq!(
            h.covered_since_ms,
            Some(T0),
            "库里更早就有记录，整段都是全的"
        );
    }

    /// **本地应答、规则拒绝了的不归哪一家**；取消的不算失败，也不在请求数里
    #[test]
    fn requests_failures_and_cancellations_are_counted_apart() {
        let mut failed = on(2, T0 + 2, "中转", "claude-sonnet-4-5");
        failed.error = Some(upstream_failed("502"));
        let mut cancelled = on(3, T0 + 3, "中转", "claude-sonnet-4-5");
        cancelled.cancelled = true;
        let mut local = on(4, T0 + 4, "中转", "claude-sonnet-4-5");
        local.local = true;
        let mut refused = on(5, T0 + 5, "", "claude-sonnet-4-5");
        refused.error = Some(upstream_failed("denied"));
        let h = health(&[
            on(1, T0 + 1, "中转", "claude-sonnet-4-5"),
            failed,
            cancelled,
            local,
            refused,
            on(6, T0 + 6, "官方", "claude-sonnet-4-5"),
            on(7, T0 + 7, "官方", "claude-sonnet-4-5"),
            on(8, T0 + 8, "官方", "claude-sonnet-4-5"),
        ]);
        assert_eq!(
            h.upstreams
                .iter()
                .map(|u| u.upstream.as_str())
                .collect::<Vec<_>>(),
            ["官方", "中转"],
            "请求多的在前；空的上游名不是一家"
        );
        let relay = checkup(&h, "中转");
        assert_eq!(
            (relay.requests, relay.failed, relay.cancelled),
            (2, 1, 1),
            "{relay:?}"
        );
        let official = checkup(&h, "官方");
        assert_eq!((official.requests, official.failed), (3, 0));
    }

    /// 回答里写的模型名：**归一之后一样的不算对不上**，对不上的举出最常见的几对。
    /// 比的是发出去的那个（规则改写之后的），不是客户端要的
    #[test]
    fn answers_are_compared_with_the_model_that_was_sent() {
        let mut rows = Vec::new();
        let mut id = 0;
        let mut add =
            |sent: &str, answered: Option<&str>, n: usize, f: &dyn Fn(&mut RequestRow)| {
                for _ in 0..n {
                    id += 1;
                    let mut r = on(id, T0 + id, "中转", sent);
                    r.answered_model = answered.map(str::to_string);
                    f(&mut r);
                    rows.push(r);
                }
            };
        let plain = |_: &mut RequestRow| {};
        // 别名 → 快照、Bedrock 的写法：同一个模型
        add(
            "claude-sonnet-4-5",
            Some("claude-sonnet-4-5-20250929"),
            4,
            &plain,
        );
        add(
            "claude-sonnet-4-5",
            Some("us.anthropic.claude-sonnet-4-5-20250929-v1:0"),
            1,
            &plain,
        );
        // 客户端要 opus、规则改写成 sonnet 发出去、回答写 sonnet：对得上
        add(
            "claude-sonnet-4-5",
            Some("claude-sonnet-4-5"),
            2,
            &|r: &mut RequestRow| r.model = "claude-opus-4-1".into(),
        );
        // 真对不上的：opus → sonnet 三次（两种写法合成一对），haiku → 3-5-haiku 一次，
        // gpt-5 → gpt-5-mini 一次，gpt-4o → gpt-4o-mini 一次
        add(
            "claude-opus-4-1",
            Some("claude-sonnet-4-5-20250929"),
            2,
            &plain,
        );
        add(
            "claude-opus-4-1-20250805",
            Some("claude-sonnet-4-5"),
            1,
            &plain,
        );
        add(
            "claude-haiku-4-5",
            Some("claude-3-5-haiku-20241022"),
            1,
            &plain,
        );
        add("gpt-5", Some("gpt-5-mini-2025-08-07"), 1, &plain);
        add("gpt-4o", Some("gpt-4o-mini"), 1, &plain);
        // 没写模型名的、取消的、本地应答的都不进样本
        add("claude-sonnet-4-5", None, 3, &plain);
        add(
            "claude-opus-4-1",
            Some("claude-haiku-4-5"),
            2,
            &|r: &mut RequestRow| r.cancelled = true,
        );
        add(
            "claude-opus-4-1",
            Some("claude-haiku-4-5"),
            2,
            &|r: &mut RequestRow| r.local = true,
        );
        // 失败的算：断在半路之前上游已经写过模型名了
        add(
            "claude-opus-4-1",
            Some("claude-sonnet-4-5"),
            1,
            &|r: &mut RequestRow| r.error = Some(upstream_failed("流断了")),
        );
        let h = health(&rows);
        let m = &checkup(&h, "中转").models;
        assert_eq!(m.named, 4 + 1 + 2 + 2 + 1 + 1 + 1 + 1 + 1, "{m:?}");
        assert_eq!(m.differed, 2 + 1 + 1 + 1 + 1 + 1, "{m:?}");
        assert_eq!(
            m.examples,
            vec![
                tw_api::ModelPair {
                    sent: "claude-opus-4-1".into(),
                    answered: "claude-sonnet-4-5-20250929".into(),
                    count: 4,
                },
                tw_api::ModelPair {
                    sent: "claude-haiku-4-5".into(),
                    answered: "claude-3-5-haiku-20241022".into(),
                    count: 1,
                },
                tw_api::ModelPair {
                    sent: "gpt-4o".into(),
                    answered: "gpt-4o-mini".into(),
                    count: 1,
                },
            ],
            "最多三对，多的在前，一样多按名字；归一之后是同一对的合在一起"
        );
    }

    /// 输入之比：**中位数，带着别家服务同一个模型时的参照**；输入按三项加起来算
    #[test]
    fn input_is_compared_with_the_estimate_and_with_other_upstreams() {
        let mut rows = Vec::new();
        let mut id = 0;
        let mut add = |provider: &str,
                       sent: &str,
                       (input, read, write): (i64, i64, i64),
                       estimate: Option<i64>,
                       f: &dyn Fn(&mut RequestRow)| {
            id += 1;
            let mut r = on(id, T0 + id, provider, sent);
            r.input_tokens = Some(input);
            r.cache_read_tokens = Some(read);
            r.cache_write_tokens = Some(write);
            r.input_estimate = estimate;
            f(&mut r);
            rows.push(r);
        };
        let plain = |_: &mut RequestRow| {};
        // 中转：sonnet 报的是估算的 1.38 倍（缓存读写拆开报，加起来才是全部输入）
        add(
            "中转",
            "claude-sonnet-4-5",
            (2_000, 8_000, 3_800),
            Some(10_000),
            &plain,
        );
        add(
            "中转",
            "claude-sonnet-4-5",
            (13_800, 0, 0),
            Some(10_000),
            &plain,
        );
        add(
            "中转",
            "claude-sonnet-4-5-20250929",
            (27_600, 0, 0),
            Some(20_000),
            &plain,
        );
        add(
            "中转",
            "claude-sonnet-4-5",
            (12_000, 0, 0),
            Some(10_000),
            &plain,
        );
        // 不进样本的：估算太短、没估算、失败、取消、没报用量
        add(
            "中转",
            "claude-sonnet-4-5",
            (5_000, 0, 0),
            Some(999),
            &plain,
        );
        add("中转", "claude-sonnet-4-5", (5_000, 0, 0), None, &plain);
        add(
            "中转",
            "claude-sonnet-4-5",
            (90_000, 0, 0),
            Some(10_000),
            &|r: &mut RequestRow| r.error = Some(upstream_failed("流断了")),
        );
        add(
            "中转",
            "claude-sonnet-4-5",
            (0, 0, 0),
            Some(10_000),
            &|r: &mut RequestRow| r.cancelled = true,
        );
        add(
            "中转",
            "claude-sonnet-4-5",
            (0, 0, 0),
            Some(10_000),
            &|r: &mut RequestRow| r.input_tokens = None,
        );
        // 官方：同一个模型报的和估算差不多
        add(
            "官方",
            "claude-sonnet-4-5",
            (10_000, 0, 0),
            Some(10_000),
            &plain,
        );
        add(
            "官方",
            "claude-sonnet-4-5",
            (500, 9_000, 600),
            Some(10_000),
            &plain,
        );
        add(
            "官方",
            "claude-sonnet-4-5",
            (10_500, 0, 0),
            Some(10_000),
            &plain,
        );
        // 另一家：只有它服务的模型，没有参照
        add("另一家", "gpt-5", (11_000, 0, 0), Some(10_000), &plain);
        let h = health(&rows);

        let relay = &checkup(&h, "中转").input;
        assert_eq!(
            relay.all,
            Some(tw_api::RatioView {
                median: 1.38,
                samples: 4
            }),
            "四个样本 1.2、1.38、1.38、1.38，最近秩法的中位数是第二个"
        );
        assert_eq!(relay.by_model.len(), 1, "两种写法是同一个模型：{relay:?}");
        let sonnet = &relay.by_model[0];
        assert_eq!(sonnet.model, "claude-sonnet-4-5");
        assert_eq!(sonnet.here, relay.all.unwrap());
        assert_eq!(
            sonnet.others,
            Some(tw_api::RatioView {
                median: 1.01,
                samples: 3
            })
        );
        assert_eq!(sonnet.other_upstreams, 1);

        let official = &checkup(&h, "官方").input;
        let sonnet = &official.by_model[0];
        assert_eq!(
            (sonnet.here.median, sonnet.here.samples),
            (1.01, 3),
            "{official:?}"
        );
        assert_eq!(
            sonnet.others,
            Some(tw_api::RatioView {
                median: 1.38,
                samples: 4
            }),
            "参照是别家的样本，不含自己的"
        );

        let other = &checkup(&h, "另一家").input;
        assert_eq!(other.by_model[0].model, "gpt-5");
        assert_eq!(
            (other.by_model[0].others, other.by_model[0].other_upstreams),
            (None, 0),
            "只有这一家服务的模型没有参照"
        );
    }

    /// 一家上游没有一条进样本的请求：输入之比是空的，不是 0
    #[test]
    fn no_samples_means_no_ratio() {
        let mut r = on(1, T0 + 1, "中转", "claude-sonnet-4-5");
        r.input_estimate = Some(500);
        let h = health(&[r]);
        let input = &checkup(&h, "中转").input;
        assert_eq!(input.all, None);
        assert!(input.by_model.is_empty());
    }

    /// 一次会话里的一轮。`total` 全部算成没命中缓存的输入，`read` 是其中读缓存的
    fn turn(
        id: i64,
        at_ms: i64,
        session: &str,
        provider: &str,
        sent: &str,
        total: i64,
        read: i64,
    ) -> RequestRow {
        let mut r = on(id, at_ms, provider, sent);
        r.session = Some(session.into());
        r.input_tokens = Some(total - read);
        r.cache_read_tokens = Some(read);
        r.cache_write_tokens = Some(0);
        r
    }

    /// **本该读得到缓存的轮次**：同一次会话的后一轮、同一家、同一个模型、离前一轮开始
    /// 不到五分钟、两轮都够长。读到的和读不到的分开数，带着别家的参照
    #[test]
    fn cache_reads_are_counted_over_turns_that_should_hit() {
        let s45 = "claude-sonnet-4-5";
        let mut failed = turn(23, T0 + 12 * MIN, "s2", "中转", s45, 30_000, 0);
        failed.error = Some(upstream_failed("流断了"));
        let mut previous_failed = turn(30, T0 + 20 * MIN, "s3", "中转", s45, 30_000, 0);
        previous_failed.cancelled = true;
        let rows = vec![
            // s1 在中转上：第一轮写缓存，第二轮读到，第三轮隔了七分钟（凉了），第四轮又
            // 紧跟着第三轮、却一个都没读到
            turn(1, T0, "s1", "中转", s45, 20_000, 0),
            turn(2, T0 + MIN, "s1", "中转", s45, 21_000, 18_000),
            turn(3, T0 + 8 * MIN, "s1", "中转", s45, 22_000, 0),
            turn(4, T0 + 9 * MIN, "s1", "中转", s45, 23_000, 0),
            // s2：前一轮在官方，这一轮换到中转 —— 缓存在官方那儿，不算；下一轮失败了，
            // 不算；再下一轮跟着失败的那一轮（它报了用量，缓存写下了），算
            turn(20, T0 + 10 * MIN, "s2", "官方", s45, 30_000, 0),
            turn(21, T0 + 11 * MIN, "s2", "中转", s45, 30_000, 0),
            failed,
            turn(24, T0 + 13 * MIN, "s2", "中转", s45, 30_000, 29_000),
            // s3：前一轮被取消了但报了用量，这一轮算
            previous_failed,
            turn(31, T0 + 21 * MIN, "s3", "中转", s45, 30_000, 0),
            // s4：太短，上游本来就不缓存
            turn(40, T0 + 30 * MIN, "s4", "中转", s45, 3_000, 0),
            turn(41, T0 + 31 * MIN, "s4", "中转", s45, 3_500, 0),
            // s5：换了模型，缓存各是各的
            turn(50, T0 + 40 * MIN, "s5", "中转", s45, 30_000, 0),
            turn(
                51,
                T0 + 41 * MIN,
                "s5",
                "中转",
                "claude-opus-4-1",
                30_000,
                0,
            ),
            // 官方：同一个模型，后一轮读到了大部分
            turn(60, T0 + 50 * MIN, "s6", "官方", s45, 40_000, 0),
            turn(61, T0 + 51 * MIN, "s6", "官方", s45, 41_000, 40_000),
            turn(
                62,
                T0 + 52 * MIN,
                "s6",
                "官方",
                "claude-sonnet-4-5-20250929",
                42_000,
                0,
            ),
        ];
        let h = health(&rows);

        let relay = &checkup(&h, "中转").cache;
        let want_relay = tw_api::CacheTally {
            turns: 4,
            zero_read_turns: 2,
            input_tokens: 21_000 + 23_000 + 30_000 + 30_000,
            cache_read_tokens: 18_000 + 29_000,
        };
        assert_eq!(relay.all, want_relay, "{relay:?}");
        assert_eq!(relay.by_model.len(), 1, "{relay:?}");
        assert_eq!(relay.by_model[0].model, "claude-sonnet-4-5");
        assert_eq!(relay.by_model[0].here, want_relay);
        let want_official = tw_api::CacheTally {
            turns: 1,
            zero_read_turns: 0,
            input_tokens: 41_000,
            cache_read_tokens: 40_000,
        };
        assert_eq!(relay.by_model[0].others, Some(want_official));
        assert_eq!(relay.by_model[0].other_upstreams, 1);

        // 官方那边：发出去的名字换了一种写法的那一轮，前一轮的名字对不上，不算
        let official = &checkup(&h, "官方").cache;
        assert_eq!(official.all, want_official, "{official:?}");
        assert_eq!(official.by_model[0].others, Some(want_relay));
    }

    /// 窗口里的第一轮，前一轮在窗口开始之前不到五分钟：**照样找得到它**
    #[test]
    fn the_first_turn_in_the_window_finds_the_one_just_before_it() {
        let db = Db::in_memory().unwrap();
        db.insert(&turn(1, T0 - 2 * MIN, "s", "中转", "m", 20_000, 0))
            .unwrap();
        db.insert(&turn(2, T0 + MIN, "s", "中转", "m", 21_000, 19_000))
            .unwrap();
        let h = db.upstream_health(T0, T0 + 10 * MIN).unwrap();
        let cache = &checkup(&h, "中转").cache;
        assert_eq!((cache.all.turns, cache.all.cache_read_tokens), (1, 19_000));
        // 窗口外的那一轮自己不算请求
        assert_eq!(checkup(&h, "中转").requests, 1);
    }

    /// 延迟和速度和按上游分的那两个查询（`latency_by_provider`、`token_rate_by_provider`）
    /// **口径一样**：同一批行，同样的中位数和样本数。取消的、失败的照样进样本，和那两页
    /// 一样
    #[test]
    fn latency_and_speed_match_the_per_upstream_queries() {
        let db = Db::in_memory().unwrap();
        for (id, ttft, rate, cancelled) in [
            (1, Some(800), Some(60), false),
            (2, Some(900), Some(70), false),
            (3, Some(3_000), None, true),
            (4, None, None, false),
        ] {
            let mut r = on(id, T0 + id, "中转", "claude-sonnet-4-5");
            r.ttft_ms = ttft;
            r.tokens_per_sec = rate;
            r.cancelled = cancelled;
            db.insert(&r).unwrap();
        }
        let mut quiet = on(9, T0 + 9, "官方", "claude-sonnet-4-5");
        quiet.ttft_ms = None;
        quiet.tokens_per_sec = None;
        db.insert(&quiet).unwrap();
        let mut local = on(10, T0 + 10, "中转", "claude-sonnet-4-5");
        local.local = true;
        local.ttft_ms = Some(1);
        db.insert(&local).unwrap();

        let (from, to) = (T0, T0 + MIN);
        let h = db.upstream_health(from, to).unwrap();
        let relay = checkup(&h, "中转");
        assert_eq!(
            relay.ttft_ms,
            Some(tw_api::MedianView {
                p50: 900,
                samples: 3
            })
        );
        assert_eq!(
            relay.tokens_per_sec,
            Some(tw_api::MedianView {
                p50: 60,
                samples: 2
            })
        );
        let latency = db.latency_by_provider(from, to).unwrap();
        let rate = db.token_rate_by_provider(from, to).unwrap();
        for u in &h.upstreams {
            let l = latency.iter().find(|l| l.model == u.upstream);
            assert_eq!(
                u.ttft_ms.map(|m| (m.p50, m.samples)),
                l.map(|l| (l.p50, l.samples as i64)),
                "{}",
                u.upstream
            );
            let r = rate.iter().find(|r| r.model == u.upstream);
            assert_eq!(
                u.tokens_per_sec.map(|m| (m.p50, m.samples)),
                r.map(|r| (i64::from(r.p50), r.samples as i64)),
                "{}",
                u.upstream
            );
        }
        // 一个样本都没有的是空的，不是 0
        let official = checkup(&h, "官方");
        assert_eq!((official.ttft_ms, official.tokens_per_sec), (None, None));
    }

    /// 线上的样子：每一项都在，没有的是 null，不是省掉
    #[test]
    fn every_field_is_on_the_wire() {
        let h = health(&[on(1, T0 + 1, "中转", "claude-sonnet-4-5")]);
        let v = serde_json::to_value(&h).unwrap();
        let u = &v["upstreams"][0];
        assert!(u["ttft_ms"].is_object(), "{u}");
        assert!(u["input"]["all"].is_null(), "{u}");
        assert!(u.get("input").unwrap().get("all").is_some(), "{u}");
        assert_eq!(u["cache"]["all"]["turns"], 0);
        assert_eq!(u["models"]["examples"], serde_json::json!([]));
    }
}
