//! 为某个客户端发一把专用密钥（`POST /clients/{id}/key`）。接管本身在桌面端做，
//! 它要的只是一把属于这个客户端的钥匙。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

const BASE: &str = "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: 我\n    key: tw-一把钥匙就够\nproviders:\n  - name: 官方\n    base_url: https://api.anthropic.com\n    key: sk-x\n";

struct Bed {
    _dir: tempfile::TempDir,
    app: axum::Router,
    state: ControlState,
}

fn bed() -> Bed {
    let store = None;
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, BASE).unwrap();
    let cfg: tw_config::Config = serde_yaml_ng::from_str(BASE).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let state = ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        store,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    };
    Bed {
        app: tw_control::router(state.clone()),
        state,
        _dir: d,
    }
}

async fn post(app: &axum::Router, path: &str, body: &str) -> (StatusCode, String) {
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
    (st, String::from_utf8_lossy(&b).to_string())
}

#[tokio::test]
async fn a_client_gets_its_own_key_once_and_it_remembers_whose_it_is() {
    // 接管、手动配 Cursor 时填进去的应该是一把只属于它的钥匙：流量、路由、
    // 并发上限才分得清是谁。**再要一次给的是同一把**，不会攒下一堆
    let b = bed();
    let (st, body) = post(&b.app, "/clients/cursor/key", "").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let k: tw_api::ClientKey = serde_json::from_str(&body).unwrap();
    assert_eq!(k.name, "cursor");
    assert!(k.created);
    assert!(k.key.starts_with("tw-"), "{}", k.key);
    let made = b
        .state
        .config()
        .clients
        .iter()
        .find(|c| c.name == "cursor")
        .cloned()
        .expect("配置里要有这把");
    assert_eq!(made.client.as_deref(), Some("cursor"), "要记着是为谁生成的");
    assert_eq!(made.key, k.key);

    let (_, body) = post(&b.app, "/clients/cursor/key", "").await;
    let again: tw_api::ClientKey = serde_json::from_str(&body).unwrap();
    assert_eq!((again.name.as_str(), again.created), ("cursor", false));
    assert_eq!(again.key, k.key);
    assert_eq!(b.state.config().clients.len(), 2, "只该多出一把");

    // 客户端认不认得是桌面端的事；这里只不认一个不像客户端 id 的词
    let (st, _) = post(&b.app, "/clients/Not%20A%20Client/key", "").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// 名字被别的密钥占了就往后编号，而为它留着的那把一直认得
#[tokio::test]
async fn a_taken_name_gets_a_number_and_the_owner_is_still_found() {
    let b = bed();
    // 一把叫 codex、却不是为 Codex 生成的钥匙（用户自己建的）
    let (st, body) = post(&b.app, "/keys", r#"{"key":{"name":"codex"}}"#).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = post(&b.app, "/clients/codex/key", "").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let k: tw_api::ClientKey = serde_json::from_str(&body).unwrap();
    assert_eq!((k.name.as_str(), k.created), ("codex-2", true));
    let (_, body) = post(&b.app, "/clients/codex/key", "").await;
    let again: tw_api::ClientKey = serde_json::from_str(&body).unwrap();
    assert_eq!((again.name.as_str(), again.created), ("codex-2", false));
}
