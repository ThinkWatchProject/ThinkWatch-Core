//! 管线第 5 步里每一跳的头一步：插件的请求钩子（见 [`crate::plugin::request`]）。
//!
//! 排在路由之后、这一跳的格式转换之前（契约附录二）。按这一跳的上游和发给它的模型名
//! 挑出管它的插件，**从客户端的原话起改**：上一跳改过什么都不带过来。插件改过的请求在
//! 这一跳发出去之前还要过几道：
//!
//! - **请求防护再看一遍**，只看插件加进来的（客户端的原话在开头看过、报过了，见
//!   [`crate::guard::screen_more`]）。拦下就拒绝整个请求，不换下一家 —— 和开头那一遍
//!   一样；
//! - 插件换了发出去的模型名：**密钥的模型范围照样管**。规则改写的模型名要过这一关，插件
//!   改的也要；上游的模型清单不再对（契约附录二）；
//! - 出站脱敏接着插件那本账编号：插件写进来的新值拿到新的号，报一条记录；
//! - 重新解码：格式转换用改过的这一份。
//!
//! 插件拒绝了、出错而策略是拒绝、或者上面哪一道没过，**整个请求被拒**，不换下一家：
//! 换一家，管它的还是这些插件。
//!
//! **发往上游的每一跳都过这一步**，不只生成回答的：数 token、Responses 的压缩一样过插件
//! （插件删掉的东西不能从这些接口漏出去），插件看不懂的接口按插件的 `on_error` 处置（见
//! [`crate::plugin::request::Shape`]）。网关自己估数、不发出去的那一跳到不了这里。

use bytes::Bytes;

use super::{Inbound, Started};
use crate::state::{AppState, Runtime};
use tw_types::{Msg, msg};

/// 插件在这一跳上改过的请求，和发出去之前要用的。
pub(super) struct Rewritten {
    /// 客户端那种格式，占位符已经换回原值
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

/// 发往 `provider` 之前跑一遍管这一跳的插件。`model` 是发给它的模型名（路由规则改写
/// 之后的），`attempt` 是这一跳在尝试链上的位置。每一次运行当场记到请求上。
///
/// `Err` 是拒绝整个请求时告诉客户端的那句话。
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
) -> Result<Plugged, Msg> {
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
            return Err(refused.why);
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
    if let Some(r) = &c.renamed {
        allowed(rt, req, r)?;
    }
    // 生成回答的请求重新解码：格式转换和请求防护用改过的这一份。数 token、压缩这些不转换
    // （只发给同格式的上游），开头也没过请求防护，不用解
    let decoded = req.api.filter(|_| reading.generates).map(|api| {
        tw_dialect::convert::decode(api.dialect(), &c.value, &c.path, req.query.as_deref())
    });
    // 请求防护：只看插件加进来的。解不开的不看 —— 和开头那一遍一样，同格式直通照样发
    if let (Some(Ok(before)), Some(Ok(after))) = (&reading.decoded, &decoded)
        && let Some(why) = crate::guard::screen_more(
            &state.bus,
            started.id,
            &provider.name,
            &crate::guard::Screen::of(rt),
            &before.request,
            &after.request,
        )
    {
        return Err(why);
    }
    // 出站脱敏：接着插件看到的那本账编号（同一个值还是同一个号），插件写进来的新值报一条
    let mode = rt.config.security.redact.mode;
    let seed = out
        .bridge
        .as_ref()
        .map_or_else(|| started.ledger.clone(), |b| b.ledger().clone());
    let (found, ledger) = crate::guard::look_from(mode, &rt.redact, &c.body, seed);
    let more = crate::guard::more_found(&started.found, found);
    if !more.is_empty() {
        state.bus.emit(tw_api::Event::SecretsFound {
            id: started.id,
            provider: provider.name.clone(),
            replaced: mode.acts(),
            items: crate::guard::items(&more),
            at_ms: crate::server::now_ms(),
        });
    }
    out.model = c.renamed.map(|r| r.model);
    out.rewritten = Some(Rewritten {
        body: c.body,
        path: c.path,
        decoded,
        ledger,
    });
    Ok(out)
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
