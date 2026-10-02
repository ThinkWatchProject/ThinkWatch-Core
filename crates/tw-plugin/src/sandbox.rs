//! 一个沙箱实例：一个 Store、一个 wasm 实例，加上它的 CPU、内存、日志三本账。
//!
//! 和 guest（`guest/src/lib.rs`）之间的约定都在这里：导出的名字和签名、输入
//! 怎么放进去（`tw_alloc` 一块、写进去、交出去）、输出怎么读（`tw_out_ptr` /
//! `tw_out_len`）、状态码。**guest 给的任何东西都按不可信处理**：指针和长度先
//! 过边界检查，输出先比上限再拷贝。

use std::time::Duration;

use wasmtime::{
    Caller, Instance, InstancePre, Linker, Memory, Module, ResourceLimiter, Store, Trap, TypedFunc,
    UpdateDeadline,
};

use crate::{Limits, LogLevel, LogLine, RunError, cpu};

/// guest 和这边的约定版本：guest 导出一个带版本号的函数名，见它的 `tw_abi_1`。
/// 改了导出的签名或含义，两边一起改名
const ABI_EXPORT: &str = "tw_abi_1";

/// 沙箱模块唯一允许的导入（build.rs 编的时候查过一次，加载时再查一次）
pub(crate) const ALLOWED_IMPORTS: &[(&str, &str)] =
    &[("tw", "log"), ("env", "__rquickjs_host_now_us")];

/// guest 函数表的上限（实际几百项）
const MAX_TABLE: usize = 4096;

/// 桥给回来的状态码（输出的第一个字节）
pub(crate) const VALUE: u8 = b'0';
pub(crate) const UNCHANGED: u8 = b'1';
pub(crate) const REJECTED: u8 = b'2';
pub(crate) const THREW: u8 = b'3';
pub(crate) const BAD: u8 = b'4';
pub(crate) const DROP: u8 = b'5';

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hook {
    Request = 0,
    ReplyText = 1,
    ReplyTextEnd = 2,
    ToolCall = 3,
}

/// 一个实例的宿主侧状态：资源账本
pub(crate) struct HostState {
    memory_cap: usize,
    memory_denied: bool,
    logs: Vec<LogLine>,
    max_log_lines: usize,
    max_log_line: usize,
    log_overflow: bool,
    /// 这一段执行的起点（线程 CPU 时间）和预算
    start: Duration,
    budget: Duration,
    cpu_hit: bool,
}

impl ResourceLimiter for HostState {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > self.memory_cap {
            // 拒绝而不是陷阱：guest 的 malloc 拿到空指针，QuickJS 抛一个
            // out of memory。记下来，这次调用按超出内存上限算
            self.memory_denied = true;
            return Ok(false);
        }
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        // guest 只有一张函数表（几百项），建实例之后从不扩它
        Ok(desired <= MAX_TABLE)
    }

    fn instances(&self) -> usize {
        1
    }

    fn tables(&self) -> usize {
        1
    }

    fn memories(&self) -> usize {
        1
    }
}

impl HostState {
    fn exceeded(&self) -> bool {
        cpu::now().saturating_sub(self.start) >= self.budget
    }
}

/// 把 guest 的两个导入接到宿主上。整个进程只建一次（`Runtime::new`）
pub(crate) fn linker(engine: &wasmtime::Engine) -> wasmtime::Result<Linker<HostState>> {
    let mut linker = Linker::new(engine);
    linker.func_wrap(
        "tw",
        "log",
        |mut caller: Caller<'_, HostState>,
         level: u32,
         ptr: u32,
         len: u32|
         -> wasmtime::Result<()> { log(&mut caller, level, ptr, len) },
    )?;
    // `Date` 用的时钟：墙上时间，粗到毫秒（`Date` 本来就是毫秒），不给插件一个
    // 高精度计时器。rquickjs-sys 的垫片按微秒要
    linker.func_wrap("env", "__rquickjs_host_now_us", || -> f64 {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        (ms as f64) * 1000.0
    })?;
    Ok(linker)
}

fn log(caller: &mut Caller<'_, HostState>, level: u32, ptr: u32, len: u32) -> wasmtime::Result<()> {
    let max_line = caller.data().max_log_line;
    let text = {
        let Some(memory) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
            return Ok(());
        };
        let data = memory.data(&caller);
        let start = ptr as usize;
        // 多读几个字节，截断时才知道是不是正好断在一个字符中间
        let take = (len as usize).min(max_line.saturating_add(4));
        let bytes = start
            .checked_add(take)
            .and_then(|end| data.get(start..end))
            .unwrap_or(&[]);
        truncate_line(
            &String::from_utf8_lossy(bytes),
            len as usize > take,
            max_line,
        )
    };
    let state = caller.data_mut();
    if state.logs.len() >= state.max_log_lines {
        // 第 max+1 行：这次调用按超出输出上限算，立刻停下
        state.log_overflow = true;
        return Err(wasmtime::Error::msg("the plugin wrote too many log lines"));
    }
    let level = match level {
        1 => LogLevel::Info,
        2 => LogLevel::Warn,
        3 => LogLevel::Error,
        _ => LogLevel::Log,
    };
    state.logs.push(LogLine { level, text });
    Ok(())
}

/// 一行日志不超过 `max` 字节：超了就在字符边界上截断，末尾标上省略号
fn truncate_line(s: &str, longer: bool, max: usize) -> String {
    if s.len() <= max && !longer {
        return s.to_string();
    }
    let mark = "…";
    let mut end = max.saturating_sub(mark.len()).min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + mark.len());
    out.push_str(&s[..end]);
    out.push_str(mark);
    out
}

struct Exports {
    alloc: TypedFunc<u32, u32>,
    out_ptr: TypedFunc<(), u32>,
    out_len: TypedFunc<(), u32>,
    compile: TypedFunc<(u32, u32), u32>,
    load: TypedFunc<(u32, u32), u32>,
    seed: TypedFunc<(u32, u32, u32, u32), u32>,
    set_ctx: TypedFunc<(u32, u32), u32>,
    call: TypedFunc<(u32, u32, u32), u32>,
}

/// 一段 JS 抛出的错误，桥整理成的样子
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Described {
    pub message: String,
    pub stack: Option<String>,
}

impl Described {
    pub(crate) fn parse(bytes: &[u8]) -> Described {
        #[derive(serde::Deserialize)]
        struct D {
            message: String,
            stack: Option<String>,
        }
        match serde_json::from_slice::<D>(bytes) {
            Ok(d) => Described {
                message: d.message,
                stack: d.stack,
            },
            Err(_) => Described {
                message: String::from_utf8_lossy(bytes).into_owned(),
                stack: None,
            },
        }
    }

    pub(crate) fn into_threw(self) -> RunError {
        RunError::Threw {
            message: self.message,
            stack: self.stack,
        }
    }
}

pub(crate) struct Sandbox {
    store: Store<HostState>,
    memory: Memory,
    f: Exports,
}

impl Sandbox {
    /// 新建一个实例。CPU 预算从这里就开始算：实例化本身也是这次调用的开销
    pub(crate) fn new(
        pre: &InstancePre<HostState>,
        limits: &Limits,
        memory_cap: usize,
        budget: Duration,
    ) -> Result<Sandbox, RunError> {
        let start = cpu::now();
        let mut store = Store::new(
            pre.module().engine(),
            HostState {
                memory_cap,
                memory_denied: false,
                logs: Vec::new(),
                max_log_lines: limits.max_log_lines,
                max_log_line: limits.max_log_line,
                log_overflow: false,
                start,
                budget,
                cpu_hit: false,
            },
        );
        store.limiter(|s| s as &mut dyn ResourceLimiter);
        store.epoch_deadline_callback(|mut ctx| {
            let s = ctx.data_mut();
            if s.exceeded() {
                s.cpu_hit = true;
                Ok(UpdateDeadline::Interrupt)
            } else {
                Ok(UpdateDeadline::Continue(1))
            }
        });
        store.set_epoch_deadline(1);
        let instance = match pre.instantiate(&mut store) {
            Ok(i) => i,
            Err(e) => {
                let s = store.data();
                if s.memory_denied {
                    return Err(RunError::MemoryLimit);
                }
                return Err(RunError::Trap(format!(
                    "the sandbox could not be created: {e}"
                )));
            }
        };
        let f = exports(&mut store, &instance)?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| RunError::Trap("the sandbox has no memory".into()))?;
        Ok(Sandbox { store, memory, f })
    }

    /// 下一段执行的 CPU 预算从现在算起
    pub(crate) fn arm(&mut self, budget: Duration) {
        let s = self.store.data_mut();
        s.start = cpu::now();
        s.budget = budget;
        s.cpu_hit = false;
        self.store.set_epoch_deadline(1);
    }

    /// 这一段执行（上次 `arm` 或建实例以来）用掉的 CPU 时间
    pub(crate) fn elapsed(&self) -> Duration {
        cpu::now().saturating_sub(self.store.data().start)
    }

    pub(crate) fn take_logs(&mut self) -> Vec<LogLine> {
        std::mem::take(&mut self.store.data_mut().logs)
    }

    /// 这次调用里内存上限有没有被碰到（有的话不管结果如何都算失败）
    fn memory_denied(&self) -> bool {
        self.store.data().memory_denied
    }

    /// 一次失败的 wasm 调用是哪一种失败
    fn classify(&self, e: wasmtime::Error) -> RunError {
        let s = self.store.data();
        if s.cpu_hit {
            return RunError::CpuLimit;
        }
        if s.log_overflow {
            return RunError::OutputLimit;
        }
        if s.memory_denied {
            return RunError::MemoryLimit;
        }
        match e.downcast_ref::<Trap>() {
            Some(Trap::Interrupt) => RunError::CpuLimit,
            Some(Trap::StackOverflow) => RunError::Trap("stack overflow".into()),
            Some(Trap::UnreachableCodeReached) => {
                RunError::Trap("the JavaScript engine aborted".into())
            }
            Some(t) => RunError::Trap(t.to_string()),
            None => RunError::Trap(e.to_string()),
        }
    }

    /// 把输入放进 guest 的内存。`nul` 时末尾补一个 0（`JS_Eval` 要）
    fn put(&mut self, bytes: &[u8], nul: bool) -> Result<u32, RunError> {
        let len = u32::try_from(bytes.len()).map_err(|_| RunError::MemoryLimit)?;
        let ptr = self
            .f
            .alloc
            .call(&mut self.store, len)
            .map_err(|e| self.classify(e))?;
        if ptr == 0 {
            // guest 的 malloc 失败：内存到顶了
            return Err(RunError::MemoryLimit);
        }
        let at = ptr as usize;
        self.memory
            .write(&mut self.store, at, bytes)
            .map_err(|e| RunError::Trap(e.to_string()))?;
        if nul {
            self.memory
                .write(&mut self.store, at + bytes.len(), &[0])
                .map_err(|e| RunError::Trap(e.to_string()))?;
        }
        Ok(ptr)
    }

    /// 读输出。超过 `cap` 字节就不拷，直接按超出输出上限算
    fn out(&mut self, cap: usize) -> Result<Vec<u8>, RunError> {
        let ptr = self
            .f
            .out_ptr
            .call(&mut self.store, ())
            .map_err(|e| self.classify(e))? as usize;
        let len = self
            .f
            .out_len
            .call(&mut self.store, ())
            .map_err(|e| self.classify(e))? as usize;
        if len > cap {
            return Err(RunError::OutputLimit);
        }
        let data = self.memory.data(&self.store);
        ptr.checked_add(len)
            .and_then(|end| data.get(ptr..end))
            .map(<[u8]>::to_vec)
            .ok_or_else(|| {
                RunError::Trap("the sandbox reported an output outside its memory".into())
            })
    }

    /// 编译插件源码。外层 `Err` 是沙箱失败（超时、内存……），内层 `Err` 是源码的错
    pub(crate) fn compile(&mut self, src: &[u8]) -> Result<Result<Vec<u8>, Described>, RunError> {
        let ptr = self.put(src, true)?;
        let len = src.len() as u32;
        let rc = self
            .f
            .compile
            .call(&mut self.store, (ptr, len))
            .map_err(|e| self.classify(e))?;
        if self.memory_denied() {
            return Err(RunError::MemoryLimit);
        }
        // 字节码大约是源码的两倍；错误描述很小
        let out = self.out(src.len().saturating_mul(8).max(1 << 20))?;
        Ok(if rc == 0 {
            Ok(out)
        } else {
            Err(Described::parse(&out))
        })
    }

    /// 求值模块顶层。成功时给回桥的那份 JSON（钩子、清单）
    pub(crate) fn load(&mut self, bytecode: &[u8]) -> Result<Result<Vec<u8>, Described>, RunError> {
        let ptr = self.put(bytecode, false)?;
        let rc = self
            .f
            .load
            .call(&mut self.store, (ptr, bytecode.len() as u32))
            .map_err(|e| self.classify(e))?;
        if self.memory_denied() {
            return Err(RunError::MemoryLimit);
        }
        let out = self.out(1 << 20)?;
        Ok(if rc == 0 {
            Ok(out)
        } else {
            Err(Described::parse(&out))
        })
    }

    /// 给 `Math.random` 换一个新种子（快照把原来的状态冻住了）
    pub(crate) fn seed(&mut self) -> Result<(), RunError> {
        let s: [u32; 4] = rand::random();
        let rc = self
            .f
            .seed
            .call(&mut self.store, (s[0], s[1], s[2], s[3]))
            .map_err(|e| self.classify(e))?;
        if rc != 0 {
            return Err(RunError::Trap(
                "the sandbox could not seed Math.random".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn set_ctx(&mut self, json: &[u8]) -> Result<(), RunError> {
        let ptr = self.put(json, false)?;
        let rc = self
            .f
            .set_ctx
            .call(&mut self.store, (ptr, json.len() as u32))
            .map_err(|e| self.classify(e))?;
        if self.memory_denied() {
            return Err(RunError::MemoryLimit);
        }
        if rc != 0 {
            let out = self.out(1 << 20)?;
            return Err(Described::parse(&out).into_threw());
        }
        Ok(())
    }

    /// 调一次钩子。返回状态码和内容；内容超过 `cap` 字节按超出输出上限算
    pub(crate) fn call(
        &mut self,
        hook: Hook,
        input: Option<&[u8]>,
        cap: usize,
    ) -> Result<(u8, Vec<u8>), RunError> {
        let (ptr, len) = match input {
            Some(bytes) => (self.put(bytes, false)?, bytes.len() as u32),
            None => (0, 0),
        };
        self.f
            .call
            .call(&mut self.store, (hook as u32, ptr, len))
            .map_err(|e| self.classify(e))?;
        if self.memory_denied() {
            return Err(RunError::MemoryLimit);
        }
        // 第一个字节是状态码
        let mut out = self.out(cap.saturating_add(1))?;
        if out.is_empty() {
            return Err(RunError::BadOutput("the sandbox returned nothing".into()));
        }
        let status = out.remove(0);
        Ok((status, out))
    }
}

fn exports(store: &mut Store<HostState>, i: &Instance) -> Result<Exports, RunError> {
    fn get<P: wasmtime::WasmParams, R: wasmtime::WasmResults>(
        store: &mut Store<HostState>,
        i: &Instance,
        name: &str,
    ) -> Result<TypedFunc<P, R>, RunError> {
        i.get_typed_func::<P, R>(&mut *store, name)
            .map_err(|e| RunError::Trap(format!("the sandbox lacks {name}: {e}")))
    }
    Ok(Exports {
        alloc: get(store, i, "tw_alloc")?,
        out_ptr: get(store, i, "tw_out_ptr")?,
        out_len: get(store, i, "tw_out_len")?,
        compile: get(store, i, "tw_compile")?,
        load: get(store, i, "tw_load")?,
        seed: get(store, i, "tw_seed")?,
        set_ctx: get(store, i, "tw_set_ctx")?,
        call: get(store, i, "tw_call")?,
    })
}

/// 核对预编译好的沙箱：导入表只有允许的那几个，该有的导出都在。不实例化 ——
/// 实例要预留一大段地址空间，启动时不该为这个检查去要
pub(crate) fn check_module(module: &Module) -> Result<(), String> {
    for import in module.imports() {
        let ok = matches!(import.ty(), wasmtime::ExternType::Func(_))
            && ALLOWED_IMPORTS
                .iter()
                .any(|(m, n)| *m == import.module() && *n == import.name());
        if !ok {
            return Err(format!(
                "the sandbox module imports {}.{}, which is not allowed",
                import.module(),
                import.name()
            ));
        }
    }
    for name in [
        "memory",
        "tw_alloc",
        "tw_out_ptr",
        "tw_out_len",
        "tw_compile",
        "tw_load",
        "tw_seed",
        "tw_set_ctx",
        "tw_call",
        ABI_EXPORT,
    ] {
        if module.get_export(name).is_none() {
            return Err(format!("the sandbox module does not export {name}"));
        }
    }
    Ok(())
}
