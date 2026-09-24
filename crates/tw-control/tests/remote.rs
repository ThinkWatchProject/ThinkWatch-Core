//! 远程控制端口：真的 TCP、真的握手。
//!
//! 放行名单、节流、并发之外，要紧的是两件事：**远程进来的做不了那几件事**
//! （关 core、取诊断包、改 `listen.control`），以及**它跟着配置走** —— 开、关、
//! 换端口都不用重启，绑不上也不连累本机的通道。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tw_api::control::{Address, ControlKey};
use tw_control::{ConfigManager, ControlState};
use tw_link::LinkError;

const KEY: &str = "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00";

fn yaml(port: u16, enabled: bool, allow: &str) -> String {
    format!(
        "version: 1\nlisten:\n  control:\n    key: {KEY}\n    remote:\n      enabled: {enabled}\n      bind: loopback\n      port: {port}\n      allow_from: {allow}\nclients:\n  - name: default\n    key: tw-aaaa\n"
    )
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Bed {
    dir: tempfile::TempDir,
    state: ControlState,
    local: Address,
}

impl Bed {
    fn path(&self) -> std::path::PathBuf {
        self.dir.path().join("config.yaml")
    }
    /// 像手改文件一样换一份配置，从文件监听那条路换入
    async fn rewrite(&self, text: &str) {
        std::fs::write(self.path(), text).unwrap();
        self.state.cfg.reload_from_disk().await.unwrap();
    }
    /// 等远程端口的状态变成 `f` 认可的样子
    async fn until(
        &self,
        f: impl Fn(&tw_control::remote::Listening) -> bool,
    ) -> tw_control::remote::Listening {
        for _ in 0..300 {
            let l = self.state.remote.listening();
            if f(&l) {
                return l;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "the remote port never got there: {:?}",
            self.state.remote.listening()
        );
    }
}

async fn bed(text: String) -> Bed {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, &text).unwrap();
    let cfg = tw_config::try_parse(&text).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    let state = ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), bus)),
        gateway: gw,
        store: None,
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    };
    let local = Address::Loopback {
        port_file: d.path().join("control.port"),
    };
    let (s2, at) = (state.clone(), local.clone());
    tokio::spawn(async move { tw_control::serve(s2, &at).await });
    for _ in 0..300 {
        if std::fs::read_to_string(d.path().join("control.port"))
            .ok()
            .and_then(|s| s.trim().parse::<u16>().ok())
            .is_some()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Bed {
        dir: d,
        state,
        local,
    }
}

type Sender = hyper::client::conn::http1::SendRequest<Body>;

async fn open_tcp(
    addr: SocketAddr,
    key: &str,
) -> Result<(Sender, tokio::task::JoinHandle<()>), LinkError> {
    let s = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(LinkError::unreachable)?;
    let (link, _) = tw_link::connect(s, &ControlKey::parse(key).unwrap(), "remote test").await?;
    let (sender, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(link))
        .await
        .unwrap();
    let done = tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok((sender, done))
}

async fn open_local(b: &Bed) -> Sender {
    let Address::Loopback { port_file } = &b.local else {
        unreachable!()
    };
    let port: u16 = std::fs::read_to_string(port_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    open_tcp(SocketAddr::from(([127, 0, 0, 1], port)), KEY)
        .await
        .unwrap()
        .0
}

async fn send(
    s: &mut Sender,
    method: &str,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let r = s
        .send_request(
            Request::builder()
                .method(method)
                .uri(path)
                .header("host", "localhost")
                .header("content-type", "application/json")
                .body(if body.is_null() {
                    Body::empty()
                } else {
                    Body::from(body.to_string())
                })
                .unwrap(),
        )
        .await
        .unwrap();
    let st = r.status();
    let b = axum::body::to_bytes(Body::new(r.into_body()), 4 << 20)
        .await
        .unwrap();
    let v = serde_json::from_slice(&b)
        .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&b).into()));
    (st, v)
}

/// 放行的来源握得上手，状态里说得出开着、听在哪。
#[tokio::test]
async fn an_allowed_source_gets_in_and_the_status_says_where_it_listens() {
    let port = free_port();
    let b = bed(yaml(port, true, "[127.0.0.1]")).await;
    let l = b.until(|l| l.addr.is_some()).await;
    assert_eq!(l.addr.unwrap().port(), port);
    let (mut s, _) = open_tcp(l.addr.unwrap(), KEY).await.unwrap();
    let (st, v) = send(&mut s, "GET", "/status", serde_json::Value::Null).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let rc = &v["remote_control"];
    assert_eq!(rc["enabled"], true, "{v}");
    assert_eq!(rc["addr"], format!("127.0.0.1:{port}"), "{v}");
    assert_eq!(rc["allow_from"], serde_json::json!(["127.0.0.1"]), "{v}");
    // 只听回环：别的机器根本连不上，说实话
    assert_eq!(rc["reachable"], serde_json::json!([]), "{v}");
    assert_eq!(v["api_version"], tw_api::CONTROL_API_VERSION);
}

/// 名单外的来源：accept 之后直接关掉，一个字节都不回 —— 客户端看到的是「被关了」，
/// 不是「钥匙不对」。
#[tokio::test]
async fn a_source_outside_allow_from_is_closed_without_a_word() {
    let port = free_port();
    let b = bed(yaml(port, true, "[10.0.0.0/8]")).await;
    let l = b.until(|l| l.addr.is_some()).await;
    let mut raw = tokio::net::TcpStream::connect(l.addr.unwrap())
        .await
        .unwrap();
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let n = tokio::time::timeout(Duration::from_secs(5), raw.read_to_end(&mut buf))
        .await
        .expect("该立刻关掉")
        .unwrap_or(0);
    assert_eq!(n, 0, "一个字节都不该回：{buf:?}");
    assert!(matches!(
        open_tcp(l.addr.unwrap(), KEY).await,
        Err(LinkError::Closed)
    ));
    // 名单改了立刻生效，不用重绑
    b.rewrite(&yaml(port, true, "[127.0.0.0/8]")).await;
    let mut ok = false;
    for _ in 0..100 {
        if open_tcp(l.addr.unwrap(), KEY).await.is_ok() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ok, "改了名单之后该放行");
}

/// 同一个来源一分钟里握手失败五次，之后连对的钥匙也直接关掉。
#[tokio::test]
async fn five_wrong_keys_from_one_source_bench_it() {
    let port = free_port();
    let b = bed(yaml(port, true, "[127.0.0.1]")).await;
    let addr = b.until(|l| l.addr.is_some()).await.addr.unwrap();
    let wrong = "1".repeat(64);
    for _ in 0..tw_control::remote::MAX_FAILURES {
        assert!(matches!(
            open_tcp(addr, &wrong).await,
            Err(LinkError::WrongKey)
        ));
    }
    // 记账在握手失败之后的那个任务里：给它一点时间
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        matches!(open_tcp(addr, KEY).await, Err(LinkError::Closed)),
        "被晾着的来源该直接关掉"
    );
    // 本机的通道不受影响
    let mut s = open_local(&b).await;
    assert_eq!(
        send(&mut s, "GET", "/status", serde_json::Value::Null)
            .await
            .0,
        StatusCode::OK
    );
}

/// 远程进来的：关不了 core、拿不到诊断包、改不了 `listen.control`；别的照常。
/// 同样的事从本机的通道做得了。
#[tokio::test]
async fn a_remote_connection_cannot_stop_the_core_take_diagnostics_or_move_its_own_door() {
    let port = free_port();
    let b = bed(yaml(port, true, "[127.0.0.1]")).await;
    let addr = b.until(|l| l.addr.is_some()).await.addr.unwrap();
    let (mut r, _) = open_tcp(addr, KEY).await.unwrap();

    let (st, v) = send(&mut r, "POST", "/shutdown", serde_json::Value::Null).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["code"], "control.remote.shutdown_refused", "{v}");

    let (st, v) = send(&mut r, "GET", "/diagnostics", serde_json::Value::Null).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["code"], "control.remote.diagnostics_refused", "{v}");

    // 整份写回，只动了远程端口的放行名单
    let (_, cur) = send(&mut r, "GET", "/config", serde_json::Value::Null).await;
    let text = cur["text"].as_str().unwrap().to_string();
    let moved = text.replace("allow_from: [127.0.0.1]", "allow_from: [0.0.0.0/0]");
    assert_ne!(moved, text);
    let (st, v) = send(
        &mut r,
        "PUT",
        "/config",
        serde_json::json!({ "base_version": cur["version"], "text": moved }),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["code"], "control.remote.control_section_locked", "{v}");
    // 按字段改也一样
    let (st, v) = send(
        &mut r,
        "PATCH",
        "/config",
        serde_json::json!({
            "base_version": cur["version"],
            "ops": [{ "op": "replace", "path": "/listen/control/remote/enabled", "value": false }]
        }),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert!(
        b.state
            .config()
            .listen
            .control
            .remote
            .as_ref()
            .unwrap()
            .enabled
    );

    // 改别的照常
    let edited = text.replace("tw-aaaa", "tw-bbbb");
    let (st, v) = send(
        &mut r,
        "PUT",
        "/config",
        serde_json::json!({ "base_version": cur["version"], "text": edited }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    // 本机的通道：诊断包拿得到，名单改得了
    let mut l = open_local(&b).await;
    let (st, _) = send(&mut l, "GET", "/diagnostics", serde_json::Value::Null).await;
    assert_eq!(st, StatusCode::OK);
    let (_, cur) = send(&mut l, "GET", "/config", serde_json::Value::Null).await;
    let text = cur["text"].as_str().unwrap().to_string();
    let (st, v) = send(
        &mut l,
        "PUT",
        "/config",
        serde_json::json!({
            "base_version": cur["version"],
            "text": text.replace("allow_from: [127.0.0.1]", "allow_from: [127.0.0.0/8]")
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // 远程那一下不该扳动开关：开关扳过的话，这里立刻就等到了
    assert!(
        tokio::time::timeout(Duration::from_millis(300), b.state.shutdown.asked())
            .await
            .is_err(),
        "远程那一下扳动了退出的开关"
    );
}

/// 跟着配置走：关掉就不听、从它进来的连接断开；换端口就换过去；绑不上就守着
/// 旧的并说为什么，本机的通道照常。
#[tokio::test]
async fn it_follows_the_configuration_live() {
    let p1 = free_port();
    let b = bed(yaml(p1, true, "[127.0.0.1]")).await;
    let a1 = b.until(|l| l.addr.is_some()).await.addr.unwrap();
    let (mut s, conn) = open_tcp(a1, KEY).await.unwrap();
    assert_eq!(
        send(&mut s, "GET", "/status", serde_json::Value::Null)
            .await
            .0,
        StatusCode::OK
    );

    // 关掉：从它进来的那条断开，端口不再有人听
    b.rewrite(&yaml(p1, false, "[127.0.0.1]")).await;
    b.until(|l| l.addr.is_none()).await;
    tokio::time::timeout(Duration::from_secs(5), conn)
        .await
        .expect("关掉远程端口，从它进来的连接该断开")
        .unwrap();
    assert!(matches!(
        open_tcp(a1, KEY).await,
        Err(LinkError::Unreachable(_))
    ));

    // 换个端口打开
    let p2 = free_port();
    b.rewrite(&yaml(p2, true, "[127.0.0.1]")).await;
    let a2 = b
        .until(|l| l.addr.is_some_and(|a| a.port() == p2))
        .await
        .addr
        .unwrap();
    assert!(open_tcp(a2, KEY).await.is_ok());

    // 换到一个被占着的端口：守着旧的，说为什么
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p3 = taken.local_addr().unwrap().port();
    b.rewrite(&yaml(p3, true, "[127.0.0.1]")).await;
    let l = b.until(|l| l.error.is_some()).await;
    assert_eq!(l.addr.unwrap().port(), p2, "旧的该还在听");
    assert_eq!(l.error.unwrap().code, "gw.listen.port_taken");
    assert!(open_tcp(a2, KEY).await.is_ok());
    let mut local = open_local(&b).await;
    let (st, v) = send(&mut local, "GET", "/status", serde_json::Value::Null).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        v["remote_control"]["error"]["code"], "gw.listen.port_taken",
        "{v}"
    );
    drop(taken);
}

/// 起来时就绑不上：本机的通道和状态照常，原因在状态里。
#[tokio::test]
async fn a_port_that_cannot_be_bound_at_start_does_not_take_the_local_channel_down() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let b = bed(yaml(port, true, "[127.0.0.1]")).await;
    let l = b.until(|l| l.error.is_some()).await;
    assert!(l.addr.is_none());
    let mut s = open_local(&b).await;
    let (st, v) = send(&mut s, "GET", "/status", serde_json::Value::Null).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["remote_control"]["enabled"], true);
    assert_eq!(v["remote_control"]["addr"], serde_json::Value::Null);
    drop(taken);
}
