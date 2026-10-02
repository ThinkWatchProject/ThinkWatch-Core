//! 试跑：拿一个存下来的请求（和它的回答）让一个插件跑一遍，看它改了什么。
//!
//! **不碰任何上游**：请求钩子对着存下来的请求体跑，回答钩子对着存下来的回答跑 ——
//! 回答先按客户端的格式收成一整份（流也收成整包），所以回答钩子是整块模式的行为
//! （流式模式的插件收到一次全文，再调一次 `onReplyTextEnd`），和非流式的回答一样。
//!
//! 给人看的前后两份都是**换过占位符的**：插件本来就只看得到占位符，界面上显示的也
//! 不该是真值。试跑不进统计、不进日志圈、不留请求记录，日志交给调用方。

use std::sync::Arc;

use serde_json::Value;
use tw_api::{PluginHook as Hook, PluginOutcome as Outcome};
use tw_dialect::ir::Dialect;
use tw_types::{Msg, msg};

use super::bridge::Bridge;
use super::host::{PluginHost, RequestOutcome};
use super::pool::Pool;
use super::request::{rejected, request_unreadable};
use super::set::LogLine;
use super::view;

/// 存下来的请求：客户端调的路径、查询串、请求体，和请求那一行上记的客户端。
pub struct StoredRequest<'a> {
    pub path: &'a str,
    pub query: Option<&'a str>,
    pub body: &'a [u8],
    pub client: Option<&'a str>,
}

/// 存下来的回答：上游的原话（流或者整包），它是什么格式、哪一家回的。
pub struct StoredReply<'a> {
    pub body: &'a [u8],
    pub upstream: Dialect,
    pub provider: &'a str,
}

/// 试跑的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Trial {
    pub request: Option<Side>,
    pub reply: Option<Side>,
    /// 按调用的先后，每条带着是哪个钩子写的
    pub logs: Vec<(Hook, LogLine)>,
    /// 插件拒绝了请求、出了错、或者存下来的东西读不出来
    pub error: Option<Msg>,
}

/// 一边改之前和改之后：缩进排好的 JSON，密钥换成了占位符。
#[derive(Debug, Clone, PartialEq)]
pub struct Side {
    pub before: String,
    pub after: String,
    pub outcome: Outcome,
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

/// 存下来的回答读不出来
fn answer_unreadable(detail: impl Into<String>) -> Msg {
    msg!(
        "gw.plugin.answer_unreadable", detail = detail.into() =>
        "The answer could not be read for the plugin: {detail}"
    )
}

/// 让 `host` 对着存下来的请求和回答各跑一遍。`rules` 是出站脱敏的规则（换占位符用），
/// `settings` 是这个插件的设置。
///
/// 一边都没跑成（插件的钩子和存下来的东西对不上：只有回答钩子而回答没存下来……）
/// 也给一句原因，不交回一个什么都没有的结果
pub async fn run(
    pool: Arc<Pool>,
    host: Arc<dyn PluginHost>,
    settings: &serde_json::Map<String, Value>,
    rules: Arc<tw_guard::redact::rules::RuleSet>,
    request: Option<StoredRequest<'_>>,
    reply: Option<StoredReply<'_>>,
) -> Trial {
    let mut t = tried(pool, host, settings, rules, request, reply).await;
    if t.request.is_none() && t.reply.is_none() && t.error.is_none() {
        t.error = Some(msg!(
            "gw.plugin.nothing_to_try" =>
            "This request has nothing stored that the plugin's hooks run on."
        ));
    }
    t
}

async fn tried(
    pool: Arc<Pool>,
    host: Arc<dyn PluginHost>,
    settings: &serde_json::Map<String, Value>,
    rules: Arc<tw_guard::redact::rules::RuleSet>,
    request: Option<StoredRequest<'_>>,
    reply: Option<StoredReply<'_>>,
) -> Trial {
    let mut t = Trial {
        request: None,
        reply: None,
        logs: Vec::new(),
        error: None,
    };
    let mut bridge = Bridge::new(rules);
    let dialect = request
        .as_ref()
        .and_then(|r| crate::client_api::ClientApi::of_path(r.path))
        .map(|a| a.dialect());
    let parsed = request
        .as_ref()
        .and_then(|r| serde_json::from_slice::<Value>(r.body).ok());
    if let Some(r) = &request {
        bridge.learn(r.body);
    }
    let model = match (&request, dialect) {
        (Some(r), Some(d)) => super::request::asked_model(d, r.path, parsed.as_ref()),
        _ => String::new(),
    };
    let client = request.as_ref().and_then(|r| r.client);
    let name = host.manifest().name.clone();

    // ── 请求钩子
    if let (Some(r), Some(d), true) = (&request, dialect, host.manifest().hooks.request) {
        match parsed.as_ref() {
            None => t.error = Some(request_unreadable("the request body is not JSON")),
            Some(raw) => {
                let mut masked = raw.clone();
                bridge.hide_value(&mut masked);
                match view::build(d, &masked, r.path) {
                    Err(e) => t.error = Some(request_unreadable(e)),
                    Ok(built) => {
                        let m = host.manifest();
                        let input = view::trim(&built.view, &m.permissions);
                        let ctx = super::request::ctx(client, &model, d, None, settings);
                        let (h, given) = (host.clone(), input.clone());
                        let before = pretty(&masked);
                        let ran = pool.run(move || h.on_request(given, ctx)).await;
                        let (outcome, after, error): (Outcome, String, Option<Msg>) = match ran {
                            Err(e) => (
                                Outcome::Error,
                                before.clone(),
                                Some(super::host::RunError::Trap(e.to_string()).msg()),
                            ),
                            Ok(inv) => {
                                t.logs
                                    .extend(inv.logs.into_iter().map(|l| (Hook::Request, l)));
                                match inv.result {
                                    Err(e) => (Outcome::Error, before.clone(), Some(e.msg())),
                                    Ok(RequestOutcome::Unchanged) => {
                                        (Outcome::Unchanged, before.clone(), None)
                                    }
                                    Ok(RequestOutcome::Rejected(reason)) => (
                                        Outcome::Rejected,
                                        before.clone(),
                                        Some(rejected(&name, reason)),
                                    ),
                                    Ok(RequestOutcome::Changed(out)) => {
                                        match view::check(
                                            &input,
                                            &out,
                                            &m.permissions,
                                            built.src.hidden_tools(),
                                        ) {
                                            Err(e) => {
                                                (Outcome::Error, before.clone(), Some(e.msg()))
                                            }
                                            Ok(edits) if edits.is_empty() => {
                                                (Outcome::Unchanged, before.clone(), None)
                                            }
                                            Ok(edits) => {
                                                let mut next = masked.clone();
                                                match view::apply(
                                                    &mut next, &built.src, &edits, r.path,
                                                ) {
                                                    Ok(_) => {
                                                        (Outcome::Changed, pretty(&next), None)
                                                    }
                                                    Err(e) => (
                                                        Outcome::Error,
                                                        before.clone(),
                                                        Some(e.msg()),
                                                    ),
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        };
                        if error.is_some() {
                            t.error = error;
                        }
                        t.request = Some(Side {
                            before,
                            after,
                            outcome,
                        });
                    }
                }
            }
        }
    }

    // ── 回答钩子
    let Some(reply) = reply else {
        return t;
    };
    if !host.manifest().hooks.on_reply() {
        return t;
    }
    // 回答按客户端的格式收成一整份
    let client_dialect = dialect.unwrap_or(reply.upstream);
    let whole = match (&request, parsed.as_ref()) {
        (Some(r), Some(raw)) => collect(client_dialect, raw, r.path, r.query, &reply),
        _ if client_dialect == reply.upstream && !looks_like_sse(reply.body) => {
            Ok(reply.body.to_vec())
        }
        _ => Err(answer_unreadable("the request it answered is missing")),
    };
    let whole = match whole {
        Ok(w) => w,
        Err(e) => {
            t.error.get_or_insert(e);
            return t;
        }
    };
    let Ok(mut masked) = serde_json::from_slice::<Value>(&whole) else {
        t.error
            .get_or_insert(answer_unreadable("the answer is not JSON"));
        return t;
    };
    bridge.hide_value(&mut masked);
    let before = pretty(&masked);
    let ctx = super::reply::ReplyCtx {
        dialect: client_dialect,
        client,
        model: &model,
        upstream: reply.provider,
        request_id: 0,
    };
    let mut chain = match super::reply::Chain::trial(pool, host.clone(), settings, &ctx).await {
        Ok(Some(c)) => c,
        Ok(None) => return t,
        Err(e) => {
            t.error.get_or_insert(e);
            t.reply = Some(Side {
                after: before.clone(),
                before,
                outcome: Outcome::Error,
            });
            return t;
        }
    };
    let ran = super::reply::whole(&mut chain, masked.to_string().as_bytes()).await;
    t.logs.extend(
        chain
            .take_trial_logs()
            .into_iter()
            .map(|l| (Hook::Reply, l)),
    );
    let (outcome, after) = match ran {
        Err(e) => {
            t.error.get_or_insert(e.detail);
            (Outcome::Error, before.clone())
        }
        Ok(b) => match serde_json::from_slice::<Value>(&b) {
            Ok(v) if v != masked => (Outcome::Changed, pretty(&v)),
            _ => (Outcome::Unchanged, before.clone()),
        },
    };
    t.reply = Some(Side {
        before,
        after,
        outcome,
    });
    t
}

fn looks_like_sse(body: &[u8]) -> bool {
    let start = body
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(0);
    !matches!(body.get(start), Some(b'{') | Some(b'['))
}

/// 上游的原话按客户端的格式收成一整份：同一份转换会话（从存下来的请求解出来），流用
/// 收集器收，整包按响应转。Gemini 不带 `alt=sse` 的流是一个 JSON 数组，先拆成帧
fn collect(
    client: Dialect,
    raw: &Value,
    path: &str,
    query: Option<&str>,
    reply: &StoredReply<'_>,
) -> Result<Vec<u8>, Msg> {
    let decoded = tw_dialect::convert::decode(client, raw, path, query)
        .map_err(|e| request_unreadable(e.0))?;
    let mut d = decoded;
    // 收成整包：客户端那一侧按不要流算
    d.request.stream = false;
    let session = d
        .encode(&tw_dialect::ir::Target {
            dialect: reply.upstream,
            official: false,
            default_max_tokens: 0,
        })
        .session;
    let body = reply.body;
    if looks_like_sse(body) {
        let mut c = session.collector();
        c.process(body);
        return c.finish().map_err(answer_unreadable);
    }
    if body.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'[') {
        // JSON 数组的流：每个元素当成一帧
        let elements: Vec<Value> =
            serde_json::from_slice(body).map_err(|e| answer_unreadable(e.to_string()))?;
        let sse: String = elements.iter().map(|e| format!("data: {e}\n\n")).collect();
        let mut c = session.collector();
        c.process(sse.as_bytes());
        return c.finish().map_err(answer_unreadable);
    }
    session
        .response(body)
        .ok_or_else(|| answer_unreadable("the answer is not JSON"))
}

#[cfg(test)]
mod tests;
