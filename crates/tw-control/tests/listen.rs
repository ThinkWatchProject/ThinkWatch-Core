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
        shutdown: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw.clone(),
        store: None,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
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
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
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
        serde_json::json!({ "bind": "loopback", "port": tw_config::DEFAULT_GATEWAY_PORT, "allow_from": tw_config::default_allow_from() }),
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
        serde_json::json!({ "bind": "loopback", "port": taken, "allow_from": [] }),
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
        serde_json::json!({ "bind": "en97", "port": free_port(), "allow_from": [] }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], "gw.listen.no_such_nic");
    assert_eq!(v["args"]["name"], "en97");
    // 真有哪些网卡一并说出来，用户不必再去跑一次 ifconfig。
    //
    // **不写死任何一个名字。**原来这里断言的是 `lo0`，而那是这台机器的
    // 名字，不是这条断言要说的事 —— Windows 上回环叫「Loopback
    // Pseudo-Interface 1」，网卡叫「Ethernet 3」。改成对着本机真实的清单
    // 比，反而比原来强：它要求**每一张**都列出来了，不是碰巧有一张。
    let listed = v["args"]["available"].as_str().unwrap();
    let here = tw_config::nics::by_name();
    assert!(!here.is_empty(), "本机一张网卡都没有？");
    for n in &here {
        assert!(listed.contains(&n.name), "{} 不在「{listed}」里", n.name);
    }
    assert_eq!(b.file(), before);

    // 写法本身不对的，按配置文件的那套规则说。**带控制字符的在两个平台上
    // 都不是网卡名**（Windows 的网卡名什么可见字符都可能有，`@` 挡不住它；
    // 首尾的空白又会先被 trim 掉）
    let (st, v) = call(
        &b.app,
        "PUT",
        "/listen",
        serde_json::json!({ "bind": "192.168.1.5\twifi", "port": free_port(), "allow_from": [] }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["code"], "control.listen.bind_invalid");

    let (st, v) = call(
        &b.app,
        "PUT",
        "/listen",
        serde_json::json!({ "bind": "loopback", "port": 0, "allow_from": [] }),
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
        serde_json::json!({ "bind": "loopback", "port": p2, "allow_from": [] }),
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
        serde_json::json!({ "bind": "loopback", "port": free_port(), "base_version": "nope", "allow_from": [] }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
}

#[tokio::test]
async fn the_allow_list_is_what_the_file_says_with_the_default_beside_it() {
    // 没写是默认名单；界面照着它画出几条网段，还要知道「恢复默认」恢复成什么
    let b = bed("version: 1
clients:
  - name: default
    key: tw-aaaa
listen:
  gateway:
    bind: all
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
");
    let (_, ov) = call(&b.app, "GET", "/overview", serde_json::Value::Null).await;
    let default = serde_json::json!(tw_config::default_allow_from());
    assert_eq!(ov["listen"]["bind"], "all");
    assert_eq!(ov["listen"]["exposed"], true);
    assert_eq!(ov["listen"]["allow_from"], default);
    assert_eq!(ov["listen"]["default_allow_from"], default);
}

#[tokio::test]
async fn an_empty_allow_list_is_written_down_and_the_default_one_is_not() {
    // 不写 = 默认名单，所以空的必须写成 `[]`；和默认名单一样的不写，
    // 配置文件不因为存了一次就多出几行
    let b = bed(&yaml(free_port()));
    let port = free_port();
    let save = |allow: serde_json::Value| serde_json::json!({ "bind": "all", "port": port, "allow_from": allow });
    let (st, v) = call(&b.app, "PUT", "/listen", save(serde_json::json!([]))).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(b.listen().allow_from.is_empty(), "{}", b.file());
    assert!(b.file().contains("allow_from: []"), "{}", b.file());

    let (st, v) = call(
        &b.app,
        "PUT",
        "/listen",
        save(serde_json::json!(tw_config::default_allow_from())),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(b.listen().allow_from, tw_config::default_allow_from());
    assert!(!b.file().contains("allow_from"), "{}", b.file());
}

#[tokio::test]
async fn interfaces_come_one_per_name() {
    // 配置里按名字存：同一张网卡列两行，选第二行等于选第一行
    let b = bed(&yaml(free_port()));
    let (st, v) = call(&b.app, "GET", "/interfaces", serde_json::Value::Null).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let names: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect();
    let mut unique = names.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(names.len(), unique.len(), "{names:?}");
}
