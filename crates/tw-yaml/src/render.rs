//! 把一个值渲染成 YAML 标量，**尽量沿用原来的风格**。
//!
//! 「尽量」有边界：原文写的是纯量而新值里有个冒号，就必须加引号，否则
//! 写出去的是一份语义变了的文件。风格保留输给正确性，但只在必须的时候。

use saphyr_parser::ScalarStyle;

#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Str(String),
    Int(i64),
    Bool(bool),
    /// 显式的空值。`null` 和「空字符串」是两回事
    Null,
}

impl Scalar {
    pub fn s(v: impl Into<String>) -> Self {
        Scalar::Str(v.into())
    }
    /// 解析器读回来会是什么。自检拿它做对照。
    pub fn as_yaml_text(&self) -> String {
        match self {
            Scalar::Str(s) => s.clone(),
            Scalar::Int(i) => i.to_string(),
            Scalar::Bool(b) => b.to_string(),
            Scalar::Null => "~".to_string(),
        }
    }
}

/// 一个纯量能不能不加引号地写出去。
///
/// **宁可多加引号。**漏加的代价是一份语义悄悄变了的配置（`key: yes` 会
/// 被读成布尔、`key: 1:30` 会被读成 sexagesimal），而多加的代价只是一对
/// 用户没写的引号。
fn needs_quotes(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    // 首尾空白会被纯量吃掉
    if s.trim() != s {
        return true;
    }
    // 控制字符、换行和 YAML 1.1 当换行的那几个，只有双引号里的转义能原样表示
    if s.chars().any(must_escape) {
        return true;
    }
    if s.starts_with([
        '-', '?', ',', '[', ']', '{', '}', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`',
    ]) {
        return true;
    }
    // `...` 开头的值单独拿出来读（`put` 写之前那道检查就是这么读的）是文档结束标记。
    // `---` 已经被上面的 `-` 拦下了
    if s.starts_with("...") {
        return true;
    }
    // 流式上下文（`[a, b]` / `{k: v}`）里这几个字符会切断纯量。这一层
    // 看不出自己在不在流式里，所以一律加引号 —— 它们在真实的值里本来
    // 就罕见。
    if s.contains([',', '[', ']', '{', '}']) {
        return true;
    }
    // **`:` 和 `#` 要按 YAML 的真实规则判，不能一见就加引号。**
    // `base_url` 是被改得最多的字段，而每个 URL 都套上一对用户没写的
    // 引号，正是想避免的那种 diff 噪音。
    //
    // 规则：`#` 只有在开头或前面是空白时才开启注释；`:` 只有在后面是
    // 空白或行尾时才是键值分隔符。所以 `http://a:8788` 和 `sk-a#b` 都
    // 是完整的纯量。判错了也还有 `set` 里那道重新解析的自检兜着。
    let b = s.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        if c == b'#' && (i == 0 || b[i - 1] == b' ' || b[i - 1] == b'\t') {
            return true;
        }
        if c == b':' && (i + 1 == b.len() || b[i + 1] == b' ' || b[i + 1] == b'\t') {
            return true;
        }
    }
    // 看起来像别的类型的字符串必须加引号，否则读回来就不是字符串了。
    // YAML 1.1 的布尔词表比 1.2 长得多，而各家实现并不统一 —— 全都躲开。
    const LOOKALIKE: &[&str] = &[
        "true", "false", "yes", "no", "on", "off", "y", "n", "null", "nil", "~",
    ];
    let lower = s.to_ascii_lowercase();
    if LOOKALIKE.contains(&lower.as_str()) {
        return true;
    }
    if s.parse::<f64>().is_ok() || s.parse::<i64>().is_ok() {
        return true;
    }
    false
}

/// 这个字符写进 YAML 要不要转义：控制字符（C0、DEL、C1，含制表符和换行）、YAML 1.1
/// 当作换行的那几个（NEL、LS、PS），以及 BOM 和两个非字符。
///
/// **这些字符原样写进文件，要么读不回来，要么读回来变了样。**配置的加载器（serde 那条路，
/// libyaml）按 YAML 1.1 读：纯量里的 LS、PS、NEL 是换行，整份文件就解析不了；双引号里的
/// NEL 折成一个空格，值悄悄变了；C1 控制字符和 U+FFFE 让整份文件被拒收。补丁层的自检用
/// 的是 saphyr（YAML 1.2），它把 LS、PS 当普通字符，**自检拦不住** —— 所以写的时候就转义。
/// BOM 和两个非字符各家解析器读法不一，一样转义。
///
/// 按路径改一个值（这里）和按名字改一项（`tw_config::edit`）用的是同一份判断。
pub fn must_escape(c: char) -> bool {
    let n = c as u32;
    n < 0x20
        || (0x7f..=0x9f).contains(&n)
        || matches!(n, 0x2028 | 0x2029 | 0xfeff | 0xfffe | 0xffff)
}

/// 一个字符串写成单行的 YAML 双引号标量。`"` 和 `\` 加反斜杠，换行、回车、制表符用
/// 各自的转义，其余要转义的（[`must_escape`]）写成 `\xNN` / `\uNNNN`，别的字符原样。
/// 值里写什么都动不了文件的结构。
pub fn double_quoted(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if must_escape(c) => {
                let n = c as u32;
                if n <= 0xff {
                    let _ = write!(out, "\\x{n:02x}");
                } else {
                    let _ = write!(out, "\\u{n:04x}");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// 渲染。`was` 是原来那个值的风格 —— 能沿用就沿用。
pub fn render_scalar(v: &Scalar, was: ScalarStyle) -> String {
    let s = match v {
        Scalar::Str(s) => s.clone(),
        // 非字符串一律裸写。给一个数字加引号，读回来就变成字符串了。
        other => return other.as_yaml_text(),
    };
    match was {
        // 原来就是单引号：能继续单引号就继续。单引号里什么都转义不了，要转义的字符一个
        // 都不能有
        ScalarStyle::SingleQuoted if !s.chars().any(must_escape) => single_quote(&s),
        ScalarStyle::DoubleQuoted => double_quoted(&s),
        // 块标量（`|` / `>`）改成单行会破坏缩进语义，交给双引号更安全
        ScalarStyle::Literal | ScalarStyle::Folded => double_quoted(&s),
        _ => {
            if needs_quotes(&s) {
                double_quoted(&s)
            } else {
                s
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> String {
        render_scalar(&Scalar::s(s), ScalarStyle::Plain)
    }

    #[test]
    fn an_ordinary_value_stays_unquoted() {
        // 给用户凭空加一对引号，diff 里就多一行噪音。
        assert_eq!(plain("sk-ant-abc123"), "sk-ant-abc123");
        assert_eq!(plain("官方"), "官方");
        assert_eq!(plain("claude-opus-4"), "claude-opus-4");
    }

    #[test]
    fn a_url_does_not_get_quoted_but_a_key_like_value_does() {
        // **`base_url` 是被改得最多的字段。**每个 URL 都套上一对用户
        // 没写的引号，正是想避免的那种 diff 噪音 —— 而
        // `http://a:8788` 本来就是一个合法的纯量。
        assert_eq!(plain("http://127.0.0.1:8788"), "http://127.0.0.1:8788");
        assert_eq!(plain("sk-a#b"), "sk-a#b");
        // 后面跟空格的冒号才是分隔符
        assert_eq!(plain("a: b"), "\"a: b\"");
        // 前面是空格的 # 才开启注释
        assert_eq!(plain("a #b"), "\"a #b\"");
        // 结尾的冒号也是分隔符
        assert_eq!(plain("a:"), "\"a:\"");
    }

    /// 单独读的时候是文档结束标记：新写一个键时，写之前那道检查读不回这个值
    #[test]
    fn a_value_starting_with_the_document_end_marker_is_quoted() {
        for s in ["...", "... x", "...x"] {
            assert!(plain(s).starts_with('"'), "{s} → {}", plain(s));
        }
        assert_eq!(plain("a ..."), "a ...");
    }

    #[test]
    fn flow_metacharacters_are_quoted_because_we_cannot_see_the_context() {
        for s in ["a,b", "a[b", "a]b", "a{b"] {
            assert!(plain(s).starts_with('"'), "{s} → {}", plain(s));
        }
    }

    #[test]
    fn things_that_look_like_other_types_get_quoted() {
        // **这是 YAML 最经典的坑。**`no` 不加引号读回来是 false ——
        // 一个叫 `no` 的 provider 会变成一个布尔值。
        for s in ["no", "yes", "on", "off", "true", "NULL", "~", "007", "1.5"] {
            assert!(
                plain(s).starts_with('"'),
                "{s} 该被引起来，实际 {}",
                plain(s)
            );
        }
    }

    #[test]
    fn a_number_written_as_a_number_is_not_quoted() {
        // 加了引号读回来就是字符串了。
        assert_eq!(
            render_scalar(&Scalar::Int(8788), ScalarStyle::Plain),
            "8788"
        );
        assert_eq!(
            render_scalar(&Scalar::Bool(false), ScalarStyle::DoubleQuoted),
            "false",
            "布尔不该因为原来带引号就被引起来"
        );
    }

    #[test]
    fn the_original_quoting_style_is_kept() {
        // 用户写的是单引号，我们改完还给他单引号 —— diff 越小越好。
        assert_eq!(
            render_scalar(&Scalar::s("abc"), ScalarStyle::SingleQuoted),
            "'abc'"
        );
        assert_eq!(
            render_scalar(&Scalar::s("abc"), ScalarStyle::DoubleQuoted),
            "\"abc\""
        );
    }

    #[test]
    fn quotes_inside_the_value_are_escaped_the_way_that_style_wants() {
        assert_eq!(
            render_scalar(&Scalar::s("it's"), ScalarStyle::SingleQuoted),
            "'it''s'"
        );
        assert_eq!(
            render_scalar(&Scalar::s("say \"hi\""), ScalarStyle::DoubleQuoted),
            "\"say \\\"hi\\\"\""
        );
    }

    #[test]
    fn an_empty_string_is_quoted_because_nothing_is_not_a_value() {
        // 裸写的话那一行变成 `key:`，读回来是 null 而不是空字符串。
        assert_eq!(plain(""), "\"\"");
    }

    #[test]
    fn leading_or_trailing_space_survives_only_inside_quotes() {
        assert_eq!(plain(" x"), "\" x\"");
        assert_eq!(plain("x "), "\"x \"");
    }
}

#[cfg(test)]
mod control_char_tests {
    use super::*;

    #[test]
    fn control_characters_are_escaped_not_written_raw() {
        // YAML 不允许控制字符裸着出现。写出去的文件我们自己的加载器都
        // 读不了 —— 而那是这一层能造成的最坏结果。
        assert_eq!(plain_of("\u{7f}"), "\"\\x7f\"");
        assert_eq!(plain_of("a\u{1}b"), "\"a\\x01b\"");
        // 有专门转义的那几个还是走专门的
        assert_eq!(plain_of("a\nb"), "\"a\\nb\"");
        assert_eq!(plain_of("a\tb"), "\"a\\tb\"");
    }

    fn plain_of(s: &str) -> String {
        render_scalar(&Scalar::s(s), ScalarStyle::Plain)
    }

    /// YAML 1.1 当换行的那几个（NEL、LS、PS）、C1、BOM 和两个非字符：不管原来是哪种
    /// 写法，一律写成转义过的双引号。原样写出去的话，配置的加载器要么整份读不了，要么
    /// 读回来变了样，而补丁层自己的自检（YAML 1.2）看不出来
    #[test]
    fn line_separators_and_the_rest_are_escaped_in_every_style() {
        let cases = [
            ("a\u{85}b", "\"a\\x85b\""),
            ("a\u{9b}b", "\"a\\x9bb\""),
            ("a\u{2028}b", "\"a\\u2028b\""),
            ("a\u{2029}b", "\"a\\u2029b\""),
            ("\u{feff}a", "\"\\ufeffa\""),
            ("a\u{fffe}\u{ffff}", "\"a\\ufffe\\uffff\""),
        ];
        for (s, want) in cases {
            for was in [
                ScalarStyle::Plain,
                ScalarStyle::SingleQuoted,
                ScalarStyle::DoubleQuoted,
                ScalarStyle::Literal,
            ] {
                assert_eq!(render_scalar(&Scalar::s(s), was), want, "{s:?} as {was:?}");
            }
        }
    }

    /// 单引号里什么都转义不了：带制表符、控制字符的值不再沿用单引号
    #[test]
    fn a_value_that_needs_escapes_leaves_single_quotes() {
        assert_eq!(
            render_scalar(&Scalar::s("a\tb"), ScalarStyle::SingleQuoted),
            "\"a\\tb\""
        );
        assert_eq!(
            render_scalar(&Scalar::s("a\u{1}b"), ScalarStyle::SingleQuoted),
            "\"a\\x01b\""
        );
        // 不需要转义的照旧单引号
        assert_eq!(
            render_scalar(&Scalar::s("a\u{a0}b"), ScalarStyle::SingleQuoted),
            "'a\u{a0}b'"
        );
    }

    /// 读得回来：转义过的写法，serde（配置的加载器）读到的就是原来那个字符串
    #[test]
    fn what_is_escaped_reads_back_exactly() {
        for s in [
            "a\u{85}b",
            "a\u{9b}b",
            "a\u{2028}b",
            "a\u{2029}b",
            "\u{feff}a",
            "a\u{fffe}\u{ffff}",
            "a\u{0}\u{7f}\tb",
        ] {
            let text = format!("k: {}\n", double_quoted(s));
            let v: serde_yaml_ng::Value =
                serde_yaml_ng::from_str(&text).unwrap_or_else(|e| panic!("{s:?}: {e}\n{text}"));
            assert_eq!(v["k"].as_str(), Some(s), "{text}");
        }
    }
}
