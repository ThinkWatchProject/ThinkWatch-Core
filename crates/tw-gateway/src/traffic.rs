//! 一个请求在网关和上游之间走了多少流量。
//!
//! **只数这一段**：走代理的、被计量的都是它；客户端和网关之间在本机（或局域网），不算。
//! 两个数（见 `tw_api::Event::RequestFinished`）：
//!
//! - **发出去的**：每一跳发给上游的请求体，按线上的样子 —— 网关自己压缩过的（Codex 后端的
//!   zstd）数压缩之后的。故障转移之前失败了的那几跳也算：它们一样走了一趟代理。请求头不算。
//! - **收回来的**：每一跳从上游收到的响应体，**解压之前**。失败了的那几跳读过的错误正文也算。
//!
//! 一个请求一个 [`Traffic`]，跟着请求走（[`crate::server`] 建、结局报）。发的那一处
//! （`server::pipeline::hop` 的 `send`）记发出去的，收的那一处不用一处一处去数：响应体在
//! 交给任何人读之前就套上一层（[`metered`]），**谁读它都记上** —— 读错误正文的、压着开头等
//! 第一段内容的、把回答交给客户端的，各走各的路，数的是同一个数，不会漏一处、也不会数两遍。
//!
//! 解压之前怎么数得到：**网关的 HTTP 客户端不解压**（reqwest 没开 gzip、brotli、zstd、
//! deflate 这几个特性），读到的就是线上的那些字节，上游压缩过的也还压缩着。哪天开了自动
//! 解压，这里数到的就成了解压之后的 —— `tests::the_client_does_not_decompress` 守着这一条。
//!
//! WebSocket 不走 HTTP 客户端：帧的载荷由那条路自己记（见 [`crate::ending::Ending::count`]、
//! [`crate::ending::Ending::sending`]）。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use http_body_util::BodyExt;

/// 一个请求到目前为止的流量。**只加不减**（连不上退回的那一笔除外，见 [`Traffic::unsent`]）。
#[derive(Debug, Default)]
pub struct Traffic {
    sent: AtomicU64,
    received: AtomicU64,
    /// 发出去过几次（一跳一次，同一家重发也算一次），连不上的退回。**有过一次就有流量可说**：
    /// 发出去了、什么都没收到的是 0，一次都没发出去的是没有（见 [`Traffic::reached`]）
    out: AtomicU64,
}

impl Traffic {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// 一跳要发出去了，请求体 `n` 字节（线上的样子）。**在发之前记**：等上游的时候放弃了
    /// 这一跳（无响应超时、手动中止），请求体多半已经发出去了，那时已经没有「发完」这一刻
    pub fn sending(&self, n: usize) {
        self.out.fetch_add(1, Ordering::Relaxed);
        self.sent.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// 刚记下的那一跳没发出去：连不上（地址不通、代理拒绝、握手失败），上游一个字节都没收到
    pub fn unsent(&self, n: usize) {
        take_back(&self.out, 1);
        take_back(&self.sent, n as u64);
    }

    /// 连上了，还没发任何东西：WebSocket 握手成功的那一刻。之后的帧另外记
    pub fn connected(&self) {
        self.out.fetch_add(1, Ordering::Relaxed);
    }

    /// 从上游收到了 `n` 字节（线上的样子）
    pub fn received(&self, n: usize) {
        self.received.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// 至少有一跳发到了上游（或者 WebSocket 连上了）。**没有的话没有流量可说** —— 报一对 0
    /// 看起来像发出去了、上游一声不吭
    pub fn reached(&self) -> bool {
        self.out.load(Ordering::Relaxed) > 0 || self.received.load(Ordering::Relaxed) > 0
    }

    /// （发出去的，收回来的）
    pub fn totals(&self) -> (u64, u64) {
        (
            self.sent.load(Ordering::Relaxed),
            self.received.load(Ordering::Relaxed),
        )
    }
}

/// 从计数里减掉 `n`，到 0 为止。手写比较交换：`fetch_update` 在新版标准库里改了名
/// （`try_update`），旧版又没有新名字，两头都要编得过（和 `tw_watch` 的 kqueue 同一个做法）
fn take_back(a: &AtomicU64, n: u64) {
    let mut cur = a.load(Ordering::Relaxed);
    while let Err(now) = a.compare_exchange_weak(
        cur,
        cur.saturating_sub(n),
        Ordering::Relaxed,
        Ordering::Relaxed,
    ) {
        cur = now;
    }
}

/// 给上游的响应套上一层：响应体的每一块被读到时记进 `traffic`。状态码、响应头原样。
///
/// **在交给任何人读之前套**（发出去拿到响应的那一刻）：之后读它的各处 —— 错误正文只读开头的、
/// 压着开头的、交给客户端的 —— 把读过的字节接回去重新包一层时，那几块已经记过了，接回去的不再
/// 经过这一层，不会数两遍。没读的部分（失败的一跳读完开头就扔掉了）没有收到，不算。
pub fn metered(r: reqwest::Response, traffic: &Arc<Traffic>) -> reqwest::Response {
    let t = traffic.clone();
    let r: http::Response<reqwest::Body> = r.into();
    reqwest::Response::from(r.map(|body| {
        reqwest::Body::wrap(body.map_frame(move |f| {
            if let Some(data) = f.data_ref() {
                t.received(data.len());
            }
            f
        }))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 解压之前怎么数得到，靠的是这一条：网关的 HTTP 客户端拿到的就是线上的字节。上游回了
    /// `Content-Encoding: gzip`，读出来的还是压缩过的那些（gzip 的魔数开头），数的是它们
    #[tokio::test]
    async fn the_client_does_not_decompress() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // 一段「压缩过的」正文：gzip 的魔数加几个字节。不是真的 gzip —— 客户端要是想解压，
        // 读出来的是一个错误，不是这些字节
        let body: &[u8] = &[0x1f, 0x8b, 0x08, 0x00, 1, 2, 3, 4, 5, 6];
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = s.read(&mut buf).await.unwrap();
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-encoding: gzip\r\n\
                 content-length: {}\r\n\r\n",
                body.len()
            );
            s.write_all(head.as_bytes()).await.unwrap();
            s.write_all(body).await.unwrap();
        });
        let client = crate::outbound::base_client_builder()
            .no_proxy()
            .build()
            .unwrap();
        let traffic = Traffic::new();
        let r = client
            .post(format!("http://{addr}/v1/messages"))
            .body("{}")
            .send()
            .await
            .unwrap();
        let got = metered(r, &traffic).bytes().await.unwrap();
        assert_eq!(&got[..], body);
        assert_eq!(traffic.totals(), (0, body.len() as u64));
    }

    /// 读过的字节接回去重新包一层（读错误正文的开头、压着开头的都这么做），接回去的那几块不再
    /// 数一遍
    #[tokio::test]
    async fn bytes_read_and_replayed_are_counted_once() {
        use futures::StreamExt;
        let traffic = Traffic::new();
        let resp = http::Response::new(reqwest::Body::from("abcdefghij"));
        let r = metered(reqwest::Response::from(resp), &traffic);
        let mut stream = r.bytes_stream();
        let first = stream.next().await.unwrap().unwrap();
        let replay = futures::stream::iter([Ok::<_, reqwest::Error>(first)]).chain(stream);
        let again =
            reqwest::Response::from(http::Response::new(reqwest::Body::wrap_stream(replay)));
        assert_eq!(&again.bytes().await.unwrap()[..], b"abcdefghij");
        assert_eq!(traffic.totals(), (0, 10));
    }

    /// 发出去之后连不上的那一跳退回：一次都没发到的没有流量可说，发到过的有
    #[test]
    fn an_attempt_that_never_connected_is_taken_back() {
        let t = Traffic::default();
        assert!(!t.reached());
        t.sending(100);
        t.unsent(100);
        assert!(!t.reached());
        assert_eq!(t.totals(), (0, 0));
        t.sending(100);
        t.sending(80);
        t.unsent(100);
        assert!(t.reached());
        assert_eq!(t.totals(), (80, 0));
    }
}
