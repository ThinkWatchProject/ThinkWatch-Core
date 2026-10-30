//! 证件号码：15 位旧身份证号、护照机读区、中国护照号、港澳台身份证号、通行证；境外证件号码。

use super::super::rules::{Hit, builtin_hit, days_in};
use super::data::PREFECTURE_CODES;
use super::escape_before;

fn num(d: &[u8]) -> u32 {
    d.iter().fold(0, |v, c| v * 10 + u32::from(c - b'0'))
}

fn digits(d: &[u8]) -> bool {
    !d.is_empty() && d.iter().all(u8::is_ascii_digit)
}

/// 年月日是一个真实的日期
fn real_date(year: u32, month: u32, day: u32) -> bool {
    (1..=12).contains(&month) && day >= 1 && day <= days_in(year, month)
}

// ---------------------------------------------------------------- 15 位旧身份证号

/// 15 位的旧身份证号：6 位地区码 + `YYMMDD` + 3 位顺序码，没有校验位。
///
/// **没有校验位，所以地区码查到地级**：前四位要是现行的地级行政区划代码（[`PREFECTURE_CODES`]），
/// 第 5–6 位不是 `00`，出生日期是 19YY 年的真实日期。撤销、改过代码的老地区认不出 —— 宁可漏
pub(super) fn resident_id_15(b: &[u8], today: u32) -> bool {
    if b.len() != 15 || !digits(b) {
        return false;
    }
    if !PREFECTURE_CODES.contains(&(num(&b[..4]) as u16)) || &b[4..6] == b"00" {
        return false;
    }
    let (year, month, day) = (1900 + num(&b[6..8]), num(&b[8..10]), num(&b[10..12]));
    real_date(year, month, day) && year * 10_000 + month * 100 + day <= today
}

// ---------------------------------------------------------------- 护照机读区

/// ICAO 9303 的字符值：数字是自己，`A`–`Z` 是 10–35，`<` 是 0
fn mrz_value(c: u8) -> u32 {
    match c {
        b'0'..=b'9' => u32::from(c - b'0'),
        b'A'..=b'Z' => u32::from(c - b'A') + 10,
        _ => 0,
    }
}

/// 权重 7、3、1 轮着乘，和除以 10 的余数
fn mrz_check(s: &[u8]) -> u32 {
    s.iter()
        .zip([7, 3, 1].iter().cycle())
        .map(|(&c, w)| mrz_value(c) * w)
        .sum::<u32>()
        % 10
}

fn mrz_char(c: u8) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'<'
}

/// 护照机读区（TD3）的第二行：护照号、出生日期、有效期、个人号码四处校验位和整行的综合
/// 校验位全对。下标从 0 数（标准里从 1 数）
fn mrz_line2(l: &[u8]) -> bool {
    if l.len() != 44 || !l.iter().all(|&c| mrz_char(c)) {
        return false;
    }
    let check = |field: &[u8], at: usize| {
        l[at].is_ascii_digit() && mrz_check(field) == u32::from(l[at] - b'0')
    };
    // 个人号码一位都没有时，它的校验位可以是 `<`
    let personal_ok = check(&l[28..42], 42) || (l[28..43].iter().all(|&c| c == b'<'));
    let date = |d: &[u8]| digits(d) && real_date(2000 + num(&d[..2]), num(&d[2..4]), num(&d[4..6]));
    let mut composite = Vec::with_capacity(39);
    composite.extend_from_slice(&l[0..10]);
    composite.extend_from_slice(&l[13..20]);
    composite.extend_from_slice(&l[21..43]);
    check(&l[0..9], 9)
        && check(&l[13..19], 19)
        && check(&l[21..27], 27)
        && personal_ok
        && check(&composite, 43)
        && date(&l[13..19])
        && date(&l[21..27])
        && matches!(l[20], b'M' | b'F' | b'X' | b'<')
        && l[10..13]
            .iter()
            .all(|&c| c.is_ascii_uppercase() || c == b'<')
}

/// 两行之间的换行：JSON 里是 `\n` 或 `\r\n` 两个、四个字符，纯文本里是换行符本身
fn newline_at(b: &[u8], at: usize) -> usize {
    for nl in [&b"\\r\\n"[..], b"\\n", b"\r\n", b"\n"] {
        if b[at..].starts_with(nl) {
            return nl.len();
        }
    }
    0
}

/// 护照机读区：两行各 44 个字符，第一行 `P` 开头、只有字母和 `<`，第二行的五处校验位全对。
/// **两行整段换掉**（第一行是姓名）。
///
/// 第一行一定有 `<<`（姓和名之间）：先按字节找它（SIMD），一段正文里没有 `<<` 的话这条规则
/// 就只花这一次查找
pub(in crate::redact) fn mrz(text: &str, out: &mut Vec<Hit>) {
    let b = text.as_bytes();
    let mut from = 0;
    while let Some(i) = memchr::memmem::find(&b[from..], b"<<") {
        let p = from + i;
        let mut s = p;
        while s > 0 && mrz_char(b[s - 1]) {
            s -= 1;
        }
        let mut e = p;
        while e < b.len() && mrz_char(b[e]) {
            e += 1;
        }
        from = e;
        // 前面紧挨着一个 `é` 这样的转义的话，它的十六进制位也连进来了
        if e - s > 44 && escape_before(b, e - 44) > 0 {
            s = e - 44;
        }
        let line1 = &b[s..e];
        if line1.len() != 44
            || line1[0] != b'P'
            || !line1.iter().all(|&c| c.is_ascii_uppercase() || c == b'<')
        {
            continue;
        }
        let nl = newline_at(b, e);
        if nl == 0 {
            continue;
        }
        let l2 = e + nl;
        let end = l2 + 44;
        if end > b.len() || b.get(end).is_some_and(|&c| mrz_char(c)) || !mrz_line2(&b[l2..end]) {
            continue;
        }
        out.push(builtin_hit("passport-mrz", s..end));
        from = end;
    }
}

/// 机读区打码：只留护照号的最后四位
pub(super) fn mask_mrz(value: &str) -> String {
    let b = value.as_bytes();
    let line2 = &b[b.len().saturating_sub(44)..];
    let no: String = line2
        .iter()
        .take(9)
        .filter(|&&c| c != b'<')
        .map(|&c| char::from(c))
        .collect();
    let tail: String = no.chars().skip(no.len().saturating_sub(4)).collect();
    format!("…{tail}")
}

// ---------------------------------------------------------------- 中国护照、通行证

pub(super) const PASSPORT_WORDS: &[&str] = &["护照", "護照", "passport"];

/// 中国护照号：`E` + 8 位数字，或者 `E` + 一个字母（不含 `I`、`O`）+ 7 位数字。没有校验位，
/// 调用方还要看前面写没写「护照」
pub(super) fn cn_passport(b: &[u8]) -> bool {
    b.len() == 9
        && b[0] == b'E'
        && (digits(&b[1..])
            || (b[1].is_ascii_uppercase() && !matches!(b[1], b'I' | b'O') && digits(&b[2..])))
}

pub(super) const HM_PERMIT_WORDS: &[&str] = &["通行证", "通行證", "回乡证", "回鄉證"];
pub(super) const TAIWAN_PERMIT_WORDS: &[&str] = &["台胞证", "台胞證", "台湾居民", "臺灣居民"];

/// 港澳居民来往内地通行证：`H`（香港）或 `M`（澳门）+ 8 位数字，后面可以带 2 位换证次数
pub(super) fn hm_permit(b: &[u8]) -> bool {
    matches!(b.len(), 9 | 11) && matches!(b[0], b'H' | b'M') && digits(&b[1..])
}

/// 台湾居民来往大陆通行证：8 位数字，后面可以带 2 位换证次数
pub(super) fn taiwan_permit(b: &[u8]) -> bool {
    matches!(b.len(), 8 | 10) && digits(b)
}

// ---------------------------------------------------------------- 港澳台身份证

/// 香港身份证：1–2 个字母 + 6 位数字 + 校验位（`0`–`9` 或 `A`），校验位可以写在括号里。
///
/// 校验：只有一个字母的前面补一个值为 36 的位；字母 `A`–`Z` 是 10–35；八位依次乘 9 到 2，
/// 校验位是 11 减去和除以 11 的余数（10 写成 `A`，11 写成 `0`）。返回号码的终点（括号写法
/// 含右括号）
pub(super) fn hk_id(text: &str, at: usize, b: &[u8]) -> Option<usize> {
    let letters = b.iter().take_while(|c| c.is_ascii_uppercase()).count();
    if !(1..=2).contains(&letters) {
        return None;
    }
    let rest = &b[letters..];
    let (body, check, end) = match rest.len() {
        7 if digits(&rest[..6]) => (&rest[..6], rest[6], at + b.len()),
        6 if digits(rest) => {
            let t = text.as_bytes();
            let p = at + b.len();
            match t.get(p..p + 3) {
                Some([b'(', c, b')']) => (rest, *c, p + 3),
                _ => return None,
            }
        }
        _ => return None,
    };
    let v = |c: u8| u32::from(c - b'A') + 10;
    let mut sum = if letters == 1 {
        36 * 9 + v(b[0]) * 8
    } else {
        v(b[0]) * 9 + v(b[1]) * 8
    };
    for (i, &d) in body.iter().enumerate() {
        sum += u32::from(d - b'0') * (7 - i as u32);
    }
    let want = match 11 - sum % 11 {
        10 => b'A',
        11 => b'0',
        n => b'0' + n as u8,
    };
    (check == want).then_some(end)
}

/// 台湾身份证、新式居留证的字母对应的两位数
const TW_LETTER: [u8; 26] = [
    10, 11, 12, 13, 14, 15, 16, 17, 34, 18, 19, 20, 21, 22, 35, 23, 24, 25, 26, 27, 28, 29, 32, 30,
    31, 33,
];

/// 台湾身份证（第二位 `1`/`2`）和新式居留证（`8`/`9`）：1 个字母 + 9 位数字。字母换成两位数，
/// 连同后面 9 位依次乘 1、9、8 … 2、1、1，和是 10 的倍数
pub(super) fn taiwan_id(b: &[u8]) -> bool {
    if b.len() != 10
        || !b[0].is_ascii_uppercase()
        || !matches!(b[1], b'1' | b'2' | b'8' | b'9')
        || !digits(&b[1..])
    {
        return false;
    }
    let n = u32::from(TW_LETTER[usize::from(b[0] - b'A')]);
    let mut sum = n / 10 + (n % 10) * 9;
    for (i, &d) in b[1..].iter().enumerate() {
        let w = if i == 8 { 1 } else { 8 - i as u32 };
        sum += u32::from(d - b'0') * w;
    }
    sum % 10 == 0
}

pub(super) const MACAU_WORDS: &[&str] = &["澳门", "澳門", "macau", "macao"];

/// 澳门身份证：`1`/`5`/`7` + 6 位数字 + 括号里的一位校验位。**校验算法没有公开**，只认格式，
/// 调用方还要看前面写没写「澳门」
pub(super) fn macau_id(text: &str, at: usize, b: &[u8]) -> bool {
    let p = at + b.len();
    b.len() == 7
        && matches!(b[0], b'1' | b'5' | b'7')
        && matches!(text.as_bytes().get(p..p + 3), Some([b'(', c, b')']) if c.is_ascii_digit())
}

// ---------------------------------------------------------------- 境外证件

/// 公开登在广告、样张上的社会安全号：从来不是谁的号码，可写进文档的地方到处都是
const SSN_SAMPLES: &[&[u8]] = &[b"078-05-1120", b"219-09-9999", b"123-45-6789"];

/// 美国社会安全号：`ddd-dd-dddd`（必须带连字符）。区号不是 `000`、`666`、`9xx`，组号不是
/// `00`，序号不是 `0000`
pub(super) fn us_ssn(b: &[u8]) -> bool {
    b.len() == 11
        && b[3] == b'-'
        && b[6] == b'-'
        && digits(&b[..3])
        && digits(&b[4..6])
        && digits(&b[7..])
        && &b[..3] != b"000"
        && &b[..3] != b"666"
        && b[0] != b'9'
        && &b[4..6] != b"00"
        && &b[7..] != b"0000"
        && !SSN_SAMPLES.contains(&b)
}

/// 英国国民保险号的两个字母：第一个不是 D F I Q U V，第二个不是 D F I O Q U V，
/// 也不是 BG GB KN NK NT TN ZZ 这几个不发的组合
pub(super) fn nino_prefix(a: u8, c: u8) -> bool {
    a.is_ascii_uppercase()
        && c.is_ascii_uppercase()
        && !b"DFIQUV".contains(&a)
        && !b"DFIOQUV".contains(&c)
        && ![b"BG", b"GB", b"KN", b"NK", b"NT", b"TN", b"ZZ"].contains(&&[a, c])
}

/// 英国国民保险号：两个字母（[`nino_prefix`]）+ 6 位数字 + `A`–`D`
pub(super) fn uk_nino(b: &[u8]) -> bool {
    b.len() == 9 && nino_prefix(b[0], b[1]) && digits(&b[2..8]) && (b'A'..=b'D').contains(&b[8])
}

/// 新加坡 NRIC / FIN：`S`/`T`/`F`/`G` + 7 位数字 + 校验字母。七位依次乘 2、7、6、5、4、3、2，
/// `T`/`G` 再加 4，除以 11 的余数在两张表里查。`M` 系列的校验表没有公开，不收
pub(super) fn sg_nric(b: &[u8]) -> bool {
    if b.len() != 9 || !matches!(b[0], b'S' | b'T' | b'F' | b'G') || !digits(&b[1..8]) {
        return false;
    }
    let mut sum: u32 = b[1..8]
        .iter()
        .zip([2, 7, 6, 5, 4, 3, 2])
        .map(|(&d, w)| u32::from(d - b'0') * w)
        .sum();
    if matches!(b[0], b'T' | b'G') {
        sum += 4;
    }
    let table = if matches!(b[0], b'S' | b'T') {
        b"JZIHGFEDCBA"
    } else {
        b"XWUTRQPNMLK"
    };
    b[8] == table[(sum % 11) as usize]
}

/// 韩国居民登录号：`YYMMDD-G######`。出生日期是真实的、不在今天以后，性别位 1–8（1、2、5、6
/// 是 1900 年代，3、4、7、8 是 2000 年代）。**不验末位**：2020 年 10 月起后六位是随机的
pub(super) fn kr_rrn(b: &[u8], today: u32) -> bool {
    if b.len() != 14 || b[6] != b'-' || !digits(&b[..6]) || !digits(&b[7..]) {
        return false;
    }
    let century = match b[7] {
        b'1' | b'2' | b'5' | b'6' => 1900,
        b'3' | b'4' | b'7' | b'8' => 2000,
        _ => return false,
    };
    let (year, month, day) = (century + num(&b[..2]), num(&b[2..4]), num(&b[4..6]));
    real_date(year, month, day) && year * 10_000 + month * 100 + day <= today
}

pub(super) const JP_WORDS: &[&str] = &[
    "マイナンバー",
    "個人番号",
    "个人编号",
    "my number",
    "mynumber",
    "my_number",
];

/// 日本个人番号：12 位，末位校验。从校验位左边那一位往左数第 n 位乘 n+1（n ≤ 6）或 n−5，
/// 和除以 11 的余数不大于 1 的校验位是 0，否则是 11 减余数
pub(super) fn my_number(d: &[u8]) -> bool {
    if d.len() != 12 || !digits(d) || d.iter().all(|&c| c == d[0]) {
        return false;
    }
    let sum: u32 = (1..=11u32)
        .map(|n| {
            let p = u32::from(d[11 - n as usize] - b'0');
            p * if n <= 6 { n + 1 } else { n - 5 }
        })
        .sum();
    let r = sum % 11;
    let check = if r <= 1 { 0 } else { 11 - r };
    u32::from(d[11] - b'0') == check
}

/// Verhoeff 的乘法表
const VD: [[u8; 10]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
    [1, 2, 3, 4, 0, 6, 7, 8, 9, 5],
    [2, 3, 4, 0, 1, 7, 8, 9, 5, 6],
    [3, 4, 0, 1, 2, 8, 9, 5, 6, 7],
    [4, 0, 1, 2, 3, 9, 5, 6, 7, 8],
    [5, 9, 8, 7, 6, 0, 4, 3, 2, 1],
    [6, 5, 9, 8, 7, 1, 0, 4, 3, 2],
    [7, 6, 5, 9, 8, 2, 1, 0, 4, 3],
    [8, 7, 6, 5, 9, 3, 2, 1, 0, 4],
    [9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
];
/// Verhoeff 的置换表
const VP: [[u8; 10]; 8] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
    [1, 5, 7, 6, 2, 8, 3, 0, 9, 4],
    [5, 8, 0, 3, 7, 9, 6, 1, 4, 2],
    [8, 9, 1, 6, 0, 4, 3, 5, 2, 7],
    [9, 4, 5, 3, 1, 2, 6, 8, 7, 0],
    [4, 2, 8, 6, 5, 7, 3, 9, 0, 1],
    [2, 7, 9, 3, 8, 0, 6, 4, 1, 5],
    [7, 0, 4, 6, 9, 1, 3, 2, 5, 8],
];

/// Verhoeff 校验：从右往左过一遍，结果是 0
pub(super) fn verhoeff(d: &[u8]) -> bool {
    let mut c = 0u8;
    for (i, &x) in d.iter().rev().enumerate() {
        c = VD[usize::from(c)][usize::from(VP[i % 8][usize::from(x - b'0')])];
    }
    c == 0
}

/// 印度 Aadhaar：12 位数字，首位 2–9，Verhoeff 校验
pub(super) fn aadhaar(d: &[u8]) -> bool {
    d.len() == 12
        && digits(d)
        && (b'2'..=b'9').contains(&d[0])
        && !d.iter().all(|&c| c == d[0])
        && verhoeff(d)
}

/// 巴西 CPF：必须写作 `ddd.ddd.ddd-dd`。两位校验：前 9 位依次乘 10 到 2、前 10 位依次乘
/// 11 到 2，余数乘 10 再除以 11（得 10 记作 0）。11 位全一样的不算（`111.111.111-11` 过得了校验）
pub(super) fn br_cpf(b: &[u8]) -> bool {
    if b.len() != 14 || b[3] != b'.' || b[7] != b'.' || b[11] != b'-' {
        return false;
    }
    let d: Vec<u8> = b
        .iter()
        .filter(|c| c.is_ascii_digit())
        .map(|c| c - b'0')
        .collect();
    if d.len() != 11 || d.iter().all(|&c| c == d[0]) {
        return false;
    }
    let check = |n: usize| {
        let sum: u32 = (0..n)
            .map(|i| u32::from(d[i]) * (n as u32 + 1 - i as u32))
            .sum();
        (sum * 10 % 11 % 10) as u8
    };
    check(9) == d[9] && check(10) == d[10]
}
