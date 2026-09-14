//! 磁盘不够时怎么办。
//!
//! **任何一种情况都不拒绝服务。**这是那条原则的具体落地：观测挂
//! 了，代理照跑。一个因为「日志写不下」而拒绝转发的网关，比一个不记日志
//! 的网关坏一百倍 —— 用户来这里是为了让 AI 客户端能用，不是为了看图表。

use std::path::Path;

/// 剩多少空间时停止写 body。
///
/// 1 GB 听起来很多，但一次长上下文请求的 body 可以有几百 KB，而**回收是
/// 定期跑的**：两次回收之间攒下来的量完全可能是几百 MB。留出余量，别让
/// 用户的磁盘因为我们而满。
pub const STOP_BLOBS_BELOW: u64 = 1024 * 1024 * 1024;

/// 剩多少空间时停止一切写入。
pub const STOP_ALL_BELOW: u64 = 200 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskLevel {
    /// 一切正常
    Ok,
    /// 只记 metadata，不存 body。**界面上要告警** —— 用户点开一个请求
    /// 发现没有 body，得知道那不是 bug
    MetadataOnly,
    /// 什么都不写，纯转发。托盘变黄
    Nothing,
}

impl DiskLevel {
    pub fn writes_blobs(&self) -> bool {
        matches!(self, DiskLevel::Ok)
    }
    pub fn writes_anything(&self) -> bool {
        !matches!(self, DiskLevel::Nothing)
    }
    pub fn label(&self) -> &'static str {
        match self {
            DiskLevel::Ok => "正常",
            DiskLevel::MetadataOnly => "磁盘快满了，只记摘要不存请求体",
            DiskLevel::Nothing => "磁盘几乎满了，已停止记录（转发不受影响）",
        }
    }
}

pub fn level_for(free_bytes: u64) -> DiskLevel {
    if free_bytes < STOP_ALL_BELOW {
        DiskLevel::Nothing
    } else if free_bytes < STOP_BLOBS_BELOW {
        DiskLevel::MetadataOnly
    } else {
        DiskLevel::Ok
    }
}

/// 这个路径所在的文件系统还剩多少字节。
///
/// **查不出来时当成「够用」。**查不出空间是我们的问题，不该表现为「用户
/// 的观测功能莫名其妙停了」—— 真的写满了，写入失败那条路径同样会把级别
/// 降下来。
pub fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: c 是一个合法的 NUL 结尾字符串，statvfs 只读它。
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return None;
        }
        // f_bavail 是**非特权用户**能用的块数，不是 f_bfree。
        // 用后者会高估 —— 那部分是给 root 留的。
        Some(st.f_bavail as u64 * st.f_frsize as u64)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_levels_are_ordered_the_way_the_thresholds_say() {
        assert_eq!(level_for(10 * STOP_BLOBS_BELOW), DiskLevel::Ok);
        assert_eq!(level_for(STOP_BLOBS_BELOW), DiskLevel::Ok);
        assert_eq!(level_for(STOP_BLOBS_BELOW - 1), DiskLevel::MetadataOnly);
        assert_eq!(level_for(STOP_ALL_BELOW), DiskLevel::MetadataOnly);
        assert_eq!(level_for(STOP_ALL_BELOW - 1), DiskLevel::Nothing);
        assert_eq!(level_for(0), DiskLevel::Nothing);
    }

    #[test]
    fn every_level_still_lets_traffic_through() {
        // **这条测的是一条产品承诺，不是一段逻辑**：观测挂了，代理照跑。
        // 哪天有人往 DiskLevel 上加一个「停止转发」，它会立刻响。
        for l in [DiskLevel::Ok, DiskLevel::MetadataOnly, DiskLevel::Nothing] {
            assert!(l.label().len() > 1, "每一级都要有一句给人看的话");
        }
        assert!(
            DiskLevel::Nothing.label().contains("转发不受影响"),
            "最坏的那一级也必须说清楚请求还是通的"
        );
    }

    #[test]
    fn a_real_path_reports_something_plausible() {
        let d = tempfile::tempdir().unwrap();
        let free = free_bytes(d.path()).expect("查不出剩余空间");
        assert!(free > 0, "剩余空间是 0？那这个测试也写不出来");
    }

    #[test]
    fn a_path_that_does_not_exist_answers_none_rather_than_zero() {
        // 返回 0 会被判成 `Nothing`，于是一个路径拼错的 bug 表现为
        // 「观测功能莫名其妙停了」。
        assert!(free_bytes(Path::new("/no/such/path/at/all")).is_none());
    }
}
