//! 把事件缝成一行，算好钱，落库（DESIGN.md §8、§4.3）。
//!
//! **它跑在数据面之外。**观测挂了，代理照跑（§4.7）—— 所以这里的每一个
//! 错误都只记一行日志，一个都不往回抛。
//!
//! 一次请求由四类事件描述（开始、响应头、结束、失败），它们分别到达，
//! 中间可能隔着几分钟。这里攒着它们，齐了就写一条。

use std::collections::HashMap;

use tw_api::Event;
use tw_pricing::{Cost, Prices, Usage};

use crate::blobs::{Blobs, Which};
use crate::db::{Db, RequestRow};
use crate::disk::{self, DiskLevel};

/// 攒着的一行。
#[derive(Debug, Clone)]
struct Partial {
    at_ms: i64,
    client: String,
    client_hint: Option<String>,
    provider: String,
    model: String,
    path: String,
    status: Option<u16>,
    ttfb_ms: Option<i64>,
    /// 路由决策，JSON。**在结束事件之前到达** —— 尝试链走完才发它
    routing: Option<String>,
    /// 服务它的那家怎么收钱（§4.3.1）
    billing: String,
}

/// 在飞的请求最多攒多少条。
///
/// **一个只发了 `RequestStarted` 就再也没有下文的请求会永远占着位置**
/// —— 客户端 Ctrl+C、进程被杀、上游把连接挂着不放，都会造成它。超了从
/// 最老的开始丢：丢掉的是一条观测记录，而留着它们会慢慢吃掉内存。
const MAX_INFLIGHT: usize = 4096;

pub struct Recorder {
    db: Db,
    blobs: Blobs,
    prices: Prices,
    inflight: HashMap<u64, Partial>,
    level: DiskLevel,
    /// 上次查磁盘的时间。**不是每次写都查** —— statvfs 在每个请求上跑
    /// 是纯粹的浪费，而磁盘不会在两秒内从 10 GB 掉到 100 MB。
    last_check_ms: i64,
}

/// 多久查一次磁盘。
const DISK_CHECK_EVERY_MS: i64 = 30_000;

impl Recorder {
    pub fn new(db: Db, blobs: Blobs, prices: Prices) -> Self {
        Self {
            db,
            blobs,
            prices,
            inflight: HashMap::new(),
            level: DiskLevel::Ok,
            last_check_ms: 0,
        }
    }

    pub fn level(&self) -> DiskLevel {
        self.level
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn blobs(&self) -> &Blobs {
        &self.blobs
    }

    /// 记一次请求体或响应体。
    ///
    /// **在事件之外单独走** —— body 不进事件流：那是个广播通道，每个
    /// 订阅者都会拿到一份拷贝，而 body 可能几百 KB。
    pub fn record_body(&self, at_ms: i64, id: u64, which: Which, body: &[u8], original_len: usize) {
        if !self.level.writes_blobs() {
            return;
        }
        self.blobs
            .put_with_len(at_ms, id as i64, which, body, original_len);
    }

    /// 吃一个事件。
    pub fn on_event(&mut self, ev: &Event) {
        let now = now_ms();
        self.maybe_check_disk(now);
        if !self.level.writes_anything() {
            return;
        }
        match ev {
            Event::RequestStarted {
                id,
                client,
                client_hint,
                provider,
                model,
                path,
                at_ms,
                ..
            } => {
                if self.inflight.len() >= MAX_INFLIGHT {
                    self.drop_oldest();
                }
                self.inflight.insert(
                    *id,
                    Partial {
                        at_ms: *at_ms as i64,
                        client: client.clone(),
                        client_hint: client_hint.clone(),
                        provider: provider.clone(),
                        model: model.clone(),
                        path: path.clone(),
                        status: None,
                        ttfb_ms: None,
                        routing: None,
                        billing: String::new(),
                    },
                );
            }
            Event::RequestRouted {
                id,
                rule,
                group,
                attempts,
                billing,
            } => {
                if let Some(p) = self.inflight.get_mut(id) {
                    p.routing = serde_json::to_string(&tw_api::RoutingView {
                        rule: rule.clone(),
                        group: group.clone(),
                        attempts: attempts.clone(),
                    })
                    .ok();
                    p.billing = billing.clone();
                }
            }
            Event::RequestHeaders {
                id,
                status,
                ttfb_ms,
            } => {
                if let Some(p) = self.inflight.get_mut(id) {
                    p.status = Some(*status);
                    p.ttfb_ms = Some(*ttfb_ms as i64);
                }
            }
            Event::RequestFinished {
                id,
                status,
                bytes,
                duration_ms,
                usage,
            } => {
                let Some(p) = self.inflight.remove(id) else {
                    return;
                };
                let u = usage.map(|u| Usage {
                    input: u.input,
                    output: u.output,
                    cache_read: u.cache_read,
                    cache_write: u.cache_write,
                    cache_1h: u.cache_1h,
                });
                // **上游没给 usage 就没有成本。**估算是 M3 后面的事
                // （tiktoken / count_tokens），而在那之前记一笔 0 是在
                // 撒谎（§4.3）。
                // **不引 tw-config** —— 存储层不该知道配置的形状。这个
                // 字符串是事件契约的一部分，比较它就够了。
                let counts_toward_money = p.billing.is_empty() || p.billing == "per-token";
                // **订阅型不按价目表算钱。**订阅制的边际成本是零，按 API
                // 价目表乘出来的数字是纯虚构的（§4.3.1）—— 而它会混进
                // 「今日花费」里，把一个诚实的面板变成一个编出来的。
                let cost = if counts_toward_money {
                    u.map(|u| self.prices.cost(&p.model, &u, false))
                } else {
                    None
                };
                let (cost_micros, estimated) = match cost {
                    Some(Cost::Known(m)) => (Some(m), false),
                    Some(Cost::Estimated(m)) => (Some(m), true),
                    // 没有价格 / 没有 usage / 不按 token 计费，
                    // 三种都是「这笔账不在这个维度上」
                    Some(Cost::Unpriced { .. }) | None => (None, false),
                };
                // 缓存命中省下了多少（§4.4）。**在这里算，不在查询时算**
                // —— 查询时算意味着要把价目表带进 SQL，而价目表会变，
                // 那样「上周省了多少」会随着一次价格更新悄悄改变。
                let cache_saved_micros = if counts_toward_money {
                    u.and_then(|u| self.prices.cache_saving(&p.model, &u))
                } else {
                    None
                };
                self.write(RequestRow {
                    id: *id as i64,
                    at_ms: p.at_ms,
                    client: p.client,
                    client_hint: p.client_hint,
                    provider: p.provider,
                    model: p.model,
                    path: p.path,
                    status: Some(*status),
                    ttfb_ms: p.ttfb_ms,
                    duration_ms: Some(*duration_ms as i64),
                    bytes: Some(*bytes as i64),
                    input_tokens: u.map(|u| u.input as i64),
                    output_tokens: u.map(|u| u.output as i64),
                    cache_read_tokens: u.map(|u| u.cache_read as i64),
                    cache_write_tokens: u.map(|u| u.cache_write as i64),
                    cost_micros,
                    cost_estimated: estimated,
                    error: None,
                    local: false,
                    routing: p.routing,
                    billing: p.billing,
                    cache_saved_micros,
                });
            }
            Event::RequestFailed { id, message, .. } => {
                let Some(p) = self.inflight.remove(id) else {
                    return;
                };
                // **失败也要落库。**「昨天有多少请求失败了」是这个面板
                // 最有用的问题之一，而只记成功的话它永远答不出来。
                self.write(RequestRow {
                    id: *id as i64,
                    at_ms: p.at_ms,
                    client: p.client,
                    client_hint: p.client_hint,
                    provider: p.provider,
                    model: p.model,
                    path: p.path,
                    status: p.status,
                    ttfb_ms: p.ttfb_ms,
                    duration_ms: None,
                    bytes: None,
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    cost_micros: None,
                    cost_estimated: false,
                    error: Some(message.clone()),
                    local: false,
                    routing: p.routing,
                    billing: p.billing,
                    cache_saved_micros: None,
                });
            }
            Event::LocallyAnswered {
                id,
                client,
                probe,
                at_ms,
            } => {
                // 成本 0、延迟 0，**而且明确标 local** —— 汇总会把它们
                // 排除在平均值之外，同时单独计数（§4.8）。
                self.write(RequestRow {
                    id: *id as i64,
                    at_ms: *at_ms as i64,
                    client: client.clone(),
                    // 本地应答的探测请求没经过上游，也就没有旁证可言
                    client_hint: None,
                    provider: String::new(),
                    model: String::new(),
                    path: probe.clone(),
                    status: Some(200),
                    ttfb_ms: Some(0),
                    duration_ms: Some(0),
                    bytes: None,
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    cost_micros: Some(0),
                    cost_estimated: false,
                    error: None,
                    local: true,
                    // 本地应答没走路由 —— 它根本没到上游
                    routing: None,
                    billing: String::new(),
                    cache_saved_micros: None,
                });
            }
            Event::LeakSeen {
                id,
                provider,
                secret,
                masked,
                at_ms,
            } => {
                // **它自己一张表。**一次请求可能同时带出好几种凭据，
                // 而「过去 7 天有 3 个请求把 key 发给了 relay-cn」这句话
                // 要按 (provider, kind) 分组数（§5.0）。
                if let Err(e) = self.db.insert_leak(&crate::db::Leak {
                    at_ms: *at_ms as i64,
                    request_id: *id as i64,
                    provider: provider.clone(),
                    kind: secret.clone(),
                    masked: masked.clone(),
                }) {
                    tracing::debug!("发现记不下来：{e}");
                }
            }
            // 配置事件和额度事件都不是请求，不落这张表。**额度是按
            // provider 的当前状态，不是按请求的历史** —— 它的家在别处。
            Event::ConfigReloaded { .. }
            | Event::ConfigRejected { .. }
            | Event::QuotaSeen { .. } => {}
        }
    }

    fn write(&self, r: RequestRow) {
        if let Err(e) = self.db.insert(&r) {
            // **不往回抛。**观测挂了，代理照跑。
            tracing::debug!(id = r.id, "请求记不下来：{e}");
        }
    }

    /// 丢最老的那条在飞记录。
    fn drop_oldest(&mut self) {
        if let Some((&id, _)) = self.inflight.iter().min_by_key(|(_, p)| p.at_ms) {
            self.inflight.remove(&id);
        }
    }

    fn maybe_check_disk(&mut self, now: i64) {
        if now - self.last_check_ms < DISK_CHECK_EVERY_MS {
            return;
        }
        self.last_check_ms = now;
        let Some(free) = disk::free_bytes(self.blobs.root()) else {
            return;
        };
        let next = disk::level_for(free);
        if next != self.level {
            // 级别变了要说出来 —— 用户点开一个请求发现没有 body，
            // 得知道那不是 bug。
            tracing::warn!(
                free_mb = free / 1024 / 1024,
                "磁盘级别变了：{}",
                next.label()
            );
            self.level = next;
        }
    }

    /// 定期回收。**返回删了多少字节**，调用方记一行日志就够了。
    pub fn gc(&self, now_ms: i64, keep_days: u64, metadata_keep_days: u64, max_bytes: u64) -> u64 {
        let freed = self.blobs.gc(now_ms, keep_days, max_bytes);
        let cutoff = now_ms - (metadata_keep_days as i64) * 86_400_000;
        match self.db.prune_before(cutoff) {
            Ok(n) if n > 0 => tracing::info!(rows = n, "清掉了过期的请求记录"),
            Err(e) => tracing::debug!("清理请求记录失败：{e}"),
            _ => {}
        }
        freed
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn rec() -> (tempfile::TempDir, Recorder) {
        let d = tempfile::tempdir().unwrap();
        let r = Recorder::new(
            Db::in_memory().unwrap(),
            Blobs::new(d.path().join("blobs")),
            Prices::builtin().unwrap(),
        );
        (d, r)
    }

    pub(super) fn started(id: u64, model: &str) -> Event {
        Event::RequestStarted {
            id,
            client_hint: None,
            client: "claude-code".into(),
            provider: "官方".into(),
            model: model.into(),
            method: "POST".into(),
            path: "/v1/messages".into(),
            at_ms: 1_000_000,
        }
    }

    pub(super) fn finished(id: u64, usage: Option<tw_api::UsageView>) -> Event {
        Event::RequestFinished {
            id,
            status: 200,
            bytes: 1234,
            duration_ms: 4000,
            usage,
        }
    }

    #[test]
    fn four_events_become_one_row() {
        let (_d, mut r) = rec();
        r.on_event(&started(1, "claude-sonnet-4-5"));
        r.on_event(&Event::RequestHeaders {
            id: 1,
            status: 200,
            ttfb_ms: 800,
        });
        r.on_event(&finished(
            1,
            Some(tw_api::UsageView {
                input: 1000,
                output: 500,
                ..Default::default()
            }),
        ));
        let row = r.db().get(1).unwrap().unwrap();
        assert_eq!(row.client, "claude-code");
        assert_eq!(row.model, "claude-sonnet-4-5");
        assert_eq!(row.ttfb_ms, Some(800), "响应头那个事件的信息丢了");
        assert_eq!(row.duration_ms, Some(4000));
        assert_eq!(row.input_tokens, Some(1000));
    }

    #[test]
    fn a_real_model_gets_a_real_cost() {
        let (_d, mut r) = rec();
        r.on_event(&started(1, "claude-sonnet-4-5"));
        r.on_event(&finished(
            1,
            Some(tw_api::UsageView {
                input: 100_000,
                output: 0,
                ..Default::default()
            }),
        ));
        let row = r.db().get(1).unwrap().unwrap();
        // Sonnet 4.5 输入 $3 / 百万 token，10 万 token 就是 $0.30
        assert_eq!(row.cost_micros, Some(300_000));
        assert!(!row.cost_estimated);

        // **超过 200k 之后换档。**写这条是因为上一条一开始用了 100 万
        // token，然后拿到了 $6 而不是 $3 —— 那不是 bug，是长上下文分层
        // 真的在生效，而我当时以为是算错了。
        r.on_event(&started(2, "claude-sonnet-4-5"));
        r.on_event(&finished(
            2,
            Some(tw_api::UsageView {
                input: 300_000,
                ..Default::default()
            }),
        ));
        let long = r.db().get(2).unwrap().unwrap();
        assert_eq!(
            long.cost_micros,
            Some(1_800_000),
            "30 万 token 该按 $6/百万 算"
        );
    }

    #[test]
    fn a_model_with_no_price_gets_no_cost_rather_than_zero() {
        // **成本三态的第三态。**记成 0 会让总额悄悄偏低（§4.3）。
        let (_d, mut r) = rec();
        r.on_event(&started(1, "中转站自己起的名字"));
        r.on_event(&finished(
            1,
            Some(tw_api::UsageView {
                input: 1000,
                ..Default::default()
            }),
        ));
        let row = r.db().get(1).unwrap().unwrap();
        assert_eq!(row.cost_micros, None);
        assert_eq!(row.input_tokens, Some(1000), "token 数还是知道的");
    }

    #[test]
    fn an_upstream_that_gives_no_usage_gets_no_cost_either() {
        // 记一笔 0 是在撒谎（§4.3）。
        let (_d, mut r) = rec();
        r.on_event(&started(1, "claude-sonnet-4-5"));
        r.on_event(&finished(1, None));
        let row = r.db().get(1).unwrap().unwrap();
        assert_eq!(row.cost_micros, None);
        assert_eq!(row.input_tokens, None);
    }

    #[test]
    fn a_failed_request_is_recorded_too() {
        // 「昨天有多少请求失败了」是这个面板最有用的问题之一，而只记
        // 成功的话它永远答不出来。
        let (_d, mut r) = rec();
        r.on_event(&started(1, "claude-sonnet-4-5"));
        r.on_event(&Event::RequestFailed {
            id: 1,
            source: "upstream".into(),
            message: "连不上".into(),
        });
        let row = r.db().get(1).unwrap().unwrap();
        assert_eq!(row.error.as_deref(), Some("连不上"));
        assert_eq!(r.db().summary(0, i64::MAX).unwrap().failed, 1);
    }

    #[test]
    fn a_locally_answered_probe_is_marked_local() {
        let (_d, mut r) = rec();
        r.on_event(&Event::LocallyAnswered {
            id: 9,
            client: "claude-code".into(),
            probe: "连通性检查".into(),
            at_ms: 1_000_000,
        });
        let row = r.db().get(9).unwrap().unwrap();
        assert!(row.local);
        let s = r.db().summary(0, i64::MAX).unwrap();
        assert_eq!(s.requests, 0, "本地应答混进了请求总数");
        assert_eq!(s.locally_answered, 1);
    }

    #[test]
    fn a_finish_without_a_start_is_ignored_rather_than_writing_a_row_full_of_holes() {
        // 进程刚起来时，上一轮的请求可能还会送来结束事件。
        let (_d, mut r) = rec();
        r.on_event(&finished(1, None));
        assert_eq!(r.db().count().unwrap(), 0);
    }

    #[test]
    fn requests_that_never_finish_do_not_grow_the_map_without_bound() {
        // 客户端 Ctrl+C、进程被杀、上游把连接挂着不放，都会造成它。
        let (_d, mut r) = rec();
        for i in 0..(MAX_INFLIGHT + 100) as u64 {
            r.on_event(&started(i, "m"));
        }
        assert!(
            r.inflight.len() <= MAX_INFLIGHT,
            "攒了 {}",
            r.inflight.len()
        );
    }

    #[test]
    fn nothing_is_written_when_the_disk_is_full() {
        // **但请求照样通** —— 这一层根本不参与转发。
        let (_d, mut r) = rec();
        r.level = DiskLevel::Nothing;
        r.last_check_ms = i64::MAX; // 别让它去查真实磁盘把级别改回来
        r.on_event(&started(1, "claude-sonnet-4-5"));
        r.on_event(&finished(1, None));
        assert_eq!(r.db().count().unwrap(), 0);
    }

    #[test]
    fn metadata_is_still_written_when_only_blobs_are_stopped() {
        // 中间那一级的意义就在这里：**先牺牲 body，留住摘要**。
        let (_d, mut r) = rec();
        r.level = DiskLevel::MetadataOnly;
        r.last_check_ms = i64::MAX;
        r.on_event(&started(1, "claude-sonnet-4-5"));
        r.on_event(&finished(1, None));
        assert_eq!(r.db().count().unwrap(), 1);
        r.record_body(1_000_000, 1, Which::Request, b"body", 4);
        assert!(
            r.blobs().get(1_000_000, 1, Which::Request).is_none(),
            "这一级不该写 body"
        );
    }

    #[test]
    fn config_events_do_not_end_up_in_the_request_table() {
        let (_d, mut r) = rec();
        r.on_event(&Event::ConfigReloaded {
            id: 1,
            version: "blake3:x".into(),
            origin: "界面".into(),
            at_ms: 0,
        });
        assert_eq!(r.db().count().unwrap(), 0);
    }
}

#[cfg(test)]
mod billing_tests {
    use super::tests::{finished, rec, started};
    use super::*;

    fn routed(id: u64, billing: &str) -> Event {
        Event::RequestRouted {
            id,
            rule: "兜底".into(),
            group: None,
            attempts: vec![tw_api::AttemptView {
                provider: "订阅账号".into(),
                outcome: "成功".into(),
                ms: 5,
            }],
            billing: billing.into(),
        }
    }

    fn usage() -> Option<tw_api::UsageView> {
        Some(tw_api::UsageView {
            input: 100_000,
            output: 1000,
            ..Default::default()
        })
    }

    #[test]
    fn a_subscription_call_does_not_get_a_made_up_price() {
        // **订阅制的边际成本是零，按 API 价目表乘出来的数字是纯虚构的**
        // （§4.3.1）。混进「今日花费」里，就把一个诚实的面板变成了一个
        // 编出来的。
        let (_d, mut r) = rec();
        r.on_event(&started(1, "claude-sonnet-4-5"));
        r.on_event(&routed(1, "subscription"));
        r.on_event(&finished(1, usage()));
        let row = r.db().get(1).unwrap().unwrap();
        assert_eq!(row.cost_micros, None, "订阅调用被按价目表算了钱");
        // **token 数还是要记的** —— 那才是订阅用户该看的量
        assert_eq!(row.input_tokens, Some(100_000));
        assert_eq!(row.billing, "subscription");
    }

    #[test]
    fn a_per_token_call_next_to_it_still_gets_priced() {
        let (_d, mut r) = rec();
        r.on_event(&started(1, "claude-sonnet-4-5"));
        r.on_event(&routed(1, "per-token"));
        r.on_event(&finished(1, usage()));
        assert!(r.db().get(1).unwrap().unwrap().cost_micros.is_some());
    }

    #[test]
    fn the_summary_keeps_subscription_calls_out_of_the_money_but_counts_them() {
        // 「混了订阅上游之后，Dashboard 的今日花费要拆成三栏：实测计费、
        // 估算计费、订阅调用量」（§4.3.1）。
        let (_d, mut r) = rec();
        r.on_event(&started(1, "claude-sonnet-4-5"));
        r.on_event(&routed(1, "per-token"));
        r.on_event(&finished(1, usage()));
        r.on_event(&started(2, "claude-sonnet-4-5"));
        r.on_event(&routed(2, "subscription"));
        r.on_event(&finished(2, usage()));

        let s = r.db().summary(0, i64::MAX).unwrap();
        assert_eq!(s.requests, 2);
        assert!(s.cost_micros_exact > 0, "按量那条该有钱");
        assert_eq!(s.subscription_requests, 1);
        assert_eq!(s.subscription_tokens, 101_000, "订阅那条的 token 量");
        // **订阅的不算「没有价格」** —— 那不是「不知道」，是「这笔账不在
        // 这个维度上」，两者在界面上是两句不同的话
        assert_eq!(s.unpriced_requests, 0);
    }

    #[test]
    fn a_model_with_no_price_is_still_counted_as_unpriced_not_as_subscription() {
        let (_d, mut r) = rec();
        r.on_event(&started(1, "中转站自己起的名字"));
        r.on_event(&routed(1, "per-token"));
        r.on_event(&finished(1, usage()));
        let s = r.db().summary(0, i64::MAX).unwrap();
        assert_eq!(s.unpriced_requests, 1);
        assert_eq!(s.subscription_requests, 0);
    }
}

#[cfg(test)]
mod cache_saving_tests {
    use super::tests::{finished, rec, started};
    use tw_api::UsageView;

    #[test]
    fn a_cache_hit_records_how_much_it_saved() {
        // §4.4：Dashboard 上要有独立的「缓存节省了多少钱」。而算的是
        // **差额** —— 「如果这些 token 没命中缓存，要多花多少」。
        let (_d, mut r) = rec();
        r.on_event(&started(1, "claude-sonnet-4-5"));
        r.on_event(&finished(
            1,
            Some(UsageView {
                input: 1000,
                output: 100,
                cache_read: 100_000,
                ..Default::default()
            }),
        ));
        let row = r.db().get(1).unwrap().unwrap();
        // Sonnet 4.5：输入 $3、缓存读 $0.30 → 十万 token 省 $0.27
        assert_eq!(row.cache_saved_micros, Some(270_000));
        assert_eq!(
            r.db().summary(0, i64::MAX).unwrap().cache_saved_micros,
            270_000
        );
    }

    #[test]
    fn a_request_with_no_cache_hit_saved_a_real_zero() {
        let (_d, mut r) = rec();
        r.on_event(&started(1, "claude-sonnet-4-5"));
        r.on_event(&finished(
            1,
            Some(UsageView {
                input: 1000,
                ..Default::default()
            }),
        ));
        assert_eq!(r.db().get(1).unwrap().unwrap().cache_saved_micros, Some(0));
    }

    #[test]
    fn an_unpriced_model_cannot_say_how_much_the_cache_saved() {
        // **「省了 0 元」和「算不出来省了多少」是两句不同的话**（§4.3）。
        let (_d, mut r) = rec();
        r.on_event(&started(1, "中转站自己起的名字"));
        r.on_event(&finished(
            1,
            Some(UsageView {
                cache_read: 100_000,
                ..Default::default()
            }),
        ));
        assert_eq!(r.db().get(1).unwrap().unwrap().cache_saved_micros, None);
    }

    #[test]
    fn latency_by_provider_answers_a_different_question_than_by_model() {
        // 「哪个模型慢」和「哪家上游慢」的下一步完全不同：前者换模型，
        // 后者换上游（M3 验收问的是后者）。
        let (_d, mut r) = rec();
        for (id, model) in [(1u64, "claude-opus-4-1"), (2, "claude-haiku-4-5")] {
            r.on_event(&started(id, model));
            r.on_event(&tw_api::Event::RequestHeaders {
                id,
                status: 200,
                ttfb_ms: if id == 1 { 3000 } else { 300 },
            });
            r.on_event(&finished(id, None));
        }
        let by_model = r.db().latency_by_model(0, i64::MAX).unwrap();
        assert_eq!(by_model.len(), 2, "两个模型该分开");
        let by_provider = r.db().latency_by_provider(0, i64::MAX).unwrap();
        assert_eq!(by_provider.len(), 1, "同一家上游该合成一行");
        assert_eq!(by_provider[0].model, "官方");
        assert_eq!(by_provider[0].samples, 2);
    }
}
