//! 每把网关密钥的用量上限：数、等、拒、结算。
//!
//! 写法在 `tw_config::limits`。这里回答四件事。
//!
//! **数什么。**请求数：一个发到了上游的请求算一个（[`Recorded::counts`]）—— 数 token 的请求、
//! 网关自己答的不算，没发到上游的也不算：上游都满着回的 429、被规则、内容过滤拒掉的，客户端
//! 照着重试，不该把自己的上限用光。token：
//! 没走缓存的输入 + 写进缓存的 + 输出，那一条开了 `cache_reads` 再加上从缓存读的。费用：
//! 记下的费用（实测的、估算的都算），没有价格的模型、不计费的上游算 0。**都按存储层记下的
//! 那一行算**（[`KeyLimits::settle`]）：重启之后从库里加回来的（[`KeyLimits::rebuild`]）和
//! 此刻内存里的是同一份数。
//!
//! **怎么拒、怎么等。**天、周、月是自然周期：用满了就拒，到下一期之前重试也没用。分钟、
//! 小时是滚动窗口：用满了先看下一个空位多久之后空出来，等得到（在这个请求的等待期限之前，
//! 期限见 [`request_wait`]）就等，等不到就拒，并说清楚多久之后再来。顺序见 [`crate::server`]
//! 的管线第 3 步：自然周期 → 并发上限 → 滚动窗口。
//!
//! **在跑的怎么算。**准入时按输入估一个数占着（预留：输入 token 的估算，和按头一个候选算
//! 的输入费用），存储层记下那一行时换成实数。几个请求同时进来时，超出上限的最多是在跑的
//! 那些。
//!
//! **什么时候算到哪一期。**天、周、月按请求开始的时刻（那一行的 `at_ms`）归期，和从库里
//! 加回来时同一个口径。**一期的开头变了**（到了下一期，或者机器换了时区）就从请求记录里把
//! 新的这一期重新加起来（[`KeyLimits::reread_with`]），不从 0 起 —— 从 0 起的话，换一次时区
//! 就把今天花掉的一笔勾销。滚动窗口里请求数记在准入的那一刻，token 和费用记在结算的那一刻：
//! 一个跑了三分钟的请求，它的输出要等它跑完才知道，按开始的时刻记的话，「每分钟多少
//! token」永远数不到它。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tw_config::{KeyLimit, LimitMeasure, LimitPer};
use tw_types::{Msg, msg};

pub(crate) mod books;
mod clock;
#[cfg(test)]
mod tests;

use books::{Amount, Books, CALENDAR, Period, Reservation, calendar_index, longest_window};
pub use clock::{Clock, SystemClock, TestClock};

/// 一个请求最多等多久：`failover.wait_secs`，0 是不等。
///
/// **一个请求合起来算**：等滚动窗口的空位在准入时、发给哪一家之前，等上游（满着的空位
/// `crate::slots`、限流暂停和上游的用量上限 `crate::throttle`）在之后的那几跳里，几段共用准入时
/// 定下的同一个期限（见 `server::pipeline::admission`）。各给一份的话，几样都碰上的请求能等
/// 好几倍那么久
pub fn request_wait(cfg: &tw_config::Config) -> Duration {
    Duration::from_secs(cfg.failover.wait_secs)
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

/// 开头变了的一期：哪把密钥、哪一种、新的开头。从请求记录里重新加它（[`KeyLimits::reread_with`]）
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Moved {
    pub key: String,
    pub per: LimitPer,
    /// 这一期的开头，Unix 毫秒：从这一刻起记下的都算进来
    pub start: i64,
}

/// 从请求记录里重新加一期的办法（[`KeyLimits::reread_with`]）。拿到要加的那一期，读完了交给
/// [`KeyLimits::reread`]
pub type Reread = Arc<dyn Fn(Moved) + Send + Sync>;

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
    /// 已经要过、还没读回来的几期。**一期只要一次**：读回来之前每个请求都会看到同一个开头，
    /// 不该每个请求去库里查一遍
    asked: HashSet<Moved>,
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
    /// 这个请求可能发到了上游（`tw_api::RoutingView::reached_upstream`）：尝试链上有一跳发
    /// 出去了，或者客户端在路由事件之前就走了
    pub reached: bool,
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
    /// 算不算进密钥的用量：**发到了上游的才算**。
    ///
    /// 网关自己答的、数 token 的不算；一个字节都没发到上游的也不算 —— 上限本身拒绝的、路由
    /// 就拒绝了的、内容过滤拒掉的、上游都满着回了 429 的、每一跳都没发出去的。它们什么都没
    /// 花，客户端照着 `Retry-After` 重试时，不该一次次把自己的上限用掉。结算一行和从库里
    /// 加回来用的是这同一个判断，两边的数才对得上。
    pub fn counts(&self) -> bool {
        self.reached && !self.local && !uncounted(&self.path)
    }

    pub(crate) fn amount(&self) -> Amount {
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
        let every = self.limit.every_or_one();
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
            // 每 5 小时这种：句子里说「每 5 个小时」，参数多一个 every，另起一套码
            (None, measure) if every > 1 => {
                let retry = self.retry_after_ms.div_ceil(1000).max(1);
                match measure {
                    LimitMeasure::Requests => msg!(
                        "gw.key_limit.requests_every", key = key, max = max, every = every,
                        per = per, used = used, retry = retry =>
                        "Gateway key `{key}` has reached its limit of {max} requests every {every} \
                         {per}s: {used} in the last {every} {per}s. Try again in {retry} s."
                    ),
                    LimitMeasure::Tokens => msg!(
                        "gw.key_limit.tokens_every", key = key, max = max, every = every,
                        per = per, used = used, retry = retry =>
                        "Gateway key `{key}` has reached its limit of {max} tokens every {every} \
                         {per}s: {used} in the last {every} {per}s. Try again in {retry} s."
                    ),
                    LimitMeasure::Cost => msg!(
                        "gw.key_limit.cost_every", key = key, max = max, every = every,
                        per = per, used = used, retry = retry =>
                        "Gateway key `{key}` has reached its limit of {max} every {every} {per}s: \
                         {used} spent in the last {every} {per}s. Try again in {retry} s."
                    ),
                }
            }
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
    /// 一期的开头变了时从请求记录里重新加（[`Self::reread_with`]）。没接上（没有存储层）是 None：
    /// 那时从 0 起
    reread: Mutex<Option<Reread>>,
    /// 报「到了八成、到了上限」，和认请求有没有结束（见 [`books::GRACE_MS`]）
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
            reread: Mutex::default(),
            bus,
        }
    }

    /// 接上请求记录：一期的开头变了（到了下一期、机器换了时区）时，用 `f` 把新的这一期
    /// 从记录里重新加起来，加好了交回 [`Self::reread`]。
    ///
    /// **`f` 在锁外面调，不该等**：它去读库，读完了再交回来（`tw_control::key_limits::follow`
    /// 起一个任务做这件事）。读回来之前这一期先从 0 起，同一期只要一次
    pub fn reread_with(&self, f: Reread) {
        *self.reread.lock().unwrap_or_else(PoisonError::into_inner) = Some(f);
    }

    /// 读回来了：`rows` 是从 `m.start` 起记下的那些（存储层的 `key_usage_since`，各把密钥的都
    /// 在里面），这一期的数换成它们加起来的。**这一期的开头又变了的话不换**：那是另一期了，
    /// 它自己会再要一次。
    ///
    /// 调用方要保证读的时候没有结算在进行（存储层在同一把锁里记下一行、结算、读库）：读到的
    /// 就是此刻结算过的全部，换上去一行不多、一行不少。到了八成、到了顶不报，和启动时加回来
    /// 一样 —— 前一期多半报过同一件事
    pub fn reread(&self, m: &Moved, rows: &[Recorded]) {
        let now = self.clock.now_ms();
        let mut sum = Amount::default();
        for r in rows.iter().filter(|r| r.client == m.key && r.counts()) {
            sum.add(&r.amount());
        }
        let mut g = self.lock();
        g.asked.remove(m);
        let Some(p) = g
            .books
            .get_mut(&m.key)
            .and_then(|b| b.periods[calendar_index(m.per)].as_mut())
            .filter(|p| p.start == m.start)
        else {
            return;
        };
        p.sum = sum;
        let _ = self.alerts(&mut g, &m.key, now);
    }

    /// 这把密钥刚换了新的那几期（[`Books::moved`]）里还没要过的：记下要过了，交回去，**在锁外面**
    /// 交给 [`Self::ask_reread`]
    fn moved(&self, g: &mut Inner, key: &str) -> Vec<Moved> {
        books::take_moved(&mut g.books, &mut g.asked, key)
    }

    /// 去请求记录里重新加这几期。没接上记录的，从 0 起
    fn ask_reread(&self, moved: Vec<Moved>) {
        if moved.is_empty() {
            return;
        }
        let f = self
            .reread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        match f {
            Some(f) => moved.into_iter().for_each(|m| f(m)),
            None => self.lock().asked.clear(),
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
        if !limits.iter().any(|l| l.window_ms().is_none()) {
            return Ok(());
        }
        let now = self.clock.now_ms();
        let (out, events, moved) = {
            let mut g = self.lock();
            self.sweep(&mut g, now);
            let out = self.calendar_locked(&mut g, key, limits, now);
            let events = match &out {
                Err(r) => self.refused_alert(&mut g, r, now),
                Ok(()) => Vec::new(),
            };
            (out, events, self.moved(&mut g, key))
        };
        self.emit(events);
        self.ask_reread(moved);
        out
    }

    /// 准入第三步，从此刻起最多等 `wait`（见 [`Self::admit_by`]）。整条连接一行的 WebSocket
    /// 连接（Realtime 和别的路径）用它：那条路不等上游的空位，这一段就是全部。Responses 连接
    /// 上的每一轮和 HTTP 的请求一样用 [`Self::admit_by`]
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
    /// 等待期限，见 [`request_wait`]）；等不到就拒，带着多久之后能再来。**过了就记上**：这个
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
            let (step, moved) = {
                let mut g = self.lock();
                self.sweep(&mut g, now);
                let step = match self.calendar_locked(&mut g, key, limits, now) {
                    Err(r) => {
                        let events = self.refused_alert(&mut g, &r, now);
                        Err((r, events))
                    }
                    Ok(()) => match self.rolling_wait(&mut g, key, limits, now) {
                        None => Ok(self.reserve(&mut g, key, limits, ask, now)),
                        Some(w) => Err((Box::new(w), Vec::new())),
                    },
                };
                (step, self.moved(&mut g, key))
            };
            self.ask_reread(moved);
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

    /// 存储层记下了一行：预留换成实数。`at_ms` 是请求开始的时刻，算在哪一期看它。
    ///
    /// **不算的那一行把准入时记上的也还回去**（[`Recorded::counts`]）：预留，和滚动窗口里的那
    /// 一个请求 —— 上游都满着回了 429 的请求，客户端过几秒重试，窗口里不该还留着它
    pub fn settle(&self, id: u64, at_ms: i64, rec: &Recorded) {
        let now = self.clock.now_ms();
        let (events, moved) = {
            let mut g = self.lock();
            let held = g.by_request.remove(&id).and_then(|s| g.held.remove(&s));
            if !rec.counts() {
                if let Some(r) = held {
                    books::unrecord(&mut g.books, &r);
                }
                return;
            }
            let key = held.map_or_else(|| rec.client.clone(), |r| r.key);
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
            (self.alerts(&mut g, &key, now), self.moved(&mut g, &key))
        };
        self.emit(events);
        self.ask_reread(moved);
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
        let view = limits
            .iter()
            .map(|l| {
                let used = self.used(&mut g, key, l, now);
                let max = l.max();
                tw_api::KeyLimitView {
                    per: l.per.into(),
                    every: l.every_or_one(),
                    measure: l.measure().into(),
                    max: max.max(0) as u64,
                    cache_reads: l.cache_reads,
                    used: used.max(0) as u64,
                    resets_at_ms: l
                        .window_ms()
                        .is_none()
                        .then(|| self.clock.period(l.per, now).1.max(0) as u64),
                    reached: used >= max,
                }
            })
            .collect();
        let moved = self.moved(&mut g, key);
        drop(g);
        self.ask_reread(moved);
        view
    }

    // ------------------------------------------------------------ 锁里面的

    /// 一条上限此刻用了多少：记下的，加上在跑的请求的预留
    fn used(&self, g: &mut Inner, key: &str, l: &KeyLimit, now: i64) -> i64 {
        books::used(&mut g.books, &g.held, self.clock.as_ref(), key, l, now)
    }

    fn calendar_locked(
        &self,
        g: &mut Inner,
        key: &str,
        limits: &[KeyLimit],
        now: i64,
    ) -> Result<(), Box<Refusal>> {
        for l in limits.iter().filter(|l| l.window_ms().is_none()) {
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
        for l in limits.iter().filter(|l| l.window_ms().is_some()) {
            let used = self.used(g, key, l, now);
            if used < l.max() {
                continue;
            }
            let free_at = books::free_at(&g.books, &g.held, key, l, used, now);
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
        books::record_request(&mut g.books, key, limits, now);
        g.seq += 1;
        let seq = g.seq;
        g.held
            .insert(seq, Reservation::new(key, now, ask.tokens, ask.cost_micros));
        seq
    }

    fn bind(&self, seq: u64, request: u64) {
        let mut g = self.lock();
        if let Some(r) = g.held.get_mut(&seq) {
            r.request = Some(request);
            g.by_request.insert(request, seq);
        }
    }

    /// 准入之后、开始之前就被丢掉的请求（[`Hold`] 的 Drop）：它一个字节都没发出去，预留和
    /// 滚动窗口里的那一个请求都还回去
    fn release(&self, seq: u64) {
        let mut g = self.lock();
        if let Some(r) = g.held.remove(&seq) {
            if let Some(id) = r.request {
                g.by_request.remove(&id);
            }
            books::unrecord(&mut g.books, &r);
        }
    }

    /// 放掉结束了太久、存储层却一直没交来那一行的预留（见 [`books::GRACE_MS`]）。顺手把滚动
    /// 窗口里滑出去的扔掉
    fn sweep(&self, g: &mut Inner, now: i64) {
        for (_, id) in books::sweep_held(&mut g.held, &self.bus, now) {
            g.by_request.remove(&id);
        }
        books::trim_windows(&mut g.books, &g.limits, now);
    }

    /// 天、周、月的上限到了八成、到了顶：**每一期、每一档报一次**。只看记下的数 ——
    /// 预留是估的，按它报的话结算之后可能又退回去，那是一条假警报
    fn alerts(&self, g: &mut Inner, key: &str, now: i64) -> Vec<tw_api::Event> {
        let Some(limits) = g.limits.get(key).cloned() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for l in limits.iter().filter(|l| l.window_ms().is_none()) {
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
