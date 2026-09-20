//! 配置文件的读写门户。
//!
//! **两个写入方，一份文件，两个方向**：UI/CLI 经 API 结构化地改，你自己
//! 拿编辑器直接改。由此产生四个问题，这个模块负责其中两个：
//!
//! - **回环**：core 写完文件，监听立刻触发，不加区分就会把自己刚写的又
//!   重载一遍。判据是写完记下的 `(mtime, size, blake3)` 三元组 ——
//!   **mtime 一个人不够**，精度不足，同一秒里的多次写会漏判。
//! - **冲突**：**绝不静默覆盖手改。**写之前重新算一遍磁盘上的 hash，
//!   和上次读到的对不上就拒绝写，把决定权交回给人。
//!
//! 这条规矩是从 cc-switch 那批「程序写坏了用户配置」的 issue 直接学来
//! 的教训，只不过这次保护的是我们自己的文件。

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// 一次写入之后文件长什么样。
///
/// 三个字段缺一不可：**只看 mtime 会漏判**（文件系统的时间精度不够，
/// 同一秒里的两次写看起来一样），只看 size 会漏判等长的改动，只看内容
/// hash 则要每次都读全文 —— 前两个是便宜的快速排除。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub mtime: Option<SystemTime>,
    pub size: u64,
    /// 内容的 blake3，十六进制
    pub hash: String,
}

impl Fingerprint {
    pub fn of(text: &str, meta: Option<&std::fs::Metadata>) -> Self {
        Self {
            mtime: meta.and_then(|m| m.modified().ok()),
            size: text.len() as u64,
            hash: hash_of(text),
        }
    }

    /// 读一遍文件，算出它现在的指纹。文件不存在时返回 None。
    pub fn read(path: &Path) -> std::io::Result<Option<(String, Self)>> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let meta = std::fs::metadata(path).ok();
        let fp = Self::of(&text, meta.as_ref());
        Ok(Some((text, fp)))
    }

    /// 这份磁盘上的东西，就是我上次写下的那份吗。
    ///
    /// **内容 hash 说了算。**mtime 会因为备份工具、云盘同步、`touch`
    /// 而变，而内容没变时重载一次配置是纯粹的浪费（还会打断正在跑的
    /// 流的观感）。前两个字段只用来解释「为什么判成不一样」。
    pub fn same_content(&self, other: &Self) -> bool {
        self.hash == other.hash
    }
}

pub fn hash_of(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

/// **版本号就是内容 hash 的前 12 位。**和 HTTP 的 `ETag` 是同一套东西：
/// 客户端带着它来改，对不上就是 409。
///
/// 截断到 12 位是因为它要出现在 API、CLI 输出和错误信息里，而人要能
/// 一眼比对两个版本号是不是同一个 —— 64 个十六进制字符做不到这件事。
/// 12 位（48 bit）在一台机器一份配置的历史长度下，碰撞概率可以忽略。
pub fn version_of(text: &str) -> String {
    format!("blake3:{}", &hash_of(text)[..12])
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{path} could not be read: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} does not exist")]
    Missing { path: PathBuf },
    /// **有人在我们读到和写下之间改了这个文件。**
    #[error(
        "the configuration file changed in the meantime (it is now {current}, and this edit is based on {expected}), so it was not overwritten. Look at the current content and try again"
    )]
    Conflict { expected: String, current: String },
}

/// 一份读到内存里的配置文本，带着它的来源和版本。
///
/// **它记住的是原始文本，不是解析后的结构。**最小替换要在原文
/// 上做，而一旦经过结构体，注释和格式就已经没了。
#[derive(Debug, Clone)]
pub struct Loaded {
    pub path: PathBuf,
    pub text: String,
    pub fingerprint: Fingerprint,
}

impl Loaded {
    pub fn version(&self) -> String {
        version_of(&self.text)
    }
}

pub fn read(path: &Path) -> Result<Loaded, StoreError> {
    match Fingerprint::read(path) {
        Ok(Some((text, fingerprint))) => Ok(Loaded {
            path: path.to_path_buf(),
            text,
            fingerprint,
        }),
        Ok(None) => Err(StoreError::Missing {
            path: path.to_path_buf(),
        }),
        Err(source) => Err(StoreError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// 把新文本写下去，**先确认磁盘上还是我们读到的那份**。
///
/// 返回写完之后的指纹 —— 调用方要拿它去堵回环：文件监听马上就会响，
/// 而那次响是我们自己造成的。
pub fn write_if_unchanged(
    path: &Path,
    expected: &Fingerprint,
    new_text: &str,
) -> Result<Fingerprint, StoreError> {
    // 写之前再读一次。**这中间的窗口关不上**（没有跨进程的文件锁能既
    // 可靠又不带来死锁风险），但它把「几分钟前读的」缩短到「几微秒前
    // 读的」—— 而真实的冲突是「用户在编辑器里改了半小时」那种。
    match Fingerprint::read(path) {
        Ok(Some((_, now))) if !now.same_content(expected) => {
            return Err(StoreError::Conflict {
                expected: format!("blake3:{}", &expected.hash[..12]),
                current: format!("blake3:{}", &now.hash[..12]),
            });
        }
        Ok(None) => {
            return Err(StoreError::Missing {
                path: path.to_path_buf(),
            });
        }
        Err(source) => {
            return Err(StoreError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
        _ => {}
    }
    write_atomic(path, new_text)
}

/// 原子写 + `0600`。
///
/// 先写临时文件再 rename —— **中断的写不该留下半份配置**。权限不能靠
/// umask 的运气：这个文件里有明文密钥。
pub fn write_atomic(path: &Path, text: &str) -> Result<Fingerprint, StoreError> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).map_err(|source| StoreError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
    }
    // 临时文件带 pid：同一个目录里两个进程同时写，各写各的那一份，
    // rename 才是那个决定胜负的原子操作。
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let io = |source| StoreError::Io {
        path: tmp.clone(),
        source,
    };
    std::fs::write(&tmp, text).map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(io)?;
    }
    std::fs::rename(&tmp, path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let meta = std::fs::metadata(path).ok();
    Ok(Fingerprint::of(text, meta.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_write_we_did_ourselves_is_recognised_as_ours() {
        // 不认出来的话，文件监听会立刻把我们刚写的又重载一遍 ——
        // 严重时是个循环。
        let d = tmpdir();
        let p = d.path().join("config.yaml");
        let fp = write_atomic(&p, "version: 1\n").unwrap();
        let (_, now) = Fingerprint::read(&p).unwrap().unwrap();
        assert!(fp.same_content(&now), "自己写的没认出来");
    }

    #[test]
    fn touching_a_file_without_changing_it_is_not_an_external_edit() {
        // 备份工具、云盘同步、`touch` 都会动 mtime。内容没变时重载一次
        // 配置是纯粹的浪费。
        let d = tmpdir();
        let p = d.path().join("config.yaml");
        let fp = write_atomic(&p, "version: 1\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // 原样重写一遍，mtime 变了内容没变
        std::fs::write(&p, "version: 1\n").unwrap();
        let (_, now) = Fingerprint::read(&p).unwrap().unwrap();
        assert_ne!(fp.mtime, now.mtime, "mtime 该变了，不然这条测了个寂寞");
        assert!(fp.same_content(&now), "内容没变却判成外部改动");
    }

    #[test]
    fn an_edit_of_the_same_length_is_still_detected() {
        // 只看 size 会漏判等长的改动 —— 而把 8788 改成 8789 正是等长的。
        let d = tmpdir();
        let p = d.path().join("config.yaml");
        let fp = write_atomic(&p, "port: 8788\n").unwrap();
        std::fs::write(&p, "port: 8789\n").unwrap();
        let (_, now) = Fingerprint::read(&p).unwrap().unwrap();
        assert_eq!(fp.size, now.size);
        assert!(!fp.same_content(&now), "等长的改动被漏判了");
    }

    #[test]
    fn writing_over_someone_elses_edit_is_refused_not_silently_done() {
        // **这条是从 cc-switch 那批 issue 直接学来的教训。**
        let d = tmpdir();
        let p = d.path().join("config.yaml");
        let base = write_atomic(&p, "port: 8788\n").unwrap();
        // 用户在编辑器里改了
        std::fs::write(&p, "port: 9999   # 我改的\n").unwrap();
        let e = write_if_unchanged(&p, &base, "port: 1234\n").unwrap_err();
        assert!(matches!(e, StoreError::Conflict { .. }), "{e:?}");
        // 而且真的没写
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "port: 9999   # 我改的\n",
            "用户的改动被覆盖了"
        );
    }

    #[test]
    fn the_conflict_message_carries_both_versions_so_you_can_tell_them_apart() {
        let d = tmpdir();
        let p = d.path().join("config.yaml");
        let base = write_atomic(&p, "a: 1\n").unwrap();
        std::fs::write(&p, "a: 2\n").unwrap();
        let e = write_if_unchanged(&p, &base, "a: 3\n").unwrap_err();
        let m = e.to_string();
        assert!(m.contains(&version_of("a: 1\n")[..18]), "{m}");
        assert!(m.contains(&version_of("a: 2\n")[..18]), "{m}");
        assert!(m.contains("was not overwritten"), "{m}");
    }

    #[test]
    fn a_normal_write_goes_through_and_returns_the_new_fingerprint() {
        let d = tmpdir();
        let p = d.path().join("config.yaml");
        let base = write_atomic(&p, "a: 1\n").unwrap();
        let after = write_if_unchanged(&p, &base, "a: 2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a: 2\n");
        let (_, now) = Fingerprint::read(&p).unwrap().unwrap();
        assert!(after.same_content(&now));
    }

    #[test]
    fn the_file_keeps_its_0600_after_every_write() {
        // 这个文件里有明文密钥。权限不能靠 umask 的运气，
        // 而**每一次写都要重新确认** —— 原子写换的是一个新 inode。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let d = tmpdir();
            let p = d.path().join("config.yaml");
            let fp = write_atomic(&p, "a: 1\n").unwrap();
            write_if_unchanged(&p, &fp, "a: 2\n").unwrap();
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "权限是 {mode:o}");
        }
    }

    #[test]
    fn a_half_written_file_never_appears_at_the_real_path() {
        // 中断的写不该留下半份配置。这里验的是「临时文件不叫最终名字」。
        let d = tmpdir();
        let p = d.path().join("config.yaml");
        write_atomic(&p, "a: 1\n").unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "config.yaml")
            .collect();
        assert!(leftovers.is_empty(), "留下了临时文件：{leftovers:?}");
    }

    #[test]
    fn a_version_is_short_enough_for_a_human_to_compare() {
        // 它要出现在 API、CLI 输出和错误信息里，而人要能一眼比对两个
        // 版本号是不是同一个 —— 64 个十六进制字符做不到这件事。
        let v = version_of("a: 1\n");
        assert!(v.starts_with("blake3:"));
        assert_eq!(v.len(), "blake3:".len() + 12);
        assert_ne!(v, version_of("a: 2\n"));
        assert_eq!(v, version_of("a: 1\n"), "同样的内容必须给同样的版本号");
    }

    #[test]
    fn reading_a_missing_file_says_so_instead_of_looking_like_an_io_error() {
        let d = tmpdir();
        let e = read(&d.path().join("nope.yaml")).unwrap_err();
        assert!(matches!(e, StoreError::Missing { .. }), "{e:?}");
    }

    #[test]
    fn a_file_that_is_not_utf8_is_an_io_error_not_a_panic() {
        // 配置文件被别的东西写坏过之后，读它不该让整个进程炸掉。
        let d = tmpdir();
        let p = d.path().join("config.yaml");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(&[0xff, 0xfe, 0x00]).unwrap();
        drop(f);
        let e = read(&p).unwrap_err();
        assert!(matches!(e, StoreError::Io { .. }), "{e:?}");
    }
}
