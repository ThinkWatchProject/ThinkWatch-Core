//! 密钥的用量上限和存储层之间的两根线：每记下一行请求就结算，启动时从请求记录里把这一天、
//! 这一周、这个月用了多少加回来（见 `tw_gateway::key_limits`）。
//!
//! **网关和存储层互不依赖**（两件平级的事），线在这里接：除了 twcore，控制面是唯一同时
//! 看得见两边的地方。twcore 起来时接上，测试也从这里接。

use std::sync::Arc;

use tw_gateway::key_limits::Recorded;

/// 存储层每记下一行，交给网关结算。
pub fn settle_hook(gw: &tw_gateway::AppState) -> tw_store::SettleHook {
    let limits = gw.key_limits.clone();
    Arc::new(move |s: &tw_store::Settled| {
        limits.settle(
            s.id,
            s.at_ms,
            &Recorded {
                client: s.client.clone(),
                path: s.path.clone(),
                local: s.local,
                error_code: s.error_code.clone(),
                attempted: s.attempted,
                requests: 1,
                input: s.input,
                output: s.output,
                cache_read: s.cache_read,
                cache_write: s.cache_write,
                // 算不出钱的（没有价格、没有用量）算 0：费用上限只管得住有价格的
                cost_micros: s.cost_micros.unwrap_or(0),
            },
        )
    })
}

/// 从请求记录里把每把密钥这一期用了多少加回来。**在第一个请求之前调**。读不了库就
/// 从 0 起，只记一行 —— 观测挂了，转发照常。
pub fn rebuild(gw: &tw_gateway::AppState, db: &tw_store::Db) {
    gw.key_limits.rebuild(|since| match db.key_usage_since(since) {
        Ok(rows) => rows.into_iter().map(recorded).collect(),
        Err(e) => {
            tracing::warn!(
                "the usage of the gateway keys could not be read back, so their limits count from zero: {e}"
            );
            Vec::new()
        }
    });
}

fn recorded(u: tw_store::KeyUsage) -> Recorded {
    let n = |v: i64| v.max(0) as u64;
    Recorded {
        client: u.client,
        path: u.path,
        local: false,
        error_code: u.error_code,
        attempted: u.attempted,
        requests: n(u.requests),
        input: n(u.input_tokens),
        output: n(u.output_tokens),
        cache_read: n(u.cache_read_tokens),
        cache_write: n(u.cache_write_tokens),
        cost_micros: u.cost_micros,
    }
}
