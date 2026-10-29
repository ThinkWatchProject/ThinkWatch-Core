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
//! - **内置规则只记改过默认开关、默认动作的那几条**，没改过的不写进文件；
//! - **一组规则一次启停只写一次文件**（组标题上的复选框，[`set_rules_enabled`]）：一条一条地
//!   写，几十条就是几十次热重载、几十个版本；
//! - **自定义规则写成配置里那个类型序列化出来的样子**，默认值不写 —— 写进去的就是
//!   读回来的那一份。
//!
//! # 例外值
//!
//! 出站脱敏的例外值（[`tw_guard::redact::exceptions`]）写在 `security.redact.exceptions`，和规则
//! 一样走配置的写入路径，随配置导出、同步、备份。**原值不落在任何地方**：
//!
//! - 手动加入时界面交原值，这里当场算哈希、按认出它的规则打码，写进配置的只有这两样；
//!   原值不进日志（[`tw_api::ExceptionAdd`] 的 `Debug` 不打它）、不进回答。
//! - 从安全日志加入时界面交的是**那一行的号**，不是哈希。值的哈希**不进请求库**，只在记录器
//!   的内存里留最近两万行（[`tw_store::Recorder::value_hash`]），重启就没了：对手机号这种短的
//!   值，一个不加盐的 SHA-256 配上打码后留下的几位是能穷举回去的，存进库等于把号码存了一份，
//!   也没理由交给不需要它的界面（事件流、日志视图都不带它）。更早的记录没法从日志加入（409），
//!   粘贴那个值手动加入。界面要的「这一条已经是例外」「这一条能列为例外」读的时候在这里
//!   照内存里的哈希和现在的例外表算好（[`annotate`]），不在内存里的两样都是 false。
//! - 几处重叠的命中合成的一段、只记录的规则让开之后剩下的几截没有哈希
//!   （[`tw_guard::redact::rules::Finding::sha256`]）：没有哪条规则认过那一段，列为例外挡不住
//!   什么。
//! - 移出按哈希：例外表本身（`GET /security/exceptions`）就是配置里的那一份，界面
//!   拿得到配置原文，哈希在这里不多泄露什么。

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde_yaml_ng::{Mapping, Value};
use tw_config::edit;
use tw_config::history::Origin;
use tw_config::{
    ContentAction, CustomContentRule, CustomRedactRule, CustomToolRule, PolicyError, RedactAction,
    SecurityMode, ToolAction,
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
        .at(ep::SecurityRuleCounts, counts)
        .at(ep::SetSecurityMode, set_mode)
        .at(ep::ToggleBuiltinRule, toggle_builtin)
        .at(ep::SetBuiltinRuleAction, set_builtin_action)
        .at(ep::SetRulesEnabled, set_rules_enabled)
        .at(ep::CreateCustomRule, create_custom)
        .at(ep::UpdateCustomRule, update_custom)
        .at(ep::DeleteCustomRule, delete_custom)
        .at(ep::TestSecurity, test)
        .at(ep::RedactExceptions, exceptions)
        .at(ep::AddRedactException, add_exception)
        .at(ep::ExceptSecurityEvent, add_exception_from_event)
        .at(ep::RemoveRedactException, remove_exception)
}

/// 这一边对每项防护要知道的事。路径里写的就是配置里的那个键。
trait GuardExt: Sized {
    fn parse(s: &str) -> Result<Self, Fail>;
    fn custom_section(self) -> edit::Section;
    fn path(self, leaf: &str) -> Vec<Step>;
    fn lists(self, cfg: &tw_config::Config) -> (&[String], &[String]);
    fn on_by_default(self, id: &str) -> Option<bool>;
    fn custom_names(self, cfg: &tw_config::Config) -> Vec<String>;
    fn toggled_custom(
        self,
        cfg: &tw_config::Config,
        name: &str,
        enabled: bool,
    ) -> Option<Result<Mapping, Fail>>;
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
                key: "name",
            },
            Guard::InspectTools => edit::Section {
                path: &["security", "inspect_tools", "custom"],
                what: "tool-call rule",
                key: "name",
            },
            Guard::Content => edit::Section {
                path: &["security", "content", "custom"],
                what: "content rule",
                key: "name",
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
    /// 这一项自定义规则的名字，按配置里的顺序
    fn custom_names(self, cfg: &tw_config::Config) -> Vec<String> {
        let s = &cfg.security;
        match self {
            Guard::Redact => s.redact.custom.iter().map(|c| c.name.clone()).collect(),
            Guard::InspectTools => s
                .inspect_tools
                .custom
                .iter()
                .map(|c| c.name.clone())
                .collect(),
            Guard::Content => s.content.custom.iter().map(|c| c.name.clone()).collect(),
        }
    }
    /// 名字是 `name` 的自定义规则启用或停用之后写成配置里的样子。没有这条是 `None`
    fn toggled_custom(
        self,
        cfg: &tw_config::Config,
        name: &str,
        enabled: bool,
    ) -> Option<Result<Mapping, Fail>> {
        let s = &cfg.security;
        Some(match self {
            Guard::Redact => {
                let mut c = s.redact.custom.iter().find(|c| c.name == name)?.clone();
                c.disabled = !enabled;
                item(&c)
            }
            Guard::InspectTools => {
                let mut c = s
                    .inspect_tools
                    .custom
                    .iter()
                    .find(|c| c.name == name)?
                    .clone();
                c.disabled = !enabled;
                item(&c)
            }
            Guard::Content => {
                let mut c = s.content.custom.iter().find(|c| c.name == name)?.clone();
                c.disabled = !enabled;
                item(&c)
            }
        })
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
    let mut page = page;
    annotate(&s.config(), &g, &mut page.events);
    Ok(Json(page))
}

/// 每条规则命中了几次。只读索引（`security_events_rule_hits`），不翻日志
async fn counts(
    State(s): State<ControlState>,
    Query(q): Query<tw_api::SecurityRuleHitsQuery>,
) -> Result<Json<Vec<tw_api::SecurityRuleHits>>, Fail> {
    let store = crate::need_store(&s)?;
    let g = store.lock().await;
    let hits = g
        .db()
        .security_rule_hits(q.guard.slug(), q.since_ms.unwrap_or(0))
        .map_err(crate::records)?;
    Ok(Json(hits))
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

/// 一次启用或停用几条规则（组标题上的复选框）：内置的按 id 改启停名单，自定义的改各自的
/// `disabled`，**写一次文件**。和出厂一样的不写。认不出的内置规则、没有的自定义规则是 404，
/// 一条都不改。
async fn set_rules_enabled(
    State(s): State<ControlState>,
    Path(guard): Path<String>,
    Json(req): Json<tw_api::RulesToggle>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let guard = Guard::parse(&guard)?;
    let mut defaults = Vec::with_capacity(req.builtin.len());
    for id in &req.builtin {
        defaults.push((
            id.clone(),
            guard.on_by_default(id).ok_or_else(|| unknown_rule(id))?,
        ));
    }
    // 自定义规则先按现在的配置查一遍：没有的当场说，不写半截
    let known = guard.custom_names(&s.config());
    if let Some(missing) = req.custom.iter().find(|n| !known.contains(n)) {
        return Err(unknown_custom_rule(missing));
    }
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let (enable, disable) = guard.lists(cfg);
            let ids: Vec<&String> = defaults.iter().map(|(id, _)| id).collect();
            let mut enable: Vec<String> = enable
                .iter()
                .filter(|x| !ids.contains(x))
                .cloned()
                .collect();
            let mut disable: Vec<String> = disable
                .iter()
                .filter(|x| !ids.contains(x))
                .cloned()
                .collect();
            for (id, on_by_default) in &defaults {
                // **只记和出厂不一样的**
                if req.enabled != *on_by_default {
                    let list = if req.enabled {
                        &mut enable
                    } else {
                        &mut disable
                    };
                    if !list.contains(id) {
                        list.push(id.clone());
                    }
                }
            }
            let mut text = if defaults.is_empty() {
                text.to_string()
            } else {
                let text = put_list(text, &guard.path("enable"), &enable)?;
                put_list(&text, &guard.path("disable"), &disable)?
            };
            for name in &req.custom {
                // 查过之后被别处删掉的，跳过
                let Some(m) = guard.toggled_custom(cfg, name, req.enabled) else {
                    continue;
                };
                let m = m.map_err(|f| ApplyError::Invalid(f.1.0))?;
                text = edit::upsert(&text, guard.custom_section(), Some(name), &m)?;
            }
            Ok(text)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

fn unknown_custom_rule(name: &str) -> Fail {
    fail(
        StatusCode::NOT_FOUND,
        msg!(
            "security.unknown_custom_rule", rule = name.to_string() =>
            "There is no custom rule `{rule}`."
        ),
    )
}

/// 契约里的规则动作，出站脱敏认的那两个。
fn redact_action_of(a: RuleAction) -> Result<RedactAction, Fail> {
    a.redact().ok_or_else(|| not_a_redact_action(a))
}

fn not_a_redact_action(a: RuleAction) -> Fail {
    fail(
        StatusCode::BAD_REQUEST,
        msg!(
            "security.redact_action_unknown", action = a.slug() =>
            "`{action}` is not an action; it is placeholder or record."
        ),
    )
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

/// 契约里的规则动作，内容过滤认的那四个。
fn content_action_of(a: RuleAction) -> Result<ContentAction, Fail> {
    a.content().ok_or_else(|| not_a_content_action(a))
}

fn not_a_content_action(a: RuleAction) -> Fail {
    fail(
        StatusCode::BAD_REQUEST,
        msg!(
            "security.content_action_bad", action = a.slug() =>
            "`{action}` is not an action; it is block, strip, replace or record."
        ),
    )
}

/// 改一条内置规则在第三档下做什么。
///
/// **内置规则提供的只是一条判据。**命中之后怎么处理，和自定义规则一样由用户
/// 定 —— 不必为了改动作先复制成一条自定义规则。和出厂一样的就把那一行删掉。
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
        Guard::Redact => {
            tw_guard::redact::rules::builtin(&id).ok_or_else(|| unknown_rule(&id))?;
            (
                redact_action_of(req.action)?.slug(),
                RedactAction::default().slug(),
            )
        }
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
                matching: req.matching.unwrap_or_default(),
                case_sensitive: req.case_sensitive.unwrap_or(false),
                // 出厂的标签和没写一样，不写进文件
                label: req
                    .label
                    .clone()
                    .filter(|l| l != tw_guard::policy::DEFAULT_LABEL),
                action: req
                    .action
                    .map(redact_action_of)
                    .transpose()?
                    .unwrap_or_default(),
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
            let action = req
                .action
                .map(content_action_of)
                .transpose()?
                .unwrap_or_default();
            let rule = CustomContentRule {
                name,
                pattern,
                scope: req
                    .scope
                    .clone()
                    .map(dedup_scope)
                    .unwrap_or_else(tw_guard::policy::request_only),
                matching: req.matching.unwrap_or_default(),
                case_sensitive: req.case_sensitive.unwrap_or(false),
                action,
                // 替换文本只在动作是替换时有意义：别的动作不写进文件（界面把出厂的 `***` 填在
                // 框里交上来，写进去就是多出来的一行）
                replacement: req
                    .replacement
                    .clone()
                    .filter(|_| action == ContentAction::Replace)
                    .unwrap_or_else(|| tw_guard::policy::DEFAULT_REPLACEMENT.to_string()),
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

/// 查哪一边：按 `request`、`response` 的顺序，重复的只留一个
fn dedup_scope(mut scope: Vec<tw_api::ContentScope>) -> Vec<tw_api::ContentScope> {
    scope.sort();
    scope.dedup();
    scope
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
        PolicyError::BadMatch { matching, .. } => bad_match(matching),
        PolicyError::EmptyScope { .. } => scope_empty(),
        PolicyError::BlockInAnswer { .. } => block_in_answer(),
        PolicyError::CodepointsInAnswer { .. } => codepoints_in_answer(),
        // 只有一条规则的策略里不会有：名字重复由写文件那一步说，内置规则的 id、例外表不在这里写
        e @ (PolicyError::DuplicateName { .. }
        | PolicyError::UnknownRule { .. }
        | PolicyError::TooManyExceptions { .. }
        | PolicyError::BadExceptionHash { .. }
        | PolicyError::DuplicateException { .. }) => tw_config::policy_msg(&e),
    };
    fail(StatusCode::BAD_REQUEST, m)
}

/// 出站脱敏的规则按原样找或者按正则，码位只有内容过滤认
fn bad_match(matching: tw_api::ContentMatch) -> tw_types::Msg {
    msg!(
        "security.redact_match_bad", matching = matching.slug() =>
        "A redaction rule matches by contains or regex, not `{matching}`."
    )
}

fn scope_empty() -> tw_types::Msg {
    msg!(
        "security.rule_scope_empty" =>
        "A content rule checks requests, answers or both; choose at least one."
    )
}

fn block_in_answer() -> tw_types::Msg {
    msg!(
        "security.rule_scope_block" =>
        "A rule that checks answers cannot block: an answer is already on its way when a match is \
         found. Choose strip, replace or record."
    )
}

fn codepoints_in_answer() -> tw_types::Msg {
    msg!(
        "security.rule_scope_codepoints" =>
        "Code points are checked in requests only; a rule that checks answers matches by contains \
         or regex."
    )
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

// ---------------------------------------------------------------- 例外值

/// 例外表那一节：按 `sha256` 认
const EXCEPTIONS: edit::Section = edit::Section {
    path: &["security", "redact", "exceptions"],
    what: "redaction exception",
    key: "sha256",
};

/// 安全日志的记录按现在的例外表标上 `excepted`、`exceptable`（见文件头）。**每个返回日志
/// 记录的端点都要过一遍**：哈希不在库里，在记录器的内存里，只有最近的那些行有
/// （[`tw_store::Recorder::value_hash`]）；不在内存里的两样都是 false
pub(crate) fn annotate<'a>(
    cfg: &tw_config::Config,
    rec: &tw_store::Recorder,
    views: impl IntoIterator<Item = &'a mut tw_api::SecurityEventView>,
) {
    let set = cfg.security.redact.exception_set();
    for v in views {
        if v.guard != Guard::Redact {
            continue;
        }
        let Some(Some(hash)) = rec.value_hash(v.id) else {
            continue;
        };
        v.excepted = set.as_ref().is_some_and(|ex| ex.contains_hex(hash));
        v.exceptable = !v.excepted;
    }
}

async fn exceptions(State(s): State<ControlState>) -> Json<tw_api::RedactExceptions> {
    Json(tw_api::RedactExceptions {
        items: s.config().security.redact.exceptions.clone(),
        max: tw_guard::policy::EXCEPTIONS_MAX as u32,
    })
}

/// 手动加入：交来的是原值。**算完哈希它就没用了**，不进任何一条日志、不进回答
async fn add_exception(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::ExceptionAdd>,
) -> Result<Json<tw_api::ExceptionAdded>, Fail> {
    // 网关手里那一份保存的凭据：贴进来一把上游的密钥，认得出它、不收
    let saved = s.gateway.runtime().redact.saved().cloned();
    let e = s
        .config()
        .security
        .redact
        .exception_of(&req.value, saved, crate::config::now_ms() as i64)
        .map_err(exception_fail)?;
    drop(req.value);
    put_exception(&s, e, req.base_version.as_deref()).await
}

/// 从安全日志的一条加入：哈希、打码后的样子、规则都取自那一行
async fn add_exception_from_event(
    State(s): State<ControlState>,
    Path(id): Path<i64>,
    Json(req): Json<tw_api::BaseVersion>,
) -> Result<Json<tw_api::ExceptionAdded>, Fail> {
    let store = crate::need_store(&s)?;
    let (src, hash) = {
        let g = store.lock().await;
        let src = g
            .db()
            .exception_source(id)
            .map_err(crate::records)?
            .ok_or_else(|| {
                fail(
                    StatusCode::NOT_FOUND,
                    msg!(
                        "security.unknown_event", id = id.to_string() =>
                        "There is no security log entry {id}; it may have passed the retention \
                         period."
                    ),
                )
            })?;
        let hash = g.value_hash(id).map(|h| h.map(str::to_string));
        (src, hash)
    };
    let not_exceptable = || {
        fail(
            StatusCode::BAD_REQUEST,
            msg!(
                "security.exception_not_exceptable" =>
                "Only a value found by outbound redaction can be an exception, and a saved \
                 secret or a span joined from overlapping matches cannot."
            ),
        )
    };
    let sha256 = match (src.guard, hash) {
        (Guard::Redact, Some(Some(h))) => h,
        // 工具调用审查、内容过滤的记录没有可以例外的「值」；保存的凭据、几处重叠合成的一段
        // 列为例外也挡不住什么
        (Guard::Redact, Some(None)) | (Guard::InspectTools | Guard::Content, _) => {
            return Err(not_exceptable());
        }
        // 记下时的哈希只在内存里留最近的一批，重启就没了（见 `tw_store::Recorder::value_hash`）
        (Guard::Redact, None) => {
            return Err(fail(
                StatusCode::CONFLICT,
                msg!(
                    "security.exception_forgotten" =>
                    "This entry is too old to be listed as an exception from the log; paste the \
                     value into Add exception instead."
                ),
            ));
        }
    };
    let e = tw_config::RedactException {
        sha256,
        masked: src.masked,
        rule: src.rule,
        added_at: crate::config::now_ms() as i64,
    };
    put_exception(&s, e, req.base_version.as_deref()).await
}

/// 写进例外表。**已经在表里的不再写一次**（不论是哪条规则加的）：回那一项和现在的版本。
/// 满了是 409。
///
/// 两样都在写入那一步里、对着那一刻的配置查：两个「列为例外」同时到，后写的那个读到的是先写
/// 完的那一份。写的时候磁盘上的配置被别人改了（[`tw_config::StoreError::Conflict`]）、而调用方
/// 没带版本时重来一次，重来时就看见了先写进去的那一项
async fn put_exception(
    s: &ControlState,
    e: tw_config::RedactException,
    base_version: Option<&str>,
) -> Result<Json<tw_api::ExceptionAdded>, Fail> {
    let m = item(&e)?;
    let mut tries = 0;
    loop {
        tries += 1;
        // 已经在表里：那一项和没改动的那一版
        let mut have: Option<(tw_config::RedactException, String)> = None;
        let r = s
            .cfg
            .transform(base_version, Origin::Ui, |text, cfg| {
                let list = &cfg.security.redact.exceptions;
                if let Some(x) = list.iter().find(|x| x.sha256 == e.sha256) {
                    have = Some((x.clone(), tw_config::version_of(text)));
                    return Err(ApplyError::InUse(already_listed()));
                }
                if list.len() >= tw_guard::policy::EXCEPTIONS_MAX {
                    return Err(ApplyError::InUse(msg!(
                        "security.exceptions_full", max = tw_guard::policy::EXCEPTIONS_MAX.to_string() =>
                        "There are already {max} exceptions; remove one before adding another."
                    )));
                }
                Ok(edit::upsert(text, EXCEPTIONS, None, &m)?)
            })
            .await;
        if let Some((exception, version)) = have {
            return Ok(Json(tw_api::ExceptionAdded { version, exception }));
        }
        match r {
            Ok(version) => {
                return Ok(Json(tw_api::ExceptionAdded {
                    version,
                    exception: e,
                }));
            }
            Err(ApplyError::Store(tw_config::StoreError::Conflict { .. }))
                if base_version.is_none() && tries < 3 => {}
            Err(err) => return Err(apply_fail(err)),
        }
    }
}

/// 只在 [`put_exception`] 里头用来停下那次写入，不会到界面上（已经在表里的回 200），所以
/// 没有码
fn already_listed() -> tw_types::Msg {
    tw_types::Msg {
        code: String::new(),
        args: Default::default(),
        text: String::new(),
    }
}

async fn remove_exception(
    State(s): State<ControlState>,
    Path(sha256): Path<String>,
    Query(q): Query<tw_api::BaseVersion>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(q.base_version.as_deref(), Origin::Ui, |text, _| {
            Ok(edit::remove(text, EXCEPTIONS, &sha256)?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

/// 一个值加不进例外表
fn exception_fail(e: tw_guard::policy::ExceptionError) -> Fail {
    use tw_guard::policy::ExceptionError;
    let m = match e {
        ExceptionError::Empty => {
            msg!("security.exception_value_empty" => "The value is empty.")
        }
        ExceptionError::Saved => msg!(
            "security.exception_saved_secret" =>
            "This value is a secret saved in ThinkWatch; saved secrets are always replaced and \
             cannot be exceptions."
        ),
        // 配置读进来时校验过，到这里还编不起来是绕过校验写进去的：那是这边的问题
        ExceptionError::Policy(e) => return internal(e),
    };
    fail(StatusCode::BAD_REQUEST, m)
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
/// 和网关手里那一份是同一份，保存的凭据也是网关手里那一份。
async fn test(
    State(s): State<ControlState>,
    Path(guard): Path<String>,
    Json(req): Json<tw_api::SecurityTestRequest>,
) -> Result<Json<tw_api::SecurityTestResult>, Fail> {
    let guard = Guard::parse(&guard)?;
    // 网关手里那一份保存的凭据：贴进来一把上游的密钥，试出来的和真的请求一样
    let saved = s.gateway.runtime().redact.saved().cloned();
    tw_guard::trial::run_with(guard, &s.config().security, &req, saved)
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
                Guard::Redact => not_a_redact_action(action),
            };
        }
        // 配置读进来时校验过，到这里还编不起来是绕过校验写进去的：那是这边的问题
        TrialError::Policy(e) => return internal(e),
        TrialError::EmptyPattern => pattern_empty(),
        TrialError::BadPattern { detail } => bad_pattern(guard, detail),
        TrialError::BadCodepoints { reason } => bad_codepoints(reason),
        TrialError::BadLabel { label } => bad_label(label),
        TrialError::BadMatch { matching } => bad_match(matching),
        TrialError::BlockInAnswer => block_in_answer(),
        TrialError::CodepointsInAnswer => codepoints_in_answer(),
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
            case_sensitive: None,
            scope: None,
            replacement: None,
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
        // 怎么认、分不分大小写：和出厂（按原样找、不分）不一样才写
        let m = custom_item(
            Guard::Redact,
            &tw_api::CustomRuleSave {
                matching: Some(tw_api::ContentMatch::Regex),
                case_sensitive: Some(true),
                ..save("a", "x+")
            },
        )
        .unwrap();
        assert_eq!(
            yaml(&m),
            "name: a\npattern: x+\nmatch: regex\ncase_sensitive: true\n"
        );
        let m = custom_item(
            Guard::Redact,
            &tw_api::CustomRuleSave {
                matching: Some(tw_api::ContentMatch::Contains),
                case_sensitive: Some(false),
                ..save("a", "x+")
            },
        )
        .unwrap();
        assert_eq!(yaml(&m), "name: a\npattern: x+\n");
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
        // 查哪一边按 request、response 的顺序写，重复的去掉；替换文本只在动作是替换时写
        let m = custom_item(
            Guard::Content,
            &tw_api::CustomRuleSave {
                scope: Some(vec![
                    tw_api::ContentScope::Response,
                    tw_api::ContentScope::Request,
                    tw_api::ContentScope::Response,
                ]),
                case_sensitive: Some(true),
                action: Some(RuleAction::Replace),
                replacement: Some("[X]".into()),
                ..save("代号", "北极星")
            },
        )
        .unwrap();
        assert_eq!(
            yaml(&m),
            "name: 代号\npattern: 北极星\nscope:\n- request\n- response\ncase_sensitive: true\naction: replace\nreplacement: '[X]'\n"
        );
        let m = custom_item(
            Guard::Content,
            &tw_api::CustomRuleSave {
                scope: Some(vec![tw_api::ContentScope::Request]),
                action: Some(RuleAction::Strip),
                replacement: Some("[X]".into()),
                ..save("代号", "北极星")
            },
        )
        .unwrap();
        assert_eq!(yaml(&m), "name: 代号\npattern: 北极星\naction: strip\n");
        let m = custom_item(
            Guard::Redact,
            &tw_api::CustomRuleSave {
                action: Some(RuleAction::Record),
                ..save("a", "x")
            },
        )
        .unwrap();
        assert_eq!(yaml(&m), "name: a\npattern: x\naction: record\n");
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
            (
                Guard::Redact,
                tw_api::CustomRuleSave {
                    matching: Some(tw_api::ContentMatch::Regex),
                    ..save("a", "(")
                },
                "security.bad_pattern",
            ),
            (
                Guard::Redact,
                tw_api::CustomRuleSave {
                    matching: Some(tw_api::ContentMatch::Codepoints),
                    ..save("a", "x")
                },
                "security.redact_match_bad",
            ),
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
                "security.content_action_bad",
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
        assert_eq!(
            run(
                Guard::Redact,
                tw_api::SecurityTestRequest {
                    matching: Some(tw_api::ContentMatch::Regex),
                    ..req("(")
                }
            ),
            "security.bad_pattern"
        );
        assert_eq!(
            run(
                Guard::Redact,
                tw_api::SecurityTestRequest {
                    matching: Some(tw_api::ContentMatch::Codepoints),
                    ..req("x")
                }
            ),
            "security.redact_match_bad"
        );
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
                    action: Some(RuleAction::Cut),
                    ..req("x")
                }
            ),
            "security.redact_action_unknown"
        );
        assert_eq!(
            run(
                Guard::Content,
                tw_api::SecurityTestRequest {
                    action: Some(RuleAction::Block),
                    scope: Some(tw_api::ContentScope::Response),
                    ..req("x")
                }
            ),
            "security.rule_scope_block"
        );
        assert_eq!(
            run(
                Guard::Content,
                tw_api::SecurityTestRequest {
                    action: Some(RuleAction::Cut),
                    ..req("x")
                }
            ),
            "security.content_action_bad"
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
