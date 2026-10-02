//! 加载：插件在加载时就能执行代码（模块顶层、manifest 的 getter），加载发生在
//! 安装前的检查和每次重载配置里 —— 卡住它就卡住了 core 的控制面。加载也是清单
//! 校验和哈希（I9）落地的地方。

mod common;

use std::collections::BTreeSet;
use std::time::Instant;

use common::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use tw_plugin::{LoadError, Permission, RequestKind, RequestOutcome};

fn load_err(src: &[u8]) -> LoadError {
    let t = Instant::now();
    let r = rt().load(src);
    assert!(t.elapsed() < BOUND, "loading took {:?}", t.elapsed());
    match r {
        Err(e) => e,
        Ok(p) => panic!(
            "expected a load error, the plugin loaded as {:?}",
            p.manifest().name
        ),
    }
}

fn load_err_named(name: &str) -> LoadError {
    load_err(&corpus(name))
}

// ── 加载时执行的代码 ─────────────────────────────────────────────

#[test]
fn an_endless_loop_at_the_top_level_fails_the_load_in_bounded_time() {
    load_err_named("load-top-level-loop");
    still_fine();
}

#[test]
fn a_memory_bomb_at_the_top_level_fails_the_load() {
    load_err_named("load-top-level-memory");
    still_fine();
}

#[test]
fn a_manifest_getter_that_never_returns_fails_the_load_in_bounded_time() {
    load_err_named("load-manifest-getter");
    still_fine();
}

#[test]
fn a_manifest_that_changes_between_reads_cannot_gain_permissions() {
    // 宿主只读一次清单、按那一次为准；或者干脆拒绝加载
    match rt().load(&corpus("load-manifest-proxy")) {
        Err(_) => {}
        Ok(p) => {
            let got: BTreeSet<Permission> = p.manifest().permissions.clone();
            assert_eq!(
                got,
                BTreeSet::from([Permission::System]),
                "a Proxy manifest gained permissions"
            );
        }
    }
}

#[test]
fn reject_at_the_top_level_does_not_turn_into_rejected_requests() {
    match rt().load(&corpus("load-top-level-reject")) {
        Err(_) => {}
        Ok(p) => {
            let o = request(&p, json!({})).result;
            assert!(
                !matches!(o, Ok(RequestOutcome::Rejected(_))),
                "a reject() at load time refused a request: {o:?}"
            );
        }
    }
}

#[test]
fn modules_cannot_be_imported() {
    // QuickJS 的 std、os，和插件文件旁边的文件，都解析不出来
    load_err_named("import-static");
    load_err_named("import-relative");
}

// ── 清单校验 ────────────────────────────────────────────────────

fn plugin(manifest: &str, hooks: &str) -> String {
    format!("export const manifest = {manifest};\n{hooks}\n")
}

const ON_REQUEST: &str = "export function onRequest(req) { return req; }";
const ON_TEXT: &str = "export function onReplyText(t) { return t; }";

#[test]
fn a_valid_manifest_loads() {
    let p = load_source(&plugin(
        r#"{ name: "合法", api: 1, permissions: ["system", "params"] }"#,
        ON_REQUEST,
    ));
    assert_eq!(p.manifest().name, "合法");
    assert_eq!(
        p.manifest().permissions,
        BTreeSet::from([Permission::System, Permission::Params])
    );
}

#[test]
fn manifests_that_break_the_rules_are_load_errors() {
    let cases: &[(&str, String)] = &[
        ("no manifest", ON_REQUEST.to_string()),
        (
            "no hooks",
            plugin(r#"{ name: "x", api: 1, permissions: ["system"] }"#, ""),
        ),
        (
            "a hook without its permission",
            plugin(r#"{ name: "x", api: 1, permissions: ["system"] }"#, ON_TEXT),
        ),
        (
            "a permission without a hook",
            plugin(
                r#"{ name: "x", api: 1, permissions: ["system", "reply.text"] }"#,
                ON_REQUEST,
            ),
        ),
        (
            "no permissions",
            plugin(r#"{ name: "x", api: 1, permissions: [] }"#, ON_REQUEST),
        ),
        (
            "an unknown permission",
            plugin(
                r#"{ name: "x", api: 1, permissions: ["system", "network"] }"#,
                ON_REQUEST,
            ),
        ),
        (
            "an empty name",
            plugin(
                r#"{ name: "", api: 1, permissions: ["system"] }"#,
                ON_REQUEST,
            ),
        ),
        (
            "a name of 65 characters",
            plugin(
                &format!(
                    r#"{{ name: "{}", api: 1, permissions: ["system"] }}"#,
                    "名".repeat(65)
                ),
                ON_REQUEST,
            ),
        ),
        (
            "a description of 501 characters",
            plugin(
                &format!(
                    r#"{{ name: "x", api: 1, description: "{}", permissions: ["system"] }}"#,
                    "述".repeat(501)
                ),
                ON_REQUEST,
            ),
        ),
        (
            "21 settings",
            plugin(
                &format!(
                    r#"{{ name: "x", api: 1, permissions: ["system"], settings: {{ {} }} }}"#,
                    (0..21)
                        .map(|i| format!(r#"s{i}: {{ type: "string", label: "s", default: "" }}"#))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                ON_REQUEST,
            ),
        ),
        (
            "a setting of an unknown type",
            plugin(
                r#"{ name: "x", api: 1, permissions: ["system"], settings: { a: { type: "file", label: "a", default: "" } } }"#,
                ON_REQUEST,
            ),
        ),
        (
            "a default that does not match its type",
            plugin(
                r#"{ name: "x", api: 1, permissions: ["system"], settings: { a: { type: "number", label: "a", default: "八" } } }"#,
                ON_REQUEST,
            ),
        ),
        (
            "an unknown reply mode",
            plugin(
                r#"{ name: "x", api: 1, permissions: ["reply.text"], reply: "batch" }"#,
                ON_TEXT,
            ),
        ),
        (
            "a scope that is not a list of strings",
            plugin(
                r#"{ name: "x", api: 1, permissions: ["system"], match: { models: "claude-*" } }"#,
                ON_REQUEST,
            ),
        ),
        (
            "a manifest that is not an object",
            plugin(r#""system""#, ON_REQUEST),
        ),
        (
            "a hook that is not a function",
            plugin(
                r#"{ name: "x", api: 1, permissions: ["system"] }"#,
                "export const onRequest = 42;",
            ),
        ),
    ];
    for (what, src) in cases {
        let t = Instant::now();
        let r = rt().load(src.as_bytes());
        assert!(t.elapsed() < BOUND);
        assert!(r.is_err(), "{what}: loaded");
    }
}

// ── 处理哪几种请求（`requests`）─────────────────────────────────────

#[test]
fn requests_default_to_conversations_and_can_list_more_kinds() {
    let p = load_source(&plugin(
        r#"{ name: "x", api: 1, permissions: ["messages"] }"#,
        ON_REQUEST,
    ));
    assert_eq!(
        p.manifest().requests,
        BTreeSet::from([RequestKind::Conversation])
    );
    let p = load_source(&plugin(
        r#"{ name: "x", api: 1, permissions: ["messages", "params"],
             requests: ["embeddings", "conversation", "completions"] }"#,
        ON_REQUEST,
    ));
    assert_eq!(p.manifest().requests, BTreeSet::from(RequestKind::ALL));
    // 只处理嵌入的：权限只有嵌入的视图里有的那几节
    let p = load_source(&plugin(
        r#"{ name: "x", api: 1, permissions: ["messages"], requests: ["embeddings"] }"#,
        ON_REQUEST,
    ));
    assert_eq!(
        p.manifest().requests,
        BTreeSet::from([RequestKind::Embeddings])
    );
}

#[test]
fn requests_that_break_the_rules_are_load_errors() {
    let both = format!("{ON_REQUEST}\n{ON_TEXT}");
    let cases: &[(&str, &str, &str)] = &[
        (
            "an empty list",
            r#"{ name: "x", api: 1, permissions: ["messages"], requests: [] }"#,
            ON_REQUEST,
        ),
        (
            "a kind that does not exist",
            r#"{ name: "x", api: 1, permissions: ["messages"], requests: ["images"] }"#,
            ON_REQUEST,
        ),
        (
            "a string instead of a list",
            r#"{ name: "x", api: 1, permissions: ["messages"], requests: "embeddings" }"#,
            ON_REQUEST,
        ),
        (
            "an entry that is not a string",
            r#"{ name: "x", api: 1, permissions: ["messages"], requests: [1] }"#,
            ON_REQUEST,
        ),
        (
            "a kind listed twice",
            r#"{ name: "x", api: 1, permissions: ["messages"], requests: ["embeddings", "embeddings"] }"#,
            ON_REQUEST,
        ),
        // 嵌入的视图里没有系统提示：只要了 `system` 的插件碰不到嵌入请求里的任何东西
        (
            "a kind none of the permissions reaches",
            r#"{ name: "x", api: 1, permissions: ["system"], requests: ["conversation", "embeddings"] }"#,
            ON_REQUEST,
        ),
        // 不处理对话，`system` 就白要了
        (
            "a permission that only applies to conversations, without conversations",
            r#"{ name: "x", api: 1, permissions: ["system", "messages"], requests: ["completions"] }"#,
            ON_REQUEST,
        ),
        // 回答钩子只在对话上跑
        (
            "reply hooks without conversations",
            r#"{ name: "x", api: 1, permissions: ["messages", "reply.text"], requests: ["embeddings"] }"#,
            &both,
        ),
    ];
    for (what, manifest, hooks) in cases {
        match rt().load(plugin(manifest, hooks).as_bytes()) {
            Err(LoadError::Manifest(why)) => assert!(why.contains("requests"), "{what}: {why}"),
            Err(e) => panic!("{what}: {e:?}"),
            Ok(_) => panic!("{what}: loaded"),
        }
    }
}

#[test]
fn an_unsupported_api_version_is_named() {
    let e = load_err(
        plugin(
            r#"{ name: "x", api: 2, permissions: ["system"] }"#,
            ON_REQUEST,
        )
        .as_bytes(),
    );
    assert!(matches!(e, LoadError::UnsupportedApi(2)), "{e:?}");
}

#[test]
fn a_syntax_error_says_where() {
    let e = load_err(b"export const manifest = { name: \"x\", api: 1,\n  permissions: [\"system\"] };\nexport function onRequest(req) { return req +; }\n");
    match e {
        LoadError::Syntax { line, .. } => assert_eq!(line, Some(3), "{e:?}"),
        e => panic!("{e:?}"),
    }
}

#[test]
fn a_file_over_one_mebibyte_is_too_large() {
    let mut src = plugin(
        r#"{ name: "x", api: 1, permissions: ["system"] }"#,
        ON_REQUEST,
    );
    src.push_str("// ");
    src.push_str(&"x".repeat(1024 * 1024));
    src.push('\n');
    assert!(matches!(load_err(src.as_bytes()), LoadError::TooLarge));
}

#[test]
fn bytes_that_are_not_utf8_are_refused() {
    let mut src = plugin(
        r#"{ name: "x", api: 1, permissions: ["system"] }"#,
        ON_REQUEST,
    )
    .into_bytes();
    src.extend_from_slice(b"// \xff\xfe\n");
    load_err(&src);
}

// ── 哈希（I9）─────────────────────────────────────────────────────

#[test]
fn the_hash_is_of_exactly_the_bytes_that_were_loaded() {
    let src = corpus("state-request");
    let p = rt().load(&src).unwrap();
    let want: [u8; 32] = Sha256::digest(&src).into();
    assert_eq!(p.sha256(), want);

    // 差一个字节（注释里的），哈希就不同
    let mut changed = src.clone();
    changed.extend_from_slice(b"// \n");
    let q = rt().load(&changed).unwrap();
    assert_ne!(q.sha256(), p.sha256());
    let want: [u8; 32] = Sha256::digest(&changed).into();
    assert_eq!(q.sha256(), want);
}
