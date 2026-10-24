//! 审查里找出来的同步问题在控制面这一层钉住：等登录的上游（加入的两种方式、登录之后）、要人
//! 处理的状态（keyring 不见了）、冲突给出能选的处理、关闭加密之后口令留着。

use std::sync::Arc;

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
    /// 数据目录：活到测试结束
    _dir: tempfile::TempDir,
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
        gw.set_plugin_engine(Arc::new(tw_gateway::plugin::fake::FakeEngine));
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
        Dev {
            app: tw_control::router(state.clone()),
            state,
            _dir: dir,
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

    fn config(&self) -> tw_config::Config {
        self.state.config().as_ref().clone()
    }

    fn provider(&self, name: &str) -> Option<tw_config::Provider> {
        self.config().providers.into_iter().find(|p| p.name == name)
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

    async fn enable(&self, loc: &std::path::Path, encrypt: bool, name: &str, join: Value) {
        let mut body = json!({
            "target": folder(loc), "encrypt": encrypt, "device_name": name, "join": "use_remote",
        });
        if encrypt {
            body["passphrase"] = json!(PASS);
        }
        if let Value::Object(extra) = join {
            for (k, v) in extra {
                body[k] = v;
            }
        }
        self.ok("POST", "/sync/enable", body).await;
    }
}

fn folder(loc: &std::path::Path) -> Value {
    json!({ "type": "folder", "path": loc.display().to_string(), "proxy": "direct" })
}

fn add_raw(text: &str, yaml: &str) -> String {
    if text.contains("\nproviders:\n") {
        text.replacen("\nproviders:\n", &format!("\nproviders:\n{yaml}"), 1)
    } else {
        format!("{text}providers:\n{yaml}")
    }
}

const CHATGPT: &str = "  - name: chatgpt\n    base_url: https://chatgpt.com/backend-api/codex\n    oauth:\n      refresh: rt-a\n      endpoint: https://auth.openai.com/oauth/token\n    headers:\n      ChatGPT-Account-Id: acct-a\n";

/// 用 YAML 改配置里的一个上游
fn with_provider(text: &str, name: &str, f: impl FnOnce(&mut serde_yaml_ng::Value)) -> String {
    let mut cfg: serde_yaml_ng::Value = serde_yaml_ng::from_str(text).unwrap();
    let p = cfg["providers"]
        .as_sequence_mut()
        .unwrap()
        .iter_mut()
        .find(|p| p["name"] == name)
        .unwrap();
    f(p);
    serde_yaml_ng::to_string(&cfg).unwrap()
}

/// 新安装加入一个有 ChatGPT 上游（A 上登录着）的同步位置：加入得了，上游在 B 上停用着，A 上
/// 照旧启用
#[tokio::test]
async fn a_fresh_device_joins_a_location_with_a_signed_in_upstream() {
    let loc = tempfile::tempdir().unwrap();
    let a = Dev::new('a');
    a.edit(|t| add_raw(t, CHATGPT)).await;
    a.enable(loc.path(), false, "A", Value::Null).await;
    let b = Dev::new('b');
    let preview = b
        .ok(
            "POST",
            "/sync/enable/preview",
            json!({ "target": folder(loc.path()) }),
        )
        .await;
    assert_eq!(preview["initial"], true);
    b.enable(loc.path(), false, "B", Value::Null).await;
    let p = b.provider("chatgpt").expect("到了 B");
    assert!(p.disabled && p.oauth.is_none());
    let v = a.ok("POST", "/sync/now", Value::Null).await;
    assert_eq!(v["conflicts"], 0, "{v}");
    let p = a.provider("chatgpt").unwrap();
    assert!(!p.disabled, "A 上没被停用");
    assert_eq!(p.oauth.unwrap().refresh, "rt-a");
}

/// 已有配置的设备以「合并」加入：导入的规矩在 B 上停用了 ChatGPT 上游，这个停用不随加入
/// 上传、不回流到 A。之后 B 上登录：照同步的样子启用
#[tokio::test]
async fn merging_in_keeps_the_upstream_on_where_it_is_signed_in() {
    let loc = tempfile::tempdir().unwrap();
    let a = Dev::new('a');
    a.edit(|t| add_raw(t, CHATGPT)).await;
    a.enable(loc.path(), false, "A", Value::Null).await;
    let b = Dev::new('b');
    b.edit(|t| {
        add_raw(
            t,
            "  - name: mine\n    base_url: https://mine.example\n    key: sk-m\n",
        )
    })
    .await;
    b.enable(
        loc.path(),
        false,
        "B",
        json!({
            "join": "merge",
            "decisions": [ { "item": { "kind": "key", "name": "default" }, "action": "skip" } ],
        }),
    )
    .await;
    assert!(b.provider("chatgpt").unwrap().disabled);
    let v = a.ok("POST", "/sync/now", Value::Null).await;
    assert_eq!(v["conflicts"], 0, "{v}");
    let p = a.provider("chatgpt").unwrap();
    assert!(!p.disabled, "B 的合并加入没有停掉 A 上的");
    assert!(a.provider("mine").is_some());

    // A 改并发：B 收到，还停用着
    a.edit(|t| with_provider(t, "chatgpt", |p| p["max_concurrent"] = 4.into()))
        .await;
    a.ok("POST", "/sync/now", Value::Null).await;
    b.ok("POST", "/sync/now", Value::Null).await;
    let p = b.provider("chatgpt").unwrap();
    assert_eq!(p.max_concurrent, Some(4));
    assert!(p.disabled);

    // B 上登录（写上自己的登录，停用还在）
    b.edit(|t| {
        with_provider(t, "chatgpt", |p| {
            p["oauth"] = serde_yaml_ng::from_str(
                "{ refresh: rt-b, endpoint: 'https://auth.openai.com/oauth/token' }",
            )
            .unwrap();
            p["headers"] = serde_yaml_ng::from_str("{ ChatGPT-Account-Id: acct-b }").unwrap();
        })
    })
    .await;
    let v = b.ok("POST", "/sync/now", Value::Null).await;
    assert_eq!(v["conflicts"], 0, "{v}");
    let p = b.provider("chatgpt").unwrap();
    assert!(!p.disabled, "登录之后照同步的样子启用");
    assert_eq!(p.oauth.unwrap().refresh, "rt-b");
    let v = a.ok("POST", "/sync/now", Value::Null).await;
    assert_eq!(v["conflicts"], 0, "{v}");
    let p = a.provider("chatgpt").unwrap();
    assert!(!p.disabled);
    assert_eq!(p.oauth.unwrap().refresh, "rt-a", "A 的登录还是 A 的");
}

/// keyring 不见了：要人处理的状态（`keyring_missing`），同步设置和口令都不动；「重新上传」
/// 放回这台记着的 keyring 之后照常
#[tokio::test]
async fn a_missing_keyring_waits_for_a_person() {
    let loc = tempfile::tempdir().unwrap();
    let a = Dev::new('a');
    a.enable(loc.path(), true, "A", Value::Null).await;
    a.ok("POST", "/sync/now", Value::Null).await;
    std::fs::remove_file(loc.path().join("keyring")).unwrap();
    let v = a.ok("POST", "/sync/now", Value::Null).await;
    assert_eq!(v["state"], "keyring_missing", "{v}");
    assert_eq!(v["error"]["code"], "sync.keyring_missing");
    let s = a.config().sync.unwrap();
    assert!(s.encrypt && s.passphrase.as_deref() == Some(PASS));
    let v = a.ok("POST", "/sync/republish", Value::Null).await;
    assert_eq!(v["state"], "idle", "{v}");
    assert_eq!(v["encrypted"], true);
    assert!(loc.path().join("keyring").exists());
}

/// 冲突带着能选的处理：网关密钥三种都能选，「两者都保留」那一份是一把新的密钥
#[tokio::test]
async fn each_conflict_says_which_choices_it_has() {
    let loc = tempfile::tempdir().unwrap();
    let (a, b) = (Dev::new('a'), Dev::new('b'));
    a.edit(|t| {
        t.replace(
            "clients:\n",
            "clients:\n  - name: k2\n    key: tw-k2-0001\n",
        )
    })
    .await;
    a.enable(loc.path(), false, "A", Value::Null).await;
    b.enable(loc.path(), false, "B", Value::Null).await;
    a.edit(|t| {
        t.replace(
            "    key: tw-k2-0001\n",
            "    key: tw-k2-0001\n    max_concurrent: 2\n",
        )
    })
    .await;
    b.edit(|t| {
        t.replace(
            "    key: tw-k2-0001\n",
            "    key: tw-k2-0001\n    max_concurrent: 3\n",
        )
    })
    .await;
    a.ok("POST", "/sync/now", Value::Null).await;
    b.ok("POST", "/sync/now", Value::Null).await;
    let list = b.ok("GET", "/sync/conflicts", Value::Null).await;
    let c = list
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["item"]["name"] == "k2")
        .unwrap_or_else(|| panic!("{list}"));
    assert_eq!(c["options"], json!(["local", "remote", "both"]));
    b.ok(
        "POST",
        "/sync/conflicts",
        json!({ "decisions": [ { "id": c["id"], "take": "both" } ] }),
    )
    .await;
    let clients = b.config().clients;
    let copy = clients.iter().find(|c| c.name.starts_with("k2 (")).unwrap();
    assert_ne!(copy.key, "tw-k2-0001");
}

/// 关闭加密之后两台设备的口令都留着（同步位置上写着「已关闭」的 keyring 靠它核对）
#[tokio::test]
async fn turning_encryption_off_keeps_the_passphrase() {
    let loc = tempfile::tempdir().unwrap();
    let (a, b) = (Dev::new('a'), Dev::new('b'));
    a.enable(loc.path(), true, "A", Value::Null).await;
    b.enable(loc.path(), true, "B", Value::Null).await;
    let v = a
        .ok(
            "POST",
            "/sync/encryption",
            json!({ "action": "disable", "passphrase": PASS }),
        )
        .await;
    assert_eq!(v["encrypted"], false, "{v}");
    let s = a.config().sync.unwrap();
    assert!(!s.encrypt && s.passphrase.as_deref() == Some(PASS));
    let v = b.ok("POST", "/sync/now", Value::Null).await;
    assert_eq!(v["encrypted"], false, "{v}");
    let s = b.config().sync.unwrap();
    assert!(!s.encrypt, "B 跟着改成不加密");
    assert_eq!(s.passphrase.as_deref(), Some(PASS), "口令留着");
    // 再开回来：B 用记着的口令跟着改回加密
    a.ok(
        "POST",
        "/sync/encryption",
        json!({ "action": "enable", "passphrase": PASS }),
    )
    .await;
    let v = b.ok("POST", "/sync/now", Value::Null).await;
    assert_eq!(v["encrypted"], true, "{v}");
    assert!(b.config().sync.unwrap().encrypt);
}

/// 收下 B 上的 `GatewayKeysChanged` 里的名字
fn keys_changed(rx: &mut tokio::sync::broadcast::Receiver<tw_api::Event>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let tw_api::Event::GatewayKeysChanged { names, .. } = ev {
            out.extend(names);
        }
    }
    out
}

/// 已有配置的设备以「合并」加入：两边的默认网关密钥同名、值不同 —— 不当作冲突（不用选，选了
/// 「跳过」也不看），采用同步数据的值；A 上的那一把不变；B 发 `GatewayKeysChanged`
#[tokio::test]
async fn merging_in_adopts_the_default_key_of_the_sync_data() {
    let loc = tempfile::tempdir().unwrap();
    let a = Dev::new('a');
    a.enable(loc.path(), false, "A", Value::Null).await;
    let b = Dev::new('b');
    b.edit(|t| {
        add_raw(
            t,
            "  - name: mine\n    base_url: https://mine.example\n    key: sk-m\n",
        )
    })
    .await;
    b.ok("PUT", "/pinned-keys", json!({ "names": ["default"] }))
        .await;
    let preview = b
        .ok(
            "POST",
            "/sync/enable/preview",
            json!({ "target": folder(loc.path()) }),
        )
        .await;
    assert_eq!(preview["initial"], false);
    assert_eq!(
        preview["auto"],
        json!([{ "item": { "kind": "key", "name": "default" }, "action": "overwrite", "reason": "default_key" }]),
        "{preview}"
    );
    assert!(
        preview["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["item"]["name"] == "default"),
        "那一行照常列出：{preview}"
    );
    let mut events = b.state.bus().subscribe();
    // 命令行以前的缺省：给默认密钥选「跳过」—— 不看
    b.enable(
        loc.path(),
        false,
        "B",
        json!({
            "join": "merge",
            "decisions": [ { "item": { "kind": "key", "name": "default" }, "action": "skip" } ],
        }),
    )
    .await;
    let key = |d: &Dev| {
        d.config()
            .clients
            .into_iter()
            .find(|c| c.name == "default")
            .unwrap()
            .key
    };
    assert_eq!(key(&b), "tw-default-a", "B 采用同步数据的值");
    assert_eq!(keys_changed(&mut events), ["default"]);
    let v = a.ok("POST", "/sync/now", Value::Null).await;
    assert_eq!(v["conflicts"], 0, "{v}");
    assert_eq!(key(&a), "tw-default-a", "A 的默认密钥没被改掉");
    assert!(a.provider("mine").is_some());
}

/// 新安装（初始配置）上已经接管了客户端：用同步数据加入时，默认密钥也采用同步数据的值，不把
/// 本机那一把传上去改掉别的设备的
#[tokio::test]
async fn a_fresh_device_with_clients_set_up_adopts_the_default_key() {
    let loc = tempfile::tempdir().unwrap();
    let a = Dev::new('a');
    a.enable(loc.path(), false, "A", Value::Null).await;
    let b = Dev::new('b');
    b.ok("PUT", "/pinned-keys", json!({ "names": ["default"] }))
        .await;
    let preview = b
        .ok(
            "POST",
            "/sync/enable/preview",
            json!({ "target": folder(loc.path()) }),
        )
        .await;
    assert_eq!(preview["initial"], true);
    assert_eq!(preview["auto"][0]["reason"], "default_key", "{preview}");
    let mut events = b.state.bus().subscribe();
    b.enable(loc.path(), false, "B", Value::Null).await;
    assert_eq!(b.config().clients[0].key, "tw-default-a");
    assert_eq!(keys_changed(&mut events), ["default"]);
    a.ok("POST", "/sync/now", Value::Null).await;
    assert_eq!(a.config().clients[0].key, "tw-default-a");
}

/// 同步位置上来了一份超出上限的通用偏好（别处写进来的）：这一轮不失败，偏好不应用、其余照常，
/// 视图里如实说（`notices`），不是错误、也不计入连不上
#[tokio::test]
async fn oversized_preferences_in_the_sync_data_do_not_fail_the_round() {
    let loc = tempfile::tempdir().unwrap();
    let (a, b) = (Dev::new('a'), Dev::new('b'));
    a.enable(loc.path(), false, "A", Value::Null).await;
    b.enable(loc.path(), false, "B", Value::Null).await;
    // 依据最新的那个版本，写一个带着很大偏好的版本进去
    let mut names: Vec<String> = std::fs::read_dir(loc.path().join("versions"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    let head = names
        .last()
        .unwrap()
        .trim_end_matches(".twconfig")
        .to_string();
    let seq: u64 = head[..10].parse().unwrap();
    let text = add_raw(
        &tw_pack::read(
            &std::fs::read(loc.path().join("versions").join(format!("{head}.twconfig"))).unwrap(),
            tw_pack::Unseal::None,
        )
        .unwrap()
        .config_yaml,
        "  - name: elsewhere\n    base_url: https://e.example\n    key: sk-e\n",
    );
    let mut pack = tw_pack::Pack {
        manifest: tw_pack::Manifest::new(tw_pack::PackKind::Full, tw_pack::PackOrigin::Sync),
        config_yaml: text,
        plugins: Default::default(),
        client_prefs: Some(json!({ "notes": "x".repeat(70 * 1024) })),
    };
    let meta = tw_sync::record::Meta {
        seq: seq + 1,
        device: "eeeeeeee-0000-4000-8000-00000000000e".into(),
        device_name: "Elsewhere".into(),
        parents: vec![head],
        sha256: Some(tw_sync::engine::content_sha256(&pack)),
        ..Default::default()
    };
    pack.manifest.sync = Some(serde_json::to_value(&meta).unwrap());
    std::fs::write(
        loc.path()
            .join("versions")
            .join(format!("{:010}-eeeeeeee.twconfig", seq + 1)),
        tw_pack::write(&pack, tw_pack::Seal::None).unwrap(),
    )
    .unwrap();
    for _ in 0..2 {
        let v = b.ok("POST", "/sync/now", Value::Null).await;
        assert_eq!(v["state"], "idle", "{v}");
        assert!(v["error"].is_null(), "{v}");
        assert!(v["failing_since_ms"].is_null(), "{v}");
        assert_eq!(v["notices"][0]["code"], "sync.prefs_too_large", "{v}");
    }
    assert!(b.provider("elsewhere").is_some(), "其余照常应用");
}
