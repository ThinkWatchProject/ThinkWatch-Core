//! 三种同步位置答同一套问题：只新增不覆盖、仅当不存在时创建、覆盖写、列目录跳过临时文件、
//! 删两次不报错、凭据不对是「拒绝访问」、连不上是「不可用」。
//!
//! 文件夹用临时目录，WebDAV 和 S3 用进程内的假服务（`common`）。

mod common;

use tw_sync::store::{self, Folder, S3, WebDav};
use tw_sync::{Store, StoreError};

const V1: &str = "versions/0000000001-aaaaaaaa.twconfig";
const V2: &str = "versions/0000000002-bbbbbbbb.twconfig";

async fn conformance(s: &impl Store) {
    assert!(
        s.list("versions").await.unwrap().is_empty(),
        "空的位置列出来是空的"
    );
    assert_eq!(s.get(V1).await.unwrap(), None);

    s.put_new(V1, b"one").await.unwrap();
    let e = s.put_new(V1, b"two").await.unwrap_err();
    assert!(matches!(e, StoreError::Exists { .. }), "{e:?}");
    assert_eq!(
        s.get(V1).await.unwrap().as_deref(),
        Some(&b"one"[..]),
        "只新增不覆盖"
    );

    s.put_new(V2, b"twotwo").await.unwrap();
    let names: Vec<(String, u64)> = s
        .list("versions")
        .await
        .unwrap()
        .into_iter()
        .map(|e| (e.name, e.size))
        .collect();
    assert_eq!(
        names,
        [
            ("0000000001-aaaaaaaa.twconfig".to_string(), 3),
            ("0000000002-bbbbbbbb.twconfig".to_string(), 6)
        ],
        "临时文件不出现在列表里"
    );

    assert!(s.create_exclusive("keyring", b"k1").await.unwrap());
    assert!(!s.create_exclusive("keyring", b"k2").await.unwrap());
    assert_eq!(s.get("keyring").await.unwrap().as_deref(), Some(&b"k1"[..]));
    s.put("keyring", b"k3").await.unwrap();
    assert_eq!(s.get("keyring").await.unwrap().as_deref(), Some(&b"k3"[..]));

    s.put("devices/aaaaaaaa", b"d1").await.unwrap();
    s.put("devices/aaaaaaaa", b"d2").await.unwrap();
    assert_eq!(
        s.get("devices/aaaaaaaa").await.unwrap().as_deref(),
        Some(&b"d2"[..])
    );
    assert_eq!(s.list("devices").await.unwrap().len(), 1);

    s.delete(V1).await.unwrap();
    s.delete(V1).await.unwrap();
    assert_eq!(s.get(V1).await.unwrap(), None);
    assert_eq!(s.list("versions").await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_folder_answers_like_the_others() {
    let d = tempfile::tempdir().unwrap();
    conformance(&Folder::new(d.path())).await;
    // 只剩正式的文件，没有半截的临时文件
    let left: Vec<String> = std::fs::read_dir(d.path().join("versions"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(left, ["0000000002-bbbbbbbb.twconfig"]);
}

#[tokio::test]
async fn webdav_answers_like_the_others() {
    let fake = common::webdav().await;
    let s = WebDav::new(
        &format!("{}/dav/root", fake.url()),
        common::USER,
        common::PASS,
        common::http(),
    )
    .unwrap();
    conformance(&s).await;
    assert!(
        fake.names().iter().all(|n| !n.contains(".tmp-")),
        "临时文件改名之后不该留下：{:?}",
        fake.names()
    );
}

#[tokio::test]
async fn webdav_without_move_or_conditional_put_still_never_overwrites_a_version() {
    let fake = common::webdav().await;
    fake.knobs
        .no_move
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let s = WebDav::new(
        &format!("{}/dav/root", fake.url()),
        common::USER,
        common::PASS,
        common::http(),
    )
    .unwrap();
    conformance(&s).await;
    // 连 If-None-Match 都不认的：keyring 先看一眼再写
    fake.knobs
        .no_conditional
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(!s.create_exclusive("keyring", b"other").await.unwrap());
    assert_eq!(fake.get("keyring").as_deref(), Some(&b"k3"[..]));
}

fn s3_store(fake: &common::Fake, ak: &str) -> S3 {
    s3_store_with(fake, ak, common::SK)
}

fn s3_store_with(fake: &common::Fake, ak: &str, sk: &str) -> S3 {
    S3::new(
        store::s3::Config {
            endpoint: fake.url(),
            region: "auto".into(),
            bucket: "tw-bucket".into(),
            prefix: "team/tw".into(),
            access_key_id: ak.into(),
            secret_access_key: sk.into(),
        },
        common::http(),
    )
    .unwrap()
}

/// 假 S3 核对签名的那一份按 AWS 文档里的两个示例对得上（GET Object、ListObjects 带查询参数）：
/// 它核对得了，`S3` 的签名在它那里过得去才算数
#[test]
fn the_fake_s3_checks_signatures_the_way_aws_does() {
    use axum::http::{HeaderMap, HeaderValue};
    let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let req = |auth: &str, range: bool| {
        let mut h = HeaderMap::new();
        h.insert(
            "host",
            HeaderValue::from_static("examplebucket.s3.amazonaws.com"),
        );
        if range {
            h.insert("range", HeaderValue::from_static("bytes=0-9"));
        }
        h.insert("x-amz-content-sha256", HeaderValue::from_static(empty));
        h.insert("x-amz-date", HeaderValue::from_static("20130524T000000Z"));
        h.insert("authorization", HeaderValue::from_str(auth).unwrap());
        h
    };
    let get = req(
        "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
         SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,\
         Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41",
        true,
    );
    common::sigv4(
        "GET",
        "/test.txt",
        "",
        &get,
        b"",
        "AKIAIOSFODNN7EXAMPLE",
        secret,
    )
    .unwrap();
    let list = req(
        "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
         SignedHeaders=host;x-amz-content-sha256;x-amz-date,\
         Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7",
        false,
    );
    common::sigv4(
        "GET",
        "/",
        "max-keys=2&prefix=J",
        &list,
        b"",
        "AKIAIOSFODNN7EXAMPLE",
        secret,
    )
    .unwrap();
    // 改一点就不认
    assert!(
        common::sigv4(
            "GET",
            "/",
            "max-keys=3&prefix=J",
            &list,
            b"",
            "AKIAIOSFODNN7EXAMPLE",
            secret
        )
        .is_err()
    );
    assert!(
        common::sigv4(
            "GET",
            "/test.txt",
            "",
            &get,
            b"",
            "AKIAIOSFODNN7EXAMPLE",
            "other"
        )
        .is_err()
    );
    assert!(
        common::sigv4(
            "GET",
            "/test.txt",
            "",
            &get,
            b"body",
            "AKIAIOSFODNN7EXAMPLE",
            secret
        )
        .is_err()
    );
}

/// 密钥 ID 对、密钥不对：签名对不上，拒绝访问
#[tokio::test]
async fn s3_with_the_wrong_secret_is_refused() {
    let fake = common::s3().await;
    assert!(matches!(
        s3_store_with(&fake, common::AK, "not-the-secret")
            .list("versions")
            .await
            .unwrap_err(),
        StoreError::AuthFailed(_)
    ));
    assert!(fake.names().is_empty());
}

#[tokio::test]
async fn s3_answers_like_the_others() {
    let fake = common::s3().await;
    conformance(&s3_store(&fake, common::AK)).await;
    assert!(
        fake.names().iter().all(|n| n.starts_with("team/tw/")),
        "键都在前缀底下：{:?}",
        fake.names()
    );
}

#[tokio::test]
async fn s3_without_conditional_writes_falls_back_to_looking_first() {
    let fake = common::s3().await;
    fake.knobs
        .no_conditional
        .store(true, std::sync::atomic::Ordering::Relaxed);
    conformance(&s3_store(&fake, common::AK)).await;
}

#[tokio::test]
async fn wrong_credentials_are_a_refusal_and_a_dead_server_is_unreachable() {
    let dav = common::webdav().await;
    let wrong = WebDav::new(
        &format!("{}/dav/root", dav.url()),
        common::USER,
        "wrong",
        common::http(),
    )
    .unwrap();
    assert!(matches!(
        wrong.list("versions").await.unwrap_err(),
        StoreError::AuthFailed(_)
    ));
    let s3 = common::s3().await;
    assert!(matches!(
        s3_store(&s3, "AKIDSOMEONEELSE").get(V1).await.unwrap_err(),
        StoreError::AuthFailed(_)
    ));
    dav.knobs
        .down
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let right = WebDav::new(
        &format!("{}/dav/root", dav.url()),
        common::USER,
        common::PASS,
        common::http(),
    )
    .unwrap();
    assert!(matches!(
        right.get(V1).await.unwrap_err(),
        StoreError::Unreachable(_)
    ));
    let gone = WebDav::new(
        &format!("{}/dav/root", common::nowhere()),
        common::USER,
        common::PASS,
        common::http(),
    )
    .unwrap();
    assert!(matches!(
        gone.list("versions").await.unwrap_err(),
        StoreError::Unreachable(_)
    ));
}

/// 服务端关掉了连接池里那条空闲连接（Apache 空闲 5 秒就关）：发出去的请求收到「连接在答复
/// 之前关了」，换一条连接再发一次就好。这里的服务端每条连接只答第一个请求，第二个请求一到
/// 就关连接 —— 没有重发的话第二次读一定失败
#[tokio::test]
async fn a_pooled_connection_the_server_closed_is_retried_once() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let conns = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let n = conns.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = l.accept().await else {
                return;
            };
            n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut served = 0;
                loop {
                    let mut chunk = [0u8; 4096];
                    let Ok(got) = sock.read(&mut chunk).await else {
                        return;
                    };
                    if got == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..got]);
                    while let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        buf.drain(..end + 4);
                        if served == 1 {
                            // 第二个请求：一个字也不答就关
                            return;
                        }
                        served += 1;
                        let _ = sock
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello")
                            .await;
                    }
                }
            });
        }
    });
    let s = WebDav::new(
        &format!("http://{addr}/dav/root"),
        common::USER,
        common::PASS,
        common::http(),
    )
    .unwrap();
    for _ in 0..3 {
        assert_eq!(s.get(V1).await.unwrap().as_deref(), Some(&b"hello"[..]));
    }
    assert!(
        conns.load(std::sync::atomic::Ordering::Relaxed) >= 3,
        "每次都换了一条连接"
    );
}
