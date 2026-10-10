//! 一个请求的报文：在跑时的实时内容（控制面的 `GET /request/{id}/live`），和结束时另存的
//! 那几份（报文头、发给上游的请求体、交给客户端的回答）。
//!
//! # 记什么
//!
//! 一个请求的报文分几段，每段由「哪一边、哪个方向、第几跳」定下（[`tw_api::HeadView`]）：
//! 客户端发来的请求和它收到的回答（第 0 跳），每一跳发给上游的请求和上游的回答（尝试链上
//! 的第几跳，从 1 数）。每段一个报文头（请求行或状态行和头）、一份正文。
//!
//! # 不多攒
//!
//! **正文只攒落盘本来就要的那些**，没人在看时实时内容不多占一个字节：
//!
//! - 客户端的请求体是交去留档的那一份（`Bytes` 的引用计数，比 [`WINDOW`] 长的只拷一次
//!   开头，两边共用）
//! - 发给上游的请求体只在它要落盘时留（和客户端那一边不是一回事，见 [`Tap`]）。失败了的那一跳
//!   的，没人在看就在换下一家时放掉；它回的错误不落盘（原因在尝试链上），有人在看才留
//! - 回答那一跳上游的回答就是留档本来要攒的那一份（以前攒在 [`crate::ending::Ending`] 自己
//!   身上，见 [`Capture::answer`]）
//! - 交给客户端的回答只在它和上游的原话不是一回事时攒（转换格式、回答钩子）：那时它也要
//!   落盘。是一回事的时候，客户端那一边就是回答那一跳上游的那一段（`mirror`）。只差模型名
//!   （换回客户端用的名称）不算：为一个名字攒、存一整份回答不值
//! - 请求记录没起来（没有 [`BodySink`]）时什么都不攒，有人在看才攒
//!
//! 报文头一律留着：几百字节，结束时要落盘。
//!
//! # 打码和落盘的一样
//!
//! 正文用落盘时的那一套（[`Redaction::apply`]：同样的规则、同样的账本），在订阅的那一头一段
//! 一段地做，段落切在 SSE 事件的边界（空行）上 —— 认得出的值不会跨过一个空行，一段一段打
//! 和整份一起打是同一个结果。不是流的正文齐了一次打。报文头在记下的那一刻就打好码：实时
//! 发出去的和落盘的是同一份（[`BodyKind::Heads`]）。
//!
//! # 订阅的人慢，转发不等
//!
//! 转发那条路只往攒着的那一份上追加、拨一下版本号；订阅的那一头自己来读新的那一截
//! （[`Watch`]）。每个订阅者只记着读到了哪儿，不另攒一份，读得慢只是它自己晚一点看到。
//! 请求结束之后过了 [`LINGER`] 还没读完的，剩下的不再给。
//!
//! # 什么时候算结束
//!
//! 记录跟着请求走（[`Capture`]）：结局、交给客户端的响应体、每一跳都拿着一份，**最后一份
//! 被丢掉时**这个请求才算完 —— 那时结局已经报了，客户端那一边的最后一个字节也交出去了。
//! 这时把要另存的交去落盘，告诉订阅的人 `end`，从 [`Contents`] 里摘掉。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use tw_api::{HeadView, LiveBody, LiveContent, LiveEnd, LiveOutcome, WireDir, WireSide};

use crate::bodies::{BodyKind, BodyRecord, BodySink, Redaction, ResponseTap, WINDOW};

/// 请求结束之后，还没读完的订阅者最多再等多久。过了就把攒着的放掉：一个不读的订阅者
/// 不该让几 MB 的正文一直留在内存里
pub const LINGER: Duration = Duration::from_secs(60);

/// 一段正文到这么长，打码挪到阻塞线程上做
const HEAVY: usize = 256 * 1024;

/// 此刻在跑、记着报文的请求。**跨重载存活**（挂在 `AppState` 上）。
#[derive(Default)]
pub struct Contents {
    map: Mutex<HashMap<u64, Arc<Shared>>>,
}

impl Contents {
    fn lock(&self) -> MutexGuard<'_, HashMap<u64, Arc<Shared>>> {
        // 锁中毒了照样拿里面的表：看不了实时内容，好过一个不转发的网关
        self.map.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 订阅请求 `id` 的实时内容。不在跑的（从没有过、已经结束了、跑在 WebSocket 连接上的）
    /// 是 None
    pub fn watch(&self, id: u64) -> Option<Watch> {
        let shared = self.lock().get(&id).cloned()?;
        shared.lock().listeners += 1;
        let rx = shared.changed.subscribe();
        Some(Watch {
            shared,
            rx,
            heads: 0,
            cursors: Vec::new(),
            queue: VecDeque::new(),
            done: false,
        })
    }

    /// 此刻记着几个
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// 一个都没有
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 一个请求的报文，转发那一头和订阅的那一头共用。
struct Shared {
    state: Mutex<State>,
    /// 有新东西就拨一下。**发的那一头和这份记录活得一样久**：订阅的人等不到「关了」
    changed: tokio::sync::watch::Sender<u64>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn bump(&self) {
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }
}

struct State {
    /// 请求记录在跑：落盘要的正文要攒
    keep: bool,
    /// 按记下的先后。同一段发了两遍的两个都在（订阅的人要知道又发了一遍），落盘只留后一个
    heads: Vec<HeadView>,
    /// 按开始的先后，只往后加：订阅的人按位置记读到了哪儿
    parts: Vec<Part>,
    listeners: usize,
    /// 回答的那一跳，和它的回答在 `parts` 里的位置
    serving: Option<(u32, usize)>,
    /// 客户端收到的回答就是回答那一跳上游的原话（见 [`Capture::serving`]）
    mirror: bool,
    /// 回答那一跳的打码（拦截档下它的账可能比开头那本多）。客户端那一边的回答也按它
    redaction: Option<Redaction>,
    /// 交给客户端的回答在 `parts` 里的位置（不是 `mirror` 时）
    client: Option<usize>,
    /// 客户端收到的状态码
    status: Option<u16>,
    outcome: Option<LiveOutcome>,
    /// 结束了：之后不会再有什么
    ended: bool,
}

impl State {
    /// 正文攒不攒：落盘要，或者有人在看
    fn keeps(&self) -> bool {
        self.keep || self.listeners > 0
    }

    /// 没人在看的话，放掉除了第 `but` 跳以外发给上游的请求体：它们失败了，不落盘
    fn release_requests(&mut self, but: u32) {
        if self.listeners > 0 {
            return;
        }
        for p in &mut self.parts {
            if p.side == WireSide::Upstream && p.dir == WireDir::Request && p.attempt != but {
                p.body = Body::None;
            }
        }
    }

    /// 往第 `at` 段正文后面接一块。`keeps` 说攒不攒
    fn append(&mut self, at: usize, chunk: &[u8], keeps: bool) {
        let p = &mut self.parts[at];
        p.total += chunk.len();
        if !keeps {
            return;
        }
        if matches!(p.body, Body::None) {
            p.body = Body::Growing(ResponseTap::new());
        }
        if let Body::Growing(t) = &mut p.body {
            t.feed(chunk);
        }
    }
}

/// 一段报文的正文。
struct Part {
    side: WireSide,
    dir: WireDir,
    attempt: u32,
    /// 这一段按什么打码
    redaction: Redaction,
    body: Body,
    /// 原本多长，没攒下的也算
    total: usize,
    /// 齐了，不会再长
    done: bool,
    /// 发给上游的请求体和客户端那一边（插件改过的话是改过的那一份）不一样。只有发给上游的
    /// 请求用它：一样的不另存
    differs: bool,
}

impl Part {
    fn new(side: WireSide, dir: WireDir, attempt: u32, redaction: Redaction) -> Self {
        Self {
            side,
            dir,
            attempt,
            redaction,
            body: Body::None,
            total: 0,
            done: false,
            differs: false,
        }
    }
}

enum Body {
    /// 没攒（没人要，或者放掉了）
    None,
    /// 一次到齐的（请求体），或者冻好了的
    Whole(Bytes),
    /// 还在长的（回答）。攒到 [`WINDOW`] 为止
    Growing(ResponseTap),
}

impl Body {
    fn bytes(&self) -> &[u8] {
        match self {
            Body::None => &[],
            Body::Whole(b) => b,
            Body::Growing(t) => t.kept(),
        }
    }

    /// 攒满了：之后来的不再留
    fn full(&self) -> bool {
        matches!(self, Body::Growing(t) if t.full())
    }

    /// 攒下的不是全部（原本 `total` 那么长）
    fn cut(&self, total: usize) -> bool {
        match self {
            Body::None => false,
            Body::Whole(b) => total > b.len(),
            Body::Growing(t) => t.cut() || t.full(),
        }
    }

    /// 冻成一份 `Bytes` 交出来（不拷贝），自己留着同一份给还没读完的订阅者。没攒的是 None
    fn freeze(&mut self) -> Option<Bytes> {
        let b = match std::mem::replace(self, Body::None) {
            Body::None => return None,
            Body::Whole(b) => b,
            Body::Growing(t) => t.finish().0,
        };
        *self = Body::Whole(b.clone());
        Some(b)
    }
}

/// 一份一次到齐的正文留多少：比 [`WINDOW`] 长的只**拷**开头 —— 切片会拽着整个请求体（最大
/// 256 MB）一直留到回答交完
pub(crate) fn prefix(body: &Bytes) -> Bytes {
    if body.len() > WINDOW {
        Bytes::copy_from_slice(&body[..WINDOW])
    } else {
        body.clone()
    }
}

/// 一个请求的报文记录。**跟着请求走**：开始时建（`server::pipeline` 发出开始事件时），结局、
/// 交给客户端的响应体、每一跳各拿一份，最后一份被丢掉时这个请求才算完（见模块说明）。
#[derive(Clone)]
pub struct Capture(Arc<Owner>);

struct Owner {
    id: u64,
    /// 请求开始的时刻：另存的正文按它归档，和请求体那一份对得上
    at_ms: i64,
    shared: Arc<Shared>,
    contents: Arc<Contents>,
    sink: Option<BodySink>,
    /// 这个请求开始时的打码：报文头和客户端的请求体用它
    redaction: Redaction,
}

impl Capture {
    /// 请求 `id` 开始了：登记上，订阅得到它了。`sink` 是落盘的去处（没有就是请求记录没起来），
    /// `redaction` 是这个请求开始时定下的打码
    pub fn open(
        contents: &Arc<Contents>,
        id: u64,
        at_ms: i64,
        sink: Option<BodySink>,
        redaction: Redaction,
    ) -> Self {
        let (changed, _) = tokio::sync::watch::channel(0);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                keep: sink.is_some(),
                heads: Vec::new(),
                parts: Vec::new(),
                listeners: 0,
                serving: None,
                mirror: false,
                redaction: None,
                client: None,
                status: None,
                outcome: None,
                ended: false,
            }),
            changed,
        });
        contents.lock().insert(id, shared.clone());
        Capture(Arc::new(Owner {
            id,
            at_ms,
            shared,
            contents: contents.clone(),
            sink,
            redaction,
        }))
    }

    /// 改一下，有人在看就叫醒他
    fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        let (out, notify) = {
            let mut st = self.0.shared.lock();
            let out = f(&mut st);
            (out, st.listeners > 0 || st.ended)
        };
        if notify {
            self.0.shared.bump();
        }
        out
    }

    /// 客户端发来的请求：请求行、头、请求体（内容过滤删过字的是删过的那一份，和存下来的
    /// 一样）。`body` 是留档的那一份（比 [`WINDOW`] 长的已经只剩开头，见 [`prefix`]），`total`
    /// 是原本多长
    pub fn client_request(
        &self,
        version: http::Version,
        uri: &http::Uri,
        headers: &http::HeaderMap,
        body: &Bytes,
        total: usize,
    ) {
        let r = &self.0.redaction;
        let target = uri.path_and_query().map_or(uri.path(), |pq| pq.as_str());
        let head = HeadView {
            side: WireSide::Client,
            dir: WireDir::Request,
            attempt: 0,
            line: format!("POST {} {}", masked_target(target, r), version_str(version)),
            headers: masked_headers(headers, &[], r),
        };
        self.with(|st| {
            st.heads.push(head);
            let mut p = Part::new(WireSide::Client, WireDir::Request, 0, r.clone());
            if st.keeps() {
                p.body = Body::Whole(body.clone());
            }
            p.total = total;
            p.done = true;
            st.parts.push(p);
        });
    }

    /// 第 `attempt` 跳发出去了（一跳可能发好几遍：换了 token、去掉上游拒绝的部分，见 [`Tap`]）
    fn sending(
        &self,
        attempt: u32,
        head: HeadView,
        body: &Bytes,
        redaction: &Redaction,
        differs: bool,
    ) {
        self.with(|st| {
            // 前面那几跳失败了（这一跳又发一遍的话，前一遍也不算了）：没人在看就放掉
            let listening = st.listeners > 0;
            for p in st.parts.iter_mut().filter(|p| {
                p.side == WireSide::Upstream && p.dir == WireDir::Request && p.attempt == attempt
            }) {
                if !listening {
                    p.body = Body::None;
                }
            }
            st.release_requests(attempt);
            st.heads.push(head);
            let mut p = Part::new(
                WireSide::Upstream,
                WireDir::Request,
                attempt,
                redaction.clone(),
            );
            // 和客户端那一边是一回事的不落盘，也就不留：有人在看才留
            if (st.keep && differs) || st.listeners > 0 {
                p.body = Body::Whole(prefix(body));
            }
            p.total = body.len();
            p.done = true;
            p.differs = differs;
            st.parts.push(p);
        });
    }

    /// 第 `attempt` 跳上游的错误正文（这一跳失败了、换了下一家）：读来认原因的那一截。它不
    /// 落盘（原因在尝试链上），有人在看才留
    fn said(&self, attempt: u32, body: &[u8], redaction: &Redaction) {
        self.with(|st| {
            let mut p = Part::new(
                WireSide::Upstream,
                WireDir::Response,
                attempt,
                redaction.clone(),
            );
            if st.listeners > 0 {
                p.body = Body::Whole(Bytes::copy_from_slice(&body[..body.len().min(WINDOW)]));
            }
            p.total = body.len();
            p.done = true;
            st.parts.push(p);
        });
    }

    /// 第 `attempt` 跳接下了这个请求，回答从这里起交给 [`Capture::answer`]。`redaction` 是这一跳
    /// 的打码；`mirror` 说客户端收到的就是上游的原话（同格式、没有回答钩子）—— 那时客户端
    /// 那一边不另攒
    pub fn serving(&self, attempt: u32, redaction: Redaction, mirror: bool) {
        self.with(|st| {
            st.release_requests(attempt);
            st.parts.push(Part::new(
                WireSide::Upstream,
                WireDir::Response,
                attempt,
                redaction.clone(),
            ));
            st.serving = Some((attempt, st.parts.len() - 1));
            st.mirror = mirror;
            st.redaction = Some(redaction);
        });
    }

    /// 回答那一跳上游来了一块（上游原话：占位符还没换回、格式还没转，Bedrock 的二进制帧已经
    /// 转成 SSE）。**落盘要攒的就是这一份**（`.res`）。还没有哪一跳接下（网关自己估的数）时
    /// 什么都不做，交回 false
    pub fn answer(&self, chunk: &[u8]) -> bool {
        let notify = {
            let mut st = self.0.shared.lock();
            let Some((_, at)) = st.serving else {
                return false;
            };
            let keeps = st.keeps();
            st.append(at, chunk, keeps);
            st.listeners > 0
        };
        if notify {
            self.0.shared.bump();
        }
        true
    }

    /// 回答那一跳的回答齐了：交出攒下的那一份和原本的长度，落盘用。还没有哪一跳接下的是
    /// None
    pub fn freeze_answer(&self) -> Option<(Bytes, usize)> {
        self.with(|st| {
            let (_, at) = st.serving?;
            let p = &mut st.parts[at];
            p.done = true;
            Some((p.body.freeze().unwrap_or_default(), p.total))
        })
    }

    /// 结局报了（见 [`crate::ending::Ending`]）。只认第一次
    pub fn settled(&self, outcome: LiveOutcome) {
        self.with(|st| {
            st.outcome.get_or_insert(outcome);
            if let Some((_, at)) = st.serving {
                st.parts[at].done = true;
            }
        });
    }

    /// 交给客户端的响应：记下状态行和响应头，响应体包一层，一边交一边记。`version` 是客户端
    /// 请求用的协议版本
    pub fn client_answer(
        self,
        version: http::Version,
        resp: axum::response::Response,
    ) -> axum::response::Response {
        let r = self.with(|st| {
            st.status = Some(resp.status().as_u16());
            st.redaction.clone()
        });
        let r = r.unwrap_or_else(|| self.0.redaction.clone());
        let head = HeadView {
            side: WireSide::Client,
            dir: WireDir::Response,
            attempt: 0,
            line: status_line(version, resp.status()),
            headers: masked_headers(resp.headers(), &[], &r),
        };
        self.with(|st| st.heads.push(head));
        let (parts, body) = resp.into_parts();
        let stream = async_stream::stream! {
            // **先声明的后丢**：响应体（连着回答和它的结局）先被丢掉，这一份记录最后 ——
            // 丢掉它的那一刻结局一定已经报了
            let capture = self;
            let mut body = body.into_data_stream();
            while let Some(chunk) = body.next().await {
                if let Ok(b) = &chunk {
                    capture.client_chunk(b);
                }
                yield chunk;
            }
        };
        axum::response::Response::from_parts(parts, axum::body::Body::from_stream(stream))
    }

    /// 交给客户端的一块
    fn client_chunk(&self, chunk: &[u8]) {
        if heartbeat(chunk) {
            return;
        }
        let notify = {
            let mut st = self.0.shared.lock();
            if st.mirror {
                return;
            }
            let at = match st.client {
                Some(at) => at,
                None => {
                    let r = st
                        .redaction
                        .clone()
                        .unwrap_or_else(|| self.0.redaction.clone());
                    st.parts
                        .push(Part::new(WireSide::Client, WireDir::Response, 0, r));
                    st.client = Some(st.parts.len() - 1);
                    st.parts.len() - 1
                }
            };
            // 落盘只要有上游回答的那种：网关自己说的（出错、估的数）不另存。有人在看照样攒
            let keeps = (st.keep && st.serving.is_some()) || st.listeners > 0;
            st.append(at, chunk, keeps);
            st.listeners > 0
        };
        if notify {
            self.0.shared.bump();
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.contents.lock().remove(&self.id);
        let (records, listening) = {
            let mut st = self.shared.lock();
            st.ended = true;
            for p in &mut st.parts {
                p.done = true;
            }
            let records = if self.sink.is_some() {
                self.records(&mut st)
            } else {
                Vec::new()
            };
            (records, st.listeners > 0)
        };
        // **这里什么都不能 panic**（可能正跑在一次 unwind 里）：往通道里 try_send、拨版本号
        for r in records {
            crate::bodies::offer(&self.sink, r);
        }
        self.shared.bump();
        // 还有人没读完：过一阵把攒着的放掉。拿弱引用 —— 读完的人走了，记录就跟着没了
        if listening && let Ok(rt) = tokio::runtime::Handle::try_current() {
            let weak = Arc::downgrade(&self.shared);
            rt.spawn(async move {
                tokio::time::sleep(LINGER).await;
                if let Some(shared) = weak.upgrade() {
                    for p in &mut shared.lock().parts {
                        p.body = Body::None;
                    }
                    shared.bump();
                }
            });
        }
    }
}

impl Owner {
    /// 结束时另存的那几份：报文头、回答那一跳发出去的请求体（和客户端那一边不一样时）、
    /// 交给客户端的回答（和上游的原话不一样时）。上游的原话（`.res`）由结局交（见
    /// [`crate::ending::Ending`]）
    fn records(&self, st: &mut State) -> Vec<BodyRecord> {
        let mut out = Vec::new();
        let heads = latest_heads(&st.heads);
        if !heads.is_empty()
            && let Ok(json) = serde_json::to_vec(&heads)
        {
            let len = json.len();
            out.push(BodyRecord::new(
                self.id,
                self.at_ms,
                BodyKind::Heads,
                Bytes::from(json),
                len,
                self.redaction.clone(),
            ));
        }
        let Some((attempt, _)) = st.serving else {
            return out;
        };
        // 回答那一跳最后一遍发出去的那一份
        if let Some(p) = st.parts.iter_mut().rev().find(|p| {
            p.side == WireSide::Upstream && p.dir == WireDir::Request && p.attempt == attempt
        }) && p.differs
            && let Some(b) = p.body.freeze().filter(|b| !b.is_empty())
        {
            out.push(BodyRecord::new(
                self.id,
                self.at_ms,
                BodyKind::UpstreamRequest,
                b,
                p.total,
                p.redaction.clone(),
            ));
        }
        if !st.mirror
            && let Some(at) = st.client
        {
            let p = &mut st.parts[at];
            if let Some(b) = p.body.freeze().filter(|b| !b.is_empty()) {
                out.push(BodyRecord::new(
                    self.id,
                    self.at_ms,
                    BodyKind::ClientResponse,
                    b,
                    p.total,
                    p.redaction.clone(),
                ));
            }
        }
        out
    }
}

/// 落盘的报文头：同一段只留最后一遍，放在第一遍的位置上
fn latest_heads(heads: &[HeadView]) -> Vec<HeadView> {
    let mut out: Vec<HeadView> = Vec::with_capacity(heads.len());
    for h in heads {
        match out
            .iter_mut()
            .find(|o| o.side == h.side && o.dir == h.dir && o.attempt == h.attempt)
        {
            Some(o) => *o = h.clone(),
            None => out.push(h.clone()),
        }
    }
    out
}

/// 一个请求的报文记录放在哪儿：`server::passthrough` 留一份、跟着请求进管线一份，管线发出
/// 开始事件时放进去（见 `server::pipeline::open`）。克隆出来的是同一个。
///
/// **拿着它的都会放手**：交给客户端的响应包好了，管线也跑完了，它就跟着没了 —— 它不该让
/// 一个已经结束的请求一直算在跑。
#[derive(Clone, Default)]
pub struct Seat(Arc<std::sync::OnceLock<Capture>>);

impl Seat {
    /// 放进去。只放一次，再放什么都不变
    pub fn put(&self, c: Capture) {
        let _ = self.0.set(c);
    }

    /// 放进去了的话，一份
    pub fn get(&self) -> Option<Capture> {
        self.0.get().cloned()
    }
}

/// 一跳在报文记录里记到哪儿（见 `server::pipeline::hop`）。
#[derive(Clone)]
pub struct Tap {
    capture: Option<Capture>,
    /// 尝试链上的第几跳，从 1 数
    attempt: u32,
    /// 这一跳的打码：它的账（拦截档下）
    redaction: Redaction,
    /// 发出去的和客户端那一边不一样
    differs: bool,
}

impl Tap {
    pub fn new(
        capture: Option<Capture>,
        attempt: u32,
        redaction: Redaction,
        differs: bool,
    ) -> Self {
        Self {
            capture,
            attempt,
            redaction,
            differs,
        }
    }

    /// 去掉了上游拒绝的部分（自动标的缓存断点、别家封存的推理）再发的那一遍：发出去的不再
    /// 是客户端那一份
    pub fn resent(&self) -> Self {
        Self {
            differs: true,
            ..self.clone()
        }
    }

    /// 这一跳发出去了：`request` 是定稿的那一个（签过名的），`body` 是压缩之前的请求体，
    /// `configured` 是上游配置里写的头（凭据和用户加的）—— 公开的那几个之外一律打码
    pub fn sending(
        &self,
        request: &reqwest::Request,
        body: &Bytes,
        configured: &[(String, String)],
    ) {
        let Some(c) = &self.capture else { return };
        let head = HeadView {
            side: WireSide::Upstream,
            dir: WireDir::Request,
            attempt: self.attempt,
            line: format!(
                "{} {}",
                request.method(),
                masked_url(request.url().as_str(), &self.redaction)
            ),
            headers: masked_headers(request.headers(), configured, &self.redaction),
        };
        c.sending(self.attempt, head, body, &self.redaction, self.differs);
    }

    /// 上游的响应头到了
    pub fn answered(&self, r: &reqwest::Response) {
        let Some(c) = &self.capture else { return };
        let head = HeadView {
            side: WireSide::Upstream,
            dir: WireDir::Response,
            attempt: self.attempt,
            line: status_line(r.version(), r.status()),
            headers: masked_headers(r.headers(), &[], &self.redaction),
        };
        c.with(|st| st.heads.push(head));
    }

    /// 这一跳失败了、换下一家：上游的错误正文（读来认原因的那一截）
    pub fn said(&self, body: &[u8]) {
        if let Some(c) = &self.capture {
            c.said(self.attempt, body, &self.redaction);
        }
    }
}

/// 网关自己插的心跳（等回答时的保活注释、Anthropic 流的 `ping`）：不是回答，不记
fn heartbeat(chunk: &[u8]) -> bool {
    chunk == crate::server::KEEPALIVE || chunk == crate::server::PING
}

/// 一个订阅者：先补发到目前为止有的，再接着发新来的，`end` 之后没有了。
///
/// **它自己来读**：转发那一头不等它，也不替它另攒（见模块说明）。
pub struct Watch {
    shared: Arc<Shared>,
    rx: tokio::sync::watch::Receiver<u64>,
    /// 发到了第几个报文头
    heads: usize,
    /// 每段正文读到了哪儿，和 `State::parts` 一一对应
    cursors: Vec<Cursor>,
    queue: VecDeque<LiveContent>,
    /// `end` 发了
    done: bool,
}

#[derive(Debug, Default, Clone, Copy)]
struct Cursor {
    /// 读到的位置（攒着的那一份里的字节）
    raw: usize,
    /// 从 `raw` 往后找过事件边界、没找到的，到这儿为止
    scanned: usize,
    /// 发出去的打过码的字节数。到 [`tw_api::BODY_MAX`] 为止
    text: usize,
    /// 这一段不会再有了
    closed: bool,
}

/// 一轮读到的新东西。
struct Round {
    heads: Vec<HeadView>,
    segments: Vec<Segment>,
    end: Option<LiveEnd>,
}

/// 一段还没打码的正文。
struct Segment {
    at: usize,
    side: WireSide,
    dir: WireDir,
    attempt: u32,
    raw: Bytes,
    redaction: Redaction,
    /// 攒下的不是全部：发完这一段就到头了
    cut: bool,
    /// 客户端那一边就是这一段（见 [`Capture::serving`]）：照样再发一份给客户端那一边
    mirror: bool,
}

impl Watch {
    /// 下一条。`end` 之后是 None
    pub async fn next(&mut self) -> Option<LiveContent> {
        loop {
            if let Some(ev) = self.queue.pop_front() {
                return Some(ev);
            }
            if self.done {
                return None;
            }
            // 先认下现在的版本再读：读完之后才拨的那一下，下面的 `changed` 等得到
            self.rx.borrow_and_update();
            let round = self.collect();
            let any = !round.heads.is_empty() || !round.segments.is_empty() || round.end.is_some();
            self.render(round).await;
            if !any {
                let _ = self.rx.changed().await;
            }
        }
    }

    /// 拿着锁读出新来的那些：只拷字节（整份到齐的连拷都不拷），打码在锁外面做
    fn collect(&mut self) -> Round {
        let st = self.shared.lock();
        let heads = st.heads[self.heads.min(st.heads.len())..].to_vec();
        self.heads = st.heads.len();
        if self.cursors.len() < st.parts.len() {
            self.cursors.resize(st.parts.len(), Cursor::default());
        }
        let mut segments = Vec::new();
        for (at, p) in st.parts.iter().enumerate() {
            let c = &mut self.cursors[at];
            if c.closed {
                continue;
            }
            let data = p.body.bytes();
            let from = c.raw.min(data.len());
            // 齐了的、攒满了的、请求结束了的：剩下的全给
            let last = p.done || p.body.full() || st.ended;
            let cut = if last {
                data.len()
            } else {
                let look = c.scanned.max(from).saturating_sub(2).max(from);
                match boundary(&data[look..]) {
                    Some(n) => look + n,
                    None => from,
                }
            };
            c.scanned = data.len();
            if cut == from && !last {
                continue;
            }
            let raw = match &p.body {
                Body::Whole(b) => b.slice(from..cut),
                _ => Bytes::copy_from_slice(&data[from..cut]),
            };
            segments.push(Segment {
                at,
                side: p.side,
                dir: p.dir,
                attempt: p.attempt,
                raw,
                redaction: p.redaction.clone(),
                cut: last && p.body.cut(p.total),
                mirror: st.mirror && st.serving.is_some_and(|(_, s)| s == at),
            });
            c.raw = cut;
            c.closed = last;
        }
        let end = st.ended.then(|| LiveEnd {
            status: st.status,
            outcome: st.outcome.unwrap_or(LiveOutcome::Failed),
        });
        Round {
            heads,
            segments,
            end,
        }
    }

    /// 打码，排进要发的队里
    async fn render(&mut self, round: Round) {
        for h in round.heads {
            self.queue.push_back(LiveContent::Head(h));
        }
        for s in round.segments {
            let text = redact(s.raw, s.redaction).await;
            let c = &mut self.cursors[s.at];
            let room = tw_api::BODY_MAX.saturating_sub(c.text);
            let capped = text.len() > room;
            let text = if capped {
                text[..floor_char_boundary(&text, room)].to_string()
            } else {
                text
            };
            c.text += text.len();
            if capped {
                c.closed = true;
            }
            let truncated = capped || s.cut;
            if text.is_empty() && !truncated {
                continue;
            }
            let body = LiveBody {
                side: s.side,
                dir: s.dir,
                attempt: s.attempt,
                text,
                truncated,
            };
            if s.mirror {
                let client = LiveBody {
                    side: WireSide::Client,
                    dir: WireDir::Response,
                    attempt: 0,
                    ..body.clone()
                };
                self.queue.push_back(LiveContent::Body(body));
                self.queue.push_back(LiveContent::Body(client));
            } else {
                self.queue.push_back(LiveContent::Body(body));
            }
        }
        if let Some(end) = round.end {
            self.queue.push_back(LiveContent::End(end));
            self.done = true;
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let mut st = self.shared.lock();
        st.listeners = st.listeners.saturating_sub(1);
    }
}

/// 一段正文打码：和落盘时同一个函数。大的挪到阻塞线程上
async fn redact(raw: Bytes, r: Redaction) -> String {
    let heavy = raw.len() >= HEAVY;
    let go = move || r.apply(&String::from_utf8_lossy(&raw));
    if heavy {
        tokio::task::spawn_blocking(go).await.unwrap_or_default()
    } else {
        go()
    }
}

/// `data` 里最后一个事件边界（一个空行）之后的位置：到那儿为止都是整件的事件。没有是 None
fn boundary(data: &[u8]) -> Option<usize> {
    memchr::memrchr_iter(b'\n', data)
        .find(|&i| data[..=i].ends_with(b"\n\n") || data[..=i].ends_with(b"\n\r\n"))
        .map(|i| i + 1)
}

/// 不超过 `at` 的最后一个字符边界
fn floor_char_boundary(s: &str, at: usize) -> usize {
    let mut i = at.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// 协议版本在请求行、状态行里的写法
fn version_str(v: http::Version) -> &'static str {
    match v {
        http::Version::HTTP_09 => "HTTP/0.9",
        http::Version::HTTP_10 => "HTTP/1.0",
        http::Version::HTTP_2 => "HTTP/2",
        http::Version::HTTP_3 => "HTTP/3",
        _ => "HTTP/1.1",
    }
}

/// 状态行：`HTTP/1.1 200 OK`。HTTP/2 起没有原因短语：`HTTP/2 200`
fn status_line(v: http::Version, status: http::StatusCode) -> String {
    match status.canonical_reason() {
        Some(reason) if v < http::Version::HTTP_2 => {
            format!("{} {} {reason}", version_str(v), status.as_u16())
        }
        _ => format!("{} {}", version_str(v), status.as_u16()),
    }
}

/// 查询串里放凭据的参数（Gemini 的 `key=`、预签名地址的签名……）
fn secret_param(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "key"
            | "api_key"
            | "apikey"
            | "api-key"
            | "access_token"
            | "token"
            | "auth"
            | "password"
            | "secret"
            | "sig"
            | "signature"
            | "x-goog-api-key"
            | "x-amz-security-token"
            | "x-amz-signature"
            | "x-amz-credential"
    )
}

/// 请求行里的路径和查询串：放凭据的参数打码，再和正文同一套脱敏
fn masked_target(target: &str, r: &Redaction) -> String {
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (target, None),
    };
    let mut out = path.to_string();
    if let Some(q) = query {
        out.push('?');
        for (i, pair) in q.split('&').enumerate() {
            if i > 0 {
                out.push('&');
            }
            match pair.split_once('=') {
                Some((k, v)) if secret_param(k) => {
                    out.push_str(k);
                    out.push('=');
                    out.push_str(&tw_secret::mask_secret(v));
                }
                _ => out.push_str(pair),
            }
        }
    }
    r.apply(&out)
}

/// 发给上游的完整地址：地址里的账号口令（`user:pass@`）去掉，路径和查询串同 [`masked_target`]
fn masked_url(url: &str, r: &Redaction) -> String {
    let origin = tw_secret::redact_url(url);
    let rest = url
        .find("://")
        .map(|i| &url[i + 3..])
        .and_then(|after| after.find(['/', '?', '#']).map(|j| &after[j..]))
        .unwrap_or("");
    let rest = rest.split('#').next().unwrap_or("");
    format!("{origin}{}", masked_target(rest, r))
}

/// 这个头放的是凭据吗。**公开的那几个**（[`tw_secret::is_public_header`]）不是；名字里带着
/// `auth`、`key`、`token`、`secret`、`password`、`signature`、`credential` 的，和 `cookie`、
/// ChatGPT 的账号 ID，是
fn credential(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if tw_secret::is_public_header(&n) {
        return false;
    }
    matches!(n.as_str(), "cookie" | "set-cookie" | "chatgpt-account-id")
        || [
            "auth",
            "key",
            "token",
            "secret",
            "password",
            "signature",
            "credential",
        ]
        .iter()
        .any(|w| n.contains(w))
}

/// 一个凭据怎么打码：和 `key_masked` 一样留头尾（[`tw_secret::mask_secret`]）。带着方案名的
/// （`Bearer …`、`Basic …`、`AWS4-HMAC-SHA256 …`）方案名留着
fn mask_credential(value: &str) -> String {
    match value.split_once(' ') {
        Some((scheme, rest))
            if !scheme.is_empty()
                && scheme.len() <= 24
                && scheme
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !rest.trim().is_empty() =>
        {
            format!("{scheme} {}", tw_secret::mask_secret(rest.trim()))
        }
        _ => tw_secret::mask_secret(value),
    }
}

/// 一组头打码：凭据（[`credential`]）和 `configured` 里点了名的（上游配置里写的，公开的
/// 那几个除外）留头尾，别的值和正文同一套脱敏
fn masked_headers(
    map: &http::HeaderMap,
    configured: &[(String, String)],
    r: &Redaction,
) -> Vec<(String, String)> {
    map.iter()
        .map(|(name, value)| {
            let n = name.as_str();
            let v = String::from_utf8_lossy(value.as_bytes());
            let named = configured.iter().any(|(k, _)| k.eq_ignore_ascii_case(n))
                && !tw_secret::is_public_header(n);
            let v = if named || credential(n) {
                mask_credential(&v)
            } else {
                v.into_owned()
            };
            (n.to_string(), r.apply(&v))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn contents() -> Arc<Contents> {
        Arc::new(Contents::default())
    }

    fn open(c: &Arc<Contents>, sink: Option<BodySink>) -> Capture {
        Capture::open(c, 7, 1_000, sink, Redaction::default())
    }

    async fn drain(w: &mut Watch) -> Vec<LiveContent> {
        let mut out = Vec::new();
        while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(50), w.next()).await {
            out.push(ev);
        }
        out
    }

    fn bodies(evs: &[LiveContent], side: WireSide, dir: WireDir, attempt: u32) -> String {
        evs.iter()
            .filter_map(|e| match e {
                LiveContent::Body(b) if b.side == side && b.dir == dir && b.attempt == attempt => {
                    Some(b.text.as_str())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_boundary_is_the_end_of_the_last_whole_event() {
        assert_eq!(boundary(b"data: a\n\ndata: b"), Some(9));
        assert_eq!(boundary(b"data: a\r\n\r\ndata: b\r\n\r\n"), Some(22));
        assert_eq!(boundary(b"data: a\ndata: b"), None);
        assert_eq!(boundary(b""), None);
    }

    #[test]
    fn status_lines_carry_a_reason_only_before_http2() {
        let ok = http::StatusCode::OK;
        assert_eq!(status_line(http::Version::HTTP_11, ok), "HTTP/1.1 200 OK");
        assert_eq!(status_line(http::Version::HTTP_2, ok), "HTTP/2 200");
        assert_eq!(
            status_line(
                http::Version::HTTP_11,
                http::StatusCode::from_u16(529).unwrap()
            ),
            "HTTP/1.1 529"
        );
    }

    /// 凭据只留头尾，方案名留着；配置里点了名的头也一样；公开的照原样
    #[test]
    fn credentials_in_headers_are_masked_like_the_key_is() {
        let mut h = http::HeaderMap::new();
        h.insert("authorization", format!("Bearer {KEY}").parse().unwrap());
        h.insert("x-api-key", KEY.parse().unwrap());
        h.insert(
            "x-goog-api-key",
            "AIzaSyA0123456789abcdefghijklmnopqrstu".parse().unwrap(),
        );
        h.insert(
            "cookie",
            "session=abcdef0123456789; theme=dark".parse().unwrap(),
        );
        h.insert("x-relay-pass", "hunter2hunter2".parse().unwrap());
        h.insert("content-type", "application/json".parse().unwrap());
        h.insert("anthropic-version", "2023-06-01".parse().unwrap());
        let configured = vec![("X-Relay-Pass".to_string(), "hunter2hunter2".to_string())];
        let got: HashMap<String, String> = masked_headers(&h, &configured, &Redaction::default())
            .into_iter()
            .collect();
        assert_eq!(
            got["authorization"],
            format!("Bearer {}", tw_secret::mask_secret(KEY))
        );
        assert_eq!(got["x-api-key"], tw_secret::mask_secret(KEY));
        assert!(!got["x-goog-api-key"].contains("0123456789abcdef"));
        assert!(
            !got["cookie"].contains("abcdef0123456789"),
            "{}",
            got["cookie"]
        );
        assert_eq!(
            got["x-relay-pass"],
            tw_secret::mask_secret("hunter2hunter2")
        );
        assert_eq!(got["content-type"], "application/json");
        assert_eq!(got["anthropic-version"], "2023-06-01");
        let dump = format!("{got:?}");
        for raw in [KEY, "hunter2hunter2"] {
            assert!(!dump.contains(raw), "{dump}");
        }
    }

    #[test]
    fn a_key_in_the_query_string_is_masked() {
        let r = Redaction::default();
        let got = masked_url(
            "https://bob:hunter2@generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse&key=AIzaSyA0123456789abcdefghijklmnopqrstu",
            &r,
        );
        assert!(got.starts_with("https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse&key="), "{got}");
        assert!(
            !got.contains("0123456789abcdef") && !got.contains("hunter2"),
            "{got}"
        );
        assert_eq!(masked_target("/v1/messages", &r), "/v1/messages");
    }

    /// 订阅得晚的先拿到到目前为止的，之后接着收；结束时一条 `end`，然后没有了
    #[tokio::test]
    async fn a_late_watcher_gets_what_came_before_then_what_comes_after() {
        let c = contents();
        let (sink, _rx) = crate::bodies::channel();
        let cap = open(&c, Some(sink));
        let uri: http::Uri = "/v1/messages".parse().unwrap();
        cap.client_request(
            http::Version::HTTP_11,
            &uri,
            &http::HeaderMap::new(),
            &Bytes::from_static(b"{\"model\":\"m\"}"),
            13,
        );
        cap.serving(1, Redaction::default(), true);
        assert!(cap.answer(b"event: a\ndata: {\"x\":1}\n\nevent: b\nda"));

        let mut w = c.watch(7).expect("在跑");
        let first = drain(&mut w).await;
        assert!(
            matches!(&first[0], LiveContent::Head(h) if h.line == "POST /v1/messages HTTP/1.1")
        );
        assert_eq!(
            bodies(&first, WireSide::Client, WireDir::Request, 0),
            "{\"model\":\"m\"}"
        );
        // 只给到最后一个整件的事件为止；客户端那一边是同一段
        let up = bodies(&first, WireSide::Upstream, WireDir::Response, 1);
        assert_eq!(up, "event: a\ndata: {\"x\":1}\n\n");
        assert_eq!(bodies(&first, WireSide::Client, WireDir::Response, 0), up);

        cap.answer(b"ta: {\"x\":2}\n\n");
        let more = drain(&mut w).await;
        assert_eq!(
            bodies(&more, WireSide::Upstream, WireDir::Response, 1),
            "event: b\ndata: {\"x\":2}\n\n"
        );

        cap.settled(LiveOutcome::Finished);
        drop(cap);
        let last = drain(&mut w).await;
        assert!(
            matches!(
                last.last(),
                Some(LiveContent::End(LiveEnd {
                    outcome: LiveOutcome::Finished,
                    ..
                }))
            ),
            "{last:?}"
        );
        assert!(w.next().await.is_none());
        assert!(c.watch(7).is_none(), "结束了就不在了");
        assert!(c.is_empty());
    }

    /// 没人在看、请求记录也没起来：一个字节都不攒；有人来了才攒
    #[tokio::test]
    async fn nothing_is_kept_when_nobody_needs_it() {
        let c = contents();
        let cap = open(&c, None);
        cap.serving(1, Redaction::default(), true);
        cap.answer(b"data: {\"x\":1}\n\n");
        {
            let st = cap.0.shared.lock();
            assert!(st.parts.iter().all(|p| p.body.bytes().is_empty()));
        }
        let mut w = c.watch(7).unwrap();
        cap.answer(b"data: {\"x\":2}\n\n");
        let got = drain(&mut w).await;
        assert_eq!(
            bodies(&got, WireSide::Upstream, WireDir::Response, 1),
            "data: {\"x\":2}\n\n"
        );
    }

    /// 失败了的那一跳发出去的请求体：没人在看就在换下一家时放掉；回答那一跳的留着
    #[tokio::test]
    async fn a_failed_attempts_request_body_is_let_go_when_nobody_watches() {
        let c = contents();
        let (sink, _rx) = crate::bodies::channel();
        let cap = open(&c, Some(sink));
        let body = Bytes::from_static(b"{\"converted\":true}");
        cap.sending(1, head(1), &body, &Redaction::default(), true);
        cap.sending(2, head(2), &body, &Redaction::default(), true);
        cap.serving(2, Redaction::default(), false);
        let st = cap.0.shared.lock();
        let kept: Vec<u32> = st
            .parts
            .iter()
            .filter(|p| p.dir == WireDir::Request && !p.body.bytes().is_empty())
            .map(|p| p.attempt)
            .collect();
        assert_eq!(kept, [2]);
    }

    /// 发给上游的和客户端那一边是一回事、失败了的那一跳回的错误：不落盘，没人在看就不留；
    /// 有人在看时照样实时发给他
    #[tokio::test]
    async fn what_is_not_stored_is_kept_only_while_someone_watches() {
        let c = contents();
        let (sink, _rx) = crate::bodies::channel();
        let cap = open(&c, Some(sink));
        let body = Bytes::from_static(b"{\"model\":\"m\"}");
        cap.sending(1, head(1), &body, &Redaction::default(), false);
        cap.said(1, b"{\"error\":\"boom\"}", &Redaction::default());
        {
            let st = cap.0.shared.lock();
            assert!(st.parts.iter().all(|p| p.body.bytes().is_empty()));
        }
        let mut w = c.watch(7).unwrap();
        cap.sending(2, head(2), &body, &Redaction::default(), false);
        cap.said(2, b"{\"error\":\"boom\"}", &Redaction::default());
        let got = drain(&mut w).await;
        assert!(bodies(&got, WireSide::Upstream, WireDir::Request, 1).is_empty());
        assert!(bodies(&got, WireSide::Upstream, WireDir::Response, 1).is_empty());
        assert_eq!(
            bodies(&got, WireSide::Upstream, WireDir::Request, 2),
            "{\"model\":\"m\"}"
        );
        assert_eq!(
            bodies(&got, WireSide::Upstream, WireDir::Response, 2),
            "{\"error\":\"boom\"}"
        );
        // 两跳的报文头都在
        assert_eq!(
            got.iter()
                .filter(|e| matches!(e, LiveContent::Head(_)))
                .count(),
            2
        );
    }

    fn head(attempt: u32) -> HeadView {
        HeadView {
            side: WireSide::Upstream,
            dir: WireDir::Request,
            attempt,
            line: "POST https://up/v1/chat/completions".into(),
            headers: Vec::new(),
        }
    }

    /// 不读的订阅者不挡转发：追加一直是立刻的，攒的只有落盘本来要的那一份
    #[tokio::test]
    async fn a_watcher_that_never_reads_does_not_hold_the_gateway_up() {
        let c = contents();
        let (sink, _rx) = crate::bodies::channel();
        let cap = open(&c, Some(sink));
        cap.serving(1, Redaction::default(), true);
        let _idle = c.watch(7).unwrap();
        let chunk = vec![b'x'; 64 * 1024];
        let start = std::time::Instant::now();
        for _ in 0..200 {
            cap.answer(&chunk);
        }
        assert!(start.elapsed() < Duration::from_secs(2));
        let st = cap.0.shared.lock();
        assert!(st.parts[0].body.bytes().len() <= WINDOW, "攒到上限为止");
        assert_eq!(st.parts[0].total, 200 * 64 * 1024);
    }

    /// 过了上限：最后一段说 truncated，之后不再有
    #[tokio::test]
    async fn past_the_cap_the_last_body_says_truncated() {
        let c = contents();
        let (sink, _rx) = crate::bodies::channel();
        let cap = open(&c, Some(sink));
        cap.serving(1, Redaction::default(), false);
        let mut w = c.watch(7).unwrap();
        let frame = format!("data: {}\n\n", "y".repeat(1000));
        for _ in 0..(WINDOW / frame.len() + 50) {
            cap.answer(frame.as_bytes());
        }
        // 4 MB 的打码在调试版里要一阵子：等到最后那一段为止
        let mut up: Vec<LiveBody> = Vec::new();
        while !up.last().is_some_and(|b| b.truncated) {
            match tokio::time::timeout(Duration::from_secs(60), w.next()).await {
                Ok(Some(LiveContent::Body(b))) if b.side == WireSide::Upstream => up.push(b),
                Ok(Some(_)) => {}
                other => panic!("没等到截断的那一段：{other:?}"),
            }
        }
        let total: usize = up.iter().map(|b| b.text.len()).sum();
        assert!(total <= tw_api::BODY_MAX, "{total}");
        assert!(up.last().unwrap().truncated);
        assert!(up[..up.len() - 1].iter().all(|b| !b.truncated));
        cap.answer(frame.as_bytes());
        let after = drain(&mut w).await;
        assert!(
            after.iter().all(|e| !matches!(e, LiveContent::Body(_))),
            "{after:?}"
        );
    }

    /// 两个订阅者各读各的，谁也不少
    #[tokio::test]
    async fn two_watchers_each_get_everything() {
        let c = contents();
        let cap = open(&c, None);
        let mut a = c.watch(7).unwrap();
        let mut b = c.watch(7).unwrap();
        cap.serving(1, Redaction::default(), false);
        cap.answer(b"data: 1\n\n");
        let got_a = drain(&mut a).await;
        cap.answer(b"data: 2\n\n");
        let got_a2 = drain(&mut a).await;
        let got_b = drain(&mut b).await;
        let up = |evs: &[LiveContent]| bodies(evs, WireSide::Upstream, WireDir::Response, 1);
        assert_eq!(up(&got_a) + &up(&got_a2), "data: 1\n\ndata: 2\n\n");
        assert_eq!(up(&got_b), "data: 1\n\ndata: 2\n\n");
        // 不是镜像：客户端那一边没有这一段
        assert!(bodies(&got_b, WireSide::Client, WireDir::Response, 0).is_empty());
    }

    /// 实时发出去的和落盘的打码一样：一段一段打和整份打是同一个结果
    #[tokio::test]
    async fn the_live_text_is_masked_exactly_like_the_stored_body() {
        let c = contents();
        let (sink, mut rx) = crate::bodies::channel();
        let cap = open(&c, Some(sink.clone()));
        cap.serving(1, Redaction::default(), true);
        let mut w = c.watch(7).unwrap();
        let frames = [
            format!("data: {{\"text\":\"key {KEY}\"}}\n\n"),
            "data: {\"text\":\"库 postgres://app:hunter2hunter2@db/x\"}\n\n".to_string(),
            "data: {\"text\":\"卡 6222 0212 3456 7894\"}\n\n".to_string(),
        ];
        let mut live = String::new();
        for f in &frames {
            cap.answer(f.as_bytes());
            live.push_str(&bodies(
                &drain(&mut w).await,
                WireSide::Upstream,
                WireDir::Response,
                1,
            ));
        }
        let (whole, len) = cap.freeze_answer().unwrap();
        crate::bodies::offer(
            &Some(sink),
            BodyRecord::new(
                7,
                1_000,
                BodyKind::Response,
                whole,
                len,
                Redaction::default(),
            ),
        );
        let stored = rx.recv().await.unwrap().for_disk();
        assert_eq!(live.as_bytes(), &stored.body[..]);
        for raw in [KEY, "hunter2hunter2", "6222 0212 3456 7894"] {
            assert!(!live.contains(raw), "{live}");
        }
    }

    /// 结束时另存的：报文头（同一段只留最后一遍）、和客户端那一边不一样的请求体、不是镜像时
    /// 客户端收到的回答
    #[tokio::test]
    async fn what_differs_is_stored_when_it_ends() {
        let c = contents();
        let (sink, mut rx) = crate::bodies::channel();
        let cap = open(&c, Some(sink));
        let body = Bytes::from_static(b"{\"messages\":[]}");
        cap.sending(1, head(1), &body, &Redaction::default(), true);
        let mut again = head(1);
        again.line = "POST https://up/v1/chat/completions?again".into();
        cap.sending(1, again, &body, &Redaction::default(), true);
        cap.serving(1, Redaction::default(), false);
        cap.answer(b"data: {}\n\n");
        let resp = axum::response::Response::new(axum::body::Body::from("event: x\ndata: {}\n\n"));
        let resp = cap.clone().client_answer(http::Version::HTTP_11, resp);
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        cap.settled(LiveOutcome::Finished);
        drop(cap);
        let mut got = HashMap::new();
        while let Ok(r) = rx.try_recv() {
            got.insert(format!("{:?}", r.kind), r.for_disk().body);
        }
        let heads: Vec<HeadView> = serde_json::from_slice(&got["Heads"]).unwrap();
        assert_eq!(heads.len(), 2, "{heads:?}");
        assert!(heads[0].line.ends_with("?again"));
        assert_eq!(heads[1].line, "HTTP/1.1 200 OK");
        assert_eq!(&got["UpstreamRequest"][..], b"{\"messages\":[]}");
        assert_eq!(&got["ClientResponse"][..], b"event: x\ndata: {}\n\n");
    }
}
