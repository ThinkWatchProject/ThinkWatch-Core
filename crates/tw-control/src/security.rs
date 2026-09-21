//! 两项防护：档位、规则、测试和日志。
//!
//! # 为什么规则要有自己的接口
//!
//! 在此之前规则是看不见的：脱敏规则写死在代码里，界面上只露出五个类别名；
//! 工具调用规则只能去改 config.yaml。用户能做的只有在「关闭 / 观察 / 拦截」
//! 之间选一个，而看不见一条误报是哪条规则报的，就只能把整项关掉 —— 连真有用
//! 的那部分一起。
//!
//! 现在每条规则都列得出来、关得掉，也能写自己的。三件事由这里保证：
//!
//! - **正则在保存时编译**，写错当场拒绝，而不是加载之后悄悄跳过那一条；
//! - **内置规则只记改过默认开关的那几条**，没改过的不写进文件；
//! - **测试和网关用的是同一个引擎、同一套判据**：出站脱敏的测试先把样本
//!   编成请求体里的样子再找，结论才和真的请求一致。

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use serde_yaml_ng::{Mapping, Value};
use tw_config::edit;
use tw_config::history::Origin;
use tw_config::{SecurityMode, ToolAction};
use tw_types::msg;
use tw_yaml::Step;

use crate::{ApplyError, ControlState, Fail, apply_fail, fail};

pub fn router() -> axum::Router<ControlState> {
    axum::Router::new()
        .route("/security", get(detail))
        .route("/security/events", get(events))
        .route("/security/{guard}/mode", put(set_mode))
        .route("/security/{guard}/builtin/{id}", put(toggle_builtin))
        .route("/security/{guard}/custom", post(create_custom))
        .route(
            "/security/{guard}/custom/{name}",
            put(update_custom).delete(delete_custom),
        )
        .route("/security/{guard}/test", post(test))
}

/// 哪一项防护。路径里写的就是配置里的那个键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Guard {
    Redact,
    Tools,
}

impl Guard {
    fn parse(s: &str) -> Result<Self, Fail> {
        match s {
            "redact" => Ok(Guard::Redact),
            "inspect_tools" => Ok(Guard::Tools),
            other => Err(fail(
                StatusCode::NOT_FOUND,
                msg!(
                    "security.unknown_guard", guard = other =>
                    "`{guard}` is not a line of defence; it is redact or inspect_tools."
                ),
            )),
        }
    }
    fn key(self) -> &'static str {
        match self {
            Guard::Redact => "redact",
            Guard::Tools => "inspect_tools",
        }
    }
    fn custom_section(self) -> edit::Section {
        match self {
            Guard::Redact => edit::Section {
                path: &["security", "redact", "custom"],
                what: "redaction rule",
            },
            Guard::Tools => edit::Section {
                path: &["security", "inspect_tools", "custom"],
                what: "tool-call rule",
            },
        }
    }
    fn path(self, leaf: &str) -> Vec<Step> {
        vec![
            Step::key("security"),
            Step::key(self.key()),
            Step::key(leaf),
        ]
    }
    /// 配置里这一项的档位、启停名单
    fn state(self, cfg: &tw_config::Config) -> (SecurityMode, &[String], &[String]) {
        let s = &cfg.security;
        match self {
            Guard::Redact => (s.redact.mode, &s.redact.enable, &s.redact.disable),
            Guard::Tools => (
                s.inspect_tools.mode,
                &s.inspect_tools.enable,
                &s.inspect_tools.disable,
            ),
        }
    }
}

// ---------------------------------------------------------------- 读

async fn detail(State(s): State<ControlState>) -> Json<tw_api::SecurityDetail> {
    Json(view(&s.config()))
}

pub fn view(cfg: &tw_config::Config) -> tw_api::SecurityDetail {
    tw_api::SecurityDetail {
        redact: redact_view(&cfg.security.redact),
        inspect_tools: tools_view(&cfg.security.inspect_tools),
    }
}

fn matcher(m: &tw_redact::rules::Matcher) -> tw_api::Matcher {
    use tw_redact::rules::Matcher as M;
    match *m {
        M::Prefix { prefix, min_tail } => tw_api::Matcher::Prefix {
            prefix: prefix.to_string(),
            min_tail,
        },
        M::OpenaiLegacy { min_len } => tw_api::Matcher::OpenaiLegacy { min_len },
        M::Pem => tw_api::Matcher::Pem,
        M::Jwt => tw_api::Matcher::Jwt,
        M::ConnString => tw_api::Matcher::ConnString,
        M::PrivateIp => tw_api::Matcher::PrivateIp,
        M::DomainSuffix { suffixes } => tw_api::Matcher::DomainSuffix {
            suffixes: suffixes.iter().map(|s| s.to_string()).collect(),
        },
    }
}

fn redact_view(p: &tw_config::RedactPolicy) -> tw_api::GuardDetail {
    let mut rules: Vec<tw_api::SecurityRuleView> = tw_redact::rules::BUILTINS
        .iter()
        .map(|b| {
            let on = if b.on_by_default {
                !p.disable.iter().any(|x| x == b.id)
            } else {
                p.enable.iter().any(|x| x == b.id)
            };
            tw_api::SecurityRuleView {
                id: b.id.to_string(),
                custom: false,
                name: b.name.to_string(),
                why: String::new(),
                kind: b.kind.slug().to_string(),
                matcher: matcher(&b.matcher),
                enabled: on,
                on_by_default: b.on_by_default,
                action: None,
            }
        })
        .collect();
    rules.extend(p.custom.iter().map(|c| tw_api::SecurityRuleView {
        id: c.name.clone(),
        custom: true,
        name: c.name.clone(),
        why: String::new(),
        kind: "custom".into(),
        matcher: tw_api::Matcher::Regex {
            pattern: c.pattern.clone(),
        },
        enabled: !c.disabled,
        on_by_default: true,
        action: None,
    }));
    tw_api::GuardDetail {
        mode: p.mode.slug().to_string(),
        rules,
        unknown: p
            .enable
            .iter()
            .chain(&p.disable)
            .filter(|id| tw_redact::rules::builtin(id).is_none())
            .cloned()
            .collect(),
    }
}

fn tools_view(p: &tw_config::ToolPolicy) -> tw_api::GuardDetail {
    let builtin = &tw_scan::rules::builtin().dangerous;
    let mut rules: Vec<tw_api::SecurityRuleView> = builtin
        .iter()
        .map(|r| tw_api::SecurityRuleView {
            id: r.id.clone(),
            custom: false,
            name: r.name.clone(),
            why: r.why.clone(),
            kind: "command".into(),
            matcher: tw_api::Matcher::Regex {
                pattern: r.pattern.clone(),
            },
            enabled: !p.disable.contains(&r.id),
            on_by_default: true,
            action: Some(if r.high() { "cut" } else { "record" }.into()),
        })
        .collect();
    rules.extend(p.custom.iter().map(|c| tw_api::SecurityRuleView {
        id: c.name.clone(),
        custom: true,
        name: c.name.clone(),
        why: String::new(),
        kind: "custom".into(),
        matcher: tw_api::Matcher::Regex {
            pattern: c.pattern.clone(),
        },
        enabled: !c.disabled,
        on_by_default: true,
        action: Some(c.action.slug().into()),
    }));
    tw_api::GuardDetail {
        mode: p.mode.slug().to_string(),
        rules,
        unknown: p
            .enable
            .iter()
            .chain(&p.disable)
            .filter(|id| !builtin.iter().any(|r| &&r.id == id))
            .cloned()
            .collect(),
    }
}

/// 日志一页要的：哪一项、哪一段、从哪条往前、几条。
#[derive(Debug, serde::Deserialize)]
struct EventsQuery {
    guard: Option<String>,
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    /// 只要这条之前的（翻页）
    before: Option<i64>,
    limit: Option<usize>,
}

/// 一页最多多少条。
const PAGE_MAX: usize = 500;

async fn events(
    State(s): State<ControlState>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<tw_api::SecurityEventsPage>, Fail> {
    let guard = match q.guard.as_deref() {
        None | Some("") => None,
        Some(g) => Some(Guard::parse(g)?.key()),
    };
    let store = crate::need_store(&s)?;
    let g = store.lock().await;
    // **缺省是「全部」，不是「今天」。**这是一张列表，「最近 N 条」本身就是
    // 一个完整的回答
    let (events, more) = g
        .db()
        .security_events(
            guard,
            q.from_ms.unwrap_or(0),
            q.to_ms.unwrap_or(i64::MAX),
            q.before,
            q.limit.unwrap_or(100).clamp(1, PAGE_MAX),
        )
        .map_err(crate::internal)?;
    Ok(Json(tw_api::SecurityEventsPage { events, more }))
}

// ---------------------------------------------------------------- 写

async fn set_mode(
    State(s): State<ControlState>,
    Path(guard): Path<String>,
    Json(req): Json<tw_api::ModeSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let guard = Guard::parse(&guard)?;
    let mode = SecurityMode::from_slug(&req.mode).ok_or_else(|| {
        fail(
            StatusCode::BAD_REQUEST,
            msg!(
                "security.unknown_mode", mode = req.mode.clone() =>
                "`{mode}` is not a mode; it is off, observe or enforce."
            ),
        )
    })?;
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, _| {
            // **默认值不写进文件**：退回「观察」就是把这一行删掉
            let value = (mode != SecurityMode::default()).then(|| Value::from(mode.slug()));
            Ok(edit::set(text, &guard.path("mode"), value.as_ref())?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

/// 一个名单写回文件：空的就整行删掉。
fn put_list(text: &str, path: &[Step], list: &[String]) -> Result<String, ApplyError> {
    let value = (!list.is_empty())
        .then(|| Value::Sequence(list.iter().map(|x| Value::from(x.as_str())).collect()));
    Ok(edit::set(text, path, value.as_ref())?)
}

async fn toggle_builtin(
    State(s): State<ControlState>,
    Path((guard, id)): Path<(String, String)>,
    Json(req): Json<tw_api::RuleToggle>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let guard = Guard::parse(&guard)?;
    let on_by_default = match guard {
        Guard::Redact => tw_redact::rules::builtin(&id).map(|b| b.on_by_default),
        Guard::Tools => tw_scan::rules::builtin()
            .dangerous
            .iter()
            .any(|r| r.id == id)
            .then_some(true),
    }
    .ok_or_else(|| {
        fail(
            StatusCode::NOT_FOUND,
            msg!(
                "security.unknown_rule", rule = id.clone() =>
                "There is no built-in rule `{rule}`."
            ),
        )
    })?;
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let (_, enable, disable) = guard.state(cfg);
            let mut enable: Vec<String> = enable.iter().filter(|x| **x != id).cloned().collect();
            let mut disable: Vec<String> = disable.iter().filter(|x| **x != id).cloned().collect();
            // **只记和出厂不一样的那一条**：打开一条出厂就开着的规则，是把它
            // 从名单里拿掉，而不是再写一遍
            if req.enabled != on_by_default {
                if req.enabled {
                    enable.push(id.clone());
                } else {
                    disable.push(id.clone());
                }
            }
            let text = put_list(text, &guard.path("enable"), &enable)?;
            put_list(&text, &guard.path("disable"), &disable)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

/// 检查一条自定义规则，写成配置里的样子。
fn custom_item(guard: Guard, req: &tw_api::CustomRuleSave) -> Result<Mapping, Fail> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            msg!("security.rule_name_empty" => "A rule needs a name."),
        ));
    }
    // **正则在保存时编译**，写错当场说清楚，不等加载时再跳过
    tw_redact::rules::compile(name, &req.pattern).map_err(bad_pattern)?;
    let mut m = Mapping::new();
    m.insert("name".into(), name.into());
    m.insert("pattern".into(), req.pattern.as_str().into());
    if guard == Guard::Tools {
        let action = match req.action.as_deref() {
            None => ToolAction::default(),
            Some(a) => ToolAction::from_slug(a).ok_or_else(|| {
                fail(
                    StatusCode::BAD_REQUEST,
                    msg!(
                        "security.unknown_action", action = a =>
                        "`{action}` is not an action; it is cut or record."
                    ),
                )
            })?,
        };
        // 默认值不写进文件
        if action != ToolAction::default() {
            m.insert("action".into(), action.slug().into());
        }
    }
    if !req.enabled {
        m.insert("disabled".into(), true.into());
    }
    Ok(m)
}

fn bad_pattern(e: tw_redact::rules::BadPattern) -> Fail {
    fail(
        StatusCode::BAD_REQUEST,
        msg!(
            "security.bad_pattern", detail = e.detail =>
            "The pattern is not a valid regular expression: {detail}"
        ),
    )
}

async fn create_custom(
    State(s): State<ControlState>,
    Path(guard): Path<String>,
    Json(req): Json<tw_api::CustomRuleSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let guard = Guard::parse(&guard)?;
    let item = custom_item(guard, &req)?;
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, _| {
            Ok(edit::upsert(text, guard.custom_section(), None, &item)?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn update_custom(
    State(s): State<ControlState>,
    Path((guard, name)): Path<(String, String)>,
    Json(req): Json<tw_api::CustomRuleSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let guard = Guard::parse(&guard)?;
    let item = custom_item(guard, &req)?;
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, _| {
            Ok(edit::upsert(
                text,
                guard.custom_section(),
                Some(&name),
                &item,
            )?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

#[derive(Debug, serde::Deserialize)]
struct DeleteQuery {
    base_version: Option<String>,
}

async fn delete_custom(
    State(s): State<ControlState>,
    Path((guard, name)): Path<(String, String)>,
    Query(q): Query<DeleteQuery>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let guard = Guard::parse(&guard)?;
    let version = s
        .cfg
        .transform(q.base_version.as_deref(), Origin::Ui, |text, _| {
            Ok(edit::remove(text, guard.custom_section(), &name)?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

// ---------------------------------------------------------------- 测试

/// 一个字节下标换成 UTF-16 码元下标。界面是 JavaScript，按它的下标切。
fn utf16_at(text: &str, byte: usize) -> usize {
    text[..byte].encode_utf16().count()
}

/// 名字给「只试这一条」用。**不会写进任何地方。**
const TRIAL: &str = "trial";

async fn test(
    State(s): State<ControlState>,
    Path(guard): Path<String>,
    Json(req): Json<tw_api::SecurityTestRequest>,
) -> Result<Json<tw_api::SecurityTestResult>, Fail> {
    let guard = Guard::parse(&guard)?;
    // 「按现在启用的规则」用的就是网关手里那一份，不另编一份
    let rt = s.gateway.runtime();
    let hits = match guard {
        Guard::Redact => {
            let trial;
            let rules = match &req.pattern {
                Some(p) => {
                    trial = tw_redact::rules::RuleSet::none()
                        .with_custom(TRIAL, p)
                        .map_err(bad_pattern)?;
                    &trial
                }
                None => rt.redact.as_ref(),
            };
            // **按它在请求体里的样子找**，结论才和真的请求一致
            tw_redact::rules::scan_plain(&req.sample, rules)
                .into_iter()
                .map(|h| {
                    let value = &req.sample[h.bytes.clone()];
                    tw_api::SecurityTestHit {
                        excerpt: tw_redact::rules::masked(&h.rule, value),
                        start: utf16_at(&req.sample, h.bytes.start),
                        end: utf16_at(&req.sample, h.bytes.end),
                        rule: h.rule.id().to_string(),
                        custom: h.rule.custom(),
                        action: None,
                    }
                })
                .collect()
        }
        Guard::Tools => {
            let trial;
            let rules = match &req.pattern {
                Some(p) => {
                    trial = tw_scan::rules::single(TRIAL, p, true).map_err(|e| {
                        fail(
                            StatusCode::BAD_REQUEST,
                            msg!(
                                "security.bad_pattern", detail = e =>
                                "The pattern is not a valid regular expression: {detail}"
                            ),
                        )
                    })?;
                    &trial
                }
                None => rt.tools.as_ref(),
            };
            // 和网关一样：**每条规则只报第一处**
            let mut out: Vec<tw_api::SecurityTestHit> = rules
                .rules
                .iter()
                .filter_map(|r| {
                    let m = r.re.find(&req.sample)?;
                    Some(tw_api::SecurityTestHit {
                        rule: r.id.clone(),
                        custom: r.custom,
                        start: utf16_at(&req.sample, m.start()),
                        end: utf16_at(&req.sample, m.end()),
                        excerpt: m.as_str().chars().take(120).collect(),
                        action: Some(if r.high { "cut" } else { "record" }.into()),
                    })
                })
                .collect();
            out.sort_by_key(|h| h.start);
            out
        }
    };
    Ok(Json(tw_api::SecurityTestResult { hits }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_offsets_count_what_javascript_counts() {
        // 中文一个字一个码元，emoji 两个 —— 按字节算的话界面标错位置
        let t = "中文🙂sk";
        assert_eq!(utf16_at(t, t.find("sk").unwrap()), 4);
    }

    #[test]
    fn every_builtin_redaction_rule_is_listed_with_its_default() {
        let v = redact_view(&Default::default());
        assert_eq!(v.rules.len(), tw_redact::rules::BUILTINS.len());
        let ip = v.rules.iter().find(|r| r.id == "internal-ip").unwrap();
        assert!(!ip.enabled && !ip.on_by_default);
        let key = v
            .rules
            .iter()
            .find(|r| r.id == "anthropic-api-key")
            .unwrap();
        assert!(key.enabled);
        assert_eq!(
            key.matcher,
            tw_api::Matcher::Prefix {
                prefix: "sk-ant-".into(),
                min_tail: 20
            }
        );
    }

    #[test]
    fn switched_rules_and_unknown_ids_show_up_as_they_are() {
        let v = redact_view(&tw_config::RedactPolicy {
            enable: vec!["internal-ip".into(), "拼错了".into()],
            disable: vec!["jwt".into()],
            ..Default::default()
        });
        assert!(
            v.rules
                .iter()
                .find(|r| r.id == "internal-ip")
                .unwrap()
                .enabled
        );
        assert!(!v.rules.iter().find(|r| r.id == "jwt").unwrap().enabled);
        assert_eq!(v.unknown, vec!["拼错了".to_string()]);
    }

    #[test]
    fn tool_rules_say_what_they_do_in_enforce() {
        let v = tools_view(&tw_config::ToolPolicy {
            custom: vec![tw_config::CustomToolRule {
                name: "删除集群资源".into(),
                pattern: r"kubectl\s+delete".into(),
                action: ToolAction::Cut,
                disabled: true,
            }],
            ..Default::default()
        });
        let curl = v.rules.iter().find(|r| r.id == "curl-pipe-sh").unwrap();
        assert_eq!(curl.action.as_deref(), Some("cut"));
        assert!(!curl.why.is_empty());
        let rm = v.rules.iter().find(|r| r.id == "rm-rf-root").unwrap();
        assert_eq!(rm.action.as_deref(), Some("record"));
        let mine = v.rules.last().unwrap();
        assert!(mine.custom && !mine.enabled);
        assert_eq!(mine.action.as_deref(), Some("cut"));
    }
}
