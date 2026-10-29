//! 长会话里每一轮看一遍要多久：不带缓存（每轮整段历史从头扫）和带缓存（没变的历史取上一轮的）。
//!
//! ```text
//! cargo test --release -p tw-guard --test scan_cache -- --ignored --nocapture
//! ```
//!
//! 一段 200 轮的对话，每轮加一句用户、一句模型（带代码、偶尔带一条工具调用），系统提示和工具
//! 定义每轮都在。量第 10、100、200 轮的「看一遍」（[`flow::look_hits_with`]：找、报、编号）：
//! 带缓存的那一份先拿前一轮的请求把缓存填好，量的是这一轮新加的两句扫一遍、其余取缓存。
//! 两种历史各量一遍：一处命中都没有的，和第 3 轮贴过一把密钥的（之后每轮都有命中，命中之后
//! 那几步 —— 认正文、去掉载荷、合并 —— 照旧在整段请求体上做）。
//!
//! **不卡时间**（CI 的机器快慢不一），只核对两条路找到的一样，数字打出来看。

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tw_dialect::ir::Dialect;
use tw_guard::policy::Mode;
use tw_guard::redact::cache::CONTEXTUAL;
use tw_guard::redact::flow::{self, ScanCache, Shape};
use tw_guard::redact::replace::{Ledger, Scheme};
use tw_guard::redact::rules::{BUILTINS, Pattern, RuleSet};

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

/// 中位数
fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

#[test]
#[ignore]
fn two_hundred_turns() {
    // 内置的全开（看得到隔壁那一段的几条除外：开着它们不走缓存），加 20 条自定义规则
    let ids: Vec<&str> = BUILTINS
        .iter()
        .map(|b| b.id)
        .filter(|id| !CONTEXTUAL.contains(id))
        .collect();
    let mut wide = RuleSet::only(&ids);
    for i in 0..10 {
        wide = wide
            .with_rule(
                &format!("lit{i}"),
                Pattern {
                    text: format!("PROJ{i}-"),
                    regex: false,
                    case_sensitive: false,
                },
                None,
            )
            .unwrap()
            .with_custom(&format!("re{i}"), &format!(r"EMP{i}-\d{{6}}"))
            .unwrap();
    }
    let rounds = 9;
    for (set, rules) in [
        ("defaults", RuleSet::defaults()),
        ("all local + 20 custom", wide),
    ] {
        for (name, key_at) in [("no hits", None), ("key at turn 3", Some(3))] {
            let mut history = Vec::new();
            let mut bodies = Vec::new();
            for i in 1..=200 {
                history.extend(turn(i, key_at));
                bodies.push(body(&history));
            }
            println!("\n{set}, {name}: per-turn look (median of {rounds})");
            println!("turn | body KB | no cache | cached | speedup");
            for n in [10, 100, 200] {
                let (prev, text) = (&bodies[n - 2], &bodies[n - 1]);
                let plain = median(
                    (0..rounds)
                        .map(|_| {
                            let t = Instant::now();
                            std::hint::black_box(look(&rules, text, None));
                            t.elapsed()
                        })
                        .collect(),
                );
                let cached = median(
                    (0..rounds)
                        .map(|_| {
                            // 前一轮的请求把缓存填好，这一轮只有新加的两句没扫过
                            let cache = ScanCache::new();
                            look(&rules, prev, Some(&cache));
                            let t = Instant::now();
                            let l = std::hint::black_box(look(&rules, text, Some(&cache)));
                            let took = t.elapsed();
                            let p = look(&rules, text, None);
                            assert_eq!((l.found, l.hits), (p.found, p.hits));
                            took
                        })
                        .collect(),
                );
                println!(
                    "{n:>4} | {:>7.0} | {:>8.2?} | {:>6.2?} | {:>6.1}x",
                    text.len() as f64 / 1024.0,
                    plain,
                    cached,
                    plain.as_secs_f64() / cached.as_secs_f64()
                );
            }
        }
    }
}
