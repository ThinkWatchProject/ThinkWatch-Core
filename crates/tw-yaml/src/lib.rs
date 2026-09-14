//! 在**原文本**上做最小替换的 YAML 补丁层。
//!
//! **绝对不能「反序列化 → 改结构体 → 重新序列化」。**那会丢注释、重排
//! 键序、统一引号风格 —— 等于每次改一个字段就把用户的文件重写一遍。
//! 这正是 cc-switch 那批覆写 issue 的同一类错误，只不过是在别人的配置
//! 文件上犯的。
//!
//! 所以：**解析出每个节点的字节区间，只替换那一段，其余原文一个字节都
//! 不动。**注释、空行、缩进风格、引号偏好全都保住了，因为我们根本没碰
//! 它们。
//!
//! 这是全项目最危险的代码 —— 它写用户唯一的配置文件。护栏见 `patch`
//! 上的注释和整个测试模块。

use std::ops::Range;

use saphyr_parser::{Event, Parser, ScalarStyle, Span};

mod render;
pub use render::{Scalar, render_scalar};

/// 到某个节点的路径。`providers[1].base_url` 写成
/// `[Key("providers"), Index(1), Key("base_url")]`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Key(String),
    Index(usize),
}

impl Step {
    pub fn key(s: impl Into<String>) -> Self {
        Step::Key(s.into())
    }
}

/// 便捷写法：`path!["providers", 1, "base_url"]`
#[macro_export]
macro_rules! path {
    ($($x:expr),* $(,)?) => { [$($crate::Step::from($x)),*] };
}

impl From<&str> for Step {
    fn from(s: &str) -> Self {
        Step::Key(s.to_string())
    }
}
impl From<String> for Step {
    fn from(s: String) -> Self {
        Step::Key(s)
    }
}
impl From<usize> for Step {
    fn from(i: usize) -> Self {
        Step::Index(i)
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PatchError {
    #[error("这份 YAML 解析不了：{0}")]
    Parse(String),
    #[error("配置里没有 `{0}` 这个位置")]
    NotFound(String),
    #[error("`{0}` 不是一个标量 —— 只有标量能这样改；结构性的增删走另一条路")]
    NotScalar(String),
    /// **写之前的最后一道闸。**改完在内存里重新解析一遍，对不上就拒绝
    /// 写入。开销几百微秒，换来「永远不会写出一个自己都解析不了的文件」。
    #[error("补丁自检没过：{0}。**没有写盘。**这是个 bug，请把配置文件贴到 issue 里")]
    SelfCheck(String),
    /// 同一个 map 里出现了两个同名的键。
    #[error(
        "`{0}` 在配置里出现了不止一次。同名的键谁生效各家解析器并不一致，改错一个的表现是「改了却没生效」—— 请先手动删掉重复的那个"
    )]
    Duplicate(String),
    #[error(
        "`{0}` 是一个多行块标量（`|` / `>`）。缩进在这里是内容的一部分，改错一格就改了值 —— 请直接编辑配置文件"
    )]
    BlockScalar(String),
    /// 锚点和别名让「改一处」不再是改一处。
    #[error(
        "`{0}` 落在 YAML 的锚点/别名（`&x` / `*x`）里。改它会同时改掉引用它的每一处，所以这里不动它 —— 请直接编辑配置文件"
    )]
    AnchorOrAlias(String),
}

/// 路径的人话形式（`providers[1].base_url`）。错误信息和写回校验都用它。
pub fn show(path: &[Step]) -> String {
    let mut s = String::new();
    for st in path {
        match st {
            Step::Key(k) => {
                if !s.is_empty() {
                    s.push('.');
                }
                s.push_str(k);
            }
            Step::Index(i) => s.push_str(&format!("[{i}]")),
        }
    }
    s
}

/// 一个标量值在原文里的位置和形态。
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// **字节区间**。saphyr 的 marker 是按 char 计的，这里已经换算过了 ——
    /// 中文配置下两者差得很远，而按 char 的下标去切 `&str` 会 panic
    /// （cc-switch 栽过两次的那个坑）。
    pub bytes: Range<usize>,
    pub value: String,
    pub style: ScalarStyle,
    /// 带锚点或者本身是别名。**这两种情况一律不改**
    pub aliased: bool,
}

/// char 下标 → 字节下标。
///
/// 只走一遍，把需要的几个位置一起换算 —— 对每个 span 都从头数一遍的话，
/// 一份两百行的配置会变成一次 O(n²)。
struct CharToByte {
    /// 第 i 个 char 的字节起点；末尾多一个哨兵 = 总字节数
    table: Vec<usize>,
}

impl CharToByte {
    fn new(s: &str) -> Self {
        let mut table: Vec<usize> = s.char_indices().map(|(i, _)| i).collect();
        table.push(s.len());
        Self { table }
    }
    fn at(&self, char_idx: usize) -> usize {
        // 越界时给总长度而不是 panic：解析器给出的位置理论上不会越界，
        // 但这一层的任何 panic 都会变成「应用打不开」。
        *self
            .table
            .get(char_idx)
            .unwrap_or(self.table.last().unwrap())
    }
    fn range(&self, sp: Span) -> Range<usize> {
        self.at(sp.start.index())..self.at(sp.end.index())
    }
}

/// 把标量的 span 收到它真正的最后一个字节。
///
/// **解析器给的 `end` 会一路跑到下一个 token 的开头** —— 对
/// `base_url: 'https://x'   # 尾注释` 来说，那意味着 span 里包着那句
/// 注释。照着它替换，用户的注释就没了：一个「只改了一个字段」的操作，
/// 悄悄吃掉了他写的东西。这是最怕的那类失败。
fn tighten(raw: &str, style: ScalarStyle) -> usize {
    match style {
        ScalarStyle::SingleQuoted => closing_quote(raw, '\'', false),
        ScalarStyle::DoubleQuoted => closing_quote(raw, '"', true),
        // 块标量（`|` / `>`）不支持原地改（见 `set`），这里只把尾部的
        // 空白收掉，让 `find` 报出来的区间不至于离谱。
        ScalarStyle::Literal | ScalarStyle::Folded => raw.trim_end().len(),
        _ => {
            // 纯量里的 `#` 只有前面是空白时才开启注释。`a#b` 是一个
            // 完整的纯量，不能从中间切开。
            let mut cut = raw.len();
            let b = raw.as_bytes();
            for (i, &c) in b.iter().enumerate() {
                if c == b'#' && i > 0 && (b[i - 1] == b' ' || b[i - 1] == b'\t') {
                    cut = i;
                    break;
                }
            }
            raw[..cut].trim_end().len()
        }
    }
}

/// 引号标量的收尾位置（含那个收尾引号）。
fn closing_quote(raw: &str, q: char, backslash_escapes: bool) -> usize {
    let mut it = raw.char_indices();
    // 第一个字符就是开引号
    let Some((_, first)) = it.next() else {
        return raw.len();
    };
    if first != q {
        return raw.trim_end().len();
    }
    while let Some((i, c)) = it.next() {
        if backslash_escapes && c == '\\' {
            it.next();
            continue;
        }
        if c == q {
            // 单引号里 `''` 表示一个引号，不是收尾
            if !backslash_escapes && matches!(raw[i + 1..].chars().next(), Some(n) if n == q) {
                it.next();
                continue;
            }
            return i + c.len_utf8();
        }
    }
    // 没找到收尾引号 —— 不该发生（解析器已经过了），退回保守值
    raw.trim_end().len()
}

enum Frame {
    Map { expect_key: bool },
    Seq { index: usize },
}

/// 一个节点是什么。
#[derive(Debug, Clone, PartialEq)]
pub enum NodeKind {
    Scalar { value: String, style: ScalarStyle },
    Map,
    Seq,
    Alias,
}

/// 文档里的一个节点。
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub path: Vec<Step>,
    /// **字节区间。**标量的已经收到真正的最后一个字节；容器的是解析器
    /// 给的原样（会一路跑到下一个 token），只用来定位、不用来替换。
    pub bytes: Range<usize>,
    pub kind: NodeKind,
    /// 带锚点（`&x`）或者本身是别名（`*x`）
    pub anchored: bool,
}

/// 光标停在这个字节位置上时，它落在哪个节点里。
///
/// 给反向联动用：文本模式里光标停在某个 provider 上 → 侧边
/// 显示它的表单。
///
/// **用解析器算，不用正则猜。**猜错的表现是「我明明点在中转上，右边
/// 显示的是官方」—— 那比没有这个功能更让人不信任这一页。
///
/// 返回**最深的那个包含它的节点**的路径。容器节点的 `bytes` 是解析器
/// 给的原样（会一路跑到下一个 token），所以这里按「起点最靠后、且起点
/// 不超过光标」来挑 —— 那正好是最内层的那个。
pub fn path_at(text: &str, offset: usize) -> Option<Vec<Step>> {
    let all = nodes(text).ok()?;
    all.into_iter()
        .filter(|n| n.bytes.start <= offset && !n.path.is_empty())
        .max_by_key(|n| (n.bytes.start, n.path.len()))
        .map(|n| n.path)
}

/// 把整份文档摊平成节点列表。
///
/// 走一遍就把所有位置算出来，比「每改一个字段解析一遍」省事，也让
/// 「这份配置里有哪些字段」成为一个能回答的问题（表单模式要用）。
pub fn nodes(text: &str) -> Result<Vec<Node>, PatchError> {
    let c2b = CharToByte::new(text);
    let mut out = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut cur: Vec<Step> = Vec::new();
    let mut pending: Option<Step> = None;

    for ev in Parser::new_from_str(text) {
        let (ev, sp) = ev.map_err(|e| PatchError::Parse(e.to_string()))?;
        match ev {
            Event::Scalar(v, style, anchor_id, _) => {
                if matches!(stack.last(), Some(Frame::Map { expect_key: true })) {
                    pending = Some(Step::Key(v.to_string()));
                    if let Some(Frame::Map { expect_key }) = stack.last_mut() {
                        *expect_key = false;
                    }
                    continue;
                }
                if let Some(Frame::Seq { index }) = stack.last() {
                    pending = Some(Step::Index(*index));
                }
                let mut bytes = c2b.range(sp);
                bytes.end = bytes.start + tighten(&text[bytes.clone()], style);
                out.push(Node {
                    path: node_path(&pending, &cur),
                    bytes,
                    kind: NodeKind::Scalar {
                        value: v.to_string(),
                        style,
                    },
                    anchored: anchor_id != 0,
                });
                advance(&mut stack, &mut pending);
            }
            Event::Alias(_) => {
                if let Some(Frame::Seq { index }) = stack.last() {
                    pending = Some(Step::Index(*index));
                }
                out.push(Node {
                    path: node_path(&pending, &cur),
                    bytes: c2b.range(sp),
                    kind: NodeKind::Alias,
                    anchored: true,
                });
                advance(&mut stack, &mut pending);
            }
            Event::MappingStart(anchor_id, _) | Event::SequenceStart(anchor_id, _) => {
                if let Some(Frame::Seq { index }) = stack.last() {
                    pending = Some(Step::Index(*index));
                }
                let p = node_path(&pending, &cur);
                let is_map = matches!(ev, Event::MappingStart(..));
                out.push(Node {
                    path: p.clone(),
                    bytes: c2b.range(sp),
                    kind: if is_map { NodeKind::Map } else { NodeKind::Seq },
                    anchored: anchor_id != 0,
                });
                cur = p;
                stack.push(if is_map {
                    Frame::Map { expect_key: true }
                } else {
                    Frame::Seq { index: 0 }
                });
                pending = None;
            }
            Event::MappingEnd | Event::SequenceEnd => {
                stack.pop();
                cur.pop();
                pending = None;
                advance(&mut stack, &mut pending);
            }
            _ => {}
        }
    }
    Ok(out)
}

fn node_path(pending: &Option<Step>, cur: &[Step]) -> Vec<Step> {
    let mut p = cur.to_vec();
    if let Some(s) = pending {
        p.push(s.clone());
    }
    p
}

/// 所有标量的路径。表单模式用它回答「这份配置里有哪些字段」。
pub fn scalar_paths(text: &str) -> Result<Vec<Vec<Step>>, PatchError> {
    Ok(nodes(text)?
        .into_iter()
        .filter(|n| matches!(n.kind, NodeKind::Scalar { .. }))
        .map(|n| n.path)
        .collect())
}

/// 找到某个路径上的标量值。
pub fn find(text: &str, path: &[Step]) -> Result<Found, PatchError> {
    let all = nodes(text)?;
    let mut hits = all.into_iter().filter(|n| n.path == path);
    let Some(n) = hits.next() else {
        return Err(PatchError::NotFound(show(path)));
    };
    // **同名键要拒绝，不能挑一个改。**谁生效各家解析器并不一致，改错
    // 那个的表现是「改了却没生效」—— 而用户会以为是我们没写进去，然后
    // 反复点保存。property test 抓到的。
    if hits.next().is_some() {
        return Err(PatchError::Duplicate(show(path)));
    }
    if n.anchored || matches!(n.kind, NodeKind::Alias) {
        return Err(PatchError::AnchorOrAlias(show(path)));
    }
    match n.kind {
        NodeKind::Scalar { value, style } => Ok(Found {
            bytes: n.bytes,
            value,
            style,
            aliased: false,
        }),
        _ => Err(PatchError::NotScalar(show(path))),
    }
}

/// 一个值走完了，让父容器往前挪一格。
fn advance(stack: &mut [Frame], pending: &mut Option<Step>) {
    match stack.last_mut() {
        Some(Frame::Map { expect_key }) => *expect_key = true,
        Some(Frame::Seq { index }) => *index += 1,
        None => {}
    }
    *pending = None;
}

/// 改一个标量值，**只动它那一段字节**。
///
/// 三道护栏，其中第三道在运行时（不只是测试时）：
///
/// 1. 只有标量能这样改。结构性的增删走别的路径。
/// 2. 锚点和别名一律不动 —— 改它会同时改掉引用它的每一处。
/// 3. **改完在内存里重新解析一遍，对不上就拒绝返回。**几百微秒，换来
///    「永远不会写出一个自己都解析不了的文件」。
pub fn set(text: &str, path: &[Step], value: &Scalar) -> Result<String, PatchError> {
    let found = find(text, path)?;
    if found.aliased {
        return Err(PatchError::AnchorOrAlias(show(path)));
    }
    // 块标量原地改要重排缩进，而缩进在块标量里**是内容的一部分** ——
    // 改错一格就改了值。这一层不碰它们（退路：结构性的东西
    // 引导到文本模式）。
    if matches!(found.style, ScalarStyle::Literal | ScalarStyle::Folded) {
        return Err(PatchError::BlockScalar(show(path)));
    }
    let rendered = render_scalar(value, found.style);
    let mut out = String::with_capacity(text.len() + rendered.len());
    out.push_str(&text[..found.bytes.start]);
    out.push_str(&rendered);
    out.push_str(&text[found.bytes.end..]);

    // ── 护栏三 ──────────────────────────────────────────────────────
    let after = find(&out, path)
        .map_err(|e| PatchError::SelfCheck(format!("改完之后 `{}` 找不回来了：{e}", show(path))))?;
    let want = value.as_yaml_text();
    if after.value != want {
        return Err(PatchError::SelfCheck(format!(
            "`{}` 改完读回来是 {:?}，期望 {:?}",
            show(path),
            after.value,
            want
        )));
    }
    // **别处一个字节都不能变。**这条比上一条更值钱：上一条只保证目标对了，
    // 而真正的灾难是「目标对了，同时把别的地方改坏了」。
    let untouched_before = text[..found.bytes.start] == out[..found.bytes.start];
    let untouched_after = text[found.bytes.end..] == out[found.bytes.start + rendered.len()..];
    if !untouched_before || !untouched_after {
        return Err(PatchError::SelfCheck("目标之外的字节被动了".into()));
    }
    Ok(out)
}

/// 往一个已经存在的映射里**插一个新的标量键**。
///
/// # 为什么必须有它
///
/// `set` 只能改已经写在文件里的键。而配置里绝大多数字段是可选的、
/// 默认不写的（`protocol`、`billing`、`trust`、`session_affinity`…）——
/// 于是表单模式只能改那些「用户碰巧写过」的字段，**用户想设一个他从来
/// 没设过的值时，界面是死的**。而那正是他最需要界面的时刻。
///
/// # 边界
///
/// 只插**标量**，而且父节点必须已经是个映射。新增一个列表项（多一个
/// provider）仍然走文本模式 —— 那是结构性改动，退路说得很清楚。
///
/// 插在父映射**最后一个子键的下一行**，缩进抄那一行的。不去猜「该插在
/// 哪两行之间」—— 那只会打乱用户自己排的顺序。
pub fn insert(text: &str, path: &[Step], value: &Scalar) -> Result<String, PatchError> {
    if find(text, path).is_ok() {
        // 已经有了就是一次普通的替换 —— 调用方不用先问一遍
        return set(text, path, value);
    }
    let Some((last, parent)) = path.split_last() else {
        return Err(PatchError::NotFound(show(path)));
    };
    let Step::Key(key) = last else {
        return Err(PatchError::NotFound(show(path)));
    };
    let all = nodes(text)?;
    // 父节点得存在，而且得是个映射
    let p = all
        .iter()
        .find(|n| n.path == parent)
        .ok_or_else(|| PatchError::NotFound(show(parent)))?;
    if !matches!(p.kind, NodeKind::Map) {
        return Err(PatchError::NotFound(show(parent)));
    }
    if p.anchored {
        return Err(PatchError::AnchorOrAlias(show(parent)));
    }
    // **行内写法（`{ a: 1, b: 2 }`）要插在花括号里面。**
    //
    // 按块式的做法在下一行插，产出的是一份解析不了的 YAML —— 护栏会
    // 拦住，但那时用户看到的是「这是个 bug，请贴到 issue 里」，而他
    // 只是用了一种完全合法的写法。格式保留语料里本来就列了
    // 「流式与块式混排」。
    if text[p.bytes.start..].starts_with('{') {
        let close = flow_end(text, p.bytes.start)
            .ok_or_else(|| PatchError::NotFound(format!("{}（行内映射没收尾）", show(parent))))?;
        let inner = text[p.bytes.start + 1..close].trim();
        let rendered = render_scalar(value, ScalarStyle::Plain);
        let piece = if inner.is_empty() {
            format!("{key}: {rendered}")
        } else {
            // 抄已有的逗号风格：`{a: 1, b: 2}` 和 `{a: 1,b: 2}` 都有人
            // 写，跟着来比统一成我们的偏好更不打扰 —— 这是他的文件
            let spaced = text[p.bytes.start..close].contains(", ");
            format!("{}{key}: {rendered}", if spaced { ", " } else { "," })
        };
        // 收尾的 `}` 前面有空格（`{ a: 1 }`）就插在那个空格之前
        let mut at = close;
        while at > p.bytes.start + 1 && text.as_bytes()[at - 1] == b' ' {
            at -= 1;
        }
        let mut out = String::with_capacity(text.len() + piece.len());
        out.push_str(&text[..at]);
        out.push_str(&piece);
        out.push_str(&text[at..]);
        return checked(text, out, path, value);
    }

    // 父映射的直接子节点里，位置最靠后的那个
    let last_child = all
        .iter()
        .filter(|n| n.path.len() == parent.len() + 1 && n.path.starts_with(parent))
        .max_by_key(|n| n.bytes.end)
        .ok_or_else(|| PatchError::NotFound(show(parent)))?;
    // **缩进抄那一行的**，而不是按层数算 —— 用户可能用的是 4 空格，
    // 也可能是列表项里那种 `- name: x` 的对齐
    let line_start = text[..last_child.bytes.start]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let indent: String = text[line_start..]
        .chars()
        .take_while(|c| *c == ' ')
        .collect();
    // 那一行的结尾。**容器子节点的 `bytes.end` 会跑过头**（注释里说了
    // 它一路跑到下一个 token），所以从行首往后找换行，而不是信它
    let after = text[line_start..]
        .find('\n')
        .map(|i| line_start + i)
        .unwrap_or(text.len());
    // 子节点跨了多行（它自己是个映射/列表）时，往后走到这一块的末尾
    let mut end = after.max(last_child.bytes.end.min(text.len()));
    if end > after {
        end = text[end..]
            .find('\n')
            .map(|i| end + i)
            .unwrap_or(text.len());
    }
    let rendered = render_scalar(value, ScalarStyle::Plain);
    let mut out = String::with_capacity(text.len() + key.len() + rendered.len() + 4);
    out.push_str(&text[..end]);
    out.push('\n');
    out.push_str(&indent);
    out.push_str(key);
    out.push_str(": ");
    out.push_str(&rendered);
    out.push_str(&text[end..]);

    checked(text, out, path, value)
}

/// 行内映射从 `{` 开始的那个位置，找到配对的 `}`。
///
/// 要认引号里的花括号 —— `{cmd: "a}b"}` 里那个不是收尾。
fn flow_end(text: &str, open: usize) -> Option<usize> {
    let b = text.as_bytes();
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    for (i, c) in b.iter().enumerate().skip(open) {
        match quote {
            Some(q) => {
                if *c == b'\\' {
                    continue;
                }
                if *c == q {
                    quote = None;
                }
            }
            None => match *c {
                b'"' | b'\'' => quote = Some(*c),
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            },
        }
    }
    None
}

/// `insert` 的三道护栏。**两条路（块式、行内）共用一份** —— 各写一份的
/// 话，迟早有一条上的检查会比另一条松。
fn checked(before: &str, out: String, path: &[Step], value: &Scalar) -> Result<String, PatchError> {
    let back = find(&out, path)
        .map_err(|e| PatchError::SelfCheck(format!("插完之后 `{}` 找不回来：{e}", show(path))))?;
    if back.value != value.as_yaml_text() {
        return Err(PatchError::SelfCheck(format!(
            "插完之后 `{}` 读回来是 `{}`，不是要写的那个",
            show(path),
            back.value
        )));
    }
    // 别的字段一个都不能动
    let before_all = nodes(before)?;
    let after_all = nodes(&out)?;
    if after_all.len() != before_all.len() + 1 {
        return Err(PatchError::SelfCheck(format!(
            "插一个字段却让节点数从 {} 变成了 {}",
            before_all.len(),
            after_all.len()
        )));
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────── 列表增删
//
// **`set` / `insert` 只动标量，而配置里一半的编辑是结构性的**：加一把
// 密钥、删一条路由规则、给某把密钥分配一条路由。这些都是往块式列表里
// 增删一项。
//
// 没有这一层的后果不是「少个功能」，是界面只能读不能写 —— 补丁协议里
// 唯一的操作是「替换一个标量」，而新建任何东西都不是替换。

/// 一项在列表里占的字节区间，**含行首缩进、不含结尾换行**。
///
/// 按行推而不是信容器节点的 `bytes`：那个区间会一路跑到下一个 token
/// （文件开头的注释里写了），拿来切字节会把下一项的开头一起切掉。
fn item_span(text: &str, seq_path: &[Step], index: usize) -> Result<Range<usize>, PatchError> {
    let all = nodes(text)?;
    let mut want = seq_path.to_vec();
    want.push(Step::Index(index));
    // 这一项底下最靠前的那个节点，就是这一项的起点所在的行
    let start_node = all
        .iter()
        .filter(|n| n.path.starts_with(&want))
        .map(|n| n.bytes.start)
        .min()
        .ok_or_else(|| PatchError::NotFound(show(&want)))?;
    let line_start = text[..start_node].rfind('\n').map(|i| i + 1).unwrap_or(0);
    // 行首到 `-` 的缩进
    let dash = text[line_start..]
        .find('-')
        .map(|i| line_start + i)
        .ok_or_else(|| PatchError::NotFound(format!("{}（不是块式列表）", show(&want))))?;
    let indent = dash - line_start;
    // 往后走到下一项的 `-`（同缩进）或者这一块结束
    let mut at = text[line_start..]
        .find('\n')
        .map(|i| line_start + i + 1)
        .unwrap_or(text.len());
    while at < text.len() {
        let line_end = text[at..].find('\n').map(|i| at + i).unwrap_or(text.len());
        let line = &text[at..line_end];
        let trimmed = line.trim_start();
        // 空行归属于**下一项**：它是分隔，不是内容。跟着上一项删的话，
        // 删掉中间一项会让前后两项贴在一起。
        if !trimmed.is_empty() {
            let this_indent = line.len() - trimmed.len();
            if this_indent <= indent {
                break;
            }
        }
        at = if line_end >= text.len() {
            text.len()
        } else {
            line_end + 1
        };
    }
    // 回退掉结尾那个换行。**只回退一个** —— 再往前是上一项的内容，
    // 而这个区间是要交给调用方切字节的。
    let end = if at > line_start && text.as_bytes()[at - 1] == b'\n' {
        at - 1
    } else {
        at
    };
    Ok(line_start..end)
}

/// 往块式列表末尾加一项。
///
/// `item` 是这一项的 YAML 片段，**不带前导的 `- `**，多行之间用 `\n`
/// 分隔：`"name: codex\nkey: tw-abc"` 变成
///
/// ```text
///   - name: codex
///     key: tw-abc
/// ```
///
/// 缩进抄已有那一项的 —— 用户用的是两格还是四格是他的事。
pub fn append(text: &str, seq_path: &[Step], item: &str) -> Result<String, PatchError> {
    let all = nodes(text)?;
    let count = all
        .iter()
        .filter(|n| n.path.len() == seq_path.len() + 1 && n.path.starts_with(seq_path))
        .filter_map(|n| match n.path.last() {
            Some(Step::Index(i)) => Some(*i + 1),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    if count == 0 {
        return Err(PatchError::NotFound(format!(
            "{}（空列表还不支持追加，先在文本模式里写第一项）",
            show(seq_path)
        )));
    }
    let last = item_span(text, seq_path, count - 1)?;
    let line_start = text[..last.start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let dash = text[line_start..last.end]
        .find('-')
        .map(|i| line_start + i)
        .ok_or_else(|| PatchError::NotFound(show(seq_path)))?;
    let indent = " ".repeat(dash - line_start);
    // `- ` 之后的内容相对行首多缩两格（`- ` 本身的宽度）
    let cont = format!("{indent}  ");
    let mut piece = String::from("\n");
    for (i, line) in item.lines().enumerate() {
        if i > 0 {
            piece.push('\n');
            piece.push_str(&cont);
        } else {
            piece.push_str(&indent);
            piece.push_str("- ");
        }
        piece.push_str(line);
    }
    let mut out = String::with_capacity(text.len() + piece.len());
    out.push_str(&text[..last.end]);
    out.push_str(&piece);
    out.push_str(&text[last.end..]);
    checked_structural(text, out, seq_path, count + 1)
}

/// 从块式列表里删掉第 `index` 项。
pub fn remove(text: &str, seq_path: &[Step], index: usize) -> Result<String, PatchError> {
    let span = item_span(text, seq_path, index)?;
    let all = nodes(text)?;
    let count = all
        .iter()
        .filter(|n| n.path.len() == seq_path.len() + 1 && n.path.starts_with(seq_path))
        .filter_map(|n| match n.path.last() {
            Some(Step::Index(i)) => Some(*i + 1),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    // 连同它后面那个换行一起删，否则会留下一个空行
    let mut end = span.end;
    if end < text.len() && text.as_bytes()[end] == b'\n' {
        end += 1;
    }
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..span.start]);
    out.push_str(&text[end..]);
    checked_structural(text, out, seq_path, count.saturating_sub(1))
}

/// 结构性改动的护栏：**改完在内存里重新解析，数一遍这个列表有几项**。
///
/// 和标量那条护栏是同一个理由，只是断言不同 —— 那边断言「写进去的值
/// 读得回来」，这边断言「列表长度正好差一」。改出一份解析不了的 YAML
/// 是这一层最该防的事，而它只会在下一次加载时暴露。
fn checked_structural(
    before: &str,
    out: String,
    seq_path: &[Step],
    want: usize,
) -> Result<String, PatchError> {
    let after = nodes(&out).map_err(|e| {
        PatchError::SelfCheck(format!("改完 `{}` 之后解析不了：{e}", show(seq_path)))
    })?;
    let got = after
        .iter()
        .filter(|n| n.path.len() == seq_path.len() + 1 && n.path.starts_with(seq_path))
        .filter_map(|n| match n.path.last() {
            Some(Step::Index(i)) => Some(*i + 1),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    if got != want {
        return Err(PatchError::SelfCheck(format!(
            "改完之后 `{}` 有 {got} 项，应该是 {want} 项",
            show(seq_path)
        )));
    }
    let _ = before;
    Ok(out)
}

#[cfg(test)]
mod seq_tests {
    use super::*;

    const CFG: &str = "version: 1\nclients:\n  # 第一把是首次运行生成的\n  - name: default\n    key: tw-aaa\n  - name: codex\n    key: tw-bbb\nproviders:\n  - name: 官方\n    base_url: https://api.anthropic.com\n";

    fn clients() -> Vec<Step> {
        vec![Step::Key("clients".into())]
    }

    #[test]
    fn appending_a_key_keeps_every_comment_and_the_other_entries() {
        let out = append(CFG, &clients(), "name: cline\nkey: tw-ccc").unwrap();
        // 注释还在 —— 这是整个 tw-yaml 存在的理由
        assert!(out.contains("# 第一把是首次运行生成的"), "{out}");
        assert!(out.contains("- name: default"), "{out}");
        assert!(out.contains("- name: codex"), "{out}");
        assert!(out.contains("  - name: cline\n    key: tw-ccc"), "{out}");
        // 新的一项在 providers 之前，不是文件末尾
        assert!(
            out.find("cline").unwrap() < out.find("providers").unwrap(),
            "{out}"
        );
        let cfg: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(cfg["clients"].as_sequence().unwrap().len(), 3);
    }

    #[test]
    fn removing_the_middle_entry_does_not_glue_its_neighbours_together() {
        let three = append(CFG, &clients(), "name: cline\nkey: tw-ccc").unwrap();
        let out = remove(&three, &clients(), 1).unwrap();
        assert!(!out.contains("codex"), "{out}");
        assert!(out.contains("- name: default"), "{out}");
        assert!(out.contains("- name: cline"), "{out}");
        assert!(out.contains("# 第一把是首次运行生成的"), "{out}");
        let cfg: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(cfg["clients"].as_sequence().unwrap().len(), 2);
    }

    #[test]
    fn removing_the_first_entry_keeps_the_rest_parseable() {
        let out = remove(CFG, &clients(), 0).unwrap();
        let cfg: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        let cs = cfg["clients"].as_sequence().unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0]["name"].as_str(), Some("codex"));
    }

    #[test]
    fn a_four_space_file_gets_four_space_entries() {
        // **缩进抄用户的，不是我们的偏好。**这是他的文件。
        let four = "clients:\n    - name: a\n      key: tw-a\n";
        let out = append(four, &clients(), "name: b\nkey: tw-b").unwrap();
        assert!(out.contains("    - name: b\n      key: tw-b"), "{out}");
        let cfg: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(cfg["clients"].as_sequence().unwrap().len(), 2);
    }

    #[test]
    fn a_trailing_comment_on_the_last_entry_stays_with_it() {
        let c = "clients:\n  - name: a\n    key: tw-a  # 这把给 Claude Code\n";
        let out = append(c, &clients(), "name: b\nkey: tw-b").unwrap();
        assert!(out.contains("tw-a  # 这把给 Claude Code"), "{out}");
        let cfg: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(cfg["clients"].as_sequence().unwrap().len(), 2);
    }

    #[test]
    fn appending_a_nested_list_round_trips() {
        // `routes` 是列表里套列表 —— 这是路由页要写的形状
        let c = "routes:\n  - name: 默认\n    default: true\n    rules:\n      - name: r1\n        to: a\n";
        let out = append(
            c,
            &[
                Step::Key("routes".into()),
                Step::Index(0),
                Step::Key("rules".into()),
            ],
            "name: r2\nto: b",
        )
        .unwrap();
        let cfg: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        let rules = cfg["routes"][0]["rules"].as_sequence().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[1]["name"].as_str(), Some("r2"));
    }

    #[test]
    fn an_empty_list_says_so_instead_of_writing_something_broken() {
        // 空列表还不支持 —— **说出来，而不是写出一份解析不了的文件**
        let c = "clients: []\n";
        let e = append(c, &clients(), "name: a\nkey: tw-a").unwrap_err();
        assert!(format!("{e}").contains("空列表"), "{e}");
    }
}
