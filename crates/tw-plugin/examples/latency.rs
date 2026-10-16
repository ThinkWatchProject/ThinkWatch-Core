//! 沙箱的开销：建实例 + 调一次钩子要多久。
//!
//! ```sh
//! cargo run --release -p tw-plugin --example latency
//! ```
//!
//! 默认上限（`Limits::default()`）下量。打印每一项的中位数和 p90/p99。

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tw_plugin::{Limits, RequestOutcome, Runtime};

fn main() {
    println!("guest wasm sha256 {}", tw_plugin::GUEST_WASM_SHA256);
    println!("built with {}", tw_plugin::GUEST_CLANG);

    let t = Instant::now();
    let rt = Runtime::new(Limits::default()).expect("runtime");
    println!(
        "Runtime::new                          {:>10.1?}",
        t.elapsed()
    );

    let small = r#"export const manifest = { name: "date", api: 1, permissions: ["system", "reply.text"] };
        export function onRequest(req, ctx) { req.system = (req.system || "") + "\nToday: " + new Date().toISOString().slice(0, 10); return req; }
        export function onReplyText(t) { return t.replaceAll("widget", "gadget"); }"#;
    let t = Instant::now();
    let p = rt.load(small.as_bytes()).expect("load");
    println!(
        "Runtime::load (small plugin)          {:>10.1?}",
        t.elapsed()
    );

    let ctx = json!({ "client": "claude-code", "model": "m", "format": "anthropic", "upstream": "anthropic", "settings": {} });
    let view = json!({ "format": "anthropic", "model": "m", "system": "be brief",
                       "messages": [ { "key": "m0", "role": "user", "parts": [ { "key": "p0", "type": "text", "text": "hello" } ] } ] });

    report("on_request, small view (fresh instance)", 2000, || {
        let inv = p.on_request(view.clone(), ctx.clone());
        assert!(matches!(inv.result, Ok(RequestOutcome::Changed(_))));
        inv.cpu
    });
    report("reply() (fresh instance + ctx)", 2000, || {
        let t = Instant::now();
        let r = p.reply(ctx.clone()).expect("reply");
        drop(r);
        t.elapsed()
    });
    let mut r = p.reply(ctx.clone()).expect("reply");
    report("on_text on a live reply instance", 20000, || {
        let inv = r.on_text(0, "a small widget delta");
        assert!(inv.result.is_ok());
        inv.cpu
    });

    let edit = rt
        .load(
            br#"export const manifest = { name: "words", api: 1, permissions: ["messages"] };
                export function onRequest(req) {
                  for (const m of req.messages) for (const p of m.parts) if (p.type === "text") p.text = p.text.replaceAll("widget", "gadget");
                  return req;
                }"#,
        )
        .expect("load");
    // 钩子什么都不改、也不返回：插件只读请求（比如只记日志）时付的代价
    let look = rt
        .load(
            br#"export const manifest = { name: "look", api: 1, permissions: ["messages"] };
                export function onRequest(req) {
                  let n = 0;
                  for (const m of req.messages) n += m.parts.length;
                  if (n < 0) return req;
                }"#,
        )
        .expect("load");
    for (label, size, n) in [("100 KB", 100usize << 10, 200), ("1 MB", 1 << 20, 30)] {
        let v = big_view(size);
        let bytes = serde_json::to_vec(&v).unwrap().len();
        report(
            &format!("read a {label} view, no return, sandbox CPU"),
            n,
            || {
                let inv = look.on_request(v.clone(), ctx.clone());
                assert!(matches!(inv.result, Ok(RequestOutcome::Unchanged)));
                inv.cpu
            },
        );
        report(
            &format!("edit a {label} view ({bytes} B), sandbox CPU"),
            n,
            || {
                let inv = edit.on_request(v.clone(), ctx.clone());
                assert!(matches!(inv.result, Ok(RequestOutcome::Changed(_))));
                inv.cpu
            },
        );
        report(
            &format!("edit a {label} view, wall incl. JSON in/out"),
            n,
            || {
                let t = Instant::now();
                let inv = edit.on_request(v.clone(), ctx.clone());
                assert!(matches!(inv.result, Ok(RequestOutcome::Changed(_))));
                t.elapsed()
            },
        );
    }

    // tw.replace / tw.streamReplace 在 1 MB、命中很密的文字上（每 29 个字三处命中，约 10.8
    // 万处）。CPU 上限放宽，量的是要多久；减去只造文字的那一份就是替换本身
    let roomy = Runtime::new(Limits {
        request_cpu: Duration::from_secs(30),
        ..Limits::default()
    })
    .expect("runtime");
    let dense = roomy
        .load(
            br##"export const manifest = { name: "dense", api: 1, permissions: ["system", "reply.text"], reply: "stream" };
                export const { onReplyText, onReplyTextEnd } = tw.streamReplace([]);
                const RULES = {
                  strings: [["widget", "gadget"], ["12", "#"], ["dollars", "USD"]],
                  mixed: [["widget", "gadget"], [/\d+/, "#"], ["dollars", "USD"]],
                };
                export function onRequest(req, ctx) {
                  const text = "the widget costs 12 dollars; ".repeat(36000);
                  const [op, kind] = ctx.settings.op.split(":");
                  let out = "";
                  if (op === "replace") out = tw.replace(text, RULES[kind]);
                  if (op === "stream") {
                    const h = tw.streamReplace(RULES[kind]);
                    for (let i = 0; i < text.length; i += 64) out += h.onReplyText(text.slice(i, i + 64), ctx);
                    out += h.onReplyTextEnd(ctx);
                  }
                  req.system = String(out.length);
                }"##,
        )
        .expect("load");
    let base = json!({ "format": "anthropic", "model": "m", "system": "" });
    let cost = |op: &str| {
        let c = json!({ "client": null, "model": "m", "format": "anthropic", "upstream": "u", "settings": { "op": op } });
        report(&format!("1 MB dense text, {op}, sandbox CPU"), 10, || {
            let inv = dense.on_request(base.clone(), c.clone());
            assert!(inv.result.is_ok(), "{op}: {:?}", inv.result);
            inv.cpu
        })
    };
    let none = cost("none:");
    for op in [
        "replace:strings",
        "replace:mixed",
        "stream:strings",
        "stream:mixed",
    ] {
        let t = cost(op);
        println!(
            "  {op}: {:.1?} more than building the text",
            t.saturating_sub(none)
        );
    }
}

/// 打印中位数和 p90/p99，返回中位数
fn report(label: &str, n: usize, mut f: impl FnMut() -> Duration) -> Duration {
    for _ in 0..(n / 10).max(3) {
        f();
    }
    let mut v: Vec<Duration> = (0..n).map(|_| f()).collect();
    v.sort();
    let q = |p: f64| v[((v.len() as f64 - 1.0) * p) as usize];
    println!(
        "{label:<46} n={n:<6} p50 {:>9.1?}  p90 {:>9.1?}  p99 {:>9.1?}",
        q(0.5),
        q(0.9),
        q(0.99)
    );
    q(0.5)
}

fn big_view(target: usize) -> Value {
    let words = [
        "the", "function", "returns", "a", "value", "widget", "请求", "🙂", "\"q\"", "a\nb",
    ];
    let mut messages = Vec::new();
    let mut size = 0;
    let mut i = 0usize;
    while size < target {
        let text: String = (0..200)
            .map(|j| words[(i * 7 + j * 13) % words.len()])
            .collect::<Vec<_>>()
            .join(" ");
        size += text.len() + 100;
        messages.push(json!({ "key": format!("m{i}"), "role": if i % 2 == 0 { "user" } else { "assistant" },
                              "parts": [ { "key": format!("p{i}"), "type": "text", "text": text } ] }));
        i += 1;
    }
    json!({ "format": "anthropic", "model": "m", "system": "s", "messages": messages })
}
