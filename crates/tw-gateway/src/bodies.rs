//! 把请求体和响应体交出去存起来（DESIGN.md §8）。
//!
//! **走一条有界的通道，不是一次直接调用。**直接调用意味着文件 I/O 跑在
//! 转发那条路上 —— 一次慢磁盘写就会变成一次慢请求，而观测永远不该有这
//! 个权力（§4.7）。通道满了就丢：丢的是一条观测记录，而等它是在惩罚
//! 真实用户。

use bytes::Bytes;

/// 响应体最多留多少。
///
/// **和落盘的上限（4 MB）是两个数。**这个是「在内存里攒着等着交出去」的
/// 量，同时在飞的请求越多它乘得越狠 —— 256 KB × 32 并发是 8 MB，可以；
/// 4 MB × 32 是 128 MB，不行。而详情抽屉要看的东西，开头这些字节里全有。
pub const RESPONSE_TAP_MAX: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    Request,
    Response,
}

/// 一份要存起来的 body。
#[derive(Debug)]
pub struct BodyRecord {
    pub id: u64,
    pub at_ms: i64,
    pub kind: BodyKind,
    pub body: Bytes,
    /// 原始长度。**截断了要能说出来** —— 不说的话，用户会以为请求本身
    /// 就长这样
    pub original_len: usize,
}

/// 往哪儿交。`None` 表示观测层没起来 —— 那时什么都不做，转发照旧。
pub type BodySender = tokio::sync::mpsc::Sender<BodyRecord>;

/// 通道容量。
///
/// 攒不下就丢。**这个数字要小**：它乘上单条 256 KB 就是内存上限，
/// 而攒着一堆等着写盘的 body 本身就说明磁盘跟不上，那时留着它们也没用。
pub const CHANNEL_CAP: usize = 64;

/// 交一份出去。**满了就丢，绝不等待。**
pub fn offer(tx: &Option<BodySender>, rec: BodyRecord) {
    let Some(tx) = tx else { return };
    if tx.try_send(rec).is_err() {
        // 不记日志：这条路上每个请求都会走一次，而写盘跟不上的时候
        // 日志会跟着刷屏 —— 那才是真的把事情变糟。
    }
}

/// 一边流一边攒响应体，**攒到上限就停**。
#[derive(Debug, Default)]
pub struct ResponseTap {
    buf: Vec<u8>,
    total: usize,
}

impl ResponseTap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        self.total += chunk.len();
        let room = RESPONSE_TAP_MAX.saturating_sub(self.buf.len());
        if room == 0 {
            return;
        }
        self.buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }

    /// 攒到的那部分，以及**原始的总长度**。
    pub fn finish(self) -> (Bytes, usize) {
        (Bytes::from(self.buf), self.total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_response_is_kept_whole() {
        let mut t = ResponseTap::new();
        t.feed(b"hello ");
        t.feed(b"world");
        let (b, total) = t.finish();
        assert_eq!(&b[..], b"hello world");
        assert_eq!(total, 11);
    }

    #[test]
    fn a_long_response_is_capped_but_reports_its_real_length() {
        // **截断了要能说出来** —— 不说的话，用户会以为响应本身就这么长。
        let mut t = ResponseTap::new();
        let chunk = vec![b'x'; 100 * 1024];
        for _ in 0..10 {
            t.feed(&chunk);
        }
        let (b, total) = t.finish();
        assert_eq!(b.len(), RESPONSE_TAP_MAX);
        assert_eq!(total, 10 * 100 * 1024, "原始长度没记住");
    }

    #[test]
    fn a_chunk_that_straddles_the_cap_is_partly_kept() {
        let mut t = ResponseTap::new();
        t.feed(&vec![b'a'; RESPONSE_TAP_MAX - 10]);
        t.feed(b"1234567890EXTRA");
        let (b, total) = t.finish();
        assert_eq!(b.len(), RESPONSE_TAP_MAX);
        assert_eq!(&b[b.len() - 10..], b"1234567890");
        assert_eq!(total, RESPONSE_TAP_MAX + 5);
    }

    #[tokio::test]
    async fn offering_into_a_full_channel_drops_rather_than_waits() {
        // **等它是在惩罚真实用户。**观测永远不该有让请求变慢的权力。
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let tx = Some(tx);
        let rec = || BodyRecord {
            id: 1,
            at_ms: 0,
            kind: BodyKind::Request,
            body: Bytes::from_static(b"x"),
            original_len: 1,
        };
        // 通道容量是 1，塞十次不该挂住
        let start = std::time::Instant::now();
        for _ in 0..10 {
            offer(&tx, rec());
        }
        assert!(
            start.elapsed() < std::time::Duration::from_millis(50),
            "offer 挂住了"
        );
    }

    #[tokio::test]
    async fn offering_when_there_is_no_sink_is_a_no_op() {
        // 观测层没起来时，转发照旧。
        offer(
            &None,
            BodyRecord {
                id: 1,
                at_ms: 0,
                kind: BodyKind::Request,
                body: Bytes::new(),
                original_len: 0,
            },
        );
    }
}
