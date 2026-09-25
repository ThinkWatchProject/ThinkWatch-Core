//! GLM Coding Plan（Z.ai / BigModel）的套餐额度。
//!
//! **响应头里没有额度**，和 Anthropic、Codex 不一样：只能去问账号的额度接口
//! （`/api/monitor/usage/quota/limit`）。这个接口没有公开文档，形状照 Z.ai 官方的
//! 开源工具读出来的逻辑写。
//!
//! 问的纪律：
//! - **界面来要时问**，60 秒内的多次合成一次；**有请求去这一家时顺手问**，5 分钟最多一次。
//! - 失败了退避（30 秒、60 秒、120 秒、300 秒），不跟着界面的每一次刷新重试。
//! - 这把 key 没有开通套餐：记成「没有额度数据」，隔一小时才再问。刚才还有额度的 key
//!   要连着说两次才算（一次临时的 500 和「没有套餐」长得一样）。
//!
//! 额度用完时模型请求回 429，业务码在 body 里（[`exhausted`]）。**那一刻就记成用完**，
//! 不等下一次问额度。

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

use crate::quota::{Quota, Window};

/// 额度接口在两个站上的路径
const QUOTA_PATH: &str = "/api/monitor/usage/quota/limit";

/// 额度接口所在的两个站。**平时是它们的**，测试里换成本机的假服务器
#[derive(Debug, Clone)]
pub struct Sites {
    /// `https://api.z.ai`
    pub zai: String,
    /// `https://open.bigmodel.cn`
    pub bigmodel: String,
}

impl Default for Sites {
    fn default() -> Self {
        Self {
            zai: "https://api.z.ai".to_string(),
            bigmodel: "https://open.bigmodel.cn".to_string(),
        }
    }
}

impl Sites {
    /// 这个上游是 GLM Coding Plan 的话，它的额度接口地址。
    ///
    /// **只按主机认**：配置里没有专门的标记，登录生成的和手动添加的都只能从 `base_url`
    /// 看出来。Anthropic 格式（`/api/anthropic`）和 OpenAI 格式（`/api/coding/paas/v4`）
    /// 的地址都在同一个主机上，额度跟着 key 走，两种都算
    pub fn quota_url(&self, base_url: &str) -> Option<String> {
        let at = host_of(base_url)?;
        [&self.zai, &self.bigmodel]
            .into_iter()
            .find(|site| host_of(site).as_ref() == Some(&at))
            .map(|site| format!("{}{QUOTA_PATH}", site.trim_end_matches('/')))
    }
}

/// 主机（小写）和写明了的端口。**不看协议**：`http://api.z.ai` 也是那一家
fn host_of(url: &str) -> Option<(String, Option<u16>)> {
    let u = reqwest::Url::parse(url.trim()).ok()?;
    Some((u.host_str()?.to_ascii_lowercase(), u.port()))
}

// ---------------------------------------------------------------- 额度接口的回答

/// 问一次额度接口的结果。
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// 套餐额度，至少有一个认得出来的窗口
    Quota(Quota),
    /// 这把 key 没有开通套餐，或者回答里没有一个认得出来的窗口：**没有额度数据**
    NoPlan,
    /// 接口不认这把 key
    Rejected,
    /// 别的失败：5xx、读不懂、业务码不认识
    Failed,
}

/// 读额度接口的回答。`now_ms`：收到回答的时刻，用来筛掉说不通的重置时刻。
///
/// 成败只看 `success` 和 `code`。**`msg` 随语言变**（「当前用户不存在coding plan」），
/// 不能拿它判断
pub fn parse(status: u16, body: &[u8], now_ms: u64) -> Answer {
    // 鉴权失败多半是 200 里的业务码，但 HTTP 层的 401 / 403 也要认
    if status == 401 || status == 403 {
        return Answer::Rejected;
    }
    if !(200..300).contains(&status) {
        return Answer::Failed;
    }
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return Answer::Failed;
    };
    let code = v.get("code").and_then(code_of);
    let succeeded = v.get("success").and_then(Value::as_bool) != Some(false)
        && matches!(code, None | Some(0) | Some(200));
    if !succeeded {
        return match code {
            Some(401 | 1000 | 1001) => Answer::Rejected,
            // `{"code":500,"msg":"当前用户不存在coding plan","success":false}`
            Some(500) => Answer::NoPlan,
            _ => Answer::Failed,
        };
    }
    let windows = v["data"]["limits"]
        .as_array()
        .map(|xs| windows(xs, now_ms))
        .unwrap_or_default();
    if windows.is_empty() {
        Answer::NoPlan
    } else {
        Answer::Quota(Quota { windows })
    }
}

/// 业务码：数字，或者写成字符串的数字
fn code_of(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

const MINUTE_MS: u64 = 60_000;
const HOUR_MS: u64 = 60 * MINUTE_MS;
const DAY_MS: u64 = 24 * HOUR_MS;

/// 重置时刻可以比窗口长度晚这么多：两边的钟差一点，不该因此丢掉一个真实的时刻
const CLOCK_SLACK_MS: u64 = MINUTE_MS;

/// `limits` 里每一项是哪个窗口、那个窗口最长多久。
///
/// **按 `unit` / `number` 分，不按 `nextResetTime` 的先后猜**：周期末尾每周窗口会比
/// 5 小时窗口先重置，按先后猜就把两个标反了。认不出来的一项不要 —— 没有名字的百分比
/// 放上界面只会被当成另一个窗口。
fn window_of(item: &Value) -> Option<(&'static str, u64)> {
    let kind = item["type"].as_str().unwrap_or_default();
    // 5 小时和每周是 token（老套餐）或积分（积分制）；每月是老套餐的 MCP 调用次数
    let usage =
        kind.eq_ignore_ascii_case("TOKENS_LIMIT") || kind.eq_ignore_ascii_case("CREDIT_LIMIT");
    let calls = kind.eq_ignore_ascii_case("TIME_LIMIT");
    match (item["unit"].as_i64(), item["number"].as_i64()) {
        (Some(3), Some(5)) if usage => Some(("5h", 5 * HOUR_MS)),
        // 每周窗口的 `number` 见过 7 也见过 1，只认 `unit`
        (Some(6), _) if usage => Some(("weekly", 7 * DAY_MS)),
        (Some(5), Some(1)) if calls => Some(("monthly", 31 * DAY_MS)),
        _ => None,
    }
}

fn windows(items: &[Value], now_ms: u64) -> Vec<Window> {
    let mut out: Vec<Window> = Vec::new();
    for item in items {
        let Some((name, span)) = window_of(item) else {
            continue;
        };
        // 同一个窗口出现两次：留第一个
        if out.iter().any(|w| w.window == name) {
            continue;
        }
        let Some(used_percent) = item["percentage"].as_f64() else {
            continue;
        };
        let used_percent = used_percent.clamp(0.0, 100.0);
        let credits = match (
            item["usage"].as_f64(),
            item["currentValue"].as_f64(),
            item["remaining"].as_f64(),
        ) {
            (Some(total), Some(used), Some(remaining)) => Some(tw_api::QuotaCredits {
                total,
                used,
                remaining,
            }),
            _ => None,
        };
        let spent = used_percent >= 100.0 || credits.is_some_and(|c| c.remaining <= 0.0);
        out.push(Window {
            window: name.to_string(),
            used_percent,
            // 用量为 0 的 5 小时窗口可能没有 `nextResetTime`。**比窗口还长的不要**：有报告
            // 说 5 小时窗口的重置时刻落在 5 小时以后，照着显示就是一个错的倒计时
            resets_at_ms: item["nextResetTime"]
                .as_f64()
                .filter(|t| t.is_finite() && *t > 0.0)
                .map(|t| t as u64)
                .filter(|t| *t > now_ms && *t <= now_ms + span + CLOCK_SLACK_MS),
            // 用满就是被拒 —— 这是上游数字的直接结论，不是推断
            status: spent.then(|| "rejected".to_string()),
            credits,
        });
    }
    out
}

// ---------------------------------------------------------------- 429 里的额度用完

/// 一个 429 说的「额度用完了」。
#[derive(Debug, Clone, PartialEq)]
pub struct Exhausted {
    /// 上游的业务码
    pub code: i64,
    /// 哪个窗口。**业务码和消息都说不清时是 None**，由调用方按已知的额度挑
    pub window: Option<&'static str>,
    /// 消息里说的重置时刻。**以额度接口的为准**，那边没有才用它
    pub resets_at_ms: Option<u64>,
}

/// 读 429 的 body：是额度用完的话，是哪个窗口、什么时候重置。
///
/// 业务码在 Anthropic 端点上是 `error.type`（字符串），在 OpenAI 端点上是 `error.code`。
/// **算用完的只有这几个**：1308（5 小时）、1310（每周或每月）、1316–1321（团队超额）。
/// 套餐到期（1309）、公平使用限制（1313）、临时限流（1302、1305）都不是额度用完，
/// 记成用完会让界面报一个重置之后也好不了的「用完了」。
pub fn exhausted(body: &[u8]) -> Option<Exhausted> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let err = v.get("error")?;
    let code = [err.get("type"), err.get("code")]
        .into_iter()
        .flatten()
        .find_map(code_of)?;
    if !matches!(code, 1308 | 1310 | 1316..=1321) {
        return None;
    }
    let message = err["message"].as_str().unwrap_or_default();
    let window = window_in(message).or(match code {
        1308 => Some("5h"),
        _ => None,
    });
    Some(Exhausted {
        code,
        window,
        resets_at_ms: reset_in(message),
    })
}

/// 消息里说的是哪个窗口。中英文两种说法
fn window_in(message: &str) -> Option<&'static str> {
    let m = message.to_ascii_lowercase();
    if m.contains("5 hour") || m.contains("5-hour") || m.contains("5小时") || m.contains("5 小时")
    {
        Some("5h")
    } else if m.contains("week") || m.contains("周") {
        Some("weekly")
    } else if m.contains("month") || m.contains("月") {
        Some("monthly")
    } else {
        None
    }
}

/// 消息里的重置时刻：`2025-10-03 08:23:14`。**不带时区，是北京时间（UTC+8）**
fn reset_in(message: &str) -> Option<u64> {
    const LEN: usize = "2025-10-03 08:23:14".len();
    let bytes = message.as_bytes();
    let shape = |w: &[u8]| {
        w.iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            10 => *b == b' ',
            13 | 16 => *b == b':',
            _ => b.is_ascii_digit(),
        })
    };
    let at = (0..bytes.len().checked_sub(LEN - 1)?).find(|&i| shape(&bytes[i..i + LEN]))?;
    // 这一段全是 ASCII，切在哪儿都是字符边界
    let naive =
        chrono::NaiveDateTime::parse_from_str(&message[at..at + LEN], "%Y-%m-%d %H:%M:%S").ok()?;
    let beijing = chrono::FixedOffset::east_opt(8 * 3600)?;
    naive
        .and_local_timezone(beijing)
        .single()?
        .timestamp_millis()
        .try_into()
        .ok()
}

/// 业务码和消息都没说是哪个窗口时，按已知的额度挑：候选里用得最多的那个。
/// 一个都不知道就是每周 —— 1310 说的是「每周或每月」，新套餐只有每周
pub fn guess_window(code: i64, known: &Quota) -> String {
    let candidates: &[&str] = match code {
        1310 => &["weekly", "monthly"],
        _ => &["5h", "weekly", "monthly"],
    };
    known
        .windows
        .iter()
        .filter(|w| candidates.contains(&w.window.as_str()))
        .max_by(|a, b| a.used_percent.total_cmp(&b.used_percent))
        .map(|w| w.window.clone())
        .unwrap_or_else(|| "weekly".to_string())
}

// ---------------------------------------------------------------- 什么时候问

/// 为什么去问。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// 界面来要
    Demand,
    /// 有请求去了这一家
    Traffic,
}

/// 界面来要时，这么久之内问过就不再问
const DEMAND_EVERY_MS: u64 = MINUTE_MS;
/// 有请求时，这么久最多问一次
const TRAFFIC_EVERY_MS: u64 = 5 * MINUTE_MS;
/// 连续失败时，第 n 次之后等多久
const BACKOFF_MS: [u64; 4] = [30_000, 60_000, 120_000, 300_000];
/// 没有开通套餐的 key 隔多久再问一次。**不是再也不问**：用户可能刚买了套餐
const NO_PLAN_RECHECK_MS: u64 = HOUR_MS;

/// 一个上游问额度的节奏。
#[derive(Debug, Default, Clone)]
struct Slot {
    /// 凭据的指纹。**换了 key 就从头来**：退避和「没有套餐」说的是旧的那把
    ident: String,
    /// 上一次问完的时刻，不论结果
    asked_at: Option<u64>,
    /// 连续失败了几次
    failures: u32,
    /// 这之前不问：退避中，或者没有套餐
    quiet_until: u64,
    /// 正在问。**同一时刻只问一次**
    running: bool,
    /// 上一次问来的是额度
    had_quota: bool,
}

impl Slot {
    fn due(&self, why: Why, now_ms: u64) -> bool {
        if self.running || now_ms < self.quiet_until {
            return false;
        }
        let every = match why {
            Why::Demand => DEMAND_EVERY_MS,
            Why::Traffic => TRAFFIC_EVERY_MS,
        };
        self.asked_at
            .is_none_or(|t| now_ms.saturating_sub(t) >= every)
    }

    /// 记下这一次的结果，交回该按哪个结论办。
    ///
    /// **刚才还有额度的 key 头一次说「没有套餐」，当成一次失败**：「没有套餐」认的是
    /// 业务码 500，而一次临时的 500 长得一模一样。照「没有套餐」办要清掉额度、一小时
    /// 不再问；当成失败只是按退避过一会儿再问。连着第二次还这么说，才是真没有了
    fn settle(&mut self, answer: Answer, now_ms: u64) -> Answer {
        let answer = match answer {
            Answer::NoPlan if self.had_quota => Answer::Failed,
            a => a,
        };
        self.had_quota = matches!(answer, Answer::Quota(_));
        self.running = false;
        self.asked_at = Some(now_ms);
        match &answer {
            Answer::Quota(_) => {
                self.failures = 0;
                self.quiet_until = 0;
            }
            Answer::NoPlan => {
                self.failures = 0;
                self.quiet_until = now_ms + NO_PLAN_RECHECK_MS;
            }
            Answer::Rejected | Answer::Failed => {
                self.failures = self.failures.saturating_add(1);
                let i = (self.failures as usize - 1).min(BACKOFF_MS.len() - 1);
                self.quiet_until = now_ms + BACKOFF_MS[i];
            }
        }
        answer
    }
}

/// 每个 GLM 上游问额度的节奏，和额度接口在哪。**跨重载存活**：改一条规则不该让
/// 所有上游的退避清零
#[derive(Default)]
pub struct Tracker {
    sites: Mutex<Sites>,
    slots: Mutex<HashMap<String, Slot>>,
}

impl Tracker {
    pub fn sites(&self) -> Sites {
        self.sites.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn set_sites(&self, sites: Sites) {
        if let Ok(mut g) = self.sites.lock() {
            *g = sites;
        }
    }

    /// 该问这一家了吗。该问就占上，问完必须 [`Tracker::settle`]
    pub fn claim(&self, provider: &str, ident: &str, why: Why, now_ms: u64) -> bool {
        let Ok(mut g) = self.slots.lock() else {
            return false;
        };
        let slot = g.entry(provider.to_string()).or_default();
        if slot.ident != ident {
            *slot = Slot {
                ident: ident.to_string(),
                ..Default::default()
            };
        }
        let due = slot.due(why, now_ms);
        if due {
            slot.running = true;
        }
        due
    }

    /// 问完了：记下节奏，交回该按哪个结论办（见 `Slot::settle`）。凭据在这中间换了
    /// 的话，这个结果说的是旧的 key，不记也不办，是 None
    pub fn settle(
        &self,
        provider: &str,
        ident: &str,
        answer: Answer,
        now_ms: u64,
    ) -> Option<Answer> {
        let mut g = self.slots.lock().ok()?;
        let slot = g.get_mut(provider).filter(|s| s.ident == ident)?;
        Some(slot.settle(answer, now_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 收到回答的时刻：真实示例里 5 小时窗口重置前约 1 小时
    const NOW: u64 = 1_790_030_000_000;

    fn quota(body: &str) -> Quota {
        match parse(200, body.as_bytes(), NOW) {
            Answer::Quota(q) => q,
            other => panic!("{other:?}"),
        }
    }

    fn window<'a>(q: &'a Quota, name: &str) -> &'a Window {
        q.windows
            .iter()
            .find(|w| w.window == name)
            .unwrap_or_else(|| panic!("no {name} in {q:?}"))
    }

    #[test]
    fn the_hosts_of_both_sites_are_glm_whatever_the_path() {
        let s = Sites::default();
        assert_eq!(
            s.quota_url("https://api.z.ai/api/anthropic").as_deref(),
            Some("https://api.z.ai/api/monitor/usage/quota/limit")
        );
        assert_eq!(
            s.quota_url("https://open.bigmodel.cn/api/coding/paas/v4")
                .as_deref(),
            Some("https://open.bigmodel.cn/api/monitor/usage/quota/limit")
        );
        // 手写的地址大小写不一、末尾带斜杠，一样认
        assert!(s.quota_url("https://API.Z.AI/api/anthropic/").is_some());
        assert!(s.quota_url("https://api.anthropic.com").is_none());
        // 只是名字里含着的不算
        assert!(
            s.quota_url("https://api.z.ai.example.com/api/anthropic")
                .is_none()
        );
        assert!(s.quota_url("https://bigmodel.cn.example.com").is_none());
        assert!(s.quota_url("不是地址").is_none());
    }

    #[test]
    fn a_test_site_is_told_apart_by_its_port() {
        let s = Sites {
            zai: "http://127.0.0.1:4000".into(),
            bigmodel: "http://127.0.0.1:4001".into(),
        };
        assert_eq!(
            s.quota_url("http://127.0.0.1:4001/api/anthropic")
                .as_deref(),
            Some("http://127.0.0.1:4001/api/monitor/usage/quota/limit")
        );
        assert!(s.quota_url("http://127.0.0.1:4002/api/anthropic").is_none());
    }

    /// 积分制 Lite 套餐的真实回答
    #[test]
    fn the_credit_plan_example_reads_as_two_windows_with_credits() {
        let q = quota(
            r#"{"code":200,"msg":"Operation successful","data":{"limits":[{"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":2000,"currentValue":23,"remaining":1976,"percentage":1,"nextResetTime":1790033645897},{"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":10000,"currentValue":268,"remaining":9731,"percentage":2,"nextResetTime":1790292019984}],"level":"lite"},"success":true}"#,
        );
        assert_eq!(q.windows.len(), 2);
        let five = window(&q, "5h");
        assert_eq!(five.used_percent, 1.0);
        assert_eq!(five.resets_at_ms, Some(1_790_033_645_897));
        assert_eq!(five.status, None);
        // **剩余是上游说的剩余**，不是 2000 − 23
        assert_eq!(
            five.credits,
            Some(tw_api::QuotaCredits {
                total: 2000.0,
                used: 23.0,
                remaining: 1976.0
            })
        );
        let week = window(&q, "weekly");
        assert_eq!(week.used_percent, 2.0);
        assert_eq!(week.resets_at_ms, Some(1_790_292_019_984));
        assert_eq!(week.credits.map(|c| c.remaining), Some(9731.0));
    }

    /// 老套餐 V1：一个 5 小时的 token 窗口，加每月的 MCP 调用次数。
    /// 每月窗口放在前面，它先重置 —— **不按重置先后猜**
    #[test]
    fn an_old_v1_plan_has_five_hours_and_monthly_calls() {
        let q = quota(&format!(
            r#"{{"code":200,"success":true,"data":{{"level":"pro","limits":[
                {{"type":"TIME_LIMIT","unit":5,"number":1,"usage":1000,"currentValue":40,"remaining":960,"percentage":4,"nextResetTime":{m}}},
                {{"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":37,"nextResetTime":{h}}}
            ]}}}}"#,
            m = NOW + 10 * MINUTE_MS,
            h = NOW + 3 * HOUR_MS,
        ));
        assert_eq!(q.windows.len(), 2);
        assert_eq!(window(&q, "5h").used_percent, 37.0);
        assert_eq!(window(&q, "5h").credits, None);
        let month = window(&q, "monthly");
        assert_eq!(month.used_percent, 4.0);
        assert_eq!(month.resets_at_ms, Some(NOW + 10 * MINUTE_MS));
    }

    /// 老套餐 V2：多了每周的 token 窗口，`number` 是 7
    #[test]
    fn an_old_v2_plan_adds_a_weekly_window() {
        let q = quota(&format!(
            r#"{{"code":0,"data":{{"limits":[
                {{"type":"TOKENS_LIMIT","unit":6,"number":7,"percentage":81,"nextResetTime":{w}}},
                {{"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":100,"nextResetTime":{h}}},
                {{"type":"TIME_LIMIT","unit":5,"number":1,"percentage":0}}
            ]}}}}"#,
            w = NOW + 2 * HOUR_MS,
            h = NOW + 4 * HOUR_MS,
        ));
        let names: Vec<_> = q.windows.iter().map(|w| w.window.as_str()).collect();
        assert_eq!(names, ["weekly", "5h", "monthly"]);
        assert_eq!(window(&q, "weekly").used_percent, 81.0);
        // 用满就是被拒
        assert_eq!(window(&q, "5h").status.as_deref(), Some("rejected"));
        assert_eq!(window(&q, "monthly").resets_at_ms, None);
    }

    #[test]
    fn a_key_without_a_plan_has_no_quota_data() {
        assert_eq!(
            parse(
                200,
                r#"{"code":500,"msg":"当前用户不存在coding plan","success":false}"#.as_bytes(),
                NOW
            ),
            Answer::NoPlan
        );
        // 英文的同一句也一样：**不看 msg**
        assert_eq!(
            parse(
                200,
                br#"{"code":500,"msg":"No coding plan for the current user","success":false}"#,
                NOW
            ),
            Answer::NoPlan
        );
        // 成功了却一个窗口都没有，也是没有额度数据，不是「用了 0%」
        assert_eq!(
            parse(
                200,
                br#"{"code":200,"success":true,"data":{"limits":[]}}"#,
                NOW
            ),
            Answer::NoPlan
        );
    }

    #[test]
    fn an_auth_failure_is_told_apart_from_other_failures() {
        for body in [
            r#"{"code":401,"msg":"令牌已过期或验证不正确","success":false}"#,
            r#"{"code":1000,"msg":"Authentication failed","success":false}"#,
            r#"{"code":"1001","msg":"Header中未收到Authorization参数","success":false}"#,
            // 业务码对了、`success` 没给也算失败
            r#"{"code":1000,"msg":"x"}"#,
        ] {
            assert_eq!(parse(200, body.as_bytes(), NOW), Answer::Rejected, "{body}");
        }
        assert_eq!(parse(401, b"", NOW), Answer::Rejected);
        assert_eq!(parse(502, b"<html>", NOW), Answer::Failed);
        assert_eq!(parse(200, b"<html>", NOW), Answer::Failed);
        assert_eq!(
            parse(200, br#"{"code":1234,"success":false}"#, NOW),
            Answer::Failed
        );
        // `success: false` 自己就是失败，哪怕业务码是 200
        assert_eq!(
            parse(200, br#"{"code":200,"success":false}"#, NOW),
            Answer::Failed
        );
    }

    #[test]
    fn a_missing_reset_time_is_none_not_made_up() {
        // 用量为 0 的 5 小时窗口可能没有 `nextResetTime`
        let q = quota(
            r#"{"success":true,"data":{"limits":[{"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":0}]}}"#,
        );
        assert_eq!(window(&q, "5h").resets_at_ms, None);
        assert_eq!(window(&q, "5h").used_percent, 0.0);
    }

    #[test]
    fn a_reset_time_that_cannot_be_right_is_dropped() {
        let q = quota(&format!(
            r#"{{"success":true,"data":{{"limits":[
                {{"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":20,"nextResetTime":{late}}},
                {{"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":20,"nextResetTime":{past}}},
                {{"type":"TIME_LIMIT","unit":5,"number":1,"percentage":20,"nextResetTime":"明天"}}
            ]}}}}"#,
            // 5 小时窗口却在 6 小时后重置
            late = NOW + 6 * HOUR_MS,
            // 已经过去了
            past = NOW - MINUTE_MS,
        ));
        assert!(q.windows.iter().all(|w| w.resets_at_ms.is_none()), "{q:?}");
        // 差一点的钟不该让一个真实的时刻被丢掉
        let q = quota(&format!(
            r#"{{"success":true,"data":{{"limits":[{{"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":20,"nextResetTime":{t}}}]}}}}"#,
            t = NOW + 5 * HOUR_MS + 30_000,
        ));
        assert_eq!(
            window(&q, "5h").resets_at_ms,
            Some(NOW + 5 * HOUR_MS + 30_000)
        );
    }

    #[test]
    fn a_window_it_cannot_name_is_left_out() {
        let q = quota(
            r#"{"success":true,"data":{"limits":[
                {"type":"TOKENS_LIMIT","percentage":50},
                {"type":"TOKENS_LIMIT","unit":4,"number":1,"percentage":50},
                {"type":"TIME_LIMIT","unit":3,"number":5,"percentage":50},
                {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":150}
            ]}}"#,
        );
        assert_eq!(q.windows.len(), 1, "{q:?}");
        assert_eq!(window(&q, "5h").used_percent, 100.0, "超过 100 的按 100");
    }

    #[test]
    fn credits_used_up_are_rejected_even_below_a_hundred_percent() {
        let q = quota(
            r#"{"success":true,"data":{"limits":[{"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":10000,"currentValue":10000,"remaining":0,"percentage":99}]}}"#,
        );
        assert_eq!(window(&q, "weekly").status.as_deref(), Some("rejected"));
    }

    // ------------------------------------------------------------ 429

    #[test]
    fn a_five_hour_limit_on_the_anthropic_endpoint() {
        let e = exhausted(
            br#"{"type":"error","error":{"type":"1308","message":"Usage limit reached for 5 hour. Your limit will reset at 2025-10-03 08:23:14"}}"#,
        )
        .unwrap();
        assert_eq!(e.code, 1308);
        assert_eq!(e.window, Some("5h"));
        // 北京时间 08:23:14 是 UTC 00:23:14
        let utc = chrono::DateTime::parse_from_rfc3339("2025-10-03T00:23:14Z").unwrap();
        assert_eq!(e.resets_at_ms, Some(utc.timestamp_millis() as u64));
    }

    #[test]
    fn a_weekly_limit_on_the_openai_endpoint_in_chinese() {
        let e = exhausted(
            r#"{"error":{"code":"1310","message":"已达到每周使用上限。您的限额将在 2025-10-06 10:00:00 重置。"}}"#
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(e.code, 1310);
        assert_eq!(e.window, Some("weekly"));
        let utc = chrono::DateTime::parse_from_rfc3339("2025-10-06T02:00:00Z").unwrap();
        assert_eq!(e.resets_at_ms, Some(utc.timestamp_millis() as u64));
    }

    #[test]
    fn the_chinese_five_hour_message_and_a_numeric_code() {
        let e = exhausted(
            r#"{"error":{"code":1308,"message":"已达到 5 小时的使用上限。您的限额将在 2025-10-03 08:23:14 重置。"}}"#
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(e.window, Some("5h"));
        assert!(e.resets_at_ms.is_some());
    }

    #[test]
    fn monthly_and_team_limits_count_too() {
        let e = exhausted(
            br#"{"type":"error","error":{"type":"1310","message":"Monthly limit exhausted."}}"#,
        )
        .unwrap();
        assert_eq!(e.window, Some("monthly"));
        assert_eq!(e.resets_at_ms, None, "消息里没有时刻就没有");
        for code in 1316..=1321 {
            let body =
                format!(r#"{{"error":{{"code":"{code}","message":"Team quota exceeded"}}}}"#);
            let e = exhausted(body.as_bytes()).unwrap_or_else(|| panic!("{code}"));
            assert_eq!(e.window, None, "{code}：说不清是哪个窗口");
        }
    }

    #[test]
    fn other_429_codes_are_not_a_used_up_quota() {
        // 套餐到期、公平使用限制、临时限流：**重置之后也好不了，或者一会儿就好**
        for code in ["1309", "1313", "1302", "1305", "1311"] {
            let body = format!(
                r#"{{"type":"error","error":{{"type":"{code}","message":"Usage limit reached for 5 hour."}}}}"#
            );
            assert_eq!(exhausted(body.as_bytes()), None, "{code}");
        }
        // 业务码之外的 429：普通的限流
        assert_eq!(
            exhausted(
                br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
            ),
            None
        );
        assert_eq!(exhausted(b"Too Many Requests"), None);
    }

    #[test]
    fn an_unnamed_window_is_the_tightest_known_candidate() {
        let known = Quota {
            windows: vec![w("5h", 100.0), w("weekly", 40.0), w("monthly", 90.0)],
        };
        // 1310 说的是每周或每月，不会是 5 小时
        assert_eq!(guess_window(1310, &known), "monthly");
        assert_eq!(guess_window(1318, &known), "5h");
        assert_eq!(guess_window(1310, &Quota::default()), "weekly");
    }

    fn w(name: &str, used: f64) -> Window {
        Window {
            window: name.into(),
            used_percent: used,
            resets_at_ms: None,
            status: None,
            credits: None,
        }
    }

    // ------------------------------------------------------------ 节奏

    #[test]
    fn demands_within_a_minute_are_asked_once() {
        let t = Tracker::default();
        assert!(t.claim("glm", "k", Why::Demand, NOW));
        // 正在问：再来的不另问
        assert!(!t.claim("glm", "k", Why::Demand, NOW + 1));
        t.settle("glm", "k", Answer::Quota(Quota::default()), NOW + 100);
        assert!(!t.claim("glm", "k", Why::Demand, NOW + 30_000));
        assert!(t.claim("glm", "k", Why::Demand, NOW + 100 + MINUTE_MS));
    }

    #[test]
    fn traffic_asks_at_most_every_five_minutes() {
        let t = Tracker::default();
        assert!(t.claim("glm", "k", Why::Traffic, NOW));
        t.settle("glm", "k", Answer::Quota(Quota::default()), NOW);
        assert!(!t.claim("glm", "k", Why::Traffic, NOW + 4 * MINUTE_MS));
        // 界面来要的照样按一分钟算
        assert!(t.claim("glm", "k", Why::Demand, NOW + 2 * MINUTE_MS));
        t.settle(
            "glm",
            "k",
            Answer::Quota(Quota::default()),
            NOW + 2 * MINUTE_MS,
        );
        assert!(!t.claim("glm", "k", Why::Traffic, NOW + 6 * MINUTE_MS));
        assert!(t.claim("glm", "k", Why::Traffic, NOW + 7 * MINUTE_MS));
    }

    #[test]
    fn failures_back_off_30_60_120_300_seconds() {
        let t = Tracker::default();
        let mut now = NOW;
        for wait in [30_000, 60_000, 120_000, 300_000, 300_000] {
            assert!(t.claim("glm", "k", Why::Demand, now), "at {}", now - NOW);
            t.settle("glm", "k", Answer::Failed, now);
            // 界面每分钟来要一次也不提前
            assert!(!t.claim("glm", "k", Why::Demand, now + wait - 1));
            now += wait.max(DEMAND_EVERY_MS);
        }
        // 成功一次，退避清零
        assert!(t.claim("glm", "k", Why::Demand, now));
        t.settle("glm", "k", Answer::Quota(Quota::default()), now);
        assert!(t.claim("glm", "k", Why::Demand, now + DEMAND_EVERY_MS));
        t.settle("glm", "k", Answer::Rejected, now + DEMAND_EVERY_MS);
        assert!(t.claim("glm", "k", Why::Demand, now + 2 * DEMAND_EVERY_MS));
    }

    #[test]
    fn a_key_without_a_plan_is_asked_again_only_after_an_hour_or_a_new_key() {
        let t = Tracker::default();
        assert!(t.claim("glm", "k", Why::Demand, NOW));
        t.settle("glm", "k", Answer::NoPlan, NOW);
        assert!(!t.claim("glm", "k", Why::Demand, NOW + 30 * MINUTE_MS));
        assert!(!t.claim("glm", "k", Why::Traffic, NOW + 30 * MINUTE_MS));
        // 换了 key：之前的结论说的是旧的那把
        assert!(t.claim("glm", "k2", Why::Demand, NOW + 30 * MINUTE_MS));
        t.settle("glm", "k2", Answer::NoPlan, NOW + 30 * MINUTE_MS);
        assert!(t.claim("glm", "k2", Why::Demand, NOW + 90 * MINUTE_MS));
    }

    /// 刚才还有额度的 key 头一次说「没有套餐」：多半是一次临时的 500，当成失败，
    /// 过一会儿再问；连着第二次才信
    #[test]
    fn a_key_that_had_a_plan_must_say_no_plan_twice() {
        let t = Tracker::default();
        let q = Answer::Quota(Quota::default());
        assert!(t.claim("glm", "k", Why::Demand, NOW));
        assert_eq!(t.settle("glm", "k", q.clone(), NOW), Some(q));
        assert!(t.claim("glm", "k", Why::Demand, NOW + MINUTE_MS));
        assert_eq!(
            t.settle("glm", "k", Answer::NoPlan, NOW + MINUTE_MS),
            Some(Answer::Failed)
        );
        let later = NOW + 2 * MINUTE_MS;
        assert!(t.claim("glm", "k", Why::Demand, later));
        assert_eq!(
            t.settle("glm", "k", Answer::NoPlan, later),
            Some(Answer::NoPlan)
        );
        assert!(!t.claim("glm", "k", Why::Demand, later + 30 * MINUTE_MS));
    }

    #[test]
    fn an_answer_for_a_replaced_key_is_not_kept() {
        let t = Tracker::default();
        assert!(t.claim("glm", "old", Why::Demand, NOW));
        assert!(t.claim("glm", "new", Why::Demand, NOW));
        t.settle("glm", "old", Answer::NoPlan, NOW);
        // 新 key 还在问，旧的结论没把它记成「没有套餐」
        t.settle("glm", "new", Answer::Quota(Quota::default()), NOW);
        assert!(t.claim("glm", "new", Why::Demand, NOW + DEMAND_EVERY_MS));
    }
}
