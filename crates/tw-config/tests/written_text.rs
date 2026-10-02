//! 写进配置的任意文字动不了文件的结构。
//!
//! 随机造字符串（换行、回车、制表符、引号、反斜杠、`#`、`: `、`---`、`...`、首尾空白、
//! 控制字符、YAML 1.1 当换行的那几个字符、中文、emoji、组合字符……），经按名字编辑的
//! 那一层（`tw_config::edit`）写进一份带注释的配置，断言：
//!
//! - 读回来一字不差：serde 那条加载路径、整份配置的解析和校验、tw-yaml 的解析器，三处
//!   读到的都是写进去的那个字符串；
//! - 被改的那一行（新加的那几行）之外，**每个字节都没动**：别的键、注释原样。
//!
//! 生成器自己写，种子可复现（`TW_PROP_SEED`），和 tw-yaml 的 property test 同一个做法。

use serde_yaml_ng::{Mapping, Value};
use tw_config::edit::{self, PLUGINS};
use tw_yaml::{NodeKind, Step};

const HASH: &str = "6f1c000000000000000000000000000000000000000000000000000000000abc";

fn doc() -> String {
    format!(
        "version: 1
# 控制面的钥匙
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00  # 别动
clients:
  # 默认那把
  - name: default
    key: tw-aaaa
    client: codex   # 给 Codex 用
plugins:
  # 第一个
  - id: first
    file: plugins/first.js
    sha256: {HASH}
    enabled: false
    settings:
      note: plain   # 行尾注释
  - id: target
    file: plugins/target.js
    sha256: {HASH}
    enabled: false
    settings:
      note: old
      keep: 1
# 插件之后
providers:
  - name: 官方
    base_url: https://api.anthropic.com  # 直连
    key: sk-a
"
    )
}

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
    'a', 'Z', '0', '9', ' ', ' ', '"', '\'', '\\', '#', ':', '-', '.', ',', '[', ']', '{', '}',
    '&', '*', '!', '|', '>', '%', '@', '`', '?', '=', '/', '\n', '\n', '\r', '\t', '\u{0}',
    '\u{1}', '\u{1b}', '\u{7f}', '\u{80}', '\u{85}', '\u{9f}', '\u{a0}', '\u{2028}', '\u{2029}',
    '\u{feff}', '\u{fffe}', '\u{ffff}', '\u{200b}', '\u{301}', '中', '文', '登', '😀', '𝄞',
];

/// 成段的写法：文档标记、键值分隔、注释、块标量的开头、锚点、标签、转义的样子……
const PIECES: &[&str] = &[
    "---",
    "...",
    ": ",
    " #",
    "# ",
    "- ",
    "? ",
    "|",
    "|-",
    ">",
    "&a ",
    "*a",
    "!tag ",
    "%YAML 1.2",
    "\\n",
    "\\x41",
    "\"\"",
    "''",
    "key: value",
    "\n---\n",
    "\n...\n",
    "\r\n",
    "  ",
    "twq0x0z",
    "true",
    "null",
    "~",
    "0x1f",
    "1e3",
    "登陆=登录",
    "\\bsk-[a-z]+\\b",
];

fn arbitrary(rng: &mut Rng) -> String {
    let mut s = String::new();
    if rng.below(5) == 0 {
        s.push_str(&" ".repeat(1 + rng.below(3)));
    }
    for _ in 0..rng.below(14) {
        if rng.below(3) == 0 {
            s.push_str(rng.pick(PIECES));
        } else {
            s.push(*rng.pick(CHARS));
        }
    }
    if rng.below(5) == 0 {
        s.push_str(&" ".repeat(1 + rng.below(3)));
    }
    s
}

/// 一眼能想到的那些，每个都试
const FIXED: &[&str] = &[
    "",
    " ",
    "\n",
    "\r\n",
    "\r",
    "\t",
    "---",
    "...",
    "--- a",
    "a\n---\nb: c",
    "a\n...\n",
    "- x",
    "? x",
    "x: y",
    "x:",
    "#",
    " # x",
    "\"",
    "'",
    "\\",
    "\\n",
    "|\n  a",
    ">\n b",
    "&anchor x",
    "*alias",
    "!!str x",
    "%TAG ! x",
    " leading",
    "trailing ",
    "  both  ",
    "\u{2028}",
    "\u{85}",
    "\u{feff}",
    "a\u{0}b",
    "line one\nline two\n\nline four",
    "登陆=登录\n帐号=账号",
    "😀\n𝄞",
];

fn seed() -> u64 {
    std::env::var("TW_PROP_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_1234_abcd_0001)
}

fn cases() -> Vec<String> {
    let mut rng = Rng(seed() | 1);
    let mut out: Vec<String> = FIXED.iter().map(|s| s.to_string()).collect();
    out.extend((0..1500).map(|_| arbitrary(&mut rng)));
    out
}

fn yaml_map(text: &str) -> Mapping {
    serde_yaml_ng::from_str(text).unwrap()
}

/// 改过的那一份里，这个位置上 tw-yaml 读到的标量
fn scalar_at(text: &str, path: &[Step]) -> String {
    let nodes =
        tw_yaml::nodes(text).unwrap_or_else(|e| panic!("tw-yaml cannot read it: {e}\n{text}"));
    let n = nodes
        .iter()
        .find(|n| n.path == path)
        .unwrap_or_else(|| panic!("tw-yaml has no {path:?}\n{text}"));
    match &n.kind {
        NodeKind::Scalar { value, .. } => value.clone(),
        other => panic!("{path:?} is {other:?}\n{text}"),
    }
}

fn note_path(index: usize, key: &str) -> Vec<Step> {
    vec![
        Step::key("plugins"),
        Step::Index(index),
        Step::key("settings"),
        Step::key(key),
    ]
}

/// 三条读法读到的都是它
fn reads_back(
    out: &str,
    path: &[Step],
    cfg_value: impl Fn(&tw_config::Config) -> Option<String>,
    s: &str,
) {
    let v: Value =
        serde_yaml_ng::from_str(out).unwrap_or_else(|e| panic!("{s:?}: serde: {e}\n{out}"));
    let mut cur = &v;
    for st in path {
        cur = match st {
            Step::Key(k) => &cur[k.as_str()],
            Step::Index(i) => &cur[*i],
        };
    }
    assert_eq!(cur.as_str(), Some(s), "serde read something else\n{out}");
    let cfg = tw_config::try_parse(out)
        .unwrap_or_else(|r| panic!("{s:?}: the configuration does not load: {r}\n{out}"));
    assert_eq!(
        cfg_value(&cfg).as_deref(),
        Some(s),
        "the configuration read something else\n{out}"
    );
    assert_eq!(
        scalar_at(out, path),
        s,
        "tw-yaml read something else\n{out}"
    );
}

/// 改了一项里的一个设置：**只有那一行变了**，而且它还是一行
#[test]
fn a_setting_written_over_an_old_one_changes_only_its_own_line() {
    let base = doc();
    for s in cases() {
        let mut item = yaml_map(&format!(
            "id: target\nfile: plugins/target.js\nsha256: {HASH}\nenabled: false\nsettings:\n  note: x\n  keep: 1\n"
        ));
        item["settings"]["note"] = Value::String(s.clone());
        let out = edit::upsert(&base, PLUGINS, Some("target"), &item)
            .unwrap_or_else(|e| panic!("{s:?}: {e}"));
        reads_back(
            &out,
            &note_path(1, "note"),
            |c| {
                c.plugins[1]
                    .settings
                    .get("note")?
                    .as_str()
                    .map(str::to_string)
            },
            &s,
        );
        let before: Vec<&str> = base.lines().collect();
        let after: Vec<&str> = out.lines().collect();
        assert_eq!(
            before.len(),
            after.len(),
            "{s:?}: lines were added or lost\n{out}"
        );
        let changed: Vec<usize> = (0..before.len())
            .filter(|&i| before[i] != after[i])
            .collect();
        let line = before.iter().position(|l| *l == "      note: old").unwrap();
        assert!(
            changed.is_empty() || changed == [line],
            "{s:?}: other lines changed: {changed:?}\n{out}"
        );
        assert!(after[line].starts_with("      note: "), "{s:?}\n{out}");
        assert!(out.ends_with('\n') && base.ends_with('\n'));
    }
}

/// 新加一项：原文**一个字节不少地**留在前后两段里，中间多出来的是完整的几行
#[test]
fn a_new_entry_is_one_insertion_of_whole_lines() {
    let base = doc();
    let mut rng = Rng((seed() ^ 0xdead_beef) | 1);
    for s in cases() {
        let t = arbitrary(&mut rng);
        let mut item = yaml_map(&format!(
            "id: added\nfile: plugins/added.js\nsha256: {HASH}\nenabled: false\nsettings:\n  note: x\n  other: y\n"
        ));
        item["settings"]["note"] = Value::String(s.clone());
        item["settings"]["other"] = Value::String(t.clone());
        let out =
            edit::upsert(&base, PLUGINS, None, &item).unwrap_or_else(|e| panic!("{s:?}: {e}"));
        reads_back(
            &out,
            &note_path(2, "note"),
            |c| {
                c.plugins[2]
                    .settings
                    .get("note")?
                    .as_str()
                    .map(str::to_string)
            },
            &s,
        );
        reads_back(
            &out,
            &note_path(2, "other"),
            |c| {
                c.plugins[2]
                    .settings
                    .get("other")?
                    .as_str()
                    .map(str::to_string)
            },
            &t,
        );
        // 最长的公共前缀之后，剩下的原文得原样是结尾
        let common = base
            .bytes()
            .zip(out.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        let start = base[..common].rfind('\n').map_or(0, |i| i + 1);
        let rest = &base[start..];
        assert!(
            out.ends_with(rest),
            "{s:?}: the original text was not kept around the new entry\n{out}"
        );
        let inserted = &out[start..out.len() - rest.len()];
        assert_eq!(
            inserted.lines().count(),
            7,
            "{s:?}/{t:?}: the new entry is not seven lines\n{inserted}"
        );
    }
}

/// 单行的字段（按路径设）：换行以外的字符照样写得进去、读得回来，只有那一行变了
#[test]
fn a_single_line_field_takes_everything_but_a_line_break() {
    let base = doc();
    let path = [Step::key("clients"), Step::Index(0), Step::key("client")];
    for s in cases() {
        let s: String = s.chars().filter(|c| !matches!(c, '\n' | '\r')).collect();
        let out = edit::set(&base, &path, Some(&Value::String(s.clone())))
            .unwrap_or_else(|e| panic!("{s:?}: {e}"));
        let v: Value = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(
            v["clients"][0]["client"].as_str(),
            Some(s.as_str()),
            "{out}"
        );
        assert_eq!(scalar_at(&out, &path), s, "{out}");
        let cfg = tw_config::try_parse(&out).unwrap_or_else(|r| panic!("{s:?}: {r}\n{out}"));
        assert_eq!(cfg.clients[0].client.as_deref(), Some(s.as_str()));
        let before: Vec<&str> = base.lines().collect();
        let after: Vec<&str> = out.lines().collect();
        assert_eq!(before.len(), after.len(), "{s:?}\n{out}");
        let changed: Vec<usize> = (0..before.len())
            .filter(|&i| before[i] != after[i])
            .collect();
        let line = before
            .iter()
            .position(|l| l.starts_with("    client: "))
            .unwrap();
        assert!(
            changed.is_empty() || changed == [line],
            "{s:?}: other lines changed: {changed:?}\n{out}"
        );
    }
}

/// 换行进不了单行的字段；进得了的那一段（插件设置）之外的字段也不行
#[test]
fn a_line_break_stays_out_of_single_line_fields() {
    let base = doc();
    for s in ["a\nb", "a\rb", "\n"] {
        let path = [Step::key("clients"), Step::Index(0), Step::key("client")];
        let e = edit::set(&base, &path, Some(&Value::String(s.into()))).unwrap_err();
        assert_eq!(e.msg().code, "config.edit.multiline", "{s:?}");
        let mut item = yaml_map(&format!(
            "id: target\nfile: plugins/target.js\nsha256: {HASH}\nenabled: false\nsettings:\n  note: old\n  keep: 1\n"
        ));
        item.insert("scope".into(), Value::Mapping(yaml_map("models: [x]")));
        item["scope"]["models"][0] = Value::String(s.into());
        let e = edit::upsert(&base, PLUGINS, Some("target"), &item).unwrap_err();
        assert_eq!(e.msg().code, "config.edit.multiline", "{s:?}");
    }
}
