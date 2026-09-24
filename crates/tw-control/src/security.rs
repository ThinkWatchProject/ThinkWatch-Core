//! 各项防护：档位、规则、测试和日志。
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
use serde_yaml_ng::{Mapping, Value};
use tw_config::edit;
use tw_config::history::Origin;
use tw_config::{ContentAction, ContentMatch, SecurityMode, ToolAction};
use tw_types::msg;
use tw_yaml::Step;

use crate::contract::RouterExt;
use crate::{ApplyError, ControlState, Fail, apply_fail, fail};
use tw_api::ep;

pub fn router() -> axum::Router<ControlState> {
    axum::Router::new()
        .at(ep::Security, detail)
        .at(ep::SecurityEvents, events)
        .at(ep::SetSecurityMode, set_mode)
        .at(ep::ToggleBuiltinRule, toggle_builtin)
        .at(ep::SetBuiltinRuleAction, set_builtin_action)
        .at(ep::SetSecurityLimit, set_limit)
        .at(ep::CreateCustomRule, create_custom)
        .at(ep::UpdateCustomRule, update_custom)
        .at(ep::DeleteCustomRule, delete_custom)
        .at(ep::TestSecurity, test)
}

/// 哪一项防护。路径里写的就是配置里的那个键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Guard {
    Redact,
    Tools,
    Hidden,
    Content,
    Output,
}

impl Guard {
    fn parse(s: &str) -> Result<Self, Fail> {
        match s {
            "redact" => Ok(Guard::Redact),
            "inspect_tools" => Ok(Guard::Tools),
            "hidden_text" => Ok(Guard::Hidden),
            "content" => Ok(Guard::Content),
            "output_limit" => Ok(Guard::Output),
            other => Err(fail(
                StatusCode::NOT_FOUND,
                msg!(
                    "security.guard_unknown", guard = other =>
                    "`{guard}` is not a line of defence; it is redact, inspect_tools, hidden_text, \
                     content or output_limit."
                ),
            )),
        }
    }
    fn key(self) -> &'static str {
        match self {
            Guard::Redact => "redact",
            Guard::Tools => "inspect_tools",
            Guard::Hidden => "hidden_text",
            Guard::Content => "content",
            Guard::Output => "output_limit",
        }
    }
    /// 自定义规则那一节。**只有这三项有自定义规则**
    fn custom_section(self) -> Result<edit::Section, Fail> {
        match self {
            Guard::Redact => Ok(edit::Section {
                path: &["security", "redact", "custom"],
                what: "redaction rule",
            }),
            Guard::Tools => Ok(edit::Section {
                path: &["security", "inspect_tools", "custom"],
                what: "tool-call rule",
            }),
            Guard::Content => Ok(edit::Section {
                path: &["security", "content", "custom"],
                what: "content rule",
            }),
            Guard::Hidden | Guard::Output => Err(fail(
                StatusCode::BAD_REQUEST,
                msg!(
                    "security.no_custom_rules", guard = self.key() =>
                    "`{guard}` has no custom rules."
                ),
            )),
        }
    }
    fn path(self, leaf: &str) -> Vec<Step> {
        vec![
            Step::key("security"),
            Step::key(self.key()),
            Step::key(leaf),
        ]
    }
    /// 出厂的档位：输出长度出厂是关的，其余是观察
    fn default_mode(self) -> SecurityMode {
        match self {
            Guard::Output => SecurityMode::Off,
            _ => SecurityMode::default(),
        }
    }
    /// 配置里这一项的启停名单：`(enable, disable)`。藏匿字符只有 `disable`
    fn lists(self, cfg: &tw_config::Config) -> (&[String], &[String]) {
        let s = &cfg.security;
        match self {
            Guard::Redact => (&s.redact.enable, &s.redact.disable),
            Guard::Tools => (&s.inspect_tools.enable, &s.inspect_tools.disable),
            Guard::Content => (&s.content.enable, &s.content.disable),
            Guard::Hidden => (&[], &s.hidden_text.disable),
            Guard::Output => (&[], &[]),
        }
    }
}

// ---------------------------------------------------------------- 读

async fn detail(State(s): State<ControlState>) -> Json<tw_api::SecurityDetail> {
    Json(view(&s.config()))
}

pub fn view(cfg: &tw_config::Config) -> tw_api::SecurityDetail {
    let o = &cfg.security.output_limit;
    tw_api::SecurityDetail {
        redact: redact_view(&cfg.security.redact),
        inspect_tools: tools_view(&cfg.security.inspect_tools),
        hidden_text: hidden_view(&cfg.security.hidden_text),
        content: content_view(&cfg.security.content),
        output_limit: tw_api::OutputLimitDetail {
            mode: o.mode.slug().to_string(),
            max_chars: o.max_chars as u64,
            default_max_chars: tw_config::DEFAULT_MAX_CHARS as u64,
            ceiling: tw_config::MAX_CHARS_CEILING as u64,
        },
    }
}

fn hidden_view(p: &tw_config::HiddenPolicy) -> tw_api::GuardDetail {
    tw_api::GuardDetail {
        mode: p.mode.slug().to_string(),
        rules: tw_guard::hidden::SMUGGLING
            .iter()
            .map(|k| tw_api::SecurityRuleView {
                id: k.slug().to_string(),
                custom: false,
                name: k.slug().to_string(),
                why: k.why().to_string(),
                kind: "invisible".into(),
                matcher: tw_api::Matcher::Codepoints {
                    ranges: k.ranges().iter().map(|r| r.to_string()).collect(),
                },
                enabled: !p.disable.iter().any(|d| d == k.slug()),
                on_by_default: true,
                action: None,
                default_action: None,
            })
            .collect(),
    }
}

fn content_matcher(matching: tw_guard::content::Match, pattern: &str) -> tw_api::Matcher {
    match matching {
        tw_guard::content::Match::Contains => tw_api::Matcher::Contains {
            text: pattern.to_string(),
        },
        tw_guard::content::Match::Regex => tw_api::Matcher::Regex {
            pattern: pattern.to_string(),
        },
    }
}

fn content_view(p: &tw_config::ContentPolicy) -> tw_api::GuardDetail {
    let mut rules: Vec<tw_api::SecurityRuleView> = tw_guard::content::builtins()
        .iter()
        .map(|b| tw_api::SecurityRuleView {
            id: b.id.clone(),
            custom: false,
            name: b.name.clone(),
            why: String::new(),
            kind: b.group.clone(),
            matcher: content_matcher(b.matching, &b.pattern),
            enabled: p.builtin_on(b),
            on_by_default: b.on_by_default,
            action: Some(p.builtin_action(b).slug().into()),
            default_action: Some(ContentAction::factory(b).slug().into()),
        })
        .collect();
    rules.extend(p.custom.iter().map(|c| tw_api::SecurityRuleView {
        id: c.name.clone(),
        custom: true,
        name: c.name.clone(),
        why: String::new(),
        kind: "custom".into(),
        matcher: content_matcher(c.matching.engine(), &c.pattern),
        enabled: !c.disabled,
        on_by_default: true,
        action: Some(c.action.slug().into()),
        default_action: None,
    }));
    tw_api::GuardDetail {
        mode: p.mode.slug().to_string(),
        rules,
    }
}

fn matcher(m: &tw_guard::redact::rules::Matcher) -> tw_api::Matcher {
    use tw_guard::redact::rules::Matcher as M;
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
    let mut rules: Vec<tw_api::SecurityRuleView> = tw_guard::redact::rules::BUILTINS
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
                default_action: None,
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
        default_action: None,
    }));
    tw_api::GuardDetail {
        mode: p.mode.slug().to_string(),
        rules,
    }
}

fn tools_view(p: &tw_config::ToolPolicy) -> tw_api::GuardDetail {
    let builtin = &tw_guard::tools::rules::builtin().dangerous;
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
            action: Some(
                p.actions
                    .get(&r.id)
                    .copied()
                    .unwrap_or_else(|| tw_config::ToolAction::factory(r))
                    .slug()
                    .into(),
            ),
            default_action: Some(tw_config::ToolAction::factory(r).slug().into()),
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
        default_action: None,
    }));
    tw_api::GuardDetail {
        mode: p.mode.slug().to_string(),
        rules,
    }
}

/// 一页最多多少条。
const PAGE_MAX: usize = 500;

async fn events(
    State(s): State<ControlState>,
    Query(q): Query<tw_api::SecurityEventsQuery>,
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
        .map_err(crate::records)?;
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
            // **默认值不写进文件**：退回出厂的档位就是把这一行删掉
            let value = (mode != guard.default_mode()).then(|| Value::from(mode.slug()));
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
        Guard::Redact => tw_guard::redact::rules::builtin(&id).map(|b| b.on_by_default),
        Guard::Tools => tw_guard::tools::rules::builtin()
            .dangerous
            .iter()
            .any(|r| r.id == id)
            .then_some(true),
        Guard::Content => tw_guard::content::builtin(&id).map(|b| b.on_by_default),
        Guard::Hidden => tw_guard::hidden::SMUGGLING
            .iter()
            .any(|k| k.slug() == id)
            .then_some(true),
        Guard::Output => None,
    }
    .ok_or_else(|| unknown_rule(&id))?;
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let (enable, disable) = guard.lists(cfg);
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

fn action_of(slug: &str) -> Result<ToolAction, Fail> {
    ToolAction::from_slug(slug).ok_or_else(|| {
        fail(
            StatusCode::BAD_REQUEST,
            msg!(
                "security.unknown_action", action = slug.to_string() =>
                "`{action}` is not an action; it is cut or record."
            ),
        )
    })
}

/// 改一条内置规则在拦截档下做什么。
///
/// **内置规则提供的只是一条正则。**命中之后切不切，和自定义规则一样由用户
/// 定 —— 不必为了改处置先复制成一条自定义规则。和出厂一样的就把那一行删掉。
async fn set_builtin_action(
    State(s): State<ControlState>,
    Path((guard, id)): Path<(String, String)>,
    Json(req): Json<tw_api::ActionSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let guard = Guard::parse(&guard)?;
    // 这条改成什么、出厂是什么，都写成 slug：两项各有各的词
    let (action, factory) = match guard {
        Guard::Tools => {
            let spec = tw_guard::tools::rules::builtin()
                .dangerous
                .iter()
                .find(|r| r.id == id)
                .ok_or_else(|| unknown_rule(&id))?;
            (
                action_of(&req.action)?.slug(),
                tw_config::ToolAction::factory(spec).slug(),
            )
        }
        Guard::Content => {
            let b = tw_guard::content::builtin(&id).ok_or_else(|| unknown_rule(&id))?;
            (
                content_action_of(&req.action)?.slug(),
                ContentAction::factory(b).slug(),
            )
        }
        Guard::Redact | Guard::Hidden | Guard::Output => {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                msg!(
                    "security.no_action_of_its_own", guard = guard.key() =>
                    "The rules of `{guard}` have no action of their own; the mode decides what \
                     happens to a match."
                ),
            ));
        }
    };
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, _| {
            let path = [
                Step::key("security"),
                Step::key(guard.key()),
                Step::key("actions"),
                Step::key(id.as_str()),
            ];
            let value = (action != factory).then(|| Value::from(action));
            Ok(edit::set(text, &path, value.as_ref())?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

fn content_action_of(slug: &str) -> Result<ContentAction, Fail> {
    ContentAction::from_slug(slug).ok_or_else(|| {
        fail(
            StatusCode::BAD_REQUEST,
            msg!(
                "security.unknown_content_action", action = slug.to_string() =>
                "`{action}` is not an action; it is block or record."
            ),
        )
    })
}

/// 改输出长度的上限。和出厂一样就把那一行删掉。
async fn set_limit(
    State(s): State<ControlState>,
    Path(guard): Path<String>,
    Json(req): Json<tw_api::LimitSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let guard = Guard::parse(&guard)?;
    if guard != Guard::Output {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            msg!(
                "security.no_limit", guard = guard.key() =>
                "`{guard}` has no limit; only output_limit does."
            ),
        ));
    }
    let max = req.max_chars as usize;
    if max == 0 || max > tw_config::MAX_CHARS_CEILING {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            msg!(
                "security.limit_range", max = req.max_chars, ceiling = tw_config::MAX_CHARS_CEILING =>
                "The output limit is {max}; it has to be between 1 and {ceiling} characters."
            ),
        ));
    }
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, _| {
            let value = (max != tw_config::DEFAULT_MAX_CHARS).then(|| Value::from(max as u64));
            Ok(edit::set(text, &guard.path("max_chars"), value.as_ref())?)
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
    let mut m = Mapping::new();
    m.insert("name".into(), name.into());
    m.insert("pattern".into(), req.pattern.as_str().into());
    if guard == Guard::Content {
        let matching = match req.matching.as_deref() {
            None => ContentMatch::default(),
            Some(x) => ContentMatch::from_slug(x).ok_or_else(|| {
                fail(
                    StatusCode::BAD_REQUEST,
                    msg!(
                        "security.unknown_match", matching = x.to_string() =>
                        "`{matching}` is not a way to match; it is contains or regex."
                    ),
                )
            })?,
        };
        let action = match req.action.as_deref() {
            None => ContentAction::default(),
            Some(a) => content_action_of(a)?,
        };
        // **和数据面同一种编法**：不分大小写、编译后的大小有上限
        tw_guard::content::Rule::new(tw_guard::content::RuleInput {
            id: name,
            name,
            custom: true,
            pattern: &req.pattern,
            matching: matching.engine(),
            action: tw_guard::content::Action::Warn,
        })
        .map_err(|e| {
            fail(
                StatusCode::BAD_REQUEST,
                msg!(
                    "security.bad_content_pattern", detail = e.detail =>
                    "The pattern cannot be used: {detail}"
                ),
            )
        })?;
        if matching != ContentMatch::default() {
            m.insert("match".into(), matching.slug().into());
        }
        if action != ContentAction::default() {
            m.insert("action".into(), action.slug().into());
        }
        if !req.enabled {
            m.insert("disabled".into(), true.into());
        }
        return Ok(m);
    }
    // **正则在保存时编译**，写错当场说清楚，不等加载时再跳过
    tw_guard::redact::rules::compile(name, &req.pattern).map_err(bad_pattern)?;
    if guard == Guard::Tools {
        let action = match req.action.as_deref() {
            None => ToolAction::default(),
            Some(a) => action_of(a)?,
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

fn bad_pattern(e: tw_guard::redact::rules::BadPattern) -> Fail {
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
    let section = guard.custom_section()?;
    let item = custom_item(guard, &req)?;
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, _| {
            Ok(edit::upsert(text, section, None, &item)?)
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
    let section = guard.custom_section()?;
    let item = custom_item(guard, &req)?;
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, _| {
            Ok(edit::upsert(text, section, Some(&name), &item)?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn delete_custom(
    State(s): State<ControlState>,
    Path((guard, name)): Path<(String, String)>,
    Query(q): Query<tw_api::BaseVersion>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let section = Guard::parse(&guard)?.custom_section()?;
    let version = s
        .cfg
        .transform(q.base_version.as_deref(), Origin::Ui, |text, _| {
            Ok(edit::remove(text, section, &name)?)
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

fn unknown_rule(id: &str) -> Fail {
    fail(
        StatusCode::NOT_FOUND,
        msg!(
            "security.unknown_rule", rule = id.to_string() =>
            "There is no built-in rule `{rule}`."
        ),
    )
}

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
            let rules = match (&req.pattern, &req.rule) {
                (Some(p), _) => {
                    trial = tw_guard::redact::rules::RuleSet::none()
                        .with_custom(TRIAL, p)
                        .map_err(bad_pattern)?;
                    &trial
                }
                (None, Some(id)) => {
                    let b = tw_guard::redact::rules::builtin(id).ok_or_else(|| unknown_rule(id))?;
                    trial = tw_guard::redact::rules::RuleSet::only(&[b.id]);
                    &trial
                }
                (None, None) => rt.redact.as_ref(),
            };
            // **按它在请求体里的样子找**，结论才和真的请求一致
            tw_guard::redact::rules::scan_plain(&req.sample, rules)
                .into_iter()
                .map(|h| {
                    let value = &req.sample[h.bytes.clone()];
                    tw_api::SecurityTestHit {
                        excerpt: tw_guard::redact::rules::masked(&h.rule, value),
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
            let rules = match (&req.pattern, &req.rule) {
                (Some(p), _) => {
                    // 只要正则引擎那半句：规则名是这里临时起的，说出来只会让人困惑
                    trial = tw_guard::tools::rules::single(TRIAL, p, true).map_err(
                        |tw_guard::tools::rules::RuleError::BadPattern { detail, .. }| {
                            fail(
                                StatusCode::BAD_REQUEST,
                                msg!(
                                    "security.bad_pattern", detail = detail =>
                                    "The pattern is not a valid regular expression: {detail}"
                                ),
                            )
                        },
                    )?;
                    &trial
                }
                (None, Some(id)) => {
                    trial = s
                        .config()
                        .security
                        .inspect_tools
                        .one_builtin(id)
                        .ok_or_else(|| unknown_rule(id))?;
                    &trial
                }
                (None, None) => rt.tools.as_ref(),
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
        Guard::Content => {
            let cfg = s.config();
            let trial;
            let rules = match (&req.pattern, &req.rule) {
                (Some(p), _) => {
                    let matching = match req.matching.as_deref() {
                        None => ContentMatch::default(),
                        Some(x) => ContentMatch::from_slug(x).unwrap_or_default(),
                    };
                    trial = tw_guard::content::Rules::build([tw_guard::content::RuleInput {
                        id: TRIAL,
                        name: TRIAL,
                        custom: true,
                        pattern: p,
                        matching: matching.engine(),
                        action: tw_guard::content::Action::Warn,
                    }])
                    .map_err(|e| {
                        fail(
                            StatusCode::BAD_REQUEST,
                            msg!(
                                "security.bad_content_pattern", detail = e.detail =>
                                "The pattern cannot be used: {detail}"
                            ),
                        )
                    })?;
                    &trial
                }
                (None, Some(id)) => {
                    trial = cfg
                        .security
                        .content
                        .one_builtin(id)
                        .ok_or_else(|| unknown_rule(id))?;
                    &trial
                }
                (None, None) => rt.content.as_ref(),
            };
            rules
                .scan_text(&req.sample)
                .into_iter()
                .map(|h| tw_api::SecurityTestHit {
                    start: utf16_at(&req.sample, h.bytes.start),
                    end: utf16_at(&req.sample, h.bytes.end),
                    excerpt: req.sample[h.bytes.clone()].chars().take(120).collect(),
                    action: Some(
                        if h.action == tw_guard::content::Action::Block {
                            "block"
                        } else {
                            "record"
                        }
                        .into(),
                    ),
                    rule: h.rule,
                    custom: h.custom,
                })
                .collect()
        }
        Guard::Hidden => {
            // 给了 `rule` 就只试那一种（关着的也能试），不给按现在开着的
            let kinds = match &req.rule {
                Some(id) => vec![
                    tw_guard::hidden::Kind::from_slug(id)
                        .filter(|k| tw_guard::hidden::SMUGGLING.contains(k))
                        .ok_or_else(|| unknown_rule(id))?,
                ],
                None => rt.hidden.clone(),
            };
            // 一个字符一处：界面要把每一个都标出来
            req.sample
                .char_indices()
                .filter_map(|(i, c)| {
                    let mut found = Vec::new();
                    tw_guard::hidden::scan_smuggled(
                        &req.sample[i..i + c.len_utf8()],
                        false,
                        &kinds,
                        &mut found,
                    );
                    let f = found.pop()?;
                    Some(tw_api::SecurityTestHit {
                        rule: f.kind.slug().to_string(),
                        custom: false,
                        start: utf16_at(&req.sample, i),
                        end: utf16_at(&req.sample, i + c.len_utf8()),
                        excerpt: f.example,
                        action: None,
                    })
                })
                .collect()
        }
        Guard::Output => {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                msg!(
                    "security.nothing_to_test" =>
                    "The output limit has no rules to try a sample against."
                ),
            ));
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
        assert_eq!(v.rules.len(), tw_guard::redact::rules::BUILTINS.len());
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
    fn switched_rules_show_up_as_they_are() {
        let v = redact_view(&tw_config::RedactPolicy {
            enable: vec!["internal-ip".into()],
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
