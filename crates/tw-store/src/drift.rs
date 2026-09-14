//! 上游行为基线：这一家最近是不是变了（防线三）。
//!
//! > 某个中转站用了三个月一直正常，某天开始返回大量 bash 调用 ——
//! > 这是统计异常，值得告警。
//!
//! # 这个功能唯一的难点是别乱叫
//!
//! 检测本身是两个比率相减。难的是**在样本不够的时候闭嘴**：一个人一天
//! 的流量可能只有几十条请求，而「3 条里有 1 条带工具调用」和「300 条里
//! 有 100 条」在数学上是同一个比率，在证据强度上差着量级。
//!
//! 所以三道闸门，全过了才说话：
//!
//! 1. **两边都要有足够样本**（各 20 条），否则一个字都不说。
//! 2. **倍数和绝对值都要够**。`0.1% → 0.4%` 是四倍，但它什么都不是。
//! 3. **报数字，不报结论。**「最近 24 小时 40%，之前 30 天 3%（样本
//!    120 / 4,200）」比「检测到异常」有用得多 —— 后者用户没法验证，
//!    也没法判断该不该管。

use crate::db::Shape;

/// 两边各自至少要有多少条**数过形状的**请求。
///
/// 20 是个小数字，但它挡掉的正是最刺眼的那类误报：刚配好一个上游、
/// 跑了三条请求，其中一条带工具调用 —— 33%，而基线是 2%。
const MIN_SAMPLE: i64 = 20;

/// 一处变化。
#[derive(Debug, Clone, PartialEq)]
pub struct Drift {
    /// `tool_calls` / `flagged` / `errors`
    pub metric: &'static str,
    pub label: &'static str,
    pub recent: f64,
    pub baseline: f64,
    /// 两边各自的样本量。**必须一起显示** —— 没有它，比率是个没法判断
    /// 可信度的数字（分位数同一条纪律）
    pub recent_n: i64,
    pub baseline_n: i64,
    /// 这一条要不要引起注意。**危险规则那一项的门槛低得多**
    pub notable: bool,
}

fn jumped(recent: f64, baseline: f64) -> bool {
    // 倍数和绝对值都要够。`0.1% → 0.4%` 是四倍，但它什么都不是
    let big_relative = baseline == 0.0 || recent >= baseline * 3.0;
    let big_absolute = recent - baseline >= 0.10;
    big_relative && big_absolute
}

/// 比一比。**样本不够就返回空** —— 一个字都不说。
pub fn compare(recent: &Shape, base: &Shape) -> Vec<Drift> {
    let mut out = Vec::new();

    // 一、危险规则命中率。**这一项的门槛低得多**：以前一次都没有过，
    // 现在开始有了 —— 那本身就是要说的事，不必等它涨到 10%
    if recent.inspected >= MIN_SAMPLE && base.inspected >= MIN_SAMPLE {
        let (r, b) = (
            recent.flag_rate().unwrap_or(0.0),
            base.flag_rate().unwrap_or(0.0),
        );
        if recent.with_flags > 0 && (b == 0.0 || jumped(r, b)) {
            out.push(Drift {
                metric: "flagged",
                label: "命中危险规则的响应",
                recent: r,
                baseline: b,
                recent_n: recent.inspected,
                baseline_n: base.inspected,
                notable: true,
            });
        }

        // 二、工具调用率。**这是那句「某天开始返回大量 bash 调用」**
        let (r, b) = (
            recent.tool_rate().unwrap_or(0.0),
            base.tool_rate().unwrap_or(0.0),
        );
        if jumped(r, b) {
            out.push(Drift {
                metric: "tool_calls",
                label: "带工具调用的响应",
                recent: r,
                baseline: b,
                recent_n: recent.inspected,
                baseline_n: base.inspected,
                notable: true,
            });
        }
    }

    // 三、失败率。它不需要「数过形状」，所以用的是总数
    if recent.total >= MIN_SAMPLE && base.total >= MIN_SAMPLE {
        let (r, b) = (
            recent.error_rate().unwrap_or(0.0),
            base.error_rate().unwrap_or(0.0),
        );
        if jumped(r, b) {
            out.push(Drift {
                metric: "errors",
                label: "失败的请求",
                recent: r,
                baseline: b,
                recent_n: recent.total,
                baseline_n: base.total,
                // 失败率变高多半是上游自己的问题，不是投毒 —— 值得看，
                // 但不该和「开始返回 bash 调用」用同一个语气
                notable: false,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(total: i64, inspected: i64, tools: i64, flags: i64, errors: i64) -> Shape {
        Shape {
            inspected,
            with_tools: tools,
            with_flags: flags,
            median_bytes: 1000,
            errors,
            total,
        }
    }

    #[test]
    fn a_relay_that_starts_returning_bash_calls_is_reported() {
        // 那句话：某个中转站用了三个月一直正常，某天开始返回
        // 大量 bash 调用。
        let base = shape(4200, 4200, 126, 0, 10); // 3%
        let recent = shape(120, 120, 48, 0, 1); // 40%
        let d = compare(&recent, &base);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].metric, "tool_calls");
        assert!(d[0].notable);
        assert!((d[0].recent - 0.4).abs() < 0.01);
        assert!((d[0].baseline - 0.03).abs() < 0.01);
        // **样本量必须一起给出来** —— 没有它，比率是个没法判断可信度的数字
        assert_eq!((d[0].recent_n, d[0].baseline_n), (120, 4200));
    }

    #[test]
    fn a_brand_new_upstream_with_three_requests_says_nothing() {
        // **挡掉的正是最刺眼的那类误报**：刚配好一个上游、跑了三条请求，
        // 其中一条带工具调用 —— 33%，而基线是 2%。
        let base = shape(4200, 4200, 84, 0, 0);
        let recent = shape(3, 3, 1, 0, 0);
        assert!(compare(&recent, &base).is_empty());
    }

    #[test]
    fn a_thin_baseline_also_keeps_us_quiet() {
        // 反过来也一样：基线只有五条的时候，任何「变化」都不构成证据。
        let base = shape(5, 5, 0, 0, 0);
        let recent = shape(200, 200, 80, 0, 0);
        assert!(compare(&recent, &base).is_empty());
    }

    #[test]
    fn a_small_ratio_change_is_not_worth_saying() {
        // `0.1% → 0.4%` 是四倍，但它什么都不是。
        let base = shape(4000, 4000, 4, 0, 0);
        let recent = shape(1000, 1000, 4, 0, 0);
        assert!(
            compare(&recent, &base).is_empty(),
            "{:?}",
            compare(&recent, &base)
        );
    }

    #[test]
    fn a_big_multiple_without_a_big_absolute_change_stays_quiet() {
        // 2% → 8% 是四倍，绝对值只涨了六个点 —— 还不够。
        let base = shape(4000, 4000, 80, 0, 0);
        let recent = shape(1000, 1000, 80, 0, 0);
        assert!(compare(&recent, &base).is_empty());
    }

    #[test]
    fn the_first_ever_dangerous_hit_is_reported_without_waiting_for_a_trend() {
        // **危险规则那一项门槛低得多**：以前一次都没有过、现在开始有了，
        // 那本身就是要说的事，不必等它涨到 10%。
        let base = shape(4200, 4200, 100, 0, 0);
        let recent = shape(100, 100, 3, 1, 0); // 1%
        let d = compare(&recent, &base);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].metric, "flagged");
        assert!(d[0].notable);
    }

    #[test]
    fn a_steady_low_rate_of_hits_is_not_news_every_day() {
        // 一直有那么一点点，不是「开始了」。天天报同一件事，用户会关掉
        // 整个功能。
        let base = shape(4200, 4200, 100, 42, 0); // 1%
        let recent = shape(100, 100, 3, 1, 0); // 1%
        assert!(compare(&recent, &base).is_empty());
    }

    #[test]
    fn a_rise_in_failures_is_reported_but_in_a_quieter_voice() {
        // 失败率变高多半是上游自己的问题，不是投毒。
        let base = shape(4000, 4000, 0, 0, 40);
        let recent = shape(200, 200, 0, 0, 60);
        let d = compare(&recent, &base);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].metric, "errors");
        assert!(!d[0].notable, "失败率不该和「开始返回 bash 调用」一个语气");
    }

    #[test]
    fn a_window_where_nothing_was_measured_produces_nothing() {
        // 关掉入站审查的那段时间没有数过形状 —— 那时候比的是空气。
        let base = shape(4000, 0, 0, 0, 0);
        let recent = shape(200, 0, 0, 0, 0);
        assert!(compare(&recent, &base).is_empty());
    }
}
