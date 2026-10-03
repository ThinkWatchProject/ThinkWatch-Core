//! 插件文件里的 manifest 字面量：在源码里找到 `export const manifest = { … }`，按**纯数据**
//! 读出来（连同它在源码里的字节范围），也能按固定的样子写回去。
//!
//! 插件的 JS 文件就是它的配置所在（契约附录四）：出错时怎么办（`on_error`）、范围
//! （`match`）、设置的值（`settings.<键>.value`）都写在 manifest 里，界面改它们就是改这一段
//! 字面量 —— **只换这一段字节，文件里别的字节一个不动**（[`rewrite`]）。所以 manifest 必须是
//! 纯数据：对象、数组、字符串、数字、`true`/`false`/`null`；键是名字或字符串，可以有结尾的
//! 逗号，数字前面可以有负号。表达式、展开、函数调用、算出来的键、带 `${}` 的模板字符串、
//! 引用、getter 都不行 —— 那样的东西读不准，也改不了。
//!
//! 加载时还要再核对一遍：这里读出来的得和沙箱求值出来的一模一样（[`crate::Runtime::load`]）。
//! 模块顶层又改了 manifest 的，同样不认。
//!
//! # 怎么找
//!
//! 不是完整的 JavaScript 解析器，是一个够用的分词器：认得注释、字符串、模板字符串（连同
//! `${…}` 里的代码）、正则字面量，这些东西里出现的 `export const manifest` 不会被当真。
//! 正则和除号按前一个记号分（和多数编辑器一样）。分错了的后果只是找不到、或者找到的不是
//! 那一份 —— 不是那一份的话和沙箱求值的对不上，加载照样失败，改写之后再编一遍也过不去，
//! 不会悄悄改错地方。
//!
//! # 写回去的样子（[`write`]）
//!
//! 两格缩进；能不加引号的键不加；字符串一律双引号；manifest 本身每个字段一行，里面的对象
//! 和数组放得进一行（[`WIDTH`] 列以内）就写成一行，放不下就一项一行；一项一行时每一项后面
//! 都有逗号。只由数据决定：同一份数据写出来永远一样。**字面量里的注释不保留**。

use std::ops::Range;

use serde_json::Value;

use crate::{OnError, Scope, SettingKind};

/// 写回去时一行最多几列（按字符数）。放得下的对象和数组写成一行
pub const WIDTH: usize = 80;

/// 最多嵌套几层。manifest 用不了几层，再深的只会是乱写的
const MAX_DEPTH: usize = 64;

// ── 数据 ─────────────────────────────────────────────────────────

/// manifest 字面量里的一个值。**对象的键按源码里的先后**，写回去时也是这个顺序
#[derive(Debug, Clone, PartialEq)]
pub enum Data {
    Null,
    Bool(bool),
    /// 和 JavaScript 一样是双精度浮点数。读出来的一定是有限的
    Number(f64),
    String(String),
    Array(Vec<Data>),
    Object(Vec<(String, Data)>),
}

impl Data {
    /// 对象里这个键的值
    pub fn get(&self, key: &str) -> Option<&Data> {
        match self {
            Data::Object(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut Data> {
        match self {
            Data::Object(m) => m.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// 写进对象：有这个键就换值，位置不变；没有就插在 `after` 里最后一个已有的键后面，一个
    /// 都没有就放在最前面
    fn set(&mut self, key: &str, value: Data, after: &[&str]) {
        let Data::Object(m) = self else { return };
        if let Some((_, v)) = m.iter_mut().find(|(k, _)| k == key) {
            *v = value;
            return;
        }
        let at = m
            .iter()
            .rposition(|(k, _)| after.contains(&k.as_str()))
            .map_or(0, |i| i + 1);
        m.insert(at, (key.to_string(), value));
    }

    fn remove(&mut self, key: &str) {
        if let Data::Object(m) = self {
            m.retain(|(k, _)| k != key);
        }
    }

    /// 换成 JSON。±2^53 以内的整数写成整数 —— 和沙箱求值之后交回来的一样
    pub fn to_json(&self) -> Value {
        match self {
            Data::Null => Value::Null,
            Data::Bool(b) => Value::Bool(*b),
            Data::Number(x) => number_json(*x),
            Data::String(s) => Value::String(s.clone()),
            Data::Array(a) => Value::Array(a.iter().map(Data::to_json).collect()),
            Data::Object(m) => {
                Value::Object(m.iter().map(|(k, v)| (k.clone(), v.to_json())).collect())
            }
        }
    }

    /// 从 JSON 来（设置的值、范围里的模式）。对象的键按 JSON 里的先后；不是有限数的数字
    /// 是 None
    pub fn from_json(v: &Value) -> Option<Data> {
        Some(match v {
            Value::Null => Data::Null,
            Value::Bool(b) => Data::Bool(*b),
            Value::Number(n) => {
                let x = n.as_f64()?;
                if !x.is_finite() {
                    return None;
                }
                Data::Number(x)
            }
            Value::String(s) => Data::String(s.clone()),
            Value::Array(a) => Data::Array(a.iter().map(Data::from_json).collect::<Option<_>>()?),
            Value::Object(m) => Data::Object(
                m.iter()
                    .map(|(k, v)| Some((k.clone(), Data::from_json(v)?)))
                    .collect::<Option<_>>()?,
            ),
        })
    }
}

fn number_json(x: f64) -> Value {
    const SAFE: f64 = 9_007_199_254_740_992.0;
    if x.fract() == 0.0 && x.abs() <= SAFE {
        // -0 也写成 0：JSON.stringify(-0) 就是 "0"
        return Value::from(x as i64);
    }
    serde_json::Number::from_f64(x).map_or(Value::Null, Value::Number)
}

// ── 找 ───────────────────────────────────────────────────────────

/// 源码里的 manifest 字面量。
#[derive(Debug, Clone, PartialEq)]
pub struct Literal {
    /// 从 `{` 到和它配对的 `}`（含）在源码里的字节范围
    pub span: Range<usize>,
    /// 读出来的值，一定是 [`Data::Object`]
    pub data: Data,
}

/// 找不到 manifest，或者它不是纯数据。行列从 1 起（列按字符数），说得出位置才有
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct NotData {
    pub message: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

/// 在源码里找 `export const manifest = { … }`，把那个对象字面量按纯数据读出来。
pub fn find(source: &str) -> Result<Literal, NotData> {
    let mut c = Cursor::new(source);
    let mut found: Option<Literal> = None;
    match scan(&mut c, &mut found) {
        Ok(()) => found.ok_or_else(|| NotData {
            message: "the manifest has to be declared as `export const manifest = { … }`".into(),
            line: None,
            column: None,
        }),
        Err(Stop::Twice(e)) => Err(e),
        // manifest 之后的代码读不懂（正则和除号分错了之类）不碍事：manifest 已经读出来了
        Err(Stop::Unreadable(e)) => found.ok_or(e),
    }
}

/// 走不下去的原因
enum Stop {
    /// manifest 声明了两次
    Twice(NotData),
    /// 读不懂（没收尾的字符串、注释……），或者 manifest 不是纯数据
    Unreadable(NotData),
}

impl From<NotData> for Stop {
    fn from(e: NotData) -> Self {
        Stop::Unreadable(e)
    }
}

/// 走一遍整个文件。找到的 manifest 放进 `found`
fn scan(c: &mut Cursor<'_>, found: &mut Option<Literal>) -> Result<(), Stop> {
    // 文件开头的 BOM 和 `#!` 那一行
    c.eat('\u{feff}');
    if c.rest().starts_with("#!") {
        c.skip_line();
    }
    // 下一个 `/` 是不是正则的开头：前一个记号结束了一个表达式的话就是除号
    let mut regex_ok = true;
    // 前一个记号是 `.`（`a.delete` 里的 `delete` 是属性名，不是关键字）
    let mut after_dot = false;
    // 花括号的层数，和模板字符串里每个 `${` 开始时的层数
    let mut depth = 0usize;
    let mut subst: Vec<usize> = Vec::new();
    // export const manifest = {：读到第几个了
    let mut state = 0u8;
    loop {
        c.trivia()?;
        let start = c.pos;
        let Some(ch) = c.peek() else { return Ok(()) };
        let top = depth == 0 && subst.is_empty();
        if state == 4 {
            state = 0;
            if found.is_some() {
                return Err(Stop::Twice(c.err(start, "the manifest is declared twice")));
            }
            if ch != '{' {
                return Err(c
                    .err(
                        start,
                        "the manifest has to be an object literal: `export const manifest = { … }`",
                    )
                    .into());
            }
            let data = Parser {
                c: &mut *c,
                depth: 0,
            }
            .object()?;
            *found = Some(Literal {
                span: start..c.pos,
                data,
            });
            regex_ok = false;
            after_dot = false;
            continue;
        }
        let mut word: Option<&str> = None;
        let mut punct: Option<char> = None;
        let ends_expr = match ch {
            '\'' | '"' => {
                c.skip_string(ch)?;
                true
            }
            '`' => {
                c.bump();
                if c.skip_template()? {
                    subst.push(depth);
                    false
                } else {
                    true
                }
            }
            '{' => {
                c.bump();
                depth += 1;
                false
            }
            '}' => {
                c.bump();
                if subst.last() == Some(&depth) {
                    // `${…}` 到头了，接着读模板字符串
                    subst.pop();
                    if c.skip_template()? {
                        subst.push(depth);
                        false
                    } else {
                        true
                    }
                } else {
                    depth = depth.saturating_sub(1);
                    // 一个块到头了：后面可以是一条以正则开头的语句
                    false
                }
            }
            '/' if regex_ok => {
                c.skip_regex()?;
                true
            }
            '0'..='9' => {
                c.skip_number();
                true
            }
            '.' if c.peek_second().is_some_and(|d| d.is_ascii_digit()) => {
                c.skip_number();
                true
            }
            '#' => {
                // 私有名字（`#x`）
                c.bump();
                c.skip_ident_rest();
                true
            }
            ch if is_ident_start(ch) || ch == '\\' => {
                let w = c.ident_raw();
                word = Some(w);
                after_dot || !EXPRESSION_FOLLOWS.contains(&w)
            }
            ')' | ']' => {
                c.bump();
                true
            }
            '+' | '-' => {
                c.bump();
                if c.eat(ch) {
                    // `++`、`--`：跟在表达式后面是后缀，表达式到这里还没完
                    !regex_ok
                } else {
                    false
                }
            }
            _ => {
                c.bump();
                punct = Some(ch);
                false
            }
        };
        after_dot = punct == Some('.');
        regex_ok = !ends_expr;
        state = match (state, word, punct) {
            (_, Some("export"), _) if top => 1,
            (1, Some("const"), _) => 2,
            (2, Some("manifest"), _) => 3,
            // `=`，不是 `==`、`=>`
            (3, _, Some('=')) if !matches!(c.peek(), Some('=' | '>')) => 4,
            _ => 0,
        };
    }
}

/// 这些关键字后面跟的是一个表达式：`/` 在它们后面是正则的开头
const EXPRESSION_FOLLOWS: &[&str] = &[
    "return",
    "typeof",
    "instanceof",
    "in",
    "of",
    "new",
    "delete",
    "void",
    "throw",
    "case",
    "do",
    "else",
    "yield",
    "await",
    "extends",
];

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\u{b}' | '\u{c}' | ' ' | '\u{a0}' | '\u{feff}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// 名字的第一个字符。ASCII 之外的字符除了空白和换行都按名字算 —— 这里只是分词，
/// 哪些字符真能做名字是沙箱编译时的事
fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic()
        || c == '$'
        || c == '_'
        || (!c.is_ascii() && !is_space(c) && !is_line_terminator(c))
}

fn is_ident_part(c: char) -> bool {
    is_ident_start(c) || c.is_ascii_digit()
}

/// 源码上的一个位置
struct Cursor<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn peek_second(&self) -> Option<char> {
        let mut it = self.rest().chars();
        it.next();
        it.next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += c.len_utf8();
            true
        } else {
            false
        }
    }

    /// `at` 那个位置上的错。行列按 `\n`、`\r\n`、`\r` 换行数（编辑器就是这么数的）
    fn err(&self, at: usize, message: impl Into<String>) -> NotData {
        let before = &self.src[..at.min(self.src.len())];
        let mut line = 1u32;
        let mut column = 1u32;
        let mut chars = before.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    line = line.saturating_add(1);
                    column = 1;
                }
                '\n' => {
                    line = line.saturating_add(1);
                    column = 1;
                }
                _ => column = column.saturating_add(1),
            }
        }
        NotData {
            message: message.into(),
            line: Some(line),
            column: Some(column),
        }
    }

    fn skip_line(&mut self) {
        while let Some(c) = self.peek() {
            if is_line_terminator(c) {
                return;
            }
            self.bump();
        }
    }

    /// 跳过空白、换行和注释。没收尾的块注释是错
    fn trivia(&mut self) -> Result<(), NotData> {
        loop {
            let Some(c) = self.peek() else { return Ok(()) };
            if is_space(c) || is_line_terminator(c) {
                self.bump();
            } else if self.rest().starts_with("//") {
                self.skip_line();
            } else if self.rest().starts_with("/*") {
                let at = self.pos;
                match self.src[at + 2..].find("*/") {
                    Some(i) => self.pos = at + 2 + i + 2,
                    None => return Err(self.err(at, "a comment is not closed")),
                }
            } else {
                return Ok(());
            }
        }
    }

    /// 跳过一个字符串（`'…'`、`"…"`）
    fn skip_string(&mut self, quote: char) -> Result<(), NotData> {
        let open = self.pos;
        self.bump();
        loop {
            match self.bump() {
                None => return Err(self.err(open, "a string is not closed")),
                Some('\\') => {
                    if self.bump().is_none() {
                        return Err(self.err(open, "a string is not closed"));
                    }
                }
                Some(c) if c == quote => return Ok(()),
                Some('\n' | '\r') => {
                    return Err(self.err(open, "a string is not closed before the end of its line"));
                }
                Some(_) => {}
            }
        }
    }

    /// 跳过模板字符串的一段（开头的 `` ` `` 或者 `${…}` 收尾的 `}` 已经读过了）。
    /// 停在一个 `${` 之后是 true，停在收尾的 `` ` `` 之后是 false
    fn skip_template(&mut self) -> Result<bool, NotData> {
        let open = self.pos;
        loop {
            match self.bump() {
                None => return Err(self.err(open, "a template literal is not closed")),
                Some('\\') => {
                    if self.bump().is_none() {
                        return Err(self.err(open, "a template literal is not closed"));
                    }
                }
                Some('`') => return Ok(false),
                Some('$') if self.eat('{') => return Ok(true),
                Some(_) => {}
            }
        }
    }

    /// 跳过一个正则字面量，连同后面的标志
    fn skip_regex(&mut self) -> Result<(), NotData> {
        let open = self.pos;
        self.bump();
        let mut class = false;
        loop {
            match self.bump() {
                None => return Err(self.err(open, "a regular expression is not closed")),
                Some(c) if is_line_terminator(c) => {
                    return Err(self.err(open, "a regular expression is not closed"));
                }
                Some('\\') => match self.bump() {
                    Some(c) if !is_line_terminator(c) => {}
                    _ => return Err(self.err(open, "a regular expression is not closed")),
                },
                Some('[') => class = true,
                Some(']') => class = false,
                Some('/') if !class => break,
                Some(_) => {}
            }
        }
        self.skip_ident_rest();
        Ok(())
    }

    /// 跳过一个数字（分词用，不求值）
    fn skip_number(&mut self) {
        let radix_prefix = {
            let r = self.rest().as_bytes();
            r.len() > 1 && r[0] == b'0' && matches!(r[1], b'x' | b'X' | b'o' | b'O' | b'b' | b'B')
        };
        if radix_prefix {
            self.pos += 2;
            self.skip_ident_rest();
            return;
        }
        let mut exp = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == '_' || c == '.' {
                self.bump();
            } else if (c == 'e' || c == 'E') && !exp {
                exp = true;
                self.bump();
                if matches!(self.peek(), Some('+' | '-')) {
                    self.bump();
                }
            } else {
                break;
            }
        }
        self.skip_ident_rest();
    }

    fn skip_ident_rest(&mut self) {
        while let Some(c) = self.peek() {
            if is_ident_part(c) {
                self.bump();
            } else if c == '\\' {
                // 名字里的 `a`
                self.bump();
                self.bump();
            } else {
                break;
            }
        }
    }

    /// 读一个名字，原样（转义不解开）
    fn ident_raw(&mut self) -> &'a str {
        let start = self.pos;
        if self.peek() == Some('\\') {
            self.bump();
            self.bump();
        } else {
            self.bump();
        }
        self.skip_ident_rest();
        &self.src[start..self.pos]
    }
}

// ── 读成数据 ─────────────────────────────────────────────────────

struct Parser<'a, 'b> {
    c: &'b mut Cursor<'a>,
    depth: usize,
}

impl Parser<'_, '_> {
    fn enter(&mut self, at: usize) -> Result<(), NotData> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.c.err(at, "the manifest is nested too deeply"));
        }
        Ok(())
    }

    /// 一个值后面只能是 `,` 或者收尾的括号：别的都说明它是一个表达式的开头
    fn after_value(&mut self, close: char, open: usize) -> Result<bool, NotData> {
        self.c.trivia()?;
        let at = self.c.pos;
        match self.c.peek() {
            Some(',') => {
                self.c.bump();
                Ok(false)
            }
            Some(c) if c == close => {
                self.c.bump();
                Ok(true)
            }
            Some('(') => Err(self.c.err(at, "a function call is not data")),
            Some(_) => Err(self
                .c
                .err(at, "an expression is not data; write the value itself")),
            None => Err(self.c.err(
                open,
                if close == '}' {
                    "an object is not closed"
                } else {
                    "a list is not closed"
                },
            )),
        }
    }

    fn object(mut self) -> Result<Data, NotData> {
        self.object_at()
    }

    fn object_at(&mut self) -> Result<Data, NotData> {
        let open = self.c.pos;
        self.enter(open)?;
        self.c.bump();
        let mut members: Vec<(String, Data)> = Vec::new();
        loop {
            self.c.trivia()?;
            let at = self.c.pos;
            match self.c.peek() {
                None => return Err(self.c.err(open, "an object is not closed")),
                Some('}') => {
                    self.c.bump();
                    break;
                }
                _ => {}
            }
            let key = self.key()?;
            if key == "__proto__" {
                return Err(self.c.err(
                    at,
                    "`__proto__` cannot be a key: it sets the prototype instead",
                ));
            }
            if members.iter().any(|(k, _)| *k == key) {
                return Err(self.c.err(at, format!("the key `{key}` appears twice")));
            }
            self.c.trivia()?;
            let colon = self.c.pos;
            match self.c.peek() {
                Some(':') => {
                    self.c.bump();
                }
                Some('(') => return Err(self.c.err(at, "a method is not data")),
                Some(',' | '}') => {
                    return Err(self.c.err(
                        at,
                        format!("`{key}` on its own refers to a variable; write `{key}: value`"),
                    ));
                }
                _ => return Err(self.c.err(colon, "expected `:` after the key")),
            }
            let v = self.value()?;
            members.push((key, v));
            if self.after_value('}', open)? {
                break;
            }
        }
        self.depth -= 1;
        Ok(Data::Object(members))
    }

    fn array(&mut self) -> Result<Data, NotData> {
        let open = self.c.pos;
        self.enter(open)?;
        self.c.bump();
        let mut items = Vec::new();
        loop {
            self.c.trivia()?;
            let at = self.c.pos;
            match self.c.peek() {
                None => return Err(self.c.err(open, "a list is not closed")),
                Some(']') => {
                    self.c.bump();
                    break;
                }
                Some(',') => return Err(self.c.err(at, "an empty slot in a list is not data")),
                _ => {}
            }
            items.push(self.value()?);
            if self.after_value(']', open)? {
                break;
            }
        }
        self.depth -= 1;
        Ok(Data::Array(items))
    }

    fn key(&mut self) -> Result<String, NotData> {
        let at = self.c.pos;
        match self.c.peek() {
            Some(q @ ('"' | '\'')) => self.c.string(q),
            Some('[') => Err(self.c.err(at, "a computed key ([…]) is not data")),
            Some('.') if self.c.rest().starts_with("...") => {
                Err(self.c.err(at, "a spread (`...`) is not data"))
            }
            Some('*') => Err(self.c.err(at, "a generator method is not data")),
            Some('`') => Err(self.c.err(at, "a template literal cannot be a key")),
            Some(c) if c.is_ascii_digit() || c == '.' => Err(self.c.err(
                at,
                "a number cannot be a key here; write the key as a name or in quotes",
            )),
            Some('\\') => Err(self
                .c
                .err(at, "write the key without escapes, or put it in quotes")),
            Some(c) if is_ident_start(c) => {
                let w = self.c.ident_raw();
                if w.contains('\\') {
                    return Err(self
                        .c
                        .err(at, "write the key without escapes, or put it in quotes"));
                }
                if matches!(w, "get" | "set" | "async") {
                    // 后面紧跟着又一个键：getter、setter 或 async 方法
                    let save = self.c.pos;
                    self.c.trivia()?;
                    let next = self.c.peek();
                    self.c.pos = save;
                    if next.is_some_and(|n| {
                        is_ident_start(n)
                            || n.is_ascii_digit()
                            || matches!(n, '"' | '\'' | '[' | '*' | '#' | '\\')
                    }) {
                        return Err(self
                            .c
                            .err(at, "a getter, setter or async method is not data"));
                    }
                }
                Ok(w.to_string())
            }
            _ => Err(self.c.err(at, "expected a key")),
        }
    }

    fn value(&mut self) -> Result<Data, NotData> {
        self.c.trivia()?;
        let at = self.c.pos;
        let Some(ch) = self.c.peek() else {
            return Err(self.c.err(at, "the manifest ends in the middle of a value"));
        };
        match ch {
            '{' => self.object_at(),
            '[' => self.array(),
            '"' | '\'' => Ok(Data::String(self.c.string(ch)?)),
            '`' => Ok(Data::String(self.c.template()?)),
            '-' => {
                self.c.bump();
                self.c.trivia()?;
                match self.c.peek() {
                    Some(d) if d.is_ascii_digit() => Ok(Data::Number(-self.c.number()?)),
                    Some('.') if self.c.peek_second().is_some_and(|d| d.is_ascii_digit()) => {
                        Ok(Data::Number(-self.c.number()?))
                    }
                    _ => Err(self
                        .c
                        .err(at, "only a number can follow a minus sign in the manifest")),
                }
            }
            d if d.is_ascii_digit() => Ok(Data::Number(self.c.number()?)),
            '.' if self.c.peek_second().is_some_and(|d| d.is_ascii_digit()) => {
                Ok(Data::Number(self.c.number()?))
            }
            '.' if self.c.rest().starts_with("...") => {
                Err(self.c.err(at, "a spread (`...`) is not data"))
            }
            '(' => Err(self.c.err(at, "an expression in parentheses is not data")),
            '/' => Err(self.c.err(at, "a regular expression is not data")),
            c if is_ident_start(c) || c == '\\' => {
                let w = self.c.ident_raw();
                match w {
                    "true" => Ok(Data::Bool(true)),
                    "false" => Ok(Data::Bool(false)),
                    "null" => Ok(Data::Null),
                    "undefined" => Err(self
                        .c
                        .err(at, "`undefined` is not data; leave the field out instead")),
                    "NaN" | "Infinity" => Err(self.c.err(at, format!("`{w}` is not data"))),
                    "function" | "class" | "async" | "new" | "await" | "typeof" | "void"
                    | "delete" | "this" | "super" | "import" | "yield" => {
                        Err(self.c.err(at, "code is not data; write the value itself"))
                    }
                    _ => {
                        self.c.trivia()?;
                        if self.c.peek() == Some('(') {
                            Err(self.c.err(at, "a function call is not data"))
                        } else {
                            Err(self.c.err(
                                at,
                                format!(
                                    "`{w}` refers to a variable; the manifest can only hold values"
                                ),
                            ))
                        }
                    }
                }
            }
            _ => Err(self
                .c
                .err(at, "an expression is not data; write the value itself")),
        }
    }
}

impl Cursor<'_> {
    /// 一个字符串字面量的值（`'…'`、`"…"`）
    fn string(&mut self, quote: char) -> Result<String, NotData> {
        let open = self.pos;
        self.bump();
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(self.err(open, "a string is not closed")),
                Some(c) if c == quote => return Ok(out),
                Some('\\') => self.escape(&mut out, open, false)?,
                Some('\n' | '\r') => {
                    return Err(self.err(open, "a string is not closed before the end of its line"));
                }
                Some(c) => out.push(c),
            }
        }
    }

    /// 一个没有 `${…}` 的模板字符串的值。换行（`\r\n`、`\r`）读成 `\n`，和 JavaScript 一样
    fn template(&mut self) -> Result<String, NotData> {
        let open = self.pos;
        self.bump();
        let mut out = String::new();
        loop {
            let at = self.pos;
            match self.bump() {
                None => return Err(self.err(open, "a template literal is not closed")),
                Some('`') => return Ok(out),
                Some('\\') => self.escape(&mut out, open, true)?,
                Some('$') if self.peek() == Some('{') => {
                    return Err(self.err(at, "a template literal with `${…}` is not data"));
                }
                Some('\r') => {
                    self.eat('\n');
                    out.push('\n');
                }
                Some(c) => out.push(c),
            }
        }
    }

    /// 反斜杠之后的那一段（反斜杠已经读过了）
    fn escape(&mut self, out: &mut String, open: usize, template: bool) -> Result<(), NotData> {
        let at = self.pos - 1;
        let Some(c) = self.bump() else {
            return Err(self.err(
                open,
                if template {
                    "a template literal is not closed"
                } else {
                    "a string is not closed"
                },
            ));
        };
        match c {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'v' => out.push('\u{b}'),
            '0' if !self.peek().is_some_and(|d| d.is_ascii_digit()) => out.push('\0'),
            '0'..='9' => {
                return Err(self.err(at, "octal escapes such as \\1 or \\07 are not allowed"));
            }
            'x' => {
                let v = self.hex_digits(2).ok_or_else(|| {
                    self.err(at, "\\x has to be followed by two hexadecimal digits")
                })?;
                out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
            }
            'u' => {
                let ch = self.unicode_escape(at)?;
                out.push(ch);
            }
            // 续行：反斜杠加换行什么都不是
            '\r' => {
                self.eat('\n');
            }
            '\n' | '\u{2028}' | '\u{2029}' => {}
            other => out.push(other),
        }
        Ok(())
    }

    /// 正好 `n` 位十六进制数
    fn hex_digits(&mut self, n: usize) -> Option<u32> {
        let digits = self.rest().get(..n)?;
        if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let v = u32::from_str_radix(digits, 16).ok()?;
        self.pos += n;
        Some(v)
    }

    /// `\u` 之后：`XXXX` 或 `{X…}`。代理对拼成一个字符，落单的代理是错
    fn unicode_escape(&mut self, at: usize) -> Result<char, NotData> {
        let first = self.code_unit(at)?;
        if (0xd800..0xdc00).contains(&first) {
            // 高位代理：后面得紧跟一个低位代理
            if self.rest().starts_with("\\u") {
                let save = self.pos;
                self.pos += 2;
                let second = self.code_unit(save)?;
                if (0xdc00..0xe000).contains(&second) {
                    let cp = 0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00);
                    return char::from_u32(cp).ok_or_else(|| self.err(at, "an invalid \\u escape"));
                }
            }
            return Err(self.err(
                at,
                "a lone surrogate (\\uD800–\\uDFFF without its pair) cannot be read",
            ));
        }
        if (0xdc00..0xe000).contains(&first) {
            return Err(self.err(
                at,
                "a lone surrogate (\\uD800–\\uDFFF without its pair) cannot be read",
            ));
        }
        char::from_u32(first).ok_or_else(|| self.err(at, "an invalid \\u escape"))
    }

    /// `\u` 之后的一个码点（`XXXX` 或 `{X…}`），不管代理
    fn code_unit(&mut self, at: usize) -> Result<u32, NotData> {
        if self.eat('{') {
            let start = self.pos;
            while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                self.bump();
            }
            let digits = self.src[start..self.pos].trim_start_matches('0');
            let closed = self.pos > start && self.eat('}');
            if !closed {
                return Err(self.err(at, "\\u{…} needs hexadecimal digits and a closing }"));
            }
            return match digits {
                "" => Ok(0),
                d if d.len() <= 6 => u32::from_str_radix(d, 16)
                    .ok()
                    .filter(|v| *v <= 0x10ffff)
                    .ok_or_else(|| self.err(at, "\\u{…} is beyond U+10FFFF")),
                _ => Err(self.err(at, "\\u{…} is beyond U+10FFFF")),
            };
        }
        self.hex_digits(4)
            .ok_or_else(|| self.err(at, "\\u has to be followed by four hexadecimal digits"))
    }

    /// 一串数字（`radix` 进制），可以用 `_` 隔开：`_` 只能夹在两个数字中间。返回去掉 `_` 的那串
    fn digits(&mut self, radix: u32, at: usize) -> Result<String, NotData> {
        let mut out = String::new();
        let mut last_sep = false;
        while let Some(c) = self.peek() {
            if c.is_digit(radix) {
                out.push(c);
                last_sep = false;
            } else if c == '_' {
                if out.is_empty() || last_sep {
                    return Err(self.err(at, "a `_` in a number has to sit between two digits"));
                }
                last_sep = true;
            } else {
                break;
            }
            self.bump();
        }
        if last_sep {
            return Err(self.err(at, "a `_` in a number has to sit between two digits"));
        }
        Ok(out)
    }

    /// 一个数字字面量的值（负号在外面处理）
    fn number(&mut self) -> Result<f64, NotData> {
        let at = self.pos;
        let prefix = self.rest().as_bytes();
        let radix = match prefix {
            [b'0', b'x' | b'X', ..] => Some(16),
            [b'0', b'o' | b'O', ..] => Some(8),
            [b'0', b'b' | b'B', ..] => Some(2),
            _ => None,
        };
        let value = if let Some(radix) = radix {
            self.pos += 2;
            let digits = self.digits(radix, at)?;
            if digits.is_empty() {
                return Err(self.err(at, "a number is missing its digits"));
            }
            u128::from_str_radix(&digits, radix)
                .map(|n| n as f64)
                .map_err(|_| self.err(at, "the number is too large"))?
        } else {
            let int = self.digits(10, at)?;
            if int.len() > 1 && int.starts_with('0') {
                return Err(self.err(
                    at,
                    "a number cannot start with 0 (old-style octal is not allowed)",
                ));
            }
            let mut text = int;
            if self.peek() == Some('.') {
                self.bump();
                text.push('.');
                text.push_str(&self.digits(10, at)?);
            }
            if matches!(self.peek(), Some('e' | 'E')) {
                self.bump();
                text.push('e');
                if let Some(sign @ ('+' | '-')) = self.peek() {
                    self.bump();
                    text.push(sign);
                }
                let exp = self.digits(10, at)?;
                if exp.is_empty() {
                    return Err(self.err(at, "an exponent needs digits"));
                }
                text.push_str(&exp);
            }
            if text == "." || text.is_empty() {
                return Err(self.err(at, "a number is missing its digits"));
            }
            text.parse::<f64>()
                .map_err(|_| self.err(at, "this is not a number"))?
        };
        match self.peek() {
            Some('n') => return Err(self.err(at, "a BigInt (…n) is not data")),
            Some(c) if is_ident_part(c) || c == '\\' => {
                return Err(self.err(at, "a number cannot be followed directly by a name"));
            }
            _ => {}
        }
        if !value.is_finite() {
            return Err(self.err(at, "the number is too large"));
        }
        Ok(value)
    }
}

// ── 写 ───────────────────────────────────────────────────────────

/// 按固定的样子写一个值（见模块说明）。最外面那一层（manifest 本身）总是一项一行；`indent`
/// 是它开头那一行的缩进，`newline` 是换行符（`\n` 或 `\r\n`）
pub fn write(data: &Data, indent: &str, newline: &str) -> String {
    let mut w = Writer {
        out: String::new(),
        newline,
    };
    w.value(data, indent, 0, true);
    w.out
}

struct Writer<'a> {
    out: String,
    newline: &'a str,
}

impl Writer<'_> {
    /// 从第 `column` 列开始写 `v`（列按字符数，含缩进）
    fn value(&mut self, v: &Data, indent: &str, column: usize, top: bool) {
        let open = match v {
            Data::Object(m) if !m.is_empty() => '{',
            Data::Array(a) if !a.is_empty() => '[',
            _ => {
                self.out.push_str(&inline(v));
                return;
            }
        };
        if !top {
            let one_line = inline(v);
            // 后面还有一个逗号
            if column + one_line.chars().count() < WIDTH {
                self.out.push_str(&one_line);
                return;
            }
        }
        let inner = format!("{indent}  ");
        let inner_cols = inner.chars().count();
        self.out.push(open);
        self.out.push_str(self.newline);
        match v {
            Data::Object(m) => {
                for (k, item) in m {
                    let k = key_text(k);
                    self.out.push_str(&inner);
                    self.out.push_str(&k);
                    self.out.push_str(": ");
                    self.value(item, &inner, inner_cols + k.chars().count() + 2, false);
                    self.out.push(',');
                    self.out.push_str(self.newline);
                }
            }
            Data::Array(a) => {
                for item in a {
                    self.out.push_str(&inner);
                    self.value(item, &inner, inner_cols, false);
                    self.out.push(',');
                    self.out.push_str(self.newline);
                }
            }
            _ => {}
        }
        self.out.push_str(indent);
        self.out.push(if open == '{' { '}' } else { ']' });
    }
}

/// 写成一行的样子：`{ a: 1, b: [2, 3] }`
fn inline(v: &Data) -> String {
    match v {
        Data::Null => "null".into(),
        Data::Bool(b) => b.to_string(),
        Data::Number(x) => js_number(*x),
        Data::String(s) => quote(s),
        Data::Array(a) if a.is_empty() => "[]".into(),
        Data::Array(a) => format!("[{}]", a.iter().map(inline).collect::<Vec<_>>().join(", ")),
        Data::Object(m) if m.is_empty() => "{}".into(),
        Data::Object(m) => format!(
            "{{ {} }}",
            m.iter()
                .map(|(k, v)| format!("{}: {}", key_text(k), inline(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// 键：ASCII 的名字不加引号，别的加双引号
fn key_text(k: &str) -> String {
    let mut chars = k.chars();
    let plain = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if plain { k.to_string() } else { quote(k) }
}

/// 双引号的字符串。转义的只有非转不可的那些：引号、反斜杠、控制字符、U+2028/2029
fn quote(s: &str) -> String {
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
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\u{2028}' | '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 数字照 JavaScript 的 `Number.prototype.toString` 写：最短的、读回来不变的那串数字，
/// 1e21 起和 1e-7 以下写成指数
fn js_number(x: f64) -> String {
    if x == 0.0 {
        // -0 也写成 0，和 JSON.stringify 一样
        return "0".into();
    }
    let (sign, x) = if x < 0.0 { ("-", -x) } else { ("", x) };
    let sci = format!("{x:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let e: i32 = exp.parse().unwrap_or(0);
    let k = digits.len() as i32;
    let n = e + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let m = if k == 1 {
            digits.clone()
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        format!("{m}e{}{}", if e < 0 { "-" } else { "+" }, e.abs())
    };
    format!("{sign}{body}")
}

/// 把源码里的字面量换成 `data` 写出来的样子，**别的字节一个不动**。缩进照字面量开头那一行，
/// 换行符照文件里第一个换行
pub fn replace(source: &str, lit: &Literal, data: &Data) -> String {
    let start = lit.span.start;
    let line_start = source[..start].rfind(['\n', '\r']).map_or(0, |i| i + 1);
    let indent: String = source[line_start..start]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    let newline = match source.find('\n') {
        Some(i) if i > 0 && source.as_bytes()[i - 1] == b'\r' => "\r\n",
        _ => "\n",
    };
    let mut out = String::with_capacity(source.len() + 64);
    out.push_str(&source[..start]);
    out.push_str(&write(data, &indent, newline));
    out.push_str(&source[lit.span.end..]);
    out
}

// ── 改数据 ───────────────────────────────────────────────────────

/// manifest 字段的先后。插入一个原来没写的字段时，放在它前面那几个里最后一个的后面
const FIELD_ORDER: &[&str] = &[
    "name",
    "api",
    "description",
    "permissions",
    "requests",
    "match",
    "on_error",
    "reply",
    "settings",
];

fn before(order: &'static [&'static str], key: &str) -> &'static [&'static str] {
    let at = order.iter().position(|k| *k == key).unwrap_or(order.len());
    &order[..at]
}

const SCOPE_LISTS: &[&str] = &["clients", "models", "upstreams"];
/// 设置项里 `value` 前面的那几个字段
const BEFORE_VALUE: &[&str] = &["type", "label"];

/// manifest 里**改了不算改代码**的那几样的值：出错时怎么办（`on_error`）、范围（`match`）、
/// 每个设置的值（`settings.<键>.value`）。别的字段（名字、权限、设置项的类型和标签……）改了
/// 就是改代码
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Values {
    pub on_error: OnError,
    pub scope: Scope,
    /// 设置的键 → 值。读出来时按 manifest 里的先后、每个声明了的设置一项；改写时只改提到的
    pub settings: Vec<(String, Value)>,
}

/// 改写不了：manifest 读不出来、设置对不上，或者改出来的东西不合规矩。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RewriteError {
    #[error("{0}")]
    NotData(NotData),
    #[error("the plugin does not declare a setting `{0}`")]
    UnknownSetting(String),
    #[error("setting `{key}` has to be a {}", kind.as_str())]
    SettingType { key: String, kind: SettingKind },
    /// 别的不合规矩（范围里的模式太长、设置的值太长、manifest 本身写得不对……），原话
    #[error("{0}")]
    Invalid(String),
}

impl Literal {
    /// 此刻写在里面的那几样（[`Values`]）。没写 `value` 的设置是它类型的空值（`""`、`0`、
    /// `false`）。写得不对（`on_error` 不是那两个词、范围不是字符串列表、值和类型对不上）的
    /// 是 None —— 那样的 manifest 本来就加载不了
    pub fn values(&self) -> Option<Values> {
        let on_error = match self.data.get("on_error") {
            None | Some(Data::Null) => OnError::Reject,
            Some(Data::String(s)) => OnError::from_manifest(s)?,
            Some(_) => return None,
        };
        let scope = match self.data.get("match") {
            None | Some(Data::Null) => Scope::default(),
            Some(m @ Data::Object(_)) => {
                let list = |k: &str| -> Option<Vec<String>> {
                    match m.get(k) {
                        None | Some(Data::Null) => Some(Vec::new()),
                        Some(Data::Array(a)) => a
                            .iter()
                            .map(|x| match x {
                                Data::String(s) => Some(s.clone()),
                                _ => None,
                            })
                            .collect(),
                        Some(_) => None,
                    }
                };
                Scope {
                    clients: list("clients")?,
                    models: list("models")?,
                    upstreams: list("upstreams")?,
                }
            }
            Some(_) => return None,
        };
        let settings = match self.data.get("settings") {
            None | Some(Data::Null) => Vec::new(),
            Some(Data::Object(m)) => m
                .iter()
                .map(|(k, spec)| {
                    let kind = setting_kind(spec)?;
                    let v = match spec.get("value") {
                        None | Some(Data::Null) => empty_value(kind),
                        Some(d) if fits(kind, d) => d.to_json(),
                        Some(_) => return None,
                    };
                    Some((k.clone(), v))
                })
                .collect::<Option<_>>()?,
            Some(_) => return None,
        };
        Some(Values {
            on_error,
            scope,
            settings,
        })
    }

    /// 去掉能改的那几样之后剩下的：两份源码这一部分一样、字面量以外的字节也一样，就是只改了
    /// 数据（[`same_code`]）
    fn code(&self) -> Data {
        let mut d = self.data.clone();
        d.remove("on_error");
        d.remove("match");
        if let Some(Data::Object(settings)) = d.get_mut("settings") {
            for (_, spec) in settings.iter_mut() {
                spec.remove("value");
            }
        }
        d
    }
}

fn setting_kind(spec: &Data) -> Option<SettingKind> {
    match spec.get("type") {
        Some(Data::String(t)) if t == "string" => Some(SettingKind::String),
        Some(Data::String(t)) if t == "number" => Some(SettingKind::Number),
        Some(Data::String(t)) if t == "boolean" => Some(SettingKind::Boolean),
        _ => None,
    }
}

fn empty_value(kind: SettingKind) -> Value {
    match kind {
        SettingKind::String => Value::String(String::new()),
        SettingKind::Number => Value::from(0),
        SettingKind::Boolean => Value::Bool(false),
    }
}

fn fits(kind: SettingKind, d: &Data) -> bool {
    matches!(
        (kind, d),
        (SettingKind::String, Data::String(_))
            | (SettingKind::Number, Data::Number(_))
            | (SettingKind::Boolean, Data::Bool(_))
    )
}

/// 两份源码是不是只差数据：字面量以外的字节一模一样，manifest 里除了 `on_error`、`match`
/// 和设置的 `value` 也一模一样（名字、权限、设置项的键、类型、标签、先后……）。哪一份读不出
/// manifest 都不算
pub fn same_code(a: &str, b: &str) -> bool {
    let (Ok(x), Ok(y)) = (find(a), find(b)) else {
        return false;
    };
    a[..x.span.start] == b[..y.span.start]
        && a[x.span.end..] == b[y.span.end..]
        && x.code() == y.code()
}

/// 改插件文件里能改的那几样：**只换 manifest 字面量那一段字节**，别的一个不动。
///
/// `on_error` 和 `scope` 是改完的样子；`settings` 只改提到的那几个，没提到的照旧。原来没写
/// 的字段，改成的值就是不写时的值（出错时拒绝、范围空着、设置是空值）就照旧不写。改完和原来
/// 一样就原样返回 —— 字面量里的注释和排版都还在。
pub fn rewrite(source: &str, values: &Values) -> Result<String, RewriteError> {
    let lit = find(source).map_err(RewriteError::NotData)?;
    let mut data = lit.data.clone();

    // 出错时怎么办
    let on_error = Data::String(values.on_error.as_str().into());
    if data.get("on_error").is_some() || values.on_error != OnError::Reject {
        data.set("on_error", on_error, before(FIELD_ORDER, "on_error"));
    }

    // 范围：和加载时同一套规矩
    let lists = [
        ("clients", &values.scope.clients),
        ("models", &values.scope.models),
        ("upstreams", &values.scope.upstreams),
    ];
    let as_json = serde_json::json!({
        "clients": values.scope.clients,
        "models": values.scope.models,
        "upstreams": values.scope.upstreams,
    });
    crate::manifest::check_scope(&as_json).map_err(RewriteError::Invalid)?;
    let strings = |l: &[String]| Data::Array(l.iter().cloned().map(Data::String).collect());
    if let Some(m @ Data::Object(_)) = data.get_mut("match") {
        for (i, (name, list)) in lists.iter().enumerate() {
            if m.get(name).is_some() || !list.is_empty() {
                m.set(name, strings(list), &SCOPE_LISTS[..i]);
            }
        }
    } else if lists.iter().any(|(_, l)| !l.is_empty()) {
        // 原来没写（或者写的是 null）：三张单子都写出来，空的也写，看得出还能填什么
        let full = Data::Object(
            lists
                .iter()
                .map(|(name, list)| (name.to_string(), strings(list)))
                .collect(),
        );
        data.set("match", full, before(FIELD_ORDER, "match"));
    }

    // 设置的值
    for (key, value) in &values.settings {
        let Some(spec) = data.get_mut("settings").and_then(|s| s.get_mut(key)) else {
            return Err(RewriteError::UnknownSetting(key.clone()));
        };
        let Some(kind) = setting_kind(spec) else {
            return Err(RewriteError::Invalid(format!(
                "setting `{key}` has no valid type"
            )));
        };
        let new = Data::from_json(value)
            .filter(|d| fits(kind, d))
            .ok_or_else(|| RewriteError::SettingType {
                key: key.clone(),
                kind,
            })?;
        if let Data::String(s) = &new
            && s.chars().count() > crate::manifest::MAX_STRING_VALUE
        {
            return Err(RewriteError::Invalid(format!(
                "the value of setting `{key}` is too long"
            )));
        }
        let empty = Data::from_json(&empty_value(kind));
        if spec.get("value").is_some() || Some(&new) != empty.as_ref() {
            spec.set("value", new, BEFORE_VALUE);
        }
    }

    if data == lit.data {
        return Ok(source.to_string());
    }
    let out = replace(source, &lit, &data);
    // 写出去的东西读回来得正好是这一份，别的字节一个没动
    match find(&out) {
        Ok(back)
            if back.data == data
                && out[..back.span.start] == source[..lit.span.start]
                && out[back.span.end..] == source[lit.span.end..] => {}
        _ => {
            return Err(RewriteError::Invalid(
                "the rewritten manifest does not read back as written".into(),
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
