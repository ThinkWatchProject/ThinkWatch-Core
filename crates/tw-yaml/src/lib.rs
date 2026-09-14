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

/// 引出 `at` 这个值的那个冒号在哪，返回它**后面**一个字节。
///
/// 容器的值起点是它内容的第一个 token（`[`、第一个 `-`、第一个子键），
/// 键写在它前面、常常在上一行。中间可能隔着整行注释，而注释里完全可能
/// 带冒号（`# 见 https://x/y`），所以按行回退、跳过空行和注释行。
fn after_key_colon(text: &str, at: usize) -> Option<usize> {
    fn strip_comment(line: &str) -> &str {
        match line.find(" #") {
            Some(i) => &line[..i],
            None => line,
        }
    }
    let mut line_start = text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    // 键和值在同一行上（`allow: []`）
    if let Some(i) = text[line_start..at].rfind(':') {
        return Some(line_start + i + 1);
    }
    while line_start > 0 {
        let prev_end = line_start - 1;
        let prev_start = text[..prev_end].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line = &text[prev_start..prev_end];
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            return strip_comment(line).rfind(':').map(|i| prev_start + i + 1);
        }
        line_start = prev_start;
    }
    None
}

/// 拿一个标量盖掉一整块列表或映射。
///
/// **三态里的「回来那一步」走的是这条路。**`allow` 从一张清单退回
/// 「什么都不限」时写进去的是 `~`，原来那一整块要跟着消失。没有它的
/// 话，`insert` 会在旁边再写一个同名键 —— 护栏拦得住，但用户看到的是
/// 「这是个 bug，请贴到 issue 里」，而他只是想把设过的东西撤回去。
fn overwrite_block(
    text: &str,
    all: &[Node],
    node: &Node,
    rendered: &str,
) -> Result<String, PatchError> {
    let at = after_key_colon(text, node.bytes.start)
        .ok_or_else(|| PatchError::NotFound(show(&node.path)))?;
    // 有后代就按整块算；`key: []` 这种没有后代，就到本行行尾
    let end = block_of(text, all, &node.path)
        .map(|(e, _)| e)
        .unwrap_or_else(|| {
            text[node.bytes.start..]
                .find('\n')
                .map(|i| node.bytes.start + i)
                .unwrap_or(text.len())
        });
    if end < at {
        return Err(PatchError::NotFound(show(&node.path)));
    }
    let mut out = String::with_capacity(text.len() + rendered.len());
    out.push_str(&text[..at]);
    out.push(' ');
    out.push_str(rendered);
    out.push_str(&text[end..]);
    Ok(out)
}

/// 值该写在哪一段字节上。
///
/// **值是空的时候（`key:` 后面什么都没有），解析器给的标记停在冒号
/// 上面而不是后面** —— 照它直接写，`allow` 会变成 `allow[]:`，`note`
/// 会变成 `note:hi` 里那个叫 `note:hi` 的标量。配置里「写了键、还没填
/// 值」很常见，而这两种产物都解析不回原来那个键。
fn value_slot(text: &str, at: &Range<usize>) -> Range<usize> {
    if !at.is_empty() {
        return at.clone();
    }
    let line_end = text[at.start..]
        .find('\n')
        .map(|i| at.start + i)
        .unwrap_or(text.len());
    match text[at.start..line_end].find(':') {
        Some(i) => {
            let after = at.start + i + 1;
            after..after
        }
        None => at.clone(),
    }
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
    let slot = value_slot(text, &found.bytes);
    // 冒号后面还没有空格就补一个
    let pad = if slot.is_empty() && !text[slot.start..].starts_with(' ') {
        " "
    } else {
        ""
    };
    let mut out = String::with_capacity(text.len() + rendered.len() + pad.len());
    out.push_str(&text[..slot.start]);
    out.push_str(pad);
    out.push_str(&rendered);
    out.push_str(&text[slot.end..]);

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
    let untouched_before = text[..slot.start] == out[..slot.start];
    let untouched_after = text[slot.end..] == out[slot.start + pad.len() + rendered.len()..];
    if !untouched_before || !untouched_after {
        return Err(PatchError::SelfCheck("目标之外的字节被动了".into()));
    }
    Ok(out)
}

/// 一个映射节点底下那一块到哪结束，以及它的子键缩进在第几列。
///
/// **不能拿「最后一个直接子节点」的位置来算。**容器子节点的 `bytes` 是
/// 解析器给的一个空区间，落在它内容的起点上 —— 对 `routes:` 来说那是
/// 第一个 `-` 那一行。照它抄缩进会抄成列表项的缩进，插入点也会落进列表
/// 中间：`insert` 和 `append_first` 各自栽过一次（一次把 `routes:` 插到
/// 了两个 client 之间，一次把 `default_route:` 插到了列表项里面），所以
/// 这段逻辑只留一份。
///
/// 按整块算：末尾取这个节点底下**所有后代**的最远处，缩进取这块里非空
/// 行的最小缩进 —— 那正是这一层键所在的列。
fn block_of(text: &str, all: &[Node], parent: &[Step]) -> Option<(usize, String)> {
    let (mut start, mut last) = (usize::MAX, 0usize);
    for n in all
        .iter()
        .filter(|n| n.path.len() > parent.len() && n.path.starts_with(parent))
    {
        start = start.min(n.bytes.start);
        last = last.max(n.bytes.end.min(text.len()));
    }
    if start == usize::MAX {
        return None;
    }
    let first_line = text[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end = text[last..]
        .find('\n')
        .map(|i| last + i)
        .unwrap_or(text.len());
    // **跳过 `-` 开头的行和注释行。**映射本身是列表的一项时
    // （`clients[0]`），它的第一个键写在 `- name: a` 这一行上，那一行的
    // 缩进是短划线的，比键实际所在的列少两格；注释则是用户想顶格写就
    // 顶格写的，跟这一层的缩进无关。
    let indent = text[first_line..end]
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.is_empty() && !t.starts_with('-') && !t.starts_with('#')
        })
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .or_else(|| {
            // 整块只有一行 `- name: a` —— 键在短划线后面两格
            text[first_line..end]
                .lines()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.len() - l.trim_start().len() + 2)
        })
        .unwrap_or(0);
    Some((end, " ".repeat(indent)))
}

/// 这个列表现在有几项。
fn count_items(all: &[Node], seq_path: &[Step]) -> usize {
    all.iter()
        .filter(|n| n.path.len() == seq_path.len() + 1 && n.path.starts_with(seq_path))
        .filter_map(|n| match n.path.last() {
            Some(Step::Index(i)) => Some(*i + 1),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

/// 把 `at` 这段值换成 `[]`，必要时在冒号后面补空格。
fn put_empty_seq(text: &str, at: &Range<usize>) -> String {
    let slot = value_slot(text, at);
    let mut out = String::with_capacity(text.len() + 3);
    out.push_str(&text[..slot.start]);
    if !text[slot.start..].starts_with(' ') {
        out.push(' ');
    }
    out.push_str("[]");
    out.push_str(&text[slot.end..]);
    out
}

/// 把 `path` 这个键写进文档，值是**已经渲染好的一段文本**。
///
/// 返回新文本，以及多出来的节点数 —— 护栏要拿它对账。
///
/// **缺的中间层会一起补出来。**`limits`、`client_probes` 这些整段默认
/// 不写（「第一天的配置是六行」），于是「改里面某一项」就是它们的第一次
/// 写入；要求父节点先存在，等于这些设置项在界面上从头到尾是死的。
fn splice_key(
    text: &str,
    all: &[Node],
    path: &[Step],
    rendered: &str,
) -> Result<(String, usize), PatchError> {
    if path.is_empty() {
        return Err(PatchError::NotFound(show(path)));
    }
    // 最深的那个已经存在、而且能往里写的祖先。根一定在。
    let mut depth = path.len() - 1;
    while depth > 0 {
        match all.iter().find(|n| n.path == path[..depth]) {
            Some(n) if n.anchored => return Err(PatchError::AnchorOrAlias(show(&path[..depth]))),
            Some(n) if matches!(n.kind, NodeKind::Map) => break,
            // 存在但不是映射 —— 往里插字段是在改它的类型，不干
            Some(_) => return Err(PatchError::NotFound(show(&path[..depth]))),
            None => depth -= 1,
        }
    }
    let anchor = &path[..depth];
    let a = all
        .iter()
        .find(|n| n.path == anchor)
        .ok_or_else(|| PatchError::NotFound(show(anchor)))?;
    if a.anchored {
        return Err(PatchError::AnchorOrAlias(show(anchor)));
    }
    if !matches!(a.kind, NodeKind::Map) {
        return Err(PatchError::NotFound(show(anchor)));
    }
    // 要补出来的那几层键，从锚点往下数
    let mut keys = Vec::with_capacity(path.len() - depth);
    for step in &path[depth..] {
        match step {
            Step::Key(k) => keys.push(k.as_str()),
            // 列表项凭空造不出来 —— 那是 append 的事
            Step::Index(_) => return Err(PatchError::NotFound(show(path))),
        }
    }

    // **行内写法（`{ a: 1, b: 2 }`）要插在花括号里面。**按块式在下一行
    // 插，产出的是一份解析不了的 YAML —— 护栏会拦住，但那时用户看到的
    // 是「这是个 bug，请贴到 issue 里」，而他只是用了一种完全合法的写法。
    // 格式保留语料里本来就列了「流式与块式混排」。
    if text[a.bytes.start..].starts_with('{') {
        // 行内映射里再补一层块式的中间键，写不出来
        if keys.len() > 1 {
            return Err(PatchError::NotFound(show(&path[..path.len() - 1])));
        }
        let key = keys[0];
        let close = flow_end(text, a.bytes.start)
            .ok_or_else(|| PatchError::NotFound(format!("{}（行内映射没收尾）", show(anchor))))?;
        let inner = text[a.bytes.start + 1..close].trim();
        let piece = if inner.is_empty() {
            format!("{key}: {rendered}")
        } else {
            // 抄已有的逗号风格：`{a: 1, b: 2}` 和 `{a: 1,b: 2}` 都有人
            // 写，跟着来比统一成我们的偏好更不打扰 —— 这是他的文件
            let spaced = text[a.bytes.start..close].contains(", ");
            format!("{}{key}: {rendered}", if spaced { ", " } else { "," })
        };
        // 收尾的 `}` 前面有空格（`{ a: 1 }`）就插在那个空格之前
        let mut at = close;
        while at > a.bytes.start + 1 && text.as_bytes()[at - 1] == b' ' {
            at -= 1;
        }
        let mut out = String::with_capacity(text.len() + piece.len());
        out.push_str(&text[..at]);
        out.push_str(&piece);
        out.push_str(&text[at..]);
        return Ok((out, 1));
    }

    let (end, indent) =
        block_of(text, all, anchor).ok_or_else(|| PatchError::NotFound(show(anchor)))?;
    let mut piece = String::new();
    for (i, k) in keys.iter().enumerate() {
        piece.push('\n');
        piece.push_str(&indent);
        for _ in 0..i {
            piece.push_str("  ");
        }
        piece.push_str(k);
        piece.push(':');
        if i + 1 == keys.len() {
            piece.push(' ');
            piece.push_str(rendered);
        }
    }
    let mut out = String::with_capacity(text.len() + piece.len());
    out.push_str(&text[..end]);
    out.push_str(&piece);
    out.push_str(&text[end..]);
    Ok((out, keys.len()))
}

/// 往映射里**插一个新的标量键**。
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
    let all = nodes(text)?;
    let rendered = render_scalar(value, ScalarStyle::Plain);
    // 这个位置已经是个列表或映射 —— 整块换掉，而不是在旁边再写一个
    // 同名键。`find` 只认标量，所以走到这儿的容器看着像「还没有」。
    if let Some(n) = all.iter().find(|n| n.path == path) {
        if n.anchored {
            return Err(PatchError::AnchorOrAlias(show(path)));
        }
        let gone = all
            .iter()
            .filter(|x| x.path.len() > path.len() && x.path.starts_with(path))
            .count();
        let out = overwrite_block(text, &all, n, &rendered)?;
        return checked(out, path, value, all.len() - gone);
    }
    let (out, added) = splice_key(text, &all, path, &rendered)?;
    checked(out, path, value, all.len() + added)
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
fn checked(
    out: String,
    path: &[Step],
    value: &Scalar,
    expected: usize,
) -> Result<String, PatchError> {
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
    let after_all = nodes(&out)?;
    if after_all.len() != expected {
        return Err(PatchError::SelfCheck(format!(
            "改完之后有 {} 个节点，应该是 {expected} 个",
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
    // **结尾的空行退回去。**上面那个循环跳过空行，是为了让「项里夹着
    // 空行」不被误判成这一项结束了；代价是 `at` 会停在那些空行的后面。
    // 不退的话，删掉最后一项会连用户分隔两个段落的那个空行一起删掉 ——
    // 一次删一行，几次之后整份配置就挤成一坨了。
    let mut cut = 0usize;
    let mut pos = 0usize;
    for line in text[line_start..at].split_inclusive('\n') {
        if !line.trim().is_empty() {
            cut = pos + line.len();
        }
        pos += line.len();
    }
    let at = line_start + cut.max(1);
    // 回退掉结尾那个换行。**只回退一个** —— 再往前是这一项的内容，
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
    let count = count_items(&all, seq_path);
    if count == 0 {
        // **第一项要单独处理，而且这条路径是常走的**：`routes`、`proxies`、
        // `allow_from` 这些键在配置里默认根本不写（「第一天的配置是六行」），
        // 所以「加第一条路由」「加第一个代理」都从这儿过。不支持它的话，
        // 界面上每一个新建功能都在用户第一次用的时候失败。
        return append_first(text, seq_path, item);
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

/// 往一个还不存在、或者空着的列表里加第一项。
///
/// 三种起点，产出的都是块式列表：
///
/// · 键根本没写 —— 在父映射末尾补 `key:` 和第一项
/// · `key: []` —— 换成块式写法
/// · `key:` 后面空着 —— 直接补第一项
fn append_first(text: &str, seq_path: &[Step], item: &str) -> Result<String, PatchError> {
    let Some((last, parent)) = seq_path.split_last() else {
        return Err(PatchError::NotFound(show(seq_path)));
    };
    let Step::Key(key) = last else {
        return Err(PatchError::NotFound(show(seq_path)));
    };
    let all = nodes(text)?;
    let parent_node = all
        .iter()
        .find(|n| n.path == parent)
        .ok_or_else(|| PatchError::NotFound(show(parent)))?;
    if !matches!(parent_node.kind, NodeKind::Map) || parent_node.anchored {
        return Err(PatchError::NotFound(show(parent)));
    }

    let (end, indent) =
        block_of(text, &all, parent).ok_or_else(|| PatchError::NotFound(show(parent)))?;

    let existing = all.iter().find(|n| n.path == seq_path);
    let piece = {
        let cont = format!("{indent}    ");
        let mut out = String::new();
        for (i, line) in item.lines().enumerate() {
            if i > 0 {
                out.push('\n');
                out.push_str(&cont);
            } else {
                out.push_str(&indent);
                out.push_str("  - ");
            }
            out.push_str(line);
        }
        out
    };

    let mut out = String::with_capacity(text.len() + piece.len() + key.len() + 4);
    match existing {
        // `key: []` 或者 `key:` 空着 —— 换掉那一行的值部分
        Some(n) => {
            let vline_start = text[..n.bytes.start]
                .rfind('\n')
                .map(|i| i + 1)
                .unwrap_or(0);
            let vline_end = text[vline_start..]
                .find('\n')
                .map(|i| vline_start + i)
                .unwrap_or(text.len());
            let vindent: String = text[vline_start..]
                .chars()
                .take_while(|c| *c == ' ')
                .collect();
            let piece = piece.replacen(&indent, &vindent, 1);
            out.push_str(&text[..vline_start]);
            out.push_str(&vindent);
            out.push_str(key);
            out.push_str(":\n");
            out.push_str(&piece);
            out.push_str(&text[vline_end..]);
        }
        // 键根本没写 —— 在这一块末尾补一整段
        None => {
            out.push_str(&text[..end]);
            out.push('\n');
            out.push_str(&indent);
            out.push_str(key);
            out.push_str(":\n");
            out.push_str(&piece);
            out.push_str(&text[end..]);
        }
    }
    checked_structural(text, out, seq_path, 1)
}

/// 从块式列表里删掉第 `index` 项。
pub fn remove(text: &str, seq_path: &[Step], index: usize) -> Result<String, PatchError> {
    let span = item_span(text, seq_path, index)?;
    let all = nodes(text)?;
    let count = count_items(&all, seq_path);
    // 连同它后面那个换行一起删，否则会留下一个空行
    let mut end = span.end;
    if end < text.len() && text.as_bytes()[end] == b'\n' {
        end += 1;
    }
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..span.start]);
    out.push_str(&text[end..]);
    // **删掉最后一项，剩下的是 `key:` —— 那是 null，不是空列表。**
    // 对 `allow` 来说这两者分别是「跟客户端方言走」和「一个都不给」，
    // 于是「删掉最后一条白名单」会静默地变成不设防。所以收尾写成 `[]`。
    // 对 `routes` / `proxies` 这些两者等价的键，顺带也省掉一个悬着的键。
    if count == 1 {
        let after = nodes(&out)?;
        if let Some(n) = after
            .iter()
            .find(|n| n.path == seq_path && matches!(n.kind, NodeKind::Scalar { .. }))
        {
            out = put_empty_seq(&out, &n.bytes);
        }
    }
    checked_structural(text, out, seq_path, count.saturating_sub(1))
}

/// 把一个列表清成 `key: []`。
///
/// **`[]` 和「这个键干脆不写」是两回事**，所以这不是「一项项删到没有」
/// 的同义词：`allow` 不写 = 跟客户端方言走，`allow: []` = 一个都不给。
/// 界面上那是两个不同的选项，协议里就得有两条不同的路。
///
/// 已有的项一项项删 —— 注释跟着各自那一项走，这是 `remove` 已经做对的
/// 事，没必要在这儿把整块铲平。
pub fn clear_seq(text: &str, seq_path: &[Step]) -> Result<String, PatchError> {
    let mut cur = text.to_string();
    loop {
        let n = count_items(&nodes(&cur)?, seq_path);
        if n == 0 {
            break;
        }
        cur = remove(&cur, seq_path, n - 1)?;
    }
    let all = nodes(&cur)?;
    let out = match all.iter().find(|n| n.path == seq_path) {
        Some(n) if n.anchored => return Err(PatchError::AnchorOrAlias(show(seq_path))),
        // 空值（`allow:`），或者别的什么标量 —— 换成 `[]`
        Some(n) if matches!(n.kind, NodeKind::Scalar { .. }) => put_empty_seq(&cur, &n.bytes),
        // 本来就是空列表
        Some(_) => cur.clone(),
        // 这个键根本没写 —— 和 insert 一样在父映射末尾补一行
        None => splice_key(&cur, &all, seq_path, "[]")?.0,
    };
    checked_structural(&cur, out, seq_path, 0)
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
    let got = count_items(&after, seq_path);
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

    /// **这四条是「第一次用」的全部路径。**`routes`、`proxies`、
    /// `allow_from` 这些键在配置里默认根本不写（第一天的配置只有六行），
    /// 所以「建第一条路由」「加第一个代理」都从这里过 —— 不支持的话，
    /// 界面上每个新建功能都在用户第一次点它的时候失败。
    #[test]
    fn a_key_that_is_not_in_the_file_yet_gets_created_with_its_first_entry() {
        let c = "version: 1\nclients:\n  - name: a\n    key: tw-a\n";
        let out = append(
            c,
            &[Step::Key("routes".into())],
            "name: 默认\nrules:\n  - name: 兜底\n    to: official",
        )
        .unwrap();
        let cfg: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        let rs = cfg["routes"].as_sequence().unwrap();
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0]["name"].as_str(), Some("默认"));
        assert_eq!(rs[0]["rules"].as_sequence().unwrap().len(), 1);
        // 原来的内容一个字没动
        assert!(out.contains("- name: a\n    key: tw-a"), "{out}");
    }

    #[test]
    fn a_flow_empty_list_becomes_a_block_list() {
        let c = "version: 1\nproxies: []\n";
        let out = append(
            c,
            &[Step::Key("proxies".into())],
            "name: 翻墙\ntype: socks5h\naddr: 127.0.0.1:1080",
        )
        .unwrap();
        let cfg: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(cfg["proxies"].as_sequence().unwrap().len(), 1);
        assert!(!out.contains("[]"), "流式的空列表该被换掉：{out}");
    }

    #[test]
    fn creating_a_key_keeps_the_comments_around_it() {
        let c =
            "version: 1\n# 这台机器上的上游\nproviders:\n  - name: 官方\n    base_url: https://x\n";
        let out = append(
            c,
            &[Step::Key("proxies".into())],
            "name: p\naddr: 1.2.3.4:1080",
        )
        .unwrap();
        assert!(out.contains("# 这台机器上的上游"), "{out}");
        let cfg: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(cfg["providers"].as_sequence().unwrap().len(), 1);
        assert_eq!(cfg["proxies"].as_sequence().unwrap().len(), 1);
    }

    #[test]
    fn a_nested_key_that_does_not_exist_yet_works_too() {
        // `clients[].allow` 也是默认不写的 —— 「限制这把密钥能看的模型」
        // 第一次点也走这条路
        let c = "clients:\n  - name: a\n    key: tw-a\n";
        let out = append(
            c,
            &[
                Step::Key("clients".into()),
                Step::Index(0),
                Step::Key("allow".into()),
            ],
            "claude-haiku-*",
        )
        .unwrap();
        let cfg: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(cfg["clients"][0]["allow"].as_sequence().unwrap().len(), 1);
    }
}

/// 界面上每一个「第一次」都从这儿过：第一条路由、第一个代理、第一次动
/// 一整段默认不写的设置、把白名单收成一个都不给。这些路径在真机上一条
/// 条炸过 —— 它们共同的性质是**默认配置里不存在**，所以只写「改一个已
/// 经在的值」的测试永远照不到。
#[cfg(test)]
mod first_write_tests {
    use super::*;

    /// 尾部是块式列表的文件，往**顶层**插一个键。
    ///
    /// 容器节点的 `bytes` 是内容起点上的一个空区间，照它算位置会把
    /// `default_route:` 插进 `routes` 的列表项中间，产出一份解析不了的
    /// YAML。真机上「把默认路由换成自定义那条」就是这么失败的。
    #[test]
    fn a_top_level_key_lands_at_the_top_level_not_inside_the_last_list() {
        let cfg = "version: 1\nclients:\n  - name: demo\n    key: tw-a\nroutes:\n  - name: 长文\n    rules:\n      - name: 兜底\n        to: relay\n";
        let out = insert(cfg, &[Step::key("default_route")], &Scalar::s("长文")).unwrap();
        let back: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(back["default_route"].as_str(), Some("长文"));
        assert_eq!(back["routes"].as_sequence().unwrap().len(), 1);
        assert_eq!(back["routes"][0]["rules"].as_sequence().unwrap().len(), 1);
        // 顶格，不是缩在列表里
        assert!(out.contains("\ndefault_route: 长文"), "{out}");
    }

    /// 一整段还没写过的设置，改它里面的某一项。
    ///
    /// `limits`、`client_probes` 这些默认根本不在文件里，「改里面某一
    /// 项」就是它们的第一次写入。父节点必须先存在的话，这些设置在界面
    /// 上从头到尾是死的。
    #[test]
    fn writing_into_a_section_that_does_not_exist_yet_creates_it() {
        let cfg = "version: 1\nlisten:\n  gateway:\n    port: 18780\n";
        let out = insert(
            cfg,
            &[Step::key("limits"), Step::key("max_body_bytes")],
            &Scalar::Int(8_388_608),
        )
        .unwrap();
        let back: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(back["limits"]["max_body_bytes"].as_i64(), Some(8_388_608));
        // 原来的东西一个没动
        assert_eq!(back["listen"]["gateway"]["port"].as_i64(), Some(18780));
        assert!(
            out.contains("\nlimits:\n  max_body_bytes: 8388608"),
            "{out}"
        );
    }

    #[test]
    fn a_missing_section_two_levels_down_gets_both_levels() {
        let cfg = "version: 1\n";
        let out = insert(
            cfg,
            &[
                Step::key("client_probes"),
                Step::key("claude-code"),
                Step::key("mode"),
            ],
            &Scalar::s("off"),
        )
        .unwrap();
        let back: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(
            back["client_probes"]["claude-code"]["mode"].as_str(),
            Some("off")
        );
    }

    /// `key:` 后面空着的时候改它的值。
    ///
    /// 那种键的值区间是空的、紧贴在冒号后面，照直写进去得到的是
    /// `key:x` —— 一个名叫 `key:x` 的标量。
    #[test]
    fn setting_a_key_whose_value_is_blank_does_not_glue_it_to_the_colon() {
        let cfg = "version: 1\nnote:\n";
        let out = set(cfg, &[Step::key("note")], &Scalar::s("hi")).unwrap();
        assert!(out.contains("note: hi"), "{out}");
        let back: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(back["note"].as_str(), Some("hi"));
    }

    const ALLOW: &str = "clients:\n  - name: demo\n    key: tw-a\n    allow:\n      # 只给这些\n      - claude-*\n      - gpt-*\n";

    fn allow_path() -> Vec<Step> {
        vec![Step::key("clients"), Step::Index(0), Step::key("allow")]
    }

    /// **「一个都不给」和「这个键不写」是两个不同的选项。**
    #[test]
    fn clearing_a_list_leaves_an_empty_list_not_a_missing_key() {
        let out = clear_seq(ALLOW, &allow_path()).unwrap();
        let back: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        let allow = &back["clients"][0]["allow"];
        assert!(
            allow.is_sequence(),
            "读回来应该是个列表，实际是 {allow:?}\n{out}"
        );
        assert_eq!(allow.as_sequence().unwrap().len(), 0, "{out}");
        // 用户写的注释还在
        assert!(out.contains("# 只给这些"), "{out}");
    }

    #[test]
    fn clearing_a_list_that_was_never_written_creates_it_empty() {
        let cfg = "clients:\n  - name: demo\n    key: tw-a\n";
        let out = clear_seq(cfg, &allow_path()).unwrap();
        let back: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(
            back["clients"][0]["allow"].as_sequence().map(|s| s.len()),
            Some(0),
            "{out}"
        );
        assert_eq!(back["clients"][0]["key"].as_str(), Some("tw-a"));
    }

    #[test]
    fn clearing_an_already_empty_list_is_a_no_op() {
        let cfg = "clients:\n  - name: demo\n    allow: []\n";
        let out = clear_seq(cfg, &allow_path()).unwrap();
        assert_eq!(out, cfg);
    }

    /// 删掉最后一条白名单 = 一个都不给，**不是**退回「跟方言走」。
    #[test]
    fn removing_the_last_entry_leaves_an_empty_list_not_a_null() {
        let one = remove(ALLOW, &allow_path(), 1).unwrap();
        let out = remove(&one, &allow_path(), 0).unwrap();
        let back: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        let allow = &back["clients"][0]["allow"];
        assert!(!allow.is_null(), "删空之后成了 null：\n{out}");
        assert_eq!(allow.as_sequence().unwrap().len(), 0, "{out}");
    }

    /// 清空之后还能再加回去 —— `[]` 得能变回块式列表。
    #[test]
    fn a_cleared_list_can_be_appended_to_again() {
        let empty = clear_seq(ALLOW, &allow_path()).unwrap();
        let out = append(&empty, &allow_path(), "claude-3-5-*").unwrap();
        let back: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(
            back["clients"][0]["allow"].as_sequence().unwrap().len(),
            1,
            "{out}"
        );
    }
}

/// 可选字段的**回头路**。
///
/// 设过的东西要能撤回去,不然界面上每个可选字段都是一扇单向门:`allow`
/// 一旦限定就永远限定,`default_route` 一旦指定就再也回不到那条隐式的
/// 默认路由。撤回写的是 `~` —— 对 `Option<T>` 来说和「这个键不写」
/// 是同一个值。
#[cfg(test)]
mod undo_tests {
    use super::*;

    fn allow() -> Vec<Step> {
        vec![Step::key("clients"), Step::Index(0), Step::key("allow")]
    }

    fn null_out(text: &str, path: &[Step]) -> String {
        insert(text, path, &Scalar::Null).unwrap()
    }

    #[test]
    fn nulling_a_block_list_replaces_it_instead_of_writing_a_second_key() {
        let cfg = "clients:\n  - name: demo\n    allow:\n      - claude-*\n      - gpt-*\n    key: tw-a\n";
        let out = null_out(cfg, &allow());
        assert_eq!(out.matches("allow").count(), 1, "同名键写了两遍：\n{out}");
        let back: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        assert!(back["clients"][0]["allow"].is_null(), "{out}");
        // 它后面的键还在原位
        assert_eq!(back["clients"][0]["key"].as_str(), Some("tw-a"), "{out}");
    }

    #[test]
    fn nulling_an_empty_flow_list_stays_on_its_own_line() {
        let cfg = "clients:\n  - name: demo\n    allow: []\n    key: tw-a\n";
        let out = null_out(cfg, &allow());
        assert!(out.contains("allow: ~"), "{out}");
        let back: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(back["clients"][0]["key"].as_str(), Some("tw-a"), "{out}");
    }

    /// 键和值之间夹着注释,而注释里有冒号。
    ///
    /// 往回找冒号时照直 `rfind` 会找到 `https://` 那个,把值切在注释
    /// 中间。
    #[test]
    fn a_comment_with_a_colon_between_key_and_value_does_not_confuse_it() {
        let cfg = "clients:\n  - name: demo\n    allow:\n      # 见 https://example.invalid/x\n      - claude-*\n";
        let out = null_out(cfg, &allow());
        let back: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        assert!(back["clients"][0]["allow"].is_null(), "{out}");
        assert_eq!(back["clients"][0]["name"].as_str(), Some("demo"), "{out}");
    }

    /// 用户用来分段的空行不归上一项所有。
    ///
    /// 不然「删掉最后一条」会连那个空行一起删 —— 一次删一行,几次之后
    /// 整份配置就挤成一坨了,而没有任何一次改动看起来是错的。
    #[test]
    fn removing_the_last_entry_keeps_the_blank_line_that_separates_sections() {
        let cfg = "clients:\n  - name: demo\n    allow:\n      - claude-*\n\nproviders:\n  - name: relay\n";
        let out = remove(cfg, &allow(), 0).unwrap();
        assert!(
            out.contains("\n\nproviders:"),
            "分段的空行被吃掉了：\n{out}"
        );
    }

    /// 撤回之后还能再设回来 —— 这扇门两边都要能走。
    #[test]
    fn a_nulled_field_can_be_filled_in_again() {
        let cfg = "clients:\n  - name: demo\n    allow:\n      - claude-*\n";
        let empty = null_out(cfg, &allow());
        let out = append(&empty, &allow(), "gpt-*").unwrap();
        let back: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(
            back["clients"][0]["allow"].as_sequence().unwrap().len(),
            1,
            "{out}"
        );
    }

    /// 顶层的可选标量:`default_route` 撤回到那条隐式的默认路由。
    #[test]
    fn a_top_level_optional_scalar_can_be_unset() {
        let cfg = "version: 1\ndefault_route: 长文\nroutes:\n  - name: 长文\n";
        let out = null_out(cfg, &[Step::key("default_route")]);
        let back: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        assert!(back["default_route"].is_null(), "{out}");
        assert_eq!(back["routes"].as_sequence().unwrap().len(), 1, "{out}");
    }
}
