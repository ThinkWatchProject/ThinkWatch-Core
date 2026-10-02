//! 一个请求怎么收场。
//!
//! 发出 `RequestStarted` 的那一刻起，这个请求就欠总线一个结局：跑完了是
//! `RequestFinished`，出错了是 `RequestFailed`，客户端先走了是
//! `RequestCancelled`。**恰好一个** —— 少一个，存储层永远等不到它：那一行
//! 不落库，上游已经计的费从账上消失，界面上那一行也永远停在「进行中」；
//! 多一个，同一行会被写两遍。
//!
//! 难的是第三种，它**走不到任何一行报结局的代码**。客户端断开时，hyper 把
//! 手上的东西整个丢掉 —— 响应头还没到时丢的是 handler 的 future，流式
//! 响应已经开始时丢的是响应体 —— 它们停在当时等着的那个 await 上，后面的
//! 代码一行都不会执行。
//!
//! 所以结局挂在一个跟着请求走的对象上（和 [`crate::live::Pass`] 同一个
//! 做法）：正常收尾时显式地报，没报就被丢掉的，由 Drop 替它报「客户端
//! 取消」。它先待在 handler 里（等响应头的那一段），拿到响应头之后交给
//! 响应体；WebSocket 那条路上交给升级之后的连接。
//!
//! **Drop 只能代表「被丢掉」。**所以 handler 里返回错误的路径一条都不能
//! 让它自己掉在地上 —— 那由 `server::passthrough` 统一按返回的错误报成
//! 失败，理由写在那儿。

use std::time::Instant;

use crate::bodies::{BodyKind, BodyRecord, BodySink, Redaction, ResponseTap};
use tw_dialect::convert::Reader;
use tw_dialect::ir;
use tw_dialect::usage::{Sniffer, Usage};
use tw_types::{Msg, msg};

/// 生成用时不到这么久的，不给速度。**块太少，量的是上游怎么分块，不是模型多快**：
/// 一段十几个 token 的回答常常一两块就到齐了，两块之间只隔几毫秒，一除就是几千
/// token/秒。
const MIN_GENERATION_MS: u64 = 500;

/// 一个还欠着结局的请求。
///
/// **到目前为止对响应知道的一切都在它身上**：状态码、收到多少字节、嗅到
/// 多少用量、攒下的响应体。放在一处是因为结局要用的正是这些 —— 不管这个
/// 结局是显式报的，还是在 Drop 里报的。
#[must_use = "dropping it reports that the client disconnected"]
pub struct Ending {
    bus: tw_observe::EventBus,
    id: u64,
    /// 客户端要的模型名。**结局带着它走**（理由见 `tw_api::Event::RequestFinished`）
    model: String,
    started: Instant,
    /// 请求开始的时刻。响应体按它归档，和请求体那一份对得上
    at_ms: i64,
    sink: Option<BodySink>,
    /// 响应体落盘之前怎么换、怎么打码：和请求体那一份是同一套（见 [`Ending::redact_with`]）
    redaction: Option<Redaction>,
    /// 上游的响应头。**没到的时候客户端就走了的，没有状态码可报** —— 那时
    /// 报一个 0 或者 499，都是在编
    status: Option<u16>,
    /// 从上游收到多少字节。**数的是上游原话**，不是还原、翻译之后的那版
    bytes: u64,
    /// 旁路嗅探。客户端走掉那一刻手里有多少用量，靠的就是它
    sniffer: Sniffer,
    tap: ResponseTap,
    /// 认第一个 token 的。**只在上游回的是成功的流时才有**（见 [`Ending::streaming`]），
    /// 认出来就扔掉 —— 之后的字节不必再解析
    first: Option<FirstToken>,
    /// 第一个 token 是什么时候、以什么开的头
    opened: Option<Opened>,
    /// 看上游有没有在流里报错。**只在上游回的是成功的流时才有**（见 [`Ending::streaming`]），
    /// 认出来就扔掉
    watch: Option<Watch>,
    /// 上游在流里报的错：哪一家、原话。**有它就不是成功** —— 响应头是 200，回答却断在了
    /// 半路
    upstream_error: Option<(String, String)>,
    /// 这段对话这一次由谁回答（见 [`crate::affinity`]）。**成功走完了才记**：失败的、
    /// 半路断了的不算回答过，下一次照常排序
    answer: Option<crate::affinity::Ticket>,
    /// 报过了。**只能报一次**
    told: bool,
}

/// 盯着流里的错误帧。
struct Watch {
    /// 哪一家上游在说话。报出去的那句话要点名
    provider: String,
    dialect: ir::Dialect,
    frames: tw_dialect::frame::Decoder,
}

impl Watch {
    /// 这些帧里上游报的第一个错
    fn error_in(&self, frames: Vec<tw_dialect::frame::Frame>) -> Option<(String, String)> {
        frames
            .iter()
            .find_map(|f| tw_dialect::convert::stream_error(self.dialect, f))
            .map(|message| (self.provider.clone(), message))
    }
}

/// 还在等第一个 token。
struct FirstToken {
    reader: Reader,
    /// 在那之前有过推理块，却一个推理的字都没有：推理被隐藏了
    hidden_thought: bool,
}

/// 第一个 token 到的那一刻。
#[derive(Debug, Clone, Copy)]
struct Opened {
    /// 从请求进来算，毫秒
    ms: u64,
    /// 开头的是推理的字：推理边想边吐，推理 token 都生成在这之后
    with_thought: bool,
    /// 开头之前推理被隐藏了：那些推理 token 生成在这之前
    hidden_thought: bool,
}

impl Ending {
    pub fn new(
        bus: tw_observe::EventBus,
        id: u64,
        model: String,
        started: Instant,
        at_ms: i64,
        sink: Option<BodySink>,
    ) -> Self {
        Self {
            bus,
            id,
            model,
            started,
            at_ms,
            sink,
            redaction: None,
            status: None,
            bytes: 0,
            sniffer: Sniffer::new(),
            tap: ResponseTap::new(),
            first: None,
            opened: None,
            watch: None,
            upstream_error: None,
            answer: None,
            told: false,
        }
    }

    /// 上游 `provider` 回的是成功的流：按它的格式认第一个 token（见
    /// `tw_api::Event::RequestFirstToken`），也看它有没有在流里报错。
    ///
    /// **按格式解析，不在字节里找关键字**，和测速（`crate::l3`）同一个读法：`message_start`、
    /// `response.created` 这些开场帧是上游收到请求就发的，认成第一个 token 的话，量到的是
    /// 建连有多快。
    ///
    /// **流里报了错的不是成功。**上游先回 200、写到一半才报 `overloaded_error` 的，客户端
    /// 拿到的是半截回答加一个错误；记成成功的话，这家上游看起来从不出错。
    pub fn streaming(&mut self, upstream: ir::Dialect, provider: &str) {
        self.first = Some(FirstToken {
            reader: Reader::new(upstream),
            hidden_thought: false,
        });
        self.watch = Some(Watch {
            provider: provider.to_string(),
            dialect: upstream,
            frames: Default::default(),
        });
    }

    /// 响应体落盘之前按什么换、打码。**和请求体用同一套**：开始时生效的规则，拦截档下
    /// 这个请求的账本 —— 上游回答里的占位符和存下来的请求对得上号。没交代的按出厂规则
    /// 打码（见 [`Redaction::default`]）
    pub fn redact_with(&mut self, r: Redaction) {
        self.redaction = Some(r);
    }

    /// 这一次由谁回答：成功走完时记下它和它读写了多少缓存。
    pub fn answered_by(&mut self, ticket: crate::affinity::Ticket) {
        self.answer = Some(ticket);
    }

    /// 上游的响应头到了。从这里起，客户端再走掉，报出去的取消带着状态码。
    pub fn responded(&mut self, status: u16) {
        self.status = Some(status);
    }

    /// 上游来了一块。**这里看的是上游原话**（带占位符的那一版）：usage
    /// 数字不受影响，而请求详情里存的正是「发出去的和收回来的」。原话里要是带着
    /// 认得出的值（观察档下模型回显的密钥），落盘之前打码（见 [`crate::bodies`]）。
    pub fn feed(&mut self, chunk: &[u8]) {
        self.bytes += chunk.len() as u64;
        self.sniffer.feed(chunk);
        // 没有去处（观测层没起来）就不攒：一个回答最多攒 4 MB，攒了也交不出去
        if self.sink.is_some() {
            self.tap.feed(chunk);
        }
        self.spot(chunk);
        self.watch_for_errors(chunk);
    }

    /// 在这一块里找上游报的错。找到一个就够了，之后不再看。
    fn watch_for_errors(&mut self, chunk: &[u8]) {
        let Some(w) = self.watch.as_mut() else {
            return;
        };
        let frames = w.frames.feed(chunk);
        if let Some(found) = w.error_in(frames) {
            self.upstream_error = Some(found);
            self.watch = None;
        }
    }

    /// 在这一块里找第一个 token。找到就报，之后不再解析。
    fn spot(&mut self, chunk: &[u8]) {
        let Some(f) = self.first.as_mut() else {
            return;
        };
        let mut opened = None;
        for e in f.reader.feed(chunk) {
            match opening(&e) {
                Some(with_thought) => {
                    opened = Some((with_thought, f.hidden_thought));
                    break;
                }
                None if hides_thought(&e) => f.hidden_thought = true,
                None => {}
            }
        }
        let Some((with_thought, hidden_thought)) = opened else {
            return;
        };
        self.first = None;
        let ms = self.duration_ms();
        self.opened = Some(Opened {
            ms,
            with_thought,
            hidden_thought,
        });
        self.bus.emit(tw_api::Event::RequestFirstToken {
            id: self.id,
            ttft_ms: ms,
        });
    }

    /// 只数字节，不嗅用量、不留档。
    ///
    /// WebSocket 那条路用它。一条连接上跑着好几轮回答，每轮各报一次用量，
    /// 而嗅探器是「每个字段取最大值」—— 喂给它，得到的是其中某一轮的数，
    /// 看起来却像整条连接的；模型名也不知道（升级请求里没有），算不了钱。
    /// **与其报一个错的数，不如说没有。**
    pub fn count(&mut self, bytes: usize) {
        self.bytes += bytes as u64;
    }

    /// 走完了。上游在流里报过错的，报的是失败（见 [`Ending::streaming`]）。
    pub fn finished(mut self, status: u16) {
        self.status = Some(status);
        // 最后一帧后面不带空行的上游：收尾时再看一眼
        if let Some(mut w) = self.watch.take() {
            let frames = w.frames.flush();
            self.upstream_error = self.upstream_error.take().or(w.error_in(frames));
        }
        if let Some((upstream, message)) = self.upstream_error.take() {
            let message: String = message.chars().take(500).collect();
            self.failed(
                tw_api::FailureSource::Upstream,
                msg!(
                    "gw.upstream.stream_error", upstream = upstream, message = message =>
                    "Upstream `{upstream}` reported an error partway through the response: {message}"
                ),
            );
            return;
        }
        let (usage, answered_model) = self.settle();
        if let Some(t) = self.answer.take()
            && (200..300).contains(&status)
        {
            t.answered(usage.as_ref().map_or(0, |u| u.cache_read + u.cache_write));
        }
        let duration_ms = self.duration_ms();
        let tokens_per_sec = rate(self.opened, usage.as_ref(), duration_ms);
        self.bus.emit(tw_api::Event::RequestFinished {
            id: self.id,
            model: std::mem::take(&mut self.model),
            status,
            bytes: self.bytes,
            duration_ms,
            usage: usage.map(view),
            tokens_per_sec,
            answered_model,
        });
    }

    /// 失败了：上游不行、策略不让、流断了或者被切断了。`source` 用
    /// `x-thinkwatch-error` 那个词表。
    ///
    /// **断在流中间的失败也带着用量** —— 上游已经为它计了费。
    pub fn failed(mut self, source: tw_api::FailureSource, message: Msg) {
        let (usage, answered_model) = self.settle();
        self.bus.emit(tw_api::Event::RequestFailed {
            id: self.id,
            model: std::mem::take(&mut self.model),
            source,
            message,
            bytes: self.received(),
            duration_ms: Some(self.duration_ms()),
            usage: usage.map(view),
            answered_model,
        });
    }

    /// 收尾：先把响应体交出去，再拿走用量和回答里写的模型名（两样是嗅探器在同一遍里
    /// 认的）。三种结局共用，只走一次。
    fn settle(&mut self) -> (Option<Usage>, Option<String>) {
        self.told = true;
        let (recorded, original_len) = std::mem::take(&mut self.tap).finish();
        if !recorded.is_empty() {
            crate::bodies::offer(
                &self.sink,
                BodyRecord::new(
                    self.id,
                    self.at_ms,
                    BodyKind::Response,
                    recorded,
                    original_len,
                    self.redaction.take().unwrap_or_default(),
                ),
            );
        }
        let sniffer = std::mem::take(&mut self.sniffer);
        let model = sniffer.model().map(str::to_string);
        (sniffer.finish(), model)
    }

    fn duration_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// 收到了多少字节。**响应头都没到的，没有「收到了多少」这回事** ——
    /// 报 0 会让它看起来像一个空响应。
    fn received(&self) -> Option<u64> {
        self.status.map(|_| self.bytes)
    }
}

impl Drop for Ending {
    fn drop(&mut self) {
        if self.told {
            return;
        }
        // **这里什么都不能 panic。**Drop 可能正跑在一次 unwind 里，那时
        // 再 panic 一次，整个进程就没了。下面每一步都是不会失败的那种：
        // 往通道里 try_send、往广播里 send、读一下时钟。
        let (usage, answered_model) = self.settle();
        let usage = usage.map(view);
        // 是网关自己的代码崩掉了。**记成取消会冤枉客户端** —— 排查的人
        // 会去问一个根本没做过这件事的客户端。
        if std::thread::panicking() {
            self.bus.emit(tw_api::Event::RequestFailed {
                id: self.id,
                model: std::mem::take(&mut self.model),
                source: tw_api::FailureSource::Internal,
                message: msg!(
                    "gw.internal" => "The request was interrupted by an error inside the gateway."
                ),
                bytes: self.received(),
                duration_ms: Some(self.duration_ms()),
                usage,
                answered_model,
            });
            return;
        }
        self.bus.emit(tw_api::Event::RequestCancelled {
            id: self.id,
            model: std::mem::take(&mut self.model),
            status: self.status,
            bytes: self.bytes,
            duration_ms: self.duration_ms(),
            usage,
            answered_model,
        });
    }
}

/// 这个事件是不是模型开口了：`Some(true)` 是推理的字，`Some(false)` 是正文或工具调用。
///
/// **工具调用一开头就算**：块开头就带着工具名，那已经是模型吐出来的了；参数的第一段
/// 常常是空的。
fn opening(e: &ir::Event) -> Option<bool> {
    match e {
        ir::Event::Delta {
            delta: ir::Delta::Thinking(t),
            ..
        } if !t.is_empty() => Some(true),
        ir::Event::Delta {
            delta: ir::Delta::Text(t) | ir::Delta::ToolInput(t),
            ..
        } if !t.is_empty() => Some(false),
        ir::Event::BlockStart {
            kind: ir::BlockKind::ToolCall { .. },
            ..
        } => Some(false),
        _ => None,
    }
}

/// 推理块开了头、或者只来了推理的签名：推理在进行，却没有字给出来
fn hides_thought(e: &ir::Event) -> bool {
    matches!(
        e,
        ir::Event::BlockStart {
            kind: ir::BlockKind::Thinking,
            ..
        } | ir::Event::Delta {
            delta: ir::Delta::Signature(_),
            ..
        }
    )
}

/// 生成速度：第一个 token 之后吐出的输出，除以从那一刻到结束用的时间。
///
/// 分子要和分母对得上。推理被隐藏的，推理 token 生成在第一个 token 之前：上游报了其中
/// 多少是推理就扣掉；没报（Anthropic 不分开报）就给不出，宁可没有也不给一个顶高好几倍
/// 的数。推理边想边吐的，推理 token 都在分母之内，照数。
fn rate(opened: Option<Opened>, usage: Option<&Usage>, duration_ms: u64) -> Option<u32> {
    let o = opened?;
    let u = usage?;
    let counted = if o.with_thought {
        u.output
    } else if u.reasoning > 0 {
        u.output.saturating_sub(u.reasoning)
    } else if o.hidden_thought {
        return None;
    } else {
        u.output
    };
    let gen_ms = duration_ms.saturating_sub(o.ms);
    if counted == 0 || gen_ms < MIN_GENERATION_MS {
        return None;
    }
    Some((counted.saturating_mul(1000) / gen_ms).min(u64::from(u32::MAX)) as u32)
}

fn view(u: Usage) -> tw_api::UsageView {
    tw_api::UsageView {
        input: u.input,
        output: u.output,
        cache_read: u.cache_read,
        cache_write: u.cache_write,
        cache_1h: u.cache_1h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tw_api::Event;

    /// Anthropic 流的第一帧：输入和缓存读是齐的，输出是个占位的 1。
    const MESSAGE_START: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5000,\"cache_read_input_tokens\":4000,\"output_tokens\":1}}}\n\n";
    const MESSAGE_DELTA: &[u8] = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":777}}\n\n";
    const MODEL: &str = "claude-sonnet-5";

    /// 一个响应头已经到了的请求。
    fn responding(bus: &tw_observe::EventBus) -> Ending {
        let mut e = Ending::new(bus.clone(), 7, MODEL.into(), Instant::now(), 1_000, None);
        e.responded(200);
        e
    }

    /// 总线上此刻有的全部事件。
    fn drain(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> Vec<Event> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// 走到末尾的流报一次结束，**Drop 时不再补一个取消**。补了的话，每一个
    /// 正常跑完的请求都会被写成两行。
    #[test]
    fn a_stream_that_reaches_its_end_is_finished_and_nothing_else() {
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        let mut e = responding(&bus);
        e.feed(MESSAGE_START);
        e.feed(MESSAGE_DELTA);
        e.finished(200);

        let got = drain(&mut rx);
        assert_eq!(got.len(), 1, "{got:?}");
        match &got[0] {
            Event::RequestFinished {
                id: 7,
                status: 200,
                bytes,
                usage: Some(u),
                ..
            } => {
                assert_eq!(*bytes, (MESSAGE_START.len() + MESSAGE_DELTA.len()) as u64);
                assert_eq!((u.input, u.output, u.cache_read), (5000, 777, 4000));
            }
            other => panic!("该是一条结束，实际 {other:?}"),
        }
    }

    #[test]
    fn a_stream_that_broke_is_failed_and_nothing_else() {
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        let mut e = responding(&bus);
        e.feed(MESSAGE_START);
        e.failed(
            tw_api::FailureSource::Upstream,
            msg!("t.broke" => "the stream broke: the upstream disconnected"),
        );

        let got = drain(&mut rx);
        assert_eq!(got.len(), 1, "{got:?}");
        match &got[0] {
            Event::RequestFailed {
                source,
                bytes,
                duration_ms: Some(_),
                usage: Some(u),
                ..
            } => {
                assert_eq!(*source, tw_api::FailureSource::Upstream);
                assert_eq!(*bytes, Some(MESSAGE_START.len() as u64));
                // **断在中间也要带着用量**：输入在第一帧里就齐了，上游已经为它计费
                assert_eq!((u.input, u.cache_read), (5000, 4000));
            }
            other => panic!("该是一条带着用量的失败，实际 {other:?}"),
        }
    }

    /// 上游先回 200、写到一半才在流里报错：**不是成功**。结局是一条带着用量的失败 ——
    /// 输入在第一帧里就齐了，上游已经为它计费
    #[test]
    fn an_error_partway_through_a_successful_stream_is_a_failure() {
        const OVERLOADED: &[u8] = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        let mut e = responding(&bus);
        e.streaming(ir::Dialect::Anthropic, "官方");
        e.feed(MESSAGE_START);
        // 错误帧被切成一个字节一个字节地到
        for b in OVERLOADED {
            e.feed(std::slice::from_ref(b));
        }
        e.finished(200);

        let got = drain(&mut rx);
        match got.as_slice() {
            [
                Event::RequestFailed {
                    source,
                    message,
                    usage: Some(u),
                    ..
                },
            ] => {
                assert_eq!(*source, tw_api::FailureSource::Upstream);
                assert_eq!(message.code, "gw.upstream.stream_error");
                assert!(message.text.contains("`官方`"), "{}", message.text);
                assert!(message.text.ends_with("Overloaded"), "{}", message.text);
                assert_eq!(u.input, 5000);
            }
            other => panic!("该是一条失败，实际 {other:?}"),
        }
    }

    /// 最后一帧后面不带空行的上游：错误在收尾时才读得全
    #[test]
    fn an_error_in_the_last_unterminated_frame_is_still_seen() {
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        let mut e = responding(&bus);
        e.streaming(ir::Dialect::Chat, "中转");
        e.feed(b"data: {\"error\":{\"message\":\"Provider disconnected\"}}");
        e.finished(200);
        assert!(
            matches!(drain(&mut rx).as_slice(), [Event::RequestFailed { message, .. }]
                if message.text.ends_with("Provider disconnected")),
        );
    }

    /// 不是流的（非流式、错误响应）不看：那些不经过这里认错误
    #[test]
    fn only_a_stream_is_watched() {
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        let mut e = responding(&bus);
        e.feed(b"event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"x\"}}\n\n");
        e.finished(200);
        assert!(matches!(
            drain(&mut rx).as_slice(),
            [Event::RequestFinished { .. }]
        ));
    }

    /// **三种结局都带着模型名。**听事件的一方可能是请求开始之后才来的（界面
    /// 的实时曲线就是这样），它手上只有结局 —— 这笔用量记在哪个模型上，只能
    /// 看结局里写的。
    #[test]
    fn every_ending_carries_the_model() {
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        responding(&bus).finished(200);
        responding(&bus).failed(tw_api::FailureSource::Upstream, msg!("t.x" => "x"));
        drop(responding(&bus));

        let got = drain(&mut rx);
        assert_eq!(got.len(), 3, "{got:?}");
        for e in &got {
            let model = match e {
                Event::RequestFinished { model, .. }
                | Event::RequestFailed { model, .. }
                | Event::RequestCancelled { model, .. } => model,
                other => panic!("该是一个结局，实际 {other:?}"),
            };
            assert_eq!(model, MODEL, "{e:?}");
        }
    }

    /// **三种结局都带着上游在回答里写的模型名**（和用量同一遍认的）。没写的、没收到
    /// 回答的、WebSocket 那条路只数字节的，都没有
    #[test]
    fn every_ending_carries_the_model_the_upstream_named() {
        const NAMED: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-4-5-20250929\",\"usage\":{\"input_tokens\":5000,\"output_tokens\":1}}}\n\n";
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        let fed = || {
            let mut e = responding(&bus);
            e.feed(NAMED);
            e
        };
        fed().finished(200);
        fed().failed(tw_api::FailureSource::Upstream, msg!("t.x" => "x"));
        drop(fed());
        // 回答里没写模型名的
        let mut silent = responding(&bus);
        silent.feed(MESSAGE_START);
        silent.finished(200);
        // WebSocket 那条路只数字节
        let mut ws = responding(&bus);
        ws.count(NAMED.len());
        ws.finished(101);

        let got = drain(&mut rx);
        let named: Vec<Option<&str>> = got
            .iter()
            .map(|e| match e {
                Event::RequestFinished { answered_model, .. }
                | Event::RequestFailed { answered_model, .. }
                | Event::RequestCancelled { answered_model, .. } => answered_model.as_deref(),
                other => panic!("该是一个结局，实际 {other:?}"),
            })
            .collect();
        let sonnet = Some("claude-sonnet-4-5-20250929");
        assert_eq!(named, [sonnet, sonnet, sonnet, None, None], "{got:?}");
    }

    /// 响应头之前就失败了（每家上游都拒绝、策略不让）。**没有字节、没有
    /// 用量，但有耗时** —— 「试了二十秒才放弃」本身就是排查的线索。
    #[test]
    fn a_failure_before_the_response_headers_has_a_duration_but_no_bytes_or_usage() {
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        Ending::new(bus.clone(), 7, MODEL.into(), Instant::now(), 1_000, None).failed(
            tw_api::FailureSource::RateLimited,
            msg!("t.limited" => "`up` rate-limited us"),
        );

        let got = drain(&mut rx);
        assert!(
            matches!(
                got.as_slice(),
                [Event::RequestFailed {
                    bytes: None,
                    duration_ms: Some(_),
                    usage: None,
                    ..
                }]
            ),
            "{got:?}"
        );
    }

    /// **这条是这个类型存在的理由。**没有人报结局就被丢掉的流，是客户端
    /// 先走了 —— 而上游已经为它看到的那些 token 计了费。
    #[test]
    fn dropped_mid_stream_it_reports_a_cancellation_with_what_it_saw() {
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        let (tx, mut bodies) = crate::bodies::channel();
        let mut e = Ending::new(
            bus.clone(),
            7,
            MODEL.into(),
            Instant::now(),
            1_000,
            Some(tx),
        );
        e.responded(200);
        e.feed(MESSAGE_START);
        drop(e);

        let got = drain(&mut rx);
        assert_eq!(got.len(), 1, "{got:?}");
        match &got[0] {
            Event::RequestCancelled {
                id: 7,
                status: Some(200),
                bytes,
                usage: Some(u),
                ..
            } => {
                assert_eq!(*bytes, MESSAGE_START.len() as u64);
                // 输入和缓存读在第一帧里就是齐的，那正是账单上最大的一块
                assert_eq!((u.input, u.cache_read), (5000, 4000));
            }
            other => panic!("该是一条取消，实际 {other:?}"),
        }
        // 收到的那一截响应也要留档 —— 详情里要能看到客户端走之前拿到了什么
        let body = bodies.try_recv().expect("取消的请求没有留下响应体");
        assert_eq!(body.kind, BodyKind::Response);
        assert_eq!(&body.body[..], MESSAGE_START);
        assert_eq!(body.at_ms, 1_000);
    }

    /// 观测层没起来（没有去处）的时候，回答一个字节都不攒
    #[test]
    fn with_nowhere_to_send_it_the_answer_is_not_kept() {
        let bus = tw_observe::EventBus::new();
        let mut e = Ending::new(bus, 7, MODEL.into(), Instant::now(), 1_000, None);
        e.responded(200);
        e.feed(MESSAGE_START);
        let (kept, seen) = std::mem::take(&mut e.tap).finish();
        assert!(kept.is_empty() && seen == 0, "{seen}");
        // 数还是照数的
        assert_eq!(e.bytes, MESSAGE_START.len() as u64);
        e.finished(200);
    }

    /// 响应头还没到，客户端就走了。**没有状态码，也没有用量** —— 两个都是
    /// None，不是 0：0 会让一次真实的调用看起来是免费的，状态码 0 则是编的。
    #[test]
    fn dropped_before_the_response_headers_it_reports_neither_a_status_nor_usage() {
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        drop(Ending::new(
            bus.clone(),
            7,
            MODEL.into(),
            Instant::now(),
            1_000,
            None,
        ));

        let got = drain(&mut rx);
        assert!(
            matches!(
                got.as_slice(),
                [Event::RequestCancelled {
                    status: None,
                    bytes: 0,
                    usage: None,
                    ..
                }]
            ),
            "{got:?}"
        );
    }

    const TEXT_BLOCK: &[u8] = b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n";
    const TEXT_DELTA: &[u8] = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n";

    fn first_tokens(got: &[Event]) -> Vec<u64> {
        got.iter()
            .filter_map(|e| match e {
                Event::RequestFirstToken { ttft_ms, .. } => Some(*ttft_ms),
                _ => None,
            })
            .collect()
    }

    /// **开场帧不算。**`message_start` 和空的文本块是上游收到请求就发的；第一个字才是
    /// 模型开口。报一次，之后不再报
    #[test]
    fn the_first_token_is_the_first_word_not_the_opening_frames() {
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        let mut e = responding(&bus);
        e.streaming(ir::Dialect::Anthropic, "up");
        e.feed(MESSAGE_START);
        e.feed(TEXT_BLOCK);
        assert!(drain(&mut rx).is_empty(), "开场帧被当成了第一个 token");
        e.feed(TEXT_DELTA);
        e.feed(TEXT_DELTA);
        let got = drain(&mut rx);
        assert_eq!(first_tokens(&got).len(), 1, "{got:?}");
        e.finished(200);
        assert!(first_tokens(&drain(&mut rx)).is_empty());
    }

    /// 工具调用一开头就算：块开头就带着工具名，参数的第一段常常是空的
    #[test]
    fn a_tool_call_opens_the_answer_before_its_arguments() {
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        let mut e = responding(&bus);
        e.streaming(ir::Dialect::Anthropic, "up");
        e.feed(MESSAGE_START);
        e.feed(b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Bash\",\"input\":{}}}\n\n");
        assert_eq!(first_tokens(&drain(&mut rx)).len(), 1);
        drop(e);
    }

    /// 没说是流的（非流式、上游回了错误）不解析，也就没有第一个 token
    #[test]
    fn a_response_that_is_not_a_stream_has_no_first_token() {
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        let mut e = responding(&bus);
        e.feed(TEXT_DELTA);
        e.finished(200);
        let got = drain(&mut rx);
        assert!(first_tokens(&got).is_empty(), "{got:?}");
        assert!(
            matches!(
                got.as_slice(),
                [Event::RequestFinished {
                    tokens_per_sec: None,
                    ..
                }]
            ),
            "{got:?}"
        );
    }

    fn usage(output: u64, reasoning: u64) -> Usage {
        Usage {
            output,
            reasoning,
            ..Default::default()
        }
    }

    fn opened(ms: u64, with_thought: bool, hidden_thought: bool) -> Option<Opened> {
        Some(Opened {
            ms,
            with_thought,
            hidden_thought,
        })
    }

    /// 从第一个 token 算到结束：第一秒在排队、读输入，之后 5 秒吐了 500 个
    #[test]
    fn the_rate_counts_from_the_first_token() {
        let u = usage(500, 0);
        assert_eq!(
            rate(opened(1_000, false, false), Some(&u), 6_000),
            Some(100)
        );
    }

    /// **推理被隐藏、上游报了其中多少是推理**：那 2000 个推理 token 生成在第一个 token
    /// 之前，不扣掉的话是 1100 token/秒
    #[test]
    fn hidden_reasoning_is_left_out_when_the_upstream_says_how_much_it_was() {
        let u = usage(2_200, 2_000);
        assert_eq!(
            rate(opened(20_000, false, true), Some(&u), 22_000),
            Some(100)
        );
        // Chat Completions 的推理模型连推理块都不发，只在用量里报推理 token
        assert_eq!(
            rate(opened(20_000, false, false), Some(&u), 22_000),
            Some(100)
        );
    }

    /// 推理被隐藏、上游又不分开报（Anthropic）：给不出一个对的数，就不给
    #[test]
    fn hidden_reasoning_of_unknown_size_gives_no_rate() {
        let u = usage(2_200, 0);
        assert_eq!(rate(opened(20_000, false, true), Some(&u), 22_000), None);
    }

    /// 推理边想边吐的，推理 token 都生成在第一个 token 之后，照数
    #[test]
    fn visible_reasoning_is_counted() {
        let u = usage(1_000, 800);
        assert_eq!(rate(opened(1_000, true, true), Some(&u), 11_000), Some(100));
    }

    /// 生成不到半秒、没有第一个 token、没有用量、没有输出的，都没有速度
    #[test]
    fn a_rate_needs_a_first_token_output_and_enough_time() {
        let u = usage(40, 0);
        assert_eq!(rate(opened(1_000, false, false), Some(&u), 1_300), None);
        assert_eq!(rate(None, Some(&u), 5_000), None);
        assert_eq!(rate(opened(1_000, false, false), None, 5_000), None);
        assert_eq!(
            rate(opened(1_000, false, false), Some(&usage(0, 0)), 5_000),
            None
        );
    }

    /// WebSocket 那条路只数字节。**帧里的 usage 不能被嗅成整条连接的用量**
    /// —— 那是某一轮的数。
    #[test]
    fn counting_bytes_does_not_turn_frames_into_usage() {
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        // WebSocket 那条路不知道模型名
        let mut e = Ending::new(bus.clone(), 7, String::new(), Instant::now(), 1_000, None);
        e.responded(101);
        e.count(MESSAGE_START.len());
        e.finished(101);

        let got = drain(&mut rx);
        assert!(
            matches!(
                got.as_slice(),
                [Event::RequestFinished { status: 101, bytes, usage: None, .. }]
                    if *bytes == MESSAGE_START.len() as u64
            ),
            "{got:?}"
        );
    }

    /// 流在网关自己的代码里崩掉了。**Drop 同样会跑**（unwind 会丢掉流里的
    /// 局部变量），而这时报「客户端取消」是在冤枉客户端。
    ///
    /// 这里用的是真的 `stream!`，因为要验的正是生成器被 unwind 时，Drop
    /// 看得见那次 panic。
    #[test]
    fn a_stream_that_panics_is_not_blamed_on_the_client() {
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        let e = responding(&bus);
        let s = async_stream::stream! {
            let mut e = e;
            e.feed(MESSAGE_START);
            yield 1u8;
            panic!("流里的一个 bug");
        };
        let mut s = Box::pin(s);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            futures::executor::block_on(async { while s.next().await.is_some() {} })
        }));
        assert!(r.is_err(), "流该 panic");
        drop(s);

        let got = drain(&mut rx);
        assert!(
            matches!(
                got.as_slice(),
                [Event::RequestFailed { source, model, .. }] if *source == tw_api::FailureSource::Internal && model == MODEL
            ),
            "{got:?}"
        );
    }
}
