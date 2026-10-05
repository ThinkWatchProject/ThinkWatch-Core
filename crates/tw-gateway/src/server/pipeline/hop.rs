//! 管线第 5 步的前半：转发，依次尝试候选上游。
//!
//! **首字节之前可以透明切换** —— 拿到响应头之前我们还没往客户端写过任何
//! 东西，换一家客户端完全无感。
//!
//! 「尝试链」要留下来：用户能看见故障转移在替他工作，**这是信任的来源**。
//! 一个静默切换过的请求和一个一次就成的请求，在用户眼里应该是不同的。
//!
//! 每一跳先过插件的请求钩子（[`super::plug`]）：发往哪一家、发什么模型名这时都定了，
//! 管这一跳的插件从客户端的原话起改，这一跳的转换、脱敏、发送用改过的那一份。换到下一
//! 家时从原话重来；同一家重发（OAuth 换 token、去封存）用这一跳定好的请求体，不重跑。

use bytes::Bytes;

use super::{Inbound, Started};
use crate::error::GatewayError;
use crate::failure::{Cause, Verdict};
use crate::forward;
use crate::server::{hop, hop_failed, note_health};
use crate::state::{AppState, Runtime};
use tw_types::msg;

/// 接下了这个请求的那一家，和回程要用的东西。
pub(super) struct Served<'a> {
    pub(super) upstream: reqwest::Response,
    pub(super) provider: &'a tw_config::Provider,
    /// 发给它的模型名：路由规则、插件改过的是改过之后的。回答钩子的 `ctx.model` 和范围看它；
    /// 和客户端要的不一样时，回答里的模型名按它认、换回客户端的（见 [`crate::answer_model`]）
    pub(super) model: String,
    /// 它是尝试链上的第几跳。回答钩子的运行记录按它分组
    pub(super) attempt: usize,
    /// 这一跳的密钥映射：跑过插件、或者管这一跳的插件里有回答钩子时才有（见
    /// [`super::plug`]）
    pub(super) bridge: Option<crate::plugin::bridge::Bridge>,
    /// 成功那一次的脱敏账本。**必须是成功那一次的** —— 每一跳都接着原文那本账换，
    /// 而那一跳发出去的体（可能转换过格式）里还有原文没有的值时，号是那一跳新发的
    pub(super) ledger: tw_guard::redact::replace::Ledger,
    /// 成功那一跳的转换。**必须是成功那一次的** —— 故障转移从 Anthropic 上游
    /// 切到 OpenAI 上游时，两跳转成的格式不一样；直通时是 None
    pub(super) session: Option<tw_dialect::convert::Session>,
    /// 交出去的是网关替上游说的一句话，不是它的原话（Bedrock 拒绝凭证，见
    /// [`bedrock_refusal`]）：这个请求失败的原因就是这一句。别的都是 None
    pub(super) refusal: Option<tw_types::Msg>,
}

/// 这个请求的着落。
pub(super) enum Answer<'a> {
    /// 一家上游接下了它
    Served(Box<Served<'a>>),
    /// 数 token 的请求由网关本地估算（见 [`crate::count`]）：要回给客户端的正文
    Estimated(Bytes),
}

/// 这一跳的客户端那种格式的请求：插件在这一跳改过的话是改过的那一份，没改过就是
/// 客户端的原话。转换、参数改写都从它起。
struct Asked<'r> {
    body: &'r Bytes,
    path: &'r str,
    decoded: Option<&'r Result<tw_dialect::convert::Decoded, tw_dialect::ir::Rejection>>,
}

impl<'r> Asked<'r> {
    fn of(
        req: &'r Inbound,
        reading: &'r crate::client_api::Reading,
        plugged: &'r super::plug::Plugged,
    ) -> Self {
        match &plugged.rewritten {
            Some(r) => Asked {
                body: &r.body,
                path: &r.path,
                decoded: r.decoded.as_ref(),
            },
            None => Asked {
                body: &req.body,
                path: req.uri.path(),
                decoded: reading.decoded.as_ref(),
            },
        }
    }
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
    /// 这一跳发哪些请求头（见 [`crate::egress`]）
    hop: crate::egress::Hop,
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
    hook: &mut crate::plugin::request::Hook<'_>,
) -> Result<Answer<'a>, GatewayError> {
    let id = started.id;
    // 数 token（见 `crate::count`）：选中的那一家数不了就由网关估，**不换模型**
    let counting = crate::client_api::ClientApi::counts_tokens(req.uri.path());
    // 数 token 第一跳要的模型（规则改写过的是改写后的）。往下只换要同一个模型的
    let mut count_model: Option<Option<String>> = None;
    // 由网关估了数的那一家
    let mut estimated: Option<&tw_config::Provider> = None;
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
    // 不再试下一家的原因：第二阶段拒绝了、规则求不了值、插件拒绝了。**路由事件照样要发**
    let mut halt: Option<GatewayError> = None;
    // 最后发出去的那一跳，插件改过的话改过之后的请求和那一跳的账：存下来的「插件改过的
    // 请求」就是它 —— 回答的那一家收到的那一份
    let mut after_plugins: Option<(Bytes, tw_guard::redact::replace::Ledger)> = None;
    // 别名对到每一家时看的清单：整个请求用同一份
    let catalog = state.catalog.load();

    for (i, name) in started.alive.iter().enumerate() {
        // 后面没有别的候选了
        let last = i + 1 == started.alive.len();
        let Some(provider) = rt.config.providers.iter().find(|p| &p.name == name) else {
            // 校验时挡过一次，能到这儿说明配置在运行中被换过。
            last_err = Some(GatewayError::config(msg!(
                "gw.route.selected_upstream_missing",
                rule = decision.matched_rule.clone(), upstream = name =>
                "Rule `{rule}` selected upstream `{upstream}`, which is not in the configuration."
            )));
            continue;
        };
        // 数 token 选中的那一家失败了（5xx、429、连不上）：往下只换同格式的上游。
        // **别的格式的那几家不估**：选中的那一家出了错，报出来的该是这个错
        if counting && !attempts.is_empty() && !same_format(req, provider) {
            continue;
        }
        attempts.push(provider.name.clone());
        let hop_started = std::time::Instant::now();

        // 数 token 选中的是别的格式的上游：不发，网关自己估
        if counting && attempts.len() == 1 && estimates(req, provider) {
            chain.push(estimated_hop(provider, None, None, hop_started));
            estimated = Some(provider);
            break;
        }
        if let Some(err) = protocol_mismatch(req, reading.generates, provider) {
            chain.push(hop_failed(
                &provider.name,
                None,
                err.detail.clone(),
                hop_started,
            ));
            last_err = Some(err);
            continue;
        }

        // 阶段二：知道走哪家了，再跑一遍含 `provider_would_be` 的规则。
        //
        // **在循环里面，因为故障转移换了 provider 之后必须重算**。
        // 否则「走中转的一律脱敏」这条规则，在从官方转移到中转时会漏掉
        // —— 而那正是最需要它的时刻。
        let (mut effective_set, renamed) = match rt.engine.phase_two(
            &reading.facts,
            &provider.name,
            &decision.set,
        ) {
            Ok(tw_engine::Outcome2::Proceed {
                set,
                rewritten_by: more,
                model,
            }) => {
                for r in more {
                    if !rewritten_by.contains(&r) {
                        rewritten_by.push(r);
                    }
                }
                (set, model)
            }
            Ok(tw_engine::Outcome2::Deny { rule, reason }) => {
                tracing::info!(%rule, provider = %provider.name, "a phase-two rule denied the request");
                let err = GatewayError::denied(msg!(
                    "gw.route.denied", rule = rule.clone(), reason = reason =>
                    "Rule `{rule}` denied this request: {reason}"
                ));
                // 被拒的这一跳没有发出去。**它在尝试链上**，原因就是那条拒绝 ——
                // 链上看得出请求本来要去哪家、在哪一步停下的
                chain.push(hop_failed(
                    &provider.name,
                    None,
                    err.detail.clone(),
                    hop_started,
                ));
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

        // 这一家要的模型（见 `Engine::asked_of`）和发给它的名字：客户端那一侧的名称（客户端
        // 写的、阶段一改写的）按别名表对到**这一家**自己的名称，故障转移换一家就重新对；
        // 指定的模型、阶段二改的名字原样发出（见 `crate::sent`）
        let asked_model =
            rt.engine
                .asked_of(&reading.facts, decision, &provider.name, renamed.as_deref());
        let Some(sent) = crate::sent::name(&rt.config, &catalog, provider, &asked_model) else {
            // 别名列的名字这一家一个都没有。路由时已经跳过了这样的候选，能到这儿说明它的
            // 清单刚刚换过：**不把别名原样发给它**，换下一家
            let err = GatewayError::new(
                crate::error::Source::Request,
                msg!(
                    "gw.model.alias_not_served",
                    upstream = provider.name.clone(), alias = asked_model.model.clone() =>
                    "Upstream `{upstream}` offers none of the models that alias {alias} lists."
                ),
            );
            chain.push(hop_failed(
                &provider.name,
                None,
                err.detail.clone(),
                hop_started,
            ));
            last_err = Some(err);
            continue;
        };
        // 和客户端写的一样就不动请求体里的模型名（改写过又对回来的也一样）
        effective_set.model = (sent != reading.facts.model).then(|| sent.clone());
        // 改写过、和客户端要的不一样的才记（见 `AttemptView::model`）
        let asked_other = |m: &String| *m != reading.facts.model;
        // 数 token 不换模型：另一个模型的 tokenizer 数出来的不是这个数。**比的是要的模型**：
        // 同一个别名在各家名字不同，是同一个模型
        if counting {
            let model = Some(asked_model.model.clone()).filter(asked_other);
            match &count_model {
                None => count_model = Some(model),
                Some(first) if *first != model => {
                    attempts.pop();
                    continue;
                }
                Some(_) => {}
            }
        }

        // 插件的请求钩子：管这一跳的从客户端的原话起改。**拒绝的是整个请求**，不换下一家
        let plugged = match super::plug::attempt(
            state,
            rt,
            req,
            reading,
            started,
            hook,
            provider,
            &sent,
            chain.len(),
        )
        .await
        {
            Ok(p) => p,
            Err(why) => {
                // 这一跳没有发出去。**它在尝试链上**，原因就是拒绝它的那句话
                chain.push(hop_failed(
                    &provider.name,
                    Some(sent.clone()).filter(asked_other),
                    why.clone(),
                    hop_started,
                ));
                halt = Some(GatewayError::denied(why));
                break;
            }
        };
        // 插件换了发给这一家的模型名：和规则改写的一样，只是盖过它
        if let Some(m) = &plugged.model {
            effective_set.model = Some(m.clone());
        }
        let model = Some(plugged.model.clone().unwrap_or(sent)).filter(asked_other);

        let asked = Asked::of(req, reading, &plugged);
        let out = match prepare(state, req, reading, &asked, provider, &effective_set, id) {
            Ok(out) => out,
            Err(err) => {
                chain.push(hop_failed(
                    &provider.name,
                    model.clone(),
                    err.detail.clone(),
                    hop_started,
                ));
                last_err = Some(err);
                continue;
            }
        };

        // 这一家在这段对话里拒过的别家封存的推理：发之前先去掉（见 `crate::seal`）
        let unsealed = unseal_upfront(state, req, started, provider, &out);
        // 出站脱敏的拦截档：换掉**这一跳真正发出去的那一份**（可能转换过
        // 格式）。规则是全局的，每一跳换掉的是同一批东西；**接着原文那本账换**，
        // 同一个值在每一跳、在存下来的那份请求里都是同一个占位符。插件改过的一跳接着
        // 插件那本账（插件写进来的新值在那里编好了号）
        let seed = plugged
            .rewritten
            .as_ref()
            .map_or(&started.ledger, |r| &r.ledger);
        let (body, ledger) =
            crate::guard::replace(rt.config.security.redact.mode, &rt.redact, unsealed, seed);

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
                chain.push(hop_failed(
                    &provider.name,
                    model.clone(),
                    err.detail.clone(),
                    hop_started,
                ));
                last_err = Some(err);
                continue;
            }
        };

        // Bedrock 的访问密钥：每个请求签名要用。**和请求头在同一处取**：环境变量没设是
        // 这一家的问题，换下一家
        let aws = match provider.aws_credentials() {
            Ok(c) => c,
            Err(e) => {
                note_health(
                    &state.bus,
                    &state.health,
                    &provider.name,
                    state.health.record_failure(&provider.name),
                );
                let err =
                    GatewayError::config(crate::state::credential_failed(e.msg(), &provider.name));
                chain.push(hop_failed(
                    &provider.name,
                    model.clone(),
                    err.detail.clone(),
                    hop_started,
                ));
                last_err = Some(err);
                continue;
            }
        };

        tracing::debug!(
            client = %req.client_name,
            provider = %provider.name,
            rule = %decision.matched_rule,
            group = ?decision.via_group,
            url = %tw_secret::redact_url(&hop_url(provider, &out)),
            attempt = attempts.len(),
            "forwarding"
        );
        // 这一跳要发出去了：插件改过的话，它收到的就是改过的那一份
        after_plugins = plugged
            .rewritten
            .as_ref()
            .map(|r| (r.body.clone(), ledger.clone()));
        // 这一跳接下了的话，回答钩子要的
        let sent_model = effective_set
            .model
            .clone()
            .unwrap_or_else(|| reading.facts.model.clone());
        let (attempt, bridge) = (chain.len(), plugged.bridge);

        let sent = send(
            state,
            req,
            provider,
            http,
            &out,
            body.clone(),
            upstream_headers.clone(),
            aws.as_ref(),
        )
        .await;
        // 上游拒绝了别家封存的推理：去掉它们，同一家再发一次
        let sent = match sent {
            Ok(r) => {
                let resend = Resend {
                    out: &out,
                    body: &body,
                    headers: &upstream_headers,
                    aws: aws.as_ref(),
                    conversation: started.conversation.as_deref(),
                };
                resend_unsealed(state, req, provider, http, resend, r).await
            }
            Err(e) => Err(e),
        };
        match sent {
            Ok(r) if !r.status().is_success() => {
                let status = r.status().as_u16();
                // 同格式的上游没实现数 token（不少中转只做了生成回答）：网关自己估。
                // **在判断换不换之前**：404 平时是「这家没有这个模型」，要换下一家
                if counting && crate::count::unsupported(status) {
                    note_health(
                        &state.bus,
                        &state.health,
                        &provider.name,
                        state.health.record_success(&provider.name),
                    );
                    chain.push(estimated_hop(
                        provider,
                        model.clone(),
                        Some(status),
                        hop_started,
                    ));
                    estimated = Some(provider);
                    break;
                }
                let headers = r.headers().clone();
                let (head, r) = peek_body(r).await;
                // GLM Coding Plan 的额度用完不在响应头里，在 429 的 body 里
                if status == 429 {
                    state.note_glm_429_body(id, provider, &head);
                }
                let verdict = crate::failure::classify(status, &headers, &head, now_ms());
                // **最后一家的 4xx 交给客户端**：没有下一家可换了，上游自己的原话
                // （「invalid x-api-key」）比我们转述的一句「回了 401」有用得多。
                // 5xx 和 429 照旧报我们的话，带着尝试链
                let hand_on = last && status < 500 && status != 429;
                match verdict {
                    Verdict::Failed(cause) if !hand_on => {
                        // 5xx、限流、没钱了、额度用完、凭据被拒、没有这个模型：换一家有
                        // 意义，那边是另一把密钥、另一个账户。停用多久看原因。
                        //
                        // 交给客户端的那一跳，下面这几样由回程（`relay`）去记
                        //
                        // 额度用完时上游回的正是 429，这一跳的额度头也要读
                        state.note_quota(id, &provider.name, &headers);
                        // 上游回了话，说明代理是通的
                        state.note_proxy_ok(&provider.proxy);
                        state.glm_traffic(provider);
                        // 凭据被拒是要让用户知道的状态，**换了下一家也一样**
                        state.note_auth(&provider.name, status);
                        let cause = known_reset(state, &provider.name, cause);
                        note_health(
                            &state.bus,
                            &state.health,
                            &provider.name,
                            state.health.record_cause(&provider.name, cause),
                        );
                        chain.push(hop(
                            &provider.name,
                            model.clone(),
                            tw_api::AttemptOutcome::Status,
                            status,
                            hop_started,
                        ));
                        // **429 要保住 429。**塌成 502 的话，客户端会当成「服务器
                        // 坏了」而不是「该退避了」，而它们该做的事完全不同。
                        last_err = Some(if status == 429 {
                            GatewayError::rate_limited(msg!(
                                "gw.upstream.rate_limited", upstream = provider.name.clone() =>
                                "Upstream `{upstream}` rate-limited the request."
                            ))
                        } else {
                            GatewayError::upstream(msg!(
                                "gw.upstream.status", upstream = provider.name.clone(), status = status =>
                                "Upstream `{upstream}` answered {status}."
                            ))
                        });
                        continue;
                    }
                    verdict => {
                        // 请求本身的问题（换一家也一样被拒），或者最后一家的 4xx：原样交出去
                        let change = match verdict {
                            Verdict::Failed(cause) => state.health.record_cause(
                                &provider.name,
                                known_reset(state, &provider.name, cause),
                            ),
                            Verdict::ClientError => state.health.record_success(&provider.name),
                        };
                        note_health(&state.bus, &state.health, &provider.name, change);
                        // Bedrock 拒绝凭证时的原话会点名账号和 IAM 身份：换成我们自己的话再交出去
                        let (r, refusal) = if provider.is_bedrock() && matches!(status, 401 | 403) {
                            let (r, ours) = bedrock_refusal(provider, r).await;
                            (r, Some(ours))
                        } else {
                            (r, None)
                        };
                        chain.push(hop(
                            &provider.name,
                            model.clone(),
                            tw_api::AttemptOutcome::Served,
                            status,
                            hop_started,
                        ));
                        served = Some(Served {
                            upstream: r,
                            provider,
                            model: sent_model,
                            attempt,
                            bridge,
                            ledger,
                            session: out.session,
                            refusal,
                        });
                        break;
                    }
                }
            }
            Ok(r) => {
                // 流式回答：**第一段内容到之前**上游在流里报的错，照样换下一家（见
                // `opening`）。最后一家不等 —— 没有下一家可换，等只会让客户端晚一点
                // 看到同一个错误
                let r = match opening_of(req, provider, &out, &r).filter(|_| !last) {
                    None => r,
                    Some((dialect, eventstream)) => {
                        let wait = std::time::Duration::from_secs(
                            rt.config.failover.stream_start_wait_secs,
                        );
                        match super::opening::watch(r, dialect, eventstream, wait).await {
                            super::opening::Opening::Go(r) => r,
                            super::opening::Opening::Failed {
                                status,
                                headers,
                                body,
                                kind,
                                message,
                                response,
                            } => {
                                match crate::failure::classify(status, &headers, &body, now_ms()) {
                                    Verdict::ClientError => response,
                                    Verdict::Failed(cause) => {
                                        state.note_quota(id, &provider.name, &headers);
                                        state.note_proxy_ok(&provider.proxy);
                                        let cause = known_reset(state, &provider.name, cause);
                                        note_health(
                                            &state.bus,
                                            &state.health,
                                            &provider.name,
                                            state.health.record_cause(&provider.name, cause),
                                        );
                                        let said = msg!(
                                            "gw.upstream.stream_opening_error",
                                            upstream = provider.name.clone(), kind = kind, message = message =>
                                            "Upstream `{upstream}` started the answer and reported an error \
                                             before any content ({kind}): {message}"
                                        );
                                        let err = if status == 429 {
                                            GatewayError::rate_limited(said)
                                        } else {
                                            GatewayError::upstream(said)
                                        };
                                        chain.push(hop_failed(
                                            &provider.name,
                                            model.clone(),
                                            err.detail.clone(),
                                            hop_started,
                                        ));
                                        last_err = Some(err);
                                        continue;
                                    }
                                }
                            }
                            super::opening::Opening::Broken(err) => {
                                note_health(
                                    &state.bus,
                                    &state.health,
                                    &provider.name,
                                    state.health.record_failure(&provider.name),
                                );
                                chain.push(hop_failed(
                                    &provider.name,
                                    model.clone(),
                                    err.detail.clone(),
                                    hop_started,
                                ));
                                last_err = Some(err);
                                continue;
                            }
                        }
                    }
                };
                note_health(
                    &state.bus,
                    &state.health,
                    &provider.name,
                    state.health.record_success(&provider.name),
                );
                chain.push(hop(
                    &provider.name,
                    model.clone(),
                    tw_api::AttemptOutcome::Served,
                    r.status().as_u16(),
                    hop_started,
                ));
                served = Some(Served {
                    upstream: r,
                    provider,
                    model: sent_model,
                    attempt,
                    bridge,
                    ledger,
                    session: out.session,
                    refusal: None,
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
                let err = match e {
                    SendError::Http(e) => {
                        // 连不上的可能是代理而不是上游 —— 检一次那个代理，说清是哪一件事
                        state.check_proxy(&provider.proxy);
                        forward::map_reqwest_error(e)
                    }
                    SendError::Sign(e) => GatewayError::config(msg!(
                        "gw.upstream.sign_failed",
                        upstream = provider.name.clone(), detail = e.0 =>
                        "The request to upstream `{upstream}` could not be signed with its AWS \
                         access keys: {detail}"
                    )),
                };
                chain.push(hop_failed(
                    &provider.name,
                    model.clone(),
                    err.detail.clone(),
                    hop_started,
                ));
                last_err = Some(err);
                continue;
            }
        }
    }

    // 尝试链走完了，两条路都要发 —— 挂在 RequestFinished 上的话，
    // 失败那条路就没有尝试链，而那恰恰是最需要看它的时候。
    // 最终服务的那家怎么收钱。**跟着请求走，不能事后查配置** ——
    // 配置随时会被热重载，而一条三天前的记录该按它当时那家的算。
    // 网关估的数不花钱
    let billing = match (&served, estimated) {
        (_, Some(_)) => tw_config::Billing::Free,
        (Some(s), None) => s.provider.billing,
        (None, None) => Default::default(),
    };
    // 插件改过的请求：最后发出去的那一跳收到的那一份（回答的那一家收到的就是它）。
    // 挂在请求那一行上，落盘前按那一跳的账换、打码
    if let Some((body, ledger)) = after_plugins {
        let len = body.len();
        crate::bodies::offer(
            &state.body_sink(),
            crate::bodies::BodyRecord::new(
                id,
                started.at_ms as i64,
                crate::bodies::BodyKind::AfterPlugins,
                body,
                len,
                crate::bodies::Redaction {
                    rules: rt.redact.clone(),
                    ledger,
                },
            ),
        );
    }
    let choice = &started.choice;
    state.bus.emit(tw_api::Event::RequestRouted {
        id,
        route: choice.route.clone(),
        rule: choice.rule.clone(),
        group: choice.group.clone(),
        rewritten_by,
        denied_by,
        affinity: choice.affinity.clone(),
        attempts: chain,
        billing: billing.into(),
    });
    if let Some(err) = halt {
        return Err(err);
    }
    if let Some(provider) = estimated {
        tracing::debug!(provider = %provider.name, "estimated the token count locally");
        let client = req
            .api
            .map(|a| a.dialect())
            .unwrap_or(tw_dialect::ir::Dialect::Anthropic);
        return Ok(Answer::Estimated(Bytes::from(crate::count::answer(
            client,
            req.uri.path(),
            &req.body,
        ))));
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
    Ok(Answer::Served(Box::new(served)))
}

/// 这一家和客户端是同一种格式（配置里没写格式的也算：照原样发过去）
fn same_format(req: &Inbound, provider: &tw_config::Provider) -> bool {
    match (req.api, provider.effective_protocol()) {
        (Some(a), Some(p)) => a.protocol() == p,
        _ => true,
    }
}

/// 数 token 选中了这一家，由网关自己估：它不是客户端那种格式。
///
/// **Anthropic 的数 token 到了 Bedrock 上游例外**，回 501（见 [`protocol_mismatch`]）：
/// Claude Code 认那个回答，会自己去数一个准的。
fn estimates(req: &Inbound, provider: &tw_config::Provider) -> bool {
    !same_format(req, provider)
        && !(req.api == Some(crate::client_api::ClientApi::AnthropicMessages)
            && provider.effective_protocol() == Some(tw_config::Protocol::Bedrock))
}

/// 网关估了数的那一跳。`status` 是上游回的（它没实现数 token），没发出去的没有
fn estimated_hop(
    provider: &tw_config::Provider,
    model: Option<String>,
    status: Option<u16>,
    started: std::time::Instant,
) -> tw_api::AttemptView {
    tw_api::AttemptView {
        provider: provider.name.clone(),
        model,
        outcome: tw_api::AttemptOutcome::Estimated,
        status,
        error: None,
        ms: started.elapsed().as_millis() as u64,
    }
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
    // 数 token 到了 Bedrock 上游：501 `not_supported`，客户端会改用别的办法数（见
    // `Source::NotSupported`）。**同样不发出去**
    if p == tw_config::Protocol::Bedrock
        && a == crate::client_api::ClientApi::AnthropicMessages
        && crate::client_api::ClientApi::counts_tokens(req.uri.path())
    {
        return Some(GatewayError::new(
            crate::error::Source::NotSupported,
            msg!(
                "gw.count_tokens.bedrock_upstream", upstream = provider.name.clone() =>
                "Counting tokens is not available through AWS Bedrock upstream `{upstream}`."
            ),
        ));
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

/// 请求强制要用、换成这一家的格式之后却没有了的工具。
///
/// Claude Code 搜网页时单独发一个请求，只带服务端工具 `web_search`，并且强制模型用它。服务端
/// 工具只有它所属的那一家执行得了，换格式时被丢掉 —— 照样发出去，模型不搜索，直接回一段话，
/// 而 Claude Code 把这段话当成搜索结果交给模型。**这样的一跳不发**：换下一家，同格式的上游
/// 可能还在后面；都不行，客户端收到的是说得清的这一句，而不是一份编出来的结果。
///
/// 只管强制的。`auto` 时这类工具只是顺带挂着（有的客户端每个请求都带），丢掉之后模型照常回答。
fn unsendable_tool(
    request: &tw_dialect::ir::Request,
    dropped: &[String],
    upstream: &str,
) -> Option<GatewayError> {
    use tw_dialect::ir::ToolChoice;
    let msg = match request.tool_choice.as_ref()? {
        ToolChoice::Named(tool) if !request.tools.iter().any(|t| t.name == *tool) => msg!(
            "gw.convert.tool_unsendable", tool = tool.clone(), upstream = upstream =>
            "The request requires the tool `{tool}`, which cannot be sent to upstream `{upstream}` \
             in its format. Server-side tools such as web search are carried out only by the \
             provider they belong to."
        ),
        ToolChoice::Required
            if request.tools.is_empty() && dropped.iter().any(|p| p.starts_with("tools.")) =>
        {
            msg!(
                "gw.convert.tools_unsendable", upstream = upstream =>
                "The request requires a tool call, and none of its tools can be sent to upstream \
                 `{upstream}` in its format. Server-side tools such as web search are carried out \
                 only by the provider they belong to."
            )
        }
        _ => return None,
    };
    Some(GatewayError::new(crate::error::Source::Request, msg))
}

/// 把这一跳的请求（客户端那种格式，插件改过的话是改过的，见 [`Asked`]）改成要发的
/// 样子：同格式时只做参数改写，跨格式时转换。转换不了就换下一家：同格式的上游可能
/// 还在后面。
fn prepare(
    state: &AppState,
    req: &Inbound,
    reading: &crate::client_api::Reading,
    asked: &Asked<'_>,
    provider: &tw_config::Provider,
    effective_set: &tw_engine::SetAction,
    id: u64,
) -> Result<Outbound, Skip> {
    let generates = reading.generates;
    // 方言互转。**同格式时是 None，这一整段零成本**
    let client_dialect = req.api.map(|a| a.dialect());
    let target = crate::translate::plan(req.api, generates, provider.effective_protocol());
    let chatgpt = generates && provider.effective_protocol() == Some(tw_config::Protocol::Chatgpt);
    // DeepSeek Harness 的扩展只有 DeepSeek 官方认。**发给它时一个字节都不改**。
    // 不止生成请求：数 token 这样的请求一样带着它的头，也可能带着会话日志
    let harness = reading.harness.is_some();
    let to_deepseek = tw_dialect::official::is_deepseek_host(&provider.base_url);
    let mut path = asked.path.to_string();
    let mut query = req.query.clone();
    let mut session: Option<tw_dialect::convert::Session> = None;
    let body = match target {
        None => {
            // 参数改写。**只在这里动 body，而且只动被点名的那几个字段** ——
            // 出站直通说过任何 body 改写都可能是缓存杀手，所以这是
            // 一个用户显式要求的例外，不是默认行为。
            let out = forward::apply_set(asked.body, effective_set, client_dialect);
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
            } else if provider.forward_client_identity {
                out
            } else {
                // 客户端自动填的身份字段不发（见 `egress` 模块）。**不算丢弃的字段**
                client_dialect
                    .and_then(|d| crate::egress::strip_body_identity(d, &out))
                    .unwrap_or(out)
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
                if let Some(Ok(d)) = asked.decoded
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
            let d = match asked.decoded {
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
            // 请求强制要用的工具发不过去：这一跳不发，换下一家（见 `unsendable_tool`）
            if let Some(err) = unsendable_tool(&d.request, &p.dropped, &provider.name) {
                return Err(err);
            }
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
            // Bedrock 上的 Claude：客户端 `anthropic-beta` 里 Bedrock 认的那几个放进请求体
            let claude_on_bedrock = dialect == tw_dialect::ir::Dialect::Bedrock
                && d.client == tw_dialect::ir::Dialect::Anthropic
                && d.request
                    .model
                    .to_ascii_lowercase()
                    .contains("anthropic.claude");
            let betas = if claude_on_bedrock {
                tw_bedrock::beta::supported(
                    req.headers
                        .get_all("anthropic-beta")
                        .iter()
                        .filter_map(|v| v.to_str().ok()),
                )
            } else {
                Vec::new()
            };
            // **客户端要不要流由会话记着**，发给 Codex 后端的这一份一律是流式
            let body = if chatgpt {
                Bytes::from(crate::chatgpt::force_stream(p.body.clone()))
            } else if let Some(b) = tw_bedrock::beta::with_betas(&p.body, &betas) {
                Bytes::from(b)
            } else if harness && to_deepseek {
                // 转换成另一种格式发给 DeepSeek 官方：直连时它收得到的扩展照样带上
                Bytes::from(
                    tw_dialect::harness::carry(asked.body, &p.body).unwrap_or(p.body.clone()),
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
    let hop = crate::egress::Hop {
        protocol: crate::egress::Hop::protocol_for(provider.effective_protocol(), client_dialect),
        translated: target.is_some(),
        client_identity: provider.forward_client_identity && !chatgpt,
        harness_to_deepseek: harness && to_deepseek,
        harness_elsewhere: harness && !to_deepseek,
    };
    Ok(Outbound {
        body,
        path,
        query,
        target,
        chatgpt,
        hop,
        session,
    })
}

/// 这一跳发出去的那一份是什么格式：转换过的是目标格式，直通的是客户端的
fn wire(req: &Inbound, out: &Outbound) -> Option<tw_dialect::ir::Dialect> {
    out.target.or(req.api.map(|a| a.dialect()))
}

/// 这一跳要发的请求体，去掉这一家在这段对话里拒过的封存（见 [`crate::seal`]）。
/// 没拒过的原样发
fn unseal_upfront(
    state: &AppState,
    req: &Inbound,
    started: &Started,
    provider: &tw_config::Provider,
    out: &Outbound,
) -> Bytes {
    let refused = wire(req, out).zip(
        started
            .conversation
            .as_deref()
            .and_then(|c| state.seals.of(c, &provider.name)),
    );
    match refused.and_then(|(w, only)| crate::seal::strip(w, &out.body, Some(&only))) {
        Some((body, removed)) => {
            tracing::debug!(
                provider = %provider.name,
                removed = removed.len(),
                "left out the reasoning this upstream refused earlier in the conversation"
            );
            Bytes::from(body)
        }
        None => out.body.clone(),
    }
}

/// 重发一次要用的：这一跳原来发的那些
struct Resend<'a> {
    out: &'a Outbound,
    body: &'a Bytes,
    headers: &'a [(String, String)],
    aws: Option<&'a tw_bedrock::Credentials>,
    conversation: Option<&'a str>,
}

/// 上游回 400、拒绝了请求里别家封存的推理（见 [`crate::seal`]）：去掉**全部**封存的
/// 推理，同一家再发一次，只一次。拒过哪些记下来，这段对话往后发给它之前先去掉。
///
/// **不换一家**：换到哪一家都可能是另一个账号，同样解不开；而这一家好好的，只是
/// 不认那几段笔记。不是这种 400 的原样交回去。
async fn resend_unsealed(
    state: &AppState,
    req: &Inbound,
    provider: &tw_config::Provider,
    http: &reqwest::Client,
    resend: Resend<'_>,
    r: reqwest::Response,
) -> Result<reqwest::Response, SendError> {
    let Some(wire) = wire(req, resend.out) else {
        return Ok(r);
    };
    if r.status() != 400 {
        return Ok(r);
    }
    let Some((stripped, removed)) = crate::seal::strip(wire, resend.body, None) else {
        return Ok(r);
    };
    let status = r.status();
    let headers = r.headers().clone();
    let said = r.bytes().await.map_err(SendError::Http)?;
    if !crate::seal::refusal(wire, &said) {
        let mut back = http::Response::new(said);
        *back.status_mut() = status;
        *back.headers_mut() = headers;
        return Ok(reqwest::Response::from(back));
    }
    if let Some(c) = resend.conversation {
        state
            .seals
            .note(c, &provider.name, &removed, crate::server::now_ms());
    }
    tracing::info!(
        provider = %provider.name,
        removed = removed.len(),
        "the upstream refused reasoning sealed by another account; sending again without it"
    );
    send(
        state,
        req,
        provider,
        http,
        resend.out,
        Bytes::from(stripped),
        resend.headers.to_vec(),
        resend.aws,
    )
    .await
}

/// 这一跳发往的完整地址。
///
/// Bedrock 的模型 id 写在路径里，按 AWS 的规矩转义（ARN 里的 `/`），见
/// [`tw_bedrock::endpoint::runtime_url`]。
fn hop_url(provider: &tw_config::Provider, out: &Outbound) -> String {
    if provider.is_bedrock() {
        tw_bedrock::endpoint::runtime_url(&provider.base_url, &out.path)
    } else {
        forward::upstream_url(&provider.base_url, &out.path, out.query.as_deref())
    }
}

/// 一跳没发出去的原因。
enum SendError {
    Http(reqwest::Error),
    /// Bedrock 的请求没签成名
    Sign(tw_bedrock::SignError),
}

/// 发出这一跳。OAuth 上游回 401 时换一个 token 再发一次。
///
/// `aws` 是 Bedrock 的访问密钥：有的话，请求在**全部定稿之后**（地址、请求头、请求体）
/// 最后一步签名，签的就是 reqwest 实际要发的那个地址。上游自己的请求头里带着 Bedrock
/// API Key 时不签。
#[allow(clippy::too_many_arguments)]
async fn send(
    state: &AppState,
    req: &Inbound,
    provider: &tw_config::Provider,
    http: &reqwest::Client,
    out: &Outbound,
    body: Bytes,
    upstream_headers: Vec<(String, String)>,
    aws: Option<&tw_bedrock::Credentials>,
) -> Result<reqwest::Response, SendError> {
    let url = hop_url(provider, out);
    let method = reqwest::Method::from_bytes(b"POST").expect("POST is a valid method");
    let (target, chatgpt, hop, headers) = (out.target, out.chatgpt, out.hop, &req.headers);
    let required = target
        .map(crate::translate::required_headers)
        .unwrap_or_default();
    // DeepSeek Harness 发给别家：直通时 `anthropic-beta` 去掉对话中途增删工具那一项（那些块
    // 已经去掉了），剩下的照发。分几行发的也要都看：客户端原来的不发，只发这一份
    let beta = (hop.harness_elsewhere && target.is_none())
        .then(|| {
            headers
                .get_all("anthropic-beta")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .collect::<Vec<_>>()
                .join(",")
        })
        .and_then(|v| tw_dialect::harness::anthropic_beta(&v));
    // Anthropic 格式原样直通、客户端没写 `anthropic-version`：Anthropic 要这个头，补上默认的
    let version = (hop.protocol == Some(tw_config::Protocol::Anthropic)
        && target.is_none()
        && !headers.contains_key("anthropic-version"))
    .then_some(("anthropic-version", tw_dialect::official::ANTHROPIC_VERSION));
    let gateway = crate::egress::gateway_headers(&hop, headers);
    // Codex 后端收 zstd 压缩的请求体（见 `chatgpt::compress`）
    let (body, encoding) = match chatgpt.then(|| crate::chatgpt::compress(&body)).flatten() {
        Some(z) => (z, Some("zstd")),
        None => (body, None),
    };
    let build = |upstream_headers: &[(String, String)]| {
        let mut req = http.request(method.clone(), &url);
        // 从客户端取的：只有这种上游的协议要的那几个（白名单，见 `egress` 模块）
        for (name, value) in headers {
            let n = name.as_str();
            if crate::egress::takes_from_client(&hop, n)
                && !required.iter().any(|(k, _)| k.eq_ignore_ascii_case(n))
                && !forward::overridden(upstream_headers, n)
            {
                req = req.header(name.clone(), value.clone());
            }
        }
        if let Some(b) = &beta
            && !forward::overridden(upstream_headers, "anthropic-beta")
        {
            req = req.header("anthropic-beta", b.as_str());
        }
        // 目标格式必需的头（Anthropic 的 anthropic-version）。上游配置里写了同名头时以配置为准
        for (k, v) in required.iter().copied().chain(version) {
            if !forward::overridden(upstream_headers, k) {
                req = req.header(k, v);
            }
        }
        // 网关自己填的：User-Agent，和转换过的请求体的类型。上游配置里写了同名头时以配置为准
        for (k, v) in &gateway {
            if !forward::overridden(upstream_headers, k) {
                req = req.header(k.as_str(), v.as_str());
            }
        }
        req = forward::apply_headers(req, upstream_headers);
        if chatgpt {
            req = forward::apply_headers(
                req,
                &crate::chatgpt::identity_headers(headers, upstream_headers),
            );
        }
        if let Some(e) = encoding {
            req = req.header(http::header::CONTENT_ENCODING, e);
        }
        req
    };
    let sent_at = std::time::Instant::now();
    let signer = aws
        .filter(|_| !tw_bedrock::carries_api_key(upstream_headers.iter().map(|(k, _)| k.as_str())))
        .map(|c| (c, provider.bedrock_region().unwrap_or_default()));
    if let Some((credentials, region)) = signer {
        let mut request = build(&upstream_headers)
            .body(body.clone())
            .build()
            .map_err(SendError::Http)?;
        crate::bedrock::sign_request(&mut request, credentials, region).map_err(SendError::Sign)?;
        return http.execute(request).await.map_err(SendError::Http);
    }
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
    sent.map_err(SendError::Http)
}

/// Bedrock 拒绝了凭证（401/403）：状态码和响应头照原样，正文换成我们自己的一句话。
/// 交回换好的响应和那句话 —— 请求记录里失败的原因也是它。
///
/// **AWS 的原话会点名账号 ID 和 IAM 身份**（`User: arn:aws:iam::…` is not authorized…），
/// 那不能交给客户端，也不能进请求记录。能说的是异常名：凭证被拒、过期、没有权限，是
/// 哪一种由它分开。
async fn bedrock_refusal(
    provider: &tw_config::Provider,
    r: reqwest::Response,
) -> (reqwest::Response, tw_types::Msg) {
    let status = r.status();
    let mut headers = r.headers().clone();
    let named = headers
        .get(tw_bedrock::error::ERROR_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = r.bytes().await.unwrap_or_default();
    let kind = tw_bedrock::error::kind_of(named.as_deref(), &body);
    let profile = provider.aws.as_ref().and_then(|a| a.profile.clone());
    let text = match (kind.as_deref(), profile) {
        // profile 的密钥由刷新它的工具写回文件，下一个请求就读新的：不用改配置
        (Some("ExpiredTokenException"), Some(profile)) => msg!(
            "gw.upstream.aws_profile_expired",
            upstream = provider.name.clone(), profile = profile =>
            "The temporary AWS credential of upstream `{upstream}` has expired. Refresh AWS \
             profile `{profile}`; the next request reads it again."
        ),
        (Some("ExpiredTokenException"), None) => msg!(
            "gw.upstream.aws_token_expired", upstream = provider.name.clone() =>
            "The temporary AWS credential of upstream `{upstream}` has expired. Replace its \
             session token and the access keys that came with it."
        ),
        (Some(kind), _) => msg!(
            "gw.upstream.bedrock_refused",
            upstream = provider.name.clone(), status = status.as_u16(), kind = kind =>
            "AWS refused the credential of upstream `{upstream}` (HTTP {status}, {kind}). Check \
             that the credential is valid and may use this model. AWS's own message names the \
             account, so it is not passed on."
        ),
        // AWS 没说是哪种异常：另一句话，不拿一个英文词组去填 `{kind}` —— 那一格在译文里
        // 就是一段没翻译的英文
        (None, _) => msg!(
            "gw.upstream.bedrock_refused_unnamed",
            upstream = provider.name.clone(), status = status.as_u16() =>
            "AWS refused the credential of upstream `{upstream}` (HTTP {status}). Check that the \
             credential is valid and may use this model. AWS's own message names the account, \
             so it is not passed on."
        ),
    };
    for h in ["content-length", "content-type", "transfer-encoding"] {
        headers.remove(h);
    }
    // 前缀只在交给客户端的这一份上（和网关自己的错误一样，见 `crate::error`）：记录里的
    // 那一句是这次请求失败的原因，界面按码翻译
    let body = serde_json::json!({ "message": format!("[ThinkWatch] {}", text.text) }).to_string();
    let mut resp = http::Response::new(body);
    *resp.status_mut() = status;
    *resp.headers_mut() = headers;
    resp.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    (reqwest::Response::from(resp), text)
}

/// 读错误响应的开头（最多 [`crate::failure::BODY_PEEK`]），交回读到的和一个照旧能从头
/// 读起的响应 —— 这一跳可能还是要原样交给客户端的。
async fn peek_body(r: reqwest::Response) -> (Bytes, reqwest::Response) {
    use futures::StreamExt;
    let status = r.status();
    let headers = r.headers().clone();
    let mut stream = r.bytes_stream();
    let mut held: Vec<Bytes> = Vec::new();
    let mut size = 0usize;
    // 错误正文都很短。**读不完就算了**：这一跳多半已经失败了，读它只为认出原因，
    // 不能因此拖住下一家
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while size < crate::failure::BODY_PEEK {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                size += chunk.len();
                held.push(chunk);
            }
            _ => break,
        }
    }
    let head: Vec<u8> = held.iter().flat_map(|c| c.iter().copied()).collect();
    let replay = futures::stream::iter(held.into_iter().map(Ok::<_, reqwest::Error>));
    let mut resp = http::Response::new(reqwest::Body::wrap_stream(replay.chain(stream)));
    *resp.status_mut() = status;
    *resp.headers_mut() = headers;
    let head = Bytes::from(head);
    (head, reqwest::Response::from(resp))
}

/// 额度用完、这一跳又没说什么时候重置的：看这家最近报的额度窗口（GLM 的在 429 的
/// 正文里读过了，别家的在响应头里）。满了的窗口里取**最晚**重置的那个
fn known_reset(state: &AppState, provider: &str, cause: Cause) -> Cause {
    let Cause::QuotaUsedUp { resets_at_ms: None } = cause else {
        return cause;
    };
    let now = now_ms();
    let resets_at_ms = state.quotas().get(provider).and_then(|q| {
        q.windows
            .iter()
            .filter(|w| w.rejected() || w.used_percent >= 100.0)
            .filter_map(|w| w.resets_at_ms)
            .filter(|t| *t > now)
            .max()
    });
    Cause::QuotaUsedUp { resets_at_ms }
}

/// 这个回答要不要等开头：生成回答的流式响应才等。等的话，上游说的是哪种格式、
/// 是不是 Bedrock 的二进制帧
fn opening_of(
    req: &Inbound,
    provider: &tw_config::Provider,
    out: &Outbound,
    r: &reqwest::Response,
) -> Option<(tw_dialect::ir::Dialect, bool)> {
    if !r.status().is_success() {
        return None;
    }
    let ct = r
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let eventstream =
        provider.is_bedrock() && ct.starts_with(tw_bedrock::eventstream::CONTENT_TYPE);
    if !(eventstream || ct.starts_with("text/event-stream")) {
        return None;
    }
    let dialect = out.target.or_else(|| req.api.map(|a| a.dialect()))?;
    Some((dialect, eventstream))
}

fn now_ms() -> u64 {
    u64::try_from(tw_breaker::now_ms()).unwrap_or(0)
}
