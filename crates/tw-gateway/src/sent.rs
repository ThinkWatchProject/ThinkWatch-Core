//! 每一家实际收到的模型名。
//!
//! 路由给出的是每个候选**要**的名字（[`tw_engine::Engine::asked`]）：客户端写的、阶段一
//! 改写的，都还是客户端那一侧的名称，可能是别名 —— 发给每一家之前按别名表对到那一家
//! 自己的名称（[`crate::models::resolve`]），故障转移换一家就重新对一次。规则指定的模型、
//! 阶段二改的名字原样发出：写下它们的时候已经知道是哪一家。
//!
//! **路由（跳过服务不了的、按价钱排序）、每一跳、试算用的是同一个函数**：试算说发给
//! Bedrock 的是哪个名字，真实转发发的就是那个。

use tw_engine::{Asked, Catalog, Origin};

/// 一个候选发出去的模型名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub provider: String,
    /// 它要的名字（[`Asked::model`]）
    pub asked: String,
    /// 发给它的名字。`None` = 这家服务不了这个别名：别名列的名字它一个都没有
    pub model: Option<String>,
    /// 发出的名字为什么和客户端写的不一样。一样时是 `None`
    pub via: Option<Via>,
}

/// 发出的名字和客户端写的不一样的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// 客户端写的是别名，对到了这一家的名称
    Alias,
    /// 规则改写了模型（`set.model`，哪个阶段都算）
    Rule,
    /// 规则指定了这一家发什么模型
    Pinned,
}

impl Via {
    /// 控制面发给界面的那个词
    pub fn slug(self) -> &'static str {
        match self {
            Via::Alias => "alias",
            Via::Rule => "rule",
            Via::Pinned => "pinned",
        }
    }
}

/// 发给 `p` 的名字：原样发出的照写，客户端那一侧的名称按别名表对到这一家。
/// `None` = 这家服务不了这个别名。
pub fn name(
    cfg: &tw_config::Config,
    catalog: &Catalog,
    p: &tw_config::Provider,
    asked: &Asked,
) -> Option<String> {
    if asked.origin.as_written() {
        Some(asked.model.clone())
    } else {
        crate::models::resolve(cfg, catalog, p, &asked.model)
    }
}

/// 发出的名字 `sent` 为什么不是客户端写的 `client`。
pub fn via(client: &str, asked: &Asked, sent: &str) -> Option<Via> {
    if sent == client {
        return None;
    }
    Some(match asked.origin {
        Origin::Pinned => Via::Pinned,
        Origin::Rule | Origin::PhaseTwo => Via::Rule,
        // 客户端写的，发出去却不一样：只能是别名对过了
        Origin::Client => Via::Alias,
    })
}

/// 每个候选发出去的名字，和 `asked` 一一对应。`client` 是客户端写的模型名。
///
/// 配置里没有的候选按要的名字算：尝试那一步会报出它不在配置里。
pub fn plan(
    cfg: &tw_config::Config,
    catalog: &Catalog,
    client: &str,
    asked: &[Asked],
) -> Vec<Sent> {
    asked
        .iter()
        .map(|a| {
            let model = match cfg.providers.iter().find(|p| p.name == a.provider) {
                Some(p) => name(cfg, catalog, p, a),
                None => Some(a.model.clone()),
            };
            let via = model.as_deref().and_then(|m| via(client, a, m));
            Sent {
                provider: a.provider.clone(),
                asked: a.model.clone(),
                model,
                via,
            }
        })
        .collect()
}

/// 每个候选和发给它的名字：按价钱排序（[`crate::AppState::unit_prices`]）、看上下文窗口用。
/// 服务不了这个别名的那一家给它要的名字 —— 它会被 [`serving`] 跳过，排不排都一样。
pub fn pairs(sent: &[Sent]) -> Vec<(String, String)> {
    sent.iter()
        .map(|s| {
            let m = s.model.as_ref().unwrap_or(&s.asked);
            (s.provider.clone(), m.clone())
        })
        .collect()
}

/// 去掉服务不了这个请求的候选（[`crate::models::serving`]），**按发出去的名字看**：一家
/// 有没有这个模型，问的是它会收到的那个名字。
///
/// 服务不了这个别名的那一家（别名列的名字它一个都没有）记成「没有这个模型」；它停用了
/// 的话照常记成停用。
pub fn serving(
    cfg: &tw_config::Config,
    catalog: &Catalog,
    sent: &[Sent],
) -> crate::models::Serving {
    let mut out = crate::models::serving(cfg, catalog, &pairs(sent));
    for s in sent.iter().filter(|s| s.model.is_none()) {
        if let Some(i) = out.usable.iter().position(|p| *p == s.provider) {
            out.usable.remove(i);
            out.skipped
                .push((s.provider.clone(), crate::models::Skip::NotOffered));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str, scope: &[&str]) -> tw_config::Provider {
        tw_config::Provider {
            name: name.into(),
            base_url: format!("https://{name}.example"),
            key: Some("sk".into()),
            models_only: (!scope.is_empty()).then(|| scope.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
    }

    fn cfg() -> tw_config::Config {
        tw_config::Config {
            providers: vec![
                provider("bedrock", &["us.anthropic.*"]),
                provider("official", &["claude-*"]),
                provider("zhipu", &["glm-*"]),
            ],
            aliases: serde_yaml_ng::from_str(
                "opus: [claude-opus-5, us.anthropic.claude-opus-5-v1:0]\n",
            )
            .unwrap(),
            ..Default::default()
        }
    }

    fn asked(provider: &str, model: &str, origin: Origin) -> Asked {
        Asked {
            provider: provider.into(),
            model: model.into(),
            origin,
        }
    }

    /// 别名对到每一家自己的名称；原样发出的不对；对不到的那一家是 None，被跳过
    #[test]
    fn each_upstream_gets_its_own_name_and_one_that_has_none_is_skipped() {
        let c = cfg();
        let catalog = Catalog::default();
        let sent = plan(
            &c,
            &catalog,
            "opus",
            &[
                asked("bedrock", "opus", Origin::Client),
                asked("official", "opus", Origin::Client),
                asked("zhipu", "opus", Origin::Client),
            ],
        );
        let got: Vec<_> = sent.iter().map(|s| (s.model.as_deref(), s.via)).collect();
        assert_eq!(
            got,
            [
                (Some("us.anthropic.claude-opus-5-v1:0"), Some(Via::Alias)),
                (Some("claude-opus-5"), Some(Via::Alias)),
                (None, None),
            ]
        );
        // 排价钱时给它要的名字：反正会被跳过
        assert_eq!(pairs(&sent)[2], ("zhipu".to_string(), "opus".to_string()));
        // 智谱：别名列的名字一个都不在它的启用范围里
        let s = serving(&c, &catalog, &sent);
        assert_eq!(s.usable, ["bedrock", "official"]);
        assert_eq!(
            s.skipped,
            [("zhipu".to_string(), crate::models::Skip::OutOfScope)]
        );
        // 没有启用范围、也没有清单的一家什么都当作有：对到第一个
        let mut open = c.clone();
        open.providers[2].models_only = None;
        let sent = plan(
            &open,
            &catalog,
            "opus",
            &[asked("zhipu", "opus", Origin::Client)],
        );
        assert_eq!(sent[0].model.as_deref(), Some("claude-opus-5"));
        // 有清单、清单里一个都没有：没有这个模型
        let listed = Catalog::build(&[tw_engine::ProviderModels {
            provider: "zhipu".into(),
            protocol: "anthropic".into(),
            known: true,
            models: vec!["glm-5".into()],
        }]);
        let sent = plan(
            &open,
            &listed,
            "opus",
            &[asked("zhipu", "opus", Origin::Client)],
        );
        assert_eq!(sent[0].model, None);
        assert_eq!(
            serving(&open, &listed, &sent).skipped,
            [("zhipu".to_string(), crate::models::Skip::NotOffered)]
        );
    }

    /// 指定的、阶段二改的原样发（名字恰好是别名也一样）；阶段一改写成别名的照常对
    #[test]
    fn names_written_for_an_upstream_go_out_as_written() {
        let c = cfg();
        let catalog = Catalog::default();
        let sent = plan(
            &c,
            &catalog,
            "claude-sonnet-5",
            &[
                asked("bedrock", "opus", Origin::Pinned),
                asked("official", "opus", Origin::Rule),
                asked("zhipu", "opus", Origin::PhaseTwo),
            ],
        );
        let got: Vec<_> = sent
            .iter()
            .map(|s| (s.model.as_deref(), s.via.map(Via::slug)))
            .collect();
        assert_eq!(
            got,
            [
                (Some("opus"), Some("pinned")),
                (Some("claude-opus-5"), Some("rule")),
                (Some("opus"), Some("rule")),
            ]
        );
        // 发出的和客户端写的一样：没有来历可说
        let same = plan(
            &c,
            &catalog,
            "claude-opus-5",
            &[asked("official", "claude-opus-5", Origin::Rule)],
        );
        assert_eq!(same[0].via, None);
    }
}
