//! 拿真实的源码树量一量脱敏规则：误报几处、扫一遍多久。
//!
//! ```text
//! cargo run --release -p tw-guard --example redact_corpus -- fp   <目录或文件>...
//! cargo run --release -p tw-guard --example redact_corpus -- perf <目录或文件>... | keys | lorem | chat-en | chat-zh
//! ```
//!
//! - `fp`：每个文件当成一段贴进对话的正文扫一遍（全部内置规则都开），按规则数命中，列出
//!   每一处（打过码）。测试样本不算：`tests`、`fixtures`、`testdata` 一类目录整个跳过，
//!   Rust 文件从 `#[cfg(test)]` 起不看，`*.test.ts` 一类不看。
//! - `perf`：把这些文件拼成一个聊天请求体（每个文件一条消息，最多 4 MiB），出厂规则扫
//!   十一遍取中位数（`RX_ROUNDS` 改遍数）。改规则前后各跑一次比；`RX_OFF=规则,规则` 关掉
//!   这几条再量，看每一条各花多少。`RX_SET` 换一组规则：`defaults`（出厂，缺省）、`all`
//!   （内置的全开）、`custom`（内置的全关，只有 20 条「包含」加 20 条正则的自定义规则）。
//!   只用公开的接口，拷到别的版本上照样跑，前后比的是同一份语料。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tw_guard::redact::rules::{BUILTINS, Pattern, RuleSet, masked, scan, scan_plain};

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
/// `chat-en`、`chat-zh` 是约 1 MiB 的英文、中文对话（[`chat`]），否则是这些文件拼成的聊天请求
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
        Some("chat-en") => msgs = chat(EN_LINES, b' '),
        Some("chat-zh") => msgs = chat(ZH_LINES, 0),
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

/// 英文对话的句子：问答里常见的说法，夹着数字、文件名、命令。都是编的，没有真实的个人信息
const EN_LINES: &[&str] = &[
    "Can you help me figure out why the build fails on the release branch?",
    "Sure, could you paste the full error output and the command you ran?",
    "It says the linker cannot find the library, even though it is installed.",
    "That usually means the search path is missing; check which directories the linker looks in.",
    "I tried setting the path in the config file but nothing changed after a restart.",
    "Let's take it step by step and confirm the environment variable is actually exported.",
    "The meeting moved to Thursday afternoon, so we have two more days for the review.",
    "Here is a short summary of the design: requests go through a queue and a pool of workers.",
    "Each worker pulls a job, runs it with a timeout of 30 seconds, and reports the result.",
    "If a job fails three times in a row, it is moved to a separate list for manual review.",
    "What happens when the queue is full and new requests keep arriving?",
    "In that case the server returns a 429 status and the client retries with a backoff.",
    "I would like the retry delay to grow slowly at first and then level off around one minute.",
    "Please rewrite this paragraph so it sounds more formal and a little shorter.",
    "The quarterly report shows revenue up 12 percent, mostly from the new subscription plan.",
    "Could you explain the difference between a process and a thread in simple terms?",
    "A process has its own memory space, while threads inside one process share memory.",
    "That is why threads can talk to each other quickly but also step on each other's data.",
    "Thanks, that makes sense. Now how would I protect a shared counter?",
    "You can wrap it in a mutex, or use an atomic integer if all you need is an increment.",
    "I am writing a story about a lighthouse keeper who finds a message in a bottle.",
    "The keeper reads the note twice, then folds it carefully and puts it in his coat pocket.",
    "Give me three ideas for a weekend trip that is less than two hours away by train.",
    "You could visit the old harbor town, hike along the river valley, or spend a day at the museum quarter.",
    "Our test suite takes almost twenty minutes; which parts should we run in parallel?",
    "Start by splitting the slow integration tests from the fast unit tests and caching dependencies.",
    "The function returns early when the list is empty, so the error never shows up in that case.",
    "I renamed the variable to make it clearer, and the warning went away after that.",
    "Is there a simple way to compare two large text files and see only the changed lines?",
    "Most editors have a built-in diff view, and the command line diff tool works well too.",
    "Let me know if you want a checklist for the release, including the steps for the changelog.",
    "We should also double check the migration on a copy of the production data before Friday.",
    "The chart looks fine on a laptop but the labels overlap on a narrow phone screen.",
    "Try rotating the labels or showing every other tick when the width is below a threshold.",
    "Write a polite reply declining the invitation, and suggest meeting next month instead.",
    "I appreciate the invitation, but I will be traveling that week; could we meet in early March?",
    "How many tokens does a typical page of English text use, roughly speaking?",
    "A page of plain prose is often around five hundred to seven hundred tokens, depending on the wording.",
    "Here is the function:\n\n```python\ndef total(items):\n    return sum(i.price * i.qty for i in items)\n```",
    "Run `cargo test --release` again and send me the last twenty lines of the output.",
];

/// 中文对话的句子：同上，夹着英文词、数字、标点
const ZH_LINES: &[&str] = &[
    "帮我看看这个报错是怎么回事，昨天还能正常编译。",
    "可以把完整的错误信息和你执行的命令贴出来吗？",
    "提示找不到动态库，可是我确认已经装好了。",
    "这通常是链接器的搜索路径里没有那个目录，先确认一下环境变量有没有导出。",
    "会议改到周四下午了，所以评审还有两天时间。",
    "整体设计是这样的：请求先进队列，再由一组工作线程依次处理。",
    "每个任务的超时是 30 秒，连续失败三次就转入人工复核的列表。",
    "队列满了以后新来的请求怎么办？",
    "服务器会返回 429，客户端按退避策略重试，最长等一分钟。",
    "请把下面这段话改得正式一些，篇幅再短一点。",
    "本季度收入同比增长 12%，主要来自新的订阅套餐。",
    "能不能用通俗的话解释一下进程和线程的区别？",
    "进程有各自独立的内存空间，同一个进程里的线程共享内存。",
    "所以线程之间通信快，但也容易互相改坏对方的数据。",
    "明白了，那共享的计数器该怎么保护？",
    "可以用互斥锁包起来；只是加一的话，原子整数就够了。",
    "我在写一个故事，讲一个灯塔看守人捡到了漂流瓶里的信。",
    "他把信读了两遍，仔细叠好，放进了大衣口袋。",
    "推荐三个周末出游的地方，坐火车两小时以内能到。",
    "可以去老港口小镇、沿着河谷徒步，或者在博物馆区待一天。",
    "我们的测试要跑将近二十分钟，哪些部分可以并行？",
    "先把慢的集成测试和快的单元测试分开，再把依赖缓存起来。",
    "列表为空的时候函数提前返回了，所以这种情况下不会报错。",
    "我把变量改了个更清楚的名字，警告就消失了。",
    "有没有简单的办法比较两个大文本文件，只看改动的行？",
    "大多数编辑器都自带对比视图，命令行的 diff 也很好用。",
    "需要的话我可以给你列一份发版检查清单，包括更新日志的步骤。",
    "周五之前还要在生产数据的副本上把迁移再跑一遍。",
    "图表在笔记本上显示正常，在手机窄屏上标签会重叠。",
    "宽度小于某个值时，把标签旋转一下，或者隔一个显示一个。",
    "帮我写一封委婉的回信，谢绝这次邀请，改约下个月见面。",
    "感谢邀请，不过那一周我在外地出差，三月初再约可以吗？",
    "一页中文大概会用多少个 token？",
    "一页正文通常在几百到一千个 token 之间，取决于用词。",
    "这个接口返回的 JSON 里少了 total 字段，前端拿到的是 undefined。",
    "把 `cargo test --release` 再跑一遍，把最后二十行输出发给我。",
    "嗯，好的，我先试试，有问题再来问你。",
    "数据库连接池的大小设成 CPU 核数的两倍左右比较合适。",
    "这段代码在 macOS 上没问题，在 Linux 上偶尔会卡住。",
    "先加点日志，看看卡在哪一步，再决定要不要换实现。",
];

/// 一段约 1 MiB 的对话：句子按固定的伪随机顺序拼，每条消息 3 到 12 句，用户和助手交替。
/// 不用随机数库，每次生成的都一样，前后比的是同一份语料
fn chat(lines: &[&str], sep: u8) -> Vec<serde_json::Value> {
    let mut msgs = Vec::new();
    let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as usize
    };
    let mut size = 0;
    while size < 1024 * 1024 {
        let k = 3 + next(10);
        let mut m = String::new();
        for j in 0..k {
            if j > 0 && sep != 0 {
                m.push(char::from(sep));
            }
            if j > 0 && next(5) == 0 {
                m.push_str("\n\n");
            }
            m.push_str(lines[next(lines.len())]);
        }
        size += m.len();
        let role = if msgs.len() % 2 == 0 {
            "user"
        } else {
            "assistant"
        };
        msgs.push(serde_json::json!({"role": role, "content": m}));
    }
    msgs
}

/// `RX_SET=custom` 用的自定义规则：20 条按原样找的（一半不分大小写），20 条正则（有固定文字
/// 的、没有的、带 `^` `$` 的都有）。挑的是源码里会出现、也不会出现的几种
fn custom_set() -> RuleSet {
    const CONTAINS: &[&str] = &[
        "ACME-INTERNAL",
        "project-nightingale",
        "do not distribute",
        "confidential",
        "TODO(security)",
        "hunter2",
        "internal.example.com",
        "x-internal-token",
        "PRIVATE BUILD",
        "staging-db",
        "corp-vpn",
        "employee_id",
        "salary",
        "ssn_last4",
        "customer_secret",
        "PROD_ONLY",
        "redacted-by-legal",
        "codename",
        "unreleased",
        "Sapphire",
    ];
    const REGEX: &[&str] = &[
        r"MYCORP-\d{6}",
        r"\bEMP\d{5}\b",
        r"(?i)secret_[a-z]+",
        r"\d{6}",
        r"[A-Z]{3}-\d{4}-[A-Z]{2}",
        r"^#!.*$",
        r"(?m)^\s*password\s*=.*$",
        r"ticket #\d+",
        r"\bINV-\d{8}\b",
        r"(?i)api[_-]?secret",
        r"[a-f0-9]{40}",
        r"\bCASE-[0-9]{5}\b",
        r"(?i)\binternal only\b",
        r"user_[0-9]{7}",
        r"\b10\.0\.\d+\.\d+\b",
        r"proj_[A-Za-z0-9]{24}",
        r"(?i)passphrase:\s*\S+",
        r"[A-Z][a-z]+ [A-Z][a-z]+, Esq\.",
        r"acct-\d{4}-\d{4}",
        r"\$\{SECRET_[A-Z_]+\}",
    ];
    let mut set = RuleSet::none();
    for (i, t) in CONTAINS.iter().enumerate() {
        let p = Pattern {
            text: (*t).to_string(),
            regex: false,
            case_sensitive: i % 2 == 0,
        };
        set = set.with_rule(&format!("contains-{i}"), p, None).unwrap();
    }
    for (i, t) in REGEX.iter().enumerate() {
        set = set.with_custom(&format!("regex-{i}"), t).unwrap();
    }
    set
}

fn perf(args: &[String]) {
    let body = body_of(args);
    let off: Vec<String> = std::env::var("RX_OFF")
        .map(|v| v.split(',').map(str::to_string).collect())
        .unwrap_or_default();
    let set = match std::env::var("RX_SET").as_deref() {
        Ok("all") => {
            let all: Vec<String> = BUILTINS.iter().map(|b| b.id.to_string()).collect();
            RuleSet::build(&all, &off, []).unwrap_or_default()
        }
        Ok("custom") => custom_set(),
        _ => RuleSet::build(&[], &off, []).unwrap_or_default(),
    };
    let mut times: Vec<Duration> = Vec::new();
    let mut found = 0;
    let rounds: usize = std::env::var("RX_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(11)
        .max(11);
    for _ in 0..rounds {
        let t = Instant::now();
        found = scan(&body, &set).len();
        times.push(t.elapsed());
    }
    times.sort_unstable();
    eprintln!(
        "body {} KiB, {} hits, median {:?} (min {:?}, max {:?})",
        body.len() / 1024,
        found,
        times[rounds / 2],
        times[0],
        times[rounds - 1]
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
