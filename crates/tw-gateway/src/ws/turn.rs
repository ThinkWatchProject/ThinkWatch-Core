//! Responses 的 WebSocket 连接上的一轮：**每个 `response.create` 是一个请求**。
//!
//! 从客户端发来这一帧，到上游回完这一次回答（`response.completed`、`response.failed`、
//! `response.incomplete`，或者一个 `error`），和 HTTP 那条路的一个请求一样：开始、路由、
//! 结局三条事件，存储层记一行。用量是这一次回答里的 `usage`（输入含缓存读、输出），结局事件
//! 交给同一个记录器，按发给这一家的模型名、照 HTTP 那条路同一套查价：费用、密钥的用量、
//! 体检、流量看到的都是它。第一个 token 什么时候到、回答里写的是哪个模型，也和 HTTP 那条路
//! 一样认（见 [`Ending::frame`]）。连接半路断了，这一轮记成取消；上游断了，记成失败。尝试链
//! 只有一跳：这条连接连着的那一家。
//!
//! **连接本身不留行**：一条连接跑好几轮、中间可以闲着很久，流量里该看的是每一轮。会话照
//! HTTP 那条路按每一帧认（Codex 每段对话带着 `prompt_cache_key`），同一段对话的几轮归到同一
//! 次会话里。没有哪一轮可挂的 —— 升级时就被规则拒绝的、连不上上游的 —— 那条连接自己留一行
//! （见 `server::upgrade`）。
//!
//! **上限和并发按轮算，和 HTTP 那条路的一个请求同一套**（[`admit`]）：天、周、月用满了就拒，
//! 密钥的并发上限等前面的结束，分钟、小时等得到就等；然后占这一家的一个位置
//! （`max_concurrent`）—— 这条连接只连着这一家，没有下一家可换：满着就等，等不到回
//! `response.failed`。等分钟、小时的空位和等这一家的位置**共用这一轮的一个期限**，和 HTTP
//! 那条路一样。被拒的这一轮替它回一个 `response.failed`，连接照常。占着的（密钥的
//! 并发通行证、这一家的位置）从发出去占到这一次回答完、或者连接断了；**闲着的连接什么都
//! 不占**。
//!
//! **快慢和成败也按轮记，和 HTTP 那条路的一跳同一笔账**：这一家的快慢样本从上游开始答这一轮
//! 量到第一段内容（[`crate::latency`]，认第一段内容的是同一个），成败一轮记一次（熔断和
//! `load-balance` 按成败分的成功率，[`crate::health`]）—— 第一段内容到了是答上了；内容之前就
//! 收了尾的，按上游最后那一帧报的错判断，和 HTTP 那条路开头报错的一跳同一个判据；上游断了是
//! 失败。等位置等不到、被上限拒的、客户端走了的、被防护切断的不记：那不是这一家的错。
//!
//! 上限看的是这一轮开始那一刻的配置，和 HTTP 那条路每个请求一样：去向和防护按升级时的走
//! 到底，上限改了，下一轮就照新的数。

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use crate::ending::Ending;
use crate::error::GatewayError;
use crate::server::Choice;
use crate::state::AppState;
use tw_types::{Msg, msg};

/// 一条连接上开始一个请求时都一样的那几项：谁发的、从哪儿来、哪条路径。整条连接一行的
/// （Realtime 和别的路径）和每一轮一行的（Responses）都由它开始。
#[derive(Clone)]
pub(crate) struct Opener {
    pub(crate) bus: tw_observe::EventBus,
    /// 网关密钥的名字
    pub(crate) client: String,
    pub(crate) client_hint: Option<String>,
    pub(crate) peer: Option<String>,
    pub(crate) key_masked: Option<String>,
    /// 升级的路径
    pub(crate) path: String,
}

/// 一个请求开始时各不相同的那几项。
pub(crate) struct Opening<'a> {
    pub(crate) choice: &'a Choice,
    /// 要发往的那一家和它怎么收钱。一家都不会去的（被拒了）是空的名字
    pub(crate) to: (&'a str, tw_api::Billing),
    /// 客户端要的模型名
    pub(crate) model: String,
    pub(crate) session: Option<String>,
    pub(crate) input_estimate: Option<u64>,
    /// 用时从哪一刻算起，和那一刻的 Unix 毫秒（那一行的 `at_ms`）
    pub(crate) started: Instant,
    pub(crate) at_ms: u64,
}

impl Opener {
    /// 发 `RequestStarted`，交回这个请求的号和它欠着的结局。**WS 的帧不留档**，结局没有正文
    /// 的去处
    pub(crate) fn open(&self, o: Opening<'_>) -> (u64, Ending) {
        let id = self.bus.next_id();
        self.bus.emit(tw_api::Event::RequestStarted {
            id,
            client: self.client.clone(),
            client_hint: self.client_hint.clone(),
            session: o.session,
            peer: self.peer.clone(),
            key_masked: self.key_masked.clone(),
            route: o.choice.route.clone(),
            rule: o.choice.rule.clone(),
            group: o.choice.group.clone(),
            rewritten_by: o.choice.rewritten_by.clone(),
            provider: o.to.0.to_string(),
            billing: o.to.1,
            model: o.model.clone(),
            method: "WS".to_string(),
            path: self.path.clone(),
            input_estimate: o.input_estimate,
            session_log_bytes: None,
            at_ms: o.at_ms,
        });
        let ending = Ending::new(
            self.bus.clone(),
            id,
            o.model,
            o.started,
            o.at_ms as i64,
            None,
        );
        (id, ending)
    }
}

/// 一条 Responses 连接上每一轮都一样的：开始事件里的那几项、升级时路由的结论、连着的那一家。
pub(crate) struct Line {
    pub(crate) opener: Opener,
    /// 升级时路由的结论：走的路由、决定去向的规则、经过的组。参数改写每一轮按那一帧求，换掉
    /// 这里的 `rewritten_by`
    pub(crate) choice: Choice,
    /// 这条连接连着的那一家
    pub(crate) provider: String,
    pub(crate) billing: tw_config::Billing,
}

/// 在 `error` 那一帧收尾的几轮，最多记住多少个回答的 id（[`Turns::late`]）
const ENDED_MAX: usize = 8;

/// 这条连接上在跑的几轮，按发出去的先后。**上游按顺序回答**：上游来的帧都算头一轮的，头一轮
/// 回答完了，下一轮接上。
pub(crate) struct Turns {
    pub(crate) line: Arc<Line>,
    queue: VecDeque<Turn>,
    /// 最近在 `error` 那一帧收尾的几轮的回答 id，和那一轮是不是被切掉了（见 [`Self::late`]）
    ended: VecDeque<(String, bool)>,
}

impl Turns {
    pub(crate) fn new(line: Arc<Line>) -> Self {
        Self {
            line,
            queue: VecDeque::new(),
            ended: VecDeque::new(),
        }
    }

    /// 上游的这一帧（`type` 是 `kind`、带着回答 `response`）是不是**已经收了尾的那一轮补发
    /// 的**，是的话那一轮是不是被切掉了。
    ///
    /// 一轮在上游的 `error` 那一帧收尾，上游之后还可能为**同一次回答**补发一个
    /// `response.failed`。按「上游来的帧都算头一轮的」，那一帧会落到排在后面的那一轮头上：
    /// 那一轮的结局成了上一轮的失败，它自己的回答再落到下一轮头上，一路错下去。所以带着回答
    /// id 的帧先对一下 id。认得保守 —— 认错了的那一轮等不到收尾，比记错一行更糟：
    ///
    /// - 只记在 `error` 那一帧收尾的几轮：在完成、失败、没答完那一帧收尾的，上游不会再补；
    /// - 一次回答只补一个收尾帧，补过就忘掉它；
    /// - `response.created` 和头一轮自己的回答 id 一律算头一轮的：有的上游每次回答都用同一个 id
    pub(crate) fn late(&mut self, response: &str, kind: &str) -> Option<bool> {
        let own = self.queue.front().and_then(|t| t.response.as_deref()) == Some(response);
        if own || kind == "response.created" {
            return None;
        }
        let at = self.ended.iter().position(|(id, _)| id == response)?;
        let cut = self.ended[at].1;
        if matches!(
            kind,
            "response.completed" | "response.failed" | "response.incomplete"
        ) {
            self.ended.remove(at);
        }
        Some(cut)
    }

    /// 头一轮收尾了，下一轮接上。在 `error` 那一帧收尾的记下它的回答 id（见 [`Self::late`]）
    fn pop_front(&mut self) -> Option<Turn> {
        let t = self.queue.pop_front()?;
        if let (true, Some(id)) = (t.errored, t.response.clone()) {
            if self.ended.len() == ENDED_MAX {
                self.ended.pop_front();
            }
            self.ended.push_back((id, t.cut.is_some()));
        }
        if let Some(next) = self.queue.front_mut() {
            next.begin();
        }
        Some(t)
    }

    /// 上游此刻在回答的那一轮
    pub(crate) fn front(&mut self) -> Option<&mut Turn> {
        self.queue.front_mut()
    }

    pub(crate) fn front_id(&self) -> Option<u64> {
        self.queue.front().map(|t| t.id)
    }

    /// 发出去了：排在后面等上游回答。前面没有在答的，上游这就开始答它
    pub(crate) fn push(&mut self, mut t: Turn) {
        if self.queue.is_empty() {
            t.begin();
        }
        self.queue.push_back(t);
    }

    /// 头一轮回答完了：报结局，放掉它占着的
    pub(crate) fn finish_front(&mut self) {
        if let Some(t) = self.pop_front() {
            t.finish();
        }
    }

    /// 头一轮失败了：被防护切断了
    pub(crate) fn fail_front(&mut self, source: tw_api::FailureSource, why: Msg) {
        if let Some(t) = self.pop_front() {
            t.fail(source, why);
        }
    }

    /// 上游断了：在跑的几轮都没答完，一样失败
    pub(crate) fn fail_all(&mut self, source: tw_api::FailureSource, why: Msg) {
        while let Some(t) = self.queue.pop_front() {
            t.fail(source, why.clone());
        }
    }

    /// 回答钩子切掉了头一轮的回答（替它发过 `response.failed`）：上游收尾时记成拒绝
    pub(crate) fn cut_front(&mut self, why: Msg) {
        if let Some(t) = self.queue.front_mut() {
            t.cut = Some(why);
        }
    }

    /// 连接断了：没答完的几轮记成取消（结局的 Drop）
    pub(crate) fn clear(&mut self) {
        self.queue.clear();
    }
}

/// 还没报的路由事件：尝试链上那一跳。**上游这一轮的第一帧到的时候报**，那一跳的用时就是等
/// 它的那一段，和 HTTP 那条路等响应头一样；等不到的在结局之前报。
struct Route {
    choice: Choice,
    attempt: tw_api::AttemptView,
    billing: tw_api::Billing,
    /// 这一跳从什么时候算：发出去的那一刻
    since: Instant,
}

/// 在跑的一轮。**丢掉就是取消**（结局的 Drop），占着的跟着还回去。
pub(crate) struct Turn {
    pub(crate) id: u64,
    /// 报了就没有了
    ending: Option<Ending>,
    state: AppState,
    /// 这条连接连着的那一家：快慢和成败记在它头上
    provider: String,
    /// 上游这一次回答的 id（`response.created` 里的）。收尾之后靠它认出上游补发的帧（见
    /// [`Turns::late`]）
    response: Option<String>,
    /// 上游这一轮来过一个 `error`：在它那一帧收尾的，上游之后还可能为同一次回答补一个收尾帧
    errored: bool,
    /// 这一轮的成败给这一家记过了（见 [`Self::judge`]）。**一轮只记一次**，和 HTTP 那条路一跳
    /// 只记一次一样
    judged: bool,
    /// 这一帧到的那一刻：首字节时间从它算，和 HTTP 那条路从请求进来算一样
    started: Instant,
    /// 上游这一轮的第一帧到了没有。到了报响应头
    responded: bool,
    route: Option<Route>,
    /// 回答钩子切掉了这一次回答：结局记成拒绝，原因是这一句
    cut: Option<Msg>,
    /// 密钥的并发通行证和这一家的位置：**跟着这一轮走**，回答完了、连接断了就还
    _pass: crate::limits::Pass,
    _slot: crate::slots::Slot,
}

impl Turn {
    /// 这一帧发出去了，发给这一家的模型名是 `model`（和客户端要的不一样时才有：别名对过的、
    /// 规则或插件换过的）。尝试链上那一跳从这一刻算
    pub(crate) fn sent(&mut self, model: Option<String>) {
        if let Some(r) = self.route.as_mut() {
            r.attempt.model = model;
            r.since = Instant::now();
        }
    }

    /// 上游开始答这一轮了：发出去时前面没有在答的，或者前面那一轮刚收尾。这一家的快慢样本
    /// 从这一刻量到第一段内容（见 [`crate::latency`]）—— 前一轮还在答的时候，这一轮排在后面
    /// 等，那一段不是这一家慢
    fn begin(&mut self) {
        if let Some(e) = self.ending.as_mut() {
            e.timed(crate::ending::Lap {
                latency: self.state.latency.clone(),
                provider: self.provider.clone(),
                sent: Instant::now(),
            });
        }
    }

    /// 上游这一轮来了一帧，**上游原话**（带占位符的那一版），`type` 是 `kind`、带着的回答 id
    /// 是 `response`：头一帧报响应头，每一帧喂给结局认用量、第一个 token、上游报的错。成败
    /// 在这里记：第一段内容到了是答上了；内容之前就收了尾的，看收尾的这一帧报没报错
    pub(crate) fn upstream(&mut self, text: &str, kind: Option<&str>, response: Option<&str>) {
        if self.response.is_none() {
            self.response = response.map(str::to_string);
        }
        self.errored |= kind == Some("error");
        if !self.responded {
            self.responded = true;
            self.routed();
            self.state.bus.emit(tw_api::Event::RequestHeaders {
                id: self.id,
                status: 200,
                ttfb_ms: self.started.elapsed().as_millis() as u64,
            });
            if let Some(e) = self.ending.as_mut() {
                e.responded(200);
            }
        }
        if let Some(e) = self.ending.as_mut() {
            e.frame(text);
        }
        if self.judged {
            return;
        }
        if self.ending.as_ref().is_some_and(Ending::has_content) {
            // 内容到了：这一家答上了。之后流里再报错也不改 —— HTTP 那条路的一跳也是开头
            // 一过就记成功
            self.judge(|h, p| h.record_success(p));
        } else if super::ends_turn(kind) {
            // 一段内容都没有就收了尾：和 HTTP 那条路开头报错的一跳同一个判据
            let dialect = tw_dialect::ir::Dialect::Responses;
            match crate::server::stream_fault(&self.state, &self.provider, dialect, text) {
                Some(cause) => self.judge(|h, p| h.record_cause(p, cause)),
                None => self.judge(|h, p| h.record_success(p)),
            }
        }
    }

    /// 给这一家记这一轮的成败（见 [`crate::health`]）：熔断和按成败分的 `load-balance` 看的是
    /// 同一笔账，状态变了照常报。**一轮只记一次**
    fn judge(
        &mut self,
        record: impl FnOnce(&crate::health::Health, &str) -> Option<crate::health::State>,
    ) {
        if std::mem::replace(&mut self.judged, true) {
            return;
        }
        let health = &self.state.health;
        let change = record(health, &self.provider);
        crate::server::note_health(&self.state.bus, health, &self.provider, change);
    }

    fn routed(&mut self) {
        let Some(r) = self.route.take() else { return };
        let mut attempt = r.attempt;
        attempt.ms = r.since.elapsed().as_millis() as u64;
        self.state
            .bus
            .emit(routed(self.id, r.choice, vec![attempt], r.billing));
    }

    /// 这一次回答完了。上游在回答里报了错的（`response.failed`、`error`）是失败，原因是它说的
    /// 那句（见 [`Ending::streaming`]）
    fn finish(mut self) {
        self.routed();
        let Some(e) = self.ending.take() else { return };
        match self.cut.take() {
            Some(why) => e.failed(tw_api::FailureSource::Denied, why),
            None => e.finished(200),
        }
    }

    /// 失败了：上游断了、被防护切断了。用量照样带着（上游已经计了费）。上游断了的，没答上
    /// 之前断的给这一家记一次失败，和 HTTP 那条路流在第一段内容之前断了一样
    pub(crate) fn fail(mut self, source: tw_api::FailureSource, why: Msg) {
        if source == tw_api::FailureSource::Upstream {
            self.judge(|h, p| h.record_failure(p));
        }
        self.routed();
        if let Some(e) = self.ending.take() {
            e.failed(source, why);
        }
    }

    /// 准入过了、这一帧却没发出去：插件拒绝了它，或者写不过去。尝试链上是 `attempts`（写不
    /// 过去的那一跳；插件拒绝的没有），没接下的不按那一家记账。写不过去是上游断了，给这一家
    /// 记一次失败，和 HTTP 那条路发不出去一样
    pub(crate) fn unsent(
        mut self,
        attempts: Vec<tw_api::AttemptView>,
        source: tw_api::FailureSource,
        why: Msg,
    ) {
        if source == tw_api::FailureSource::Upstream {
            self.judge(|h, p| h.record_failure(p));
        }
        if let Some(r) = self.route.take() {
            self.state.bus.emit(routed(
                self.id,
                r.choice,
                attempts,
                tw_api::Billing::PerToken,
            ));
        }
        if let Some(e) = self.ending.take() {
            e.failed(source, why);
        }
    }
}

impl Drop for Turn {
    /// 没答完就被丢掉了（连接断了）：路由事件要在结局之前到 —— 存储层落库时手上没有它的话，
    /// 这一行没有尝试链。结局随后由它自己的 Drop 报成取消
    fn drop(&mut self) {
        self.routed();
    }
}

fn routed(
    id: u64,
    choice: Choice,
    attempts: Vec<tw_api::AttemptView>,
    billing: tw_api::Billing,
) -> tw_api::Event {
    tw_api::Event::RequestRouted {
        id,
        route: choice.route,
        rule: choice.rule,
        group: choice.group,
        rewritten_by: choice.rewritten_by,
        denied_by: None,
        affinity: None,
        attempts,
        billing,
    }
}

/// 一帧 `response.create` 要过准入时手上的。
pub(crate) struct Admit {
    pub(crate) state: AppState,
    pub(crate) line: Arc<Line>,
    /// 这一帧的参数改写附加的规则：阶段一按这一帧求的，加上阶段二的
    pub(crate) rewritten_by: Vec<String>,
    /// 客户端要的模型名
    pub(crate) requested: String,
    /// 发给这一家的（[`super::Naming`] 定的）。插件之后可能还会换
    pub(crate) sent: String,
    /// 这一帧的会话指纹（见 [`crate::session::fingerprint`]）
    pub(crate) fingerprint: Option<String>,
    /// 输入 token 的估算。解不开的帧没有
    pub(crate) input_estimate: Option<u64>,
    /// 内容过滤的结论：开始之后挂在这一轮的号上报
    pub(crate) screening: tw_guard::content::Screening,
    /// 这一帧到的那一刻
    pub(crate) arrived: Instant,
    pub(crate) at_ms: u64,
}

/// 这一轮没过准入。**行已经留下了**（开始、路由、结局），这里只说怎么告诉客户端。
pub(crate) enum NotAdmitted {
    /// 这一轮不发，替它回一个 `response.failed`，连接照常：上限拒了，这一家满着等不到
    Failed(GatewayError),
    /// 内容过滤拒了这一帧：切断连接，告诉客户端的是这句话
    Cut(Msg),
}

/// 一帧 `response.create` 的准入，**和 HTTP 那条路的一个请求同一套、同一个顺序**（见
/// `server::pipeline::admission` 和 `hop`）：
///
/// 1. 天、周、月的上限 —— 用满了就拒；
/// 2. 密钥的并发上限 —— 等前面的结束；
/// 3. 分钟、小时的上限 —— 下一个空位在这一轮的等待期限之前空出来就等，等不到就拒；过了
///    就按输入的估算占着，存储层记下这一行时换成实数；
/// 4. 开始：发开始事件，内容过滤的结论挂在这一轮上报（拒绝的切断连接）；
/// 5. 这一家的位置（`max_concurrent`）—— 满着就等，等到同一个期限；这条连接只连着
///    这一家，等不到就是这一家忙，回 429 那句话。
///
/// **等待期限一轮只有一个**：过了密钥的并发上限那一刻起算 `failover.slot_wait_secs`，第 3 步、
/// 第 5 步的等待都算在里面，和 HTTP 那条路一样（见 `server::pipeline::admission`）。
///
/// 被上限拒的、等不到位置的**照样留一行**，流量里看得见它为什么没发出去。拿到的通行证和
/// 位置交给这一轮（[`Turn`]），跟着它走。
pub(crate) async fn admit(a: Admit) -> Result<Turn, NotAdmitted> {
    let state = &a.state;
    let line = a.line.clone();
    let key_name = line.opener.client.as_str();
    let rt = state.runtime();
    let key = rt.config.clients.iter().find(|c| c.name == key_name);
    let max_concurrent = key.and_then(|c| c.max_concurrent);
    let limits: &[tw_config::KeyLimit] = key.map(|c| c.limits.as_slice()).unwrap_or_default();
    let choice = Choice {
        rewritten_by: a.rewritten_by.clone(),
        ..line.choice.clone()
    };
    if let Err(r) = state.key_limits.calendar(key_name, limits) {
        return Err(refused(&a, &choice, r.error()));
    }
    let pass = state.gate.acquire(key_name, max_concurrent).await;
    // 等待期限从这里起算：等滚动窗口的空位、等这一家的位置共用它（见
    // `crate::key_limits::slot_wait`）
    let wait_until = tokio::time::Instant::now() + crate::key_limits::slot_wait(&rt.config);
    let ask = ask(state, &a, limits);
    let hold = match state
        .key_limits
        .admit_by(key_name, limits, ask, wait_until)
        .await
    {
        Ok(hold) => hold,
        Err(r) => return Err(refused(&a, &choice, r.error())),
    };
    let (id, mut ending) = line.opener.open(Opening {
        choice: &choice,
        to: (&line.provider, line.billing.into()),
        model: a.requested.clone(),
        session: session(&a),
        input_estimate: a.input_estimate,
        started: a.arrived,
        at_ms: a.at_ms,
    });
    hold.bind(id);
    ending.streaming(tw_dialect::ir::Dialect::Responses, &line.provider);
    // 拒绝的也在开始之后：被拒是一次来源为 `denied` 的失败，一个字节都不发
    if let Some(why) = crate::guard::report(&state.bus, id, &line.provider, &a.screening) {
        ending.failed(tw_api::FailureSource::Denied, why.clone());
        return Err(NotAdmitted::Cut(why));
    }
    let model = Some(a.sent.clone()).filter(|m| !m.is_empty() && *m != a.requested);
    let hop_started = Instant::now();
    let (slot, queued_ms) = match state.slots.try_take(&line.provider) {
        Some(slot) => (slot, None),
        None => {
            let mut waited = None;
            let mut slot = None;
            // 这一轮能等的已经等完了（或者配置的是不等）：不再等
            if tokio::time::Instant::now() < wait_until {
                slot = state.slots.take_by(&line.provider, wait_until).await;
                waited = Some(hop_started.elapsed().as_millis() as u64);
            }
            match slot {
                Some(slot) => (slot, waited),
                None => {
                    let limit = state.slots.limit(&line.provider).unwrap_or_default();
                    let busy =
                        crate::server::hop_busy(&line.provider, model, limit, waited, hop_started);
                    state
                        .bus
                        .emit(routed(id, choice, vec![busy], tw_api::Billing::PerToken));
                    // 和 HTTP 那条路候选全满时同一句：能服务它的就这一家
                    let err = GatewayError::busy(msg!(
                        "gw.busy_all", upstreams = format!("`{}`", line.provider) =>
                        "Every upstream that can serve this request is at its concurrency limit \
                         (max_concurrent): {upstreams}. None had a free slot in time; try again shortly."
                    ));
                    ending.failed(err.source.into(), err.detail.clone());
                    return Err(NotAdmitted::Failed(err));
                }
            }
        }
    };
    let mut attempt = crate::server::hop(
        &line.provider,
        model,
        tw_api::AttemptOutcome::Served,
        200,
        hop_started,
    );
    attempt.queued_ms = queued_ms;
    Ok(Turn {
        id,
        ending: Some(ending),
        state: state.clone(),
        provider: line.provider.clone(),
        response: None,
        errored: false,
        judged: false,
        started: a.arrived,
        responded: false,
        route: Some(Route {
            choice,
            attempt,
            billing: line.billing.into(),
            since: Instant::now(),
        }),
        cut: None,
        _pass: pass,
        _slot: slot,
    })
}

/// 这一帧归到哪一次会话（见 [`crate::session::Sessions`]）。认不出来是 None
fn session(a: &Admit) -> Option<String> {
    a.fingerprint
        .as_deref()
        .map(|fp| a.state.sessions.assign(fp, a.at_ms))
}

/// 这一轮要占多少：输入 token 的估算，和设了费用上限时按这一家、发给它的名字算的输入费用。
/// 没有价格、不计费的是 0 —— 和结算时一样（见 `server::pipeline::admission`）
fn ask(state: &AppState, a: &Admit, limits: &[tw_config::KeyLimit]) -> crate::key_limits::Ask {
    let tokens = a.input_estimate.unwrap_or(0);
    let priced = limits
        .iter()
        .any(|l| l.measure() == tw_config::LimitMeasure::Cost);
    let cost_micros = if priced && tokens > 0 {
        let usage = tw_pricing::Usage {
            input: tokens,
            ..Default::default()
        };
        crate::quote::quote(
            &state.pricing.load(),
            &a.line.provider,
            &a.sent,
            &usage,
            a.line.billing,
        )
        .cost_micros
        .unwrap_or(0)
    } else {
        0
    };
    crate::key_limits::Ask {
        tokens,
        cost_micros,
    }
}

/// 被上限拒了：照样开始、留一行（尝试链是空的），交回替它回 `response.failed` 的那个错误。
fn refused(a: &Admit, choice: &Choice, why: GatewayError) -> NotAdmitted {
    tracing::info!(key = %a.line.opener.client, "a usage limit of the key refused a WebSocket request");
    let (id, ending) = a.line.opener.open(Opening {
        choice,
        to: ("", tw_api::Billing::PerToken),
        model: a.requested.clone(),
        session: session(a),
        input_estimate: a.input_estimate,
        started: a.arrived,
        at_ms: a.at_ms,
    });
    a.state
        .bus
        .emit(crate::server::routed_nowhere(id, choice.clone()));
    ending.failed(why.source.into(), why.detail.clone());
    NotAdmitted::Failed(why)
}

/// 规则拒绝了这一帧：**和 HTTP 那条路被规则拒绝的请求一样留一行**（开始、空尝试链的路由、
/// 一条 `denied` 的失败）。阶段二拒绝的，拒绝它的那条规则记在路由事件的 `denied_by` 上；阶段
/// 一拒绝的，它就是决定去向的那条
#[allow(clippy::too_many_arguments)]
pub(crate) fn denied(
    state: &AppState,
    line: &Line,
    by: super::Denied,
    why: &GatewayError,
    requested: String,
    fingerprint: Option<&str>,
    input_estimate: Option<u64>,
    started: Instant,
    at_ms: u64,
) {
    let mut choice = line.choice.clone();
    let denied_by = match by {
        super::Denied::PhaseOne(rule) => {
            choice.rule = rule;
            None
        }
        super::Denied::PhaseTwo(rule) => Some(rule),
    };
    let (id, ending) = line.opener.open(Opening {
        choice: &choice,
        to: ("", tw_api::Billing::PerToken),
        model: requested,
        session: fingerprint.map(|fp| state.sessions.assign(fp, at_ms)),
        input_estimate,
        started,
        at_ms,
    });
    let mut ev = crate::server::routed_nowhere(id, choice);
    if let tw_api::Event::RequestRouted { denied_by: d, .. } = &mut ev {
        *d = denied_by;
    }
    state.bus.emit(ev);
    ending.failed(why.source.into(), why.detail.clone());
}
