//! 升级不断线：控制面这一边。
//!
//! # 分工
//!
//! 交接的状态机在 `twcore` 里（只有它同时看得见锁、监听、后台任务和交接连接）。这里
//! 只放控制面要用的三样：
//!
//! - `POST /handover/arm` 的处理函数：先挡掉不该交接的（远程来的、安全模式、登录进行中），
//!   过了门交给 `twcore`（[`Handover::arm_requests`]），等它把交接端点开好再答应；
//! - [`Status::handover`](tw_api::Status::handover) / `draining` 报的那两个数；
//! - 本机控制面的监听（[`ControlListener`]）：交出去、停止 accept、从交过来的上面服务。
//!
//! # 为什么交出控制面时旧的要停止 accept
//!
//! 网关的监听两边短暂地同时 accept 没关系（每个连接只会被一方接受，哪边接都一样转发）。
//! 控制面不行：新的桌面端要和新 core 说话，落到旧 core 上看到的是旧版本、旧协议。所以
//! 控制面一交出去，旧的就不再从它上面接新连接；交接没成（新的提交前崩了），再接着接。

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use tokio::sync::{mpsc, oneshot, watch};
use tw_api::HandoverState;
use tw_types::{Msg, msg};

use crate::{ControlState, Fail, fail};

/// 等接替者多久，缺省。
pub const DEFAULT_WAIT: Duration = Duration::from_secs(120);

/// `twcore` 开交接端点最多等多久。开一个 socket 是毫秒级的事；等不到说明出了别的问题，
/// 桌面端退回现行流程
const ARM_REPLY_WITHIN: Duration = Duration::from_secs(10);

/// 一次 arm：交给 `twcore`，它开好交接端点、脱离父进程之后回话。
pub struct Arm {
    pub wait: Duration,
    pub relaunch: Option<tw_api::Relaunch>,
    /// `Err` 时那句话原样回给桌面端
    pub reply: oneshot::Sender<Result<(), Msg>>,
}

/// 交接在控制面上的样子。`ControlState` 里一份。
pub struct Handover {
    phase: watch::Sender<HandoverState>,
    draining: AtomicUsize,
    /// 这个 core 没有数据面（安全模式），没有可交接的
    no_data_plane: AtomicBool,
    arms: Mutex<Option<mpsc::Sender<Arm>>>,
    control: ControlListener,
}

impl Default for Handover {
    fn default() -> Self {
        Self {
            phase: watch::channel(HandoverState::Idle).0,
            draining: AtomicUsize::new(0),
            no_data_plane: AtomicBool::new(false),
            arms: Mutex::new(None),
            control: ControlListener::default(),
        }
    }
}

impl Handover {
    pub fn phase(&self) -> HandoverState {
        *self.phase.borrow()
    }

    pub fn set_phase(&self, p: HandoverState) {
        self.phase.send_replace(p);
    }

    /// 跟着交接走到哪一步（测试和 `twcore` 等某一步用）
    pub fn watch_phase(&self) -> watch::Receiver<HandoverState> {
        self.phase.subscribe()
    }

    /// 排空中的旧 core 手上还有几个请求（[`tw_api::Status::draining`]）
    pub fn draining(&self) -> usize {
        self.draining.load(Ordering::Relaxed)
    }

    pub fn set_draining(&self, n: usize) {
        self.draining.store(n, Ordering::Relaxed);
    }

    /// 这个 core 没有数据面：arm 一律拒绝（`control.handover.safe_mode`）
    pub fn set_no_data_plane(&self, yes: bool) {
        self.no_data_plane.store(yes, Ordering::Relaxed);
    }

    /// `twcore` 接 arm 请求的那一头。**只有一个**：再调一次换掉上一个。没人接时 arm 被拒
    pub fn arm_requests(&self) -> mpsc::Receiver<Arm> {
        let (tx, rx) = mpsc::channel(4);
        if let Ok(mut g) = self.arms.lock() {
            *g = Some(tx);
        }
        rx
    }

    /// 本机控制面的监听
    pub fn control(&self) -> &ControlListener {
        &self.control
    }
}

/// 拒绝 arm 的几种情形，说给人听（码是契约，桌面端据此退回现行流程）。
pub(crate) fn refused_remote() -> Fail {
    fail(
        StatusCode::FORBIDDEN,
        msg!(
            "control.handover.remote" =>
            "A handover cannot be prepared over a remote connection."
        ),
    )
}

pub(crate) fn refused_safe_mode() -> Fail {
    fail(
        StatusCode::CONFLICT,
        msg!(
            "control.handover.safe_mode" =>
            "The gateway is in safe mode and is not forwarding, so there is nothing to hand over."
        ),
    )
}

pub(crate) fn refused_busy() -> Fail {
    fail(
        StatusCode::CONFLICT,
        msg!(
            "control.handover.busy" =>
            "An account sign-in or another handover is in progress; try again once it has finished."
        ),
    )
}

/// arm 之后要开始一次账号登录：不行（回调端口不交接，登录要在接替者上走）。
pub(crate) fn login_refused() -> Fail {
    fail(
        StatusCode::CONFLICT,
        msg!(
            "control.handover.login_refused" =>
            "The gateway is about to be updated; sign in again once the update has finished."
        ),
    )
}

/// 正在交接（已 arm、或者接替者正在接管）：这时候不开始新的账号登录
pub(crate) fn refuses_logins(h: &Handover) -> bool {
    matches!(h.phase(), HandoverState::Armed | HandoverState::TakingOver)
}

/// 交接端点开不了（arm 答应不了）。
pub fn endpoint_failed(detail: impl std::fmt::Display) -> Msg {
    msg!(
        "control.handover.endpoint_failed", detail = detail =>
        "The handover endpoint could not be opened: {detail}"
    )
}

/// `POST /handover/arm`
pub(crate) async fn arm(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::HandoverArm>,
) -> Result<Json<tw_api::HandoverArmed>, Fail> {
    // 远程来的不行：服务器上的 core 归服务管理器管，也没有桌面端会来接替它
    if crate::remote::is_remote() {
        return Err(refused_remote());
    }
    let h = &s.handover;
    if h.no_data_plane.load(Ordering::Relaxed) {
        return Err(refused_safe_mode());
    }
    // **登录进行中不交接**：回调端口（1455/1457）不交接，登录要在这个 core 上走完
    if s.chatgpt.in_progress() || s.zai.in_progress() {
        return Err(refused_busy());
    }
    // 同一时刻至多一个交接（H8）：已经有接替者在接管了
    if h.phase() == HandoverState::TakingOver {
        return Err(refused_busy());
    }
    let wait = req
        .wait_secs
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_WAIT);
    let Some(tx) = h.arms.lock().ok().and_then(|g| g.clone()) else {
        // 没人接（这个进程不跑交接）：和安全模式一样，没有可交接的
        return Err(refused_safe_mode());
    };
    let (reply, answer) = oneshot::channel();
    let sent = tx
        .send(Arm {
            wait,
            relaunch: req.relaunch,
            reply,
        })
        .await;
    if sent.is_err() {
        return Err(refused_safe_mode());
    }
    match tokio::time::timeout(ARM_REPLY_WITHIN, answer).await {
        Ok(Ok(Ok(()))) => {
            tracing::info!(wait_secs = wait.as_secs(), "armed for a handover");
            Ok(Json(tw_api::HandoverArmed {
                version: tw_handover::VERSION,
            }))
        }
        Ok(Ok(Err(m))) => Err(fail(StatusCode::INTERNAL_SERVER_ERROR, m)),
        _ => Err(crate::internal(
            "the handover endpoint did not open in time",
        )),
    }
}

/// 本机控制面的监听：交出去、停止 accept、从交过来的上面服务。
///
/// `serve` 起来时把一个复制出来的句柄放在这里（同一个内核套接字），交接时交出去的是
/// 它的又一个复制；停止 accept 只是 accept 循环不再去接，句柄一直留着 —— 交接没成时
/// 接着接。
pub struct ControlListener {
    accepting: watch::Sender<bool>,
    /// 跳一下：已经连着的本机连接全部断开（见 [`ControlListener::close_connections`]）
    closing: watch::Sender<u64>,
    held: Mutex<Option<tw_handover::Socket>>,
    inherited: Mutex<Option<tw_handover::Socket>>,
}

impl Default for ControlListener {
    fn default() -> Self {
        Self {
            accepting: watch::channel(true).0,
            closing: watch::channel(0).0,
            held: Mutex::new(None),
            inherited: Mutex::new(None),
        }
    }
}

impl ControlListener {
    /// 新 core：控制面在旧 core 交过来的这个上面服务，不自己绑。**在 `serve` 之前调**。
    pub fn inherit(&self, s: tw_handover::Socket) {
        if let Ok(mut g) = self.inherited.lock() {
            *g = Some(s);
        }
    }

    pub(crate) fn take_inherited(&self) -> Option<tw_handover::Socket> {
        self.inherited.lock().ok().and_then(|mut g| g.take())
    }

    pub(crate) fn hold(&self, s: tw_handover::Socket) {
        if let Ok(mut g) = self.held.lock() {
            *g = Some(s);
        }
    }

    /// 旧 core：交出去的那个（复制出来的句柄）。控制面还没起来时是错误
    pub fn export(&self) -> std::io::Result<tw_handover::Socket> {
        let g = self
            .held
            .lock()
            .map_err(|_| std::io::Error::other("the control listener lock is poisoned"))?;
        match g.as_ref() {
            Some(s) => clone_socket(s),
            None => Err(std::io::Error::other("the control plane is not listening")),
        }
    }

    /// 不再从控制面接新连接（已经连着的不动）
    pub fn pause(&self) {
        self.accepting.send_replace(false);
    }

    /// 接着接
    pub fn resume(&self) {
        self.accepting.send_replace(true);
    }

    pub(crate) fn accepting(&self) -> watch::Receiver<bool> {
        self.accepting.subscribe()
    }

    /// 断开已经连着的本机控制面连接（交出控制面时调）。
    ///
    /// **为什么断开而不是只拒绝改配置**：连着的多半是新桌面端在交接之前连上来的（它的事件流、
    /// 请求都落在这个旧 core 上），只拒绝改动的话它照样从这里读状态、收事件，看到的是一个已经
    /// 不再转发新请求的进程；断开之后它按断线重连，连到的就是接替者。命令行的一次 `twcore call`
    /// 碰巧在这时候被断，重来一次就好。交接没成时控制面恢复 accept，断开的那些重连回到这里
    pub fn close_connections(&self) {
        self.closing.send_modify(|n| *n += 1);
    }

    pub(crate) fn closing(&self) -> watch::Receiver<u64> {
        self.closing.subscribe()
    }
}

/// 等「断开」那一下。没有信号（测试）就一直等
pub(crate) async fn closed(rx: &mut Option<watch::Receiver<u64>>) {
    match rx {
        Some(rx) => {
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending().await,
    }
}

/// 再复制一个句柄。
pub fn clone_socket(s: &tw_handover::Socket) -> std::io::Result<tw_handover::Socket> {
    Ok(match s {
        tw_handover::Socket::Tcp(l) => tw_handover::Socket::Tcp(l.try_clone()?),
        #[cfg(unix)]
        tw_handover::Socket::Unix(l) => tw_handover::Socket::Unix(l.try_clone()?),
    })
}

/// 等到可以接（`accepting` 为真）。通道没了就当一直能接。
pub(crate) async fn until_accepting(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|on| *on).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pausing_and_resuming_the_control_plane_is_seen_by_the_accept_loop() {
        let c = ControlListener::default();
        let mut rx = c.accepting();
        // 默认能接
        tokio::time::timeout(Duration::from_secs(1), until_accepting(&mut rx))
            .await
            .expect("默认就该接");
        c.pause();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), until_accepting(&mut rx))
                .await
                .is_err(),
            "停了还在接"
        );
        c.resume();
        tokio::time::timeout(Duration::from_secs(1), until_accepting(&mut rx))
            .await
            .expect("恢复之后该接");
    }

    #[tokio::test]
    async fn arming_over_the_remote_port_is_refused() {
        // 远程连接进来的请求带着「远程」的标记：处理函数第一步就挡掉，不看别的
        let refused =
            crate::remote::as_remote(async { crate::remote::is_remote().then(refused_remote) })
                .await
                .expect("远程连接上该被挡掉");
        assert_eq!(refused.0, StatusCode::FORBIDDEN);
        assert_eq!(refused.1.0.code, "control.handover.remote");
    }

    #[test]
    fn only_a_core_being_handed_over_refuses_sign_ins() {
        let h = Handover::default();
        for (p, refused) in [
            (HandoverState::Idle, false),
            (HandoverState::Armed, true),
            (HandoverState::TakingOver, true),
            // 交接完成、前任在排空的新 core 是主：照常能登
            (HandoverState::Draining, false),
        ] {
            h.set_phase(p);
            assert_eq!(refuses_logins(&h), refused, "{p:?}");
        }
    }

    #[tokio::test]
    async fn closing_wakes_the_connections_and_no_signal_waits_forever() {
        let c = ControlListener::default();
        let mut rx = Some(c.closing());
        c.close_connections();
        tokio::time::timeout(Duration::from_secs(1), closed(&mut rx))
            .await
            .expect("断开的信号没到");
        let mut none = None;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), closed(&mut none))
                .await
                .is_err()
        );
    }

    #[test]
    fn exporting_before_listening_is_an_error_not_a_panic() {
        assert!(ControlListener::default().export().is_err());
    }

    #[test]
    fn an_exported_socket_is_the_same_listener() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let c = ControlListener::default();
        c.hold(tw_handover::Socket::Tcp(l));
        #[allow(clippy::infallible_destructuring_match)]
        let dup = match c.export().unwrap() {
            tw_handover::Socket::Tcp(l) => l,
            #[cfg(unix)]
            tw_handover::Socket::Unix(_) => panic!("交出去的不是 TCP"),
        };
        assert_eq!(dup.local_addr().unwrap(), addr, "复制出来的是同一个套接字");
    }
}
