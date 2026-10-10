//! 时间条件：`[<days> ]<HH:MM>-<HH:MM>`，按 core 所在机器的本地时间。
//!
//! 「工作日白天走公司的账号、夜里和周末走便宜的」是按时间分流的全部需求，
//! 所以一条写法只有一个窗口：哪几天、几点到几点。要几个窗口就写几条值，
//! 满足其一即可，和 `intent` 一样。
//!
//! - 天：`mon,tue,wed,thu,fri,sat,sun`，用逗号列出，`mon-fri`、`sat-sun` 是一段，
//!   `fri-mon` 跨过周末绕回去。不写天就是每天。大小写不论，存的时候是小写。
//! - 时刻：24 小时制，起点含、终点不含，终点可以是 `24:00`。终点早于起点的是过夜的
//!   窗口（`22:00-06:00` 是 22:00 到次日 05:59），**天指的是窗口开始的那一天**。
//!
//! **按本地时间**：用户说「九点到六点」，说的是他墙上的钟，不是 UTC。

use std::fmt;

/// 一个时刻：星期几、从零点起过了多少分钟。本地时间。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LocalTime {
    /// 0 = 周一 … 6 = 周日
    pub weekday: u8,
    /// 从零点起过了多少分钟，0 到 1439
    pub minute: u16,
}

const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

impl LocalTime {
    /// 此刻，按 core 所在机器的时区。
    pub fn now() -> Self {
        Self::from(chrono::Local::now())
    }

    /// 某一天、某一刻。`weekday` 0 = 周一；超出范围的按 7 取余、分钟封顶到 23:59。
    pub fn at(weekday: u8, hour: u16, minute: u16) -> Self {
        Self {
            weekday: weekday % 7,
            minute: (hour * 60 + minute).min(24 * 60 - 1),
        }
    }
}

impl<Tz: chrono::TimeZone> From<chrono::DateTime<Tz>> for LocalTime {
    fn from(t: chrono::DateTime<Tz>) -> Self {
        use chrono::{Datelike, Timelike};
        Self {
            weekday: t.weekday().num_days_from_monday() as u8,
            minute: (t.hour() * 60 + t.minute()) as u16,
        }
    }
}

/// 写出来是 `fri 17:30`：试算里「实际的值」用它，和条件的写法对得上
impl fmt::Display for LocalTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {:02}:{:02}",
            DAYS[(self.weekday % 7) as usize],
            self.minute / 60,
            self.minute % 60
        )
    }
}

/// 一个时间窗口：哪几天的几点到几点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// 位 0 = 周一 … 位 6 = 周日。没写天的是全满
    pub days: u8,
    /// 起点，从零点起的分钟数，含
    pub start: u16,
    /// 终点，从零点起的分钟数，不含；`24:00` 是 1440。小于等于起点的是过夜的窗口
    pub end: u16,
}

const ALL_DAYS: u8 = 0b111_1111;

impl Window {
    /// 这一刻在窗口里吗。
    ///
    /// 过夜的窗口分两段看：起点那一天从起点到午夜，和**下一天**从零点到终点 ——
    /// 周五 `22:00-06:00` 管到周六早上六点，周六自己不用在天的列表里。
    pub fn contains(&self, t: LocalTime) -> bool {
        let day = t.weekday % 7;
        let on = |d: u8| self.days & (1 << d) != 0;
        if self.end > self.start {
            return on(day) && t.minute >= self.start && t.minute < self.end;
        }
        let yesterday = (day + 6) % 7;
        (on(day) && t.minute >= self.start) || (on(yesterday) && t.minute < self.end)
    }
}

/// 写法错在哪儿。
///
/// 对外只有一句话（`engine.rule_time_syntax`，见 [`crate::RouteError`]）：这几种原因是给
/// 测试和日志分辨用的，用户看到的那句话把整个写法说一遍更管用。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TimeError {
    #[error("the value is empty")]
    Empty,
    #[error("`{0}` is not a day; days are mon, tue, wed, thu, fri, sat, sun")]
    BadDay(String),
    #[error("`{0}` is not a day range like mon-fri")]
    BadDayRange(String),
    #[error("`{0}` is not a time of day written as HH:MM")]
    BadTime(String),
    #[error("`{0}` is not a time range written as HH:MM-HH:MM")]
    BadTimeRange(String),
    #[error("the window starts and ends at the same minute")]
    EmptyWindow,
    #[error("more than a day list and a time range")]
    TooManyParts,
}

/// 解析 `[<days> ]<HH:MM>-<HH:MM>`。
///
/// 纯函数，大小写不论。多余的空白容忍（首尾、天和时间之间可以多个），天之间的逗号和
/// 时间的连字符旁边不能有空白 —— 一个窗口一个写法，才能照原样回填到界面上。
pub fn parse(s: &str) -> Result<Window, TimeError> {
    let lower = s.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return Err(TimeError::Empty);
    }
    let mut parts = lower.split_whitespace();
    let first = parts.next().ok_or(TimeError::Empty)?;
    let (days, times) = match parts.next() {
        None => (ALL_DAYS, first),
        Some(second) => {
            if parts.next().is_some() {
                return Err(TimeError::TooManyParts);
            }
            (parse_days(first)?, second)
        }
    };
    let (a, b) = times
        .split_once('-')
        .ok_or_else(|| TimeError::BadTimeRange(times.to_string()))?;
    let start = parse_clock(a)?;
    let end = parse_clock(b)?;
    // 起点不能是 24:00：那一分钟不存在。终点是 24:00 时是「到这一天结束」
    if start >= 24 * 60 {
        return Err(TimeError::BadTime(a.to_string()));
    }
    if start == end {
        return Err(TimeError::EmptyWindow);
    }
    Ok(Window { days, start, end })
}

/// `mon,wed,fri-sun` → 位图。
fn parse_days(s: &str) -> Result<u8, TimeError> {
    let mut days = 0u8;
    for item in s.split(',') {
        match item.split_once('-') {
            None => days |= 1 << day_index(item)?,
            Some((from, to)) => {
                if from.is_empty() || to.is_empty() {
                    return Err(TimeError::BadDayRange(item.to_string()));
                }
                let (from, to) = (day_index(from)?, day_index(to)?);
                // `fri-mon` 绕过周日回到周一：从起点一天天走到终点为止
                let mut d = from;
                loop {
                    days |= 1 << d;
                    if d == to {
                        break;
                    }
                    d = (d + 1) % 7;
                }
            }
        }
    }
    Ok(days)
}

fn day_index(s: &str) -> Result<u8, TimeError> {
    DAYS.iter()
        .position(|d| *d == s)
        .map(|i| i as u8)
        .ok_or_else(|| TimeError::BadDay(s.to_string()))
}

/// `HH:MM` → 分钟数。`24:00` 读成 1440，由调用方决定允不允许。
fn parse_clock(s: &str) -> Result<u16, TimeError> {
    let bad = || TimeError::BadTime(s.to_string());
    let (h, m) = s.split_once(':').ok_or_else(bad)?;
    if h.len() != 2 || m.len() != 2 || !h.bytes().chain(m.bytes()).all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let (h, m): (u16, u16) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
    if m >= 60 || h > 24 || (h == 24 && m != 0) {
        return Err(bad());
    }
    Ok(h * 60 + m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Window {
        parse(s).unwrap_or_else(|e| panic!("`{s}`: {e}"))
    }

    fn at(day: &str, hhmm: &str) -> LocalTime {
        let (h, m) = hhmm.split_once(':').unwrap();
        LocalTime::at(
            day_index(day).unwrap(),
            h.parse().unwrap(),
            m.parse().unwrap(),
        )
    }

    #[test]
    fn the_three_examples_from_the_manual_parse() {
        assert_eq!(
            w("mon-fri 09:00-18:00"),
            Window {
                days: 0b001_1111,
                start: 9 * 60,
                end: 18 * 60
            }
        );
        assert_eq!(
            w("sat,sun 00:00-24:00"),
            Window {
                days: 0b110_0000,
                start: 0,
                end: 24 * 60
            }
        );
        assert_eq!(
            w("22:00-06:00"),
            Window {
                days: ALL_DAYS,
                start: 22 * 60,
                end: 6 * 60
            }
        );
    }

    #[test]
    fn day_lists_mix_single_days_and_ranges() {
        assert_eq!(w("mon,wed,fri 09:00-10:00").days, 0b001_0101);
        assert_eq!(w("mon,thu-sat 09:00-10:00").days, 0b011_1001);
        assert_eq!(w("sun 09:00-10:00").days, 0b100_0000);
        // 同一天写两遍只是多余，不是错
        assert_eq!(w("mon,mon 09:00-10:00").days, 0b000_0001);
    }

    #[test]
    fn a_day_range_may_wrap_past_sunday() {
        // 周五到周一：五、六、日、一
        assert_eq!(w("fri-mon 09:00-10:00").days, 0b111_0001);
        assert_eq!(w("sun-mon 09:00-10:00").days, 0b100_0001);
        // 起点就是终点：只有那一天
        assert_eq!(w("wed-wed 09:00-10:00").days, 0b000_0100);
    }

    #[test]
    fn input_is_case_insensitive_and_tolerates_outer_whitespace() {
        assert_eq!(w("  Mon-FRI   09:00-18:00 "), w("mon-fri 09:00-18:00"));
        assert_eq!(w("SAT,Sun 00:00-24:00"), w("sat,sun 00:00-24:00"));
    }

    #[test]
    fn malformed_values_are_refused() {
        for (bad, why) in [
            ("", TimeError::Empty),
            ("   ", TimeError::Empty),
            ("mon-fri", TimeError::BadTime("mon".into())),
            ("09:00", TimeError::BadTimeRange("09:00".into())),
            ("9:00-18:00", TimeError::BadTime("9:00".into())),
            ("09:00-18", TimeError::BadTime("18".into())),
            ("09:60-18:00", TimeError::BadTime("09:60".into())),
            ("25:00-26:00", TimeError::BadTime("25:00".into())),
            ("24:00-06:00", TimeError::BadTime("24:00".into())),
            ("09:00-24:30", TimeError::BadTime("24:30".into())),
            ("09:00-09:00", TimeError::EmptyWindow),
            ("monday 09:00-18:00", TimeError::BadDay("monday".into())),
            ("mon- 09:00-18:00", TimeError::BadDayRange("mon-".into())),
            ("mon,,fri 09:00-18:00", TimeError::BadDay("".into())),
            ("mon fri 09:00-18:00", TimeError::TooManyParts),
            ("09:00 - 18:00", TimeError::TooManyParts),
            ("mon-fri 09:00-18:00 extra", TimeError::TooManyParts),
        ] {
            assert_eq!(parse(bad), Err(why), "`{bad}`");
        }
    }

    #[test]
    fn start_is_inclusive_and_end_is_exclusive() {
        let office = w("mon-fri 09:00-18:00");
        assert!(office.contains(at("mon", "09:00")));
        assert!(office.contains(at("fri", "17:59")));
        assert!(!office.contains(at("mon", "08:59")));
        assert!(!office.contains(at("fri", "18:00")));
        assert!(!office.contains(at("sat", "12:00")));
    }

    #[test]
    fn midnight_as_the_end_covers_the_last_minute_of_the_day() {
        let weekend = w("sat,sun 00:00-24:00");
        assert!(weekend.contains(at("sat", "00:00")));
        assert!(weekend.contains(at("sun", "23:59")));
        assert!(!weekend.contains(at("mon", "00:00")));
        assert!(!weekend.contains(at("fri", "23:59")));
    }

    #[test]
    fn an_overnight_window_belongs_to_the_day_it_starts() {
        let night = w("fri 22:00-06:00");
        assert!(night.contains(at("fri", "22:00")));
        assert!(night.contains(at("fri", "23:59")));
        assert!(night.contains(at("sat", "00:00")));
        assert!(night.contains(at("sat", "05:59")));
        assert!(!night.contains(at("sat", "06:00")));
        assert!(
            !night.contains(at("sat", "22:00")),
            "周六晚上不在周五的窗口里"
        );
        assert!(!night.contains(at("fri", "21:59")));
        assert!(
            !night.contains(at("fri", "05:00")),
            "周五凌晨属于周四的窗口"
        );
        // 每天的过夜窗口：周日晚绕到周一早上
        let every = w("22:00-06:00");
        assert!(every.contains(at("sun", "23:00")));
        assert!(every.contains(at("mon", "05:00")));
        assert!(!every.contains(at("mon", "12:00")));
    }

    #[test]
    fn an_overnight_window_ending_at_midnight_stops_at_the_day_end() {
        // 终点 00:00 早于起点：过夜的写法，实际只到这一天的 23:59
        let late = w("mon 22:00-00:00");
        assert!(late.contains(at("mon", "23:59")));
        assert!(!late.contains(at("tue", "00:00")));
    }

    #[test]
    fn a_local_time_prints_like_a_condition_value() {
        assert_eq!(at("fri", "17:30").to_string(), "fri 17:30");
        assert_eq!(at("mon", "00:05").to_string(), "mon 00:05");
    }

    #[test]
    fn a_local_time_comes_from_a_chrono_date_in_its_own_zone() {
        use chrono::TimeZone;
        // 2026-10-10 是周六
        let t = chrono::FixedOffset::east_opt(8 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 10, 10, 9, 5, 0)
            .unwrap();
        assert_eq!(LocalTime::from(t), at("sat", "09:05"));
        // 同一瞬间在 UTC 还是周六凌晨一点多
        assert_eq!(
            LocalTime::from(t.with_timezone(&chrono::Utc)),
            at("sat", "01:05")
        );
    }
}
