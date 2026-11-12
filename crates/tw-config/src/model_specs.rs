//! 手写的模型规格：某家上游的某个模型的上下文窗口、输出上限、会不会推理、收不收图。
//!
//! ```yaml
//! providers:
//!   - name: relay
//!     base_url: https://relay.example.com/v1
//!     model_specs:
//!       glm-5-air: { context_window: 128000, max_output_tokens: 16384, reasoning: true }
//! ```
//!
//! 价目表里没有的模型（中转站自己的名字）说不出这几项，价目表写错的也有。手写的
//! **只管这一家的这一个模型**（名字要完全相等，没有通配），写了就优先于价目表。
//!
//! **哪个值优先只在 [`resolve`] 里定一次。**列模型（`/v1/models` 的各种格式）、上游页的
//! 模型清单、别名、管线里「输入超出了上下文」、转换到 Anthropic 时补的输出上限、测速
//! 给推理留的额度，全都经它取值 —— 哪一处另起炉灶，哪一处就会忘了手写的那个值。

use serde::{Deserialize, Serialize};

use crate::{Config, Provider};
pub use tw_api::{ReasoningEffort, ReasoningLevel};

/// Request-side reasoning-history capability. Independent of the API protocol.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningHistory {
    #[default]
    Auto,
    ReasoningContent,
    Reasoning,
    ContentBlocks,
    Disabled,
}

/// `providers[].model_specs` 的一项。每项都可选，但至少写一项（校验管）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSpec {
    /// Override this model's request-side reasoning-history format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_history: Option<ReasoningHistory>,
    /// 一次最多输入多少 token，也就是上下文窗口
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    /// 一次最多输出多少 token
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// 会不会推理
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    /// 整组覆盖上游档位。非空表示支持推理，空列表明确表示不可调。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// 收不收图
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_input: Option<bool>,
}

impl ModelSpec {
    /// 一项都没写。写进配置里的不许这样（[`crate::ValidationError::ModelSpecEmpty`]）；
    /// 界面交过来全空，是要删掉这一项
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// 一个数是从哪儿来的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecSource {
    /// 价目表
    PriceTable,
    /// 这一家的 `model_specs`
    Manual,
    /// 上游模型目录
    Upstream,
}

/// 只含上游实际报告的规格；未知字段保持 None，不从模型名字推测。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedSpec {
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub reasoning: Option<bool>,
    pub image_input: Option<bool>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// 一个值和它的来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sourced<T> {
    pub value: T,
    pub source: SpecSource,
}

/// 一个模型的规格，每一项连同来源。**不知道就是 `None`**：编出来的数客户端会照着截断，
/// 编出来的「不会推理」客户端会照着不让选推理档。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedSpec {
    pub context_window: Option<Sourced<u64>>,
    pub max_output_tokens: Option<Sourced<u64>>,
    pub reasoning: Option<Sourced<bool>>,
    pub image_input: Option<Sourced<bool>>,
    pub reasoning_effort: Option<Sourced<ReasoningEffort>>,
}

impl ResolvedSpec {
    pub fn context_window(&self) -> Option<u64> {
        self.context_window.map(|s| s.value)
    }

    pub fn max_output_tokens(&self) -> Option<u64> {
        self.max_output_tokens.map(|s| s.value)
    }

    pub fn reasoning(&self) -> Option<bool> {
        self.reasoning.map(|s| s.value)
    }

    pub fn image_input(&self) -> Option<bool> {
        self.image_input.map(|s| s.value)
    }

    /// 不知道哪家上游服务它时：只看默认价目表。
    pub fn priced(book: &tw_pricing::PriceBook, model: &str) -> Self {
        resolve(book, None, None, model)
    }
}

impl Provider {
    /// 这一家的这个模型的规格：手写的优先，没写的取它选的价目表。
    pub fn model_spec(&self, book: &tw_pricing::PriceBook, model: &str) -> ResolvedSpec {
        resolve(book, Some(&self.name), self.model_specs.get(model), model)
    }

    /// 客户端目录和控制面共用：手动 > 上游 > 价目表；价目表没有档位。
    pub fn model_spec_with_metadata(
        &self,
        book: &tw_pricing::PriceBook,
        model: &str,
        reported: Option<&ReportedSpec>,
    ) -> ResolvedSpec {
        let mut out = self.model_spec(book, model);
        if let Some(reported) = reported {
            fn reported_over_table<T>(slot: &mut Option<Sourced<T>>, value: Option<T>) {
                if !slot
                    .as_ref()
                    .is_some_and(|s| s.source == SpecSource::Manual)
                    && let Some(value) = value
                {
                    *slot = Some(Sourced {
                        value,
                        source: SpecSource::Upstream,
                    });
                }
            }
            reported_over_table(&mut out.context_window, reported.context_window);
            reported_over_table(&mut out.max_output_tokens, reported.max_output_tokens);
            reported_over_table(
                &mut out.reasoning,
                reported.reasoning.or_else(|| {
                    reported
                        .reasoning_effort
                        .as_ref()
                        .filter(|effort| !effort.supported_levels.is_empty())
                        .map(|_| true)
                }),
            );
            reported_over_table(&mut out.image_input, reported.image_input);
            reported_over_table(&mut out.reasoning_effort, reported.reasoning_effort.clone());
        }
        // 禁止推理时不提供可调档位；空档位本身不表示模型不会推理。
        if let Some(reasoning) = out.reasoning.filter(|s| !s.value) {
            out.reasoning_effort = Some(Sourced {
                value: ReasoningEffort::default(),
                source: reasoning.source,
            });
        }
        if let Some(effort) = &mut out.reasoning_effort {
            effort.value = effort.value.normalized();
        }
        out
    }
}

impl Config {
    /// 同 [`Provider::model_spec`]，按上游的名字找。配置里没有这一家（刚删掉）时只看
    /// 价目表。
    pub fn model_spec(
        &self,
        book: &tw_pricing::PriceBook,
        provider: &str,
        model: &str,
    ) -> ResolvedSpec {
        let spec = self
            .providers
            .iter()
            .find(|p| p.name == provider)
            .and_then(|p| p.model_specs.get(model));
        resolve(book, Some(provider), spec, model)
    }
}

/// **唯一定先后的地方**：一项一项看，手写了就用手写的，没写的那一项取价目表。各项各管
/// 各的 —— 只写了上下文窗口，输出上限照样来自价目表。
///
/// 价目表按这一家选的那张查（`provider`），和计价同一个查法；这几项在每张价目表里都
/// 取自默认价目表（自定义价目表只改单价），所以给不给上游通常是同一个值。
fn resolve(
    book: &tw_pricing::PriceBook,
    provider: Option<&str>,
    spec: Option<&ModelSpec>,
    model: &str,
) -> ResolvedSpec {
    let priced = match provider {
        Some(p) => book.resolve_for(p, model),
        None => book.resolve(None, model),
    };
    let price = priced.as_ref().map(|r| &r.price);
    fn pick<T>(manual: Option<T>, table: Option<T>) -> Option<Sourced<T>> {
        match manual {
            Some(value) => Some(Sourced {
                value,
                source: SpecSource::Manual,
            }),
            None => table.map(|value| Sourced {
                value,
                source: SpecSource::PriceTable,
            }),
        }
    }
    ResolvedSpec {
        context_window: pick(
            spec.and_then(|s| s.context_window).map(u64::from),
            price.and_then(|p| p.max_input_tokens),
        ),
        max_output_tokens: pick(
            spec.and_then(|s| s.max_output_tokens).map(u64::from),
            price.and_then(|p| p.max_output_tokens),
        ),
        reasoning: pick(
            spec.and_then(|s| {
                s.reasoning.or_else(|| {
                    s.reasoning_effort
                        .as_ref()
                        .filter(|effort| !effort.supported_levels.is_empty())
                        .map(|_| true)
                })
            }),
            price.and_then(|p| p.reasoning),
        ),
        image_input: pick(
            spec.and_then(|s| s.image_input),
            price.and_then(|p| p.image_input),
        ),
        reasoning_effort: pick(spec.and_then(|s| s.reasoning_effort.clone()), None),
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
            model_specs: specs
                .iter()
                .map(|(m, s)| (m.to_string(), s.clone()))
                .collect(),
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
                reasoning: Some(false),
                ..Default::default()
            },
        )]);
        let l = p.model_spec(&b, "claude-sonnet-4-5");
        assert_eq!(
            l.context_window,
            Some(Sourced {
                value: 1_000_000,
                source: SpecSource::Manual
            })
        );
        // 没写的那一项照样取价目表
        assert_eq!(
            l.max_output_tokens,
            Some(Sourced {
                value: 64_000,
                source: SpecSource::PriceTable
            })
        );
        // 推理也是一项一项来：手写的「不会」盖过价目表的「会」，收不收图照样取价目表
        assert_eq!(
            l.reasoning,
            Some(Sourced {
                value: false,
                source: SpecSource::Manual
            })
        );
        assert_eq!(
            l.image_input,
            Some(Sourced {
                value: true,
                source: SpecSource::PriceTable
            })
        );
        // 别的模型、别的上游不受影响
        assert_eq!(
            p.model_spec(&b, "claude-haiku-4-5")
                .context_window
                .map(|s| s.source),
            Some(SpecSource::PriceTable)
        );
        let cfg = Config {
            providers: vec![p, relay(&[])],
            ..Default::default()
        };
        assert_eq!(
            cfg.model_spec(&b, "relay", "claude-sonnet-4-5")
                .context_window(),
            Some(1_000_000)
        );
        assert_eq!(
            ResolvedSpec::priced(&b, "claude-sonnet-4-5").context_window(),
            Some(200_000)
        );
    }

    #[test]
    fn a_model_the_price_table_does_not_know_has_only_what_was_written() {
        let b = book();
        let p = relay(&[(
            "中转自有模型",
            ModelSpec {
                max_output_tokens: Some(8_000),
                image_input: Some(true),
                ..Default::default()
            },
        )]);
        let l = p.model_spec(&b, "中转自有模型");
        assert_eq!(l.context_window, None);
        assert_eq!(l.max_output_tokens(), Some(8_000));
        assert_eq!(l.image_input(), Some(true));
        // 价目表不认识、也没手写的：不知道，不是「不会」
        assert_eq!(l.reasoning, None);
        // 名字要完全相等：带日期的另一个名字不算
        assert_eq!(
            p.model_spec(&b, "中转自有模型-2026"),
            ResolvedSpec::default()
        );
        // 配置里没有这一家：只看价目表
        let cfg = Config::default();
        assert_eq!(
            cfg.model_spec(&b, "relay", "中转自有模型"),
            ResolvedSpec::default()
        );
    }
}

#[cfg(test)]
mod effort_tests {
    use super::*;

    fn profile(levels: &[ReasoningLevel], default: Option<ReasoningLevel>) -> ReasoningEffort {
        ReasoningEffort {
            supported_levels: levels.to_vec(),
            default_level: default,
        }
    }

    #[test]
    fn manual_profiles_are_atomic_and_imply_reasoning_without_copying_the_upstream_default() {
        use ReasoningLevel::*;
        let book = tw_pricing::PriceBook::builtin().unwrap();
        let mut provider = Provider::default();
        let reported = ReportedSpec {
            reasoning: Some(false),
            reasoning_effort: Some(profile(&[Low, High], Some(High))),
            ..Default::default()
        };
        provider.model_specs.insert(
            "private".into(),
            ModelSpec {
                reasoning_effort: Some(profile(&[Max, Low, Medium], None)),
                ..Default::default()
            },
        );
        let resolved = provider.model_spec_with_metadata(&book, "private", Some(&reported));
        assert_eq!(resolved.reasoning(), Some(true));
        let effort = resolved.reasoning_effort.unwrap();
        assert_eq!(effort.source, SpecSource::Manual);
        assert_eq!(effort.value, profile(&[Low, Medium, Max], Some(Medium)));
        provider.model_specs.clear();
        let resolved = provider.model_spec_with_metadata(&book, "private", Some(&reported));
        assert_eq!(resolved.reasoning(), Some(false));
        assert!(
            resolved
                .reasoning_effort
                .unwrap()
                .value
                .supported_levels
                .is_empty()
        );
    }

    #[test]
    fn unknown_empty_and_reasoning_without_levels_remain_distinct() {
        let book = tw_pricing::PriceBook::builtin().unwrap();
        let mut provider = Provider::default();
        assert!(
            provider
                .model_spec_with_metadata(&book, "private", None)
                .reasoning_effort
                .is_none()
        );
        provider.model_specs.insert(
            "private".into(),
            ModelSpec {
                reasoning: Some(true),
                ..Default::default()
            },
        );
        assert!(
            provider
                .model_spec_with_metadata(&book, "private", None)
                .reasoning_effort
                .is_none()
        );
        provider
            .model_specs
            .get_mut("private")
            .unwrap()
            .reasoning_effort = Some(ReasoningEffort::default());
        let resolved = provider.model_spec_with_metadata(&book, "private", None);
        assert_eq!(resolved.reasoning(), Some(true));
        assert_eq!(
            resolved.reasoning_effort.unwrap().value,
            ReasoningEffort::default()
        );
    }

    #[test]
    fn upstream_specs_override_price_table_and_manual_values_win_field_by_field() {
        let book = tw_pricing::PriceBook::builtin().unwrap();
        let mut provider = Provider::default();
        let reported = ReportedSpec {
            context_window: Some(12345),
            image_input: Some(false),
            ..Default::default()
        };
        provider.model_specs.insert(
            "claude-sonnet-4-5".into(),
            ModelSpec {
                image_input: Some(true),
                ..Default::default()
            },
        );
        let spec = provider.model_spec_with_metadata(&book, "claude-sonnet-4-5", Some(&reported));
        assert_eq!(spec.context_window(), Some(12345));
        assert_eq!(spec.context_window.unwrap().source, SpecSource::Upstream);
        assert_eq!(spec.image_input(), Some(true));
        assert_eq!(spec.image_input.unwrap().source, SpecSource::Manual);
    }

    #[test]
    fn invalid_manual_efforts_fail_before_the_configuration_is_applied() {
        let yaml = |fields: &str| {
            format!(
                "version: 1\nclients:\n  - name: c\n    key: tw-test\nproviders:\n  - name: relay\n    base_url: https://relay.example/v1\n    model_specs:\n      private: {fields}\n"
            )
        };
        for (fields, code) in [
            (
                "{ reasoning_effort: { supported_levels: [high, high] } }",
                "config.model_spec_effort_duplicate",
            ),
            (
                "{ reasoning_effort: { supported_levels: [low], default_level: high } }",
                "config.model_spec_effort_default",
            ),
            (
                "{ reasoning: false, reasoning_effort: { supported_levels: [high] } }",
                "config.model_spec_effort_disabled",
            ),
            (
                "{ reasoning_effort: { supported_levels: [], default_level: high } }",
                "config.model_spec_effort_default",
            ),
        ] {
            let cfg: Config = serde_yaml_ng::from_str(&yaml(fields)).unwrap();
            assert_eq!(
                crate::check_model_spec(
                    "relay",
                    "private",
                    cfg.providers[0].model_specs.get("private")
                )
                .unwrap_err()
                .msg()
                .code,
                code,
                "{fields}"
            );
        }
        for fields in [
            "{ reasoning_effort: { supported_levels: [imaginary] } }",
            "{ reasoning_effort: { supported_levels: [high], typo: high } }",
            "{ reasoning_effort: {} }",
        ] {
            assert!(
                serde_yaml_ng::from_str::<Config>(&yaml(fields)).is_err(),
                "{fields}"
            );
        }
    }
}
