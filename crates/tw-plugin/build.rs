//! 编出插件沙箱：QuickJS-ng → wasm → 快照 → 目标平台的机器码，嵌进二进制。
//!
//! 1. **wasm**：用一个能出 wasm32 的 clang 把 `guest/`（QuickJS-ng 源码来自钉死的
//!    rquickjs-sys）编成 `wasm32-unknown-unknown` 模块。C 由 clang 编，链接用 rustc
//!    自带的 rust-lld —— 所以外部只要 clang 和 llvm-ar 两样。这里**不下载任何
//!    东西**：找不到就报错，告诉人怎么装。
//! 2. **快照**：在构建机上跑一遍 `tw_init(bridge.js)`，把初始化好的整块内存写回
//!    模块的数据段（Wizer 的做法）。之后每个实例都从初始化完的状态起步。
//! 3. **预编译**：用 Cranelift 把快照编成目标平台的 `.cwasm`。交叉编译时就编给
//!    目标平台（`Config::target`），也就不会带上构建机 CPU 的特性。运行时只有
//!    Wasmtime 的运行时部分，没有编译器。
//!
//! 产物只在 `OUT_DIR` 里，仓库里不放任何二进制。用的 clang 版本和 wasm 的
//! SHA-256 记进二进制（`tw_plugin::GUEST_CLANG`、`GUEST_WASM_SHA256`）和
//! `OUT_DIR/guest-build.txt`，发出去的每一版都能对上是哪个工具链编的。

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

#[path = "src/engine.rs"]
mod engine;

/// 沙箱模块唯一允许的导入。多一个都不编：多出来的就是一条通往宿主的路
const ALLOWED_IMPORTS: &[(&str, &str)] = &[("tw", "log"), ("env", "__rquickjs_host_now_us")];

const WASM_TARGET: &str = "wasm32-unknown-unknown";

fn main() {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let target = env::var("TARGET").expect("TARGET");

    for p in [
        "build.rs",
        "src/engine.rs",
        "src/bridge.js",
        "guest/Cargo.toml",
        "guest/Cargo.lock",
        "guest/src",
    ] {
        println!("cargo:rerun-if-changed={p}");
    }
    for v in ["TW_WASM_CLANG", "TW_WASM_AR"] {
        println!("cargo:rerun-if-env-changed={v}");
    }

    let tools = find_tools(&out_dir);
    check_rust_wasm_target();
    let wasm = build_guest(&manifest_dir, &out_dir, &tools);
    check_imports(&wasm);

    let sha = hex(&Sha256::digest(&wasm));
    fs::write(out_dir.join("guest.wasm"), &wasm).expect("write guest.wasm");
    fs::write(
        out_dir.join("guest-build.txt"),
        format!(
            "guest.wasm sha256 {sha}\nguest.wasm bytes {}\nclang {}\nclang path {}\nllvm-ar path {}\n",
            wasm.len(),
            tools.version,
            tools.clang.display(),
            tools.ar.display()
        ),
    )
    .expect("write guest-build.txt");
    println!("cargo:rustc-env=TW_PLUGIN_GUEST_SHA256={sha}");
    println!("cargo:rustc-env=TW_PLUGIN_GUEST_CLANG={}", tools.version);

    let bridge = fs::read(manifest_dir.join("src/bridge.js")).expect("read src/bridge.js");
    let snapshot = snapshot(&wasm, &bridge);
    let cwasm = precompile(&snapshot, &target);
    fs::write(out_dir.join("guest.cwasm"), cwasm).expect("write guest.cwasm");
}

// ── 工具链 ───────────────────────────────────────────────────────

struct Tools {
    clang: PathBuf,
    ar: PathBuf,
    /// `clang --version` 的第一行
    version: String,
}

/// 依次找：显式指定 → Homebrew 的 llvm（macOS）→ PATH 上的 clang / clang-N。
/// 每个候选都真的编一个 wasm32 的目标文件试过才算数：Apple 自带的 clang 就编不了。
fn find_tools(out_dir: &Path) -> Tools {
    let mut tried: Vec<String> = Vec::new();

    if let Some(clang) = env::var_os("TW_WASM_CLANG") {
        let clang = PathBuf::from(clang);
        let ar = match env::var_os("TW_WASM_AR") {
            Some(ar) => PathBuf::from(ar),
            None => ar_for(&clang).unwrap_or_else(|| {
                fail(&format!(
                    "TW_WASM_CLANG is set to {} but no llvm-ar was found next to it or on PATH; \
                     set TW_WASM_AR as well",
                    clang.display()
                ))
            }),
        };
        match probe(&clang, &ar, out_dir) {
            Ok(version) => return Tools { clang, ar, version },
            Err(e) => fail(&format!(
                "TW_WASM_CLANG={} cannot build for wasm32: {e}",
                clang.display()
            )),
        }
    }

    for c in candidates() {
        let Some(ar) = ar_for(&c) else {
            tried.push(format!("{} (no matching llvm-ar)", c.display()));
            continue;
        };
        match probe(&c, &ar, out_dir) {
            Ok(version) => {
                return Tools {
                    clang: c,
                    ar,
                    version,
                };
            }
            Err(e) => tried.push(format!("{}: {e}", c.display())),
        }
    }

    fail(&missing_clang_message(&tried))
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// 候选的 clang，按优先级排好
fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    // Homebrew 的 llvm 是 keg-only，默认不在 PATH 上；macOS 自带的 clang 编不了 wasm
    if cfg!(target_os = "macos") {
        for prefix in ["/opt/homebrew/opt/llvm", "/usr/local/opt/llvm"] {
            let p = Path::new(prefix).join("bin/clang");
            if p.is_file() {
                out.push(p);
            }
        }
        if let Some(prefix) = brew_prefix_llvm() {
            let p = prefix.join("bin/clang");
            if p.is_file() && !out.contains(&p) {
                out.push(p);
            }
        }
    }
    if let Some(p) = which(&exe("clang")) {
        out.push(p);
    }
    // 发行版常见的带版本号的名字（clang-18 配 llvm-ar-18），新的优先
    for n in (13..=30).rev() {
        if let Some(p) = which(&exe(&format!("clang-{n}"))) {
            out.push(p);
        }
    }
    // Windows 上 LLVM 安装包的默认位置（安装时不一定加进 PATH）
    if cfg!(windows) {
        for base in [env::var_os("ProgramFiles"), env::var_os("ProgramW6432")]
            .into_iter()
            .flatten()
        {
            let p = PathBuf::from(base)
                .join("LLVM")
                .join("bin")
                .join("clang.exe");
            if p.is_file() && !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

fn brew_prefix_llvm() -> Option<PathBuf> {
    let out = Command::new("brew")
        .args(["--prefix", "llvm"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

/// 和这个 clang 配套的 llvm-ar：`TW_WASM_AR`，或者它旁边的（顺着符号链接再找
/// 一次），`clang-N` 配 `llvm-ar-N`，最后才是 PATH 上的。BSD 的 `ar` 给 wasm
/// 目标文件建不了符号索引，不用它
fn ar_for(clang: &Path) -> Option<PathBuf> {
    if let Some(ar) = env::var_os("TW_WASM_AR") {
        return Some(PathBuf::from(ar));
    }
    let suffix = clang
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_prefix("clang"))
        .unwrap_or("")
        .to_string();
    let names = if suffix.is_empty() {
        vec![exe("llvm-ar")]
    } else {
        vec![exe(&format!("llvm-ar{suffix}")), exe("llvm-ar")]
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = clang.parent() {
        dirs.push(d.to_path_buf());
    }
    if let Some(d) = fs::canonicalize(clang)
        .ok()
        .and_then(|real| real.parent().map(Path::to_path_buf))
        && !dirs.contains(&d)
    {
        dirs.push(d);
    }
    for d in &dirs {
        for n in &names {
            let p = d.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    names.iter().find_map(|n| which(n))
}

fn which(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// 真的编一个 wasm32 的目标文件、真的打一个包。返回 `clang --version` 的第一行
fn probe(clang: &Path, ar: &Path, out_dir: &Path) -> Result<String, String> {
    let dir = out_dir.join("probe");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let src = dir.join("probe.c");
    let obj = dir.join("probe.o");
    let lib = dir.join("libprobe.a");
    let _ = fs::remove_file(&obj);
    let _ = fs::remove_file(&lib);
    fs::write(&src, "int tw_probe(int x) { return x * 2; }\n").map_err(|e| e.to_string())?;
    let out = Command::new(clang)
        .arg(format!("--target={WASM_TARGET}"))
        .args(["-O2", "-c"])
        .arg(&src)
        .arg("-o")
        .arg(&obj)
        .output()
        .map_err(|e| format!("cannot run it ({e})"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let first = err
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();
        return Err(format!("it cannot target wasm32 ({first})"));
    }
    let head = fs::read(&obj).map_err(|e| e.to_string())?;
    if !head.starts_with(b"\0asm") {
        return Err("its output for wasm32 is not a wasm object".into());
    }
    let out = Command::new(ar)
        .arg("crs")
        .arg(&lib)
        .arg(&obj)
        .output()
        .map_err(|e| format!("cannot run {} ({e})", ar.display()))?;
    if !out.status.success() {
        return Err(format!("{} cannot archive a wasm object", ar.display()));
    }
    let out = Command::new(clang)
        .arg("--version")
        .output()
        .map_err(|e| e.to_string())?;
    let version = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("unknown clang")
        .trim()
        .to_string();
    Ok(version)
}

fn missing_clang_message(tried: &[String]) -> String {
    let how = if cfg!(target_os = "macos") {
        "  macOS:          brew install llvm\n                  (Apple's clang cannot target wasm; Homebrew's is found automatically)"
    } else if cfg!(windows) {
        "  Windows:        install LLVM from https://github.com/llvm/llvm-project/releases\n                  (the LLVM-<version>-win64.exe installer), or `winget install LLVM.LLVM`"
    } else {
        "  Debian/Ubuntu:  sudo apt install clang llvm\n  Fedora:         sudo dnf install clang llvm"
    };
    let mut msg = String::from(
        "Building tw-plugin needs a clang that can compile C to WebAssembly, plus the matching llvm-ar.\n\
         None was found.\n\nInstall one:\n",
    );
    msg.push_str(how);
    msg.push_str(
        "\n\nOr point the build at one explicitly:\n  TW_WASM_CLANG=/path/to/clang  TW_WASM_AR=/path/to/llvm-ar\n",
    );
    if !tried.is_empty() {
        msg.push_str("\nTried:\n");
        for t in tried {
            msg.push_str("  ");
            msg.push_str(t);
            msg.push('\n');
        }
    }
    msg
}

/// rustc 要有 wasm32-unknown-unknown 的标准库（core）。根目录的 rust-toolchain.toml
/// 列了这个目标，rustup 会自己装；不走 rustup 的要手动装
fn check_rust_wasm_target() {
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
    let out = Command::new(&rustc)
        .args(["--print", "target-libdir", "--target", WASM_TARGET])
        .output();
    let ok = match out {
        Ok(o) if o.status.success() => {
            let dir = PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_string());
            fs::read_dir(&dir)
                .map(|rd| {
                    rd.flatten().any(|e| {
                        let n = e.file_name();
                        let n = n.to_string_lossy();
                        n.starts_with("libcore-") && n.ends_with(".rlib")
                    })
                })
                .unwrap_or(false)
        }
        _ => false,
    };
    if !ok {
        fail(
            "Building tw-plugin needs Rust's wasm32-unknown-unknown target.\n\
             Install it with:\n  rustup target add wasm32-unknown-unknown\n\
             (rustup does this by itself for this repository: rust-toolchain.toml lists the target.)",
        );
    }
}

// ── 编 guest ─────────────────────────────────────────────────────

fn build_guest(manifest_dir: &Path, out_dir: &Path, tools: &Tools) -> Vec<u8> {
    let guest = manifest_dir.join("guest");
    let target_dir = out_dir.join("guest-target");
    let cargo_home = env::var_os("CARGO_HOME").map(PathBuf::from).or_else(|| {
        env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .map(|h| PathBuf::from(h).join(".cargo"))
    });

    let mut cmd = nested_cargo();
    cmd.args([
        "build",
        "--release",
        "--locked",
        "--target",
        WASM_TARGET,
        "--manifest-path",
    ])
    .arg(guest.join("Cargo.toml"))
    .arg("--target-dir")
    .arg(&target_dir);

    // 编出来的东西不带构建机的路径：同一套工具链在哪台机器上编都一样
    let mut remap = vec![
        format!("--remap-path-prefix={}=/guest", guest.display()),
        format!("--remap-path-prefix={}=/target", target_dir.display()),
    ];
    if let Some(h) = &cargo_home {
        remap.push(format!("--remap-path-prefix={}=/cargo", h.display()));
    }
    cmd.env("CARGO_ENCODED_RUSTFLAGS", remap.join("\u{1f}"));

    // C 那一半交给探测到的 clang / llvm-ar。断言里的 __FILE__ 固定成一个名字：
    // 否则它是构建目录下的绝对路径
    let mut cflags = vec![
        "-Wno-builtin-macro-redefined".to_string(),
        "-D__FILE__=\"quickjs\"".to_string(),
    ];
    if cfg!(windows) {
        // rquickjs-sys 把它带的 libc 头文件目录 canonicalize 成 `\\?\C:\…` 交给
        // clang。这种写法里 `/` 不算分隔符，于是头文件里的
        // `#include <bits/alltypes.h>` 找不到。同一个目录换普通写法再给一遍没用：
        // clang 认出是同一个目录，把后给的那个去掉了。所以拷一份到 OUT_DIR，
        // 当作另一个目录给它 —— 前一个找不到时就找到这里
        let include = rquickjs_sys_dir(&guest)
            .map(|d| d.join("vendor").join("wasi-libc").join("include"))
            .unwrap_or_else(|e| fail(&format!("cannot locate rquickjs-sys: {e}")));
        let copy = out_dir.join("wasi-libc-include");
        copy_dir(&include, &copy)
            .unwrap_or_else(|e| fail(&format!("cannot copy {}: {e}", include.display())));
        cflags.push("-isystem".into());
        cflags.push(copy.display().to_string());
    }
    let triple = WASM_TARGET.replace('-', "_");
    // 按 shell 的规则拆：路径里可以有空格
    let quoted: Vec<String> = cflags.iter().map(|f| sh_quote(f)).collect();
    cmd.env(format!("CC_{triple}"), &tools.clang)
        .env(format!("AR_{triple}"), &tools.ar)
        .env("CC_SHELL_ESCAPED_FLAGS", "1")
        .env(format!("CFLAGS_{triple}"), quoted.join(" "));

    let status = cmd
        .status()
        .unwrap_or_else(|e| fail(&format!("cannot run cargo: {e}")));
    if !status.success() {
        fail(&format!(
            "building the QuickJS guest for {WASM_TARGET} failed (clang: {})",
            tools.clang.display()
        ));
    }
    let wasm_path = target_dir
        .join(WASM_TARGET)
        .join("release")
        .join("tw_plugin_guest.wasm");
    fs::read(&wasm_path)
        .unwrap_or_else(|e| fail(&format!("cannot read {}: {e}", wasm_path.display())))
}

/// 一个干净的 cargo：外层 cargo 给构建脚本的环境里有它自己的编译选项（CI 的
/// `-D warnings`、clippy 的包装器、用户的 profile 覆盖、给本机用的 C 编译器和
/// 选项），都不该落到这个独立的小工程上
fn nested_cargo() -> Command {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut cmd = Command::new(cargo);
    for (key, _) in env::vars_os() {
        let Some(key) = key.to_str() else { continue };
        let drop = matches!(
            key,
            "RUSTFLAGS"
                | "CARGO_ENCODED_RUSTFLAGS"
                | "CARGO_BUILD_RUSTFLAGS"
                | "RUSTDOCFLAGS"
                | "CARGO_ENCODED_RUSTDOCFLAGS"
                | "RUSTC_WRAPPER"
                | "RUSTC_WORKSPACE_WRAPPER"
                | "CARGO_BUILD_RUSTC_WRAPPER"
                | "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER"
                | "CARGO_TARGET_DIR"
                | "CARGO_BUILD_TARGET_DIR"
                | "CARGO_BUILD_TARGET"
                | "CARGO_INCREMENTAL"
                | "CARGO_BUILD_INCREMENTAL"
                | "CC"
                | "CFLAGS"
                | "AR"
                | "TARGET_CC"
                | "TARGET_CFLAGS"
                | "TARGET_AR"
                | "CC_SHELL_ESCAPED_FLAGS"
        ) || key.starts_with("CARGO_PROFILE_")
            || key.starts_with("CARGO_TARGET_");
        if drop {
            cmd.env_remove(key);
        }
    }
    cmd
}

/// guest 用的那份 rquickjs-sys 在哪（问 cargo，源码可能在注册表缓存里，也可能
/// 是 vendor 出来的）
fn rquickjs_sys_dir(guest: &Path) -> Result<PathBuf, String> {
    let out = nested_cargo()
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--manifest-path",
        ])
        .arg(guest.join("Cargo.toml"))
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    meta["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["name"] == "rquickjs-sys")
        .and_then(|p| p["manifest_path"].as_str())
        .and_then(|m| Path::new(m).parent().map(Path::to_path_buf))
        .ok_or_else(|| "rquickjs-sys is not in the guest's dependency graph".into())
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// 给 cc 的 `CC_SHELL_ESCAPED_FLAGS` 用的单引号括起来的写法
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 导入表必须正好是允许的那几个，内存必须是模块自己的、导出出来的
fn check_imports(wasm: &[u8]) {
    use wasmparser::{Parser, Payload, TypeRef};
    for payload in Parser::new(0).parse_all(wasm) {
        let payload =
            payload.unwrap_or_else(|e| fail(&format!("the guest wasm does not parse: {e}")));
        if let Payload::ImportSection(reader) = payload {
            for import in reader.into_imports() {
                let import = import.unwrap_or_else(|e| fail(&format!("bad import: {e}")));
                let allowed = matches!(import.ty, TypeRef::Func(_))
                    && ALLOWED_IMPORTS
                        .iter()
                        .any(|(m, n)| *m == import.module && *n == import.name);
                if !allowed {
                    fail(&format!(
                        "the guest wasm imports {}.{} ({:?}); only {:?} are allowed",
                        import.module, import.name, import.ty, ALLOWED_IMPORTS
                    ));
                }
            }
        }
    }
}

// ── 快照 ─────────────────────────────────────────────────────────

/// 在构建机上实例化一次、跑 `tw_init(bridge.js)`，把那一刻的整块内存写回数据段
fn snapshot(wasm: &[u8], bridge: &[u8]) -> Vec<u8> {
    use wasmtime::{Engine, Linker, Module, Store};

    let engine = Engine::new(&engine::config()).unwrap_or_else(|e| fail(&format!("wasmtime: {e}")));
    let module =
        Module::new(&engine, wasm).unwrap_or_else(|e| fail(&format!("compile the guest: {e}")));
    let mut linker: Linker<()> = Linker::new(&engine);
    // 初始化时不该有日志；时钟给 0，快照里就不会冻进构建那一刻的时间
    linker
        .func_wrap("tw", "log", |_: u32, _: u32, _: u32| {})
        .and_then(|l| l.func_wrap("env", "__rquickjs_host_now_us", || -> f64 { 0.0 }))
        .unwrap_or_else(|e| fail(&format!("linker: {e}")));
    let mut store = Store::new(&engine, ());
    // 纪元不会推进（没有计时线程），截止时间给多远都行
    store.set_epoch_deadline(u64::MAX / 2);
    let instance = linker
        .instantiate(&mut store, &module)
        .unwrap_or_else(|e| fail(&format!("instantiate the guest: {e}")));
    let memory = instance
        .get_memory(&mut store, "memory")
        .unwrap_or_else(|| fail("the guest exports no memory"));

    let alloc = instance
        .get_typed_func::<u32, u32>(&mut store, "tw_alloc")
        .unwrap_or_else(|e| fail(&format!("tw_alloc: {e}")));
    let init = instance
        .get_typed_func::<(u32, u32), u32>(&mut store, "tw_init")
        .unwrap_or_else(|e| fail(&format!("tw_init: {e}")));
    let out_ptr = instance
        .get_typed_func::<(), u32>(&mut store, "tw_out_ptr")
        .unwrap_or_else(|e| fail(&format!("tw_out_ptr: {e}")));
    let out_len = instance
        .get_typed_func::<(), u32>(&mut store, "tw_out_len")
        .unwrap_or_else(|e| fail(&format!("tw_out_len: {e}")));

    let len = u32::try_from(bridge.len()).expect("bridge.js is small");
    let ptr = alloc
        .call(&mut store, len)
        .unwrap_or_else(|e| fail(&format!("tw_alloc: {e}")));
    let mut src = bridge.to_vec();
    src.push(0);
    memory
        .write(&mut store, ptr as usize, &src)
        .unwrap_or_else(|e| fail(&format!("write bridge.js: {e}")));
    let rc = init
        .call(&mut store, (ptr, len))
        .unwrap_or_else(|e| fail(&format!("tw_init trapped: {e:?}")));
    if rc != 0 {
        let p = out_ptr.call(&mut store, ()).unwrap_or(0) as usize;
        let n = out_len.call(&mut store, ()).unwrap_or(0) as usize;
        let msg = memory
            .data(&store)
            .get(p..p + n)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        fail(&format!("bridge.js failed to initialize: {msg}"));
    }
    let image = memory.data(&store).to_vec();
    rewrite(wasm, &image)
}

/// 把模块的数据段换成 `image`，初始内存页数改成快照时的页数。
///
/// 只对这一类模块成立：唯一可变的全局是影子栈指针，而它在 `tw_init` 返回时已经
/// 回到初值；表在初始化期间没有变；数据段全是主动段。不满足就不编，不猜
fn rewrite(wasm: &[u8], image: &[u8]) -> Vec<u8> {
    use wasm_encoder as we;
    use wasmparser::{DataKind, Parser, Payload};

    const PAGE: usize = 65536;
    let pages = (image.len() / PAGE) as u64;
    let segments = segments(image);

    let mut out = we::Module::new();
    let mut saw_data = false;
    for payload in Parser::new(0).parse_all(wasm) {
        let payload = payload.unwrap_or_else(|e| fail(&format!("parse the guest: {e}")));
        match &payload {
            Payload::Version { .. } | Payload::End(_) => {}
            Payload::MemorySection(reader) => {
                let mut section = we::MemorySection::new();
                let mut n = 0;
                for m in reader.clone() {
                    let m = m.unwrap_or_else(|e| fail(&format!("memory section: {e}")));
                    n += 1;
                    section.memory(we::MemoryType {
                        minimum: pages,
                        maximum: m.maximum,
                        memory64: m.memory64,
                        shared: m.shared,
                        page_size_log2: m.page_size_log2,
                    });
                }
                if n != 1 {
                    fail(&format!("the guest has {n} memories; expected one"));
                }
                out.section(&section);
            }
            Payload::GlobalSection(reader) => {
                let mutable = reader
                    .clone()
                    .into_iter()
                    .filter(|g| g.as_ref().is_ok_and(|g| g.ty.mutable))
                    .count();
                if mutable > 1 {
                    fail(&format!(
                        "the guest has {mutable} mutable globals; the snapshot only knows how to keep the stack pointer"
                    ));
                }
                raw(&mut out, &payload, wasm);
            }
            Payload::StartSection { .. } => {
                fail("the guest has a start function; a snapshot would run it twice")
            }
            Payload::DataCountSection { .. } => {
                out.section(&we::DataCountSection {
                    count: segments.len() as u32,
                });
            }
            Payload::DataSection(reader) => {
                saw_data = true;
                for d in reader.clone() {
                    let d = d.unwrap_or_else(|e| fail(&format!("data section: {e}")));
                    if !matches!(
                        d.kind,
                        DataKind::Active {
                            memory_index: 0,
                            ..
                        }
                    ) {
                        fail("the guest has a passive data segment; the snapshot cannot keep it");
                    }
                }
                let mut section = we::DataSection::new();
                for (offset, bytes) in &segments {
                    section.active(
                        0,
                        &we::ConstExpr::i32_const(*offset as i32),
                        bytes.iter().copied(),
                    );
                }
                out.section(&section);
            }
            // 名字、producers 之类的自定义段运行时用不着
            Payload::CustomSection(_) => {}
            _ => raw(&mut out, &payload, wasm),
        }
    }
    if !saw_data {
        fail("the guest has no data section");
    }
    out.finish()
}

fn raw(out: &mut wasm_encoder::Module, payload: &wasmparser::Payload<'_>, wasm: &[u8]) {
    if let Some((id, range)) = payload.as_section() {
        out.section(&wasm_encoder::RawSection {
            id,
            data: &wasm[range.start as usize..range.end as usize],
        });
    }
}

/// 内存里非零的连续片段；中间夹着不到 1 KiB 的零就并进同一段，段数少一些
fn segments(mem: &[u8]) -> Vec<(usize, &[u8])> {
    let mut segs = Vec::new();
    let mut i = 0;
    while i < mem.len() {
        if mem[i] == 0 {
            i += 1;
            continue;
        }
        let start = i;
        let mut last = i;
        while i < mem.len() && i - last < 1024 {
            if mem[i] != 0 {
                last = i;
            }
            i += 1;
        }
        segs.push((start, &mem[start..=last]));
        i = last + 1;
    }
    segs
}

// ── 预编译 ───────────────────────────────────────────────────────

fn precompile(wasm: &[u8], target: &str) -> Vec<u8> {
    let mut config = engine::config();
    config
        .target(target)
        .unwrap_or_else(|e| fail(&format!("Wasmtime cannot compile for {target}: {e}")));
    let engine = wasmtime::Engine::new(&config).unwrap_or_else(|e| fail(&format!("wasmtime: {e}")));
    engine
        .precompile_module(wasm)
        .unwrap_or_else(|e| fail(&format!("precompile the guest for {target}: {e:?}")))
}

// ── 杂项 ─────────────────────────────────────────────────────────

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn fail(msg: &str) -> ! {
    eprintln!("\nerror: tw-plugin: {msg}\n");
    std::process::exit(1);
}
