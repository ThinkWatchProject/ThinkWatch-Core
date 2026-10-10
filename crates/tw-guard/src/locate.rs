//! 一处命中在请求里的位置：哪一部分（系统提示、消息、工具结果、工具调用）、第几条消息、
//! 谁说的、哪个工具，和它在请求体里的 JSON 路径。
//!
//! 安全日志的一条记录要说得出命中在哪儿：一个请求几百条消息，光说「这个请求里有一把
//! 密钥」，人得自己去一条条翻。两个网关共用这几样：
//!
//! - 按客户端的格式读请求体，说出一个位置是哪一部分（[`place`]）；
//! - 在 JSON 原文上走一遍，说出一个字节落在哪个字符串里、路径是什么（[`walk`]）——
//!   出站脱敏是在原文上找的，找到的是原文里的字节区间；
//! - 字符串原文（转义没解开）和解开之后的样子之间换算位置（[`Literal`]）；
//! - 路径写成人读得懂的样子（[`path_text`]）：`messages[3].content[0].text`。
//!
//! **路径是客户端发来的那一份里的**：按客户端的格式，转换成别的格式之前。

use std::ops::Range;

use serde_json::Value;
pub use tw_dialect::caller::Step;
use tw_dialect::ir::Dialect;

/// 命中在请求（或回答）的哪一部分。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "HitPart"))]
#[serde(rename_all = "snake_case")]
pub enum Part {
    /// 系统提示（Chat、Responses 里 `system` / `developer` 角色的消息也是），和消息之外的
    /// 字段：工具定义、元数据……
    System,
    /// 一条消息里的文字：调用方说的，或者模型以前的回答
    Message,
    /// 工具结果：工具抓回来的网页、读到的文件
    ToolResult,
    /// 工具调用：要调哪个工具、参数是什么
    ToolCall,
    /// 回答的正文
    ResponseText,
}

words!(Part {
    System = "system",
    Message = "message",
    ToolResult = "tool_result",
    ToolCall = "tool_call",
    ResponseText = "response_text",
});

/// 命中在调用方发来的请求里，还是在交给调用方的回答里。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "SecurityDirection"))]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// 出站脱敏、内容过滤：查的是请求
    Request,
    /// 工具调用审查：查的是回答
    Response,
}

words!(Direction {
    Request = "request",
    Response = "response",
});

/// 一个位置在请求的哪一部分（见 [`place`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub part: Part,
    /// 在客户端的消息数组（`messages`、`input`、`contents`）里的下标。消息之外的（`system`
    /// 字段、工具定义）没有
    pub message_index: Option<u32>,
    /// 那条消息的角色，客户端写的原样（`user`、`assistant`、`tool`、`developer`、`model`……）。
    /// 没写的没有
    pub role: Option<String>,
    /// 工具调用、工具结果是哪个工具。工具结果按它对上的那个调用找名字，找不到的没有
    pub tool: Option<String>,
}

impl Place {
    pub fn of(part: Part) -> Self {
        Place {
            part,
            message_index: None,
            role: None,
            tool: None,
        }
    }

    /// 消息数组里第 `i` 条，角色是 `role`。`system`、`developer` 角色的算系统提示
    fn message(i: usize, role: Option<&str>) -> Self {
        let part = match role {
            Some("system" | "developer") => Part::System,
            _ => Part::Message,
        };
        Place {
            part,
            message_index: u32::try_from(i).ok(),
            role: role.map(str::to_string),
            tool: None,
        }
    }
}

/// 按客户端的格式读：`body` 里 `path` 指的那个位置在请求的哪一部分。
///
/// `dialect` 是 `None`（认不出是哪种格式的请求：嵌入、旧版补全……）时一律算消息，只有路径
/// 说得出在哪儿。消息数组之外的字段（工具定义、元数据、`model`）算系统提示：它们是配置
/// 请求的那一方写的，不是哪一条消息。
pub fn place(dialect: Option<Dialect>, body: &Value, path: &[Step]) -> Place {
    match dialect {
        Some(Dialect::Anthropic) => anthropic(body, path),
        Some(Dialect::Chat) => chat(body, path),
        Some(Dialect::Responses) => responses(body, path),
        Some(Dialect::Gemini) => gemini(body, path),
        Some(Dialect::Bedrock) => bedrock(body, path),
        None => Place::of(Part::Message),
    }
}

fn key_at(path: &[Step], i: usize) -> Option<&str> {
    match path.get(i) {
        Some(Step::Key(k)) => Some(k),
        _ => None,
    }
}

fn index_at(path: &[Step], i: usize) -> Option<usize> {
    match path.get(i) {
        Some(Step::Index(n)) => Some(*n),
        _ => None,
    }
}

fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn items<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Anthropic：`system` 字段；`messages[i].content[j]` 里 `tool_use` 是调用、`tool_result`
/// 是结果（按 `tool_use_id` 找调用的名字）
fn anthropic(body: &Value, path: &[Step]) -> Place {
    let (Some("messages"), Some(i)) = (key_at(path, 0), index_at(path, 1)) else {
        return Place::of(Part::System);
    };
    let m = &body["messages"][i];
    let mut p = Place::message(i, str_of(m, "role"));
    if key_at(path, 2) == Some("content")
        && let Some(j) = index_at(path, 3)
    {
        let b = &m["content"][j];
        match str_of(b, "type") {
            Some("tool_use" | "server_tool_use") => {
                p.part = Part::ToolCall;
                p.tool = str_of(b, "name").map(str::to_string);
            }
            Some("tool_result") => {
                p.part = Part::ToolResult;
                p.tool = str_of(b, "tool_use_id").and_then(|id| {
                    items(body, "messages")
                        .iter()
                        .flat_map(|m| items(m, "content"))
                        .find(|b| {
                            matches!(str_of(b, "type"), Some("tool_use" | "server_tool_use"))
                                && str_of(b, "id") == Some(id)
                        })
                        .and_then(|b| str_of(b, "name"))
                        .map(str::to_string)
                });
            }
            _ => {}
        }
    }
    p
}

/// OpenAI Chat：`messages[i]`，`tool` 角色是工具结果（按 `tool_call_id` 找名字），
/// `tool_calls[k]` 是调用
fn chat(body: &Value, path: &[Step]) -> Place {
    let (Some("messages"), Some(i)) = (key_at(path, 0), index_at(path, 1)) else {
        return Place::of(Part::System);
    };
    let m = &body["messages"][i];
    let role = str_of(m, "role");
    let mut p = Place::message(i, role);
    match (role, key_at(path, 2)) {
        (_, Some("tool_calls")) => {
            p.part = Part::ToolCall;
            p.tool = index_at(path, 3)
                .and_then(|k| m["tool_calls"][k]["function"]["name"].as_str())
                .map(str::to_string);
        }
        (_, Some("function_call")) => {
            p.part = Part::ToolCall;
            p.tool = m["function_call"]["name"].as_str().map(str::to_string);
        }
        (Some("tool"), _) => {
            p.part = Part::ToolResult;
            p.tool = str_of(m, "tool_call_id").and_then(|id| {
                items(body, "messages")
                    .iter()
                    .flat_map(|m| items(m, "tool_calls"))
                    .find(|c| str_of(c, "id") == Some(id))
                    .and_then(|c| c["function"]["name"].as_str())
                    .map(str::to_string)
            });
        }
        // 旧写法：`function` 角色的消息带着函数名
        (Some("function"), _) => {
            p.part = Part::ToolResult;
            p.tool = str_of(m, "name").map(str::to_string);
        }
        _ => {}
    }
    p
}

/// OpenAI Responses：`instructions` 是系统提示；`input` 是一个字符串时整个是一条消息，
/// 是数组时按每一项的 `type` 认（不写是 `message`）
fn responses(body: &Value, path: &[Step]) -> Place {
    match key_at(path, 0) {
        Some("input") => {}
        _ => return Place::of(Part::System),
    }
    let Some(i) = index_at(path, 1) else {
        return Place::of(Part::Message);
    };
    let item = &body["input"][i];
    let mut p = Place::message(i, str_of(item, "role"));
    match str_of(item, "type").unwrap_or("message") {
        "function_call" | "custom_tool_call" => {
            p.part = Part::ToolCall;
            p.tool = str_of(item, "name").map(str::to_string);
        }
        "function_call_output" | "custom_tool_call_output" => {
            p.part = Part::ToolResult;
            p.tool = str_of(item, "call_id").and_then(|id| {
                items(body, "input")
                    .iter()
                    .find(|c| {
                        matches!(
                            str_of(c, "type"),
                            Some("function_call" | "custom_tool_call")
                        ) && str_of(c, "call_id") == Some(id)
                    })
                    .and_then(|c| str_of(c, "name"))
                    .map(str::to_string)
            });
        }
        _ => {}
    }
    p
}

/// Gemini：`systemInstruction`（或 `system_instruction`）是系统提示；`contents[i].parts[k]`
/// 里 `functionCall` 是调用、`functionResponse` 是结果，名字都写在里面
fn gemini(body: &Value, path: &[Step]) -> Place {
    let (Some("contents"), Some(i)) = (key_at(path, 0), index_at(path, 1)) else {
        return Place::of(Part::System);
    };
    let c = &body["contents"][i];
    let mut p = Place::message(i, str_of(c, "role"));
    if key_at(path, 2) == Some("parts")
        && let Some(k) = index_at(path, 3)
    {
        let part = &c["parts"][k];
        let named = |key: &str| part[key]["name"].as_str().map(str::to_string);
        match key_at(path, 4) {
            Some(k @ ("functionCall" | "function_call")) => {
                p.part = Part::ToolCall;
                p.tool = named(k);
            }
            Some(k @ ("functionResponse" | "function_response")) => {
                p.part = Part::ToolResult;
                p.tool = named(k);
            }
            _ => {}
        }
    }
    p
}

/// Bedrock Converse：`system` 是系统提示；`messages[i].content[j]` 里 `toolUse` 是调用、
/// `toolResult` 是结果（按 `toolUseId` 找名字）
fn bedrock(body: &Value, path: &[Step]) -> Place {
    let (Some("messages"), Some(i)) = (key_at(path, 0), index_at(path, 1)) else {
        return Place::of(Part::System);
    };
    let m = &body["messages"][i];
    let mut p = Place::message(i, str_of(m, "role"));
    if key_at(path, 2) == Some("content") && index_at(path, 3).is_some() {
        match key_at(path, 4) {
            Some("toolUse") => {
                p.part = Part::ToolCall;
                p.tool = index_at(path, 3)
                    .and_then(|j| m["content"][j]["toolUse"]["name"].as_str())
                    .map(str::to_string);
            }
            Some("toolResult") => {
                p.part = Part::ToolResult;
                let id = index_at(path, 3)
                    .and_then(|j| m["content"][j]["toolResult"]["toolUseId"].as_str());
                p.tool = id.and_then(|id| {
                    items(body, "messages")
                        .iter()
                        .flat_map(|m| items(m, "content"))
                        .filter_map(|b| b.get("toolUse"))
                        .find(|u| str_of(u, "toolUseId") == Some(id))
                        .and_then(|u| str_of(u, "name"))
                        .map(str::to_string)
                });
            }
            _ => {}
        }
    }
    p
}

/// 路径写成人读得懂的样子：`messages[3].content[0].text`、`input[7].output`。
///
/// 像标识符的键（字母或下划线开头，后面是字母、数字、下划线）用点连，别的键写成
/// `["…"]`（JSON 字符串的写法）。根是空串。
pub fn path_text(path: &[Step]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for s in path {
        match s {
            Step::Key(k) if plain_key(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(k);
            }
            Step::Key(k) => {
                out.push('[');
                out.push_str(&serde_json::to_string(k).unwrap_or_default());
                out.push(']');
            }
            Step::Index(i) => {
                let _ = write!(out, "[{i}]");
            }
        }
    }
    out
}

fn plain_key(k: &str) -> bool {
    let mut b = k.bytes();
    b.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// 原文里的一个标量：字符串（连同对象的键）、数、`true`、`false`、`null`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scalar {
    /// 在原文里的字节区间。字符串是引号里面的部分，原样（转义没解开）
    pub raw: Range<usize>,
    /// 是一个字符串
    pub string: bool,
    /// 是一个对象的键。交给 [`walk`] 的回调的路径是这个键所在的成员
    pub key: bool,
}

/// 走一遍 JSON 原文，每个标量连同它的路径交给 `f`，按在原文里的先后。
///
/// **只扫一遍、不建树**：请求体可以有几十 MB，为找几处命中的路径建一棵树不值得。不是
/// 合法 JSON 的原文走到认不出的地方为止。
pub fn walk(text: &str, mut f: impl FnMut(&[Step], &Scalar)) {
    #[derive(PartialEq)]
    enum Want {
        Value,
        /// `[` 之后：一个值，或者 `]`
        ValueOrClose,
        /// `{` 之后：一个键，或者 `}`
        KeyOrClose,
        /// `,` 之后的键
        Key,
        Colon,
        /// 一个值之后：`,`，或者收尾的括号
        Next,
    }
    enum Frame {
        Object,
        Array(usize),
    }
    let b = text.as_bytes();
    let mut path: Vec<Step> = Vec::new();
    let mut frames: Vec<Frame> = Vec::new();
    let mut want = Want::Value;
    let mut i = 0;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let Some(&c) = b.get(i) else { break };
        match want {
            Want::Value | Want::ValueOrClose => {
                if want == Want::ValueOrClose && c == b']' {
                    frames.pop();
                    path.pop();
                    want = Want::Next;
                    i += 1;
                    continue;
                }
                match c {
                    b'{' => {
                        frames.push(Frame::Object);
                        want = Want::KeyOrClose;
                        i += 1;
                    }
                    b'[' => {
                        frames.push(Frame::Array(0));
                        path.push(Step::Index(0));
                        want = Want::ValueOrClose;
                        i += 1;
                    }
                    b'"' => {
                        let Some(end) = string_end(b, i + 1) else {
                            break;
                        };
                        f(
                            &path,
                            &Scalar {
                                raw: i + 1..end,
                                string: true,
                                key: false,
                            },
                        );
                        i = end + 1;
                        want = Want::Next;
                    }
                    _ => {
                        let end = b[i..]
                            .iter()
                            .position(|c| {
                                c.is_ascii_whitespace() || matches!(c, b',' | b']' | b'}' | b':')
                            })
                            .map_or(b.len(), |n| i + n);
                        if end == i {
                            break;
                        }
                        f(
                            &path,
                            &Scalar {
                                raw: i..end,
                                string: false,
                                key: false,
                            },
                        );
                        i = end;
                        want = Want::Next;
                    }
                }
            }
            Want::KeyOrClose | Want::Key => {
                if want == Want::KeyOrClose && c == b'}' {
                    frames.pop();
                    want = Want::Next;
                    i += 1;
                    continue;
                }
                if c != b'"' {
                    break;
                }
                let Some(end) = string_end(b, i + 1) else {
                    break;
                };
                path.push(Step::Key(unescape(&text[i + 1..end])));
                f(
                    &path,
                    &Scalar {
                        raw: i + 1..end,
                        string: true,
                        key: true,
                    },
                );
                i = end + 1;
                want = Want::Colon;
            }
            Want::Colon => {
                if c != b':' {
                    break;
                }
                i += 1;
                want = Want::Value;
            }
            Want::Next => match frames.last_mut() {
                None => break,
                Some(Frame::Object) => {
                    // 这个成员完了：键出栈
                    path.pop();
                    match c {
                        b',' => want = Want::Key,
                        b'}' => {
                            frames.pop();
                            want = Want::Next;
                        }
                        _ => break,
                    }
                    i += 1;
                }
                Some(Frame::Array(n)) => {
                    match c {
                        b',' => {
                            *n += 1;
                            let n = *n;
                            if let Some(last) = path.last_mut() {
                                *last = Step::Index(n);
                            }
                            want = Want::Value;
                        }
                        b']' => {
                            frames.pop();
                            path.pop();
                            want = Want::Next;
                        }
                        _ => break,
                    }
                    i += 1;
                }
            },
        }
    }
}

/// 从 `start`（开引号之后）起，这个字符串的闭引号在哪儿。没闭上是 `None`
fn string_end(b: &[u8], start: usize) -> Option<usize> {
    let mut j = start;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return Some(j),
            _ => j += 1,
        }
    }
    None
}

/// 一组字节位置各落在原文里哪个标量上（见 [`walk`]）：每个位置是它所在的那个标量和它的
/// 路径，不在任何标量里的（落在括号、逗号上，或者原文认不出）是 `None`。
pub fn scalars_at(text: &str, offsets: &[usize]) -> Vec<Option<(Vec<Step>, Scalar)>> {
    let mut order: Vec<usize> = (0..offsets.len()).collect();
    order.sort_by_key(|&k| offsets[k]);
    let mut out: Vec<Option<(Vec<Step>, Scalar)>> = vec![None; offsets.len()];
    let mut next = 0;
    walk(text, |path, s| {
        // 落在这个标量之前的那几个不在任何标量里
        while next < order.len() && offsets[order[next]] < s.raw.start {
            next += 1;
        }
        while next < order.len() && offsets[order[next]] < s.raw.end {
            out[order[next]] = Some((path.to_vec(), s.clone()));
            next += 1;
        }
    });
    out
}

/// 一个字符串字面量的原文（引号里面，转义没解开）：里面每个转义序列在哪儿，用来和解开
/// 之后的样子换算位置。
#[derive(Debug, Clone)]
pub struct Literal {
    /// 在整段原文里的区间
    pub range: Range<usize>,
    /// 每个转义序列：在整段原文里的起点、原文几个字节、在解开之后的样子里的起点（相对
    /// 字面量开头）、解开之后几个字节
    escapes: Vec<Escape>,
}

#[derive(Debug, Clone, Copy)]
struct Escape {
    raw: usize,
    raw_len: usize,
    decoded: usize,
    decoded_len: usize,
}

impl Literal {
    /// `text[range]` 是一个字符串字面量引号里面的部分
    pub fn new(text: &str, range: Range<usize>) -> Self {
        let b = text.as_bytes();
        let mut escapes = Vec::new();
        let mut shift = 0usize;
        let mut i = range.start;
        while i < range.end {
            if b[i] != b'\\' {
                i += 1;
                continue;
            }
            let (raw_len, decoded_len) = escape_len(&text[i..range.end]);
            escapes.push(Escape {
                raw: i,
                raw_len,
                decoded: i - range.start - shift,
                decoded_len,
            });
            shift += raw_len - decoded_len;
            i += raw_len;
        }
        Literal { range, escapes }
    }

    /// 解开之后的样子里的字节位置（相对字面量开头），在原文里是哪儿。落在一个转义解出来
    /// 的字符中间的，算这个转义的开头
    pub fn raw_at(&self, decoded: usize) -> usize {
        let k = self.escapes.partition_point(|e| e.decoded < decoded);
        let Some(prev) = k.checked_sub(1).map(|k| self.escapes[k]) else {
            return self.range.start + decoded;
        };
        if decoded < prev.decoded + prev.decoded_len {
            return prev.raw;
        }
        prev.raw + prev.raw_len + (decoded - prev.decoded - prev.decoded_len)
    }

    /// 原文里的位置挪到一个转义序列的边界上：落在一个转义中间的挪到它后面（`forward`）
    /// 或者前面
    pub fn snap(&self, pos: usize, forward: bool) -> usize {
        let k = self.escapes.partition_point(|e| e.raw < pos);
        match k.checked_sub(1).map(|k| self.escapes[k]) {
            Some(e) if pos < e.raw + e.raw_len => {
                if forward {
                    e.raw + e.raw_len
                } else {
                    e.raw
                }
            }
            _ => pos,
        }
    }
}

/// `s` 以反斜杠开头：这个转义序列原文几个字节、解开之后几个字节
fn escape_len(s: &str) -> (usize, usize) {
    let b = s.as_bytes();
    match b.get(1) {
        None => (1, 1),
        Some(b'u') => {
            let unit = |at: usize| -> Option<u32> {
                let hex = s.get(at + 2..at + 6)?;
                if s.as_bytes().get(at) != Some(&b'\\')
                    || s.as_bytes().get(at + 1) != Some(&b'u')
                    || !hex.bytes().all(|c| c.is_ascii_hexdigit())
                {
                    return None;
                }
                u32::from_str_radix(hex, 16).ok()
            };
            match unit(0) {
                None => (2, 2),
                Some(0xD800..=0xDBFF) => match unit(6) {
                    Some(0xDC00..=0xDFFF) => (12, 4),
                    // 半个代理对：解开是一个替换字符
                    _ => (6, '\u{FFFD}'.len_utf8()),
                },
                Some(0xDC00..=0xDFFF) => (6, '\u{FFFD}'.len_utf8()),
                Some(c) => (6, char::from_u32(c).map_or(3, char::len_utf8)),
            }
        }
        Some(_) => {
            // 反斜杠后面那个字符：`\n` 这种两个字节解成一个；认不出的照字面留着
            let c = s[1..].chars().next().map_or(1, char::len_utf8);
            match b[1] {
                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => (2, 1),
                _ => (1 + c, 1 + c),
            }
        }
    }
}

/// 字符串原文（引号里面）解开。**宽容**：认不出的转义照字面留着，半个代理对换成替换
/// 字符 —— 给人看的一小段，宁可留着几个怪字，也不整段丢掉。
pub fn unescape(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let rest = &raw[i..];
        if !rest.starts_with('\\') {
            let next = rest.find('\\').unwrap_or(rest.len());
            out.push_str(&rest[..next]);
            i += next;
            continue;
        }
        let (raw_len, _) = escape_len(rest);
        let esc = &rest[..raw_len];
        match esc.as_bytes().get(1) {
            Some(b'u') if raw_len >= 6 => {
                let unit = |at: usize| u32::from_str_radix(&esc[at + 2..at + 6], 16).ok();
                let c = match (unit(0), raw_len) {
                    (Some(hi), 12) => unit(6).and_then(|lo| {
                        char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))
                    }),
                    (Some(c), _) => char::from_u32(c),
                    _ => None,
                };
                out.push(c.unwrap_or('\u{FFFD}'));
            }
            Some(b'"') => out.push('"'),
            Some(b'\\') => out.push('\\'),
            Some(b'/') => out.push('/'),
            Some(b'b') => out.push('\u{8}'),
            Some(b'f') => out.push('\u{c}'),
            Some(b'n') => out.push('\n'),
            Some(b'r') => out.push('\r'),
            Some(b't') => out.push('\t'),
            _ => out.push_str(esc),
        }
        i += raw_len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn steps(path: &[&str]) -> Vec<Step> {
        path.iter()
            .map(|s| match s.parse::<usize>() {
                Ok(n) => Step::Index(n),
                Err(_) => Step::Key(s.to_string()),
            })
            .collect()
    }

    #[test]
    fn paths_read_like_the_body() {
        assert_eq!(
            path_text(&steps(&["messages", "3", "content", "0", "text"])),
            "messages[3].content[0].text"
        );
        assert_eq!(
            path_text(&steps(&["input", "7", "output"])),
            "input[7].output"
        );
        assert_eq!(
            path_text(&[Step::Key("a.b".into()), Step::Key("x-api-key".into())]),
            r#"["a.b"]["x-api-key"]"#
        );
        assert_eq!(path_text(&[]), "");
    }

    /// 走一遍原文：每个字符串的路径和引号里面的区间，键、数、嵌套、转义、空的容器都对
    #[test]
    fn the_walk_names_every_string_where_it_is() {
        let text = r#"{"a":[1,{"b":"x\"y"},[],{}],"c\"d":true,"e":["",null]}"#;
        let mut got = Vec::new();
        walk(text, |path, s| {
            got.push((path_text(path), text[s.raw.clone()].to_string(), s.key));
        });
        assert_eq!(
            got,
            [
                ("a", "a", true),
                ("a[0]", "1", false),
                ("a[1].b", "b", true),
                ("a[1].b", r#"x\"y"#, false),
                (r#"["c\"d"]"#, r#"c\"d"#, true),
                (r#"["c\"d"]"#, "true", false),
                ("e", "e", true),
                ("e[0]", "", false),
                ("e[1]", "null", false),
            ]
            .map(|(p, s, k)| (p.to_string(), s.to_string(), k))
        );
    }

    #[test]
    fn offsets_find_the_string_they_are_in() {
        let text = r#"{"messages":[{"role":"user","content":"hi sk-x"}]}"#;
        let at = text.find("sk-x").unwrap();
        let comma = text.find(',').unwrap();
        let got = scalars_at(text, &[at, comma]);
        let (path, s) = got[0].clone().unwrap();
        assert_eq!(path_text(&path), "messages[0].content");
        assert_eq!(&text[s.raw], "hi sk-x");
        assert!(got[1].is_none());
    }

    /// 原文和解开之后的位置换算：`\n` 两个字节解成一个，`\u00e9` 六个解成两个，代理对十二个
    /// 解成四个
    #[test]
    fn a_literal_maps_decoded_offsets_back_to_the_raw_text() {
        let text = r#""a\nb\u00e9c\ud83d\ude00d""#;
        let lit = Literal::new(text, 1..text.len() - 1);
        let decoded = unescape(&text[1..text.len() - 1]);
        assert_eq!(decoded, "a\nbéc😀d");
        for (d, c) in decoded.char_indices() {
            let r = lit.raw_at(d);
            let raw_char = unescape(&text[r..lit.snap(r + 1, true)]);
            assert_eq!(raw_char.chars().next(), Some(c), "at {d}");
        }
        assert_eq!(lit.raw_at(decoded.len()), text.len() - 1);
        // 落在 `\u00e9` 中间的挪到它的边上
        let e = text.find("\\u00e9").unwrap();
        assert_eq!(lit.snap(e + 3, true), e + 6);
        assert_eq!(lit.snap(e + 3, false), e);
    }

    #[test]
    fn unescape_keeps_what_it_cannot_read() {
        assert_eq!(unescape(r#"x\qy"#), r#"x\qy"#);
        assert_eq!(unescape(r#"\ud83d!"#), "\u{FFFD}!");
        assert_eq!(unescape("plain"), "plain");
    }

    fn at(dialect: Dialect, body: &Value, path: &[&str]) -> Place {
        place(Some(dialect), body, &steps(path))
    }

    #[test]
    fn anthropic_parts_roles_and_tool_names() {
        let b = json!({
            "system": "s",
            "tools": [{"name": "bash", "description": "d"}],
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "ok"},
                    {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "files"}
                ]}
            ]
        });
        let p = at(Dialect::Anthropic, &b, &["system"]);
        assert_eq!((p.part, p.message_index), (Part::System, None));
        assert_eq!(
            at(Dialect::Anthropic, &b, &["tools", "0", "description"]).part,
            Part::System
        );
        let p = at(Dialect::Anthropic, &b, &["messages", "0", "content"]);
        assert_eq!(p.part, Part::Message);
        assert_eq!(
            (p.message_index, p.role.as_deref()),
            (Some(0), Some("user"))
        );
        let p = at(
            Dialect::Anthropic,
            &b,
            &["messages", "1", "content", "1", "input", "command"],
        );
        assert_eq!((p.part, p.tool.as_deref()), (Part::ToolCall, Some("bash")));
        assert_eq!(p.role.as_deref(), Some("assistant"));
        let p = at(
            Dialect::Anthropic,
            &b,
            &["messages", "2", "content", "0", "content"],
        );
        assert_eq!(
            (p.part, p.tool.as_deref()),
            (Part::ToolResult, Some("bash"))
        );
        assert_eq!(p.message_index, Some(2));
    }

    #[test]
    fn chat_parts_roles_and_tool_names() {
        let b = json!({"messages": [
            {"role": "system", "content": "s"},
            {"role": "assistant", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "read", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "c1", "content": "x"},
            {"role": "user", "content": [{"type": "text", "text": "y"}]}
        ]});
        let p = at(Dialect::Chat, &b, &["messages", "0", "content"]);
        assert_eq!((p.part, p.message_index), (Part::System, Some(0)));
        let p = at(
            Dialect::Chat,
            &b,
            &["messages", "1", "tool_calls", "0", "function", "arguments"],
        );
        assert_eq!((p.part, p.tool.as_deref()), (Part::ToolCall, Some("read")));
        let p = at(Dialect::Chat, &b, &["messages", "2", "content"]);
        assert_eq!(
            (p.part, p.tool.as_deref()),
            (Part::ToolResult, Some("read"))
        );
        assert_eq!(p.role.as_deref(), Some("tool"));
        let p = at(
            Dialect::Chat,
            &b,
            &["messages", "3", "content", "0", "text"],
        );
        assert_eq!((p.part, p.role.as_deref()), (Part::Message, Some("user")));
    }

    #[test]
    fn responses_parts_roles_and_tool_names() {
        let b = json!({
            "instructions": "s",
            "input": [
                {"role": "developer", "content": "d"},
                {"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "out"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "u"}]}
            ]
        });
        assert_eq!(
            at(Dialect::Responses, &b, &["instructions"]).part,
            Part::System
        );
        let p = at(Dialect::Responses, &b, &["input", "0", "content"]);
        assert_eq!(
            (p.part, p.role.as_deref()),
            (Part::System, Some("developer"))
        );
        let p = at(Dialect::Responses, &b, &["input", "1", "arguments"]);
        assert_eq!((p.part, p.tool.as_deref()), (Part::ToolCall, Some("shell")));
        let p = at(Dialect::Responses, &b, &["input", "2", "output"]);
        assert_eq!(
            (p.part, p.tool.as_deref()),
            (Part::ToolResult, Some("shell"))
        );
        assert_eq!(p.message_index, Some(2));
        let p = at(
            Dialect::Responses,
            &b,
            &["input", "3", "content", "0", "text"],
        );
        assert_eq!((p.part, p.role.as_deref()), (Part::Message, Some("user")));
        // 一个字符串的 input 整个是一条消息
        let s = json!({"input": "hello"});
        let p = at(Dialect::Responses, &s, &["input"]);
        assert_eq!((p.part, p.message_index), (Part::Message, None));
    }

    #[test]
    fn gemini_parts_roles_and_tool_names() {
        let b = json!({
            "system_instruction": {"parts": [{"text": "s"}]},
            "contents": [
                {"role": "model", "parts": [{"functionCall": {"name": "lookup", "args": {"q": "x"}}}]},
                {"role": "user", "parts": [
                    {"functionResponse": {"name": "lookup", "response": {"output": "y"}}},
                    {"text": "z"}
                ]}
            ]
        });
        assert_eq!(
            at(
                Dialect::Gemini,
                &b,
                &["system_instruction", "parts", "0", "text"]
            )
            .part,
            Part::System
        );
        let p = at(
            Dialect::Gemini,
            &b,
            &["contents", "0", "parts", "0", "functionCall", "args", "q"],
        );
        assert_eq!(
            (p.part, p.tool.as_deref()),
            (Part::ToolCall, Some("lookup"))
        );
        assert_eq!(p.role.as_deref(), Some("model"));
        let p = at(
            Dialect::Gemini,
            &b,
            &[
                "contents",
                "1",
                "parts",
                "0",
                "functionResponse",
                "response",
                "output",
            ],
        );
        assert_eq!(
            (p.part, p.tool.as_deref()),
            (Part::ToolResult, Some("lookup"))
        );
        let p = at(
            Dialect::Gemini,
            &b,
            &["contents", "1", "parts", "1", "text"],
        );
        assert_eq!((p.part, p.message_index), (Part::Message, Some(1)));
    }

    #[test]
    fn bedrock_parts_and_tool_names() {
        let b = json!({
            "system": [{"text": "s"}],
            "messages": [
                {"role": "assistant", "content": [{"toolUse": {"toolUseId": "u1", "name": "grep", "input": {}}}]},
                {"role": "user", "content": [{"toolResult": {"toolUseId": "u1", "content": [{"text": "r"}]}}]}
            ]
        });
        assert_eq!(
            at(Dialect::Bedrock, &b, &["system", "0", "text"]).part,
            Part::System
        );
        let p = at(
            Dialect::Bedrock,
            &b,
            &[
                "messages",
                "1",
                "content",
                "0",
                "toolResult",
                "content",
                "0",
                "text",
            ],
        );
        assert_eq!(
            (p.part, p.tool.as_deref()),
            (Part::ToolResult, Some("grep"))
        );
    }
}
