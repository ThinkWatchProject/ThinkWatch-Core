//! 扩充的两组规则（[`super::secrets`]、[`super::personal`]）共用的几个判据。
#![allow(dead_code)] // 两路接上之前没人用；集成时删掉这一行

/// 一个值是不是占位写法，而不是真值：`${VAR}`、`$VAR`、`{{x}}`、`<your-key>`、`xxx`、
/// `***`、`your_…`、`changeme`、`example` ……
///
/// 按位置认的规则（请求头、配置赋值、按字段名认的个人信息）都先过这一道：文档和示例里
/// 这些位置上写的多半是占位，换掉它只会让模型读不懂示例。
pub(super) fn placeholder_value(v: &str) -> bool {
    let t = v.trim_matches(|c| c == '"' || c == '\'');
    if t.is_empty() {
        return true;
    }
    let b = t.as_bytes();
    // 引用别的东西：环境变量、模板
    if b[0] == b'$' || t.starts_with("{{") || (b[0] == b'<' && b[b.len() - 1] == b'>') {
        return true;
    }
    // 同一个字符重复：xxxx、****、0000
    if b.iter().all(|&c| c == b[0]) {
        return true;
    }
    let lower = t.to_ascii_lowercase();
    const WORDS: &[&str] = &[
        "changeme",
        "change_me",
        "change-me",
        "example",
        "placeholder",
        "password",
        "secret",
        "redacted",
        "your_",
        "your-",
        "yourkey",
        "xxx",
        "***",
        "todo",
        "dummy",
        "sample",
        "test123",
    ];
    WORDS.iter().any(|w| lower.contains(w))
}

/// `at` 前面 `window` 个字符（按字符数，不是字节）里有没有其中一个关键词，不分大小写。
///
/// 给没有校验位、形状又太普通的号码用：上下文写明了是什么，才按它处理。
pub(super) fn preceded_by(text: &str, at: usize, window: usize, words: &[&str]) -> bool {
    let head = &text[..at];
    let from = head
        .char_indices()
        .rev()
        .nth(window.saturating_sub(1))
        .map_or(0, |(i, _)| i);
    let near = head[from..].to_lowercase();
    words.iter().any(|w| near.contains(&w.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_not_values() {
        for p in [
            "${OPENAI_API_KEY}",
            "$TOKEN",
            "{{ secret }}",
            "<your-api-key>",
            "xxxxxxxx",
            "********",
            "changeme",
            "your_password_here",
            "\"example-token\"",
        ] {
            assert!(placeholder_value(p), "{p}");
        }
        for v in ["hunter2hunter2", "s3cr3t-Pa55", "wJalrXUtnFEMI/K7MDENG"] {
            assert!(!placeholder_value(v), "{v}");
        }
    }

    #[test]
    fn keywords_count_only_within_the_window() {
        let t = "护照号码：E12345678";
        let at = t.find('E').unwrap();
        assert!(preceded_by(t, at, 20, &["护照"]));
        assert!(!preceded_by(t, at, 3, &["护照"]));
        let t = "Passport no. E12345678";
        assert!(preceded_by(t, t.find('E').unwrap(), 20, &["passport"]));
    }
}
