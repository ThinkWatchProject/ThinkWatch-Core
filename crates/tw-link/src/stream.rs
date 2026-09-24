//! 握手之后的那条流：`[u16 长度][密文]` 一帧接一帧。

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::{MAX_FRAME, MAX_PLAINTEXT, TAG_LEN, invalid};

/// 加密的流。读写都是明文，线上是一帧一帧的密文。
///
/// **写是缓冲一帧的**：`poll_write` 把收下的那一段加密成一帧放进缓冲，
/// 立刻报「收下了」，下一次写、`flush`、`shutdown` 之前先把它送完。所以
/// 用它的一方要照常 `flush` —— hyper 会。
///
/// 每个方向一个计数器当 nonce（snow 管），2^64 帧用不完，不需要换钥。
pub struct SecureStream<S> {
    inner: S,
    noise: snow::TransportState,
    /// 从底层读上来、还没拆成帧的字节：`raw[start..end]`
    raw: Vec<u8>,
    start: usize,
    end: usize,
    /// 解密好、还没交给调用方的明文：`plain[pos..]`
    plain: Vec<u8>,
    pos: usize,
    /// 对面在帧边界上关了写的一侧
    eof: bool,
    /// 加密好、还没送出去的一帧：`out[sent..]`
    out: Vec<u8>,
    sent: usize,
}

impl<S> SecureStream<S> {
    pub(crate) fn new(inner: S, noise: snow::TransportState) -> Self {
        Self {
            inner,
            noise,
            // 两帧的空间：一帧没读完时后面那一帧的开头也放得下，挪一次就够
            raw: vec![0u8; 2 * (2 + MAX_FRAME)],
            start: 0,
            end: 0,
            plain: Vec::with_capacity(MAX_FRAME),
            pos: 0,
            eof: false,
            out: Vec::with_capacity(2 + MAX_FRAME),
            sent: 0,
        }
    }

    /// 底层的流。**只读**：绕过加密往里写会把帧打乱。
    pub fn get_ref(&self) -> &S {
        &self.inner
    }

    #[cfg(test)]
    pub(crate) fn into_inner_for_tests(self) -> S {
        self.inner
    }
}

impl<S> std::fmt::Debug for SecureStream<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecureStream").finish_non_exhaustive()
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> SecureStream<S> {
    /// 缓冲里已经有一整帧的话，解出来放进 `plain`。
    fn take_frame(&mut self) -> io::Result<bool> {
        let have = self.end - self.start;
        if have < 2 {
            return Ok(false);
        }
        let len = u16::from_be_bytes([self.raw[self.start], self.raw[self.start + 1]]) as usize;
        if len < TAG_LEN {
            return Err(invalid(format!(
                "a {len}-byte frame is too short to carry a tag"
            )));
        }
        if have < 2 + len {
            return Ok(false);
        }
        let body = self.start + 2..self.start + 2 + len;
        self.plain.resize(len, 0);
        let n = self
            .noise
            .read_message(&self.raw[body], &mut self.plain)
            .map_err(|e| invalid(format!("a frame did not decrypt: {e}")))?;
        self.plain.truncate(n);
        self.pos = 0;
        self.start += 2 + len;
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        }
        Ok(true)
    }

    /// 把缓冲里那一帧送完。
    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.sent < self.out.len() {
            let n = ready!(Pin::new(&mut self.inner).poll_write(cx, &self.out[self.sent..]))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.sent += n;
        }
        self.out.clear();
        self.sent = 0;
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for SecureStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.pos < this.plain.len() {
                let n = (this.plain.len() - this.pos).min(buf.remaining());
                buf.put_slice(&this.plain[this.pos..this.pos + n]);
                this.pos += n;
                return Poll::Ready(Ok(()));
            }
            // 空帧（零字节的明文）也是合法的一帧，解完接着读下一帧
            if this.take_frame()? {
                continue;
            }
            if this.eof {
                return Poll::Ready(Ok(()));
            }
            // 半帧挪到开头，腾出后面的空间
            if this.start > 0 && this.raw.len() - this.end < 2 + MAX_FRAME {
                this.raw.copy_within(this.start..this.end, 0);
                this.end -= this.start;
                this.start = 0;
            }
            let mut rb = ReadBuf::new(&mut this.raw[this.end..]);
            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut rb))?;
            let n = rb.filled().len();
            if n == 0 {
                if this.end > this.start {
                    // 帧读到一半对面就关了：不是正常结束
                    return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
                }
                this.eof = true;
                return Poll::Ready(Ok(()));
            }
            this.end += n;
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for SecureStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        // 上一帧没送完就不收新的：否则缓冲会无限长
        ready!(this.poll_drain(cx))?;
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let n = buf.len().min(MAX_PLAINTEXT);
        this.out.resize(2 + n + TAG_LEN, 0);
        let len = this
            .noise
            .write_message(&buf[..n], &mut this.out[2..])
            .map_err(|e| io::Error::other(format!("a frame did not encrypt: {e}")))?;
        this.out.truncate(2 + len);
        let len = u16::try_from(len).expect("a frame is at most 65535 bytes");
        this.out[..2].copy_from_slice(&len.to_be_bytes());
        this.sent = 0;
        // 顺手送一下。送不完也已经收下了：下一次写或 flush 会接着送
        if let Poll::Ready(Err(e)) = this.poll_drain(cx) {
            return Poll::Ready(Err(e));
        }
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}
