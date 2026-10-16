//! 插件辅助函数 `tw`（`src/helpers.js`）在真的沙箱里的行为：每个函数正常的用法、各种
//! 用错的地方、缺权限时的报错；替换规则的语义；逐段替换的切分性质和多块文字交错；插件
//! 改了内建函数之后 tw 照常工作。
//!
//! 插件源码都写在测试里；切分性质的随机测试是语料里的 `helpers-split.js`，在沙箱里自己
//! 循环切分，跑得快。

mod common;

use std::sync::OnceLock;

use serde_json::{Value, json};
use tw_plugin::*;

fn rt() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new(Limits::default()).expect("runtime"))
}

fn load(src: &str) -> Plugin {
    match rt().load(src.as_bytes()) {
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

fn ctx(settings: Value) -> Value {
    json!({
        "client": "claude-code",
        "model": "claude-x",
        "format": "anthropic",
        "upstream": "relay",
        "settings": settings,
    })
}

/// 一段对话的视图：每种片段都有
fn view() -> Value {
    json!({
        "format": "anthropic",
        "model": "claude-x",
        "system": "be brief",
        "messages": [
            { "key": "m0", "role": "user", "parts": [
                { "key": "p0", "type": "text", "text": "read /mnt/c/a.txt" },
                { "key": "p1", "type": "image", "media_type": "image/png" },
            ] },
            { "key": "m1", "role": "assistant", "parts": [
                { "key": "p2", "type": "thinking", "text": "hmm" },
                { "key": "p3", "type": "text", "text": "ok" },
                { "key": "p4", "type": "tool_call", "id": "t1", "name": "Read", "input": { "path": "/mnt/c/a.txt", "n": [1, "/mnt/d"] } },
            ] },
            { "key": "m2", "role": "tool", "parts": [
                { "key": "p5", "type": "tool_result", "call_id": "t1", "text": "contents", "is_error": false },
                { "key": "p6", "type": "other", "label": "document" },
            ] },
        ],
        "params": { "model": "claude-x", "max_tokens": 1024 }
    })
}

/// 嵌入的视图：一项输入一条 user 消息，没有系统提示词
fn inputs_view() -> Value {
    json!({
        "format": "openai_embeddings",
        "model": "e",
        "messages": [
            { "key": "m0", "role": "user", "parts": [ { "key": "p0", "type": "text", "text": "one" } ] },
            { "key": "m1", "role": "user", "parts": [
                { "key": "p1", "type": "text", "text": "two" },
                { "key": "p2", "type": "other", "label": "tokens" },
            ] },
        ],
    })
}

fn plugin(perms: &str, body: &str) -> Plugin {
    load(&format!(
        "export const manifest = {{ name: \"t\", api: 1, permissions: [{perms}] }};\n\
         export function onRequest(req, ctx) {{ {body} }}"
    ))
}

const ALL: &str = r#""system", "messages", "tools", "params""#;

/// 有全部请求权限的插件对 `view` 跑一次
fn run_on(view: Value, body: &str) -> Invocation<RequestOutcome> {
    plugin(ALL, body).on_request(view, ctx(json!({})))
}

fn run(body: &str) -> Invocation<RequestOutcome> {
    run_on(view(), body)
}

fn changed(inv: &Invocation<RequestOutcome>) -> Value {
    match &inv.result {
        Ok(RequestOutcome::Changed(v)) => v.clone(),
        other => panic!("expected Changed, got {other:?} (logs {:?})", inv.logs),
    }
}

/// 算一个值带出来：`out(v)` 写进系统提示词
fn eval(expr: &str) -> Value {
    let inv = run(&format!("req.system = JSON.stringify({expr});"));
    let v = changed(&inv);
    serde_json::from_str(v["system"].as_str().unwrap()).unwrap()
}

fn threw(inv: &Invocation<RequestOutcome>) -> String {
    match &inv.result {
        Err(RunError::Threw { message, .. }) => message.clone(),
        other => panic!("expected Threw, got {other:?}"),
    }
}

fn eval_err(expr: &str) -> String {
    threw(&run(&format!("req.system = JSON.stringify({expr});")))
}

// ── 全局 ─────────────────────────────────────────────────────────

#[test]
fn tw_is_a_frozen_global_the_plugin_may_shadow() {
    let v = eval(
        r#"{
          frozen: Object.isFrozen(tw),
          names: Object.keys(tw).sort(),
          enumerable: Object.keys(globalThis).includes("tw"),
          assign: (() => { try { tw.replace = null; return "assigned"; } catch (e) { return e.constructor.name; } })(),
        }"#,
    );
    assert_eq!(v["frozen"], true);
    assert_eq!(
        v["names"],
        json!([
            "appendSystem",
            "lines",
            "mapStrings",
            "pairs",
            "replace",
            "streamReplace",
            "texts",
            "toolCalls"
        ])
    );
    assert_eq!(v["enumerable"], false);
    assert_eq!(v["assign"], "TypeError");
    // 和 console、reject 一样，全局的这个名字插件可以换掉：只影响它自己
    let inv = run("globalThis.tw = { appendSystem() {} }; tw.appendSystem(req, 'x');");
    assert_eq!(inv.result, Ok(RequestOutcome::Unchanged));
}

#[test]
fn errors_point_at_the_plugin_not_at_tw() {
    let inv = run("tw.toolCalls(req, 5);");
    let Err(RunError::Threw { message, stack }) = &inv.result else {
        panic!("{:?}", inv.result);
    };
    assert_eq!(
        message,
        "TypeError: tw.toolCalls expects a function, not a number"
    );
    let stack = stack.as_deref().unwrap_or("");
    assert!(stack.contains("plugin.js"), "{stack}");
    assert!(!stack.contains("bridge.js"), "{stack}");
}

// ── appendSystem ─────────────────────────────────────────────────

#[test]
fn append_system_adds_a_paragraph_once() {
    let v = changed(&run("tw.appendSystem(req, 'Answer in English.');"));
    assert_eq!(v["system"], "be brief\n\nAnswer in English.");
    // 已经有了：不再附加
    assert_eq!(
        run("tw.appendSystem(req, 'brief');").result,
        Ok(RequestOutcome::Unchanged)
    );
    // 空的系统提示词：直接写入
    let v = changed(&run("req.system = ''; tw.appendSystem(req, 'x');"));
    assert_eq!(v["system"], "x");
    // 返回 req 也一样
    let v = changed(&run("tw.appendSystem(req, 'y'); return req;"));
    assert_eq!(v["system"], "be brief\n\ny");
}

#[test]
fn append_system_needs_the_system_permission_and_a_string() {
    let p = plugin(r#""messages""#, "tw.appendSystem(req, 'x');");
    let mut v = view();
    v.as_object_mut().unwrap().remove("system");
    assert_eq!(
        threw(&p.on_request(v, ctx(json!({})))),
        r#"TypeError: tw.appendSystem needs req.system: add the "system" permission"#
    );
    assert_eq!(
        threw(&run("tw.appendSystem(req, 5);")),
        "TypeError: tw.appendSystem: the text must be a string, not a number"
    );
    assert_eq!(
        threw(&run("tw.appendSystem(null, 'x');")),
        "TypeError: tw.appendSystem expects the request object, not null"
    );
    assert_eq!(
        threw(&run("req.system = 3; tw.appendSystem(req, 'x');")),
        "TypeError: tw.appendSystem: req.system must be a string, not a number"
    );
}

// ── texts ────────────────────────────────────────────────────────

#[test]
fn texts_visits_the_system_prompt_text_and_tool_results_in_order() {
    let inv = run(r#"
        const seen = [];
        tw.texts(req, (t, place) => { seen.push([t, place.kind, place.role]); return t === "ok" ? "OK" : undefined; });
        console.log(JSON.stringify(seen));
    "#);
    let seen: Value = serde_json::from_str(&inv.logs[0].text).unwrap();
    assert_eq!(
        seen,
        json!([
            ["be brief", "system", null],
            ["read /mnt/c/a.txt", "text", "user"],
            ["ok", "text", "assistant"],
            ["contents", "tool_result", "tool"],
        ])
    );
    let v = changed(&inv);
    assert_eq!(v["messages"][1]["parts"][1]["text"], "OK");
    // 别的原样：思考、工具调用、图片
    let mut want = view();
    want["messages"][1]["parts"][1]["text"] = json!("OK");
    assert_eq!(v, want);
}

#[test]
fn texts_with_replace_rewrites_every_text() {
    let v = changed(&run(
        "tw.texts(req, (t) => tw.replace(t, [[\"/mnt/c/\", \"C:/\"], [\"brief\", \"short\"]]));",
    ));
    assert_eq!(v["system"], "be short");
    assert_eq!(v["messages"][0]["parts"][0]["text"], "read C:/a.txt");
    // 工具调用的参数不是文字
    assert_eq!(
        v["messages"][1]["parts"][2]["input"]["path"],
        "/mnt/c/a.txt"
    );
}

#[test]
fn texts_on_an_inputs_view_passes_each_input_as_user_text() {
    let inv = run_on(
        inputs_view(),
        r#"
        const seen = [];
        tw.texts(req, (t, place) => { seen.push([t, place.kind, place.role]); return t.toUpperCase(); });
        console.log(JSON.stringify(seen));
    "#,
    );
    let seen: Value = serde_json::from_str(&inv.logs[0].text).unwrap();
    assert_eq!(
        seen,
        json!([["one", "text", "user"], ["two", "text", "user"]])
    );
    let v = changed(&inv);
    assert_eq!(v["messages"][1]["parts"][0]["text"], "TWO");
    assert_eq!(
        v["messages"][1]["parts"][1],
        json!({ "key": "p2", "type": "other", "label": "tokens" })
    );
}

#[test]
fn texts_checks_what_the_function_returns_and_what_it_is_given() {
    assert_eq!(
        threw(&run("tw.texts(req, () => 5);")),
        "TypeError: tw.texts: the function returned a number; return a string, or undefined to keep the text"
    );
    assert_eq!(
        threw(&run("tw.texts(req, () => null);")),
        "TypeError: tw.texts: the function returned null; return a string, or undefined to keep the text"
    );
    assert_eq!(
        threw(&run("tw.texts(req);")),
        "TypeError: tw.texts expects a function, not undefined"
    );
    // 两节都没给
    let p = plugin(r#""params""#, "tw.texts(req, (t) => t);");
    assert_eq!(
        threw(&p.on_request(
            json!({ "format": "anthropic", "model": "m", "params": {} }),
            ctx(json!({}))
        )),
        r#"TypeError: tw.texts needs req.system or req.messages: add the "system" or "messages" permission"#
    );
    // 只给了一节：那一节照常
    let p = plugin(r#""system""#, "tw.texts(req, (t) => t + '!');");
    let inv = p.on_request(
        json!({ "format": "anthropic", "model": "m", "system": "s" }),
        ctx(json!({})),
    );
    assert_eq!(changed(&inv)["system"], "s!");
    assert_eq!(
        threw(&run("req.messages = {}; tw.texts(req, (t) => t);")),
        "TypeError: tw.texts: req.messages is an object, not an array"
    );
    assert_eq!(
        threw(&run(
            "req.messages.push({ role: 'user' }); tw.texts(req, (t) => t);"
        )),
        "TypeError: tw.texts: req.messages[3] has no parts array"
    );
}

// ── toolCalls ────────────────────────────────────────────────────

#[test]
fn tool_calls_hands_over_the_live_parts() {
    let inv = run(r#"
        const seen = [];
        tw.toolCalls(req, (call) => { seen.push(call.name); call.input.path = "C:/a.txt"; return "ignored"; });
        console.log(JSON.stringify(seen));
    "#);
    assert_eq!(inv.logs[0].text, r#"["Read"]"#);
    let v = changed(&inv);
    assert_eq!(
        v["messages"][1]["parts"][2],
        json!({ "key": "p4", "type": "tool_call", "id": "t1", "name": "Read", "input": { "path": "C:/a.txt", "n": [1, "/mnt/d"] } })
    );
    // 什么都没改：没改
    assert_eq!(
        run("tw.toolCalls(req, () => {});").result,
        Ok(RequestOutcome::Unchanged)
    );
}

#[test]
fn tool_calls_needs_the_messages_permission() {
    let p = plugin(r#""system""#, "tw.toolCalls(req, () => {});");
    assert_eq!(
        threw(&p.on_request(
            json!({ "format": "anthropic", "model": "m", "system": "s" }),
            ctx(json!({}))
        )),
        r#"TypeError: tw.toolCalls needs req.messages: add the "messages" permission"#
    );
    assert_eq!(
        threw(&run("tw.toolCalls(req, 'f');")),
        "TypeError: tw.toolCalls expects a function, not a string"
    );
}

// ── mapStrings ───────────────────────────────────────────────────

#[test]
fn map_strings_copies_and_rewrites_string_values_only() {
    let v = eval(
        r#"(() => {
          const input = { path: "a", list: ["b", 1, true, null, { deep: "c" }], n: 2, "x y": "d" };
          const keys = [];
          const out = tw.mapStrings(input, (s, k) => { keys.push(k); return s.toUpperCase(); });
          return { out, keys, untouched: input.path === "a" && input.list[4].deep === "c", top: tw.mapStrings("t", (s, k) => s + String(k)) };
        })()"#,
    );
    assert_eq!(
        v["out"],
        json!({ "path": "A", "list": ["B", 1, true, null, { "deep": "C" }], "n": 2, "x y": "D" })
    );
    assert_eq!(v["keys"], json!(["path", 0, "deep", "x y"]));
    assert_eq!(v["untouched"], true);
    assert_eq!(v["top"], "tnull");
}

#[test]
fn map_strings_keeps_proto_keys_as_plain_keys() {
    // JSON.parse 建出来的 `__proto__` 是一个普通的自有键：副本里也是，原型不变
    let v = eval(
        r#"(() => {
          const input = JSON.parse('{"__proto__": {"polluted": "yes"}, "constructor": "c"}');
          const out = tw.mapStrings(input, (s) => s + "!");
          return {
            own: Object.prototype.hasOwnProperty.call(out, "__proto__"),
            protoIntact: Object.getPrototypeOf(out) === Object.prototype,
            polluted: ({}).polluted === undefined ? "no" : "yes",
            json: JSON.stringify(out),
          };
        })()"#,
    );
    assert_eq!(v["own"], true);
    assert_eq!(v["protoIntact"], true);
    assert_eq!(v["polluted"], "no");
    assert_eq!(
        v["json"],
        r#"{"__proto__":{"polluted":"yes!"},"constructor":"c!"}"#
    );
}

#[test]
fn map_strings_rejects_bad_functions_and_cycles() {
    assert_eq!(
        eval_err("tw.mapStrings({ a: 'x' }, () => 1)"),
        "TypeError: tw.mapStrings: the function returned a number; it has to return a string"
    );
    assert_eq!(
        eval_err("tw.mapStrings({ a: 'x' })"),
        "TypeError: tw.mapStrings expects a function, not undefined"
    );
    assert_eq!(
        eval_err(
            "(() => { const o = { a: [] }; o.a.push(o); return tw.mapStrings(o, (s) => s); })()"
        ),
        "TypeError: tw.mapStrings: the value refers to itself"
    );
    // 同一个对象出现两次不是环
    assert_eq!(
        eval("(() => { const s = { a: 'x' }; return tw.mapStrings([s, s], (t) => t + '1'); })()"),
        json!([{ "a": "x1" }, { "a": "x1" }])
    );
}

// ── replace ──────────────────────────────────────────────────────

#[test]
fn replace_with_strings_is_literal_and_case_sensitive() {
    assert_eq!(
        eval(r#"tw.replace("a $1 cost $5, A", [["$5", "$&$$"], ["a", "b"]])"#),
        "b $1 cost $&$$, A"
    );
    assert_eq!(eval(r#"tw.replace("aAa", [[/a/i, "x"]])"#), "xxx");
}

#[test]
fn replace_with_regexes_works_like_string_replace() {
    assert_eq!(
        eval(r#"tw.replace("John Smith", [[/(\w+)\s(\w+)/, "$2, $1"]])"#),
        "Smith, John"
    );
    assert_eq!(
        eval(
            r#"tw.replace("2026-10-16", [[/(?<y>\d{4})-(?<m>\d\d)-(?<d>\d\d)/, "$<d>/$<m>/$<y>"]])"#
        ),
        "16/10/2026"
    );
    assert_eq!(
        eval(r#"tw.replace("abc", [[/b/, "[$&|$`|$'|$$|$0|$9|$<x>]"]])"#),
        "a[b|a|c|$|$0|$9|$<x>]c"
    );
    assert_eq!(
        eval(r#"tw.replace("abc", [[/(b)/, "$01$11$2"]])"#),
        "abb1$2c"
    );
    // 函数：命中、各组、位置、整段、具名组，和 String.prototype.replace 一样
    assert_eq!(
        eval(
            r#"tw.replace("x-ab-ab", [[/(a)(?<n>b)/, (m, a, b, at, s, g) => `${m}${a}${b}${at}${s.length}${g.n}`]])"#
        ),
        "x-abab27b-abab57b"
    );
    // 没有 g 也找全部；不用写 g
    assert_eq!(eval(r#"tw.replace("aaa", [[/a/, "b"]])"#), "bbb");
    assert_eq!(eval(r#"tw.replace("aaa", [[/a/g, "b"]])"#), "bbb");
    // 函数替换，查找是字符串
    assert_eq!(
        eval(r#"tw.replace("xaxa", [["a", (m, at) => m + at]])"#),
        "xa1xa3"
    );
}

#[test]
fn replace_scans_once_left_to_right_and_the_first_rule_wins_a_tie() {
    // 换进去的字不再参与匹配：A→B、B→C 并存时，A 不会变成 C
    assert_eq!(eval(r#"tw.replace("AB", [["A", "B"], ["B", "C"]])"#), "BC");
    // 同一位置：列表里靠前的
    assert_eq!(
        eval(r#"tw.replace("abc", [["ab", "1"], ["abc", "2"]])"#),
        "1c"
    );
    assert_eq!(
        eval(r#"tw.replace("abc", [["abc", "2"], ["ab", "1"]])"#),
        "2"
    );
    // 不同位置：靠左的，不管规则的先后
    assert_eq!(
        eval(r#"tw.replace("abc", [["bc", "1"], ["ab", "2"]])"#),
        "2c"
    );
    // 正则和字符串一起比
    assert_eq!(eval(r#"tw.replace("a1", [[/\d/, '#'], ["a1", "x"]])"#), "x");
    // Map 也行
    assert_eq!(
        eval(r#"tw.replace("cat dog", new Map([["cat", "dog"], ["dog", "cat"]]))"#),
        "dog cat"
    );
    // 别的可迭代对象
    assert_eq!(
        eval(r#"tw.replace("ab", (function* () { yield ["a", "1"]; yield ["b", "2"]; })())"#),
        "12"
    );
    assert_eq!(eval(r#"tw.replace("ab", [])"#), "ab");
}

#[test]
fn replace_leaves_placeholders_whole() {
    // 占位符整个跳过：里面的字不匹配，命中也不跨过它
    assert_eq!(
        eval(
            r#"tw.replace("key <<TW_SECRET_12>> and <<TW_EMAIL_3>>", [["SECRET", "x"], ["_1", "y"], ["<<", "«"], [/\d+>>/, "?"], ["> and <", "|"]])"#
        ),
        "key <<TW_SECRET_12>> and <<TW_EMAIL_3>>"
    );
    assert_eq!(
        eval(r#"tw.replace("a<<TW_SECRET_1>>b", [["ab", "X"], [/a.*b/s, "Y"]])"#),
        "a<<TW_SECRET_1>>b"
    );
    // 两个占位符之间的每一节是一段单独的文字：^、$ 落在占位符旁边
    assert_eq!(
        eval(r#"tw.replace("ab<<TW_SECRET_1>>ab", [[/^a/, "^"], [/b$/, "$$"]])"#),
        "^$<<TW_SECRET_1>>^$"
    );
    // 不像占位符的照常换
    assert_eq!(
        eval(r#"tw.replace("<<TW_x>> <<TW_SECRET_1>", [["<<", "«"]])"#),
        "«TW_x>> «TW_SECRET_1>"
    );
}

#[test]
fn replace_rejects_bad_rules() {
    for (rules, msg) in [
        (
            "'a'",
            "tw.replace: the rules must be an array or a Map of [find, replacement] pairs, not a string",
        ),
        (
            "null",
            "tw.replace: the rules must be an array or a Map of [find, replacement] pairs, not null",
        ),
        (
            "[['a']]",
            "tw.replace: rule 1 is not a [find, replacement] pair",
        ),
        (
            "[['a', 'b'], 'c']",
            "tw.replace: rule 2 is not a [find, replacement] pair",
        ),
        (
            "[['', 'b']]",
            "tw.replace: rule 1 looks for an empty string",
        ),
        (
            "[[1, 'b']]",
            "tw.replace: rule 1 looks for a number; use a string or a regular expression",
        ),
        (
            "[[/a*/, 'b']]",
            "tw.replace: rule 1 can match an empty string; a rule has to match at least one character",
        ),
        (
            "[[/(?:)/, 'b']]",
            "tw.replace: rule 1 can match an empty string; a rule has to match at least one character",
        ),
        (
            "[[/a/y, 'b']]",
            "tw.replace: rule 1 has the y flag, which a rule cannot use",
        ),
        (
            "[['a', 1]]",
            "tw.replace: rule 1 replaces with a number; use a string or a function",
        ),
        (
            "[['a', null]]",
            "tw.replace: rule 1 replaces with null; use a string or a function",
        ),
    ] {
        assert_eq!(
            eval_err(&format!("tw.replace('abc', {rules})")),
            format!("TypeError: {msg}"),
            "{rules}"
        );
    }
    assert_eq!(
        eval_err("tw.replace(5, [])"),
        "TypeError: tw.replace: the text must be a string, not a number"
    );
    // 只在某些位置匹配空串的（\b、前瞻）：碰到时报错
    assert_eq!(
        eval_err(r#"tw.replace("a b", [[/\b/, "|"]])"#),
        "TypeError: tw.replace: rule 1 matched an empty string; a rule has to match at least one character"
    );
    // 替换函数抛的错原样抛给插件
    assert_eq!(
        eval_err(r#"tw.replace("a", [["a", () => { throw new RangeError("mine"); }]])"#),
        "RangeError: mine"
    );
}

#[test]
fn replace_handles_a_large_text_quickly() {
    // 1 MB 的文字、几条规则：远在请求钩子的 CPU 上限之内
    let inv = run(r#"
        const text = "the widget costs 12 dollars; ".repeat(36000);
        const out = tw.replace(text, [["widget", "gadget"], [/\d+/, '#'], ["dollars", "USD"]]);
        req.system = out.length + "|" + out.slice(0, 22);
    "#);
    assert_eq!(changed(&inv)["system"], "864000|the gadget costs # USD");
}

// ── lines / pairs ────────────────────────────────────────────────

#[test]
fn lines_splits_trims_and_drops_empty_lines() {
    assert_eq!(
        eval(r#"tw.lines(" a \r\n\n  b\t\n\r\n c")"#),
        json!(["a", "b", "c"])
    );
    assert_eq!(eval(r#"tw.lines("")"#), json!([]));
    assert_eq!(
        eval(r#"Array.isArray(tw.lines("a")) && tw.lines("a").map((x) => x + "!")[0]"#),
        "a!"
    );
    assert_eq!(
        eval_err("tw.lines(undefined)"),
        "TypeError: tw.lines: the text must be a string, not undefined"
    );
}

#[test]
fn pairs_splits_each_line_at_the_first_separator() {
    assert_eq!(
        eval(r#"tw.pairs("colour => color\n\n  a=>b=>c \r\nx =>")"#),
        json!([["colour", "color"], ["a", "b=>c"], ["x", ""]])
    );
    assert_eq!(
        eval(r#"tw.pairs("a: b\nc:d", ":")"#),
        json!([["a", "b"], ["c", "d"]])
    );
    // 格式不对的行：报错，说出第几行（空行也算行号）
    assert_eq!(
        eval_err(r#"tw.pairs("a => b\n\nno separator")"#),
        r#"Error: tw.pairs: line 3 has no "=>""#
    );
    assert_eq!(
        eval_err(r#"tw.pairs("a => b\n => c")"#),
        r#"Error: tw.pairs: line 2 has nothing before "=>""#
    );
    assert_eq!(
        eval_err(r#"tw.pairs("a", "")"#),
        "TypeError: tw.pairs: the separator must not be empty"
    );
    assert_eq!(
        eval_err(r#"tw.pairs("a", 1)"#),
        "TypeError: tw.pairs: the separator must be a string, not a number"
    );
    assert_eq!(
        eval_err(r#"tw.pairs(1)"#),
        "TypeError: tw.pairs: the text must be a string, not a number"
    );
    // 和 replace 连起来用
    assert_eq!(
        eval(r#"tw.replace("the colour", tw.pairs("colour => color"))"#),
        "the color"
    );
}

// ── 插件改了内建函数 ─────────────────────────────────────────────

#[test]
fn tw_keeps_working_after_the_plugin_tampers_with_built_ins() {
    // 逐段模式的插件：tw.streamReplace 走扣住文字的那条路
    let p = load(
        r#"export const manifest = { name: "t", api: 1, permissions: ["system", "reply.text"], reply: "stream" };
           export const { onReplyText, onReplyTextEnd } = tw.streamReplace([]);
           export function onRequest(req) {
             const stringify = JSON.stringify;
             String.prototype.indexOf = () => { throw new Error("tampered indexOf"); };
             String.prototype.slice = () => "tampered";
             String.prototype.trim = () => "tampered";
             String.prototype.charCodeAt = () => 60;
             RegExp.prototype.exec = () => { throw new Error("tampered exec"); };
             Array.prototype.join = () => "tampered";
             Array.prototype.push = () => { throw new Error("tampered push"); };
             Object.defineProperty(Array.prototype, "0", { set() { throw new Error("tampered setter"); }, configurable: true });
             Object.prototype.get = function () { return "tampered"; };
             Map.prototype.get = () => { throw new Error("tampered map"); };
             Set.prototype.has = () => { throw new Error("tampered set"); };
             Reflect.apply = () => { throw new Error("tampered apply"); };
             Object.defineProperty = () => { throw new Error("tampered defineProperty"); };
             JSON.stringify = () => "tampered";
             const results = {
               replace: tw.replace("a <<TW_SECRET_1>> b1", [["a", "A"], [/b(\d)/, "B$1"]]),
               lines: tw.lines(" x \n y "),
               pairs: tw.pairs("p => q"),
               map: tw.mapStrings({ k: ["v"] }, (s) => s + "!"),
             };
             const h = tw.streamReplace([["ab", "X"]]);
             results.stream = h.onReplyText("xa", {}) + h.onReplyText("b", {}) + h.onReplyTextEnd({});
             tw.appendSystem(req, "added");
             delete Array.prototype[0];
             delete Object.prototype.get;
             req.system += "|" + stringify(results);
           }"#,
    );
    let inv = p.on_request(
        json!({ "format": "anthropic", "model": "m", "system": "s" }),
        ctx(json!({})),
    );
    // JSON.stringify 换掉了也没关系：桥比对、交出结果用的是它自己拿住的那一份
    let system = changed(&inv)["system"].as_str().unwrap().to_string();
    let (head, json) = system.split_once('|').unwrap();
    assert_eq!(head, "s\n\nadded");
    assert_eq!(
        serde_json::from_str::<Value>(json).unwrap(),
        json!({
            "replace": "A <<TW_SECRET_1>> B1",
            "lines": ["x", "y"],
            "pairs": [["p", "q"]],
            "map": { "k": ["v!"] },
            "stream": "xX",
        })
    );
}

// ── streamReplace ────────────────────────────────────────────────

const STREAM_HEAD: &str = r#"export const manifest = { name: "s", api: 1, permissions: ["reply.text"], reply: "stream" };
"#;

fn stream_plugin(body: &str) -> Plugin {
    load(&format!("{STREAM_HEAD}{body}"))
}

/// 一块文字逐段交进去，返回每段交出来的（`None` 记成原样）和块结束时补的
fn feed(r: &mut Reply, lane: u64, pieces: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = pieces
        .iter()
        .map(|p| match r.on_text(lane, p).result {
            Ok(Some(s)) => s,
            Ok(None) => p.to_string(),
            Err(e) => panic!("onReplyText failed: {e:?}"),
        })
        .collect();
    match r.on_text_end(lane).result {
        Ok(Some(s)) => out.push(s),
        Ok(None) => out.push(String::new()),
        Err(e) => panic!("onReplyTextEnd failed: {e:?}"),
    }
    out
}

#[test]
fn stream_replace_with_strings_holds_only_a_possible_start() {
    let p = stream_plugin(
        r#"export const { onReplyText, onReplyTextEnd } = tw.streamReplace([["widget", "gadget"], ["cat", "dog"]]);"#,
    );
    let mut r = p.reply(ctx(json!({}))).unwrap();
    // 「wid」可能是 widget 的开头：先扣着；「ca」同理；别的马上放出
    assert_eq!(
        feed(&mut r, 0, &["a wid", "get and a ca", "t; done", "!"]),
        ["a ", "gadget and a ", "dog; done", "!", ""]
    );
    // 扣着的到块结束放出
    assert_eq!(feed(&mut r, 0, &["the wid"]), ["the ", "wid"]);
}

#[test]
fn stream_replace_with_a_regex_holds_max_match_characters() {
    let p = stream_plugin(
        r#"export const { onReplyText, onReplyTextEnd } = tw.streamReplace([[/\d{3,}/, '#']], { maxMatch: 4 });"#,
    );
    let mut r = p.reply(ctx(json!({}))).unwrap();
    assert_eq!(
        feed(&mut r, 0, &["abcdefgh", "12", "34 x"]),
        ["abcd", "ef", "gh#", " x"]
    );
}

#[test]
fn stream_replace_options_are_checked() {
    for (opts, msg) in [
        (
            "{ maxMatch: 0 }",
            "maxMatch must be a positive integer, not 0",
        ),
        (
            "{ maxMatch: 2.5 }",
            "maxMatch must be a positive integer, not 2.5",
        ),
        (
            "{ maxMatch: '9' }",
            "maxMatch must be a positive integer, not a string",
        ),
        ("5", "the options must be an object, not a number"),
    ] {
        let e = load_err(&format!(
            "{STREAM_HEAD}export const {{ onReplyText, onReplyTextEnd }} = tw.streamReplace([['a', 'b']], {opts});"
        ));
        assert!(
            e.to_string().contains(&format!("tw.streamReplace: {msg}")),
            "{opts}: {e}"
        );
    }
    // 规则写错了：加载时就报
    let e = load_err(&format!(
        "{STREAM_HEAD}export const {{ onReplyText, onReplyTextEnd }} = tw.streamReplace([['', 'b']]);"
    ));
    assert!(
        e.to_string()
            .contains("tw.streamReplace: rule 1 looks for an empty string"),
        "{e}"
    );
}

#[test]
fn stream_replace_reads_rules_once_per_block() {
    let p = stream_plugin(
        r#"export const { onReplyText, onReplyTextEnd } = tw.streamReplace((ctx) => {
             console.log("rules");
             return tw.pairs(ctx.settings.terms);
           });"#,
    );
    let mut r = p
        .reply(ctx(json!({ "terms": "colour => color\nfavour => favor" })))
        .unwrap();
    let first = r.on_text(0, "my favourite col");
    assert_eq!(first.logs.len(), 1);
    let second = r.on_text(0, "our");
    assert!(second.logs.is_empty());
    let end = r.on_text_end(0);
    assert_eq!(
        format!(
            "{}{}{}",
            first.result.unwrap().unwrap(),
            second.result.unwrap().unwrap_or_default(),
            end.result.unwrap().unwrap_or_default()
        ),
        "my favorite color"
    );
    // 下一块再读一次
    assert_eq!(r.on_text(0, "x").logs.len(), 1);
    // 一个字都没收到的块结束：什么都不补，也不读规则
    let mut fresh = p.reply(ctx(json!({ "terms": "a => b" }))).unwrap();
    let end = fresh.on_text_end(3);
    assert_eq!(end.result, Ok(Some(String::new())));
    assert!(end.logs.is_empty());
}

#[test]
fn stream_replace_never_splits_a_placeholder() {
    let p = stream_plugin(
        r#"export const { onReplyText, onReplyTextEnd } = tw.streamReplace([["SECRET", "x"], ["<<", "«"], ["key", "KEY"]]);"#,
    );
    let mut r = p.reply(ctx(json!({}))).unwrap();
    let text = "my key <<TW_SECRET_12>> ok << and <<TW_ab";
    let pieces: Vec<String> = text.chars().map(String::from).collect();
    let pieces: Vec<&str> = pieces.iter().map(String::as_str).collect();
    let out = feed(&mut r, 0, &pieces);
    assert_eq!(out.concat(), "my KEY <<TW_SECRET_12>> ok « and «TW_ab");
    // 占位符整个在某一段里
    assert!(
        out.iter().any(|o| o.contains("<<TW_SECRET_12>>")),
        "{out:?}"
    );
}

#[test]
fn stream_replace_keeps_interleaved_blocks_apart() {
    let p = stream_plugin(
        r#"export const { onReplyText, onReplyTextEnd } = tw.streamReplace([["hello", "HI"], [/w\w{3}d/, "W"]], { maxMatch: 8 });"#,
    );
    let mut r = p.reply(ctx(json!({}))).unwrap();
    let big = 1u64 << 40;
    let mut out: std::collections::BTreeMap<u64, String> = Default::default();
    let mut push = |lane: u64, inv: Invocation<Option<String>>, given: &str| {
        let s = match inv.result {
            Ok(Some(s)) => s,
            Ok(None) => given.to_string(),
            Err(e) => panic!("{e:?}"),
        };
        out.entry(lane).or_default().push_str(&s);
    };
    // 两块交替送段：每块各自扣着自己的尾巴
    for (lane, piece) in [
        (0, "say hel"),
        (big, "hello wo"),
        (0, "lo wor"),
        (big, "rld, hel"),
        (0, "ld"),
        (big, "l"),
    ] {
        let inv = r.on_text(lane, piece);
        push(lane, inv, piece);
    }
    let inv = r.on_text_end(big);
    push(big, inv, "");
    let inv = r.on_text_end(0);
    push(0, inv, "");
    assert_eq!(out[&0], "say HI W");
    assert_eq!(out[&big], "HI W, hell");
}

#[test]
fn stream_replace_needs_its_end_hook_in_stream_mode() {
    let p = stream_plugin(r#"export const { onReplyText } = tw.streamReplace([["a", "b"]]);"#);
    let mut r = p.reply(ctx(json!({}))).unwrap();
    match r.on_text(0, "abc").result {
        Err(RunError::Threw { message, .. }) => assert_eq!(
            message,
            "Error: tw.streamReplace: export its onReplyTextEnd as well; without it the text held back at the end of a block is lost"
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn stream_replace_in_block_mode_replaces_each_whole_block() {
    let p = load(
        r#"export const manifest = { name: "b", api: 1, permissions: ["reply.text"] };
           export const { onReplyText } = tw.streamReplace([["widget", "gadget"], [/\d+/, '#']]);"#,
    );
    let mut r = p.reply(ctx(json!({}))).unwrap();
    assert_eq!(
        r.on_text(0, "a widget for 12345").result,
        Ok(Some("a gadget for #".into()))
    );
    assert_eq!(r.on_text(1, "nothing").result, Ok(None));
}

// ── 切分性质 ─────────────────────────────────────────────────────

/// 随机文字、随机规则：参照实现 == 整段 tw.replace == 任意切分后的逐段结果；逐段交出的
/// 每一段都不切开占位符和代理对。见语料 `helpers-split.js`
#[test]
fn stream_replace_matches_whole_replace_under_any_split() {
    let p = common::load_roomy("helpers-split");
    for seed in [1u32, 0x2545_f491, 0x9e37_79b9] {
        let inv = p.on_request(
            json!({ "format": "anthropic", "model": "m", "system": "" }),
            ctx(json!({ "seed": seed, "cases": 1500, "splits": 4 })),
        );
        let v = changed(&inv);
        let report: Value = serde_json::from_str(v["system"].as_str().unwrap()).unwrap();
        assert_eq!(
            report["failures"],
            json!([]),
            "seed {seed}: {}",
            serde_json::to_string_pretty(&report["failures"]).unwrap()
        );
        assert!(report["checked"].as_u64().unwrap() >= 5000, "{report}");
    }
}

/// 多块交错：两个 lane 交替送段（随机切开），每块的结果和整块模式（一次交一整块）的
/// 结果一样 —— 各块的缓冲不串
#[test]
fn interleaved_blocks_come_out_as_if_each_were_sent_alone() {
    const RULES: &str = r##"[["widget", "gadget"], ["<<", "«"], ["SECRET", "x"], [/(?<=a)b/, "B"], [/c$/m, "C"], [/\d{2,4}/, "#"], [/😀/u, ":)"]]"##;
    let stream = stream_plugin(&format!(
        "export const {{ onReplyText, onReplyTextEnd }} = tw.streamReplace({RULES}, {{ maxMatch: 6 }});"
    ));
    let block = load(&format!(
        r#"export const manifest = {{ name: "b", api: 1, permissions: ["reply.text"] }};
           export const {{ onReplyText }} = tw.streamReplace({RULES});"#
    ));
    let words = [
        "a",
        "b",
        "c",
        "ab",
        "\n",
        " ",
        "1",
        "23",
        "wid",
        "get",
        "widget",
        "<",
        "<<",
        "😀",
        "<<TW_SECRET_1>>",
        "<<TW_SEC",
        "RET",
    ];
    let mut seed = 0x1234_5678u32;
    let mut rand = move |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed as usize) % n
    };
    let lanes = [0u64, 7, 1 << 40];
    for case in 0..200 {
        let mut r = stream.reply(ctx(json!({}))).unwrap();
        let mut blocks = block.reply(ctx(json!({}))).unwrap();
        // 每块一段文字，切成几段
        let texts: Vec<String> = lanes
            .iter()
            .map(|_| (0..rand(25)).map(|_| words[rand(words.len())]).collect())
            .collect();
        let mut queues: Vec<Vec<String>> = texts
            .iter()
            .map(|t| {
                let chars: Vec<char> = t.chars().collect();
                let mut pieces = Vec::new();
                let mut cur = String::new();
                for c in chars {
                    cur.push(c);
                    if rand(3) == 0 {
                        pieces.push(std::mem::take(&mut cur));
                    }
                }
                pieces.push(cur);
                pieces.reverse();
                pieces
            })
            .collect();
        let mut got = vec![String::new(); lanes.len()];
        // 随机挑一块送下一段，送完的块随机时候结束
        while queues.iter().any(|q| !q.is_empty()) {
            let i = rand(lanes.len());
            let Some(piece) = queues[i].pop() else {
                continue;
            };
            match r.on_text(lanes[i], &piece).result {
                Ok(Some(s)) => got[i].push_str(&s),
                Ok(None) => got[i].push_str(&piece),
                Err(e) => panic!("case {case}: {e:?}"),
            }
            if queues[i].is_empty() {
                got[i].push_str(&r.on_text_end(lanes[i]).result.unwrap().unwrap_or_default());
            }
        }
        for (i, t) in texts.iter().enumerate() {
            let want = match blocks.on_text(lanes[i], t).result {
                Ok(Some(s)) => s,
                Ok(None) => t.clone(),
                Err(e) => panic!("case {case}: {e:?}"),
            };
            assert_eq!(got[i], want, "case {case}, lane {}: {t:?}", lanes[i]);
        }
    }
}
