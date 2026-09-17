//! 价目表。
//!
//! 菜单栏上那个 `$3.42` 的唯一输入。两层：
//!
//! | 层 | 内容 | 从哪儿来 |
//! |---|---|---|
//! | 默认价目表 | 公开价格数据集（LiteLLM） | 随版本内置一份，联网定期刷新 |
//! | 自定义价目表 | 在默认价目表上设倍率、单独覆盖个别模型 | 用户写在 config.yaml 里 |
//!
//! 上游默认按默认价目表计价，可以改选一张自定义价目表；一张自定义价目表
//! 可以给多个上游用。**中转站的价格和官方不同，而且没有任何公开数据集
//! 会收录它们** —— 第二层是必需的。
//!
//! 默认价目表会变，但**已经记下的金额不会跟着变**：每个请求的花费在它
//! 结束时算好、落库，之后价格更新只影响之后的请求。
//!
//! 贯穿这一层的一条规矩：**算不出来就说算不出来。**没有价格的模型不能
//! 记成 0 —— 那是在撒谎，而一个会撒谎的成本面板不如没有。

mod book;
pub mod name;
mod sheet;
mod table;

use serde::{Deserialize, Serialize};

pub use book::{PriceBook, Rates, Resolved, Shared, Source, shared};
pub use sheet::{PerMillion, PricingConfig, SheetDef, SheetError};
pub use table::{SNAPSHOT_DATE, Table, TableSource, UPDATE_URL};

/// 一个模型的价格。单位一律是**每 token 的美元**，和上游数据集一致。
///
/// 缓存写入分两档不是过度设计：Sonnet 4.5 的 5 分钟档是 \$3.75、1 小时档
/// 是 \$6.00。**只用 5 分钟档会让开了 1 小时 TTL 的用户被系统性低估
/// 60%**，而那是一个看起来很确定的数字。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    /// 一次最多输出多少。转换到必须写 `max_tokens` 的格式（Anthropic）、而客户端
    /// 没写时用它 —— 写大了上游会拒绝，写小了回答会被截断
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
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

/// 成本三态。
#[derive(Debug, Clone, PartialEq)]
pub enum Cost {
    /// 上游给了 usage，价目表里有这个模型 —— 这是个可以相加的数
    Known(Micros),
    /// usage 是我们估的，或者价格是跨平台借来的。**不能混进「今日花费」
    /// 的精确数字里**
    Estimated(Micros),
    /// 价目表里没有这个模型。**不是 0** —— 当成 0 会让总额悄悄偏低，
    /// 而用户没有任何线索知道少算了什么
    Unpriced { model: String },
}

/// 微分：百万分之一美元。
///
/// **整数，不是浮点。**金额相加是它唯一的用途，而浮点相加一万次之后的
/// 尾差会让「今日花费」和「逐条相加」对不上 —— 那种对不上没法解释。
pub type Micros = i64;

pub fn to_micros(usd: f64) -> Micros {
    (usd * 1_000_000.0).round() as i64
}

#[derive(Debug, thiserror::Error)]
pub enum PricingError {
    #[error("内置价目表无法解析：{0}")]
    Snapshot(String),
    #[error("价格数据无法解析：{0}")]
    Dataset(String),
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
        let t = Table::builtin().unwrap();
        let mut checked = 0;
        let mut differed = 0;
        for k in t.models() {
            let direct = t.exact(k).unwrap();
            if k.contains('/') || k.contains('.') {
                continue;
            }
            let Some(bedrock) = t.exact(&format!("anthropic.{k}-v1:0")) else {
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
        let t = Table::builtin().unwrap();
        let (Some(v), Some(b)) = (
            t.exact("vertex_ai/claude-3-5-haiku"),
            t.exact("anthropic.claude-3-5-haiku-20241022-v1:0"),
        ) else {
            // 上游改了键名的话这条自动跳过 —— 它是个警示，不是硬约束
            return;
        };
        assert_ne!(v.input, b.input, "Vertex 和 Bedrock 的价格竟然一样了？");
    }

    /// 快照里那几个主力模型的绝对价格。
    ///
    /// **和厂商官方定价页对过**（发版前交叉校验）。数字变了要
    /// 人来确认，而不是跟着上游悄悄改 —— 一个自动跟随的数字，出错时没有
    /// 任何人会发现。
    #[test]
    fn the_headline_models_cost_what_the_vendor_says_they_cost() {
        let p = Table::builtin().unwrap();
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
        let p = PriceBook::builtin().unwrap();
        assert!(p.table().get("claude-3-5-haiku-20241022").is_some());
        let c = p.cost_for(
            "任意一家",
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
        let p = PriceBook::builtin().unwrap();
        assert!(matches!(
            p.cost_for(
                "任意一家",
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
    fn a_sheet_override_still_wins_over_the_cross_platform_fallback() {
        // 中转站的价格是用户写的，它比我们从别的平台推来的可信得多。
        let cfg: PricingConfig = serde_yaml_ng::from_str(
            "sheets:\n  - name: 中转\n    models:\n      claude-3-5-haiku-20241022: { input: 9, output: 10, cache_read: 0.9, cache_write_5m: 11.25, cache_write_1h: 18 }\n",
        )
        .unwrap();
        let p = PriceBook::new(
            std::sync::Arc::new(Table::builtin().unwrap()),
            cfg,
            [("relay".to_string(), "中转".to_string())],
        );
        assert!(matches!(
            p.cost_for(
                "relay",
                "claude-3-5-haiku-20241022",
                &Usage {
                    input: 1,
                    ..Default::default()
                },
                false
            ),
            Cost::Known(_),
        ));
        // 没选这张价目表的上游仍然是跨平台估算
        assert!(matches!(
            p.cost_for(
                "官方",
                "claude-3-5-haiku-20241022",
                &Usage {
                    input: 1,
                    ..Default::default()
                },
                false
            ),
            Cost::Estimated(_),
        ));
    }
}

#[cfg(test)]
mod verified_tests {
    use std::collections::HashMap;

    use super::*;

    /// 人工核对过的那份（`data/verified.yaml`）。
    #[derive(Debug, serde::Deserialize)]
    struct Verified {
        /// **还没有被人对着定价页核对过。**为真时下面的数字是从快照里
        /// 抄下来的 —— 那条比对测试只能抓住「快照漂了」，抓不住「快照
        /// 一开始就错了」。
        pending_human_verification: bool,
        checked_on: String,
        sources: HashMap<String, String>,
        models: HashMap<String, VerifiedPrice>,
    }

    /// **单位是每百万 token 的美元** —— 和厂商定价页上印的一样。
    /// 内置快照用的是每 token，换算在比对时做：让人核对的时候看的是
    /// 他在页面上看到的那个数。
    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct VerifiedPrice {
        input: f64,
        output: f64,
        cache_read: Option<f64>,
        cache_write_5m: Option<f64>,
        cache_write_1h: Option<f64>,
    }

    fn verified() -> Verified {
        serde_yaml_ng::from_str(include_str!("../data/verified.yaml"))
            .expect("verified.yaml 自己解析不了")
    }

    /// **这是挡住「打包了错数据」的唯一手段**。
    ///
    /// 换快照之后这条红了，说明上游改了价。要做的**不是**改
    /// `verified.yaml` 让它闭嘴，而是打开定价页看一眼 —— 文件头上写了
    /// 完整的三步。
    #[test]
    fn the_builtin_snapshot_matches_what_a_human_checked_against_the_vendor_page() {
        let v = verified();
        let p = PriceBook::builtin().unwrap();
        let mut problems = Vec::new();
        for (name, want) in &v.models {
            let Some(got) = p.table().get(name) else {
                problems.push(format!("`{name}` 在快照里查不到了"));
                continue;
            };
            let mut cmp = |field: &str, want: Option<f64>, got: Option<f64>| {
                let (Some(w), Some(g)) = (want, got) else {
                    if want.is_some() {
                        problems.push(format!("`{name}` 的 {field} 在快照里没有了"));
                    }
                    return;
                };
                // 每百万 token 换算成每 token
                if (g * 1e6 - w).abs() > 1e-9 {
                    problems.push(format!(
                        "`{name}` 的 {field}：快照说 ${:.4}/百万，人工核对的是 ${w:.4}/百万",
                        g * 1e6
                    ));
                }
            };
            cmp("输入", Some(want.input), Some(got.input));
            cmp("输出", Some(want.output), Some(got.output));
            cmp("缓存读", want.cache_read, got.cache_read);
            cmp("缓存写 5 分钟", want.cache_write_5m, got.cache_write_5m);
            cmp("缓存写 1 小时", want.cache_write_1h, got.cache_write_1h);
        }
        assert!(
            problems.is_empty(),
            "内置快照和 verified.yaml 对不上（核对日期 {}）：\n  {}\n\n\
             **不要直接改 verified.yaml 让这条闭嘴。**先去看一眼官方定价页：\n  {}",
            v.checked_on,
            problems.join("\n  "),
            v.sources
                .iter()
                .map(|(k, u)| format!("{k}: {u}"))
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    }

    #[test]
    fn the_verification_covers_enough_models_to_be_worth_something() {
        // 只核对两三个模型的话，这条检查给的是虚假的安心。
        let v = verified();
        assert!(
            v.models.len() >= 5,
            "只核对了 {} 个模型，太少了",
            v.models.len()
        );
        assert!(!v.sources.is_empty(), "得写清是对着哪个页面核对的");
    }

    /// **这条测试在说一句实话，不是在检查什么。**
    ///
    /// 现在的 `verified.yaml` 是从快照里抄下来的，所以上面那条比对只能
    /// 抓住「快照漂了」，抓不住「快照一开始就错了」。而这一点很清楚：
    /// 和另一个第三方数据集对，只是把赌注换个地方押 —— 而和自己对，
    /// 连换地方都没换。
    ///
    /// 发版前必须有人真的打开定价页对一遍，然后把那个标志改掉。这条
    /// 测试的存在是为了让那件事没做的时候，代码里有个地方在说。
    #[test]
    fn the_verification_file_admits_that_no_human_has_checked_it_yet() {
        let v = verified();
        if !v.pending_human_verification {
            // 有人核对过了 —— 那这条就该消失，而不是留着一个空壳
            return;
        }
        assert!(
            v.pending_human_verification,
            "标志改成 false 了，说明有人核对过 —— 那就把这条测试删掉"
        );
    }

    #[test]
    fn the_check_date_is_a_real_date_so_staleness_is_visible() {
        // **一个没有日期的「已核对」等于没核对。**半年前对过一次，和
        // 昨天对过一次，可信度完全不同（同一条道理）。
        let v = verified();
        assert_eq!(v.checked_on.len(), 10, "{}", v.checked_on);
        assert!(
            v.checked_on.chars().all(|c| c.is_ascii_digit() || c == '-'),
            "{}",
            v.checked_on
        );
    }
}

#[cfg(test)]
mod saving_tests {
    use super::*;

    fn saving(p: &PriceBook, model: &str, u: &Usage) -> Option<Micros> {
        p.resolve(None, model).map(|r| r.cache_saving(u))
    }

    #[test]
    fn a_cache_hit_saves_the_difference_not_the_whole_price() {
        // **用户想知道的是那个差额**：cache read 是 0.1 倍单价，所以省下
        // 的是 0.9 倍，不是全部。
        let p = PriceBook::builtin().unwrap();
        let u = Usage {
            cache_read: 100_000,
            ..Default::default()
        };
        let saved = saving(&p, "claude-sonnet-4-5", &u).unwrap();
        // Sonnet 4.5：输入 $3、缓存读 $0.30 → 十万 token 省 $0.27
        assert_eq!(saved, 270_000);
    }

    #[test]
    fn no_cache_reads_means_nothing_saved_which_is_a_real_zero() {
        let p = PriceBook::builtin().unwrap();
        assert_eq!(saving(&p, "claude-sonnet-4-5", &Usage::default()), Some(0));
    }

    #[test]
    fn the_write_premium_is_subtracted_not_ignored() {
        // **这条是这个函数改过一次的理由。**缓存写是 1.25 倍单价：
        // 只算读省下的、不减写多花的，等于声称缓存永远只会让人省钱。
        let p = PriceBook::builtin().unwrap();
        let read_only = saving(
            &p,
            "claude-sonnet-4-5",
            &Usage {
                cache_read: 100_000,
                ..Default::default()
            },
        )
        .unwrap();
        let with_writes = saving(
            &p,
            "claude-sonnet-4-5",
            &Usage {
                cache_read: 100_000,
                cache_write: 100_000,
                ..Default::default()
            },
        )
        .unwrap();
        // Sonnet 4.5：输入 $3、写 $3.75 → 十万 token 多花 $0.075
        assert_eq!(read_only - with_writes, 75_000);
    }

    #[test]
    fn a_cache_that_never_gets_read_is_a_loss_and_says_so() {
        // **负数是一条结论**，不是一个要被夹到零的边界：这个用法上
        // 缓存在亏钱，而那正是最该让人看见的一种情况。
        let p = PriceBook::builtin().unwrap();
        let v = saving(
            &p,
            "claude-sonnet-4-5",
            &Usage {
                cache_write: 200_000,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(v, -150_000, "写二十万、一次没读到，多花 $0.15");
    }

    /// 回本线：读量到写量的约 28% 就打平。
    #[test]
    fn reading_back_a_third_of_what_was_written_already_pays_for_it() {
        let p = PriceBook::builtin().unwrap();
        let v = saving(
            &p,
            "claude-sonnet-4-5",
            &Usage {
                cache_write: 100_000,
                cache_read: 33_000,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(v > 0, "读回三分之一就该是赚的，实际 {v}");
    }

    #[test]
    fn an_unpriced_model_cannot_say_how_much_was_saved() {
        // **「省了 0 元」和「算不出来省了多少」是两句不同的话**。
        let p = PriceBook::builtin().unwrap();
        let u = Usage {
            cache_read: 1000,
            ..Default::default()
        };
        assert_eq!(saving(&p, "某个中转站的模型", &u), None);
    }

    #[test]
    fn a_long_context_cache_hit_saves_more_because_the_full_rate_is_higher() {
        // 超过 200k 之后输入单价翻倍，那时缓存命中省下的也更多。
        let p = PriceBook::builtin().unwrap();
        let short = saving(
            &p,
            "claude-sonnet-4-5",
            &Usage {
                cache_read: 100_000,
                ..Default::default()
            },
        )
        .unwrap();
        let long = saving(
            &p,
            "claude-sonnet-4-5",
            &Usage {
                input: 150_000,
                cache_read: 100_000,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            long > short,
            "长上下文的缓存命中省得更多：{short} vs {long}"
        );
    }
    #[test]
    fn an_unknown_model_has_no_unit_price_rather_than_a_made_up_one() {
        let p = PriceBook::builtin().unwrap();
        assert_eq!(p.unit_micros("x", "完全没见过的模型"), None);
    }
}
