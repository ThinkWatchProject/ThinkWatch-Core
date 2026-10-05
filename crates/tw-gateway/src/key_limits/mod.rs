//! 每把网关密钥的用量上限：数、等、拒、结算。
//!
//! 写法在 `tw_config::limits`。这里回答四件事。
//!
//! **数什么。**请求数：一个准入的请求算一个，数 token 的请求、网关自己答的不算。token：
//! 没走缓存的输入 + 写进缓存的 + 输出，那一条开了 `cache_reads` 再加上从缓存读的。费用：
//! 记下的费用（实测的、估算的都算），没有价格的模型、不计费的上游算 0。**都按存储层记下的
//! 那一行算**（[`KeyLimits::settle`]）：重启之后从库里加回来的（[`KeyLimits::rebuild`]）和
//! 此刻内存里的是同一份数。
//!
//! **怎么拒、怎么等。**天、周、月是自然周期：用满了就拒，到下一期之前重试也没用。分钟、
//! 小时是滚动窗口：用满了先看下一个空位多久之后空出来，等得到（在这个请求的等待期限之前，
//! 期限见 [`slot_wait`]）就等，等不到就拒，并说清楚多久之后再来。顺序见 [`crate::server`]
//! 的管线第 3 步：自然周期 → 并发上限 → 滚动窗口。
//!
//! **在跑的怎么算。**准入时按输入估一个数占着（预留：输入 token 的估算，和按头一个候选算
//! 的输入费用），存储层记下那一行时换成实数。几个请求同时进来时，超出上限的最多是在跑的
//! 那些。
//!
//! **什么时候算到哪一期。**天、周、月按请求开始的时刻（那一行的 `at_ms`）归期，和从库里
//! 加回来时同一个口径。滚动窗口里请求数记在准入的那一刻，token 和费用记在结算的那一刻：
//! 一个跑了三分钟的请求，它的输出要等它跑完才知道，按开始的时刻记的话，「每分钟多少
//! token」永远数不到它。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tw_config::{KeyLimit, LimitMeasure, LimitPer};
use tw_types::{Msg, msg};

mod clock;
#[cfg(test)]
mod tests;

pub use clock::{Clock, SystemClock, TestClock};

/// 自然周期。下标就是 [`Books::periods`] 的下标
const CALENDAR: [LimitPer; 3] = [LimitPer::Day, LimitPer::Week, LimitPer::Month];

/// 请求已经结束、存储层却还没交来它那一行时，它的预留再留多久。
///
/// 存储层落后、丢了事件时那一行永远不会来：不放掉的话，那份预留一直占着这把密钥的
/// 额度。存储层正常时几毫秒就到
const GRACE_MS: i64 = 60_000;

/// 准入之前就被拒的请求那一行的失败码：**路由**拒绝了它（规则拒绝、选中的上游都服务不了），
/// 一跳都没有。这样的请求没有经过准入，不算进请求数（[`Recorded::counts`]）。
///
/// 只看码不够：同样的码在尝试链的某一跳上也会出现（阶段二的规则拒绝），那时请求已经准入
/// 过了 —— 所以还要看有没有一跳。
const NOT_ADMITTED: &[&str] = &[
    "gw.route.denied",
    "gw.route.all_selected_disabled",
    "gw.model.no_upstream_available",
];

/// 上限本身拒绝的请求那一行的失败码前缀。它们也不算进请求数：不然一个被拒的客户端每重试
/// 一次，窗口就往后推一次，永远等不到空位
const REFUSED: &str = "gw.key_limit.";

/// 一个请求最多等多久：`failover.slot_wait_secs`，0 是不等。
///
/// **一个请求合起来算**：等滚动窗口的空位在准入时、发给哪一家之前，等上游空位
/// （`crate::slots`）在之后的那几跳里，两段共用准入时定下的同一个期限（见
/// `server::pipeline::admission`）。各给一份的话，两样都碰上的请求能等两倍那么久
pub fn slot_wait(cfg: &tw_config::Config) -> Duration {
    Duration::from_secs(cfg.failover.slot_wait_secs)
}

/// 这个路径的请求算不算：数 token 的不算（Anthropic 的 `count_tokens`、Gemini 的
/// `:countTokens`、Responses 的 `input_tokens`）—— 它们不跑模型、不收钱。
pub fn uncounted(path: &str) -> bool {
    if crate::client_api::ClientApi::counts_tokens(path) {
        return true;
    }
    let p = path.trim_end_matches('/');
    p.strip_prefix("/v1").unwrap_or(p) == "/responses/input_tokens"
}

/// 一份用量。四样分开记，各条上限各取各的。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Amount {
    requests: i64,
    /// 没走缓存的输入 + 写进缓存的 + 输出
    tokens: i64,
    cache_read: i64,
    /// 微分
    cost: i64,
}

impl Amount {
    fn of(&self, m: LimitMeasure, cache_reads: bool) -> i64 {
        match m {
            LimitMeasure::Requests => self.requests,
            LimitMeasure::Tokens if cache_reads => self.tokens.saturating_add(self.cache_read),
            LimitMeasure::Tokens => self.tokens,
            LimitMeasure::Cost => self.cost,
        }
    }
    fn add(&mut self, o: &Amount) {
        self.requests = self.requests.saturating_add(o.requests);
        self.tokens = self.tokens.saturating_add(o.tokens);
        self.cache_read = self.cache_read.saturating_add(o.cache_read);
        self.cost = self.cost.saturating_add(o.cost);
    }
}

/// 一期（天、周、月）的合计。
#[derive(Debug, Clone, Copy)]
struct Period {
    start: i64,
    end: i64,
    sum: Amount,
}

/// 一把密钥的账。**跨重载存活**，按密钥的名字记 —— 存储层那一行记的也是名字。
#[derive(Debug, Default)]
struct Books {
    /// 天、周、月，和 [`CALENDAR`] 同一个顺序。到了下一期就换一本新的
    periods: [Option<Period>; 3],
    /// 滚动窗口用：最近的每一笔（请求数在准入时，token 和费用在结算时），按记下的先后。
    /// **只有设了分钟、小时上限的密钥才记**，留到最长的那个窗口为止
    recent: VecDeque<(i64, Amount)>,
}

impl Books {
    /// `now` 所在的那一期。到了下一期就从 0 起
    fn period(&mut self, clock: &dyn Clock, per: LimitPer, now: i64) -> &mut Period {
        let (start, end) = clock.period(per, now);
        let slot = &mut self.periods[calendar_index(per)];
        match slot {
            Some(p) if p.start == start => {}
            _ => {
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

fn calendar_index(per: LimitPer) -> usize {
    CALENDAR.iter().position(|p| *p == per).unwrap_or(0)
}

/// 一个在跑的请求占着的那一份。
#[derive(Debug)]
struct Reservation {
    key: String,
    /// 准入的那一刻。算在哪一期、在不在滚动窗口里看它
    at_ms: i64,
    /// 输入 token 的估算
    tokens: i64,
    /// 头一个候选的输入费用估算，微分
    cost: i64,
    /// 它是哪个请求。准入之后、开始事件之前还没有号
    request: Option<u64>,
    /// 发现这个请求已经结束的那一刻（见 [`GRACE_MS`]）
    closed_since: Option<i64>,
}

/// 报过的那一档：哪把密钥、哪一条、哪一期、八成还是到顶。**上限改了就是另一条**，
/// 重新报
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Told {
    key: String,
    per: LimitPer,
    measure: LimitMeasure,
    cache_reads: bool,
    max: i64,
    period_start: i64,
    reached: bool,
}

#[derive(Debug, Default)]
struct Inner {
    books: HashMap<String, Books>,
    /// 在跑的请求的预留，按一个自己的号
    held: HashMap<u64, Reservation>,
    /// 请求号 → 预留的号
    by_request: HashMap<u64, u64>,
    seq: u64,
    /// 此刻配置里每把密钥的上限。结算时用：要不要记滚动窗口、报不报到了八成
    limits: HashMap<String, Vec<KeyLimit>>,
    told: HashSet<Told>,
}

/// 准入时一个请求要占的：输入 token 的估算，和按头一个候选算的输入费用。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Ask {
    pub tokens: u64,
    pub cost_micros: i64,
}

/// 存储层记下的一行（结算），或者库里加起来的一组（重建）：算不算、算多少，看的是这几样。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Recorded {
    /// 密钥的名字
    pub client: String,
    pub path: String,
    /// 网关自己答的
    pub local: bool,
    pub error_code: Option<String>,
    /// 尝试链上有没有至少一跳
    pub attempted: bool,
    /// 几个请求：结算时是 1，重建时是这一组的行数
    pub requests: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// 记下的费用，微分。算不出来的（没有价格、没有用量）是 0
    pub cost_micros: i64,
}

impl Recorded {
    /// 算不算进密钥的用量：**准入过的才算**。
    ///
    /// 网关自己答的、数 token 的不经过准入；上限本身拒绝的、路由就拒绝了的没有准入。
    /// 结算一行和从库里加回来用的是这同一个判断，两边的数才对得上。
    pub fn counts(&self) -> bool {
        if self.local || uncounted(&self.path) {
            return false;
        }
        match self.error_code.as_deref() {
            Some(code) if code.starts_with(REFUSED) => false,
            Some(code) if !self.attempted && NOT_ADMITTED.contains(&code) => false,
            _ => true,
        }
    }

    fn amount(&self) -> Amount {
        let n = |v: u64| v.min(i64::MAX as u64) as i64;
        Amount {
            requests: n(self.requests),
            tokens: n(self
                .input
                .saturating_add(self.cache_write)
                .saturating_add(self.output)),
            cache_read: n(self.cache_read),
            // 退款之类的负数不会有；有也不让它把用量往回拨
            cost: self.cost_micros.max(0),
        }
    }
}

/// 一次拒绝：哪把密钥、哪一条上限、用了多少、什么时候能再来。
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub key: String,
    pub limit: KeyLimit,
    pub used: i64,
    /// 多久之后能再来，毫秒。自然周期是到下一期的时间
    pub retry_after_ms: u64,
    /// 自然周期：这一期结束的时刻，和写给人看的样子
    pub resets: Option<(i64, String)>,
}

impl Refusal {
    /// 客户端收到的那个错误：429，带 `Retry-After`。自然周期用完了说到重置之前别再试
    /// （见 [`crate::error::Retry`]）
    pub fn error(&self) -> crate::GatewayError {
        let retry = crate::error::Retry {
            after_ms: self.retry_after_ms,
            until_reset: self.resets.is_some(),
        };
        crate::GatewayError::rate_limited(self.msg()).with_retry(retry)
    }

    /// 那句话。**一种量、一种周期一句**：界面按码翻，参数里的 `per` 是一个英文词
    /// （`day`），界面按它查自己的词表
    pub fn msg(&self) -> Msg {
        let key = self.key.clone();
        let per = self.limit.per.word();
        let measure = self.limit.measure();
        let (max, used) = match measure {
            LimitMeasure::Cost => (dollars(self.limit.max()), dollars(self.used)),
            _ => (self.limit.max().to_string(), self.used.to_string()),
        };
        match (&self.resets, measure) {
            (Some((at, resets)), LimitMeasure::Requests) => msg!(
                "gw.key_limit.requests_per_period", key = key, max = max, per = per,
                used = used, resets = resets, resets_at_ms = at =>
                "Gateway key `{key}` has reached its limit of {max} requests per {per}: {used} so \
                 far. It resets at {resets}."
            ),
            (Some((at, resets)), LimitMeasure::Tokens) => msg!(
                "gw.key_limit.tokens_per_period", key = key, max = max, per = per,
                used = used, resets = resets, resets_at_ms = at =>
                "Gateway key `{key}` has reached its limit of {max} tokens per {per}: {used} so \
                 far. It resets at {resets}."
            ),
            (Some((at, resets)), LimitMeasure::Cost) => msg!(
                "gw.key_limit.cost_per_period", key = key, max = max, per = per,
                used = used, resets = resets, resets_at_ms = at =>
                "Gateway key `{key}` has reached its limit of {max} per {per}: {used} spent so \
                 far. It resets at {resets}."
            ),
            (None, measure) => {
                let retry = self.retry_after_ms.div_ceil(1000).max(1);
                match measure {
                    LimitMeasure::Requests => msg!(
                        "gw.key_limit.requests_rolling", key = key, max = max, per = per,
                        used = used, retry = retry =>
                        "Gateway key `{key}` has reached its limit of {max} requests per {per}: \
                         {used} in the last {per}. Try again in {retry} s."
                    ),
                    LimitMeasure::Tokens => msg!(
                        "gw.key_limit.tokens_rolling", key = key, max = max, per = per,
                        used = used, retry = retry =>
                        "Gateway key `{key}` has reached its limit of {max} tokens per {per}: \
                         {used} in the last {per}. Try again in {retry} s."
                    ),
                    LimitMeasure::Cost => msg!(
                        "gw.key_limit.cost_rolling", key = key, max = max, per = per,
                        used = used, retry = retry =>
                        "Gateway key `{key}` has reached its limit of {max} per {per}: {used} \
                         spent in the last {per}. Try again in {retry} s."
                    ),
                }
            }
        }
    }
}

/// 微分写成美元：`$5.00`、`$0.0042`。至少两位小数，多的照实写
pub(crate) fn dollars(micros: i64) -> String {
    let sign = if micros < 0 { "-" } else { "" };
    let m = micros.unsigned_abs();
    let mut frac = format!("{:06}", m % 1_000_000);
    while frac.len() > 2 && frac.ends_with('0') {
        frac.pop();
    }
    format!("{sign}${}.{frac}", m / 1_000_000)
}

/// 准入过的请求占着的那一份。**丢掉就放掉**：准入之后、开始事件之前出了岔子的请求，
/// 不会一直占着额度。开始之后由 [`Hold::bind`] 交给请求号，等存储层结算。
#[must_use = "dropping it gives the reservation back"]
pub struct Hold {
    owner: Option<Arc<KeyLimits>>,
    seq: u64,
}

impl Hold {
    /// 没有预留：这把密钥没设上限，或者这个请求不算（数 token 的）
    pub fn none() -> Self {
        Self {
            owner: None,
            seq: 0,
        }
    }

    /// 请求有号了：预留跟着它，等存储层记下那一行时换成实数
    pub fn bind(mut self, request: u64) {
        if let Some(owner) = self.owner.take() {
            owner.bind(self.seq, request);
        }
    }
}

impl std::fmt::Debug for Hold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hold")
            .field("held", &self.owner.is_some())
            .finish()
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.take() {
            owner.release(self.seq);
        }
    }
}

/// 全部密钥的用量账。**跨重载存活**（在 `AppState` 里）：改一条路由规则不该让今天花的
/// 钱归零。
pub struct KeyLimits {
    inner: Mutex<Inner>,
    clock: Arc<dyn Clock>,
    /// 报「到了八成、到了上限」，和认请求有没有结束（见 [`GRACE_MS`]）
    bus: tw_observe::EventBus,
}

impl KeyLimits {
    pub fn new(bus: tw_observe::EventBus) -> Self {
        Self::with_clock(bus, Arc::new(SystemClock))
    }

    pub fn with_clock(bus: tw_observe::EventBus, clock: Arc<dyn Clock>) -> Self {
        Self {
            inner: Mutex::default(),
            clock,
            bus,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 配置换了：记下每把密钥此刻的上限。**账不动**：上限改了，已经用掉的照样算
    pub fn configure(&self, cfg: &tw_config::Config) {
        let limits = cfg
            .clients
            .iter()
            .filter(|c| !c.limits.is_empty())
            .map(|c| (c.name.clone(), c.limits.clone()))
            .collect();
        self.lock().limits = limits;
    }

    /// 密钥改了名（控制面改的）：账跟着走。手改配置文件改的名认不出来 —— 那和删掉一把、
    /// 新建一把看起来一样，新的那把从 0 起
    pub fn rename(&self, from: &str, to: &str) {
        if from == to {
            return;
        }
        let mut g = self.lock();
        if let Some(b) = g.books.remove(from) {
            g.books.insert(to.to_string(), b);
        }
        for r in g.held.values_mut() {
            if r.key == from {
                r.key = to.to_string();
            }
        }
        let told: HashSet<Told> = g
            .told
            .drain()
            .map(|mut t| {
                if t.key == from {
                    t.key = to.to_string();
                }
                t
            })
            .collect();
        g.told = told;
        if let Some(l) = g.limits.remove(from) {
            g.limits.insert(to.to_string(), l);
        }
    }

    /// 准入第一步：天、周、月的上限，**用满了就拒**。在并发上限之前：一个这一期已经
    /// 用满的请求不用先排一轮队
    pub fn calendar(&self, key: &str, limits: &[KeyLimit]) -> Result<(), Box<Refusal>> {
        if !limits.iter().any(|l| l.per.rolling_ms().is_none()) {
            return Ok(());
        }
        let now = self.clock.now_ms();
        let (out, events) = {
            let mut g = self.lock();
            self.sweep(&mut g, now);
            let out = self.calendar_locked(&mut g, key, limits, now);
            let events = match &out {
                Err(r) => self.refused_alert(&mut g, r, now),
                Ok(()) => Vec::new(),
            };
            (out, events)
        };
        self.emit(events);
        out
    }

    /// 准入第三步，从此刻起最多等 `wait`（见 [`Self::admit_by`]）。WebSocket 的连接用它：
    /// 那条路之后不再等上游的空位，这一段就是全部
    pub async fn admit(
        self: &Arc<Self>,
        key: &str,
        limits: &[KeyLimit],
        ask: Ask,
        wait: Duration,
    ) -> Result<Hold, Box<Refusal>> {
        self.admit_by(key, limits, ask, tokio::time::Instant::now() + wait)
            .await
    }

    /// 准入第三步：分钟、小时的上限。用满了先等下一个空位，最多等到 `until`（这个请求的
    /// 等待期限，见 [`slot_wait`]）；等不到就拒，带着多久之后能再来。**过了就记上**：这个
    /// 请求算进滚动窗口，输入的估算占上预留。
    ///
    /// 天、周、月在这里再看一遍：等并发名额、等空位的那一阵，别的请求可能把它用满了。
    pub async fn admit_by(
        self: &Arc<Self>,
        key: &str,
        limits: &[KeyLimit],
        ask: Ask,
        until: tokio::time::Instant,
    ) -> Result<Hold, Box<Refusal>> {
        if limits.is_empty() {
            return Ok(Hold::none());
        }
        loop {
            let now = self.clock.now_ms();
            let step = {
                let mut g = self.lock();
                self.sweep(&mut g, now);
                match self.calendar_locked(&mut g, key, limits, now) {
                    Err(r) => {
                        let events = self.refused_alert(&mut g, &r, now);
                        Err((r, events))
                    }
                    Ok(()) => match self.rolling_wait(&mut g, key, limits, now) {
                        None => Ok(self.reserve(&mut g, key, limits, ask, now)),
                        Some(w) => Err((Box::new(w), Vec::new())),
                    },
                }
            };
            let refusal = match step {
                Ok(seq) => {
                    return Ok(Hold {
                        owner: Some(self.clone()),
                        seq,
                    });
                }
                Err((r, events)) => {
                    self.emit(events);
                    r
                }
            };
            // 滚动窗口：空位在期限之前空出来就等，等不到就拒
            let left = until.saturating_duration_since(tokio::time::Instant::now());
            if refusal.resets.is_some() || u128::from(refusal.retry_after_ms) > left.as_millis() {
                return Err(refusal);
            }
            tokio::time::sleep(Duration::from_millis(refusal.retry_after_ms)).await;
        }
    }

    /// 存储层记下了一行：预留换成实数。`at_ms` 是请求开始的时刻，算在哪一期看它
    pub fn settle(&self, id: u64, at_ms: i64, rec: &Recorded) {
        let now = self.clock.now_ms();
        let events = {
            let mut g = self.lock();
            let key = match g.by_request.remove(&id).and_then(|s| g.held.remove(&s)) {
                Some(r) => r.key,
                None => rec.client.clone(),
            };
            if !rec.counts() {
                return;
            }
            let amount = rec.amount();
            let rolling = g
                .limits
                .get(&key)
                .and_then(|ls| longest_window(ls))
                .is_some();
            let clock = self.clock.as_ref();
            let books = g.books.entry(key.clone()).or_default();
            for per in CALENDAR {
                let p = books.period(clock, per, now);
                if at_ms >= p.start && at_ms < p.end {
                    p.sum.add(&amount);
                }
            }
            // 请求数在准入时已经进了滚动窗口
            if rolling {
                books.recent.push_back((
                    now,
                    Amount {
                        requests: 0,
                        ..amount
                    },
                ));
            }
            self.alerts(&mut g, &key, now)
        };
        self.emit(events);
    }

    /// 重启之后把天、周、月的数从请求记录里加回来。`since(t)` 给出从 `t` 起每把密钥
    /// 记下的那些（存储层的 `key_usage_since`）。分钟、小时的窗口从空的开始。
    ///
    /// **已经过了的档不再报**：重启之前多半报过了，再报一遍就是一条重复的通知。
    pub fn rebuild(&self, mut since: impl FnMut(i64) -> Vec<Recorded>) {
        let now = self.clock.now_ms();
        let mut sums: Vec<(LimitPer, i64, i64, HashMap<String, Amount>)> = Vec::new();
        for per in CALENDAR {
            let (start, end) = self.clock.period(per, now);
            let mut by_key: HashMap<String, Amount> = HashMap::new();
            for r in since(start).iter().filter(|r| r.counts()) {
                by_key.entry(r.client.clone()).or_default().add(&r.amount());
            }
            sums.push((per, start, end, by_key));
        }
        let mut g = self.lock();
        for (per, start, end, by_key) in sums {
            for b in g.books.values_mut() {
                b.periods[calendar_index(per)] = Some(Period {
                    start,
                    end,
                    sum: Amount::default(),
                });
            }
            for (key, sum) in by_key {
                g.books.entry(key).or_default().periods[calendar_index(per)] =
                    Some(Period { start, end, sum });
            }
        }
        let keys: Vec<String> = g.limits.keys().cloned().collect();
        for key in keys {
            // 只记下，不报
            let _ = self.alerts(&mut g, &key, now);
        }
    }

    /// 一把密钥的每条上限此刻用了多少（`GET /keys`）。在跑的请求按预留算，和准入看的
    /// 是同一个数
    pub fn view(&self, key: &str, limits: &[KeyLimit]) -> Vec<tw_api::KeyLimitView> {
        if limits.is_empty() {
            return Vec::new();
        }
        let now = self.clock.now_ms();
        let mut g = self.lock();
        self.sweep(&mut g, now);
        limits
            .iter()
            .map(|l| {
                let used = self.used(&mut g, key, l, now);
                let max = l.max();
                tw_api::KeyLimitView {
                    per: l.per.into(),
                    measure: l.measure().into(),
                    max: max.max(0) as u64,
                    cache_reads: l.cache_reads,
                    used: used.max(0) as u64,
                    resets_at_ms: l
                        .per
                        .rolling_ms()
                        .is_none()
                        .then(|| self.clock.period(l.per, now).1.max(0) as u64),
                    reached: used >= max,
                }
            })
            .collect()
    }

    // ------------------------------------------------------------ 锁里面的

    /// 一条上限此刻用了多少：记下的，加上在跑的请求的预留
    fn used(&self, g: &mut Inner, key: &str, l: &KeyLimit, now: i64) -> i64 {
        let (m, cr) = (l.measure(), l.cache_reads);
        let reserved = |r: &Reservation| match m {
            LimitMeasure::Requests => 1,
            LimitMeasure::Tokens => r.tokens,
            LimitMeasure::Cost => r.cost,
        };
        match l.per.rolling_ms() {
            None => {
                let p = *g.books.entry(key.to_string()).or_default().period(
                    self.clock.as_ref(),
                    l.per,
                    now,
                );
                let held: i64 = g
                    .held
                    .values()
                    .filter(|r| r.key == key && r.at_ms >= p.start && r.at_ms < p.end)
                    .map(reserved)
                    .sum();
                p.sum.of(m, cr).saturating_add(held)
            }
            Some(w) => {
                let cutoff = now - w;
                let books = g.books.get(key);
                let done: i64 = books
                    .map(|b| {
                        b.recent
                            .iter()
                            .filter(|(t, _)| *t > cutoff)
                            .map(|(_, a)| a.of(m, cr))
                            .sum()
                    })
                    .unwrap_or(0);
                // 请求数在准入时已经进了窗口，预留里只算 token 和费用
                let held: i64 = g
                    .held
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

    fn calendar_locked(
        &self,
        g: &mut Inner,
        key: &str,
        limits: &[KeyLimit],
        now: i64,
    ) -> Result<(), Box<Refusal>> {
        for l in limits.iter().filter(|l| l.per.rolling_ms().is_none()) {
            let used = self.used(g, key, l, now);
            if used >= l.max() {
                let end = self.clock.period(l.per, now).1;
                return Err(Box::new(Refusal {
                    key: key.to_string(),
                    limit: l.clone(),
                    used,
                    retry_after_ms: (end - now).max(1) as u64,
                    resets: Some((end, self.clock.show(end))),
                }));
            }
        }
        Ok(())
    }

    /// 滚动窗口要等多久才有空位。不用等是 None；要等的那条里等得最久的那一条的拒绝
    fn rolling_wait(
        &self,
        g: &mut Inner,
        key: &str,
        limits: &[KeyLimit],
        now: i64,
    ) -> Option<Refusal> {
        let mut worst: Option<Refusal> = None;
        for l in limits {
            let Some(w) = l.per.rolling_ms() else {
                continue;
            };
            let used = self.used(g, key, l, now);
            let max = l.max();
            if used < max {
                continue;
            }
            // 窗口里的每一笔按时间先后滑出去，滑到用量低于上限的那一刻就是空位
            let (m, cr) = (l.measure(), l.cache_reads);
            let cutoff = now - w;
            let mut items: Vec<(i64, i64)> = g
                .books
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
                    g.held
                        .values()
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
            let wait = (free_at - now).max(1) as u64;
            if worst.as_ref().is_none_or(|r| wait > r.retry_after_ms) {
                worst = Some(Refusal {
                    key: key.to_string(),
                    limit: l.clone(),
                    used,
                    retry_after_ms: wait,
                    resets: None,
                });
            }
        }
        worst
    }

    /// 记上这个请求：滚动窗口里的一个请求，和它的预留。交回预留的号
    fn reserve(&self, g: &mut Inner, key: &str, limits: &[KeyLimit], ask: Ask, now: i64) -> u64 {
        if let Some(w) = longest_window(limits) {
            let books = g.books.entry(key.to_string()).or_default();
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
        g.seq += 1;
        let seq = g.seq;
        g.held.insert(
            seq,
            Reservation {
                key: key.to_string(),
                at_ms: now,
                tokens: ask.tokens.min(i64::MAX as u64) as i64,
                cost: ask.cost_micros.max(0),
                request: None,
                closed_since: None,
            },
        );
        seq
    }

    fn bind(&self, seq: u64, request: u64) {
        let mut g = self.lock();
        if let Some(r) = g.held.get_mut(&seq) {
            r.request = Some(request);
            g.by_request.insert(request, seq);
        }
    }

    fn release(&self, seq: u64) {
        let mut g = self.lock();
        if let Some(r) = g.held.remove(&seq)
            && let Some(id) = r.request
        {
            g.by_request.remove(&id);
        }
    }

    /// 放掉结束了太久、存储层却一直没交来那一行的预留（见 [`GRACE_MS`]）。顺手把滚动
    /// 窗口里滑出去的扔掉
    fn sweep(&self, g: &mut Inner, now: i64) {
        let mut gone = Vec::new();
        for (seq, r) in g.held.iter_mut() {
            let Some(id) = r.request else { continue };
            if self.bus.is_open(id) {
                r.closed_since = None;
                continue;
            }
            let since = *r.closed_since.get_or_insert(now);
            if now - since >= GRACE_MS {
                gone.push((*seq, id));
            }
        }
        for (seq, id) in gone {
            g.held.remove(&seq);
            g.by_request.remove(&id);
        }
        let windows: HashMap<String, i64> = g
            .limits
            .iter()
            .filter_map(|(k, ls)| longest_window(ls).map(|w| (k.clone(), w)))
            .collect();
        for (key, b) in g.books.iter_mut() {
            match windows.get(key) {
                Some(w) => {
                    while b.recent.front().is_some_and(|(t, _)| *t <= now - w) {
                        b.recent.pop_front();
                    }
                }
                None => b.recent.clear(),
            }
        }
    }

    /// 天、周、月的上限到了八成、到了顶：**每一期、每一档报一次**。只看记下的数 ——
    /// 预留是估的，按它报的话结算之后可能又退回去，那是一条假警报
    fn alerts(&self, g: &mut Inner, key: &str, now: i64) -> Vec<tw_api::Event> {
        let Some(limits) = g.limits.get(key).cloned() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for l in limits.iter().filter(|l| l.per.rolling_ms().is_none()) {
            let p = *g.books.entry(key.to_string()).or_default().period(
                self.clock.as_ref(),
                l.per,
                now,
            );
            let used = p.sum.of(l.measure(), l.cache_reads);
            let max = l.max();
            let reached = used >= max;
            let near = used.saturating_mul(5) >= max.saturating_mul(4);
            if !near {
                continue;
            }
            let told = |reached: bool| Told {
                key: key.to_string(),
                per: l.per,
                measure: l.measure(),
                cache_reads: l.cache_reads,
                max,
                period_start: p.start,
                reached,
            };
            // 一下子从八成以下跳过了顶：只报到顶，八成那一档一并算报过
            let new_near = g.told.insert(told(false));
            let new_reached = reached && g.told.insert(told(true));
            if new_reached || (new_near && !reached) {
                out.push(self.alert(key, l, used, reached, p.end, now));
            }
        }
        out
    }

    /// 被拒了：用满了的那一条一定到顶了，没报过就报
    fn refused_alert(&self, g: &mut Inner, r: &Refusal, now: i64) -> Vec<tw_api::Event> {
        let Some((end, _)) = r.resets else {
            return Vec::new();
        };
        let start = self.clock.period(r.limit.per, now).0;
        let told = |reached: bool| Told {
            key: r.key.clone(),
            per: r.limit.per,
            measure: r.limit.measure(),
            cache_reads: r.limit.cache_reads,
            max: r.limit.max(),
            period_start: start,
            reached,
        };
        g.told.insert(told(false));
        if g.told.insert(told(true)) {
            vec![self.alert(&r.key, &r.limit, r.used, true, end, now)]
        } else {
            Vec::new()
        }
    }

    fn alert(
        &self,
        key: &str,
        l: &KeyLimit,
        used: i64,
        reached: bool,
        resets: i64,
        now: i64,
    ) -> tw_api::Event {
        tw_api::Event::KeyLimitAlert {
            id: self.bus.next_id(),
            key: key.to_string(),
            per: l.per.into(),
            measure: l.measure().into(),
            max: l.max().max(0) as u64,
            used: used.max(0) as u64,
            cache_reads: l.cache_reads,
            reached,
            resets_at_ms: resets.max(0) as u64,
            at_ms: now.max(0) as u64,
        }
    }

    /// 在锁外面报
    fn emit(&self, events: Vec<tw_api::Event>) {
        for e in events {
            self.bus.emit(e);
        }
    }
}

/// 一把密钥的分钟、小时上限里最长的那个窗口。没有就是 None
fn longest_window(limits: &[KeyLimit]) -> Option<i64> {
    limits.iter().filter_map(|l| l.per.rolling_ms()).max()
}

/// 这把密钥用得到、却没有价格的模型（`ClientView.unpriced_models`）。
///
/// **和 `GET /v1/models` 同一份清单**：目录里这把密钥的 `allow` 放行的名称（不挑格式 ——
/// 一把密钥给哪种客户端用都行），每一个看提供它的每一家：按量计费、价目表里查不到发给
/// 那一家的名字的，就是它。别名按对到那一家的名称查。只是查表，不发请求。
pub fn unpriced_models(state: &crate::AppState, cfg: &tw_config::Config, key: &str) -> Vec<String> {
    let catalog = state.catalog.load();
    let book = state.pricing.load();
    let allow = crate::models::key_allow(cfg, key);
    catalog
        .resolve_allowed(None, allow)
        .into_iter()
        .filter(|name| {
            catalog.providers_for(name).iter().any(|p| {
                let Some(provider) = cfg.providers.iter().find(|x| &x.name == p) else {
                    return false;
                };
                if provider.billing == tw_config::Billing::Free {
                    return false;
                }
                let sent = catalog
                    .served(name)
                    .iter()
                    .find(|(sp, _)| sp == p)
                    .map_or(name.as_str(), |(_, m)| m.as_str());
                book.resolve_for(p, sent).is_none()
            })
        })
        .collect()
}
