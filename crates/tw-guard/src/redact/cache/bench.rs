//! 带缓存和不带缓存各要多久（`cargo test --release -p tw-guard --lib redact::cache::bench -- --ignored --nocapture`）。
//!
//! 每种请求体量三样，各取几遍的中位数：不带缓存、头一回（一份空的缓存）、存好之后（后台存完再量）。
//! 请求体有四种：几万条很短的消息（短于存的下限）、几万条七十来字节的、三千条三百字节的、两千条
//! 两千字节的；外加一段 200 轮的对话，量第 10、100、200 轮（存好的是前一轮的，这一轮新加的两句
//! 要扫）。量的是「看一遍」（[`flow::look_hits_with`]：找、报、编号）。
//!
//! 再加一段新的对话、开头的系统提示和存过的另一段一样（开头对得上，要切开看了才知道没存过）。
//!
//! 头一回不能比不带缓存慢两成以上，存好之后不能比不带缓存慢（留 5% 的量的误差）。**不在 CI 上跑**（机器快慢不一、
//! 测试并行跑），改了缓存在本机跑一遍

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tw_dialect::ir::Dialect;

use super::ScanCache;
use crate::policy::Mode;
use crate::redact::flow::{self, Shape};
use crate::redact::replace::{Ledger, Scheme};
use crate::redact::rules::RuleSet;

const SHAPE: Shape = Shape::Generate(Dialect::Anthropic);

const CODE: &str = r#"```rust
fn parse(input: &str) -> Result<Config, Error> {
    let mut out = Config::default();
    for (i, line) in input.lines().enumerate() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k.trim() {
            "port" => out.port = v.trim().parse().map_err(|_| Error::Line(i))?,
            "host" => out.host = v.trim().to_string(),
            _ => {}
        }
    }
    Ok(out)
}
```"#;

const PROSE: &str = "The function reads the file line by line and splits each line at the first \
equals sign. Lines without one are skipped, unknown keys are ignored, and a port that does not \
parse is reported with its line number so the caller can point at it. ";

/// 第 `i` 轮：用户问一句（有时贴一段代码），模型答一段（带代码，有时调一次工具）
fn turn(i: usize, key_at: Option<usize>) -> [Value; 2] {
    let mut ask = format!(
        "Turn {i}: why does the parser skip line {}? {PROSE}",
        i % 17
    );
    if i % 3 == 0 {
        ask.push_str(CODE);
    }
    if key_at == Some(i) {
        ask.push_str(" my key is sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789");
    }
    let user = json!({"role": "user", "content": ask});
    let mut blocks = vec![json!({"type": "text",
        "text": format!("{PROSE}{PROSE}\n{CODE}\n{PROSE}")})];
    if i % 4 == 0 {
        blocks.push(
            json!({"type": "tool_use", "id": format!("toolu_{i}"), "name": "bash",
            "input": {"command": format!("cargo test -p parser -- line_{i}")}}),
        );
    }
    let assistant = json!({"role": "assistant", "content": blocks});
    [user, assistant]
}

fn body(history: &[Value]) -> String {
    let system = PROSE.repeat(40);
    let tools: Vec<Value> = (0..8)
        .map(|t| {
            json!({"name": format!("tool_{t}"), "description": PROSE.repeat(6),
                   "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}}})
        })
        .collect();
    json!({"model": "claude", "max_tokens": 4096, "system": system,
           "messages": history, "tools": tools, "stream": true})
    .to_string()
}

fn look(rules: &RuleSet, text: &str, cache: Option<&ScanCache>) -> flow::Look {
    flow::look_hits_with(
        Mode::Enforce,
        rules,
        text.as_bytes(),
        SHAPE,
        Ledger::new(Scheme::SECRET),
        cache,
    )
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

const ROUNDS: usize = 21;

/// 不带缓存、头一回、存好之后。`warm_with`：存好的是哪一份（多轮对话是前一轮的）。
///
/// 三样在每一遍里各量一次、轮换先后：单独一次的耗时比连着跑几遍的长（处理器的缓存是凉的），
/// 不带缓存的连着跑、带缓存的单独跑的话，比出来的不是一回事
fn measure(rules: &RuleSet, text: &str, warm_with: &str) -> (Duration, Duration, Duration) {
    let (mut plain, mut cold, mut warm) = (Vec::new(), Vec::new(), Vec::new());
    for round in 0..ROUNDS {
        // 缓存跟着运行时建，请求来的时候后台线程早就起好、闲着
        let fresh = ScanCache::new();
        let filled = ScanCache::new();
        for _ in 0..2 {
            look(rules, warm_with, Some(&filled));
            filled.settle();
        }
        std::thread::sleep(Duration::from_millis(2));
        for k in 0..3 {
            match (round + k) % 3 {
                0 => {
                    let t = Instant::now();
                    std::hint::black_box(look(rules, text, None));
                    plain.push(t.elapsed());
                }
                1 => {
                    let t = Instant::now();
                    std::hint::black_box(look(rules, text, Some(&fresh)));
                    cold.push(t.elapsed());
                    fresh.settle();
                }
                _ => {
                    let t = Instant::now();
                    let l = std::hint::black_box(look(rules, text, Some(&filled)));
                    warm.push(t.elapsed());
                    let p = look(rules, text, None);
                    assert_eq!((l.found, l.hits), (p.found, p.hits));
                    filled.settle();
                }
            }
        }
    }
    (median(plain), median(cold), median(warm))
}

fn row(name: &str, text: &str, (plain, cold, warm): (Duration, Duration, Duration)) -> (f64, f64) {
    let (c, w) = (
        cold.as_secs_f64() / plain.as_secs_f64(),
        warm.as_secs_f64() / plain.as_secs_f64(),
    );
    println!(
        "{name:<22} | {:>7.0} | {:>9.2?} | {:>9.2?} {c:>5.2}x | {:>9.2?} {w:>5.2}x",
        text.len() as f64 / 1024.0,
        plain,
        cold,
        warm
    );
    (c, w)
}

fn messages(n: usize, content: impl Fn(usize) -> String) -> String {
    let msgs: Vec<Value> = (0..n)
        .map(|i| json!({"role": if i % 2 == 0 {"user"} else {"assistant"}, "content": content(i)}))
        .collect();
    json!({"model": "m", "messages": msgs}).to_string()
}

#[test]
#[ignore]
fn cold_and_warm_against_no_cache() {
    let rules = RuleSet::defaults();
    println!(
        "\nbody                   |      KB |  no cache |      cold (ratio) |      warm (ratio)"
    );
    let mut worst = (0f64, 0f64);
    let mut note = |r: (f64, f64)| worst = (worst.0.max(r.0), worst.1.max(r.1));
    for (name, text) in [
        ("20k x <64 B", messages(20_000, |i| format!("ok {i}"))),
        (
            "20k x 70 B",
            messages(20_000, |i| {
                format!("{i} {}", "lorem ipsum dolor ".repeat(3))
            }),
        ),
        (
            "3k x 300 B",
            messages(3_000, |i| {
                format!("{i} {}", "lorem ipsum dolor ".repeat(16))
            }),
        ),
        (
            "2k x 2000 B",
            messages(2_000, |i| {
                format!("{i} {}", "lorem ipsum dolor ".repeat(111))
            }),
        ),
    ] {
        note(row(name, &text, measure(&rules, &text, &text)));
    }
    for key_at in [None, Some(3)] {
        let mut history = Vec::new();
        let mut bodies = Vec::new();
        for i in 1..=200 {
            history.extend(turn(i, key_at));
            bodies.push(body(&history));
        }
        for n in [10, 100, 200] {
            let name = format!(
                "chat turn {n}{}",
                if key_at.is_some() { " +key" } else { "" }
            );
            note(row(
                &name,
                &bodies[n - 1],
                measure(&rules, &bodies[n - 1], &bodies[n - 2]),
            ));
        }
    }
    // 一段新的对话，开头（几 KB 的系统提示）和存过的另一段一模一样：开头对得上，要切开再看
    let chat = |seed: usize| {
        let mut all = vec![json!({"role": "system", "content": PROSE.repeat(40)})];
        for i in 1..=100 {
            all.extend(turn(i + seed, None));
        }
        json!({"messages": all, "model": "m"}).to_string()
    };
    // 存好的是另一段对话：这一列量的也是头一回，按头一回的标准算
    let (other, new) = (chat(1_000), chat(0));
    let (c, w) = row("new chat, same system", &new, measure(&rules, &new, &other));
    note((c.max(w), 0.0));
    assert!(worst.0 <= 1.2, "头一回比不带缓存慢了 {:.2} 倍", worst.0);
    // 不走缓存的（太短的请求体）两边做的是同一件事，差的是量的误差
    assert!(worst.1 <= 1.05, "存好之后比不带缓存慢了 {:.2} 倍", worst.1);
}
