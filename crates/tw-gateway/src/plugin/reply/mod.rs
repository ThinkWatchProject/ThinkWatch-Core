//! 回答钩子：上游的回答交给客户端之前，按顺序交给范围内的插件改。
//!
//! # 位置
//!
//! 在中继里排在格式转换之后（插件看到的是**客户端那种格式**的回答），在工具调用审查
//! 之前（约定 I7）：审查看的是插件改过的那一版 —— 插件塞进来一个危险的工具调用，照样
//! 被切断。占位符的还原排在转换之前（按上游的原话还原），所以
//! 插件这一步收到的是真值：进插件之前按这个请求的映射换回占位符，出来再换回去
//! （[`super::bridge`]，约定 I5）。
//!
//! # 一次回答一个实例
//!
//! 回答开始时给每个范围内的插件起一个实例（[`Chain::start`]），这次回答的文字和工具
//! 调用都交给它，回答结束就扔掉（约定 I3）。
//!
//! 每个实例占一个名额，整个进程同时活着的回答实例有上限（见 [`super::pool`]）。名额满了，
//! 这个插件这次回答不起实例，按它的 `on_error`：拒绝就是这个请求失败，跳过就是这次回答
//! 绕过它。名额和实例放在一起，回答收尾（[`Chain::finish`]）、插件出错被拿掉、客户端走了
//! （整条链被扔掉）时跟着实例一起还回去。
//!
//! - **文字**按块交：整块模式攒齐一块再交一次，交回来的才发给客户端；流式模式每段
//!   增量交一次，交回什么现在就发什么（空串是先扣着），块结束时调 `onReplyTextEnd`
//!   把扣着的补上。几个插件串起来，前一个交出的是后一个收到的。
//! - **工具调用**攒到完整再交：不变、换成别的（一个或几个）、去掉。换出来的按客户端
//!   的格式写成完整的调用，后面块的序号跟着挪。
//! - 推理内容不交给插件，原样发。
//!
//! 插件出错了按它的 `on_error`：拒绝就切断这次回答（流式从那一帧起不再发，整包整个
//! 换成错误），跳过就让这次回答剩下的部分绕过它。
//!
//! 流和整包是两条路：流按格式拆帧、改帧（[`Stream`]），整包在收齐之后按格式改那一份
//! JSON（[`whole`]）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tw_api::{OnError, PluginHook, PluginOutcome, ReplyMode};
use tw_dialect::ir::Dialect;
use tw_types::{Msg, msg};

use super::bridge::Bridge;
use super::host::{Invocation, PluginHost, ReplyHost, RunError, ToolCallOutcome};
use super::pool::{Pool, Slot};
use super::set::{Active, LogLine, PluginRun, PluginSet};
use crate::error::GatewayError;

mod anthropic;
mod chat;
mod gemini;
mod responses;

/// 一块文字在这次回答里的编号。由各格式自己发
pub type Lane = u64;

/// 一个工具调用：id（插件换出来的可能没有）、名字、参数。
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub id: Option<String>,
    pub name: String,
    pub input: Value,
}

/// 这次回答在谁那儿、要什么：插件的 `ctx` 和范围都看它。
pub struct ReplyCtx<'a> {
    pub dialect: Dialect,
    pub client: Option<&'a str>,
    /// 发给回答它的那一家的模型名：路由规则、请求钩子改过的是改过之后的
    pub model: &'a str,
    /// 客户端要的模型
    pub requested_model: &'a str,
    /// 回答它的那一家
    pub upstream: &'a str,
    pub request_id: u64,
    /// 回答它的那一跳是尝试链上的第几跳。记在每一次运行的 `detail` 里
    pub attempt: usize,
}

/// 一个插件在这次回答里的状态。
struct Stage {
    /// 表里的那一项（计数、日志记到它身上）。试跑没有
    active: Option<Arc<Active>>,
    name: String,
    on_error: OnError,
    mode: ReplyMode,
    text: bool,
    text_end: bool,
    tools: bool,
    /// 出错之后被拿掉了（跳过）的、回答收尾了的是 None
    instance: Option<Instance>,
    lanes: HashMap<Lane, StageLane>,
    cpu: Duration,
    counts: Counts,
    error: Option<Msg>,
    /// 这次回答里它写的日志，回答结束时一起交出去
    logs: Vec<LogLine>,
}

/// 一个插件在这次回答里的实例，连同它占着的名额：**一起扔掉，一起还回去**。调用时整个
/// 交给插件线程、跑完再交回来，所以在插件线程上没了（panic、调用方已经走了）的实例，
/// 名额也跟着还
struct Instance {
    host: Box<dyn ReplyHost>,
    _slot: Slot,
}

#[derive(Default)]
struct StageLane {
    /// 整块模式：攒着的这一块。流式模式：为了不把半截密钥交给插件先扣着的尾巴
    buf: String,
    /// 流式模式：上一次交出非空的东西之后收到的原文。插件半路出错又是跳过时，它
    /// 扣着的那些字从这里补回去，模型说过的话不丢
    pending: String,
}

#[derive(Default, Clone, Copy)]
struct Counts {
    text_calls: u64,
    text_changed: u64,
    tool_calls: u64,
    replaced: u64,
    dropped: u64,
    added: u64,
}

/// 一次回答上的插件链。
pub struct Chain {
    pool: Arc<Pool>,
    /// 记录交给它（计数、日志、失败的通知）。试跑没有
    state: Option<crate::AppState>,
    stages: Vec<Stage>,
    bridge: Bridge,
    dialect: Dialect,
    request_id: u64,
    attempt: usize,
    recorded: bool,
}

/// 插件出错时报给客户端的那一句
fn failed(name: &str, detail: &Msg) -> GatewayError {
    GatewayError::denied(msg!(
        "gw.plugin.reply_failed", plugin = name, detail = detail.text.clone() =>
        "Plugin `{plugin}` failed while handling the answer: {detail}"
    ))
}

/// 名额满了：这个插件这次回答没起实例（见 [`super::pool`]）。记在这次运行上，拒绝时
/// 也是报给客户端的那一句
fn busy(plugin: &str, max: usize) -> Msg {
    msg!(
        "gw.plugin.reply_busy", plugin = plugin, max = max =>
        "Plugin `{plugin}` was not started for this answer: the limit of {max} plugins running \
         on answers at the same time was reached."
    )
}

/// 一个插件这次回答没起来。
enum NotStarted {
    /// 名额满了。记的、拒绝时报给客户端的都是 [`busy`] 那一句
    Busy(Msg),
    /// 起实例出错了。记的是这个错误，拒绝时报给客户端的是 [`failed`] 那一句
    Failed(Msg),
}

impl NotStarted {
    /// 记在这次运行上的那一句
    fn why(&self) -> &Msg {
        match self {
            NotStarted::Busy(m) | NotStarted::Failed(m) => m,
        }
    }
}

/// 拿一个名额、在插件线程上起这个插件的实例。**名额交给插件线程上的那一步**：调用方
/// 半路走了，实例照样起完，和名额一起扔掉
async fn instantiate(
    pool: &Pool,
    host: Arc<dyn PluginHost>,
    name: &str,
    ctx: Value,
) -> Result<Instance, NotStarted> {
    let Some(slot) = pool.reply_slot() else {
        return Err(NotStarted::Busy(busy(name, pool.reply_cap())));
    };
    pool.run(move || host.reply(ctx).map(|host| Instance { host, _slot: slot }))
        .await
        .map_err(|e| RunError::Trap(e.to_string()))
        .and_then(|r| r)
        .map_err(|e| NotStarted::Failed(e.msg()))
}

fn stage(
    active: Option<Arc<Active>>,
    m: &super::engine::Manifest,
    on_error: OnError,
    instance: Instance,
) -> Stage {
    Stage {
        name: active
            .as_ref()
            .map_or_else(|| m.name.clone(), |a| a.name.clone()),
        active,
        on_error,
        mode: m.reply_mode,
        text: m.hooks.reply_text,
        text_end: m.hooks.reply_text_end,
        tools: m.hooks.tool_call,
        instance: Some(instance),
        lanes: HashMap::new(),
        cpu: Duration::ZERO,
        counts: Counts::default(),
        error: None,
        logs: Vec::new(),
    }
}

impl Chain {
    /// 给这次回答起插件实例。范围内一个回答钩子都没有时是 `None` —— 这次回答原样走，
    /// 不付任何代价。
    ///
    /// 起实例失败、名额满了（见 [`super::pool`]）按 `on_error`：拒绝就是这个错误（这时
    /// 一个字节都还没发给客户端），跳过就不要它。两样都和别的插件错误一样记一笔。
    pub async fn start(
        state: &crate::AppState,
        set: &PluginSet,
        bridge: Bridge,
        ctx: &ReplyCtx<'_>,
    ) -> Result<Option<Chain>, GatewayError> {
        let mut stages = Vec::new();
        // 跑不了的插件在回答它的那一次发出去之前已经按 `on_error` 处理过了：这里只有能跑的
        for a in set.for_reply(ctx.client, ctx.model, ctx.upstream) {
            let Some(host) = a.ready().cloned() else {
                continue;
            };
            let m = host.manifest().clone();
            let c = super::request::ctx(
                ctx.client,
                ctx.model,
                ctx.requested_model,
                super::format_name(ctx.dialect),
                ctx.upstream,
                &a.settings,
            );
            match instantiate(&state.plugin_pool, host, &a.name, c).await {
                Ok(instance) => stages.push(stage(Some(a.clone()), &m, a.on_error, instance)),
                Err(not) => {
                    let run = PluginRun {
                        plugin_id: a.id.clone(),
                        plugin_name: a.name.clone(),
                        hook: PluginHook::Reply,
                        outcome: PluginOutcome::Error,
                        error: Some(not.why().clone()),
                        cpu_us: 0,
                        detail: Some(json!({ "attempt": ctx.attempt })),
                    };
                    state.plugin_ran(ctx.request_id, &a, run, Vec::new());
                    if a.on_error == OnError::Reject {
                        // 已经起好的那几个也记一笔（一次都没调用过），名额还回去
                        let mut started = Chain {
                            pool: state.plugin_pool.clone(),
                            state: Some(state.clone()),
                            stages,
                            bridge,
                            dialect: ctx.dialect,
                            request_id: ctx.request_id,
                            attempt: ctx.attempt,
                            recorded: false,
                        };
                        started.finish();
                        return Err(match not {
                            NotStarted::Busy(why) => GatewayError::denied(why),
                            NotStarted::Failed(why) => failed(&a.name, &why),
                        });
                    }
                }
            }
        }
        if stages.is_empty() {
            return Ok(None);
        }
        Ok(Some(Chain {
            pool: state.plugin_pool.clone(),
            state: Some(state.clone()),
            stages,
            bridge,
            dialect: ctx.dialect,
            request_id: ctx.request_id,
            attempt: ctx.attempt,
            recorded: false,
        }))
    }

    /// 一条试跑用的链：只有这一个插件，日志收下来交给调用方，不进统计、日志圈和记录。
    /// 起不来（名额满了也算）是那个错误
    pub(crate) async fn trial(
        pool: Arc<Pool>,
        host: Arc<dyn PluginHost>,
        settings: &serde_json::Map<String, Value>,
        ctx: &ReplyCtx<'_>,
    ) -> Result<Option<Chain>, Msg> {
        let m = host.manifest().clone();
        if !m.hooks.on_reply() {
            return Ok(None);
        }
        let c = super::request::ctx(
            ctx.client,
            ctx.model,
            ctx.requested_model,
            super::format_name(ctx.dialect),
            ctx.upstream,
            settings,
        );
        let instance = instantiate(&pool, host, &m.name, c)
            .await
            .map_err(|not| not.why().clone())?;
        Ok(Some(Chain {
            pool,
            state: None,
            stages: vec![stage(None, &m, OnError::Reject, instance)],
            // 试跑给插件的已经是换过占位符的那一份：这里不再换，也不换回去
            bridge: Bridge::new(Arc::new(tw_guard::redact::rules::RuleSet::none())),
            dialect: ctx.dialect,
            request_id: ctx.request_id,
            attempt: ctx.attempt,
            recorded: false,
        }))
    }

    /// 试跑收下来的日志
    pub(crate) fn take_trial_logs(&mut self) -> Vec<LogLine> {
        self.stages
            .iter_mut()
            .flat_map(|s| std::mem::take(&mut s.logs))
            .collect()
    }

    /// 有插件要文字
    pub fn wants_text(&self) -> bool {
        self.stages.iter().any(|s| s.text)
    }

    /// 有插件要工具调用
    pub fn wants_tools(&self) -> bool {
        self.stages.iter().any(|s| s.tools)
    }

    /// 在插件线程上调这个插件的实例。实例拿出去、跑完再放回来
    async fn invoke<T: Send + 'static>(
        &mut self,
        i: usize,
        f: impl FnOnce(&mut dyn ReplyHost) -> Invocation<T> + Send + 'static,
    ) -> Result<T, Msg> {
        let Some(mut inst) = self.stages[i].instance.take() else {
            return Err(RunError::Trap("the plugin instance is gone".into()).msg());
        };
        let ran = self
            .pool
            .run(move || {
                let inv = f(inst.host.as_mut());
                (inst, inv)
            })
            .await;
        match ran {
            Ok((inst, inv)) => {
                let st = &mut self.stages[i];
                st.instance = Some(inst);
                st.cpu += inv.cpu;
                st.logs.extend(inv.logs);
                inv.result.map_err(|e| e.msg())
            }
            // 实例跟着 panic 一起没了：这个插件这次回答不能再用
            Err(e) => Err(RunError::Trap(e.to_string()).msg()),
        }
    }

    /// 第 `i` 个插件出错了：拒绝就是这个错误，跳过就把它从这次回答里拿掉
    fn fail(&mut self, i: usize, detail: Msg) -> Result<(), GatewayError> {
        let st = &mut self.stages[i];
        if st.error.is_none() {
            st.error = Some(detail.clone());
        }
        st.instance = None;
        if st.on_error == OnError::Reject {
            return Err(failed(&st.name, &detail));
        }
        Ok(())
    }

    fn live(&self, i: usize) -> bool {
        self.stages[i].instance.is_some()
    }

    /// 一段文字流过第 `i` 个插件，返回它此刻交出的（可能没有）
    async fn piece(
        &mut self,
        i: usize,
        lane: Lane,
        p: String,
    ) -> Result<Option<String>, GatewayError> {
        if !self.live(i) {
            return Ok(Some(p));
        }
        if self.stages[i].mode == ReplyMode::Block {
            self.stages[i]
                .lanes
                .entry(lane)
                .or_default()
                .buf
                .push_str(&p);
            return Ok(None);
        }
        // 流式：账里某个值的开头先扣着，等它要么补全、要么证明不是
        let l = self.stages[i].lanes.entry(lane).or_default();
        let mut buf = std::mem::take(&mut l.buf);
        buf.push_str(&p);
        let cut = self.bridge.hold_from(&buf);
        let send = buf[..cut].to_string();
        self.stages[i].lanes.entry(lane).or_default().buf = buf[cut..].to_string();
        if send.is_empty() {
            return Ok(None);
        }
        self.stream_call(i, lane, send).await
    }

    /// 流式插件的一次 `onReplyText`
    async fn stream_call(
        &mut self,
        i: usize,
        lane: Lane,
        send: String,
    ) -> Result<Option<String>, GatewayError> {
        let hidden = self.bridge.hide(&send);
        let given = hidden.clone();
        self.stages[i].counts.text_calls += 1;
        match self.invoke(i, move |r| r.on_text(&given)).await {
            Ok(None) => {
                self.stages[i]
                    .lanes
                    .entry(lane)
                    .or_default()
                    .pending
                    .clear();
                Ok(Some(send))
            }
            Ok(Some(s)) => {
                if s != hidden {
                    self.stages[i].counts.text_changed += 1;
                }
                let out = self.bridge.reveal(&s);
                let l = self.stages[i].lanes.entry(lane).or_default();
                if out.is_empty() {
                    l.pending.push_str(&send);
                    Ok(None)
                } else {
                    l.pending.clear();
                    Ok(Some(out))
                }
            }
            Err(e) => {
                self.fail(i, e)?;
                // 跳过：它扣着的、这一段、还没交给它的尾巴，原样往下走
                let l = self.stages[i].lanes.remove(&lane).unwrap_or_default();
                Ok(Some(format!("{}{send}{}", l.pending, l.buf)))
            }
        }
    }

    /// 一块文字的一段增量。返回此刻该发给客户端的（可能是空的：插件扣着）
    pub async fn text(&mut self, lane: Lane, piece: &str) -> Result<String, GatewayError> {
        let mut pieces = vec![piece.to_string()];
        for i in 0..self.stages.len() {
            if !self.stages[i].text {
                continue;
            }
            let mut next = Vec::with_capacity(pieces.len());
            for p in pieces {
                if p.is_empty() {
                    continue;
                }
                if let Some(o) = self.piece(i, lane, p).await? {
                    next.push(o);
                }
            }
            pieces = next;
        }
        Ok(pieces.concat())
    }

    /// 一块文字结束了：整块模式这时才交给插件，流式模式补上扣着的。返回要补发的
    pub async fn text_end(&mut self, lane: Lane) -> Result<String, GatewayError> {
        let mut carry: Vec<String> = Vec::new();
        for i in 0..self.stages.len() {
            if !self.stages[i].text {
                continue;
            }
            let mut out = Vec::new();
            for p in std::mem::take(&mut carry) {
                if p.is_empty() {
                    continue;
                }
                if let Some(o) = self.piece(i, lane, p).await? {
                    out.push(o);
                }
            }
            if !self.live(i) {
                // 半路被拿掉的：它攒着的原样放出来
                if let Some(l) = self.stages[i].lanes.remove(&lane) {
                    out.push(format!("{}{}", l.pending, l.buf));
                }
                carry = out;
                continue;
            }
            let l = self.stages[i].lanes.remove(&lane).unwrap_or_default();
            match self.stages[i].mode {
                ReplyMode::Block => {
                    if !l.buf.is_empty() {
                        let whole = l.buf;
                        let hidden = self.bridge.hide(&whole);
                        let given = hidden.clone();
                        self.stages[i].counts.text_calls += 1;
                        match self.invoke(i, move |r| r.on_text(&given)).await {
                            Ok(None) => out.push(whole),
                            Ok(Some(s)) => {
                                if s != hidden {
                                    self.stages[i].counts.text_changed += 1;
                                }
                                out.push(self.bridge.reveal(&s));
                            }
                            Err(e) => {
                                self.fail(i, e)?;
                                out.push(whole);
                            }
                        }
                    }
                }
                ReplyMode::Stream => {
                    if !l.buf.is_empty() {
                        // 扣着的尾巴到头了：不会再长成别的，交出去
                        if let Some(o) = self.stream_call(i, lane, l.buf).await? {
                            out.push(o);
                        }
                    }
                    if self.live(i) && self.stages[i].text_end {
                        match self.invoke(i, |r| r.on_text_end()).await {
                            Ok(None) => {}
                            Ok(Some(s)) => {
                                if !s.is_empty() {
                                    self.stages[i].counts.text_changed += 1;
                                }
                                out.push(self.bridge.reveal(&s));
                            }
                            Err(e) => {
                                let pending = self.stages[i]
                                    .lanes
                                    .remove(&lane)
                                    .map(|l| l.pending)
                                    .unwrap_or_default();
                                self.fail(i, e)?;
                                out.push(pending);
                            }
                        }
                    }
                    self.stages[i].lanes.remove(&lane);
                }
            }
            carry = out;
        }
        Ok(carry.concat())
    }

    /// 一个完整的工具调用。`None` 是谁都没改；`Some` 是改过之后的样子（空的就是去掉了）
    pub async fn tool_call(&mut self, call: Call) -> Result<Option<Vec<Call>>, GatewayError> {
        let mut calls = vec![call];
        let mut changed = false;
        for i in 0..self.stages.len() {
            if !self.stages[i].tools || !self.live(i) {
                continue;
            }
            let mut next = Vec::with_capacity(calls.len());
            for c in calls {
                if !self.live(i) {
                    next.push(c);
                    continue;
                }
                let mut given = json!({ "id": c.id, "name": c.name, "input": c.input });
                self.bridge.hide_value(&mut given);
                let shown = given.clone();
                self.stages[i].counts.tool_calls += 1;
                match self.invoke(i, move |r| r.on_tool_call(given)).await {
                    Ok(ToolCallOutcome::Unchanged) => next.push(c),
                    Ok(ToolCallOutcome::Drop) => {
                        changed = true;
                        self.stages[i].counts.dropped += 1;
                    }
                    // 交回一个空数组也是去掉
                    Ok(ToolCallOutcome::Replace(vals)) if vals.is_empty() => {
                        changed = true;
                        self.stages[i].counts.dropped += 1;
                    }
                    Ok(ToolCallOutcome::Replace(vals)) => {
                        // 原样交回来的一个调用就是没改
                        if let [one] = vals.as_slice()
                            && same_call(one, &shown)
                        {
                            next.push(c);
                            continue;
                        }
                        match self.calls_from(vals) {
                            Ok(new) => {
                                changed = true;
                                let st = &mut self.stages[i];
                                st.counts.replaced += 1;
                                st.counts.added += (new.len() as u64).saturating_sub(1);
                                next.extend(new);
                            }
                            Err(why) => {
                                self.fail(i, RunError::BadOutput(why).msg())?;
                                next.push(c);
                            }
                        }
                    }
                    Err(e) => {
                        self.fail(i, e)?;
                        next.push(c);
                    }
                }
            }
            calls = next;
        }
        Ok(changed.then_some(calls))
    }

    /// 插件换出来的调用：核对形状，占位符换回去
    fn calls_from(&self, vals: Vec<Value>) -> Result<Vec<Call>, String> {
        let mut out = Vec::with_capacity(vals.len());
        for (n, mut v) in vals.into_iter().enumerate() {
            let Some(o) = v.as_object() else {
                return Err(format!("tool call {n} is not an object"));
            };
            if let Some(k) = o
                .keys()
                .find(|k| !matches!(k.as_str(), "id" | "name" | "input"))
            {
                return Err(format!("tool call {n} has an unknown field `{k}`"));
            }
            let id = match o.get("id") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                Some(_) => return Err(format!("tool call {n}: `id` must be a string")),
            };
            if o.get("name")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return Err(format!("tool call {n} needs a `name`"));
            }
            let Some(input) = o.get("input") else {
                return Err(format!("tool call {n} has no `input`"));
            };
            if matches!(self.dialect, Dialect::Anthropic | Dialect::Gemini) && !input.is_object() {
                return Err(format!(
                    "tool call {n}: this client takes only an object as `input`"
                ));
            }
            self.bridge.reveal_value(&mut v);
            out.push(Call {
                id: id.map(|s| self.bridge.reveal(&s)),
                name: v["name"].as_str().unwrap_or_default().to_string(),
                input: v["input"].clone(),
            });
        }
        Ok(out)
    }

    /// 回答结束了（或者断了）：每个插件一条记录，改了几处写在 `detail` 里。只记一次。
    ///
    /// **实例这时就扔掉**，名额还回去：调用方还攥着这条链（流还要补一段收尾、整包还要过
    /// 一遍审查）的那一会儿，实例已经用不上了
    pub fn finish(&mut self) {
        for s in &mut self.stages {
            s.instance = None;
        }
        if self.recorded {
            return;
        }
        self.recorded = true;
        let Some(state) = self.state.clone() else {
            return;
        };
        for s in &mut self.stages {
            let Some(a) = s.active.clone() else { continue };
            let c = s.counts;
            let changed = c.text_changed + c.replaced + c.dropped > 0;
            let run = PluginRun {
                plugin_id: a.id.clone(),
                plugin_name: a.name.clone(),
                hook: PluginHook::Reply,
                outcome: if s.error.is_some() {
                    PluginOutcome::Error
                } else if changed {
                    PluginOutcome::Changed
                } else {
                    PluginOutcome::Unchanged
                },
                error: s.error.clone(),
                cpu_us: s.cpu.as_micros().min(u64::MAX as u128) as u64,
                detail: Some(json!({
                    "attempt": self.attempt,
                    "text_calls": c.text_calls,
                    "text_changed": c.text_changed,
                    "tool_calls": c.tool_calls,
                    "tool_calls_replaced": c.replaced,
                    "tool_calls_dropped": c.dropped,
                    "tool_calls_added": c.added,
                })),
            };
            state.plugin_ran(self.request_id, &a, run, std::mem::take(&mut s.logs));
        }
    }
}

impl Drop for Chain {
    /// 客户端半路走了，流被丢掉：记录照样交
    fn drop(&mut self) {
        self.finish();
    }
}

/// 交回来的调用和交出去的一样（id、名字、参数都没变）。参数按 JavaScript 的眼光比
/// （[`tw_plugin::js_equal`]）：进出一趟 JS 的 `1.0` 回来是 `1`，那不算改
fn same_call(v: &Value, given: &Value) -> bool {
    let o = match v.as_object() {
        Some(o) => o,
        None => return false,
    };
    o.keys()
        .all(|k| matches!(k.as_str(), "id" | "name" | "input"))
        && o.get("name") == given.get("name")
        && match (o.get("input"), given.get("input")) {
            (Some(a), Some(b)) => tw_plugin::js_equal(a, b),
            (a, b) => a == b,
        }
        && o.get("id").is_none_or(|id| Some(id) == given.get("id"))
}

// ───────────────────────────────────────────────────────── 帧

/// 一帧：SSE 的一帧，或者 JSON 数组流里的一个元素。
pub(crate) struct Frame {
    /// 原来的字节。没改过就原样发
    raw: Vec<u8>,
    event: Option<String>,
    /// 解析出来的 JSON。不是 JSON 的（`[DONE]`）是 None
    data: Option<Value>,
    dirty: bool,
}

impl Frame {
    fn kind(&self) -> &str {
        self.data
            .as_ref()
            .and_then(|d| d.get("type"))
            .and_then(Value::as_str)
            .or(self.event.as_deref())
            .unwrap_or("")
    }

    fn u64(&self, k: &str) -> Option<u64> {
        self.data.as_ref()?.get(k)?.as_u64()
    }
}

/// 要发出去的一帧。
pub(crate) enum Out {
    Keep(Frame),
    New { event: Option<String>, data: Value },
}

impl Out {
    fn new(event: Option<&str>, data: Value) -> Out {
        Out::New {
            event: event.map(str::to_string),
            data,
        }
    }
}

/// 流怎么分帧
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    Sse,
    /// Gemini 客户端不带 `alt=sse` 时：一个逐个元素写出的 JSON 数组
    JsonArray,
}

enum Codec {
    Anthropic(anthropic::Codec),
    Chat(chat::Codec),
    Responses(responses::Codec),
    Gemini(gemini::Codec),
}

impl Codec {
    async fn frame(
        &mut self,
        c: &mut Chain,
        f: Frame,
        out: &mut Vec<Out>,
    ) -> Result<(), GatewayError> {
        match self {
            Codec::Anthropic(x) => x.frame(c, f, out).await,
            Codec::Chat(x) => x.frame(c, f, out).await,
            Codec::Responses(x) => x.frame(c, f, out).await,
            Codec::Gemini(x) => x.frame(c, f, out).await,
        }
    }

    async fn finish(&mut self, c: &mut Chain, out: &mut Vec<Out>) -> Result<(), GatewayError> {
        match self {
            Codec::Anthropic(x) => x.finish(c, out).await,
            Codec::Chat(x) => x.finish(c, out).await,
            Codec::Responses(x) => x.finish(c, out).await,
            Codec::Gemini(x) => x.finish(c, out).await,
        }
    }

    /// 攒着的帧（一个工具调用没收齐时后面来的）要放回去接着处理
    fn requeue(&mut self) -> Vec<Frame> {
        match self {
            Codec::Anthropic(x) => std::mem::take(&mut x.requeue),
            Codec::Responses(x) => std::mem::take(&mut x.requeue),
            Codec::Chat(_) | Codec::Gemini(_) => Vec::new(),
        }
    }

    /// Responses 的每一帧带序号，插件增删了帧就要重新编
    fn renumber(&mut self, out: &mut [Out]) {
        if let Codec::Responses(x) = self {
            x.renumber(out);
        }
    }
}

/// 一条流上的插件这一步：拆帧，交给格式那一层改，再拼回去。
pub struct Stream {
    chain: Chain,
    codec: Codec,
    framing: Framing,
    partial: Vec<u8>,
    /// JSON 数组：开头的 `[` 发了没有、发没发过元素、收尾的 `]` 发了没有
    opened: bool,
    closed: bool,
    /// 出过错（拒绝）之后剩下的一律原样过
    spent: bool,
}

impl Stream {
    pub fn new(chain: Chain, framing: Framing) -> Self {
        let codec = match chain.dialect {
            Dialect::Anthropic => Codec::Anthropic(anthropic::Codec::new(&chain)),
            Dialect::Chat => Codec::Chat(chat::Codec::new(&chain)),
            Dialect::Responses => Codec::Responses(responses::Codec::new(&chain)),
            _ => Codec::Gemini(gemini::Codec::new(&chain)),
        };
        Self {
            chain,
            codec,
            framing,
            partial: Vec::new(),
            opened: false,
            closed: false,
            spent: false,
        }
    }

    /// 喂一块客户端格式的字节，返回现在该发的。插件出错而策略是拒绝时，返回出错之前
    /// 能发的那些和这个错误
    pub async fn feed(&mut self, chunk: &[u8]) -> (Vec<u8>, Option<GatewayError>) {
        if self.spent {
            return (self.pass(chunk), None);
        }
        self.partial.extend_from_slice(chunk);
        let frames = self.split(false);
        self.run(frames, false).await
    }

    /// 流结束了。`broke`：断在半路，不再交给插件，攒着的不发
    pub async fn finish(&mut self, broke: bool) -> (Vec<u8>, Option<GatewayError>) {
        if self.spent || broke {
            self.spent = true;
            let rest = std::mem::take(&mut self.partial);
            let out = self.pass(&rest);
            self.chain.finish();
            return (out, None);
        }
        let frames = self.split(true);
        let r = self.run(frames, true).await;
        self.chain.finish();
        r
    }

    /// 切断之后补的那段收尾（错误帧）：不再交给插件。JSON 数组要按这边发过的
    /// 重新接好，否则拼出来的不是一个数组
    pub fn tail(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.spent = true;
        self.pass(bytes)
    }

    fn pass(&mut self, bytes: &[u8]) -> Vec<u8> {
        match self.framing {
            Framing::Sse => bytes.to_vec(),
            Framing::JsonArray => {
                self.partial.extend_from_slice(bytes);
                let frames = self.split(true);
                let mut out = Vec::new();
                for f in frames {
                    self.write(Out::Keep(f), &mut out);
                }
                out
            }
        }
    }

    async fn run(&mut self, frames: Vec<Frame>, end: bool) -> (Vec<u8>, Option<GatewayError>) {
        let mut out = Vec::new();
        let mut work: VecDeque<Frame> = frames.into();
        let mut pending: Vec<Out> = Vec::new();
        while let Some(f) = work.pop_front() {
            // 不是 JSON 的帧（`[DONE]`、JSON 数组的括号）也交给格式那一层：`[DONE]`
            // 之前要把攒着的补出来
            if let Err(e) = self.codec.frame(&mut self.chain, f, &mut pending).await {
                self.flush(&mut pending, &mut out);
                self.spent = true;
                return (out, Some(e));
            }
            for f in self.codec.requeue().into_iter().rev() {
                work.push_front(f);
            }
            self.flush(&mut pending, &mut out);
        }
        if end {
            if let Err(e) = self.codec.finish(&mut self.chain, &mut pending).await {
                self.flush(&mut pending, &mut out);
                self.spent = true;
                return (out, Some(e));
            }
            for f in self.codec.requeue() {
                pending.push(Out::Keep(f));
            }
            self.flush(&mut pending, &mut out);
        }
        (out, None)
    }

    fn flush(&mut self, pending: &mut Vec<Out>, out: &mut Vec<u8>) {
        self.codec.renumber(pending);
        for o in pending.drain(..) {
            self.write(o, out);
        }
    }

    /// 拆出收齐了的帧。`all`：流结束了，没收齐的最后一截也算一帧
    fn split(&mut self, all: bool) -> Vec<Frame> {
        let mut frames = Vec::new();
        match self.framing {
            Framing::Sse => {
                while let Some((n, sep)) = tw_dialect::frame::frame_end(&self.partial) {
                    let raw: Vec<u8> = self.partial.drain(..n + sep).collect();
                    frames.push(sse_frame(raw, n));
                }
                if all && !self.partial.is_empty() {
                    let raw = std::mem::take(&mut self.partial);
                    let n = raw.len();
                    frames.push(sse_frame(raw, n));
                }
            }
            Framing::JsonArray => {
                while let Some(f) = json_element(&mut self.partial, all) {
                    frames.push(f);
                }
            }
        }
        frames
    }

    fn write(&mut self, o: Out, out: &mut Vec<u8>) {
        match self.framing {
            Framing::Sse => match o {
                Out::Keep(f) if !f.dirty => out.extend_from_slice(&f.raw),
                Out::Keep(f) => out.extend_from_slice(&rewrite_sse(&f)),
                Out::New { event, data } => {
                    let data = data.to_string();
                    match event {
                        Some(e) => out
                            .extend_from_slice(format!("event: {e}\ndata: {data}\n\n").as_bytes()),
                        None => out.extend_from_slice(format!("data: {data}\n\n").as_bytes()),
                    }
                }
            },
            Framing::JsonArray => {
                let (bytes, close) = match o {
                    Out::Keep(f) if f.raw == b"]" => (Vec::new(), true),
                    Out::Keep(f) if f.raw == b"[" => return,
                    Out::Keep(f) if !f.dirty => (f.raw, false),
                    Out::Keep(f) => (
                        f.data.map(|d| d.to_string().into_bytes()).unwrap_or(f.raw),
                        false,
                    ),
                    Out::New { data, .. } => (data.to_string().into_bytes(), false),
                };
                if self.closed {
                    return;
                }
                if close {
                    if !self.opened {
                        out.push(b'[');
                        self.opened = true;
                    }
                    out.push(b']');
                    self.closed = true;
                    return;
                }
                out.extend_from_slice(if self.opened { b",\r\n" } else { b"[" });
                self.opened = true;
                out.extend_from_slice(&bytes);
            }
        }
    }
}

/// 一帧 SSE 的字节（含结尾的空行）读成帧。`n` 是去掉空行之后的长度
fn sse_frame(raw: Vec<u8>, n: usize) -> Frame {
    let parsed = tw_dialect::frame::parse(&raw[..n.min(raw.len())]);
    let (event, data) = match parsed {
        Some(p) => (p.event, serde_json::from_str::<Value>(&p.data).ok()),
        None => (None, None),
    };
    Frame {
        raw,
        event,
        data,
        dirty: false,
    }
}

/// 改过的一帧写回 SSE：只换 `data:` 那一行，别的行（`event:`、`id:`）原样
fn rewrite_sse(f: &Frame) -> Vec<u8> {
    let Some(d) = &f.data else {
        return f.raw.clone();
    };
    let text = String::from_utf8_lossy(&f.raw);
    let json = d.to_string();
    let mut out = Vec::with_capacity(f.raw.len() + 16);
    let mut wrote = false;
    for line in text.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\n', '\r']);
        if tw_dialect::frame::data_of(bare).is_some() {
            if !wrote {
                out.extend_from_slice(b"data: ");
                out.extend_from_slice(json.as_bytes());
                out.extend_from_slice(&line.as_bytes()[bare.len()..]);
                wrote = true;
            }
            continue;
        }
        out.extend_from_slice(line.as_bytes());
    }
    if !wrote {
        out = format!("data: {json}\n\n").into_bytes();
    }
    // 没收尾的最后一帧补上空行
    if !out.ends_with(b"\n\n") && !out.ends_with(b"\r\n\r\n") {
        out.extend_from_slice(b"\n\n");
    }
    out
}

/// 从 JSON 数组流里取下一个元素（或者开头的 `[`、结尾的 `]`）。没收齐返回 None
fn json_element(buf: &mut Vec<u8>, all: bool) -> Option<Frame> {
    let start = buf
        .iter()
        .position(|b| !(b.is_ascii_whitespace() || *b == b','))?;
    let tok = |buf: &mut Vec<u8>, end: usize| -> Vec<u8> {
        let raw: Vec<u8> = buf.drain(..end).collect();
        raw[start..].to_vec()
    };
    match buf[start] {
        b'[' => {
            let raw = tok(buf, start + 1);
            return Some(Frame {
                raw,
                event: None,
                data: None,
                dirty: false,
            });
        }
        b']' => {
            let raw = tok(buf, start + 1);
            return Some(Frame {
                raw,
                event: None,
                data: None,
                dirty: false,
            });
        }
        b'{' => {}
        _ => {
            // 认不出的东西：流结束时原样交出去，否则等更多字节
            if !all {
                return None;
            }
            let raw = tok(buf, buf.len());
            return Some(Frame {
                raw,
                event: None,
                data: None,
                dirty: false,
            });
        }
    }
    let (mut depth, mut in_str, mut esc) = (0usize, false, false);
    for i in start..buf.len() {
        let b = buf[i];
        if in_str {
            match (esc, b) {
                (true, _) => esc = false,
                (false, b'\\') => esc = true,
                (false, b'"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    let raw = tok(buf, i + 1);
                    let data = serde_json::from_slice::<Value>(&raw).ok();
                    return Some(Frame {
                        raw,
                        event: None,
                        data,
                        dirty: false,
                    });
                }
            }
            _ => {}
        }
    }
    if all {
        let raw = tok(buf, buf.len());
        return Some(Frame {
            raw,
            event: None,
            data: None,
            dirty: false,
        });
    }
    None
}

// ───────────────────────────────────────────────────────── 整包

/// 一份整包的回答（客户端的格式）交给插件。没改就是原样的字节
pub async fn whole(chain: &mut Chain, body: &[u8]) -> Result<Vec<u8>, GatewayError> {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        chain.finish();
        return Ok(body.to_vec());
    };
    let changed = match chain.dialect {
        Dialect::Anthropic => anthropic::whole(chain, &mut v).await,
        Dialect::Chat => chat::whole(chain, &mut v).await,
        Dialect::Responses => responses::whole(chain, &mut v).await,
        _ => gemini::whole(chain, &mut v).await,
    };
    chain.finish();
    match changed? {
        true => Ok(v.to_string().into_bytes()),
        false => Ok(body.to_vec()),
    }
}

/// 一块整段的文字交给插件：整块模式交一次，流式模式交一次再调一次 `onReplyTextEnd`
async fn whole_text(chain: &mut Chain, lane: Lane, text: &str) -> Result<String, GatewayError> {
    let mut out = chain.text(lane, text).await?;
    out.push_str(&chain.text_end(lane).await?);
    Ok(out)
}

/// 给新的工具调用发 id：插件给了就用（同一次回答里重复的另发一个），没给就生成
pub(crate) struct Ids {
    seen: HashSet<String>,
}

impl Ids {
    fn new() -> Self {
        Self {
            seen: HashSet::new(),
        }
    }

    fn take(&mut self, wanted: Option<&str>, prefix: &str) -> String {
        if let Some(w) = wanted
            && self.seen.insert(w.to_string())
        {
            return w.to_string();
        }
        loop {
            let id = tw_dialect::ir::new_id(prefix);
            if self.seen.insert(id.clone()) {
                return id;
            }
        }
    }

    fn note(&mut self, id: &str) {
        self.seen.insert(id.to_string());
    }
}

#[cfg(test)]
mod tests;
