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
    S3::new(
        store::s3::Config {
            endpoint: fake.url(),
            region: "auto".into(),
            bucket: "tw-bucket".into(),
            prefix: "team/tw".into(),
            access_key_id: ak.into(),
            secret_access_key: common::SK.into(),
        },
        common::http(),
    )
    .unwrap()
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
