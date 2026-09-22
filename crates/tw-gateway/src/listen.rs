//! 在哪儿听，以及换地方听。
//!
//! # 换监听为什么要「新的先起来」
//!
//! 改端口、换网卡时，旧的监听器停止接新连接，等在跑的请求自然结束 —— 这段
//! 时间可能是几分钟（一个长的流）。等它结束才去绑新的话，这几分钟里两边都
//! 不接新连接：用户刚按下保存，所有客户端就连不上了。所以先绑新的，绑上了
//! 再让旧的退场；**绑不上就什么都不动**，旧的照常服务，并且说清为什么。
//!
//! # 为什么要记下「此刻在听哪儿」
//!
//! 界面上的地址以前是启动时记的一次。换了端口，网关已经在新端口上服务，
//! 界面还写着旧的 —— 用户分不清是没生效还是没刷新。

use std::net::SocketAddr;

use tokio::net::TcpListener;
use tw_types::{Msg, msg};

use crate::server::{AppState, now_ms, router};

/// 此刻在听的地址，和上一次换监听为什么没换成。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Listening {
    /// 真的绑上了的地址，配置里写的那个排在最后（前面是顺带开着的回环）。
    /// **空 = 数据面没在听**：安全模式，或者还没起来
    pub addrs: Vec<SocketAddr>,
    /// 配置里的地址没能换上时的原因。**这时旧的还在听**，`addrs` 说的是旧的
    pub error: Option<Msg>,
}

impl Listening {
    /// 配置里写的那个地址。界面上显示它，不显示顺带开着的回环
    pub fn primary(&self) -> Option<SocketAddr> {
        self.addrs.last().copied()
    }
}

/// 绑不上的原因，说给人听。**端口被占是最常见的那一种**，单独一个码。
pub fn bind_failure(addr: SocketAddr, e: &std::io::Error) -> Msg {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::AddrInUse => msg!(
            "gw.listen.port_taken", addr = addr =>
            "{addr} is already in use by another program."
        ),
        ErrorKind::AddrNotAvailable => msg!(
            "gw.listen.addr_unavailable", addr = addr =>
            "{addr} is not an address of this machine right now."
        ),
        ErrorKind::PermissionDenied => msg!(
            "gw.listen.denied", addr = addr =>
            "The system does not allow listening on {addr}. Ports below 1024 need administrator rights."
        ),
        _ => msg!(
            "gw.listen.bind_failed", addr = addr, detail = e =>
            "{addr} could not be listened on: {detail}"
        ),
    }
}

/// 网卡解析不出地址的原因，说给人听。**两种情况要做的事不同**：名字不对要
/// 改设置，网卡没地址要去插网线、连 Wi-Fi。
pub fn unresolved(e: &tw_config::BindError) -> Msg {
    match e {
        tw_config::BindError::NoSuchNic { name, available } => msg!(
            "gw.listen.no_such_nic", name = name, available = available.join(", ") =>
            "This machine has no interface named {name}; it has {available}."
        ),
        tw_config::BindError::NicHasNoAddr { name } => msg!(
            "gw.listen.nic_no_addr", name = name =>
            "Interface {name} currently has no address. Check the cable or the Wi-Fi connection."
        ),
    }
}

/// 这几个地址现在绑不绑得上。**只试还没在听的那几个** —— 已经在听的会原样
/// 留下，拿它们去试只会撞上我们自己。试完就放掉。
///
/// 控制面在写配置之前问这一句：绑不上的配置不该落盘，否则网关守着旧地址，
/// 配置文件却说着新地址，两边从那一刻起各说各的。
pub async fn check(state: &AppState, want: &[SocketAddr]) -> Result<(), Msg> {
    let held = state.listening().addrs;
    let fresh: Vec<SocketAddr> = want.iter().filter(|a| !held.contains(a)).copied().collect();
    bind_all(&fresh)
        .await
        .map(drop)
        .map_err(|(a, e)| bind_failure(a, &e))
}

/// 一个地址上的服务。
struct Bound {
    /// 配置算出来的地址。比对「变没变」用它 —— 端口写 0 时实际拿到的每次都不一样
    want: SocketAddr,
    /// 真的绑上的
    actual: SocketAddr,
    /// 发一下（或者丢掉）就停止接新连接，已经在跑的请求自己跑完
    stop: tokio::sync::oneshot::Sender<()>,
}

/// 绑这几个地址。**一个绑不上就全部作废** —— 半套监听器比一套都没换更难说清。
async fn bind_all(
    want: &[SocketAddr],
) -> Result<Vec<(SocketAddr, TcpListener)>, (SocketAddr, std::io::Error)> {
    let mut out = Vec::with_capacity(want.len());
    for &a in want {
        match TcpListener::bind(a).await {
            Ok(l) => out.push((a, l)),
            Err(e) => return Err((a, e)),
        }
    }
    Ok(out)
}

fn start(state: &AppState, want: SocketAddr, listener: TcpListener) -> std::io::Result<Bound> {
    let actual = listener.local_addr()?;
    if state.runtime().allow.is_empty() {
        tracing::info!(%actual, "the gateway is listening");
    } else {
        tracing::info!(%actual, "the gateway is listening (source allow-list in effect)");
    }
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let st = state.clone();
    tokio::spawn(async move {
        // `into_make_service_with_connect_info` 是拿到对端地址的唯一办法 ——
        // 少了它，来源白名单收到的永远是 unwrap 出来的默认值。
        let r = axum::serve(
            listener,
            router(st).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = stopped.await;
        })
        .await;
        match r {
            Ok(()) => tracing::info!(%actual, "stopped listening"),
            Err(e) => tracing::error!(%actual, %e, "the listener ended"),
        }
    });
    Ok(Bound { want, actual, stop })
}

fn snapshot(bound: &[Bound], error: Option<Msg>) -> Listening {
    Listening {
        addrs: bound.iter().map(|b| b.actual).collect(),
        error,
    }
}

/// 起服务。`follow` 为真时**跟着配置里的监听地址走**（「温」那一级热重载）。
///
/// 命令行给了 `--port` 时 `follow` 为假：那是一个显式的覆盖，不该被配置
/// 文件推翻。
///
/// 起始那一次绑不上是致命的，错误带着地址交给调用方 —— 那时还没有旧的
/// 可以守，守护进程据此重启或进安全模式。之后的每一次换监听都不会让它返回。
pub async fn serve_at(state: AppState, want: Vec<SocketAddr>, follow: bool) -> std::io::Result<()> {
    let mut bound = Vec::with_capacity(want.len());
    for (a, l) in bind_all(&want)
        .await
        .map_err(|(a, e)| std::io::Error::new(e.kind(), format!("{}", bind_failure(a, &e))))?
    {
        bound.push(start(&state, a, l)?);
    }
    state.set_listening(snapshot(&bound, None));
    if !follow {
        // 监听器各自在自己的任务里跑。这个 future 活着，它们就不收到停止信号
        std::future::pending::<()>().await;
    }
    loop {
        state.relisten_signal().notified().await;
        let want = match state.runtime().config.listen.gateway.addrs() {
            Ok(a) => a,
            // **新配置的地址算不出来就守住旧的。**`bind` 指着一张刚被拔掉
            // 的网卡时，正确的动作不是把一个正在工作的监听器拆掉 —— 那会
            // 让所有客户端立刻断线，而它们本来好好的。
            Err(e) => {
                tracing::error!(%e, "the new listen address cannot be resolved; keeping the current one");
                state.set_listening(snapshot(&bound, Some(unresolved(&e))));
                continue;
            }
        };
        let have: Vec<SocketAddr> = bound.iter().map(|b| b.want).collect();
        if want == have {
            // 通知来了但地址没变（比如又改回去了）。上一次没换成的那句话到此作废
            state.set_listening(snapshot(&bound, None));
            continue;
        }
        // **还要的地址原样留着，只绑新增的** —— 从「仅本机」换到「局域网」时，
        // 回环那一个一直在听，正在上面跑的请求感觉不到任何变化
        let fresh: Vec<SocketAddr> = want.iter().filter(|a| !have.contains(a)).copied().collect();
        let listeners = match bind_all(&fresh).await {
            Ok(l) => l,
            Err((a, e)) => {
                tracing::error!(%a, %e, "the new listen address cannot be bound; keeping the current one");
                state.set_listening(snapshot(&bound, Some(bind_failure(a, &e))));
                continue;
            }
        };
        // 新的都绑上了，旧的才退场：停止接新连接，在跑的请求自己跑完
        let (keep, gone): (Vec<Bound>, Vec<Bound>) =
            bound.into_iter().partition(|b| want.contains(&b.want));
        for b in gone {
            tracing::info!(addr = %b.actual, "no longer wanted; draining");
            let _ = b.stop.send(());
        }
        bound = keep;
        for (a, l) in listeners {
            bound.push(start(&state, a, l)?);
        }
        // 配置里写的那个排在最后，和 `addrs()` 同一个顺序
        bound.sort_by_key(|b| want.iter().position(|w| *w == b.want));
        state.set_listening(snapshot(&bound, None));
    }
}

impl AppState {
    /// 此刻在听的地址，和上一次换监听为什么没换成。
    pub fn listening(&self) -> Listening {
        self.listening.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// 记下新的监听状态，**变了才报**。
    fn set_listening(&self, next: Listening) {
        let changed = match self.listening.lock() {
            Ok(mut g) if *g != next => {
                *g = next.clone();
                true
            }
            _ => false,
        };
        if changed {
            self.bus.emit(tw_api::Event::ListenChanged {
                id: self.bus.next_id(),
                addr: next.primary().map(|a| a.to_string()),
                error: next.error,
                at_ms: now_ms(),
            });
        }
    }

    /// 叫监听那一边照着当前配置再对一次。
    ///
    /// 配置换入时自己会叫（地址变了的话）；控制面保存监听设置之后也叫一次
    /// —— 上一次没换成、这次又原样保存的时候，配置没变，但该再试一次。
    pub fn relisten(&self) {
        // `notify_one` 而不是 `notify_waiters`：监听那一边正忙着换监听器时
        // 来的通知要留着，否则改了两次只生效一次
        self.relisten_signal().notify_one();
    }
}
