//! 第一层（企业版依赖的那几个 crate）只准依赖彼此。
//!
//! 企业版钉 core 的 tag、只声明第一层。第一层里谁依赖了一个第二层的 crate，
//! 企业版就会连带编进桌面网关的东西 —— tw-guard 为了一个打码函数把 tw-secret
//! 带进企业版，就是这么发生的。名单和根 `Cargo.toml` 里的「第一层」一致。

use std::process::Command;

use serde_json::Value;

const LAYER_ONE: &[&str] = &["tw-dialect", "tw-guard", "tw-breaker"];

#[test]
fn layer_one_depends_only_on_itself() {
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

    // 工作区里的每一个成员都是 core 自己的 crate
    let ours: Vec<&str> = packages.iter().filter_map(|p| p["name"].as_str()).collect();
    for name in LAYER_ONE {
        assert!(
            ours.contains(name),
            "{name} is no longer a workspace member"
        );
    }

    let mut wrong = Vec::new();
    for p in packages {
        let name = p["name"].as_str().unwrap_or_default();
        if !LAYER_ONE.contains(&name) {
            continue;
        }
        for d in p["dependencies"].as_array().into_iter().flatten() {
            let dep = d["name"].as_str().unwrap_or_default();
            if ours.contains(&dep) && !LAYER_ONE.contains(&dep) {
                wrong.push(format!("{name} → {dep}"));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "layer one reaches into layer two: {wrong:?}"
    );
}
