//! 一个请求的管线：身份识别之后，到响应交给客户端为止。
//!
//! 按「管线第 N 步」分成几个函数，[`pipeline`] 只负责按顺序串起来 ——
//! 每一步各自决定「放行、挡下、还是本地就答了」，顺序本身就是设计：
//! 本地应答在准入之前（离线也要能答），准入在路由之前（列表即承诺），
//! 并发闸门在路由之后（被规则挡下的不用先排队）。
//!
//! 发出开始事件之后的两段各自一个子模块：[`hop`] 依次试候选上游，
//! [`relay`] 把选中那一家的响应交给客户端。

use std::sync::Arc;

use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;

use super::{Choice, Sender, now_ms};
use crate::error::GatewayError;
use crate::forward;
use crate::state::{AppState, Runtime};
use tw_types::msg;

mod hop;
mod relay;

/// 256 MiB。大到能装下几张 4K 图的 base64（膨胀 33%），小到失控的
/// 客户端打不爆内存。
const MAX_BODY: usize = 256 * 1024 * 1024;

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
    /// 熔断过滤之后的候选，按顺序试
    alive: Vec<String>,
    /// 第一阶段的结论。路由事件在它上面补上第二阶段和尝试链
    choice: Choice,
}

pub(super) async fn pipeline(
    state: AppState,
    rt: Arc<Runtime>,
    req: Inbound,
    live: crate::live::Pass,
    ending: &mut Option<crate::ending::Ending>,
) -> Result<Response, GatewayError> {
    forward::check_body_size(&req.body, MAX_BODY)?;

    let intent = match probe(&state, &rt, &req) {
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

    let (reading, fp) = read(&req, intent);
    admit(&state, &rt, &req, &reading)?;
    let (choice, decision) = match route(&state, &rt, &req, &reading, fp.as_deref())? {
        Routed::Go(choice, decision) => (choice, decision),
        // 规则做了决定，请求却一家上游都不会去。**照样开始、照样报路由**，失败由
        // `passthrough` 按这个错误报 —— 不排队：它一个字节都不会发出去
        Routed::Refused(choice, why) => {
            let to = ("", tw_api::Billing::PerToken);
            let id = open(&state, &req, &reading, &choice, to, fp.as_deref(), ending);
            state.bus.emit(super::routed_nowhere(id, choice));
            return Err(why);
        }
    };

    // 管线第 3 步：这把密钥自己的并发上限。**等，不拒绝** —— 理由在
    // `crate::limits`。放在路由之后：被规则挡下的请求不用先等一轮
    let limit = rt
        .config
        .clients
        .iter()
        .find(|c| c.name == req.client_name)
        .and_then(|c| c.max_concurrent);
    let _pass = state.gate.acquire(&req.client_name, limit).await;

    let started = start(
        &state,
        &rt,
        &req,
        &reading,
        choice,
        &decision,
        fp.as_deref(),
        ending,
    );
    screen(&state, &rt, &reading, &started)?;
    let served = hop::try_upstreams(&state, &rt, &req, &reading, &decision, &started).await?;
    let ending = ending
        .take()
        .expect("written when the start event was emitted");
    Ok(relay::respond(
        &state,
        &rt,
        &req,
        reading.generates,
        served,
        started.id,
        live,
        ending,
    ))
}

/// 管线第 1.3 步的结果。
enum Probe {
    /// 本地答了，不往下走
    Answered(Response),
    /// 照常往下走。`route` 档打上的标记，其余是空串
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
fn probe(state: &AppState, rt: &Runtime, req: &Inbound) -> Probe {
    let Some(kind) =
        crate::clientprobe::classify(&req.body, is_claude_code(&req.client_name, &req.headers))
    else {
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
        // `route` 交给规则处理：打一个标记让 `when: { intent: ... }`
        // 能匹配到，然后照常往下走。
        Route => Probe::Intent(kind.slug().to_string()),
        // **`passthrough` 不打标记。**打了的话，一条
        // `when: { intent: assistant_internal }` 的规则会在用户还
        // 没把那类请求配成 route 的时候就开始生效 —— 而配置文件里
        // 看不出任何线索。
        Passthrough => Probe::Intent(String::new()),
    }
}

/// 管线第 2 步的前半：读出路由事实，和这段对话的指纹。
///
/// **只解析一次，指纹也只算一次。**路由要它，会话粘滞的策略组要指纹，开始事件
/// 归会话也要指纹，而 body 可能有几百 KB —— 解两遍、哈希两遍是白付一份钱。
///
/// 生成回答的请求解码成中间表示，**四种格式的客户端读出同一份路由事实**。
/// body 解不开时用空的性质走兜底规则。**不要因此拒绝请求** —— 我们的解析器
/// 不认识的东西，上游可能完全认识（只有需要转换时才用得上解码结果）
fn read(req: &Inbound, intent: String) -> (crate::client_api::Reading, Option<String>) {
    let parsed = serde_json::from_slice::<serde_json::Value>(&req.body).ok();
    let mut reading =
        crate::client_api::read(req.uri.path(), req.query.as_deref(), parsed.as_ref());
    reading.facts.client = req.client_name.clone();
    reading.facts.intent = intent;
    reading.harness = tw_dialect::harness::detect(
        req.headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok()),
        parsed.as_ref(),
    );
    // 认出「这几十个请求是同一次任务」。**认不出来就是 None** —— 硬凑一个会把
    // 互不相干的请求并成一个「会话」
    let fp = parsed.as_ref().and_then(crate::session::fingerprint);
    (reading, fp)
}

/// 管线第 1.5 步：模型准入。**和 `GET /v1/models` 共用同一个函数**
/// —— 列出来的一定能用，能用的一定列了出来。
///
/// 目录空着时不拦：那说明探测还没回来或者上游都不给列表，这时候拦
/// 等于把整个网关关掉。
fn admit(
    state: &AppState,
    rt: &Runtime,
    req: &Inbound,
    reading: &crate::client_api::Reading,
) -> Result<(), GatewayError> {
    let facts = &reading.facts;
    let catalog = state.catalog.load();
    if catalog.is_empty() || facts.model.is_empty() {
        return Ok(());
    }
    let allow = rt
        .config
        .clients
        .iter()
        .find(|c| c.name == req.client_name)
        .and_then(|c| c.allow.clone());
    let servable = req
        .api
        .map(|a| crate::client_api::slugs(a.servable_by(reading.generates)));
    if catalog.admits(&facts.model, servable.as_deref(), allow.as_deref()) {
        return Ok(());
    }
    // 错误信息要说清是哪一种：没有上游提供它，和这个客户端不让用它，
    // 该去改的地方不一样
    let why = if catalog.providers_for(&facts.model).is_empty() {
        msg!(
            "gw.model.no_upstream", model = facts.model.clone() =>
            "No upstream serves model {model}. GET /v1/models lists the models that \
             are available."
        )
    } else {
        msg!(
            "gw.model.not_allowed", key = req.client_name.clone(), model = facts.model.clone() =>
            "Gateway key `{key}` may not use model {model}. GET /v1/models lists the \
             models that are available."
        )
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
/// 那时说不出这个请求算在哪条规则上，不留这一行，和鉴权没过一样。
fn route(
    state: &AppState,
    rt: &Runtime,
    req: &Inbound,
    reading: &crate::client_api::Reading,
    fp: Option<&str>,
) -> Result<Routed, GatewayError> {
    let facts = &reading.facts;
    let route = rt.engine.route_of(&req.client_name).to_string();
    let mut decision = match rt.engine.route(facts).map_err(|e| {
        GatewayError::config(msg!("gw.route.failed", detail = e => "Routing failed: {detail}"))
    })? {
        tw_engine::Outcome::Route(d) => d,
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
            };
            return Ok(Routed::Refused(choice, why));
        }
    };
    let choice = Choice {
        route,
        rule: decision.matched_rule.clone(),
        group: decision.via_group.clone(),
        rewritten_by: decision.rewritten_by.clone(),
    };
    // 去掉服务不了这个请求的候选：停用的、范围外的、清单里没有这个模型的。
    // **在排序之前** —— `cheapest` 和 `url-test` 要在能服务的上游里挑。
    //
    // 不跳过的话，一家没有这个模型的上游排在前面，它回的 404 不触发故障
    // 转移，请求就在一家能服务它的上游旁边失败了。
    let serving = crate::models::serving(
        &rt.config,
        &state.catalog.load(),
        &decision.candidates,
        &facts.model,
    );
    if serving.usable.is_empty() {
        return Ok(Routed::Refused(choice, serving.explain(&facts.model)));
    }
    if !serving.skipped.is_empty() {
        tracing::debug!(skipped = ?serving.skipped, model = %facts.model, "skipping the candidates that cannot serve this request");
    }
    decision.candidates = serving.usable;
    // 策略组排序。**引擎给的是集合，顺序在这儿定** ——
    // 因为 `load-balance` / `url-test` / `cheapest` 都要运行时的数字，
    // 而路由决策本身必须是纯的、可试算的。
    //
    // `fallback` 和 `select` 走不到这里面 —— 那是绝大多数人的配置，
    // 它们连一个 HashMap 都不用建。
    if let Some(gname) = decision.via_group.clone()
        && let Some(kind) = rt
            .engine
            .groups()
            .iter()
            .find(|g| g.name == gname)
            .map(|g| g.kind)
        && kind.needs_runtime()
    {
        let facts_rt = tw_engine::Facts {
            seq: state.bus.peek_id(),
            ttfb_ms: match kind {
                tw_engine::GroupType::UrlTest => state.latency.snapshot(&decision.candidates),
                _ => Default::default(),
            },
            price: match kind {
                tw_engine::GroupType::Cheapest => {
                    state.unit_prices(&rt.config.providers, &decision.candidates, &facts.model)
                }
                _ => Default::default(),
            },
            session: fp.map(str::to_string),
        };
        decision.candidates = rt
            .engine
            .order(Some(&gname), &decision.candidates, &facts_rt);
    }
    Ok(Routed::Go(choice, decision))
}

/// 发出开始事件：熔断过滤、`RequestStarted`、结局、入站脱敏的记录、请求体留档。
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
    let id = open(
        state,
        req,
        reading,
        &choice,
        (first, billing.into()),
        fp,
        ending,
    );

    // 出站脱敏：按全局的规则看一遍客户端发来的原文。**观察档和拦截档报的
    // 是同一条记录**，差别只在换没换 —— 真正的替换在每一跳发出去之前做，
    // 那一跳的请求体可能是转换过格式的。
    let redact_mode = rt.config.security.redact.mode;
    let found = crate::guard::find(redact_mode, &rt.redact, &req.body);
    if !found.is_empty() {
        state.bus.emit(tw_api::Event::SecretsFound {
            id,
            provider: alive.first().cloned().unwrap_or_default(),
            replaced: redact_mode.acts(),
            items: crate::guard::items(&found),
            at_ms: now_ms(),
        });
    }
    Started { id, alive, choice }
}

/// 发 `RequestStarted`、把这个请求欠着的结局放进 `ending`、把请求体交去留档，
/// 交回这个请求的号。`to` 是要发往的那一家和它怎么收钱；一家都不会去的（被规则
/// 拒绝了）是空的名字。
///
/// **会话在这里定**（见 [`crate::session::Sessions`]）：开始事件带着它，落库的
/// 那一行记的也是它。
fn open(
    state: &AppState,
    req: &Inbound,
    reading: &crate::client_api::Reading,
    choice: &Choice,
    to: (&str, tw_api::Billing),
    fp: Option<&str>,
    ending: &mut Option<crate::ending::Ending>,
) -> u64 {
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
        session_log_bytes: reading.harness.and_then(|h| h.session_log_bytes),
        at_ms,
    });
    let sink = state.body_sink();
    *ending = Some(crate::ending::Ending::new(
        state.bus.clone(),
        id,
        facts.model.clone(),
        req.started,
        at_ms as i64,
        sink.clone(),
    ));

    // 请求体交给观测层。**这时候它已经完整在内存里了**，所以这一步
    // 除了一次 `Bytes` 的引用计数之外没有别的成本（说过入站是要
    // 整个解析的，所以本来就在）。
    crate::bodies::offer(
        &sink,
        crate::bodies::BodyRecord {
            id,
            at_ms: at_ms as i64,
            kind: crate::bodies::BodyKind::Request,
            body: req.body.clone(),
            original_len: req.body.len(),
        },
    );
    id
}

/// 请求防护：调用方发来的正文里（连同工具结果）有没有藏起来的字符、有没有命中
/// 内容规则（见 [`crate::guard::screen`]）。
///
/// **在开始事件之后**：记录要挂在这个请求上，拒掉的请求也要在流量里留一行 ——
/// 被拒是一次来源为 `denied` 的失败。**在尝试上游之前**：拒掉的一个字节都不发。
///
/// 按解码出来的消息看，所以只有生成回答的请求才看：计 token、嵌入这些接口没有
/// 「调用方的消息」可言；解不开的体也不看 —— 同格式直通照样发，上游可能认得它。
fn screen(
    state: &AppState,
    rt: &Runtime,
    reading: &crate::client_api::Reading,
    started: &Started,
) -> Result<(), GatewayError> {
    let Some(Ok(d)) = &reading.decoded else {
        return Ok(());
    };
    let provider = started.alive.first().map(String::as_str).unwrap_or("");
    match crate::guard::screen(
        &state.bus,
        started.id,
        provider,
        &crate::guard::Screen::of(rt),
        &d.request,
    ) {
        Some(why) => Err(GatewayError::denied(why)),
        None => Ok(()),
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
