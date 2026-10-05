//! 模型别名在目录里：`/v1/models` 怎么列、密钥的 `allow` 怎么继承、准入和挑候选
//! 怎么看别名和指定模型。
//!
//! 假上游列出自己的清单，**收什么模型都答**，并说出自己是谁、收到的是哪个名字 ——
//! 断言落在「准入放没放行、请求落在了哪一家」上。发过去叫什么是发送那一步的事，
//! 这里只在指定模型（原样发出）时看。

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
use serde_json::Value;
use tw_config::{Client, Config, Provider};

/// 一个假上游：列出 `models`，什么模型的请求都答。
async fn upstream(name: &'static str, models: &'static [&'static str]) -> SocketAddr {
    let app = Router::new()
        .route(
            "/v1/models",
            get(move || async move {
                axum::Json(serde_json::json!({
                    "data": models.iter().map(|m| serde_json::json!({ "id": m })).collect::<Vec<_>>()
                }))
            }),
        )
        .route(
            "/v1/messages",
            post(move |body: axum::body::Bytes| async move {
                let v: Value = serde_json::from_slice(&body).unwrap_or_default();
                let sent = v["model"].as_str().unwrap_or_default().to_string();
                axum::Json(serde_json::json!({ "by": name, "sent": sent }))
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

fn provider(name: &str, at: SocketAddr) -> Provider {
    Provider {
        name: name.into(),
        base_url: format!("http://{at}"),
        key: Some("k".into()),
        protocol: Some(tw_config::Protocol::Anthropic),
        ..Default::default()
    }
}

/// 一把密钥。`allow` 不写就是什么都放行
fn client(key: &str, allow: Option<&[&str]>, route: Option<&str>) -> Client {
    Client {
        name: key.into(),
        key: key.into(),
        allow: allow.map(|a| a.iter().map(|s| s.to_string()).collect()),
        route: route.map(String::from),
        ..Default::default()
    }
}

/// 三家上游、四个别名：
///
/// - `sonnet`：官方叫 `claude-sonnet-4-5`，中转站叫 `anthropic/claude-sonnet-4-5`
/// - `gpt-x`：只有 openai 的 `gpt-x-2026`。中转站有个同名的真模型，没列进来
/// - `ghost`：谁都没有
async fn setup(clients: Vec<Client>, routes: &str) -> Config {
    let official = upstream("official", &["claude-sonnet-4-5"]).await;
    let relay = upstream("relay", &["anthropic/claude-sonnet-4-5", "gpt-x"]).await;
    let openai = upstream("openai", &["gpt-x-2026"]).await;
    let mut cfg = Config {
        clients,
        providers: vec![
            provider("official", official),
            provider("relay", relay),
            provider("openai", openai),
        ],
        ..Default::default()
    };
    cfg.aliases = serde_yaml_ng::from_str(
        "sonnet: [claude-sonnet-4-5, anthropic/claude-sonnet-4-5]\n\
         gpt-x: gpt-x-2026\n\
         ghost: nobody-has-this\n",
    )
    .unwrap();
    if !routes.is_empty() {
        cfg.routes = serde_yaml_ng::from_str(routes).unwrap();
    }
    cfg
}

async fn serve(cfg: Config) -> SocketAddr {
    let state = tw_gateway::AppState::new(cfg).unwrap();
    tw_gateway::models::refresh_all(&state).await;
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

/// `GET path`，按 OpenAI 的样子（Bearer）或 Anthropic 的样子（`x-api-key` + 版本头）。
async fn get_json(gw: SocketAddr, key: &str, path: &str, anthropic: bool) -> (u16, Value) {
    let mut r = reqwest::Client::new().get(format!("http://{gw}{path}"));
    r = if anthropic {
        r.header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
    } else {
        r.header("authorization", format!("Bearer {key}"))
    };
    let r = r.send().await.unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or_default())
}

async fn listed(gw: SocketAddr, key: &str) -> Vec<String> {
    let (_, v) = get_json(gw, key, "/v1/models", false).await;
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect()
}

async fn ask(gw: SocketAddr, key: &str, model: &str) -> (u16, Value) {
    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", key)
        .body(serde_json::json!({ "model": model, "messages": [] }).to_string())
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or_default())
}

fn message(body: &Value) -> &str {
    body["error"]["message"].as_str().unwrap_or_default()
}

// ─────────────────────────────────────────────────────────── 列表

#[tokio::test]
async fn aliases_are_listed_with_the_metadata_of_the_model_they_stand_for() {
    let gw = serve(setup(vec![client("tw-k", None, None)], "").await).await;

    // 谁都服务不了的别名不列；中转站那个同名的 gpt-x 让给了别名，只出现一次
    assert_eq!(
        listed(gw, "tw-k").await,
        [
            "anthropic/claude-sonnet-4-5",
            "claude-sonnet-4-5",
            "gpt-x",
            "gpt-x-2026",
            "sonnet"
        ]
    );

    // 别名的上下文窗口按它第一个有人提供的模型查（claude-sonnet-4-5，官方）
    let (_, list) = get_json(gw, "tw-k", "/v1/models", true).await;
    let sonnet = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "sonnet")
        .unwrap()
        .clone();
    assert_eq!(sonnet["max_input_tokens"], 200_000, "{sonnet}");
    assert_eq!(sonnet["context_window"], 200_000, "{sonnet}");
    assert_eq!(sonnet["supports_1m"], false, "{sonnet}");
    // 显示名按别名自己的名称推：看不出是 Claude 的就是名称本身
    assert_eq!(sonnet["display_name"], "sonnet");

    // 单点查询和列表里是同一个对象
    let (status, one) = get_json(gw, "tw-k", "/v1/models/sonnet", true).await;
    assert_eq!(status, 200, "{one}");
    assert_eq!(one, sonnet);
    let (status, _) = get_json(gw, "tw-k", "/v1/models/ghost", true).await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn a_keys_allow_reaches_an_alias_through_its_models_but_not_the_other_way() {
    let gw = serve(
        setup(
            vec![
                // 写上游的模型名：指向它的别名一起放行
                client("by-model", Some(&["claude-sonnet-*", "gpt-x-*"]), None),
                // 写别名：只有别名本身
                client("by-alias", Some(&["sonnet"]), None),
            ],
            "",
        )
        .await,
    )
    .await;

    assert_eq!(
        listed(gw, "by-model").await,
        ["claude-sonnet-4-5", "gpt-x", "gpt-x-2026", "sonnet"]
    );
    assert_eq!(listed(gw, "by-alias").await, ["sonnet"]);

    // 准入和列表一致
    let (status, body) = ask(gw, "by-model", "sonnet").await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = ask(gw, "by-alias", "sonnet").await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = ask(gw, "by-alias", "claude-sonnet-4-5").await;
    assert_eq!(status, 400, "{body}");
    assert!(
        message(&body).contains("may not use model claude-sonnet-4-5"),
        "{body}"
    );
    let (status, body) = ask(gw, "by-model", "anthropic/claude-sonnet-4-5").await;
    assert_eq!(status, 400, "{body}");
}

// ─────────────────────────────────────────────────────────── 准入与挑候选

#[tokio::test]
async fn an_alias_skips_an_upstream_whose_same_named_model_it_does_not_list() {
    // 中转站排在前面，它有一个叫 gpt-x 的真模型；别名 gpt-x 没列它，请求到不了它
    let mut cfg = setup(
        vec![client("tw-k", None, None)],
        "- name: default
  rules:
    - { name: 都走, to: pool }
",
    )
    .await;
    cfg.groups = vec![
        serde_yaml_ng::from_str("name: pool\ntype: fallback\nproviders: [relay, openai]\n")
            .unwrap(),
    ];
    let gw = serve(cfg).await;
    let (status, body) = ask(gw, "tw-k", "gpt-x").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["by"], "openai");

    // 别名谁都服务不了：说清是别名、列表里的名字一个都没人提供
    let (status, body) = ask(gw, "tw-k", "ghost").await;
    assert_eq!(status, 400, "{body}");
    assert!(
        message(&body).contains("No upstream serves alias ghost")
            && message(&body).contains("nobody-has-this"),
        "{body}"
    );
}

#[tokio::test]
async fn a_pinned_model_is_judged_at_its_own_upstream_and_as_written() {
    let gw = serve(
        setup(
            vec![
                client("tw-k", None, Some("pinned")),
                client("missing", None, Some("missing")),
                client("narrow", Some(&["claude-*"]), Some("narrow")),
            ],
            "- name: pinned
  rules:
    - name: 走中转站
      to:
        - { provider: relay, model: gpt-x }
- name: missing
  rules:
    - name: 指定了没有的
      to:
        - { provider: official, model: claude-opus-5 }
        - { provider: openai, model: gpt-x }
- name: narrow
  rules:
    - name: 指定中转站的叫法
      to:
        - { provider: relay, model: anthropic/claude-sonnet-4-5 }
",
        )
        .await,
    )
    .await;

    // 指定模型原样发出、不经过别名表：中转站自己的 gpt-x
    let (status, body) = ask(gw, "tw-k", "anything").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["by"], "relay");
    assert_eq!(body["sent"], "gpt-x");

    // 指定的模型那一家没有：说是哪条规则、哪一家、哪个名字
    let (status, body) = ask(gw, "missing", "anything").await;
    assert_eq!(status, 400, "{body}");
    assert!(
        message(&body).contains("Rule `指定了没有的` pins models")
            && message(&body).contains("official (claude-opus-5), openai (gpt-x)"),
        "{body}"
    );

    // 密钥不让用指定的那个名字
    let (status, body) = ask(gw, "narrow", "claude-sonnet-4-5").await;
    assert_eq!(status, 400, "{body}");
    assert!(
        message(&body).contains("pins model anthropic/claude-sonnet-4-5 on `relay`")
            && message(&body).contains("`narrow` may not use"),
        "{body}"
    );
}
