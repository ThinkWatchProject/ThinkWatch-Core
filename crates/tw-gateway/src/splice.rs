//! 在 JSON 原文上按字节改请求体：只动改了的那几个成员，别的字节一个不动。
//!
//! 解析再写回不行：没开 `preserve_order` 的 serde_json 会把每个对象的键按字母重排 ——
//! 工具定义、工具参数、整个请求都变了样，而上游的提示缓存按字节认。所以去掉客户端的身份
//! 字段（[`crate::egress`]）、按路由规则改参数（[`crate::forward::apply_set`]）都在原文上
//! 剪、换、补。
//!
//! **只给解得开的 JSON 用**：调用方解析过它。写坏了的地方这里不报错，只保证不越界。

use std::borrow::Cow;
use std::ops::Range;

use serde_json::{Map, Value};

/// 对象里的一个成员：键（连同引号）和值在原文里的区间
#[derive(Debug, Clone)]
pub(crate) struct Member {
    pub(crate) key: Range<usize>,
    pub(crate) value: Range<usize>,
}

impl Member {
    /// 键解出来的样子：写了转义的照 serde_json 解
    fn name<'b>(&self, b: &'b [u8]) -> Option<Cow<'b, str>> {
        let raw = &b[self.key.start + 1..self.key.end - 1];
        if !raw.contains(&b'\\') {
            return std::str::from_utf8(raw).ok().map(Cow::Borrowed);
        }
        serde_json::from_slice::<String>(&b[self.key.clone()])
            .ok()
            .map(Cow::Owned)
    }

    /// 键解出来是不是 `name`
    pub(crate) fn is(&self, b: &[u8], name: &str) -> bool {
        self.name(b).is_some_and(|k| k == name)
    }
}

/// 从 `i` 起跳过空白
pub(crate) fn ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// `b[i]` 是一个字符串的开头引号：返回结尾引号之后的位置
fn string_end(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'"') {
        return None;
    }
    let mut j = i + 1;
    loop {
        j += memchr::memchr2(b'"', b'\\', b.get(j..)?)?;
        if b[j] == b'"' {
            return Some(j + 1);
        }
        j += 2;
    }
}

/// `b[i]` 起的一个值：返回它之后的位置
fn value_end(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' => string_end(b, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            loop {
                match *b.get(j)? {
                    b'"' => {
                        j = string_end(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
        }
        // 数、true、false、null：到下一个分隔为止
        _ => {
            let end = b[i..]
                .iter()
                .position(|c| matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r'))
                .map_or(b.len(), |n| i + n);
            (end > i).then_some(end)
        }
    }
}

/// `b[open]` 是一个对象的 `{`：它的成员，按先后
pub(crate) fn object(b: &[u8], open: usize) -> Option<Vec<Member>> {
    if b.get(open) != Some(&b'{') {
        return None;
    }
    let mut out = Vec::new();
    let mut i = ws(b, open + 1);
    if b.get(i) == Some(&b'}') {
        return Some(out);
    }
    loop {
        let key = i..string_end(b, i)?;
        let colon = ws(b, key.end);
        if b.get(colon) != Some(&b':') {
            return None;
        }
        let v = ws(b, colon + 1);
        let value = v..value_end(b, v)?;
        let next = ws(b, value.end);
        out.push(Member { key, value });
        match *b.get(next)? {
            b',' => i = ws(b, next + 1),
            b'}' => return Some(out),
            _ => return None,
        }
    }
}

/// 剪掉 `members` 里第 `gone` 几个（按先后）要剪的区间，按先后、互不重叠。剪完还是
/// 合法的 JSON：剪掉一个成员连同它后面的逗号；排在最后的，连同它前面的逗号
pub(crate) fn cuts(members: &[Member], gone: &[usize]) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut k = 0;
    while k < gone.len() {
        // 连着要剪的一串
        let first = gone[k];
        let mut last = first;
        while k + 1 < gone.len() && gone[k + 1] == last + 1 {
            k += 1;
            last += 1;
        }
        k += 1;
        out.push(if last + 1 < members.len() {
            members[first].key.start..members[last + 1].key.start
        } else if first > 0 {
            members[first - 1].value.end..members[last].value.end
        } else {
            members[first].key.start..members[last].value.end
        });
    }
    out
}

/// [`rewrite`] 的结果。
#[derive(Debug, PartialEq)]
pub(crate) enum Rewrite {
    /// 改了的那一份
    Changed(Vec<u8>),
    /// 改完和原来一样：原文一个字节都不用动
    Same,
    /// 改到的对象里同一个键写了两遍（serde_json 留后一个，剪的结果说不准和它一样），或者
    /// 原文不像一个 JSON 对象：交给调用方解析、写回
    Unusual,
}

/// 原文 `body`（解出来是 `before`）改成 `after` 的样子，**只动改了的地方**：
///
/// - 值变了的成员只换它的值，两边都是对象的往里一层一层地比，只换里面变了的；
/// - 没了的成员连同分隔它的逗号剪掉；
/// - 新加的成员补在那个对象的末尾（按键名的先后）。
///
/// 没变的成员、它们的先后、空白和转义都照原文。改出来的那一份解析出来就是 `after`（测试
/// 拿它和解析、写回的那一份比过）。
pub(crate) fn rewrite(body: &[u8], before: &Value, after: &Value) -> Rewrite {
    let (Some(before), Some(after)) = (before.as_object(), after.as_object()) else {
        return Rewrite::Unusual;
    };
    let mut edits = Vec::new();
    if diff(body, ws(body, 0), before, after, &mut edits).is_none() {
        return Rewrite::Unusual;
    }
    if edits.is_empty() {
        return Rewrite::Same;
    }
    // 同一处起的：先补（区间是空的），再剪、再换
    edits.sort_by_key(|(r, _): &(Range<usize>, Vec<u8>)| (r.start, r.end));
    let mut out = Vec::with_capacity(body.len() + 64);
    let mut at = 0;
    for (r, text) in edits {
        debug_assert!(r.start >= at, "edits overlap");
        out.extend_from_slice(&body[at..r.start]);
        out.extend_from_slice(&text);
        at = r.end;
    }
    out.extend_from_slice(&body[at..]);
    Rewrite::Changed(out)
}

/// `b[open]` 处的对象从 `before` 改成 `after` 要做的改动，记进 `edits`（原文的区间换成
/// 什么）。同一个键写了两遍、或者原文对不上的是 `None`
fn diff(
    b: &[u8],
    open: usize,
    before: &Map<String, Value>,
    after: &Map<String, Value>,
    edits: &mut Vec<(Range<usize>, Vec<u8>)>,
) -> Option<()> {
    let members = object(b, open)?;
    let names: Vec<Cow<str>> = members.iter().map(|m| m.name(b)).collect::<Option<_>>()?;
    let mut sorted: Vec<&str> = names.iter().map(|n| n.as_ref()).collect();
    sorted.sort_unstable();
    if sorted.windows(2).any(|w| w[0] == w[1]) {
        return None;
    }
    let mut gone = Vec::new();
    for (i, (m, name)) in members.iter().zip(&names).enumerate() {
        let Some(now) = after.get(name.as_ref()) else {
            gone.push(i);
            continue;
        };
        match (before.get(name.as_ref()), now) {
            (Some(was), now) if was == now => {}
            (Some(Value::Object(was)), Value::Object(now)) if b[m.value.start] == b'{' => {
                diff(b, m.value.start, was, now, edits)?;
            }
            _ => edits.push((m.value.clone(), serde_json::to_vec(now).ok()?)),
        }
    }
    let mut added = Vec::new();
    for (k, v) in after {
        if !names.iter().any(|n| n == k) {
            added.push(format!(
                "{}:{}",
                serde_json::to_string(k).ok()?,
                serde_json::to_string(v).ok()?
            ));
        }
    }
    let added = added.join(",");
    let kept = members.len() - gone.len();
    if !added.is_empty() && kept == 0 && !members.is_empty() {
        // 原来的成员都没了：整段换成新加的
        let last = members.len() - 1;
        edits.push((
            members[0].key.start..members[last].value.end,
            added.into_bytes(),
        ));
        return Some(());
    }
    for c in cuts(&members, &gone) {
        edits.push((c, Vec::new()));
    }
    if !added.is_empty() {
        match members
            .iter()
            .enumerate()
            .rev()
            .find(|(i, _)| !gone.contains(i))
        {
            Some((_, m)) => {
                let at = m.value.end;
                edits.push((at..at, format!(",{added}").into_bytes()));
            }
            // 空对象：写在 `{` 后面
            None => edits.push((open + 1..open + 1, added.into_bytes())),
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewritten(body: &str, after: impl FnOnce(&mut Value)) -> Rewrite {
        let before: Value = serde_json::from_str(body).unwrap();
        let mut now = before.clone();
        after(&mut now);
        rewrite(body.as_bytes(), &before, &now)
    }

    fn text(r: Rewrite) -> String {
        match r {
            Rewrite::Changed(b) => String::from_utf8(b).unwrap(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn only_what_changed_is_touched() {
        let body = r#"{ "zeta": 1, "model": "opus",  "tools": [{"name":"t","input_schema":{"type":"object","properties":{"b":{},"a":{}},"required":["b"]}}], "conf": {"y": 1, "x": 2, "drop": 3}, "gone": true }"#;
        let out = text(rewritten(body, |v| {
            v["model"] = "haiku".into();
            v["conf"]["x"] = 20.into();
            v["conf"].as_object_mut().unwrap().remove("drop");
            v["conf"]["new"] = "n".into();
            v.as_object_mut().unwrap().remove("gone");
            v["added"] = 7.into();
        }));
        assert_eq!(
            out,
            r#"{ "zeta": 1, "model": "haiku",  "tools": [{"name":"t","input_schema":{"type":"object","properties":{"b":{},"a":{}},"required":["b"]}}], "conf": {"y": 1, "x": 20,"new":"n"},"added":7 }"#
        );
    }

    #[test]
    fn nothing_changed_is_the_same_and_an_emptied_object_takes_the_new_members() {
        assert_eq!(rewritten(r#"{"a":1}"#, |_| {}), Rewrite::Same);
        assert_eq!(
            text(rewritten(r#"{"c":{"old":1}}"#, |v| v["c"] =
                serde_json::json!({"new": 2}))),
            r#"{"c":{"new":2}}"#
        );
        assert_eq!(
            text(rewritten(r#"{"c":{ }}"#, |v| v["c"]["k"] = 1.into())),
            r#"{"c":{"k":1 }}"#
        );
        assert_eq!(
            text(rewritten(r#"{}"#, |v| v["k"] = 1.into())),
            r#"{"k":1}"#
        );
    }

    #[test]
    fn a_key_written_twice_where_something_changes_is_unusual() {
        assert_eq!(
            rewritten(r#"{"a":1,"a":2}"#, |v| v["b"] = 1.into()),
            Rewrite::Unusual
        );
        // 写了两遍的在没改到的那一层里：照原文留着，和客户端发的一样
        assert_eq!(
            text(rewritten(r#"{"m":{"a":1,"a":2},"x":1}"#, |v| v["x"] = 2.into())),
            r#"{"m":{"a":1,"a":2},"x":2}"#
        );
    }
}
