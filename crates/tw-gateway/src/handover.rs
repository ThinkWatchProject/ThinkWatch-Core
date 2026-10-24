//! 交接时网关这一边要交出去、接进来的东西：内存状态的快照，排空期每个请求的结算。
//!
//! 搬运这些的是 `tw-handover`，它只当成 JSON；格式由这里定义（[`wire`]），和交接连接同一个
//! 版本号（`tw_handover::VERSION`），**不做跨版本兼容**。
//!
//! # 快照
//!
//! 接管开始时旧 core 交出（[`AppState::handover_export`]），新 core 接进来
//! （[`AppState::handover_import`]），新的就不从零开始：对话粘滞、会话归属、加密历史与缓存
//! 断点的拒收记录、熔断与健康、限流暂停、延迟与速度、负载均衡轮次、OAuth 访问令牌缓存、
//! 密钥和上游的用量与在途预留，以及旧 core 此刻占着的并发名额（按密钥、按上游）。
//!
//! **不搬的**：模型目录、余额、订阅额度、GLM 额度 —— 新 core 起来后台重新取，期间显示为
//! 获取中，搬过去的也只是几分钟前的样子；对话这一轮沿用的路由决定 —— 它认的是旧 core 那一份
//! 引擎，换了进程一定对不上，同一轮留在原来那一家靠的是上次回答它的那一家；限流的排队和
//! 探路名额、在途请求的中止柄和实时内容 —— 都是旧 core 手上的请求，跟着它们在旧 core 上结束；
//! 「已经报过」的那几张表（写回成功、凭据失效、额度用完）—— 新 core 的界面也是新开的，状况
//! 还在的话再报一次。
//!
//! # 排空期的名额
//!
//! 两个进程同时转发时，名额和用量上限不能翻倍：快照里旧 core 占着的名额，新 core 记成已占用
//! （「替它占着」，见 [`crate::limits::Gate::phantom`]、[`crate::slots::Slots::phantom`]）；旧 core
//! 每放掉一个名额、每结算一份用量发一条结算（[`AppState::begin_drain`] 的接收端，格式见
//! [`wire::Event`]），新 core 据此放掉（[`AppState::handover_settled`]）。交接连接断了，剩下的
//! 一并放掉（[`AppState::handover_release_all`]），之后由请求库重建用量校正。
//!
//! **密钥的名额连排着队等的也算**：旧 core 在提交之前收下、还在排队的请求，排在新 core 的新
//! 请求前面 —— 不算的话，它前面那个结束、新 core 放掉一个名额的那一刻，新 core 的新请求和它
//! 会同时进去。
//!
//! **从快照到排空开始之间**，旧 core 照常转发（接管还没提交），名额的变化不报：排空开始时
//! 重报一份此刻占着的全部（[`wire::Event::GateHolds`] 等），新 core 照它换掉手上记着的。用量的
//! 变化从快照那一刻起就攒着（[`feed::Feed`]），排空开始时一起交出去：快照之后结算的那些请求
//! 用了多少，新 core 一笔不落。
//!
//! # WebSocket
//!
//! 排空开始后，Responses 的连接在手上这一轮答完时以 1012（服务重启）关闭，闲着的当场关闭：
//! 客户端重连，连到新 core 上（见 `ws`）。整条连接一行的那几种（Realtime 等）分不出一轮
//! 一轮，照常开到客户端或上游收场。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::de::DeserializeOwned;
use tokio::sync::{mpsc, watch};

use crate::state::AppState;

pub(crate) mod feed;
#[cfg(test)]
mod tests;
pub(crate) mod wire;

/// 交接时网关这一边记着的。**跨重载存活**（在 `AppState` 里）
pub(crate) struct Handover {
    /// 导出快照时开的那一路和它的接收端：用量的变化从那一刻起攒在里面，排空开始时交出去
    pending: Mutex<Option<(Arc<feed::Feed>, mpsc::UnboundedReceiver<serde_json::Value>)>>,
    /// 排空开始了（旧 core）：WebSocket 看着它收场
    drain: watch::Sender<bool>,
    /// 替旧 core 占着的名额（新 core）
    phantoms: Mutex<Phantoms>,
}

impl Default for Handover {
    fn default() -> Self {
        Self {
            pending: Mutex::default(),
            drain: watch::channel(false).0,
            phantoms: Mutex::default(),
        }
    }
}

/// 替旧 core 占着的名额：按密钥的通行证、按上游的位置。**丢掉就放掉**，和别的一样
#[derive(Default)]
struct Phantoms {
    gate: HashMap<String, Vec<crate::limits::Pass>>,
    slots: HashMap<String, Vec<crate::slots::Slot>>,
}

impl Handover {
    fn phantoms(&self) -> MutexGuard<'_, Phantoms> {
        self.phantoms.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 看着排空开始的那一下（`ws` 用）
    pub(crate) fn draining(&self) -> watch::Receiver<bool> {
        self.drain.subscribe()
    }
}

/// 快照里的一节：读不懂就丢掉这一节，记一行
fn section<T: DeserializeOwned>(
    m: &serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Option<T> {
    let v = m.get(name)?;
    match serde_json::from_value(v.clone()) {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::warn!(
                section = name,
                "a part of the handover snapshot could not be read and is left out: {e}"
            );
            None
        }
    }
}

/// 快照里一节的每一条：读不懂的那一条丢掉，别的照接
fn entries<T: DeserializeOwned>(
    m: &serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Vec<T> {
    let Some(items) = m.get(name).and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let out: Vec<T> = items
        .iter()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect();
    if out.len() < items.len() {
        tracing::warn!(
            section = name,
            left_out = items.len() - out.len(),
            "some entries of the handover snapshot could not be read and are left out"
        );
    }
    out
}

impl AppState {
    /// 旧 core：此刻的内存状态。
    ///
    /// 从这一刻起用量的变化（预留、结算）攒着，[`Self::begin_drain`] 时一起交出去。**再调一次
    /// 从头攒**：上一次接管没成、换了一个接替者，上一次攒的那些没人要了
    pub fn handover_export(&self) -> serde_json::Value {
        let cfg = self.config();
        let (feed, rx) = feed::Feed::new();
        let key_limits = self.key_limits.handover_export(Some(feed.clone()));
        let upstream_limits = self.upstream_limits.handover_export(Some(feed.clone()));
        *self
            .handover
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((feed, rx));
        let mut m = serde_json::Map::new();
        let mut put = |name: &str, v: serde_json::Result<serde_json::Value>| match v {
            Ok(v) => {
                m.insert(name.to_string(), v);
            }
            Err(e) => tracing::warn!(
                section = name,
                "a part of the handover snapshot could not be written: {e}"
            ),
        };
        put(wire::AT_MS, Ok(crate::server::now_ms().into()));
        put(
            wire::AFFINITY,
            serde_json::to_value(self.affinity.handover_export()),
        );
        put(
            wire::SEALS,
            serde_json::to_value(self.seals.handover_export()),
        );
        put(
            wire::CACHE_MARKS,
            serde_json::to_value(self.cache_marks.handover_export()),
        );
        put(
            wire::SESSIONS,
            serde_json::to_value(self.sessions.handover_export()),
        );
        put(
            wire::HEALTH,
            serde_json::to_value(self.health.handover_export()),
        );
        put(
            wire::THROTTLES,
            serde_json::to_value(self.throttles.handover_export()),
        );
        put(
            wire::LATENCY,
            serde_json::to_value(self.latency.handover_export()),
        );
        put(
            wire::PACE,
            serde_json::to_value(self.pace.handover_export()),
        );
        put(
            wire::BALANCE,
            serde_json::to_value(self.balance.handover_export()),
        );
        put(
            wire::OAUTH,
            serde_json::to_value(self.oauth.handover_export(&cfg.providers)),
        );
        put(wire::KEY_LIMITS, serde_json::to_value(key_limits));
        put(wire::UPSTREAM_LIMITS, serde_json::to_value(upstream_limits));
        put(wire::GATE, serde_json::to_value(self.gate.claims.counts()));
        put(
            wire::SLOTS,
            serde_json::to_value(self.slots.claims.counts()),
        );
        serde_json::Value::Object(m)
    }

    /// 新 core：接进旧 core 的快照。**认不出的部分丢掉**，不让交接失败 —— 版本号
    /// 相同时不该发生，发生了也只是少接一样状态。
    ///
    /// 只调一次，在第一个请求之前（快照在监听交过来之前就到了）。再调一次的话，上一次替旧 core
    /// 占着的名额先全部放掉
    pub fn handover_import(&self, snapshot: &serde_json::Value) {
        let Some(m) = snapshot.as_object() else {
            tracing::warn!("the handover snapshot is not an object and is ignored");
            return;
        };
        self.handover_release_all();
        let lag_ms = m
            .get(wire::AT_MS)
            .and_then(serde_json::Value::as_u64)
            .map_or(0, |at| crate::server::now_ms().saturating_sub(at));
        let cfg = self.config();
        self.affinity.handover_import(entries(m, wire::AFFINITY));
        self.seals.handover_import(entries(m, wire::SEALS));
        self.cache_marks
            .handover_import(entries(m, wire::CACHE_MARKS));
        self.sessions.handover_import(entries(m, wire::SESSIONS));
        if let Some(h) = section(m, wire::HEALTH) {
            self.health.handover_import(h);
        }
        self.throttles
            .handover_import(entries(m, wire::THROTTLES), lag_ms);
        self.latency.handover_import(entries(m, wire::LATENCY));
        self.pace.handover_import(entries(m, wire::PACE));
        self.balance.handover_import(entries(m, wire::BALANCE));
        self.oauth
            .handover_import(entries(m, wire::OAUTH), &cfg.providers, lag_ms);
        if let Some(b) = section(m, wire::KEY_LIMITS) {
            self.key_limits.handover_import(b);
        }
        if let Some(b) = section(m, wire::UPSTREAM_LIMITS) {
            self.upstream_limits.handover_import(b);
        }
        if let Some(n) = section::<HashMap<String, usize>>(m, wire::GATE) {
            self.hold_gate(&n);
        }
        if let Some(n) = section::<HashMap<String, usize>>(m, wire::SLOTS) {
            self.hold_slots(&n);
        }
    }

    /// 旧 core：提交之后调一次。从这一刻起：
    ///
    /// - 每放掉一个名额、每结算一份用量，往返回的接收端发一条结算（格式见 [`wire::Event`]）；
    ///   第一批是此刻占着的全部名额和预留，之前攒着的用量变化排在它们前面；
    /// - WebSocket 会话在当前这一轮结束后以 1012（服务重启）关闭，客户端重连到新 core。
    ///
    /// **只调一次**。再调的话交回一个新的接收端，之前那个从此收不到东西
    pub fn begin_drain(&self) -> tokio::sync::mpsc::UnboundedReceiver<serde_json::Value> {
        let pending = self
            .handover
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let (feed, rx) = match pending {
            Some((f, rx)) if !f.lost() => (f, rx),
            // 没导出过快照，或者攒满了不再攒：从现在起报，开头那份全部照报
            _ => feed::Feed::new(),
        };
        feed.start_draining();
        self.gate.claims.drain_into(feed.clone());
        self.slots.claims.drain_into(feed.clone());
        self.key_limits.handover_drain(feed.clone());
        self.upstream_limits.handover_drain(feed);
        self.handover.drain.send_replace(true);
        rx
    }

    /// 新 core：旧 core 发来一条结算（[`Self::begin_drain`] 的接收端收到的那一条）。读不懂的
    /// 丢掉
    pub fn handover_settled(&self, settled: &serde_json::Value) {
        let ev: wire::Event = match serde_json::from_value(settled.clone()) {
            Ok(ev) => ev,
            Err(e) => {
                tracing::debug!("a handover settlement could not be read and is ignored: {e}");
                return;
            }
        };
        match &ev {
            wire::Event::GateHolds { n } => self.hold_gate(n),
            wire::Event::Gate { key, d } => self.step_gate(key, *d),
            wire::Event::SlotHolds { n } => self.hold_slots(n),
            wire::Event::Slot { upstream, d } => self.step_slot(upstream, *d),
            ev => {
                if !self.key_limits.handover_apply(ev) {
                    self.upstream_limits.handover_apply(ev);
                }
            }
        }
    }

    /// 新 core：交接连接断了（旧 core 退出或崩溃）。快照里记着的旧 core 的名额与
    /// 预留全部放掉。
    pub fn handover_release_all(&self) {
        // 在锁外面丢：放掉一个通行证要拿闸自己的锁
        let dropped = std::mem::take(&mut *self.handover.phantoms());
        drop(dropped);
        self.key_limits.handover_release_all();
        self.upstream_limits.handover_release_all();
    }

    // ------------------------------------------------------------ 替旧 core 占着的名额

    /// 这把密钥此刻配置里的并发上限。没设的不用替谁占
    fn key_limit(&self, key: &str) -> Option<usize> {
        self.config()
            .clients
            .iter()
            .find(|c| c.name == key)
            .and_then(|c| c.max_concurrent)
    }

    /// 替旧 core 占着的密钥名额换成 `want` 那么多
    fn hold_gate(&self, want: &HashMap<String, usize>) {
        let mut dropped = Vec::new();
        {
            let mut ph = self.handover.phantoms();
            let stale: Vec<String> = ph
                .gate
                .keys()
                .filter(|k| !want.contains_key(*k))
                .cloned()
                .collect();
            for k in stale {
                dropped.extend(ph.gate.remove(&k).unwrap_or_default());
            }
            for (key, n) in want {
                let Some(limit) = self.key_limit(key) else {
                    dropped.extend(ph.gate.remove(key).unwrap_or_default());
                    continue;
                };
                let held = ph.gate.entry(key.clone()).or_default();
                while held.len() > *n {
                    dropped.extend(held.pop());
                }
                while held.len() < *n {
                    held.push(self.gate.phantom(key, limit));
                }
            }
            ph.gate.retain(|_, v| !v.is_empty());
        }
        drop(dropped);
    }

    fn step_gate(&self, key: &str, d: i64) {
        let dropped = {
            let mut ph = self.handover.phantoms();
            if d > 0 {
                if let Some(limit) = self.key_limit(key) {
                    let pass = self.gate.phantom(key, limit);
                    ph.gate.entry(key.to_string()).or_default().push(pass);
                }
                None
            } else {
                let held = ph.gate.get_mut(key);
                let one = held.and_then(Vec::pop);
                ph.gate.retain(|_, v| !v.is_empty());
                one
            }
        };
        drop(dropped);
    }

    /// 替旧 core 占着的上游位置换成 `want` 那么多
    fn hold_slots(&self, want: &HashMap<String, usize>) {
        let mut dropped = Vec::new();
        {
            let mut ph = self.handover.phantoms();
            let stale: Vec<String> = ph
                .slots
                .keys()
                .filter(|k| !want.contains_key(*k))
                .cloned()
                .collect();
            for k in stale {
                dropped.extend(ph.slots.remove(&k).unwrap_or_default());
            }
            for (name, n) in want {
                let held = ph.slots.entry(name.clone()).or_default();
                while held.len() > *n {
                    dropped.extend(held.pop());
                }
                while held.len() < *n {
                    // 这家此刻不限并发：不用替谁占
                    let Some(slot) = self.slots.phantom(name) else {
                        break;
                    };
                    held.push(slot);
                }
            }
            ph.slots.retain(|_, v| !v.is_empty());
        }
        drop(dropped);
    }

    fn step_slot(&self, name: &str, d: i64) {
        let dropped = {
            let mut ph = self.handover.phantoms();
            if d > 0 {
                if let Some(slot) = self.slots.phantom(name) {
                    ph.slots.entry(name.to_string()).or_default().push(slot);
                }
                None
            } else {
                let one = ph.slots.get_mut(name).and_then(Vec::pop);
                ph.slots.retain(|_, v| !v.is_empty());
                one
            }
        };
        drop(dropped);
    }
}
