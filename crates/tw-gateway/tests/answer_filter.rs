//! 内容过滤查回答的正文（`scope` 含 `response`）：客户端收到的回答里，命中的字删掉、换掉，
//! 一个字都不送到客户端；观察档只记录。流着的回答不攒整份，只扣住可能还会长成一处命中的
//! 那一截；整份的回答整份查。查的是客户端那种格式、回答钩子之后的那一版，存下来的是客户端
//! 收到的那一份。
//!
//! 同一条规则也能查请求：替换在请求上一样，上游收到的是换过的。出站脱敏的规则可以只记录：
//! 找到的照样报，原值照发。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::routing::post;
use tw_api::{ContentOutcome, Event, OutcomeDetail, SecurityDirection};
use tw_config::{
    Client, Config, ContentAction, ContentPolicy, ContentScope, CustomContentRule, Listen,
    Provider, Security, SecurityMode,
};

const KEY: &str = "tw-reh4xqqrzyvbutjacvjywb4e";

/// 上游的回答正文：一处「北极星」切在两段增量里，一处整个在一段里
const PIECES: [&str; 4] = ["关于北", "极星的说", "明：北极星", "。"];

/// 假上游：Anthropic 的 `/v1/messages`，按请求的 `stream` 回流或整包，正文是 [`PIECES`]。
/// 流的字节**按七个字节一块**发，一个字常常切在两块里。记下收到的每一个请求体
struct Up {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl Up {
    fn last(&self) -> serde_json::Value {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("上游什么都没收到")
    }
}

fn anthropic_stream() -> String {
    let mut s = String::from(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n\
         event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    );
    for p in PIECES {
        s.push_str(&format!(
            "event: content_block_delta\ndata: {}\n\n",
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":p}})
        ));
    }
    s.push_str(
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
         event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n\
         event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    s
}

async fn upstream() -> Up {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sn = seen.clone();
    let app = Router::new().route(
        "/v1/messages",
        post(move |body: axum::body::Bytes| {
            let sn = sn.clone();
            async move {
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let stream = v["stream"].as_bool() == Some(true);
                sn.lock().unwrap().push(v);
                if stream {
                    let bytes = anthropic_stream().into_bytes();
                    let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
                        bytes.chunks(7).map(|c| Ok(c.to_vec())).collect();
                    axum::response::Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(axum::body::Body::from_stream(futures::stream::iter(chunks)))
                        .unwrap()
                } else {
                    axum::response::Response::builder()
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(
                            serde_json::json!({
                                "id": "msg", "type": "message", "role": "assistant",
                                "model": "claude-sonnet-4-5",
                                "content": [{"type": "text", "text": PIECES.concat()}],
                                "stop_reason": "end_turn",
                                "usage": {"input_tokens": 1, "output_tokens": 5}
                            })
                            .to_string(),
                        ))
                        .unwrap()
                }
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Up { addr, seen }
}

fn config(up: &Up, security: Security) -> Config {
    Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "claude-code".into(),
            key: KEY.into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "relay".into(),
            base_url: format!("http://{}", up.addr),
            key: Some("sk-upstream".into()),
            protocol: Some(tw_config::Protocol::Anthropic),
            ..Default::default()
        }],
        security,
        ..Default::default()
    }
}

fn rule(scope: Vec<ContentScope>, action: ContentAction) -> CustomContentRule {
    CustomContentRule {
        name: "项目代号".into(),
        pattern: "北极星".into(),
        scope,
        matching: Default::default(),
        case_sensitive: false,
        action,
        replacement: "***".into(),
        disabled: false,
    }
}

fn content(mode: SecurityMode, custom: Vec<CustomContentRule>) -> Security {
    Security {
        content: ContentPolicy {
            mode,
            custom,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// 只查回答、把「北极星」换成 `***` 的那一条
fn answers(mode: SecurityMode) -> Security {
    content(
        mode,
        vec![rule(vec![ContentScope::Response], ContentAction::Replace)],
    )
}

struct Sent {
    status: u16,
    body: String,
    evs: Vec<Event>,
    stored: Option<String>,
}

/// 发一个请求，读完回答、收齐这个请求的事件。交回存下来的客户端那一份回答（和上游原话不是
/// 一回事时才另存）
async fn send(cfg: Config, path: &str, body: serde_json::Value) -> Sent {
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let mut rx = state.bus.subscribe();
    let (tx, mut bodies) = tw_gateway::bodies::channel();
    state.set_body_sink(tx);
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let r = reqwest::Client::new()
        .post(format!("http://{addr}{path}"))
        .header("x-api-key", KEY)
        .header("authorization", format!("Bearer {KEY}"))
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body = r.text().await.unwrap();
    let mut evs = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
        let end = matches!(
            ev,
            Event::RequestFinished { .. } | Event::RequestFailed { .. }
        );
        evs.push(ev);
        if end {
            break;
        }
    }
    let mut stored = None;
    while let Ok(Some(rec)) = tokio::time::timeout(Duration::from_millis(500), bodies.recv()).await
    {
        if rec.kind == tw_gateway::bodies::BodyKind::ClientResponse {
            stored = Some(String::from_utf8(rec.for_disk().body.to_vec()).unwrap());
        }
    }
    Sent {
        status,
        body,
        evs,
        stored,
    }
}

fn claude(stream: bool, text: &str) -> serde_json::Value {
    serde_json::json!({
        "model": "claude-sonnet-4-5", "max_tokens": 64, "stream": stream,
        "messages": [{"role": "user", "content": text}]
    })
}

/// SSE 里所有的正文增量拼起来
fn streamed_text(body: &str, pick: impl Fn(&serde_json::Value) -> Option<String>) -> String {
    body.split("\n\n")
        .filter_map(|f| {
            f.lines()
                .find_map(|l| l.strip_prefix("data:"))
                .and_then(|d| serde_json::from_str::<serde_json::Value>(d.trim()).ok())
        })
        .filter_map(|v| pick(&v))
        .collect()
}

fn anthropic_text(v: &serde_json::Value) -> Option<String> {
    (v["delta"]["type"] == "text_delta").then(|| v["delta"]["text"].as_str().unwrap().to_string())
}

/// 回答里的命中：`(规则, 结果, 几处, 细节)`
fn answer_hits(evs: &[Event]) -> Vec<(String, ContentOutcome, u64, tw_api::SecurityHitDetail)> {
    evs.iter()
        .filter_map(|e| match e {
            Event::ContentMatched {
                direction: SecurityDirection::Response,
                rule,
                outcome,
                count,
                detail,
                ..
            } => Some((rule.clone(), *outcome, *count, detail.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_streamed_answer_has_the_matches_replaced_and_the_hit_is_logged() {
    let up = upstream().await;
    let r = send(
        config(&up, answers(SecurityMode::Enforce)),
        "/v1/messages",
        claude(true, "介绍一下"),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        streamed_text(&r.body, anthropic_text),
        "关于***的说明：***。"
    );
    assert!(!r.body.contains("北极星"), "命中的字到了客户端：{}", r.body);
    assert!(r.body.contains("message_stop"), "{}", r.body);
    let hits = answer_hits(&r.evs);
    assert_eq!(hits.len(), 1, "{:?}", r.evs);
    let (rule, outcome, count, detail) = &hits[0];
    assert_eq!(
        (rule.as_str(), *outcome, *count),
        ("项目代号", ContentOutcome::Replaced, 2)
    );
    assert_eq!(detail.direction, SecurityDirection::Response);
    assert_eq!(
        detail.outcome_detail,
        OutcomeDetail::Replaced {
            placeholders: vec![],
            segments: Some(2),
            replacement: Some("***".into())
        }
    );
    let at = &detail.locations[0];
    assert_eq!(at.part, tw_api::HitPart::ResponseText);
    assert_eq!(at.path, "content[0].text");
    assert_eq!(
        (at.before.as_str(), at.matched.as_str()),
        ("关于", "北极星")
    );
    // 命中在结局之前报：存储层那时还认得这个请求
    let matched = r
        .evs
        .iter()
        .position(|e| matches!(e, Event::ContentMatched { .. }))
        .unwrap();
    let finished = r
        .evs
        .iter()
        .position(|e| matches!(e, Event::RequestFinished { .. }))
        .unwrap();
    assert!(matched < finished);
    // 存下来的是客户端收到的那一份
    let stored = r.stored.expect("客户端收到的和上游原话不一样，要另存");
    assert!(
        stored.contains("***") && !stored.contains("北极星"),
        "{stored}"
    );
    // 请求这一边不查
    assert_eq!(up.last()["messages"][0]["content"], "介绍一下");
}

#[tokio::test]
async fn a_whole_answer_is_filtered_before_a_byte_of_it_is_sent() {
    let up = upstream().await;
    let r = send(
        config(&up, answers(SecurityMode::Enforce)),
        "/v1/messages",
        claude(false, "介绍一下"),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["content"][0]["text"], "关于***的说明：***。");
    let hits = answer_hits(&r.evs);
    assert_eq!((hits[0].1, hits[0].2), (ContentOutcome::Replaced, 2));
}

#[tokio::test]
async fn a_converted_answer_is_filtered_in_the_format_the_client_reads() {
    // Chat 的流
    let up = upstream().await;
    let r = send(
        config(&up, answers(SecurityMode::Enforce)),
        "/v1/chat/completions",
        serde_json::json!({"model": "claude-sonnet-4-5", "stream": true,
            "messages": [{"role": "user", "content": "介绍一下"}]}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let text = streamed_text(&r.body, |v| {
        v["choices"][0]["delta"]["content"]
            .as_str()
            .map(str::to_string)
    });
    assert_eq!(text, "关于***的说明：***。", "{}", r.body);
    assert!(r.body.trim_end().ends_with("data: [DONE]"), "{}", r.body);
    assert_eq!(
        answer_hits(&r.evs)[0].3.locations[0].path,
        "choices[0].message.content"
    );
    // Chat 的整包
    let up = upstream().await;
    let r = send(
        config(&up, answers(SecurityMode::Enforce)),
        "/v1/chat/completions",
        serde_json::json!({"model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": "介绍一下"}]}),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(
        v["choices"][0]["message"]["content"],
        "关于***的说明：***。"
    );
    // Gemini 不带 `alt=sse` 的流：一个逐个元素写出的 JSON 数组，过滤之后还是一个数组
    let up = upstream().await;
    let r = send(
        config(&up, answers(SecurityMode::Enforce)),
        "/v1beta/models/claude-sonnet-4-5:streamGenerateContent",
        serde_json::json!({"contents": [{"role": "user", "parts": [{"text": "介绍一下"}]}]}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let v: serde_json::Value = serde_json::from_str(&r.body).expect("still one JSON array");
    let text: String = v
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|e| {
            e["candidates"][0]["content"]["parts"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|p| p["text"].as_str().map(str::to_string))
        .collect();
    assert_eq!(text, "关于***的说明：***。");
}

#[tokio::test]
async fn observe_records_the_hit_and_changes_nothing() {
    let up = upstream().await;
    let r = send(
        config(&up, answers(SecurityMode::Observe)),
        "/v1/messages",
        claude(true, "介绍一下"),
    )
    .await;
    assert_eq!(streamed_text(&r.body, anthropic_text), PIECES.concat());
    let hits = answer_hits(&r.evs);
    assert_eq!((hits[0].1, hits[0].2), (ContentOutcome::Recorded, 2));
    assert_eq!(hits[0].3.outcome_detail, OutcomeDetail::Recorded {});
    assert!(
        r.stored.is_none(),
        "没改字：客户端那一份就是上游原话，不另存"
    );
    // 关着不查
    let up = upstream().await;
    let r = send(
        config(&up, answers(SecurityMode::Off)),
        "/v1/messages",
        claude(true, "介绍一下"),
    )
    .await;
    assert_eq!(streamed_text(&r.body, anthropic_text), PIECES.concat());
    assert!(answer_hits(&r.evs).is_empty());
}

#[tokio::test]
async fn one_rule_checks_both_sides_and_the_request_gets_the_replacement_too() {
    let up = upstream().await;
    let r = send(
        config(
            &up,
            content(
                SecurityMode::Enforce,
                vec![rule(
                    vec![ContentScope::Request, ContentScope::Response],
                    ContentAction::Replace,
                )],
            ),
        ),
        "/v1/messages",
        claude(true, "北极星项目的发布说明"),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(up.last()["messages"][0]["content"], "***项目的发布说明");
    assert_eq!(
        streamed_text(&r.body, anthropic_text),
        "关于***的说明：***。"
    );
    let sides: Vec<(SecurityDirection, ContentOutcome)> = r
        .evs
        .iter()
        .filter_map(|e| match e {
            Event::ContentMatched {
                direction, outcome, ..
            } => Some((*direction, *outcome)),
            _ => None,
        })
        .collect();
    assert_eq!(
        sides,
        [
            (SecurityDirection::Request, ContentOutcome::Replaced),
            (SecurityDirection::Response, ContentOutcome::Replaced)
        ]
    );
    // 只查请求的那条不查回答
    let up = upstream().await;
    let r = send(
        config(
            &up,
            content(
                SecurityMode::Enforce,
                vec![rule(vec![ContentScope::Request], ContentAction::Strip)],
            ),
        ),
        "/v1/messages",
        claude(true, "北极星项目"),
    )
    .await;
    assert_eq!(up.last()["messages"][0]["content"], "项目");
    assert_eq!(streamed_text(&r.body, anthropic_text), PIECES.concat());
}

/// 出站脱敏的规则只记录：找到的照样报，原值照发，回答里没有要换回来的
#[tokio::test]
async fn a_record_only_redaction_rule_reports_and_sends_the_value_as_it_is() {
    let up = upstream().await;
    let security = Security {
        redact: tw_config::RedactPolicy {
            mode: SecurityMode::Enforce,
            custom: vec![
                tw_config::CustomRedactRule {
                    name: "项目号".into(),
                    pattern: r"PRJ-\d{6}".into(),
                    matching: tw_config::ContentMatch::Regex,
                    case_sensitive: false,
                    label: None,
                    action: tw_config::RedactAction::Record,
                    disabled: false,
                },
                tw_config::CustomRedactRule {
                    name: "工号".into(),
                    pattern: r"E-\d{4}".into(),
                    matching: tw_config::ContentMatch::Regex,
                    case_sensitive: false,
                    label: None,
                    action: tw_config::RedactAction::Placeholder,
                    disabled: false,
                },
            ],
            ..Default::default()
        },
        ..Default::default()
    };
    let r = send(
        config(&up, security),
        "/v1/messages",
        claude(false, "PRJ-123456 和 E-1234"),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        up.last()["messages"][0]["content"],
        "PRJ-123456 和 <<TW_SECRET_1>>",
        "只记录的原样发，换成占位符的换"
    );
    let items: Vec<(String, bool, OutcomeDetail)> = r
        .evs
        .iter()
        .filter_map(|e| match e {
            Event::SecretsFound {
                items, replaced, ..
            } => {
                assert!(replaced, "事件的 replaced 说的是档位");
                Some(items.clone())
            }
            _ => None,
        })
        .flatten()
        .map(|i| (i.rule, i.replaced, i.detail.outcome_detail))
        .collect();
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!((items[0].0.as_str(), items[0].1), ("项目号", false));
    assert_eq!(items[0].2, OutcomeDetail::Recorded {});
    assert_eq!((items[1].0.as_str(), items[1].1), ("工号", true));
    assert!(
        matches!(&items[1].2, OutcomeDetail::Replaced { placeholders, .. } if placeholders == &["<<TW_SECRET_1>>"])
    );
}
