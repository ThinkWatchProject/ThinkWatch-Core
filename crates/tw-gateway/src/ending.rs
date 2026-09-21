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

use crate::bodies::{BodyKind, BodyRecord, BodySender, ResponseTap};
use crate::usage::{Sniffer, Usage};
use tw_types::{Msg, msg};

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
    sink: Option<BodySender>,
    /// 上游的响应头。**没到的时候客户端就走了的，没有状态码可报** —— 那时
    /// 报一个 0 或者 499，都是在编
    status: Option<u16>,
    /// 从上游收到多少字节。**数的是上游原话**，不是还原、翻译之后的那版
    bytes: u64,
    /// 旁路嗅探。客户端走掉那一刻手里有多少用量，靠的就是它
    sniffer: Sniffer,
    tap: ResponseTap,
    /// 报过了。**只能报一次**
    told: bool,
}

impl Ending {
    pub fn new(
        bus: tw_observe::EventBus,
        id: u64,
        model: String,
        started: Instant,
        at_ms: i64,
        sink: Option<BodySender>,
    ) -> Self {
        Self {
            bus,
            id,
            model,
            started,
            at_ms,
            sink,
            status: None,
            bytes: 0,
            sniffer: Sniffer::new(),
            tap: ResponseTap::new(),
            told: false,
        }
    }

    /// 上游的响应头到了。从这里起，客户端再走掉，报出去的取消带着状态码。
    pub fn responded(&mut self, status: u16) {
        self.status = Some(status);
    }

    /// 上游来了一块。**这里看的是上游原话**（带占位符的那一版）：usage
    /// 数字不受影响，而请求详情里存的正是「发出去的和收回来的」。
    pub fn feed(&mut self, chunk: &[u8]) {
        self.bytes += chunk.len() as u64;
        self.sniffer.feed(chunk);
        self.tap.feed(chunk);
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

    /// 走完了。
    pub fn finished(mut self, status: u16) {
        self.status = Some(status);
        let usage = self.settle();
        self.bus.emit(tw_api::Event::RequestFinished {
            id: self.id,
            model: std::mem::take(&mut self.model),
            status,
            bytes: self.bytes,
            duration_ms: self.duration_ms(),
            usage,
        });
    }

    /// 失败了：上游不行、策略不让、流断了或者被切断了。`source` 用
    /// `x-thinkwatch-error` 那个词表。
    ///
    /// **断在流中间的失败也带着用量** —— 上游已经为它计了费。
    pub fn failed(mut self, source: &str, message: Msg) {
        let usage = self.settle();
        self.bus.emit(tw_api::Event::RequestFailed {
            id: self.id,
            model: std::mem::take(&mut self.model),
            source: source.to_string(),
            message,
            bytes: self.received(),
            duration_ms: Some(self.duration_ms()),
            usage,
        });
    }

    /// 收尾：先把响应体交出去，再拿走用量。三种结局共用，只走一次。
    fn settle(&mut self) -> Option<tw_api::UsageView> {
        self.told = true;
        let (recorded, original_len) = std::mem::take(&mut self.tap).finish();
        if !recorded.is_empty() {
            crate::bodies::offer(
                &self.sink,
                BodyRecord {
                    id: self.id,
                    at_ms: self.at_ms,
                    kind: BodyKind::Response,
                    body: recorded,
                    original_len,
                },
            );
        }
        std::mem::take(&mut self.sniffer).finish().map(view)
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
        let usage = self.settle();
        // 是网关自己的代码崩掉了。**记成取消会冤枉客户端** —— 排查的人
        // 会去问一个根本没做过这件事的客户端。
        if std::thread::panicking() {
            self.bus.emit(tw_api::Event::RequestFailed {
                id: self.id,
                model: std::mem::take(&mut self.model),
                source: "internal".to_string(),
                message: msg!(
                    "gw.internal" => "The request was interrupted by an error inside the gateway."
                ),
                bytes: self.received(),
                duration_ms: Some(self.duration_ms()),
                usage,
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
        });
    }
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
            "upstream",
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
                assert_eq!(source, "upstream");
                assert_eq!(*bytes, Some(MESSAGE_START.len() as u64));
                // **断在中间也要带着用量**：输入在第一帧里就齐了，上游已经为它计费
                assert_eq!((u.input, u.cache_read), (5000, 4000));
            }
            other => panic!("该是一条带着用量的失败，实际 {other:?}"),
        }
    }

    /// **三种结局都带着模型名。**听事件的一方可能是请求开始之后才来的（界面
    /// 的实时曲线就是这样），它手上只有结局 —— 这笔用量记在哪个模型上，只能
    /// 看结局里写的。
    #[test]
    fn every_ending_carries_the_model() {
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        responding(&bus).finished(200);
        responding(&bus).failed("upstream", msg!("t.x" => "x"));
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

    /// 响应头之前就失败了（每家上游都拒绝、策略不让）。**没有字节、没有
    /// 用量，但有耗时** —— 「试了二十秒才放弃」本身就是排查的线索。
    #[test]
    fn a_failure_before_the_response_headers_has_a_duration_but_no_bytes_or_usage() {
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        Ending::new(bus.clone(), 7, MODEL.into(), Instant::now(), 1_000, None)
            .failed("rate_limited", msg!("t.limited" => "`up` rate-limited us"));

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
        let (tx, mut bodies) = tokio::sync::mpsc::channel(4);
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
                [Event::RequestFailed { source, model, .. }] if source == "internal" && model == MODEL
            ),
            "{got:?}"
        );
    }
}
