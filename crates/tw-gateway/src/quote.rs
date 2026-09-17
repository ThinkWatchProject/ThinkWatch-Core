//! 发出一个会产生费用的请求之前的报价。推理测速和回放共用。
//!
//! **四种计费方式的金额各不相同**：订阅制和计费方式未知的没有金额，不计费
//! 的是 0。怎么向用户说明由界面按计费方式决定，token 数由调用方一起给出
//! —— 那是唯一一个确定知道的量。

use tw_config::Billing;

#[derive(Debug, Clone, PartialEq)]
pub struct Quote {
    /// 微分。按量计费且算得出来时是那个数；不计费时是 0。**订阅制、计费方式
    /// 未知、无法计价时是空** —— 不是 0
    pub cost_micros: Option<i64>,
    pub billing: Billing,
}

/// 这家上游跑这个模型、用这么多 token，报一个价。
pub fn quote(
    book: &tw_pricing::PriceBook,
    provider: &str,
    model: &str,
    usage: &tw_pricing::Usage,
    billing: Billing,
) -> Quote {
    let cost_micros = match billing {
        // **订阅制不是免费**：它不按 token 收费，消耗的是额度
        Billing::Subscription | Billing::Unknown => None,
        Billing::Free => Some(0),
        Billing::PerToken => match book.cost_for(provider, model, usage, false) {
            tw_pricing::Cost::Known(m) | tw_pricing::Cost::Estimated(m) => Some(m),
            tw_pricing::Cost::Unpriced { .. } => None,
        },
    };
    Quote {
        cost_micros,
        billing,
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
    fn each_billing_mode_has_its_own_amount() {
        let q = |b| quote(&book(), "p", "claude-sonnet-4-5", &usage(), b);
        let per_token = q(Billing::PerToken);
        assert!(per_token.cost_micros.unwrap() > 0);
        let sub = q(Billing::Subscription);
        assert_eq!(sub.cost_micros, None);
        let free = q(Billing::Free);
        assert_eq!(free.cost_micros, Some(0));
        let unknown = q(Billing::Unknown);
        assert_eq!(unknown.cost_micros, None);
        let unpriced = quote(
            &book(),
            "p",
            "中转站自己起的名字",
            &usage(),
            Billing::PerToken,
        );
        assert_eq!(unpriced.cost_micros, None);
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
