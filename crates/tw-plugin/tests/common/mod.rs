//! 对抗用例共用的小工具：一个进程一个运行时，读 `tests/corpus/` 里的插件，
//! 跑一次钩子并量墙钟时间。

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tw_plugin::{
    Invocation, Limits, Plugin, Reply, RequestOutcome, RunError, Runtime, ToolCallOutcome,
};

/// 任何一次调用（包括失败的加载）最多花这么久。上限本身是几百毫秒，这里留足
/// CI 机器上并行跑测试时的余量 —— 要防的是「挂住」，不是「慢了一点」
pub const BOUND: Duration = Duration::from_secs(20);

/// 一个进程一个运行时，和 core 里的用法一样
pub fn rt() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new(Limits::default()).expect("the plugin runtime starts"))
}

pub fn corpus_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/corpus")
        .join(format!("{name}.js"))
}

pub fn corpus(name: &str) -> Vec<u8> {
    std::fs::read(corpus_path(name)).unwrap_or_else(|e| panic!("read corpus/{name}.js: {e}"))
}

/// 加载一个对抗插件。加载本身就失败的那几个用 [`load_err`]
pub fn load(name: &str) -> Plugin {
    let t = Instant::now();
    let p = rt()
        .load(&corpus(name))
        .unwrap_or_else(|e| panic!("corpus/{name}.js failed to load: {e:?}"));
    assert!(t.elapsed() < BOUND, "loading {name} took {:?}", t.elapsed());
    p
}

pub fn load_source(src: &str) -> Plugin {
    rt().load(src.as_bytes())
        .unwrap_or_else(|e| panic!("the plugin failed to load: {e:?}\n{src}"))
}

/// 请求视图：只有 system 一节（对抗插件申请的都是 `system`）
pub fn view() -> Value {
    json!({
        "format": "anthropic",
        "model": "claude-sonnet-4-5",
        "system": "你是助手。",
    })
}

pub fn ctx(settings: Value) -> Value {
    json!({
        "client": "claude-code",
        "model": "claude-sonnet-4-5",
        "format": "anthropic",
        "upstream": null,
        "settings": settings,
    })
}

pub fn reply_ctx(settings: Value) -> Value {
    json!({
        "client": "claude-code",
        "model": "claude-sonnet-4-5",
        "format": "anthropic",
        "upstream": "relay",
        "settings": settings,
    })
}

/// 跑一次请求钩子，墙钟时间必须在 [`BOUND`] 之内
pub fn request(p: &Plugin, settings: Value) -> Invocation<RequestOutcome> {
    let t = Instant::now();
    let inv = p.on_request(view(), ctx(settings));
    assert!(t.elapsed() < BOUND, "onRequest took {:?}", t.elapsed());
    inv
}

pub fn reply(p: &Plugin, settings: Value) -> Reply {
    p.reply(reply_ctx(settings))
        .unwrap_or_else(|e| panic!("instantiating the reply failed: {e:?}"))
}

pub fn text(r: &mut Reply, s: &str) -> Invocation<Option<String>> {
    let t = Instant::now();
    let inv = r.on_text(s);
    assert!(t.elapsed() < BOUND, "onReplyText took {:?}", t.elapsed());
    inv
}

pub fn text_end(r: &mut Reply) -> Invocation<Option<String>> {
    let t = Instant::now();
    let inv = r.on_text_end();
    assert!(t.elapsed() < BOUND, "onReplyTextEnd took {:?}", t.elapsed());
    inv
}

pub fn tool_call(r: &mut Reply, call: Value) -> Invocation<ToolCallOutcome> {
    let t = Instant::now();
    let inv = r.on_tool_call(call);
    assert!(t.elapsed() < BOUND, "onToolCall took {:?}", t.elapsed());
    inv
}

/// 请求钩子必须失败；返回那个错误
pub fn request_err(p: &Plugin, settings: Value) -> RunError {
    match request(p, settings.clone()).result {
        Err(e) => e,
        Ok(o) => panic!("expected a RunError with settings {settings}, got {o:?}"),
    }
}

/// 一个什么都不碰的插件：每次攻击之后跑它，证明运行时本身没被弄坏
pub const BENIGN: &str = r#"
export const manifest = { name: "无害", api: 1, permissions: ["system"] };
export function onRequest(req) {
  req.system = req.system + "（已读）";
  return req;
}
"#;

/// 攻击之后：同一个插件再跑一次，失败的方式和上次一样（没有残留状态）；
/// 一个无害的插件照常工作（运行时没被弄坏）
pub fn after_attack(p: &Plugin, settings: Value, first: &RunError) {
    let again = request_err(p, settings);
    assert_eq!(
        std::mem::discriminant(&again),
        std::mem::discriminant(first),
        "the same attack failed differently the second time: {first:?} then {again:?}"
    );
    still_fine();
}

pub fn still_fine() {
    let p = load_source(BENIGN);
    match request(&p, json!({})).result {
        Ok(RequestOutcome::Changed(v)) => assert_eq!(v["system"], "你是助手。（已读）"),
        other => panic!("a harmless plugin stopped working after an attack: {other:?}"),
    }
}

pub fn system_of(o: &Result<RequestOutcome, RunError>) -> String {
    match o {
        Ok(RequestOutcome::Changed(v)) => v["system"]
            .as_str()
            .unwrap_or_else(|| panic!("no system in {v}"))
            .to_string(),
        other => panic!("expected a changed request, got {other:?}"),
    }
}
