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
//!
//! 发出去之后 `failover.idle_timeout_secs` 还没有内容的一家也换掉（见 [`super::idle`]）；在等
//! 上游的时候被手动中止的，丢掉这一跳、不再换（见 [`crate::abort`]）。
//!
//! 候选都试过了，最后一跳在内容之前出了暂时的错（过载、5xx、连不上、流断了）：在这个请求的
//! 等待期限之内退避一阵，把暂时出错的那几家再试一轮（见 [`crate::retry`]）。

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
    /// 这一跳在这家占着的位置（见 [`crate::slots`]）。**跟着回答走**：交完、或者客户端
    /// 走掉，响应体被丢掉时才还回去
    pub(super) slot: crate::slots::Slot,
    /// 这一跳发出去的那一刻。这一家的快慢样本从这里算起（见 [`crate::latency`]）
    pub(super) sent_at: std::time::Instant,
    /// 这一家答没答上还不知道：最后一家的开头没压（整包的、等不起的），或者压着时到点了还没有
    /// 内容、在开头报了限流以外的错，原样交了出去。成败的账等回程看到第一段内容才记（见
    /// `relay`）。没有内容就超时了记失败，和压着开头时一样
    pub(super) unconfirmed: bool,
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
    /// `body` 解得开（是 JSON）：客户端的原话照管线开头解的那一遍，插件改过的那一份是
    /// 网关自己写出来的，一定解得开
    json: bool,
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
                json: true,
            },
            None => Asked {
                body: &req.body,
                path: req.uri.path(),
                decoded: reading.decoded.as_ref(),
                json: reading.json,
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
    /// 请求体里的提示缓存断点是转换时自动标的（客户端一个都没标）：发给的是这个模型。
    /// 上游拒了这些断点就去掉再发一次（见 [`crate::cache_marks`]）
    auto_cache: Option<String>,
    /// 发出去的和客户端那一份（插件改过的话是改过的）说的不是一回事了：转换过格式，或者
    /// 去掉了上游不认的字段。报文记录照它决定另不另存（见 [`crate::content`]）。只改了模型名、
    /// 去掉了客户端的身份字段不算：模型名在尝试链上，为它另存一整份不值
    reshaped: bool,
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
    // 这把密钥的模型范围：每一跳发出的名字都要过它（见 `crate::sent::name`）
    let allow = crate::models::key_allow(&rt.config, &req.client_name);
    // 无响应超时：每一跳从发出去算起，等多久（见 `super::idle`）
    let quiet = super::idle::Quiet::of(state, rt);
    // 还没看的候选，按顺序
    let mut queue: std::collections::VecDeque<&String> = started.alive.iter().collect();
    // 暂时发不出去、等得到的那几家（见 `crate::slots`、`crate::throttle`）：满着的、限流暂停着
    // 的、分钟或小时的用量上限满了的，和跳过时它是哪一样，按候选的顺序。候选都看过一遍还没有
    // 着落时，在它们里面等先能发的那一家
    let mut waiting: Vec<(&String, Wait)> = Vec::new();
    // 这一期用完了、跳过了的那几家，和它们重置的时刻。一家都发不出去时，429 里说到它们
    let mut spent: Vec<(&String, u64)> = Vec::new();
    // 等到什么时候：准入时定下的那一刻（见 `super::admission`），**整个请求共用** —— 等密钥的
    // 分钟、小时上限已经用掉的，这里就少等那么久。流式的回答等过一阵先交出 `200` 和保活（见
    // `super::commit`），整包的等的时候客户端一个字节都收不到
    let until = started.wait_until;
    let deadline_ms = now_ms()
        + until
            .saturating_duration_since(tokio::time::Instant::now())
            .as_millis() as u64;
    // 这个请求在哪几家的队里排着（见 `crate::throttle`）。**丢掉就离开**：客户端走了、被中止了，
    // 后面的不用替它等
    let mut waiter = state.throttles.waiter();
    // 等过的那一跳在尝试链上的位置和等了多久。那一跳怎么收场都只进一行，进了之后补上
    let mut queued: Option<(usize, u64)> = None;
    // 等到最后，剩下的候选还都发不出去
    let mut stalled = false;
    // 有一跳真的发到了上游、失败了（回了错误、流在内容之前断了、连不上、没有内容超时了）。
    // 等发不出去的那几家等到最后的话，交出去的是这一家的错，不是「都发不出去」（见下面的
    // `stalled`）。**回 429 的不算**：那是让等一会儿，不是失败
    let mut reached = false;
    // 退避重试的账（见 `crate::retry`）：最后一个失败是不是暂时的错、这一轮里暂时出错的是哪几家、
    // 重试过的那几轮
    let mut retries = Retries::default();
    // 客户端收的是 SSE 流：压着开头过了时限先交响应头、发保活（见 `super::commit`）。最后一家的
    // 开头只在这时为重试压着
    let early = super::commit::early(state, req, reading).is_some();
    // 插件写进来的、这个请求已经报过的命中：每一跳再查一遍，同样的只报一次（见
    // `crate::guard::Reported`）
    let mut reported = crate::guard::Reported::default();

    loop {
        stamp_queued(&mut chain, queued.take());
        // 候选都试过了，最后一跳在内容之前暂时出错：等待期限之内退避一阵，把暂时出错的那几家再
        // 试一轮。不重试的，交出去的是最后那个错误
        if queue.is_empty() && waiting.is_empty() {
            let Some((round, wait)) = retry_round(state, started, &mut retries, counting, until)
            else {
                break;
            };
            let n = retries.next();
            retrying(state, id, &round, n, wait, deadline_ms);
            let t = std::time::Instant::now();
            let woke = tokio::select! {
                biased;
                _ = tokio::time::sleep(wait) => true,
                // 退避的时候被手动中止：一个字节都还没发出去，尝试链上不多一跳
                _ = req.abort.wait() => false,
            };
            dequeued(state, id);
            if !woke {
                halt = Some(GatewayError::aborted());
                break;
            }
            retries
                .rounds
                .push((chain.len(), n, t.elapsed().as_millis() as u64));
            queue.extend(round);
        }
        // 下一个候选。都看过了，就在等得到的那几家里等先能发的：等到的那一家带着占到的位置
        let (name, mut granted) = match queue.pop_front() {
            Some(name) => (name, None),
            None if waiting.is_empty() => break,
            None => {
                // 每一家都要到等待期限之后才能发：不用干等，当场说清多久之后再来。这个请求能等的
                // 已经等完了（或者配置的是不等）也一样 —— **它没排过队**，不报排队（界面会显示成
                // 「排队中」）
                if tokio::time::Instant::now() >= until
                    || hopeless(state, rt, &waiting, deadline_ms)
                {
                    stalled = true;
                    break;
                }
                let names: Vec<(&str, &[tw_config::KeyLimit])> = waiting
                    .iter()
                    .map(|(n, _)| (n.as_str(), usage_limits_of(rt, n)))
                    .collect();
                queued_up(state, id, &names, deadline_ms);
                let t = std::time::Instant::now();
                let ready = tokio::select! {
                    biased;
                    ready = crate::throttle::first_ready(state, &mut waiter, &names, until) => ready,
                    // 等的时候被手动中止：一个字节都还没发出去，尝试链上不多一跳
                    _ = req.abort.wait() => {
                        dequeued(state, id);
                        halt = Some(GatewayError::aborted());
                        break;
                    }
                };
                dequeued(state, id);
                match ready {
                    crate::throttle::Outcome::Ready(k, ready) => {
                        (waiting.remove(k).0, Some((ready, t.elapsed())))
                    }
                    crate::throttle::Outcome::Late => {
                        stalled = true;
                        break;
                    }
                    // 等着的都不用等了：这一期用完了的记下来（429 里说到它们），停用了的交给熔断
                    // （尝试链上已经有跳过它们的那一行）
                    crate::throttle::Outcome::Gone => {
                        for (n, _) in std::mem::take(&mut waiting) {
                            if let crate::throttle::Peek::UsedUp { until_ms, .. } =
                                state.throttles.peek(n, usage_limits_of(rt, n))
                            {
                                spent.push((n, until_ms));
                            }
                        }
                        continue;
                    }
                }
            }
        };
        // 后面没有别的候选了。暂时发不出去、还等得到的那几家也算在后面
        let last = queue.is_empty() && waiting.is_empty();
        // 最后一个失败是不是暂时的错，看的是这一跳：暂时出错的那几处再记上
        retries.fault = None;
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
        let mut hop_started = std::time::Instant::now();

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
        let sent = match crate::sent::name(
            &rt.config,
            &catalog,
            decision,
            provider,
            &asked_model,
            allow,
        ) {
            Ok(sent) => sent,
            // 这一家服务不了（别名列的名字它一个都没有，或者清单里没有这个名字）。路由时已经
            // 跳过了这样的候选，能到这儿说明它的清单刚刚换过：**不把别名原样发给它**，换下一家。
            // 发给它的名字密钥不让用的，路由时也跳过了；这里再看一遍，哪一跳都发不出它
            Err(skip) => {
                let mut serving = crate::models::Serving::default();
                serving.skipped.push((provider.name.clone(), skip));
                let err = serving.explain(&asked_model.model);
                chain.push(hop_failed(
                    &provider.name,
                    None,
                    err.detail.clone(),
                    hop_started,
                ));
                last_err = Some(err);
                continue;
            }
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

        // 这一家此刻发得出去吗（见 `crate::throttle`）：限流暂停着、用量上限满了或用完了，和
        // 并发上限（见 `crate::slots`）—— 要发出去了，先占这家一个位置。**在插件和转换之前**：
        // 没发的一跳不跑插件、不报转换。等不是失败：不停用、不进熔断的账。
        //
        // 数 token 不排队、不占位置：它不跑模型，一眨眼就回来；为它排在几个长回答后面等上半分钟、
        // 最后回一个 429，客户端连上下文还剩多少都看不到。这一家暂时发不出去的，由网关估
        let limits = crate::throttle::usage_limits(provider);
        let (slot, mut probe) = match granted.take() {
            Some((ready, waited)) => {
                queued = Some((chain.len(), waited.as_millis() as u64));
                hop_started = std::time::Instant::now();
                (ready.slot, ready.probe)
            }
            None if counting => {
                if state.throttles.peek(&provider.name, limits) != crate::throttle::Peek::Open {
                    chain.push(estimated_hop(provider, None, None, hop_started));
                    estimated = Some(provider);
                    break;
                }
                (crate::slots::Slot::free(), None)
            }
            None => {
                let model = Some(sent.clone()).filter(asked_other);
                // 这段对话留在这家是为了它的缓存：等它，等不到再换下一家（缓存就丢在这家了）。
                // 别的候选发不出去当场跳过，记下来，候选都看过之后再等
                let sticky = started.choice.stayed_on.as_ref() == Some(name);
                let wait = match state
                    .throttles
                    .gate(&provider.name, limits, Some(waiter.id))
                {
                    // 这一期用完了：到下一期之前等多久都一样，跳过，不等
                    crate::throttle::Gate::UsedUp { until_ms, .. } => {
                        chain.push(crate::server::hop_used_up(
                            &provider.name,
                            model,
                            until_ms,
                            hop_started,
                        ));
                        spent.push((name, until_ms));
                        continue;
                    }
                    crate::throttle::Gate::Go(probe) => {
                        match state.slots.try_take(&provider.name) {
                            Some(slot) => Ok((slot, probe)),
                            None => Err((Wait::Busy, None)),
                        }
                    }
                    crate::throttle::Gate::Wait { kind, until_ms } => {
                        Err((Wait::of(kind), until_ms))
                    }
                };
                match wait {
                    Ok(go) => go,
                    Err((why, until_ms)) => {
                        // 留下的那一家等得到才等：说得出什么时候恢复、又晚于这个请求的等待期限的，
                        // 等也白等。这个请求能等的已经等完了（或者配置的是不等）也不再等
                        let mut waited = None;
                        if sticky
                            && until_ms.is_none_or(|t| t <= deadline_ms)
                            && tokio::time::Instant::now() < until
                        {
                            let one = [(provider.name.as_str(), limits)];
                            queued_up(state, id, &one, deadline_ms);
                            let t = std::time::Instant::now();
                            let ready = tokio::select! {
                                biased;
                                ready = crate::throttle::first_ready(state, &mut waiter, &one, until) => ready,
                                _ = req.abort.wait() => {
                                    dequeued(state, id);
                                    halt = Some(GatewayError::aborted());
                                    break;
                                }
                            };
                            dequeued(state, id);
                            waited = Some(t.elapsed().as_millis() as u64);
                            hop_started = std::time::Instant::now();
                            if let crate::throttle::Outcome::Ready(_, ready) = ready {
                                queued = waited.map(|ms| (chain.len(), ms));
                                (ready.slot, ready.probe)
                            } else if let crate::throttle::Peek::UsedUp { until_ms, .. } =
                                state.throttles.peek(&provider.name, limits)
                            {
                                // 等的时候这一期用完了：跳过，不再等它
                                chain.push(crate::server::hop_used_up(
                                    &provider.name,
                                    model,
                                    until_ms,
                                    hop_started,
                                ));
                                spent.push((name, until_ms));
                                continue;
                            } else {
                                chain.push(skipped_hop(
                                    state,
                                    provider,
                                    model,
                                    (why, until_ms),
                                    waited,
                                    hop_started,
                                ));
                                waiting.push((name, why));
                                continue;
                            }
                        } else {
                            chain.push(skipped_hop(
                                state,
                                provider,
                                model,
                                (why, until_ms),
                                waited,
                                hop_started,
                            ));
                            waiting.push((name, why));
                            continue;
                        }
                    }
                }
            }
        };
        // 这一家的用量上限：要发出去了，请求数此刻记上，token 和费用按请求估一个数占着（见
        // `crate::upstream_limits`）。没发出去的还回去（丢掉就还），发出去了的见下面的 [`Spent`]。
        // 数 token 的不算
        let usage_hold = (!counting && !limits.is_empty()).then(|| {
            let ask = usage_ask(state, reading, provider, &sent, limits);
            state.upstream_limits.reserve(&provider.name, limits, &ask)
        });

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
            &mut reported,
        )
        .await
        {
            Ok(p) => p,
            Err(super::plug::Stop::Request(why)) => {
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
            // 插件换上的别名这一家服务不了：这一家不发，换下一家
            Err(super::plug::Stop::Hop(why)) => {
                chain.push(hop_failed(
                    &provider.name,
                    Some(sent.clone()).filter(asked_other),
                    why.clone(),
                    hop_started,
                ));
                last_err = Some(GatewayError::new(crate::error::Source::Request, why));
                continue;
            }
        };
        // 插件换了发给这一家的模型名：和规则改写的一样，只是盖过它
        if let Some(m) = &plugged.model {
            effective_set.model = Some(m.clone());
        }
        let model = Some(plugged.model.clone().unwrap_or(sent)).filter(asked_other);

        let asked = Asked::of(req, reading, &plugged);
        let out = match super::heavy(asked.body, || {
            prepare(state, req, reading, &asked, provider, &effective_set, id)
        }) {
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

        // 出站脱敏的拦截档：换掉**这一跳真正发出去的那一份**（可能转换过
        // 格式）。规则是全局的，每一跳换掉的是同一批东西；**接着原文那本账换**，
        // 同一个值在每一跳、在存下来的那份请求里都是同一个占位符。插件改过的一跳接着
        // 插件那本账（插件写进来的新值在那里编好了号）
        let seed = plugged
            .rewritten
            .as_ref()
            .map_or(&started.ledger, |r| &r.ledger);
        let (body, ledger, differs) = super::heavy(&out.body, || {
            // 这一家在这段对话里拒过的别家封存的推理：发之前先去掉（见 `crate::seal`）
            let unsealed = unseal_upfront(state, req, started, provider, &out);
            // 发出去的就是客户端原文的那些字节（同格式直通、一个字节都没改）：开头那一遍在
            // 它上面找到的就是这一跳要换的，不再找一遍
            let known = started.hits.as_deref().filter(|_| {
                plugged.rewritten.is_none()
                    && unsealed.as_ptr() == req.body.as_ptr()
                    && unsealed.len() == req.body.len()
            });
            // 发出去的和这一跳客户端那一边（插件改过的话是改过的那一份）不是一回事了：转换过、
            // 去掉过字段（上游不认的、它拒过的封存推理）。报文记录另存它（见 `crate::content`）。
            // 换成占位符不算：落盘时两边换的是同一个
            let differs = out.reshaped || unsealed.as_ptr() != out.body.as_ptr();
            let mode = rt.config.security.redact.mode;
            // 这一跳发出去的是什么形状：转换过的按转成的格式，直通的按路径
            let shape = match out.target {
                Some(d) => Some(crate::guard::Shape::Generate(d)),
                None => crate::client_api::ClientApi::written(&out.path),
            };
            let (body, ledger) =
                crate::guard::replace_found(mode, &rt.redact, unsealed, seed, shape, known);
            (body, ledger, differs)
        });

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
        // 这一跳在报文记录里记到哪儿（见 `crate::content`）：尝试链上的第几跳，从 1 数
        let tap = crate::content::Tap::new(
            req.seat.get(),
            chain.len() as u32 + 1,
            crate::bodies::Redaction {
                rules: rt.redact.clone(),
                ledger: ledger.clone(),
            },
            differs,
        );
        // 这一跳发出去的那一刻：这一家的快慢样本、无响应超时都从这里算起（见 `crate::latency`、
        // `super::idle`）。之前等空位、跑插件的时间都不算
        let sent_at = std::time::Instant::now();
        let quiet_by = quiet.after(sent_at);
        // 这一家的答复也看这一刻（见 `crate::throttle`）：最近一次 429 之前发出去的不算数。
        // 暂停结束后探路的就是这一跳的话，从这一刻起计它的宽限
        if let Some(p) = probe.as_mut() {
            p.sent();
        }
        // 发出去了：这一跳在这一家的用量上限上**算一个请求**，怎么收场都算（失败的、超时的、被中止
        // 的上游都已经收到了它），只有回了 429 的、其实没发出去的（连不上、签不了名）还回去
        let mut usage_hold = Spent {
            hold: usage_hold,
            id,
        };

        // 发出去、等响应头。**等着的这个 future 只活在这一块里**：放弃这一家、被中止时它跟着
        // 丢掉，连接随之断开
        let waited = {
            let sending = async {
                let sent = send(
                    state,
                    req,
                    provider,
                    http,
                    &out,
                    body.clone(),
                    upstream_headers.clone(),
                    aws.as_ref(),
                    &tap,
                )
                .await;
                // 上游拒绝了自动标的缓存断点、别家封存的推理：去掉它们，同一家再发一次
                match sent {
                    Ok(r) => {
                        let resend = Resend {
                            out: &out,
                            body: &body,
                            headers: &upstream_headers,
                            aws: aws.as_ref(),
                            conversation: started.conversation.as_deref(),
                            tap: &tap,
                        };
                        let (r, uncached) =
                            resend_uncached(state, req, provider, http, resend, r).await?;
                        let resend = Resend {
                            out: &out,
                            body: uncached.as_ref().unwrap_or(&body),
                            headers: &upstream_headers,
                            aws: aws.as_ref(),
                            conversation: started.conversation.as_deref(),
                            tap: &tap,
                        };
                        resend_unsealed(state, req, provider, http, resend, r).await
                    }
                    Err(e) => Err(e),
                }
            };
            tokio::select! {
                biased;
                sent = sending => Waited::Done(sent),
                _ = req.abort.wait() => Waited::Aborted,
                _ = tokio::time::sleep_until(quiet_by) => Waited::Quiet,
            }
        };
        let sent = match waited {
            Waited::Done(sent) => {
                // 上游的响应头（重发过的是最后那一遍的）。这一家在不在限流（见 `crate::throttle`）
                // 等看清了这个答复再说：压着开头的回答，流的开头还可能报限流
                if let Ok(r) = &sent {
                    tap.answered(r);
                }
                sent
            }
            // 被手动中止：这一跳到此为止，不再换下一家。不停用、不算失败；探路的名额还回去
            Waited::Aborted => {
                if let Some(p) = probe.take() {
                    p.withdraw();
                }
                chain.push(super::idle::aborted_hop(
                    &provider.name,
                    model.clone(),
                    None,
                    hop_started,
                ));
                halt = Some(GatewayError::aborted());
                break;
            }
            // 响应头都没等到：放弃这一家，换下一家；没有下一家了，交出去的就是这个超时
            Waited::Quiet => {
                reached = true;
                state.throttles.released(&provider.name, sent_at);
                last_err = Some(gave_up(
                    state,
                    started,
                    reading,
                    Abandoned {
                        provider,
                        model: model.clone(),
                        status: None,
                        usage: None,
                        quiet,
                        hop_started,
                    },
                    &mut chain,
                ));
                continue;
            }
        };
        match sent {
            Ok(r) if !r.status().is_success() => {
                let status = r.status().as_u16();
                // 同格式的上游没实现数 token（不少中转只做了生成回答）：网关自己估。
                // **在判断换不换之前**：404 平时是「这家没有这个模型」，要换下一家
                if counting && crate::count::unsupported(status) {
                    state.throttles.answered(&provider.name, sent_at);
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
                if !matches!(verdict, Verdict::Failed(Cause::RateLimited { .. })) {
                    to_throttle(state, &provider.name, sent_at, verdict);
                }
                // **最后一家的 4xx 交给客户端**：没有下一家可换了，上游自己的原话
                // （「invalid x-api-key」）比我们转述的一句「回了 401」有用得多。
                // 5xx 和 429 照旧报我们的话，带着尝试链
                let hand_on = last && status < 500 && status != 429;
                match verdict {
                    // 限流（额度用完不算）：**不是失败**。这一家暂停（见 `crate::throttle`），这个请求
                    // 记下它、接着看下一家，候选都看过了就等它恢复 —— 客户端什么都还没收到，再发
                    // 一遍它察觉不到。不进熔断的账，也不当成交出去的那条错误
                    Verdict::Failed(Cause::RateLimited { .. }) => {
                        tap.said(&head);
                        state.note_quota(id, &provider.name, &headers);
                        state.note_proxy_ok(&provider.proxy);
                        state.glm_traffic(provider);
                        let hint = crate::throttle::pause_hint(&headers, &head, now_ms());
                        let pause = state.throttles.limited(&provider.name, sent_at, hint);
                        usage_hold.release();
                        let mut a = hop(
                            &provider.name,
                            model.clone(),
                            tw_api::AttemptOutcome::Status,
                            status,
                            hop_started,
                        );
                        a.pause_ms = Some(pause.as_millis() as u64);
                        chain.push(a);
                        // 数 token 不等：网关估
                        if counting {
                            chain.push(estimated_hop(provider, model.clone(), None, hop_started));
                            estimated = Some(provider);
                            break;
                        }
                        // 探路的又被限流了：它下一次等的时候回到这一家的队头，后面的照旧排在它
                        // 后面。**先去试别的候选时不占着**（见 `throttle::Waiter::keep_place`）
                        if probe.is_some() {
                            waiter.keep_place(&provider.name);
                        }
                        waiting.push((name, Wait::Throttled));
                        continue;
                    }
                    Verdict::Failed(cause) if !hand_on => {
                        // 换下一家：这一跳的回答就是读来认原因的那一截
                        tap.said(&head);
                        // 5xx、没钱了、额度用完、凭据被拒、没有这个模型：换一家有意义，那边是
                        // 另一把密钥、另一个账户。停用多久看原因。后面只剩暂时发不出去的那几家
                        // 也一样等它们：先能发的那一家可能答得上
                        reached = true;
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
                        // 回了 429 的（额度用完）上游没接：不算进这家的用量上限，和记录里一样
                        // （`tw_api::AttemptView::counts_for_upstream`）
                        if status == 429 {
                            usage_hold.release();
                        }
                        let cause = known_reset(state, &provider.name, cause);
                        note_health(
                            &state.bus,
                            &state.health,
                            &provider.name,
                            state.health.record_cause(&provider.name, cause),
                        );
                        // 同一家再试只认几个状态码（见 `crate::retry::transient_status`），正文说是
                        // 请求本身的问题的不算
                        if crate::retry::transient(cause)
                            && crate::retry::transient_status(status)
                            && !crate::retry::request_fault(&head)
                        {
                            let hint = crate::throttle::pause_hint(&headers, &head, now_ms());
                            retries.shaky(name, hint);
                        }
                        chain.push(hop(
                            &provider.name,
                            model.clone(),
                            tw_api::AttemptOutcome::Status,
                            status,
                            hop_started,
                        ));
                        // **429 要保住 429。**塌成 502 的话，客户端会当成「服务器
                        // 坏了」而不是「该退避了」，而它们该做的事完全不同。到这儿的 429 是额度
                        // 用完、没钱了（限流在上面那一支）
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
                            slot,
                            sent_at,
                            unconfirmed: false,
                        });
                        break;
                    }
                }
            }
            Ok(r) => {
                // 回答的开头：**第一段内容到之前**上游在流里报的错照样换下一家（报的是限流就排队
                // 等它，和回 429 一样），一直没有内容也换（见 `opening`）。
                //
                // 最后一家只压流式的、这个请求又还等得起的，而且是两种时候：客户端收的是 SSE 流（过了
                // 时限先拿到响应头和保活，见 `super::commit`）—— 开头报的暂时的错（过载、5xx）、流在
                // 内容之前断了，退避一阵再试一遍（见 `crate::retry`）；或者这一家可能在限流（探路的、
                // 最近被限流过的，见 `throttle::Throttles::wary`）。流的开头报的限流暂停这一家、排队
                // 等它恢复。客户端都察觉不到 —— 不压的话那个错误直接交给了客户端。别的时候不压：
                // 整包的回答压着的话，客户端到正文的第一个字节才拿到响应头，中转站的空格保活也被
                // 吞掉；等不起的没有下一家可换、也不能再试。不压的由回程按无响应超时收尾（见 `relay`）
                let hold = hold_of(req, reading, provider, &out, &r);
                let held = hold.as_ref().is_some_and(|h| {
                    !last
                        || (matches!(h, Hold::Stream(..))
                            && tokio::time::Instant::now() < until
                            && (early || state.throttles.wary(&provider.name)))
                });
                // 不压开头的最后一家：它答没答上（成败的账）要等回程看到第一段内容才知道
                let mut unconfirmed = last && hold.is_some() && !held;
                let r = match hold.filter(|_| held) {
                    None => {
                        // 不压开头：响应头就是它的答复，这一家没在限流（探路的过了，连续限流的次数
                        // 归零）
                        state.throttles.answered(&provider.name, sent_at);
                        r
                    }
                    Some(hold) => {
                        let status = r.status().as_u16();
                        let watching = async move {
                            match hold {
                                Hold::Stream(dialect, eventstream) => {
                                    super::opening::watch(r, dialect, eventstream, quiet_by).await
                                }
                                Hold::Body => super::opening::first_bytes(r, quiet_by).await,
                            }
                        };
                        let opened = tokio::select! {
                            biased;
                            o = watching => Some(o),
                            // 压着开头的时候被中止：响应跟着丢掉，连接断开
                            _ = req.abort.wait() => None,
                        };
                        match opened {
                            None => {
                                if let Some(p) = probe.take() {
                                    p.withdraw();
                                }
                                chain.push(super::idle::aborted_hop(
                                    &provider.name,
                                    model.clone(),
                                    Some(status),
                                    hop_started,
                                ));
                                halt = Some(GatewayError::aborted());
                                break;
                            }
                            // 内容来了：**这时**才是这一家的答复。响应头是 200、流里接着报限流的
                            // 上游，响应头不说明它好了
                            Some(super::opening::Opening::Go(r)) => {
                                state.throttles.answered(&provider.name, sent_at);
                                r
                            }
                            // 到点了还没有内容：这一跳没有结论
                            Some(super::opening::Opening::Slow { response, usage }) => {
                                state.throttles.released(&provider.name, sent_at);
                                if last {
                                    // 最后一家：和不压开头时一样交给回程，由它按无响应超时收尾
                                    unconfirmed = true;
                                    response
                                } else {
                                    // 放弃这一家。**响应跟着这一轮循环丢掉**，和上游的连接随之
                                    // 断开，它不再接着生成
                                    let status = response.status().as_u16();
                                    drop(response);
                                    reached = true;
                                    // 上游回了话，说明代理是通的
                                    state.note_proxy_ok(&provider.proxy);
                                    last_err = Some(gave_up(
                                        state,
                                        started,
                                        reading,
                                        Abandoned {
                                            provider,
                                            model: model.clone(),
                                            status: Some(status),
                                            usage,
                                            quiet,
                                            hop_started,
                                        },
                                        &mut chain,
                                    ));
                                    continue;
                                }
                            }
                            Some(super::opening::Opening::Failed {
                                status,
                                headers,
                                body,
                                kind,
                                message,
                                transient,
                                response,
                            }) => {
                                let verdict =
                                    crate::failure::classify(status, &headers, &body, now_ms());
                                let limited =
                                    matches!(verdict, Verdict::Failed(Cause::RateLimited { .. }));
                                if !limited {
                                    to_throttle(state, &provider.name, sent_at, verdict);
                                }
                                // 暂时的错（名单里的码，见 `opening::transient_in`）同一家可以再试。
                                // 最后一家报的、又真的再试得了（等待期限够下一次退避、留在一家的
                                // 对话留的就是能再试的那一家，同 `retry_round`）：和换下一家时一样
                                // 记下，候选都试过了就退避重试（见 `crate::retry`）。再试不了的原样
                                // 交出去，不丢上游自己的那句话
                                let shaky = transient
                                    && matches!(verdict, Verdict::Failed(c) if crate::retry::transient(c));
                                let hint = crate::throttle::pause_hint(&headers, &body, now_ms());
                                let retried = shaky
                                    && retries
                                        .feasible(state, started, name, hint, counting, until);
                                if verdict == Verdict::ClientError || (last && !limited && !retried)
                                {
                                    // 请求本身的问题，或者最后一家报了限流以外、不再重试的错：原样
                                    // 交出去，和不压开头时一样（后者的成败由回程记）
                                    unconfirmed |= verdict != Verdict::ClientError;
                                    response
                                } else {
                                    // 换下一家：这一跳的回答就是上游在开头报的那个错
                                    tap.said(&body);
                                    state.note_quota(id, &provider.name, &headers);
                                    state.note_proxy_ok(&provider.proxy);
                                    let said = msg!(
                                        "gw.upstream.stream_opening_error",
                                        upstream = provider.name.clone(), kind = kind, message = message =>
                                        "Upstream `{upstream}` started the answer and reported an error \
                                         before any content ({kind}): {message}"
                                    );
                                    let mut a = hop_failed(
                                        &provider.name,
                                        model.clone(),
                                        said.clone(),
                                        hop_started,
                                    );
                                    match verdict {
                                        // 流开头报的限流：和回 429 一样，暂停这一家，这个请求接着看
                                        // 下一家或者等它（见上面回 429 的那一支）
                                        Verdict::Failed(Cause::RateLimited { .. }) => {
                                            let hint = crate::throttle::pause_hint(
                                                &headers,
                                                &body,
                                                now_ms(),
                                            );
                                            let pause = state.throttles.limited(
                                                &provider.name,
                                                sent_at,
                                                hint,
                                            );
                                            usage_hold.release();
                                            a.pause_ms = Some(pause.as_millis() as u64);
                                            chain.push(a);
                                            if probe.is_some() {
                                                waiter.keep_place(&provider.name);
                                            }
                                            waiting.push((name, Wait::Throttled));
                                        }
                                        Verdict::Failed(cause) => {
                                            reached = true;
                                            let cause = known_reset(state, &provider.name, cause);
                                            note_health(
                                                &state.bus,
                                                &state.health,
                                                &provider.name,
                                                state.health.record_cause(&provider.name, cause),
                                            );
                                            if shaky {
                                                retries.shaky(name, hint);
                                            }
                                            chain.push(a);
                                            last_err = Some(if status == 429 {
                                                GatewayError::rate_limited(said)
                                            } else {
                                                GatewayError::upstream(said)
                                            });
                                        }
                                        // 请求本身的问题：上面原样交出去了
                                        Verdict::ClientError => {}
                                    }
                                    continue;
                                }
                            }
                            Some(super::opening::Opening::Broken(err)) => {
                                reached = true;
                                retries.shaky(name, None);
                                state.throttles.released(&provider.name, sent_at);
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
                if !unconfirmed {
                    note_health(
                        &state.bus,
                        &state.health,
                        &provider.name,
                        state.health.record_success(&provider.name),
                    );
                }
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
                    slot,
                    sent_at,
                    unconfirmed,
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
                    // 连不上：上游一个字节都没收到，这一跳不算
                    SendError::Http(e) if e.is_connect() => {
                        usage_hold.release();
                        reached = true;
                        retries.shaky(name, None);
                        // 没有结论：探路的不再探，交给熔断（见 `crate::throttle`）
                        state.throttles.released(&provider.name, sent_at);
                        state.check_proxy(&provider.proxy);
                        forward::map_reqwest_error(e)
                    }
                    SendError::Http(e) => {
                        reached = true;
                        // 响应头之前断了：和连不上一样是暂时的错
                        retries.shaky(name, None);
                        state.throttles.released(&provider.name, sent_at);
                        // 连不上的可能是代理而不是上游 —— 检一次那个代理，说清是哪一件事
                        state.check_proxy(&provider.proxy);
                        forward::map_reqwest_error(e)
                    }
                    // 签不了名：没发出去，探路的名额还回去
                    SendError::Sign(e) => {
                        usage_hold.release();
                        if let Some(p) = probe.take() {
                            p.withdraw();
                        }
                        GatewayError::config(msg!(
                            "gw.upstream.sign_failed",
                            upstream = provider.name.clone(), detail = e.0 =>
                            "The request to upstream `{upstream}` could not be signed with its AWS \
                             access keys: {detail}"
                        ))
                    }
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

    stamp_queued(&mut chain, queued.take());
    stamp_retries(&mut chain, &retries.rounds);
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
    // 每一跳走的代理，和这个请求从哪个出口出去的（见 `AttemptView::proxy`、
    // `RequestRouted::egress`）。**按这个请求的运行时查**：配置半路换过，这个请求走的还是它
    // 开始时的那一份 Client
    for a in &mut chain {
        a.proxy = rt
            .config
            .providers
            .iter()
            .find(|p| p.name == a.provider)
            .and_then(crate::outbound::egress);
    }
    let egress = egress_of(&chain);
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
        egress,
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

    // 剩下的候选等到最后还都发不出去（满着、限流暂停着、用量上限满了），**一家都没发到过**：
    // 429，带 `Retry-After`。请求本身没问题，过一会儿再来就发得出去；报成哪一家的失败都不对，
    // 它们一个字节都没收到 —— 回了 429 的也算没收到：它让等一会儿。候选全都是这一期用完了的也
    // 一样，那时到重置之前别再试（见 `crate::throttle::refusal`）。
    //
    // 发到过的话，交出去的是最后那一家失败的原因，和没有发不出去的上游时一样：乙回了 401、甲一直
    // 满着，客户端该看到的是乙的错 —— 一个「都满着」的 429 会让它退避了再试，而该修的是乙的
    // 密钥
    if !reached && (stalled || (last_err.is_none() && served.is_none() && !spent.is_empty())) {
        let now = now_ms();
        let mut stuck: Vec<(usize, String, crate::throttle::Stuck)> = waiting
            .iter()
            .map(|(n, why)| (n, stuck_now(state, rt, n, *why, now)))
            .chain(
                spent
                    .iter()
                    .map(|(n, t)| (n, crate::throttle::Stuck::UsedUp(*t))),
            )
            .map(|(n, s)| {
                let at = started.alive.iter().position(|a| a == *n);
                (at.unwrap_or(usize::MAX), n.to_string(), s)
            })
            .collect();
        stuck.sort_by_key(|(at, _, _)| *at);
        let stuck: Vec<(String, crate::throttle::Stuck)> =
            stuck.into_iter().map(|(_, n, s)| (n, s)).collect();
        return Err(crate::throttle::refusal(&stuck, now));
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
    if !retries.rounds.is_empty() {
        tracing::info!(
            chain = %attempts.join(" → "),
            retries = retries.rounds.len(),
            "retried after a temporary failure; {} answered in the end",
            served.provider.name
        );
    } else if attempts.len() > 1 {
        tracing::info!(
            chain = %attempts.join(" → "),
            "failed over; {} answered in the end",
            served.provider.name
        );
    }
    Ok(Answer::Served(Box::new(served)))
}

/// 这个请求从哪个出口出去的：接下它的那一跳走的代理。一家都没接下的，是最后一个发到了上游的
/// 那一跳走的 —— 它的流量也是从那儿出去的；一跳都没发出去的没有
fn egress_of(chain: &[tw_api::AttemptView]) -> Option<String> {
    chain
        .iter()
        .rev()
        .find(|a| a.outcome == tw_api::AttemptOutcome::Served)
        .or_else(|| chain.iter().rev().find(|a| a.sent()))
        .and_then(|a| a.proxy.clone())
}

/// 等过空位的那一跳：在尝试链上补上它等了多久。`at` 是它那一行的位置 —— 等到之后，
/// 这一跳不管怎么收场都只进一行
fn stamp_queued(chain: &mut [tw_api::AttemptView], queued: Option<(usize, u64)>) {
    if let Some((at, ms)) = queued
        && let Some(a) = chain.get_mut(at)
    {
        a.queued_ms = Some(ms);
    }
}

/// 发出去了的一跳在这一家用量上限上占着的（见 [`crate::upstream_limits`]）。**丢掉就交给请求号**，
/// 这一跳算一个请求、等结算：不管它怎么收场，上游都收到了它。回了 429 的、其实没发出去的调
/// [`Spent::release`] 还回去
struct Spent {
    hold: Option<crate::upstream_limits::UpstreamHold>,
    id: u64,
}

impl Spent {
    fn release(&mut self) {
        if let Some(h) = self.hold.take() {
            h.release();
        }
    }
}

impl Drop for Spent {
    fn drop(&mut self) {
        if let Some(h) = self.hold.take() {
            h.bind(self.id);
        }
    }
}

/// 退避重试的账（见 [`crate::retry`]）。
#[derive(Default)]
struct Retries<'n> {
    /// 最后一个失败是暂时的错：外面一层是有没有，里面是那一跳上游说了多久之后再来（没说是
    /// None，按退避算）
    fault: Option<Option<std::time::Duration>>,
    /// 这一轮里暂时出错的那几家。重试时再试它们
    shaky: Vec<&'n String>,
    /// 重试过的每一轮：在尝试链上从哪一跳起、是第几次、发出去之前退避了多少毫秒
    rounds: Vec<(usize, u32, u64)>,
    /// 下一次重试之前按退避该等多久，算过一次就定了（浮动是随机的）：先看再试不试得了
    /// （[`Self::feasible`]）、再真的退避（[`retry_round`]），两处用的是同一个数
    drawn: Option<std::time::Duration>,
}

impl<'n> Retries<'n> {
    /// 这一跳暂时出错了。`hint` 是上游说的多久之后再来
    fn shaky(&mut self, name: &'n String, hint: Option<std::time::Duration>) {
        self.fault = Some(hint);
        if !self.shaky.contains(&name) {
            self.shaky.push(name);
        }
    }

    /// 下一次是第几次重试，从 1 数
    fn next(&self) -> u32 {
        self.rounds.len() as u32 + 1
    }

    /// 下一次重试之前按退避该等多久（见 [`Self::drawn`]）
    fn backoff(&mut self, state: &AppState) -> std::time::Duration {
        let n = self.next();
        *self
            .drawn
            .get_or_insert_with(|| state.retry_backoff.before(n))
    }

    /// 这一跳（最后一家）暂时出错的话，再试得了吗：和 [`retry_round`] 同一个判据，只是不动账。
    /// 再试不了的，上游自己的回答原样交出去
    fn feasible(
        &mut self,
        state: &AppState,
        started: &'n Started,
        name: &'n String,
        hint: Option<std::time::Duration>,
        counting: bool,
        until: tokio::time::Instant,
    ) -> bool {
        let mut shaky = self.shaky.clone();
        if !shaky.contains(&name) {
            shaky.push(name);
        }
        let backoff = self.backoff(state);
        plan(state, started, &shaky, backoff, hint, counting, until).is_some()
    }
}

/// 候选都试过了，要不要退避重试（见 [`crate::retry`]）：要的话，交回这一轮再试哪几家、先退避
/// 多久。最后一个失败不是暂时的错的、数 token 的（它不排队，也不该为它等）、剩下的时间不够
/// 这一次退避的不重试。留在一家的对话只再试那一家；在冷却的跳过，都在冷却的照样试（同
/// [`crate::health::Health::filter`]）
fn retry_round<'n>(
    state: &AppState,
    started: &'n Started,
    retries: &mut Retries<'n>,
    counting: bool,
    until: tokio::time::Instant,
) -> Option<(Vec<&'n String>, std::time::Duration)> {
    let hint = retries.fault.take()?;
    let shaky = std::mem::take(&mut retries.shaky);
    let backoff = retries.backoff(state);
    let planned = plan(state, started, &shaky, backoff, hint, counting, until);
    if planned.is_some() {
        retries.drawn = None;
    }
    planned
}

/// 暂时出错的是 `shaky` 这几家，这一次按退避该等 `backoff`、上游说的是 `hint`：再试哪几家、先
/// 等多久（见 [`retry_round`]）。等多久取两者里长的（[`crate::retry::wait`]）
fn plan<'n>(
    state: &AppState,
    started: &'n Started,
    shaky: &[&'n String],
    backoff: std::time::Duration,
    hint: Option<std::time::Duration>,
    counting: bool,
    until: tokio::time::Instant,
) -> Option<(Vec<&'n String>, std::time::Duration)> {
    if counting {
        return None;
    }
    let wait = crate::retry::wait(backoff, hint);
    if until.saturating_duration_since(tokio::time::Instant::now()) < wait {
        return None;
    }
    let pinned = started.choice.stayed_on.as_ref();
    let mut round: Vec<&String> = started
        .alive
        .iter()
        .filter(|a| shaky.contains(a) && pinned.is_none_or(|p| p == *a))
        .collect();
    let open: Vec<&String> = round
        .iter()
        .copied()
        .filter(|a| state.health.is_available(a))
        .collect();
    if round.len() > 1 && !open.is_empty() {
        round = open;
    }
    (!round.is_empty()).then_some((round, wait))
}

/// 重试的那几轮：在尝试链上补上每一跳是第几次重试，每一轮的头一跳发出去之前退避了多久
fn stamp_retries(chain: &mut [tw_api::AttemptView], rounds: &[(usize, u32, u64)]) {
    for (i, &(from, n, ms)) in rounds.iter().enumerate() {
        let to = rounds.get(i + 1).map_or(chain.len(), |r| r.0);
        for a in chain.get_mut(from..to).into_iter().flatten() {
            a.retry = Some(n);
        }
        if from < to {
            chain[from].backoff_ms = Some(ms);
        }
    }
}

/// 一家暂时发不出去、等得到：跳过它时它是哪一样。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wait {
    /// 满着（`max_concurrent`）
    Busy,
    /// 限流暂停着
    Throttled,
    /// 分钟、小时的用量上限满了
    WindowFull,
}

impl Wait {
    fn of(kind: tw_api::ThrottleKind) -> Self {
        match kind {
            tw_api::ThrottleKind::WindowFull => Wait::WindowFull,
            _ => Wait::Throttled,
        }
    }
}

/// 这一家的用量上限（见 [`crate::throttle::usage_limits`]）。配置里没有它的是空的
pub(super) fn usage_limits_of<'r>(rt: &'r Runtime, name: &str) -> &'r [tw_config::KeyLimit] {
    rt.config
        .providers
        .iter()
        .find(|p| p.name == name)
        .map(crate::throttle::usage_limits)
        .unwrap_or_default()
}

/// 跳过了的、等过没等到的一跳：满着的、限流暂停着的、用量上限满了的。`until` 是那一刻看到的
/// 预计恢复的时刻
fn skipped_hop(
    state: &AppState,
    provider: &tw_config::Provider,
    model: Option<String>,
    (why, until_ms): (Wait, Option<u64>),
    queued_ms: Option<u64>,
    started: std::time::Instant,
) -> tw_api::AttemptView {
    let name = &provider.name;
    match why {
        Wait::Busy => crate::server::hop_busy(
            name,
            model,
            state.slots.limit(name).unwrap_or_default(),
            queued_ms,
            started,
        ),
        Wait::Throttled => crate::server::hop_waiting(
            name,
            model,
            tw_api::ThrottleKind::RateLimited,
            until_ms,
            queued_ms,
            started,
        ),
        Wait::WindowFull => crate::server::hop_waiting(
            name,
            model,
            tw_api::ThrottleKind::WindowFull,
            until_ms,
            queued_ms,
            started,
        ),
    }
}

/// 要等的这几家，**每一家都要到等待期限之后才恢复**：说得出什么时候、又晚于期限。满着的随时
/// 可能空出来，说不出什么时候的（探路的还没回话）也等得到，都不算
fn hopeless(state: &AppState, rt: &Runtime, waiting: &[(&String, Wait)], deadline_ms: u64) -> bool {
    waiting.iter().all(|(n, why)| {
        *why != Wait::Busy
            && match state.throttles.peek(n, usage_limits_of(rt, n)) {
                crate::throttle::Peek::Wait {
                    until_ms: Some(t), ..
                } => t > deadline_ms,
                crate::throttle::Peek::UsedUp { .. } => true,
                _ => false,
            }
    })
}

/// 一家等到最后还发不出去：此刻它是哪一样、预计什么时候恢复（见 [`crate::throttle::refusal`]）
fn stuck_now(
    state: &AppState,
    rt: &Runtime,
    name: &str,
    why: Wait,
    now: u64,
) -> crate::throttle::Stuck {
    use crate::throttle::{Peek, Stuck};
    if why == Wait::Busy {
        return Stuck::Busy;
    }
    match state.throttles.peek(name, usage_limits_of(rt, name)) {
        Peek::UsedUp { until_ms, .. } => Stuck::UsedUp(until_ms),
        Peek::Wait {
            kind: tw_api::ThrottleKind::WindowFull,
            until_ms,
        } => Stuck::WindowFull(until_ms),
        Peek::Wait { until_ms, .. } => Stuck::RateLimited(until_ms),
        // 到点那一刻刚好恢复了：马上再来就行
        Peek::Open if why == Wait::WindowFull => Stuck::WindowFull(Some(now)),
        Peek::Open => Stuck::RateLimited(Some(now)),
    }
}

/// 这个请求开始排队了（见 [`tw_api::Event::RequestQueued`]）：在等哪几家，最多等到什么时候
fn queued_up(
    state: &AppState,
    id: u64,
    names: &[(&str, &[tw_config::KeyLimit])],
    deadline_ms: u64,
) {
    state.bus.emit(tw_api::Event::RequestQueued {
        id,
        upstreams: names.iter().map(|(n, _)| n.to_string()).collect(),
        until_ms: deadline_ms,
        at_ms: now_ms(),
    });
}

/// 这个请求开始退避、之后重试（见 [`tw_api::Event::RequestRetrying`]）：再试哪几家、第几次、
/// 退避多久，最多等到什么时候
fn retrying(
    state: &AppState,
    id: u64,
    round: &[&String],
    attempt: u32,
    wait: std::time::Duration,
    deadline_ms: u64,
) {
    let at_ms = now_ms();
    state.bus.emit(tw_api::Event::RequestRetrying {
        id,
        upstreams: round.iter().map(|n| n.to_string()).collect(),
        attempt,
        retry_at_ms: at_ms + wait.as_millis() as u64,
        until_ms: deadline_ms,
        at_ms,
    });
}

/// 这个请求不排队、不退避了（见 [`tw_api::Event::RequestDequeued`]）：等到了要发出去、被中止了、
/// 等不到了
fn dequeued(state: &AppState, id: u64) {
    state.bus.emit(tw_api::Event::RequestDequeued {
        id,
        at_ms: now_ms(),
    });
}

/// 一跳有了不是限流的结论，告诉 [`crate::throttle`]：额度用完、没钱了是等不到 —— 不再探路，交给
/// 熔断（[`crate::throttle::Throttles::released`]）；别的是这一家的答复，它没在限流
fn to_throttle(state: &AppState, name: &str, sent_at: std::time::Instant, verdict: Verdict) {
    match verdict {
        Verdict::Failed(Cause::QuotaUsedUp { .. } | Cause::NoBalance) => {
            state.throttles.released(name, sent_at)
        }
        _ => state.throttles.answered(name, sent_at),
    }
}

/// 这一跳在这一家的用量上限上占多少：输入 token 的估算（解不开的请求没有估算，占 0），和设了
/// 费用上限时按发给它的名字算的输入费用。没有价格、不计费的是 0 —— 和结算时一样（见
/// `super::admission`）
fn usage_ask(
    state: &AppState,
    reading: &crate::client_api::Reading,
    provider: &tw_config::Provider,
    model: &str,
    limits: &[tw_config::KeyLimit],
) -> crate::key_limits::Ask {
    let tokens = if matches!(reading.decoded, Some(Ok(_))) {
        reading.facts.input_tokens
    } else {
        0
    };
    let priced = limits
        .iter()
        .any(|l| l.measure() == tw_config::LimitMeasure::Cost);
    let cost_micros = if priced && tokens > 0 {
        let usage = tw_pricing::Usage {
            input: tokens,
            ..Default::default()
        };
        crate::quote::quote(
            &state.pricing.load(),
            &provider.name,
            model,
            &usage,
            provider.billing,
        )
        .cost_micros
        .unwrap_or(0)
    } else {
        0
    };
    crate::key_limits::Ask {
        tokens,
        cost_micros,
    }
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
        usage: None,
        queued_ms: None,
        skipped: None,
        pause_ms: None,
        retry: None,
        backoff_ms: None,
        proxy: None,
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
/// 还在后面。`id` 是这个请求的号，做了转换、丢了字段要报在它上面。
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
    let mut auto_cache: Option<String> = None;
    // 转换过格式就是；同格式直通时看有没有去掉字段
    let mut reshaped = target.is_some();
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
                // 客户端自动填的身份字段不发（见 `egress` 模块）。**不算丢弃的字段**。按字节
                // 剪，要请求体解得开：上面几步要么原样交回、要么照解出来的写回，解不开的照旧
                // 解不开、原样发
                client_dialect
                    .filter(|_| asked.json)
                    .and_then(|d| crate::egress::strip_body_identity(d, &out))
                    .unwrap_or(out)
            };
            reshaped = !dropped.is_empty();
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
                    provider,
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
            // 客户端一个断点都没标：请求体里的断点是编码器自动标的。这一家的这个模型拒过的话
            // 先去掉，不用再被拒一次（见 `crate::cache_marks`）
            let mut encoded = p.body.clone();
            if d.request.cache.is_empty() && tw_dialect::cache::may_have_marks(dialect, &encoded) {
                let model = &d.request.model;
                if !state.cache_marks.refused(&provider.name, model) {
                    auto_cache = Some(model.clone());
                } else if let Some(stripped) = tw_dialect::cache::strip_marks(dialect, &encoded) {
                    tracing::debug!(
                        provider = %provider.name,
                        model = %model,
                        "left out the cache breakpoints this upstream refused before"
                    );
                    encoded = stripped;
                }
            }
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
                Bytes::from(crate::chatgpt::force_stream(encoded))
            } else if let Some(b) = tw_bedrock::beta::with_betas(&encoded, &betas) {
                Bytes::from(b)
            } else if harness && to_deepseek {
                // 转换成另一种格式发给 DeepSeek 官方：直连时它收得到的扩展照样带上
                Bytes::from(tw_dialect::harness::carry(asked.body, &encoded).unwrap_or(encoded))
            } else {
                Bytes::from(encoded)
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
        auto_cache,
        reshaped,
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
    /// 这一跳在报文记录里记到哪儿。再发的那一遍不再是客户端那一份（[`crate::content::Tap::resent`]）
    tap: &'a crate::content::Tap,
}

/// 上游回 400、拒绝了转换时自动标的提示缓存断点（见 [`crate::cache_marks`]）：去掉，同一家
/// 再发一次，只一次。记下这一家的这个模型不认，往后发给它之前先去掉。
///
/// 客户端自己标了断点的不管（[`Outbound::auto_cache`] 是 None）：那是它的决定，被拒了原样
/// 交回去。不是这种 400 的也原样交回去。返回再发时用的请求体，没再发是 None
async fn resend_uncached(
    state: &AppState,
    req: &Inbound,
    provider: &tw_config::Provider,
    http: &reqwest::Client,
    resend: Resend<'_>,
    r: reqwest::Response,
) -> Result<(reqwest::Response, Option<Bytes>), SendError> {
    let (Some(model), Some(wire)) = (resend.out.auto_cache.as_deref(), wire(req, resend.out))
    else {
        return Ok((r, None));
    };
    if r.status() != 400 {
        return Ok((r, None));
    }
    let Some(stripped) = tw_dialect::cache::strip_marks(wire, resend.body) else {
        return Ok((r, None));
    };
    let status = r.status();
    let headers = r.headers().clone();
    let said = r.bytes().await.map_err(SendError::Http)?;
    if !crate::cache_marks::refusal(&said) {
        let mut back = http::Response::new(said);
        *back.status_mut() = status;
        *back.headers_mut() = headers;
        return Ok((reqwest::Response::from(back), None));
    }
    state
        .cache_marks
        .note(&provider.name, model, crate::server::now_ms());
    tracing::info!(
        provider = %provider.name,
        model = %model,
        "the upstream refused the cache breakpoints added on conversion; sending again without them"
    );
    let stripped = Bytes::from(stripped);
    let r = send(
        state,
        req,
        provider,
        http,
        resend.out,
        stripped.clone(),
        resend.headers.to_vec(),
        resend.aws,
        &resend.tap.resent(),
    )
    .await?;
    Ok((r, Some(stripped)))
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
        &resend.tap.resent(),
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
    tap: &crate::content::Tap,
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
    // Codex 后端收 zstd 压缩的请求体（见 `chatgpt::compress`）。报文记录记压缩之前的
    let plain = body.clone();
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
    // 发出去的请求体按线上的样子记：压缩过的是压缩之后的（见 `crate::traffic`）
    let (traffic, size) = (&req.traffic, body.len());
    if let Some((credentials, region)) = signer {
        let mut request = build(&upstream_headers)
            .body(body.clone())
            .build()
            .map_err(SendError::Http)?;
        crate::bedrock::sign_request(&mut request, credentials, region).map_err(SendError::Sign)?;
        tap.sending(&request, &plain, &upstream_headers);
        return dispatch(traffic, size, http.execute(request))
            .await
            .map_err(SendError::Http);
    }
    // 定稿了再发：报文记录记下真正发出去的请求头
    let request = build(&upstream_headers)
        .body(body.clone())
        .build()
        .map_err(SendError::Http)?;
    tap.sending(&request, &plain, &upstream_headers);
    let mut sent = dispatch(traffic, size, http.execute(request)).await;
    // **OAuth 上游回 401：换一个 access token 再发一次，只一次。**token 可能在别处被
    // 吊销了、提前失效了；不重试的话，这个请求连同之后每一个请求都会原样失败，直到
    // 缓存里那个 token 按时间过期。换回来的还是 401，说明问题不在 token
    if provider.oauth.is_some() && matches!(&sent, Ok(r) if r.status() == 401) {
        match state.headers_after_401(provider, http, sent_at).await {
            // 换回来的还是同一个（刚换过不久）：再发一次也是 401
            Ok(fresh) if fresh != upstream_headers => match build(&fresh).body(body).build() {
                Ok(request) => {
                    tap.sending(&request, &plain, &fresh);
                    sent = dispatch(traffic, size, http.execute(request)).await;
                }
                Err(e) => sent = Err(e),
            },
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(provider = %provider.name, "the upstream answered 401 and the token could not be renewed: {e}")
            }
        }
    }
    sent.map_err(SendError::Http)
}

/// 发出去一次：请求体（`size` 字节）记进这个请求的流量，响应套上一层、读到的每一块都记上
/// （见 [`crate::traffic`]）；压缩着回的在计数之外再套一层解开（见 [`crate::inflate`]），
/// 后面读它的每一处拿到的都是解开的字节。**连不上的退回**：地址不通、代理拒绝、握手失败时
/// 上游一个字节都没收到。别的错误（超时、发到一半断了）照记：请求体多半已经出去了
async fn dispatch(
    traffic: &std::sync::Arc<crate::traffic::Traffic>,
    size: usize,
    sending: impl std::future::Future<Output = reqwest::Result<reqwest::Response>>,
) -> reqwest::Result<reqwest::Response> {
    traffic.sending(size);
    match sending.await {
        Ok(r) => Ok(crate::inflate::inflated(crate::traffic::metered(
            r, traffic,
        ))),
        Err(e) => {
            if e.is_connect() {
                traffic.unsent(size);
            }
            Err(e)
        }
    }
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
    // 正文换成了我们自己的一句话：说明原来那份正文长什么样的头都不能留
    for h in [
        "content-length",
        "content-type",
        "content-encoding",
        "transfer-encoding",
    ] {
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

/// 一个流式回答在第一段内容之前就收了尾，收尾的那个事件是 `data`：这算不算 `provider` 的错、
/// 按什么原因停用。**和这里开头报错的一跳同一个判据**（[`super::opening`] 读出状态码，
/// [`crate::failure::classify`] 判断）：5xx、限流、额度用完、凭据被拒是 `Some`；请求本身的
/// 问题、不是错误的是 `None` —— 那是这一家答上了。WebSocket 上 Responses 的一轮用它（见
/// `crate::ws::turn`）
pub(crate) fn stream_fault(
    state: &AppState,
    provider: &str,
    dialect: tw_dialect::ir::Dialect,
    data: &str,
) -> Option<Cause> {
    let (status, body) = super::opening::stream_error(dialect, data)?;
    match crate::failure::classify(status, &http::HeaderMap::new(), &body, now_ms()) {
        Verdict::Failed(cause) => Some(known_reset(state, provider, cause)),
        Verdict::ClientError => None,
    }
}

/// 一跳的响应头到了之后怎么等开头（见 [`super::opening`]）。
enum Hold {
    /// 流式：等第一段内容。上游说的是哪种格式、是不是 Bedrock 的二进制帧
    Stream(tw_dialect::ir::Dialect, bool),
    /// 整包：等正文的第一个字节
    Body,
}

/// 这个回答要不要等开头：生成回答的成功响应才等。流式的等第一段内容，整包的等正文开头
fn hold_of(
    req: &Inbound,
    reading: &crate::client_api::Reading,
    provider: &tw_config::Provider,
    out: &Outbound,
    r: &reqwest::Response,
) -> Option<Hold> {
    if !r.status().is_success() || !reading.generates {
        return None;
    }
    let ct = r
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let eventstream =
        provider.is_bedrock() && ct.starts_with(tw_bedrock::eventstream::CONTENT_TYPE);
    // Codex 后端的流式响应没有 Content-Type（发给它的一律是流，见 `relay::Plan`）
    let sse = ct.starts_with("text/event-stream") || (out.chatgpt && ct.is_empty());
    if !(eventstream || sse) {
        return Some(Hold::Body);
    }
    let dialect = out.target.or_else(|| req.api.map(|a| a.dialect()))?;
    Some(Hold::Stream(dialect, eventstream))
}

/// 发出去、等响应头的结局
enum Waited<T> {
    Done(T),
    /// 被手动中止了
    Aborted,
    /// 无响应超时到了
    Quiet,
}

/// 无响应超时之前一个字都没给的那一跳
struct Abandoned<'p> {
    provider: &'p tw_config::Provider,
    model: Option<String>,
    /// 上游回的状态码，响应头没到的没有
    status: Option<u16>,
    /// 流开头里上游报的用量
    usage: Option<tw_dialect::usage::Usage>,
    quiet: super::idle::Quiet,
    hop_started: std::time::Instant,
}

/// 放弃这一家（见 [`super::idle`]）：记一次失败、记它被给的那段时间、这段对话这一轮不再留在
/// 它那儿，尝试链上记一跳 `idle_timeout`。交回候选用完时交给客户端的那条错误（504）
fn gave_up(
    state: &AppState,
    started: &Started,
    reading: &crate::client_api::Reading,
    a: Abandoned<'_>,
    chain: &mut Vec<tw_api::AttemptView>,
) -> GatewayError {
    let name = &a.provider.name;
    note_health(
        &state.bus,
        &state.health,
        name,
        state.health.record_failure(name),
    );
    super::idle::timed_out(state, name, a.quiet.window);
    if let Some(c) = &started.turn {
        state.affinity.left(c, name);
    }
    chain.push(super::idle::abandoned(
        name,
        a.model,
        a.status,
        a.usage,
        reading,
        a.quiet,
        a.hop_started,
    ));
    GatewayError::timeout(super::idle::said(name, a.quiet.secs))
}

fn now_ms() -> u64 {
    u64::try_from(tw_breaker::now_ms()).unwrap_or(0)
}
