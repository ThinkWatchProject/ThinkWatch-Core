//! 按路径写进去的任意文字读得回来，而且动不了文件的结构。
//!
//! 随机造字符串（引号、反斜杠、`#`、`: `、首尾空白、换行、控制字符、YAML 1.1 当换行的
//! 那几个、BOM、非字符、中文、emoji……），用 `set` 写进原来是纯量、单引号、双引号的值，
//! 用 `insert` 写进一个还没写过的键，断言：
//!
//! - serde（配置的加载器走的那条路，YAML 1.1）和 tw-yaml 自己的解析器读到的，都是写进去
//!   的那个字符串；
//! - 被改的那一行（新加的那一行）之外，每个字节都没动。
//!
//! 哪些字段能写换行不归这一层管（见 `tw_config::edit::check_line_breaks`）：这里只管写得对。
//! 生成器自己写，种子可复现（`TW_PROP_SEED`），和 `property.rs` 同一个做法。

use tw_yaml::{NodeKind, Scalar, Step, insert, nodes, set};

const DOC: &str = "# 说明
a:
  plain: old   # 行尾注释
  single: 'old'
  double: \"old\"
  # 中间的注释
  last: 1
b: keep   # 别动
";

/// xorshift64：要的是可复现，不是随机质量
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

/// 单个字符：可打印的 ASCII（含 YAML 的指示符）和最容易出事的那些
const CHARS: &[char] = &[
    'a', 'Z', '0', ' ', ' ', '"', '\'', '\\', '#', ':', '-', '.', ',', '[', ']', '{', '}', '&',
    '*', '!', '|', '>', '%', '@', '`', '?', '\n', '\r', '\t', '\u{0}', '\u{1}', '\u{1b}', '\u{7f}',
    '\u{80}', '\u{85}', '\u{9b}', '\u{9f}', '\u{a0}', '\u{2028}', '\u{2029}', '\u{feff}',
    '\u{fffe}', '\u{ffff}', '\u{200b}', '\u{301}', '中', '文', '😀', '𝄞',
];

/// 成段的写法：文档标记、键值分隔、注释、块标量的开头、锚点、转义的样子……
const PIECES: &[&str] = &[
    "---",
    "...",
    ": ",
    " #",
    "# ",
    "- ",
    "? ",
    "|",
    ">",
    "&a ",
    "*a",
    "!tag ",
    "\\n",
    "\\x41",
    "\\u2028",
    "\"\"",
    "''",
    "\r\n",
    "true",
    "null",
    "~",
    "1e3",
    "a\u{2028}b",
    "\u{85}",
];

fn arbitrary(rng: &mut Rng) -> String {
    let mut s = String::new();
    if rng.below(5) == 0 {
        s.push(' ');
    }
    for _ in 0..rng.below(12) {
        if rng.below(3) == 0 {
            s.push_str(rng.pick(PIECES));
        } else {
            s.push(*rng.pick(CHARS));
        }
    }
    if rng.below(5) == 0 {
        s.push(' ');
    }
    s
}

/// 一眼能想到的那些，每个都试
const FIXED: &[&str] = &[
    "",
    "\u{2028}",
    "\u{2029}",
    "\u{85}",
    "a\u{2028}b",
    "a\u{2029}b",
    "a\u{85}b",
    "a\u{9b}b",
    "\u{feff}a",
    "a\u{fffe}\u{ffff}",
    "a\u{0}b",
    "a\tb",
    "a\nb",
    "line one\r\nline two",
    "'",
    "\"",
    "\\",
    " # x",
    "x: y",
    "...",
    "... x",
    "---",
];

fn seed() -> u64 {
    std::env::var("TW_PROP_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_2028_abcd_0001)
}

fn cases() -> Vec<String> {
    let mut rng = Rng(seed() | 1);
    let mut out: Vec<String> = FIXED.iter().map(|s| s.to_string()).collect();
    out.extend((0..1500).map(|_| arbitrary(&mut rng)));
    out
}

fn path(key: &str) -> Vec<Step> {
    vec![Step::key("a"), Step::key(key)]
}

/// 两个解析器读到的都是它
fn reads_back(out: &str, key: &str, s: &str) {
    let v: serde_yaml_ng::Value = serde_yaml_ng::from_str(out)
        .unwrap_or_else(|e| panic!("{s:?}: serde cannot read it: {e}\n{out}"));
    assert_eq!(
        v["a"][key].as_str(),
        Some(s),
        "serde read something else\n{out}"
    );
    assert_eq!(v["b"].as_str(), Some("keep"), "{s:?}\n{out}");
    let all = nodes(out).unwrap_or_else(|e| panic!("{s:?}: tw-yaml cannot read it: {e}\n{out}"));
    let n = all
        .iter()
        .find(|n| n.path == path(key))
        .unwrap_or_else(|| panic!("{s:?}: tw-yaml has no a.{key}\n{out}"));
    match &n.kind {
        NodeKind::Scalar { value, .. } => {
            assert_eq!(value, s, "tw-yaml read something else\n{out}")
        }
        other => panic!("{s:?}: a.{key} is {other:?}\n{out}"),
    }
}

/// 改一个原来是纯量、单引号、双引号的值：读得回来，只有那一行变了，而且还是一行
#[test]
fn any_text_set_over_an_old_value_reads_back_and_changes_only_its_line() {
    for key in ["plain", "single", "double"] {
        let line = DOC
            .lines()
            .position(|l| l.trim_start().starts_with(&format!("{key}:")))
            .unwrap();
        for s in cases() {
            let out = set(DOC, &path(key), &Scalar::s(s.clone()))
                .unwrap_or_else(|e| panic!("{key} {s:?}: {e}"));
            reads_back(&out, key, &s);
            let before: Vec<&str> = DOC.lines().collect();
            let after: Vec<&str> = out.lines().collect();
            assert_eq!(
                before.len(),
                after.len(),
                "{key} {s:?}: lines added or lost\n{out}"
            );
            let changed: Vec<usize> = (0..before.len())
                .filter(|&i| before[i] != after[i])
                .collect();
            assert!(
                changed.is_empty() || changed == [line],
                "{key} {s:?}: other lines changed: {changed:?}\n{out}"
            );
        }
    }
}

/// 写一个还没写过的键：读得回来，原文一个字节不少，多出来的正好一行
#[test]
fn any_text_inserted_under_a_new_key_reads_back_as_one_new_line() {
    for s in cases() {
        let out = insert(DOC, &path("new"), &Scalar::s(s.clone()))
            .unwrap_or_else(|e| panic!("{s:?}: {e}"));
        reads_back(&out, "new", &s);
        let common = DOC
            .bytes()
            .zip(out.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        let start = DOC[..common].rfind('\n').map_or(0, |i| i + 1);
        let rest = &DOC[start..];
        assert!(
            out.ends_with(rest),
            "{s:?}: the original text was not kept\n{out}"
        );
        let added = &out[start..out.len() - rest.len()];
        assert_eq!(added.lines().count(), 1, "{s:?}: {added:?}\n{out}");
    }
}
