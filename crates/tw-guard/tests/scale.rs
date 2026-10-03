//! 一个请求里有几万个不同的值时，出站脱敏的每一步都还是线性的。
//!
//! 曾经不是：合并找到的值时在合过的里面一条条找、换的时候在原文上就地换（换一处，后面
//! 整段挪一次）、还原时拿账里的每个占位符去整段里找一遍 —— 一个 840 KB、4 万个不同
//! `AKIA…` 的请求，看一遍要 2 秒，回答还原要 7 秒，10 MiB 的请求要几分钟。一份导出的
//! 凭据清单、一段日志贴进对话，就是这种请求。
//!
//! **不卡严格的时间**（CI 的机器快慢不一、测试并行跑）：同一套事做 1 万个值和 4 万个值
//! 各几遍、各取最快的一遍，看两者之比。线性的是 4 倍上下，平方级的是 16 倍。

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tw_dialect::ir::Dialect;
use tw_guard::policy::Mode;
use tw_guard::redact::flow;
use tw_guard::redact::replace;
use tw_guard::redact::rules::RuleSet;
use tw_guard::redact::sse::SseRestorer;
use tw_guard::redact::stream::Restorer;

/// 第 `i` 个 AWS 访问密钥的样子，两两不同
fn key(i: usize) -> String {
    let mut tail = String::new();
    let mut n = i;
    for _ in 0..16 {
        tail.push(char::from(b'A' + (n % 26) as u8));
        n /= 26;
    }
    format!("AKIA{tail}")
}

/// 一条用户消息，里面是 `n` 个不同的密钥
fn request(n: usize) -> String {
    let keys: Vec<String> = (0..n).map(key).collect();
    json!({"messages": [{"role": "user", "content": keys.join(" ")}]}).to_string()
}

/// 看一遍、换一遍、整段还原、切成小块流式还原、按 SSE 帧还原，每一步都核对结果
fn round_trip(n: usize) -> Duration {
    let rules = RuleSet::defaults();
    let body = request(n);
    let started = Instant::now();

    let (found, ledger) = flow::look(Mode::Enforce, &rules, body.as_bytes());
    assert_eq!(found.len(), n, "一个不同的值一条");
    assert_eq!(ledger.len(), n);
    let (sent, ledger) = flow::replace(Mode::Enforce, &rules, body.clone().into(), &ledger);
    let sent = String::from_utf8(sent.to_vec()).unwrap();
    assert!(!sent.contains("AKIA"), "每一个都换掉了");
    assert!(sent.contains(&format!("<<TW_SECRET_{n}>>")));

    // 回答把占位符原样念了一遍
    assert_eq!(replace::restore(&sent, &ledger), body);
    let mut streamed = Restorer::new(&ledger);
    let mut out = String::new();
    for piece in sent.as_bytes().chunks(64) {
        out.push_str(&streamed.process(std::str::from_utf8(piece).unwrap()));
    }
    out.push_str(&streamed.flush());
    assert_eq!(out, body);

    // 同一段话按 Anthropic 的 SSE 一帧几十个字流回来
    let text = serde_json::from_str::<Value>(&sent).unwrap()["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_string();
    let mut sse = SseRestorer::new(&ledger, Dialect::Anthropic);
    let mut raw = Vec::new();
    for piece in text.as_bytes().chunks(48) {
        let delta = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": std::str::from_utf8(piece).unwrap()}
        });
        raw.extend(
            sse.process(format!("event: content_block_delta\ndata: {delta}\n\n").as_bytes()),
        );
    }
    raw.extend(sse.flush());
    let restored: String = String::from_utf8(raw)
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter_map(|v| v["delta"]["text"].as_str().map(str::to_string))
        .collect();
    assert_eq!(restored, (0..n).map(key).collect::<Vec<_>>().join(" "));

    started.elapsed()
}

#[test]
fn tens_of_thousands_of_distinct_values_cost_linear_time() {
    let fastest = |n| (0..3).map(|_| round_trip(n)).min().unwrap();
    let small = fastest(10_000);
    let large = fastest(40_000);
    let ratio = large.as_secs_f64() / small.as_secs_f64();
    // 线性的是 4 倍上下；平方级的是 16 倍。留足余量，只挡住平方级的
    assert!(
        ratio < 10.0,
        "4 万个值花了 1 万个值的 {ratio:.1} 倍（{small:?} → {large:?}）：有一步不再是线性的"
    );
}
