//! 传输编码：压缩过的请求体和上游回答在这里解开。
//!
//! 两头各一处。**进来的**请求体由 `server::intake` 读完整个再解（[`Coding::decode`]，解出来的
//! 有上限）；**收回来的**上游回答边收边解（[`inflated`]），流式回答一块一块地解，SSE 照样一条
//! 一条地到。
//!
//! 只认 gzip、deflate、zstd：HTTP 里常见的就是这三种，而 gzip、zstd 的解码库本来就在依赖树里
//! （价目表、Codex 后端的请求体）。`br` 要另引一个库，没见哪家上游在没问过 `Accept-Encoding`
//! 时主动发它，不认：带着它的回答原样交给客户端，和从前一样。
//!
//! **上游回答的解压不交给 reqwest**（它的 gzip、zstd 特性都没开）：流量按线上的字节数
//! （[`crate::traffic`]），计数套在解压之前，开了自动解压就数不到压缩着的那些了。网关也不发
//! `Accept-Encoding`：没问就压缩着回的上游不多，问了反而要为每一次 SSE 多解一层。

use std::io::{self, Read, Write};

use bytes::Bytes;
use futures::StreamExt;
use http_body_util::BodyExt;

/// 一个 `Content-Encoding` 值说的是哪种。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coding {
    Gzip,
    Deflate,
    Zstd,
}

/// 整个解开时出的事
pub enum Decode {
    /// 解出来的超过了上限
    OverLimit,
    /// 数据和编码对不上、解到一半断了
    Failed(String),
}

impl Coding {
    /// `identity` 和没写一样是 `None`；大小写不论；`x-gzip` 是 gzip 的旧名。叠了几层的
    /// （`gzip, zstd`）和不认识的一律是 `None`，由调用的那一头决定拒还是原样
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "gzip" | "x-gzip" => Some(Self::Gzip),
            "deflate" => Some(Self::Deflate),
            "zstd" => Some(Self::Zstd),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Deflate => "deflate",
            Self::Zstd => "zstd",
        }
    }

    /// 解开 `raw`，解出来的最多 `max` 字节。**读到第 `max + 1` 个字节就停**：解出来的有多大，
    /// 压缩数据自己说了不算（zstd 的帧头可以写一个假的长度），只信实际解出来的
    pub fn decode(self, raw: &[u8], max: usize) -> Result<Bytes, Decode> {
        let reader: Box<dyn Read> = match self {
            Self::Gzip => Box::new(flate2::read::MultiGzDecoder::new(raw)),
            // HTTP 的 deflate 是 zlib 封装的（RFC 9110 §8.4.1.2）
            Self::Deflate => Box::new(flate2::read::ZlibDecoder::new(raw)),
            Self::Zstd => Box::new(
                zstd::stream::read::Decoder::new(raw).map_err(|e| Decode::Failed(e.to_string()))?,
            ),
        };
        let mut out = Vec::new();
        reader
            .take(max as u64 + 1)
            .read_to_end(&mut out)
            .map_err(|e| Decode::Failed(e.to_string()))?;
        if out.len() > max {
            return Err(Decode::OverLimit);
        }
        Ok(Bytes::from(out))
    }
}

/// 边收边解：喂一块压缩数据，吐出此刻解得出的那些
enum Unpacker {
    Gzip(flate2::write::MultiGzDecoder<Vec<u8>>),
    Deflate(flate2::write::ZlibDecoder<Vec<u8>>),
    Zstd(zstd::stream::write::Decoder<'static, Vec<u8>>),
}

impl Unpacker {
    fn new(coding: Coding) -> io::Result<Self> {
        Ok(match coding {
            Coding::Gzip => Self::Gzip(flate2::write::MultiGzDecoder::new(Vec::new())),
            Coding::Deflate => Self::Deflate(flate2::write::ZlibDecoder::new(Vec::new())),
            Coding::Zstd => Self::Zstd(zstd::stream::write::Decoder::new(Vec::new())?),
        })
    }

    /// 喂一块。**喂完就 flush**：SSE 的一条事件压缩后多半不满一个压缩块，不 flush 的话它会
    /// 留在解码器里等下一块，客户端看到的就晚了一条
    fn feed(&mut self, chunk: &[u8]) -> io::Result<Bytes> {
        match self {
            Self::Gzip(d) => {
                d.write_all(chunk)?;
                d.flush()?;
                Ok(Bytes::from(std::mem::take(d.get_mut())))
            }
            Self::Deflate(d) => {
                d.write_all(chunk)?;
                d.flush()?;
                Ok(Bytes::from(std::mem::take(d.get_mut())))
            }
            Self::Zstd(d) => {
                d.write_all(chunk)?;
                d.flush()?;
                Ok(Bytes::from(std::mem::take(d.get_mut())))
            }
        }
    }

    /// 收完了：把最后那点吐出来。gzip 在这里核对尾部的校验和
    fn finish(self) -> io::Result<Bytes> {
        Ok(Bytes::from(match self {
            Self::Gzip(d) => d.finish()?,
            Self::Deflate(d) => d.finish()?,
            Self::Zstd(mut d) => {
                d.flush()?;
                d.into_inner()
            }
        }))
    }
}

/// 上游的回答压缩着（`Content-Encoding` 是认得的那三种之一）就套一层解码：读它的人拿到的是
/// 解开的字节，`Content-Encoding` 摘掉，`Content-Length`（压缩着的长度）也摘掉。没压缩的、
/// 认不得的原样返回。
///
/// **套在 [`crate::traffic::metered`] 之后**：流量数的是线上的字节。解到一半对不上，流就在那里
/// 断掉，读的那一头当成上游断了
pub fn inflated(r: reqwest::Response) -> reqwest::Response {
    let coding = r
        .headers()
        .get(http::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .and_then(Coding::parse);
    let Some(coding) = coding else {
        return r;
    };
    let r: http::Response<reqwest::Body> = r.into();
    let (mut parts, body) = r.into_parts();
    parts.headers.remove(http::header::CONTENT_ENCODING);
    parts.headers.remove(http::header::CONTENT_LENGTH);
    let body = reqwest::Body::wrap_stream(unpacked(coding, body.into_data_stream()));
    reqwest::Response::from(http::Response::from_parts(parts, body))
}

type Chunk = Result<Bytes, Box<dyn std::error::Error + Send + Sync>>;

/// 压缩着的字节流解成解开的字节流。空块不吐：读 SSE 的那一头把一个空块当成什么都没来
fn unpacked(
    coding: Coding,
    mut packed: impl futures::Stream<Item = reqwest::Result<Bytes>> + Unpin + Send + 'static,
) -> impl futures::Stream<Item = Chunk> + Send + 'static {
    async_stream::try_stream! {
        let mut unpacker = Unpacker::new(coding)?;
        while let Some(chunk) = packed.next().await {
            let out = unpacker.feed(&chunk?)?;
            if !out.is_empty() {
                yield out;
            }
        }
        let rest = unpacker.finish()?;
        if !rest.is_empty() {
            yield rest;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gz_frames(parts: &[&[u8]]) -> Vec<Vec<u8>> {
        // 一个 gzip 流分几段发：每段 flush 一次，像上游边生成边压缩那样
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut frames = Vec::new();
        for p in parts {
            e.write_all(p).unwrap();
            e.flush().unwrap();
            frames.push(std::mem::take(e.get_mut()));
        }
        frames.push(e.finish().unwrap());
        frames
    }

    fn response(encoding: &'static str, frames: Vec<Vec<u8>>) -> reqwest::Response {
        let stream = futures::stream::iter(
            frames
                .into_iter()
                .map(|f| Ok::<_, std::io::Error>(Bytes::from(f))),
        );
        let r = http::Response::builder()
            .header("content-encoding", encoding)
            .header("content-length", "999")
            .header("content-type", "text/event-stream")
            .body(reqwest::Body::wrap_stream(stream))
            .unwrap();
        reqwest::Response::from(r)
    }

    /// 每一段压缩数据到了，它里面的那条事件就解得出来，不等整个流结束
    #[tokio::test]
    async fn each_frame_comes_out_as_it_arrives() {
        let frames = gz_frames(&[b"event: a\n\n", b"event: b\n\n"]);
        let r = inflated(response("gzip", frames));
        assert!(r.headers().get("content-encoding").is_none());
        assert!(r.headers().get("content-length").is_none());
        assert_eq!(
            r.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
        let mut s = r.bytes_stream();
        assert_eq!(&s.next().await.unwrap().unwrap()[..], b"event: a\n\n");
        assert_eq!(&s.next().await.unwrap().unwrap()[..], b"event: b\n\n");
        assert!(s.next().await.is_none());
    }

    #[tokio::test]
    async fn zstd_and_deflate_are_decoded_whole() {
        let z = zstd::stream::encode_all(&b"{\"ok\":true}"[..], 3).unwrap();
        let got = inflated(response("zstd", vec![z])).bytes().await.unwrap();
        assert_eq!(&got[..], b"{\"ok\":true}");
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(b"{\"ok\":true}").unwrap();
        let got = inflated(response("deflate", vec![e.finish().unwrap()]))
            .bytes()
            .await
            .unwrap();
        assert_eq!(&got[..], b"{\"ok\":true}");
    }

    /// 认不得的编码原样交出去，头也留着
    #[tokio::test]
    async fn an_unknown_encoding_is_left_alone() {
        let r = inflated(response("br", vec![b"xyz".to_vec()]));
        assert_eq!(r.headers().get("content-encoding").unwrap(), "br");
        assert_eq!(&r.bytes().await.unwrap()[..], b"xyz");
    }

    #[tokio::test]
    async fn a_corrupt_stream_errors_instead_of_handing_over_garbage() {
        let r = inflated(response("gzip", vec![b"not gzip at all".to_vec()]));
        assert!(r.bytes().await.is_err());
    }
}
