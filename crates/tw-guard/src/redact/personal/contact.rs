//! IBAN、手机号的几种写法、固定电话、国际电话；和几种分组写的号码（跨 token 接起来看）。

use super::super::rules::{Hit, builtin_hit};
use super::data::{AREA_CODES, CALLING, IBAN};
use super::{On, escape_before, ids, near};

fn digits(d: &[u8]) -> bool {
    !d.is_empty() && d.iter().all(u8::is_ascii_digit)
}

/// `at` 是不是紧跟在上一段（终点 `end`）后面、中间正好隔一个空格
fn spaced_after(text: &str, end: usize, at: usize) -> bool {
    at == end + 1 && text.as_bytes()[end] == b' '
}

// ---------------------------------------------------------------- IBAN

/// 各国 IBAN 规范、银行文档里登的样例。**开发者天天往代码、文档里贴它们**（和公开的测试卡号
/// 一样，见 `rules::TEST_CARDS`），换成占位符纯属打扰
const IBAN_SAMPLES: &[&[u8]] = &[
    b"DE89370400440532013000",
    b"GB82WEST12345698765432",
    b"GB29NWBK60161331926819",
    b"GB33BUKB20201555555555",
    b"FR1420041010050500013M02606",
    b"FR7630006000011234567890189",
    b"NL91ABNA0417164300",
    b"CH9300762011623852957",
    b"ES9121000418450200051332",
    b"IT60X0542811101000000123456",
    b"BE68539007547034",
    b"AT611904300234573201",
];

/// 一段 IBAN（去掉了空格）是否成立：国家查得到、总长对、BBAN 每一截的字符类型对，
/// 并且 MOD 97-10 余 1。公开的样例（[`IBAN_SAMPLES`]）不算
pub(super) fn iban(s: &[u8]) -> bool {
    let Some(spec) = s.get(..2).and_then(|cc| IBAN.get(&[cc[0], cc[1]])) else {
        return false;
    };
    if s.len() != spec.len || !s[2].is_ascii_digit() || !s[3].is_ascii_digit() {
        return false;
    }
    let mut at = 4;
    for &(kind, n) in &spec.parts {
        let Some(part) = s.get(at..at + n) else {
            return false;
        };
        let ok = match kind {
            b'F' => part.iter().all(u8::is_ascii_digit),
            b'U' => part.iter().all(u8::is_ascii_uppercase),
            _ => part
                .iter()
                .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase()),
        };
        if !ok {
            return false;
        }
        at += n;
    }
    // 头四位挪到末尾，字母换成 10–35，整个数除以 97 余 1。边读边取余，不做大数
    let mut r = 0u32;
    for &c in s[4..].iter().chain(&s[..4]) {
        r = if c.is_ascii_digit() {
            (r * 10 + u32::from(c - b'0')) % 97
        } else {
            (r * 100 + u32::from(c - b'A') + 10) % 97
        };
    }
    r == 1 && !IBAN_SAMPLES.contains(&s)
}

/// IBAN 的头四位：两个大写字母（查得到的国家）+ 两位数字
fn iban_head(b: &[u8]) -> bool {
    b.len() >= 4
        && b[0].is_ascii_uppercase()
        && b[1].is_ascii_uppercase()
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
}

/// 连着写的 IBAN 是一个 token；四位一组、隔一个空格写的（`DE89 3704 0044 0532 0130 00`）
/// 是一串 token，在这里接起来。**出厂开着**，每个 token 都过这一关：头两个字节对不上就回去
pub(super) struct IbanRun {
    active: bool,
    start: usize,
    end: usize,
    buf: [u8; 34],
    n: usize,
    want: usize,
}

impl Default for IbanRun {
    fn default() -> Self {
        IbanRun {
            active: false,
            start: 0,
            end: 0,
            buf: [0; 34],
            n: 0,
            want: 0,
        }
    }
}

impl IbanRun {
    /// 正接着一串分组写的
    pub(super) fn active(&self) -> bool {
        self.active
    }

    pub(super) fn reset(&mut self) {
        self.active = false;
    }

    pub(super) fn feed(&mut self, text: &str, body: &str, at: usize, out: &mut Vec<Hit>) {
        let b = body.as_bytes();
        if self.active {
            let last_full = self.n % 4 == 0;
            if last_full
                && spaced_after(text, self.end, at)
                && (1..=4).contains(&b.len())
                && self.n + b.len() <= self.want
                && b.iter()
                    .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
            {
                self.buf[self.n..self.n + b.len()].copy_from_slice(b);
                self.n += b.len();
                self.end = at + b.len();
                if self.n == self.want {
                    self.active = false;
                    if iban(&self.buf[..self.n]) {
                        out.push(builtin_hit("iban", self.start..self.end));
                    }
                }
                return;
            }
            self.active = false;
        }
        if !iban_head(b) {
            return;
        }
        if b.len() == 4 {
            if let Some(spec) = IBAN.get(&[b[0], b[1]]) {
                *self = IbanRun {
                    active: true,
                    start: at,
                    end: at + 4,
                    buf: [0; 34],
                    n: 4,
                    want: spec.len,
                };
                self.buf[..4].copy_from_slice(b);
            }
        } else if (15..=34).contains(&b.len()) && iban(b) {
            out.push(builtin_hit("iban", at..at + b.len()));
        }
    }
}

// ---------------------------------------------------------------- 分组写的号码

/// 一组：一个 token（去掉句末句号）
#[derive(Clone, Copy)]
struct Grp {
    start: usize,
    end: usize,
}

/// 隔一个空格连着写的几个 token：`138 1234 5678`、`2345 6789 0123`、`AB 12 34 56 C`。
///
/// 接起来的一串里每来一个新的，就看「到上一个为止」的几种写法成不成立 —— 这时才知道它
/// 后面跟着的是什么：后面紧跟着又一组数字的，是一个更长的号码的一部分（卡号、表格里的
/// 一行数），不算。只留最后七组：最长的写法（`+86` 加 3-4-4、英国保险号的五段）用不了更多
#[derive(Default)]
pub(super) struct Groups {
    g: Vec<Grp>,
}

impl Groups {
    pub(super) fn feed(&mut self, text: &str, body: &str, at: usize, on: &On, out: &mut Vec<Hit>) {
        let new = Grp {
            start: at,
            end: at + body.len(),
        };
        match self.g.last() {
            Some(last) if spaced_after(text, last.end, at) => {
                self.eval(text, Some(new), on, out);
                if self.g.len() == 7 {
                    self.g.remove(0);
                }
            }
            _ => {
                self.eval(text, None, on, out);
                self.g.clear();
            }
        }
        self.g.push(new);
    }

    pub(super) fn finish(&mut self, text: &str, on: &On, out: &mut Vec<Hit>) {
        self.eval(text, None, on, out);
        self.g.clear();
    }

    /// 到最后一组为止的几种写法。`next`：后面紧跟着的那一组
    fn eval(&self, text: &str, next: Option<Grp>, on: &On, out: &mut Vec<Hit>) {
        let g = &self.g;
        let n = g.len();
        if n < 3 {
            return;
        }
        let t = |x: &Grp| &text.as_bytes()[x.start..x.end];
        let num = |x: &Grp| digits(t(x));
        // 前后紧挨着的是不是数字组
        let next_num = next.is_some_and(|x| num(&x));
        let lens = |k: usize| g[n - k..].iter().map(|x| x.end - x.start);
        let last3 = &g[n - 3..];
        if !next_num && last3.iter().all(num) {
            let prev = (n >= 4).then(|| t(&g[n - 4]));
            let prev_num = prev.is_some_and(digits);
            let d: Vec<u8> = last3.iter().flat_map(|x| t(x).iter().copied()).collect();
            let span = last3[0].start..last3[2].end;
            // 3-4-4 的手机号，前面可以是 `86`、`0086`（`+86 138 1234 5678`）
            if on.phone
                && lens(3).eq([3, 4, 4])
                && (!prev_num || prev.is_some_and(|p| p == b"86" || p == b"0086"))
                && mobile(&d)
            {
                out.push(builtin_hit("cn-mobile-phone", span.clone()));
            }
            if lens(3).eq([4, 4, 4]) && !prev_num {
                if on.aadhaar && ids::aadhaar(&d) {
                    out.push(builtin_hit("in-aadhaar", span.clone()));
                }
                if on.jp && ids::my_number(&d) && near(text, span.start, true, ids::JP_WORDS) {
                    out.push(builtin_hit("jp-my-number", span.clone()));
                }
            }
        }
        // 英国国民保险号的五段写法：`AB 12 34 56 C`
        if on.nino && n >= 5 && lens(5).eq([2, 2, 2, 2, 1]) {
            let p = t(&g[n - 5]);
            let s = t(&g[n - 1])[0];
            if ids::nino_prefix(p[0], p[1])
                && g[n - 4..n - 1].iter().all(num)
                && (b'A'..=b'D').contains(&s)
            {
                out.push(builtin_hit("uk-nino", g[n - 5].start..g[n - 1].end));
            }
        }
    }
}

/// `1234-5678-9012` 这样四位一组、连字符隔开的 12 位数：去掉连字符的那 12 位
pub(super) fn grouped_4_4_4(b: &[u8]) -> Option<[u8; 12]> {
    if b.len() != 14 || b[4] != b'-' || b[9] != b'-' {
        return None;
    }
    let mut d = [0u8; 12];
    let mut n = 0;
    for &c in b.iter().filter(|&&c| c != b'-') {
        if !c.is_ascii_digit() || n == 12 {
            return None;
        }
        d[n] = c;
        n += 1;
    }
    (n == 12).then_some(d)
}

// ---------------------------------------------------------------- 手机号

/// 11 位、`1` 开头、第二位是 3–9
fn mobile(d: &[u8]) -> bool {
    d.len() == 11 && d[0] == b'1' && (b'3'..=b'9').contains(&d[1]) && digits(d)
}

/// 手机号在一个 token 里的另几种写法（11 位连写的由 `rules::mobile_phones` 认）：
///
/// - 前面粘着国家码：`+8613812345678`（`+` 不在 token 里，要看前一个字节）、`008613812345678`；
/// - 3-4-4 用连字符分开：`138-1234-5678`，前面可以是 `86-`、`0086-`、`+86-`。
///
/// 只换号码本身，国家码留着
pub(super) fn mobile_written(text: &str, body: &str, at: usize, out: &mut Vec<Hit>) {
    let b = body.as_bytes();
    if digits(b) {
        let plus = at > 0 && text.as_bytes()[at - 1] == b'+';
        let skip = match b.len() {
            13 if plus && b.starts_with(b"86") => 2,
            15 if b.starts_with(b"0086") => 4,
            _ => return,
        };
        if mobile(&b[skip..]) {
            out.push(builtin_hit("cn-mobile-phone", at + skip..at + b.len()));
        }
        return;
    }
    let parts: Vec<&[u8]> = b.split(|&c| c == b'-').collect();
    let k = parts.len();
    if !(3..=4).contains(&k) || parts.iter().any(|p| !digits(p)) {
        return;
    }
    if k == 4 && parts[0] != b"86" && parts[0] != b"0086" {
        return;
    }
    let tail = &parts[k - 3..];
    if tail.iter().map(|p| p.len()).eq([3, 4, 4]) {
        let d: Vec<u8> = tail.concat();
        if mobile(&d) {
            out.push(builtin_hit(
                "cn-mobile-phone",
                at + b.len() - 13..at + b.len(),
            ));
        }
    }
}

// ---------------------------------------------------------------- 固定电话

/// 区号（带开头的 0）和本地号码是不是一个大陆固定电话：区号查表；两位区号（`010`、`02x`）
/// 后面是 8 位，三位区号后面是 7 或 8 位；本地号码不以 0、1 开头
fn landline(area: &[u8], local: &[u8]) -> bool {
    if !digits(area)
        || !digits(local)
        || area.first() != Some(&b'0')
        || matches!(local[0], b'0' | b'1')
    {
        return false;
    }
    let code: u16 = std::str::from_utf8(&area[1..])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let ok_len = match area.len() {
        3 => local.len() == 8,
        4 => matches!(local.len(), 7 | 8),
        _ => false,
    };
    ok_len && AREA_CODES.contains(&code)
}

/// `010-12345678`、`0755-1234567`：区号和号码用连字符隔开，一个 token
pub(super) fn landline_dashed(body: &str, at: usize, out: &mut Vec<Hit>) {
    let b = body.as_bytes();
    let Some(dash) = b.iter().position(|&c| c == b'-') else {
        return;
    };
    if landline(&b[..dash], &b[dash + 1..]) {
        out.push(builtin_hit("cn-landline", at..at + b.len()));
    }
}

/// 左括号，半角或者全角
fn open_paren(b: &[u8], at: usize) -> usize {
    if at >= 1 && b[at - 1] == b'(' {
        1
    } else if at >= 3 && &b[at - 3..at] == "（".as_bytes() {
        3
    } else {
        0
    }
}

fn close_paren(b: &[u8], at: usize) -> usize {
    if b.get(at) == Some(&b')') {
        1
    } else if b.get(at..at + 3) == Some("）".as_bytes()) {
        3
    } else {
        0
    }
}

/// `(0755)1234567`、`（010） 12345678`：区号在括号里，号码是下一个 token
#[derive(Default)]
pub(super) struct LandlineParen {
    /// 左括号的位置、区号的区间、号码该从哪里开始（右括号后面，或者再隔一个空格）
    pending: Option<(usize, std::ops::Range<usize>, usize)>,
}

impl LandlineParen {
    pub(super) fn feed(&mut self, text: &str, body: &str, at: usize, out: &mut Vec<Hit>) {
        let b = text.as_bytes();
        if let Some((open, area, next)) = self.pending.take() {
            let gap = &b[next..at.max(next)];
            if (at == next || (at == next + 1 && gap == b" "))
                && landline(&b[area], body.as_bytes())
            {
                out.push(builtin_hit("cn-landline", open..at + body.len()));
                return;
            }
        }
        let t = body.as_bytes();
        if !(3..=4).contains(&t.len()) || t[0] != b'0' || !digits(t) {
            return;
        }
        let (open, close) = (open_paren(b, at), close_paren(b, at + t.len()));
        if open > 0 && close > 0 {
            self.pending = Some((at - open, at..at + t.len(), at + t.len() + close));
        }
    }
}

// ---------------------------------------------------------------- 国际电话

/// 国际电话：`+` + 国家码（查表）+ 该国规定位数的号码，中间可以有空格、连字符、括号
/// （`+1 (415) 555-2671`、`+44 20 7946 0958`、`+81-3-1234-5678`）。
///
/// 前面紧挨着字母数字的不算（`1e+10`、`a+123…`）；后面紧挨着字母数字的不算。国内号码至少
/// 7 位、整个号码不超过 15 位（E.164）
pub(in crate::redact) fn intl_phones(text: &str, out: &mut Vec<Hit>) {
    let b = text.as_bytes();
    for p in memchr::memchr_iter(b'+', b) {
        if p > 0 && b[p - 1].is_ascii_alphanumeric() && escape_before(b, p) == 0 {
            continue;
        }
        let mut d = [0u8; 15];
        let mut n = 0;
        let mut i = p + 1;
        let mut end = p;
        let mut seps = 0;
        while i < b.len() {
            let c = b[i];
            if c.is_ascii_digit() {
                if n == 15 {
                    n = 16;
                    break;
                }
                d[n] = c;
                n += 1;
                end = i + 1;
                seps = 0;
            } else if matches!(c, b' ' | b'-' | b'(' | b')' | b'.') && n > 0 && seps < 2 {
                seps += 1;
            } else {
                break;
            }
            i += 1;
        }
        if !(8..=15).contains(&n) || b.get(end).is_some_and(u8::is_ascii_alphanumeric) {
            continue;
        }
        let d = &d[..n];
        let cc = (1..=3).find_map(|k| {
            let code: u16 = std::str::from_utf8(&d[..k]).ok()?.parse().ok()?;
            CALLING.get(&code).map(|lens| (k, lens))
        });
        let Some((k, lens)) = cc else {
            continue;
        };
        // 少于 8 位的多半是只在本地拨的短号（美国的 7 位），除非这个国家的号码本来就短
        let national = n - k;
        let longest = lens.iter().copied().max().unwrap_or(0);
        if national >= 7 && (national >= 8 || longest < 8) && lens.contains(&(national as u8)) {
            out.push(builtin_hit("intl-phone", p..end));
        }
    }
}
