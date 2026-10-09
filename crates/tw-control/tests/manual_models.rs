//! 手动添加的模型：上游的清单里没有、配置里写进 `models` 的模型。上游页的模型清单说出
//! 哪些是手动添加的、哪些上游自己也列了，`PUT /provider-manual-models` 设整份、清空，
//! 模型列表和试算把它们当作列出的模型；上游不给清单时它们就是全部，启用范围照样管着。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

struct Bed {
    dir: tempfile::TempDir,
    app: axum::Router,
}

impl Bed {
    fn file(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("config.yaml")).unwrap()
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

async fn call(app: &axum::Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
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
    let v = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (st, v)
}

/// 一个假上游：认 `sk-good`，列出两个模型。
async fn upstream() -> std::net::SocketAddr {
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

/// `relay` 那一项后面接 `extra`（缩进四格的字段），整份配置后面接 `tail`
fn config(up: std::net::SocketAddr, key: &str, extra: &str, tail: &str) -> String {
    format!(
        "version: 1
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
clients:
  - name: c
    key: tw-k
providers:
  - name: relay
    base_url: http://{up}
    key: {key}
    protocol: anthropic
{extra}{tail}"
    )
}

/// 向上游问一遍，交回整个清单视图
async fn refreshed(b: &Bed) -> Value {
    let (st, v) = call(
        &b.app,
        "POST",
        "/providers/relay/models/refresh",
        Value::Null,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v
}

/// 不问上游，读 core 记下的清单
async fn current(b: &Bed) -> Value {
    let (st, v) = call(&b.app, "GET", "/providers/relay/models", Value::Null).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v
}

fn ids(view: &Value) -> Vec<String> {
    view["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect()
}

fn row<'a>(view: &'a Value, id: &str) -> &'a Value {
    view["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .unwrap_or_else(|| panic!("no row {id}: {view}"))
}

async fn dry_run(b: &Bed, model: &str) -> Value {
    let (st, v) = call(
        &b.app,
        "POST",
        "/dryrun",
        json!({ "model": model, "client": "c" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v
}

async fn known(b: &Bed) -> Vec<String> {
    let (st, v) = call(&b.app, "GET", "/models", Value::Null).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v.as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect()
}

async fn set(b: &Bed, body: Value) -> (StatusCode, Value) {
    call(&b.app, "PUT", "/provider-manual-models", body).await
}

#[tokio::test]
async fn each_row_says_whether_it_was_added_by_hand_and_whether_the_upstream_lists_it() {
    let up = upstream().await;
    let b = bed(&config(
        up,
        "sk-good",
        "    models: [gpt-6-luna, claude-haiku-4-5]\n",
        "aliases:\n  luna: gpt-6-luna\n",
    ));
    let v = refreshed(&b).await;
    assert_eq!(v["source"], "discovered", "{v}");
    // 上游列出的在前，手动添加的接在后面；两边都有的只算一次
    assert_eq!(
        ids(&v),
        ["claude-sonnet-4-5", "claude-haiku-4-5", "gpt-6-luna"]
    );
    let flags = |id: &str| {
        let r = row(&v, id);
        (
            r["manual"].clone(),
            r["listed"].clone(),
            r["enabled"].clone(),
        )
    };
    assert_eq!(
        flags("claude-sonnet-4-5"),
        (json!(false), json!(true), json!(true))
    );
    assert_eq!(
        flags("claude-haiku-4-5"),
        (json!(true), json!(true), json!(true))
    );
    assert_eq!(
        flags("gpt-6-luna"),
        (json!(true), json!(false), json!(true))
    );
    assert_eq!(row(&v, "gpt-6-luna")["aliases"], json!(["luna"]));

    // 模型列表、概览的个数、试算都把它当作这家的模型
    let (_, ov) = call(&b.app, "GET", "/overview", Value::Null).await;
    assert_eq!(ov["providers"][0]["model_count"], 3, "{ov}");
    assert_eq!(
        ov["providers"][0]["models"],
        json!(["gpt-6-luna", "claude-haiku-4-5"])
    );
    let models = known(&b).await;
    assert!(models.iter().any(|m| m == "gpt-6-luna"), "{models:?}");
    assert!(models.iter().any(|m| m == "luna"), "{models:?}");
    let d = dry_run(&b, "gpt-6-luna").await;
    assert_eq!(d["outcome"], "route", "{d}");
    assert_eq!(d["candidates"], json!(["relay"]));
    let d = dry_run(&b, "luna").await;
    assert_eq!(d["outcome"], "route", "{d}");
    assert_eq!(d["candidates"], json!(["relay"]));
}

#[tokio::test]
async fn the_endpoint_sets_and_clears_the_models_added_by_hand() {
    let up = upstream().await;
    let b = bed(&config(up, "sk-good", "", ""));
    let before = b.file();
    let v = refreshed(&b).await;
    assert!(
        v["models"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["manual"] == false),
        "{v}"
    );
    assert_eq!(dry_run(&b, "gpt-6-luna").await["outcome"], "unavailable");

    // 去掉首尾空白；上游也列了的可以加
    let (st, w) = set(
        &b,
        json!({ "provider": "relay", "models": [" gpt-6-luna ", "claude-haiku-4-5"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{w}");
    assert!(w["version"].as_str().is_some(), "{w}");
    let cfg = tw_config::try_parse(&b.file()).unwrap();
    assert_eq!(
        cfg.providers[0].models,
        ["gpt-6-luna", "claude-haiku-4-5"],
        "{}",
        b.file()
    );
    // 马上就在清单里，不用重新向上游问
    let v = current(&b).await;
    assert_eq!(v["source"], "discovered", "{v}");
    let luna = row(&v, "gpt-6-luna");
    assert_eq!(
        (&luna["manual"], &luna["listed"]),
        (&json!(true), &json!(false))
    );
    let d = dry_run(&b, "gpt-6-luna").await;
    assert_eq!(d["outcome"], "route", "{d}");
    assert!(known(&b).await.iter().any(|m| m == "gpt-6-luna"));

    // 换成一份更短的：拿掉的那个不在了
    let (st, w) = set(&b, json!({ "provider": "relay", "models": ["gpt-6-luna"] })).await;
    assert_eq!(st, StatusCode::OK, "{w}");
    let v = current(&b).await;
    assert_eq!(row(&v, "claude-haiku-4-5")["manual"], false);

    // 清空：文件回到原样，回到只认上游的清单
    let (st, w) = set(&b, json!({ "provider": "relay", "models": [] })).await;
    assert_eq!(st, StatusCode::OK, "{w}");
    assert_eq!(b.file(), before);
    let v = current(&b).await;
    assert_eq!(ids(&v), ["claude-sonnet-4-5", "claude-haiku-4-5"]);
    let d = dry_run(&b, "gpt-6-luna").await;
    assert_eq!(d["outcome"], "unavailable", "{d}");
    assert!(!known(&b).await.iter().any(|m| m == "gpt-6-luna"));

    // 基于旧版本的保存被拒
    let (st, _) = set(
        &b,
        json!({ "provider": "relay", "models": ["m"], "base_version": "nope" }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(b.file(), before);
}

#[tokio::test]
async fn a_bad_model_added_by_hand_is_refused_in_the_configurations_words() {
    let up = upstream().await;
    let b = bed(&config(up, "sk-good", "", ""));
    let before = b.file();
    let long = "m".repeat(257);
    for (models, status, code) in [
        (
            json!([""]),
            StatusCode::BAD_REQUEST,
            "config.manual_model_blank",
        ),
        (
            json!(["  "]),
            StatusCode::BAD_REQUEST,
            "config.manual_model_blank",
        ),
        (
            json!(["gpt-*"]),
            StatusCode::BAD_REQUEST,
            "config.manual_model_wildcard",
        ),
        // 去掉空白之后重复
        (
            json!(["gpt-6", " gpt-6 "]),
            StatusCode::BAD_REQUEST,
            "config.manual_model_duplicate",
        ),
        (
            json!([long]),
            StatusCode::BAD_REQUEST,
            "config.manual_model_too_long",
        ),
        (
            json!(["a\nb"]),
            StatusCode::BAD_REQUEST,
            "config.edit.multiline",
        ),
    ] {
        let (st, v) = set(&b, json!({ "provider": "relay", "models": models })).await;
        assert_eq!(st, status, "{models}: {v}");
        assert_eq!(v["code"], code, "{models}: {v}");
        assert_eq!(b.file(), before);
    }
    let (st, v) = set(&b, json!({ "provider": "ghost", "models": ["m"] })).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(v["code"], "config.edit.not_found", "{v}");
    assert_eq!(b.file(), before);
}

/// 上游不给清单（这里是密钥被拒）：手动添加的就是全部，照样标着；启用范围照样管着它们
#[tokio::test]
async fn without_a_list_from_the_upstream_the_models_added_by_hand_are_the_whole_list() {
    let up = upstream().await;
    let b = bed(&config(
        up,
        "sk-bad",
        "    models: [手写的模型, gpt-6-luna]\n    models_only: [gpt-*]\n",
        "",
    ));
    let v = refreshed(&b).await;
    assert_eq!(v["source"], "manual", "{v}");
    assert_eq!(v["status"], "failed", "{v}");
    assert_eq!(ids(&v), ["手写的模型", "gpt-6-luna"]);
    for id in ["手写的模型", "gpt-6-luna"] {
        let r = row(&v, id);
        assert_eq!(
            (&r["manual"], &r["listed"]),
            (&json!(true), &json!(false)),
            "{r}"
        );
    }
    assert_eq!(row(&v, "手写的模型")["enabled"], false);
    assert_eq!(row(&v, "gpt-6-luna")["enabled"], true);

    let d = dry_run(&b, "gpt-6-luna").await;
    assert_eq!(d["outcome"], "route", "{d}");
    // 范围外的手动模型不列、不路由
    assert!(!known(&b).await.iter().any(|m| m == "手写的模型"));
    let d = dry_run(&b, "手写的模型").await;
    assert_ne!(d["outcome"], "route", "{d}");
}
