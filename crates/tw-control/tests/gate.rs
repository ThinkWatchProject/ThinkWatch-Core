//! 控制面的门：真的 socket、真的握手、真的 HTTP。
//!
//! 另外几个测试文件直接拿 `router()` 跑处理函数，门不在那里。这里起的是
//! `tw_control::serve` —— 桌面端连的就是它 —— 断言三件事：握上手之后 HTTP
//! 和事件流原样能用；钥匙不对的进不来；钥匙换了，旧钥匙进来的连接被断开。
//!
//! 还有一件和门同样要紧的：**钥匙不从控制面出去，也不能经控制面改掉。**

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tokio::io::{AsyncRead, AsyncWrite};
use tower::ServiceExt;
use tw_api::control::{Address, ControlKey, KEY_MASK};
use tw_config::history::Origin;
use tw_control::{ConfigManager, ControlState};
use tw_link::LinkError;

const KEY: &str = "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00";

fn yaml(key: &str) -> String {
    format!(
        "version: 1\n# 注释留着\nlisten:\n  control:\n    key: {key}\nclients:\n  - name: default\n    key: tw-aaaa\n"
    )
}

struct Bed {
    dir: tempfile::TempDir,
    state: ControlState,
}

impl Bed {
    fn file(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("config.yaml")).unwrap()
    }
    fn app(&self) -> axum::Router {
        tw_control::router(self.state.clone())
    }
}

fn bed() -> Bed {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, yaml(KEY)).unwrap();
    let cfg = tw_config::try_parse(&yaml(KEY)).unwrap();
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
    Bed { dir: d, state }
}

fn key(hex: &str) -> ControlKey {
    ControlKey::parse(hex).unwrap()
}

/// 起控制面，等它听起来。
async fn listen(b: &Bed, at: Address) {
    let state = b.state.clone();
    let at2 = at.clone();
    tokio::spawn(async move { tw_control::serve(state, &at2).await });
    for _ in 0..200 {
        let up = match &at {
            Address::Socket(p) => p.exists(),
            // 文件出现和号码写完之间有一瞬：读得出号码才算
            Address::Loopback { port_file } => std::fs::read_to_string(port_file)
                .ok()
                .and_then(|s| s.trim().parse::<u16>().ok())
                .is_some(),
        };
        if up {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the control plane never started listening");
}

type Stream = Box<dyn AsyncStream>;
trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}

async fn dial(at: &Address) -> Stream {
    match at {
        #[cfg(unix)]
        Address::Socket(p) => Box::new(tokio::net::UnixStream::connect(p).await.unwrap()),
        #[cfg(not(unix))]
        Address::Socket(_) => unreachable!(),
        Address::Loopback { port_file } => {
            let port: u16 = std::fs::read_to_string(port_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            Box::new(
                tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .unwrap(),
            )
        }
    }
}

type Sender = hyper::client::conn::http1::SendRequest<Body>;

/// 握手，然后在加密的流上开一条 HTTP/1.1。
async fn open(
    at: &Address,
    k: &ControlKey,
) -> Result<(Sender, tokio::task::JoinHandle<()>), LinkError> {
    let (link, hello) = tw_link::connect(dial(at).await, k, "gate test").await?;
    assert!(hello.accept);
    assert_eq!(hello.core, env!("CARGO_PKG_VERSION"));
    let (sender, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(link))
        .await
        .unwrap();
    let done = tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok((sender, done))
}

async fn get(s: &mut Sender, path: &str) -> (StatusCode, String) {
    let r = s
        .send_request(
            Request::builder()
                .uri(path)
                .header("host", "localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let st = r.status();
    let b = axum::body::to_bytes(Body::new(r.into_body()), 1 << 20)
        .await
        .unwrap();
    (st, String::from_utf8_lossy(&b).to_string())
}

/// 同一件事在两种传输上各走一遍：它们的差别只在怎么拿到流。
async fn http_and_events_run_over_the_handshake(at: Address, b: &Bed) {
    listen(b, at.clone()).await;
    let (mut s, _) = open(&at, &key(KEY)).await.unwrap();
    let (st, body) = get(&mut s, "/status").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains(&format!("\"api_version\":{}", tw_api::CONTROL_API_VERSION)),
        "{body}"
    );
    // 同一条连接上接着发：keep-alive 照常
    let (st, body) = get(&mut s, "/config").await;
    assert_eq!(st, StatusCode::OK);
    assert!(!body.contains(KEY), "钥匙从 GET /config 出去了：{body}");
    assert!(body.contains(KEY_MASK), "{body}");

    // 事件流：另开一条，发一件事，读得到
    let (mut ev, _) = open(&at, &key(KEY)).await.unwrap();
    let r = ev
        .send_request(
            Request::builder()
                .uri("/events")
                .header("host", "localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let mut body = Body::new(r.into_body()).into_data_stream();
    let bus = b.state.bus().clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        bus.emit(tw_api::Event::HealthChanged {
            id: 7,
            provider: "p-through-the-gate".into(),
            state: tw_api::BreakerState::Open,
            at_ms: 0,
        });
    });
    let seen = tokio::time::timeout(Duration::from_secs(5), async {
        use futures::StreamExt;
        let mut all = String::new();
        while let Some(d) = body.next().await {
            all.push_str(&String::from_utf8_lossy(&d.unwrap()));
            if all.contains("p-through-the-gate") {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(seen, "事件流上没收到那件事");

    // 钥匙不对：握手就被拒，说的是钥匙不对
    let wrong = key(&"1".repeat(64));
    assert!(matches!(open(&at, &wrong).await, Err(LinkError::WrongKey)));
}

#[cfg(unix)]
#[tokio::test]
async fn over_the_unix_socket() {
    let b = bed();
    let at = Address::Socket(b.dir.path().join("twcore.sock"));
    http_and_events_run_over_the_handshake(at, &b).await;
}

#[tokio::test]
async fn over_the_loopback_port() {
    let b = bed();
    let at = Address::Loopback {
        port_file: b.dir.path().join("control.port"),
    };
    http_and_events_run_over_the_handshake(at, &b).await;
}

/// 换钥匙：旧钥匙进来的连接被断开，旧钥匙再也进不来，新钥匙进得来。
#[tokio::test]
async fn a_new_key_closes_the_connections_made_with_the_old_one() {
    let b = bed();
    let at = Address::Loopback {
        port_file: b.dir.path().join("control.port"),
    };
    listen(&b, at.clone()).await;
    let (mut s, conn) = open(&at, &key(KEY)).await.unwrap();
    assert_eq!(get(&mut s, "/status").await.0, StatusCode::OK);

    // 和 `twcore control-key --rotate` 一样：改文件，core 从文件监听那条路重载
    let path = b.dir.path().join("config.yaml");
    let new = tw_config::control_key::rotate_file(&path).unwrap();
    b.state.cfg.reload_from_disk().await.unwrap();

    tokio::time::timeout(Duration::from_secs(5), conn)
        .await
        .expect("用旧钥匙进来的连接该被断开")
        .unwrap();
    assert!(matches!(
        open(&at, &key(KEY)).await,
        Err(LinkError::WrongKey)
    ));
    let (mut s, _) = open(&at, &new).await.unwrap();
    assert_eq!(get(&mut s, "/status").await.0, StatusCode::OK);
}

/// 配置里的钥匙写坏了（外部改动）：配置不收，旧钥匙继续管用。
#[tokio::test]
async fn a_broken_key_in_the_file_is_rejected_and_the_old_one_keeps_working() {
    let b = bed();
    let path = b.dir.path().join("config.yaml");
    std::fs::write(&path, yaml("tooshort")).unwrap();
    assert!(b.state.cfg.reload_from_disk().await.is_err());
    let rejected = b.state.cfg.rejected().expect("该记下这次被拒");
    assert_eq!(rejected.message.code, "config.control_key_invalid");
    assert_eq!(
        b.state.config().listen.control.key(),
        Some(key(KEY)),
        "旧钥匙该还在管用"
    );
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
    (
        st,
        serde_json::from_slice(&b).unwrap_or(serde_json::Value::Null),
    )
}

/// 编辑器整份写回：打码原样带回来就是钥匙不动，别处的改动照写。
#[tokio::test]
async fn writing_back_the_masked_text_keeps_the_key() {
    let b = bed();
    let app = b.app();
    let (_, got) = call(&app, "GET", "/config", serde_json::Value::Null).await;
    let text = got["text"].as_str().unwrap().to_string();
    assert!(text.contains(KEY_MASK) && !text.contains(KEY), "{text}");
    let edited = text.replace("# 注释留着", "# 注释留着，改了一个字");
    let (st, body) = call(
        &app,
        "PUT",
        "/config",
        serde_json::json!({ "base_version": got["version"], "text": edited }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let file = b.file();
    assert!(file.contains(&format!("key: {KEY}")), "{file}");
    assert!(file.contains("改了一个字"), "{file}");
    assert!(!file.contains(KEY_MASK), "{file}");
}

/// 经控制面换掉、删掉钥匙，一律拒绝，文件不动。
#[tokio::test]
async fn the_key_cannot_be_changed_or_removed_through_the_control_plane() {
    let b = bed();
    let app = b.app();
    let before = b.file();
    let version = tw_config::store::version_of(&before);

    let other = "2".repeat(64);
    for text in [
        yaml(&other),
        // 删掉整节
        "version: 1\nclients:\n  - name: default\n    key: tw-aaaa\n".to_string(),
    ] {
        let (st, body) = call(
            &app,
            "PUT",
            "/config",
            serde_json::json!({ "base_version": version, "text": text }),
        )
        .await;
        // 删掉的那份先过不了校验（缺钥匙），换掉的那份撞上这条规矩；都不写
        assert!(
            st == StatusCode::FORBIDDEN || st == StatusCode::BAD_REQUEST,
            "{st} {body}"
        );
        assert_eq!(b.file(), before);
    }
    let (st, body) = call(
        &app,
        "PUT",
        "/config",
        serde_json::json!({ "base_version": version, "text": yaml(&other) }),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "control.control_key_locked", "{body}");

    let (st, body) = call(
        &app,
        "PATCH",
        "/config",
        serde_json::json!({
            "base_version": version,
            "ops": [{ "op": "replace", "path": "/listen/control/key", "value": other }]
        }),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(b.file(), before);
}

/// 历史里没有钥匙；回滚回去，钥匙是现在这一把。
#[tokio::test]
async fn a_rollback_through_the_control_plane_keeps_the_key() {
    let b = bed();
    let app = b.app();
    let path = b.dir.path().join("config.yaml");
    let v0 = tw_config::store::version_of(&b.file());
    b.state
        .cfg
        .write(&yaml(KEY).replace("tw-aaaa", "tw-bbbb"), None, Origin::Ui)
        .await
        .unwrap();
    for v in tw_config::history::list(&path).unwrap() {
        let t = tw_config::history::read(&v).unwrap();
        assert!(!t.contains(KEY), "历史里有钥匙：{t}");
    }
    let (st, body) = call(
        &app,
        "POST",
        "/config/rollback",
        serde_json::json!({ "version": v0 }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(b.file(), yaml(KEY));
}
