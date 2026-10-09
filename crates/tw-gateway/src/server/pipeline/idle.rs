//! 无响应超时（配置的 `failover.idle_timeout_secs`）：上游多久没有内容就不再等它。
//!
//! 有的上游收下请求之后一直不出内容：中转站排着队，上游过载却不报错，响应头都迟迟不来；
//! 也有的答到一半停住，连接却不断。整体超时不能设（一个跑了六分钟的回答不该被掐断，见
//! `crate::outbound`），所以看的是**多久没有内容**：从请求发给这一家的那一刻算起（之前等
//! 空位的时间不算），每来一段真内容重新计时，心跳不算（判据见 [`crate::pulse`]）。整包的回答
//! 没有「一段段」，从发出去到整份回来算一段。
//!
//! 到点了怎么办，看客户端收到了什么：
//!
//! - **还没有内容交给客户端**：放弃这一家（丢掉响应或者还在等的请求，连接跟着断，上游不再
//!   接着生成），记一次失败（和 5xx 一样算进停用的账），换下一家（见 `hop`）。客户端无感 ——
//!   流式的回答压过 [`crate::OPENING_HOLD`] 之后响应头已经交出去了，它收到的只是保活注释（见
//!   [`super::commit`]）。没有下一家了，回一个超时错误（504，见
//!   [`crate::error::Source::Timeout`]），尝试链带着；响应头已经交出去的，在流里报这个错。
//! - **最后一家的响应已经交出去了**（最后一家不压开头）、或者**已经有内容交给了客户端**：
//!   换不了 —— 换一家会把开头再发一遍。回答按客户端的格式以一条错误收尾，请求记成失败
//!   （见 `relay`）。还没有内容的照样记这一家一次失败；内容出到一半才停的不记，和流在半路
//!   断了一样。
//!
//! 几条规矩：
//!
//! - **它的快慢样本记它被给的那段时间**（见 [`timed_out`]）：`url-test` 和按快慢分的
//!   `load-balance` 照这个把它往后排。
//! - **这段对话这一轮不再留在它那儿**（见 [`crate::affinity::Affinity::left`]）：下一个请求
//!   照常排序，不会因为「上次回答的就是它」又被送回去。
//! - **尝试链上记一跳 `idle_timeout`**，带着上游可能已经收了钱的输入（见
//!   [`tw_api::AttemptUsage`]）。
//!
//! **头 15 秒不给客户端发保活**（[`crate::OPENING_HOLD`]）：响应头要等选定了哪一家才发（见
//! `relay`），先发响应头再发 `: keep-alive` 的话，状态码就定死成了 200 —— 之后几家全都失败，
//! 429、5xx 和最后一家原样交出的 4xx 都给不出去，只能在流里报错；上游的响应头（请求号、额度）
//! 也带不过去。可一直压到无响应超时，客户端等响应头的计时又先到了，所以压着的时间有上限，
//! 过了才先交响应头、发保活（见 [`super::commit`]）。

use std::time::Duration;

use crate::state::{AppState, Runtime};
use tw_types::msg;

/// 这个请求的无响应超时：配置的秒数，和它在这个进程里有多长（测试把一秒调短，见
/// `AppState::idle_tick`）。
#[derive(Debug, Clone, Copy)]
pub(super) struct Quiet {
    /// 配置写的秒数。报给人看的句子用它
    pub(super) secs: u64,
    /// 真正等多久
    pub(super) window: Duration,
}

impl Quiet {
    pub(super) fn of(state: &AppState, rt: &Runtime) -> Self {
        let secs = rt.config.failover.idle_timeout_secs;
        let window = state
            .idle_tick
            .checked_mul(u32::try_from(secs).unwrap_or(u32::MAX))
            .unwrap_or(Duration::MAX);
        Self { secs, window }
    }

    /// 从 `from`（这一跳发出去、或者上一段内容到的那一刻）算起，到什么时候为止
    pub(super) fn after(&self, from: std::time::Instant) -> tokio::time::Instant {
        let from = tokio::time::Instant::from_std(from);
        from.checked_add(self.window)
            .unwrap_or_else(|| from + Duration::from_secs(86_400 * 365))
    }
}

/// 放弃了这一家：给它记一个快慢样本，就是它被给的那段时间（见 [`crate::latency`]）。
///
/// **它至少这么慢**，这是个下限：记成这个数，`url-test` 和按快慢分的 `load-balance` 就把它
/// 排到慢的那一头。什么都不记的话，它留着的还是从前快的样本，下一个请求照样先发给它，
/// 而等它的这段时间算到了接下来那一家头上。
pub(super) fn timed_out(state: &AppState, provider: &str, waited: Duration) {
    state.latency.record(provider, crate::latency::ms(waited));
}

/// 放弃了的那一跳：尝试链上的一行。`status` 是上游回的（响应头没到的没有），`seen` 是流
/// 开头里上游报的用量。
#[allow(clippy::too_many_arguments)]
pub(super) fn abandoned(
    provider: &str,
    model: Option<String>,
    status: Option<u16>,
    seen: Option<tw_dialect::usage::Usage>,
    reading: &crate::client_api::Reading,
    quiet: Quiet,
    started: std::time::Instant,
) -> tw_api::AttemptView {
    tw_api::AttemptView {
        provider: provider.to_string(),
        model,
        outcome: tw_api::AttemptOutcome::IdleTimeout,
        status,
        error: Some(said(provider, quiet.secs)),
        ms: started.elapsed().as_millis() as u64,
        usage: usage(seen, reading),
        // 等过空位的话，等了多久由尝试链补上（`stamp_queued`）
        queued_ms: None,
        skipped: None,
    }
}

/// 等上游的时候被手动中止的那一跳（见 [`crate::abort`]）。`status` 是上游回的（响应头没到的
/// 没有）
pub(super) fn aborted_hop(
    provider: &str,
    model: Option<String>,
    status: Option<u16>,
    started: std::time::Instant,
) -> tw_api::AttemptView {
    tw_api::AttemptView {
        provider: provider.to_string(),
        model,
        outcome: tw_api::AttemptOutcome::Aborted,
        status,
        error: Some(crate::error::GatewayError::aborted().detail),
        ms: started.elapsed().as_millis() as u64,
        usage: None,
        queued_ms: None,
        skipped: None,
    }
}

/// 放弃的那一家可能已经收了钱的输入：上游报了的用它报的，没报的用网关估的（和开始事件的
/// `input_estimate` 同一个数），估不出来（请求解不开）就没有。
fn usage(
    seen: Option<tw_dialect::usage::Usage>,
    reading: &crate::client_api::Reading,
) -> Option<tw_api::AttemptUsage> {
    match seen.filter(|u| u.prompt_total() > 0) {
        Some(u) => Some(tw_api::AttemptUsage {
            input: u.input,
            cache_read: u.cache_read,
            cache_write: u.cache_write,
            estimated: false,
        }),
        None => matches!(reading.decoded, Some(Ok(_))).then_some(tw_api::AttemptUsage {
            input: reading.facts.input_tokens,
            cache_read: 0,
            cache_write: 0,
            estimated: true,
        }),
    }
}

/// 还没有内容就超时了：尝试链上那一跳的说明，也是候选用完时交给客户端的那条错误
pub(super) fn said(upstream: &str, secs: u64) -> tw_types::Msg {
    msg!(
        "gw.upstream.idle_timeout", upstream = upstream, secs = secs =>
        "Upstream `{upstream}` sent no content within {secs} seconds."
    )
}

/// 答到一半停住了
pub(super) fn stalled(upstream: &str, secs: u64) -> tw_types::Msg {
    msg!(
        "gw.upstream.idle_timeout_mid_stream", upstream = upstream, secs = secs =>
        "Upstream `{upstream}` stopped sending content partway through the answer and sent \
         nothing more for {secs} seconds."
    )
}
