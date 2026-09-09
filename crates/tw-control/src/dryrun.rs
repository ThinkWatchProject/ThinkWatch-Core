//! 路由试算（DESIGN.md §3.4、§11 的 M4）。
//!
//! 回答的不是「会走到哪儿」，而是**「为什么没走我以为的那条」**。这两个
//! 是同一个问题的两面，而后者才是用户真正在问的 —— 所以每条没命中的
//! 规则也要列出来，并说清它卡在哪一步。
//!
//! **它只算，不发任何请求**，也不改任何状态。

use axum::{Json, extract::State, http::StatusCode};
use tw_engine::{Outcome, RequestFacts, RouteError};

use crate::ControlState;

type Fail = (StatusCode, String);

fn facts(req: &tw_api::DryRunRequest, fallback_client: &str) -> RequestFacts {
    RequestFacts {
        model: req.model.clone(),
        client: if req.client.is_empty() {
            fallback_client.to_string()
        } else {
            req.client.clone()
        },
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
    let f = facts(
        &req,
        cfg.clients.first().map(|c| c.name.as_str()).unwrap_or(""),
    );

    // 每条规则的下场。**先走一遍这个，再问结果** —— 顺序反过来的话，
    // 「命中了哪条」会变成唯一的输出，而那正是不够用的那半个答案。
    let mut trace = Vec::new();
    for r in engine.routes() {
        if r.when.is_phase_two() {
            trace.push(tw_api::RuleTrace {
                name: r.name.clone(),
                verdict: "phase_two".into(),
                // 阶段二的条件要等路由决定完才知道，静态试算给不了结论
                why: Some("它的条件要等选完上游才知道，这一轮先跳过".into()),
            });
            continue;
        }
        match r.when.matches(&f) {
            Ok(true) => trace.push(tw_api::RuleTrace {
                name: r.name.clone(),
                verdict: "matched".into(),
                why: None,
            }),
            Ok(false) => trace.push(tw_api::RuleTrace {
                name: r.name.clone(),
                verdict: "skipped".into(),
                why: Some(unmatched(&r.when, &f)),
            }),
            Err(e) => trace.push(tw_api::RuleTrace {
                name: r.name.clone(),
                verdict: "skipped".into(),
                why: Some(e.to_string()),
            }),
        }
    }

    let mut out = tw_api::DryRunResult {
        outcome: "no_match".into(),
        rule: None,
        reason: None,
        candidates: Vec::new(),
        via_group: None,
        set: Vec::new(),
        trace,
        hurts_cache: false,
        circuit_open: Vec::new(),
    };

    match engine.route(&f) {
        Ok(Outcome::Route(d)) => {
            out.outcome = "route".into();
            out.rule = Some(d.matched_rule.clone());
            out.via_group = d.via_group.clone();
            out.hurts_cache = d
                .via_group
                .as_deref()
                .and_then(|g| engine.groups().iter().find(|x| x.name == g))
                .map(|g| g.kind.hurts_cache())
                .unwrap_or(false);
            // **熔断是当下的事实，不是静态结论。**试算说「会走 A」，而 A
            // 此刻正熔断着 —— 不说出来的话，用户会拿着一个对的答案去查
            // 一个错的现象。
            out.circuit_open = d
                .candidates
                .iter()
                .filter(|c| !s.health().is_available(c))
                .cloned()
                .collect();
            out.set = describe(&d.set);
            out.candidates = d.candidates;
        }
        Ok(Outcome::Deny { rule, reason }) => {
            out.outcome = "deny".into();
            out.rule = Some(rule);
            out.reason = Some(reason);
        }
        Err(RouteError::NoMatch) => {
            out.outcome = "no_match".into();
            out.reason = Some(RouteError::NoMatch.to_string());
        }
        Err(e) => return Err((StatusCode::BAD_REQUEST, e.to_string())),
    }
    Ok(Json(out))
}

fn describe(set: &tw_engine::SetAction) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(m) = &set.model {
        // 换模型会作废整个 prompt cache，而这件事在长会话里可能比不换
        // 还贵（§3.4）—— 试算里就要说出来
        v.push(format!("换模型 → {m}（会作废整个 prompt cache）"));
    }
    if let Some(t) = set.max_tokens {
        v.push(format!("max_tokens → {t}"));
    }
    if let Some(t) = set.thinking {
        v.push(format!("thinking → {t}"));
    }
    if set.only_at_session_start {
        v.push("只在新会话开始时应用".into());
    }
    v
}

/// 这条规则卡在哪个条件上。
///
/// **逐条试，报第一个不满足的。**报「不匹配」等于什么都没说 —— 用户看
/// 试算就是为了知道差在哪儿。
fn unmatched(when: &tw_engine::rule::When, f: &RequestFacts) -> String {
    let eq = |want: &Option<String>, got: &str| -> Option<String> {
        match want {
            Some(w) if w != got => Some(format!("要求 `{w}`，实际是 `{got}`")),
            _ => None,
        }
    };
    let b = |want: Option<bool>, got: bool, name: &str| -> Option<String> {
        match want {
            Some(w) if w != got => Some(format!("要求 {name}={w}，实际是 {got}")),
            _ => None,
        }
    };
    if let Some(w) = &when.model
        && !tw_engine::rule::glob_match(w, &f.model)
    {
        return format!("model 要匹配 `{w}`，实际是 `{}`", f.model);
    }
    if let Some(m) = eq(&when.client, &f.client) {
        return format!("client {m}");
    }
    if let Some(m) = eq(&when.dialect, &f.dialect) {
        return format!("dialect {m}");
    }
    if let Some(w) = &when.intent
        && !w.contains(&f.intent)
    {
        return format!(
            "intent 要在 {w:?} 里，实际是 `{}`",
            if f.intent.is_empty() {
                "（真实用户请求）"
            } else {
                &f.intent
            }
        );
    }
    for (want, got, name) in [
        (when.cache, f.cache, "cache"),
        (when.tools, f.tools, "tools"),
        (when.image, f.image, "image"),
        (when.thinking, f.thinking, "thinking"),
        (when.stream, f.stream, "stream"),
    ] {
        if let Some(m) = b(want, got, name) {
            return m;
        }
    }
    for (want, got, name) in [
        (&when.input_tokens, f.input_tokens as f64, "input_tokens"),
        (&when.tool_count, f.tool_count as f64, "tool_count"),
        (
            &when.max_tokens,
            f.max_tokens.unwrap_or(0) as f64,
            "max_tokens",
        ),
    ] {
        if let Some(w) = want {
            return format!("{name} 要满足 `{w}`，实际是 {got}");
        }
    }
    // **阶段二的条件不在这里**：它们在上面就被单独归类了。走到这儿说明
    // 有个条件我们没覆盖到 —— 与其编一句，不如承认。
    "有条件没对上（这条 ThinkWatch 还没能给出更具体的解释）".to_string()
}
