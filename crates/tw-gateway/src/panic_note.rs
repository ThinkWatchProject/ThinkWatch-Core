//! 网关的代码 panic 了：记下是哪一行、为什么，交给那个请求的结局。
//!
//! 一个请求的流在 poll 里 panic 时，栈展开把它的 [`crate::ending::Ending`] 丢掉，结局只能
//! 由 Drop 补报一句「被网关内部的错误中断」。panic 的说明默认只打到 stderr，而桌面版起
//! core 时 stderr 没有人接 —— 那一句就成了唯一的线索，排查的人看不出是哪儿坏了。
//!
//! 所以装一个 panic 钩子：先把说明和代码位置记在**当前线程**上（Drop 在同一个线程的
//! 栈展开里跑，取得到），再写一条日志，最后照旧交给原来的钩子。
//!
//! **说明里不留数据**：标准库的一些 panic 会把出错的那段数据原样印进去 —— 切字符串
//! 切错了位置时是 ``of `整段字符串` ``，`unwrap` 一个错误时是 `value: 错误的内容` ——
//! 而那可能是请求里的密钥。这两段去掉，再截到 [`MAX`] 个字符。

use std::cell::RefCell;

/// 记下的说明最多几个字符
const MAX: usize = 200;

thread_local! {
    static LAST: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// 装上钩子。进程里装一次（`twcore serve` 开头）；再装一次也只是多包一层。
pub fn install() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let note = describe(info);
        // **钩子里什么都不能 panic**：借不到就不记
        let _ = LAST.try_with(|l| {
            if let Ok(mut l) = l.try_borrow_mut() {
                *l = Some(note.clone());
            }
        });
        tracing::error!(panic = %note, "the gateway panicked");
        previous(info);
    }));
}

/// 这个线程上最近一次 panic 的说明，取走。没装钩子、没 panic 过是 `None`
pub fn take() -> Option<String> {
    LAST.try_with(|l| l.try_borrow_mut().ok().and_then(|mut l| l.take()))
        .ok()
        .flatten()
}

/// `说明（在 文件:行:列）`
fn describe(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("a panic without a message");
    let message = clean(message);
    match info.location() {
        Some(at) => format!("{message} (at {}:{}:{})", at.file(), at.line(), at.column()),
        None => message,
    }
}

/// 去掉标准库印进去的数据（`` of `…` ``、`value: …`），截到 [`MAX`] 个字符
fn clean(message: &str) -> String {
    let mut head = message;
    if let Some(i) = head.find(" of `") {
        head = &head[..i];
    }
    if let Some(i) = head.find(" value: ") {
        head = &head[..i + " value".len()];
    }
    let head = head.trim();
    let mut out: String = head.chars().take(MAX).collect();
    if head.chars().count() > MAX {
        out.push('…');
    }
    if out.is_empty() {
        out.push_str("a panic without a message");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_follows_a_backtick_is_left_out() {
        assert_eq!(
            clean(
                "byte index 5 is not a char boundary; it is inside '中' (bytes 4..7) of `sk-ant-secret中文`"
            ),
            "byte index 5 is not a char boundary; it is inside '中' (bytes 4..7)"
        );
        assert_eq!(
            clean("called `Result::unwrap()` on an `Err` value: Secret(\"x\")"),
            "called `Result::unwrap()` on an `Err` value"
        );
        assert_eq!(
            clean("index out of bounds: the len is 3 but the index is 7"),
            "index out of bounds: the len is 3 but the index is 7"
        );
        assert_eq!(clean(""), "a panic without a message");
        let long = "x".repeat(500);
        assert_eq!(clean(&long).chars().count(), MAX + 1);
    }

    #[test]
    fn a_panic_on_this_thread_is_noted_with_where_it_happened() {
        install();
        let r = std::panic::catch_unwind(|| {
            let s = String::from("中文");
            let n = s.len() - 1;
            let _ = &s[..n];
        });
        assert!(r.is_err());
        let note = take().expect("noted");
        assert!(
            note.contains("byte index 5 is not a char boundary"),
            "{note}"
        );
        assert!(!note.contains("中文"), "{note}");
        assert!(note.contains("panic_note.rs:"), "{note}");
        assert_eq!(take(), None, "taken once");
    }
}
