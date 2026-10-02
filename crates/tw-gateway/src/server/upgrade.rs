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
/// 有效。之后把连接交给 [`crate::ws::proxy`]，那里会在
/// 每一帧上重新点一遍管线的保护。路由事件也由那边发：选中的那一家接没
/// 接下，要和它握完手才知道。
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
    // 对它不适用，而那是对的：这条连接上会跑什么模型，现在还不知道
    let facts = tw_engine::RequestFacts {
        client: client_name.clone(),
        ..Default::default()
    };
    let route = rt.engine.route_of(&client_name).to_string();
    // 开始事件。**被规则拒绝的升级也发**（和 HTTP 那条路一样，见
    // `super::routed_nowhere`）：流量里要有这一行，规则的命中也要数得到它
    let open = |choice: &Choice, provider: &str, billing: tw_api::Billing| {
        let id = state.bus.next_id();
        let at_ms = now_ms();
        state.bus.emit(tw_api::Event::RequestStarted {
            id,
            client: client_name.clone(),
            client_hint: crate::hint::client_hint(&headers),
            // 升级请求没有正文，认不出是哪段对话
            session: None,
            peer: from.peer.clone(),
            key_masked: from.key.clone(),
            route: choice.route.clone(),
            rule: choice.rule.clone(),
            group: choice.group.clone(),
            rewritten_by: choice.rewritten_by.clone(),
            provider: provider.to_string(),
            billing,
            model: String::new(),
            method: "WS".to_string(),
            path: uri.path().to_string(),
            // 升级请求没有正文，没有可估的
            input_estimate: None,
            session_log_bytes: None,
            at_ms,
        });
        // 这条连接怎么断的，就是这个请求的结局。**跟着连接走**：升级没完成
        // 就被丢掉的 —— 客户端没等到 101 就走了 —— 由 Drop 报成取消。WS 帧
        // 不留档，所以没有 body 的去处
        let ending = crate::ending::Ending::new(
            state.bus.clone(),
            id,
            String::new(),
            started,
            at_ms as i64,
            None,
        );
        (id, ending)
    };
    let decision = match rt.engine.route(&facts).map_err(|e| {
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
            };
            let (id, ending) = open(&choice, "", tw_api::Billing::PerToken);
            state.bus.emit(super::routed_nowhere(id, choice));
            ending.failed(err.source.into(), err.detail.clone());
            return Err(err);
        }
    };
    // 升级请求没有正文，规则附加的改写无从谈起
    let choice = Choice {
        route,
        rule: decision.matched_rule.clone(),
        group: decision.via_group.clone(),
        rewritten_by: Vec::new(),
        // WebSocket 那条路一条连接跑好几轮，不按对话记
        affinity: None,
    };
    let (alive, _) = state.health.filter(&decision.candidates);
    let Some(name) = alive.first().map(|s| s.to_string()) else {
        return Err(GatewayError::config(msg!(
            "gw.route.no_upstream_alive" => "No upstream is available."
        )));
    };
    let Some(provider) = rt.config.providers.iter().find(|p| p.name == name) else {
        return Err(GatewayError::config(msg!(
            "gw.route.upstream_missing", upstream = name.clone() =>
            "`{upstream}` is not in the configuration."
        )));
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
    let upstream_headers = state
        .headers_for(provider, http)
        .await
        .map_err(|e| GatewayError::config(crate::state::credential_failed(e, &name)))?;
    let (id, ending) = open(&choice, &name, provider.billing.into());
    // 插件：升级那一刻的那一份表，一条连接用到底。**插件只看得懂 Responses 的 WebSocket**
    // （每个 `response.create` 是一次请求）；别的路径上的帧插件看不懂，管得着的插件按它的
    // `on_error` —— 拒绝就不接这条连接，跳过就记一笔、这条连接不过插件
    let hint = crate::hint::client_hint(&headers);
    let plugins = if rt.plugins.is_empty() {
        None
    } else if crate::client_api::ClientApi::of_path(uri.path())
        == Some(crate::client_api::ClientApi::OpenaiResponses)
        && crate::client_api::ClientApi::generates(uri.path())
    {
        Some(crate::ws::Plugins {
            pool: state.plugin_pool.clone(),
            set: rt.plugins.clone(),
            client: hint.clone(),
        })
    } else {
        let to = crate::plugin::request::Target {
            upstream: &name,
            // 升级请求没有正文：说不出是哪个模型
            model: "",
            requested_model: "",
            attempt: 0,
        };
        let hop_started = std::time::Instant::now();
        match crate::plugin::request::unreadable(&rt.plugins, hint.as_deref(), uri.path(), &to) {
            Ok(runs) => {
                crate::plugin::request::record(&state, id, &runs);
                None
            }
            Err(refused) => {
                crate::plugin::request::record(&state, id, &refused.runs);
                let err = GatewayError::denied(refused.why);
                // 和 HTTP 那条路一样：没发出去的这一跳在尝试链上，原因就是拒绝它的那句话
                state.bus.emit(tw_api::Event::RequestRouted {
                    id,
                    route: choice.route,
                    rule: choice.rule,
                    group: choice.group,
                    rewritten_by: Vec::new(),
                    denied_by: None,
                    affinity: None,
                    attempts: vec![crate::server::hop_failed(
                        &name,
                        None,
                        err.detail.clone(),
                        hop_started,
                    )],
                    billing: tw_api::Billing::PerToken,
                });
                ending.failed(err.source.into(), err.detail.clone());
                return Err(err);
            }
        }
    };
    let upstream = crate::ws::Upstream {
        url: crate::ws::upstream_url(&provider.base_url, uri.path(), query.as_deref()),
        headers: upstream_headers,
        provider: provider.clone(),
        route: choice.route,
        rule: choice.rule,
        group: choice.group,
    };
    let rules = crate::ws::Rules {
        redact_mode: rt.config.security.redact.mode,
        redact: rt.redact.clone(),
        inspect_mode: rt.config.security.inspect_tools.mode,
        tools: rt.tools.clone(),
        screen: crate::guard::Screen::of(&rt),
        limit_mode: rt.config.security.output_limit.mode,
        limit: rt.config.security.output_limit.limit(),
    };
    Ok(ws.on_upgrade(move |sock| async move {
        // 一条 WS 连接活多久，这个请求就算在服务中多久
        let _live = live;
        let mut ending = ending;
        ending.responded(101);
        crate::ws::proxy(state, sock, upstream, rules, id, ending, plugins).await;
    }))
}
