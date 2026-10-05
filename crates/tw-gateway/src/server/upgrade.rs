//! WebSocket 升级。

use std::sync::Arc;

use axum::http::HeaderMap;
use axum::response::Response;

use super::{Choice, Sender, now_ms};
use crate::error::GatewayError;
use crate::state::{AppState, Runtime};
use tw_types::msg;

/// 接管一次 WebSocket 升级。
///
/// 路由照走一遍 —— **一次升级也是一次请求**，`deny` 规则、熔断对它一样
/// 有效，候选的次序和 HTTP 那条路是同一段（[`super::pipeline::arrange`]）：`load-balance`
/// 按权重轮、记同一本账，`url-test`、`cheapest` 同样排。之后把连接交给 [`crate::ws::proxy`]，
/// 那里会在每一帧上重新点一遍管线的保护。路由事件也由那边发：选中的那一家接没
/// 接下，要和它握完手才知道。
///
/// **Responses 的连接上每个 `response.create` 是一个请求**（见 `crate::ws::turn`）：连接
/// 本身不留行，密钥的用量上限、并发上限按轮算，升级时不看。Realtime 和别的路径的连接照旧
/// 整条连接一行，升级时过一遍用量上限。
#[allow(clippy::too_many_arguments)]
pub(super) async fn ws_upgrade(
    state: AppState,
    rt: Arc<Runtime>,
    ws: axum::extract::WebSocketUpgrade,
    client_name: String,
    uri: axum::http::Uri,
    query: Option<String>,
    headers: HeaderMap,
    started: std::time::Instant,
    live: crate::live::Pass,
    from: Sender,
) -> Result<Response, GatewayError> {
    // 升级请求没有体，所以性质里只有客户端名字 —— 按模型路由的规则
    // 对它不适用，而那是对的：这条连接上会跑什么模型，现在还不知道。这次的决定
    // 里定下的指定模型、模型改写照样作用在之后每一帧上（见 `crate::ws::Naming`）。
    //
    // **Realtime 的连接例外**：它的模型写在查询串里（`?model=gpt-realtime`），一条连接只用
    // 这一个。和 HTTP 那条路按请求体里的模型一样，路由、密钥的模型范围、别名都按它来
    let realtime = crate::ws::realtime(uri.path());
    let facts = tw_engine::RequestFacts {
        client: client_name.clone(),
        model: realtime
            .then(|| crate::ws::query_model(query.as_deref()))
            .flatten()
            .unwrap_or_default(),
        ..Default::default()
    };
    let route = rt.engine.route_of(&client_name).to_string();
    // 这条连接上开始的请求都一样的那几项：整条连接一行的那一行，Responses 的连接上的每一轮
    let opener = crate::ws::turn::Opener {
        bus: state.bus.clone(),
        client: client_name.clone(),
        client_hint: crate::hint::client_hint(&headers),
        peer: from.peer.clone(),
        key_masked: from.key.clone(),
        path: uri.path().to_string(),
    };
    // 开始事件。**被规则拒绝的升级也发**（和 HTTP 那条路一样，见
    // `super::routed_nowhere`）：流量里要有这一行，规则的命中也要数得到它
    //
    // 这条连接怎么断的，就是这个请求的结局。**跟着连接走**：升级没完成就被丢掉的 ——
    // 客户端没等到 101 就走了 —— 由 Drop 报成取消
    let open = |choice: &Choice, provider: &str, billing: tw_api::Billing| {
        opener.open(crate::ws::turn::Opening {
            choice,
            to: (provider, billing),
            // 客户端写的模型名：Realtime 的连接写在查询串里，别的连接升级时还不知道
            model: facts.model.clone(),
            // 升级请求没有正文，认不出是哪段对话，也没有可估的
            session: None,
            input_estimate: None,
            started,
            at_ms: now_ms(),
        })
    };
    let mut decision = match rt.engine.route(&facts).map_err(|e| {
        GatewayError::config(msg!("gw.route.failed", detail = e => "Routing failed: {detail}"))
    })? {
        tw_engine::Outcome::Route(d) => d,
        tw_engine::Outcome::Deny { rule, reason } => {
            tracing::info!(%rule, "a rule denied the WebSocket upgrade");
            let err = GatewayError::denied(msg!(
                "gw.route.denied", rule = rule.clone(), reason = reason =>
                "Rule `{rule}` denied this request: {reason}"
            ));
            let choice = Choice {
                route,
                rule,
                group: None,
                rewritten_by: Vec::new(),
                affinity: None,
                stayed_on: None,
            };
            let (id, ending) = open(&choice, "", tw_api::Billing::PerToken);
            state.bus.emit(super::routed_nowhere(id, choice));
            ending.failed(err.source.into(), err.detail.clone());
            return Err(err);
        }
    };
    // 候选的次序：**和 HTTP 那条路同一段**（见 `super::pipeline::arrange`）。先去掉服务不了的
    // （停用的；Realtime 的连接写了模型，还有别名对不上、密钥不让用的），再按组排 ——
    // `load-balance` 照权重轮到谁就是谁，不是一律连头一家。升级请求没有正文，认不出是哪段
    // 对话，没有粘性。一家都服务不了的照旧交给下面一家家看，说得出为什么
    let catalog = state.catalog.load();
    let allow = crate::models::key_allow(&rt.config, &client_name);
    let asked = rt
        .engine
        .asked(rt.engine.rules_for_client(&client_name), &facts, &decision);
    let sent = crate::sent::plan(&rt.config, &catalog, &decision, &facts.model, &asked, allow);
    let serving = crate::sent::serving(&rt.config, &catalog, &decision, &asked, allow);
    if !serving.usable.is_empty() {
        decision.candidates = serving.usable;
    }
    let arranged = super::pipeline::arrange(
        &state,
        &rt,
        &mut decision,
        &crate::sent::pairs(&sent),
        None,
        now_ms(),
    );
    // 发给哪一家、每个 `response.create` 发出去的模型名怎么定：和 HTTP 那条路的一跳同一套，
    // 按这次的决定定（指定模型、阶段一的改写），别名对到这一家（见 `crate::ws::Naming`）
    let naming_for = |provider: &tw_config::Provider| crate::ws::Naming {
        config: rt.config.clone(),
        engine: rt.engine.clone(),
        decision: decision.clone(),
        provider: provider.clone(),
        client: client_name.clone(),
        path: uri.path().to_string(),
    };
    let (alive, _) = state.health.filter(&decision.candidates);
    // 升级请求带着模型的（Realtime）：挑头一家连得上的 —— 别名对得上、要发给它的名字密钥
    // 让用，和 HTTP 那条路挑候选时一样。别的连接升级时不知道模型，连头一家活着的
    let picked = if facts.model.is_empty() {
        alive.first().map(|s| (s.to_string(), None))
    } else {
        let mut unserved = None;
        let mut barred = None;
        let mut picked = None;
        for c in &alive {
            let Some(p) = rt.config.providers.iter().find(|p| p.name == **c) else {
                continue;
            };
            match naming_for(p).connect(&catalog, &facts) {
                crate::ws::Connect::To {
                    model,
                    rewritten_by,
                } => {
                    picked = Some((c.to_string(), Some((model, rewritten_by))));
                    break;
                }
                crate::ws::Connect::Skip { barred: true, why } => {
                    barred.get_or_insert(why);
                }
                crate::ws::Connect::Skip { barred: false, why } => {
                    unserved.get_or_insert(why);
                }
                // 阶段二的规则拒绝了：**和规则在阶段一拒绝一样留一行**，拒绝它的是哪条规则
                // 写进路由事件
                crate::ws::Connect::Refused { rule, why } => {
                    let choice = Choice {
                        route,
                        rule: decision.matched_rule.clone(),
                        group: decision.via_group.clone(),
                        rewritten_by: decision.rewritten_by.clone(),
                        affinity: None,
                        stayed_on: None,
                    };
                    let Some(rule) = rule else { return Err(why) };
                    tracing::info!(%rule, provider = %c, "a phase-two rule denied the WebSocket upgrade");
                    let (id, ending) = open(&choice, "", tw_api::Billing::PerToken);
                    let mut routed = super::routed_nowhere(id, choice);
                    if let tw_api::Event::RequestRouted { denied_by, .. } = &mut routed {
                        *denied_by = Some(rule);
                    }
                    state.bus.emit(routed);
                    ending.failed(why.source.into(), why.detail.clone());
                    return Err(why);
                }
            }
        }
        // 一家都连不上。每一家要发的名字密钥都不让用的，说是密钥的事（和 HTTP 那条路的准入
        // 一样）；有让用的，说它为什么连不上（别名对不上）。**不留这一行**，和 HTTP 那条路
        // 准入没过一样
        match (picked, unserved, barred) {
            (Some(p), _, _) => Some(p),
            (None, Some(why), _) | (None, None, Some(why)) => return Err(why),
            (None, None, None) => None,
        }
    };
    let Some((name, realtime_model)) = picked else {
        return Err(GatewayError::config(msg!(
            "gw.route.no_upstream_alive" => "No upstream is available."
        )));
    };
    // `load-balance` 记账：记在真连的那一家头上（排在它前面的服务不了这个模型时，不是排头的
    // 那一家）
    arranged.charge(&decision.candidates, &name);
    let Some(provider) = rt.config.providers.iter().find(|p| p.name == name) else {
        return Err(GatewayError::config(msg!(
            "gw.route.upstream_missing", upstream = name.clone() =>
            "`{upstream}` is not in the configuration."
        )));
    };
    // 附加了参数改写的规则：Realtime 的连接升级时就作用上了（查询串里的模型），和 HTTP 那条路
    // 一样报阶段一、阶段二的。Responses 的连接上参数改写每一帧按这一帧求，那时没有事件可报
    let (sent_model, rewritten_by) = match realtime_model {
        Some((model, more)) => {
            let mut by = decision.rewritten_by.clone();
            by.extend(
                more.into_iter()
                    .filter(|r| !decision.rewritten_by.contains(r)),
            );
            (Some(model), by)
        }
        None => (None, Vec::new()),
    };
    let choice = Choice {
        route,
        rule: decision.matched_rule.clone(),
        group: decision.via_group.clone(),
        rewritten_by,
        // WebSocket 那条路一条连接跑好几轮，不按对话记
        affinity: None,
        stayed_on: None,
    };
    // **走代理的上游不代理 WS**，而且要明说。悄悄绕过用户配的代理，
    // 等于把他以为在代理后面的流量直接发出去
    if provider.proxy != tw_config::DIRECT {
        return Err(GatewayError::config(msg!(
            "gw.ws.proxy_unsupported", upstream = name.clone(), proxy = provider.proxy.clone() =>
            "Upstream `{upstream}` goes through proxy `{proxy}`. WebSocket connections are not \
             forwarded through a proxy yet; only directly connected upstreams are."
        )));
    }
    let http = rt.clients.get(&name).unwrap_or(&state.http);
    // 凭据取不到是这一家的问题，和 HTTP 那条路一样记一次失败（见 `crate::health`）
    let upstream_headers = match state.headers_for(provider, http).await {
        Ok(h) => h,
        Err(e) => {
            let change = state.health.record_failure(&name);
            super::note_health(&state.bus, &state.health, &name, change);
            return Err(GatewayError::config(crate::state::credential_failed(
                e, &name,
            )));
        }
    };
    // Responses 的连接：每个 `response.create` 是一个请求（见 `crate::ws::turn`）
    let responses = crate::client_api::ClientApi::of_path(uri.path())
        == Some(crate::client_api::ClientApi::OpenaiResponses)
        && crate::client_api::ClientApi::generates(uri.path());
    let rows = if responses {
        // 连接本身不留行，上限按轮看。连不上上游时按升级的这一刻补上这一行
        crate::ws::Rows::Turns {
            line: std::sync::Arc::new(crate::ws::turn::Line {
                opener: opener.clone(),
                choice: choice.clone(),
                provider: name.clone(),
                billing: provider.billing,
            }),
            upgraded: (started, now_ms()),
        }
    } else {
        // 这把密钥的用量上限：**整条连接算一个请求**，连上之前看一遍，和 HTTP 那条路的准入
        // 同一套（见 `crate::key_limits`）。这一行不带用量，用量的上限只数得到它的请求数
        let limits = rt
            .config
            .clients
            .iter()
            .find(|c| c.name == client_name)
            .map(|c| c.limits.as_slice())
            .unwrap_or_default();
        let hold = match state
            .key_limits
            .admit(
                &client_name,
                limits,
                Default::default(),
                crate::key_limits::slot_wait(&rt.config),
            )
            .await
        {
            Ok(hold) => hold,
            // 被拒的照样留一行，和规则拒绝的一样
            Err(r) => {
                let why = r.error();
                let (id, ending) = open(&choice, "", tw_api::Billing::PerToken);
                state.bus.emit(super::routed_nowhere(id, choice));
                ending.failed(why.source.into(), why.detail.clone());
                return Err(why);
            }
        };
        let (id, ending) = open(&choice, &name, provider.billing.into());
        hold.bind(id);
        crate::ws::Rows::Connection {
            id,
            ending: Box::new(ending),
        }
    };
    // 插件：升级那一刻的那一份表，一条连接用到底。**插件只管 Responses 的 WebSocket**（每个
    // `response.create` 是一次对话请求）；别的路径上的连接（比如 Realtime 的 `/v1/realtime`）
    // 不属于插件处理的任何一种请求，所有插件都不管：原样接上，什么都不记
    let plugins = (responses && !rt.plugins.is_empty()).then(|| crate::ws::Plugins {
        pool: state.plugin_pool.clone(),
        set: rt.plugins.clone(),
        client: crate::hint::client_hint(&headers),
    });
    // 每个 `response.create` 发出去的模型名和参数改写（见 `crate::ws::Naming`）。和插件一样
    // 只有 Responses 的连接有
    let naming = responses.then(|| naming_for(provider));
    // Realtime 的连接：查询串里的模型写成发给这一家的名字，别的项原样
    let query = match (&sent_model, query) {
        (Some(m), Some(q)) if *m != facts.model => Some(crate::ws::with_query_model(&q, m)),
        (_, q) => q,
    };
    let upstream = crate::ws::Upstream {
        url: crate::ws::upstream_url(&provider.base_url, uri.path(), query.as_deref()),
        headers: upstream_headers,
        provider: provider.clone(),
        route: choice.route,
        rule: choice.rule,
        group: choice.group,
        rewritten_by: choice.rewritten_by,
        model: sent_model.filter(|m| *m != facts.model),
    };
    let rules = crate::ws::Rules {
        redact_mode: rt.config.security.redact.mode,
        redact: rt.redact.clone(),
        inspect_mode: rt.config.security.inspect_tools.mode,
        tools: rt.tools.clone(),
        screen: crate::guard::Screen::of(&rt),
    };
    Ok(ws.on_upgrade(move |sock| async move {
        // 一条 WS 连接活多久，这个请求就算在服务中多久
        let _live = live;
        crate::ws::proxy(state, sock, upstream, rules, rows, plugins, naming).await;
    }))
}
