//! 推理强度，端到端：规则改写（`set.reasoning`），上游因为推理强度回 400 时同一家改一档再发
//! 一次、记下来（见 `tw_gateway::effort_memory`）。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use serde_json::{Value, json};

/// 一个假上游收到的请求体
type Seen = Arc<Mutex<Vec<Value>>>;

/// 一个假上游：按到达的顺序依次用 `replies` 回（用完了一直用最后一个），记下收到的请求体
async fn upstream(replies: Vec<(u16, Value)>) -> (SocketAddr, Seen) {
    let seen: Seen = Default::default();
    let app = Router::new()
        .fallback(move |State(s): State<Seen>, body: bytes::Bytes| {
            let replies = replies.clone();
            async move {
                let mut s = s.lock().unwrap();
                let n = s.len();
                s.push(serde_json::from_slice(&body).unwrap_or(Value::Null));
                let (status, reply) = replies[n.min(replies.len() - 1)].clone();
                axum::response::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(reply.to_string()))
                    .unwrap()
            }
        })
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, seen)
}

fn config(up: SocketAddr, protocol: &str, rules: &str) -> tw_config::Config {
    let routes = if rules.is_empty() {
        String::new()
    } else {
        format!("routes:\n  - name: default\n    rules:\n{rules}")
    };
    serde_yaml_ng::from_str(&format!(
        "version: 1
clients:
  - name: c
    key: tw-k
providers:
  - name: relay
    base_url: http://{up}
    key: sk-upstream
    protocol: {protocol}
{routes}"
    ))
    .unwrap()
}

struct Gw {
    addr: SocketAddr,
    state: tw_gateway::AppState,
    rx: tokio::sync::broadcast::Receiver<tw_api::Event>,
}

async fn gateway(cfg: tw_config::Config, dir: Option<&std::path::Path>) -> Gw {
    let state = tw_gateway::AppState::new(cfg).unwrap();
    if let Some(d) = dir {
        state.set_config_dir(d.to_path_buf());
    }
    let rx = state.bus.subscribe();
    let addr = tw_gateway::serve(state.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    Gw { addr, state, rx }
}

async fn post(gw: &Gw, path: &str, body: Value) -> (u16, String) {
    let resp = reqwest::Client::new()
        .post(format!("http://{}{path}", gw.addr))
        .header("content-type", "application/json")
        .header("x-api-key", "tw-k")
        .header("authorization", "Bearer tw-k")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap())
}

/// 这个请求的尝试链
async fn attempts(
    rx: &mut tokio::sync::broadcast::Receiver<tw_api::Event>,
) -> Vec<tw_api::AttemptView> {
    loop {
        let e = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("no routing event")
            .unwrap();
        if let tw_api::Event::RequestRouted { attempts, .. } = e {
            return attempts;
        }
    }
}

fn anthropic_ok() -> Value {
    json!({"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-opus-4-6",
           "content": [{"type": "text", "text": "好"}], "stop_reason": "end_turn",
           "usage": {"input_tokens": 10, "output_tokens": 1}})
}

fn chat_ok() -> Value {
    json!({"id": "c1", "object": "chat.completion", "model": "glm-5",
           "choices": [{"index": 0, "message": {"role": "assistant", "content": "好"}, "finish_reason": "stop"}],
           "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
}

fn anthropic_error(message: &str) -> Value {
    json!({"type": "error", "error": {"type": "invalid_request_error", "message": message}})
}

/// 自适应思考、写着 `effort` 的 Claude 请求
fn claude(effort: &str) -> Value {
    json!({
        "model": "claude-opus-4-6", "max_tokens": 2000,
        "thinking": {"type": "adaptive"}, "output_config": {"effort": effort},
        "messages": [{"role": "user", "content": "hi"}],
    })
}

fn effort_of(v: &Value) -> Option<&str> {
    v.pointer("/output_config/effort").and_then(Value::as_str)
}

fn change(
    asked: &str,
    sent: Option<&str>,
    reason: tw_api::EffortReason,
) -> Option<tw_api::EffortChange> {
    Some(tw_api::EffortChange {
        asked: asked.into(),
        sent: sent.map(str::to_string),
        reason,
    })
}

const NOT_VALID: &str = "The parameter `output_config.effort` specified in the request are not valid: expected `low`, `medium`, `high` or `max`, but got `xhigh` instead.";

/// 上游不认 `xhigh`、列出了认的那几档：同一家改用最近的 `max` 再发，是尝试链上新的一跳；记下来，
/// 下一个请求直接发 `max`。存在数据目录里
#[tokio::test]
async fn a_refused_effort_is_sent_again_once_with_the_nearest_level_and_remembered() {
    let dir = tempfile::tempdir().unwrap();
    let (up, seen) = upstream(vec![
        (400, anthropic_error(NOT_VALID)),
        (200, anthropic_ok()),
    ])
    .await;
    let mut gw = gateway(config(up, "anthropic", ""), Some(dir.path())).await;
    let (status, body) = post(&gw, "/v1/messages", claude("xhigh")).await;
    assert_eq!(status, 200, "{body}");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(effort_of(&seen[0]), Some("xhigh"));
        assert_eq!(effort_of(&seen[1]), Some("max"));
        // 别的照旧
        assert_eq!(seen[1]["thinking"], json!({"type": "adaptive"}));
        assert_eq!(seen[1]["messages"], seen[0]["messages"]);
    }
    let chain = attempts(&mut gw.rx).await;
    assert_eq!(chain.len(), 2, "{chain:?}");
    assert_eq!(chain[0].outcome, tw_api::AttemptOutcome::Status);
    assert_eq!(chain[0].status, Some(400));
    assert_eq!(chain[0].effort, None, "被拒的那一跳发的就是客户端要的");
    assert_eq!(chain[1].outcome, tw_api::AttemptOutcome::Served);
    assert_eq!(chain[1].provider, "relay");
    assert_eq!(chain[1].retry, None, "不是退避重试");
    assert_eq!(
        chain[1].effort,
        change("xhigh", Some("max"), tw_api::EffortReason::Rejected)
    );

    // 记住了：直接发 max，上游只收到一次
    let (status, _) = post(&gw, "/v1/messages", claude("xhigh")).await;
    assert_eq!(status, 200);
    assert_eq!(seen.lock().unwrap().len(), 3);
    assert_eq!(effort_of(&seen.lock().unwrap()[2]), Some("max"));
    let chain = attempts(&mut gw.rx).await;
    assert_eq!(chain.len(), 1);
    assert_eq!(
        chain[0].effort,
        change("xhigh", Some("max"), tw_api::EffortReason::Remembered)
    );
    // 别的档位不受影响
    post(&gw, "/v1/messages", claude("high")).await;
    assert_eq!(effort_of(&seen.lock().unwrap()[3]), Some("high"));
    assert_eq!(attempts(&mut gw.rx).await[0].effort, None);

    // 存在数据目录里（写文件挪到了阻塞线程上，等它一下）
    let file = dir.path().join(tw_gateway::effort_memory::FILE);
    let mut text = String::new();
    for _ in 0..50 {
        text = std::fs::read_to_string(&file).unwrap_or_default();
        if !text.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        text.contains("claude-opus-4-6") && text.contains("\"use\": \"max\""),
        "{text}"
    );
    assert!(!text.contains("sk-upstream"), "{text}");

    // 换了密钥：已经是另一个上游了，记着的作废，再试原来那一档
    let mut cfg = gw.state.config().as_ref().clone();
    cfg.providers[0].key = Some("sk-other".into());
    gw.state.reload(cfg).unwrap();
    post(&gw, "/v1/messages", claude("xhigh")).await;
    assert_eq!(effort_of(&seen.lock().unwrap()[4]), Some("xhigh"));
}

/// 「high 或以下」是上限：改用 high
#[tokio::test]
async fn a_ceiling_in_the_refusal_is_the_level_sent_again() {
    let (up, seen) = upstream(vec![
        (
            400,
            anthropic_error(
                "output_config.effort 'xhigh' is not supported when thinking is disabled on this model. Use effort 'high' or below, or enable thinking.",
            ),
        ),
        (200, anthropic_ok()),
    ])
    .await;
    let mut gw = gateway(config(up, "anthropic", ""), None).await;
    let (status, body) = post(&gw, "/v1/messages", claude("xhigh")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(effort_of(&seen.lock().unwrap()[1]), Some("high"));
    let chain = attempts(&mut gw.rx).await;
    assert_eq!(
        chain[1].effort,
        change("xhigh", Some("high"), tw_api::EffortReason::Rejected)
    );
}

/// 不提 `effort`、只列了几档的（GLM 始终思考的模型，码 1210）：离 medium 最近的 low 和 high
/// 一样近，取 high
#[tokio::test]
async fn levels_listed_without_the_word_effort_are_understood_too() {
    let refusal = json!({"error": {"code": "1210",
        "message": "该模型始终思考，不支持关闭思考；请使用 low、high 或 max。"}});
    let (up, seen) = upstream(vec![(400, refusal), (200, chat_ok())]).await;
    let mut gw = gateway(config(up, "openai-chat", ""), None).await;
    let (status, body) = post(
        &gw,
        "/v1/chat/completions",
        json!({"model": "glm-5", "reasoning_effort": "medium",
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1]["reasoning_effort"], "high");
    }
    let chain = attempts(&mut gw.rx).await;
    assert_eq!(
        chain[1].effort,
        change("medium", Some("high"), tw_api::EffortReason::Rejected)
    );
}

/// 与推理强度无关的 400：原样交给客户端，不重发
#[tokio::test]
async fn another_400_is_not_sent_again() {
    let reply = anthropic_error("messages.0.content: field required");
    let (up, seen) = upstream(vec![(400, reply.clone()), (200, anthropic_ok())]).await;
    let mut gw = gateway(config(up, "anthropic", ""), None).await;
    let (status, body) = post(&gw, "/v1/messages", claude("xhigh")).await;
    assert_eq!(status, 400);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), reply);
    assert_eq!(seen.lock().unwrap().len(), 1);
    let chain = attempts(&mut gw.rx).await;
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].outcome, tw_api::AttemptOutcome::Served);
}

/// 原话里碰巧有发出去的那一档的名字（「max context length」），又说到了思考：照规矩重发一次，再被拒就把那个
/// 错误交给客户端，**而且不记**：下一个请求照样发 max
#[tokio::test]
async fn a_resend_refused_again_is_handed_on_and_not_remembered() {
    let reply =
        anthropic_error("prompt is too long for extended thinking: max context length exceeded");
    let (up, seen) = upstream(vec![(400, reply.clone())]).await;
    let mut gw = gateway(config(up, "anthropic", ""), None).await;
    let (status, body) = post(&gw, "/v1/messages", claude("max")).await;
    assert_eq!(status, 400);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), reply);
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "只重发一次");
        assert_eq!(effort_of(&seen[0]), Some("max"));
        assert_eq!(effort_of(&seen[1]), None, "说不出用哪一档：去掉强度");
    }
    let chain = attempts(&mut gw.rx).await;
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[1].status, Some(400));
    assert_eq!(
        chain[1].effort,
        change("max", None, tw_api::EffortReason::Rejected)
    );
    assert!(!gw.state.effort_memory.has(
        "relay",
        "claude-opus-4-6",
        tw_dialect::ir::Dialect::Anthropic
    ));
    post(&gw, "/v1/messages", claude("max")).await;
    let seen = seen.lock().unwrap();
    assert_eq!(effort_of(&seen[2]), Some("max"), "没有记住");
}

/// 规则改写推理：改一档只改开着推理的请求，`off` 连强度一起去掉，没开推理的原样不动
#[tokio::test]
async fn a_rule_rewrites_reasoning_only_where_it_is_on() {
    let ok = anthropic_ok();
    let (up, seen) = upstream(vec![(200, ok)]).await;
    let rules =
        "      - { name: 想多点, when: { model: claude-opus-4-6 }, set: { reasoning: xhigh } }
      - { name: 别想, when: { model: claude-haiku-4-5 }, set: { reasoning: off } }
      - { name: 兜底, to: relay }
";
    let mut gw = gateway(config(up, "anthropic", rules), None).await;
    post(&gw, "/v1/messages", claude("high")).await;
    assert_eq!(effort_of(&seen.lock().unwrap()[0]), Some("xhigh"));
    assert_eq!(
        attempts(&mut gw.rx).await[0].effort,
        change("high", Some("xhigh"), tw_api::EffortReason::Rule)
    );
    // 没开推理的（生成标题这类）：一个字节都不动
    let plain = json!({"model": "claude-opus-4-6", "max_tokens": 50,
                       "messages": [{"role": "user", "content": "起个标题"}]});
    post(&gw, "/v1/messages", plain.clone()).await;
    assert_eq!(seen.lock().unwrap()[1], plain);
    assert_eq!(attempts(&mut gw.rx).await[0].effort, None);
    // off：开关和强度一起去掉
    let mut haiku = claude("high");
    haiku["model"] = json!("claude-haiku-4-5");
    post(&gw, "/v1/messages", haiku).await;
    {
        let seen = seen.lock().unwrap();
        assert!(seen[2].get("thinking").is_none(), "{}", seen[2]);
        assert!(seen[2].get("output_config").is_none(), "{}", seen[2]);
    }
    assert_eq!(
        attempts(&mut gw.rx).await[0].effort,
        change("high", None, tw_api::EffortReason::Rule)
    );
}

/// 规则定的那一档上游不认：照样改一档重发（自动兼容在规则之后）
#[tokio::test]
async fn a_level_set_by_a_rule_is_fixed_like_any_other() {
    let (up, seen) = upstream(vec![
        (400, anthropic_error(NOT_VALID)),
        (200, anthropic_ok()),
    ])
    .await;
    let rules = "      - { name: 想多点, set: { reasoning: xhigh } }
      - { name: 兜底, to: relay }
";
    let mut gw = gateway(config(up, "anthropic", rules), None).await;
    let (status, _) = post(&gw, "/v1/messages", claude("low")).await;
    assert_eq!(status, 200);
    {
        let seen = seen.lock().unwrap();
        assert_eq!(effort_of(&seen[0]), Some("xhigh"));
        assert_eq!(effort_of(&seen[1]), Some("max"));
    }
    let chain = attempts(&mut gw.rx).await;
    assert_eq!(
        chain[0].effort,
        change("low", Some("xhigh"), tw_api::EffortReason::Rule)
    );
    assert_eq!(
        chain[1].effort,
        change("xhigh", Some("max"), tw_api::EffortReason::Rejected),
        "被拒的是规则改出来的 xhigh"
    );
}

/// 转换过格式的请求一样：改的是转成上游格式之后的那一份
#[tokio::test]
async fn a_converted_request_is_fixed_in_the_upstream_format() {
    let (up, seen) = upstream(vec![
        (400, anthropic_error(NOT_VALID)),
        (200, anthropic_ok()),
    ])
    .await;
    let mut gw = gateway(config(up, "anthropic", ""), None).await;
    let (status, body) = post(
        &gw,
        "/v1/chat/completions",
        json!({"model": "claude-opus-4-6", "reasoning_effort": "xhigh",
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(effort_of(&seen[0]), Some("xhigh"));
        assert_eq!(effort_of(&seen[1]), Some("max"));
    }
    let chain = attempts(&mut gw.rx).await;
    assert_eq!(
        chain[1].effort,
        change("xhigh", Some("max"), tw_api::EffortReason::Rejected)
    );
}

/// 关着思考、写着强度的请求（Anthropic 说「关着思考时不认 xhigh，用 high 或以下」）：按写着的
/// 字段认，照样改一档重发
#[tokio::test]
async fn an_effort_written_with_thinking_off_is_fixed_too() {
    let (up, seen) = upstream(vec![
        (
            400,
            anthropic_error(
                "output_config.effort 'xhigh' is not supported when thinking is disabled on this model. Use effort 'high' or below, or enable thinking.",
            ),
        ),
        (200, anthropic_ok()),
    ])
    .await;
    let mut gw = gateway(config(up, "anthropic", ""), None).await;
    let mut body = claude("xhigh");
    body["thinking"] = json!({"type": "disabled"});
    let (status, text) = post(&gw, "/v1/messages", body).await;
    assert_eq!(status, 200, "{text}");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(effort_of(&seen[1]), Some("high"));
        assert_eq!(seen[1]["thinking"], json!({"type": "disabled"}));
    }
    let chain = attempts(&mut gw.rx).await;
    assert_eq!(
        chain[1].effort,
        change("xhigh", Some("high"), tw_api::EffortReason::Rejected)
    );
}

/// 记着的改法自己被拒了：那一条删掉，照新的原话再改一次，成了记新的；重发成了用的正是原来那一档
/// 的（上游又认了）：删掉，往后照原样发
#[tokio::test]
async fn a_remembered_fix_that_is_refused_is_replaced_or_forgotten() {
    let ceiling = anthropic_error(
        "effort 'max' is not supported when thinking is disabled. Use effort 'high' or below.",
    );
    let back = anthropic_error("effort 'max' is not available for this model: use xhigh");
    let (up, seen) = upstream(vec![
        (400, anthropic_error(NOT_VALID)),
        (200, anthropic_ok()),
        (400, ceiling),
        (200, anthropic_ok()),
        (200, anthropic_ok()),
    ])
    .await;
    let mut gw = gateway(config(up, "anthropic", ""), None).await;
    let e = |i: usize| effort_of(&seen.lock().unwrap()[i]).map(str::to_string);
    // 记下 xhigh → max
    post(&gw, "/v1/messages", claude("xhigh")).await;
    attempts(&mut gw.rx).await;
    // max 自己被拒，上限 high：改记 xhigh → high
    let (status, _) = post(&gw, "/v1/messages", claude("xhigh")).await;
    assert_eq!(status, 200);
    assert_eq!((e(2), e(3)), (Some("max".into()), Some("high".into())));
    let chain = attempts(&mut gw.rx).await;
    assert_eq!(
        chain[0].effort,
        change("xhigh", Some("max"), tw_api::EffortReason::Remembered)
    );
    assert_eq!(
        chain[1].effort,
        change("xhigh", Some("high"), tw_api::EffortReason::Rejected),
        "asked 是改之前写明的那一档"
    );
    post(&gw, "/v1/messages", claude("xhigh")).await;
    assert_eq!(e(4), Some("high".into()));
    attempts(&mut gw.rx).await;

    // 另一家：记着 xhigh → max，max 被拒、原话让用 xhigh，重发 xhigh 成了 —— 删掉
    let (up, seen) = upstream(vec![
        (400, anthropic_error(NOT_VALID)),
        (200, anthropic_ok()),
        (400, back),
        (200, anthropic_ok()),
    ])
    .await;
    let mut gw = gateway(config(up, "anthropic", ""), None).await;
    let e = |i: usize| effort_of(&seen.lock().unwrap()[i]).map(str::to_string);
    post(&gw, "/v1/messages", claude("xhigh")).await;
    attempts(&mut gw.rx).await;
    post(&gw, "/v1/messages", claude("xhigh")).await;
    assert_eq!((e(2), e(3)), (Some("max".into()), Some("xhigh".into())));
    attempts(&mut gw.rx).await;
    assert!(!gw.state.effort_memory.has(
        "relay",
        "claude-opus-4-6",
        tw_dialect::ir::Dialect::Anthropic
    ));
    post(&gw, "/v1/messages", claude("xhigh")).await;
    assert_eq!(e(4), Some("xhigh".into()));
}

/// 只写了预算的请求没有写明的强度：上游的 400 说到了思考和档位名也不重发、不记
#[tokio::test]
async fn a_budget_only_request_is_not_an_effort_rejection() {
    let reply = anthropic_error(
        "thinking.budget_tokens: the max is lower for this model; use low, medium or high effort instead",
    );
    let (up, seen) = upstream(vec![(400, reply.clone()), (200, anthropic_ok())]).await;
    let gw = gateway(config(up, "anthropic", ""), None).await;
    let (status, _) = post(
        &gw,
        "/v1/messages",
        json!({"model": "claude-sonnet-4-5", "max_tokens": 64000,
               "thinking": {"type": "enabled", "budget_tokens": 50000},
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert!(!gw.state.effort_memory.has(
        "relay",
        "claude-sonnet-4-5",
        tw_dialect::ir::Dialect::Anthropic
    ));
    // 记着这个模型不认 xhigh，预算折算出来正是 xhigh 的请求也不改：预算不是写明的强度
    let p = gw.state.config().providers[0].clone();
    gw.state.effort_memory.note(
        &p,
        "claude-sonnet-4-5",
        tw_dialect::ir::Dialect::Anthropic,
        tw_dialect::params::WrittenEffort::Level(tw_dialect::ir::Effort::XHigh),
        tw_dialect::think::EffortFix::Use(tw_dialect::ir::Effort::Low),
        tw_gateway::effort_memory::now_secs(),
    );
    post(
        &gw,
        "/v1/messages",
        json!({"model": "claude-sonnet-4-5", "max_tokens": 64000,
               "thinking": {"type": "enabled", "budget_tokens": 50000},
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(seen.lock().unwrap()[1]["thinking"]["budget_tokens"], 50000);
}

/// 重发马上发给同一家：排在它后面的候选一个都不碰，这家只许一个并发也不用等
#[tokio::test]
async fn the_resend_goes_to_the_same_upstream_at_once() {
    let (up, seen) = upstream(vec![
        (400, anthropic_error(NOT_VALID)),
        (200, anthropic_ok()),
    ])
    .await;
    let (backup, backup_seen) = upstream(vec![(200, anthropic_ok())]).await;
    let cfg: tw_config::Config = serde_yaml_ng::from_str(&format!(
        "version: 1
clients:
  - name: c
    key: tw-k
providers:
  - name: relay
    base_url: http://{up}
    key: sk-upstream
    protocol: anthropic
    max_concurrent: 1
  - name: backup
    base_url: http://{backup}
    key: sk-backup
    protocol: anthropic
groups:
  - name: both
    type: fallback
    providers: [relay, backup]
routes:
  - name: default
    rules:
      - {{ name: 兜底, to: both }}
"
    ))
    .unwrap();
    let mut gw = gateway(cfg, None).await;
    let (status, body) = post(&gw, "/v1/messages", claude("xhigh")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert!(backup_seen.lock().unwrap().is_empty());
    let chain = attempts(&mut gw.rx).await;
    assert_eq!(
        chain
            .iter()
            .map(|a| a.provider.as_str())
            .collect::<Vec<_>>(),
        ["relay", "relay"]
    );
    assert_eq!(chain[1].queued_ms, None, "没有排队");
}

/// `off` 在 OpenAI 的格式里写 `none`（去掉的话 GPT-5 这类按默认的那一档想）；上游不认 `none`
/// 的去掉再发，记下来，往后直接去掉
#[tokio::test]
async fn off_writes_none_and_an_upstream_that_refuses_it_gets_the_field_removed() {
    let refusal = json!({"error": {"message": "Unsupported value: 'none' is not supported with this model for reasoning_effort. Supported values are: 'low', 'medium', and 'high'.",
        "type": "invalid_request_error", "param": "reasoning_effort", "code": "unsupported_value"}});
    let (up, seen) = upstream(vec![(400, refusal), (200, chat_ok())]).await;
    let rules = "      - { name: 别想, set: { reasoning: off } }
      - { name: 兜底, to: relay }
";
    let mut gw = gateway(config(up, "openai-chat", rules), None).await;
    let ask = json!({"model": "o3-mini", "reasoning_effort": "high",
                     "messages": [{"role": "user", "content": "hi"}]});
    let (status, body) = post(&gw, "/v1/chat/completions", ask.clone()).await;
    assert_eq!(status, 200, "{body}");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0]["reasoning_effort"], "none");
        assert!(seen[1].get("reasoning_effort").is_none(), "{}", seen[1]);
    }
    let chain = attempts(&mut gw.rx).await;
    assert_eq!(
        chain[0].effort,
        change("high", Some("none"), tw_api::EffortReason::Rule)
    );
    assert_eq!(
        chain[1].effort,
        change("none", None, tw_api::EffortReason::Rejected)
    );
    post(&gw, "/v1/chat/completions", ask).await;
    assert!(seen.lock().unwrap()[2].get("reasoning_effort").is_none());
    assert_eq!(
        attempts(&mut gw.rx).await[0].effort,
        change("none", None, tw_api::EffortReason::Remembered)
    );
}
