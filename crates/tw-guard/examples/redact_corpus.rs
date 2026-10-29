//! 拿真实的源码树量一量脱敏规则：误报几处、扫一遍多久。
//!
//! ```text
//! cargo run --release -p tw-guard --example redact_corpus -- fp   <目录或文件>...
//! cargo run --release -p tw-guard --example redact_corpus -- perf <目录或文件>... | keys | lorem
//! ```
//!
//! - `fp`：每个文件当成一段贴进对话的正文扫一遍（全部内置规则都开），按规则数命中，列出
//!   每一处（打过码）。测试样本不算：`tests`、`fixtures`、`testdata` 一类目录整个跳过，
//!   Rust 文件从 `#[cfg(test)]` 起不看，`*.test.ts` 一类不看。
//! - `perf`：把这些文件拼成一个聊天请求体（每个文件一条消息，最多 4 MiB），出厂规则扫
//!   十一遍取中位数（`RX_ROUNDS` 改遍数）。改规则前后各跑一次比；`RX_OFF=规则,规则` 关掉
//!   这几条再量，看每一条各花多少。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tw_guard::redact::rules::{BUILTINS, RuleSet, masked, scan, scan_plain};

const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    ".next",
    ".astro",
    "vendor",
    "resources",
    "tests",
    "test",
    "__tests__",
    "fixtures",
    "__fixtures__",
    "testdata",
    "snapshots",
    "__snapshots__",
];

const EXTS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "md", "mdx", "yaml", "yml", "json", "toml", "py",
    "go", "sh", "swift", "css", "html", "astro", "txt", "sql", "ps1", "nsi", "plist", "lock",
    "xml", "env", "example", "ini", "cfg", "conf",
];

fn is_test_file(name: &str) -> bool {
    name.contains(".test.") || name.contains(".spec.") || name.ends_with("_test.go")
}

fn walk(p: &Path, out: &mut Vec<PathBuf>) {
    let Ok(meta) = std::fs::symlink_metadata(p) else {
        return;
    };
    if meta.is_dir() {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if SKIP_DIRS.contains(&name) {
            return;
        }
        let Ok(rd) = std::fs::read_dir(p) else {
            return;
        };
        let mut kids: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        kids.sort();
        for k in kids {
            walk(&k, out);
        }
    } else if meta.is_file() && meta.len() < 2 * 1024 * 1024 {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !is_test_file(name) && (EXTS.contains(&ext) || name.starts_with(".env")) {
            out.push(p.to_path_buf());
        }
    }
}

/// 文件内容，去掉测试部分。不是 UTF-8 的不要
fn content(p: &Path) -> Option<String> {
    let mut s = std::fs::read_to_string(p).ok()?;
    if p.extension().is_some_and(|e| e == "rs")
        && let Some(at) = s.find("#[cfg(test)]")
    {
        s.truncate(at);
    }
    Some(s)
}

fn files(args: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for a in args {
        walk(Path::new(a), &mut out);
    }
    out
}

fn fp(args: &[String]) {
    let all: Vec<&str> = BUILTINS.iter().map(|b| b.id).collect();
    let set = RuleSet::only(&all);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut n = 0;
    let mut bytes = 0;
    for p in files(args) {
        let Some(text) = content(&p) else { continue };
        n += 1;
        bytes += text.len();
        for h in scan_plain(&text, &set) {
            let value = &text[h.bytes.clone()];
            *counts.entry(h.rule.id().to_string()).or_default() += 1;
            println!(
                "{}\t{}\t{}",
                p.display(),
                h.rule.id(),
                masked(&h.rule, value)
            );
        }
    }
    eprintln!("scanned {n} files, {} KiB", bytes / 1024);
    for b in BUILTINS {
        let c = counts.get(b.id).copied().unwrap_or(0);
        if c > 0 {
            eprintln!(
                "{:>6}  {}{}",
                c,
                b.id,
                if b.on_by_default { "" } else { " (off)" }
            );
        }
    }
}

/// 一个请求体：`keys` 是 4 万个不同的 AWS 访问密钥（`tests/scale.rs` 那一份），`lorem` 是一段
/// 拉丁文填充加几百个 GitHub 令牌（`reported_items_on_a_large_body_cost_linear_time` 那一份），
/// 否则是这些文件拼成的聊天请求
fn body_of(args: &[String]) -> String {
    let mut msgs = Vec::new();
    match args.first().map(String::as_str) {
        Some("keys") => {
            let keys: Vec<String> = (0..40_000usize)
                .map(|i| {
                    let mut n = i;
                    let tail: String = (0..16)
                        .map(|_| {
                            let c = char::from(b'A' + (n % 26) as u8);
                            n /= 26;
                            c
                        })
                        .collect();
                    format!("AKIA{tail}")
                })
                .collect();
            msgs.push(serde_json::json!({"role": "user", "content": keys.join(" ")}));
        }
        Some("lorem") => {
            for rep in 0..50 {
                for i in 0..80 {
                    msgs.push(serde_json::json!({"role": "user", "content": format!("token ghp_{i:036} rep {rep}")}));
                }
            }
            let pad = "lorem ipsum dolor sit amet ".repeat(40);
            for _ in 0..(1024 * 1024 / (pad.len() + 40)) {
                msgs.push(serde_json::json!({"role": "assistant", "content": pad}));
            }
        }
        _ => {
            let mut size = 0;
            for p in files(args) {
                let Some(text) = content(&p) else { continue };
                size += text.len();
                msgs.push(serde_json::json!({"role": "user", "content": text}));
                if size > 4 * 1024 * 1024 {
                    break;
                }
            }
        }
    }
    serde_json::json!({"model": "m", "messages": msgs}).to_string()
}

fn perf(args: &[String]) {
    let body = body_of(args);
    let off: Vec<String> = std::env::var("RX_OFF")
        .map(|v| v.split(',').map(str::to_string).collect())
        .unwrap_or_default();
    let set = RuleSet::build(&[], &off, []).unwrap_or_default();
    let mut times: Vec<Duration> = Vec::new();
    let mut found = 0;
    let rounds: usize = std::env::var("RX_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(11);
    for _ in 0..rounds.max(11) {
        let t = Instant::now();
        found = scan(&body, &set).len();
        times.push(t.elapsed());
    }
    times.sort_unstable();
    eprintln!(
        "body {} KiB, {} hits, median {:?} (min {:?}, max {:?})",
        body.len() / 1024,
        found,
        times[5],
        times[0],
        times[10]
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("fp") => fp(&args[1..]),
        Some("perf") => perf(&args[1..]),
        _ => eprintln!("usage: redact_corpus fp|perf <paths>..."),
    }
}
