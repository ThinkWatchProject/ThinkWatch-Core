use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use super::*;

/// unix socket 的路径有长度上限（macOS 104 字节），系统临时目录可能很深，
/// 所以 unix 上固定放 /tmp 底下。
fn tempdir() -> tempfile::TempDir {
    #[cfg(unix)]
    return tempfile::Builder::new()
        .prefix("twho")
        .tempdir_in("/tmp")
        .unwrap();
    #[cfg(not(unix))]
    return tempfile::tempdir().unwrap();
}

/// 开端点、连上去，返回（旧 core 那一头，新 core 那一头）。
async fn pair(dir: &Path) -> (Endpoint, Link, Link) {
    let ep = Endpoint::bind(dir).await.unwrap();
    let (old, new) = tokio::join!(ep.accept(), connect(dir));
    (ep, old.unwrap(), new.unwrap().expect("armed"))
}

/// Windows 上 `Socket` 只有 TCP 一种，模式匹配是不可反驳的。
#[allow(irrefutable_let_patterns)]
fn into_tcp(s: Socket) -> TcpListener {
    let Socket::Tcp(l) = s else {
        panic!("expected a TCP listener")
    };
    l
}

fn me() -> u32 {
    std::process::id()
}

fn hello() -> Message {
    Message::Hello {
        version: VERSION,
        core_version: "0.0.0-test".into(),
        pid: me(),
    }
}

/// 在收到的监听上把排队的连接都接出来，读出每个连接发来的那一个字节。
fn drain_ids(l: &TcpListener, want: usize) -> BTreeSet<u8> {
    l.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ids = BTreeSet::new();
    while ids.len() < want {
        match l.accept() {
            Ok((mut s, _)) => {
                s.set_nonblocking(false).unwrap();
                let mut b = [0u8];
                s.read_exact(&mut b).unwrap();
                ids.insert(b[0]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "only {} of {want} connections arrived",
                    ids.len()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("{e}"),
        }
    }
    ids
}

#[tokio::test]
async fn not_armed_is_none() {
    let dir = tempdir();
    assert!(connect(dir.path()).await.unwrap().is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn stale_socket_file_is_none() {
    let dir = tempdir();
    let path = dir.path().join(ENDPOINT_FILE);
    // 绑过、没人在听了，文件还在 —— 上次没退干净的样子
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(path.exists());
    assert!(connect(dir.path()).await.unwrap().is_none());
    // 重新 bind 时把残留删掉
    let ep = Endpoint::bind(dir.path()).await.unwrap();
    let (old, new) = tokio::join!(ep.accept(), connect(dir.path()));
    assert!(old.is_ok() && new.unwrap().is_some());
}

#[tokio::test]
async fn bind_connect_close() {
    let dir = tempdir();
    let (ep, old, new) = pair(dir.path()).await;
    let file = dir.path().join(ENDPOINT_FILE);
    assert!(file.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // 同一进程：两头问到的都是自己
        assert_eq!(old.peer_pid(), Some(me()));
        assert_eq!(new.peer_pid(), Some(me()));
    }
    #[cfg(windows)]
    {
        assert!(dir.path().join(TOKEN_FILE).exists());
        assert_eq!(old.peer_pid(), None);
    }
    drop((old, new));
    ep.close();
    assert!(!file.exists());
    #[cfg(windows)]
    assert!(!dir.path().join(TOKEN_FILE).exists());
    assert!(connect(dir.path()).await.unwrap().is_none());
}

#[tokio::test]
async fn closing_does_not_remove_a_newer_endpoint() {
    // 排空中的旧 core 关端点时，新 core 可能已经在同一路径上开了自己的（H8）
    let dir = tempdir();
    let first = Endpoint::bind(dir.path()).await.unwrap();
    let second = Endpoint::bind(dir.path()).await.unwrap();
    first.close();
    assert!(dir.path().join(ENDPOINT_FILE).exists());
    let (old, new) = tokio::join!(second.accept(), connect(dir.path()));
    assert!(old.is_ok() && new.unwrap().is_some());
    second.close();
    assert!(!dir.path().join(ENDPOINT_FILE).exists());
}

#[tokio::test]
async fn messages_round_trip() {
    let dir = tempdir();
    let (_ep, mut old, mut new) = pair(dir.path()).await;

    new.send(&hello()).await.unwrap();
    let (m, s) = old.recv().await.unwrap();
    assert_eq!(m, hello());
    assert!(s.is_empty());
    // Windows 上 pid 从 Hello 来
    assert_eq!(old.peer_pid(), Some(me()));

    // 大快照：一帧要分好几次读
    let big = serde_json::json!({ "affinity": "x".repeat(3 << 20) });
    let from_old = [
        Message::Refused {
            reason: "version".into(),
        },
        Message::Control {
            info: ListenerInfo {
                role: Role::Control,
                want: None,
            },
        },
        Message::Snapshot { data: big },
        Message::Listeners {
            infos: vec![
                ListenerInfo {
                    role: Role::Gateway,
                    want: Some("127.0.0.1:8080".parse().unwrap()),
                },
                ListenerInfo {
                    role: Role::Remote,
                    want: Some("[::]:9000".parse().unwrap()),
                },
            ],
        },
        Message::Settled {
            data: serde_json::json!({ "key": "k", "cost": 1.5 }),
        },
        Message::InFlight { n: 3 },
        Message::Bye,
    ];
    let from_new = [Message::WantListeners, Message::Commit, Message::Fallback];

    // 一口气发完再收：多帧挤在一次读里也要拆得对
    let send = async {
        for m in &from_old {
            old.send(m).await.unwrap();
        }
        old
    };
    let recv = async {
        let mut got = Vec::new();
        for _ in 0..from_old.len() {
            got.push(new.recv().await.unwrap().0);
        }
        (new, got)
    };
    let (mut old, (mut new, got)) = tokio::join!(send, recv);
    assert_eq!(got, from_old);

    for m in &from_new {
        new.send(m).await.unwrap();
    }
    for m in &from_new {
        assert_eq!(&old.recv().await.unwrap().0, m);
    }

    drop(new);
    match old.recv().await {
        Err(Error::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
        other => panic!("expected EOF, got {other:?}"),
    }
}

/// **关键性质**：排在队列里、还没被 accept 的连接，随套接字一起交过去，一个不丢；
/// 发送方交出之后自己照样能 accept，关掉自己的句柄也不影响接收方。
#[tokio::test]
async fn queued_tcp_connections_survive_the_handover() {
    let dir = tempdir();
    let (_ep, mut old, mut new) = pair(dir.path()).await;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    const N: u8 = 6;
    let clients: Vec<TcpStream> = (0..N)
        .map(|i| {
            let mut c = TcpStream::connect(addr).unwrap();
            c.write_all(&[i]).unwrap();
            c
        })
        .collect();

    let socket = Socket::Tcp(listener);
    let infos = vec![ListenerInfo {
        role: Role::Gateway,
        want: Some(addr),
    }];
    old.send_with(
        &Message::Listeners {
            infos: infos.clone(),
        },
        &[&socket],
        me(),
    )
    .await
    .unwrap();
    let (m, mut sockets) = new.recv().await.unwrap();
    assert_eq!(m, Message::Listeners { infos });
    assert_eq!(sockets.len(), 1);
    let received = into_tcp(sockets.pop().unwrap());
    assert_eq!(received.local_addr().unwrap(), addr);

    // 发送方的句柄不受影响：它还能接一个
    let original = into_tcp(socket);
    let (mut s, _) = original.accept().unwrap();
    let mut b = [0u8];
    s.read_exact(&mut b).unwrap();
    drop(original);

    let mut ids = drain_ids(&received, (N - 1) as usize);
    ids.insert(b[0]);
    assert_eq!(ids, (0..N).collect::<BTreeSet<_>>());

    // 交接之后来的新连接也落在接收方
    let mut late = TcpStream::connect(addr).unwrap();
    late.write_all(&[N]).unwrap();
    assert_eq!(drain_ids(&received, 1), BTreeSet::from([N]));
    drop(clients);
}

#[cfg(unix)]
#[tokio::test]
async fn queued_unix_connections_survive_the_handover() {
    use std::os::unix::net::{UnixListener, UnixStream};

    let dir = tempdir();
    let (_ep, mut old, mut new) = pair(dir.path()).await;

    let path = dir.path().join("control.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let clients: Vec<UnixStream> = (0..4u8)
        .map(|i| {
            let mut c = UnixStream::connect(&path).unwrap();
            c.write_all(&[i]).unwrap();
            c
        })
        .collect();

    let socket = Socket::Unix(listener);
    let info = ListenerInfo {
        role: Role::Control,
        want: None,
    };
    old.send_with(&Message::Control { info }, &[&socket], 0)
        .await
        .unwrap();
    drop(socket);
    let (_, mut sockets) = new.recv().await.unwrap();
    let Some(Socket::Unix(received)) = sockets.pop() else {
        panic!("expected a unix listener");
    };
    let mut ids = BTreeSet::new();
    for _ in 0..4 {
        let (mut s, _) = received.accept().unwrap();
        let mut b = [0u8];
        s.read_exact(&mut b).unwrap();
        ids.insert(b[0]);
    }
    assert_eq!(ids, (0..4).collect::<BTreeSet<_>>());
    drop(clients);
}

/// 几个套接字一帧发、前后夹着不带套接字的消息：顺序和归属都要对得上。
#[tokio::test]
async fn several_sockets_keep_their_order() {
    let dir = tempdir();
    let (_ep, mut old, mut new) = pair(dir.path()).await;

    let a = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let b = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let c = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addrs: Vec<_> = [&a, &b, &c]
        .iter()
        .map(|l| l.local_addr().unwrap())
        .collect();
    let (a, b, c) = (Socket::Tcp(a), Socket::Tcp(b), Socket::Tcp(c));

    old.send(&Message::InFlight { n: 1 }).await.unwrap();
    old.send_with(&Message::Commit, &[&a, &b], me())
        .await
        .unwrap();
    old.send(&Message::InFlight { n: 2 }).await.unwrap();
    old.send_with(&Message::Bye, &[&c], me()).await.unwrap();

    let port = |s: &Socket| match s {
        Socket::Tcp(l) => l.local_addr().unwrap(),
        #[cfg(unix)]
        Socket::Unix(_) => panic!("expected TCP"),
    };
    let (m, s) = new.recv().await.unwrap();
    assert_eq!((m, s.len()), (Message::InFlight { n: 1 }, 0));
    let (m, s) = new.recv().await.unwrap();
    assert_eq!(m, Message::Commit);
    assert_eq!(s.iter().map(port).collect::<Vec<_>>(), addrs[..2]);
    let (m, s) = new.recv().await.unwrap();
    assert_eq!((m, s.len()), (Message::InFlight { n: 2 }, 0));
    let (m, s) = new.recv().await.unwrap();
    assert_eq!(m, Message::Bye);
    assert_eq!(s.iter().map(port).collect::<Vec<_>>(), addrs[2..]);
}

#[tokio::test]
async fn oversized_message_is_not_sent() {
    let dir = tempdir();
    let (_ep, mut old, _new) = pair(dir.path()).await;
    let data = serde_json::Value::String("x".repeat(MAX_FRAME));
    assert!(matches!(
        old.send(&Message::Snapshot { data }).await,
        Err(Error::Malformed(_))
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn oversized_frame_is_rejected() {
    use tokio::io::AsyncWriteExt;

    let dir = tempdir();
    let ep = Endpoint::bind(dir.path()).await.unwrap();
    let raw = tokio::net::UnixStream::connect(dir.path().join(ENDPOINT_FILE));
    let (old, raw) = tokio::join!(ep.accept(), raw);
    let (mut old, mut raw) = (old.unwrap(), raw.unwrap());
    raw.write_all(&((MAX_FRAME as u32) + 1).to_be_bytes())
        .await
        .unwrap();
    assert!(matches!(old.recv().await, Err(Error::Malformed(_))));
}

#[cfg(unix)]
#[tokio::test]
async fn garbage_is_malformed() {
    use tokio::io::AsyncWriteExt;

    let dir = tempdir();
    let ep = Endpoint::bind(dir.path()).await.unwrap();
    let raw = tokio::net::UnixStream::connect(dir.path().join(ENDPOINT_FILE));
    let (old, raw) = tokio::join!(ep.accept(), raw);
    let (mut old, mut raw) = (old.unwrap(), raw.unwrap());
    let body = br#"{"m":{"t":"no_such_message"}}"#;
    raw.write_all(&(body.len() as u32).to_be_bytes())
        .await
        .unwrap();
    raw.write_all(body).await.unwrap();
    assert!(matches!(old.recv().await, Err(Error::Malformed(_))));
}

#[test]
fn token_comparison() {
    assert!(token::eq(b"abcd", b"abcd"));
    assert!(!token::eq(b"abcd", b"abce"));
    assert!(!token::eq(b"abcd", b"abc"));
    assert!(!token::eq(b"", b"a"));
    assert!(token::eq(b"", b""));
}

#[test]
fn hex_round_trip() {
    let bytes: Vec<u8> = (0..=255).collect();
    let s = token::hex(&bytes);
    assert_eq!(s.len(), 512);
    assert_eq!(token::unhex(&s).unwrap(), bytes);
    assert_eq!(token::unhex("0aFf").unwrap(), vec![0x0a, 0xff]);
    assert!(token::unhex("abc").is_none());
    assert!(token::unhex("zz").is_none());
}

/// 网关会把连接放进 spawn 出去的任务里用：这几个 future 必须是 Send。
#[allow(dead_code)]
fn futures_are_send(ep: &Endpoint, link: &mut Link, dir: &Path) {
    fn send<T: Send>(_: T) {}
    send(Endpoint::bind(dir));
    send(ep.accept());
    send(connect(dir));
    send(link.recv());
}

#[allow(dead_code)]
fn send_with_is_send(link: &mut Link, s: &Socket) {
    fn send<T: Send>(_: T) {}
    send(link.send_with(&Message::Bye, &[s], 0));
}

/// 连上端点但什么都不说：只是一条原始连接。
#[cfg(unix)]
async fn raw_connect(dir: &Path) -> tokio::net::UnixStream {
    tokio::net::UnixStream::connect(dir.join(ENDPOINT_FILE))
        .await
        .unwrap()
}

#[cfg(windows)]
async fn raw_connect(dir: &Path) -> tokio::net::TcpStream {
    let port: u16 = std::fs::read_to_string(dir.join(ENDPOINT_FILE))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap()
}

fn frame(body: &[u8]) -> Vec<u8> {
    let mut f = (body.len() as u32).to_be_bytes().to_vec();
    f.extend_from_slice(body);
    f
}

/// 口令门**并发**核对：一堆连上来不说话的（比同时核对的上限还多）、一个交错口令的，
/// 都挡不住随后交对口令的那一个，也不会被错放进来。Windows 的门就是这个，这里在
/// 各个平台上都用它跑一遍。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_gate_is_not_held_up() {
    use tokio::io::AsyncWriteExt;

    let dir = tempdir();
    let ep = std::sync::Arc::new(Endpoint::bind(dir.path()).await.unwrap());
    let gate = Gate::Token {
        token: std::sync::Arc::from(&b"secret"[..]),
        wait: Duration::from_secs(5),
    };
    let start = Instant::now();
    let admitted = tokio::spawn({
        let ep = ep.clone();
        async move { ep.admit(gate).await }
    });

    let mut silent = Vec::new();
    for _ in 0..(MAX_CHECKS + 36) {
        silent.push(raw_connect(dir.path()).await);
    }
    let mut wrong = raw_connect(dir.path()).await;
    wrong.write_all(&frame(b"guess!")).await.unwrap();
    wrong
        .write_all(&frame(br#"{"m":{"t":"fallback"}}"#))
        .await
        .unwrap();

    let mut right = raw_connect(dir.path()).await;
    right.write_all(&frame(b"secret")).await.unwrap();
    right
        .write_all(&frame(br#"{"m":{"t":"bye"}}"#))
        .await
        .unwrap();

    let mut link = tokio::time::timeout(Duration::from_secs(3), admitted)
        .await
        .expect("held up by silent connections")
        .unwrap()
        .unwrap();
    assert!(start.elapsed() < Duration::from_secs(3));
    // 放进来的是交对口令的那一个
    assert_eq!(link.recv().await.unwrap().0, Message::Bye);
    drop((silent, wrong));
}

/// 真端点上的同一件事：Windows 上门是口令，unix 上门由内核当场判定。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accept_is_not_held_up_by_silent_connections() {
    let dir = tempdir();
    let ep = std::sync::Arc::new(Endpoint::bind(dir.path()).await.unwrap());
    let accepted = tokio::spawn({
        let ep = ep.clone();
        async move { ep.accept().await }
    });
    #[cfg(windows)]
    let silent = {
        let mut v = Vec::new();
        for _ in 0..(MAX_CHECKS + 36) {
            v.push(raw_connect(dir.path()).await);
        }
        v
    };
    let mut new = connect(dir.path()).await.unwrap().expect("armed");
    new.send(&hello()).await.unwrap();
    let mut old = tokio::time::timeout(Duration::from_secs(3), accepted)
        .await
        .expect("held up")
        .unwrap()
        .unwrap();
    assert_eq!(old.recv().await.unwrap().0, hello());
    #[cfg(windows)]
    drop(silent);
}

/// Windows：端点的监听、还原出来的监听都不能被子进程继承。
#[cfg(windows)]
#[tokio::test]
async fn sockets_are_not_inheritable() {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT};

    fn inheritable(raw: std::os::windows::io::RawSocket) -> bool {
        let mut flags = 0u32;
        // SAFETY: raw 是有效的套接字句柄，出参是本地变量
        assert_ne!(
            unsafe { GetHandleInformation(raw as HANDLE, &mut flags) },
            0
        );
        flags & HANDLE_FLAG_INHERIT != 0
    }

    let dir = tempdir();
    let (ep, mut old, mut new) = pair(dir.path()).await;
    assert!(!inheritable(ep.listener.raw()));

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let socket = Socket::Tcp(listener);
    old.send_with(&Message::Commit, &[&socket], me())
        .await
        .unwrap();
    let (_, mut sockets) = new.recv().await.unwrap();
    let received = into_tcp(sockets.pop().unwrap());
    assert!(!inheritable(received.as_raw_socket()));
}
