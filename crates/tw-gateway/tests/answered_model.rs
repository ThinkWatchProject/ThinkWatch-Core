//! 上游体检要的两样事实，**从数据面一路走到事件里**：
//!
//! - 开始事件带着本地估的输入 token 数（发给上游的那一份）；
//! - 结局带着上游在回答里写的模型名 —— 认的是上游的原话，格式转换之前的那一份，
//!   不是转给客户端时网关写上去的。
//!
//! 起真网关、真上游：嗅探器认得出模型名在单元测试里验过了，这里验的是它接在了
//! 真正经过的那条路上（直通、转换、流式），以及解不开的请求不硬估。

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::routing::any;
use tokio::sync::broadcast::Receiver;
use tw_api::Event;
use tw_config::{Client, Config, Protocol, Provider};

/// 一个只会回同一段话的上游
async fn upstream(body: &'static str, content_type: &'static str) -> SocketAddr {
    let app = Router::new().fallback(any(move || async move {
        axum::response::Response::builder()
            .header("content-type", content_type)
            .body(axum::body::Body::from(body))
            .unwrap()
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

fn cfg(upstream: SocketAddr, protocol: Protocol) -> Config {
    Config {
        clients: vec![Client {
            name: "me".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "up".into(),
            base_url: format!("http://{upstream}"),
            key: Some("sk-x".into()),
            protocol: Some(protocol),
            ..Default::default()
        }],
        ..Default::default()
    }
}

async fn serve(cfg: Config) -> (SocketAddr, Receiver<Event>) {
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let events = state.bus.subscribe();
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, events)
}

async fn ask(gw: SocketAddr, path: &str, body: &str) -> String {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{gw}{path}"))
        .header("x-api-key", "tw-k")
        .header("authorization", "Bearer tw-k")
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

/// 一个请求从开始到结局：（开始事件里的估算，结局里的模型名）
async fn facts(rx: &mut Receiver<Event>) -> (Option<u64>, Option<String>) {
    let mut estimate = None;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("5 秒内没等到结局")
            .expect("事件流断了");
        match ev {
            Event::RequestStarted { input_estimate, .. } => estimate = input_estimate,
            Event::RequestFinished { answered_model, .. }
            | Event::RequestFailed { answered_model, .. }
            | Event::RequestCancelled { answered_model, .. } => {
                return (estimate, answered_model);
            }
            _ => {}
        }
    }
}

/// 一段 4000 个 ASCII 字符的话：估出来是 1000 个 token，加上一条消息的结构开销 3 个
fn anthropic_request(model: &str, stream: bool) -> String {
    format!(
        r#"{{"model":"{model}","max_tokens":16,"stream":{stream},"messages":[{{"role":"user","content":"{}"}}]}}"#,
        "abcd".repeat(1_000)
    )
}

/// 直通：上游写的模型名原样记下，开始事件带着估算
#[tokio::test]
async fn a_passthrough_answer_names_its_model_and_the_start_carries_the_estimate() {
    let up = upstream(
        r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929",
            "content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn",
            "usage":{"input_tokens":1010,"output_tokens":1}}"#,
        "application/json",
    )
    .await;
    let (gw, mut rx) = serve(cfg(up, Protocol::Anthropic)).await;
    ask(
        gw,
        "/v1/messages",
        &anthropic_request("claude-sonnet-4-5", false),
    )
    .await;
    assert_eq!(
        facts(&mut rx).await,
        (Some(1_003), Some("claude-sonnet-4-5-20250929".into()))
    );
}

/// 转换：客户端说 Anthropic、上游说 Chat。**记的是上游自己写的**，不是转给客户端时
/// 网关写进 Anthropic 格式里的那一个
#[tokio::test]
async fn a_converted_answer_is_read_in_the_upstreams_own_words() {
    let up = upstream(
        r#"{"id":"c1","object":"chat.completion","created":1,"model":"gpt-4o-2024-08-06",
            "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":1010,"completion_tokens":1,"total_tokens":1011}}"#,
        "application/json",
    )
    .await;
    let (gw, mut rx) = serve(cfg(up, Protocol::OpenaiChat)).await;
    let said = ask(gw, "/v1/messages", &anthropic_request("gpt-4o", false)).await;
    assert!(
        said.contains("\"type\":\"message\""),
        "没转成 Anthropic 格式：{said}"
    );
    assert_eq!(
        facts(&mut rx).await,
        (Some(1_003), Some("gpt-4o-2024-08-06".into())),
        "估的是同一段内容，转换不动它"
    );
}

/// 流式：模型名在开场那一帧里
#[tokio::test]
async fn a_streamed_answer_names_its_model_in_the_opening_frame() {
    let up = upstream(
        concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",",
            "\"model\":\"claude-haiku-4-5-20251001\",\"usage\":{\"input_tokens\":1010,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
            "\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ),
        "text/event-stream",
    )
    .await;
    let (gw, mut rx) = serve(cfg(up, Protocol::Anthropic)).await;
    ask(
        gw,
        "/v1/messages",
        &anthropic_request("claude-haiku-4-5", true),
    )
    .await;
    assert_eq!(
        facts(&mut rx).await,
        (Some(1_003), Some("claude-haiku-4-5-20251001".into()))
    );
}

/// 回答里没写模型名的：没有就是没有，不拿请求里的名字去补
#[tokio::test]
async fn an_answer_without_a_model_records_none() {
    let up = upstream(
        r#"{"type":"message","content":[],"usage":{"input_tokens":3,"output_tokens":1}}"#,
        "application/json",
    )
    .await;
    let (gw, mut rx) = serve(cfg(up, Protocol::Anthropic)).await;
    ask(
        gw,
        "/v1/messages",
        &anthropic_request("claude-sonnet-4-5", false),
    )
    .await;
    assert_eq!(facts(&mut rx).await.1, None);
}

/// **对话存在上游服务端的请求不估**：内容不在请求里，估出来的数和上游看到的不是一回事
#[tokio::test]
async fn a_request_that_leans_on_server_side_state_has_no_estimate() {
    let up = upstream(
        r#"{"id":"resp_2","object":"response","status":"completed","model":"gpt-5-2025-08-07",
            "output":[],"usage":{"input_tokens":9000,"output_tokens":1,"total_tokens":9001}}"#,
        "application/json",
    )
    .await;
    let (gw, mut rx) = serve(cfg(up, Protocol::OpenaiResponses)).await;
    ask(
        gw,
        "/v1/responses",
        r#"{"model":"gpt-5","previous_response_id":"resp_1","input":"接着说"}"#,
    )
    .await;
    assert_eq!(
        facts(&mut rx).await,
        (None, Some("gpt-5-2025-08-07".into())),
        "模型名照记，估算没有"
    );
}
