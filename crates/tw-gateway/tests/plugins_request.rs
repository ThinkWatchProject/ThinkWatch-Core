//! 插件的请求钩子，从客户端到假上游走一整圈。
//!
//! 插件用替身（`tw_gateway::plugin::host::double`）：钩子是 Rust 闭包。要证明的是
//! 接线 —— 跑在哪一步、跑几次、改过的请求谁看得见、拒绝和出错怎么回给客户端、
//! 插件看到的是不是占位符 —— 这些和插件用什么语言写无关。
//!
//! 请求钩子排在路由之后、**每发往一个上游跑一次**（契约附录二）：换上游从客户端的
//! 原话重来，范围按这一次的上游和发出去的模型名算，同一家重发不重跑。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::Uri;
use bytes::Bytes;
use serde_json::{Value, json};
use tw_api::{OnError, Permission};
use tw_config::{Client, Config, Listen, Protocol, Provider, RedactPolicy, Security, SecurityMode};
use tw_gateway::plugin::host::double::{self, Double};
use tw_gateway::plugin::{
    Active, Broken, Invocation, PluginSet, RequestOutcome, RunError, Scope, State as PluginState,
};

const USER_KEY: &str = "sk-ant-api03-USERSOWNKEYAAAAAAAAAAAAAA";

/// 假上游：记下收到的每一个请求（路径和正文，正文连同原样的字节），按格式回一个最简单
/// 的回答。`fail_first` 次请求回 500，用来看故障转移。
#[derive(Clone, Default)]
struct Upstream {
    seen: Arc<Mutex<Vec<(String, Value)>>>,
    raw: Arc<Mutex<Vec<Bytes>>>,
    fail_first: Arc<AtomicUsize>,
}

impl Upstream {
    /// 先回 `n` 次 500
    fn failing(n: usize) -> Self {
        let u = Upstream::default();
        u.fail_first.store(n, Ordering::SeqCst);
        u
    }

    /// 收到的第 `i` 个请求的系统提示，整个写成一串（Anthropic 的 system 可能是几块）
    fn system(&self, i: usize) -> String {
        self.seen.lock().unwrap()[i].1["system"].to_string()
    }

    fn hits(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

async fn start_upstream(u: Upstream) -> SocketAddr {
    async fn answer(State(u): State<Upstream>, uri: Uri, body: Bytes) -> axum::response::Response {
        let path = uri.path().to_string();
        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        u.seen.lock().unwrap().push((path.clone(), v));
        u.raw.lock().unwrap().push(body.clone());
        if u.fail_first.load(Ordering::SeqCst) > 0 {
            u.fail_first.fetch_sub(1, Ordering::SeqCst);
            return axum::response::Response::builder()
                .status(500)
                .body(axum::body::Body::from("{\"error\":{\"message\":\"boom\"}}"))
                .unwrap();
        }
        let reply = if path.contains("/messages") {
            json!({ "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
                    "content": [{ "type": "text", "text": "ok" }], "stop_reason": "end_turn",
                    "usage": { "input_tokens": 1, "output_tokens": 1 } })
        } else if path.contains("/chat/completions") {
            json!({ "id": "c1", "object": "chat.completion", "model": "m",
                    "choices": [{ "index": 0, "message": { "role": "assistant", "content": "ok" }, "finish_reason": "stop" }],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1 } })
        } else if path.contains("/responses") {
            json!({ "id": "resp_1", "object": "response", "status": "completed", "model": "m",
                    "output": [{ "type": "message", "id": "msg_1", "role": "assistant",
                                 "content": [{ "type": "output_text", "text": "ok" }] }],
                    "usage": { "input_tokens": 1, "output_tokens": 1 } })
        } else {
            json!({ "candidates": [{ "content": { "role": "model", "parts": [{ "text": "ok" }] }, "finishReason": "STOP" }],
                    "usageMetadata": { "promptTokenCount": 1, "candidatesTokenCount": 1 } })
        };
        axum::response::Response::builder()
            .header("content-type", "application/json")
            .body(axum::body::Body::from(reply.to_string()))
            .unwrap()
    }
    let app = Router::new()
        .fallback(axum::routing::post(answer))
        .with_state(u);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

struct Gw {
    addr: SocketAddr,
    state: tw_gateway::AppState,
    bodies: tokio::sync::mpsc::Receiver<tw_gateway::bodies::BodyRecord>,
    runs: Arc<Mutex<Vec<tw_gateway::plugin::RunRecord>>>,
}

impl Gw {
    /// 一个插件到现在的计数
    fn stats(&self, id: &str) -> tw_api::PluginStats {
        self.state.runtime().plugins.get(id).unwrap().stats.view()
    }

    /// 记下的每一次运行：`(插件, 结局, 跑在第几跳)`，按先后
    fn runs(&self) -> Vec<(String, String, u64)> {
        self.runs
            .lock()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r.run.plugin_id.clone(),
                    r.run.outcome.slug().to_string(),
                    r.run
                        .detail
                        .as_ref()
                        .and_then(|d| d["attempt"].as_u64())
                        .expect("every run says which attempt it ran on"),
                )
            })
            .collect()
    }

    /// 插件改过之后存下来的那一份请求体（没有就是 None）
    async fn after_plugins(&mut self) -> Option<Value> {
        self.bodies()
            .await
            .into_iter()
            .find(|b| b.kind == tw_gateway::bodies::BodyKind::AfterPlugins)
            .map(|b| serde_json::from_slice(&b.body).unwrap())
    }

    /// 交去存的正文，一直收到 `wait` 里再没有新的为止
    async fn bodies(&mut self) -> Vec<tw_gateway::bodies::BodyRecord> {
        let mut out = Vec::new();
        while let Ok(Some(b)) =
            tokio::time::timeout(Duration::from_millis(300), self.bodies.recv()).await
        {
            out.push(b);
        }
        out
    }
}

fn provider(name: &str, base: SocketAddr, protocol: Protocol) -> Provider {
    Provider {
        name: name.into(),
        base_url: format!("http://{base}"),
        key: Some("sk-upstream".into()),
        protocol: Some(protocol),
        ..Default::default()
    }
}

async fn gateway(providers: Vec<Provider>, mode: SecurityMode, entries: Vec<Arc<Active>>) -> Gw {
    gateway_with(providers, mode, entries, Security::default()).await
}

async fn gateway_with(
    providers: Vec<Provider>,
    mode: SecurityMode,
    entries: Vec<Arc<Active>>,
    mut security: Security,
) -> Gw {
    security.redact = RedactPolicy {
        mode,
        ..Default::default()
    };
    gateway_of(
        Config {
            providers,
            security,
            ..config()
        },
        entries,
    )
    .await
}

/// 一个密钥（`claude-code`），别的都空着
fn config() -> Config {
    Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "claude-code".into(),
            key: "tw-testkey".into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

async fn gateway_of(cfg: Config, entries: Vec<Arc<Active>>) -> Gw {
    let state = tw_gateway::AppState::new(cfg).unwrap();
    state.swap_plugins(PluginSet::new(entries));
    let (tx, bodies) = tw_gateway::bodies::channel();
    state.set_body_sink(tx);
    let runs: Arc<Mutex<Vec<tw_gateway::plugin::RunRecord>>> = Arc::default();
    let (rtx, mut rrx) = tokio::sync::mpsc::channel(tw_gateway::plugin::RUN_CHANNEL_CAP);
    state.set_plugin_sink(rtx);
    let r = runs.clone();
    tokio::spawn(async move {
        while let Some(rec) = rrx.recv().await {
            r.lock().unwrap().push(rec);
        }
    });
    let addr = tw_gateway::serve(state.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    Gw {
        addr,
        state,
        bodies,
        runs,
    }
}

fn entry(id: &str, d: Double) -> Arc<Active> {
    entry_with(id, d, |_| {})
}

/// 装一个插件，名字是 `Plugin {id}`，再按 `f` 改几样（出错时怎么办、范围）
fn entry_with(id: &str, d: Double, f: impl FnOnce(&mut Active)) -> Arc<Active> {
    let mut a = double::active(id, d);
    a.name = format!("Plugin {id}");
    f(&mut a);
    Arc::new(a)
}

async fn post(gw: &Gw, path: &str, body: &Value) -> (u16, Value) {
    let r = reqwest::Client::new()
        .post(format!("http://{}{path}", gw.addr))
        .header("x-api-key", "tw-testkey")
        .header("content-type", "application/json")
        .header("user-agent", "claude-cli/2.1.0 (external, cli)")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let text = r.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

fn anthropic_body() -> Value {
    json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 64,
        "system": [{ "type": "text", "text": "You are Claude Code.", "cache_control": { "type": "ephemeral" } }],
        "messages": [{ "role": "user", "content": "hello" }]
    })
}

/// 在系统提示末尾加一句的插件
fn add_date() -> Double {
    Double::new("add date")
        .permit(&[Permission::System])
        .on_request(|mut view, _ctx| {
            let s = view["system"].as_str().unwrap().to_string();
            view["system"] = json!(format!("{s}\n\nToday is 2026-10-02."));
            Invocation::ok(RequestOutcome::Changed(view))
        })
}

#[tokio::test]
async fn a_changed_request_is_what_the_upstream_receives_and_the_original_is_kept_for_the_record() {
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let mut gw = gateway(
        vec![provider("a", base, Protocol::Anthropic)],
        SecurityMode::Enforce,
        vec![entry("add-date", add_date())],
    )
    .await;
    let (status, _) = post(&gw, "/v1/messages", &anthropic_body()).await;
    assert_eq!(status, 200);
    let seen = up.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    let sys = seen[0].1["system"].as_array().unwrap();
    assert_eq!(sys.len(), 2);
    // 缓存断点留在原来那一块上
    assert_eq!(sys[0]["cache_control"], json!({ "type": "ephemeral" }));
    assert_eq!(
        sys[1],
        json!({ "type": "text", "text": "Today is 2026-10-02." })
    );
    // 存下来的：客户端发来的原样，和插件改过的那一份，挂在同一个请求上
    let bodies = gw.bodies().await;
    let req = bodies
        .iter()
        .find(|b| b.kind == tw_gateway::bodies::BodyKind::Request)
        .expect("the request body");
    let stored: Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(stored, anthropic_body());
    let after = bodies
        .iter()
        .find(|b| b.kind == tw_gateway::bodies::BodyKind::AfterPlugins)
        .expect("the body after plugins");
    assert_eq!(after.id, req.id);
    let after: Value = serde_json::from_slice(&after.body).unwrap();
    assert_eq!(after["system"][1]["text"], "Today is 2026-10-02.");
    let st = gw.stats("add-date");
    assert_eq!((st.calls, st.changed), (1, 1));
}

#[tokio::test]
async fn an_unchanged_result_sends_the_original_bytes() {
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let same = Double::new("look only")
        .permit(&[Permission::Messages])
        .on_request(|view, _| Invocation::ok(RequestOutcome::Changed(view)));
    let gw = gateway(
        vec![provider("a", base, Protocol::Anthropic)],
        SecurityMode::Off,
        vec![entry("same", same)],
    )
    .await;
    // 不是规范写法的 JSON（多余的空格、键的顺序）：发出去还是这些字节
    let raw = "{ \"messages\": [{\"role\":\"user\",\"content\":\"hi\"}],  \"model\":\"claude-sonnet-4-5\", \"max_tokens\": 8 }";
    let r = reqwest::Client::new()
        .post(format!("http://{}/v1/messages", gw.addr))
        .header("x-api-key", "tw-testkey")
        .body(raw)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // 一个字节都不差：插件跑在这一跳上，没改就发原样
    assert_eq!(
        up.raw.lock().unwrap()[0],
        Bytes::from_static(raw.as_bytes())
    );
    let st = gw.stats("same");
    assert_eq!((st.calls, st.changed), (1, 0));
    let mut gw = gw;
    assert!(
        gw.bodies()
            .await
            .iter()
            .all(|b| b.kind != tw_gateway::bodies::BodyKind::AfterPlugins)
    );
}

/// 插件换的模型名只换掉发给这一家的名字：**不重新路由**（换成 `gpt-5`，按规则本该去 b，
/// 还是发给 a），记录上客户端要的那个照旧，尝试链上记的是发出去的那个
#[tokio::test]
async fn a_new_model_renames_what_this_upstream_gets_and_the_record_keeps_the_asked_one() {
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let other = Upstream::default();
    let other_base = start_upstream(other.clone()).await;
    let swap = Double::new("swap model")
        .permit(&[Permission::Params])
        .on_request(|mut view, ctx| {
            assert_eq!(ctx["model"], "claude-sonnet-4-5");
            view["params"]["model"] = json!("gpt-5");
            Invocation::ok(RequestOutcome::Changed(view))
        });
    let gw = gateway_of(
        Config {
            providers: vec![
                provider("a", base, Protocol::Anthropic),
                provider("b", other_base, Protocol::Anthropic),
            ],
            routes: by_model(None),
            ..config()
        },
        vec![entry("swap", swap)],
    )
    .await;
    let rx = gw.state.bus.subscribe();
    let (status, _) = post(&gw, "/v1/messages", &anthropic_body()).await;
    assert_eq!(status, 200);
    assert_eq!((up.hits(), other.hits()), (1, 0), "the plugin re-routed it");
    assert_eq!(up.seen.lock().unwrap()[0].1["model"], "gpt-5");
    let mut rx = rx;
    let mut started_model = None;
    let mut attempt_model = None;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
        match ev {
            tw_api::Event::RequestStarted { model, .. } => started_model = Some(model),
            tw_api::Event::RequestRouted { attempts, .. } => {
                attempt_model = attempts.last().and_then(|a| a.model.clone())
            }
            _ => {}
        }
    }
    assert_eq!(started_model.as_deref(), Some("claude-sonnet-4-5"));
    assert_eq!(attempt_model.as_deref(), Some("gpt-5"));
}

/// `claude-*` 发给 a（`set_model` 给了的话改名发），别的发给 b
fn by_model(set_model: Option<&str>) -> Vec<tw_engine::RouteSet> {
    vec![tw_engine::RouteSet::default_with(vec![
        tw_engine::Rule {
            name: "claude".into(),
            when: tw_engine::rule::When {
                model: Some("claude-*".into()),
                ..Default::default()
            },
            to: Some("a".into()),
            set: set_model.map(|m| tw_engine::SetAction {
                model: Some(m.into()),
                ..Default::default()
            }),
            deny: None,
        },
        tw_engine::Rule {
            name: "rest".into(),
            when: Default::default(),
            to: Some("b".into()),
            set: None,
            deny: None,
        },
    ])]
}

/// 插件看到的模型名是发给这一家的（规则改写之后的）：`ctx.model`、视图里的 `model` 和
/// `params.model` 都是它；`ctx.requested_model` 是客户端要的，`ctx.upstream` 是这一家。
/// 插件再换名字，盖过规则的改写
#[tokio::test]
async fn the_hook_sees_the_upstream_the_sent_model_and_the_asked_one() {
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let saw = Arc::new(Mutex::new(Value::Null));
    let s = saw.clone();
    let look = Double::new("look")
        .permit(&[Permission::System, Permission::Params])
        .on_request(move |mut view, ctx| {
            *s.lock().unwrap() = json!({ "view": view.clone(), "ctx": ctx });
            view["system"] = json!("short");
            Invocation::ok(RequestOutcome::Changed(view))
        });
    let gw = gateway_of(
        Config {
            providers: vec![provider("a", base, Protocol::Anthropic)],
            routes: by_model(Some("glm-4.6")),
            ..config()
        },
        vec![entry("look", look)],
    )
    .await;
    let (status, _) = post(&gw, "/v1/messages", &anthropic_body()).await;
    assert_eq!(status, 200);
    let saw = saw.lock().unwrap().clone();
    assert_eq!(saw["ctx"]["upstream"], "a");
    assert_eq!(saw["ctx"]["model"], "glm-4.6");
    assert_eq!(saw["ctx"]["requested_model"], "claude-sonnet-4-5");
    assert_eq!(saw["ctx"]["client"], "claude-code");
    assert_eq!(saw["view"]["model"], "glm-4.6");
    assert_eq!(saw["view"]["params"]["model"], "glm-4.6");
    // 没改模型名：规则的改写照常落到请求体上
    let sent = up.seen.lock().unwrap()[0].1.clone();
    assert_eq!(sent["model"], "glm-4.6");
    assert_eq!(sent["system"][0]["text"], "short");

    // 插件换了名字：发出去的是插件的，不是规则的
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let rename = Double::new("rename")
        .permit(&[Permission::Params])
        .on_request(|mut view, _| {
            view["params"]["model"] = json!("glm-5");
            Invocation::ok(RequestOutcome::Changed(view))
        });
    let gw = gateway_of(
        Config {
            providers: vec![provider("a", base, Protocol::Anthropic)],
            routes: by_model(Some("glm-4.6")),
            ..config()
        },
        vec![entry("rename", rename)],
    )
    .await;
    let (status, _) = post(&gw, "/v1/messages", &anthropic_body()).await;
    assert_eq!(status, 200);
    assert_eq!(up.seen.lock().unwrap()[0].1["model"], "glm-5");
}

#[tokio::test]
async fn a_rejection_is_answered_in_the_clients_format_and_nothing_goes_upstream() {
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let no = Double::new("gate")
        .permit(&[Permission::Messages])
        .on_request(|_, _| Invocation::ok(RequestOutcome::Rejected("not today".into())));
    let gw = gateway(
        vec![provider("a", base, Protocol::Anthropic)],
        SecurityMode::Off,
        vec![entry("gate", no)],
    )
    .await;
    let rx = gw.state.bus.subscribe();
    let (status, body) = post(&gw, "/v1/messages", &anthropic_body()).await;
    assert_eq!(status, 403);
    assert_eq!(body["error"]["type"], "permission_error");
    assert_eq!(
        body["error"]["message"],
        "[ThinkWatch] Plugin `Plugin gate` refused this request: not today"
    );
    assert!(up.seen.lock().unwrap().is_empty());
    // 照样有一行：开始、失败，码是插件的
    let mut rx = rx;
    let mut failed = None;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
        if let tw_api::Event::RequestFailed { message, .. } = ev {
            failed = Some(message.code);
        }
    }
    assert_eq!(failed.as_deref(), Some("gw.plugin.rejected"));
    assert_eq!(gw.stats("gate").rejected, 1);
    // Chat 客户端收到的是 Chat 的错误形状
    let (status, body) = post(
        &gw,
        "/v1/chat/completions",
        &json!({ "model": "gpt-5", "messages": [{ "role": "user", "content": "hi" }] }),
    )
    .await;
    assert_eq!(status, 403);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not today")
    );
}

#[tokio::test]
async fn a_failure_follows_on_error() {
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let broken = || {
        Double::new("broken")
            .permit(&[Permission::Messages])
            .on_request(|_, _| {
                Invocation::err(RunError::Threw {
                    message: "TypeError: x is undefined".into(),
                    stack: None,
                })
            })
    };
    let gw = gateway(
        vec![provider("a", base, Protocol::Anthropic)],
        SecurityMode::Off,
        vec![entry("broken", broken())],
    )
    .await;
    let mut events = gw.state.bus.subscribe();
    let (status, body) = post(&gw, "/v1/messages", &anthropic_body()).await;
    assert_eq!(status, 403);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("failed, so the request was not sent: The plugin threw an error: TypeError"),
        "{body}"
    );
    assert!(up.seen.lock().unwrap().is_empty());
    let mut failed = None;
    while let Ok(ev) = events.try_recv() {
        if let tw_api::Event::PluginFailed {
            plugin_id, message, ..
        } = ev
        {
            failed = Some((plugin_id, message.code));
        }
    }
    assert_eq!(
        failed,
        Some(("broken".to_string(), "gw.plugin.threw".to_string()))
    );
    assert_eq!(gw.stats("broken").errors, 1);

    // 跳过：请求照常，原样发出
    let e = entry_with("broken", broken(), |a| a.on_error = OnError::Skip);
    let gw = gateway(
        vec![provider("a", base, Protocol::Anthropic)],
        SecurityMode::Off,
        vec![e, entry("add-date", add_date())],
    )
    .await;
    let (status, _) = post(&gw, "/v1/messages", &anthropic_body()).await;
    assert_eq!(status, 200);
    assert_eq!(gw.stats("broken").errors, 1);
    // 后面那个插件照样跑
    assert_eq!(gw.stats("add-date").changed, 1);
}

#[tokio::test]
async fn a_rule_breaking_answer_is_a_failure() {
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    // 只给了 system，却交回了改过的消息
    let sneaky = Double::new("sneaky")
        .permit(&[Permission::System])
        .on_request(|mut view, _| {
            view["messages"] =
                json!([{ "role": "user", "parts": [{ "type": "text", "text": "x" }] }]);
            Invocation::ok(RequestOutcome::Changed(view))
        });
    let gw = gateway(
        vec![provider("a", base, Protocol::Anthropic)],
        SecurityMode::Off,
        vec![entry("sneaky", sneaky)],
    )
    .await;
    let (status, body) = post(&gw, "/v1/messages", &anthropic_body()).await;
    assert_eq!(status, 403);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("has no permission to change"),
        "{body}"
    );
    assert_eq!(gw.stats("sneaky").errors, 1);
}

#[tokio::test]
async fn an_inactive_plugin_follows_on_error_without_running() {
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let changed = |on_error| {
        let mut a = double::active("old", Double::new("Old"));
        a.on_error = on_error;
        a.state = PluginState::Broken(Broken::Changed);
        Arc::new(a)
    };
    let gw = gateway(
        vec![provider("a", base, Protocol::Anthropic)],
        SecurityMode::Off,
        vec![changed(OnError::Reject)],
    )
    .await;
    let (status, body) = post(&gw, "/v1/messages", &anthropic_body()).await;
    assert_eq!(status, 403);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("changed on disk")
    );
    assert!(up.seen.lock().unwrap().is_empty());

    let gw = gateway(
        vec![provider("a", base, Protocol::Anthropic)],
        SecurityMode::Off,
        vec![changed(OnError::Skip)],
    )
    .await;
    let (status, _) = post(&gw, "/v1/messages", &anthropic_body()).await;
    assert_eq!(status, 200);
    // 没跑：跳过不算一次调用
    assert_eq!(gw.stats("old").calls, 0);
}

#[tokio::test]
async fn out_of_scope_plugins_do_not_run_and_count_tokens_is_left_alone() {
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counting = |c: Arc<AtomicUsize>| {
        Double::new("count")
            .permit(&[Permission::System])
            .on_request(move |_, _| {
                c.fetch_add(1, Ordering::SeqCst);
                Invocation::ok(RequestOutcome::Unchanged)
            })
    };
    let other_models = entry_with("other-models", counting(calls.clone()), |a| {
        a.scope = Scope {
            clients: vec![],
            models: vec!["gpt-*".into()],
            upstreams: vec![],
        }
    });
    let other_apps = entry_with("other-apps", counting(calls.clone()), |a| {
        a.scope.clients = vec!["codex".into()];
    });
    let gw = gateway(
        vec![provider("a", base, Protocol::Anthropic)],
        SecurityMode::Off,
        vec![other_models, other_apps],
    )
    .await;
    let (status, _) = post(&gw, "/v1/messages", &anthropic_body()).await;
    assert_eq!(status, 200);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    // 数 token 不是一次回答：范围内的插件也不跑
    let everyone = entry("everyone", counting(calls.clone()));
    let gw = gateway(
        vec![provider("a", base, Protocol::Anthropic)],
        SecurityMode::Off,
        vec![everyone],
    )
    .await;
    let _ = post(&gw, "/v1/messages/count_tokens", &anthropic_body()).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    // 生成回答的请求照常跑
    let (status, _) = post(&gw, "/v1/messages", &anthropic_body()).await;
    assert_eq!(status, 200);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// 把这一次的上游写进系统提示的插件，数自己跑了几次
fn tag(calls: Arc<AtomicUsize>) -> Double {
    Double::new("tag")
        .permit(&[Permission::System])
        .on_request(move |mut view, ctx| {
            calls.fetch_add(1, Ordering::SeqCst);
            // 跑在插件线程上，不在 tokio 的线程上
            assert!(
                std::thread::current()
                    .name()
                    .unwrap_or_default()
                    .starts_with("tw-plugin-")
            );
            let s = view["system"].as_str().unwrap().to_string();
            view["system"] = json!(format!(
                "{s}\n\n[for {}]",
                ctx["upstream"].as_str().unwrap()
            ));
            Invocation::ok(RequestOutcome::Changed(view))
        })
}

/// a 先回 500，b 接下：两家各一个假上游
async fn a_fails_then_b() -> (Upstream, Upstream, Vec<Provider>) {
    let a = Upstream::failing(1);
    let b = Upstream::default();
    let providers = vec![
        provider("a", start_upstream(a.clone()).await, Protocol::Anthropic),
        provider("b", start_upstream(b.clone()).await, Protocol::Anthropic),
    ];
    (a, b, providers)
}

/// 故障转移从客户端的原话重来：插件每一跳跑一次，给 a 的改动到不了 b。存下来的「插件
/// 改过的请求」是回答的那一家（b）收到的那一份
#[tokio::test]
async fn failing_over_starts_again_from_the_clients_original_request() {
    let (a, b, providers) = a_fails_then_b().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut gw = gateway(
        providers,
        SecurityMode::Off,
        vec![entry("tag", tag(calls.clone()))],
    )
    .await;
    let (status, _) = post(&gw, "/v1/messages", &anthropic_body()).await;
    assert_eq!(status, 200);
    assert_eq!((a.hits(), b.hits()), (1, 1), "one failure, one success");
    assert!(a.system(0).contains("[for a]"), "{}", a.system(0));
    let to_b = b.system(0);
    assert!(to_b.contains("[for b]"), "{to_b}");
    assert!(!to_b.contains("[for a]"), "a's edit reached b: {to_b}");
    // 缓存断点还在原来那一块上
    assert_eq!(
        b.seen.lock().unwrap()[0].1["system"][0]["cache_control"],
        json!({ "type": "ephemeral" })
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    // 每一跳一条，带着第几跳
    assert_eq!(
        gw.runs(),
        [
            ("tag".to_string(), "changed".to_string(), 0),
            ("tag".to_string(), "changed".to_string(), 1)
        ]
    );
    let after = gw.after_plugins().await.expect("the body after plugins");
    assert!(after["system"].to_string().contains("[for b]"), "{after}");
    assert!(!after["system"].to_string().contains("[for a]"), "{after}");
}

/// 只管 a 的插件只在发往 a 时跑；发往 b 的是客户端的原话。反过来也一样
#[tokio::test]
async fn a_plugin_scoped_to_one_upstream_runs_only_for_it() {
    for only in ["a", "b"] {
        let (a, b, providers) = a_fails_then_b().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut gw = gateway(
            providers,
            SecurityMode::Off,
            vec![entry_with("tag", tag(calls.clone()), |e| {
                e.scope.upstreams = vec![only.into()]
            })],
        )
        .await;
        let (status, _) = post(&gw, "/v1/messages", &anthropic_body()).await;
        assert_eq!(status, 200, "{only}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "{only}");
        let (tagged, untouched) = if only == "a" { (&a, &b) } else { (&b, &a) };
        assert!(
            tagged.system(0).contains(&format!("[for {only}]")),
            "{only}: {}",
            tagged.system(0)
        );
        assert_eq!(
            untouched.seen.lock().unwrap()[0].1,
            anthropic_body(),
            "{only}: the plugin ran for an upstream outside its scope"
        );
        let attempt = if only == "a" { 0 } else { 1 };
        assert_eq!(
            gw.runs(),
            [("tag".to_string(), "changed".to_string(), attempt)]
        );
        // 回答的那一家收到的没被改过，就没有「插件改过的请求」
        let after = gw.after_plugins().await;
        if only == "a" {
            assert_eq!(after, None, "b got the original");
        } else {
            assert!(after.is_some());
        }
    }
}

/// 坏了的插件只拦管得着的那一跳：它只管 a，请求只去 b 时照常；要发往它管的那一家时
/// 拒绝整个请求（不换下一家），发往别家的那一跳照常发过
#[tokio::test]
async fn a_broken_plugin_rejects_only_attempts_in_its_scope() {
    let broken = |upstream: &str| {
        let mut a = double::active("old", Double::new("Old"));
        a.state = PluginState::Broken(Broken::Changed);
        a.scope.upstreams = vec![upstream.into()];
        Arc::new(a)
    };
    // 只去 b：管 a 的坏插件不拦它
    let b = Upstream::default();
    let gw = gateway(
        vec![provider(
            "b",
            start_upstream(b.clone()).await,
            Protocol::Anthropic,
        )],
        SecurityMode::Off,
        vec![broken("a")],
    )
    .await;
    let (status, body) = post(&gw, "/v1/messages", &anthropic_body()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(b.hits(), 1);
    assert!(gw.runs().is_empty(), "{:?}", gw.runs());

    // a 失败、换到 b：管 b 的坏插件拦下整个请求，b 一个字节都没收到
    let (a, b, providers) = a_fails_then_b().await;
    let gw = gateway(providers, SecurityMode::Off, vec![broken("b")]).await;
    let rx = gw.state.bus.subscribe();
    let (status, body) = post(&gw, "/v1/messages", &anthropic_body()).await;
    assert_eq!(status, 403, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("changed on disk"),
        "{body}"
    );
    assert_eq!((a.hits(), b.hits()), (1, 0));
    assert_eq!(gw.runs(), [("old".to_string(), "error".to_string(), 1)]);
    // 尝试链上：a 回了 500，b 被插件拦下
    let attempts = routed(rx).await;
    assert_eq!(attempts.len(), 2, "{attempts:?}");
    assert_eq!(
        (attempts[0].provider.as_str(), attempts[0].status),
        ("a", Some(500))
    );
    assert_eq!(attempts[1].provider, "b");
    assert_eq!(
        attempts[1].error.as_ref().map(|e| e.code.as_str()),
        Some("gw.plugin.changed")
    );
}

/// 插件出错（策略是拒绝）、插件 `reject`：拒绝的是整个请求，不换到下一家
#[tokio::test]
async fn a_plugin_refusal_rejects_the_whole_request_without_failing_over() {
    let only_a = |rejects: bool| {
        Double::new("picky")
            .permit(&[Permission::System])
            .on_request(move |_, ctx| {
                if ctx["upstream"] != "a" {
                    return Invocation::ok(RequestOutcome::Unchanged);
                }
                if rejects {
                    Invocation::ok(RequestOutcome::Rejected("not for a".into()))
                } else {
                    Invocation::err(RunError::Threw {
                        message: "only on a".into(),
                        stack: None,
                    })
                }
            })
    };
    for (rejects, code) in [
        (true, "gw.plugin.rejected"),
        (false, "gw.plugin.request_failed"),
    ] {
        let a = Upstream::default();
        let b = Upstream::default();
        let gw = gateway(
            vec![
                provider("a", start_upstream(a.clone()).await, Protocol::Anthropic),
                provider("b", start_upstream(b.clone()).await, Protocol::Anthropic),
            ],
            SecurityMode::Off,
            vec![entry("picky", only_a(rejects))],
        )
        .await;
        let rx = gw.state.bus.subscribe();
        let (status, body) = post(&gw, "/v1/messages", &anthropic_body()).await;
        assert_eq!(status, 403, "{rejects}: {body}");
        assert_eq!((a.hits(), b.hits()), (0, 0), "{rejects}");
        let attempts = routed(rx).await;
        assert_eq!(attempts.len(), 1, "{rejects}: {attempts:?}");
        assert_eq!(attempts[0].provider, "a");
        assert_eq!(
            attempts[0].error.as_ref().map(|e| e.code.as_str()),
            Some(code),
            "{rejects}"
        );
    }
}

/// 这个请求的尝试链（路由事件里的）
async fn routed(
    mut rx: tokio::sync::broadcast::Receiver<tw_api::Event>,
) -> Vec<tw_api::AttemptView> {
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
        if let tw_api::Event::RequestRouted { attempts, .. } = ev {
            return attempts;
        }
    }
    panic!("no routing event");
}

/// OAuth 上游回 401：换一个 token、向同一家再发一次。**用这一跳定好的请求体，插件不重跑**
#[tokio::test]
async fn a_same_upstream_oauth_retry_does_not_run_the_hook_again() {
    // token 端点：每次换发一个新的（at-1、at-2……）
    let issued = Arc::new(AtomicUsize::new(0));
    let i = issued.clone();
    let tokens = Router::new().route(
        "/token",
        axum::routing::post(move || {
            let n = i.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                axum::Json(json!({
                    "access_token": format!("at-{n}"), "token_type": "Bearer", "expires_in": 3600
                }))
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token_addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, tokens).await.unwrap() });
    // 上游：配置里那个 token（at-0）在别处被吊销了，回 401；换来的照常回答
    let bodies: Arc<Mutex<Vec<Bytes>>> = Arc::default();
    let seen = bodies.clone();
    let app = Router::new().fallback(axum::routing::post(
        move |headers: axum::http::HeaderMap, body: Bytes| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(body);
                // Anthropic 的上游，token 放在 x-api-key 里
                let first = ["x-api-key", "authorization"].iter().any(|h| {
                    headers
                        .get(*h)
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| v.ends_with("at-0"))
                });
                let (status, reply) = if first {
                    (401, json!({ "type": "error", "error": { "type": "authentication_error", "message": "revoked" } }))
                } else {
                    (200, json!({ "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
                                  "content": [{ "type": "text", "text": "ok" }], "stop_reason": "end_turn",
                                  "usage": { "input_tokens": 1, "output_tokens": 1 } }))
                };
                axum::response::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(reply.to_string()))
                    .unwrap()
            }
        },
    ));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let oauth = Provider {
        name: "a".into(),
        base_url: format!("http://{up_addr}"),
        oauth: Some(tw_config::OAuth {
            access: Some("at-0".into()),
            expires_at: None,
            refresh: "rt-test".into(),
            endpoint: format!("http://{token_addr}/token"),
            client_id: Some("tw-test".into()),
            client_secret: None,
            refresh_before: Some("5m".into()),
        }),
        protocol: Some(Protocol::Anthropic),
        ..Default::default()
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let gw = gateway(
        vec![oauth],
        SecurityMode::Off,
        vec![entry("tag", tag(calls.clone()))],
    )
    .await;
    let (status, body) = post(&gw, "/v1/messages", &anthropic_body()).await;
    assert_eq!(status, 200, "{body}");
    let bodies = bodies.lock().unwrap().clone();
    assert_eq!(
        bodies.len(),
        2,
        "refused, then sent again with a fresh token"
    );
    assert_eq!(bodies[0], bodies[1], "the retry was not the same request");
    assert!(String::from_utf8_lossy(&bodies[1]).contains("[for a]"));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the hook ran again for the retry"
    );
    assert_eq!(issued.load(Ordering::SeqCst), 1);
}

/// 插件看到的是占位符，和脱敏开在哪一档无关；它改过的地方占位符换回真值，然后才轮到
/// 出站脱敏按档位决定上游看到什么
#[tokio::test]
async fn plugins_see_placeholders_whatever_the_redaction_mode() {
    for mode in [
        SecurityMode::Enforce,
        SecurityMode::Observe,
        SecurityMode::Off,
    ] {
        let up = Upstream::default();
        let base = start_upstream(up.clone()).await;
        let saw = Arc::new(Mutex::new(String::new()));
        let s = saw.clone();
        let echo = Double::new("echo")
            .permit(&[Permission::Messages])
            .on_request(move |mut view, _| {
                *s.lock().unwrap() = view.to_string();
                let t = view["messages"][0]["parts"][0]["text"]
                    .as_str()
                    .unwrap()
                    .to_string();
                view["messages"][0]["parts"][0]["text"] = json!(format!("{t} (checked)"));
                Invocation::ok(RequestOutcome::Changed(view))
            });
        let mut gw = gateway(
            vec![provider("a", base, Protocol::Anthropic)],
            mode,
            vec![entry("echo", echo)],
        )
        .await;
        let body = json!({
            "model": "claude-sonnet-4-5", "max_tokens": 8,
            "messages": [{ "role": "user", "content": format!("my key is {USER_KEY}") }]
        });
        let (status, _) = post(&gw, "/v1/messages", &body).await;
        assert_eq!(status, 200, "{mode:?}");
        // 插件改过的那一份落盘时和别的请求体一样换掉、打码
        let after = gw
            .bodies()
            .await
            .into_iter()
            .find(|b| b.kind == tw_gateway::bodies::BodyKind::AfterPlugins)
            .expect("the body after plugins");
        let disk = String::from_utf8(after.for_disk().body.to_vec()).unwrap();
        assert!(!disk.contains(USER_KEY), "{mode:?}: {disk}");
        let saw = saw.lock().unwrap().clone();
        assert!(
            !saw.contains(USER_KEY),
            "{mode:?}: the plugin saw the key: {saw}"
        );
        assert!(saw.contains("<<TW_SECRET_1>>"), "{mode:?}: {saw}");
        let sent = up.seen.lock().unwrap()[0].1["messages"][0]["content"]
            .as_str()
            .unwrap()
            .to_string();
        match mode {
            SecurityMode::Enforce => {
                assert_eq!(sent, "my key is <<TW_SECRET_1>> (checked)", "{mode:?}")
            }
            _ => assert_eq!(sent, format!("my key is {USER_KEY} (checked)"), "{mode:?}"),
        }
    }
}

/// 插件看到的号就是上游看到的号：插件删掉了带 1 号的那条消息，剩下的那把还是 2 号，
/// 不会因为改过的请求里它排到了第一个就重新编成 1 号
#[tokio::test]
async fn the_numbers_a_plugin_sees_are_the_numbers_the_upstream_gets() {
    const OTHER: &str = "ghp_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let saw = Arc::new(Mutex::new(String::new()));
    let s = saw.clone();
    let drop_first = Double::new("drop first")
        .permit(&[Permission::Messages])
        .on_request(move |mut view, _| {
            *s.lock().unwrap() = view.to_string();
            view["messages"].as_array_mut().unwrap().remove(0);
            Invocation::ok(RequestOutcome::Changed(view))
        });
    let gw = gateway(
        vec![provider("a", base, Protocol::Anthropic)],
        SecurityMode::Enforce,
        vec![entry("drop-first", drop_first)],
    )
    .await;
    let body = json!({
        "model": "claude-sonnet-4-5", "max_tokens": 8,
        "messages": [
            { "role": "user", "content": format!("first {USER_KEY}") },
            { "role": "assistant", "content": "ok" },
            { "role": "user", "content": format!("second {OTHER}") }
        ]
    });
    let (status, _) = post(&gw, "/v1/messages", &body).await;
    assert_eq!(status, 200);
    let saw = saw.lock().unwrap().clone();
    assert!(saw.contains("second <<TW_SECRET_2>>"), "{saw}");
    let sent = up.seen.lock().unwrap()[0].1.clone();
    assert_eq!(sent["messages"][1]["content"], "second <<TW_SECRET_2>>");
}

#[tokio::test]
async fn screening_sees_the_body_after_plugins() {
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let security = Security {
        content: tw_config::ContentPolicy {
            mode: SecurityMode::Enforce,
            custom: vec![tw_config::CustomContentRule {
                name: "no plan".into(),
                pattern: "forbidden-plan".into(),
                matching: Default::default(),
                action: tw_config::ContentAction::Block,
                disabled: false,
            }],
            ..Default::default()
        },
        ..Default::default()
    };
    let inject = Double::new("inject")
        .permit(&[Permission::Messages])
        .on_request(|mut view, _| {
            view["messages"][0]["parts"][0]["text"] = json!("the forbidden-plan");
            Invocation::ok(RequestOutcome::Changed(view))
        });
    let gw = gateway_with(
        vec![provider("a", base, Protocol::Anthropic)],
        SecurityMode::Off,
        vec![entry("inject", inject)],
        security,
    )
    .await;
    let rx = gw.state.bus.subscribe();
    let (status, body) = post(&gw, "/v1/messages", &anthropic_body()).await;
    assert_eq!(status, 403, "{body}");
    assert!(up.seen.lock().unwrap().is_empty());
    // 拦在这一跳上：尝试链上看得出本来要发给谁、为什么没发
    let attempts = routed(rx).await;
    assert_eq!(attempts.len(), 1, "{attempts:?}");
    assert_eq!(
        attempts[0].error.as_ref().map(|e| e.code.as_str()),
        Some("gw.content.refused")
    );
}

/// 插件改过的请求再看一遍时，只报插件加进来的：客户端原话里就有的那一处命中（观察档）
/// 已经在开头报过，不因为插件改了系统提示再报一次。插件写进来的新密钥报一条，记在这一跳
/// 的上游上
#[tokio::test]
async fn after_a_plugin_only_what_it_added_is_reported_again() {
    const WRITTEN: &str = "ghp_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let security = Security {
        content: tw_config::ContentPolicy {
            mode: SecurityMode::Observe,
            custom: vec![tw_config::CustomContentRule {
                name: "no plan".into(),
                pattern: "forbidden-plan".into(),
                matching: Default::default(),
                action: tw_config::ContentAction::Block,
                disabled: false,
            }],
            ..Default::default()
        },
        ..Default::default()
    };
    let note = Double::new("note")
        .permit(&[Permission::System])
        .on_request(|mut view, _| {
            let s = view["system"].as_str().unwrap().to_string();
            view["system"] = json!(format!("{s}\n\nCI token: {WRITTEN}"));
            Invocation::ok(RequestOutcome::Changed(view))
        });
    let gw = gateway_with(
        vec![provider("a", base, Protocol::Anthropic)],
        SecurityMode::Observe,
        vec![entry("note", note)],
        security,
    )
    .await;
    let mut rx = gw.state.bus.subscribe();
    let mut body = anthropic_body();
    body["messages"][0]["content"] = json!(format!("the forbidden-plan, key {USER_KEY}"));
    let (status, answer) = post(&gw, "/v1/messages", &body).await;
    assert_eq!(status, 200, "{answer}");
    let (mut matched, mut secrets) = (0, Vec::new());
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
        match ev {
            tw_api::Event::ContentMatched { .. } => matched += 1,
            tw_api::Event::SecretsFound {
                provider, items, ..
            } => secrets.push((provider, items.len(), items[0].masked.clone())),
            _ => {}
        }
    }
    assert_eq!(matched, 1, "the client's own match was reported again");
    assert_eq!(secrets.len(), 2, "{secrets:?}");
    // 开头那一条是客户端的那把；插件写进来的那一把另报一条，只有它
    assert!(secrets[0].2.starts_with("sk-an"), "{secrets:?}");
    assert_eq!(secrets[1].0, "a");
    assert_eq!(secrets[1].1, 1, "{secrets:?}");
    assert!(secrets[1].2.starts_with("ghp_"), "{secrets:?}");
}

#[tokio::test]
async fn each_client_format_is_rewritten_in_its_own_shape() {
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let shout = Double::new("shout")
        .permit(&[Permission::Messages, Permission::Params])
        .on_request(|mut view, ctx| {
            let last = view["messages"].as_array().unwrap().len() - 1;
            let t = view["messages"][last]["parts"][0]["text"]
                .as_str()
                .unwrap()
                .to_uppercase();
            view["messages"][last]["parts"][0]["text"] = json!(t);
            if ctx["format"] == "gemini" {
                view["params"]["model"] = json!("gemini-2.5-flash");
            }
            Invocation::ok(RequestOutcome::Changed(view))
        });
    let cases = [
        (
            "/v1/chat/completions",
            json!({ "model": "gpt-5", "messages": [{ "role": "system", "content": "s" }, { "role": "user", "content": "hello" }] }),
        ),
        (
            "/v1/responses",
            json!({ "model": "gpt-5", "input": [{ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "hello" }] }] }),
        ),
        (
            "/v1beta/models/gemini-2.5-pro:generateContent",
            json!({ "contents": [{ "role": "user", "parts": [{ "text": "hello" }] }] }),
        ),
    ];
    for (path, body) in cases {
        // 每种格式一家同格式的上游：直通，原样看得到写回的结果
        let protocol = match path {
            "/v1/chat/completions" => Protocol::OpenaiChat,
            "/v1/responses" => Protocol::OpenaiResponses,
            _ => Protocol::Gemini,
        };
        let gw = gateway(
            vec![provider("same", base, protocol)],
            SecurityMode::Off,
            vec![entry("shout", shout.clone())],
        )
        .await;
        up.seen.lock().unwrap().clear();
        let (status, answer) = post(&gw, path, &body).await;
        assert_eq!(status, 200, "{path}: {answer}");
        let (got_path, sent) = up.seen.lock().unwrap()[0].clone();
        let text = match path {
            "/v1/chat/completions" => sent["messages"][1]["content"].clone(),
            "/v1/responses" => sent["input"][0]["content"][0]["text"].clone(),
            _ => sent["contents"][0]["parts"][0]["text"].clone(),
        };
        assert_eq!(text, "HELLO", "{path}: {sent}");
        if path.contains("gemini") {
            // 换了模型就是换了路径
            assert_eq!(got_path, "/v1beta/models/gemini-2.5-flash:generateContent");
        }
    }
}
