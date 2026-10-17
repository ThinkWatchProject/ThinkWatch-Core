//! 一个在跑的请求此刻生成得多快（`/live` 的速度，见 `tw_api::LiveView::tokens_per_sec`）。
//!
//! 数据面每读到一段输出就记一笔（[`Meter::add`]），记的是估出来的 token 数；`/live` 来问时
//! 看最近 [`WINDOW`] 里记了多少。**按格记，不按笔记**：一条流一秒几十笔，留着每一笔
//! 既费内存又要逐笔加；按 [`SLOT`] 一格攒着，窗口里最多 [`SLOTS`] 格。
//!
//! 一个请求一份，开始事件时由总线建好（见 `EventBus::meter`），结局时跟着请求一起划掉。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 速度看最近多久
pub const WINDOW: Duration = Duration::from_millis(tw_api::LIVE_RATE_WINDOW_MS);
/// 一格多长
const SLOT: Duration = Duration::from_millis(250);
/// 窗口里有几格
const SLOTS: u64 = tw_api::LIVE_RATE_WINDOW_MS / 250;

#[derive(Debug)]
pub struct Meter {
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// 格号从这一刻数起
    origin: Instant,
    /// 第一笔的时刻。**之前没有速度**：还在等第一个 token
    first: Option<Instant>,
    /// 到目前为止一共记了多少
    total: u64,
    /// （格号，这一格里记了多少），格号从小到大，只留窗口里的
    slots: VecDeque<(u64, u64)>,
}

/// 此刻的读数
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    pub tokens_per_sec: u32,
    pub total: u64,
}

impl Meter {
    pub fn new(now: Instant) -> Self {
        Self {
            inner: Mutex::new(Inner {
                origin: now,
                first: None,
                total: 0,
                slots: VecDeque::new(),
            }),
        }
    }

    /// 又生成了 `tokens` 个。**0 也算开了口**：第一个 token 的那一段可能估出 0 个（一个
    /// 空格），之后的速度要从这一刻算起
    pub fn add(&self, tokens: u64) {
        self.add_at(tokens, Instant::now());
    }

    pub fn add_at(&self, tokens: u64, now: Instant) {
        let mut m = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        m.first.get_or_insert(now);
        m.total = m.total.saturating_add(tokens);
        let slot = m.slot(now);
        match m.slots.back_mut() {
            Some((s, n)) if *s == slot => *n = n.saturating_add(tokens),
            _ => m.slots.push_back((slot, tokens)),
        }
        m.forget(slot);
    }

    /// 到目前为止一共记了多少
    pub fn total(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).total
    }

    /// 此刻的速度：窗口里记的除以窗口的长度 —— 开口还不到一个窗口的，除以开口以来的时间
    /// （至少一格，刚开口那一下不至于除出个天文数字）。还没开口的没有读数
    pub fn read_at(&self, now: Instant) -> Option<Reading> {
        let mut m = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let first = m.first?;
        let slot = m.slot(now);
        m.forget(slot);
        let tokens: u64 = m.slots.iter().map(|(_, n)| n).sum();
        let span = now
            .saturating_duration_since(first)
            .clamp(SLOT, WINDOW)
            .as_millis() as u64;
        Some(Reading {
            tokens_per_sec: (tokens.saturating_mul(1000) / span).min(u64::from(u32::MAX)) as u32,
            total: m.total,
        })
    }
}

impl Inner {
    fn slot(&self, now: Instant) -> u64 {
        (now.saturating_duration_since(self.origin).as_millis() / SLOT.as_millis()) as u64
    }

    /// 掉出窗口的格扔掉：此刻在第 `slot` 格，留它和它之前的 `SLOTS - 1` 格
    fn forget(&mut self, slot: u64) {
        while self.slots.front().is_some_and(|(s, _)| *s + SLOTS <= slot) {
            self.slots.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn nothing_before_the_first_token() {
        let t = Instant::now();
        let m = Meter::new(t);
        assert_eq!(m.read_at(t + ms(5_000)), None);
        // 开了口、估出 0 个：从这一刻起有速度，是 0
        m.add_at(0, t + ms(5_000));
        assert_eq!(
            m.read_at(t + ms(6_000)),
            Some(Reading {
                tokens_per_sec: 0,
                total: 0
            })
        );
    }

    /// 开口不到一个窗口：除以开口以来的时间，不是整个窗口 —— 不然刚开口的一秒只显示三分之一
    #[test]
    fn a_fresh_stream_is_divided_by_the_time_since_it_opened() {
        let t = Instant::now();
        let m = Meter::new(t);
        m.add_at(10, t + ms(1_000));
        m.add_at(40, t + ms(1_500));
        m.add_at(50, t + ms(2_000));
        assert_eq!(m.read_at(t + ms(2_000)).unwrap().tokens_per_sec, 100);
    }

    /// 窗口之外的不算；停下来不吐字的时候一路掉到 0
    #[test]
    fn the_window_forgets_and_a_stalled_stream_falls_to_zero() {
        let t = Instant::now();
        let m = Meter::new(t);
        for i in 0..20 {
            m.add_at(30, t + ms(i * 250));
        }
        // 第 5 秒：窗口是 2 秒到 5 秒那 12 格，每格 30 个
        let r = m.read_at(t + ms(4_999)).unwrap();
        assert_eq!(r.tokens_per_sec, 120);
        assert_eq!(r.total, 600);
        assert_eq!(m.read_at(t + ms(20_000)).unwrap().tokens_per_sec, 0);
        assert_eq!(m.read_at(t + ms(20_000)).unwrap().total, 600);
    }

    #[test]
    fn the_very_first_instant_is_not_divided_by_zero() {
        let t = Instant::now();
        let m = Meter::new(t);
        m.add_at(5, t);
        assert_eq!(m.read_at(t).unwrap().tokens_per_sec, 20);
    }
}
