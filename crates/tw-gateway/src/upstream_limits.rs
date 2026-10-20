//! 每家上游的用量上限：数、看、预留、结算。
//!
//! 写法和密钥的一样（`tw_config::limits`，写在 `providers[].limits`），数法也一样（账本和
//! 密钥的共用，见 [`crate::key_limits::books`]）。**不一样的是数哪几跳、到了怎么办。**
//!
//! **数什么。**请求数：发到这家的每一跳算一个，成功失败都算，**这家回了 429 的不算** —— 它
//! 没接，请求换一家或者等它，不该把这家的上限用掉；没发出去的也不算，数 token 的请求不算。
//! token、费用：接下请求的那一跳（尝试链的最后一跳）的用量，记在那一家名下，算法和密钥的
//! 一样（没走缓存的输入 + 写进缓存的 + 输出，开了 `cache_reads` 再加从缓存读的；费用是记下的，
//! 没有价格、不计费的算 0）。**都按存储层记下的那一行算**（[`UpstreamLimits::settle`]，哪几跳
//! 算看 `tw_api::RoutingView::upstream_hops`、用量算谁的看 `served_by`）：重启之后从库里加
//! 回来的（[`UpstreamLimits::rebuild`]）和此刻内存里的是同一份数。
//!
//! **到了怎么办。**这里不拒绝谁，只说这家此刻能不能发（[`UpstreamLimits::state`]）：分钟、
//! 小时的窗口满了，什么时候空出来算得出来，是等得到的；天、周、月用满了，到下一期之前只能
//! 跳过。换一家、排队、等多久由管线决定（`server::pipeline::hop`），它在
//! [`UpstreamLimits::changed`] 上等这家空出来。
//!
//! **在跑的怎么算。**一跳发出去的那一刻按输入估一个数占着（[`UpstreamLimits::reserve`]：一个
//! 请求、输入 token 的估算、按这家算的输入费用）。这一跳回了 429、没发出去，整份还回去
//! （[`UpstreamHold::release`]，丢掉也一样）；别的结局交给请求号（[`UpstreamHold::bind`]），
//! 存储层记下那一行时换成实数。几个请求同时发往一家时，超出上限的最多是在路上的那些。
//!
//! **什么时候算到哪一期。**和密钥的一样：天、周、月按请求开始的时刻归期，一期的开头变了就从
//! 请求记录里把新的这一期重新加起来（[`UpstreamLimits::reread_with`]）；滚动窗口里请求数记在
//! 发出的那一刻，token 和费用记在结算的那一刻。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::Notify;
use tw_config::KeyLimit;

use crate::key_limits::books::{self, Amount, Books, CALENDAR, Period, Reservation};
use crate::key_limits::{Ask, Clock, Moved, Recorded, Reread, SystemClock, uncounted};

/// 一家上游此刻能不能发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitState {
    Free,
    /// 滚动窗口满了：到 `until_ms`（Unix 毫秒）空出位置。`limit` 是满了的那一条在这家
    /// `limits` 里的下标；几条都满了的，是空出来得最晚的那一条
    WindowFull {
        until_ms: u64,
        limit: u32,
    },
    /// 天、周、月用满了：`until_ms` 是下一期开始的时刻。`limit` 同上；几条都满了的，是重新
    /// 开始得最晚的那一条。**它比窗口满了要紧**：两样都有时报它 —— 等不到
    PeriodUsedUp {
        until_ms: u64,
        limit: u32,
    },
}

#[derive(Default)]
struct Inner {
    books: HashMap<String, Books>,
    /// 在路上的那几跳的预留，按一个自己的号
    held: HashMap<u64, Reservation>,
    /// 请求号 → 它发出去的那几跳的预留的号。**一个请求可以有好几跳**，同一家也可以有两跳
    by_request: HashMap<u64, Vec<u64>>,
    seq: u64,
    /// 此刻配置里每家上游的上限。结算时要不要记滚动窗口、[`UpstreamLimits::changed`] 算
    /// 什么时候再看一眼，看它
    limits: HashMap<String, Vec<KeyLimit>>,
    /// 已经要过、还没读回来的几期，一期只要一次（见 `KeyLimits` 的同名字段）
    asked: HashSet<Moved>,
    /// 等这家变化的（[`UpstreamLimits::changed`]）
    wake: HashMap<String, Arc<Notify>>,
}

struct Shared {
    inner: Mutex<Inner>,
    clock: Arc<dyn Clock>,
    /// 一期的开头变了时从请求记录里重新加（[`UpstreamLimits::reread_with`]）。没接上是 None：
    /// 那时从 0 起
    reread: Mutex<Option<Reread>>,
    /// 认请求有没有结束（见 [`books::GRACE_MS`]）
    bus: tw_observe::EventBus,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 放掉结束了太久、存储层一直没交来那一行的预留；顺手把滚动窗口里滑出去的扔掉。交回
    /// 放掉了预留的那几家
    fn sweep(&self, g: &mut Inner, now: i64) -> Vec<String> {
        let mut touched = Vec::new();
        let before: HashMap<u64, String> =
            g.held.iter().map(|(s, r)| (*s, r.key.clone())).collect();
        for (seq, id) in books::sweep_held(&mut g.held, &self.bus, now) {
            if let Some(seqs) = g.by_request.get_mut(&id) {
                seqs.retain(|s| *s != seq);
                if seqs.is_empty() {
                    g.by_request.remove(&id);
                }
            }
            if let Some(k) = before.get(&seq) {
                touched.push(k.clone());
            }
        }
        books::trim_windows(&mut g.books, &g.limits, now);
        touched
    }

    /// 叫醒等这几家的（在锁外面调）
    fn wake<'a>(&self, names: impl IntoIterator<Item = &'a str>) {
        let notifies: Vec<Arc<Notify>> = {
            let g = self.lock();
            names
                .into_iter()
                .filter_map(|n| g.wake.get(n).cloned())
                .collect()
        };
        for n in notifies {
            n.notify_waiters();
        }
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

    fn bind(&self, seq: u64, request: u64) {
        let mut g = self.lock();
        if let Some(r) = g.held.get_mut(&seq) {
            r.request = Some(request);
            g.by_request.entry(request).or_default().push(seq);
        }
    }

    /// 一跳回了 429、没发出去（[`UpstreamHold`] 的 Drop）：预留和滚动窗口里的那一个请求都
    /// 还回去
    fn release(&self, seq: u64) {
        let key = {
            let mut g = self.lock();
            let g = &mut *g;
            let Some(r) = g.held.remove(&seq) else {
                return;
            };
            if let Some(id) = r.request
                && let Some(seqs) = g.by_request.get_mut(&id)
            {
                seqs.retain(|s| *s != seq);
                if seqs.is_empty() {
                    g.by_request.remove(&id);
                }
            }
            books::unrecord(&mut g.books, &r);
            r.key
        };
        self.wake([key.as_str()]);
    }
}

/// 一跳发出去时占着的那一份（[`UpstreamLimits::reserve`]）。**丢掉就还回去**：回了 429、
/// 没发出去的那一跳不会一直占着这家的额度。上游接下了（回的不是 429，或者发出去之后才
/// 失败）就交给请求号（[`Self::bind`]），等存储层结算。
#[must_use = "dropping it gives the reservation back"]
pub struct UpstreamHold {
    owner: Option<Arc<Shared>>,
    seq: u64,
}

impl UpstreamHold {
    /// 没有预留：这家没设上限
    pub fn none() -> Self {
        Self {
            owner: None,
            seq: 0,
        }
    }

    /// 这一跳算数：上游回了 429 以外的答复，或者发出去之后才失败（超时、断了）。预留跟着
    /// 请求号，等存储层记下那一行时换成实数；请求数留在窗口里
    pub fn bind(mut self, request: u64) {
        if let Some(owner) = self.owner.take() {
            owner.bind(self.seq, request);
        }
    }

    /// 这一跳不算：上游回了 429，或者没发出去。整份还回去 —— 和丢掉它一样，写出来是为了
    /// 读的人看得见
    pub fn release(self) {}
}

impl std::fmt::Debug for UpstreamHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamHold")
            .field("held", &self.owner.is_some())
            .finish()
    }
}

impl Drop for UpstreamHold {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.take() {
            owner.release(self.seq);
        }
    }
}

/// 全部上游的用量账。**跨重载存活**（在 `AppState` 里）：改一条路由规则不该让这家今天用的
/// 归零。按上游的名字记。
pub struct UpstreamLimits {
    shared: Arc<Shared>,
}

/// 这一行算不算进上游的用量：网关自己答的、数 token 的不算。发到了哪几家看尝试链
fn counts(rec: &Recorded) -> bool {
    !rec.local && !uncounted(&rec.path)
}

impl UpstreamLimits {
    pub fn new(bus: tw_observe::EventBus) -> Self {
        Self::with_clock(bus, Arc::new(SystemClock))
    }

    pub fn with_clock(bus: tw_observe::EventBus, clock: Arc<dyn Clock>) -> Self {
        Self {
            shared: Arc::new(Shared {
                inner: Mutex::default(),
                clock,
                reread: Mutex::default(),
                bus,
            }),
        }
    }

    fn now(&self) -> i64 {
        self.shared.clock.now_ms()
    }

    /// 接上请求记录：一期的开头变了（到了下一期、机器换了时区）时，用 `f` 把新的这一期从
    /// 记录里重新加起来，加好了交回 [`Self::reread`]。和 `KeyLimits::reread_with` 一样：`f` 在
    /// 锁外面调、不该等，读回来之前这一期先从 0 起，同一期只要一次
    pub fn reread_with(&self, f: Reread) {
        *self
            .shared
            .reread
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(f);
    }

    /// 读回来了：`rows` 是从 `m.start` 起记下的那些，按上游分好（存储层的
    /// `upstream_usage_since`），`m.key` 是上游名。这一期的数换成它们加起来的；这一期的开头
    /// 又变了的话不换。调用方要保证读的时候没有结算在进行（见 `KeyLimits::reread`）
    pub fn reread(&self, m: &Moved, rows: &[(String, Recorded)]) {
        let mut sum = Amount::default();
        for (_, r) in rows.iter().filter(|(up, r)| *up == m.key && counts(r)) {
            sum.add(&r.amount());
        }
        {
            let mut g = self.shared.lock();
            g.asked.remove(m);
            let Some(p) = g
                .books
                .get_mut(&m.key)
                .and_then(|b| b.periods[books::calendar_index(m.per)].as_mut())
                .filter(|p| p.start == m.start)
            else {
                return;
            };
            p.sum = sum;
        }
        self.shared.wake([m.key.as_str()]);
    }

    /// 配置换了：记下每家此刻的上限。**账不动**：上限改了，已经用掉的照样算。等着的都叫醒
    /// 看一眼：上限可能松了
    pub fn configure(&self, cfg: &tw_config::Config) {
        let limits = cfg
            .providers
            .iter()
            .filter(|p| !p.limits.is_empty())
            .map(|p| (p.name.clone(), p.limits.clone()))
            .collect();
        let all: Vec<Arc<Notify>> = {
            let mut g = self.shared.lock();
            g.limits = limits;
            g.wake.values().cloned().collect()
        };
        for n in all {
            n.notify_waiters();
        }
    }

    /// 上游改了名（控制面改的）：账跟着走。手改配置文件改的名认不出来 —— 那和删掉一家、
    /// 新建一家看起来一样，新的那家从 0 起
    pub fn rename(&self, from: &str, to: &str) {
        if from == to {
            return;
        }
        let woken = {
            let mut g = self.shared.lock();
            if let Some(b) = g.books.remove(from) {
                g.books.insert(to.to_string(), b);
            }
            for r in g.held.values_mut() {
                if r.key == from {
                    r.key = to.to_string();
                }
            }
            if let Some(l) = g.limits.remove(from) {
                g.limits.insert(to.to_string(), l);
            }
            g.wake.remove(from)
        };
        // 等旧名字的叫醒：它们再看时用的是新名字
        if let Some(n) = woken {
            n.notify_waiters();
        }
    }

    /// 这家此刻能不能发。**在路上的也算**：发出去还没结束的按预留算，和 [`Self::view`] 是
    /// 同一个数。天、周、月用满了的先报（等不到）；都没满再看分钟、小时的窗口
    pub fn state(&self, upstream: &str, limits: &[KeyLimit]) -> LimitState {
        if limits.is_empty() {
            return LimitState::Free;
        }
        let now = self.now();
        let clock = self.shared.clock.as_ref();
        let (state, moved, touched) = {
            let mut g = self.shared.lock();
            let g = &mut *g;
            let touched = self.shared.sweep(g, now);
            let mut period: Option<(i64, u32)> = None;
            let mut window: Option<(i64, u32)> = None;
            for (i, l) in limits.iter().enumerate() {
                let used = books::used(&mut g.books, &g.held, clock, upstream, l, now);
                if used < l.max() {
                    continue;
                }
                let i = i as u32;
                match l.window_ms() {
                    None => {
                        let end = clock.period(l.per, now).1;
                        if period.is_none_or(|(t, _)| end > t) {
                            period = Some((end, i));
                        }
                    }
                    Some(_) => {
                        let at = books::free_at(&g.books, &g.held, upstream, l, used, now);
                        if window.is_none_or(|(t, _)| at > t) {
                            window = Some((at, i));
                        }
                    }
                }
            }
            let ms = |t: i64| t.max(0) as u64;
            let state = match (period, window) {
                (Some((t, limit)), _) => LimitState::PeriodUsedUp {
                    until_ms: ms(t),
                    limit,
                },
                (None, Some((t, limit))) => LimitState::WindowFull {
                    until_ms: ms(t),
                    limit,
                },
                (None, None) => LimitState::Free,
            };
            (
                state,
                books::take_moved(&mut g.books, &mut g.asked, upstream),
                touched,
            )
        };
        self.shared.wake(touched.iter().map(String::as_str));
        self.shared.ask_reread(moved);
        state
    }

    /// 一跳发出去的那一刻：记上一个请求（有分钟、小时上限的进滚动窗口），按输入估的 token 和
    /// 费用占着。**不看满没满** —— 那是 [`Self::state`] 的事，发不发由调用方定。这家没设上限的
    /// 什么都不记
    pub fn reserve(&self, upstream: &str, limits: &[KeyLimit], ask: &Ask) -> UpstreamHold {
        if limits.is_empty() {
            return UpstreamHold::none();
        }
        let now = self.now();
        let seq = {
            let mut g = self.shared.lock();
            let g = &mut *g;
            books::record_request(&mut g.books, upstream, limits, now);
            g.seq += 1;
            g.held.insert(
                g.seq,
                Reservation::new(upstream, now, ask.tokens, ask.cost_micros),
            );
            g.seq
        };
        UpstreamHold {
            owner: Some(self.shared.clone()),
            seq,
        }
    }

    /// 存储层记下了一行：这个请求的几跳的预留换成实数。`at_ms` 是请求开始的时刻，算在哪一期
    /// 看它。`hops` 是算数的那几跳各发给了哪一家（`tw_api::RoutingView::upstream_hops`），
    /// `served` 是用量记在哪一家（`served_by`）。
    ///
    /// 记录里算数的那几跳，每一跳给那家记一个请求；用量记在 `served` 名下。**记录里不算的那
    /// 几跳把发出时记上的也还回去**：网关自己答的、数 token 的请求，和记录里没有的那几跳
    pub fn settle(
        &self,
        id: u64,
        at_ms: i64,
        hops: &[String],
        served: Option<&str>,
        rec: &Recorded,
    ) {
        let now = self.now();
        let clock = self.shared.clock.as_ref();
        let (touched, moved) = {
            let mut g = self.shared.lock();
            let g = &mut *g;
            let held: Vec<Reservation> = g
                .by_request
                .remove(&id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|s| g.held.remove(&s))
                .collect();
            let mut touched: HashSet<String> = held.iter().map(|r| r.key.clone()).collect();
            let counted = counts(rec);
            // 滚动窗口里的请求数：发出时就记上了。记录里算数的那几跳留着，别的撤掉
            let mut left: Vec<&str> = if counted {
                hops.iter().map(String::as_str).collect()
            } else {
                Vec::new()
            };
            for r in &held {
                match left.iter().position(|h| *h == r.key) {
                    Some(i) => {
                        left.swap_remove(i);
                    }
                    None => books::unrecord(&mut g.books, r),
                }
            }
            if counted {
                let one = Amount {
                    requests: 1,
                    ..Default::default()
                };
                for up in hops {
                    add_to_periods(
                        g.books.entry(up.clone()).or_default(),
                        clock,
                        &one,
                        at_ms,
                        now,
                    );
                    touched.insert(up.clone());
                }
                if let Some(up) = served {
                    let amount = Amount {
                        requests: 0,
                        ..rec.amount()
                    };
                    let rolling = g
                        .limits
                        .get(up)
                        .and_then(|ls| books::longest_window(ls))
                        .is_some();
                    let b = g.books.entry(up.to_string()).or_default();
                    add_to_periods(b, clock, &amount, at_ms, now);
                    if rolling {
                        b.recent.push_back((now, amount));
                    }
                    touched.insert(up.to_string());
                }
            }
            let moved: Vec<Moved> = touched
                .iter()
                .flat_map(|k| books::take_moved(&mut g.books, &mut g.asked, k))
                .collect();
            (touched, moved)
        };
        self.shared.wake(touched.iter().map(String::as_str));
        self.shared.ask_reread(moved);
    }

    /// 重启之后把天、周、月的数从请求记录里加回来。`since(t)` 给出从 `t` 起每家上游记下的
    /// 那些（存储层的 `upstream_usage_since`：请求数是算数的那几跳，用量是它接下的那些）。
    /// 分钟、小时的窗口从空的开始
    pub fn rebuild(&self, mut since: impl FnMut(i64) -> Vec<(String, Recorded)>) {
        let now = self.now();
        let mut sums: Vec<(usize, i64, i64, HashMap<String, Amount>)> = Vec::new();
        for (i, per) in CALENDAR.into_iter().enumerate() {
            let (start, end) = self.shared.clock.period(per, now);
            let mut by_up: HashMap<String, Amount> = HashMap::new();
            for (up, r) in since(start).iter().filter(|(_, r)| counts(r)) {
                by_up.entry(up.clone()).or_default().add(&r.amount());
            }
            sums.push((i, start, end, by_up));
        }
        let names: Vec<String> = {
            let mut g = self.shared.lock();
            for (i, start, end, by_up) in sums {
                for b in g.books.values_mut() {
                    b.periods[i] = Some(Period {
                        start,
                        end,
                        sum: Amount::default(),
                    });
                }
                for (up, sum) in by_up {
                    g.books.entry(up).or_default().periods[i] = Some(Period { start, end, sum });
                }
            }
            g.wake.keys().cloned().collect()
        };
        self.shared.wake(names.iter().map(String::as_str));
    }

    /// 一家的每条上限此刻用了多少（`ProviderView::limits`）。在路上的按预留算，和
    /// [`Self::state`] 看的是同一个数
    pub fn view(&self, upstream: &str, limits: &[KeyLimit]) -> Vec<tw_api::KeyLimitView> {
        if limits.is_empty() {
            return Vec::new();
        }
        let now = self.now();
        let clock = self.shared.clock.as_ref();
        let (view, moved, touched) = {
            let mut g = self.shared.lock();
            let g = &mut *g;
            let touched = self.shared.sweep(g, now);
            let view = limits
                .iter()
                .map(|l| {
                    let used = books::used(&mut g.books, &g.held, clock, upstream, l, now);
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
                            .then(|| clock.period(l.per, now).1.max(0) as u64),
                        reached: used >= max,
                    }
                })
                .collect();
            (
                view,
                books::take_moved(&mut g.books, &mut g.asked, upstream),
                touched,
            )
        };
        self.shared.wake(touched.iter().map(String::as_str));
        self.shared.ask_reread(moved);
        view
    }

    /// 等到这家的 [`Self::state`] 可能变了：有预留还回来、有一行结算了、配置换了、改了名，
    /// 或者到了它满着的那条上限空出来（下一期开始）的时刻。**可能没变** —— 醒了再看一眼
    /// `state`，还满着就再等。按此刻配置里这家的上限算时刻
    pub async fn changed(&self, upstream: &str) {
        let notify = self
            .shared
            .lock()
            .wake
            .entry(upstream.to_string())
            .or_default()
            .clone();
        let notified = notify.notified();
        tokio::pin!(notified);
        // 先登记再看：看完之后、等之前发生的变化也叫得醒它
        notified.as_mut().enable();
        let limits = self
            .shared
            .lock()
            .limits
            .get(upstream)
            .cloned()
            .unwrap_or_default();
        let until = match self.state(upstream, &limits) {
            LimitState::Free => None,
            LimitState::WindowFull { until_ms, .. } | LimitState::PeriodUsedUp { until_ms, .. } => {
                Some(until_ms)
            }
        };
        match until {
            Some(t) => {
                let ms = (t as i64 - self.now()).max(0) as u64;
                tokio::select! {
                    _ = &mut notified => {}
                    _ = tokio::time::sleep(Duration::from_millis(ms)) => {}
                }
            }
            None => notified.await,
        }
    }
}

/// 一笔记进天、周、月：`at_ms`（请求开始的时刻）落在 `now` 所在的那一期里的才记
fn add_to_periods(b: &mut Books, clock: &dyn Clock, amount: &Amount, at_ms: i64, now: i64) {
    for per in CALENDAR {
        let p = b.period(clock, per, now);
        if at_ms >= p.start && at_ms < p.end {
            p.sum.add(amount);
        }
    }
}

#[cfg(test)]
mod tests;
