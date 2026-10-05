//! 每家最近的成功率（`load-balance` 按成败分新对话时看它），端到端。
//!
//! 盯的是**哪些结果算这家的失败**：上游自己的问题算（5xx、限流、连不上、流在第一段
//! 内容之前报错），请求本身的问题不算、算它答上了，这家没有这个模型、客户端中途走了
//! 都不记。判据和熔断是同一套（`tw_gateway::health`），这里从真请求一路看过去。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::any;
use tw_config::{Client, Config, Failover, Provider};
use tw_gateway::Health;

mod common;

/// 每次都回同一个状态码、类型和正文的上游。
async fn answering(status: u16, content_type: &'static str, body: &'static str) -> SocketAddr {
    listen(Router::new().fallback(any(move || async move {
        axum::response::Response::builder()
            .status(status)
            .header("content-type", content_type)
            .body(axum::body::Body::from(body))
            .unwrap()
    })))
    .await
}

/// 一直不回响应头的上游。
async fn silent() -> SocketAddr {
    listen(Router::new().fallback(any(|| async {
        tokio::time::sleep(Duration::from_secs(60)).await;
        "late"
    })))
    .await
}

async fn listen(app: Router) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

fn provider(name: &str, at: SocketAddr) -> Provider {
    Provider {
        name: name.into(),
        base_url: format!("http://{at}"),
        key: Some("k".into()),
        ..Default::default()
    }
}

/// 起一个网关，交回它的地址和健康状态。**停用的门槛调高**：这里要数每一次的成败，
/// 不想让熔断把失败的那家从候选里拿掉
async fn gateway(providers: Vec<Provider>) -> (SocketAddr, Arc<Health>) {
    let cfg = Config {
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers,
        failover: Failover {
            failures_to_pause: 100,
            ..Default::default()
        },
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let health = state.health.clone();
    let gw = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    (gw, health)
}

/// 发 `n` 个请求，每个最多等 `wait`：等不到就放弃（连接随之关闭）
async fn send(gw: SocketAddr, body: &str, n: usize, wait: Duration) {
    for _ in 0..n {
        let r = tokio::time::timeout(
            wait,
            reqwest::Client::new()
                .post(format!("http://{gw}/v1/messages"))
                .header("x-api-key", "tw-k")
                .body(body.to_string())
                .send(),
        )
        .await;
        if let Ok(r) = r {
            // 把正文读完：流式回答的那一家要等到这时才算交完
            let _ = r.unwrap().bytes().await;
        }
    }
}

const PLAIN: &str = r#"{"model":"claude-sonnet-4-5"}"#;

fn rate(h: &Health, name: &str) -> Option<f64> {
    h.success_rates(&[name.to_string()]).get(name).copied()
}

/// 只有这一家时，回 `status` 五次之后它的成功率。
async fn after_five(status: u16, body: &'static str) -> Option<f64> {
    let up = answering(status, "application/json", body).await;
    let (gw, health) = gateway(vec![provider("only", up)]).await;
    send(gw, PLAIN, 5, Duration::from_secs(10)).await;
    rate(&health, "only")
}

#[tokio::test]
async fn an_upstream_that_answers_counts_as_a_success() {
    assert_eq!(after_five(200, r#"{"content":[]}"#).await, Some(1.0));
}

#[tokio::test]
async fn server_errors_and_rate_limits_count_against_the_upstream() {
    assert_eq!(after_five(503, "{}").await, Some(0.0));
    assert_eq!(after_five(429, "{}").await, Some(0.0));
    // 凭据被拒：换一家有意义，那边是另一把密钥
    assert_eq!(after_five(401, "{}").await, Some(0.0));
}

#[tokio::test]
async fn a_bad_request_is_the_requests_fault_and_counts_as_answered() {
    let got = after_five(
        400,
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: too large"}}"#,
    )
    .await;
    assert_eq!(got, Some(1.0));
}

#[tokio::test]
async fn a_missing_model_is_not_counted_either_way() {
    // 它对别的模型照样好好的
    let got = after_five(
        404,
        r#"{"type":"error","error":{"type":"not_found_error","message":"model not found"}}"#,
    )
    .await;
    assert_eq!(got, None);
}

#[tokio::test]
async fn an_upstream_that_cannot_be_reached_counts_against_it() {
    let nobody = SocketAddr::from(([127, 0, 0, 1], common::spare_port()));
    let (gw, health) = gateway(vec![provider("gone", nobody)]).await;
    send(gw, PLAIN, 5, Duration::from_secs(10)).await;
    assert_eq!(rate(&health, "gone"), Some(0.0));
}

#[tokio::test]
async fn a_client_that_walks_away_leaves_no_mark_on_the_upstream() {
    let (gw, health) = gateway(vec![provider("slow", silent().await)]).await;
    send(gw, PLAIN, 5, Duration::from_millis(300)).await;
    // 给网关一点时间：丢掉的 handler 要是还记了什么，这时也该记上了
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(rate(&health, "slow"), None);
}

#[tokio::test]
async fn a_stream_that_reports_an_error_before_any_content_counts_against_it() {
    // 第一家开了流、第一个事件就是过载：换到第二家。前者算失败，后者算答上了
    let overloaded = answering(
        200,
        "text/event-stream",
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\n\
         event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
    )
    .await;
    let good = answering(
        200,
        "text/event-stream",
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\n\
         event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"answer\"}}\n\n",
    )
    .await;
    let (gw, health) = gateway(vec![
        provider("first", overloaded),
        provider("second", good),
    ])
    .await;
    send(
        gw,
        r#"{"model":"claude-sonnet-4-5","stream":true}"#,
        5,
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(rate(&health, "first"), Some(0.0));
    assert_eq!(rate(&health, "second"), Some(1.0));
}
