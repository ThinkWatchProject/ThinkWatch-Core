//! 管线第 5 步里每一跳的头一步：插件的请求钩子（见 [`crate::plugin::request`]）。
//!
//! 排在路由之后、这一跳的格式转换之前（契约附录二）。按这一跳的上游和发给它的模型名
//! 挑出管它的插件，**从客户端的原话起改**：上一跳改过什么都不带过来。客户端的原话在第 4
//! 步查过内容过滤，删过的话插件拿到的是删过的那一份。插件改过的请求在这一跳发出去之前
//! 还要过几道：
//!
//! - **内容过滤再查一遍**，只报、只按插件加进来的拒绝（插件拿到的那一份在开头查过、报
//!   过了，见 [`crate::guard::rescreen`]）。拒绝的话拒绝整个请求，不换下一家 —— 和开头
//!   那一遍一样；处置档下插件加进来的字命中了删除规则的，删掉之后再发；
//! - 插件换了发出去的模型名：**密钥的模型范围照样管**。规则改写的模型名要过这一关，插件
//!   改的也要；上游的模型清单不再对（契约附录二）。插件写的是客户端那一侧的名字，**可以是
//!   别名**：和客户端要的一样，按这一家对上它自己的那个名字（[`crate::models::resolve`]）
//!   再发；这一家服务不了这个别名的，这一跳不发、换下一家（[`Stop::Hop`]）；
//! - 出站脱敏接着插件那本账编号：插件写进来的新值拿到新的号，报一条记录；
//! - 重新解码：格式转换用改过的这一份。
//!
//! 插件拒绝了、出错而策略是拒绝、或者上面哪一道没过（别名对不上除外），**整个请求被拒**，
//! 不换下一家：换一家，管它的还是这些插件。
//!
//! **发往上游的每一跳都过这一步**，不只生成回答的：数 token、Responses 的压缩一样过插件
//! （插件删掉的东西不能从这些接口漏出去），嵌入和旧版补全过声明了它们的插件，别的接口
//! 插件不管（见 [`crate::plugin::request::Shape`]）。网关自己估数、不发出去的那一跳到不了
//! 这里。内容过滤再查哪些和开头那一遍一样（数 token 不查），只多了一样：嵌入和旧版补全
//! 开头不查，插件写进去的字照样查（见 [`rescreen`]）。

use bytes::Bytes;
use serde_json::Value;

use super::{Inbound, Started};
use crate::state::{AppState, Runtime};
use tw_types::{Msg, msg};

/// 插件在这一跳上改过的请求，和发出去之前要用的。
pub(super) struct Rewritten {
    /// 客户端那种格式，占位符已经换回原值；内容过滤删过的话是删过的
    pub(super) body: Bytes,
    /// 调的路径（Gemini 换了模型时是新的）
    pub(super) path: String,
    /// 改过的请求解码出来的中间表示：格式转换用它。不生成回答的请求不转换，没有
    pub(super) decoded: Option<Result<tw_dialect::convert::Decoded, tw_dialect::ir::Rejection>>,
    /// 这一跳出站脱敏接着编号的账：拦截档下是插件那本账接着编的（插件写进来的新值有了
    /// 新的号），别的档位是空的
    pub(super) ledger: tw_guard::redact::replace::Ledger,
}

/// 请求钩子在这一跳上的结果。
#[derive(Default)]
pub(super) struct Plugged {
    /// 插件改过的话，改过的请求
    pub(super) rewritten: Option<Rewritten>,
    /// 插件改了 `params.model` 的话，发给这一家的新模型名
    pub(super) model: Option<String>,
    /// 这一跳的密钥映射。回答钩子接着用（见 [`crate::plugin::request::Plugged::bridge`]）
    pub(super) bridge: Option<crate::plugin::bridge::Bridge>,
}

/// 这一跳过不了插件这一步：说给客户端（和尝试链）的那句话，和接下来怎么办。
pub(super) enum Stop {
    /// 拒绝整个请求，不换下一家
    Request(Msg),
    /// 只是这一家不发，换下一家：插件换上的别名这一家服务不了，后面的上游可能可以
    Hop(Msg),
}

impl From<Msg> for Stop {
    fn from(why: Msg) -> Self {
        Stop::Request(why)
    }
}

/// 发往 `provider` 之前跑一遍管这一跳的插件。`model` 是发给它的模型名（路由规则改写
/// 之后的），`attempt` 是这一跳在尝试链上的位置。每一次运行当场记到请求上。
#[allow(clippy::too_many_arguments)]
pub(super) async fn attempt(
    state: &AppState,
    rt: &Runtime,
    req: &Inbound,
    reading: &crate::client_api::Reading,
    started: &Started,
    hook: &mut crate::plugin::request::Hook<'_>,
    provider: &tw_config::Provider,
    model: &str,
    attempt: usize,
) -> Result<Plugged, Stop> {
    let to = crate::plugin::request::Target {
        upstream: &provider.name,
        model,
        requested_model: &reading.facts.model,
        attempt,
    };
    let p = match hook.attempt(&state.plugin_pool, &to).await {
        Ok(p) => p,
        Err(refused) => {
            crate::plugin::request::record(state, started.id, &refused.runs);
            return Err(Stop::Request(refused.why));
        }
    };
    crate::plugin::request::record(state, started.id, &p.runs);
    let mut out = Plugged {
        bridge: p.bridge,
        ..Default::default()
    };
    let Some(c) = p.changed else {
        return Ok(out);
    };
    // 插件换上的名字发给这一家时叫什么：别名对到这一家自己的那个名字
    let renamed = match &c.renamed {
        Some(r) => {
            allowed(rt, req, r)?;
            Some(sent_name(state, rt, provider, r)?)
        }
        None => None,
    };
    // 内容过滤：只报插件加进来的。处置档下删过的话，后面一律用删过的那一份
    let (body, value) = rescreen(state, rt, req, started, &provider.name, &c)?;
    // 生成回答的请求重新解码：格式转换用改过的这一份。数 token、压缩这些不转换（只发给
    // 同格式的上游），不用解
    let decoded = req.api.filter(|_| reading.generates).map(|api| {
        value.and_then(|v| {
            tw_dialect::convert::decode(api.dialect(), &v, &c.path, req.query.as_deref())
        })
    });
    // 出站脱敏：接着插件看到的那本账编号（同一个值还是同一个号），插件写进来的新值报一条
    let mode = rt.config.security.redact.mode;
    let seed = out
        .bridge
        .as_ref()
        .map_or_else(|| started.ledger.clone(), |b| b.ledger().clone());
    let (found, ledger) = crate::guard::look_from(mode, &rt.redact, &body, seed);
    let more = crate::guard::more_found(&started.found, found);
    // 和开头那一条加起来，一个请求报的有上限（见 `crate::guard::REPORTED_MAX`）
    let items = crate::guard::items(&more, started.found.len());
    if !items.is_empty() {
        state.bus.emit(tw_api::Event::SecretsFound {
            id: started.id,
            provider: provider.name.clone(),
            replaced: mode.acts(),
            items,
            at_ms: crate::server::now_ms(),
        });
    }
    out.model = renamed;
    out.rewritten = Some(Rewritten {
        body,
        path: c.path,
        decoded,
        ledger,
    });
    Ok(out)
}

/// 插件改过的请求再查一遍内容过滤：**只报插件加进来的，也只按插件加进来的拒绝**（见
/// [`crate::guard::rescreen`]）。插件拿到的那一份（`req.body`：客户端的原话，处置档下删过的
/// 话是删过的）在第 4 步查过、报过了，这里两份都查，原来就在的减掉。
///
/// 查哪些请求和开头那一遍一样（见 [`crate::client_api::ClientApi::screened`]）：生成回答和
/// 压缩上下文按客户端格式的消息结构查，数 token 不查。**嵌入和旧版补全例外**：开头不查
/// （只有输入，分不出调用方自己打的字和工具抓回来的），插件写进去的字照样查 —— 见
/// [`rescreen_inputs`]。插件的输出不能绕过内容过滤。
///
/// 交回这一跳要发的请求体和它的 JSON：处置档下插件加进来的字命中了删除规则的，是删过的
/// 那一份。`Err` 是拒绝整个请求时告诉客户端的那句话。
fn rescreen(
    state: &AppState,
    rt: &Runtime,
    req: &Inbound,
    started: &Started,
    provider: &str,
    c: &crate::plugin::request::Changed,
) -> Result<(Bytes, Result<Value, tw_dialect::ir::Rejection>), Msg> {
    let screen = crate::guard::Screen::of(rt);
    let unchanged = || Ok((c.body.clone(), Ok(c.value.clone())));
    if !screen.mode.detects() {
        return unchanged();
    }
    if let Some(inputs) = &c.inputs {
        return rescreen_inputs(state, &screen, started, provider, c, inputs);
    }
    let screened = crate::client_api::ClientApi::screened(req.uri.path());
    let Some(api) = req.api.filter(|_| screened) else {
        return unchanged();
    };
    let sc = crate::guard::rescreen(&screen, api.dialect(), &req.body, &c.body);
    if let Some(why) = crate::guard::report(&state.bus, started.id, provider, &sc) {
        return Err(why);
    }
    Ok(match sc.body {
        Some(body) => {
            let value = serde_json::from_slice::<Value>(&body).map_err(|_| {
                tw_dialect::ir::Rejection("The request body is not valid JSON.".into())
            });
            (body, value)
        }
        None => (c.body.clone(), Ok(c.value.clone())),
    })
}

/// [`rescreen`] 的嵌入、旧版补全那一支：**只查插件改过的那几项输入**。没改的那几项和没有
/// 插件时一样，不查、不动。
///
/// 改过的每一项写成一条调用方的消息（一段 Chat 格式的对话），改前、改后各查一遍，只报、
/// 只按插件加进来的拒绝；删过的话，删过的文字写回原文的那几项
fn rescreen_inputs(
    state: &AppState,
    screen: &crate::guard::Screen,
    started: &Started,
    provider: &str,
    c: &crate::plugin::request::Changed,
    inputs: &crate::plugin::request::Inputs,
) -> Result<(Bytes, Result<Value, tw_dialect::ir::Rejection>), Msg> {
    use crate::plugin::view::inputs::{rewrite_texts, texts};
    let as_chat = |texts: &[&String]| {
        let messages: Vec<Value> = texts
            .iter()
            .map(|t| serde_json::json!({ "role": "user", "content": t }))
            .collect();
        serde_json::json!({ "messages": messages }).to_string()
    };
    let after = texts(inputs.form, &c.value, &c.path);
    // 嵌入、补全只改得了文字，不增不减：两边一一对应。对不上（不该发生）就整份都算改过
    let changed: Vec<usize> = if after.len() == inputs.before.len() {
        (0..after.len())
            .filter(|&i| after[i] != inputs.before[i])
            .collect()
    } else {
        (0..after.len()).collect()
    };
    if changed.is_empty() {
        return Ok((c.body.clone(), Ok(c.value.clone())));
    }
    let was: Vec<&String> = changed
        .iter()
        .filter_map(|&i| inputs.before.get(i))
        .collect();
    let now: Vec<&String> = changed.iter().map(|&i| &after[i]).collect();
    let chat = tw_dialect::ir::Dialect::Chat;
    let sc = crate::guard::rescreen(
        screen,
        chat,
        as_chat(&was).as_bytes(),
        as_chat(&now).as_bytes(),
    );
    if let Some(why) = crate::guard::report(&state.bus, started.id, provider, &sc) {
        return Err(why);
    }
    let Some(stripped) = sc.body else {
        return Ok((c.body.clone(), Ok(c.value.clone())));
    };
    // 删过的那一份还是一项一条消息、先后不变：按先后换回改过的那几项
    let cleaned: Vec<String> = serde_json::from_slice::<Value>(&stripped)
        .ok()
        .and_then(|v| {
            v["messages"].as_array().map(|ms| {
                ms.iter()
                    .map(|m| m["content"].as_str().unwrap_or_default().to_string())
                    .collect()
            })
        })
        .unwrap_or_default();
    if cleaned.len() != changed.len() {
        // 删除只改字、不增减消息，对不上只能是出了错：宁可拒绝，也不发没删的那一份
        return Err(request_unscreenable());
    }
    let mut value = c.value.clone();
    let (mut k, mut next) = (0, changed.iter().zip(cleaned).peekable());
    rewrite_texts(inputs.form, &mut value, &c.path, |t| {
        if let Some((_, clean)) = next.next_if(|(i, _)| **i == k) {
            *t = clean;
        }
        k += 1;
    });
    match serde_json::to_vec(&value) {
        Ok(b) => Ok((Bytes::from(b), Ok(value))),
        Err(_) => Err(request_unscreenable()),
    }
}

/// 插件改过的请求删不干净（不该发生：删除只改字）。拒绝，不发没删的那一份
fn request_unscreenable() -> Msg {
    msg!("gw.internal" => "The request was interrupted by an error inside the gateway.")
}

/// 插件换上的模型名发给 `provider` 时叫什么（[`crate::models::resolve`]）。插件写的是客户端
/// 那一侧的名字：是别名的话，发这一家自己的那个，不是别名本身。这一家服务不了这个别名
/// 是 [`Stop::Hop`]：这一跳不发，后面的上游可能可以。
///
/// 不是别名的照旧原样发：插件改的名字不对上游的模型清单（见模块说明）。
fn sent_name(
    state: &AppState,
    rt: &Runtime,
    provider: &tw_config::Provider,
    r: &crate::plugin::request::Renamed,
) -> Result<String, Stop> {
    crate::models::resolve(&rt.config, &state.catalog.load(), provider, &r.model).ok_or_else(|| {
        Stop::Hop(msg!(
            "gw.plugin.alias_unserved",
            plugin = r.by.clone(), model = r.model.clone(), upstream = provider.name.clone() =>
            "Plugin `{plugin}` changed the model to the alias {model}, and upstream `{upstream}` \
             offers none of its models, so the request was not sent there."
        ))
    })
}

/// 插件换上的模型名，这把密钥用不用得了。**和路由规则改写的模型名过同一关**（见
/// `super::admit` 里的说法）：密钥的模型范围管的是发出去的模型，谁改的都一样。
fn allowed(rt: &Runtime, req: &Inbound, r: &crate::plugin::request::Renamed) -> Result<(), Msg> {
    let allow = rt
        .config
        .clients
        .iter()
        .find(|c| c.name == req.client_name)
        .and_then(|c| c.allow.as_deref());
    match allow {
        Some(patterns)
            if !patterns
                .iter()
                .any(|p| tw_engine::rule::glob_match(p, &r.model)) =>
        {
            Err(msg!(
                "gw.plugin.model_not_allowed",
                plugin = r.by.clone(), model = r.model.clone(), key = req.client_name.clone() =>
                "Plugin `{plugin}` changed the model to {model}, which gateway key `{key}` may not \
                 use, so the request was not sent."
            ))
        }
        _ => Ok(()),
    }
}
