//! 模型别名：增删改、预览、谁在用，以及「同一个模型在各家叫不同名称」的建议。
//!
//! 别名是什么见 `tw_config::aliases`；请求里的别名怎么对到各家上游见
//! `tw_gateway::models::resolve`。这里回答的是界面要问的几件事：
//!
//! - **它现在发往哪儿**（`served_by`）：每家启用的上游按 `resolve` 取名。
//! - **它挡住了谁**（`shadows`）：别名优先，某家清单里有一个同名的真模型、而别名的
//!   列表里没有这个名称，这个名称就不会再发给那一家。
//! - **还有谁有同一个模型**（建议、`same_model`）：只认 Claude，见 [`claude_key`]。
//!
//! 看「某家有没有某个名称」时读**每家原始的清单**（`tw_gateway::models::Directory`），
//! 按启用范围过滤，不读汇总的目录：目录里别名和真名是登记在一起的（同名时只留别名），
//! 而这里要问的恰恰是真名。

use std::collections::HashSet;

use axum::Json;
use axum::extract::{Path, Query, State};
use tw_api::{AliasRuleRef, PinnedModel, ep};
use tw_config::edit;
use tw_config::history::Origin;
use tw_config::refs::{self, AliasRefs};

use crate::contract::RouterExt;
use crate::{ApplyError, ControlState, Fail, apply_fail};

pub fn router() -> axum::Router<ControlState> {
    axum::Router::new()
        .at(ep::Aliases, list)
        .at(ep::CreateAlias, create)
        .at(ep::PreviewAlias, preview)
        .at(ep::UpdateAlias, update)
        .at(ep::DeleteAlias, delete_alias)
        .at(ep::AliasUsage, usage)
}

fn not_found(name: &str) -> ApplyError {
    crate::resources::not_found(edit::ALIAS, name)
}

// ---------------------------------------------------------------- 读

async fn list(State(s): State<ControlState>) -> Json<tw_api::AliasesView> {
    let cfg = s.config();
    let catalog = s.gateway.catalog.load();
    let lists = lists(&s, &cfg);
    let used = usage_24h(&s).await;
    let book = s.gateway.pricing.load();
    let aliases = cfg
        .aliases
        .iter()
        .map(|a| {
            let served_by = served_by(&cfg, &catalog, &a.name);
            let (requests_24h, cost_micros_24h) = used
                .iter()
                .find(|u| u.name == a.name)
                .map(|u| (u.requests, u.cost))
                .unwrap_or_default();
            tw_api::AliasView {
                name: a.name.clone(),
                models: a
                    .models
                    .iter()
                    .map(|m| tw_api::AliasModel {
                        model: m.clone(),
                        providers: offering(&lists, m),
                    })
                    .collect(),
                shadows: shadows(&lists, a),
                context_window: served_by.first().and_then(|first| {
                    book.resolve_for(&first.provider, &first.model)
                        .and_then(|r| r.price.max_input_tokens)
                }),
                served_by,
                requests_24h,
                cost_micros_24h,
            }
        })
        .collect();
    Json(tw_api::AliasesView {
        aliases,
        suggestions: suggestions(&lists, &cfg.aliases),
    })
}

async fn usage(
    State(s): State<ControlState>,
    Path(name): Path<String>,
) -> Result<Json<tw_api::AliasUsage>, Fail> {
    let cfg = s.config();
    if !cfg.aliases.contains(&name) {
        return Err(apply_fail(not_found(&name)));
    }
    let requests_24h = usage_24h(&s)
        .await
        .into_iter()
        .find(|u| u.name == name)
        .map(|u| u.requests)
        .unwrap_or_default();
    Ok(Json(usage_view(
        refs::alias_refs(&cfg, &name),
        requests_24h,
    )))
}

/// 预览一个还没保存的别名。**什么都不写**；问题照保存时的校验说，但不挡着别的几项：
/// 界面边填边看，名字还空着的时候「发往各上游」照样有用。
async fn preview(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::AliasPreviewRequest>,
) -> Json<tw_api::AliasPreview> {
    let cfg = s.config();
    let catalog = s.gateway.catalog.load();
    let lists = lists(&s, &cfg);
    let draft = draft(&req.alias);
    let mut problems = Vec::new();
    if let Err(m) = crate::resources::checked_name(&req.alias.name, edit::ALIAS) {
        problems.push(m);
    }
    // 和保存之后的那张表一起校验：重名、指向别的别名，要看整张表才说得出
    let mut table = cfg.aliases.clone();
    match req.original.as_deref() {
        Some(old) => match table.0.iter().position(|a| a.name == old) {
            Some(i) => table.0[i] = draft.clone(),
            None => {
                problems.push(
                    edit::EditError::NotFound {
                        what: edit::ALIAS,
                        name: old.to_string(),
                    }
                    .msg(),
                );
                table.0.push(draft.clone());
            }
        },
        None => table.0.push(draft.clone()),
    }
    // 名字空着上面已经说过了（同一件事，那一句说的是这个对话框）
    if let Err(e) = tw_config::check_aliases(&table)
        && !(matches!(e, tw_config::ValidationError::AliasEmptyName) && !problems.is_empty())
    {
        problems.push(e.msg());
    }

    // 「存了之后」：这张表里只有这一个别名就够 resolve 用 —— 重名的时候也不会认错
    let mut with = (*cfg).clone();
    with.aliases = tw_config::Aliases(vec![draft.clone()]);
    let listed = |m: &String| draft.models.contains(m);
    let reached: HashSet<&str> = lists
        .iter()
        .filter(|(_, ms)| ms.iter().any(listed))
        .map(|(p, _)| p.as_str())
        .collect();
    let keys: HashSet<String> = draft.models.iter().filter_map(|m| claude_key(m)).collect();
    let same_model = lists
        .iter()
        .filter(|(p, _)| !reached.contains(p.as_str()))
        .flat_map(|(p, ms)| {
            ms.iter()
                .filter(|m| !listed(m) && claude_key(m).is_some_and(|k| keys.contains(&k)))
                .map(|m| PinnedModel {
                    provider: p.clone(),
                    model: m.clone(),
                })
        })
        .collect();
    Json(tw_api::AliasPreview {
        problems,
        served_by: served_by(&with, &catalog, &draft.name),
        unserved: draft
            .models
            .iter()
            .filter(|m| !m.trim().is_empty() && offering(&lists, m).is_empty())
            .cloned()
            .collect(),
        shadows: shadows(&lists, &draft),
        same_model,
    })
}

// ---------------------------------------------------------------- 写

async fn create(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::AliasSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, _| {
            let a = to_alias(&req.alias)?;
            Ok(edit::upsert_alias(text, None, &a)?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn update(
    State(s): State<ControlState>,
    Path(name): Path<String>,
    Json(req): Json<tw_api::AliasSave>,
) -> Result<Json<tw_api::AliasWritten>, Fail> {
    let mut renamed = AliasRefs::default();
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, cfg| {
            if !cfg.aliases.contains(&name) {
                return Err(not_found(&name));
            }
            let a = to_alias(&req.alias)?;
            let mut out = edit::upsert_alias(text, Some(&name), &a)?;
            if a.name != name {
                // **和别名在同一个版本里改**：分两次写的话，中间那一版里密钥放行的、
                // 规则匹配的是一个已经不存在的名字 —— 配置照样通过校验，请求却悄悄变了
                renamed = refs::alias_refs(cfg, &name);
                out = refs::rename_alias(&out, cfg, &name, &a.name)?;
            }
            Ok(out)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::AliasWritten {
        version,
        renamed_in: usage_view(renamed, 0),
    }))
}

/// 删掉。**引用它的地方不拦**：密钥的 `allow`、规则的 `when.model` 写着一个不再是别名
/// 的名字，配置照样成立（那是一个上游模型名）。删之前界面先拿 `AliasUsage` 摆给人看。
async fn delete_alias(
    State(s): State<ControlState>,
    Path(name): Path<String>,
    Query(q): Query<tw_api::BaseVersion>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(q.base_version.as_deref(), Origin::Ui, |text, _| {
            Ok(edit::remove_alias(text, &name)?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

// ---------------------------------------------------------------- 表单 → 配置

/// 交过来的别名，模型名去掉首尾空白、去掉重复（顺序不变）。**不在这里挡空的**：
/// 那一句由校验说（`config.alias_blank_model`），和手改配置时说的一样
fn draft(input: &tw_api::AliasInput) -> tw_config::Alias {
    let mut models: Vec<String> = Vec::new();
    for m in input.models.iter().map(|m| m.trim()) {
        if !models.iter().any(|x| x == m) {
            models.push(m.to_string());
        }
    }
    tw_config::Alias {
        name: input.name.clone(),
        models,
    }
}

fn to_alias(input: &tw_api::AliasInput) -> Result<tw_config::Alias, ApplyError> {
    crate::resources::checked_name(&input.name, edit::ALIAS).map_err(crate::resources::invalid)?;
    Ok(draft(input))
}

fn usage_view(r: AliasRefs, requests_24h: u64) -> tw_api::AliasUsage {
    tw_api::AliasUsage {
        requests_24h,
        keys: r.keys,
        rules: r
            .rules
            .into_iter()
            .map(|x| AliasRuleRef {
                route: x.route,
                rule: x.rule,
                field: x.field.to_string(),
            })
            .collect(),
    }
}

// ---------------------------------------------------------------- 各家有什么

/// 每家启用的上游的清单：原始的（不含别名），按启用范围过滤过，按配置里的顺序。
/// **不知道有什么的上游不在里面**（没有清单，也没有手写的）。
fn lists(s: &ControlState, cfg: &tw_config::Config) -> Vec<(String, Vec<String>)> {
    cfg.providers
        .iter()
        .filter(|p| !p.disabled)
        .filter_map(|p| {
            let l = s.gateway.models.listing(p);
            (l.source != tw_gateway::models::Source::None).then(|| {
                (
                    p.name.clone(),
                    l.models.into_iter().filter(|m| p.uses_model(m)).collect(),
                )
            })
        })
        .collect()
}

/// 清单里有 `model` 的上游
fn offering(lists: &[(String, Vec<String>)], model: &str) -> Vec<String> {
    lists
        .iter()
        .filter(|(_, ms)| ms.iter().any(|m| m == model))
        .map(|(p, _)| p.clone())
        .collect()
}

/// 每家启用的上游发出的名称（`resolve`）。服务不了的不在里面
fn served_by(
    cfg: &tw_config::Config,
    catalog: &tw_engine::Catalog,
    name: &str,
) -> Vec<PinnedModel> {
    cfg.providers
        .iter()
        .filter(|p| !p.disabled)
        .filter_map(|p| {
            tw_gateway::models::resolve(cfg, catalog, p, name).map(|model| PinnedModel {
                provider: p.name.clone(),
                model,
            })
        })
        .collect()
}

/// 清单里有一个和别名同名的真模型、别名却没列这个名称的上游
fn shadows(lists: &[(String, Vec<String>)], a: &tw_config::Alias) -> Vec<String> {
    if a.name.trim().is_empty() || a.models.contains(&a.name) {
        return Vec::new();
    }
    offering(lists, &a.name)
}

/// 一个名称最近 24 小时的请求数和费用。
struct Used {
    name: String,
    requests: u64,
    cost: Option<i64>,
}

/// 最近 24 小时按客户端写的模型名分的请求和费用。**读不了记录就当没有**：别名页不该
/// 因为请求记录坏了就打不开
async fn usage_24h(s: &ControlState) -> Vec<Used> {
    let Some(store) = &s.store else {
        return Vec::new();
    };
    let now = crate::now_ms();
    let groups = store
        .lock()
        .await
        .db()
        .cost_by(tw_api::CostDim::Model, now - 24 * 3600 * 1000, now)
        .unwrap_or_else(|e| {
            tracing::warn!("the requests of the last 24 hours could not be read: {e}");
            Vec::new()
        });
    groups
        .into_iter()
        .map(|g| {
            // 一条都算不出钱（没有价格、没有用量）时是「不知道」，不是 $0
            let priced = g.requests > g.unpriced_requests + g.no_usage_requests;
            Used {
                requests: g.requests.max(0) as u64,
                cost: priced.then_some(g.cost_micros),
                name: g.name,
            }
        })
        .collect()
}

// ---------------------------------------------------------------- 同一个模型

/// 几家清单里名称不同、归一之后相同的 Claude 模型。至少两家上游、至少两种写法才成组；
/// 已经有一个别名替这一组的每一家都列了它在那一家的名称，这一组就不出。
fn suggestions(
    lists: &[(String, Vec<String>)],
    aliases: &tw_config::Aliases,
) -> Vec<tw_api::AliasSuggestion> {
    let mut groups: Vec<(String, Vec<PinnedModel>)> = Vec::new();
    for (p, ms) in lists {
        for m in ms {
            let Some(key) = claude_key(m) else { continue };
            let pinned = PinnedModel {
                provider: p.clone(),
                model: m.clone(),
            };
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, g)) if !g.contains(&pinned) => g.push(pinned),
                Some(_) => {}
                None => groups.push((key, vec![pinned])),
            }
        }
    }
    groups
        .into_iter()
        .filter(|(_, g)| {
            let providers: HashSet<&str> = g.iter().map(|x| x.provider.as_str()).collect();
            let names: HashSet<&str> = g.iter().map(|x| x.model.as_str()).collect();
            providers.len() >= 2 && names.len() >= 2
        })
        .filter(|(_, g)| !aliases.iter().any(|a| covers(a, g)))
        .map(|(key, models)| tw_api::AliasSuggestion {
            label: label(&key),
            models,
        })
        .collect()
}

/// 别名 `a` 替这一组里的每一家上游都列了它在那一家的某个名称。**按上游算，不按名称算**：
/// Bedrock 一家就列着同一个模型的好几种写法（基础 id、`us.`、`global.`），别名只要
/// 列其中一种，那一家就到得了
fn covers(a: &tw_config::Alias, group: &[PinnedModel]) -> bool {
    group.iter().all(|x| {
        group
            .iter()
            .any(|y| y.provider == x.provider && a.models.contains(&y.model))
    })
}

/// 一个名称归一成 Claude 模型的身份；不是 Claude、认不出的是 `None`。
///
/// **两个名称归一之后完全相等，才算同一个模型**（拿来给建议，所以宁缺毋滥）：
///
/// - 只留路径的最后一段：`anthropic/claude-sonnet-4.5`、Vertex 的资源路径；
/// - Vertex 的 `@日期` 写成 `-日期`（`claude-opus-4-1@20250805`）；别的 `@` 认不出；
/// - Bedrock：地域前缀（`us.`、`global.`……）、`anthropic.`，以及跟在后面的 `-v1:0` / `-v1`；
/// - 版本号里的点写成横线（`4.5` → `4-5`）；
/// - **日期保留**：`claude-opus-4-1` 和 `claude-opus-4-1-20250805` 不算同一个，两个日期
///   也不算。
///
/// 剩下的必须以 `claude-` 开头、只有小写字母、数字和单个横线 —— OpenRouter 的
/// `:thinking` 这类变体、`[1m]` 这类标记都认不出，不进建议。
pub(crate) fn claude_key(id: &str) -> Option<String> {
    let lower = id.trim().to_ascii_lowercase();
    let last = lower.rsplit('/').find(|s| !s.is_empty())?;
    let s = match last.split_once('@') {
        Some((base, date)) if is_date(date) => format!("{base}-{date}"),
        Some(_) => return None,
        None => last.to_string(),
    };
    let s = match s.split_once('.') {
        None => s,
        Some(_) => bedrock(&s)?.to_string(),
    };
    let s = dots_between_digits(&s);
    let clean = s.strip_prefix("claude-").is_some_and(|rest| {
        !rest.is_empty()
            && rest.split('-').all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            })
    });
    clean.then_some(s)
}

/// Bedrock 的写法剥成 Anthropic 自己的名字：地域前缀（可以没有）、`anthropic.`、版本。
/// 不是 Anthropic 的模型是 `None`。版本号里的点（`claude-sonnet-4.5`）不归这里管
fn bedrock(id: &str) -> Option<&str> {
    let id = match id.split_once('.') {
        Some((geo, rest)) if tw_pricing::name::BEDROCK_GEOS.contains(&geo) => rest,
        _ => id,
    };
    let Some(name) = id.strip_prefix("anthropic.") else {
        // `claude-sonnet-4.5` 这种只是版本号里有点
        return (!id.contains("anthropic.")).then_some(id);
    };
    let Some(i) = name.rfind("-v") else {
        return Some(name);
    };
    let (major, minor) = match name[i + 2..].split_once(':') {
        Some((a, b)) => (a, Some(b)),
        None => (&name[i + 2..], None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if digits(major) && minor.is_none_or(digits) {
        Some(&name[..i])
    } else {
        Some(name)
    }
}

/// 数字之间的点换成横线：`4.5` 和 `4-5` 是同一个版本的两种写法
fn dots_between_digits(s: &str) -> String {
    let b = s.as_bytes();
    s.char_indices()
        .map(|(i, c)| {
            let between = c == '.'
                && i > 0
                && b[i - 1].is_ascii_digit()
                && b.get(i + 1).is_some_and(u8::is_ascii_digit);
            if between { '-' } else { c }
        })
        .collect()
}

fn is_date(s: &str) -> bool {
    s.len() == 8 && s.bytes().all(|b| b.is_ascii_digit()) && s.starts_with("20")
}

/// 建议的名字：`claude-opus-4-1-20250805` → `Claude Opus 4.1 (2025-08-05)`。和
/// `/v1/models` 的 `display_name` 同一种推法（那一份在 tw-gateway 里，不对外）：相邻的
/// 数字用点连起来，单词首字母大写；日期不进名字，写在后面的括号里 —— 同一个模型的两个
/// 快照各成一组，名字要分得开
fn label(key: &str) -> String {
    let (base, date) = match key.rsplit_once('-') {
        Some((base, d)) if is_date(d) => (base, Some(d)),
        _ => (key, None),
    };
    let mut words: Vec<String> = vec!["Claude".into()];
    let mut number = false;
    for part in base.strip_prefix("claude-").unwrap_or(base).split('-') {
        if part.is_empty() {
            continue;
        }
        if part.bytes().all(|b| b.is_ascii_digit()) {
            match words.last_mut() {
                Some(w) if number => {
                    w.push('.');
                    w.push_str(part);
                }
                _ => words.push(part.to_string()),
            }
            number = true;
        } else {
            let mut c = part.chars();
            let first = c.next().map(|f| f.to_ascii_uppercase());
            words.push(first.into_iter().chain(c).collect());
            number = false;
        }
    }
    let name = words.join(" ");
    match date {
        Some(d) => format!("{name} ({}-{}-{})", &d[..4], &d[4..6], &d[6..]),
        None => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str) -> Option<String> {
        claude_key(id)
    }

    /// 各家对同一个模型的写法，归一之后一样
    #[test]
    fn the_same_claude_model_under_each_upstreams_name_has_one_key() {
        let k = Some("claude-sonnet-4-5-20250929".to_string());
        for id in [
            "claude-sonnet-4-5-20250929",
            "anthropic.claude-sonnet-4-5-20250929-v1:0",
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "eu.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "us-gov.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "claude-sonnet-4-5@20250929",
            "publishers/anthropic/models/claude-sonnet-4-5@20250929",
            "anthropic/claude-sonnet-4.5-20250929",
            "  Claude-Sonnet-4-5-20250929 ",
        ] {
            assert_eq!(key(id), k, "{id}");
        }
        // 没有日期的那一档
        let k = Some("claude-sonnet-4-5".to_string());
        for id in [
            "claude-sonnet-4-5",
            "anthropic/claude-sonnet-4.5",
            "us.anthropic.claude-sonnet-4-5-v1",
            "global.anthropic.claude-sonnet-4-5",
        ] {
            assert_eq!(key(id), k, "{id}");
        }
        assert_eq!(
            key("anthropic.claude-3-5-sonnet-20241022-v2:0").as_deref(),
            Some("claude-3-5-sonnet-20241022")
        );
        assert_eq!(
            key("anthropic/claude-opus-4.1").as_deref(),
            Some("claude-opus-4-1")
        );
        assert_eq!(
            key("claude-opus-4-1@20250805").as_deref(),
            Some("claude-opus-4-1-20250805")
        );
    }

    /// 不同的模型、不同的快照、带日期和不带日期的，都不是同一个
    #[test]
    fn different_models_dates_and_undated_names_stay_apart() {
        let pairs = [
            ("claude-opus-4-1-20250805", "claude-sonnet-4-1-20250805"),
            ("claude-opus-5", "anthropic/claude-sonnet-5"),
            ("claude-3-5-sonnet-20240620", "claude-3-5-sonnet-20241022"),
            (
                "us.anthropic.claude-3-5-sonnet-20240620-v1:0",
                "claude-3-5-sonnet-20241022",
            ),
            ("claude-opus-4-1", "claude-opus-4-1-20250805"),
            ("anthropic/claude-sonnet-4.5", "claude-sonnet-4-5-20250929"),
            ("claude-opus-4", "claude-opus-4-1"),
            ("claude-haiku-4-5", "claude-3-5-haiku"),
        ];
        for (a, b) in pairs {
            let (ka, kb) = (key(a), key(b));
            assert!(ka.is_some() && kb.is_some(), "{a} / {b}");
            assert_ne!(ka, kb, "{a} / {b}");
        }
    }

    /// 只认 Claude；认不出的变体不进建议
    #[test]
    fn only_plain_claude_names_get_a_key() {
        for id in [
            "gpt-5",
            "deepseek.v3-v1:0",
            "us.meta.llama3-3-70b-instruct-v1:0",
            "anthropic/claude-3.7-sonnet:thinking",
            "claude-opus-4-1[1m]",
            "claude-opus-4@latest",
            "claude-",
            "claude--x",
            "anthropic.titan.claude-x",
            "",
        ] {
            assert_eq!(key(id), None, "{id}");
        }
    }

    #[test]
    fn a_suggestion_is_named_like_the_listing_with_its_date() {
        assert_eq!(label("claude-opus-5"), "Claude Opus 5");
        assert_eq!(
            label("claude-opus-4-1-20250805"),
            "Claude Opus 4.1 (2025-08-05)"
        );
        assert_eq!(
            label("claude-3-5-sonnet-20241022"),
            "Claude 3.5 Sonnet (2024-10-22)"
        );
        assert_eq!(label("claude-sonnet-4-5"), "Claude Sonnet 4.5");
    }

    fn lists(x: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
        x.iter()
            .map(|(p, ms)| (p.to_string(), ms.iter().map(|m| m.to_string()).collect()))
            .collect()
    }

    fn alias(name: &str, models: &[&str]) -> tw_config::Alias {
        tw_config::Alias {
            name: name.into(),
            models: models.iter().map(|m| m.to_string()).collect(),
        }
    }

    #[test]
    fn suggestions_need_two_upstreams_and_two_spellings_and_skip_covered_groups() {
        let l = lists(&[
            (
                "anthropic",
                &["claude-opus-5", "claude-sonnet-5", "claude-haiku-5"],
            ),
            (
                "bedrock",
                &[
                    "anthropic.claude-opus-5-v1:0",
                    "us.anthropic.claude-opus-5-v1:0",
                    "global.anthropic.claude-sonnet-5-v1:0",
                ],
            ),
            // 和官方同一种写法：用不着别名
            ("relay", &["claude-haiku-5", "anthropic/claude-opus-5"]),
        ]);
        let got = suggestions(&l, &tw_config::Aliases::default());
        let labels: Vec<&str> = got.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["Claude Opus 5", "Claude Sonnet 5"]);
        let opus: Vec<(&str, &str)> = got[0]
            .models
            .iter()
            .map(|m| (m.provider.as_str(), m.model.as_str()))
            .collect();
        assert_eq!(
            opus,
            [
                ("anthropic", "claude-opus-5"),
                ("bedrock", "anthropic.claude-opus-5-v1:0"),
                ("bedrock", "us.anthropic.claude-opus-5-v1:0"),
                ("relay", "anthropic/claude-opus-5"),
            ]
        );

        // 每一家都列了一种写法：这一组不出。Bedrock 只列了 `us.` 那一种也算
        let covered = tw_config::Aliases(vec![alias(
            "opus",
            &[
                "claude-opus-5",
                "us.anthropic.claude-opus-5-v1:0",
                "anthropic/claude-opus-5",
            ],
        )]);
        let got = suggestions(&l, &covered);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].label, "Claude Sonnet 5");
        // 少了一家：照样出
        let partial = tw_config::Aliases(vec![alias(
            "opus",
            &["claude-opus-5", "us.anthropic.claude-opus-5-v1:0"],
        )]);
        assert_eq!(suggestions(&l, &partial).len(), 2);
    }
}
