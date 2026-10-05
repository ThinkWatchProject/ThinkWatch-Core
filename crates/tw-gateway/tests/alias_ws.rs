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

/// 下一轮的路由事件里那一跳记下的模型名：发给这一家的那个，和这一帧写的不一样时才有。
/// **一轮是一个请求**（见 `tw_gateway::ws::turn`），尝试链按轮记
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
    c.send(create("codex-fast")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(
        attempt_model(&mut events).await.as_deref(),
        Some("gpt-5.1-codex-mini")
    );
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
    assert_eq!(attempt_model(&mut events).await, None, "原样发的不记");
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
    // 指定的原样发，写的是别名也不对
    for asked in ["gpt-5.1-codex", "codex-fast"] {
        c.send(create(asked)).await.unwrap();
        let frames = one_answer(&mut c).await;
        assert_eq!(
            attempt_model(&mut events).await.as_deref(),
            Some("gpt-5.1-codex-max")
        );
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
    c.send(create("gpt-5.1-codex")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(
        attempt_model(&mut events).await.as_deref(),
        Some("gpt-5.1-codex-mini")
    );
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
    // 被规则拒绝的这一轮**照样留一行**，和 HTTP 那条路一样：空的尝试链，阶段二拒绝它的是
    // 哪条规则
    let (denied_by, attempts) = loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("no routing event for the denied request")
            .unwrap();
        if let Event::RequestRouted {
            denied_by: Some(rule),
            attempts,
            ..
        } = ev
        {
            break (rule, attempts);
        }
    };
    assert_eq!(denied_by, "不给");
    assert!(attempts.is_empty(), "{attempts:?}");

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
    c.send(create("gpt-5.1-codex")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(
        refusal(&frames),
        "[ThinkWatch] A routing rule rewrote model gpt-5.1-codex to alias claude-sonnet. This \
         WebSocket connection goes to upstream `up`, which offers none of its models \
         (claude-sonnet-5, us.anthropic.claude-sonnet-5-v1:0), so the request was not sent."
    );
    assert!(seen.lock().unwrap().is_empty());
    // 这一家服务不了要的别名：**不留这一行**，和 HTTP 那条路准入没过一样
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
        assert!(
            !matches!(
                ev,
                Event::RequestStarted { .. } | Event::RequestRouted { .. }
            ),
            "{ev:?}"
        );
    }
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

// ---------------------------------------------------------------- 密钥的模型范围与参数改写

/// 只许用 `allow` 里那几个模型的配置
fn scoped(p: Provider, aliases: &str, rules: &str, allow: &[&str]) -> Config {
    let mut c = config(p, aliases, rules);
    c.clients[0].allow = Some(allow.iter().map(|s| s.to_string()).collect());
    c
}

/// 密钥的 `allow` 每一帧都看：范围外的模型这一帧不发，替它回一个说清楚的 `response.failed`，
/// 连接照常 —— 下一帧要的是范围里的就照发。和 HTTP 那条路同一句话
#[tokio::test]
async fn a_frame_asking_for_a_model_outside_the_keys_allow_is_refused_and_the_connection_stays() {
    let (up, seen) = upstream().await;
    let (gw, _) = gateway(scoped(provider(up, &[]), "", "", &["gpt-5*"]), vec![]).await;
    let mut c = connect(gw).await;

    c.send(create("other-model")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(
        refusal(&frames),
        "[ThinkWatch] Gateway key `codex` may not use model other-model. GET /v1/models lists the \
         models that are available."
    );
    assert!(seen.lock().unwrap().is_empty());

    c.send(create("gpt-5.1-codex")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(answered(&frames), ["gpt-5.1-codex-2026-09-30"; 2]);
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(seen.lock().unwrap()[0]["model"], "gpt-5.1-codex");
}

/// 和 HTTP 那条路同一套继承：写上游模型名的 `allow` 也放行列了它的别名，写别名的只管别名
/// 本身。原样发出的名字（阶段二改的、指定的）按名字本身看，没有别名可继承；阶段一改写成的
/// 是客户端那一侧的名称，照样继承
#[tokio::test]
async fn the_keys_allow_is_inherited_by_aliases_over_websocket_like_over_http() {
    let aliases = format!("{FAST}{SONNET}");
    let rules = "
- { name: 阶段一改别名, when: { model: fast-please }, set: { model: codex-fast } }
- { name: 阶段二改名, when: { provider_would_be: up, model: special }, set: { model: codex-fast } }
- { name: 兜底, to: __all__ }
";
    let (up, seen) = upstream().await;
    let (gw, _) = gateway(
        scoped(provider(up, SCOPE), &aliases, rules, &["gpt-5.1-*"]),
        vec![],
    )
    .await;
    let mut c = connect(gw).await;

    // codex-fast 列了 gpt-5.1-codex-mini：放行，发这一家的名字
    c.send(create("codex-fast")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(
        answered(&frames),
        ["codex-fast", "codex-fast"],
        "{frames:?}"
    );
    assert_eq!(seen.lock().unwrap()[0]["model"], "gpt-5.1-codex-mini");

    // claude-sonnet 列的名字一个都不在范围里
    c.send(create("claude-sonnet")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(
        refusal(&frames),
        "[ThinkWatch] Gateway key `codex` may not use model claude-sonnet. GET /v1/models lists \
         the models that are available."
    );

    // 阶段一改写成别名：客户端那一侧的名称，照样继承
    c.send(create("fast-please")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(
        answered(&frames),
        ["fast-please", "fast-please"],
        "{frames:?}"
    );
    assert_eq!(seen.lock().unwrap()[1]["model"], "gpt-5.1-codex-mini");

    // 阶段二改的名字原样发：按 codex-fast 这个名字本身看，不在范围里
    c.send(create("special")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(
        refusal(&frames),
        "[ThinkWatch] A routing rule rewrote model special to codex-fast, which gateway key \
         `codex` may not use. GET /v1/models lists the models that are available."
    );
    assert_eq!(seen.lock().unwrap().len(), 2);

    // 写别名的只管别名本身：它列的模型不跟着放出来
    let (up, seen) = upstream().await;
    let (gw, _) = gateway(
        scoped(provider(up, SCOPE), FAST, "", &["codex-fast"]),
        vec![],
    )
    .await;
    let mut c = connect(gw).await;
    c.send(create("gpt-5.1-codex-mini")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(
        refusal(&frames),
        "[ThinkWatch] Gateway key `codex` may not use model gpt-5.1-codex-mini. GET /v1/models \
         lists the models that are available."
    );
    c.send(create("codex-fast")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(answered(&frames), ["codex-fast", "codex-fast"]);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

/// 规则指定的模型密钥不让用：每一帧都不发，说出是哪条规则指定在哪一家。插件换上范围外的
/// 名字：那一帧不发，和 HTTP 那条路同一句
#[tokio::test]
async fn a_pinned_model_or_a_plugin_rename_outside_the_keys_allow_is_refused_per_frame() {
    let (up, seen) = upstream().await;
    let rules = "
- { name: Codex 指定模型, when: { client: codex }, to: [{ provider: up, model: o3-pro }] }
- { name: 兜底, to: __all__ }
";
    let (gw, _) = gateway(scoped(provider(up, &[]), "", rules, &["gpt-5*"]), vec![]).await;
    let mut c = connect(gw).await;
    c.send(create("gpt-5.1-codex")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(
        refusal(&frames),
        "[ThinkWatch] Rule `Codex 指定模型` pins model o3-pro on `up`, which gateway key `codex` \
         may not use."
    );
    assert!(seen.lock().unwrap().is_empty());

    let swap = Double::new("swap")
        .permit(&[Permission::Params])
        .on_request(|mut view, ctx| {
            if ctx["requested_model"] == "swap-me" {
                view["params"]["model"] = json!("o3-pro");
            }
            Invocation::ok(RequestOutcome::Changed(view))
        });
    let mut a = double::active("swap", swap);
    a.name = "Plugin swap".into();
    let (up, seen) = upstream().await;
    let (gw, _) = gateway(
        scoped(provider(up, &[]), "", "", &["gpt-5*", "swap-me"]),
        vec![Arc::new(a)],
    )
    .await;
    let mut c = connect(gw).await;
    c.send(create("swap-me")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(
        refusal(&frames),
        "[ThinkWatch] Plugin `Plugin swap` changed the model to o3-pro, which gateway key `codex` \
         may not use, so the request was not sent."
    );
    assert!(seen.lock().unwrap().is_empty());
    c.send(create("gpt-5.1-codex")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(answered(&frames), ["gpt-5.1-codex-2026-09-30"; 2]);
}

/// 一帧 `response.create`，要的是 `model`，开着推理
fn create_reasoning(model: &str) -> WsMsg {
    let WsMsg::Text(t) = create(model) else {
        unreachable!()
    };
    let mut v: Value = serde_json::from_str(&t).unwrap();
    v["reasoning"] = json!({ "effort": "high" });
    WsMsg::Text(v.to_string().into())
}

/// 规则的参数改写每一帧照 HTTP 那条路改（Responses 的字段名）：`set.max_tokens` 写成
/// `max_output_tokens`，`set.thinking: false` 去掉 `reasoning`。条件按这一帧求 —— 按模型附加的
/// 改写、按模型拒绝的规则，升级时还不知道模型，每一帧照样生效
#[tokio::test]
async fn rule_parameter_rewrites_and_model_conditions_apply_to_each_frame() {
    let (up, seen) = upstream().await;
    let rules = "
- { name: Codex 限长, when: { client: codex }, set: { max_tokens: 1234 } }
- { name: Max 不思考, when: { model: gpt-5.1-codex-max }, set: { thinking: false, max_tokens: 99 } }
- { name: 不给, when: { model: banned }, deny: 这个模型不走这里 }
- { name: 兜底, to: __all__ }
";
    let (gw, _) = gateway(config(provider(up, &[]), "", rules), vec![]).await;
    let mut c = connect(gw).await;

    c.send(create_reasoning("gpt-5.1-codex")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(answered(&frames), ["gpt-5.1-codex-2026-09-30"; 2]);
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0]["max_output_tokens"], 1234, "{:?}", seen[0]);
        assert_eq!(seen[0]["reasoning"]["effort"], "high");
        // 别的字段原样
        assert_eq!(seen[0]["instructions"], "You are Codex.");
        assert_eq!(seen[0]["type"], "response.create");
    }

    c.send(create_reasoning("gpt-5.1-codex-max")).await.unwrap();
    one_answer(&mut c).await;
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen[1]["max_output_tokens"], 99, "{:?}", seen[1]);
        assert!(seen[1].get("reasoning").is_none(), "{:?}", seen[1]);
        assert_eq!(seen[1]["model"], "gpt-5.1-codex-max");
    }

    c.send(create("banned")).await.unwrap();
    let frames = one_answer(&mut c).await;
    assert_eq!(
        refusal(&frames),
        "[ThinkWatch] Rule `不给` denied this request: 这个模型不走这里"
    );
    assert_eq!(seen.lock().unwrap().len(), 2);

    // 没有改写的帧一个字节都不动
    let (up, seen) = upstream().await;
    let (gw, _) = gateway(config(provider(up, &[]), "", ""), vec![]).await;
    let mut c = connect(gw).await;
    c.send(create_reasoning("gpt-5.1-codex")).await.unwrap();
    one_answer(&mut c).await;
    let WsMsg::Text(t) = create_reasoning("gpt-5.1-codex") else {
        unreachable!()
    };
    assert_eq!(
        seen.lock().unwrap()[0],
        serde_json::from_str::<Value>(&t).unwrap()
    );
}

// ---------------------------------------------------------------- Realtime

/// Realtime 那样的上游：记下升级请求的查询串，回一帧 `session.created`
async fn realtime_upstream() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let queries: Arc<Mutex<Vec<String>>> = Arc::default();
    let app = Router::new()
        .route(
            "/v1/realtime",
            axum::routing::any(
                |State(q): State<Arc<Mutex<Vec<String>>>>,
                 axum::extract::RawQuery(query): axum::extract::RawQuery,
                 ws: WebSocketUpgrade| async move {
                    q.lock().unwrap().push(query.unwrap_or_default());
                    ws.on_upgrade(|mut sock: WebSocket| async move {
                        let created = json!({ "type": "session.created" }).to_string();
                        let _ = sock.send(Message::Text(created.into())).await;
                        while let Some(Ok(_)) = sock.recv().await {}
                    })
                },
            ),
        )
        .with_state(queries.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, queries)
}

/// 连 Realtime：`query` 是网关这边的查询串
async fn realtime(
    gw: SocketAddr,
    query: &str,
) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = format!("ws://{gw}/v1/realtime?{query}")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("x-api-key", "tw-wskey".parse().unwrap());
    tokio_tungstenite::connect_async(req).await.map(|(s, _)| s)
}

/// 升级被拒时回的那句话
fn upgrade_refusal(e: tokio_tungstenite::tungstenite::Error) -> (u16, String) {
    let tokio_tungstenite::tungstenite::Error::Http(r) = e else {
        panic!("the upgrade failed without an answer: {e}");
    };
    let body = String::from_utf8_lossy(r.body().as_deref().unwrap_or_default()).into_owned();
    (r.status().as_u16(), body)
}

/// Realtime 的模型写在查询串里：别名对到这一家自己的名称再发（别的项原样），尝试链上记它，
/// 流量里记客户端写的那个
#[tokio::test]
async fn a_realtime_alias_in_the_query_connects_with_the_upstreams_own_name() {
    let (up, queries) = realtime_upstream().await;
    let aliases = "voice: [gpt-realtime-2, gpt-realtime]\n";
    let (gw, mut events) =
        gateway(config(provider(up, &["gpt-realtime"]), aliases, ""), vec![]).await;
    let mut c = realtime(gw, "model=voice&intent=chat").await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(3), c.next())
        .await
        .expect("no frame")
        .unwrap()
        .unwrap();
    assert!(first.into_text().unwrap().contains("session.created"));
    assert_eq!(
        queries.lock().unwrap().as_slice(),
        ["model=gpt-realtime&intent=chat"]
    );
    let mut started = None;
    let attempt = loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("no routing event")
            .unwrap();
        match ev {
            Event::RequestStarted { model, .. } => started = Some(model),
            Event::RequestRouted { attempts, .. } => break attempts[0].model.clone(),
            _ => {}
        }
    };
    assert_eq!(started.as_deref(), Some("voice"));
    assert_eq!(attempt.as_deref(), Some("gpt-realtime"));

    // 不是别名的照旧原样，查询串一个字节都不动
    let mut c = realtime(gw, "model=gpt-realtime%2Bx&intent=chat")
        .await
        .unwrap();
    c.next().await;
    assert_eq!(
        queries.lock().unwrap()[1],
        "model=gpt-realtime%2Bx&intent=chat"
    );

    // 规则指定的模型原样发
    let (up, queries) = realtime_upstream().await;
    let rules = "
- { name: 语音指定, when: { model: voice }, to: [{ provider: up, model: gpt-realtime-pinned }] }
- { name: 兜底, to: __all__ }
";
    let (gw, _) = gateway(
        config(provider(up, &["gpt-realtime"]), aliases, rules),
        vec![],
    )
    .await;
    let mut c = realtime(gw, "model=voice").await.unwrap();
    c.next().await;
    assert_eq!(
        queries.lock().unwrap().as_slice(),
        ["model=gpt-realtime-pinned"]
    );
}

/// Realtime 的模型密钥不让用：升级本身被拒，和 HTTP 那条路同一句话；上游连都没连。别名照样
/// 继承写上游模型名的 `allow`。这一家服务不了要的别名：也不连
#[tokio::test]
async fn a_realtime_model_outside_the_keys_allow_refuses_the_upgrade() {
    let (up, queries) = realtime_upstream().await;
    let aliases = format!("voice: [gpt-realtime-2, gpt-realtime]\n{SONNET}");
    let (gw, _) = gateway(
        scoped(
            provider(up, &["gpt-realtime", "other-model"]),
            &aliases,
            "",
            &["gpt-realtime*"],
        ),
        vec![],
    )
    .await;

    let (status, body) = upgrade_refusal(realtime(gw, "model=other-model").await.err().unwrap());
    assert_eq!(status, 400);
    assert!(
        body.contains(
            "Gateway key `codex` may not use model other-model. GET /v1/models lists the models \
             that are available."
        ),
        "{body}"
    );
    assert!(queries.lock().unwrap().is_empty());

    let mut c = realtime(gw, "model=voice").await.unwrap();
    c.next().await;
    assert_eq!(queries.lock().unwrap().as_slice(), ["model=gpt-realtime"]);

    let (gw, _) = gateway(
        config(provider(up, &["gpt-realtime"]), &aliases, ""),
        vec![],
    )
    .await;
    let (status, body) = upgrade_refusal(realtime(gw, "model=claude-sonnet").await.err().unwrap());
    assert_eq!(status, 400);
    assert!(
        body.contains("which offers none of the models of alias claude-sonnet"),
        "{body}"
    );
    assert_eq!(queries.lock().unwrap().len(), 1);
}
