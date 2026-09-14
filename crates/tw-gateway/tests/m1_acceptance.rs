//! M1 的验收标准，逐条可执行。
//!
//! 原文四条：
//!
//! > Opus 走官方、Haiku 走中转；经代理访问官方成功而本地 Ollama 不受
//! > 影响；一个 6 分钟的长任务不被中间层掐断；上游返回 401 时 Claude
//! > Code 里显示的是可读的、能看出来自哪一层的错误。
//!
//! **把验收标准写成测试，而不是写成一份手动清单。**手动清单只会在里程碑
//! 那天跑一次，之后每一次改动都可能悄悄破坏它们。

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::routing::any;
use tw_config::{Client, Config, Provider};

/// 会说出自己是谁的上游。
async fn named_upstream(name: &'static str) -> SocketAddr {
    let app = Router::new().fallback(any(move || async move {
        axum::response::Response::builder()
            .header("content-type", "application/json")
            .body(axum::body::Body::from(format!(
                r#"{{"served_by":"{name}"}}"#
            )))
            .unwrap()
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

fn provider(name: &str, at: SocketAddr) -> Provider {
    Provider {
        name: name.into(),
        base_url: format!("http://{at}"),
        key: "sk-x".into(),
        protocol: Some(tw_config::Protocol::Anthropic),
        ..Default::default()
    }
}

async fn serve(cfg: Config) -> SocketAddr {
    let state = tw_gateway::AppState::new(cfg).unwrap();
    let addr = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move { tw_gateway::serve(state, addr).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

fn base(providers: Vec<Provider>, routes: Vec<tw_engine::Rule>) -> Config {
    Config {
        clients: vec![Client {
            name: "claude-code".into(),
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

async fn ask(gw: SocketAddr, model: &str) -> String {
    reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(format!(r#"{{"model":"{model}","messages":[]}}"#))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

#[tokio::test]
async fn one_opus_goes_to_official_and_haiku_goes_to_the_relay() {
    let official = named_upstream("official").await;
    let relay = named_upstream("relay").await;
    let gw = serve(base(
        vec![provider("official", official), provider("relay", relay)],
        vec![
            tw_engine::Rule {
                name: "opus 走官方".into(),
                when: serde_yaml_ng::from_str("{ model: claude-opus-* }").unwrap(),
                to: Some("official".into()),
                set: None,
                deny: None,
                guard: None,
            },
            tw_engine::Rule {
                name: "兜底走中转".into(),
                when: Default::default(),
                to: Some("relay".into()),
                set: None,
                deny: None,
                guard: None,
            },
        ],
    ))
    .await;
    assert!(ask(gw, "claude-opus-4").await.contains("official"));
    assert!(ask(gw, "claude-3-5-haiku-20241022").await.contains("relay"));
}

/// 一个最小的 HTTP 代理：只认 CONNECT 之外的绝对形式请求，转发出去。
///
/// 用真代理而不是设 `HTTP_PROXY` 环境变量：环境变量是进程全局的，会污染
/// 同一个测试二进制里并行跑的别的用例，而那种失败是随机的。
async fn counting_http_proxy() -> (SocketAddr, Arc<AtomicUsize>) {
    let seen = Arc::new(AtomicUsize::new(0));
    let s = seen.clone();
    let app = Router::new()
        .fallback(any(
            |State(s): State<Arc<AtomicUsize>>, req: axum::extract::Request| async move {
                s.fetch_add(1, Ordering::SeqCst);
                let url = req.uri().to_string();
                let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                    .await
                    .unwrap();
                let r = reqwest::Client::builder()
                    .no_proxy()
                    .build()
                    .unwrap()
                    .post(&url)
                    .body(body)
                    .send()
                    .await
                    .unwrap();
                axum::response::Response::builder()
                    .status(r.status().as_u16())
                    .body(axum::body::Body::from(r.bytes().await.unwrap()))
                    .unwrap()
            },
        ))
        .with_state(s);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (a, seen)
}

#[tokio::test]
async fn two_the_proxied_upstream_goes_through_it_and_the_local_one_does_not() {
    // **「本地 Ollama 不受影响」是这条验收的重点。**代理是 per-provider
    // 的 —— 一个全局开关会让本地上游必然连不上，而配置文件里
    // 看不出任何线索。
    let (proxy, hits) = counting_http_proxy().await;
    let official = named_upstream("official").await;
    let local = named_upstream("local").await;

    let mut cfg = base(
        vec![
            Provider {
                proxy: "代理一".into(),
                ..provider("official", official)
            },
            // 不写 proxy 就是 direct，而 direct 会**强制**忽略一切系统
            // 代理设置 —— 显式优于隐式。
            provider("local", local),
        ],
        vec![
            tw_engine::Rule {
                name: "本地模型走本地".into(),
                when: serde_yaml_ng::from_str("{ model: 'llama*' }").unwrap(),
                to: Some("local".into()),
                set: None,
                deny: None,
                guard: None,
            },
            tw_engine::Rule {
                name: "其余走官方".into(),
                when: Default::default(),
                to: Some("official".into()),
                set: None,
                deny: None,
                guard: None,
            },
        ],
    );
    cfg.proxies = vec![tw_config::Proxy {
        name: "代理一".into(),
        kind: tw_config::ProxyKind::Http,
        addr: proxy.to_string(),
        auth: None,
    }];
    let gw = serve(cfg).await;

    assert!(ask(gw, "claude-opus-4").await.contains("official"));
    assert_eq!(hits.load(Ordering::SeqCst), 1, "走官方那次该经过代理");

    assert!(ask(gw, "llama3").await.contains("local"));
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "本地上游不该碰代理 —— 碰了就连不上了"
    );
}

#[tokio::test]
async fn three_a_slow_task_is_not_cut_off_by_us() {
    // 完整的验收是「6 分钟」，那在 CI 里跑不了。**但会掐断的不是时长
    // 本身，是有没有一个整体超时** —— 所以这里验的是那件事：一个明显
    // 长于任何合理默认值的请求，我们不动它。
    //
    // 配一个整体超时的话，这条会立刻响。
    let slow = {
        let app = Router::new().fallback(any(|| async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            axum::response::Response::builder()
                .header("content-type", "application/json")
                .body(axum::body::Body::from(r#"{"served_by":"slow"}"#))
                .unwrap()
        }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        a
    };
    let gw = serve(base(vec![provider("slow", slow)], vec![])).await;
    let started = std::time::Instant::now();
    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        // 客户端自己愿意等多久由它决定 —— 我们比它更早放弃才是问题
        .timeout(Duration::from_secs(30))
        .body(r#"{"model":"m","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("slow"));
    assert!(started.elapsed() >= Duration::from_secs(3), "上游其实没慢");
}

#[tokio::test]
async fn four_an_upstream_401_is_readable_and_you_can_tell_which_layer_it_came_from() {
    // 用户手里有**两把 key**：网关的和上游的。一个笼统的 401 会让他去
    // 查错的那一把 —— 而这正是「浪费时间问错人」的原型。
    let unauthorised = {
        let app = Router::new().fallback(any(|| async {
            axum::response::Response::builder()
                .status(401)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
                ))
                .unwrap()
        }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        a
    };
    let gw = serve(base(vec![provider("中转-A", unauthorised)], vec![])).await;
    let c = reqwest::Client::new();

    let r = c
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"m","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    // 是哪一家说的，写在头上
    assert_eq!(r.headers().get("x-thinkwatch-upstream").unwrap(), "中转-A");
    let t = r.text().await.unwrap();
    // **上游的话原样透传，不加我们的前缀** —— 加了就意味着「这是我们的
    // 判断」，而这确实是它说的。
    assert!(t.contains("invalid x-api-key"), "{t}");
    assert!(!t.contains("[ThinkWatch]"), "上游的话被我们改写了：{t}");

    // 对照组：网关自己的 401 带前缀，而且不带 upstream 头 —— 两者一眼
    // 能分开，这就是「看得出来自哪一层」。
    let r = c
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "错的")
        .body(r#"{"model":"m","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert!(r.headers().get("x-thinkwatch-upstream").is_none());
    let t = r.text().await.unwrap();
    assert!(t.contains("[ThinkWatch]"), "{t}");
    assert!(t.contains("网关密钥"), "得说清是哪把 key：{t}");
}

/// 上游成功时也带这个头 —— 它是「谁服务的」而不是「谁出错了」。
#[tokio::test]
async fn the_upstream_header_is_there_on_success_too_so_the_ui_can_always_show_it() {
    let a = named_upstream("official").await;
    let gw = serve(base(vec![provider("official", a)], vec![])).await;
    let r = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-k")
        .body(r#"{"model":"m","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.headers().get("x-thinkwatch-upstream").unwrap(),
        "official"
    );
}

/// **单点查询要走和列表同一道准入**。
///
/// 不接这条路的话它掉进 fallback 直接透传上游 —— 一个被 `allow` 限制成
/// 只能用便宜模型的 client，`GET /v1/models/claude-opus-4` 照样拿 200。
/// 这个洞在别的项目里点过名（「都没做过滤」），而我们自己也漏了。
#[tokio::test]
async fn a_single_model_lookup_obeys_the_same_allow_list_as_the_list() {
    let up = named_upstream("官方").await;
    let mut p = provider("官方", up);
    p.models = vec!["claude-haiku-4-5".into(), "claude-opus-4".into()];
    let mut cfg = base(vec![p], vec![]);
    cfg.clients[0].allow = Some(vec!["claude-haiku-*".into()]);
    let gw = serve(cfg).await;
    let c = reqwest::Client::new();
    let get = |path: String| {
        let c = c.clone();
        let url = format!("http://{gw}{path}");
        async move { c.get(url).header("x-api-key", "tw-k").send().await.unwrap() }
    };
    // 列表里只有被许可的那个
    let list = get("/v1/models".into()).await.text().await.unwrap();
    assert!(list.contains("claude-haiku-4-5"), "{list}");
    assert!(!list.contains("claude-opus-4"), "列表漏了：{list}");

    // 被许可的单点查得到
    assert_eq!(
        get("/v1/models/claude-haiku-4-5".into()).await.status(),
        200
    );

    // **不许可的要当它不存在**，而不是 403 —— 回 403 等于告诉对方
    // 「这个模型在，只是你不能用」，而列表里根本没列它
    let r = get("/v1/models/claude-opus-4".into()).await;
    assert_ne!(r.status(), 200, "**限制了的模型单点还是查得到**");
    let body = r.text().await.unwrap();
    assert!(body.contains("没有叫"), "{body}");
}
