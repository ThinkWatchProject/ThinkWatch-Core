//! 沙箱里有什么、实例之间留下什么（I2、I3）。
//!
//! - 全局只有 ECMAScript 标准内置、`console` 和 `reject`；连网、读文件、拿环境
//!   变量、加载模块的东西一样都没有。
//! - 每次请求钩子都是新实例；一个回答一个实例，回答之间、插件之间什么都不共享。
//! - `ctx` 冻结，改不动。
//! - 时钟是真的，随机数每个实例不同（快照冻住的种子会让每个实例一模一样）。

mod common;

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use common::*;
use serde_json::{Value, json};
use tw_plugin::{RequestOutcome, RunError};

/// ECMAScript 2026 规范里全局对象上的属性（第 19 章，含附录 B 的 escape、unescape），
/// 外加插件的两个：`console`、`reject`。引擎没实现的可以缺，**多出来的一个都不行**
const ALLOWED_GLOBALS: &[&str] = &[
    // 值属性
    "globalThis",
    "Infinity",
    "NaN",
    "undefined",
    // 函数属性
    "eval",
    "isFinite",
    "isNaN",
    "parseFloat",
    "parseInt",
    "decodeURI",
    "decodeURIComponent",
    "encodeURI",
    "encodeURIComponent",
    "escape",
    "unescape",
    // 构造函数
    "AggregateError",
    "Array",
    "ArrayBuffer",
    "AsyncDisposableStack",
    "BigInt",
    "BigInt64Array",
    "BigUint64Array",
    "Boolean",
    "DataView",
    "Date",
    "DisposableStack",
    "Error",
    "EvalError",
    "FinalizationRegistry",
    "Float16Array",
    "Float32Array",
    "Float64Array",
    "Function",
    "Int8Array",
    "Int16Array",
    "Int32Array",
    "Iterator",
    "Map",
    "Number",
    "Object",
    "Promise",
    "Proxy",
    "RangeError",
    "ReferenceError",
    "RegExp",
    "Set",
    "SharedArrayBuffer",
    "String",
    "SuppressedError",
    "Symbol",
    "SyntaxError",
    "TypeError",
    "Uint8Array",
    "Uint8ClampedArray",
    "Uint16Array",
    "Uint32Array",
    "URIError",
    "WeakMap",
    "WeakRef",
    "WeakSet",
    // 其他对象
    "Atomics",
    "JSON",
    "Math",
    "Reflect",
    // 插件的
    "console",
    "reject",
];

#[test]
fn the_global_object_holds_only_standard_built_ins_console_and_reject() {
    let p = load("globals");
    let names: Vec<String> =
        serde_json::from_str(&system_of(&request(&p, json!({})).result)).unwrap();
    let allowed: BTreeSet<&str> = ALLOWED_GLOBALS.iter().copied().collect();
    let extra: Vec<&String> = names
        .iter()
        // 符号键（Symbol.toStringTag 之类）是标准的
        .filter(|n| !n.starts_with("Symbol("))
        .filter(|n| !allowed.contains(n.as_str()))
        .collect();
    assert!(
        extra.is_empty(),
        "globals outside the allowed set: {extra:?}"
    );
    for must in ["console", "reject", "JSON", "Date", "Math", "RegExp"] {
        assert!(
            names.iter().any(|n| n == must),
            "{must} is missing: {names:?}"
        );
    }
}

#[test]
fn nothing_reaches_the_network_the_files_the_environment_or_a_module_loader() {
    let p = load("io-probes");
    let inv = request(&p, json!({}));
    let report: Value = serde_json::from_str(&system_of(&inv.result)).unwrap();
    for (name, ty) in report["found"].as_object().unwrap() {
        assert_eq!(ty, "undefined", "{name} exists in the sandbox ({ty})");
    }
    // 动态 import 可以是一个被拒绝的 Promise，也可以直接抛错；**不能**加载成功
    let dynamic = report["dynamicImport"].as_str().unwrap();
    assert!(
        dynamic == "promise" || dynamic.starts_with("threw"),
        "{dynamic}"
    );
    assert!(
        !inv.logs
            .iter()
            .any(|l| l.text.contains("dynamic import resolved")),
        "import(\"os\") resolved: {:?}",
        inv.logs
    );
    // new Function 是标准的；它看到的全局和插件一样
    assert_eq!(report["functionCtor"], "undefined", "{report}");
}

#[test]
fn every_request_hook_call_starts_from_the_same_state() {
    // 模块变量、全局变量、内置原型上的标记：三样都不会留到下一次请求
    let p = load("state-request");
    for _ in 0..3 {
        assert_eq!(system_of(&request(&p, json!({})).result), "[1,1,1]");
    }
}

#[test]
fn a_reply_shares_one_instance_and_the_next_reply_starts_afresh() {
    let p = load("state-reply");
    let mut r = reply(&p, json!({}));
    for want in ["1", "2", "3"] {
        assert_eq!(text(&mut r, "x").result.unwrap().as_deref(), Some(want));
    }
    let mut next = reply(&p, json!({}));
    assert_eq!(
        text(&mut next, "x").result.unwrap().as_deref(),
        Some("1"),
        "the second reply saw the first one's state"
    );
}

#[test]
fn two_replies_at_once_do_not_see_each_other() {
    let p = load("state-reply");
    let mut a = reply(&p, json!({}));
    let mut b = reply(&p, json!({}));
    assert_eq!(text(&mut a, "x").result.unwrap().as_deref(), Some("1"));
    assert_eq!(text(&mut a, "x").result.unwrap().as_deref(), Some("2"));
    assert_eq!(text(&mut b, "x").result.unwrap().as_deref(), Some("1"));
    assert_eq!(text(&mut a, "x").result.unwrap().as_deref(), Some("3"));
}

#[test]
fn plugins_share_nothing_with_each_other() {
    let a = load("cross-plugin-a");
    let b = load("cross-plugin-b");
    assert_eq!(system_of(&request(&a, json!({})).result), "A");
    assert_eq!(
        system_of(&request(&b, json!({})).result),
        r#"["undefined","undefined"]"#
    );
}

#[test]
fn ctx_and_its_settings_are_frozen() {
    let p = load("ctx-mutation");
    let settings = json!({ "note": "原值" });
    for _ in 0..2 {
        let report: Value =
            serde_json::from_str(&system_of(&request(&p, settings.clone()).result)).unwrap();
        assert_eq!(report["frozen"], true, "{report}");
        assert_eq!(report["settingsFrozen"], true, "{report}");
        for (attempt, worked) in report.as_object().unwrap() {
            if attempt == "frozen" || attempt == "settingsFrozen" {
                continue;
            }
            assert_eq!(worked, false, "{attempt} changed ctx: {report}");
        }
    }
}

#[test]
fn tampering_with_built_ins_affects_nothing_but_the_plugin_itself() {
    // 插件改掉 JSON.stringify、Object.prototype.toJSON 之后，宿主拿到的要么是一个
    // 错误，要么是一个合法的值（交给网关核对）；别的插件和下一次调用不受影响
    let p = load("tamper-builtins");
    for _ in 0..2 {
        match request(&p, json!({})).result {
            Ok(RequestOutcome::Changed(v)) => assert!(v.is_object(), "{v}"),
            Ok(RequestOutcome::Unchanged) | Err(_) => {}
            Ok(RequestOutcome::Rejected(r)) => panic!("tampering turned into a rejection: {r}"),
        }
    }
    still_fine();
    let b = load("cross-plugin-b");
    assert_eq!(
        system_of(&request(&b, json!({})).result),
        r#"["undefined","undefined"]"#
    );
}

#[test]
fn the_clock_is_real_and_random_numbers_differ_between_instances() {
    let p = load("clock-random");
    let first: Value = serde_json::from_str(&system_of(&request(&p, json!({})).result)).unwrap();
    let second: Value = serde_json::from_str(&system_of(&request(&p, json!({})).result)).unwrap();
    let host_now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64;
    let plugin_now = first["now"].as_f64().unwrap();
    assert!(
        (host_now - plugin_now).abs() < 5.0 * 60.0 * 1000.0,
        "Date.now() in the sandbox is {plugin_now}, the host's is {host_now}"
    );
    let r1 = &first["random"];
    let r2 = &second["random"];
    assert_ne!(r1[0], r1[1], "Math.random() repeats within one call: {r1}");
    assert_ne!(
        r1, r2,
        "every instance draws the same random numbers (a seed frozen in the snapshot): {r1}"
    );
}

#[test]
fn reject_is_refused_outside_on_request() {
    let p = load("reject-in-reply");
    let mut r = reply(&p, json!({}));
    let e = text(&mut r, "一段回答")
        .result
        .expect_err("reject() worked in a reply hook");
    assert!(
        matches!(e, RunError::Threw { .. } | RunError::BadOutput(_)),
        "{e:?}"
    );
}

#[test]
fn reject_cannot_be_turned_against_the_host() {
    let p = load("reject-misuse");
    // 超长的理由被截短
    match request(&p, json!({ "kind": "huge" })).result {
        Ok(RequestOutcome::Rejected(why)) => {
            assert!(
                why.len() <= 64 * 1024,
                "a {}-byte reason was kept",
                why.len()
            )
        }
        Err(_) => {}
        other => panic!("{other:?}"),
    }
    // toString 是死循环的理由：在上限之内结束
    let inv = request(&p, json!({ "kind": "tostring-loop" }));
    assert!(
        matches!(
            inv.result,
            Ok(RequestOutcome::Rejected(_)) | Err(RunError::CpuLimit | RunError::BadOutput(_))
        ),
        "{:?}",
        inv.result
    );
    // 理由不是字符串：拒绝照常成立，或者算坏输出
    let inv = request(&p, json!({ "kind": "not-string" }));
    assert!(
        matches!(
            inv.result,
            Ok(RequestOutcome::Rejected(_)) | Err(RunError::BadOutput(_) | RunError::Threw { .. })
        ),
        "{:?}",
        inv.result
    );
    // 调用了 reject 又把它接住：一旦拒绝就算数，不能再放行
    let inv = request(&p, json!({ "kind": "caught" }));
    assert!(
        matches!(inv.result, Ok(RequestOutcome::Rejected(_))),
        "a caught reject() let the request through: {:?}",
        inv.result
    );
    still_fine();
}
