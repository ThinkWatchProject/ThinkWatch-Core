//! 手写的模型规格（`providers[].model_specs`）在网关里处处优先于价目表：三种格式的
//! `/v1/models`、单点查询、别名，以及一轮半路输入超出上下文时重新求值。

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::routing::{any, get};
use serde_json::Value;
use tokio::sync::broadcast::Receiver;
use tw_api::Event;
use tw_config::Config;

/// 一个假上游：列出 `models`，什么请求都答一个读了缓存的回答。
async fn upstream(models: &'static [&'static str]) -> SocketAddr {
    let app = Router::new()
        .route(
            "/v1/models",
            get(move || async move {
                axum::Json(serde_json::json!({
                    "data": models.iter().map(|m| serde_json::json!({ "id": m })).collect::<Vec<_>>()
                }))
            }),
        )
        .fallback(any(|| async {
            axum::response::Response::builder()
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    r#"{"type":"message","content":[],"usage":{"input_tokens":3,"cache_read_input_tokens":5000,"output_tokens":1}}"#,
                ))
                .unwrap()
        }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

async fn serve(cfg: Config) -> (SocketAddr, Receiver<Event>) {
    let state = tw_gateway::AppState::new(cfg).unwrap();
    tw_gateway::models::refresh_all(&state).await;
    let events = state.bus.subscribe();
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, events)
}

/// 一家中转站：`claude-sonnet-4-5` 价目表里是 200k，这里手写成 1M、输出上限照旧；
/// `glm-5-air` 价目表里没有，两项都手写。别名 `air` 只列 `glm-5-air`。
async fn relay() -> Config {
    let up = upstream(&["claude-sonnet-4-5", "glm-5-air", "claude-haiku-4-5"]).await;
    serde_yaml_ng::from_str(&format!(
        "version: 1
clients:
  - name: me
    key: tw-k
providers:
  - name: relay
    base_url: http://{up}
    key: sk-x
    protocol: anthropic
    model_specs:
      claude-sonnet-4-5: {{ context_window: 1000000 }}
      glm-5-air: {{ context_window: 128000, max_output_tokens: 16384 }}
aliases:
  air: glm-5-air
"
    ))
    .unwrap()
}

/// `GET path`：`anthropic` 带 `x-api-key` 和版本头，`gemini` 用 Google 的位置，其余 Bearer。
async fn get_json(gw: SocketAddr, path: &str, shape: &str) -> Value {
    let r = reqwest::Client::new().get(format!("http://{gw}{path}"));
    let r = match shape {
        "anthropic" => r
            .header("x-api-key", "tw-k")
            .header("anthropic-version", "2023-06-01"),
        "gemini" => r.header("x-goog-api-key", "tw-k"),
        _ => r.header("authorization", "Bearer tw-k"),
    };
    let r = r.send().await.unwrap();
    assert_eq!(r.status(), 200, "{path}");
    r.json().await.unwrap()
}

fn find<'a>(list: &'a [Value], field: &str, id: &str) -> &'a Value {
    list.iter()
        .find(|m| m[field] == id)
        .unwrap_or_else(|| panic!("{id} 不在列表里：{list:?}"))
}

#[tokio::test]
async fn every_listing_shape_carries_the_spec_written_for_the_upstream() {
    let (gw, _) = serve(relay().await).await;

    let openai = get_json(gw, "/v1/models", "openai").await;
    let data = openai["data"].as_array().unwrap();
    let sonnet = find(data, "id", "claude-sonnet-4-5");
    for f in ["context_window", "context_length", "max_input_tokens"] {
        assert_eq!(sonnet[f], 1_000_000, "{f}: {sonnet}");
    }
    let air = find(data, "id", "glm-5-air");
    assert_eq!(air["context_window"], 128_000, "{air}");
    // 没手写的照旧取价目表
    assert_eq!(
        find(data, "id", "claude-haiku-4-5")["context_window"],
        200_000
    );
    // 别名按服务它的那个模型：中转站手写的那个数
    assert_eq!(find(data, "id", "air")["context_window"], 128_000);

    let anthropic = get_json(gw, "/v1/models", "anthropic").await;
    let data = anthropic["data"].as_array().unwrap();
    let sonnet = find(data, "id", "claude-sonnet-4-5");
    assert_eq!(sonnet["max_input_tokens"], 1_000_000, "{sonnet}");
    assert_eq!(sonnet["supports_1m"], true, "手写到一百万就是 1M：{sonnet}");
    let air = find(data, "id", "glm-5-air");
    assert_eq!(air["max_input_tokens"], 128_000);
    assert_eq!(air["supports_1m"], false);
    assert_eq!(find(data, "id", "air")["max_input_tokens"], 128_000);
    let one = get_json(gw, "/v1/models/claude-sonnet-4-5", "anthropic").await;
    assert_eq!(one, *sonnet, "单点查询和列表里的应该是同一个对象");

    let gemini = get_json(gw, "/v1beta/models", "gemini").await;
    let data = gemini["models"].as_array().unwrap();
    let sonnet = find(data, "name", "models/claude-sonnet-4-5");
    assert_eq!(sonnet["inputTokenLimit"], 1_000_000, "{sonnet}");
    // 输出上限没手写：价目表的
    assert_eq!(sonnet["outputTokenLimit"], 64_000, "{sonnet}");
    let air = find(data, "name", "models/glm-5-air");
    assert_eq!(air["inputTokenLimit"], 128_000, "{air}");
    assert_eq!(air["outputTokenLimit"], 16_384, "{air}");
    let alias = find(data, "name", "models/air");
    assert_eq!(alias["outputTokenLimit"], 16_384, "{alias}");
    let one = get_json(gw, "/v1beta/models/glm-5-air", "gemini").await;
    assert_eq!(one, *air);
}

// ─────────────────────────────────────────────────────────── 管线

/// 输入大的去乙，其余去甲。两家都在同一个假上游上，都列着价目表里没有的 `relay-model`；
/// `window` 给了就是甲手写的上下文窗口
async fn two_upstreams(window: Option<u32>) -> Config {
    let up = upstream(&["relay-model"]).await;
    let specs = window
        .map(|n| format!("    model_specs:\n      relay-model: {{ context_window: {n} }}\n"))
        .unwrap_or_default();
    serde_yaml_ng::from_str(&format!(
        "version: 1
clients:
  - name: me
    key: tw-k
providers:
  - name: 甲
    base_url: http://{up}
    key: sk-x
    protocol: anthropic
{specs}  - name: 乙
    base_url: http://{up}
    key: sk-x
    protocol: anthropic
routes:
  - name: default
    rules:
      - name: 大输入
        when: {{ input_tokens: '>2000' }}
        to: 乙
      - name: 其余
        to: 甲
"
    ))
    .unwrap()
}

/// 发一个请求，交回它的路由事件：规则、实际回答的那一家、这一轮的决定是不是沿用的
async fn ask(gw: SocketAddr, rx: &mut Receiver<Event>, messages: &str) -> (String, String, bool) {
    let body = format!(
        r#"{{"model":"relay-model","max_tokens":16,"system":"你是一个助手","messages":{messages}}}"#
    );
    let st = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .header("x-claude-code-session-id", "会话-1")
        .body(body)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(st, 200);
    let mut routed = None;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("5 秒内没等到结局")
            .expect("事件流断了");
        match ev {
            Event::RequestRouted {
                rule,
                attempts,
                affinity,
                ..
            } => {
                routed = Some((
                    rule,
                    attempts.last().unwrap().provider.clone(),
                    affinity.is_some_and(|a| a.held_route),
                ))
            }
            Event::RequestFinished { .. } => return routed.expect("没有路由事件"),
            Event::RequestFailed { message, .. } => panic!("失败了：{message:?}"),
            _ => {}
        }
    }
}

/// 同一轮里先小后大：第二个请求本该沿用这一轮开头的决定
async fn a_turn_that_grows(cfg: Config) -> (String, String, bool) {
    let (gw, mut rx) = serve(cfg).await;
    let first = r#"{"role":"user","content":"帮我重构"}"#;
    let (rule, to, _) = ask(gw, &mut rx, &format!("[{first}]")).await;
    assert_eq!((rule.as_str(), to.as_str()), ("其余", "甲"));
    let big = "读到的文件内容 ".repeat(1500);
    let tool = format!(
        r#"{{"role":"assistant","content":[{{"type":"tool_use","id":"t1","name":"Read","input":{{}}}}]}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":"{big}"}}]}}"#
    );
    ask(gw, &mut rx, &format!("[{first},{tool}]")).await
}

/// 价目表不认识 `relay-model`，说不出它装不装得下：一轮半路照常沿用。甲手写了 2000 的
/// 上下文窗口，变大的输入装不下了：重新求值，去了乙
#[tokio::test]
async fn a_turn_that_outgrows_the_window_written_by_hand_is_routed_again() {
    let (rule, to, held) = a_turn_that_grows(two_upstreams(None).await).await;
    assert_eq!((rule.as_str(), to.as_str(), held), ("其余", "甲", true));

    let (rule, to, held) = a_turn_that_grows(two_upstreams(Some(2_000)).await).await;
    assert_eq!((rule.as_str(), to.as_str(), held), ("大输入", "乙", false));
}
