//! 升级不断线：新旧 core 交接的状态机（方案第 4、6、7 节）。
//!
//! 两个角色，同一个二进制：
//!
//! - **旧 core**（[`serve_arms`]）：桌面端 `POST /handover/arm` 之后脱离父进程、开交接端点、
//!   等接替者；接替者连上来就把控制面、快照、监听交过去，提交之后停止 accept、停掉后台
//!   事务、放弃锁，做完手上的请求再退出。
//! - **新 core**（[`approach`] → [`Incoming`]）：取锁时发现持有者活着，就去连它的交接端点；
//!   连上了就在交过来的控制面上服务、读配置建运行时、要监听、提交，成为锁的持有者。
//!
//! **提交是唯一的不可逆点。**提交之前任何一步出错，旧 core 回到待交接、照常服务（它始终
//! 握着全部监听）；提交之后新 core 是唯一的「主」，旧 core 只做完手上的请求。任何情况下
//! 都不出现两个进程都不监听的时刻。
//!
//! # 时限
//!
//! | 时限 | 多久 | 谁在等 |
//! | --- | --- | --- |
//! | 待交接 | arm 给的 `wait_secs`，缺省 120 秒 | 旧 core 等接替者连上来（H7） |
//! | 接管 | 30 秒（[`TAKEOVER_LIMIT`]） | 旧 core 交出监听之后等提交（F4） |
//! | 排空 | 30 分钟（[`DRAIN_LIMIT`]） | 旧 core 提交之后等手上的请求（H6） |
//! | 退回 | 180 秒（[`FALLBACK_DRAIN`]） | 版本不同时旧 core 等请求结束再退（H2） |
//!
//! **接管时限从交出监听算起，不从 hello 算起**（和方案 9.4 的写法不同）：新 core 配置读
//! 不进来时（H3），它只接管控制面、等用户把配置修好，这可以是几分钟、几小时 —— 这段
//! 时间旧 core 照常转发，没有理由让交接失败。这一段的失败（新 core 崩了）靠交接连接断开
//! 发现。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::watch;
use tokio::time::Instant;
use tw_api::HandoverState;
use tw_handover::{Link, ListenerInfo, Message, Role, Socket};

use crate::background::{self, Background};
use crate::lockfile::LockFile;

/// 旧 core 交出监听之后等提交多久（F4）。
pub const TAKEOVER_LIMIT: Duration = Duration::from_secs(30);
/// 新 core 收到监听之后，自己最晚什么时候提交。**比旧 core 等的短一截**：旧 core 等到点
/// 放弃、新 core 同一刻发出提交的话，两边都会以为自己是主
const COMMIT_WITHIN: Duration = Duration::from_secs(20);
/// 旧 core 提交之后排空多久（H6）。
pub const DRAIN_LIMIT: Duration = Duration::from_secs(30 * 60);
/// 退回现行流程时旧 core 等请求结束多久（H2；和桌面端现行的等待一样长）。
pub const FALLBACK_DRAIN: Duration = Duration::from_secs(180);
/// 连上之后对方多久不说话就算了：新 core 等旧 core 的第一句、旧 core 等 `Hello`。
const HELLO_WITHIN: Duration = Duration::from_secs(10);
/// 连着几次交接在交出监听之后失败（F4、F5），下一个接替者就请它退回现行流程（F5）。
pub const MAX_STRIKES: u32 = 3;
/// 新 core 请旧 core 退回之后，等它退出多久（[`FALLBACK_DRAIN`] 加上退出的余量）。
const FALLBACK_EXIT_WITHIN: Duration = Duration::from_secs(200);

/// 接替者的请求号从旧 core 的号往后跳这么多。
///
/// **两个进程同时在发请求号**：旧 core 排空期间还在记自己的请求，新 core 的记录要等它退出
/// 才进库（见 `record`），库写的是 `INSERT OR REPLACE`，号撞了就是覆盖。快照里带着旧 core
/// 此刻的号，新 core 从它往后跳一大截：旧 core 从快照到退出用掉的号（交接前的重叠、排空
/// 期间的事件）远到不了这么多。
pub const ID_GAP: u64 = 10_000_000;

// ─────────────────────────────────────────────────────────── 快照外面那层

/// 交接连接上的快照：网关的那份（格式归 `tw_gateway::handover`）外面包一层 core 自己的。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct Snapshot {
    gateway: serde_json::Value,
    /// 旧 core 此刻发到了哪个请求号（见 [`ID_GAP`]）
    next_id: u64,
}

fn wrap(gateway: serde_json::Value, next_id: u64) -> serde_json::Value {
    serde_json::to_value(Snapshot { gateway, next_id }).unwrap_or(serde_json::Value::Null)
}

/// 拆开。**认不出就当没有**：版本号相同时不该发生，发生了也只是少接一样状态
fn unwrap(v: serde_json::Value) -> Option<Snapshot> {
    serde_json::from_value(v).ok()
}

// ─────────────────────────────────────────────────────────── 待交接的时钟（纯逻辑）

/// 待交接时等不到接替者怎么办（H7）。
#[derive(Debug, Clone, PartialEq)]
pub struct ArmClock {
    pub deadline: Instant,
    wait: Duration,
    can_relaunch: bool,
    relaunched: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    /// 重新打开桌面端一次，再等一个周期
    Relaunch,
    /// 不等了：按现行方式排空后退出
    GiveUp,
}

impl ArmClock {
    pub fn new(now: Instant, wait: Duration, can_relaunch: bool) -> Self {
        Self {
            deadline: now + wait,
            wait,
            can_relaunch,
            relaunched: false,
        }
    }

    /// 到点了。**只重开一次**
    pub fn expired(&mut self, now: Instant) -> Expiry {
        if self.can_relaunch && !self.relaunched {
            self.relaunched = true;
            self.deadline = now + self.wait;
            Expiry::Relaunch
        } else {
            Expiry::GiveUp
        }
    }

    /// 一次交接没成、回到待交接：重新等一个周期（重开过的不再重开）
    pub fn restart(&mut self, now: Instant) {
        self.deadline = now + self.wait;
    }
}

// ─────────────────────────────────────────────────────────── 排空判定（纯逻辑）

/// 排空期什么时候算做完：网关的在途计数归零、走 HTTP 的连接都结束，**连着几拍都是这样**
/// （WebSocket 以 1012 关掉的那一帧、最后一条结算要有工夫发出去）。
#[derive(Debug, Default)]
pub struct Drained {
    calm: u32,
    last: Option<usize>,
}

/// 连着几拍（每拍 [`DRAIN_TICK`]）
const CALM_TICKS: u32 = 3;
const DRAIN_TICK: Duration = Duration::from_millis(200);

impl Drained {
    /// 这一拍看到的在途数和还在服务的连接数。返回（要不要报一条新的在途数，做完没有）
    pub fn tick(&mut self, in_flight: usize, serving: usize) -> (Option<usize>, bool) {
        let report = (self.last != Some(in_flight)).then_some(in_flight);
        self.last = Some(in_flight);
        if in_flight == 0 && serving == 0 {
            self.calm += 1;
        } else {
            self.calm = 0;
        }
        (report, self.calm >= CALM_TICKS)
    }
}

// ─────────────────────────────────────────────────────────── 旧 core

/// 进程为什么该退了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// 交给了接替者、手上的做完了。后台事务早已停下，**不再同步、不碰配置**
    HandedOver,
    /// 没交出去（等不到接替者、版本不同）：按现行方式排空后退出，这时还是主
    Retired,
}

/// 两个角色都要碰的那些东西。`main` 建一份，交接的任务拿一份。
pub struct Core {
    pub dir: PathBuf,
    pub state: tw_gateway::AppState,
    pub control: tw_control::ControlState,
    pub gateway: tw_gateway::listen::Handoff,
    pub background: Arc<Mutex<Background>>,
    pub sync: Mutex<Option<Arc<tw_control::sync::Sync>>>,
    /// 配置文件、插件目录的监听。**丢掉就不再盯**：交接提交之后旧 core 不再读配置
    pub watchers: Mutex<Vec<Box<dyn Send>>>,
    pub lock: Mutex<Option<LockFile>>,
    /// `--parent` 守望。arm 之后关掉：桌面端要重启，core 不能跟着退
    pub parent: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// 请求库打开了没有。接替者要等它的前任退出才打开（见 `record::open_later`）；
    /// **它自己排空完也要等这个**才说 `Bye`（H8：库只在最老的那个退出之后才由下一个打开）
    pub store_opened: watch::Receiver<bool>,
    /// 交了出去
    pub handed_over: AtomicBool,
}

impl Core {
    fn handover(&self) -> &tw_control::handover::Handover {
        &self.control.handover
    }

    /// 同步服务（这个进程是接替者时，提交之后才有）
    fn sync(&self) -> Option<Arc<tw_control::sync::Sync>> {
        self.sync.lock().ok().and_then(|g| g.clone())
    }
}

/// 跑旧 core 那一半：接 arm、等接替者、交接、排空。返回的接收端在该退出时收到 [`Exit`]。
pub fn serve_arms(core: Arc<Core>) -> tokio::sync::oneshot::Receiver<Exit> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut arms = core.handover().arm_requests();
    tokio::spawn(async move {
        let exit = run_arms(&core, &mut arms).await;
        let _ = tx.send(exit);
    });
    rx
}

struct Params {
    wait: Duration,
    relaunch: Option<tw_api::Relaunch>,
}

async fn run_arms(
    core: &Arc<Core>,
    arms: &mut tokio::sync::mpsc::Receiver<tw_control::handover::Arm>,
) -> Exit {
    // 第一次 arm：开端点、脱离父进程
    let (mut endpoint, mut params) = loop {
        let Some(a) = arms.recv().await else {
            return std::future::pending().await;
        };
        match tw_handover::Endpoint::bind(&core.dir).await {
            Ok(ep) => {
                // **先把标准输出、错误输出从桌面端的管道上挪开**：桌面端马上要退出，那之后再
                // 往管道里写会出错，`eprintln!` 和日志的出错处理会因此 panic，排空中的旧 core
                // 就挂了
                stdio_to_file(&core.dir);
                if let Some(h) = core.parent.lock().ok().and_then(|mut g| g.take()) {
                    h.abort();
                    tracing::info!("armed for a handover; no longer exiting with the parent");
                }
                core.handover().set_phase(HandoverState::Armed);
                let _ = a.reply.send(Ok(()));
                break (
                    Some(ep),
                    Params {
                        wait: a.wait,
                        relaunch: a.relaunch,
                    },
                );
            }
            Err(e) => {
                tracing::warn!("the handover endpoint could not be opened: {e}");
                let _ = a.reply.send(Err(tw_control::handover::endpoint_failed(&e)));
            }
        }
    };
    let mut clock = ArmClock::new(Instant::now(), params.wait, params.relaunch.is_some());
    let mut strikes = 0u32;
    loop {
        if endpoint.is_none() {
            match tw_handover::Endpoint::bind(&core.dir).await {
                Ok(ep) => endpoint = Some(ep),
                // 开不回来：等不到接替者，到点照 H7 收场
                Err(e) => tracing::warn!("the handover endpoint could not be reopened: {e}"),
            }
        }
        let accepted = async {
            match endpoint.as_ref() {
                Some(ep) => ep.accept().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            r = accepted => {
                let link = match r {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!("the handover endpoint failed: {e}");
                        endpoint = None;
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };
                // **接到一个就关掉端点**（H8）：同一时刻至多一个接替者；再来的连不上，按「已有
                // 实例在运行」退避重试
                if let Some(ep) = endpoint.take() {
                    ep.close();
                }
                match give(core, link, strikes >= MAX_STRIKES).await {
                    Given::Committed(link) => {
                        drain(core, link).await;
                        return Exit::HandedOver;
                    }
                    Given::Fallback => {
                        tracing::info!("the successor cannot take over; draining and exiting the current way");
                        retire(core, FALLBACK_DRAIN).await;
                        return Exit::Retired;
                    }
                    Given::Abandoned { strike } => {
                        if strike {
                            strikes += 1;
                        }
                        // 回到待交接：控制面接着接，重新等一个周期（F4）
                        core.handover().control().resume();
                        core.handover().set_phase(HandoverState::Armed);
                        clock.restart(Instant::now());
                        tracing::info!(strikes, "the handover did not complete; serving on and waiting for the next successor");
                    }
                    Given::NotASuccessor => {}
                }
            }
            _ = tokio::time::sleep_until(clock.deadline) => match clock.expired(Instant::now()) {
                Expiry::Relaunch => {
                    if let Some(r) = &params.relaunch {
                        relaunch(r);
                    }
                }
                Expiry::GiveUp => {
                    // H7：不留孤儿进程
                    tracing::info!("no successor arrived; draining and exiting the current way");
                    if let Some(ep) = endpoint.take() {
                        ep.close();
                    }
                    core.handover().set_phase(HandoverState::Idle);
                    retire(core, FALLBACK_DRAIN).await;
                    return Exit::Retired;
                }
            },
            Some(a) = arms.recv() => {
                // 又 arm 了一次（桌面端重试）：按新的参数重新等
                params = Params { wait: a.wait, relaunch: a.relaunch };
                clock = ArmClock::new(Instant::now(), params.wait, params.relaunch.is_some());
                let _ = a.reply.send(Ok(()));
            }
        }
    }
}

enum Given {
    /// 提交了：从此只做完手上的请求
    Committed(Link),
    /// 接替者请我们按现行方式排空后退出（H2）
    Fallback,
    /// 没成，回到待交接。`strike`：在交出监听之后失败的（F4、F5），记一次
    Abandoned { strike: bool },
    /// 连上来的不说 `Hello`（不是接替者，或者它已经放弃了）
    NotASuccessor,
}

/// 把控制面、快照、监听交给接替者，等它提交。
async fn give(core: &Core, mut link: Link, refuse: bool) -> Given {
    let hello = tokio::time::timeout(HELLO_WITHIN, link.recv()).await;
    let (version, peer) = match hello {
        Ok(Ok((
            Message::Hello {
                version,
                pid,
                core_version,
            },
            _,
        ))) => {
            tracing::info!(pid, %core_version, "a successor connected");
            // Windows 上 peer_pid 要等收到 Hello 才有（就是它报的那个）
            (version, link.peer_pid().unwrap_or(pid))
        }
        _ => return Given::NotASuccessor,
    };
    core.handover().set_phase(HandoverState::TakingOver);
    if version != tw_handover::VERSION || refuse {
        // H2 / F5：退回现行流程。等它说一声 Fallback（它会马上说）
        let reason = if refuse {
            format!("{MAX_STRIKES} handovers in a row failed after the listeners were handed over")
        } else {
            format!(
                "handover version {} here, {version} on the successor",
                tw_handover::VERSION
            )
        };
        tracing::info!(%reason, "refusing the handover");
        let _ = link.send(&Message::Refused { reason }).await;
        return match tokio::time::timeout(HELLO_WITHIN, link.recv()).await {
            Ok(Ok((Message::Fallback, _))) => Given::Fallback,
            _ => Given::Abandoned { strike: false },
        };
    }

    // ① 控制面。**交出去就不再从它上面接新连接**（见 `tw_control::handover`）
    let sock = match core.handover().control().export() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("the control plane listener could not be handed over: {e}");
            return Given::Abandoned { strike: true };
        }
    };
    core.handover().control().pause();
    let control = Message::Control {
        info: ListenerInfo {
            role: Role::Control,
            want: None,
        },
    };
    if let Err(e) = link.send_with(&control, &[&sock], peer).await {
        tracing::warn!("the control plane listener could not be handed over: {e}");
        return Given::Abandoned { strike: true };
    }
    drop(sock);

    // ② 快照。之前先把手上的收尾：正在写回的 refresh token、还没上传的本机改动
    background::settle_rotation(&core.background).await;
    if let Some(sync) = core.sync() {
        sync.flush().await;
    }
    let snapshot = wrap(core.state.handover_export(), core.state.bus.next_id());
    if let Err(e) = link.send(&Message::Snapshot { data: snapshot }).await {
        tracing::warn!("the snapshot could not be sent to the successor: {e}");
        return Given::Abandoned { strike: false };
    }

    // ③ 等它要监听。**不限时**（H3：它的配置读不进来时在等人修），断了就是没成
    loop {
        match link.recv().await {
            Ok((Message::WantListeners, _)) => break,
            Ok((Message::Fallback, _)) => return Given::Fallback,
            Ok((other, _)) => {
                tracing::warn!(?other, "an unexpected handover message; ignoring it")
            }
            Err(e) => {
                tracing::info!("the successor left before taking the listeners: {e}");
                return Given::Abandoned { strike: false };
            }
        }
    }

    // ④ 监听：网关的每一个、远程控制端口
    let mut infos = Vec::new();
    let mut socks = Vec::new();
    match core.gateway.export() {
        Ok(gw) => {
            for (want, l) in gw {
                infos.push(ListenerInfo {
                    role: Role::Gateway,
                    want: Some(want),
                });
                socks.push(Socket::Tcp(l));
            }
        }
        Err(e) => {
            tracing::warn!("the gateway listeners could not be handed over: {e}");
            return Given::Abandoned { strike: true };
        }
    }
    match core.control.remote.export() {
        Ok(Some((want, l))) => {
            infos.push(ListenerInfo {
                role: Role::Remote,
                want: Some(want),
            });
            socks.push(Socket::Tcp(l));
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!("the remote control port could not be handed over: {e}");
            return Given::Abandoned { strike: true };
        }
    }
    let refs: Vec<&Socket> = socks.iter().collect();
    if let Err(e) = link
        .send_with(&Message::Listeners { infos }, &refs, peer)
        .await
    {
        tracing::warn!("the listeners could not be handed over: {e}");
        return Given::Abandoned { strike: true };
    }
    drop(refs);
    drop(socks);

    // ⑤ 等提交（F4：30 秒）。提交之前两边同时 accept 是允许的（H4）
    match tokio::time::timeout(TAKEOVER_LIMIT, link.recv()).await {
        Ok(Ok((Message::Commit, _))) => Given::Committed(link),
        Ok(Ok((Message::Fallback, _))) => Given::Fallback,
        Ok(Ok((other, _))) => {
            tracing::warn!(?other, "expected a commit from the successor");
            Given::Abandoned { strike: true }
        }
        Ok(Err(e)) => {
            tracing::info!("the successor left before committing: {e}");
            Given::Abandoned { strike: true }
        }
        Err(_) => {
            tracing::warn!("the successor did not commit within {TAKEOVER_LIMIT:?}");
            Given::Abandoned { strike: true }
        }
    }
}

/// 提交之后：停止 accept、停掉后台事务、放弃锁，做完手上的请求，说 `Bye`。
async fn drain(core: &Core, mut link: Link) {
    tracing::info!("committed; the successor takes new requests from now on");
    core.handed_over.store(true, Ordering::SeqCst);
    // 停止 accept：**只丢自己的句柄**，内核队列里没被接受的连接由接替者取走
    core.gateway.stop_accepting();
    // 远程控制端口：从它进来的连接现在断开，远端按断线重连连到接替者
    core.control.remote.stop_accepting();
    let mut settled = core.state.begin_drain();
    background::settle_rotation(&core.background).await;
    if let Ok(mut bg) = core.background.lock() {
        bg.stop();
    }
    // 不再读配置：新版本的配置格式可能变了
    if let Ok(mut w) = core.watchers.lock() {
        w.clear();
    }
    // 锁归接替者：退出时不删文件
    if let Some(l) = core.lock.lock().ok().and_then(|mut g| g.take()) {
        l.disown();
    }

    let deadline = Instant::now() + DRAIN_LIMIT;
    let mut drained = Drained::default();
    let mut tick = tokio::time::interval(DRAIN_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            Some(s) = settled.recv() => {
                // 接替者没了（F7）也照样排空，发不出去就算了
                let _ = link.send(&Message::Settled { data: s }).await;
            }
            _ = tick.tick() => {
                let (report, done) = drained.tick(core.state.live.count(), core.gateway.serving());
                if let Some(n) = report {
                    let _ = link.send(&Message::InFlight { n: n as u64 }).await;
                }
                if done {
                    break;
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                // H6：到点了还没结束的中断掉，记成中断
                let n = abort_all(&core.state);
                tracing::warn!(n, "the drain limit passed; aborting the requests still running");
                // 让结局进记录
                tokio::time::sleep(Duration::from_secs(2)).await;
                break;
            }
        }
    }
    while let Ok(s) = settled.try_recv() {
        let _ = link.send(&Message::Settled { data: s }).await;
    }
    let _ = link.send(&Message::InFlight { n: 0 }).await;
    // H8：我们自己也是接替者、前任还没退的话，等它（请求库要由最老的那个退出之后才打开）
    let mut opened = core.store_opened.clone();
    if !*opened.borrow() {
        tracing::info!("waiting for the core before this one to exit before leaving");
        let _ = tokio::time::timeout(DRAIN_LIMIT, opened.wait_for(|o| *o)).await;
    }
    let _ = link.send(&Message::Bye).await;
    tracing::info!("drained; exiting");
}

/// 没交出去：照常 accept，等手上的请求结束（最多 `limit`），然后退出。和桌面端现行的
/// 「等请求结束再重启」一样。
async fn retire(core: &Core, limit: Duration) {
    let deadline = Instant::now() + limit;
    while core.state.live.count() > 0 && Instant::now() < deadline {
        tokio::time::sleep(DRAIN_TICK).await;
    }
}

/// 中止所有在跑的请求。返回几个
fn abort_all(state: &tw_gateway::AppState) -> usize {
    state
        .bus
        .in_flight()
        .requests
        .iter()
        .filter(|r| state.aborts.request(r.id).is_ok())
        .count()
}

/// arm 之后的日志写到哪：数据目录里，每次 arm 从头写。
pub const HANDOVER_LOG: &str = "twcore.handover.log";

/// 标准输出、错误输出改写进 [`HANDOVER_LOG`]（开不了就丢掉），标准输入接空设备。**再也
/// 不碰桌面端的管道**：它退出之后写管道会出错。
fn stdio_to_file(dir: &Path) {
    let path = dir.join(HANDOVER_LOG);
    let file = {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        o.open(&path)
    };
    let file = match file {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path = %path.display(), "the handover log could not be opened; output is discarded: {e}");
            match std::fs::OpenOptions::new().write(true).open(null_device()) {
                Ok(f) => f,
                Err(_) => return,
            }
        }
    };
    redirect(file);
}

#[cfg(unix)]
fn null_device() -> &'static str {
    "/dev/null"
}

#[cfg(windows)]
fn null_device() -> &'static str {
    "NUL"
}

#[cfg(unix)]
fn redirect(file: std::fs::File) {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: dup2 把 1、2 号描述符原子地换成这个文件；之后 `file` 关掉自己那个也不影响
    unsafe {
        libc::dup2(fd, libc::STDOUT_FILENO);
        libc::dup2(fd, libc::STDERR_FILENO);
        if let Ok(null) = std::fs::File::open("/dev/null") {
            libc::dup2(null.as_raw_fd(), libc::STDIN_FILENO);
        }
    }
}

#[cfg(windows)]
fn redirect(file: std::fs::File) {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle};
    // 句柄交给进程的标准句柄表，**不再关**（std 每次写都现取标准句柄）
    let h = file.into_raw_handle();
    // SAFETY: 换的是本进程的标准句柄；h 是一个开着的文件句柄，之后一直开着
    unsafe {
        SetStdHandle(STD_OUTPUT_HANDLE, h as _);
        SetStdHandle(STD_ERROR_HANDLE, h as _);
    }
}

/// 重新打开桌面端一次（H7）。**和 core 脱开**：core 随后退出时不能把它带走。
fn relaunch(r: &tw_api::Relaunch) {
    use std::process::{Command, Stdio};
    let mut cmd = match r {
        tw_api::Relaunch::BundleId { id } => {
            let mut c = Command::new("open");
            c.arg("-b").arg(id);
            c
        }
        tw_api::Relaunch::Exe { path, args } => {
            let mut c = Command::new(path);
            c.args(args);
            detach(&mut c);
            c
        }
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            tracing::info!(?r, "no successor yet; reopening the desktop app once");
            // 收尸，不留僵尸进程
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => tracing::warn!(?r, "the desktop app could not be reopened: {e}"),
    }
}

#[cfg(unix)]
fn detach(c: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid 是 async-signal-safe 的，fork 之后、exec 之前调它是标准做法
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach(c: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    c.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

// ─────────────────────────────────────────────────────────── 新 core

/// 去连锁的持有者，结果是三种之一。
pub enum Approach {
    /// 它没有 arm（或者正忙着交给别人）：照旧报「已有实例在运行」
    NotArmed,
    /// 连上了，控制面和快照到手
    Proceed(Incoming),
    /// 它已经退了（退回现行流程，或者交出监听之前就崩了，F6）：照常启动
    StartFresh,
}

/// 接替者手上的交接连接。
pub struct Incoming {
    link: Link,
    /// 控制面的监听（旧 core 交过来的）
    pub control: Option<Socket>,
    snapshot: Option<Snapshot>,
    /// 旧 core 的 pid（锁文件里的那个）
    pub predecessor: u32,
}

/// 旧 core 交过来的网关与远程控制端口的监听。
pub struct Inherited {
    pub gateway: Vec<(SocketAddr, std::net::TcpListener)>,
    pub remote: Option<(SocketAddr, std::net::TcpListener)>,
}

/// 新 core 取锁时发现持有者活着：连它的交接端点（H1）。
pub async fn approach(dir: &Path, holder: u32) -> Approach {
    let mut link = match tw_handover::connect(dir).await {
        Ok(Some(l)) => l,
        Ok(None) => return Approach::NotArmed,
        Err(e) => {
            tracing::warn!("the handover endpoint is there but could not be used: {e}");
            return Approach::NotArmed;
        }
    };
    let hello = Message::Hello {
        version: tw_handover::VERSION,
        core_version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
    };
    if let Err(e) = link.send(&hello).await {
        tracing::warn!("the running core did not take the handover greeting: {e}");
        return gone_or_busy(holder).await;
    }
    let control = match tokio::time::timeout(HELLO_WITHIN, link.recv()).await {
        Ok(Ok((Message::Control { .. }, mut socks))) if socks.len() == 1 => socks.pop(),
        Ok(Ok((Message::Refused { reason }, _))) => {
            tracing::info!(%reason, "the running core cannot hand over; asking it to drain and exit the current way");
            return fall_back(link, holder).await;
        }
        // 认不出它的话：版本不同的另一种样子（H2）
        Ok(Err(tw_handover::Error::Malformed(e))) => {
            tracing::info!(
                "the running core speaks another handover version ({e}); asking it to drain and exit the current way"
            );
            return fall_back(link, holder).await;
        }
        Ok(Ok((other, _))) => {
            tracing::warn!(?other, "an unexpected first handover message");
            return fall_back(link, holder).await;
        }
        Ok(Err(e)) => {
            tracing::info!("the running core closed the handover: {e}");
            return gone_or_busy(holder).await;
        }
        // 不说话：它正在交给另一个接替者
        Err(_) => return Approach::NotArmed,
    };
    let snapshot = match tokio::time::timeout(HELLO_WITHIN, link.recv()).await {
        Ok(Ok((Message::Snapshot { data }, _))) => unwrap(data),
        other => {
            tracing::warn!(ok = other.is_ok(), "the snapshot did not arrive");
            // 控制面的套接字丢掉：旧的那个回到待交接、接着用它
            drop(control);
            return gone_or_busy(holder).await;
        }
    };
    Approach::Proceed(Incoming {
        link,
        control,
        snapshot,
        predecessor: holder,
    })
}

/// 请旧 core 按现行方式排空后退出，等它退（H2）。
async fn fall_back(mut link: Link, holder: u32) -> Approach {
    let _ = link.send(&Message::Fallback).await;
    if wait_gone(holder, FALLBACK_EXIT_WITHIN).await {
        Approach::StartFresh
    } else {
        tracing::warn!(pid = holder, "the running core did not exit in time");
        Approach::NotArmed
    }
}

/// 交接连接没走下去：旧 core 没了就照常启动（F6），还在就算「已有实例」（守护退避重试）。
async fn gone_or_busy(holder: u32) -> Approach {
    if wait_gone(holder, Duration::from_secs(2)).await {
        Approach::StartFresh
    } else {
        Approach::NotArmed
    }
}

async fn wait_gone(pid: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if !crate::proc::alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

impl Incoming {
    /// 接进旧 core 的快照。请求号从它的号往后跳（见 [`ID_GAP`]）
    pub fn import(&mut self, state: &tw_gateway::AppState) {
        let Some(s) = self.snapshot.take() else {
            tracing::warn!(
                "the snapshot from the previous core was not readable; starting from scratch"
            );
            return;
        };
        state.bus.resume_after(s.next_id.saturating_add(ID_GAP));
        state.handover_import(&s.gateway);
    }

    /// 要监听（配置读进来了、运行时建好了）。
    pub async fn listeners(&mut self) -> Result<Inherited> {
        self.link
            .send(&Message::WantListeners)
            .await
            .context("the previous core left before handing over the listeners")?;
        let (m, socks) = tokio::time::timeout(HELLO_WITHIN, self.link.recv())
            .await
            .context("the previous core did not hand over the listeners in time")?
            .context("the previous core left before handing over the listeners")?;
        let Message::Listeners { infos } = m else {
            anyhow::bail!("expected the listeners from the previous core, got {m:?}");
        };
        inherited(infos, socks)
    }

    /// 提交：从此不再回退（H5）。**发不出去也算提交了**：旧 core 这时候没了的话（F6），
    /// 套接字已经在手上，直接接着服务
    pub async fn commit(&mut self) {
        if let Err(e) = self.link.send(&Message::Commit).await {
            tracing::warn!("the previous core is gone at commit; serving on without it: {e}");
        }
    }

    /// 提交之后跟着旧 core 排空：结算、在途数、`Bye`。连接断了（旧 core 退出或崩溃）就放掉
    /// 快照里记着的旧 core 的名额，等它的进程没了再回到平常。
    pub fn follow_drain(
        mut self,
        state: tw_gateway::AppState,
        handover: Arc<tw_control::handover::Handover>,
    ) {
        tokio::spawn(async move {
            loop {
                match self.link.recv().await {
                    Ok((Message::Settled { data }, _)) => state.handover_settled(&data),
                    Ok((Message::InFlight { n }, _)) => handover.set_draining(n as usize),
                    Ok((Message::Bye, _)) => break,
                    Ok((other, _)) => tracing::debug!(?other, "ignoring a handover message"),
                    Err(_) => break,
                }
            }
            // 收到 Bye 的话等它的进程真没了再放：它说完 Bye 到退出之间手上已经没有请求了，
            // 但名额要等它真的不可能再用才算放开。连接断了（崩了）也一样等一下
            let _ = wait_gone(self.predecessor, Duration::from_secs(60)).await;
            state.handover_release_all();
            handover.set_draining(0);
            // 期间又被 arm 了（H8）就别改回去
            if handover.phase() == HandoverState::Draining {
                handover.set_phase(HandoverState::Idle);
            }
            tracing::info!("the previous core has finished draining");
        });
    }

    /// 等交接连接断开（安全模式的接替者用：它不要监听，只是一直握着连接）
    pub async fn hold(mut self) {
        while self.link.recv().await.is_ok() {}
    }
}

/// 按角色分开收到的监听（`infos` 和套接字按顺序一一对应）。
fn inherited(infos: Vec<ListenerInfo>, socks: Vec<Socket>) -> Result<Inherited> {
    anyhow::ensure!(
        infos.len() == socks.len(),
        "{} listeners described, {} handed over",
        infos.len(),
        socks.len()
    );
    let mut out = Inherited {
        gateway: Vec::new(),
        remote: None,
    };
    for (info, s) in infos.into_iter().zip(socks) {
        // Windows 上只有 TCP 一种，这个 match 在那儿只有一支
        #[allow(clippy::infallible_destructuring_match)]
        let l = match s {
            Socket::Tcp(l) => l,
            #[cfg(unix)]
            Socket::Unix(_) => {
                anyhow::bail!(
                    "a {:?} listener was handed over as a unix socket",
                    info.role
                )
            }
        };
        let want = info
            .want
            .with_context(|| format!("a {:?} listener without its address", info.role))?;
        match info.role {
            Role::Gateway => out.gateway.push((want, l)),
            Role::Remote => out.remote = Some((want, l)),
            Role::Control => anyhow::bail!("a control listener among the listeners"),
        }
    }
    Ok(out)
}

/// 新 core 收到监听之后最晚什么时候提交（见 [`COMMIT_WITHIN`]）
pub fn commit_within() -> Duration {
    COMMIT_WITHIN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arm_clock_relaunches_once_then_gives_up() {
        let t0 = Instant::now();
        let w = Duration::from_secs(120);
        let mut c = ArmClock::new(t0, w, true);
        assert_eq!(c.deadline, t0 + w);
        assert_eq!(c.expired(t0 + w), Expiry::Relaunch);
        assert_eq!(c.deadline, t0 + w * 2, "重开之后再等一个周期");
        assert_eq!(c.expired(t0 + w * 2), Expiry::GiveUp, "只重开一次");
    }

    #[test]
    fn without_a_relaunch_the_arm_clock_gives_up_at_once() {
        let t0 = Instant::now();
        let mut c = ArmClock::new(t0, Duration::from_secs(5), false);
        assert_eq!(c.expired(t0), Expiry::GiveUp);
    }

    #[test]
    fn a_failed_takeover_waits_a_fresh_period_but_does_not_relaunch_again() {
        let t0 = Instant::now();
        let w = Duration::from_secs(10);
        let mut c = ArmClock::new(t0, w, true);
        assert_eq!(c.expired(t0 + w), Expiry::Relaunch);
        c.restart(t0 + w * 5);
        assert_eq!(c.deadline, t0 + w * 6);
        assert_eq!(c.expired(c.deadline), Expiry::GiveUp);
    }

    #[test]
    fn draining_reports_changes_and_needs_a_few_calm_ticks() {
        let mut d = Drained::default();
        assert_eq!(d.tick(2, 1), (Some(2), false));
        assert_eq!(d.tick(2, 1), (None, false), "没变不报");
        assert_eq!(d.tick(0, 1), (Some(0), false), "还有连接没结束");
        assert_eq!(d.tick(0, 0), (None, false));
        assert_eq!(d.tick(0, 0), (None, false));
        assert_eq!(d.tick(0, 0), (None, true), "连着几拍都空了才算做完");
        let mut d = Drained::default();
        d.tick(0, 0);
        d.tick(0, 0);
        assert_eq!(d.tick(1, 0), (Some(1), false), "中间又来了一个就重新数");
        assert_eq!(d.tick(0, 0), (Some(0), false));
    }

    #[test]
    fn the_snapshot_wrapper_round_trips_and_tolerates_garbage() {
        let g = serde_json::json!({"affinity": [1, 2, 3]});
        let s = unwrap(wrap(g.clone(), 42)).expect("拆得开");
        assert_eq!(s.gateway, g);
        assert_eq!(s.next_id, 42);
        assert!(unwrap(serde_json::json!("不是快照")).is_none());
    }

    #[test]
    fn listeners_are_sorted_by_role_and_a_count_mismatch_is_an_error() {
        let l = || std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a: SocketAddr = "127.0.0.1:8788".parse().unwrap();
        let r: SocketAddr = "0.0.0.0:9788".parse().unwrap();
        let infos = vec![
            ListenerInfo {
                role: Role::Gateway,
                want: Some(a),
            },
            ListenerInfo {
                role: Role::Remote,
                want: Some(r),
            },
        ];
        let got = inherited(infos.clone(), vec![Socket::Tcp(l()), Socket::Tcp(l())]).unwrap();
        assert_eq!(got.gateway.len(), 1);
        assert_eq!(got.gateway[0].0, a);
        assert_eq!(got.remote.as_ref().map(|x| x.0), Some(r));
        assert!(inherited(infos, vec![Socket::Tcp(l())]).is_err());
    }

    #[test]
    fn the_successor_commits_well_before_the_previous_core_gives_up() {
        // 两边同一刻到点的话会各自以为是主：新 core 的时限要短一截
        const { assert!(COMMIT_WITHIN.as_secs() + 5 <= TAKEOVER_LIMIT.as_secs()) };
    }
}
