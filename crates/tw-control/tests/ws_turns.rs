//! Responses 的 WebSocket 上**每一轮是一个请求**：存储层给每一轮记一行，带着这一轮回答的
//! 用量，**和 HTTP 的请求同一套查价**；密钥的用量上限按这一行结算。
//!
//! 网关、存储层、密钥用量的结算和 `twcore` 一样接（见 `tw_control::key_limits`）。上游是一个
//! 像 Responses 的 WebSocket 那样回答的假服务。

use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocketUpgrade};
use futures::{SinkExt, StreamExt};

/// 每一轮回答的用量：输入 1200（其中 1000 走了缓存）、输出 30。Responses 的输入数包含缓存读
const USAGE: &str = r#"{"input_tokens":1200,"input_tokens_details":{"cached_tokens":1000},"output_tokens":30,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":1230}"#;

/// 每个 `response.create` 回 created、一段文字、completed（带用量）
async fn upstream() -> SocketAddr {
    let app = axum::Router::new().route(
        "/v1/responses",
        axum::routing::any(|ws: WebSocketUpgrade| async move {
            ws.on_upgrade(|mut sock| async move {
                while let Some(Ok(m)) = sock.recv().await {
                    let Message::Text(_) = m else { continue };
                    let usage: serde_json::Value = serde_json::from_str(USAGE).unwrap();
                    let frames = [
                        serde_json::json!({"type":"response.created","response":{"id":"resp_1","status":"in_progress","model":"gpt-5","output":[]}}),
                        serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg","role":"assistant","content":[]}}),
                        serde_json::json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg","delta":"hello"}),
                        serde_json::json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","model":"gpt-5","output":[],"usage":usage}}),
                    ];
                    for f in frames {
                        if sock.send(Message::Text(f.to_string().into())).await.is_err() {
                            return;
                        }
                    }
                }
            })
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

/// 两轮，每一轮一行：用量是那一轮回答的，费用按 gpt-5 的价（$1.25/M 输入、$0.125/M 缓存读、
/// $10/M 输出）算：200 × 1.25 + 1000 × 0.125 + 30 × 10 = 675 微美元，**是实数不是估算**。密钥
/// 这一天的 token 和费用就是两行加起来
#[tokio::test]
async fn each_websocket_turn_is_recorded_priced_and_counted_against_the_key() {
    let up = upstream().await;
    let d = tempfile::tempdir().unwrap();
    let yaml = format!(
        "version: 1
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
clients:
  - name: codex
    key: tw-k
    limits:
      - {{ per: day, tokens: 100000 }}
      - {{ per: day, cost: 5 }}
providers:
  - name: openai
    base_url: http://{up}
    key: sk-x
    protocol: openai-responses
"
    );
    let cfg = tw_config::try_parse(&yaml).unwrap();
    let limits = cfg.clients[0].limits.clone();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let (_bodies, rx) = tokio::sync::mpsc::channel(1);
    let store = tw_store::task::spawn(
        tw_store::Recorder::new(
            tw_store::Db::open(&d.path().join("data.db")).unwrap(),
            tw_store::Blobs::new(d.path().join("blobs")),
            gw.pricing.clone(),
        )
        .settling_to(tw_control::key_limits::settle_hook(&gw)),
        gw.bus.subscribe(),
        rx,
    );
    let addr = tw_gateway::serve(gw.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = format!("ws://{addr}/v1/responses")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", "Bearer tw-k".parse().unwrap());
    let (mut c, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    for text in ["one", "two"] {
        let frame = serde_json::json!({
            "type": "response.create",
            "model": "gpt-5",
            "prompt_cache_key": "conversation-1",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": text}]}]
        });
        c.send(tokio_tungstenite::tungstenite::Message::Text(
            frame.to_string().into(),
        ))
        .await
        .unwrap();
        loop {
            let m = tokio::time::timeout(Duration::from_secs(5), c.next())
                .await
                .expect("the answer did not end")
                .unwrap()
                .unwrap();
            if m.into_text().unwrap().contains("response.completed") {
                break;
            }
        }
    }

    // 两行都落库了（落库之前已经结算过）
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let rows = loop {
        let rows = store.lock().await.db().recent(None, 10).unwrap();
        if rows.len() >= 2 {
            break rows;
        }
        assert!(std::time::Instant::now() < deadline, "rows: {rows:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    // 连接还开着：连接本身不留行，两行都是那两轮
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_ne!(rows[0].id, rows[1].id);
    for r in &rows {
        assert_eq!(
            (r.client.as_str(), r.provider.as_str()),
            ("codex", "openai")
        );
        assert_eq!(
            (r.model.as_str(), r.sent_model.as_str()),
            ("gpt-5", "gpt-5")
        );
        assert_eq!(r.path, "/v1/responses");
        assert_eq!(r.status, Some(200));
        assert_eq!(
            (r.input_tokens, r.cache_read_tokens, r.output_tokens),
            (Some(200), Some(1000), Some(30)),
            "{r:?}"
        );
        assert_eq!(r.cost_micros, Some(675), "{r:?}");
        assert!(!r.cost_estimated);
        assert!(r.ttft_ms.is_some(), "第一个 token 的时刻没记下：{r:?}");
        assert!(r.error.is_none() && !r.cancelled, "{r:?}");
        let routing = r.routing.as_deref().unwrap();
        assert!(routing.contains("\"provider\":\"openai\""), "{routing}");
    }
    // 同一段对话的两轮归到同一次会话里
    assert!(rows[0].session.is_some());
    assert_eq!(rows[0].session, rows[1].session);

    // 密钥这一天：两行的 token（没走缓存的输入 + 输出）和费用
    let view = gw.key_limits.view("codex", &limits);
    let used: Vec<(tw_api::LimitMeasure, u64)> = view.iter().map(|v| (v.measure, v.used)).collect();
    assert_eq!(
        used,
        [
            (tw_api::LimitMeasure::Tokens, 2 * (200 + 30)),
            (tw_api::LimitMeasure::Cost, 2 * 675),
        ]
    );
    drop(c);
}
