//! 网关密钥按资源增删改、更换，以及「哪一把是默认的」。
//!
//! 断言落在**配置文件本身**上：写进去的是不是那个值、改名有没有带着引用一起改、
//! 删不掉的时候有没有说清为什么。密钥的值由 core 生成，所以断言的是它的形状
//! （`tw-` 开头、和原来的不一样），不是一个写死的串。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

const BASE: &str = "version: 1
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
# 这把是默认的，别动
clients:
  - name: default
    key: tw-aaaa
  - name: codex
    key: tw-bbbb
    client: codex
providers:
  - name: 官方
    base_url: https://api.anthropic.com
    key: sk-official
";

struct Bed {
    dir: tempfile::TempDir,
    app: axum::Router,
}

impl Bed {
    fn file(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("config.yaml")).unwrap()
    }
    fn parsed(&self) -> tw_config::Config {
        tw_config::try_parse(&self.file()).unwrap()
    }
    fn key(&self, name: &str) -> tw_config::Client {
        self.parsed()
            .clients
            .into_iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("配置里没有密钥「{name}」"))
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

// ─────────────────────────────────────────────────────────── 读

#[tokio::test]
async fn the_list_shows_the_value_and_says_which_one_is_the_default() {
    let b = bed(BASE);
    let (st, v) = call(&b.app, "GET", "/keys", serde_json::Value::Null).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v[0]["name"], "default");
    // 密钥页把值原样显示出来：这把钥匙的用处就是被复制进客户端
    assert_eq!(v[0]["key"], "tw-aaaa");
    assert_eq!(v[0]["default"], true);
    // 没有 default_key 时，名字叫 default 的那把就是默认的
    assert_eq!(v[1]["name"], "codex");
    assert!(v[1]["default"].is_null() || v[1]["default"] == false);
    assert_eq!(v[1]["client"], "codex");

    // 「复制」按名字取此刻配置里的值
    let (st, v) = call(&b.app, "GET", "/keys/codex/value", serde_json::Value::Null).await;
    assert_eq!((st, v["key"].as_str()), (StatusCode::OK, Some("tw-bbbb")));

    // 概览到处都在读，它那份网关密钥是脱敏的
    let (_, ov) = call(&b.app, "GET", "/overview", serde_json::Value::Null).await;
    assert_ne!(ov["clients"][0]["key"], "tw-aaaa");
}

#[tokio::test]
async fn default_key_names_one_even_when_it_is_not_called_default() {
    let b = bed("version: 1
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
default_key: codex
clients:
  - name: default
    key: tw-aaaa
  - name: codex
    key: tw-bbbb
");
    let (_, v) = call(&b.app, "GET", "/keys", serde_json::Value::Null).await;
    assert!(v[0]["default"].is_null() || v[0]["default"] == false);
    assert_eq!(v[1]["default"], true);
}

// ─────────────────────────────────────────────────────────── 写

#[tokio::test]
async fn a_new_key_gets_its_value_from_core_and_keeps_the_comments() {
    let b = bed(BASE);
    let (st, v) = call(
        &b.app,
        "POST",
        "/keys",
        serde_json::json!({ "key": { "name": "试用", "max_concurrent": 2 } }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let made = b.key("试用");
    // 字母表和长度是安全决定，界面不该有第二份
    assert!(made.key.starts_with("tw-"), "{}", made.key);
    assert_eq!(made.max_concurrent, Some(2));
    assert!(b.file().contains("# 这把是默认的，别动"), "{}", b.file());
}

#[tokio::test]
async fn renaming_a_key_carries_the_rules_and_the_default_with_it() {
    let b = bed("version: 1
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
default_key: codex
clients:
  - name: codex
    key: tw-bbbb
providers:
  - name: 官方
    base_url: https://api.anthropic.com
    key: sk-official
routes:
  - name: 默认
    rules:
      - name: 只给 codex
        when: { client: codex }
        to: 官方
      - name: 兜底
        to: 官方
");
    let (st, v) = call(
        &b.app,
        "PUT",
        "/keys/codex",
        serde_json::json!({ "key": { "name": "codex-cli" } }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let cfg = b.parsed();
    // 规则里的 client 是精确匹配：不跟着改的话，这条规则从此一次也不会命中
    assert_eq!(
        cfg.routes[0].rules[0].when.client.as_deref(),
        Some("codex-cli")
    );
    assert_eq!(cfg.default_key.as_deref(), Some("codex-cli"));
    // 值没被换掉：改名不是换钥匙
    assert_eq!(b.key("codex-cli").key, "tw-bbbb");
}

#[tokio::test]
async fn the_default_key_can_be_neither_deleted_nor_disabled() {
    let b = bed(BASE);
    let (st, v) = call(&b.app, "DELETE", "/keys/default", serde_json::Value::Null).await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert!(v.to_string().contains("default key"), "{v}");

    let (st, v) = call(
        &b.app,
        "PUT",
        "/keys/default",
        serde_json::json!({ "key": { "name": "default", "disabled": true } }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    // 两条都没落盘
    assert!(!b.key("default").disabled);
    assert_eq!(b.parsed().clients.len(), 2);
}

#[tokio::test]
async fn a_key_a_rule_still_points_at_cannot_be_deleted() {
    let b = bed("version: 1
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
clients:
  - name: default
    key: tw-aaaa
  - name: codex
    key: tw-bbbb
providers:
  - name: 官方
    base_url: https://api.anthropic.com
    key: sk-official
routes:
  - name: 默认
    rules:
      - name: 只给 codex
        when: { client: codex }
        to: 官方
      - name: 兜底
        to: 官方
");
    let (st, v) = call(&b.app, "DELETE", "/keys/codex", serde_json::Value::Null).await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert!(v.to_string().contains("只给 codex"), "要说清是谁在用：{v}");
}

#[tokio::test]
async fn an_unused_key_is_deleted_and_the_rest_of_the_file_is_untouched() {
    let b = bed(BASE);
    let (st, v) = call(&b.app, "DELETE", "/keys/codex", serde_json::Value::Null).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(b.parsed().clients.len(), 1);
    assert!(b.file().contains("# 这把是默认的，别动"));
}

#[tokio::test]
async fn the_default_can_be_moved_to_another_key() {
    let b = bed(BASE);
    let (st, v) = call(
        &b.app,
        "PUT",
        "/default_key",
        serde_json::json!({ "name": "codex" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(b.parsed().default_key.as_deref(), Some("codex"));

    // 换回叫 default 的那把时不写进文件：默认值不写
    let (st, _) = call(
        &b.app,
        "PUT",
        "/default_key",
        serde_json::json!({ "name": "default" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(b.parsed().default_key, None);
    assert!(
        b.parsed()
            .default_client()
            .is_some_and(|c| c.name == "default")
    );
}

// ─────────────────────────────────────────────────────────── 更换

#[tokio::test]
async fn rotating_gives_a_new_value_once_and_leaves_everything_else_alone() {
    let b = bed(BASE);
    let before = b.key("codex").key;
    let (st, v) = call(&b.app, "POST", "/keys/codex/rotate", serde_json::json!({})).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let fresh = v["key"].as_str().unwrap();
    assert!(fresh.starts_with("tw-"), "{fresh}");
    assert_ne!(fresh, before);
    assert_eq!(b.key("codex").key, fresh);
    // 同步进客户端的配置是桌面端的事，这里不说
    assert!(v.get("synced").is_none(), "{v}");
    // 别的东西一个没动
    assert_eq!(b.key("default").key, "tw-aaaa");
    assert_eq!(b.key("codex").client.as_deref(), Some("codex"));
}

#[tokio::test]
async fn rotating_a_key_that_is_not_there_is_a_404() {
    let b = bed(BASE);
    let (st, _) = call(&b.app, "POST", "/keys/nope/rotate", serde_json::json!({})).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

// ─────────────────────────────────────────────────────────── 并发写

#[tokio::test]
async fn a_stale_base_version_is_refused() {
    let b = bed(BASE);
    let (st, v) = call(
        &b.app,
        "POST",
        "/keys",
        serde_json::json!({ "key": { "name": "x" }, "base_version": "blake3:0000" }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(b.parsed().clients.len(), 2);
}
