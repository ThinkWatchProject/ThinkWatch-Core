//! 手动中止在跑的请求（控制面的 `POST /request/{id}/abort`、`POST /sessions/{id}/abort`）。
//!
//! 每个请求进门时带一个开关（[`Switch`]），发出开始事件、有了请求号时登记在这里（见
//! [`Aborts::enter`]）。叫停就是扳开关：在等上游的那几处（发出去等响应头、等开头、等空位）
//! 和交回答的那个循环都看着它，扳了就**丢掉和上游的那条连接**（上游不再生成），给客户端
//! 回一个它那种格式的错误，请求记成手动中止（[`tw_api::FailureSource::Aborted`]）。
//!
//! **不是上游的错**：那一家不停用、不算失败，快慢样本也不记。
//!
//! 登记跟着请求的结局走（挂在 [`crate::ending::Ending`] 上）：结局报了、或者被丢掉了，登记
//! 就没了。所以「在不在这张表里」就是「这个请求还在不在跑」，叫停一个已经结束的请求得到
//! 的是「没有在跑」，而不是一次什么都没发生的成功。**扳过开关之后被丢掉的结局报手动中止**，
//! 不报客户端取消：停在看不着开关的地方（取凭据、跑插件）时，是外面那一层把整个请求丢掉的。
//!
//! WebSocket 那条路不登记：一轮回答跑在一条长连接上，叫停一轮要连带处置整条连接，这一版
//! 不做。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

/// 一个请求的中止开关。克隆出来的是同一个。
#[derive(Clone, Default)]
pub struct Switch(Arc<Inner>);

#[derive(Default)]
struct Inner {
    thrown: AtomicBool,
    notify: Notify,
}

impl Switch {
    /// 扳下去：等着它的都醒过来，之后再等的立刻返回。扳过一次再扳什么都不变
    pub fn throw(&self) {
        self.0.thrown.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    /// 扳过了没有
    pub fn thrown(&self) -> bool {
        self.0.thrown.load(Ordering::SeqCst)
    }

    /// 等它被扳下去。**没人扳就一直等**：放在 `select!` 里和真正要等的东西并排
    pub async fn wait(&self) {
        loop {
            let notified = self.0.notify.notified();
            let mut notified = std::pin::pin!(notified);
            // 先登记再看：看完和开始等之间扳下去的，`notify_waiters` 也叫得醒它
            notified.as_mut().enable();
            if self.thrown() {
                return;
            }
            notified.await;
        }
    }
}

/// 一个登记着的请求。
struct Entry {
    /// 它属于哪次会话（开始事件里的那个）。认不出会话的没有
    session: Option<String>,
    switch: Switch,
}

/// 此刻在跑、可以叫停的请求。**跨重载存活**（挂在 `AppState` 上）：改一条规则不该让
/// 在跑的请求叫不停。
#[derive(Default)]
pub struct Aborts {
    entries: Mutex<HashMap<u64, Entry>>,
}

/// 登记。**丢掉它就是注销**：跟着请求的结局走，结局报了、请求被丢掉了，它跟着没了。
#[must_use = "dropping the registration takes the request off the list"]
pub struct Registered {
    aborts: Arc<Aborts>,
    id: u64,
    switch: Switch,
}

impl Registered {
    /// 这个请求被叫停了没有
    pub fn thrown(&self) -> bool {
        self.switch.thrown()
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        self.aborts.lock().remove(&self.id);
    }
}

/// 要叫停的不在跑（从没有过、已经结束了）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotRunning;

impl Aborts {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Entry>> {
        // 锁中毒了照样拿里面的表：叫不停一个请求，好过一个不转发的网关
        self.entries.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 请求 `id`（会话 `session`）开始了，用 `switch` 叫停它。
    pub fn enter(self: &Arc<Self>, id: u64, session: Option<String>, switch: Switch) -> Registered {
        self.lock().insert(
            id,
            Entry {
                session,
                switch: switch.clone(),
            },
        );
        Registered {
            aborts: Arc::clone(self),
            id,
            switch,
        }
    }

    /// 叫停请求 `id`。
    pub fn request(&self, id: u64) -> Result<(), NotRunning> {
        let map = self.lock();
        let e = map.get(&id).ok_or(NotRunning)?;
        e.switch.throw();
        Ok(())
    }

    /// 叫停会话 `session` 里所有在跑的请求，交回它们的号，从小到大（也就是开始的先后）。
    /// 一个都没有是空的
    pub fn session(&self, session: &str) -> Vec<u64> {
        let map = self.lock();
        let mut ids: Vec<u64> = map
            .iter()
            .filter(|(_, e)| e.session.as_deref() == Some(session))
            .map(|(id, e)| {
                e.switch.throw();
                *id
            })
            .collect();
        ids.sort_unstable();
        ids
    }

    /// 此刻登记着几个
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// 一个都没有
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn a_thrown_switch_wakes_who_waits_and_whoever_comes_later() {
        let s = Switch::default();
        let waiting = tokio::spawn({
            let s = s.clone();
            async move { s.wait().await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiting.is_finished());
        s.throw();
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("扳下去要叫醒在等的")
            .unwrap();
        // 扳过之后再等的立刻返回
        tokio::time::timeout(Duration::from_millis(50), s.wait())
            .await
            .expect("扳过的开关不用再等");
    }

    #[test]
    fn only_what_is_running_can_be_aborted_and_a_session_takes_all_of_its_own() {
        let aborts = Arc::new(Aborts::default());
        let (a, b, c) = (Switch::default(), Switch::default(), Switch::default());
        let ra = aborts.enter(7, Some("s1".into()), a.clone());
        let _rb = aborts.enter(3, Some("s1".into()), b.clone());
        let _rc = aborts.enter(9, None, c.clone());
        assert_eq!(aborts.len(), 3);

        assert_eq!(aborts.request(42), Err(NotRunning));
        assert_eq!(aborts.session("s1"), vec![3, 7]);
        assert!(a.thrown() && b.thrown() && !c.thrown());
        assert!(aborts.session("nope").is_empty());

        // 结局报了、登记丢掉了：再叫停就是不在跑
        drop(ra);
        assert_eq!(aborts.request(7), Err(NotRunning));
        assert_eq!(aborts.request(9), Ok(()));
        assert!(c.thrown());
    }
}
