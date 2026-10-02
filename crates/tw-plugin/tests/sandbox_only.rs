//! 插件代码只在 Wasmtime 的沙箱里跑（I1），沙箱只导入桥自己的几个函数（I2）。
//!
//! - I1：core 的任何一个 crate 都不把 JS 引擎编进本机代码。引擎（QuickJS）只在
//!   编成 wasm 的 guest 里，guest 不是工作区成员。
//! - I2：guest 的导入表里只有桥的日志和时钟，没有 WASI 的文件、套接字、环境变量、
//!   命令行参数、进程。导入表就是插件够得着的全部宿主能力：插件的 JS 改不了它。

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

/// 任何一个都不该出现在 core 的本机依赖里
const JS_ENGINES: &[&str] = &[
    "rquickjs",
    "rquickjs-core",
    "rquickjs-sys",
    "quickjs-rs",
    "quickjs-sys",
    "quick-js",
    "libquickjs-sys",
    "boa_engine",
    "boa_runtime",
    "v8",
    "deno_core",
    "javy",
    "mquickjs",
    "rusty_v8",
];

/// 工作区成员和它们声明的依赖。**只读工作区自己的清单**（`--no-deps`）：离线也拿得到，
/// 不用为别的平台的依赖去下载
fn members() -> Value {
    let out = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo metadata");
    assert!(
        out.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("cargo metadata is JSON")
}

/// 工作区 `Cargo.lock` 里的全部包名。锁文件覆盖所有平台、所有种类的依赖（普通、构建、
/// 测试），所以「不在锁文件里」比「不在某个平台的普通依赖里」更强
fn locked_packages() -> BTreeSet<String> {
    let lock = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let text = std::fs::read_to_string(&lock).expect("read Cargo.lock");
    text.lines()
        .filter_map(|l| l.strip_prefix("name = \""))
        .filter_map(|l| l.strip_suffix('"'))
        .map(str::to_string)
        .collect()
}

#[test]
fn no_crate_of_core_compiles_a_javascript_engine_into_native_code() {
    let meta = members();
    let names: Vec<&str> = meta["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"tw-plugin"),
        "tw-plugin is not a workspace member"
    );
    assert!(
        !names.contains(&"tw-plugin-guest"),
        "the guest became a workspace member: its JavaScript engine would be built natively"
    );

    // 引擎只在 guest 自己的锁文件里（它编成 wasm）。工作区的锁文件里出现任何一个，
    // 就是有 crate 把 JS 引擎编进了本机代码 —— 哪怕只是构建或测试时
    let locked = locked_packages();
    assert!(
        locked.contains("wasmtime"),
        "Cargo.lock has no wasmtime: {locked:?}"
    );
    let found: Vec<&&str> = JS_ENGINES.iter().filter(|e| locked.contains(**e)).collect();
    assert!(
        found.is_empty(),
        "a JavaScript engine is in core's own dependency tree: {found:?}"
    );
}

#[test]
fn the_plugin_runtime_does_not_depend_on_the_gateway() {
    // 契约 §5：tw-plugin 是第二层的叶子，网关依赖它，不是反过来
    let meta = members();
    let tw_plugin = meta["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "tw-plugin")
        .expect("tw-plugin");
    for d in tw_plugin["dependencies"].as_array().unwrap() {
        let name = d["name"].as_str().unwrap();
        assert!(
            !matches!(name, "tw-gateway" | "tw-control" | "twcore"),
            "tw-plugin depends on {name}"
        );
    }
}

// ── 沙箱的导入表 ─────────────────────────────────────────────────

/// 沙箱可以从宿主导入的全部函数：桥的日志（`console.*`），rquickjs-sys 垫片里的
/// 时钟（`Date`）。契约还允许 WASI 的时钟和随机数，这一版用不着；其余的 WASI 一个都不行
const ALLOWED_IMPORTS: &[&str] = &[
    "tw.log",
    "env.__rquickjs_host_now_us",
    "wasi_snapshot_preview1.clock_time_get",
    "wasi_snapshot_preview1.random_get",
];

#[test]
fn the_sandbox_imports_only_the_bridge_and_the_clock() {
    let rt = tw_plugin::Runtime::new(tw_plugin::Limits::default()).expect("the runtime starts");
    let imports = rt.sandbox_imports();
    assert!(
        imports.iter().any(|i| i == "tw.log"),
        "the sandbox does not even import the log: {imports:?}"
    );
    let extra: Vec<&String> = imports
        .iter()
        .filter(|i| !ALLOWED_IMPORTS.contains(&i.as_str()))
        .collect();
    assert!(
        extra.is_empty(),
        "the sandbox imports more than the bridge and the clock: {extra:?}"
    );
}
