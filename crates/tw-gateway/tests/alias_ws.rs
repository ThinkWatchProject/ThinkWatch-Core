//! WebSocket 那条路上发出去的模型名：别名、指定模型、规则改写、插件改名，和 HTTP 那条路的
//! 一跳同一套（见 `tw_gateway::ws::Naming`）。回答里写着的、和发出去的是同一个模型的名称
//! 写回客户端用的那个。
//!
//! 起真网关、真上游：上游记下收到的每一帧，回答里的模型名是收到的那个的带日期快照（上游
//! 常这么写）。上游不给模型清单，名字靠启用范围（`models_only`）区分 —— 没有清单的上游当作
//! 什么都有，别名对到它启用范围里的第一个名称。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::broadcast::Receiver;
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tw_api::{Event, Permission};
use tw_config::{Client, Config, Provider};
use tw_gateway::plugin::host::double::{self, Double};
use tw_gateway::plugin::{Active, Invocation, PluginSet, RequestOutcome};

/// 假上游：记下收到的每一帧，每个 `response.create` 回一次完整的回答
async fn upstream() -> (SocketAddr, Arc<Mutex<Vec<Value>>>) {
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let app = Router::new()
        .route(
            "/backend-api/codex/responses",
            axum::routing::any(
                |State(seen): State<Arc<Mutex<Vec<Value>>>>, ws: WebSocketUpgrade| async move {
                    ws.on_upgrade(move |sock| answer(sock, seen))
                },
            ),
        )
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, seen)
}

async fn answer(mut sock: WebSocket, seen: Arc<Mutex<Vec<Value>>>) {
    while let Some(Ok(m)) = sock.recv().await {
        let Message::Text(t) = m else { continue };
        let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
        let model = format!("{}-2026-09-30", v["model"].as_str().unwrap_or_default());
        let n = {
            let mut s = seen.lock().unwrap();
            s.push(v);
            s.len()
        };
        let id = format!("resp_{n}");
        let frames = [
            json!({"type":"response.created","sequence_number":0,
                   "response":{"id":id,"status":"in_progress","model":model,"output":[]}}),
            json!({"type":"response.output_text.delta","sequence_number":1,
                   "output_index":0,"content_index":0,"item_id":"msg","delta":"hello"}),
            json!({"type":"response.completed","sequence_number":2,
                   "response":{"id":id,"status":"completed","model":model,"output":[]}}),
        ];
        for f in frames {
            if sock
                .send(Message::Text(f.to_string().into()))
                .await
                .is_err()
            {
                return;
            }
        }
    }
}

/// 一个上游，只用 `scope` 里的模型（空的就是全部）
fn provider(up: SocketAddr, scope: &[&str]) -> Provider {
    Provider {
        name: "up".into(),
        base_url: format!("http://{up}"),
        key: Some("sk-upstream".into()),
        protocol: Some(tw_config::Protocol::OpenaiResponses),
        models_only: (!scope.is_empty()).then(|| scope.iter().map(|s| s.to_string()).collect()),
        ..Default::default()
    }
}

/// `aliases` 是别名表、`rules` 是默认路由的规则（YAML；空的就是不写）
fn config(p: Provider, aliases: &str, rules: &str) -> Config {
    let mut c = Config {
        version: 1,
        clients: vec![Client {
            name: "codex".into(),
            key: "tw-wskey".into(),
            ..Default::default()
        }],
        providers: vec![p],
        aliases: if aliases.is_empty() {
            Default::default()
        } else {
            serde_yaml_ng::from_str(aliases).unwrap()
        },
        ..Default::default()
    };
    if !rules.is_empty() {
        c.routes = vec![tw_engine::RouteSet::default_with(
            serde_yaml_ng::from_str(rules).unwrap(),
        )];
    }
    c.engine().validate().unwrap();
    c
}

/// 起网关。**先订阅事件再起服务** —— 之后才订的话，早到的事件就看不见了
async fn gateway(cfg: Config, plugins: Vec<Arc<Active>>) -> (SocketAddr, Receiver<Event>) {
    let state = tw_gateway::AppState::new(cfg).unwrap();
    if !plugins.is_empty() {
        state.swap_plugins(PluginSet::new(plugins));
    }
    let events = state.bus.subscribe();
    let addr = tw_gateway::serve(state, ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;
    (addr, events)
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(gw: SocketAddr) -> Socket {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = format!("ws://{gw}/backend-api/codex/responses")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("x-api-key", "tw-wskey".parse().unwrap());
    tokio_tungstenite::connect_async(req).await.unwrap().0
}

/// 一帧 `response.create`，要的是 `model`
fn create(model: &str) -> WsMsg {
    WsMsg::Text(
        json!({
            "type": "response.create",
            "model": model,
            "instructions": "You are Codex.",
            "input": [{ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "hi" }] }],
            "stream": true
        })
        .to_string()
        .into(),
    )
}

/// 收到这一次回答的结尾为止的每一帧（解成 JSON）
async fn one_answer(c: &mut Socket) -> Vec<Value> {
    let mut out = Vec::new();
    while let Ok(Some(Ok(m))) = tokio::time::timeout(Duration::from_secs(3), c.next()).await {
        let WsMsg::Text(t) = m else { continue };
        let v: Value = serde_json::from_str(&t).unwrap_or_else(|_| json!({ "raw": t.as_str() }));
        let end = matches!(
            v["type"].as_str(),
            Some("response.completed" | "response.failed")
        );
        out.push(v);
        if end {
            break;
        }
    }
    out
}

/// 回答里写着的模型名（`response.created`、`response.completed` 各一个）
fn answered(frames: &[Value]) -> Vec<String> {
    frames
        .iter()
        .filter_map(|v| v["response"]["model"].as_str().map(str::to_string))
        .collect()
}

/// 这一次没发出去：替它回的 `response.failed` 里那句话
fn refusal(frames: &[Value]) -> String {
    let last = frames.last().expect("no frame at all");
    assert_eq!(last["type"], "response.failed", "{frames:?}");
    last["response"]["error"]["message"]
        .as_str()
        .unwrap()
        .to_string()
}

/// 握手后发的路由事件里那一跳记下的模型名
async fn attempt_model(rx: &mut Receiver<Event>) -> Option<String> {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("5 秒内没等到路由事件")
            .expect("事件流断了");
        if let Event::RequestRouted { attempts, .. } = ev {
            assert_eq!(attempts.len(), 1, "{attempts:?}");
            return attempts[0].model.clone();
        }
    }
}

/// `codex-fast` 在这一家叫 `gpt-5.1-codex-mini`（列表里头一个名字不在它的启用范围里）
const FAST: &str = "codex-fast: [gpt-6-fast, gpt-5.1-codex-mini]\n";
/// 只在 Claude 那几家有的别名
const SONNET: &str = "claude-sonnet: [claude-sonnet-5, us.anthropic.claude-sonnet-5-v1:0]\n";
const SCOPE: &[&str] = &["gpt-5.1-*", "codex-fast", "special"];

/// 别名：发给这一家的是它自己的名称，不是别名本身；回答里写回别名。不是别名的照旧原样，
/// 回答也原样
#[tokio::test]
async fn an_alias_goes_out_as_the_upstreams_own_name_and_the_answer_shows_the_alias() {
    let (up, seen) = upstream().await;
    let (gw, mut events) = gateway(config(provider(up, SCOPE), FAST, ""), vec![]).await;
    let mut c = connect(gw).await;
    // 别名要看每一帧写的是什么：升级时说不上来
    assert_eq!(attempt_model(&mut events).await, None);

    c.send(create("codex-fast")).await.unwrap();
    let frames = one_answer(&mut c).await;
    let sent = seen.lock().unwrap()[0].clone();
    assert_eq!(sent["model"], "gpt-5.1-codex-mini");
    // 别的字段原样
    assert_eq!(sent["type"], "response.create");
    assert_eq!(sent["instructions"], "You are Codex.");
    assert_eq!(
        answered(&frames),
        ["codex-fast", "codex-fast"],
        "{frames:?}"
    );

    c.send(create("gpt-5.1-codex")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(seen.lock().unwrap()[1]["model"], "gpt-5.1-codex");
    assert_eq!(
        answered(&frames),
        ["gpt-5.1-codex-2026-09-30", "gpt-5.1-codex-2026-09-30"]
    );
}

/// 只按客户端认的规则指定了模型：升级时选中那一家，每一帧发的是指定的名字（不是这一帧
/// 写的），尝试链上记它；回答里写回客户端要的
#[tokio::test]
async fn a_rule_that_pins_a_model_for_the_client_sends_the_pinned_name() {
    let (up, seen) = upstream().await;
    let rules = "
- { name: Codex 指定模型, when: { client: codex }, to: [{ provider: up, model: gpt-5.1-codex-max }] }
- { name: 兜底, to: __all__ }
";
    let (gw, mut events) = gateway(config(provider(up, &[]), FAST, rules), vec![]).await;
    let mut c = connect(gw).await;
    assert_eq!(
        attempt_model(&mut events).await.as_deref(),
        Some("gpt-5.1-codex-max")
    );
    // 指定的原样发，写的是别名也不对
    for asked in ["gpt-5.1-codex", "codex-fast"] {
        c.send(create(asked)).await.unwrap();
        let frames = one_answer(&mut c).await;
        assert_eq!(
            seen.lock().unwrap().last().unwrap()["model"],
            "gpt-5.1-codex-max"
        );
        assert_eq!(answered(&frames), [asked, asked], "{frames:?}");
    }
}

/// 阶段一改写成别名：按别名表对到这一家，尝试链上记对到的名字。阶段二改的名字原样发
/// （名字恰好是别名也一样）。阶段二拒绝的只是那一帧，连接照常
#[tokio::test]
async fn a_rule_rewrite_to_an_alias_is_resolved_and_a_phase_two_name_goes_out_as_written() {
    let (up, seen) = upstream().await;
    let rules = "
- { name: Codex 用别名, when: { client: codex }, set: { model: codex-fast } }
- { name: 阶段二改名, when: { provider_would_be: up, model: special }, set: { model: codex-fast } }
- { name: 不给, when: { provider_would_be: up, model: banned }, deny: 这个模型不走这里 }
- { name: 兜底, to: __all__ }
";
    let (gw, mut events) = gateway(config(provider(up, SCOPE), FAST, rules), vec![]).await;
    let mut c = connect(gw).await;
    assert_eq!(
        attempt_model(&mut events).await.as_deref(),
        Some("gpt-5.1-codex-mini")
    );

    c.send(create("gpt-5.1-codex")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(seen.lock().unwrap()[0]["model"], "gpt-5.1-codex-mini");
    assert_eq!(answered(&frames), ["gpt-5.1-codex", "gpt-5.1-codex"]);

    c.send(create("special")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(seen.lock().unwrap()[1]["model"], "codex-fast");
    assert_eq!(answered(&frames), ["special", "special"]);

    c.send(create("banned")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(
        refusal(&frames),
        "[ThinkWatch] Rule `不给` denied this request: 这个模型不走这里"
    );
    assert_eq!(seen.lock().unwrap().len(), 2);

    c.send(create("gpt-5.1-codex")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(answered(&frames), ["gpt-5.1-codex", "gpt-5.1-codex"]);
    assert_eq!(seen.lock().unwrap().len(), 3);
}

/// 这一家服务不了要的别名：这一帧不发，替它回一个说清楚的 `response.failed`；连接照常，
/// 下一帧要的是这一家有的就照发。规则改写成的别名一样，那句话说出是规则改的
#[tokio::test]
async fn an_alias_the_upstream_cannot_serve_fails_that_request_and_the_connection_stays() {
    let (up, seen) = upstream().await;
    let aliases = format!("{FAST}{SONNET}");
    let (gw, _) = gateway(config(provider(up, SCOPE), &aliases, ""), vec![]).await;
    let mut c = connect(gw).await;
    c.send(create("claude-sonnet")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(
        refusal(&frames),
        "[ThinkWatch] This WebSocket connection goes to upstream `up`, which offers none of the \
         models of alias claude-sonnet (claude-sonnet-5, us.anthropic.claude-sonnet-5-v1:0), so \
         the request was not sent."
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "{:?}",
        seen.lock().unwrap()
    );

    c.send(create("codex-fast")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(answered(&frames), ["codex-fast", "codex-fast"]);
    assert_eq!(seen.lock().unwrap()[0]["model"], "gpt-5.1-codex-mini");

    // 阶段一的规则把模型改写成了这一家服务不了的别名
    let (up, seen) = upstream().await;
    let rules = "
- { name: Codex 用 Sonnet, when: { client: codex }, set: { model: claude-sonnet } }
- { name: 兜底, to: __all__ }
";
    let (gw, mut events) = gateway(config(provider(up, SCOPE), &aliases, rules), vec![]).await;
    let mut c = connect(gw).await;
    assert_eq!(attempt_model(&mut events).await, None);
    c.send(create("gpt-5.1-codex")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(
        refusal(&frames),
        "[ThinkWatch] A routing rule rewrote model gpt-5.1-codex to alias claude-sonnet. This \
         WebSocket connection goes to upstream `up`, which offers none of its models \
         (claude-sonnet-5, us.anthropic.claude-sonnet-5-v1:0), so the request was not sent."
    );
    assert!(seen.lock().unwrap().is_empty());
}

/// 插件把模型名换成别名：发给这一家的是它自己的名称，回答里写回客户端要的。插件的
/// `ctx.model` 是发给这一家的名字。换成这一家服务不了的别名：这一帧不发，连接照常
#[tokio::test]
async fn a_plugin_renaming_to_an_alias_sends_the_upstreams_own_name() {
    let (up, seen) = upstream().await;
    let ctx_models: Arc<Mutex<Vec<String>>> = Arc::default();
    let cm = ctx_models.clone();
    let swap = Double::new("swap")
        .permit(&[Permission::Params])
        .on_request(move |mut view, ctx| {
            cm.lock()
                .unwrap()
                .push(ctx["model"].as_str().unwrap_or_default().to_string());
            match ctx["requested_model"].as_str() {
                Some("codex") => view["params"]["model"] = json!("codex-fast"),
                Some("sonnet-please") => view["params"]["model"] = json!("claude-sonnet"),
                _ => {}
            }
            Invocation::ok(RequestOutcome::Changed(view))
        });
    let mut a = double::active("swap", swap);
    a.name = "Plugin swap".into();
    let aliases = format!("{FAST}{SONNET}");
    let (gw, _) = gateway(
        config(
            provider(up, &["gpt-5.1-*", "codex", "sonnet-please"]),
            &aliases,
            "",
        ),
        vec![Arc::new(a)],
    )
    .await;
    let mut c = connect(gw).await;

    c.send(create("codex")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(seen.lock().unwrap()[0]["model"], "gpt-5.1-codex-mini");
    assert_eq!(answered(&frames), ["codex", "codex"], "{frames:?}");

    c.send(create("sonnet-please")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(
        refusal(&frames),
        "[ThinkWatch] Plugin `Plugin swap` changed the model to the alias claude-sonnet, and \
         upstream `up` offers none of its models, so the request was not sent there."
    );
    assert_eq!(seen.lock().unwrap().len(), 1);

    // 客户端自己要别名：插件看到的 `ctx.model` 是这一家的名称
    c.send(create("codex-fast")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(seen.lock().unwrap()[1]["model"], "gpt-5.1-codex-mini");
    assert_eq!(answered(&frames), ["codex-fast", "codex-fast"]);
    assert_eq!(
        ctx_models.lock().unwrap().as_slice(),
        ["codex", "sonnet-please", "gpt-5.1-codex-mini"]
    );
}
