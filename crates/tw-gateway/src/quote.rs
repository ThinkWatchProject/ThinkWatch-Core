//! 发出一个会花钱的请求之前的报价。推理测速和回放共用。
//!
//! **四种计费方式各说各的**，而都要给出 token 数 —— 那是唯一一个我们确定
//! 知道的量。

use tw_config::Billing;

#[derive(Debug, Clone, PartialEq)]
pub struct Quote {
    /// 微分。按量计费且算得出来时是那个数；不计费时是 0。**订阅制、计费方式
    /// 未知、无法计价时是空** —— 不是 0
    pub cost_micros: Option<i64>,
    pub billing: Billing,
    /// 给人看的那一句。**金额再小也要显示** —— 用户按下按钮时有权知道自己在
    /// 花什么
    pub note: String,
}

/// 这家上游跑这个模型、用这么多 token，报一个价。
pub fn quote(
    book: &tw_pricing::PriceBook,
    provider: &str,
    model: &str,
    usage: &tw_pricing::Usage,
    billing: Billing,
) -> Quote {
    let tokens = usage.input + usage.output;
    let (cost_micros, note) = match billing {
        // **订阅制不是免费**：它不按 token 收钱，消耗的是额度
        Billing::Subscription => (None, format!("计入订阅额度，约 {tokens} tokens")),
        Billing::Free => (Some(0), format!("不计费，约 {tokens} tokens")),
        Billing::Unknown => (
            None,
            format!("计费方式未知，无法预估费用，约 {tokens} tokens"),
        ),
        Billing::PerToken => match book.cost_for(provider, model, usage, false) {
            tw_pricing::Cost::Known(m) | tw_pricing::Cost::Estimated(m) => {
                // 五位小数：一次测速是几百微分的量级，两位小数会显示成
                // $0.00，而那等于告诉用户「这不花钱」
                (Some(m), format!("约 ${:.5}", m as f64 / 1e6))
            }
            tw_pricing::Cost::Unpriced { .. } => (
                None,
                format!("无法计价：`{model}` 不在价目表中，约 {tokens} tokens"),
            ),
        },
    };
    Quote {
        cost_micros,
        billing,
        note,
    }
}

/// 一批报价的合计。
///
/// **订阅制那几项不进合计**：它们消耗的是额度，界面上单独说。以前它们和
/// 「无法计价」一样让合计变成空 —— 于是只要勾上一家订阅制上游，用户就再也
/// 看不到这批请求要花多少钱。
///
/// **其余任何一项没有金额，合计就是空**：给一个看起来完整的数字，用户会以为
/// 那就是全部代价。
pub fn total<'a>(quotes: impl IntoIterator<Item = &'a Quote>) -> Option<i64> {
    quotes
        .into_iter()
        .filter(|q| q.billing != Billing::Subscription)
        .map(|q| q.cost_micros)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> tw_pricing::PriceBook {
        tw_pricing::PriceBook::builtin().unwrap()
    }

    fn usage() -> tw_pricing::Usage {
        tw_pricing::Usage {
            input: 10,
            output: 8,
            ..Default::default()
        }
    }

    #[test]
    fn each_billing_mode_says_what_it_costs_and_how_many_tokens() {
        let q = |b| quote(&book(), "p", "claude-sonnet-4-5", &usage(), b);
        let per_token = q(Billing::PerToken);
        assert!(per_token.cost_micros.unwrap() > 0);
        assert!(
            per_token.note.starts_with("约 $0.000"),
            "{}",
            per_token.note
        );
        let sub = q(Billing::Subscription);
        assert_eq!(sub.cost_micros, None);
        assert!(
            sub.note.contains("额度") && sub.note.contains("18"),
            "{}",
            sub.note
        );
        let free = q(Billing::Free);
        assert_eq!(free.cost_micros, Some(0));
        let unknown = q(Billing::Unknown);
        assert_eq!(unknown.cost_micros, None);
        assert!(unknown.note.contains("18"), "{}", unknown.note);
        let unpriced = quote(
            &book(),
            "p",
            "中转站自己起的名字",
            &usage(),
            Billing::PerToken,
        );
        assert_eq!(unpriced.cost_micros, None);
        assert!(unpriced.note.contains("无法计价"), "{}", unpriced.note);
    }

    #[test]
    fn a_subscription_stays_out_of_the_total_and_anything_else_without_an_amount_voids_it() {
        let q = |m: &str, b| quote(&book(), "p", m, &usage(), b);
        let priced = q("claude-sonnet-4-5", Billing::PerToken);
        let sub = q("claude-sonnet-4-5", Billing::Subscription);
        let free = q("claude-sonnet-4-5", Billing::Free);
        assert_eq!(
            total([&priced, &sub, &free]),
            priced.cost_micros,
            "订阅不进合计，不计费是 0"
        );
        let unknown = q("claude-sonnet-4-5", Billing::Unknown);
        assert_eq!(total([&priced, &unknown]), None);
        let unpriced = q("中转站自己起的名字", Billing::PerToken);
        assert_eq!(total([&priced, &unpriced]), None);
    }
}
