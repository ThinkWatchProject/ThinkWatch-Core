//! 插件沙箱里的 QuickJS，和宿主（`tw-plugin`）之间的那层胶水。
//!
//! **它只导入两个函数**：`tw.log`（插件的 `console.*`）和
//! `env.__rquickjs_host_now_us`（rquickjs-sys 垫片里的时钟，`Date` 用）。没有
//! WASI，没有文件、网络、环境变量、进程。宿主在实例化前核对这张导入表。
//!
//! 宿主只通过下面这些导出驱动它：
//!
//! 1. 构建时：`tw_init(bridge.js)` 建运行时和上下文、求值桥脚本；之后整块内存
//!    做成快照，每个实例都从那里开始（`tw-plugin` 的 `build.rs`）。
//! 2. 加载插件：`tw_compile(源码)` 出字节码；再在新实例里走一遍 3 读出清单。
//! 3. 每个实例：`tw_seed` → `tw_load(字节码)`（求值模块顶层）→ `tw_set_ctx`。
//! 4. 每次钩子：`tw_call` —— 调钩子、跑完 Promise 任务、取结果。
//!
//! 结果放在一块输出缓冲里，宿主用 `tw_out_ptr` / `tw_out_len` 读。输入由宿主用
//! `tw_alloc` 要一块内存写进来，交给导出函数之后归这边释放。
//!
//! **没有 std，也没有 `alloc`**：内存一律走 libc 的 `malloc`（QuickJS 用的同一个
//! 堆）。这里的代码刻意不留任何会 panic 的写法（下标、`unwrap`）—— panic 会把
//! 源文件路径编进 wasm，而路径的写法随构建机器变（Windows 是反斜杠），同一份源码
//! 在不同机器上就编不出逐字节相同的 wasm 了。`tw-plugin` 有测试守着这一条。

#![no_std]
// 这些导出只由宿主调用，约定（谁分配、谁释放、指针指向哪里）就是上面那一段；
// 每个函数再写一遍「# Safety」只是重复
#![allow(clippy::missing_safety_doc)]

use core::ffi::{CStr, c_char, c_int, c_void};
use core::ptr::null_mut;

use rquickjs_sys as q;
use rquickjs_sys::{JSContext, JSRuntime, JSValue};

// ── 导入 ─────────────────────────────────────────────────────────

#[link(wasm_import_module = "tw")]
unsafe extern "C" {
    /// 插件的一行日志。宿主截断过长的行、数行数，超出上限就让这次调用失败。
    #[link_name = "log"]
    fn host_log(level: u32, ptr: *const u8, len: usize);
}

// wasi-libc 的 dlmalloc，QuickJS 的默认分配器用的也是它
unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

// ── 状态 ─────────────────────────────────────────────────────────
//
// 全部在线性内存里，所以会进快照。单线程，没有并发访问。

static mut RT: *mut JSRuntime = null_mut();
static mut CTX: *mut JSContext = null_mut();
/// 桥脚本求值出来的对象。只有这里拿着它，插件够不着
static mut BRIDGE: JSValue = q::JS_UNDEFINED;

/// 输出缓冲：要么是 QuickJS 给的 C 字符串，要么是它 `js_malloc` 的字节码，
/// 要么是这边 `malloc` 的一块。下一次写输出前释放上一块。
static mut OUT_PTR: *const u8 = core::ptr::null();
static mut OUT_LEN: usize = 0;
static mut OUT_KIND: u8 = OUT_NONE;
const OUT_NONE: u8 = 0;
const OUT_CSTRING: u8 = 1;
const OUT_JS_MALLOC: u8 = 2;
const OUT_STATIC: u8 = 3;

/// QuickJS 自己的栈上限（它量的是 wasm 线性内存里的影子栈，总共 1 MiB）。
/// 递归太深时插件拿到一个 RangeError，而不是一个陷阱
const JS_STACK_LIMIT: usize = 256 * 1024;

// ── 导出 ─────────────────────────────────────────────────────────

/// 宿主和胶水之间的约定版本写在这个导出的名字里（宿主按名字找它，不用实例化）。
/// 改了导出的签名或含义就改名，两边一起改
#[unsafe(no_mangle)]
pub extern "C" fn tw_abi_1() {}

/// 要一块 `len` 字节的内存给宿主写输入。多给一个字节：要求以 NUL 结尾的
/// 输入（`JS_Eval`）由宿主在末尾补 0
#[unsafe(no_mangle)]
pub extern "C" fn tw_alloc(len: usize) -> *mut u8 {
    unsafe { malloc(len.saturating_add(1)) as *mut u8 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tw_free(ptr: *mut u8) {
    unsafe { free(ptr as *mut c_void) }
}

#[unsafe(no_mangle)]
pub extern "C" fn tw_out_ptr() -> *const u8 {
    unsafe { OUT_PTR }
}

#[unsafe(no_mangle)]
pub extern "C" fn tw_out_len() -> usize {
    unsafe { OUT_LEN }
}

/// 建运行时和上下文，求值桥脚本（`src` 以 NUL 结尾，`len` 不含它）。只在构建
/// 时调一次。0 成功；1 失败，输出是错误描述。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tw_init(src: *mut u8, len: usize) -> u32 {
    unsafe {
        let rt = q::JS_NewRuntime();
        if rt.is_null() {
            free(src as *mut c_void);
            return fail(c"cannot create the JavaScript runtime");
        }
        q::JS_SetMaxStackSize(rt, JS_STACK_LIMIT as q::size_t);
        let ctx = q::JS_NewContextRaw(rt);
        if ctx.is_null() {
            free(src as *mut c_void);
            return fail(c"cannot create the JavaScript context");
        }
        RT = rt;
        CTX = ctx;
        // 标准 ECMAScript 的那些。**不加** performance（高精度计时）、atob/btoa、
        // DOMException —— 它们不是 ECMAScript；桥脚本另外再删一遍不在白名单里的全局
        if q::JS_AddIntrinsicBaseObjects(ctx) != 0
            || q::JS_AddIntrinsicDate(ctx) != 0
            || q::JS_AddIntrinsicEval(ctx) != 0
            || q::JS_AddIntrinsicRegExp(ctx) != 0
            || q::JS_AddIntrinsicJSON(ctx) != 0
            || q::JS_AddIntrinsicProxy(ctx) != 0
            || q::JS_AddIntrinsicMapSet(ctx) != 0
            || q::JS_AddIntrinsicTypedArrays(ctx) != 0
            || q::JS_AddIntrinsicPromise(ctx) != 0
            || q::JS_AddIntrinsicWeakRef(ctx) != 0
        {
            free(src as *mut c_void);
            return fail(c"cannot add the standard built-ins");
        }

        // 桥脚本先拿走这个函数，再把它从全局上删掉
        let log = q::JS_NewCFunction2(
            ctx,
            Some(js_log),
            c"log".as_ptr(),
            2,
            q::JSCFunctionEnum_JS_CFUNC_generic,
            0,
        );
        let global = q::JS_GetGlobalObject(ctx);
        let set = q::JS_SetPropertyStr(ctx, global, c"__tw_log".as_ptr(), log);
        q::JS_FreeValue(ctx, global);
        if set < 0 {
            free(src as *mut c_void);
            return fail_exception();
        }

        q::JS_UpdateStackTop(rt);
        let bridge = q::JS_Eval(
            ctx,
            src as *const c_char,
            len as q::size_t,
            c"bridge.js".as_ptr(),
            (q::JS_EVAL_TYPE_GLOBAL | q::JS_EVAL_FLAG_STRICT) as c_int,
        );
        free(src as *mut c_void);
        if q::JS_IsException(bridge) {
            return fail_exception();
        }
        if !q::JS_IsObject(bridge) {
            q::JS_FreeValue(ctx, bridge);
            return fail(c"bridge.js did not evaluate to the bridge object");
        }
        BRIDGE = bridge;
        set_out_static(c"");
        0
    }
}

/// 把插件源码（以 NUL 结尾）编成模块字节码，不执行。0 成功，输出是字节码；
/// 1 失败（语法错误等），输出是错误描述。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tw_compile(src: *mut u8, len: usize) -> u32 {
    unsafe {
        q::JS_UpdateStackTop(RT);
        let module = q::JS_Eval(
            CTX,
            src as *const c_char,
            len as q::size_t,
            c"plugin.js".as_ptr(),
            (q::JS_EVAL_TYPE_MODULE | q::JS_EVAL_FLAG_STRICT | q::JS_EVAL_FLAG_COMPILE_ONLY)
                as c_int,
        );
        free(src as *mut c_void);
        if q::JS_IsException(module) {
            return fail_exception();
        }
        // 模块值不能 FreeValue（QuickJS 在那里 abort）：它归上下文的模块表管
        let mut size: q::size_t = 0;
        let buf = q::JS_WriteObject(
            CTX,
            &mut size,
            module,
            (q::JS_WRITE_OBJ_BYTECODE | q::JS_WRITE_OBJ_STRIP_SOURCE) as c_int,
        );
        if buf.is_null() {
            return fail_exception();
        }
        set_out(buf, size as usize, OUT_JS_MALLOC);
        0
    }
}

/// 读入字节码、求值模块顶层（跑完它排下的 Promise 任务），再让桥把钩子和清单
/// 取出来。0 成功，输出是桥给的 JSON；1 失败，输出是错误描述。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tw_load(bc: *mut u8, len: usize) -> u32 {
    unsafe {
        q::JS_UpdateStackTop(RT);
        // 不带 ROM_DATA：QuickJS 自己拷一份，这块输入马上就能放掉
        let module = q::JS_ReadObject(
            CTX,
            bc as *const u8,
            len as q::size_t,
            q::JS_READ_OBJ_BYTECODE as c_int,
        );
        free(bc as *mut c_void);
        if q::JS_IsException(module) {
            return fail_exception();
        }
        if q::JS_VALUE_GET_TAG(module) != q::JS_TAG_MODULE {
            return fail(c"the bytecode is not a module");
        }
        let def = q::JS_VALUE_GET_PTR(module) as *mut q::JSModuleDef;
        // JS_EvalFunction 会放掉传进去的那一份引用，模块自己要留着
        let promise = q::JS_EvalFunction(CTX, q::JS_DupValue(CTX, module));
        if q::JS_IsException(promise) {
            return fail_exception();
        }
        drain_jobs();
        if q::JS_IsPromise(promise) {
            let state = q::JS_PromiseState(CTX, promise);
            if state == q::JSPromiseStateEnum_JS_PROMISE_REJECTED {
                let err = q::JS_PromiseResult(CTX, promise);
                q::JS_FreeValue(CTX, promise);
                return fail_value(err);
            }
            if state == q::JSPromiseStateEnum_JS_PROMISE_PENDING {
                q::JS_FreeValue(CTX, promise);
                return fail(c"the module's top-level await never finished");
            }
        }
        q::JS_FreeValue(CTX, promise);

        let ns = q::JS_GetModuleNamespace(CTX, def);
        if q::JS_IsException(ns) {
            return fail_exception();
        }
        let mut args = [ns];
        let info = call_bridge(c"load", &mut args);
        q::JS_FreeValue(CTX, ns);
        if q::JS_IsException(info) {
            return fail_exception();
        }
        let ok = out_string(info);
        q::JS_FreeValue(CTX, info);
        if !ok {
            return fail_exception();
        }
        0
    }
}

/// 给这个实例的 `Math.random` 换种子。快照把随机数状态冻住了，每个实例都要换
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tw_seed(a: u32, b: u32, c: u32, d: u32) -> u32 {
    unsafe {
        let mut args = [int(a), int(b), int(c), int(d)];
        let r = call_bridge(c"seed", &mut args);
        if q::JS_IsException(r) {
            return fail_exception();
        }
        q::JS_FreeValue(CTX, r);
        0
    }
}

/// 设定这个实例的 `ctx`（JSON，桥把它解析出来并整个冻住）
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tw_set_ctx(json: *mut u8, len: usize) -> u32 {
    unsafe {
        let s = q::JS_NewStringLen(CTX, json as *const c_char, len as q::size_t);
        free(json as *mut c_void);
        if q::JS_IsException(s) {
            return fail_exception();
        }
        let mut args = [s];
        let r = call_bridge(c"setCtx", &mut args);
        q::JS_FreeValue(CTX, s);
        if q::JS_IsException(r) {
            return fail_exception();
        }
        q::JS_FreeValue(CTX, r);
        0
    }
}

/// 调一次钩子。`kind`：0 onRequest，1 onReplyText，2 onReplyTextEnd，3 onToolCall。
/// `input` 可以是空指针（onReplyTextEnd 没有输入）。
///
/// 输出总是「一位状态码 + 内容」，返回值也是那位状态码（见 `bridge.js`）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tw_call(kind: u32, input: *mut u8, len: usize) -> u32 {
    unsafe {
        q::JS_UpdateStackTop(RT);
        let arg = if input.is_null() {
            q::JS_UNDEFINED
        } else {
            let s = q::JS_NewStringLen(CTX, input as *const c_char, len as q::size_t);
            free(input as *mut c_void);
            s
        };
        if q::JS_IsException(arg) {
            return threw();
        }
        let mut args = [int(kind), arg];
        let r = call_bridge(c"call", &mut args);
        q::JS_FreeValue(CTX, arg);
        if q::JS_IsException(r) {
            return threw();
        }
        q::JS_FreeValue(CTX, r);
        // 钩子排下的 Promise 任务（async 钩子的后半段也在这里）都在这次调用里跑完
        drain_jobs();
        let mut args = [int(kind)];
        let s = call_bridge(c"settle", &mut args);
        if q::JS_IsException(s) {
            return threw();
        }
        let ok = out_string(s);
        q::JS_FreeValue(CTX, s);
        if !ok {
            return threw();
        }
        if OUT_LEN == 0 {
            return 3;
        }
        let status = *OUT_PTR;
        if status.is_ascii_digit() {
            (status - b'0') as u32
        } else {
            3
        }
    }
}

// ── 内部 ─────────────────────────────────────────────────────────

/// `console.*` 落到这里：`__tw_log(level, text)`
unsafe extern "C" fn js_log(
    ctx: *mut JSContext,
    _this: JSValue,
    argc: c_int,
    argv: *mut JSValue,
) -> JSValue {
    unsafe {
        if argc < 2 || argv.is_null() {
            return q::JS_UNDEFINED;
        }
        let mut level: i32 = 0;
        if q::JS_ToInt32(ctx, &mut level, *argv) < 0 {
            return q::JS_EXCEPTION;
        }
        let mut len: q::size_t = 0;
        let s = q::JS_ToCStringLen2(ctx, &mut len, *argv.add(1), false);
        if s.is_null() {
            return q::JS_EXCEPTION;
        }
        host_log(level as u32, s as *const u8, len as usize);
        q::JS_FreeCString(ctx, s);
        q::JS_UNDEFINED
    }
}

fn int(v: u32) -> JSValue {
    q::JS_MKVAL(q::JS_TAG_INT, v as i32)
}

unsafe fn call_bridge(name: &CStr, args: &mut [JSValue]) -> JSValue {
    unsafe {
        let f = q::JS_GetPropertyStr(CTX, BRIDGE, name.as_ptr());
        if q::JS_IsException(f) {
            return f;
        }
        let r = q::JS_Call(CTX, f, BRIDGE, args.len() as c_int, args.as_mut_ptr());
        q::JS_FreeValue(CTX, f);
        r
    }
}

/// 跑完所有排着的 Promise 任务。一条永不结束的任务链由宿主的 CPU 时间上限打断
unsafe fn drain_jobs() {
    unsafe {
        let mut job_ctx: *mut JSContext = null_mut();
        loop {
            let r = q::JS_ExecutePendingJob(RT, &mut job_ctx);
            if r == 0 {
                break;
            }
            if r < 0 && !job_ctx.is_null() {
                // 某个任务抛了异常（没人接的 Promise 拒绝）。它留在上下文上，清掉
                let e = q::JS_GetException(job_ctx);
                q::JS_FreeValue(job_ctx, e);
            }
        }
    }
}

unsafe fn release_out() {
    unsafe {
        match OUT_KIND {
            OUT_CSTRING => q::JS_FreeCString(CTX, OUT_PTR as *const c_char),
            OUT_JS_MALLOC => q::js_free(CTX, OUT_PTR as *mut c_void),
            _ => {}
        }
        OUT_PTR = core::ptr::null();
        OUT_LEN = 0;
        OUT_KIND = OUT_NONE;
    }
}

unsafe fn set_out(ptr: *const u8, len: usize, kind: u8) {
    unsafe {
        release_out();
        OUT_PTR = ptr;
        OUT_LEN = len;
        OUT_KIND = kind;
    }
}

unsafe fn set_out_static(s: &'static CStr) {
    unsafe { set_out(s.as_ptr() as *const u8, s.count_bytes(), OUT_STATIC) }
}

/// 把一个 JS 字符串转成 UTF-8 放进输出。失败（内存不够）时异常留在上下文上
unsafe fn out_string(v: JSValue) -> bool {
    unsafe {
        let mut len: q::size_t = 0;
        let s = q::JS_ToCStringLen2(CTX, &mut len, v, false);
        if s.is_null() {
            return false;
        }
        set_out(s as *const u8, len as usize, OUT_CSTRING);
        true
    }
}

/// 一个 JS 值（通常是异常）交给桥描述成 `{"message","stack"}`
unsafe fn describe_into_out(err: JSValue) {
    unsafe {
        let mut args = [err];
        let d = call_bridge(c"describe", &mut args);
        if q::JS_IsException(d) {
            let e = q::JS_GetException(CTX);
            q::JS_FreeValue(CTX, e);
            set_out_static(c"{\"message\":\"the plugin failed and the error could not be described\",\"stack\":null}");
            return;
        }
        if !out_string(d) {
            let e = q::JS_GetException(CTX);
            q::JS_FreeValue(CTX, e);
            set_out_static(c"{\"message\":\"out of memory\",\"stack\":null}");
        }
        q::JS_FreeValue(CTX, d);
    }
}

unsafe fn fail_exception() -> u32 {
    unsafe {
        let e = q::JS_GetException(CTX);
        fail_value(e)
    }
}

unsafe fn fail_value(err: JSValue) -> u32 {
    unsafe {
        describe_into_out(err);
        q::JS_FreeValue(CTX, err);
        1
    }
}

unsafe fn fail(msg: &'static CStr) -> u32 {
    unsafe {
        if CTX.is_null() || !q::JS_IsObject(BRIDGE) {
            set_out_static(msg);
            return 1;
        }
        let s = q::JS_NewStringLen(CTX, msg.as_ptr(), msg.count_bytes() as q::size_t);
        fail_value(s)
    }
}

/// 桥本身没走完（多半是内存耗尽）：状态码 3（抛出），内容是错误描述
unsafe fn threw() -> u32 {
    unsafe {
        let e = q::JS_GetException(CTX);
        let mut args = [e];
        let d = call_bridge(c"describeThrown", &mut args);
        q::JS_FreeValue(CTX, e);
        if q::JS_IsException(d) || !out_string(d) {
            let e = q::JS_GetException(CTX);
            q::JS_FreeValue(CTX, e);
            set_out_static(c"3{\"message\":\"out of memory\",\"stack\":null}");
        }
        q::JS_FreeValue(CTX, d);
        3
    }
}
