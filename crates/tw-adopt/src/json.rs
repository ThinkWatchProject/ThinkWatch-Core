//! 保序、保格式的 JSON 外科手术：**写入必须是字段级合并**。
//!
//! 规矩不是「把我们认识的字段合并进去」，而是**「除了这几个字段，其余
//! 字节原样不动」**。前者是拷贝 —— 拷贝就要枚举「要保留什么」，而那是
//! 个我们不控制、还在增长的集合，cc-switch 的 147 commits 撤回就撤在
//! 这儿。后者是原地手术，只要按 span 替换字节，没被点名的东西根本没有
//! 机会丢。
//!
//! 所以这里不用 `serde_json` 往返：反序列化再序列化会重排、重排缩进、
//! 重写转义，用户第二天打开文件会看到一份「我没动过它却全变了」的配置。
//! 我们自己扫一遍拿到每个值的字节区间，只切那几段。
//!
//! 顺带一个好处：扫描器把 `//` 和 `/* */` 当空白跳过，所以 Zed 那种带
//! 注释的 JSONC 也能改 —— 而且注释一个字都不会掉，因为我们从来不重新
//! 生成整个文件。

use std::collections::BTreeMap;
use std::ops::Range;

use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum JErr {
    #[error("byte {at} is not valid JSON: {msg}")]
    Syntax { at: usize, msg: String },
    #[error("{0} is not an object, so no field can be written into it")]
    NotObject(String),
    #[error("the file is empty")]
    Empty,
}

/// JSON 的语义值。数字保留字面量 —— 把 `1.0` 读成 f64 再写回会变成 `1`，
/// 而写回校验比的就是「除了那几处，其余完全一致」，浮点往返
/// 会让每一次校验都失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Val {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<Val>),
    Obj(Vec<(String, Val)>),
}

impl Val {
    pub fn s(v: impl Into<String>) -> Val {
        Val::Str(v.into())
    }
    /// 当成字符串读。给哨兵记录原值用。
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Val::Str(s) => Some(s),
            _ => None,
        }
    }
    /// 人能看懂的一行表示，写进哨兵注释里。
    pub fn to_line(&self) -> String {
        match self {
            Val::Str(s) => s.clone(),
            Val::Null => "null".into(),
            Val::Bool(b) => b.to_string(),
            Val::Num(n) => n.clone(),
            Val::Arr(es) => format!(
                "[{}]",
                es.iter()
                    .map(|e| e.to_line())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Val::Obj(ms) => format!(
                "{{{}}}",
                ms.iter()
                    .map(|(k, v)| format!("{k}: {}", v.to_line()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
    fn get<'a>(&'a self, path: &[&str]) -> Option<&'a Val> {
        let mut cur = self;
        for k in path {
            let Val::Obj(ms) = cur else { return None };
            cur = &ms.iter().find(|(mk, _)| mk == k)?.1;
        }
        Some(cur)
    }
    /// 键排序之后的样子。**只给写回校验用。**
    ///
    /// 校验要回答的是「除了点名的那几个字段，内容有没有变」，而不是
    /// 「键的顺序有没有变」。后者对 JSON 由 span 补丁本身保证（它压根
    /// 不重排），对 TOML 则根本无从谈起 —— `toml_edit` 会把新的值键排在
    /// 子表前面，这是 TOML 的语法要求，不是内容变化。带着顺序比，会让
    /// 每一次 Codex 接管都「校验失败」。
    pub fn normalized(&self) -> Val {
        match self {
            Val::Obj(ms) => {
                let mut ms: Vec<_> = ms
                    .iter()
                    .map(|(k, v)| (k.clone(), v.normalized()))
                    .collect();
                ms.sort_by(|a, b| a.0.cmp(&b.0));
                Val::Obj(ms)
            }
            Val::Arr(es) => Val::Arr(es.iter().map(|e| e.normalized()).collect()),
            other => other.clone(),
        }
    }

    /// 「原值 + 预期改动」—— 写回校验拿它当参照物。
    pub fn with(&self, path: &[&str], v: &Val) -> Val {
        let Some((head, rest)) = path.split_first() else {
            return v.clone();
        };
        let mut ms = match self {
            Val::Obj(ms) => ms.clone(),
            _ => Vec::new(),
        };
        match ms.iter_mut().find(|(k, _)| k == head) {
            Some(slot) => slot.1 = slot.1.with(rest, v),
            None => ms.push((head.to_string(), Val::Null.with(rest, v))),
        }
        Val::Obj(ms)
    }
    /// 删掉一个字段（还原时用）。路径不存在就原样返回。
    pub fn without(&self, path: &[&str]) -> Val {
        let Some((head, rest)) = path.split_first() else {
            return self.clone();
        };
        let Val::Obj(ms) = self else {
            return self.clone();
        };
        let mut out = Vec::with_capacity(ms.len());
        for (k, v) in ms {
            if k == head {
                if rest.is_empty() {
                    continue;
                }
                out.push((k.clone(), v.without(rest)));
            } else {
                out.push((k.clone(), v.clone()));
            }
        }
        Val::Obj(out)
    }
}

#[derive(Debug, Clone)]
struct Node {
    span: Range<usize>,
    body: Body,
}

#[derive(Debug, Clone)]
enum Body {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<Node>),
    Obj(Vec<Member>),
}

#[derive(Debug, Clone)]
struct Member {
    key: String,
    /// 从键的开引号到值的最后一个字节
    whole: Range<usize>,
    val: Node,
}

// ---------------------------------------------------------------- 扫描

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    loop {
        while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
            i += 1;
        }
        // JSONC：注释当空白。**只是跳过，不是删除** —— 它们的字节留在
        // 原处，因为我们从不重新生成文件。
        if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        return i;
    }
}

fn err(at: usize, msg: &str) -> JErr {
    JErr::Syntax {
        at,
        msg: msg.into(),
    }
}

/// 扫一个字符串字面量。`i` 停在开引号上，返回解码后的内容和收引号之后
/// 的位置。
///
/// **按字节扫是安全的**：UTF-8 的续字节都 ≥ 0x80，`"` 和 `\` 这两个
/// ASCII 字节不可能出现在多字节字符中间。这个项目已经被字节切片坑过
/// 三次，所以这句话必须写下来，不能靠「应该没问题」。
fn scan_str(b: &[u8], i: usize) -> Result<(String, usize), JErr> {
    if b.get(i) != Some(&b'"') {
        return Err(err(i, "a string was expected here"));
    }
    let mut out = String::new();
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'"' => return Ok((out, j + 1)),
            b'\\' => {
                j += 1;
                let c = *b
                    .get(j)
                    .ok_or_else(|| err(j, "the escape sequence is incomplete"))?;
                j += 1;
                match c {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let hex = |b: &[u8], p: usize| -> Result<u32, JErr> {
                            let s = b.get(p..p + 4).ok_or_else(|| {
                                err(p, "\\u is followed by fewer than four digits")
                            })?;
                            let s = std::str::from_utf8(s)
                                .map_err(|_| err(p, "\\u is not followed by hexadecimal digits"))?;
                            u32::from_str_radix(s, 16)
                                .map_err(|_| err(p, "\\u is not followed by hexadecimal digits"))
                        };
                        let hi = hex(b, j)?;
                        j += 4;
                        let ch = if (0xD800..0xDC00).contains(&hi) {
                            // 代理对。落单的高代理没法变成 char，用替换符
                            // 兜住 —— 解码只服务于比较和展示，原字节永远
                            // 还在文件里。
                            if b.get(j) == Some(&b'\\') && b.get(j + 1) == Some(&b'u') {
                                let lo = hex(b, j + 2)?;
                                j += 6;
                                char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))
                                    .unwrap_or('\u{fffd}')
                            } else {
                                '\u{fffd}'
                            }
                        } else {
                            char::from_u32(hi).unwrap_or('\u{fffd}')
                        };
                        out.push(ch);
                    }
                    _ => return Err(err(j - 1, "an escape sequence that is not recognized")),
                }
            }
            _ => {
                let start = j;
                while j < b.len() && b[j] != b'"' && b[j] != b'\\' {
                    j += 1;
                }
                out.push_str(
                    std::str::from_utf8(&b[start..j]).map_err(|_| err(start, "not UTF-8"))?,
                );
            }
        }
    }
    Err(err(i, "the string has no closing quote"))
}

fn parse_value(b: &[u8], i: usize) -> Result<(Node, usize), JErr> {
    let i = skip_ws(b, i);
    let c = *b.get(i).ok_or_else(|| err(i, "a value is missing here"))?;
    match c {
        b'{' => {
            let mut j = skip_ws(b, i + 1);
            let mut ms: Vec<Member> = Vec::new();
            loop {
                if b.get(j) == Some(&b'}') {
                    return Ok((
                        Node {
                            span: i..j + 1,
                            body: Body::Obj(ms),
                        },
                        j + 1,
                    ));
                }
                let (key, after_key) = scan_str(b, j)?;
                let kstart = j;
                let c = skip_ws(b, after_key);
                if b.get(c) != Some(&b':') {
                    return Err(err(c, "a colon was expected after the key"));
                }
                let (val, after_val) = parse_value(b, c + 1)?;
                ms.push(Member {
                    key,
                    whole: kstart..val.span.end,
                    val,
                });
                j = skip_ws(b, after_val);
                if b.get(j) == Some(&b',') {
                    j = skip_ws(b, j + 1);
                    continue; // 尾逗号：下一轮直接撞上 `}`，被上面接住
                }
                if b.get(j) == Some(&b'}') {
                    return Ok((
                        Node {
                            span: i..j + 1,
                            body: Body::Obj(ms),
                        },
                        j + 1,
                    ));
                }
                return Err(err(j, "the object is missing a comma or a closing brace"));
            }
        }
        b'[' => {
            let mut j = skip_ws(b, i + 1);
            let mut es = Vec::new();
            loop {
                if b.get(j) == Some(&b']') {
                    return Ok((
                        Node {
                            span: i..j + 1,
                            body: Body::Arr(es),
                        },
                        j + 1,
                    ));
                }
                let (v, after) = parse_value(b, j)?;
                es.push(v);
                j = skip_ws(b, after);
                if b.get(j) == Some(&b',') {
                    j = skip_ws(b, j + 1);
                    continue;
                }
                if b.get(j) == Some(&b']') {
                    return Ok((
                        Node {
                            span: i..j + 1,
                            body: Body::Arr(es),
                        },
                        j + 1,
                    ));
                }
                return Err(err(j, "the array is missing a comma or a closing bracket"));
            }
        }
        b'"' => {
            let (s, after) = scan_str(b, i)?;
            Ok((
                Node {
                    span: i..after,
                    body: Body::Str(s),
                },
                after,
            ))
        }
        _ => {
            let start = i;
            let mut j = i;
            while j < b.len()
                && !matches!(
                    b[j],
                    b' ' | b'\t' | b'\n' | b'\r' | b',' | b'}' | b']' | b'/'
                )
            {
                j += 1;
            }
            let lit = std::str::from_utf8(&b[start..j]).map_err(|_| err(start, "not UTF-8"))?;
            let body = match lit {
                "true" => Body::Bool(true),
                "false" => Body::Bool(false),
                "null" => Body::Null,
                "" => return Err(err(i, "a value is missing here")),
                n if n.parse::<f64>().is_ok() => Body::Num(n.to_string()),
                other => {
                    return Err(err(
                        start,
                        &format!("a literal that is not recognized: {other}"),
                    ));
                }
            };
            Ok((
                Node {
                    span: start..j,
                    body,
                },
                j,
            ))
        }
    }
}

fn parse(text: &str) -> Result<Node, JErr> {
    let b = text.as_bytes();
    if skip_ws(b, 0) >= b.len() {
        return Err(JErr::Empty);
    }
    let (n, after) = parse_value(b, 0)?;
    let rest = skip_ws(b, after);
    if rest < b.len() {
        return Err(err(
            rest,
            "there is extra content after the end of the document",
        ));
    }
    Ok(n)
}

fn to_val(n: &Node) -> Val {
    match &n.body {
        Body::Null => Val::Null,
        Body::Bool(b) => Val::Bool(*b),
        Body::Num(s) => Val::Num(s.clone()),
        Body::Str(s) => Val::Str(s.clone()),
        Body::Arr(es) => Val::Arr(es.iter().map(to_val).collect()),
        Body::Obj(ms) => Val::Obj(ms.iter().map(|m| (m.key.clone(), to_val(&m.val))).collect()),
    }
}

/// 整份文件的语义值。写回校验拿它和「原值 + 预期改动」比。
pub fn value(text: &str) -> Result<Val, JErr> {
    Ok(to_val(&parse(text)?))
}

/// 读一个字段的当前值。哨兵要记「原本是什么」，包括「原本没有」。
pub fn get(text: &str, path: &[&str]) -> Result<Option<Val>, JErr> {
    Ok(value(text)?.get(path).cloned())
}

// ---------------------------------------------------------------- 生成

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            // **非 ASCII 原样输出。**把中文转成 \uXXXX 在技术上没错，但
            // 用户打开文件会看到自己写的注释名变成一串乱码。
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn render(v: &Val, base: &str, unit: &str) -> String {
    match v {
        Val::Null => "null".into(),
        Val::Bool(b) => b.to_string(),
        Val::Num(n) => n.clone(),
        Val::Str(s) => escape(s),
        Val::Arr(es) if es.is_empty() => "[]".into(),
        Val::Arr(es) => {
            let inner = format!("{base}{unit}");
            let items: Vec<_> = es
                .iter()
                .map(|e| format!("{inner}{}", render(e, &inner, unit)))
                .collect();
            format!("[\n{}\n{base}]", items.join(",\n"))
        }
        Val::Obj(ms) if ms.is_empty() => "{}".into(),
        Val::Obj(ms) => {
            let inner = format!("{base}{unit}");
            let items: Vec<_> = ms
                .iter()
                .map(|(k, v)| format!("{inner}{}: {}", escape(k), render(v, &inner, unit)))
                .collect();
            format!("{{\n{}\n{base}}}", items.join(",\n"))
        }
    }
}

/// 某个位置所在行的缩进。插进去的字段要跟邻居对齐 —— 对不齐，用户下次
/// 打开会以为文件被搅乱了。
fn indent_at(text: &str, pos: usize) -> String {
    let line_start = text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    text[line_start..pos]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

/// 猜这份文件用几个空格缩进。猜错不会坏事，只是新字段对不齐。
fn unit_of(text: &str) -> String {
    let mut votes: BTreeMap<String, usize> = BTreeMap::new();
    for line in text.lines() {
        let ws: String = line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        if !ws.is_empty() && ws.len() <= 8 {
            *votes.entry(ws).or_default() += 1;
        }
    }
    votes
        .into_iter()
        .max_by_key(|(k, n)| (*n, std::cmp::Reverse(k.len())))
        .map(|(k, _)| k)
        .unwrap_or_else(|| "  ".into())
}

fn splice(text: &str, at: Range<usize>, with: &str) -> String {
    let mut out = String::with_capacity(text.len() + with.len());
    out.push_str(&text[..at.start]);
    out.push_str(with);
    out.push_str(&text[at.end..]);
    out
}

// ---------------------------------------------------------------- 改

/// 写一个字段：有就替换那一段字节，没有就插进去（缺的中间层一起补）。
///
/// **没被点名的字节一个都不动** —— 包括注释、空行、缩进风格、键的顺序。
pub fn set(text: &str, path: &[&str], v: &Val) -> Result<String, JErr> {
    if path.is_empty() {
        return Err(JErr::NotObject(String::new()));
    }
    let root = parse(text)?;
    let unit = unit_of(text);

    // 顺着走，走到走不动为止
    let mut node = &root;
    let mut depth = 0usize;
    while depth < path.len() {
        let Body::Obj(ms) = &node.body else {
            return Err(JErr::NotObject(path[..depth].join(".")));
        };
        match ms.iter().find(|m| m.key == path[depth]) {
            Some(m) => {
                node = &m.val;
                depth += 1;
            }
            None => break,
        }
    }

    if depth == path.len() {
        let base = indent_at(text, node.span.start);
        return Ok(splice(text, node.span.clone(), &render(v, &base, &unit)));
    }

    // 缺的层从里往外包起来，一次插进去
    let mut nested = v.clone();
    for k in path[depth + 1..].iter().rev() {
        nested = Val::Obj(vec![((*k).to_string(), nested)]);
    }
    insert_member(
        text,
        node,
        path[depth],
        &nested,
        &unit,
        &path[..depth].join("."),
    )
}

fn insert_member(
    text: &str,
    obj: &Node,
    key: &str,
    v: &Val,
    unit: &str,
    where_: &str,
) -> Result<String, JErr> {
    let Body::Obj(ms) = &obj.body else {
        return Err(JErr::NotObject(where_.to_string()));
    };
    let multiline = text[obj.span.clone()].contains('\n');

    if let Some(last) = ms.last() {
        let sep = if multiline {
            format!(",\n{}", indent_at(text, last.whole.start))
        } else {
            ", ".to_string()
        };
        let base = indent_at(text, last.whole.start);
        let piece = format!("{sep}{}: {}", escape(key), render(v, &base, unit));
        return Ok(splice(text, last.whole.end..last.whole.end, &piece));
    }

    // 空对象
    let base = indent_at(text, obj.span.start);
    let at = obj.span.start + 1;
    if multiline {
        let piece = format!(
            "\n{base}{unit}{}: {}",
            escape(key),
            render(v, &format!("{base}{unit}"), unit)
        );
        Ok(splice(text, at..at, &piece))
    } else {
        let inner = &text[at..obj.span.end - 1];
        let piece = format!("{}: {}", escape(key), render(v, &base, unit));
        if inner.is_empty() {
            Ok(splice(text, at..at, &piece))
        } else {
            Ok(splice(text, at..at, &format!("{piece},")))
        }
    }
}

/// 删掉一个字段。还原走的是这条路 —— 「原本没有」的字段要真的消失，
/// 而不是被写成空串。
pub fn remove(text: &str, path: &[&str]) -> Result<String, JErr> {
    let Some((leaf, parents)) = path.split_last() else {
        return Err(JErr::NotObject(String::new()));
    };
    let root = parse(text)?;
    let mut node = &root;
    for k in parents {
        let Body::Obj(ms) = &node.body else {
            return Ok(text.to_string());
        };
        match ms.iter().find(|m| m.key == *k) {
            Some(m) => node = &m.val,
            None => return Ok(text.to_string()),
        }
    }
    let Body::Obj(ms) = &node.body else {
        return Ok(text.to_string());
    };
    let Some(idx) = ms.iter().position(|m| m.key == *leaf) else {
        return Ok(text.to_string());
    };

    let cut = if idx + 1 < ms.len() {
        // 连着后面那个逗号一起删，下一个成员的缩进由它自己带着
        ms[idx].whole.start..ms[idx + 1].whole.start
    } else if idx > 0 {
        // 最后一个：往前吃掉逗号
        ms[idx - 1].whole.end..ms[idx].whole.end
    } else {
        ms[idx].whole.clone()
    };
    let out = splice(text, cut.clone(), "");
    Ok(drop_blank_line_at(&out, cut.start))
}

/// 删完之后如果那一行只剩空白，把整行拿掉。**只删全空白的行** —— 行里
/// 还有别的东西（比如一句注释）就留着。
fn drop_blank_line_at(text: &str, pos: usize) -> String {
    let pos = pos.min(text.len());
    let start = text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end = text[pos..]
        .find('\n')
        .map(|i| pos + i + 1)
        .unwrap_or(text.len());
    if text[start..end].trim().is_empty() && end > start {
        splice(text, start..end, "")
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_one_field_leaves_every_other_byte_alone() {
        // 这是整个模块存在的理由。cc-switch #6902 把 23 个顶层键写剩 1 个，
        // 就是因为它走的是「读成结构体再写回去」。
        let src = "{\n  \"a\": 1,\n  \"env\": {\n    \"X\": \"old\"\n  },\n  \"z\": [1, 2]\n}\n";
        let out = set(src, &["env", "X"], &Val::s("new")).unwrap();
        assert_eq!(
            out,
            "{\n  \"a\": 1,\n  \"env\": {\n    \"X\": \"new\"\n  },\n  \"z\": [1, 2]\n}\n"
        );
    }

    #[test]
    fn inserting_a_field_matches_the_neighbours_indentation() {
        let src = "{\n    \"env\": {\n        \"A\": 1\n    }\n}\n";
        let out = set(src, &["env", "B"], &Val::s("v")).unwrap();
        assert_eq!(
            out,
            "{\n    \"env\": {\n        \"A\": 1,\n        \"B\": \"v\"\n    }\n}\n"
        );
    }

    #[test]
    fn a_missing_middle_level_gets_created() {
        // ~/.claude/settings.json 里可能根本没有 env 段。
        let src = "{\n  \"model\": \"opus\"\n}\n";
        let out = set(
            src,
            &["env", "ANTHROPIC_BASE_URL"],
            &Val::s("http://127.0.0.1:8080"),
        )
        .unwrap();
        assert_eq!(
            out,
            "{\n  \"model\": \"opus\",\n  \"env\": {\n    \"ANTHROPIC_BASE_URL\": \"http://127.0.0.1:8080\"\n  }\n}\n"
        );
    }

    #[test]
    fn comments_survive_because_we_never_regenerate_the_file() {
        // Zed 的 settings.json 是 JSONC，用户在里面写满了注释。
        // serde_json 往返会把它们全部吃掉。
        let src =
            "{\n  // 我调了三个月的设置\n  \"theme\": \"dark\", /* 别动 */\n  \"env\": {}\n}\n";
        let out = set(src, &["env", "K"], &Val::s("v")).unwrap();
        assert!(out.contains("// 我调了三个月的设置"), "{out}");
        assert!(out.contains("/* 别动 */"), "{out}");
        assert!(out.contains("\"K\": \"v\""), "{out}");
    }

    #[test]
    fn key_order_is_preserved_including_keys_we_never_touch() {
        let src = r#"{"z":1,"a":2,"m":3}"#;
        let out = set(src, &["a"], &Val::Num("9".into())).unwrap();
        assert_eq!(out, r#"{"z":1,"a":9,"m":3}"#);
        let Val::Obj(ms) = value(&out).unwrap() else {
            panic!()
        };
        let keys: Vec<_> = ms.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["z", "a", "m"]);
    }

    #[test]
    fn multibyte_content_does_not_get_sliced_in_half() {
        // 这个项目已经被字节切片坑过三次。键、值、注释全用中文。
        let src = "{\n  // 中文注释\n  \"模型\": \"通义千问\",\n  \"env\": {\n    \"名字\": \"张三\"\n  }\n}\n";
        let out = set(src, &["env", "名字"], &Val::s("李四")).unwrap();
        assert!(out.contains("\"名字\": \"李四\""), "{out}");
        assert!(out.contains("\"模型\": \"通义千问\""), "{out}");
        let out2 = set(&out, &["新字段"], &Val::s("值")).unwrap();
        assert!(out2.contains("\"新字段\": \"值\""), "{out2}");
        assert_eq!(get(&out2, &["模型"]).unwrap(), Some(Val::s("通义千问")));
    }

    #[test]
    fn removing_restores_the_shape_the_file_had_before() {
        // 「原本没有」的字段还原时要真的消失。写成空串会让客户端
        // 拿着一个空 base URL 去连。
        let src = "{\n  \"model\": \"opus\"\n}\n";
        let with = set(src, &["env", "X"], &Val::s("v")).unwrap();
        let back = remove(&with, &["env", "X"]).unwrap();
        let back = remove(&back, &["env"]).unwrap();
        assert_eq!(back, src);
    }

    #[test]
    fn removing_a_middle_member_keeps_the_others_lined_up() {
        let src = "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}\n";
        assert_eq!(
            remove(src, &["b"]).unwrap(),
            "{\n  \"a\": 1,\n  \"c\": 3\n}\n"
        );
        assert_eq!(
            remove(src, &["c"]).unwrap(),
            "{\n  \"a\": 1,\n  \"b\": 2\n}\n"
        );
        assert_eq!(
            remove(src, &["a"]).unwrap(),
            "{\n  \"b\": 2,\n  \"c\": 3\n}\n"
        );
    }

    #[test]
    fn removing_a_field_that_is_not_there_is_not_an_error() {
        let src = "{\n  \"a\": 1\n}\n";
        assert_eq!(remove(src, &["nope"]).unwrap(), src);
        assert_eq!(remove(src, &["deep", "nope"]).unwrap(), src);
    }

    #[test]
    fn the_semantic_value_matches_original_plus_the_intended_change() {
        // 写回校验：写完重新解析，和「原文件 + 预期的那几处改动」
        // 比。对不上就拒绝落盘 —— 这比事后备份更前置。
        let src = "{\n  \"a\": 1,\n  \"env\": { \"X\": \"old\" }\n}\n";
        let out = set(src, &["env", "X"], &Val::s("new")).unwrap();
        let expected = value(src).unwrap().with(&["env", "X"], &Val::s("new"));
        assert_eq!(value(&out).unwrap(), expected);
    }

    #[test]
    fn a_number_keeps_its_literal_form() {
        // 1.0 读成 f64 再写回会变成 1，然后每一次写回校验都会失败。
        let src = r#"{"t": 1.0, "big": 100000000000000000000}"#;
        let out = set(src, &["x"], &Val::Bool(true)).unwrap();
        assert!(out.contains("1.0"), "{out}");
        assert!(out.contains("100000000000000000000"), "{out}");
    }

    #[test]
    fn empty_objects_get_their_first_member_both_ways() {
        assert_eq!(
            set("{}", &["a"], &Val::Num("1".into())).unwrap(),
            r#"{"a": 1}"#
        );
        assert_eq!(
            set("{\n}\n", &["a"], &Val::Num("1".into())).unwrap(),
            "{\n  \"a\": 1\n}\n"
        );
    }

    #[test]
    fn escapes_survive_a_round_trip() {
        let src = r#"{"p": "C:\\a\\b", "q": "say \"hi\"", "u": "\u00e9\ud83d\ude00"}"#;
        assert_eq!(get(src, &["p"]).unwrap(), Some(Val::s(r"C:\a\b")));
        assert_eq!(get(src, &["q"]).unwrap(), Some(Val::s("say \"hi\"")));
        assert_eq!(get(src, &["u"]).unwrap(), Some(Val::s("é😀")));
        let out = set(src, &["r"], &Val::s("tab\there")).unwrap();
        assert_eq!(get(&out, &["r"]).unwrap(), Some(Val::s("tab\there")));
        assert_eq!(get(&out, &["p"]).unwrap(), Some(Val::s(r"C:\a\b")));
    }

    #[test]
    fn a_broken_file_is_refused_rather_than_rewritten() {
        // 解析不了就什么都不做。绝不能「尽力而为」地写一个我们自己都
        // 没看懂的文件。
        assert!(matches!(
            set("{oops}", &["a"], &Val::Null),
            Err(JErr::Syntax { .. })
        ));
        assert!(matches!(set("", &["a"], &Val::Null), Err(JErr::Empty)));
        assert!(matches!(
            set("{\"a\":1} extra", &["b"], &Val::Null),
            Err(JErr::Syntax { .. })
        ));
    }

    #[test]
    fn writing_into_something_that_is_not_an_object_is_refused() {
        let src = r#"{"env": "not an object"}"#;
        assert_eq!(
            set(src, &["env", "X"], &Val::Null),
            Err(JErr::NotObject("env".into()))
        );
    }

    #[test]
    fn trailing_commas_are_tolerated() {
        // JSONC 允许，而且用户手写时经常留着。
        let src = "{\n  \"a\": 1,\n}\n";
        let out = set(src, &["b"], &Val::Num("2".into())).unwrap();
        assert_eq!(get(&out, &["b"]).unwrap(), Some(Val::Num("2".into())));
        assert_eq!(get(&out, &["a"]).unwrap(), Some(Val::Num("1".into())));
    }

    #[test]
    fn a_nested_value_folds_onto_one_readable_line() {
        // 哨兵注释和字段摘要都是一行一条，多行的缩进折进去只会变成一串
        // 空格。
        let v = value(r#"{"h":{"X-A":"b"},"l":[1,2]}"#).unwrap();
        assert_eq!(v.to_line(), "{h: {X-A: b}, l: [1, 2]}");
    }

    #[test]
    fn normalizing_ignores_key_order_but_not_content() {
        let a = value(r#"{"b":1,"a":{"y":2,"x":3}}"#).unwrap();
        let b = value(r#"{"a":{"x":3,"y":2},"b":1}"#).unwrap();
        assert_ne!(a, b, "带着顺序比的时候这两个本来就不一样");
        assert_eq!(a.normalized(), b.normalized());
        let c = value(r#"{"a":{"x":3,"y":9},"b":1}"#).unwrap();
        assert_ne!(a.normalized(), c.normalized(), "内容变了还说一样就白校验了");
    }

    #[test]
    fn without_removes_only_the_named_leaf() {
        let v = value(r#"{"a":1,"env":{"X":"x","Y":"y"}}"#).unwrap();
        let w = v.without(&["env", "X"]);
        assert_eq!(w.get(&["env", "X"]), None);
        assert_eq!(w.get(&["env", "Y"]), Some(&Val::s("y")));
        assert_eq!(w.get(&["a"]), Some(&Val::Num("1".into())));
    }
}
