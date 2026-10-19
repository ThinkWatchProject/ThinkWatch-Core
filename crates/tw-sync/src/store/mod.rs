//! 同步位置的存储：文件夹、WebDAV、S3 兼容存储，**同一套五个动作**。
//!
//! 同步只追加新版本、不覆盖已有文件，所以不依赖存储服务的条件写入：版本的文件名
//! （序号-设备标识）由写它的那台设备独占，两台设备永远不会去写同一个名字。只有两样东西
//! 不是这样：
//!
//! - `keyring` **仅当不存在时创建**（[`Store::create_exclusive`]）：两台设备同时开启加密同步时，
//!   后写入的一方要知道自己晚了一步，转入加入流程；
//! - 设备记录和改口令后的 keyring 要覆盖（[`Store::put`]）：设备记录只有它自己那台设备写。
//!
//! 路径一律相对同步位置的根，用 `/` 分隔，只有两层（`versions/<名字>`、`devices/<名字>`、
//! `keyring`）。名字由这个 crate 生成，只含 ASCII 字母、数字、连字符和点。

use std::future::Future;

use tw_types::{Msg, msg};

pub mod folder;
pub mod s3;
pub mod webdav;
mod xml;

pub use folder::Folder;
pub use s3::S3;
pub use webdav::WebDav;

/// 目录里的一个文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub size: u64,
}

/// 一次存储操作没成。
///
/// **按「接下来怎么办」分类**，不按出错的那一层：界面上同步位置不可用（等它恢复）、
/// 拒绝访问（改连接参数）是两种不同的提示。
#[derive(Debug, Clone, thiserror::Error)]
pub enum StoreError {
    /// 连不上、超时、服务端 5xx。**过一会儿再试**
    #[error("{}", .0.text)]
    Unreachable(Msg),
    /// 401 / 403：用户名、密码、密钥不对，或者没有这个位置的权限
    #[error("{}", .0.text)]
    AuthFailed(Msg),
    /// `put_new`：这个名字已经有文件了
    #[error("{}", self.msg())]
    Exists { path: String },
    /// 别的：服务端回了一个看不懂的答复、本机磁盘写不进去
    #[error("{}", .0.text)]
    Other(Msg),
}

impl StoreError {
    pub fn msg(&self) -> Msg {
        match self {
            StoreError::Unreachable(m) | StoreError::AuthFailed(m) | StoreError::Other(m) => {
                m.clone()
            }
            StoreError::Exists { path } => msg!(
                "sync.store.exists", path = path =>
                "the sync location already has {path}"
            ),
        }
    }
}

/// 同步位置。实现见 [`Folder`]、[`WebDav`]、[`S3`]；运行时按配置选一个，用 [`AnyStore`]。
pub trait Store: Send + Sync {
    /// 一个目录里的文件（不递归，不含子目录）。目录不存在是空的，不是错误
    fn list(&self, dir: &str) -> impl Future<Output = Result<Vec<Entry>, StoreError>> + Send;
    /// 读一个文件。不存在是 `None`
    fn get(&self, path: &str) -> impl Future<Output = Result<Option<Vec<u8>>, StoreError>> + Send;
    /// **只新增**：已有同名文件时是 [`StoreError::Exists`]，原来那个不动。写的过程中别的设备
    /// 读不到半个文件（文件夹和 WebDAV 先写临时名再改名）
    fn put_new(
        &self,
        path: &str,
        bytes: &[u8],
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// 覆盖写。只用于设备记录（只有它自己那台设备写）和改口令后的 keyring
    fn put(&self, path: &str, bytes: &[u8]) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// 删掉。本来就没有不算错
    fn delete(&self, path: &str) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// 仅当不存在时创建：建成是 `true`，已经有了是 `false`（原来那个不动）
    fn create_exclusive(
        &self,
        path: &str,
        bytes: &[u8],
    ) -> impl Future<Output = Result<bool, StoreError>> + Send;
}

/// 按配置选出来的那一种。
pub enum AnyStore {
    Folder(Folder),
    WebDav(WebDav),
    S3(S3),
}

impl AnyStore {
    /// 按 `sync.target` 建一个。`http` 是发 WebDAV、S3 请求用的客户端（带着出站代理），
    /// 文件夹用不到它
    pub fn new(
        target: &tw_config::sync::Target,
        http: reqwest::Client,
    ) -> Result<Self, StoreError> {
        use tw_config::sync::Target;
        Ok(match target {
            Target::Folder { path, .. } => AnyStore::Folder(Folder::new(path)),
            Target::Webdav {
                url, user, pass, ..
            } => AnyStore::WebDav(WebDav::new(url, user, pass, http)?),
            Target::S3 {
                endpoint,
                region,
                bucket,
                prefix,
                access_key_id,
                secret_access_key,
                ..
            } => AnyStore::S3(S3::new(
                s3::Config {
                    endpoint: endpoint.clone(),
                    region: region.clone(),
                    bucket: bucket.clone(),
                    prefix: prefix.clone(),
                    access_key_id: access_key_id.clone(),
                    secret_access_key: secret_access_key.clone(),
                },
                http,
            )?),
        })
    }

    /// 文件夹位置的根目录。别的是 `None`
    pub fn folder_root(&self) -> Option<&std::path::Path> {
        match self {
            AnyStore::Folder(f) => Some(f.root()),
            _ => None,
        }
    }
}

macro_rules! each {
    ($self:ident, $s:ident => $e:expr) => {
        match $self {
            AnyStore::Folder($s) => $e,
            AnyStore::WebDav($s) => $e,
            AnyStore::S3($s) => $e,
        }
    };
}

impl Store for AnyStore {
    async fn list(&self, dir: &str) -> Result<Vec<Entry>, StoreError> {
        each!(self, s => s.list(dir).await)
    }
    async fn get(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError> {
        each!(self, s => s.get(path).await)
    }
    async fn put_new(&self, path: &str, bytes: &[u8]) -> Result<(), StoreError> {
        each!(self, s => s.put_new(path, bytes).await)
    }
    async fn put(&self, path: &str, bytes: &[u8]) -> Result<(), StoreError> {
        each!(self, s => s.put(path, bytes).await)
    }
    async fn delete(&self, path: &str) -> Result<(), StoreError> {
        each!(self, s => s.delete(path).await)
    }
    async fn create_exclusive(&self, path: &str, bytes: &[u8]) -> Result<bool, StoreError> {
        each!(self, s => s.create_exclusive(path, bytes).await)
    }
}

/// 临时文件名：点开头（列目录时跳过），带随机串（两个进程不撞）
pub(crate) fn temp_name(final_name: &str) -> String {
    let r: u64 = rand::random();
    format!(".tmp-{r:016x}-{final_name}")
}

/// 一个路径拆成目录和文件名。只有一层目录
pub(crate) fn split(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((d, f)) => (d, f),
        None => ("", path),
    }
}

/// 路径里只许出现我们自己生成的那些字符：拼进 URL 和文件系统都不用转义，也走不出根目录
pub(crate) fn check_path(path: &str) -> Result<(), StoreError> {
    let ok = !path.is_empty()
        && path.split('/').all(|seg| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        });
    if ok {
        Ok(())
    } else {
        Err(StoreError::Other(msg!(
            "sync.store.bad_path", path = path =>
            "{path} is not a path inside the sync location"
        )))
    }
}

/// HTTP 层的失败按「接下来怎么办」归类。`what` 是这次在做什么（`GET versions/…`），只进英文原句
pub(crate) fn http_status(status: reqwest::StatusCode, what: &str) -> StoreError {
    let code = status.as_u16();
    if code == 401 || code == 403 {
        return StoreError::AuthFailed(msg!(
            "sync.store.denied", status = code =>
            "the sync location refused access ({status}). Check the user name, password or keys"
        ));
    }
    if status.is_server_error() || code == 408 || code == 429 {
        return StoreError::Unreachable(msg!(
            "sync.store.server_error", status = code, what = what =>
            "the sync location answered {status} to {what}"
        ));
    }
    StoreError::Other(msg!(
        "sync.store.unexpected", status = code, what = what =>
        "the sync location answered {status} to {what}"
    ))
}

/// 请求没发出去或者没收到答复
pub(crate) fn http_error(e: &reqwest::Error) -> StoreError {
    let mut detail = e.to_string();
    let mut cur = std::error::Error::source(e);
    while let Some(c) = cur {
        detail.push_str(": ");
        detail.push_str(&c.to_string());
        cur = c.source();
    }
    StoreError::Unreachable(msg!(
        "sync.store.unreachable", detail = detail =>
        "the sync location could not be reached: {detail}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_paths_we_generate_are_taken() {
        for ok in [
            "keyring",
            "versions/0000000041-3f9a2c1e.twconfig",
            "devices/3f9a2c1e",
        ] {
            check_path(ok).unwrap();
        }
        for bad in [
            "",
            "/keyring",
            "versions/../x",
            "a//b",
            "a b",
            "versions/%2e%2e",
            "a\\b",
        ] {
            assert!(check_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn statuses_are_sorted_by_what_to_do_next() {
        use reqwest::StatusCode as S;
        assert!(matches!(
            http_status(S::UNAUTHORIZED, "x"),
            StoreError::AuthFailed(_)
        ));
        assert!(matches!(
            http_status(S::FORBIDDEN, "x"),
            StoreError::AuthFailed(_)
        ));
        assert!(matches!(
            http_status(S::BAD_GATEWAY, "x"),
            StoreError::Unreachable(_)
        ));
        assert!(matches!(
            http_status(S::TOO_MANY_REQUESTS, "x"),
            StoreError::Unreachable(_)
        ));
        assert!(matches!(
            http_status(S::CONFLICT, "x"),
            StoreError::Other(_)
        ));
    }
}
