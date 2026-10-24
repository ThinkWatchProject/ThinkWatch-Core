//! 旧 core 往交接连接上报变化的那一路（[`Feed`]），和名额的计数（[`Claims`]）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::mpsc;

use super::wire::Event;

/// 导出快照之后、排空开始之前最多攒多少条。
///
/// **攒着是为了排空开始时一条不漏**：快照是接管一开始导出的，之后到提交那一刻，旧 core 照常
/// 转发，用量上限的预留和结算还在变。交接正常时这一段不过一两秒；交接失败退回待交接、又迟迟
/// 没有下一个接替者时，攒下的东西没人要 —— 到了这个数就不攒了，排空开始时改发一份此刻占着的
/// 全部（`*_holds`），这一段的用量等新 core 打开请求库后从记录里重建
const PENDING_MAX: usize = 20_000;

/// 一路结算。**发不阻塞**：持着各自的锁发（见各个调用处），同一样东西的变化按发生的先后进来。
pub(crate) struct Feed {
    tx: mpsc::UnboundedSender<serde_json::Value>,
    /// 排空开始之前攒了几条
    pending: AtomicUsize,
    /// 排空开始了：接收端有人在读，不再数
    draining: AtomicBool,
    /// 攒满了，后面的不再攒
    lost: AtomicBool,
}

impl std::fmt::Debug for Feed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Feed")
            .field("draining", &self.draining.load(Ordering::Relaxed))
            .field("lost", &self.lost.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Feed {
    pub(crate) fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<serde_json::Value>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let feed = Arc::new(Self {
            tx,
            pending: AtomicUsize::new(0),
            draining: AtomicBool::new(false),
            lost: AtomicBool::new(false),
        });
        (feed, rx)
    }

    pub(crate) fn send(&self, ev: &Event) {
        if self.lost.load(Ordering::Relaxed) {
            return;
        }
        if !self.draining.load(Ordering::Relaxed)
            && self.pending.fetch_add(1, Ordering::Relaxed) >= PENDING_MAX
        {
            self.lost.store(true, Ordering::Relaxed);
            return;
        }
        // 接收端没了（交接连接断了）：发不出去就算了，没有人再要它
        let _ = self.tx.send(ev.to_value());
    }

    pub(crate) fn start_draining(&self) {
        self.draining.store(true, Ordering::Relaxed);
    }

    pub(crate) fn lost(&self) -> bool {
        self.lost.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn mark_lost(&self) {
        self.lost.store(true, Ordering::Relaxed);
    }
}

/// 发到这一路上（没接上就什么都不做）
pub(crate) fn send(feed: &Option<Arc<Feed>>, ev: impl FnOnce() -> Event) {
    if let Some(f) = feed {
        f.send(&ev());
    }
}

/// 名额是密钥的还是上游的
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Kind {
    #[default]
    Gate,
    Slot,
}

/// 每个名字此刻占着几个名额（密钥的连排着队等的也算）。导出快照、排空开始时报的就是它；
/// 排空开始之后每变一次报一次（[`Event::Gate`]、[`Event::Slot`]）。
///
/// **和信号量分开数**：信号量说不出「此刻有几个在等」，也分不清空着的位置是上限调小了欠着的
/// 还是真空着。计数和报出去的变化在同一把锁里，排空开始时报的那份全部和之后的变化前后接得上。
#[derive(Default)]
pub(crate) struct Claims {
    kind: Kind,
    inner: Mutex<ClaimsInner>,
}

#[derive(Default)]
struct ClaimsInner {
    n: HashMap<String, usize>,
    feed: Option<Arc<Feed>>,
}

impl Claims {
    pub(crate) fn new(kind: Kind) -> Self {
        Self {
            kind,
            inner: Mutex::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, ClaimsInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn event(&self, name: &str, d: i64) -> Event {
        match self.kind {
            Kind::Gate => Event::Gate {
                key: name.to_string(),
                d,
            },
            Kind::Slot => Event::Slot {
                upstream: name.to_string(),
                d,
            },
        }
    }

    /// 占上一个。**丢掉交回的那一个就放掉**
    pub(crate) fn begin(self: &Arc<Self>, name: &str) -> Claim {
        let mut g = self.lock();
        *g.n.entry(name.to_string()).or_default() += 1;
        send(&g.feed, || self.event(name, 1));
        Claim {
            claims: self.clone(),
            name: name.to_string(),
        }
    }

    fn end(&self, name: &str) {
        let mut g = self.lock();
        if let Some(n) = g.n.get_mut(name) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                g.n.remove(name);
            }
        }
        send(&g.feed, || self.event(name, -1));
    }

    /// 此刻每个名字占着几个
    pub(crate) fn counts(&self) -> HashMap<String, usize> {
        self.lock().n.clone()
    }

    /// 排空开始：报一份此刻的全部，之后的每一次变化接着报
    pub(crate) fn drain_into(&self, feed: Arc<Feed>) {
        let mut g = self.lock();
        let n = g.n.clone();
        feed.send(&match self.kind {
            Kind::Gate => Event::GateHolds { n },
            Kind::Slot => Event::SlotHolds { n },
        });
        g.feed = Some(feed);
    }
}

/// 占着的一个名额。**丢掉就放掉**，跟着通行证（[`crate::limits::Pass`]、[`crate::slots::Slot`]）走
pub(crate) struct Claim {
    claims: Arc<Claims>,
    name: String,
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.claims.end(&self.name);
    }
}
