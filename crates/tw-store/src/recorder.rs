//! 把事件缝成一行，算好钱，落库。
//!
//! **它跑在数据面之外。**观测挂了，代理照跑 —— 所以这里的每一个
//! 错误都只记一行日志，一个都不往回抛。
//!
//! 一次请求由几类事件描述（开始、响应头，以及结束、失败、客户端取消三者
//! 之一），它们分别到达，中间可能隔着几分钟。这里攒着它们，齐了就写一条。

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
    /// 响应里有几个工具调用、命中几条规则（防线三）
    tool_calls: Option<i64>,
    flagged: Option<i64>,
    /// 出站脱敏换掉了几处（防线一）。`None` = 那次没开脱敏
    redacted: Option<i64>,
    client: String,
    client_hint: Option<String>,
    session: Option<String>,
    provider: String,
    model: String,
    path: String,
    status: Option<u16>,
    ttfb_ms: Option<i64>,
    /// 路由决策，JSON。**在结束事件之前到达** —— 尝试链走完才发它
    routing: Option<String>,
    /// 服务它的那家怎么收钱
    billing: String,
}

/// 在飞的请求最多攒多少条。
///
/// **一个只发了 `RequestStarted` 就再也没有下文的请求会一直占着位置。**
/// 网关给每个开始了的请求都报一个结局（见 `tw_gateway::ending`），所以
/// 剩下的来源是结局还没来得及发出去进程就没了，以及上游把连接挂着不放、
/// 客户端又一直在等的那些 —— 它们迟早有结局，只是可能很久。超了从最老的
/// 开始丢：丢掉的是一条观测记录，而留着它们会慢慢吃掉内存。
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
    /// 每个对话指纹当前归到哪一次会话，以及它最后一次出现是什么时候。
    ///
    /// **只在内存里。**core 重启之后，同一段对话会被算成新的一次任务 ——
    /// 那不理想，但比把它持久化成第四份状态好：会话是个观测概念，不是
    /// 事实来源。
    sessions: HashMap<String, (String, i64)>,
    /// 算完价钱之后往回报一条。
    ///
    /// **这一层是唯一知道价钱的地方** —— 网关只知道用了多少 token，
    /// 单价在这里查价目表。不报的话，界面想知道花了多少就只能在请求
    /// 结束之后回库里再查一遍。
    ///
    /// `None` 表示没人要听（测试、以及不带总线的调用方）。
    bus: Option<tw_observe::EventBus>,
}

/// 多久查一次磁盘。
const DISK_CHECK_EVERY_MS: i64 = 30_000;

/// 隔多久算另一次任务。
///
/// **半小时是按「人」定的，不是按机器**：中间去开了个会再回来接着改，
/// 那多半还是同一件事；隔了一夜再打开同一个仓库，那通常不是。切错的
/// 代价是对称的（并多了或分多了），所以取一个人能理解的整数。
const SESSION_GAP_MS: i64 = 30 * 60 * 1000;

impl Recorder {
    pub fn new(db: Db, blobs: Blobs, prices: Prices) -> Self {
        Self {
            db,
            blobs,
            prices,
            inflight: HashMap::new(),
            sessions: HashMap::new(),
            level: DiskLevel::Ok,
            last_check_ms: 0,
            bus: None,
        }
    }

    /// 把算出来的价钱报回总线上。
    pub fn reporting_to(mut self, bus: tw_observe::EventBus) -> Self {
        self.bus = Some(bus);
        self
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
    /// 指纹 → 会话 id。同一个指纹隔太久再出现，算新的一次任务。
    fn session_for(&mut self, fp: &str, at_ms: i64) -> String {
        // **会话 id 里带着起始时刻**，所以同一段对话隔天再聊会得到两条
        // 记录 —— 而那正是我们想要的：它们是两次任务
        let fresh = format!("{fp}-{at_ms}");
        let e = self
            .sessions
            .entry(fp.to_string())
            .or_insert((fresh.clone(), at_ms));
        if at_ms - e.1 > SESSION_GAP_MS {
            *e = (fresh, at_ms);
        } else {
            e.1 = at_ms.max(e.1);
        }
        e.0.clone()
    }

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
                session_fp,
                provider,
                model,
                path,
                at_ms,
                ..
            } => {
                if self.inflight.len() >= MAX_INFLIGHT {
                    self.drop_oldest();
                }
                // **指纹在这里变成会话 id**：同一个指纹、离上一条不太久，
                // 就还是那一次任务；隔久了就是新的一次
                let session = session_fp
                    .as_ref()
                    .map(|fp| self.session_for(fp, *at_ms as i64));
                self.inflight.insert(
                    *id,
                    Partial {
                        at_ms: *at_ms as i64,
                        client: client.clone(),
                        client_hint: client_hint.clone(),
                        session,
                        tool_calls: None,
                        flagged: None,
                        redacted: None,
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
                    // **归到实际服务的那家，不是第一个候选。**
                    // `RequestStarted` 发出时只知道候选链的头一个，而故障
                    // 转移之后那一家恰恰是失败的那一家。不改的话成本记在
                    // 没服务的上游头上，而「哪家上游慢」会把成功
                    // 那一跳的延迟算给超时的那一家 —— 两个数字都指向错的
                    // 上游，而且没有任何东西会提示它们错了。
                    //
                    // 判据是「链的最后一跳」，不是按 outcome 的字符串匹配
                    // 「成功」：转移在第一次成功时就 break，所以最后一跳
                    // 要么是服务的那家，要么是放弃前试的最后一家。两种都
                    // 是这一行该归的对象，而这条性质是结构性的 —— 改了
                    // outcome 的措辞不会让它失效。
                    if let Some(last) = attempts.last() {
                        p.provider = last.provider.clone();
                    }
                    p.routing = serde_json::to_string(&tw_api::RoutingView {
                        rule: rule.clone(),
                        group: group.clone(),
                        attempts: attempts.clone(),
                    })
                    .ok();
                    p.billing = billing.clone();
                }
            }
            /*
                这次请求里换掉了几处。

                **只记条数，不记内容。**换掉的正是不能落盘的东西 ——
                记下来等于把外泄搬了个家。

                以前这条事件整个不落表，理由是「它属于请求详情」。但
                请求详情本身也没存它，于是切到「拦截」档之后，面板上
                那条证据链断了：观察档看得见「检测到 12 处外泄」，拦截
                档反而什么都没有，而后者是防护更强的一档。
            */
            Event::Redacted { id, items, .. } => {
                if let Some(p) = self.inflight.get_mut(id) {
                    let n: i64 = items.iter().map(|x| x.count as i64).sum();
                    p.redacted = Some(p.redacted.unwrap_or(0) + n);
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
            } => self.settle(*id, Some(*status), *bytes, *duration_ms, *usage, false),
            /*
                客户端没等到响应结束就走了。

                **照样落库，照样算钱。**上游那边已经计了费：输入全额，输出
                算到断开为止。以前这类请求只在内存里挂着，从来不写 —— 那笔
                钱就从账上消失了。

                它和正常结束只差一个标记，所以走同一条路。
            */
            Event::RequestCancelled {
                id,
                status,
                bytes,
                duration_ms,
                usage,
            } => self.settle(*id, *status, *bytes, *duration_ms, *usage, true),
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
                    session: p.session,
                    tool_calls: p.tool_calls,
                    flagged: p.flagged,
                    redacted: p.redacted,
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
                    cancelled: false,
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
                // 排除在平均值之外，同时单独计数。
                self.write(RequestRow {
                    id: *id as i64,
                    at_ms: *at_ms as i64,
                    client: client.clone(),
                    // 本地应答的探测请求没经过上游，也就没有旁证可言
                    client_hint: None,
                    session: None,
                    tool_calls: None,
                    flagged: None,
                    redacted: None,
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
                    cancelled: false,
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
                // 要按 (provider, kind) 分组数。
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
            // 配置事件、额度事件、扫描告警都不是请求，不落这张表。
            // **额度是按 provider 的当前状态，不是按请求的历史** ——
            // 它的家在别处；扫描告警同理，它说的是磁盘上的文件。
            Event::ConfigReloaded { .. }
            | Event::ConfigRejected { .. }
            | Event::QuotaSeen { .. }
            | Event::ScanAlert { .. }
            | Event::ToolCallFlagged { .. }
            | Event::Translated { .. }
            // 凭据轮换说的是配置文件该改了，跟哪一次请求无关
            | Event::CredentialRotated { .. }
            // 客户端配置面变了、某家上游熔断了 —— 都是「现在什么情况」，
            // 不是「刚才发生过什么」。这张表只装后者。
            | Event::ClientsChanged { .. }
            | Event::HealthChanged { .. }
            // 自己刚报出去的那条。**不能再处理一遍** —— 那是一个回路
            | Event::RequestPriced { .. } => {}
            Event::ResponseInspected {
                id,
                tool_calls,
                flagged,
            } => {
                if let Some(p) = self.inflight.get_mut(id) {
                    p.tool_calls = Some(*tool_calls as i64);
                    p.flagged = Some(*flagged as i64);
                }
            }
        }
    }

    /// 一个拿到了用量的结局：跑完了，或者客户端中途走了。算钱，落库。
    fn settle(
        &mut self,
        id: u64,
        // 响应头之前客户端就走了的，没有状态码
        status: Option<u16>,
        bytes: u64,
        duration_ms: u64,
        usage: Option<tw_api::UsageView>,
        cancelled: bool,
    ) {
        let Some(p) = self.inflight.remove(&id) else {
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
        // 撒谎。
        // **不引 tw-config** —— 存储层不该知道配置的形状。这个
        // 字符串是事件契约的一部分，比较它就够了。
        let counts_toward_money = p.billing.is_empty() || p.billing == "per-token";
        // **订阅型不按价目表算钱。**订阅制的边际成本是零，按 API
        // 价目表乘出来的数字是纯虚构的 —— 而它会混进
        // 「今日花费」里，把一个诚实的面板变成一个编出来的。
        //
        // **取消的一律按估算记。**输出只算到断开那一刻，而 Anthropic 在流的
        // 末尾才报累计输出 —— 断在中间时手里那个数是个占位。按它算出来的钱
        // 只会偏低，当成实测会让「今日花费」悄悄少一截。
        let cost = if counts_toward_money {
            u.map(|u| self.prices.cost(&p.model, &u, cancelled))
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
        // 缓存命中省下了多少。**在这里算，不在查询时算**
        // —— 查询时算意味着要把价目表带进 SQL，而价目表会变，
        // 那样「上周省了多少」会随着一次价格更新悄悄改变。
        let cache_saved_micros = if counts_toward_money {
            u.and_then(|u| self.prices.cache_saving(&p.model, &u))
        } else {
            None
        };
        // **算完就报，不等人来问。**这是整条链上唯一知道价钱的
        // 地方，而界面上那一列金额在它到达之前只能是「—」。
        if let Some(bus) = &self.bus {
            bus.emit(Event::RequestPriced {
                id,
                cost_micros,
                cost_estimated: estimated,
                cache_saved_micros,
                at_ms: p.at_ms as u64,
            });
        }
        self.write(RequestRow {
            id: id as i64,
            at_ms: p.at_ms,
            client: p.client,
            client_hint: p.client_hint,
            session: p.session,
            tool_calls: p.tool_calls,
            flagged: p.flagged,
            redacted: p.redacted,
            provider: p.provider,
            model: p.model,
            path: p.path,
            status: status.or(p.status),
            ttfb_ms: p.ttfb_ms,
            duration_ms: Some(duration_ms as i64),
            bytes: Some(bytes as i64),
            input_tokens: u.map(|u| u.input as i64),
            output_tokens: u.map(|u| u.output as i64),
            cache_read_tokens: u.map(|u| u.cache_read as i64),
            cache_write_tokens: u.map(|u| u.cache_write as i64),
            cost_micros,
            cost_estimated: estimated,
            error: None,
            local: false,
            cancelled,
            routing: p.routing,
            billing: p.billing,
            cache_saved_micros,
        });
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
            session_fp: None,
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

    /// 故障转移之后，这一行该归给**实际服务的那家**。
    ///
    /// `RequestStarted` 发的是候选链的头一个，而转移之后那家恰恰是失败
    /// 的。归错的后果不是一条难看的记录：成本记在没服务的上游头上，
    /// 而「哪家上游慢」会把成功那一跳的 5 秒算给超时 10 秒的那一家。
    /// 两个数字都指向错的上游，而且没有任何东西会提示它们错了。
    ///
    /// 这条是真机上撞出来的：冒烟脚本里 relay 超时、official 接手，
    /// 而 `/history` 那一行写着 relay。
    #[test]
    fn a_failover_attributes_the_row_to_whoever_served_it() {
        let (_d, mut r) = rec();
        r.on_event(&started(1, "claude-sonnet-4-5")); // provider = 官方
        r.on_event(&Event::RequestRouted {
            id: 1,
            rule: "默认".into(),
            group: Some("__all__".into()),
            attempts: vec![
                tw_api::AttemptView {
                    provider: "官方".into(),
                    outcome: "上游超时".into(),
                    ms: 10_003,
                },
                tw_api::AttemptView {
                    provider: "中转".into(),
                    outcome: "成功".into(),
                    ms: 5_042,
                },
            ],
            billing: "per-token".into(),
        });
        r.on_event(&finished(1, None));

        let row = r.db().get(1).unwrap().unwrap();
        assert_eq!(
            row.provider, "中转",
            "转移之后这一行还归给第一个候选，成本和延迟都会记到没服务的那家头上"
        );
        // 尝试链本身一个字都不能少 —— 归属改了，但「试过谁、为什么失败」
        // 是排查的全部价值。
        let routing: tw_api::RoutingView =
            serde_json::from_str(row.routing.as_deref().unwrap()).unwrap();
        assert_eq!(routing.attempts.len(), 2);
        assert_eq!(routing.attempts[0].provider, "官方");
        assert_eq!(routing.attempts[0].outcome, "上游超时");
    }

    /// 一次就成的请求不该被这条规则改坏：链长度为 1，最后一跳就是它自己。
    #[test]
    fn a_request_that_succeeds_first_try_keeps_its_provider() {
        let (_d, mut r) = rec();
        r.on_event(&started(2, "claude-sonnet-4-5"));
        r.on_event(&Event::RequestRouted {
            id: 2,
            rule: "默认".into(),
            group: None,
            attempts: vec![tw_api::AttemptView {
                provider: "官方".into(),
                outcome: "成功".into(),
                ms: 300,
            }],
            billing: "per-token".into(),
        });
        r.on_event(&finished(2, None));
        assert_eq!(r.db().get(2).unwrap().unwrap().provider, "官方");
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
        // **成本三态的第三态。**记成 0 会让总额悄悄偏低。
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
        // 记一笔 0 是在撒谎。
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
        // 结局没来得及发出去进程就没了，或者上游把连接挂着不放。
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
        // **订阅制的边际成本是零，按 API 价目表乘出来的数字是纯虚构的**。
        // 混进「今日花费」里，就把一个诚实的面板变成了一个
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
        // 估算计费、订阅调用量」。
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
mod pricing_report_tests {
    use super::tests::{finished, rec, started};
    use tw_api::UsageView;

    /// **算完的价钱要报回去，不能等界面回头来问。**这一层是整条链上
    /// 唯一知道单价的地方：网关只知道用了多少 token。不报的话，界面上
    /// 那一列金额在请求结束之后只能靠再查一次库才填得上。
    #[test]
    fn the_price_goes_back_onto_the_bus() {
        let (_d, mut r) = rec();
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        r = r.reporting_to(bus);
        r.on_event(&started(1, "claude-sonnet-4-5"));
        r.on_event(&finished(
            1,
            Some(UsageView {
                // 压在 200k 以下：超过之后走的是长上下文那档单价
                input: 100_000,
                output: 0,
                ..Default::default()
            }),
        ));
        let mut priced = None;
        while let Ok(ev) = rx.try_recv() {
            if let tw_api::Event::RequestPriced {
                id, cost_micros, ..
            } = ev
            {
                priced = Some((id, cost_micros));
            }
        }
        // Sonnet 4.5 输入 $3/M —— 十万 token 是 $0.30
        assert_eq!(priced, Some((1, Some(300_000))));
    }

    /// **「算不出来」要原样报出去，不能报成零。**订阅制上游、价目表里
    /// 没有的模型，都是这一档 —— 报 0 的话界面会画一个「免费」。
    #[test]
    fn an_unpriceable_request_reports_nothing_not_zero() {
        let (_d, mut r) = rec();
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        r = r.reporting_to(bus);
        r.on_event(&started(1, "某个中转站的模型"));
        r.on_event(&finished(
            1,
            Some(UsageView {
                input: 1000,
                output: 10,
                ..Default::default()
            }),
        ));
        let mut priced = None;
        while let Ok(ev) = rx.try_recv() {
            if let tw_api::Event::RequestPriced { cost_micros, .. } = ev {
                priced = Some(cost_micros);
            }
        }
        assert_eq!(priced, Some(None));
    }
}

#[cfg(test)]
mod redaction_tests {
    use super::tests::{finished, rec, started};

    /// **拦截档也要留下痕迹。**观察档产出的是「检测到的外泄」证据，
    /// 而拦截档把它们就地换掉了 —— 那一刻如果什么都不记，面板在防护
    /// 最强的一档上反而是空的，读起来像什么都没发生。
    #[test]
    fn a_redaction_is_counted_on_the_row_it_happened_to() {
        let (_d, mut r) = rec();
        r.on_event(&started(1, "claude-sonnet-4-5"));
        r.on_event(&tw_api::Event::Redacted {
            id: 1,
            provider: "relay".into(),
            items: vec![
                tw_api::RedactedItem {
                    kind: "api_key".into(),
                    what: "sk-…".into(),
                    count: 2,
                },
                tw_api::RedactedItem {
                    kind: "token".into(),
                    what: "ghp_…".into(),
                    count: 1,
                },
            ],
            at_ms: 0,
        });
        r.on_event(&finished(1, None));
        assert_eq!(r.db().get(1).unwrap().unwrap().redacted, Some(3));
    }

    /// **「没开脱敏」和「开了但这次没换」是两件事。**记成 0 的话，
    /// 关掉脱敏的那段时间在统计里会变成「一处都没换过」—— 那是假的。
    #[test]
    fn no_redaction_event_means_unknown_not_zero() {
        let (_d, mut r) = rec();
        r.on_event(&started(1, "claude-sonnet-4-5"));
        r.on_event(&finished(1, None));
        assert_eq!(r.db().get(1).unwrap().unwrap().redacted, None);
    }
}

#[cfg(test)]
mod cache_saving_tests {
    use super::tests::{finished, rec, started};
    use tw_api::UsageView;

    #[test]
    fn a_cache_hit_records_how_much_it_saved() {
        // Dashboard 上要有独立的「缓存节省了多少钱」。而算的是
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
        // **「省了 0 元」和「算不出来省了多少」是两句不同的话**。
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

#[cfg(test)]
mod cancellation_tests {
    use super::tests::{finished, rec, started};
    use super::*;
    use tw_api::UsageView;

    fn cancelled(id: u64, usage: Option<UsageView>) -> Event {
        Event::RequestCancelled {
            id,
            status: Some(200),
            bytes: 312,
            duration_ms: 2_500,
            usage,
        }
    }

    /// 按 Esc 那一刻手里的用量：输入是齐的，输出是 `message_start` 里那个
    /// 占位的 1。
    fn partial() -> Option<UsageView> {
        Some(UsageView {
            input: 100_000,
            output: 1,
            ..Default::default()
        })
    }

    /// **这一行以前根本不存在。**客户端中途走掉的请求只在内存里挂着，从来
    /// 不写 —— 而上游已经为那十万个输入 token 计了费。
    #[test]
    fn a_request_the_client_walked_away_from_is_written_and_priced() {
        let (_d, mut r) = rec();
        r.on_event(&started(1, "claude-sonnet-4-5"));
        r.on_event(&Event::RequestHeaders {
            id: 1,
            status: 200,
            ttfb_ms: 900,
        });
        r.on_event(&cancelled(1, partial()));

        let row = r.db().get(1).unwrap().expect("取消的请求没有落库");
        assert!(row.cancelled);
        assert_eq!(row.error, None, "取消不是失败");
        assert_eq!(row.status, Some(200));
        assert_eq!(row.ttfb_ms, Some(900));
        assert_eq!(row.duration_ms, Some(2_500));
        assert_eq!(row.bytes, Some(312));
        assert_eq!(row.input_tokens, Some(100_000));
        assert_eq!(row.output_tokens, Some(1));
        // Sonnet 4.5：输入 $3/M、输出 $15/M —— 十万个输入加一个输出
        assert_eq!(row.cost_micros, Some(300_015));
        assert!(row.cost_estimated, "输出只算到断开那一刻，这个数只能是估算");
    }

    /// 取消不进失败数。**进了的话，一个常按 Esc 的人会看到一个失败率很高
    /// 的面板**，而上游什么都没做错。钱照算，只是算在估算那一栏。
    #[test]
    fn a_cancellation_is_not_a_failure_but_its_money_still_counts() {
        let (_d, mut r) = rec();
        r.on_event(&started(1, "claude-sonnet-4-5"));
        r.on_event(&cancelled(1, partial()));
        r.on_event(&started(2, "claude-sonnet-4-5"));
        r.on_event(&finished(
            2,
            Some(UsageView {
                input: 100_000,
                ..Default::default()
            }),
        ));

        let s = r.db().summary(0, i64::MAX).unwrap();
        assert_eq!(s.requests, 2);
        assert_eq!(s.failed, 0, "取消被算成了失败");
        assert_eq!(s.cost_micros_exact, 300_000);
        assert_eq!(s.cost_micros_estimated, 300_015);
    }

    /// 算出来的价钱照样报回总线，**而且标着估算** —— 界面上那一列金额
    /// 要带着 `~` 出现。
    #[test]
    fn the_price_of_a_cancellation_goes_back_onto_the_bus_as_an_estimate() {
        let (_d, mut r) = rec();
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        r = r.reporting_to(bus);
        r.on_event(&started(1, "claude-sonnet-4-5"));
        r.on_event(&cancelled(1, partial()));

        let priced = std::iter::from_fn(|| rx.try_recv().ok()).find_map(|ev| match ev {
            Event::RequestPriced {
                id,
                cost_micros,
                cost_estimated,
                ..
            } => Some((id, cost_micros, cost_estimated)),
            _ => None,
        });
        assert_eq!(priced, Some((1, Some(300_015), true)));
    }

    /// 客户端在第一帧之前就走了。**没有用量就没有金额，不是零** —— 而这
    /// 一行还是要写：这个请求发生过，本身就是事实。
    #[test]
    fn a_cancellation_before_any_usage_is_written_without_a_price_rather_than_as_free() {
        let (_d, mut r) = rec();
        r.on_event(&started(1, "claude-sonnet-4-5"));
        r.on_event(&cancelled(1, None));

        let row = r.db().get(1).unwrap().expect("没有用量的取消也该落库");
        assert!(row.cancelled);
        assert_eq!(row.input_tokens, None);
        assert_eq!(row.cost_micros, None);
    }

    /// 响应头还没到客户端就走了（非流式请求、慢的中转站）。**没有状态码**
    /// —— 不是 0，也不是随手编一个 499；耗时倒是真的，那是它等了多久。
    #[test]
    fn a_cancellation_before_the_response_headers_has_no_status() {
        let (_d, mut r) = rec();
        r.on_event(&started(1, "claude-sonnet-4-5"));
        r.on_event(&Event::RequestCancelled {
            id: 1,
            status: None,
            bytes: 0,
            duration_ms: 12_000,
            usage: None,
        });

        let row = r.db().get(1).unwrap().expect("响应头之前的取消也该落库");
        assert!(row.cancelled);
        assert_eq!(row.status, None);
        assert_eq!(row.duration_ms, Some(12_000));
        assert_eq!(row.error, None);
    }

    /// 会话里的那一轮也要看得出是取消的。否则在每轮花费里，它就是一轮
    /// 花了钱、输出却只有一个 token 的「正常」请求。
    #[test]
    fn a_cancelled_turn_says_so_in_its_session() {
        let (_d, mut r) = rec();
        r.on_event(&Event::RequestStarted {
            id: 1,
            client: "claude-code".into(),
            client_hint: None,
            session_fp: Some("fp".into()),
            provider: "官方".into(),
            model: "claude-sonnet-4-5".into(),
            method: "POST".into(),
            path: "/v1/messages".into(),
            at_ms: 1_000_000,
        });
        r.on_event(&cancelled(1, partial()));

        let sessions = r.db().sessions(10).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].errors, 0, "取消被算成了会话里的失败");
        let turns = r.db().turns(&sessions[0].id).unwrap();
        assert!(turns[0].cancelled);
    }

    /// 结局只认第一个。同一个 id 后面再来一个结束，**不该把已经写下的那一行
    /// 盖掉** —— 写库用的是 INSERT OR REPLACE，第二次写就是覆盖。
    #[test]
    fn a_second_ending_for_the_same_request_does_not_overwrite_the_first() {
        let (_d, mut r) = rec();
        r.on_event(&started(1, "claude-sonnet-4-5"));
        r.on_event(&cancelled(1, partial()));
        r.on_event(&finished(1, None));

        assert_eq!(r.db().count().unwrap(), 1);
        assert!(r.db().get(1).unwrap().unwrap().cancelled);
    }
}
