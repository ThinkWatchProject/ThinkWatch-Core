//! 每家上游的用量上限（`providers[].limits`）。
//!
//! **这一份是占位**：签名照约定写好，什么都不数 —— 状态一律是 [`LimitState::Free`]，预留什么都
//! 不占，[`UpstreamLimits::changed`] 永远等不到。真正的实现（按 `crate::key_limits` 的样子数请求、
//! token 和费用）换进来时整个文件替换掉，调用它的地方不用改。

use tw_config::KeyLimit;

/// 一家上游此刻在用量上限上的样子。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitState {
    Free,
    /// 滚动窗口满了：这一刻（Unix 毫秒）又能发；是第几条上限
    WindowFull {
        until_ms: u64,
        limit: u32,
    },
    /// 自然周期用完了：这一刻重置；是第几条上限
    PeriodUsedUp {
        until_ms: u64,
        limit: u32,
    },
}

/// 每家上游的用量账，按上游的名字记。
pub struct UpstreamLimits {
    _bus: tw_observe::EventBus,
}

impl UpstreamLimits {
    pub fn new(bus: tw_observe::EventBus) -> Self {
        Self { _bus: bus }
    }

    /// 照这份配置定每家的上限。启动时和每次重载时调
    pub fn configure(&self, _cfg: &tw_config::Config) {}

    /// 上游改了名：账跟着走
    pub fn rename(&self, _from: &str, _to: &str) {}

    /// 此刻的样子，在跑的预留也算
    pub fn state(&self, _upstream: &str, _limits: &[KeyLimit]) -> LimitState {
        LimitState::Free
    }

    /// 这一跳要发出去了：请求数此刻记上，token 和费用按请求估一个数占着
    pub fn reserve(
        &self,
        _upstream: &str,
        _limits: &[KeyLimit],
        _ask: &crate::key_limits::Ask,
    ) -> UpstreamHold {
        UpstreamHold {}
    }

    /// 存储层记下了这个请求那一行：发到了的每一跳（`hops`）记一个请求，接下它的那一家（`served`）
    /// 的 token 和费用换成实数
    pub fn settle(
        &self,
        _request: u64,
        _at_ms: i64,
        _hops: &[String],
        _served: Option<&str>,
        _rec: &crate::key_limits::Recorded,
    ) {
    }

    /// 启动时从请求记录里把这一期加回来
    pub fn rebuild(&self, _since: impl FnMut(i64) -> Vec<(String, crate::key_limits::Recorded)>) {}

    /// 界面上每一条上限的设置和用量
    pub fn view(&self, _upstream: &str, _limits: &[KeyLimit]) -> Vec<tw_api::KeyLimitView> {
        Vec::new()
    }

    /// 这一家的样子可能变了（结算了、放掉了、窗口滑过去了）
    pub async fn changed(&self, _upstream: &str) {
        std::future::pending::<()>().await
    }
}

/// 一跳占着的那一份。**丢掉就还回去**（和 [`UpstreamHold::release`] 一样）
#[must_use = "dropping it gives the reservation back"]
pub struct UpstreamHold {}

impl UpstreamHold {
    /// 什么都不占的一份
    pub fn none() -> Self {
        Self {}
    }

    /// 这一跳回了 429、或者没发出去：全部还回去
    pub fn release(self) {}

    /// 这一跳发出去了、没回 429：交给请求号，等结算（[`UpstreamLimits::settle`]）
    pub fn bind(self, _request: u64) {}
}

impl Drop for UpstreamHold {
    fn drop(&mut self) {}
}
