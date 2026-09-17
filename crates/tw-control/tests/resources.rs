//! 上游和代理按资源增删改、检测。
//!
//! 这一组接口写的是用户唯一的那份配置文件，所以断言都落在**文件本身**上：
//! 值有没有按原样写进去、没改的地方和注释是不是一个字节没动、引用有没有
//! 跟着改名、删不掉的时候有没有说清是谁在用。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

const BASE: &str = "version: 1
clients:
  - name: c
    key: tw-k
# 这家是官方的，别动
providers:
  - name: 官方
    base_url: https://api.anthropic.com  # 直连
    key: sk-official
groups:
  - name: pool
    type: fallback
    providers: [官方]
routes:
  - name: 默认
    rules:
      - name: 兜底
        to: pool
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
}

fn bed(yaml: &str) -> Bed {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, yaml).unwrap();
    let cfg = tw_config::try_parse(yaml).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let state = ControlState {
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        gateway_addr: None,
        store: None,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        // **测试里绝不能碰开发者自己的配置**
        home: d.path().join("home"),
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
) -> (StatusCode, String) {
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
    (st, String::from_utf8_lossy(&b).to_string())
}

fn json(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap_or_else(|e| panic!("{e}: {s}"))
}

fn relay(name: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "base_url": "https://relay.example",
        "key": { "mode": "set", "value": "sk-relay#1: x" },
        "protocol": "anthropic",
    })
}

// ─────────────────────────────────────────────────────────── 上游

#[tokio::test]
async fn a_new_upstream_lands_with_its_key_intact_and_every_comment_kept() {
    // 以前界面拼字符串：`#` 之后会被读成注释，`: ` 会把值切成一个映射
    let b = bed(BASE);
    let (st, body) = call(
        &b.app,
        "POST",
        "/providers",
        serde_json::json!({ "provider": relay("relay") }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let cfg = b.parsed();
    let p = cfg.providers.iter().find(|p| p.name == "relay").unwrap();
    assert_eq!(
        p.key.as_ref().map(tw_config::Secret::raw),
        Some("sk-relay#1: x")
    );
    assert_eq!(p.protocol, Some(tw_config::Protocol::Anthropic));
    let file = b.file();
    assert!(file.contains("# 这家是官方的，别动"), "{file}");
    assert!(
        file.contains("base_url: https://api.anthropic.com  # 直连"),
        "{file}"
    );
}

#[tokio::test]
async fn editing_without_a_key_keeps_the_key_and_touches_only_what_changed() {
    let b = bed(BASE);
    let (st, body) = call(
        &b.app,
        "PUT",
        "/providers/官方",
        serde_json::json!({ "provider": {
            "name": "官方",
            "proxy": "system",
            "billing": "subscription",
        }}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let file = b.file();
    // 没交地址和凭据 = 保持原样
    assert!(file.contains("key: sk-official"), "{file}");
    assert!(
        file.contains("base_url: https://api.anthropic.com  # 直连"),
        "{file}"
    );
    let p = &b.parsed().providers[0];
    assert_eq!(p.proxy, "system");
    assert_eq!(p.billing, Some(tw_config::Billing::Subscription));
}

#[tokio::test]
async fn renaming_an_upstream_moves_its_references_in_the_same_version() {
    let b = bed(BASE);
    let (st, body) = call(
        &b.app,
        "PUT",
        "/providers/官方",
        serde_json::json!({ "provider": { "name": "anthropic" } }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let cfg = b.parsed();
    assert_eq!(cfg.providers[0].name, "anthropic");
    assert_eq!(cfg.groups[0].providers, ["anthropic"]);
    // 一次保存一个版本：历史里只多了改之前的那一份
    let history = tw_config::history::list(&b.dir.path().join("config.yaml")).unwrap();
    assert_eq!(
        history
            .iter()
            .filter(|v| v.origin == tw_config::history::Origin::Ui)
            .count(),
        2
    );
}

#[tokio::test]
async fn an_upstream_still_in_use_is_not_deleted_and_the_answer_names_who_uses_it() {
    let b = bed(BASE);
    let (st, body) = call(&b.app, "DELETE", "/providers/官方", serde_json::json!({})).await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("策略组「pool」"), "{body}");
    assert_eq!(b.parsed().providers.len(), 1);

    // 没人用的那家删得掉
    call(
        &b.app,
        "POST",
        "/providers",
        serde_json::json!({ "provider": relay("relay") }),
    )
    .await;
    let (st, body) = call(&b.app, "DELETE", "/providers/relay", serde_json::json!({})).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(b.parsed().providers.iter().all(|p| p.name != "relay"));
}

#[tokio::test]
async fn a_duplicate_name_and_a_stale_version_are_both_conflicts() {
    let b = bed(BASE);
    let (st, body) = call(
        &b.app,
        "POST",
        "/providers",
        serde_json::json!({ "provider": relay("官方") }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("已经有叫「官方」的上游"), "{body}");

    let (st, body) = call(
        &b.app,
        "POST",
        "/providers",
        serde_json::json!({ "provider": relay("relay"), "base_version": "sha256:不是这一版" }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
}

#[tokio::test]
async fn the_overview_says_where_a_credential_comes_from_and_never_the_value() {
    let b = bed(BASE);
    let (st, body) = call(
        &b.app,
        "POST",
        "/providers",
        serde_json::json!({ "provider": {
            "name": "env",
            "base_url": "https://relay.example",
            "key": { "mode": "set", "value": "${RELAY_KEY}" },
            "headers": [
                { "name": "X-Relay-Tenant", "value": "tenant-verysecretvalue" },
                { "name": "anthropic-version", "value": "2023-06-01" },
            ],
        }}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (_, body) = call(&b.app, "GET", "/overview", serde_json::Value::Null).await;
    assert!(!body.contains("sk-official"), "{body}");
    assert!(!body.contains("tenant-verysecretvalue"), "{body}");
    let v = json(&body);
    let providers = v["providers"].as_array().unwrap();
    let official = &providers[0];
    assert!(
        official["key"]["display"].as_str().unwrap().contains('…'),
        "{body}"
    );
    assert_eq!(official["auth_header"], "x-api-key");
    assert_eq!(official["protocol"], "anthropic");
    assert_eq!(official["protocol_explicit"], false);
    assert_eq!(official["trust"], "official");
    assert_eq!(official["references"][0]["kind"], "group");
    let env = providers.iter().find(|p| p["name"] == "env").unwrap();
    assert_eq!(env["key"]["env"], "RELAY_KEY");
    assert_eq!(env["headers"][0]["name"], "X-Relay-Tenant");
    assert_eq!(env["headers"][0]["masked"], true);
    assert_eq!(env["headers"][1]["value"], "2023-06-01");
    assert_eq!(env["headers"][1]["masked"], false);
}

#[tokio::test]
async fn a_header_saved_without_a_value_keeps_the_stored_one() {
    // 视图里的值是打过码的，界面不回填 —— 不改的那一行就不带值
    let b = bed(BASE);
    call(
        &b.app,
        "POST",
        "/providers",
        serde_json::json!({ "provider": {
            "name": "relay",
            "base_url": "https://relay.example",
            "headers": [{ "name": "X-Relay-Token", "value": "rt-1" }],
        }}),
    )
    .await;
    let (st, body) = call(
        &b.app,
        "PUT",
        "/providers/relay",
        serde_json::json!({ "provider": {
            "name": "relay",
            "headers": [
                { "name": "X-Relay-Token" },
                { "name": "X-Extra", "value": "e" },
            ],
        }}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let cfg = b.parsed();
    let p = cfg.providers.iter().find(|p| p.name == "relay").unwrap();
    assert_eq!(p.headers.get("x-relay-token").unwrap().value.raw(), "rt-1");
    assert_eq!(p.headers.get("X-Extra").unwrap().value.raw(), "e");
}

#[tokio::test]
async fn a_claude_subscription_login_cannot_be_saved_as_oauth() {
    let b = bed(BASE);
    let (st, body) = call(
        &b.app,
        "POST",
        "/providers",
        serde_json::json!({ "provider": {
            "name": "max",
            "base_url": "https://api.anthropic.com",
            "oauth": { "mode": "set", "refresh": "rt", "endpoint": "https://console.anthropic.com/v1/oauth/token" },
        }}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("Claude 订阅账号"), "{body}");
    assert_eq!(b.parsed().providers.len(), 1);
}

// ─────────────────────────────────────────────────────────── 代理

fn corp(auth: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "name": "corp", "kind": "http", "addr": "10.0.0.1:8080", "auth": auth })
}

#[tokio::test]
async fn a_proxy_password_is_written_intact_kept_on_edit_and_never_shown() {
    let b = bed(BASE);
    let (st, body) = call(
        &b.app,
        "POST",
        "/proxies",
        serde_json::json!({ "proxy": corp(serde_json::json!({ "mode": "set", "user": "svc", "pass": "p@ss #1: x" })) }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    // 改地址、不动认证
    let (st, body) = call(
        &b.app,
        "PUT",
        "/proxies/corp",
        serde_json::json!({ "proxy": {
            "name": "corp", "kind": "http", "addr": "10.0.0.2:8080", "auth": { "mode": "keep" }
        }}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let cfg = b.parsed();
    let auth = cfg.proxies[0].auth.as_ref().expect("没动认证，认证不该丢");
    assert_eq!(auth.user, "svc");
    assert_eq!(auth.pass.raw(), "p@ss #1: x");
    assert_eq!(cfg.proxies[0].addr, "10.0.0.2:8080");

    let (_, body) = call(&b.app, "GET", "/overview", serde_json::Value::Null).await;
    assert!(!body.contains("p@ss"), "{body}");
    assert!(!body.contains("\"svc\""), "用户名也是凭据的一半：{body}");
    assert_eq!(json(&body)["proxies"][0]["has_auth"], true);
}

#[tokio::test]
async fn renaming_a_proxy_moves_the_upstreams_that_use_it_and_a_used_proxy_is_not_deleted() {
    let b = bed(BASE);
    call(
        &b.app,
        "POST",
        "/proxies",
        serde_json::json!({ "proxy": corp(serde_json::json!({ "mode": "none" })) }),
    )
    .await;
    let mut r = relay("relay");
    r["proxy"] = "corp".into();
    let (st, body) = call(
        &b.app,
        "POST",
        "/providers",
        serde_json::json!({ "provider": r }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");

    let (st, body) = call(&b.app, "DELETE", "/proxies/corp", serde_json::json!({})).await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("「relay」"), "{body}");

    let (st, body) = call(
        &b.app,
        "PUT",
        "/proxies/corp",
        serde_json::json!({ "proxy": {
            "name": "corp-http", "kind": "http", "addr": "10.0.0.1:8080", "auth": { "mode": "keep" }
        }}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let cfg = b.parsed();
    assert_eq!(
        cfg.providers
            .iter()
            .find(|p| p.name == "relay")
            .unwrap()
            .proxy,
        "corp-http"
    );
    let (_, body) = call(&b.app, "GET", "/overview", serde_json::Value::Null).await;
    assert_eq!(
        json(&body)["proxies"][0]["used_by"],
        serde_json::json!(["relay"])
    );
}

// ─────────────────────────────────────────────────────────── 检测

/// 一个假的上游：`/v1/models` 只认 `x-api-key: sk-good`。
async fn fake_upstream() -> std::net::SocketAddr {
    let app = axum::Router::new().route(
        "/v1/models",
        axum::routing::get(|h: axum::http::HeaderMap| async move {
            if h.get("x-api-key").and_then(|v| v.to_str().ok()) != Some("sk-good") {
                return (StatusCode::UNAUTHORIZED, "{}".to_string());
            }
            (
                StatusCode::OK,
                r#"{"data":[{"id":"claude-sonnet-4-5"},{"id":"claude-haiku-4-5"}]}"#.to_string(),
            )
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

#[tokio::test]
async fn testing_an_unsaved_upstream_uses_the_protocol_and_lists_the_models() {
    let b = bed(BASE);
    let up = fake_upstream().await;
    let (st, body) = call(
        &b.app,
        "POST",
        "/providers/test",
        serde_json::json!({ "provider": {
            "name": "relay",
            "base_url": format!("http://{up}"),
            "key": { "mode": "set", "value": "sk-good" },
            "protocol": "anthropic",
        }}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let v = json(&body);
    assert_eq!(v["ok"], true, "{body}");
    assert_eq!(v["protocol"], "anthropic");
    assert_eq!(v["models"]["kind"], "listed");
    assert_eq!(v["models"]["models"].as_array().unwrap().len(), 2);
    // 什么都没存
    assert_eq!(b.parsed().providers.len(), 1);
}

#[tokio::test]
async fn testing_an_edited_upstream_without_a_new_key_uses_the_stored_one() {
    let up = fake_upstream().await;
    let b = bed(&BASE
        .replace("https://api.anthropic.com  # 直连", &format!("http://{up}"))
        .replace("sk-official", "sk-good"));
    let (_, body) = call(
        &b.app,
        "POST",
        "/providers/test",
        serde_json::json!({ "provider": { "name": "官方" }, "current": "官方" }),
    )
    .await;
    assert_eq!(json(&body)["ok"], true, "{body}");
}

#[tokio::test]
async fn an_unsaved_oauth_credential_is_not_refreshed_just_to_test_it() {
    // 拿 refresh token 去换，服务端可能当场作废旧的那把
    let b = bed(BASE);
    let (_, body) = call(
        &b.app,
        "POST",
        "/providers/test",
        serde_json::json!({ "provider": {
            "name": "corp-llm",
            "base_url": "https://llm.corp.example",
            "oauth": { "mode": "set", "refresh": "rt", "endpoint": "https://auth.example/token" },
        }}),
    )
    .await;
    let v = json(&body);
    assert_eq!(v["ok"], false);
    assert!(
        v["error"].as_str().unwrap().contains("保存后才能检测"),
        "{body}"
    );
}

/// 一个假的 HTTP 代理：CONNECT 只认 `svc:good`。
async fn fake_connect_proxy() -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut b = [0u8; 1];
                while !buf.ends_with(b"\r\n\r\n") {
                    if s.read_exact(&mut b).await.is_err() {
                        return;
                    }
                    buf.push(b[0]);
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                // base64("svc:good")
                let reply = if head.contains("Proxy-Authorization: Basic c3ZjOmdvb2Q=") {
                    "HTTP/1.1 200 Connection established\r\n\r\n"
                } else {
                    "HTTP/1.1 407 Proxy Authentication Required\r\n\r\n"
                };
                let _ = s.write_all(reply.as_bytes()).await;
            });
        }
    });
    a
}

#[tokio::test]
async fn testing_a_proxy_checks_its_credentials_including_the_stored_ones() {
    let px = fake_connect_proxy().await;
    let b = bed(BASE);
    let with = |auth: serde_json::Value| serde_json::json!({ "name": "corp", "kind": "http", "addr": px.to_string(), "auth": auth });
    // 密码错了：只量 TCP 的话这里会是「通」
    let (_, body) = call(
        &b.app,
        "POST",
        "/proxies/test",
        serde_json::json!({ "proxy": with(serde_json::json!({ "mode": "set", "user": "svc", "pass": "bad" })) }),
    )
    .await;
    let v = json(&body);
    assert_eq!(v["ok"], false, "{body}");
    assert!(v["error"].as_str().unwrap().contains("407"), "{body}");

    // 存下正确的，再用「保持原样」检测 —— 用的是存着的那份
    call(
        &b.app,
        "POST",
        "/proxies",
        serde_json::json!({ "proxy": with(serde_json::json!({ "mode": "set", "user": "svc", "pass": "good" })) }),
    )
    .await;
    let (_, body) = call(
        &b.app,
        "POST",
        "/proxies/test",
        serde_json::json!({ "proxy": with(serde_json::json!({ "mode": "keep" })), "current": "corp" }),
    )
    .await;
    assert_eq!(json(&body)["ok"], true, "{body}");
}
