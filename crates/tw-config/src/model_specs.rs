//! 手写的模型规格：某家上游的某个模型的上下文窗口和输出上限。
//!
//! ```yaml
//! providers:
//!   - name: relay
//!     base_url: https://relay.example.com/v1
//!     model_specs:
//!       glm-5-air: { context_window: 128000, max_output_tokens: 16384 }
//! ```
//!
//! 价目表里没有的模型（中转站自己的名字）说不出上下文窗口，价目表写错的也有。手写的
//! **只管这一家的这一个模型**（名字要完全相等，没有通配），写了就优先于价目表。
//!
//! **哪个数优先只在 [`resolve`] 里定一次。**列模型（`/v1/models` 的各种格式）、上游页的
//! 模型清单、别名、管线里「输入超出了上下文」、转换到 Anthropic 时补的输出上限，全都
//! 经它取数 —— 哪一处另起炉灶，哪一处就会忘了手写的那个数。

use serde::{Deserialize, Serialize};

use crate::{Config, Provider};

/// `providers[].model_specs` 的一项。两项都可选，但至少写一项（校验管）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSpec {
    /// 一次最多输入多少 token，也就是上下文窗口
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    /// 一次最多输出多少 token
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

impl ModelSpec {
    /// 两项都没写。写进配置里的不许这样（[`crate::ValidationError::ModelSpecEmpty`]）；
    /// 界面交过来两项都空，是要删掉这一项
    pub fn is_empty(&self) -> bool {
        self.context_window.is_none() && self.max_output_tokens.is_none()
    }
}

/// 一个数是从哪儿来的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecSource {
    /// 价目表
    PriceTable,
    /// 这一家的 `model_specs`
    Manual,
}

/// 一个 token 数和它的来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sourced {
    pub tokens: u64,
    pub source: SpecSource,
}

/// 一个模型的上下文窗口和输出上限。**不知道就是 `None`**：编出来的数客户端会照着截断。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelLimits {
    pub context_window: Option<Sourced>,
    pub max_output_tokens: Option<Sourced>,
}

impl ModelLimits {
    pub fn context_window(&self) -> Option<u64> {
        self.context_window.map(|s| s.tokens)
    }

    pub fn max_output_tokens(&self) -> Option<u64> {
        self.max_output_tokens.map(|s| s.tokens)
    }

    /// 不知道哪家上游服务它时：只看默认价目表。
    pub fn priced(book: &tw_pricing::PriceBook, model: &str) -> Self {
        resolve(book, None, None, model)
    }
}

impl Provider {
    /// 这一家的这个模型的上下文窗口和输出上限：手写的优先，没写的取它选的价目表。
    pub fn model_limits(&self, book: &tw_pricing::PriceBook, model: &str) -> ModelLimits {
        resolve(book, Some(&self.name), self.model_specs.get(model), model)
    }
}

impl Config {
    /// 同 [`Provider::model_limits`]，按上游的名字找。配置里没有这一家（刚删掉）时只看
    /// 价目表。
    pub fn model_limits(
        &self,
        book: &tw_pricing::PriceBook,
        provider: &str,
        model: &str,
    ) -> ModelLimits {
        let spec = self
            .providers
            .iter()
            .find(|p| p.name == provider)
            .and_then(|p| p.model_specs.get(model));
        resolve(book, Some(provider), spec, model)
    }
}

/// **唯一定先后的地方**：一项一项看，手写了就用手写的，没写的那一项取价目表。两项各管
/// 各的 —— 只写了上下文窗口，输出上限照样来自价目表。
///
/// 价目表按这一家选的那张查（`provider`），和计价同一个查法；上下文窗口和输出上限在
/// 每张价目表里都取自默认价目表，所以给不给上游通常是同一个数。
fn resolve(
    book: &tw_pricing::PriceBook,
    provider: Option<&str>,
    spec: Option<&ModelSpec>,
    model: &str,
) -> ModelLimits {
    let priced = match provider {
        Some(p) => book.resolve_for(p, model),
        None => book.resolve(None, model),
    };
    let price = priced.as_ref().map(|r| &r.price);
    let pick = |manual: Option<u32>, table: Option<u64>| match manual {
        Some(n) => Some(Sourced {
            tokens: n.into(),
            source: SpecSource::Manual,
        }),
        None => table.map(|n| Sourced {
            tokens: n,
            source: SpecSource::PriceTable,
        }),
    };
    ModelLimits {
        context_window: pick(
            spec.and_then(|s| s.context_window),
            price.and_then(|p| p.max_input_tokens),
        ),
        max_output_tokens: pick(
            spec.and_then(|s| s.max_output_tokens),
            price.and_then(|p| p.max_output_tokens),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> tw_pricing::PriceBook {
        tw_pricing::PriceBook::builtin().unwrap()
    }

    fn relay(specs: &[(&str, ModelSpec)]) -> Provider {
        Provider {
            name: "relay".into(),
            base_url: "https://relay.example.com/v1".into(),
            model_specs: specs.iter().map(|(m, s)| (m.to_string(), *s)).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_spec_written_by_hand_wins_over_the_price_table_field_by_field() {
        let b = book();
        let p = relay(&[(
            "claude-sonnet-4-5",
            ModelSpec {
                context_window: Some(1_000_000),
                max_output_tokens: None,
            },
        )]);
        let l = p.model_limits(&b, "claude-sonnet-4-5");
        assert_eq!(
            l.context_window,
            Some(Sourced {
                tokens: 1_000_000,
                source: SpecSource::Manual
            })
        );
        // 没写的那一项照样取价目表
        assert_eq!(
            l.max_output_tokens,
            Some(Sourced {
                tokens: 64_000,
                source: SpecSource::PriceTable
            })
        );
        // 别的模型、别的上游不受影响
        assert_eq!(
            p.model_limits(&b, "claude-haiku-4-5")
                .context_window
                .map(|s| s.source),
            Some(SpecSource::PriceTable)
        );
        let cfg = Config {
            providers: vec![p, relay(&[])],
            ..Default::default()
        };
        assert_eq!(
            cfg.model_limits(&b, "relay", "claude-sonnet-4-5")
                .context_window(),
            Some(1_000_000)
        );
        assert_eq!(
            ModelLimits::priced(&b, "claude-sonnet-4-5").context_window(),
            Some(200_000)
        );
    }

    #[test]
    fn a_model_the_price_table_does_not_know_has_only_what_was_written() {
        let b = book();
        let p = relay(&[(
            "中转自有模型",
            ModelSpec {
                context_window: None,
                max_output_tokens: Some(8_000),
            },
        )]);
        let l = p.model_limits(&b, "中转自有模型");
        assert_eq!(l.context_window, None);
        assert_eq!(l.max_output_tokens(), Some(8_000));
        // 名字要完全相等：带日期的另一个名字不算
        assert_eq!(
            p.model_limits(&b, "中转自有模型-2026"),
            ModelLimits::default()
        );
        // 配置里没有这一家：只看价目表
        let cfg = Config::default();
        assert_eq!(
            cfg.model_limits(&b, "relay", "中转自有模型"),
            ModelLimits::default()
        );
    }
}
