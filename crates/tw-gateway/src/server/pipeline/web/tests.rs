use super::*;
use axum::{Json, Router, routing::post};
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;

fn config() -> tw_config::Config {
    let mut config = tw_config::Config::default();
    config.clients.push(tw_config::Client {
        name: "test".into(),
        key: "test-key".into(),
        max_concurrent: Some(1),
        ..Default::default()
    });
    config.web_tools.mode = tw_api::WebToolsMode::Core;
    config.web_tools.proxy = tw_config::DIRECT.into();
    config
}

fn sample() -> tw_api::WebToolResult {
    tw_api::WebToolResult {
        sources: vec![tw_api::WebSource {
            url: "https://sqlite.org/changes.html".into(),
            title: "SQLite changes".into(),
            text: "Release 3.50".into(),
            engine: Some(tw_api::SearchEngine::Bing),
            published_at: None,
        }],
        cached: false,
        elapsed_ms: 1,
        warnings: Vec::new(),
        truncated: false,
    }
}

async fn send(state: AppState, path: &str, body: Value, key: &str) -> Response {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("x-api-key", key)
        .extension(axum::extract::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        crate::router(state).oneshot(request),
    )
    .await
    .unwrap()
    .unwrap()
}

async fn seed(state: &AppState) {
    state
        .runtime()
        .web_tools
        .seed(
            "test",
            &tw_api::WebToolRequest::Search {
                query: "SQLite".into(),
                allowed_domains: Vec::new(),
                blocked_domains: Vec::new(),
            },
            sample(),
        )
        .await;
}

#[tokio::test]
async fn standalone_search_needs_no_upstream_and_preserves_auth_and_native_sse() {
    let state = AppState::new(config()).unwrap();
    seed(&state).await;
    for stream in [false, true] {
        let body = json!({"model":"any-model","stream":stream,"max_tokens":1024,"messages":[{"role":"user","content":"Perform a web search for the query: SQLite"}],"tools":[{"type":"web_search_20250305","name":"web_search"}],"tool_choice":{"type":"tool","name":"web_search"}});
        assert_eq!(
            send(state.clone(), "/v1/messages", body.clone(), "wrong")
                .await
                .status(),
            401
        );
        let response = send(state.clone(), "/v1/messages", body, "test-key").await;
        assert_eq!(response.status(), 200);
        let bytes = axum::body::to_bytes(response.into_body(), 100000)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("web_search_tool_result"));
        assert!(text.contains("https://sqlite.org/changes.html"));
        if stream {
            assert!(text.contains("event: message_stop"));
        } else {
            assert_eq!(
                serde_json::from_str::<Value>(&text).unwrap()["usage"]["output_tokens"],
                0
            );
        }
    }
}

#[tokio::test]
async fn responses_loop_searches_then_resumes_model_without_deadlocking_a_one_slot_key() {
    let calls = Arc::new(AtomicUsize::new(0));
    let n = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/v1/responses", post(move |Json(body): Json<Value>| {
            let n = n.clone(); async move {
                let i = n.fetch_add(1, Ordering::SeqCst);
                assert!(!body["tools"].as_array().unwrap().iter().any(|t| t["type"] == "web_search"));
                if i == 0 {
                    Json(json!({"id":"resp_1","object":"response","status":"completed","model":"model","output":[{"type":"function_call","id":"fc_1","call_id":"call_1","name":protocol::SEARCH,"arguments":"{\"query\":\"SQLite\"}"}],"usage":{"input_tokens":10,"output_tokens":3}}))
                } else {
                    assert!(body["input"].as_array().unwrap().iter().any(|v| v["type"] == "function_call_output"));
                    Json(json!({"id":"resp_2","object":"response","status":"completed","model":"model","output":[{"type":"message","id":"msg_2","role":"assistant","status":"completed","content":[{"type":"output_text","text":"See https://sqlite.org/changes.html","annotations":[]}]}],"usage":{"input_tokens":20,"output_tokens":6}}))
                }
            }
        }))).await.unwrap();
    });
    let mut config = config();
    config.providers.push(tw_config::Provider {
        name: "fake".into(),
        base_url: format!("http://{addr}"),
        protocol: Some(tw_config::Protocol::OpenaiResponses),
        proxy: tw_config::DIRECT.into(),
        ..Default::default()
    });
    let state = AppState::new(config).unwrap();
    seed(&state).await;
    let body = json!({"model":"model","input":"Find SQLite","tools":[{"type":"web_search"}],"stream":false});
    let response = send(state.clone(), "/v1/responses", body, "test-key").await;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 100000)
        .await
        .unwrap();
    let reply: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status, 200, "{reply}");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(reply["output"][0]["type"], "web_search_call");
    assert_eq!(reply["usage"]["input_tokens"], 30);
    assert_eq!(
        reply["output"][1]["content"][0]["annotations"][0]["url"],
        "https://sqlite.org/changes.html"
    );
    let mut next = json!({"input":reply["output"]});
    state
        .runtime()
        .web_tools
        .history
        .restore("test", Dialect::Responses, &mut next, true)
        .unwrap();
    assert_eq!(next["input"][0]["type"], "function_call");
    assert_eq!(next["input"][1]["type"], "function_call_output");
    server.abort();
}

async fn mock_upstream(
    config: &mut tw_config::Config,
    api: tw_config::Protocol,
    path: &'static str,
    handler: impl Fn(Value, usize) -> Response + Send + Sync + 'static,
) -> (tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let handler = Arc::new(handler);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    config.providers.push(tw_config::Provider {
        name: "mock".into(),
        base_url: format!("http://{addr}"),
        protocol: Some(api),
        proxy: tw_config::DIRECT.into(),
        ..Default::default()
    });
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                path,
                post(move |Json(body): Json<Value>| {
                    let handler = handler.clone();
                    let count = count.clone();
                    async move { handler(body, count.fetch_add(1, Ordering::SeqCst)) }
                }),
            ),
        )
        .await
        .unwrap();
    });
    (server, calls)
}

async fn json_body(response: Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn anthropic_mixed_client_tool_history_and_cached_usage_survive_the_next_turn() {
    let mut cfg = config();
    let (server, calls) = mock_upstream(&mut cfg, tw_config::Protocol::Anthropic, "/v1/messages", |body, i| {
        if i == 0 {
            Json(json!({"id":"msg_1","type":"message","role":"assistant","model":"model","content":[
                {"type":"tool_use","id":"client_1","name":"Read","input":{"path":"README.md"}},
                {"type":"tool_use","id":"search_1","name":protocol::SEARCH,"input":{"query":"SQLite"}}
            ],"stop_reason":"tool_use","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":30}})).into_response()
        } else {
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(messages[1]["content"][0]["name"], protocol::SEARCH);
            assert_eq!(messages[2]["content"][0]["tool_use_id"], "search_1");
            assert_eq!(messages[3]["content"][0]["id"], "client_1");
            assert_eq!(messages[4]["content"][0]["tool_use_id"], "client_1");
            assert!(!body.to_string().contains("encrypted_content"));
            Json(json!({"id":"msg_2","type":"message","role":"assistant","model":"model","content":[{"type":"text","text":"Done"}],"stop_reason":"end_turn","usage":{"input_tokens":4,"output_tokens":2}})).into_response()
        }
    }).await;
    let state = AppState::new(cfg).unwrap();
    seed(&state).await;
    let tools = json!([{"type":"web_search_20250305","name":"web_search"},{"name":"Read","input_schema":{"type":"object","properties":{"path":{"type":"string"}}}}]);
    let request = json!({"model":"model","max_tokens":1024,"tools":tools,"messages":[{"role":"user","content":"Search SQLite and read README"}]});
    let response = send(state.clone(), "/v1/messages", request.clone(), "test-key").await;
    assert_eq!(response.status(), 200);
    let response = json_body(response).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "client tools must return to the client immediately"
    );
    assert_eq!(response["usage"]["cache_read_input_tokens"], 30);
    assert_eq!(response["content"][0]["type"], "server_tool_use");
    assert_eq!(response["content"][2]["id"], "client_1");
    let mut next = request;
    next["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":response["content"]}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"client_1","content":"README content"}]}),
    ]);
    let response = send(state, "/v1/messages", next, "test-key").await;
    assert_eq!(response.status(), 200, "{}", json_body(response).await);
    server.abort();
}

#[tokio::test]
async fn responses_open_find_and_search_preserve_cached_token_totals() {
    let mut cfg = config();
    let (server, _) = mock_upstream(&mut cfg, tw_config::Protocol::OpenaiResponses, "/v1/responses", |body, i| {
        let output = if i < 3 {
            let (name, arguments) = match i {
                0 => (protocol::SEARCH, json!({"query":"SQLite"})),
                1 => (protocol::OPEN, json!({"url":"https://sqlite.org/changes.html"})),
                _ => (protocol::FIND, json!({"url":"https://sqlite.org/changes.html","pattern":"Release"})),
            };
            json!([{"type":"function_call","id":format!("fc_{i}"),"call_id":format!("call_{i}"),"name":name,"arguments":arguments.to_string()}])
        } else {
            assert_eq!(body["input"].as_array().unwrap().iter().filter(|v| v["type"] == "function_call_output").count(), 3);
            json!([{"type":"message","id":"msg_final","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Source https://sqlite.org/changes.html","annotations":[]}]}])
        };
        Json(json!({"id":format!("resp_{i}"),"object":"response","status":"completed","model":"model","output":output,"usage":{"input_tokens":20,"input_tokens_details":{"cached_tokens":10},"output_tokens":5}})).into_response()
    }).await;
    let state = AppState::new(cfg).unwrap();
    seed(&state).await;
    for request in [
        tw_api::WebToolRequest::OpenPage {
            url: "https://sqlite.org/changes.html".into(),
            offset: 0,
        },
        tw_api::WebToolRequest::FindInPage {
            url: "https://sqlite.org/changes.html".into(),
            pattern: "Release".into(),
        },
    ] {
        state
            .runtime()
            .web_tools
            .seed("test", &request, sample())
            .await;
    }
    let response = send(
        state,
        "/v1/responses",
        json!({"model":"model","input":"Read the release","tools":[{"type":"web_search"}]}),
        "test-key",
    )
    .await;
    assert_eq!(response.status(), 200);
    let body = json_body(response).await;
    assert_eq!(body["usage"]["input_tokens"], 80);
    assert_eq!(body["usage"]["input_tokens_details"]["cached_tokens"], 40);
    assert_eq!(body["usage"]["total_tokens"], 100);
    let actions: Vec<_> = body["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["action"]["type"].as_str())
        .collect();
    assert_eq!(actions, ["search", "open_page", "find_in_page"]);
    server.abort();
}

#[tokio::test]
async fn upstream_failure_status_and_retry_headers_are_preserved() {
    let mut cfg = config();
    let (server, _) = mock_upstream(&mut cfg, tw_config::Protocol::OpenaiResponses, "/v1/responses", |_, _| {
        (axum::http::StatusCode::UNPROCESSABLE_ENTITY, [("retry-after", "17")], Json(json!({"error":{"type":"invalid_request_error","message":"Model option is unsupported"}}))).into_response()
    }).await;
    let state = AppState::new(cfg).unwrap();
    let response = send(
        state,
        "/v1/responses",
        json!({"model":"model","input":"Search","tools":[{"type":"web_search"}]}),
        "test-key",
    )
    .await;
    assert_eq!(response.status(), 422);
    assert_eq!(response.headers()["retry-after"], "17");
    assert_eq!(
        json_body(response).await["error"]["message"],
        "Model option is unsupported"
    );
    server.abort();
}

#[tokio::test]
async fn websocket_search_and_previous_response_use_the_http_pipeline() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
    let mut cfg = config();
    let (server, _) = mock_upstream(&mut cfg, tw_config::Protocol::OpenaiResponses, "/v1/responses", |body, i| {
        let output = if i == 0 {
            json!([{"type":"function_call","id":"fc_ws","call_id":"c_ws","name":protocol::SEARCH,"arguments":"{\"query\":\"SQLite\"}"}])
        } else {
            if i == 2 {
                assert!(body.get("previous_response_id").is_none());
                assert!(body["input"].as_array().unwrap().iter().any(|v| v["type"] == "function_call_output"));
            }
            json!([{"type":"message","id":format!("msg_{i}"),"role":"assistant","status":"completed","content":[{"type":"output_text","text":"SQLite source","annotations":[]}]}])
        };
        Json(json!({"id":format!("resp_{i}"),"object":"response","status":"completed","model":"model","output":output,"usage":{"input_tokens":2,"output_tokens":2}})).into_response()
    }).await;
    let state = AppState::new(cfg).unwrap();
    seed(&state).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let gateway = tokio::spawn(async move {
        axum::serve(
            listener,
            crate::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let mut request = format!("ws://{addr}/v1/responses")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", "Bearer test-key".parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    for i in 0..2 {
        let mut body = json!({"type":"response.create","model":"model","input":"Search SQLite","tools":[{"type":"web_search"}]});
        if i == 1 {
            body["previous_response_id"] = json!("resp_1");
        }
        ws.send(Message::Text(body.to_string().into()))
            .await
            .unwrap();
        let done = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let value: Value = serde_json::from_str(&text).unwrap();
                assert_ne!(value["type"], "error", "{value}");
                if value["type"] == "response.completed" {
                    return value;
                }
            }
            panic!("WebSocket closed without a completed response");
        })
        .await
        .unwrap();
        assert_eq!(done["response"]["status"], "completed");
        if i == 0 {
            assert_eq!(done["response"]["output"][0]["type"], "web_search_call");
        }
    }
    ws.close(None).await.unwrap();
    gateway.abort();
    server.abort();
}
