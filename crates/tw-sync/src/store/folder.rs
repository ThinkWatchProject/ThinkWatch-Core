//! 文件夹位置：本机的一个目录，由 iCloud Drive、Dropbox、OneDrive、Syncthing 或者 NAS
//! 共享在设备之间传送。
//!
//! **写入先写临时名再改名**：第三方客户端随时可能把写了一半的文件传走，另一台设备读到半个
//! 版本。临时名以点开头，列目录时跳过；云盘客户端多数也不传点开头的临时文件。
//!
//! 云盘里还没下载到本机的文件（iCloud 的 `.名字.icloud` 占位文件、Dropbox 的在线文件）列目录
//! 时要么不出现、要么读不出来 —— 调用方把读不出来的版本当成「尚未到达」，到了再合并。

use std::io::Write;
use std::path::{Path, PathBuf};

use tw_types::msg;

use super::{Entry, Store, StoreError, check_path, split, temp_name};

pub struct Folder {
    root: PathBuf,
}

impl Folder {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn at(&self, path: &str) -> Result<PathBuf, StoreError> {
        check_path(path)?;
        let mut p = self.root.clone();
        for seg in path.split('/') {
            p.push(seg);
        }
        Ok(p)
    }
}

fn io(path: &Path, e: std::io::Error) -> StoreError {
    let detail = e.to_string();
    let path = path.display().to_string();
    // 根目录不在（外接盘没插、网络共享没挂上）：和网络位置连不上是同一种情况，等它回来
    if e.kind() == std::io::ErrorKind::NotFound {
        return StoreError::Unreachable(msg!(
            "sync.folder.missing", path = path =>
            "the sync folder {path} is not there. If it is on an external drive or a network \
             share, connect it"
        ));
    }
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        return StoreError::AuthFailed(msg!(
            "sync.folder.denied", path = path, detail = detail =>
            "{path} cannot be written: {detail}"
        ));
    }
    StoreError::Other(msg!(
        "sync.folder.io", path = path, detail = detail =>
        "{path} could not be used: {detail}"
    ))
}

/// 建一个只有本机用户读得到的新文件，写完、落盘
fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// 父目录：要在根目录底下。**根目录本身不替用户建**：它不在说明外接盘、共享没挂上，在一个
/// 空目录里写进去的版本谁也收不到
fn ensure_parent(root: &Path, p: &Path) -> Result<(), StoreError> {
    if !root.is_dir() {
        return Err(io(
            root,
            std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
        ));
    }
    if let Some(dir) = p.parent()
        && dir != root
    {
        std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
    }
    Ok(())
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, StoreError> + Send + 'static,
) -> Result<T, StoreError> {
    tokio::task::spawn_blocking(f).await.unwrap_or_else(|e| {
        Err(StoreError::Other(msg!(
            "sync.folder.task", detail = e =>
            "the sync folder could not be used: {detail}"
        )))
    })
}

/// 改名成最终的名字，**目标已经有了就不改**。硬链接是原子的「不存在才建」；不支持硬链接的
/// 文件系统（FAT、有的网络共享）退回到先看一眼再改名 —— 名字归写它的那台设备独占，看一眼
/// 和改名之间不会有别人来写同一个名字
fn link_or_rename(tmp: &Path, dest: &Path) -> std::io::Result<bool> {
    match std::fs::hard_link(tmp, dest) {
        Ok(()) => {
            let _ = std::fs::remove_file(tmp);
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(tmp);
            Ok(false)
        }
        Err(_) => {
            if dest.exists() {
                let _ = std::fs::remove_file(tmp);
                return Ok(false);
            }
            std::fs::rename(tmp, dest).map(|()| true)
        }
    }
}

impl Store for Folder {
    async fn list(&self, dir: &str) -> Result<Vec<Entry>, StoreError> {
        let root = self.root.clone();
        let d = self.at(dir)?;
        blocking(move || {
            if !root.is_dir() {
                return Err(io(
                    &root,
                    std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
                ));
            }
            let rd = match std::fs::read_dir(&d) {
                Ok(r) => r,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(io(&d, e)),
            };
            let mut out = Vec::new();
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                // 临时文件、云盘的占位文件、系统的 .DS_Store
                if name.starts_with('.') {
                    continue;
                }
                let Ok(meta) = e.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                out.push(Entry {
                    name,
                    size: meta.len(),
                });
            }
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        })
        .await
    }

    async fn get(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let root = self.root.clone();
        let p = self.at(path)?;
        blocking(move || match std::fs::read(&p) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if root.is_dir() {
                    Ok(None)
                } else {
                    Err(io(&root, e))
                }
            }
            Err(e) => Err(io(&p, e)),
        })
        .await
    }

    async fn put_new(&self, path: &str, bytes: &[u8]) -> Result<(), StoreError> {
        let root = self.root.clone();
        let dest = self.at(path)?;
        let (_, name) = split(path);
        let tmp = dest.with_file_name(temp_name(name));
        let bytes = bytes.to_vec();
        let shown = path.to_string();
        blocking(move || {
            ensure_parent(&root, &dest)?;
            if dest.exists() {
                return Err(StoreError::Exists { path: shown });
            }
            write_new(&tmp, &bytes).map_err(|e| io(&tmp, e))?;
            match link_or_rename(&tmp, &dest) {
                Ok(true) => Ok(()),
                Ok(false) => Err(StoreError::Exists { path: shown }),
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    Err(io(&dest, e))
                }
            }
        })
        .await
    }

    async fn put(&self, path: &str, bytes: &[u8]) -> Result<(), StoreError> {
        let root = self.root.clone();
        let dest = self.at(path)?;
        let (_, name) = split(path);
        let tmp = dest.with_file_name(temp_name(name));
        let bytes = bytes.to_vec();
        blocking(move || {
            ensure_parent(&root, &dest)?;
            write_new(&tmp, &bytes).map_err(|e| io(&tmp, e))?;
            // rename 在两个平台上都换掉已有的目标（Windows 上 std 用的是 MOVEFILE_REPLACE_EXISTING）
            std::fs::rename(&tmp, &dest).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                io(&dest, e)
            })
        })
        .await
    }

    async fn delete(&self, path: &str) -> Result<(), StoreError> {
        let p = self.at(path)?;
        blocking(move || match std::fs::remove_file(&p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io(&p, e)),
        })
        .await
    }

    async fn create_exclusive(&self, path: &str, bytes: &[u8]) -> Result<bool, StoreError> {
        let root = self.root.clone();
        let dest = self.at(path)?;
        let (_, name) = split(path);
        let tmp = dest.with_file_name(temp_name(name));
        let bytes = bytes.to_vec();
        blocking(move || {
            ensure_parent(&root, &dest)?;
            if dest.exists() {
                return Ok(false);
            }
            write_new(&tmp, &bytes).map_err(|e| io(&tmp, e))?;
            link_or_rename(&tmp, &dest).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                io(&dest, e)
            })
        })
        .await
    }
}

/// 盯着同步文件夹：新版本、新的 keyring 一到就发一个信号（见 [`tw_watch::watch`]）。盯的是
/// 根目录和 `versions/`，不递归；`versions/` 还没有就先建出来，不然盯不住。
///
/// **设备记录不盯**：每台设备同步完都会写自己那一份，盯着它的话，两台设备会你写一份、我
/// 同步一轮、我再写一份地来回转
pub fn watch(
    root: &Path,
) -> Result<(tw_watch::Watch, tokio::sync::mpsc::Receiver<()>), tw_watch::WatchError> {
    let versions = root.join(crate::layout::VERSIONS);
    // 根目录不在（外接盘没插）时不替它建
    if root.is_dir() {
        let _ = std::fs::create_dir(&versions);
    }
    let dirs: Vec<PathBuf> = [root.to_path_buf(), versions]
        .into_iter()
        .filter(|d| d.is_dir())
        .collect();
    tw_watch::watch(&dirs, std::time::Duration::from_millis(500), |p| {
        // 我们自己的临时文件、云盘的占位文件不算：改名成正式的名字那一下才算
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| !n.starts_with('.'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bed() -> (tempfile::TempDir, Folder) {
        let d = tempfile::tempdir().unwrap();
        let f = Folder::new(d.path());
        (d, f)
    }

    #[tokio::test]
    async fn put_new_never_overwrites_and_leaves_no_temporary_file() {
        let (d, f) = bed();
        f.put_new("versions/0000000001-aaaaaaaa.twconfig", b"one")
            .await
            .unwrap();
        let e = f
            .put_new("versions/0000000001-aaaaaaaa.twconfig", b"two")
            .await
            .unwrap_err();
        assert!(matches!(e, StoreError::Exists { .. }), "{e:?}");
        assert_eq!(
            f.get("versions/0000000001-aaaaaaaa.twconfig")
                .await
                .unwrap(),
            Some(b"one".to_vec())
        );
        let names: Vec<String> = std::fs::read_dir(d.path().join("versions"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, ["0000000001-aaaaaaaa.twconfig"]);
    }

    #[tokio::test]
    async fn create_exclusive_says_whether_it_made_the_file() {
        let (_d, f) = bed();
        assert!(f.create_exclusive("keyring", b"first").await.unwrap());
        assert!(!f.create_exclusive("keyring", b"second").await.unwrap());
        assert_eq!(f.get("keyring").await.unwrap(), Some(b"first".to_vec()));
        f.put("keyring", b"changed").await.unwrap();
        assert_eq!(f.get("keyring").await.unwrap(), Some(b"changed".to_vec()));
    }

    #[tokio::test]
    async fn listing_skips_hidden_and_placeholder_files() {
        let (d, f) = bed();
        assert!(f.list("versions").await.unwrap().is_empty());
        f.put_new("versions/0000000002-bbbbbbbb.twconfig", b"x")
            .await
            .unwrap();
        std::fs::write(
            d.path()
                .join("versions/.0000000003-cccccccc.twconfig.icloud"),
            b"placeholder",
        )
        .unwrap();
        std::fs::create_dir(d.path().join("versions/sub")).unwrap();
        let got = f.list("versions").await.unwrap();
        assert_eq!(
            got,
            [Entry {
                name: "0000000002-bbbbbbbb.twconfig".into(),
                size: 1
            }]
        );
        f.delete("versions/0000000002-bbbbbbbb.twconfig")
            .await
            .unwrap();
        f.delete("versions/0000000002-bbbbbbbb.twconfig")
            .await
            .unwrap();
        assert!(f.list("versions").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_missing_root_is_unreachable_and_never_created() {
        let d = tempfile::tempdir().unwrap();
        let gone = d.path().join("not-mounted");
        let f = Folder::new(&gone);
        let e = f.put_new("versions/x.twconfig", b"x").await.unwrap_err();
        assert!(matches!(e, StoreError::Unreachable(_)), "{e:?}");
        assert!(matches!(
            f.list("versions").await.unwrap_err(),
            StoreError::Unreachable(_)
        ));
        assert!(!gone.exists(), "根目录不该替用户建出来");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn files_are_readable_only_by_this_user() {
        use std::os::unix::fs::PermissionsExt;
        let (d, f) = bed();
        f.put_new("devices/aaaaaaaa", b"x").await.unwrap();
        let mode = std::fs::metadata(d.path().join("devices/aaaaaaaa"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn a_new_version_arriving_wakes_the_watcher() {
        let (d, f) = bed();
        let (_w, mut rx) = watch(d.path()).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        f.put_new("versions/0000000009-dddddddd.twconfig", b"v")
            .await
            .unwrap();
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("五秒内没收到信号");
        assert_eq!(got, Some(()));
    }
}
