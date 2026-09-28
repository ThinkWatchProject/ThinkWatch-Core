//! Bedrock 上游，端到端。
//!
//! 假的 Bedrock 是一个本地服务：地址不是 AWS 的标准地址，所以签名用的区域写在
//! `aws.region`，模型目录也由它回答。验的是网关把几件事接对了：
//!
//! - 生成请求转成 Converse，发到 `/model/{id}/converse[-stream]`，ARN 的 `/` 转义
//! - 访问密钥：签名覆盖实际发出的地址和请求体；API Key：原样 Bearer，不签
//! - eventstream 的流转成客户端的格式，用量照记；流里的异常让请求失败
//! - 拒绝凭证时 AWS 的原话（点名账号）不交给客户端
//! - 缓存断点和 Bedrock 认的 beta 带过去
//! - 回显的占位符切在两帧里也还原得回来
//! - 模型目录：基础模型、预设和应用推理配置

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use aws_smithy_eventstream::frame::write_message_to;
use aws_smithy_types::event_stream::{Header, HeaderValue, Message};
use axum::Router;
use axum::extract::{OriginalUri, State};
use axum::http::HeaderMap;
use serde_json::{Value, json};
use tw_config::{
    Aws, Client, Config, Listen, Protocol, Provider, RedactPolicy, Secret, Security, SecurityMode,
};

const AK: &str = "AKIAIOSFODNN7EXAMPLE";
const SK: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
const REGION: &str = "us-east-1";
const MODEL: &str = "us.anthropic.claude-sonnet-4-5-20250929-v1:0";

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    /// 原样，没解码
    uri: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

/// 假 Bedrock 怎么回答
type Answer = Arc<dyn Fn(&Seen) -> axum::response::Response + Send + Sync>;

async fn bedrock(answer: Answer) -> (SocketAddr, Arc<Mutex<Vec<Seen>>>) {
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let app = Router::new()
        .fallback(
            move |State(log): State<Arc<Mutex<Vec<Seen>>>>,
                  method: axum::http::Method,
                  OriginalUri(uri): OriginalUri,
                  headers: HeaderMap,
                  body: bytes::Bytes| {
                let answer = answer.clone();
                async move {
                    let s = Seen {
                        method: method.to_string(),
                        uri: uri.to_string(),
                        headers,
                        body: body.to_vec(),
                    };
                    let resp = answer(&s);
                    log.lock().unwrap().push(s);
                    resp
                }
            },
        )
        .with_state(log);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, seen)
}

fn json_answer(status: u16, v: Value) -> axum::response::Response {
    axum::response::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(v.to_string()))
        .unwrap()
}

fn converse_reply(text: &str) -> Value {
    json!({
        "output": {"message": {"role": "assistant", "content": [{"text": text}]}},
        "stopReason": "end_turn",
        "usage": {"inputTokens": 11, "outputTokens": 7, "totalTokens": 18}
    })
}

/// 一帧 eventstream
fn frame(message_type: &str, kind: &str, payload: Value) -> Vec<u8> {
    let kind_header = if message_type == "exception" {
        ":exception-type"
    } else {
        ":event-type"
    };
    let m = Message::new(payload.to_string().into_bytes())
        .add_header(Header::new(
            ":message-type",
            HeaderValue::String(message_type.to_string().into()),
        ))
        .add_header(Header::new(
            kind_header,
            HeaderValue::String(kind.to_string().into()),
        ))
        .add_header(Header::new(
            ":content-type",
            HeaderValue::String("application/json".into()),
        ));
    let mut out = Vec::new();
    write_message_to(&m, &mut out).unwrap();
    out
}

fn event(kind: &str, payload: Value) -> Vec<u8> {
    frame("event", kind, payload)
}

/// ConverseStream 的一次回答：`texts` 一段一帧
fn stream_frames(texts: &[&str]) -> Vec<u8> {
    let mut out = event("messageStart", json!({"role": "assistant"}));
    for t in texts {
        out.extend(event(
            "contentBlockDelta",
            json!({"contentBlockIndex": 0, "delta": {"text": t}}),
        ));
    }
    out.extend(event("contentBlockStop", json!({"contentBlockIndex": 0})));
    out.extend(event("messageStop", json!({"stopReason": "end_turn"})));
    out.extend(event(
        "metadata",
        json!({"usage": {"inputTokens": 21, "outputTokens": 9, "totalTokens": 30}, "metrics": {"latencyMs": 5}}),
    ));
    out
}

fn eventstream_answer(bytes: Vec<u8>) -> axum::response::Response {
    axum::response::Response::builder()
        .status(200)
        .header("content-type", "application/vnd.amazon.eventstream")
        .body(axum::body::Body::from(bytes))
        .unwrap()
}

fn with_keys(up: SocketAddr) -> Provider {
    Provider {
        name: "br".into(),
        base_url: format!("http://{up}"),
        protocol: Some(Protocol::Bedrock),
        aws: Some(Aws {
            access_key_id: Some(Secret::new(AK)),
            secret_access_key: Some(Secret::new(SK)),
            region: Some(REGION.into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn with_api_key(up: SocketAddr) -> Provider {
    Provider {
        name: "br".into(),
        base_url: format!("http://{up}"),
        protocol: Some(Protocol::Bedrock),
        key: Some("ABSK-test".into()),
        ..Default::default()
    }
}

async fn gateway_with(
    p: Provider,
    security: Security,
) -> (
    SocketAddr,
    tw_gateway::AppState,
    tokio::sync::broadcast::Receiver<tw_api::Event>,
) {
    let cfg = Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![p],
        security,
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let rx = state.bus.subscribe();
    let addr = tw_gateway::serve(state.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, state, rx)
}

async fn gateway(
    p: Provider,
) -> (
    SocketAddr,
    tw_gateway::AppState,
    tokio::sync::broadcast::Receiver<tw_api::Event>,
) {
    gateway_with(p, Security::default()).await
}

async fn post(gw: SocketAddr, path: &str, headers: &[(&str, &str)], body: Value) -> (u16, String) {
    let mut r = reqwest::Client::new()
        .post(format!("http://{gw}{path}"))
        .header("content-type", "application/json");
    for (k, v) in headers {
        r = r.header(*k, *v);
    }
    let resp = r.body(body.to_string()).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap())
}

/// 这条请求的结局
async fn ending(rx: &mut tokio::sync::broadcast::Receiver<tw_api::Event>) -> tw_api::Event {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let e = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("no ending within 5 s")
            .unwrap();
        if matches!(
            e,
            tw_api::Event::RequestFinished { .. }
                | tw_api::Event::RequestFailed { .. }
                | tw_api::Event::RequestCancelled { .. }
        ) {
            return e;
        }
    }
}

/// `x-amz-date` → 签名时刻
fn signed_at(s: &Seen) -> SystemTime {
    let d = s.headers["x-amz-date"].to_str().unwrap();
    let n = |a: usize, b: usize| d[a..b].parse::<i64>().unwrap();
    let (y, mo, day, h, mi, sec) = (n(0, 4), n(4, 6), n(6, 8), n(9, 11), n(11, 13), n(13, 15));
    // 公历日期 → 1970 年起的天数（Howard Hinnant 的算法）
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    SystemTime::UNIX_EPOCH + Duration::from_secs((days * 86400 + h * 3600 + mi * 60 + sec) as u64)
}

/// 收到的请求自称的签名，就是拿同样的密钥、在同一时刻、对**收到的**地址和请求体签出来的
/// 那个。签名算法本身对着 SigV4 规范的核对在 tw-bedrock 里；这里验的是网关签的正是它发的
fn assert_signed_as_sent(s: &Seen) {
    let url = format!("http://{}{}", s.headers["host"].to_str().unwrap(), s.uri);
    let expected = tw_bedrock::sign::sign(
        &tw_bedrock::Credentials {
            access_key_id: AK.into(),
            secret_access_key: SK.into(),
            session_token: None,
        },
        REGION,
        &s.method,
        &url,
        Some(&s.body),
        signed_at(s),
    )
    .unwrap();
    let auth = expected
        .iter()
        .find(|(k, _)| k == "authorization")
        .map(|(_, v)| v.clone())
        .unwrap();
    assert_eq!(s.headers["authorization"].to_str().unwrap(), auth, "{s:?}");
    assert_eq!(s.headers["content-type"], "application/json");
}

#[tokio::test]
async fn a_chat_client_reaches_claude_on_bedrock_signed_with_access_keys() {
    let (up, seen) = bedrock(Arc::new(|_| json_answer(200, converse_reply("晴")))).await;
    let (gw, _, mut rx) = gateway(with_keys(up)).await;
    let (status, body) = post(
        gw,
        "/v1/chat/completions",
        &[("authorization", "Bearer tw-k")],
        json!({"model": MODEL, "messages": [{"role": "user", "content": "天气？"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "晴");

    let s = seen.lock().unwrap()[0].clone();
    assert_eq!(s.uri, format!("/model/{MODEL}/converse"));
    let sent: Value = serde_json::from_slice(&s.body).unwrap();
    assert_eq!(sent["messages"][0]["content"][0]["text"], "天气？");
    assert!(
        s.headers["authorization"]
            .to_str()
            .unwrap()
            .starts_with("AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/"),
        "{s:?}"
    );
    assert_signed_as_sent(&s);

    match ending(&mut rx).await {
        tw_api::Event::RequestFinished { usage: Some(u), .. } => {
            assert_eq!((u.input, u.output), (11, 7));
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn an_api_key_goes_out_as_bearer_and_nothing_is_signed() {
    let (up, seen) = bedrock(Arc::new(|_| json_answer(200, converse_reply("ok")))).await;
    let (gw, _, _) = gateway(with_api_key(up)).await;
    let (status, body) = post(
        gw,
        "/v1/messages",
        &[("x-api-key", "tw-k")],
        json!({"model": MODEL, "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let s = seen.lock().unwrap()[0].clone();
    assert_eq!(s.headers["authorization"], "Bearer ABSK-test");
    assert!(s.headers.get("x-amz-date").is_none(), "{s:?}");
}

#[tokio::test]
async fn a_converse_stream_reaches_an_anthropic_client_as_its_own_sse() {
    let (up, seen) = bedrock(Arc::new(|_| {
        eventstream_answer(stream_frames(&["sun", "ny"]))
    }))
    .await;
    let (gw, _, mut rx) = gateway(with_keys(up)).await;
    let (status, body) = post(
        gw,
        "/v1/messages",
        &[("x-api-key", "tw-k")],
        json!({"model": MODEL, "max_tokens": 64, "stream": true,
               "messages": [{"role": "user", "content": "weather?"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        seen.lock().unwrap()[0].uri,
        format!("/model/{MODEL}/converse-stream")
    );
    let text: String = body
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter_map(|v| v["delta"]["text"].as_str().map(str::to_string))
        .collect();
    assert_eq!(text, "sunny", "{body}");
    assert!(body.contains("message_stop"), "{body}");
    match ending(&mut rx).await {
        tw_api::Event::RequestFinished { usage: Some(u), .. } => {
            assert_eq!((u.input, u.output), (21, 9));
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn an_exception_inside_the_stream_fails_the_request() {
    let mut wire = event("messageStart", json!({"role": "assistant"}));
    wire.extend(event(
        "contentBlockDelta",
        json!({"contentBlockIndex": 0, "delta": {"text": "半"}}),
    ));
    wire.extend(frame(
        "exception",
        "throttlingException",
        json!({"message": "Too many tokens, please wait before trying again."}),
    ));
    let (up, _) = bedrock(Arc::new(move |_| eventstream_answer(wire.clone()))).await;
    let (gw, _, mut rx) = gateway(with_keys(up)).await;
    let (status, body) = post(
        gw,
        "/v1/messages",
        &[("x-api-key", "tw-k")],
        json!({"model": MODEL, "max_tokens": 64, "stream": true,
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    // 200 早就发出去了，错误只能在流里说
    assert_eq!(status, 200);
    assert!(body.contains("event: error"), "{body}");
    assert!(body.contains("throttlingException"), "{body}");
    match ending(&mut rx).await {
        tw_api::Event::RequestFailed {
            message, source, ..
        } => {
            assert_eq!(message.code, "gw.upstream.stream_exception", "{message:?}");
            assert_eq!(message.arg("kind"), "throttlingException");
            // 限流按限流记：客户端该退避，不是这家坏了
            assert_eq!(source, tw_api::FailureSource::RateLimited);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_refused_credential_does_not_pass_on_what_aws_said_about_the_account() {
    let (up, _) = bedrock(Arc::new(|_| {
        axum::response::Response::builder()
            .status(403)
            .header("content-type", "application/json")
            .header(
                "x-amzn-errortype",
                "AccessDeniedException:http://internal.amazon.com/coral/com.amazon.bedrock/",
            )
            .body(axum::body::Body::from(
                json!({"message": "User: arn:aws:iam::123456789012:user/alice is not authorized to perform: bedrock:InvokeModel"}).to_string(),
            ))
            .unwrap()
    }))
    .await;
    let (gw, _, _) = gateway(with_keys(up)).await;
    let (status, body) = post(
        gw,
        "/v1/messages",
        &[("x-api-key", "tw-k")],
        json!({"model": MODEL, "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, 403);
    assert!(!body.contains("123456789012"), "{body}");
    assert!(!body.contains("alice"), "{body}");
    assert!(body.contains("AccessDeniedException"), "{body}");
    assert!(body.contains("[ThinkWatch]"), "{body}");
}

#[tokio::test]
async fn cache_points_and_the_betas_bedrock_takes_reach_it() {
    let (up, seen) = bedrock(Arc::new(|_| json_answer(200, converse_reply("ok")))).await;
    let (gw, _, _) = gateway(with_keys(up)).await;
    let (status, body) = post(
        gw,
        "/v1/messages",
        &[
            ("x-api-key", "tw-k"),
            (
                "anthropic-beta",
                "claude-code-20250219,interleaved-thinking-2025-05-14,oauth-2025-04-20",
            ),
        ],
        json!({
            "model": MODEL,
            "max_tokens": 16,
            "system": [{"type": "text", "text": "rules", "cache_control": {"type": "ephemeral"}}],
            "messages": [{"role": "user", "content": "hi"}]
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let s = seen.lock().unwrap()[0].clone();
    let sent: Value = serde_json::from_slice(&s.body).unwrap();
    assert_eq!(
        sent["system"],
        json!([{"text": "rules"}, {"cachePoint": {"type": "default"}}])
    );
    assert_eq!(
        sent["additionalModelRequestFields"]["anthropic_beta"],
        json!(["interleaved-thinking-2025-05-14"])
    );
    assert!(s.headers.get("anthropic-beta").is_none(), "{s:?}");
    assert_signed_as_sent(&s);
}

#[tokio::test]
async fn an_arn_model_keeps_its_slash_inside_the_path_and_the_signature_holds() {
    const ARN: &str = "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/a1b2c3";
    let (up, seen) = bedrock(Arc::new(|_| json_answer(200, converse_reply("ok")))).await;
    let (gw, _, _) = gateway(with_keys(up)).await;
    let (status, body) = post(
        gw,
        "/v1/messages",
        &[("x-api-key", "tw-k")],
        json!({"model": ARN, "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let s = seen.lock().unwrap()[0].clone();
    assert_eq!(
        s.uri,
        "/model/arn:aws:bedrock:us-east-1:123456789012:application-inference-profile%2Fa1b2c3/converse"
    );
    assert_signed_as_sent(&s);
}

#[tokio::test]
async fn a_placeholder_echoed_across_two_deltas_comes_back_as_the_secret() {
    const USER_KEY: &str = "sk-ant-api03-USERSOWNKEYAAAAAAAAAAAAAA";
    // 把收到的占位符拆成两半，分在两帧里回显
    let (up, seen) = bedrock(Arc::new(|s: &Seen| {
        let text = String::from_utf8_lossy(&s.body).to_string();
        let start = text.find("<<TW_").expect("the key went out unredacted");
        let end = start + text[start..].find(">>").unwrap() + 2;
        let placeholder = &text[start..end];
        let (a, b) = placeholder.split_at(placeholder.len() / 2);
        eventstream_answer(stream_frames(&["key 是 ", a, b]))
    }))
    .await;
    let security = Security {
        redact: RedactPolicy {
            mode: SecurityMode::Enforce,
            ..Default::default()
        },
        ..Default::default()
    };
    let (gw, _, _) = gateway_with(with_keys(up), security).await;
    let (status, body) = post(
        gw,
        "/v1/messages",
        &[("x-api-key", "tw-k")],
        json!({"model": MODEL, "max_tokens": 64, "stream": true,
               "messages": [{"role": "user", "content": format!("我的 key 是 {USER_KEY}")}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let sent = String::from_utf8_lossy(&seen.lock().unwrap()[0].body).to_string();
    assert!(!sent.contains(USER_KEY), "{sent}");
    let text: String = body
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter_map(|v| v["delta"]["text"].as_str().map(str::to_string))
        .collect();
    assert_eq!(text, format!("key 是 {USER_KEY}"), "{body}");
}

#[tokio::test]
async fn the_catalog_lists_foundation_models_and_both_kinds_of_inference_profile() {
    const ARN: &str = "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/a1b2c3";
    let (up, seen) = bedrock(Arc::new(|s: &Seen| {
        let path = s.uri.split('?').next().unwrap();
        let q = s.uri.split('?').nth(1).unwrap_or_default();
        match path {
            "/foundation-models" => json_answer(
                200,
                json!({"modelSummaries": [{"modelId": "amazon.nova-lite-v1:0"}]}),
            ),
            "/inference-profiles" if q.contains("type=APPLICATION") => json_answer(
                200,
                json!({"inferenceProfileSummaries": [{"inferenceProfileId": "a1b2c3", "inferenceProfileArn": ARN}]}),
            ),
            "/inference-profiles" => json_answer(
                200,
                json!({"inferenceProfileSummaries": [{"inferenceProfileId": MODEL}]}),
            ),
            _ => json_answer(404, json!({"message": "no"})),
        }
    }))
    .await;
    let (_, state, _) = gateway(with_keys(up)).await;
    let listing = tw_gateway::models::refresh_one(&state, "br").await.unwrap();
    assert_eq!(listing.models, ["amazon.nova-lite-v1:0", ARN, MODEL]);
    for s in seen.lock().unwrap().iter() {
        assert_eq!(s.method, "GET");
        assert!(
            s.headers["authorization"]
                .to_str()
                .unwrap()
                .starts_with("AWS4-HMAC-SHA256"),
            "{s:?}"
        );
    }
}

#[tokio::test]
async fn a_speed_test_signs_its_request_and_reads_the_converse_stream() {
    let (up, seen) = bedrock(Arc::new(|_| {
        eventstream_answer(stream_frames(&["Hi", "!"]))
    }))
    .await;
    let p = with_keys(up);
    let r = tw_gateway::l3::run(&reqwest::Client::new(), &p, &[], MODEL, Some(8)).await;
    assert!(r.ok, "{r:?}");
    assert!(r.ttft_ms.is_some(), "{r:?}");
    assert_eq!(r.output_tokens, Some(9), "{r:?}");
    let s = seen.lock().unwrap()[0].clone();
    assert_eq!(s.uri, format!("/model/{MODEL}/converse-stream"));
    assert_signed_as_sent(&s);
}

#[tokio::test]
async fn a_speed_test_reports_an_exception_inside_the_stream() {
    let mut wire = event("messageStart", json!({"role": "assistant"}));
    wire.extend(frame(
        "exception",
        "serviceUnavailableException",
        json!({"message": "Bedrock is unable to process your request."}),
    ));
    let (up, _) = bedrock(Arc::new(move |_| eventstream_answer(wire.clone()))).await;
    let r = tw_gateway::l3::run(&reqwest::Client::new(), &with_keys(up), &[], MODEL, Some(8)).await;
    assert!(!r.ok, "{r:?}");
    let e = r.error.unwrap();
    assert_eq!(e.code, "l3.stream_failed", "{e:?}");
    assert!(e.text.contains("serviceUnavailableException"), "{e:?}");
}

/// Anthropic 的数 token 到了 Bedrock 上游：回 501 `not_supported`，**什么都不发给 AWS**。
/// Claude Code 认这个回答，会改用一次 `max_tokens: 1` 的请求来数；回 400 的话它当成
/// 请求写错了
#[tokio::test]
async fn counting_tokens_is_not_supported_the_way_claude_code_expects() {
    let (up, seen) = bedrock(Arc::new(|s: &Seen| {
        let path = s.uri.split('?').next().unwrap();
        match path {
            "/foundation-models" => json_answer(200, json!({"modelSummaries": []})),
            "/inference-profiles" => json_answer(
                200,
                json!({"inferenceProfileSummaries": [{"inferenceProfileId": MODEL}]}),
            ),
            _ => json_answer(500, json!({"message": "nothing else should arrive"})),
        }
    }))
    .await;
    let (gw, state, _) = gateway(with_keys(up)).await;
    let body = json!({"model": MODEL, "messages": [{"role": "user", "content": "hi"}]});
    let count = |body: Value| {
        post(
            gw,
            "/v1/messages/count_tokens",
            &[("x-api-key", "tw-k")],
            body,
        )
    };

    // 模型清单还没取到：选上游时认出来
    let (status, text) = count(body.clone()).await;
    assert_eq!(status, 501, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["error"]["type"], "not_supported", "{text}");
    assert!(text.contains("upstream `br`"), "{text}");

    // 取到了：准入时就认出来
    tw_gateway::models::refresh_one(&state, "br").await.unwrap();
    let (status, text) = count(body).await;
    assert_eq!(status, 501, "{text}");
    assert!(
        text.contains("only AWS Bedrock upstreams serve it"),
        "{text}"
    );

    assert!(
        seen.lock().unwrap().iter().all(|s| s.method == "GET"),
        "counting tokens reached AWS"
    );
}
