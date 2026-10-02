//! 适配层：真的 JavaScript 插件编出来、跑起来，交回的都是网关那一份类型。
//!
//! 沙箱本身的行为（上限、隔离、清单的每条规则）在 `tw-plugin` 的测试里；这里只看
//! 换过来的东西对不对得上。

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::*;

fn load(src: &str) -> Arc<dyn PluginHost> {
    match Sandbox.load(src.as_bytes()) {
        Ok(h) => h,
        Err(e) => panic!("load failed: {e:?}\n{src}"),
    }
}

fn ctx() -> Value {
    json!({ "client": "claude-code", "model": "claude-sonnet-4-5", "format": "anthropic",
            "upstream": null, "settings": { "note": "Friday" } })
}

fn view() -> Value {
    json!({
        "format": "anthropic",
        "model": "claude-sonnet-4-5",
        "system": "Be brief.",
        "messages": [
            { "key": "m0", "role": "user", "parts": [ { "key": "m0.p0", "type": "text", "text": "hi" } ] }
        ]
    })
}

const BOTH: &str = r#"
export const manifest = {
  name: "Both",
  api: 1,
  description: "adds a note and shouts",
  permissions: ["reply.text", "system", "reply.tool_calls"],
  match: { clients: ["claude-*"], models: [], upstreams: ["anthropic"] },
  reply: "stream",
  settings: {
    note: { type: "string", label: "Note", default: "today" },
    loud: { type: "boolean", label: "Loud", default: true },
  },
};
export function onRequest(req, ctx) {
  console.info("saw", req.system);
  req.system = req.system + " Note: " + ctx.settings.note;
  return req;
}
let held = "";
export function onReplyText(text) {
  held += text;
  return "";
}
export function onReplyTextEnd() {
  const out = held.toUpperCase();
  held = "";
  return out;
}
export function onToolCall(call) {
  if (call.name === "Bash") return null;
  return { ...call, input: { ...call.input, checked: true } };
}
"#;

/// manifest 换成网关那一份：权限按 `Permission::ALL` 排，设置按作者写的先后
#[test]
fn the_manifest_is_carried_over() {
    let h = load(BOTH);
    let m = h.manifest();
    assert_eq!(m.name, "Both");
    assert_eq!(m.api, 1);
    assert_eq!(m.description.as_deref(), Some("adds a note and shouts"));
    assert_eq!(
        m.permissions,
        [
            tw_api::Permission::System,
            tw_api::Permission::ReplyText,
            tw_api::Permission::ReplyToolCalls
        ]
    );
    assert_eq!(m.scope.clients, ["claude-*"]);
    assert!(m.scope.models.is_empty());
    assert_eq!(m.scope.upstreams, ["anthropic"]);
    assert_eq!(m.reply_mode, tw_api::ReplyMode::Stream);
    let keys: Vec<(&str, tw_api::SettingKind)> = m
        .settings
        .iter()
        .map(|s| (s.key.as_str(), s.kind))
        .collect();
    assert_eq!(
        keys,
        [
            ("note", tw_api::SettingKind::String),
            ("loud", tw_api::SettingKind::Boolean)
        ]
    );
    assert_eq!(m.settings[0].label, "Note");
    assert_eq!(m.settings[0].default, json!("today"));
    assert_eq!(
        m.hooks,
        Hooks {
            request: true,
            reply_text: true,
            reply_text_end: true,
            tool_call: true
        }
    );
    // 没写 `requests`：只处理对话
    assert_eq!(m.requests, [tw_api::RequestKind::Conversation]);
    // 哈希的就是交进来的那些字节（不变式 I9）
    let sha: [u8; 32] = Sha256::digest(BOTH.as_bytes()).into();
    assert_eq!(h.sha256(), sha);
}

/// 声明了的几种请求换过来，按 `RequestKind::ALL` 排
#[test]
fn the_kinds_of_request_are_carried_over_in_order() {
    let h = load(
        r#"export const manifest = { name: "Inputs", api: 1, permissions: ["messages"],
                                     requests: ["completions", "embeddings", "conversation"] };
           export function onRequest(req) {}"#,
    );
    assert_eq!(
        h.manifest().requests,
        [
            tw_api::RequestKind::Conversation,
            tw_api::RequestKind::Embeddings,
            tw_api::RequestKind::Completions
        ]
    );
}

#[test]
fn a_request_hook_changes_the_view_and_its_log_comes_along() {
    let h = load(BOTH);
    let inv = h.on_request(view(), ctx());
    let Ok(RequestOutcome::Changed(v)) = &inv.result else {
        panic!("{:?}", inv.result);
    };
    assert_eq!(v["system"], "Be brief. Note: Friday");
    assert_eq!(inv.logs.len(), 1, "{:?}", inv.logs);
    assert_eq!(inv.logs[0].level, tw_api::PluginLogLevel::Info);
    assert_eq!(inv.logs[0].text, "saw Be brief.");
    assert!(inv.cpu > std::time::Duration::ZERO);
}

#[test]
fn a_rejection_and_a_throw_keep_their_meaning() {
    let src = |body: &str| {
        format!(
            "export const manifest = {{ name: \"t\", api: 1, permissions: [\"messages\"] }};\n\
             export function onRequest(req, ctx) {{ {body} }}"
        )
    };
    let no = load(&src("reject(\"not on Fridays\");"));
    assert_eq!(
        no.on_request(view(), ctx()).result,
        Ok(RequestOutcome::Rejected("not on Fridays".into()))
    );
    let same = load(&src("return req;"));
    assert_eq!(
        same.on_request(view(), ctx()).result,
        Ok(RequestOutcome::Unchanged)
    );
    let boom = load(&src(
        "console.error(\"about to fail\"); throw new Error(\"boom\");",
    ));
    let inv = boom.on_request(view(), ctx());
    match &inv.result {
        Err(RunError::Threw { message, .. }) => assert!(message.contains("boom"), "{message}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(inv.logs[0].level, tw_api::PluginLogLevel::Error);
    // 记在运行上的那一句是网关的消息码
    assert_eq!(inv.result.unwrap_err().msg().code, "gw.plugin.threw");
}

#[test]
fn a_reply_instance_keeps_its_state_for_one_answer() {
    let h = load(BOTH);
    let mut r = h.reply(ctx()).unwrap();
    assert_eq!(r.on_text("hel").result, Ok(Some(String::new())));
    assert_eq!(r.on_text("lo").result, Ok(Some(String::new())));
    assert_eq!(r.on_text_end().result, Ok(Some("HELLO".into())));
    // 工具调用：丢掉一个、改一个
    assert_eq!(
        r.on_tool_call(json!({"id": "t1", "name": "Bash", "input": {"command": "ls"}}))
            .result,
        Ok(ToolCallOutcome::Drop)
    );
    let Ok(ToolCallOutcome::Replace(calls)) = r
        .on_tool_call(json!({"id": "t2", "name": "Read", "input": {"path": "a"}}))
        .result
    else {
        panic!("not replaced");
    };
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["input"], json!({"path": "a", "checked": true}));

    // 另一个回答是另一个实例：上一个攒着的不会漏过来
    let mut fresh = h.reply(ctx()).unwrap();
    assert_eq!(fresh.on_text_end().result, Ok(Some(String::new())));
}

#[test]
fn load_errors_keep_their_line_and_their_code() {
    let e = Sandbox
        .load(b"export const manifest = { name: \"t\", api: 1, permissions: [\"system\"] };\nexport function onRequest( {\n")
        .err()
        .expect("a syntax error loaded");
    match &e {
        LoadError::Syntax { line, .. } => assert!(line.is_some(), "{e:?}"),
        other => panic!("{other:?}"),
    }
    assert!(e.msg().code.starts_with("gw.plugin.syntax"), "{e:?}");

    let e = Sandbox
        .load(b"export const manifest = { name: \"t\", api: 1, permissions: [\"system\"] };\n")
        .err()
        .expect("a manifest without its hook loaded");
    assert!(matches!(e, LoadError::Manifest(_)), "{e:?}");
    assert_eq!(e.msg().code, "gw.plugin.manifest");

    let e = Sandbox
        .load(b"export const manifest = { name: \"t\", api: 2, permissions: [\"system\"] };\nexport function onRequest(r) {}\n")
        .err()
        .expect("API 2 loaded");
    assert_eq!(e, LoadError::UnsupportedApi(2));
}

/// 编插件不看调用方的栈有多大：换配置那一路可能在一根小栈的线程上（Windows 的主线程
/// 只有 1 MiB），而模块顶层是在沙箱里真跑的
#[test]
fn a_plugin_compiles_from_a_thread_with_a_small_stack() {
    let src = "export const manifest = { name: \"deep\", api: 1, permissions: [\"system\"] };\n\
               function depth(n) { return n === 0 ? 0 : 1 + depth(n - 1); }\n\
               const d = depth(400);\n\
               export function onRequest(req) { req.system = String(d); return req; }\n";
    let loaded = std::thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(move || {
            Sandbox
                .load(src.as_bytes())
                .map(|h| h.manifest().name.clone())
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(loaded.unwrap(), "deep");
}
