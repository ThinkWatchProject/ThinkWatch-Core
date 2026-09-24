//! 控制面的门：每条连接先握手（`tw-link`），握上了才交给 HTTP。
//!
//! # 为什么 socket 之外还要一道门
//!
//! unix socket 那一套（`0700` 的文件，只有属主连得上）是文件系统给的保证，
//! **只在这个平台上成立**。Windows 上控制面只能落在回环 TCP 上，本机任意进程
//! 都连得上；远程端口更不用说。所以门是另外装的一道，**每一种通道装的是
//! 同一道**：只有一条鉴权代码路径，桌面版每天都在跑它。
//!
//! # 钥匙换了
//!
//! 钥匙是每次握手现取的（配置里生效的那一把），所以换钥匙之后，新连接自然
//! 按新的算。**已经连着的也要断开**：用旧钥匙进来的连接不该在换锁之后还
//! 一直开着 —— 换钥匙往往就是因为旧的那把不可信了。本机的桌面端断开之后
//! 重读一次配置就接上了。

use tokio::sync::watch;
use tw_api::control::ControlKey;
use tw_link::{Acceptor, LinkError};

use crate::ControlState;

/// 握手用的那一侧，外加「钥匙现在是哪一把」。克隆很便宜，每条连接一份。
#[derive(Clone)]
pub(crate) struct Gate {
    acceptor: Acceptor,
    current: watch::Receiver<Option<ControlKey>>,
}

impl Gate {
    /// **要在 tokio 里调**：它起一个跟着配置重载更新钥匙的后台任务。
    pub(crate) fn new(state: &ControlState) -> Self {
        let gw = state.gateway.clone();
        let acceptor = Acceptor::new(
            move || gw.config().listen.control.key(),
            env!("CARGO_PKG_VERSION"),
        );
        let (tx, rx) = watch::channel(state.config().listen.control.key());
        let mut events = state.bus().subscribe();
        let gw = state.gateway.clone();
        tokio::spawn(async move {
            use tokio::sync::broadcast::error::RecvError;
            loop {
                match events.recv().await {
                    // 丢了事件就不知道错过了什么：照样对一遍
                    Ok(tw_api::Event::ConfigReloaded { .. }) | Err(RecvError::Lagged(_)) => {
                        let now = gw.config().listen.control.key();
                        tx.send_if_modified(|k| {
                            if *k == now {
                                return false;
                            }
                            *k = now;
                            true
                        });
                    }
                    Ok(_) => {}
                    Err(RecvError::Closed) => return,
                }
            }
        });
        Self {
            acceptor,
            current: rx,
        }
    }

    /// 握手。失败时该回的都回过了，这里只记一行，**不记钥匙**。
    pub(crate) async fn admit<S>(&self, stream: S) -> Result<tw_link::Accepted<S>, LinkError>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let r = self.acceptor.accept(stream).await;
        match &r {
            Ok(a) => {
                tracing::debug!(app = %a.hello.app, "a control-plane client connected");
            }
            Err(e @ (LinkError::WrongKey | LinkError::VersionMismatch { .. })) => {
                // 桌面端和 core 版本不一致、拿着旧钥匙的，都是用户看得到的状态，
                // 值得在日志里留一行
                tracing::info!("a control-plane connection was turned away: {e}");
            }
            Err(e) => {
                tracing::debug!("a control-plane handshake did not finish: {e}");
            }
        }
        r
    }

    /// 等到钥匙换成了别的（不再是 `used`）。
    pub(crate) async fn key_changed_from(&self, used: &ControlKey) {
        let mut rx = self.current.clone();
        loop {
            if rx.borrow_and_update().as_ref() != Some(used) {
                return;
            }
            if rx.changed().await.is_err() {
                // 跟着配置的那个任务没了（core 在退出）：不会再换了
                std::future::pending::<()>().await;
            }
        }
    }
}
