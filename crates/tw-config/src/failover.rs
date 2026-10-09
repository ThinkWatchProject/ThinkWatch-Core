//! 故障转移：一家上游失败之后停用多久、多久没有内容就不再等它、上游满着时最多等多久。

use serde::{Deserialize, Serialize};

/// 一家上游失败之后停用多久。
///
/// **按失败的原因分开算**，因为上游说出来的原因差别很大：余额不足要等用户去
/// 充值，额度用完要等到上游说的重置时刻，限流通常几十秒就过去 —— 一律「冷却
/// 60 秒」的话，前两种每分钟白试一次，最后一种又停得太久。
///
/// 只有一家候选的请求不受这些影响：没有别的家可切，停用它只会把用户锁死
/// （见 `tw_gateway::health`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failover {
    /// 说不出原因的失败（5xx、连不上）连续几次之后停用
    #[serde(default = "d_failures_to_pause")]
    pub failures_to_pause: u32,
    /// 这类失败第一次停用多少秒。**之后每再停一次翻一倍**，直到
    /// [`Self::max_pause_secs`]；成功一次就回到这个数
    #[serde(default = "d_pause_secs")]
    pub pause_secs: u64,
    /// 翻倍的上限，秒
    #[serde(default = "d_max_pause_secs")]
    pub max_pause_secs: u64,
    /// 上游说余额不足时停用多少秒
    #[serde(default = "d_no_balance_pause_secs")]
    pub no_balance_pause_secs: u64,
    /// 上游说额度用完、又没说什么时候重置时，停用多少秒。说了的就停到那一刻
    #[serde(default = "d_quota_pause_secs")]
    pub quota_pause_secs: u64,
    /// 限流时按上游给的 `Retry-After` 停用，**最多停多少秒**。没给就当作说不出
    /// 原因的失败
    #[serde(default = "d_rate_limit_max_pause_secs")]
    pub rate_limit_max_pause_secs: u64,
    /// 上游多少秒没有内容就不再等它（无响应超时）。从请求发给它的那一刻算起，**每来一段
    /// 内容重新计时**：正文、推理、工具调用都算，心跳不算（SSE 注释、Anthropic 的 `ping`、
    /// 只有角色的空块……）—— 否则一家只发心跳的上游会一直挂着。整包的回答没有「一段段」，
    /// 从发出去到整份回来算一段。
    ///
    /// 到点时还没有内容交给客户端的，这一家记一次失败，换下一家；没有下一家了就回超时
    /// 错误。已经交出去一部分的换不了（客户端会收到两遍开头），按客户端的格式报错收尾
    #[serde(default = "d_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    /// 一个请求最多等多少秒，**整个请求合起来算**：准入时等密钥的分钟、小时上限空出名额，
    /// 之后上游的并发数满了（`providers[].max_concurrent`）时等空位，共用这一段。留在那一家
    /// 的对话等它空出来，候选都满了时等先空出来的那一家；等不到的换下一家，或者回 429。
    /// 0 是不等
    #[serde(default = "d_slot_wait_secs")]
    pub slot_wait_secs: u64,
}

fn d_failures_to_pause() -> u32 {
    3
}
fn d_pause_secs() -> u64 {
    60
}
fn d_max_pause_secs() -> u64 {
    600
}
fn d_no_balance_pause_secs() -> u64 {
    1800
}
fn d_quota_pause_secs() -> u64 {
    3600
}
fn d_rate_limit_max_pause_secs() -> u64 {
    3600
}
fn d_idle_timeout_secs() -> u64 {
    300
}
fn d_slot_wait_secs() -> u64 {
    30
}

impl Default for Failover {
    fn default() -> Self {
        Self {
            failures_to_pause: d_failures_to_pause(),
            pause_secs: d_pause_secs(),
            max_pause_secs: d_max_pause_secs(),
            no_balance_pause_secs: d_no_balance_pause_secs(),
            quota_pause_secs: d_quota_pause_secs(),
            rate_limit_max_pause_secs: d_rate_limit_max_pause_secs(),
            idle_timeout_secs: d_idle_timeout_secs(),
            slot_wait_secs: d_slot_wait_secs(),
        }
    }
}

/// 停用时长最多写多少秒：一周。**额度用完停到重置时刻不受它限制** —— 那是上游
/// 说的时刻，周额度本来就可能在七天之后
pub const MAX_PAUSE_SECS: u64 = 7 * 24 * 3600;

/// 无响应超时最少写多少秒。再短的话，先想好再输出的模型还没开口就被放弃了
pub const MIN_IDLE_TIMEOUT_SECS: u64 = 30;

/// 无响应超时最多写多少秒：一小时。再长就等于不设
pub const MAX_IDLE_TIMEOUT_SECS: u64 = 3600;

/// 等空位最多写多少秒。等的时候客户端一个字节都收不到，再长的话它先超时了
pub const MAX_SLOT_WAIT_SECS: u64 = 300;

impl Failover {
    /// 不在允许范围里的第一项：字段名、写的值、下限、上限。
    pub(crate) fn out_of_range(&self) -> Option<(&'static str, u64, u64, u64)> {
        let fields: [(&'static str, u64, u64, u64); 8] = [
            (
                "failures_to_pause",
                u64::from(self.failures_to_pause),
                1,
                100,
            ),
            ("pause_secs", self.pause_secs, 1, MAX_PAUSE_SECS),
            (
                "max_pause_secs",
                self.max_pause_secs,
                self.pause_secs.max(1),
                MAX_PAUSE_SECS,
            ),
            (
                "no_balance_pause_secs",
                self.no_balance_pause_secs,
                1,
                MAX_PAUSE_SECS,
            ),
            ("quota_pause_secs", self.quota_pause_secs, 1, MAX_PAUSE_SECS),
            (
                "rate_limit_max_pause_secs",
                self.rate_limit_max_pause_secs,
                1,
                MAX_PAUSE_SECS,
            ),
            (
                "idle_timeout_secs",
                self.idle_timeout_secs,
                MIN_IDLE_TIMEOUT_SECS,
                MAX_IDLE_TIMEOUT_SECS,
            ),
            ("slot_wait_secs", self.slot_wait_secs, 0, MAX_SLOT_WAIT_SECS),
        ];
        fields
            .into_iter()
            .find(|(_, v, min, max)| v < min || v > max)
    }
}
