//! 工具调用碰 ThinkWatch 自己的数据目录。
//!
//! 那个目录里有明文的全部上游密钥（`config.yaml`），也有管着这几项防护的设置。
//! 读它，一步就拿走全部凭据；改它，就能关掉管着自己的防护 —— 连文件都不必写：
//! 拿着文件里的控制面钥匙经 socket 改配置，是同一件事。文件的 `0600` 挡的是别的
//! 用户，挡不住以当前用户身份运行的智能体，所以这一道放在网关看得见的地方：模型
//! 发出的工具调用。
//!
//! # 为什么是代码，不是一条正则
//!
//! **提到这个路径不等于动它。**改文档、写注释、回答「配置在哪」的工具调用里到处是
//! `~/.thinkwatch`；路径一出现就切断，每个编辑这些文档的会话都会被误切（和
//! `ordinary_documentation_does_not_trip_the_rules` 同一个道理）。所以按参数的
//! **角色**看：
//!
//! - 参数是 JSON（函数调用）：只看路径类的键（`file_path`、`path`、`cwd`、`workdir`……）
//!   和命令类的键（`command`、`cmd`、`code`、`args`……）的值；`content`、`new_string`、
//!   `description` 这类正文不看。任何值里有补丁的文件头（`*** Update File: …`）也算
//!   —— 那是在改这个文件。
//!
//!   **JSON 不解析，逐个字符串往下读**：流式时每来一片就对攒到一半的参数查一遍
//!   （[`super::wall`]），超过上限的参数也只攒前一段 —— 两种都不是完整的 JSON。解析不了
//!   就退回按原文看，那会在一段还没收齐的正文里误切。
//! - 参数不是 JSON（自定义工具的原文，比如 Codex 的 `apply_patch`）：补丁的文件头，
//!   或者同一行里先有一个读写文件的命令、后有这个目录。
//!
//! # 认得哪些位置
//!
//! 默认的那几处：`~/.thinkwatch`（任何名为 `.thinkwatch` 的一级路径）、Windows 的
//! `%APPDATA%\ThinkWatch`、服务器上的 `/var/lib/thinkwatch` 和 `/etc/thinkwatch`
//! （`env` 里是密钥）。`THINKWATCH_HOME` 指到别处的这里不知道，要用户自己加一条
//! 自定义规则。**不分大小写**：macOS 默认的文件系统不分。

use std::ops::Range;
use std::sync::OnceLock;

use regex::Regex;

/// 指进数据目录的一段路径。前后的边界让 `.thinkwatch-lite`、`foo.thinkwatch`、
/// `~/Dev/thinkwatch-core` 都不算。分隔符写成 `[\\/]+`：JSON 原文里 Windows 的
/// 反斜杠是成对的。
const DIR: &str = concat!(
    r#"(?:(?:^|[\s"'`=:(,\\/~])\.thinkwatch"#,
    r#"|(?:%appdata%|\$env:appdata|\$\{?appdata\}?|appdata[\\/]+roaming)[\\/]+thinkwatch"#,
    r#"|(?:^|[\s"'`=:(,])/(?:var/lib|etc)/thinkwatch)"#,
    r#"(?:[\\/\s"'`;|&),]|$)"#,
);

/// 读写文件的命令。原文（不是 JSON）里，同一行先有它、后有数据目录才算。
///
/// **不收常见英文词**（`open`、`code`、`type`、`copy`、`more`）：原文里也有说明文字。
const VERB: &str = concat!(
    r"(?:\b(?:cat|bat|head|tail|less|cp|mv|scp|rsync|sed|awk|grep|rg|vi|vim|nano|sqlite3",
    r"|curl|socat|nc|rm|ls|cd|tee|chmod|get-content|set-content|remove-item|notepad)\b|>)",
);

/// 补丁里「改哪个文件」的那一行。
const PATCH: &str = r"^\*\*\*\s*(?:add file|update file|delete file|move to):";

fn dir() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        crate::bounded(&format!("(?i){DIR}")).expect("the data-directory pattern compiles")
    })
}

/// 原文里算数的一行：补丁文件头，或者命令在前、目录在后。
fn line() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        crate::bounded(&format!("(?im)(?:{PATCH}|{VERB})[^\n]*?{DIR}"))
            .expect("the command-line pattern compiles")
    })
}

/// 任何一个值里的补丁文件头（JSON 里补丁可能放在 `input`、`patch` 之类的键下）。
fn patch() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        crate::bounded(&format!("(?im){PATCH}[^\n]*?{DIR}")).expect("the patch pattern compiles")
    })
}

/// 这个键的值是路径或命令，而不是正文。
fn operative(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    ["path", "file", "dir", "cwd", "folder"]
        .iter()
        .any(|w| k.contains(w))
        || k.ends_with("url")
        || k.ends_with("uri")
        || matches!(
            k.as_str(),
            "command" | "commands" | "cmd" | "script" | "code" | "args" | "argv"
        )
}

/// `within` 那一段里第一处数据目录，返回在 `s` 里的位置。
fn dir_in(s: &str, within: Range<usize>) -> Option<Range<usize>> {
    let m = dir().find(&s[within.clone()])?;
    Some(within.start + m.start()..within.start + m.end())
}

/// 一个字符串值里算数的命中，返回那一段路径（给摘录定位用）。
///
/// `closed` 为假是还没收齐的最后一个值：目录名后面得已经跟着分隔符，否则
/// `~/.thinkwatch` 后面也许是 `-backup`。
fn hit_in(s: &str, operative_key: bool, closed: bool) -> Option<String> {
    let m = if operative_key {
        dir_in(s, 0..s.len())
    } else {
        patch().find(s).and_then(|p| dir_in(s, p.range()))
    }?;
    let ends_at_eof =
        m.end == s.len() && !s[..m.end].ends_with(|c: char| "\\/ \t\n\"'`;|&),".contains(c));
    if !closed && ends_at_eof {
        return None;
    }
    Some(s[token(s, m)].to_string())
}

/// 读到哪一层了：对象里记着当前的键，数组里的值算外层那个键的。
enum Frame {
    Obj { key: String, at_key: bool },
    Arr,
}

/// 逐个字符串读一段（也许没收齐的）JSON，找第一处算数的命中。
fn json_hit(args: &str) -> Option<String> {
    let b = args.as_bytes();
    let mut stack: Vec<Frame> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'{' => stack.push(Frame::Obj {
                key: String::new(),
                at_key: true,
            }),
            b'[' => stack.push(Frame::Arr),
            b'}' | b']' => {
                stack.pop();
            }
            b',' => {
                if let Some(Frame::Obj { at_key, .. }) = stack.last_mut() {
                    *at_key = true;
                }
            }
            b'"' => {
                let (raw, closed, next) = string_at(args, i + 1);
                i = next;
                let s = unescape(raw);
                if let Some(Frame::Obj { key, at_key }) = stack.last_mut()
                    && *at_key
                {
                    *key = s;
                    *at_key = false;
                    continue;
                }
                let op = stack
                    .iter()
                    .rev()
                    .find_map(|f| match f {
                        Frame::Obj { key, .. } => Some(operative(key)),
                        Frame::Arr => None,
                    })
                    .unwrap_or(false);
                if let Some(t) = hit_in(&s, op, closed) {
                    return Some(t);
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// 从 `start`（开引号之后）读到闭引号：`(原文, 闭合了没有, 下一个位置)`。
fn string_at(s: &str, start: usize) -> (&str, bool, usize) {
    let b = s.as_bytes();
    let mut i = start;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return (&s[start..i], true, i + 1),
            _ => i += 1,
        }
    }
    (&s[start..], false, b.len())
}

/// JSON 字符串转义还原。**宽松**：没收齐的结尾、坏掉的 `\u` 直接丢掉。
fn unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut it = raw.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('u') => {
                let hex: String = it.by_ref().take(4).collect();
                if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(ch);
                }
            }
            Some('b' | 'f') | None => {}
            Some(other) => out.push(other),
        }
    }
    out
}

/// 摘录：命中所在的那一整段路径（`~/.thinkwatch/config.yaml`），而不只是目录名。
fn token(s: &str, r: Range<usize>) -> Range<usize> {
    let stop = |c: u8| c.is_ascii_whitespace() || b"\"'`=(,;|&<>".contains(&c);
    let b = s.as_bytes();
    let mut start = r.start;
    // 正则把前面那个分隔符也吃进来了
    while start < r.end && stop(b[start]) {
        start += 1;
    }
    while start > 0 && !stop(b[start - 1]) {
        start -= 1;
    }
    let mut end = r.end;
    while end < b.len() && !stop(b[end]) {
        end += 1;
    }
    // 同理，后面那个分隔符；以及 JSON 转义留下的反斜杠
    while end > start && (stop(b[end - 1]) || b[end - 1] == b'\\') {
        end -= 1;
    }
    start..end
}

pub(super) fn find(args: &str) -> Option<Range<usize>> {
    // 绝大多数调用在这里就结束了：参数里根本没有这个目录
    let first = dir().find(args)?;
    if matches!(args.trim_start().as_bytes().first(), Some(b'{' | b'[')) {
        let hit = json_hit(args)?;
        // 摘录落在原文里那一段；Windows 路径在 JSON 原文里转义过、找不到原样的，
        // 退回原文里第一处
        return Some(
            args.find(&hit)
                .map(|i| i..i + hit.len())
                .unwrap_or_else(|| token(args, first.range())),
        );
    }
    let l = line().find(args)?;
    Some(token(args, dir_in(args, l.range())?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fires(args: &str) -> bool {
        find(args).is_some()
    }

    fn excerpt(args: &str) -> &str {
        let r = find(args).expect("should fire");
        &args[r]
    }

    #[test]
    fn reading_or_changing_the_data_directory_fires() {
        for v in [
            // Claude Code：Bash、Read、Edit
            json!({"command": "cat ~/.thinkwatch/config.yaml", "description": "Show the config"}),
            json!({"file_path": "/Users/alice/.thinkwatch/config.yaml"}),
            json!({"file_path": "/home/bob/.thinkwatch/config.yaml",
                   "old_string": "mode: enforce", "new_string": "mode: off"}),
            // Codex：shell 的命令是数组，exec_command 是 cmd
            json!({"command": ["bash", "-lc", "sqlite3 $HOME/.thinkwatch/data.db .dump"], "workdir": "/tmp"}),
            json!({"cmd": "curl --unix-socket ~/.thinkwatch/twcore.sock http://x/config"}),
            json!({"command": "echo 'security: {}' > ${HOME}/.thinkwatch/config.yaml"}),
            // 工作目录就在里面
            json!({"command": "cat config.yaml", "cwd": "/Users/alice/.thinkwatch"}),
            // 写代码去读
            json!({"code": "open(os.path.expanduser('~/.thinkwatch/config.yaml')).read()"}),
            json!({"code": "(Path.home() / '.thinkwatch' / 'config.yaml').read_text()"}),
            // 嵌套、数组里的路径
            json!({"input": {"command": "cat ~/.thinkwatch/config.yaml"}}),
            json!({"paths": ["README.md", "~/.thinkwatch/config.yaml"]}),
            json!({"uri": "file:///Users/alice/.thinkwatch/config.yaml"}),
            // 补丁放在随便哪个键下
            json!({"input": "*** Begin Patch\n*** Update File: /Users/a/.thinkwatch/config.yaml\n@@\n-  mode: enforce\n+  mode: off\n*** End Patch"}),
            // Windows
            json!({"path": r"C:\Users\a\AppData\Roaming\ThinkWatch\config.yaml"}),
            json!({"command": r"type %APPDATA%\ThinkWatch\config.yaml"}),
            json!({"command": r"Get-Content $env:APPDATA\ThinkWatch\config.yaml"}),
            // 服务器
            json!({"command": "sudo cat /etc/thinkwatch/env"}),
            json!({"command": "ls /var/lib/thinkwatch/"}),
            // macOS 不分大小写
            json!({"file_path": "/Users/alice/.ThinkWatch/config.yaml"}),
        ] {
            let args = v.to_string();
            assert!(fires(&args), "漏了：{args}");
        }
    }

    #[test]
    fn raw_tool_input_fires_on_a_patch_header_or_a_command() {
        assert!(fires(
            "*** Begin Patch\n*** Update File: ~/.thinkwatch/config.yaml\n@@\n-x\n+y\n*** End Patch"
        ));
        assert!(fires("cat ~/.thinkwatch/config.yaml"));
        assert!(fires("cd ~/.thinkwatch && ls"));
    }

    #[test]
    fn mentioning_the_directory_does_not_fire() {
        // **误报是这个功能最大的敌人**：编辑说到这个目录的文档，是最常见的情形
        for v in [
            json!({"file_path": "/Users/a/project/README.md", "old_string": "x",
                   "new_string": "Core keeps its data in ~/.thinkwatch."}),
            json!({"file_path": "docs/config.md", "content": "`~/.thinkwatch/config.yaml` holds the keys"}),
            json!({"command": "ls", "description": "the config lives in ~/.thinkwatch"}),
            json!({"input": "*** Begin Patch\n*** Update File: README.md\n+Data lives in ~/.thinkwatch.\n*** End Patch"}),
            // 名字像、但不是那个目录
            json!({"command": "cd ~/Dev/thinkwatch-lite && pnpm test"}),
            json!({"command": "git clone https://github.com/ThinkWatchProject/thinkwatch.github.io"}),
            json!({"command": "cat ~/.thinkwatch-backup/notes.txt"}),
            json!({"command": "THINKWATCH_HOME=/tmp/twl twcore serve"}),
            json!({"url": "https://thinkwat.ch/docs/core/config"}),
        ] {
            let args = v.to_string();
            assert!(!fires(&args), "误报了：{args}");
        }
        // 原文里的说明文字
        assert!(!fires(
            "ThinkWatch keeps its configuration in ~/.thinkwatch/config.yaml."
        ));
        assert!(!fires(
            "*** Begin Patch\n*** Update File: docs/a.md\n+see ~/.thinkwatch/config.yaml\n*** End Patch"
        ));
    }

    #[test]
    fn half_received_arguments_are_read_by_key_too() {
        // 流式时每来一片就查一遍攒到一半的参数：路径一到就认得出
        assert!(fires(r#"{"file_path":"/Users/a/.thinkwatch/con"#));
        assert!(fires(
            r#"{"command":["bash","-lc","sqlite3 ~/.thinkwatch/data"#
        ));
        // 目录名后面还没到分隔符时不急：也许是 `.thinkwatch-backup`
        assert!(!fires(r#"{"command":"cat ~/.thinkwatch"#));
        assert!(fires(r#"{"command":"cat ~/.thinkwatch"}"#));
        // 半截的正文照样不看 —— 退回按原文看的话，`cat` 和 `>` 会让它误切
        assert!(!fires(
            r#"{"file_path":"README.md","new_string":"a -> b; run cat ~/.thinkwatch/config.yaml to see"#
        ));
        // 超过上限被截断的参数：只要路径在前面那一段里
        let long = format!(
            r#"{{"file_path":"/Users/a/.thinkwatch/config.yaml","content":"{}"#,
            "x".repeat(100_000)
        );
        assert!(fires(&long));
    }

    #[test]
    fn escapes_in_the_json_text_are_undone_before_matching() {
        // 补丁里的换行在 JSON 原文里是 `\n`，文件头要还原了才认得出
        let args = json!({"patch": "*** Begin Patch\n*** Delete File: ~/.thinkwatch/config.yaml\n*** End Patch"})
            .to_string();
        assert!(fires(&args));
        assert_eq!(unescape(r#"a\"b\\c\u0041\"#), "a\"b\\cA");
    }

    #[test]
    fn the_excerpt_is_the_whole_path() {
        let args = json!({"command": "cat ~/.thinkwatch/config.yaml | head"}).to_string();
        assert_eq!(excerpt(&args), "~/.thinkwatch/config.yaml");
        // 正文里先提到了，摘录仍落在真正动手的那一处
        let args = json!({"description": "read ~/.thinkwatch", "file_path": "/Users/a/.thinkwatch/data.db"})
            .to_string();
        assert_eq!(excerpt(&args), "/Users/a/.thinkwatch/data.db");
        assert_eq!(
            excerpt("cat ~/.thinkwatch/config.yaml"),
            "~/.thinkwatch/config.yaml"
        );
        // Windows 路径在 JSON 原文里转义过：退回原文里那一段，照样是一整段路径
        let args =
            json!({"path": r"C:\Users\a\AppData\Roaming\ThinkWatch\config.yaml"}).to_string();
        assert!(excerpt(&args).contains("ThinkWatch"), "{}", excerpt(&args));
    }
}
