//! 一把网关密钥的用量上限：每分钟、每小时、每天、每周、每月最多多少个请求、多少 token、
//! 花多少钱。
//!
//! **上限挂在密钥上**，不挂在上游上：要管住的是「这台机器上的这个脚本」，不是「这家
//! 上游」。一把密钥可以有好几条，**每一条都要过**。
//!
//! 怎么数、怎么等、到了怎么拒，在 `tw_gateway::key_limits`；这里只有写法。

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

    /// 滚动的那两种：窗口多长，毫秒。自然周期是 None
    pub fn rolling_ms(&self) -> Option<i64> {
        match self {
            LimitPer::Minute => Some(60_000),
            LimitPer::Hour => Some(3_600_000),
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

/// 一条上限：`{ per: day, cost: 5 }`。**一条只写一种量**（请求数、token、费用三选一），
/// 校验时查。
///
/// 三种量写成三个可选字段而不是 `{ measure: cost, max: 5 }`：手写的人读
/// `requests: 30` 不用再对一遍哪个数是什么单位。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyLimit {
    pub per: LimitPer,
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

impl KeyLimit {
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

    /// 两条算不算同一条：同一段时间、同一种量、缓存读算不算进去也一样
    pub fn same_as(&self, other: &KeyLimit) -> bool {
        self.per == other.per
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
            requests: Some(3),
            tokens: None,
            cost: None,
            cache_reads: false,
        })
        .unwrap();
        assert_eq!(out.trim(), "per: hour\nrequests: 3");
    }

    #[test]
    fn a_typo_is_refused_rather_than_ignored() {
        assert!(serde_yaml_ng::from_str::<KeyLimit>("{per: day, request: 3}").is_err());
        assert!(serde_yaml_ng::from_str::<KeyLimit>("{per: daily, requests: 3}").is_err());
    }
}
