//! Windows：回环上的随机端口 + 口令；套接字用 `WSADuplicateSocketW` /
//! `WSASocketW` 复制到对端进程。
//!
//! **Winsock 不用自己初始化**：两端在用到这些函数之前，都已经经由 std / tokio
//! 建过 TCP 套接字（端点本身、交接连接本身），`WSAStartup` 早就调过了。

use std::io;
use std::mem;
use std::net::{Ipv4Addr, SocketAddr};
use std::os::windows::io::{AsRawSocket, FromRawSocket, RawSocket};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use windows_sys::Win32::Networking::WinSock::{
    FROM_PROTOCOL_INFO, INVALID_SOCKET, SOCKET, WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED,
    WSADuplicateSocketW, WSAGetLastError, WSAPROTOCOL_INFOW, WSASocketW,
};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};

use crate::{ENDPOINT_FILE, Error, Result, Socket, TOKEN_FILE, token};

/// 一次最多读这么多。
const CHUNK: usize = 64 * 1024;

/// 连上来之后交口令的时限。
pub(crate) const TOKEN_WAIT: Duration = Duration::from_secs(5);

/// 口令帧的长度上限（口令本身是 64 个十六进制字符）。
pub(crate) const TOKEN_FRAME_MAX: usize = 256;

/// 口令的随机字节数（文件里是它的十六进制）。
const TOKEN_BYTES: usize = 32;

pub(crate) struct Listener {
    listener: TcpListener,
    dir: PathBuf,
    port: u16,
    /// 十六进制口令，即对端要交上来的那一帧
    token: String,
}

impl Listener {
    pub(crate) async fn bind(dir: &Path) -> Result<Listener> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let token = token::hex(&random(TOKEN_BYTES)?);
        // **先口令后端口**：对端以端口文件为准判断 arm 了没有，看见端口时口令必须已经在
        write_atomic(&dir.join(TOKEN_FILE), &token)?;
        write_atomic(&dir.join(ENDPOINT_FILE), &port.to_string())?;
        Ok(Listener {
            listener,
            dir: dir.to_path_buf(),
            port,
            token,
        })
    }

    /// 身份核对（口令）在 `Endpoint::accept` 里做，这里只管接。
    pub(crate) async fn accept(&self) -> Result<Option<(Stream, Option<u32>)>> {
        let (s, _) = self.listener.accept().await?;
        // pid 要等对端的 Hello
        Ok(Some((Stream { s }, None)))
    }

    pub(crate) fn token(&self) -> &[u8] {
        self.token.as_bytes()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // 内容还是自己写的才删：排空中的旧 core 不能删掉新 core 后来开的端点（H8）
        let port = self.dir.join(ENDPOINT_FILE);
        if std::fs::read_to_string(&port).is_ok_and(|s| s.trim() == self.port.to_string()) {
            let _ = std::fs::remove_file(&port);
        }
        let token = self.dir.join(TOKEN_FILE);
        if std::fs::read_to_string(&token).is_ok_and(|s| s.trim() == self.token) {
            let _ = std::fs::remove_file(&token);
        }
    }
}

pub(crate) async fn connect(dir: &Path) -> Result<Option<(Stream, Option<u32>)>> {
    let Ok(text) = std::fs::read_to_string(dir.join(ENDPOINT_FILE)) else {
        return Ok(None);
    };
    let Ok(port) = text.trim().parse::<u16>() else {
        return Ok(None);
    };
    if !dir.join(TOKEN_FILE).exists() {
        return Ok(None);
    }
    let Ok(s) = TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await else {
        return Ok(None);
    };
    // 旧 core 的 pid 这一头问不到（它不发 Hello）；调用方要的话看锁文件
    Ok(Some((Stream { s }, None)))
}

pub(crate) fn read_token(dir: &Path) -> Result<String> {
    Ok(std::fs::read_to_string(dir.join(TOKEN_FILE))?
        .trim()
        .to_string())
}

pub(crate) struct Stream {
    s: TcpStream,
}

impl Stream {
    pub(crate) async fn write_frame(&mut self, frame: &[u8], _sockets: &[&Socket]) -> Result<()> {
        // 套接字已经在 export 里变成帧里的 WSAPROTOCOL_INFOW 了
        self.s.write_all(frame).await?;
        Ok(())
    }

    pub(crate) async fn read_some(&mut self, buf: &mut Vec<u8>) -> Result<usize> {
        buf.reserve(CHUNK);
        Ok(self.s.read_buf(buf).await?)
    }

    pub(crate) fn import(&mut self, n: usize, w: &[String]) -> Result<Vec<Socket>> {
        if w.len() != n {
            return Err(Error::Malformed(format!(
                "the message carries {n} sockets but {} protocol infos",
                w.len()
            )));
        }
        w.iter().map(|hex| socket_from_info(hex)).collect()
    }
}

/// 对 `peer_pid` 复制每个套接字，得到它的 `WSAPROTOCOL_INFOW`（十六进制）。
/// 发送方自己的句柄不受影响。
pub(crate) fn export(sockets: &[&Socket], peer_pid: u32) -> Result<Vec<String>> {
    sockets
        .iter()
        .map(|s| {
            let Socket::Tcp(l) = s;
            let raw = l.as_raw_socket() as SOCKET;
            // SAFETY: WSAPROTOCOL_INFOW 是纯数据结构，全零合法；出参由系统填满
            let mut info: WSAPROTOCOL_INFOW = unsafe { mem::zeroed() };
            if unsafe { WSADuplicateSocketW(raw, peer_pid, &mut info) } != 0 {
                return Err(Error::Transfer(format!(
                    "WSADuplicateSocketW for pid {peer_pid} failed: {}",
                    wsa_error()
                )));
            }
            // SAFETY: 把一个已初始化的纯数据结构按字节看
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    (&info as *const WSAPROTOCOL_INFOW).cast::<u8>(),
                    mem::size_of::<WSAPROTOCOL_INFOW>(),
                )
            };
            Ok(token::hex(bytes))
        })
        .collect()
}

fn socket_from_info(hex: &str) -> Result<Socket> {
    let bytes = token::unhex(hex)
        .filter(|b| b.len() == mem::size_of::<WSAPROTOCOL_INFOW>())
        .ok_or_else(|| Error::Malformed("a socket's protocol info is not readable".into()))?;
    // SAFETY: 长度刚核对过；WSAPROTOCOL_INFOW 是纯数据结构，任何字节都是合法值
    let info: WSAPROTOCOL_INFOW =
        unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<WSAPROTOCOL_INFOW>()) };
    // SAFETY: info 活到调用结束
    let s = unsafe {
        WSASocketW(
            FROM_PROTOCOL_INFO,
            FROM_PROTOCOL_INFO,
            FROM_PROTOCOL_INFO,
            &info,
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    if s == INVALID_SOCKET {
        return Err(Error::Transfer(format!(
            "WSASocketW failed: {}",
            wsa_error()
        )));
    }
    // SAFETY: s 是刚还原出来、只归我们所有的套接字
    Ok(Socket::Tcp(unsafe {
        std::net::TcpListener::from_raw_socket(s as RawSocket)
    }))
}

fn wsa_error() -> io::Error {
    // SAFETY: 没有参数，只读本线程的错误码
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

/// 系统随机源（`BCryptGenRandom`，系统首选的 RNG）。
fn random(n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    // SAFETY: buf 有 n 个字节可写；算法句柄为空配 SYSTEM_PREFERRED 是文档给的用法
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            n as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        return Err(Error::Io(io::Error::other(format!(
            "BCryptGenRandom failed: {status:#x}"
        ))));
    }
    Ok(buf)
}

/// 先写临时文件再改名，对端不会读到写了一半的内容。
fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}
