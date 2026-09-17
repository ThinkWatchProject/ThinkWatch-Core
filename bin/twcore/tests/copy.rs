//! 产品代码里给人看的文字是书面语。
//!
//! 错误、提示、诊断结论、日志、命令行输出，最后都会被人读到：界面把
//! 控制面返回的句子原样显示，AI 客户端把网关的错误打印在终端里。桌面端
//! 的文案规则是书面语、不用第一和第二人称、不解释内部机制，它自己的
//! `copy.test.ts` 只查得到界面代码 —— core 发过去的句子由这里查。
//!
//! 查的是**字符串字面量**，不是注释：注释是写给读代码的人的，不受这条
//! 规则管。测试代码也不查，那里的中文是假装用户写的内容。

use std::path::{Path, PathBuf};

/// 桌面端 `copy.test.ts` 拒绝的说法，加上 core 里出现过的那些。
const SPOKEN: &[&str] = &[
    // 桌面端同一份清单
    "就好",
    "就行",
    "怎么",
    "哪家",
    "哪儿",
    "啥",
    "扫一眼",
    "攒着",
    "别的设备",
    "这么多秒",
    "条子",
    "亏钱",
    "回本",
    "送上去",
    "白跑",
    "花钱",
    "随便点",
    "稍等",
    "要是",
    "马上",
    "多半",
    "还没",
    "没法",
    "看看",
    "接下来",
    "为什么",
    "连不上",
    "起不来",
    "读不动",
    "写好了",
    "什么都不用",
    "拿到",
    "删掉",
    "发出去",
    "换回来",
    "在听",
    "只看",
    "只计到",
    "那一刻",
    "一分钱",
    "一字节",
    "打码",
    "这家",
    "那家",
    "一家",
    "每家",
    "知道了",
    "吗？",
    // 人称
    "你",
    "您",
    "我们",
    // 同一个概念只有一个叫法
    "关掉",
    "测一下",
    "算账",
    "花费",
    "成本",
    // 开发流程的词
    "仓库",
    "编译",
    "调试",
    "堆栈",
    // core 里改掉过的说法
    "读不懂",
    "读不通",
    "读不了",
    "写不了",
    "取不到",
    "盯不住",
    "不认识",
    "对不上",
    "那把",
    "这把",
    "——",
    "**",
];

/// 不是给人读的句子，而是存盘格式的一部分。
///
/// 接管时写在客户端配置旁边的 `*.thinkwatch.json` 用这几个中文键，还原时
/// 按键名读回来。改掉它们，已经接管的客户端就还原不了。
const FORMAT: &[&str] = &["怎么手动还原", "文件是我们建的"];

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
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// 一段源码里的字符串字面量，连同所在行。**跳过注释和字符字面量**，
/// 原始字符串（`r#"…"#`）按原样取。
fn literals(src: &str) -> Vec<(usize, String)> {
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
                }
            }
            'r' if (i == 0 || !ident(c[i - 1]))
                && matches!(c.get(i + 1), Some('"') | Some('#')) =>
            {
                let mut j = i + 1;
                let mut hashes = 0;
                while c.get(j) == Some(&'#') {
                    hashes += 1;
                    j += 1;
                }
                if c.get(j) != Some(&'"') {
                    i += 1;
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
                out.push((line, lit.clone()));
                line += lit.matches('\n').count();
                i = end + 1 + hashes;
            }
            '"' => {
                let start = i + 1;
                let mut k = start;
                while k < c.len() && c[k] != '"' {
                    k += if c[k] == '\\' { 2 } else { 1 };
                }
                let end = k.min(c.len());
                let lit: String = c[start..end].iter().collect();
                out.push((line, lit.clone()));
                line += lit.matches('\n').count();
                i = end + 1;
            }
            _ => i += 1,
        }
    }
    out
}

#[test]
fn product_text_is_written_not_spoken() {
    let root = workspace();
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    rust_files(&root.join("bin"), &mut files);
    files.sort();

    let mut found = Vec::new();
    for f in &files {
        let rel = f.strip_prefix(&root).unwrap_or(f).display().to_string();
        // 集成测试和这个文件自己都不查
        if rel.contains("/tests/") {
            continue;
        }
        let src = std::fs::read_to_string(f).unwrap();
        // 看的是产品代码那一半（测试模块在文件末尾）
        let product = src.split("#[cfg(test)]").next().unwrap();
        for (line, lit) in literals(product) {
            // SQL 语句里的注释不是给人读的文字
            if FORMAT.contains(&lit.as_str()) || lit.contains("CREATE TABLE") {
                continue;
            }
            for w in SPOKEN.iter().filter(|w| lit.contains(**w)) {
                found.push(format!("{rel}:{line}  「{w}」  {lit}"));
            }
        }
    }

    // 内置扫描规则的说明会原样出现在发现和工具调用告警里
    let rules = root.join("crates/tw-scan/data/rules.yaml");
    for (n, l) in std::fs::read_to_string(&rules).unwrap().lines().enumerate() {
        let Some(why) = l.trim().strip_prefix("why:") else {
            continue;
        };
        for w in SPOKEN.iter().filter(|w| why.contains(**w)) {
            found.push(format!(
                "crates/tw-scan/data/rules.yaml:{}  「{w}」  {why}",
                n + 1
            ));
        }
    }

    assert!(
        found.is_empty(),
        "这些文字用了口语或人称：\n{}",
        found.join("\n")
    );
}

#[test]
fn the_literal_reader_skips_comments_and_keeps_strings() {
    let src = r####"
// "你好" 在注释里
let a = "上游响应超时"; /* "我们" */
let b = r#"原始 "字符串""#;
let c = '中';
fn f<'a>(x: &'a str) {}
"####;
    let got: Vec<String> = literals(src).into_iter().map(|(_, s)| s).collect();
    assert_eq!(got, ["上游响应超时", "原始 \"字符串\""]);
}
