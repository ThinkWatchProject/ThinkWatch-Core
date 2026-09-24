//! 握手和传输，两端都在同一个进程里，中间是一根内存管道。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};
use tw_api::CONTROL_API_VERSION;

use super::*;

fn key(b: u8) -> ControlKey {
    ControlKey::from_bytes([b; 32])
}

fn acceptor(k: ControlKey) -> Acceptor {
    Acceptor::new(move || Some(k.clone()), "2026.9.30")
}

/// 两端同时握手，各自拿到结果。
async fn pair(
    client_key: ControlKey,
    server: Acceptor,
) -> (
    Result<(SecureStream<DuplexStream>, ServerHello), LinkError>,
    Result<Accepted<DuplexStream>, LinkError>,
) {
    let (a, b) = duplex(64 * 1024);
    let c = tokio::spawn(async move { connect(a, &client_key, "lite 2026.9.30").await });
    let s = server.accept(b).await;
    (c.await.unwrap(), s)
}

async fn linked() -> (SecureStream<DuplexStream>, SecureStream<DuplexStream>) {
    let (c, s) = pair(key(7), acceptor(key(7))).await;
    let (c, hello) = c.unwrap();
    let s = s.unwrap();
    assert!(hello.accept);
    assert_eq!(hello.core, "2026.9.30");
    assert_eq!(hello.proto, CONTROL_API_VERSION);
    assert_eq!(s.hello.app, "lite 2026.9.30");
    assert_eq!(s.key, key(7));
    (c, s.stream)
}

#[tokio::test]
async fn a_round_trip_carries_bytes_both_ways() {
    let (mut c, mut s) = linked().await;
    c.write_all(b"GET /status HTTP/1.1\r\n\r\n").await.unwrap();
    c.flush().await.unwrap();
    let mut got = [0u8; 24];
    s.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"GET /status HTTP/1.1\r\n\r\n");

    s.write_all(b"HTTP/1.1 200 OK\r\n").await.unwrap();
    s.flush().await.unwrap();
    let mut got = [0u8; 17];
    c.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"HTTP/1.1 200 OK\r\n");
}

/// 比一帧大得多的正文要拆成几帧，拼回来一个字节不差。**两个方向同时写**：
/// 管道只有 64 KB，一边光写不读就会卡住另一边 —— 真实的 socket 也一样。
#[tokio::test]
async fn a_large_body_is_split_into_frames_and_arrives_whole() {
    let (c, s) = linked().await;
    let body: Vec<u8> = (0..3_000_000u32).map(|i| (i * 31 % 251) as u8).collect();
    let (mut cr, mut cw) = tokio::io::split(c);
    let (mut sr, mut sw) = tokio::io::split(s);

    let up = body.clone();
    let writer = tokio::spawn(async move {
        cw.write_all(&up).await.unwrap();
        cw.shutdown().await.unwrap();
    });
    let down = body.clone();
    let echo = tokio::spawn(async move {
        sw.write_all(&down).await.unwrap();
        sw.shutdown().await.unwrap();
    });
    let mut at_server = Vec::new();
    sr.read_to_end(&mut at_server).await.unwrap();
    let mut at_client = Vec::new();
    cr.read_to_end(&mut at_client).await.unwrap();
    writer.await.unwrap();
    echo.await.unwrap();
    assert_eq!(at_server.len(), body.len());
    assert!(at_server == body, "服务端收到的和发出的不一样");
    assert!(at_client == body, "客户端收到的和发出的不一样");
}

/// 一连串很小的写，每一段都是一帧，读的一方照样按顺序拿全。
#[tokio::test]
async fn many_small_writes_arrive_in_order() {
    let (c, s) = linked().await;
    let (_cr, mut cw) = tokio::io::split(c);
    let (mut sr, _sw) = tokio::io::split(s);
    let writer = tokio::spawn(async move {
        for i in 0..20_000u32 {
            cw.write_all(&i.to_be_bytes()[1..]).await.unwrap();
        }
        cw.shutdown().await.unwrap();
    });
    let mut got = Vec::new();
    sr.read_to_end(&mut got).await.unwrap();
    writer.await.unwrap();
    assert_eq!(got.len(), 20_000 * 3);
    for (i, chunk) in got.chunks(3).enumerate() {
        assert_eq!(chunk, &(i as u32).to_be_bytes()[1..], "第 {i} 段");
    }
}

/// 读的一方给的缓冲比一帧小：一帧的明文分几次读完，不丢不重。
#[tokio::test]
async fn a_frame_can_be_read_a_few_bytes_at_a_time() {
    let (mut c, mut s) = linked().await;
    c.write_all(&[9u8; 1000]).await.unwrap();
    c.flush().await.unwrap();
    let mut total = 0;
    let mut buf = [0u8; 7];
    while total < 1000 {
        let n = s.read(&mut buf).await.unwrap();
        assert!(n > 0);
        assert!(buf[..n].iter().all(|b| *b == 9));
        total += n;
    }
    assert_eq!(total, 1000);
}

#[tokio::test]
async fn a_wrong_key_is_told_apart_on_both_sides() {
    let (c, s) = pair(key(1), acceptor(key(2))).await;
    assert!(matches!(c, Err(LinkError::WrongKey)), "{:?}", c.err());
    assert!(matches!(s, Err(LinkError::WrongKey)), "{:?}", s.err());
}

/// 此刻没有钥匙（配置里还没有）：谁来都拒。
#[tokio::test]
async fn with_no_key_in_effect_everyone_is_turned_away() {
    let a = Acceptor::new(|| None, "x");
    let (c, s) = pair(key(1), a).await;
    assert!(matches!(c, Err(LinkError::WrongKey)));
    assert!(matches!(s, Err(LinkError::WrongKey)));
}

/// 钥匙是每次握手现取的：换了之后，旧钥匙进不来、新钥匙进得来。
#[tokio::test]
async fn a_rotated_key_takes_effect_on_the_next_handshake() {
    let current = Arc::new(Mutex::new(key(1)));
    let src = current.clone();
    let a = Acceptor::new(move || Some(src.lock().unwrap().clone()), "x");
    let (c, _) = pair(key(1), a.clone()).await;
    assert!(c.is_ok());
    *current.lock().unwrap() = key(2);
    let (c, s) = pair(key(1), a.clone()).await;
    assert!(matches!(c, Err(LinkError::WrongKey)));
    assert!(matches!(s, Err(LinkError::WrongKey)));
    let (c, s) = pair(key(2), a).await;
    assert!(c.is_ok());
    assert_eq!(s.unwrap().key, key(2));
}

/// 版本不一致：core 回 `accept: false` 就断开，两边各自拿到两个版本号。
#[tokio::test]
async fn a_version_mismatch_names_both_versions() {
    let (a, b) = duplex(64 * 1024);
    let k = key(3);
    let c =
        tokio::spawn(
            async move { handshake::client(a, &k, "lite old", CONTROL_API_VERSION - 1).await },
        );
    let s = acceptor(key(3)).accept(b).await;
    match c.await.unwrap() {
        Err(LinkError::VersionMismatch {
            ours,
            theirs,
            peer_version,
        }) => {
            assert_eq!(ours, CONTROL_API_VERSION - 1);
            assert_eq!(theirs, CONTROL_API_VERSION);
            assert_eq!(peer_version, "2026.9.30");
        }
        other => panic!("{:?}", other.err()),
    }
    match s {
        Err(LinkError::VersionMismatch {
            ours,
            theirs,
            peer_version,
        }) => {
            assert_eq!(ours, CONTROL_API_VERSION);
            assert_eq!(theirs, CONTROL_API_VERSION - 1);
            assert_eq!(peer_version, "lite old");
        }
        other => panic!("{:?}", other.err()),
    }
}

/// 对面一声不吭：客户端按时放弃。
#[tokio::test(start_paused = true)]
async fn a_silent_server_times_out() {
    let (a, _b) = duplex(1024);
    let r = connect(a, &key(1), "x").await;
    assert!(matches!(r, Err(LinkError::Timeout)), "{:?}", r.err());
}

/// 连上来就不说话的连接不能一直占着服务端。
#[tokio::test(start_paused = true)]
async fn a_silent_client_times_out() {
    let (_a, b) = duplex(1024);
    let r = acceptor(key(1)).accept(b).await;
    assert!(matches!(r, Err(LinkError::Timeout)), "{:?}", r.err());
}

/// 对面读完第一条什么都不回就关了（远程端口上被放行名单挡住的样子）。
#[tokio::test]
async fn a_server_that_hangs_up_is_closed_not_wrong_key() {
    let (a, mut b) = duplex(64 * 1024);
    let server = tokio::spawn(async move {
        let mut buf = [0u8; 256];
        let _ = b.read(&mut buf).await;
        drop(b);
    });
    let r = connect(a, &key(1), "x").await;
    server.await.unwrap();
    assert!(matches!(r, Err(LinkError::Closed)), "{:?}", r.err());
}

#[tokio::test]
async fn a_client_that_hangs_up_before_speaking_is_closed() {
    let (a, b) = duplex(1024);
    drop(a);
    let r = acceptor(key(1)).accept(b).await;
    assert!(matches!(r, Err(LinkError::Closed)), "{:?}", r.err());
}

/// 拿 curl 直接敲过来：服务端照钥匙不对回一个字节，不去等 18 KB。
#[tokio::test]
async fn plain_http_gets_the_reject_byte() {
    let (mut a, b) = duplex(64 * 1024);
    let s = tokio::spawn(async move { acceptor(key(1)).accept(b).await });
    a.write_all(b"GET /status HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut got = Vec::new();
    a.read_to_end(&mut got).await.unwrap();
    assert_eq!(got, [REJECT]);
    assert!(matches!(s.await.unwrap(), Err(LinkError::WrongKey)));
}

/// 半关：客户端关了写的一侧，服务端在帧边界上读到结尾（0 字节，不是错误），
/// 之后照样能回话；服务端关了，客户端也读到结尾。
#[tokio::test]
async fn half_close_is_an_orderly_end_of_stream() {
    let (mut c, mut s) = linked().await;
    c.write_all(b"last words").await.unwrap();
    c.shutdown().await.unwrap();
    let mut got = Vec::new();
    s.read_to_end(&mut got).await.unwrap();
    assert_eq!(got, b"last words");
    // 读到结尾之后再读还是结尾
    let mut more = [0u8; 4];
    assert_eq!(s.read(&mut more).await.unwrap(), 0);

    s.write_all(b"reply").await.unwrap();
    s.shutdown().await.unwrap();
    let mut got = Vec::new();
    c.read_to_end(&mut got).await.unwrap();
    assert_eq!(got, b"reply");
}

/// 一帧读到一半对面就没了：这不是正常结束，要报错。
#[tokio::test]
async fn a_frame_cut_short_is_an_error_not_an_end() {
    let (c, s) = pair_raw().await;
    let (mut client, raw) = (c, s);
    // 直接往底层写半个帧头 + 几个字节，然后关掉
    let mut raw = raw;
    raw.write_all(&[0x00, 0x40, 1, 2, 3]).await.unwrap();
    drop(raw);
    let mut buf = [0u8; 16];
    let e = client.read(&mut buf).await.unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);
}

/// 被改过的帧解不开：读的一方报错，不交出任何明文。
#[tokio::test]
async fn a_tampered_frame_is_refused() {
    let (mut client, mut raw) = pair_raw().await;
    // 一帧 20 字节的「密文」：长度头对，内容是乱写的
    let mut frame = vec![0x00, 20];
    frame.extend_from_slice(&[0xAB; 20]);
    raw.write_all(&frame).await.unwrap();
    let mut buf = [0u8; 16];
    let e = client.read(&mut buf).await.unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
}

/// 握手完成之后，服务端那一侧换成原始的管道，好往里写坏数据。
async fn pair_raw() -> (SecureStream<DuplexStream>, DuplexStream) {
    let (c, s) = pair(key(5), acceptor(key(5))).await;
    let (c, _) = c.unwrap();
    let s = s.unwrap();
    // 拿回底层的管道：SecureStream 不交出可写的引用，这里只能借结构体拆开
    let raw = into_inner(s.stream);
    (c, raw)
}

fn into_inner(s: SecureStream<DuplexStream>) -> DuplexStream {
    s.into_inner_for_tests()
}

/// 限时可以自己定。
#[tokio::test(start_paused = true)]
async fn the_limit_can_be_set() {
    let (a, _b) = duplex(1024);
    let started = tokio::time::Instant::now();
    let r = connect_with_timeout(a, &key(1), "x", Duration::from_millis(200)).await;
    assert!(matches!(r, Err(LinkError::Timeout)));
    assert!(started.elapsed() < Duration::from_secs(1));
}
