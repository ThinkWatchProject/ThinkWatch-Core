//! 请求钩子：请求发往一个上游之前，按顺序交给管这一次的插件改。
//!
//! # 位置和次数（契约附录二）
//!
//! 排在路由之后：路由、模型准入、会话指纹看的都是客户端的原话，插件左右不了请求去
//! 哪一家。**每发往一个上游跑一次**：管线每试一家（[`Hook::attempt`]），按这一次的
//! 客户端、发给这一家的模型名和这一家挑出管它的插件，从客户端的原话起改 —— 换到下一
//! 家时重新从原话起，给上一家的改动到不了下一家。同一家重发（OAuth 换 token、去封存）
//! 用这一跳已经定好的请求体，不重跑。
//!
//! 插件看到的 `model`（视图里的和 `params.model`）、`ctx.model` 都是**发给这一家的
//! 模型名**（路由规则改写之后的），`ctx.requested_model` 是客户端要的，`ctx.upstream`
//! 是这一家。插件改了 `params.model`，只是换掉发给这一家的名字：不重新路由，也不再对
//! 一遍上游的模型清单。
//!
//! # 每个插件一步
//!
//! 1. 读出这一刻的请求（前一个插件改过的话就是改过的）的视图，按权限裁掉没给的部分；
//! 2. 认得出的密钥换成占位符（[`super::bridge`]）；
//! 3. 在插件线程池上调 `onRequest`；
//! 4. 核对交回来的东西（[`super::view::Src::check`]，按这种请求的规矩），占位符换回去，
//!    写回原文。
//!
//! 插件 `reject` 了，或者出错而它的 `on_error` 是拒绝，**整个请求被拒**，不换下一家：
//! 换一家，管它的还是这个插件。文件变了、装不上的插件跑不了，管得着这一次的同样按
//! `on_error` 处理；只管别的上游、别的模型的，这一次不算它。
//!
//! # 哪些请求过插件
//!
//! 按客户端调的接口分成几种（见 [`Shape`]），**插件只处理它声明了的那几种**（manifest
//! 的 `requests`，不写是只有对话）：
//!
//! - 对话 —— 生成回答：上面说的那样；数 token（Anthropic 的 `count_tokens`、Gemini 的
//!   `:countTokens`、Responses 的 `input_tokens`）、Responses 的压缩：请求体就是一段对话，
//!   插件照样看、照样改，上游数的、压的是改过的那一份 —— 插件删掉的东西不能从旁边的接口
//!   漏出去。网关自己估数、一个字节都不发的那几种不跑插件；
//! - 嵌入（OpenAI 的 `/v1/embeddings`，Gemini 的 `:embedContent`、`:batchEmbedContents`）、
//!   旧版补全（OpenAI 的 `/v1/completions`）：一项输入一条消息，只改得了文字（见
//!   [`super::view::inputs`]）。回答钩子不在它们上面跑；
//! - 别的接口（图片、音频、认不出的）：不属于任何一种，**所有插件都不管**。
//!
//! 没声明这一种的插件**不在这一次的范围里**：请求原样过去，什么都不记，它出错、文件
//! 变了、装不上也拦不着这种请求 —— 不管它的 `on_error` 是什么。声明了的那几种里，请求体
//! 读不出来（不是 JSON 之类）时，管得着的插件按它的 `on_error`：拒绝就拒掉整个请求，跳过
//! 就原样发、记一笔跳过（`gw.plugin.cannot_read_body`）。

use std::borrow::Cow;
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

/// 一个插件在这一次上的运行，连同它写的日志。
pub type Ran = (Arc<Active>, PluginRun, Vec<LogLine>);

/// 插件怎么看一个请求体：按客户端调的接口分（见 [`crate::client_api::ClientApi`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// 生成回答（一段对话）
    Generate,
    /// 请求体和生成回答同一种形状、却不生成回答的接口：数 token、Responses 的压缩（见
    /// [`crate::client_api::ClientApi::like_generation`]）。也算对话：插件照样看、照样改，
    /// **`params` 里只写回模型名** —— 这些接口不收输出上限、温度这些参数（带上是一个
    /// 400），数出来的 token 也和它们无关
    Alike,
    /// 嵌入：OpenAI 的 `/v1/embeddings`，Gemini 的 `:embedContent`、`:batchEmbedContents`
    Embeddings,
    /// 旧版补全：OpenAI 的 `/v1/completions`
    Completions,
    /// 别的接口（图片、音频、认不出的……）：**不属于任何一种，插件一律不管**
    Other,
}

impl Shape {
    /// 客户端调的这个路径是哪一种
    pub fn of(path: &str) -> Shape {
        use crate::client_api::ClientApi;
        if ClientApi::of_path(path).is_none() {
            Shape::Other
        } else if ClientApi::generates(path) {
            Shape::Generate
        } else if ClientApi::like_generation(path) {
            Shape::Alike
        } else if ClientApi::embeds(path) {
            Shape::Embeddings
        } else if ClientApi::completes(path) {
            Shape::Completions
        } else {
            Shape::Other
        }
    }

    /// 插件怎么读这种请求体。`dialect` 是客户端的格式。**插件不管的接口是 None**
    pub fn form(self, dialect: Dialect) -> Option<view::Form> {
        match self {
            Shape::Generate | Shape::Alike => Some(view::Form::Conversation(dialect)),
            Shape::Embeddings if dialect == Dialect::Gemini => Some(view::Form::GeminiEmbed),
            Shape::Embeddings => Some(view::Form::OpenaiEmbeddings),
            Shape::Completions => Some(view::Form::OpenaiCompletions),
            Shape::Other => None,
        }
    }
}

/// 一个请求上的请求钩子。管线每发往一个上游调一次 [`Hook::attempt`]，**每次都从客户端
/// 的原话起**；原文只解析一次、密钥只编一次号，几次尝试共用。
pub struct Hook<'a> {
    set: &'a PluginSet,
    rules: Arc<tw_guard::redact::rules::RuleSet>,
    /// 客户端的格式
    dialect: Dialect,
    /// 客户端调的路径（Gemini 的模型在里面）
    path: &'a str,
    /// 按路径分出来的那一种
    shape: Shape,
    /// 客户端是哪个应用（请求那一行上记的那个，认不出是 `None`）
    client: Option<&'a str>,
    /// 客户端发来的原文
    body: &'a Bytes,
    /// 原文解析出来的 JSON。第一次有插件要跑时才解析
    parsed: Option<Option<Value>>,
    /// 原文读不读得成插件的视图：读不成时是原因。和 `parsed` 一起第一次要用时才看
    readable: Option<Result<(), String>>,
    /// 按原文编好号的那本账。第一次要用时才编
    base: Option<Bridge>,
}

/// 这一次发往哪儿。
pub struct Target<'a> {
    pub upstream: &'a str,
    /// 发给它的模型名：路由规则改写过的是改写之后的
    pub model: &'a str,
    /// 客户端要的模型
    pub requested_model: &'a str,
    /// 尝试链上的第几跳（从 0 起）。记在每一次运行的 `detail` 里，界面按它分组
    pub attempt: usize,
}

/// 一次尝试上请求钩子跑完之后交回管线的东西。
#[derive(Default)]
pub struct Plugged {
    /// 每个跑过（或者该跑没跑）的插件一条，按顺序
    pub runs: Vec<Ran>,
    /// 插件改过的话，改过之后的请求
    pub changed: Option<Changed>,
    /// 这一次的密钥映射：跑过插件、或者管这一次的插件里有回答钩子时才有。回答钩子
    /// 接着用它：同一个值在两头是同一个占位符
    pub bridge: Option<Bridge>,
}

/// 插件改过之后的请求：客户端那种格式，占位符已经换回原值。
pub struct Changed {
    pub body: Bytes,
    /// 改过之后的 JSON。管线要重新解码它（内容审查、格式转换）
    pub value: Value,
    /// 调的路径。Gemini 换了模型时是新的
    pub path: String,
    /// 插件改了 `params.model` 的话，发给这一家的新模型名
    pub renamed: Option<Renamed>,
    /// 嵌入、旧版补全：改前、改后请求防护各看哪一份（见 [`view::inputs::screenable`]）。
    /// 它们没有中间表示，管线拿这两份比，只看插件加进来的。对话是 None：管线自己解码
    pub screen: Option<(tw_dialect::ir::Request, tw_dialect::ir::Request)>,
}

/// 插件换了发给这一家的模型名。
pub struct Renamed {
    pub model: String,
    /// 最后改它的那个插件的名字（报错时说是谁）。**插件写的字**
    pub by: String,
}

/// 请求被插件拒了：回给客户端的那句话，和到这一步为止的记录。
pub struct Refused {
    pub why: Msg,
    pub runs: Vec<Ran>,
}

impl<'a> Hook<'a> {
    pub fn new(
        set: &'a PluginSet,
        rules: Arc<tw_guard::redact::rules::RuleSet>,
        dialect: Dialect,
        path: &'a str,
        client: Option<&'a str>,
        body: &'a Bytes,
    ) -> Self {
        Self {
            set,
            rules,
            dialect,
            path,
            shape: Shape::of(path),
            client,
            body,
            parsed: None,
            readable: None,
            base: None,
        }
    }

    /// 按客户端原文编好号的那本账（第一次调时才编）
    fn base(&mut self) -> Bridge {
        let (rules, body) = (&self.rules, self.body);
        self.base
            .get_or_insert_with(|| {
                let mut b = Bridge::new(rules.clone());
                b.learn(body);
                b
            })
            .clone()
    }

    /// 发往 `to` 之前跑一遍管这一次的插件。**从客户端的原话起**，上一次尝试改过什么
    /// 都不算。管这一次的一个都没有时什么都不做，连请求体都不解析：插件不管的接口、
    /// 插件没声明的那种请求都是这样 —— 原样发，什么都不记。
    pub async fn attempt(&mut self, pool: &Pool, to: &Target<'_>) -> Result<Plugged, Box<Refused>> {
        let mut out = Plugged::default();
        if self.set.is_empty() {
            return Ok(out);
        }
        let Some(form) = self.shape.form(self.dialect) else {
            return Ok(out);
        };
        let here = self
            .set
            .for_request(form.kind(), self.client, to.model, to.upstream);
        if here.is_empty() {
            // 回答钩子要这个请求的密钥映射：管这一次的里面有，就现在记账。不生成回答的
            // 请求没有回答钩子可跑
            if self.shape == Shape::Generate
                && !self
                    .set
                    .for_reply(self.client, to.model, to.upstream)
                    .is_empty()
            {
                out.bridge = Some(self.base());
            }
            return Ok(out);
        }
        let mut bridge = self.base();
        let (body, dialect, client, shape) = (self.body, self.dialect, self.client, self.shape);
        let original = self
            .parsed
            .get_or_insert_with(|| serde_json::from_slice::<Value>(body).ok())
            .as_ref();
        // 这种请求插件读得懂，这一个却读不成视图：管这一次的插件一个都跑不了，各按各的
        // `on_error`。回答钩子照样要这本账（生成回答的请求，上游也许认得它）
        let path = self.path;
        let readable = self
            .readable
            .get_or_insert_with(|| match original {
                None => Err("the request body is not JSON".into()),
                Some(v) => view::build(form, wrapped_count(dialect, path, v).unwrap_or(v), path)
                    .map(|_| ()),
            })
            .clone();
        if let Err(why) = readable {
            out.runs = unreadable(here, &why, to.attempt)?;
            out.bridge = Some(bridge);
            return Ok(out);
        }
        let mut raw: Option<Cow<'_, Value>> = original.map(Cow::Borrowed);
        let mut path = self.path.to_string();
        // 发给这一家的模型名：前一个插件改了 `params.model`，后面的看到的就是新的
        let mut model = to.model.to_string();
        let mut renamed_by: Option<String> = None;
        let mut changed = false;
        for a in here {
            let host = match &a.state {
                super::set::State::Broken(why) => {
                    let (outcome, refusal) = broken(a.on_error, &a.name, why);
                    let run = not_run(&a, outcome, broken_reason(&a.name, why), to.attempt);
                    out.runs.push((a.clone(), run, Vec::new()));
                    if let Some(why) = refusal {
                        return Err(Box::new(Refused {
                            why,
                            runs: out.runs,
                        }));
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
                detail: Some(json!({ "attempt": to.attempt })),
            };
            let mut logs = Vec::new();
            let result: Result<Option<Rewritten>, Failure> = async {
                let Some(current) = raw.as_deref() else {
                    return Err(Failure::Unreadable("the request body is not JSON".into()));
                };
                let readable = wrapped_count(dialect, &path, current).unwrap_or(current);
                let mut built = view::build(form, readable, &path).map_err(Failure::Unreadable)?;
                sending(&mut built.view, &model);
                let mut input = view::trim(&built.view, &a.permissions);
                bridge.hide_value(&mut input);
                let ctx = ctx(
                    client,
                    &model,
                    to.requested_model,
                    form.name(),
                    to.upstream,
                    &a.settings,
                );
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
                        let mut edits = built
                            .src
                            .check(&input, &returned, &a.permissions)
                            .map_err(Failure::Edit)?;
                        if shape == Shape::Alike {
                            model_only(&mut edits);
                        }
                        if edits.is_empty() {
                            return Ok(None);
                        }
                        let sections = sections(&edits);
                        edits.reveal(&bridge);
                        let new_model = edits.params.as_ref().and_then(|p| p.model.clone());
                        let mut next = current.clone();
                        let new_path =
                            write_back(dialect, &mut next, &built.src, &edits, &path, &model)
                                .map_err(Failure::Edit)?;
                        Ok(Some(Rewritten {
                            value: next,
                            path: new_path,
                            model: new_model,
                            sections,
                        }))
                    }
                }
            }
            .await;
            match result {
                Ok(None) => {}
                Ok(Some(r)) => {
                    raw = Some(Cow::Owned(r.value));
                    if let Some(p) = r.path {
                        path = p;
                    }
                    if let Some(m) = r.model {
                        model = m;
                        renamed_by = Some(a.name.clone());
                    }
                    changed = true;
                    run.outcome = PluginOutcome::Changed;
                    run.detail = Some(json!({ "attempt": to.attempt, "changed": r.sections }));
                }
                Err(Failure::Rejected(reason)) => {
                    run.outcome = PluginOutcome::Rejected;
                    run.error = Some(reason_msg(&reason));
                    out.runs.push((a.clone(), run, logs));
                    return Err(Box::new(Refused {
                        why: rejected(&a.name, reason),
                        runs: out.runs,
                    }));
                }
                Err(f) => {
                    let why = f.msg();
                    run.outcome = PluginOutcome::Error;
                    run.error = Some(why.clone());
                    out.runs.push((a.clone(), run, logs));
                    if a.on_error == OnError::Reject {
                        return Err(Box::new(Refused {
                            why: msg!(
                                "gw.plugin.request_failed",
                                plugin = a.name.clone(), detail = why.text =>
                                "Plugin `{plugin}` failed, so the request was not sent: {detail}"
                            ),
                            runs: out.runs,
                        }));
                    }
                    continue;
                }
            }
            out.runs.push((a.clone(), run, logs));
        }
        if changed && let Some(v) = raw {
            let value = v.into_owned();
            let screen = match (form, original) {
                (view::Form::Conversation(_), _) | (_, None) => None,
                (f, Some(before)) => Some((
                    view::inputs::screenable(f, before, self.path),
                    view::inputs::screenable(f, &value, &path),
                )),
            };
            match serde_json::to_vec(&value) {
                Ok(b) => {
                    out.changed = Some(Changed {
                        body: Bytes::from(b),
                        value,
                        path,
                        renamed: renamed_by.map(|by| Renamed { model, by }),
                        screen,
                    })
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
}

/// 这种请求插件读得懂、这一个的请求体却读不成视图（不是 JSON 之类）：`here` 里管这一次
/// 的插件一个都跑不了。跑不了的插件（文件变了、装不上）照旧，能跑的按它的 `on_error`：
/// 拒绝就拒掉整个请求，跳过就原样发、记一笔跳过。`why` 是读不成的原因
fn unreadable(here: Vec<Arc<Active>>, why: &str, attempt: usize) -> Result<Vec<Ran>, Box<Refused>> {
    let mut runs = Vec::new();
    for a in here {
        let (outcome, error, refusal) = match &a.state {
            super::set::State::Broken(b) => {
                let (outcome, refusal) = broken(a.on_error, &a.name, b);
                (outcome, broken_reason(&a.name, b), refusal)
            }
            super::set::State::Ready(_) => {
                let why = cannot_read(&a.name, why);
                match a.on_error {
                    OnError::Skip => (PluginOutcome::Skipped, why, None),
                    OnError::Reject => (PluginOutcome::Error, why.clone(), Some(why)),
                }
            }
        };
        let run = not_run(&a, outcome, error, attempt);
        runs.push((a, run, Vec::new()));
        if let Some(why) = refusal {
            return Err(Box::new(Refused { why, runs }));
        }
    }
    Ok(runs)
}

/// 没跑的一次：跑不了的插件，读不出来的请求体
fn not_run(a: &Active, outcome: PluginOutcome, error: Msg, attempt: usize) -> PluginRun {
    PluginRun {
        plugin_id: a.id.clone(),
        plugin_name: a.name.clone(),
        hook: PluginHook::Request,
        outcome,
        error: Some(error),
        cpu_us: 0,
        detail: Some(json!({ "attempt": attempt })),
    }
}

/// 插件声明了这种请求，这一个的请求体却读不成它的视图（见 [`unreadable`]）。`detail`
/// 是读不成的原因
pub(super) fn cannot_read(plugin: &str, detail: &str) -> Msg {
    msg!(
        "gw.plugin.cannot_read_body", plugin = plugin, detail = detail =>
        "Plugin `{plugin}` cannot read this request: {detail}"
    )
}

/// Gemini 数 token 的请求
fn gemini_count(dialect: Dialect, path: &str) -> bool {
    dialect == Dialect::Gemini && path.trim_end_matches('/').ends_with(":countTokens")
}

/// Gemini 数 token 的请求体有两种写法：`contents` 直接放在外面，或者整个生成请求包在
/// `generateContentRequest` 里。**插件看的、改的都是那一份生成请求**：包着的是里面那一份
pub(super) fn wrapped_count<'v>(dialect: Dialect, path: &str, raw: &'v Value) -> Option<&'v Value> {
    if !gemini_count(dialect, path) {
        return None;
    }
    raw.get("generateContentRequest").filter(|v| v.is_object())
}

/// 把插件的改动写回原文（见 [`view::apply`]），返回新的路径（Gemini 换了模型时）。
///
/// Gemini 数 token 的请求体照它原来的写法写：包着的写回里面那一份；没包着的，插件加了
/// 系统提示、工具就包起来 —— 外面那一层只收 `contents`，系统提示和工具要放进
/// `generateContentRequest` 才数得进去，放在外面上游回 400。包着的那一份里也写着模型名
/// （`models/…`），插件换了模型名就跟着换，和路径上的对得上。`model` 是发给这一家的模型名
pub(super) fn write_back(
    dialect: Dialect,
    raw: &mut Value,
    src: &view::Src,
    edits: &view::Edits,
    path: &str,
    model: &str,
) -> Result<Option<String>, view::EditError> {
    if !gemini_count(dialect, path) {
        return view::apply(raw, src, edits, path);
    }
    let renamed = edits.params.as_ref().and_then(|p| p.model.as_deref());
    let wrapped = raw
        .get("generateContentRequest")
        .is_some_and(Value::is_object);
    let new_path = match raw.get_mut("generateContentRequest") {
        Some(inner) if wrapped => view::apply(inner, src, edits, path)?,
        _ => view::apply(raw, src, edits, path)?,
    };
    if !wrapped && (edits.system.is_some() || edits.tools.is_some()) {
        *raw = json!({ "generateContentRequest": std::mem::take(raw) });
    }
    if let Some(inner) = raw
        .get_mut("generateContentRequest")
        .and_then(Value::as_object_mut)
        && (renamed.is_some() || !wrapped)
    {
        let model = renamed.unwrap_or(model);
        inner.insert("model".into(), json!(format!("models/{model}")));
    }
    Ok(new_path)
}

/// 不生成回答的接口（[`Shape::Alike`]）：`params` 里只留模型名，别的改动不写回
pub(super) fn model_only(edits: &mut view::Edits) {
    if let Some(p) = edits.params.as_mut() {
        *p = view::ParamsEdit {
            model: p.model.take(),
            ..Default::default()
        };
    }
}

/// 视图里的模型名换成发给这一家的那个（`model` 和 `params.model`）：插件看到的就是要发出去
/// 的，和 `ctx.model` 一致。原文里写的是客户端要的那个，路由规则的改写在格式转换那一步才
/// 落到请求体上
pub(super) fn sending(view: &mut Value, model: &str) {
    let Some(o) = view.as_object_mut() else {
        return;
    };
    o.insert("model".into(), json!(model));
    if let Some(p) = o.get_mut("params").and_then(Value::as_object_mut) {
        p.insert("model".into(), json!(model));
    }
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

/// 插件看到的 `ctx`。请求钩子和回答钩子是同一个样子：`model` 是发给上游的模型名，
/// `requested_model` 是客户端要的，`format` 是请求体的写法（[`view::Form::name`]），
/// `upstream` 是这一次发往的那一家
pub fn ctx(
    client: Option<&str>,
    model: &str,
    requested_model: &str,
    format: &str,
    upstream: &str,
    settings: &serde_json::Map<String, Value>,
) -> Value {
    json!({
        "client": client,
        "model": model,
        "requested_model": requested_model,
        "format": format,
        "upstream": upstream,
        "settings": settings,
    })
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

/// 一个插件改过的请求
struct Rewritten {
    /// 新的原文
    value: Value,
    /// 新的路径（换了的话）
    path: Option<String>,
    /// 新的模型名（改了 `params.model` 的话）
    model: Option<String>,
    /// 改了哪几部分
    sections: Vec<&'static str>,
}

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

/// 把这一次的运行记到请求上（计数、日志、失败的通知，见 [`crate::AppState::plugin_ran`]）
pub fn record(state: &crate::AppState, id: u64, runs: &[Ran]) {
    for (a, run, logs) in runs {
        state.plugin_ran(id, a, run.clone(), logs.clone());
    }
}
