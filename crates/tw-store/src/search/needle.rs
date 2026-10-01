//! 搜索词怎么比：和界面同一种不分大小写、原始字节上的粗筛、命中前后的那一小段。
//!
//! **大小写按界面的规则来。**流量页的筛选框是 `x.toLowerCase().includes(q)`，而
//! JavaScript 的 `toLowerCase` 按 Unicode 的表转小写 —— 「Über」和「über」是对得上的。
//! SQLite 的 `LIKE` 只认 ASCII 的大小写，搬到库里照样用它的话，同一个词在界面上筛得到、
//! 在整份记录里却找不到。这里用 Rust 的 `str::to_lowercase`，和 JavaScript 是同一张表
//! （连词尾的 Σ → ς 都一样）。

use std::ops::Range;

/// 命中前后各留多少个字。
pub const EXCERPT_CHARS: usize = 40;

/// 脱敏时命中两边各多带多少个字。比摘录宽得多：密钥被打成 `sk-an…7f9c` 之后会变短，
/// 留得太窄的话，打完码摘录就只剩一两个字
const MASK_MARGIN: usize = 160;

/// 一段正文里最多试几处命中。前几处都在密钥里面（打码之后对不上）的，再往后找也多半
/// 一样，而每试一处都要打一次码
const MAX_TRIES: usize = 16;

/// 去掉首尾的空白，**和 JavaScript 的 `String.prototype.trim` 去掉的是同一批字符**。
///
/// 界面先 trim 再比（`filterRows` 里的 `f.q.trim()`）。Rust 的 `str::trim` 按 Unicode 的
/// White_Space 认，比 JavaScript 多去一个 U+0085、少去一个 U+FEFF。
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
}

/// 搜索词的样子：去掉首尾空白、转小写。只剩空白的是 `None` —— 和界面一样，那等于
/// 没有按文本筛。
pub fn fold(q: &str) -> Option<String> {
    let t = js_trim(q);
    (!t.is_empty()).then(|| t.to_lowercase())
}

/// `hay` 转小写之后含不含 `needle`（已经转过小写）。界面上 `hay.toLowerCase().includes(q)`
/// 那一句。
pub fn contains_folded(hay: &str, needle: &str) -> bool {
    if hay.is_ascii() {
        // ASCII 的小写还是 ASCII，装不下 ASCII 以外的字；只有 A–Z 会变，不必整串转一遍
        return needle.is_ascii() && find_ascii_ci(hay.as_bytes(), needle.as_bytes(), 0).is_some();
    }
    hay.to_lowercase().contains(needle)
}

/// 从 `from` 起找 `needle`（小写的 ASCII），ASCII 字母不分大小写。
fn find_ascii_ci(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let Some((&first, rest)) = needle.split_first() else {
        return Some(from.min(hay.len()));
    };
    let last = hay.len().checked_sub(needle.len())?;
    let mut i = from;
    while i <= last {
        // 先找首字节，绝大多数位置在这一步就过去了
        let skip = hay[i..=last]
            .iter()
            .position(|b| b.to_ascii_lowercase() == first)?;
        i += skip;
        if hay[i + 1..i + needle.len()]
            .iter()
            .zip(rest)
            .all(|(h, n)| h.to_ascii_lowercase() == *n)
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// 在 `text` 里从 `from`（字节，落在字符边界上）起找 `needle`，给出原文里的字节范围。
fn find_folded(text: &str, needle: &str, from: usize) -> Option<Range<usize>> {
    if text.is_ascii() {
        let i = needle
            .is_ascii()
            .then(|| find_ascii_ci(text.as_bytes(), needle.as_bytes(), from))??;
        return Some(i..i + needle.len());
    }
    // 小写会改字节数（İ → i̇、K → k），所以在小写之后的文本里找，再一个字一个字地数回
    // 原文。逐字的小写和整串的小写只在词尾的 Σ 上不同（σ/ς），两者字节数一样，数得准。
    // 整串转小写而不是只转 `from` 之后那段：Σ 是不是词尾要看前面的字
    let lower = text.to_lowercase();
    let lower_from: usize = text[..from]
        .chars()
        .map(|c| c.to_lowercase().map(char::len_utf8).sum::<usize>())
        .sum();
    let p = lower_from + lower[lower_from..].find(needle)?;
    let end = p + needle.len();
    // 从头再数一遍，找出盖住 [p, end) 的那几个原文的字
    let mut acc = 0;
    let mut start = None;
    for (i, c) in text.char_indices() {
        let w: usize = c.to_lowercase().map(char::len_utf8).sum();
        if start.is_none() && acc + w > p {
            start = Some(i);
        }
        if acc + w >= end {
            return Some(start.unwrap_or(i)..i + c.len_utf8());
        }
        acc += w;
    }
    None
}

/// 一个要在正文里找的词。
#[derive(Debug, Clone)]
pub struct Needle {
    /// 转过小写的搜索词
    folded: String,
    /// 在原始字节上粗筛用的那一小段
    probe: Option<Probe>,
}

/// 搜索词里挑出来的一段，**它在 JSON 原文里一定照样写着**（只要正文里有这个词）。
#[derive(Debug, Clone)]
enum Probe {
    /// 一段 ASCII 字母和数字，小写。没有哪个 JSON 写法会转义它们；大写的照样认
    Ascii {
        bytes: Vec<u8>,
        /// 含 `i` 或 `k`：U+0130（İ）和 U+212A（开尔文符号 K）小写之后是 ASCII 的
        /// i 和 k，原文里写的却不是。只有这两个字会这样
        exotic: bool,
    },
    /// 一段没有大小写之分的非 ASCII 字（汉字之类）。原文里要么就是这几个字的 UTF-8，
    /// 要么全写成 `\uXXXX`：Python 默认把 ASCII 以外的字全部转义
    Wide { utf8: Vec<u8>, escaped: Vec<u8> },
}

impl Needle {
    /// `folded` 是 [`fold`] 出来的样子。
    pub fn new(folded: String) -> Needle {
        let probe = probe_of(&folded);
        Needle { folded, probe }
    }

    /// 这份 JSON 原文（请求体，或者整包的响应）里可能有这个词吗。
    ///
    /// **只会多说、不会少说。**说「没有」的那些就不用解析了 —— 一个请求体动辄几百 KB，
    /// 而绝大多数请求里根本没有这个词。只对整包的 JSON 成立：流式的回答一个词可能被切在
    /// 两帧里，原文里找不到整个词。
    pub fn might_be_in(&self, raw: &[u8]) -> bool {
        match &self.probe {
            None => true,
            Some(Probe::Ascii { bytes, exotic }) => {
                find_ascii_ci(raw, bytes, 0).is_some() || (*exotic && has_exotic(raw))
            }
            Some(Probe::Wide { utf8, escaped }) => {
                memfind(raw, utf8) || find_ascii_ci(raw, escaped, 0).is_some()
            }
        }
    }

    /// 第一处对上、**打完码之后还对得上**的地方，连同前后那一小段。
    ///
    /// 先在原文里找，找到了只给它两边的一段打码再核一遍：整段正文打码太贵（一个工具结果
    /// 就能有几十 KB），而对得上的只是少数。落在密钥里的命中打完码就对不上了 —— 那不能
    /// 交出去，接着找下一处。
    pub fn excerpt(&self, text: &str) -> Option<Excerpt> {
        let mut from = 0;
        for _ in 0..MAX_TRIES {
            let hit = find_folded(text, &self.folded, from)?;
            let (ws, we) = mask_window(text, &hit);
            let masked = tw_secret::mask_body(&text[ws..we]);
            if let Some(m) = find_folded(&masked, &self.folded, 0) {
                return Some(cut(&masked, m, ws > 0, we < text.len()));
            }
            // 下一处从这一处的第二个字起找：命中可以互相重叠
            from = hit.start + text[hit.start..].chars().next().map_or(1, char::len_utf8);
        }
        None
    }
}

/// 命中和前后的字：空白（含换行）并成一个空格。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excerpt {
    pub before: String,
    pub matched: String,
    pub after: String,
}

/// 从搜索词里挑一段粗筛用的。
///
/// 优先长一点的 ASCII 字母数字（三个以上），否则一段汉字这类没有大小写的字，再否则短的
/// ASCII 也行。一样都挑不出（全是标点、或者全是有大小写的外文字母）就不粗筛。
fn probe_of(folded: &str) -> Option<Probe> {
    let ascii = longest_run(folded, |c| c.is_ascii_alphanumeric());
    let wide = longest_run(folded, literal_wide);
    let ascii_probe = || {
        let bytes = ascii.as_bytes().to_vec();
        let exotic = bytes.iter().any(|b| matches!(b, b'i' | b'k'));
        Probe::Ascii { bytes, exotic }
    };
    if ascii.len() >= 3 {
        Some(ascii_probe())
    } else if !wide.is_empty() {
        Some(Probe::Wide {
            utf8: wide.as_bytes().to_vec(),
            escaped: escape(wide).into_bytes(),
        })
    } else if !ascii.is_empty() {
        Some(ascii_probe())
    } else {
        None
    }
}

/// 满足 `keep` 的最长一段（按字数）。
fn longest_run(s: &str, keep: impl Fn(char) -> bool) -> &str {
    let mut best = 0..0;
    let mut start = None;
    let mut count = 0;
    let mut best_count = 0;
    for (i, c) in s.char_indices().chain(std::iter::once((s.len(), '\0'))) {
        if i < s.len() && keep(c) {
            if start.is_none() {
                start = Some(i);
                count = 0;
            }
            count += 1;
        } else if let Some(st) = start.take()
            && count > best_count
        {
            best = st..i;
            best_count = count;
        }
    }
    &s[best]
}

/// 这个字在 JSON 原文里只会写成它自己的 UTF-8 或者 `\uXXXX`，而且正文里只有它自己
/// 小写之后是它。
///
/// 有大小写之分的不行（原文里可能是大写）；U+0307 是 İ 小写之后的后半个字；U+2028、
/// U+2029 Go 的 JSON 会单独转义（别的字照写），两种写法掺在一起时找不到。
fn literal_wide(c: char) -> bool {
    !c.is_ascii()
        && !matches!(c, '\u{307}' | '\u{2028}' | '\u{2029}')
        && c.to_lowercase().eq(std::iter::once(c))
        && c.to_uppercase().eq(std::iter::once(c))
}

/// 全写成 `\uXXXX` 的样子，十六进制小写。BMP 以外的字是一对代理项
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 4);
    let mut units = [0u16; 2];
    for c in s.chars() {
        for u in c.encode_utf16(&mut units) {
            out.push_str(&format!("\\u{u:04x}"));
        }
    }
    out
}

/// 原文里有没有那两个小写之后变成 ASCII 字母的字（见 `Probe::Ascii::exotic`），
/// UTF-8 或者转义的写法都算。
fn has_exotic(raw: &[u8]) -> bool {
    memfind(raw, "\u{130}".as_bytes())
        || memfind(raw, "\u{212a}".as_bytes())
        || find_ascii_ci(raw, b"\\u0130", 0).is_some()
        || find_ascii_ci(raw, b"\\u212a", 0).is_some()
}

fn memfind(hay: &[u8], needle: &[u8]) -> bool {
    let Some((&first, rest)) = needle.split_first() else {
        return true;
    };
    let Some(last) = hay.len().checked_sub(needle.len()) else {
        return false;
    };
    let mut i = 0;
    while i <= last {
        let Some(skip) = hay[i..=last].iter().position(|b| *b == first) else {
            return false;
        };
        i += skip;
        if &hay[i + 1..i + needle.len()] == rest {
            return true;
        }
        i += 1;
    }
    false
}

/// 和 `tw_secret::mask_body` 认同一种 token：一个密钥一定是一整个 token，窗口的两头
/// 不能把它切开
fn is_tok(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
}

/// 给命中打码时取的那一段：两边各 [`MASK_MARGIN`] 个字，再往外扩到 token 的边界。
///
/// **两头都落在 token 边界上，这一段打出来的码就和整段正文打码时这一段的样子一样**：
/// `mask_body` 一个 token 一个 token 地判断，只看 token 自己和它前面那个字是不是 token 的
/// 一部分。
fn mask_window(text: &str, hit: &Range<usize>) -> (usize, usize) {
    let mut start = hit.start;
    let mut back = text[..hit.start].char_indices().rev();
    for _ in 0..MASK_MARGIN {
        match back.next() {
            Some((i, _)) => start = i,
            None => break,
        }
    }
    while let Some(c) = text[..start].chars().next_back()
        && is_tok(c)
    {
        start -= c.len_utf8();
    }
    let mut end = hit.end;
    let mut fwd = text[hit.end..].chars();
    for _ in 0..MASK_MARGIN {
        match fwd.next() {
            Some(c) => end += c.len_utf8(),
            None => break,
        }
    }
    while let Some(c) = text[end..].chars().next()
        && is_tok(c)
    {
        end += c.len_utf8();
    }
    (start, end)
}

/// 空白和控制字符都算空白：换行、制表符、NUL 在一行摘录里都只该是一个空格
fn blank(c: char) -> bool {
    c.is_whitespace() || c.is_control()
}

/// 把空白并成一个空格。
fn squeeze(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut gap = false;
    for c in s.chars() {
        if blank(c) {
            gap = true;
            continue;
        }
        if gap && !out.is_empty() {
            out.push(' ');
        }
        gap = false;
        out.push(c);
    }
    if gap {
        out.push(' ');
    }
    out
}

/// 切出命中和前后各 [`EXCERPT_CHARS`] 个字。`more_before` / `more_after`：这一段之外
/// 还有正文。
fn cut(text: &str, hit: Range<usize>, more_before: bool, more_after: bool) -> Excerpt {
    // 往前数：空白并成一个空格，并完之后的字才算数
    let mut before: Vec<char> = Vec::new();
    let mut seen = 0;
    let mut gap = false;
    let mut rest_before = false;
    for c in text[..hit.start].chars().rev() {
        if blank(c) {
            gap = true;
            continue;
        }
        if seen == EXCERPT_CHARS {
            rest_before = true;
            break;
        }
        if gap && !before.is_empty() {
            before.push(' ');
        }
        gap = false;
        before.push(c);
        seen += 1;
    }
    // 紧挨着命中的空白要留一个：「我的 key」里命中 key 时，前面是「我的 」
    let touching = text[..hit.start].chars().next_back().is_some_and(blank);
    before.reverse();
    let mut b: String = before.into_iter().collect();
    if touching && !b.is_empty() && !b.ends_with(' ') {
        b.push(' ');
    }
    if rest_before || more_before {
        b.insert(0, '…');
    }

    let mut after = String::new();
    let mut seen = 0;
    let mut gap = false;
    let mut rest_after = false;
    for c in text[hit.end..].chars() {
        if blank(c) {
            gap = true;
            continue;
        }
        if seen == EXCERPT_CHARS {
            rest_after = true;
            break;
        }
        // 命中后面紧跟着的空白也留一个
        if gap {
            after.push(' ');
        }
        gap = false;
        after.push(c);
        seen += 1;
    }
    if rest_after || more_after {
        after.push('…');
    }
    Excerpt {
        before: b,
        matched: squeeze(&text[hit]),
        after,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn needle(q: &str) -> Needle {
        Needle::new(fold(q).unwrap())
    }

    /// 界面先 `trim()` 再比。JavaScript 去掉的那一批：U+FEFF 去、U+0085 不去
    #[test]
    fn trimming_matches_javascript() {
        assert_eq!(js_trim("\u{feff}\u{3000} hi\t\n"), "hi");
        assert_eq!(js_trim("\u{85}hi\u{85}"), "\u{85}hi\u{85}");
        assert_eq!(fold("   "), None);
        assert_eq!(fold(" GPT-5.5 "), Some("gpt-5.5".into()));
    }

    /// 和 `toLowerCase().includes()` 一样：ASCII 以外的大小写也认，词尾的 Σ 是 ς
    #[test]
    fn case_folding_is_the_unicode_one_the_ui_uses() {
        assert!(contains_folded("Übersetzer-Relay", "über"));
        assert!(contains_folded("ЯНДЕКС", "яндекс"));
        assert!(contains_folded("ΟΔΟΣ", "οδος"), "词尾的 Σ 转成 ς");
        assert!(!contains_folded("ΟΔΟΣ", "οδοσ"));
        assert!(contains_folded("Claude-Sonnet", "sonnet"));
        assert!(!contains_folded("claude", "中"));
        // 开尔文符号小写之后就是 k
        assert!(contains_folded("2\u{212a}", "2k"));
        // 不是通配符
        assert!(!contains_folded("500", "50%"));
        assert!(!contains_folded("axb", "a_b"));
        assert!(contains_folded("a_b 50%", "a_b"));
    }

    #[test]
    fn the_hit_is_reported_where_it_is_in_the_original_text() {
        let t = "前缀 İstanbul ÜBER alles";
        let r = find_folded(t, "über", 0).unwrap();
        assert_eq!(&t[r], "ÜBER");
        // 小写之后变长的字（İ → i̇）在命中前面，位置照样对得上
        let r = find_folded(t, "alles", 0).unwrap();
        assert_eq!(&t[r], "alles");
        // 命中落在一个字小写之后的前半截上：报整个字
        let r = find_folded(t, "i", 0).unwrap();
        assert_eq!(&t[r], "İ");
        // 从后面接着找
        let r = find_folded("ab AB ab", "ab", 1).unwrap();
        assert_eq!(r, 3..5);
    }

    /// 粗筛只会多说：正文里有这个词，原文里就一定找得到那一段
    #[test]
    fn the_prefilter_never_hides_a_word_that_is_there() {
        // 大写写着的
        assert!(needle("Read_File").might_be_in(br#"{"t":"READ_FILE x"}"#));
        // Python 把汉字全写成 \uXXXX，大写十六进制也有人写（`\x5c` 是反斜杠）
        let n = needle("中文搜索");
        assert!(n.might_be_in("{\"t\":\"中文搜索\"}".as_bytes()));
        assert!(n.might_be_in(b"{\"t\":\"\x5cu4e2d\x5cu6587\x5cu641c\x5cu7d22\"}"));
        assert!(n.might_be_in(b"{\"t\":\"\x5cu4E2D\x5cu6587\x5cu641C\x5cu7D22\"}"));
        // BMP 以外的字是一对代理项
        let n = needle("\u{1f980}");
        assert!(n.might_be_in(b"{\"t\":\"\x5cud83e\x5cudd80\"}"));
        // 开尔文符号和 İ 小写之后是 ASCII 的 k、i
        assert!(needle("2k").might_be_in("{\"t\":\"2\u{212a}\"}".as_bytes()));
        assert!(needle("2k").might_be_in(b"{\"t\":\"2\x5cu212A\"}"));
        assert!(needle("ist").might_be_in("{\"t\":\"\u{130}st\"}".as_bytes()));
        // 有大小写的外文字母挑不出粗筛的那一段，一律放过
        assert!(needle("über").might_be_in(b"{\"t\":\"\x5cu00dcBER\"}"));
        assert!(needle("\u{ff}").might_be_in(b"{}"));
    }

    #[test]
    fn the_prefilter_turns_away_what_cannot_contain_the_word() {
        assert!(!needle("timeout").might_be_in(br#"{"t":"time out"}"#));
        // 转义写到一半就断了（截断的请求体）
        assert!(!needle("中文").might_be_in(b"{\"t\":\"\x5cu4e2d\x5cu65"));
        assert!(!needle("搜索").might_be_in("{\"t\":\"中文\"}".as_bytes()));
    }

    #[test]
    fn the_excerpt_is_forty_characters_each_side_with_blanks_squeezed() {
        let text = format!("{}\n\n  needle here\t{}", "前".repeat(60), "后".repeat(60));
        let e = needle("NEEDLE").excerpt(&text).unwrap();
        assert_eq!(e.matched, "needle");
        assert_eq!(e.before, format!("…{} ", "前".repeat(40)));
        assert_eq!(e.after, format!(" here {}…", "后".repeat(36)));
        assert_eq!(
            e.after.chars().filter(|c| *c != ' ' && *c != '…').count(),
            40
        );
    }

    #[test]
    fn a_short_text_is_not_marked_as_cut() {
        let e = needle("key").excerpt("我的 key 是这个").unwrap();
        assert_eq!(
            (e.before.as_str(), e.matched.as_str(), e.after.as_str()),
            ("我的 ", "key", " 是这个")
        );
        let e = needle("abc").excerpt("abc").unwrap();
        assert_eq!((e.before.as_str(), e.after.as_str()), ("", ""));
    }

    /// 摘录要交给界面，**和详情抽屉同一套打码**：命中旁边的密钥打掉，落在密钥里面的
    /// 命中不算
    #[test]
    fn the_excerpt_is_masked_and_a_hit_inside_a_secret_does_not_count() {
        let key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123";
        let text = format!("我的 key 是 {key}，请保管好");
        let e = needle("key 是").excerpt(&text).unwrap();
        assert!(!e.after.contains("abcdefghijklmnop"), "{e:?}");
        assert!(e.after.contains("sk-an…"), "{e:?}");
        // 只出现在密钥里的词：打完码就没有了
        assert_eq!(needle("klmnop").excerpt(&text), None);
        // 密钥里有、正文里也有：交出正文里那一处
        let text = format!("{key} 之后又说了一遍 klmnop");
        let e = needle("klmnop").excerpt(&text).unwrap();
        assert!(e.before.ends_with("说了一遍 "), "{e:?}");
        assert!(!e.before.contains("abcdefghij"), "{e:?}");
    }

    /// 打码的那一段两头不切开 token：一把很长的密钥正好跨在边界上也认得出来。
    ///
    /// 这把密钥只靠 `sk-` 开头认得出来（后面没有数字）。从中间切开的话，后半截看着就是
    /// 一串普通字母，会原样漏进摘录
    #[test]
    fn a_secret_straddling_the_mask_window_is_still_masked() {
        let key = format!("sk-{}", "abcdefgh".repeat(30));
        assert!(key.len() > MASK_MARGIN);
        let text = format!("{key} needle");
        let e = needle("needle").excerpt(&text).unwrap();
        assert!(!e.before.contains("abcdefghabcdefgh"), "{e:?}");
        assert!(e.before.starts_with("sk-ab…"), "{e:?}");
    }

    #[test]
    fn longest_runs_pick_the_probe() {
        assert_eq!(
            longest_run("a.bcd-ef", |c| c.is_ascii_alphanumeric()),
            "bcd"
        );
        assert_eq!(longest_run("中文 x 搜索词", literal_wide), "搜索词");
        assert_eq!(longest_run("", literal_wide), "");
        assert!(matches!(probe_of("gpt"), Some(Probe::Ascii { .. })));
        assert!(matches!(probe_of("go 中文"), Some(Probe::Wide { .. })));
        assert!(matches!(probe_of("go"), Some(Probe::Ascii { .. })));
        assert!(probe_of("!?-").is_none());
        // 有大小写的外文字母不行（原文里可能是大写），省略号这种没有大小写的可以
        assert!(probe_of("ü").is_none());
        assert!(matches!(probe_of("…"), Some(Probe::Wide { .. })));
        assert_eq!(escape("中🦀"), "\\u4e2d\\ud83e\\udd80");
    }
}
