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

use axum::body::{Body, HttpBody};
use axum::http::{HeaderMap, header};
use bytes::{Bytes, BytesMut};
use http_body_util::BodyExt;

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
    read_with_budget(headers, body, max, crate::resources::body_budget()).await
}

async fn read_with_budget(
    headers: &mut HeaderMap,
    mut body: Body,
    max: usize,
    budget: &std::sync::Arc<crate::resources::Budget>,
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
    // 已知上界的未压缩正文先留足预算，再并行读取。看 body 自己的上界，不信
    // 客户端随手写的 Content-Length；HTTP 传输层负责兑现它的 size_hint。
    let bounded = coding.is_none().then(|| body.size_hint().upper()).flatten();
    let mut admission = Some(budget.admit().await?);
    let mut reservation = budget.reserve();
    if let Some(size) = bounded {
        reservation.wait(size.min(max as u64) as usize).await?;
        drop(admission.take());
    }
    let mut raw = BytesMut::new();
    while let Some(frame) = tokio::time::timeout(crate::resources::BODY_WAIT, body.frame())
        .await
        .map_err(|_| {
            GatewayError::request(msg!(
                "gw.request.body_idle" =>
                "The request body stopped arriving for 30 seconds. Send the request again."
            ))
        })?
    {
        let frame = frame.map_err(|e| {
            GatewayError::request(msg!(
                "gw.request.body_unreadable", detail = e =>
                "The request body could not be read: {detail}"
            ))
        })?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        let next = raw.len().saturating_add(data.len());
        if next > max {
            return Err(GatewayError::too_large(msg!(
                "gw.request.body_over_limit", max = max =>
                "The request body is over the {max}-byte limit."
            )));
        }
        // 只有一个未知长度的读取者会拿着半份正文等预算；其他未完成读取的请求
        // 都已经按可信上界留足了空间，完成它们不需要再等预算。
        if admission.is_some() {
            reservation.wait(next).await?;
        } else if !reservation.grow(next) {
            // 传输层违反 size_hint 上界时仍不绕过预算，也不拿着半份正文排队。
            return Err(crate::resources::memory_busy());
        }
        raw.extend_from_slice(&data);
    }
    let raw = raw.freeze();
    let Some(coding) = coding else {
        reservation.shrink(raw.len());
        return Ok(reservation.wrap(raw));
    };
    headers.remove(header::CONTENT_ENCODING);
    let decoded = decode(coding, raw, max, reservation).await;
    match decoded {
        Ok(b) => Ok(b),
        Err(Decode::Busy) => Err(crate::resources::memory_busy()),
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

async fn decode(
    coding: Coding,
    mut raw: Bytes,
    max: usize,
    mut reservation: crate::resources::Reservation,
) -> Result<Bytes, Decode> {
    let mut retried = false;
    loop {
        // 解压不占 async 线程。取消时阻塞任务仍持有自己的 CPU 名额和预算。
        let slot = crate::resources::heavy_slot().await;
        let (input, held, decoded) = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            let decoded = coding.decode_with(&raw, max, |output| {
                if reservation.grow(raw.len().saturating_add(output)) {
                    Ok(())
                } else {
                    Err(Decode::Busy)
                }
            });
            (raw, reservation, decoded)
        })
        .await
        .map_err(|e| Decode::Failed(e.to_string()))?;
        raw = input;
        reservation = held;
        match decoded {
            Ok(decoded) => {
                // 原始压缩字节用完了，不让它的预留跟随解开的正文继续占着。
                drop(raw);
                reservation.shrink(decoded.len());
                return Ok(reservation.wrap(decoded));
            }
            Err(Decode::Busy) if !retried => {
                // 失败的输出已释放。拿着 intake 名额等，后来的小正文不会抢走空间。
                // 不持 CPU 名额等内存，其他请求还需要 CPU 才能结束并归还预算。
                reservation.shrink(raw.len());
                reservation.exclusive().await.map_err(|_| Decode::Busy)?;
                // 独占后所有 grow 都能成功，因此至多重新解压一次。
                retried = true;
            }
            Err(e) => return Err(e),
        }
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

    #[tokio::test(start_paused = true)]
    async fn concurrent_bodies_share_a_budget_until_the_last_clone_is_dropped() {
        let budget = crate::resources::Budget::new(30);
        let body = read_with_budget(&mut HeaderMap::new(), chunked(&[b"123456"]), 100, &budget)
            .await
            .unwrap();
        let clone = body.clone();
        drop(body);
        let e = read_with_budget(&mut HeaderMap::new(), chunked(&[b"12345"]), 100, &budget)
            .await
            .unwrap_err();
        assert_eq!(e.detail.code, "gw.request.memory_busy");
        assert_eq!(e.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
        assert!(e.retry.is_some());
        drop(clone);
        assert!(
            read_with_budget(&mut HeaderMap::new(), chunked(&[b"12345"]), 100, &budget)
                .await
                .is_ok()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_a_partial_read_returns_its_budget() {
        use futures::StreamExt;
        let budget = crate::resources::Budget::new(30);
        let stream =
            futures::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(b"123456"))])
                .chain(futures::stream::pending());
        let mut headers = HeaderMap::new();
        let mut reading = Box::pin(read_with_budget(
            &mut headers,
            Body::from_stream(stream),
            100,
            &budget,
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut reading)
                .await
                .is_err()
        );
        assert!(
            read_with_budget(&mut HeaderMap::new(), chunked(&[b"12345"]), 100, &budget)
                .await
                .is_err()
        );
        drop(reading);
        assert!(
            read_with_budget(&mut HeaderMap::new(), chunked(&[b"12345"]), 100, &budget)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn an_oversized_body_releases_its_partial_reservation() {
        let budget = crate::resources::Budget::new(30);
        let e = read_with_budget(
            &mut HeaderMap::new(),
            chunked(&[b"123456", b"789"]),
            8,
            &budget,
        )
        .await
        .unwrap_err();
        assert_eq!(e.detail.code, "gw.request.body_over_limit");
        assert!(
            read_with_budget(
                &mut HeaderMap::new(),
                chunked(&[b"123456789"]),
                100,
                &budget
            )
            .await
            .is_ok()
        );
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

    #[tokio::test(start_paused = true)]
    async fn decompression_reserves_output_and_releases_everything_on_failure() {
        let budget = crate::resources::Budget::new(3_000);
        let held = read_with_budget(
            &mut HeaderMap::new(),
            Body::from(vec![b'h'; 500]),
            10_000,
            &budget,
        )
        .await
        .unwrap();
        // 压缩数据放得下，解压后的那份放不下：必须在解压增长时检查预算。
        let e = read_with_budget(
            &mut with_encoding("gzip"),
            Body::from(gz(&[b'x'; 1_000])),
            10_000,
            &budget,
        )
        .await
        .unwrap_err();
        assert_eq!(e.detail.code, "gw.request.memory_busy");
        drop(held);
        let decoded = read_with_budget(
            &mut with_encoding("gzip"),
            Body::from(gz(&[b'x'; 1_000])),
            10_000,
            &budget,
        )
        .await
        .unwrap();
        assert_eq!(&decoded[..], &[b'x'; 1_000]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_large_waiter_gets_budget_before_later_small_bodies() {
        let budget = crate::resources::Budget::new(30);
        let held = read_with_budget(&mut HeaderMap::new(), Body::from("123456"), 100, &budget)
            .await
            .unwrap();
        let mut large_headers = HeaderMap::new();
        let mut large = Box::pin(read_with_budget(
            &mut large_headers,
            Body::from("123456789"),
            100,
            &budget,
        ));
        assert!(futures::poll!(&mut large).is_pending());
        let mut small_headers = HeaderMap::new();
        let mut small = Box::pin(read_with_budget(
            &mut small_headers,
            Body::from("1234"),
            100,
            &budget,
        ));
        // 旧实现会让这份小正文直接进入，大正文的每次重试都可能被抢先。
        assert!(futures::poll!(&mut small).is_pending());
        drop(held);
        let large = large.await.unwrap();
        assert_eq!(&large[..], b"123456789");
        assert!(futures::poll!(&mut small).is_pending());
        drop(large);
        assert_eq!(&small.await.unwrap()[..], b"1234");
    }

    #[tokio::test(start_paused = true)]
    async fn a_chunked_body_can_finish_growing_without_partial_read_deadlock() {
        let budget = crate::resources::Budget::new(30);
        let held = read_with_budget(&mut HeaderMap::new(), Body::from("123456"), 100, &budget)
            .await
            .unwrap();
        let mut first_headers = HeaderMap::new();
        let mut first = Box::pin(read_with_budget(
            &mut first_headers,
            chunked(&[b"123", b"456789"]),
            100,
            &budget,
        ));
        assert!(futures::poll!(&mut first).is_pending());
        let mut second_headers = HeaderMap::new();
        let mut second = Box::pin(read_with_budget(
            &mut second_headers,
            chunked(&[b"123", b"456789"]),
            100,
            &budget,
        ));
        assert!(futures::poll!(&mut second).is_pending());
        drop(held);
        let first = first.await.unwrap();
        assert!(futures::poll!(&mut second).is_pending());
        drop(first);
        assert_eq!(&second.await.unwrap()[..], b"123456789");
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_the_head_waiter_returns_its_partial_body_and_queue_place() {
        let budget = crate::resources::Budget::new(30);
        let held = read_with_budget(&mut HeaderMap::new(), Body::from("123456"), 100, &budget)
            .await
            .unwrap();
        let mut large_headers = HeaderMap::new();
        let mut large = Box::pin(read_with_budget(
            &mut large_headers,
            chunked(&[b"123", b"456789"]),
            100,
            &budget,
        ));
        assert!(futures::poll!(&mut large).is_pending());
        let mut small_headers = HeaderMap::new();
        let mut small = Box::pin(read_with_budget(
            &mut small_headers,
            Body::from("1234"),
            100,
            &budget,
        ));
        assert!(futures::poll!(&mut small).is_pending());
        drop(large);
        // 原来的竞争者还活着；只有取消者自己的预算和排队名额被归还。
        assert_eq!(&small.await.unwrap()[..], b"1234");
        drop(held);
    }

    #[tokio::test(start_paused = true)]
    async fn permanently_occupied_budget_has_a_bounded_wait_and_recovers() {
        let budget = crate::resources::Budget::new(30);
        let held = read_with_budget(&mut HeaderMap::new(), Body::from("123456"), 100, &budget)
            .await
            .unwrap();
        let started = tokio::time::Instant::now();
        let e = read_with_budget(&mut HeaderMap::new(), Body::from("123456789"), 100, &budget)
            .await
            .unwrap_err();
        assert_eq!(started.elapsed(), crate::resources::BODY_WAIT);
        assert_eq!(e.detail.code, "gw.request.memory_busy");
        assert_eq!(e.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(e.retry.unwrap().after_ms, 5_000);
        drop(held);
        assert!(
            read_with_budget(&mut HeaderMap::new(), Body::from("123456789"), 100, &budget,)
                .await
                .is_ok()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_upload_releases_its_budget_and_admission_place() {
        use futures::StreamExt;
        let budget = crate::resources::Budget::new(30);
        let stream =
            futures::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(b"123456"))])
                .chain(futures::stream::pending());
        let e = read_with_budget(
            &mut HeaderMap::new(),
            Body::from_stream(stream),
            100,
            &budget,
        )
        .await
        .unwrap_err();
        assert_eq!(e.detail.code, "gw.request.body_idle");
        assert!(
            read_with_budget(&mut HeaderMap::new(), Body::from("123456789"), 100, &budget,)
                .await
                .is_ok()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn decoded_bodies_return_the_compressed_input_headroom() {
        let budget = crate::resources::Budget::new(3_003);
        let decoded = read_with_budget(
            &mut with_encoding("gzip"),
            Body::from(gz(&[b'x'; 1_000])),
            10_000,
            &budget,
        )
        .await
        .unwrap();
        assert_eq!(decoded.len(), 1_000);
        // 解开的正文占 3,000；已经丢掉的压缩字节不能继续占着剩余 3 字节。
        let next = read_with_budget(&mut HeaderMap::new(), Body::from("x"), 10_000, &budget)
            .await
            .unwrap();
        assert_eq!(&next[..], b"x");
        drop((decoded, next));
    }

    #[tokio::test]
    async fn http_large_requests_wait_and_recover_before_later_small_requests() {
        use axum::{Router, extract::State, routing::post};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::sync::mpsc;
        type HttpState = (
            std::sync::Arc<crate::resources::Budget>,
            mpsc::UnboundedSender<String>,
            mpsc::UnboundedSender<(String, Bytes)>,
        );
        async fn receive(
            State((budget, entered, finished)): State<HttpState>,
            mut headers: HeaderMap,
            body: Body,
        ) -> Result<(), GatewayError> {
            let label = headers["x-test-request"].to_str().unwrap().to_owned();
            entered.send(label.clone()).unwrap();
            let body = read_with_budget(&mut headers, body, 10_000, &budget).await?;
            // 模拟下游仍持有正文。测试决定何时归还，不依靠请求之间的延时碰运气。
            finished.send((label, body)).unwrap();
            Ok(())
        }
        async fn event<T>(receiver: &mut mpsc::UnboundedReceiver<T>) -> T {
            tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
                .await
                .unwrap()
                .unwrap()
        }
        let budget = crate::resources::Budget::new(3_000);
        let json_body = |text: String| {
            serde_json::json!({
                "model": "gpt-6.1-sol",
                "input": [{"role": "user", "content": text}]
            })
            .to_string()
        };
        let held = read_with_budget(
            &mut HeaderMap::new(),
            Body::from(json_body("h".repeat(530))),
            10_000,
            &budget,
        )
        .await
        .unwrap();
        let (entered, mut arrivals) = mpsc::unbounded_channel();
        let (finished, mut bodies) = mpsc::unbounded_channel();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let url = format!("http://{address}/v1/responses");
        let router = Router::new()
            .route("/v1/responses", post(receive))
            .with_state((budget, entered, finished));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();
        let large = client
            .post(&url)
            .header("x-test-request", "large")
            .header("content-type", "application/json")
            .body(json_body("x".repeat(900)))
            .send();
        let large = tokio::spawn(large);
        assert_eq!(event(&mut arrivals).await, "large");
        let small = client
            .post(&url)
            .header("x-test-request", "small")
            .body(json_body("hello".to_owned()))
            .send();
        let small = tokio::spawn(small);
        assert_eq!(event(&mut arrivals).await, "small");
        drop(held);
        let (label, body) = event(&mut bodies).await;
        assert_eq!(label, "large");
        assert_eq!(large.await.unwrap().unwrap().status(), 200);
        assert!(!small.is_finished());
        drop(body);
        let (label, body) = event(&mut bodies).await;
        assert_eq!(label, "small");
        drop(body);
        assert_eq!(small.await.unwrap().unwrap().status(), 200);

        // 已知长度的上传只发了一半时，其他放得下的正文仍能并行进入。
        let partial = json_body("p".repeat(530));
        let mut upload = tokio::net::TcpStream::connect(address).await.unwrap();
        let headers = format!(
            "POST /v1/responses HTTP/1.1\r\nHost: {address}\r\n\
             Content-Length: {}\r\nx-test-request: partial\r\n\
             Connection: close\r\n\r\n",
            partial.len()
        );
        upload.write_all(headers.as_bytes()).await.unwrap();
        let halfway = partial.len() / 2;
        upload
            .write_all(&partial.as_bytes()[..halfway])
            .await
            .unwrap();
        assert_eq!(event(&mut arrivals).await, "partial");
        let parallel = client
            .post(&url)
            .header("x-test-request", "parallel")
            .body(json_body("hello".to_owned()))
            .send();
        let parallel = tokio::spawn(parallel);
        assert_eq!(event(&mut arrivals).await, "parallel");
        let (label, body) = event(&mut bodies).await;
        assert_eq!(label, "parallel");
        assert_eq!(parallel.await.unwrap().unwrap().status(), 200);
        drop(body);
        upload
            .write_all(&partial.as_bytes()[halfway..])
            .await
            .unwrap();
        let (label, body) = event(&mut bodies).await;
        assert_eq!(label, "partial");
        drop(body);
        let mut response = Vec::new();
        upload.read_to_end(&mut response).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200"));
        server.abort();
        let _ = server.await;
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
