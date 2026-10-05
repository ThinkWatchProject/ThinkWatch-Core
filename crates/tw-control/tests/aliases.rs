//! 模型别名：列表（发往哪儿、挡住了谁、用量、建议）、增删改、改名带着引用、预览、
//! 谁在用。
//!
//! 各家上游的清单用配置里手写的 `models`（不连网）；断言落在 JSON 和**配置文件本身**上：
//! 注释留没留、顺序变没变、改名有没有把引用一起改掉。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

const BASE: &str = "version: 1
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
clients:
  - name: default
    key: tw-aaaa
    allow: [sonnet, 'claude-*']  # 只给 Claude
  - name: codex
    key: tw-bbbb
providers:
  - name: anthropic
    base_url: https://api.anthropic.com
    key: sk-a
    models: [claude-sonnet-4-5, claude-opus-4-1-20250805, sonnet]
  - name: bedrock
    base_url: https://bedrock.example
    key: sk-b
    protocol: anthropic
    models:
      - us.anthropic.claude-sonnet-4-5-v1:0
      - global.anthropic.claude-sonnet-4-5-v1:0
      - anthropic.claude-opus-4-1-20250805-v1:0
  - name: relay
    base_url: https://relay.example
    key: sk-c
    protocol: anthropic
    models: [anthropic/claude-sonnet-4.5, anthropic/claude-opus-4.1]
  - name: off
    base_url: https://off.example
    key: sk-d
    protocol: anthropic
    disabled: true
    models: [claude-sonnet-4-5, sonnet]
  # 没有清单：不知道它有什么
  - name: blind
    base_url: https://blind.example
    key: sk-e
    protocol: anthropic
    models_only: ['claude-*']
# 别名，顺序是我排的
aliases:
  sonnet:  # Bedrock 上有两种写法，列一种就够
    - claude-sonnet-4-5
    - us.anthropic.claude-sonnet-4-5-v1:0
routes:
  - name: default
    rules:
      - name: 降级
        when: { model: sonnet }
        set: { model: sonnet }
      - name: 兜底
        to: anthropic
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
    async fn version(&self) -> String {
        let (_, v) = call(&self.app, "GET", "/config", Value::Null).await;
        v["version"].as_str().unwrap().to_string()
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// 一条客户端用 `model` 发来的请求，`ago_ms` 毫秒之前。
fn row(id: i64, model: &str, ago_ms: i64, cost: Option<i64>) -> tw_store::db::RequestRow {
    tw_store::db::RequestRow {
        session_log_bytes: None,
        key_masked: None,
        peer: None,
        id,
        at_ms: now_ms() - ago_ms,
        client: "default".into(),
        client_hint: None,
        session: None,
        provider: "anthropic".into(),
        model: model.into(),
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
        cost_micros: cost,
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

fn bed(yaml: &str, rows: Option<&[tw_store::db::RequestRow]>) -> Bed {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, yaml).unwrap();
    let store = rows.map(|rows| {
        let db = tw_store::Db::open(&d.path().join("data.db")).unwrap();
        for r in rows {
            db.insert(r).unwrap();
        }
        let rec = tw_store::Recorder::new(
            db,
            tw_store::Blobs::new(d.path().join("blobs")),
            tw_pricing::shared(tw_pricing::PriceBook::builtin().unwrap()),
        );
        Arc::new(tokio::sync::Mutex::new(rec))
    });
    let cfg = tw_config::try_parse(yaml).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let state = ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        store,
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
    (
        st,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

fn pinned(v: &Value) -> Vec<(String, String)> {
    v.as_array()
        .unwrap_or_else(|| panic!("not a list: {v}"))
        .iter()
        .map(|x| {
            (
                x["provider"].as_str().unwrap().to_string(),
                x["model"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn pairs(x: &[(&str, &str)]) -> Vec<(String, String)> {
    x.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

fn codes(v: &Value) -> Vec<String> {
    v["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["code"].as_str().unwrap().to_string())
        .collect()
}

// ─────────────────────────────────────────────────────────── 读

/// 一行别名：每家发出的名称、谁有列表里的哪个名称、同名被挡住的上游、上下文窗口、
/// 24 小时用量；以及只认 Claude 的建议
#[tokio::test]
async fn the_list_says_where_each_alias_goes_and_what_it_shadows() {
    let b = bed(
        BASE,
        Some(&[
            row(1, "sonnet", 60_000, Some(1_000)),
            // 算不出钱的不进费用，照样算一次请求
            row(2, "sonnet", 120_000, None),
            // 24 小时之前的不算
            row(3, "sonnet", 2 * 24 * 3600 * 1000, Some(5_000)),
            row(4, "claude-sonnet-4-5", 60_000, Some(7_000)),
        ]),
    );
    let (st, v) = call(&b.app, "GET", "/aliases", Value::Null).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let a = &v["aliases"][0];
    assert_eq!(a["name"], "sonnet");
    // 停用的上游不在里面；没有清单的当作能服务，取第一个在启用范围里的名称；有清单
    // 而两个名称都没有的（relay）服务不了
    assert_eq!(
        pinned(&a["served_by"]),
        pairs(&[
            ("anthropic", "claude-sonnet-4-5"),
            ("bedrock", "us.anthropic.claude-sonnet-4-5-v1:0"),
            ("blind", "claude-sonnet-4-5"),
        ])
    );
    assert_eq!(a["models"][0]["model"], "claude-sonnet-4-5");
    assert_eq!(a["models"][0]["providers"], json!(["anthropic"]));
    assert_eq!(a["models"][1]["providers"], json!(["bedrock"]));
    // anthropic 有一个叫 sonnet 的真模型，别名没列它：这个名称不会再发给它
    assert_eq!(a["shadows"], json!(["anthropic"]));
    assert_eq!(a["context_window"], 200_000);
    assert_eq!(a["requests_24h"], 2);
    assert_eq!(a["cost_micros_24h"], 1_000);

    // Sonnet 4.5：relay 那一家别名还没列 → 出；Opus 4.1 带日期的两家 → 出；relay 的
    // `claude-opus-4.1` 不带日期，不是同一个
    let s = v["suggestions"].as_array().unwrap();
    let labels: Vec<&str> = s.iter().map(|x| x["label"].as_str().unwrap()).collect();
    assert_eq!(
        labels,
        ["Claude Sonnet 4.5", "Claude Opus 4.1 (2025-08-05)"],
        "{v}"
    );
    assert_eq!(
        pinned(&s[0]["models"]),
        pairs(&[
            ("anthropic", "claude-sonnet-4-5"),
            ("bedrock", "us.anthropic.claude-sonnet-4-5-v1:0"),
            ("bedrock", "global.anthropic.claude-sonnet-4-5-v1:0"),
            ("relay", "anthropic/claude-sonnet-4.5"),
        ])
    );
    assert_eq!(
        pinned(&s[1]["models"]),
        pairs(&[
            ("anthropic", "claude-opus-4-1-20250805"),
            ("bedrock", "anthropic.claude-opus-4-1-20250805-v1:0"),
        ])
    );
}

/// 没有请求记录（库没起来）也打得开：用量是 0，费用不知道
#[tokio::test]
async fn the_list_opens_without_request_records() {
    let b = bed(BASE, None);
    let (st, v) = call(&b.app, "GET", "/aliases", Value::Null).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["aliases"][0]["requests_24h"], 0);
    assert!(v["aliases"][0].get("cost_micros_24h").is_none(), "{v}");
}

/// 别名替一组里的每一家都列了一种写法：这一组的建议不再出
#[tokio::test]
async fn a_group_every_upstream_of_which_an_alias_reaches_is_not_suggested() {
    let yaml = BASE.replace(
        "    - us.anthropic.claude-sonnet-4-5-v1:0\nroutes:",
        "    - us.anthropic.claude-sonnet-4-5-v1:0\n    - anthropic/claude-sonnet-4.5\nroutes:",
    );
    let b = bed(&yaml, None);
    let (_, v) = call(&b.app, "GET", "/aliases", Value::Null).await;
    let labels: Vec<&str> = v["suggestions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["Claude Opus 4.1 (2025-08-05)"], "{v}");
    // 列全了之后，relay 也发得到
    assert!(
        pinned(&v["aliases"][0]["served_by"])
            .contains(&("relay".into(), "anthropic/claude-sonnet-4.5".into()))
    );
}

#[tokio::test]
async fn usage_lists_the_keys_and_rules_that_name_the_alias() {
    let b = bed(BASE, Some(&[row(1, "sonnet", 1_000, Some(10))]));
    let (st, v) = call(&b.app, "GET", "/aliases/sonnet/usage", Value::Null).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["requests_24h"], 1);
    // `claude-*` 是一批名字，不算
    assert_eq!(v["keys"], json!(["default"]));
    assert_eq!(
        v["rules"],
        json!([
            { "route": "default", "rule": "降级", "field": "when.model" },
            { "route": "default", "rule": "降级", "field": "set.model" },
        ])
    );
    let (st, v) = call(&b.app, "GET", "/aliases/nope/usage", Value::Null).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(v["code"], "config.edit.not_found");
}

// ─────────────────────────────────────────────────────────── 写

/// 新的接在表的末尾；模型名去掉首尾空白；注释和别的项原样
#[tokio::test]
async fn a_new_alias_goes_to_the_end_and_keeps_the_comments() {
    let b = bed(BASE, None);
    let (st, v) = call(
        &b.app,
        "POST",
        "/aliases",
        json!({ "alias": { "name": "opus", "models": [
            "claude-opus-4-1-20250805", " anthropic.claude-opus-4-1-20250805-v1:0 "
        ] } }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["version"].as_str().unwrap().starts_with("blake3:"), "{v}");
    let f = b.file();
    assert!(
        f.contains("  sonnet:  # Bedrock 上有两种写法，列一种就够\n"),
        "{f}"
    );
    assert!(f.contains("# 别名，顺序是我排的"), "{f}");
    let all = b.parsed().aliases;
    let names: Vec<&str> = all.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["sonnet", "opus"]);
    assert_eq!(
        all[1].models,
        [
            "claude-opus-4-1-20250805",
            "anthropic.claude-opus-4-1-20250805-v1:0"
        ]
    );
    // 一个模型写成字符串
    let (st, _) = call(
        &b.app,
        "POST",
        "/aliases",
        json!({ "alias": { "name": "haiku", "models": ["claude-haiku-4-5"] } }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        b.file().contains("\n  haiku: claude-haiku-4-5\n"),
        "{}",
        b.file()
    );
    // 新的一版生效了：列表里有它
    let (_, v) = call(&b.app, "GET", "/aliases", Value::Null).await;
    assert_eq!(v["aliases"][1]["name"], "opus");
}

#[tokio::test]
async fn a_taken_name_a_bad_alias_or_a_stale_version_is_refused() {
    let b = bed(BASE, None);
    let before = b.file();
    let (st, v) = call(
        &b.app,
        "POST",
        "/aliases",
        json!({ "alias": { "name": "sonnet", "models": ["x"] } }),
    )
    .await;
    assert_eq!(
        (st, v["code"].as_str()),
        (StatusCode::CONFLICT, Some("config.edit.name_taken")),
        "{v}"
    );
    // 配置校验说的那一句
    let (st, v) = call(
        &b.app,
        "POST",
        "/aliases",
        json!({ "alias": { "name": "x", "models": [] } }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["code"], "config.alias_no_models", "{v}");
    let (st, v) = call(
        &b.app,
        "POST",
        "/aliases",
        json!({ "alias": { "name": "x", "models": ["sonnet"] } }),
    )
    .await;
    assert_eq!(
        (st, v["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("config.alias_chained")),
        "{v}"
    );
    let (st, v) = call(
        &b.app,
        "POST",
        "/aliases",
        json!({ "alias": { "name": " x", "models": ["a"] } }),
    )
    .await;
    assert_eq!(
        (st, v["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("control.name_whitespace")),
        "{v}"
    );
    // 有人抢先改了：409，什么都不写
    for (method, path) in [("POST", "/aliases"), ("PUT", "/aliases/sonnet")] {
        let (st, v) = call(
            &b.app,
            method,
            path,
            json!({ "alias": { "name": "y", "models": ["a"] }, "base_version": "blake3:000000000000" }),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "{method} {path}: {v}");
        assert_eq!(v["code"], "control.config_stale");
    }
    let (st, _) = call(
        &b.app,
        "DELETE",
        "/aliases/sonnet?base_version=blake3:000000000000",
        Value::Null,
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(b.file(), before);
}

/// 改名在原位，引用旧名的密钥（整项）和规则（`when.model`、`set.model`）在同一个版本里
/// 跟着改，返回改了哪些
#[tokio::test]
async fn renaming_an_alias_carries_its_references_in_the_same_version() {
    let b = bed(BASE, None);
    let base = b.version().await;
    let (st, v) = call(
        &b.app,
        "PUT",
        "/aliases/sonnet",
        json!({
            "alias": { "name": "claude-sonnet", "models": [
                "claude-sonnet-4-5", "us.anthropic.claude-sonnet-4-5-v1:0"
            ] },
            "base_version": base,
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_ne!(v["version"], base.as_str());
    assert_eq!(v["renamed_in"]["keys"], json!(["default"]));
    assert_eq!(v["renamed_in"]["requests_24h"], 0);
    assert_eq!(
        v["renamed_in"]["rules"],
        json!([
            { "route": "default", "rule": "降级", "field": "when.model" },
            { "route": "default", "rule": "降级", "field": "set.model" },
        ])
    );
    let f = b.file();
    // 位置、注释、别的写法原样
    assert!(
        f.contains("  claude-sonnet:  # Bedrock 上有两种写法，列一种就够\n"),
        "{f}"
    );
    assert!(
        f.contains("    allow: [claude-sonnet, 'claude-*']  # 只给 Claude\n"),
        "{f}"
    );
    assert!(
        f.contains("        when: { model: claude-sonnet }\n"),
        "{f}"
    );
    let cfg = b.parsed();
    assert_eq!(cfg.aliases[0].name, "claude-sonnet");
    assert_eq!(
        cfg.routes[0].rules[0]
            .set
            .as_ref()
            .unwrap()
            .model
            .as_deref(),
        Some("claude-sonnet")
    );

    // 只改模型：没有引用要改
    let (st, v) = call(
        &b.app,
        "PUT",
        "/aliases/claude-sonnet",
        json!({ "alias": { "name": "claude-sonnet", "models": ["claude-sonnet-4-5"] } }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["renamed_in"]["keys"], json!([]));
    assert_eq!(v["renamed_in"]["rules"], json!([]));
    // 一个模型写成字符串，还在原来的位置（列表里的注释跟着旧的列表走了）
    assert!(
        b.file()
            .contains("aliases:\n  claude-sonnet: claude-sonnet-4-5\nroutes:"),
        "{}",
        b.file()
    );

    let (st, v) = call(
        &b.app,
        "PUT",
        "/aliases/nope",
        json!({ "alias": { "name": "nope", "models": ["x"] } }),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
}

/// 删掉；引用它的地方不拦（那是一个上游模型名，配置照样成立）；删到最后一个，整张表
/// 一起没了
#[tokio::test]
async fn deleting_the_last_alias_removes_the_table() {
    let b = bed(BASE, None);
    let base = b.version().await;
    let (st, v) = call(
        &b.app,
        "DELETE",
        &format!("/aliases/sonnet?base_version={base}"),
        Value::Null,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let f = b.file();
    assert!(!f.contains("aliases:"), "{f}");
    // 规则和密钥原样
    assert!(f.contains("        when: { model: sonnet }\n"), "{f}");
    let (_, v) = call(&b.app, "GET", "/aliases", Value::Null).await;
    assert_eq!(v["aliases"], json!([]));
    let (st, v) = call(&b.app, "DELETE", "/aliases/sonnet", Value::Null).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
}

// ─────────────────────────────────────────────────────────── 预览

#[tokio::test]
async fn a_preview_says_where_a_draft_would_go_and_suggests_the_same_model_elsewhere() {
    let b = bed(BASE, None);
    let before = b.file();
    let (st, v) = call(
        &b.app,
        "POST",
        "/alias-preview",
        json!({ "alias": { "name": "opus", "models": ["claude-opus-4-1-20250805", "nope-model"] } }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(codes(&v), Vec::<String>::new());
    assert_eq!(
        pinned(&v["served_by"]),
        pairs(&[
            ("anthropic", "claude-opus-4-1-20250805"),
            ("blind", "claude-opus-4-1-20250805"),
        ])
    );
    assert_eq!(v["unserved"], json!(["nope-model"]));
    assert_eq!(v["shadows"], json!([]));
    // relay 的 `claude-opus-4.1` 没有日期，不是同一个
    assert_eq!(
        pinned(&v["same_model"]),
        pairs(&[("bedrock", "anthropic.claude-opus-4-1-20250805-v1:0")])
    );
    // 什么都没写
    assert_eq!(b.file(), before);
}

/// 编辑时带上原名：不算和自己重名；别的上游上的同一个模型，已经到得了的上游不再建议
#[tokio::test]
async fn editing_an_alias_is_not_a_duplicate_of_itself() {
    let b = bed(BASE, None);
    let draft = json!({ "name": "sonnet", "models": ["claude-sonnet-4-5"] });
    let (_, v) = call(&b.app, "POST", "/alias-preview", json!({ "alias": draft })).await;
    assert_eq!(codes(&v), ["config.alias_duplicate"], "{v}");

    let (_, v) = call(
        &b.app,
        "POST",
        "/alias-preview",
        json!({ "alias": draft, "original": "sonnet" }),
    )
    .await;
    assert_eq!(codes(&v), Vec::<String>::new(), "{v}");
    assert_eq!(v["shadows"], json!(["anthropic"]));
    assert_eq!(
        pinned(&v["same_model"]),
        pairs(&[
            ("bedrock", "us.anthropic.claude-sonnet-4-5-v1:0"),
            ("bedrock", "global.anthropic.claude-sonnet-4-5-v1:0"),
            ("relay", "anthropic/claude-sonnet-4.5"),
        ])
    );
    // 改名也一样
    let (_, v) = call(
        &b.app,
        "POST",
        "/alias-preview",
        json!({ "alias": { "name": "sonnet-4.5", "models": ["claude-sonnet-4-5"] }, "original": "sonnet" }),
    )
    .await;
    assert_eq!(codes(&v), Vec::<String>::new(), "{v}");
    // 列了自己的名字：不挡任何一家
    let (_, v) = call(
        &b.app,
        "POST",
        "/alias-preview",
        json!({ "alias": { "name": "sonnet", "models": ["sonnet", "claude-sonnet-4-5"] }, "original": "sonnet" }),
    )
    .await;
    assert_eq!(v["shadows"], json!([]), "{v}");
    assert_eq!(
        pinned(&v["served_by"])[0],
        ("anthropic".into(), "sonnet".into())
    );
}

/// 挡着保存的问题，和保存时说的同一句
#[tokio::test]
async fn a_preview_lists_what_would_stop_the_save() {
    let b = bed(BASE, None);
    let case = |alias: Value, original: Option<&str>| {
        let app = b.app.clone();
        let body = match original {
            Some(o) => json!({ "alias": alias, "original": o }),
            None => json!({ "alias": alias }),
        };
        async move {
            let (st, v) = call(&app, "POST", "/alias-preview", body).await;
            assert_eq!(st, StatusCode::OK, "{v}");
            codes(&v)
        }
    };
    assert_eq!(
        case(json!({ "name": "x", "models": [] }), None).await,
        ["config.alias_no_models"]
    );
    assert_eq!(
        case(json!({ "name": "x", "models": ["sonnet"] }), None).await,
        ["config.alias_chained"]
    );
    assert_eq!(
        case(json!({ "name": "claude-*", "models": ["a"] }), None).await,
        ["config.alias_wildcard"]
    );
    assert_eq!(
        case(json!({ "name": "x", "models": ["a", "  "] }), None).await,
        ["config.alias_blank_model"]
    );
    // 名字空着只说一次；发往哪儿照样算
    assert_eq!(
        case(json!({ "name": "", "models": ["a"] }), None).await,
        ["control.name_empty"]
    );
    let (_, v) = call(
        &b.app,
        "POST",
        "/alias-preview",
        json!({ "alias": { "name": "", "models": ["claude-opus-4-1-20250805"] } }),
    )
    .await;
    assert_eq!(pinned(&v["served_by"]).len(), 2, "{v}");
    // 名字的问题和模型的问题一起说
    assert_eq!(
        case(json!({ "name": " x", "models": [] }), None).await,
        ["control.name_whitespace", "config.alias_no_models"]
    );
    assert_eq!(
        case(json!({ "name": "x", "models": ["a"] }), Some("gone")).await,
        ["config.edit.not_found"]
    );
}
