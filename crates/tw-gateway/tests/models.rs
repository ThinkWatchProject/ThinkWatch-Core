//! 每个上游提供哪些模型，以及路由怎么用这件事。
//!
//! 假上游会回答 `GET /v1/models`，并在响应里说出自己是谁 —— 断言落在
//! 「请求真的落在了哪一家」上。

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
use tw_config::{Client, Config, Provider};

/// 一个假上游：列出 `models`，只接受这些模型的请求，其余回 404。
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
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                let model = v["model"].as_str().unwrap_or_default().to_string();
                if models.contains(&model.as_str()) {
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(serde_json::json!({ "by": name, "model": model })),
                    )
                } else {
                    // 中转站对它没有的模型就是这么回的：一个 4xx，不会触发故障转移
                    (
                        axum::http::StatusCode::NOT_FOUND,
                        axum::Json(serde_json::json!({
                            "type": "error",
                            "error": { "type": "not_found_error", "message": format!("model: {model}") }
                        })),
                    )
                }
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
        key: "k".into(),
        protocol: Some(tw_config::Protocol::Anthropic),
        ..Default::default()
    }
}

fn config(providers: Vec<Provider>) -> Config {
    Config {
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers,
        ..Default::default()
    }
}

/// 所有上游进一个按顺序尝试的组，默认路由走这个组。
fn grouped(mut cfg: Config, order: &[&str]) -> Config {
    cfg.groups = vec![
        serde_yaml_ng::from_str(&format!(
            "name: pool\ntype: fallback\nproviders: [{}]\n",
            order.join(", ")
        ))
        .unwrap(),
    ];
    cfg.routes = vec![tw_engine::RouteSet::default_with(vec![tw_engine::Rule {
        name: "全部".into(),
        when: Default::default(),
        to: Some("pool".into()),
        set: None,
        deny: None,
        guard: None,
    }])];
    cfg
}

async fn serve(state: tw_gateway::AppState) -> SocketAddr {
    let addr = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    let s = state.clone();
    tokio::spawn(async move { tw_gateway::serve(s, addr).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

async fn ask(gw: SocketAddr, model: &str) -> (u16, serde_json::Value) {
    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(serde_json::json!({ "model": model, "messages": [] }).to_string())
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or_default())
}

async fn listed(gw: SocketAddr) -> Vec<String> {
    let v: serde_json::Value = reqwest::Client::new()
        .get(format!("http://{gw}/v1/models"))
        .header("x-api-key", "tw-k")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect()
}

/// 等到 `/v1/models` 列出 `model`。
async fn until_listed(gw: SocketAddr, model: &str) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if listed(gw).await.iter().any(|m| m == model) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

// ─────────────────────────────────────────────────────────── 改配置之后

#[tokio::test]
async fn adding_an_upstream_keeps_what_was_discovered_and_lists_the_new_one_promptly() {
    let a = upstream("a", &["a-model"]).await;
    let b = upstream("b", &["b-model"]).await;
    // 手写清单的那一家：让目录在改配置之后不是空的（空目录不做准入检查）
    let mut manual = provider("manual", a);
    manual.models = vec!["manual-model".into()];
    let cfg = config(vec![provider("a", a), manual]);
    let state = tw_gateway::AppState::new(cfg.clone()).unwrap();
    tw_gateway::models::spawn(state.clone());
    let gw = serve(state.clone()).await;
    assert!(until_listed(gw, "a-model").await, "启动之后没探到 a 的模型");

    let mut next = cfg.clone();
    next.providers.push(provider("b", b));
    state.reload(next).unwrap();

    // 已经探到的模型不能因为别处改了配置就消失
    let (status, body) = ask(gw, "a-model").await;
    assert_eq!(status, 200, "改配置之后 a 的模型被拒了：{body}");
    // 新加的上游不用等到明天
    assert!(until_listed(gw, "b-model").await, "{:?}", listed(gw).await);
    let (status, body) = ask(gw, "b-model").await;
    assert_eq!(status, 200, "{body}");
}

// ─────────────────────────────────────────────────────────── 路由

#[tokio::test]
async fn a_group_skips_an_upstream_that_does_not_offer_the_model() {
    // relay 排在前面，但它没有这个模型。它回的 404 不会触发故障转移，
    // 所以不跳过它的话，这个请求在一家能服务它的上游就在旁边时失败
    let relay = upstream("relay", &["shared"]).await;
    let official = upstream("official", &["shared", "official-only"]).await;
    let cfg = grouped(
        config(vec![
            provider("relay", relay),
            provider("official", official),
        ]),
        &["relay", "official"],
    );
    let state = tw_gateway::AppState::new(cfg).unwrap();
    tw_gateway::models::refresh_all(&state).await;
    let gw = serve(state).await;

    let (status, body) = ask(gw, "official-only").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["by"], "official");
    // 两家都有的还是按组里的顺序
    let (_, body) = ask(gw, "shared").await;
    assert_eq!(body["by"], "relay");
}

#[tokio::test]
async fn a_disabled_upstream_takes_no_requests_and_lists_nothing() {
    let relay = upstream("relay", &["shared", "relay-only"]).await;
    let official = upstream("official", &["shared"]).await;
    let mut cfg = grouped(
        config(vec![
            provider("relay", relay),
            provider("official", official),
        ]),
        &["relay", "official"],
    );
    cfg.providers[0].disabled = true;
    let state = tw_gateway::AppState::new(cfg.clone()).unwrap();
    tw_gateway::models::refresh_all(&state).await;
    let gw = serve(state.clone()).await;

    let (status, body) = ask(gw, "shared").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["by"], "official");
    assert_eq!(listed(gw).await, ["shared"], "停用的上游的模型还在列表里");

    // 组里的都停了：说清是配置的事，而不是请求写错了
    let mut all_off = cfg.clone();
    all_off.providers[1].disabled = true;
    state.reload(all_off).unwrap();
    let (status, body) = ask(gw, "shared").await;
    assert_eq!(status, 500, "{body}");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("已停用") && msg.contains("official"), "{msg}");
}

#[tokio::test]
async fn a_model_outside_an_upstreams_scope_is_not_listed_or_sent_there() {
    let relay = upstream("relay", &["scoped", "other"]).await;
    let official = upstream("official", &["other"]).await;
    let mut cfg = grouped(
        config(vec![
            provider("relay", relay),
            provider("official", official),
        ]),
        &["relay", "official"],
    );
    cfg.providers[0].models_only = Some(vec!["scop*".into()]);
    let state = tw_gateway::AppState::new(cfg).unwrap();
    tw_gateway::models::refresh_all(&state).await;
    let gw = serve(state).await;

    assert_eq!(listed(gw).await, ["other", "scoped"]);
    let (_, body) = ask(gw, "scoped").await;
    assert_eq!(body["by"], "relay");
    // relay 也有 other，但不在它的范围里
    let (_, body) = ask(gw, "other").await;
    assert_eq!(body["by"], "official");
}

#[tokio::test]
async fn when_no_candidate_serves_the_model_the_error_names_each_one_and_why() {
    let relay = upstream("relay", &["relay-only"]).await;
    let official = upstream("official", &["official-only"]).await;
    let mut cfg = config(vec![
        provider("relay", relay),
        provider("official", official),
    ]);
    // 这条规则只把请求交给 relay
    cfg.routes = vec![tw_engine::RouteSet::default_with(vec![tw_engine::Rule {
        name: "都给 relay".into(),
        when: Default::default(),
        to: Some("relay".into()),
        set: None,
        deny: None,
        guard: None,
    }])];
    let state = tw_gateway::AppState::new(cfg).unwrap();
    tw_gateway::models::refresh_all(&state).await;
    let gw = serve(state).await;

    let (status, body) = ask(gw, "official-only").await;
    assert_eq!(status, 400, "{body}");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("official-only") && msg.contains("relay 不提供该模型"),
        "{msg}"
    );

    // 没有任何上游提供的模型：准入就拦下，而不是说客户端不允许
    let (status, body) = ask(gw, "nobody-has-this").await;
    assert_eq!(status, 400, "{body}");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("没有上游提供模型"), "{msg}");
}
