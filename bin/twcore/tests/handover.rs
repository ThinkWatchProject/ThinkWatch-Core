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
        Self::with_client(chunks, "").await
    }

    /// 密钥 `t` 带一条按天数请求的上限（看用量从哪儿来、算没算重）
    async fn with_daily_limit(chunks: usize) -> Bed {
        Self::with_client(
            chunks,
            "    limits:\n      - { per: day, requests: 100000 }\n",
        )
        .await
    }

    async fn with_client(chunks: usize, client_extra: &str) -> Bed {
        let up = upstream(chunks).await;
        let dir = short_dir();
        let port = spare_port();
        let cfg = format!(
            "version: 1\nlisten:\n  gateway:\n    port: {port}\nclients:\n  - name: t\n    key: tw-test\n{client_extra}providers:\n  - name: up\n    base_url: http://{up}\n    key: k\n    protocol: anthropic\n"
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
        self.core_with(name, &[])
    }

    /// 多带几个 `serve` 的参数（比如 `--parent`）
    fn core_with(&self, name: &str, args: &[&str]) -> Child {
        // 日志写在 stdout（tracing 的默认），stderr 留给 panic：两个都收
        let log = std::fs::File::create(self.dir.path().join(format!("{name}.log"))).unwrap();
        self.command()
            .args(args)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap()
    }

    fn arm(&self, wait_secs: u64) -> Option<serde_json::Value> {
        self.call(
            "POST",
            "/handover/arm",
            Some(&format!(r#"{{"wait_secs":{wait_secs},"relaunch":null}}"#)),
        )
    }

    /// 此刻答控制面的那个 core 记着的请求号（最近的在前）
    fn history_ids(&self) -> Vec<u64> {
        self.call("GET", "/history?limit=500", None)
            .and_then(|v| {
                v.as_array().map(|rows| {
                    rows.iter()
                        .filter_map(|r| r["id"].as_u64())
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_default()
    }

    /// 密钥 `t` 那条按天上限此刻用了多少
    fn used_today(&self) -> Option<u64> {
        self.call("GET", "/keys", None)?
            .as_array()?
            .iter()
            .find(|k| k["name"] == "t")?["limits"][0]["used"]
            .as_u64()
    }

    fn draining_marked(&self) -> bool {
        self.dir.path().join("twcore.draining").exists()
    }

    /// 和桌面端一样，stdout、stderr 都接管道（桌面端退出时这两根管道的读端就没了）
    fn core_on_a_pipe(&self) -> Child {
        self.command()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
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
            .env_remove("https_proxy");
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
        for n in ["a", "b", "c", "twcore.handover"] {
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
    drop(reap.0[0].stdout.take());
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
        // 第二份快照：旧 core 暂停续期之后导出的那一份
        let (m, _) = link.recv().await.unwrap();
        assert!(matches!(m, tw_handover::Message::Snapshot { .. }), "{m:?}");
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

/// 扮接替者走到拿到监听为止：招呼、控制面、第一份快照、要监听、第二份快照、监听。
async fn take_listeners(dir: &Path) -> (tw_handover::Link, Vec<tw_handover::Socket>) {
    let mut link = fake_successor(dir, tw_handover::VERSION).await;
    let (m, _control) = link.recv().await.unwrap();
    assert!(matches!(m, tw_handover::Message::Control { .. }), "{m:?}");
    let (m, _) = link.recv().await.unwrap();
    assert!(matches!(m, tw_handover::Message::Snapshot { .. }), "{m:?}");
    link.send(&tw_handover::Message::WantListeners)
        .await
        .unwrap();
    let (m, _) = link.recv().await.unwrap();
    assert!(matches!(m, tw_handover::Message::Snapshot { .. }), "{m:?}");
    let (m, socks) = link.recv().await.unwrap();
    assert!(matches!(m, tw_handover::Message::Listeners { .. }), "{m:?}");
    (link, socks)
}

/// 提交要确认（CommitAck）：旧 core 收到 Commit 先回确认，再停止 accept、排空、说 Bye 退出。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_old_core_acknowledges_the_commit_then_drains_and_says_bye() {
    let bed = Bed::new(1).await;
    let mut reap = Reap(vec![bed.core("a")]);
    assert!(
        until(Duration::from_secs(20), || bed.status().is_some()).await,
        "{}",
        bed.logs()
    );
    assert!(bed.arm(30).is_some_and(|v| v["version"] == 1));
    let (mut link, _socks) = take_listeners(bed.dir.path()).await;
    link.send(&tw_handover::Message::Commit).await.unwrap();
    let (m, _) = link.recv().await.unwrap();
    assert_eq!(m, tw_handover::Message::CommitAck, "提交之后头一条该是确认");
    // 排空：没有在跑的请求，马上说 Bye
    let bye = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match link.recv().await {
                Ok((tw_handover::Message::Bye, _)) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    })
    .await;
    assert_eq!(bye.ok(), Some(true), "{}", bed.logs());
    assert!(
        until(Duration::from_secs(10), || exited(&mut reap.0[0])).await,
        "{}",
        bed.logs()
    );
    assert!(reap.0[0].try_wait().unwrap().unwrap().success());
}

/// 旧 core 等提交的时限过了就回到待交接：**迟到的 Commit 不回确认**、连接关掉，它照常服务。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_commit_that_comes_after_the_old_core_gave_up_is_not_acknowledged() {
    let bed = Bed::new(1).await;
    let mut reap = Reap(vec![bed.core("a")]);
    assert!(
        until(Duration::from_secs(20), || bed.status().is_some()).await,
        "{}",
        bed.logs()
    );
    assert!(bed.arm(120).is_some_and(|v| v["version"] == 1));
    let (mut link, socks) = take_listeners(bed.dir.path()).await;
    drop(socks);
    // 旧 core 等 30 秒
    tokio::time::sleep(Duration::from_secs(32)).await;
    let _ = link.send(&tw_handover::Message::Commit).await;
    let late = tokio::time::timeout(Duration::from_secs(5), link.recv()).await;
    assert!(
        !matches!(late, Ok(Ok((tw_handover::Message::CommitAck, _)))),
        "放弃之后又确认了一个迟到的提交"
    );
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
}

/// 接替者发出提交、旧 core 却不确认（它还活着）：**接替者不能当主**，退出、不改锁。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successor_whose_commit_is_not_acknowledged_exits_without_taking_the_lock() {
    let bed = Bed::new(1).await;
    // 这个测试进程扮旧 core：锁写自己的 pid，开交接端点
    std::fs::write(
        bed.dir.path().join("twcore.lock"),
        std::process::id().to_string(),
    )
    .unwrap();
    let endpoint = tw_handover::Endpoint::bind(bed.dir.path()).await.unwrap();
    let mut reap = Reap(vec![bed.core("b")]);

    let mut link = tokio::time::timeout(Duration::from_secs(20), endpoint.accept())
        .await
        .expect("接替者没连上来")
        .unwrap();
    let (m, _) = link.recv().await.unwrap();
    let tw_handover::Message::Hello { pid, .. } = m else {
        panic!("头一条该是 Hello：{m:?}");
    };
    let peer = link.peer_pid().unwrap_or(pid);
    #[cfg(unix)]
    let control = tw_handover::Socket::Unix(
        std::os::unix::net::UnixListener::bind(bed.dir.path().join("ctl.sock")).unwrap(),
    );
    #[cfg(not(unix))]
    let control = tw_handover::Socket::Tcp(std::net::TcpListener::bind("127.0.0.1:0").unwrap());
    let info = tw_handover::ListenerInfo {
        role: tw_handover::Role::Control,
        want: None,
    };
    link.send_with(&tw_handover::Message::Control { info }, &[&control], peer)
        .await
        .unwrap();
    let snapshot = serde_json::json!({ "gateway": {}, "next_id": 1 });
    link.send(&tw_handover::Message::Snapshot {
        data: snapshot.clone(),
    })
    .await
    .unwrap();
    let (m, _) = tokio::time::timeout(Duration::from_secs(20), link.recv())
        .await
        .expect("没要监听")
        .unwrap();
    assert_eq!(m, tw_handover::Message::WantListeners, "{}", bed.logs());
    link.send(&tw_handover::Message::Snapshot { data: snapshot })
        .await
        .unwrap();
    let gw = tw_handover::Socket::Tcp(std::net::TcpListener::bind(bed.gateway).unwrap());
    let infos = vec![tw_handover::ListenerInfo {
        role: tw_handover::Role::Gateway,
        want: Some(bed.gateway),
    }];
    link.send_with(&tw_handover::Message::Listeners { infos }, &[&gw], peer)
        .await
        .unwrap();
    let (m, _) = tokio::time::timeout(Duration::from_secs(25), link.recv())
        .await
        .expect("没提交")
        .unwrap();
    assert_eq!(m, tw_handover::Message::Commit);
    // 不确认，连接也不关：接替者自己等不到就退出
    assert!(
        until(Duration::from_secs(25), || exited(&mut reap.0[0])).await,
        "没确认的接替者还在跑\n{}",
        bed.logs()
    );
    assert!(!reap.0[0].try_wait().unwrap().unwrap().success());
    assert_eq!(bed.lock_pid(), Some(std::process::id()), "没确认就改了锁");
    drop(link);
}

/// 读不懂的第一帧（将来 Hello 改了样子）：**也回 Refused**，不是一声不响地断开。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_greeting_is_answered_with_refused() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let bed = Bed::new(1).await;
    let _reap = Reap(vec![bed.core("a")]);
    assert!(
        until(Duration::from_secs(20), || bed.status().is_some()).await,
        "{}",
        bed.logs()
    );
    assert!(bed.arm(30).is_some_and(|v| v["version"] == 1));
    let mut s = tokio::net::UnixStream::connect(bed.dir.path().join(tw_handover::ENDPOINT_FILE))
        .await
        .unwrap();
    let junk = br#"{"hello from the future":true}"#;
    s.write_all(&(junk.len() as u32).to_be_bytes())
        .await
        .unwrap();
    s.write_all(junk).await.unwrap();
    let mut len = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(10), s.read_exact(&mut len))
        .await
        .expect("读不懂的招呼没有回话")
        .unwrap();
    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
    s.read_exact(&mut body).await.unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("refused"), "{body}");
    drop(s);
    // 之后照旧待交接
    assert!(
        until(Duration::from_secs(10), || bed
            .status()
            .is_some_and(|s| s["handover"] == "armed"))
        .await,
        "{}",
        bed.logs()
    );
}

/// arm 之后桌面端没有重启（更新失败、取消）：到点时原来的父进程还在，**撤销 arm**、恢复
/// 父进程守望，不重开、不退出。arm 时另起会话（unix），日志接着往后写。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_arm_is_called_off_when_the_desktop_app_did_not_restart() {
    let bed = Bed::new(1).await;
    // 扮桌面端：一个活着的进程
    #[cfg(windows)]
    let parent = Command::new("ping")
        .args(["-n", "120", "127.0.0.1"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    #[cfg(not(windows))]
    let parent = Command::new("sleep").arg("120").spawn().unwrap();
    let ppid = parent.id().to_string();
    let mut reap = Reap(vec![bed.core_with("a", &["--parent", &ppid]), parent]);
    assert!(
        until(Duration::from_secs(20), || bed.status().is_some()).await,
        "{}",
        bed.logs()
    );
    for round in 0..2 {
        assert!(
            bed.arm(1).is_some_and(|v| v["version"] == 1),
            "第 {round} 次"
        );
        assert_eq!(bed.status().unwrap()["handover"], "armed");
        assert!(
            until(Duration::from_secs(8), || bed
                .status()
                .is_some_and(|s| s["handover"] == "idle"))
            .await,
            "父进程还在，arm 没撤销\n{}",
            bed.logs()
        );
        assert!(!exited(&mut reap.0[0]), "撤销 arm 时退出了");
        assert!(quick(&fresh_client(), bed.gateway).await.is_ok());
    }
    #[cfg(unix)]
    {
        let a = reap.0[0].id() as i32;
        // SAFETY: getsid 只是问
        assert_eq!(unsafe { libc::getsid(a) }, a, "arm 之后没有另起会话");
    }
    let log = std::fs::read_to_string(bed.dir.path().join("twcore.handover.log")).unwrap();
    assert_eq!(
        log.matches("no longer exiting with the parent").count(),
        2,
        "两次 arm 的日志该都在（接着往后写）：{log}"
    );
    // 父进程守望恢复了：父进程没了，它跟着退
    let _ = reap.0[1].kill();
    let _ = reap.0[1].wait();
    assert!(
        until(Duration::from_secs(10), || exited(&mut reap.0[0])).await,
        "父进程没了还在跑\n{}",
        bed.logs()
    );
}

/// 一条长流跑着的时候走一遍：A 交给 B；B 在 A 排空期间自己的请求照样结算（过了预留的宽限期
/// 也还算在用量里）；B 在 A 排空完之前退出，**锁和排空标记都留着**；C 照常起来，看见 A 还在
/// 排空，就以「前任在排空」起来、请求号跳过去、等 A 退了才开库；开库之后用量以库为准、不重算。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_successor_that_exits_while_its_predecessor_drains_leaves_the_next_core_waiting() {
    let bed = Bed::with_daily_limit(450).await; // 长流约 90 秒
    let mut reap = Reap(vec![bed.core("a")]);
    assert!(
        until(Duration::from_secs(20), || bed.status().is_some()).await,
        "{}",
        bed.logs()
    );
    let a_pid = reap.0[0].id();
    let client = fresh_client();
    for _ in 0..3 {
        quick(&client, bed.gateway).await.unwrap();
    }
    let gw = bed.gateway;
    let long = tokio::spawn(async move {
        let r = fresh_client()
            .post(format!("http://{gw}/v1/messages"))
            .header("x-api-key", "tw-test")
            .header("content-type", "application/json")
            .body(r#"{"model":"m","max_tokens":5,"stream":true,"messages":[{"role":"user","content":"slow"}]}"#)
            .timeout(Duration::from_secs(200))
            .send()
            .await
            .map_err(|e| format!("{e:?}"))?;
        r.text().await.map_err(|e| format!("{e:?}"))
    });
    tokio::time::sleep(Duration::from_millis(500)).await;

    // A → B
    assert!(bed.arm(60).is_some_and(|v| v["version"] == 1));
    reap.0.push(bed.core("b"));
    let b_pid = reap.0[1].id();
    assert!(
        until(Duration::from_secs(20), || bed.lock_pid() == Some(b_pid)).await,
        "{}",
        bed.logs()
    );
    assert_eq!(bed.status().unwrap()["handover"], "draining");
    assert!(bed.draining_marked(), "没记下 A 在排空");

    // B 自己的请求：A 排空期间库没开，照样结算（过了 60 秒的宽限期也不丢）
    for _ in 0..5 {
        quick(&client, bed.gateway).await.unwrap();
    }
    let _ = bed.used_today();
    tokio::time::sleep(Duration::from_secs(65)).await;
    let used = bed.used_today().unwrap();
    assert!(
        used >= 3 + 5,
        "B 自己的请求没算进用量：{used}\n{}",
        bed.logs()
    );

    // B 在 A 排空完之前退出
    assert!(bed.call("POST", "/shutdown", None).is_some());
    assert!(
        until(Duration::from_secs(10), || exited(&mut reap.0[1])).await,
        "{}",
        bed.logs()
    );
    assert!(!exited(&mut reap.0[0]), "A 不该这时就退了（长流还在跑）");
    assert_eq!(bed.lock_pid(), Some(b_pid), "A 还在排空，B 把锁删了");
    assert!(bed.draining_marked(), "排空标记没了");

    // C 照常起来：前任在排空
    reap.0.push(bed.core("c"));
    let c_pid = reap.0[2].id();
    assert!(
        until(Duration::from_secs(20), || bed
            .status()
            .is_some_and(|s| s["pid"].as_u64() == Some(c_pid as u64)))
        .await,
        "{}",
        bed.logs()
    );
    assert_eq!(bed.lock_pid(), Some(c_pid));
    assert_eq!(bed.status().unwrap()["handover"], "draining");
    for _ in 0..2 {
        quick(&client, bed.gateway).await.unwrap();
    }
    let early = bed.history_ids();
    assert!(
        early.iter().all(|id| *id > 15_000_000),
        "C 的请求号没跳过前任的：{early:?}"
    );

    // 长流在 A 上收完，A 退出；C 回到平常、开库
    let body = long.await.unwrap().expect("长流断了");
    assert!(body.contains("message_stop"), "{body}");
    assert!(
        until(Duration::from_secs(20), || exited(&mut reap.0[0])).await,
        "{}",
        bed.logs()
    );
    assert!(
        until(Duration::from_secs(20), || bed
            .status()
            .is_some_and(|s| s["handover"] == "idle"))
        .await,
        "{}",
        bed.logs()
    );
    assert!(until(Duration::from_secs(10), || !bed.draining_marked()).await);
    // 库里：A 的 3 个短请求和那条长流、C 的 2 个（B 的那 5 个没进库：它退出时库还没开）
    let ids = bed.history_ids();
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "请求号撞了：{ids:?}");
    assert_eq!(ids.len(), 3 + 1 + 2, "{ids:?}\n{}", bed.logs());
    // 用量以库为准：不把 C 排队时结算过的再算一遍
    assert_eq!(bed.used_today(), Some(3 + 1 + 2), "{}", bed.logs());
    assert_ne!(a_pid, c_pid);
    assert!(
        std::fs::read_to_string(bed.dir.path().join("b.log"))
            .unwrap()
            .contains("queued request events are not recorded"),
        "B 退出时没说丢了几条\n{}",
        bed.logs()
    );
}
