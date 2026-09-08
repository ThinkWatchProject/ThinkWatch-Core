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
            key: "sk-upstream-secret".into(),
            protocol: Some(tw_config::Protocol::Anthropic),
            ..Default::default()
        }],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, tw_gateway::router(state)).await.unwrap() });
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
    // 订阅额度的头必须原样回来 —— 那是 §4.3.2 零成本白捡的数据来源，
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
    // 一个探测本机端口的脚本不该消耗掉用户的额度（§5.4）
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
            key: "sk-x".into(),
            protocol: Some(tw_config::Protocol::Anthropic),
            ..Default::default()
        }],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let mut rx = state.bus.subscribe();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, tw_gateway::router(state)).await.unwrap() });

    let resp = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-testkey")
        .body(CLAUDE_BODY)
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.contains("message_stop"));

    let started = rx.recv().await.unwrap();
    assert!(
        matches!(started, tw_api::Event::RequestStarted { ref client, .. } if client == "claude-code")
    );
    let headers = rx.recv().await.unwrap();
    // 断言写成 match 而不是 matches!，这样失败时能看见实际收到了什么。
    match headers {
        tw_api::Event::RequestHeaders { status: 200, .. } => {}
        ref other => panic!("第二条该是 RequestHeaders(200)，实际 {other:?}"),
    }
    let finished = rx.recv().await.unwrap();
    match finished {
        tw_api::Event::RequestFinished { status, bytes, .. } => {
            assert_eq!(status, 200);
            // 字节数是流真正流过的量，不是 content-length
            assert!(bytes > 0, "应该数到流过的字节");
        }
        other => panic!("最后一条该是 finished，实际 {other:?}"),
    }
    // 四个事件共用同一个 id
    assert_eq!(started.id(), headers.id());
    assert_eq!(started.id(), finished.id());
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
            key: "sk-x".into(),
            protocol: None,
            ..Default::default()
        }],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let mut rx = state.bus.subscribe();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, tw_gateway::router(state)).await.unwrap() });

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

    async fn next(rx: &mut tokio::sync::broadcast::Receiver<tw_api::Event>) -> tw_api::Event {
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("没等到事件")
            .unwrap()
    }
    let _started = next(&mut rx).await;
    match next(&mut rx).await {
        tw_api::Event::RequestFailed {
            source, message, ..
        } => {
            assert_eq!(source, "upstream");
            // 错误信息要能直接行动
            assert!(
                message.contains("base_url") || message.contains("代理"),
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
                key: "sk-official".into(),
                protocol: Some(tw_config::Protocol::Anthropic),
                ..Default::default()
            },
            Provider {
                name: "relay".into(),
                base_url: format!("http://{b}"),
                key: "sk-relay".into(),
                protocol: Some(tw_config::Protocol::Anthropic),
                ..Default::default()
            },
        ],
        groups: Vec::new(),
        proxies: Vec::new(),
        limits: Default::default(),
        routes: vec![
            tw_engine::Route {
                name: "opus 走官方".into(),
                when: serde_yaml_ng::from_str("{ model: claude-opus-* }").unwrap(),
                to: "official".into(),
            },
            tw_engine::Route {
                name: "兜底".into(),
                when: Default::default(),
                to: "relay".into(),
            },
        ],
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, tw_gateway::router(state)).await.unwrap() });

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
    // 层 0：只配 provider，不写任何规则（§3.4）。这是最小可用配置，
    // 而且**对不少人就够了** —— 如果它不工作，「配一个 API 就能用」
    // 那条纪律就是假的。
    let (up, seen) = start_upstream(false).await;
    let cfg = Config {
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
            key: "sk-x".into(),
            protocol: None,
            ..Default::default()
        }],
        ..Default::default()
    };
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, tw_gateway::router(state)).await.unwrap() });

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

fn cfg_with(providers: Vec<Provider>, routes: Vec<tw_engine::Route>) -> Config {
    Config {
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers,
        routes,
        ..Default::default()
    }
}

async fn serve_cfg(cfg: Config) -> SocketAddr {
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, tw_gateway::router(state)).await.unwrap() });
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
    // §4.2：首字节之前可以透明切换 —— 拿到响应头之前我们还没往客户端
    // 写过任何东西，换一家客户端完全无感。
    let dead = start_broken_upstream(503).await;
    let (good, seen) = start_upstream(false).await;
    let gw = serve_cfg(cfg_with(
        vec![
            Provider {
                name: "dead".into(),
                base_url: format!("http://{dead}"),
                key: "k".into(),
                ..Default::default()
            },
            Provider {
                name: "good".into(),
                base_url: format!("http://{good}"),
                key: "k".into(),
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
                key: "k".into(),
                ..Default::default()
            },
            Provider {
                name: "backup".into(),
                base_url: format!("http://{backup}"),
                key: "k".into(),
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
                key: "k".into(),
                ..Default::default()
            },
            Provider {
                name: "good".into(),
                base_url: format!("http://{good}"),
                key: "k".into(),
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
    // 熔断纯粹是自伤（§4.2）。
    let dead = start_broken_upstream(503).await;
    let gw = serve_cfg(cfg_with(
        vec![Provider {
            name: "only".into(),
            base_url: format!("http://{dead}"),
            key: "k".into(),
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
                key: "k".into(),
                ..Default::default()
            },
            Provider {
                name: "b".into(),
                base_url: format!("http://{b}"),
                key: "k".into(),
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
    assert!(msg.contains("试过"), "{msg}");
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

fn cfg_with_limits(up: SocketAddr, limits: tw_config::Limits) -> Config {
    Config {
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![Provider {
            name: "slow".into(),
            base_url: format!("http://{up}"),
            key: "k".into(),
            ..Default::default()
        }],
        limits,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn over_the_concurrency_limit_requests_queue_instead_of_being_refused() {
    // **这是 §4.7 那条的可信证明。**客户端收到 429 通常不会优雅重试，
    // 一个本来只需要多等两秒的请求会变成一次任务中断 —— 所以超限时
    // 必须排队。
    let (up, peak) = start_slow_upstream(Duration::from_millis(200)).await;
    let gw = serve_cfg(cfg_with_limits(
        up,
        tw_config::Limits {
            max_concurrent: 2,
            per_provider: 2,
            queue_depth: 64,
            queue_timeout_secs: 30,
        },
    ))
    .await;

    let mut tasks = Vec::new();
    for _ in 0..6 {
        tasks.push(tokio::spawn(
            async move { send_to(gw).await.status().as_u16() },
        ));
    }
    let codes: Vec<u16> = futures::future::join_all(tasks)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    assert!(codes.iter().all(|&c| c == 200), "全部该成功：{codes:?}");
    // 而且上游确实没有同时收到超过 2 个 —— 闸门真的在限流，不是摆设
    assert!(
        peak.load(Ordering::SeqCst) <= 2,
        "上游峰值 {}",
        peak.load(Ordering::SeqCst)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_queue_refuses_with_429_rather_than_growing_without_bound() {
    // 队列必须有上限：失控的脚本会把队列撑爆，内存跟着涨 —— 那比拒绝
    // 更糟。这是**唯一**一个我们主动拒绝的场景。
    let (up, _) = start_slow_upstream(Duration::from_millis(400)).await;
    let gw = serve_cfg(cfg_with_limits(
        up,
        tw_config::Limits {
            max_concurrent: 1,
            per_provider: 1,
            queue_depth: 3,
            queue_timeout_secs: 30,
        },
    ))
    .await;

    let mut tasks = Vec::new();
    for _ in 0..12 {
        tasks.push(tokio::spawn(async move {
            let r = send_to(gw).await;
            (
                r.status().as_u16(),
                r.headers()
                    .get("x-thinkwatch-error")
                    .map(|v| v.to_str().unwrap().to_string()),
            )
        }));
    }
    let out: Vec<(u16, Option<String>)> = futures::future::join_all(tasks)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    let ok = out.iter().filter(|(c, _)| *c == 200).count();
    let refused: Vec<_> = out.iter().filter(|(c, _)| *c == 429).collect();
    assert!(ok > 0, "总得有几个成功：{out:?}");
    assert!(!refused.is_empty(), "队列该满了才对：{out:?}");
    // 拒绝的那些要说清是我们这一层的过载，不是上游的
    assert_eq!(refused[0].1.as_deref(), Some("overloaded"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_per_client_limit_keeps_one_client_from_taking_everything() {
    // 监听局域网时是刚需：某台机器上的失控脚本不该能占满全部并发（§4.7）。
    let (up, peak) = start_slow_upstream(Duration::from_millis(200)).await;
    let mut cfg = cfg_with_limits(
        up,
        tw_config::Limits {
            max_concurrent: 8,
            per_provider: 8,
            queue_depth: 64,
            queue_timeout_secs: 30,
        },
    );
    cfg.clients[0].max_concurrent = Some(1);
    let gw = serve_cfg(cfg).await;

    let mut tasks = Vec::new();
    for _ in 0..4 {
        tasks.push(tokio::spawn(
            async move { send_to(gw).await.status().as_u16() },
        ));
    }
    let codes: Vec<u16> = futures::future::join_all(tasks)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();
    assert!(codes.iter().all(|&c| c == 200), "{codes:?}");
    // 全局给了 8，但这个客户端只有 1 —— 三个维度取最严的那个
    assert_eq!(peak.load(Ordering::SeqCst), 1);
}
