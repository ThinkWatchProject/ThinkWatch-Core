//! 默认价目表：公开价格数据集。
//!
//! 随版本内置一份（离线、首次启动时用），之后联网定期刷新。

use std::collections::HashMap;

use crate::{ModelPrice, PricingError, name};

/// 内置快照。**pin 在一个具体的 commit 上**，来历见 `data/PROVENANCE.md`。
const SNAPSHOT: &[u8] = include_bytes!("../data/model_prices.json.gz");

/// 快照对应的上游 commit 日期。
pub const SNAPSHOT_DATE: &str = "2026-09-09";

/// 刷新去哪儿拉。
///
/// **写死在代码里，不从配置读。**一个「价目表源」配置项等于给了任何能
/// 改 config.yaml 的人一个往这个进程里喂 JSON 的入口，而那份 JSON 会
/// 决定用户看到的每一个金额。
pub const UPDATE_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// 这份表是从哪儿来的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableSource {
    /// 随版本内置的快照
    Builtin,
    /// 联网拉回来的
    Fetched,
    /// 什么都没加载成 —— 每个模型都会是「价格未知」
    Empty,
}

impl TableSource {
    pub fn slug(&self) -> &'static str {
        match self {
            TableSource::Builtin => "builtin",
            TableSource::Fetched => "fetched",
            TableSource::Empty => "empty",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Table {
    prices: HashMap<String, ModelPrice>,
    /// **这份表自己的日期**。成本旁边要标它：一个两个月前的价目表算出来的
    /// 数字，和一个昨天的，可信度完全不同
    pub date: String,
    pub source: TableSource,
}

impl Table {
    /// 随版本内置的那份。
    pub fn builtin() -> Result<Self, PricingError> {
        let raw = decompress(SNAPSHOT)?;
        let prices = parse_dataset(&raw)?;
        Ok(Self {
            prices,
            date: SNAPSHOT_DATE.to_string(),
            source: TableSource::Builtin,
        })
    }

    /// 联网拉回来的数据集。`date` 是拉取那天。
    pub fn fetched(raw: &[u8], date: String) -> Result<Self, PricingError> {
        Ok(Self {
            prices: parse_dataset(raw)?,
            date,
            source: TableSource::Fetched,
        })
    }

    /// 一张空表。**每个模型都会是「价格未知」** —— 那是价目表读不了时
    /// 唯一诚实的答案。
    pub fn empty() -> Self {
        Self {
            prices: HashMap::new(),
            date: String::new(),
            source: TableSource::Empty,
        }
    }

    pub fn len(&self) -> usize {
        self.prices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.prices.is_empty()
    }

    /// 查一个模型的价格，**并且说清是不是跨平台借来的**（第二个值为真时，
    /// 算出来的钱一律标成估算）。
    pub fn lookup(&self, model: &str) -> Option<(&ModelPrice, bool)> {
        for c in name::candidates(model) {
            if let Some(p) = self.prices.get(&c) {
                return Some((p, false));
            }
        }
        // 跨平台的最后一招。**它不精确**，见 name::cross_platform_fallback
        let c = name::cross_platform_fallback(model)?;
        self.prices.get(&c).map(|p| (p, true))
    }

    pub fn get(&self, model: &str) -> Option<&ModelPrice> {
        self.lookup(model).map(|(p, _)| p)
    }

    /// 按原样查一个键，**不做任何名字归一化**。给核对数据集本身的测试用。
    pub fn exact(&self, key: &str) -> Option<&ModelPrice> {
        self.prices.get(key)
    }

    /// 表里所有的模型名。
    pub fn models(&self) -> impl Iterator<Item = &str> {
        self.prices.keys().map(String::as_str)
    }

    /// 和 `old` 比，有几个模型的价格变了、是新增的、或者不在了。
    pub fn changed_from(&self, old: &Table) -> usize {
        let changed_or_new = self
            .prices
            .iter()
            .filter(|(m, p)| old.prices.get(*m) != Some(p))
            .count();
        let gone = old
            .prices
            .keys()
            .filter(|m| !self.prices.contains_key(*m))
            .count();
        changed_or_new + gone
    }
}

/// 解析 LiteLLM 那份 JSON。**一个带价格的模型都没有就是失败** —— 那多半
/// 不是那份数据集（被劫持的响应、一个错误页）。
fn parse_dataset(raw: &[u8]) -> Result<HashMap<String, ModelPrice>, PricingError> {
    let parsed: HashMap<String, serde_json::Value> =
        serde_json::from_slice(raw).map_err(|e| PricingError::Dataset(e.to_string()))?;
    let prices: HashMap<String, ModelPrice> = parsed
        .into_iter()
        // `sample_spec` 那种说明用的伪条目没有价格，自然会被滤掉
        .filter_map(|(k, v)| price_from(&v).map(|p| (k, p)))
        .collect();
    if prices.is_empty() {
        return Err(PricingError::Dataset(
            "里面一个带价格的模型都没有 —— 多半不是那份数据集".into(),
        ));
    }
    Ok(prices)
}

fn decompress(gz: &[u8]) -> Result<Vec<u8>, PricingError> {
    use std::io::Read;
    let mut d = flate2::read::GzDecoder::new(gz);
    let mut out = Vec::with_capacity(2_500_000);
    d.read_to_end(&mut out)
        .map_err(|e| PricingError::Snapshot(e.to_string()))?;
    Ok(out)
}

fn price_from(v: &serde_json::Value) -> Option<ModelPrice> {
    let f = |k: &str| v.get(k).and_then(|x| x.as_f64());
    // 没有输入或输出单价的条目不是一个能算钱的模型（嵌入、审核、以及
    // 数据集里那条 `sample_spec` 说明）。
    let input = f("input_cost_per_token")?;
    let output = f("output_cost_per_token").unwrap_or(0.0);
    Some(ModelPrice {
        input,
        output,
        cache_read: f("cache_read_input_token_cost"),
        cache_write_5m: f("cache_creation_input_token_cost"),
        cache_write_1h: f("cache_creation_input_token_cost_above_1hr"),
        input_above_200k: f("input_cost_per_token_above_200k_tokens"),
        output_above_200k: f("output_cost_per_token_above_200k_tokens"),
        max_input_tokens: v.get("max_input_tokens").and_then(|x| x.as_u64()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_snapshot_loads_and_has_the_models_people_actually_use() {
        let t = Table::builtin().unwrap();
        assert!(t.len() > 1000, "只有 {} 个模型，快照像是坏的", t.len());
        assert_eq!(t.source, TableSource::Builtin);
        assert_eq!(t.date, SNAPSHOT_DATE);
        for m in [
            "claude-sonnet-4-5",
            "claude-opus-4-20250514",
            "claude-haiku-4-5",
            "claude-3-5-haiku-20241022",
            "gpt-4o",
        ] {
            assert!(t.get(m).is_some(), "查不到 {m}");
        }
    }

    #[test]
    fn a_response_that_is_not_the_dataset_is_refused() {
        // 被劫持的响应、一个错误页：解析得了的 JSON，但一个价格都没有
        assert!(Table::fetched(b"{\"error\": \"rate limited\"}", "x".into()).is_err());
        assert!(Table::fetched(b"<html>", "x".into()).is_err());
    }

    #[test]
    fn the_snapshot_date_is_a_real_date_because_it_has_to_be_shown() {
        assert_eq!(SNAPSHOT_DATE.len(), 10, "日期得是 YYYY-MM-DD");
    }

    #[test]
    fn a_refresh_counts_changed_new_and_removed_models() {
        let mk = |json: &str| Table::fetched(json.as_bytes(), "d".into()).unwrap();
        let old = mk(
            r#"{"a":{"input_cost_per_token":1e-6,"output_cost_per_token":2e-6},"b":{"input_cost_per_token":1e-6,"output_cost_per_token":2e-6},"gone":{"input_cost_per_token":1e-6}}"#,
        );
        let new = mk(
            r#"{"a":{"input_cost_per_token":1e-6,"output_cost_per_token":2e-6},"b":{"input_cost_per_token":3e-6,"output_cost_per_token":2e-6},"c":{"input_cost_per_token":1e-6,"output_cost_per_token":2e-6}}"#,
        );
        // b 改价、c 新增、gone 不在了；a 没变
        assert_eq!(new.changed_from(&old), 3);
        assert_eq!(new.changed_from(&new), 0);
    }
}
