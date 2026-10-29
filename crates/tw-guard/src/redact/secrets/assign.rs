//! 配置文件里的密钥赋值：`.env`、YAML、TOML、INI、JSON、`~/.aws/credentials` 里名字写明是
//! 口令、密钥的那一行（`DB_PASSWORD=…`、`aws_secret_access_key = …`、`"apiKey": "…"`）。
//!
//! 把配置文件贴进对话是最常见的外泄方式，而这些值没有前缀可认。认的只是**写死的字面量**：
//! 引号里的一个值，或者不加引号、到行尾为止的一个值。代码里的表达式（`password =
//! request.form["password"]`、`API_KEY = os.getenv(…)`）、变量（`token = access_token`）、
//! 类型（`password: string`）都不是 —— 它们没有值，换掉只会让模型读不懂代码。
//!
//! 变量名从切 token 那一遍来：名字本身就是一个 token，结尾对得上 [`super::ASSIGNMENT_SUFFIXES`]
//! 才往后看。**只换值**，名字留着。

use std::ops::Range;

use super::super::rules::Hit;

/// [`super::ASSIGNMENT_SUFFIXES`] 去掉分隔符、小写（测试钉着两张表一致）
pub(super) const SUFFIXES: &[&[u8]] = &[
    b"secret",
    b"secretkey",
    b"password",
    b"passwd",
    b"pwd",
    b"token",
    b"apikey",
    b"accesskey",
    b"privatekey",
    b"clientsecret",
];

/// 名字以 `token` 结尾、却不是凭据的：翻页游标、分词器的特殊符号、防跨站的一次性令牌。
/// 还有 shell 的 `PWD`、`OLDPWD`（值是当前目录）
const NOT_SECRETS: &[&[u8]] = &[
    b"pagetoken",
    b"nexttoken",
    b"prevtoken",
    b"previoustoken",
    b"continuationtoken",
    b"cursortoken",
    b"synctoken",
    b"canceltoken",
    b"cancellationtoken",
    b"csrftoken",
    b"xsrftoken",
    b"eostoken",
    b"bostoken",
    b"padtoken",
    b"unktoken",
    b"masktoken",
    b"septoken",
    b"clstoken",
    b"stoptoken",
    b"specialtoken",
    b"maxtoken",
    b"oldpwd",
];

/// `b` 去掉 `_`、`-` 之后不分大小写地以 `s`（小写、无分隔符）结尾。返回 `s` 在 `b` 里的起点
fn ends_norm(b: &[u8], s: &[u8]) -> Option<usize> {
    let (mut i, mut j) = (b.len(), s.len());
    while j > 0 {
        if i == 0 {
            return None;
        }
        let c = b[i - 1];
        if (c == b'_' || c == b'-') && j < s.len() {
            i -= 1;
            continue;
        }
        if c.to_ascii_lowercase() != s[j - 1] {
            return None;
        }
        i -= 1;
        j -= 1;
    }
    Some(i)
}

/// 这个 token 作为变量名，结尾是不是口令、密钥那几个词之一，而且词前面是名字里的分界：开头、
/// `_`、`-`、`.`，或者驼峰（`dbPassword`）。`compassword`、`mytokens` 不算
#[inline]
pub(super) fn name_ends_right(tok: &str) -> bool {
    let b = tok.as_bytes();
    // 先看最后三个字母：每个词的结尾只有这几种。绝大多数 token 在这里就回去了
    let n = b.len();
    if !matches!(b[n - 1] | 0x20, b't' | b'y' | b'd' | b'n') {
        return false;
    }
    let last3 = [
        b[n - 3].to_ascii_lowercase(),
        b[n - 2].to_ascii_lowercase(),
        b[n - 1].to_ascii_lowercase(),
    ];
    if !matches!(&last3, b"ret" | b"key" | b"ord" | b"swd" | b"pwd" | b"ken") {
        return false;
    }
    let hit = SUFFIXES.iter().any(|s| {
        ends_norm(b, s).is_some_and(|i| {
            i == 0
                || matches!(b[i - 1], b'_' | b'-' | b'.')
                || (b[i].is_ascii_uppercase()
                    && (b[i - 1].is_ascii_lowercase() || b[i - 1].is_ascii_digit()))
        })
    });
    hit && !b.eq_ignore_ascii_case(b"pwd") && !NOT_SECRETS.iter().any(|s| ends_norm(b, s).is_some())
}

/// 不加引号的值里能有的字符：到空白、引号、反斜杠（JSON 里的转义）和代码里的括号、分隔为止
fn bare(c: u8) -> bool {
    c.is_ascii_graphic()
        && !matches!(
            c,
            b'"' | b'\''
                | b'`'
                | b'\\'
                | b','
                | b';'
                | b'('
                | b')'
                | b'{'
                | b'}'
                | b'['
                | b']'
                | b'<'
                | b'>'
                | b'&'
        )
}

/// `i` 处这一句话说完了：文本结尾、换行（真的，或者 JSON 里的 `\n`）、注释、语句和字面量
/// 之间的分隔，或者装着这段话的 JSON 字符串结束了
fn closes(b: &[u8], i: usize) -> bool {
    match b.get(i) {
        None => true,
        Some(b'\n' | b'\r' | b',' | b';' | b'}' | b')' | b']' | b'#' | b'"') => true,
        Some(b'/') => b.get(i + 1) == Some(&b'/'),
        Some(b'\\') => matches!(b.get(i + 1), Some(b'n' | b'r' | b'"' | b'\\')),
        _ => false,
    }
}

/// 名字（`name` 是它在原文里的区间）后面：可以先是引号（JSON 的键），然后是 `=`、`:`、
/// `=>`、`:=`，然后是一个字面量值。找得到就把值记下来
pub(super) fn after_name(text: &str, name: Range<usize>, placed: &mut Vec<Hit>) {
    use super::place::{quote_at, run, skip_blank};
    let b = text.as_bytes();
    // `$TOKEN = …`、`${TOKEN}` 是在用一个变量；`?token=` 是 URL 参数
    if name.start > 0 && matches!(b[name.start - 1], b'$' | b'?' | b'&' | b'{') {
        return;
    }
    let mut i = name.end;
    if let Some((n, _)) = quote_at(b, i) {
        i += n;
    }
    i = skip_blank(b, i);
    match (b.get(i), b.get(i + 1)) {
        (Some(b'='), Some(b'>')) | (Some(b':'), Some(b'=')) => i += 2,
        (Some(b'='), Some(b'=' | b'~')) | (Some(b':'), Some(b':')) => return,
        (Some(b'=' | b':'), _) => i += 1,
        _ => return,
    }
    i = skip_blank(b, i);
    let (from, to) = if let Some((n, q)) = quote_at(b, i) {
        let from = i + n;
        let to = run(b, from, |c| {
            c.is_ascii_graphic() && !matches!(c, b'"' | b'\'' | b'`' | b'\\')
        });
        // 同一种引号收尾，后面这一句就完了：`"…" + x`、`"…" % x` 是一段表达式
        if quote_at(b, to) != Some((n, q)) || !closes(b, skip_blank(b, to + n)) {
            return;
        }
        (from, to)
    } else {
        let to = run(b, i, bare);
        if !closes(b, skip_blank(b, to)) {
            return;
        }
        (i, to)
    };
    let v = &text[from..to];
    // 路径、URL、引用（`$X`、`%X%`、`@Value`、`!Ref`、`*alias`）不是口令
    if v.starts_with(['/', '~', '.', '$', '%', '@', '!', '*', '&']) || v.contains("://") {
        return;
    }
    if super::credible(v, 8) {
        placed.push(super::super::rules::builtin_hit(
            "config-assignment",
            from..to,
        ));
    }
}
