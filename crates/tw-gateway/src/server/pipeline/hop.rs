//! 管线第 5 步的前半：转发，依次尝试候选上游。
//!
//! **首字节之前可以透明切换** —— 拿到响应头之前我们还没往客户端写过任何
//! 东西，换一家客户端完全无感。
//!
//! 「尝试链」要留下来：用户能看见故障转移在替他工作，**这是信任的来源**。
//! 一个静默切换过的请求和一个一次就成的请求，在用户眼里应该是不同的。

use bytes::Bytes;

use super::{Inbound, Started};
use crate::error::GatewayError;
use crate::forward;
use crate::server::{hop, hop_failed, note_health};
use crate::state::{AppState, Runtime};
use tw_types::msg;

/// 接下了这个请求的那一家，和回程要用的东西。
pub(super) struct Served<'a> {
    pub(super) upstream: reqwest::Response,
    pub(super) provider: &'a tw_config::Provider,
    /// 成功那一次的脱敏账本。**必须是成功那一次的** —— 每一跳发出去的体可能
    /// 转换过格式，占位符按那一份的顺序编号
    pub(super) ledger: tw_guard::redact::replace::Ledger,
    /// 成功那一跳的转换。**必须是成功那一次的** —— 故障转移从 Anthropic 上游
    /// 切到 OpenAI 上游时，两跳转成的格式不一样；直通时是 None
    pub(super) session: Option<tw_dialect::convert::Session>,
}

/// 这一跳要发出去的东西。
struct Outbound {
    body: Bytes,
    path: String,
    query: Option<String>,
    /// 转换成了哪种格式。同格式直通时是 None
    target: Option<tw_dialect::ir::Dialect>,
    /// ChatGPT 账号（Codex 后端）：只收流式、不认输出上限、身份头由网关填
    chatgpt: bool,
    /// DeepSeek Harness 的请求发给 DeepSeek 官方以外的上游：它自己的请求头不转发
    /// （见 [`tw_dialect::harness`]）
    harness_elsewhere: bool,
    /// 回程要用的转换会话：转换过的，或者直通到 Codex 后端、客户端却要整包时
    /// 收齐流要用的
    session: Option<tw_dialect::convert::Session>,
}

/// 这一跳没发出去的原因。尝试链里记的和报给客户端的是同一句。
type Skip = GatewayError;

pub(super) async fn try_upstreams<'a>(
    state: &AppState,
    rt: &'a Runtime,
    req: &Inbound,
    reading: &crate::client_api::Reading,
    decision: &tw_engine::Decision,
    started: &Started,
) -> Result<Served<'a>, GatewayError> {
    let id = started.id;
    let mut attempts: Vec<String> = Vec::new();
    // 每一跳的结果和耗时。**失败的原因要留着** —— 一条说「试过 A → B →
    // C」的链，和一条还说清每一跳为什么失败的链，排查价值差得远。
    let mut chain: Vec<tw_api::AttemptView> = Vec::new();
    let mut last_err: Option<GatewayError> = None;
    let mut served: Option<Served<'a>> = None;
    // 改写了这个请求的规则：第一阶段的，加上每一跳第二阶段又加的
    let mut rewritten_by = started.choice.rewritten_by.clone();
    // 第二阶段拒绝了它的那条规则
    let mut denied_by: Option<String> = None;
    // 不再试下一家的原因：第二阶段拒绝了，或者规则求不了值。**路由事件照样要发**
    let mut halt: Option<GatewayError> = None;

    for name in &started.alive {
        let Some(provider) = rt.config.providers.iter().find(|p| &p.name == name) else {
            // 校验时挡过一次，能到这儿说明配置在运行中被换过。
            last_err = Some(GatewayError::config(msg!(
                "gw.route.selected_upstream_missing",
                rule = decision.matched_rule.clone(), upstream = name =>
                "Rule `{rule}` selected upstream `{upstream}`, which is not in the configuration."
            )));
            continue;
        };
        attempts.push(provider.name.clone());
        let hop_started = std::time::Instant::now();

        if let Some(err) = protocol_mismatch(req, reading.generates, provider) {
            chain.push(hop_failed(&provider.name, err.detail.clone(), hop_started));
            last_err = Some(err);
            continue;
        }

        // 阶段二：知道走哪家了，再跑一遍含 `provider_would_be` 的规则。
        //
        // **在循环里面，因为故障转移换了 provider 之后必须重算**。
        // 否则「走中转的一律脱敏」这条规则，在从官方转移到中转时会漏掉
        // —— 而那正是最需要它的时刻。
        let effective_set = match rt
            .engine
            .phase_two(&reading.facts, &provider.name, &decision.set)
        {
            Ok(tw_engine::Outcome2::Proceed {
                set,
                rewritten_by: more,
            }) => {
                for r in more {
                    if !rewritten_by.contains(&r) {
                        rewritten_by.push(r);
                    }
                }
                set
            }
            Ok(tw_engine::Outcome2::Deny { rule, reason }) => {
                tracing::info!(%rule, provider = %provider.name, "a phase-two rule denied the request");
                let err = GatewayError::denied(msg!(
                    "gw.route.denied", rule = rule.clone(), reason = reason =>
                    "Rule `{rule}` denied this request: {reason}"
                ));
                // 被拒的这一跳没有发出去。**它在尝试链上**，原因就是那条拒绝 ——
                // 链上看得出请求本来要去哪家、在哪一步停下的
                chain.push(hop_failed(&provider.name, err.detail.clone(), hop_started));
                denied_by = Some(rule);
                halt = Some(err);
                break;
            }
            Err(e) => {
                halt = Some(GatewayError::config(msg!(
                    "gw.route.rule_failed", detail = e => "A rule could not be evaluated: {detail}"
                )));
                break;
            }
        };

        let out = match prepare(state, req, reading, provider, &effective_set, id) {
            Ok(out) => out,
            Err(err) => {
                chain.push(hop_failed(&provider.name, err.detail.clone(), hop_started));
                last_err = Some(err);
                continue;
            }
        };

        // 出站脱敏的拦截档：换掉**这一跳真正发出去的那一份**（可能转换过
        // 格式）。规则是全局的，每一跳换掉的是同一批东西
        let (body, ledger) =
            crate::guard::replace(rt.config.security.redact.mode, &rt.redact, out.body.clone());

        // 用这个 provider 自己的 Client —— 它带着该走的代理。**在取密钥
        // 之前拿到**：OAuth 换 token 也要走这条代理。
        let http = rt.clients.get(&provider.name).unwrap_or(&state.http);
        let upstream_headers = match state.headers_for(provider, http).await {
            Ok(h) => h,
            Err(e) => {
                // 密钥取不到是这一家的问题（环境变量没设、token 端点
                // 连不上），换下一家是合理的 —— 而且**必须**换：不换的话
                // 一家 OAuth 上游的 token 端点抽风会让整个网关不可用。
                note_health(
                    &state.bus,
                    &state.health,
                    &provider.name,
                    state.health.record_failure(&provider.name),
                );
                let err = GatewayError::config(crate::state::credential_failed(e, &provider.name));
                chain.push(hop_failed(&provider.name, err.detail.clone(), hop_started));
                last_err = Some(err);
                continue;
            }
        };

        tracing::debug!(
            client = %req.client_name,
            provider = %provider.name,
            rule = %decision.matched_rule,
            group = ?decision.via_group,
            url = %tw_secret::redact_url(&forward::upstream_url(&provider.base_url, &out.path, out.query.as_deref())),
            attempt = attempts.len(),
            "forwarding"
        );

        match send(state, req, provider, http, &out, body, upstream_headers).await {
            Ok(r) if r.status().is_server_error() || r.status() == 429 => {
                // 额度用完时上游回的正是 429，这一跳的额度头也要读
                state.note_quota(id, &provider.name, r.headers());
                // 上游回了话，说明代理是通的
                state.note_proxy_ok(&provider.proxy);
                // 5xx 和限流：换一家有意义，那边可能有不同的额度或地域。
                // **4xx 不换**（除了 429）—— 请求本身有问题的话，换一家
                // 也一样被拒，还会白白污染那家的健康度。
                note_health(
                    &state.bus,
                    &state.health,
                    &provider.name,
                    state.health.record_failure(&provider.name),
                );
                chain.push(hop(
                    &provider.name,
                    tw_api::AttemptOutcome::Status,
                    r.status().as_u16(),
                    hop_started,
                ));
                // **429 要保住 429。**塌成 502 的话，客户端会当成「服务器
                // 坏了」而不是「该退避了」，而它们该做的事完全不同。
                last_err = Some(if r.status() == 429 {
                    GatewayError::rate_limited(msg!(
                        "gw.upstream.rate_limited", upstream = provider.name.clone() =>
                        "Upstream `{upstream}` rate-limited the request."
                    ))
                } else {
                    GatewayError::upstream(msg!(
                        "gw.upstream.status", upstream = provider.name.clone(), status = r.status().as_u16() =>
                        "Upstream `{upstream}` answered {status}."
                    ))
                });
                // GLM Coding Plan 的额度用完不在响应头里，在 429 的 body 里
                if r.status() == 429 {
                    state.note_glm_429(id, provider, r).await;
                }
                state.glm_traffic(provider);
                continue;
            }
            Ok(r) => {
                note_health(
                    &state.bus,
                    &state.health,
                    &provider.name,
                    state.health.record_success(&provider.name),
                );
                chain.push(hop(
                    &provider.name,
                    tw_api::AttemptOutcome::Served,
                    r.status().as_u16(),
                    hop_started,
                ));
                served = Some(Served {
                    upstream: r,
                    provider,
                    ledger,
                    session: out.session,
                });
                break;
            }
            Err(e) => {
                note_health(
                    &state.bus,
                    &state.health,
                    &provider.name,
                    state.health.record_failure(&provider.name),
                );
                // 连不上的可能是代理而不是上游 —— 检一次那个代理，说清是哪一件事
                state.check_proxy(&provider.proxy);
                let err = forward::map_reqwest_error(e);
                chain.push(hop_failed(&provider.name, err.detail.clone(), hop_started));
                last_err = Some(err);
                continue;
            }
        }
    }

    // 尝试链走完了，两条路都要发 —— 挂在 RequestFinished 上的话，
    // 失败那条路就没有尝试链，而那恰恰是最需要看它的时候。
    // 最终服务的那家怎么收钱。**跟着请求走，不能事后查配置** ——
    // 配置随时会被热重载，而一条三天前的记录该按它当时那家的算。
    let billing = served
        .as_ref()
        .map(|s| s.provider.billing)
        .unwrap_or_default();
    let choice = &started.choice;
    state.bus.emit(tw_api::Event::RequestRouted {
        id,
        route: choice.route.clone(),
        rule: choice.rule.clone(),
        group: choice.group.clone(),
        rewritten_by,
        denied_by,
        attempts: chain,
        billing: billing.into(),
    });
    if let Some(err) = halt {
        return Err(err);
    }

    let Some(served) = served else {
        let mut err = last_err.unwrap_or_else(|| {
            GatewayError::config(msg!("gw.route.no_upstream_alive" => "No upstream is available."))
        });
        // **尝试链要进客户端看到的那条错误**，不只进我们的事件流 ——
        // 用户看的是他自己终端里的报错。一条说「试过 A → B → C 都不行」
        // 的错误，和一条只说「503」的错误，是两种产品：前者说明我们替
        // 他做了工作，后者让他以为我们什么都没干。
        if attempts.len() > 1 {
            err = err.with_attempts(&attempts);
        }
        // 失败也必须有结局。少了它，UI 上那一行会永远停在「进行中」——
        // 而「一直转圈」比「明确失败」更让人怀疑是我们卡住了。它由
        // `passthrough` 按这里返回的错误报出去，带着上面那串尝试链，
        // `source` 也是这个错误自己的（被限流的就是 `rate_limited`）。
        return Err(err);
    };
    if attempts.len() > 1 {
        tracing::info!(
            chain = %attempts.join(" → "),
            "failed over; {} answered in the end",
            served.provider.name
        );
    }
    Ok(served)
}

/// 生成回答以外的接口（计 token、嵌入……）没有别的格式可以转换，只能交给同格式
/// 的上游。**不发出去**：打到别家的同名路径上，好的情况是 404，坏的情况是被
/// 当成另一个接口执行
fn protocol_mismatch(
    req: &Inbound,
    generates: bool,
    provider: &tw_config::Provider,
) -> Option<Skip> {
    if generates {
        return None;
    }
    let (Some(a), Some(p)) = (req.api, provider.effective_protocol()) else {
        return None;
    };
    if a.protocol() == p {
        return None;
    }
    Some(GatewayError::new(
        crate::error::Source::Request,
        msg!(
            "gw.route.protocol_mismatch",
            path = req.uri.path(), wanted = a.slug(),
            upstream = provider.name.clone(), got = p.slug() =>
            "{path} can only be served by a {wanted} upstream, and `{upstream}` is {got}."
        ),
    ))
}

/// 把客户端的请求改成这一跳要发的样子：同格式时只做参数改写，
/// 跨格式时转换。转换不了就换下一家：同格式的上游可能还在后面。
fn prepare(
    state: &AppState,
    req: &Inbound,
    reading: &crate::client_api::Reading,
    provider: &tw_config::Provider,
    effective_set: &tw_engine::SetAction,
    id: u64,
) -> Result<Outbound, Skip> {
    let generates = reading.generates;
    // 方言互转。**同格式时是 None，这一整段零成本**
    let client_dialect = req.api.map(|a| a.dialect());
    let target = crate::translate::plan(req.api, generates, provider.effective_protocol());
    let chatgpt = generates && provider.effective_protocol() == Some(tw_config::Protocol::Chatgpt);
    // DeepSeek Harness 的扩展只有 DeepSeek 官方认。**发给它时一个字节都不改**
    let harness = generates && reading.harness.is_some();
    let to_deepseek = tw_dialect::official::is_deepseek_host(&provider.base_url);
    let mut path = req.uri.path().to_string();
    let mut query = req.query.clone();
    let mut session: Option<tw_dialect::convert::Session> = None;
    let body = match target {
        None => {
            // 参数改写。**只在这里动 body，而且只动被点名的那几个字段** ——
            // 出站直通说过任何 body 改写都可能是缓存杀手，所以这是
            // 一个用户显式要求的例外，不是默认行为。
            let out = forward::apply_set(&req.body, effective_set, client_dialect);
            if let (Some(tw_dialect::ir::Dialect::Gemini), Some(m)) =
                (client_dialect, &effective_set.model)
            {
                path = forward::gemini_path_with_model(&path, m);
            }
            // 对话之前被转换过、这一跳直通时，去掉客户端带回来的转换签名：
            // 这个上游不认，整个请求会被拒
            let out = match client_dialect
                .filter(|_| generates)
                .and_then(|d| tw_dialect::convert::strip_carried(d, &out))
            {
                Some(b) => Bytes::from(b),
                None => out,
            };
            // DeepSeek Harness 发给别家：去掉只有 DeepSeek 认的扩展。**去掉了什么要说**，
            // 和转换丢了字段一样记在这一跳上
            let mut dropped = Vec::new();
            let out = match client_dialect
                .filter(|_| harness && !to_deepseek)
                .and_then(|d| tw_dialect::harness::clean(d, &out))
            {
                Some(c) => {
                    dropped = c.dropped;
                    Bytes::from(c.body)
                }
                None => out,
            };
            let out = if chatgpt {
                // **只动 Codex 后端不认的那几个字段**，其余原样发（见 `chatgpt` 模块）
                let (shaped, more) = crate::chatgpt::shape_passthrough(&out);
                dropped.extend(more);
                shaped
            } else {
                out
            };
            if let Some(d) = client_dialect.filter(|_| !dropped.is_empty()) {
                let same = crate::wire::dialect(d);
                state.bus.emit(tw_api::Event::Translated {
                    id,
                    provider: provider.name.clone(),
                    from: same,
                    to: same,
                    dropped,
                    at_ms: crate::server::now_ms(),
                });
            }
            if chatgpt {
                // 客户端要整包，后端只给流：由网关收齐。收齐要知道客户端的格式，所以要一个会话
                if let Some(Ok(d)) = &reading.decoded
                    && !d.request.stream
                {
                    session = Some(
                        d.clone()
                            .encode(&tw_dialect::ir::Target {
                                dialect: tw_dialect::ir::Dialect::Responses,
                                official: tw_dialect::official::is_official_host(
                                    &provider.base_url,
                                ),
                                default_max_tokens: 0,
                            })
                            .session,
                    );
                }
            }
            out
        }
        Some(dialect) => {
            let d = match &reading.decoded {
                Some(Ok(d)) => d,
                other => {
                    let why = match other {
                        Some(Err(rej)) => rej.0.clone(),
                        _ => "the request body is not valid JSON".to_string(),
                    };
                    return Err(GatewayError::new(
                        crate::error::Source::Request,
                        msg!(
                            "gw.convert.failed", upstream = provider.name.clone(), detail = why =>
                            "The request could not be converted to the format upstream \
                             `{upstream}` speaks: {detail}"
                        ),
                    ));
                }
            };
            let mut d = d.clone();
            crate::translate::apply_set(&mut d.request, effective_set);
            // Codex 后端不认输出上限：带着它发过去是一个 400
            let limit = if chatgpt {
                crate::chatgpt::drop_output_limit(&mut d.request, d.client)
            } else {
                None
            };
            let p = d.encode(&tw_dialect::ir::Target {
                dialect,
                official: tw_dialect::official::is_official_host(&provider.base_url),
                default_max_tokens: crate::translate::default_max_tokens(
                    &state.pricing.load(),
                    &d.request.model,
                ),
            });
            // **转换了就要说一声，丢了字段更要说。**用户会发现「扩展思考开了
            // 却没生效」而完全不知道从哪儿查起
            let mut dropped = p.dropped.clone();
            dropped.extend(limit);
            state.bus.emit(tw_api::Event::Translated {
                id,
                provider: provider.name.clone(),
                from: crate::wire::dialect(d.client),
                to: crate::wire::dialect(dialect),
                dropped,
                at_ms: crate::server::now_ms(),
            });
            path = p.path.clone();
            query = p.query.clone();
            // **客户端要不要流由会话记着**，发给 Codex 后端的这一份一律是流式
            let body = if chatgpt {
                Bytes::from(crate::chatgpt::force_stream(p.body.clone()))
            } else if harness && to_deepseek {
                // 转换成另一种格式发给 DeepSeek 官方：直连时它收得到的扩展照样带上
                Bytes::from(
                    tw_dialect::harness::carry(&req.body, &p.body).unwrap_or(p.body.clone()),
                )
            } else {
                Bytes::from(p.body.clone())
            };
            session = Some(p.session);
            body
        }
    };

    if chatgpt {
        // Codex 后端的生成接口是 `{base}/responses`，不在 `/v1` 下
        path = "/responses".to_string();
        query = None;
    }
    Ok(Outbound {
        body,
        path,
        query,
        target,
        chatgpt,
        harness_elsewhere: harness && !to_deepseek,
        session,
    })
}

/// 发出这一跳。OAuth 上游回 401 时换一个 token 再发一次。
async fn send(
    state: &AppState,
    req: &Inbound,
    provider: &tw_config::Provider,
    http: &reqwest::Client,
    out: &Outbound,
    body: Bytes,
    upstream_headers: Vec<(String, String)>,
) -> Result<reqwest::Response, reqwest::Error> {
    let url = forward::upstream_url(&provider.base_url, &out.path, out.query.as_deref());
    let method = reqwest::Method::from_bytes(b"POST").expect("POST is a valid method");
    let client_dialect = req.api.map(|a| a.dialect());
    let (target, chatgpt, headers) = (out.target, out.chatgpt, &req.headers);
    let required = target
        .map(crate::translate::required_headers)
        .unwrap_or_default();
    // DeepSeek Harness 发给别家：它自己的头不转发；直通时 `anthropic-beta` 去掉对话中途
    // 增删工具那一项（那些块已经去掉了），剩下的照发
    let harness = out.harness_elsewhere;
    let beta = headers
        .get("anthropic-beta")
        .filter(|_| harness && target.is_none())
        .and_then(|v| v.to_str().ok())
        .and_then(tw_dialect::harness::anthropic_beta);
    let build = |upstream_headers: &[(String, String)]| {
        let mut req = http.request(method.clone(), &url);
        req = forward::forward_headers_filtered(req, headers, |n| {
            let own = match (target, client_dialect) {
                (Some(_), Some(c)) => !crate::translate::keeps_header(c, n),
                _ => false,
            };
            // 请求来自谁由网关如实填写：客户端报的来源（比如 Codex CLI 的 originator）
            // 不转发，请求经过的是 ThinkWatch
            let identity = chatgpt && !crate::chatgpt::keeps_client_header(n);
            let dsh = harness
                && (tw_dialect::harness::own_header(n) || n.eq_ignore_ascii_case("anthropic-beta"));
            !own && !identity
                && !dsh
                && !required.iter().any(|(k, _)| k.eq_ignore_ascii_case(n))
                && !forward::overridden(upstream_headers, n)
        });
        if let Some(b) = &beta
            && !forward::overridden(upstream_headers, "anthropic-beta")
        {
            req = req.header("anthropic-beta", b.as_str());
        }
        // 目标格式必需的头（Anthropic 的 anthropic-version）。上游配置里写了同名头时以配置为准
        for (k, v) in required {
            if !forward::overridden(upstream_headers, k) {
                req = req.header(*k, *v);
            }
        }
        req = forward::apply_headers(req, upstream_headers);
        if chatgpt {
            req = forward::apply_headers(
                req,
                &crate::chatgpt::identity_headers(headers, upstream_headers),
            );
        }
        req
    };
    let sent_at = std::time::Instant::now();
    let mut sent = build(&upstream_headers).body(body.clone()).send().await;
    // **OAuth 上游回 401：换一个 access token 再发一次，只一次。**token 可能在别处被
    // 吊销了、提前失效了；不重试的话，这个请求连同之后每一个请求都会原样失败，直到
    // 缓存里那个 token 按时间过期。换回来的还是 401，说明问题不在 token
    if provider.oauth.is_some() && matches!(&sent, Ok(r) if r.status() == 401) {
        match state.headers_after_401(provider, http, sent_at).await {
            // 换回来的还是同一个（刚换过不久）：再发一次也是 401
            Ok(fresh) if fresh != upstream_headers => {
                sent = build(&fresh).body(body).send().await;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(provider = %provider.name, "the upstream answered 401 and the token could not be renewed: {e}")
            }
        }
    }
    sent
}
