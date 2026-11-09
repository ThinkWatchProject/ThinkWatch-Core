//! Native discovery and its cache must follow the live gateway directory.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{OriginalUri, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use tw_config::{Client, Config, ModelSpec, Protocol, Provider};
use tw_gateway::AppState;

#[derive(Clone)]
struct Upstream {
    body: Arc<Mutex<Value>>,
    requests: Arc<Mutex<Vec<(String, String)>>>,
    control: &'static str,
    gate: Option<Arc<tokio::sync::Notify>>,
    started: Arc<tokio::sync::Notify>,
}

impl Upstream {
    fn new(body: Value) -> Self {
        Self {
            body: Arc::new(Mutex::new(body)),
            requests: Default::default(),
            control: "private, max-age=300",
            gate: None,
            started: Default::default(),
        }
    }

    async fn serve(&self) -> SocketAddr {
        async fn models(
            State(up): State<Upstream>,
            OriginalUri(uri): OriginalUri,
            headers: HeaderMap,
        ) -> impl axum::response::IntoResponse {
            up.requests.lock().unwrap().push((
                uri.to_string(),
                headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .into(),
            ));
            up.started.notify_one();
            if let Some(gate) = &up.gate {
                gate.notified().await;
            }
            (
                [("cache-control", up.control)],
                Json(up.body.lock().unwrap().clone()),
            )
        }
        let app = Router::new()
            .route("/v1/models", get(models))
            .route("/models", get(models))
            .with_state(self.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    }
}

fn native(slug: &str) -> Value {
    json!({
        "slug":slug,"display_name":"Native model","base_instructions":"Native coding instructions",
        "supported_reasoning_levels":[{"effort":"high","description":"More reasoning"}],
        "default_reasoning_level":"high","shell_type":"shell_command","visibility":"list",
        "supported_in_api":true,"priority":0,"support_verbosity":false,
        "truncation_policy":{"mode":"tokens","limit":10000},
        "experimental_supported_tools":["native_tool"],"upgrade":null,
        "context_window":128000,"max_context_window":256000,"input_modalities":["text","image"],
        "future_tool_setting":{"enabled":true}
    })
}

fn provider(name: &str, addr: SocketAddr, models: &[&str]) -> Provider {
    Provider {
        name: name.into(),
        base_url: format!("http://{addr}/v1"),
        protocol: Some(Protocol::OpenaiResponses),
        key: Some("upstream-test-key".into()),
        models: models.iter().map(|s| (*s).into()).collect(),
        ..Default::default()
    }
}

fn config(providers: Vec<Provider>) -> Config {
    Config {
        providers,
        clients: vec![Client {
            name: "codex".into(),
            key: "gateway-test-key".into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

async fn serve(config: Config) -> (AppState, SocketAddr) {
    let state = AppState::new(config).unwrap();
    let addr = tw_gateway::serve(state.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    (state, addr)
}

fn request(addr: SocketAddr) -> reqwest::RequestBuilder {
    http()
        .get(format!("http://{addr}/codex/models?client_version=0.166.0"))
        .bearer_auth("gateway-test-key")
}

async fn catalog(addr: SocketAddr) -> Value {
    request(addr)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn slugs(body: &Value) -> Vec<&str> {
    body["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["slug"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn native_metadata_is_version_scoped_and_uses_only_provider_credentials() {
    let up = Upstream::new(json!({"models":[native("private-model")]}));
    let addr = up.serve().await;
    let mut cfg = config(vec![provider("native", addr, &["private-model"])]);
    cfg.aliases = serde_yaml_ng::from_str("coding: private-model").unwrap();
    let (_, gw) = serve(cfg).await;
    let body = catalog(gw).await;
    assert_eq!(slugs(&body), ["coding", "private-model"]);
    for m in body["models"].as_array().unwrap() {
        assert_eq!(m["base_instructions"], "Native coding instructions");
        assert_eq!(m["experimental_supported_tools"], json!(["native_tool"]));
        assert_eq!(m["future_tool_setting"]["enabled"], true);
        assert_eq!(m["supported_reasoning_levels"][0]["effort"], "high");
        assert_eq!(m["input_modalities"], json!(["text", "image"]));
        assert_eq!(m["context_window"], 128000);
        assert_eq!(m["max_context_window"], 256000);
        assert!(m["upgrade"].is_null());
    }
    assert_eq!(body["models"][0]["display_name"], "coding");
    catalog(gw).await;
    assert_eq!(
        up.requests.lock().unwrap().len(),
        1,
        "native metadata should be cached"
    );
    let response = http()
        .get(format!("http://{gw}/codex/models?client_version=0.165.0"))
        .bearer_auth("gateway-test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let requests = up.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0],
        (
            "/v1/models?client_version=0.166.0".into(),
            "Bearer upstream-test-key".into()
        )
    );
    assert!(requests[1].0.contains("0.165.0"));
}

#[tokio::test]
async fn chatgpt_metadata_uses_the_actual_codex_version_instead_of_the_inventory_version() {
    let up = Upstream::new(json!({"models":[native("one")]}));
    let addr = up.serve().await;
    let mut p = provider("account", addr, &["one"]);
    p.base_url = format!("http://{addr}");
    p.protocol = Some(Protocol::Chatgpt);
    let (_, gw) = serve(config(vec![p])).await;
    assert_eq!(
        catalog(gw).await["models"][0]["base_instructions"],
        "Native coding instructions"
    );
    assert_eq!(
        up.requests.lock().unwrap()[0].0,
        "/models?client_version=0.166.0"
    );
}

#[tokio::test]
async fn ordinary_model_lists_get_a_generic_profile_and_standard_listing_is_unchanged() {
    let up = Upstream::new(json!({"data":[{"id":"unknown-model"}]}));
    let (_, gw) = serve(config(vec![provider(
        "ordinary",
        up.serve().await,
        &["unknown-model"],
    )]))
    .await;
    let body = catalog(gw).await;
    let model = &body["models"][0];
    assert_eq!(model["slug"], "unknown-model");
    assert!(model["base_instructions"].is_string());
    assert!(model.get("context_window").is_none());
    assert_eq!(model["supported_reasoning_levels"], json!([]));
    assert_eq!(model["input_modalities"], json!(["text"]));
    let standard: Value = http()
        .get(format!("http://{gw}/v1/models"))
        .bearer_auth("gateway-test-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(standard["data"][0]["id"], "unknown-model");
    assert!(standard.get("models").is_none());
}

#[tokio::test]
async fn live_reload_changes_names_aliases_permissions_and_specs_before_conditional_reads() {
    let up = Upstream::new(json!({"models":[native("one"),native("two")]}));
    let mut cfg = config(vec![provider("native", up.serve().await, &["one", "two"])]);
    cfg.clients[0].allow = Some(vec!["one".into()]);
    let (state, gw) = serve(cfg.clone()).await;
    let first = request(gw).send().await.unwrap();
    let etag = first.headers()["etag"].clone();
    assert_eq!(slugs(&first.json::<Value>().await.unwrap()), ["one"]);
    let unchanged = request(gw)
        .header("if-none-match", etag.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(unchanged.status(), 304);
    assert!(unchanged.bytes().await.unwrap().is_empty());

    cfg.providers[0].models.push("new".into());
    cfg.aliases = serde_yaml_ng::from_str("coding: two").unwrap();
    cfg.clients[0].allow = Some(vec!["coding".into(), "new".into()]);
    cfg.providers[0].model_specs.insert(
        "two".into(),
        ModelSpec {
            context_window: Some(64000),
            reasoning: Some(false),
            image_input: Some(false),
            ..Default::default()
        },
    );
    state.reload(cfg.clone()).unwrap();
    let changed = request(gw)
        .header("if-none-match", etag.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(changed.status(), 200);
    let body: Value = changed.json().await.unwrap();
    assert_eq!(slugs(&body), ["coding", "new"]);
    assert_eq!(body["models"][0]["context_window"], 64000);
    assert_eq!(body["models"][0]["supported_reasoning_levels"], json!([]));
    assert_eq!(body["models"][0]["input_modalities"], json!(["text"]));
    assert_eq!(
        up.requests.lock().unwrap().len(),
        1,
        "reload must reapply policy over cached native metadata"
    );
    cfg.clients[0].disabled = true;
    state.reload(cfg).unwrap();
    assert_eq!(
        request(gw)
            .header("if-none-match", etag)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
}

#[tokio::test]
async fn provider_credential_changes_partition_native_metadata() {
    let up = Upstream::new(json!({"models":[native("one")]}));
    let mut cfg = config(vec![provider("native", up.serve().await, &["one"])]);
    let (state, gw) = serve(cfg.clone()).await;
    catalog(gw).await;
    up.body.lock().unwrap()["models"][0]["base_instructions"] = json!("New account instructions");
    cfg.providers[0].key = Some("new-account-key".into());
    state.reload(cfg).unwrap();
    assert_eq!(
        catalog(gw).await["models"][0]["base_instructions"],
        "New account instructions"
    );
    assert_eq!(up.requests.lock().unwrap()[1].1, "Bearer new-account-key");
}

#[tokio::test]
async fn no_store_and_concurrent_reads_obey_upstream_cache_controls() {
    let mut up = Upstream::new(json!({"models":[native("one")]}));
    up.control = "no-store";
    let (_, gw) = serve(config(vec![provider("native", up.serve().await, &["one"])])).await;
    catalog(gw).await;
    up.body.lock().unwrap()["models"][0]["base_instructions"] = json!("Fresh instructions");
    assert_eq!(
        catalog(gw).await["models"][0]["base_instructions"],
        "Fresh instructions"
    );
    assert_eq!(up.requests.lock().unwrap().len(), 2);

    let up = Upstream::new(json!({"models":[native("one")]}));
    let (_, gw) = serve(config(vec![provider("native", up.serve().await, &["one"])])).await;
    let (a, b, c) = tokio::join!(catalog(gw), catalog(gw), catalog(gw));
    assert_eq!(a, b);
    assert_eq!(b, c);
    assert_eq!(up.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn aliases_intersect_capabilities_across_all_candidates() {
    let one = Upstream::new(json!({"models":[native("one")]}));
    let two = Upstream::new(json!({"models":[native("two")]}));
    let mut cfg = config(vec![
        provider("first", one.serve().await, &["one"]),
        provider("second", two.serve().await, &["two"]),
    ]);
    cfg.aliases = serde_yaml_ng::from_str("coding: [one, two]").unwrap();
    cfg.providers[1].model_specs.insert(
        "two".into(),
        ModelSpec {
            context_window: Some(32000),
            image_input: Some(false),
            reasoning: Some(false),
            ..Default::default()
        },
    );
    let (_, gw) = serve(cfg).await;
    let body = catalog(gw).await;
    let alias = &body["models"][0];
    assert_eq!(alias["slug"], "coding");
    assert_eq!(alias["context_window"], 32000);
    assert_eq!(alias["supported_reasoning_levels"], json!([]));
    assert_eq!(alias["input_modalities"], json!(["text"]));
}

#[tokio::test]
async fn metadata_fetch_cannot_resurrect_permissions_or_an_old_account_during_reload() {
    let mut up = Upstream::new(json!({"models":[native("one")]}));
    let gate = Arc::new(tokio::sync::Notify::new());
    up.gate = Some(gate.clone());
    let mut cfg = config(vec![provider("native", up.serve().await, &["one"])]);
    let (state, gw) = serve(cfg.clone()).await;
    let pending = tokio::spawn(async move { catalog(gw).await });
    tokio::time::timeout(Duration::from_secs(3), up.started.notified())
        .await
        .unwrap();
    cfg.providers[0].key = Some("different-account".into());
    cfg.providers[0].models = vec!["new".into()];
    cfg.clients[0].allow = Some(vec!["new".into()]);
    state.reload(cfg).unwrap();
    gate.notify_one();
    let body = pending.await.unwrap();
    assert_eq!(slugs(&body), ["new"]);
    assert_ne!(
        body["models"][0]["base_instructions"],
        "Native coding instructions"
    );
}

#[tokio::test]
async fn invalid_versions_and_unauthorized_reads_never_reach_upstream() {
    let up = Upstream::new(json!({"models":[native("one")]}));
    let (_, gw) = serve(config(vec![provider("native", up.serve().await, &["one"])])).await;
    for query in [
        "",
        "client_version=nope",
        "client_version=1.2.3&client_version=1.2.4",
    ] {
        let response = http()
            .get(format!("http://{gw}/codex/models?{query}"))
            .bearer_auth("gateway-test-key")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    }
    assert_eq!(
        http()
            .get(format!("http://{gw}/codex/models?client_version=0.166.0"))
            .header("if-none-match", "*")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert!(up.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn oversized_or_non_native_upstream_metadata_falls_back_to_a_bounded_profile() {
    let mut model = native("one");
    model["base_instructions"] = json!("x".repeat(1024 * 1024));
    let up = Upstream::new(json!({"models":[model]}));
    let (_, gw) = serve(config(vec![provider("native", up.serve().await, &["one"])])).await;
    let body = catalog(gw).await;
    assert!(
        body["models"][0]["base_instructions"]
            .as_str()
            .unwrap()
            .len()
            < 1000
    );
    assert!(body["models"][0].get("context_window").is_none());
}

#[tokio::test]
async fn source_restrictions_apply_to_native_discovery() {
    use tower::ServiceExt;
    let state = AppState::new(config(vec![])).unwrap();
    let response = tw_gateway::server::router(state)
        .oneshot(
            axum::http::Request::builder()
                .uri("/codex/models?client_version=0.166.0")
                .header("authorization", "Bearer gateway-test-key")
                .extension(axum::extract::ConnectInfo(
                    "203.0.113.10:12345".parse::<SocketAddr>().unwrap(),
                ))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn oversized_projected_catalog_is_rejected_instead_of_truncated() {
    let mut p = Provider {
        name: "local".into(),
        base_url: "http://127.0.0.1:1".into(),
        protocol: Some(Protocol::Anthropic),
        ..Default::default()
    };
    p.models = (0..2000).map(|i| format!("local-model-{i}")).collect();
    let (_, gw) = serve(config(vec![p])).await;
    assert_eq!(request(gw).send().await.unwrap().status(), 413);
}
