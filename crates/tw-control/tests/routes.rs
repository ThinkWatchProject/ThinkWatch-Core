//! 路由与策略组按资源增删改、更换默认路由。
//!
//! 和上游那一组一样，断言落在**文件本身**上：规则按交过来的顺序写进去、
//! 密钥跟着路由改名、删除时密钥改用用户选的那一条、合成的默认路由保存即
//! 写入、内置的「全部上游」动不了。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

/// 没有默认路由：它由网关合成，兜底指向「全部上游」。
const BASE: &str = "version: 1
clients:
  - name: claude-code
    key: tw-a
  - name: codex
    key: tw-b
    route: codex
  - name: scripts
    key: tw-c
# 两家上游
providers:
  - name: 官方
    base_url: https://api.anthropic.com
    key: sk-a
  - name: 中转
    base_url: https://relay.example.com
    key: sk-b
    models: [claude-sonnet-4-5, 中转自有模型]
groups:
  - name: 主力
    type: select
    providers: [官方, 中转]
    selected: 官方
routes:
  - name: codex
    rules:
      - name: 禁用 Opus
        when: { model: 'claude-opus-*' }
        deny: 此密钥不提供 Opus 模型
      - name: 兜底
        to: 主力
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
    fn route_of(&self, key: &str) -> Option<String> {
        self.parsed()
            .clients
            .iter()
            .find(|c| c.name == key)
            .unwrap()
            .route
            .clone()
    }
    async fn overview(&self) -> serde_json::Value {
        let (st, v) = call(&self.app, "GET", "/overview", json!(null)).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        v
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
        chatgpt: Default::default(),
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

fn find<'a>(list: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == name)
        .unwrap_or_else(|| panic!("没有「{name}」：{list}"))
}

fn rule(name: &str, conditions: serde_json::Value, to: &str) -> serde_json::Value {
    json!({ "name": name, "conditions": conditions, "to": to })
}

fn catch_all(to: &str) -> serde_json::Value {
    rule("兜底", json!([]), to)
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

// ─────────────────────────────────────────────────────────── 概览

#[tokio::test]
async fn the_overview_says_what_is_synthesised_and_gives_every_rule_in_full() {
    let b = bed(BASE);
    let ov = b.overview().await;
    let default = find(&ov["routes"], "默认");
    assert_eq!(default["default"], true);
    assert_eq!(default["builtin"], true, "配置文件里没有它：{default}");
    assert_eq!(default["has_catch_all"], true);
    assert_eq!(default["rules"][0]["name"], "兜底");
    assert_eq!(default["rules"][0]["to"], "__all__");
    assert_eq!(default["rules"][0]["catch_all"], true);

    let codex = find(&ov["routes"], "codex");
    assert_eq!(codex["builtin"], false);
    assert_eq!(codex["clients"], json!(["codex"]));
    // 拒绝原因要给出来：编辑对话框靠它回填
    assert_eq!(codex["rules"][0]["deny"], "此密钥不提供 Opus 模型");
    assert_eq!(
        codex["rules"][0]["conditions"],
        json!([{ "field": "model", "values": ["claude-opus-*"] }])
    );

    let all = find(&ov["groups"], "__all__");
    assert_eq!(all["builtin"], true);
    assert_eq!(all["providers"], json!(["官方", "中转"]));
    assert_eq!(find(&ov["groups"], "主力")["builtin"], false);
}

// ─────────────────────────────────────────────────────────── 路由

#[tokio::test]
async fn saving_the_synthesised_default_route_writes_it_into_the_file() {
    let b = bed(BASE);
    let (st, v) = call(
        &b.app,
        "PUT",
        &format!("/routes/{}", enc("默认")),
        json!({ "route": { "name": "默认", "rules": [
            rule("长上下文", json!([{ "field": "input_tokens", "values": [">200k"] }]), "中转"),
            catch_all("__all__"),
        ]}}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let cfg = b.parsed();
    let r = cfg.routes.iter().find(|r| r.name == "默认").unwrap();
    assert_eq!(
        r.rules.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(),
        ["长上下文", "兜底"]
    );
    // 兜底规则不写 `when: {}`，默认路由的名字也不写进 `default_route`
    let file = b.file();
    assert!(!file.contains("when: {}"), "{file}");
    assert!(!file.contains("default_route"), "{file}");
    assert!(file.contains("# 两家上游"), "{file}");
    assert_eq!(
        find(&b.overview().await["routes"], "默认")["builtin"],
        false
    );
}

#[tokio::test]
async fn rules_are_written_in_the_order_they_were_given() {
    // 通用补丁只能追加：新规则落在兜底之后，永远不会被选中
    let b = bed(BASE);
    let (st, v) = call(
        &b.app,
        "PUT",
        "/routes/codex",
        json!({ "route": { "name": "codex", "rules": [
            rule("GPT 模型", json!([{ "field": "model", "values": ["gpt-*"] }]), "中转"),
            json!({ "name": "禁用 Opus",
                    "conditions": [{ "field": "model", "values": ["claude-opus-*"] }],
                    "deny": "此密钥不提供 Opus 模型" }),
            catch_all("主力"),
        ]}}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let cfg = b.parsed();
    let names: Vec<_> = cfg.routes[0]
        .rules
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(names, ["GPT 模型", "禁用 Opus", "兜底"]);
}

#[tokio::test]
async fn a_rule_after_the_catch_all_is_saved_but_reported_as_shadowed() {
    let b = bed(BASE);
    let (st, v) = call(
        &b.app,
        "PUT",
        "/routes/codex",
        json!({ "route": { "name": "codex", "rules": [
            catch_all("主力"),
            rule("Gemini 模型", json!([{ "field": "model", "values": ["gemini-*"] }]), "中转"),
        ]}}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let codex = find(&b.overview().await["routes"], "codex").clone();
    assert_eq!(codex["rules"][0]["shadowed"], false);
    assert_eq!(codex["rules"][1]["shadowed"], true, "{codex}");
}

#[tokio::test]
async fn a_new_route_takes_exactly_the_keys_it_was_given() {
    let b = bed(BASE);
    let (st, v) = call(
        &b.app,
        "POST",
        "/routes",
        json!({ "route": { "name": "批处理", "rules": [catch_all("中转")] },
                "keys": ["scripts"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(b.route_of("scripts").as_deref(), Some("批处理"));
    assert_eq!(b.route_of("claude-code"), None, "没选的密钥不动");
    assert_eq!(b.route_of("codex").as_deref(), Some("codex"));
}

#[tokio::test]
async fn the_name_of_the_synthesised_default_is_taken() {
    // 配置里没有「默认」，但它就在那儿 —— 同名的新路由会顶替掉它
    let b = bed(BASE);
    let (st, _) = call(
        &b.app,
        "POST",
        "/routes",
        json!({ "route": { "name": "默认", "rules": [catch_all("中转")] } }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_key_moved_out_of_a_route_goes_back_to_the_default() {
    let b = bed(BASE);
    let (st, v) = call(
        &b.app,
        "PUT",
        "/routes/codex",
        json!({ "route": { "name": "codex", "rules": [catch_all("主力")] }, "keys": [] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(b.route_of("codex"), None);
}

#[tokio::test]
async fn renaming_a_route_moves_its_keys_in_the_same_write() {
    let b = bed(BASE);
    let (st, v) = call(
        &b.app,
        "PUT",
        "/routes/codex",
        json!({ "route": { "name": "codex-cli", "rules": [catch_all("主力")] } }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(b.route_of("codex").as_deref(), Some("codex-cli"));
}

#[tokio::test]
async fn a_broken_condition_is_refused_before_anything_is_written() {
    let b = bed(BASE);
    let before = b.file();
    for (conditions, says) in [
        // 少了比较符：写进去的后果是这条规则永远不命中
        (
            json!([{ "field": "input_tokens", "values": ["200k"] }]),
            "比较符",
        ),
        (
            json!([{ "field": "dialect", "values": ["anthorpic"] }]),
            "anthorpic",
        ),
        (
            json!([{ "field": "client", "values": ["没有这把"] }]),
            "没有这把",
        ),
        (
            json!([{ "field": "cache", "values": ["yes"] }]),
            "true 或 false",
        ),
        (json!([{ "field": "colour", "values": ["red"] }]), "colour"),
    ] {
        let (st, v) = call(
            &b.app,
            "PUT",
            "/routes/codex",
            json!({ "route": { "name": "codex", "rules": [
                rule("坏的", conditions, "中转"),
                catch_all("主力"),
            ]}}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
        assert!(v.as_str().unwrap_or_default().contains(says), "{v}");
    }
    assert_eq!(b.file(), before);
}

#[tokio::test]
async fn a_rule_that_only_adds_a_guard_can_be_saved() {
    let b = bed(BASE);
    let (st, v) = call(
        &b.app,
        "PUT",
        "/routes/codex",
        json!({ "route": { "name": "codex", "rules": [
            { "name": "中转加强保护",
              "conditions": [{ "field": "provider_would_be", "values": ["中转"] }],
              "guard": { "redact": ["internal"], "untrusted": true } },
            catch_all("主力"),
        ]}}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let codex = find(&b.overview().await["routes"], "codex").clone();
    assert_eq!(codex["rules"][0]["phase_two"], true);
    assert_eq!(codex["rules"][0]["guard"]["untrusted"], true, "{codex}");
}

#[tokio::test]
async fn an_assistant_request_condition_can_route_those_requests_in_the_same_write() {
    // 生成标题默认原样放行，不带类别标记：条件写进去也永远不满足
    let b = bed(BASE);
    let (st, v) = call(
        &b.app,
        "PUT",
        "/routes/codex",
        json!({ "route": { "name": "codex", "rules": [
            { "name": "标题用小模型",
              "conditions": [{ "field": "intent", "values": ["titling"] }],
              "set": { "model": "claude-haiku-4-5" } },
            catch_all("主力"),
        ]}, "route_probes": ["titling"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let probes = b.parsed().client_probes;
    assert_eq!(probes.titling, tw_config::ProbeAction::Route);
    assert_eq!(
        probes.topic_detect,
        tw_config::ProbeAction::Passthrough,
        "没选的不动"
    );

    let (st, _) = call(
        &b.app,
        "PUT",
        "/routes/codex",
        json!({ "route": { "name": "codex", "rules": [catch_all("主力")] },
                "route_probes": ["assistant_internal"] }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn deleting_a_route_sends_its_keys_where_the_user_chose() {
    let b = bed(BASE);
    call(
        &b.app,
        "POST",
        "/routes",
        json!({ "route": { "name": "批处理", "rules": [catch_all("中转")] } }),
    )
    .await;
    let (st, v) = call(
        &b.app,
        "DELETE",
        &format!("/routes/codex?reassign_to={}", enc("批处理")),
        json!(null),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(b.route_of("codex").as_deref(), Some("批处理"));
    assert!(!b.parsed().routes.iter().any(|r| r.name == "codex"));

    // 不选就是默认路由：不指定任何一条
    let (st, v) = call(
        &b.app,
        "DELETE",
        &format!("/routes/{}", enc("批处理")),
        json!(null),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(b.route_of("codex"), None);
}

#[tokio::test]
async fn the_default_route_cannot_be_deleted() {
    let b = bed(BASE);
    let (st, v) = call(
        &b.app,
        "DELETE",
        &format!("/routes/{}", enc("默认")),
        json!(null),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
}

#[tokio::test]
async fn changing_the_default_keeps_the_synthesised_one_as_an_ordinary_route() {
    let b = bed(BASE);
    let (st, v) = call(&b.app, "PUT", "/default_route", json!({ "name": "codex" })).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let cfg = b.parsed();
    assert_eq!(cfg.default_route.as_deref(), Some("codex"));
    let kept = cfg
        .routes
        .iter()
        .find(|r| r.name == "默认")
        .expect("「默认」留下来");
    assert_eq!(kept.rules[0].to.as_deref(), Some("__all__"));

    // 换回来：默认值不写进文件
    let (st, v) = call(&b.app, "PUT", "/default_route", json!({ "name": "默认" })).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(!b.file().contains("default_route"), "{}", b.file());
}

// ─────────────────────────────────────────────────────────── 策略组

#[tokio::test]
async fn a_group_forwarded_to_by_a_rule_cannot_be_deleted() {
    let b = bed(BASE);
    let (st, v) = call(
        &b.app,
        "DELETE",
        &format!("/groups/{}", enc("主力")),
        json!(null),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert!(v.as_str().unwrap().contains("codex"), "说清是谁在用：{v}");
}

#[tokio::test]
async fn renaming_a_group_moves_the_rules_that_forward_to_it() {
    let b = bed(BASE);
    let (st, v) = call(
        &b.app,
        "PUT",
        &format!("/groups/{}", enc("主力")),
        json!({ "group": { "name": "主力组", "kind": "select",
                           "providers": ["官方", "中转"], "selected": "中转" } }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let cfg = b.parsed();
    assert_eq!(cfg.routes[0].rules[1].to.as_deref(), Some("主力组"));
    assert_eq!(cfg.groups[0].selected.as_deref(), Some("中转"));
}

#[tokio::test]
async fn a_new_group_writes_no_defaults() {
    let b = bed(BASE);
    let (st, v) = call(
        &b.app,
        "POST",
        "/groups",
        json!({ "group": { "name": "便宜优先", "kind": "cheapest",
                           "providers": ["中转", "官方"] } }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let file = b.file();
    assert!(!file.contains("session_affinity"), "{file}");
    let g = b
        .parsed()
        .groups
        .into_iter()
        .find(|g| g.name == "便宜优先")
        .unwrap();
    assert_eq!(g.providers, ["中转", "官方"]);
}

#[tokio::test]
async fn the_built_in_group_and_reserved_names_are_refused() {
    let b = bed(BASE);
    let group = |name: &str| json!({ "group": { "name": name, "kind": "fallback", "providers": ["官方"] } });
    let (st, _) = call(&b.app, "PUT", "/groups/__all__", group("__all__")).await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, _) = call(&b.app, "DELETE", "/groups/__all__", json!(null)).await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, v) = call(&b.app, "POST", "/groups", group("__mine")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    let (st, v) = call(&b.app, "POST", "/groups", group("官方")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "和上游同名：{v}");
}

// ─────────────────────────────────────────────────────────── 已知模型

#[tokio::test]
async fn known_models_come_from_the_same_catalog_as_the_model_list() {
    // 手动清单也算「知道」：条件和试算的模型建议从这里来
    let b = bed(BASE);
    let (st, v) = call(&b.app, "GET", "/models", json!(null)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let sonnet = find_id(&v, "claude-sonnet-4-5");
    assert_eq!(sonnet["providers"], json!(["中转"]));
    assert!(
        v.as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "中转自有模型"),
        "{v}"
    );
}

fn find_id<'a>(list: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == id)
        .unwrap_or_else(|| panic!("没有「{id}」：{list}"))
}
