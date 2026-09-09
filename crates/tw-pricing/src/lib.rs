//! 价目表（DESIGN.md §4.3.0）。
//!
//! 菜单栏上那个 `$3.42` 的唯一输入。三层：
//!
//! | 层 | 内容 | 谁写 |
//! |---|---|---|
//! | 内置快照 | 随版本发布，pin 到具体 commit | 我们，发版时核对过 |
//! | 可选更新 | 用户点「检查价格更新」才拉 | 用户主动触发 |
//! | 用户覆盖 | `~/.thinkwatch/pricing.yaml` | 用户 |
//!
//! 第三层是**必需的**：中转站的价格和官方不同，而且没有任何公开数据集
//! 会收录它们。
//!
//! 贯穿这一层的一条规矩：**算不出来就说算不出来。**没有价格的模型不能
//! 记成 0 —— 那是在撒谎，而一个会撒谎的成本面板不如没有（§4.3）。

pub mod name;

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// 内置快照。**pin 在一个具体的 commit 上**，来历见 `data/PROVENANCE.md`。
const SNAPSHOT: &[u8] = include_bytes!("../data/model_prices.json.gz");

/// 快照对应的上游 commit 日期。
///
/// **成本旁边要标它**（§4.3.0）：一个两个月前的价目表算出来的数字，和
/// 一个昨天的，可信度完全不同 —— 而用户没有别的办法知道这件事。
pub const SNAPSHOT_DATE: &str = "2026-09-09";
pub const SNAPSHOT_SOURCE: &str = "LiteLLM model_prices_and_context_window.json";

/// 一个模型的价格。单位一律是**每 token 的美元**，和上游数据集一致。
///
/// 缓存写入分两档不是过度设计：Sonnet 4.5 的 5 分钟档是 \$3.75、1 小时档
/// 是 \$6.00。**只用 5 分钟档会让开了 1 小时 TTL 的用户被系统性低估
/// 60%**，而那是一个看起来很确定的数字（§4.3.0）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
// 覆盖文件里写错一个字段名（`inptu`）被静默忽略的话，用户会以为自己
// 已经改过价格了，而面板上的数字一直是错的。
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_5m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<f64>,
    /// 长上下文分层。超过 200k 之后单价不同
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_above_200k: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_above_200k: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
}

/// 一次调用用掉了什么。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// 缓存写用的是 1 小时 TTL 吗。**差价接近一倍**，猜不得
    pub cache_1h: bool,
}

/// 成本三态（§4.3）。
#[derive(Debug, Clone, PartialEq)]
pub enum Cost {
    /// 上游给了 usage，价目表里有这个模型 —— 这是个可以相加的数
    Known(Micros),
    /// usage 是我们估的。**不能混进「今日花费」的精确数字里**
    Estimated(Micros),
    /// 价目表里没有这个模型。**不是 0** —— 当成 0 会让总额悄悄偏低，
    /// 而用户没有任何线索知道少算了什么
    Unpriced { model: String },
}

/// 这个价是怎么查到的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Match {
    /// 同一个模型、同一条路 —— 只是名字的写法不同
    Exact,
    /// 只在别的平台的键上找到了价格。**结果一律标成估算**
    CrossPlatform,
}

/// 微分：百万分之一美元。
///
/// **整数，不是浮点。**金额相加是它唯一的用途，而浮点相加一万次之后的
/// 尾差会让「今日花费」和「逐条相加」对不上 —— 那种对不上没法解释。
pub type Micros = i64;

pub fn to_micros(usd: f64) -> Micros {
    (usd * 1_000_000.0).round() as i64
}

#[derive(Debug)]
pub struct Prices {
    table: HashMap<String, ModelPrice>,
    /// 用户覆盖层。**查它优先** —— 中转站的价格只有用户自己知道
    overrides: HashMap<String, Option<ModelPrice>>,
    pub snapshot_date: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PricingError {
    #[error("内置价目表解不开，这是个打包错误：{0}")]
    Snapshot(String),
    #[error("读 {path} 失败：{source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("{path} 不是合法的价格覆盖：{source}")]
    Parse {
        path: String,
        source: serde_yaml_ng::Error,
    },
}

/// 用户的覆盖文件。
///
/// ```yaml
/// models:
///   中转站自己的模型:
///     input: 0.000001
///     output: 0.000002
///   # 删掉一条：写 null，那个模型就变回「没有价格」
///   gpt-4o: null
/// ```
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overrides {
    #[serde(default)]
    pub models: HashMap<String, Option<ModelPrice>>,
}

impl Prices {
    /// 只有内置快照。
    pub fn builtin() -> Result<Self, PricingError> {
        let raw = decompress(SNAPSHOT)?;
        let parsed: HashMap<String, serde_json::Value> =
            serde_json::from_slice(&raw).map_err(|e| PricingError::Snapshot(e.to_string()))?;
        let mut table = HashMap::with_capacity(parsed.len());
        for (k, v) in parsed {
            // `sample_spec` 那种说明用的伪条目没有价格，自然会被滤掉。
            if let Some(p) = price_from(&v) {
                table.insert(k, p);
            }
        }
        Ok(Self {
            table,
            overrides: HashMap::new(),
            snapshot_date: SNAPSHOT_DATE.to_string(),
        })
    }

    /// 一张空表。**每个模型都会是「价格未知」** —— 那是价目表读不了
    /// 时唯一诚实的答案，比退回一个可能过期的内置快照好。
    pub fn empty() -> Self {
        Self {
            table: HashMap::new(),
            overrides: HashMap::new(),
            snapshot_date: "（价目表没能加载）".to_string(),
        }
    }

    /// 叠上用户的覆盖文件。**文件不存在是正常状态**，不是错误。
    pub fn with_overrides(mut self, path: &Path) -> Result<Self, PricingError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(self),
            Err(source) => {
                return Err(PricingError::Io {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        let o: Overrides =
            serde_yaml_ng::from_str(&text).map_err(|source| PricingError::Parse {
                path: path.display().to_string(),
                source,
            })?;
        self.overrides = o.models;
        Ok(self)
    }

    pub fn len(&self) -> usize {
        self.table.len()
    }
    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    /// 查一个模型的价格。
    ///
    /// **覆盖层优先，而且能删。**用户写 `gpt-4o: null` 就是说「我不要这
    /// 条」—— 那不是一个奇怪的需求：一个只走中转的用户，官方价目表里的
    /// 数字对他是错的。
    pub fn get(&self, model: &str) -> Option<&ModelPrice> {
        self.lookup(model).map(|(p, _)| p)
    }

    /// 查价，**并且说清这个价有多可信**。
    fn lookup(&self, model: &str) -> Option<(&ModelPrice, Match)> {
        for c in name::candidates(model) {
            if let Some(o) = self.overrides.get(&c) {
                // 显式删除：命中了但值是 null → 这个模型没有价格
                return o.as_ref().map(|p| (p, Match::Exact));
            }
            if let Some(p) = self.table.get(&c) {
                return Some((p, Match::Exact));
            }
        }
        // 跨平台的最后一招。**它不精确**，见 name::cross_platform_fallback
        let c = name::cross_platform_fallback(model)?;
        if let Some(o) = self.overrides.get(&c) {
            return o.as_ref().map(|p| (p, Match::CrossPlatform));
        }
        self.table.get(&c).map(|p| (p, Match::CrossPlatform))
    }

    /// 算一次调用的成本。
    pub fn cost(&self, model: &str, u: &Usage, estimated: bool) -> Cost {
        let Some((p, m)) = self.lookup(model) else {
            return Cost::Unpriced {
                model: model.to_string(),
            };
        };
        // 价格本身就不精确的话，结果一定是估算 —— 哪怕 usage 是上游给的
        let estimated = estimated || m == Match::CrossPlatform;
        // 长上下文分层：**按这次请求的输入量选档**，不是按模型的窗口。
        let long = u.input > 200_000;
        let input_rate = if long {
            p.input_above_200k.unwrap_or(p.input)
        } else {
            p.input
        };
        let output_rate = if long {
            p.output_above_200k.unwrap_or(p.output)
        } else {
            p.output
        };
        // 缓存读没单独定价时按输入价算 —— 那是数据集里「这家不区分」的
        // 表达方式，不是「免费」。
        let cache_read_rate = p.cache_read.unwrap_or(input_rate);
        let cache_write_rate = if u.cache_1h {
            // 1 小时档没有就退回 5 分钟档，再退回输入价。**退回时只会
            // 低估** —— 所以这条要在文档里说清楚，见 §4.3.0。
            p.cache_write_1h.or(p.cache_write_5m).unwrap_or(input_rate)
        } else {
            p.cache_write_5m.unwrap_or(input_rate)
        };

        let usd = u.input as f64 * input_rate
            + u.output as f64 * output_rate
            + u.cache_read as f64 * cache_read_rate
            + u.cache_write as f64 * cache_write_rate;
        let m = to_micros(usd);
        if estimated {
            Cost::Estimated(m)
        } else {
            Cost::Known(m)
        }
    }
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
    // 没有输入或输出单价的条目不是一个能算钱的模型（嵌入、审核、
    // 以及数据集里那条 `sample_spec` 说明）。
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

    fn prices() -> Prices {
        Prices::builtin().unwrap()
    }

    #[test]
    fn the_builtin_snapshot_loads_and_has_the_models_people_actually_use() {
        let p = prices();
        assert!(p.len() > 1000, "只有 {} 个模型，快照像是坏的", p.len());
        for m in [
            "claude-sonnet-4-5",
            "claude-opus-4-20250514",
            "claude-haiku-4-5",
            "claude-3-5-haiku-20241022",
            "gpt-4o",
        ] {
            assert!(p.get(m).is_some(), "查不到 {m}");
        }
    }

    #[test]
    fn the_one_hour_cache_tier_is_there_and_costs_more_than_five_minutes() {
        // **这是选 LiteLLM 的决定性理由**（§4.3.0）。少了它，开 1 小时
        // TTL 的用户会被系统性低估 60%，而那是个看起来很确定的数字。
        let p = prices();
        let s = p.get("claude-sonnet-4-5").unwrap();
        let five = s.cache_write_5m.expect("没有 5 分钟档");
        let hour = s
            .cache_write_1h
            .expect("没有 1 小时档 —— 那这个数据集就白选了");
        assert!(hour > five, "1 小时档 {hour} 不比 5 分钟档 {five} 贵？");
    }

    #[test]
    fn a_one_hour_cache_write_really_costs_more_than_a_five_minute_one() {
        // 上一条测的是数据在不在，这条测的是**我们真的用了它**。
        let p = prices();
        let u = |h| Usage {
            cache_write: 100_000,
            cache_1h: h,
            ..Default::default()
        };
        let (Cost::Known(five), Cost::Known(hour)) = (
            p.cost("claude-sonnet-4-5", &u(false), false),
            p.cost("claude-sonnet-4-5", &u(true), false),
        ) else {
            panic!("该是 Known");
        };
        assert!(hour > five, "5 分钟 {five} vs 1 小时 {hour}");
        // Sonnet 4.5 是 $3.75 vs $6.00，差 60%
        assert!(
            (hour as f64 / five as f64) > 1.5,
            "差价没对上：{five} → {hour}"
        );
    }

    #[test]
    fn a_model_with_no_price_is_unpriced_not_zero() {
        // **成本三态的第三态。**当成 0 会让总额悄悄偏低，而用户没有任何
        // 线索知道少算了什么（§4.3）。
        let p = prices();
        let c = p.cost("某个中转站自己起的名字", &Usage::default(), false);
        match c {
            Cost::Unpriced { model } => assert_eq!(model, "某个中转站自己起的名字"),
            other => panic!("该是 Unpriced，实际 {other:?}"),
        }
    }

    #[test]
    fn an_estimated_usage_yields_an_estimated_cost() {
        // 估算值不能混进「今日花费」的精确数字里假装准确（§4.3）。
        let p = prices();
        let u = Usage {
            input: 1000,
            output: 100,
            ..Default::default()
        };
        assert!(matches!(
            p.cost("claude-sonnet-4-5", &u, true),
            Cost::Estimated(_)
        ));
        assert!(matches!(
            p.cost("claude-sonnet-4-5", &u, false),
            Cost::Known(_)
        ));
    }

    #[test]
    fn a_long_context_request_uses_the_above_200k_rate() {
        let p = prices();
        let per_token = |n: u64| {
            let Cost::Known(m) = p.cost(
                "claude-sonnet-4-5",
                &Usage {
                    input: n,
                    ..Default::default()
                },
                false,
            ) else {
                panic!()
            };
            m as f64 / n as f64
        };
        let short = per_token(100_000);
        let long = per_token(300_000);
        assert!(
            long > short * 1.5,
            "超过 200k 之后单价没换档：{short} → {long}"
        );
    }

    #[test]
    fn the_dated_name_a_client_actually_sends_resolves_to_a_price() {
        // Claude Code 发的是带日期的那种。查不到的话，**每一个真实请求
        // 都会变成「没有价格」** —— 而那时成本面板整个是空的。
        let p = prices();
        for m in [
            "claude-sonnet-4-5-20250929",
            "claude-3-5-haiku-20241022",
            "anthropic/claude-sonnet-4-5",
        ] {
            assert!(p.get(m).is_some(), "查不到 {m} —— 真实流量用的就是这种名字");
        }
    }

    #[test]
    fn costs_are_integers_so_that_a_thousand_of_them_add_up_exactly() {
        // 浮点相加一万次之后的尾差会让「今日花费」和「逐条相加」对不上，
        // 而那种对不上没有任何办法解释给用户听。
        let p = prices();
        let u = Usage {
            input: 1234,
            output: 567,
            ..Default::default()
        };
        let Cost::Known(one) = p.cost("claude-sonnet-4-5", &u, false) else {
            panic!()
        };
        let total: i64 = (0..10_000).map(|_| one).sum();
        assert_eq!(total, one * 10_000);
    }

    #[test]
    fn a_user_override_wins_over_the_builtin_table() {
        // **中转站的价格和官方不同，而且没有任何公开数据集会收录它们。**
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("pricing.yaml");
        std::fs::write(
            &f,
            "models:\n  claude-sonnet-4-5:\n    input: 0.000001\n    output: 0.000002\n",
        )
        .unwrap();
        let p = prices().with_overrides(&f).unwrap();
        let s = p.get("claude-sonnet-4-5").unwrap();
        assert_eq!(s.input, 0.000001);
        assert_eq!(s.output, 0.000002);
        // 没覆盖的模型不受影响
        assert!(p.get("gpt-4o").unwrap().input > 0.0);
    }

    #[test]
    fn an_override_can_delete_a_price_so_it_becomes_unpriced() {
        // 一个只走中转的用户，官方价目表里的数字对他是错的。**能删是
        // 必需的** —— 而不是逼他填一个自己也不知道的价格。
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("pricing.yaml");
        std::fs::write(&f, "models:\n  claude-sonnet-4-5: null\n").unwrap();
        let p = prices().with_overrides(&f).unwrap();
        assert!(p.get("claude-sonnet-4-5").is_none());
        assert!(matches!(
            p.cost("claude-sonnet-4-5", &Usage::default(), false),
            Cost::Unpriced { .. }
        ));
    }

    #[test]
    fn a_missing_override_file_is_normal_not_an_error() {
        // 绝大多数用户永远不会有这个文件。
        let d = tempfile::tempdir().unwrap();
        let p = prices()
            .with_overrides(&d.path().join("nope.yaml"))
            .unwrap();
        assert!(p.get("claude-sonnet-4-5").is_some());
    }

    #[test]
    fn a_broken_override_file_says_which_file_and_why() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("pricing.yaml");
        std::fs::write(&f, "models:\n  x:\n    inptu: 1\n").unwrap();
        let e = prices().with_overrides(&f).unwrap_err();
        let m = e.to_string();
        assert!(m.contains("pricing.yaml"), "{m}");
        assert!(m.contains("inptu"), "得说清是哪个字段写错了：{m}");
    }

    #[test]
    fn the_snapshot_date_is_available_because_it_has_to_be_shown() {
        // 一个两个月前的价目表算出来的数字，和一个昨天的，可信度完全
        // 不同 —— 而用户没有别的办法知道这件事（§4.3.0）。
        assert_eq!(prices().snapshot_date, SNAPSHOT_DATE);
        assert_eq!(SNAPSHOT_DATE.len(), 10, "日期得是 YYYY-MM-DD");
    }

    #[test]
    fn cache_reads_are_much_cheaper_than_fresh_input() {
        // 这是整个 prompt cache 论证的数字基础（§3.4：命中与否成本差
        // 5 到 10 倍）。数据集要是把它记反了，我们所有关于缓存的建议
        // 都是错的。
        let p = prices();
        let s = p.get("claude-sonnet-4-5").unwrap();
        let read = s.cache_read.expect("没有缓存读价格");
        assert!(
            read < s.input / 5.0,
            "缓存读 {read} 没比输入 {} 便宜多少",
            s.input
        );
    }
}

#[cfg(test)]
mod snapshot_invariants {
    use super::*;

    /// **这条测试是一条别名规则的全部依据。**
    ///
    /// `name::candidates` 最后会试一次 `anthropic.<名字>-v1:0`，因为数据
    /// 集里有些模型（比如 `claude-3-5-haiku-20241022`）只有那个键。那一
    /// 步成立的前提是「Bedrock 的单价和直连一样」—— 而那不是我们能假设
    /// 的事，只能是观察到的事实。
    ///
    /// 所以这里把它变成一条覆盖整份快照的检查：**凡是两种写法都在的模
    /// 型，四个价格必须逐个相等**。哪天上游的数据不再满足它，这条会先响，
    /// 而不是等用户发现账单对不上。
    #[test]
    fn bedrock_and_direct_prices_agree_on_the_headline_rates_but_not_on_cache() {
        let p = Prices::builtin().unwrap();
        let mut checked = 0;
        let mut differed = 0;
        for (k, direct) in &p.table {
            if k.contains('/') || k.contains('.') {
                continue;
            }
            let Some(bedrock) = p.table.get(&format!("anthropic.{k}-v1:0")) else {
                continue;
            };
            checked += 1;
            // 输入输出价是一样的
            assert_eq!(
                (direct.input, direct.output),
                (bedrock.input, bedrock.output),
                "`{k}` 连输入输出价都不一样了 —— 那条跨平台兜底连「估算」都算不上了"
            );
            // 但缓存价**不一定**一样。`claude-3-haiku-20240307` 就差 17%
            if direct.cache_read != bedrock.cache_read {
                differed += 1;
            }
        }
        assert!(checked >= 5, "只对上了 {checked} 个模型，样本太少");
        assert!(
            differed > 0,
            "缓存价竟然全都一样了？那 name.rs 里那段「所以标成估算」的理由要重写"
        );
    }

    /// Vertex 的价格**确实**不一样 —— 这条钉住那个反例，免得哪天有人
    /// 顺手把别名规则推广到所有前缀上。
    #[test]
    fn vertex_prices_differ_which_is_why_the_alias_rule_is_anthropic_only() {
        let p = Prices::builtin().unwrap();
        let (Some(v), Some(b)) = (
            p.table.get("vertex_ai/claude-3-5-haiku"),
            p.table.get("anthropic.claude-3-5-haiku-20241022-v1:0"),
        ) else {
            // 上游改了键名的话这条自动跳过 —— 它是个警示，不是硬约束
            return;
        };
        assert_ne!(v.input, b.input, "Vertex 和 Bedrock 的价格竟然一样了？");
    }

    /// 快照里那几个主力模型的绝对价格。
    ///
    /// **和厂商官方定价页对过**（§4.3.0 的发版前交叉校验）。数字变了要
    /// 人来确认，而不是跟着上游悄悄改 —— 一个自动跟随的数字，出错时没有
    /// 任何人会发现。
    #[test]
    fn the_headline_models_cost_what_the_vendor_says_they_cost() {
        let p = Prices::builtin().unwrap();
        // (模型, 输入/百万 token, 输出/百万 token)
        for (m, input_per_m, output_per_m) in [
            ("claude-sonnet-4-5", 3.0, 15.0),
            ("claude-opus-4-20250514", 15.0, 75.0),
            ("claude-haiku-4-5", 1.0, 5.0),
        ] {
            let x = p.get(m).unwrap_or_else(|| panic!("查不到 {m}"));
            assert!(
                (x.input * 1e6 - input_per_m).abs() < 1e-9,
                "{m} 输入价是 {}，期望 {input_per_m}",
                x.input * 1e6
            );
            assert!(
                (x.output * 1e6 - output_per_m).abs() < 1e-9,
                "{m} 输出价是 {}，期望 {output_per_m}",
                x.output * 1e6
            );
        }
    }
}

#[cfg(test)]
mod cross_platform_tests {
    use super::*;

    #[test]
    fn a_model_only_bedrock_knows_about_gets_a_price_but_marked_as_estimated() {
        // `claude-3-5-haiku-20241022` 是 Claude Code 真的会发的模型，而
        // 数据集里只有它的 Bedrock 键。**给一个带波浪号的数字，比给一个
        // 空白有用**；而假装它精确，就是那种「看起来很确定的错数字」。
        let p = Prices::builtin().unwrap();
        assert!(p.get("claude-3-5-haiku-20241022").is_some());
        let c = p.cost(
            "claude-3-5-haiku-20241022",
            &Usage {
                input: 1000,
                output: 100,
                ..Default::default()
            },
            // 注意这里传的是 false（usage 是上游给的），结果仍然是估算
            false,
        );
        match c {
            Cost::Estimated(m) => assert!(m > 0),
            other => panic!("跨平台查到的价格必须标成估算，实际 {other:?}"),
        }
    }

    #[test]
    fn a_model_with_a_direct_price_is_not_downgraded_to_estimated() {
        let p = Prices::builtin().unwrap();
        assert!(matches!(
            p.cost(
                "claude-sonnet-4-5",
                &Usage {
                    input: 1000,
                    ..Default::default()
                },
                false
            ),
            Cost::Known(_)
        ));
    }

    #[test]
    fn the_fallback_does_not_fire_for_non_anthropic_models() {
        // 按名字根本分不出用户走的是哪条路，而 Vertex 的价格差 25% ——
        // 分不出的时候就别猜。
        assert!(name::cross_platform_fallback("gpt-4o").is_none());
        assert!(name::cross_platform_fallback("gemini-2.5-pro").is_none());
        assert_eq!(
            name::cross_platform_fallback("claude-x"),
            Some("anthropic.claude-x-v1:0".to_string())
        );
    }

    #[test]
    fn a_user_override_still_wins_over_the_cross_platform_fallback() {
        // 中转站的价格是用户写的，它比我们从别的平台推来的可信得多。
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("pricing.yaml");
        std::fs::write(
            &f,
            "models:\n  claude-3-5-haiku-20241022:\n    input: 0.000009\n    output: 0.00001\n",
        )
        .unwrap();
        let p = Prices::builtin().unwrap().with_overrides(&f).unwrap();
        assert_eq!(p.get("claude-3-5-haiku-20241022").unwrap().input, 0.000009);
        assert!(matches!(
            p.cost(
                "claude-3-5-haiku-20241022",
                &Usage {
                    input: 1,
                    ..Default::default()
                },
                false
            ),
            Cost::Known(_),
        ));
    }
}
