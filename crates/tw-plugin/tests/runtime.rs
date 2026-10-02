//! 沙箱的行为：钩子的输入输出、四个上限、状态隔离、清单核对。
//!
//! 插件源码都写在测试里，一眼能看出每条测的是什么。

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tw_plugin::*;

fn rt() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new(Limits::default()).expect("runtime"))
}

/// 测别的上限（内存、输出、栈）时用：CPU 时间放得很宽。CI 的机器比开发机慢好几倍，
/// 测试又是 debug 构建，默认的 200 ms 可能先到，测到的就成了 CPU 上限
/// 超出预算之后最多还能跑多久才停下。unix 上量的是线程的 CPU 时间，只差一格
/// 节拍；Windows 上量的是墙上时间，并发跑测试时线程会被抢占，留宽一些
fn slack() -> Duration {
    if cfg!(windows) {
        Duration::from_secs(1)
    } else {
        Duration::from_millis(100)
    }
}

fn roomy() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        Runtime::new(Limits {
            request_cpu: Duration::from_secs(30),
            reply_call_cpu: Duration::from_secs(30),
            reply_total_cpu: Duration::from_secs(60),
            ..Limits::default()
        })
        .expect("runtime")
    })
}

fn load(src: &str) -> Plugin {
    load_in(rt(), src)
}

fn load_in(rt: &Runtime, src: &str) -> Plugin {
    match rt.load(src.as_bytes()) {
        Ok(p) => p,
        Err(e) => panic!("load failed: {e:?}\n{src}"),
    }
}

fn load_err(src: &str) -> LoadError {
    match rt().load(src.as_bytes()) {
        Ok(p) => panic!("expected a load error, got {:?}", p.manifest()),
        Err(e) => e,
    }
}

fn manifest_err(src: &str) -> String {
    match load_err(src) {
        LoadError::Manifest(m) => m,
        other => panic!("expected a manifest error, got {other:?}"),
    }
}

fn ctx() -> Value {
    json!({
        "client": "claude-code",
        "model": "claude-x",
        "format": "anthropic",
        "upstream": null,
        "settings": { "note": "hello" }
    })
}

fn view() -> Value {
    json!({
        "format": "anthropic",
        "model": "claude-x",
        "system": "be brief",
        "messages": [
            { "key": "m0", "role": "user", "parts": [ { "key": "p0", "type": "text", "text": "hi" } ] }
        ],
        "params": { "model": "claude-x", "max_tokens": 1024, "temperature": 1.0 }
    })
}

const SYSTEM: &str = r#"export const manifest = { name: "t", api: 1, permissions: ["system"] };
"#;

fn request(body: &str) -> Plugin {
    load(&format!(
        "{SYSTEM}export function onRequest(req, ctx) {{ {body} }}"
    ))
}

fn run(body: &str) -> Invocation<RequestOutcome> {
    request(body).on_request(view(), ctx())
}

fn run_roomy(body: &str) -> Invocation<RequestOutcome> {
    load_in(
        roomy(),
        &format!("{SYSTEM}export function onRequest(req, ctx) {{ {body} }}"),
    )
    .on_request(view(), ctx())
}

fn changed(inv: &Invocation<RequestOutcome>) -> &Value {
    match &inv.result {
        Ok(RequestOutcome::Changed(v)) => v,
        other => panic!("expected Changed, got {other:?} (logs {:?})", inv.logs),
    }
}

fn threw(inv: &Invocation<RequestOutcome>) -> String {
    match &inv.result {
        Err(RunError::Threw { message, .. }) => message.clone(),
        other => panic!("expected Threw, got {other:?}"),
    }
}

// ── 基本 ─────────────────────────────────────────────────────────

#[test]
fn a_request_hook_edits_the_view() {
    let inv = run(r#"req.system = req.system + " / " + ctx.settings.note; return req;"#);
    assert_eq!(changed(&inv)["system"], "be brief / hello");
    assert!(inv.cpu > Duration::ZERO);
}

#[test]
fn undefined_and_an_identical_view_are_unchanged() {
    assert_eq!(run("").result, Ok(RequestOutcome::Unchanged));
    assert_eq!(
        run("return undefined;").result,
        Ok(RequestOutcome::Unchanged)
    );
    // 原样返回：1.0 进出 JS 变成 1，也不算改过
    assert_eq!(run("return req;").result, Ok(RequestOutcome::Unchanged));
}

#[test]
fn big_integers_that_lose_precision_in_js_do_not_count_as_changes() {
    let p = request("return req;");
    let mut v = view();
    v["params"]["seed"] = json!(12345678901234567891u64);
    assert_eq!(p.on_request(v, ctx()).result, Ok(RequestOutcome::Unchanged));
}

#[test]
fn reject_refuses_the_request_even_when_caught() {
    assert_eq!(
        run(r#"reject("no secrets here");"#).result,
        Ok(RequestOutcome::Rejected("no secrets here".into()))
    );
    assert_eq!(
        run(r#"try { reject("caught"); } catch (e) {} return req;"#).result,
        Ok(RequestOutcome::Rejected("caught".into()))
    );
    // 第一次给的理由算数
    assert_eq!(
        run(r#"try { reject("first"); } catch (e) {} reject("second");"#).result,
        Ok(RequestOutcome::Rejected("first".into()))
    );
}

#[test]
fn async_hooks_work_and_can_reject_after_await() {
    let p = load(&format!(
        "{SYSTEM}export async function onRequest(req) {{ await null; req.system = 'async'; return req; }}"
    ));
    assert_eq!(changed(&p.on_request(view(), ctx()))["system"], "async");
    let p = load(&format!(
        "{SYSTEM}export async function onRequest(req) {{ await Promise.resolve(); reject('later'); }}"
    ));
    assert_eq!(
        p.on_request(view(), ctx()).result,
        Ok(RequestOutcome::Rejected("later".into()))
    );
    let p = load(&format!(
        "{SYSTEM}export function onRequest(req) {{ return new Promise(() => {{}}); }}"
    ));
    assert!(
        matches!(p.on_request(view(), ctx()).result, Err(RunError::BadOutput(m)) if m.contains("never settled"))
    );
}

#[test]
fn return_types_are_checked() {
    for (body, want) in [
        ("return 42;", "not a number"),
        ("return 'text';", "not a string"),
        ("return null;", "not null"),
        ("return [];", "not an array"),
        (
            "const o = {}; o.self = o; return o;",
            "cannot be turned into JSON",
        ),
        ("return { n: 1n };", "cannot be turned into JSON"),
    ] {
        match run(body).result {
            Err(RunError::BadOutput(m)) => assert!(m.contains(want), "{body}: {m}"),
            other => panic!("{body}: expected BadOutput, got {other:?}"),
        }
    }
}

#[test]
fn thrown_errors_and_non_errors_are_reported() {
    let inv = run("throw new TypeError('bad thing');");
    match &inv.result {
        Err(RunError::Threw { message, stack }) => {
            assert_eq!(message, "TypeError: bad thing");
            let stack = stack.as_deref().unwrap_or_default();
            assert!(stack.contains("onRequest (plugin.js:2:"), "{stack}");
            assert!(!stack.contains("bridge.js"), "{stack}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        threw(&run("throw 'just a string';")),
        "Uncaught just a string"
    );
    assert_eq!(threw(&run("throw { code: 7 };")), r#"Uncaught {"code":7}"#);
    // 一个会抛的 getter 也不让描述错误的那段代码跟着出事
    assert!(threw(&run(
        "throw new Proxy({}, { get() { throw new Error('trap'); }, getPrototypeOf() { throw new Error('trap'); } });"
    ))
    .starts_with("Uncaught"));
}

#[test]
fn ctx_is_frozen_and_read_only() {
    let m = threw(&run("ctx.model = 'other';"));
    assert!(m.starts_with("TypeError"), "{m}");
    let m = threw(&run("ctx.settings.note = 'x';"));
    assert!(m.starts_with("TypeError"), "{m}");
    assert_eq!(
        run("if (!Object.isFrozen(ctx.settings)) throw 1;").result,
        Ok(RequestOutcome::Unchanged)
    );
}

#[test]
fn console_lines_are_captured_with_levels() {
    let inv = run(
        r#"console.log("a", 1, {b: 2}, [3]); console.info("i"); console.warn("w"); console.error(new Error("e")); console.debug("d");"#,
    );
    let lines: Vec<(LogLevel, &str)> = inv
        .logs
        .iter()
        .map(|l| (l.level, l.text.as_str()))
        .collect();
    assert_eq!(
        lines,
        vec![
            (LogLevel::Log, r#"a 1 {"b":2} [3]"#),
            (LogLevel::Info, "i"),
            (LogLevel::Warn, "w"),
            (LogLevel::Error, "Error: e"),
            (LogLevel::Log, "d"),
        ]
    );
}

// ── 没有环境里的 I/O ───────────────────────────────────────────

#[test]
fn the_sandbox_imports_only_the_log_and_the_clock() {
    let mut imports = rt().sandbox_imports();
    imports.sort();
    assert_eq!(imports, vec!["env.__rquickjs_host_now_us", "tw.log"]);
}

#[test]
fn only_standard_globals_exist() {
    let inv = run(
        r#"for (const n of ["fetch", "require", "std", "os", "process", "queueMicrotask", "setTimeout",
              "setInterval", "performance", "navigator", "XMLHttpRequest", "WebAssembly", "Deno", "Bun",
              "print", "gc", "scriptArgs", "atob", "btoa", "__tw_log", "module", "exports"]) {
             if (typeof globalThis[n] !== "undefined") throw new Error(n + " is defined");
           }
           if (typeof console.log !== "function" || typeof reject !== "function") throw new Error("missing");
           return undefined;"#,
    );
    assert_eq!(
        inv.result,
        Ok(RequestOutcome::Unchanged),
        "{:?}",
        inv.result
    );
}

#[test]
fn static_and_dynamic_imports_do_not_load_anything() {
    let e = load_err(&format!(
        "import fs from 'fs';\n{SYSTEM}export function onRequest() {{}}"
    ));
    assert!(
        matches!(&e, LoadError::Syntax { message, .. } if message.contains("fs")),
        "{e:?}"
    );
    let inv = run("return import('os').then(() => ({ system: 'loaded' }));");
    assert!(
        matches!(inv.result, Err(RunError::Threw { .. })),
        "{:?}",
        inv.result
    );
}

#[test]
fn date_is_the_real_time_and_random_is_reseeded() {
    let p = request("return { ...req, system: String(Date.now()) + ' ' + Math.random() };");
    let a = changed(&p.on_request(view(), ctx()))["system"]
        .as_str()
        .unwrap()
        .to_string();
    let b = changed(&p.on_request(view(), ctx()))["system"]
        .as_str()
        .unwrap()
        .to_string();
    let (ms, ra) = a.split_once(' ').unwrap();
    let (_, rb) = b.split_once(' ').unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64;
    let ms: f64 = ms.parse().unwrap();
    assert!(
        (now - ms).abs() < 60_000.0,
        "Date.now() = {ms}, now = {now}"
    );
    assert_ne!(ra, rb, "Math.random repeats between instances");
}

// ── 状态 ─────────────────────────────────────────────────────────

#[test]
fn state_does_not_survive_between_requests() {
    let p = load(&format!(
        "{SYSTEM}let n = 0; globalThis.g = (globalThis.g || 0);
         export function onRequest(req) {{ n++; globalThis.g++; Object.prototype.polluted = true; req.system = n + ',' + globalThis.g; return req; }}"
    ));
    for _ in 0..3 {
        assert_eq!(changed(&p.on_request(view(), ctx()))["system"], "1,1");
    }
    // 另一个插件看不到上一个留下的原型污染
    assert_eq!(
        run("if (({}).polluted !== undefined) throw new Error('leak');").result,
        Ok(RequestOutcome::Unchanged)
    );
}

#[test]
fn state_persists_within_one_reply() {
    let p = load(
        r##"export const manifest = { name: "count", api: 1, permissions: ["reply.text"] };
           let n = 0;
           export function onReplyText(text) { n++; return text + "#" + n; }"##,
    );
    let mut r = p.reply(ctx()).unwrap();
    assert_eq!(r.on_text("a").result, Ok(Some("a#1".into())));
    assert_eq!(r.on_text("b").result, Ok(Some("b#2".into())));
    let mut r2 = p.reply(ctx()).unwrap();
    assert_eq!(r2.on_text("c").result, Ok(Some("c#1".into())));
    assert_eq!(r.on_text("d").result, Ok(Some("d#3".into())));
}

// ── 回答钩子 ─────────────────────────────────────────────────────

#[test]
fn stream_mode_holds_text_and_flushes_it_at_the_end() {
    let p = load(
        r#"export const manifest = { name: "hold", api: 1, permissions: ["reply.text"], reply: "stream" };
           let held = "";
           export function onReplyText(t) { held += t; if (held.length < 6) return ""; const out = held; held = ""; return out.toUpperCase(); }
           export function onReplyTextEnd() { const out = held; held = ""; return out; }"#,
    );
    assert_eq!(p.manifest().reply_mode, ReplyMode::Stream);
    assert!(p.manifest().hooks.reply_text_end);
    let mut r = p.reply(ctx()).unwrap();
    assert_eq!(r.on_text("abc").result, Ok(Some(String::new())));
    assert_eq!(r.on_text("def").result, Ok(Some("ABCDEF".into())));
    assert_eq!(r.on_text("gh").result, Ok(Some(String::new())));
    assert_eq!(r.on_text_end().result, Ok(Some("gh".into())));
}

#[test]
fn reply_text_output_is_validated_and_made_well_formed() {
    let p = load(
        r#"export const manifest = { name: "x", api: 1, permissions: ["reply.text"] };
           export function onReplyText(t) {
             if (t === "num") return 5;
             if (t === "split") return "a😀".slice(0, 2);
             if (t === "same") return t;
             if (t === "reject") reject("no");
             return undefined;
           }"#,
    );
    let mut r = p.reply(ctx()).unwrap();
    assert!(
        matches!(r.on_text("num").result, Err(RunError::BadOutput(m)) if m.contains("must return a string"))
    );
    // 切半的代理对换成 U+FFFD，不让整段回答出错
    assert_eq!(r.on_text("split").result, Ok(Some("a\u{fffd}".into())));
    assert_eq!(r.on_text("same").result, Ok(None));
    assert_eq!(r.on_text("other").result, Ok(None));
    let m = match r.on_text("reject").result {
        Err(RunError::Threw { message, .. }) => message,
        other => panic!("{other:?}"),
    };
    assert!(m.contains("only be called inside onRequest"), "{m}");
}

#[test]
fn tool_call_hooks_replace_drop_and_keep_calls() {
    let p = load(
        r#"export const manifest = { name: "tools", api: 1, permissions: ["reply.tool_calls"] };
           export function onToolCall(call) {
             switch (call.name) {
               case "keep": return undefined;
               case "same": return call;
               case "drop": return null;
               case "none": return [];
               case "path": call.input.path = call.input.path.replace("/mnt/c/", "C:\\"); return call;
               case "split": return [{ name: "a", input: {} }, { id: null, name: "b", input: { x: 1 } }];
               case "noname": return { input: {} };
               case "extra": return { name: "x", input: {}, cmd: "rm -rf /" };
               case "scalar": return 1;
             }
           }"#,
    );
    let mut r = p.reply(ctx()).unwrap();
    let call =
        |name: &str| json!({ "id": "call_1", "name": name, "input": { "path": "/mnt/c/x" } });
    assert_eq!(
        r.on_tool_call(call("keep")).result,
        Ok(ToolCallOutcome::Unchanged)
    );
    assert_eq!(
        r.on_tool_call(call("same")).result,
        Ok(ToolCallOutcome::Unchanged)
    );
    assert_eq!(
        r.on_tool_call(call("drop")).result,
        Ok(ToolCallOutcome::Drop)
    );
    assert_eq!(
        r.on_tool_call(call("none")).result,
        Ok(ToolCallOutcome::Drop)
    );
    assert_eq!(
        r.on_tool_call(call("path")).result,
        Ok(ToolCallOutcome::Replace(vec![
            json!({ "id": "call_1", "name": "path", "input": { "path": "C:\\x" } })
        ]))
    );
    assert_eq!(
        r.on_tool_call(call("split")).result,
        Ok(ToolCallOutcome::Replace(vec![
            json!({ "name": "a", "input": {} }),
            json!({ "name": "b", "input": { "x": 1 } }),
        ]))
    );
    for bad in ["noname", "extra", "scalar"] {
        assert!(
            matches!(
                r.on_tool_call(call(bad)).result,
                Err(RunError::BadOutput(_))
            ),
            "{bad}"
        );
    }
}

#[test]
fn hooks_a_plugin_does_not_export_are_no_ops() {
    let p = request("return req;");
    let mut r = p.reply(ctx()).unwrap();
    assert_eq!(r.on_text("x").result, Ok(None));
    assert_eq!(r.on_text_end().result, Ok(None));
    assert_eq!(
        r.on_tool_call(json!({"name": "x", "input": {}})).result,
        Ok(ToolCallOutcome::Unchanged)
    );
}

// ── 上限 ─────────────────────────────────────────────────────────

#[test]
fn an_infinite_loop_is_stopped_by_the_cpu_limit() {
    let t = Instant::now();
    let inv = run("for (;;) {}");
    let wall = t.elapsed();
    assert_eq!(inv.result, Err(RunError::CpuLimit));
    let limit = rt().limits().request_cpu;
    assert!(inv.cpu >= limit, "stopped after {:?}", inv.cpu);
    assert!(inv.cpu < limit + slack(), "overran: {:?}", inv.cpu);
    assert!(wall < Duration::from_secs(5), "wall {wall:?}");
}

#[test]
fn loop_free_cpu_burners_are_stopped_too() {
    // 不经过 JS 循环的：指数级递归、正则灾难回溯、对大字符串反复调内建函数
    for body in [
        "function f(n) { return n < 2 ? n : f(n - 1) + f(n - 2); } f(60);",
        "/(a+)+$/.test('a'.repeat(40) + 'b');",
        "const s = 'x'.repeat(1 << 20); for (let i = 0; i < 1e9; i++) s.toUpperCase();",
    ] {
        assert_eq!(run(body).result, Err(RunError::CpuLimit), "{body}");
    }
}

#[test]
fn a_memory_bomb_is_stopped_by_the_memory_limit() {
    let inv = run_roomy("const a = []; for (;;) a.push(new Uint8Array(16 << 20));");
    assert_eq!(inv.result, Err(RunError::MemoryLimit));
    // 接住内存耗尽的异常也没用：碰过上限就算超了
    let inv = run_roomy(
        "try { const a = []; for (;;) a.push(new Array(1 << 20).fill(1)); } catch (e) {} return req;",
    );
    assert_eq!(inv.result, Err(RunError::MemoryLimit));
    // 一次要一大块
    let inv = run_roomy("new ArrayBuffer(512 * 1024 * 1024);");
    assert_eq!(inv.result, Err(RunError::MemoryLimit));
}

#[test]
fn deep_recursion_is_an_error_not_a_crash() {
    let m = threw(&run_roomy("function f(n) { return f(n + 1) + 1; } f(0);"));
    assert!(m.contains("stack"), "{m}");
    // 嵌得很深的数据交给 C 写的内建函数：要么是 RangeError，要么是 wasm 栈耗尽的陷阱
    for body in [
        "let o = {}; for (let i = 0; i < 2e5; i++) o = { o }; JSON.stringify(o);",
        "JSON.parse('['.repeat(1e6));",
    ] {
        match run_roomy(body).result {
            Err(RunError::Threw { .. }) | Err(RunError::Trap(_)) => {}
            other => panic!("{body}: {other:?}"),
        }
    }
    // 之后照常能用
    assert_eq!(
        run("return undefined;").result,
        Ok(RequestOutcome::Unchanged)
    );
}

#[test]
fn a_giant_output_is_stopped_by_the_output_cap() {
    // 视图几百字节，上限约 1 MiB；返回 2 MiB
    let inv = run_roomy("req.system = 'x'.repeat(2 << 20); return req;");
    assert_eq!(inv.result, Err(RunError::OutputLimit));
    // 攒着到最后才放出来的文字也一样
    let p = load_in(
        roomy(),
        r#"export const manifest = { name: "big", api: 1, permissions: ["reply.text"], reply: "stream" };
           export function onReplyText() { return ""; }
           export function onReplyTextEnd() { return "y".repeat(2 << 20); }"#,
    );
    let mut r = p.reply(ctx()).unwrap();
    assert_eq!(r.on_text("a").result, Ok(Some(String::new())));
    assert_eq!(r.on_text_end().result, Err(RunError::OutputLimit));
}

#[test]
fn log_volume_is_capped() {
    let limits = rt().limits();
    let inv = run(&format!(
        "for (let i = 0; i < {}; i++) console.log(i);",
        limits.max_log_lines
    ));
    assert_eq!(inv.result, Ok(RequestOutcome::Unchanged));
    assert_eq!(inv.logs.len(), limits.max_log_lines);
    // 再多一行这次调用就失败；已经写下的留着
    let inv = run(&format!(
        "for (let i = 0; i <= {}; i++) console.log(i);",
        limits.max_log_lines
    ));
    assert_eq!(inv.result, Err(RunError::OutputLimit));
    assert_eq!(inv.logs.len(), limits.max_log_lines);
    // 一行太长就截断，不失败
    let inv = run("console.log('é'.repeat(10000));");
    assert_eq!(inv.result, Ok(RequestOutcome::Unchanged));
    let line = &inv.logs[0].text;
    assert!(line.len() <= limits.max_log_line, "{}", line.len());
    assert!(line.ends_with('…'));
}

#[test]
fn reply_calls_have_their_own_and_a_total_cpu_budget() {
    let limits = rt().limits().clone();
    let p = load(
        r#"export const manifest = { name: "slow", api: 1, permissions: ["reply.text"] };
           export function onReplyText(t) {
             if (t === "spin") for (;;) {}
             const end = Date.now() + Number(t); while (Date.now() < end) {}
             return undefined;
           }"#,
    );
    let mut r = p.reply(ctx()).unwrap();
    let inv = r.on_text("spin");
    assert_eq!(inv.result, Err(RunError::CpuLimit));
    assert!(
        inv.cpu >= limits.reply_call_cpu && inv.cpu < limits.reply_call_cpu + slack(),
        "{:?}",
        inv.cpu
    );
    // 被打断过的实例不再用
    assert_eq!(r.on_text("0").result, Err(RunError::CpuLimit));

    // 每次都没超单次的预算，但加起来超过一个回答的总预算。单次预算放宽到
    // 远大于每次的用量：Windows 上量的是墙上时间，被抢占一下就可能先撞单次上限
    static RT: OnceLock<Runtime> = OnceLock::new();
    let rt = RT.get_or_init(|| {
        Runtime::new(Limits {
            reply_call_cpu: Duration::from_millis(500),
            reply_total_cpu: Duration::from_secs(1),
            ..Limits::default()
        })
        .expect("runtime")
    });
    let total = rt.limits().reply_total_cpu;
    let p = rt
        .load(
            br#"export const manifest = { name: "busy", api: 1, permissions: ["reply.text"] };
                export function onReplyText(t) { const end = Date.now() + Number(t); while (Date.now() < end) {} }"#,
        )
        .unwrap();
    let mut r = p.reply(ctx()).unwrap();
    let mut used = Duration::ZERO;
    let mut stopped = false;
    for _ in 0..1000 {
        let inv = r.on_text("10");
        used += inv.cpu;
        if inv.result == Err(RunError::CpuLimit) {
            stopped = true;
            break;
        }
        assert_eq!(inv.result, Ok(None));
    }
    assert!(stopped, "never stopped after {used:?}");
    assert!(used >= total - Duration::from_millis(5), "{used:?}");
    assert!(used < total + slack(), "{used:?}");
}

#[test]
fn traps_never_take_the_process_down() {
    // 多个线程同时撞各种上限、各种陷阱，然后一切照常
    let p = request("return req;");
    let bombs = [
        "for (;;) {}",
        "const a = []; for (;;) a.push(new Uint8Array(1 << 20));",
        "JSON.parse('['.repeat(1e6));",
        "function f() { f(); } f();",
    ];
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let body = bombs[i % bombs.len()];
            std::thread::spawn(move || run(body).result)
        })
        .collect();
    for h in handles {
        assert!(h.join().expect("the thread survived").is_err());
    }
    assert_eq!(
        p.on_request(view(), ctx()).result,
        Ok(RequestOutcome::Unchanged)
    );
}

#[test]
fn a_plugin_is_shared_across_threads() {
    let p = request("req.system = String(ctx.model); return req;");
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let p = p.clone();
            std::thread::spawn(move || {
                for _ in 0..20 {
                    let inv = p.on_request(view(), ctx());
                    assert_eq!(changed(&inv)["system"], "claude-x");
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

// ── 大请求 ───────────────────────────────────────────────────────

/// 一个像真的 Anthropic 请求视图：很多轮对话，文字里夹着要替换的词
fn big_view(target: usize) -> (Value, usize) {
    let words = [
        "the",
        "function",
        "returns",
        "a",
        "value",
        "when",
        "config",
        "widget",
        "请求",
        "上游",
        "🙂",
        "\"quoted\"",
        "line\nbreak",
    ];
    let mut messages = Vec::new();
    let mut size = 0;
    let mut hits = 0;
    let mut i = 0usize;
    while size < target {
        let mut text = String::new();
        for j in 0..200 {
            let w = words[(i * 7 + j * 13) % words.len()];
            if w == "widget" {
                hits += 1;
            }
            text.push_str(w);
            text.push(' ');
        }
        size += text.len() + 100;
        messages.push(json!({ "key": format!("m{i}"), "role": if i % 2 == 0 { "user" } else { "assistant" },
                              "parts": [ { "key": format!("p{i}"), "type": "text", "text": text } ] }));
        i += 1;
    }
    (
        json!({ "format": "anthropic", "model": "claude-x", "system": "s", "messages": messages }),
        hits,
    )
}

fn edit_big_view(size: usize) -> Duration {
    let p = load(
        r#"export const manifest = { name: "words", api: 1, permissions: ["messages"] };
           export function onRequest(req) {
             for (const m of req.messages) for (const p of m.parts) if (p.type === "text") p.text = p.text.replaceAll("widget", "gadget");
             return req;
           }"#,
    );
    let (v, hits) = big_view(size);
    let bytes = serde_json::to_vec(&v).unwrap().len();
    assert!(bytes >= size);
    let t = Instant::now();
    let inv = p.on_request(v, ctx());
    let wall = t.elapsed();
    let out = changed(&inv);
    let text = serde_json::to_string(out).unwrap();
    assert_eq!(text.matches("gadget").count(), hits);
    assert!(!text.contains("widget"));
    println!(
        "edit a {bytes} byte view: wall {wall:?}, sandbox cpu {:?}",
        inv.cpu
    );
    inv.cpu
}

#[test]
fn a_plugin_edits_a_100_kb_view() {
    edit_big_view(100 << 10);
}

#[test]
fn a_plugin_edits_a_1_mb_view() {
    let cpu = edit_big_view(1 << 20);
    // 默认的预算放得下：量的是 CPU 时间（Windows 上是墙上时间，并发跑测试时不准）
    if !cfg!(windows) {
        assert!(cpu < rt().limits().request_cpu, "{cpu:?}");
    }
}

// ── 加载 ─────────────────────────────────────────────────────────

#[test]
fn syntax_errors_carry_line_and_column() {
    match load_err(
        "export const manifest = { name: 'x', api: 1, permissions: ['system'] };\n\nconst x = ;\nexport function onRequest() {}\n",
    ) {
        LoadError::Syntax {
            message,
            line,
            column,
        } => {
            assert!(message.starts_with("SyntaxError"), "{message}");
            assert_eq!(line, Some(3));
            assert!(column.is_some());
        }
        other => panic!("{other:?}"),
    }
    // 顶层一执行就抛：也带位置
    match load_err(&format!(
        "{SYSTEM}\nnull.x;\nexport function onRequest() {{}}"
    )) {
        LoadError::Syntax { message, line, .. } => {
            assert!(message.starts_with("TypeError"), "{message}");
            assert_eq!(line, Some(3));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn source_must_be_small_utf8() {
    let big = format!(
        "{SYSTEM}export function onRequest() {{}}\n//{}",
        "x".repeat(1 << 20)
    );
    assert_eq!(load_err(&big), LoadError::TooLarge);
    let mut bytes = b"export const manifest = {};\n// ok\n// \xff\n".to_vec();
    bytes.extend_from_slice(b"\n");
    match rt().load(&bytes) {
        Err(LoadError::Syntax { line, column, .. }) => {
            assert_eq!(line, Some(3));
            assert_eq!(column, Some(4));
        }
        other => panic!("{other:?}"),
    }
    // 开头的 BOM 照常
    let p = rt()
        .load(format!("\u{feff}{SYSTEM}export function onRequest() {{}}").as_bytes())
        .unwrap();
    assert_eq!(p.manifest().name, "t");
}

#[test]
fn top_level_limits_fail_the_load() {
    for (rt, body, want) in [
        (rt(), "for (;;) {}", "CPU"),
        (
            roomy(),
            "const a = []; for (;;) a.push(new Uint8Array(16 << 20));",
            "memory",
        ),
    ] {
        let src = format!("{SYSTEM}{body}\nexport function onRequest() {{}}");
        match rt.load(src.as_bytes()) {
            Err(LoadError::Syntax { message, .. }) => assert!(message.contains(want), "{message}"),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn the_sha256_is_of_the_exact_bytes() {
    let src = format!("{SYSTEM}export function onRequest() {{}}\n");
    let p = load(&src);
    use sha2::Digest;
    let want: [u8; 32] = sha2::Sha256::digest(src.as_bytes()).into();
    assert_eq!(p.sha256(), want);
}

#[test]
fn a_full_manifest_is_read() {
    let p = load(
        r#"export const manifest = {
             name: "  附加当前日期  ",
             api: 1,
             description: "adds the date",
             permissions: ["system", "params", "reply.text"],
             match: { clients: ["claude-code"], models: ["claude-*"], upstreams: ["anthropic"] },
             reply: "block",
             settings: {
               zeta: { type: "string", label: "附加内容", default: "x" },
               alpha: { type: "number" },
               mid: { type: "boolean", label: "On", default: true },
             },
           };
           export function onRequest() {}
           export function onReplyText() {}"#,
    );
    let m = p.manifest();
    assert_eq!(m.name, "附加当前日期");
    assert_eq!(m.description.as_deref(), Some("adds the date"));
    assert_eq!(
        m.permissions.iter().copied().collect::<Vec<_>>(),
        vec![
            Permission::System,
            Permission::Params,
            Permission::ReplyText
        ]
    );
    assert_eq!(m.scope.models, vec!["claude-*"]);
    // 设置项保持作者写的先后
    let keys: Vec<_> = m.settings.iter().map(|s| s.key.as_str()).collect();
    assert_eq!(keys, vec!["zeta", "alpha", "mid"]);
    assert_eq!(m.settings[0].label, "附加内容");
    assert_eq!(m.settings[1].label, "alpha");
    assert_eq!(m.settings[1].default, json!(0));
    assert_eq!(m.settings[2].default, json!(true));
    assert_eq!(
        m.hooks,
        Hooks {
            request: true,
            reply_text: true,
            reply_text_end: false,
            tool_call: false
        }
    );
}

#[test]
fn manifest_errors_say_what_is_wrong() {
    let hook = "export function onRequest() {}";
    let cases: Vec<(String, &str)> = vec![
        (hook.to_string(), "does not export a manifest"),
        (format!("export default {{ manifest: {{}} }};\n{hook}"), "not as a default export"),
        (format!("export const manifest = 3;\n{hook}"), "must be an object"),
        (format!("export const manifest = {{ api: 1, permissions: ['system'] }};\n{hook}"), "needs a `name`"),
        (format!("export const manifest = {{ name: '', api: 1, permissions: ['system'] }};\n{hook}"), "must not be empty"),
        (format!("export const manifest = {{ name: 'x'.repeat(65), api: 1, permissions: ['system'] }};\n{hook}"), "at most 64"),
        (format!("export const manifest = {{ name: 'x', permissions: ['system'] }};\n{hook}"), "api: 1"),
        (format!("export const manifest = {{ name: 'x', api: 1, permissions: [] }};\n{hook}"), "at least one"),
        (format!("export const manifest = {{ name: 'x', api: 1, permissions: ['network'] }};\n{hook}"), "unknown permission \"network\""),
        (format!("export const manifest = {{ name: 'x', api: 1, permissions: ['system', 'system'] }};\n{hook}"), "listed twice"),
        (format!("export const manifest = {{ name: 'x', api: 1, permissions: ['system'], extra: 1 }};\n{hook}"), "unknown field `extra`"),
        (format!("export const manifest = {{ name: 'x', api: 1, permissions: ['system'], reply: 'fast' }};\n{hook}"), "\"block\" or \"stream\""),
        (format!("export const manifest = {{ name: 'x', api: 1, permissions: ['system'], match: {{ hosts: [] }} }};\n{hook}"), "unknown field `hosts`"),
        (format!("export const manifest = {{ name: 'x', api: 1, permissions: ['system'], settings: {{ a: {{ type: 'date' }} }} }};\n{hook}"), "needs a type"),
        (format!("export const manifest = {{ name: 'x', api: 1, permissions: ['system'], settings: {{ a: {{ type: 'number', default: 'x' }} }} }};\n{hook}"), "must be a number"),
        (format!("export const manifest = {{ name: 'x', api: 1, permissions: ['system'], settings: Object.fromEntries(Array.from({{length: 21}}, (_, i) => ['k' + i, {{ type: 'string' }}])) }};\n{hook}"), "at most 20"),
        (format!("export const manifest = {{ name: 'x', api: 1, permissions: ['system'], settings: {{ '1x': {{ type: 'string' }} }} }};\n{hook}"), "invalid name"),
        // 钩子和权限一一对应
        ("export const manifest = { name: 'x', api: 1, permissions: ['reply.text'] };\nexport function onRequest() {}".into(), "requests none of"),
        ("export const manifest = { name: 'x', api: 1, permissions: ['system', 'reply.text'] };\nexport function onRequest() {}".into(), "onReplyText is not exported"),
        ("export const manifest = { name: 'x', api: 1, permissions: ['tools'] };\nexport function onReplyText() {}".into(), "onRequest is not exported"),
        ("export const manifest = { name: 'x', api: 1, permissions: ['system'] };\nexport function onRequest() {}\nexport function onToolCall() {}".into(), "\"reply.tool_calls\""),
        ("export const manifest = { name: 'x', api: 1, permissions: ['reply.text'] };\nexport function onReplyText() {}\nexport function onReplyTextEnd() {}".into(), "only called in stream mode"),
        ("export const manifest = { name: 'x', api: 1, permissions: ['system'] };\nexport const onRequest = 5;".into(), "not a function"),
        ("export const manifest = { name: 'x', api: 1, permissions: ['system'] };\nexport function onReqest() {}".into(), "onRequest is not exported"),
    ];
    for (src, want) in cases {
        let m = manifest_err(&src);
        assert!(m.contains(want), "{src}\n=> {m}\n(want {want})");
    }
    assert_eq!(
        load_err(&format!(
            "export const manifest = {{ name: 'x', api: 2, permissions: ['system'], newField: 1 }};\n{hook}"
        )),
        LoadError::UnsupportedApi(2)
    );
}

#[test]
fn the_types_cross_threads_as_promised() {
    fn send_sync<T: Send + Sync>() {}
    fn send<T: Send>() {}
    send_sync::<Runtime>();
    send_sync::<Plugin>();
    send::<Reply>();
}
