//! 升级不断线，端到端：起真的 twcore 进程，在压测中交接。
//!
//! - 交接前开始的长流在旧 core 上完整结束；交接期间持续发的请求**没有一个被拒绝或重置**
//!   （方案 V1）；旧 core 排空后退出，锁归新 core。
//! - 接替者在提交之前走掉：旧 core 回到待交接、控制面接着接、网关照常（F4）。
//! - 版本号不同：旧 core 拒绝，接替者请它退回，它排空后退出（H2）。
//! - 等不到接替者：到点排空退出，不留孤儿进程（H7）。
//!
//! 这几条要跑真的交接连接和网关钩子，整合之前标着 ignore。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::post;

const BIN: &str = env!("CARGO_BIN_EXE_twcore");

/// 数据目录。**路径要短**：控制面的 unix socket 路径有 104 字节的上限，macOS 的临时目录
/// 本身就占了一大半
fn short_dir() -> tempfile::TempDir {
    #[cfg(unix)]
    {
        tempfile::Builder::new()
            .prefix("twho")
            .tempdir_in("/tmp")
            .unwrap()
    }
    #[cfg(not(unix))]
    {
        tempfile::tempdir().unwrap()
    }
}

fn spare_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// 假的 Anthropic 上游：请求体里有 `slow` 就慢慢流（每 200 毫秒一段，`chunks` 段），
/// 否则立刻答一条。
async fn upstream(chunks: usize) -> SocketAddr {
    let app = Router::new().route(
        "/v1/messages",
        post(move |body: String| async move {
            if !body.contains("slow") {
                return axum::response::Response::builder()
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"id":"msg_1","type":"message","role":"assistant","model":"m","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#,
                    ))
                    .unwrap();
            }
            let stream = async_stream(chunks);
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from_stream(stream))
                .unwrap()
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}

fn async_stream(
    chunks: usize,
) -> impl futures::Stream<Item = Result<String, std::convert::Infallible>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::convert::Infallible>>(4);
    tokio::spawn(async move {
        let ev = |name: &str, data: &str| format!("event: {name}\ndata: {data}\n\n");
        let _ = tx
            .send(Ok(ev(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"m","content":[],"stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}}"#,
            )))
            .await;
        let _ = tx
            .send(Ok(ev(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            )))
            .await;
        for i in 0..chunks {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let d = format!(
                r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"part{i} "}}}}"#
            );
            if tx.send(Ok(ev("content_block_delta", &d))).await.is_err() {
                return;
            }
        }
        let _ = tx
            .send(Ok(ev(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            )))
            .await;
        let _ = tx
            .send(Ok(ev(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
            )))
            .await;
        let _ = tx
            .send(Ok(ev("message_stop", r#"{"type":"message_stop"}"#)))
            .await;
    });
    tokio_stream_from(rx)
}

/// mpsc 接收端当成 Stream（不为这一处引 tokio-stream）
fn tokio_stream_from<T: Send + 'static>(
    rx: tokio::sync::mpsc::Receiver<T>,
) -> impl futures::Stream<Item = T> {
    futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|x| (x, rx)) })
}

struct Bed {
    dir: tempfile::TempDir,
    gateway: SocketAddr,
}

impl Bed {
    async fn new(chunks: usize) -> Bed {
        let up = upstream(chunks).await;
        let dir = short_dir();
        let port = spare_port();
        let cfg = format!(
            "version: 1\nlisten:\n  gateway:\n    port: {port}\nclients:\n  - name: t\n    key: tw-test\nproviders:\n  - name: up\n    base_url: http://{up}\n    key: k\n    protocol: anthropic\n"
        );
        std::fs::write(dir.path().join("config.yaml"), cfg).unwrap();
        Bed {
            dir,
            gateway: SocketAddr::from(([127, 0, 0, 1], port)),
        }
    }

    fn config(&self) -> PathBuf {
        self.dir.path().join("config.yaml")
    }

    fn core(&self, name: &str) -> Child {
        let log = std::fs::File::create(self.dir.path().join(format!("{name}.log"))).unwrap();
        self.command().stderr(log).spawn().unwrap()
    }

    /// 和桌面端一样，stderr 接一根管道（桌面端退出时这根管道的读端就没了）
    fn core_on_a_pipe(&self) -> Child {
        self.command().stderr(Stdio::piped()).spawn().unwrap()
    }

    fn command(&self) -> Command {
        let mut c = Command::new(BIN);
        c.arg("--config")
            .arg(self.config())
            .arg("serve")
            .env("TWCORE_LOG", "info")
            .env_remove("HTTP_PROXY")
            .env_remove("HTTPS_PROXY")
            .env_remove("http_proxy")
            .env_remove("https_proxy")
            .stdout(Stdio::null());
        c
    }

    /// `twcore call`：控制面上发一个请求，拿回 JSON
    fn call(&self, method: &str, path: &str, data: Option<&str>) -> Option<serde_json::Value> {
        let mut c = Command::new(BIN);
        c.arg("--config")
            .arg(self.config())
            .args(["call", path, "-X", method]);
        if let Some(d) = data {
            c.args(["-d", d]);
        }
        let out = c.output().ok()?;
        serde_json::from_slice(&out.stdout).ok()
    }

    fn status(&self) -> Option<serde_json::Value> {
        self.call("GET", "/status", None)
    }

    fn lock_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.dir.path().join("twcore.lock"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    fn logs(&self) -> String {
        let mut s = String::new();
        for n in ["a", "b", "twcore.handover"] {
            if let Ok(t) = std::fs::read_to_string(self.dir.path().join(format!("{n}.log"))) {
                s.push_str(&format!("── {n} ──\n{t}\n"));
            }
        }
        s
    }
}

/// 等到 `f` 成立（最多 `within`）
async fn until(within: Duration, mut f: impl FnMut() -> bool) -> bool {
    let t = Instant::now();
    while t.elapsed() < within {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

fn exited(c: &mut Child) -> bool {
    matches!(c.try_wait(), Ok(Some(_)))
}

/// 收尸（测试失败时也别把进程留下）
struct Reap(Vec<Child>);

impl Drop for Reap {
    fn drop(&mut self) {
        for c in &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// 每个请求一条新连接：**连接被拒绝、被重置都会算出来**（复用连接的话看不出监听那一边）
fn fresh_client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .no_proxy()
        .build()
        .unwrap()
}

async fn quick(c: &reqwest::Client, gw: SocketAddr) -> Result<(), String> {
    let r = c
        .post(format!("http://{gw}/v1/messages"))
        .header("x-api-key", "tw-test")
        .header("content-type", "application/json")
        .body(r#"{"model":"m","max_tokens":5,"messages":[{"role":"user","content":"hi"}]}"#)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("{e:?}"))?;
    let status = r.status();
    r.bytes().await.map_err(|e| format!("{e:?}"))?;
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("{status}"))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]

async fn a_handover_under_load_refuses_no_connection_and_finishes_the_long_stream() {
    let bed = Bed::new(25).await;
    let mut reap = Reap(vec![bed.core_on_a_pipe()]);
    assert!(
        until(Duration::from_secs(20), || bed.status().is_some()).await,
        "a 没起来\n{}",
        bed.logs()
    );
    let a_pid = bed.status().unwrap()["pid"].as_u64().unwrap() as u32;
    assert!(
        until(Duration::from_secs(5), || std::net::TcpStream::connect(
            bed.gateway
        )
        .is_ok())
        .await
    );

    // 一条长流，交接前开始（约 5 秒）
    let gw = bed.gateway;
    let long = tokio::spawn(async move {
        let r = fresh_client()
            .post(format!("http://{gw}/v1/messages"))
            .header("x-api-key", "tw-test")
            .header("content-type", "application/json")
            .body(r#"{"model":"m","max_tokens":5,"stream":true,"messages":[{"role":"user","content":"slow"}]}"#)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| format!("{e:?}"))?;
        let body = r.text().await.map_err(|e| format!("{e:?}"))?;
        Ok::<String, String>(body)
    });

    // 压测：几路并发，每个请求一条新连接
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (ok, bad) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(std::sync::Mutex::new(Vec::new())),
    );
    let mut load = Vec::new();
    for _ in 0..4 {
        let (stop, ok, bad) = (stop.clone(), ok.clone(), bad.clone());
        load.push(tokio::spawn(async move {
            let c = fresh_client();
            while !stop.load(Ordering::Relaxed) {
                match quick(&c, gw).await {
                    Ok(()) => {
                        ok.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => bad.lock().unwrap().push(e),
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }));
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // arm，然后起接替者（桌面端的顺序：arm → 自己重启 → 新的桌面端起新 core）
    let armed = bed.call(
        "POST",
        "/handover/arm",
        Some(r#"{"wait_secs":30,"relaunch":null}"#),
    );
    assert_eq!(
        armed.as_ref().and_then(|v| v["version"].as_u64()),
        Some(1),
        "{armed:?}\n{}",
        bed.logs()
    );
    // 桌面端退出：a 的 stderr 那根管道没人读了。a 之后再写日志不能挂（日志已经改写进数据目录）
    drop(reap.0[0].stderr.take());
    reap.0.push(bed.core("b"));
    let b_pid = reap.0[1].id();
    assert!(
        until(Duration::from_secs(20), || bed.lock_pid() == Some(b_pid)).await,
        "锁没交给 b\n{}",
        bed.logs()
    );
    // 控制面现在是 b 在答
    let st = bed.status().unwrap();
    assert_eq!(st["pid"].as_u64(), Some(b_pid as u64));
    assert!(
        matches!(st["handover"].as_str(), Some("draining" | "idle")),
        "{st}"
    );

    // 长流在 a 上完整结束
    let body = long.await.unwrap().expect("长流断了");
    assert!(body.contains("part24"), "长流没收全：{body}");
    assert!(body.contains("message_stop"), "长流没收尾：{body}");

    // a 排空后退出
    assert!(
        until(Duration::from_secs(20), || exited(&mut reap.0[0])).await,
        "a 没退\n{}",
        bed.logs()
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    stop.store(true, Ordering::Relaxed);
    for l in load {
        l.await.unwrap();
    }
    let bad = bad.lock().unwrap().clone();
    assert!(
        bad.is_empty(),
        "交接期间有请求失败：{bad:?}\n{}",
        bed.logs()
    );
    assert!(ok.load(Ordering::Relaxed) > 20, "压测没压起来");
    assert_eq!(bed.lock_pid(), Some(b_pid), "a 退出时把锁删了");
    // a 是自己排空完退的，不是写坏了管道挂掉的；它 arm 之后的日志在数据目录里
    let status = reap.0[0].try_wait().unwrap().unwrap();
    assert!(
        status.success(),
        "a 不是正常退出的：{status:?}\n{}",
        bed.logs()
    );
    let after = std::fs::read_to_string(bed.dir.path().join("twcore.handover.log")).unwrap();
    assert!(after.contains("drained"), "{after}");
    assert_ne!(a_pid, b_pid);
    // 平常了
    assert!(
        until(Duration::from_secs(10), || {
            bed.status()
                .is_some_and(|s| s["handover"] == "idle" && s["draining"] == 0)
        })
        .await,
        "{:?}",
        bed.status()
    );
}

/// 自己扮接替者：连交接端点，打招呼，收下控制面和快照。
async fn fake_successor(dir: &Path, version: u32) -> tw_handover::Link {
    let mut link = tw_handover::connect(dir)
        .await
        .unwrap()
        .expect("arm 之后交接端点该开着");
    link.send(&tw_handover::Message::Hello {
        version,
        core_version: "test".into(),
        pid: std::process::id(),
    })
    .await
    .unwrap();
    link
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]

async fn a_successor_that_leaves_before_committing_leaves_the_old_core_serving() {
    let bed = Bed::new(1).await;
    let mut reap = Reap(vec![bed.core("a")]);
    assert!(
        until(Duration::from_secs(20), || bed.status().is_some()).await,
        "{}",
        bed.logs()
    );
    let armed = bed.call(
        "POST",
        "/handover/arm",
        Some(r#"{"wait_secs":30,"relaunch":null}"#),
    );
    assert!(armed.is_some_and(|v| v["version"] == 1));

    // 要监听、收下，然后不提交就走（F4）：块结束时连接和套接字都丢掉
    {
        let mut link = fake_successor(bed.dir.path(), tw_handover::VERSION).await;
        let (m, socks) = link.recv().await.unwrap();
        assert!(matches!(m, tw_handover::Message::Control { .. }), "{m:?}");
        assert_eq!(socks.len(), 1);
        let (m, _) = link.recv().await.unwrap();
        assert!(matches!(m, tw_handover::Message::Snapshot { .. }), "{m:?}");
        link.send(&tw_handover::Message::WantListeners)
            .await
            .unwrap();
        let (m, socks) = link.recv().await.unwrap();
        assert!(matches!(m, tw_handover::Message::Listeners { .. }), "{m:?}");
        assert!(!socks.is_empty());
    }

    // 旧的回到待交接：控制面又是它在答，网关照常
    assert!(
        until(Duration::from_secs(10), || bed
            .status()
            .is_some_and(|s| s["handover"] == "armed"))
        .await,
        "{:?}\n{}",
        bed.status(),
        bed.logs()
    );
    assert!(quick(&fresh_client(), bed.gateway).await.is_ok());
    assert!(!exited(&mut reap.0[0]));
    // 锁还是它的
    assert_eq!(bed.lock_pid(), Some(reap.0[0].id()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]

async fn a_different_handover_version_falls_back_to_drain_and_exit() {
    let bed = Bed::new(1).await;
    let mut reap = Reap(vec![bed.core("a")]);
    assert!(
        until(Duration::from_secs(20), || bed.status().is_some()).await,
        "{}",
        bed.logs()
    );
    assert!(
        bed.call(
            "POST",
            "/handover/arm",
            Some(r#"{"wait_secs":30,"relaunch":null}"#)
        )
        .is_some()
    );
    let mut link = fake_successor(bed.dir.path(), tw_handover::VERSION + 1).await;
    let (m, socks) = link.recv().await.unwrap();
    assert!(matches!(m, tw_handover::Message::Refused { .. }), "{m:?}");
    assert!(socks.is_empty());
    link.send(&tw_handover::Message::Fallback).await.unwrap();
    // H2：没有在跑的请求，马上退
    assert!(
        until(Duration::from_secs(10), || exited(&mut reap.0[0])).await,
        "{}",
        bed.logs()
    );
    // 按现行方式退出：锁文件删了，下一个照常启动
    assert_eq!(bed.lock_pid(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]

async fn without_a_successor_the_armed_core_drains_and_exits() {
    let bed = Bed::new(1).await;
    let mut reap = Reap(vec![bed.core("a")]);
    assert!(
        until(Duration::from_secs(20), || bed.status().is_some()).await,
        "{}",
        bed.logs()
    );
    let armed = bed.call(
        "POST",
        "/handover/arm",
        Some(r#"{"wait_secs":1,"relaunch":null}"#),
    );
    assert!(armed.is_some_and(|v| v["version"] == 1));
    assert_eq!(bed.status().unwrap()["handover"], "armed");
    // H7：等一个周期、没有可重开的，排空退出
    assert!(
        until(Duration::from_secs(10), || exited(&mut reap.0[0])).await,
        "{}",
        bed.logs()
    );
    assert_eq!(bed.lock_pid(), None);
}
