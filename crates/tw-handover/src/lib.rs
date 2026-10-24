//! 新旧 core 之间的交接：把正在监听的套接字和运行时状态交给接替它的那一个。
//!
//! # 为什么传套接字，而不是各自绑一次
//!
//! 传过去的是**同一个内核套接字**，不是重新绑定。旧的停止 accept 之后，队列里
//! 还没被接受的连接由新的取走，一个都不丢；两边短暂地同时 accept 也没关系，
//! 每个连接只会被一方接受。各自绑（`SO_REUSEPORT`）做不到这一点：Linux 上旧的
//! 那个关掉时，它队列里的连接会被重置；Windows 上没有等价的东西。
//!
//! # 交接连接
//!
//! 旧 core 被 arm 之后在数据目录里开一个**只为交接用**的端点（[`Endpoint`]），
//! 新 core 取锁时发现锁的持有者还活着，就去连它（[`connect`]）。连不上 = 没有
//! arm，照旧报「已有实例在运行」。
//!
//! - macOS / Linux：数据目录下的 unix socket（[`ENDPOINT_FILE`]），套接字随
//!   `SCM_RIGHTS` 辅助消息走；对端用 `getpeereid` / `SO_PEERCRED` 核对是同一个用户。
//! - Windows：回环上的随机端口，号码写进 [`ENDPOINT_FILE`]；连上之后先交
//!   [`TOKEN_FILE`] 里的口令。套接字由发送方对接收方的 pid 调
//!   `WSADuplicateSocketW`，把得到的 `WSAPROTOCOL_INFOW` 随消息发过去，接收方
//!   `WSASocketW` 还原。
//!
//! 消息是带长度前缀的 JSON（[`Message`]）。
//!
//! # 不做兼容
//!
//! [`VERSION`] 是这条连接自己的版本号，和 `tw-api` 的协议号无关。**格式有任何
//! 改动都升它**；双方不同就不交接，退回「等请求结束再重启」（[`Message::Refused`]）。
//! 快照、结算的内容由网关定义，这里只当成 JSON 搬运，也跟着这个号走。

use std::net::SocketAddr;
use std::path::Path;

/// 交接连接的版本号。**格式有任何改动都升它**，见模块文档。
pub const VERSION: u32 = 1;

/// 交接端点在数据目录里的文件名：unix 上是 socket 本身，Windows 上是写着端口号的文件。
#[cfg(unix)]
pub const ENDPOINT_FILE: &str = "twcore.handover.sock";
#[cfg(not(unix))]
pub const ENDPOINT_FILE: &str = "twcore.handover.port";

/// Windows 上交接连接的口令文件。回环端口挡不住同机的任何进程，门靠它。
#[cfg(not(unix))]
pub const TOKEN_FILE: &str = "twcore.handover.token";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// 对端发来的不是一条看得懂的消息
    #[error("the handover peer sent something unreadable: {0}")]
    Malformed(String),
    /// 对端不是同一个用户，或者口令不对
    #[error("the handover peer is not allowed: {0}")]
    Denied(String),
    /// 套接字传不过去（句柄无效、权限不足、对端 pid 不对）
    #[error("a listening socket could not be handed over: {0}")]
    Transfer(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// 交接的一个监听。
#[derive(Debug)]
pub enum Socket {
    Tcp(std::net::TcpListener),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixListener),
}

/// 一个监听是做什么的，以及它对应配置里的哪个地址。随 [`Message::Listeners`]、
/// [`Message::Control`] 走，和消息里附带的套接字**按顺序一一对应**。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ListenerInfo {
    pub role: Role,
    /// 配置算出来的地址（网关监听比对「变没变」用它）。控制面的 unix socket 没有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub want: Option<SocketAddr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// 数据面
    Gateway,
    /// 远程控制端口（tw-link）
    Remote,
    /// 本机控制面
    Control,
}

/// 交接连接上的一条消息。顺序见方案第 4 节：
///
/// ```text
/// 新 → 旧  Hello
/// 旧 → 新  Refused（版本不同，到此为止）| Control + 控制面套接字
/// 旧 → 新  Snapshot
/// 新 → 旧  WantListeners            （新 core 的配置读进来了、运行时建好了）
/// 旧 → 新  Listeners + 网关、远程控制端口的套接字
/// 新 → 旧  Commit                   （新 core 已在全部监听上 accept；不可逆）
/// 旧 → 新  Settled* / InFlight*     （排空期，每个请求结束一条）
/// 旧 → 新  Bye                      （马上退出）
/// 新 → 旧  Fallback                 （任何时候：请按现行方式排空后退出，新的自己绑）
/// ```
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Message {
    Hello {
        version: u32,
        core_version: String,
        pid: u32,
    },
    Refused {
        reason: String,
    },
    Control {
        info: ListenerInfo,
    },
    Snapshot {
        data: serde_json::Value,
    },
    WantListeners,
    Listeners {
        infos: Vec<ListenerInfo>,
    },
    Commit,
    Settled {
        data: serde_json::Value,
    },
    InFlight {
        n: u64,
    },
    Bye,
    Fallback,
}

/// 单帧上限。快照可能较大（几千条亲和、限额计数），但不该没有边：长度前缀
/// 读错了时宁可报错，也别照着一个 4 GiB 的数去分配。
const MAX_FRAME: usize = 16 << 20;

/// 一帧最多附几个套接字。网关每张网卡一个、再加远程控制端口，远远够了；
/// 也在 Linux 一条辅助消息的上限（`SCM_MAX_FD` = 253）之内。
const MAX_SOCKETS: usize = 64;

/// 同时在过门的连接最多几个，见 [`Endpoint::admit`]。
const MAX_CHECKS: usize = 64;

/// 端点的门：连上来的连接要过了它才交出去。
#[derive(Clone)]
enum Gate {
    /// unix：同一用户已经由内核核对过（`Listener::accept` 里），直接放行
    #[cfg(unix)]
    Open,
    /// 第一帧必须是这个口令，`wait` 之内交上来
    #[cfg(any(windows, test))]
    Token {
        token: std::sync::Arc<[u8]>,
        wait: std::time::Duration,
    },
}

/// 口令帧的长度上限（口令本身是 64 个十六进制字符）。没过门的连接不配让我们
/// 分配 16 MiB。
#[cfg(any(windows, test))]
const TOKEN_FRAME_MAX: usize = 256;

impl Gate {
    async fn check(self, link: Link) -> Option<Link> {
        match self {
            #[cfg(unix)]
            Gate::Open => Some(link),
            #[cfg(any(windows, test))]
            Gate::Token { token, wait } => {
                let mut link = link;
                let frame = link.read_frame(TOKEN_FRAME_MAX);
                match tokio::time::timeout(wait, frame).await {
                    Ok(Ok(t)) if token::eq(&t, &token) => Some(link),
                    _ => None,
                }
            }
        }
    }
}

/// 线上的一帧：消息外面包一层，带上套接字的附件信息。两端都在本 crate，
/// 不对外。
#[derive(serde::Serialize)]
struct EnvelopeOut<'a> {
    m: &'a Message,
    /// 随这一帧走的套接字个数（unix：`SCM_RIGHTS` 里的句柄数）
    #[serde(skip_serializing_if = "is_zero")]
    n: usize,
    /// Windows：每个套接字的 `WSAPROTOCOL_INFOW`，十六进制
    #[serde(skip_serializing_if = "Vec::is_empty")]
    w: Vec<String>,
}

#[derive(serde::Deserialize)]
struct EnvelopeIn {
    m: Message,
    #[serde(default)]
    n: usize,
    #[serde(default)]
    w: Vec<String>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// 旧 core 那一头：arm 之后开着，等接替者连上来。
pub struct Endpoint {
    listener: sys::Listener,
}

impl Endpoint {
    /// 在数据目录里开交接端点。已有的同名文件（上次没退干净）先删掉。
    pub async fn bind(dir: &Path) -> Result<Endpoint> {
        Ok(Endpoint {
            listener: sys::Listener::bind(dir).await?,
        })
    }

    /// 等一个接替者。**核对过身份才交出 [`Link`]**（unix：同一用户；Windows：口令）；
    /// 不对的连接直接断开，接着等下一个。
    ///
    /// 可以被取消（放在 `select!` 里）：正在核对的连接随之丢掉。
    pub async fn accept(&self) -> Result<Link> {
        self.admit(self.listener.gate()).await
    }

    /// 接连接、过门，第一个过了门的交出去。
    ///
    /// **各个连接并发地过门**：Windows 的门是「5 秒内交口令」，而回环端口同机谁都
    /// 连得上 —— 一个一个核对的话，反复连上来不说话就能把真正的接替者一直挡在外面。
    /// 同时核对的个数有上限，满了就**丢掉最早的那个**再接新的：真正的接替者连上
    /// 就交口令（回环上是微秒级），要把它挤掉，得在这点时间里连进来上限那么多个。
    /// 满了停下不接是不行的：那样占满名额就能一直堵着。
    ///
    /// unix 的门是一次 `getsockopt`，当场就有结论，不存在被拖住的问题；走同一条
    /// 路只是为了两边一套代码。
    async fn admit(&self, gate: Gate) -> Result<Link> {
        use std::collections::VecDeque;
        use tokio::task::{AbortHandle, JoinSet};

        // JoinSet 被丢掉时（返回、取消）会中止里面所有任务，没过门的连接跟着关掉
        let mut checks: JoinSet<Option<Link>> = JoinSet::new();
        let mut order: VecDeque<AbortHandle> = VecDeque::new();
        loop {
            tokio::select! {
                r = self.listener.accept() => {
                    let Some((io, pid)) = r? else {
                        continue;
                    };
                    if order.len() >= MAX_CHECKS {
                        order.pop_front().expect("full").abort();
                    }
                    let gate = gate.clone();
                    order.push_back(checks.spawn(gate.check(Link::new(io, pid))));
                }
                Some(done) = checks.join_next_with_id(), if !checks.is_empty() => {
                    let (id, link) = match done {
                        Ok((id, link)) => (id, link),
                        // 被挤掉的那个
                        Err(e) => (e.id(), None),
                    };
                    order.retain(|h| h.id() != id);
                    if let Some(link) = link {
                        return Ok(link);
                    }
                }
            }
        }
    }

    /// 关掉端点，删掉文件。被丢掉时也做同一件事。
    pub fn close(self) {}
}

/// 新 core 那一头：去连锁的持有者。**`Ok(None)` = 它没有 arm**（端点不存在或连不上），
/// 调用方照旧报「已有实例在运行」。
pub async fn connect(dir: &Path) -> Result<Option<Link>> {
    let Some((io, pid)) = sys::connect(dir).await? else {
        return Ok(None);
    };
    #[allow(unused_mut)] // Windows 上要先在它上面交口令
    let mut link = Link::new(io, pid);
    #[cfg(windows)]
    {
        let token = sys::read_token(dir)?;
        link.write_frame(token.as_bytes(), &[]).await?;
    }
    Ok(Some(link))
}

/// 一条交接连接。
pub struct Link {
    io: sys::Stream,
    /// 读进来、还没拼成整帧的字节
    rbuf: Vec<u8>,
    peer_pid: Option<u32>,
}

impl Link {
    fn new(io: sys::Stream, peer_pid: Option<u32>) -> Link {
        Link {
            io,
            rbuf: Vec::new(),
            peer_pid,
        }
    }

    /// 对端的 pid（unix 上从内核问到的；Windows 上取 `Hello` 里报的）。
    pub fn peer_pid(&self) -> Option<u32> {
        self.peer_pid
    }

    /// 发一条不带套接字的消息。
    pub async fn send(&mut self, m: &Message) -> Result<()> {
        self.send_with(m, &[], 0).await
    }

    /// 发一条消息，附带这几个套接字（顺序即 `infos` 的顺序）。`peer_pid` 只在
    /// Windows 上用得到（`WSADuplicateSocketW` 要它）。**发送方自己的句柄不受影响**，
    /// 照样能 accept，直到它自己关掉。
    pub async fn send_with(
        &mut self,
        m: &Message,
        sockets: &[&Socket],
        peer_pid: u32,
    ) -> Result<()> {
        if sockets.len() > MAX_SOCKETS {
            return Err(Error::Transfer(format!(
                "{} sockets in one message, at most {MAX_SOCKETS}",
                sockets.len()
            )));
        }
        let env = EnvelopeOut {
            m,
            n: sockets.len(),
            w: sys::export(sockets, peer_pid)?,
        };
        let body = serde_json::to_vec(&env).map_err(|e| Error::Malformed(e.to_string()))?;
        self.write_frame(&body, sockets).await
    }

    /// 收一条消息和它附带的套接字。对端关闭连接时返回 `Io(UnexpectedEof)`。
    pub async fn recv(&mut self) -> Result<(Message, Vec<Socket>)> {
        let body = self.read_frame(MAX_FRAME).await?;
        let env: EnvelopeIn =
            serde_json::from_slice(&body).map_err(|e| Error::Malformed(e.to_string()))?;
        if env.n > MAX_SOCKETS {
            return Err(Error::Malformed(format!(
                "{} sockets in one message",
                env.n
            )));
        }
        let sockets = self.io.import(env.n, &env.w)?;
        #[cfg(windows)]
        if let Message::Hello { pid, .. } = &env.m {
            // Windows 问不到对端 pid，只能信它自己报的 —— 对端已经交过口令，
            // 是同一个用户；报错了 pid 也只是复制不出套接字
            self.peer_pid = Some(*pid);
        }
        Ok((env.m, sockets))
    }

    /// 写一帧：4 字节大端长度 + 正文。unix 上套接字附在这一帧的第一个字节上。
    async fn write_frame(&mut self, body: &[u8], sockets: &[&Socket]) -> Result<()> {
        if body.len() > MAX_FRAME {
            return Err(Error::Malformed(format!(
                "a message of {} bytes is over the {MAX_FRAME}-byte limit",
                body.len()
            )));
        }
        let mut frame = Vec::with_capacity(4 + body.len());
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(body);
        self.io.write_frame(&frame, sockets).await
    }

    /// 读一整帧的正文，长于 `max` 的报 `Malformed`。
    async fn read_frame(&mut self, max: usize) -> Result<Vec<u8>> {
        loop {
            if self.rbuf.len() >= 4 {
                let len = u32::from_be_bytes(self.rbuf[..4].try_into().expect("4 bytes")) as usize;
                if len > max {
                    return Err(Error::Malformed(format!(
                        "a message of {len} bytes is over the {max}-byte limit"
                    )));
                }
                if self.rbuf.len() >= 4 + len {
                    let body = self.rbuf[4..4 + len].to_vec();
                    self.rbuf.drain(..4 + len);
                    return Ok(body);
                }
                // 知道要多长了，一次留够，别边读边翻倍
                self.rbuf.reserve(4 + len - self.rbuf.len());
            }
            if self.io.read_some(&mut self.rbuf).await? == 0 {
                return Err(Error::Io(std::io::ErrorKind::UnexpectedEof.into()));
            }
        }
    }
}

#[cfg(any(windows, test))]
mod token;

#[cfg(unix)]
#[path = "unix.rs"]
mod sys;

#[cfg(windows)]
#[path = "windows.rs"]
mod sys;

#[cfg(test)]
mod tests;
