//! 请求/响应体，按天分目录。
//!
//! **不进 SQLite。**body 可能几百 KB（长上下文），塞进库里会让它膨胀到
//! GB 级 —— VACUUM 慢、备份慢、回收难。存文件系统按天分目录，过期直接
//! 删掉整个目录，**回收成本 O(1)**。
//!
//! **一份正文要么整个在、要么不在**：先写到旁边一个临时文件，写完再换上去（见
//! [`Blobs::put`]）。读的一方（详情、对话记录、按正文找）和写的一方不排队，以前读到一半
//! 写着的文件会当成一份读不懂的正文。

use std::path::{Path, PathBuf};

/// 单个 body 的上限（[`tw_api::BODY_MAX`]，4 MiB）。
///
/// 超过就只留开头。**一个 200 MB 的请求体存下来对排查没有额外帮助** ——
/// 而它会把当天的目录一次撑爆，把别的请求的 body 挤掉。
///
/// 留几天、总共留多少不在这里定：那是配置里的 `retention`，回收按它来（见 [`Blobs::gc`]）。
pub const MAX_ONE: usize = tw_api::BODY_MAX;

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

/// 一天的目录名换回那一天零点（UTC）的毫秒数，[`day_of`] 反过来。不是日期的是 `None`。
fn start_of_day(name: &str) -> Option<i64> {
    if !looks_like_a_day(name) {
        return None;
    }
    let y: i64 = name[0..4].parse().ok()?;
    let m: i64 = name[5..7].parse().ok()?;
    let d: i64 = name[8..10].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // 同一位的 days_from_civil：三月算一年的头一个月，闰日落在年末
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400_000)
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
    ///
    /// 交到这里的已经是换过、打过码的那一份（网关的 `bodies::BodyRecord::for_disk`）：
    /// 脱敏规则认得出的值进不了磁盘。**规则认不全**，所以权限照样收紧。
    pub fn put(&self, at_ms: i64, id: i64, which: Which, body: &[u8]) -> bool {
        let p = self.path_for(at_ms, id, which);
        let Some(dir) = p.parent() else { return false };
        if create_private_dirs(dir).is_err() {
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
        match replace_private(&p, slice) {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!(path = %p.display(), "the request body could not be written: {e}");
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

    /// 写一个 body，连同它原本多长。详情页要靠它说出「只存了开头 4 MB」，重放靠它
    /// 拒绝一份截断过的请求。
    ///
    /// `original_len` 比**真正存下的**长时才另记一个 `.len` —— 截断可能发生在交来之前
    /// （网关只攒了开头），也可能发生在这里（`body` 比 [`MAX_ONE`] 长）。以前只看前一种：
    /// 一个 5 MB 的请求体存下 4 MB，却没有一处说它被截过，重放照样把半截 JSON 发了出去。
    ///
    /// **`.len` 先写**：正文一出现，读的一方就该知道它是不是截过的。反过来的话，中间有一刻
    /// 读到的是一份看起来完整的半截正文。
    pub fn put_with_len(
        &self,
        at_ms: i64,
        id: i64,
        which: Which,
        body: &[u8],
        original_len: usize,
    ) -> bool {
        if original_len > body.len().min(MAX_ONE) {
            let p = self
                .path_for(at_ms, id, which)
                .with_extension(format!("{}.len", which.suffix()));
            if let Some(dir) = p.parent()
                && create_private_dirs(dir).is_ok()
            {
                let _ = replace_private(&p, original_len.to_string().as_bytes());
            }
        }
        self.put(at_ms, id, which, body)
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

    /// 盘上最老的那一天从哪一刻起（那天零点，UTC）。一天都没有是 `None`。
    ///
    /// **按正文找时用它划界**：比它早的请求一份正文都不会有，不必一条一条去盘上问 ——
    /// 三个月的记录里，正文只占最近几天。回收先删最老的，整天整天地删；只剩最新的一天时
    /// 才从那一天里最早的请求删起。所以比它早的一份都没有，它之后的照样要去盘上问。
    pub fn oldest_ms(&self) -> Option<i64> {
        self.days().first().and_then(|(name, _)| start_of_day(name))
    }

    /// 回收：先按天数删，再按总量删。返回删掉的字节数。
    ///
    /// **按总量删时，最新的那一天不整个删。**从最老的那天整天整天地删；删到只剩最新的一天
    /// 还超，就在这一天里从最早的请求删起（[`trim_oldest`]）。以前它也整个删：一天的正文就
    /// 超过上限时（一个请求最多存 8 MB 多），每小时的回收都把当天的目录清空一次，连同正在
    /// 看的那次会话。
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
        let mut total: u64 = remaining.iter().map(|(_, _, s)| *s).sum();
        let Some(((_, newest, _), older)) = remaining.split_last() else {
            return freed;
        };
        // 还超总量的话，继续从最旧的开始删。**旧的整目录删，不删单个文件** ——
        // 半天的记录比没有记录更难解释（「为什么上午的请求点开是空的」）。
        for (_, path, size) in older {
            if total <= max_bytes {
                break;
            }
            if std::fs::remove_dir_all(path).is_ok() {
                total -= size;
                freed += size;
            }
        }
        if total > max_bytes {
            freed += trim_oldest(newest, total - max_bytes);
        }
        freed
    }

    pub fn total_bytes(&self) -> u64 {
        self.days().iter().map(|(_, p)| dir_size(p)).sum()
    }
}

/// 建目录，**新建的每一层生来就是 0700**（`mkdir(2)` 那一刻给，不是事后收）。
fn create_private_dirs(dir: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

/// 写一个只给自己看的文件：**新建时带着 0600 建出来**，不是写完再 `chmod`
/// —— 那中间有一个按 umask 给的 0644 窗口，窗口里别的用户能打开它。已经在的
/// 文件 `mode` 不生效，由调用方随后收紧。
fn write_private(p: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(p)?.write_all(bytes)
}

/// 换上一份只给自己看的文件：**先写到旁边的临时文件，写完再改名换上去**。改名是一步完成
/// 的，读的一方要么读到旧的（或者没有），要么读到写完的那一份，不会读到一半。
fn replace_private(p: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = p.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let done = write_private(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, p));
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    #[cfg(unix)]
    if done.is_ok() {
        // 临时文件早就在（上次写到一半断了）的话，新建时给的 0600 不生效，这里补上
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
    }
    done
}

/// 在一天的目录里从最早的请求删起，删够 `excess` 字节为止。返回删掉的字节数。
///
/// **一个请求的几份一起删**（请求体、回答、插件改过的、`.len`）：按文件名开头的请求号归在
/// 一起，号小的先删 —— 号是按请求开始的先后发的。认不出请求号的文件不动。
fn trim_oldest(day: &Path, excess: u64) -> u64 {
    let Ok(rd) = std::fs::read_dir(day) else {
        return 0;
    };
    let mut by_id: std::collections::BTreeMap<u64, Vec<(PathBuf, u64)>> = Default::default();
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|n| n.split_once('.'))
            .and_then(|(id, _)| id.parse::<u64>().ok())
        else {
            continue;
        };
        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
        by_id.entry(id).or_default().push((e.path(), size));
    }
    let mut freed = 0;
    for files in by_id.into_values() {
        if freed >= excess {
            break;
        }
        for (path, size) in files {
            if std::fs::remove_file(&path).is_ok() {
                freed += size;
            }
        }
    }
    freed
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
    /// 插件改过之后的请求体，挨着 `{id}.req` 放（`{id}.after-plugins`）
    AfterPlugins,
}

impl Which {
    fn suffix(&self) -> &'static str {
        match self {
            Which::Request => "req",
            Which::Response => "res",
            Which::AfterPlugins => "after-plugins",
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

    /// 目录名换回那一天的零点：和 `day_of` 互为反函数，闰日、世纪年都对得上
    #[test]
    fn a_day_name_turns_back_into_the_start_of_that_day() {
        for ms in [
            0,
            DAY - 1,
            1_709_164_800_000,
            951_782_400_000,
            1_788_912_000_000 + 12_345_678,
            -DAY,
        ] {
            let start = start_of_day(&day_of(ms)).unwrap();
            assert_eq!(start, ms - ms.rem_euclid(DAY), "{}", day_of(ms));
        }
        for d in 0..2000 {
            assert_eq!(start_of_day(&day_of(d * DAY + 1)), Some(d * DAY));
        }
        assert_eq!(start_of_day("2026-13-01"), None);
        assert_eq!(start_of_day("我的备份"), None);
    }

    #[test]
    fn the_oldest_day_on_disk_is_where_bodies_start() {
        let (_d, b) = setup();
        assert_eq!(b.oldest_ms(), None, "一份正文都没有");
        b.put(10 * DAY + 5, 1, Which::Request, b"x");
        b.put(12 * DAY + 5, 2, Which::Request, b"y");
        // 用户自己放的目录不算一天
        std::fs::create_dir_all(b.root().join("0000-backup")).unwrap();
        assert_eq!(b.oldest_ms(), Some(10 * DAY));
        b.gc(13 * DAY, 1, u64::MAX);
        assert_eq!(b.oldest_ms(), Some(12 * DAY));
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

    /// 截在这里的也要留下原本多长。以前只有「交来之前就截过」的才记：一个比上限长的
    /// 请求体整份交进来、在这里被截，读回去的人看不出它少了一截 —— 重放照样把半截发出去
    #[test]
    fn a_body_cut_here_records_how_long_it_was() {
        let (_d, b) = setup();
        let huge = vec![b'x'; MAX_ONE + 1000];
        assert!(b.put_with_len(0, 1, Which::Request, &huge, huge.len()));
        assert_eq!(b.get(0, 1, Which::Request).unwrap().len(), MAX_ONE);
        assert_eq!(b.original_len(0, 1, Which::Request), Some(MAX_ONE + 1000));

        // 截在交来之前的：交来的是开头，原本的长度另给
        assert!(b.put_with_len(0, 2, Which::Response, b"head", 9_999));
        assert_eq!(b.original_len(0, 2, Which::Response), Some(9_999));

        // 整份都存下了的不记：读的人拿存下的长度当原本的
        assert!(b.put_with_len(0, 3, Which::Request, b"whole", 5));
        assert_eq!(b.original_len(0, 3, Which::Request), None);
    }

    #[test]
    fn gc_deletes_whole_days_older_than_the_cutoff() {
        let (_d, b) = setup();
        let now = 100 * DAY;
        for i in 0..10 {
            b.put(now - i * DAY, i, Which::Request, &vec![b'x'; 1000]);
        }
        assert_eq!(b.days().len(), 10);
        let freed = b.gc(now, 3, u64::MAX);
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

    /// **一天的正文就超过上限时，那一天不整个删。**以前每小时的回收都把当天的目录清空：
    /// 正在看的那次会话、刚结束的那个请求，点开都是空的。现在删到只剩最新的一天，就在这一天
    /// 里从最早的请求删起，一个请求的几份一起删
    #[test]
    fn gc_trims_the_newest_day_from_its_oldest_requests_instead_of_emptying_it() {
        let (_d, b) = setup();
        let now = 100 * DAY;
        // 前一天的整个删
        b.put(now - DAY, 1, Which::Request, &vec![b'x'; 1000]);
        // 当天五个请求，各有请求体和回答，2000 字节一个；第 10 个还记着原本多长
        for id in 10..15 {
            b.put(now + id, id, Which::Request, &vec![b'x'; 1000]);
            b.put(now + id, id, Which::Response, &vec![b'x'; 1000]);
        }
        b.put_with_len(now, 10, Which::Request, &vec![b'x'; 1000], 9999);
        let today = b.root().join(day_of(now));
        let before = b.total_bytes();

        let freed = b.gc(now, 30, 3500);

        assert_eq!(b.days().len(), 1, "{:?}", b.days());
        assert!(today.is_dir(), "当天的目录被整个删了");
        assert!(b.total_bytes() <= 3500, "{}", b.total_bytes());
        assert_eq!(freed, before - b.total_bytes());
        // 最早的四个连同 `.len` 一起没了，最新的那个还在
        for id in 10..14 {
            assert!(b.get(now, id, Which::Request).is_none(), "{id}");
            assert!(b.get(now, id, Which::Response).is_none(), "{id}");
        }
        assert_eq!(b.original_len(now, 10, Which::Request), None);
        assert!(b.get(now, 14, Which::Request).is_some());
        assert!(b.get(now, 14, Which::Response).is_some());

        // 下一小时没有新写入：已经在上限之内，什么都不再删
        assert_eq!(b.gc(now + 3_600_000, 30, 3500), 0);
        assert!(b.get(now, 14, Which::Response).is_some());
    }

    /// 写到一半的正文读不到：先写临时文件再换上去，换完不留临时文件
    #[test]
    fn a_body_is_swapped_in_whole_and_leaves_no_temporary_file_behind() {
        let (_d, b) = setup();
        assert!(b.put(0, 1, Which::Response, b"first"));
        assert!(b.put(0, 1, Which::Response, b"second, longer"));
        assert_eq!(b.get(0, 1, Which::Response).unwrap(), b"second, longer");
        let names: Vec<String> = std::fs::read_dir(b.root().join(day_of(0)))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, ["1.res"]);
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
        assert_eq!(b.gc(0, 7, u64::MAX), 0);
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
