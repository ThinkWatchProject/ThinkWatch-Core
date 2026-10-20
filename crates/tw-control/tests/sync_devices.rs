//! 多设备同步走控制面：两台设备（两个数据目录、两份配置、两个控制面）经同一个文件夹位置。
//!
//! 合并规则本身在 `tw-sync` 的测试里逐条测过；这里测的是接缝：开启与加入、经写入流程应用
//! （来源记为同步、热更新）、状态事件、密钥变了的事件、冲突经端点处理、安全模式暂停、
//! 本机改动 10 秒后上传而另一台的文件夹监听即时收到。

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

const PASS: &str = "correct horse battery staple";

fn base(key: char) -> String {
    format!(
        "version: 1\nlisten:\n  control:\n    key: {}\nclients:\n  - name: default\n    key: tw-default-{key}\n",
        key.to_string().repeat(64)
    )
}

struct Dev {
    dir: tempfile::TempDir,
    state: ControlState,
    app: axum::Router,
}

impl Dev {
    fn new(key: char) -> Dev {
        tw_sync::keyring::set_work_factor_for_tests(4);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yaml");
        let text = base(key);
        std::fs::write(&p, &text).unwrap();
        let cfg = tw_config::try_parse(&text).unwrap();
        let gw = tw_gateway::AppState::new(cfg).unwrap();
        // 插件引擎用假的：照真的那一套读 manifest，不起沙箱
        gw.set_plugin_engine(Arc::new(tw_gateway::plugin::fake::FakeEngine));
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
        Dev {
            app: tw_control::router(state.clone()),
            state,
            dir,
        }
    }

    async fn call(&self, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let r = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(if body.is_null() {
                        String::new()
                    } else {
                        body.to_string()
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn ok(&self, method: &str, path: &str, body: Value) -> Value {
        let (s, v) = self.call(method, path, body).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }

    fn text(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("config.yaml")).unwrap()
    }

    fn config(&self) -> tw_config::Config {
        self.state.config().as_ref().clone()
    }

    /// 用户在界面上改了配置（`PUT /config`）
    async fn edit(&self, f: impl FnOnce(&str) -> String) {
        let cur = self.ok("GET", "/config", Value::Null).await;
        let next = f(cur["text"].as_str().unwrap());
        self.ok(
            "PUT",
            "/config",
            json!({ "base_version": cur["version"], "text": next }),
        )
        .await;
    }
}

fn folder(loc: &std::path::Path) -> Value {
    json!({ "type": "folder", "path": loc.display().to_string(), "proxy": "direct" })
}

fn add_provider(text: &str, name: &str, url: &str, key: &str) -> String {
    let p = format!("  - name: {name}\n    base_url: {url}\n    key: {key}\n");
    if text.contains("\nproviders:\n") {
        text.replacen("\nproviders:\n", &format!("\nproviders:\n{p}"), 1)
    } else {
        format!("{text}providers:\n{p}")
    }
}

/// A 开启（同步位置空着，加密），B 加入（新安装：直接用同步数据）
async fn pair(loc: &std::path::Path) -> (Dev, Dev) {
    let (a, b) = (Dev::new('a'), Dev::new('b'));
    let probe = a
        .ok("POST", "/sync/test", json!({ "target": folder(loc) }))
        .await;
    assert_eq!(probe["writable"], true);
    assert!(probe["existing"].is_null(), "空的同步位置：{probe}");
    // 口令太短不收
    let (s, v) = a
        .call(
            "POST",
            "/sync/enable",
            json!({ "target": folder(loc), "encrypt": true, "passphrase": "short", "device_name": "Mac mini", "join": "use_remote" }),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["code"], "sync.passphrase_short");
    a.ok(
        "POST",
        "/sync/enable",
        json!({ "target": folder(loc), "encrypt": true, "passphrase": PASS, "device_name": "Mac mini", "join": "use_remote" }),
    )
    .await;
    assert!(a.config().sync.is_some(), "同步设置写进了配置");
    let v = a.ok("GET", "/sync", Value::Null).await;
    assert_ne!(v["state"], "off", "开启之后马上就不是关着：{v}");
    assert_eq!(v["device"]["name"], "Mac mini");

    let probe = b
        .ok(
            "POST",
            "/sync/test",
            json!({ "target": folder(loc), "passphrase": PASS }),
        )
        .await;
    assert_eq!(probe["existing"]["encrypted"], true);
    assert_eq!(probe["existing"]["unlocked"], true);
    assert_eq!(probe["existing"]["devices"][0]["name"], "Mac mini");
    let preview = b
        .ok(
            "POST",
            "/sync/enable/preview",
            json!({ "target": folder(loc), "passphrase": PASS }),
        )
        .await;
    assert_eq!(preview["initial"], true, "新安装的配置不出现合并这一步");
    // 口令不对加入不了
    let (s, _) = b
        .call(
            "POST",
            "/sync/enable",
            json!({ "target": folder(loc), "encrypt": true, "passphrase": "wrong passphrase!", "device_name": "MacBook Air", "join": "use_remote" }),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    b.ok(
        "POST",
        "/sync/enable",
        json!({ "target": folder(loc), "encrypt": true, "passphrase": PASS, "device_name": "MacBook Air", "join": "use_remote" }),
    )
    .await;
    (a, b)
}

#[tokio::test]
async fn two_devices_turn_sync_on_and_changes_flow_both_ways() {
    let loc = tempfile::tempdir().unwrap();
    let (a, b) = pair(loc.path()).await;
    let mut events = b.state.bus().subscribe();

    a.edit(|t| add_provider(t, "relay", "https://relay.example/v1", "sk-relay"))
        .await;
    a.ok("POST", "/sync/now", Value::Null).await;
    let v = b.ok("POST", "/sync/now", Value::Null).await;
    assert_eq!(v["state"], "idle", "{v}");
    assert_eq!(v["encrypted"], true);
    assert_eq!(v["devices"].as_array().unwrap().len(), 2, "{v}");
    // B 收到了，而且已经生效（网关换入了新配置）
    let p = b.config().providers.into_iter().find(|p| p.name == "relay");
    assert!(p.is_some(), "{}", b.text());
    assert!(
        b.text().contains(&"b".repeat(64)),
        "B 的控制面钥匙是它自己的"
    );
    // 来源记为同步
    let mut saw_sync = false;
    let mut saw_status = false;
    while let Ok(ev) = events.try_recv() {
        match ev {
            tw_api::Event::ConfigReloaded {
                origin: tw_api::ConfigOrigin::Sync,
                ..
            } => saw_sync = true,
            tw_api::Event::SyncStatus { view, .. } => {
                saw_status = true;
                assert_eq!(view.device.unwrap().name, "MacBook Air");
            }
            _ => {}
        }
    }
    assert!(saw_sync, "应用别的设备的版本，来源是 sync");
    assert!(saw_status, "状态变了要发 sync_status");

    // 用同步数据加入：B 的网关密钥就是 A 的那一把
    assert_eq!(b.config().clients[0].key, "tw-default-a");
    // B 换了网关密钥的值：A 那边发 gateway_keys_changed
    let mut a_events = a.state.bus().subscribe();
    b.edit(|t| t.replace("tw-default-a", "tw-default-rotated"))
        .await;
    b.ok("POST", "/sync/now", Value::Null).await;
    a.ok("POST", "/sync/now", Value::Null).await;
    assert_eq!(a.config().clients[0].key, "tw-default-rotated");
    let mut names = None;
    while let Ok(ev) = a_events.try_recv() {
        if let tw_api::Event::GatewayKeysChanged { names: n, .. } = ev {
            names = Some(n);
        }
    }
    assert_eq!(names, Some(vec!["default".to_string()]));
}

#[tokio::test]
async fn a_conflict_is_listed_and_resolved_through_the_endpoints() {
    let loc = tempfile::tempdir().unwrap();
    let (a, b) = pair(loc.path()).await;
    a.edit(|t| add_provider(t, "relay", "https://relay.example", "sk-base"))
        .await;
    a.ok("POST", "/sync/now", Value::Null).await;
    b.ok("POST", "/sync/now", Value::Null).await;
    a.edit(|t| t.replace("sk-base", "sk-a")).await;
    b.edit(|t| t.replace("sk-base", "sk-b")).await;
    a.ok("POST", "/sync/now", Value::Null).await;
    let v = b.ok("POST", "/sync/now", Value::Null).await;
    assert_eq!(v["state"], "conflict", "{v}");
    assert_eq!(v["conflicts"], 1);
    let list = b.ok("GET", "/sync/conflicts", Value::Null).await;
    let c = &list[0];
    assert_eq!(c["item"], json!({ "kind": "provider", "name": "relay" }));
    assert_eq!(c["units"], json!(["conn"]));
    assert_eq!(c["local"]["device"], "MacBook Air");
    assert_eq!(c["remote"]["device"], "Mac mini");
    assert_eq!(c["both_allowed"], true);
    assert_eq!(c["diff"][0]["unit"], "conn");
    assert_eq!(c["diff"][0]["local"]["key"], "sk-b");
    assert_eq!(c["diff"][0]["remote"]["key"], "sk-a");
    // A 看到的是同一处，本机那一边是 A
    a.ok("POST", "/sync/now", Value::Null).await;
    let la = a.ok("GET", "/sync/conflicts", Value::Null).await;
    assert_eq!(la[0]["id"], c["id"]);
    assert_eq!(la[0]["local"]["device"], "Mac mini");

    let v = b
        .ok(
            "POST",
            "/sync/conflicts",
            json!({ "decisions": [{ "id": c["id"], "take": "remote" }] }),
        )
        .await;
    assert_eq!(v["conflicts"], 0, "{v}");
    let key = |d: &Dev| {
        d.config()
            .providers
            .into_iter()
            .find(|p| p.name == "relay")
            .unwrap()
            .key
            .unwrap()
            .raw()
            .to_string()
    };
    assert_eq!(key(&b), "sk-a");
    let v = a.ok("POST", "/sync/now", Value::Null).await;
    assert_eq!(v["conflicts"], 0, "A 上的冲突随之清除：{v}");
    assert_eq!(key(&a), "sk-a");
}

#[tokio::test]
async fn joining_with_a_configuration_merges_by_the_import_rules() {
    let loc = tempfile::tempdir().unwrap();
    let a = Dev::new('a');
    a.edit(|t| add_provider(t, "shared", "https://a.example", "sk-a"))
        .await;
    a.ok(
        "POST",
        "/sync/enable",
        json!({ "target": folder(loc.path()), "encrypt": false, "device_name": "A", "join": "use_remote" }),
    )
    .await;
    let b = Dev::new('b');
    b.edit(|t| {
        let t = add_provider(t, "shared", "https://b.example", "sk-b");
        add_provider(&t, "mine", "https://mine.example", "sk-m")
    })
    .await;
    let preview = b
        .ok(
            "POST",
            "/sync/enable/preview",
            json!({ "target": folder(loc.path()) }),
        )
        .await;
    assert_eq!(preview["initial"], false);
    let row = preview["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["item"]["name"] == "shared")
        .unwrap()
        .clone();
    assert_eq!(row["status"], "conflict");
    assert_eq!(row["suggested_name"], "shared (2)");
    // 冲突没选：不收
    let (s, v) = b
        .call(
            "POST",
            "/sync/enable",
            json!({ "target": folder(loc.path()), "encrypt": false, "device_name": "B", "join": "merge" }),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["code"], "sync.join_undecided");
    b.ok(
        "POST",
        "/sync/enable",
        json!({
            "target": folder(loc.path()), "encrypt": false, "device_name": "B", "join": "merge",
            "decisions": [
                { "item": { "kind": "provider", "name": "shared" }, "action": "rename", "new_name": "shared (A)" },
            ],
        }),
    )
    .await;
    let names = |d: &Dev| {
        let mut v: Vec<String> = d.config().providers.into_iter().map(|p| p.name).collect();
        v.sort();
        v
    };
    assert_eq!(names(&b), ["mine", "shared", "shared (A)"]);
    a.ok("POST", "/sync/now", Value::Null).await;
    assert_eq!(names(&a), ["mine", "shared", "shared (A)"], "{}", a.text());
    // 两边各自生成的默认密钥值不同：不用选，B 采用同步数据的值，A 的不变
    assert_eq!(b.config().clients[0].key, "tw-default-a");
    assert_eq!(a.config().clients[0].key, "tw-default-a");
}

#[tokio::test]
async fn an_excluded_upstream_is_marked_and_stays_local() {
    let loc = tempfile::tempdir().unwrap();
    let (a, b) = pair(loc.path()).await;
    a.edit(|t| add_provider(t, "ollama", "http://127.0.0.1:11434", "none"))
        .await;
    a.ok("PUT", "/sync/settings", json!({ "excluded": ["ollama"] }))
        .await;
    let v = a.ok("POST", "/sync/now", Value::Null).await;
    assert_eq!(v["excluded"], json!(["ollama"]));
    b.ok("POST", "/sync/now", Value::Null).await;
    assert!(b.config().providers.is_empty(), "{}", b.text());
    let ov = a.ok("GET", "/overview", Value::Null).await;
    assert_eq!(ov["providers"][0]["sync_excluded"], true);
    // 换成另一个同步位置不在这里改
    let other = tempfile::tempdir().unwrap();
    let (s, v) = a
        .call(
            "PUT",
            "/sync/settings",
            json!({ "target": folder(other.path()) }),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(v["code"], "sync.place_changed");
}

#[tokio::test]
async fn safe_mode_pauses_sync() {
    let loc = tempfile::tempdir().unwrap();
    let (a, _b) = pair(loc.path()).await;
    tw_control::sync::start(&a.state, true);
    a.edit(|t| add_provider(t, "relay", "https://relay.example", "sk"))
        .await;
    let v = a.ok("POST", "/sync/now", Value::Null).await;
    assert_eq!(v["state"], "paused");
    let names: Vec<String> = std::fs::read_dir(loc.path().join("versions"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(names.len(), 2, "暂停时不上传：{names:?}");
}

#[tokio::test]
async fn turning_sync_off_keeps_the_configuration_and_purging_empties_the_location() {
    let loc = tempfile::tempdir().unwrap();
    let (a, b) = pair(loc.path()).await;
    a.edit(|t| add_provider(t, "relay", "https://relay.example", "sk"))
        .await;
    a.ok("POST", "/sync/now", Value::Null).await;
    a.ok("POST", "/sync/disable", json!({ "purge": true }))
        .await;
    assert!(a.config().sync.is_none());
    assert!(!a.config().providers.is_empty(), "本机保留当前配置");
    assert!(!a.dir.path().join("sync").exists(), "本机的同步状态删掉了");
    assert!(!loc.path().join("keyring").exists());
    // B：读到空的先不下结论；连着几轮、过了一段时间才是「同步位置空了」，同步设置不动，由人
    // 选重新上传或者停止
    for _ in 0..3 {
        let v = b.ok("POST", "/sync/now", Value::Null).await;
        assert_eq!(v["error"]["code"], "sync.looks_empty", "{v}");
        assert_ne!(v["state"], "emptied");
    }
    let local = tw_sync::local::Local::new(b.dir.path());
    let mut st = local.state();
    st.empty_since_ms = st.empty_since_ms.map(|t| t - 11 * 60 * 1000);
    local.save_state(&st).unwrap();
    let v = b.ok("POST", "/sync/now", Value::Null).await;
    assert_eq!(v["state"], "emptied", "{v}");
    assert_eq!(v["error"]["code"], "sync.location_empty");
    assert!(b.config().sync.is_some(), "同步设置不动");
    // 重新上传本机配置：keyring 放回 B 记着的那一份，版本以 B 的配置上传
    let v = b.ok("POST", "/sync/republish", Value::Null).await;
    assert_eq!(v["state"], "idle", "{v}");
    assert!(v["error"].is_null(), "{v}");
    assert!(loc.path().join("keyring").exists());
    assert_eq!(
        std::fs::read_dir(loc.path().join("versions"))
            .unwrap()
            .count(),
        1
    );
}

/// 这台设备最近一次同步成功的时刻（`last_synced_ms`）。还没成功过是 0
async fn synced_at(d: &Dev) -> u64 {
    d.ok("GET", "/sync", Value::Null).await["last_synced_ms"]
        .as_u64()
        .unwrap_or(0)
}

/// 调度：本机改了配置，10 秒合并之后上传；另一台的文件夹监听一收到就合并
#[tokio::test]
async fn a_local_change_is_uploaded_after_it_settles_and_the_folder_watcher_picks_it_up() {
    let loc = tempfile::tempdir().unwrap();
    let (a, b) = pair(loc.path()).await;
    let before = [synced_at(&a).await, synced_at(&b).await];
    tw_control::sync::start(&a.state, false);
    tw_control::sync::start(&b.state, false);
    // 启动那一轮先跑完。**不按固定的时长等**：机器慢的时候那一轮还没走完，下面第一处改动
    // 就被它顺手传上去了，看起来像是没等稳定期
    let started = std::time::Instant::now();
    loop {
        let (sa, sb) = (synced_at(&a).await, synced_at(&b).await);
        let busy = a.ok("GET", "/sync", Value::Null).await["state"] == "syncing"
            || b.ok("GET", "/sync", Value::Null).await["state"] == "syncing";
        if sa > before[0] && sb > before[1] && !busy {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "启动那一轮 20 秒没跑完"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    a.edit(|t| add_provider(t, "first", "https://first.example", "sk-1"))
        .await;
    a.edit(|t| add_provider(t, "second", "https://second.example", "sk-2"))
        .await;
    let started = std::time::Instant::now();
    loop {
        let names: Vec<String> = b.config().providers.into_iter().map(|p| p.name).collect();
        if names.len() == 2 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(40),
            "40 秒内没收到：{names:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        started.elapsed() >= tw_control::sync::SETTLE - Duration::from_millis(500),
        "改动要稳下来才上传"
    );
    // 两次改动合成了一个版本：开启、加入、这一次
    let names: Vec<String> = std::fs::read_dir(loc.path().join("versions"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert!(names.len() <= 2, "{names:?}");
    // A 那一轮的尾巴（写设备记录）可能还没走完：不该一直停在「正在同步」
    let mut v = a.ok("GET", "/sync", Value::Null).await;
    for _ in 0..50 {
        if v["state"] != "syncing" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        v = a.ok("GET", "/sync", Value::Null).await;
    }
    assert_eq!(v["state"], "idle", "{v}");
}

/// 不加密的同步：经同步新增的插件在本设备停用着，要在本设备启用；上游的地址经同步改了要
/// 标出来（改之前的地址、哪台设备）
#[tokio::test]
async fn without_encryption_arriving_plugins_start_disabled_and_address_changes_are_flagged() {
    let loc = tempfile::tempdir().unwrap();
    let (a, b) = (Dev::new('a'), Dev::new('b'));
    a.edit(|t| add_provider(t, "relay", "https://relay.example", "sk"))
        .await;
    a.ok(
        "POST",
        "/sync/enable",
        json!({ "target": folder(loc.path()), "encrypt": false, "device_name": "Mac mini", "join": "use_remote" }),
    )
    .await;
    b.ok(
        "POST",
        "/sync/enable",
        json!({ "target": folder(loc.path()), "encrypt": false, "device_name": "MacBook Air", "join": "use_remote" }),
    )
    .await;
    let src = tw_gateway::plugin::fake::source(
        json!({ "name": "Shout", "api": 1, "permissions": ["reply.text"] }),
        &["onReplyText"],
    );
    let ov = a.ok("GET", "/overview", Value::Null).await;
    a.ok(
        "POST",
        "/plugins",
        json!({ "source": src, "enabled": true, "base_version": ov["config_version"] }),
    )
    .await;
    a.edit(|t| t.replace("https://relay.example", "https://moved.example"))
        .await;
    a.ok("POST", "/sync/now", Value::Null).await;
    b.ok("POST", "/sync/now", Value::Null).await;
    let bp = b.config().plugins;
    assert_eq!(bp.len(), 1, "{}", b.text());
    assert!(
        !bp[0].enabled,
        "经不加密的同步新来的插件，在这台设备上停用着"
    );
    assert!(a.config().plugins[0].enabled, "A 上照旧开着");
    let ov = b.ok("GET", "/overview", Value::Null).await;
    let relay = ov["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "relay")
        .unwrap()
        .clone();
    assert_eq!(relay["base_url"], "https://moved.example");
    assert_eq!(relay["sync_url_changed"]["from"], "https://relay.example");
    assert_eq!(relay["sync_url_changed"]["device"], "Mac mini");
    // 再同步几轮：B 的停用不会传回去
    b.ok("POST", "/sync/now", Value::Null).await;
    a.ok("POST", "/sync/now", Value::Null).await;
    assert!(a.config().plugins[0].enabled);
    // B 在本机启用：之后就和别的设备一样了
    let id = bp[0].id.clone();
    let cur = b.ok("GET", "/config", Value::Null).await;
    let text = cur["text"]
        .as_str()
        .unwrap()
        .replace("enabled: false", "enabled: true");
    b.ok(
        "PUT",
        "/config",
        json!({ "base_version": cur["version"], "text": text }),
    )
    .await;
    b.ok("POST", "/sync/now", Value::Null).await;
    a.ok("POST", "/sync/now", Value::Null).await;
    assert!(b.config().plugins.iter().any(|p| p.id == id && p.enabled));
    assert!(a.config().plugins[0].enabled);
}

/// 安全模式：配置读不进来，在服务的是一份临时配置。同步不自动跑，状态是 `paused`；同步位置上
/// 的版本照样列得出来（出错页的「从备份恢复」要用）
#[tokio::test]
async fn in_safe_mode_the_sync_location_still_lists_its_versions() {
    let loc = tempfile::tempdir().unwrap();
    let (a, _b) = pair(loc.path()).await;
    let path = a.dir.path().join("config.yaml");
    let broken = format!("{}a_field_from_nowhere: 1\n", a.text());
    std::fs::write(&path, &broken).unwrap();
    let (cfg, rejected) = tw_config::stand_in(&broken).expect("钥匙还在，顶得上");
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let state = ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: Arc::new(ConfigManager::new(path, gw.clone(), bus).standing_in(&rejected)),
        gateway: gw,
        store: None,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    };
    let sync = tw_control::sync::start(&state, true);
    let v = sync.view();
    assert_eq!(v.state, tw_api::SyncState::Paused);
    assert!(v.target.is_some(), "同步位置照原文里的");
    let versions = sync.remote_versions().await;
    assert_eq!(versions.len(), 2, "{versions:?}");
    assert!(versions.iter().any(|v| v.device_name == "Mac mini"));
    let pack = sync
        .remote_pack(&versions[0].content_id)
        .await
        .unwrap()
        .expect("按内容标识取得到");
    assert!(!pack.config_yaml.contains("listen"), "同步版本不带本机设置");
}

/// 备份列表里有同步位置上的版本（本机也有的同一份合成一行，只在同步位置上的是
/// `sync-<content_id>`）；经同步应用的本机版本记着「同步自」哪台设备；恢复预览说得出会同步到
/// 几台别的设备；接管的客户端在用的密钥（`PUT /pinned-keys`）不被别的设备删掉
#[tokio::test]
async fn backups_see_the_sync_location_and_pinned_keys_survive() {
    let loc = tempfile::tempdir().unwrap();
    let (a, b) = pair(loc.path()).await;
    a.edit(|t| {
        let t = add_provider(t, "relay", "https://relay.example", "sk");
        t.replace(
            "clients:\n",
            "clients:\n  - name: codex\n    key: tw-codex-0002\n",
        )
    })
    .await;
    a.ok("POST", "/sync/now", Value::Null).await;
    b.ok("POST", "/sync/now", Value::Null).await;

    let list = b.ok("GET", "/backups", Value::Null).await;
    let rows = list.as_array().unwrap();
    let synced = rows
        .iter()
        .find(|r| r["origin"] == "sync" && r["local"] == true)
        .unwrap_or_else(|| panic!("经同步应用的那一版：{list}"));
    assert_eq!(synced["device"], "Mac mini", "同步自哪台设备：{synced}");
    assert_eq!(synced["sync"], true, "同步位置上也有这一份：{synced}");
    assert_eq!(synced["current"], true);
    // A 那边看：同步位置上只有、本机没有的版本（B 加入时上传的那一版）是 sync-<content_id>
    let la = a.ok("GET", "/backups", Value::Null).await;
    let remote_only = la
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["local"] == false)
        .cloned();
    if let Some(r) = remote_only {
        assert!(r["id"].as_str().unwrap().starts_with("sync-"), "{r}");
        let file = a
            .ok("POST", "/backups/file", json!({ "id": r["id"] }))
            .await;
        assert!(file["data_b64"].as_str().is_some_and(|d| !d.is_empty()));
    }
    let mine = la
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["local"] == true)
        .unwrap()
        .clone();
    let preview = a
        .ok("POST", "/backups/preview", json!({ "id": mine["id"] }))
        .await;
    assert_eq!(
        preview["sync_devices"], 1,
        "恢复之后会同步到另一台：{preview}"
    );

    // B 上接管的客户端在用 codex；A 删了它：B 不删，作为冲突列出
    b.ok("PUT", "/pinned-keys", json!({ "names": ["codex"] }))
        .await;
    a.edit(|t| t.replace("  - name: codex\n    key: tw-codex-0002\n", ""))
        .await;
    a.ok("POST", "/sync/now", Value::Null).await;
    let v = b.ok("POST", "/sync/now", Value::Null).await;
    assert!(
        b.config().clients.iter().any(|c| c.name == "codex"),
        "{}",
        b.text()
    );
    assert_eq!(v["conflicts"], 1, "{v}");
    let c = b.ok("GET", "/sync/conflicts", Value::Null).await;
    assert_eq!(c[0]["reason"], "key_in_use");
    // 关掉同步：备份列表不再看同步位置
    a.ok("POST", "/sync/disable", json!({ "purge": false }))
        .await;
    let la = a.ok("GET", "/backups", Value::Null).await;
    assert!(
        la.as_array().unwrap().iter().all(|r| r["sync"] == false),
        "{la}"
    );
}
