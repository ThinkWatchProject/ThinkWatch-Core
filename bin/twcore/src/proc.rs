//! 一个 pid 还在不在。
//!
//! 两个地方问的是同一个问题：单实例锁要判断锁文件里那个 pid 是不是上次没退
//! 干净留下的，`--parent` 要判断拉起我们的那个 UI 是不是已经没了。
//!
//! 以前各写了一份，而且**两份答得不一样** —— 锁那边把「存在但不归我们管」
//! 算活着，父进程那边算死。这里只留一份，取安全的那个答法：**拿不准就算
//! 活着**。反过来（拿不准就说死了）的代价是删掉另一个实例正在用的锁，
//! 或者无缘无故把自己退掉。
//!
//! **pid 会被系统回收**，两个平台都是。所以这个问题严格说来是「现在有没有
//! 一个进程叫这个号」，不是「当初那个进程还在不在」。锁文件靠 `Drop` 尽量
//! 把自己删掉来缩小这个窗口；再往下要对得上身份，就得存别的东西（创建时间、
//! 或者干脆换成一个内核对象），而那是另一件事。

/// 现在有没有一个进程是这个 pid。
#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    // SAFETY: 0 号信号不发信号，只做存在性和权限检查。这是 POSIX 上的标准问法。
    unsafe {
        // ESRCH 表示进程不存在；EPERM 表示存在但不归我们管（也算活着）。
        libc::kill(pid as i32, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(windows)]
pub fn alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
    };

    /// winnt.h 里的标准访问权限之一。
    ///
    /// **自己写一遍而不是从 windows-sys 里导入**：它只在
    /// `Win32::Storage::FileSystem` 下导出（类型还是 `FILE_ACCESS_RIGHTS`），
    /// 为一个固定不变的常量把整个文件系统的 feature 拉进来不划算。
    const SYNCHRONIZE: u32 = 0x0010_0000;

    // SAFETY: 只开一个查询用的句柄，拿到就用、用完就关。
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid);
        if h.is_null() {
            // **打不开不等于不存在。**ACCESS_DENIED 说明它在，只是不归我们管
            // —— 和 unix 上的 EPERM 是同一件事，按上面那条算活着。
            return std::io::Error::last_os_error().raw_os_error()
                == Some(ERROR_ACCESS_DENIED as i32);
        }
        // **等 0 毫秒，不问退出码。**进程句柄在进程退出的那一刻变为有信号，
        // 所以「等不到」就是「还在跑」，这个答案是准的。
        //
        // 流传更广的写法是 `GetExitCodeProcess` 然后比 `STILL_ACTIVE`，它有
        // 一个真实的坑：`STILL_ACTIVE` 就是 259，于是一个正常退出、退出码
        // 恰好是 259 的进程会被永远判成活着。core 自己不会那样退，但锁文件里
        // 那个 pid 早就可能被系统回收给了任何一个别的程序。
        let r = WaitForSingleObject(h, 0);
        CloseHandle(h);
        r == WAIT_TIMEOUT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_very_process_is_alive() {
        assert!(alive(std::process::id()));
    }

    /// 起一个、等它退干净、再问。**不能随便编一个 pid** —— 编出来的那个
    /// 号上完全可能真的有一个进程，那样这个测试会偶发地失败。
    #[test]
    fn a_process_that_has_exited_is_not() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit"])
            .spawn()
            .or_else(|_| {
                std::process::Command::new("/bin/sh")
                    .args(["-c", "exit 0"])
                    .spawn()
            })
            .expect("起一个立刻退出的子进程");
        let pid = child.id();
        assert!(alive(pid), "刚起来就说它死了");
        child.wait().expect("等它退出");
        // 收尸之后那个 pid 才真正消失：在此之前它是僵尸，而僵尸是「活着」。
        assert!(!alive(pid), "已经退出并收尸了，还说它活着");
    }

    /// pid 1 存在但不归我们管（unix 上是 init/launchd）。**打不开要算活着**，
    /// 否则一个不归我们管的父进程会让 core 自己退掉。
    #[cfg(unix)]
    #[test]
    fn a_process_we_may_not_touch_still_counts_as_alive() {
        assert!(alive(1));
    }
}
