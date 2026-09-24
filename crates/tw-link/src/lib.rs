//! 控制通道：握手与加密。core 和桌面端共用这一份。
//!
//! # 为什么不是 TLS
//!
//! 控制面的两端是同一个人的两个程序：本机的 socket、Windows 的回环端口，
//! 以及服务器上的远程端口。TLS 要证书，证书要么自签（每台 Mac 都得学会信它），
//! 要么去申请（服务器常常只有一个内网地址）。**两端本来就共享一把钥匙**
//! （`listen.control.key`），拿它当 Noise 的 PSK，双向鉴权、前向保密都有了，
//! 没有证书这回事。
//!
//! # 协议
//!
//! `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`，实现用 snow，这里不写任何密码算法。
//!
//! ```text
//! 应用 → core   [u16 长度][e, 加密的 ClientHello { proto, app }]
//! core → 应用   [u16 长度][e, ee, 加密的 ServerHello { proto, core, accept }]
//!               或者一个明文字节 REJECT，然后断开（第一条解不开：钥匙不对）
//! 之后双向      [u16 长度][密文]……   每帧密文最多 65535 字节，含 16 字节的标签
//! ```
//!
//! PSK 放在第 0 位：第一条消息的载荷就是用钥匙加密的，不知道钥匙的一方连
//! 第一条都写不对，core 读到的那一刻就知道。版本在握手里交换，不一致时
//! core 回 `accept: false` 再断开 —— 应用拿到的是两个确切的版本号，不是一个
//! 说不清的 HTTP 错误。
//!
//! 握手之后是 [`SecureStream`]，一个普通的 `AsyncRead + AsyncWrite`。hyper 的
//! HTTP/1.1（包括 SSE 事件流）原样跑在它上面，端点、请求、响应一概不变。
//!
//! # 用法
//!
//! 客户端：拿到底层的流（unix socket、TCP），[`connect`]。服务端：一个
//! [`Acceptor`]，每条连接 [`Acceptor::accept`]。钥匙在服务端是**每次握手现取**
//! 的（一个闭包）：配置重载换了钥匙，下一条连接就按新的算。

use std::time::Duration;

use serde::{Deserialize, Serialize};

mod handshake;
mod key;
mod stream;
#[cfg(test)]
mod tests;

pub use handshake::{Accepted, Acceptor, KeySource, connect, connect_with_timeout};
pub use key::{KeyReadError, key_in_config, read_key};
pub use stream::SecureStream;
pub use tw_api::control::{ControlKey, KeyFormatError};

/// 握手的名字。两端必须一字不差。
pub const PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";

/// 混进握手哈希的前言。**换协议时改它**：两端前言不同，握手就对不上 ——
/// 不会有一个旧客户端误打误撞地和新协议握上手。
pub const PROLOGUE: &[u8] = b"ThinkWatch control channel 1";

/// 握手最多等多久。两端都按它算：卡在半路的连接不能一直占着。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// core 解不开第一条消息时回的那个明文字节。
///
/// **它可以被伪造**，而伪造的后果只是应用显示一句「钥匙不对」—— 没有别的
/// 效果。握手消息的长度头第一个字节永远是 0（握手消息不会超过
/// [`MAX_HANDSHAKE_LEN`]），所以它和一条正常的第二条消息分得开。
pub const REJECT: u8 = 0xFF;

/// 一条握手消息最长多少。载荷只有两个版本号，几百字节足够；给个上限是
/// 为了一条乱写的长度头不能让对端去等 64 KB。
pub const MAX_HANDSHAKE_LEN: usize = 1024;

/// 一帧密文最长多少：长度头是 u16。
pub const MAX_FRAME: usize = 65535;

/// ChaChaPoly 的标签长度。
pub const TAG_LEN: usize = 16;

/// 一帧里最多放多少明文。写得更长的会被拆成几帧。
pub const MAX_PLAINTEXT: usize = MAX_FRAME - TAG_LEN;

/// 第一条消息的载荷：应用说自己是谁。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    /// 应用讲的控制面版本（[`tw_api::CONTROL_API_VERSION`]）
    pub proto: u32,
    /// 应用自己的版本，给日志看
    pub app: String,
}

/// 第二条消息的载荷：core 的回答。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHello {
    /// core 讲的控制面版本
    pub proto: u32,
    /// core 的版本（CalVer），应用据此说「服务器是 X」
    pub core: String,
    /// 版本对得上吗。`false` 时 core 发完这条就断开
    pub accept: bool,
}

/// 握手或传输的失败，**按应用要显示的状态分**。
///
/// 应用按变体选说法：连不上、被关了、钥匙不对、版本不一致、超时。
/// `Io` 是剩下那些（数据坏了、读写出错），说法是「连接出错」。
#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    /// 连不上：地址不对、端口没开、socket 文件不在。**这个 crate 自己不建连**，
    /// 调用方建连失败时用 [`LinkError::unreachable`] 包成这一种，好让应用只认
    /// 一个错误类型。
    #[error("the control plane could not be reached: {0}")]
    Unreachable(#[source] std::io::Error),
    /// 连上了，对面什么都没说就关了。远程端口上不在放行名单里的来源就是这样
    #[error("the control plane closed the connection without answering")]
    Closed,
    /// 钥匙不对。客户端：core 回了 [`REJECT`]。服务端：第一条消息解不开
    #[error("the control key does not match")]
    WrongKey,
    /// 两边讲的控制面版本不一样。`ours` 是这一端的，`theirs` 是对面的；
    /// `peer_version` 是对面程序自己的版本（客户端看到的是 core 的 CalVer，
    /// 服务端看到的是应用的版本）
    #[error(
        "the control-plane versions differ: this side speaks {ours}, the other side speaks {theirs} ({peer_version})"
    )]
    VersionMismatch {
        ours: u32,
        theirs: u32,
        peer_version: String,
    },
    /// 握手没在 [`HANDSHAKE_TIMEOUT`] 之内走完
    #[error("the handshake did not finish in time")]
    Timeout,
    #[error("the control channel failed: {0}")]
    Io(#[source] std::io::Error),
}

impl LinkError {
    /// 建连那一步的失败。
    pub fn unreachable(e: std::io::Error) -> Self {
        LinkError::Unreachable(e)
    }

    /// 读写时的失败。**对面断开的几种说法都归成 [`LinkError::Closed`]** ——
    /// 对应用来说它们是同一件事，而 Windows 和 unix 报的错误码各不相同。
    pub(crate) fn from_io(e: std::io::Error) -> Self {
        use std::io::ErrorKind::*;
        match e.kind() {
            UnexpectedEof | ConnectionReset | ConnectionAborted | BrokenPipe => LinkError::Closed,
            _ => LinkError::Io(e),
        }
    }
}

fn params() -> snow::params::NoiseParams {
    PATTERN
        .parse()
        .expect("the pattern name is a constant that snow understands")
}

fn invalid(what: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what.into())
}
