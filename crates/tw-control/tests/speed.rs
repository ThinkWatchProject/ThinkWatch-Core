//! 推理测速（L3）走控制面的那一段。
//!
//! 这一层花钱，所以两件事必须和转发时完全一致：**请求经过哪条出站路径**、
//! **请求长什么样**。任何一处不一致，测出来的结论就是关于另一条路的 ——
//! 而用户会据此去换一个其实没问题的上游。

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, Uri};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

struct Bed {
    _dir: tempfile::TempDir,
    app: axum::Router,
}

fn bed(yaml: &str) -> Bed {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, yaml).unwrap();
    let cfg: tw_config::Config = serde_yaml_ng::from_str(yaml).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let state = ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        store: None,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    };
    Bed {
        app: tw_control::router(state),
        _dir: d,
    }
}

async fn post(app: &axum::Router, path: &str, body: &str) -> (StatusCode, serde_json::Value) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let st = r.status();
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (
        st,
        serde_json::from_slice(&b).unwrap_or(serde_json::Value::Null),
    )
}

/// 收到了什么请求。
#[derive(Default, Clone)]
struct Seen {
    path: String,
    host: String,
    anthropic_version: Option<String>,
}

/// 一个假的 HTTP 代理，同时扮演上游。
///
/// **上游地址故意写成解析不了的域名**：请求只有真的经过代理才能到达这里。
/// 走默认 client 的话，它会在 DNS 那一步失败 —— 测试要抓的正是这个。
async fn proxy_that_answers(seen: Arc<Mutex<Option<Seen>>>) -> std::net::SocketAddr {
    let app = axum::Router::new().fallback(move |uri: Uri, headers: HeaderMap| {
        let seen = seen.clone();
        async move {
            *seen.lock().unwrap() = Some(Seen {
                path: uri.path().to_string(),
                host: headers
                    .get("host")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string(),
                anthropic_version: headers
                    .get("anthropic-version")
                    .and_then(|v| v.to_str().ok())
                    .map(String::from),
            });
            let sse = concat!(
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":2}}\n\n",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            );
            ([("content-type", "text/event-stream")], sse)
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app).await.unwrap();
    });
    addr
}

#[tokio::test]
async fn the_speed_run_goes_out_through_the_upstreams_own_proxy() {
    let seen = Arc::new(Mutex::new(None));
    let proxy = proxy_that_answers(seen.clone()).await;
    let yaml = format!(
        "version: 1
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
clients:
  - name: c
    key: tw-k
proxies:
  - name: corp
    type: http
    addr: {proxy}
providers:
  - name: relay
    base_url: http://relay.speed-test.invalid
    key: sk-x
    protocol: anthropic
    proxy: corp
"
    );
    let b = bed(&yaml);
    let (st, v) = post(
        &b.app,
        "/speed/run",
        r#"{"providers":["relay"],"model":"claude-sonnet-4-5"}"#,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let r = &v[0];
    assert_eq!(r["ok"], true, "没有经过代理，请求到不了上游：{v}");

    let seen = seen.lock().unwrap().clone().expect("代理没收到请求");
    assert_eq!(seen.path, "/v1/messages");
    assert_eq!(seen.host, "relay.speed-test.invalid");
    // 官方端点没有它就回 400，而那会被报成「这家不通」
    assert_eq!(seen.anthropic_version.as_deref(), Some("2023-06-01"));
}

#[tokio::test]
async fn every_upstream_is_quoted_by_its_price_sheet_and_free_is_zero() {
    // 订阅账号也按价目表报价；不计费的那一家报 0，合计照样算得出来
    let b = bed("version: 1
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
clients:
  - name: c
    key: tw-k
providers:
  - name: official
    base_url: https://api.anthropic.com
    key: sk-x
  - name: max
    base_url: https://api.anthropic.com
    key: sk-y
  - name: local
    base_url: http://127.0.0.1:11434
    key: sk-z
    protocol: anthropic
    billing: free
");
    let (st, v) = post(&b.app, "/speed/quote", r#"{"model":"claude-sonnet-4-5"}"#).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let items = v["items"].as_array().unwrap();
    let pick = |name: &str| items.iter().find(|i| i["provider"] == name).unwrap();
    let (official, max, local) = (pick("official"), pick("max"), pick("local"));
    assert_eq!(max["billing"], "per-token", "{v}");
    assert_eq!(
        max["cost_micros"], official["cost_micros"],
        "同一张价目表报的价不一样：{v}"
    );
    assert_eq!(
        (local["billing"].as_str(), local["cost_micros"].as_i64()),
        (Some("free"), Some(0)),
        "{v}"
    );
    assert_eq!(
        v["total_micros"].as_i64(),
        Some(official["cost_micros"].as_i64().unwrap() * 2),
        "合计是两家按量计费的加上不计费的 0：{v}"
    );
}

#[tokio::test]
async fn a_quote_covers_the_chosen_upstreams_and_marks_the_ones_that_cannot_serve_the_model() {
    let b = bed("version: 1
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
clients:
  - name: c
    key: tw-k
providers:
  - name: official
    base_url: https://api.anthropic.com
    key: sk-x
  - name: haiku-only
    base_url: https://relay.example
    key: sk-y
    models_only: [claude-haiku-*]
  - name: third
    base_url: https://relay.example
    key: sk-z
");
    let (st, v) = post(
        &b.app,
        "/speed/quote",
        r#"{"providers":["official","haiku-only"],"model":"claude-sonnet-4-5"}"#,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "只报点名的那几家：{v}");
    let official = items.iter().find(|i| i["provider"] == "official").unwrap();
    let scoped = items
        .iter()
        .find(|i| i["provider"] == "haiku-only")
        .unwrap();
    assert_eq!(scoped["skipped"], "out_of_scope", "{v}");
    assert!(official.get("skipped").is_none(), "{v}");
    // 不会被测的那一家不进合计
    assert_eq!(v["total_micros"], official["cost_micros"], "{v}");

    // 服务不了的不发请求：这里一个请求都不会发出去
    let (st, v) = post(
        &b.app,
        "/speed/run",
        r#"{"providers":["haiku-only"],"model":"claude-sonnet-4-5"}"#,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v, serde_json::json!([]));

    let (st, _) = post(
        &b.app,
        "/speed/quote",
        r#"{"providers":["没有这家"],"model":"claude-sonnet-4-5"}"#,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}
