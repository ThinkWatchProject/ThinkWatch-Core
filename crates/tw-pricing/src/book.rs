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

use crate::{Cost, Micros, ModelPrice, PricingConfig, SheetDef, Table, Usage, name, to_micros};

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
    /// 所有单价乘上同一个倍率。上下文窗口不变。
    pub fn scaled(&self, m: f64) -> ModelPrice {
        ModelPrice {
            input: self.input * m,
            output: self.output * m,
            cache_read: self.cache_read.map(|v| v * m),
            cache_write_5m: self.cache_write_5m.map(|v| v * m),
            cache_write_1h: self.cache_write_1h.map(|v| v * m),
            input_above_200k: self.input_above_200k.map(|v| v * m),
            output_above_200k: self.output_above_200k.map(|v| v * m),
            max_input_tokens: self.max_input_tokens,
            max_output_tokens: self.max_output_tokens,
        }
    }

    /// 实际计费用的每 token 单价。`long`：这次请求落在长上下文那一档。
    ///
    /// **计费和显示都走这里**，数据集里没单独定价的档在这里按它的约定补上：
    /// 缓存读写没有单独价 = 按输入价算（那是「这家不区分」，不是「免费」）；
    /// 1 小时写入没有 = 按 5 分钟写入算（**退回时只会低估**）。
    pub fn rates(&self, long: bool) -> Rates {
        let input = match self.input_above_200k {
            Some(v) if long => v,
            _ => self.input,
        };
        let output = match self.output_above_200k {
            Some(v) if long => v,
            _ => self.output,
        };
        Rates {
            input,
            output,
            cache_read: self.cache_read.unwrap_or(input),
            cache_write_5m: self.cache_write_5m.unwrap_or(input),
            cache_write_1h: self.cache_write_1h.or(self.cache_write_5m).unwrap_or(input),
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
}

impl Rates {
    fn cache_write(&self, one_hour: bool) -> f64 {
        if one_hour {
            self.cache_write_1h
        } else {
            self.cache_write_5m
        }
    }
}

/// 按一个单价算一次调用的成本。
pub(crate) fn cost_of(p: &ModelPrice, u: &Usage, estimated: bool) -> Cost {
    // 长上下文分层：**按这次请求的输入量选档**，不是按模型的窗口。
    let r = p.rates(u.input > 200_000);
    let usd = u.input as f64 * r.input
        + u.output as f64 * r.output
        + u.cache_read as f64 * r.cache_read
        + u.cache_write as f64 * r.cache_write(u.cache_1h);
    let m = to_micros(usd);
    if estimated {
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
/// 没有单独的缓存价时没有节省或溢价 —— 那时它本来就按输入价算。
pub(crate) fn saving_of(p: &ModelPrice, u: &Usage) -> Micros {
    let r = p.rates(u.input + u.cache_read > 200_000);
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
    fn a_long_request_is_charged_at_the_long_context_rates_including_its_cache_reads() {
        // 数据集里 Sonnet 4.5 有长上下文档、但没有单独的长上下文缓存价
        let b = book("{}", &[]);
        let r = b.resolve(None, "claude-sonnet-4-5").unwrap();
        let long = r.price.rates(true);
        assert_eq!(per_m(long.input), 6.0);
        assert_eq!(per_m(long.output), 22.5);
        let short = r.price.rates(false);
        assert_eq!(per_m(short.cache_read), 0.3);
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
