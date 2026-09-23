//! 请网关退出。
//!
//! # 为什么这是控制面上的一条请求，而不是一个信号
//!
//! **Windows 上没有 SIGTERM。**而桌面端有两件事非做不可：改完配置之后重启
//! core，以及装更新之前停掉它并且**等它真的退出**（不等的话，新起来的那个
//! 会撞上旧的还没放开的锁）。那个平台上能用的只剩强杀，而强杀意味着 SQLite
//! 的 WAL 没有收尾、在途的请求直接断在半路。
//!
//! 所以做成一条请求。它比信号还好一点：**有应答**，桌面端因此知道对方收到了，
//! 而不是发完信号去猜。
//!
//! 两个平台都走它。只在一个平台上生效的路径，是没人日常测的路径。

use std::sync::Arc;

/// 一个「该退了」的开关。
#[derive(Clone, Default)]
pub struct Shutdown(Arc<tokio::sync::Notify>);

impl Shutdown {
    /// 请它退出。
    ///
    /// **用 `notify_one` 而不是 `notify_waiters`。**前者在还没有人等的时候
    /// 会把这一次记下来，后者直接丢掉 —— 而「请求先到、主循环才开始等」
    /// 这个顺序在启动那几毫秒里是可能的，丢掉的后果是一个永远不退的进程。
    pub fn ask(&self) {
        self.0.notify_one();
    }

    /// 等到有人请它退出。
    pub async fn asked(&self) {
        self.0.notified().await;
    }
}

impl std::fmt::Debug for Shutdown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Shutdown")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn asking_wakes_the_waiter() {
        let s = Shutdown::default();
        let w = s.clone();
        let h = tokio::spawn(async move { w.asked().await });
        s.ask();
        tokio::time::timeout(std::time::Duration::from_secs(5), h)
            .await
            .expect("请求没把等的人叫醒")
            .unwrap();
    }

    /// 请求先到、主循环后开始等 —— 启动那几毫秒里真的可能这样。
    /// 丢掉这一次的后果是一个永远不退的进程。
    #[tokio::test]
    async fn a_request_that_arrives_before_anyone_waits_is_not_lost() {
        let s = Shutdown::default();
        s.ask();
        tokio::time::timeout(std::time::Duration::from_secs(5), s.asked())
            .await
            .expect("先到的那次请求被丢了");
    }
}
