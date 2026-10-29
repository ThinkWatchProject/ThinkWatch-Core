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

/// 请求体都写成 Anthropic 的样子
const SHAPE: flow::Shape = flow::Shape::Generate(Dialect::Anthropic);

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

    let (found, ledger) = flow::look(Mode::Enforce, &rules, body.as_bytes(), SHAPE);
    assert_eq!(found.len(), n, "一个不同的值一条");
    assert_eq!(ledger.len(), n);
    let (sent, ledger) = flow::replace(Mode::Enforce, &rules, body.clone().into(), &ledger, SHAPE);
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

/// 账本里记着这几个原值
fn ledger_of(values: &[String]) -> replace::Ledger {
    let text = values.join("\u{1}");
    let mut hits = Vec::new();
    let mut at = 0;
    for v in values {
        hits.push(tw_guard::redact::rules::Hit {
            bytes: at..at + v.len(),
            rule: tw_guard::redact::rules::Rule::Custom("c".into()),
            label: None,
        });
        at += v.len() + 1;
    }
    replace::apply_text(&text, &hits, replace::Ledger::new(replace::Scheme::SECRET)).ledger
}

/// 落盘前把账本里的原值换回占位符：`n` 个值、文字跟着长，里面每个值念一遍，还夹着一样多的
/// 占位符
fn hide(n: usize) -> Duration {
    let values: Vec<String> = (0..n)
        .map(|i| format!("user{i:06}@corp.example.com"))
        .collect();
    let ledger = ledger_of(&values);
    let content: String = values
        .iter()
        .enumerate()
        .map(|(i, v)| format!("<<TW_SECRET_{}>> lorem ipsum {v} dolor ", i + 1))
        .collect();
    let body =
        json!({"messages": [{"role": "user", "content": content}], "max_tokens": 4096}).to_string();
    let started = Instant::now();
    let out = replace::hide_values(&body, &ledger, replace::Form::Json);
    let took = started.elapsed();
    assert!(!out.contains("@corp.example.com"));
    assert!(serde_json::from_str::<Value>(&out).is_ok());
    took
}

#[test]
fn hiding_tens_of_thousands_of_values_costs_linear_time() {
    let (small, large) = medians(|| hide(5_000), || hide(40_000));
    let ratio = large.as_secs_f64() / small.as_secs_f64();
    // 线性的是 8 倍上下，平方级的是 64 倍
    assert!(
        ratio < 24.0,
        "4 万个值花了 5 千个值的 {ratio:.1} 倍（{small:?} → {large:?}）：落盘前打码不再是线性的"
    );
}

/// 两种大小**交替**各跑五遍、各取中位数：并行跑的测试抢 CPU 时，一阵忙落在哪一遍上都不至于
/// 左右结论
fn medians(small: impl Fn() -> Duration, large: impl Fn() -> Duration) -> (Duration, Duration) {
    let (mut s, mut l) = (Vec::new(), Vec::new());
    for _ in 0..5 {
        s.push(small());
        l.push(large());
    }
    s.sort_unstable();
    l.sort_unstable();
    (s[2], l[2])
}

/// 原值一个套一个：`\d{6,}` 认出来的一串串 1，长度 6 到 `6 + k`。正文长度是 k² 级的；每个
/// 位置只取最长的那一个，打码的花费跟着正文走（四倍的 k 是十六倍的正文），不是正文 × 值的
/// 个数（六十四倍），也不占上 G 的内存
fn nested_runs(k: usize) -> Duration {
    let rules = RuleSet::none().with_custom("ids", r"\d{6,}").unwrap();
    let content: String = (6..6 + k).map(|l| format!("{} ", "1".repeat(l))).collect();
    let body = json!({"messages": [{"role": "user", "content": content}]}).to_string();
    let (_, ledger) = flow::look(Mode::Enforce, &rules, body.as_bytes(), SHAPE);
    assert_eq!(ledger.len(), k);
    let started = Instant::now();
    let out = replace::hide_values(&body, &ledger, replace::Form::Json);
    let took = started.elapsed();
    assert!(!out.contains("111111"));
    took
}

#[test]
fn nested_values_are_hidden_in_time_linear_in_the_text() {
    let (small, large) = medians(|| nested_runs(100), || nested_runs(400));
    let ratio = large.as_secs_f64() / small.as_secs_f64();
    assert!(
        ratio < 36.0,
        "十六倍的正文花了 {ratio:.1} 倍（{small:?} → {large:?}）：一个套一个的原值在一处处地列"
    );
}

/// 按位置认的规则（请求头、命令行、URL 参数、环境变量口令、配置赋值、Webhook 地址）从标记往后
/// 找值。同一个标记连着写上几万遍、中间不断开时，每个标记都把后面整段看一遍就是平方级的 ——
/// 曾经 100 KB 的 `api-key=` 要 2 秒、400 KB 要 39 秒。每一种各占一条消息
fn repeated_markers(len: usize) -> Duration {
    const UNITS: &[&str] = &[
        "api-key=",
        "x-api-key: ",
        "TOKEN=",
        "f(token=t1,",
        "SECRET=\"",
        "\"apiKey\": \"",
        "?sig=",
        "&access_token=",
        "PGPASSWORD=",
        "curl ",
        "curl -u ",
        "mysql ",
        "mysql -p",
        "hooks.slack.com/",
        "x.webhook.office.com/",
    ];
    let messages: Vec<Value> = UNITS
        .iter()
        .map(|u| json!({"role": "user", "content": u.repeat(len / u.len())}))
        .collect();
    let body = json!({ "messages": messages }).to_string();
    let ids: Vec<&str> = tw_guard::redact::rules::BUILTINS
        .iter()
        .map(|b| b.id)
        .collect();
    let rules = RuleSet::only(&ids);
    let started = Instant::now();
    let _ = tw_guard::redact::rules::scan(&body, &rules);
    started.elapsed()
}

#[test]
fn repeated_position_markers_cost_linear_time() {
    let (small, large) = medians(|| repeated_markers(20_000), || repeated_markers(80_000));
    let ratio = large.as_secs_f64() / small.as_secs_f64();
    // 线性的是 4 倍上下，平方级的是 16 倍
    assert!(
        ratio < 10.0,
        "四倍长的标记串花了 {ratio:.1} 倍（{small:?} → {large:?}）：按位置认的规则不再是线性的"
    );
}
