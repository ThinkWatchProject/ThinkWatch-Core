//! 一个在跑的请求的实时内容（`GET /request/{id}/live`），和结束时另存的报文头、上游那一边的
//! 正文。
//!
//! 整条路走一遍：真的网关、存储层写进临时目录、控制面，两家说 OpenAI Chat 的假上游 ——
//! 第一家回 500（故障转移到第二家），第二家流式回答、回显用户的话，答到一半停住等测试放行。
//! 客户端说 Anthropic，所以两边的报文不一样（格式转换）。在它停住的时候订阅：先补发到那时
//! 为止的，放行之后接着收到结尾。**原值一个都不能出现**：用户粘进对话的密钥、网关的钥匙、
//! 上游的钥匙，实时的和落盘的都一样。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::StreamExt;
use tower::ServiceExt;
use tw_api::{HeadView, LiveContent, LiveOutcome, WireDir, WireSide};
use tw_control::{ConfigManager, ControlState};

/// 用户粘进对话里的一把 API key（出厂就开的规则认得它）
const KEY: &str = "sk-ant-api03-USERSOWNKEYAAAAAAAAAAAAAA";
/// 网关的钥匙：客户端拿它来
const GATEWAY_KEY: &str = "tw-0123456789abcdef0123456789abcdef";
/// 上游的钥匙：网关拿它去
const UPSTREAM_KEY: &str = "sk-upstream-0123456789abcdefABCDEF";

/// 说 OpenAI Chat 的假上游：回显用户最后一句话，发完开头两帧就停住，等 `gate` 放行再发完
async fn chat_upstream(gate: Arc<tokio::sync::Notify>) -> SocketAddr {
    let app = axum::Router::new().fallback(move |body: bytes::Bytes| {
        let gate = gate.clone();
        async move {
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let last = v["messages"]
                .as_array()
                .and_then(|m| m.last())
                .cloned()
                .unwrap_or_default();
            let text = match &last["content"] {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Array(parts) => parts
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<String>(),
                _ => String::new(),
            };
            let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
                format!(
                    "data: {}\n\n",
                    serde_json::json!({
                        "id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "gpt-x",
                        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
                    })
                )
            };
            let opening = format!(
                "{}{}",
                chunk(serde_json::json!({"role": "assistant", "content": ""}), serde_json::Value::Null),
                chunk(serde_json::json!({"content": format!("echo: {text}")}), serde_json::Value::Null),
            );
            let rest = format!(
                "{}data: {}\n\ndata: [DONE]\n\n",
                chunk(serde_json::json!({"content": " done"}), serde_json::Value::Null),
                serde_json::json!({
                    "id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "gpt-x",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
                }),
            );
            gated(opening, rest, gate)
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

/// 一个 SSE 回答：先发 `opening`，停住等 `gate` 放行，再发 `rest`
fn gated(
    opening: String,
    rest: String,
    gate: Arc<tokio::sync::Notify>,
) -> axum::response::Response {
    let first =
        futures::stream::once(async move { Ok::<_, std::io::Error>(bytes::Bytes::from(opening)) });
    let second = futures::stream::once(async move {
        gate.notified().await;
        Ok::<_, std::io::Error>(bytes::Bytes::from(rest))
    });
    axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(first.chain(second)))
        .unwrap()
}

/// 说 Anthropic 的假上游：流式回显用户的话，发完开头就停住，等 `gate` 放行再发完
async fn anthropic_upstream(gate: Arc<tokio::sync::Notify>) -> SocketAddr {
    let app = axum::Router::new().fallback(move |body: bytes::Bytes| {
        let gate = gate.clone();
        async move {
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let text = v["messages"][0]["content"].as_str().unwrap_or("").to_string();
            let quoted = serde_json::to_string(&format!("echo: {text}")).unwrap();
            let opening = format!(
                "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"model\":\"gpt-x\",\"usage\":{{\"input_tokens\":10,\"output_tokens\":1}}}}}}\n\n\
                 event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
                 event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{quoted}}}}}\n\n"
            );
            let rest = "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                 event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n\
                 event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
                .to_string();
            gated(opening, rest, gate)
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

/// 一回 500 的上游
async fn broken_upstream() -> SocketAddr {
    let app = axum::Router::new().fallback(|| async {
        axum::response::Response::builder()
            .status(500)
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"error":{"message":"boom","type":"server_error"}}"#,
            ))
            .unwrap()
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

struct World {
    _dir: tempfile::TempDir,
    blobs: std::path::PathBuf,
    gw: SocketAddr,
    bus: tw_observe::EventBus,
    store: Arc<tokio::sync::Mutex<tw_store::Recorder>>,
    app: axum::Router,
}

/// 网关、存储层、控制面，**和 `twcore` 一样接**。`providers` 是配置里 `providers:` 下面那几行
async fn world(providers: &str, mode: &str) -> World {
    let d = tempfile::tempdir().unwrap();
    let yaml = format!(
        "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\n\
         clients:\n  - name: 我\n    key: {GATEWAY_KEY}\n\
         providers:\n{providers}\
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
            use tw_gateway::bodies::BodyKind;
            let which = match disk.kind {
                BodyKind::Request => tw_store::Which::Request,
                BodyKind::Response => tw_store::Which::Response,
                BodyKind::AfterPlugins => tw_store::Which::AfterPlugins,
                BodyKind::UpstreamRequest => tw_store::Which::UpstreamRequest,
                BodyKind::ClientResponse => tw_store::Which::ClientResponse,
                BodyKind::Heads => tw_store::Which::Heads,
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
    let bus = gw.bus.clone();
    let addr = tw_gateway::serve(gw, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    World {
        _dir: d,
        blobs,
        gw: addr,
        bus,
        store,
        app,
    }
}

fn said() -> String {
    format!("帮我看看这把 key：{KEY}")
}

/// 一个发出去的请求：它的号，客户端收到第一段回答时响一下，收齐了的回答（读完才有）
struct Asked {
    id: u64,
    first: tokio::sync::oneshot::Receiver<()>,
    answer: tokio::task::JoinHandle<String>,
}

/// 发一个流式请求（Anthropic 格式），在后台一直读
async fn ask(w: &World) -> Asked {
    let mut rx = w.bus.subscribe();
    let body = serde_json::json!({
        "model": "gpt-x",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role": "user", "content": said()}],
    });
    let gw = w.gw;
    let (tx, first) = tokio::sync::oneshot::channel();
    let answer = tokio::spawn(async move {
        let resp = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("http://{gw}/v1/messages"))
            .header("x-api-key", GATEWAY_KEY)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        let mut tx = Some(tx);
        let mut got = Vec::new();
        let mut chunks = resp.bytes_stream();
        while let Some(c) = chunks.next().await {
            got.extend_from_slice(&c.unwrap());
            if let Some(tx) = tx.take() {
                let _ = tx.send(());
            }
        }
        String::from_utf8(got).unwrap()
    });
    let id = loop {
        let e = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("没有开始")
            .unwrap();
        if let tw_api::Event::RequestStarted { id, .. } = e {
            break id;
        }
    };
    Asked { id, first, answer }
}

/// 订阅的那一头：控制面回的 SSE，一条一条读出来
struct Live {
    body: axum::body::BodyDataStream,
    buf: Vec<u8>,
}

impl Live {
    async fn open(app: &axum::Router, id: u64) -> Result<Live, (StatusCode, serde_json::Value)> {
        let r = app
            .clone()
            .oneshot(
                Request::get(tw_api::ep::RequestLive::path(id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        if r.status() != StatusCode::OK {
            let status = r.status();
            let b = axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap();
            return Err((status, serde_json::from_slice(&b).unwrap_or_default()));
        }
        assert_eq!(
            r.headers()["content-type"].to_str().unwrap(),
            "text/event-stream"
        );
        Ok(Live {
            body: r.into_body().into_data_stream(),
            buf: Vec::new(),
        })
    }

    /// 下一条。流关了是 None
    async fn next(&mut self) -> Option<LiveContent> {
        loop {
            if let Some(i) = self.buf.windows(2).position(|w| w == b"\n\n") {
                let frame = String::from_utf8(self.buf.drain(..i + 2).collect()).unwrap();
                let mut event = None;
                let mut data = String::new();
                for line in frame.lines() {
                    if let Some(e) = line.strip_prefix("event:") {
                        event = Some(e.trim().to_string());
                    } else if let Some(d) = line.strip_prefix("data:") {
                        data.push_str(d.trim_start());
                    }
                }
                // 心跳（一行注释）没有 `event:`
                let Some(event) = event else { continue };
                return Some(
                    LiveContent::parse(&event, &data)
                        .unwrap_or_else(|| panic!("读不懂的一条：{event} {data}")),
                );
            }
            let chunk = tokio::time::timeout(Duration::from_secs(10), self.body.next())
                .await
                .expect("实时内容停住了")?
                .unwrap();
            self.buf.extend_from_slice(&chunk);
        }
    }

    /// 一直读到 `pred` 认的那一条为止，交回读到的全部
    async fn until(&mut self, pred: impl Fn(&[LiveContent]) -> bool) -> Vec<LiveContent> {
        let mut got = Vec::new();
        while !pred(&got) {
            got.push(self.next().await.expect("没等到就关了"));
        }
        got
    }
}

fn heads(evs: &[LiveContent]) -> Vec<&HeadView> {
    evs.iter()
        .filter_map(|e| match e {
            LiveContent::Head(h) => Some(h),
            _ => None,
        })
        .collect()
}

fn text(evs: &[LiveContent], side: WireSide, dir: WireDir, attempt: u32) -> String {
    evs.iter()
        .filter_map(|e| match e {
            LiveContent::Body(b) if b.side == side && b.dir == dir && b.attempt == attempt => {
                Some(b.text.as_str())
            }
            _ => None,
        })
        .collect()
}

fn head(evs: &[LiveContent], side: WireSide, dir: WireDir, attempt: u32) -> &HeadView {
    heads(evs)
        .into_iter()
        .rfind(|h| h.side == side && h.dir == dir && h.attempt == attempt)
        .unwrap_or_else(|| panic!("没有 {side} {dir} {attempt} 的报文头：{evs:#?}"))
}

fn header<'a>(h: &'a HeadView, name: &str) -> &'a str {
    h.headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .unwrap_or_else(|| panic!("{} 没有 {name}：{h:?}", h.line))
}

/// 一个原值都没有
fn assert_no_secret(what: &str, text: &str) {
    for secret in [KEY, GATEWAY_KEY, UPSTREAM_KEY] {
        assert!(!text.contains(secret), "{what} 里有原值 {secret}：{text}");
    }
}

/// 正文目录里叫 `name` 的那个文件在了没有
fn blob_exists(root: &std::path::Path, name: &str) -> bool {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .any(|day| day.path().join(name).exists())
}

/// 等这个请求落了库、`files` 都落了盘
async fn settled(w: &World, id: u64, files: &[&str]) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let row = w.store.lock().await.db().get(id as i64).unwrap().is_some();
        if row
            && files
                .iter()
                .all(|f| blob_exists(&w.blobs, &format!("{id}.{f}")))
        {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "没有落库或落盘");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn detail(w: &World, id: u64) -> (StatusCode, serde_json::Value) {
    let r = w
        .app
        .clone()
        .oneshot(
            Request::get(tw_api::ep::RequestDetail::path(id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let b = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&b).unwrap())
}

/// 客户端收到了回答的开头（上游停在半路）之后才订阅：交回订阅和补发的那些 —— **订阅之前
/// 就有了的**，一条一条读到不再有新的为止
async fn subscribe_midway(w: &World, asked: &mut Asked) -> (Live, Vec<LiveContent>) {
    tokio::time::timeout(Duration::from_secs(10), &mut asked.first)
        .await
        .expect("客户端没收到回答的开头")
        .unwrap();
    let mut l = Live::open(&w.app, asked.id).await.expect("在跑");
    let mut replayed = Vec::new();
    while let Ok(Some(e)) = tokio::time::timeout(Duration::from_millis(300), l.next()).await {
        replayed.push(e);
    }
    (l, replayed)
}

/// 故障转移、格式转换、流式、半路订阅：先补发、再接着收，客户端和上游两边各是各的样子；
/// 结束之后 404，报文头和上游那一边的正文落了盘
#[tokio::test]
async fn a_converted_stream_with_a_failover_is_seen_live_and_kept_when_it_ends() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let (dead, up) = (broken_upstream().await, chat_upstream(gate.clone()).await);
    let w = world(
        &format!(
            "  - name: 坏的\n    base_url: http://{dead}\n    key: {UPSTREAM_KEY}\n    protocol: openai-chat\n    billing: free\n\
             \x20 - name: 好的\n    base_url: http://{up}\n    key: {UPSTREAM_KEY}\n    protocol: openai-chat\n    billing: free\n"
        ),
        "enforce",
    )
    .await;
    let mut asked = ask(&w).await;
    let id = asked.id;

    // 上游停在半路：客户端收到了回答的开头，再订阅
    let (mut live, replayed) = subscribe_midway(&w, &mut asked).await;
    let l = &mut live;
    let replayed = &replayed;

    // 补发的：两跳的报文头、第一跳的错误、客户端的请求、第二跳发出去的（转换过的）和回来的开头
    let order: Vec<(WireSide, WireDir, u32)> = heads(replayed)
        .iter()
        .map(|h| (h.side, h.dir, h.attempt))
        .collect();
    assert_eq!(
        order,
        [
            (WireSide::Client, WireDir::Request, 0),
            (WireSide::Upstream, WireDir::Request, 1),
            (WireSide::Upstream, WireDir::Response, 1),
            (WireSide::Upstream, WireDir::Request, 2),
            (WireSide::Upstream, WireDir::Response, 2),
            (WireSide::Client, WireDir::Response, 0),
        ]
    );
    let creq = head(replayed, WireSide::Client, WireDir::Request, 0);
    assert_eq!(creq.line, "POST /v1/messages HTTP/1.1");
    assert_eq!(
        header(creq, "x-api-key"),
        tw_secret::mask_secret(GATEWAY_KEY)
    );
    let ureq = head(replayed, WireSide::Upstream, WireDir::Request, 2);
    assert_eq!(ureq.line, format!("POST http://{up}/v1/chat/completions"));
    assert_eq!(
        header(ureq, "authorization"),
        format!("Bearer {}", tw_secret::mask_secret(UPSTREAM_KEY))
    );
    assert_eq!(
        head(replayed, WireSide::Upstream, WireDir::Response, 1).line,
        "HTTP/1.1 500 Internal Server Error"
    );
    // 第一跳回的错误不落盘（原因在尝试链上），那时没人在看，补发里没有它
    assert!(text(replayed, WireSide::Upstream, WireDir::Response, 1).is_empty());
    assert!(text(replayed, WireSide::Upstream, WireDir::Request, 1).is_empty());
    assert_eq!(
        head(replayed, WireSide::Client, WireDir::Response, 0).line,
        "HTTP/1.1 200 OK"
    );
    // 客户端发来的是 Anthropic 的，发给上游的是转换过的 Chat 的；拦截档下两边都是占位符
    let client_req = text(replayed, WireSide::Client, WireDir::Request, 0);
    let up_req = text(replayed, WireSide::Upstream, WireDir::Request, 2);
    assert!(client_req.contains("\"max_tokens\":64"), "{client_req}");
    assert!(client_req.contains("<<TW_SECRET_1>>"), "{client_req}");
    let converted: serde_json::Value = serde_json::from_str(&up_req).expect("发出去的是 JSON");
    assert_eq!(converted["messages"][0]["role"], "user", "{up_req}");
    assert!(up_req.contains("<<TW_SECRET_1>>"), "{up_req}");
    let up_so_far = text(replayed, WireSide::Upstream, WireDir::Response, 2);
    assert!(up_so_far.contains("chat.completion.chunk"), "{up_so_far}");
    assert!(up_so_far.contains("echo: "), "{up_so_far}");

    // 放行：接着收到结尾，`end` 之后关闭
    gate.notify_one();
    let rest = l
        .until(|got| matches!(got.last(), Some(LiveContent::End(_))))
        .await;
    match rest.last() {
        Some(LiveContent::End(end)) => {
            assert_eq!(end.outcome, LiveOutcome::Finished);
            assert_eq!(end.status, Some(200));
        }
        other => panic!("{other:?}"),
    }
    assert!(l.next().await.is_none(), "end 之后流关了");
    let all: Vec<LiveContent> = replayed.iter().cloned().chain(rest).collect();
    let up_answer = text(&all, WireSide::Upstream, WireDir::Response, 2);
    assert!(up_answer.contains("[DONE]"), "{up_answer}");
    // 客户端收到的是转回 Anthropic 的流：它的回显（客户端那里是原值）和落盘一样换成占位符
    let client_answer = text(&all, WireSide::Client, WireDir::Response, 0);
    assert!(
        client_answer.contains("event: message_start"),
        "{client_answer}"
    );
    assert!(
        client_answer.contains("content_block_delta"),
        "{client_answer}"
    );
    assert!(client_answer.contains("<<TW_SECRET_1>>"), "{client_answer}");
    assert!(!client_answer.contains("chat.completion.chunk"));
    let got = asked.answer.await.unwrap();
    assert!(got.contains(KEY), "客户端那里还原成了原值：{got}");
    for e in &all {
        assert_no_secret("实时内容", &serde_json::to_string(e).unwrap());
    }

    // 结束了：不在跑
    let (status, body) = match Live::open(&w.app, id).await {
        Ok(_) => panic!("结束了还订阅得到"),
        Err(e) => e,
    };
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "control.request_not_running");

    // 落盘的：报文头（和实时的同一份）、发给上游的请求体、上游的原话、客户端收到的回答
    settled(&w, id, &["req", "res", "heads", "up-req", "client-res"]).await;
    let (status, d) = detail(&w, id).await;
    assert_eq!(status, StatusCode::OK);
    assert_no_secret("请求详情", &d.to_string());
    let stored: Vec<HeadView> = serde_json::from_value(d["heads"].clone()).unwrap();
    let live_heads: Vec<HeadView> = heads(&all).into_iter().cloned().collect();
    assert_eq!(stored, live_heads, "落盘的报文头就是实时发出去的那些");
    assert_eq!(d["upstream_request_body"]["text"], up_req);
    assert_eq!(d["upstream_response_body"]["text"], up_answer);
    assert_eq!(d["response_body"]["text"], client_answer);
    assert_eq!(d["request_body"]["text"], client_req);
}

/// 不转换、不改名：客户端那一边就是上游那一段，实时的各发一份，落盘不另存
#[tokio::test]
async fn an_answer_passed_on_unchanged_is_the_client_side_too() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let up = anthropic_upstream(gate.clone()).await;
    let w = world(
        &format!(
            "  - name: 官方\n    base_url: http://{up}\n    key: {UPSTREAM_KEY}\n    protocol: anthropic\n    billing: free\n"
        ),
        "observe",
    )
    .await;
    let mut asked = ask(&w).await;
    let id = asked.id;
    let (mut l, replayed) = subscribe_midway(&w, &mut asked).await;
    // 补发的里面已经有两边的开头
    assert!(text(&replayed, WireSide::Client, WireDir::Response, 0).contains("echo: "));
    gate.notify_one();
    let rest = l
        .until(|got| matches!(got.last(), Some(LiveContent::End(_))))
        .await;
    let all: Vec<LiveContent> = replayed.into_iter().chain(rest).collect();
    // 两边一字不差，补发的那一截也是
    let up_side = text(&all, WireSide::Upstream, WireDir::Response, 1);
    let client_side = text(&all, WireSide::Client, WireDir::Response, 0);
    assert!(up_side.contains("message_stop"), "{up_side}");
    assert_eq!(up_side, client_side);
    // 观察档：上游收到、回显的是原值，实时发出去的打了码
    assert!(up_side.contains(&tw_secret::mask_secret(KEY)), "{up_side}");
    for e in &all {
        assert_no_secret("实时内容", &serde_json::to_string(e).unwrap());
    }
    let got = asked.answer.await.unwrap();
    assert!(got.contains("echo: "), "{got}");
    settled(&w, id, &["req", "res", "heads"]).await;
    let (_, d) = detail(&w, id).await;
    assert!(d["upstream_request_body"].is_null(), "{d}");
    assert!(d["upstream_response_body"].is_null(), "{d}");
    let answer = d["response_body"]["text"].as_str().unwrap();
    assert!(answer.contains("event: message_start"), "{answer}");
    // 观察档：上游收到、回显的是原值，存下来的打了码
    assert!(answer.contains(&tw_secret::mask_secret(KEY)), "{answer}");
    assert_no_secret("请求详情", &d.to_string());
    assert!(!blob_exists(&w.blobs, &format!("{id}.up-req")));
    assert!(!blob_exists(&w.blobs, &format!("{id}.client-res")));
    let stored: Vec<HeadView> = serde_json::from_value(d["heads"].clone()).unwrap();
    assert_eq!(
        stored
            .iter()
            .map(|h| (h.side, h.dir, h.attempt))
            .collect::<Vec<_>>(),
        [
            (WireSide::Client, WireDir::Request, 0),
            (WireSide::Upstream, WireDir::Request, 1),
            (WireSide::Upstream, WireDir::Response, 1),
            (WireSide::Client, WireDir::Response, 0),
        ]
    );
}

/// 从来没有过的请求：404，和叫停一个不在跑的请求同一个码
#[tokio::test]
async fn a_request_that_is_not_running_has_no_live_content() {
    let w = world(
        "  - name: 官方\n    base_url: http://127.0.0.1:9\n    key: sk-x\n    protocol: anthropic\n",
        "observe",
    )
    .await;
    let (status, body) = match Live::open(&w.app, 424242).await {
        Ok(_) => panic!("不在跑的也订阅得到"),
        Err(e) => e,
    };
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "control.request_not_running");
    assert_eq!(body["args"]["id"], "424242");
}
