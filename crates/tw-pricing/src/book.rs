//! 价格簿：默认价目表 + 自定义价目表 + 哪个上游用哪张。
//!
//! **全进程只有一份，而且是活的。**网关的 `cheapest`、记账、测速报价、
//! 回放都从同一个 [`Shared`] 里取 —— 以前记账那一层攥着启动时的一份
//! 副本，改了价格要重启才生效，而「成本栏显示的」和「按最便宜选的」
//! 可以是两份不同的表。
//!
//! 生效单价按这个顺序定（对上游 U、模型 M）：
//!
//! 1. U 没有选价目表：默认价目表里 M 的单价。
//! 2. U 选了价目表 P：P 单独覆盖了 M，就用覆盖价；否则默认价目表单价 × P 的倍率。

use std::collections::HashMap;
use std::sync::Arc;

use crate::{
    Cost, LongTier, Micros, ModelPrice, PricingConfig, SheetDef, Table, Usage, name, to_micros,
};

/// 全进程共用的那一份，可以原子地整份换掉。
pub type Shared = Arc<arc_swap::ArcSwap<PriceBook>>;

pub fn shared(book: PriceBook) -> Shared {
    Arc::new(arc_swap::ArcSwap::from_pointee(book))
}

#[derive(Debug)]
pub struct PriceBook {
    table: Arc<Table>,
    config: PricingConfig,
    /// 编译好的价目表：覆盖价已经换算成每 token、缺的字段已经补全
    sheets: HashMap<String, Sheet>,
    /// 上游 → 它选的价目表
    assign: HashMap<String, String>,
}

#[derive(Debug)]
struct Sheet {
    multiplier: f64,
    overrides: HashMap<String, ModelPrice>,
}

impl Sheet {
    fn compile(def: &SheetDef, table: &Table) -> Self {
        Self {
            multiplier: def.multiplier,
            overrides: def
                .models
                .iter()
                .map(|(m, p)| (m.clone(), p.to_price(table.get(m))))
                .collect(),
        }
    }
}

/// 一个价格是从哪儿来的。**每一个金额都要能追溯到它。**
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// 默认价目表
    Default,
    /// 默认价目表 × 某张价目表的倍率
    Scaled { sheet: String, multiplier: f64 },
    /// 某张价目表单独覆盖的
    Override { sheet: String },
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub price: ModelPrice,
    pub source: Source,
    /// 只在别的平台的键上找到了价格。**按它算出来的钱一律标成估算**
    pub cross_platform: bool,
}

impl Resolved {
    /// 用了这么多 token 花了多少。`estimated`：用量是不是估的。
    pub fn cost(&self, usage: &Usage, estimated: bool) -> Cost {
        // 价格本身就不精确的话，结果一定是估算 —— 哪怕 usage 是上游给的
        cost_of(&self.price, usage, estimated || self.cross_platform)
    }

    /// 用了缓存之后净省（或净多花）了多少。见 [`saving_of`]。
    pub fn cache_saving(&self, usage: &Usage) -> Micros {
        if usage.cache_read == 0 && usage.cache_write == 0 {
            return 0;
        }
        saving_of(&self.price, usage)
    }
}

impl PriceBook {
    /// `assign`：上游名 → 价目表名。没有出现的上游按默认价目表。
    pub fn new(
        table: Arc<Table>,
        config: PricingConfig,
        assign: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        let sheets = config
            .sheets
            .iter()
            .map(|s| (s.name.clone(), Sheet::compile(s, &table)))
            .collect();
        Self {
            table,
            config,
            sheets,
            assign: assign.into_iter().collect(),
        }
    }

    /// 只有内置快照、没有自定义价目表。测试和兜底用。
    pub fn builtin() -> Result<Self, crate::PricingError> {
        Ok(Self::new(
            Arc::new(Table::builtin()?),
            PricingConfig::default(),
            [],
        ))
    }

    /// 换一份默认价目表，**自定义价目表按新表重新补全**。
    pub fn with_table(&self, table: Arc<Table>) -> Self {
        Self::new(table, self.config.clone(), self.assign.clone())
    }

    /// 换一份配置（价目表和上游的选择），默认价目表不变。
    pub fn with_config(
        &self,
        config: PricingConfig,
        assign: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self::new(self.table.clone(), config, assign)
    }

    pub fn table(&self) -> &Arc<Table> {
        &self.table
    }

    pub fn config(&self) -> &PricingConfig {
        &self.config
    }

    /// 这个上游选了哪张价目表。`None` = 默认价目表。
    pub fn sheet_of(&self, provider: &str) -> Option<&str> {
        self.assign.get(provider).map(String::as_str)
    }

    /// 按某张价目表查价。`None` 查默认价目表。
    pub fn resolve(&self, sheet: Option<&str>, model: &str) -> Option<Resolved> {
        match sheet {
            Some(n) => self.resolve_in(self.sheets.get(n).map(|s| (n, s)), model),
            None => self.resolve_in(None, model),
        }
    }

    /// 放进一张**还没保存**的价目表，同名的那张被它替换。只给编辑对话框预览用。
    pub fn with_draft(&self, def: SheetDef) -> Self {
        let mut config = self.config.clone();
        config.sheets.retain(|s| s.name != def.name);
        config.sheets.push(def);
        Self::new(self.table.clone(), config, self.assign.clone())
    }

    /// 按这个上游选的价目表查价。
    pub fn resolve_for(&self, provider: &str, model: &str) -> Option<Resolved> {
        self.resolve(self.sheet_of(provider), model)
    }

    fn resolve_in(&self, sheet: Option<(&str, &Sheet)>, model: &str) -> Option<Resolved> {
        if let Some((name, s)) = sheet {
            // **覆盖的键也要按名字归一化来匹配** —— 用户写的是
            // `claude-sonnet-4-5`，客户端发的是带日期的那个
            for c in name::candidates(model) {
                if let Some(p) = s.overrides.get(&c) {
                    return Some(Resolved {
                        price: p.clone(),
                        source: Source::Override {
                            sheet: name.to_string(),
                        },
                        cross_platform: false,
                    });
                }
            }
        }
        let (base, cross_platform) = self.table.lookup(model)?;
        Some(match sheet {
            Some((name, s)) => Resolved {
                price: base.scaled(s.multiplier),
                source: Source::Scaled {
                    sheet: name.to_string(),
                    multiplier: s.multiplier,
                },
                cross_platform,
            },
            None => Resolved {
                price: base.clone(),
                source: Source::Default,
                cross_platform,
            },
        })
    }

    /// 这个上游跑这个模型、用了这么多 token，花了多少。
    pub fn cost_for(&self, provider: &str, model: &str, usage: &Usage, estimated: bool) -> Cost {
        match self.resolve_for(provider, model) {
            Some(r) => r.cost(usage, estimated),
            None => Cost::Unpriced {
                model: model.to_string(),
            },
        }
    }

    /// 这家跑这个模型的 (输入, 输出) 单价，微分/百万 token。给 `cheapest`
    /// 排序用 —— **整数**，因为浮点比较在「两家价钱一样」这种边界上会给出
    /// 不稳定的顺序，而那意味着 prompt cache 白断一次。
    pub fn unit_micros(&self, provider: &str, model: &str) -> Option<(Micros, Micros)> {
        let p = self.resolve_for(provider, model)?.price;
        Some((
            to_micros(p.input * 1_000_000.0),
            to_micros(p.output * 1_000_000.0),
        ))
    }
}

impl ModelPrice {
    /// 所有单价乘上同一个倍率，长上下文那一档也乘。门槛和上下文窗口不变。
    pub fn scaled(&self, m: f64) -> ModelPrice {
        ModelPrice {
            input: self.input * m,
            output: self.output * m,
            cache_read: self.cache_read.map(|v| v * m),
            cache_write_5m: self.cache_write_5m.map(|v| v * m),
            cache_write_1h: self.cache_write_1h.map(|v| v * m),
            long: self.long.as_ref().map(|t| LongTier {
                above: t.above,
                input: t.input * m,
                output: t.output * m,
                cache_read: t.cache_read.map(|v| v * m),
                cache_write_5m: t.cache_write_5m.map(|v| v * m),
                cache_write_1h: t.cache_write_1h.map(|v| v * m),
            }),
            max_input_tokens: self.max_input_tokens,
            max_output_tokens: self.max_output_tokens,
            reasoning: self.reasoning,
        }
    }

    /// 实际计费用的每 token 单价。`prompt`：这次请求的输入一共多少（见
    /// [`Usage::prompt`]），超过长上下文的门槛就按那一档算。
    ///
    /// **计费和显示都走这里**，价目表上没写的单价在这里按下面的规则补上。补出来
    /// 的记在 [`Rates::guessed`] 里，用到它们的费用标成估算：
    ///
    /// - 缓存读写一个单价都没写：按输入价算。那是「这家不区分」，不是「免费」，
    ///   也不是猜的
    /// - 1 小时写入没写、5 分钟的写了：按输入价的 2 倍算。1 小时缓存只有 Claude
    ///   有，这是 Anthropic 公布的倍率 —— 以前退回 5 分钟档的价，会少算三成多
    /// - 长上下文那一档没写缓存价：平档的缓存价按这一档输入价涨的比例放大。各家
    ///   的长上下文都是这么涨的（Claude、Gemini、GPT-5.x 在数据集里都是）
    pub fn rates(&self, prompt: u64) -> Rates {
        let tier = self.long.as_ref().filter(|t| prompt > t.above);
        let (input, output) = match tier {
            Some(t) => (t.input, t.output),
            None => (self.input, self.output),
        };
        // 平档的缓存价放大到这一档：输入价涨了多少倍就涨多少倍
        let grow = |v: f64| {
            if self.input > 0.0 {
                v * input / self.input
            } else {
                v
            }
        };
        // 这一档写了的就用，没写的从平档推
        let pick = |flat: Option<f64>, long: Option<Option<f64>>| match long {
            None => (flat, false),
            Some(Some(v)) => (Some(v), false),
            Some(None) => (flat.map(grow), flat.is_some()),
        };
        let (read, read_guessed) = pick(self.cache_read, tier.map(|t| t.cache_read));
        let (write_5m, write_5m_guessed) =
            pick(self.cache_write_5m, tier.map(|t| t.cache_write_5m));
        let (write_1h, write_1h_guessed) =
            match pick(self.cache_write_1h, tier.map(|t| t.cache_write_1h)) {
                (Some(v), guessed) => (v, guessed),
                (None, _) if write_5m.is_some() => (2.0 * input, true),
                (None, _) => (input, false),
            };
        Rates {
            input,
            output,
            cache_read: read.unwrap_or(input),
            cache_write_5m: write_5m.unwrap_or(input),
            cache_write_1h: write_1h,
            guessed: Guessed {
                cache_read: read_guessed,
                cache_write_5m: write_5m_guessed,
                cache_write_1h: write_1h_guessed,
            },
        }
    }
}

/// 一档的实际单价，每 token 的美元。见 [`ModelPrice::rates`]。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    /// 哪几项是推出来的、不是价目表上写的。**用到了它们的费用标成估算**
    pub guessed: Guessed,
}

/// 推出来的缓存单价。输入输出价从来不推：没有它们就没有这个模型的价格。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Guessed {
    pub cache_read: bool,
    pub cache_write_5m: bool,
    pub cache_write_1h: bool,
}

impl Rates {
    fn cache_write(&self, one_hour: bool) -> f64 {
        if one_hour {
            self.cache_write_1h
        } else {
            self.cache_write_5m
        }
    }

    /// 这些用量用到了推出来的单价吗
    fn guessed_for(&self, u: &Usage) -> bool {
        let g = self.guessed;
        let write = if u.cache_1h {
            g.cache_write_1h
        } else {
            g.cache_write_5m
        };
        (u.cache_read > 0 && g.cache_read) || (u.cache_write > 0 && write)
    }
}

/// 按一个单价算一次调用的成本。
pub(crate) fn cost_of(p: &ModelPrice, u: &Usage, estimated: bool) -> Cost {
    // 长上下文分层：**按这次请求的输入量选档**（连同缓存读写），不是按模型的窗口。
    let r = p.rates(u.prompt());
    let usd = u.input as f64 * r.input
        + u.output as f64 * r.output
        + u.cache_read as f64 * r.cache_read
        + u.cache_write as f64 * r.cache_write(u.cache_1h);
    let m = to_micros(usd);
    if estimated || r.guessed_for(u) {
        Cost::Estimated(m)
    } else {
        Cost::Known(m)
    }
}

/// 用了缓存之后，净多花还是净少花了多少。
///
/// **算的是「如果完全不用缓存，这次要多花多少」** —— 两笔都要算：命中
/// 省下的（读是 0.1 倍单价），以及写入多花的（**写是 1.25 倍单价，不是
/// 免费的**）。所以它可以是负数，而负数是一条结论：这个用法上缓存在亏钱。
///
/// 不用缓存的话这些 token 全是输入，**输入总量不变**，落在哪一档也不变。
///
/// 没有单独的缓存价时没有节省或溢价 —— 那时它本来就按输入价算。
pub(crate) fn saving_of(p: &ModelPrice, u: &Usage) -> Micros {
    let r = p.rates(u.prompt());
    let saved = u.cache_read as f64 * (r.input - r.cache_read);
    let spent = u.cache_write as f64 * (r.cache_write(u.cache_1h) - r.input);
    to_micros(saved - spent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(yaml: &str, assign: &[(&str, &str)]) -> PriceBook {
        let cfg: PricingConfig = serde_yaml_ng::from_str(yaml).unwrap();
        cfg.validate().unwrap();
        PriceBook::new(
            Arc::new(Table::builtin().unwrap()),
            cfg,
            assign.iter().map(|(p, s)| (p.to_string(), s.to_string())),
        )
    }

    const SHEETS: &str = "
sheets:
  - name: 中转协议价
    multiplier: 0.8
    models:
      claude-sonnet-4-5-thinking:
        { input: 3, output: 15, cache_read: 0.3, cache_write_5m: 3.75, cache_write_1h: 6 }
";

    fn per_m(v: f64) -> f64 {
        (v * 1e6 * 1e6).round() / 1e6
    }

    #[test]
    fn an_upstream_without_a_sheet_pays_the_default_price() {
        let b = book(SHEETS, &[("relay-hk", "中转协议价")]);
        let r = b.resolve_for("anthropic", "claude-sonnet-4-5").unwrap();
        assert_eq!(r.source, Source::Default);
        assert_eq!(per_m(r.price.input), 3.0);
    }

    #[test]
    fn a_sheet_scales_every_price_including_the_cache_tiers() {
        // 倍率只乘输入输出的话，缓存占大头的流量会被高估
        let b = book(SHEETS, &[("relay-hk", "中转协议价")]);
        let r = b
            .resolve_for("relay-hk", "claude-sonnet-4-5-20250929")
            .unwrap();
        assert_eq!(
            r.source,
            Source::Scaled {
                sheet: "中转协议价".into(),
                multiplier: 0.8
            }
        );
        assert_eq!(per_m(r.price.input), 2.4);
        assert_eq!(per_m(r.price.output), 12.0);
        assert_eq!(per_m(r.price.cache_read.unwrap()), 0.24);
        assert_eq!(per_m(r.price.cache_write_5m.unwrap()), 3.0);
        assert_eq!(per_m(r.price.cache_write_1h.unwrap()), 4.8);
    }

    #[test]
    fn an_override_is_the_price_and_the_multiplier_does_not_touch_it() {
        let b = book(SHEETS, &[("relay-hk", "中转协议价")]);
        let r = b
            .resolve_for("relay-hk", "claude-sonnet-4-5-thinking")
            .unwrap();
        assert_eq!(
            r.source,
            Source::Override {
                sheet: "中转协议价".into()
            }
        );
        assert_eq!(per_m(r.price.input), 3.0);
        assert_eq!(per_m(r.price.cache_write_1h.unwrap()), 6.0);
    }

    #[test]
    fn an_override_does_not_move_when_the_default_table_is_refreshed() {
        // 覆盖价是用户亲手设的。默认价目表每天都可能刷新，它不能跟着变
        let b = book(SHEETS, &[("relay-hk", "中转协议价")]);
        let fresh = Table::fetched(
            br#"{"claude-sonnet-4-5-thinking":{"input_cost_per_token":9e-6,"output_cost_per_token":9e-5,"cache_read_input_token_cost":9e-7}}"#,
            "2026-09-17".into(),
        )
        .unwrap();
        let b = b.with_table(Arc::new(fresh));
        let r = b
            .resolve_for("relay-hk", "claude-sonnet-4-5-thinking")
            .unwrap();
        assert_eq!(per_m(r.price.input), 3.0);
        assert_eq!(per_m(r.price.cache_read.unwrap()), 0.3);
    }

    #[test]
    fn a_long_request_is_charged_at_the_long_context_rates_including_its_cache() {
        // 数据集里 Sonnet 4.5 的长上下文档连缓存读写都有单独的价
        let b = book("{}", &[]);
        let r = b.resolve(None, "claude-sonnet-4-5").unwrap();
        let long = r.price.rates(200_001);
        assert_eq!(per_m(long.input), 6.0);
        assert_eq!(per_m(long.output), 22.5);
        assert_eq!(per_m(long.cache_read), 0.6);
        assert_eq!(per_m(long.cache_write_5m), 7.5);
        assert_eq!(per_m(long.cache_write_1h), 12.0);
        assert_eq!(long.guessed, Guessed::default());
        // 正好 200K 还是平档：「超过」才换档
        let short = r.price.rates(200_000);
        assert_eq!(per_m(short.input), 3.0);
        assert_eq!(per_m(short.cache_read), 0.3);
    }

    #[test]
    fn the_threshold_counts_cache_reads_and_writes_too() {
        // 一个缓存暖好的长请求：没走缓存的只有几千 token，加上缓存读写过了 200K。
        // **只数没走缓存的那部分，它会被当成短请求按半价记**
        let b = book("{}", &[]);
        let u = Usage {
            input: 5_000,
            cache_read: 190_000,
            cache_write: 10_000,
            output: 1_000,
            cache_1h: false,
        };
        let Cost::Known(m) = b.cost_for("anthropic", "claude-sonnet-4-5", &u, false) else {
            panic!()
        };
        // 5000×6 + 190000×0.6 + 10000×7.5 + 1000×22.5，每百万 token 的美元
        assert_eq!(m, 30_000 + 114_000 + 75_000 + 22_500);
    }

    #[test]
    fn a_one_hour_write_without_its_own_price_is_twice_the_input_and_an_estimate() {
        // Bedrock 上的 Sonnet 4 在数据集里没有 1 小时写入的价
        let b = book("{}", &[]);
        let r = b
            .resolve(None, "us.anthropic.claude-sonnet-4-20250514-v1:0")
            .unwrap();
        assert!(!r.cross_platform, "本名就在数据集里");
        assert_eq!(r.price.cache_write_1h, None);
        assert_eq!(per_m(r.price.rates(0).cache_write_1h), 6.0);
        let one_hour = Usage {
            input: 1_000,
            cache_write: 1_000,
            cache_1h: true,
            ..Default::default()
        };
        assert!(matches!(r.cost(&one_hour, false), Cost::Estimated(_)));
        // 5 分钟的写入有价，照样是精确的
        let five_minutes = Usage {
            cache_1h: false,
            ..one_hour
        };
        assert!(matches!(r.cost(&five_minutes, false), Cost::Known(_)));
    }

    #[test]
    fn a_long_tier_without_cache_prices_grows_the_flat_ones_and_says_so() {
        let t = Table::fetched(
            br#"{"m": {"input_cost_per_token": 1e-6, "output_cost_per_token": 4e-6,
                       "cache_read_input_token_cost": 1e-7,
                       "input_cost_per_token_above_128k_tokens": 2e-6,
                       "output_cost_per_token_above_128k_tokens": 6e-6}}"#,
            "d".into(),
        )
        .unwrap();
        let b = PriceBook::new(Arc::new(t), PricingConfig::default(), []);
        let r = b.resolve(None, "m").unwrap();
        let long = r.price.rates(128_001);
        assert_eq!(per_m(long.cache_read), 0.2, "输入价翻倍，缓存读也翻倍");
        assert!(long.guessed.cache_read);
        // 没用到缓存读的长请求：推出来的那一项没用上，照样精确
        let u = Usage {
            input: 130_000,
            ..Default::default()
        };
        assert!(matches!(r.cost(&u, false), Cost::Known(_)));
        let u = Usage { cache_read: 1, ..u };
        assert!(matches!(r.cost(&u, false), Cost::Estimated(_)));
    }

    #[test]
    fn a_sheet_scales_the_long_context_tier_but_not_its_threshold() {
        let b = book(SHEETS, &[("relay-hk", "中转协议价")]);
        let r = b.resolve_for("relay-hk", "claude-sonnet-4-5").unwrap();
        let long = r.price.long.unwrap();
        assert_eq!(long.above, 200_000);
        assert_eq!(per_m(long.input), 4.8);
        assert_eq!(long.cache_read.map(per_m), Some(0.48));
    }

    #[test]
    fn a_bedrock_profile_missing_from_the_table_borrows_and_is_marked_estimated() {
        let b = book("{}", &[]);
        // 数据集里有这个配置文件自己的键：按它的价，精确
        let r = b
            .resolve(None, "us.anthropic.claude-sonnet-4-5-20250929-v1:0")
            .unwrap();
        assert!(!r.cross_platform);
        assert_eq!(per_m(r.price.input), 3.3, "地域配置文件比全球的贵 10%");
        // 数据集里没有 APAC 的 Sonnet 4.5：借模型本身的价，标成估算
        let r = b
            .resolve(None, "apac.anthropic.claude-sonnet-4-5-20250929-v1:0")
            .unwrap();
        assert!(r.cross_platform);
        assert_eq!(per_m(r.price.input), 3.0);
        // 应用推理配置文件的 ARN 看不出是哪个模型：查不到就说查不到
        assert!(
            b.resolve(
                None,
                "arn:aws:bedrock:us-east-2:123456789012:application-inference-profile/a1b2c3"
            )
            .is_none()
        );
    }

    #[test]
    fn the_same_request_costs_what_each_upstreams_sheet_says() {
        // 这是整件事的目的：按上游设的价格真的进了记账
        let b = book(SHEETS, &[("relay-hk", "中转协议价")]);
        // 十万 token：不到长上下文那一档
        let u = Usage {
            input: 100_000,
            ..Default::default()
        };
        let Cost::Known(official) = b.cost_for("anthropic", "claude-sonnet-4-5", &u, false) else {
            panic!()
        };
        let Cost::Known(relay) = b.cost_for("relay-hk", "claude-sonnet-4-5", &u, false) else {
            panic!()
        };
        assert_eq!(official, 300_000);
        assert_eq!(relay, 240_000);
        let (relay_in, _) = b.unit_micros("relay-hk", "claude-sonnet-4-5").unwrap();
        let (official_in, _) = b.unit_micros("anthropic", "claude-sonnet-4-5").unwrap();
        assert!(relay_in < official_in, "cheapest 看到的也得是同一份价");
    }

    #[test]
    fn a_model_with_no_price_anywhere_is_unpriced_not_zero() {
        let b = book(SHEETS, &[("relay-hk", "中转协议价")]);
        assert!(matches!(
            b.cost_for(
                "relay-hk",
                "某个中转站自己起的名字",
                &Usage::default(),
                false
            ),
            Cost::Unpriced { .. }
        ));
    }

    #[test]
    fn a_new_default_table_recompiles_the_sheets_on_top_of_it() {
        let b = book(SHEETS, &[("relay-hk", "中转协议价")]);
        let fresh = Table::fetched(
            br#"{"claude-sonnet-4-5":{"input_cost_per_token":5e-6,"output_cost_per_token":25e-6}}"#,
            "2026-09-17".into(),
        )
        .unwrap();
        let b = b.with_table(Arc::new(fresh));
        assert_eq!(b.table().date, "2026-09-17");
        let r = b.resolve_for("relay-hk", "claude-sonnet-4-5").unwrap();
        assert_eq!(per_m(r.price.input), 4.0);
    }

    #[test]
    fn a_draft_sheet_is_previewed_without_touching_the_saved_one() {
        let b = book(SHEETS, &[("relay-hk", "中转协议价")]);
        let draft: SheetDef =
            serde_yaml_ng::from_str("name: 中转协议价\nmultiplier: 0.5\n").unwrap();
        let preview = b.with_draft(draft);
        let r = preview
            .resolve(Some("中转协议价"), "claude-sonnet-4-5")
            .unwrap();
        assert_eq!(per_m(r.price.input), 1.5);
        // 草稿里去掉了那条覆盖，而默认价目表里没有这个模型
        assert!(
            preview
                .resolve(Some("中转协议价"), "claude-sonnet-4-5-thinking")
                .is_none()
        );
        // 保存着的那张没变
        let r = b.resolve_for("relay-hk", "claude-sonnet-4-5").unwrap();
        assert_eq!(per_m(r.price.input), 2.4);
    }
}
