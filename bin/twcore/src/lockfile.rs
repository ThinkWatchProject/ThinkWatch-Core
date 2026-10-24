//! 单实例与僵尸检测。
//!
//! 网关端口是固定的（客户端配置里写死了它），所以「上次没退干净」这件事
//! 会表现为启动失败，而错误信息必须能说清是这个原因 —— 否则用户会以为
//! 自己电脑坏了。

use std::path::{Path, PathBuf};

/// 锁文件的名字。
pub const LOCK_FILE: &str = "twcore.lock";

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
        let path = dir.join(LOCK_FILE);
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
        Self::write(dir.join(LOCK_FILE))
    }

    /// 放弃锁：退出时不删文件（交接提交之后，文件里马上会是接替者的 pid）。
    pub fn disown(mut self) {
        self.owned = false;
    }

    /// **先写临时文件再改名**：改名是原子的，读锁的那一方看到的要么是旧的、要么是新的，
    /// 不会读到写了一半的（空的）—— 空的会被当成「上次没退干净」直接覆盖
    fn write(path: PathBuf) -> std::io::Result<LockFile> {
        write_atomic(&path, std::process::id().to_string().as_bytes())?;
        Ok(LockFile { path, owned: true })
    }
}

/// 先写同目录下的临时文件，再改名盖过去。
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// 锁文件里此刻写的 pid（读不到、不是数字是 `None`）。
pub fn holder(dir: &Path) -> Option<u32> {
    std::fs::read_to_string(dir.join(LOCK_FILE))
        .ok()
        .and_then(|t| t.trim().parse().ok())
}

impl Drop for LockFile {
    fn drop(&mut self) {
        if !self.owned {
            return;
        }
        let Some(dir) = self.path.parent() else {
            return;
        };
        // **只删自己的**：文件里已经是别人的 pid（交接提交了、接替者写上了自己），删了它
        // 下一个起来的就看不见锁
        if holder(dir) != Some(std::process::id()) {
            return;
        }
        // **还有前任在排空就不删**（它还在写请求库）：留下这个锁（进程没了就是陈旧的），
        // 下一个起来的照常拿到锁，再从排空标记看见前任，等它退了才开库
        if !draining(dir).is_empty() {
            return;
        }
        // 尽力而为。删不掉也无所谓 —— 下次启动的僵尸检测会认出它。
        let _ = std::fs::remove_file(&self.path);
    }
}

// ─────────────────────────────────────────────────────────── 排空中的前任

/// 排空标记的文件名：交接之后还在排空的旧 core。
pub const DRAINING_FILE: &str = "twcore.draining";

/// 标记多老就不认了（旧 core 最多排空 30 分钟，加点余量）：pid 被系统回收给了别的进程时，
/// 不至于一直等下去
const DRAINING_STALE_MS: u64 = 35 * 60 * 1000;

/// 一个交接之后还在排空的旧 core。
///
/// **为什么要记**：接替者在它排空完之前退出（关掉、崩溃、跟着父进程退）时，下一个起来的
/// 看不见交接连接，只看得见锁。不记的话，它以为没有别人、马上开请求库 —— 和还在写库的旧 core
/// 一起写，请求号互相覆盖。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Draining {
    pub pid: u32,
    /// 它（和它之前的）用的请求号都在这之下
    pub ids_below: u64,
    /// 记下的时刻，Unix 毫秒
    pub since_ms: u64,
}

impl Draining {
    /// 此刻记下的
    pub fn new(pid: u32, ids_below: u64) -> Self {
        Self {
            pid,
            ids_below,
            since_ms: now_ms(),
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn read_draining(dir: &Path) -> Vec<Draining> {
    std::fs::read(dir.join(DRAINING_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// 此刻还在排空的前任（进程活着、标记不太老）。
pub fn draining(dir: &Path) -> Vec<Draining> {
    let now = now_ms();
    read_draining(dir)
        .into_iter()
        .filter(|d| d.pid != std::process::id())
        .filter(|d| now.saturating_sub(d.since_ms) < DRAINING_STALE_MS && crate::proc::alive(d.pid))
        .collect()
}

/// 记下一个刚交出去、开始排空的前任（同一个 pid 的旧记录换掉）。不在了的顺手清掉
pub fn mark_draining(dir: &Path, d: Draining) -> std::io::Result<()> {
    let mut all = draining(dir);
    all.retain(|x| x.pid != d.pid);
    all.push(d);
    write_draining(dir, &all)
}

/// 清掉已经不在了的。一个都不剩就删文件
pub fn prune_draining(dir: &Path) {
    let all = draining(dir);
    let _ = write_draining(dir, &all);
}

fn write_draining(dir: &Path, all: &[Draining]) -> std::io::Result<()> {
    let path = dir.join(DRAINING_FILE);
    if all.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let bytes = serde_json::to_vec(all).map_err(std::io::Error::other)?;
    write_atomic(&path, &bytes)
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
        let p = d.path().join(LOCK_FILE);
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
        // 先写临时文件再改名：临时文件不留下
        let left: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn a_lock_rewritten_by_someone_else_is_not_removed() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(LOCK_FILE);
        let LockOutcome::Acquired(l) = LockFile::acquire(d.path()).unwrap() else {
            panic!("应该拿到锁");
        };
        // 交接提交了、接替者写上了它自己
        std::fs::write(&p, "4242").unwrap();
        drop(l);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "4242", "删了别人的锁");
    }

    #[test]
    fn a_lock_is_kept_while_a_predecessor_is_still_draining() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(LOCK_FILE);
        let mut child = sleeping_child();
        mark_draining(d.path(), Draining::new(child.id(), 123)).unwrap();
        let LockOutcome::Acquired(l) = LockFile::acquire(d.path()).unwrap() else {
            panic!("应该拿到锁");
        };
        drop(l);
        let kept = p.exists();
        let seen = draining(d.path());
        let _ = child.kill();
        let _ = child.wait();
        assert!(kept, "前任还在排空，锁被删了");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].ids_below, 123);
        // 前任没了：标记清掉，锁照常删
        prune_draining(d.path());
        assert!(!d.path().join(DRAINING_FILE).exists());
        let LockOutcome::Acquired(l) = LockFile::acquire(d.path()).unwrap() else {
            panic!("应该拿到锁");
        };
        drop(l);
        assert!(!p.exists());
    }

    #[test]
    fn dead_or_stale_draining_marks_are_ignored() {
        let d = tempfile::tempdir().unwrap();
        let dead = {
            let mut c = sleeping_child();
            let pid = c.id();
            c.kill().unwrap();
            c.wait().unwrap();
            pid
        };
        mark_draining(d.path(), Draining::new(dead, 1)).unwrap();
        assert!(draining(d.path()).is_empty(), "进程没了还算在排空");
        let mut old = Draining::new(std::process::id() + 1, 1);
        old.pid = 1; // 一个一定活着、又不是我们的进程
        old.since_ms = 0;
        mark_draining(d.path(), old).unwrap();
        assert!(draining(d.path()).is_empty(), "很久以前的标记还在算");
    }

    #[test]
    fn dropping_the_lock_removes_the_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(LOCK_FILE);
        {
            let _l = LockFile::acquire(d.path()).unwrap();
            assert!(p.exists());
        }
        assert!(!p.exists());
    }
}
