//! 发出一个会产生费用的请求之前的报价。推理测速和回放共用。
//!
//! **两种计费方式**：按量计费的按价目表算，不计费的是 0。订阅账号也按量
//! 计费 —— 报出来的是按 API 价格折算的费用，它消耗的额度另由上游报。
//! token 数由调用方一起给出，那是唯一一个确定知道的量。

use tw_config::Billing;

#[derive(Debug, Clone, PartialEq)]
pub struct Quote {
    /// 微分。按量计费且算得出来时是那个数；不计费时是 0。**无法计价时是空**
    /// —— 不是 0
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
/// **任何一项没有金额，合计就是空**：给一个看起来完整的数字，用户会以为
/// 那就是全部代价。
pub fn total<'a>(quotes: impl IntoIterator<Item = &'a Quote>) -> Option<i64> {
    quotes.into_iter().map(|q| q.cost_micros).sum()
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
    fn per_token_is_priced_and_free_is_zero() {
        let q = |b| quote(&book(), "p", "claude-sonnet-4-5", &usage(), b);
        assert!(q(Billing::PerToken).cost_micros.unwrap() > 0);
        assert_eq!(q(Billing::Free).cost_micros, Some(0));
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
    fn free_adds_nothing_and_anything_without_an_amount_voids_the_total() {
        let q = |m: &str, b| quote(&book(), "p", m, &usage(), b);
        let priced = q("claude-sonnet-4-5", Billing::PerToken);
        let free = q("claude-sonnet-4-5", Billing::Free);
        assert_eq!(total([&priced, &free]), priced.cost_micros, "不计费是 0");
        let unpriced = q("中转站自己起的名字", Billing::PerToken);
        assert_eq!(total([&priced, &unpriced]), None);
    }
}
