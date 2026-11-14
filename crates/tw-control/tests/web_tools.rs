use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
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
        shutdown: Default::default(),
        remote: Default::default(),
        handover: Default::default(),
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
        dir: d,
    }
}

async fn call(app: &axum::Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
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
    let v = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (st, v)
}

#[tokio::test]
async fn web_settings_persist_validate_versions_and_use_the_same_public_url_policy() {
    let bed = bed(&format!(
        "version: 1\nlisten:\n  control:\n    key: {}\nclients:\n  - name: test\n    key: tw-test\n",
        "11".repeat(32)
    ));
    let (status, view) = call(&bed.app, "GET", "/web-tools", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["config"]["mode"], "upstream");
    let mut config = view["config"].clone();
    config["mode"] = json!("core");
    config["engines"] = json!(["bing"]);
    let request = json!({"base_version":view["config_version"],"config":config});
    let (status, saved) = call(&bed.app, "PUT", "/web-tools", request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(
        tw_config::try_parse(&bed.file()).unwrap().web_tools.mode,
        tw_api::WebToolsMode::Core
    );
    assert_eq!(
        call(&bed.app, "PUT", "/web-tools", request).await.0,
        StatusCode::CONFLICT
    );
    config["max_concurrent"] = json!(0);
    let invalid = json!({"base_version":saved["version"],"config":config});
    assert!(
        !call(&bed.app, "PUT", "/web-tools", invalid)
            .await
            .0
            .is_success()
    );
    let (status, error) = call(
        &bed.app,
        "POST",
        "/web-tools/test",
        json!({"action":"open_page","url":"http://127.0.0.1/"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(error.to_string().contains("non-public"), "{error}");
}
