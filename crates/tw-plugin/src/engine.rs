//! 引擎配置：`build.rs`（预编译）和运行时（加载预编译结果）共用这一份。
//!
//! 预编译出来的 `.cwasm` 只肯在「编译它时的配置」下加载 —— 中断方式、内存的
//! 预留和保护区大小、wasm 特性，哪一项对不上 Wasmtime 都拒绝加载。两边各写一份
//! 迟早会对不上，所以都从这里取。
//!
//! 只用运行时和编译器两种构建里都有的设置。

/// 两边都要的那部分配置。`build.rs` 在此之上再指定目标平台。
pub fn config() -> wasmtime::Config {
    let mut c = wasmtime::Config::new();
    // CPU 时间上限靠它：后台线程定期推进纪元，到点就进回调，回调决定继续还是
    // 打断。比「燃料」便宜（实测纪元慢 15–18%，燃料慢 25–50%），而且编译进
    // 代码里的检查点也覆盖正则回溯这种不回到 JS 解释器的循环
    c.epoch_interruption(true);
    // 内存按需分配（不用池）：池在启动时就按槽位预留 4 GiB 一个的地址空间，
    // `ulimit -v` 或严格的 overcommit 下进程直接起不来。按需分配只在实例活着的
    // 时候占地址空间，拿不到就是这一次调用失败
    c.allocation_strategy(wasmtime::InstanceAllocationStrategy::OnDemand);
    // 快照的内存镜像尽量写时复制（Linux 上用 memfd；macOS 和 Windows 上
    // Wasmtime 退回逐页拷贝）
    c.memory_init_cow(true);
    // 宿主线程上 wasm 能用的栈。调用方的线程要留出比这更多的栈（2 MiB 足够）
    c.max_wasm_stack(1 << 20);
    c
}
