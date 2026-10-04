//! 安全模式下配置文件读不了：core 顶上一份临时配置起控制面（`tw_config::stand_in`），
//! 界面连上来就看得到哪一行错了，能一键修复（`/config/repair`），修好的那一份换进来。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

const GOOD: &str = "version: 1
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
clients:
  - name: default
    key: tw-aaaa
";

struct Bed {
    dir: tempfile::TempDir,
    app: axum::Router,
    gw: tw_gateway::AppState,
    mgr: Arc<ConfigManager>,
}

impl Bed {
    fn file(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("config.yaml")).unwrap()
    }
}

/// 照 `twcore serve --safe` 起控制面那样：读不了就顶上
fn safe_mode(yaml: &str) -> Bed {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, yaml).unwrap();
    let (cfg, r) = tw_config::stand_in(yaml).expect("这份配置读不了、钥匙还在");
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let mgr = Arc::new(ConfigManager::new(p, gw.clone(), bus).standing_in(&r));
    let state = ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: mgr.clone(),
        gateway: gw.clone(),
        store: None,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    };
    Bed {
        app: tw_control::router(state),
        dir: d,
        gw,
        mgr,
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
    (
        st,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
    )
}

#[tokio::test]
async fn the_status_says_which_line_is_wrong_from_the_start() {
    // **界面半路才连上来**：被拒那件事是现状，不靠事件。安全模式一起来就挂着
    let b = safe_mode(&format!("{GOOD}client_probes:\n  titling: passthrough\n"));
    let (st, s) = call(&b.app, "GET", "/status", serde_json::Value::Null).await;
    assert_eq!(st, StatusCode::OK, "{s}");
    let r = &s["config_rejected"];
    assert_eq!(r["stage"], "schema", "{s}");
    assert_eq!(r["line"], 9, "{s}");
    assert_eq!(r["excerpt"], "  titling: passthrough", "{s}");
    assert_eq!(r["message"]["code"], "config.unknown_variant", "{s}");
    assert_eq!(
        r["message"]["args"]["field"], "client_probes.titling",
        "{s}"
    );
}

#[tokio::test]
async fn one_click_repair_lists_the_fixes_then_writes_and_swaps_in() {
    let b = safe_mode(&format!(
        "{GOOD}client_probes:\n  titling: passthrough\nretention:\n  body_dayz: 3\n"
    ));
    let (st, plan) = call(&b.app, "GET", "/config/repair", serde_json::Value::Null).await;
    assert_eq!(st, StatusCode::OK, "{plan}");
    let fixes = plan["fixes"].as_array().unwrap();
    assert_eq!(fixes.len(), 2, "{plan}");
    assert_eq!(fixes[0]["kind"], "unknown_value");
    assert_eq!(fixes[0]["value"], "passthrough");
    assert_eq!(fixes[0]["now"], "forward");
    assert_eq!(fixes[1]["kind"], "unknown_field");
    assert_eq!(fixes[1]["field"], "retention.body_dayz");

    let (st, w) = call(
        &b.app,
        "POST",
        "/config/repair",
        serde_json::json!({ "base_version": plan["base_version"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{w}");
    assert_eq!(b.file(), GOOD, "只删那几处，别的原样");
    // 修好的那一份换进来了，被拒那件事也就过去了
    assert_eq!(b.gw.config().clients.len(), 1);
    let (_, s) = call(&b.app, "GET", "/status", serde_json::Value::Null).await;
    assert!(s.get("config_rejected").is_none_or(|v| v.is_null()), "{s}");
    // 没什么可修的了
    let (_, plan) = call(&b.app, "GET", "/config/repair", serde_json::Value::Null).await;
    assert_eq!(plan["fixes"], serde_json::json!([]));
}

#[tokio::test]
async fn a_repair_against_an_older_version_is_refused() {
    let b = safe_mode(&format!("{GOOD}client_probes:\n  titling: passthrough\n"));
    let (st, _) = call(
        &b.app,
        "POST",
        "/config/repair",
        serde_json::json!({ "base_version": "blake3:000000000000" }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert!(b.file().contains("passthrough"), "文件变了就不修");
}

#[tokio::test]
async fn an_error_that_is_not_fixable_offers_nothing() {
    // 两个同名上游：整份配置的事，删哪一处都是猜
    let b = safe_mode(&format!(
        "{GOOD}providers:\n  - name: a\n    base_url: https://x\n  - name: a\n    base_url: https://y\n"
    ));
    let (_, plan) = call(&b.app, "GET", "/config/repair", serde_json::Value::Null).await;
    assert_eq!(plan["fixes"], serde_json::json!([]), "{plan}");
    let (st, e) = call(
        &b.app,
        "POST",
        "/config/repair",
        serde_json::json!({ "base_version": plan["base_version"] }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{e}");
    assert_eq!(e["code"], "control.config_not_repairable", "{e}");
}

#[tokio::test]
async fn saving_the_same_broken_text_again_does_not_clear_the_rejection() {
    // 在服务的是临时配置，不是文件里那份：同样的内容再写一次（编辑器里保存、touch）
    // 不能被当成「改回了在服务的那一版」
    let bad = format!("{GOOD}client_probes:\n  titling: passthrough\n");
    let b = safe_mode(&bad);
    std::fs::write(b.dir.path().join("config.yaml"), &bad).unwrap();
    assert!(
        b.mgr.reload_from_disk().await.is_err(),
        "同样的坏内容还是读不进来"
    );
    let (_, s) = call(&b.app, "GET", "/status", serde_json::Value::Null).await;
    assert!(!s["config_rejected"].is_null(), "{s}");
    // 在文件里改好了（编辑器里保存），文件监听那一路换得进来
    std::fs::write(b.dir.path().join("config.yaml"), GOOD).unwrap();
    assert!(b.mgr.reload_from_disk().await.unwrap().is_some());
    let (_, s) = call(&b.app, "GET", "/status", serde_json::Value::Null).await;
    assert!(s.get("config_rejected").is_none_or(|v| v.is_null()), "{s}");
}
