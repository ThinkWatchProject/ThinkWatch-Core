//! 控制面：unix socket 上的 HTTP。
//!
//! **不占 TCP 端口**（DESIGN.md §2.1）。理由不是省端口，是权限：一个
//! `0700` 的 socket 文件天然只有当前用户能连，不需要再发明一套 token。
//! 只有用户显式开启远程访问时才监听 TCP，那时才需要 token。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::Stream;
use tokio::sync::broadcast;

pub use tw_observe::EventBus;

#[derive(Clone)]
pub struct ControlState {
    pub started: std::time::Instant,
    pub config: Arc<tw_config::Config>,
    pub config_path: PathBuf,
    pub gateway_addr: Option<String>,
    pub bus: EventBus,
    /// 探测复用数据面的 HTTP 客户端 —— 同一套超时、同一套代理设置。
    /// 另起一个会让「探测通了但实际请求不通」变成可能。
    pub http: reqwest::Client,
    /// 上游健康。界面要显示哪家在熔断中。
    pub health: Arc<tw_gateway::Health>,
}

pub fn router(state: ControlState) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/events", get(events))
        .route("/overview", get(overview))
        .route("/probe", post(probe))
        .route("/l1", post(l1))
        .route("/setup", post(setup))
        .with_state(state)
}

async fn status(State(s): State<ControlState>) -> Json<tw_api::Status> {
    Json(tw_api::Status {
        api_version: tw_api::CONTROL_API_VERSION,
        version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
        gateway_addr: s.gateway_addr.clone(),
        config_path: s.config_path.display().to_string(),
        clients: s.config.clients.len(),
        providers: s.config.providers.len(),
        uptime_secs: s.started.elapsed().as_secs(),
    })
}

async fn events(
    State(s): State<ControlState>,
) -> Sse<impl Stream<Item = Result<SseEvent, std::convert::Infallible>>> {
    let rx = s.bus.subscribe();
    let stream = async_stream_from(rx);
    // 心跳。UI 那边要能区分「没有请求」和「连接断了」—— 没有心跳的话
    // 一个安静的下午看起来就像挂了。
    Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
}

fn async_stream_from(
    mut rx: broadcast::Receiver<tw_api::Event>,
) -> impl Stream<Item = Result<SseEvent, std::convert::Infallible>> {
    futures::stream::unfold(rx_state(&mut rx), |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let data = serde_json::to_string(&ev).unwrap_or_default();
                    return Some((Ok(SseEvent::default().data(data)), rx));
                }
                // 订阅者跟不上时 broadcast 会丢最老的。**继续收而不是断开** ——
                // UI 少几行实时日志无所谓，断掉重连才是真的难受。
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped = n, "控制面订阅者跟不上，丢了一些事件");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

fn rx_state(rx: &mut broadcast::Receiver<tw_api::Event>) -> broadcast::Receiver<tw_api::Event> {
    rx.resubscribe()
}

/// 界面要显示的配置概览。**密钥只给来源，不给值。**
async fn overview(State(s): State<ControlState>) -> Json<tw_api::Overview> {
    let cfg = &s.config;
    let engine = cfg.engine();
    Json(tw_api::Overview {
        providers: cfg
            .providers
            .iter()
            .map(|p| tw_api::ProviderView {
                name: p.name.clone(),
                base_url: tw_secret::redact_url(&p.base_url),
                key_source: p.key.describe(),
                protocol: p.effective_protocol().map(|x| format!("{x:?}")),
                proxy: p.proxy.clone(),
                health: match s.health.state(&p.name) {
                    tw_gateway::health::State::Closed => "ok".into(),
                    tw_gateway::health::State::Open => "open".into(),
                },
            })
            .collect(),
        routes: engine
            .routes()
            .iter()
            .map(|r| tw_api::RouteView {
                name: r.name.clone(),
                // 阶段二的规则没有去向 —— 它们只改参数或拒绝（§3.4）
                to: r.to.clone().unwrap_or_else(|| {
                    if r.deny.is_some() {
                        "拒绝".into()
                    } else {
                        "（只改参数）".into()
                    }
                }),
                conditions: describe_when(&r.when),
            })
            .collect(),
        groups: engine
            .groups()
            .iter()
            .map(|g| tw_api::GroupView {
                name: g.name.clone(),
                kind: format!("{:?}", g.kind).to_lowercase(),
                providers: g.providers.clone(),
                hurts_cache: g.kind.hurts_cache(),
            })
            .collect(),
        clients: cfg
            .clients
            .iter()
            .map(|c| tw_api::ClientView {
                name: c.name.clone(),
                key: tw_secret::mask_secret(&c.key),
                max_concurrent: c.max_concurrent,
            })
            .collect(),
        listen: tw_api::ListenView {
            bind: format!("{:?}", cfg.listen.gateway.bind).to_lowercase(),
            port: cfg.listen.gateway.port,
            allow_from: cfg.listen.gateway.effective_allow_from(),
            exposed: cfg.listen.gateway.bind.is_exposed(),
        },
    })
}

/// 把 `when` 写成人话。
///
/// **规则列表上必须能直接读懂条件** —— 让用户去对着 YAML 猜「这条为什么
/// 没命中」，正是 §7.11 那类「我明明配了」问题的来源。
fn describe_when(w: &tw_engine::rule::When) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(m) = &w.model {
        out.push(format!("模型 {m}"));
    }
    if let Some(c) = &w.client {
        out.push(format!("客户端 {c}"));
    }
    if let Some(d) = &w.dialect {
        out.push(format!("方言 {d}"));
    }
    for (label, v) in [
        ("输入 token", &w.input_tokens),
        ("max_tokens", &w.max_tokens),
        ("工具数", &w.tool_count),
    ] {
        if let Some(x) = v {
            out.push(format!("{label} {x}"));
        }
    }
    for (label, v) in [
        ("带缓存", w.cache),
        ("带工具", w.tools),
        ("带图片", w.image),
        ("扩展思考", w.thinking),
        ("流式", w.stream),
    ] {
        if let Some(b) = v {
            out.push(if b {
                label.to_string()
            } else {
                format!("不{label}")
            });
        }
    }
    out
}

/// 探一个上游。零成本，用户可以随便点。
async fn probe(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::ProbeRequest>,
) -> Json<tw_api::ProbeResponse> {
    let r = tw_gateway::probe(&s.http, &req.base_url, &req.key, None).await;
    // 两边的枚举是同一份契约的两个副本（core 内部一份、控制面契约一份）。
    // 手工转换是为了让 tw-api 不依赖 tw-gateway —— UI 和 CLI 只该依赖
    // 契约，不该被拖上整个数据面。
    let models = match r.models {
        tw_gateway::ModelList::Listed { models } => tw_api::ModelList::Listed { models },
        tw_gateway::ModelList::NotImplemented { status } => {
            tw_api::ModelList::NotImplemented { status }
        }
        tw_gateway::ModelList::Unrecognized { sample } => {
            tw_api::ModelList::Unrecognized { sample }
        }
        tw_gateway::ModelList::Empty => tw_api::ModelList::Empty,
    };
    Json(tw_api::ProbeResponse {
        ok: r.ok,
        protocol: r.protocol,
        latency_ms: r.latency_ms,
        models,
        error: r.error,
    })
}

/// L1 测速。**零成本**，不发任何业务请求，用户可以随便点。
///
/// 一次可能测好几家，所以逐个测而不是并发：**并发会让每一段的耗时互相
/// 干扰**，六条线一起抢带宽测出来的 TLS 时间不是任何一条线的真实值，
/// 而这一层存在的全部意义就是那几个数字准不准。
async fn l1(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::L1Request>,
) -> Result<Json<Vec<tw_api::L1Result>>, (StatusCode, String)> {
    let mut out = Vec::new();

    // 只测代理本身。§4.6：代理影响的是网络层，测到 L1 就够了。
    if let Some(name) = &req.proxy {
        let p = s
            .config
            .proxies
            .iter()
            .find(|x| x.name == *name)
            .ok_or_else(|| (StatusCode::NOT_FOUND, format!("没有叫 `{name}` 的代理")))?;
        let mut r = tw_gateway::l1_tcp(&p.addr).await;
        r.notes
            .push("只测到代理这一跳的 TCP 握手。代理影响的是网络层，再往上就该测上游了。".into());
        out.push(view(format!("代理 {}", p.name), None, r));
        return Ok(Json(out));
    }

    // 还没保存时的临时地址（首次配置那一步）。
    if let Some(url) = &req.base_url {
        let r = tw_gateway::l1(url, None).await;
        out.push(view(url.clone(), None, r));
        return Ok(Json(out));
    }

    let targets: Vec<&tw_config::Provider> = match &req.provider {
        Some(n) => vec![
            s.config
                .providers
                .iter()
                .find(|p| p.name == *n)
                .ok_or_else(|| (StatusCode::NOT_FOUND, format!("没有叫 `{n}` 的上游")))?,
        ],
        None => s.config.providers.iter().collect(),
    };
    for p in targets {
        let hop = match resolve_hop(&s.config, p) {
            Ok(h) => h,
            Err(e) => {
                out.push(tw_api::L1Result {
                    target: p.name.clone(),
                    via: Some(p.proxy.clone()),
                    ok: false,
                    segments: Vec::new(),
                    total_ms: 0,
                    notes: Vec::new(),
                    error: Some(e),
                });
                continue;
            }
        };
        let via = hop.as_ref().map(|_| p.proxy.clone());
        let r = tw_gateway::l1(&p.base_url, hop.as_ref()).await;
        out.push(view(p.name.clone(), via, r));
    }
    Ok(Json(out))
}

fn view(target: String, via: Option<String>, r: tw_gateway::L1Result) -> tw_api::L1Result {
    tw_api::L1Result {
        target,
        via,
        ok: r.ok,
        segments: r
            .segments
            .into_iter()
            .map(|x| tw_api::L1Segment {
                name: x.name,
                ms: x.ms,
            })
            .collect(),
        total_ms: r.total_ms,
        notes: r.notes,
        error: r.error,
    }
}

/// 把 provider 的代理名解析成一跳。
///
/// **`system` 这里测不了**：跟随系统代理是 reqwest 在建连时才去查环境的，
/// 我们没有那份地址可以去握手。说出来，而不是假装直连测一遍给个漂亮
/// 数字 —— 那个数字测的根本不是用户实际会走的路。
fn resolve_hop(
    cfg: &tw_config::Config,
    p: &tw_config::Provider,
) -> Result<Option<tw_gateway::ProxyHop>, String> {
    match p.proxy.as_str() {
        tw_config::DIRECT => Ok(None),
        tw_config::SYSTEM => Err(
            "这家走的是系统代理，而系统代理的地址要到建连时才由环境决定 —— L1 测不到它。想量这条线的话，把代理显式配成一个命名条目。"
                .into(),
        ),
        name => {
            let px = cfg
                .proxies
                .iter()
                .find(|x| x.name == name)
                .ok_or_else(|| format!("provider `{}` 要走代理 `{name}`，但 proxies 段里没有这个名字。", p.name))?;
            let auth = match &px.auth {
                None => None,
                Some(a) => Some((
                    a.user.clone(),
                    a.pass
                        .resolve()
                        .map_err(|e| format!("代理 `{name}` 的密码取不出来：{e}"))?,
                )),
            };
            Ok(Some(tw_gateway::ProxyHop {
                kind: px.kind,
                addr: px.addr.clone(),
                auth,
            }))
        }
    }
}

/// 首次运行：写下第一个上游。
///
/// **整文件生成**，不走 §3.8 的最小替换 —— 那是两套机制（§7.6 第 1 步）。
/// 只在还没有 provider 时可用，之后改配置归 M2 的双向同步管。
async fn setup(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::SetupRequest>,
) -> Result<Json<tw_api::SetupResponse>, (StatusCode, String)> {
    // **从磁盘重新读，不看 `s.config`。** 那是启动时的快照，而这个端点
    // 自己就会改磁盘 —— 用快照做守卫，第二次调用会因为看到一份过期的
    // 「零 provider」而通过，然后把刚写好的配置整个覆盖掉。
    //
    // 热重载（M2）之后快照会跟着磁盘走，但那时这条守卫也不该改回去：
    // 「会不会覆盖用户的文件」这种判断，就该问文件本身。
    let current = tw_config::load(&s.config_path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("读配置失败：{e}"),
        )
    })?;
    if !current.providers.is_empty() {
        // 拒绝而不是覆盖。这个端点存在的前提是「还没有配置」，一旦有了
        // 配置，整文件重写会把用户的注释和格式全抹掉。
        return Err((
            StatusCode::CONFLICT,
            "已经配过上游了。改配置请直接编辑 config.yaml，或者等界面上的配置页。".to_string(),
        ));
    }
    let mut cfg = current;
    cfg.providers.push(tw_config::Provider {
        name: req.name.clone(),
        base_url: req.base_url.clone(),
        key: tw_config::Secret::Literal(req.key.clone()),
        // 猜得出来就不写进文件 —— 少一行是一行（§0.6）。
        ..Default::default()
    });
    tw_config::validate(&cfg).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    tw_config::write(&s.config_path, &cfg).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("写配置失败：{e}"),
        )
    })?;

    let gateway_key = cfg
        .clients
        .first()
        .map(|c| c.key.clone())
        .unwrap_or_default();
    Ok(Json(tw_api::SetupResponse {
        gateway_key,
        gateway_addr: s.gateway_addr.clone().unwrap_or_default(),
        config_path: s.config_path.display().to_string(),
    }))
}

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("控制面 socket {path} 起不来：{source}")]
    Bind {
        path: PathBuf,
        source: std::io::Error,
    },
    /// **系统级的硬上限，不是我们的规矩。**`sockaddr_un.sun_path` 在
    /// macOS 上是 104 字节、Linux 上 108 —— 超了 `bind` 会失败，而 libc
    /// 给的原话是「path must be shorter than SUN_LEN」，看不出上限是多少、
    /// 也看不出自己超了多少。
    #[error(
        "控制面 socket 的路径太长：{len} 字节，系统上限是 {max}。\n{path}\n把配置放到一个短一点的目录下（默认的 ~/.thinkwatch 不会有这个问题）。"
    )]
    PathTooLong {
        path: PathBuf,
        len: usize,
        max: usize,
    },
}

/// `sockaddr_un.sun_path` 的容量。macOS 104、Linux 108，取小的那个 ——
/// 差的那 4 个字节不值得为它分平台。
const SUN_PATH_MAX: usize = 104;

/// 路径放得下吗。**在 bind 之前问**，这样错误信息能说清上限和超出量。
pub fn socket_path_fits(path: &Path) -> Result<(), ControlError> {
    // 末尾的 NUL 也占一个字节
    let len = path.as_os_str().as_encoded_bytes().len() + 1;
    if len > SUN_PATH_MAX {
        return Err(ControlError::PathTooLong {
            path: path.to_path_buf(),
            len,
            max: SUN_PATH_MAX,
        });
    }
    Ok(())
}

/// 在 unix socket 上起控制面。
///
/// 陈旧的 socket 文件直接删掉重建 —— 它和 lock 文件不一样，没有「另一个
/// 实例可能还在用」的歧义：单实例锁已经在上一步挡住了。
pub async fn serve_unix(state: ControlState, path: &Path) -> Result<(), ControlError> {
    socket_path_fits(path)?;
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let listener = tokio::net::UnixListener::bind(path).map_err(|source| ControlError::Bind {
        path: path.to_path_buf(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 0700：只有当前用户能连。这就是不需要 token 的原因。
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    tracing::info!(path = %path.display(), "控制面已监听");

    let app = router(state);
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("控制面 accept 失败：{e}");
                continue;
            }
        };
        let svc = app.clone();
        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let svc = hyper::service::service_fn(move |req| {
                use tower::ServiceExt;
                svc.clone().oneshot(req)
            });
            if let Err(e) =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(io, svc)
                    .await
            {
                tracing::debug!("控制面连接结束：{e}");
            }
        });
    }
}

/// 默认 socket 路径。和配置放一起，这样「一个目录装下全部状态」这条成立。
pub fn default_socket_path() -> PathBuf {
    tw_config::default_dir().join("twcore.sock")
}

#[cfg(test)]
mod socket_path_tests {
    use super::*;

    #[test]
    fn a_path_that_does_not_fit_says_the_limit_and_by_how_much() {
        // libc 的原话是「path must be shorter than SUN_LEN」—— 看不出
        // 上限是多少，也看不出自己超了多少。两个数字都得说。
        let long = PathBuf::from("/tmp")
            .join("x".repeat(200))
            .join("twcore.sock");
        let e = socket_path_fits(&long).unwrap_err();
        let m = e.to_string();
        assert!(m.contains("104"), "{m}");
        assert!(
            m.contains(&format!("{}", long.as_os_str().len() + 1)),
            "{m}"
        );
        // 还要说怎么办
        assert!(m.contains(".thinkwatch"), "{m}");
    }

    #[test]
    fn the_default_path_fits_with_room_to_spare() {
        // 这条不是形式主义：如果哪天默认目录变深了，它会立刻响。
        let p = tw_config::default_dir().join("twcore.sock");
        socket_path_fits(&p).unwrap();
    }

    #[test]
    fn the_boundary_is_the_nul_terminator_not_the_byte_count() {
        // 正好 104 字节的路径**放不下** —— 结尾的 NUL 也要占一个。
        // 差这一个字节的话，失败会推迟到 bind，而那时的报错完全不同。
        let base = "/tmp/";
        let exact = PathBuf::from(format!("{base}{}", "a".repeat(104 - base.len())));
        assert_eq!(exact.as_os_str().len(), 104);
        assert!(socket_path_fits(&exact).is_err());
        let one_less = PathBuf::from(format!("{base}{}", "a".repeat(103 - base.len())));
        assert!(socket_path_fits(&one_less).is_ok());
    }
}
