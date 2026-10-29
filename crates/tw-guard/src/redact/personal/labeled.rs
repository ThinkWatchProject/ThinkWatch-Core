//! 按字段名认的个人信息：姓名、住址、出生日期、账号、健康。
//!
//! 内容本身没有结构可依（`张三`、`高血压`），**只看它写在哪个字段名后面**。认这几种写法：
//!
//! - JSON：`"real_name": "张三"`，请求体本身的，和正文里一段 JSON 的（`\"real_name\": \"张三\"`）；
//! - 配置、代码：`real_name = "张三"`、`real_name: 张三`（YAML）、`real_name=张三`（查询串）；
//! - 中文表单：`姓名：张三`、`姓名: 张三`。
//!
//! 字段名不分大小写，`_`、`-`、空格、驼峰都算分隔（`real_name`、`realName`、`Real Name` 是同一个）；
//! 长一点的字段名以表里的一项结尾也算（`patient_diagnosis`、`患者姓名`）。
//!
//! **裸的 `name`、`address` 另算**：工具定义里每个工具都有 `name`，服务器配置里都有
//! `address`，它们几乎从来不是一个人的姓名、住址。只有同一个 JSON 对象里（不是 JSON 的，前后
//! 两行以内）还有另一项个人信息字段（`phone`、`email`、`身份证号`、上面几张表里别的字段）时
//! 才算。
//!
//! 值是占位写法（`${NAME}`、`<your-name>`、`xxx`）、类型名（`string`、`String`、`date`）、
//! 代码表达式（`user.name`、`request.form["name"]`）的不换。

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::LazyLock;

use super::super::common::placeholder_value;
use super::super::rules::{Hit, builtin_hit, json_string_spans};
use super::{escape_before, unescape};

/// 值是哪一种：各有各的样子
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Address,
    Birth,
    Account,
    Health,
}

/// 一条按字段名认的规则
pub(super) struct LabeledRule {
    pub id: &'static str,
    names: &'static [&'static str],
    field: Field,
}

pub(in crate::redact) const NAME_FIELDS: &[&str] = &[
    "姓名",
    "真实姓名",
    "联系人",
    "收件人",
    "收货人",
    "real_name",
    "full_name",
    "legal_name",
    "first_name",
    "last_name",
    "given_name",
    "family_name",
    "name",
];
pub(in crate::redact) const ADDRESS_FIELDS: &[&str] = &[
    "住址",
    "家庭住址",
    "收货地址",
    "联系地址",
    "home_address",
    "shipping_address",
    "residential_address",
    "address",
];
pub(in crate::redact) const BIRTH_FIELDS: &[&str] = &[
    "出生日期",
    "生日",
    "birthday",
    "birth_date",
    "date_of_birth",
    "dob",
];
pub(in crate::redact) const ACCOUNT_FIELDS: &[&str] = &[
    "银行账号",
    "开户账号",
    "银行卡号",
    "account_number",
    "bank_account",
    "bank_account_number",
];
pub(in crate::redact) const HEALTH_FIELDS: &[&str] = &[
    "病历号",
    "住院号",
    "诊断",
    "病史",
    "medical_record_number",
    "diagnosis",
];

pub(super) const RULES: [LabeledRule; 5] = [
    LabeledRule {
        id: "labeled-name",
        names: NAME_FIELDS,
        field: Field::Name,
    },
    LabeledRule {
        id: "labeled-address",
        names: ADDRESS_FIELDS,
        field: Field::Address,
    },
    LabeledRule {
        id: "labeled-birth-date",
        names: BIRTH_FIELDS,
        field: Field::Birth,
    },
    LabeledRule {
        id: "labeled-account",
        names: ACCOUNT_FIELDS,
        field: Field::Account,
    },
    LabeledRule {
        id: "labeled-health",
        names: HEALTH_FIELDS,
        field: Field::Health,
    },
];

/// 只有整个字段名就是它时才算、还要同一对象里有别的个人信息字段的
const BARE: &[&str] = &["name", "address"];

/// 除了上面几张表，这些字段名也说明这个对象是一个人的资料（给裸的 `name`、`address` 作证）
const WITNESSES: &[&str] = &[
    "phone",
    "mobile",
    "telephone",
    "phone_number",
    "email",
    "id_card",
    "id_number",
    "gender",
    "手机",
    "手机号",
    "电话",
    "联系电话",
    "邮箱",
    "身份证",
    "身份证号",
    "性别",
];

/// 字段名规范化：ASCII 转小写、去掉分隔；汉字原样
fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

struct Table {
    /// 规范化的字段名 → 第几条规则、是不是裸的
    names: HashMap<String, (usize, bool)>,
    /// 表里英文字段名的最后一个词（`name`、`number`、`birth` …）、中文字段名的最后一个字。
    /// 字段名的结尾对不上这些的直接回去：正文里的冒号、等号绝大多数在这里就打发了
    tails: HashSet<String>,
    /// 给裸字段名作证的（表里不裸的、[`WITNESSES`]）
    witnesses: HashSet<String>,
}

static TABLE: LazyLock<Table> = LazyLock::new(|| {
    let mut names = HashMap::new();
    let mut tails = HashSet::new();
    let mut witnesses: HashSet<String> = WITNESSES.iter().map(|w| norm(w)).collect();
    for (i, r) in RULES.iter().enumerate() {
        for n in r.names {
            tails.insert(tail(n));
            let bare = BARE.contains(n);
            names.insert(norm(n), (i, bare));
            if !bare {
                witnesses.insert(norm(n));
            }
        }
    }
    Table {
        names,
        tails,
        witnesses,
    }
});

/// 字段名的最后一个词（英文，转小写）或者最后一个字（中文）
fn tail(key: &str) -> String {
    if key.is_ascii() {
        words(key).pop().unwrap_or_default()
    } else {
        key.chars()
            .next_back()
            .map(String::from)
            .unwrap_or_default()
    }
}

/// 字段名的结尾对得上表里某一项的结尾（[`Table::tails`]）。**不分配**：每个分隔符都要问一次
fn plausible_key(key: &str) -> bool {
    let t = &*TABLE;
    if !key.is_ascii() {
        let Some(c) = key.chars().next_back() else {
            return false;
        };
        let mut buf = [0u8; 4];
        return t.tails.contains(c.encode_utf8(&mut buf) as &str);
    }
    // 最后一个词：从末尾往回到分隔符或者小写接大写处
    let b = key.as_bytes();
    let mut s = b.len();
    while s > 0 && b[s - 1].is_ascii_alphanumeric() {
        s -= 1;
        if s > 0 && b[s].is_ascii_uppercase() && b[s - 1].is_ascii_lowercase() {
            break;
        }
    }
    let w = &key[s..];
    if w.is_empty() || w.len() > 16 {
        return false;
    }
    let mut buf = [0u8; 16];
    buf[..w.len()].copy_from_slice(w.as_bytes());
    buf[..w.len()].make_ascii_lowercase();
    std::str::from_utf8(&buf[..w.len()]).is_ok_and(|w| t.tails.contains(w))
}

/// 字段名切成词：非字母数字处、小写接大写处（驼峰）
fn words(key: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in key.chars() {
        if !c.is_ascii_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_ascii_uppercase() && prev_lower && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        cur.push(c.to_ascii_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// 字段名是哪一条规则的：整个对上，或者（不是裸的）以表里的一项结尾
fn lookup(key: &str) -> Option<(usize, bool)> {
    let t = &*TABLE;
    if key.is_empty() || key.len() > 64 {
        return None;
    }
    if key.is_ascii() {
        let w = words(key);
        if w.is_empty() || w.len() > 5 {
            return None;
        }
        for i in 0..w.len() {
            if let Some(&(r, bare)) = t.names.get(&w[i..].concat())
                && (!bare || i == 0)
            {
                return Some((r, bare));
            }
        }
        return None;
    }
    // 汉字字段名：以表里的一项结尾（`患者姓名`）
    let k = norm(key);
    let starts: Vec<usize> = k.char_indices().map(|(i, _)| i).collect();
    for &s in &starts {
        if let Some(&(r, bare)) = t.names.get(&k[s..])
            && !bare
        {
            return Some((r, bare));
        }
    }
    None
}

/// 是不是给裸字段名作证的字段名
fn witness(key: &str) -> bool {
    let t = &*TABLE;
    if key.is_ascii() {
        return t.witnesses.contains(&words(key).concat());
    }
    let k = norm(key);
    k.char_indices().any(|(i, _)| t.witnesses.contains(&k[i..]))
}

// ---------------------------------------------------------------- 找字段名

/// 字段名两边是哪种引号
#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    /// `"`
    Plain,
    /// JSON 字符串里转义了的 `\"`
    Escaped,
    /// `'`
    Single,
}

/// 字段名里能有的汉字（和日文假名）
fn cjk(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c) || ('\u{3040}'..='\u{30ff}').contains(&c)
}

/// `b[i]` 这个反斜杠前面连着偶数个反斜杠：它自己是转义的开头
fn lone_backslash(b: &[u8], i: usize) -> bool {
    let mut n = 0;
    while n < i && b[i - 1 - n] == b'\\' {
        n += 1;
    }
    n % 2 == 0
}

/// 分隔符（`:`、`=`、`：`）在 `p`，它前面的字段名：字节区间和引号
fn key_before(text: &str, p: usize) -> Option<(Range<usize>, Quote)> {
    let b = text.as_bytes();
    let mut k = p;
    while k > 0 && matches!(b[k - 1], b' ' | b'\t') && p - k < 4 {
        k -= 1;
    }
    if k == 0 {
        return None;
    }
    let (quote, kend) = match b[k - 1] {
        b'"' if k >= 2 && b[k - 2] == b'\\' && lone_backslash(b, k - 2) => (Quote::Escaped, k - 2),
        b'"' => (Quote::Plain, k - 1),
        b'\'' => (Quote::Single, k - 1),
        _ => (Quote::None, k),
    };
    if quote != Quote::None {
        // 往回找配对的引号，至多 80 个字节
        let lo = kend.saturating_sub(80);
        let mut i = kend;
        while i > lo {
            i -= 1;
            let c = b[i];
            if matches!(c, b'\n' | b'{' | b',' | b'}') {
                return None;
            }
            let hit = match quote {
                Quote::Escaped => {
                    c == b'"' && i >= 1 && b[i - 1] == b'\\' && lone_backslash(b, i - 1)
                }
                Quote::Plain => c == b'"' && !(i >= 1 && b[i - 1] == b'\\'),
                _ => c == b'\'',
            };
            if hit {
                let ks = i + 1;
                return (ks < kend && text.is_char_boundary(ks)).then_some((ks..kend, quote));
            }
        }
        return None;
    }
    // 没有引号：往回接字母数字（至多五个词，词间一个空格），或者汉字（至多八个）
    let head = &text[..kend];
    let last = head.chars().next_back()?;
    let mut ks = kend;
    if last.is_ascii_alphanumeric() || last == '_' || last == '-' {
        let mut words = 0;
        loop {
            let w0 = ks;
            while ks > 0 && (b[ks - 1].is_ascii_alphanumeric() || matches!(b[ks - 1], b'_' | b'-'))
            {
                ks -= 1;
            }
            if ks == w0 {
                break;
            }
            words += 1;
            if words == 5 || ks < 2 || b[ks - 1] != b' ' || !b[ks - 2].is_ascii_alphanumeric() {
                break;
            }
            ks -= 1;
        }
        // 行首的 `\nname` 里那个 `n` 是转义的一部分
        if ks > 0 && b[ks - 1] == b'\\' && lone_backslash(b, ks - 1) {
            let unicode = b[ks] == b'u'
                && b.get(ks + 1..ks + 5)
                    .is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit));
            ks += if unicode { 5 } else { 1 };
        }
    } else if cjk(last) {
        for (n, (i, c)) in head.char_indices().rev().enumerate() {
            if !cjk(c) || n == 8 {
                break;
            }
            ks = i;
        }
    }
    (ks < kend).then_some((ks..kend, quote))
}

/// 一段字段名（可能带 JSON 转义：`姓名` 就是「姓名」）的原样
fn key_text(raw: &str) -> std::borrow::Cow<'_, str> {
    if raw.contains('\\') {
        std::borrow::Cow::Owned(unescape(raw))
    } else {
        std::borrow::Cow::Borrowed(raw)
    }
}

/// 一段文本里所有的分隔符：`:`、`=`、全角冒号 `：`。返回它的位置和字节数
fn separators(b: &[u8], range: Range<usize>) -> impl Iterator<Item = (usize, usize)> + '_ {
    let base = range.start;
    let ascii = memchr::memchr2_iter(b':', b'=', &b[range.clone()]).filter_map(move |i| {
        let p = base + i;
        let next = b.get(p + 1).copied();
        if b[p] == b':' {
            // `std::fmt`、`https://`
            (next != Some(b':') && next != Some(b'/') && !(p > 0 && b[p - 1] == b':'))
                .then_some((p, 1))
        } else {
            // `==`、`!=`、`<=`、`>=`、`=>`
            (!matches!(next, Some(b'=' | b'>'))
                && !(p > 0 && matches!(b[p - 1], b'=' | b'!' | b'<' | b'>')))
            .then_some((p, 1))
        }
    });
    // 全角冒号另找：它的头一个字节（0xEF）是所有全角标点共用的，混进上面一起找的话，中文
    // 正文里每个逗号、括号都要停下来看一眼。起点在 `range` 里就算，和逐字节找的一样
    let wide_end = (range.end + 2).min(b.len());
    let wide = memchr::memmem::find_iter(&b[base..wide_end], "：".as_bytes())
        .map(move |i| (base + i, 3))
        .filter(move |&(p, _)| p < range.end);
    Merged {
        a: ascii.peekable(),
        b: wide.peekable(),
    }
}

/// 两串按位置排好的分隔符并成一串，仍按位置
struct Merged<A: Iterator<Item = (usize, usize)>, B: Iterator<Item = (usize, usize)>> {
    a: std::iter::Peekable<A>,
    b: std::iter::Peekable<B>,
}

impl<A, B> Iterator for Merged<A, B>
where
    A: Iterator<Item = (usize, usize)>,
    B: Iterator<Item = (usize, usize)>,
{
    type Item = (usize, usize);

    fn next(&mut self) -> Option<(usize, usize)> {
        match (self.a.peek(), self.b.peek()) {
            (Some(x), Some(y)) if y.0 < x.0 => self.b.next(),
            (Some(_), _) => self.a.next(),
            _ => self.b.next(),
        }
    }
}

// ---------------------------------------------------------------- 取值

/// 分隔符后面的值：字节区间（引号里面那一段，或者没有引号时到行尾为止）和它带没带引号
fn value_after(
    text: &str,
    from: usize,
    structural: bool,
    spans: &[Range<usize>],
    field: Field,
) -> Option<(Range<usize>, bool)> {
    let b = text.as_bytes();
    let mut v = from;
    while v < b.len() && matches!(b[v], b' ' | b'\t') {
        v += 1;
    }
    if structural {
        // 请求体本身的 `"key": value`：值得是字符串，数值、对象不是
        while v < b.len() && b[v].is_ascii_whitespace() {
            v += 1;
        }
        if b.get(v) != Some(&b'"') {
            return None;
        }
        let i = spans.binary_search_by_key(&(v + 1), |s| s.start).ok()?;
        return Some((spans[i].clone(), true));
    }
    let rest = &b[v..];
    let find = |pat: &[u8], from: usize| {
        memchr::memmem::find(&b[from..(from + 400).min(b.len())], pat).map(|i| from + i)
    };
    if rest.starts_with(b"\\\"") {
        let s = v + 2;
        let e = find(b"\\\"", s)?;
        return Some((s..e, true));
    }
    if rest.first() == Some(&b'"') || rest.first() == Some(&b'\'') {
        let q = rest[0];
        let s = v + 1;
        let e = s + memchr::memchr(q, &b[s..(s + 400).min(b.len())])?;
        return Some((s..e, true));
    }
    // 没有引号：到行尾、字符串结尾、`&`、中文的逗号分号句号为止
    let mut e = v;
    while e < b.len() && e - v < 400 {
        let c = b[e];
        if c == b'\\' {
            // `\uXXXX` 是正文里的一个字，别的转义（`\n`、`\"`、`\t`）是边界
            if b.get(e + 1) == Some(&b'u') {
                e += 6;
                continue;
            }
            break;
        }
        if matches!(c, b'\n' | b'\r' | b'\t' | b'"' | b'&' | b';' | b'|')
            || (c == b',' && field != Field::Address)
            || (c == b' ' && b.get(e + 1) == Some(&b' '))
        {
            break;
        }
        if c >= 0x80 {
            if !text.is_char_boundary(e) {
                e += 1;
                continue;
            }
            let tail = &text[e..];
            if tail.starts_with('，') || tail.starts_with('；') || tail.starts_with('。') {
                break;
            }
        }
        e += 1;
    }
    let e = e.min(b.len());
    if !text.is_char_boundary(e) {
        return None;
    }
    let mut val = &text[v..e];
    // 后面又跟着一个字段（`张三 性别：男`）：截在它前面那个空格
    // 冒号前面是数字的是时间（`08:00`），不是字段
    let label = val.char_indices().find(|&(i, c)| {
        matches!(c, '：' | ':' | '=') && !val[..i].ends_with(|p: char| p.is_ascii_digit())
    });
    if let Some((c, _)) = label {
        let cut = val[..c].rfind(' ')?;
        val = &val[..cut];
    }
    // YAML 的行尾注释
    if let Some(c) = val.find(" #") {
        val = &val[..c];
    }
    let val = val.trim_end_matches([' ', ',', '.', '。']);
    (!val.is_empty()).then_some((v..v + val.len(), false))
}

/// 值不是真值的几种：空、占位写法、类型名
const NOT_VALUES: &[&str] = &[
    "null",
    "none",
    "nil",
    "undefined",
    "true",
    "false",
    "n/a",
    "na",
    "-",
    "无",
    "未知",
    "不详",
    "string",
    "str",
    "text",
    "number",
    "integer",
    "int",
    "date",
    "datetime",
    "object",
    "array",
    "boolean",
    "bool",
    "optional",
];

/// 一个值像不像这一种个人信息。`quoted`：写在引号里（不带引号的更可能是代码）
fn plausible(field: Field, v: &str, quoted: bool) -> bool {
    let v = v.trim();
    if v.is_empty() || placeholder_value(v) || NOT_VALUES.contains(&v.to_lowercase().as_str()) {
        return false;
    }
    let chars = v.chars().count();
    let digits = v.chars().filter(char::is_ascii_digit).count();
    if !quoted {
        // 代码：表达式、调用、下标、模板
        if v.contains([
            '(', ')', '[', ']', '{', '}', '$', '`', '<', '>', '+', '*', '\\', '"', '\'',
        ]) {
            return false;
        }
        // 一个光秃秃的英文单词多半是类型名、变量名（`String`、`user`），不是值
        let ascii_word = v.is_ascii() && !v.contains(' ');
        if ascii_word && digits < 4 {
            return false;
        }
    }
    match field {
        Field::Name => {
            chars <= 40
                && digits == 0
                && !v.contains(['@', '/', ':', '\\', '=', '_'])
                && v.chars().any(char::is_alphabetic)
        }
        Field::Address => (4..=150).contains(&chars) && v.chars().any(char::is_alphabetic),
        Field::Birth => chars <= 30 && (digits >= 2 || v.contains('年')),
        Field::Account => {
            chars <= 40
                && digits >= 6
                && v.chars()
                    .all(|c| c.is_ascii_digit() || c == ' ' || c == '-')
        }
        Field::Health => (2..=300).contains(&chars),
    }
}

// ---------------------------------------------------------------- 裸字段名的旁证

/// `i` 在不在一个 JSON 字符串里面（`spans` 按起点排好、互不重叠）
fn inside(spans: &[Range<usize>], i: usize) -> bool {
    let k = spans.partition_point(|s| s.end < i);
    spans.get(k).is_some_and(|s| s.start <= i && i < s.end)
}

/// 请求体本身的一个对象（字段名的引号从 `key_open` 起）里，直接的字段名有没有一个作证的
fn object_has_witness(text: &str, spans: &[Range<usize>], key_open: usize) -> bool {
    const LIMIT: usize = 64 * 1024;
    let b = text.as_bytes();
    // 往回找包着它的 `{`
    let mut depth = 0usize;
    let mut i = key_open;
    let lo = key_open.saturating_sub(LIMIT);
    let open = loop {
        if i <= lo {
            return false;
        }
        i -= 1;
        match b[i] {
            b'"' => {
                // 一个字符串的右引号：跳到它的左引号
                let k = spans.partition_point(|s| s.end < i);
                if let Some(s) = spans.get(k).filter(|s| s.end == i) {
                    i = s.start - 1;
                }
            }
            b'}' | b']' => depth += 1,
            b'{' | b'[' if depth > 0 => depth -= 1,
            b'{' => break i,
            b'[' => return false,
            _ => {}
        }
    };
    // 往后过一遍这个对象：深度为 0 的字符串后面跟着冒号的是它的字段名
    let mut depth = 0usize;
    let mut i = open + 1;
    let hi = (open + LIMIT).min(b.len());
    while i < hi {
        match b[i] {
            b'"' => {
                let Ok(k) = spans.binary_search_by_key(&(i + 1), |s| s.start) else {
                    return false;
                };
                let s = spans[k].clone();
                let mut j = s.end + 1;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if depth == 0 && b.get(j) == Some(&b':') && witness(&key_text(&text[s.clone()])) {
                    return true;
                }
                i = s.end + 1;
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' if depth == 0 => return false,
            b'}' | b']' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    false
}

/// 一个没有引号的字段名（YAML、表单）从行首缩进多少：行首到它之间只有空白和列表的 `-`。
/// 不在行首的是 `None`（`actions: { email: record }` 里的 `email`）
fn line_indent(b: &[u8], key: usize) -> Option<usize> {
    let mut i = key;
    while i > 0 && matches!(b[i - 1], b' ' | b'\t' | b'-') {
        i -= 1;
    }
    let at_start = i == 0
        || matches!(b[i - 1], b'\n' | b'\r' | b'"')
        || (b[i - 1] == b'n' && i >= 2 && b[i - 2] == b'\\');
    at_start.then_some(key - i)
}

/// 前后两行以内（[`five_lines`]）有没有一个作证的字段名：和裸字段名一样在行首、缩进相同 ——
/// 同一张 YAML 映射、同一张表单里的另一项
fn lines_have_witness(text: &str, range: Range<usize>, skip: usize, key: usize) -> bool {
    let b = text.as_bytes();
    let Some(indent) = line_indent(b, key) else {
        return false;
    };
    separators(b, range).any(|(p, _)| {
        p != skip
            && key_before(text, p).is_some_and(|(k, q)| {
                q == Quote::None
                    && line_indent(b, k.start) == Some(indent)
                    && witness(&key_text(&text[k]))
            })
    })
}

/// 正文里一段 JSON 的一个对象（`range` 从它的 `{` 起）里，直接的字段名有没有一个作证的。
/// 嵌在里面的对象不算：`{"name": "chatgpt", "oauth": {"email": …}}` 里的 `email` 说的是
/// 账号，不是这个 `name`
fn object_text_has_witness(text: &str, range: Range<usize>, skip: usize) -> bool {
    let b = text.as_bytes();
    let mut depth = 0usize;
    let mut seps = separators(b, range.clone()).peekable();
    for i in range {
        while seps.peek().is_some_and(|&(p, _)| p < i) {
            seps.next();
        }
        match b[i] {
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if let Some(&(p, _)) = seps.peek()
            && p == i
            && p != skip
            && depth == 1
            && key_before(text, p).is_some_and(|(k, _)| witness(&key_text(&text[k])))
        {
            return true;
        }
    }
    false
}

/// 正文里一段 JSON（`\"name\": \"…\"`）中包着 `at` 的那个对象的大致范围：往两边数大括号
fn escaped_object(b: &[u8], at: usize, within: Range<usize>) -> Range<usize> {
    const LIMIT: usize = 16 * 1024;
    let lo = within.start.max(at.saturating_sub(LIMIT));
    let hi = within.end.min(at + LIMIT);
    let mut depth = 0usize;
    let mut s = at;
    while s > lo {
        s -= 1;
        match b[s] {
            b'}' => depth += 1,
            b'{' if depth == 0 => break,
            b'{' => depth -= 1,
            _ => {}
        }
    }
    let mut depth = 0usize;
    let mut e = at;
    while e < hi {
        match b[e] {
            b'{' => depth += 1,
            b'}' if depth == 0 => break,
            b'}' => depth -= 1,
            _ => {}
        }
        e += 1;
    }
    s..e
}

/// `at` 所在那一行的前后两行（共五行）。行以换行符、JSON 的 `\n`、字符串的引号为界
fn five_lines(b: &[u8], at: usize, within: Range<usize>) -> Range<usize> {
    const LIMIT: usize = 4 * 1024;
    let brk = |i: usize| b[i] == b'\n' || (b[i] == b'n' && i > 0 && b[i - 1] == b'\\');
    let lo = within.start.max(at.saturating_sub(LIMIT));
    let hi = within.end.min(at + LIMIT);
    let mut s = at;
    let mut n = 0;
    while s > lo {
        s -= 1;
        if brk(s) {
            n += 1;
            if n == 3 {
                s += 1;
                break;
            }
        }
    }
    let mut e = at;
    let mut n = 0;
    while e < hi {
        if brk(e) {
            n += 1;
            if n == 3 {
                break;
            }
        }
        e += 1;
    }
    s..e
}

// ---------------------------------------------------------------- 入口

/// 按字段名找。`on`：五条规则各开没开（[`RULES`] 的顺序）
pub(in crate::redact) fn scan(
    text: &str,
    json: bool,
    on: &[bool; RULES.len()],
    given: Option<&[Range<usize>]>,
    out: &mut Vec<Hit>,
) {
    let b = text.as_bytes();
    // 字符串的区间只有字段名查得到的时候才要（一个请求体里没几处）：切 token 时记下了的直接用，
    // 没有就用到了再算
    let spans_cell = std::cell::OnceCell::new();
    let spans = || -> &[Range<usize>] {
        if let Some(g) = given {
            return g;
        }
        spans_cell.get_or_init(|| {
            if json {
                json_string_spans(text)
            } else {
                Vec::new()
            }
        })
    };
    for (p, len) in separators(b, 0..b.len()) {
        let Some((key, quote)) = key_before(text, p) else {
            continue;
        };
        let raw = &text[key.clone()];
        if !raw.contains('\\') && !plausible_key(raw) {
            continue;
        }
        let Some((rule, bare)) = lookup(&key_text(raw)) else {
            continue;
        };
        if !on[rule] {
            continue;
        }
        let field = RULES[rule].field;
        let in_string = json && inside(spans(), p);
        let structural = json && !in_string;
        // 请求体本身的字段名得是一个字符串
        if structural && quote != Quote::Plain {
            continue;
        }
        let Some((val, quoted)) = value_after(text, p + len, structural, spans(), field) else {
            continue;
        };
        let shown = key_text(&text[val.clone()]);
        if !plausible(field, &shown, quoted) {
            continue;
        }
        if bare {
            let open = key
                .start
                .saturating_sub(if quote == Quote::Escaped { 2 } else { 1 });
            let within = if in_string {
                let k = spans().partition_point(|s| s.end < p);
                spans()[k].clone()
            } else {
                0..b.len()
            };
            let ok = if structural {
                object_has_witness(text, spans(), open)
            } else if quote == Quote::None {
                lines_have_witness(text, five_lines(b, p, within), p, key.start)
            } else {
                object_text_has_witness(text, escaped_object(b, p, within), p)
            };
            if !ok {
                continue;
            }
        }
        // 值的头上若是转义的尾巴（不该有），不要
        if escape_before(b, val.start) > 0 && !quoted {
            continue;
        }
        out.push(builtin_hit(RULES[rule].id, val));
    }
}
