//! 地址空间受限（`ulimit -v`）的服务器上，起 core 不能因为插件沙箱失败。
//!
//! 每个沙箱实例要预留 4 GiB 出头的地址空间（换来不做边界检查的内存访问）。
//! `Runtime::new` 不建实例，所以它在这种机器上照样成功；真要跑插件时预留不到，
//! 是那一次调用的错误，不是崩溃。
//!
//! 只在 Linux 上有意义：macOS 不执行 RLIMIT_AS。限额要设在一个子进程里，
//! 不能设在跑其他测试的这个进程上。
#![cfg(target_os = "linux")]

use std::process::Command;

const CHILD: &str = "TW_PLUGIN_ADDRESS_SPACE_CHILD";

#[test]
fn startup_survives_a_small_address_space() {
    let out = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "child_with_a_small_address_space",
            "--include-ignored",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .expect("run the child");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("1 passed"), "{text}");
}

#[test]
#[ignore = "only runs as the child of startup_survives_a_small_address_space"]
fn child_with_a_small_address_space() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    // 现在已经用了多少，再加 1 GiB：远不够一个沙箱实例要的 4 GiB
    let status = std::fs::read_to_string("/proc/self/status").expect("/proc/self/status");
    let vm_kib: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmSize:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("VmSize");
    let limit = (vm_kib << 10) + (1 << 30);
    let rl = libc::rlimit {
        rlim_cur: limit,
        rlim_max: limit,
    };
    // SAFETY: 只设这个进程自己的限额
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_AS, &rl) }, 0);

    let rt =
        tw_plugin::Runtime::new(tw_plugin::Limits::default()).expect("startup under RLIMIT_AS");
    let src = r#"export const manifest = { name: "t", api: 1, permissions: ["system"] };
                 export function onRequest() {}"#;
    match rt.load(src.as_bytes()) {
        // 预留不到：一个干净的错误
        Err(tw_plugin::LoadError::Engine(m)) => println!("load failed cleanly: {m}"),
        Ok(_) => println!("load succeeded"),
        Err(other) => panic!("unexpected error: {other:?}"),
    }
}
