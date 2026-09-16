//! 一个响应体怎么收场。
//!
//! 流式响应的结局只能由流自己来报 —— handler 在第一个字节发出去之前就
//! 已经返回了。跑完了报 `RequestFinished`，上游断了或者被防火墙切断报
//! `RequestFailed`，这两种都走得到流的末尾。
//!
//! **第三种走不到。**客户端先走了（Claude Code 里按一下 Esc），hyper 丢掉
//! 响应体，流停在它当时等着的那个 await 上被整个丢掉，末尾的代码一行都
//! 不会执行。可那一刻上游已经在计费了：输入全额，输出算到断开为止。什么
//! 都不报的话，存储层永远等不到这个请求的结局 —— 它不落库，那笔钱就从账
//! 上消失了，界面上那一行也永远停在「进行中」。
//!
//! 所以结局挂在一个跟着流走的对象上（和 [`crate::live::Pass`] 同一个
//! 做法）：正常收尾时显式地报，没报就被丢掉的，由 Drop 替它报「客户端
//! 取消」。一个请求因此**恰好有一个结局** —— 少一个是一条永远不落库的
//! 记录，多一个是同一行被写两遍。

use std::time::Instant;

use crate::bodies::{BodyKind, BodyRecord, BodySender, ResponseTap};
use crate::usage::{Sniffer, Usage};

/// 一个还欠着结局的请求。
///
/// **到目前为止对响应知道的一切都在它身上**：收到多少字节、嗅到多少用量、
/// 攒下的响应体。放在一处是因为结局要用的正是这些 —— 不管这个结局是走到
/// 流的末尾报的，还是在 Drop 里报的。
#[must_use = "丢掉它就等于报告客户端已经走了"]
pub struct Ending {
    bus: tw_observe::EventBus,
    id: u64,
    status: u16,
    started: Instant,
    /// 请求开始的时刻。响应体按它归档，和请求体那一份对得上
    at_ms: i64,
    sink: Option<BodySender>,
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
        status: u16,
        started: Instant,
        at_ms: i64,
        sink: Option<BodySender>,
    ) -> Self {
        Self {
            bus,
            id,
            status,
            started,
            at_ms,
            sink,
            bytes: 0,
            sniffer: Sniffer::new(),
            tap: ResponseTap::new(),
            told: false,
        }
    }

    /// 上游来了一块。**这里看的是上游原话**（带占位符的那一版）：usage
    /// 数字不受影响，而请求详情里存的正是「发出去的和收回来的」。
    pub fn feed(&mut self, chunk: &[u8]) {
        self.bytes += chunk.len() as u64;
        self.sniffer.feed(chunk);
        self.tap.feed(chunk);
    }

    /// 流走到了末尾。
    pub fn finished(mut self) {
        let usage = self.settle();
        self.bus.emit(tw_api::Event::RequestFinished {
            id: self.id,
            status: self.status,
            bytes: self.bytes,
            duration_ms: self.duration_ms(),
            usage,
        });
    }

    /// 流断了，或者被切断了。**响应头早就发出去了**，这条事件是界面上
    /// 那一行唯一能知道它失败了的途径。
    pub fn failed(mut self, source: &str, message: String) {
        self.settle();
        self.bus.emit(tw_api::Event::RequestFailed {
            id: self.id,
            source: source.to_string(),
            message,
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
        // 流是在网关自己的代码里崩掉的。**记成取消会冤枉客户端** ——
        // 排查的人会去问一个根本没做过这件事的客户端。
        if std::thread::panicking() {
            self.bus.emit(tw_api::Event::RequestFailed {
                id: self.id,
                source: "internal".to_string(),
                message: "流中断：网关内部出错".to_string(),
            });
            return;
        }
        self.bus.emit(tw_api::Event::RequestCancelled {
            id: self.id,
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

    fn ending(bus: &tw_observe::EventBus) -> Ending {
        Ending::new(bus.clone(), 7, 200, Instant::now(), 1_000, None)
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
        let mut e = ending(&bus);
        e.feed(MESSAGE_START);
        e.feed(MESSAGE_DELTA);
        e.finished();

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
        let mut e = ending(&bus);
        e.feed(MESSAGE_START);
        e.failed("upstream", "流中断：上游断开了".into());

        let got = drain(&mut rx);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(
            matches!(&got[0], Event::RequestFailed { source, .. } if source == "upstream"),
            "{got:?}"
        );
    }

    /// **这条是这个类型存在的理由。**没有人报结局就被丢掉的流，是客户端
    /// 先走了 —— 而上游已经为它看到的那些 token 计了费。
    #[test]
    fn dropped_before_its_end_it_reports_a_cancellation_with_what_it_saw() {
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        let (tx, mut bodies) = tokio::sync::mpsc::channel(4);
        let mut e = Ending::new(bus.clone(), 7, 200, Instant::now(), 1_000, Some(tx));
        e.feed(MESSAGE_START);
        drop(e);

        let got = drain(&mut rx);
        assert_eq!(got.len(), 1, "{got:?}");
        match &got[0] {
            Event::RequestCancelled {
                id: 7,
                status: 200,
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

    /// 客户端在第一帧之前就走了。**没有用量就是 None，不是零** —— 零会让
    /// 一次真实的调用看起来是免费的。
    #[test]
    fn dropped_before_the_first_chunk_it_reports_no_usage_rather_than_zero() {
        let bus = tw_observe::EventBus::new();
        let mut rx = bus.subscribe();
        drop(ending(&bus));

        let got = drain(&mut rx);
        assert!(
            matches!(
                got.as_slice(),
                [Event::RequestCancelled {
                    bytes: 0,
                    usage: None,
                    ..
                }]
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
        let e = ending(&bus);
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
            matches!(got.as_slice(), [Event::RequestFailed { source, .. }] if source == "internal"),
            "{got:?}"
        );
    }
}
