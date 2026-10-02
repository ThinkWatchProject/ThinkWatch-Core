//! 脚本插件的沙箱：在 WebAssembly 里跑一个插件的钩子。
//!
//! 插件是一个 JavaScript 模块。它在 QuickJS 里执行，而 QuickJS 本身编成了 wasm、
//! 跑在 Wasmtime 里（`build.rs` 把它编好、做成快照、预编译成本机机器码嵌进来）。
//! 沙箱从宿主那里只拿得到两样东西：写一行日志、看一眼时钟。没有文件、网络、
//! 环境变量、进程，也没有别的插件。
//!
//! 每次请求钩子都是一个**新实例**；一个回答用**一个实例**，那个回答的每次钩子
//! 调用共用它，回答结束就丢掉。实例从快照起步（QuickJS 已经初始化好），然后求值
//! 插件模块的顶层 —— 所以插件的模块级变量在请求之间不会留下来。
//!
//! 每次调用都有 CPU 时间、内存、输出大小、日志量四个上限（[`Limits`]），超了就是
//! 一个 [`RunError`]。所有调用都是阻塞的、吃 CPU 的：调用方放到专用线程上跑，
//! 别放在异步运行时的工作线程上。线程栈要有 2 MiB 以上（wasm 自己最多用 1 MiB）。
//!
//! 这个 crate 只管「跑」：视图怎么构造、权限怎么裁、改动怎么写回，都在 tw-gateway。

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};
use wasmtime::{Engine, InstancePre, Module};

mod cpu;
mod engine;
mod manifest;
mod sandbox;
mod ticker;

use sandbox::{Described, Hook, Sandbox};

/// 嵌进来的沙箱模块（QuickJS 编成的 wasm，做成快照后预编译成本机机器码）
static GUEST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/guest.cwasm"));

/// 沙箱的 wasm（快照之前）的 SHA-256。和 [`GUEST_CLANG`] 一起，能对上发出去的
/// 二进制里是哪一份沙箱、用哪个编译器编的
pub const GUEST_WASM_SHA256: &str = env!("TW_PLUGIN_GUEST_SHA256");

/// 编沙箱里 C 那一半（QuickJS）用的 clang（`clang --version` 的第一行）
pub const GUEST_CLANG: &str = env!("TW_PLUGIN_GUEST_CLANG");

// ── 上限 ─────────────────────────────────────────────────────────

/// 每次调用的资源上限。超过任何一项都是 [`RunError`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// 一次请求钩子（含建实例、求值模块顶层）的 CPU 时间
    pub request_cpu: Duration,
    /// 回答钩子每次调用的 CPU 时间
    pub reply_call_cpu: Duration,
    /// 一个回答所有钩子调用加起来的 CPU 时间（含建实例）
    pub reply_total_cpu: Duration,
    /// 请求钩子实例的内存（wasm 线性内存的上限）
    pub request_memory: usize,
    /// 回答实例的内存
    pub reply_memory: usize,
    /// 钩子输出的大小
    pub max_output: OutputCap,
    /// 一次调用最多写几行日志。再多一行这次调用就失败
    pub max_log_lines: usize,
    /// 一行日志最多几个字节，超出的部分截掉
    pub max_log_line: usize,
    /// 插件源码最多几个字节
    pub max_source: usize,
}

/// 钩子输出的上限：`factor × 输入字节数 + extra`。
///
/// 请求钩子的输入是视图的 JSON；回答钩子的输入是这次的文字或工具调用。
/// `onReplyTextEnd` 没有输入，于是攒着到最后才放出的文字最多 `extra`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputCap {
    pub factor: usize,
    pub extra: usize,
}

impl OutputCap {
    pub fn limit(&self, input: usize) -> usize {
        input.saturating_mul(self.factor).saturating_add(self.extra)
    }
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            request_cpu: Duration::from_millis(200),
            reply_call_cpu: Duration::from_millis(20),
            reply_total_cpu: Duration::from_secs(2),
            request_memory: 128 << 20,
            reply_memory: 64 << 20,
            max_output: OutputCap {
                factor: 2,
                extra: 1 << 20,
            },
            max_log_lines: 100,
            max_log_line: 4096,
            max_source: 1 << 20,
        }
    }
}

// ── 结果 ─────────────────────────────────────────────────────────

/// 一次调用的结果，连同它写的日志和用掉的 CPU 时间
#[derive(Debug, Clone, PartialEq)]
pub struct Invocation<T> {
    pub result: Result<T, RunError>,
    pub logs: Vec<LogLine>,
    pub cpu: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub level: LogLevel,
    pub text: String,
}

/// `console.log` / `info` / `warn` / `error`（`console.debug` 算 `Log`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Log,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RequestOutcome {
    /// 返回了 `undefined`，或者返回的视图和传进去的一样
    Unchanged,
    /// 改过的视图（没核对过结构和权限，那是 tw-gateway 的事）
    Changed(Value),
    /// 调了 `reject(理由)`
    Rejected(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolCallOutcome {
    Unchanged,
    /// 换成这些调用（一个对象也包成一个元素）。每个都是 `{ id?, name, input }`，
    /// 没有 `id` 的由调用方生成
    Replace(Vec<Value>),
    /// 返回了 `null` 或空数组
    Drop,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunError {
    #[error("the plugin ran past its CPU time limit")]
    CpuLimit,
    #[error("the plugin ran past its memory limit")]
    MemoryLimit,
    #[error("the plugin's output or log was too large")]
    OutputLimit,
    #[error("the plugin threw: {message}")]
    Threw {
        message: String,
        stack: Option<String>,
    },
    #[error("the plugin returned something invalid: {0}")]
    BadOutput(String),
    #[error("the sandbox stopped: {0}")]
    Trap(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoadError {
    #[error("the plugin file is larger than the limit")]
    TooLarge,
    /// 源码编不过，或者模块顶层一执行就出错（`line`/`column` 从 1 数）
    #[error("{message}")]
    Syntax {
        message: String,
        line: Option<u32>,
        column: Option<u32>,
    },
    #[error("{0}")]
    Manifest(String),
    #[error("the plugin is written for plugin API {0}; this version supports API 1")]
    UnsupportedApi(u32),
    #[error("the sandbox failed: {0}")]
    Engine(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the plugin sandbox cannot start: {0}")]
pub struct InitError(pub String);

// ── 清单 ─────────────────────────────────────────────────────────

/// 插件导出的 `manifest`，核对过的
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub api: u32,
    pub description: Option<String>,
    pub permissions: BTreeSet<Permission>,
    pub scope: Scope,
    pub reply_mode: ReplyMode,
    /// 按作者写的先后
    pub settings: Vec<SettingSpec>,
    pub hooks: Hooks,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    System,
    Messages,
    Tools,
    Params,
    ReplyText,
    ReplyToolCalls,
}

impl Permission {
    pub const ALL: [Permission; 6] = [
        Permission::System,
        Permission::Messages,
        Permission::Tools,
        Permission::Params,
        Permission::ReplyText,
        Permission::ReplyToolCalls,
    ];

    /// 控制面和配置里的写法：`reply_text`
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::System => "system",
            Permission::Messages => "messages",
            Permission::Tools => "tools",
            Permission::Params => "params",
            Permission::ReplyText => "reply_text",
            Permission::ReplyToolCalls => "reply_tool_calls",
        }
    }

    /// 插件清单里的写法：`reply.text`
    pub fn manifest_name(self) -> &'static str {
        match self {
            Permission::ReplyText => "reply.text",
            Permission::ReplyToolCalls => "reply.tool_calls",
            other => other.as_str(),
        }
    }

    pub fn from_manifest(s: &str) -> Option<Permission> {
        Permission::ALL.into_iter().find(|p| p.manifest_name() == s)
    }
}

/// 清单里的 `match`。每一项是带 `*` 的通配；空的表示不限
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    pub clients: Vec<String>,
    pub models: Vec<String>,
    pub upstreams: Vec<String>,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ReplyMode {
    #[default]
    Block,
    Stream,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingSpec {
    pub key: String,
    pub kind: SettingKind,
    pub label: String,
    /// 和 `kind` 同类型的值；清单没写就是 `""` / `0` / `false`
    pub default: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingKind {
    String,
    Number,
    Boolean,
}

impl SettingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SettingKind::String => "string",
            SettingKind::Number => "number",
            SettingKind::Boolean => "boolean",
        }
    }
}

/// 插件导出了哪些钩子
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Hooks {
    pub request: bool,
    pub reply_text: bool,
    pub reply_text_end: bool,
    pub tool_call: bool,
}

// ── 运行时 ───────────────────────────────────────────────────────

/// 整个进程一份。克隆很便宜
#[derive(Clone)]
pub struct Runtime {
    inner: Arc<RuntimeInner>,
}

struct RuntimeInner {
    limits: Limits,
    pre: InstancePre<sandbox::HostState>,
    ticker: ticker::Ticker,
}

impl fmt::Debug for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Runtime")
            .field("limits", &self.inner.limits)
            .finish_non_exhaustive()
    }
}

impl Runtime {
    /// 加载嵌进来的沙箱（约 0.5 ms），起一个计时线程。**不建实例**：实例要预留
    /// 一大段地址空间，地址空间受限的机器上也不会在这一步失败
    pub fn new(limits: Limits) -> Result<Runtime, InitError> {
        let engine =
            Engine::new(&engine::config()).map_err(|e| InitError(format!("wasmtime: {e}")))?;
        // SAFETY: 这些字节是 build.rs 用同一版本的 Wasmtime、同一份配置预编译出来、
        // 在编译期嵌进二进制的（include_bytes）—— 不是从任何可写的位置读来的
        let module = unsafe { Module::deserialize(&engine, GUEST) }
            .map_err(|e| InitError(format!("the embedded sandbox does not load: {e}")))?;
        sandbox::check_module(&module).map_err(InitError)?;
        let linker = sandbox::linker(&engine).map_err(|e| InitError(e.to_string()))?;
        let pre = linker
            .instantiate_pre(&module)
            .map_err(|e| InitError(format!("the sandbox cannot be linked: {e}")))?;
        let ticker = ticker::Ticker::start(engine)
            .map_err(|e| InitError(format!("cannot start the timer thread: {e}")))?;
        Ok(Runtime {
            inner: Arc::new(RuntimeInner {
                limits,
                pre,
                ticker,
            }),
        })
    }

    pub fn limits(&self) -> &Limits {
        &self.inner.limits
    }

    /// 沙箱模块的导入（`模块.名字`）。只有日志和时钟两项
    pub fn sandbox_imports(&self) -> Vec<String> {
        self.inner
            .pre
            .module()
            .imports()
            .map(|i| format!("{}.{}", i.module(), i.name()))
            .collect()
    }

    /// 编译并核对一个插件。只用这里给的字节：哈希的、编译的都是它们
    pub fn load(&self, source: &[u8]) -> Result<Plugin, LoadError> {
        let limits = &self.inner.limits;
        if source.len() > limits.max_source {
            return Err(LoadError::TooLarge);
        }
        let sha256: [u8; 32] = Sha256::digest(source).into();
        let text = std::str::from_utf8(source).map_err(|e| not_utf8(source, e.valid_up_to()))?;
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);

        let _running = self.inner.ticker.enter();
        // 编译只在加载时做一次，给宽一点的时间；模块顶层的预算和每次调用一样
        let mut sb = Sandbox::new(
            &self.inner.pre,
            limits,
            limits.request_memory,
            limits.reply_total_cpu.max(limits.request_cpu),
        )
        .map_err(sandbox_failed)?;
        let bytecode = match sb.compile(text.as_bytes()).map_err(top_level_failed)? {
            Ok(bc) => bc,
            Err(d) => return Err(syntax(d)),
        };
        sb.arm(limits.request_cpu);
        sb.seed().map_err(top_level_failed)?;
        let info = match sb.load(&bytecode).map_err(top_level_failed)? {
            Ok(info) => info,
            Err(d) => return Err(syntax(d)),
        };
        drop(sb);
        let manifest = manifest::parse(&info)?;

        let plugin = Plugin {
            inner: Arc::new(PluginInner {
                rt: Arc::clone(&self.inner),
                manifest,
                sha256,
                bytecode,
            }),
        };
        // 回答实例的内存更小。模块顶层在那里放不下的话，现在就说
        if plugin.inner.manifest.hooks.reply_text || plugin.inner.manifest.hooks.tool_call {
            plugin
                .setup(limits.reply_memory)
                .map_err(|(e, _)| top_level_failed(e))?;
        }
        Ok(plugin)
    }
}

// ── 插件 ─────────────────────────────────────────────────────────

/// 一个加载好的插件。克隆很便宜，可以跨线程共享
#[derive(Clone)]
pub struct Plugin {
    inner: Arc<PluginInner>,
}

struct PluginInner {
    rt: Arc<RuntimeInner>,
    manifest: Manifest,
    sha256: [u8; 32],
    bytecode: Vec<u8>,
}

impl fmt::Debug for Plugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Plugin")
            .field("name", &self.inner.manifest.name)
            .field("sha256", &hex(&self.inner.sha256))
            .finish_non_exhaustive()
    }
}

impl Plugin {
    pub fn manifest(&self) -> &Manifest {
        &self.inner.manifest
    }

    /// 交给 [`Runtime::load`] 的那些字节的 SHA-256
    pub fn sha256(&self) -> [u8; 32] {
        self.inner.sha256
    }

    /// 新实例 → 求值模块顶层 → 设好 `ctx`。返回实例、它写的日志
    fn setup(&self, memory: usize) -> Result<(Sandbox, Vec<LogLine>), (RunError, Vec<LogLine>)> {
        let limits = &self.inner.rt.limits;
        let mut sb = Sandbox::new(&self.inner.rt.pre, limits, memory, limits.request_cpu)
            .map_err(|e| (e, Vec::new()))?;
        let r = (|| {
            sb.seed()?;
            if let Err(d) = sb.load(&self.inner.bytecode)? {
                return Err(d.into_threw());
            }
            Ok(())
        })();
        match r {
            Ok(()) => {
                let logs = sb.take_logs();
                Ok((sb, logs))
            }
            Err(e) => {
                let logs = sb.take_logs();
                Err((e, logs))
            }
        }
    }

    /// 跑一次请求钩子：新实例，用完即弃。`view` 是已经按权限裁好的请求视图
    pub fn on_request(&self, view: Value, ctx: Value) -> Invocation<RequestOutcome> {
        if !self.inner.manifest.hooks.request {
            return Invocation {
                result: Ok(RequestOutcome::Unchanged),
                logs: Vec::new(),
                cpu: Duration::ZERO,
            };
        }
        let limits = &self.inner.rt.limits;
        let view_json = to_json(&view);
        let ctx_json = to_json(&ctx);
        let _running = self.inner.rt.ticker.enter();
        let start = cpu::now();
        let (mut sb, mut logs) = match self.setup(limits.request_memory) {
            Ok(x) => x,
            Err((e, logs)) => {
                return Invocation {
                    result: Err(e),
                    logs,
                    cpu: cpu::now().saturating_sub(start),
                };
            }
        };
        let cap = limits.max_output.limit(view_json.len());
        let raw = sb
            .set_ctx(&ctx_json)
            .and_then(|()| sb.call(Hook::Request, Some(&view_json), cap));
        // 量到钩子返回为止：解析输出、和原视图比较是这边的事
        let spent = sb.elapsed();
        logs.extend(sb.take_logs());
        drop(sb);
        Invocation {
            result: raw.and_then(|(status, payload)| decode_request(status, &payload, &view)),
            logs,
            cpu: spent,
        }
    }

    /// 为一个回答建实例。这个回答的每次钩子调用都用它，回答结束就丢掉
    pub fn reply(&self, ctx: Value) -> Result<Reply, RunError> {
        let limits = &self.inner.rt.limits;
        let ctx_json = to_json(&ctx);
        let _running = self.inner.rt.ticker.enter();
        let (mut sb, mut logs) = self.setup(limits.reply_memory).map_err(|(e, _)| e)?;
        sb.set_ctx(&ctx_json)?;
        logs.extend(sb.take_logs());
        let cpu = sb.elapsed();
        Ok(Reply {
            plugin: Arc::clone(&self.inner),
            sb,
            used: cpu,
            carry_logs: logs,
            carry_cpu: cpu,
            failed: None,
        })
    }
}

// ── 回答 ─────────────────────────────────────────────────────────

/// 一个回答的实例。一次只能一个线程用它
pub struct Reply {
    plugin: Arc<PluginInner>,
    sb: Sandbox,
    /// 这个回答到现在一共用掉的 CPU 时间
    used: Duration,
    /// 建实例时的日志和 CPU 时间，记到第一次调用头上
    carry_logs: Vec<LogLine>,
    carry_cpu: Duration,
    /// 实例中途被打断过（超时、超内存、陷阱）：它的状态不再可信，之后的调用都给这个错
    failed: Option<RunError>,
}

impl fmt::Debug for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reply")
            .field("plugin", &self.plugin.manifest.name)
            .field("used", &self.used)
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

impl Reply {
    /// 一段助手文字。分块模式下是整块，流式时是一段增量。`None` 表示照原样
    pub fn on_text(&mut self, text: &str) -> Invocation<Option<String>> {
        if !self.plugin.manifest.hooks.reply_text {
            return self.skip(None);
        }
        let cap = self.plugin.rt.limits.max_output.limit(text.len());
        self.run(Hook::ReplyText, Some(text.as_bytes()), cap)
            .then(|(status, payload)| match status {
                sandbox::VALUE => {
                    let s = String::from_utf8_lossy(&payload).into_owned();
                    Ok(if s == text { None } else { Some(s) })
                }
                sandbox::UNCHANGED => Ok(None),
                other => Err(decode_error(other, payload, "onReplyText")),
            })
    }

    /// 流式模式下一个文字块结束：放出还攒着的文字
    pub fn on_text_end(&mut self) -> Invocation<Option<String>> {
        if !self.plugin.manifest.hooks.reply_text_end {
            return self.skip(None);
        }
        let cap = self.plugin.rt.limits.max_output.limit(0);
        self.run(Hook::ReplyTextEnd, None, cap)
            .then(|(status, payload)| match status {
                sandbox::VALUE => Ok(Some(String::from_utf8_lossy(&payload).into_owned())),
                sandbox::UNCHANGED => Ok(None),
                other => Err(decode_error(other, payload, "onReplyTextEnd")),
            })
    }

    /// 一个完整的工具调用 `{ id, name, input }`
    pub fn on_tool_call(&mut self, call: Value) -> Invocation<ToolCallOutcome> {
        if !self.plugin.manifest.hooks.tool_call {
            return self.skip(ToolCallOutcome::Unchanged);
        }
        let input = to_json(&call);
        let cap = self.plugin.rt.limits.max_output.limit(input.len());
        self.run(Hook::ToolCall, Some(&input), cap)
            .then(|(status, payload)| decode_tool_call(status, payload, &call))
    }

    fn skip<T>(&mut self, value: T) -> Invocation<T> {
        Invocation {
            result: Ok(value),
            logs: std::mem::take(&mut self.carry_logs),
            cpu: std::mem::take(&mut self.carry_cpu),
        }
    }

    fn run(&mut self, hook: Hook, input: Option<&[u8]>, cap: usize) -> Invocation<(u8, Vec<u8>)> {
        let mut logs = std::mem::take(&mut self.carry_logs);
        let mut cpu = std::mem::take(&mut self.carry_cpu);
        if let Some(e) = &self.failed {
            return Invocation {
                result: Err(e.clone()),
                logs,
                cpu,
            };
        }
        let limits = &self.plugin.rt.limits;
        let left = limits.reply_total_cpu.saturating_sub(self.used);
        if left.is_zero() {
            self.failed = Some(RunError::CpuLimit);
            return Invocation {
                result: Err(RunError::CpuLimit),
                logs,
                cpu,
            };
        }
        let _running = self.plugin.rt.ticker.enter();
        self.sb.arm(limits.reply_call_cpu.min(left));
        let result = self.sb.call(hook, input, cap);
        let spent = self.sb.elapsed();
        self.used += spent;
        cpu += spent;
        logs.extend(self.sb.take_logs());
        if let Err(e) = &result
            && !matches!(e, RunError::Threw { .. } | RunError::BadOutput(_))
        {
            self.failed = Some(e.clone());
        }
        Invocation { result, logs, cpu }
    }
}

impl<T> Invocation<T> {
    fn then<U>(self, f: impl FnOnce(T) -> Result<U, RunError>) -> Invocation<U> {
        Invocation {
            result: self.result.and_then(f),
            logs: self.logs,
            cpu: self.cpu,
        }
    }
}

// ── 解码 ─────────────────────────────────────────────────────────

fn decode_request(status: u8, payload: &[u8], view: &Value) -> Result<RequestOutcome, RunError> {
    match status {
        sandbox::VALUE => {
            let v: Value = serde_json::from_slice(payload).map_err(|e| {
                RunError::BadOutput(format!(
                    "onRequest returned something that is not valid JSON: {e}"
                ))
            })?;
            if !v.is_object() {
                return Err(RunError::BadOutput(
                    "onRequest must return the request object or undefined".into(),
                ));
            }
            Ok(if js_equal(&v, view) {
                RequestOutcome::Unchanged
            } else {
                RequestOutcome::Changed(v)
            })
        }
        sandbox::UNCHANGED => Ok(RequestOutcome::Unchanged),
        sandbox::REJECTED => Ok(RequestOutcome::Rejected(
            String::from_utf8_lossy(payload).into_owned(),
        )),
        other => Err(decode_error(other, payload.to_vec(), "onRequest")),
    }
}

fn decode_tool_call(
    status: u8,
    payload: Vec<u8>,
    call: &Value,
) -> Result<ToolCallOutcome, RunError> {
    match status {
        sandbox::VALUE => {
            let v: Value = serde_json::from_slice(&payload).map_err(|e| {
                RunError::BadOutput(format!(
                    "onToolCall returned something that is not valid JSON: {e}"
                ))
            })?;
            let Value::Array(calls) = v else {
                return Err(RunError::BadOutput(
                    "onToolCall must return a tool call, an array of them, null or undefined"
                        .into(),
                ));
            };
            if calls.is_empty() {
                return Ok(ToolCallOutcome::Drop);
            }
            let calls = calls
                .into_iter()
                .enumerate()
                .map(|(i, c)| {
                    tool_call(c).map_err(|m| RunError::BadOutput(format!("tool call {i}: {m}")))
                })
                .collect::<Result<Vec<_>, _>>()?;
            if calls.len() == 1 && js_equal(&calls[0], call) {
                return Ok(ToolCallOutcome::Unchanged);
            }
            Ok(ToolCallOutcome::Replace(calls))
        }
        sandbox::UNCHANGED => Ok(ToolCallOutcome::Unchanged),
        sandbox::DROP => Ok(ToolCallOutcome::Drop),
        other => Err(decode_error(other, payload, "onToolCall")),
    }
}

/// 一个替换用的工具调用：`{ id?, name, input }`，别的字段不收
fn tool_call(v: Value) -> Result<Value, String> {
    let Value::Object(mut m) = v else {
        return Err("must be an object { id, name, input }".into());
    };
    for k in m.keys() {
        if !matches!(k.as_str(), "id" | "name" | "input") {
            return Err(format!(
                "unknown field `{k}`; a tool call has id, name and input"
            ));
        }
    }
    match m.get("name") {
        Some(Value::String(s)) if !s.trim().is_empty() => {}
        _ => return Err("needs a non-empty string `name`".into()),
    }
    if !m.contains_key("input") {
        return Err("needs an `input`".into());
    }
    match m.get("id") {
        None => {}
        Some(Value::Null) => {
            m.remove("id");
        }
        Some(Value::String(s)) if !s.is_empty() => {}
        Some(_) => return Err("`id` must be a non-empty string or left out".into()),
    }
    Ok(Value::Object(m))
}

fn decode_error(status: u8, payload: Vec<u8>, hook: &str) -> RunError {
    match status {
        sandbox::THREW => Described::parse(&payload).into_threw(),
        sandbox::BAD => RunError::BadOutput(String::from_utf8_lossy(&payload).into_owned()),
        sandbox::REJECTED => {
            RunError::BadOutput(format!("{hook} cannot reject; only onRequest can"))
        }
        _ => RunError::BadOutput(format!("{hook} returned an unexpected result")),
    }
}

/// 两个 JSON 值在 JavaScript 看来是否相等。
///
/// 值进出一趟 JS 会变样：`1.0` 回来是 `1`，超过 2^53 的整数会丢精度。插件没碰的
/// 部分不该因为这个被当成「改过」—— 数字按双精度浮点比。对象的键不分先后。
/// 写回请求时判断某一项有没有被改，也该用这个比。
pub fn js_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => x == y,
            _ => x == y,
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| js_equal(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| js_equal(v, w)))
        }
        _ => a == b,
    }
}

// ── 杂项 ─────────────────────────────────────────────────────────

fn to_json(v: &Value) -> Vec<u8> {
    // serde_json::Value 的键都是字符串，序列化不会失败
    serde_json::to_vec(v).unwrap_or_else(|_| b"null".to_vec())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 编译或求值模块时抛出的错误：从栈里找出 `plugin.js:行:列`
fn syntax(d: Described) -> LoadError {
    let (line, column) = d
        .stack
        .as_deref()
        .and_then(location)
        .or_else(|| location(&d.message))
        .map_or((None, None), |(l, c)| (Some(l), Some(c)));
    LoadError::Syntax {
        message: d.message,
        line,
        column,
    }
}

fn location(s: &str) -> Option<(u32, u32)> {
    let mut rest = s;
    while let Some(at) = rest.find("plugin.js:") {
        rest = &rest[at + "plugin.js:".len()..];
        let line_end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let line = rest[..line_end].parse::<u32>().ok();
        let after = &rest[line_end..];
        if let (Some(line), Some(col)) = (line, after.strip_prefix(':')) {
            let col_end = col.find(|c: char| !c.is_ascii_digit()).unwrap_or(col.len());
            if let Ok(column) = col[..col_end].parse::<u32>() {
                return Some((line, column));
            }
        }
    }
    None
}

fn not_utf8(source: &[u8], valid_up_to: usize) -> LoadError {
    let before = String::from_utf8_lossy(&source[..valid_up_to]);
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    LoadError::Syntax {
        message: "the plugin file is not valid UTF-8".into(),
        line: u32::try_from(line).ok(),
        column: u32::try_from(column).ok(),
    }
}

/// 建实例这一步就失败了：不是插件的错
fn sandbox_failed(e: RunError) -> LoadError {
    match e {
        RunError::MemoryLimit => {
            LoadError::Engine("the sandbox could not get its initial memory".into())
        }
        other => LoadError::Engine(other.to_string()),
    }
}

/// 编译或模块顶层跑到一半撞了上限
fn top_level_failed(e: RunError) -> LoadError {
    let message = match e {
        RunError::CpuLimit => "the plugin's top-level code ran past the CPU time limit".to_string(),
        RunError::MemoryLimit => {
            "the plugin's top-level code ran past the memory limit".to_string()
        }
        RunError::OutputLimit => "the plugin's top-level code wrote too many log lines".to_string(),
        RunError::Threw { message, stack } => {
            return syntax(Described { message, stack });
        }
        RunError::BadOutput(m) => return LoadError::Engine(m),
        RunError::Trap(m) => format!("the plugin's top-level code stopped the sandbox: {m}"),
    };
    LoadError::Syntax {
        message,
        line: None,
        column: None,
    }
}
