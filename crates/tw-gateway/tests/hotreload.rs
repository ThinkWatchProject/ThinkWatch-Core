//! 热重载：换的是什么，不换的是什么（第 ④⑤ 步）。
//!
//! 分堆的判据是**这个东西丢了会不会让用户感觉到**。所以这些测试断言的
//! 大多是「某个状态**没有**被重置」—— 那类回归在手工测试里几乎发现不了，
//! 因为它们的表现是「偶尔多试了一次上游」这种看起来像随机的东西。

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::routing::any;
use tw_config::{Client, Config, Provider};

mod common;
use common::spare_port;

async fn counting_upstream(name: &'static str) -> (SocketAddr, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let app = Router::new()
        .fallback(any(move |State(h): State<Arc<AtomicUsize>>| async move {
            h.fetch_add(1, Ordering::SeqCst);
            axum::response::Response::builder()
                .header("content-type", "application/json")
                .body(axum::body::Body::from(format!(r#"{{"by":"{name}"}}"#)))
                .unwrap()
        }))
        .with_state(h);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (a, hits)
}

fn cfg(providers: Vec<Provider>, routes: Vec<tw_engine::Rule>) -> Config {
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

fn provider(name: &str, at: SocketAddr) -> Provider {
    Provider {
        name: name.into(),
        base_url: format!("http://{at}"),
        key: Some("k".into()),
        protocol: Some(tw_config::Protocol::Anthropic),
        ..Default::default()
    }
}

async fn serve(state: tw_gateway::AppState) -> SocketAddr {
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

/// 起一个会数「建了几条 TCP 连接」的上游。
///
/// **连接池复用没复用，只能从这里看。**reqwest 不暴露 Client 的内部
/// 指针，而比较 Debug 输出会把两个配置相同的新旧 Client 判成同一个 ——
/// 那样的测试是空的。数连接数才是真的。
async fn connection_counting_upstream() -> (SocketAddr, Arc<AtomicUsize>) {
    let conns = Arc::new(AtomicUsize::new(0));
    let c = conns.clone();
    let app = Router::new().fallback(any(|| async {
        axum::response::Response::builder()
            .header("content-type", "application/json")
            .body(axum::body::Body::from(r#"{"by":"a"}"#))
            .unwrap()
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = l.accept().await else {
                return;
            };
            c.fetch_add(1, Ordering::SeqCst);
            let svc = app.clone();
            tokio::spawn(async move {
                let io = hyper_util::rt::TokioIo::new(stream);
                let _ = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                )
                .serve_connection(
                    io,
                    hyper::service::service_fn(move |r| {
                        use tower::ServiceExt;
                        svc.clone().oneshot(r)
                    }),
                )
                .await;
            });
        }
    });
    (addr, conns)
}

async fn ask(gw: SocketAddr) -> String {
    reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"m","messages":[]}"#)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_reload_takes_effect_on_the_very_next_request() {
    let (a, _) = counting_upstream("a").await;
    let (b, _) = counting_upstream("b").await;
    let state = tw_gateway::AppState::new(cfg(vec![provider("a", a)], vec![])).unwrap();
    let gw = serve(state.clone()).await;
    assert!(ask(gw).await.contains("\"a\""));

    state.reload(cfg(vec![provider("b", b)], vec![])).unwrap();
    assert!(ask(gw).await.contains("\"b\""), "换了配置但请求还走老路");
}

#[tokio::test]
async fn a_reload_that_cannot_build_leaves_the_old_config_serving() {
    // **桌面工具不能拒绝服务。**运行时对象建不起来（比如代理地址
    // reqwest 不认）时，旧配置必须原样继续干活。
    let (a, _) = counting_upstream("a").await;
    let state = tw_gateway::AppState::new(cfg(vec![provider("a", a)], vec![])).unwrap();
    let gw = serve(state.clone()).await;

    let mut broken = cfg(vec![provider("a", a)], vec![]);
    // 白名单写错一个字。**校验层会先挡下来**，但这一层也必须挡 ——
    // 它是最后一道，而「校验过了但运行时对象建不起来」不是不可能。
    broken.listen.gateway.bind = tw_config::Bind::All;
    broken.listen.gateway.allow_from = vec!["10.0.0.0/999".into()];
    assert!(state.reload(broken).is_err(), "坏配置该被拒绝");
    assert!(ask(gw).await.contains("\"a\""), "旧配置没能继续服务");
}

#[tokio::test]
async fn opening_the_breaker_is_announced_on_the_bus() {
    // **熔断是界面上看得见的状态，所以它必须能被推出去。**没有这条
    // 事件，界面想知道「哪家被熔断了」就只能定时去问 `/overview` ——
    // 而那是在一条完全空闲的连接上反复问同一个问题。
    let dead = {
        let app = Router::new().fallback(any(|| async {
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        a
    };
    let (good, _) = counting_upstream("good").await;
    let c = cfg(vec![provider("dead", dead), provider("good", good)], vec![]);
    let state = tw_gateway::AppState::new(c).unwrap();
    let mut rx = state.bus.subscribe();
    let gw = serve(state.clone()).await;

    for _ in 0..6 {
        ask(gw).await;
    }
    assert!(!state.health.is_available("dead"), "dead 没被熔断");

    let mut opened = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let tw_api::Event::HealthChanged {
            provider, state, ..
        } = ev
        {
            opened.push((provider, state));
        }
    }
    assert_eq!(
        opened
            .iter()
            .filter(|(p, s)| p == "dead" && *s == tw_api::BreakerState::Open)
            .count(),
        1,
        "dead 熔断了却没报，或者报了不止一次：{opened:?}"
    );
    // **成功的那家不报。**没变化的状态不该产生事件 —— 每次成功都报一条
    // 的话，这个事件就退化成了另一个请求流。
    assert!(
        !opened.iter().any(|(p, _)| p == "good"),
        "good 一直是好的，不该有状态变化：{opened:?}"
    );
}

#[tokio::test]
async fn the_circuit_breaker_state_survives_a_reload() {
    // **一家刚被熔断的上游，不该因为你改了条规则就立刻又被试一遍**。
    // 这类回归的表现是「偶尔多打了一次已知坏掉的上游」，
    // 在手工测试里几乎发现不了。
    let dead = {
        let app = Router::new().fallback(any(|| async {
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        a
    };
    let (good, good_hits) = counting_upstream("good").await;
    let c = cfg(vec![provider("dead", dead), provider("good", good)], vec![]);
    let state = tw_gateway::AppState::new(c.clone()).unwrap();
    let gw = serve(state.clone()).await;

    // 打到 dead 熔断为止
    for _ in 0..6 {
        ask(gw).await;
    }
    assert!(
        !state.health.is_available("dead"),
        "dead 没被熔断，这条测了个寂寞"
    );

    // 改一条完全无关的东西
    let mut next = c.clone();
    next.clients[0].max_concurrent = Some(7);
    state.reload(next).unwrap();
    assert!(
        !state.health.is_available("dead"),
        "重载把熔断状态重置了 —— 下一个请求会白白再打一次已知坏掉的上游"
    );
    let before = good_hits.load(Ordering::SeqCst);
    ask(gw).await;
    assert_eq!(
        good_hits.load(Ordering::SeqCst),
        before + 1,
        "请求没有直接走到健康的那家"
    );
}

#[tokio::test]
async fn the_event_stream_is_not_interrupted_by_a_reload() {
    // 界面上的实时列表不该因为改了配置断一次 —— 用户会以为是我们挂了。
    let (a, _) = counting_upstream("a").await;
    let c = cfg(vec![provider("a", a)], vec![]);
    let state = tw_gateway::AppState::new(c.clone()).unwrap();
    let gw = serve(state.clone()).await;
    // 起监听时报的那一条（`ListenChanged`）不是这里要看的：订阅从重载之前开始就够了
    let mut rx = state.bus.subscribe();

    let mut next = c.clone();
    next.clients[0].max_concurrent = Some(3);
    state.reload(next).unwrap();

    ask(gw).await;
    let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("订阅在重载时断了")
        .expect("订阅在重载时断了");
    assert!(matches!(ev, tw_api::Event::RequestStarted { .. }), "{ev:?}");
}

#[tokio::test]
async fn a_connection_pool_survives_an_unrelated_edit() {
    // 每次重载都重建 Client，等于把每个上游的连接池连同已经握好的 TLS
    // 一起扔掉。**改一条路由规则不该让下一个请求多付一次完整的建连。**
    let (a, conns) = connection_counting_upstream().await;
    let c = cfg(vec![provider("a", a)], vec![]);
    let state = tw_gateway::AppState::new(c.clone()).unwrap();
    let gw = serve(state.clone()).await;

    ask(gw).await;
    assert_eq!(conns.load(Ordering::SeqCst), 1, "第一个请求该建一条连接");

    // 改一条完全无关的规则
    let mut next = c.clone();
    next.routes = vec![tw_engine::RouteSet::default_with(vec![tw_engine::Rule {
        name: "新规则".into(),
        when: Default::default(),
        to: Some("a".into()),
        set: None,
        deny: None,
    }])];
    state.reload(next).unwrap();
    ask(gw).await;
    assert_eq!(
        conns.load(Ordering::SeqCst),
        1,
        "改一条路由规则就把连接池扔了 —— 下一个请求白付了一次建连"
    );

    // 换一把 key 更不该重建：Client 不绑凭据，凭据是每个请求现加的
    let mut next2 = c.clone();
    next2.providers[0].key = Some("另一把".into());
    state.reload(next2).unwrap();
    ask(gw).await;
    assert_eq!(conns.load(Ordering::SeqCst), 1, "改一把 key 就把连接池扔了");

    // 但改了代理就必须重建 —— 代理绑在 Client 上
    let mut next3 = c.clone();
    next3.providers[0].proxy = tw_config::SYSTEM.into();
    state.reload(next3).unwrap();
    ask(gw).await;
    assert_eq!(
        conns.load(Ordering::SeqCst),
        2,
        "改了代理却复用了旧 Client —— 流量还走老路，而且完全静默"
    );
}

#[tokio::test]
async fn changing_the_proxy_does_rebuild_that_client() {
    // 代理是绑在 Client 上的，改了就必须重建 —— 复用会让「改了代理但
    // 流量还走老路」变成可能，而那种不一致完全静默。
    let (a, _) = counting_upstream("a").await;
    let c = cfg(vec![provider("a", a)], vec![]);
    let state = tw_gateway::AppState::new(c.clone()).unwrap();

    let mut next = c.clone();
    next.providers[0].proxy = "代理一".into();
    next.proxies = vec![tw_config::Proxy {
        name: "代理一".into(),
        kind: tw_config::ProxyKind::Socks5h,
        addr: "127.0.0.1:17890".into(),
        auth: None,
    }];
    state.reload(next).unwrap();
    // 代理起不来，请求该失败 —— 这就是「真的换成新 Client 了」的证据
    let gw = serve(state.clone()).await;
    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"m","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_ne!(r.status(), 200, "改了代理但流量还走老路");
}

#[tokio::test]
async fn deleting_a_provider_takes_its_models_off_the_list_immediately() {
    // 「列表即承诺」。删掉一个上游之后 `/v1/models` 还列着它的
    // 模型，等于承诺一个已经不存在的东西。
    let (a, _) = counting_upstream("a").await;
    let (b, _) = counting_upstream("b").await;
    let mut c = cfg(vec![provider("a", a), provider("b", b)], vec![]);
    c.providers[0].models = vec!["only-on-a".into()];
    c.providers[1].models = vec!["only-on-b".into()];
    let state = tw_gateway::AppState::new(c.clone()).unwrap();
    let gw = serve(state.clone()).await;

    let list = |gw: SocketAddr| async move {
        reqwest::Client::new()
            .get(format!("http://{gw}/v1/models"))
            .header("x-api-key", "tw-k")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    };
    let before = list(gw).await;
    assert!(
        before.contains("only-on-a") && before.contains("only-on-b"),
        "{before}"
    );

    let mut next = c.clone();
    next.providers.remove(0);
    state.reload(next).unwrap();
    let after = list(gw).await;
    assert!(
        !after.contains("only-on-a"),
        "删掉的上游还在列表里：{after}"
    );
    assert!(after.contains("only-on-b"), "剩下那家不该被牵连：{after}");
}

#[tokio::test]
async fn an_unrelated_edit_does_not_blank_the_model_list() {
    // 反过来也要成立：改一条规则时把目录清空，`/v1/models` 会短暂地空
    // 一下，而客户端很可能正好在那一刻问。
    let (a, _) = counting_upstream("a").await;
    let mut c = cfg(vec![provider("a", a)], vec![]);
    c.providers[0].models = vec!["m1".into()];
    let state = tw_gateway::AppState::new(c.clone()).unwrap();
    let gw = serve(state.clone()).await;

    let mut next = c.clone();
    next.routes = vec![tw_engine::RouteSet::default_with(vec![tw_engine::Rule {
        name: "r".into(),
        when: Default::default(),
        to: Some("a".into()),
        set: None,
        deny: None,
    }])];
    state.reload(next).unwrap();
    let after = reqwest::Client::new()
        .get(format!("http://{gw}/v1/models"))
        .header("x-api-key", "tw-k")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(after.contains("m1"), "无关改动把模型目录清空了：{after}");
}

#[tokio::test]
async fn a_request_in_flight_finishes_on_the_config_it_started_with() {
    // **正在跑的请求持有旧的 Arc，跑完自然释放。**中途换配置不该让它
    // 半途改道 —— 那会让一个请求跨在两份配置上。
    let slow = {
        let app = Router::new().fallback(any(|| async {
            tokio::time::sleep(Duration::from_millis(400)).await;
            axum::response::Response::builder()
                .header("content-type", "application/json")
                .body(axum::body::Body::from(r#"{"by":"slow"}"#))
                .unwrap()
        }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        a
    };
    let (fast, fast_hits) = counting_upstream("fast").await;
    let state = tw_gateway::AppState::new(cfg(vec![provider("slow", slow)], vec![])).unwrap();
    let gw = serve(state.clone()).await;

    let inflight = tokio::spawn(async move { ask(gw).await });
    tokio::time::sleep(Duration::from_millis(80)).await;
    state
        .reload(cfg(vec![provider("fast", fast)], vec![]))
        .unwrap();

    let body = inflight.await.unwrap();
    assert!(body.contains("slow"), "跑到一半的请求被换了上游：{body}");
    assert_eq!(fast_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn reloading_from_zero_providers_to_one_starts_working_without_a_restart() {
    // 首次运行的那条路：core 先起来（零 provider），用户在界面上加了
    // 第一个上游，然后**不重启**就该能用。
    let (a, _) = counting_upstream("a").await;
    let state = tw_gateway::AppState::new(cfg(vec![], vec![])).unwrap();
    let gw = serve(state.clone()).await;
    let first = ask(gw).await;
    assert!(first.contains("No upstream is configured"), "{first}");

    state.reload(cfg(vec![provider("a", a)], vec![])).unwrap();
    assert!(ask(gw).await.contains("\"a\""), "加完上游还是不通");
}

#[tokio::test]
async fn the_allow_list_is_reloaded_too() {
    // 来源白名单改了却不生效，是个安全问题而不只是个体验问题。
    let (a, _) = counting_upstream("a").await;
    let mut c = cfg(vec![provider("a", a)], vec![]);
    let state = tw_gateway::AppState::new(c.clone()).unwrap();
    let gw = serve(state.clone()).await;
    assert!(ask(gw).await.contains("\"a\""));

    let other: std::net::IpAddr = "192.168.7.7".parse().unwrap();
    assert!(state.runtime().allow.allows(other), "默认名单是私网段");

    // 监听改成所有网卡，只放行一个不含那台设备的段
    c.listen.gateway.bind = tw_config::Bind::All;
    c.listen.gateway.allow_from = vec!["10.0.0.0/8".into()];
    state.reload(c).unwrap();
    assert!(!state.runtime().allow.allows(other), "白名单没生效");
    // 本机永远放行：名单管的是别的设备
    assert!(ask(gw).await.contains("\"a\""), "本机被新名单挡在了外面");
}

#[tokio::test]
async fn changing_the_port_actually_moves_the_listener() {
    // **「温」那一级**。换端口不能只换配置：监听器是启动时建的，
    // 不重建的话新端口上什么都没有，而旧端口还在服务 —— 那种「改了没
    // 反应」比报错难查得多。
    let (up, _) = counting_upstream("a").await;
    let mut c = cfg(vec![provider("a", up)], vec![]);
    let (p1, p2) = (spare_port(), spare_port());
    c.listen.gateway.port = p1;
    let state = tw_gateway::AppState::new(c.clone()).unwrap();
    let mut events = state.bus.subscribe();
    following(&state, &c).await;
    assert_eq!(primary(&state), Some(p1));

    let at = |port: u16| async move {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", "tw-k")
            .timeout(Duration::from_secs(2))
            .body(r#"{"model":"m","messages":[]}"#)
            .send()
            .await
    };
    assert!(at(p1).await.is_ok(), "起始端口不通");

    let mut next = c.clone();
    next.listen.gateway.port = p2;
    state.reload(next).unwrap();
    // 重建监听器要一小会儿
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(at(p2).await.is_ok(), "新端口上什么都没有");
    assert!(at(p1).await.is_err(), "旧端口还在服务");
    // **界面上的地址跟着走**：状态和事件都说新端口，不是启动时的那个
    assert_eq!(primary(&state), Some(p2));
    let said = listen_events(&mut events);
    assert_eq!(
        said.last().and_then(|(a, _)| a.clone()),
        Some(format!("127.0.0.1:{p2}")),
        "{said:?}"
    );
}

/// 起一个跟着配置走的网关，等它绑上。
async fn following(state: &tw_gateway::AppState, c: &Config) {
    let s = state.clone();
    let want = c.listen.gateway.clone();
    tokio::spawn(async move { tw_gateway::serve_at(s, want, true).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(80)).await;
}

fn primary(state: &tw_gateway::AppState) -> Option<u16> {
    state.listening().primary().map(|a| a.port())
}

/// 到现在为止发出来的换监听事件：(地址, 没换成的原因的码)
fn listen_events(
    rx: &mut tokio::sync::broadcast::Receiver<tw_api::Event>,
) -> Vec<(Option<String>, Option<String>)> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if let tw_api::Event::ListenChanged { addr, error, .. } = e {
            out.push((addr, error.map(|m| m.code)));
        }
    }
    out
}

#[tokio::test]
async fn a_port_already_taken_keeps_the_old_listener_and_says_why() {
    // **绑不上就什么都不动。**以前这一步失败会让整个网关退出，守护进程再按
    // 同一份配置拉起来、再失败 —— 用户改了个端口，换来的是所有客户端断线
    let (up, _) = counting_upstream("a").await;
    let mut c = cfg(vec![provider("a", up)], vec![]);
    // 占着的程序后面会退出、把端口让给网关：放掉的那一下不能有别人插进来，
    // 所以它占的也是一个系统不会转手的号
    let taken = spare_port();
    let squatter = std::net::TcpListener::bind(("127.0.0.1", taken)).unwrap();
    let p1 = spare_port();
    c.listen.gateway.port = p1;
    let state = tw_gateway::AppState::new(c.clone()).unwrap();
    let mut events = state.bus.subscribe();
    following(&state, &c).await;

    let mut next = c.clone();
    next.listen.gateway.port = taken;
    state.reload(next).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let gw: SocketAddr = format!("127.0.0.1:{p1}").parse().unwrap();
    assert!(ask(gw).await.contains("\"a\""), "旧端口不该停");
    let now = state.listening();
    assert_eq!(primary(&state), Some(p1), "地址该是还在服务的那个");
    assert_eq!(
        now.error.as_ref().map(|m| m.code.as_str()),
        Some("gw.listen.port_taken"),
        "{now:?}"
    );
    assert!(
        listen_events(&mut events)
            .iter()
            .any(|(_, e)| e.as_deref() == Some("gw.listen.port_taken")),
        "没换成也要说一声"
    );

    // 占着的程序退了，再存一次同样的配置：这回换过去，那句话也跟着消失
    drop(squatter);
    state.relisten();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(primary(&state), Some(taken));
    assert!(state.listening().error.is_none());
}

#[tokio::test]
async fn a_request_in_flight_survives_the_listener_being_rebuilt() {
    // **新的先起来，老的停止接受新连接并等现有请求自然结束**。
    // 一个跑了六分钟的流不该因为你改了个端口而断掉。
    let slow = {
        let app = Router::new().fallback(any(|| async {
            tokio::time::sleep(Duration::from_millis(600)).await;
            axum::response::Response::builder()
                .header("content-type", "application/json")
                .body(axum::body::Body::from(r#"{"by":"slow"}"#))
                .unwrap()
        }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        a
    };
    let mut c = cfg(vec![provider("slow", slow)], vec![]);
    let (p1, p2) = (spare_port(), spare_port());
    c.listen.gateway.port = p1;
    let state = tw_gateway::AppState::new(c.clone()).unwrap();
    following(&state, &c).await;

    let inflight = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{p1}/v1/messages"))
            .header("x-api-key", "tw-k")
            .timeout(Duration::from_secs(10))
            .body(r#"{"model":"m","messages":[]}"#)
            .send()
            .await
    });
    tokio::time::sleep(Duration::from_millis(120)).await;
    let mut next = c.clone();
    next.listen.gateway.port = p2;
    state.reload(next).unwrap();

    // **新端口不等旧请求跑完就要能连。**以前是等旧的那一个排空了才去绑
    // 新的 —— 一个长的流在跑时，这几分钟里两边都不接新连接
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(!inflight.is_finished(), "测试的前提：旧请求还在跑");
    let probe = tokio::net::TcpStream::connect(("127.0.0.1", p2)).await;
    assert!(probe.is_ok(), "旧请求还没跑完，新端口就该能连了");

    let r = inflight
        .await
        .unwrap()
        .expect("换监听器把跑到一半的请求掐了");
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("slow"));
}

fn bind(raw: &str) -> tw_config::Bind {
    serde_yaml_ng::from_str(raw).unwrap()
}

/// 换一次监听，等它换完（看状态里的地址，不靠睡多久）。
async fn switch(state: &tw_gateway::AppState, next: Config) {
    let want = next.listen.gateway.addrs().unwrap();
    state.reload(next).unwrap();
    for _ in 0..100 {
        let now = state.listening();
        if now.addrs == want || now.error.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the listener never switched: {:?}", state.listening());
}

#[tokio::test]
async fn switching_between_all_interfaces_and_one_address_on_the_same_port_takes_effect() {
    // **同一个端口上，通配地址和具体地址不能并存**（Linux）：`bind: all` 换成
    // 一张网卡时，新的 127.0.0.1 撞上的是我们自己还没退场的 0.0.0.0。以前这一步
    // 报「端口被占」然后守着旧的，要重启才生效
    let (up, _) = counting_upstream("a").await;
    let mut c = cfg(vec![provider("a", up)], vec![]);
    let port = spare_port();
    c.listen.gateway.port = port;
    c.listen.gateway.bind = bind("all");
    let state = tw_gateway::AppState::new(c.clone()).unwrap();
    following(&state, &c).await;
    let gw: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    assert!(ask(gw).await.contains("\"a\""));

    for to in ["127.0.0.1", "all", "127.0.0.1"] {
        let mut next = c.clone();
        next.listen.gateway.bind = bind(to);
        switch(&state, next.clone()).await;
        let now = state.listening();
        assert_eq!(now.error, None, "switching to {to}: {now:?}");
        assert_eq!(
            now.addrs,
            next.listen.gateway.addrs().unwrap(),
            "switching to {to}"
        );
        assert!(
            ask(gw).await.contains("\"a\""),
            "nothing answers after switching to {to}"
        );
    }
}

#[tokio::test]
async fn the_pre_save_check_does_not_call_our_own_port_taken() {
    // 控制面在写配置之前问「绑不绑得上」。挡路的是我们自己在听的那一个时，
    // 不该回答「被别的程序占了」—— 那样这份设置根本存不进去
    let (up, _) = counting_upstream("a").await;
    let mut c = cfg(vec![provider("a", up)], vec![]);
    c.listen.gateway.port = spare_port();
    c.listen.gateway.bind = bind("all");
    let state = tw_gateway::AppState::new(c.clone()).unwrap();
    following(&state, &c).await;
    let mut next = c.clone();
    next.listen.gateway.bind = bind("127.0.0.1");
    let want = next.listen.gateway.addrs().unwrap();
    assert_eq!(tw_gateway::listen::check(&state, &want).await, Ok(()));

    // 真被别人占着的，照样说
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = squatter.local_addr().unwrap();
    let e = tw_gateway::listen::check(&state, &[taken])
        .await
        .unwrap_err();
    assert_eq!(e.code, "gw.listen.port_taken");
}

#[tokio::test]
async fn a_request_in_flight_survives_the_old_listener_giving_way() {
    // 旧的先让出端口时，**让的只是监听**：已经连上、跑到一半的请求照常跑完
    let slow = {
        let app = Router::new().fallback(any(|| async {
            tokio::time::sleep(Duration::from_millis(600)).await;
            axum::response::Response::builder()
                .header("content-type", "application/json")
                .body(axum::body::Body::from(r#"{"by":"slow"}"#))
                .unwrap()
        }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        a
    };
    let mut c = cfg(vec![provider("slow", slow)], vec![]);
    let port = spare_port();
    c.listen.gateway.port = port;
    c.listen.gateway.bind = bind("all");
    let state = tw_gateway::AppState::new(c.clone()).unwrap();
    following(&state, &c).await;

    let inflight = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", "tw-k")
            .timeout(Duration::from_secs(10))
            .body(r#"{"model":"m","messages":[]}"#)
            .send()
            .await
    });
    tokio::time::sleep(Duration::from_millis(120)).await;
    let mut next = c.clone();
    next.listen.gateway.bind = bind("127.0.0.1");
    switch(&state, next).await;
    assert_eq!(state.listening().error, None);
    assert!(!inflight.is_finished(), "测试的前提：旧请求还在跑");
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok(),
        "旧请求还没跑完，新的监听就该能连了"
    );
    let r = inflight
        .await
        .unwrap()
        .expect("让出端口把跑到一半的请求掐了");
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn when_the_new_address_is_really_taken_the_old_one_is_taken_back() {
    // 让出来之后新的还是绑不上（这回真是别的程序）：旧的原样绑回去，并说清原因。
    //
    // 场景：0.0.0.0:p 在听，换到 [::1]:p，而 [::1]:p 被别人占着。同端口，所以
    // 走「先让出来」那条路；让了也没用，于是回到 0.0.0.0:p
    let port = spare_port();
    let Ok(squatter) = std::net::TcpListener::bind(("::1", port)) else {
        eprintln!("no IPv6 loopback here; skipping");
        return;
    };
    let (up, _) = counting_upstream("a").await;
    let mut c = cfg(vec![provider("a", up)], vec![]);
    c.listen.gateway.port = port;
    c.listen.gateway.bind = bind("all");
    let state = tw_gateway::AppState::new(c.clone()).unwrap();
    following(&state, &c).await;
    let before = state.listening().addrs;

    let mut next = c.clone();
    next.listen.gateway.bind = bind("::1");
    switch(&state, next).await;
    let now = state.listening();
    assert_eq!(
        now.error.as_ref().map(|m| m.code.as_str()),
        Some("gw.listen.port_taken"),
        "{now:?}"
    );
    assert_eq!(now.addrs, before, "旧的该原样回来");
    let gw: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    assert!(ask(gw).await.contains("\"a\""), "旧的地址不该停");
    drop(squatter);
}

/// 起一个网关，**起始那一次绑定不许失败**：等它交回监听状态。
async fn started(state: &tw_gateway::AppState, c: &Config) -> tw_gateway::Listening {
    let s = state.clone();
    let listen = c.listen.gateway.clone();
    let serving = tokio::spawn(async move { tw_gateway::serve_at(s, listen, true).await });
    for _ in 0..100 {
        assert!(
            !serving.is_finished(),
            "the gateway gave up: {:?}",
            serving.await
        );
        let now = state.listening();
        if !now.addrs.is_empty() {
            return now;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the gateway never started listening");
}

#[tokio::test]
async fn an_interface_that_is_not_there_yet_does_not_stop_the_gateway_from_starting() {
    // **开机自启时 WSL 还没起来**，它的虚拟网卡也就还不在。以前这一步让 twcore
    // 直接退出，这台电脑上的客户端跟着全部连不上；现在先只听回环，网卡出现了
    // 再补上
    let (up, _) = counting_upstream("a").await;
    let mut c = cfg(vec![provider("a", up)], vec![]);
    let port = spare_port();
    c.listen.gateway.port = port;
    c.listen.gateway.bind = bind("tw-not-here0");
    let state = tw_gateway::AppState::new(c.clone()).unwrap();
    let now = started(&state, &c).await;

    let lo: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    assert_eq!(now.addrs, [lo]);
    assert_eq!(
        now.error.as_ref().map(|m| m.code.as_str()),
        Some("gw.listen.no_such_nic"),
        "界面上要说清为什么只听了本机: {now:?}"
    );
    assert!(ask(lo).await.contains("\"a\""), "回环该照常服务");

    // 换成一个解析得出来的设置：照常换过去，那句话也跟着消失
    let mut next = c.clone();
    next.listen.gateway.bind = bind("all");
    let want = next.listen.gateway.addrs().unwrap();
    state.reload(next).unwrap();
    // `switch` 看见错误就回来，而这时错误本来就在：等地址换过去
    for _ in 0..100 {
        if state.listening().addrs == want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let now = state.listening();
    assert_eq!(now.addrs, want, "{now:?}");
    assert_eq!(now.error, None, "{now:?}");
    assert!(ask(lo).await.contains("\"a\""));
}

#[tokio::test]
async fn an_address_that_is_not_on_this_machine_yet_does_not_stop_the_gateway_from_starting() {
    // 写死的地址此刻不是这台机器的（网卡还没拿到它）：和网卡不在一样，先只听
    // 回环。198.51.100.1 是文档专用的地址段，哪台机器上都不会有
    let (up, _) = counting_upstream("a").await;
    let mut c = cfg(vec![provider("a", up)], vec![]);
    let port = spare_port();
    c.listen.gateway.port = port;
    c.listen.gateway.bind = bind("198.51.100.1");
    let state = tw_gateway::AppState::new(c.clone()).unwrap();
    let now = started(&state, &c).await;

    let lo: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    assert_eq!(now.addrs, [lo]);
    assert_eq!(
        now.error.as_ref().map(|m| m.code.as_str()),
        Some("gw.listen.addr_unavailable"),
        "{now:?}"
    );
    assert!(ask(lo).await.contains("\"a\""));
}

#[tokio::test]
async fn a_taken_loopback_port_still_stops_the_gateway_from_starting() {
    // 回环那一个绑不上不是「还没出现」：端口被占，照旧交给调用方，守护进程
    // 据此重启或进安全模式
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = squatter.local_addr().unwrap().port();
    let (up, _) = counting_upstream("a").await;
    let mut c = cfg(vec![provider("a", up)], vec![]);
    c.listen.gateway.port = port;
    c.listen.gateway.bind = bind("tw-not-here0");
    let state = tw_gateway::AppState::new(c.clone()).unwrap();
    let r = tokio::time::timeout(
        Duration::from_secs(2),
        tw_gateway::serve_at(state, c.listen.gateway.clone(), true),
    )
    .await
    .expect("serve_at should give up rather than wait");
    assert_eq!(
        r.unwrap_err().kind(),
        std::io::ErrorKind::AddrInUse,
        "端口被占要照旧报出来"
    );
}

#[tokio::test]
async fn a_new_port_takes_effect_on_loopback_while_the_interface_is_still_missing() {
    // 网卡还没出现、只在听回环时改了端口：没有什么正在工作的要守，回环换到
    // 新端口上，网卡那一个照旧等它出现
    let (up, _) = counting_upstream("a").await;
    let mut c = cfg(vec![provider("a", up)], vec![]);
    c.listen.gateway.port = spare_port();
    c.listen.gateway.bind = bind("tw-not-here0");
    let state = tw_gateway::AppState::new(c.clone()).unwrap();
    started(&state, &c).await;

    let p2 = spare_port();
    let lo2: SocketAddr = format!("127.0.0.1:{p2}").parse().unwrap();
    let mut next = c.clone();
    next.listen.gateway.port = p2;
    state.reload(next).unwrap();
    for _ in 0..100 {
        if state.listening().addrs == [lo2] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let now = state.listening();
    assert_eq!(now.addrs, [lo2], "{now:?}");
    assert_eq!(
        now.error.as_ref().map(|m| m.code.as_str()),
        Some("gw.listen.no_such_nic"),
        "{now:?}"
    );
    assert!(ask(lo2).await.contains("\"a\""));
}
