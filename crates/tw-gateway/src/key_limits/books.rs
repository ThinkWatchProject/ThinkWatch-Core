//! 用量的账：密钥的用量上限（[`super::KeyLimits`]）和上游的（[`crate::upstream_limits`]）共用的
//! 那一半 —— 一期一期的合计、滚动窗口里的每一笔、在跑的预留，和从它们算出来的「此刻用了
//! 多少」「什么时候空出来」。
//!
//! **两边数法一样，记在谁的名下不一样**：密钥的记在密钥名下，上游的记在上游名下。什么时候
//! 记上、记多少、用满了怎么办，各自决定。

use std::collections::{HashMap, HashSet, VecDeque};

use tw_config::{KeyLimit, LimitMeasure, LimitPer};

use super::{Clock, Moved};

/// 自然周期。下标就是 [`Books::periods`] 的下标
pub(crate) const CALENDAR: [LimitPer; 3] = [LimitPer::Day, LimitPer::Week, LimitPer::Month];

/// 请求已经结束、存储层却还没交来它那一行时，它的预留再留多久。
///
/// 存储层落后、丢了事件时那一行永远不会来：不放掉的话，那份预留一直占着额度。存储层
/// 正常时几毫秒就到
pub(crate) const GRACE_MS: i64 = 60_000;

pub(crate) fn calendar_index(per: LimitPer) -> usize {
    CALENDAR.iter().position(|p| *p == per).unwrap_or(0)
}

/// 一份用量。四样分开记，各条上限各取各的。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Amount {
    pub requests: i64,
    /// 没走缓存的输入 + 写进缓存的 + 输出
    pub tokens: i64,
    pub cache_read: i64,
    /// 微分
    pub cost: i64,
}

impl Amount {
    pub fn of(&self, m: LimitMeasure, cache_reads: bool) -> i64 {
        match m {
            LimitMeasure::Requests => self.requests,
            LimitMeasure::Tokens if cache_reads => self.tokens.saturating_add(self.cache_read),
            LimitMeasure::Tokens => self.tokens,
            LimitMeasure::Cost => self.cost,
        }
    }
    pub fn add(&mut self, o: &Amount) {
        self.requests = self.requests.saturating_add(o.requests);
        self.tokens = self.tokens.saturating_add(o.tokens);
        self.cache_read = self.cache_read.saturating_add(o.cache_read);
        self.cost = self.cost.saturating_add(o.cost);
    }
}

/// 一期（天、周、月）的合计。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Period {
    pub start: i64,
    pub end: i64,
    pub sum: Amount,
}

/// 一个名字（一把密钥、一家上游）的账。**跨重载存活**，按名字记 —— 存储层那一行记的也是
/// 名字。
#[derive(Debug, Default)]
pub(crate) struct Books {
    /// 天、周、月，和 [`CALENDAR`] 同一个顺序。到了下一期就换一本新的
    pub periods: [Option<Period>; 3],
    /// 滚动窗口用：最近的每一笔（请求数在记上它的那一刻，token 和费用在结算时），按记下的
    /// 先后。**只有设了分钟、小时上限的才记**，留到最长的那个窗口为止
    pub recent: VecDeque<(i64, Amount)>,
    /// 开头变了、换了一本新的那几期（哪一种、新的开头），等着从请求记录里重新加（见
    /// [`super::KeyLimits::reread_with`]）
    pub moved: Vec<(LimitPer, i64)>,
}

impl Books {
    /// `now` 所在的那一期。开头和记着的不一样（到了下一期、换了时区）就换一本新的，记下它
    /// 要从请求记录里重新加；加回来之前先从 0 起。从没记过这一种的，从 0 起就是对的
    /// —— 启动时从库里加回来过（`rebuild`），之后的每一行都结算过
    pub fn period(&mut self, clock: &dyn Clock, per: LimitPer, now: i64) -> &mut Period {
        let (start, end) = clock.period(per, now);
        let slot = &mut self.periods[calendar_index(per)];
        match slot {
            Some(p) if p.start == start => {}
            _ => {
                if slot.is_some() {
                    self.moved.push((per, start));
                }
                *slot = Some(Period {
                    start,
                    end,
                    sum: Amount::default(),
                })
            }
        }
        slot.as_mut().expect("set just above")
    }
}

/// 一个在跑的请求（密钥的），或者一跳发出去的请求（上游的）占着的那一份。
#[derive(Debug)]
pub(crate) struct Reservation {
    /// 记在谁的名下：密钥名，或者上游名
    pub key: String,
    /// 记上它的那一刻。算在哪一期、在不在滚动窗口里看它
    pub at_ms: i64,
    /// 输入 token 的估算
    pub tokens: i64,
    /// 输入费用的估算，微分
    pub cost: i64,
    /// 它是哪个请求。还没交给请求号之前没有
    pub request: Option<u64>,
    /// 发现这个请求已经结束的那一刻（见 [`GRACE_MS`]）
    pub closed_since: Option<i64>,
}

impl Reservation {
    pub fn new(key: &str, at_ms: i64, tokens: u64, cost_micros: i64) -> Self {
        Self {
            key: key.to_string(),
            at_ms,
            tokens: tokens.min(i64::MAX as u64) as i64,
            cost: cost_micros.max(0),
            request: None,
            closed_since: None,
        }
    }
}

/// 一组上限里最长的那个滚动窗口。没有就是 None
pub(crate) fn longest_window(limits: &[KeyLimit]) -> Option<i64> {
    limits.iter().filter_map(KeyLimit::window_ms).max()
}

/// 一条上限此刻用了多少：记下的，加上在跑的请求的预留
pub(crate) fn used(
    books: &mut HashMap<String, Books>,
    held: &HashMap<u64, Reservation>,
    clock: &dyn Clock,
    key: &str,
    l: &KeyLimit,
    now: i64,
) -> i64 {
    let (m, cr) = (l.measure(), l.cache_reads);
    let reserved = |r: &Reservation| match m {
        LimitMeasure::Requests => 1,
        LimitMeasure::Tokens => r.tokens,
        LimitMeasure::Cost => r.cost,
    };
    match l.window_ms() {
        None => {
            let p = *books
                .entry(key.to_string())
                .or_default()
                .period(clock, l.per, now);
            let held: i64 = held
                .values()
                .filter(|r| r.key == key && r.at_ms >= p.start && r.at_ms < p.end)
                .map(reserved)
                .sum();
            p.sum.of(m, cr).saturating_add(held)
        }
        Some(w) => {
            let cutoff = now - w;
            let done: i64 = books
                .get(key)
                .map(|b| {
                    b.recent
                        .iter()
                        .filter(|(t, _)| *t > cutoff)
                        .map(|(_, a)| a.of(m, cr))
                        .sum()
                })
                .unwrap_or(0);
            // 请求数在记上的那一刻已经进了窗口，预留里只算 token 和费用
            let held: i64 = held
                .values()
                .filter(|r| r.key == key && r.at_ms > cutoff)
                .map(|r| match m {
                    LimitMeasure::Requests => 0,
                    _ => reserved(r),
                })
                .sum();
            done.saturating_add(held)
        }
    }
}

/// 一条滚动窗口上限用了 `used`（不小于上限）时，什么时候空出一个位置：窗口里的每一笔按
/// 时间先后滑出去，滑到用量低于上限的那一刻。Unix 毫秒
pub(crate) fn free_at(
    books: &HashMap<String, Books>,
    held: &HashMap<u64, Reservation>,
    key: &str,
    l: &KeyLimit,
    used: i64,
    now: i64,
) -> i64 {
    let Some(w) = l.window_ms() else {
        return now;
    };
    let (m, cr, max) = (l.measure(), l.cache_reads, l.max());
    let cutoff = now - w;
    let mut items: Vec<(i64, i64)> = books
        .get(key)
        .map(|b| {
            b.recent
                .iter()
                .filter(|(t, _)| *t > cutoff)
                .map(|(t, a)| (*t, a.of(m, cr)))
                .collect()
        })
        .unwrap_or_default();
    if m != LimitMeasure::Requests {
        items.extend(
            held.values()
                .filter(|r| r.key == key && r.at_ms > cutoff)
                .map(|r| {
                    let v = if m == LimitMeasure::Tokens {
                        r.tokens
                    } else {
                        r.cost
                    };
                    (r.at_ms, v)
                }),
        );
    }
    items.retain(|(_, v)| *v > 0);
    items.sort_by_key(|(t, _)| *t);
    let mut left = used;
    let mut free_at = now + w;
    for (t, v) in items {
        left -= v;
        if left < max {
            free_at = t + w;
            break;
        }
    }
    free_at
}

/// 在滚动窗口里记上一个请求（这组上限有分钟、小时的才记），顺手把滑出去的扔掉
pub(crate) fn record_request(
    books: &mut HashMap<String, Books>,
    key: &str,
    limits: &[KeyLimit],
    now: i64,
) {
    if let Some(w) = longest_window(limits) {
        let books = books.entry(key.to_string()).or_default();
        books.recent.push_back((
            now,
            Amount {
                requests: 1,
                ..Default::default()
            },
        ));
        while books.recent.front().is_some_and(|(t, _)| *t <= now - w) {
            books.recent.pop_front();
        }
    }
}

/// 撤掉记上这份预留时在滚动窗口里记的那一个请求（[`record_request`] 记在那一刻，和预留的
/// `at_ms` 是同一个数）。请求数一个一个都一样，撤哪一个都行。已经滑出窗口的不用撤
pub(crate) fn unrecord(books: &mut HashMap<String, Books>, r: &Reservation) {
    let Some(b) = books.get_mut(&r.key) else {
        return;
    };
    if let Some(i) = b
        .recent
        .iter()
        .position(|(t, a)| *t == r.at_ms && a.requests > 0)
    {
        b.recent[i].1.requests -= 1;
        if b.recent[i].1 == Amount::default() {
            b.recent.remove(i);
        }
    }
}

/// 放掉结束了太久、存储层却一直没交来那一行的预留（见 [`GRACE_MS`]）。交回放掉的那几份：
/// 预留的号、请求号
pub(crate) fn sweep_held(
    held: &mut HashMap<u64, Reservation>,
    bus: &tw_observe::EventBus,
    now: i64,
) -> Vec<(u64, u64)> {
    let mut gone = Vec::new();
    for (seq, r) in held.iter_mut() {
        let Some(id) = r.request else { continue };
        if bus.is_open(id) {
            r.closed_since = None;
            continue;
        }
        let since = *r.closed_since.get_or_insert(now);
        if now - since >= GRACE_MS {
            gone.push((*seq, id));
        }
    }
    for (seq, _) in &gone {
        held.remove(seq);
    }
    gone
}

/// 把滚动窗口里滑出去的扔掉；此刻没有分钟、小时上限的，整个清空
pub(crate) fn trim_windows(
    books: &mut HashMap<String, Books>,
    limits: &HashMap<String, Vec<KeyLimit>>,
    now: i64,
) {
    let windows: HashMap<&str, i64> = limits
        .iter()
        .filter_map(|(k, ls)| longest_window(ls).map(|w| (k.as_str(), w)))
        .collect();
    for (key, b) in books.iter_mut() {
        match windows.get(key.as_str()) {
            Some(w) => {
                while b.recent.front().is_some_and(|(t, _)| *t <= now - w) {
                    b.recent.pop_front();
                }
            }
            None => b.recent.clear(),
        }
    }
}

/// 这个名字刚换了新的那几期（[`Books::moved`]）里还没要过的：记下要过了，交回去，**在锁
/// 外面**去请求记录里重新加
pub(crate) fn take_moved(
    books: &mut HashMap<String, Books>,
    asked: &mut HashSet<Moved>,
    key: &str,
) -> Vec<Moved> {
    let Some(b) = books.get_mut(key) else {
        return Vec::new();
    };
    std::mem::take(&mut b.moved)
        .into_iter()
        .map(|(per, start)| Moved {
            key: key.to_string(),
            per,
            start,
        })
        .filter(|m| asked.insert(m.clone()))
        .collect()
}

// ------------------------------------------------------------ 交接（见 `crate::handover`）

/// 一份预留写成交接连接上的样子。`seq` 是这里给它的号
pub(crate) fn held_wire(seq: u64, r: &Reservation) -> crate::handover::wire::Held {
    crate::handover::wire::Held {
        seq,
        key: r.key.clone(),
        at_ms: r.at_ms,
        tokens: r.tokens,
        cost: r.cost,
    }
}

/// 旧 core 的一份预留，在这里替它记着。**没有请求号**：它不会被当成「结束了太久」放掉（这里
/// 的事件总线上没有那个请求），只等旧 core 的结算
pub(crate) fn held_reservation(h: &crate::handover::wire::Held) -> Reservation {
    Reservation {
        key: h.key.clone(),
        at_ms: h.at_ms,
        tokens: h.tokens,
        cost: h.cost.max(0),
        request: None,
        closed_since: None,
    }
}

/// 在滚动窗口里记上 `at_ms` 那一刻的一个请求（旧 core 记的那一个，见 [`record_request`]）
pub(crate) fn record_at(books: &mut HashMap<String, Books>, key: &str, at_ms: i64) {
    books.entry(key.to_string()).or_default().recent.push_back((
        at_ms,
        Amount {
            requests: 1,
            ..Default::default()
        },
    ));
}

/// 账和预留写成交接连接上的样子
pub(crate) fn export(
    books: &HashMap<String, Books>,
    held: &HashMap<u64, Reservation>,
) -> crate::handover::wire::LimitBooks {
    let mut held: Vec<_> = held.iter().map(|(s, r)| held_wire(*s, r)).collect();
    held.sort_by_key(|h| h.seq);
    crate::handover::wire::LimitBooks {
        books: books
            .iter()
            .map(|(name, b)| crate::handover::wire::NamedBooks {
                name: name.clone(),
                periods: b.periods,
                recent: b.recent.iter().copied().collect(),
            })
            .collect(),
        held,
    }
}

/// 把旧 core 的账并进来。**并，不是换**：这里已经记着的（多半什么都没有 —— 新 core 接进快照
/// 时还没转发过）照样算。同一期的合计相加，滚动窗口里的每一笔按时刻排进去；开头不一样的
/// 一期（交接正好跨过零点）留这里的
pub(crate) fn import(
    books: &mut HashMap<String, Books>,
    theirs: Vec<crate::handover::wire::NamedBooks>,
) {
    for nb in theirs {
        let b = books.entry(nb.name).or_default();
        for (mine, theirs) in b.periods.iter_mut().zip(nb.periods) {
            let Some(theirs) = theirs else { continue };
            match mine {
                Some(p) if p.start == theirs.start => p.sum.add(&theirs.sum),
                Some(_) => {}
                None => *mine = Some(theirs),
            }
        }
        if !nb.recent.is_empty() {
            let mut all: Vec<(i64, Amount)> = b.recent.drain(..).chain(nb.recent).collect();
            all.sort_by_key(|(t, _)| *t);
            b.recent = all.into();
        }
    }
}
