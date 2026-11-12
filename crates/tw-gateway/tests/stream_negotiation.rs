//! Response-mode negotiation through real HTTP, including the client wire shape.
use axum::{Router, body::Body, http::Uri, response::Response};
use bytes::Bytes;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tw_config::{Client, Config, Protocol, Provider};
use tw_dialect::ir::{Dialect, Target};

type Seen = Arc<Mutex<Vec<(String, Value)>>>;

fn response(status: u16, ct: &str, body: String) -> Response {
    // Deliberately split SSE frames and tool arguments across transport chunks.
    let chunks: Vec<_> = body
        .as_bytes()
        .chunks(7)
        .map(|b| Ok::<_, std::io::Error>(Bytes::copy_from_slice(b)))
        .collect();
    let mut builder = Response::builder()
        .status(status)
        .header("content-type", ct);
    if status == 429 {
        builder = builder.header("retry-after", "3600");
    }
    builder
        .body(Body::from_stream(futures::stream::iter(chunks)))
        .unwrap()
}

async fn upstream(
    reply: impl Fn(&str, &Value, usize) -> Response + Send + Sync + 'static,
) -> (SocketAddr, Seen) {
    let seen = Seen::default();
    let app = Router::new().fallback({
        let seen = seen.clone();
        let reply = Arc::new(reply);
        move |uri: Uri, body: Bytes| {
            let (seen, reply) = (seen.clone(), reply.clone());
            async move {
                let v: Value = serde_json::from_slice(&body).unwrap();
                let path = uri.to_string();
                let n = {
                    let mut seen = seen.lock().unwrap();
                    seen.push((path.clone(), v.clone()));
                    seen.len()
                };
                reply(&path, &v, n)
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, seen)
}

async fn gateway(up: SocketAddr, protocol: Protocol) -> (SocketAddr, tw_gateway::AppState) {
    let mut cfg = Config {
        version: 1,
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "up".into(),
            base_url: format!("http://{up}"),
            protocol: Some(protocol),
            key: (protocol == Protocol::Bedrock).then(|| "ABSK-test".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    cfg.failover.wait_secs = 1;
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let addr = tw_gateway::serve(state.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    (addr, state)
}

fn refusal(stream: bool) -> Response {
    response(400, "application/json", json!({"error":{
        "type":"invalid_request_error", "param":"stream",
        "message": if stream { "Streaming is not supported." } else { "This model only supports streaming." }
    }}).to_string())
}

fn whole() -> Value {
    json!({"id":"answer","object":"chat.completion","model":"m","choices":[{"index":0,"message":{
        "role":"assistant","content":"ok","tool_calls":[{"id":"call_1","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Paris\"}"}}]
    },"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}})
}

fn sse() -> String {
    let chunks = [
        json!({"choices":[{"index":0,"delta":{"role":"assistant","content":"o"}}]}),
        json!({"choices":[{"index":0,"delta":{"content":"k","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"weather","arguments":"{\"city\":"}}]}}]}),
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Paris\"}"}}]},"finish_reason":"tool_calls"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}}),
    ];
    let mut out: String = chunks.iter().map(|v| format!("data: {v}\n\n")).collect();
    out.push_str("data: [DONE]\n\n");
    out
}

fn request(dialect: Dialect, stream: bool) -> (&'static str, Value) {
    match dialect {
        Dialect::Chat => {
            let mut v = json!({"model":"m","stream":stream,"messages":[{"role":"user","content":"hello"}],"tools":[{"type":"function","function":{"name":"weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}]});
            if stream {
                v["stream_options"] = json!({"include_usage":true});
            }
            ("/v1/chat/completions", v)
        }
        Dialect::Anthropic => (
            "/v1/messages",
            json!({"model":"m","stream":stream,"max_tokens":4096,"messages":[{"role":"user","content":"hello"}],"tools":[{"name":"weather","input_schema":{"type":"object","properties":{"city":{"type":"string"}}}}]}),
        ),
        Dialect::Responses => (
            "/v1/responses",
            json!({"model":"m","stream":stream,"input":[{"role":"user","content":"hello"}],"tools":[{"type":"function","name":"weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}]}),
        ),
        _ => unreachable!(),
    }
}

async fn post(gw: SocketAddr, path: &str, body: &Value) -> (u16, String, Vec<u8>) {
    let r = reqwest::Client::new()
        .post(format!("http://{gw}{path}"))
        .header("authorization", "Bearer tw-k")
        .json(body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let ct = r.headers()["content-type"].to_str().unwrap().to_owned();
    (status, ct, r.bytes().await.unwrap().to_vec())
}

fn assert_answer(dialect: Dialect, stream: bool, path: &str, req: &Value, ct: &str, body: &[u8]) {
    assert!(
        ct.starts_with(if stream {
            "text/event-stream"
        } else {
            "application/json"
        }),
        "{ct}"
    );
    // Read the client's actual response back as a common Chat shape. This also
    // verifies that tool deltas form valid JSON and that usage survived both ways.
    let mut session = tw_dialect::convert::decode(dialect, req, path, None)
        .unwrap()
        .encode(&Target {
            dialect,
            official: false,
            default_max_tokens: 0,
        })
        .session;
    session.client = Dialect::Chat;
    let normalized = if stream {
        let mut collector = session.collector();
        collector.process(body);
        collector.finish().unwrap()
    } else {
        session.response(body).unwrap()
    };
    let v: Value = serde_json::from_slice(&normalized).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "ok", "{v}");
    let call = &v["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["id"], "call_1", "{v}");
    assert_eq!(call["function"]["name"], "weather");
    assert_eq!(
        serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap()).unwrap(),
        json!({"city":"Paris"})
    );
    assert_eq!(v["usage"]["prompt_tokens"], 11, "{v}");
    assert_eq!(v["usage"]["completion_tokens"], 7, "{v}");
}

#[tokio::test]
async fn both_directions_keep_the_client_protocol_tools_usage_and_learn_the_mode() {
    for dialect in [Dialect::Chat, Dialect::Anthropic, Dialect::Responses] {
        for client_stream in [false, true] {
            let (up, seen) = upstream(move |_, v, _| {
                let stream = v["stream"].as_bool().unwrap_or(false);
                if stream == client_stream {
                    refusal(stream)
                } else if stream {
                    response(200, "text/event-stream", sse())
                } else {
                    response(200, "application/json", whole().to_string())
                }
            })
            .await;
            let (gw, state) = gateway(up, Protocol::OpenaiChat).await;
            let (path, req) = request(dialect, client_stream);
            for count in [2, 3] {
                let (status, ct, body) = post(gw, path, &req).await;
                assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
                assert_answer(dialect, client_stream, path, &req, &ct, &body);
                assert_eq!(seen.lock().unwrap().len(), count);
            }
            let sent = seen.lock().unwrap().clone();
            assert_eq!(
                sent[0].1["stream"].as_bool().unwrap_or(false),
                client_stream
            );
            assert_eq!(sent[1].1["stream"], !client_stream);
            assert_eq!(sent[0].1["messages"], sent[1].1["messages"]);
            assert_eq!(sent[0].1["tools"], sent[1].1["tools"]);
            if client_stream {
                assert!(sent[1].1.get("stream_options").is_none());
            } else {
                assert_eq!(sent[1].1["stream_options"]["include_usage"], true);
            }
            // A reload forgets runtime capability evidence.
            state.reload((*state.config()).clone()).unwrap();
            assert_eq!(post(gw, path, &req).await.0, 200);
            assert_eq!(seen.lock().unwrap().len(), 5);
        }
    }
}

#[tokio::test]
async fn opaque_rejections_can_probe_both_directions_and_learn_only_a_working_mode() {
    for status in [400, 422, 501] {
        for client_stream in [false, true] {
            let (up, seen) = upstream(move |_, v, _| {
                if v["stream"].as_bool().unwrap_or(false) == client_stream {
                    response(
                        status,
                        "application/json",
                        json!({"error":{
                            "code":"vendor_1027", "message":"请使用另一种返回方式。"
                        }})
                        .to_string(),
                    )
                } else if client_stream {
                    response(200, "application/json", whole().to_string())
                } else {
                    response(200, "text/event-stream", sse())
                }
            })
            .await;
            let (gw, _) = gateway(up, Protocol::OpenaiChat).await;
            let (path, req) = request(Dialect::Chat, client_stream);
            for count in [2, 3] {
                let (status, ct, body) = post(gw, path, &req).await;
                assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
                assert_answer(Dialect::Chat, client_stream, path, &req, &ct, &body);
                assert_eq!(seen.lock().unwrap().len(), count);
            }
        }
    }
}

#[tokio::test]
async fn failed_probes_preserve_the_original_response_and_both_attempt_outcomes() {
    for probe_status in [400, 422, 501, 503, 401, 403, 429, 200] {
        let original = json!({"error":{"code":"vendor_1027", "message":"Original rejection"}});
        let first = original.clone();
        let (up, seen) = upstream(move |_, _, n| {
            if n == 1 {
                let mut r = response(422, "application/json", first.to_string());
                r.headers_mut()
                    .insert("x-request-id", "original-id".parse().unwrap());
                r
            } else if probe_status == 200 {
                response(200, "text/event-stream", format!("data: {}\n\n", json!({"error":{"type":"invalid_request_error","message":"probe failed before content"}})))
            } else {
                response(
                    probe_status,
                    "application/json",
                    json!({"error":{"message":"Probe rejection"}}).to_string(),
                )
            }
        })
        .await;
        let (gw, state) = gateway(up, Protocol::OpenaiChat).await;
        let mut events = state.bus.subscribe();
        let (path, req) = request(Dialect::Chat, false);
        let r = reqwest::Client::new()
            .post(format!("http://{gw}{path}"))
            .header("authorization", "Bearer tw-k")
            .json(&req)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 422, "probe {probe_status}");
        assert_eq!(r.headers()["x-request-id"], "original-id");
        assert_eq!(r.json::<Value>().await.unwrap(), original);
        let sent = seen.lock().unwrap().clone();
        assert_eq!(sent.len(), 2, "probe {probe_status}");
        assert_eq!(sent[0].1["stream"], false);
        assert_eq!(sent[1].1["stream"], true);
        let mut routed = None;
        let mut auth = Vec::new();
        while let Ok(event) = events.try_recv() {
            match event {
                tw_api::Event::RequestRouted { attempts, .. } => routed = Some(attempts),
                tw_api::Event::AuthChanged { state, .. } => auth.push(state),
                _ => {}
            }
        }
        let attempts = routed.unwrap();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].status, Some(422));
        assert_eq!(attempts[0].outcome, tw_api::AttemptOutcome::Served);
        if probe_status == 200 {
            assert_eq!(attempts[1].outcome, tw_api::AttemptOutcome::Error);
            assert!(
                attempts[1]
                    .error
                    .as_ref()
                    .unwrap()
                    .text
                    .contains("probe failed before content")
            );
        } else {
            assert_eq!(attempts[1].status, Some(probe_status));
            assert_eq!(attempts[1].outcome, tw_api::AttemptOutcome::Status);
        }
        if matches!(probe_status, 401 | 403) {
            assert_eq!(auth.last(), Some(&tw_api::AuthState::Rejected));
        }
        if probe_status == 429 {
            assert!(attempts[1].pause_ms.is_some());
            assert!(!attempts[1].counts_for_upstream());
        }
    }
}

#[tokio::test]
async fn a_failed_server_error_probe_still_fails_over_with_the_original_request_mode() {
    let (up, first) = upstream(|_, _, n| {
        response(
            if n == 1 { 501 } else { 503 },
            "application/json",
            json!({"error":{"message":"Request rejected"}}).to_string(),
        )
    })
    .await;
    let (backup, second) =
        upstream(|_, _, _| response(200, "application/json", whole().to_string())).await;
    let cfg: Config = serde_yaml_ng::from_str(&format!(
        "version: 1\ndefault_route: all\nclients:\n  - name: c\n    key: tw-k\nproviders:\n  - name: up\n    base_url: http://{up}\n    protocol: openai-chat\n  - name: next\n    base_url: http://{backup}\n    protocol: openai-chat\ngroups:\n  - name: pool\n    type: fallback\n    providers: [up, next]\nroutes:\n  - name: all\n    rules:\n      - name: all\n        to: pool\n"
    )).unwrap();
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let gw = tw_gateway::serve(state.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    let mut events = state.bus.subscribe();
    let (path, req) = request(Dialect::Chat, false);
    let (status, ct, body) = post(gw, path, &req).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_answer(Dialect::Chat, false, path, &req, &ct, &body);
    let first = first.lock().unwrap();
    assert_eq!(first.len(), 2);
    assert_eq!(first[0].1["stream"], false);
    assert_eq!(first[1].1["stream"], true);
    let second = second.lock().unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].1["stream"], false);
    let mut routed = None;
    while let Ok(event) = events.try_recv() {
        if let tw_api::Event::RequestRouted { attempts, .. } = event {
            routed = Some(attempts);
        }
    }
    let attempts = routed.unwrap();
    assert_eq!(
        attempts.iter().map(|a| a.status).collect::<Vec<_>>(),
        vec![Some(501), Some(503), Some(200)]
    );
}

#[tokio::test]
async fn an_unrelated_opaque_rejection_can_cost_one_probe_but_never_teaches_a_preference() {
    let original = json!({"error":{"message":"Invalid request"}});
    let error = original.clone();
    let (up, seen) =
        upstream(move |_, _, _| response(400, "application/json", error.to_string())).await;
    let (gw, _) = gateway(up, Protocol::OpenaiChat).await;
    let (path, req) = request(Dialect::Chat, false);
    for count in [2, 4] {
        let (status, _, body) = post(gw, path, &req).await;
        assert_eq!(status, 400);
        assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), original);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), count);
        assert_eq!(seen[count - 2].1["stream"], false);
        assert_eq!(seen[count - 1].1["stream"], true);
    }
}

#[tokio::test]
async fn retry_hints_and_mislabelled_account_errors_do_not_probe() {
    for error in [
        json!({"type":"authentication_error","message":"opaque"}),
        json!({"code":"rate_limit_exceeded","message":"opaque"}),
        json!({"status":"RESOURCE_EXHAUSTED","message":"opaque"}),
        json!({"message":"Request timed out"}),
        json!({"message":"请求频率过高"}),
        json!({"message":"Retry later"}),
    ] {
        let with_hint = error["message"] == "Retry later";
        let (up, seen) = upstream(move |_, _, _| {
            let mut r = response(400, "application/json", json!({"error":error}).to_string());
            if with_hint {
                r.headers_mut().insert("retry-after", "30".parse().unwrap());
            }
            r
        })
        .await;
        let (gw, _) = gateway(up, Protocol::OpenaiChat).await;
        let (path, req) = request(Dialect::Chat, false);
        assert_eq!(post(gw, path, &req).await.0, 400);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn successful_opposite_mode_is_converted_without_an_extra_request() {
    for client_stream in [false, true] {
        let (up, seen) = upstream(move |_, _, _| {
            if client_stream {
                response(200, "application/json", whole().to_string())
            } else {
                response(200, "text/event-stream", sse())
            }
        })
        .await;
        let (gw, _) = gateway(up, Protocol::OpenaiChat).await;
        let (path, req) = request(Dialect::Chat, client_stream);
        let (status, ct, body) = post(gw, path, &req).await;
        assert_eq!(status, 200);
        assert_answer(Dialect::Chat, client_stream, path, &req, &ct, &body);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn failed_alternatives_and_incomplete_or_error_streams_are_not_cached() {
    for alternative in [0, 1, 2, 3] {
        let (up, seen) = upstream(move |_, v, _| {
            if v["stream"] != true {
                return refusal(false);
            }
            match alternative {
                0 => refusal(true),
                1 => response(
                    200,
                    "text/event-stream",
                    "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n".into(),
                ),
                2 => response(
                    200,
                    "text/event-stream",
                    format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        json!({"error":{"message":"failed","type":"invalid_request_error"}})
                    ),
                ),
                _ => response(
                    200,
                    "application/json",
                    "{\"error\":{\"message\":\"failed\"}}".into(),
                ),
            }
        })
        .await;
        let (gw, _) = gateway(up, Protocol::OpenaiChat).await;
        let (path, req) = request(Dialect::Chat, false);
        for count in [2, 4] {
            let _ = post(gw, path, &req).await;
            assert_eq!(
                seen.lock().unwrap().len(),
                count,
                "alternative {alternative}"
            );
        }
    }
}

#[tokio::test]
async fn rate_limits_authentication_and_explicit_non_mode_validation_never_switch_modes() {
    for (status, message, param) in [
        (429, "This model only supports streaming.", "stream"),
        (400, "Rate limit exceeded", "stream"),
        (401, "This model only supports streaming.", "stream"),
        (403, "This model only supports streaming.", "stream"),
        (422, "invalid messages", "messages"),
        (400, "Streaming is not supported.", "stream_options"),
    ] {
        let (up, seen) = upstream(move |_, _, _| {
            response(
                status,
                "application/json",
                json!({"error":{"message":message,"param":param}}).to_string(),
            )
        })
        .await;
        let (gw, _) = gateway(up, Protocol::OpenaiChat).await;
        let (path, req) = request(Dialect::Chat, false);
        let _ = post(gw, path, &req).await;
        assert_eq!(seen.lock().unwrap().len(), 1, "status {status}: {message}");
        assert_eq!(seen.lock().unwrap()[0].1["stream"], false);
    }
}

#[tokio::test]
async fn native_response_features_the_mode_converter_cannot_preserve_are_not_retried() {
    let (up, seen) = upstream(|_, _, _| refusal(false)).await;
    let (gw, _) = gateway(up, Protocol::OpenaiChat).await;
    let (path, original) = request(Dialect::Chat, false);
    for (i, extra) in [
        json!({"n":2}),
        json!({"logprobs":true}),
        json!({"modalities":["text","audio"]}),
    ]
    .into_iter()
    .enumerate()
    {
        let mut req = original.clone();
        req.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert_eq!(post(gw, path, &req).await.0, 400);
        assert_eq!(seen.lock().unwrap().len(), i + 1);
    }
}

#[tokio::test]
async fn a_json_error_inside_http_200_preserves_the_original_error_and_is_not_cached() {
    for client_stream in [false, true] {
        let (up, seen) = upstream(move |_, v, _| {
            let stream = v["stream"].as_bool().unwrap_or(false);
            if stream == client_stream {
                refusal(stream)
            } else {
                response(
                    200,
                    "application/json",
                    json!({"error":{"message":"generation failed"}}).to_string(),
                )
            }
        })
        .await;
        let (gw, _) = gateway(up, Protocol::OpenaiChat).await;
        let (path, req) = request(Dialect::Chat, client_stream);
        for count in [2, 4] {
            let (status, ct, body) = post(gw, path, &req).await;
            assert_eq!(status, 400);
            assert!(ct.starts_with("application/json"));
            let error: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                error["error"]["message"],
                if client_stream {
                    "Streaming is not supported."
                } else {
                    "This model only supports streaming."
                }
            );
            assert_eq!(seen.lock().unwrap().len(), count);
        }
    }
}

#[tokio::test]
async fn a_remembered_mode_can_be_relearned_when_the_upstream_changes() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let streams = Arc::new(AtomicBool::new(true));
    let allowed = streams.clone();
    let (up, seen) = upstream(move |_, v, _| {
        let stream = v["stream"].as_bool().unwrap_or(false);
        if stream != allowed.load(Ordering::SeqCst) {
            refusal(stream)
        } else if stream {
            response(200, "text/event-stream", sse())
        } else {
            response(200, "application/json", whole().to_string())
        }
    })
    .await;
    let (gw, _) = gateway(up, Protocol::OpenaiChat).await;
    let (path, req) = request(Dialect::Chat, false);
    assert_eq!(post(gw, path, &req).await.0, 200);
    assert_eq!(seen.lock().unwrap().len(), 2);
    streams.store(false, Ordering::SeqCst);
    assert_eq!(post(gw, path, &req).await.0, 200);
    assert_eq!(seen.lock().unwrap().len(), 4);
    assert_eq!(post(gw, path, &req).await.0, 200);
    assert_eq!(seen.lock().unwrap().len(), 5);
    let (path, req) = request(Dialect::Chat, true);
    assert_eq!(post(gw, path, &req).await.0, 200);
    assert_eq!(seen.lock().unwrap().len(), 6);
}

#[tokio::test]
async fn gemini_negotiation_changes_the_endpoint_and_keeps_the_client_framing() {
    for client_stream in [false, true] {
        let (up, seen) = upstream(move |path, _, _| {
            let stream = path.contains(":streamGenerateContent");
            if stream == client_stream { return refusal(stream); }
            let v = json!({"candidates":[{"content":{"parts":[{"text":"ok"}],"role":"model"},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":7,"totalTokenCount":18}});
            if stream { response(200, "text/event-stream", format!("data: {v}\n\n")) }
            else { response(200, "application/json", v.to_string()) }
        }).await;
        let (gw, _) = gateway(up, Protocol::Gemini).await;
        let path = if client_stream {
            "/v1beta/models/m:streamGenerateContent?alt=sse"
        } else {
            "/v1beta/models/m:generateContent"
        };
        let req = json!({"contents":[{"role":"user","parts":[{"text":"hello"}]}]});
        for count in [2, 3] {
            let (status, ct, body) = post(gw, path, &req).await;
            assert_eq!(status, 200);
            assert!(ct.starts_with(if client_stream {
                "text/event-stream"
            } else {
                "application/json"
            }));
            assert!(String::from_utf8_lossy(&body).contains("ok"));
            assert_eq!(seen.lock().unwrap().len(), count);
        }
        let sent = seen.lock().unwrap();
        assert_eq!(sent[0].1, sent[1].1);
        assert_eq!(
            sent[1].0.contains(":streamGenerateContent?alt=sse"),
            !client_stream
        );
        assert!(!sent[1].0.contains("generateContent?alt=sse"));
    }
}

#[tokio::test]
async fn native_anthropic_and_responses_keep_their_response_shape_in_both_directions() {
    for (dialect, protocol) in [
        (Dialect::Anthropic, Protocol::Anthropic),
        (Dialect::Responses, Protocol::OpenaiResponses),
    ] {
        for client_stream in [false, true] {
            let (up, seen) = upstream(move |_, v, _| {
                let stream = v["stream"].as_bool().unwrap_or(false);
                if stream == client_stream { return refusal(stream); }
                let (message, events) = if dialect == Dialect::Anthropic {
                    let message = json!({"id":"msg_1","type":"message","role":"assistant","model":"m","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":11,"output_tokens":7}});
                    let events = vec![
                        json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"m","content":[],"usage":{"input_tokens":11,"output_tokens":0}}}),
                        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
                        json!({"type":"content_block_stop","index":0}),
                        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}),
                        json!({"type":"message_stop"}),
                    ];
                    (message, events)
                } else {
                    let item = json!({"id":"msg_1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ok","annotations":[]}]});
                    let message = json!({"id":"resp_1","object":"response","model":"m","status":"completed","error":null,"output":[item],"usage":{"input_tokens":11,"output_tokens":7,"total_tokens":18}});
                    let events = vec![
                        json!({"type":"response.created","response":{"id":"resp_1","model":"m","status":"in_progress"}}),
                        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_1","type":"message","role":"assistant","content":[]}}),
                        json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}),
                        json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"ok"}),
                        json!({"type":"response.output_item.done","output_index":0,"item":item}),
                        json!({"type":"response.completed","response":message}),
                    ];
                    (message, events)
                };
                if stream {
                    response(200, "text/event-stream", events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect())
                } else { response(200, "application/json", message.to_string()) }
            }).await;
            let (gw, _) = gateway(up, protocol).await;
            let (path, req) = request(dialect, client_stream);
            for count in [2, 3] {
                let (status, ct, body) = post(gw, path, &req).await;
                assert_eq!(status, 200);
                assert!(ct.starts_with(if client_stream {
                    "text/event-stream"
                } else {
                    "application/json"
                }));
                assert!(String::from_utf8_lossy(&body).contains("ok"));
                assert_eq!(seen.lock().unwrap().len(), count);
            }
        }
    }
}

#[tokio::test]
async fn bedrock_negotiates_converse_actions_and_collects_real_eventstream_frames() {
    use aws_smithy_eventstream::frame::write_message_to;
    use aws_smithy_types::event_stream::{Header, HeaderValue, Message};
    for client_stream in [false, true] {
        let (up, seen) = upstream(move |path, _, _| {
            let stream = path.ends_with("/converse-stream");
            if stream == client_stream { return refusal(stream); }
            if !stream {
                return response(200, "application/json", json!({"output":{"message":{"role":"assistant","content":[{"text":"ok"}]}},"stopReason":"end_turn","usage":{"inputTokens":11,"outputTokens":7,"totalTokens":18}}).to_string());
            }
            let mut bytes = Vec::new();
            for (kind, v) in [
                ("messageStart", json!({"role":"assistant"})),
                ("contentBlockDelta", json!({"contentBlockIndex":0,"delta":{"text":"ok"}})),
                ("contentBlockStop", json!({"contentBlockIndex":0})),
                ("messageStop", json!({"stopReason":"end_turn"})),
                ("metadata", json!({"usage":{"inputTokens":11,"outputTokens":7,"totalTokens":18}})),
            ] {
                let frame = Message::new(v.to_string().into_bytes())
                    .add_header(Header::new(":message-type", HeaderValue::String("event".into())))
                    .add_header(Header::new(":event-type", HeaderValue::String(kind.into())))
                    .add_header(Header::new(":content-type", HeaderValue::String("application/json".into())));
                write_message_to(&frame, &mut bytes).unwrap();
            }
            Response::builder().header("content-type", "application/vnd.amazon.eventstream").body(Body::from(bytes)).unwrap()
        }).await;
        let (gw, _) = gateway(up, Protocol::Bedrock).await;
        let (path, req) = request(Dialect::Chat, client_stream);
        for count in [2, 3] {
            let (status, ct, body) = post(gw, path, &req).await;
            assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
            assert!(ct.starts_with(if client_stream {
                "text/event-stream"
            } else {
                "application/json"
            }));
            assert!(String::from_utf8_lossy(&body).contains("ok"));
            assert_eq!(seen.lock().unwrap().len(), count);
        }
        let sent = seen.lock().unwrap();
        assert_eq!(sent[1].0.ends_with("/converse-stream"), !client_stream);
        assert_eq!(sent[0].1, sent[1].1);
    }
}
