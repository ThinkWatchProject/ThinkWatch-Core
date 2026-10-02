//! 请求钩子：客户端的请求发出去之前，按顺序交给范围内的插件改。
//!
//! # 位置和次数
//!
//! 排在本地应答之后、读路由事实之前（不变式 I7）：插件改过的请求体重新解码，路由、
//! 内容审查、会话指纹、出站脱敏看到的都是改过的那一份；`params.model` 改了，路由
//! 就按新的模型走。**一个客户端请求只跑一次**（I8）：故障转移、OAuth 重试、去封存
//! 重发用的都是这一份结果。
//!
//! # 每个插件一步
//!
//! 1. 读出这一刻的请求（前一个插件改过的话就是改过的）的视图，按权限裁掉没给的部分；
//! 2. 认得出的密钥换成占位符（[`super::bridge`]）；
//! 3. 在插件线程池上调 `onRequest`；
//! 4. 核对交回来的东西（[`super::view::check`]），占位符换回去，写回原文。
//!
//! 插件 `reject` 了，这个请求就被拒；出错了（沙箱报错、交回来的东西不合规矩）按它的
//! `on_error`：拒绝这个请求，或者跳过它接着往下走。文件变了、装不上的插件跑不了，
//! 范围内的请求同样按 `on_error` 处理 —— **只看客户端和模型**：它要是只有回答钩子、
//! 只管某几家上游，这时候还不知道会去哪一家，宁可多拦（这是插件坏着的时候，用户
//! 会收到通知）。

use std::sync::Arc;

use bytes::Bytes;
use serde_json::{Value, json};
use tw_api::{OnError, PluginHook, PluginOutcome};
use tw_dialect::ir::Dialect;
use tw_types::{Msg, msg};

use super::bridge::Bridge;
use super::host::{RequestOutcome, RunError};
use super::pool::Pool;
use super::set::{Active, Broken, LogLine, PluginRun, PluginSet};
use super::view;

/// 请求钩子跑完之后交回管线的东西。
#[derive(Default)]
pub struct Plugged {
    /// 每个跑过（或者该跑没跑）的插件一条，按顺序，连同它写的日志。**请求的号这时
    /// 还没发**，开始事件之后再交出去（见 [`record`]）
    pub runs: Vec<(Arc<Active>, PluginRun, Vec<LogLine>)>,
    /// 插件改过之后的请求体和路径（Gemini 换了模型时路径也变）
    pub body: Option<Bytes>,
    pub path: Option<String>,
    /// 这个请求的密钥映射。回答钩子接着用它：同一个值在两头是同一个占位符
    pub bridge: Option<Bridge>,
    /// 客户端要的模型（插件改之前的）
    pub model: String,
}

impl Plugged {
    pub fn changed(&self) -> bool {
        self.body.is_some()
    }
}

/// 请求被插件拒了：回给客户端的错误，和到这一步为止的记录。
pub struct Refused {
    pub why: Msg,
    pub plugged: Plugged,
}

/// 这个请求是谁发的、要什么：插件的 `ctx` 和范围都看它。
pub struct Asked<'a> {
    pub dialect: Dialect,
    /// 客户端调的路径（Gemini 的模型在里面）
    pub path: &'a str,
    /// 客户端是哪个应用（请求那一行上记的那个，认不出是 `None`）
    pub client: Option<&'a str>,
}

/// 客户端要的模型：请求体里的 `model`，Gemini 写在路径里。
pub fn asked_model(dialect: Dialect, path: &str, raw: Option<&Value>) -> String {
    if dialect == Dialect::Gemini {
        return path
            .split_once("/models/")
            .and_then(|(_, rest)| rest.rsplit_once(':'))
            .map(|(m, _)| m.to_string())
            .unwrap_or_default();
    }
    raw.and_then(|v| v.get("model"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// 插件看到的 `ctx`
pub fn ctx(
    client: Option<&str>,
    model: &str,
    dialect: Dialect,
    upstream: Option<&str>,
    settings: &serde_json::Map<String, Value>,
) -> Value {
    json!({
        "client": client,
        "model": model,
        "format": super::format_name(dialect),
        "upstream": upstream,
        "settings": settings,
    })
}

/// 跑请求钩子。范围内一个插件都没有时什么都不做，连请求体都不解析。
pub async fn run(
    pool: &Pool,
    set: &PluginSet,
    rules: &Arc<tw_guard::redact::rules::RuleSet>,
    asked: &Asked<'_>,
    body: &Bytes,
) -> Result<Plugged, Box<Refused>> {
    let mut out = Plugged::default();
    if set.is_empty() {
        return Ok(out);
    }
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let model = asked_model(asked.dialect, asked.path, parsed.as_ref());
    out.model = model.clone();
    let here = set.for_request(asked.client, &model);
    // 回答钩子要用这个请求的密钥映射：范围里有回答钩子的话，现在就记账
    let later = set.all().iter().any(|a| {
        a.enabled
            && a.ready().is_some()
            && a.hooks.on_reply()
            && a.scope.covers_request(asked.client, &model)
    });
    if here.is_empty() && !later {
        return Ok(out);
    }
    let mut bridge = Bridge::new(rules.clone());
    bridge.learn(body);
    if here.is_empty() {
        out.bridge = Some(bridge);
        return Ok(out);
    }
    let mut raw = parsed;
    let mut path = asked.path.to_string();
    let mut changed = false;
    for a in here {
        let host = match &a.state {
            super::set::State::Broken(why) => {
                let (outcome, refusal) = broken(a.on_error, &a.name, why);
                let run = PluginRun {
                    plugin_id: a.id.clone(),
                    plugin_name: a.name.clone(),
                    hook: PluginHook::Request,
                    outcome,
                    error: Some(broken_reason(&a.name, why)),
                    cpu_us: 0,
                    detail: None,
                };
                out.runs.push((a.clone(), run, Vec::new()));
                if let Some(why) = refusal {
                    out.bridge = Some(bridge);
                    return Err(Box::new(Refused { why, plugged: out }));
                }
                continue;
            }
            super::set::State::Ready(h) => h.clone(),
        };
        let mut run = PluginRun {
            plugin_id: a.id.clone(),
            plugin_name: a.name.clone(),
            hook: PluginHook::Request,
            outcome: PluginOutcome::Unchanged,
            error: None,
            cpu_us: 0,
            detail: None,
        };
        let mut logs = Vec::new();
        let result: Result<Option<Rewritten>, Failure> = async {
            let Some(current) = raw.as_ref() else {
                return Err(Failure::Unreadable("the request body is not JSON".into()));
            };
            let built = view::build(asked.dialect, current, &path).map_err(Failure::Unreadable)?;
            let mut input = view::trim(&built.view, &a.permissions);
            bridge.hide_value(&mut input);
            let ctx = ctx(asked.client, &model, asked.dialect, None, &a.settings);
            let (h, given) = (host.clone(), input.clone());
            let inv = pool
                .run(move || h.on_request(given, ctx))
                .await
                .map_err(|e| Failure::Run(RunError::Trap(e.to_string())))?;
            run.cpu_us = inv.cpu.as_micros().min(u64::MAX as u128) as u64;
            logs = inv.logs;
            match inv.result.map_err(Failure::Run)? {
                RequestOutcome::Unchanged => Ok(None),
                RequestOutcome::Rejected(reason) => Err(Failure::Rejected(reason)),
                RequestOutcome::Changed(returned) => {
                    let mut edits =
                        view::check(&input, &returned, &a.permissions, built.src.hidden_tools())
                            .map_err(Failure::Edit)?;
                    if edits.is_empty() {
                        return Ok(None);
                    }
                    let sections = sections(&edits);
                    edits.reveal(&bridge);
                    let mut next = current.clone();
                    let new_path =
                        view::apply(&mut next, &built.src, &edits, &path).map_err(Failure::Edit)?;
                    Ok(Some((next, new_path, sections)))
                }
            }
        }
        .await;
        match result {
            Ok(None) => {}
            Ok(Some((next, new_path, sections))) => {
                raw = Some(next);
                if let Some(p) = new_path {
                    path = p;
                }
                changed = true;
                run.outcome = PluginOutcome::Changed;
                run.detail = Some(json!({ "changed": sections }));
            }
            Err(Failure::Rejected(reason)) => {
                run.outcome = PluginOutcome::Rejected;
                run.error = Some(reason_msg(&reason));
                out.runs.push((a.clone(), run, logs));
                out.bridge = Some(bridge);
                return Err(Box::new(Refused {
                    why: rejected(&a.name, reason),
                    plugged: out,
                }));
            }
            Err(f) => {
                let why = f.msg();
                run.outcome = PluginOutcome::Error;
                run.error = Some(why.clone());
                out.runs.push((a.clone(), run, logs));
                if a.on_error == OnError::Reject {
                    out.bridge = Some(bridge);
                    return Err(Box::new(Refused {
                        why: msg!(
                            "gw.plugin.request_failed",
                            plugin = a.name.clone(), detail = why.text =>
                            "Plugin `{plugin}` failed, so the request was not sent: {detail}"
                        ),
                        plugged: out,
                    }));
                }
                continue;
            }
        }
        out.runs.push((a.clone(), run, logs));
    }
    if changed && let Some(v) = &raw {
        match serde_json::to_vec(v) {
            Ok(b) => {
                out.body = Some(Bytes::from(b));
                if path != asked.path {
                    out.path = Some(path);
                }
            }
            // 序列化不该失败；真失败了就当没改过，不发半个请求体
            Err(e) => {
                tracing::error!("the request changed by plugins could not be serialized: {e}")
            }
        }
    }
    out.bridge = Some(bridge);
    Ok(out)
}

/// 跑不了的插件：记成什么，要不要拒掉这个请求
fn broken(on_error: OnError, name: &str, why: &Broken) -> (PluginOutcome, Option<Msg>) {
    if on_error == OnError::Skip {
        return (PluginOutcome::Skipped, None);
    }
    let refusal = match why {
        Broken::Changed => msg!(
            "gw.plugin.changed", plugin = name =>
            "Plugin `{plugin}` changed on disk and has not been approved again, so the request \
             was not sent."
        ),
        Broken::Error(detail) => msg!(
            "gw.plugin.unavailable", plugin = name, detail = detail.text.clone() =>
            "Plugin `{plugin}` could not be loaded, so the request was not sent: {detail}"
        ),
    };
    (PluginOutcome::Error, Some(refusal))
}

/// 跑不了的原因，记在这一次运行上：和插件变成跑不了时那条通知同一句
fn broken_reason(name: &str, why: &Broken) -> Msg {
    match why {
        Broken::Changed => super::load::file_changed(name),
        Broken::Error(m) => m.clone(),
    }
}

/// 插件拒绝了请求：报给客户端的那一句
pub(super) fn rejected(plugin: &str, reason: String) -> Msg {
    msg!(
        "gw.plugin.rejected", plugin = plugin, reason = reason =>
        "Plugin `{plugin}` refused this request: {reason}"
    )
}

/// 插件拒绝时说的原因，原样记在这一次运行上
fn reason_msg(reason: &str) -> Msg {
    msg!("gw.plugin.reason", reason = reason => "{reason}")
}

/// 请求读不成插件的视图
pub(super) fn request_unreadable(detail: impl Into<String>) -> Msg {
    msg!(
        "gw.plugin.request_unreadable", detail = detail.into() =>
        "The request could not be read for the plugin: {detail}"
    )
}

/// 改了哪几部分，记在这一条的 `detail` 里
fn sections(e: &view::Edits) -> Vec<&'static str> {
    let mut s = Vec::new();
    if e.system.is_some() {
        s.push("system");
    }
    if e.messages.is_some() {
        s.push("messages");
    }
    if e.tools.is_some() {
        s.push("tools");
    }
    if e.params.as_ref().is_some_and(|p| !p.is_empty()) {
        s.push("params");
    }
    s
}

/// 一个插件改过的请求：新的原文、新的路径（换了的话）、改了哪几部分
type Rewritten = (Value, Option<String>, Vec<&'static str>);

/// 一个插件没跑成。
enum Failure {
    Rejected(String),
    Run(RunError),
    Edit(view::EditError),
    /// 请求读不成视图
    Unreadable(String),
}

impl Failure {
    /// 记在这次运行上的那一句
    fn msg(&self) -> Msg {
        match self {
            Failure::Rejected(r) => reason_msg(r),
            Failure::Run(e) => e.msg(),
            Failure::Edit(e) => e.msg(),
            Failure::Unreadable(detail) => request_unreadable(detail.clone()),
        }
    }
}

/// 请求那一行有了号之后，把请求钩子的记录交出去：每一次运行（计数、日志、失败的通知，
/// 见 [`crate::AppState::plugin_ran`]）。改过的请求体由开始事件那一步另外交给存请求体的
/// 那一层
pub fn record(state: &crate::AppState, id: u64, plugged: &Plugged) {
    for (a, run, logs) in &plugged.runs {
        state.plugin_ran(id, a, run.clone(), logs.clone());
    }
}
