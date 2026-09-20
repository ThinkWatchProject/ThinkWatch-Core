//! 路由试算（M4）。
//!
//! 回答的不是「会走到哪儿」，而是**「为什么没走我以为的那条」**。这两个
//! 是同一个问题的两面，而后者才是用户真正在问的 —— 所以每条没命中的
//! 规则也要列出来，并说清它卡在哪一步。
//!
//! **它只算，不发任何请求**，也不改任何状态。

use axum::{Json, extract::State, http::StatusCode};

/// 和数据面同一段排序。
///
/// 试算页存在的全部意义是「告诉你这条请求会走哪儿」，所以它**必须**用
/// 同一个函数、同一份数字 —— 各算各的话，两边迟早会不一样，而那时
/// 试算比没有更糟。
fn order_like_the_data_plane(
    s: &crate::ControlState,
    engine: &tw_engine::Engine,
    d: &tw_engine::Decision,
    f: &tw_engine::RequestFacts,
) -> Vec<String> {
    let Some(gname) = d.via_group.clone() else {
        return d.candidates.clone();
    };
    let Some(kind) = engine
        .groups()
        .iter()
        .find(|g| g.name == gname)
        .map(|g| g.kind)
    else {
        return d.candidates.clone();
    };
    if !kind.needs_runtime() {
        return d.candidates.clone();
    }
    let cfg = s.config();
    let facts = tw_engine::Facts {
        session: None,
        seq: s.gateway.bus.peek_id(),
        ttfb_ms: s.gateway.latency.snapshot(&d.candidates),
        price: s
            .gateway
            .unit_prices(&cfg.providers, &d.candidates, &f.model),
    };
    engine.order(Some(&gname), &d.candidates, &facts)
}
use tw_engine::{Outcome, RequestFacts, RouteError};

use crate::ControlState;
use tw_types::msg;

use crate::{Fail, fail};

fn facts(req: &tw_api::DryRunRequest) -> RequestFacts {
    RequestFacts {
        model: req.model.clone(),
        client: req.client.clone(),
        dialect: req.dialect.clone(),
        input_tokens: req.input_tokens,
        max_tokens: req.max_tokens,
        cache: req.cache,
        tools: req.tools,
        tool_count: req.tool_count,
        image: req.image,
        thinking: req.thinking,
        stream: req.stream,
        intent: req.intent.clone(),
    }
}

pub async fn dry_run(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::DryRunRequest>,
) -> Result<Json<tw_api::DryRunResult>, Fail> {
    let cfg = s.config();
    let rt = s.gateway.runtime();
    let engine = &rt.engine;
    let f = facts(&req);
    if !f.client.is_empty() && !cfg.clients.iter().any(|c| c.name == f.client) {
        return Err(fail(
            StatusCode::NOT_FOUND,
            msg!(
                "control.key_not_found", key = f.client.clone() =>
                "There is no gateway key named `{key}`."
            ),
        ));
    }

    // 按哪张规则表求值：**未保存的草稿 > 指定的路由 > 密钥使用的路由**。
    // 以前密钥为空时悄悄取配置里的第一把，而那一把可能指定了另一条路由 ——
    // 试算给出一个对的答案，回答的却是另一个问题。
    let draft;
    let (route, rules): (String, &[tw_engine::Rule]) = if let Some(d) = &req.draft {
        draft = d
            .rules
            .iter()
            .map(|r| crate::routes::to_rule(r, &cfg))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                fail(
                    StatusCode::BAD_REQUEST,
                    msg!("control.bad_rule", detail = e => "{detail}"),
                )
            })?;
        engine.check_rules(&draft).map_err(|e| {
            fail(
                StatusCode::BAD_REQUEST,
                msg!("control.bad_rule", detail = e => "{detail}"),
            )
        })?;
        (d.name.clone(), draft.as_slice())
    } else if let Some(name) = req.route.as_deref().filter(|n| !n.is_empty()) {
        let rules = engine
            .rules_of(name)
            .ok_or_else(|| {
                fail(
                    StatusCode::NOT_FOUND,
                    msg!("control.route_not_found", route = name => "There is no route named `{route}`."),
                )
            })?;
        (name.to_string(), rules)
    } else if !f.client.is_empty() {
        let name = engine.route_of(&f.client).to_string();
        (name, engine.rules_for_client(&f.client))
    } else {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            msg!("control.dryrun_needs_target" => "Name a gateway key or a route."),
        ));
    };

    // 每条规则的下场。**先走一遍这个，再问结果** —— 顺序反过来的话，
    // 「命中了哪条」会变成唯一的输出，而那正是不够用的那半个答案。
    // **只列这条路由里的规则。**试算问的是「我这个请求会怎么走」，
    // 而别的路由里的规则对这个请求没有任何影响 —— 列出来只会让人以为
    // 它们被跳过了，而实际上它们根本不在这条求值链上。
    let mut trace = Vec::new();
    let mut decided = false;
    for r in rules {
        if r.when.is_phase_two() {
            trace.push(tw_api::RuleTrace {
                name: r.name.clone(),
                // 阶段二的条件要等路由决定完才知道，静态试算给不了结论
                verdict: "phase_two".into(),
                mismatch: None,
                error: None,
                effect: None,
            });
            continue;
        }
        match r.when.matches(&f) {
            Ok(true) => {
                // 命中之后起了什么作用。**「命中了但没用上」要说出来** —— 兜底
                // 规则在试算里命中，而去向早已由前面的规则决定
                let effect = if !decided && r.decides() {
                    decided = true;
                    "decide"
                } else if r.adds() {
                    "apply"
                } else {
                    "none"
                };
                trace.push(tw_api::RuleTrace {
                    name: r.name.clone(),
                    verdict: "matched".into(),
                    mismatch: None,
                    error: None,
                    effect: Some(effect.into()),
                })
            }
            Ok(false) => trace.push(tw_api::RuleTrace {
                name: r.name.clone(),
                verdict: "skipped".into(),
                mismatch: unmatched(&r.when, &f),
                error: None,
                effect: None,
            }),
            Err(e) => trace.push(tw_api::RuleTrace {
                name: r.name.clone(),
                verdict: "skipped".into(),
                mismatch: None,
                error: Some(e.to_string()),
                effect: None,
            }),
        }
    }

    let mut out = tw_api::DryRunResult {
        route,
        outcome: "no_match".into(),
        strategy: None,
        rule: None,
        reason: None,
        candidates: Vec::new(),
        via_group: None,
        set: Vec::new(),
        trace,
        hurts_cache: false,
        circuit_open: Vec::new(),
        skipped: Vec::new(),
        converted: Vec::new(),
    };

    match engine.route_with(rules, &f) {
        Ok(Outcome::Route(mut d)) => {
            out.outcome = "route".into();
            out.rule = Some(d.matched_rule.clone());
            out.via_group = d.via_group.clone();
            // 按这个组的配置判断：开着会话粘滞的轮询组不伤缓存
            out.hurts_cache = d
                .via_group
                .as_deref()
                .and_then(|g| engine.groups().iter().find(|x| x.name == g))
                .is_some_and(|g| g.hurts_cache());
            out.set = describe(&d.set);
            out.strategy = d
                .via_group
                .as_deref()
                .and_then(|g| engine.groups().iter().find(|x| x.name == g))
                .map(|g| g.kind.slug().to_string());
            // 和数据面同一步：去掉服务不了这个请求的候选（停用的、范围外的、
            // 清单里没有这个模型的）。**被跳过的要列出来** —— 「规则明明写的
            // 是 A」正是用户会来试算的原因
            let serving = tw_gateway::models::serving(
                &rt.config,
                &s.gateway.catalog.load(),
                &d.candidates,
                &f.model,
            );
            out.skipped = serving
                .skipped
                .iter()
                .map(|(provider, why)| tw_api::SkippedView {
                    provider: provider.clone(),
                    reason: why.slug().to_string(),
                })
                .collect();
            // 服务不了的原因都在 `skipped` 里，不再另说一遍
            if serving.usable.is_empty() {
                out.outcome = "unavailable".into();
                return Ok(Json(out));
            }
            d.candidates = serving.usable;
            // **熔断是当下的事实，不是静态结论。**试算说「会走 A」，而 A
            // 此刻正熔断着 —— 不说出来的话，用户会拿着一个对的答案去查
            // 一个错的现象。
            out.circuit_open = d
                .candidates
                .iter()
                .filter(|c| !s.health().is_available(c))
                .cloned()
                .collect();
            // **顺序要和数据面一样，否则试算就是在撒谎。**`load-balance`
            // / `url-test` / `cheapest` 的次序由运行时的数字定，
            // 这里走的是同一个 `order`，喂的是同一份延迟表和价目表。
            //
            // 会话那一维**故意留空**：试算是「假设现在来一个请求」，
            // 而它属于哪次会话取决于请求正文，试算没有那个东西。
            // 于是它显示的是轮转序列里的当前位置 —— 而那正是一个没有
            // 会话指纹的请求真的会走的路。
            out.candidates = order_like_the_data_plane(&s, engine, &d, &f);
            // 哪些候选要转换格式。**试算里要说出来**：转换可能丢掉请求里的字段，
            // 而「规则把我分到了一个别的格式的上游」本身就是用户来试算想知道的事。
            // 协议认不出来的上游直通，不算
            out.converted = out
                .candidates
                .iter()
                .filter_map(|name| {
                    let p = rt.config.providers.iter().find(|p| &p.name == name)?;
                    // 比的是格式，不是协议：ChatGPT 账号说的就是 Responses 格式
                    let to = tw_gateway::translate::dialect_of(p.effective_protocol()?).slug();
                    (to != req.dialect).then(|| tw_api::ConvertedView {
                        provider: name.clone(),
                        from: req.dialect.clone(),
                        to: to.to_string(),
                    })
                })
                .collect();
        }
        Ok(Outcome::Deny { rule, reason }) => {
            out.outcome = "deny".into();
            out.rule = Some(rule);
            out.reason = Some(reason);
        }
        Err(RouteError::NoMatch) => {
            out.outcome = "no_match".into();
        }
        Err(e) => {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                msg!("control.route_failed", detail = e => "{detail}"),
            ));
        }
    }
    Ok(Json(out))
}

fn describe(set: &tw_engine::SetAction) -> Vec<tw_api::SetView> {
    let mut v = Vec::new();
    let mut push = |field: &str, value: String| {
        v.push(tw_api::SetView {
            field: field.to_string(),
            value,
        })
    };
    // 换模型会作废整个 prompt cache，而这件事在长会话里可能比不换还贵 ——
    // 界面上要说出来，所以它单独是一项，不和别的参数混在一起
    if let Some(m) = &set.model {
        push("model", m.clone());
    }
    if let Some(t) = set.max_tokens {
        push("max_tokens", t.to_string());
    }
    if let Some(t) = set.thinking {
        push("thinking", t.to_string());
    }
    if set.only_at_session_start {
        push("only_at_session_start", "true".to_string());
    }
    v
}

/// 这条规则卡在哪个条件上。
///
/// **逐条试，报第一个不满足的。**报「不匹配」等于什么都没说 —— 用户看
/// 试算就是为了知道差在哪儿。
fn unmatched(when: &tw_engine::rule::When, f: &RequestFacts) -> Option<tw_api::MismatchView> {
    let miss = |field: &str, want: Vec<String>, got: String| {
        Some(tw_api::MismatchView {
            field: field.to_string(),
            want,
            got,
        })
    };
    if let Some(w) = &when.model
        && !tw_engine::rule::glob_match(w, &f.model)
    {
        return miss("model", vec![w.clone()], f.model.clone());
    }
    for (field, want, got) in [
        ("client", &when.client, &f.client),
        ("dialect", &when.dialect, &f.dialect),
    ] {
        if let Some(w) = want
            && w != got
        {
            return miss(field, vec![w.clone()], got.clone());
        }
    }
    // 和 `When::matches` 一样：`assistant_internal` 是五类里的任意一类，而
    // 真实的用户请求（空）不属于任何一类
    if let Some(w) = &when.intent
        && (f.intent.is_empty() || !(w.contains(&f.intent) || w.contains("assistant_internal")))
    {
        // 实际值为空表示真实的用户请求，由界面说明
        return miss("intent", crate::one_or_many(w), f.intent.clone());
    }
    for (want, got, field) in [
        (when.cache, f.cache, "cache"),
        (when.tools, f.tools, "tools"),
        (when.image, f.image, "image"),
        (when.thinking, f.thinking, "thinking"),
        (when.stream, f.stream, "stream"),
    ] {
        if let Some(w) = want
            && w != got
        {
            return miss(field, vec![w.to_string()], got.to_string());
        }
    }
    // **逐个比较，报真没对上的那个。**以前这里报的是第一个写了的数量条件，
    // 哪怕它其实满足 —— 「要求输入 >100k，实际 200000」这种自相矛盾的话
    for (want, got, field) in [
        (&when.input_tokens, Some(f.input_tokens), "input_tokens"),
        (&when.tool_count, Some(f.tool_count as u64), "tool_count"),
        (&when.max_tokens, f.max_tokens, "max_tokens"),
    ] {
        let Some(w) = want else { continue };
        let Ok(cmp) = w.parse::<tw_engine::num::Compare>() else {
            continue;
        };
        match got {
            // 请求没写 max_tokens：任何比较都不满足。实际值留空，由界面说明
            None => return miss(field, vec![w.clone()], String::new()),
            Some(v) if !cmp.matches(v as f64) => {
                return miss(field, vec![w.clone()], v.to_string());
            }
            Some(_) => {}
        }
    }
    // **阶段二的条件不在这里**：它们在上面就被单独归类了。走到这儿说明
    // 有个条件没覆盖到 —— 与其编一句，不如承认说不出来。
    None
}
