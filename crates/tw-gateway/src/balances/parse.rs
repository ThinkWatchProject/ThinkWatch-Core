//! 读懂各家余额接口的回答。**只看 JSON，不联网**：取数在 [`super`]，这里每一种回答一个
//! 小函数，读不懂的是 `None`（[`super::Failure::Unrecognized`]）。
//!
//! 一条贯穿这里的纪律和订阅额度一样：**每个数都是上游给的**。上游没给的不补成 0 ——
//! 「余额 $0」和「不知道余额」是两句话，前者会让人去充值。

use serde_json::Value;
use tw_api::{BalanceQuota, BalanceScope, BalanceWindow, Money, Spent, SpentPeriod};

/// 一次读到的余额：[`tw_api::Balance`] 里从上游来的那几样。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reading {
    pub wallet: Option<Money>,
    pub quota: Option<BalanceQuota>,
    pub windows: Vec<BalanceWindow>,
    pub expires_at_ms: Option<u64>,
    pub spent: Option<Spent>,
}

const USD: &str = "USD";

/// 一个数：JSON 的数，或者写成字符串的数（DeepSeek 的余额就是字符串）。**不是有限的数不要**
fn number(v: &Value) -> Option<f64> {
    let n = match v {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    n.is_finite().then_some(n)
}

/// 一个时刻：RFC 3339，或者 Unix 时间戳（秒或毫秒，按大小分）。0 和负数是「没有」
fn moment(v: &Value) -> Option<u64> {
    match v {
        Value::String(s) if !s.trim().is_empty() => {
            let s = s.trim();
            if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
                return u64::try_from(t.timestamp_millis()).ok().filter(|t| *t > 0);
            }
            number(v).and_then(stamp)
        }
        Value::Number(_) => number(v).and_then(stamp),
        _ => None,
    }
}

/// 数字的时间戳：比这个大的是毫秒，否则是秒。近年的秒数在十亿上下，毫秒在一万亿以上
fn stamp(n: f64) -> Option<u64> {
    if n <= 0.0 {
        return None;
    }
    let ms = if n >= 1e11 { n } else { n * 1000.0 };
    Some(ms as u64)
}

// ---------------------------------------------------------------- OpenRouter

/// `GET /api/v1/key`：`{"data":{"limit":10,"limit_remaining":7.5,"limit_reset":"monthly",
/// "usage":42.25,"usage_daily":0.5,"usage_monthly":2.5,"expires_at":null,…}}`，美元。
///
/// - 设了额度（`limit` 不是 null）是总额度。**已用按额度减剩余算**：设了按天、按月重置的
///   额度时，`usage` 是这把密钥从来用过的总数，`limit_remaining` 才是这一期还剩多少。没给
///   剩余时才用 `usage`；
/// - 花了多少：这个月的（`usage_monthly`），没给就是从来一共的（`usage`）；
/// - `expires_at` 是到期时刻。
///
/// **没有钱包**：账户余额只有管理密钥问得到，能转发请求的密钥看不到
pub fn openrouter(v: &Value) -> Option<Reading> {
    let data = v.get("data")?.as_object()?;
    let usage = data.get("usage").and_then(number);
    let quota = match data.get("limit") {
        None | Some(Value::Null) => None,
        Some(l) => {
            let limit = number(l)?;
            let used = match (data.get("limit_remaining").and_then(number), usage) {
                (Some(remaining), _) => (limit - remaining).max(0.0),
                (None, Some(usage)) => usage,
                (None, None) => return None,
            };
            Some(BalanceQuota {
                limit,
                used,
                unit: USD.to_string(),
            })
        }
    };
    let spent = match data.get("usage_monthly").and_then(number) {
        Some(month) => Some((month, SpentPeriod::Month)),
        None => usage.map(|total| (total, SpentPeriod::Total)),
    }
    .map(|(amount, period)| Spent {
        amount,
        currency: USD.to_string(),
        period,
        scope: None,
    });
    // 额度和花费都说不出来的，不是这个接口的回答
    if quota.is_none() && spent.is_none() {
        return None;
    }
    Some(Reading {
        quota,
        spent,
        expires_at_ms: data.get("expires_at").and_then(moment),
        ..Default::default()
    })
}

// ---------------------------------------------------------------- DeepSeek

/// `GET /user/balance`：`{"is_available":true,"balance_infos":[{"currency":"CNY",
/// "total_balance":"110.00",…}]}`。
///
/// 每种货币一项。**显示第一个有钱的**，都没钱就是第一个：充的是人民币的账户，美元那一项
/// 是 0，显示它就成了「余额 $0」
pub fn deepseek(v: &Value) -> Option<Reading> {
    let infos = v.get("balance_infos")?.as_array()?;
    let wallets: Vec<Money> = infos
        .iter()
        .filter_map(|i| {
            Some(Money {
                amount: number(i.get("total_balance")?)?,
                currency: i.get("currency")?.as_str()?.trim().to_string(),
            })
        })
        .collect();
    if wallets.is_empty() && !infos.is_empty() {
        return None;
    }
    let wallet = wallets
        .iter()
        .find(|w| w.amount > 0.0)
        .or(wallets.first())
        .cloned();
    Some(Reading {
        wallet,
        ..Default::default()
    })
}

// ---------------------------------------------------------------- Moonshot

/// `GET /v1/users/me/balance`：`{"code":0,"data":{"available_balance":49.58,…},"status":true}`。
///
/// 回答里不写货币：`currency` 由调用方按站点给（`.cn` 是人民币，`.ai` 是美元）
pub fn moonshot(v: &Value, currency: &str) -> Option<Reading> {
    let amount = number(v.get("data")?.get("available_balance")?)?;
    Some(Reading {
        wallet: Some(Money {
            amount,
            currency: currency.to_string(),
        }),
        ..Default::default()
    })
}

// ---------------------------------------------------------------- Sub2API

/// 这是 Sub2API 的用量接口吗：顶层有一个字符串的 `mode`
pub fn is_sub2api(v: &Value) -> bool {
    v.get("mode").is_some_and(Value::is_string)
}

/// `GET /v1/usage`，两种模式：
///
/// - `quota_limited`：密钥有总额度（`quota{limit,used}`）和按时间窗口的限额
///   （`rate_limits[]{window,limit,used,reset_at}`），美元；
/// - `unrestricted`：订阅的日、周、月限额（`subscription{daily_usage_usd,daily_limit_usd,…}`），
///   或者钱包余额（`balance`）。
///
/// 金额的单位看顶层的 `unit`，没写是美元。`expires_at` 是到期时刻。用量明细里只取今天
/// 花了多少：**实际扣的**（`usage.today.actual_cost`，按分组倍率算过的），没给才用按标价
/// 算的 `cost`；别的（`daily_usage`、`model_stats`）不要
pub fn sub2api(v: &Value) -> Option<Reading> {
    let mode = v.get("mode")?.as_str()?;
    let unit = v
        .get("unit")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .unwrap_or(USD)
        .to_string();
    let mut r = Reading {
        expires_at_ms: v
            .get("expires_at")
            .and_then(moment)
            .or_else(|| v.get("subscription")?.get("expires_at").and_then(moment)),
        spent: v
            .get("usage")
            .and_then(|u| u.get("today"))
            .and_then(|t| {
                t.get("actual_cost")
                    .and_then(number)
                    .or_else(|| t.get("cost").and_then(number))
            })
            .map(|amount| Spent {
                amount,
                currency: unit.clone(),
                period: SpentPeriod::Today,
                scope: None,
            }),
        ..Default::default()
    };
    if mode == "quota_limited" {
        r.quota = v.get("quota").and_then(|q| {
            Some(BalanceQuota {
                limit: number(q.get("limit")?)?,
                used: q.get("used").and_then(number).unwrap_or(0.0),
                unit: unit.clone(),
            })
        });
        r.windows = v
            .get("rate_limits")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|w| {
                Some(BalanceWindow {
                    window: w.get("window")?.as_str()?.trim().to_string(),
                    limit: number(w.get("limit")?)?,
                    used: w.get("used").and_then(number).unwrap_or(0.0),
                    unit: unit.clone(),
                    resets_at_ms: w.get("reset_at").and_then(moment),
                    scope: None,
                })
            })
            .filter(|w| !w.window.is_empty())
            .collect();
        return Some(r);
    }
    if let Some(sub) = v.get("subscription").filter(|s| s.is_object()) {
        for name in ["daily", "weekly", "monthly"] {
            // 限额没写、写了 0 的窗口不限，不是「额度为 0」
            let Some(limit) = sub
                .get(format!("{name}_limit_usd"))
                .and_then(number)
                .filter(|l| *l > 0.0)
            else {
                continue;
            };
            r.windows.push(BalanceWindow {
                window: name.to_string(),
                limit,
                used: sub
                    .get(format!("{name}_usage_usd"))
                    .and_then(number)
                    .unwrap_or(0.0),
                unit: USD.to_string(),
                resets_at_ms: None,
                scope: None,
            });
        }
    }
    r.wallet = v.get("balance").and_then(number).map(|amount| Money {
        amount,
        currency: unit,
    });
    Some(r)
}

// ---------------------------------------------------------------- New API / One API

/// 「不限额度」的密钥报的总额度就是这个数（New API 和 One API 都写死成它）
const UNLIMITED: f64 = 100_000_000.0;

/// 数是按 token 计的（New API 站点的额度按 token 显示）
pub const TOKENS: &str = "tokens";
/// 说不清数是按什么计的：站点没说它的额度怎么显示
pub const UNKNOWN: &str = "unknown";

/// 这是 New API / One API 的账单接口吗：有 `hard_limit_usd` 或 `system_hard_limit_usd`
pub fn is_newapi(v: &Value) -> bool {
    hard_limit(v).is_some()
}

fn hard_limit(v: &Value) -> Option<f64> {
    v.get("hard_limit_usd")
        .and_then(number)
        .or_else(|| v.get("system_hard_limit_usd").and_then(number))
}

/// New API / One API 账单接口里的数是按什么计的，看站点的状态接口（`GET /api/status`，
/// 公开的）：`{"success":true,"data":{"quota_display_type":"CNY","display_in_currency":true,…}}`。
///
/// 账单接口**按站点的额度显示方式折算**，字段名里的 `usd` 不作数：
/// - `quota_display_type`（New API 新版）：`USD` 是美元；`CNY` 是人民币；`TOKENS` 是 token；
///   `CUSTOM`（自定义货币）在账单接口里仍按美元折算，是美元；
/// - 没有它的（New API 旧版、One API）看 `display_in_currency`：是 true 按美元折算，
///   false 是 token；
/// - 都说不出来的是 [`UNKNOWN`]，**不当作美元**：猜错了，一个 token 数就成了一笔钱
pub fn newapi_unit(status: &Value) -> &'static str {
    let Some(data) = status.get("data") else {
        return UNKNOWN;
    };
    let in_currency = data.get("display_in_currency").and_then(Value::as_bool);
    match data.get("quota_display_type").and_then(Value::as_str) {
        Some("USD" | "CUSTOM") => USD,
        Some("CNY") => "CNY",
        Some("TOKENS") => TOKENS,
        // 不认识的显示方式：只说得出是不是 token
        Some(_) => match in_currency {
            Some(false) => TOKENS,
            _ => UNKNOWN,
        },
        None => match in_currency {
            Some(true) => USD,
            Some(false) => TOKENS,
            None => UNKNOWN,
        },
    }
}

/// `GET /v1/dashboard/billing/subscription`（总额度 `hard_limit_usd`；到期 `access_until`，
/// Unix 秒，0 是不到期）加 `GET /v1/dashboard/billing/usage`（已用 `total_usage`，**乘过
/// 100**，美元时是美分）。单位 `unit` 是 [`newapi_unit`] 问出来的，两个接口一样。
///
/// 不限额度的密钥报的总额度是 1 亿，没有额度可言，只说得出花了多少。按钱计的，1 亿以上
/// 都不会是真的额度；**按 token 计（或者不知道按什么计）的只认正好 1 亿**：比它大的
/// token 额度是真的
pub fn newapi(subscription: &Value, usage: &Value, unit: &str) -> Option<Reading> {
    let limit = hard_limit(subscription)?;
    let used = number(usage.get("total_usage")?)? / 100.0;
    let mut r = Reading {
        expires_at_ms: subscription.get("access_until").and_then(moment),
        ..Default::default()
    };
    let unlimited = match unit {
        TOKENS | UNKNOWN => limit == UNLIMITED,
        _ => limit >= UNLIMITED,
    };
    if unlimited {
        r.spent = Some(Spent {
            amount: used,
            currency: unit.to_string(),
            period: SpentPeriod::Total,
            scope: None,
        });
    } else {
        r.quota = Some(BalanceQuota {
            limit,
            used,
            unit: unit.to_string(),
        });
    }
    Some(r)
}

// ---------------------------------------------------------------- ThinkWatch 企业网关

/// 这是 ThinkWatch 企业网关的用量接口吗：顶层有一个 `limits` 数组（没有 `mode`）
pub fn is_thinkwatch(v: &Value) -> bool {
    !is_sub2api(v) && v.get("limits").is_some_and(Value::is_array)
}

/// `GET /v1/usage`：
///
/// ```json
/// {"usage":{"requests_today":3,"tokens_today":9000,"requests_month":80,"tokens_month":400000,
///           "cost_usd_month":12.5},
///  "limits":[{"scope":"key","kind":"requests","window":"1h","window_secs":3600,"limit":100,
///             "used":7,"resets_at":"…"}],
///  "expires_at":null}
/// ```
///
/// 每一条限额是一个窗口，单位就是 `kind`（`requests`、`tokens`），带着它管的是这把密钥还是
/// 它所属的用户（`scope`）；认不出的一条不要。这个月花了多少（`usage.cost_usd_month`）是
/// 花费，顶层的 `scope` 说它是这把密钥的还是用户整体的。**没有钱包，也没有总额度**：企业网关的限额都是按时间窗口的
pub fn thinkwatch(v: &Value) -> Option<Reading> {
    let limits = v.get("limits")?.as_array()?;
    let windows = limits
        .iter()
        .filter_map(|item| {
            let unit = match item.get("kind")?.as_str()? {
                "requests" => "requests",
                "tokens" => "tokens",
                _ => return None,
            };
            let window = item.get("window")?.as_str()?.trim();
            if window.is_empty() {
                return None;
            }
            Some(BalanceWindow {
                window: window.to_string(),
                limit: number(item.get("limit")?)?,
                used: item.get("used").and_then(number).unwrap_or(0.0),
                unit: unit.to_string(),
                resets_at_ms: item.get("resets_at").and_then(moment),
                scope: item
                    .get("scope")
                    .and_then(Value::as_str)
                    .and_then(BalanceScope::from_slug),
            })
        })
        .collect();
    Some(Reading {
        windows,
        expires_at_ms: v.get("expires_at").and_then(moment),
        spent: v
            .get("usage")
            .and_then(|u| u.get("cost_usd_month"))
            .and_then(number)
            .map(|amount| Spent {
                amount,
                currency: USD.to_string(),
                period: SpentPeriod::Month,
                // 顶层的 `scope`：密钥自己设了限额时是这把密钥花的，没设时是用户整体花的
                scope: v
                    .get("scope")
                    .and_then(Value::as_str)
                    .and_then(BalanceScope::from_slug),
            }),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    fn usd(amount: f64) -> Option<Money> {
        Some(Money {
            amount,
            currency: "USD".into(),
        })
    }

    fn ms(rfc3339: &str) -> u64 {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .timestamp_millis() as u64
    }

    // ------------------------------------------------------------ OpenRouter

    /// 设了额度的密钥，照 `/api/v1/key` 真实回答的字段写
    const OPENROUTER_LIMITED: &str = r#"{"data":{"label":"sk-or-v1-abc...xyz","limit":10,"limit_remaining":7.5,"limit_reset":"monthly","include_byok_in_limit":false,
        "usage":42.25,"usage_daily":0.5,"usage_weekly":1.75,"usage_monthly":2.5,
        "byok_usage":0,"byok_usage_daily":0,"byok_usage_weekly":0,"byok_usage_monthly":0,
        "is_free_tier":false,"is_management_key":false,"is_provisioning_key":false,
        "expires_at":"2027-01-01T00:00:00Z","creator_user_id":"user_x","organization_id":null,"workspace_id":null,
        "allowed_data_regions":["global"],"free_model_daily_requests":{"limit":1000,"used":0,"remaining":1000},
        "rate_limit":{"requests":-1,"interval":"10s","note":"deprecated"}}}"#;

    /// 没设额度的密钥：大多数密钥都是这样
    const OPENROUTER_UNLIMITED: &str = r#"{"data":{"label":"sk-or-v1-def...uvw","limit":null,"limit_remaining":null,"limit_reset":null,"include_byok_in_limit":false,
        "usage":3.1,"usage_daily":0.2,"usage_weekly":0.9,"usage_monthly":1.4,
        "byok_usage":0,"byok_usage_daily":0,"byok_usage_weekly":0,"byok_usage_monthly":0,
        "is_free_tier":false,"is_management_key":false,"is_provisioning_key":false,
        "creator_user_id":"user_x","organization_id":null,"workspace_id":null,
        "allowed_data_regions":["global"],"free_model_daily_requests":{"limit":1000,"used":3,"remaining":997},
        "rate_limit":{"requests":-1,"interval":"10s","note":"deprecated"}}}"#;

    fn spent_usd(amount: f64, period: SpentPeriod) -> Option<Spent> {
        Some(Spent {
            amount,
            currency: "USD".into(),
            period,
            scope: None,
        })
    }

    #[test]
    fn an_openrouter_key_with_a_limit_reads_as_a_quota() {
        let r = openrouter(&json(OPENROUTER_LIMITED)).unwrap();
        // **已用是这一期的**：额度 10、剩 7.5，用了 2.5；42.25 是这把密钥从来用过的
        assert_eq!(
            r.quota,
            Some(BalanceQuota {
                limit: 10.0,
                used: 2.5,
                unit: "USD".into()
            })
        );
        assert_eq!(r.spent, spent_usd(2.5, SpentPeriod::Month));
        assert_eq!(r.expires_at_ms, Some(ms("2027-01-01T00:00:00Z")));
        assert_eq!(r.wallet, None);
        // 没给剩余就用 usage；没给这个月的，花费是从来一共的
        let r = openrouter(&json(r#"{"data":{"limit":10,"usage":4}}"#)).unwrap();
        assert_eq!(r.quota.unwrap().used, 4.0);
        assert_eq!(r.spent, spent_usd(4.0, SpentPeriod::Total));
    }

    /// 没设额度：**只说得出花了多少**，没有钱包 —— 账户余额这把密钥问不到
    #[test]
    fn an_openrouter_key_without_a_limit_says_what_it_spent() {
        let r = openrouter(&json(OPENROUTER_UNLIMITED)).unwrap();
        assert_eq!(
            r,
            Reading {
                spent: spent_usd(1.4, SpentPeriod::Month),
                ..Default::default()
            }
        );
        let r = openrouter(&json(r#"{"data":{"limit":null,"usage":3.1}}"#)).unwrap();
        assert_eq!(r.spent, spent_usd(3.1, SpentPeriod::Total));
        assert_eq!(r.quota, None);
    }

    #[test]
    fn an_openrouter_answer_it_cannot_read_is_none() {
        assert_eq!(openrouter(&json(r#"{"error":"nope"}"#)), None);
        assert_eq!(openrouter(&json(r#"{"data":{"limit":"ten"}}"#)), None);
        assert_eq!(openrouter(&json(r#"{"data":{"limit":10}}"#)), None);
        assert_eq!(openrouter(&json(r#"{"data":{"limit":null}}"#)), None);
    }

    // ------------------------------------------------------------ DeepSeek

    #[test]
    fn deepseek_shows_the_first_currency_that_has_money() {
        let r = deepseek(&json(
            r#"{"is_available":true,"balance_infos":[
                {"currency":"USD","total_balance":"0.00","granted_balance":"0.00","topped_up_balance":"0.00"},
                {"currency":"CNY","total_balance":"110.00","granted_balance":"10.00","topped_up_balance":"100.00"}
            ]}"#,
        ))
        .unwrap();
        assert_eq!(
            r.wallet,
            Some(Money {
                amount: 110.0,
                currency: "CNY".into()
            })
        );
        // 都没钱：第一个，**是 0，不是没有**
        let r = deepseek(&json(
            r#"{"is_available":false,"balance_infos":[
                {"currency":"CNY","total_balance":"0.00"},
                {"currency":"USD","total_balance":"0.00"}
            ]}"#,
        ))
        .unwrap();
        assert_eq!(
            r.wallet,
            Some(Money {
                amount: 0.0,
                currency: "CNY".into()
            })
        );
    }

    #[test]
    fn deepseek_without_balance_infos_is_not_understood() {
        assert_eq!(deepseek(&json(r#"{"error":{"message":"x"}}"#)), None);
        assert_eq!(
            deepseek(&json(r#"{"balance_infos":[{"currency":"CNY"}]}"#)),
            None
        );
        // 空的清单：读懂了，只是没有钱包
        assert_eq!(
            deepseek(&json(r#"{"balance_infos":[]}"#)),
            Some(Reading::default())
        );
    }

    // ------------------------------------------------------------ Moonshot

    #[test]
    fn moonshot_reads_the_available_balance_in_the_currency_of_its_site() {
        let body = json(
            r#"{"code":0,"data":{"available_balance":49.58894,"voucher_balance":46.58893,"cash_balance":3.00001},"scode":"0x0","status":true}"#,
        );
        assert_eq!(
            moonshot(&body, "CNY").unwrap().wallet,
            Some(Money {
                amount: 49.58894,
                currency: "CNY".into()
            })
        );
        assert_eq!(moonshot(&body, "USD").unwrap().wallet, usd(49.58894));
        assert_eq!(moonshot(&json(r#"{"code":5,"data":null}"#), "CNY"), None);
    }

    // ------------------------------------------------------------ Sub2API

    /// 一个真实的钱包模式回答的样子：用量明细一概不要
    #[test]
    fn a_sub2api_wallet_reads_as_a_wallet() {
        let v = json(
            r#"{"mode":"unrestricted","isValid":true,"planName":"钱包余额","unit":"USD","balance":12.3456,"remaining":12.3456,
                "usage":{"today":{"requests":3,"cost":0.12,"actual_cost":0.096},"total":{"requests":90,"cost":7.65,"actual_cost":6.12}},
                "daily_usage":[{"date":"2026-10-10","cost":0.12}],
                "model_stats":[{"model":"claude-sonnet-4-5","requests":3}]}"#,
        );
        assert!(is_sub2api(&v));
        assert!(!is_thinkwatch(&v));
        let r = sub2api(&v).unwrap();
        assert_eq!(r.wallet, usd(12.3456));
        assert_eq!(r.quota, None);
        assert!(r.windows.is_empty());
        assert_eq!(r.expires_at_ms, None);
        // 今天花了多少：**实际扣的**，不是按标价算的 0.12
        assert_eq!(
            r.spent,
            Some(Spent {
                amount: 0.096,
                currency: "USD".into(),
                period: SpentPeriod::Today,
                scope: None,
            })
        );
        // 没给实际扣的：按标价算的
        let r = sub2api(&json(
            r#"{"mode":"unrestricted","balance":1,"usage":{"today":{"requests":3,"cost":0.12}}}"#,
        ))
        .unwrap();
        assert_eq!(r.spent.unwrap().amount, 0.12);
    }

    #[test]
    fn a_sub2api_subscription_reads_as_daily_weekly_and_monthly_windows() {
        let r = sub2api(&json(
            r#"{"mode":"unrestricted","unit":"USD","expires_at":"2026-12-31T00:00:00Z",
                "subscription":{"daily_usage_usd":1.5,"daily_limit_usd":10,"weekly_usage_usd":20,"weekly_limit_usd":50,
                                "monthly_usage_usd":20,"monthly_limit_usd":0}}"#,
        ))
        .unwrap();
        let names: Vec<_> = r.windows.iter().map(|w| w.window.as_str()).collect();
        // 月限额是 0：不限，不是「额度为 0」
        assert_eq!(names, ["daily", "weekly"]);
        assert_eq!(r.windows[0].limit, 10.0);
        assert_eq!(r.windows[0].used, 1.5);
        assert_eq!(r.windows[0].unit, "USD");
        assert_eq!(r.windows[1].used, 20.0);
        assert_eq!(r.wallet, None, "没写钱包就没有，不是 $0");
        assert_eq!(r.expires_at_ms, Some(ms("2026-12-31T00:00:00Z")));
    }

    #[test]
    fn a_sub2api_quota_limited_key_reads_as_a_quota_and_its_rate_limits() {
        let r = sub2api(&json(
            r#"{"mode":"quota_limited","unit":"USD","expires_at":1798675200,
                "quota":{"limit":100,"used":37.5,"remaining":62.5},
                "rate_limits":[
                    {"window":"5h","limit":5,"used":1.25,"reset_at":"2026-10-11T03:00:00Z"},
                    {"window":"1d","limit":20,"used":4,"reset_at":null},
                    {"window":"7d","limit":"60","used":4},
                    {"limit":1,"used":0}
                ]}"#,
        ))
        .unwrap();
        assert_eq!(
            r.quota,
            Some(BalanceQuota {
                limit: 100.0,
                used: 37.5,
                unit: "USD".into()
            })
        );
        let names: Vec<_> = r.windows.iter().map(|w| w.window.as_str()).collect();
        assert_eq!(names, ["5h", "1d", "7d"], "没有名字的窗口不要");
        assert_eq!(r.windows[0].resets_at_ms, Some(ms("2026-10-11T03:00:00Z")));
        assert_eq!(r.windows[1].resets_at_ms, None);
        assert_eq!(r.windows[2].limit, 60.0);
        // 秒的时间戳
        assert_eq!(r.expires_at_ms, Some(1_798_675_200_000));
        assert_eq!(r.wallet, None);
    }

    #[test]
    fn sub2api_takes_the_unit_it_says() {
        let r = sub2api(&json(
            r#"{"mode":"unrestricted","unit":"CNY","balance":"8.5"}"#,
        ))
        .unwrap();
        assert_eq!(
            r.wallet,
            Some(Money {
                amount: 8.5,
                currency: "CNY".into()
            })
        );
    }

    #[test]
    fn without_a_string_mode_it_is_not_sub2api() {
        assert!(!is_sub2api(&json(r#"{"mode":1}"#)));
        assert!(!is_sub2api(&json(r#"{"object":"list"}"#)));
        assert_eq!(sub2api(&json(r#"{"balance":3}"#)), None);
    }

    // ------------------------------------------------------------ New API

    #[test]
    fn a_limited_newapi_key_reads_as_a_quota_in_dollars() {
        let sub = json(
            r#"{"object":"billing_subscription","has_payment_method":true,"soft_limit_usd":25,"hard_limit_usd":25,"system_hard_limit_usd":25,"access_until":1798675200}"#,
        );
        assert!(is_newapi(&sub));
        let r = newapi(
            &sub,
            &json(r#"{"object":"list","total_usage":1234.5}"#),
            "USD",
        )
        .unwrap();
        // 已用是美分
        assert_eq!(
            r.quota,
            Some(BalanceQuota {
                limit: 25.0,
                used: 12.345,
                unit: "USD".into()
            })
        );
        assert_eq!(r.spent, None);
        assert_eq!(r.expires_at_ms, Some(1_798_675_200_000));
    }

    #[test]
    fn an_unlimited_newapi_key_only_says_what_it_spent() {
        let sub = json(
            r#"{"object":"billing_subscription","hard_limit_usd":100000000,"system_hard_limit_usd":100000000,"access_until":0}"#,
        );
        let r = newapi(&sub, &json(r#"{"total_usage":500}"#), "USD").unwrap();
        assert_eq!(r.quota, None, "1 亿美元不是一个额度");
        assert_eq!(r.spent, spent_usd(5.0, SpentPeriod::Total));
        assert_eq!(r.expires_at_ms, None, "0 是不到期");
    }

    #[test]
    fn the_system_hard_limit_is_enough_to_recognize_newapi() {
        let sub = json(r#"{"system_hard_limit_usd":7}"#);
        assert!(is_newapi(&sub));
        let r = newapi(&sub, &json(r#"{"total_usage":100}"#), "USD").unwrap();
        assert_eq!(r.quota.unwrap().limit, 7.0);
        assert!(!is_newapi(&json(r#"{"object":"billing_subscription"}"#)));
        assert_eq!(newapi(&sub, &json(r#"{"object":"list"}"#), "USD"), None);
    }

    /// 站点的状态接口：照 New API 新版、旧版和 One API 回答里的那两个字段写
    fn status(data: &str) -> Value {
        json(&format!(
            r#"{{"success":true,"message":"","data":{{"version":"v0.9.0","system_name":"New API",{data}"quota_per_unit":500000}}}}"#
        ))
    }

    #[test]
    fn the_newapi_unit_is_the_display_type_of_the_site() {
        let unit = |data: &str| newapi_unit(&status(data));
        assert_eq!(
            unit(r#""quota_display_type":"USD","display_in_currency":true,"#),
            "USD"
        );
        assert_eq!(
            unit(
                r#""quota_display_type":"CNY","display_in_currency":true,"usd_exchange_rate":7.3,"#
            ),
            "CNY"
        );
        assert_eq!(
            unit(r#""quota_display_type":"TOKENS","display_in_currency":false,"#),
            "tokens"
        );
        // 自定义货币只在网页上换算，账单接口仍按美元折算
        assert_eq!(
            unit(
                r#""quota_display_type":"CUSTOM","display_in_currency":true,"custom_currency_symbol":"€","#
            ),
            "USD"
        );
        // 旧版和 One API 只有 display_in_currency
        assert_eq!(unit(r#""display_in_currency":true,"#), "USD");
        assert_eq!(unit(r#""display_in_currency":false,"#), "tokens");
        // 说不出来的：不知道，**不当作美元**
        assert_eq!(unit(""), "unknown");
        assert_eq!(
            unit(r#""quota_display_type":"POINTS","display_in_currency":true,"#),
            "unknown"
        );
        assert_eq!(
            unit(r#""quota_display_type":"POINTS","display_in_currency":false,"#),
            "tokens"
        );
        assert_eq!(newapi_unit(&json(r#"{"error":"not found"}"#)), "unknown");
    }

    #[test]
    fn a_newapi_site_in_yuan_reads_in_yuan() {
        let sub = json(r#"{"hard_limit_usd":73,"access_until":0}"#);
        let r = newapi(&sub, &json(r#"{"total_usage":1460}"#), "CNY").unwrap();
        assert_eq!(
            r.quota,
            Some(BalanceQuota {
                limit: 73.0,
                used: 14.6,
                unit: "CNY".into()
            })
        );
    }

    /// 按 token 计：数是 token；**只有正好 1 亿是不限额度**，比它大的 token 额度是真的
    #[test]
    fn a_newapi_site_in_tokens_reads_in_tokens() {
        let r = newapi(
            &json(r#"{"hard_limit_usd":500000000}"#),
            &json(r#"{"total_usage":12000000000}"#),
            "tokens",
        )
        .unwrap();
        assert_eq!(
            r.quota,
            Some(BalanceQuota {
                limit: 500_000_000.0,
                used: 120_000_000.0,
                unit: "tokens".into()
            })
        );
        let r = newapi(
            &json(r#"{"hard_limit_usd":100000000}"#),
            &json(r#"{"total_usage":250000}"#),
            "tokens",
        )
        .unwrap();
        assert_eq!(r.quota, None);
        assert_eq!(
            r.spent,
            Some(Spent {
                amount: 2500.0,
                currency: "tokens".into(),
                period: SpentPeriod::Total,
                scope: None,
            })
        );
    }

    /// 不知道按什么计：单位照实说不知道，数照给
    #[test]
    fn a_newapi_site_that_does_not_say_has_an_unknown_unit() {
        let r = newapi(
            &json(r#"{"hard_limit_usd":300000000}"#),
            &json(r#"{"total_usage":100}"#),
            "unknown",
        )
        .unwrap();
        let q = r.quota.unwrap();
        assert_eq!((q.limit, q.unit.as_str()), (300_000_000.0, "unknown"));
    }

    // ------------------------------------------------------------ ThinkWatch

    /// 企业网关的回答：`limits` 是这里给的那几条，`usage` 和 `expires_at` 照真实的写
    fn enterprise(limits: &str) -> Value {
        json(&format!(
            r#"{{"usage":{{"requests_today":3,"tokens_today":9000,"requests_month":80,"tokens_month":400000,"cost_usd_month":12.5}},
                 "limits":[{limits}],"expires_at":"2027-01-01T00:00:00Z"}}"#
        ))
    }

    /// 顶层的 `scope` 落到花费上：密钥自己设了限额是 key，没设是用户整体；没写是空
    #[test]
    fn enterprise_spent_says_whose_money_it_is() {
        let with = |scope: &str| {
            json(&format!(
                r#"{{"scope":{scope},"usage":{{"cost_usd_month":3.5}},"limits":[],"expires_at":null}}"#
            ))
        };
        let spent = |v: &Value| thinkwatch(v).unwrap().spent.unwrap();
        assert_eq!(spent(&with(r#""key""#)).scope, Some(BalanceScope::Key));
        assert_eq!(spent(&with(r#""user""#)).scope, Some(BalanceScope::User));
        assert_eq!(spent(&with("null")).scope, None);
        assert_eq!(spent(&with(r#""team""#)).scope, None);
        assert_eq!(spent(&with(r#""user""#)).amount, 3.5);
    }

    fn scopes(r: &Reading) -> Vec<(String, Option<BalanceScope>)> {
        r.windows
            .iter()
            .map(|w| (w.window.clone(), w.scope))
            .collect()
    }

    #[test]
    fn enterprise_limits_of_the_key_alone() {
        let v = enterprise(
            r#"{"scope":"key","kind":"requests","window":"1h","window_secs":3600,"limit":100,"used":7,"resets_at":"2026-10-10T15:00:00Z"},
               {"scope":"key","kind":"tokens","window":"daily","window_secs":null,"limit":1000000,"used":250000,"resets_at":null}"#,
        );
        assert!(is_thinkwatch(&v));
        assert!(!is_sub2api(&v));
        let r = thinkwatch(&v).unwrap();
        assert_eq!(
            r.windows[0],
            BalanceWindow {
                window: "1h".into(),
                limit: 100.0,
                used: 7.0,
                unit: "requests".into(),
                resets_at_ms: Some(ms("2026-10-10T15:00:00Z")),
                scope: Some(BalanceScope::Key),
            }
        );
        assert_eq!(r.windows[1].unit, "tokens");
        assert_eq!(r.windows[1].resets_at_ms, None);
        assert_eq!(
            r.spent,
            Some(Spent {
                amount: 12.5,
                currency: "USD".into(),
                period: SpentPeriod::Month,
                scope: None,
            })
        );
        assert_eq!(r.expires_at_ms, Some(ms("2027-01-01T00:00:00Z")));
        // 企业网关没有钱包、没有总额度
        assert_eq!(r.wallet, None);
        assert_eq!(r.quota, None);
    }

    #[test]
    fn enterprise_limits_of_the_user_alone() {
        let r = thinkwatch(&enterprise(
            r#"{"scope":"user","kind":"tokens","window":"1w","window_secs":604800,"limit":5000000,"used":1200000,"resets_at":"2026-10-13T00:00:00Z"}"#,
        ))
        .unwrap();
        assert_eq!(scopes(&r), [("1w".to_string(), Some(BalanceScope::User))]);
        assert_eq!(r.windows[0].used, 1_200_000.0);
    }

    #[test]
    fn enterprise_limits_of_the_key_and_the_user_together() {
        let r = thinkwatch(&enterprise(
            r#"{"scope":"key","kind":"requests","window":"1m","window_secs":60,"limit":20,"used":2,"resets_at":null},
               {"scope":"user","kind":"requests","window":"1m","window_secs":60,"limit":60,"used":9,"resets_at":null},
               {"scope":"user","kind":"tokens","window":"monthly","window_secs":null,"limit":9000000,"used":400000,"resets_at":null},
               {"scope":"team","kind":"cost","window":"1d","limit":5,"used":1}"#,
        ))
        .unwrap();
        assert_eq!(
            scopes(&r),
            [
                ("1m".to_string(), Some(BalanceScope::Key)),
                ("1m".to_string(), Some(BalanceScope::User)),
                ("monthly".to_string(), Some(BalanceScope::User)),
            ],
            "认不出的 kind 不要"
        );
    }

    /// 没有限额：只说得出这个月花了多少
    #[test]
    fn an_enterprise_key_without_limits_only_says_what_it_spent() {
        let r = thinkwatch(&json(
            r#"{"usage":{"requests_today":0,"tokens_today":0,"requests_month":4,"tokens_month":800,"cost_usd_month":0.42},"limits":[],"expires_at":null}"#,
        ))
        .unwrap();
        assert!(r.windows.is_empty());
        assert_eq!(r.spent.as_ref().unwrap().amount, 0.42);
        assert_eq!(r.spent.unwrap().period, SpentPeriod::Month);
        assert_eq!(r.expires_at_ms, None);
        // 这个月的花费不知道（null）：没有花费，不是 $0
        let r = thinkwatch(&json(
            r#"{"usage":{"cost_usd_month":null},"limits":[],"expires_at":null}"#,
        ))
        .unwrap();
        assert_eq!(r, Reading::default());
        assert_eq!(thinkwatch(&json(r#"{"limits":"none"}"#)), None);
    }
}
