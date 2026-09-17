//! 结构性写入：换掉一个键的整个值、删掉一个键、原地换掉列表里的一项。
//!
//! # 为什么要有这一层
//!
//! `set` / `insert` 只写标量，`append` / `remove` 只增删整项。而一个
//! 「在对话框里编辑上游」的保存，是这几件事的混合：改几个标量、补一段
//! 嵌套的值（OAuth 凭据、脱敏类别）、删掉用户清空了的可选字段。以前界面
//! 只能拿字符串拼一段 YAML 丢进 `append`，一个带 `#` 的密码就能写坏整份
//! 配置。
//!
//! # 和其余部分同一条纪律
//!
//! 只动目标那一段字节。**每一次写完都在内存里重新解析，核对两件事**：
//!
//! 1. 目标位置读回来的结构和要写的那段完全一致；
//! 2. **目标之外的每一个节点**（路径、类型、标量值）和写之前一模一样。
//!
//! 第二条比第一条值钱 —— 真正的灾难是「目标对了，同时把别处改坏了」。

use super::*;

/// 要写进去的值，**已经渲染好的 YAML 文本**。
///
/// 调用方负责渲染（引号、转义）—— 这一层不知道值的类型，只保证它落在
/// 该在的位置、并且读回来和给的一样。
#[derive(Debug, Clone, Copy)]
pub enum Put<'a> {
    /// 单行：`0.8`、`'sk-a#b'`、`[]`、`{ a: 1 }`
    Inline(&'a str),
    /// 多行块，**零缩进**：`input: 3\noutput: 15`，或 `- a\n- b`
    Block(&'a str),
}

impl Put<'_> {
    fn text(&self) -> &str {
        match self {
            Put::Inline(s) | Put::Block(s) => s,
        }
    }
}

/// 把 `path` 这个键的值换成 `value`。键不存在就写进去，**缺的中间层一起
/// 补出来**；已经存在就整个换掉，不管原来是标量、块式还是行内的容器。
///
/// 原值那一行的行尾注释会留下。原值内部（块里面）的注释跟着旧值一起走 ——
/// 它们注释的是已经不存在的东西。
pub fn put(text: &str, path: &[Step], value: Put<'_>) -> Result<String, PatchError> {
    let Some(Step::Key(_)) = path.last() else {
        return Err(PatchError::NotFound(show(path)));
    };
    let frag = nodes(value.text())
        .map_err(|e| PatchError::Parse(format!("要写进 `{}` 的值本身解析不了：{e}", show(path))))?;
    if matches!(value, Put::Inline(s) if s.contains('\n')) {
        return Err(PatchError::Parse(format!(
            "写进 `{}` 的单行值里有换行",
            show(path)
        )));
    }
    let before = nodes(text)?;
    let mut hits = before.iter().filter(|n| n.path == path);
    let hit = hits.next();
    if hits.next().is_some() {
        return Err(PatchError::Duplicate(show(path)));
    }
    let out = match hit {
        Some(n) => replace_value(text, &before, n, value)?,
        None => add_key(text, &before, path, value)?,
    };
    let after = nodes(&out)
        .map_err(|e| PatchError::SelfCheck(format!("写完 `{}` 之后解析不了：{e}", show(path))))?;
    untouched_outside(&before, &after, path)?;
    lands_as(&after, path, &frag)?;
    Ok(out)
}

/// 删掉 `path` 这个键，连同它的值。
///
/// **删空了的父映射一起删。**`pricing: { auto_update: false }` 退回默认
/// 值之后，留下一个空的 `pricing:` 读回来是 `null` —— 对一个结构体字段
/// 来说那是解析错误，不是「没写」。
///
/// 只认块式映射里的键。行内映射（`{ a: 1, b: 2 }`）里的键删不了，调用方
/// 要么整项换掉，要么让用户自己改。
pub fn remove_key(text: &str, path: &[Step]) -> Result<String, PatchError> {
    let Some((Step::Key(_), parent)) = path.split_last() else {
        return Err(PatchError::NotFound(show(path)));
    };
    let before = nodes(text)?;
    let mut hits = before.iter().filter(|n| n.path == path);
    let Some(node) = hits.next() else {
        return Err(PatchError::NotFound(show(path)));
    };
    if hits.next().is_some() {
        return Err(PatchError::Duplicate(show(path)));
    }
    if node.anchored {
        return Err(PatchError::AnchorOrAlias(show(path)));
    }
    let parent_node = before
        .iter()
        .find(|n| n.path == parent)
        .ok_or_else(|| PatchError::NotFound(show(parent)))?;
    if parent_node.anchored {
        return Err(PatchError::AnchorOrAlias(show(parent)));
    }
    if !matches!(parent_node.kind, NodeKind::Map) || is_flow(text, parent_node) {
        return Err(PatchError::NotFound(format!(
            "{}（行内映射里的键只能整段改写）",
            show(path)
        )));
    }
    let siblings = before
        .iter()
        .filter(|n| n.path.len() == path.len() && n.path.starts_with(parent))
        .count();
    // 父映射只剩这一个键，而父映射本身是某个键的值 —— 连父一起删。
    if siblings == 1 {
        match parent.last() {
            Some(Step::Key(_)) => return remove_key(text, parent),
            // 列表里的一项只剩这一个键：删掉它，那一项就成了 `null`，
            // 而不是一个空映射 —— 整项删是 `remove` 的事
            Some(Step::Index(_)) => {
                return Err(PatchError::NotFound(format!(
                    "{}（这是那一项里唯一的键，要删就删整项）",
                    show(path)
                )));
            }
            None => {}
        }
    }

    let colon = key_colon(text, node).ok_or_else(|| PatchError::NotFound(show(path)))?;
    let line_start = text[..colon].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let value_end = line_end(text, value_end(text, &before, node)?);
    let head = &text[line_start..colon];
    let out = if head.trim_start().starts_with("- ") || head.trim_start() == "-" {
        // 列表项的第一个键写在 `- ` 那一行上。删掉它，那一行只剩 `-`，
        // 其余的键原样留在下面 —— `-` 后面换行再接缩进的映射是合法的
        // 同一项。
        let dash = line_start + head.find('-').unwrap_or(0);
        let mut out = String::with_capacity(text.len());
        out.push_str(&text[..dash + 1]);
        out.push_str(&text[value_end..]);
        out
    } else {
        // 连同行尾的换行一起删，否则留下一个空行
        let end = if value_end < text.len() {
            value_end + 1
        } else {
            value_end
        };
        let mut out = String::with_capacity(text.len());
        out.push_str(&text[..line_start]);
        out.push_str(&text[end..]);
        out
    };

    let after = nodes(&out)
        .map_err(|e| PatchError::SelfCheck(format!("删掉 `{}` 之后解析不了：{e}", show(path))))?;
    if after.iter().any(|n| n.path == path) {
        return Err(PatchError::SelfCheck(format!(
            "删完之后 `{}` 还在",
            show(path)
        )));
    }
    untouched_outside(&before, &after, path)?;
    Ok(out)
}

/// 原地换掉列表的第 `index` 项。**位置不变**，缩进抄原来那一项的。
///
/// 给「这一项原来是行内写法」那种情况用：`- { name: a, key: b }` 里的键
/// 删不了、嵌套的值也塞不进去，整项换成块式是唯一不猜的做法。
pub fn replace_item(
    text: &str,
    seq_path: &[Step],
    index: usize,
    item: &str,
) -> Result<String, PatchError> {
    let frag = nodes(item)?;
    let before = nodes(text)?;
    let count = count_items(&before, seq_path);
    let span = item_span(text, seq_path, index)?;
    let dash = text[span.start..span.end]
        .find('-')
        .ok_or_else(|| PatchError::NotFound(show(seq_path)))?;
    let indent = " ".repeat(dash);
    let mut piece = String::new();
    for (i, line) in item.lines().enumerate() {
        if i == 0 {
            piece.push_str(&indent);
            piece.push_str("- ");
        } else {
            piece.push('\n');
            if !line.is_empty() {
                piece.push_str(&indent);
                piece.push_str("  ");
            }
        }
        piece.push_str(line);
    }
    let mut out = String::with_capacity(text.len() + piece.len());
    out.push_str(&text[..span.start]);
    out.push_str(&piece);
    out.push_str(&text[span.end..]);

    let mut path = seq_path.to_vec();
    path.push(Step::Index(index));
    let after = nodes(&out)
        .map_err(|e| PatchError::SelfCheck(format!("换掉 `{}` 之后解析不了：{e}", show(&path))))?;
    if count_items(&after, seq_path) != count {
        return Err(PatchError::SelfCheck(format!(
            "换完之后 `{}` 的项数变了",
            show(seq_path)
        )));
    }
    untouched_outside(&before, &after, &path)?;
    lands_as(&after, &path, &frag)?;
    Ok(out)
}

/// 这个位置上的容器是行内写法（`{…}` / `[…]`）吗。不存在或者不是容器
/// 时是 `false`。
///
/// 给资源层判断「这一项能不能按字段改」：行内映射里的键删不了，只能整项
/// 换成块式。
pub fn is_flow_at(text: &str, path: &[Step]) -> Result<bool, PatchError> {
    let all = nodes(text)?;
    Ok(all
        .iter()
        .find(|n| n.path == path)
        .is_some_and(|n| is_flow(text, n)))
}

/// 一个零缩进的单项块式列表：`- 第一行\n  其余行`。
pub(crate) fn one_item_seq(item: &str) -> String {
    let mut out = String::new();
    for (i, line) in item.lines().enumerate() {
        if i == 0 {
            out.push_str("- ");
        } else {
            out.push('\n');
            if !line.is_empty() {
                out.push_str("  ");
            }
        }
        out.push_str(line);
    }
    out
}

// ─────────────────────────────────────────────────────────── 内部

fn replace_value(
    text: &str,
    all: &[Node],
    node: &Node,
    value: Put<'_>,
) -> Result<String, PatchError> {
    if node.anchored || matches!(node.kind, NodeKind::Alias) {
        return Err(PatchError::AnchorOrAlias(show(&node.path)));
    }
    let colon = key_colon(text, node).ok_or_else(|| PatchError::NotFound(show(&node.path)))?;
    // 值从哪里开始算：单行标量就是它自己那一段（保住冒号后面的空格
    // 风格和行尾注释），其余一律从冒号后面算起
    let (start, end) = match &node.kind {
        NodeKind::Scalar { style, .. }
            if !matches!(style, ScalarStyle::Literal | ScalarStyle::Folded) =>
        {
            let slot = value_slot(text, &node.bytes);
            (slot.start.max(colon), slot.end.max(colon))
        }
        _ => (colon, value_end(text, all, node)?),
    };
    let same_line_end = line_end(text, end);
    // 值后面、同一行上还剩什么（通常是注释）
    let trailing = &text[end..same_line_end];
    let mut out = String::with_capacity(text.len() + value.text().len() + 16);
    match value {
        Put::Inline(s) => {
            out.push_str(&text[..start]);
            if start == colon {
                out.push(' ');
            }
            out.push_str(s);
            out.push_str(&text[end..]);
        }
        Put::Block(s) => {
            let indent = " ".repeat(key_column(text, colon) + 2);
            out.push_str(&text[..colon]);
            // 行尾注释跟着键走：`key:  # 说明` 后面接块
            if !trailing.trim().is_empty() {
                out.push_str(trailing);
            }
            push_block(&mut out, s, &indent);
            out.push_str(&text[same_line_end..]);
        }
    }
    Ok(out)
}

fn add_key(text: &str, all: &[Node], path: &[Step], value: Put<'_>) -> Result<String, PatchError> {
    // 最深的那个已经存在、而且能往里写的祖先。根一定在。
    let mut depth = path.len() - 1;
    while depth > 0 {
        match all.iter().find(|n| n.path == path[..depth]) {
            Some(n) if n.anchored => return Err(PatchError::AnchorOrAlias(show(&path[..depth]))),
            Some(n) if matches!(n.kind, NodeKind::Map) => break,
            // 存在但不是映射 —— 往里写字段是在改它的类型，不干
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
    let mut keys = Vec::with_capacity(path.len() - depth);
    for step in &path[depth..] {
        match step {
            Step::Key(k) => keys.push(k.as_str()),
            // 列表项凭空造不出来 —— 那是 append 的事
            Step::Index(_) => return Err(PatchError::NotFound(show(path))),
        }
    }

    // **行内写法（`{ a: 1, b: 2 }`）要插在花括号里面。**按块式在下一行
    // 插，产出的是一份解析不了的 YAML。
    if is_flow(text, a) {
        let (1, Put::Inline(rendered)) = (keys.len(), value) else {
            return Err(PatchError::NotFound(format!(
                "{}（行内映射里写不下多层或多行的值）",
                show(path)
            )));
        };
        let key = keys[0];
        let close = flow_end(text, a.bytes.start)
            .ok_or_else(|| PatchError::NotFound(format!("{}（行内映射没收尾）", show(anchor))))?;
        let inner = text[a.bytes.start + 1..close].trim();
        let piece = if inner.is_empty() {
            format!("{key}: {rendered}")
        } else {
            // 抄已有的逗号风格：`{a: 1, b: 2}` 和 `{a: 1,b: 2}` 都有人写
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
        return Ok(out);
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
    }
    match value {
        Put::Inline(s) => {
            piece.push(' ');
            piece.push_str(s);
        }
        Put::Block(s) => {
            let deeper = format!("{indent}{}", "  ".repeat(keys.len()));
            push_block(&mut piece, s, &deeper);
        }
    }
    let mut out = String::with_capacity(text.len() + piece.len());
    out.push_str(&text[..end]);
    out.push_str(&piece);
    out.push_str(&text[end..]);
    Ok(out)
}

/// 把一段零缩进的块逐行缩进后接在 `out` 后面，每行前面换行。
fn push_block(out: &mut String, block: &str, indent: &str) {
    for line in block.lines() {
        out.push('\n');
        if !line.is_empty() {
            out.push_str(indent);
        }
        out.push_str(line);
    }
}

/// 这个节点是行内写法（`{…}` / `[…]`）吗。
fn is_flow(text: &str, n: &Node) -> bool {
    matches!(n.kind, NodeKind::Map | NodeKind::Seq)
        && matches!(
            text[n.bytes.start.min(text.len())..].chars().next(),
            Some('{' | '[')
        )
}

/// 引出这个值的冒号的位置（冒号本身的下一个字节）。
fn key_colon(text: &str, n: &Node) -> Option<usize> {
    after_key_colon(text, n.bytes.start)
}

/// 键在第几列。`- key:` 那种写法里，键在短划线后面。
fn key_column(text: &str, colon: usize) -> usize {
    let line_start = text[..colon].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line = &text[line_start..colon];
    let trimmed = line.trim_start();
    let mut col = line.len() - trimmed.len();
    if let Some(rest) = trimmed.strip_prefix('-') {
        let after = rest.trim_start();
        col += 1 + (rest.len() - after.len());
    }
    col
}

/// 一个值在原文里结束的位置（最后一个字节之后）。
fn value_end(text: &str, all: &[Node], n: &Node) -> Result<usize, PatchError> {
    match &n.kind {
        NodeKind::Scalar { .. } | NodeKind::Alias => Ok(n.bytes.end.min(text.len())),
        NodeKind::Map | NodeKind::Seq if is_flow(text, n) => flow_end(text, n.bytes.start)
            .map(|i| i + 1)
            .ok_or_else(|| PatchError::NotFound(format!("{}（行内写法没收尾）", show(&n.path)))),
        NodeKind::Map | NodeKind::Seq => block_of(text, all, &n.path)
            .map(|(end, _)| end)
            .ok_or_else(|| PatchError::NotFound(show(&n.path))),
    }
}

/// `at` 所在那一行的行尾（换行符的位置，或文件末尾）。
fn line_end(text: &str, at: usize) -> usize {
    text[at..].find('\n').map(|i| at + i).unwrap_or(text.len())
}

/// 节点的形状，不含位置。护栏拿它比「变了没有」。
#[derive(Debug, PartialEq)]
enum Shape<'a> {
    Scalar(&'a str),
    Map,
    Seq,
    Alias,
}

fn shape(k: &NodeKind) -> Shape<'_> {
    match k {
        NodeKind::Scalar { value, .. } => Shape::Scalar(value),
        NodeKind::Map => Shape::Map,
        NodeKind::Seq => Shape::Seq,
        NodeKind::Alias => Shape::Alias,
    }
}

/// **目标之外的节点一个都不能变。**
///
/// 「之外」不含目标的祖先：补出来的中间层是新的，删空了的父映射会没掉，
/// 而它们都是这次写入本身的一部分。
fn untouched_outside(before: &[Node], after: &[Node], path: &[Step]) -> Result<(), PatchError> {
    let outside = |n: &&Node| !(n.path.starts_with(path) || path.starts_with(&n.path));
    let b: Vec<_> = before
        .iter()
        .filter(outside)
        .map(|n| (&n.path, shape(&n.kind)))
        .collect();
    let a: Vec<_> = after
        .iter()
        .filter(outside)
        .map(|n| (&n.path, shape(&n.kind)))
        .collect();
    if a != b {
        let diff = b
            .iter()
            .zip(a.iter())
            .find(|(x, y)| x != y)
            .map(|(x, _)| show(x.0))
            .unwrap_or_else(|| format!("节点数 {} → {}", b.len(), a.len()));
        return Err(PatchError::SelfCheck(format!(
            "写 `{}` 的时候动到了别处（{diff}）",
            show(path)
        )));
    }
    Ok(())
}

/// 写进去的东西读回来，结构和要写的那段一模一样吗。
fn lands_as(after: &[Node], path: &[Step], frag: &[Node]) -> Result<(), PatchError> {
    let got: Vec<_> = after
        .iter()
        .filter(|n| n.path.starts_with(path))
        .map(|n| (n.path[path.len()..].to_vec(), shape(&n.kind)))
        .collect();
    let want: Vec<_> = frag
        .iter()
        .map(|n| (n.path.clone(), shape(&n.kind)))
        .collect();
    if got != want {
        return Err(PatchError::SelfCheck(format!(
            "`{}` 写完读回来和要写的不一样",
            show(path)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(keys: &[&str]) -> Vec<Step> {
        keys.iter()
            .map(|k| match k.parse::<usize>() {
                Ok(i) => Step::Index(i),
                Err(_) => Step::key(*k),
            })
            .collect()
    }

    fn back(s: &str) -> serde_yaml_ng::Value {
        serde_yaml_ng::from_str(s).unwrap_or_else(|e| panic!("{e}\n{s}"))
    }

    const CFG: &str = "version: 1\n# 两家上游\nproviders:\n  - name: 官方\n    base_url: https://api.anthropic.com  # 直连\n    key: sk-a\n  - name: relay\n    base_url: https://relay.example\n    key: sk-b\n    redact: [api_keys, jwt]\n    pricing:\n      sheet: 旧\nroutes: []\n";

    #[test]
    fn a_nested_value_is_written_under_a_key_that_did_not_exist() {
        let out = put(
            CFG,
            &p(&["providers", "0", "key"]),
            Put::Block("oauth:\n  refresh: r\n  endpoint: https://a/token"),
        )
        .unwrap();
        let v = back(&out);
        assert_eq!(v["providers"][0]["key"]["oauth"]["refresh"], "r");
        assert_eq!(v["providers"][1]["key"], "sk-b");
        // 同一行的注释还在
        assert!(out.contains("https://api.anthropic.com  # 直连"), "{out}");
        assert!(out.contains("# 两家上游"), "{out}");
    }

    #[test]
    fn a_missing_section_and_its_parents_are_created_in_one_write() {
        let out = put(
            CFG,
            &p(&["pricing", "sheets"]),
            Put::Block("- name: 中转\n  multiplier: 0.8"),
        )
        .unwrap();
        let v = back(&out);
        assert_eq!(v["pricing"]["sheets"][0]["name"], "中转");
        assert_eq!(v["pricing"]["sheets"][0]["multiplier"], 0.8);
        assert_eq!(v["providers"].as_sequence().unwrap().len(), 2);
        // 顶格，不是缩进在 routes 里
        assert!(
            out.contains("\npricing:\n  sheets:\n    - name: 中转"),
            "{out}"
        );
    }

    #[test]
    fn a_flow_value_is_replaced_by_a_block_and_the_rest_stays() {
        let out = put(
            CFG,
            &p(&["providers", "1", "redact"]),
            Put::Block("- internal"),
        )
        .unwrap();
        let v = back(&out);
        assert_eq!(v["providers"][1]["redact"][0], "internal");
        assert_eq!(v["providers"][1]["redact"].as_sequence().unwrap().len(), 1);
        assert_eq!(v["providers"][1]["pricing"]["sheet"], "旧");
    }

    #[test]
    fn a_block_value_is_replaced_by_an_inline_one() {
        let out = put(CFG, &p(&["providers", "1", "pricing"]), Put::Inline("中转")).unwrap();
        let v = back(&out);
        assert_eq!(v["providers"][1]["pricing"], "中转");
        assert!(!out.contains("sheet: 旧"), "{out}");
        assert_eq!(v["routes"].as_sequence().unwrap().len(), 0);
    }

    #[test]
    fn a_scalar_keeps_its_trailing_comment() {
        let out = put(
            CFG,
            &p(&["providers", "0", "base_url"]),
            Put::Inline("https://api.anthropic.com/v2"),
        )
        .unwrap();
        assert!(
            out.contains("base_url: https://api.anthropic.com/v2  # 直连"),
            "{out}"
        );
    }

    #[test]
    fn a_value_that_would_break_the_yaml_is_refused_before_writing() {
        // 调用方渲染坏了（没加引号的冒号）—— 读回来不一样就不写
        let e = put(CFG, &p(&["providers", "0", "key"]), Put::Inline("a: b")).unwrap_err();
        assert!(matches!(e, PatchError::SelfCheck(_)), "{e:?}");
    }

    #[test]
    fn removing_an_optional_field_leaves_its_neighbours() {
        let out = remove_key(CFG, &p(&["providers", "1", "redact"])).unwrap();
        let v = back(&out);
        assert!(v["providers"][1].get("redact").is_none());
        assert_eq!(v["providers"][1]["key"], "sk-b");
        assert_eq!(v["providers"][1]["pricing"]["sheet"], "旧");
    }

    #[test]
    fn removing_the_last_key_of_a_section_removes_the_section() {
        // 留一个空的 `pricing:` 读回来是 null，对结构体字段是解析错误
        let out = remove_key(CFG, &p(&["providers", "1", "pricing", "sheet"])).unwrap();
        let v = back(&out);
        assert!(v["providers"][1].get("pricing").is_none(), "{out}");
        assert_eq!(v["providers"][1]["redact"][1], "jwt");
    }

    #[test]
    fn removing_the_key_on_the_dash_line_keeps_the_item() {
        let out = remove_key(CFG, &p(&["providers", "0", "name"])).unwrap();
        let v = back(&out);
        assert_eq!(v["providers"].as_sequence().unwrap().len(), 2);
        assert!(v["providers"][0].get("name").is_none());
        assert_eq!(v["providers"][0]["key"], "sk-a");
    }

    #[test]
    fn a_key_inside_a_flow_mapping_is_not_removed_by_guessing() {
        let cfg = "version: 1\nproviders:\n  - { name: a, key: k, protocol: anthropic }\n";
        let e = remove_key(cfg, &p(&["providers", "0", "protocol"])).unwrap_err();
        assert!(matches!(e, PatchError::NotFound(_)), "{e:?}");
    }

    #[test]
    fn a_flow_item_is_replaced_in_place_by_a_block_one() {
        let cfg = "version: 1\nproviders:\n  - { name: a, key: k, protocol: anthropic }\n  - name: b\n    key: k2\nroutes: []\n";
        let out = replace_item(
            cfg,
            &p(&["providers"]),
            0,
            "name: a\nkey: k\nproxy: hk\npricing: 中转",
        )
        .unwrap();
        let v = back(&out);
        assert_eq!(v["providers"][0]["proxy"], "hk");
        assert!(v["providers"][0].get("protocol").is_none());
        // 位置不变
        assert_eq!(v["providers"][1]["name"], "b");
        assert!(
            out.contains("  - name: a\n    key: k\n    proxy: hk\n"),
            "{out}"
        );
    }

    #[test]
    fn a_quoted_key_with_a_colon_is_found_and_replaced() {
        let cfg = "version: 1\npricing:\n  sheets:\n    - name: s\n      models:\n        \"anthropic.claude-3-5-haiku-20241022-v1:0\":\n          input: 1\n          output: 5\n";
        let path = p(&[
            "pricing",
            "sheets",
            "0",
            "models",
            "anthropic.claude-3-5-haiku-20241022-v1:0",
        ]);
        let out = put(cfg, &path, Put::Block("input: 0.8\noutput: 4")).unwrap();
        let v = back(&out);
        assert_eq!(
            v["pricing"]["sheets"][0]["models"]["anthropic.claude-3-5-haiku-20241022-v1:0"]["input"],
            0.8
        );
    }
}
