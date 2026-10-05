//! 开头慢就换下一家（配置的 `failover.next_on_slow_start`）。
//!
//! 有的上游收下请求之后很久不出内容：中转站排着队，上游过载却不报错，响应头都迟迟不来。
//! 开着这一项时，从请求发出去算起等 `failover.stream_start_wait_secs`，还没有内容就**断开
//! 这一家**（丢掉响应或者还在等的请求，连接跟着断，上游不再接着生成），换下一家。客户端
//! 这时一个字节都还没收到，换一家它无感。
//!
//! 几条规矩：
//!
//! - **最后一家不换**，照常等下去。「最后」按后面还有没有接得下的算（见
//!   `hop::successor`）：停用着的、这一跳发不出去的不算 —— 否则放弃了一个慢的，换来的是
//!   一个注定失败的。
//! - **这一家不停用、不算失败**：慢不是坏，下一个请求它可能就快了。
//! - **尝试链上记一跳 `slow_start`**，带着上游可能已经收了钱的输入（见
//!   [`tw_api::AttemptUsage`]）。
//! - 只管客户端要流式的请求：整包的请求本来就要等全部生成完，开头慢说明不了什么。
//!
//! **等的时候不给客户端发保活。**响应头要等选定了哪一家才发（见 `relay`），这期间客户端
//! 那条连接上什么都没有；先发响应头再发 `: keepalive` 的话，状态码就定死成了 200 —— 之后
//! 几家全都失败，429、5xx 和最后一家原样交出的 4xx 都给不出去，只能在流里报错，客户端按
//! 状态码重试的逻辑就落空了；上游的响应头（请求号、额度）也带不过去。何况 Gemini 官方的
//! Python SDK 会把注释行当成一段 JSON 去解析，直接报错。

use std::time::Duration;

use crate::state::Runtime;
use tw_types::msg;

/// 这个请求开头慢了换不换、换的话等多久：开着这一项、客户端要的是流时才有。
pub(super) fn wait(rt: &Runtime, reading: &crate::client_api::Reading) -> Option<Duration> {
    let f = &rt.config.failover;
    let streams = matches!(&reading.decoded, Some(Ok(d)) if d.request.stream);
    (f.next_on_slow_start && streams).then(|| Duration::from_secs(f.stream_start_wait_secs))
}

/// 放弃了的那一跳：尝试链上的一行。`status` 是上游回的（响应头没到的没有），`seen` 是流
/// 开头里上游报的用量。
pub(super) fn abandoned(
    provider: &str,
    model: Option<String>,
    status: Option<u16>,
    seen: Option<tw_dialect::usage::Usage>,
    reading: &crate::client_api::Reading,
    waited: Duration,
    started: std::time::Instant,
) -> tw_api::AttemptView {
    tw_api::AttemptView {
        provider: provider.to_string(),
        model,
        outcome: tw_api::AttemptOutcome::SlowStart,
        status,
        error: Some(said(provider, waited)),
        ms: started.elapsed().as_millis() as u64,
        usage: usage(seen, reading),
        // 等过空位的话，等了多久由尝试链补上（`stamp_queued`）
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

/// 尝试链上那一跳的说明。后面几家也都不行时，它也是交给客户端的那条错误的退路
pub(super) fn said(upstream: &str, waited: Duration) -> tw_types::Msg {
    msg!(
        "gw.slow_start", upstream = upstream, secs = waited.as_secs() =>
        "Upstream `{upstream}` sent no content within {secs} seconds, so the request moved on \
         to the next upstream."
    )
}
