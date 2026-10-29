//! 个人信息规则查的几张表。**随 core 发布、编进二进制**：都是公开的、几年才变一次的东西，
//! 不值得在运行时去取。
//!
//! 来源和整理日期写在各个数据文件的头两行（`#` 开头的行是注释）。四个文件合起来不到 50 KB。

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// 去掉注释和空行
fn lines(s: &'static str) -> impl Iterator<Item = &'static str> {
    s.lines().filter(|l| !l.is_empty() && !l.starts_with('#'))
}

// ---------------------------------------------------------------- 行政区划

/// 中国大陆的行政区划名称：省级、地级、县级三层。
pub(super) struct Divisions {
    /// 省级全称和简称（`广东省`、`广东`、`北京市`、`北京`）
    pub provinces: HashSet<&'static str>,
    /// 地级全称和去掉「市」的简称（`深圳市`、`深圳`）
    pub prefectures: HashSet<&'static str>,
    /// 县级全称（`南山区`、`昆山市`、`长白朝鲜族自治县`）
    pub counties: HashSet<&'static str>,
    /// 最长的名称有几个字
    pub longest: usize,
}

impl Divisions {
    /// 是不是一个行政区划的名称，哪一层都行
    pub fn any(&self, s: &str) -> bool {
        self.counties.contains(s) || self.prefectures.contains(s) || self.provinces.contains(s)
    }
}

/// 省级名称的简称：去掉「省」「市」和「…自治区」「特别行政区」
fn province_short(name: &'static str) -> &'static str {
    for suffix in [
        "壮族自治区",
        "回族自治区",
        "维吾尔自治区",
        "自治区",
        "特别行政区",
        "省",
        "市",
    ] {
        if let Some(s) = name.strip_suffix(suffix)
            && s.chars().count() >= 2
        {
            return s;
        }
    }
    name
}

pub(super) static DIVISIONS: LazyLock<Divisions> = LazyLock::new(|| {
    let mut d = Divisions {
        provinces: HashSet::new(),
        prefectures: HashSet::new(),
        counties: HashSet::new(),
        longest: 0,
    };
    for line in lines(include_str!("data/cn-divisions.txt")) {
        let mut parts = line.split(' ');
        let (Some(code), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        if code.len() == 2 {
            d.provinces.insert(name);
            d.provinces.insert(province_short(name));
        } else if name != "-" {
            d.prefectures.insert(name);
            if let Some(s) = name.strip_suffix('市')
                && s.chars().count() >= 2
            {
                d.prefectures.insert(s);
            }
        }
        d.counties.extend(parts);
    }
    d.longest = d
        .provinces
        .iter()
        .chain(&d.prefectures)
        .chain(&d.counties)
        .map(|n| n.chars().count())
        .max()
        .unwrap_or(0);
    d
});

/// 现行的地级行政区划代码（身份证号的前四位）
pub(super) static PREFECTURE_CODES: LazyLock<HashSet<u16>> = LazyLock::new(|| {
    lines(include_str!("data/cn-divisions.txt"))
        .filter_map(|l| l.split(' ').next())
        .filter(|c| c.len() == 4)
        .filter_map(|c| c.parse().ok())
        .collect()
});

// ---------------------------------------------------------------- 长途区号

/// 中国大陆的长途区号，去掉开头的 0：`10`、`20`…`29`、`311` …
pub(super) static AREA_CODES: LazyLock<HashSet<u16>> = LazyLock::new(|| {
    lines(include_str!("data/cn-area-codes.txt"))
        .flat_map(|l| l.split(' '))
        .filter_map(|c| c.parse().ok())
        .collect()
});

// ---------------------------------------------------------------- IBAN

/// 一个国家的 IBAN：总长，和 BBAN 那一段每一截的字符类型与位数
pub(super) struct IbanSpec {
    pub len: usize,
    /// `(类型, 位数)`：`F` 数字、`A` 大写字母或数字、`U` 大写字母
    pub parts: Vec<(u8, usize)>,
}

pub(super) static IBAN: LazyLock<HashMap<[u8; 2], IbanSpec>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    for line in lines(include_str!("data/iban.txt")) {
        let mut f = line.split(' ');
        let (Some(cc), Some(len), Some(fmt)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        let (Ok(len), Ok(cc)) = (len.parse(), <[u8; 2]>::try_from(cc.as_bytes())) else {
            continue;
        };
        let b = fmt.as_bytes();
        let parts = b
            .chunks(3)
            .filter_map(|c| {
                let n = std::str::from_utf8(&c[1..]).ok()?.parse().ok()?;
                Some((c[0], n))
            })
            .collect();
        m.insert(cc, IbanSpec { len, parts });
    }
    m
});

// ---------------------------------------------------------------- 国际电话

/// 国家码 → 国内号码可能有几位（同一个国家码下各地区的并集）。国家码互不为前缀
pub(super) static CALLING: LazyLock<HashMap<u16, Vec<u8>>> = LazyLock::new(|| {
    lines(include_str!("data/calling-codes.txt"))
        .filter_map(|l| {
            let (cc, lens) = l.split_once(' ')?;
            let lens = lens.split(',').filter_map(|n| n.parse().ok()).collect();
            Some((cc.parse().ok()?, lens))
        })
        .collect()
});

// ---------------------------------------------------------------- 国籍

/// ISO 3166-1 数字代码（2026-10 的 249 个）。新版外国人永久居留身份证的第 4–6 位是持证人
/// 国籍的这个代码
const ISO_NUMERIC: [u16; 249] = [
    4, 8, 10, 12, 16, 20, 24, 28, 31, 32, 36, 40, 44, 48, 50, 51, 52, 56, 60, 64, 68, 70, 72, 74,
    76, 84, 86, 90, 92, 96, 100, 104, 108, 112, 116, 120, 124, 132, 136, 140, 144, 148, 152, 156,
    158, 162, 166, 170, 174, 175, 178, 180, 184, 188, 191, 192, 196, 203, 204, 208, 212, 214, 218,
    222, 226, 231, 232, 233, 234, 238, 239, 242, 246, 248, 250, 254, 258, 260, 262, 266, 268, 270,
    275, 276, 288, 292, 296, 300, 304, 308, 312, 316, 320, 324, 328, 332, 334, 336, 340, 344, 348,
    352, 356, 360, 364, 368, 372, 376, 380, 384, 388, 392, 398, 400, 404, 408, 410, 414, 417, 418,
    422, 426, 428, 430, 434, 438, 440, 442, 446, 450, 454, 458, 462, 466, 470, 474, 478, 480, 484,
    492, 496, 498, 499, 500, 504, 508, 512, 516, 520, 524, 528, 531, 533, 534, 535, 540, 548, 554,
    558, 562, 566, 570, 574, 578, 580, 581, 583, 584, 585, 586, 591, 598, 600, 604, 608, 612, 616,
    620, 624, 626, 630, 634, 638, 642, 643, 646, 652, 654, 659, 660, 662, 663, 666, 670, 674, 678,
    682, 686, 688, 690, 694, 702, 703, 704, 705, 706, 710, 716, 724, 728, 729, 732, 740, 744, 748,
    752, 756, 760, 762, 764, 768, 772, 776, 780, 784, 788, 792, 795, 796, 798, 800, 804, 807, 818,
    826, 831, 832, 833, 834, 840, 850, 854, 858, 860, 862, 876, 882, 887, 894,
];

/// 是不是一个 ISO 3166-1 数字代码（[`ISO_NUMERIC`]）
pub(in crate::redact) fn nationality(code: u32) -> bool {
    u16::try_from(code).is_ok_and(|c| ISO_NUMERIC.binary_search(&c).is_ok())
}
