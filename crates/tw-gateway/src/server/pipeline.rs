//! 一个请求的管线：身份识别之后，到响应交给客户端为止。
//!
//! 按「管线第 N 步」分成几个函数，[`pipeline`] 只负责按顺序串起来 ——
//! 每一步各自决定「放行、挡下、还是本地就答了」，顺序本身就是设计：
//! 本地应答在准入之前（离线也要能答），准入在路由之前（列表即承诺），
//! 并发闸门在路由之后（被规则挡下的不用先排队）。
//!
//! 发出开始事件之后的两段各自一个子模块：[`hop`] 依次试候选上游（每一跳先过插件的
//! 请求钩子，见 [`plug`]），[`relay`] 把选中那一家的响应交给客户端。

use std::sync::Arc;

use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;

use super::{Choice, Sender, now_ms};
use crate::error::GatewayError;
use crate::state::{AppState, Runtime};
use tw_types::msg;

mod admission;
mod hop;
mod opening;
mod plug;
mod relay;
mod slow;

pub(crate) use hop::stream_fault;

/// 进管线时就定了的东西：身份识别之后，每一步都只读不改。
pub(super) struct Inbound {
    pub(super) uri: axum::http::Uri,
    pub(super) query: Option<String>,
    pub(super) headers: HeaderMap,
    pub(super) body: Bytes,
    pub(super) client_name: String,
    /// 客户端调的是哪种 API（看路径，见 `client_api`）。认不出的路径是 None
    pub(super) api: Option<crate::client_api::ClientApi>,
    /// 出错时用哪种格式回
    pub(super) dialect: tw_dialect::ir::Dialect,
    pub(super) started: std::time::Instant,
    pub(super) from: Sender,
}

/// 发出开始事件之后，后面几步都要用的。
struct Started {
    id: u64,
    /// 开始的时刻：请求那一行的 `at_ms`。之后才交去存的正文（插件改过的请求）挂在它上面
    at_ms: u64,
    /// 熔断过滤之后的候选，按顺序试
    alive: Vec<String>,
    /// 第一阶段的结论。路由事件在它上面补上第二阶段和尝试链
    choice: Choice,
    /// 这是哪段对话（见 [`crate::affinity::identity`]）。认不出来是 None
    conversation: Option<String>,
    /// 出站脱敏的账本：拦截档下按客户端原文编好了号，每一跳接着它换（见
    /// [`crate::guard::look`]）。别的档位是空的
    ledger: tw_guard::redact::replace::Ledger,
    /// 出站脱敏在客户端原文里找到的。插件改过的那一跳只再报插件写进来的（见 [`plug`]）
    found: Vec<tw_guard::redact::rules::Finding>,
    /// 拦截档下，出站脱敏在客户端原文（`req.body`）里找到的命中。一跳发出去的就是原文的
    /// 那些字节时（同格式直通、一个字节都没改），换的时候照它换，不再找一遍（见 [`hop`]）
    hits: Option<std::sync::Arc<[tw_guard::redact::rules::Hit]>>,
    /// 这个请求最多等到什么时候：准入时定下（见 [`admission`]），等密钥的分钟、小时上限和
    /// 等上游的空位共用这一段（`failover.slot_wait_secs`）
    wait_until: tokio::time::Instant,
}

pub(super) async fn pipeline(
    state: AppState,
    rt: Arc<Runtime>,
    mut req: Inbound,
    live: crate::live::Pass,
    ending: &mut Option<crate::ending::Ending>,
) -> Result<Response, GatewayError> {
    // 请求体**只解析一次**：本地应答的判定、路由事实、对话的指纹、内容过滤都用这一份（见
    // `read`）。解不开是 None —— 照样往下走，同格式直通照样发
    let (parsed, probed) = heavy(&req.body, || {
        let parsed = serde_json::from_slice::<serde_json::Value>(&req.body).ok();
        let probed = probe(&state, &rt, &req, parsed.as_ref());
        (parsed, probed)
    });
    let intent = match probed {
        Probe::Answered(resp) => return Ok(resp),
        Probe::Intent(intent) => intent,
    };

    // 首次运行还没配完是正常状态，不是配置错误。这条要在路由之前挡，
    // 因为「一个 provider 都没有」时任何路由结果都是空的，而那条错误
    // 说不清下一步。
    if rt.config.providers.is_empty() {
        return Err(GatewayError::config(msg!(
            "gw.config.no_upstreams" =>
            "No upstream is configured yet. Add one in ThinkWatch Lite, or under `providers` in \
             config.yaml."
        )));
    }

    // 管线第 2 步：读出路由事实，路由。**看的是客户端的原话**：插件的请求钩子排在路由
    // 之后（每发往一个上游跑一次，见 `plug`），左右不了请求去哪一家
    let (mut reading, fp) = heavy(&req.body, || read(&req, intent, parsed.as_ref()));
    let conv = conversation(&rt, &req, &reading, fp.as_deref());
    let (choice, decision) = match route(&state, &rt, &req, &reading, conv.as_ref())? {
        Routed::Go(choice, decision) => (choice, decision),
        // 规则做了决定，请求却一家上游都不会去。**照样开始、照样报路由**，失败由
        // `passthrough` 按这个错误报 —— 不排队：它一个字节都不会发出去
        Routed::Refused(choice, why) => {
            let to = ("", tw_api::Billing::PerToken);
            // 一个字节都没发出去，也没什么可报的；存下来的请求照样按这一档换、打码
            let seen = heavy(&req.body, || look(&rt, &req));
            let redaction = redaction(&rt, seen.ledger);
            let (id, _) = open(
                &state,
                &req,
                &reading,
                &choice,
                to,
                fp.as_deref(),
                ending,
                (redaction, seen.hits.map(Into::into)),
            );
            state.bus.emit(super::routed_nowhere(id, choice));
            return Err(why);
        }
    };

    // 管线第 3 步：这把密钥的用量上限和并发上限（见 `admission`）。放在路由之后：被规则
    // 挡下的请求不用先等一轮。被用量上限拒绝的照样留一行
    //
    // **并发的通行证交给回程，跟着响应体走**（见 `relay`）：放在这里的话它在响应头交出去的
    // 那一刻就还了，一条还在流的回答不再算数，上限管的只是等响应头的那一段
    let admission::Admitted {
        pass,
        hold,
        wait_until,
    } = admission::admit(
        &state,
        &rt,
        &req,
        &reading,
        &choice,
        &decision,
        fp.as_deref(),
        ending,
    )
    .await?;

    // 管线第 4 步：内容过滤先下结论，不发事件。删过的话，后面一律用删过的那一份
    let size = req.body.len();
    let (screening, started) = heavy_for(size, || {
        let screening = screen(&rt, &mut req, &mut reading, parsed.as_ref());
        // 解析出来的那一份到这里就用完了：删过字的话它也不再是请求体的样子
        drop(parsed);
        let started = start(
            &state,
            &rt,
            &req,
            &reading,
            choice,
            wait_until,
            &decision,
            fp.as_deref(),
            ending,
        );
        (screening, started)
    });
    // 用量上限的预留跟着请求号走，等存储层记下这一行时换成实数
    hold.bind(started.id);
    // 结论挂在请求号上报。**拒绝的也在开始之后**：被拒是一次来源为 `denied` 的失败，
    // 流量里照样留一行；一个字节都不发
    let provider = started.alive.first().map(String::as_str).unwrap_or("");
    if let Some(why) = crate::guard::report(&state.bus, started.id, provider, &screening) {
        return Err(GatewayError::denied(why));
    }
    // 插件的请求钩子在每一跳里跑（见 `plug`）：从客户端的原话起改 —— 内容过滤删过的话是
    // 删过的那一份 —— 几跳共用原文的解析和密钥的编号。插件表跟着运行时走：**整个请求是
    // 同一份**，回答钩子用的也是它
    let hint = crate::hint::client_hint(&req.headers);
    let mut hook = crate::plugin::request::Hook::new(
        &rt.plugins,
        rt.redact.clone(),
        req.dialect,
        req.uri.path(),
        hint.as_deref(),
        &req.body,
    );
    let answer =
        hop::try_upstreams(&state, &rt, &req, &reading, &decision, &started, &mut hook).await?;
    // 网关估的数不是哪一家回答的：不记这段对话留在哪一家
    let mut served = match answer {
        hop::Answer::Served(served) => *served,
        hop::Answer::Estimated(body) => {
            let ending = ending
                .take()
                .expect("written when the start event was emitted");
            return Ok(estimated(&state, &req, started.id, body, ending));
        }
    };
    // 这一跳的账比开头那本多了号（插件往请求里写了新的值，拦截档下接着编了号）：回答
    // 落盘时按这一本换，回显的占位符和存下来的请求对得上
    if served.ledger.len() != started.ledger.len()
        && let Some(e) = ending.as_mut()
    {
        e.redact_with(redaction(&rt, served.ledger.clone()));
    }
    // 回答钩子：上游回了成功的回答才有。**在交出结局之前起实例**：起不来而策略是拒绝时，
    // 这个请求按返回的错误收场，客户端还一个字节都没收到
    let reply_plugins = match served.bridge.take() {
        Some(bridge) if reading.generates && served.upstream.status().is_success() => {
            // 拦截档下回答里的占位符是这一跳编的（接着请求的账），换回占位符时用同一本
            let bridge = if served.ledger.is_empty() {
                bridge
            } else {
                bridge.with_ledger(served.ledger.clone())
            };
            let ctx = crate::plugin::reply::ReplyCtx {
                dialect: req.dialect,
                client: hint.as_deref(),
                model: &served.model,
                requested_model: &reading.facts.model,
                upstream: &served.provider.name,
                request_id: started.id,
                attempt: served.attempt,
            };
            crate::plugin::reply::Chain::start(&state, &rt.plugins, bridge, &ctx).await?
        }
        _ => None,
    };
    let mut ending = ending
        .take()
        .expect("written when the start event was emitted");
    // 记下实际回答的那一家：故障转移之后接下的备选，就是这段对话之后留下的那一家
    if let Some(c) = &conv {
        ending.answered_by(state.affinity.ticket(
            c,
            decision.via_group.clone(),
            &served.provider.name,
        ));
    }
    Ok(relay::respond(
        &state,
        &rt,
        &req,
        reading.generates,
        &reading.facts.model,
        served,
        started.id,
        (live, pass),
        ending,
        reply_plugins,
    ))
}

/// 数 token 由网关估了数（见 [`crate::count`]）：把它交给客户端，照常报响应头和结局。
///
/// **不经过 `relay`**：那里按上游的回答记快慢样本、额度、凭据和代理的状态，而这个
/// 回答不是上游给的 —— 记上去的话，一家从没被问过的上游会显示成「刚刚答得飞快」。
/// 响应头上带 `x-thinkwatch-local`，和本地应答的一样。
fn estimated(
    state: &AppState,
    req: &Inbound,
    id: u64,
    body: Bytes,
    mut ending: crate::ending::Ending,
) -> Response {
    state.bus.emit(tw_api::Event::RequestHeaders {
        id,
        status: 200,
        ttfb_ms: req.started.elapsed().as_millis() as u64,
    });
    ending.responded(200);
    // 留档：请求详情里看得到回了什么数
    ending.feed(&body);
    ending.finished(200);
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/json"),
            ),
            (
                axum::http::HeaderName::from_static("x-thinkwatch-local"),
                axum::http::HeaderValue::from_static("1"),
            ),
        ],
        body,
    )
        .into_response()
}

/// 管线第 1.3 步的结果。
enum Probe {
    /// 本地答了，不往下走
    Answered(Response),
    /// 照常往下走。转发的辅助请求带着它的类别，普通请求是空串
    Intent(String),
}

/// 管线第 1.3 步：客户端的自言自语。
///
/// **位置在身份识别之后、模型准入和路由之前。**在准入之前不是偷懒：
/// 一个被本地应答的请求永远不会到达任何上游，而模型准入回答的是
/// 「哪些上游可以为你服务」，对它无从谈起。
///
/// 更要紧的是**离线时也要能应答** —— sub2api 把判定放在选号之后，
/// 于是断网时健康检查照样失败，白白丢掉这个功能最有价值的场景。
/// 同样的理由让它排在「一个 provider 都没有」那一条之前：那一条也是
/// 一种「没有可用上游」，而本地应答本来就不需要上游。
fn probe(
    state: &AppState,
    rt: &Runtime,
    req: &Inbound,
    parsed: Option<&serde_json::Value>,
) -> Probe {
    let Some(kind) = crate::clientprobe::classify(
        &req.body,
        parsed,
        is_claude_code(&req.client_name, &req.headers),
    ) else {
        return Probe::Intent(String::new());
    };
    use tw_config::ProbeAction::*;
    match kind.action(&rt.config.client_probes) {
        Intercept => {
            let id = state.bus.next_id();
            state.bus.emit(tw_api::Event::LocallyAnswered {
                id,
                client: req.client_name.clone(),
                client_hint: crate::hint::client_hint(&req.headers),
                peer: req.from.peer.clone(),
                key_masked: req.from.key.clone(),
                probe: kind.into(),
                at_ms: now_ms(),
            });
            tracing::debug!(client = %req.client_name, kind = kind.slug(), "answered locally");
            Probe::Answered(local_answer(kind, &req.body))
        }
        // 打一个标记让 `when: { intent: ... }` 能匹配到，然后照常往下走。
        // 规则里没写这个条件时，它和普通请求走同一条路
        Forward => Probe::Intent(kind.slug().to_string()),
    }
}

/// 管线第 2 步的前半：读出路由事实，和这段对话的指纹。
///
/// **只解析一次，指纹也只算一次。**`parsed` 是管线开头解出来的那一份（本地应答的判定、
/// 内容过滤用的也是它）。路由要它，认对话（见 `crate::affinity`）要指纹，开始事件归会话
/// 也要指纹，而 body 可能有几 MB —— 解两遍、哈希两遍是白付一份钱。
///
/// 生成回答的请求解码成中间表示，**四种格式的客户端读出同一份路由事实**。
/// body 解不开时用空的性质走兜底规则。**不要因此拒绝请求** —— 我们的解析器
/// 不认识的东西，上游可能完全认识（只有需要转换时才用得上解码结果）
fn read(
    req: &Inbound,
    intent: String,
    parsed: Option<&serde_json::Value>,
) -> (crate::client_api::Reading, Option<String>) {
    let mut reading = crate::client_api::read(req.uri.path(), req.query.as_deref(), parsed);
    reading.facts.client = req.client_name.clone();
    reading.facts.intent = intent;
    reading.harness = tw_dialect::harness::detect(
        req.headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok()),
        parsed,
    );
    // 认出「这几十个请求是同一次任务」。**认不出来就是 None** —— 硬凑一个会把
    // 互不相干的请求并成一个「会话」
    let fp = parsed.and_then(crate::session::fingerprint);
    (reading, fp)
}

/// 这次请求在哪段对话的第几轮（见 [`crate::affinity`]）。
///
/// **数得出轮次的才算**：要解码出整段对话才知道最后一条是不是工具结果。计 token、
/// 嵌入、解不开的请求不记 —— 它们照常按策略排序。
fn conversation(
    rt: &Runtime,
    req: &Inbound,
    reading: &crate::client_api::Reading,
    fp: Option<&str>,
) -> Option<crate::affinity::Conversation> {
    let Some(Ok(d)) = &reading.decoded else {
        return None;
    };
    let id = crate::affinity::identity(&req.headers, fp)?;
    Some(crate::affinity::Conversation::new(
        rt.engine.route_of(&req.client_name),
        &id,
        crate::affinity::turn_of(&d.request),
    ))
}

/// 输入超出了这个决定所选模型的上下文窗口：这一轮沿用的决定要重新求值。
///
/// 按候选里最小的那个窗口算，留 5% 的余量（输入是估的）。**知道窗口的才算**：价目表
/// 里没写、那一家也没手写（`model_specs`）的模型，说不出它装不装得下，照常沿用。窗口按
/// 每一家发出去的名字查：别名在各家是各家的名字（见 [`crate::sent`]）。
fn outgrown(
    state: &AppState,
    rt: &Runtime,
    req: &Inbound,
    facts: &tw_engine::RequestFacts,
    d: &tw_engine::Decision,
) -> bool {
    let book = state.pricing.load();
    let asked = rt
        .engine
        .asked(rt.engine.rules_for_client(&req.client_name), facts, d);
    let allow = crate::models::key_allow(&rt.config, &req.client_name);
    crate::sent::plan(
        &rt.config,
        &state.catalog.load(),
        d,
        &facts.model,
        &asked,
        allow,
    )
    .iter()
    .filter_map(|s| {
        rt.config
            .model_spec(&book, &s.provider, s.model.as_deref().ok()?)
            .context_window()
    })
    .min()
    .is_some_and(|limit| facts.input_tokens.saturating_mul(100) >= limit.saturating_mul(95))
}

/// 管线第 2 步的中段：模型准入。**和 `GET /v1/models` 共用同一个函数**
/// —— 列出来的一定能用。
///
/// **看的是每个候选实际要的模型，不是客户端写的那个**，所以放在规则做完决定之后：
/// 一条把 `claude-*` 改成 `glm-*` 的规则，请求里的名字哪一家的清单里都没有，
/// 改写后的才有。`asked` 里有一个过得去就放行 —— 哪一家服务不了、哪一家要的名字
/// 密钥不让用，由下一步把它跳过（见 [`crate::sent::serving`]）。
///
/// 每个候选要的名字按它的来历看（[`tw_engine::Origin`]）：
///
/// - 客户端那一侧的名称（客户端写的、阶段一改写的，可能是别名）：按目录看 —— 有清单的
///   上游里有谁服务它，`allow` 写它的名称、或者写它列表里任一模型名都算放行（继承只从
///   真名到别名）。
/// - 原样发出的名字（指定模型、阶段二改的）：看这一家自己的清单里有没有这个名字（没有
///   清单的当作有，和挑候选时一样），`allow` 按这个名字本身看。**不经过别名表**：阶段二
///   给中转站写的 `glm-air` 恰好和一个别名同名，发出去的也是 `glm-air` 本身，按别名看
///   就会拿别名的模型列表去问这一家。
///
/// 目录空着时不看清单：那说明探测还没回来或者上游都不给列表，这时候拦等于把整个网关
/// 关掉。**密钥的 `allow` 照样看**：它写在配置里，和清单问没问到无关 —— 不看的话，
/// 下一步按密钥跳过候选时，请求会以「没有上游可用」被拒，说不清是密钥的事。
fn admit(
    state: &AppState,
    rt: &Runtime,
    req: &Inbound,
    reading: &crate::client_api::Reading,
    decision: &tw_engine::Decision,
    asked: &[tw_engine::Asked],
) -> Result<(), GatewayError> {
    let facts = &reading.facts;
    if facts.model.is_empty() {
        return Ok(());
    }
    let catalog = state.catalog.load();
    let listed = !catalog.is_empty();
    let allow = crate::models::key_allow(&rt.config, &req.client_name);
    // 数 token 也不挑格式：别的格式的上游由网关本地估算（见 `crate::count`）
    let any = reading.generates || crate::client_api::ClientApi::counts_tokens(req.uri.path());
    let servable = req
        .api
        .map(|a| crate::client_api::slugs(a.servable_by(any)));
    // 阶段一之后要的模型。规则一家候选都没给时就看它
    let model = decision.set.model.as_deref().unwrap_or(&facts.model);
    // 按名称要的（客户端那一侧，别名还是别名），和原样发出的（指定模型、阶段二改的：
    // 哪一家、哪个名字）分开看
    let mut named: Vec<&str> = Vec::new();
    let mut written: Vec<&tw_engine::Asked> = Vec::new();
    for a in asked {
        if a.origin.as_written() {
            written.push(a);
        } else {
            named.push(&a.model);
        }
    }
    if named.is_empty() && written.is_empty() {
        named.push(model);
    }
    let named_ok = |m: &str| {
        if listed {
            catalog.admits(m, servable.as_deref(), allow)
        } else {
            crate::models::allowed(&catalog, allow, m, false)
        }
    };
    // 这一家有没有这个名字。配置里没有这一家的留给尝试那一步报出来
    let offered_at = |a: &tw_engine::Asked| {
        !listed
            || rt
                .config
                .providers
                .iter()
                .find(|x| x.name == a.provider)
                .is_none_or(|x| crate::models::fit(&catalog, x, &a.model).is_none())
    };
    if named.iter().any(|m| named_ok(m))
        || written
            .iter()
            .any(|a| offered_at(a) && crate::models::allowed(&catalog, allow, &a.model, true))
    {
        return Ok(());
    }
    // 错误信息要说清是哪一种：没有上游提供它，和这个客户端不让用它，
    // 该去改的地方不一样。**改写过的两个名字都要说**：客户端写的是一个，
    // 报错里说的是另一个，不说清楚像是网关认错了模型
    let key = req.client_name.clone();
    // 原样发出的名字里有上游提供、只是密钥不让用的：要改的是密钥
    let barred = written.iter().find(|a| offered_at(a));
    let why = if named.is_empty()
        && written
            .iter()
            .all(|a| a.origin == tw_engine::Origin::Pinned)
    {
        // 去向是指定模型：说出是哪条规则指定的、指定在哪一家
        let rule = decision.matched_rule.clone();
        match barred {
            Some(a) => msg!(
                "gw.model.pinned_not_allowed", rule = rule, model = a.model.clone(),
                upstream = a.provider.clone(), key = key =>
                "Rule `{rule}` pins model {model} on `{upstream}`, which gateway key `{key}` may \
                 not use."
            ),
            None => {
                let detail = written
                    .iter()
                    .map(|a| format!("{} ({})", a.provider, a.model))
                    .collect::<Vec<_>>()
                    .join(", ");
                msg!(
                    "gw.model.pinned_not_offered", rule = rule, detail = detail =>
                    "Rule `{rule}` pins models that their upstreams do not offer: {detail}."
                )
            }
        }
    } else {
        // 按名称要的有上游提供（目录空着时当作有），那就是密钥不让用
        let named_offered = named
            .iter()
            .any(|m| !listed || !catalog.providers_for(m).is_empty());
        // 说哪个名字、有没有上游提供它、它是不是别名。按名称要的说不出什么（谁都不提供）时，
        // 说阶段二给某一家改的那个：先说有上游提供、密钥不让用的
        let (shown, offered, alias) = match (named_offered, barred, named.is_empty()) {
            (true, _, _) => (model, true, catalog.alias(model)),
            (false, Some(a), _) => (a.model.as_str(), true, None),
            (false, None, false) => (model, false, catalog.alias(model)),
            (false, None, true) => (written[0].model.as_str(), false, None),
        };
        let alias = alias.map(|ms| ms.join(", "));
        match (offered, shown != facts.model, alias) {
            (false, false, Some(models)) => msg!(
                "gw.model.alias_unserved", model = facts.model.clone(), models = models =>
                "No upstream serves alias {model}: none offers any of {models}. GET /v1/models \
                 lists the models that are available."
            ),
            (false, true, Some(models)) => msg!(
                "gw.model.alias_unserved_rewritten", from = facts.model.clone(), model = shown,
                models = models =>
                "A routing rule rewrote model {from} to alias {model}, and no upstream offers any \
                 of {models}. GET /v1/models lists the models that are available."
            ),
            (false, false, None) => msg!(
                "gw.model.no_upstream", model = facts.model.clone() =>
                "No upstream serves model {model}. GET /v1/models lists the models that \
                 are available."
            ),
            (true, false, _) => msg!(
                "gw.model.not_allowed", key = key, model = facts.model.clone() =>
                "Gateway key `{key}` may not use model {model}. GET /v1/models lists the \
                 models that are available."
            ),
            (false, true, None) => msg!(
                "gw.model.no_upstream_rewritten", from = facts.model.clone(), model = shown =>
                "A routing rule rewrote model {from} to {model}, and no upstream serves {model}. \
                 GET /v1/models lists the models that are available."
            ),
            (true, true, _) => msg!(
                "gw.model.not_allowed_rewritten", from = facts.model.clone(), model = shown,
                key = key =>
                "A routing rule rewrote model {from} to {model}, which gateway key `{key}` may not \
                 use. GET /v1/models lists the models that are available."
            ),
        }
    };
    Err(GatewayError::new(crate::error::Source::Request, why))
}

/// 管线第 2 步的后半的结果。
enum Routed {
    /// 选出了候选，照常往下走
    Go(Choice, tw_engine::Decision),
    /// 规则做了决定，请求却一家上游都不会去：规则拒绝了它，或者规则选中的上游
    /// 都服务不了它。**这一行要留下**（见 [`super::routed_nowhere`]）
    Refused(Choice, GatewayError),
}

/// 管线第 2 步的后半：路由。**规则引擎在这里** —— M0 那句「取第一个
/// provider」就是留给这一段的接缝。
///
/// 出来的候选已经去掉了服务不了的、按策略组排好了序；熔断过滤在并发
/// 闸门之后（见 [`start`]）。
///
/// 返回的错误是**规则还没做出决定**的那些（没有一条规则命中、规则求不了值）：
/// 那时说不出这个请求算在哪条规则上，不留这一行，和鉴权没过一样。模型准入没过
/// （[`admit`]）也从这里返回，同样不留这一行 —— 和准入放在路由之前时一样。
fn route(
    state: &AppState,
    rt: &Runtime,
    req: &Inbound,
    reading: &crate::client_api::Reading,
    conv: Option<&crate::affinity::Conversation>,
) -> Result<Routed, GatewayError> {
    let facts = &reading.facts;
    let route = rt.engine.route_of(&req.client_name).to_string();
    let now = now_ms();
    // 同一轮里沿用这一轮开头的决定：按输入大小、有没有图片分流的规则，不能让一轮
    // 半路换家（见 `crate::affinity`）。输入超出了所选模型的上下文的，重新求值
    let held = conv
        .and_then(|c| state.affinity.held(c, &rt.engine, now))
        .filter(|d| !outgrown(state, rt, req, facts, d));
    let held_route = held.is_some();
    let mut decision = match held {
        Some(d) => d,
        None => match rt.engine.route(facts).map_err(|e| {
            GatewayError::config(msg!("gw.route.failed", detail = e => "Routing failed: {detail}"))
        })? {
            tw_engine::Outcome::Route(d) => {
                if let Some(c) = conv {
                    state.affinity.decided(c, &rt.engine, &d, now);
                }
                d
            }
            tw_engine::Outcome::Deny { rule, reason } => {
                // **带理由的拒绝。**一个没有理由的拒绝，和一个 bug，在用户
                // 眼里没有区别。
                tracing::info!(%rule, "a rule denied the request");
                let why = GatewayError::denied(msg!(
                    "gw.route.denied", rule = rule.clone(), reason = reason =>
                    "Rule `{rule}` denied this request: {reason}"
                ));
                // 拒绝了就什么都没改：累积的改写跟着这个决定一起作废
                let choice = Choice {
                    route,
                    rule,
                    group: None,
                    rewritten_by: Vec::new(),
                    affinity: None,
                    stayed_on: None,
                };
                return Ok(Routed::Refused(choice, why));
            }
        },
    };
    let mut choice = Choice {
        route,
        rule: decision.matched_rule.clone(),
        group: decision.via_group.clone(),
        rewritten_by: decision.rewritten_by.clone(),
        affinity: held_route.then_some(tw_api::AffinityView {
            held_route,
            stayed: None,
        }),
        stayed_on: None,
    };
    // 每个候选实际要的模型和它的来历：规则改写过的按改写后的算；客户端写的、阶段一改写的
    // 是客户端那一侧的名称（可能是别名），指定的、阶段二改的原样发出。准入看它
    let asked = rt.engine.asked(
        rt.engine.rules_for_client(&req.client_name),
        facts,
        &decision,
    );
    admit(state, rt, req, reading, &decision, &asked)?;
    // 每一家发出去的名字：别名对到各家自己的名称。跳过、比价看它（见 `crate::sent`）
    let catalog = state.catalog.load();
    let allow = crate::models::key_allow(&rt.config, &req.client_name);
    let sent = crate::sent::plan(&rt.config, &catalog, &decision, &facts.model, &asked, allow);
    // 去掉服务不了这个请求的候选：停用的、范围外的、清单里没有这个模型的、别名对不到的，
    // 和发给它的名字密钥不让用的。**在排序之前** —— `cheapest` 和 `url-test` 要在能服务的
    // 上游里挑。
    //
    // 不跳过的话，一家没有这个模型的上游排在前面，它回的 404 不触发故障
    // 转移，请求就在一家能服务它的上游旁边失败了；一家指定了密钥不让用的模型的上游
    // 排在后面，前一家失败时请求就被转到了它那里。
    let serving = crate::sent::serving(&rt.config, &catalog, &decision, &asked, allow);
    let model = decision.set.model.as_deref().unwrap_or(&facts.model);
    if serving.usable.is_empty() {
        return Ok(Routed::Refused(choice, serving.explain(model)));
    }
    if !serving.skipped.is_empty() {
        tracing::debug!(skipped = ?serving.skipped, %model, "skipping the candidates that cannot serve this request");
    }
    decision.candidates = serving.usable;
    let arranged = arrange(
        state,
        rt,
        &mut decision,
        &crate::sent::pairs(&sent),
        conv,
        now,
    );
    if let Some(why) = arranged.stayed {
        choice.affinity = Some(tw_api::AffinityView {
            held_route,
            stayed: Some(why),
        });
        // 留下的那一家在头上。满着时等它，不当场跳过（见 `crate::slots`）
        choice.stayed_on = decision.candidates.first().cloned();
    }
    // `load-balance` 记账：**记粘性之后排头的那一家**，不是按权重轮到的那一家。一段对话
    // 留在了上次回答它的那一家，这一次就算那一家的；之后的新对话把差的补回去
    if let Some(leader) = decision.candidates.first() {
        arranged.charge(&decision.candidates, leader);
    }
    Ok(Routed::Go(choice, decision))
}

/// 排好了的候选（[`arrange`]）：会话粘性留下了哪一家的理由，和 `load-balance` 这一次还没记的账。
pub(super) struct Arranged<'a> {
    /// 排头的是上次回答这段对话的那一家：留下的理由。没留的是 None
    pub(super) stayed: Option<tw_api::Stay>,
    /// `load-balance` 这一轮（拿着它的锁）和排序时的那一份事实。别的组没有
    ledger: Option<(crate::balance::Turn<'a>, tw_engine::Facts)>,
}

impl Arranged<'_> {
    /// `load-balance` 记账：这一次排头的是 `leader`，`members` 是排好的那一份候选（粘性只换了
    /// 次序，没换集合，还是同一轮）。HTTP 的请求记粘性之后排头的那一家，WebSocket 的升级记
    /// 真连上的那一家。不经过 `load-balance` 的什么都不记
    pub(super) fn charge(self, members: &[String], leader: &str) {
        if let Some((turn, f)) = self.ledger {
            turn.charge(members, &f, leader);
        }
    }
}

/// 管线第 2 步的最后：给候选排序、会话粘性。**HTTP 的请求和 WebSocket 的升级共用这一个**（见
/// `super::upgrade`）：同一个组排出同一个顺序，`load-balance` 记同一本账，试算说的就是两条路
/// 下一个新对话会去的那一家。
///
/// `decision.candidates` 进来时是能服务这个请求的那几家，出去时是排好的次序。`sent` 是每一家和
/// 发给它的名字（比价按它算）；`conv` 是这段对话，认不出来的（WebSocket 的升级）没有粘性。
/// 记账交给调用方（[`Arranged::charge`]）：从排序到记账一直拿着 `load-balance` 那一组的锁，
/// 同时进来的几个一个接一个地排，后一个看到的是前一个记过的账
pub(super) fn arrange<'a>(
    state: &'a AppState,
    rt: &'a Runtime,
    decision: &mut tw_engine::Decision,
    sent: &[(String, String)],
    conv: Option<&crate::affinity::Conversation>,
    now: u64,
) -> Arranged<'a> {
    let group = decision
        .via_group
        .as_deref()
        .and_then(|n| rt.engine.groups().iter().find(|g| g.name == n));
    // `load-balance` 这一次轮到谁（见 `crate::balance`）
    let turn = group
        .filter(|g| g.kind == tw_engine::GroupType::LoadBalance)
        .map(|g| state.balance.turn(g));
    // 策略组排序。**引擎给的是集合，顺序在这儿定** ——
    // 因为 `load-balance` / `url-test` / `cheapest` 都要运行时的数字，
    // 而路由决策本身必须是纯的、可试算的。
    //
    // `fallback` 和 `select` 走不到这里面 —— 那是绝大多数人的配置，
    // 它们连一个 HashMap 都不用建。
    let mut facts_rt = None;
    if let Some(g) = group
        && g.kind.needs_runtime()
    {
        let f = state.group_facts(
            &rt.config.providers,
            g,
            &decision.candidates,
            sent,
            turn.as_ref().map(crate::balance::Turn::current),
        );
        decision.candidates = rt.engine.order(Some(&g.name), &decision.candidates, &f);
        facts_rt = Some(f);
    }
    // 留在上次回答这段对话的那一家：同一轮里一律留，跨轮看缓存值不值得留。**排在
    // 策略组排序之后** —— 该留的时候盖过策略，放开的时候策略照常说了算
    let stayed = conv.and_then(|c| {
        state.affinity.stay(
            c,
            decision.via_group.as_deref(),
            &mut decision.candidates,
            |p| state.health.is_available(p),
            now,
        )
    });
    Arranged {
        stayed,
        ledger: turn.zip(facts_rt),
    }
}

/// 发出开始事件：熔断过滤、出站脱敏看一遍、`RequestStarted`、结局、脱敏的记录、请求体留档。
///
/// **发了开始，就欠一个结局。**从这里起，管线返回的错误由调用方报成
/// 失败，这个 future 被丢掉由 Drop 报成取消（见 `passthrough`）。
#[allow(clippy::too_many_arguments)]
fn start(
    state: &AppState,
    rt: &Runtime,
    req: &Inbound,
    reading: &crate::client_api::Reading,
    choice: Choice,
    wait_until: tokio::time::Instant,
    decision: &tw_engine::Decision,
    fp: Option<&str>,
    ending: &mut Option<crate::ending::Ending>,
) -> Started {
    // 熔断过滤。**只有一个候选时完全旁路**，全都熔断时 fail-open ——
    // 两条边界都在 `Health::filter` 里，理由写在那儿。
    let (alive, fail_open) = state.health.filter(&decision.candidates);
    if fail_open {
        tracing::warn!(
            candidates = ?decision.candidates,
            "every candidate upstream is open-circuited; trying them anyway (fail-open)"
        );
    }
    let alive: Vec<String> = alive.into_iter().map(|s| s.to_string()).collect();

    // 头一个候选。故障转移之后实际服务的是谁、按什么记账，由后面的
    // `RequestRouted` 改过来
    let first = alive.first().map(String::as_str).unwrap_or_default();
    let billing = rt
        .config
        .providers
        .iter()
        .find(|p| p.name == first)
        .map(|p| p.billing)
        .unwrap_or_default();

    // 出站脱敏：按全局的规则看一遍客户端发来的原文。**观察档和拦截档报的
    // 是同一条记录**，差别只在换没换 —— 真正的替换在每一跳发出去之前做，
    // 那一跳的请求体可能是转换过格式的。拦截档下账本在这里就编好号：每一跳、
    // 存下来的那份请求都按它换，同一个值处处是同一个占位符
    let seen = look(rt, req);
    let (found, ledger) = (seen.found, seen.ledger);
    let hits: Option<std::sync::Arc<[tw_guard::redact::rules::Hit]>> = seen.hits.map(Into::into);
    let (id, at_ms) = open(
        state,
        req,
        reading,
        &choice,
        (first, billing.into()),
        fp,
        ending,
        (redaction(rt, ledger.clone()), hits.clone()),
    );
    let redact_mode = rt.config.security.redact.mode;
    if !found.is_empty() {
        state.bus.emit(tw_api::Event::SecretsFound {
            id,
            provider: alive.first().cloned().unwrap_or_default(),
            replaced: redact_mode.acts(),
            items: crate::guard::items(&found, 0),
            at_ms: now_ms(),
        });
    }
    Started {
        id,
        at_ms,
        alive,
        choice,
        conversation: crate::affinity::identity(&req.headers, fp),
        ledger,
        found,
        hits: hits.filter(|_| redact_mode.acts()),
        wait_until,
    }
}

/// 出站脱敏看一遍客户端发来的原文（见 [`crate::guard::look`]）。
///
/// 插件的密钥映射按同一份原文、同一个找法编号（见 [`crate::plugin::bridge`]）：插件看到的
/// 占位符和这本账里的是同一个号。插件往某一跳写进新的值，那一跳接着编（见 [`plug`]）。
///
/// 找到的命中跟着请求体交去留档：落盘前打码不再把同一份正文找一遍（见
/// [`crate::bodies::BodyRecord::found`]）。
fn look(rt: &Runtime, req: &Inbound) -> tw_guard::redact::flow::Look {
    crate::guard::look_hits(rt.config.security.redact.mode, &rt.redact, &req.body)
}

/// 这个请求的正文落盘之前怎么换、怎么打码：此刻生效的规则，和这个请求的账本。
fn redaction(rt: &Runtime, ledger: tw_guard::redact::replace::Ledger) -> crate::bodies::Redaction {
    crate::bodies::Redaction {
        rules: rt.redact.clone(),
        ledger,
    }
}

/// 发 `RequestStarted`、把这个请求欠着的结局放进 `ending`、把请求体交去留档，
/// 交回这个请求的号和开始的时刻。`to` 是要发往的那一家和它怎么收钱；一家都不会去的
/// （被规则拒绝了）是空的名字。`redaction` 是请求体、响应体落盘之前怎么换、打码，和出站
/// 脱敏在请求体上已经找到的命中（见 [`look`]）。
///
/// **会话在这里定**（见 [`crate::session::Sessions`]）：开始事件带着它，落库的
/// 那一行记的也是它。
#[allow(clippy::too_many_arguments)]
fn open(
    state: &AppState,
    req: &Inbound,
    reading: &crate::client_api::Reading,
    choice: &Choice,
    to: (&str, tw_api::Billing),
    fp: Option<&str>,
    ending: &mut Option<crate::ending::Ending>,
    (redaction, found): (
        crate::bodies::Redaction,
        Option<std::sync::Arc<[tw_guard::redact::rules::Hit]>>,
    ),
) -> (u64, u64) {
    let facts = &reading.facts;
    let id = state.bus.next_id();
    let at_ms = now_ms();
    state.bus.emit(tw_api::Event::RequestStarted {
        id,
        client: req.client_name.clone(),
        // **旁证，不是身份。**只用来显示和判断「接管生效了吗」，
        // 不参与鉴权、路由、配额（见 crate::hint）。
        client_hint: crate::hint::client_hint(&req.headers),
        session: fp.map(|fp| state.sessions.assign(fp, at_ms)),
        peer: req.from.peer.clone(),
        key_masked: req.from.key.clone(),
        route: choice.route.clone(),
        rule: choice.rule.clone(),
        group: choice.group.clone(),
        rewritten_by: choice.rewritten_by.clone(),
        provider: to.0.to_string(),
        billing: to.1,
        model: facts.model.clone(),
        method: "POST".to_string(),
        path: req.uri.path().to_string(),
        // 路由已经估过的那个数，不再算一遍。**它也就是发给上游的那一份的估算**：之后每
        // 一跳只会改模型名、输出上限和推理开关（规则）、换一种写法（格式转换）、把几个值
        // 换成占位符（脱敏）—— 前两样不动这个数，脱敏差出的几个 token 在估算本身的误差
        // 之内。解不开的请求没有中间表示，也就没有估算
        input_estimate: matches!(reading.decoded, Some(Ok(_))).then_some(facts.input_tokens),
        session_log_bytes: reading.harness.and_then(|h| h.session_log_bytes),
        at_ms,
    });
    let sink = state.body_sink();
    let mut end = crate::ending::Ending::new(
        state.bus.clone(),
        id,
        facts.model.clone(),
        req.started,
        at_ms as i64,
        sink.clone(),
    );
    end.redact_with(redaction.clone());
    *ending = Some(end);

    // 请求体交给观测层。**这时候它已经完整在内存里了**，所以这一步
    // 除了一次 `Bytes` 的引用计数之外没有别的成本（说过入站是要
    // 整个解析的，所以本来就在）；比存得下的还长的，只拷开头那一段（见
    // `bodies::offer`）。**交出去的是原文**：换掉、打码在落盘那一头做，不占
    // 转发这条路（见 `crate::bodies`）。
    //
    // 存的是**客户端发来的那一份**（内容过滤删过字的话是删过的样子，见 `screen`）。
    // 插件改过的话，回答它的那一跳收到的那一份在试完上游之后另存（见 `hop`），换掉、
    // 打码的规矩一样
    crate::bodies::offer(
        &sink,
        crate::bodies::BodyRecord::new(
            id,
            at_ms as i64,
            crate::bodies::BodyKind::Request,
            req.body.clone(),
            req.body.len(),
            redaction,
        )
        .found(found),
    );
    (id, at_ms)
}

/// 管线第 4 步：内容过滤（见 [`crate::guard::screen`]）。**只下结论，不发事件**：记录
/// 要挂在请求号上，开始之后再报（[`crate::guard::report`]）。
///
/// **在开始事件之前**：处置档下删过的话，`req.body` 换成删过的那一份，中间表示也照它
/// 重新解码 —— 之后的出站脱敏、开始事件、留档、每一跳的插件、转换和发送用的都是它，
/// 存下来的就是真正发出去的那一份（插件改过的另存，见 `hop`）。路由在这之前按客户端的
/// 原文做完了。插件在某一跳改过的请求，在那一跳再查一遍，只报插件加进来的（见 [`plug`]）。
///
/// 查的是会让模型读调用方正文的请求：生成回答和压缩上下文。**计 token 不查**：不跑
/// 模型，查了只会在真正的请求之前把同一处命中多记一遍、还可能把计数请求拒掉（理由见
/// [`crate::client_api::ClientApi::screened`]）。在原文上查，中间表示解不开的请求照样查
/// （同格式直通照样发，上游可能认得它）。
///
/// `parsed` 是请求体解析出来的样子（管线开头解的那一份），解不开的是 None：解不开的请求体
/// 没有调用方的正文可言，什么都不报。
fn screen(
    rt: &Runtime,
    req: &mut Inbound,
    reading: &mut crate::client_api::Reading,
    parsed: Option<&serde_json::Value>,
) -> tw_guard::content::Screening {
    let screened = crate::client_api::ClientApi::screened(req.uri.path());
    let (Some(api), Some(parsed)) = (req.api.filter(|_| screened), parsed) else {
        return Default::default();
    };
    let dialect = api.dialect();
    let sc = crate::guard::screen_value(&crate::guard::Screen::of(rt), dialect, parsed);
    if let Some(body) = &sc.body {
        req.body = body.clone();
        if reading.decoded.is_some() {
            reading.decoded = Some(
                serde_json::from_slice::<serde_json::Value>(body)
                    .map_err(|_| {
                        tw_dialect::ir::Rejection("The request body is not valid JSON.".into())
                    })
                    .and_then(|v| {
                        tw_dialect::convert::decode(
                            dialect,
                            &v,
                            req.uri.path(),
                            req.query.as_deref(),
                        )
                    }),
            );
        }
    }
    sc
}

/// 请求体到这么大，管线上整份解它、扫它的那几步挪出异步线程（见 [`heavy`]）。
const HEAVY_BODY: usize = 1024 * 1024;

/// 在整个请求体上做的 CPU 活：解析、本地应答的判定、内容过滤、出站脱敏、每一跳改写请求体。
///
/// **大的请求体挪出异步线程做**（`block_in_place`）：一个 8 MB 的请求要算几十毫秒，就地算的话，
/// 同一个线程上别的连接 —— 正在流的回答 —— 跟着停住这么久。挪出去时这个线程上排着的活交给
/// 别的线程接着干。小的就地做：挪一次的开销比它本身还大。
///
/// 只在多线程的运行时上挪：单线程的运行时（测试用的就是它）里 `block_in_place` 会 panic。
pub(super) fn heavy<T>(body: &[u8], f: impl FnOnce() -> T) -> T {
    heavy_for(body.len(), f)
}

/// [`heavy`]，按请求体的长度：要在 `f` 里改请求体的调用方先量好长度
pub(super) fn heavy_for<T>(size: usize, f: impl FnOnce() -> T) -> T {
    use tokio::runtime::{Handle, RuntimeFlavor};
    let threads =
        Handle::try_current().is_ok_and(|h| h.runtime_flavor() == RuntimeFlavor::MultiThread);
    if size >= HEAVY_BODY && threads {
        tokio::task::block_in_place(f)
    } else {
        f()
    }
}

/// 这个请求是 Claude Code 发的吗。
///
/// **`max_tokens: 1` 那条判定必须同时要求它**，否则会误伤别人
/// 真实的 `max_tokens: 1` 请求 —— 而误判的代价是用户看到一个凭空出现的
/// 假答案，且完全无从察觉。
///
/// 两个信号取或：配置里那个客户端叫什么（`twcore init` 生成的名字就是
/// `claude-code`），以及 UA。**都不是铁证**，所以这里只做「更窄」用：
/// 认不出来就不拦，那是正确的失败方向。
fn is_claude_code(client_name: &str, headers: &HeaderMap) -> bool {
    if client_name == "claude-code" {
        return true;
    }
    headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ua| ua.to_ascii_lowercase().starts_with("claude-cli/"))
}

/// 伪造一个应答。形状跟着请求走 —— 客户端按自己请求的形状去解析，
/// 回错了形状比不拦截更糟。
fn local_answer(kind: crate::clientprobe::ProbeKind, body: &Bytes) -> Response {
    if crate::clientprobe::wants_stream(body) {
        return (
            [
                (axum::http::header::CONTENT_TYPE, "text/event-stream"),
                (axum::http::header::CACHE_CONTROL, "no-cache"),
                // 让本地应答在响应里也是可见的。**对客户端逼真，对用户
                // 透明** —— 这两件事不矛盾，因为看这个头的是人。
                (
                    axum::http::HeaderName::from_static("x-thinkwatch-local"),
                    "1",
                ),
            ],
            crate::clientprobe::sse_response(kind, body),
        )
            .into_response();
    }
    (
        [(
            axum::http::HeaderName::from_static("x-thinkwatch-local"),
            "1",
        )],
        axum::Json(crate::clientprobe::json_response(kind, body)),
    )
        .into_response()
}
