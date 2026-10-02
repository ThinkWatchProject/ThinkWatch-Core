//! 脚本插件的管理面：装、改、换源码、文件被改了、批准、排顺序、删、日志、记录。
//!
//! 断言落在**磁盘上**：插件文件和底稿写没写、写的是不是那一份字节、配置里那一条长
//! 什么样 —— 网关照着这些重读插件，哈希对不上就不跑（不变式 I9）。引擎是假的
//! （`tw_gateway::plugin::fake`）：它照约定的写法读出 manifest，不跑 JavaScript。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};
use tw_gateway::plugin::fake::{FakeEngine, source};

const BASE: &str = "version: 1
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
# 默认那把
clients:
  - name: default
    key: tw-aaaa
";

struct Bed {
    _tmp: tempfile::TempDir,
    /// 配置文件所在的目录
    dir: PathBuf,
    gw: tw_gateway::AppState,
    store: Arc<tokio::sync::Mutex<tw_store::Recorder>>,
    app: axum::Router,
}

impl Bed {
    fn config(&self) -> String {
        std::fs::read_to_string(self.dir.join("config.yaml")).unwrap()
    }
    fn parsed(&self) -> tw_config::Config {
        tw_config::try_parse(&self.config()).unwrap()
    }
    fn file(&self, id: &str) -> PathBuf {
        tw_config::plugins::file_path(&self.dir, id)
    }
    fn approved(&self, id: &str) -> PathBuf {
        tw_config::plugins::approved_path(&self.dir, id)
    }
    async fn version(&self) -> String {
        let (_, v) = call(&self.app, "GET", "/overview", None).await;
        v["config_version"].as_str().unwrap().to_string()
    }
    async fn plugins(&self) -> Vec<Value> {
        let (st, v) = call(&self.app, "GET", "/plugins", None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        v.as_array().unwrap().clone()
    }
    async fn plugin(&self, id: &str) -> Value {
        self.plugins()
            .await
            .into_iter()
            .find(|p| p["id"] == id)
            .unwrap_or_else(|| panic!("no plugin {id}"))
    }
    /// 装一个，返回 id
    async fn install(&self, src: &str, extra: Value) -> String {
        let mut body = json!({
            "source": src,
            "enabled": true,
            "on_error": "reject",
            "scope": { "clients": [], "models": [], "upstreams": [] },
            "settings": {},
            "base_version": self.version().await,
        });
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (st, v) = call(&self.app, "POST", "/plugins", Some(body)).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let cfg = self.parsed();
        cfg.plugins.last().unwrap().id.clone()
    }
}

/// 一张床：配置文件在 `dir`（相对临时目录的一段路径）里，引擎是假的
fn bed_in(sub: &str) -> Bed {
    bed_with(sub, true)
}

fn bed_with(sub: &str, fake: bool) -> Bed {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(sub);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("config.yaml");
    std::fs::write(&p, BASE).unwrap();
    let db = tw_store::Db::open(&dir.join("data.db")).unwrap();
    let rec = tw_store::Recorder::new(
        db,
        tw_store::Blobs::new(dir.join("blobs")),
        tw_pricing::shared(tw_pricing::PriceBook::builtin().unwrap()),
    );
    let store = Arc::new(tokio::sync::Mutex::new(rec));
    let gw = tw_gateway::AppState::new(tw_config::try_parse(BASE).unwrap()).unwrap();
    if fake {
        gw.set_plugin_engine(Arc::new(FakeEngine));
    }
    let bus = gw.bus.clone();
    let state = ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw.clone(),
        store: Some(store.clone()),
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    };
    Bed {
        _tmp: tmp,
        dir,
        gw,
        store,
        app: tw_control::router(state),
    }
}

fn bed() -> Bed {
    bed_in("home")
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let r = app
        .clone()
        .oneshot(
            req.body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
                .unwrap(),
        )
        .await
        .unwrap();
    let st = r.status();
    let b = axum::body::to_bytes(r.into_body(), 1 << 22).await.unwrap();
    (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

fn add_date() -> String {
    source(
        json!({"name": "附加日期", "api": 1, "description": "在系统提示里写上今天的日期",
               "permissions": ["system"], "match": {"clients": ["claude-code"]},
               "settings": {"note": {"type": "string", "label": "附加内容", "default": "今天"},
                            "days": {"type": "number", "label": "天数", "default": 1}}}),
        &["onRequest"],
    )
}

fn shout() -> String {
    source(
        json!({"name": "Shout", "api": 1, "permissions": ["reply.text"]}),
        &["onReplyText"],
    )
}

fn sha(s: &str) -> String {
    tw_gateway::plugin::load::sha256_hex(s.as_bytes())
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

#[tokio::test]
async fn inspecting_a_source_says_what_it_is_and_leaves_nothing_behind() {
    let b = bed();
    let src = add_date();
    let (st, v) = call(
        &b.app,
        "POST",
        "/plugins/inspect",
        Some(json!({"source": src})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["sha256"], sha(&src));
    assert!(v["error"].is_null(), "{v}");
    let m = &v["manifest"];
    assert_eq!(m["name"], "附加日期");
    assert_eq!(m["permissions"], json!(["system"]));
    assert_eq!(m["scope"]["clients"], json!(["claude-code"]));
    assert_eq!(
        m["hooks"],
        json!({"request": true, "reply_text": false, "tool_call": false})
    );
    assert_eq!(m["settings_schema"][0]["key"], "days");
    assert_eq!(m["settings_schema"][0]["kind"], "number");
    assert_eq!(m["settings_schema"][0]["default"], json!(1.0));

    let bad = format!("{src}// @@syntax@@\n");
    let (st, v) = call(
        &b.app,
        "POST",
        "/plugins/inspect",
        Some(json!({"source": bad})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["manifest"].is_null());
    assert_eq!(v["error"]["message"]["code"], "gw.plugin.syntax_at");
    assert_eq!(v["error"]["line"], 3);
    assert_eq!(v["error"]["column"], 1);

    // 什么都没留下
    assert_eq!(b.config(), BASE);
    assert!(files_in(&b.dir.join("plugins")).is_empty());
}

#[tokio::test]
async fn installing_writes_the_file_its_approved_copy_and_one_entry() {
    let b = bed();
    let src = add_date();
    let id = b
        .install(
            &src,
            json!({"scope": {"clients": ["claude-code"], "models": [], "upstreams": []},
                              "settings": {"note": "明天"}}),
        )
        .await;
    // 名字里没有拉丁字母
    assert_eq!(id, "plugin");
    assert_eq!(std::fs::read_to_string(b.file(&id)).unwrap(), src);
    assert_eq!(std::fs::read_to_string(b.approved(&id)).unwrap(), src);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for p in [b.file(&id), b.approved(&id)] {
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", p.display());
        }
        let mode = std::fs::metadata(b.dir.join("plugins"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }

    let cfg = b.parsed();
    let p = &cfg.plugins[0];
    assert_eq!(p.file, "plugins/plugin.js");
    assert_eq!(p.sha256, sha(&src));
    assert!(p.enabled);
    assert_eq!(p.on_error, tw_config::PluginOnError::Reject);
    assert_eq!(p.scope.clients, ["claude-code"]);
    // 设置每一项都写明：给了的照写，没给的写默认值；整数写成整数
    assert_eq!(p.settings["note"], serde_yaml_ng::Value::from("明天"));
    assert_eq!(p.settings["days"], serde_yaml_ng::Value::from(1));
    assert!(b.config().contains("    days: 1\n"), "{}", b.config());
    // 注释还在
    assert!(b.config().contains("# 默认那把"));

    let v = b.plugin(&id).await;
    assert_eq!(v["status"], json!({"kind": "ok"}));
    assert_eq!(v["name"], "附加日期");
    assert_eq!(v["description"], "在系统提示里写上今天的日期");
    assert_eq!(v["sha256"], sha(&src));
    assert_eq!(v["settings"], json!({"note": "明天", "days": 1.0}));
    assert_eq!(v["stats"]["calls"], 0);
    // 网关手里的那一份能跑
    let rt = b.gw.runtime();
    assert!(rt.plugins.get(&id).unwrap().ready().is_some());
}

#[tokio::test]
async fn an_id_is_checked_and_a_second_plugin_of_the_same_name_gets_its_own() {
    let b = bed();
    let first = b.install(&shout(), json!({})).await;
    let second = b.install(&shout(), json!({})).await;
    assert_eq!((first.as_str(), second.as_str()), ("shout", "shout-2"));

    for (id, code) in [
        ("Bad_Id", "control.plugin.bad_id"),
        ("order", "control.plugin.reserved_id"),
        ("shout", "control.plugin.id_taken"),
    ] {
        let (st, v) = call(
            &b.app,
            "POST",
            "/plugins",
            Some(
                json!({"source": shout(), "id": id, "enabled": true, "on_error": "skip",
                        "scope": {"clients": [], "models": [], "upstreams": []}, "settings": {}}),
            ),
        )
        .await;
        assert!(st.is_client_error(), "{id}: {st} {v}");
        assert_eq!(v["code"], code, "{id}");
    }
}

/// 同名的两个同时装：后一个看得见前一个，各得各的 id，文件互不覆盖
#[tokio::test]
async fn two_plugins_of_one_name_installed_at_once_get_their_own_ids() {
    let b = bed();
    let body = json!({"source": shout(), "enabled": true, "on_error": "reject",
                      "scope": {"clients": [], "models": [], "upstreams": []}, "settings": {}});
    let (one, two) = tokio::join!(
        call(&b.app, "POST", "/plugins", Some(body.clone())),
        call(&b.app, "POST", "/plugins", Some(body)),
    );
    assert_eq!(
        (one.0, two.0),
        (StatusCode::OK, StatusCode::OK),
        "{} {}",
        one.1,
        two.1
    );
    let mut ids: Vec<String> = b.parsed().plugins.into_iter().map(|p| p.id).collect();
    ids.sort();
    assert_eq!(ids, ["shout", "shout-2"]);
    assert!(b.file("shout").exists() && b.file("shout-2").exists());
}

/// 装之前编一遍：编不成、设置不对的都不装，**一个文件都不写**
#[tokio::test]
async fn a_plugin_that_does_not_load_or_has_wrong_settings_is_not_installed() {
    let b = bed();
    let body = |src: String, settings: Value| {
        json!({"source": src, "enabled": true, "on_error": "reject",
               "scope": {"clients": [], "models": [], "upstreams": []}, "settings": settings})
    };
    let (st, v) = call(
        &b.app,
        "POST",
        "/plugins",
        Some(body(format!("{}// @@syntax@@\n", add_date()), json!({}))),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["code"], "gw.plugin.syntax_at");
    let (st, v) = call(
        &b.app,
        "POST",
        "/plugins",
        Some(body(add_date(), json!({"days": "x"}))),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["code"], "gw.plugin.setting_type");
    let (st, v) = call(
        &b.app,
        "POST",
        "/plugins",
        Some(body(add_date(), json!({"colour": "red"}))),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["code"], "gw.plugin.setting_unknown");
    assert_eq!(b.config(), BASE);
    assert!(files_in(&b.dir.join("plugins")).is_empty());
}

/// 配置没写成（版本对不上），刚写的文件还原：不留一个和配置对不上的插件文件
#[tokio::test]
async fn a_stale_write_puts_the_files_back() {
    let b = bed();
    let (st, v) = call(
        &b.app,
        "POST",
        "/plugins",
        Some(
            json!({"source": shout(), "enabled": true, "on_error": "reject",
                    "scope": {"clients": [], "models": [], "upstreams": []}, "settings": {},
                    "base_version": "not-this-one"}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(b.config(), BASE);
    assert!(!b.file("shout").exists());
    assert!(!b.approved("shout").exists());

    // 换源码同理：旧的那一份原样回来
    let id = b.install(&shout(), json!({})).await;
    let newer = shout().replace("Shout", "Louder");
    let (st, _) = call(
        &b.app,
        "PUT",
        &format!("/plugins/{id}/source"),
        Some(json!({"source": newer, "base_version": "not-this-one"})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(std::fs::read_to_string(b.file(&id)).unwrap(), shout());
    assert_eq!(std::fs::read_to_string(b.approved(&id)).unwrap(), shout());
}

#[tokio::test]
async fn replacing_the_source_rewrites_the_file_the_copy_and_the_hash_and_keeps_fitting_settings() {
    let b = bed();
    let id = b
        .install(
            &add_date(),
            json!({"settings": {"note": "明天", "days": 3}}),
        )
        .await;
    // 新的一版：`days` 改成了字符串，`note` 没变，多了一个 `loud`
    let newer = source(
        json!({"name": "附加日期", "api": 1, "permissions": ["system"],
               "settings": {"note": {"type": "string", "label": "附加内容", "default": ""},
                            "days": {"type": "string", "label": "天数", "default": "1"},
                            "loud": {"type": "boolean", "label": "大声", "default": true}}}),
        &["onRequest"],
    );
    let (st, v) = call(
        &b.app,
        "PUT",
        &format!("/plugins/{id}/source"),
        Some(json!({"source": newer, "base_version": b.version().await})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(std::fs::read_to_string(b.file(&id)).unwrap(), newer);
    assert_eq!(std::fs::read_to_string(b.approved(&id)).unwrap(), newer);
    let cfg = b.parsed();
    let p = &cfg.plugins[0];
    assert_eq!(p.sha256, sha(&newer));
    assert_eq!(p.settings["note"], serde_yaml_ng::Value::from("明天"));
    assert_eq!(p.settings["days"], serde_yaml_ng::Value::from("1"));
    assert_eq!(p.settings["loud"], serde_yaml_ng::Value::from(true));
    assert_eq!(b.plugin(&id).await["status"], json!({"kind": "ok"}));

    let (st, _) = call(
        &b.app,
        "PUT",
        "/plugins/nobody/source",
        Some(json!({"source": newer})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// I9：磁盘上的文件被改了，插件停用、说一声；看过改动、批准了才回来
#[tokio::test]
async fn a_file_edited_on_disk_stops_the_plugin_until_the_change_is_approved() {
    let b = bed();
    let id = b
        .install(&add_date(), json!({"settings": {"days": 2}}))
        .await;
    let mut events = b.gw.bus.subscribe();

    let edited = format!("{}// 加了一行\n", add_date());
    std::fs::write(b.file(&id), &edited).unwrap();
    b.gw.reload_plugins();

    let v = b.plugin(&id).await;
    assert_eq!(v["status"], json!({"kind": "changed"}));
    // 批准过的那一份照样显示
    assert_eq!(v["name"], "附加日期");
    assert!(b.gw.runtime().plugins.get(&id).unwrap().ready().is_none());
    match events.try_recv().expect("no plugin_failed") {
        tw_api::Event::PluginFailed {
            plugin_id,
            request_id,
            message,
            ..
        } => {
            assert_eq!(plugin_id, id);
            assert_eq!(request_id, None);
            assert_eq!(message.code, "gw.plugin.file_changed");
        }
        other => panic!("{other:?}"),
    }

    let (st, diff) = call(&b.app, "GET", &format!("/plugins/{id}/source"), None).await;
    assert_eq!(st, StatusCode::OK, "{diff}");
    assert_eq!(diff["approved"], add_date());
    assert_eq!(diff["approved_sha256"], sha(&add_date()));
    assert_eq!(diff["current"], edited);
    assert_eq!(diff["current_sha256"], sha(&edited));

    // 看过之后又被改了一次：不批
    let again = format!("{edited}// 又一行\n");
    std::fs::write(b.file(&id), &again).unwrap();
    let (st, v) = call(
        &b.app,
        "POST",
        &format!("/plugins/{id}/approve"),
        Some(json!({"sha256": sha(&edited), "base_version": b.version().await})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], "control.plugin.file_moved_on");

    let (st, v) = call(
        &b.app,
        "POST",
        &format!("/plugins/{id}/approve"),
        Some(json!({"sha256": sha(&again), "base_version": b.version().await})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(b.parsed().plugins[0].sha256, sha(&again));
    assert_eq!(std::fs::read_to_string(b.approved(&id)).unwrap(), again);
    // 文件本身没被动过；设置照旧
    assert_eq!(std::fs::read_to_string(b.file(&id)).unwrap(), again);
    assert_eq!(
        b.parsed().plugins[0].settings["days"],
        serde_yaml_ng::Value::from(2)
    );
    assert_eq!(b.plugin(&id).await["status"], json!({"kind": "ok"}));
}

/// 文件被删掉也是「变了」；批准不了一个不存在的文件
#[tokio::test]
async fn a_deleted_file_is_changed_and_cannot_be_approved() {
    let b = bed();
    let id = b.install(&shout(), json!({})).await;
    std::fs::remove_file(b.file(&id)).unwrap();
    b.gw.reload_plugins();
    assert_eq!(b.plugin(&id).await["status"], json!({"kind": "changed"}));
    let (_, diff) = call(&b.app, "GET", &format!("/plugins/{id}/source"), None).await;
    assert!(
        diff["current"].is_null() && diff["current_sha256"].is_null(),
        "{diff}"
    );
    let (st, v) = call(
        &b.app,
        "POST",
        &format!("/plugins/{id}/approve"),
        Some(json!({"sha256": sha(&shout())})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], "control.plugin.file_missing");
}

/// 底稿也被人动过：说不出批准的是什么，就不拿它冒充
#[tokio::test]
async fn a_tampered_approved_copy_is_not_shown_as_approved() {
    let b = bed();
    let id = b.install(&shout(), json!({})).await;
    std::fs::write(b.approved(&id), "something else").unwrap();
    let (_, diff) = call(&b.app, "GET", &format!("/plugins/{id}/source"), None).await;
    assert_eq!(diff["approved"], "");
    assert_eq!(diff["current"], shout());
}

/// 目录监听：插件文件一动，几秒之内就停用，不等下一次改配置
#[tokio::test]
async fn the_watcher_notices_an_edited_plugin_within_seconds() {
    let b = bed();
    let id = b.install(&shout(), json!({})).await;
    let _w = tw_control::plugins::spawn_watcher(b.gw.clone(), &b.dir.join("config.yaml")).unwrap();
    std::fs::write(b.file(&id), format!("{}// x\n", shout())).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if b.gw.runtime().plugins.get(&id).unwrap().broken().is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the edited plugin kept running"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(b.plugin(&id).await["status"], json!({"kind": "changed"}));
}

#[tokio::test]
async fn updating_changes_the_switches_scope_and_settings_and_nothing_else() {
    let b = bed();
    let id = b.install(&add_date(), json!({})).await;
    let (st, v) = call(
        &b.app,
        "PUT",
        &format!("/plugins/{id}"),
        Some(json!({"enabled": false, "on_error": "skip",
                    "scope": {"clients": [], "models": ["claude-*"], "upstreams": ["anthropic"]},
                    "settings": {"note": "后天", "days": 2.5},
                    "base_version": b.version().await})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let cfg = b.parsed();
    let p = &cfg.plugins[0];
    assert!(!p.enabled);
    assert_eq!(p.on_error, tw_config::PluginOnError::Skip);
    assert!(p.scope.clients.is_empty());
    assert_eq!(p.scope.models, ["claude-*"]);
    assert_eq!(p.settings["days"], serde_yaml_ng::Value::from(2.5));
    assert_eq!(p.sha256, sha(&add_date()));
    let v = b.plugin(&id).await;
    assert_eq!(v["status"], json!({"kind": "disabled"}));
    assert_eq!(v["on_error"], "skip");
    assert_eq!(v["scope"]["upstreams"], json!(["anthropic"]));

    for (settings, code) in [
        (json!({"days": true}), "gw.plugin.setting_type"),
        (json!({"nope": 1}), "gw.plugin.setting_unknown"),
    ] {
        let (st, v) = call(
            &b.app,
            "PUT",
            &format!("/plugins/{id}"),
            Some(json!({"enabled": true, "on_error": "reject",
                        "scope": {"clients": [], "models": [], "upstreams": []},
                        "settings": settings})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["code"], code);
    }
    let (st, v) = call(
        &b.app,
        "PUT",
        &format!("/plugins/{id}"),
        Some(json!({"enabled": true, "on_error": "reject",
                    "scope": {"clients": [" "], "models": [], "upstreams": []},
                    "settings": {}})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["code"], "control.plugin.blank_pattern");
    let (st, _) = call(
        &b.app,
        "PUT",
        "/plugins/nobody",
        Some(json!({"enabled": true, "on_error": "reject",
                    "scope": {"clients": [], "models": [], "upstreams": []}, "settings": {}})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reordering_changes_the_run_order_and_needs_every_plugin_once() {
    let b = bed();
    let a = b.install(&shout(), json!({"id": "a"})).await;
    let c = b.install(&shout(), json!({"id": "c"})).await;
    let d = b.install(&add_date(), json!({"id": "d"})).await;
    let (st, v) = call(
        &b.app,
        "PUT",
        "/plugins/order",
        Some(json!({"ids": [d, a, c], "base_version": b.version().await})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let ids: Vec<String> = b.parsed().plugins.into_iter().map(|p| p.id).collect();
    assert_eq!(ids, ["d", "a", "c"]);
    let listed: Vec<String> = b
        .plugins()
        .await
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(listed, ["d", "a", "c"]);
    // 每一项搬过去时整项都在
    assert_eq!(b.parsed().plugins[0].settings.len(), 2);

    for ids in [
        json!(["d", "a"]),
        json!(["d", "a", "a"]),
        json!(["d", "a", "x"]),
    ] {
        let (st, v) = call(&b.app, "PUT", "/plugins/order", Some(json!({"ids": ids}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["code"], "control.plugin.order");
    }
}

#[tokio::test]
async fn deleting_removes_the_entry_and_both_files() {
    let b = bed();
    let id = b.install(&shout(), json!({})).await;
    let (st, v) = call(
        &b.app,
        "DELETE",
        &format!("/plugins/{id}?base_version={}", b.version().await),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(b.parsed().plugins.is_empty());
    assert!(!b.config().contains("plugins:"), "{}", b.config());
    assert!(!b.file(&id).exists() && !b.approved(&id).exists());
    assert!(b.plugins().await.is_empty());
    let (st, _) = call(&b.app, "DELETE", &format!("/plugins/{id}"), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// 计数和日志：数据面每跑一次报一次，界面从这里取
#[tokio::test]
async fn runs_show_up_in_the_stats_and_the_logs() {
    let b = bed();
    let id = b.install(&add_date(), json!({})).await;
    let active = b.gw.runtime().plugins.get(&id).unwrap().clone();
    b.gw.plugin_ran(
        7,
        &active,
        tw_gateway::plugin::PluginRun {
            plugin_id: id.clone(),
            plugin_name: active.name.clone(),
            hook: tw_api::PluginHook::Request,
            outcome: tw_api::PluginOutcome::Changed,
            error: None,
            cpu_us: 300,
            detail: None,
        },
        vec![tw_gateway::plugin::LogLine {
            level: tw_api::PluginLogLevel::Warn,
            text: "<b>not markup</b>".into(),
        }],
    );
    let v = b.plugin(&id).await;
    assert_eq!(v["stats"]["calls"], 1);
    assert_eq!(v["stats"]["changed"], 1);
    assert_eq!(v["stats"]["avg_cpu_us"], 300);
    let (st, logs) = call(&b.app, "GET", &format!("/plugins/{id}/logs"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(logs[0]["request_id"], 7);
    assert_eq!(logs[0]["hook"], "request");
    assert_eq!(logs[0]["level"], "warn");
    assert_eq!(logs[0]["text"], "<b>not markup</b>");
    let (st, _) = call(&b.app, "GET", "/plugins/nobody/logs", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

fn row(id: i64, at_ms: i64) -> tw_store::RequestRow {
    tw_store::RequestRow {
        session_log_bytes: None,
        key_masked: None,
        peer: None,
        id,
        at_ms,
        client: "default".into(),
        client_hint: Some("claude-code".into()),
        session: None,
        provider: "anthropic".into(),
        model: "claude-sonnet-4-5".into(),
        sent_model: "claude-sonnet-4-5".into(),
        answered_model: None,
        path: "/v1/messages".into(),
        status: Some(200),
        ttfb_ms: Some(100),
        ttft_ms: None,
        duration_ms: Some(200),
        tokens_per_sec: None,
        bytes: Some(10),
        input_tokens: Some(50),
        output_tokens: Some(20),
        cache_read_tokens: None,
        cache_write_tokens: None,
        input_estimate: None,
        cost_micros: Some(1_000),
        cost_estimated: false,
        error: None,
        local: false,
        cancelled: false,
        routing: None,
        billing: tw_api::Billing::PerToken,
        cache_saved_micros: None,
        price_source: None,
        translated: None,
    }
}

fn run_row(request_id: i64, at_ms: i64, outcome: tw_api::PluginOutcome) -> tw_store::PluginRunRow {
    tw_store::PluginRunRow {
        request_id,
        at_ms,
        plugin_id: "add-date".into(),
        plugin_name: "附加日期".into(),
        hook: tw_api::PluginHook::Request,
        outcome,
        error: None,
        cpu_us: 120,
        detail: None,
    }
}

/// I10：每一次运行记在那条请求上 —— 详情里列得出来，改过的请求在列表上带徽标，
/// 改过之后的请求体和别的正文一样打着码给出来（落盘前那一道在网关的
/// `BodyRecord::for_disk`，这里直接写进存储，看的是读出来那一道）
#[tokio::test]
async fn a_request_shows_its_plugin_runs_and_the_body_after_them() {
    let b = bed();
    {
        let g = b.store.lock().await;
        g.db().insert(&row(1, 1_000)).unwrap();
        g.db().insert(&row(2, 2_000)).unwrap();
        // 故障转移过一次：第 0 跳、第 1 跳各跑一次请求钩子，回答钩子跑在回答的第 1 跳上
        let mut first = run_row(1, 1_000, tw_api::PluginOutcome::Changed);
        first.detail = Some(r#"{"attempt":0,"changed":["system"]}"#.into());
        g.record_plugin_run(&first);
        let mut second = run_row(1, 1_100, tw_api::PluginOutcome::Changed);
        second.detail = Some(r#"{"attempt":1,"changed":["system"]}"#.into());
        g.record_plugin_run(&second);
        let mut reply = run_row(1, 1_500, tw_api::PluginOutcome::Error);
        reply.hook = tw_api::PluginHook::Reply;
        reply.error = Some(tw_types::msg!("gw.plugin.failed" => "The plugin failed."));
        reply.detail = Some(r#"{"attempt":1,"text_calls":1}"#.into());
        g.record_plugin_run(&reply);
        g.record_plugin_run(&run_row(2, 2_000, tw_api::PluginOutcome::Unchanged));
        g.record_body(
            1_000,
            1,
            tw_store::Which::Request,
            b"{\"system\":\"hi\"}",
            15,
        );
        let after = br#"{"system":"hi, today is Friday","key":"sk-ant-api03-USERSOWNKEYAAAAAAAAAAAAAAAAAA"}"#;
        g.record_body(1_000, 1, tw_store::Which::AfterPlugins, after, after.len());
    }
    let (st, d) = call(&b.app, "GET", "/request/1", None).await;
    assert_eq!(st, StatusCode::OK, "{d}");
    let runs = d["plugins"].as_array().unwrap();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0]["hook"], "request");
    assert_eq!(runs[0]["outcome"], "changed");
    assert_eq!(runs[0]["cpu_us"], 120);
    let attempts: Vec<u64> = runs
        .iter()
        .map(|r| r["attempt"].as_u64().unwrap())
        .collect();
    assert_eq!(attempts, [0, 1, 1]);
    assert_eq!(runs[2]["hook"], "reply");
    assert_eq!(runs[2]["error"]["code"], "gw.plugin.failed");
    assert_eq!(d["row"]["plugin_changed"], true);
    let after = d["request_after_plugins"]["text"].as_str().unwrap();
    assert!(after.contains("today is Friday"), "{after}");
    assert!(
        !after.contains("USERSOWNKEY"),
        "a secret was shown: {after}"
    );
    assert_eq!(d["request_after_plugins"]["truncated"], false);

    let (_, d2) = call(&b.app, "GET", "/request/2", None).await;
    assert_eq!(d2["row"]["plugin_changed"], false);
    assert!(d2["request_after_plugins"].is_null());

    let (_, list) = call(&b.app, "GET", "/history?limit=10", None).await;
    let flags: Vec<(i64, bool)> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_i64().unwrap(),
                r["plugin_changed"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(flags, [(2, false), (1, true)]);
    // 搜索翻出来的那一页也带着
    let (_, page) = call(
        &b.app,
        "POST",
        "/history/search",
        Some(json!({"limit": 10})),
    )
    .await;
    let flags: Vec<bool> = page["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["plugin_changed"].as_bool().unwrap())
        .collect();
    assert_eq!(flags, [false, true]);
}

#[tokio::test]
async fn a_trial_needs_a_known_plugin_and_a_recorded_request() {
    let b = bed();
    let id = b.install(&add_date(), json!({})).await;
    let (st, v) = call(
        &b.app,
        "POST",
        &format!("/plugins/{id}/trial"),
        Some(json!({"request_id": 9})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(v["code"], "control.request_not_found");
    b.store.lock().await.db().insert(&row(9, 1_000)).unwrap();
    let (st, v) = call(
        &b.app,
        "POST",
        "/plugins/nobody/trial",
        Some(json!({"request_id": 9})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
    let (st, v) = call(
        &b.app,
        "POST",
        &format!("/plugins/{id}/trial"),
        Some(json!({"request_id": 9})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // 这一条什么正文都没存下来
    assert_eq!(v["error"]["code"], "gw.plugin.nothing_to_try", "{v}");
    assert!(v["logs"].as_array().unwrap().is_empty());

    // 改过还没批准的代码不试
    std::fs::write(b.file(&id), "changed").unwrap();
    b.gw.reload_plugins();
    let (_, v) = call(
        &b.app,
        "POST",
        &format!("/plugins/{id}/trial"),
        Some(json!({"request_id": 9})),
    )
    .await;
    assert_eq!(v["error"]["code"], "control.plugin.trial_changed");
}

/// 跑得起钩子的引擎：manifest 照假引擎读，请求钩子在系统提示后面补一句，回答钩子把字
/// 换成大写
struct Running;

struct RunningHost(Arc<dyn tw_gateway::plugin::PluginHost>);

impl tw_gateway::plugin::PluginHost for RunningHost {
    fn manifest(&self) -> &tw_gateway::plugin::Manifest {
        self.0.manifest()
    }
    fn sha256(&self) -> [u8; 32] {
        self.0.sha256()
    }
    fn on_request(
        &self,
        mut view: Value,
        _ctx: Value,
    ) -> tw_gateway::plugin::Invocation<tw_gateway::plugin::RequestOutcome> {
        let system = view["system"].as_str().unwrap_or_default().to_string();
        view["system"] = json!(format!("{system} Today is Friday."));
        let mut inv =
            tw_gateway::plugin::Invocation::ok(tw_gateway::plugin::RequestOutcome::Changed(view));
        inv.logs.push(tw_gateway::plugin::LogLine {
            level: tw_api::PluginLogLevel::Info,
            text: "added the date".into(),
        });
        inv
    }
    fn reply(
        &self,
        _ctx: Value,
    ) -> Result<Box<dyn tw_gateway::plugin::ReplyHost>, tw_gateway::plugin::RunError> {
        use tw_gateway::plugin::{Invocation, ToolCallOutcome};
        Ok(Box::new(tw_gateway::plugin::host::double::Closures {
            text: Box::new(|t| Invocation::ok(Some(t.to_uppercase()))),
            end: Box::new(|| Invocation::ok(None)),
            tool: Box::new(|_| Invocation::ok(ToolCallOutcome::Unchanged)),
        }))
    }
}

impl tw_gateway::plugin::Engine for Running {
    fn load(
        &self,
        source: &[u8],
    ) -> Result<Arc<dyn tw_gateway::plugin::PluginHost>, tw_gateway::plugin::LoadError> {
        Ok(Arc::new(RunningHost(tw_gateway::plugin::Engine::load(
            &FakeEngine,
            source,
        )?)))
    }
}

/// 试跑接到数据面上：记下的请求和回答各跑一遍，前后两份都打着码，日志交回来、不进
/// 插件自己的日志
#[tokio::test]
async fn a_trial_runs_the_plugin_on_the_recorded_request_and_answer() {
    let b = bed();
    b.gw.set_plugin_engine(Arc::new(Running));
    let src = source(
        json!({"name": "Both", "api": 1, "permissions": ["system", "reply.text"]}),
        &["onRequest", "onReplyText"],
    );
    let id = b.install(&src, json!({})).await;
    let key = "sk-ant-api03-TRIALKEYAAAAAAAAAAAAAAAAAAAA";
    let request = json!({
        "model": "claude-sonnet-4-5", "max_tokens": 64, "stream": true,
        "system": "Be brief.",
        "messages": [{"role": "user", "content": format!("my key is {key}")}]
    })
    .to_string();
    let answer = [
        json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","model":"m","content":[],"usage":{"input_tokens":1,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello there"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
        json!({"type":"message_stop"}),
    ]
    .iter()
    .map(|c| format!("event: {}\ndata: {c}\n\n", c["type"].as_str().unwrap()))
    .collect::<String>();
    {
        let g = b.store.lock().await;
        g.db().insert(&row(9, 1_000)).unwrap();
        g.record_body(
            1_000,
            9,
            tw_store::Which::Request,
            request.as_bytes(),
            request.len(),
        );
        g.record_body(
            1_000,
            9,
            tw_store::Which::Response,
            answer.as_bytes(),
            answer.len(),
        );
    }
    let (st, v) = call(
        &b.app,
        "POST",
        &format!("/plugins/{id}/trial"),
        Some(json!({"request_id": 9})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["error"].is_null(), "{v}");
    assert_eq!(v["request"]["outcome"], "changed", "{v}");
    let after = v["request"]["after"].as_str().unwrap();
    assert!(after.contains("Be brief. Today is Friday."), "{after}");
    assert_eq!(v["reply"]["outcome"], "changed", "{v}");
    let reply: Value = serde_json::from_str(v["reply"]["after"].as_str().unwrap()).unwrap();
    assert_eq!(reply["content"][0]["text"], "HELLO THERE");
    assert!(
        !v.to_string().contains("TRIALKEY"),
        "a secret was shown: {v}"
    );
    let logs = v["logs"].as_array().unwrap();
    assert_eq!(logs.len(), 1, "{v}");
    assert_eq!(logs[0]["hook"], "request");
    assert_eq!(logs[0]["request_id"], 9);
    // 试跑不进插件自己的日志和计数
    let (_, mine) = call(&b.app, "GET", &format!("/plugins/{id}/logs"), None).await;
    assert!(
        mine.as_array().is_none_or(|l| l.is_empty()),
        "the trial was logged: {mine}"
    );
}

/// 休眠的插件（停用着、运行时没起，只有显示用的 manifest）也试得了：试之前真的编一遍
#[tokio::test]
async fn a_dormant_plugin_is_compiled_for_a_trial() {
    let b = bed();
    b.gw.set_plugin_engine(Arc::new(Running));
    let src = source(
        json!({"name": "Both", "api": 1, "permissions": ["system", "reply.text"]}),
        &["onRequest", "onReplyText"],
    );
    let id = b.install(&src, json!({"enabled": false})).await;
    // 换一个运行时，编过的都清掉：一个插件都没开，它就休眠了
    b.gw.set_plugin_engine(Arc::new(Running));
    let a = b.gw.runtime().plugins.get(&id).unwrap().clone();
    assert!(a.ready().unwrap().dormant());
    let request = json!({
        "model": "claude-sonnet-4-5", "max_tokens": 64,
        "system": "Be brief.",
        "messages": [{"role": "user", "content": "hi"}]
    })
    .to_string();
    {
        let g = b.store.lock().await;
        g.db().insert(&row(9, 1_000)).unwrap();
        g.record_body(
            1_000,
            9,
            tw_store::Which::Request,
            request.as_bytes(),
            request.len(),
        );
    }
    let (st, v) = call(
        &b.app,
        "POST",
        &format!("/plugins/{id}/trial"),
        Some(json!({"request_id": 9})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["error"].is_null(), "{v}");
    assert_eq!(v["request"]["outcome"], "changed", "{v}");
    let after = v["request"]["after"].as_str().unwrap();
    assert!(after.contains("Be brief. Today is Friday."), "{after}");
}

/// 改得了回答里工具调用的插件：一份设置、一份范围
fn rewrite_calls() -> String {
    source(
        json!({"name": "改工具调用", "api": 1, "permissions": ["reply.tool_calls"],
               "match": {"models": ["claude-*", "gpt-*"]},
               "settings": {"mode": {"type": "string", "label": "方式", "default": "a"},
                            "depth": {"type": "number", "label": "层数", "default": 2}}}),
        &["onToolCall"],
    )
}

/// 一份 `PluginUpdate`：开关、出错时怎么办、模型范围、设置
fn update_body(enabled: bool, on_error: &str, models: Value, settings: Value) -> Value {
    json!({"enabled": enabled, "on_error": on_error,
           "scope": {"clients": [], "models": models, "upstreams": []},
           "settings": settings})
}

async fn put(b: &Bed, path: &str, mut body: Value) -> (StatusCode, Value) {
    body["base_version"] = json!(b.version().await);
    call(&b.app, "PUT", path, Some(body)).await
}

/// 网页那条路改不了工具调用插件做什么：打开它、改设置、改范围都要点过头。停用、改出错时
/// 怎么办、排顺序、删照常；确认过的那条路什么都改得了
#[tokio::test]
async fn a_tool_call_plugin_is_turned_on_or_steered_only_after_a_confirmation() {
    let b = bed();
    let id = b
        .install(
            &rewrite_calls(),
            json!({"enabled": false,
                   "scope": {"clients": [], "models": ["claude-*", "gpt-*"], "upstreams": []}}),
        )
        .await;
    let other = b.install(&shout(), json!({})).await;
    let at = format!("/plugins/{id}");
    let before = b.config();
    let as_is = || {
        update_body(
            false,
            "reject",
            json!(["claude-*", "gpt-*"]),
            json!({"mode": "a", "depth": 2}),
        )
    };

    for (what, body) in [
        (
            "turning it on",
            update_body(
                true,
                "reject",
                json!(["claude-*", "gpt-*"]),
                json!({"mode": "a", "depth": 2}),
            ),
        ),
        (
            "a setting",
            update_body(
                false,
                "reject",
                json!(["claude-*", "gpt-*"]),
                json!({"mode": "b", "depth": 2}),
            ),
        ),
        (
            "a number setting",
            update_body(
                false,
                "reject",
                json!(["claude-*", "gpt-*"]),
                json!({"mode": "a", "depth": 3}),
            ),
        ),
        (
            "the scope",
            update_body(
                false,
                "reject",
                json!(["*"]),
                json!({"mode": "a", "depth": 2}),
            ),
        ),
        (
            "the scope by removing an entry",
            update_body(
                false,
                "reject",
                json!(["claude-*"]),
                json!({"mode": "a", "depth": 2}),
            ),
        ),
    ] {
        let (st, v) = put(&b, &at, body).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{what}: {v}");
        assert_eq!(
            v["code"], "control.plugin.needs_confirmation",
            "{what}: {v}"
        );
        assert_eq!(v["args"]["plugin"], "改工具调用", "{what}: {v}");
        assert_eq!(b.config(), before, "{what}");
    }

    // 什么都没变、只是交回原样（顺序不同、没给的设置按默认值算）：照收
    let (st, v) = put(&b, &at, as_is()).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, v) = put(
        &b,
        &at,
        update_body(false, "reject", json!(["gpt-*", "claude-*"]), json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // 出错时怎么办照改
    let (st, v) = put(
        &b,
        &at,
        update_body(
            false,
            "skip",
            json!(["claude-*", "gpt-*"]),
            json!({"mode": "a"}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        b.parsed().plugins[0].on_error,
        tw_config::PluginOnError::Skip
    );

    // 确认过的那条路：打开、改设置、改范围一次改完
    let (st, v) = put(
        &b,
        &format!("{at}/confirmed"),
        update_body(
            true,
            "skip",
            json!(["claude-*"]),
            json!({"mode": "b", "depth": 5}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let p = b.parsed().plugins[0].clone();
    assert!(p.enabled);
    assert_eq!(p.scope.models, ["claude-*"]);
    assert_eq!(p.settings["mode"], serde_yaml_ng::Value::from("b"));
    assert_eq!(p.settings["depth"], serde_yaml_ng::Value::from(5));
    assert_eq!(b.plugin(&id).await["status"], json!({"kind": "ok"}));

    // 开着的时候：改设置照样要点头；只改出错时怎么办不用
    let (st, v) = put(
        &b,
        &at,
        update_body(
            true,
            "skip",
            json!(["claude-*"]),
            json!({"mode": "c", "depth": 5}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    let (st, v) = put(
        &b,
        &at,
        update_body(
            true,
            "reject",
            json!(["claude-*"]),
            json!({"mode": "b", "depth": 5}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // 停用照常
    let (st, v) = put(
        &b,
        &at,
        update_body(
            false,
            "reject",
            json!(["claude-*"]),
            json!({"mode": "b", "depth": 5}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(!b.parsed().plugins[0].enabled);

    // 排顺序、删照常
    let (st, v) = put(&b, "/plugins/order", json!({"ids": [other, id]})).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, v) = call(
        &b.app,
        "DELETE",
        &format!("{at}?base_version={}", b.version().await),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(b.parsed().plugins.iter().all(|p| p.id != id));

    // 确认过的那条路也要插件在
    let (st, _) = put(&b, "/plugins/nobody/confirmed", as_is()).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// 没有工具调用权限的插件：网页那条路照常打开、改设置、改范围
#[tokio::test]
async fn a_plugin_without_tool_calls_is_changed_without_a_confirmation() {
    let b = bed();
    let id = b.install(&add_date(), json!({"enabled": false})).await;
    let (st, v) = put(
        &b,
        &format!("/plugins/{id}"),
        update_body(
            true,
            "reject",
            json!(["gpt-*"]),
            json!({"note": "明天", "days": 4}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(b.parsed().plugins[0].enabled);
}

/// 批准的那份字节读不回来（文件和底稿都被动过）：真的权限编不出来，按改得了工具调用算
/// —— 它此刻跑不了，可一旦又跑得了，网页替它打开的开关就生效了。列表上显示的是之前编过
/// 的那一份（只拿来显示），判断不认它
#[tokio::test]
async fn a_plugin_whose_permissions_cannot_be_read_needs_a_confirmation_too() {
    let b = bed();
    let id = b.install(&shout(), json!({"enabled": false})).await;
    std::fs::write(b.file(&id), "tampered").unwrap();
    std::fs::write(b.approved(&id), "tampered too").unwrap();
    b.gw.reload_plugins();
    assert_eq!(
        b.gw.runtime().plugins.get(&id).unwrap().broken(),
        Some(&tw_gateway::plugin::Broken::Changed)
    );
    let (st, v) = put(
        &b,
        &format!("/plugins/{id}"),
        update_body(true, "reject", json!([]), json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["code"], "control.plugin.needs_confirmation");
    assert_eq!(v["args"]["plugin"], "Shout");
    // 改出错时怎么办照常
    let (st, v) = put(
        &b,
        &format!("/plugins/{id}"),
        update_body(false, "skip", json!([]), json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, v) = put(
        &b,
        &format!("/plugins/{id}/confirmed"),
        update_body(true, "skip", json!([]), json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(b.parsed().plugins[0].enabled);
}

/// 远程 core：配置不在默认的地方，插件文件就在那份配置旁边 —— 文件由 core 自己写
#[tokio::test]
async fn plugin_files_live_next_to_the_configuration_wherever_it_is() {
    let b = bed_in("srv/thinkwatch/etc");
    let id = b.install(&shout(), json!({})).await;
    let expected = b.dir.join("plugins").join(format!("{id}.js"));
    assert!(expected.exists(), "{}", expected.display());
    assert!(
        b.dir
            .join("plugins/.approved")
            .join(format!("{id}.js"))
            .exists()
    );
    assert_eq!(b.plugin(&id).await["status"], json!({"kind": "ok"}));
    assert_eq!(
        tw_control::plugins::dir_of(Path::new("config.yaml")),
        PathBuf::from(".")
    );
}

/// 一份手写的配置里插件文件改了（core 不在跑的时候）：启动时读到的就是「变了」
#[tokio::test]
async fn a_plugin_changed_while_core_was_down_starts_out_changed() {
    let b = bed();
    let id = b.install(&shout(), json!({})).await;
    std::fs::write(b.file(&id), "tampered").unwrap();
    // 重新起一份网关和控制面，读同一份配置
    let cfg = b.parsed();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    gw.set_plugin_engine(Arc::new(FakeEngine));
    let _mgr = ConfigManager::new(b.dir.join("config.yaml"), gw.clone(), gw.bus.clone());
    let p = gw.runtime().plugins.get(&id).unwrap().clone();
    assert_eq!(p.broken(), Some(&tw_gateway::plugin::Broken::Changed));
}

/// 真的沙箱：从源码到装上、文件被改、批准，整条路走一遍（不跑钩子，那是数据面的事）
#[tokio::test]
async fn a_real_plugin_goes_through_the_sandbox_from_source_to_approval() {
    let b = bed_with("real", false);
    let src = r#"export const manifest = {
  name: "Add date",
  api: 1,
  permissions: ["system"],
  match: { models: ["claude-*"] },
  settings: { note: { type: "string", label: "Note", default: "today" } },
};
export function onRequest(req, ctx) {
  return { ...req, system: `${req.system} ${ctx.settings.note}` };
}
"#;
    let (st, v) = call(
        &b.app,
        "POST",
        "/plugins/inspect",
        Some(json!({"source": src})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["error"].is_null(), "{v}");
    assert_eq!(v["manifest"]["name"], "Add date");
    assert_eq!(v["manifest"]["scope"]["models"], json!(["claude-*"]));

    let broken = src.replace("return {", "return {{");
    let (_, v) = call(
        &b.app,
        "POST",
        "/plugins/inspect",
        Some(json!({"source": broken})),
    )
    .await;
    assert!(v["manifest"].is_null(), "{v}");
    assert!(v["error"]["line"].is_number(), "{v}");

    let id = b
        .install(
            src,
            json!({"scope": {"clients": [], "models": ["claude-*"], "upstreams": []}}),
        )
        .await;
    assert_eq!(id, "add-date");
    assert_eq!(b.plugin(&id).await["status"], json!({"kind": "ok"}));
    assert!(b.gw.runtime().plugins.get(&id).unwrap().ready().is_some());

    let edited = src.replace("today", "tomorrow");
    std::fs::write(b.file(&id), &edited).unwrap();
    b.gw.reload_plugins();
    assert_eq!(b.plugin(&id).await["status"], json!({"kind": "changed"}));
    let (st, v) = call(
        &b.app,
        "POST",
        &format!("/plugins/{id}/approve"),
        Some(json!({"sha256": sha(&edited)})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let v = b.plugin(&id).await;
    assert_eq!(v["status"], json!({"kind": "ok"}));
    assert_eq!(v["settings_schema"][0]["default"], "tomorrow");
}
