//! 读请求体。**整个网关只有这一道大小上限。**
//!
//! 体在处理函数里、过完来源和密钥检查之后才读（见 `passthrough`），不交给 axum 的
//! `Bytes` 提取器：提取器在处理函数之前就把整个体读进内存，一个不在放行网段里、
//! 没带密钥的连接也能先塞满上限再被拒；它自带的 2 MB 默认上限还会把贴了几张截图的
//! 对话拒成 413，连压缩请求一起，会话就再也压不回来了（Core#295）。

use axum::body::Body;
use axum::http::{HeaderMap, header};
use bytes::Bytes;
use http_body_util::{BodyExt, LengthLimitError, Limited};

use crate::error::GatewayError;
use tw_types::msg;

/// 256 MiB。大到能装下几张 4K 图的 base64（膨胀 33%），小到失控的
/// 客户端打不爆内存。
pub(super) const MAX_BODY: usize = 256 * 1024 * 1024;

/// 读完请求体，最多 `max` 字节。
///
/// **`Content-Length` 只用来提前回绝**：声明的已经超了，不读就拒，老实的客户端不用先
/// 传完几百兆再挨一个 413。真正卡住的是读的时候按实际收到的字节计数 —— 声明的长度
/// 是对方自己写的，所以报错里只说「声明的」。
pub(super) async fn read(
    headers: &HeaderMap,
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
    match Limited::new(body, max).collect().await {
        Ok(c) => Ok(c.to_bytes()),
        Err(e) if e.downcast_ref::<LengthLimitError>().is_some() => {
            Err(GatewayError::too_large(msg!(
                "gw.request.body_over_limit", max = max =>
                "The request body is over the {max}-byte limit."
            )))
        }
        // 客户端传到一半断了。它多半已经听不到这句了，但这一行得有个结局
        Err(e) => Err(GatewayError::request(msg!(
            "gw.request.body_unreadable", detail = e =>
            "The request body could not be read: {detail}"
        ))),
    }
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
        let b = read(&HeaderMap::new(), chunked(&[b"12", b"345"]), 5)
            .await
            .unwrap();
        assert_eq!(&b[..], b"12345");
    }

    #[tokio::test]
    async fn a_body_without_a_length_stops_at_the_limit() {
        let e = read(&HeaderMap::new(), chunked(&[b"123", b"456"]), 5)
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
            &h,
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
        let e = read(&h, chunked(&[b"123", b"456"]), 5).await.unwrap_err();
        assert_eq!(e.detail.code, "gw.request.body_over_limit");
    }
}
