//! 配置手册里的字段表是从这里生成的，手册和代码对不上就失败。
//!
//! 手册是 `docs/config.md`（英文）和 `docs/config.zh-CN.md`（中文）。正文是人写的，
//! **字段表不是**：两个文件里每一处
//!
//! ```text
//! <!-- generated: table listen.gateway -->
//! …
//! <!-- /generated -->
//! ```
//!
//! 之间的内容由 [`schema`] 里的声明渲染出来。声明和代码之间有三道核对：
//!
//! - **字段一个不多一个不少**：每一节的字段名问 serde 要（[`fields`]），和声明的逐个比；
//!   代码里加了字段、手册没写，这里就挂，并且说出是哪一个。
//! - **默认值是真的**：声明写「默认 8788」，就拿一份不写这个字段的和一份写了 8788 的
//!   各解析一遍，两者得完全一样；「必填」的字段，删掉它就得解析失败。
//! - **取值是生成的**：枚举的可选值问 serde 要，不在声明里抄。
//!
//! 还没进代码的字段（远程控制那几项）声明成 [`Ty::Pending`]：照样出现在手册里，
//! 并且**一旦代码里有了这个字段，这里就挂**，逼着把它换成真正的核对。
//!
//! 手册过期了：`UPDATE_CONFIG_DOCS=1 cargo test -p tw-config --test manual` 重写
//! 两个文件里的生成段，正文不动。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::{self, DeserializeOwned, Visitor};

const UPDATE: &str = "UPDATE_CONFIG_DOCS";

// ── 问 serde 要字段名 ─────────────────────────────────────────

/// 一个什么都不给的反序列化器：derive 出来的 `Deserialize` 一开口就报出自己的
/// 字段名（`deserialize_struct` 的 `fields`）或变体名（`deserialize_enum` 的
/// `variants`），它记下来就停。**名字是 serde 真正认的那个**，改名、
/// `rename_all` 都已经算进去了。
struct Probe;

#[derive(Debug)]
struct Found(Vec<&'static str>);

impl std::fmt::Display for Found {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}
impl std::error::Error for Found {}
impl de::Error for Found {
    fn custom<T: std::fmt::Display>(_: T) -> Self {
        Found(Vec::new())
    }
}

impl<'de> de::Deserializer<'de> for Probe {
    type Error = Found;
    fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Found> {
        Err(Found(Vec::new()))
    }
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        fields: &'static [&'static str],
        _: V,
    ) -> Result<V::Value, Found> {
        Err(Found(fields.to_vec()))
    }
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _: &'static str,
        variants: &'static [&'static str],
        _: V,
    ) -> Result<V::Value, Found> {
        Err(Found(variants.to_vec()))
    }
    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map identifier ignored_any
    }
}

/// 一个 derive 出来的结构体的字段名，或者一个枚举的变体名。
pub fn fields<T: DeserializeOwned>() -> Vec<&'static str> {
    match T::deserialize(Probe) {
        Err(Found(v)) if !v.is_empty() => v,
        _ => panic!(
            "{} does not derive Deserialize as a struct or an enum, so its fields cannot be listed",
            std::any::type_name::<T>()
        ),
    }
}

/// 写了这个值和不写，解析出来是不是同一个东西。`minimal` 是这一节最少要写的那几个字段。
pub fn same_as_omitted<T: DeserializeOwned + Serialize>(
    minimal: &str,
    field: &str,
    value: &str,
) -> Result<bool, String> {
    let base: serde_yaml_ng::Value = serde_yaml_ng::from_str(minimal).map_err(|e| e.to_string())?;
    let mut with = base.clone();
    let v: serde_yaml_ng::Value = serde_yaml_ng::from_str(value).map_err(|e| e.to_string())?;
    with.as_mapping_mut()
        .ok_or("the minimal form is not a mapping")?
        .insert(field.into(), v);
    let a: T = serde_yaml_ng::from_value(base).map_err(|e| format!("without it: {e}"))?;
    let b: T = serde_yaml_ng::from_value(with).map_err(|e| format!("with it: {e}"))?;
    let a = serde_yaml_ng::to_value(a).map_err(|e| e.to_string())?;
    let b = serde_yaml_ng::to_value(b).map_err(|e| e.to_string())?;
    Ok(a == b)
}

/// 去掉这个字段还解析得了吗。
pub fn parses_without<T: DeserializeOwned>(minimal: &str, field: &str) -> Result<bool, String> {
    let mut base: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(minimal).map_err(|e| e.to_string())?;
    base.as_mapping_mut()
        .ok_or("the minimal form is not a mapping")?
        .remove(field);
    Ok(serde_yaml_ng::from_value::<T>(base).is_ok())
}

// ── 声明的形状 ────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Zh,
}

/// 一句话的两种语言。
#[derive(Clone, Copy)]
pub struct T2 {
    pub en: &'static str,
    pub zh: &'static str,
}

impl T2 {
    fn get(&self, l: Lang) -> &'static str {
        match l {
            Lang::En => self.en,
            Lang::Zh => self.zh,
        }
    }
}

/// 字段的类型。**写成节名的那几种要有对应的一节**，测试核对。
#[derive(Clone, Copy)]
pub enum Kind {
    Str,
    Int,
    Num,
    Bool,
    Strs,
    /// 可以带 `${VAR}` 的字符串
    Secret,
    /// `loopback` / `all` / 网卡名 / 地址
    Bind,
    /// 一个字符串或一组字符串
    OneOrMany,
    /// `30s` / `5m` / `1h`
    Duration,
    /// 比较式：`">200k"`
    Compare,
    /// 请求头名 → 值
    Headers,
    /// 可选值由枚举生成
    Enum(fn() -> Vec<&'static str>),
    /// 键 → 枚举值
    EnumMap(T2, fn() -> Vec<&'static str>),
    /// 一个对象，见那一节
    Obj(&'static str),
    /// 一组对象，见那一节
    Objs(&'static str),
    /// 键 → 对象，见那一节
    ObjMap(T2, &'static str),
}

#[derive(Clone, Copy)]
pub enum Def {
    /// 不写就解析失败
    Required,
    /// 可选，不写就没有；意思在说明里
    Unset,
    /// 一个 YAML 值。**核对过**：写它和不写解析出来一样
    Is(&'static str),
    /// 核对不了的默认值（生成的、在别处算的），只能照说
    Said(T2),
    /// 一个对象：默认值见那一节
    Section,
}

pub struct Row {
    pub name: &'static str,
    pub kind: Kind,
    pub def: Def,
    pub doc: T2,
}

/// 这一节对着哪个 Rust 类型核对。
pub enum Ty {
    Code {
        fields: fn() -> Vec<&'static str>,
        same: fn(&str, &str, &str) -> Result<bool, String>,
        parses_without: fn(&str, &str) -> Result<bool, String>,
        /// 这一节最少要写的字段，YAML 映射
        minimal: &'static str,
    },
    /// 还没进代码。**进了代码这里就挂**：`probe` 是一份把这一节写成 `{}` 的配置，
    /// 现在它必须因为「不认识这个字段」被拒
    Pending { probe: &'static str },
}

pub struct Section {
    /// `listen.gateway`、`providers[]`、`pricing.sheets[].models.*`
    pub path: &'static str,
    pub ty: Ty,
    pub rows: Vec<Row>,
}

/// 核对一节要的那几个函数，按类型生成。
macro_rules! checked {
    ($t:ty, $minimal:expr) => {
        $crate::Ty::Code {
            fields: $crate::fields::<$t>,
            same: $crate::same_as_omitted::<$t>,
            parses_without: $crate::parses_without::<$t>,
            minimal: $minimal,
        }
    };
}

// 声明在另一个文件里。放在宏后面，宏才看得见
#[path = "manual/schema.rs"]
mod schema;

// ── 渲染 ──────────────────────────────────────────────────────

/// 节名在手册里的锚点。
pub fn anchor(path: &str) -> String {
    let mut out = String::from("cfg-");
    let mut dash = false;
    for c in path.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

fn values(v: &[&str]) -> String {
    v.iter()
        .map(|s| format!("`{s}`"))
        .collect::<Vec<_>>()
        .join(" \\| ")
}

fn link(path: &str) -> String {
    format!("[`{path}`](#{})", anchor(path))
}

fn kind(k: &Kind, l: Lang) -> String {
    let zh = l == Lang::Zh;
    let pick = |en: &str, z: &str| if zh { z.to_string() } else { en.to_string() };
    match k {
        Kind::Str => pick("string", "字符串"),
        Kind::Int => pick("integer", "整数"),
        Kind::Num => pick("number", "数字"),
        Kind::Bool => pick("bool", "布尔"),
        Kind::Strs => pick("list of strings", "字符串列表"),
        Kind::Secret => pick("string, `${VAR}` allowed", "字符串，可写 `${VAR}`"),
        Kind::Bind => pick(
            "`loopback` \\| `all` \\| interface name \\| IP address",
            "`loopback` \\| `all` \\| 网卡名 \\| IP 地址",
        ),
        Kind::OneOrMany => pick("string or list of strings", "字符串或字符串列表"),
        Kind::Duration => pick("duration (`30s`, `5m`, `1h`)", "时长（`30s`、`5m`、`1h`）"),
        Kind::Compare => pick(
            "comparison (`>200k`, `<=4k`, `==3`)",
            "比较式（`>200k`、`<=4k`、`==3`）",
        ),
        Kind::Headers => pick("map of header name → value", "请求头名 → 值的映射"),
        Kind::Enum(f) => values(&f()),
        Kind::EnumMap(key, f) => format!(
            "{} {} → {}",
            pick("map of", "映射："),
            key.get(l),
            values(&f())
        ),
        Kind::Obj(p) => format!("{} {}", pick("object,", "对象，见"), link(p)),
        Kind::Objs(p) => format!("{} {}", pick("list of", "对象列表，见"), link(p)),
        Kind::ObjMap(key, p) => {
            format!("{} {} → {}", pick("map of", "映射："), key.get(l), link(p))
        }
    }
}

fn default(d: &Def, l: Lang) -> String {
    match d {
        Def::Required => match l {
            Lang::En => "**required**".into(),
            Lang::Zh => "**必填**".into(),
        },
        Def::Unset | Def::Section => "—".into(),
        Def::Is(v) => format!("`{v}`"),
        Def::Said(t) => t.get(l).into(),
    }
}

/// 表格里的一格：竖线要转义，换行不能有。
fn cell(s: &str) -> String {
    s.replace('\n', " ")
}

pub fn render_table(s: &Section, l: Lang) -> String {
    let mut out = format!("<a id=\"{}\"></a>\n\n", anchor(s.path));
    out += match l {
        Lang::En => "| Field | Type | Default | Description |\n",
        Lang::Zh => "| 字段 | 类型 | 默认值 | 说明 |\n",
    };
    out += "|---|---|---|---|\n";
    for r in &s.rows {
        out += &format!(
            "| `{}` | {} | {} | {} |\n",
            r.name,
            cell(&kind(&r.kind, l)),
            cell(&default(&r.def, l)),
            cell(r.doc.get(l)),
        );
    }
    out
}

// ── 核对 ──────────────────────────────────────────────────────

fn check_section(s: &Section, all: &[Section], errs: &mut Vec<String>) {
    for r in &s.rows {
        match r.kind {
            Kind::Obj(p) | Kind::Objs(p) | Kind::ObjMap(_, p) => {
                if !all.iter().any(|x| x.path == p) {
                    errs.push(format!(
                        "{}.{} points at section `{p}`, which is not declared",
                        s.path, r.name
                    ));
                }
                if !matches!(r.def, Def::Section | Def::Is(_) | Def::Unset) {
                    errs.push(format!(
                        "{}.{} is an object; its default is the section's own",
                        s.path, r.name
                    ));
                }
            }
            _ => {
                if matches!(r.def, Def::Section) {
                    errs.push(format!("{}.{} is not an object", s.path, r.name));
                }
            }
        }
    }
    match &s.ty {
        Ty::Code {
            fields,
            same,
            parses_without,
            minimal,
        } => {
            let code: BTreeSet<&str> = fields().into_iter().collect();
            let declared: BTreeSet<&str> = s.rows.iter().map(|r| r.name).collect();
            for f in code.difference(&declared) {
                errs.push(format!(
                    "`{}.{f}` is in the code but not in the manual: add a row for it in \
                     crates/tw-config/tests/manual/schema.rs, then regenerate with {UPDATE}=1",
                    s.path
                ));
            }
            for f in declared.difference(&code) {
                errs.push(format!(
                    "`{}.{f}` is in the manual but not in the code: remove its row from \
                     crates/tw-config/tests/manual/schema.rs",
                    s.path
                ));
            }
            for r in &s.rows {
                if !code.contains(r.name) {
                    continue;
                }
                match r.def {
                    Def::Is(v) => match same(minimal, r.name, v) {
                        Ok(true) => {}
                        Ok(false) => errs.push(format!(
                            "`{}.{}`: the manual says the default is `{v}`, and writing that \
                             changes what the configuration means, so it is not the default",
                            s.path, r.name
                        )),
                        Err(e) => errs.push(format!(
                            "`{}.{}`: the default `{v}` does not parse: {e}",
                            s.path, r.name
                        )),
                    },
                    Def::Required => match parses_without(minimal, r.name) {
                        Ok(false) => {}
                        Ok(true) => errs.push(format!(
                            "`{}.{}` is marked required, and the section parses without it",
                            s.path, r.name
                        )),
                        Err(e) => errs.push(format!("`{}`: {e}", s.path)),
                    },
                    _ => {}
                }
            }
            // 最少的写法本身要能解析，而且只写了必填的
            let required: BTreeSet<&str> = s
                .rows
                .iter()
                .filter(|r| matches!(r.def, Def::Required))
                .map(|r| r.name)
                .collect();
            match serde_yaml_ng::from_str::<serde_yaml_ng::Mapping>(minimal) {
                Ok(m) => {
                    let written: BTreeSet<&str> = m.keys().filter_map(|k| k.as_str()).collect();
                    if written != required {
                        errs.push(format!(
                            "`{}`: the minimal form writes {written:?}, and the required fields \
                             are {required:?}",
                            s.path
                        ));
                    }
                }
                Err(e) => errs.push(format!("`{}`: the minimal form: {e}", s.path)),
            }
        }
        Ty::Pending { probe } => {
            let segs: Vec<&str> = s
                .path
                .split('.')
                .map(|p| p.trim_end_matches("[]"))
                .collect();
            match serde_yaml_ng::from_str::<tw_config::Config>(probe) {
                Err(e)
                    if segs
                        .iter()
                        .any(|seg| e.to_string().contains(&format!("unknown field `{seg}`"))) => {}
                other => errs.push(format!(
                    "`{}` is declared as not yet in the code, and the code now reads it ({}). \
                     Replace `Ty::Pending` with checked!(TheType, minimal) in \
                     crates/tw-config/tests/manual/schema.rs so the manual is checked against it",
                    s.path,
                    match other {
                        Ok(_) => "the probe parses".to_string(),
                        Err(e) => e.to_string(),
                    }
                )),
            }
        }
    }
}

#[test]
fn every_section_matches_the_code() {
    let all = schema::sections();
    let mut errs = Vec::new();
    let mut seen = BTreeSet::new();
    for s in &all {
        if !seen.insert(s.path) {
            errs.push(format!("section `{}` is declared twice", s.path));
        }
        check_section(s, &all, &mut errs);
    }
    assert!(errs.is_empty(), "\n{}\n", errs.join("\n"));
}

/// 拿 [`Probe`] 问一个手写 `Deserialize` 的类型会得到什么 —— 它说不出字段，
/// 所以 [`fields`] 必须报错而不是返回空。
#[test]
fn the_probe_refuses_types_it_cannot_list() {
    let r = std::panic::catch_unwind(fields::<tw_config::Bind>);
    assert!(r.is_err());
    assert_eq!(
        fields::<tw_config::ProxyKind>(),
        ["socks5", "socks5h", "http", "https"]
    );
}

// ── 手册文件 ──────────────────────────────────────────────────

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

const OPEN: &str = "<!-- generated: ";
const CLOSE: &str = "<!-- /generated -->";

/// 一个生成段该是什么。`what` 是标记里写的那句：`table listen.gateway`、`rules redact`
fn generate(what: &str, l: Lang, all: &[Section]) -> Result<String, String> {
    if let Some(path) = what.strip_prefix("table ") {
        let s = all
            .iter()
            .find(|s| s.path == path)
            .ok_or_else(|| format!("no section `{path}` is declared"))?;
        return Ok(render_table(s, l));
    }
    if let Some(kind) = what.strip_prefix("rules ") {
        return schema::rules(kind, l).ok_or_else(|| format!("no rule list `{kind}`"));
    }
    Err(format!("`{what}` is not something that can be generated"))
}

/// 把一份手册里的生成段全部换成该有的样子，顺便记下写了哪些。
fn regenerate(text: &str, l: Lang, all: &[Section]) -> Result<(String, Vec<String>), String> {
    let mut out = String::new();
    let mut rest = text;
    let mut used = Vec::new();
    while let Some(i) = rest.find(OPEN) {
        let after = &rest[i + OPEN.len()..];
        let end = after
            .find("-->")
            .ok_or("a generated marker is not closed")?;
        let what = after[..end].trim().to_string();
        let body_start = i + OPEN.len() + end + 3;
        let close = rest[body_start..]
            .find(CLOSE)
            .ok_or_else(|| format!("`{what}` has no {CLOSE}"))?;
        out += &rest[..body_start];
        out += "\n";
        out += &generate(&what, l, all)?;
        out += CLOSE;
        rest = &rest[body_start + close + CLOSE.len()..];
        used.push(what);
    }
    out += rest;
    Ok((out, used))
}

#[test]
fn the_manual_is_what_the_code_says() {
    let all = schema::sections();
    let update = std::env::var_os(UPDATE).is_some();
    let mut errs = Vec::new();
    for (file, l) in [
        ("docs/config.md", Lang::En),
        ("docs/config.zh-CN.md", Lang::Zh),
    ] {
        let path = workspace().join(file);
        // Windows 上的检出可能把换行转成了 CRLF，比的是内容不是换行
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{file}: {e}"))
            .replace("\r\n", "\n");
        let (next, used) = match regenerate(&text, l, &all) {
            Ok(x) => x,
            Err(e) => {
                errs.push(format!("{file}: {e}"));
                continue;
            }
        };
        // 每一节都得在手册里出现，而且只出现一次
        for s in &all {
            let n = used
                .iter()
                .filter(|u| **u == format!("table {}", s.path))
                .count();
            if n != 1 {
                errs.push(format!(
                    "{file}: `table {}` appears {n} times; it has to appear once",
                    s.path
                ));
            }
        }
        for kind in schema::RULE_LISTS {
            if !used.iter().any(|u| *u == format!("rules {kind}")) {
                errs.push(format!("{file}: `rules {kind}` is missing"));
            }
        }
        if next != text {
            if update {
                std::fs::write(&path, next).unwrap();
            } else {
                errs.push(format!(
                    "{file} is out of date with the code. Regenerate it:\n  \
                     {UPDATE}=1 cargo test -p tw-config --test manual"
                ));
            }
        }
    }
    assert!(errs.is_empty(), "\n{}\n", errs.join("\n"));
}

/// 手册里的 YAML 示例读得进来。**只查写法，不查引用** —— 示例里的
/// `pricing: relay-discount` 指向的价目表写在另一段示例里。
///
/// 从第一列开始写的示例才是整份配置的一段；缩进开头的是某一项的片段，
/// 带 `…` 的是占位，都跳过。还没进代码的那几节同样跳过，[`Ty::Pending`] 管它们。
#[test]
fn the_examples_in_the_manual_parse() {
    let all = schema::sections();
    let pending: Vec<&str> = all
        .iter()
        .filter(|s| matches!(s.ty, Ty::Pending { .. }))
        .map(|s| s.path.rsplit('.').next().unwrap_or(s.path))
        .collect();
    let mut errs = Vec::new();
    for file in [
        "docs/config.md",
        "docs/config.zh-CN.md",
        "docs/server.md",
        "docs/server.zh-CN.md",
    ] {
        let text = std::fs::read_to_string(workspace().join(file))
            .unwrap()
            .replace("\r\n", "\n");
        let mut rest = text.as_str();
        while let Some(i) = rest.find("```yaml\n") {
            let body = &rest[i + 8..];
            let end = body.find("```").expect("an unclosed code block");
            let block = &body[..end];
            rest = &body[end + 3..];
            let skip = block.starts_with(' ')
                || block.contains('…')
                || pending.iter().any(|p| block.contains(&format!("{p}:")));
            if skip {
                continue;
            }
            if let Err(e) =
                serde_yaml_ng::from_str::<tw_config::Config>(&format!("version: 1\n{block}"))
            {
                errs.push(format!("{file}: an example does not parse: {e}\n{block}"));
            }
        }
    }
    assert!(errs.is_empty(), "\n{}\n", errs.join("\n"));
}
