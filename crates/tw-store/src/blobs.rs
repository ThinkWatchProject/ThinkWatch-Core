//! 请求/响应体，按天分目录。
//!
//! **不进 SQLite。**body 可能几百 KB（长上下文），塞进库里会让它膨胀到
//! GB 级 —— VACUUM 慢、备份慢、回收难。存文件系统按天分目录，过期直接
//! 删掉整个目录，**回收成本 O(1)**。

use std::path::{Path, PathBuf};

/// body 默认留几天。
pub const KEEP_DAYS: u64 = 7;
/// 总量上限。超了从最旧的天目录开始删。
pub const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// 单个 body 的上限。
///
/// 超过就只留开头。**一个 200 MB 的请求体存下来对排查没有额外帮助** ——
/// 而它会把当天的目录一次撑爆，把别的请求的 body 挤掉。
pub const MAX_ONE: usize = 4 * 1024 * 1024;

pub struct Blobs {
    root: PathBuf,
}

/// 一天的目录名：`2026-09-07`。
///
/// **自己算，不引 chrono**：这一层只需要把毫秒时间戳映射到一个稳定的
/// 目录名，而目录名的唯一要求是「同一天的落在一起、字典序即时间序」。
fn day_of(at_ms: i64) -> String {
    let days = at_ms.div_euclid(86_400_000);
    // 1970-01-01 起的民用历法换算（Howard Hinnant 的 civil_from_days）
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

impl Blobs {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, at_ms: i64, id: i64, which: Which) -> PathBuf {
        self.root
            .join(day_of(at_ms))
            .join(format!("{id}.{}", which.suffix()))
    }

    /// 写一个 body。**失败只返回 false，不往上抛** —— 观测挂了，代理照跑。
    ///
    /// **权限是 0600，目录是 0700。**这些文件里有用户的 system prompt、
    /// 代码、有时还有他自己粘进去的密钥 —— 和 config.yaml 一样敏感，
    /// 而它们比 config.yaml 多得多。默认 umask 通常给 0644，那意味着
    /// 同一台机器上的别的用户能把它们全读走（那条「权限就是认证」
    /// 的同一个道理）。
    pub fn put(&self, at_ms: i64, id: i64, which: Which, body: &[u8]) -> bool {
        let p = self.path_for(at_ms, id, which);
        let Some(dir) = p.parent() else { return false };
        if std::fs::create_dir_all(dir).is_err() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // 目录也要收 —— 文件名里有 id，而目录名本身就泄漏「哪天用过」
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            let _ = std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700));
        }
        // 截断而不是跳过：**开头那几 KB 是最有用的部分**（模型名、system
        // prompt、工具定义都在前面），而完整存下来会挤掉别人的。
        let slice = &body[..body.len().min(MAX_ONE)];
        match std::fs::write(&p, slice) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
                }
                true
            }
            Err(e) => {
                tracing::debug!(path = %p.display(), "body 写不下：{e}");
                false
            }
        }
    }

    pub fn get(&self, at_ms: i64, id: i64, which: Which) -> Option<Vec<u8>> {
        std::fs::read(self.path_for(at_ms, id, which)).ok()
    }

    /// 这个 body 被截断过吗。详情页要说出来 —— 不说的话用户会以为请求
    /// 本身就长这样。
    pub fn was_truncated(len: usize) -> bool {
        len > MAX_ONE
    }

    /// 存了多少、原本多长。**两个数一起返回** —— 详情页要靠它说出
    /// 「只存了开头 256 KB」。
    pub fn put_with_len(
        &self,
        at_ms: i64,
        id: i64,
        which: Which,
        body: &[u8],
        original_len: usize,
    ) -> bool {
        if !self.put(at_ms, id, which, body) {
            return false;
        }
        if original_len > body.len() {
            let p = self
                .path_for(at_ms, id, which)
                .with_extension(format!("{}.len", which.suffix()));
            if std::fs::write(&p, original_len.to_string()).is_ok() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
                }
            }
        }
        true
    }

    /// 原始长度。**没有这个文件说明没截断** —— 那时 body 自己的长度
    /// 就是真相。
    pub fn original_len(&self, at_ms: i64, id: i64, which: Which) -> Option<usize> {
        let p = self
            .path_for(at_ms, id, which)
            .with_extension(format!("{}.len", which.suffix()));
        std::fs::read_to_string(p).ok()?.trim().parse().ok()
    }

    /// 所有天目录，**从旧到新**。
    fn days(&self) -> Vec<(String, PathBuf)> {
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut out: Vec<(String, PathBuf)> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
            // 只认 `YYYY-MM-DD`。用户往这个目录里放过别的东西时，**不该
            // 被我们删掉** —— 这是他自己的磁盘。
            .filter(|(n, _)| looks_like_a_day(n))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// 回收：先按天数删，再按总量删。返回删掉的字节数。
    pub fn gc(&self, now_ms: i64, keep_days: u64, max_bytes: u64) -> u64 {
        let cutoff = day_of(now_ms - (keep_days as i64) * 86_400_000);
        let mut freed = 0;
        let mut remaining: Vec<(String, PathBuf, u64)> = Vec::new();
        for (name, path) in self.days() {
            let size = dir_size(&path);
            if name < cutoff {
                if std::fs::remove_dir_all(&path).is_ok() {
                    freed += size;
                }
            } else {
                remaining.push((name, path, size));
            }
        }
        // 还超总量的话，继续从最旧的开始删。**整目录删，不删单个文件** ——
        // 半天的记录比没有记录更难解释（「为什么上午的请求点开是空的」）。
        let mut total: u64 = remaining.iter().map(|(_, _, s)| *s).sum();
        for (_, path, size) in &remaining {
            if total <= max_bytes {
                break;
            }
            if std::fs::remove_dir_all(path).is_ok() {
                total -= size;
                freed += size;
            }
        }
        freed
    }

    pub fn total_bytes(&self) -> u64 {
        self.days().iter().map(|(_, p)| dir_size(p)).sum()
    }
}

fn looks_like_a_day(n: &str) -> bool {
    let b = n.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| matches!(i, 4 | 7) || c.is_ascii_digit())
}

fn dir_size(p: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else {
        return 0;
    };
    rd.flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Request,
    Response,
}

impl Which {
    fn suffix(&self) -> &'static str {
        match self {
            Which::Request => "req",
            Which::Response => "res",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000;

    fn setup() -> (tempfile::TempDir, Blobs) {
        let d = tempfile::tempdir().unwrap();
        let b = Blobs::new(d.path().join("blobs"));
        (d, b)
    }

    #[test]
    fn a_body_comes_back_byte_for_byte() {
        let (_d, b) = setup();
        let body = b"{\"model\":\"claude-opus-4\"}";
        assert!(b.put(1_757_000_000_000, 42, Which::Request, body));
        assert_eq!(b.get(1_757_000_000_000, 42, Which::Request).unwrap(), body);
    }

    #[test]
    fn request_and_response_do_not_overwrite_each_other() {
        let (_d, b) = setup();
        b.put(0, 1, Which::Request, b"in");
        b.put(0, 1, Which::Response, b"out");
        assert_eq!(b.get(0, 1, Which::Request).unwrap(), b"in");
        assert_eq!(b.get(0, 1, Which::Response).unwrap(), b"out");
    }

    #[test]
    fn the_day_directory_name_is_the_real_calendar_date() {
        // 自己算历法最容易在闰年和月末错一天，而错了的表现是「昨天的
        // 请求在今天的目录里」—— 回收会连着把不该删的删掉。
        assert_eq!(day_of(0), "1970-01-01");
        assert_eq!(day_of(DAY - 1), "1970-01-01");
        assert_eq!(day_of(DAY), "1970-01-02");
        // 2026-09-09
        assert_eq!(day_of(1_788_912_000_000), "2026-09-09");
        // 闰日
        assert_eq!(day_of(1_709_164_800_000), "2024-02-29");
        assert_eq!(day_of(1_709_164_800_000 + DAY), "2024-03-01");
        // 2000 是闰年，1900 不是（这是那个经典的错法）
        assert_eq!(day_of(951_782_400_000), "2000-02-29");
    }

    #[test]
    fn day_names_sort_the_same_way_the_days_do() {
        // 回收靠字典序找「最旧的」。位数变化时字典序和时间序分家的话，
        // 会删错目录。
        let mut names: Vec<String> = (0..400).map(|i| day_of(i * DAY)).collect();
        let sorted = {
            let mut c = names.clone();
            c.sort();
            c
        };
        names.dedup();
        assert_eq!(sorted.first().unwrap(), "1970-01-01");
        assert!(sorted.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn an_oversized_body_is_truncated_not_dropped() {
        // **开头那几 KB 是最有用的部分**（模型名、system prompt、工具
        // 定义都在前面），而完整存下来会挤掉别人的。
        let (_d, b) = setup();
        let huge = vec![b'x'; MAX_ONE + 1000];
        assert!(b.put(0, 1, Which::Request, &huge));
        let got = b.get(0, 1, Which::Request).unwrap();
        assert_eq!(got.len(), MAX_ONE);
        assert!(Blobs::was_truncated(huge.len()), "截断了要能说出来");
        assert!(!Blobs::was_truncated(10));
    }

    #[test]
    fn gc_deletes_whole_days_older_than_the_cutoff() {
        let (_d, b) = setup();
        let now = 100 * DAY;
        for i in 0..10 {
            b.put(now - i * DAY, i, Which::Request, &vec![b'x'; 1000]);
        }
        assert_eq!(b.days().len(), 10);
        let freed = b.gc(now, 3, MAX_BYTES);
        // 留 now、now-1、now-2、now-3 这四天（cutoff 是 now-3 那天）
        assert_eq!(b.days().len(), 4, "{:?}", b.days());
        assert_eq!(freed, 6000);
    }

    #[test]
    fn gc_also_deletes_by_total_size_starting_from_the_oldest() {
        let (_d, b) = setup();
        let now = 100 * DAY;
        for i in 0..5 {
            b.put(now - i * DAY, i, Which::Request, &vec![b'x'; 1000]);
        }
        // 天数够新，但总量只允许 2500 字节
        b.gc(now, 30, 2500);
        let left = b.days();
        assert_eq!(left.len(), 2, "{left:?}");
        // 留下的是最新的两天
        assert_eq!(left[1].0, day_of(now));
    }

    #[test]
    fn gc_removes_whole_days_rather_than_individual_files() {
        // **半天的记录比没有记录更难解释**（「为什么上午的请求点开是
        // 空的」）。
        let (_d, b) = setup();
        let now = 100 * DAY;
        for i in 0..3 {
            b.put(now - DAY, i, Which::Request, &vec![b'x'; 1000]);
        }
        b.put(now, 99, Which::Request, &vec![b'x'; 1000]);
        b.gc(now, 30, 1500);
        let left = b.days();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].0, day_of(now));
    }

    #[test]
    fn a_stray_directory_the_user_put_there_is_left_alone() {
        // 这是用户自己的磁盘。**不认识的东西不删。**
        let (_d, b) = setup();
        std::fs::create_dir_all(b.root().join("我的备份")).unwrap();
        std::fs::write(b.root().join("我的备份/x"), b"important").unwrap();
        b.put(0, 1, Which::Request, b"y");
        b.gc(100 * DAY, 1, 1);
        assert!(b.root().join("我的备份/x").exists(), "用户的目录被删了");
    }

    #[test]
    fn a_missing_body_reads_as_none_not_a_panic() {
        // 回收删掉之后详情页还会来问。
        let (_d, b) = setup();
        assert!(b.get(0, 404, Which::Request).is_none());
    }

    #[test]
    fn gc_on_an_empty_or_missing_root_does_nothing_and_says_zero() {
        let (_d, b) = setup();
        assert_eq!(b.gc(0, 7, MAX_BYTES), 0);
        assert_eq!(b.total_bytes(), 0);
    }
}

#[cfg(all(test, unix))]
mod permission_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// **这些文件里有用户的 system prompt、代码、有时还有他自己粘进去的
    /// 密钥。**和 config.yaml 一样敏感，而它们比 config.yaml 多得多。
    ///
    /// 默认 umask 通常给 0644 —— 同一台机器上的别的用户能把它们全读走。
    /// 这条是真机烟测撞出来的：写完之后去看了一眼落盘的那份，发现权限
    /// 是敞开的。
    #[test]
    fn a_stored_body_is_not_readable_by_other_users() {
        let d = tempfile::tempdir().unwrap();
        let b = Blobs::new(d.path().join("blobs"));
        assert!(b.put(0, 1, Which::Request, b"sk-ant-secret"));
        let p = b.root().join(day_of(0)).join("1.req");
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "body 的权限是 {mode:o}");
    }

    #[test]
    fn the_day_directory_is_not_listable_by_other_users() {
        // 目录名本身就泄漏「哪天用过这个工具」，文件名里还有请求 id。
        let d = tempfile::tempdir().unwrap();
        let b = Blobs::new(d.path().join("blobs"));
        b.put(0, 1, Which::Request, b"x");
        for p in [b.root().to_path_buf(), b.root().join(day_of(0))] {
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{} 的权限是 {mode:o}", p.display());
        }
    }

    #[test]
    fn the_truncation_marker_file_is_locked_down_too() {
        // 它只存一个长度数字，但它和 body 在同一个目录里 —— 漏一个
        // 就等于给那个目录开了个口子。
        let d = tempfile::tempdir().unwrap();
        let b = Blobs::new(d.path().join("blobs"));
        b.put_with_len(0, 1, Which::Request, b"abc", 9999);
        let p = b.root().join(day_of(0)).join("1.req.len");
        assert!(p.exists(), "截断标记没写出来");
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "截断标记的权限是 {mode:o}");
    }
}
