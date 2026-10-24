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

/// 旧 core 那一头：arm 之后开着，等接替者连上来。
pub struct Endpoint {
    _todo: (),
}

impl Endpoint {
    /// 在数据目录里开交接端点。已有的同名文件（上次没退干净）先删掉。
    pub async fn bind(_dir: &Path) -> Result<Endpoint> {
        todo!("C1")
    }

    /// 等一个接替者。**核对过身份才交出 [`Link`]**（unix：同一用户；Windows：口令）；
    /// 不对的连接直接断开，接着等下一个。
    pub async fn accept(&self) -> Result<Link> {
        todo!("C1")
    }

    /// 关掉端点，删掉文件。被丢掉时也做同一件事。
    pub fn close(self) {}
}

/// 新 core 那一头：去连锁的持有者。**`Ok(None)` = 它没有 arm**（端点不存在或连不上），
/// 调用方照旧报「已有实例在运行」。
pub async fn connect(_dir: &Path) -> Result<Option<Link>> {
    todo!("C1")
}

/// 一条交接连接。
pub struct Link {
    _todo: (),
}

impl Link {
    /// 对端的 pid（unix 上从内核问到的；Windows 上取 `Hello` 里报的）。
    pub fn peer_pid(&self) -> Option<u32> {
        todo!("C1")
    }

    /// 发一条不带套接字的消息。
    pub async fn send(&mut self, _m: &Message) -> Result<()> {
        todo!("C1")
    }

    /// 发一条消息，附带这几个套接字（顺序即 `infos` 的顺序）。`peer_pid` 只在
    /// Windows 上用得到（`WSADuplicateSocketW` 要它）。**发送方自己的句柄不受影响**，
    /// 照样能 accept，直到它自己关掉。
    pub async fn send_with(
        &mut self,
        _m: &Message,
        _sockets: &[&Socket],
        _peer_pid: u32,
    ) -> Result<()> {
        todo!("C1")
    }

    /// 收一条消息和它附带的套接字。对端关闭连接时返回 `Io(UnexpectedEof)`。
    pub async fn recv(&mut self) -> Result<(Message, Vec<Socket>)> {
        todo!("C1")
    }
}
