//! 端到端：起一个假上游，起网关，用 Claude Code 会发的那种请求打过去。
//!
//! 这些测试存在的理由很具体 —— M0 的验收标准是「Claude Code 指向本地能
//! 正常干活，含流式和工具调用」。单元测试证明不了这件事，因为它的失败
//! 模式全在接缝上：头被剔错、body 被动过、流被缓冲。

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use tw_config::{Client, Config, Listen, Provider};

/// 假上游收到的东西，测试拿它来断言。
#[derive(Default, Debug)]
struct Seen {
    headers: HeaderMap,
    body: Vec<u8>,
}

async fn start_upstream(sse: bool) -> (SocketAddr, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let s = seen.clone();
    let app = Router::new()
        .route(
            "/v1/messages",
            post(move |State(s): State<Arc<Mutex<Seen>>>, headers: HeaderMap, body: bytes::Bytes| async move {
                {
                    let mut g = s.lock().unwrap();
                    g.headers = headers;
                    g.body = body.to_vec();
                }
                if sse {
                    // 分三帧发，这样「有没有被整块缓冲」是可观测的。
                    axum::response::Response::builder()
                        .header("content-type", "text/event-stream")
                        .header("anthropic-ratelimit-unified-5h-utilization", "0.62")
                        .body(axum::body::Body::from(
                            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n\
                             event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\n\
                             event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                        ))
                        .unwrap()
                } else {
                    axum::response::Response::builder()
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(r#"{"type":"message","id":"msg_01"}"#))
                        .unwrap()
                }
            }),
        )
        .with_state(s);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, seen)
}

async fn start_gateway(upstream: SocketAddr) -> SocketAddr {
    let cfg = Config {
        retention: Default::default(),
        default_route: None,
        default_key: None,
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "claude-code".into(),
            key: "tw-testkey".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "mock".into(),
            base_url: format!("http://{upstream}"),
            key: Some("sk-upstream-secret".into()),
            protocol: Some(tw_config::Protocol::Anthropic),
            ..Default::default()
        }],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    // 先 bind 拿端口，再放掉让 serve 自己 bind —— serve 需要自己建
    // 监听器才能带上 connect info（对端地址）。
    let addr = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, addr).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

/// Claude Code 真会发的那种 body，带工具定义。
const CLAUDE_BODY: &str = r#"{"model":"claude-sonnet-4-5","max_tokens":1024,"messages":[{"role":"user","content":"hi"}],"tools":[{"name":"Read","input_schema":{"type":"object","properties":{"file_path":{"type":"string"}}}}],"stream":true}"#;

#[tokio::test]
async fn a_claude_code_request_reaches_the_upstream_byte_for_byte() {
    let (up, seen) = start_upstream(false).await;
    let gw = start_gateway(up).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-testkey")
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(CLAUDE_BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let g = seen.lock().unwrap();
    // 出站直通：请求体一个字节都不能变。工具定义、schema、嵌套对象，
    // 任何重新序列化都可能改动它 —— 而那正是缓存杀手。
    assert_eq!(String::from_utf8_lossy(&g.body), CLAUDE_BODY);
    // 方言头必须转过去，否则上游不认
    assert_eq!(g.headers.get("anthropic-version").unwrap(), "2023-06-01");
}

#[tokio::test]
async fn the_gateway_key_is_swapped_for_the_real_one_and_never_leaks_upstream() {
    let (up, seen) = start_upstream(false).await;
    let gw = start_gateway(up).await;

    reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-testkey")
        .body(CLAUDE_BODY)
        .send()
        .await
        .unwrap();

    let g = seen.lock().unwrap();
    // 上游拿到的是它自己的 key
    assert_eq!(g.headers.get("x-api-key").unwrap(), "sk-upstream-secret");
    // 我们的网关密钥一个字节都不能出现在发给中转站的请求里
    let all: String = g
        .headers
        .iter()
        .map(|(k, v)| format!("{k}:{}", v.to_str().unwrap_or("")))
        .collect();
    assert!(!all.contains("tw-testkey"), "网关密钥泄漏给了上游：{all}");
}

#[tokio::test]
async fn streaming_is_not_buffered_and_upstream_headers_survive() {
    let (up, _) = start_upstream(true).await;
    let gw = start_gateway(up).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-testkey")
        .body(CLAUDE_BODY)
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    // 订阅额度的头必须原样回来 —— 那是零成本白捡的数据来源，
    // 剔掉它等于把那个功能的输入掐了。
    assert_eq!(
        resp.headers()
            .get("anthropic-ratelimit-unified-5h-utilization")
            .unwrap(),
        "0.62"
    );
    // content-length 不能跟着来：我们重新分块了，它对不上
    assert!(resp.headers().get("content-length").is_none());

    let text = resp.text().await.unwrap();
    assert!(text.contains("message_start"));
    assert!(text.contains("message_stop"));
}

#[tokio::test]
async fn a_request_without_a_key_is_refused_before_anything_reaches_the_upstream() {
    let (up, seen) = start_upstream(false).await;
    let gw = start_gateway(up).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .body(CLAUDE_BODY)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
    assert_eq!(resp.headers().get("x-thinkwatch-error").unwrap(), "auth");
    let body: serde_json::Value = resp.json().await.unwrap();
    // 客户端解析的是自己方言的错误结构
    assert_eq!(body["error"]["type"], "authentication_error");
    // 一眼看出是哪一层拒的
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("[ThinkWatch]")
    );
    // 而且上游根本没被打扰
    assert!(seen.lock().unwrap().body.is_empty());
}

#[tokio::test]
async fn a_wrong_key_is_refused_and_the_upstream_key_is_not_burned() {
    let (up, seen) = start_upstream(false).await;
    let gw = start_gateway(up).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-wrong")
        .body(CLAUDE_BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    // 一个探测本机端口的脚本不该消耗掉用户的额度
    assert!(seen.lock().unwrap().body.is_empty());
}

#[tokio::test]
async fn healthz_needs_no_key() {
    let (up, _) = start_upstream(false).await;
    let gw = start_gateway(up).await;
    let r = reqwest::get(format!("http://{gw}/healthz")).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn a_request_emits_the_four_lifecycle_events_in_order() {
    // UI 的实时列表靠这四个事件缝成一行。缺了 RequestHeaders 那条，
    // 一个跑六分钟的流式请求在列表里要六分钟后才出现。
    let (up, _) = start_upstream(true).await;
    let cfg = Config {
        retention: Default::default(),
        default_route: None,
        default_key: None,
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "claude-code".into(),
            key: "tw-testkey".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "mock".into(),
            base_url: format!("http://{up}"),
            key: Some("sk-x".into()),
            protocol: Some(tw_config::Protocol::Anthropic),
            ..Default::default()
        }],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let mut rx = state.bus.subscribe();
    // 先 bind 拿端口，再放掉让 serve 自己 bind —— serve 需要自己建
    // 监听器才能带上 connect info（对端地址）。
    let gw = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, gw).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-testkey")
        .body(CLAUDE_BODY)
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.contains("message_stop"));

    let started = next_lifecycle(&mut rx).await;
    assert!(
        matches!(started, tw_api::Event::RequestStarted { ref client, .. } if client == "claude-code")
    );
    let headers = next_lifecycle(&mut rx).await;
    // 断言写成 match 而不是 matches!，这样失败时能看见实际收到了什么。
    match headers {
        tw_api::Event::RequestHeaders { status: 200, .. } => {}
        ref other => panic!("第二条该是 RequestHeaders(200)，实际 {other:?}"),
    }
    let finished = next_lifecycle(&mut rx).await;
    match finished {
        tw_api::Event::RequestFinished { status, bytes, .. } => {
            assert_eq!(status, 200);
            // 字节数是流真正流过的量，不是 content-length
            assert!(bytes > 0, "应该数到流过的字节");
        }
        ref other => panic!("最后一条该是 finished，实际 {other:?}"),
    }
    // 四个事件共用同一个 id
    assert_eq!(started.id(), headers.id());
    assert_eq!(started.id(), finished.id());
}

/// 下一条路由事件里那家的计费方式。
async fn next_billing(rx: &mut tokio::sync::broadcast::Receiver<tw_api::Event>) -> String {
    for _ in 0..8 {
        if let Ok(Ok(tw_api::Event::RequestRouted { billing, .. })) =
            tokio::time::timeout(Duration::from_secs(2), rx.recv()).await
        {
            return billing;
        }
    }
    panic!("没等到路由事件");
}

/// 下一条**生命周期**事件（开始 / 响应头 / 结束 / 失败 / 取消）。
///
/// 观测类的事件（路由链、订阅额度、密钥发现、配置变更）会插在它们中间，
/// 而且**以后还会更多** —— 每加一个就去改一遍这些测试是错的做法：那些
/// 测试断言的是「四个生命周期事件按序到达」，不是「总线上只有它们」。
async fn next_lifecycle(rx: &mut tokio::sync::broadcast::Receiver<tw_api::Event>) -> tw_api::Event {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("5 秒内没等到事件")
            .expect("事件流断了");
        if matches!(
            ev,
            tw_api::Event::RequestStarted { .. }
                | tw_api::Event::RequestHeaders { .. }
                | tw_api::Event::RequestFinished { .. }
                | tw_api::Event::RequestFailed { .. }
                | tw_api::Event::RequestCancelled { .. }
        ) {
            return ev;
        }
    }
}

/// 每一步都套一个超时。**卡住的测试比失败的测试更糟** —— CI 只会报一个
/// 超时，不告诉你卡在哪一行。
#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_upstream_emits_a_failure_event_and_a_502() {
    // 绑一个端口再立刻放掉，这样能拿到一个**确定没人在听**的端口。
    // 不要写死一个「大概没人用」的端口号：低位端口在 macOS 上可能被
    // 防火墙黑洞掉，表现为连接挂住十秒而不是立刻被拒。
    let dead_port = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let cfg = Config {
        retention: Default::default(),
        default_route: None,
        default_key: None,
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "dead".into(),
            base_url: format!("http://127.0.0.1:{dead_port}"),
            key: Some("sk-x".into()),
            protocol: None,
            ..Default::default()
        }],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let mut rx = state.bus.subscribe();
    // 先 bind 拿端口，再放掉让 serve 自己 bind —— serve 需要自己建
    // 监听器才能带上 connect info（对端地址）。
    let gw = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, gw).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        reqwest::Client::new()
            .post(format!("http://{gw}/v1/messages"))
            .header("x-api-key", "tw-k")
            .body("{}")
            .send(),
    )
    .await
    .expect("网关在 20 秒内没回话 —— 连不上上游时它必须立刻返回 502，而不是挂着")
    .unwrap();
    // 502 而不是 500 —— 说清楚是上游那边，不是我们
    assert_eq!(resp.status(), 502);

    let _started = next_lifecycle(&mut rx).await;
    match next_lifecycle(&mut rx).await {
        tw_api::Event::RequestFailed {
            source, message, ..
        } => {
            assert_eq!(source, "upstream");
            // 错误信息要能直接行动
            assert!(
                message.text.contains("endpoint address") || message.text.contains("proxy"),
                "{message}"
            );
        }
        other => panic!("该是 failed，实际 {other:?}"),
    }
}

#[tokio::test]
async fn a_rule_sends_opus_to_one_upstream_and_everything_else_to_another() {
    // 路由的最小可信证明：**两家上游各自能说出自己是谁**，然后看请求
    // 真的落在了规则说的那家。只断言「没报错」证明不了任何事。
    let (a, seen_a) = start_upstream(false).await;
    let (b, seen_b) = start_upstream(false).await;

    let cfg = Config {
        retention: Default::default(),
        default_route: None,
        default_key: None,
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "claude-code".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![
            Provider {
                name: "official".into(),
                base_url: format!("http://{a}"),
                key: Some("sk-official".into()),
                protocol: Some(tw_config::Protocol::Anthropic),
                ..Default::default()
            },
            Provider {
                name: "relay".into(),
                base_url: format!("http://{b}"),
                key: Some("sk-relay".into()),
                protocol: Some(tw_config::Protocol::Anthropic),
                ..Default::default()
            },
        ],
        groups: Vec::new(),
        proxies: Vec::new(),
        pricing: Default::default(),
        client_probes: Default::default(),
        security: Default::default(),
        routes: vec![tw_engine::RouteSet::default_with(vec![
            tw_engine::Rule {
                name: "opus 走官方".into(),
                when: serde_yaml_ng::from_str("{ model: claude-opus-* }").unwrap(),
                to: Some("official".into()),
                set: None,
                deny: None,
            },
            tw_engine::Rule {
                name: "兜底".into(),
                when: Default::default(),
                to: Some("relay".into()),
                set: None,
                deny: None,
            },
        ])],
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    // 先 bind 拿端口，再放掉让 serve 自己 bind —— serve 需要自己建
    // 监听器才能带上 connect info（对端地址）。
    let gw = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, gw).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let send = |model: &str| {
        let body =
            format!(r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}}]}}"#);
        async move {
            reqwest::Client::new()
                .post(format!("http://{gw}/v1/messages"))
                .header("x-api-key", "tw-k")
                .body(body)
                .send()
                .await
                .unwrap()
        }
    };

    send("claude-opus-4-5").await;
    assert!(
        !seen_a.lock().unwrap().body.is_empty(),
        "opus 该落在 official"
    );
    assert!(
        seen_b.lock().unwrap().body.is_empty(),
        "opus 不该落在 relay"
    );

    send("claude-sonnet-4-5").await;
    assert!(
        !seen_b.lock().unwrap().body.is_empty(),
        "sonnet 该走兜底到 relay"
    );

    // 每家收到的是它自己的 key，不是对方的
    assert_eq!(
        seen_a.lock().unwrap().headers.get("x-api-key").unwrap(),
        "sk-official"
    );
    assert_eq!(
        seen_b.lock().unwrap().headers.get("x-api-key").unwrap(),
        "sk-relay"
    );
}

#[tokio::test]
async fn with_no_routes_at_all_requests_still_go_somewhere() {
    // 层 0：只配 provider，不写任何规则。这是最小可用配置，
    // 而且**对不少人就够了** —— 如果它不工作，「配一个 API 就能用」
    // 那条纪律就是假的。
    let (up, seen) = start_upstream(false).await;
    let cfg = Config {
        retention: Default::default(),
        default_route: None,
        default_key: None,
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "only".into(),
            base_url: format!("http://{up}"),
            key: Some("sk-x".into()),
            protocol: None,
            ..Default::default()
        }],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    // 先 bind 拿端口，再放掉让 serve 自己 bind —— serve 需要自己建
    // 监听器才能带上 connect info（对端地址）。
    let gw = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, gw).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"anything"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(!seen.lock().unwrap().body.is_empty());
}

/// 一个总是返回给定状态码的上游。
async fn start_broken_upstream(status: u16) -> SocketAddr {
    let app = Router::new().fallback(axum::routing::any(move || async move {
        axum::http::StatusCode::from_u16(status).unwrap()
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

fn cfg_with(providers: Vec<Provider>, routes: Vec<tw_engine::Rule>) -> Config {
    Config {
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers,
        routes: if routes.is_empty() {
            Vec::new()
        } else {
            vec![tw_engine::RouteSet::default_with(routes)]
        },
        ..Default::default()
    }
}

async fn serve_cfg(cfg: Config) -> SocketAddr {
    let state = tw_gateway::AppState::new(cfg).unwrap();
    // 先 bind 拿端口，再放掉让 serve 自己 bind —— serve 需要自己建
    // 监听器才能带上 connect info（对端地址）。
    let addr = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, addr).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

async fn send_to(gw: SocketAddr) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"claude-sonnet-4-5"}"#)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_dead_first_provider_fails_over_to_the_next_one() {
    // 首字节之前可以透明切换 —— 拿到响应头之前我们还没往客户端
    // 写过任何东西，换一家客户端完全无感。
    let dead = start_broken_upstream(503).await;
    let (good, seen) = start_upstream(false).await;
    let gw = serve_cfg(cfg_with(
        vec![
            Provider {
                name: "dead".into(),
                base_url: format!("http://{dead}"),
                key: Some("k".into()),
                ..Default::default()
            },
            Provider {
                name: "good".into(),
                base_url: format!("http://{good}"),
                key: Some("k".into()),
                ..Default::default()
            },
        ],
        vec![],
    ))
    .await;

    let r = send_to(gw).await;
    assert_eq!(r.status(), 200, "客户端应该完全看不出发生过故障转移");
    assert!(
        !seen.lock().unwrap().body.is_empty(),
        "请求最终落在了第二家"
    );
}

#[tokio::test]
async fn a_client_error_does_not_burn_the_other_providers() {
    // **4xx 不换家**（429 除外）。请求本身有问题的话，换一家也一样被拒，
    // 还会白白污染那家的健康度 —— 而那家可能完全是好的。
    let bad_request = start_broken_upstream(400).await;
    let (backup, seen) = start_upstream(false).await;
    let gw = serve_cfg(cfg_with(
        vec![
            Provider {
                name: "first".into(),
                base_url: format!("http://{bad_request}"),
                key: Some("k".into()),
                ..Default::default()
            },
            Provider {
                name: "backup".into(),
                base_url: format!("http://{backup}"),
                key: Some("k".into()),
                ..Default::default()
            },
        ],
        vec![],
    ))
    .await;

    let r = send_to(gw).await;
    assert_eq!(r.status(), 400, "400 原样回给客户端");
    assert!(seen.lock().unwrap().body.is_empty(), "第二家不该被打扰");
}

#[tokio::test]
async fn rate_limiting_does_fail_over_because_another_account_may_have_quota() {
    // 429 和别的 4xx 不一样：另一家可能有不同的额度。
    let limited = start_broken_upstream(429).await;
    let (good, seen) = start_upstream(false).await;
    let gw = serve_cfg(cfg_with(
        vec![
            Provider {
                name: "limited".into(),
                base_url: format!("http://{limited}"),
                key: Some("k".into()),
                ..Default::default()
            },
            Provider {
                name: "good".into(),
                base_url: format!("http://{good}"),
                key: Some("k".into()),
                ..Default::default()
            },
        ],
        vec![],
    ))
    .await;
    assert_eq!(send_to(gw).await.status(), 200);
    assert!(!seen.lock().unwrap().body.is_empty());
}

#[tokio::test]
async fn the_only_provider_keeps_being_tried_no_matter_how_broken() {
    // 唯一的上游被自己熔断就把用户锁死了。没有别的家可切的时候，
    // 熔断纯粹是自伤。
    let dead = start_broken_upstream(503).await;
    let gw = serve_cfg(cfg_with(
        vec![Provider {
            name: "only".into(),
            base_url: format!("http://{dead}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![],
    ))
    .await;
    // 打满熔断阈值之后再打一次，仍然应该是「上游的错误」而不是
    // 「我们编的无可用上游」
    for _ in 0..5 {
        send_to(gw).await;
    }
    let r = send_to(gw).await;
    assert_eq!(r.headers().get("x-thinkwatch-error").unwrap(), "upstream");
}

#[tokio::test]
async fn everything_broken_still_tries_rather_than_refusing() {
    // fail-open：宁可放行到一个可能坏的上游让用户看见真实错误，也不要
    // 返回一个我们自己编的「无可用上游」—— 后者会让用户以为是我们坏了。
    let a = start_broken_upstream(503).await;
    let b = start_broken_upstream(503).await;
    let gw = serve_cfg(cfg_with(
        vec![
            Provider {
                name: "a".into(),
                base_url: format!("http://{a}"),
                key: Some("k".into()),
                ..Default::default()
            },
            Provider {
                name: "b".into(),
                base_url: format!("http://{b}"),
                key: Some("k".into()),
                ..Default::default()
            },
        ],
        vec![],
    ))
    .await;
    for _ in 0..6 {
        send_to(gw).await;
    }
    let r = send_to(gw).await;
    assert_eq!(r.status(), 502);
    let body: serde_json::Value = r.json().await.unwrap();
    let msg = body["error"]["message"].as_str().unwrap();
    // 错误里要能看出「试过谁」—— 用户能看见故障转移在替他工作，
    // 这是信任的来源。
    assert!(msg.contains("tried:"), "{msg}");
}

/// 一个慢上游：每个请求要 `delay`，用来观察并发行为。
async fn start_slow_upstream(delay: Duration) -> (SocketAddr, Arc<AtomicUsize>) {
    let inflight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let (i2, p2) = (inflight.clone(), peak.clone());
    let app = Router::new().fallback(axum::routing::any(move || {
        let (i, p) = (i2.clone(), p2.clone());
        async move {
            let now = i.fetch_add(1, Ordering::SeqCst) + 1;
            p.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(delay).await;
            i.fetch_sub(1, Ordering::SeqCst);
            axum::Json(serde_json::json!({"ok": true}))
        }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, peak)
}

fn cfg_slow(up: SocketAddr) -> Config {
    Config {
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "slow".into(),
            base_url: format!("http://{up}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        ..Default::default()
    }
}

async fn send_many(gw: SocketAddr, n: usize) -> Vec<u16> {
    let tasks: Vec<_> = (0..n)
        .map(|_| tokio::spawn(async move { send_to(gw).await.status().as_u16() }))
        .collect();
    futures::future::join_all(tasks)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_gateway_itself_puts_no_ceiling_on_concurrent_requests() {
    // 没有全局上限、没有单个上游的上限：一台电脑上几个客户端各开几个
    // 会话，本来就该同时跑。十二个同时发，上游就同时收到十二个
    let (up, peak) = start_slow_upstream(Duration::from_millis(500)).await;
    let gw = serve_cfg(cfg_slow(up)).await;

    let codes = send_many(gw, 12).await;
    assert!(codes.iter().all(|&c| c == 200), "全部该成功：{codes:?}");
    assert_eq!(peak.load(Ordering::SeqCst), 12, "有什么在限流");
}

#[tokio::test(flavor = "multi_thread")]
async fn over_a_keys_limit_requests_wait_instead_of_being_refused() {
    // **这是那条的可信证明。**客户端收到 429 通常不会优雅重试，
    // 一个本来只需要多等两秒的请求会变成一次任务中断 —— 所以超出一把
    // 密钥的上限时，请求等着，而上游确实没有同时收到超过上限的数
    let (up, peak) = start_slow_upstream(Duration::from_millis(200)).await;
    let mut cfg = cfg_slow(up);
    cfg.clients[0].max_concurrent = Some(2);
    let gw = serve_cfg(cfg).await;

    let codes = send_many(gw, 6).await;
    assert!(codes.iter().all(|&c| c == 200), "全部该成功：{codes:?}");
    assert_eq!(peak.load(Ordering::SeqCst), 2, "上游峰值");
}

#[tokio::test]
async fn listing_and_admission_come_from_the_same_place() {
    // 核心：**列出来的一定能用，能用的一定列了出来**。两处各写
    // 一遍的话，「列表里有但用不了」这种状态迟早出现 —— 而 one-api 和
    // new-api 都栽在这上面。
    let (up, _) = start_upstream(false).await;
    let mut cfg = cfg_with(
        vec![Provider {
            name: "relay".into(),
            base_url: format!("http://{up}"),
            key: Some("k".into()),
            protocol: Some(tw_config::Protocol::Anthropic),
            // 这家不实现 /v1/models，所以手写兜底
            models: vec!["claude-sonnet-4-5".into(), "claude-haiku-4-5".into()],
            ..Default::default()
        }],
        vec![],
    );
    cfg.clients[0].allow = Some(vec!["claude-haiku-*".into()]);
    let gw = serve_cfg(cfg).await;

    // 列表里只有 haiku
    let listed: serde_json::Value = reqwest::Client::new()
        .get(format!("http://{gw}/v1/models"))
        .header("x-api-key", "tw-k")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["claude-haiku-4-5"]);

    // 列出来的能用
    let ok = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"claude-haiku-4-5"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);

    // 没列出来的用不了，而且错误要说人话
    let refused = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"claude-sonnet-4-5"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 400);
    let body: serde_json::Value = refused.json().await.unwrap();
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("claude-sonnet-4-5"), "{msg}");
    assert!(msg.contains("/v1/models"), "要告诉他去哪看能用什么：{msg}");
}

#[tokio::test]
async fn an_empty_allow_list_disables_the_client_entirely() {
    // 「临时禁用这个客户端」的正当用法。one-api 和 new-api 在这里语义
    // 正好相反，所以必须有个测试钉住我们这边是哪一种。
    let (up, _) = start_upstream(false).await;
    let mut cfg = cfg_with(
        vec![Provider {
            name: "relay".into(),
            base_url: format!("http://{up}"),
            key: Some("k".into()),
            protocol: Some(tw_config::Protocol::Anthropic),
            models: vec!["claude-sonnet-4-5".into()],
            ..Default::default()
        }],
        vec![],
    );
    cfg.clients[0].allow = Some(vec![]);
    let gw = serve_cfg(cfg).await;

    let listed: serde_json::Value = reqwest::Client::new()
        .get(format!("http://{gw}/v1/models"))
        .header("x-api-key", "tw-k")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(listed["data"].as_array().unwrap().is_empty());

    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"claude-sonnet-4-5"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn with_nothing_discovered_the_gateway_does_not_lock_itself_shut() {
    // 目录空着说明探测还没回来、或者上游都不给列表也没写兜底。这时候
    // 拦等于把整个网关关掉 —— 而用户完全看不出为什么。
    let (up, seen) = start_upstream(false).await;
    let gw = serve_cfg(cfg_with(
        vec![Provider {
            name: "relay".into(),
            base_url: format!("http://{up}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![],
    ))
    .await;
    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"whatever"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(!seen.lock().unwrap().body.is_empty());
}

/// 会记录收到了什么、然后按指定状态码回话的上游。
///
/// `start_broken_upstream` 不记录 body，而两阶段求值的证据恰恰在
/// **第一家收到了什么** —— 只看第二家证明不了「切换前后算的是两次」。
async fn start_recording_upstream(status: u16) -> (SocketAddr, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let s = seen.clone();
    let app = Router::new().fallback(axum::routing::any(
        move |State(s): State<Arc<Mutex<Seen>>>, headers: HeaderMap, body: bytes::Bytes| async move {
            {
                let mut g = s.lock().unwrap();
                g.headers = headers;
                g.body = body.to_vec();
            }
            axum::http::StatusCode::from_u16(status).unwrap()
        },
    )).with_state(s);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, seen)
}

fn body_of(seen: &Arc<Mutex<Seen>>) -> serde_json::Value {
    let g = seen.lock().unwrap();
    assert!(!g.body.is_empty(), "这家上游根本没收到请求");
    serde_json::from_slice(&g.body).expect("上游收到的不是 JSON")
}

#[tokio::test]
async fn a_phase_two_rule_is_recomputed_after_failover() {
    // **这是两阶段求值存在的全部理由**。`provider_would_be` 的
    // 值要等路由决定完才知道，而故障转移会在之后再改一次去向 —— 所以
    // 它必须在转移循环**里面**重算。
    //
    // 否则「走中转的一律关掉 thinking」这条规则，会在从官方转移到中转
    // 的那一刻失效 —— 而那正是最需要它的时刻。
    let (official, seen_official) = start_recording_upstream(503).await;
    let (relay, seen_relay) = start_upstream(false).await;

    let mut cfg = cfg_with(
        vec![
            Provider {
                name: "official".into(),
                base_url: format!("http://{official}"),
                key: Some("k".into()),
                protocol: Some(tw_config::Protocol::Anthropic),
                ..Default::default()
            },
            Provider {
                name: "relay".into(),
                base_url: format!("http://{relay}"),
                key: Some("k".into()),
                protocol: Some(tw_config::Protocol::Anthropic),
                ..Default::default()
            },
        ],
        vec![tw_engine::Rule {
            name: "中转不开思考".into(),
            when: serde_yaml_ng::from_str("{ provider_would_be: relay }").unwrap(),
            to: None,
            set: Some(tw_engine::SetAction {
                thinking: Some(false),
                ..Default::default()
            }),
            deny: None,
        }],
    );
    // 没有别的规则时层 0 会补一条兜底 —— 但只有在 routes 为空时。
    // 这里已经有一条阶段二规则了，所以显式写出兜底。
    cfg.routes[0].rules.push(tw_engine::Rule {
        name: "兜底".into(),
        when: Default::default(),
        to: Some("official".into()),
        set: None,
        deny: None,
    });
    // 兜底只指一家的话就没得转移了 —— 用组把两家串起来。
    cfg.groups = vec![tw_engine::Group {
        name: "全部".into(),
        kind: tw_engine::GroupType::Fallback,
        providers: vec!["official".into(), "relay".into()],
        session_affinity: false,
        selected: None,
    }];
    cfg.routes[0].rules.last_mut().unwrap().to = Some("全部".into());

    let gw = serve_cfg(cfg).await;
    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"claude-sonnet-4-5","thinking":{"type":"enabled","budget_tokens":1024}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // 第一家（官方）：规则不该命中，body 原样带着 thinking
    assert!(
        body_of(&seen_official).get("thinking").is_some(),
        "阶段二规则在官方那一跳就生效了 —— 说明它没在循环里重算，而是只算了一次"
    );
    // 第二家（中转）：转移之后重算，thinking 被摘掉
    assert!(
        body_of(&seen_relay).get("thinking").is_none(),
        "转移到中转之后，阶段二规则没有重新生效"
    );
}

#[tokio::test]
async fn a_phase_two_deny_reaches_the_client_with_its_reason() {
    // 一个没有理由的拒绝，和一个 bug，在用户眼里没有区别。
    let (relay, seen) = start_upstream(false).await;
    let gw = serve_cfg(cfg_with(
        vec![Provider {
            name: "relay".into(),
            base_url: format!("http://{relay}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![
            tw_engine::Rule {
                name: "中转不许发这个".into(),
                when: serde_yaml_ng::from_str("{ provider_would_be: relay }").unwrap(),
                to: None,
                set: None,
                deny: Some("这段内容不发给中转站".into()),
            },
            tw_engine::Rule {
                name: "兜底".into(),
                when: Default::default(),
                to: Some("relay".into()),
                set: None,
                deny: None,
            },
        ],
    ))
    .await;

    let r = send_to(gw).await;
    // 表：deny 是 403 + permission_error，不是 400 ——
    // 请求本身完全合法，是策略不让。
    assert_eq!(r.status(), 403, "是策略不让，不是请求本身有问题");
    let text = r.text().await.unwrap();
    assert!(text.contains("这段内容不发给中转站"), "{text}");
    assert!(
        seen.lock().unwrap().body.is_empty(),
        "被拒绝的请求一个字节都不该到上游"
    );
}

#[tokio::test]
async fn a_set_that_changes_nothing_leaves_the_body_byte_for_byte() {
    // 出站直通：**改写是显式要求的例外，不是默认行为**。
    // cc-switch 那次把缓存命中率从 99% 打到 20%，就是因为一个「看起来
    // 无害」的重写跑在了每个请求上。
    let (up, seen) = start_upstream(false).await;
    let gw = serve_cfg(cfg_with(
        vec![Provider {
            name: "up".into(),
            base_url: format!("http://{up}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![],
    ))
    .await;
    // 键顺序刻意不是字典序 —— 任何一次 JSON 往返都会把它重排。
    let raw = r#"{"model":"claude-sonnet-4-5","z_last":1,"a_first":2,"messages":[]}"#;
    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(raw)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        String::from_utf8(seen.lock().unwrap().body.clone()).unwrap(),
        raw,
        "没有 set 的请求体必须一个字节都没动过"
    );
}

#[tokio::test]
async fn a_phase_one_set_applies_on_every_attempt_including_after_failover() {
    // `set` 从所有命中的规则累积，而阶段一的结果是阶段二的基线 ——
    // 转移到第二家之后，第一家算出来的改写不能丢。
    let (dead, seen_dead) = start_recording_upstream(503).await;
    let (good, seen_good) = start_upstream(false).await;
    let mut cfg = cfg_with(
        vec![
            Provider {
                name: "dead".into(),
                base_url: format!("http://{dead}"),
                key: Some("k".into()),
                ..Default::default()
            },
            Provider {
                name: "good".into(),
                base_url: format!("http://{good}"),
                key: Some("k".into()),
                ..Default::default()
            },
        ],
        vec![tw_engine::Rule {
            name: "统一压一下上限".into(),
            when: Default::default(),
            to: Some("全部".into()),
            set: Some(tw_engine::SetAction {
                max_tokens: Some(4096),
                ..Default::default()
            }),
            deny: None,
        }],
    );
    cfg.groups = vec![tw_engine::Group {
        name: "全部".into(),
        kind: tw_engine::GroupType::Fallback,
        providers: vec!["dead".into(), "good".into()],
        session_affinity: false,
        selected: None,
    }];
    let gw = serve_cfg(cfg).await;
    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"claude-sonnet-4-5","max_tokens":64000}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(body_of(&seen_dead)["max_tokens"], 4096);
    assert_eq!(body_of(&seen_good)["max_tokens"], 4096, "转移之后丢了");
}

#[tokio::test]
async fn a_health_check_is_answered_locally_and_never_reaches_the_upstream() {
    // A 类：客户端只想知道「通不通」，回什么内容它不看。
    let (up, seen) = start_upstream(false).await;
    let gw = serve_cfg(cfg_with(
        vec![Provider {
            name: "up".into(),
            base_url: format!("http://{up}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![],
    ))
    .await;
    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .header("user-agent", "claude-cli/1.0.0 (external, cli)")
        .body(r#"{"model":"claude-3-5-haiku-20241022","max_tokens":1,"messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // 对用户透明：响应上有标记
    assert_eq!(r.headers().get("x-thinkwatch-local").unwrap(), "1");
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["stop_reason"], "max_tokens");
    assert!(v["id"].as_str().unwrap().starts_with("msg_01"));
    assert!(
        seen.lock().unwrap().body.is_empty(),
        "本地应答的请求一个字节都不该到上游"
    );
}

#[tokio::test]
async fn a_health_check_still_works_with_every_upstream_dead() {
    // **这是这个功能最有价值的场景**。sub2api 把判定放在选号
    // 之后，于是断网时健康检查照样失败 —— 而客户端会因此报错。
    let dead = start_broken_upstream(503).await;
    let gw = serve_cfg(cfg_with(
        vec![Provider {
            name: "dead".into(),
            // 连端口都没人听
            base_url: format!("http://{dead}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![],
    ))
    .await;
    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .header("user-agent", "claude-cli/1.0.0")
        .body(r#"{"model":"claude-3-5-haiku-20241022","max_tokens":1,"messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "上游全挂时健康检查也必须能答");
}

#[tokio::test]
async fn a_titling_request_goes_to_the_upstream_untouched() {
    // **B 类默认放行。**拦掉的话，用户在 /resume 里看到的每个会话都叫
    // 同一个名字 —— 那不是省钱，那是把一个功能关掉了。
    let (up, seen) = start_upstream(false).await;
    let gw = serve_cfg(cfg_with(
        vec![Provider {
            name: "up".into(),
            base_url: format!("http://{up}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![],
    ))
    .await;
    let raw = r#"{"model":"claude-3-5-haiku-20241022","messages":[{"role":"user","content":"Please write a 5-10 word title for the following conversation: 修 bug"}]}"#;
    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .header("user-agent", "claude-cli/1.0.0")
        .body(raw)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().get("x-thinkwatch-local").is_none());
    assert_eq!(
        String::from_utf8(seen.lock().unwrap().body.clone()).unwrap(),
        raw,
        "放行的请求必须一个字节都没动过"
    );
}

#[tokio::test]
async fn intercepting_a_probe_emits_its_own_event_not_a_request_pair() {
    // 成本 0、延迟 0 的东西混进请求总数和延迟统计里，会让那两个数字
    // 都变得没意义。
    let (up, _seen) = start_upstream(false).await;
    let cfg = cfg_with(
        vec![Provider {
            name: "up".into(),
            base_url: format!("http://{up}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![],
    );
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let addr = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    let st = state.clone();
    tokio::spawn(async move { tw_gateway::serve(st, addr).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    // 监听起来时报的那一条不算：这里只看请求带出来的事件
    let mut rx = state.bus.subscribe();

    reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .header("x-api-key", "tw-k")
        .header("user-agent", "claude-cli/1.0.0")
        .body(r#"{"model":"claude-3-5-haiku-20241022","max_tokens":1,"messages":[]}"#)
        .send()
        .await
        .unwrap();

    let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("2 秒内没等到事件")
        .unwrap();
    match ev {
        tw_api::Event::LocallyAnswered { probe, .. } => assert_eq!(probe, "health_check"),
        other => panic!("该是本地应答，实际 {other:?}"),
    }
    // 后面不该再跟着一对 started/finished
    assert!(
        tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .is_err(),
        "本地应答不该再发请求事件"
    );
}

#[tokio::test]
async fn turning_off_the_interception_sends_the_health_check_upstream() {
    // 拦截是个默认值，不是一条铁律。想看真实探测流量的人要能关掉它。
    let (up, seen) = start_upstream(false).await;
    let mut cfg = cfg_with(
        vec![Provider {
            name: "up".into(),
            base_url: format!("http://{up}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![],
    );
    cfg.client_probes.health_check = tw_config::ProbeAction::Passthrough;
    let gw = serve_cfg(cfg).await;
    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .header("user-agent", "claude-cli/1.0.0")
        .body(r#"{"model":"claude-3-5-haiku-20241022","max_tokens":1,"messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(!seen.lock().unwrap().body.is_empty(), "关了就该发出去");
}

#[tokio::test]
async fn a_probe_set_to_route_can_be_sent_somewhere_cheaper() {
    // 第三个选项。**它成立的前提是你手里真有一个更便宜的地方** ——
    // 所以这是高级用法，默认没人会走到这里。
    let (cheap, seen_cheap) = start_upstream(false).await;
    let (normal, seen_normal) = start_upstream(false).await;
    let mut cfg = cfg_with(
        vec![
            Provider {
                name: "便宜的".into(),
                base_url: format!("http://{cheap}"),
                key: Some("k".into()),
                ..Default::default()
            },
            Provider {
                name: "正常的".into(),
                base_url: format!("http://{normal}"),
                key: Some("k".into()),
                ..Default::default()
            },
        ],
        vec![
            tw_engine::Rule {
                name: "客户端辅助请求".into(),
                when: serde_yaml_ng::from_str("{ intent: assistant_internal }").unwrap(),
                to: Some("便宜的".into()),
                set: None,
                deny: None,
            },
            tw_engine::Rule {
                name: "兜底".into(),
                when: Default::default(),
                to: Some("正常的".into()),
                set: None,
                deny: None,
            },
        ],
    );
    cfg.client_probes.titling = tw_config::ProbeAction::Route;
    let gw = serve_cfg(cfg).await;

    let send = |body: String| async move {
        reqwest::Client::new()
            .post(format!("http://{gw}/v1/messages"))
            .header("x-api-key", "tw-k")
            .header("user-agent", "claude-cli/1.0.0")
            .body(body)
            .send()
            .await
            .unwrap()
    };
    send(r#"{"model":"m","messages":[{"role":"user","content":"Please write a 5-10 word title for the following conversation: x"}]}"#.into()).await;
    assert!(
        !seen_cheap.lock().unwrap().body.is_empty(),
        "配成 route 的标题请求该走便宜的那家"
    );
    assert!(seen_normal.lock().unwrap().body.is_empty());

    // 真实请求照旧走兜底
    send(r#"{"model":"m","messages":[{"role":"user","content":"帮我改个 bug"}]}"#.into()).await;
    assert!(!seen_normal.lock().unwrap().body.is_empty());
}

#[tokio::test]
async fn an_intent_rule_does_not_fire_while_the_probe_is_still_passthrough() {
    // **passthrough 不打标记。**打了的话，一条 intent 规则会在用户还没
    // 把那类请求配成 route 的时候就开始生效 —— 而配置文件里看不出线索。
    let (cheap, seen_cheap) = start_upstream(false).await;
    let (normal, seen_normal) = start_upstream(false).await;
    let cfg = cfg_with(
        vec![
            Provider {
                name: "便宜的".into(),
                base_url: format!("http://{cheap}"),
                key: Some("k".into()),
                ..Default::default()
            },
            Provider {
                name: "正常的".into(),
                base_url: format!("http://{normal}"),
                key: Some("k".into()),
                ..Default::default()
            },
        ],
        vec![
            tw_engine::Rule {
                name: "客户端辅助请求".into(),
                when: serde_yaml_ng::from_str("{ intent: assistant_internal }").unwrap(),
                to: Some("便宜的".into()),
                set: None,
                deny: None,
            },
            tw_engine::Rule {
                name: "兜底".into(),
                when: Default::default(),
                to: Some("正常的".into()),
                set: None,
                deny: None,
            },
        ],
    );
    // titling 保持默认的 passthrough
    let gw = serve_cfg(cfg).await;
    reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .header("user-agent", "claude-cli/1.0.0")
        .body(r#"{"model":"m","messages":[{"role":"user","content":"Please write a 5-10 word title for the following conversation: x"}]}"#)
        .send()
        .await
        .unwrap();
    assert!(
        seen_cheap.lock().unwrap().body.is_empty(),
        "还没配成 route，intent 规则就不该命中"
    );
    assert!(!seen_normal.lock().unwrap().body.is_empty());
}

#[tokio::test]
async fn a_health_check_works_before_any_upstream_is_configured() {
    // 「一个 provider 都没有」也是一种「没有可用上游」，而本地应答本来
    // 就不需要上游。真实请求照样会拿到那句「还没有配置任何上游」。
    let gw = serve_cfg(cfg_with(vec![], vec![])).await;
    let c = reqwest::Client::new();
    let probe = c
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .header("user-agent", "claude-cli/1.0.0")
        .body(r#"{"model":"claude-3-5-haiku-20241022","max_tokens":1,"messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(probe.status(), 200);

    let real = c
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"claude-sonnet-4-5","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_ne!(real.status(), 200);
    assert!(
        real.text()
            .await
            .unwrap()
            .contains("No upstream is configured")
    );
}

/// **错误必须用入站方言的原生格式返回。**一个 Anthropic 客户端
/// 收到 OpenAI 形状的 error body，会在解析时炸掉，然后报一个和真实原因
/// 完全无关的错。
///
/// 方言按路径认，不按密钥放在哪儿认。
#[tokio::test]
async fn an_error_comes_back_in_the_dialect_the_client_speaks() {
    let gw = serve_cfg(cfg_with(vec![], vec![])).await;
    let c = reqwest::Client::new();
    let post = |path: &str, auth: &str| {
        let r = c
            .post(format!("http://{gw}{path}"))
            .body(r#"{"model":"m","messages":[]}"#);
        match auth {
            "bearer" => r.bearer_auth("tw-k"),
            h => r.header(h, "tw-k"),
        }
    };

    // Anthropic：`{type:"error", error:{type,message}}`
    for auth in ["x-api-key", "bearer"] {
        // Bearer 是 Claude Code 用 `ANTHROPIC_AUTH_TOKEN` 时的发法 —— 照样是 Anthropic
        let v: serde_json::Value = post("/v1/messages", auth)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(v["type"], "error", "{auth}: {v}");
        assert_eq!(v["error"]["type"], "api_error", "{auth}: {v}");
    }

    // OpenAI：没有外层 type，多了 param / code
    for path in ["/v1/chat/completions", "/v1/responses"] {
        let v: serde_json::Value = post(path, "bearer")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(v.get("type").is_none(), "{path}: OpenAI 没有外层 type：{v}");
        assert_eq!(v["error"]["type"], "server_error", "{path}: {v}");
        assert!(v["error"].get("param").is_some(), "{path}: {v}");
    }

    // Gemini：`{error:{code,message,status}}`
    let v: serde_json::Value = post(
        "/v1beta/models/gemini-2.5-pro:generateContent",
        "x-goog-api-key",
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(v["error"]["status"], "UNAVAILABLE", "{v}");
    assert_eq!(v["error"]["code"], 500, "{v}");
}

#[tokio::test]
async fn every_dialect_still_gets_the_thinkwatch_prefix_and_header() {
    // 用户遇到报错的第一反应是去找中转站客服。**分不清是哪一层，他会
    // 浪费时间问错人，而且会觉得是我们坏了**。
    let gw = serve_cfg(cfg_with(vec![], vec![])).await;
    let c = reqwest::Client::new();
    let url = format!("http://{gw}/v1/messages");
    for auth in ["x-api-key", "x-goog-api-key"] {
        let r = c
            .post(&url)
            .header(auth, "tw-k")
            .body(r#"{"model":"m"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(r.headers().get("x-thinkwatch-error").unwrap(), "config");
        let t = r.text().await.unwrap();
        assert!(t.contains("[ThinkWatch]"), "{auth}: {t}");
    }
}

#[tokio::test]
async fn a_deny_rule_is_403_not_400() {
    // 表：`deny` 是 403 + permission_error。**和「你这个请求
    // 本身有问题」分开** —— 混在一起的话，用户会去改他的请求，而该改
    // 的是规则。
    let (up, seen) = start_upstream(false).await;
    let gw = serve_cfg(cfg_with(
        vec![Provider {
            name: "up".into(),
            base_url: format!("http://{up}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![
            tw_engine::Rule {
                name: "不许用 opus".into(),
                when: serde_yaml_ng::from_str("{ model: claude-opus-* }").unwrap(),
                to: None,
                set: None,
                deny: Some("这个项目不用 opus".into()),
            },
            tw_engine::Rule {
                name: "兜底".into(),
                when: Default::default(),
                to: Some("up".into()),
                set: None,
                deny: None,
            },
        ],
    ))
    .await;
    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"claude-opus-4","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    assert_eq!(r.headers().get("x-thinkwatch-error").unwrap(), "denied");
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["error"]["type"], "permission_error");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("这个项目不用 opus")
    );
    assert!(seen.lock().unwrap().body.is_empty());
}

/// 首字节之后上游断了：SSE 流里补一个 `error` 帧，并且发一条失败事件。
///
/// **截断和「答完了」在 SSE 里长得一模一样。**什么都不做的话，用户会
/// 以为模型就答了这么多，而 UI 上那一行会永远停在「进行中」。
#[tokio::test]
async fn a_stream_that_dies_midway_says_so_instead_of_just_stopping() {
    // 声明 content-length 比实际发的多，然后把连接关掉 —— 客户端库会
    // 把它报成一个流错误，这正是「上游中途没了」的样子。
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                          content-length: 900\r\n\r\n\
                          event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
                    )
                    .await;
                let _ = s.flush().await;
                // 说好 900 字节，只发了几十个，然后走人
                drop(s);
            });
        }
    });

    let cfg = cfg_with(
        vec![Provider {
            name: "up".into(),
            base_url: format!("http://{up}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![],
    );
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let mut rx = state.bus.subscribe();
    let gw = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, gw).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let text = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"m","stream":true}"#)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap_or_default();
    assert!(
        text.contains("event: error"),
        "流断了却没有任何交代：{text:?}"
    );
    assert!(text.contains("[ThinkWatch]"), "{text:?}");

    // UI 那一行不能永远停在「进行中」
    let mut saw_failed = false;
    for _ in 0..6 {
        match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Ok(tw_api::Event::RequestFailed { message, .. })) => {
                assert!(message.text.contains("stream broke"), "{message}");
                saw_failed = true;
                break;
            }
            Ok(Ok(_)) => continue,
            _ => break,
        }
    }
    assert!(saw_failed, "断流之后没有发失败事件，UI 会一直转圈");
}

#[tokio::test]
async fn an_upstream_rate_limit_stays_a_429_instead_of_becoming_a_502() {
    // 429 塌成 502 的话，客户端会当成「服务器坏了」而不是「该退避了」,
    // 而它们该做的事完全不同。
    let limited = start_broken_upstream(429).await;
    let gw = serve_cfg(cfg_with(
        vec![Provider {
            name: "limited".into(),
            base_url: format!("http://{limited}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![],
    ))
    .await;
    let r = send_to(gw).await;
    assert_eq!(r.status(), 429);
    assert_eq!(
        r.headers().get("x-thinkwatch-error").unwrap(),
        "rate_limited"
    );
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["error"]["type"], "rate_limit_error");
}

#[tokio::test]
async fn the_upstream_usage_reaches_the_event_stream_without_buffering_the_response() {
    // **上游返回的 usage 是真相**，所以要拿到它 —— 但不能为此
    // 把流缓冲起来。这条同时验两件事：数字对，而且流还是流。
    let up = {
        let app = Router::new().fallback(axum::routing::any(|| async {
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from(
                    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5000,\"cache_read_input_tokens\":4000}}}\n\n\
                     event: content_block_delta\ndata: {\"delta\":{\"text\":\"答案\"}}\n\n\
                     event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":777}}\n\n",
                ))
                .unwrap()
        }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        a
    };
    let state = tw_gateway::AppState::new(cfg_with(
        vec![Provider {
            name: "up".into(),
            base_url: format!("http://{up}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![],
    ))
    .unwrap();
    let mut rx = state.bus.subscribe();
    let gw = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, gw).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let text = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"claude-sonnet-4-5","stream":true}"#)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // 流的内容一个字节都没被动过
    assert!(text.contains("event: message_start"), "{text}");
    assert!(text.contains("答案"), "{text}");

    let mut usage = None;
    for _ in 0..6 {
        match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Ok(tw_api::Event::RequestFinished { usage: u, .. })) => {
                usage = u;
                break;
            }
            Ok(Ok(_)) => continue,
            _ => break,
        }
    }
    let u = usage.expect("结束事件里没有 usage");
    assert_eq!(u.input, 5000, "开头那个输入没被记住");
    assert_eq!(u.output, 777, "结尾那个累计输出没被记住");
    assert_eq!(u.cache_read, 4000);
}

#[tokio::test]
async fn an_upstream_that_gives_no_usage_reports_none_rather_than_zeroes() {
    // **零会让一次真实的调用看起来是免费的**。有些中转站就是
    // 不给 usage，那时该走估算那条路。
    let (up, _seen) = start_upstream(false).await;
    let state = tw_gateway::AppState::new(cfg_with(
        vec![Provider {
            name: "up".into(),
            base_url: format!("http://{up}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![],
    ))
    .unwrap();
    let mut rx = state.bus.subscribe();
    let gw = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, gw).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    send_to(gw).await;

    for _ in 0..6 {
        if let Ok(Ok(tw_api::Event::RequestFinished { usage, .. })) =
            tokio::time::timeout(Duration::from_secs(2), rx.recv()).await
        {
            assert!(usage.is_none(), "上游没给 usage，却报了 {usage:?}");
            return;
        }
    }
    panic!("没等到结束事件");
}

#[tokio::test]
async fn a_key_pasted_into_a_prompt_is_noticed_but_the_request_goes_through_untouched() {
    // **观察态只记录，不改变任何行为**。这条同时验两件事：
    // 发现了，而且请求体一个字节都没被动过 —— 后者是这一态的全部承诺。
    let (up, seen) = start_upstream(false).await;
    let state = tw_gateway::AppState::new(cfg_with(
        vec![Provider {
            name: "中转".into(),
            base_url: format!("http://{up}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![],
    ))
    .unwrap();
    let mut rx = state.bus.subscribe();
    let gw = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, gw).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let raw = r#"{"model":"m","messages":[{"role":"user","content":"我的 key 是 sk-ant-api03-abcdefghijklmnopqrstuvwxyz1234"}]}"#;
    reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(raw)
        .send()
        .await
        .unwrap();

    // **请求体原样发出去了。**观察态动了 body 就不是观察态了。
    assert_eq!(
        String::from_utf8(seen.lock().unwrap().body.clone()).unwrap(),
        raw,
        "观察态改了请求体"
    );

    let mut found = None;
    for _ in 0..8 {
        match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Ok(tw_api::Event::SecretsFound {
                items,
                provider,
                replaced,
                ..
            })) => {
                found = Some((items, provider, replaced));
                break;
            }
            Ok(Ok(_)) => continue,
            _ => break,
        }
    }
    let (items, provider, replaced) = found.expect("请求体里有 key，却没有发现");
    assert!(!replaced, "观察档说自己换了");
    assert_eq!(items[0].rule, "anthropic-api-key");
    assert_eq!(provider, "中转", "得知道发给了谁");
    // 报出来的东西一律打码：「发现了 sk-ant-xxx」本身就是一次泄漏
    assert!(
        !items[0].masked.contains("abcdefghijklmnop"),
        "{:?}",
        items[0]
    );
}

#[tokio::test]
async fn turning_the_detector_off_stops_it_looking_at_all() {
    let (up, _seen) = start_upstream(false).await;
    let mut cfg = cfg_with(
        vec![Provider {
            name: "up".into(),
            base_url: format!("http://{up}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![],
    );
    cfg.security.redact.mode = tw_config::SecurityMode::Off;
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let mut rx = state.bus.subscribe();
    let gw = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, gw).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"m","messages":[{"role":"user","content":"sk-ant-api03-abcdefghijklmnopqrstuvwxyz1234"}]}"#)
        .send()
        .await
        .unwrap();

    for _ in 0..8 {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Ok(tw_api::Event::SecretsFound { .. })) => panic!("关掉了却还在检测"),
            Ok(Ok(_)) => continue,
            _ => break,
        }
    }
}

#[tokio::test]
async fn the_attempt_chain_records_every_hop_and_why_each_one_failed() {
    // **一条说「试过 A → B → C」的链，和一条还说清每一跳为什么失败的
    // 链，排查价值差得远**。
    let dead = start_broken_upstream(503).await;
    let limited = start_broken_upstream(429).await;
    let (good, _) = start_upstream(false).await;
    let mut cfg = cfg_with(
        vec![
            Provider {
                name: "挂了的".into(),
                base_url: format!("http://{dead}"),
                key: Some("k".into()),
                ..Default::default()
            },
            Provider {
                name: "限流的".into(),
                base_url: format!("http://{limited}"),
                key: Some("k".into()),
                ..Default::default()
            },
            Provider {
                name: "好的".into(),
                base_url: format!("http://{good}"),
                key: Some("k".into()),
                ..Default::default()
            },
        ],
        vec![],
    );
    cfg.groups = vec![tw_engine::Group {
        name: "全部".into(),
        kind: tw_engine::GroupType::Fallback,
        providers: vec!["挂了的".into(), "限流的".into(), "好的".into()],
        session_affinity: false,
        selected: None,
    }];
    cfg.routes = vec![tw_engine::RouteSet::default_with(vec![tw_engine::Rule {
        name: "都走这一组".into(),
        when: Default::default(),
        to: Some("全部".into()),
        set: None,
        deny: None,
    }])];
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let mut rx = state.bus.subscribe();
    let gw = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, gw).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(send_to(gw).await.status(), 200);

    let mut routed = None;
    for _ in 0..8 {
        match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Ok(tw_api::Event::RequestRouted {
                rule,
                group,
                attempts,
                ..
            })) => {
                routed = Some((rule, group, attempts));
                break;
            }
            Ok(Ok(_)) => continue,
            _ => break,
        }
    }
    let (rule, group, attempts) = routed.expect("没有发出路由事件");
    // **「命中第 4 条」远不如「命中『都走这一组』」有用**
    assert_eq!(rule, "都走这一组");
    assert_eq!(group.as_deref(), Some("全部"));
    assert_eq!(attempts.len(), 3, "{attempts:?}");
    assert_eq!(attempts[0].provider, "挂了的");
    assert_eq!(attempts[0].outcome, "status", "{:?}", attempts[0]);
    assert_eq!(attempts[0].status, Some(503), "{:?}", attempts[0]);
    assert_eq!(attempts[1].provider, "限流的");
    assert_eq!(attempts[1].status, Some(429), "{:?}", attempts[1]);
    assert_eq!(attempts[2].provider, "好的");
    assert_eq!(attempts[2].outcome, "served");
    assert_eq!(attempts[2].status, Some(200));
}

#[tokio::test]
async fn a_request_that_succeeds_first_try_still_has_a_chain_of_one() {
    // 「只试了一家」和「试了三家」在用户眼里应该是不同的 —— 而只在
    // 发生过转移时才记链，那两件事在界面上就长得一样了。
    let (good, _) = start_upstream(false).await;
    let state = tw_gateway::AppState::new(cfg_with(
        vec![Provider {
            name: "官方".into(),
            base_url: format!("http://{good}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![],
    ))
    .unwrap();
    let mut rx = state.bus.subscribe();
    let gw = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, gw).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    send_to(gw).await;

    for _ in 0..8 {
        if let Ok(Ok(tw_api::Event::RequestRouted { attempts, .. })) =
            tokio::time::timeout(Duration::from_secs(2), rx.recv()).await
        {
            assert_eq!(attempts.len(), 1);
            assert_eq!(attempts[0].outcome, "served");
            return;
        }
    }
    panic!("没有发出路由事件");
}

#[tokio::test]
async fn a_request_that_fails_everywhere_still_reports_the_chain() {
    // **失败那条路才是最需要看尝试链的时候。**挂在 RequestFinished 上
    // 的话，它恰好在那时缺席。
    let dead = start_broken_upstream(503).await;
    let state = tw_gateway::AppState::new(cfg_with(
        vec![Provider {
            name: "挂了的".into(),
            base_url: format!("http://{dead}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![],
    ))
    .unwrap();
    let mut rx = state.bus.subscribe();
    let gw = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, gw).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_ne!(send_to(gw).await.status(), 200);

    for _ in 0..8 {
        if let Ok(Ok(tw_api::Event::RequestRouted { attempts, .. })) =
            tokio::time::timeout(Duration::from_secs(2), rx.recv()).await
        {
            assert_eq!(attempts.len(), 1);
            assert_eq!(attempts[0].outcome, "status", "{:?}", attempts[0]);
            assert_eq!(attempts[0].status, Some(503), "{:?}", attempts[0]);
            return;
        }
    }
    panic!("全失败的请求没有发出路由事件");
}

#[tokio::test]
async fn reporting_quota_does_not_change_how_an_upstream_is_billed() {
    // **订阅账号也按价目表算费用。**以前报过额度头的上游从第二个请求起就
    // 改记成订阅、不算钱，于是同一个账号的第一个请求有费用、之后的没有。
    // 额度是额度，计费是计费：额度头只进额度，不改计费方式。
    let up = {
        let app = Router::new().fallback(axum::routing::any(|| async {
            axum::response::Response::builder()
                .header("content-type", "application/json")
                .header("anthropic-ratelimit-unified-5h-utilization", "40")
                .body(axum::body::Body::from(r#"{"id":"m"}"#))
                .unwrap()
        }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        a
    };
    let state = tw_gateway::AppState::new(cfg_with(
        vec![Provider {
            name: "订阅账号".into(),
            base_url: format!("http://{up}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![],
    ))
    .unwrap();
    let mut rx = state.bus.subscribe();
    let gw = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, gw).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;

    for n in 1..=2 {
        send_to(gw).await;
        assert_eq!(
            next_billing(&mut rx).await,
            "per-token",
            "第 {n} 个请求：报过额度头之后不该改记成别的计费方式"
        );
    }
}

#[tokio::test]
async fn free_billing_in_the_config_goes_out_with_the_request() {
    // 本地模型、免费额度写 `billing: free`，记账那一层按这个词记 $0
    let (up, _) = start_upstream(false).await;
    let mut cfg = cfg_with(
        vec![Provider {
            name: "本地".into(),
            base_url: format!("http://{up}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![],
    );
    cfg.providers[0].billing = tw_config::Billing::Free;
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let mut rx = state.bus.subscribe();
    let gw = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, gw).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    send_to(gw).await;

    for _ in 0..8 {
        if let Ok(Ok(tw_api::Event::RequestRouted { billing, .. })) =
            tokio::time::timeout(Duration::from_secs(2), rx.recv()).await
        {
            assert_eq!(billing, "free", "配置里写了却没生效");
            return;
        }
    }
    panic!("没等到路由事件");
}

/// 额度的重置是一个**时刻**。以前存的是「还有多少秒」，之后原样给出去 ——
/// 界面每次读到的都是收到响应那一刻的秒数，一个不会走的倒计时。
#[tokio::test]
async fn a_quota_reset_is_kept_as_the_moment_it_happens() {
    let up = {
        let app = Router::new().fallback(axum::routing::any(|| async {
            axum::response::Response::builder()
                .header("content-type", "application/json")
                .header("anthropic-ratelimit-unified-5h-utilization", "40")
                .header("anthropic-ratelimit-unified-5h-reset", "7200")
                .body(axum::body::Body::from(r#"{"id":"m"}"#))
                .unwrap()
        }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        a
    };
    let state = tw_gateway::AppState::new(cfg_with(
        vec![Provider {
            name: "订阅账号".into(),
            base_url: format!("http://{up}"),
            key: Some("k".into()),
            ..Default::default()
        }],
        vec![],
    ))
    .unwrap();
    let kept = state.clone();
    let mut rx = state.bus.subscribe();
    let gw = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, gw).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    send_to(gw).await;

    let (resets_at, seen_at) = loop {
        match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Ok(tw_api::Event::QuotaSeen { windows, at_ms, .. })) => {
                break (windows[0].resets_at_ms, at_ms);
            }
            Ok(Ok(_)) => continue,
            other => panic!("没等到额度事件：{other:?}"),
        }
    };
    let resets_at = resets_at.expect("上游说了多久重置，事件里却没有");
    // 收到响应的那一刻往后两小时。两个时刻各自取的钟，差几毫秒
    assert!(
        (seen_at + 7_200_000 - 1_000..=seen_at + 7_200_000 + 1_000).contains(&resets_at),
        "{resets_at} vs {seen_at}"
    );
    // **存下来的是同一个时刻**：过一会儿再问，它不会变成「又是两小时」
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        kept.quotas()["订阅账号"].windows[0].resets_at_ms,
        Some(resets_at)
    );
}
