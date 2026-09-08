//! 单实例与僵尸检测。
//!
//! 网关端口是固定的（客户端配置里写死了它），所以「上次没退干净」这件事
//! 会表现为启动失败，而错误信息必须能说清是这个原因 —— 否则用户会以为
//! 自己电脑坏了（DESIGN.md §2.2、§2.4）。

use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct LockFile {
    path: PathBuf,
}

#[derive(Debug)]
pub enum LockOutcome {
    Acquired(LockFile),
    /// 另一个实例活着，附上它的 pid 让用户能自己去看。
    AlreadyRunning {
        pid: u32,
    },
}

impl LockFile {
    /// 尝试拿锁。发现陈旧的锁文件（进程已经没了）就接管它。
    pub fn acquire(dir: &Path) -> std::io::Result<LockOutcome> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("twcore.lock");
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(pid) = text.trim().parse::<u32>()
            && pid != std::process::id()
            && process_alive(pid)
        {
            return Ok(LockOutcome::AlreadyRunning { pid });
        }
        // 到这儿说明：没有锁文件、内容不是数字、或者那个 pid 已经死了。
        // 后两种都是「上次没退干净」，直接覆盖。
        let mut f = std::fs::File::create(&path)?;
        write!(f, "{}", std::process::id())?;
        Ok(LockOutcome::Acquired(LockFile { path }))
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        // 尽力而为。删不掉也无所谓 —— 下次启动的僵尸检测会认出它。
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 这个 pid 还活着吗。
///
/// `kill(pid, 0)` 是 POSIX 上的标准问法：不发信号，只做权限和存在性检查。
fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        // ESRCH 表示进程不存在；EPERM 表示存在但不归我们管（也算活着）。
        libc::kill(pid as i32, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquiring_twice_in_the_same_process_succeeds() {
        // 同一个进程再拿一次不该被自己挡住 —— 那会让重启逻辑死锁。
        let d = tempfile::tempdir().unwrap();
        let a = LockFile::acquire(d.path()).unwrap();
        assert!(matches!(a, LockOutcome::Acquired(_)));
        let b = LockFile::acquire(d.path()).unwrap();
        assert!(matches!(b, LockOutcome::Acquired(_)));
    }

    #[test]
    fn a_stale_lock_from_a_dead_process_is_taken_over() {
        // 「上次没退干净」是最常见的情况，不该需要用户手动删文件。
        let d = tempfile::tempdir().unwrap();
        // pid 1 之外挑一个几乎肯定不存在的
        std::fs::write(d.path().join("twcore.lock"), "999999").unwrap();
        assert!(matches!(
            LockFile::acquire(d.path()).unwrap(),
            LockOutcome::Acquired(_)
        ));
    }

    #[test]
    fn a_garbage_lock_file_is_taken_over_not_fatal() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("twcore.lock"), "这不是 pid").unwrap();
        assert!(matches!(
            LockFile::acquire(d.path()).unwrap(),
            LockOutcome::Acquired(_)
        ));
    }

    #[test]
    fn a_live_pid_blocks_and_reports_who() {
        let d = tempfile::tempdir().unwrap();
        // 用 pid 1（init/launchd），它一定活着且不是我们
        std::fs::write(d.path().join("twcore.lock"), "1").unwrap();
        match LockFile::acquire(d.path()).unwrap() {
            LockOutcome::AlreadyRunning { pid } => assert_eq!(pid, 1),
            other => panic!("应该被挡住，实际 {other:?}"),
        }
    }

    #[test]
    fn dropping_the_lock_removes_the_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("twcore.lock");
        {
            let _l = LockFile::acquire(d.path()).unwrap();
            assert!(p.exists());
        }
        assert!(!p.exists());
    }
}
