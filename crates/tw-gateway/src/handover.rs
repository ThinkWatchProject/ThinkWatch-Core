//! 交接时网关这一边要交出去、接进来的东西：内存状态的快照，排空期每个请求的结算。
//!
//! 搬运这些的是 `tw-handover`，它只当成 JSON；格式由这里定义，和交接连接同一个
//! 版本号（`tw_handover::VERSION`），**不做跨版本兼容**。
//!
//! # 快照
//!
//! 接管开始时旧 core 交出（[`AppState::handover_export`]），新 core 接进来
//! （[`AppState::handover_import`]），新的就不从零开始：对话粘滞、加密历史与缓存
//! 断点的拒收记录、熔断与健康、限流暂停、延迟与速度、负载均衡轮次、OAuth 访问
//! 令牌缓存、密钥用量与在途预留，以及旧 core 此刻占着的并发名额（按密钥、按上游）。
//!
//! # 排空期的名额
//!
//! 两个进程同时转发时，名额和用量上限不能翻倍：快照里旧 core 占着的名额，新 core
//! 记成已占用；旧 core 每结束一个请求发一条结算（[`AppState::begin_drain`] 的
//! 接收端），新 core 据此放掉（[`AppState::handover_settled`]）。交接连接断了，
//! 剩下的一并放掉（[`AppState::handover_release_all`]），之后由请求库重建用量校正。

use crate::state::AppState;

impl AppState {
    /// 旧 core：此刻的内存状态。
    pub fn handover_export(&self) -> serde_json::Value {
        todo!("C3")
    }

    /// 新 core：接进旧 core 的快照。**认不出的部分丢掉**，不让交接失败 —— 版本号
    /// 相同时不该发生，发生了也只是少接一样状态。
    pub fn handover_import(&self, _snapshot: &serde_json::Value) {
        todo!("C3")
    }

    /// 旧 core：提交之后调一次。从这一刻起：
    ///
    /// - 每结束一个请求，往返回的接收端发一条结算（密钥、上游、用量、费用、放掉的名额）；
    /// - WebSocket 会话在当前这一轮结束后以 1012（服务重启）关闭，客户端重连到新 core。
    pub fn begin_drain(&self) -> tokio::sync::mpsc::UnboundedReceiver<serde_json::Value> {
        todo!("C3")
    }

    /// 新 core：旧 core 结束了一个请求。
    pub fn handover_settled(&self, _settled: &serde_json::Value) {
        todo!("C3")
    }

    /// 新 core：交接连接断了（旧 core 退出或崩溃）。快照里记着的旧 core 的名额与
    /// 预留全部放掉。
    pub fn handover_release_all(&self) {
        todo!("C3")
    }
}
