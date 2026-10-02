//! 插件端到端测试的架子：假上游、装着插件的网关、读客户端收到的东西。插件跑在网关
//! 默认的引擎里，也就是生产上那一个（`tw_gateway::plugin::sandbox`）。
//!
//! 假上游说 Anthropic（`/v1/messages`）和 OpenAI Responses（`/v1/responses`），按请求
//! 的 `stream` 回流式或整包。流式的文字**一个字符一帧**、工具参数分三片 —— 占位符
//! 必然被切碎，逐段模式的插件每次只拿到一个字。

#![allow(dead_code)]

mod formats;
mod ws;

// 每个测试文件只用到其中一部分
#[allow(unused_imports)]
pub use formats::*;
#[allow(unused_imports)]
pub use ws::*;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{OriginalUri, State};
use serde_json::{Value, json};
use tw_config::{Client, Config, Listen, Protocol, Provider, Security};

pub const KEY: &str = "tw-reh4xqqrzyvbutjacvjywb4e";

/// 对抗用例的源码（`tw-plugin` 的 `tests/corpus/`）
pub fn corpus(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tw-plugin/tests/corpus")
        .join(format!("{name}.js"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

// ── 假上游 ───────────────────────────────────────────────────────

/// 上游对一个请求的回答
#[derive(Clone, Debug)]
pub enum Answer {
    /// 一段文字
    Text(String),
    /// 把请求里第一段带 `<<` 或 `sk-ant-` 的字符串原样说一遍。模型确实会重复别人
    /// 给它的东西 —— 那正是占位符要换回真值的地方
    Echo,
    /// 同上，说在一个工具调用的参数里：`{ "text": … }`
    EchoInTool(String),
    /// 一句话，接一个工具调用
    Tool { name: String, input: Value },
    /// 这个状态码，带一个 Anthropic 格式的错误
    Status(u16),
    /// OpenAI Responses：拒绝别的账号封存的推理（400 invalid_encrypted_content）
    RefuseSealed,
    /// OpenAI Responses：一段文字
    ResponsesText(String),
}

#[derive(Clone)]
pub struct Upstream {
    pub addr: SocketAddr,
    seen: Arc<Mutex<Vec<Vec<u8>>>>,
}

#[derive(Clone)]
struct UpState {
    seen: Arc<Mutex<Vec<Vec<u8>>>>,
    answers: Arc<Mutex<VecDeque<Answer>>>,
}

impl Upstream {
    /// 按到达的顺序依次用 `answers` 回答，用完了一直用最后一个
    pub async fn start(answers: Vec<Answer>) -> Upstream {
        assert!(!answers.is_empty());
        let seen: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
        let st = UpState {
            seen: seen.clone(),
            answers: Arc::new(Mutex::new(answers.into())),
        };
        // 列模型的那个请求不算一次「收到的请求」：网关问清单用（模型准入要它）
        let app = Router::new()
            .route(
                "/v1/models",
                axum::routing::get(|| async {
                    axum::Json(json!({ "data": [
                        { "id": "claude-sonnet-4-5" }, { "id": "claude-opus-4-1" }, { "id": "gpt-5" }
                    ] }))
                }),
            )
            .fallback(respond)
            .with_state(st);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        Upstream { addr, seen }
    }

    pub fn hits(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    /// 第 `i` 个请求的原文
    pub fn raw(&self, i: usize) -> String {
        let seen = self.seen.lock().unwrap();
        let b = seen
            .get(i)
            .unwrap_or_else(|| panic!("the upstream got {} requests, not {}", seen.len(), i + 1));
        String::from_utf8_lossy(b).into_owned()
    }

    pub fn raw_all(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect()
    }

    pub fn body(&self, i: usize) -> Value {
        serde_json::from_str(&self.raw(i)).unwrap_or_else(|e| panic!("{e}: {}", self.raw(i)))
    }
}

async fn respond(
    State(st): State<UpState>,
    OriginalUri(uri): OriginalUri,
    body: bytes::Bytes,
) -> axum::response::Response {
    st.seen.lock().unwrap().push(body.to_vec());
    let answer = {
        let mut a = st.answers.lock().unwrap();
        if a.len() > 1 {
            a.pop_front().unwrap()
        } else {
            a.front().unwrap().clone()
        }
    };
    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let stream = req["stream"].as_bool() == Some(true);
    let echoed = || {
        String::from_utf8_lossy(&body)
            .split('"')
            .find(|p| p.contains("<<") || p.contains("sk-ant-"))
            .unwrap_or("（没看到）")
            .to_string()
    };
    let path = uri.path().to_string();
    match answer {
        Answer::Status(code) => reply(
            code,
            "application/json",
            json!({ "type": "error", "error": { "type": "api_error", "message": "boom" } })
                .to_string(),
        ),
        Answer::RefuseSealed => reply(
            400,
            "application/json",
            json!({ "error": { "message": "The encrypted content for item rs_0 could not be verified.",
                               "type": "invalid_request_error", "param": null,
                               "code": "invalid_encrypted_content" } })
            .to_string(),
        ),
        Answer::ResponsesText(t) => reply(
            200,
            "application/json",
            json!({ "id": "resp_1", "object": "response", "status": "completed", "model": "gpt-5",
                    "output": [{ "type": "message", "id": "msg_1", "role": "assistant",
                                 "content": [{ "type": "output_text", "text": t }] }],
                    "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 } })
            .to_string(),
        ),
        Answer::Text(t) => anthropic(stream, vec![Block::Text(t)]),
        Answer::Echo => anthropic(stream, vec![Block::Text(echoed())]),
        Answer::EchoInTool(name) => anthropic(
            stream,
            vec![
                Block::Text("记下了。".into()),
                Block::Tool {
                    name,
                    input: json!({ "text": echoed() }),
                },
            ],
        ),
        Answer::Tool { name, input } => {
            assert!(path.ends_with("/messages"), "{path}");
            anthropic(
                stream,
                vec![Block::Text("我看一下。".into()), Block::Tool { name, input }],
            )
        }
    }
}

enum Block {
    Text(String),
    Tool { name: String, input: Value },
}

fn reply(status: u16, ty: &str, body: String) -> axum::response::Response {
    axum::response::Response::builder()
        .status(status)
        .header("content-type", ty)
        .body(axum::body::Body::from(body))
        .unwrap()
}

fn anthropic(stream: bool, blocks: Vec<Block>) -> axum::response::Response {
    let has_tool = blocks.iter().any(|b| matches!(b, Block::Tool { .. }));
    let stop = if has_tool { "tool_use" } else { "end_turn" };
    if !stream {
        let content: Vec<Value> = blocks
            .iter()
            .enumerate()
            .map(|(i, b)| match b {
                Block::Text(t) => json!({ "type": "text", "text": t }),
                Block::Tool { name, input } => {
                    json!({ "type": "tool_use", "id": format!("toolu_{i}"), "name": name, "input": input })
                }
            })
            .collect();
        return reply(
            200,
            "application/json",
            json!({ "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5",
                    "content": content, "stop_reason": stop,
                    "usage": { "input_tokens": 10, "output_tokens": 5 } })
            .to_string(),
        );
    }
    let mut s = String::new();
    let mut frame = |event: &str, data: Value| {
        s.push_str(&format!("event: {event}\ndata: {data}\n\n"));
    };
    frame(
        "message_start",
        json!({ "type": "message_start", "message": { "id": "msg_1", "type": "message", "role": "assistant",
                "model": "claude-sonnet-4-5", "content": [], "stop_reason": null,
                "usage": { "input_tokens": 10, "output_tokens": 0 } } }),
    );
    for (i, b) in blocks.iter().enumerate() {
        match b {
            Block::Text(t) => {
                frame(
                    "content_block_start",
                    json!({ "type": "content_block_start", "index": i, "content_block": { "type": "text", "text": "" } }),
                );
                for c in t.chars() {
                    frame(
                        "content_block_delta",
                        json!({ "type": "content_block_delta", "index": i, "delta": { "type": "text_delta", "text": c.to_string() } }),
                    );
                }
            }
            Block::Tool { name, input } => {
                frame(
                    "content_block_start",
                    json!({ "type": "content_block_start", "index": i, "content_block":
                            { "type": "tool_use", "id": format!("toolu_{i}"), "name": name, "input": {} } }),
                );
                let args = input.to_string();
                let chars: Vec<char> = args.chars().collect();
                let third = chars.len().div_ceil(3).max(1);
                for part in chars.chunks(third) {
                    frame(
                        "content_block_delta",
                        json!({ "type": "content_block_delta", "index": i, "delta":
                                { "type": "input_json_delta", "partial_json": part.iter().collect::<String>() } }),
                    );
                }
            }
        }
        frame(
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": i }),
        );
    }
    frame(
        "message_delta",
        json!({ "type": "message_delta", "delta": { "stop_reason": stop }, "usage": { "output_tokens": 5 } }),
    );
    frame("message_stop", json!({ "type": "message_stop" }));
    reply(200, "text/event-stream", s)
}

// ── 配置 ─────────────────────────────────────────────────────────

pub fn provider(name: &str, up: &Upstream) -> Provider {
    Provider {
        name: name.into(),
        base_url: format!("http://{}", up.addr),
        key: Some("sk-upstream".into()),
        protocol: Some(Protocol::Anthropic),
        ..Default::default()
    }
}

pub fn config(up: &Upstream, security: Security) -> Config {
    Config {
        version: 1,
        listen: Listen::default(),
        clients: vec![Client {
            name: "claude-code".into(),
            key: KEY.into(),
            ..Default::default()
        }],
        providers: vec![provider("relay", up)],
        security,
        ..Default::default()
    }
}

// ── 读客户端收到的东西 ───────────────────────────────────────────

pub struct Resp {
    pub status: u16,
    /// `x-thinkwatch-error`：网关自己拒绝时说是哪一类
    pub source: Option<String>,
    pub body: String,
}

fn sse_data(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .collect()
}

/// 流里的全部文字（text_delta 拼起来）
pub fn sse_text(body: &str) -> String {
    sse_data(body)
        .iter()
        .filter_map(|v| v["delta"]["text"].as_str().map(str::to_string))
        .collect()
}

/// 流里第 `index` 块的工具参数（input_json_delta 拼起来）
pub fn sse_tool_input(body: &str, index: u64) -> String {
    sse_data(body)
        .iter()
        .filter(|v| v["index"].as_u64() == Some(index))
        .filter_map(|v| v["delta"]["partial_json"].as_str().map(str::to_string))
        .collect()
}

/// 流里名叫 `name` 的那个工具调用的参数
pub fn sse_tool_input_named(body: &str, name: &str) -> String {
    let data = sse_data(body);
    let index = data.iter().find_map(|v| {
        (v["type"] == "content_block_start" && v["content_block"]["name"] == name)
            .then(|| v["index"].as_u64())
            .flatten()
    });
    match index {
        Some(i) => sse_tool_input(body, i),
        None => String::new(),
    }
}

/// 整包回答里的全部文字
pub fn json_text(body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    v["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| b["text"].as_str())
        .collect()
}

/// 整包回答里第一个工具调用的参数
pub fn json_tool_input(body: &str) -> Value {
    let v: Value = serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"));
    v["content"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|b| b["type"] == "tool_use")
        .map(|b| b["input"].clone())
        .unwrap_or_else(|| panic!("no tool_use in {body}"))
}

/// 对抗插件把看到的东西编成 `seen:` 加一串十六进制码点（点号分隔）；解开它
pub fn decode_seen(s: &str) -> String {
    let at = s
        .find("seen:")
        .unwrap_or_else(|| panic!("no seen: marker in {s}"));
    s[at + 5..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit() || *c == '.')
        .collect::<String>()
        .split('.')
        .filter(|h| !h.is_empty())
        .map(|h| char::from_u32(u32::from_str_radix(h, 16).unwrap()).unwrap())
        .collect()
}

pub async fn wait_a_moment() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

// ── 装着插件的网关 ───────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnError {
    Reject,
    Skip,
}

/// 一个要装上的插件：写进 `plugins/<id>.js`，配置里记下它的哈希（就是批准过的那一份）
pub struct Plug {
    id: String,
    source: String,
    on_error: OnError,
    settings: Value,
    models: Vec<String>,
    upstreams: Vec<String>,
}

impl Plug {
    pub fn new(id: &str, source: impl Into<String>) -> Plug {
        Plug {
            id: id.into(),
            source: source.into(),
            on_error: OnError::Reject,
            settings: json!({}),
            models: Vec::new(),
            upstreams: Vec::new(),
        }
    }

    /// 适用范围里的上游
    pub fn upstreams(mut self, upstreams: &[&str]) -> Plug {
        self.upstreams = upstreams.iter().map(|u| u.to_string()).collect();
        self
    }

    /// 适用范围里的模型（配置里那一份，装上时照 manifest 填的就是它）
    pub fn models(mut self, models: &[&str]) -> Plug {
        self.models = models.iter().map(|m| m.to_string()).collect();
        self
    }

    pub fn settings(mut self, settings: Value) -> Plug {
        self.settings = settings;
        self
    }

    pub fn on_error(mut self, on_error: OnError) -> Plug {
        self.on_error = on_error;
        self
    }
}

pub struct Gateway {
    pub addr: SocketAddr,
    pub state: tw_gateway::AppState,
    dir: tempfile::TempDir,
    runs: Arc<Mutex<Vec<tw_gateway::plugin::load::RunRecord>>>,
}

impl Gateway {
    pub async fn start(mut cfg: Config, plugs: Vec<Plug>) -> Gateway {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("plugins")).unwrap();
        for p in &plugs {
            std::fs::write(
                dir.path().join("plugins").join(format!("{}.js", p.id)),
                &p.source,
            )
            .unwrap();
            cfg.plugins.push(tw_config::Plugin {
                id: p.id.clone(),
                file: format!("plugins/{}.js", p.id),
                sha256: tw_gateway::plugin::load::sha256_hex(p.source.as_bytes()),
                enabled: true,
                on_error: match p.on_error {
                    OnError::Reject => tw_config::PluginOnError::Reject,
                    OnError::Skip => tw_config::PluginOnError::Skip,
                },
                scope: tw_config::PluginScope {
                    models: p.models.clone(),
                    upstreams: p.upstreams.clone(),
                    ..Default::default()
                },
                settings: p
                    .settings
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_yaml_ng::to_value(v).unwrap()))
                    .collect(),
            });
        }
        // 引擎用网关默认的那一个（`tw-plugin` 的沙箱），和生产上一样
        let state = tw_gateway::AppState::new(cfg).unwrap();
        state.set_config_dir(dir.path().to_path_buf());
        let runs: Arc<Mutex<Vec<tw_gateway::plugin::load::RunRecord>>> = Default::default();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        state.plugins.set_sink(tx);
        let r = runs.clone();
        tokio::spawn(async move {
            while let Some(rec) = rx.recv().await {
                r.lock().unwrap().push(rec);
            }
        });
        // 装上的每一个都要是好的（这些测试不测「装不上」）
        let rt = state.runtime();
        for p in &plugs {
            let a = rt
                .plugins
                .get(&p.id)
                .unwrap_or_else(|| panic!("{} is not in the plugin set", p.id));
            assert!(
                a.ready().is_some(),
                "{} did not load: {:?}",
                p.id,
                a.broken()
            );
        }
        let addr = tw_gateway::serve(state.clone(), ([127, 0, 0, 1], 0).into())
            .await
            .unwrap();
        wait_a_moment().await;
        Gateway {
            addr,
            state,
            dir,
            runs,
        }
    }

    pub async fn post(&self, path: &str, body: Value) -> Resp {
        let r = reqwest::Client::new()
            .post(format!("http://{}{path}", self.addr))
            .header("x-api-key", KEY)
            .header("authorization", format!("Bearer {KEY}"))
            .header("x-goog-api-key", KEY)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        let source = r
            .headers()
            .get("x-thinkwatch-error")
            .map(|v| v.to_str().unwrap().to_string());
        let body = r.text().await.unwrap();
        // 运行记录是请求结束之后交出去的：等它一下
        wait_a_moment().await;
        Resp {
            status,
            source,
            body,
        }
    }

    /// Anthropic 的 `/v1/messages`
    pub async fn ask(&self, body: Value) -> Resp {
        self.post("/v1/messages", body).await
    }

    /// 原样发这些字节（空白、键的顺序、数字的写法都由调用方定）
    pub async fn post_raw(&self, path: &str, body: &str) -> Resp {
        let r = reqwest::Client::new()
            .post(format!("http://{}{path}", self.addr))
            .header("x-api-key", KEY)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        let source = r
            .headers()
            .get("x-thinkwatch-error")
            .map(|v| v.to_str().unwrap().to_string());
        let body = r.text().await.unwrap();
        wait_a_moment().await;
        Resp {
            status,
            source,
            body,
        }
    }

    /// 连上网关的 WebSocket（Codex 的 Responses WebSocket 那一路）
    pub async fn ws(&self) -> WsClient {
        WsClient::connect(self.addr, KEY).await
    }

    /// 向每个上游问一遍模型清单。模型准入要它：清单空着时网关不拦
    pub async fn refresh_models(&self) {
        tw_gateway::models::refresh_all(&self.state).await;
    }

    /// 这个插件最近写的日志
    pub fn logs(&self, id: &str) -> Vec<String> {
        self.state
            .runtime()
            .plugins
            .get(id)
            .unwrap_or_else(|| panic!("no plugin {id}"))
            .logs
            .lines()
            .into_iter()
            .map(|l| l.text)
            .collect()
    }

    pub fn events(&self) -> tokio::sync::broadcast::Receiver<tw_api::Event> {
        self.state.bus.subscribe()
    }

    /// 插件从启动以来跑了几次（跳过的不算）
    pub fn calls(&self, id: &str) -> u64 {
        self.state
            .runtime()
            .plugins
            .get(id)
            .unwrap_or_else(|| panic!("no plugin {id}"))
            .stats
            .view()
            .calls
    }

    /// 这个插件每次运行的结局，按先后
    pub fn outcomes(&self, id: &str) -> Vec<String> {
        self.runs
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.run.plugin_id == id)
            .map(|r| r.run.outcome.slug().to_string())
            .collect()
    }

    /// 这个插件在某一种钩子（`request` / `reply`）上每次运行的结局，按先后
    pub fn outcomes_of(&self, id: &str, hook: &str) -> Vec<String> {
        self.runs
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.run.plugin_id == id && r.run.hook.slug() == hook)
            .map(|r| r.run.outcome.slug().to_string())
            .collect()
    }

    /// 这个插件每次出错、被拒时记下的消息码，按先后
    pub fn error_codes(&self, id: &str) -> Vec<String> {
        self.runs
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.run.plugin_id == id)
            .filter_map(|r| r.run.error.as_ref().map(|m| m.code.clone()))
            .collect()
    }

    /// 全部运行记录：`(插件, 钩子, 结局)`
    pub fn recorded(&self) -> Vec<(String, String, String)> {
        self.runs
            .lock()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r.run.plugin_id.clone(),
                    r.run.hook.slug().to_string(),
                    r.run.outcome.slug().to_string(),
                )
            })
            .collect()
    }

    /// 批准之后，有人改了磁盘上的插件文件（往末尾加一行）。网关重读插件
    pub async fn tamper(&self, id: &str, append: &str) {
        let path = self.dir.path().join("plugins").join(format!("{id}.js"));
        let mut src = std::fs::read_to_string(&path).unwrap();
        src.push_str(append);
        std::fs::write(&path, src).unwrap();
        self.state.reload_plugins();
        let rt = self.state.runtime();
        let a = rt.plugins.get(id).unwrap();
        assert!(
            a.ready().is_none(),
            "{id} still runs after its file changed"
        );
    }
}
