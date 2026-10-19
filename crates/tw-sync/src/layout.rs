//! 同步位置上有什么、叫什么、留哪些。
//!
//! ```text
//! <同步位置>/
//!   keyring                          以同步口令加密的版本密钥（仅加密时存在）
//!   versions/
//!     0000000041-3f9a2c1e.twconfig   序号-设备标识，只新增、不覆盖
//!     0000000042-3f9a2c1e.twconfig
//!     0000000042-b71d04aa.twconfig   两台设备同时上传，序号相同，下次同步时合并
//!   devices/
//!     3f9a2c1e                       设备记录（名称、Core 版本、最后同步时间）
//! ```
//!
//! 同步位置能看到的只有文件名（版本序号和设备标识的前 8 位）、大小和修改时间。

use std::collections::BTreeSet;

pub const KEYRING: &str = "keyring";
pub const VERSIONS: &str = "versions";
pub const DEVICES: &str = "devices";
/// 版本文件的扩展名，和导出的配置包一样
pub const EXT: &str = ".twconfig";
/// 序号写几位
const SEQ_DIGITS: usize = 10;

/// 一个版本的名字：序号和写它的设备（设备标识的前 8 位）。**按序号排，同序号按设备排**
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VersionName {
    pub seq: u64,
    pub device: String,
}

impl VersionName {
    pub fn new(seq: u64, device_id: &str) -> Self {
        Self {
            seq,
            device: device_tag(device_id),
        }
    }

    /// `0000000042-3f9a2c1e`，不带扩展名。版本互相引用（上一版本）时用它
    pub fn id(&self) -> String {
        format!("{:0width$}-{}", self.seq, self.device, width = SEQ_DIGITS)
    }

    /// 文件名
    pub fn file(&self) -> String {
        format!("{}{EXT}", self.id())
    }

    /// 在同步位置里的路径
    pub fn path(&self) -> String {
        format!("{VERSIONS}/{}", self.file())
    }

    /// 认文件名（带扩展名）或者版本标识（不带）。别的名字（临时文件、云盘的冲突副本
    /// `… (conflicted copy).twconfig`、手放进去的文件）是 `None`
    pub fn parse(name: &str) -> Option<Self> {
        let id = name.strip_suffix(EXT).unwrap_or(name);
        let (seq, dev) = id.split_once('-')?;
        if seq.len() != SEQ_DIGITS || !seq.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if dev.len() != tw_config::sync::DEVICE_ID_PREFIX
            || !dev
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        Some(Self {
            seq: seq.parse().ok()?,
            device: dev.to_string(),
        })
    }
}

impl std::fmt::Display for VersionName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.id())
    }
}

/// 设备标识的前 8 位：文件名、设备记录名里用它
pub fn device_tag(device_id: &str) -> String {
    device_id
        .chars()
        .take(tw_config::sync::DEVICE_ID_PREFIX)
        .collect()
}

/// 设备记录的路径
pub fn device_path(device_id: &str) -> String {
    format!("{DEVICES}/{}", device_tag(device_id))
}

/// `devices/` 里的这个名字是不是一份设备记录（设备标识的前 8 位）。别的是用户自己放的
pub fn is_device_file(name: &str) -> bool {
    name.len() == tw_config::sync::DEVICE_ID_PREFIX
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 同步位置上的一个版本：名字，以及它说自己依据的上一版本（读出来之后才知道）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: VersionName,
    pub parents: Vec<VersionName>,
    /// 更早的祖先（版本自己记着的）
    pub history: Vec<VersionName>,
}

/// 上传新版本之后要删掉哪些。
///
/// **保留最新版本与上一个版本**：每一个「头」（没有别的版本依据它的版本 —— 通常只有最新的
/// 那一个），加上它们依据的上一版本。两台设备同时上传、还没合并时，头有两个，它们和它们
/// 共同的上一版本都留着，等下一台设备合并：合并要用那个上一版本当基准。晚到的版本（云盘
/// 后传到的）也是头，**不会因为序号小就被删掉**。别的都删。
///
/// `listed` 里的 `parents` 只有读出来了的版本才有。读不出来的（尚未到达、坏了）当作头留着；
/// 最新那一层有读不出来的时，序号紧挨着的那一层也留着
pub fn to_delete(listed: &[Listed]) -> Vec<VersionName> {
    let Some(top) = listed.iter().map(|l| l.name.seq).max() else {
        return Vec::new();
    };
    let referenced: BTreeSet<&VersionName> = listed.iter().flat_map(|l| &l.parents).collect();
    let heads: Vec<&Listed> = listed
        .iter()
        .filter(|l| !referenced.contains(&l.name))
        .collect();
    let mut keep: BTreeSet<VersionName> = heads.iter().map(|l| l.name.clone()).collect();
    for l in &heads {
        keep.extend(l.parents.iter().cloned());
    }
    // 不止一个头：它们共同的祖先也留着，连同从它到每个头之间的版本 —— 下一台设备合并它们要
    // 用它当基准，删了就只能两方比较
    if heads.len() > 1 {
        let lineage = |l: &Listed| -> BTreeSet<VersionName> {
            l.history
                .iter()
                .chain(&l.parents)
                .cloned()
                .chain([l.name.clone()])
                .collect()
        };
        let sets: Vec<BTreeSet<VersionName>> = heads.iter().map(|l| lineage(l)).collect();
        let common = sets.iter().skip(1).fold(sets[0].clone(), |acc, s| {
            acc.intersection(s).cloned().collect()
        });
        if let Some(ca) = common.iter().max() {
            for s in &sets {
                keep.extend(s.iter().filter(|n| n.seq >= ca.seq).cloned());
            }
        }
    }
    // 不知道上一版本是谁（读不出来）时，至少留住序号紧挨着的那一层
    if listed
        .iter()
        .any(|l| l.name.seq == top && l.parents.is_empty())
        && let Some(prev) = listed.iter().map(|l| l.name.seq).filter(|s| *s < top).max()
    {
        keep.extend(
            listed
                .iter()
                .filter(|l| l.name.seq == prev)
                .map(|l| l.name.clone()),
        );
    }
    listed
        .iter()
        .map(|l| l.name.clone())
        .filter(|n| !keep.contains(n))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(seq: u64, dev: &str) -> VersionName {
        VersionName {
            seq,
            device: dev.into(),
        }
    }

    fn l(seq: u64, dev: &str, parents: &[(u64, &str)]) -> Listed {
        Listed {
            name: v(seq, dev),
            parents: parents.iter().map(|(s, d)| v(*s, d)).collect(),
            history: Vec::new(),
        }
    }

    #[test]
    fn names_round_trip_and_strangers_are_ignored() {
        let n = VersionName::new(42, "3f9a2c1e-0b1d-4e2f-9a3b-5c6d7e8f9a0b");
        assert_eq!(n.file(), "0000000042-3f9a2c1e.twconfig");
        assert_eq!(n.path(), "versions/0000000042-3f9a2c1e.twconfig");
        assert_eq!(VersionName::parse(&n.file()), Some(n.clone()));
        assert_eq!(VersionName::parse(&n.id()), Some(n));
        for stranger in [
            "0000000042-3f9a2c1e (conflicted copy).twconfig",
            "42-3f9a2c1e.twconfig",
            "0000000042-3F9A2C1E.twconfig",
            "0000000042-3f9a2c.twconfig",
            "notes.txt",
            ".tmp-1-0000000042-3f9a2c1e.twconfig",
        ] {
            assert_eq!(VersionName::parse(stranger), None, "{stranger}");
        }
    }

    #[test]
    fn versions_sort_by_sequence_then_device() {
        let mut all = vec![v(10, "bbbbbbbb"), v(9, "cccccccc"), v(10, "aaaaaaaa")];
        all.sort();
        assert_eq!(
            all,
            [v(9, "cccccccc"), v(10, "aaaaaaaa"), v(10, "bbbbbbbb")]
        );
    }

    #[test]
    fn the_latest_and_the_one_before_it_are_kept() {
        let listed = [
            l(1, "aaaaaaaa", &[]),
            l(2, "aaaaaaaa", &[(1, "aaaaaaaa")]),
            l(3, "bbbbbbbb", &[(2, "aaaaaaaa")]),
        ];
        assert_eq!(to_delete(&listed), [v(1, "aaaaaaaa")]);
    }

    #[test]
    fn unmerged_versions_keep_their_common_parent_until_someone_merges_them() {
        let listed = [
            l(4, "aaaaaaaa", &[(3, "aaaaaaaa")]),
            l(3, "aaaaaaaa", &[(2, "aaaaaaaa")]),
            l(5, "aaaaaaaa", &[(4, "aaaaaaaa")]),
            l(5, "bbbbbbbb", &[(4, "aaaaaaaa")]),
        ];
        assert_eq!(to_delete(&listed), [v(3, "aaaaaaaa")]);
        // 合并之后：新版本依据那两个，它们留着，共同的上一版本可以删了
        let merged = [
            l(4, "aaaaaaaa", &[(3, "aaaaaaaa")]),
            l(5, "aaaaaaaa", &[(4, "aaaaaaaa")]),
            l(5, "bbbbbbbb", &[(4, "aaaaaaaa")]),
            l(6, "cccccccc", &[(5, "aaaaaaaa"), (5, "bbbbbbbb")]),
        ];
        assert_eq!(to_delete(&merged), [v(4, "aaaaaaaa")]);
    }

    /// 云盘后传到的版本：序号比最新的小，但没有谁依据它 —— 它是一个还没合并的头，留着
    #[test]
    fn a_late_arrival_is_a_head_and_is_kept_with_its_parent() {
        let listed = [
            l(4, "aaaaaaaa", &[(3, "aaaaaaaa")]),
            l(5, "aaaaaaaa", &[(4, "aaaaaaaa")]),
            l(6, "aaaaaaaa", &[(5, "aaaaaaaa")]),
            l(5, "bbbbbbbb", &[(4, "aaaaaaaa")]),
        ];
        assert!(to_delete(&listed).is_empty(), "{:?}", to_delete(&listed));
    }

    /// 两个头的共同祖先比它们的上一版本还早：它和中间的版本都留着
    #[test]
    fn the_common_ancestor_of_two_heads_is_kept() {
        let mut late = l(3, "bbbbbbbb", &[(2, "aaaaaaaa")]);
        late.history = vec![v(1, "aaaaaaaa")];
        let mut top = l(4, "aaaaaaaa", &[(3, "aaaaaaaa")]);
        top.history = vec![v(1, "aaaaaaaa"), v(2, "aaaaaaaa")];
        let listed = [
            l(1, "aaaaaaaa", &[]),
            l(2, "aaaaaaaa", &[(1, "aaaaaaaa")]),
            l(3, "aaaaaaaa", &[(2, "aaaaaaaa")]),
            top,
            late,
        ];
        assert_eq!(to_delete(&listed), [v(1, "aaaaaaaa")]);
    }

    #[test]
    fn a_version_that_has_not_arrived_still_keeps_the_layer_below() {
        // 3 读不出来，不知道它依据谁：2 也还是头，2 和它依据的 1 都留着
        let listed = [
            l(1, "aaaaaaaa", &[]),
            l(2, "aaaaaaaa", &[(1, "aaaaaaaa")]),
            l(3, "bbbbbbbb", &[]),
        ];
        assert!(to_delete(&listed).is_empty());
        // 3 读出来了、依据 2：1 可以删了
        let listed = [
            l(1, "aaaaaaaa", &[]),
            l(2, "aaaaaaaa", &[(1, "aaaaaaaa")]),
            l(3, "bbbbbbbb", &[(2, "aaaaaaaa")]),
        ];
        assert_eq!(to_delete(&listed), [v(1, "aaaaaaaa")]);
        // 都读不出来：最新的和紧挨着的那一层留着
        let listed = [
            l(1, "aaaaaaaa", &[]),
            l(2, "aaaaaaaa", &[]),
            l(3, "bbbbbbbb", &[]),
        ];
        assert_eq!(to_delete(&listed), Vec::<VersionName>::new());
        assert!(to_delete(&[]).is_empty());
    }
}
