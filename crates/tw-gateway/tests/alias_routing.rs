//! 别名、规则改写、指定模型：每一家实际收到的模型名。
//!
//! 起真网关、真上游：每个上游记下收到的请求体，断言落在「每一家收到的是哪个名字」和
//! 尝试链每一跳记下的模型上。上游都不给模型清单，名字靠启用范围（`models_only`）区分
//! —— 没有清单的上游当作什么都有，别名对到它启用范围里的第一个名称。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::routing::post;
use tokio::sync::broadcast::Receiver;
use tw_api::Event;
use tw_config::{Client, Config, Protocol, Provider};

/// 一个上游收到过的请求体
type Seen = Arc<Mutex<Vec<serde_json::Value>>>;

/// 一个上游：记下收到的请求体，一律回 `status`，回答里写着自己是谁、收到的模型名。
async fn upstream(name: &'static str, status: u16) -> (SocketAddr, Seen) {
    let seen: Seen = Default::default();
    let log = seen.clone();
    let app = Router::new().route(
        "/v1/messages",
        post(move |body: axum::body::Bytes| {
            let log = log.clone();
            async move {
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                let sent = v["model"].clone();
                log.lock().unwrap().push(v);
                (
                    axum::http::StatusCode::from_u16(status).unwrap(),
                    axum::Json(serde_json::json!({ "by": name, "sent": sent })),
                )
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (a, seen)
}

/// 一个上游，只用 `scope` 里的模型（空的就是全部）
fn provider(name: &str, at: SocketAddr, scope: &[&str]) -> Provider {
    Provider {
        name: name.into(),
        base_url: format!("http://{at}"),
        key: Some("k".into()),
        protocol: Some(Protocol::Anthropic),
        models_only: (!scope.is_empty()).then(|| scope.iter().map(|s| s.to_string()).collect()),
        ..Default::default()
    }
}

/// `opus` 在官方叫 `claude-opus-5`，在 Bedrock 叫 `us.anthropic.claude-opus-5-v1:0`
const OPUS: &str = "opus: [claude-opus-5, us.anthropic.claude-opus-5-v1:0]\n";

/// `routes` 是默认路由的规则（YAML 列表）；空的就是只配上游
fn config(providers: Vec<Provider>, aliases: &str, rules: &str) -> Config {
    let mut c = Config {
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers,
        aliases: serde_yaml_ng::from_str(aliases).unwrap(),
        ..Default::default()
    };
    if !rules.is_empty() {
        c.routes = vec![tw_engine::RouteSet::default_with(
            serde_yaml_ng::from_str(rules).unwrap(),
        )];
    }
    // 规则写得对（指定模型、阶段二不带去向……）
    c.engine().validate().unwrap();
    c
}

async fn serve(cfg: Config) -> (SocketAddr, Receiver<Event>) {
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let events = state.bus.subscribe();
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, events)
}

async fn ask(gw: SocketAddr, model: &str) -> (u16, serde_json::Value) {
    let r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(serde_json::json!({ "model": model, "max_tokens": 1000, "messages": [] }).to_string())
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or_default())
}

/// 这个请求的尝试链：每一跳的上游和记下的模型（和客户端写的一样时不记）
async fn attempts(rx: &mut Receiver<Event>) -> Vec<(String, Option<String>)> {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("5 秒内没等到路由事件")
            .expect("事件流断了");
        if let Event::RequestRouted { attempts, .. } = ev {
            return attempts
                .into_iter()
                .map(|a| (a.provider, a.model))
                .collect();
        }
    }
}

/// 一个上游收到的模型名，按收到的顺序
fn models(seen: &Seen) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|v| v["model"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn some(s: &str) -> Option<String> {
    Some(s.to_string())
}

/// 请求别名：每一家发它自己的名字，故障转移到下一家时重新对。别名列的名字一个都没有的
/// 那一家跳过，什么都收不到
#[tokio::test]
async fn an_alias_fails_over_and_each_upstream_gets_its_own_name() {
    let (zhipu, zhipu_seen) = upstream("zhipu", 200).await;
    let (bedrock, bedrock_seen) = upstream("bedrock", 500).await;
    let (official, official_seen) = upstream("official", 200).await;
    let cfg = config(
        vec![
            provider("zhipu", zhipu, &["glm-*"]),
            provider("bedrock", bedrock, &["us.anthropic.*"]),
            provider("official", official, &["claude-*"]),
        ],
        OPUS,
        "",
    );
    let (gw, mut rx) = serve(cfg).await;

    let (status, body) = ask(gw, "opus").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["by"], "official");
    assert_eq!(body["sent"], "claude-opus-5");
    assert!(models(&zhipu_seen).is_empty(), "智谱服务不了这个别名");
    assert_eq!(models(&bedrock_seen), ["us.anthropic.claude-opus-5-v1:0"]);
    assert_eq!(models(&official_seen), ["claude-opus-5"]);
    // 尝试链每一跳记的是发给那一家的名字
    assert_eq!(
        attempts(&mut rx).await,
        [
            (
                "bedrock".to_string(),
                some("us.anthropic.claude-opus-5-v1:0")
            ),
            ("official".to_string(), some("claude-opus-5")),
        ]
    );
}

/// 规则把模型改成一个别名：照常按别名表对到每一家
#[tokio::test]
async fn a_rule_can_rewrite_the_model_to_an_alias() {
    let (bedrock, bedrock_seen) = upstream("bedrock", 500).await;
    let (official, official_seen) = upstream("official", 200).await;
    let cfg = config(
        vec![
            provider("bedrock", bedrock, &["us.anthropic.*"]),
            provider("official", official, &["claude-*"]),
        ],
        OPUS,
        "
- { name: Sonnet 换成 Opus, when: { model: claude-sonnet-* }, set: { model: opus } }
- { name: 兜底, to: __all__ }
",
    );
    let (gw, mut rx) = serve(cfg).await;

    let (status, body) = ask(gw, "claude-sonnet-5").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["by"], "official");
    assert_eq!(models(&bedrock_seen), ["us.anthropic.claude-opus-5-v1:0"]);
    assert_eq!(models(&official_seen), ["claude-opus-5"]);
    assert_eq!(
        attempts(&mut rx).await,
        [
            (
                "bedrock".to_string(),
                some("us.anthropic.claude-opus-5-v1:0")
            ),
            ("official".to_string(), some("claude-opus-5")),
        ]
    );
}

/// 指定模型：按列表的顺序（不是上游声明的顺序）故障转移，每一家原样发指定的名字。阶段二
/// 的改名盖不过它，别的改写照常
#[tokio::test]
async fn pinned_models_fail_over_in_list_order_and_go_out_as_written() {
    let (bedrock, bedrock_seen) = upstream("bedrock", 200).await;
    let (official, official_seen) = upstream("official", 500).await;
    let cfg = config(
        vec![
            provider("bedrock", bedrock, &[]),
            provider("official", official, &[]),
        ],
        OPUS,
        "
- { name: 降上限, set: { max_tokens: 64 } }
- name: 指定
  to:
    - { provider: official, model: claude-opus-5 }
    - { provider: bedrock, model: us.anthropic.claude-opus-5-v1:0 }
- { name: Bedrock 改名, when: { provider_would_be: bedrock }, set: { model: something-else } }
",
    );
    let (gw, mut rx) = serve(cfg).await;

    let (status, body) = ask(gw, "opus").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["by"], "bedrock");
    assert_eq!(body["sent"], "us.anthropic.claude-opus-5-v1:0");
    assert_eq!(models(&official_seen), ["claude-opus-5"]);
    assert_eq!(models(&bedrock_seen), ["us.anthropic.claude-opus-5-v1:0"]);
    // 别的规则改写的输出上限照常
    for seen in [&official_seen, &bedrock_seen] {
        assert_eq!(seen.lock().unwrap()[0]["max_tokens"], 64);
    }
    assert_eq!(
        attempts(&mut rx).await,
        [
            ("official".to_string(), some("claude-opus-5")),
            (
                "bedrock".to_string(),
                some("us.anthropic.claude-opus-5-v1:0")
            ),
        ]
    );
}

/// 阶段二（选定上游之后）改的名字原样发出：恰好是别名也不对 —— 写它的时候已经知道是
/// 哪一家
#[tokio::test]
async fn a_phase_two_rename_goes_out_as_written() {
    let (relay, relay_seen) = upstream("relay", 200).await;
    let cfg = config(
        vec![provider("relay", relay, &[])],
        OPUS,
        "
- { name: 中转的叫法, when: { provider_would_be: relay }, set: { model: opus } }
- { name: 兜底, to: __all__ }
",
    );
    let (gw, mut rx) = serve(cfg).await;

    let (status, body) = ask(gw, "claude-sonnet-5").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(models(&relay_seen), ["opus"]);
    assert_eq!(
        attempts(&mut rx).await,
        [("relay".to_string(), some("opus"))]
    );

    // 同一个别名换成阶段一改写：按别名表对，发的是这一家的名称
    let (relay, relay_seen) = upstream("relay", 200).await;
    let cfg = config(
        vec![provider("relay", relay, &[])],
        OPUS,
        "
- { name: 改成别名, set: { model: opus } }
- { name: 兜底, to: __all__ }
",
    );
    let (gw, _rx) = serve(cfg).await;
    let (status, body) = ask(gw, "claude-sonnet-5").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(models(&relay_seen), ["claude-opus-5"]);
}

/// 写上游模型名的规则条件也管到列了它的别名：请求别名，命中的是写 Bedrock 名字的那条
#[tokio::test]
async fn a_condition_on_a_real_name_routes_its_alias() {
    let (bedrock, bedrock_seen) = upstream("bedrock", 200).await;
    let (official, official_seen) = upstream("official", 200).await;
    let cfg = config(
        vec![
            provider("bedrock", bedrock, &[]),
            provider("official", official, &[]),
        ],
        "opus: [us.anthropic.claude-opus-5-v1:0, claude-opus-5]\n",
        "
- { name: Bedrock 写法, when: { model: 'us.anthropic.*' }, to: bedrock }
- { name: 兜底, to: official }
",
    );
    let (gw, _rx) = serve(cfg).await;

    let (status, body) = ask(gw, "opus").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["by"], "bedrock");
    assert_eq!(models(&bedrock_seen), ["us.anthropic.claude-opus-5-v1:0"]);
    // 请求真名不受写别名的规则影响；这里请求的官方写法也对不上 Bedrock 那条
    let (status, body) = ask(gw, "claude-opus-5").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["by"], "official");
    assert_eq!(models(&official_seen), ["claude-opus-5"]);
}
