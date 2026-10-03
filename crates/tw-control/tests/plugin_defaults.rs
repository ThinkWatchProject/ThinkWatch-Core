//! 默认插件：第一次装上（停用着）、用户删了不再加回、用户改了不动、出了新版换上、
//! 新版多要了权限就停用、用户自己的插件占着那个 id 不动、配置不在默认位置也一样，
//! 以及每换入一份配置再走一遍。
//!
//! 断言落在磁盘上：插件文件和底稿、配置里那一条、`plugins/.defaults.json`。规则用自己
//! 造的几个插件测（假引擎）；随 core 发的那一份清单另用真的沙箱整个走一遍。
//!
//! 还有**不起运行时**这一条：装默认插件、列出停用的插件都不编（数着引擎编了几次），
//! 显示用的 manifest 缓存被人改了也骗不过「打开工具调用插件要点头」那道关。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tw_control::plugins::defaults::{Seeded, Seeder, record_path};
use tw_control::{ConfigManager, ControlState};
use tw_gateway::plugin::fake::{FakeEngine, source};
use tw_gateway::plugin::{Engine, LoadError, PluginHost};

/// 数着编了几次的引擎，编的事交给里面那一个
struct Counting {
    inner: Arc<dyn Engine>,
    n: AtomicUsize,
}

impl Counting {
    fn new(inner: Arc<dyn Engine>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            n: AtomicUsize::new(0),
        })
    }
    fn count(&self) -> usize {
        self.n.load(Ordering::SeqCst)
    }
}

impl Engine for Counting {
    fn load(&self, source: &[u8]) -> Result<Arc<dyn PluginHost>, LoadError> {
        self.n.fetch_add(1, Ordering::SeqCst);
        self.inner.load(source)
    }
}

fn real() -> Arc<dyn Engine> {
    Arc::new(tw_gateway::plugin::sandbox::Sandbox)
}

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
    /// 「重启」出来的那一份不拿着目录：它和原来那一份共用
    _tmp: Option<tempfile::TempDir>,
    dir: PathBuf,
    gw: tw_gateway::AppState,
    mgr: Arc<ConfigManager>,
    app: axum::Router,
}

impl Bed {
    fn config(&self) -> String {
        std::fs::read_to_string(self.dir.join("config.yaml")).unwrap()
    }
    fn parsed(&self) -> tw_config::Config {
        tw_config::try_parse(&self.config()).unwrap()
    }
    fn entry(&self, id: &str) -> Option<tw_config::Plugin> {
        self.parsed().plugins.into_iter().find(|p| p.id == id)
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
    /// `.defaults.json` 里记着的：id → 哈希
    fn offered(&self) -> Value {
        let text = std::fs::read_to_string(record_path(&self.dir)).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        v["offered"].clone()
    }
    async fn version(&self) -> String {
        let (_, v) = call(&self.app, "GET", "/overview", None).await;
        v["config_version"].as_str().unwrap().to_string()
    }
    async fn plugin(&self, id: &str) -> Value {
        let (st, v) = call(&self.app, "GET", "/plugins", None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        v.as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no plugin {id}: {v}"))
    }
    /// 开着、出错时跳过、范围和设置都改过 —— 用户用过一阵子的样子
    async fn customize(&self, id: &str, settings: Value) {
        let (st, v) = call(
            &self.app,
            "PUT",
            &format!("/plugins/{id}/confirmed"),
            Some(json!({"enabled": true, "on_error": "skip",
                        "scope": {"clients": [], "models": ["deepseek-chat"], "upstreams": []},
                        "settings": settings, "base_version": self.version().await})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
    }
}

fn bed_in(sub: &str, fake: bool) -> Bed {
    let engine: Arc<dyn Engine> = if fake { Arc::new(FakeEngine) } else { real() };
    bed_with(sub, engine)
}

fn bed_with(sub: &str, engine: Arc<dyn Engine>) -> Bed {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(sub);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.yaml"), BASE).unwrap();
    let mut b = open(dir, engine);
    b._tmp = Some(tmp);
    b
}

impl Bed {
    /// 「重启」：同一个目录上起一份新的网关和控制面（编译结果、显示用的缓存都从头来）
    fn restart(&self, engine: Arc<dyn Engine>) -> Bed {
        open(self.dir.clone(), engine)
    }
}

fn open(dir: PathBuf, engine: Arc<dyn Engine>) -> Bed {
    let p = dir.join("config.yaml");
    let text = std::fs::read_to_string(&p).unwrap();
    let gw = tw_gateway::AppState::new(tw_config::try_parse(&text).unwrap()).unwrap();
    gw.set_plugin_engine(engine);
    let mgr = Arc::new(ConfigManager::new(p, gw.clone(), gw.bus.clone()));
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
        _tmp: None,
        dir,
        gw,
        mgr,
        app: tw_control::router(state),
    }
}

fn bed() -> Bed {
    bed_in("home", true)
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

fn sha(s: &str) -> String {
    tw_gateway::plugin::load::sha256_hex(s.as_bytes())
}

/// 第一版：改系统指令，两项设置，其中一项默认值有两行；只管 deepseek 开头的模型
fn alpha() -> String {
    source(
        json!({"name": "Alpha", "api": 1, "description": "first",
               "permissions": ["system"], "match": {"models": ["deepseek*"]},
               "settings": {"lang": {"type": "string", "label": "语言", "default": "简体中文"},
                            "terms": {"type": "string", "label": "对照表", "default": "登陆=登录\n帐号=账号"}}}),
        &["onRequest"],
    )
}

/// 第二版：权限不变；`terms` 不要了，多了一个 `count`
fn alpha_v2() -> String {
    source(
        json!({"name": "Alpha", "api": 1, "description": "second",
               "permissions": ["system"], "match": {"models": ["deepseek*"]},
               "settings": {"lang": {"type": "string", "label": "语言", "default": "English"},
                            "count": {"type": "number", "label": "次数", "default": 3}}}),
        &["onRequest"],
    )
}

/// 第三版：多要了 `messages`
fn alpha_v3() -> String {
    source(
        json!({"name": "Alpha", "api": 1, "description": "third",
               "permissions": ["system", "messages"], "match": {"models": ["deepseek*"]},
               "settings": {"lang": {"type": "string", "label": "语言", "default": "简体中文"}}}),
        &["onRequest"],
    )
}

fn beta() -> String {
    source(
        json!({"name": "Beta", "api": 1, "permissions": ["reply.text"]}),
        &["onReplyText"],
    )
}

fn seeder(list: &[(&str, &str)]) -> Seeder {
    Seeder::new(list.iter().copied())
}

fn ids(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

/// 第一次：文件、底稿、配置里一条（停用、出错时拒绝、范围照 manifest、设置都是默认值），
/// 记录里记着给出去的哈希。再走一遍什么都不做
#[tokio::test]
async fn the_first_run_adds_every_default_turned_off() {
    let b = bed();
    let (a, c) = (alpha(), beta());
    let s = seeder(&[("alpha", &a), ("beta", &c)]);
    let done = s.seed(&b.mgr).await;
    assert_eq!(ids(&done.added), ["alpha", "beta"], "{done:?}");
    assert!(done.failed.is_empty(), "{done:?}");

    assert_eq!(b.read(b.file("alpha")), a);
    assert_eq!(b.read(b.approved("alpha")), a);
    assert_eq!(b.read(b.file("beta")), c);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for p in [b.file("alpha"), b.approved("alpha"), record_path(&b.dir)] {
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", p.display());
        }
    }
    let p = b.entry("alpha").unwrap();
    assert!(!p.enabled);
    assert_eq!(p.on_error, tw_config::PluginOnError::Reject);
    assert_eq!(p.scope.models, ["deepseek*"]);
    assert_eq!(p.sha256, sha(&a));
    assert_eq!(p.settings["lang"], serde_yaml_ng::Value::from("简体中文"));
    // 两行的默认值照样写进去：一行双引号，读回来一字不差
    assert_eq!(
        p.settings["terms"],
        serde_yaml_ng::Value::from("登陆=登录\n帐号=账号")
    );
    assert!(
        b.config()
            .contains("      terms: \"登陆=登录\\n帐号=账号\"\n"),
        "{}",
        b.config()
    );
    assert!(b.config().contains("# 默认那把"), "{}", b.config());
    assert_eq!(b.offered(), json!({"alpha": sha(&a), "beta": sha(&c)}));

    // 和用户装的插件在同一张单子上，停用着，能跑
    let v = b.plugin("alpha").await;
    assert_eq!(v["status"], json!({"kind": "disabled"}));
    assert_eq!(v["name"], "Alpha");
    assert!(
        b.gw.runtime()
            .plugins
            .get("alpha")
            .unwrap()
            .ready()
            .is_some()
    );

    // 写配置的这一版来源是 defaults，一版、一条历史
    let (_, history) = call(&b.app, "GET", "/config/history", None).await;
    let now = history
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["current"] == true)
        .cloned()
        .unwrap_or_else(|| panic!("{history}"));
    assert_eq!(now["origin"], "defaults", "{history}");

    let text = b.config();
    let again = s.seed(&b.mgr).await;
    assert_eq!(again, Seeded::default());
    assert_eq!(b.config(), text);
}

/// 用户删掉的默认插件不再回来：这一次不回来，重启之后（新的一路）也不回来
#[tokio::test]
async fn a_default_the_user_deleted_never_comes_back() {
    let b = bed();
    let a = alpha();
    seeder(&[("alpha", &a)]).seed(&b.mgr).await;
    let (st, v) = call(
        &b.app,
        "DELETE",
        &format!("/plugins/alpha?base_version={}", b.version().await),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    for list in [vec![("alpha", a.as_str())], vec![("alpha", &*alpha_v2())]] {
        let done = seeder(&list).seed(&b.mgr).await;
        assert_eq!(done, Seeded::default());
        assert!(b.entry("alpha").is_none(), "{}", b.config());
        assert!(!b.file("alpha").exists() && !b.approved("alpha").exists());
        assert_eq!(b.offered(), json!({"alpha": sha(&a)}));
    }
}

/// 用户改过文件（批准了也好、没批准也好、换了源码也好）：新版来了也不动它。
/// 同一次里没动过的那一个照样换成新版
#[tokio::test]
async fn a_default_the_user_changed_is_left_alone() {
    let b = bed();
    let a = alpha();
    let c = beta();
    let d = beta().replace("Beta", "Gamma");
    let e = beta().replace("Beta", "Delta");
    seeder(&[("alpha", &a), ("beta", &c), ("gamma", &d), ("delta", &e)])
        .seed(&b.mgr)
        .await;

    // alpha：磁盘上的文件被改了，没批准
    let edited = format!("{a}// 用户加的一行\n");
    std::fs::write(b.file("alpha"), &edited).unwrap();
    // beta：改了、也批准了
    let approved = format!("{c}// 批准过的改动\n");
    std::fs::write(b.file("beta"), &approved).unwrap();
    b.gw.reload_plugins();
    let (st, v) = call(
        &b.app,
        "POST",
        "/plugins/beta/approve",
        Some(json!({"sha256": sha(&approved)})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // gamma：换了一份源码
    let replaced = source(
        json!({"name": "Mine", "api": 1, "permissions": ["reply.text"]}),
        &["onReplyText"],
    );
    let (st, v) = call(
        &b.app,
        "PUT",
        "/plugins/gamma/source",
        Some(json!({"source": replaced})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let entries = |b: &Bed| ["alpha", "beta", "gamma"].map(|id| b.entry(id).unwrap());
    let before = entries(&b);

    let newer = beta().replace("\"api\":1", "\"api\":1,\"description\":\"v2\"");
    assert_ne!(newer, c);
    let delta = newer.replace("Beta", "Delta");
    let done = seeder(&[
        ("alpha", &alpha_v2()),
        ("beta", &newer),
        ("gamma", &newer.replace("Beta", "Gamma")),
        ("delta", &delta),
    ])
    .seed(&b.mgr)
    .await;
    assert_eq!(ids(&done.updated), ["delta"], "{done:?}");
    assert!(done.added.is_empty() && done.marked.is_empty() && done.failed.is_empty());
    assert_eq!(entries(&b), before);
    assert_eq!(b.read(b.file("alpha")), edited);
    assert_eq!(b.read(b.approved("alpha")), a);
    assert_eq!(b.read(b.file("beta")), approved);
    assert_eq!(b.read(b.file("gamma")), replaced);
    assert_eq!(b.read(b.file("delta")), delta);
    assert_eq!(
        b.offered(),
        json!({"alpha": sha(&a), "beta": sha(&c), "gamma": sha(&d), "delta": sha(&delta)})
    );
}

/// 出了新版、用户没动过：文件、底稿、哈希换成新版；开关、出错时怎么办、范围、还声明着的
/// 设置照旧，新声明的设置取默认值，不再声明的去掉
#[tokio::test]
async fn a_new_version_replaces_an_untouched_default_and_keeps_its_settings() {
    let b = bed();
    let a = alpha();
    seeder(&[("alpha", &a)]).seed(&b.mgr).await;
    b.customize("alpha", json!({"lang": "日本語", "terms": "a=b"}))
        .await;

    let v2 = alpha_v2();
    let done = seeder(&[("alpha", &v2)]).seed(&b.mgr).await;
    assert_eq!(ids(&done.updated), ["alpha"], "{done:?}");
    assert!(
        done.disabled.is_empty() && done.failed.is_empty(),
        "{done:?}"
    );
    assert_eq!(b.read(b.file("alpha")), v2);
    assert_eq!(b.read(b.approved("alpha")), v2);
    let p = b.entry("alpha").unwrap();
    assert_eq!(p.sha256, sha(&v2));
    assert!(p.enabled);
    assert_eq!(p.on_error, tw_config::PluginOnError::Skip);
    assert_eq!(p.scope.models, ["deepseek-chat"]);
    assert_eq!(p.settings["lang"], serde_yaml_ng::Value::from("日本語"));
    assert_eq!(p.settings["count"], serde_yaml_ng::Value::from(3));
    assert!(!p.settings.contains_key("terms"), "{:?}", p.settings);
    assert_eq!(b.offered(), json!({"alpha": sha(&v2)}));
    let v = b.plugin("alpha").await;
    assert_eq!(v["status"], json!({"kind": "ok"}));
    assert_eq!(v["description"], "second");
}

/// 新版已经换上了、记录却没写成（写记录那一步失败了）：补记一笔，别的什么都不动 ——
/// 不会把已经换上的新版当成「用户改过的」
#[tokio::test]
async fn a_lost_record_of_an_update_is_written_again_and_nothing_else_moves() {
    let b = bed();
    let a = alpha();
    seeder(&[("alpha", &a)]).seed(&b.mgr).await;
    b.customize("alpha", json!({"lang": "日本語"})).await;
    let v2 = alpha_v2();
    seeder(&[("alpha", &v2)]).seed(&b.mgr).await;
    // 记录退回到更新之前
    std::fs::write(
        record_path(&b.dir),
        json!({"offered": {"alpha": sha(&a)}}).to_string(),
    )
    .unwrap();
    let before = b.config();
    let done = seeder(&[("alpha", &v2)]).seed(&b.mgr).await;
    assert_eq!(ids(&done.marked), ["alpha"], "{done:?}");
    assert!(
        done.updated.is_empty() && done.failed.is_empty(),
        "{done:?}"
    );
    assert_eq!(b.config(), before);
    assert_eq!(b.offered(), json!({"alpha": sha(&v2)}));
    // 再出一版时照常更新
    let v4 = alpha_v2().replace("second", "fourth");
    let done = seeder(&[("alpha", &v4)]).seed(&b.mgr).await;
    assert_eq!(ids(&done.updated), ["alpha"], "{done:?}");
    assert!(b.entry("alpha").unwrap().enabled);
}

/// 新版要了旧版没要的权限：换上，但停用 —— 用户没答应过的权限不该悄悄开着
#[tokio::test]
async fn a_new_version_that_wants_more_permissions_comes_back_turned_off() {
    let b = bed();
    let a = alpha();
    seeder(&[("alpha", &a)]).seed(&b.mgr).await;
    b.customize("alpha", json!({"lang": "日本語"})).await;

    let v3 = alpha_v3();
    let done = seeder(&[("alpha", &v3)]).seed(&b.mgr).await;
    assert_eq!(ids(&done.updated), ["alpha"], "{done:?}");
    assert_eq!(ids(&done.disabled), ["alpha"], "{done:?}");
    let p = b.entry("alpha").unwrap();
    assert_eq!(p.sha256, sha(&v3));
    assert!(!p.enabled);
    assert_eq!(p.on_error, tw_config::PluginOnError::Skip);
    assert_eq!(p.scope.models, ["deepseek-chat"]);
    assert_eq!(p.settings["lang"], serde_yaml_ng::Value::from("日本語"));
    let v = b.plugin("alpha").await;
    assert_eq!(v["status"], json!({"kind": "disabled"}));
    assert_eq!(v["permissions"], json!(["system", "messages"]));
}

/// 新版多处理了一种请求（`requests` 多了嵌入），权限一样：和多要一个权限一样，换上但
/// 停用 —— 插件看得到、改得了的东西变多了，要用户自己再打开
#[tokio::test]
async fn a_new_version_that_handles_more_kinds_of_request_comes_back_turned_off() {
    let b = bed();
    let scrub = |requests: Value| {
        source(
            json!({"name": "Scrub", "api": 1, "permissions": ["messages"],
                   "requests": requests}),
            &["onRequest"],
        )
    };
    let v1 = scrub(json!(["conversation"]));
    seeder(&[("scrub", &v1)]).seed(&b.mgr).await;
    b.customize("scrub", json!({})).await;
    assert_eq!(b.plugin("scrub").await["requests"], json!(["conversation"]));

    let v2 = scrub(json!(["conversation", "embeddings"]));
    let done = seeder(&[("scrub", &v2)]).seed(&b.mgr).await;
    assert_eq!(ids(&done.updated), ["scrub"], "{done:?}");
    assert_eq!(ids(&done.disabled), ["scrub"], "{done:?}");
    let p = b.entry("scrub").unwrap();
    assert_eq!(p.sha256, sha(&v2));
    assert!(!p.enabled);
    let v = b.plugin("scrub").await;
    assert_eq!(v["status"], json!({"kind": "disabled"}));
    assert_eq!(v["permissions"], json!(["messages"]));
    assert_eq!(v["requests"], json!(["conversation", "embeddings"]));
}

/// 用户自己的插件正好用了一个默认插件的 id：只记一笔「给过了」，它的文件和配置都不动，
/// 之后出了新版也不动
#[tokio::test]
async fn a_user_plugin_that_has_a_default_id_is_untouched() {
    let b = bed();
    let mine = source(
        json!({"name": "My own", "api": 1, "permissions": ["reply.text"]}),
        &["onReplyText"],
    );
    let (st, v) = call(
        &b.app,
        "POST",
        "/plugins",
        Some(
            json!({"source": mine, "id": "alpha", "enabled": true, "on_error": "reject",
                    "scope": {"clients": [], "models": [], "upstreams": []}, "settings": {}}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let before = b.config();

    let a = alpha();
    let done = seeder(&[("alpha", &a)]).seed(&b.mgr).await;
    assert_eq!(ids(&done.marked), ["alpha"], "{done:?}");
    assert!(done.added.is_empty() && done.updated.is_empty(), "{done:?}");
    assert_eq!(b.config(), before);
    assert_eq!(b.read(b.file("alpha")), mine);
    assert_eq!(b.read(b.approved("alpha")), mine);
    assert_eq!(b.offered(), json!({"alpha": sha(&a)}));

    let done = seeder(&[("alpha", &alpha_v2())]).seed(&b.mgr).await;
    assert_eq!(done, Seeded::default());
    assert_eq!(b.config(), before);
    assert_eq!(b.read(b.file("alpha")), mine);
}

/// 远程 core：配置不在默认的地方，默认插件和记录就在那份配置旁边
#[tokio::test]
async fn defaults_live_next_to_the_configuration_wherever_it_is() {
    let b = bed_in("srv/thinkwatch/etc", true);
    let a = alpha();
    let done = seeder(&[("alpha", &a)]).seed(&b.mgr).await;
    assert_eq!(ids(&done.added), ["alpha"], "{done:?}");
    for p in [
        b.dir.join("plugins/alpha.js"),
        b.dir.join("plugins/.approved/alpha.js"),
        b.dir.join("plugins/.defaults.json"),
    ] {
        assert!(p.exists(), "{}", p.display());
    }
    assert_eq!(
        b.plugin("alpha").await["status"],
        json!({"kind": "disabled"})
    );
}

/// 启动之后每换入一份配置再走一遍：这一次别处写了配置，默认插件跟着补上
#[tokio::test]
async fn every_configuration_change_runs_it_again() {
    let b = bed();
    let a = alpha();
    let _task = tw_control::plugins::defaults::spawn(seeder(&[("alpha", &a)]), b.mgr.clone());
    let (st, v) = call(
        &b.app,
        "PUT",
        "/config",
        Some(json!({"text": BASE.replace("# 默认那把", "# 改了一个注释"),
                    "base_version": b.version().await})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while b.entry("alpha").is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "the default was not added after a configuration change"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(b.config().contains("# 改了一个注释"), "{}", b.config());
}

/// 一个编不成的默认插件：别的照样装上；它自己这次不记「给过了」，说一声，同一个问题
/// 只说一次
#[tokio::test]
async fn a_default_that_does_not_load_is_skipped_and_reported_once() {
    let b = bed();
    let mut events = b.gw.bus.subscribe();
    let broken = format!("{}// @@syntax@@\n", beta());
    let a = alpha();
    let s = seeder(&[("broken", &broken), ("alpha", &a)]);
    let done = s.seed(&b.mgr).await;
    assert_eq!(ids(&done.added), ["alpha"], "{done:?}");
    assert_eq!(done.failed.len(), 1, "{done:?}");
    assert_eq!(done.failed[0].0, "broken");
    assert_eq!(done.failed[0].1.code, "gw.plugin.syntax_at");
    assert!(b.entry("broken").is_none());
    assert!(!b.file("broken").exists());
    assert_eq!(b.offered(), json!({"alpha": sha(&a)}));

    let mut failed = Vec::new();
    while let Ok(ev) = events.try_recv() {
        if let tw_api::Event::PluginFailed {
            plugin_id,
            request_id,
            message,
            ..
        } = ev
        {
            failed.push((plugin_id, request_id, message.code));
        }
    }
    assert_eq!(
        failed,
        [(
            "broken".to_string(),
            None,
            "gw.plugin.syntax_at".to_string()
        )]
    );
    let again = s.seed(&b.mgr).await;
    assert_eq!(again.failed.len(), 1);
    while let Ok(ev) = events.try_recv() {
        assert!(
            !matches!(ev, tw_api::Event::PluginFailed { .. }),
            "the same problem was announced twice: {ev:?}"
        );
    }
}

/// 记录读不出来：分不清哪些是用户删掉的，一个都不加、什么都不改
#[tokio::test]
async fn an_unreadable_record_adds_nothing() {
    let b = bed();
    std::fs::create_dir_all(b.dir.join("plugins")).unwrap();
    std::fs::write(record_path(&b.dir), "{ not json").unwrap();
    let done = seeder(&[("alpha", &alpha())]).seed(&b.mgr).await;
    assert_eq!(done, Seeded::default());
    assert_eq!(b.config(), BASE);
    assert_eq!(
        std::fs::read_to_string(record_path(&b.dir)).unwrap(),
        "{ not json"
    );
}

/// 随 core 发的那一份清单，真的沙箱：每个都装上、都停用着；**一个都不编**（不起运行时），
/// 列表照样说得出它们是什么 —— 重启之后也一样
#[tokio::test]
async fn the_shipped_defaults_go_in_turned_off_without_starting_the_sandbox() {
    let engine = Counting::new(real());
    let b = bed_with("real", engine.clone());
    let done = Seeder::shipped().seed(&b.mgr).await;
    let all = tw_gateway::plugin::defaults::ALL;
    let want: Vec<&str> = all.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids(&done.added), want, "{done:?}");
    assert!(done.failed.is_empty(), "{done:?}");
    for (id, src) in all {
        let p = b.entry(id).unwrap();
        assert!(!p.enabled, "{id}");
        assert_eq!(p.sha256, sha(src), "{id}");
        let v = b.plugin(id).await;
        assert_eq!(v["status"], json!({"kind": "disabled"}), "{id}: {v}");
        let m = tw_gateway::plugin::defaults::manifest(id).unwrap();
        assert_eq!(v["name"], m.name.as_str(), "{id}");
        assert!(
            !v["permissions"].as_array().unwrap().is_empty(),
            "{id}: {v}"
        );
        let a = b.gw.runtime().plugins.get(id).unwrap().clone();
        assert!(a.ready().unwrap().dormant(), "{id}");
    }
    assert_eq!(
        b.plugin("reply-language").await["settings"],
        json!({"language": "简体中文"})
    );
    assert_eq!(engine.count(), 0, "seeding or listing started the sandbox");
    assert_eq!(Seeder::shipped().seed(&b.mgr).await, Seeded::default());

    // 重启：显示用的缓存里读得出来，照样不编
    let engine = Counting::new(real());
    let again = b.restart(engine.clone());
    assert_eq!(Seeder::shipped().seed(&again.mgr).await, Seeded::default());
    let v = again.plugin("wsl-paths").await;
    assert_eq!(v["name"], "Convert WSL and Windows paths");
    assert_eq!(v["permissions"], json!(["messages", "reply_tool_calls"]));
    assert_eq!(engine.count(), 0);
}

/// 打开一个默认插件：这时才真的编（起运行时），编出来就能跑
#[tokio::test]
async fn enabling_a_default_compiles_it_and_then_it_runs() {
    let engine = Counting::new(real());
    let b = bed_with("real", engine.clone());
    Seeder::shipped().seed(&b.mgr).await;
    assert_eq!(engine.count(), 0);
    let (st, v) = call(
        &b.app,
        "PUT",
        "/plugins/reply-language",
        Some(json!({"enabled": true, "on_error": "reject",
                    "scope": {"clients": [], "models": [], "upstreams": []},
                    "settings": {"language": "English"}, "base_version": b.version().await})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(engine.count() > 0, "turning it on did not compile it");
    assert_eq!(
        b.plugin("reply-language").await["status"],
        json!({"kind": "ok"})
    );
    let a =
        b.gw.runtime()
            .plugins
            .get("reply-language")
            .unwrap()
            .clone();
    let host = a.ready().unwrap().clone();
    assert!(!host.dormant());
    let out = tokio::task::spawn_blocking(move || {
        host.on_request(
            json!({"format": "anthropic", "model": "claude-sonnet-4-5", "system": "你是助手。"}),
            json!({"client": null, "model": "claude-sonnet-4-5",
                   "requested_model": "claude-sonnet-4-5", "format": "anthropic",
                   "upstream": "anthropic", "settings": {"language": "English"}}),
        )
    })
    .await
    .unwrap();
    match out.result {
        Ok(tw_gateway::plugin::RequestOutcome::Changed(v)) => assert_eq!(
            v["system"],
            "你是助手。\n\nAlways respond in English, unless the user explicitly asks for another \
             language."
        ),
        other => panic!("{other:?}"),
    }
}

/// 一个改得了工具调用的插件，装上时停用着
async fn install_calls(b: &Bed) -> String {
    let src = source(
        json!({"name": "改工具调用", "api": 1, "permissions": ["reply.tool_calls"],
               "settings": {"mode": {"type": "string", "label": "方式", "default": "a"}}}),
        &["onToolCall"],
    );
    let (st, v) = call(
        &b.app,
        "POST",
        "/plugins",
        Some(
            json!({"source": src, "id": "calls", "enabled": false, "on_error": "reject",
                    "scope": {"clients": [], "models": [], "upstreams": []}, "settings": {}}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    src
}

fn cache_file(b: &Bed) -> PathBuf {
    b.dir.join("plugins").join(".manifests.json")
}

/// 显示用的缓存被人改了（藏起了 reply_tool_calls）：列表上是改过的样子，可网页那条路照样
/// 打不开它、改不了它的设置 —— 判断用的是真的编出来的 manifest
#[tokio::test]
async fn a_tampered_manifest_cache_cannot_hide_tool_calls_from_the_confirmation() {
    let b = bed();
    install_calls(&b).await;
    let text = std::fs::read_to_string(cache_file(&b)).unwrap();
    assert!(text.contains("\"reply_tool_calls\""), "{text}");
    std::fs::write(
        cache_file(&b),
        text.replace("\"reply_tool_calls\"", "\"system\""),
    )
    .unwrap();

    let engine = Counting::new(Arc::new(FakeEngine));
    let again = b.restart(engine.clone());
    let v = again.plugin("calls").await;
    assert_eq!(v["permissions"], json!(["system"]), "{v}");
    assert_eq!(engine.count(), 0);
    // 只改出错时怎么办：用不着判断，也就不编
    let (st, v) = call(
        &again.app,
        "PUT",
        "/plugins/calls",
        Some(json!({"enabled": false, "on_error": "skip",
                    "scope": {"clients": [], "models": [], "upstreams": []},
                    "settings": {"mode": "a"}, "base_version": again.version().await})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(engine.count(), 0, "an on_error change started the sandbox");
    for (what, enabled, mode) in [("turning it on", true, "a"), ("a setting", false, "b")] {
        let (st, v) = call(
            &again.app,
            "PUT",
            "/plugins/calls",
            Some(json!({"enabled": enabled, "on_error": "reject",
                        "scope": {"clients": [], "models": [], "upstreams": []},
                        "settings": {"mode": mode}, "base_version": again.version().await})),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{what}: {v}");
        assert_eq!(v["code"], "control.plugin.needs_confirmation", "{what}");
    }
    assert!(
        engine.count() > 0,
        "the decision was not made on a compiled manifest"
    );
    let p = again.entry("calls").unwrap();
    assert!(!p.enabled);
    assert_eq!(p.settings["mode"], serde_yaml_ng::Value::from("a"));
    let (st, v) = call(
        &again.app,
        "PUT",
        "/plugins/calls/confirmed",
        Some(json!({"enabled": true, "on_error": "reject",
                    "scope": {"clients": [], "models": [], "upstreams": []},
                    "settings": {"mode": "b"}, "base_version": again.version().await})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // 编过一遍之后，列表上也是真的那一份了
    assert_eq!(
        again.plugin("calls").await["permissions"],
        json!(["reply_tool_calls"])
    );
}

/// 缓存对不上（版本不对、是别的字节的）：不认，插件只按 id 和状态列出来，也不为此编
#[tokio::test]
async fn a_cache_entry_that_does_not_match_is_ignored() {
    let b = bed();
    let src = install_calls(&b).await;
    let stored: Value =
        serde_json::from_str(&std::fs::read_to_string(cache_file(&b)).unwrap()).unwrap();
    let sha_now = sha(&src);
    assert!(stored["manifests"][&sha_now].is_object(), "{stored}");

    let mut old_version = stored.clone();
    old_version["version"] = json!("0.0.0/old/1");
    let mut other_bytes = stored.clone();
    let entry = other_bytes["manifests"][&sha_now].clone();
    other_bytes["manifests"] = json!({ sha_now.replace(|c: char| c != '0', "0"): entry });
    for (what, file) in [
        ("another version", old_version.to_string()),
        ("other bytes", other_bytes.to_string()),
        ("a broken file", "{ not json".to_string()),
    ] {
        std::fs::write(cache_file(&b), file).unwrap();
        let engine = Counting::new(Arc::new(FakeEngine));
        let again = b.restart(engine.clone());
        let v = again.plugin("calls").await;
        assert_eq!(v["name"], "calls", "{what}: {v}");
        assert_eq!(v["permissions"], json!([]), "{what}: {v}");
        assert_eq!(v["status"], json!({"kind": "disabled"}), "{what}: {v}");
        assert_eq!(engine.count(), 0, "{what}");
    }
}

/// `wsl-paths` 改得了回答里的工具调用：网页那条路打不开它，确认过的那条打得开
#[tokio::test]
async fn wsl_paths_turns_on_only_with_a_confirmation() {
    let b = bed_in("real", false);
    Seeder::shipped().seed(&b.mgr).await;
    // 只是打开：范围和设置都是装上时的那样
    let body = |base: String| {
        json!({"enabled": true, "on_error": "reject",
               "scope": {"clients": [], "models": [], "upstreams": []},
               "settings": {"windows_client": false}, "base_version": base})
    };
    let (st, v) = call(
        &b.app,
        "PUT",
        "/plugins/wsl-paths",
        Some(body(b.version().await)),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["code"], "control.plugin.needs_confirmation");
    assert!(!b.entry("wsl-paths").unwrap().enabled);

    let (st, v) = call(
        &b.app,
        "PUT",
        "/plugins/wsl-paths/confirmed",
        Some(body(b.version().await)),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(b.entry("wsl-paths").unwrap().enabled);
    assert_eq!(b.plugin("wsl-paths").await["status"], json!({"kind": "ok"}));
}
