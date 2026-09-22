//! 保存监听设置（`PUT /listen`）。
//!
//! 断言落在两处：**配置文件本身**（写进去的是什么、默认值有没有留下痕迹），
//! 和**绑不上时什么都没写** —— 这个接口存在的理由就是后者。

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

struct Bed {
    dir: tempfile::TempDir,
    app: axum::Router,
    gw: tw_gateway::AppState,
}

impl Bed {
    fn file(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("config.yaml")).unwrap()
    }
    fn listen(&self) -> tw_config::GatewayListen {
        tw_config::try_parse(&self.file()).unwrap().listen.gateway
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
        gateway: gw.clone(),
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
        gw,
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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn yaml(port: u16) -> String {
    format!(
        "version: 1
clients:
  - name: default
    key: tw-aaaa
listen:
  gateway:
    port: {port}
"
    )
}

#[tokio::test]
async fn saving_writes_what_was_chosen_and_leaves_no_trace_of_defaults() {
    let b = bed(&yaml(free_port()));
    let port = free_port();
    let (st, v) = call(
        &b.app,
        "PUT",
        "/listen",
        serde_json::json!({ "bind": "all", "port": port, "allow_from": [" 192.168.1.0/24 ", ""] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["version"].is_string());
    let l = b.listen();
    assert_eq!(l.bind, tw_config::Bind::All);
    assert_eq!(l.port, port);
    assert_eq!(l.allow_from, ["192.168.1.0/24"], "空的和两头的空格不该留下");

    // 退回出厂的样子：这一段整个消失，而不是留下三行默认值
    let (st, v) = call(
        &b.app,
        "PUT",
        "/listen",
        serde_json::json!({ "bind": "loopback", "port": tw_config::DEFAULT_GATEWAY_PORT }),
    )
    .await;
    // 8788 可能正被开发者自己的网关占着：那时拒绝是对的，这一半就不测了
    if st == StatusCode::CONFLICT {
        assert_eq!(v["code"], "gw.listen.port_taken", "{v}");
        return;
    }
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(!b.file().contains("listen"), "{}", b.file());
}

#[tokio::test]
async fn a_port_in_use_is_refused_and_nothing_is_written() {
    // **这个接口存在的理由。**写进去之后才发现绑不上，网关守着旧地址而配置
    // 文件说着新地址 —— 两边从那一刻起各说各的
    let start = free_port();
    let b = bed(&yaml(start));
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = squatter.local_addr().unwrap().port();
    let before = b.file();

    let (st, v) = call(
        &b.app,
        "PUT",
        "/listen",
        serde_json::json!({ "bind": "loopback", "port": taken }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], "gw.listen.port_taken");
    assert!(
        v["text"].as_str().unwrap().contains(&taken.to_string()),
        "{v}"
    );
    assert_eq!(b.file(), before, "绑不上的配置不该落盘");
}

#[tokio::test]
async fn an_interface_that_is_not_there_is_refused_by_name() {
    let b = bed(&yaml(free_port()));
    let before = b.file();
    let (st, v) = call(
        &b.app,
        "PUT",
        "/listen",
        serde_json::json!({ "bind": "en97", "port": free_port() }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], "control.listen.unresolved");
    assert!(v["text"].as_str().unwrap().contains("en97"), "{v}");
    assert_eq!(b.file(), before);

    // 写法本身不对的，按配置文件的那套规则说
    let (st, v) = call(
        &b.app,
        "PUT",
        "/listen",
        serde_json::json!({ "bind": "lan", "port": free_port() }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["code"], "control.listen.bad_bind");

    let (st, v) = call(
        &b.app,
        "PUT",
        "/listen",
        serde_json::json!({ "bind": "loopback", "port": 0 }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["code"], "control.listen.bad_port");
}

#[tokio::test]
async fn the_status_follows_the_listener_after_a_save() {
    // 界面左下角的地址以前是启动时记的一次：改了端口，网关已经在新端口上
    // 服务，状态里还写着旧的
    let p1 = free_port();
    let b = bed(&yaml(p1));
    let gw = b.gw.clone();
    let want = tw_config::try_parse(&b.file())
        .unwrap()
        .listen
        .gateway
        .addrs()
        .unwrap();
    tokio::spawn(async move { tw_gateway::serve_at(gw, want, true).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(80)).await;

    let (_, s) = call(&b.app, "GET", "/status", serde_json::Value::Null).await;
    assert_eq!(s["gateway_addr"], format!("127.0.0.1:{p1}"));

    let p2 = free_port();
    let (st, v) = call(
        &b.app,
        "PUT",
        "/listen",
        serde_json::json!({ "bind": "loopback", "port": p2 }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let (_, s) = call(&b.app, "GET", "/status", serde_json::Value::Null).await;
    assert_eq!(s["gateway_addr"], format!("127.0.0.1:{p2}"));
    assert!(s.get("listen_error").is_none(), "{s}");
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", p2))
            .await
            .is_ok(),
        "新端口上该有东西在听"
    );
}

#[tokio::test]
async fn a_stale_version_is_refused_like_any_other_edit() {
    let b = bed(&yaml(free_port()));
    let (st, _) = call(
        &b.app,
        "PUT",
        "/listen",
        serde_json::json!({ "bind": "loopback", "port": free_port(), "base_version": "nope" }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
}
