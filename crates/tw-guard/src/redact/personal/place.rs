//! 详细地址、精确坐标、车牌号。

use std::sync::LazyLock;

use aho_corasick::AhoCorasick;

use super::super::rules::{Hit, builtin_hit};
use super::data::DIVISIONS;

// ---------------------------------------------------------------- 详细地址

/// 行政区划名称以这几个字结尾：县级的「区」「县」「市」「旗」，地级的「市」「州」「盟」
/// 「地区」。在正文里先找这几个字，再往前查表 —— 不必拿三千多个名称挨个去找
static ADMIN_END: LazyLock<AhoCorasick> =
    LazyLock::new(|| AhoCorasick::new(["区", "县", "市", "旗", "州", "盟"]).expect("固定的几个字"));

/// 街路巷村：门牌前面得有一个
const ROADS: &[&str] = &[
    "路", "街", "道", "巷", "弄", "村", "里", "胡同", "大道", "屯", "组",
];

/// 门牌：数字后面跟着其中一个
const DOORS: &[&str] = &[
    "号楼", "号院", "号", "栋", "幢", "座", "单元", "层", "楼", "室", "房", "户",
];

/// 门牌里的数字：阿拉伯数字、字母（`A座`）、中文数字
fn door_digit(c: char) -> bool {
    c.is_ascii_alphanumeric() || "零〇一二三四五六七八九十百".contains(c)
}

/// 地址里能有的字：汉字、字母数字、连字符、`#`。标点、空白、引号、反斜杠（JSON 的转义）都是
/// 地址的边界
fn addr_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(c, '-' | '#' | '－')
        || ('\u{4e00}'..='\u{9fff}').contains(&c)
}

/// 以 `end` 结尾的最长的一个行政区划名称的起点。`sets` 选查哪几层
fn name_ending_at(text: &str, end: usize, county: bool) -> Option<usize> {
    let d = &*DIVISIONS;
    let mut best = None;
    let mut start = end;
    for len in 1..=d.longest {
        let Some((i, _)) = text[..start].char_indices().next_back() else {
            break;
        };
        start = i;
        if len < 2 {
            continue;
        }
        let s = &text[start..end];
        if (county && d.counties.contains(s))
            || d.prefectures.contains(s)
            || d.provinces.contains(s)
        {
            best = Some(start);
        }
    }
    best
}

/// 从 `at` 起有没有一个门牌：数字（字母）后面跟着门牌字。返回它的终点
fn door_at(text: &str, at: usize) -> Option<usize> {
    let rest = &text[at..];
    let n: usize = rest
        .chars()
        .take_while(|&c| door_digit(c))
        .map(char::len_utf8)
        .sum();
    if n == 0 || !rest[..n].chars().any(|c| !c.is_ascii_alphabetic()) {
        return None;
    }
    DOORS
        .iter()
        .find(|d| rest[n..].starts_with(*d))
        .map(|d| at + n + d.len())
}

/// 中国大陆的详细地址：（省级）+ 地级或县级名称（查表）+ 街路巷村 + 门牌。
///
/// 从区划名称往后看至多 40 个字：中间要有一个街路巷村，其后要有门牌（`号`、`栋`、`单元`、`室`
/// 至少一个）。**只到区县、不含门牌的不算**：「北京市朝阳区」是一个地方，不是谁的住址。
/// 前面紧挨着的省、市名称一起换（`广东省深圳市南山区…`）
pub(in crate::redact) fn addresses(text: &str, out: &mut Vec<Hit>) {
    let mut covered = 0;
    for m in ADMIN_END.find_iter(text) {
        let end = m.end();
        if end <= covered {
            continue;
        }
        let Some(mut start) = name_ending_at(text, end, true) else {
            continue;
        };
        if start < covered {
            continue;
        }
        let Some(stop) = tail(text, end) else {
            continue;
        };
        // 往前接上紧挨着的省、市名称
        while let Some(s) = name_ending_at(text, start, false) {
            start = s;
        }
        out.push(builtin_hit("cn-address", start..stop));
        covered = stop;
    }
}

/// 区划名称后面那一段：街路巷村之后的门牌，接着的几段门牌（`88号院3号楼2单元1201室`）一起
/// 算上。返回地址的终点；不成立是 `None`
fn tail(text: &str, from: usize) -> Option<usize> {
    let mut road = None;
    let mut end = None;
    let mut i = from;
    let mut chars = 0;
    while i < text.len() && chars < 40 {
        let rest = &text[i..];
        let c = rest.chars().next()?;
        if !addr_char(c) {
            break;
        }
        if road.is_none() {
            if let Some(r) = ROADS.iter().find(|r| rest.starts_with(*r)) {
                road = Some(i);
                i += r.len();
                chars += r.chars().count();
                continue;
            }
        } else if let Some(e) = door_at(text, i) {
            end = Some(e);
            chars += text[i..e].chars().count();
            i = e;
            continue;
        }
        i += c.len_utf8();
        chars += 1;
    }
    end
}

/// 一段地址开头的行政区划名称（`北京市朝阳区`）。打码时只留这一段
pub(super) fn county_prefix(value: &str) -> Option<&str> {
    let d = &*DIVISIONS;
    let mut at = 0;
    'next: while at < value.len() {
        let rest = &value[at..];
        let mut ends: Vec<usize> = rest.char_indices().map(|(i, _)| i).skip(2).collect();
        ends.push(rest.len());
        for &e in ends.iter().take(d.longest).rev() {
            if d.any(&rest[..e]) {
                at += e;
                continue 'next;
            }
        }
        break;
    }
    (at > 0).then(|| &value[..at])
}

// ---------------------------------------------------------------- 精确坐标

/// 一个十进制小数：可以带负号，整数部分 1–3 位，小数至少 5 位（约 1 米）
fn coordinate(s: &str) -> Option<f64> {
    let t = s.strip_prefix('-').unwrap_or(s);
    let (int, frac) = t.split_once('.')?;
    if !(1..=3).contains(&int.len())
        || frac.len() < 5
        || frac.len() > 15
        || !int.bytes().all(|c| c.is_ascii_digit())
        || !frac.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    s.parse().ok()
}

/// 两个数之间的那一段让它们成对：只有逗号、空格、分号（`31.23045, 121.47370`），或者是
/// 经度、纬度的键名（`,"lng":`、`, longitude: `）
fn pairs(gap: &str) -> bool {
    let g = gap.as_bytes();
    if g.len() <= 4 && g.iter().all(|c| matches!(c, b',' | b' ' | b';')) && !g.is_empty() {
        return true;
    }
    if gap == "，" || gap == "， " {
        return true;
    }
    if g.len() > 40
        || !g
            .iter()
            .all(|&c| c.is_ascii_alphabetic() || b" ,;:=\"'\\_&".contains(&c))
    {
        return false;
    }
    let l = gap.to_ascii_lowercase();
    l.contains("lng") || l.contains("lon") || l.contains("lat")
}

/// 一对坐标：纬度、经度（或者反过来，GeoJSON 是经度在前）各一个，取值在范围内，小数都不少于
/// 5 位。**只认正好两个数成对的**：三个以上连着的是一列数（向量、包围盒），不是一个位置；
/// 两个数都小于 1 的多半是概率、权重。每个数各换成一个占位符
#[derive(Default)]
pub(super) struct GeoChain {
    /// 连着的几个数：起点、终点、值
    nums: Vec<(usize, usize, f64)>,
}

impl GeoChain {
    pub(super) fn feed(&mut self, text: &str, body: &str, at: usize, out: &mut Vec<Hit>) {
        let Some(v) = coordinate(body) else {
            return;
        };
        let linked = self
            .nums
            .last()
            .is_some_and(|&(_, end, _)| end <= at && pairs(&text[end..at]));
        if !linked {
            self.finish(text, out);
        }
        if self.nums.len() < 3 {
            self.nums.push((at, at + body.len(), v));
        }
    }

    pub(super) fn finish(&mut self, _text: &str, out: &mut Vec<Hit>) {
        if let [(s1, e1, a), (s2, e2, b)] = self.nums[..] {
            let lat = |x: f64| (-90.0..=90.0).contains(&x);
            let lon = |x: f64| (-180.0..=180.0).contains(&x);
            if ((lat(a) && lon(b)) || (lon(a) && lat(b))) && a.abs().max(b.abs()) >= 1.0 {
                out.push(builtin_hit("geo-coordinates", s1..e1));
                out.push(builtin_hit("geo-coordinates", s2..e2));
            }
        }
        self.nums.clear();
    }
}

/// 坐标打码：留两位小数，`31.23…`
pub(super) fn mask_coordinate(value: &str) -> String {
    match value.split_once('.') {
        Some((int, frac)) => format!("{int}.{}…", &frac[..frac.len().min(2)]),
        None => "…".to_string(),
    }
}

// ---------------------------------------------------------------- 车牌号

/// 车牌开头的省份简称
const PLATE_PROVINCES: &str = "京津沪渝冀豫云辽黑湘皖鲁新苏浙赣鄂桂甘晋蒙陕吉闽贵粤青藏川宁琼";

/// 车牌的字母、数字：大写，没有 `I`、`O`
fn plate_char(c: u8) -> bool {
    (c.is_ascii_uppercase() && c != b'I' && c != b'O') || c.is_ascii_digit()
}

/// 车牌号省份简称和发牌机关字母之后的那一段：
///
/// - 普通号牌 5 位，至多两个字母（其余是数字）；
/// - 新能源号牌 6 位：小型车第一位是 `D`/`F`、末四位是数字，大型车前五位是数字、末位是 `D`/`F`
fn plate_tail(t: &[u8]) -> bool {
    if !t.iter().all(|&c| plate_char(c)) {
        return false;
    }
    let digits = |s: &[u8]| s.iter().all(u8::is_ascii_digit);
    match t.len() {
        5 => t.iter().filter(|c| c.is_ascii_digit()).count() >= 3,
        6 => {
            (matches!(t[0], b'D' | b'F') && digits(&t[2..]))
                || (digits(&t[..5]) && matches!(t[5], b'D' | b'F'))
        }
        _ => false,
    }
}

/// `at` 前面紧挨着的是不是一个省份简称，是的话返回它的起点
fn province_before(text: &str, at: usize) -> Option<usize> {
    let (i, c) = text[..at].char_indices().next_back()?;
    PLATE_PROVINCES.contains(c).then_some(i)
}

/// 车牌号：省份简称 + 字母 + 5 位（新能源 6 位）。字母、数字之间可以隔一个 `·` 或空格
/// （`京A·12345`），这时是两个 token，在这里接起来
#[derive(Default)]
pub(super) struct PlatePending {
    /// 省份简称的起点、发牌机关字母之后号码该从哪里开始
    pending: Option<(usize, usize)>,
}

impl PlatePending {
    pub(super) fn feed(&mut self, text: &str, body: &str, at: usize, out: &mut Vec<Hit>) {
        let b = body.as_bytes();
        if let Some((start, next)) = self.pending.take()
            && at == next
            && plate_tail(b)
        {
            out.push(builtin_hit("cn-license-plate", start..at + b.len()));
            return;
        }
        if !b[0].is_ascii_uppercase() || !plate_char(b[0]) {
            return;
        }
        let Some(start) = province_before(text, at) else {
            return;
        };
        if b.len() > 1 {
            if plate_tail(&b[1..]) {
                out.push(builtin_hit("cn-license-plate", start..at + b.len()));
            }
            return;
        }
        // 只有一个字母：后面隔一个 `·`、`•` 或空格接着号码
        let rest = &text[at + 1..];
        let sep = ["·", "•", " "]
            .iter()
            .find(|s| rest.starts_with(*s))
            .map_or(0, |s| s.len());
        if sep > 0 {
            self.pending = Some((start, at + 1 + sep));
        }
    }
}
