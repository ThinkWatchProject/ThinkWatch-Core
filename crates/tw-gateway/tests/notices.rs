//! 要发通知的那几件事，在数据面这一侧。
//!
//! 桌面版据此决定弹不弹系统通知，所以每一条的要求都一样：**只在状态变化那一刻发一次**，
//! 恢复也要说。这里验的是两件熔断器看不见的事 ——
//!
//! - 上游拒绝凭据（401/403）：4xx 不算失败，那样的上游永远不会被熔断
//! - 代理不通：不检查的话，界面上看到的是「好几家上游同时不通」

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tw_config::{Client, Config, Listen, Provider};

// ---------------------------------------------------------------- 假上游

/// 先回 `reject` 次 401，之后回 200
async fn upstream(reject: usize) -> SocketAddr {
    let left = Arc::new(AtomicUsize::new(reject));
    let app = axum::Router::new()
        .route(
            "/v1/messages",
            axum::routing::post(|axum::extract::State(left): axum::extract::State<Arc<AtomicUsize>>| async move {
                if left.load(Ordering::SeqCst) > 0 {
                    left.fetch_sub(1, Ordering::SeqCst);
                    return (
                        axum::http::StatusCode::UNAUTHORIZED,
                        axum::Json(json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}})),
                    );
                }
                (
                    axum::http::StatusCode::OK,
                    axum::Json(json!({
                        "id": "msg_1",
                        "type": "message",
                        "role": "assistant",
                        "model": "claude-sonnet-4-5",
                        "content": [{"type": "text", "text": "ok"}],
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 5, "output_tokens": 2}
                    })),
                )
            }),
        )
        .with_state(left);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

// ---------------------------------------------------------------- 假代理

/// 一个能坏掉的 HTTP 代理：认 `CONNECT`，也认绝对形式的请求。
///
/// **两种都要认**：转发走的是绝对形式，而代理检查走的是 `CONNECT`。坏掉的时候
/// 连上就断，这正是代理进程没了的样子。
async fn proxy(broken: Arc<AtomicBool>) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut down, _)) = l.accept().await else {
                return;
            };
            if broken.load(Ordering::SeqCst) {
                let _ = down.shutdown().await;
                continue;
            }
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match down.read(&mut byte).await {
                        Ok(1) => head.push(byte[0]),
                        _ => return,
                    }
                }
                let text = String::from_utf8_lossy(&head).to_string();
                let mut parts = text.split_whitespace();
                let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                let (host, rest) = if method == "CONNECT" {
                    (target.to_string(), None)
                } else {
                    // `POST http://127.0.0.1:1234/v1/messages HTTP/1.1`
                    let after = target.strip_prefix("http://").unwrap_or(target);
                    let (h, path) = after.split_once('/').unwrap_or((after, ""));
                    (h.to_string(), Some(format!("/{path}")))
                };
                let Ok(mut up) = tokio::net::TcpStream::connect(&host).await else {
                    return;
                };
                match rest {
                    None => {
                        let _ = down
                            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                            .await;
                    }
                    Some(path) => {
                        let head = text.replacen(target, &path, 1);
                        let _ = up.write_all(head.as_bytes()).await;
                    }
                }
                let _ = tokio::io::copy_bidirectional(&mut down, &mut up).await;
            });
        }
    });
    addr
}

// ---------------------------------------------------------------- 网关

fn config(p: Provider) -> Config {
    Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        providers: vec![p],
        ..Default::default()
    }
}

/// 起一个网关。**状态也交出来**：变化靠事件，现状要从它上面读
async fn serve(
    cfg: Config,
) -> (
    SocketAddr,
    tokio::sync::broadcast::Receiver<tw_api::Event>,
    tw_gateway::AppState,
) {
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let rx = state.bus.subscribe();
    let kept = state.clone();
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    (addr, rx, kept)
}

async fn ask(gw: SocketAddr) -> u16 {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{gw}/v1/messages"))
        .header("content-type", "application/json")
        .header("x-api-key", "tw-k")
        .body(json!({"model": "claude-sonnet-4-5", "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]}).to_string())
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// 等一条自己关心的事件。**别的事件要跳过**：一次请求会发好几条
async fn next_change(
    rx: &mut tokio::sync::broadcast::Receiver<tw_api::Event>,
    want: &str,
) -> (String, Option<String>) {
    let want = want.to_string();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await.unwrap() {
                tw_api::Event::AuthChanged {
                    provider, state, ..
                } if want == "auth" => return (state.slug().to_string(), Some(provider)),
                tw_api::Event::ProxyChanged {
                    proxy,
                    state,
                    detail,
                    ..
                } if want == "proxy" => {
                    assert_eq!(proxy, "代理一");
                    // **返回码，不返回句子。**这条测试要的是「说清卡在哪一步」，
                    // 而句子随时会改措辞 —— 比字符串的话，改一个词就红
                    return (state.slug().to_string(), detail.map(|d| d.code));
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("10 秒内没有等到 {want} 的状态变化"))
}

// ---------------------------------------------------------------- 用例

#[tokio::test]
async fn an_upstream_that_rejects_the_credential_is_reported_once_and_again_when_it_recovers() {
    let up = upstream(2).await;
    let (gw, mut rx, st) = serve(config(Provider {
        name: "relay".into(),
        base_url: format!("http://{up}"),
        key: Some(tw_config::Secret::new("sk-stale")),
        ..Default::default()
    }))
    .await;

    assert_eq!(ask(gw).await, 401);
    assert_eq!(next_change(&mut rx, "auth").await.0, "rejected");
    // **现状也要问得到**：界面晚打开就错过了那条事件，概览靠的是它
    assert_eq!(st.auth_rejected("relay"), Some(401));
    // 第二次还是 401：**同一件事不再说第二遍**
    assert_eq!(ask(gw).await, 401);
    // 凭据又被接受了
    assert_eq!(ask(gw).await, 200);
    let (state, provider) = next_change(&mut rx, "auth").await;
    assert_eq!(
        (state.as_str(), provider.as_deref()),
        ("accepted", Some("relay"))
    );
    assert_eq!(st.auth_rejected("relay"), None);
}

#[tokio::test]
async fn a_dead_proxy_is_named_as_the_cause_and_reported_again_when_it_comes_back() {
    let up = upstream(0).await;
    let broken = Arc::new(AtomicBool::new(true));
    let px = proxy(broken.clone()).await;
    let mut cfg = config(Provider {
        name: "relay".into(),
        base_url: format!("http://{up}"),
        key: Some(tw_config::Secret::new("sk-good")),
        proxy: "代理一".into(),
        ..Default::default()
    });
    cfg.proxies = vec![tw_config::Proxy {
        name: "代理一".into(),
        kind: tw_config::ProxyKind::Http,
        addr: px.to_string(),
        auth: None,
    }];
    let (gw, mut rx, st) = serve(cfg).await;

    // 代理连上就断：请求失败，而失败的是代理不是上游
    assert_ne!(ask(gw).await, 200);
    let (state, code) = next_change(&mut rx, "proxy").await;
    assert_eq!(state, "unreachable");
    assert!(
        code.as_ref().is_some_and(|c| c.starts_with("l1.")),
        "要说清卡在哪一步"
    );
    // **现状和事件说的是同一件事**：界面晚打开时读的是这一份
    let fault = st.proxy_fault("代理一").expect("不通的代理，现状里没有");
    assert_eq!(Some(fault.detail.code), code);
    assert!(fault.failed.is_some(), "要说清卡在哪一步");

    broken.store(false, Ordering::SeqCst);
    assert_eq!(ask(gw).await, 200);
    assert_eq!(next_change(&mut rx, "proxy").await.0, "reachable");
    assert!(st.proxy_fault("代理一").is_none(), "通了，现状还说不通");
}
