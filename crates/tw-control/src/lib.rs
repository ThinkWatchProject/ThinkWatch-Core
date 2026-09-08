//! 控制面：unix socket 上的 HTTP。
//!
//! **不占 TCP 端口**（DESIGN.md §2.1）。理由不是省端口，是权限：一个
//! `0700` 的 socket 文件天然只有当前用户能连，不需要再发明一套 token。
//! 只有用户显式开启远程访问时才监听 TCP，那时才需要 token。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::State;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::get;
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
}

pub fn router(state: ControlState) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/events", get(events))
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

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("控制面 socket {path} 起不来：{source}")]
    Bind {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// 在 unix socket 上起控制面。
///
/// 陈旧的 socket 文件直接删掉重建 —— 它和 lock 文件不一样，没有「另一个
/// 实例可能还在用」的歧义：单实例锁已经在上一步挡住了。
pub async fn serve_unix(state: ControlState, path: &Path) -> Result<(), ControlError> {
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
