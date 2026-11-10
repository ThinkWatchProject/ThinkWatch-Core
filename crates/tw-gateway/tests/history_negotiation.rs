//! Regression boundaries for request-side runtime capability negotiation.
use axum::{Router, body::Body, response::Response};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tw_config::{Client, Config, Protocol, Provider};

type Seen = Arc<Mutex<Vec<Value>>>;
async fn upstream(
    reply: impl Fn(&Value, usize) -> (u16, &'static str, String) + Send + Sync + 'static,
) -> (SocketAddr, Seen) {
    let seen = Seen::default();
    let app = Router::new().fallback({
        let seen = seen.clone();
        let reply = Arc::new(reply);
        move |body: bytes::Bytes| {
            let seen = seen.clone();
            let reply = reply.clone();
            async move {
                let v: Value = serde_json::from_slice(&body).unwrap();
                let count = {
                    let mut seen = seen.lock().unwrap();
                    seen.push(v.clone());
                    seen.len()
                };
                let (status, ct, body) = reply(&v, count);
                Response::builder()
                    .status(status)
                    .header("content-type", ct)
                    .body(Body::from(body))
                    .unwrap()
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, seen)
}
fn rejected() -> (u16, &'static str, String) {
    (422,"application/json",json!({"detail":[{"type":"extra_forbidden","loc":["body","messages",0,"assistant","reasoning_content"]}]}).to_string())
}
fn accepted() -> (u16, &'static str, String) {
    (
        200,
        "application/json",
        json!({"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]})
            .to_string(),
    )
}
async fn gateway(ups: &[SocketAddr]) -> (SocketAddr, tw_gateway::AppState) {
    let cfg = if ups.len() == 1 {
        Config {
            version: 1,
            clients: vec![Client {
                name: "c".into(),
                key: "tw-k".into(),
                ..Default::default()
            }],
            providers: vec![Provider {
                name: "up".into(),
                base_url: format!("http://{}", ups[0]),
                protocol: Some(Protocol::OpenaiChat),
                ..Default::default()
            }],
            ..Default::default()
        }
    } else {
        serde_yaml_ng::from_str(&format!("version: 1\ndefault_route: all\nclients:\n  - name: c\n    key: tw-k\nproviders:\n  - name: up\n    base_url: http://{}\n    protocol: openai-chat\n  - name: next\n    base_url: http://{}\n    protocol: openai-chat\ngroups:\n  - name: pool\n    type: fallback\n    providers: [up, next]\nroutes:\n  - name: all\n    rules:\n      - name: all\n        to: pool\n",ups[0],ups[1])).unwrap()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let addr = tw_gateway::serve(state.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    (addr, state)
}
fn request() -> Value {
    json!({"model":"m","messages":[{"role":"assistant","content":"answer","reasoning_content":"thought"},{"role":"user","content":"next"}]})
}
async fn post(gw: SocketAddr, request: &Value) -> u16 {
    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/chat/completions"))
        .header("authorization", "Bearer tw-k")
        .json(request)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let _ = r.bytes().await.unwrap();
    status
}

#[tokio::test]
async fn unsuccessful_adjustment_is_not_cached() {
    let (up, seen) = upstream(|_, _| rejected()).await;
    let (gw, state) = gateway(&[up]).await;
    for count in [2, 4] {
        assert_eq!(post(gw, &request()).await, 422);
        assert_eq!(seen.lock().unwrap().len(), count);
        assert!(
            state
                .runtime()
                .history_memory
                .view("up", tw_gateway::history_memory::now_ms())
                .is_empty()
        );
    }
}

#[tokio::test]
async fn signed_native_history_is_never_automatically_removed() {
    let (up, seen) = upstream(|_, _| rejected()).await;
    let (gw, _) = gateway(&[up]).await;
    let mut v = request();
    v["messages"][0]["content"] = json!([{"type":"thinking","thinking":[{"type":"text","text":"native"}],"signature":"opaque"},{"type":"text","text":"answer"}]);
    assert_eq!(post(gw, &v).await, 422);
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(
        seen.lock().unwrap()[0]["messages"][0]["content"][0]["signature"],
        "opaque"
    );
}

#[tokio::test]
async fn later_candidates_receive_original_history_and_share_the_repair_budget() {
    let (first, a) = upstream(|_, n| {
        if n == 1 {
            rejected()
        } else {
            (
                503,
                "application/json",
                json!({"error":{"message":"overloaded"}}).to_string(),
            )
        }
    })
    .await;
    let (next, b) = upstream(|_, _| rejected()).await;
    let (gw, state) = gateway(&[first, next]).await;
    assert_eq!(post(gw, &request()).await, 422);
    assert_eq!(a.lock().unwrap().len(), 2);
    assert!(
        a.lock().unwrap()[1]["messages"][0]
            .get("reasoning_content")
            .is_none()
    );
    assert_eq!(
        b.lock().unwrap().len(),
        1,
        "negotiation must not restart at each provider"
    );
    assert_eq!(
        b.lock().unwrap()[0]["messages"][0]["reasoning_content"],
        "thought"
    );
    assert!(
        state
            .runtime()
            .history_memory
            .view("up", tw_gateway::history_memory::now_ms())
            .is_empty()
    );
}

#[tokio::test]
async fn sse_errors_neither_trigger_negotiation_nor_confirm_a_trial() {
    let stream_error = || {
        (
            200,
            "text/event-stream",
            format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"error":{"type":"invalid_request_error","message":"reasoning_content is unsupported","code":422}})
            ),
        )
    };
    let (up, seen) = upstream(move |_, n| if n == 1 { rejected() } else { stream_error() }).await;
    let (gw, state) = gateway(&[up]).await;
    let mut v = request();
    v["stream"] = json!(true);
    assert_eq!(post(gw, &v).await, 200);
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert!(
        state
            .runtime()
            .history_memory
            .view("up", tw_gateway::history_memory::now_ms())
            .is_empty()
    );
    assert_eq!(post(gw, &v).await, 200);
    assert_eq!(
        seen.lock().unwrap().len(),
        3,
        "an SSE error does not initiate a repair"
    );
}

#[tokio::test]
async fn tool_and_thinking_modes_are_independently_verified_and_reload_clears_them() {
    let (up, seen) = upstream(|v, _| {
        if v["messages"][0].get("reasoning_content").is_some() {
            rejected()
        } else {
            accepted()
        }
    })
    .await;
    let (gw, state) = gateway(&[up]).await;
    let original = request();
    let mut thinking = original.clone();
    thinking["reasoning_effort"] = json!("high");
    let mut tools = original.clone();
    tools["tools"] = json!([{"type":"function","function":{"name":"f","parameters":{"type":"object","properties":{}}}}]);
    for (i, v) in [original.clone(), thinking, tools].iter().enumerate() {
        assert_eq!(post(gw, v).await, 200);
        assert_eq!(seen.lock().unwrap().len(), (i + 1) * 2);
    }
    assert_eq!(
        state
            .runtime()
            .history_memory
            .view("up", tw_gateway::history_memory::now_ms())
            .len(),
        3
    );
    state.reload((*state.config()).clone()).unwrap();
    assert!(
        state
            .runtime()
            .history_memory
            .view("up", tw_gateway::history_memory::now_ms())
            .is_empty()
    );
    assert_eq!(post(gw, &original).await, 200);
    assert_eq!(seen.lock().unwrap().len(), 8);
}
