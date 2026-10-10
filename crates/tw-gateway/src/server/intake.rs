//! 读请求体。**整个网关只有这一道大小上限。**
//!
//! 体在处理函数里、过完来源和密钥检查之后才读（见 `passthrough`），不交给 axum 的
//! `Bytes` 提取器：提取器在处理函数之前就把整个体读进内存，一个不在放行网段里、
//! 没带密钥的连接也能先塞满上限再被拒；它自带的 2 MB 默认上限还会把贴了几张截图的
//! 对话拒成 413，连压缩请求一起，会话就再也压不回来了（Core#295）。
//!
//! **压缩过的请求体在这里解开**（`Content-Encoding: gzip`、`deflate`、`zstd`）：读完、解完，
//! 后面的每一条路 —— 解析、转换、直通、Codex 后端的再压缩 —— 拿到的都是解开的字节，和客户端
//! 没压缩时一个样；`Content-Encoding` 头一并摘掉，上游不会看到它。上限按**解开之后**的大小算：
//! 一小段压缩数据能解出几百兆，声明的长度和压缩的长度都不作数。

use axum::body::Body;
use axum::http::{HeaderMap, header};
use bytes::Bytes;
use http_body_util::{BodyExt, LengthLimitError, Limited};

use crate::error::GatewayError;
use crate::inflate::{Coding, Decode};
use tw_types::msg;

/// 256 MiB。大到能装下几张 4K 图的 base64（膨胀 33%），小到失控的
/// 客户端打不爆内存。
pub(super) const MAX_BODY: usize = 256 * 1024 * 1024;

/// 读完请求体，最多 `max` 字节；压缩过的解开，解开的也最多 `max` 字节。
///
/// **`Content-Length` 只用来提前回绝**：声明的已经超了，不读就拒，老实的客户端不用先
/// 传完几百兆再挨一个 413。真正卡住的是读的时候按实际收到的字节计数 —— 声明的长度
/// 是对方自己写的，所以报错里只说「声明的」。
///
/// 解开之后 `Content-Encoding` 从 `headers` 里摘掉：从这里往下的请求就是一个没压缩的请求。
pub(super) async fn read(
    headers: &mut HeaderMap,
    body: Body,
    max: usize,
) -> Result<Bytes, GatewayError> {
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if let Some(size) = declared.filter(|&n| n > max as u64) {
        return Err(GatewayError::too_large(msg!(
            "gw.request.body_declared_too_large", size = size, max = max =>
            "The request declares a {size}-byte body, over the {max}-byte limit."
        )));
    }
    // 认不得的编码在读体之前就拒：不用先收完几百兆再说不认识
    let coding = coding_of(headers)?;
    let raw = match Limited::new(body, max).collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) if e.downcast_ref::<LengthLimitError>().is_some() => {
            return Err(GatewayError::too_large(msg!(
                "gw.request.body_over_limit", max = max =>
                "The request body is over the {max}-byte limit."
            )));
        }
        // 客户端传到一半断了。它多半已经听不到这句了，但这一行得有个结局
        Err(e) => {
            return Err(GatewayError::request(msg!(
                "gw.request.body_unreadable", detail = e =>
                "The request body could not be read: {detail}"
            )));
        }
    };
    let Some(coding) = coding else {
        return Ok(raw);
    };
    headers.remove(header::CONTENT_ENCODING);
    // 解压是纯 CPU 的活，几百兆要几百毫秒：不占 async 线程
    let decoded = tokio::task::spawn_blocking(move || coding.decode(&raw, max))
        .await
        .unwrap_or_else(|e| Err(Decode::Failed(e.to_string())));
    match decoded {
        Ok(b) => Ok(b),
        Err(Decode::OverLimit) => Err(GatewayError::too_large(msg!(
            "gw.request.decoded_body_over_limit", max = max =>
            "The request body is over the {max}-byte limit once decoded."
        ))),
        Err(Decode::Failed(detail)) => Err(GatewayError::request(msg!(
            "gw.request.bad_encoding", encoding = coding.name(), detail = detail =>
            "The request body could not be decoded as {encoding}: {detail}"
        ))),
    }
}

/// 这个请求的体压没压：没压是 `Ok(None)`，压成认不得的样子是 400。
///
/// **只认 gzip、deflate、zstd**（见 [`crate::inflate`]）；叠了几层的（`gzip, zstd`）也不认
fn coding_of(headers: &HeaderMap) -> Result<Option<Coding>, GatewayError> {
    let Some(v) = headers.get(header::CONTENT_ENCODING) else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(v.as_bytes()).trim().to_string();
    if text.is_empty() || text.eq_ignore_ascii_case("identity") {
        return Ok(None);
    }
    Coding::parse(&text).map(Some).ok_or_else(|| {
        GatewayError::request(msg!(
            "gw.request.unsupported_encoding", encoding = text =>
            "The request body is encoded as {encoding}; the gateway decodes only gzip, deflate and zstd."
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn chunked(parts: &[&'static [u8]]) -> Body {
        let parts: Vec<Result<Bytes, std::io::Error>> =
            parts.iter().map(|p| Ok(Bytes::from_static(p))).collect();
        Body::from_stream(futures::stream::iter(parts))
    }

    #[tokio::test]
    async fn a_body_at_the_limit_is_read_whole() {
        let b = read(&mut HeaderMap::new(), chunked(&[b"12", b"345"]), 5)
            .await
            .unwrap();
        assert_eq!(&b[..], b"12345");
    }

    #[tokio::test]
    async fn a_body_without_a_length_stops_at_the_limit() {
        let e = read(&mut HeaderMap::new(), chunked(&[b"123", b"456"]), 5)
            .await
            .unwrap_err();
        assert_eq!(e.detail.code, "gw.request.body_over_limit");
        assert_eq!(e.source, crate::error::Source::TooLarge);
    }

    #[tokio::test]
    async fn a_declared_length_over_the_limit_is_refused_unread() {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_LENGTH, HeaderValue::from_static("6"));
        // 体是一条永远不结束的流：读了就挂住，拒得掉说明根本没读
        let e = read(
            &mut h,
            Body::from_stream(futures::stream::pending::<Result<Bytes, std::io::Error>>()),
            5,
        )
        .await
        .unwrap_err();
        assert_eq!(e.detail.code, "gw.request.body_declared_too_large");
        assert_eq!(e.detail.args["size"], "6");
    }

    #[tokio::test]
    async fn a_declared_length_that_understates_the_body_does_not_lift_the_limit() {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_LENGTH, HeaderValue::from_static("1"));
        let e = read(&mut h, chunked(&[b"123", b"456"]), 5)
            .await
            .unwrap_err();
        assert_eq!(e.detail.code, "gw.request.body_over_limit");
    }

    fn gz(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn with_encoding(v: &'static str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static(v));
        h
    }

    #[tokio::test]
    async fn a_gzip_body_is_decoded_and_the_encoding_header_is_dropped() {
        let packed = gz(b"12345");
        let mut h = with_encoding("gzip");
        let b = read(&mut h, Body::from(packed), 1024).await.unwrap();
        assert_eq!(&b[..], b"12345");
        assert!(!h.contains_key(header::CONTENT_ENCODING));
    }

    #[tokio::test]
    async fn deflate_and_zstd_bodies_are_decoded() {
        use std::io::Write;
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(b"12345").unwrap();
        let b = read(
            &mut with_encoding("deflate"),
            Body::from(e.finish().unwrap()),
            1024,
        )
        .await
        .unwrap();
        assert_eq!(&b[..], b"12345");
        let z = zstd::stream::encode_all(&b"12345"[..], 3).unwrap();
        let b = read(&mut with_encoding("zstd"), Body::from(z), 1024)
            .await
            .unwrap();
        assert_eq!(&b[..], b"12345");
    }

    /// 一小段压缩数据解出一大片：按解开之后的算，和没压缩的超限一样是 413。压缩着的
    /// 那几十字节本身在上限之内，拦下它的只能是解开之后的长度
    #[tokio::test]
    async fn a_body_that_decodes_to_more_than_the_limit_is_too_large() {
        let packed = gz(&[b'x'; 10_000]);
        assert!(packed.len() < 100);
        let e = read(&mut with_encoding("gzip"), Body::from(packed), 100)
            .await
            .unwrap_err();
        assert_eq!(e.detail.code, "gw.request.decoded_body_over_limit");
        assert_eq!(e.source, crate::error::Source::TooLarge);
    }

    #[tokio::test]
    async fn a_body_that_is_not_what_its_encoding_says_is_a_bad_request() {
        let e = read(
            &mut with_encoding("gzip"),
            Body::from(&b"this is not gzip"[..]),
            1024,
        )
        .await
        .unwrap_err();
        assert_eq!(e.detail.code, "gw.request.bad_encoding");
        assert_eq!(e.detail.args["encoding"], "gzip");
        assert_eq!(e.source, crate::error::Source::Request);
        // 解到一半断了的也是
        let mut packed = gz(&[b'x'; 10_000]);
        packed.truncate(packed.len() / 2);
        let e = read(&mut with_encoding("gzip"), Body::from(packed), 1 << 20)
            .await
            .unwrap_err();
        assert_eq!(e.detail.code, "gw.request.bad_encoding");
    }

    /// 认不得的编码在读体之前就拒：体是一条永远不结束的流
    #[tokio::test]
    async fn an_unknown_encoding_is_refused_without_reading_the_body() {
        let e = read(
            &mut with_encoding("br"),
            Body::from_stream(futures::stream::pending::<Result<Bytes, std::io::Error>>()),
            5,
        )
        .await
        .unwrap_err();
        assert_eq!(e.detail.code, "gw.request.unsupported_encoding");
        assert_eq!(e.detail.args["encoding"], "br");
        // 叠了几层的也不认
        let e = read(&mut with_encoding("gzip, zstd"), Body::from(gz(b"1")), 5)
            .await
            .unwrap_err();
        assert_eq!(e.detail.code, "gw.request.unsupported_encoding");
    }

    #[tokio::test]
    async fn identity_means_not_encoded() {
        let b = read(&mut with_encoding("identity"), Body::from(&b"123"[..]), 5)
            .await
            .unwrap();
        assert_eq!(&b[..], b"123");
    }
}
