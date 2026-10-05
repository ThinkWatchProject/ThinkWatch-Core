//! 快慢样本（`url-test` 和按快慢分的 `load-balance` 看的那个数）量的是哪一段，端到端：从
//! 这一跳发出去到回答的第一段内容 —— 不含之前失败了的几跳，不是响应头，和这一跳排在第几家
//! 无关；整包的回答不记；开头慢被放弃的那一家记它被给的那段时间。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use bytes::Bytes;
use serde_json::{Value, json};
use tw_config::{Client, Config, Failover, Protocol, Provider};
use tw_gateway::latency::Latency;

/// 一个假上游的样子
#[derive(Clone, Default)]
struct Script {
    /// 响应头之前等多久
    header_delay_ms: u64,
    /// 回一个错误（这个状态码），不回答
    status: Option<u16>,
    /// 流式：开头的例行帧之后隔多久出第一段内容
    content_after_ms: u64,
    /// 流式：发完开头就只发心跳，一直不出内容
    stall: bool,
}

const MESSAGE_START: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":1}}}\n\n";
const PING: &str = "event: ping\ndata: {\"type\":\"ping\"}\n\n";
const ANSWER: &str = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
     event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n\
     event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
     event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
const WHOLE: &str = r#"{"type":"message","role":"assistant","content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":12,"output_tokens":1}}"#;

async fn upstream(script: Script) -> SocketAddr {
    let app = Router::new().fallback(axum::routing::any(move |body: Bytes| {
        let script = script.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(script.header_delay_ms)).await;
            if let Some(status) = script.status {
                return axum::response::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"type":"error","error":{"type":"api_error","message":"boom"}}"#,
                    ))
                    .unwrap();
            }
            let streams = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|v| v["stream"].as_bool())
                .unwrap_or(false);
            if !streams {
                tokio::time::sleep(Duration::from_millis(script.content_after_ms)).await;
                return axum::response::Response::builder()
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(WHOLE))
                    .unwrap();
            }
            let (after, stall) = (script.content_after_ms, script.stall);
            let body = async_stream::stream! {
                yield Ok::<_, std::convert::Infallible>(Bytes::from_static(MESSAGE_START.as_bytes()));
                if stall {
                    loop {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        yield Ok(Bytes::from_static(PING.as_bytes()));
                    }
                } else {
                    tokio::time::sleep(Duration::from_millis(after)).await;
                    yield Ok(Bytes::from_static(ANSWER.as_bytes()));
                }
            };
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from_stream(body))
                .unwrap()
        }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

fn provider(name: &str, at: SocketAddr) -> Provider {
    Provider {
        name: name.into(),
        base_url: format!("http://{at}"),
        key: Some("sk-upstream".into()),
        protocol: Some(Protocol::Anthropic),
        ..Default::default()
    }
}

/// 起网关，按声明的顺序故障转移。开头最多等 1 秒（测试里图快；配置校验要求换家时至少
/// 5 秒，网关自己不查）
async fn gateway(providers: Vec<Provider>, switch: bool) -> (SocketAddr, Arc<Latency>) {
    let cfg = Config {
        version: 1,
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers,
        failover: Failover {
            stream_start_wait_secs: 1,
            next_on_slow_start: switch,
            ..Default::default()
        },
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let latency = state.latency.clone();
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, latency)
}

/// 发 `n` 个请求，每个都读完
async fn ask(gw: SocketAddr, stream: bool, n: usize) {
    let body = json!({"model": "claude-sonnet-5", "max_tokens": 64, "stream": stream,
                      "messages": [{"role": "user", "content": "Say hello."}]});
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for _ in 0..n {
        let resp = client
            .post(format!("http://{gw}/v1/messages"))
            .header("content-type", "application/json")
            .header("x-api-key", "tw-k")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        assert_eq!(status, 200, "{text}");
        assert!(text.contains("hello"), "{text}");
    }
}

#[tokio::test]
async fn the_sample_runs_to_the_first_content_not_to_the_response_headers() {
    // 响应头和开头的例行帧马上就到，内容 400 毫秒之后才来：上游在排队。它是唯一的一家（最后
    // 一家，开头不看直接转发），以前记的是响应头到的那一刻
    let up = upstream(Script {
        content_after_ms: 400,
        ..Default::default()
    })
    .await;
    let (gw, latency) = gateway(vec![provider("排队", up)], false).await;
    ask(gw, true, 3).await;
    let t = latency.typical("排队").expect("三个流式回答该有三个样本");
    assert!((400..1_500).contains(&t), "{t} 毫秒");
}

#[tokio::test]
async fn earlier_hops_are_not_charged_to_the_upstream_that_answers() {
    // 第一家 700 毫秒后回 500，第二家 100 毫秒出内容：第二家的样本里没有第一家那段
    let bad = upstream(Script {
        header_delay_ms: 700,
        status: Some(500),
        ..Default::default()
    })
    .await;
    let good = upstream(Script {
        content_after_ms: 100,
        ..Default::default()
    })
    .await;
    let (gw, latency) = gateway(vec![provider("坏", bad), provider("好", good)], false).await;
    ask(gw, true, 3).await;
    let t = latency.typical("好").expect("三个样本");
    assert!(
        (100..600).contains(&t),
        "{t} 毫秒：第一家的时间算到了它头上"
    );
    assert_eq!(latency.typical("坏"), None, "失败的不记");
}

#[tokio::test]
async fn the_position_of_the_hop_does_not_change_what_is_measured() {
    // 排在前面的那一家开头要先看一眼有没有报错，再转发；量到的和它排在最后时是同一段
    let first = upstream(Script {
        content_after_ms: 300,
        ..Default::default()
    })
    .await;
    let spare = upstream(Script::default()).await;
    let (gw, latency) = gateway(vec![provider("先", first), provider("备", spare)], false).await;
    ask(gw, true, 3).await;
    let t = latency.typical("先").expect("三个样本");
    assert!((300..1_200).contains(&t), "{t} 毫秒");
    assert_eq!(latency.typical("备"), None, "没用上的不记");
}

#[tokio::test]
async fn an_answer_that_is_not_streamed_leaves_no_sample() {
    // 整包的回答只有「全到了」那一个时刻：分不出排队和说话
    let up = upstream(Script {
        content_after_ms: 50,
        ..Default::default()
    })
    .await;
    let (gw, latency) = gateway(vec![provider("整包", up)], false).await;
    ask(gw, false, 3).await;
    assert_eq!(latency.typical("整包"), None);
}

#[tokio::test]
async fn an_upstream_given_up_on_is_charged_the_time_it_was_given() {
    // 慢的那一家以前很快（留着三个快的样本），现在开了流就不出内容：每次等满 1 秒被放弃。
    // 它记 1 秒，中位数就落到慢的那一头；接下来答的那一家不替它背这 1 秒
    let slow = upstream(Script {
        stall: true,
        ..Default::default()
    })
    .await;
    let quick = upstream(Script::default()).await;
    let (gw, latency) = gateway(vec![provider("慢", slow), provider("快", quick)], true).await;
    for _ in 0..3 {
        latency.record("慢", 50);
    }
    ask(gw, true, 3).await;
    assert_eq!(latency.typical("慢"), Some(1_000));
    let t = latency.typical("快").expect("三个样本");
    assert!(t < 800, "{t} 毫秒：放弃的那一家的等待算到了它头上");
}

#[tokio::test]
async fn an_upstream_whose_headers_never_come_is_charged_the_time_it_was_given() {
    let mute = upstream(Script {
        header_delay_ms: 10_000,
        ..Default::default()
    })
    .await;
    let quick = upstream(Script::default()).await;
    let (gw, latency) = gateway(vec![provider("无声", mute), provider("快", quick)], true).await;
    ask(gw, true, 3).await;
    assert_eq!(latency.typical("无声"), Some(1_000));
    assert!(latency.typical("快").is_some_and(|t| t < 800));
}
