//! 方言互转端到端（M6+）。
//!
//! 验的是那一个真实场景：**手上一把 DeepSeek / Kimi / GLM 的 key，
//! 想让 Claude Code 用上。**所以这里的入站请求是 Claude Code 真会发的
//! 那种形状（system 块数组、工具定义、tool_result），而假上游**只会说
//! OpenAI chat**，收到别的形状就 400。

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::routing::post;
use tw_config::{Client, Config, Listen, Provider};

/// 一个**只认 OpenAI chat** 的上游。
///
/// 它会检查形状：路径不对、有 `anthropic-version` 头、body 里有
/// `system` 顶层字段 —— 任何一条都 400。**这就是「静默改坏请求」的
/// 那道闸**：转换有问题的话，这里会立刻红。
async fn start_openai_upstream(sse: bool) -> (SocketAddr, Arc<Mutex<Vec<u8>>>) {
    let seen: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(
                move |State(s): State<Arc<Mutex<Vec<u8>>>>,
                      headers: axum::http::HeaderMap,
                      body: bytes::Bytes| async move {
                    *s.lock().unwrap() = body.to_vec();
                    if headers.contains_key("anthropic-version") {
                        return axum::response::Response::builder()
                            .status(400)
                            .body(axum::body::Body::from("我不认识 anthropic-version 这个头"))
                            .unwrap();
                    }
                    let v: serde_json::Value = match serde_json::from_slice(&body) {
                        Ok(v) => v,
                        Err(e) => {
                            return axum::response::Response::builder()
                                .status(400)
                                .body(axum::body::Body::from(format!("body 不是 JSON：{e}")))
                                .unwrap();
                        }
                    };
                    if v.get("system").is_some() || v.get("stop_sequences").is_some() {
                        return axum::response::Response::builder()
                            .status(400)
                            .body(axum::body::Body::from("这是 Anthropic 的形状，我不认"))
                            .unwrap();
                    }
                    if sse {
                        // 一片一片地发，工具参数横跨三帧 —— 和真实上游一样
                        let mut out = String::new();
                        out.push_str("data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"我看一下。\"},\"finish_reason\":null}]}\n\n");
                        out.push_str("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"Read\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n");
                        out.push_str("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"file_path\\\":\"}}]},\"finish_reason\":null}]}\n\n");
                        out.push_str("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"/a/b.rs\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n");
                        out.push_str("data: {\"choices\":[],\"usage\":{\"prompt_tokens\":2450,\"completion_tokens\":95,\"prompt_tokens_details\":{\"cached_tokens\":1800}}}\n\n");
                        out.push_str("data: [DONE]\n\n");
                        axum::response::Response::builder()
                            .header("content-type", "text/event-stream")
                            .body(axum::body::Body::from(out))
                            .unwrap()
                    } else {
                        let out = serde_json::json!({
                            "id": "chatcmpl-1",
                            "model": "deepseek-chat",
                            "choices": [{
                                "message": { "role": "assistant", "content": "读完了。" },
                                "finish_reason": "stop"
                            }],
                            "usage": { "prompt_tokens": 20, "completion_tokens": 4 }
                        });
                        axum::response::Response::builder()
                            .header("content-type", "application/json")
                            .body(axum::body::Body::from(out.to_string()))
                            .unwrap()
                    }
                },
            ),
        )
        .with_state(s);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, seen)
}

async fn start_gateway(
    up: SocketAddr,
) -> (SocketAddr, tokio::sync::broadcast::Receiver<tw_api::Event>) {
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "claude-code".into(),
            key: "tw-testkey".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "deepseek".into(),
            base_url: format!("http://{up}"),
            key: Some("sk-deepseek".into()),
            // **这一行是整个功能的开关**
            protocol: Some(tw_config::Protocol::OpenaiChat),
            ..Default::default()
        }],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let rx = state.bus.subscribe();
    let addr = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    let s2 = state.clone();
    tokio::spawn(async move { tw_gateway::serve(s2, addr).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, rx)
}

/// Claude Code 真会发的那种形状。
fn claude_body(stream: bool) -> String {
    serde_json::json!({
        "model": "deepseek-chat",
        "max_tokens": 1024,
        "system": [{"type":"text","text":"你是一个编程助手","cache_control":{"type":"ephemeral"}}],
        "messages": [
            {"role":"user","content":"帮我看看 a/b.rs"},
            {"role":"assistant","content":[{"type":"tool_use","id":"tu_0","name":"Read","input":{"file_path":"/a/b.rs"}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_0","content":"fn main() {}"}]}
        ],
        "tools": [{"name":"Read","description":"读文件","input_schema":{"type":"object","properties":{"file_path":{"type":"string"}}}}],
        "stop_sequences": ["\n\nHuman:"],
        "thinking": {"type":"enabled","budget_tokens":1024},
        "stream": stream
    })
    .to_string()
}

async fn ask(gw: SocketAddr, body: String) -> (u16, String) {
    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-testkey")
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.text().await.unwrap())
}

#[tokio::test]
async fn a_claude_code_request_reaches_an_openai_only_upstream_in_the_right_shape() {
    let (up, seen) = start_openai_upstream(false).await;
    let (gw, _) = start_gateway(up).await;

    let (status, body) = ask(gw, claude_body(false)).await;
    assert_eq!(status, 200, "上游拒了：{body}");

    let sent: serde_json::Value =
        serde_json::from_slice(&seen.lock().unwrap().clone()).expect("发出去的不是 JSON");
    // system 变成了第一条消息
    assert_eq!(sent["messages"][0]["role"], "system");
    assert_eq!(sent["messages"][0]["content"], "你是一个编程助手");
    // tool_use → tool_calls，参数是**字符串**
    assert_eq!(
        sent["messages"][2]["tool_calls"][0]["function"]["name"],
        "Read"
    );
    assert!(sent["messages"][2]["tool_calls"][0]["function"]["arguments"].is_string());
    // tool_result 变成了独立的一条 role: tool
    assert_eq!(sent["messages"][3]["role"], "tool");
    assert_eq!(sent["messages"][3]["tool_call_id"], "tu_0");
    // 工具定义换了形状
    assert_eq!(sent["tools"][0]["type"], "function");
    assert_eq!(
        sent["tools"][0]["function"]["parameters"]["properties"]["file_path"]["type"],
        "string"
    );
    // stop_sequences 改名了
    assert_eq!(sent["stop"][0], "\n\nHuman:");
    assert!(sent.get("stop_sequences").is_none());
    // 思考预算折成推理强度（1024 是最低一档）
    assert!(sent.get("thinking").is_none());
    assert_eq!(sent["reasoning_effort"], "minimal");
}

#[tokio::test]
async fn the_answer_comes_back_in_the_shape_the_claude_client_expects() {
    let (up, _) = start_openai_upstream(false).await;
    let (gw, _) = start_gateway(up).await;
    let (status, body) = ask(gw, claude_body(false)).await;
    assert_eq!(status, 200, "{body}");

    let v: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("回来的不是 JSON：{e}\n{body}"));
    assert_eq!(v["type"], "message");
    assert_eq!(v["role"], "assistant");
    assert_eq!(v["content"][0]["type"], "text");
    assert_eq!(v["content"][0]["text"], "读完了。");
    assert_eq!(v["stop_reason"], "end_turn");
    assert_eq!(v["usage"]["input_tokens"], 20);
    assert_eq!(v["usage"]["output_tokens"], 4);
}

#[tokio::test]
async fn a_streamed_answer_arrives_as_a_complete_anthropic_envelope() {
    let (up, _) = start_openai_upstream(true).await;
    let (gw, _) = start_gateway(up).await;
    let (status, body) = ask(gw, claude_body(true)).await;
    assert_eq!(status, 200, "{body}");

    let mut kinds = Vec::new();
    let mut text = String::new();
    let mut args = String::new();
    let mut tool_name = None;
    let mut stop = None;
    let mut usage = None;
    let mut ev = String::new();
    for line in body.lines() {
        if let Some(e) = line.strip_prefix("event: ") {
            ev = e.to_string();
            kinds.push(e.to_string());
        } else if let Some(d) = line.strip_prefix("data: ")
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(d)
        {
            if let Some(t) = v["delta"]["text"].as_str() {
                text.push_str(t);
            }
            if let Some(a) = v["delta"]["partial_json"].as_str() {
                args.push_str(a);
            }
            if v["content_block"]["type"] == "tool_use" {
                tool_name = v["content_block"]["name"].as_str().map(|s| s.to_string());
            }
            if ev == "message_delta" {
                stop = v["delta"]["stop_reason"].as_str().map(|s| s.to_string());
                usage = Some(v["usage"].clone());
            }
        }
    }
    // **六种帧都要在**，少一种客户端就卡住或者报错
    for want in [
        "message_start",
        "content_block_start",
        "content_block_delta",
        "content_block_stop",
        "message_delta",
        "message_stop",
    ] {
        assert!(kinds.iter().any(|k| k == want), "少了 {want}：\n{body}");
    }
    assert_eq!(text, "我看一下。");
    assert_eq!(tool_name.as_deref(), Some("Read"));
    // 工具参数拼起来必须是合法 JSON —— 客户端就是这么用它的
    let parsed: serde_json::Value =
        serde_json::from_str(&args).unwrap_or_else(|e| panic!("参数拼不出合法 JSON：{e}\n{args}"));
    assert_eq!(parsed["file_path"], "/a/b.rs");
    assert_eq!(stop.as_deref(), Some("tool_use"));
    // 缓存命中要搬对位置、而且从 input 里减掉
    let u = usage.expect("没有 usage");
    assert_eq!(u["input_tokens"], 650);
    assert_eq!(u["cache_read_input_tokens"], 1800);
    assert_eq!(u["output_tokens"], 95);
}

#[tokio::test]
async fn what_could_not_be_translated_is_reported_to_the_user() {
    // **用户会发现「扩展思考开了却没生效」而完全不知道从哪儿查起。**
    let (up, _) = start_openai_upstream(false).await;
    let (gw, mut rx) = start_gateway(up).await;
    // 服务端工具只有 Anthropic 能执行
    let mut body: serde_json::Value = serde_json::from_str(&claude_body(false)).unwrap();
    body["tools"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"type": "web_search_20250305", "name": "web_search"}));
    ask(gw, body.to_string()).await;

    let mut found = None;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
        if let tw_api::Event::Translated {
            from, to, dropped, ..
        } = ev
        {
            found = Some((from, to, dropped));
            break;
        }
    }
    let (from, to, dropped) = found.expect("没发翻译事件");
    assert_eq!((from.slug(), to.slug()), ("anthropic", "openai-chat"));
    assert_eq!(
        dropped,
        ["tools.web_search_20250305"],
        "没说服务端工具被丢了"
    );
}

#[tokio::test]
async fn the_usage_sniffer_still_sees_the_numbers_after_translation() {
    // **这是最容易断的一处接缝。**翻译出来的流要能被我们自己的嗅探器
    // 认出来，否则成本面板对所有互转请求集体失明。
    let (up, _) = start_openai_upstream(true).await;
    let (gw, mut rx) = start_gateway(up).await;
    ask(gw, claude_body(true)).await;

    let mut usage = None;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
        if let tw_api::Event::RequestFinished { usage: u, .. } = ev {
            usage = u;
            break;
        }
    }
    let u = usage.expect("嗅探器一个数字都没拿到 —— 成本面板会对互转请求集体失明");
    assert_eq!(u.output, 95);
    assert_eq!(u.cache_read, 1800);
}
