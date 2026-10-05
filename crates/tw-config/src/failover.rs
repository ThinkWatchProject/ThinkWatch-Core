//! 故障转移：一家上游失败之后停用多久、流开头最多等多久、等不到内容换不换下一家。

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
    /// 流式回答开头最多等多少秒。在第一段内容到达之前，上游报的错误照样换下一家；
    /// 等过这么久还没有内容，就不再等，把已经收到的交给客户端
    #[serde(default = "d_stream_start_wait_secs")]
    pub stream_start_wait_secs: u64,
    /// 流式回答等过 [`Self::stream_start_wait_secs`] 还没有内容时，放弃这一家、换下一家。
    /// **最后一家不换**，照常等下去；这一家不停用，也不算一次失败。默认关：先想好再
    /// 输出的模型开头本来就慢，开着时要把等待调长
    #[serde(default)]
    pub next_on_slow_start: bool,
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
fn d_stream_start_wait_secs() -> u64 {
    15
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
            stream_start_wait_secs: d_stream_start_wait_secs(),
            next_on_slow_start: false,
        }
    }
}

/// 停用时长最多写多少秒：一周。**额度用完停到重置时刻不受它限制** —— 那是上游
/// 说的时刻，周额度本来就可能在七天之后
pub const MAX_PAUSE_SECS: u64 = 7 * 24 * 3600;

/// 流开头最多等多少秒。再长的话，一家卡在半路的上游会让客户端先超时
pub const MAX_STREAM_START_WAIT_SECS: u64 = 120;

/// 开着「开头慢就换下一家」时，流开头至少等多少秒。再短的话，平常的请求还没开口就被
/// 切掉了
pub const MIN_SLOW_START_WAIT_SECS: u64 = 5;

impl Failover {
    /// 不在允许范围里的第一项：字段名、写的值、下限、上限。
    pub(crate) fn out_of_range(&self) -> Option<(&'static str, u64, u64, u64)> {
        let fields: [(&'static str, u64, u64, u64); 7] = [
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
                "stream_start_wait_secs",
                self.stream_start_wait_secs,
                1,
                MAX_STREAM_START_WAIT_SECS,
            ),
        ];
        fields
            .into_iter()
            .find(|(_, v, min, max)| v < min || v > max)
    }

    /// 开着「开头慢就换下一家」、等待却短于 [`MIN_SLOW_START_WAIT_SECS`]：写的等待秒数。
    pub(crate) fn slow_start_too_short(&self) -> Option<u64> {
        (self.next_on_slow_start && self.stream_start_wait_secs < MIN_SLOW_START_WAIT_SECS)
            .then_some(self.stream_start_wait_secs)
    }
}
