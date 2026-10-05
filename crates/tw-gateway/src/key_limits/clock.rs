//! 用量上限看的时钟：此刻几点，和某一刻所在的那一天、那一周、那个月从哪儿起、到哪儿止。
//!
//! **天、周、月按 core 所在机器的本地时区算**：用户说「每天 $5」，说的是他自己那一天，
//! 不是 UTC 那一天 —— 东八区的人早上八点之前花的钱，按 UTC 算会记到前一天去。
//!
//! 时钟可以换：测试要把时间拨到零点前一秒、拨过周一、拨过一号，还要让滚动窗口的等待
//! 跟着 tokio 的假时间走。

use chrono::{Datelike, Days, NaiveDate, NaiveTime, TimeZone};
use tw_config::LimitPer;

pub trait Clock: Send + Sync + 'static {
    /// 此刻，Unix 毫秒
    fn now_ms(&self) -> i64;
    /// `at_ms` 所在的那一期（天、周、月）的开头和结尾，Unix 毫秒，结尾不含。分钟、小时
    /// 是滚动的，没有「那一期」：给的是以 `at_ms` 结尾的那一段
    fn period(&self, per: LimitPer, at_ms: i64) -> (i64, i64);
    /// 某一刻写给人看：`2026-10-06 00:00 +08:00`。带着时区，远程连上来的人也看得懂
    fn show(&self, at_ms: i64) -> String;
}

/// 系统时钟，本机时区。
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
    fn period(&self, per: LimitPer, at_ms: i64) -> (i64, i64) {
        period_in(&chrono::Local, per, at_ms)
    }
    fn show(&self, at_ms: i64) -> String {
        show_in(&chrono::Local, at_ms)
    }
}

/// 测试用的时钟：固定的时区，时间从 `base_ms` 起跟着 tokio 的时钟走 —— 测试里
/// `tokio::time::pause` 之后 `advance` 多少，它就走多少，滚动窗口的等待也一起快进。
pub struct TestClock {
    offset: chrono::FixedOffset,
    base_ms: i64,
    start: tokio::time::Instant,
}

impl TestClock {
    /// `offset_secs`：时区比 UTC 快多少秒（东八区是 `8 * 3600`）
    pub fn new(base_ms: i64, offset_secs: i32) -> Self {
        Self {
            offset: chrono::FixedOffset::east_opt(offset_secs).expect("a valid offset"),
            base_ms,
            start: tokio::time::Instant::now(),
        }
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.base_ms + self.start.elapsed().as_millis() as i64
    }
    fn period(&self, per: LimitPer, at_ms: i64) -> (i64, i64) {
        period_in(&self.offset, per, at_ms)
    }
    fn show(&self, at_ms: i64) -> String {
        show_in(&self.offset, at_ms)
    }
}

fn show_in<Tz: TimeZone>(tz: &Tz, at_ms: i64) -> String
where
    Tz::Offset: std::fmt::Display,
{
    match tz.timestamp_millis_opt(at_ms).single() {
        Some(t) => t.format("%Y-%m-%d %H:%M %:z").to_string(),
        None => at_ms.to_string(),
    }
}

/// 某一刻所在的那一期，按时区 `tz`。周从周一开始。
pub(crate) fn period_in<Tz: TimeZone>(tz: &Tz, per: LimitPer, at_ms: i64) -> (i64, i64) {
    if let Some(w) = per.rolling_ms() {
        return (at_ms - w, at_ms);
    }
    let Some(t) = tz.timestamp_millis_opt(at_ms).single() else {
        return (at_ms, at_ms + 86_400_000);
    };
    let d = t.date_naive();
    let (start, end) = match per {
        LimitPer::Week => {
            let s = d - Days::new(u64::from(d.weekday().num_days_from_monday()));
            (s, s + Days::new(7))
        }
        LimitPer::Month => {
            let s = d.with_day(1).unwrap_or(d);
            let e = if s.month() == 12 {
                NaiveDate::from_ymd_opt(s.year() + 1, 1, 1)
            } else {
                NaiveDate::from_ymd_opt(s.year(), s.month() + 1, 1)
            };
            (s, e.unwrap_or(s + Days::new(31)))
        }
        _ => (d, d + Days::new(1)),
    };
    (midnight(tz, start), midnight(tz, end))
}

/// 这一天零点，Unix 毫秒。
///
/// **夏令时在零点切换的地方，零点可能不存在**（时钟从 23:59 直接跳到 01:00）：那一天
/// 从跳过去之后的第一刻算起。零点出现两次的（往回拨），从第一次算起。
fn midnight<Tz: TimeZone>(tz: &Tz, d: NaiveDate) -> i64 {
    let at = d.and_time(NaiveTime::MIN);
    for hours in 0..=3 {
        let local = at + chrono::Duration::hours(hours);
        if let Some(t) = tz.from_local_datetime(&local).earliest() {
            return t.timestamp_millis();
        }
    }
    // 找不到就按 UTC 算：只差几个小时，总比不算好
    at.and_utc().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-05 是周一。东八区的那一天从前一天 16:00 UTC 起
    #[test]
    fn a_day_a_week_and_a_month_start_at_local_midnight() {
        let tz = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let at = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .timestamp_millis()
        };
        // 周三中午
        let now = at("2026-10-07T12:00:00+08:00");
        assert_eq!(
            period_in(&tz, LimitPer::Day, now),
            (
                at("2026-10-07T00:00:00+08:00"),
                at("2026-10-08T00:00:00+08:00")
            )
        );
        assert_eq!(
            period_in(&tz, LimitPer::Week, now),
            (
                at("2026-10-05T00:00:00+08:00"),
                at("2026-10-12T00:00:00+08:00")
            ),
            "周从周一开始"
        );
        assert_eq!(
            period_in(&tz, LimitPer::Month, now),
            (
                at("2026-10-01T00:00:00+08:00"),
                at("2026-11-01T00:00:00+08:00")
            )
        );
        // 十二月的下一期在明年
        assert_eq!(
            period_in(&tz, LimitPer::Month, at("2026-12-31T23:59:59+08:00")).1,
            at("2027-01-01T00:00:00+08:00")
        );
        // UTC 已经是第二天了，本地还是这一天：按本地算
        assert_eq!(
            period_in(&tz, LimitPer::Day, at("2026-10-07T23:30:00+08:00")).0,
            at("2026-10-07T00:00:00+08:00")
        );
        // 周日还在这一周里
        assert_eq!(
            period_in(&tz, LimitPer::Week, at("2026-10-11T23:59:59+08:00")).0,
            at("2026-10-05T00:00:00+08:00")
        );
        // 滚动的：以此刻结尾的那一段
        assert_eq!(period_in(&tz, LimitPer::Minute, now), (now - 60_000, now));
    }

    #[test]
    fn a_moment_is_shown_with_its_offset() {
        let tz = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let ms = chrono::DateTime::parse_from_rfc3339("2026-10-06T00:00:00+08:00")
            .unwrap()
            .timestamp_millis();
        assert_eq!(show_in(&tz, ms), "2026-10-06 00:00 +08:00");
    }
}
