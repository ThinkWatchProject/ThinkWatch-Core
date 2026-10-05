//! 手写的模型规格：上游页的模型清单说出每个数是手写的还是价目表的，`PUT
//! /provider-model-spec` 设一项、删一项，别名跟着服务它的模型走，改名和删上游时
//! 规格跟着那一家走。

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

/// 一个假上游：列出三个模型，其中一个价目表不认识。
async fn upstream() -> std::net::SocketAddr {
    let app = axum::Router::new().route(
        "/v1/models",
        axum::routing::get(|| async {
            r#"{"data":[{"id":"claude-sonnet-4-5"},{"id":"claude-haiku-4-5"},{"id":"中转自有模型"}]}"#
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

/// `relay` 那一项后面接 `extra`（缩进四格的字段），整份配置后面接 `tail`
fn config(up: std::net::SocketAddr, extra: &str, tail: &str) -> String {
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
    key: sk-good
    protocol: anthropic
{extra}{tail}"
    )
}

/// 问一遍清单，交回 `id` → 那一行
async fn rows(b: &Bed) -> Vec<Value> {
    let (st, v) = call(
        &b.app,
        "POST",
        "/providers/relay/models/refresh",
        Value::Null,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v["models"].as_array().unwrap().clone()
}

fn row<'a>(rows: &'a [Value], id: &str) -> &'a Value {
    rows.iter().find(|m| m["id"] == id).unwrap()
}

#[tokio::test]
async fn each_row_says_whether_its_numbers_were_written_by_hand() {
    let up = upstream().await;
    let b = bed(&config(
        up,
        "    model_specs:
      claude-sonnet-4-5: { max_output_tokens: 8000 }
      中转自有模型: { context_window: 32000 }
",
        "",
    ));
    let rows = rows(&b).await;

    let sonnet = row(&rows, "claude-sonnet-4-5");
    assert_eq!(sonnet["context_window"], 200_000, "{sonnet}");
    assert_eq!(sonnet["context_window_source"], "price_table");
    assert_eq!(sonnet["max_output_tokens"], 8_000);
    assert_eq!(sonnet["max_output_tokens_source"], "manual");

    let own = row(&rows, "中转自有模型");
    assert_eq!(own["context_window"], 32_000, "{own}");
    assert_eq!(own["context_window_source"], "manual");
    // 不知道就整个不出现，来源也没有
    assert!(own.get("max_output_tokens").is_none(), "{own}");
    assert!(own.get("max_output_tokens_source").is_none(), "{own}");

    let haiku = row(&rows, "claude-haiku-4-5");
    assert_eq!(haiku["context_window_source"], "price_table", "{haiku}");
    assert_eq!(haiku["max_output_tokens_source"], "price_table", "{haiku}");
}

#[tokio::test]
async fn a_spec_is_set_changed_and_cleared_through_the_endpoint() {
    let up = upstream().await;
    let b = bed(&config(up, "", ""));
    let before = b.file();
    let set = |cw: Value, out: Value| {
        json!({
            "provider": "relay",
            "model": " 中转自有模型 ",
            "context_window": cw,
            "max_output_tokens": out,
        })
    };

    let (st, v) = call(
        &b.app,
        "PUT",
        "/provider-model-spec",
        set(json!(64000), Value::Null),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["version"].as_str().is_some(), "{v}");
    let cfg = tw_config::try_parse(&b.file()).unwrap();
    assert_eq!(
        cfg.providers[0].model_specs["中转自有模型"],
        tw_config::ModelSpec {
            context_window: Some(64_000),
            max_output_tokens: None,
        },
        "模型 ID 去掉首尾空白：{}",
        b.file()
    );
    let own = row(&rows(&b).await, "中转自有模型").clone();
    assert_eq!(own["context_window"], 64_000);
    assert_eq!(own["context_window_source"], "manual");

    // 改：输出上限也写上
    let (st, v) = call(
        &b.app,
        "PUT",
        "/provider-model-spec",
        set(json!(64000), json!(4096)),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let own = row(&rows(&b).await, "中转自有模型").clone();
    assert_eq!(own["max_output_tokens"], 4_096);
    assert_eq!(own["max_output_tokens_source"], "manual");

    // 两项都空：删掉，回到价目表（它不认识这个模型），文件回到原样
    let (st, v) = call(
        &b.app,
        "PUT",
        "/provider-model-spec",
        set(Value::Null, Value::Null),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(b.file(), before);
    let own = row(&rows(&b).await, "中转自有模型").clone();
    assert!(own.get("context_window").is_none(), "{own}");
    assert!(own.get("context_window_source").is_none(), "{own}");
}

#[tokio::test]
async fn a_bad_spec_is_refused_in_the_configurations_words() {
    let up = upstream().await;
    let b = bed(&config(up, "", ""));
    let before = b.file();
    for (body, status, code) in [
        (
            json!({ "provider": "relay", "model": "m", "context_window": 0 }),
            StatusCode::BAD_REQUEST,
            "config.model_spec_zero",
        ),
        (
            json!({ "provider": "relay", "model": "m", "max_output_tokens": 0 }),
            StatusCode::BAD_REQUEST,
            "config.model_spec_zero",
        ),
        (
            json!({ "provider": "relay", "model": "glm-*", "context_window": 1000 }),
            StatusCode::BAD_REQUEST,
            "config.model_spec_wildcard",
        ),
        (
            json!({ "provider": "relay", "model": "  ", "context_window": 1000 }),
            StatusCode::BAD_REQUEST,
            "config.model_spec_blank_model",
        ),
        // 删一项也要说清楚是哪个模型
        (
            json!({ "provider": "relay", "model": "" }),
            StatusCode::BAD_REQUEST,
            "config.model_spec_blank_model",
        ),
        (
            json!({ "provider": "ghost", "model": "m", "context_window": 1000 }),
            StatusCode::NOT_FOUND,
            "config.edit.not_found",
        ),
    ] {
        let (st, v) = call(&b.app, "PUT", "/provider-model-spec", body.clone()).await;
        assert_eq!(st, status, "{body}: {v}");
        assert_eq!(v["code"], code, "{body}: {v}");
    }
    // 负数、写错字段名：请求体本身不对
    let (st, _) = call(
        &b.app,
        "PUT",
        "/provider-model-spec",
        json!({ "provider": "relay", "model": "m", "context_window": -1 }),
    )
    .await;
    assert!(st.is_client_error(), "{st}");
    assert_eq!(b.file(), before);
}

#[tokio::test]
async fn an_alias_takes_the_spec_of_the_model_that_serves_it() {
    let up = upstream().await;
    let b = bed(&config(
        up,
        "    model_specs:
      中转自有模型: { context_window: 32000 }
",
        "aliases:
  own: 中转自有模型
",
    ));
    rows(&b).await;
    let (st, v) = call(&b.app, "GET", "/aliases", Value::Null).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let own = &v["aliases"][0];
    assert_eq!(own["name"], "own");
    assert_eq!(own["context_window"], 32_000, "{own}");
}

/// 规格在那一项里面：编辑对话框保存（不带规格）不丢，改名跟着走，删掉一起没
#[tokio::test]
async fn specs_stay_with_their_upstream_through_edits_renames_and_deletes() {
    let up = upstream().await;
    let b = bed(&config(
        up,
        "    model_specs:
      中转自有模型: { context_window: 32000 }
",
        "",
    ));
    let (st, v) = call(
        &b.app,
        "PUT",
        "/providers/relay",
        json!({ "provider": {
            "name": "中转",
            "base_url": format!("http://{up}"),
            "key": "sk-good",
            "protocol": "anthropic",
        }}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let cfg = tw_config::try_parse(&b.file()).unwrap();
    assert_eq!(cfg.providers[0].name, "中转");
    assert_eq!(
        cfg.providers[0].model_specs["中转自有模型"].context_window,
        Some(32_000),
        "{}",
        b.file()
    );
    // 规格按名字跟着这一家：旧名字上设不了
    let (st, _) = call(
        &b.app,
        "PUT",
        "/provider-model-spec",
        json!({ "provider": "relay", "model": "m", "context_window": 1000 }),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    let (st, v) = call(&b.app, "DELETE", "/providers/中转", json!({})).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(!b.file().contains("model_specs"), "{}", b.file());
}
