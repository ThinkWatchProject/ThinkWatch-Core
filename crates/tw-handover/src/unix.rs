//! macOS / Linux：数据目录下的 unix socket，套接字随 `SCM_RIGHTS` 走。
//!
//! tokio 的 `UnixStream` 不收发辅助消息，所以读写都自己调 `recvmsg` / `sendmsg`，
//! 外面套 `async_io` —— 它在 `WouldBlock` 时让出、等就绪再试，不会堵住运行时。

use std::collections::VecDeque;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::io::Interest;
use tokio::net::{UnixListener, UnixStream};

use crate::{ENDPOINT_FILE, Error, MAX_SOCKETS, Result, Socket};

/// 一次 `recvmsg` 最多读这么多正文。
const CHUNK: usize = 64 * 1024;

#[cfg(any(target_os = "linux", target_os = "android"))]
const SEND_FLAGS: libc::c_int = libc::MSG_NOSIGNAL;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const SEND_FLAGS: libc::c_int = 0;

/// Linux 能让内核直接给收到的句柄打上 close-on-exec；macOS 没有这个标志，
/// 收到后补一次 `fcntl`（两边都补，不差这一次系统调用）。
#[cfg(any(target_os = "linux", target_os = "android"))]
const RECV_FLAGS: libc::c_int = libc::MSG_CMSG_CLOEXEC;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const RECV_FLAGS: libc::c_int = 0;

pub(crate) struct Listener {
    listener: UnixListener,
    path: PathBuf,
    /// 绑定时那个 socket 文件的 (dev, ino)。删文件前比一比：**排空中的旧 core
    /// 不能删掉新 core 后来在同一路径上开的端点**（方案 H8：排空中再更新）
    id: Option<(u64, u64)>,
}

impl Listener {
    pub(crate) async fn bind(dir: &Path) -> Result<Listener> {
        let path = dir.join(ENDPOINT_FILE);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let listener = UnixListener::bind(&path)?;
        // 0600：只有当前用户能连。accept 时还会问内核对端是谁 —— 两道都在
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let id = std::fs::symlink_metadata(&path)
            .ok()
            .map(|m| (m.dev(), m.ino()));
        Ok(Listener { listener, path, id })
    }

    /// `Ok(None)` = 连上来的不是同一个用户，已经断开。
    pub(crate) async fn accept(&self) -> Result<Option<(Stream, Option<u32>)>> {
        let (s, _) = self.listener.accept().await?;
        match peer(&s) {
            Ok((uid, pid)) if uid == own_uid() => Ok(Some((Stream::new(s), pid))),
            _ => Ok(None),
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let same = std::fs::symlink_metadata(&self.path)
            .ok()
            .map(|m| (m.dev(), m.ino()));
        if same.is_some() && same == self.id {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub(crate) async fn connect(dir: &Path) -> Result<Option<(Stream, Option<u32>)>> {
    // 文件不在、没人在听（上次没删干净的残留）、连不上 —— 都是「没有 arm」
    let Ok(s) = UnixStream::connect(dir.join(ENDPOINT_FILE)).await else {
        return Ok(None);
    };
    let (uid, pid) = peer(&s)?;
    if uid != own_uid() {
        return Err(Error::Denied(format!(
            "the handover endpoint belongs to uid {uid}"
        )));
    }
    Ok(Some((Stream::new(s), pid)))
}

fn own_uid() -> u32 {
    // SAFETY: geteuid 总是成功，没有参数
    unsafe { libc::geteuid() }
}

/// 问内核对端的 (uid, pid)。
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn peer(s: &UnixStream) -> io::Result<(u32, Option<u32>)> {
    let fd = s.as_raw_fd();
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: fd 是有效的 unix socket，两个出参都是本地变量
    if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut pid: libc::pid_t = 0;
    let mut len = mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: 出参是一个 pid_t，长度如实给出
    let ok = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut len,
        )
    } == 0;
    Ok((uid, (ok && pid > 0).then_some(pid as u32)))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer(s: &UnixStream) -> io::Result<(u32, Option<u32>)> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: 出参是一个 ucred，长度如实给出
    if unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok((cred.uid, (cred.pid > 0).then_some(cred.pid as u32)))
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "linux",
    target_os = "android"
)))]
fn peer(s: &UnixStream) -> io::Result<(u32, Option<u32>)> {
    let c = s.peer_cred()?;
    Ok((c.uid(), c.pid().and_then(|p| u32::try_from(p).ok())))
}

pub(crate) struct Stream {
    s: UnixStream,
    /// 收到、还没认领的句柄。每帧的句柄附在它的第一个字节上，帧又是按序的，
    /// 所以按先来后到认领就对得上 —— 哪怕一次读进了好几帧
    fds: VecDeque<OwnedFd>,
}

impl Stream {
    fn new(s: UnixStream) -> Stream {
        Stream {
            s,
            fds: VecDeque::new(),
        }
    }

    pub(crate) async fn write_frame(&mut self, frame: &[u8], sockets: &[&Socket]) -> Result<()> {
        let fds: Vec<RawFd> = sockets
            .iter()
            .map(|s| match s {
                Socket::Tcp(l) => l.as_raw_fd(),
                Socket::Unix(l) => l.as_raw_fd(),
            })
            .collect();
        let fd = self.s.as_raw_fd();
        let mut off = 0;
        let mut attach: &[RawFd] = &fds;
        while off < frame.len() {
            let rest = &frame[off..];
            let n = self
                .s
                .async_io(Interest::WRITABLE, || sendmsg(fd, rest, attach))
                .await?;
            if n == 0 {
                return Err(Error::Io(io::ErrorKind::WriteZero.into()));
            }
            // 只要发出去了一个字节，句柄就跟着它走了；后面的字节不再带
            attach = &[];
            off += n;
        }
        Ok(())
    }

    pub(crate) async fn read_some(&mut self, buf: &mut Vec<u8>) -> Result<usize> {
        buf.reserve(CHUNK);
        let fd = self.s.as_raw_fd();
        let spare = buf.spare_capacity_mut();
        // 地址按整数存：裸指针不是 Send，跨 await 拿着它会让整个 Link 的
        // future 不能 spawn
        let (dst, cap) = (spare.as_mut_ptr() as usize, spare.len());
        let (n, fds, truncated) = self
            .s
            .async_io(Interest::READABLE, || recvmsg(fd, dst as *mut u8, cap))
            .await?;
        // SAFETY: recvmsg 刚往 spare 的前 n 个字节里写了数据
        unsafe { buf.set_len(buf.len() + n) };
        if truncated {
            // 收到的那几个已经在 fds 里，随它一起关掉；这条连接上的句柄对不上了
            drop(fds);
            return Err(Error::Transfer(
                "the sockets did not fit the ancillary buffer".into(),
            ));
        }
        self.fds.extend(fds);
        Ok(n)
    }

    /// 认领这一帧的 n 个句柄，还原成监听。
    pub(crate) fn import(&mut self, n: usize, _w: &[String]) -> Result<Vec<Socket>> {
        if self.fds.len() < n {
            return Err(Error::Transfer(format!(
                "the message carries {n} sockets but {} arrived",
                self.fds.len()
            )));
        }
        let fds: Vec<OwnedFd> = self.fds.drain(..n).collect();
        fds.into_iter().map(socket_from_fd).collect()
    }
}

/// unix 上套接字走辅助消息，帧里不用带别的。
pub(crate) fn export(_sockets: &[&Socket], _peer_pid: u32) -> Result<Vec<String>> {
    Ok(Vec::new())
}

/// 按地址族把句柄还原成 TCP 或 unix 监听。
fn socket_from_fd(fd: OwnedFd) -> Result<Socket> {
    let raw = fd.as_raw_fd();
    let mut ty: libc::c_int = 0;
    let mut len = mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: 出参是一个 c_int，长度如实给出
    if unsafe {
        libc::getsockopt(
            raw,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut ty as *mut libc::c_int).cast(),
            &mut len,
        )
    } != 0
    {
        return Err(Error::Transfer(format!(
            "a received handle is not a socket: {}",
            io::Error::last_os_error()
        )));
    }
    if ty != libc::SOCK_STREAM {
        return Err(Error::Transfer(format!(
            "a received socket is not a stream socket (type {ty})"
        )));
    }
    // SAFETY: sockaddr_storage 全零是合法值；出参长度如实给出
    let mut addr: libc::sockaddr_storage = unsafe { mem::zeroed() };
    let mut len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    if unsafe {
        libc::getsockname(
            raw,
            (&mut addr as *mut libc::sockaddr_storage).cast(),
            &mut len,
        )
    } != 0
    {
        return Err(Error::Transfer(io::Error::last_os_error().to_string()));
    }
    match addr.ss_family as libc::c_int {
        libc::AF_INET | libc::AF_INET6 => Ok(Socket::Tcp(std::net::TcpListener::from(fd))),
        libc::AF_UNIX => Ok(Socket::Unix(std::os::unix::net::UnixListener::from(fd))),
        family => Err(Error::Transfer(format!(
            "a received socket has address family {family}"
        ))),
    }
}

fn sendmsg(fd: RawFd, data: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    // SAFETY: msghdr 全零是合法值（没有 name、没有 control）
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    let mut control: Vec<u64>;
    if !fds.is_empty() {
        let payload = mem::size_of_val(fds);
        // SAFETY: 两个宏只做算术
        let (space, cmsg_len) = unsafe {
            (
                libc::CMSG_SPACE(payload as libc::c_uint) as usize,
                libc::CMSG_LEN(payload as libc::c_uint) as usize,
            )
        };
        // 用 u64 数组，是为了让 cmsghdr 按它要的方式对齐
        control = vec![0u64; space.div_ceil(8)];
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        // SAFETY: control 至少 CMSG_SPACE(payload) 字节且已对齐，第一个头必然在里面；
        // CMSG_DATA 之后正好放得下 payload 字节
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = cmsg_len as _;
            std::ptr::copy_nonoverlapping(fds.as_ptr().cast::<u8>(), libc::CMSG_DATA(c), payload);
        }
    }
    // SAFETY: msg 指向的 iov、control 都活到这次调用结束
    let n = unsafe { libc::sendmsg(fd, &msg, SEND_FLAGS) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// 返回（读到的字节数，收到的句柄，辅助数据是否被截断）。
fn recvmsg(fd: RawFd, dst: *mut u8, cap: usize) -> io::Result<(usize, Vec<OwnedFd>, bool)> {
    let mut iov = libc::iovec {
        iov_base: dst.cast(),
        iov_len: cap,
    };
    // SAFETY: 只做算术
    let space =
        unsafe { libc::CMSG_SPACE((MAX_SOCKETS * mem::size_of::<libc::c_int>()) as libc::c_uint) }
            as usize;
    let mut control = vec![0u64; space.div_ceil(8)];
    // SAFETY: msghdr 全零是合法值
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: dst 往后 cap 个字节可写（调用方保证），control 活到调用结束
    let n = unsafe { libc::recvmsg(fd, &mut msg, RECV_FLAGS) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    // **先把收到的句柄都接住**，哪怕接下来要报错 —— 它们已经在本进程里了，
    // 不接住就漏掉，接住了出错时随 OwnedFd 一起关
    let mut fds = Vec::new();
    // SAFETY: 遍历内核刚填好的 control；每个头的长度由内核给出
    unsafe {
        let header = libc::CMSG_LEN(0) as usize;
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let count =
                    ((*c).cmsg_len as usize).saturating_sub(header) / mem::size_of::<libc::c_int>();
                let data = libc::CMSG_DATA(c).cast::<libc::c_int>();
                for i in 0..count {
                    let raw = std::ptr::read_unaligned(data.add(i));
                    fds.push(OwnedFd::from_raw_fd(raw));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    for f in &fds {
        // SAFETY: f 是刚收到的有效句柄
        unsafe { libc::fcntl(f.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    let truncated = msg.msg_flags & libc::MSG_CTRUNC != 0;
    Ok((n as usize, fds, truncated))
}
