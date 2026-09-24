//! WebSocket 升级。

use std::sync::Arc;

use axum::http::HeaderMap;
use axum::response::Response;

use super::{Sender, now_ms};
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
    let decision = match rt.engine.route(&facts).map_err(|e| {
        GatewayError::config(msg!("gw.route.failed", detail = e => "Routing failed: {detail}"))
    })? {
        tw_engine::Outcome::Route(d) => d,
        tw_engine::Outcome::Deny { rule, reason } => {
            tracing::info!(%rule, "a rule denied the WebSocket upgrade");
            return Err(GatewayError::denied(msg!(
                "gw.route.denied", rule = rule, reason = reason =>
                "Rule `{rule}` denied this request: {reason}"
            )));
        }
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
    let id = state.bus.next_id();
    state.bus.emit(tw_api::Event::RequestStarted {
        id,
        client: client_name,
        client_hint: crate::hint::client_hint(&headers),
        session_fp: None,
        peer: from.peer,
        key_masked: from.key,
        provider: name.clone(),
        billing: provider.billing.into(),
        model: String::new(),
        method: "WS".to_string(),
        path: uri.path().to_string(),
        at_ms: now_ms(),
    });
    // 这条连接怎么断的，就是这个请求的结局。**跟着连接走**：升级没完成
    // 就被丢掉的 —— 客户端没等到 101 就走了 —— 由 Drop 报成取消。WS 帧
    // 不留档，所以没有 body 的去处
    let ending = crate::ending::Ending::new(
        state.bus.clone(),
        id,
        String::new(),
        started,
        now_ms() as i64,
        None,
    );
    let upstream = crate::ws::Upstream {
        url: crate::ws::upstream_url(&provider.base_url, uri.path(), query.as_deref()),
        headers: upstream_headers,
        provider: provider.clone(),
        rule: decision.matched_rule,
        group: decision.via_group,
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
        crate::ws::proxy(state, sock, upstream, rules, id, ending).await;
    }))
}
