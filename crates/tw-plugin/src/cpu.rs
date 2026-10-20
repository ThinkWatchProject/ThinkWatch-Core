//! 这个线程用掉的 CPU 时间。
//!
//! 插件的时间上限按 CPU 时间算，不按墙上时间：机器忙的时候线程被抢占，墙上时间
//! 照走，而插件其实没在跑 —— 按墙上时间算，一个 5 ms 的钩子在编译大工程时也会被
//! 判超时，请求跟着被拒。
//!
//! unix（Linux、macOS）用 `CLOCK_THREAD_CPUTIME_ID`，精确到纳秒。
//!
//! **Windows 的线程时间按时钟中断记账**（`GetThreadTimes`，15.6 ms 一格），对 20 ms 的
//! 预算单独用太粗：一个只跑了 5 ms 的钩子，碰上一次中断就被记成一整格。所以那里取两个
//! 数里小的那个：墙上时间，和线程时间加一格。机器空闲时两者差不多，墙上时间更小、更准，
//! 和以前一样；机器忙时墙上时间照走，线程时间不走，被抢占的那段不再算在插件头上 ——
//! 至多多算一格。
//!
//! 起点和量的那一刻要在**同一个线程**上（调用都在专用的线程池上，一段执行不换线程）。

use std::time::{Duration, Instant};

/// 一段执行的起点。[`Mark::elapsed`] 是从这里起这个线程用掉的 CPU 时间
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mark {
    wall: Instant,
    /// 这个线程到这一刻的 CPU 时间。量不到时是 None，退回墙上时间
    thread: Option<Duration>,
}

impl Mark {
    pub(crate) fn now() -> Self {
        Self {
            wall: Instant::now(),
            thread: thread_time(),
        }
    }

    pub(crate) fn elapsed(&self) -> Duration {
        let wall = self.wall.elapsed();
        match (self.thread, thread_time()) {
            (Some(from), Some(to)) => charge(wall, to.saturating_sub(from)),
            _ => wall,
        }
    }
}

/// Windows 上线程时间的一格：时钟中断的间隔
#[cfg_attr(not(windows), allow(dead_code))]
const TICK: Duration = Duration::from_micros(15_625);

/// 这段执行记多少：线程时间按格记账的平台上，取墙上时间和「线程时间加一格」里小的那个
#[cfg_attr(not(windows), allow(dead_code))]
fn coarse(wall: Duration, thread: Duration) -> Duration {
    wall.min(thread.saturating_add(TICK))
}

#[cfg(windows)]
fn charge(wall: Duration, thread: Duration) -> Duration {
    coarse(wall, thread)
}

#[cfg(not(windows))]
fn charge(_wall: Duration, thread: Duration) -> Duration {
    thread
}

#[cfg(unix)]
fn thread_time() -> Option<Duration> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: 只写我们给的这个 timespec
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    (rc == 0).then(|| Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
}

#[cfg(windows)]
fn thread_time() -> Option<Duration> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: GetCurrentThread 是不用关闭的伪句柄；只写我们给的这四个 FILETIME
    let ok = unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    let units = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    // 单位是 100 ns
    (ok != 0).then(|| Duration::from_nanos((units(kernel) + units(user)).saturating_mul(100)))
}

#[cfg(not(any(unix, windows)))]
fn thread_time() -> Option<Duration> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 睡着的线程不花预算：被抢占、在等的时间不算在插件头上
    #[test]
    fn a_thread_that_sleeps_spends_almost_none_of_its_budget() {
        let m = Mark::now();
        std::thread::sleep(Duration::from_millis(80));
        let spent = m.elapsed();
        // unix 上几乎是 0；Windows 上一两格（醒来那一刻碰上时钟中断会多记一格），都远小于
        // 睡掉的 80 ms
        assert!(spent < Duration::from_millis(40), "{spent:?}");
    }

    /// 按格记账时，记的不超过墙上时间，也不超过线程时间加一格
    #[test]
    fn a_coarse_thread_clock_is_capped_by_the_wall_and_by_one_tick() {
        let ms = Duration::from_millis;
        // 空闲：墙上时间更小，用它
        assert_eq!(coarse(ms(5), ms(0)), ms(5));
        assert_eq!(coarse(ms(12), ms(15)), ms(12));
        // 忙：墙上走了 200 ms，线程只跑了 2 格
        assert_eq!(coarse(ms(200), Duration::from_micros(31_250)), TICK * 3);
        // 失控的循环：两者一起涨，照样超过预算
        assert!(coarse(ms(40), ms(37)) >= ms(20));
    }

    /// 真在跑的线程照样花预算
    #[test]
    fn a_busy_thread_spends_its_budget() {
        let m = Mark::now();
        let until = Instant::now() + Duration::from_millis(60);
        let mut x = 0u64;
        while Instant::now() < until {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        // 机器再忙，60 ms 的忙循环也至少拿到几毫秒的 CPU
        assert!(m.elapsed() > Duration::ZERO, "{x}");
    }
}
