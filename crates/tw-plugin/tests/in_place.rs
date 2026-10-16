//! 修改传入的对象即生效：`onRequest`、`onToolCall` 不返回（`undefined`）时，以钩子结束时
//! 那个对象的样子为准。
//!
//! 桥把它序列化，和宿主给的原文比：一字不差就是没改，不交给宿主；不一样就当钩子返回了
//! 它，交给宿主照常解析、核对（改了又改回、只是键的先后变了，宿主照样判成没改）。返回
//! 值的规矩照旧：返回对象就以返回的为准，`null`、数组、`reject()` 都不变。

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

fn ctx() -> Value {
    json!({ "client": null, "model": "m", "format": "anthropic", "upstream": "u", "settings": {} })
}

fn view() -> Value {
    json!({
        "format": "anthropic",
        "model": "m",
        "system": "be brief",
        "messages": [
            { "key": "m0", "role": "user", "parts": [ { "key": "p0", "type": "text", "text": "hi" } ] }
        ],
        // 1.0 进出 JS 变成 1、大整数丢精度：序列化出来和原文不一样，但不算改动
        "params": { "model": "m", "max_tokens": 1024, "temperature": 1.0, "seed": 12345678901234567890u64 }
    })
}

fn request(body: &str) -> Invocation<RequestOutcome> {
    load(&format!(
        r#"export const manifest = {{ name: "t", api: 1, permissions: ["system", "messages", "params"] }};
           export async function onRequest(req, ctx) {{ {body} }}"#
    ))
    .on_request(view(), ctx())
}

fn changed(inv: &Invocation<RequestOutcome>) -> Value {
    match &inv.result {
        Ok(RequestOutcome::Changed(v)) => v.clone(),
        other => panic!("expected Changed, got {other:?} (logs {:?})", inv.logs),
    }
}

#[test]
fn an_edited_request_counts_without_a_return() {
    let v = changed(&request(
        "req.system += ' and kind'; req.params.max_tokens = 8;",
    ));
    assert_eq!(v["system"], "be brief and kind");
    assert_eq!(v["params"]["max_tokens"], 8);
    // 别的照原样（数字按 JS 的眼光不变）
    assert_eq!(v["messages"], view()["messages"]);
}

#[test]
fn an_untouched_or_restored_request_is_unchanged() {
    for body in [
        "",
        "return undefined;",
        "const s = req.system; req.system = 'other'; req.system = s;",
        // 键的先后变了：不算改
        "const s = req.system; delete req.system; req.system = s;",
        "req.messages.push({ role: 'user', parts: [] }); req.messages.pop();",
        // 读一读、在别处造一份改过的：传入的那份没动
        "const copy = JSON.parse(JSON.stringify(req)); copy.system = 'x';",
    ] {
        assert_eq!(
            request(body).result,
            Ok(RequestOutcome::Unchanged),
            "{body}"
        );
    }
}

#[test]
fn a_returned_object_still_wins() {
    // 返回传入的那一个：和以前一样
    assert_eq!(
        changed(&request("req.system = 'x'; return req;"))["system"],
        "x"
    );
    // 返回另一个对象：以返回的为准，传入的那一份改了也不算
    let v = changed(&request(
        "const other = JSON.parse(JSON.stringify(req)); other.system = 'returned'; req.system = 'ignored'; return other;",
    ));
    assert_eq!(v["system"], "returned");
    // 返回原样的另一份：没改
    assert_eq!(
        request("req.system = 'ignored'; const o = JSON.parse(JSON.stringify(req)); o.system = 'be brief'; return o;").result,
        Ok(RequestOutcome::Unchanged)
    );
}

#[test]
fn async_hooks_count_the_object_as_it_is_when_they_settle() {
    let v = changed(&request(
        "await null; req.system = 'later'; await Promise.resolve(); req.params.max_tokens = 1;",
    ));
    assert_eq!(v["system"], "later");
    assert_eq!(v["params"]["max_tokens"], 1);
    // 先改、await 之后改回
    assert_eq!(
        request("req.system = 'x'; await null; req.system = 'be brief';").result,
        Ok(RequestOutcome::Unchanged)
    );
}

#[test]
fn reject_and_throw_beat_an_edit() {
    assert_eq!(
        request("req.system = 'x'; reject('no');").result,
        Ok(RequestOutcome::Rejected("no".into()))
    );
    assert_eq!(
        request("req.system = 'x'; await null; reject('later');").result,
        Ok(RequestOutcome::Rejected("later".into()))
    );
    match request("req.system = 'x'; throw new Error('boom');").result {
        Err(RunError::Threw { message, .. }) => assert_eq!(message, "Error: boom"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_edit_that_cannot_be_written_as_json_is_an_error() {
    match request("req.params.big = 1n;").result {
        Err(RunError::BadOutput(m)) => assert!(
            m.starts_with(
                "onRequest changed its argument into something that cannot be turned into JSON"
            ),
            "{m}"
        ),
        other => panic!("{other:?}"),
    }
    match request("req.self = req;").result {
        Err(RunError::BadOutput(m)) => assert!(m.contains("cannot be turned into JSON"), "{m}"),
        other => panic!("{other:?}"),
    }
}

// ── onToolCall ───────────────────────────────────────────────────

fn tool_plugin() -> Plugin {
    load(
        r#"export const manifest = { name: "t", api: 1, permissions: ["reply.tool_calls"] };
           export async function onToolCall(call) {
             switch (call.name) {
               case "edit": call.input.path = "C:/x"; return;
               case "same": return;
               case "restore": { const p = call.input.path; call.input.path = "y"; call.input.path = p; return; }
               case "reorder": { const i = call.input; delete call.input; call.input = i; return; }
               case "later": await null; call.input.path = "later"; return;
               case "returned": call.input.path = "ignored"; return { name: "returned", input: {} };
               case "self": call.input.path = "self"; return call;
               case "drop": call.input.path = "ignored"; return null;
               case "split": call.input.path = "ignored"; return [{ name: "a", input: {} }];
               case "extra": call.extra = 1; return;
               case "bigint": call.input.n = 1n; return;
               case "noname": call.name = ""; return;
             }
           }"#,
    )
}

fn call(name: &str) -> Value {
    json!({ "id": "c1", "name": name, "input": { "path": "/mnt/c/x", "n": 1.0 } })
}

#[test]
fn an_edited_tool_call_counts_without_a_return() {
    let p = tool_plugin();
    let mut r = p.reply(ctx()).unwrap();
    assert_eq!(
        r.on_tool_call(call("edit")).result,
        Ok(ToolCallOutcome::Replace(vec![
            json!({ "id": "c1", "name": "edit", "input": { "path": "C:/x", "n": 1 } })
        ]))
    );
    assert_eq!(
        r.on_tool_call(call("later")).result,
        Ok(ToolCallOutcome::Replace(vec![
            json!({ "id": "c1", "name": "later", "input": { "path": "later", "n": 1 } })
        ]))
    );
    // 没有 id 的调用（null）也一样
    assert_eq!(
        r.on_tool_call(json!({ "id": null, "name": "edit", "input": { "path": "a" } }))
            .result,
        Ok(ToolCallOutcome::Replace(vec![
            json!({ "name": "edit", "input": { "path": "C:/x" } })
        ]))
    );
}

#[test]
fn an_untouched_or_restored_tool_call_is_unchanged() {
    let p = tool_plugin();
    let mut r = p.reply(ctx()).unwrap();
    for name in ["same", "restore", "reorder", "unknown"] {
        assert_eq!(
            r.on_tool_call(call(name)).result,
            Ok(ToolCallOutcome::Unchanged),
            "{name}"
        );
    }
}

#[test]
fn what_a_tool_call_hook_returns_still_wins() {
    let p = tool_plugin();
    let mut r = p.reply(ctx()).unwrap();
    assert_eq!(
        r.on_tool_call(call("returned")).result,
        Ok(ToolCallOutcome::Replace(vec![
            json!({ "name": "returned", "input": {} })
        ]))
    );
    assert_eq!(
        r.on_tool_call(call("self")).result,
        Ok(ToolCallOutcome::Replace(vec![
            json!({ "id": "c1", "name": "self", "input": { "path": "self", "n": 1 } })
        ]))
    );
    assert_eq!(
        r.on_tool_call(call("drop")).result,
        Ok(ToolCallOutcome::Drop)
    );
    assert_eq!(
        r.on_tool_call(call("split")).result,
        Ok(ToolCallOutcome::Replace(vec![
            json!({ "name": "a", "input": {} })
        ]))
    );
}

#[test]
fn an_edited_tool_call_is_checked_like_a_returned_one() {
    let p = tool_plugin();
    let mut r = p.reply(ctx()).unwrap();
    for (name, want) in [
        ("extra", "unknown field `extra`"),
        ("bigint", "cannot be turned into JSON"),
        ("noname", "needs a non-empty string `name`"),
    ] {
        match r.on_tool_call(call(name)).result {
            Err(RunError::BadOutput(m)) => assert!(m.contains(want), "{name}: {m}"),
            other => panic!("{name}: {other:?}"),
        }
    }
}

#[test]
fn reply_text_hooks_are_unaffected() {
    // 字符串改不了：不返回就是没改
    let p = load(
        r#"export const manifest = { name: "t", api: 1, permissions: ["reply.text"] };
           export function onReplyText(t) { t = t.toUpperCase(); }"#,
    );
    let mut r = p.reply(ctx()).unwrap();
    assert_eq!(r.on_text(0, "abc").result, Ok(None));
}
