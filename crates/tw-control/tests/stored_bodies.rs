//! 落盘的正文里没有原值。
//!
//! 换掉、打码在落盘那一头做（`tw_gateway::bodies::BodyRecord::for_disk`）。这里把整条路
//! 走一遍：真的网关、一个把收到的东西回显出来的假上游、存储层写进临时目录 —— 然后把
//! 正文目录里的**每一个文件**读一遍，一个原值都不能有。拦截档、观察档、关闭各一遍，
//! 整包的回答和流式的回答各一个请求。
//!
//! 读出这些正文的几条路也在这里走一遍：请求详情、按正文找、重放。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tw_control::{ConfigManager, ControlState};

/// 用户粘进对话里的三样：一把 API key、一个身份证号、连接串里的口令。三条都是出厂就开的规则
const KEY: &str = "sk-ant-api03-USERSOWNKEYAAAAAAAAAAAAAA";
const ID_NUMBER: &str = "11010519491231002X";
const DB_PASSWORD: &str = "hunter2hunter2";

fn said() -> String {
    format!("key {KEY}，身份证 {ID_NUMBER}，库 postgres://app:{DB_PASSWORD}@db.example.com/prod")
}

/// 假上游：存下收到的 body，再把用户那句话原样回显 —— 模型确实会重复你给它的东西
/// （拦截档下它看到的是占位符，观察档下是原值）。
async fn echoing_upstream() -> (SocketAddr, Arc<Mutex<Vec<u8>>>) {
    let seen: Arc<Mutex<Vec<u8>>> = Arc::default();
    let s = seen.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |body: bytes::Bytes| {
            let s = s.clone();
            async move {
                *s.lock().unwrap() = body.to_vec();
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                let text = v["messages"][0]["content"].as_str().unwrap_or("").to_string();
                let quoted = serde_json::to_string(&text).unwrap();
                if v["stream"] == true {
                    let sse = format!(
                        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"model\":\"claude-sonnet-4-5\",\"usage\":{{\"input_tokens\":10,\"output_tokens\":1}}}}}}\n\n\
                         event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
                         event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{quoted}}}}}\n\n\
                         event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
                         event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":5}}}}\n\n\
                         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
                    );
                    axum::response::Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from(sse))
                        .unwrap()
                } else {
                    axum::response::Response::builder()
                        .header("content-type", "application/json")
                        .body(Body::from(format!(
                            "{{\"type\":\"message\",\"model\":\"claude-sonnet-4-5\",\"content\":[{{\"type\":\"text\",\"text\":{quoted}}}],\"stop_reason\":\"end_turn\",\"usage\":{{\"input_tokens\":10,\"output_tokens\":5}}}}"
                        )))
                        .unwrap()
                }
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, seen)
}

struct World {
    _dir: tempfile::TempDir,
    blobs: std::path::PathBuf,
    gw: SocketAddr,
    seen: Arc<Mutex<Vec<u8>>>,
    store: Arc<tokio::sync::Mutex<tw_store::Recorder>>,
    app: axum::Router,
}

/// 网关、存储层、控制面，**和 `twcore` 一样接**：网关交出原文，落盘之前换过、打过码。
async fn world(mode: &str) -> World {
    let (up, seen) = echoing_upstream().await;
    let d = tempfile::tempdir().unwrap();
    let yaml = format!(
        "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\n\
         clients:\n  - name: 我\n    key: tw-k\n\
         providers:\n  - name: 中转\n    base_url: http://{up}\n    key: sk-upstream\n    protocol: anthropic\n    billing: free\n\
         security:\n  redact:\n    mode: {mode}\n"
    );
    let p = d.path().join("config.yaml");
    std::fs::write(&p, &yaml).unwrap();
    let cfg: tw_config::Config = serde_yaml_ng::from_str(&yaml).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();

    let (sink, mut bodies) = tw_gateway::bodies::channel();
    gw.set_body_sink(sink);
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        while let Some(b) = bodies.recv().await {
            let disk = tokio::task::spawn_blocking(move || b.for_disk())
                .await
                .unwrap();
            let which = match disk.kind {
                tw_gateway::bodies::BodyKind::Request => tw_store::Which::Request,
                tw_gateway::bodies::BodyKind::Response => tw_store::Which::Response,
                tw_gateway::bodies::BodyKind::AfterPlugins => tw_store::Which::AfterPlugins,
            };
            let stored = tw_store::StoredBody {
                id: disk.id,
                at_ms: disk.at_ms,
                which,
                body: disk.body,
                original_len: disk.original_len,
            };
            if tx.send(stored).await.is_err() {
                return;
            }
        }
    });
    let blobs = d.path().join("blobs");
    let store = tw_store::task::spawn(
        tw_store::Recorder::new(
            tw_store::Db::open(&d.path().join("data.db")).unwrap(),
            tw_store::Blobs::new(blobs.clone()),
            tw_pricing::shared(tw_pricing::PriceBook::builtin().unwrap()),
        ),
        gw.bus.record_feed().unwrap(),
        rx,
    );
    let state = ControlState {
        shutdown: Default::default(),
        remote: Default::default(),
        cfg: Arc::new(ConfigManager::new(p, gw.clone(), gw.bus.clone())),
        gateway: gw.clone(),
        store: Some(store.clone()),
        started: std::time::Instant::now(),
        price_updater: Default::default(),
        chatgpt: Default::default(),
        zai: Default::default(),
    };
    let app = tw_control::router(state);
    let addr = tw_gateway::serve(gw, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    World {
        _dir: d,
        blobs,
        gw: addr,
        seen,
        store,
        app,
    }
}

/// 发一个请求，等它落了库、两份正文都落了盘。交回它的号和客户端拿到的回答
async fn ask(w: &World, stream: bool) -> (i64, String) {
    let before = w.store.lock().await.db().count().unwrap();
    let body = serde_json::json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 64,
        "stream": stream,
        "messages": [{"role": "user", "content": said()}],
    });
    let got = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{}/v1/messages", w.gw))
        .header("x-api-key", "tw-k")
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let id = {
            let g = w.store.lock().await;
            (g.db().count().unwrap() > before).then(|| g.db().recent(None, 1).unwrap()[0].id)
        };
        if let Some(id) = id
            && files(&w.blobs)
                .iter()
                .filter(|(name, _)| name == &format!("{id}.req") || name == &format!("{id}.res"))
                .count()
                == 2
        {
            return (id, got);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "请求没有落库，或者正文没有落盘"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 正文目录里的每一个文件：(文件名, 内容)
fn files(root: &std::path::Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(days) = std::fs::read_dir(root) else {
        return out;
    };
    for day in days.flatten() {
        for f in std::fs::read_dir(day.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = f.file_name().to_string_lossy().to_string();
            // 正文先写进 `.tmp` 再改名：还在写的那份不算，列目录之后才改名走的也跳过
            if name.ends_with(".tmp") {
                continue;
            }
            let Ok(bytes) = std::fs::read(f.path()) else {
                continue;
            };
            out.push((name, String::from_utf8_lossy(&bytes).to_string()));
        }
    }
    out
}

fn stored(w: &World, name: &str) -> String {
    files(&w.blobs)
        .into_iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("没有 {name}"))
        .1
}

/// **这一条是这个文件存在的理由**：盘上没有一个文件带着原值。
fn assert_nothing_real_on_disk(w: &World) {
    let all = files(&w.blobs);
    assert!(all.len() >= 4, "{all:?}");
    for (name, text) in &all {
        for secret in [KEY, ID_NUMBER, DB_PASSWORD] {
            assert!(!text.contains(secret), "{name} 里有原值 {secret}：{text}");
        }
    }
}

async fn call(app: &axum::Router, req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let r = app.clone().oneshot(req).await.unwrap();
    let status = r.status();
    let b = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&b).unwrap_or_default())
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, serde_json::Value) {
    call(app, Request::get(path).body(Body::empty()).unwrap()).await
}

async fn post(
    app: &axum::Router,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    call(
        app,
        Request::post(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

/// 拦截档：存下来的请求带着上游收到的那些占位符，回答里的占位符和它对得上号。这个请求
/// 在路上没被改写过（同格式直通、没有改参数的规则），所以两份一个字节都不差
#[tokio::test]
async fn under_enforce_the_disk_holds_what_the_upstream_got_and_nothing_real() {
    let w = world("enforce").await;
    let (whole, answer) = ask(&w, false).await;
    // 客户端拿回来的照样是原值：落盘那一头换掉的不影响回程的还原
    assert!(answer.contains(KEY), "{answer}");
    let sent = String::from_utf8(w.seen.lock().unwrap().clone()).unwrap();
    assert_eq!(stored(&w, &format!("{whole}.req")), sent);
    assert!(
        sent.contains("key <<TW_SECRET_1>>，身份证 <<TW_ID_NUMBER_1>>，库 postgres://app:<<TW_SECRET_2>>@db.example.com/prod"),
        "{sent}"
    );
    assert!(
        stored(&w, &format!("{whole}.res"))
            .contains("key <<TW_SECRET_1>>，身份证 <<TW_ID_NUMBER_1>>"),
    );

    let (streamed, answer) = ask(&w, true).await;
    assert!(answer.contains(KEY), "{answer}");
    let sent = String::from_utf8(w.seen.lock().unwrap().clone()).unwrap();
    assert_eq!(stored(&w, &format!("{streamed}.req")), sent);
    let res = stored(&w, &format!("{streamed}.res"));
    assert!(res.contains("event: content_block_delta"), "{res}");
    assert!(res.contains("<<TW_SECRET_2>>"), "{res}");

    assert_nothing_real_on_disk(&w);
}

/// 观察档：上游收到的是原值、回显的也是原值，盘上的打了码 —— 和安全日志里一种写法
#[tokio::test]
async fn under_observe_the_upstream_sees_the_values_and_the_disk_does_not() {
    let w = world("observe").await;
    let (whole, _) = ask(&w, false).await;
    let sent = String::from_utf8(w.seen.lock().unwrap().clone()).unwrap();
    assert!(sent.contains(KEY), "观察档动了发出去的那一份：{sent}");
    let req = stored(&w, &format!("{whole}.req"));
    assert!(
        req.contains(
            "key sk-an…AAAA，身份证 …002X，库 postgres://app:hunte…ter2@db.example.com/prod"
        ),
        "{req}"
    );
    assert!(
        !req.contains("<<TW_"),
        "观察档没换过，存下来的不该有占位符：{req}"
    );
    serde_json::from_str::<serde_json::Value>(&req).expect("存下来的请求还是 JSON");
    let (streamed, _) = ask(&w, true).await;
    assert!(stored(&w, &format!("{streamed}.res")).contains("sk-an…AAAA"));

    assert_nothing_real_on_disk(&w);
}

/// 关着也一样：关掉的是出站的检测和替换，不是落盘前的打码
#[tokio::test]
async fn with_redaction_off_the_disk_still_holds_nothing_real() {
    let w = world("off").await;
    ask(&w, false).await;
    ask(&w, true).await;
    assert_nothing_real_on_disk(&w);
}

/// 读出这些正文的几条路：详情、按正文找、重放。都照常工作，也都碰不到原值
#[tokio::test]
async fn the_detail_search_and_replay_work_on_what_was_stored() {
    let w = world("enforce").await;
    let (id, _) = ask(&w, false).await;

    // 详情：存下来的就是给人看的样子，读的时候再打一遍码什么都不变
    let (st, v) = get(&w.app, &format!("/request/{id}")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let req = v["request_body"]["text"].as_str().unwrap();
    assert_eq!(req, stored(&w, &format!("{id}.req")));
    assert_eq!(v["request_body"]["truncated"], false, "{v}");
    assert_eq!(v["request_body"]["original_len"], req.len(), "{v}");
    assert!(!v.to_string().contains(KEY), "{v}");

    // 按正文找：找得到那句话，摘录里是占位符；只在原值里出现的字找不到
    let (st, v) = post(
        &w.app,
        "/history/search",
        serde_json::json!({"q": "身份证", "content": true}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let hits = v["hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "{v}");
    assert!(
        hits[0]["after"]
            .as_str()
            .unwrap()
            .contains("<<TW_ID_NUMBER_1>>"),
        "{v}"
    );
    let (_, v) = post(
        &w.app,
        "/history/search",
        serde_json::json!({"q": "USERSOWNKEY", "content": true}),
    )
    .await;
    assert!(v["hits"].as_array().unwrap().is_empty(), "{v}");

    // 重放：发出去的是存下来的那一份，占位符原样发，回显里也原样留着 —— 没有原值可还原
    let (st, v) = post(
        &w.app,
        "/replay/run",
        serde_json::json!({"id": id, "provider": "中转"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let replayed = String::from_utf8(w.seen.lock().unwrap().clone()).unwrap();
    assert_eq!(replayed, stored(&w, &format!("{id}.req")));
    assert!(!replayed.contains(KEY), "{replayed}");
    assert!(
        v["body"].as_str().unwrap().contains("key <<TW_SECRET_1>>"),
        "{v}"
    );
}
