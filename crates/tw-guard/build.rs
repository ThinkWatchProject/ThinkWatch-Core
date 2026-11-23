//! 把 `data/redact.yaml`（出站脱敏的内置规则目录）编成 Rust 常量。
//!
//! **为什么在编译期，不在运行时读**：`rules::BUILTINS` 要是 `&'static [Builtin]` 常量 ——
//! 快速排除用的 `KEY_MIN` / `KEY_FIRST` 是在编译期对着它算出来的，扫描路径上的代码也都按
//! `&'static str` 的 id 拿它。运行时读一份 YAML 再泄漏成静态数据，那几个常量就得改成运行时
//! 算，碰到扫描路径。编译期生成，现有的调用方一行不用改。
//!
//! 生成两个文件，`rules.rs` 用 `include!` 引进去：
//!
//! - `redact_builtins.rs`：`BUILTINS` 的值，一条规则一个 `Builtin { .. }`；
//! - `redact_kinds.rs`：分组的名字，`KIND_NAMES` 的值。
//!
//! 写错了（字段拼错、前缀类写了说明、代码判据的没写说明……）在这里就停下，报出是哪一条。

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    kinds: Vec<KindSpec>,
    rules: Vec<RuleSpec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KindSpec {
    slug: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleSpec {
    id: String,
    kind: String,
    #[serde(default)]
    on_by_default: bool,
    #[serde(default)]
    label: Option<String>,
    name: String,
    #[serde(default)]
    about: Option<String>,
    #[serde(default)]
    hosts: Vec<String>,
    #[serde(default)]
    prefix: Option<OnePrefix>,
    #[serde(default)]
    prefixes: Option<Vec<Prefix>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OnePrefix {
    prefix: String,
    min_tail: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prefix {
    prefix: String,
    min_tail: usize,
    #[serde(default)]
    tail: Tail,
}

#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Tail {
    #[default]
    Any,
    Hex,
    Alnum,
}

impl Tail {
    fn variant(self) -> &'static str {
        match self {
            Tail::Any => "Any",
            Tail::Hex => "Hex",
            Tail::Alnum => "Alnum",
        }
    }
    fn en(self) -> &'static str {
        match self {
            Tail::Any => "characters",
            Tail::Hex => "hex characters",
            Tail::Alnum => "letters or digits",
        }
    }
}

const SOURCE: &str = "data/redact.yaml";

fn main() {
    println!("cargo:rerun-if-changed={SOURCE}");
    println!("cargo:rerun-if-changed=build.rs");
    let text = std::fs::read_to_string(SOURCE).unwrap_or_else(|e| panic!("{SOURCE}: {e}"));
    let file: File = serde_yaml_ng::from_str(&text).unwrap_or_else(|e| panic!("{SOURCE}: {e}"));
    let out = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    let out = Path::new(&out);
    std::fs::write(out.join("redact_kinds.rs"), kinds(&file)).expect("write redact_kinds.rs");
    std::fs::write(out.join("redact_builtins.rs"), builtins(&file))
        .expect("write redact_builtins.rs");
}

/// `ai-services` → `AiServices`：`Kind` 的变体名。不认识的分组在编译 `include!` 时报错
fn variant(slug: &str) -> String {
    slug.split('-')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
                .unwrap_or_default()
        })
        .collect()
}

fn kinds(file: &File) -> String {
    let mut s = String::from("&[\n");
    for k in &file.kinds {
        assert!(
            !k.name.trim().is_empty(),
            "{SOURCE}: kind `{}` needs a name",
            k.slug
        );
        writeln!(
            s,
            "    KindName {{ kind: Kind::{}, name: {:?} }},",
            variant(&k.slug),
            k.name
        )
        .unwrap();
    }
    s.push(']');
    s
}

fn builtins(file: &File) -> String {
    let kinds: HashSet<&str> = file.kinds.iter().map(|k| k.slug.as_str()).collect();
    let mut ids = HashSet::new();
    let mut s = String::from("&[\n");
    for r in &file.rules {
        let at = format!("{SOURCE}: rule `{}`", r.id);
        assert!(ids.insert(r.id.as_str()), "{at} is listed twice");
        assert!(
            kinds.contains(r.kind.as_str()),
            "{at}: unknown kind `{}`",
            r.kind
        );
        let ok = |t: &str| !t.trim().is_empty();
        assert!(ok(&r.name), "{at} needs a name");
        let (matcher, about) = match (&r.prefix, &r.prefixes) {
            (Some(_), Some(_)) => panic!("{at}: `prefix` or `prefixes`, not both"),
            (Some(p), None) => {
                assert!(!p.prefix.is_empty(), "{at}: empty prefix");
                let one = [Prefix {
                    prefix: p.prefix.clone(),
                    min_tail: p.min_tail,
                    tail: Tail::Any,
                }];
                (
                    format!(
                        "Matcher::Prefix {{ prefix: {:?}, min_tail: {} }}",
                        p.prefix, p.min_tail
                    ),
                    describe(&one),
                )
            }
            (None, Some(list)) => {
                assert!(!list.is_empty(), "{at}: empty `prefixes`");
                let mut m = String::from("Matcher::Prefixes { list: &[");
                for p in list {
                    assert!(!p.prefix.is_empty(), "{at}: empty prefix");
                    write!(
                        m,
                        "Pfx {{ prefix: {:?}, min_tail: {}, tail: Tail::{} }}, ",
                        p.prefix,
                        p.min_tail,
                        p.tail.variant()
                    )
                    .unwrap();
                }
                m.push_str("] }");
                (m, describe(list))
            }
            // 代码里的判据：`detector` 是 const fn，查不到这个 id 就是编译错误
            (None, None) => (format!("detector({:?})", r.id), String::new()),
        };
        let about = if matcher.starts_with("detector") {
            match &r.about {
                Some(about) if ok(about) => about.clone(),
                _ => panic!("{at}: a rule whose check is in code needs `about`"),
            }
        } else {
            assert!(
                r.about.is_none(),
                "{at}: a prefix rule's description is generated from its prefixes; drop `about`"
            );
            about
        };
        let label = match &r.label {
            Some(l) => format!("Some({l:?})"),
            None => "None".into(),
        };
        writeln!(
            s,
            "    Builtin {{ id: {:?}, kind: Kind::{}, name: {:?}, about: {:?}, \
             on_by_default: {}, matcher: {matcher}, label: {label}, hosts: &{:?} }},",
            r.id,
            variant(&r.kind),
            r.name,
            about,
            r.on_by_default,
            r.hosts,
        )
        .unwrap();
    }
    s.push(']');
    s
}

/// 前缀类规则的英文说明。尾巴要求（最短几个、什么字符）一样的前缀连成一段，段的先后
/// 按各自第一次出现的位置（`hvs.`、`hvb.` 尾长不同就是两段）
fn describe(list: &[Prefix]) -> String {
    let mut runs: Vec<(usize, Tail, Vec<String>)> = Vec::new();
    for p in list {
        let code = format!("`{}`", p.prefix);
        match runs
            .iter_mut()
            .find(|(n, t, _)| *n == p.min_tail && *t == p.tail)
        {
            Some(run) => run.2.push(code),
            None => runs.push((p.min_tail, p.tail, vec![code])),
        }
    }
    runs.iter()
        .enumerate()
        .map(|(i, (n, t, ps))| {
            // 分号后面接着的一段不大写
            let starts = if i == 0 { "Starts" } else { "starts" };
            format!(
                "{starts} with {}, followed by at least {n} {}",
                or_list(ps),
                t.en()
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// `a`、`a or b`、`a, b or c`
fn or_list(xs: &[String]) -> String {
    match xs {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} or {last}", init.join(", ")),
    }
}
