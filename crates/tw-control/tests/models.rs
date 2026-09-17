//! 上游的模型：清单从哪儿来、哪些在启用范围里、怎么计价；停用和启用范围
//! 怎么写进配置；试算怎么说出被跳过的上游。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

struct Bed {
    dir: tempfile::TempDir,
    app: axum::Router,
}

impl Bed {
    fn file(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("config.yaml")).unwrap()
    }
}

fn bed(yaml: &str) -> Bed {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, yaml).unwrap();
    let cfg = tw_config::try_parse(yaml).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let state = ControlState {
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        gateway_addr: None,
        store: None,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        // **测试里绝不能碰开发者自己的配置**
        home: d.path().join("home"),
    };
    Bed {
        app: tw_control::router(state),
        dir: d,
    }
}

async fn call(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let st = r.status();
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    let text = String::from_utf8_lossy(&b).to_string();
    let v = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
    (st, v)
}

/// 一个假上游：认 `sk-good`，列出三个模型。
async fn upstream() -> std::net::SocketAddr {
    let app = axum::Router::new().route(
        "/v1/models",
        axum::routing::get(|h: axum::http::HeaderMap| async move {
            if h.get("x-api-key").and_then(|v| v.to_str().ok()) != Some("sk-good") {
                return (StatusCode::UNAUTHORIZED, "{}".to_string());
            }
            (
                StatusCode::OK,
                r#"{"data":[{"id":"claude-sonnet-4-5"},{"id":"claude-haiku-4-5"},{"id":"中转自有模型"}]}"#
                    .to_string(),
            )
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

fn config(up: std::net::SocketAddr, key: &str, extra: &str) -> String {
    format!(
        "version: 1
clients:
  - name: c
    key: tw-k
providers:
  - name: relay
    base_url: http://{up}
    key: {key}
    protocol: anthropic
{extra}"
    )
}

#[tokio::test]
async fn an_upstreams_models_come_with_their_scope_context_window_and_price() {
    let up = upstream().await;
    let b = bed(&config(
        up,
        "sk-good",
        "    models_only: [claude-sonnet-*]\n",
    ));

    // 还没问过：不知道它有什么
    let (st, v) = call(
        &b.app,
        "GET",
        "/providers/relay/models",
        serde_json::json!(null),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["source"], "none");
    assert!(v.get("checked_at_ms").is_none(), "{v}");

    let (st, v) = call(
        &b.app,
        "POST",
        "/providers/relay/models/refresh",
        serde_json::json!(null),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["source"], "discovered");
    assert!(v["checked_at_ms"].as_u64().is_some(), "{v}");
    let models = v["models"].as_array().unwrap();
    let find = |id: &str| models.iter().find(|m| m["id"] == id).unwrap().clone();
    let sonnet = find("claude-sonnet-4-5");
    assert_eq!(sonnet["enabled"], true);
    assert_eq!(sonnet["context_window"], 200_000);
    assert_eq!(sonnet["price"]["input"], 3.0);
    assert_eq!(sonnet["price_source"]["kind"], "default");
    assert_eq!(find("claude-haiku-4-5")["enabled"], false);
    // 无法计价就是无法计价
    assert!(find("中转自有模型").get("price").is_none(), "{v}");

    let (_, ov) = call(&b.app, "GET", "/overview", serde_json::json!(null)).await;
    let p = &ov["providers"][0];
    assert_eq!(p["model_source"], "discovered");
    assert_eq!(p["model_count"], 1, "范围外的不算：{p}");
    assert_eq!(p["models_only"], serde_json::json!(["claude-sonnet-*"]));
    assert_eq!(p["disabled"], false);
}

#[tokio::test]
async fn a_refresh_the_upstream_refuses_says_why_and_the_manual_list_stands_in() {
    let up = upstream().await;
    let b = bed(&config(up, "sk-bad", "    models: [手写的模型]\n"));
    let (st, v) = call(
        &b.app,
        "POST",
        "/providers/relay/models/refresh",
        serde_json::json!(null),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["source"], "manual");
    assert!(v["error"].as_str().unwrap().contains("401"), "{v}");
    assert_eq!(v["models"][0]["id"], "手写的模型");

    let (st, _) = call(
        &b.app,
        "GET",
        "/providers/没有这家/models",
        serde_json::json!(null),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn disabling_and_scoping_an_upstream_is_written_and_an_empty_scope_is_refused() {
    let up = upstream().await;
    let b = bed(&config(up, "sk-good", ""));
    let (st, v) = call(
        &b.app,
        "PUT",
        "/providers/relay",
        serde_json::json!({ "provider": {
            "name": "relay",
            "protocol": "anthropic",
            "models_only": ["claude-*"],
            "disabled": true,
        }}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let written = &tw_config::try_parse(&b.file()).unwrap().providers[0];
    assert!(written.disabled);
    assert_eq!(written.models_only, Some(vec!["claude-*".to_string()]));
    let (_, ov) = call(&b.app, "GET", "/overview", serde_json::json!(null)).await;
    assert_eq!(ov["providers"][0]["disabled"], true);
    assert_eq!(ov["providers"][0]["model_count"], 0);

    // 空范围：一个模型都不服务。停用有自己的开关
    let before = b.file();
    let (st, v) = call(
        &b.app,
        "PUT",
        "/providers/relay",
        serde_json::json!({ "provider": {
            "name": "relay",
            "protocol": "anthropic",
            "models_only": [],
        }}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert!(v.as_str().unwrap().contains("disabled"), "{v}");
    assert_eq!(b.file(), before);
}

#[tokio::test]
async fn the_dry_run_names_the_upstreams_it_skips_and_why() {
    let yaml = "version: 1
clients:
  - name: c
    key: tw-k
providers:
  - name: 中转
    base_url: https://relay.example.com
    key: sk-b
    disabled: true
  - name: 官方
    base_url: https://api.anthropic.com
    key: sk-a
    models_only: [claude-sonnet-*]
groups:
  - name: 都试试
    type: fallback
    providers: [中转, 官方]
routes:
  - name: 默认
    rules:
      - name: 全部
        to: 都试试
";
    let b = bed(yaml);
    let (st, v) = call(
        &b.app,
        "POST",
        "/dryrun",
        serde_json::json!({ "model": "claude-sonnet-4-5" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["outcome"], "route");
    assert_eq!(v["candidates"], serde_json::json!(["官方"]));
    assert_eq!(
        v["skipped"],
        serde_json::json!([{ "provider": "中转", "reason": "disabled" }])
    );

    // 两家都服务不了：说出来，而不是给一条空的候选链
    let (_, v) = call(
        &b.app,
        "POST",
        "/dryrun",
        serde_json::json!({ "model": "claude-opus-4-1" }),
    )
    .await;
    assert_eq!(v["outcome"], "unavailable", "{v}");
    assert_eq!(v["candidates"], serde_json::json!([]));
    assert_eq!(v["skipped"][1]["reason"], "out_of_scope", "{v}");
    assert!(
        v["reason"].as_str().unwrap().contains("claude-opus-4-1"),
        "{v}"
    );
}

#[tokio::test]
async fn an_address_being_typed_previews_what_automatic_detection_will_pick() {
    let b = bed(&config(upstream().await, "sk-good", ""));
    let (st, v) = call(
        &b.app,
        "POST",
        "/providers/preview",
        serde_json::json!({ "base_url": "https://api.anthropic.com" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["protocol"], "anthropic");
    assert_eq!(v["official"], true);
    assert_eq!(v["redact"], serde_json::json!([]), "官方端点不脱敏");

    let (_, v) = call(
        &b.app,
        "POST",
        "/providers/preview",
        serde_json::json!({ "base_url": "https://relay.example/v1" }),
    )
    .await;
    assert!(v.get("protocol").is_none(), "{v}");
    assert_eq!(v["official"], false);
    assert!(
        v["redact"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("api-keys"))
    );

    // 没写计费方式时，概览说出它实际按什么计费
    let (_, ov) = call(&b.app, "GET", "/overview", serde_json::json!(null)).await;
    assert!(ov["providers"][0].get("billing").unwrap().is_null());
    assert_eq!(ov["providers"][0]["billing_effective"], "per-token");
}
