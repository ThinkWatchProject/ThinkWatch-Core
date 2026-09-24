//! 远程控制端口：`listen.control.remote`。
//!
//! # 是另开的一扇门，不是换一扇
//!
//! 本机的通道（socket、Windows 的回环端口）照旧在。远程端口绑不上、写错了
//! 放行名单、把自己挡在外面，服务器上的人照样能从本机的通道改回来；网关也
//! 不受影响。所以这里的任何失败都**只记下来**（`Status.remote_control.error`），
//! 不让 core 退出。
//!
//! # 进门之前的三道筛子
//!
//! 1. **来源**：`allow_from` 之外的地址 accept 之后立刻关掉，一个字节都不回 ——
//!    不向扫描者暴露这是什么服务。和网关不同，**回环不自动放行**：这台机器
//!    上的人有 socket，远程端口是给别的机器的。
//! 2. **节流**：同一个来源一分钟里握手失败 [`MAX_FAILURES`] 次，接下来一分钟
//!    它的连接一律直接关掉。钥匙是 256 位的，猜不出来；这一道挡的是有人拿
//!    连接去耗我们的握手。
//! 3. **并发**：远程连接最多 [`MAX_CONNECTIONS`] 条，多出来的直接关掉。
//!
//! 过了这三道才握手（和本机同一个 `gate`），握上了才是 HTTP。
//!
//! # 远程连接做不了的事
//!
//! 关 core（服务器上它归 systemd 管）、取诊断包（写的是服务器的文件系统）、
//! 改 `listen.control` 这一节（远程连接改得动自己进来的那扇门，改错了就再也
//! 连不上）。**在 core 这边判**，不靠桌面端的白名单：见 [`is_remote`]。

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::StatusCode;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
use tw_types::{Msg, msg};

use crate::gate::Gate;
use crate::{ControlState, Fail, fail};

/// 一分钟里失败几次就先不理它。
pub const MAX_FAILURES: usize = 5;
/// 数失败次数的窗口，也是不理它的时长。
pub const WINDOW: Duration = Duration::from_secs(60);
/// 同时最多几条远程连接。桌面端一台机器用两三条（请求 + 事件流）。
pub const MAX_CONNECTIONS: usize = 32;

tokio::task_local! {
    /// 这条连接是从远程端口进来的。在 `hand_off` 里给整条连接设上，处理函数
    /// 和 `ConfigManager` 里用 [`is_remote`] 问。
    static REMOTE: bool;
}

/// 此刻处理的请求是不是从远程端口进来的。
///
/// **不在任何连接里（测试直接调处理函数、core 自己的后台任务）就是本机。**
/// 远程连接只走 HTTP/1.1（见 `hand_off`），请求在连接自己的任务里处理，
/// 这个标记一定在。
pub fn is_remote() -> bool {
    REMOTE.try_with(|r| *r).unwrap_or(false)
}

/// 在「远程」这个标记下跑 `f`。
pub(crate) async fn as_remote<F: std::future::Future>(f: F) -> F::Output {
    REMOTE.scope(true, f).await
}

/// 远程连接做不了的那几件事，说给人听。
pub(crate) fn refused(what: Refused) -> Fail {
    let m = match what {
        Refused::Shutdown => msg!(
            "control.remote.shutdown_refused" =>
            "The core cannot be stopped over a remote connection. On the server it is managed \
             by the service manager, for example systemctl stop twcore."
        ),
        Refused::Diagnostics => msg!(
            "control.remote.diagnostics_refused" =>
            "The diagnostic bundle is only available on the machine the core runs on."
        ),
    };
    fail(StatusCode::FORBIDDEN, m)
}

pub(crate) enum Refused {
    Shutdown,
    Diagnostics,
}

/// 此刻在听哪儿，上一次换监听为什么没换成。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Listening {
    pub addr: Option<SocketAddr>,
    pub error: Option<Msg>,
}

/// 远程端口的运行时状态。`ControlState` 里一份。
pub struct Remote {
    listening: Mutex<Listening>,
    strikes: Mutex<HashMap<IpAddr, Strikes>>,
    slots: Arc<Semaphore>,
}

impl Default for Remote {
    fn default() -> Self {
        Self {
            listening: Mutex::default(),
            strikes: Mutex::default(),
            slots: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        }
    }
}

#[derive(Default)]
struct Strikes {
    at: Vec<Instant>,
    until: Option<Instant>,
}

impl Remote {
    pub fn listening(&self) -> Listening {
        self.listening.lock().map(|g| g.clone()).unwrap_or_default()
    }

    fn set_listening(&self, next: Listening) {
        if let Ok(mut g) = self.listening.lock() {
            *g = next;
        }
    }

    /// 这个来源此刻被晾着吗。
    fn benched(&self, ip: IpAddr, now: Instant) -> bool {
        self.strikes
            .lock()
            .ok()
            .and_then(|g| g.get(&ip).and_then(|s| s.until))
            .is_some_and(|until| now < until)
    }

    /// 记一次握手失败。满了就晾它一分钟。
    fn strike(&self, ip: IpAddr, now: Instant) {
        let Ok(mut g) = self.strikes.lock() else {
            return;
        };
        // 顺手清掉早就没事了的，表不会一直长
        g.retain(|_, s| {
            s.at.retain(|t| now.duration_since(*t) < WINDOW);
            !s.at.is_empty() || s.until.is_some_and(|u| now < u)
        });
        let s = g.entry(ip).or_default();
        s.at.push(now);
        if s.at.len() >= MAX_FAILURES {
            s.at.clear();
            s.until = Some(now + WINDOW);
            tracing::info!(
                %ip,
                "a remote source failed the handshake {MAX_FAILURES} times within a minute; \
                 its connections are closed for a minute"
            );
        }
    }
}

/// 来源在不在放行名单里。**回环不自动放行**（见模块头）。
fn allowed(entries: &[String], ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    entries.iter().any(|e| match e.parse::<IpAddr>() {
        Ok(a) => a.to_canonical() == ip,
        Err(_) => e.parse::<tw_gateway::Cidr>().is_ok_and(|c| c.contains(ip)),
    })
}

/// 别的机器连 `addr` 该用的地址：回环是空的；`0.0.0.0` / `::` 换成这台机器
/// 每张网卡的地址。
pub fn reachable(addr: SocketAddr) -> Vec<String> {
    let port = addr.port();
    let show = |ip: IpAddr| SocketAddr::new(ip, port).to_string();
    let ip = addr.ip();
    if ip.is_loopback() {
        return Vec::new();
    }
    if ip.is_unspecified() {
        return tw_config::nics::by_name()
            .into_iter()
            .filter(|n| !n.addr.is_loopback())
            .map(|n| show(n.addr))
            .collect();
    }
    vec![show(ip)]
}

/// 一个在听的远程端口。丢掉它（或者 `stop` 发一下）就不再接新连接，**从它
/// 进来的连接也一起断开**：关掉远程端口的意思就是现在谁都不能从远程进来。
struct Running {
    want: SocketAddr,
    _stop: watch::Sender<()>,
}

/// 跟着配置开关远程端口。`serve` 起一次，一直跑。
///
/// 配置每换入一次就对一遍：开了没、地址变没变。**换地址先绑新的**，绑不上
/// 就守着旧的、记下原因 —— 和网关换监听同一个规矩。放行名单不用重绑，每次
/// accept 现读。
pub(crate) async fn follow(state: ControlState, app: Router, gate: Gate) {
    use tokio::sync::broadcast::error::RecvError;
    let mut events = state.bus().subscribe();
    let mut running: Option<Running> = None;
    loop {
        apply(&state, &app, &gate, &mut running).await;
        loop {
            match events.recv().await {
                Ok(tw_api::Event::ConfigReloaded { .. }) | Err(RecvError::Lagged(_)) => break,
                Ok(_) => {}
                Err(RecvError::Closed) => return,
            }
        }
    }
}

async fn apply(state: &ControlState, app: &Router, gate: &Gate, running: &mut Option<Running>) {
    let r = &state.remote;
    let cfg = state.config();
    let want = match &cfg.listen.control.remote {
        Some(x) if x.enabled => x.clone(),
        _ => {
            if running.take().is_some() {
                tracing::info!("the remote control port is closed");
            }
            r.set_listening(Listening::default());
            return;
        }
    };
    let held = r.listening().addr;
    let addr = match want.bind.resolve() {
        Ok(ip) => SocketAddr::new(ip, want.port),
        Err(e) => {
            tracing::warn!(%e, "the remote control port's address cannot be resolved");
            r.set_listening(Listening {
                addr: held,
                error: Some(tw_gateway::listen::unresolved(&e)),
            });
            return;
        }
    };
    if let Some(run) = running.as_ref()
        && run.want == addr
    {
        r.set_listening(Listening {
            addr: held,
            error: None,
        });
        return;
    }
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(%addr, %e, "the remote control port cannot be listened on");
            r.set_listening(Listening {
                addr: held,
                error: Some(tw_gateway::listen::bind_failure(addr, &e)),
            });
            return;
        }
    };
    let actual = listener.local_addr().ok();
    let (stop, stopped) = watch::channel(());
    // 旧的那个丢掉：不再接新连接，从它进来的也断开
    *running = Some(Running {
        want: addr,
        _stop: stop,
    });
    r.set_listening(Listening {
        addr: actual,
        error: None,
    });
    tracing::info!(addr = ?actual, "the remote control port is listening");
    tokio::spawn(accept_loop(
        listener,
        stopped,
        state.clone(),
        app.clone(),
        gate.clone(),
    ));
}

async fn accept_loop(
    listener: TcpListener,
    mut stopped: watch::Receiver<()>,
    state: ControlState,
    app: Router,
    gate: Gate,
) {
    loop {
        let (stream, peer) = tokio::select! {
            _ = stopped.changed() => return,
            r = listener.accept() => match r {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!("the remote control port could not accept a connection: {e}");
                    continue;
                }
            },
        };
        let ip = peer.ip().to_canonical();
        let entries = state
            .config()
            .listen
            .control
            .remote
            .as_ref()
            .map(|r| r.allow_from.clone())
            .unwrap_or_default();
        // 三道筛子都是**直接关掉**：一个字节都不回
        if !allowed(&entries, ip) {
            tracing::debug!(%ip, "a remote connection from outside allow_from was closed");
            continue;
        }
        if state.remote.benched(ip, Instant::now()) {
            tracing::debug!(%ip, "a remote connection from a source being ignored was closed");
            continue;
        }
        let Ok(slot) = state.remote.slots.clone().try_acquire_owned() else {
            tracing::info!(%ip, "too many remote control connections; one was closed");
            continue;
        };
        let remote = state.remote.clone();
        crate::hand_off(
            stream,
            app.clone(),
            gate.clone(),
            Some(crate::RemoteConn {
                on_failure: Box::new(move || remote.strike(ip, Instant::now())),
                closed: stopped.clone(),
                _slot: slot,
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_from_is_a_plain_list_and_loopback_is_not_waved_through() {
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert!(allowed(&l(&["192.168.0.0/16"]), ip("192.168.3.4")));
        assert!(!allowed(&l(&["192.168.0.0/16"]), ip("10.0.0.1")));
        assert!(
            !allowed(&l(&["192.168.0.0/16"]), ip("127.0.0.1")),
            "回环不自动放行"
        );
        assert!(allowed(&l(&["127.0.0.1"]), ip("127.0.0.1")));
        // 绑 `::` 时 IPv4 的来源报成映射地址
        assert!(allowed(&l(&["10.0.0.0/8"]), ip("::ffff:10.1.2.3")));
        assert!(!allowed(&[], ip("10.1.2.3")), "空名单谁都不放");
    }

    #[test]
    fn five_failures_in_a_minute_bench_a_source_for_a_minute() {
        let r = Remote::default();
        let ip: IpAddr = "10.0.0.9".parse().unwrap();
        let other: IpAddr = "10.0.0.8".parse().unwrap();
        let t0 = Instant::now();
        for i in 0..MAX_FAILURES - 1 {
            r.strike(ip, t0 + Duration::from_secs(i as u64));
            assert!(!r.benched(ip, t0 + Duration::from_secs(i as u64)));
        }
        r.strike(ip, t0 + Duration::from_secs(10));
        assert!(r.benched(ip, t0 + Duration::from_secs(11)));
        assert!(
            !r.benched(other, t0 + Duration::from_secs(11)),
            "只晾它自己"
        );
        assert!(
            !r.benched(ip, t0 + Duration::from_secs(71)),
            "一分钟之后放出来"
        );
    }

    #[test]
    fn failures_spread_over_more_than_a_minute_do_not_add_up() {
        let r = Remote::default();
        let ip: IpAddr = "10.0.0.9".parse().unwrap();
        let t0 = Instant::now();
        for i in 0..10u64 {
            r.strike(ip, t0 + Duration::from_secs(i * 20));
        }
        assert!(!r.benched(ip, t0 + Duration::from_secs(181)));
    }

    #[test]
    fn a_loopback_bind_is_not_reachable_and_a_concrete_one_is_itself() {
        assert!(reachable("127.0.0.1:9000".parse().unwrap()).is_empty());
        assert_eq!(
            reachable("192.168.1.5:9000".parse().unwrap()),
            ["192.168.1.5:9000"]
        );
        for a in reachable("0.0.0.0:9000".parse().unwrap()) {
            assert!(a.ends_with(":9000"), "{a}");
            assert!(!a.starts_with("127."), "{a}");
        }
    }
}
