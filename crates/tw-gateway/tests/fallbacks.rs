//! 上游接不住时网关替它补上的两件事，端到端。
//!
//! - 数 token 选中了别的格式的上游、或者同格式的上游没实现它（404 / 405）：网关
//!   自己估一个数，一个字节都不发给别的格式的上游（见 `tw_gateway::count`）
//! - 上游拒绝了别的账号封存的推理：去掉它们同一家再发一次，这段对话往后发给它之前
//!   先去掉拒过的那些（见 `tw_gateway::seal`）

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{OriginalUri, State};
use serde_json::{Value, json};
use tw_config::{Client, Config, Protocol, Provider};

/// 一个假上游收到的请求：路径和请求体
type Seen = Arc<Mutex<Vec<(String, Value)>>>;

/// 一个假上游：按到达的顺序依次用 `replies` 回（用完了一直用最后一个），记下收到的请求。
async fn upstream(replies: Vec<(u16, Value)>) -> (SocketAddr, Seen) {
    let seen: Seen = Default::default();
    let app = Router::new()
        .fallback(
            move |State(s): State<Seen>, OriginalUri(uri): OriginalUri, body: bytes::Bytes| {
                let replies = replies.clone();
                async move {
                    let mut s = s.lock().unwrap();
                    let n = s.len();
                    s.push((
                        uri.to_string(),
                        serde_json::from_slice(&body).unwrap_or(Value::Null),
                    ));
                    let (status, reply) = replies[n.min(replies.len() - 1)].clone();
                    axum::response::Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(reply.to_string()))
                        .unwrap()
                }
            },
        )
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, seen)
}

fn provider(name: &str, up: SocketAddr, protocol: Protocol) -> Provider {
    Provider {
        name: name.into(),
        base_url: format!("http://{up}"),
        key: Some("sk-upstream".into()),
        protocol: Some(protocol),
        ..Default::default()
    }
}

async fn gateway(
    providers: Vec<Provider>,
) -> (SocketAddr, tokio::sync::broadcast::Receiver<tw_api::Event>) {
    let cfg = Config {
        version: 1,
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers,
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let rx = state.bus.subscribe();
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, rx)
}

/// 发一个请求：状态码、`x-thinkwatch-local` 在不在、正文
async fn post(gw: SocketAddr, path: &str, body: Value) -> (u16, bool, String) {
    let resp = reqwest::Client::new()
        .post(format!("http://{gw}{path}"))
        .header("content-type", "application/json")
        .header("x-api-key", "tw-k")
        .header("authorization", "Bearer tw-k")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let local = resp.headers().contains_key("x-thinkwatch-local");
    (status, local, resp.text().await.unwrap())
}

/// 这个请求的路由事件和结束事件
async fn routed_and_finished(
    rx: &mut tokio::sync::broadcast::Receiver<tw_api::Event>,
) -> (Vec<tw_api::AttemptView>, tw_api::Billing, tw_api::Event) {
    let mut routed = None;
    loop {
        let e = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("no ending")
            .unwrap();
        match e {
            tw_api::Event::RequestRouted {
                attempts, billing, ..
            } => routed = Some((attempts, billing)),
            e @ (tw_api::Event::RequestFinished { .. } | tw_api::Event::RequestFailed { .. }) => {
                let (attempts, billing) = routed.expect("routed before the ending");
                return (attempts, billing, e);
            }
            _ => {}
        }
    }
}

const COUNT: &str = "/v1/messages/count_tokens";

fn count_body() -> Value {
    json!({"model": "gpt-5", "system": "You are terse.",
           "messages": [{"role": "user", "content": "Summarise the README in one line."}]})
}

#[tokio::test]
async fn counting_tokens_through_an_upstream_of_another_format_is_estimated_locally() {
    let (up, seen) = upstream(vec![(500, json!({"error": "nothing should arrive"}))]).await;
    let (gw, mut rx) = gateway(vec![provider("openai", up, Protocol::OpenaiChat)]).await;
    let (status, local, body) = post(gw, COUNT, count_body()).await;
    assert_eq!(status, 200, "{body}");
    assert!(local, "an estimate carries x-thinkwatch-local");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v["input_tokens"].as_u64().unwrap() > 5, "{v}");
    assert!(seen.lock().unwrap().is_empty(), "the upstream was asked");

    let (attempts, billing, end) = routed_and_finished(&mut rx).await;
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].provider, "openai");
    assert_eq!(attempts[0].outcome, tw_api::AttemptOutcome::Estimated);
    assert_eq!(attempts[0].status, None, "nothing was sent");
    assert_eq!(billing, tw_api::Billing::Free);
    assert!(
        matches!(
            end,
            tw_api::Event::RequestFinished {
                status: 200,
                usage: None,
                ..
            }
        ),
        "{end:?}"
    );
}

#[tokio::test]
async fn an_anthropic_relay_without_count_tokens_gets_estimated_for() {
    let (up, seen) = upstream(vec![(404, json!({"error": "not found"}))]).await;
    let (gw, mut rx) = gateway(vec![provider("relay", up, Protocol::Anthropic)]).await;
    let (status, local, body) = post(gw, COUNT, count_body()).await;
    assert_eq!((status, local), (200, true), "{body}");
    assert!(serde_json::from_str::<Value>(&body).unwrap()["input_tokens"].is_u64());
    assert_eq!(seen.lock().unwrap().len(), 1);

    let (attempts, _, _) = routed_and_finished(&mut rx).await;
    assert_eq!(attempts[0].outcome, tw_api::AttemptOutcome::Estimated);
    assert_eq!(attempts[0].status, Some(404), "what the relay said");
}

#[tokio::test]
async fn an_anthropic_upstream_that_counts_is_still_asked() {
    let (up, seen) = upstream(vec![(200, json!({"input_tokens": 42}))]).await;
    let (gw, _rx) = gateway(vec![provider("official", up, Protocol::Anthropic)]).await;
    let (status, local, body) = post(gw, COUNT, count_body()).await;
    assert_eq!((status, local), (200, false), "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({"input_tokens": 42})
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_failed_count_does_not_move_to_an_upstream_of_another_format() {
    // 选中的那一家 503：往下只换同格式的。别的格式的那一家既不问，也不替它估一个数
    let (a, seen_a) = upstream(vec![(503, json!({"error": "overloaded"}))]).await;
    let (b, seen_b) = upstream(vec![(500, json!({"error": "nothing should arrive"}))]).await;
    let (gw, mut rx) = gateway(vec![
        provider("official", a, Protocol::Anthropic),
        provider("openai", b, Protocol::OpenaiChat),
    ])
    .await;
    let (status, local, body) = post(gw, COUNT, count_body()).await;
    assert!(!local, "{body}");
    assert_eq!(status, 502, "{body}");
    assert_eq!(seen_a.lock().unwrap().len(), 1);
    assert!(seen_b.lock().unwrap().is_empty());
    let (attempts, _, _) = routed_and_finished(&mut rx).await;
    assert_eq!(attempts.len(), 1, "{attempts:?}");
}

#[tokio::test]
async fn geminis_count_tokens_is_estimated_in_geminis_shape() {
    let (up, seen) = upstream(vec![(500, json!({"error": "nothing should arrive"}))]).await;
    let (gw, _rx) = gateway(vec![provider("claude", up, Protocol::Anthropic)]).await;
    let (status, local, body) = post(
        gw,
        "/v1beta/models/gemini-2.5-pro:countTokens",
        json!({"contents": [{"role": "user", "parts": [{"text": "Summarise the README."}]}]}),
    )
    .await;
    assert_eq!((status, local), (200, true), "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v["totalTokens"].as_u64().unwrap() > 0, "{v}");
    assert!(seen.lock().unwrap().is_empty());
}

// ---------------------------------------------------------------- 封存的推理

fn refused_by_openai() -> (u16, Value) {
    (
        400,
        json!({"error": {"message": "The encrypted content for item rs_1 could not be verified.",
                         "type": "invalid_request_error", "param": null,
                         "code": "invalid_encrypted_content"}}),
    )
}

fn responses_ok() -> (u16, Value) {
    (
        200,
        json!({"id": "resp_1", "object": "response", "status": "completed", "model": "gpt-5",
               "output": [{"type": "message", "role": "assistant",
                           "content": [{"type": "output_text", "text": "done"}]}],
               "usage": {"input_tokens": 10, "output_tokens": 2, "total_tokens": 12}}),
    )
}

fn codex_turn(reasoning: &[&str]) -> Value {
    let mut input = vec![json!({"role": "user", "content": "list the files"})];
    for (i, sealed) in reasoning.iter().enumerate() {
        input.push(
            json!({"type": "reasoning", "id": format!("rs_{i}"), "summary": [],
                          "encrypted_content": sealed}),
        );
        input.push(
            json!({"type": "function_call", "call_id": format!("c{i}"), "name": "ls",
                          "arguments": "{}"}),
        );
        input.push(
            json!({"type": "function_call_output", "call_id": format!("c{i}"),
                          "output": "a.txt"}),
        );
    }
    json!({"model": "gpt-5", "prompt_cache_key": "conv-1", "input": input,
           "include": ["reasoning.encrypted_content"]})
}

fn sealed_in(v: &Value) -> Vec<String> {
    v["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["encrypted_content"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn reasoning_sealed_by_another_account_is_left_out_and_sent_again_once() {
    let (up, seen) = upstream(vec![refused_by_openai(), responses_ok(), responses_ok()]).await;
    let (gw, _rx) = gateway(vec![provider("openai", up, Protocol::OpenaiResponses)]).await;

    let (status, _, body) = post(gw, "/v1/responses", codex_turn(&["gAAA-other"])).await;
    assert_eq!(status, 200, "{body}");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "refused, then sent again");
        assert_eq!(sealed_in(&seen[0].1), ["gAAA-other"]);
        assert!(sealed_in(&seen[1].1).is_empty());
        // 说过的话、调过的工具都还在
        assert_eq!(seen[1].1["input"].as_array().unwrap().len(), 3);
    }

    // 下一轮：拒过的那段发之前就去掉，这一家自己后来封存的照样带上。不再先被拒一次
    let (status, _, body) = post(
        gw,
        "/v1/responses",
        codex_turn(&["gAAA-other", "gAAA-mine"]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert_eq!(sealed_in(&seen[2].1), ["gAAA-mine"]);
}

#[tokio::test]
async fn another_400_comes_back_as_it_was_and_is_not_sent_again() {
    let reply = json!({"error": {"message": "max_output_tokens is too large",
                                  "type": "invalid_request_error", "code": "invalid_value"}});
    let (up, seen) = upstream(vec![(400, reply.clone()), responses_ok()]).await;
    let (gw, _rx) = gateway(vec![provider("openai", up, Protocol::OpenaiResponses)]).await;
    let (status, _, body) = post(gw, "/v1/responses", codex_turn(&["gAAA-other"])).await;
    assert_eq!(status, 400);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), reply);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_refused_thinking_signature_is_left_out_and_sent_again() {
    let refused = json!({"type": "error", "error": {"type": "invalid_request_error",
        "message": "messages.1.content.0: Invalid `signature` in `thinking` block"}});
    let ok = json!({"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-opus-4-7",
                    "content": [{"type": "text", "text": "好"}], "stop_reason": "end_turn",
                    "usage": {"input_tokens": 10, "output_tokens": 1}});
    let (up, seen) = upstream(vec![(400, refused), (200, ok)]).await;
    let (gw, _rx) = gateway(vec![provider("relay", up, Protocol::Anthropic)]).await;
    let (status, _, body) = post(
        gw,
        "/v1/messages",
        json!({
            "model": "claude-opus-4-7", "max_tokens": 2000,
            "thinking": {"type": "enabled", "budget_tokens": 1024},
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "想", "signature": "EqQBCkgIBx"},
                    {"type": "text", "text": "你好"},
                ]},
                {"role": "user", "content": "再说一句"},
            ],
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[1].1["messages"][1]["content"],
        json!([{"type": "text", "text": "你好"}])
    );
    // 最后一条 assistant 消息不在调工具：推理照样开着
    assert!(seen[1].1.get("thinking").is_some());
}
