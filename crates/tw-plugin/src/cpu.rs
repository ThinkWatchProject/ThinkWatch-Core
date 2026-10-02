//! 这个线程用掉的 CPU 时间。
//!
//! 插件的时间上限按 CPU 时间算，不按墙上时间：机器忙的时候线程被抢占，墙上时间
//! 照走，而插件其实没在跑 —— 按墙上时间算，一个 5 ms 的钩子在编译大工程时也会被
//! 判超时，请求跟着被拒。
//!
//! unix（Linux、macOS）用 `CLOCK_THREAD_CPUTIME_ID`。Windows 的线程时间按时钟中断
//! 记账（15.6 ms 一格），对 20 ms 的预算太粗，那里退回墙上时间：调用都跑在专用
//! 的线程池上，平时两者差不多，只在机器很忙时偏严。

use std::time::Duration;

/// 从某个任意起点算起的「这个线程的 CPU 时间」。只拿来相减
#[cfg(unix)]
pub(crate) fn now() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: 只写我们给的这个 timespec
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    if rc == 0 {
        Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
    } else {
        wall()
    }
}

#[cfg(not(unix))]
pub(crate) fn now() -> Duration {
    wall()
}

fn wall() -> Duration {
    use std::sync::OnceLock;
    use std::time::Instant;
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN.get_or_init(Instant::now).elapsed()
}
