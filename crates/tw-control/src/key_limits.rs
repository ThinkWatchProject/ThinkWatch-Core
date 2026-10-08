//! 密钥的用量上限和存储层之间的三根线：每记下一行请求就结算，启动时从请求记录里把这一天、
//! 这一周、这个月用了多少加回来，一期的开头变了（到了下一期、机器换了时区）时把新的那一期
//! 重新加起来（见 `tw_gateway::key_limits`）。
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
                reached: s.reached,
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

/// 一期的开头变了：从请求记录里把新的这一期重新加起来（见
/// `tw_gateway::key_limits::KeyLimits::reread_with`）。启动时 [`rebuild`] 之后接上。
///
/// **在存储层那把锁里读、在锁里交回去**：结算也在那把锁里（存储层记下一行时调
/// [`settle_hook`]），拿着它读到的就是此刻结算过的全部，换上去一行不多、一行不少。读库
/// 在另起的任务上：要它的那一刻可能正拿着这把锁（结算一行时发现到了下一期）。读不了库就
/// 从 0 起，只记一行
pub fn follow(gw: &tw_gateway::AppState, store: Arc<tokio::sync::Mutex<tw_store::Recorder>>) {
    // 弱引用：网关的账拿着这个办法，办法再拿着账就是一个圈，谁都放不掉
    let limits = Arc::downgrade(&gw.key_limits);
    // **接上时的运行时，记在这里**：结算在存储层自己的记录线程上，那条线程不在运行时里，在
    // 那儿问「当前的运行时」问不到，新的一期就永远不会从记录里加回来
    let here = tokio::runtime::Handle::try_current().ok();
    gw.key_limits
        .reread_with(Arc::new(move |m: tw_gateway::key_limits::Moved| {
            let (limits, store) = (limits.clone(), store.clone());
            let Some(rt) = here
                .clone()
                .or_else(|| tokio::runtime::Handle::try_current().ok())
            else {
                return;
            };
            rt.spawn(async move {
                let rec = store.lock().await;
                let Some(limits) = limits.upgrade() else {
                    return;
                };
                match rec.db().key_usage_since(m.start) {
                    Ok(rows) => {
                        let rows: Vec<Recorded> = rows.into_iter().map(recorded).collect();
                        limits.reread(&m, &rows);
                    }
                    Err(e) => tracing::warn!(
                        key = %m.key,
                        "the usage of a gateway key for its new period could not be read back, \
                         so it counts from zero: {e}"
                    ),
                }
            });
        }));
}

fn recorded(u: tw_store::KeyUsage) -> Recorded {
    let n = |v: i64| v.max(0) as u64;
    Recorded {
        client: u.client,
        path: u.path,
        local: false,
        reached: u.reached,
        requests: n(u.requests),
        input: n(u.input_tokens),
        output: n(u.output_tokens),
        cache_read: n(u.cache_read_tokens),
        cache_write: n(u.cache_write_tokens),
        cost_micros: u.cost_micros,
    }
}
