//! 配置包、导入导出、备份与恢复、通用偏好、固定密钥：经控制面走一遍，断言落在文件上。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

const KEY: &str = "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00";

fn mine() -> String {
    format!(
        "version: 1
listen:
  control:
    key: {KEY}
clients:
  - name: default
    key: tw-aaaa
  - name: claude-code
    key: tw-cccc
    client: claude-code
# 我的上游
providers:
  - name: 官方
    base_url: https://api.anthropic.com
    key: sk-official
routes:
  - name: default
    rules:
      - name: 兜底
        to: 官方
"
    )
}

fn theirs() -> String {
    "version: 1
listen:
  control:
    key: 1111111111111111111111111111111111111111111111111111111111111111
clients:
  - name: default
    key: tw-theirs
providers:
  - name: 官方
    base_url: https://api.anthropic.com/v2
    key: sk-theirs
  - name: relay
    base_url: https://relay.example
    key: sk-relay
failover:
  pause_secs: 30
"
    .to_string()
}

struct Bed {
    dir: tempfile::TempDir,
    app: axum::Router,
    state: ControlState,
}

impl Bed {
    fn path(&self) -> std::path::PathBuf {
        self.dir.path().join("config.yaml")
    }
    fn file(&self) -> String {
        std::fs::read_to_string(self.path()).unwrap()
    }
    fn version(&self) -> String {
        tw_config::store::version_of(&self.file())
    }
}

fn state_for(
    p: std::path::PathBuf,
    mgr: Arc<ConfigManager>,
    gw: tw_gateway::AppState,
) -> ControlState {
    let _ = p;
    ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: mgr,
        gateway: gw,
        store: None,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    }
}

fn bed(yaml: &str) -> Bed {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, yaml).unwrap();
    let cfg = tw_config::try_parse(yaml).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let mgr = Arc::new(ConfigManager::new(p.clone(), gw.clone(), bus));
    let state = state_for(p, mgr, gw);
    Bed {
        app: tw_control::router(state.clone()),
        state,
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
    let b = axum::body::to_bytes(r.into_body(), 1 << 24).await.unwrap();
    let text = String::from_utf8_lossy(&b).to_string();
    let v = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
    (st, v)
}

async fn export_all(b: &Bed, secrets: bool) -> serde_json::Value {
    let (st, v) = call(
        &b.app,
        "POST",
        "/config/export",
        serde_json::json!({ "scope": "all", "secrets": secrets }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v
}

#[tokio::test]
async fn an_export_carries_no_local_settings_and_imports_item_by_item() {
    let src = bed(&theirs());
    let file = export_all(&src, true).await;
    assert!(file["file_name"].as_str().unwrap().ends_with(".twconfig"));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(file["data_b64"].as_str().unwrap())
        .unwrap();
    let pack = tw_pack::read(&bytes, tw_pack::Unseal::None).unwrap();
    assert!(
        !pack.config_yaml.contains("1111111111"),
        "{}",
        pack.config_yaml
    );
    assert!(!pack.config_yaml.contains("listen"), "{}", pack.config_yaml);

    let dst = bed(&mine());
    let (st, preview) = call(
        &dst.app,
        "POST",
        "/config/import/preview",
        serde_json::json!({ "data_b64": file["data_b64"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{preview}");
    let status = |kind: &str, name: &str| -> String {
        preview["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["item"]["kind"] == kind && r["item"]["name"] == name)
            .map(|r| r["status"].as_str().unwrap().to_string())
            .unwrap_or_else(|| panic!("no row {kind} {name}: {preview}"))
    };
    assert_eq!(status("provider", "relay"), "new");
    assert_eq!(status("provider", "官方"), "conflict");
    assert_eq!(status("global", "failover"), "new");
    assert_eq!(preview["manifest"]["kind"], "full");
    assert_eq!(preview["base_version"], dst.version());

    let (st, v) = call(
        &dst.app,
        "POST",
        "/config/import",
        serde_json::json!({
            "data_b64": file["data_b64"],
            "base_version": preview["base_version"],
            "decisions": [
                { "item": { "kind": "provider", "name": "relay" }, "action": "import" },
                { "item": { "kind": "provider", "name": "官方" }, "action": "rename", "new_name": "官方 (2)" },
                { "item": { "kind": "global", "name": "failover" }, "action": "import" },
            ],
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let text = dst.file();
    let cfg = tw_config::try_parse(&text).unwrap();
    let names: Vec<&str> = cfg.providers.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["官方", "官方 (2)", "relay"]);
    assert_eq!(cfg.failover.pause_secs, 30);
    assert!(
        text.contains("# 我的上游"),
        "untouched lines keep their comments: {text}"
    );
    assert!(text.contains(KEY));
    // 导入留了一个版本，来源是导入
    let (st, list) = call(&dst.app, "GET", "/backups", serde_json::json!(null)).await;
    assert_eq!(st, StatusCode::OK, "{list}");
    assert_eq!(list[0]["origin"], "import");
    assert_eq!(list[0]["current"], true);
    // 预览之后配置变了：409
    let (st, _) = call(
        &dst.app,
        "POST",
        "/config/import",
        serde_json::json!({
            "data_b64": file["data_b64"],
            "base_version": preview["base_version"],
            "decisions": [],
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
}

#[tokio::test]
async fn an_encrypted_export_needs_its_passphrase_to_preview() {
    tw_pack::TEST_WORK_FACTOR.store(10, std::sync::atomic::Ordering::Relaxed);
    let src = bed(&theirs());
    let (st, v) = call(
        &src.app,
        "POST",
        "/config/export",
        serde_json::json!({ "scope": "all", "secrets": false, "passphrase": "short" }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(v["code"], "pack.passphrase_short");
    let (st, file) = call(
        &src.app,
        "POST",
        "/config/export",
        serde_json::json!({ "scope": "all", "secrets": false, "passphrase": "long enough" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{file}");
    let dst = bed(&mine());
    for (pass, code) in [
        (serde_json::Value::Null, "pack.need_passphrase"),
        (serde_json::json!("wrong one!"), "pack.wrong_passphrase"),
    ] {
        let (st, v) = call(
            &dst.app,
            "POST",
            "/config/import/preview",
            serde_json::json!({ "data_b64": file["data_b64"], "passphrase": pass }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["code"], code);
    }
    let (st, preview) = call(
        &dst.app,
        "POST",
        "/config/import/preview",
        serde_json::json!({ "data_b64": file["data_b64"], "passphrase": "long enough" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{preview}");
    assert_eq!(preview["manifest"]["secrets"], false);
    assert_eq!(preview["manifest"]["encrypted"], true);
    let relay = preview["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["item"]["name"] == "relay")
        .unwrap();
    assert_eq!(relay["flags"], serde_json::json!(["missing_key"]));
}

#[tokio::test]
async fn exporting_chosen_items_brings_what_they_refer_to() {
    let b = bed(&mine());
    let (st, v) = call(
        &b.app,
        "POST",
        "/config/export",
        serde_json::json!({
            "scope": { "items": [{ "kind": "route", "name": "default" }] },
            "secrets": true,
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let included: Vec<String> = v["included"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            format!(
                "{}/{}",
                i["kind"].as_str().unwrap(),
                i["name"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(included, ["provider/官方", "route/default"]);
    let (st, v) = call(
        &b.app,
        "POST",
        "/config/export",
        serde_json::json!({
            "scope": { "items": [{ "kind": "provider", "name": "nope" }] },
            "secrets": true,
        }),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(v["code"], "pack.no_such_item");
    // 项目清单
    let (st, items) = call(&b.app, "GET", "/config/items", serde_json::json!(null)).await;
    assert_eq!(st, StatusCode::OK, "{items}");
    let route = items
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["item"]["kind"] == "route")
        .unwrap();
    assert_eq!(
        route["refs"],
        serde_json::json!([{ "kind": "provider", "name": "官方" }])
    );
}

#[tokio::test]
async fn a_restore_brings_back_items_keeps_pinned_keys_and_is_itself_a_version() {
    let b = bed(&mine());
    // 记下现在这一版，再改两处：删掉一把密钥、换掉上游地址
    b.state.cfg.record_startup();
    let changed = mine()
        .replace(
            "  - name: claude-code\n    key: tw-cccc\n    client: claude-code\n",
            "",
        )
        .replace("https://api.anthropic.com\n", "https://changed.example\n");
    b.state
        .cfg
        .write(&changed, None, tw_config::history::Origin::Ui)
        .await
        .unwrap();
    // 接管着 claude-code 的那把在现在的配置里已经没了；固定的是 default
    let (st, _) = call(
        &b.app,
        "PUT",
        "/pinned-keys",
        serde_json::json!({ "names": ["default"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        tw_pack::disk::read_pinned(&b.path()),
        ["default".to_string()].into_iter().collect()
    );
    let (_, list) = call(&b.app, "GET", "/backups", serde_json::json!(null)).await;
    let oldest = list.as_array().unwrap().last().unwrap()["id"].clone();
    let (st, preview) = call(
        &b.app,
        "POST",
        "/backups/preview",
        serde_json::json!({ "id": oldest }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{preview}");
    let rows: Vec<String> = preview["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            format!(
                "{} {}/{}",
                r["change"].as_str().unwrap(),
                r["item"]["kind"].as_str().unwrap(),
                r["item"]["name"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(rows, ["modify provider/官方", "add key/claude-code"]);
    // 只恢复上游
    let (st, v) = call(
        &b.app,
        "POST",
        "/backups/restore",
        serde_json::json!({
            "id": oldest,
            "base_version": preview["base_version"],
            "items": [{ "kind": "provider", "name": "官方" }],
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let cfg = tw_config::try_parse(&b.file()).unwrap();
    assert_eq!(cfg.providers[0].base_url, "https://api.anthropic.com");
    assert!(!cfg.clients.iter().any(|c| c.name == "claude-code"));
    let (_, list) = call(&b.app, "GET", "/backups", serde_json::json!(null)).await;
    assert_eq!(list[0]["origin"], "restore");
    // 另存为配置包：本机设置不在里面
    let (st, file) = call(
        &b.app,
        "POST",
        "/backups/file",
        serde_json::json!({ "id": oldest }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{file}");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(file["data_b64"].as_str().unwrap())
        .unwrap();
    let pack = tw_pack::read(&bytes, tw_pack::Unseal::None).unwrap();
    assert!(!pack.config_yaml.contains("listen"), "{}", pack.config_yaml);
    let (st, v) = call(
        &b.app,
        "POST",
        "/backups/preview",
        serde_json::json!({ "id": "123" }),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(v["code"], "backup.not_found");
}

#[tokio::test]
async fn client_preferences_are_kept_announced_and_part_of_a_version() {
    let b = bed(&mine());
    let mut events = b.state.bus().subscribe();
    let (st, v) = call(&b.app, "GET", "/client-prefs", serde_json::json!(null)).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v, serde_json::json!({ "prefs": null }));
    let prefs = serde_json::json!({ "language": "en", "appearance": "dark" });
    let (st, _) = call(
        &b.app,
        "PUT",
        "/client-prefs",
        serde_json::json!({ "prefs": prefs }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, v) = call(&b.app, "GET", "/client-prefs", serde_json::json!(null)).await;
    assert_eq!(v["prefs"], prefs);
    let got = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if let Ok(tw_api::Event::ClientPrefsChanged { .. }) = events.recv().await {
                return;
            }
        }
    })
    .await;
    assert!(got.is_ok(), "no client_prefs_changed");
    let last = tw_pack::backup::list(&b.path()).unwrap().pop().unwrap();
    assert_eq!(
        tw_pack::backup::open(&last).unwrap().client_prefs,
        Some(prefs)
    );
    let big = "x".repeat(tw_api::CLIENT_PREFS_MAX + 1);
    let (st, v) = call(
        &b.app,
        "PUT",
        "/client-prefs",
        serde_json::json!({ "prefs": { "big": big } }),
    )
    .await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
}

#[tokio::test]
async fn overwriting_a_key_by_import_announces_it() {
    let src = bed(&theirs());
    let file = export_all(&src, true).await;
    let dst = bed(&mine());
    let mut events = dst.state.bus().subscribe();
    let (_, preview) = call(
        &dst.app,
        "POST",
        "/config/import/preview",
        serde_json::json!({ "data_b64": file["data_b64"] }),
    )
    .await;
    let (st, v) = call(
        &dst.app,
        "POST",
        "/config/import",
        serde_json::json!({
            "data_b64": file["data_b64"],
            "base_version": preview["base_version"],
            "decisions": [{ "item": { "kind": "key", "name": "default" }, "action": "overwrite" }],
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let names = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if let Ok(tw_api::Event::GatewayKeysChanged { names, .. }) = events.recv().await {
                return names;
            }
        }
    })
    .await
    .expect("no gateway_keys_changed");
    assert_eq!(names, ["default"]);
}

/// 安全模式：配置读不进来，从备份恢复整份换回去，钥匙是现在这一把
#[tokio::test]
async fn in_safe_mode_a_backup_comes_back_whole() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, mine()).unwrap();
    // 先有一版好的备份
    tw_pack::backup::record(&p, &mine(), tw_config::history::Origin::Ui).unwrap();
    let broken = format!(
        "version: 1\nlisten:\n  control:\n    key: {KEY}\nclients:\n  - name: default\n    kye: x\n"
    );
    std::fs::write(&p, &broken).unwrap();
    let (cfg, r) = tw_config::stand_in(&broken).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let mgr = Arc::new(ConfigManager::new(p.clone(), gw.clone(), bus).standing_in(&r));
    let app = tw_control::router(state_for(p.clone(), mgr, gw));
    let (st, list) = call(&app, "GET", "/backups", serde_json::json!(null)).await;
    assert_eq!(st, StatusCode::OK, "{list}");
    let id = list[0]["id"].clone();
    let (st, preview) = call(
        &app,
        "POST",
        "/backups/preview",
        serde_json::json!({ "id": id }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{preview}");
    assert!(
        preview["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["change"] == "add")
    );
    let (st, v) = call(
        &app,
        "POST",
        "/backups/restore",
        serde_json::json!({ "id": id, "base_version": preview["base_version"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let text = std::fs::read_to_string(&p).unwrap();
    assert_eq!(text, mine());
}
