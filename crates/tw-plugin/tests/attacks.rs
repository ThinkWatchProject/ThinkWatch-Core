//! 资源耗尽与坏返回值（I4）：`tests/corpus/` 里一个文件一种攻击。
//!
//! 每一条都证明三件事：攻击失败得干净（是一个 `RunError`，不是 panic，也不是
//! 挂住）；失败在上限之内（墙钟时间有界，见 `common::BOUND`）；不影响下一次
//! 调用（同一个插件再跑一次失败方式相同，无害的插件照常工作）。

mod common;

use std::time::Duration;

use common::*;
use serde_json::{Value, json};
use tw_plugin::{Limits, RequestOutcome, RunError, Runtime, ToolCallOutcome};

fn kind(k: &str) -> Value {
    json!({ "kind": k })
}

// ── CPU ─────────────────────────────────────────────────────────

#[test]
fn an_endless_loop_hits_the_cpu_limit() {
    let p = load("cpu-loop");
    let e = request_err(&p, json!({}));
    assert!(matches!(e, RunError::CpuLimit), "{e:?}");
    after_attack(&p, json!({}), &e);
}

#[test]
fn burning_cpu_through_recursion_alone_hits_the_cpu_limit() {
    let p = load("cpu-recursion");
    let e = request_err(&p, json!({}));
    assert!(matches!(e, RunError::CpuLimit), "{e:?}");
    after_attack(&p, json!({}), &e);
}

#[test]
fn catastrophic_backtracking_inside_the_regex_engine_hits_the_cpu_limit() {
    let p = load("cpu-regex");
    let e = request_err(&p, json!({}));
    assert!(matches!(e, RunError::CpuLimit), "{e:?}");
    after_attack(&p, json!({}), &e);
}

#[test]
fn the_reported_cpu_time_stays_near_the_limit() {
    // 中止得及时：报出来的 CPU 时间不会远超上限
    let p = load("cpu-loop");
    let inv = request(&p, json!({}));
    assert!(matches!(inv.result, Err(RunError::CpuLimit)));
    let limit = Limits::default().request_cpu;
    assert!(
        inv.cpu < limit * 5,
        "stopped only after {:?} of CPU (limit {limit:?})",
        inv.cpu
    );
}

// ── 内存 ────────────────────────────────────────────────────────

#[test]
fn a_memory_bomb_hits_the_memory_limit() {
    let p = load_roomy("mem-bomb");
    let e = request_err(&p, kind("buffers"));
    assert!(matches!(e, RunError::MemoryLimit), "{e:?}");
    after_attack(&p, kind("buffers"), &e);
}

#[test]
fn a_memory_bomb_that_is_slow_to_grow_is_stopped_by_one_limit_or_the_other() {
    // 字符串翻倍：引擎可能用绳索串接，长度先撞上它自己的上限（string too long），
    // 也可能先用完内存或 CPU 时间。哪一道先到都行，不能是跑完
    let p = load_roomy("mem-bomb");
    let e = request_err(&p, kind("strings"));
    assert!(
        matches!(
            e,
            RunError::MemoryLimit | RunError::CpuLimit | RunError::Threw { .. }
        ),
        "{e:?}"
    );
    after_attack(&p, kind("strings"), &e);
}

#[test]
fn one_huge_allocation_is_refused() {
    // 引擎可能在分配之前就拒绝（RangeError），也可能分配到一半撞上限 —— 两种都是
    // 干净的失败。不允许的是分配成功
    for k in ["arraybuffer", "array", "string"] {
        let p = load_roomy("mem-single");
        let e = request_err(&p, kind(k));
        // 填两亿个元素的数组可能先撞上 CPU 上限
        assert!(
            matches!(
                e,
                RunError::MemoryLimit | RunError::Threw { .. } | RunError::CpuLimit
            ),
            "{k}: {e:?}"
        );
        after_attack(&p, kind(k), &e);
    }
}

// ── 栈 ──────────────────────────────────────────────────────────

#[test]
fn endless_recursion_fails_without_taking_the_host_down() {
    let p = load("stack-js");
    let e = request_err(&p, json!({}));
    assert!(
        matches!(e, RunError::Threw { .. } | RunError::Trap(_)),
        "{e:?}"
    );
    after_attack(&p, json!({}), &e);
}

#[test]
fn deep_recursion_inside_the_engine_fails_without_taking_the_host_down() {
    // 耗尽的是 WebAssembly 的栈（引擎的 C 代码在递归），不是 JS 的调用栈
    for k in ["parse", "stringify"] {
        let p = load_roomy("stack-native");
        let e = request_err(&p, kind(k));
        assert!(
            matches!(
                e,
                RunError::Threw { .. }
                    | RunError::Trap(_)
                    | RunError::MemoryLimit
                    | RunError::CpuLimit
            ),
            "{k}: {e:?}"
        );
        after_attack(&p, kind(k), &e);
    }
}

// ── 输出 ────────────────────────────────────────────────────────

#[test]
fn an_output_far_larger_than_the_input_hits_the_output_limit() {
    let p = load_roomy("out-giant");
    let e = request_err(&p, json!({}));
    assert!(matches!(e, RunError::OutputLimit), "{e:?}");
    after_attack(&p, json!({}), &e);
}

#[test]
fn a_tojson_that_never_returns_is_stopped_by_the_cpu_limit() {
    // 序列化返回值也在 CPU 上限之内
    let p = load("out-tojson-loop");
    let e = request_err(&p, json!({}));
    assert!(matches!(e, RunError::CpuLimit), "{e:?}");
    after_attack(&p, json!({}), &e);
}

#[test]
fn a_getter_that_never_returns_is_stopped_by_the_cpu_limit() {
    let p = load("out-getter-loop");
    let e = request_err(&p, json!({}));
    assert!(matches!(e, RunError::CpuLimit), "{e:?}");
    after_attack(&p, json!({}), &e);
}

#[test]
fn a_proxy_cannot_hand_the_host_a_value_that_shifts_under_it() {
    // 读属性就抛错、列键就死循环的 Proxy：干净地失败
    for (k, cpu) in [("throw", false), ("loop", true)] {
        let p = load("out-proxy");
        let e = request_err(&p, kind(k));
        if cpu {
            assert!(matches!(e, RunError::CpuLimit), "{k}: {e:?}");
        } else {
            assert!(
                matches!(e, RunError::BadOutput(_) | RunError::Threw { .. }),
                "{k}: {e:?}"
            );
        }
        after_attack(&p, kind(k), &e);
    }
    // 每次读到不同值的 Proxy：宿主拿到的是**一次**序列化的结果，一个前后一致的
    // JSON 对象
    let p = load("out-proxy");
    match request(&p, kind("shifting")).result {
        Ok(RequestOutcome::Changed(v)) => {
            assert!(v.is_object(), "{v}");
            let s = v["system"].as_str().unwrap_or_default();
            assert!(s.starts_with("第 ") && s.ends_with(" 次读取"), "{v}");
        }
        Err(e) => assert!(matches!(e, RunError::BadOutput(_)), "{e:?}"),
        other => panic!("{other:?}"),
    }
    still_fine();
}

#[test]
fn a_cyclic_value_is_bad_output() {
    let p = load("out-cyclic");
    let e = request_err(&p, json!({}));
    assert!(matches!(e, RunError::BadOutput(_)), "{e:?}");
    after_attack(&p, json!({}), &e);
}

#[test]
fn a_deeply_nested_value_does_not_overflow_the_host_stack() {
    // 宿主解析一个嵌套五千层的 JSON：栈溢出就是整个 core 进程崩溃。要么序列化
    // 那一步在沙箱里失败，要么宿主的解析器拒绝它；成功也可以，但宿主得活着
    let p = load_roomy("out-deep");
    match request(&p, json!({})).result {
        Err(e) => assert!(
            matches!(
                e,
                RunError::BadOutput(_) | RunError::Trap(_) | RunError::Threw { .. }
            ),
            "{e:?}"
        ),
        Ok(o) => {
            // 能交回来的话，宿主手里的值也要能安全地丢掉（Drop 也是递归的）
            drop(o);
        }
    }
    still_fine();
}

#[test]
fn a_request_hook_must_return_the_request_or_nothing() {
    for k in [
        "number", "string", "boolean", "function", "symbol", "bigint", "array", "null",
    ] {
        let p = load("out-wrong-type");
        let e = request_err(&p, kind(k));
        assert!(matches!(e, RunError::BadOutput(_)), "{k}: {e:?}");
    }
    still_fine();
}

#[test]
fn a_promise_from_a_request_hook_is_awaited_or_refused_never_leaked() {
    // 钩子返回 Promise：要么由运行时等它落定（值照常核对），要么当作坏输出。
    // 不允许的是把 Promise 本身当成「请求」交回来
    let p = load("out-wrong-type");
    match request(&p, kind("promise")).result {
        // 落定的值就是传进去的那份视图
        Ok(RequestOutcome::Unchanged) => {}
        Ok(RequestOutcome::Changed(v)) => assert_eq!(v["system"], "你是助手。", "{v}"),
        Err(e) => assert!(matches!(e, RunError::BadOutput(_)), "{e:?}"),
        other => panic!("{other:?}"),
    }
    still_fine();
}

// ── 异常 ────────────────────────────────────────────────────────

#[test]
fn throwing_something_that_is_not_an_error_still_gives_a_readable_message() {
    for k in [
        "string",
        "number",
        "null",
        "undefined",
        "object",
        "symbol",
        "proxy",
    ] {
        let p = load("throw-values");
        match request_err(&p, kind(k)) {
            RunError::Threw { message, .. } => {
                assert!(!message.is_empty(), "{k}: empty message");
                assert!(message.len() <= 64 * 1024, "{k}: {} bytes", message.len());
            }
            e => panic!("{k}: expected Threw, got {e:?}"),
        }
    }
    still_fine();
}

#[test]
fn an_error_whose_message_never_finishes_is_still_bounded() {
    // 把异常变成文字时会调用插件的代码（getter、toString）：那段代码也在上限之内
    for k in ["tostring-loop", "message-getter-loop"] {
        let p = load("throw-values");
        let e = request_err(&p, kind(k));
        assert!(
            matches!(e, RunError::CpuLimit | RunError::Threw { .. }),
            "{k}: {e:?}"
        );
        after_attack(&p, kind(k), &e);
    }
}

#[test]
fn a_huge_error_message_is_cut_short() {
    // 错误信息会进请求记录、通知和给客户端的错误：16 MiB 的消息不能原样流出去
    let p = load_roomy("throw-values");
    match request_err(&p, kind("huge-message")) {
        RunError::Threw { message, stack } => {
            assert!(message.len() <= 64 * 1024, "{} bytes", message.len());
            if let Some(s) = stack {
                assert!(s.len() <= 64 * 1024, "stack: {} bytes", s.len());
            }
        }
        RunError::MemoryLimit | RunError::OutputLimit => {}
        e => panic!("{e:?}"),
    }
    still_fine();
}

#[test]
fn a_microtask_left_behind_cannot_run_outside_the_limits() {
    // 钩子返回之后留下一个死循环的微任务：要么不执行，要么在上限之内被中止
    let p = load("async-hooks");
    match request(&p, kind("microtask")).result {
        Ok(RequestOutcome::Unchanged) | Err(RunError::CpuLimit) => {}
        other => panic!("{other:?}"),
    }
    still_fine();
}

#[test]
fn an_async_request_hook_is_awaited_or_refused() {
    let p = load("async-hooks");
    match request(&p, kind("async")).result {
        Ok(RequestOutcome::Unchanged) => {}
        Ok(RequestOutcome::Changed(v)) => assert_eq!(v["system"], "你是助手。", "{v}"),
        Err(e) => assert!(matches!(e, RunError::BadOutput(_)), "{e:?}"),
        other => panic!("{other:?}"),
    }
}

// ── 日志 ────────────────────────────────────────────────────────

#[test]
fn a_log_flood_stays_within_the_log_limits() {
    let limits = Limits::default();
    for k in ["lines", "long-line", "cyclic", "getter-loop"] {
        // getter 死循环要靠 CPU 上限停下；另外三种测的是日志的上限
        let p = if k == "getter-loop" {
            load("log-flood")
        } else {
            load_roomy("log-flood")
        };
        let inv = request(&p, kind(k));
        // 超出日志上限可以是错误（I4），也可以是截断；**不能**是原样收下
        assert!(
            inv.logs.len() <= limits.max_log_lines + 1,
            "{k}: {} lines kept",
            inv.logs.len()
        );
        for l in &inv.logs {
            assert!(
                l.text.len() <= limits.max_log_line + 64,
                "{k}: a {}-byte line was kept",
                l.text.len()
            );
        }
        if k == "getter-loop" {
            assert!(
                matches!(
                    inv.result,
                    Ok(RequestOutcome::Unchanged) | Err(RunError::CpuLimit)
                ),
                "{k}: {:?}",
                inv.result
            );
        }
    }
    still_fine();
}

// ── 回答钩子 ────────────────────────────────────────────────────

#[test]
fn holding_back_a_reply_and_releasing_it_inflated_hits_the_output_limit() {
    let p = load_roomy("reply-hoard");
    let mut r = reply(&p, json!({}));
    for _ in 0..8 {
        match text(&mut r, "一段回答。").result {
            Ok(Some(s)) => assert_eq!(s, ""),
            other => panic!("{other:?}"),
        }
    }
    let e = text_end(&mut r)
        .result
        .expect_err("65536 copies of the held text came out");
    assert!(matches!(e, RunError::OutputLimit), "{e:?}");
    // 下一个回答是新实例，照常工作
    let mut r = reply(&p, json!({}));
    assert!(matches!(text(&mut r, "x").result, Ok(Some(_))));
}

#[test]
fn a_reply_hook_over_its_per_call_cpu_limit_is_stopped() {
    let p = load("reply-slow");
    let mut r = reply(&p, kind("over-call"));
    let e = text(&mut r, "一段")
        .result
        .expect_err("an endless reply hook returned");
    assert!(matches!(e, RunError::CpuLimit), "{e:?}");
}

#[test]
fn many_cheap_reply_calls_hit_the_limit_for_the_whole_reply() {
    // 单次的上限放宽到两秒、整条回答只给 300 毫秒：每段一份固定的计算，单次远远不到，
    // 累计到了就该停，之前的调用照常。上限是另配的，测的是「累计」这一道本身，不受机器
    // 快慢影响
    let rt = Runtime::new(Limits {
        reply_call_cpu: Duration::from_secs(2),
        reply_total_cpu: Duration::from_millis(300),
        ..Limits::default()
    })
    .unwrap();
    let p = rt.load(&corpus("reply-slow")).unwrap();
    let mut r = reply(&p, kind("under-call"));
    let mut ok = 0;
    let mut stopped = None;
    for _ in 0..2000 {
        match text(&mut r, "一段").result {
            Ok(_) => ok += 1,
            Err(e) => {
                stopped = Some(e);
                break;
            }
        }
    }
    let e = stopped.unwrap_or_else(|| panic!("{ok} calls all passed"));
    assert!(matches!(e, RunError::CpuLimit), "{e:?}");
    assert!(ok >= 2, "stopped after only {ok} calls");
}

#[test]
fn a_reply_instance_that_failed_keeps_failing_instead_of_resuming() {
    // 一个被中止的实例里，引擎的状态可能停在半路：之后的调用不能当作什么都没发生
    let p = load("reply-slow");
    let mut r = reply(&p, kind("over-call"));
    assert!(text(&mut r, "一段").result.is_err());
    assert!(
        text(&mut r, "再一段").result.is_err(),
        "the reply instance kept running after it was stopped"
    );
}

#[test]
fn replacing_one_tool_call_with_two_hundred_thousand_fails() {
    let p = load_roomy("toolcall-flood");
    let mut r = reply(&p, json!({}));
    let inv = tool_call(
        &mut r,
        json!({ "id": "toolu_1", "name": "Read", "input": { "file_path": "/tmp/a" } }),
    );
    match inv.result {
        Err(
            RunError::OutputLimit
            | RunError::BadOutput(_)
            | RunError::MemoryLimit
            | RunError::CpuLimit,
        ) => {}
        Ok(ToolCallOutcome::Replace(calls)) => {
            panic!("{} tool calls came out of one", calls.len())
        }
        other => panic!("{other:?}"),
    }
}
