//! 订阅额度：**答案就在响应头里**。
//!
//! 按量付费的用户看金额，订阅用户看百分比 —— 而后者那个数字一直在我们
//! 手上：Anthropic 在**每一个响应**的头里带着订阅窗口的用量，Codex 是
//! 同形状的另一组。
//!
//! **零成本**：不发额外请求、不要额外 scope、不占用户自己的配额 ——
//! 顺着真实流量白捡。
//!
//! **只被动采样，不主动探测。**主动查会占掉用户自己的配额，还可能触发
//! 上游的反滥用检测；而桌面用户手里就一两个账号，每一个都金贵。唯一的
//! 例外是他在界面上手动点「立即刷新」—— 那是他明确的意图，而且他知道
//! 代价。
//!
//! 一条贯穿这里的纪律：**分清真实信号和本地猜测。**上游给的数字可以
//! 直接显示，我们推断的要标成推断。理由和三态成本完全一样：
//! **一个编出来的精确数字，比一个诚实的「不知道」更有害。**

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

/// 一个额度窗口的状态。**每个字段都直接来自响应头，没有一个是推算的。**
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// 哪个窗口：`5h` / `7d`（Anthropic）/ `weekly`（Codex）
    pub window: String,
    /// 用了百分之多少。0–100
    pub used_percent: f64,
    /// 还有多少秒重置。**上游没给就是 None** —— 那时界面上只能说
    /// 「不知道什么时候重置」，不能编一个倒计时
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_in_secs: Option<u64>,
    /// `allowed` / `allowed_warning` / `rejected`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

impl Window {
    /// 上游说「快到了」。**限流不再是突然发生的**。
    pub fn warning(&self) -> bool {
        matches!(self.status.as_deref(), Some("allowed_warning"))
            // 上游没给 status 时，用百分比自己判 —— 而这一条是**推断**，
            // 调用方要按推断展示
            || (self.status.is_none() && self.used_percent >= 80.0)
    }

    /// 上游明确说「不行了」。收到它就该提前退避，而不是等 429。
    pub fn rejected(&self) -> bool {
        matches!(self.status.as_deref(), Some("rejected"))
    }

    /// 这个数字是上游给的，还是我们推断的。
    ///
    /// **界面上必须区分。**「5h 窗口 62%」和「估计用了 62%」是两句不同
    /// 的话，而后者说成前者就是在编。
    pub fn from_upstream(&self) -> bool {
        true
    }
}

/// 一次响应里读到的全部额度信息。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Quota {
    pub windows: Vec<Window>,
}

impl Quota {
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// 最紧张的那个窗口。菜单栏只有 50 像素，显示它。
    pub fn tightest(&self) -> Option<&Window> {
        self.windows
            .iter()
            .max_by(|a, b| a.used_percent.total_cmp(&b.used_percent))
    }
}

/// 从响应头里读额度。**读不到就是空的**，不是零 —— 按量付费的账号本来
/// 就没有这些头，把它当成「用了 0%」会在菜单栏上显示一个假的进度条。
pub fn from_headers(h: &HeaderMap) -> Quota {
    let mut windows = Vec::new();
    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok());

    // ── Anthropic ────────────────────────────────────────────────
    for key in ["5h", "7d"] {
        let Some(u) = get(&format!("anthropic-ratelimit-unified-{key}-utilization"))
            .and_then(|v| v.parse::<f64>().ok())
        else {
            continue;
        };
        windows.push(Window {
            window: key.to_string(),
            // 上游给的是 0–100 还是 0–1，各家不一样。**大于 1 就当成
            // 百分比**：一个真实的 0.62 和一个真实的 62 都要能读对，而
            // 「用了 0.62%」这种值在订阅场景下没有意义。
            used_percent: normalize_percent(u),
            reset_in_secs: get(&format!("anthropic-ratelimit-unified-{key}-reset"))
                .and_then(parse_reset),
            status: get(&format!("anthropic-ratelimit-unified-{key}-status"))
                .map(|s| s.to_string()),
        });
    }
    // 7 天窗口的越线标记是个独立的头
    if get("anthropic-ratelimit-unified-7d-surpassed-threshold") == Some("true")
        && let Some(w) = windows.iter_mut().find(|w| w.window == "7d")
        && w.status.is_none()
    {
        w.status = Some("allowed_warning".to_string());
    }

    // ── Codex ────────────────────────────────────────────────────
    //
    // **窗口多长看 `-window-minutes`，不按 primary / secondary 猜。**Plus 账号实测：
    // primary 是 10080 分钟（一周），secondary 报 0 分钟 —— 那个窗口没有启用，按名字猜
    // 会显示出一个并不存在的「5 小时额度已用 0%」。
    //
    // 百分比本来就是 0–100，**不做小数换算**：「用了 1%」读成 100%，会凭空报出一次额度用完。
    for prefix in ["primary", "secondary"] {
        let Some(u) =
            get(&format!("x-codex-{prefix}-used-percent")).and_then(|v| v.parse::<f64>().ok())
        else {
            continue;
        };
        let window = match get(&format!("x-codex-{prefix}-window-minutes"))
            .and_then(|v| v.parse::<u64>().ok())
        {
            Some(0) => continue,
            Some(minutes) => codex_window(minutes),
            // 不带窗口长度的响应：沿用之前的叫法
            None if prefix == "primary" => "weekly".to_string(),
            None => "5h".to_string(),
        };
        let used_percent = u.clamp(0.0, 100.0);
        windows.push(Window {
            window,
            used_percent,
            reset_in_secs: get(&format!("x-codex-{prefix}-reset-after-seconds"))
                .and_then(|v| v.parse().ok()),
            // Codex 不报状态。用满就是被拒 —— 这是上游数字的直接结论，不是推断
            status: (used_percent >= 100.0).then(|| "rejected".to_string()),
        });
    }
    Quota { windows }
}

/// Codex 的窗口名：一周叫 `weekly`，其余按长度写成 `5h`、`1d`、`90m`
pub fn codex_window(minutes: u64) -> String {
    match minutes {
        10080 => "weekly".to_string(),
        m if m % 1440 == 0 => format!("{}d", m / 1440),
        m if m % 60 == 0 => format!("{}h", m / 60),
        m => format!("{m}m"),
    }
}

fn normalize_percent(v: f64) -> f64 {
    let p = if v <= 1.0 { v * 100.0 } else { v };
    p.clamp(0.0, 100.0)
}

/// `reset` 头可能是秒数，也可能是一个 RFC 3339 时间戳。
///
/// **认不出来就是 None。**编一个倒计时出来，用户会照着它安排自己的活。
fn parse_reset(v: &str) -> Option<u64> {
    if let Ok(secs) = v.parse::<u64>() {
        return Some(secs);
    }
    // `2026-09-09T12:00:00Z`
    let t = chrono::DateTime::parse_from_rfc3339(v).ok()?;
    let now = chrono::Utc::now();
    let d = t.with_timezone(&chrono::Utc) - now;
    d.num_seconds().try_into().ok()
}

/// 从 reqwest 的响应头读。
///
/// **两个 HeaderMap 类型，一段逻辑。**reqwest 和 axum 各有各的
/// `HeaderMap`，而把解析抄两遍必然会漂移出两套行为。
pub fn from_headers_reqwest(h: &reqwest::header::HeaderMap) -> Quota {
    let mut axum_map = HeaderMap::new();
    for (k, v) in h.iter() {
        if let (Ok(name), Ok(val)) = (
            axum::http::HeaderName::from_bytes(k.as_ref()),
            axum::http::HeaderValue::from_bytes(v.as_bytes()),
        ) {
            axum_map.insert(name, val);
        }
    }
    from_headers(&axum_map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn anthropic_windows_are_read_straight_from_the_headers() {
        let q = from_headers(&headers(&[
            ("anthropic-ratelimit-unified-5h-utilization", "62"),
            ("anthropic-ratelimit-unified-5h-reset", "7200"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-utilization", "18"),
        ]));
        assert_eq!(q.windows.len(), 2);
        let five = &q.windows[0];
        assert_eq!(five.window, "5h");
        assert_eq!(five.used_percent, 62.0);
        assert_eq!(five.reset_in_secs, Some(7200));
        assert_eq!(five.status.as_deref(), Some("allowed"));
    }

    #[test]
    fn a_fractional_utilization_is_read_as_a_percentage_too() {
        // 各家给 0–1 还是 0–100 不一样，而「用了 0.62%」在订阅场景下
        // 没有意义 —— 读错的话菜单栏上会显示一个几乎空着的进度条。
        let q = from_headers(&headers(&[(
            "anthropic-ratelimit-unified-5h-utilization",
            "0.62",
        )]));
        assert_eq!(q.windows[0].used_percent, 62.0);
    }

    #[test]
    fn a_pay_as_you_go_response_yields_nothing_not_zero() {
        // **按量付费的账号本来就没有这些头。**当成「用了 0%」会在菜单栏
        // 上显示一个假的进度条。
        let q = from_headers(&headers(&[("content-type", "application/json")]));
        assert!(q.is_empty());
        assert!(q.tightest().is_none());
    }

    #[test]
    fn codex_headers_work_too() {
        let q = from_headers(&headers(&[
            ("x-codex-primary-used-percent", "45"),
            ("x-codex-primary-reset-after-seconds", "86400"),
            ("x-codex-secondary-used-percent", "88"),
        ]));
        assert_eq!(q.windows.len(), 2);
        assert_eq!(q.tightest().unwrap().used_percent, 88.0);
        assert_eq!(q.windows[0].reset_in_secs, Some(86400));
    }

    #[test]
    fn a_codex_window_is_named_by_its_length_and_an_unused_one_is_left_out() {
        // Plus 账号实测的那组头：secondary 报 0 分钟，那个窗口没有启用
        let q = from_headers(&headers(&[
            ("x-codex-primary-used-percent", "21"),
            ("x-codex-primary-window-minutes", "10080"),
            ("x-codex-primary-reset-after-seconds", "410912"),
            ("x-codex-secondary-used-percent", "0"),
            ("x-codex-secondary-window-minutes", "0"),
            ("x-codex-secondary-reset-after-seconds", "0"),
        ]));
        assert_eq!(q.windows.len(), 1, "{q:?}");
        assert_eq!(q.windows[0].window, "weekly");
        assert_eq!(q.windows[0].used_percent, 21.0);
        assert!(!q.windows[0].rejected());

        let five = from_headers(&headers(&[
            ("x-codex-primary-used-percent", "1"),
            ("x-codex-primary-window-minutes", "300"),
        ]));
        assert_eq!(five.windows[0].window, "5h");
        assert_eq!(five.windows[0].used_percent, 1.0, "1% 不是 100%");
    }

    #[test]
    fn a_full_codex_window_is_rejected() {
        let q = from_headers(&headers(&[
            ("x-codex-primary-used-percent", "100"),
            ("x-codex-primary-window-minutes", "10080"),
        ]));
        assert!(q.windows[0].rejected());
    }

    #[test]
    fn the_tightest_window_is_the_one_the_menubar_shows() {
        let q = from_headers(&headers(&[
            ("anthropic-ratelimit-unified-5h-utilization", "20"),
            ("anthropic-ratelimit-unified-7d-utilization", "91"),
        ]));
        assert_eq!(q.tightest().unwrap().window, "7d");
    }

    #[test]
    fn an_upstream_warning_is_taken_at_face_value() {
        // **限流不再是突然发生的**。
        let q = from_headers(&headers(&[
            ("anthropic-ratelimit-unified-5h-utilization", "30"),
            ("anthropic-ratelimit-unified-5h-status", "allowed_warning"),
        ]));
        assert!(q.windows[0].warning(), "上游说快到了，我们却没当回事");
        assert!(!q.windows[0].rejected());
    }

    #[test]
    fn a_rejected_status_is_distinct_from_a_warning() {
        let q = from_headers(&headers(&[
            ("anthropic-ratelimit-unified-5h-utilization", "100"),
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
        ]));
        assert!(q.windows[0].rejected());
    }

    #[test]
    fn the_seven_day_threshold_header_turns_into_a_warning() {
        let q = from_headers(&headers(&[
            ("anthropic-ratelimit-unified-7d-utilization", "83"),
            ("anthropic-ratelimit-unified-7d-surpassed-threshold", "true"),
        ]));
        assert!(q.windows[0].warning());
    }

    #[test]
    fn a_reset_header_we_cannot_read_becomes_none_not_a_made_up_countdown() {
        // **编一个倒计时出来，用户会照着它安排自己的活。**
        let q = from_headers(&headers(&[
            ("anthropic-ratelimit-unified-5h-utilization", "50"),
            ("anthropic-ratelimit-unified-5h-reset", "不是时间"),
        ]));
        assert_eq!(q.windows[0].reset_in_secs, None);
    }

    #[test]
    fn an_rfc3339_reset_timestamp_is_turned_into_seconds() {
        let then = chrono::Utc::now() + chrono::Duration::seconds(3600);
        let q = from_headers(&headers(&[
            ("anthropic-ratelimit-unified-5h-utilization", "50"),
            ("anthropic-ratelimit-unified-5h-reset", &then.to_rfc3339()),
        ]));
        let s = q.windows[0].reset_in_secs.expect("时间戳没认出来");
        assert!((3590..=3600).contains(&s), "{s}");
    }

    #[test]
    fn a_percentage_out_of_range_is_clamped_rather_than_shown_as_is() {
        // 一个 150% 的进度条只会让人以为界面坏了。
        let q = from_headers(&headers(&[(
            "anthropic-ratelimit-unified-5h-utilization",
            "150",
        )]));
        assert_eq!(q.windows[0].used_percent, 100.0);
    }

    #[test]
    fn a_garbage_utilization_header_is_skipped_not_defaulted_to_zero() {
        let q = from_headers(&headers(&[(
            "anthropic-ratelimit-unified-5h-utilization",
            "unknown",
        )]));
        assert!(q.is_empty(), "读不懂的头产生了一个假窗口");
    }
}
