//! 各项防护：档位、规则、测试和日志。
//!
//! # 为什么规则要有自己的接口
//!
//! 在此之前规则是看不见的：脱敏规则写死在代码里，界面上只露出五个类别名；
//! 工具调用规则只能去改 config.yaml。用户能做的只有在「关闭 / 观察 / 第三档」
//! 之间选一个，而看不见一条误报是哪条规则报的，就只能把整项关掉 —— 连真有用
//! 的那部分一起。
//!
//! 现在每条规则都列得出来、关得掉，也能写自己的。
//!
//! # 这里只管写文件
//!
//! **规则长什么样、出厂是什么、怎么校验、视图和「测试…」怎么算，都在共享层**
//! （[`tw_guard::policy`]、[`tw_guard::view`]、[`tw_guard::trial`]）：企业版的管理
//! 接口返回的是同一份 JSON。这里剩下的是桌面版自己的事 —— 把改动写进 config.yaml：
//!
//! - **一条规则在保存时就按读配置的那套校验查一遍**，写错当场说清楚，而不是写进去
//!   之后整份配置加载不了；
//! - **内置规则只记改过默认开关、默认处置的那几条**，没改过的不写进文件；
//! - **自定义规则写成配置里那个类型序列化出来的样子**，默认值不写 —— 写进去的就是
//!   读回来的那一份。

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde_yaml_ng::{Mapping, Value};
use tw_config::edit;
use tw_config::history::Origin;
use tw_config::{
    ContentAction, CustomContentRule, CustomRedactRule, CustomToolRule, PolicyError, SecurityMode,
    ToolAction,
};
use tw_guard::trial::TrialError;
use tw_types::msg;
use tw_yaml::Step;

use crate::contract::RouterExt;
use crate::{ApplyError, ControlState, Fail, apply_fail, fail, internal};
use tw_api::{Guard, RuleAction, ep};

pub fn router() -> axum::Router<ControlState> {
    axum::Router::new()
        .at(ep::Security, detail)
        .at(ep::SecurityEvents, events)
        .at(ep::SetSecurityMode, set_mode)
        .at(ep::ToggleBuiltinRule, toggle_builtin)
        .at(ep::SetBuiltinRuleAction, set_builtin_action)
        .at(ep::CreateCustomRule, create_custom)
        .at(ep::UpdateCustomRule, update_custom)
        .at(ep::DeleteCustomRule, delete_custom)
        .at(ep::TestSecurity, test)
}

/// 这一边对每项防护要知道的事。路径里写的就是配置里的那个键。
trait GuardExt: Sized {
    fn parse(s: &str) -> Result<Self, Fail>;
    fn custom_section(self) -> edit::Section;
    fn path(self, leaf: &str) -> Vec<Step>;
    fn lists(self, cfg: &tw_config::Config) -> (&[String], &[String]);
    fn on_by_default(self, id: &str) -> Option<bool>;
}

impl GuardExt for Guard {
    fn parse(s: &str) -> Result<Self, Fail> {
        Guard::from_slug(s).ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                msg!(
                    "security.unknown_guard", guard = s =>
                    "`{guard}` is not a line of defence; it is redact, inspect_tools or content."
                ),
            )
        })
    }
    /// 自定义规则那一节
    fn custom_section(self) -> edit::Section {
        match self {
            Guard::Redact => edit::Section {
                path: &["security", "redact", "custom"],
                what: "redaction rule",
            },
            Guard::InspectTools => edit::Section {
                path: &["security", "inspect_tools", "custom"],
                what: "tool-call rule",
            },
            Guard::Content => edit::Section {
                path: &["security", "content", "custom"],
                what: "content rule",
            },
        }
    }
    fn path(self, leaf: &str) -> Vec<Step> {
        vec![
            Step::key("security"),
            Step::key(self.slug()),
            Step::key(leaf),
        ]
    }
    /// 配置里这一项的启停名单：`(enable, disable)`
    fn lists(self, cfg: &tw_config::Config) -> (&[String], &[String]) {
        let s = &cfg.security;
        match self {
            Guard::Redact => (&s.redact.enable, &s.redact.disable),
            Guard::InspectTools => (&s.inspect_tools.enable, &s.inspect_tools.disable),
            Guard::Content => (&s.content.enable, &s.content.disable),
        }
    }
    /// 这条内置规则出厂时开不开。不是这一项的内置规则是 `None`
    fn on_by_default(self, id: &str) -> Option<bool> {
        match self {
            Guard::Redact => tw_guard::redact::rules::builtin(id).map(|b| b.on_by_default),
            // 危险命令一组出厂全开
            Guard::InspectTools => tw_guard::tools::rules::builtin()
                .dangerous
                .iter()
                .any(|r| r.id == id)
                .then_some(true),
            Guard::Content => tw_guard::content::builtin(id).map(|b| b.on_by_default),
        }
    }
}

// ---------------------------------------------------------------- 读

async fn detail(State(s): State<ControlState>) -> Json<tw_api::SecurityDetail> {
    Json(tw_guard::view::detail(&s.config().security))
}

/// 一页最多多少条。
const PAGE_MAX: usize = 500;

async fn events(
    State(s): State<ControlState>,
    Query(q): Query<tw_api::SecurityEventsQuery>,
) -> Result<Json<tw_api::SecurityEventsPage>, Fail> {
    let guard = q.guard.map(Guard::slug);
    let store = crate::need_store(&s)?;
    let g = store.lock().await;
    // **缺省是「全部」，不是「今天」。**这是一张列表，「最近 N 条」本身就是
    // 一个完整的回答
    let page = g
        .db()
        .security_events(
            guard,
            q.from_ms.unwrap_or(0),
            q.to_ms.unwrap_or(i64::MAX),
            q.before,
            q.limit.unwrap_or(100).clamp(1, PAGE_MAX),
        )
        .map_err(crate::records)?;
    Ok(Json(page))
}

// ---------------------------------------------------------------- 写

async fn set_mode(
    State(s): State<ControlState>,
    Path(guard): Path<String>,
    Json(req): Json<tw_api::ModeSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let guard = Guard::parse(&guard)?;
    let mode: SecurityMode = req.mode;
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, _| {
            // **默认值不写进文件**：退回出厂的档位就是把这一行删掉
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
    let on_by_default = guard.on_by_default(&id).ok_or_else(|| unknown_rule(&id))?;
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

/// 契约里的规则动作，工具调用审查认的那两个。
fn tool_action_of(a: RuleAction) -> Result<ToolAction, Fail> {
    a.tool().ok_or_else(|| not_a_tool_action(a))
}

fn not_a_tool_action(a: RuleAction) -> Fail {
    fail(
        StatusCode::BAD_REQUEST,
        msg!(
            "security.unknown_action", action = a.slug() =>
            "`{action}` is not an action; it is cut or record."
        ),
    )
}

/// 契约里的规则动作，内容过滤认的那三个。
fn content_action_of(a: RuleAction) -> Result<ContentAction, Fail> {
    a.content().ok_or_else(|| not_a_content_action(a))
}

fn not_a_content_action(a: RuleAction) -> Fail {
    fail(
        StatusCode::BAD_REQUEST,
        msg!(
            "security.content_action_unknown", action = a.slug() =>
            "`{action}` is not an action; it is block, strip or record."
        ),
    )
}

/// 出站脱敏的规则命中就替换，没有处置可言。
fn no_action_of_its_own(guard: Guard) -> Fail {
    fail(
        StatusCode::BAD_REQUEST,
        msg!(
            "security.no_action_of_its_own", guard = guard.slug() =>
            "The rules of `{guard}` have no action of their own; the mode decides what \
             happens to a match."
        ),
    )
}

/// 改一条内置规则在第三档下做什么。
///
/// **内置规则提供的只是一条判据。**命中之后怎么处置，和自定义规则一样由用户
/// 定 —— 不必为了改处置先复制成一条自定义规则。和出厂一样的就把那一行删掉。
async fn set_builtin_action(
    State(s): State<ControlState>,
    Path((guard, id)): Path<(String, String)>,
    Json(req): Json<tw_api::ActionSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let guard = Guard::parse(&guard)?;
    // 这条改成什么、出厂是什么，都写成 slug：两项各有各的词
    let (action, factory) = match guard {
        Guard::InspectTools => {
            let spec = tw_guard::tools::rules::builtin()
                .dangerous
                .iter()
                .find(|r| r.id == id)
                .ok_or_else(|| unknown_rule(&id))?;
            (
                tool_action_of(req.action)?.slug(),
                ToolAction::factory(spec).slug(),
            )
        }
        Guard::Content => {
            let b = tw_guard::content::builtin(&id).ok_or_else(|| unknown_rule(&id))?;
            (
                content_action_of(req.action)?.slug(),
                ContentAction::factory(b).slug(),
            )
        }
        Guard::Redact => return Err(no_action_of_its_own(guard)),
    };
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, _| {
            let path = [
                Step::key("security"),
                Step::key(guard.slug()),
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

/// 一条规则序列化成配置里的样子：一个映射，默认值不写。
fn item<T: serde::Serialize>(rule: &T) -> Result<Mapping, Fail> {
    match serde_yaml_ng::to_value(rule) {
        Ok(Value::Mapping(m)) => Ok(m),
        Ok(other) => Err(internal(format!("a rule became {other:?}, not a mapping"))),
        Err(e) => Err(internal(e)),
    }
}

/// 检查一条自定义规则，写成配置里的样子。
///
/// **和读配置用的是同一套校验**（[`tw_config::Security::check`]）：先把这一条放进一份
/// 只有它的策略里查一遍，过了才写。名字重不重复由写文件那一步说（它看得见别的规则）。
fn custom_item(guard: Guard, req: &tw_api::CustomRuleSave) -> Result<Mapping, Fail> {
    let name = req.name.trim().to_string();
    let pattern = req.pattern.clone();
    let disabled = !req.enabled;
    let mut one = tw_config::Security::default();
    let m = match guard {
        Guard::Redact => {
            let rule = CustomRedactRule {
                name,
                pattern,
                // 出厂的标签和没写一样，不写进文件
                label: req
                    .label
                    .clone()
                    .filter(|l| l != tw_guard::policy::DEFAULT_LABEL),
                disabled,
            };
            let m = item(&rule)?;
            one.redact.custom.push(rule);
            m
        }
        Guard::InspectTools => {
            let rule = CustomToolRule {
                name,
                pattern,
                action: req
                    .action
                    .map(tool_action_of)
                    .transpose()?
                    .unwrap_or_default(),
                disabled,
            };
            let m = item(&rule)?;
            one.inspect_tools.custom.push(rule);
            m
        }
        Guard::Content => {
            let rule = CustomContentRule {
                name,
                pattern,
                matching: req.matching.unwrap_or_default(),
                action: req
                    .action
                    .map(content_action_of)
                    .transpose()?
                    .unwrap_or_default(),
                disabled,
            };
            let m = item(&rule)?;
            one.content.custom.push(rule);
            m
        }
    };
    one.check().map_err(rule_fail)?;
    Ok(m)
}

/// 一条自定义规则过不了校验。
fn rule_fail(e: PolicyError) -> Fail {
    let m = match e {
        PolicyError::EmptyName { .. } => {
            msg!("security.rule_name_empty" => "A rule needs a name.")
        }
        PolicyError::EmptyPattern { .. } => pattern_empty(),
        PolicyError::BadPattern { guard, detail, .. } => bad_pattern(guard, detail),
        PolicyError::BadCodepoints { reason, .. } => bad_codepoints(reason),
        PolicyError::BadLabel { label, .. } => bad_label(label),
        // 只有一条规则的策略里不会有：名字重复由写文件那一步说，内置规则的 id 不在这里写
        e @ (PolicyError::DuplicateName { .. } | PolicyError::UnknownRule { .. }) => {
            tw_config::policy_msg(&e)
        }
    };
    fail(StatusCode::BAD_REQUEST, m)
}

fn pattern_empty() -> tw_types::Msg {
    msg!("security.pattern_empty" => "The pattern is empty.")
}

/// 内容过滤的判据不只是正则（还有码位、编出来的大小上限），说法各用各的码
fn bad_pattern(guard: Guard, detail: String) -> tw_types::Msg {
    match guard {
        Guard::Content => msg!(
            "security.bad_content_pattern", detail = detail =>
            "The pattern cannot be used: {detail}"
        ),
        Guard::Redact | Guard::InspectTools => msg!(
            "security.bad_pattern", detail = detail =>
            "The pattern is not a valid regular expression: {detail}"
        ),
    }
}

fn bad_codepoints(reason: tw_guard::content::CodepointError) -> tw_types::Msg {
    msg!(
        "security.bad_codepoints", detail = reason.to_string() =>
        "The code points are not written right: {detail}"
    )
}

fn bad_label(label: String) -> tw_types::Msg {
    msg!(
        "security.bad_label", label = label =>
        "The placeholder name `{label}` has to be 1 to 24 capital letters, digits and \
         underscores, starting with a letter."
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

async fn delete_custom(
    State(s): State<ControlState>,
    Path((guard, name)): Path<(String, String)>,
    Query(q): Query<tw_api::BaseVersion>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let section = Guard::parse(&guard)?.custom_section();
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

fn unknown_rule(id: &str) -> Fail {
    fail(
        StatusCode::NOT_FOUND,
        msg!(
            "security.unknown_rule", rule = id.to_string() =>
            "There is no built-in rule `{rule}`."
        ),
    )
}

/// 「测试…」。怎么算全在共享层（[`tw_guard::trial`]）：按配置里现在这一份策略试，
/// 和网关手里那一份是同一份。
async fn test(
    State(s): State<ControlState>,
    Path(guard): Path<String>,
    Json(req): Json<tw_api::SecurityTestRequest>,
) -> Result<Json<tw_api::SecurityTestResult>, Fail> {
    let guard = Guard::parse(&guard)?;
    tw_guard::trial::run(guard, &s.config().security, &req)
        .map(Json)
        .map_err(|e| trial_fail(guard, e))
}

/// 试不了。码和保存一条规则时报的是同一组
fn trial_fail(guard: Guard, e: TrialError) -> Fail {
    let m = match e {
        TrialError::UnknownRule { id, .. } => return unknown_rule(&id),
        TrialError::BadAction { guard, action } => {
            return match guard {
                Guard::InspectTools => not_a_tool_action(action),
                Guard::Content => not_a_content_action(action),
                Guard::Redact => no_action_of_its_own(guard),
            };
        }
        // 配置读进来时校验过，到这里还编不起来是绕过校验写进去的：那是这边的问题
        TrialError::Policy(e) => return internal(e),
        TrialError::EmptyPattern => pattern_empty(),
        TrialError::BadPattern { detail } => bad_pattern(guard, detail),
        TrialError::BadCodepoints { reason } => bad_codepoints(reason),
        TrialError::BadLabel { label } => bad_label(label),
    };
    fail(StatusCode::BAD_REQUEST, m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save(name: &str, pattern: &str) -> tw_api::CustomRuleSave {
        tw_api::CustomRuleSave {
            name: name.into(),
            pattern: pattern.into(),
            action: None,
            matching: None,
            label: None,
            enabled: true,
            base_version: None,
        }
    }

    fn yaml(m: &Mapping) -> String {
        serde_yaml_ng::to_string(m).unwrap()
    }

    fn code(f: Fail) -> String {
        f.1.0.code.to_string()
    }

    /// 写进文件的只有和默认不一样的那几项，写出来的就是配置读回来的那一份
    #[test]
    fn a_custom_rule_is_written_as_the_config_reads_it() {
        let m = custom_item(Guard::Redact, &save(" 项目代号 ", "Project-[A-Z]+")).unwrap();
        assert_eq!(yaml(&m), "name: 项目代号\npattern: Project-[A-Z]+\n");
        // 出厂的标签显式交上来也不写：和没写是同一份
        let m = custom_item(
            Guard::Redact,
            &tw_api::CustomRuleSave {
                label: Some("SECRET".into()),
                ..save("a", "x")
            },
        )
        .unwrap();
        assert!(!yaml(&m).contains("label"), "{}", yaml(&m));
        let m = custom_item(
            Guard::Redact,
            &tw_api::CustomRuleSave {
                label: Some("PROJECT".into()),
                enabled: false,
                ..save("a", "x")
            },
        )
        .unwrap();
        assert_eq!(
            yaml(&m),
            "name: a\npattern: x\nlabel: PROJECT\ndisabled: true\n"
        );
        let m = custom_item(
            Guard::Content,
            &tw_api::CustomRuleSave {
                matching: Some(tw_api::ContentMatch::Codepoints),
                action: Some(RuleAction::Strip),
                ..save("零宽", "U+200B–U+200D")
            },
        )
        .unwrap();
        assert_eq!(
            yaml(&m),
            "name: 零宽\npattern: U+200B–U+200D\nmatch: codepoints\naction: strip\n"
        );
        let back: tw_config::CustomContentRule =
            serde_yaml_ng::from_value(Value::Mapping(m)).unwrap();
        assert_eq!(back.action, ContentAction::Strip);
        let m = custom_item(
            Guard::InspectTools,
            &tw_api::CustomRuleSave {
                action: Some(RuleAction::Cut),
                ..save("删集群", r"kubectl\s+delete")
            },
        )
        .unwrap();
        assert!(yaml(&m).contains("action: cut"), "{}", yaml(&m));
    }

    /// 写错的当场说清楚，每一种一个码
    #[test]
    fn a_bad_custom_rule_is_refused_with_its_own_code() {
        let cases = [
            (Guard::Redact, save("  ", "x"), "security.rule_name_empty"),
            (Guard::Redact, save("a", ""), "security.pattern_empty"),
            (Guard::Content, save("a", "  "), "security.pattern_empty"),
            (Guard::Redact, save("a", "("), "security.bad_pattern"),
            (Guard::InspectTools, save("a", "("), "security.bad_pattern"),
            (
                Guard::Content,
                tw_api::CustomRuleSave {
                    matching: Some(tw_api::ContentMatch::Regex),
                    ..save("a", "(")
                },
                "security.bad_content_pattern",
            ),
            (
                Guard::Content,
                tw_api::CustomRuleSave {
                    matching: Some(tw_api::ContentMatch::Codepoints),
                    ..save("a", "U+GG")
                },
                "security.bad_codepoints",
            ),
            (
                Guard::Redact,
                tw_api::CustomRuleSave {
                    label: Some("project".into()),
                    ..save("a", "x")
                },
                "security.bad_label",
            ),
            (
                Guard::InspectTools,
                tw_api::CustomRuleSave {
                    action: Some(RuleAction::Strip),
                    ..save("a", "x")
                },
                "security.unknown_action",
            ),
            (
                Guard::Content,
                tw_api::CustomRuleSave {
                    action: Some(RuleAction::Cut),
                    ..save("a", "x")
                },
                "security.content_action_unknown",
            ),
        ];
        for (guard, req, want) in cases {
            let got = code(custom_item(guard, &req).unwrap_err());
            assert_eq!(got, want, "{guard} {:?}", req.pattern);
        }
    }

    /// 「测试…」报的错和保存时同一组码；名字是临时起的，不出现在话里
    #[test]
    fn a_trial_that_cannot_run_says_why_with_the_same_codes() {
        let policy = tw_config::Security::default();
        let run = |guard: Guard, req: tw_api::SecurityTestRequest| {
            code(trial_fail(
                guard,
                tw_guard::trial::run(guard, &policy, &req).unwrap_err(),
            ))
        };
        let req = |pattern: &str| tw_api::SecurityTestRequest {
            sample: "x".into(),
            pattern: Some(pattern.into()),
            ..Default::default()
        };
        assert_eq!(run(Guard::Redact, req("(")), "security.bad_pattern");
        assert_eq!(run(Guard::Redact, req("")), "security.pattern_empty");
        assert_eq!(
            run(
                Guard::Content,
                tw_api::SecurityTestRequest {
                    matching: Some(tw_api::ContentMatch::Codepoints),
                    ..req("U+11FFFF")
                }
            ),
            "security.bad_codepoints"
        );
        assert_eq!(
            run(
                Guard::Redact,
                tw_api::SecurityTestRequest {
                    label: Some("1ABC".into()),
                    ..req("x")
                }
            ),
            "security.bad_label"
        );
        assert_eq!(
            run(
                Guard::Redact,
                tw_api::SecurityTestRequest {
                    action: Some(RuleAction::Record),
                    ..req("x")
                }
            ),
            "security.no_action_of_its_own"
        );
        assert_eq!(
            run(
                Guard::Content,
                tw_api::SecurityTestRequest {
                    action: Some(RuleAction::Cut),
                    ..req("x")
                }
            ),
            "security.content_action_unknown"
        );
        let f = trial_fail(
            Guard::Content,
            tw_guard::trial::run(
                Guard::Content,
                &policy,
                &tw_api::SecurityTestRequest {
                    rule: Some("no-such-rule".into()),
                    ..Default::default()
                },
            )
            .unwrap_err(),
        );
        assert_eq!(f.0, StatusCode::NOT_FOUND);
        assert_eq!(code(f), "security.unknown_rule");
    }

    #[test]
    fn every_guard_knows_its_builtin_rules_and_their_defaults() {
        assert_eq!(Guard::Redact.on_by_default("internal-ip"), Some(false));
        assert_eq!(Guard::Redact.on_by_default("anthropic-api-key"), Some(true));
        assert_eq!(
            Guard::InspectTools.on_by_default("curl-pipe-sh"),
            Some(true)
        );
        assert_eq!(Guard::Content.on_by_default("unicode-tags"), Some(true));
        assert_eq!(Guard::Content.on_by_default("zero-width"), Some(false));
        assert_eq!(Guard::Content.on_by_default("anthropic-api-key"), None);
        assert_eq!(
            code(Guard::parse("hidden_text").unwrap_err()),
            "security.unknown_guard"
        );
    }
}
