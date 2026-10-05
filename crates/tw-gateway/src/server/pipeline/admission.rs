//! 管线第 3 步：这把密钥的用量上限和并发上限。
//!
//! **顺序是设计**：
//!
//! 1. 天、周、月的上限（[`crate::key_limits`]）—— 这一期用满了直接拒：到下一期之前等多久
//!    都一样，不用先排一轮并发的队；
//! 2. 并发上限（[`crate::limits`]）—— 等，不拒，理由在那儿；
//! 3. 分钟、小时的上限 —— 下一个空位在这个请求的等待期限之前空出来就等，等不到就拒，并
//!    说清多久之后再来。过了就把这个请求记上，按输入估一个数占着，等存储层记下它那一行时
//!    换成实数。
//!
//! **等待期限一个请求只有一个**：过了并发上限那一刻起算 `failover.slot_wait_secs`，这里等
//! 滚动窗口的空位、之后等上游的空位（见 `hop`）都算在里面 —— 两段各给一份的话，一个请求能
//! 等两倍那么久，而等的时候客户端一个字节都收不到。并发上限那一段不算：它等前面的请求结束，
//! 不拒绝，等多久由客户端决定（见 [`crate::limits`]）。
//!
//! 被上限拒绝的请求**照样开始、照样留一行**（和路由拒绝的一样，见
//! [`crate::server::routed_nowhere`]）：流量里看得见它被哪一条上限拒了。数 token 的请求
//! 不算用量，只过并发上限。

use super::{Inbound, look, open, redaction};
use crate::error::GatewayError;
use crate::key_limits::{Ask, Hold, Refusal};
use crate::server::Choice;
use crate::state::{AppState, Runtime};

/// 过了这一步的请求手里拿着的。
pub(super) struct Admitted {
    /// 并发闸门的通行证。丢掉就归还
    pub(super) pass: crate::limits::Pass,
    /// 用量上限的预留。开始事件之后交给请求号（[`Hold::bind`]），丢掉就放掉
    pub(super) hold: Hold,
    /// 这个请求最多等到什么时候。这一步等滚动窗口用掉的，之后等上游空位就少等那么久
    pub(super) wait_until: tokio::time::Instant,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn admit(
    state: &AppState,
    rt: &Runtime,
    req: &Inbound,
    reading: &crate::client_api::Reading,
    choice: &Choice,
    decision: &tw_engine::Decision,
    fp: Option<&str>,
    ending: &mut Option<crate::ending::Ending>,
) -> Result<Admitted, GatewayError> {
    let key = rt.config.clients.iter().find(|c| c.name == req.client_name);
    let max_concurrent = key.and_then(|c| c.max_concurrent);
    let limits: &[tw_config::KeyLimit] = match key {
        Some(c) if !crate::key_limits::uncounted(req.uri.path()) => &c.limits,
        _ => &[],
    };
    if let Err(r) = state.key_limits.calendar(&req.client_name, limits) {
        return Err(refused(state, rt, req, reading, choice, fp, ending, &r));
    }
    let pass = state.gate.acquire(&req.client_name, max_concurrent).await;
    // 等待期限从这里起算：之后的两段等待共用它
    let wait_until = tokio::time::Instant::now() + crate::key_limits::slot_wait(&rt.config);
    let ask = if limits.is_empty() {
        Ask::default()
    } else {
        ask(state, rt, reading, decision, limits)
    };
    match state
        .key_limits
        .admit_by(&req.client_name, limits, ask, wait_until)
        .await
    {
        Ok(hold) => Ok(Admitted {
            pass,
            hold,
            wait_until,
        }),
        Err(r) => Err(refused(state, rt, req, reading, choice, fp, ending, &r)),
    }
}

/// 这个请求要占多少：输入 token 的估算（解不开的请求没有估算，占 0），和设了费用上限
/// 时按头一个候选、发给它的名字算的输入费用。没有价格、不计费的是 0 —— 和结算时一样。
fn ask(
    state: &AppState,
    rt: &Runtime,
    reading: &crate::client_api::Reading,
    decision: &tw_engine::Decision,
    limits: &[tw_config::KeyLimit],
) -> Ask {
    let facts = &reading.facts;
    let tokens = if matches!(reading.decoded, Some(Ok(_))) {
        facts.input_tokens
    } else {
        0
    };
    let priced = limits
        .iter()
        .any(|l| l.measure() == tw_config::LimitMeasure::Cost);
    let cost_micros = if priced && tokens > 0 {
        input_cost(state, rt, facts, decision, tokens).unwrap_or(0)
    } else {
        0
    };
    Ask {
        tokens,
        cost_micros,
    }
}

fn input_cost(
    state: &AppState,
    rt: &Runtime,
    facts: &tw_engine::RequestFacts,
    decision: &tw_engine::Decision,
    tokens: u64,
) -> Option<i64> {
    let first = decision.candidates.first()?;
    let p = rt.config.providers.iter().find(|p| &p.name == first)?;
    let asked = rt.engine.asked_of(facts, decision, &p.name, None);
    let catalog = state.catalog.load();
    let allow = crate::models::key_allow(&rt.config, &facts.client);
    let model = crate::sent::name(&rt.config, &catalog, decision, p, &asked, allow).ok()?;
    let usage = tw_pricing::Usage {
        input: tokens,
        ..Default::default()
    };
    crate::quote::quote(&state.pricing.load(), &p.name, &model, &usage, p.billing).cost_micros
}

/// 被上限拒了：照样开始、留一行（尝试链是空的），交回给客户端的那个 429。
#[allow(clippy::too_many_arguments)]
fn refused(
    state: &AppState,
    rt: &Runtime,
    req: &Inbound,
    reading: &crate::client_api::Reading,
    choice: &Choice,
    fp: Option<&str>,
    ending: &mut Option<crate::ending::Ending>,
    r: &Refusal,
) -> GatewayError {
    tracing::info!(key = %req.client_name, per = r.limit.per.word(), "a usage limit of the key refused the request");
    let why = r.error();
    // 一个字节都没发出去；存下来的请求照样按这一档换、打码
    let (_, ledger) = look(rt, req);
    let (id, _) = open(
        state,
        req,
        reading,
        choice,
        ("", tw_api::Billing::PerToken),
        fp,
        ending,
        redaction(rt, ledger),
    );
    state
        .bus
        .emit(crate::server::routed_nowhere(id, choice.clone()));
    why
}
