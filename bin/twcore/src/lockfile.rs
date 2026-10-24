//! 单实例与僵尸检测。
//!
//! 网关端口是固定的（客户端配置里写死了它），所以「上次没退干净」这件事
//! 会表现为启动失败，而错误信息必须能说清是这个原因 —— 否则用户会以为
//! 自己电脑坏了。

use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct LockFile {
    path: PathBuf,
    /// 退出时删不删文件。**交接之后不删**：那时文件里写的已经是接替者的 pid
    owned: bool,
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
            && crate::proc::alive(pid)
        {
            return Ok(LockOutcome::AlreadyRunning { pid });
        }
        // 到这儿说明：没有锁文件、内容不是数字、或者那个 pid 已经死了。
        // 后两种都是「上次没退干净」，直接覆盖。
        Ok(LockOutcome::Acquired(Self::write(path)?))
    }

    /// 接替者在交接提交时成为锁的持有者：**不看现在写的是谁**，直接写上自己。
    ///
    /// 此刻文件里写的是旧 core 的 pid、它也还活着（在排空），但它已经放弃了锁（见
    /// [`LockFile::disown`]）。
    pub fn claim(dir: &Path) -> std::io::Result<LockFile> {
        std::fs::create_dir_all(dir)?;
        Self::write(dir.join("twcore.lock"))
    }

    /// 放弃锁：退出时不删文件（交接提交之后，文件里马上会是接替者的 pid）。
    pub fn disown(mut self) {
        self.owned = false;
    }

    fn write(path: PathBuf) -> std::io::Result<LockFile> {
        let mut f = std::fs::File::create(&path)?;
        write!(f, "{}", std::process::id())?;
        Ok(LockFile { path, owned: true })
    }
}

/// 锁文件里此刻写的 pid（读不到、不是数字是 `None`）。
#[cfg(test)]
pub fn holder(dir: &Path) -> Option<u32> {
    std::fs::read_to_string(dir.join("twcore.lock"))
        .ok()
        .and_then(|t| t.trim().parse().ok())
}

impl Drop for LockFile {
    fn drop(&mut self) {
        if !self.owned {
            return;
        }
        // 尽力而为。删不掉也无所谓 —— 下次启动的僵尸检测会认出它。
        let _ = std::fs::remove_file(&self.path);
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
        // **不编一个 pid。**以前这里写死 999999，理由是 macOS 的 pid_max
        // 是 99998、Windows 的 pid 都是 4 的倍数 —— 而 Linux 上 systemd 把
        // pid_max 设成 4194304，999999 完全可能是一个活着的进程。起一个、
        // 等它退干净，拿它的号：这个号此刻一定没人用（除非刚好被回收，
        // 那个窗口比编一个号小得多）。
        let pid = {
            let mut c = sleeping_child();
            let pid = c.id();
            c.kill().unwrap();
            c.wait().unwrap();
            pid
        };
        std::fs::write(d.path().join("twcore.lock"), pid.to_string()).unwrap();
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

    /// 一个会活一会儿的子进程。
    ///
    /// **不借用 pid 1。**那在 unix 上是 init/launchd，而 Windows 上根本没有
    /// 这个 pid —— 这条测试要的只是「一个确实活着、又不是我们自己的 pid」，
    /// 起一个就是了，不必挑一个碰巧存在的。
    fn sleeping_child() -> std::process::Child {
        #[cfg(windows)]
        // `timeout` 要一个控制台，测试进程里没有；`ping` 不要
        let mut c = std::process::Command::new("ping");
        #[cfg(windows)]
        c.args(["-n", "60", "127.0.0.1"]);
        #[cfg(not(windows))]
        let mut c = std::process::Command::new("sleep");
        #[cfg(not(windows))]
        c.arg("60");
        c.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("起一个会活一会儿的子进程")
    }

    #[test]
    fn a_live_pid_blocks_and_reports_who() {
        let d = tempfile::tempdir().unwrap();
        let mut child = sleeping_child();
        let alive = child.id();
        std::fs::write(d.path().join("twcore.lock"), alive.to_string()).unwrap();
        let got = LockFile::acquire(d.path());
        // **先收尸再断言** —— 断言失败会 panic，而 panic 之后这个子进程
        // 就留在那儿活满六十秒
        let _ = child.kill();
        let _ = child.wait();
        match got.unwrap() {
            LockOutcome::AlreadyRunning { pid } => assert_eq!(pid, alive),
            other => panic!("应该被挡住，实际 {other:?}"),
        }
    }

    #[test]
    fn a_disowned_lock_leaves_the_file_for_the_successor() {
        // 交接提交之后旧 core 放弃锁：它退出时文件里写的已经是接替者，不能删
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("twcore.lock");
        let LockOutcome::Acquired(l) = LockFile::acquire(d.path()).unwrap() else {
            panic!("应该拿到锁");
        };
        l.disown();
        assert!(p.exists(), "放弃了锁，文件还该在");
    }

    #[test]
    fn claiming_writes_our_pid_over_a_live_holder() {
        let d = tempfile::tempdir().unwrap();
        let mut child = sleeping_child();
        std::fs::write(d.path().join("twcore.lock"), child.id().to_string()).unwrap();
        let claimed = LockFile::claim(d.path());
        let _ = child.kill();
        let _ = child.wait();
        let _l = claimed.unwrap();
        assert_eq!(holder(d.path()), Some(std::process::id()));
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
