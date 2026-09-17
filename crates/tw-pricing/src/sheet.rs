//! 自定义价目表：写在 config.yaml 的 `pricing` 段里。
//!
//! ```yaml
//! pricing:
//!   auto_update: false        # 不写就是开
//!   sheets:
//!     - name: 中转协议价
//!       multiplier: 0.8       # 作用于默认价目表的全部单价
//!       models:               # 单独覆盖的模型，不受倍率影响
//!         claude-sonnet-4-5-thinking:
//!           input: 3
//!           output: 15
//!           cache_read: 0.3
//!           cache_write_5m: 3.75
//!           cache_write_1h: 6
//!
//! providers:
//!   - name: relay-hk
//!     pricing: 中转协议价      # 不写就按默认价目表
//! ```
//!
//! **单位是每百万 tokens 的美元**，和厂商定价页上印的一样。让用户写
//! `0.000003` 是在要求他做一次换算，而换算是会错的 —— 差一个数量级的
//! 价格，比没有价格更糟，因为它看起来是个确定的数字。
//!
//! **覆盖价的每个字段都写明数值，计算时不推算。**只写输入输出、其余按
//! 默认价目表的比例补全的做法有两个问题：默认价目表会定期刷新，一次刷新
//! 就能让用户亲手设的覆盖价悄悄变掉；而文件里写着的数字也不再是实际计费
//! 的数字。按比例预填是界面在新建覆盖时做的一次性的事，写进文件的是结果。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ModelPrice;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PricingConfig {
    /// 定期联网刷新默认价目表。**默认开**，和应用自己的更新检查一样
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub auto_update: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sheets: Vec<SheetDef>,
}

impl Default for PricingConfig {
    fn default() -> Self {
        Self {
            auto_update: true,
            sheets: Vec::new(),
        }
    }
}

fn yes() -> bool {
    true
}

fn is_yes(v: &bool) -> bool {
    *v
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SheetDef {
    pub name: String,
    /// 作用于默认价目表的**全部**单价，包括缓存与长上下文单价
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub multiplier: f64,
    /// 单独覆盖的模型。**覆盖价不受倍率影响** —— 它就是那个价
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, PerMillion>,
}

fn one() -> f64 {
    1.0
}

fn is_one(v: &f64) -> bool {
    *v == 1.0
}

/// 一个模型的单价，**每百万 tokens 的美元**。
///
/// 输入、输出和三档缓存单价必填。长上下文单价**成对**写或者都不写 ——
/// 不写就是不分档，多长的请求都按上面那组单价算。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerMillion {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    /// 单次请求输入超过 200K tokens 之后的单价
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_above_200k: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_above_200k: Option<f64>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SheetError {
    #[error("存在未填写名称的价目表")]
    EmptyName,
    #[error("价目表名称「{0}」首尾不能包含空白")]
    PaddedName(String),
    #[error("价目表名称重复：{0}")]
    DuplicateName(String),
    #[error("价目表「{sheet}」的倍率 {value} 无效，倍率必须大于 0")]
    BadMultiplier { sheet: String, value: f64 },
    #[error("价目表「{sheet}」中存在未填写名称的模型")]
    EmptyModel { sheet: String },
    #[error("价目表「{sheet}」中模型 {model} 的{field}为 {value}，单价不能为负数")]
    BadPrice {
        sheet: String,
        model: String,
        field: &'static str,
        value: f64,
    },
    #[error("价目表「{sheet}」中模型 {model} 的长上下文输入单价与输出单价需要同时填写")]
    HalfLongContext { sheet: String, model: String },
}

impl PricingConfig {
    /// **负价不是「便宜」，是写错了。**让它进去的话，`cheapest` 会永远
    /// 选中那一家，而费用栏会往下走。
    pub fn validate(&self) -> Result<(), SheetError> {
        let mut seen = std::collections::HashSet::new();
        for s in &self.sheets {
            s.validate()?;
            if !seen.insert(s.name.as_str()) {
                return Err(SheetError::DuplicateName(s.name.clone()));
            }
        }
        Ok(())
    }

    pub fn sheet(&self, name: &str) -> Option<&SheetDef> {
        self.sheets.iter().find(|s| s.name == name)
    }
}

impl SheetDef {
    /// 这一张本身写得对不对。**名字重不重复要看整份配置**，不在这里查。
    pub fn validate(&self) -> Result<(), SheetError> {
        if self.name.trim().is_empty() {
            return Err(SheetError::EmptyName);
        }
        if self.name.trim() != self.name {
            return Err(SheetError::PaddedName(self.name.clone()));
        }
        if !(self.multiplier.is_finite() && self.multiplier > 0.0) {
            return Err(SheetError::BadMultiplier {
                sheet: self.name.clone(),
                value: self.multiplier,
            });
        }
        for (model, p) in &self.models {
            if model.trim().is_empty() {
                return Err(SheetError::EmptyModel {
                    sheet: self.name.clone(),
                });
            }
            for (field, value) in p.fields() {
                if !(value.is_finite() && value >= 0.0) {
                    return Err(SheetError::BadPrice {
                        sheet: self.name.clone(),
                        model: model.clone(),
                        field,
                        value,
                    });
                }
            }
            if p.input_above_200k.is_some() != p.output_above_200k.is_some() {
                return Err(SheetError::HalfLongContext {
                    sheet: self.name.clone(),
                    model: model.clone(),
                });
            }
        }
        Ok(())
    }
}

impl PerMillion {
    fn fields(&self) -> Vec<(&'static str, f64)> {
        let mut out = vec![
            ("输入单价", self.input),
            ("输出单价", self.output),
            ("缓存读取单价", self.cache_read),
            ("缓存写入（5 分钟）单价", self.cache_write_5m),
            ("缓存写入（1 小时）单价", self.cache_write_1h),
        ];
        for (name, v) in [
            ("长上下文输入单价", self.input_above_200k),
            ("长上下文输出单价", self.output_above_200k),
        ] {
            if let Some(v) = v {
                out.push((name, v));
            }
        }
        out
    }

    /// 换成每 token 的价格。**按写的算，一个字段都不推** —— 见模块注释。
    ///
    /// 上下文窗口不是价格，从默认价目表里这个模型那儿取。
    pub(crate) fn to_price(&self, max_input_tokens: Option<u64>) -> ModelPrice {
        const M: f64 = 1e-6;
        ModelPrice {
            input: self.input * M,
            output: self.output * M,
            cache_read: Some(self.cache_read * M),
            cache_write_5m: Some(self.cache_write_5m * M),
            cache_write_1h: Some(self.cache_write_1h * M),
            input_above_200k: self.input_above_200k.map(|v| v * M),
            output_above_200k: self.output_above_200k.map(|v| v * M),
            max_input_tokens,
        }
    }

    /// 一个价格**实际按什么单价计费**，换成每百万 tokens。
    ///
    /// 数据集里没单独定价的缓存档按计费时的同一套规则补上（见
    /// [`ModelPrice::rates`]）—— 显示出来的数字和记账用的必须是同一个。
    pub fn of(p: &ModelPrice) -> Self {
        const M: f64 = 1e6;
        // 浮点换算会带出 `2.9999999999999996` 这种尾巴 —— 这个数要显示给人看，
        // 也可能被写回配置。十位小数足够表示任何真实的单价
        let r = |v: f64| (v * M * 1e10).round() / 1e10;
        let rates = p.rates(false);
        Self {
            input: r(rates.input),
            output: r(rates.output),
            cache_read: r(rates.cache_read),
            cache_write_5m: r(rates.cache_write_5m),
            cache_write_1h: r(rates.cache_write_1h),
            input_above_200k: p.input_above_200k.map(r),
            output_above_200k: p.output_above_200k.map(r),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(y: &str) -> PricingConfig {
        serde_yaml_ng::from_str(y).unwrap()
    }

    const FULL: &str =
        "input: 3, output: 15, cache_read: 0.3, cache_write_5m: 3.75, cache_write_1h: 6";

    #[test]
    fn nothing_written_means_auto_update_on_and_no_sheets() {
        let c = cfg("{}");
        assert!(c.auto_update);
        assert!(c.sheets.is_empty());
        // 默认值不写回文件
        assert_eq!(serde_yaml_ng::to_string(&c).unwrap().trim(), "{}");
    }

    #[test]
    fn a_misspelled_price_field_is_an_error_not_a_silent_zero() {
        let e = serde_yaml_ng::from_str::<PricingConfig>(
            "sheets:\n  - name: a\n    models:\n      m: { inptu: 1, output: 2 }\n",
        )
        .unwrap_err();
        assert!(e.to_string().contains("inptu"), "{e}");
    }

    #[test]
    fn an_override_has_to_spell_out_the_cache_prices() {
        // 只写输入输出的话，缓存读按什么算就成了一个隐藏的决定
        let e = serde_yaml_ng::from_str::<PricingConfig>(
            "sheets:\n  - name: a\n    models:\n      m: { input: 1, output: 2 }\n",
        )
        .unwrap_err();
        assert!(e.to_string().contains("cache_read"), "{e}");
    }

    #[test]
    fn a_negative_price_a_zero_multiplier_and_half_a_long_context_tier_are_refused() {
        let e = cfg(
            "sheets:\n  - name: a\n    models:\n      m: { input: 3, output: 15, cache_read: -0.3, cache_write_5m: 3.75, cache_write_1h: 6 }\n",
        )
        .validate()
        .unwrap_err();
        assert!(
            matches!(
                e,
                SheetError::BadPrice {
                    field: "缓存读取单价",
                    ..
                }
            ),
            "{e}"
        );
        let e = cfg("sheets:\n  - name: a\n    multiplier: 0\n")
            .validate()
            .unwrap_err();
        assert!(matches!(e, SheetError::BadMultiplier { .. }), "{e}");
        let e = cfg("sheets:\n  - name: a\n  - name: a\n")
            .validate()
            .unwrap_err();
        assert_eq!(e, SheetError::DuplicateName("a".into()));
        let e = cfg(&format!(
            "sheets:\n  - name: a\n    models:\n      m: {{ {FULL}, input_above_200k: 6 }}\n"
        ))
        .validate()
        .unwrap_err();
        assert!(matches!(e, SheetError::HalfLongContext { .. }), "{e}");
    }

    #[test]
    fn an_override_is_taken_exactly_as_written() {
        let p = PerMillion {
            input: 2.0,
            output: 10.0,
            cache_read: 0.25,
            cache_write_5m: 2.0,
            cache_write_1h: 2.0,
            input_above_200k: None,
            output_above_200k: None,
        }
        .to_price(Some(200_000));
        let per_m = |v: Option<f64>| v.map(|x| (x * 1e6 * 1e6).round() / 1e6);
        assert_eq!(per_m(p.cache_read), Some(0.25));
        assert_eq!(per_m(p.cache_write_5m), Some(2.0));
        // 没写长上下文单价就是不分档
        assert_eq!(p.input_above_200k, None);
        assert_eq!(p.max_input_tokens, Some(200_000));
    }

    #[test]
    fn the_displayed_rates_are_the_rates_that_get_charged() {
        // 数据集里没单独定价的缓存档：按输入价计费，也就按输入价显示
        let p = ModelPrice {
            input: 1e-6,
            output: 2e-6,
            cache_write_5m: Some(1.25e-6),
            ..Default::default()
        };
        let v = PerMillion::of(&p);
        assert_eq!(v.cache_read, 1.0);
        assert_eq!(v.cache_write_5m, 1.25);
        // 1 小时档没有就按 5 分钟档
        assert_eq!(v.cache_write_1h, 1.25);
        // 显示的和写回去的是同一组数：换算一个来回不变
        assert_eq!(PerMillion::of(&v.to_price(None)), v);
    }
}
