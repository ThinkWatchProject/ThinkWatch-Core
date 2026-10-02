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
async fn out_of_scope_plugins_do_not_run_and_count_tokens_runs_the_ones_in_scope() {
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

    // 数 token 发往上游的也是一个请求体：范围内的插件照样跑，范围外的照样不跑
    let everyone = entry("everyone", counting(calls.clone()));
    let gw = gateway(
        vec![provider("a", base, Protocol::Anthropic)],
        SecurityMode::Off,
        vec![everyone],
    )
    .await;
    let (status, _) = post(&gw, "/v1/messages/count_tokens", &anthropic_body()).await;
    assert_eq!(status, 200);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let (status, _) = post(&gw, "/v1/messages", &anthropic_body()).await;
    assert_eq!(status, 200);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
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

// ───────────────────────────────────────── 不生成回答的接口

/// 插件要删掉的东西
const MARK: &str = "SECRET-PROJECT";

/// 把 [`MARK`] 从系统提示、消息文字和工具结果里删掉的插件
fn scrub() -> Double {
    Double::new("scrub")
        .permit(&[Permission::System, Permission::Messages])
        .on_request(|mut view, _| {
            let s = view["system"].as_str().unwrap().replace(MARK, "[removed]");
            view["system"] = json!(s);
            for m in view["messages"].as_array_mut().unwrap() {
                for p in m["parts"].as_array_mut().unwrap() {
                    if (p["type"] == "text" || p["type"] == "tool_result")
                        && let Some(t) = p["text"].as_str()
                    {
                        p["text"] = json!(t.replace(MARK, "[removed]"));
                    }
                }
            }
            Invocation::ok(RequestOutcome::Changed(view))
        })
}

fn anthropic_count_body() -> Value {
    json!({
        "model": "claude-sonnet-4-5",
        "system": format!("About {MARK}."),
        "messages": [
            { "role": "user", "content": format!("Plan {MARK}") },
            { "role": "assistant", "content": [{ "type": "tool_use", "id": "t1", "name": "Read", "input": { "path": "a" } }] },
            { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": format!("{MARK} notes") }] }
        ]
    })
}

fn responses_body() -> Value {
    json!({
        "model": "gpt-5",
        "instructions": format!("About {MARK}."),
        "input": [{ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": format!("Plan {MARK}") }] }]
    })
}

const GEMINI_COUNT: &str = "/v1beta/models/gemini-2.5-pro:countTokens";

/// 数 token、Responses 的压缩：请求体就是一段对话，插件照样改，**上游数的、压的是改过的那
/// 一份** —— 插件删掉的东西不从这些接口漏出去。每种客户端格式、Gemini 的两种写法都一样
#[tokio::test]
async fn token_counts_and_compactions_reach_the_upstream_as_the_plugins_left_them() {
    let cases = [
        (
            "/v1/messages/count_tokens",
            Protocol::Anthropic,
            anthropic_count_body(),
        ),
        (
            GEMINI_COUNT,
            Protocol::Gemini,
            json!({ "contents": [{ "role": "user", "parts": [{ "text": format!("Plan {MARK}") }] }] }),
        ),
        (
            GEMINI_COUNT,
            Protocol::Gemini,
            json!({ "generateContentRequest": {
                "model": "models/gemini-2.5-pro",
                "systemInstruction": { "parts": [{ "text": format!("About {MARK}.") }] },
                "contents": [{ "role": "user", "parts": [{ "text": format!("Plan {MARK}") }] }]
            } }),
        ),
        (
            "/v1/responses/compact",
            Protocol::OpenaiResponses,
            responses_body(),
        ),
        (
            "/v1/responses/input_tokens",
            Protocol::OpenaiResponses,
            responses_body(),
        ),
        (
            "/backend-api/codex/responses/compact",
            Protocol::OpenaiResponses,
            responses_body(),
        ),
    ];
    for (path, protocol, body) in cases {
        let up = Upstream::default();
        let base = start_upstream(up.clone()).await;
        let mut gw = gateway(
            vec![provider("same", base, protocol)],
            SecurityMode::Off,
            vec![entry("scrub", scrub())],
        )
        .await;
        let (status, answer) = post(&gw, path, &body).await;
        assert_eq!(status, 200, "{path}: {answer}");
        assert_eq!(up.hits(), 1, "{path}");
        let (got_path, _) = up.seen.lock().unwrap()[0].clone();
        assert_eq!(got_path, path);
        let raw = String::from_utf8(up.raw.lock().unwrap()[0].to_vec()).unwrap();
        assert!(!raw.contains(MARK), "{path}: the upstream got {raw}");
        assert!(raw.contains("[removed]"), "{path}: {raw}");
        // 和生成回答一样记在请求上：跑在第几跳、改了什么，改过的那一份另存
        assert_eq!(
            gw.runs(),
            [("scrub".to_string(), "changed".to_string(), 0)],
            "{path}"
        );
        let after = gw.after_plugins().await.expect("the body after plugins");
        assert!(!after.to_string().contains(MARK), "{path}: {after}");
        // 写法照原样：包着的还包着，没包着的没加系统提示也不包
        if path == GEMINI_COUNT {
            let sent: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(
                sent.get("generateContentRequest").is_some(),
                body.get("generateContentRequest").is_some(),
                "{sent}"
            );
        }
    }
}

/// 数 token 上的插件和生成回答上的一样：同样按客户端、发出去的模型、上游挑，同样只看到
/// 占位符，出错、`reject` 同样按 `on_error` 拒掉整个请求
#[tokio::test]
async fn counting_follows_the_same_scope_placeholders_and_on_error() {
    const COUNT: &str = "/v1/messages/count_tokens";
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let providers = || vec![provider("a", base, Protocol::Anthropic)];

    // 范围外：别的模型、别的上游的插件不跑，上游收到的是原话
    for scoped in [
        entry_with("scrub", scrub(), |a| a.scope.models = vec!["gpt-*".into()]),
        entry_with("scrub", scrub(), |a| a.scope.upstreams = vec!["b".into()]),
    ] {
        up.raw.lock().unwrap().clear();
        let gw = gateway(providers(), SecurityMode::Off, vec![scoped]).await;
        let (status, _) = post(&gw, COUNT, &anthropic_count_body()).await;
        assert_eq!(status, 200);
        assert!(String::from_utf8_lossy(&up.raw.lock().unwrap()[0]).contains(MARK));
        assert!(gw.runs().is_empty(), "{:?}", gw.runs());
    }

    // 占位符：插件看不到真的密钥，改过的地方换回去再发
    let saw = Arc::new(Mutex::new(String::new()));
    let s = saw.clone();
    let checked = Double::new("checked")
        .permit(&[Permission::Messages])
        .on_request(move |mut view, ctx| {
            *s.lock().unwrap() = view.to_string();
            assert_eq!(ctx["upstream"], "a");
            assert_eq!(ctx["model"], "claude-sonnet-4-5");
            let t = view["messages"][0]["parts"][0]["text"]
                .as_str()
                .unwrap()
                .to_string();
            view["messages"][0]["parts"][0]["text"] = json!(format!("{t} (checked)"));
            Invocation::ok(RequestOutcome::Changed(view))
        });
    up.raw.lock().unwrap().clear();
    let gw = gateway(
        providers(),
        SecurityMode::Off,
        vec![entry("checked", checked)],
    )
    .await;
    let body = json!({ "model": "claude-sonnet-4-5",
                       "messages": [{ "role": "user", "content": format!("my key is {USER_KEY}") }] });
    let (status, _) = post(&gw, COUNT, &body).await;
    assert_eq!(status, 200);
    let saw = saw.lock().unwrap().clone();
    assert!(!saw.contains(USER_KEY), "the plugin saw the key: {saw}");
    assert!(saw.contains("<<TW_SECRET_1>>"), "{saw}");
    let sent: Value = serde_json::from_slice(&up.raw.lock().unwrap()[0]).unwrap();
    assert_eq!(
        sent["messages"][0]["content"],
        format!("my key is {USER_KEY} (checked)")
    );

    // 出错、拒绝：拒绝时整个请求不发，跳过时原样发
    let failing = || {
        Double::new("failing")
            .permit(&[Permission::Messages])
            .on_request(|_, _| {
                Invocation::err(RunError::Threw {
                    message: "nope".into(),
                    stack: None,
                })
            })
    };
    let refusing = Double::new("refusing")
        .permit(&[Permission::Messages])
        .on_request(|_, _| Invocation::ok(RequestOutcome::Rejected("not counted".into())));
    for (e, code) in [
        (entry("failing", failing()), "gw.plugin.request_failed"),
        (entry("refusing", refusing), "gw.plugin.rejected"),
    ] {
        up.raw.lock().unwrap().clear();
        let gw = gateway(providers(), SecurityMode::Off, vec![e]).await;
        let rx = gw.state.bus.subscribe();
        let (status, body) = post(&gw, COUNT, &anthropic_count_body()).await;
        assert_eq!(status, 403, "{code}: {body}");
        assert!(up.raw.lock().unwrap().is_empty(), "{code}");
        let attempts = routed(rx).await;
        assert_eq!(
            attempts[0].error.as_ref().map(|e| e.code.as_str()),
            Some(code)
        );
    }
    up.raw.lock().unwrap().clear();
    let gw = gateway(
        providers(),
        SecurityMode::Off,
        vec![entry_with("failing", failing(), |a| {
            a.on_error = OnError::Skip
        })],
    )
    .await;
    let (status, _) = post(&gw, COUNT, &anthropic_count_body()).await;
    assert_eq!(status, 200);
    let sent: Value = serde_json::from_slice(&up.raw.lock().unwrap()[0]).unwrap();
    assert_eq!(sent, anthropic_count_body());
    assert_eq!(gw.stats("failing").errors, 1);
}

/// 网关自己估的数：一个字节都不发给上游，也就不跑插件
#[tokio::test]
async fn a_count_the_gateway_estimates_runs_no_plugin() {
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let gw = gateway(
        vec![provider("chat", base, Protocol::OpenaiChat)],
        SecurityMode::Off,
        vec![entry("tag", tag(calls.clone()))],
    )
    .await;
    let (status, answer) = post(&gw, "/v1/messages/count_tokens", &anthropic_count_body()).await;
    assert_eq!(status, 200, "{answer}");
    assert!(answer["input_tokens"].as_u64().unwrap() > 0, "{answer}");
    assert_eq!(up.hits(), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(gw.runs().is_empty());
}

/// 数 token、压缩不收输出上限、温度这些参数：插件改的只写回模型名。Gemini 没包着的数
/// token 请求，插件加了系统提示就包起来（外面那一层只收 `contents`），模型名跟着插件改
#[tokio::test]
async fn counting_and_compacting_take_only_the_model_from_params() {
    let tune = Double::new("tune")
        .permit(&[Permission::System, Permission::Params])
        .on_request(|mut view, ctx| {
            let s = view["system"].as_str().unwrap().to_string();
            view["system"] = json!(format!("{s} Be brief.").trim().to_string());
            view["params"]["max_tokens"] = json!(99);
            view["params"]["temperature"] = json!(0.1);
            if ctx["format"] == "gemini" {
                view["params"]["model"] = json!("gemini-2.5-flash");
            } else {
                view["params"]["model"] =
                    json!(format!("{}-renamed", ctx["model"].as_str().unwrap()));
            }
            Invocation::ok(RequestOutcome::Changed(view))
        });
    let cases = [
        (
            "/v1/messages/count_tokens",
            Protocol::Anthropic,
            json!({ "model": "claude-sonnet-4-5", "messages": [{ "role": "user", "content": "hi" }] }),
        ),
        (
            "/v1/responses/compact",
            Protocol::OpenaiResponses,
            json!({ "model": "gpt-5", "input": "hi" }),
        ),
        (
            GEMINI_COUNT,
            Protocol::Gemini,
            json!({ "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }] }),
        ),
        (
            GEMINI_COUNT,
            Protocol::Gemini,
            json!({ "generateContentRequest": { "model": "models/gemini-2.5-pro",
                    "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }] } }),
        ),
    ];
    for (path, protocol, body) in cases {
        let up = Upstream::default();
        let base = start_upstream(up.clone()).await;
        let gw = gateway(
            vec![provider("same", base, protocol)],
            SecurityMode::Off,
            vec![entry("tune", tune.clone())],
        )
        .await;
        let (status, answer) = post(&gw, path, &body).await;
        assert_eq!(status, 200, "{path}: {answer}");
        let (got_path, sent) = up.seen.lock().unwrap()[0].clone();
        let text = sent.to_string();
        assert!(text.contains("Be brief."), "{path}: {text}");
        for param in [
            "max_tokens",
            "max_output_tokens",
            "maxOutputTokens",
            "temperature",
            "generationConfig",
        ] {
            assert!(!text.contains(param), "{path}: {param} was sent: {text}");
        }
        match protocol {
            Protocol::Gemini => {
                assert_eq!(got_path, "/v1beta/models/gemini-2.5-flash:countTokens");
                let inner = &sent["generateContentRequest"];
                assert_eq!(inner["model"], "models/gemini-2.5-flash", "{text}");
                assert_eq!(
                    inner["systemInstruction"]["parts"][0]["text"], "Be brief.",
                    "{text}"
                );
                assert_eq!(inner["contents"][0]["parts"][0]["text"], "hi", "{text}");
                assert!(sent.get("contents").is_none(), "{text}");
            }
            _ => assert!(
                sent["model"].as_str().unwrap().ends_with("-renamed"),
                "{path}: {text}"
            ),
        }
    }
}

// ───────────────────────────────────────────────────────── 嵌入、旧版补全

const EMBED_GEMINI: &str = "/v1beta/models/gemini-embedding-001:embedContent";
const EMBED_GEMINI_BATCH: &str = "/v1beta/models/gemini-embedding-001:batchEmbedContents";

fn embeddings_body() -> Value {
    json!({ "model": "text-embedding-3-small", "dimensions": 256,
            "input": [format!("Plan {MARK}"), "unrelated", [9906, 1917]] })
}

fn completions_body() -> Value {
    json!({ "model": "gpt-3.5-turbo-instruct", "max_tokens": 16, "suffix": " end",
            "prompt": [format!("Plan {MARK}"), [9906, 1917], format!("{MARK} notes")] })
}

fn gemini_embed_body() -> Value {
    json!({ "model": "models/gemini-embedding-001", "taskType": "RETRIEVAL_DOCUMENT",
            "content": { "parts": [{ "text": format!("Plan {MARK}") }] } })
}

fn gemini_batch_body() -> Value {
    json!({ "requests": [
        { "model": "models/gemini-embedding-001",
          "content": { "parts": [{ "text": format!("Plan {MARK}") }] } },
        { "model": "models/gemini-embedding-001",
          "content": { "parts": [{ "text": "second" }, { "text": format!("{MARK} notes") }] } }
    ] })
}

/// 四种非对话的请求体，各配一家同格式的上游：(路径, 协议, 请求体)
fn input_cases() -> Vec<(&'static str, Protocol, Value)> {
    vec![
        ("/v1/embeddings", Protocol::OpenaiChat, embeddings_body()),
        ("/v1/completions", Protocol::OpenaiChat, completions_body()),
        (EMBED_GEMINI, Protocol::Gemini, gemini_embed_body()),
        (EMBED_GEMINI_BATCH, Protocol::Gemini, gemini_batch_body()),
    ]
}

/// 把 [`MARK`] 从每项输入的文字里删掉的插件，**声明了嵌入和补全**。`ctx.format` 记下来
fn scrub_inputs(formats: Arc<Mutex<Vec<String>>>) -> Double {
    Double::new("scrub inputs")
        .permit(&[Permission::Messages])
        .requests(&[
            tw_api::RequestKind::Conversation,
            tw_api::RequestKind::Embeddings,
            tw_api::RequestKind::Completions,
        ])
        .on_request(move |mut view, ctx| {
            formats
                .lock()
                .unwrap()
                .push(ctx["format"].as_str().unwrap().to_string());
            assert_eq!(view["format"], ctx["format"]);
            for m in view["messages"].as_array_mut().unwrap() {
                assert_eq!(m["role"], "user");
                for p in m["parts"].as_array_mut().unwrap() {
                    if p["type"] == "text" {
                        let t = p["text"].as_str().unwrap().replace(MARK, "[removed]");
                        p["text"] = json!(t);
                    }
                }
            }
            Invocation::ok(RequestOutcome::Changed(view))
        })
}

/// 声明了嵌入、补全的插件：上游收到的是删过记号的那一份，**除了那几段文字一个字节都
/// 不差**（客户端发来的就是排好序的紧凑 JSON，改过的请求体也是这么写的）。一串 token 原样；
/// 改过的那一份照样存下来，运行照样记在请求上
#[tokio::test]
async fn a_plugin_that_declares_embeddings_and_completions_scrubs_their_inputs() {
    for (path, protocol, body) in input_cases() {
        let up = Upstream::default();
        let base = start_upstream(up.clone()).await;
        let formats = Arc::new(Mutex::new(Vec::new()));
        let mut gw = gateway(
            vec![provider("same", base, protocol)],
            SecurityMode::Off,
            vec![entry("scrub", scrub_inputs(formats.clone()))],
        )
        .await;
        let (status, answer) = post(&gw, path, &body).await;
        assert_eq!(status, 200, "{path}: {answer}");
        assert_eq!(up.hits(), 1, "{path}");
        let (got_path, _) = up.seen.lock().unwrap()[0].clone();
        assert_eq!(got_path, path);
        let raw = String::from_utf8(up.raw.lock().unwrap()[0].to_vec()).unwrap();
        assert_eq!(
            raw,
            body.to_string().replace(MARK, "[removed]"),
            "{path}: only the inputs' text may differ"
        );
        assert_eq!(
            gw.runs(),
            [("scrub".to_string(), "changed".to_string(), 0)],
            "{path}"
        );
        let after = gw.after_plugins().await.expect("the body after plugins");
        assert!(!after.to_string().contains(MARK), "{path}: {after}");
        let want = match path {
            "/v1/embeddings" => "openai_embeddings",
            "/v1/completions" => "openai_completions",
            _ => "gemini_embed",
        };
        assert_eq!(formats.lock().unwrap().as_slice(), [want], "{path}");
    }
}

/// **没声明的那种请求不在插件的范围里**：原样发、什么都不记、不算一次调用 —— 出错时拒绝
/// 也一样。插件一律不管的接口（认不出的、空正文的）也是这样
#[tokio::test]
async fn kinds_a_plugin_did_not_declare_pass_through_unrecorded() {
    for on_error in [OnError::Reject, OnError::Skip] {
        // 只处理对话的插件，跑一次就失败：在范围里的话，拒绝档下请求就被拒了
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let failing = Double::new("failing")
            .permit(&[Permission::Messages])
            .on_request(move |_, _| {
                c.fetch_add(1, Ordering::SeqCst);
                Invocation::err(RunError::Threw {
                    message: "nope".into(),
                    stack: None,
                })
            });
        for (path, protocol, body) in input_cases() {
            let up = Upstream::default();
            let base = start_upstream(up.clone()).await;
            let gw = gateway(
                vec![provider("same", base, protocol)],
                SecurityMode::Off,
                vec![entry_with("failing", failing.clone(), |a| {
                    a.on_error = on_error
                })],
            )
            .await;
            let (status, answer) = post(&gw, path, &body).await;
            assert_eq!(status, 200, "{on_error:?} {path}: {answer}");
            // 原样：客户端发来的那些字节
            assert_eq!(
                up.raw.lock().unwrap()[0],
                Bytes::from(body.to_string()),
                "{on_error:?} {path}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(gw.runs().is_empty(), "{on_error:?} {path}: {:?}", gw.runs());
            assert_eq!(gw.stats("failing"), tw_api::PluginStats::default());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{on_error:?}");
    }

    // 插件一律不管的接口：认不出的路径，和没有正文的那种（取消一次 Responses 的回答）
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let gw = gateway(
        vec![provider("chat", base, Protocol::OpenaiChat)],
        SecurityMode::Off,
        vec![entry("scrub", scrub_inputs(Default::default()))],
    )
    .await;
    let (status, _) = post(
        &gw,
        "/v1/rerank",
        &json!({ "model": "rerank-1", "query": MARK }),
    )
    .await;
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&up.raw.lock().unwrap()[0]).contains(MARK));
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let gw2 = gateway(
        vec![provider("responses", base, Protocol::OpenaiResponses)],
        SecurityMode::Off,
        vec![entry("scrub", scrub_inputs(Default::default()))],
    )
    .await;
    let r = reqwest::Client::new()
        .post(format!("http://{}/v1/responses/resp_1/cancel", gw2.addr))
        .header("authorization", "Bearer tw-testkey")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(up.hits(), 1);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(gw.runs().is_empty() && gw2.runs().is_empty());
}

/// 跑不了的插件（文件变了）只拦它声明过的那几种：只处理对话的拦不着嵌入，声明了嵌入的
/// 照它的 `on_error` 拒掉嵌入请求
#[tokio::test]
async fn a_broken_plugin_only_rejects_the_kinds_it_declared() {
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let changed = |kinds: &[tw_api::RequestKind]| {
        let mut a = double::active(
            "old",
            Double::new("Old")
                .permit(&[Permission::Messages])
                .requests(kinds),
        );
        a.state = PluginState::Broken(Broken::Changed);
        Arc::new(a)
    };
    let providers = || vec![provider("chat", base, Protocol::OpenaiChat)];

    // 只处理对话：嵌入照常，对话被拒
    let gw = gateway(
        providers(),
        SecurityMode::Off,
        vec![changed(&[tw_api::RequestKind::Conversation])],
    )
    .await;
    let (status, answer) = post(&gw, "/v1/embeddings", &embeddings_body()).await;
    assert_eq!(status, 200, "{answer}");
    let (status, answer) = post(&gw, "/v1/completions", &completions_body()).await;
    assert_eq!(status, 200, "{answer}");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(gw.runs().is_empty(), "{:?}", gw.runs());
    let chat = json!({ "model": "gpt-5", "messages": [{ "role": "user", "content": "hi" }] });
    let (status, answer) = post(&gw, "/v1/chat/completions", &chat).await;
    assert_eq!(status, 403, "{answer}");
    assert_eq!(gw.runs(), [("old".to_string(), "error".to_string(), 0)]);
    assert_eq!(up.hits(), 2);

    // 声明了嵌入：嵌入被拒，补全照常
    let gw = gateway(
        providers(),
        SecurityMode::Off,
        vec![changed(&[tw_api::RequestKind::Embeddings])],
    )
    .await;
    let (status, answer) = post(&gw, "/v1/embeddings", &embeddings_body()).await;
    assert_eq!(status, 403, "{answer}");
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("changed on disk"),
        "{answer}"
    );
    let (status, _) = post(&gw, "/v1/completions", &completions_body()).await;
    assert_eq!(status, 200);
    assert_eq!(up.hits(), 3);
}

/// 嵌入、补全上的插件和对话上的一样：按发出去的模型、上游挑；只看到占位符；出错、
/// `reject` 按 `on_error` 拒掉整个请求；不能多一项、少一项输入；请求体读不出来时按
/// `on_error` —— 拒绝就不发（`gw.plugin.cannot_read_body`），跳过就原样发、记一笔跳过
#[tokio::test]
async fn embeddings_follow_the_same_scope_placeholders_and_on_error() {
    const EMBED: &str = "/v1/embeddings";
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let providers = || vec![provider("a", base, Protocol::OpenaiChat)];
    let kinds = [tw_api::RequestKind::Embeddings];

    // 范围外：别的模型的插件不跑
    let gw = gateway(
        providers(),
        SecurityMode::Off,
        vec![entry_with("scrub", scrub_inputs(Default::default()), |a| {
            a.scope.models = vec!["text-embedding-3-large".into()]
        })],
    )
    .await;
    let (status, _) = post(&gw, EMBED, &embeddings_body()).await;
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&up.raw.lock().unwrap()[0]).contains(MARK));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(gw.runs().is_empty());

    // 占位符：插件看不到真的密钥，改过的地方换回去再发
    let saw = Arc::new(Mutex::new(String::new()));
    let s = saw.clone();
    let checked = Double::new("checked")
        .permit(&[Permission::Messages])
        .requests(&kinds)
        .on_request(move |mut view, ctx| {
            *s.lock().unwrap() = view.to_string();
            assert_eq!(
                (ctx["upstream"].as_str(), ctx["format"].as_str()),
                (Some("a"), Some("openai_embeddings"))
            );
            let t = view["messages"][0]["parts"][0]["text"]
                .as_str()
                .unwrap()
                .to_string();
            view["messages"][0]["parts"][0]["text"] = json!(format!("{t} (checked)"));
            Invocation::ok(RequestOutcome::Changed(view))
        });
    up.raw.lock().unwrap().clear();
    let gw = gateway(
        providers(),
        SecurityMode::Off,
        vec![entry("checked", checked)],
    )
    .await;
    let body =
        json!({ "model": "text-embedding-3-small", "input": format!("my key is {USER_KEY}") });
    let (status, _) = post(&gw, EMBED, &body).await;
    assert_eq!(status, 200);
    let saw = saw.lock().unwrap().clone();
    assert!(!saw.contains(USER_KEY), "the plugin saw the key: {saw}");
    assert!(saw.contains("<<TW_SECRET_1>>"), "{saw}");
    let sent: Value = serde_json::from_slice(&up.raw.lock().unwrap()[0]).unwrap();
    assert_eq!(sent["input"], format!("my key is {USER_KEY} (checked)"));

    // 出错、拒绝、多加一项输入：整个请求不发
    let failing = Double::new("failing")
        .permit(&[Permission::Messages])
        .requests(&kinds)
        .on_request(|_, _| {
            Invocation::err(RunError::Threw {
                message: "nope".into(),
                stack: None,
            })
        });
    let refusing = Double::new("refusing")
        .permit(&[Permission::Messages])
        .requests(&kinds)
        .on_request(|_, _| Invocation::ok(RequestOutcome::Rejected("not embedded".into())));
    let adding = Double::new("adding")
        .permit(&[Permission::Messages])
        .requests(&kinds)
        .on_request(|mut view, _| {
            view["messages"]
                .as_array_mut()
                .unwrap()
                .push(json!({ "role": "user", "parts": [{ "type": "text", "text": "one more" }] }));
            Invocation::ok(RequestOutcome::Changed(view))
        });
    for (e, code) in [
        (entry("failing", failing), "gw.plugin.request_failed"),
        (entry("refusing", refusing), "gw.plugin.rejected"),
        (entry("adding", adding), "gw.plugin.request_failed"),
    ] {
        up.raw.lock().unwrap().clear();
        let gw = gateway(providers(), SecurityMode::Off, vec![e]).await;
        let rx = gw.state.bus.subscribe();
        let (status, body) = post(&gw, EMBED, &embeddings_body()).await;
        assert_eq!(status, 403, "{code}: {body}");
        assert!(up.raw.lock().unwrap().is_empty(), "{code}");
        let attempts = routed(rx).await;
        assert_eq!(
            attempts[0].error.as_ref().map(|e| e.code.as_str()),
            Some(code)
        );
    }

    // 请求体读不出来：拒绝就不发，跳过就原样发、记一笔跳过（不算一次调用）
    let not_json = |gw: SocketAddr| {
        reqwest::Client::new()
            .post(format!("http://{gw}/v1/embeddings"))
            .header("x-api-key", "tw-testkey")
            .header("content-type", "application/json")
            .body("{ not json")
    };
    let gw = gateway(
        providers(),
        SecurityMode::Off,
        vec![entry("scrub", scrub_inputs(Default::default()))],
    )
    .await;
    up.raw.lock().unwrap().clear();
    let r = not_json(gw.addr).send().await.unwrap();
    assert_eq!(r.status(), 403);
    let answer: Value = r.json().await.unwrap();
    assert_eq!(
        answer["error"]["message"],
        "[ThinkWatch] Plugin `Plugin scrub` cannot read this request: the request body is not JSON"
    );
    assert!(up.raw.lock().unwrap().is_empty());
    assert_eq!(gw.runs(), [("scrub".to_string(), "error".to_string(), 0)]);
    let gw = gateway(
        providers(),
        SecurityMode::Off,
        vec![entry_with("scrub", scrub_inputs(Default::default()), |a| {
            a.on_error = OnError::Skip
        })],
    )
    .await;
    let r = not_json(gw.addr).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(up.raw.lock().unwrap()[0], Bytes::from("{ not json"));
    assert_eq!(gw.runs(), [("scrub".to_string(), "skipped".to_string(), 0)]);
    assert_eq!(gw.stats("scrub").calls, 0);
}

/// 一串 token 只读：插件原样交回，上游收到的是客户端的原话；改它就是越权
#[tokio::test]
async fn token_id_inputs_are_read_only() {
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let body = json!({ "model": "gpt-3.5-turbo-instruct", "prompt": [[1, 2, 3], [4, 5]] });
    let saw = Arc::new(Mutex::new(Value::Null));
    let s = saw.clone();
    let look = Double::new("look")
        .permit(&[Permission::Messages])
        .requests(&[tw_api::RequestKind::Completions])
        .on_request(move |view, _| {
            *s.lock().unwrap() = view.clone();
            Invocation::ok(RequestOutcome::Changed(view))
        });
    let gw = gateway(
        vec![provider("a", base, Protocol::OpenaiChat)],
        SecurityMode::Off,
        vec![entry("look", look)],
    )
    .await;
    let (status, _) = post(&gw, "/v1/completions", &body).await;
    assert_eq!(status, 200);
    assert_eq!(up.raw.lock().unwrap()[0], Bytes::from(body.to_string()));
    let saw = saw.lock().unwrap().clone();
    let parts: Vec<&Value> = saw["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| &m["parts"][0])
        .collect();
    assert_eq!(parts.len(), 2);
    for p in parts {
        assert_eq!(
            (p["type"].as_str(), p["label"].as_str()),
            (Some("other"), Some("tokens"))
        );
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        gw.runs(),
        [("look".to_string(), "unchanged".to_string(), 0)]
    );

    let forge = Double::new("forge")
        .permit(&[Permission::Messages])
        .requests(&[tw_api::RequestKind::Completions])
        .on_request(|mut view, _| {
            view["messages"][0]["parts"][0] =
                json!({ "key": "m0.p0", "type": "text", "text": "hi" });
            Invocation::ok(RequestOutcome::Changed(view))
        });
    let gw = gateway(
        vec![provider("a", base, Protocol::OpenaiChat)],
        SecurityMode::Off,
        vec![entry("forge", forge)],
    )
    .await;
    let (status, answer) = post(&gw, "/v1/completions", &body).await;
    assert_eq!(status, 403, "{answer}");
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no permission to change"),
        "{answer}"
    );
    assert_eq!(up.hits(), 1);
}

/// 插件改过的嵌入请求再查一遍内容过滤，**只看插件加进来的**：插件写进来的命中拦下整个
/// 请求；客户端原话里就有的（嵌入开头不过内容过滤）不因为插件改了别的一项被拦
#[tokio::test]
async fn screening_sees_the_inputs_a_plugin_wrote() {
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
    let write = |text: &'static str| {
        Double::new("write")
            .permit(&[Permission::Messages])
            .requests(&[tw_api::RequestKind::Embeddings])
            .on_request(move |mut view, _| {
                view["messages"][1]["parts"][0]["text"] = json!(text);
                Invocation::ok(RequestOutcome::Changed(view))
            })
    };
    let gw = gateway_with(
        vec![provider("a", base, Protocol::OpenaiChat)],
        SecurityMode::Off,
        vec![entry("write", write("the forbidden-plan"))],
        security.clone(),
    )
    .await;
    let rx = gw.state.bus.subscribe();
    let (status, body) = post(&gw, "/v1/embeddings", &embeddings_body()).await;
    assert_eq!(status, 403, "{body}");
    assert!(up.raw.lock().unwrap().is_empty());
    let attempts = routed(rx).await;
    assert_eq!(
        attempts[0].error.as_ref().map(|e| e.code.as_str()),
        Some("gw.content.refused")
    );

    let gw = gateway_with(
        vec![provider("a", base, Protocol::OpenaiChat)],
        SecurityMode::Off,
        vec![entry("write", write("harmless"))],
        security,
    )
    .await;
    let mut body = embeddings_body();
    body["input"][0] = json!("the forbidden-plan, as the client wrote it");
    let (status, answer) = post(&gw, "/v1/embeddings", &body).await;
    assert_eq!(status, 200, "{answer}");
    let sent: Value = serde_json::from_slice(&up.raw.lock().unwrap()[0]).unwrap();
    assert_eq!(sent["input"][1], "harmless");
}

/// 处置档下，插件写进一项输入的零宽字符删掉之后再发：**删在插件改过的那一项上**。插件
/// 没改的那几项和没有插件时一样不查、不动 —— 客户端自己写在里面的零宽字符原样到上游
#[tokio::test]
async fn hidden_characters_a_plugin_writes_into_an_input_are_stripped_there() {
    let up = Upstream::default();
    let base = start_upstream(up.clone()).await;
    let security = Security {
        content: tw_config::ContentPolicy {
            mode: SecurityMode::Enforce,
            enable: vec!["zero-width".into()],
            ..Default::default()
        },
        ..Default::default()
    };
    let hide = Double::new("hide")
        .permit(&[Permission::Messages])
        .requests(&[tw_api::RequestKind::Embeddings])
        .on_request(|mut view, _| {
            view["messages"][1]["parts"][0]["text"] = json!("un\u{200B}related");
            Invocation::ok(RequestOutcome::Changed(view))
        });
    let gw = gateway_with(
        vec![provider("a", base, Protocol::OpenaiChat)],
        SecurityMode::Off,
        vec![entry("hide", hide)],
        security,
    )
    .await;
    let mut rx = gw.state.bus.subscribe();
    let mut body = embeddings_body();
    body["input"][0] = json!("the client's own zero\u{200B}width");
    let (status, answer) = post(&gw, "/v1/embeddings", &body).await;
    assert_eq!(status, 200, "{answer}");
    let sent: Value = serde_json::from_slice(&up.raw.lock().unwrap()[0]).unwrap();
    assert_eq!(
        sent["input"][1], "unrelated",
        "what the plugin hid reached the upstream"
    );
    assert_eq!(
        sent["input"][0], "the client's own zero\u{200B}width",
        "an input the plugin did not touch was changed"
    );
    assert_eq!(sent["input"][2], json!([9906, 1917]));
    let mut matched = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
        if let tw_api::Event::ContentMatched { rule, outcome, .. } = ev {
            matched.push((rule, outcome));
        }
    }
    assert_eq!(
        matched,
        [("zero-width".to_string(), tw_api::ContentOutcome::Stripped)]
    );
}
