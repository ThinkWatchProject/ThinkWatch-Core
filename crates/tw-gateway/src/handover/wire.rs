//! 快照与结算的格式。**只在交接连接上走**，和 `tw_handover::VERSION` 同一个版本号，不做
//! 跨版本兼容：改了这里任何一样都升那个号。
//!
//! 时刻一律是墙上的时钟（Unix 毫秒）或者「还剩多久 / 过去多久」的毫秒数 —— `Instant` 出不了
//! 这个进程。快照里的「还剩多久」从导出的那一刻（[`AT_MS`] 那一节）算起，接进来时扣掉路上
//! 用掉的那一点。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::key_limits::Recorded;
use crate::key_limits::books::{Amount, Period};

/// 快照里各节的名字。**一节读不懂只丢那一节**（见 [`crate::state::AppState::handover_import`]）
pub(crate) const AT_MS: &str = "at_ms";
pub(crate) const AFFINITY: &str = "affinity";
pub(crate) const SEALS: &str = "seals";
pub(crate) const CACHE_MARKS: &str = "cache_marks";
pub(crate) const SESSIONS: &str = "sessions";
pub(crate) const HEALTH: &str = "health";
pub(crate) const THROTTLES: &str = "throttles";
pub(crate) const LATENCY: &str = "latency";
pub(crate) const PACE: &str = "pace";
pub(crate) const BALANCE: &str = "balance";
pub(crate) const OAUTH: &str = "oauth";
pub(crate) const KEY_LIMITS: &str = "key_limits";
pub(crate) const UPSTREAM_LIMITS: &str = "upstream_limits";
pub(crate) const GATE: &str = "gate";
pub(crate) const SLOTS: &str = "slots";

// ------------------------------------------------------------ 快照的各节

/// 一段对话记着的（[`crate::affinity`]）。**这一轮沿用的路由决定不搬**：它认的是旧 core 那一份
/// 引擎（配置换了就不沿用），换了进程一定对不上；同一轮留在原来那一家靠的是 `answer`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct AffinityEntry {
    pub key: String,
    pub touched_ms: u64,
    pub answer: Option<AffinityAnswer>,
    pub holder: Option<String>,
    pub sealed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct AffinityAnswer {
    pub group: Option<String>,
    pub provider: String,
    pub turn: u32,
    pub at_ms: u64,
    pub cache: u64,
}

/// 一段对话里一家拒过的别家封存的推理（[`crate::seal`]）。指纹写成十六进制
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SealEntry {
    pub conversation: String,
    pub upstream: String,
    pub last_ms: u64,
    pub seals: Vec<String>,
}

/// 拒过自动标的缓存断点的（上游，模型）（[`crate::cache_marks`]）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CacheMark {
    pub upstream: String,
    pub model: String,
    pub at_ms: u64,
}

/// 一段对话此刻归在哪一次会话（[`crate::session`]）。不搬的话，交接时正在进行的任务在流量里
/// 被切成两段
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SessionEntry {
    pub fp: String,
    pub id: String,
    pub last_ms: u64,
}

/// 熔断与最近的成败（[`crate::health`]）
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Health {
    pub entries: Vec<HealthEntry>,
    pub recent: Vec<HealthRecent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HealthEntry {
    pub name: String,
    pub breaker: tw_breaker::Breaker,
    pub trips: u32,
    pub held_until_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HealthRecent {
    pub name: String,
    /// （时刻，成没成），先的在前
    pub samples: Vec<(i64, bool)>,
}

/// 一家的限流暂停（[`crate::throttle`]）。**排着的队和探路的名额不搬**：那是旧 core 手上的请求
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ThrottleEntry {
    pub upstream: String,
    /// 暂停还剩多久。没在暂停是 None
    pub pause_left_ms: Option<u64>,
    /// 最近一次被限流是多久之前
    pub limited_ago_ms: Option<u64>,
    pub strikes: u32,
    pub probing: bool,
}

/// 一家的首段内容用时样本（[`crate::latency`]）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct LatencyEntry {
    pub upstream: String,
    pub real: Vec<u32>,
    pub seed: Option<u32>,
}

/// 流里估数和实数之比（[`crate::pace`]）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PaceEntry {
    pub upstream: String,
    pub model: String,
    pub ratio: f64,
}

/// 一个 `load-balance` 组轮到哪儿了（[`crate::balance`]）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct BalanceRound {
    pub group: String,
    pub members: Vec<tw_engine::Member>,
    pub current: HashMap<String, i64>,
}

/// 一家的 OAuth access token（[`crate::oauth`]）。**原文**：只在交接连接上走（同一个用户、
/// 本机），和 config.yaml 里写着的是同一份东西
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct OauthEntry {
    pub upstream: String,
    /// 生成它的那份配置（refresh、端点、client）的指纹，blake3。新 core 的配置对得上才接
    pub cfg: String,
    pub access: String,
    pub refresh: String,
    /// 还有多久该去刷。服务器没说有效期是 None
    pub renew_in_ms: Option<u64>,
    /// 是多久之前从 token 端点换来的。从配置里读的是 None
    pub age_ms: Option<u64>,
    pub failed: Option<OauthFailed>,
}

impl std::fmt::Debug for OauthEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OauthEntry")
            .field("upstream", &self.upstream)
            .field("renew_in_ms", &self.renew_in_ms)
            .field("age_ms", &self.age_ms)
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct OauthFailed {
    pub cause: crate::oauth::OauthError,
    pub retry_in_ms: u64,
}

/// 用量的账（密钥的、上游的同一个样子，见 [`crate::key_limits::books`]）：每个名字的天、周、
/// 月和滚动窗口，和此刻在跑的预留
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct LimitBooks {
    pub books: Vec<NamedBooks>,
    pub held: Vec<Held>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct NamedBooks {
    pub name: String,
    pub periods: [Option<Period>; 3],
    pub recent: Vec<(i64, Amount)>,
}

/// 一份在跑的预留。`seq` 是**发出它的那个 core** 给的号，结算时认它
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Held {
    pub seq: u64,
    pub key: String,
    pub at_ms: i64,
    pub tokens: i64,
    pub cost: i64,
}

// ------------------------------------------------------------ 排空期的结算

/// 排空期旧 core 发来的一条结算（`tw_handover::Message::Settled` 的 `data`）。
///
/// **一条是一样东西的一次变化**，不是一个请求一条：名额在请求结束（或者换下一家、排队时客户端
/// 走了）的那一刻放掉，用量要等存储层记下那一行才知道 —— 两样不在同一刻，按请求凑成一条的话，
/// 要么名额压到用量出来才放，要么用量丢了。
///
/// - `*_holds`：旧 core 此刻占着的全部（排空开始时发一次），新 core 照它换掉手上记着的；
/// - `gate` / `slot`：一把密钥、一家上游的名额多占（`d` 是 1）或放掉（`d` 是 -1）了一个。密钥的
///   名额**连排着队等的也算**：它们是排在前面的，新 core 不该趁它们还在等时把位置给了别人；
/// - `key_*` / `up_*`：用量上限的预留占上、结算成实数、退回、过期作废。结算带着密钥、上游、
///   token 用量和费用（[`Recorded`]）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub(crate) enum Event {
    GateHolds {
        n: HashMap<String, usize>,
    },
    Gate {
        key: String,
        d: i64,
    },
    SlotHolds {
        n: HashMap<String, usize>,
    },
    Slot {
        upstream: String,
        d: i64,
    },
    KeyHolds {
        held: Vec<Held>,
    },
    /// `windowed`：占上时滚动窗口里记了这一个请求
    KeyReserve {
        held: Held,
        windowed: bool,
    },
    /// 存储层记下了一行。`seq` 是它占着的预留（没有预留的是 None：那把密钥没设上限）
    KeySettle {
        seq: Option<u64>,
        at_ms: i64,
        rec: Recorded,
    },
    /// 预留整份退回，滚动窗口里那一个请求也撤掉（准入之后、开始之前就丢掉了）
    KeyRelease {
        seq: u64,
    },
    /// 预留作废（请求结束了太久、存储层一直没交来那一行）
    KeyDrop {
        seq: u64,
    },
    UpHolds {
        held: Vec<Held>,
    },
    UpReserve {
        held: Held,
        windowed: bool,
    },
    UpSettle {
        seqs: Vec<u64>,
        at_ms: i64,
        hops: Vec<String>,
        served: Option<String>,
        rec: Recorded,
    },
    UpRelease {
        seq: u64,
    },
    UpDrop {
        seq: u64,
    },
}

impl Event {
    pub(crate) fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}
