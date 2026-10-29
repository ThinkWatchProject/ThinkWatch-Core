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

use super::super::anchor::{Anchor, Anchors};
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

/// 锚点：变量名以 `secret`、`key`、`password`、`passwd`、`pwd`、`token` 的最后三个字母结尾
/// （不分大小写），至少三个字符（[`ends_like_name`]）
pub(in crate::redact) const ANCHORS: Anchors = Anchors {
    near: None,
    chars: super::super::anchor::ANY_CHARS,
    list: &[Anchor::Ends(&["ret", "key", "ord", "swd", "pwd", "ken"], 3)],
    fine: Some(ends_like_name),
};

/// 最后三个字母是那几个词之一的结尾。[`name_ends_right`] 头一步看的就是它：绝大多数 token 在
/// 这里就回去了
#[inline]
pub(in crate::redact) fn ends_like_name(b: &[u8]) -> bool {
    let n = b.len();
    if n < 3 || !matches!(b[n - 1] | 0x20, b't' | b'y' | b'd' | b'n') {
        return false;
    }
    let last3 = [
        b[n - 3].to_ascii_lowercase(),
        b[n - 2].to_ascii_lowercase(),
        b[n - 1].to_ascii_lowercase(),
    ];
    matches!(&last3, b"ret" | b"key" | b"ord" | b"swd" | b"pwd" | b"ken")
}

/// 这个 token 作为变量名，结尾是不是口令、密钥那几个词之一，而且词前面是名字里的分界：开头、
/// `_`、`-`、`.`，或者驼峰（`dbPassword`）。`compassword`、`mytokens` 不算
#[inline]
pub(super) fn name_ends_right(tok: &str) -> bool {
    let b = tok.as_bytes();
    // 先看最后三个字母：每个词的结尾只有这几种
    if !ends_like_name(b) {
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
/// `=>`、`:=`，然后是一个字面量值。找得到就把值记下来。返回往后看到了哪儿（值多长都看，
/// 线性靠调用方跳过落在看过的那一段里的名字）。`env_seen` 是 `.env` 写法的宽扫描看到了哪儿，
/// 见 [`env_value`]
pub(super) fn after_name(
    text: &str,
    name: Range<usize>,
    placed: &mut Vec<Hit>,
    env_seen: &mut usize,
) -> usize {
    use super::place::{quote_at, run, skip_blank};
    let b = text.as_bytes();
    let none = name.end;
    // `$TOKEN = …`、`${TOKEN}` 是在用一个变量；`?token=`、`&token=` 是 URL 参数（`&` 也可以是
    // 转义过的 `\u0026`）
    if name.start > 0 && matches!(b[name.start - 1], b'$' | b'?' | b'&' | b'{')
        || super::place::amp_before(b, name.start)
    {
        return none;
    }
    let mut i = name.end;
    if let Some((n, _)) = quote_at(b, i) {
        i += n;
    }
    i = skip_blank(b, i);
    match (b.get(i), b.get(i + 1)) {
        (Some(b'='), Some(b'>')) | (Some(b':'), Some(b'=')) => i += 2,
        (Some(b'='), Some(b'=' | b'~')) | (Some(b':'), Some(b':')) => return none,
        (Some(b'=' | b':'), _) => i += 1,
        _ => return none,
    }
    // `.env`、shell 的写法：名字、`=`、值紧挨着（`DB_PASSWORD=…`）。这种写法里的值就是一个
    // 字面量，引号里可以有空格，不加引号的可以带着代码里才是分隔的标点，见 [`env_value`]
    let env = b[name.end] == b'=' && i == name.end + 1 && skip_blank(b, i) == i;
    i = skip_blank(b, i);
    // 宽扫描没认出来时，它看过的那一段**不算**看过：那一段里后面的名字（`f(token=x,password=…)`
    // 的 `password`）照样要按通常的写法看。它自己另记一份，同一段不再宽扫第二遍
    if env
        && i >= *env_seen
        && let Some((from, to, end)) = env_value(b, i, env_seen)
    {
        return push(text, from, to, placed).max(end);
    }
    let (from, to) = if let Some((n, q)) = quote_at(b, i) {
        let from = i + n;
        let to = run(b, from, |c| {
            c.is_ascii_graphic() && !matches!(c, b'"' | b'\'' | b'`' | b'\\')
        });
        // 同一种引号收尾，后面这一句就完了：`"…" + x`、`"…" % x` 是一段表达式
        if quote_at(b, to) != Some((n, q)) || !closes(b, skip_blank(b, to + n)) {
            return to;
        }
        (from, to)
    } else {
        let to = run(b, i, bare);
        if !closes(b, skip_blank(b, to)) {
            return to;
        }
        (i, to)
    };
    push(text, from, to, placed)
}

/// 找到的值像不像口令，像就记下来。返回 `to`
fn push(text: &str, from: usize, to: usize, placed: &mut Vec<Hit>) -> usize {
    let v = &text[from..to];
    // 路径、URL、引用（`$X`、`%X%`、`@Value`、`!Ref`、`*alias`）不是口令
    if v.starts_with(['/', '~', '.', '$', '%', '@', '!', '*', '&']) || v.contains("://") {
        return to;
    }
    if super::credible(v, 8) {
        placed.push(super::super::rules::builtin_hit(
            "config-assignment",
            from..to,
        ));
    }
    to
}

/// `.env` 写法（`NAME=值`）的值，`i` 是 `=` 后面。返回 (值的起点, 终点, 引号收尾之后)，
/// `seen` 记下宽扫描看到了哪儿（认没认出来都记：同一段不宽扫第二遍，整遍扫描是线性的）。
///
/// - 引号里的：到**同一行**里同一种引号为止，中间可以有空格（`"correct horse battery 9"`）；
/// - 不加引号的：到空白、行尾为止，`&,;()[]{}<>` 都算值里的（`Tr0ub4dor&3xyz`）。但下面这几种
///   是代码，不是 `.env`，交回给通常的写法（到第一个标点为止）：括号不配对（`f(token=abc)`
///   的 `)`）、以括号开头（数组、对象、模板）、像一次调用（`make_token(1)`）；`&名字=` 是
///   表单编码里的下一个参数，值在它前面截断
fn env_value(b: &[u8], i: usize, seen: &mut usize) -> Option<(usize, usize, usize)> {
    use super::place::{quote_at, run, skip_blank};
    if let Some((n, q)) = quote_at(b, i) {
        let from = i + n;
        // 反斜杠**按一整串数一次**：在串里每隔一个字节问一次 `quote_at`（它把剩下的反斜杠再数一遍）
        // 是平方级的，`TOKEN="` 后面跟着几百 KB 的反斜杠就要几秒
        let mut j = from;
        let mut closed = false;
        while j < b.len() {
            match b[j] {
                b'\\' => {
                    let k = run(b, j, |c| c == b'\\');
                    let count = k - j;
                    // 收尾的引号前面带着 n - 1 个反斜杠，再往前的成对，是正文里的反斜杠
                    if b.get(k) == Some(&q) && count + 1 >= n && (count + 1 - n) % 2 == 0 {
                        j = k + 1 - n;
                        closed = true;
                        break;
                    }
                    // 奇数个：最后一个转义了后面那个字符。转义的换行是这一行完了
                    if count % 2 == 1 {
                        if matches!(b.get(k), Some(b'n' | b'r')) {
                            j = k;
                            break;
                        }
                        j = k + 1;
                    } else {
                        j = k;
                    }
                }
                c if c == q && n == 1 => {
                    closed = true;
                    break;
                }
                b'\n' | b'\r' => break,
                // 转义过的引号收尾时，一个原样的 `"` 是装着这段话的 JSON 字符串结束了
                b'"' if n > 1 => break,
                _ => j += 1,
            }
        }
        *seen = (*seen).max(j);
        if !closed || !closes(b, skip_blank(b, j + n)) {
            return None;
        }
        return Some((from, j, j + n));
    }
    let mut to = run(b, i, |c| {
        c.is_ascii_graphic() && !matches!(c, b'"' | b'\'' | b'`' | b'\\')
    });
    *seen = (*seen).max(to);
    // 表单编码：`token=abc123&state=xyz` 的值到 `&` 为止，后面是下一个参数
    if let Some(amp) = b[i..to].iter().position(|&c| c == b'&') {
        let rest = &b[i + amp + 1..to];
        let name = rest
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
            .count();
        if name > 0 && rest.get(name) == Some(&b'=') {
            to = i + amp;
            let v = &b[i..to];
            return (v.iter().all(|&c| bare(c)) && !v.is_empty()).then_some((i, to, to));
        }
    }
    // shell 里分隔命令的 `;`：后面是这一行的结尾，或者 `fi`、`done` 这样的关键字
    if let Some(semi) = b[i..to]
        .iter()
        .enumerate()
        .position(|(k, &c)| c == b';' && (i + k + 1 == to || shell_keyword(&b[i + k + 1..to])))
    {
        to = i + semi;
    }
    let v = &b[i..to];
    if v.iter().all(|&c| bare(c)) || !closes(b, skip_blank(b, to)) || !literal(v) {
        // 没有多出来的标点（通常的写法照样认得出），或者是代码
        return None;
    }
    Some((i, to, to))
}

/// `b` 以一个 shell 的关键字开头，后面是结尾或者 `;`：`TOKEN=abc;fi` 的 `fi`
fn shell_keyword(b: &[u8]) -> bool {
    const WORDS: &[&[u8]] = &[
        b"fi", b"done", b"then", b"do", b"else", b"elif", b"esac", b"export", b"echo", b"unset",
    ];
    let n = b
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_')
        .count();
    // 关键字后面是值的结尾或者下一个 `;`：`then#…`、`do!…` 是口令里的一段
    WORDS.contains(&&b[..n]) && matches!(b.get(n), None | Some(b';'))
}

/// 一段带着括号、尖括号的值是不是一个字面量：不以括号开头，括号都配对，不像一次调用、一次
/// 下标（`cfg[key2]`、`arr[i+1]`）或者一个泛型类型（`Map<String,V2>`）。下标、泛型的前面是个
/// 名字；那一段本身就像口令（`S3cret[2024]…`、`Xk9mQ2pLz7wR<3>`，字母数字都有）时拿不准，
/// 当成字面量换掉 —— 宁可多换一处代码，不可漏掉一个口令
fn literal(v: &[u8]) -> bool {
    if matches!(v.first(), Some(b'(' | b'[' | b'{' | b'<')) {
        return false;
    }
    let ident = v
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.'))
        .count();
    if ident > 0 && (v[0].is_ascii_alphabetic() || v[0] == b'_') {
        match v.get(ident) {
            Some(b'(') => return false,
            Some(b'[' | b'<')
                if !std::str::from_utf8(&v[..ident]).is_ok_and(|x| super::credible(x, 4)) =>
            {
                return false;
            }
            _ => {}
        }
    }
    let mut depth = [0i32; 4];
    for &c in v {
        let (k, d) = match c {
            b'(' => (0, 1),
            b')' => (0, -1),
            b'[' => (1, 1),
            b']' => (1, -1),
            b'{' => (2, 1),
            b'}' => (2, -1),
            b'<' => (3, 1),
            b'>' => (3, -1),
            _ => continue,
        };
        depth[k] += d;
        if depth[k] < 0 {
            return false;
        }
    }
    depth == [0; 4]
}
