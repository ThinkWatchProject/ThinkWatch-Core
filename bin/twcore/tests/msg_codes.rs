//! core 发得出的每一个消息码都在 `crates/tw-api/msg-codes.txt` 里，清单里也
//! 没有发不出的码。
//!
//! 桌面端按码翻译（`src/i18n/core.zh.json`），它钉着 core 的某个 tag，从那个
//! tag 的 tw-api 里读这份清单（[`tw_api::MSG_CODES`]），和自己的译文表对一遍：
//! 多一个没翻的码，中文界面上那一句就悄悄变成英文。以前两边对得上全靠人记得。
//!
//! 码从源码里找，**不从运行时收集**：有的码只在另一个平台上编译得到
//! （`#[cfg(windows)]` 那一支），而清单要两个平台的都有。认得的写法只有两种：
//!
//! - `msg!("码", …)`，或者 `msg!(常量, …)` 而那个常量是个字符串字面量；
//! - `code!("码")` —— 码和句子分开存在表里的地方。
//!
//! `msg!` 的第一个参数是别的样子时这里直接失败，而不是漏掉它。
//!
//! 客户端接管、MCP、扫描的码不在这里：那些代码在桌面端，清单也在那边
//! （桌面端仓库的 `src-tauri/msg-codes.txt`，同一套扫法）。
//!
//! 清单一行一个码，按字母排。后面可以跟一个标记：
//!
//! - `passthrough`：句子只有占位符（`"{detail}"`），原文是系统或上游的话，
//!   界面照 `text` 显示，不用翻（位置参数 `"{}"` 不算，那是句子在别处写好了）；
//! - `test`：只在测试里出现，不会到界面上。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

const MANIFEST: &str = "crates/tw-api/msg-codes.txt";
const UPDATE: &str = "UPDATE_MSG_CODES";

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Str(String),
    Ident(String),
    Punct(char),
}

/// 把一段源码切成记号，连同所在行。**跳过注释、字符字面量和生命周期。**
fn lex(src: &str) -> Vec<(usize, Tok)> {
    let c: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let (mut i, mut line) = (0, 1);
    let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
    while i < c.len() {
        match c[i] {
            '\n' => {
                line += 1;
                i += 1;
            }
            ch if ch.is_whitespace() => i += 1,
            '/' if c.get(i + 1) == Some(&'/') => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            }
            '/' if c.get(i + 1) == Some(&'*') => {
                let mut depth = 0;
                while i < c.len() {
                    if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                        depth += 1;
                        i += 2;
                    } else if c[i] == '*' && c.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        if c[i] == '\n' {
                            line += 1;
                        }
                        i += 1;
                    }
                }
            }
            '\'' => {
                // 字符字面量（'x'、'\n'、'中'）整个跳过；否则是生命周期
                if c.get(i + 1) == Some(&'\\') {
                    i += 2;
                    while i < c.len() && c[i] != '\'' {
                        i += 1;
                    }
                    i += 1;
                } else if c.get(i + 2) == Some(&'\'') {
                    i += 3;
                } else {
                    i += 1;
                    while i < c.len() && ident(c[i]) {
                        i += 1;
                    }
                }
            }
            // 原始字符串 r"…"、r#"…"#，以及字节串 b"…"、br"…"
            'r' | 'b'
                if (i == 0 || !ident(c[i - 1]))
                    && (matches!(c.get(i + 1), Some('"') | Some('#'))
                        || (c[i] == 'b'
                            && c.get(i + 1) == Some(&'r')
                            && matches!(c.get(i + 2), Some('"') | Some('#')))) =>
            {
                let raw = c[i] == 'r' || c.get(i + 1) == Some(&'r');
                let mut j = if c[i] == 'b' && raw { i + 2 } else { i + 1 };
                if !raw {
                    // b"…"：和普通字符串一样读
                    i = j;
                    let (lit, end) = quoted(&c, i);
                    line += lit.matches('\n').count();
                    out.push((line, Tok::Str(lit)));
                    i = end;
                    continue;
                }
                let mut hashes = 0;
                while c.get(j) == Some(&'#') {
                    hashes += 1;
                    j += 1;
                }
                if c.get(j) != Some(&'"') {
                    // r#ident：原始标识符
                    let s = j;
                    while j < c.len() && ident(c[j]) {
                        j += 1;
                    }
                    out.push((line, Tok::Ident(c[s..j].iter().collect())));
                    i = j;
                    continue;
                }
                let start = j + 1;
                let mut k = start;
                let end = loop {
                    if k >= c.len() {
                        break c.len();
                    }
                    if c[k] == '"' && (1..=hashes).all(|h| c.get(k + h) == Some(&'#')) {
                        break k;
                    }
                    k += 1;
                };
                let lit: String = c[start..end].iter().collect();
                out.push((line, Tok::Str(lit.clone())));
                line += lit.matches('\n').count();
                i = end + 1 + hashes;
            }
            '"' => {
                let (lit, end) = quoted(&c, i);
                out.push((line, Tok::Str(lit.clone())));
                line += lit.matches('\n').count();
                i = end;
            }
            ch if ident(ch) => {
                let s = i;
                while i < c.len() && ident(c[i]) {
                    i += 1;
                }
                out.push((line, Tok::Ident(c[s..i].iter().collect())));
            }
            ch => {
                out.push((line, Tok::Punct(ch)));
                i += 1;
            }
        }
    }
    out
}

/// 从 `c[at]` 的 `"` 读到配对的 `"`，返回内容（转义原样保留）和结束之后的位置。
fn quoted(c: &[char], at: usize) -> (String, usize) {
    let start = at + 1;
    let mut k = start;
    while k < c.len() && c[k] != '"' {
        k += if c[k] == '\\' { 2 } else { 1 };
    }
    let end = k.min(c.len());
    (c[start..end].iter().collect(), end + 1)
}

fn is(t: Option<&(usize, Tok)>, want: &Tok) -> bool {
    t.is_some_and(|(_, t)| t == want)
}

/// 哪些记号在测试代码里：`#[cfg(test)]`、`#[test]`、`#[tokio::test]` 标着的那一项。
fn test_mask(toks: &[(usize, Tok)]) -> Vec<bool> {
    let mut mask = vec![false; toks.len()];
    let mut i = 0;
    while i < toks.len() {
        if !(is(toks.get(i), &Tok::Punct('#')) && is(toks.get(i + 1), &Tok::Punct('['))) {
            i += 1;
            continue;
        }
        // 这一条属性到哪儿结束
        let mut j = i + 2;
        let mut depth = 1;
        while j < toks.len() && depth > 0 {
            match &toks[j].1 {
                Tok::Punct('[') => depth += 1,
                Tok::Punct(']') => depth -= 1,
                _ => {}
            }
            j += 1;
        }
        let attr: Vec<&Tok> = toks[i + 2..j - 1].iter().map(|(_, t)| t).collect();
        let cfg_test = attr
            == [
                &Tok::Ident("cfg".into()),
                &Tok::Punct('('),
                &Tok::Ident("test".into()),
                &Tok::Punct(')'),
            ];
        let test_fn = attr.last() == Some(&&Tok::Ident("test".into()));
        if cfg_test || test_fn {
            // 标着的那一项：到第一个 `;`（没有体）或者配对的 `}` 为止
            let mut k = j;
            let mut depth = 0;
            while k < toks.len() {
                match &toks[k].1 {
                    Tok::Punct(';') if depth == 0 => break,
                    Tok::Punct('{') => depth += 1,
                    Tok::Punct('}') => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                k += 1;
            }
            for m in &mut mask[i..=k.min(toks.len() - 1)] {
                *m = true;
            }
        }
        i = j;
    }
    mask
}

#[derive(Debug, Default, Clone, Copy)]
struct Seen {
    product: bool,
    test: bool,
    /// 产品代码里有一处的句子不只是占位符
    worded: bool,
}

/// 一处 `msg!` 的第一个参数：字面量，或者一个常量名。
enum First {
    Lit(String),
    Const(String),
}

struct Site {
    file: String,
    line: usize,
    first: First,
    /// `=>` 后面的第一个字符串字面量。`code!` 没有
    template: Option<String>,
    test: bool,
}

fn scan(root: &Path) -> (Vec<Site>, HashMap<String, String>) {
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    rust_files(&root.join("bin"), &mut files);
    files.sort();

    let mut sites = Vec::new();
    let mut consts = HashMap::new();
    for f in &files {
        let rel_path = f.strip_prefix(root).unwrap_or(f);
        let rel = rel_path.display().to_string();
        // 这个文件自己的例子不算
        if rel_path.ends_with("tests/msg_codes.rs") {
            continue;
        }
        // **按路径的组成部分判断**，Windows 上分隔符是 `\`
        let test_file = rel_path.components().any(|c| {
            let c = c.as_os_str();
            c.eq_ignore_ascii_case("tests")
                || c.eq_ignore_ascii_case("examples")
                || c.eq_ignore_ascii_case("benches")
        });
        let src = std::fs::read_to_string(f).unwrap();
        let toks = lex(&src);
        let mask = test_mask(&toks);
        for i in 0..toks.len() {
            // const NAME: &str = "…";
            if let (Tok::Ident(kw), Some((_, Tok::Ident(name)))) = (&toks[i].1, toks.get(i + 1))
                && kw == "const"
                && let Some(eq) =
                    (i + 2..(i + 8).min(toks.len())).find(|&k| toks[k].1 == Tok::Punct('='))
                && let Some((_, Tok::Str(v))) = toks.get(eq + 1)
                && toks[i + 2..eq]
                    .iter()
                    .any(|(_, t)| *t == Tok::Ident("str".into()))
            {
                consts.insert(name.clone(), v.clone());
            }

            let Tok::Ident(mac) = &toks[i].1 else {
                continue;
            };
            if !(mac == "msg" || mac == "code")
                || !is(toks.get(i + 1), &Tok::Punct('!'))
                || !is(toks.get(i + 2), &Tok::Punct('('))
            {
                continue;
            }
            let line = toks[i].0;
            let first = match toks.get(i + 3).map(|(_, t)| t) {
                Some(Tok::Str(s)) => First::Lit(s.clone()),
                Some(Tok::Ident(n)) if mac == "msg" => {
                    // 路径形式（`a::B`）取最后一段
                    let mut k = i + 3;
                    while is(toks.get(k + 1), &Tok::Punct(':'))
                        && is(toks.get(k + 2), &Tok::Punct(':'))
                    {
                        k += 3;
                    }
                    match toks.get(k).map(|(_, t)| t) {
                        Some(Tok::Ident(n)) => First::Const(n.clone()),
                        _ => First::Const(n.clone()),
                    }
                }
                other => panic!(
                    "{rel}:{line}: `{mac}!` 的第一个参数不是字符串字面量也不是常量（{other:?}）。\
                     消息码要写成字面量，这个测试才找得到它"
                ),
            };
            let template = if mac == "msg" {
                // 在这对括号里找 `=>`，取它后面的第一个字符串
                let mut k = i + 3;
                let mut depth = 1;
                let mut found = None;
                while k < toks.len() && depth > 0 {
                    match &toks[k].1 {
                        Tok::Punct('(' | '[' | '{') => depth += 1,
                        Tok::Punct(')' | ']' | '}') => depth -= 1,
                        Tok::Punct('=') if depth == 1 && is(toks.get(k + 1), &Tok::Punct('>')) => {
                            found = toks[k + 2..].iter().find_map(|(_, t)| match t {
                                Tok::Str(s) => Some(s.clone()),
                                _ => None,
                            });
                            break;
                        }
                        _ => {}
                    }
                    k += 1;
                }
                Some(found.unwrap_or_else(|| {
                    panic!("{rel}:{line}: 这处 `msg!` 找不到 `=>` 后面的英文原句")
                }))
            } else {
                None
            };
            sites.push(Site {
                file: rel.clone(),
                line,
                first,
                template,
                test: test_file || mask[i],
            });
        }
    }
    (sites, consts)
}

/// 句子里除了具名的 `{占位符}` 什么都没有。
///
/// **位置参数（`"{}"`）不算**：那是句子在别处写好了、拿来填进格式串的写法
/// （`msg!("adopt.takes_effect", … => "{}", note)`），界面照样要按码翻。
fn only_placeholders(t: &str) -> bool {
    let mut rest = t.trim();
    while let Some(s) = rest.strip_prefix('{') {
        let Some(end) = s.find('}') else {
            return false;
        };
        if s[..end].trim().is_empty() {
            return false;
        }
        rest = s[end + 1..].trim_start();
    }
    rest.is_empty()
}

/// 按源码生成的清单全文。
fn manifest(root: &Path) -> String {
    let (sites, consts) = scan(root);
    let mut codes: BTreeMap<String, Seen> = BTreeMap::new();
    for s in &sites {
        let code = match &s.first {
            First::Lit(c) => c.clone(),
            First::Const(n) => consts.get(n).cloned().unwrap_or_else(|| {
                panic!(
                    "{}:{}: `msg!({n}, …)` 里的 `{n}` 找不到一个 `const {n}: &str = \"…\"`",
                    s.file, s.line
                )
            }),
        };
        let e = codes.entry(code).or_default();
        if s.test {
            e.test = true;
        } else {
            e.product = true;
            if s.template.as_deref().is_none_or(|t| !only_placeholders(t)) {
                e.worded = true;
            }
        }
    }
    let mut out = String::from(
        "# Every message code core can emit, one per line, sorted.\n\
         # Generated: UPDATE_MSG_CODES=1 cargo test -p twcore --test msg_codes\n\
         # A second column marks codes a UI does not translate:\n\
         #   passthrough  the sentence is only placeholders (system or upstream text); show `text`\n\
         #   test         only produced by tests; never reaches a UI\n",
    );
    for (code, s) in &codes {
        out.push_str(code);
        if !s.product {
            out.push_str(" test");
        } else if !s.worded {
            out.push_str(" passthrough");
        }
        out.push('\n');
    }
    out
}

/// 从清单里读出码（去掉注释和标记）。
fn codes_in(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| l.split_whitespace().next().unwrap().to_string())
        .collect()
}

#[test]
fn the_manifest_lists_every_code_core_can_emit() {
    let root = workspace();
    let want = manifest(&root);
    let path = root.join(MANIFEST);
    if std::env::var_os(UPDATE).is_some() {
        std::fs::write(&path, &want).unwrap();
        return;
    }
    // Windows 上的检出可能把换行换成了 CRLF，内容一样就算一样
    let have = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    if have != want {
        let (h, w) = (codes_in(&have), codes_in(&want));
        let added: Vec<_> = w.difference(&h).collect();
        let gone: Vec<_> = h.difference(&w).collect();
        panic!(
            "{MANIFEST} is stale. Run `{UPDATE}=1 cargo test -p twcore --test msg_codes` and commit it.\n\
             new codes: {added:?}\nremoved codes: {gone:?}\n\
             (if both are empty, a code's marker changed)"
        );
    }
    // 桌面端读的是 tw-api 里的这个常量，它就是这个文件
    assert_eq!(tw_api::MSG_CODES.replace("\r\n", "\n"), want);
}

#[test]
fn the_scanner_reads_the_shapes_it_claims_to() {
    let src = r####"
const K: &str = "x.from_const";
fn f() {
    msg!("x.plain", a = 1 => "Hello {a}");
    tw_types::msg!("x.path" => "{detail}");
    msg!("x.positional", k = 1 => "{}", note);
    let t = (code!("x.table"), "Text");
    // msg!("x.comment" => "no")
    msg!(K, detail = e => "{detail}");
}
#[cfg(test)]
use tw_types::msg;
fn g() { msg!("x.after_cfg_use" => "Still product."); }
#[cfg(test)]
mod tests {
    fn h() { msg!("t.only" => "x"); }
}
"####;
    let dir = std::env::temp_dir().join(format!("tw-msg-codes-{}", std::process::id()));
    let crate_src = dir.join("crates/x/src");
    std::fs::create_dir_all(&crate_src).unwrap();
    std::fs::write(crate_src.join("lib.rs"), src).unwrap();
    let got = manifest(&dir);
    std::fs::remove_dir_all(&dir).unwrap();
    let body: Vec<&str> = got.lines().filter(|l| !l.starts_with('#')).collect();
    assert_eq!(
        body,
        [
            "t.only test",
            "x.after_cfg_use",
            "x.from_const passthrough",
            "x.path passthrough",
            "x.plain",
            "x.positional",
            "x.table",
        ]
    );
}
