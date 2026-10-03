//! 脚本插件的管理面：装、保存、改写、文件被改了、批准、排顺序、删、日志、记录，以及
//! 什么时候要在系统的确认框里点头。
//!
//! 断言落在**磁盘上**：插件文件和底稿写没写、写的是不是那一份字节、配置里那一条长
//! 什么样 —— 网关照着这些重读插件，哈希对不上就不跑（不变式 I9）。插件的配置（出错时
//! 怎么办、范围、设置的值）在插件文件自己的 manifest 里，配置里只有 id、文件、哈希和
//! 开关。引擎是假的（`tw_gateway::plugin::fake`）：它照真的那一套把 manifest 读成纯数据，
//! 不跑 JavaScript。

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
    fn read(&self, path: PathBuf) -> String {
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
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
    /// 装一个（网页那条路），返回 id
    async fn install(&self, src: &str, extra: Value) -> String {
        self.install_at("/plugins", src, extra).await
    }
    /// 装一个（点过头的那条路），返回 id
    async fn install_confirmed(&self, src: &str, extra: Value) -> String {
        self.install_at("/plugins/confirmed", src, extra).await
    }
    async fn install_at(&self, path: &str, src: &str, extra: Value) -> String {
        let mut body = json!({
            "source": src,
            "enabled": true,
            "base_version": self.version().await,
        });
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (st, v) = call(&self.app, "POST", path, Some(body)).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let cfg = self.parsed();
        cfg.plugins.last().unwrap().id.clone()
    }
    /// 保存：源码和开关
    async fn save(&self, id: &str, src: &str, enabled: bool) -> (StatusCode, Value) {
        self.save_at(&format!("/plugins/{id}"), src, enabled).await
    }
    async fn save_confirmed(&self, id: &str, src: &str, enabled: bool) -> (StatusCode, Value) {
        self.save_at(&format!("/plugins/{id}/confirmed"), src, enabled)
            .await
    }
    async fn save_at(&self, path: &str, src: &str, enabled: bool) -> (StatusCode, Value) {
        call(
            &self.app,
            "PUT",
            path,
            Some(json!({"source": src, "enabled": enabled,
                        "base_version": self.version().await})),
        )
        .await
    }
    /// 改写源码里的数据（`POST /plugins/rewrite`），交回改写之后的源码
    async fn rewrite(&self, src: &str, on_error: &str, models: Value, settings: Value) -> String {
        let (st, v) = call(
            &self.app,
            "POST",
            "/plugins/rewrite",
            Some(json!({"source": src, "on_error": on_error,
                        "scope": {"clients": [], "models": models, "upstreams": []},
                        "settings": settings})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        v["source"].as_str().unwrap().to_string()
    }
    /// 批准磁盘上那一份
    async fn approve_at(&self, path: &str, src: &str) -> (StatusCode, Value) {
        call(
            &self.app,
            "POST",
            path,
            Some(json!({"sha256": sha(src), "base_version": self.version().await})),
        )
        .await
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
               "settings": {"note": {"type": "string", "label": "附加内容", "value": "今天"},
                            "days": {"type": "number", "label": "天数", "value": 1}}}),
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

/// 配置里那一条只有这四样：出错时怎么办、范围、设置都在插件文件里
fn assert_four_fields(config: &str) {
    for gone in ["on_error:", "scope:", "settings:"] {
        assert!(!config.contains(gone), "{gone} in\n{config}");
    }
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
    assert_eq!(m["on_error"], "reject");
    assert_eq!(
        m["hooks"],
        json!({"request": true, "reply_text": false, "tool_call": false})
    );
    // 设置项按源码里写的先后（`source` 写出来的 JSON 键按字母排），带着此刻的值
    assert_eq!(m["settings_schema"][0]["key"], "days");
    assert_eq!(m["settings_schema"][0]["kind"], "number");
    assert_eq!(m["settings_schema"][0]["value"], json!(1.0));
    assert_eq!(m["settings_schema"][1]["key"], "note");
    assert_eq!(m["settings_schema"][1]["value"], "今天");

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

    // 不是纯数据的 manifest：说得出在哪一行哪一列
    let expr = "export const manifest = {\n  name: \"x\" + \"y\",\n  api: 1,\n  permissions: [\"system\"],\n};\nexport function onRequest(req, ctx) {}\n";
    let (_, v) = call(
        &b.app,
        "POST",
        "/plugins/inspect",
        Some(json!({"source": expr})),
    )
    .await;
    assert_eq!(
        v["error"]["message"]["code"], "gw.plugin.manifest_not_data_at",
        "{v}"
    );
    assert_eq!(
        (v["error"]["line"].clone(), v["error"]["column"].clone()),
        (json!(2), json!(13))
    );

    // 什么都没留下
    assert_eq!(b.config(), BASE);
    assert!(files_in(&b.dir.join("plugins")).is_empty());
}

#[tokio::test]
async fn installing_writes_the_file_its_approved_copy_and_one_entry() {
    let b = bed();
    let src = add_date();
    let id = b.install(&src, json!({})).await;
    // 名字里没有拉丁字母
    assert_eq!(id, "plugin");
    assert_eq!(b.read(b.file(&id)), src);
    assert_eq!(b.read(b.approved(&id)), src);
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

    // 配置里只有 id、文件、哈希和开关
    let cfg = b.parsed();
    let p = &cfg.plugins[0];
    assert_eq!(p.file, "plugins/plugin.js");
    assert_eq!(p.sha256, sha(&src));
    assert!(p.enabled);
    assert!(
        b.config().ends_with(&format!(
            "plugins:\n  - id: plugin\n    file: plugins/plugin.js\n    sha256: {}\n    enabled: true\n",
            sha(&src)
        )),
        "{}",
        b.config()
    );
    // 注释还在
    assert!(b.config().contains("# 默认那把"));

    // 出错时怎么办、范围、设置的值都是文件里写的
    let v = b.plugin(&id).await;
    assert_eq!(v["status"], json!({"kind": "ok"}));
    assert_eq!(v["name"], "附加日期");
    assert_eq!(v["description"], "在系统提示里写上今天的日期");
    assert_eq!(v["sha256"], sha(&src));
    assert_eq!(v["on_error"], "reject");
    assert_eq!(v["scope"]["clients"], json!(["claude-code"]));
    assert_eq!(v["settings_schema"][0]["value"], json!(1.0));
    assert_eq!(v["settings_schema"][1]["value"], "今天");
    assert!(v.get("settings").is_none(), "{v}");
    assert_eq!(v["stats"]["calls"], 0);
    // 网关手里的那一份能跑
    let rt = b.gw.runtime();
    let a = rt.plugins.get(&id).unwrap();
    assert!(a.ready().is_some());
    assert_eq!(a.settings["note"], json!("今天"));
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
        ("rewrite", "control.plugin.reserved_id"),
        ("confirmed", "control.plugin.reserved_id"),
        ("shout", "control.plugin.id_taken"),
    ] {
        let (st, v) = call(
            &b.app,
            "POST",
            "/plugins",
            Some(json!({"source": shout(), "id": id, "enabled": true})),
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
    let body = json!({"source": shout(), "enabled": true});
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

/// 装之前编一遍：编不成、manifest 不是纯数据的都不装，**一个文件都不写**
#[tokio::test]
async fn a_plugin_that_does_not_load_is_not_installed() {
    let b = bed();
    for (src, code) in [
        (
            format!("{}// @@syntax@@\n", add_date()),
            "gw.plugin.syntax_at",
        ),
        (
            "export const manifest = { name: NAME, api: 1, permissions: [\"system\"] };\nexport function onRequest(req) {}\n".to_string(),
            "gw.plugin.manifest_not_data_at",
        ),
        (
            source(
                json!({"name": "x", "api": 1, "permissions": ["system"],
                       "settings": {"a": {"type": "number", "label": "A", "default": 1}}}),
                &["onRequest"],
            ),
            "gw.plugin.manifest",
        ),
    ] {
        let (st, v) = call(
            &b.app,
            "POST",
            "/plugins",
            Some(json!({"source": src, "enabled": true})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["code"], code, "{src}");
    }
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
        Some(json!({"source": shout(), "enabled": true, "base_version": "not-this-one"})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(b.config(), BASE);
    assert!(!b.file("shout").exists());
    assert!(!b.approved("shout").exists());

    // 保存同理：旧的那一份原样回来
    let id = b.install(&shout(), json!({})).await;
    for newer in [
        shout().replace("Shout", "Louder"),
        b.rewrite(&shout(), "skip", json!([]), json!({})).await,
    ] {
        let (st, v) = call(
            &b.app,
            "PUT",
            &format!("/plugins/{id}"),
            Some(json!({"source": newer, "enabled": true, "base_version": "not-this-one"})),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "{v}");
        assert_eq!(b.read(b.file(&id)), shout());
        assert_eq!(b.read(b.approved(&id)), shout());
    }
}

/// 改代码：文件、底稿、哈希一起换成新的一份，开关照交来的
#[tokio::test]
async fn saving_new_code_rewrites_the_file_the_copy_and_the_hash() {
    let b = bed();
    let id = b.install(&add_date(), json!({})).await;
    let newer = source(
        json!({"name": "附加日期", "api": 1, "permissions": ["system"],
               "settings": {"note": {"type": "string", "label": "附加内容", "value": "后天"},
                            "loud": {"type": "boolean", "label": "大声", "value": true}}}),
        &["onRequest"],
    );
    let (st, v) = b.save(&id, &newer, false).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(b.read(b.file(&id)), newer);
    assert_eq!(b.read(b.approved(&id)), newer);
    let p = &b.parsed().plugins[0];
    assert_eq!(p.sha256, sha(&newer));
    assert!(!p.enabled);
    let v = b.plugin(&id).await;
    assert_eq!(v["status"], json!({"kind": "disabled"}));
    assert_eq!(v["settings_schema"][0]["key"], "loud");

    // 编不成的不存，文件不动
    let (st, v) = b.save(&id, &format!("{newer}// @@syntax@@\n"), true).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["code"], "gw.plugin.syntax_at");
    assert_eq!(b.read(b.file(&id)), newer);

    let (st, _) = b.save("nobody", &newer, true).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// 只改了数据（出错时怎么办、范围、设置的值）：文件、底稿和哈希一次换掉，**中间没有
/// 「文件变了」的那一刻** —— 目录监听开着也看不到，插件一直跑着
#[tokio::test]
async fn a_data_only_save_swaps_the_file_and_the_hash_without_a_changed_state() {
    let b = bed();
    let src = add_date();
    let id = b.install(&src, json!({})).await;
    let _w = tw_control::plugins::spawn_watcher(b.gw.clone(), &b.dir.join("config.yaml")).unwrap();
    let mut events = b.gw.bus.subscribe();
    let newer = b
        .rewrite(
            &src,
            "skip",
            json!(["claude-*"]),
            json!({"note": "明天", "days": 3}),
        )
        .await;
    assert_ne!(newer, src);
    let (st, v) = b.save(&id, &newer, true).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(b.read(b.file(&id)), newer);
    assert_eq!(b.read(b.approved(&id)), newer);
    assert_eq!(b.parsed().plugins[0].sha256, sha(&newer));
    // 监听等一等：它要是看到了「变了」，这里就是 changed
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    let v = b.plugin(&id).await;
    assert_eq!(v["status"], json!({"kind": "ok"}), "{v}");
    assert_eq!(v["on_error"], "skip");
    assert_eq!(v["scope"]["models"], json!(["claude-*"]));
    // 范围是整份交的：交来的 clients 是空的
    assert_eq!(v["scope"]["clients"], json!([]));
    assert_eq!(v["settings_schema"][0]["value"], json!(3.0));
    assert_eq!(v["settings_schema"][1]["value"], "明天");
    let a = b.gw.runtime().plugins.get(&id).unwrap().clone();
    assert!(a.ready().is_some());
    assert_eq!(a.settings["days"], json!(3));
    while let Ok(ev) = events.try_recv() {
        assert!(
            !matches!(ev, tw_api::Event::PluginFailed { .. }),
            "a data-only save was announced as a failure: {ev:?}"
        );
    }
}

/// 源码和批准的一字不差：只改开关，**文件不动** —— 磁盘上被人改过、还没批准的那一份
/// 照样留着待批
#[tokio::test]
async fn turning_a_plugin_off_and_on_leaves_the_files_alone() {
    let b = bed();
    let id = b.install(&add_date(), json!({})).await;
    let edited = format!("{}// 还没批准的改动\n", add_date());
    std::fs::write(b.file(&id), &edited).unwrap();
    b.gw.reload_plugins();
    for enabled in [false, true] {
        let (st, v) = b.save(&id, &add_date(), enabled).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(b.parsed().plugins[0].enabled, enabled);
        assert_eq!(b.read(b.file(&id)), edited);
        assert_eq!(b.read(b.approved(&id)), add_date());
        assert_eq!(b.parsed().plugins[0].sha256, sha(&add_date()));
    }
    assert_eq!(b.plugin(&id).await["status"], json!({"kind": "changed"}));
}

/// 改写：只换 manifest 那一段，交回改写之后的源码；**什么都不写**
#[tokio::test]
async fn rewriting_returns_the_new_source_and_writes_nothing() {
    let b = bed();
    let src = format!("// 上面的注释\n{}// 下面的注释\n", add_date());
    let out = b
        .rewrite(
            &src,
            "skip",
            json!([" gpt-* ", "claude-*"]),
            json!({"days": 2.5}),
        )
        .await;
    assert!(
        out.starts_with("// 上面的注释\nexport const manifest = {\n"),
        "{out}"
    );
    assert!(out.ends_with("// 下面的注释\n"), "{out}");
    assert!(out.contains("  on_error: \"skip\",\n"), "{out}");
    // 范围是整份交的：交来的 clients 是空的，就改成空的
    assert!(
        out.contains("match: { clients: [], models: [\"gpt-*\", \"claude-*\"] },"),
        "{out}"
    );
    assert!(out.contains("value: 2.5 },"), "{out}");
    // 改写的结果原样交给 inspect，读出来就是改成的那样
    let (_, v) = call(
        &b.app,
        "POST",
        "/plugins/inspect",
        Some(json!({"source": out})),
    )
    .await;
    assert_eq!(v["manifest"]["on_error"], "skip");
    assert_eq!(
        v["manifest"]["scope"]["models"],
        json!(["gpt-*", "claude-*"])
    );
    assert_eq!(v["manifest"]["settings_schema"][0]["value"], json!(2.5));
    // 交来的就是原来的值：原样交回，一个字节不变
    let (st, v) = call(
        &b.app,
        "POST",
        "/plugins/rewrite",
        Some(json!({"source": src, "on_error": "reject",
                    "scope": {"clients": ["claude-code"], "models": [], "upstreams": []},
                    "settings": {"note": "今天"}})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["source"], src);

    for (body, code) in [
        (
            json!({"source": src, "on_error": "reject",
                   "scope": {"clients": [], "models": [], "upstreams": []},
                   "settings": {"colour": "red"}}),
            "gw.plugin.setting_unknown",
        ),
        (
            json!({"source": src, "on_error": "reject",
                   "scope": {"clients": [], "models": [], "upstreams": []},
                   "settings": {"days": true}}),
            "gw.plugin.setting_type",
        ),
        (
            json!({"source": src, "on_error": "reject",
                   "scope": {"clients": [" "], "models": [], "upstreams": []},
                   "settings": {}}),
            "control.plugin.blank_pattern",
        ),
        (
            json!({"source": "export const manifest = make();", "on_error": "reject",
                   "scope": {"clients": [], "models": [], "upstreams": []},
                   "settings": {}}),
            "gw.plugin.manifest_not_data_at",
        ),
        (
            json!({"source": "const nothing = 1;", "on_error": "reject",
                   "scope": {"clients": [], "models": [], "upstreams": []},
                   "settings": {}}),
            "gw.plugin.manifest_not_data",
        ),
    ] {
        let (st, v) = call(&b.app, "POST", "/plugins/rewrite", Some(body)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["code"], code);
    }
    assert_eq!(b.config(), BASE);
    assert!(files_in(&b.dir.join("plugins")).is_empty());
}

/// I9：磁盘上的文件被改了，插件停用、说一声；看过改动、批准了才回来
#[tokio::test]
async fn a_file_edited_on_disk_stops_the_plugin_until_the_change_is_approved() {
    let b = bed();
    let id = b.install(&add_date(), json!({})).await;
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
    // 文件本身没被动过
    assert_eq!(std::fs::read_to_string(b.file(&id)).unwrap(), again);
    assert_four_fields(&b.config());
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
    assert_eq!(b.parsed().plugins[0].sha256, sha(&add_date()));

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
               "settings": {"mode": {"type": "string", "label": "方式", "value": "a"},
                            "depth": {"type": "number", "label": "层数", "value": 2}}}),
        &["onToolCall"],
    )
}

fn refused(st: StatusCode, v: &Value, what: &str) {
    assert_eq!(st, StatusCode::FORBIDDEN, "{what}: {v}");
    assert_eq!(
        v["code"], "control.plugin.needs_confirmation",
        "{what}: {v}"
    );
}

/// 确认的规则（契约附录四第 3 节），表里的每一行：网页调得到的那条路拒不拒，点过头的那条
/// 路收不收。拒绝的那几次，配置和文件一个字节都不动
#[tokio::test]
async fn the_confirmation_rule_row_by_row() {
    let b = bed();
    let calls = rewrite_calls();

    // 装一个改得了工具调用的：网页那条路拒绝，点过头的那条收
    let before = b.config();
    let (st, v) = call(
        &b.app,
        "POST",
        "/plugins",
        Some(json!({"source": calls, "enabled": false})),
    )
    .await;
    refused(st, &v, "installing a tool-call plugin");
    assert_eq!(v["args"]["plugin"], "改工具调用");
    assert_eq!(b.config(), before);
    assert!(files_in(&b.dir.join("plugins")).is_empty());
    let id = b
        .install_confirmed(&calls, json!({"id": "calls", "enabled": false}))
        .await;
    assert_eq!(b.read(b.file(&id)), calls);

    // 装一个改不了的：网页那条路照收，开着装也行
    let plain = b.install(&add_date(), json!({"id": "plain"})).await;
    assert!(b.parsed().plugins[1].enabled);

    // 打开改得了工具调用的：拒绝；点过头的那条打得开
    let before = b.config();
    let (st, v) = b.save(&id, &calls, true).await;
    refused(st, &v, "turning it on");
    assert_eq!(b.config(), before);

    // 只改数据：照收（停用着的、开着的都是）
    let data = b
        .rewrite(
            &calls,
            "skip",
            json!(["claude-*"]),
            json!({"mode": "b", "depth": 5}),
        )
        .await;
    let (st, v) = b.save(&id, &data, false).await;
    assert_eq!(st, StatusCode::OK, "a data-only save: {v}");
    assert_eq!(b.read(b.file(&id)), data);
    let (st, v) = b.save_confirmed(&id, &data, true).await;
    assert_eq!(st, StatusCode::OK, "turning it on, confirmed: {v}");
    assert!(b.parsed().plugins[0].enabled);
    let more = b
        .rewrite(&data, "reject", json!(["*"]), json!({"mode": "c"}))
        .await;
    let (st, v) = b.save(&id, &more, true).await;
    assert_eq!(st, StatusCode::OK, "a data-only save while it is on: {v}");
    let v = b.plugin(&id).await;
    assert_eq!(v["status"], json!({"kind": "ok"}));
    assert_eq!(v["scope"]["models"], json!(["*"]));

    // 改代码：拒绝（manifest 以外改了一个字节、manifest 里别的字段改了，都算）
    for (what, code) in [
        (
            "a change outside the manifest",
            format!("{more}// 多一行\n"),
        ),
        (
            "a new label",
            more.replace("label: \"方式\"", "label: \"Mode\""),
        ),
        (
            "dropping reply.tool_calls",
            source(
                json!({"name": "改工具调用", "api": 1, "permissions": ["reply.text"]}),
                &["onReplyText"],
            ),
        ),
    ] {
        assert_ne!(code, more, "{what}");
        let before = b.config();
        let (st, v) = b.save(&id, &code, true).await;
        refused(st, &v, what);
        assert_eq!(b.config(), before, "{what}");
        assert_eq!(b.read(b.file(&id)), more, "{what}");
    }
    let code = format!("{more}// 多一行\n");
    let (st, v) = b.save_confirmed(&id, &code, true).await;
    assert_eq!(st, StatusCode::OK, "a code change, confirmed: {v}");
    assert_eq!(b.read(b.file(&id)), code);

    // 给一个改不了工具调用的插件加上 reply_tool_calls：拒绝
    let adds = source(
        json!({"name": "附加日期", "api": 1, "permissions": ["system", "reply.tool_calls"]}),
        &["onRequest", "onToolCall"],
    );
    let (st, v) = b.save(&plain, &adds, true).await;
    refused(st, &v, "adding reply.tool_calls");
    assert_eq!(v["args"]["plugin"], "附加日期");
    assert_eq!(b.read(b.file(&plain)), add_date());
    // 改不了工具调用的插件改代码：照收
    let plain_code = format!("{}// 改了一行\n", add_date());
    let (st, v) = b.save(&plain, &plain_code, true).await;
    assert_eq!(st, StatusCode::OK, "a code change of a plain plugin: {v}");

    // 批准磁盘上改过的文件：改得了工具调用的拒绝，改不了的照收
    let on_disk = format!("{code}// 磁盘上改的\n");
    std::fs::write(b.file(&id), &on_disk).unwrap();
    let plain_disk = format!("{plain_code}// 磁盘上改的\n");
    std::fs::write(b.file(&plain), &plain_disk).unwrap();
    b.gw.reload_plugins();
    let before = b.config();
    let (st, v) = b
        .approve_at(&format!("/plugins/{id}/approve"), &on_disk)
        .await;
    refused(st, &v, "approving a tool-call plugin's file");
    assert_eq!(b.config(), before);
    assert_eq!(b.read(b.approved(&id)), code);
    let (st, v) = b
        .approve_at(&format!("/plugins/{plain}/approve"), &plain_disk)
        .await;
    assert_eq!(st, StatusCode::OK, "approving a plain plugin's file: {v}");
    let (st, v) = b
        .approve_at(&format!("/plugins/{id}/approve/confirmed"), &on_disk)
        .await;
    assert_eq!(st, StatusCode::OK, "approving, confirmed: {v}");
    assert_eq!(b.read(b.approved(&id)), on_disk);
    assert_eq!(b.plugin(&id).await["status"], json!({"kind": "ok"}));

    // 点过头的那几条也要插件在
    let (st, _) = b.save_confirmed("nobody", &calls, true).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = b
        .approve_at("/plugins/nobody/approve/confirmed", &calls)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// 只改数据的保存同时又要打开它：打开那一半要点头
#[tokio::test]
async fn a_data_only_save_that_also_turns_a_tool_call_plugin_on_needs_a_confirmation() {
    let b = bed();
    let id = b
        .install_confirmed(&rewrite_calls(), json!({"enabled": false}))
        .await;
    let data = b
        .rewrite(&rewrite_calls(), "skip", json!([]), json!({"mode": "z"}))
        .await;
    let (st, v) = b.save(&id, &data, true).await;
    refused(st, &v, "a data-only save that turns it on");
    assert!(!b.parsed().plugins[0].enabled);
    assert_eq!(b.read(b.file(&id)), rewrite_calls());
}

/// 停用、删、排顺序：开着的工具调用插件也照常，网页那条路就行
#[tokio::test]
async fn disabling_deleting_and_reordering_never_need_a_confirmation() {
    let b = bed();
    let id = b.install_confirmed(&rewrite_calls(), json!({})).await;
    let other = b.install(&shout(), json!({})).await;
    assert!(b.parsed().plugins[0].enabled);
    let (st, v) = b.save(&id, &rewrite_calls(), false).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(!b.parsed().plugins[0].enabled);
    let (st, v) = call(
        &b.app,
        "PUT",
        "/plugins/order",
        Some(json!({"ids": [other, id], "base_version": b.version().await})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, v) = call(
        &b.app,
        "DELETE",
        &format!("/plugins/{id}?base_version={}", b.version().await),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(b.parsed().plugins.iter().all(|p| p.id != id));
}

/// 批准的那份字节读不回来（文件和底稿都被动过）：真的权限编不出来，按改得了工具调用算
/// —— 它此刻跑不了，可一旦又跑得了，网页替它打开的开关就生效了。只停用照常
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
    // 打开它（源码交的就是它装上时那一份，可那份字节已经找不到了：按改了代码算）
    let (st, v) = b.save(&id, &shout(), true).await;
    refused(st, &v, "turning on a plugin whose approved bytes are gone");
    assert_eq!(v["args"]["plugin"], "Shout");
    // 批准磁盘上那一份（新的一份读不出 manifest，编不成）：拒绝的是编不成
    let (st, v) = b
        .approve_at(&format!("/plugins/{id}/approve"), "tampered")
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    // 点过头的那条路：打开、换成新的一份都行
    let (st, v) = b.save_confirmed(&id, &shout(), true).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(b.parsed().plugins[0].enabled);
    assert_eq!(b.plugin(&id).await["status"], json!({"kind": "ok"}));
}

/// 直接写配置原文的那几条路（整份写回、按路径改、回滚）也是网页调得到的：打开、批准、装上
/// 一个改得了工具调用的插件，在那里同样要点头；改不了的照常
#[tokio::test]
async fn raw_configuration_writes_cannot_bypass_the_confirmation() {
    let b = bed();
    let id = b
        .install_confirmed(&rewrite_calls(), json!({"id": "calls", "enabled": false}))
        .await;
    let plain = b.install(&shout(), json!({"enabled": false})).await;
    let flip = |text: &str, which: &str| {
        let at = text.find(&format!("  - id: {which}\n")).unwrap();
        let end = text[at..].find("enabled: false").unwrap() + at;
        format!(
            "{}enabled: true{}",
            &text[..end],
            &text[end + "enabled: false".len()..]
        )
    };
    let put = |text: String| {
        let b = &b;
        async move {
            call(
                &b.app,
                "PUT",
                "/config",
                Some(json!({"text": text, "base_version": b.version().await})),
            )
            .await
        }
    };
    // 界面手里的原文：控制面的钥匙是打码的
    let shown = || {
        let b = &b;
        async move {
            let (_, v) = call(&b.app, "GET", "/config", None).await;
            let text = v["text"].as_str().unwrap().to_string();
            assert!(!text.contains("c0ffee00c0ffee00"), "{text}");
            text
        }
    };

    // 整份写回：打开改得了工具调用的，拒绝；改不了的照常
    let before = shown().await;
    let on_disk = b.config();
    let (st, v) = put(flip(&before, &id)).await;
    refused(st, &v, "turning it on in the text");
    assert_eq!(b.config(), on_disk);
    let (st, v) = put(flip(&before, &plain)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(b.parsed().plugins[1].enabled);

    // 按路径改：同样
    let before = b.config();
    let (st, v) = call(
        &b.app,
        "PATCH",
        "/config",
        Some(
            json!({"ops": [{"op": "replace", "path": "/plugins/0/enabled", "value": true}],
                    "base_version": b.version().await}),
        ),
    )
    .await;
    refused(st, &v, "turning it on by path");
    assert_eq!(b.config(), before);

    // 把批准的哈希换成磁盘上改过的那一份 = 批准它：拒绝
    let edited = format!("{}// 磁盘上改的\n", rewrite_calls());
    std::fs::write(b.file(&id), &edited).unwrap();
    let (st, v) = put(shown().await.replace(&sha(&rewrite_calls()), &sha(&edited))).await;
    refused(st, &v, "approving it in the text");
    assert_eq!(b.config(), before);
    std::fs::write(b.file(&id), rewrite_calls()).unwrap();

    // 回滚到它开着的那一版：拒绝
    let (st, v) = b.save_confirmed(&id, &rewrite_calls(), true).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let on = b.version().await;
    let (st, v) = b.save(&id, &rewrite_calls(), false).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let before = b.config();
    let (st, v) = call(
        &b.app,
        "POST",
        "/config/rollback",
        Some(json!({"version": on})),
    )
    .await;
    refused(st, &v, "rolling back to a version where it was on");
    assert_eq!(b.config(), before);

    // 和插件无关的改动照常
    let (st, v) = put(shown().await.replace("# 默认那把", "# 改了一个注释")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
}

/// 0.58 写下的配置：插件那一条还带着出错时怎么办、范围、设置。照样加载（这几项不起作用，
/// 插件按文件里写的跑），**下一次写插件那一节时整节都去掉**，别的字节不动
#[tokio::test]
async fn a_configuration_written_by_0_58_loads_and_loses_the_old_fields_on_the_next_write() {
    let b = bed();
    let a = b.install(&add_date(), json!({"id": "a"})).await;
    let c = b.install(&shout(), json!({"id": "c"})).await;
    let text = b
        .config()
        .replace(
            &format!("  - id: {a}\n"),
            &format!(
                "  # 我的第一个插件\n  - id: {a}\n    on_error: skip\n    scope:\n      models: [\"gpt-*\"]\n    settings:\n      note: 旧的\n      days: [1, 2]\n"
            ),
        )
        .replace(
            &format!("  - id: {c}\n"),
            &format!("  - id: {c}\n    settings: {{ unknown: yes }}\n"),
        );
    let (st, v) = call(
        &b.app,
        "PUT",
        "/config",
        Some(json!({"text": text, "base_version": b.version().await})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(b.config().contains("on_error: skip"));
    // 不起作用：照文件里写的
    let v = b.plugin(&a).await;
    assert_eq!(v["status"], json!({"kind": "ok"}));
    assert_eq!(v["on_error"], "reject");
    assert_eq!(v["scope"]["models"], json!([]));
    assert_eq!(v["settings_schema"][1]["value"], "今天");

    // 下一次写插件那一节（这里是改另一个插件的开关）：两条里的旧字段都去掉
    let (st, v) = b.save(&c, &shout(), false).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let after = b.config();
    assert_four_fields(&after);
    assert!(after.contains("  # 我的第一个插件\n  - id: a\n"), "{after}");
    assert!(after.contains("# 默认那把"), "{after}");
    let p = b.parsed();
    assert_eq!(p.plugins.len(), 2);
    assert!(p.plugins[0].enabled && !p.plugins[1].enabled);
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
  settings: { note: { type: "string", label: "Note", value: "today" } },
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

    let id = b.install(src, json!({})).await;
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
    assert_eq!(v["settings_schema"][0]["value"], "tomorrow");

    // 在界面上改数据：改写、保存，真的沙箱编出来就是改成的那样
    let data = b
        .rewrite(
            &edited,
            "skip",
            json!(["gpt-*"]),
            json!({"note": "next week"}),
        )
        .await;
    assert!(data.ends_with("export function onRequest(req, ctx) {\n  return { ...req, system: `${req.system} ${ctx.settings.note}` };\n}\n"), "{data}");
    let (st, v) = b.save(&id, &data, true).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let v = b.plugin(&id).await;
    assert_eq!(v["status"], json!({"kind": "ok"}));
    assert_eq!(v["on_error"], "skip");
    assert_eq!(v["scope"]["models"], json!(["gpt-*"]));
    assert_eq!(v["settings_schema"][0]["value"], "next week");
    let a = b.gw.runtime().plugins.get(&id).unwrap().clone();
    assert!(a.ready().is_some_and(|h| !h.dormant()));
    assert_eq!(a.settings["note"], json!("next week"));
}

/// 试跑记下的嵌入请求，从控制面一路到真的沙箱：声明了嵌入的插件跑在一项输入一条消息的
/// 视图上，前后两份打着码，回答钩子不试；`inspect` 和列表说得出它处理哪几种请求。只处理
/// 对话的插件说清它当时没跑
#[tokio::test]
async fn a_trial_on_a_recorded_embeddings_request_runs_only_plugins_that_declare_embeddings() {
    let b = bed_with("real", false);
    let scrub = r#"export const manifest = {
  name: "Scrub inputs",
  api: 1,
  permissions: ["messages"],
  requests: ["conversation", "embeddings"],
};
export function onRequest(req, ctx) {
  console.log(ctx.format);
  for (const m of req.messages) {
    for (const p of m.parts) {
      if (p.type === "text") p.text = p.text.replaceAll("PROJECT-X", "[removed]");
    }
  }
  return req;
}
"#;
    let (_, v) = call(
        &b.app,
        "POST",
        "/plugins/inspect",
        Some(json!({"source": scrub})),
    )
    .await;
    assert_eq!(
        v["manifest"]["requests"],
        json!(["conversation", "embeddings"]),
        "{v}"
    );
    let id = b.install(scrub, json!({})).await;
    assert_eq!(
        b.plugin(&id).await["requests"],
        json!(["conversation", "embeddings"])
    );
    let key = "sk-ant-api03-TRIALKEYAAAAAAAAAAAAAAAAAAAA";
    let request = json!({
        "model": "text-embedding-3-small",
        "input": ["PROJECT-X roadmap", format!("key {key}"), [101, 102]]
    })
    .to_string();
    let answer =
        json!({"object": "list", "data": [], "model": "text-embedding-3-small"}).to_string();
    let mut embeddings = row(9, 1_000);
    embeddings.path = "/v1/embeddings".into();
    embeddings.provider = "openai".into();
    embeddings.model = "text-embedding-3-small".into();
    embeddings.sent_model = "text-embedding-3-small".into();
    {
        let g = b.store.lock().await;
        g.db().insert(&embeddings).unwrap();
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
    assert!(v["reply"].is_null(), "{v}");
    assert_eq!(v["request"]["outcome"], "changed", "{v}");
    let after: Value = serde_json::from_str(v["request"]["after"].as_str().unwrap()).unwrap();
    assert_eq!(after["input"][0], "[removed] roadmap");
    assert_eq!(after["input"][2], json!([101, 102]));
    assert!(
        !v.to_string().contains("TRIALKEY"),
        "a secret was shown: {v}"
    );
    assert_eq!(v["logs"][0]["text"], "openai_embeddings", "{v}");

    // 只处理对话的插件：当时它就不在这个请求的范围里
    let chat_only = r#"export const manifest = { name: "Chat only", api: 1, permissions: ["messages"] };
export function onRequest(req) { return req; }
"#;
    let other = b.install(chat_only, json!({})).await;
    assert_eq!(b.plugin(&other).await["requests"], json!(["conversation"]));
    let (st, v) = call(
        &b.app,
        "POST",
        &format!("/plugins/{other}/trial"),
        Some(json!({"request_id": 9})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["request"].is_null(), "{v}");
    assert_eq!(v["error"]["code"], "gw.plugin.not_declared", "{v}");
}
