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

/// 较旧版本导出的包：读不进来的字段在内存里修好，预览说修了哪几处；更新的版本写的说太新
#[tokio::test]
async fn an_older_pack_is_previewed_repaired_and_a_newer_one_is_too_new() {
    let dst = bed(&mine());
    let pack_b64 = |core: &str| {
        let mut m = tw_pack::Manifest::new(tw_pack::PackKind::Full, tw_pack::PackOrigin::Export);
        m.core_version = core.to_string();
        let p = tw_pack::Pack {
            manifest: m,
            config_yaml: "version: 1\nproviders:\n  - name: old\n    base_url: https://old.example\n    retired_field: 1\nclient_probes:\n  titling: passthrough\n".into(),
            plugins: Default::default(),
            client_prefs: None,
        };
        base64::engine::general_purpose::STANDARD
            .encode(tw_pack::write(&p, tw_pack::Seal::None).unwrap())
    };
    let old = pack_b64("0.1.0");
    let (st, preview) = call(
        &dst.app,
        "POST",
        "/config/import/preview",
        serde_json::json!({ "data_b64": old }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{preview}");
    let fields: Vec<&str> = preview["repairs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["field"].as_str().unwrap())
        .collect();
    assert_eq!(
        fields,
        ["providers[0].retired_field", "client_probes.titling"]
    );
    assert_eq!(preview["repairs"][1]["kind"], "unknown_value");
    let (st, v) = call(
        &dst.app,
        "POST",
        "/config/import",
        serde_json::json!({
            "data_b64": old,
            "base_version": preview["base_version"],
            "decisions": [{ "item": { "kind": "provider", "name": "old" }, "action": "import" }],
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(dst.file().contains("https://old.example"));
    assert!(!dst.file().contains("retired_field"));
    let (st, v) = call(
        &dst.app,
        "POST",
        "/config/import/preview",
        serde_json::json!({ "data_b64": pack_b64("999.0.0") }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["code"], "pack.too_new");
}

/// 导入的请求体放宽到装得下一个最大的包；别的端点照旧
#[tokio::test]
async fn a_large_pack_reaches_the_import_and_other_bodies_keep_the_default_limit() {
    let b = bed(&mine());
    // 3 MiB 的 base64：超过默认的 2 MiB，远不到配置包的上限
    let big = "A".repeat(3 * 1024 * 1024);
    let (st, v) = call(
        &b.app,
        "POST",
        "/config/import/preview",
        serde_json::json!({ "data_b64": big }),
    )
    .await;
    assert_ne!(st, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
    assert_eq!(v["code"], "pack.corrupt", "{v}");
    let (st, _) = call(
        &b.app,
        "PUT",
        "/client-prefs",
        serde_json::json!({ "prefs": { "big": big } }),
    )
    .await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE);
}

/// 固定密钥按来源记：两个 Lite 各报各的，取并集；一个来源再报一次换掉它自己的
#[tokio::test]
async fn pinned_keys_are_kept_per_source() {
    let b = bed(&mine());
    for (source, names) in [("lite-a", vec!["default"]), ("lite-b", vec!["claude-code"])] {
        let (st, v) = call(
            &b.app,
            "PUT",
            "/pinned-keys",
            serde_json::json!({ "source": source, "names": names }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
    }
    let all: Vec<String> = b.state.cfg.pinned_keys().into_iter().collect();
    assert_eq!(all, ["claude-code", "default"]);
    call(
        &b.app,
        "PUT",
        "/pinned-keys",
        serde_json::json!({ "source": "lite-a", "names": [] }),
    )
    .await;
    let all: Vec<String> = b.state.cfg.pinned_keys().into_iter().collect();
    assert_eq!(all, ["claude-code"]);
}

/// 导入的包里带着超大的通用偏好：和 `PUT /client-prefs` 一样挡下
#[tokio::test]
async fn oversized_preferences_in_a_pack_are_refused() {
    let dst = bed(&mine());
    let mut m = tw_pack::Manifest::new(tw_pack::PackKind::Full, tw_pack::PackOrigin::Export);
    m.secrets = true;
    let p = tw_pack::Pack {
        manifest: m,
        config_yaml: "version: 1\n".into(),
        plugins: Default::default(),
        client_prefs: Some(serde_json::json!({ "big": "x".repeat(tw_api::CLIENT_PREFS_MAX + 1) })),
    };
    let data = base64::engine::general_purpose::STANDARD
        .encode(tw_pack::write(&p, tw_pack::Seal::None).unwrap());
    let (_, preview) = call(
        &dst.app,
        "POST",
        "/config/import/preview",
        serde_json::json!({ "data_b64": data }),
    )
    .await;
    let (st, v) = call(
        &dst.app,
        "POST",
        "/config/import",
        serde_json::json!({
            "data_b64": data,
            "base_version": preview["base_version"],
            "decisions": [{ "item": { "kind": "client_prefs", "name": "" }, "action": "import" }],
        }),
    )
    .await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
    assert_eq!(v["code"], "backup.prefs_too_large");
    assert!(tw_pack::disk::read_prefs(&dst.path()).is_none());
}

/// 同步位置上的版本：里面没有本机那几节
struct OneRemote(tw_pack::Pack);

impl tw_control::backups::RemoteBackups for OneRemote {
    fn list(
        &self,
    ) -> tw_control::backups::BoxFuture<'_, Vec<tw_control::backups::RemoteVersionInfo>> {
        let v = tw_control::backups::RemoteVersionInfo {
            content_id: self.0.content_id(),
            at_ms: 1,
            device_name: "MacBook Air".into(),
            core_version: tw_pack::CORE_VERSION.into(),
            counts: self.0.manifest.counts,
        };
        Box::pin(async move { vec![v] })
    }
    fn fetch<'a>(
        &'a self,
        _content_id: &'a str,
    ) -> tw_control::backups::BoxFuture<'a, Result<tw_pack::Pack, tw_types::Msg>> {
        let p = self.0.clone();
        Box::pin(async move { Ok(p) })
    }
}

/// 安全模式下整份恢复同步位置上的一个版本：钥匙、监听是现在这份原文里的，写得进去
#[tokio::test]
async fn in_safe_mode_a_synced_version_comes_back_whole() {
    let src = bed(&theirs());
    let file = export_all(&src, true).await;
    let synced = tw_pack::read(
        &base64::engine::general_purpose::STANDARD
            .decode(file["data_b64"].as_str().unwrap())
            .unwrap(),
        tw_pack::Unseal::None,
    )
    .unwrap();
    assert!(!synced.config_yaml.contains("listen"));
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    let broken = format!("{}not_a_field: 1\n", mine());
    std::fs::write(&p, &broken).unwrap();
    let (cfg, r) = tw_config::stand_in(&broken).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let mgr = Arc::new(ConfigManager::new(p.clone(), gw.clone(), bus).standing_in(&r));
    mgr.set_remote_backups(Some(Arc::new(OneRemote(synced.clone()))));
    let app = tw_control::router(state_for(p.clone(), mgr, gw));
    let (st, list) = call(&app, "GET", "/backups", serde_json::json!(null)).await;
    assert_eq!(st, StatusCode::OK, "{list}");
    let id = list[0]["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("sync-"), "{list}");
    let (st, preview) = call(
        &app,
        "POST",
        "/backups/preview",
        serde_json::json!({ "id": id }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{preview}");
    let (st, v) = call(
        &app,
        "POST",
        "/backups/restore",
        serde_json::json!({ "id": id, "base_version": preview["base_version"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let text = std::fs::read_to_string(&p).unwrap();
    let cfg = tw_config::try_parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    assert_eq!(cfg.listen.control.key.as_deref(), Some(KEY));
    assert!(cfg.providers.iter().any(|p| p.name == "relay"));
}

fn rows_of(v: &serde_json::Value) -> Vec<String> {
    v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            format!(
                "{} {}/{}{}",
                r["change"].as_str().unwrap(),
                r["item"]["kind"].as_str().unwrap(),
                r["item"]["name"].as_str().unwrap(),
                r["kept"]
                    .as_str()
                    .map(|k| format!(" ({k})"))
                    .unwrap_or_default()
            )
        })
        .collect()
}

/// 只恢复几项：预览接收所选的项目，和写入走同一个计划；写入的结果说实际动了哪几项
#[tokio::test]
async fn a_partial_restore_writes_what_its_preview_listed() {
    let with = format!(
        "{}proxies:\n  - name: px\n    type: http\n    addr: 10.0.0.1:8080\n",
        mine()
    )
    .replace(
        "routes:",
        "  - name: gone\n    base_url: https://gone.example\n    key: sk-g\n    proxy: px\nroutes:",
    );
    tw_config::try_parse(&with).unwrap();
    let b = bed(&with);
    b.state.cfg.record_startup();
    b.state
        .cfg
        .write(&mine(), None, tw_config::history::Origin::Ui)
        .await
        .unwrap();
    let (_, list) = call(&b.app, "GET", "/backups", serde_json::json!(null)).await;
    let oldest = list.as_array().unwrap().last().unwrap()["id"].clone();
    let items = serde_json::json!([{ "kind": "provider", "name": "gone" }]);
    let (st, preview) = call(
        &b.app,
        "POST",
        "/backups/preview",
        serde_json::json!({ "id": oldest, "items": items }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{preview}");
    assert_eq!(rows_of(&preview), ["add provider/gone", "add proxy/px"]);
    let (st, v) = call(
        &b.app,
        "POST",
        "/backups/restore",
        serde_json::json!({ "id": oldest, "base_version": preview["base_version"], "items": items }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(rows_of(&v), rows_of(&preview));
    assert_eq!(v["version"], serde_json::json!(b.version()));
    let cfg = tw_config::try_parse(&b.file()).unwrap();
    assert_eq!(cfg.proxies.len(), 1);
}

/// 恢复同步位置上的版本：本机不同步的上游（和指向它的路由）那一版里本来就没有，不列成删除
#[tokio::test]
async fn restoring_a_synced_version_leaves_what_this_machine_does_not_sync() {
    let d = tempfile::tempdir().unwrap();
    let here = format!(
        "{}sync:\n  device_id: 3f9a2c1e0b1d4e2f\n  device_name: srv\n  encrypt: false\n  exclude: [ollama]\n  target:\n    type: folder\n    path: {}\n",
        mine().replace(
            "routes:",
            "  - name: ollama\n    base_url: http://127.0.0.1:11434\nroutes:\n  - name: local\n    rules:\n      - name: all\n        to: ollama\n"
        ),
        d.path().join("sync").display()
    );
    tw_config::try_parse(&here).unwrap_or_else(|e| panic!("{e}\n{here}"));
    // 同步位置上的那一版：没有 ollama 和 local，官方的地址不一样
    let synced = {
        let mut m = tw_pack::Manifest::new(tw_pack::PackKind::Full, tw_pack::PackOrigin::Sync);
        m.secrets = true;
        tw_pack::Pack {
            manifest: m,
            config_yaml: mine()
                .replace("https://api.anthropic.com\n", "https://synced.example\n")
                .replace(&format!("listen:\n  control:\n    key: {KEY}\n"), ""),
            plugins: Default::default(),
            client_prefs: None,
        }
    };
    let b = bed(&here);
    b.state
        .cfg
        .set_remote_backups(Some(Arc::new(OneRemote(synced))));
    let (_, list) = call(&b.app, "GET", "/backups", serde_json::json!(null)).await;
    let id = list
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"].as_str().unwrap().starts_with("sync-"))
        .unwrap()["id"]
        .clone();
    let (st, preview) = call(
        &b.app,
        "POST",
        "/backups/preview",
        serde_json::json!({ "id": id }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{preview}");
    assert_eq!(rows_of(&preview), ["modify provider/官方"]);
    let (st, v) = call(
        &b.app,
        "POST",
        "/backups/restore",
        serde_json::json!({ "id": id, "base_version": preview["base_version"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let cfg = tw_config::try_parse(&b.file()).unwrap();
    let names: Vec<&str> = cfg.providers.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["官方", "ollama"]);
    assert_eq!(cfg.providers[0].base_url, "https://synced.example");
    assert!(cfg.routes.iter().any(|r| r.name == "local"));
}

/// 固定密钥：来源不过期，有上限；早先的文件照样算；同时报的几个来源谁也不盖掉谁
#[tokio::test]
async fn pinned_keys_have_limits_and_an_older_file_still_counts() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, mine()).unwrap();
    std::fs::write(
        tw_pack::disk::pinned_path(&p),
        br#"{"names":["claude-code"]}"#,
    )
    .unwrap();
    let gw = tw_gateway::AppState::new(tw_config::try_parse(&mine()).unwrap()).unwrap();
    let bus = gw.bus.clone();
    let mgr = Arc::new(ConfigManager::new(p.clone(), gw.clone(), bus));
    assert!(mgr.pinned_keys().contains("claude-code"));
    let app = tw_control::router(state_for(p.clone(), mgr.clone(), gw));
    for (body, code) in [
        (
            serde_json::json!({ "source": "x".repeat(129), "names": ["default"] }),
            "backup.pinned.source_too_long",
        ),
        (
            serde_json::json!({ "source": "a", "names": ["n".repeat(257)] }),
            "backup.pinned.name_too_long",
        ),
        (
            serde_json::json!({ "source": "a", "names": (0..1025).map(|i| i.to_string()).collect::<Vec<_>>() }),
            "backup.pinned.too_many",
        ),
    ] {
        let (st, v) = call(&app, "PUT", "/pinned-keys", body).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["code"], code);
    }
    let tasks: Vec<_> = (0..16)
        .map(|i| {
            let app = app.clone();
            tokio::spawn(async move {
                call(
                    &app,
                    "PUT",
                    "/pinned-keys",
                    serde_json::json!({ "source": format!("lite-{i}"), "names": [format!("k{i}")] }),
                )
                .await
                .0
            })
        })
        .collect();
    for t in tasks {
        assert_eq!(t.await.unwrap(), StatusCode::OK);
    }
    let on_disk = tw_pack::disk::read_pinned(&p);
    for i in 0..16 {
        assert!(on_disk.contains(&format!("k{i}")), "{on_disk:?}");
    }
    assert!(on_disk.contains("claude-code"));
    assert_eq!(on_disk, mgr.pinned_keys());
}

/// 已经存着一份超大的通用偏好（上限之前存的）：恢复、导入照样能做，只有要写下去的新偏好才查
#[tokio::test]
async fn a_large_preference_already_here_does_not_block_a_restore() {
    let b = bed(&mine());
    let big = serde_json::json!({ "big": "x".repeat(tw_api::CLIENT_PREFS_MAX + 1) });
    std::fs::write(
        tw_pack::disk::prefs_path(&b.path()),
        serde_json::to_vec(&big).unwrap(),
    )
    .unwrap();
    b.state.cfg.record_startup();
    b.state
        .cfg
        .write(
            &mine().replace("https://api.anthropic.com\n", "https://changed.example\n"),
            None,
            tw_config::history::Origin::Ui,
        )
        .await
        .unwrap();
    let (_, list) = call(&b.app, "GET", "/backups", serde_json::json!(null)).await;
    let oldest = list.as_array().unwrap().last().unwrap()["id"].clone();
    let (_, preview) = call(
        &b.app,
        "POST",
        "/backups/preview",
        serde_json::json!({ "id": oldest }),
    )
    .await;
    let (st, v) = call(
        &b.app,
        "POST",
        "/backups/restore",
        serde_json::json!({ "id": oldest, "base_version": preview["base_version"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(tw_pack::disk::read_prefs(&b.path()), Some(big));
}

/// 启动时从备份里找回插件文件（写插件文件和写配置之间断过电）：网关照找回来的重读，插件
/// 照常跑。读不成 YAML 的原文不记成版本
#[tokio::test]
async fn a_plugin_put_back_at_startup_runs_again() {
    use tw_gateway::plugin::fake::{FakeEngine, source};
    let v1 = source(
        serde_json::json!({"name": "A", "api": 1, "permissions": ["system"]}),
        &["onRequest"],
    );
    let v2 = source(
        serde_json::json!({"name": "A2", "api": 1, "permissions": ["system"]}),
        &["onRequest"],
    );
    let sha = tw_gateway::plugin::load::sha256_hex(v1.as_bytes());
    let text = format!(
        "{}plugins:\n  - id: a\n    file: plugins/a.js\n    sha256: {sha}\n",
        mine()
    );
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, &text).unwrap();
    let files = |s: &str| tw_pack::PluginFiles {
        script: s.as_bytes().to_vec(),
        approved: Some(s.as_bytes().to_vec()),
    };
    let one = |s: &str| std::collections::BTreeMap::from([("a".to_string(), files(s))]);
    tw_pack::disk::write_plugins(&p, &Default::default(), &one(&v1)).unwrap();
    tw_pack::backup::record(&p, &text, tw_config::history::Origin::Ui).unwrap();
    // 断在半路：两份文件都已经是新的，配置还是旧的
    tw_pack::disk::write_plugins(&p, &one(&v1), &one(&v2)).unwrap();
    let gw = tw_gateway::AppState::new(tw_config::try_parse(&text).unwrap()).unwrap();
    gw.set_plugin_engine(Arc::new(FakeEngine));
    let bus = gw.bus.clone();
    let mgr = Arc::new(ConfigManager::new(p.clone(), gw.clone(), bus));
    let app = tw_control::router(state_for(p.clone(), mgr.clone(), gw));
    let status = || async {
        let (_, v) = call(&app, "GET", "/plugins", serde_json::json!(null)).await;
        v[0]["status"].clone()
    };
    assert_eq!(status().await, serde_json::json!({"kind": "changed"}));
    mgr.record_startup();
    assert_eq!(status().await, serde_json::json!({"kind": "ok"}));
    assert_eq!(
        std::fs::read_to_string(tw_config::plugins::file_path(d.path(), "a")).unwrap(),
        v1
    );

    let before = tw_pack::backup::list(&p).unwrap().len();
    std::fs::write(&p, "listen: [\n  :::\n").unwrap();
    mgr.record_startup();
    assert_eq!(tw_pack::backup::list(&p).unwrap().len(), before);
}
