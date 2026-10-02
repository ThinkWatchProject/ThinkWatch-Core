//! 谁能依赖 tw-plugin。
//!
//! 编 tw-plugin 要一个能出 wasm 的 clang（见 build.rs）。桌面端从 git 编 core 的
//! 几个 crate（tw-api、tw-types、tw-yaml、tw-guard、tw-watch、tw-link），企业版编
//! 第一层（tw-dialect、tw-guard、tw-breaker、tw-bedrock）—— 它们哪个沾上 tw-plugin，
//! 不管直接还是间接，桌面端和企业版的构建就突然都要装 clang 了。
//!
//! 所以：直接依赖它的只能是 tw-gateway 和 twcore；上面那些 crate 顺着依赖
//! 往下走也碰不到它。

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use serde_json::Value;

const PLUGIN: &str = "tw-plugin";
const MAY_DEPEND: &[&str] = &["tw-gateway", "twcore"];
/// 桌面端（Lite）从 git 编的
const LITE: &[&str] = &[
    "tw-api", "tw-types", "tw-yaml", "tw-guard", "tw-watch", "tw-link",
];
/// 企业版依赖的第一层
const ENTERPRISE: &[&str] = &["tw-dialect", "tw-guard", "tw-breaker", "tw-bedrock"];

/// 工作区里每个 crate 依赖的工作区 crate（各种依赖都算：构建依赖、开发依赖也会
/// 让 `cargo test -p` 那个 crate 时要 clang）
fn workspace_graph() -> BTreeMap<String, BTreeSet<String>> {
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
    let meta: Value = serde_json::from_slice(&out.stdout).expect("cargo metadata is JSON");
    let packages = meta["packages"].as_array().expect("packages");
    let ours: BTreeSet<String> = packages
        .iter()
        .filter_map(|p| p["name"].as_str().map(str::to_string))
        .collect();
    packages
        .iter()
        .map(|p| {
            let name = p["name"].as_str().unwrap_or_default().to_string();
            let deps = p["dependencies"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|d| d["name"].as_str())
                .filter(|d| ours.contains(*d))
                .map(str::to_string)
                .collect();
            (name, deps)
        })
        .collect()
}

fn reaches(
    graph: &BTreeMap<String, BTreeSet<String>>,
    from: &str,
    to: &str,
) -> Option<Vec<String>> {
    // 深度优先，带上路径，报错时说清楚是哪条路
    fn walk(
        graph: &BTreeMap<String, BTreeSet<String>>,
        at: &str,
        to: &str,
        path: &mut Vec<String>,
        seen: &mut BTreeSet<String>,
    ) -> bool {
        if !seen.insert(at.to_string()) {
            return false;
        }
        path.push(at.to_string());
        if at == to {
            return true;
        }
        for next in graph.get(at).into_iter().flatten() {
            if walk(graph, next, to, path, seen) {
                return true;
            }
        }
        path.pop();
        false
    }
    let mut path = Vec::new();
    walk(graph, from, to, &mut path, &mut BTreeSet::new()).then_some(path)
}

#[test]
fn only_the_gateway_and_the_binary_depend_on_tw_plugin() {
    let graph = workspace_graph();
    assert!(
        graph.contains_key(PLUGIN),
        "{PLUGIN} is no longer a workspace member"
    );
    let direct: Vec<&String> = graph
        .iter()
        .filter(|(_, deps)| deps.contains(PLUGIN))
        .map(|(name, _)| name)
        .filter(|name| !MAY_DEPEND.contains(&name.as_str()))
        .collect();
    assert!(
        direct.is_empty(),
        "only {MAY_DEPEND:?} may depend on {PLUGIN}, but these do: {direct:?}"
    );
}

#[test]
fn what_lite_and_enterprise_build_never_reaches_tw_plugin() {
    let graph = workspace_graph();
    for name in LITE.iter().chain(ENTERPRISE) {
        assert!(
            graph.contains_key(*name),
            "{name} is no longer a workspace member"
        );
        if let Some(path) = reaches(&graph, name, PLUGIN) {
            panic!(
                "{name} reaches {PLUGIN} ({}), so building it would need a wasm clang",
                path.join(" → ")
            );
        }
    }
}
