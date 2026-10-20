//! 用量上限：每 N 分钟、每 N 小时、每天、每周、每月最多多少个请求、多少 token、花多少钱。
//!
//! **同一种写法挂在两处**，管的是两件事：
//!
//! - 网关密钥上（`clients[].limits`）：管住「这台机器上的这个脚本」。到了就拒绝这把密钥的
//!   请求，见 `tw_gateway::key_limits`。
//! - 上游上（`providers[].limits`）：照着那家上游自己的限额写（中转站、订阅套餐常有「每
//!   5 小时多少次」）。到了就不往那家发，换下一家或者等它空出来，见
//!   `tw_gateway::upstream_limits`。
//!
//! 一处可以有好几条，**每一条都要过**。这里只有写法。

use serde::{Deserialize, Serialize};

/// 一段时间。分钟、小时是**滚动的**（最近 60 秒、最近 60 分钟）；天、周、月是**自然的**，
/// 按 core 所在机器的本地时区：零点、周一零点、一号零点重新算。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LimitPer {
    Minute,
    Hour,
    Day,
    Week,
    Month,
}

impl LimitPer {
    /// 写在配置里的那个词。报错、消息参数里用它
    pub fn word(&self) -> &'static str {
        match self {
            LimitPer::Minute => "minute",
            LimitPer::Hour => "hour",
            LimitPer::Day => "day",
            LimitPer::Week => "week",
            LimitPer::Month => "month",
        }
    }

    /// 滚动的那两种：一个单位多长，毫秒。自然周期是 None。一条上限的窗口还要乘上它的
    /// `every`，见 [`KeyLimit::window_ms`]
    pub fn rolling_ms(&self) -> Option<i64> {
        match self {
            LimitPer::Minute => Some(60_000),
            LimitPer::Hour => Some(3_600_000),
            _ => None,
        }
    }

    /// `every` 最多写多少：一小时以内的分钟、一天以内的小时。再长就是写天。自然周期不能写
    /// `every`，是 None
    pub fn every_max(&self) -> Option<u32> {
        match self {
            LimitPer::Minute => Some(60),
            LimitPer::Hour => Some(24),
            _ => None,
        }
    }
}

/// 一条上限数的是什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LimitMeasure {
    Requests,
    Tokens,
    Cost,
}

impl LimitMeasure {
    pub fn word(&self) -> &'static str {
        match self {
            LimitMeasure::Requests => "requests",
            LimitMeasure::Tokens => "tokens",
            LimitMeasure::Cost => "cost",
        }
    }
}

/// 一条上限：`{ per: day, cost: 5 }`、`{ per: hour, every: 5, requests: 300 }`。**一条只写
/// 一种量**（请求数、token、费用三选一），校验时查。
///
/// 三种量写成三个可选字段而不是 `{ measure: cost, max: 5 }`：手写的人读
/// `requests: 30` 不用再对一遍哪个数是什么单位。
///
/// 名字里的 Key 是它先有的那一处；挂在上游上的是同一个类型（[`UsageLimit`]），写法、校验
/// 一样
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyLimit {
    pub per: LimitPer,
    /// 滚动窗口是几个单位：`per: hour, every: 5` 是最近 5 小时。**只有 `minute`（1 到 60）、
    /// `hour`（1 到 24）能写**，不写是 1。天、周、月是自然的，没有「每 3 天」：三天从哪天
    /// 算起说不清
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every: Option<u32>,
    /// 请求数。**写成有符号的**：写了负数时报的是「要大于 0」，不是一句解析器的话
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<i64>,
    /// token 数：没走缓存的输入 + 写进缓存的 + 输出，`cache_reads` 开着时再加上从缓存读的
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<i64>,
    /// 费用，美元（core 记的费用都是美元）。没有价格的模型、不计费的上游算 0
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    /// token 上限把从缓存读的也算进去。**只有 token 上限能写**。缓存读的单价低、量大：
    /// 一段几十万 token 的对话每一轮都整段读一遍，算进去的话上限很快就到
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cache_reads: bool,
}

/// 挂在上游上的用量上限：和密钥的是同一种写法，换个名字读起来清楚
pub type UsageLimit = KeyLimit;

impl KeyLimit {
    /// 滚动窗口是几个单位。不写是 1；自然周期一律是 1
    pub fn every_or_one(&self) -> u32 {
        match self.per.rolling_ms() {
            Some(_) => self.every.unwrap_or(1),
            None => 1,
        }
    }

    /// 滚动窗口多长，毫秒：一个单位乘上 `every`。自然周期是 None。
    ///
    /// **数滚动窗口的地方一律用它**，不用 [`LimitPer::rolling_ms`]：那只是一个单位
    pub fn window_ms(&self) -> Option<i64> {
        self.per
            .rolling_ms()
            .map(|w| w.saturating_mul(i64::from(self.every_or_one().max(1))))
    }

    /// 写了的那几种量。校验要求恰好一种
    pub fn measures(&self) -> Vec<LimitMeasure> {
        let mut out = Vec::new();
        if self.requests.is_some() {
            out.push(LimitMeasure::Requests);
        }
        if self.tokens.is_some() {
            out.push(LimitMeasure::Tokens);
        }
        if self.cost.is_some() {
            out.push(LimitMeasure::Cost);
        }
        out
    }

    /// 这一条数什么。**过了校验的才有意义**：一种都没写、写了两种时是写在前面的那种
    pub fn measure(&self) -> LimitMeasure {
        self.measures()
            .first()
            .copied()
            .unwrap_or(LimitMeasure::Requests)
    }

    /// 上限，整数：请求数、token 数，费用是微分（百万分之一美元，和记账同一个单位）
    pub fn max(&self) -> i64 {
        match self.measure() {
            LimitMeasure::Requests => self.requests.unwrap_or(0),
            LimitMeasure::Tokens => self.tokens.unwrap_or(0),
            LimitMeasure::Cost => self.cost.map(tw_pricing::to_micros).unwrap_or(0),
        }
    }

    /// 两条算不算同一条：同一段时间（`every` 也一样）、同一种量、缓存读算不算进去也一样。
    /// 每 1 小时和每 5 小时是两条：管的是两件事
    pub fn same_as(&self, other: &KeyLimit) -> bool {
        self.per == other.per
            && self.every_or_one() == other.every_or_one()
            && self.measure() == other.measure()
            && self.cache_reads == other.cache_reads
    }
}

impl From<LimitPer> for tw_api::LimitPer {
    fn from(p: LimitPer) -> Self {
        match p {
            LimitPer::Minute => tw_api::LimitPer::Minute,
            LimitPer::Hour => tw_api::LimitPer::Hour,
            LimitPer::Day => tw_api::LimitPer::Day,
            LimitPer::Week => tw_api::LimitPer::Week,
            LimitPer::Month => tw_api::LimitPer::Month,
        }
    }
}

impl From<tw_api::LimitPer> for LimitPer {
    fn from(p: tw_api::LimitPer) -> Self {
        match p {
            tw_api::LimitPer::Minute => LimitPer::Minute,
            tw_api::LimitPer::Hour => LimitPer::Hour,
            tw_api::LimitPer::Day => LimitPer::Day,
            tw_api::LimitPer::Week => LimitPer::Week,
            tw_api::LimitPer::Month => LimitPer::Month,
        }
    }
}

impl From<LimitMeasure> for tw_api::LimitMeasure {
    fn from(m: LimitMeasure) -> Self {
        match m {
            LimitMeasure::Requests => tw_api::LimitMeasure::Requests,
            LimitMeasure::Tokens => tw_api::LimitMeasure::Tokens,
            LimitMeasure::Cost => tw_api::LimitMeasure::Cost,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_measure_reads_back_as_written_and_cost_is_in_micros() {
        let l: KeyLimit = serde_yaml_ng::from_str("{per: day, cost: 5}").unwrap();
        assert_eq!((l.measure(), l.max()), (LimitMeasure::Cost, 5_000_000));
        let l: KeyLimit = serde_yaml_ng::from_str("{per: minute, requests: 30}").unwrap();
        assert_eq!((l.measure(), l.max()), (LimitMeasure::Requests, 30));
        let l: KeyLimit =
            serde_yaml_ng::from_str("{per: week, tokens: 1000000, cache_reads: true}").unwrap();
        assert_eq!((l.measure(), l.max()), (LimitMeasure::Tokens, 1_000_000));
        assert!(l.cache_reads);
        // 默认值不写回去
        let out = serde_yaml_ng::to_string(&KeyLimit {
            per: LimitPer::Hour,
            every: None,
            requests: Some(3),
            tokens: None,
            cost: None,
            cache_reads: false,
        })
        .unwrap();
        assert_eq!(out.trim(), "per: hour\nrequests: 3");
    }

    #[test]
    fn every_multiplies_the_rolling_window() {
        let l: KeyLimit = serde_yaml_ng::from_str("{per: hour, every: 5, requests: 300}").unwrap();
        assert_eq!(l.every, Some(5));
        assert_eq!(l.window_ms(), Some(5 * 3_600_000));
        let l: KeyLimit = serde_yaml_ng::from_str("{per: minute, requests: 30}").unwrap();
        assert_eq!((l.every_or_one(), l.window_ms()), (1, Some(60_000)));
        // 自然周期没有窗口；写了 every 的由校验拒，这里不当真
        let l: KeyLimit = serde_yaml_ng::from_str("{per: day, every: 3, requests: 1}").unwrap();
        assert_eq!((l.every_or_one(), l.window_ms()), (1, None));
        // 每 1 小时和每 5 小时是两条；写明的 1 和不写是同一条
        let one: KeyLimit = serde_yaml_ng::from_str("{per: hour, requests: 3}").unwrap();
        let five: KeyLimit = serde_yaml_ng::from_str("{per: hour, every: 5, requests: 9}").unwrap();
        let also_one: KeyLimit =
            serde_yaml_ng::from_str("{per: hour, every: 1, requests: 9}").unwrap();
        assert!(!one.same_as(&five));
        assert!(one.same_as(&also_one));
        // 不写不写回去，写了照写
        assert!(!serde_yaml_ng::to_string(&one).unwrap().contains("every"));
        assert!(
            serde_yaml_ng::to_string(&five)
                .unwrap()
                .contains("every: 5")
        );
    }

    #[test]
    fn a_typo_is_refused_rather_than_ignored() {
        assert!(serde_yaml_ng::from_str::<KeyLimit>("{per: day, request: 3}").is_err());
        assert!(serde_yaml_ng::from_str::<KeyLimit>("{per: daily, requests: 3}").is_err());
    }
}
