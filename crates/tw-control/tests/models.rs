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
    /// 从一开始就订阅的事件
    events: tokio::sync::broadcast::Receiver<tw_api::Event>,
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
    let events = bus.subscribe();
    let state = ControlState {
        shutdown: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        store: None,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
        // **测试里绝不能碰开发者自己的配置**
        home: d.path().join("home"),
    };
    Bed {
        app: tw_control::router(state),
        dir: d,
        events,
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

/// 等到这一家的 `models_changed` 来够 `n` 条。
async fn models_changed(events: &mut tokio::sync::broadcast::Receiver<tw_api::Event>, n: usize) {
    let mut seen = 0;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while seen < n {
            if let tw_api::Event::ModelsChanged { provider, .. } = events.recv().await.unwrap() {
                assert_eq!(provider, "relay");
                seen += 1;
            }
        }
    })
    .await
    .expect("等 models_changed 超时");
}

#[tokio::test]
async fn opening_the_page_asks_what_it_has_not_got_and_says_when_the_answer_is_back() {
    let up = upstream().await;
    let mut b = bed(&config(up, "sk-good", ""));
    let (_, ov) = call(&b.app, "GET", "/overview", serde_json::json!(null)).await;
    assert_eq!(ov["providers"][0]["model_status"], "pending", "{ov}");

    let (st, v) = call(&b.app, "POST", "/models/refresh", serde_json::json!(null)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["providers"], serde_json::json!(["relay"]));
    // 开始问一条、问完一条
    models_changed(&mut b.events, 2).await;
    let (_, ov) = call(&b.app, "GET", "/overview", serde_json::json!(null)).await;
    let p = &ov["providers"][0];
    assert_eq!(p["model_status"], "listed", "{p}");
    assert_eq!(p["model_source"], "discovered");
    assert_eq!(p["model_fetching"], false);
    assert_eq!(p["model_count"], 3);
    assert!(p["model_checked_at_ms"].as_u64().is_some(), "{p}");
    assert!(p.get("model_error").is_none(), "{p}");

    // 刚问过：再打开一次页面不再去问
    let (_, v) = call(&b.app, "POST", "/models/refresh", serde_json::json!(null)).await;
    assert_eq!(v["providers"], serde_json::json!([]));
    let (_, v) = call(
        &b.app,
        "GET",
        "/providers/relay/models",
        serde_json::json!(null),
    )
    .await;
    assert_eq!(
        (v["status"].as_str(), v["fetching"].as_bool()),
        (Some("listed"), Some(false))
    );
}

#[tokio::test]
async fn an_upstream_that_refuses_the_key_is_failed_with_the_reason_not_merely_unknown() {
    let up = upstream().await;
    let mut b = bed(&config(up, "sk-bad", ""));
    call(&b.app, "POST", "/models/refresh", serde_json::json!(null)).await;
    models_changed(&mut b.events, 2).await;
    let (_, ov) = call(&b.app, "GET", "/overview", serde_json::json!(null)).await;
    let p = &ov["providers"][0];
    assert_eq!(
        (p["model_status"].as_str(), p["model_source"].as_str()),
        (Some("failed"), Some("none"))
    );
    // 一句带码的话，界面按码翻；状态码是参数
    assert_eq!(p["model_error"]["code"], "gw.probe.key_rejected", "{p}");
    assert_eq!(p["model_error"]["args"]["status"], "401", "{p}");
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
    assert_eq!(v["error"]["code"], "gw.probe.key_rejected", "{v}");
    assert!(v["error"]["text"].as_str().unwrap().contains("401"), "{v}");
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
    // 错误体现在是一个 JSON 的 Msg：码给界面，text 给读日志的人
    assert!(v["text"].as_str().unwrap().contains("disabled"), "{v}");
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
        serde_json::json!({ "model": "claude-sonnet-4-5", "client": "c" }),
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
        serde_json::json!({ "model": "claude-opus-4-1", "client": "c" }),
    )
    .await;
    assert_eq!(v["outcome"], "unavailable", "{v}");
    assert_eq!(v["candidates"], serde_json::json!([]));
    assert_eq!(v["skipped"][1]["reason"], "out_of_scope", "{v}");
    // 原因都在 `skipped` 里，不再另写一句
    assert!(v["reason"].is_null(), "{v}");
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
    // 安全设置是全局的，预览里不再有「这家脱不脱敏、可不可信」
    assert!(
        v.get("official").is_none() && v.get("redact").is_none(),
        "{v}"
    );
    assert_eq!(v["auth_header"], "x-api-key");

    // 选了协议：密钥按选的协议放，「自动识别」那一项仍然说按地址推断的结果
    let (st, v) = call(
        &b.app,
        "POST",
        "/providers/preview",
        serde_json::json!({ "base_url": "https://api.anthropic.com", "protocol": "openai-chat" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["protocol"], "anthropic");
    assert_eq!(v["auth_header"], "authorization");

    let (st, _) = call(
        &b.app,
        "POST",
        "/providers/preview",
        serde_json::json!({ "base_url": "https://api.anthropic.com", "protocol": "grpc" }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let (_, v) = call(
        &b.app,
        "POST",
        "/providers/preview",
        serde_json::json!({ "base_url": "https://relay.example/v1" }),
    )
    .await;
    assert!(v.get("protocol").is_none(), "{v}");

    // 没写计费方式就是按量计费，概览照实说
    let (_, ov) = call(&b.app, "GET", "/overview", serde_json::json!(null)).await;
    assert_eq!(ov["providers"][0]["billing"], "per-token");
    assert!(ov["providers"][0].get("billing_effective").is_none());
}
