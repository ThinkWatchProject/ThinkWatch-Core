//! 正在服务中的请求有几个。
//!
//! 数的是**客户端那一端还在等**的请求：从进入数据面起，到响应体的最后一
//! 个字节发完、或者客户端先走了为止。
//!
//! **不能数到 handler 返回为止。**流式响应在 handler 返回之后才真正开始
//! —— 那样数的话，一个正在吐字的六分钟任务在第一个字节发出去时就已经
//! 「结束」了。所以通行证跟着响应体走：体被丢掉的那一刻才还回来，而
//! 客户端中途断开时 hyper 丢掉的正是这个体。
//!
//! 它存在的理由是桌面版的更新：换版本要重启网关，重启会掐断所有还在流式
//! 输出的请求。等这个数归零再重启，用户手上跑着的任务就不会断在半截。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 计数器本身。克隆出来的都指向同一个数。
///
/// **跨重载存活**（它挂在 `AppState` 上，不在 `Runtime` 里）：改一条规则
/// 不会让正在跑的请求从这个数里消失。
#[derive(Clone, Default)]
pub struct Live(Arc<AtomicUsize>);

/// 一个请求的通行证。**Drop 时自动减一** —— 手工减的话，迟早有一条错误
/// 路径会漏掉，而漏掉的表现是这个数只增不减，等它归零的人永远等不到。
#[must_use = "丢掉通行证就等于这个请求已经结束"]
pub struct Pass(Arc<AtomicUsize>);

impl Live {
    pub fn enter(&self) -> Pass {
        self.0.fetch_add(1, Ordering::SeqCst);
        Pass(self.0.clone())
    }

    pub fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

impl Drop for Pass {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pass_counts_while_it_is_held() {
        let live = Live::default();
        assert_eq!(live.count(), 0);
        let a = live.enter();
        let b = live.clone().enter();
        assert_eq!(live.count(), 2, "克隆出来的计数器数的是同一个数");
        drop(a);
        assert_eq!(live.count(), 1);
        drop(b);
        assert_eq!(live.count(), 0);
    }

    /// 通行证可能在任何线程上被丢掉 —— 响应体在哪个线程上结束，由运行时
    /// 决定，不由我们决定。
    #[test]
    fn a_pass_dropped_on_another_thread_still_counts_down() {
        let live = Live::default();
        let pass = live.enter();
        std::thread::spawn(move || drop(pass)).join().unwrap();
        assert_eq!(live.count(), 0);
    }
}
