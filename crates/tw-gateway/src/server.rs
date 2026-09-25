//! HTTP 服务：路由、来源与身份检查，然后交给管线（见 [`pipeline`]）。

use std::sync::Arc;

use axum::Router;
use axum::extract::{OriginalUri, RawQuery, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{any, get};
use bytes::Bytes;

use crate::error::GatewayError;
use crate::health::Health;
use crate::state::AppState;
use listing::{get_model, list_models};
use tw_types::msg;

mod listing;
mod pipeline;
mod upgrade;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        // **和准入共用同一个函数** —— 列表和准入不可能不一致。
        .route("/v1/models", get(list_models))
        // **单点查询要走同一道准入**。不接这条的话它掉进
        // fallback 直接透传上游 —— 一个被 `allow` 限制成只能用便宜
        // 模型的 client，`GET /v1/models/claude-opus-4` 照样拿 200。
        // 这个洞在别的项目里点过名（「都没做过滤」），而我们自己
        // 也漏了。**同一个 `admits` 函数，列表和单点不可能不一致。**
        //
        // **只截 GET。**Gemini 把调用写成 `POST /v1beta/models/{model}:generateContent`，
        // 和单点查询是同一个路径模式 —— 没有这个 `fallback`，那些请求会被这条只认
        // GET 的路由拒成 405，永远到不了透传
        .route("/v1/models/{model}", get(get_model).fallback(passthrough))
        // Gemini 方言的路径。它的客户端问的是 `/v1beta/models` 和 `/v1beta/models/x`
        .route("/v1beta/models", get(list_models))
        .route(
            "/v1beta/models/{model}",
            get(get_model).fallback(passthrough),
        )
        // M0 只有透传：任何方法、任何路径都往上游送。M1 加路由时，
        // 这里会先过规则引擎再决定送给谁。
        .fallback(any(passthrough))
        .with_state(state)
}

async fn passthrough(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    OriginalUri(uri): OriginalUri,
    RawQuery(query): RawQuery,
    // **必须排在 `body` 前面。**提取器按顺序跑，而 `Bytes` 会把体吃掉
    // —— 一次升级要的是那条连接本身，体被读走之后就没得升了
    crate::ws::MaybeUpgrade(upgrade): crate::ws::MaybeUpgrade,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, GatewayError> {
    let started = std::time::Instant::now();
    // 从这一刻起它就算「在服务中」。**排队等并发名额的也算** —— 那时客户
    // 端的连接已经开着在等了，这时候重启网关一样会让它失败。
    let live = state.live.enter();
    // **整个请求只取一次运行时。**中途重新取会让一个请求跨在两份配置
    // 上：按新规则选了 provider，却拿旧的 Client 去发 —— 而那种不一致
    // 完全静默。
    let rt = state.runtime();
    // 来源检查在身份检查**之前**：一个不该连过来的地址，不该有机会
    // 试密钥。
    if !rt.allow.allows(peer.ip()) {
        return Err(GatewayError::new(
            crate::error::Source::Auth,
            msg!(
                "gw.auth.source_not_allowed_hint", peer = peer.ip() =>
                "{peer} is not among the allowed source addresses. Change \
                 listen.gateway.allow_from, or set bind to loopback."
            ),
        ));
    }
    let (client_name, position) = state.identify(&headers, query.as_deref())?;
    // 除了密钥的名字，这条请求是谁发的
    let from = Sender {
        peer: crate::hint::peer_of(peer.ip()),
        key: rt
            .config
            .clients
            .iter()
            .find(|c| c.name == client_name)
            .map(|c| tw_secret::mask_secret(&c.key)),
    };
    // WebSocket 升级。**在鉴权之后、解体之前分叉** —— 鉴权
    // 在前是因为一个不该连过来的地址不该有机会升级；解体之前是因为
    // 升级要的是那条连接，而 `Bytes` 会把它读干净。
    if let Some(ws) = upgrade.filter(|_| crate::ws::is_upgrade(&headers)) {
        return upgrade::ws_upgrade(
            state,
            rt,
            ws,
            client_name,
            uri,
            query,
            headers,
            started,
            live,
            from,
        )
        .await;
    }
    // 客户端调的是哪种 API：**看路径**（见 `client_api`）。认不出的路径照旧
    // 直通，出错时的格式退回按密钥位置猜。
    let api = crate::client_api::ClientApi::of_path(uri.path());
    // 从这里往下，所有错误都要用客户端自己那套结构回。
    // **认证失败在这一行之前，那时方言还猜不出来** —— key 就是没认出来
    // 的，只能退回 Anthropic 形状，而那是桌面版的主用例。
    let dialect = api
        .map(|a| a.dialect())
        .unwrap_or_else(|| position.dialect());
    //
    // 这个请求的结局（见 `crate::ending`）。**管线发出开始事件时把它放
    // 进来。**
    //
    // 它待在这一层而不是管线里面，是因为只有这里分得清两件事：管线**返回
    // 了**一个错误，和管线**被丢掉了**。前者是失败，按返回的错误报；后者
    // 是客户端在响应头到达之前就走了 —— hyper 丢掉整个 handler，这个变量
    // 跟着被丢掉，由 Drop 报成取消。
    //
    // 交给管线里每一条 `return Err` 各自去报的话，漏掉一条的后果不是没报，
    // 而是被 Drop 报成「客户端取消」—— 一次策略拒绝会记到客户端头上。
    let mut ending: Option<crate::ending::Ending> = None;
    let req = pipeline::Inbound {
        uri,
        query,
        headers,
        body,
        client_name,
        api,
        dialect,
        started,
        from,
    };
    let result = pipeline::pipeline(state, rt, req, live, &mut ending).await;
    if let Some(end) = ending.take() {
        match &result {
            Err(e) => end.failed(e.source.into(), e.detail.clone()),
            // 成功的路径都把结局交给了响应体，**走到这里是漏交了**。那也只能
            // 按拿到的状态码报结束 —— 不能让它掉在地上，被记成一次取消
            Ok(resp) => end.finished(resp.status().as_u16()),
        }
    }
    // **在一个地方给方言，而不是在每个 return 点。**后者只要漏一处，
    // 那条路径上的客户端就会收到一个它解析不了的 body，而那个失败看
    // 起来和真实原因毫无关系。
    result.map_err(|e| e.in_dialect(dialect))
}

/// 熔断状态变了就报一条，没变什么都不做。
///
/// **用自己的 id，不用这次请求的。**挂上请求的 id 会让存储层把它当成
/// 那次请求的一部分 —— 而熔断说的是「这家上游现在什么情况」，和触发它
/// 的那一次请求已经没关系了。
///
/// 开的时候顺手排一个定时器：**冷却到点本身就是一次状态变化**，而它
/// 不由任何调用触发。不报的话，界面会一直显示「熔断中」，直到碰巧又有
/// 一个请求打到这家为止。
///
/// 定时器会**自己续期**，因为熔断期间的每一次失败都会把冷却顶到更晚
/// （全都熔断时我们是放行的，所以那些失败照样打得到这家）。只睡一次
/// 就下结论的话，它醒来时看到的是一段还没走完的冷却 —— 然后闭嘴，而
/// 真正到点的那一刻再也没有人报。
fn note_health(
    bus: &tw_observe::EventBus,
    health: &Arc<Health>,
    provider: &str,
    change: Option<crate::health::State>,
) {
    let Some(next) = change else { return };
    let say = |bus: &tw_observe::EventBus, name: String, open: bool| {
        let id = bus.next_id();
        bus.emit(tw_api::Event::HealthChanged {
            id,
            provider: name,
            state: if open {
                tw_api::BreakerState::Open
            } else {
                tw_api::BreakerState::Closed
            },
            at_ms: now_ms(),
        });
    };
    let open = next == crate::health::State::Open;
    say(bus, provider.to_string(), open);
    if !open {
        return;
    }
    let (bus, health, name) = (bus.clone(), health.clone(), provider.to_string());
    tokio::spawn(async move {
        let mut left = crate::health::COOLDOWN;
        loop {
            tokio::time::sleep(left).await;
            match health.cooldown_left(&name) {
                // 中途成功过了 —— `record_success` 已经报过恢复
                None => return,
                Some(d) if d.is_zero() => return say(&bus, name, false),
                // 又失败了一次，冷却被顶后。接着等，别现在就说它好了
                Some(d) => left = d,
            }
        }
    });
}

/// 这条请求是谁发的：密钥的名字之外的两样。**记下的是请求那一刻的** ——
/// 密钥换过之后，老记录上的尾巴照样对得上。
#[derive(Debug, Clone, Default)]
struct Sender {
    /// 这条连接对面的地址。本机来的是 None（见 `hint::peer_of`）
    peer: Option<String>,
    /// 请求带的那把网关密钥打码后的样子（`tw-re…wb4e`）
    key: Option<String>,
}

/// 在一个地址上起服务，**不跟配置走**。绑上了就返回，交回真的地址；服务在后台
/// 一直跑到运行时结束。测试用它。
///
/// **端口给 0，由系统挑**，从返回值拿到真的端口。别先绑一个 0 端口拿号、放掉、
/// 再把号交给这里：放掉之后号回到系统手里，并行的测试绑 0、发起连接都会被分到
/// 它，赶在这里绑之前拿走就是一次和代码无关的「端口被占」。
pub async fn serve(
    state: AppState,
    addr: std::net::SocketAddr,
) -> std::io::Result<std::net::SocketAddr> {
    crate::listen::serve_detached(state, addr).await
}

/// 上游回了话的一跳：`served` 或者 `status`。
pub(crate) fn hop(
    provider: &str,
    outcome: tw_api::AttemptOutcome,
    status: u16,
    started: std::time::Instant,
) -> tw_api::AttemptView {
    tw_api::AttemptView {
        provider: provider.to_string(),
        outcome,
        status: Some(status),
        error: None,
        ms: started.elapsed().as_millis() as u64,
    }
}

/// 没有收到响应的一跳。`error` 和这一跳报给客户端的那条错误是同一句。
pub(crate) fn hop_failed(
    provider: &str,
    error: tw_types::Msg,
    started: std::time::Instant,
) -> tw_api::AttemptView {
    tw_api::AttemptView {
        provider: provider.to_string(),
        outcome: tw_api::AttemptOutcome::Error,
        status: None,
        error: Some(error),
        ms: started.elapsed().as_millis() as u64,
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 规则名后面那半句「为什么」。**自定义规则没有这一句**，那时整个括号都不要，
/// 不留一对空括号。
pub(crate) fn because(why: &str) -> String {
    if why.is_empty() {
        String::new()
    } else {
        format!(" ({why})")
    }
}

/// 一次工具调用命中写成事件。流式、整包、WebSocket 三条路共用 —— 字段写漏
/// 一个，就有一条路上的日志说不清是哪条规则。
pub(crate) fn flagged(
    id: u64,
    provider: &str,
    v: &tw_guard::tools::wall::Verdict,
    blocked: bool,
) -> tw_api::Event {
    tw_api::Event::ToolCallFlagged {
        id,
        provider: provider.to_string(),
        tool: v.tool.clone(),
        rule: v.rule.clone(),
        custom: v.custom,
        why: v.why.clone(),
        excerpt: v.excerpt.clone(),
        action: if v.cut {
            tw_api::RuleAction::Cut
        } else {
            tw_api::RuleAction::Record
        },
        blocked,
        at_ms: now_ms(),
    }
}
