//! 数据面守卫：出站脱敏、请求防护（藏匿字符、内容过滤）和输出长度的接线。
//!
//! 规则本身住在 [`tw_guard::redact`] 里，这个文件只回答一个问题：**一个请求体该
//! 怎么处理。**
//!
//! # 全局的，对所有上游一视同仁
//!
//! 档位和规则不再随上游变化：以前「脱哪几类」写在每个上游上（官方端点默认
//! 不脱），路由规则还能再加一层，于是没人说得清一个请求到底按什么规格走。
//!
//! **观察档和拦截档用的是同一套规则。**以前观察档按全部类别检测、拦截档按
//! 上游的类别替换，于是同一个请求观察时报「检测到」，切到拦截后一处不换 ——
//! 用户看到的证据，和他切过去之后得到的保护，说的不是一件事。
//!
//! 两步分开：[`find`] 在尝试上游之前对客户端发来的原文看一遍，报出去的记录
//! 只有这一份；[`replace`] 在每一跳发出去之前替换 —— 那一跳的请求体可能是
//! 转换过格式的，要换的是真正发出去的那一份。

use tw_config::SecurityMode as Mode;
use tw_guard::redact::replace::{Ledger, Scheme};
use tw_guard::redact::rules::{Finding, RuleSet};

/// 找一遍。**观察档和拦截档都找**，关闭时不找。
///
/// **不是 UTF-8 就不看。**图片之类的二进制体里不会有粘贴进来的 key。
pub fn find(mode: Mode, rules: &RuleSet, body: &[u8]) -> Vec<Finding> {
    if !mode.detects() || rules.is_empty() {
        return Vec::new();
    }
    let Ok(text) = std::str::from_utf8(body) else {
        return Vec::new();
    };
    let hits = tw_guard::redact::rules::scan(text, rules);
    tw_guard::redact::rules::findings(text, &hits)
}

/// 拦截档下换掉要发出去的这一份。返回换过的体和还原用的账本；**不在拦截档、
/// 或者没找到东西时与进来时逐字节相同**，账本是空的。
pub fn replace(mode: Mode, rules: &RuleSet, body: bytes::Bytes) -> (bytes::Bytes, Ledger) {
    if !mode.acts() || rules.is_empty() {
        return (body, Ledger::new(Scheme::SECRET));
    }
    // 按字节乱切一个非 UTF-8 的体，得到的是一份坏掉的请求
    let Ok(text) = std::str::from_utf8(&body) else {
        return (body, Ledger::new(Scheme::SECRET));
    };
    let hits = tw_guard::redact::rules::scan(text, rules);
    if hits.is_empty() {
        // 没命中就原样返回，连一次拷贝都不做
        return (body, Ledger::new(Scheme::SECRET));
    }
    let r = tw_guard::redact::replace::apply(text, &hits, Ledger::new(Scheme::SECRET));
    (bytes::Bytes::from(r.text), r.ledger)
}

/// 找到的东西写成事件里的样子。
pub fn items(found: &[Finding]) -> Vec<tw_api::SecretItem> {
    found
        .iter()
        .map(|f| tw_api::SecretItem {
            rule: f.rule.id().to_string(),
            custom: f.rule.custom(),
            kind: f.rule.kind().slug().to_string(),
            masked: f.masked.clone(),
            count: f.count,
        })
        .collect()
}

/// 请求防护此刻的档位和规则：藏匿字符和内容过滤。
///
/// **HTTP 和 WebSocket 两条路共用**（见 [`screen`]）。升级那一刻取一次，一条连接
/// 活多久就按它开始时的配置走多久。
#[derive(Clone)]
pub struct Screen {
    pub hidden_mode: Mode,
    pub hidden: Vec<tw_guard::hidden::Kind>,
    pub content_mode: Mode,
    pub content: std::sync::Arc<tw_guard::content::Rules>,
}

impl Screen {
    pub fn of(rt: &crate::state::Runtime) -> Self {
        let sec = &rt.config.security;
        Self {
            hidden_mode: sec.hidden_text.mode,
            hidden: rt.hidden.clone(),
            content_mode: sec.content.mode,
            content: rt.content.clone(),
        }
    }
}

/// 看一遍调用方发来的正文（连同工具结果）：藏匿字符、内容规则。
///
/// **两项都看完、都报完再下结论** —— 一个请求既藏了字符又命中了规则，日志里两件
/// 事都该在。拦截档下该拒的话返回给客户端的那句话；藏匿字符排在前面，它几乎不会
/// 误报。
pub fn screen(
    bus: &tw_observe::EventBus,
    id: u64,
    provider: &str,
    s: &Screen,
    request: &tw_dialect::ir::Request,
) -> Option<tw_types::Msg> {
    let hidden = if s.hidden_mode.detects() {
        tw_guard::hidden::scan_request(request, &s.hidden)
    } else {
        Vec::new()
    };
    let mut refusal = hidden_found(bus, id, provider, s.hidden_mode, &hidden);
    if s.content_mode.detects() && !s.content.is_empty() {
        let hits = s.content.scan_request(request);
        let refused = content_matched(bus, id, provider, s.content_mode, &hits);
        refusal = refusal.or(refused);
    }
    refusal
}

/// 没法按消息结构读的正文（解不开的 WebSocket 帧）：**只查藏匿字符** —— 它在任何
/// 地方都没有正当用途；内容规则按整段原文查的话，系统提示里的话也会被当成调用方的。
pub fn screen_text(
    bus: &tw_observe::EventBus,
    id: u64,
    provider: &str,
    s: &Screen,
    text: &str,
) -> Option<tw_types::Msg> {
    if !s.hidden_mode.detects() {
        return None;
    }
    let mut found = Vec::new();
    tw_guard::hidden::scan_smuggled(text, false, &s.hidden, &mut found);
    hidden_found(bus, id, provider, s.hidden_mode, &found)
}

fn hidden_found(
    bus: &tw_observe::EventBus,
    id: u64,
    provider: &str,
    mode: Mode,
    found: &[tw_guard::hidden::Smuggled],
) -> Option<tw_types::Msg> {
    if found.is_empty() {
        return None;
    }
    let blocked = mode.acts();
    tracing::warn!(
        provider,
        blocked,
        kinds = ?found.iter().map(|f| f.kind.slug()).collect::<Vec<_>>(),
        "the request carries invisible characters"
    );
    bus.emit(tw_api::Event::HiddenTextFound {
        id,
        provider: provider.to_string(),
        blocked,
        items: found
            .iter()
            .map(|f| tw_api::HiddenItem {
                kind: f.kind.slug().to_string(),
                in_tool_result: f.in_tool_result,
                count: f.count as u64,
                example: f.example.clone(),
                revealed: f.revealed.clone(),
            })
            .collect(),
        at_ms: crate::server::now_ms(),
    });
    if !blocked {
        return None;
    }
    let mut kinds: Vec<&str> = found.iter().map(|f| f.kind.slug()).collect();
    kinds.dedup();
    let kinds = kinds.join(", ");
    // 在工具结果里和在调用方自己打的字里，是两句话：前者要去查是哪个工具抓回来的
    Some(if found.iter().any(|f| f.in_tool_result) {
        tw_types::msg!(
            "gw.hidden_text.refused_tool_result", kinds = kinds =>
            "A tool result in this request contains invisible characters that can hide \
             instructions from a reader ({kinds}), so the request was not sent."
        )
    } else {
        tw_types::msg!(
            "gw.hidden_text.refused_message", kinds = kinds =>
            "The message contains invisible characters that can hide instructions from a \
             reader ({kinds}), so the request was not sent."
        )
    })
}

fn content_matched(
    bus: &tw_observe::EventBus,
    id: u64,
    provider: &str,
    mode: Mode,
    hits: &[tw_guard::content::Hit],
) -> Option<tw_types::Msg> {
    use tw_guard::content::Action;
    let worst = tw_guard::content::worst(hits)?;
    // 规则是拦 + 拦截档 = 拒
    let refuse = mode.acts() && worst.action == Action::Block;
    for h in hits {
        let blocking = h.action == Action::Block;
        bus.emit(tw_api::Event::ContentMatched {
            id,
            provider: provider.to_string(),
            rule: h.rule.clone(),
            custom: h.custom,
            action: if blocking {
                tw_api::RuleAction::Block
            } else {
                tw_api::RuleAction::Record
            },
            blocked: refuse && blocking,
            in_tool_result: h.in_tool_result,
            excerpt: h.snippet.clone(),
            at_ms: crate::server::now_ms(),
        });
    }
    // 命中的原文是调用方的正文，**不进应用日志**：日志只说哪条规则
    tracing::info!(
        provider,
        refused = refuse,
        rules = ?hits.iter().map(|h| h.rule.as_str()).collect::<Vec<_>>(),
        "the request matched content rules"
    );
    refuse.then(|| {
        tw_types::msg!(
            "gw.content.refused",
            rule = worst.rule.clone(), name = worst.name.clone(), excerpt = worst.snippet.clone() =>
            "Content rule “{name}” matched this request (“{excerpt}”), so it was not sent."
        )
    })
}

/// 回答超过了输出长度：报一条，拦截档下给出切断时告诉客户端的那句话。
///
/// `whole`：整包（整份没发）还是流（从那一帧起没发）—— 两句话。
pub fn output_limited(
    bus: &tw_observe::EventBus,
    id: u64,
    provider: &str,
    mode: Mode,
    max: usize,
    seen: usize,
    whole: bool,
) -> Option<tw_types::Msg> {
    let cut = mode.acts();
    tracing::warn!(
        provider,
        max,
        seen,
        cut,
        "the answer passed the output limit"
    );
    bus.emit(tw_api::Event::OutputLimited {
        id,
        provider: provider.to_string(),
        max_chars: max as u64,
        seen_chars: seen as u64,
        cut,
        at_ms: crate::server::now_ms(),
    });
    if !cut {
        return None;
    }
    Some(if whole {
        tw_types::msg!(
            "gw.output_limit.withheld", upstream = provider.to_string(), max = max, seen = seen =>
            "The answer from upstream `{upstream}` is {seen} characters, over the output limit of \
             {max}, so it was withheld."
        )
    } else {
        tw_types::msg!(
            "gw.output_limit.cut", upstream = provider.to_string(), max = max =>
            "The answer from upstream `{upstream}` passed the output limit of {max} characters, \
             so it was cut off."
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn body() -> bytes::Bytes {
        bytes::Bytes::from(format!(
            "{{\"messages\":[{{\"content\":\"我的 key 是 {KEY}\"}}]}}"
        ))
    }

    #[test]
    fn enforce_replaces_with_a_placeholder_and_keeps_the_body_valid_json() {
        let (out, ledger) = replace(Mode::Enforce, &RuleSet::defaults(), body());
        let text = String::from_utf8(out.to_vec()).unwrap();
        assert!(!text.contains(KEY), "{text}");
        assert!(text.contains("<<TW_SECRET_1>>"), "{text}");
        assert_eq!(ledger.len(), 1);
        // 换完还得是合法 JSON —— 占位符里没有需要转义的字符
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(
            v["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("<<TW_SECRET_1>>")
        );
    }

    #[test]
    fn observe_finds_what_enforce_would_replace_and_changes_nothing() {
        // 观察档**只记录，不改变任何行为**；它报的和拦截档会换的是同一批
        let seen = find(Mode::Observe, &RuleSet::defaults(), &body());
        assert_eq!(seen.len(), 1);
        assert!(!seen[0].masked.contains("AAAAAAAAAAAA"));
        assert_eq!(seen, find(Mode::Enforce, &RuleSet::defaults(), &body()));
        let (out, ledger) = replace(Mode::Observe, &RuleSet::defaults(), body());
        assert_eq!(out, body());
        assert!(ledger.is_empty());
    }

    #[test]
    fn off_does_not_even_look() {
        assert!(find(Mode::Off, &RuleSet::defaults(), &body()).is_empty());
        let (out, _) = replace(Mode::Off, &RuleSet::defaults(), body());
        assert_eq!(out, body());
    }

    #[test]
    fn a_binary_body_is_left_alone_instead_of_being_mangled() {
        // 按字节乱切一个非 UTF-8 的体，得到的是一份坏掉的请求。
        let raw = bytes::Bytes::from(vec![0xff, 0xfe, 0x00, 0x01]);
        let (out, l) = replace(Mode::Enforce, &RuleSet::defaults(), raw.clone());
        assert_eq!(out, raw);
        assert!(l.is_empty());
        assert!(find(Mode::Enforce, &RuleSet::defaults(), &raw).is_empty());
    }

    #[test]
    fn a_body_with_nothing_to_redact_is_returned_untouched() {
        let plain = bytes::Bytes::from_static(b"{\"messages\":[]}");
        let (out, l) = replace(Mode::Enforce, &RuleSet::defaults(), plain.clone());
        assert_eq!(out, plain);
        assert!(l.is_empty());
    }

    #[test]
    fn the_event_items_name_the_rule_and_never_carry_the_value() {
        let it = items(&find(Mode::Observe, &RuleSet::defaults(), &body()));
        assert_eq!(it[0].rule, "anthropic-api-key");
        assert_eq!(it[0].kind, "api-keys");
        assert!(!it[0].custom);
        assert!(!it[0].masked.contains("AAAAAAAAAAAA"), "{}", it[0].masked);
    }
}
