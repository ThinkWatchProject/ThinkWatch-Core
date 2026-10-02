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
//! 两步分开：[`look`] 在尝试上游之前对客户端发来的原文看一遍，报出去的记录
//! 只有这一份；[`replace`] 在每一跳发出去之前替换 —— 那一跳的请求体可能是
//! 转换过格式的，要换的是真正发出去的那一份。
//!
//! # 一个值一个占位符，整个请求里都一样
//!
//! 拦截档下 [`look`] 按客户端原文里出现的先后给找到的值编好号，每一跳都接着这本账换。
//! 以前每一跳各起一本账，按那一跳发出去的那份的先后编号：转换过格式、字段换了顺序的
//! 那一跳，同一把密钥可能是 2 号，而上一跳、存下来的那份请求里它是 1 号 —— 请求详情里
//! 存的请求和回答对不上号。

use tw_config::SecurityMode as Mode;
use tw_guard::redact::replace::{Ledger, Scheme};
use tw_guard::redact::rules::{Finding, Hit, RuleSet};

/// 按规则找一遍，**不算我们自己的占位符**。
///
/// 连接串里写着 `postgres://app:<<TW_SECRET_2>>@db` 的那一段，在口令那条规则看来就是
/// 一个口令 —— 可它是我们换上去的：存下来的请求拿去重放、用户把详情里看到的东西贴回
/// 对话，都会带着它。当成凭据的话，它会被再换一次、在安全日志里报一次、落盘时被打成
/// `<<TW_…_2>>`。压在一个占位符上的命中都不算。
pub fn hits(text: &str, rules: &RuleSet) -> Vec<Hit> {
    let mut hits = tw_guard::redact::rules::scan(text, rules);
    if hits.is_empty() || !text.contains(Scheme::SECRET.open) {
        return hits;
    }
    let ours = Scheme::SECRET.find_in(text);
    hits.retain(|h| {
        !ours
            .iter()
            .any(|(at, _, _)| at.start < h.bytes.end && h.bytes.start < at.end)
    });
    hits
}

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
    tw_guard::redact::rules::findings(text, &hits(text, rules))
}

/// 一本新账，让开 `body` 里已经写着的占位符（见 [`Ledger::avoiding`]）。
///
/// 存下来的请求（拦截档下存的是换过的那一份）拿去重放时，里面写着的 1 号不能再发给
/// 新找到的值 —— 回显里的 1 号会被还原成那个新值。
pub fn ledger_for(body: &[u8]) -> Ledger {
    let fresh = Ledger::new(Scheme::SECRET);
    match std::str::from_utf8(body) {
        Ok(text) => fresh.avoiding(text),
        Err(_) => fresh,
    }
}

/// 看一遍客户端发来的原文：报出去的记录（同 [`find`]），和这个请求的账本。
///
/// **拦截档下账本在这里就编好号**：原文里找到的每个值按出现的先后发号，让开原文里本来
/// 就写着的占位符。之后每一跳都接着这本账换（[`replace`]），存下来的那份请求也照它换
/// （[`crate::bodies::Redaction`]）。不在拦截档时账本是空的。
pub fn look(mode: Mode, rules: &RuleSet, body: &[u8]) -> (Vec<Finding>, Ledger) {
    look_from(mode, rules, body, Ledger::new(Scheme::SECRET))
}

/// [`look`]，**接着 `seed` 的账编号**：插件跑过的请求，插件看到的占位符是按客户端原文
/// 编的（见 [`crate::plugin::bridge`]），改过之后的这一份接着那本账编，同一个值还是同一个
/// 号；插件改出来的新值接着往后编。
pub fn look_from(mode: Mode, rules: &RuleSet, body: &[u8], seed: Ledger) -> (Vec<Finding>, Ledger) {
    let empty = || Ledger::new(Scheme::SECRET);
    if !mode.detects() || rules.is_empty() {
        return (Vec::new(), empty());
    }
    let Ok(text) = std::str::from_utf8(body) else {
        return (Vec::new(), empty());
    };
    let hits = hits(text, rules);
    let found = tw_guard::redact::rules::findings(text, &hits);
    if !mode.acts() {
        return (found, empty());
    }
    let seed = seed.avoiding(text);
    let ledger = if hits.is_empty() {
        seed
    } else {
        tw_guard::redact::replace::apply(text, &hits, seed).ledger
    };
    (found, ledger)
}

/// 拦截档下换掉要发出去的这一份，**接着 `ledger` 的账**（见 [`look`]）。返回换过的体和
/// 还原用的账本；**不在拦截档、或者没找到东西时与进来时逐字节相同**，账本就是交进来的那本。
pub fn replace(
    mode: Mode,
    rules: &RuleSet,
    body: bytes::Bytes,
    ledger: &Ledger,
) -> (bytes::Bytes, Ledger) {
    if !mode.acts() || rules.is_empty() {
        return (body, ledger.clone());
    }
    // 按字节乱切一个非 UTF-8 的体，得到的是一份坏掉的请求
    let Ok(text) = std::str::from_utf8(&body) else {
        return (body, ledger.clone());
    };
    let hits = hits(text, rules);
    if hits.is_empty() {
        // 没命中就原样返回，连一次拷贝都不做
        return (body, ledger.clone());
    }
    let r = tw_guard::redact::replace::apply(text, &hits, ledger.clone());
    (bytes::Bytes::from(r.text), r.ledger)
}

/// `after` 里 `before` 没有的那些值：插件写进请求里的（见 [`crate::plugin::request`]）。
///
/// 按规则和打过码的样子比：同一个值在两份里打出来的码一样。客户端原话里就有的值，开头
/// 那一遍已经报过了，插件改过的那一份里再出现不再报一次。
pub fn more_found(before: &[Finding], after: Vec<Finding>) -> Vec<Finding> {
    after
        .into_iter()
        .filter(|f| {
            !before
                .iter()
                .any(|b| b.rule == f.rule && b.masked == f.masked)
        })
        .collect()
}

/// 找到的东西写成事件里的样子。
pub fn items(found: &[Finding]) -> Vec<tw_api::SecretItem> {
    found
        .iter()
        .map(|f| tw_api::SecretItem {
            rule: f.rule.id().to_string(),
            custom: f.rule.custom(),
            kind: crate::wire::secret_kind(f.rule.kind()),
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

/// 插件改过的请求再看一遍：**只看插件加进来的。**
///
/// 客户端的原话在开头已经看过（[`screen`]），该报的报了、该拒的拒了；插件改过的那一份
/// 要是整个再报一遍，同一处藏匿字符、同一条命中会在安全日志里出现两次。所以两份都扫，
/// 原话里就有的那几处减掉，剩下的照 [`screen`] 的规矩报、下结论 —— 拦截档下原话里命中
/// 「拦」的请求走不到这一步，这一遍拒不拒只看插件加进来的。
pub fn screen_more(
    bus: &tw_observe::EventBus,
    id: u64,
    provider: &str,
    s: &Screen,
    before: &tw_dialect::ir::Request,
    after: &tw_dialect::ir::Request,
) -> Option<tw_types::Msg> {
    let hidden = if s.hidden_mode.detects() {
        let was = tw_guard::hidden::scan_request(before, &s.hidden);
        let mut now = tw_guard::hidden::scan_request(after, &s.hidden);
        // 一种藏法在一个地方合成一条：插件往同一处又藏了几个，那一条就变了，整条再报
        now.retain(|n| {
            !was.iter().any(|w| {
                w.kind == n.kind
                    && w.in_tool_result == n.in_tool_result
                    && w.example == n.example
                    && w.revealed == n.revealed
                    && n.count <= w.count
            })
        });
        now
    } else {
        Vec::new()
    };
    let mut refusal = hidden_found(bus, id, provider, s.hidden_mode, &hidden);
    if s.content_mode.detects() && !s.content.is_empty() {
        let mut was = s.content.scan_request(before);
        let mut hits = s.content.scan_request(after);
        // 一处一处地减：原话里有一处，插件那一版里同样的一处就不是新的。位置不比 ——
        // 插件在前面加了字，后面的位置都挪了
        hits.retain(|h| {
            match was.iter().position(|w| {
                w.rule == h.rule
                    && w.custom == h.custom
                    && w.action == h.action
                    && w.snippet == h.snippet
                    && w.in_tool_result == h.in_tool_result
            }) {
                Some(i) => {
                    was.swap_remove(i);
                    false
                }
                None => true,
            }
        });
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
                kind: crate::wire::hidden_kind(f.kind),
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

    fn fresh() -> Ledger {
        Ledger::new(Scheme::SECRET)
    }

    #[test]
    fn enforce_replaces_with_a_placeholder_and_keeps_the_body_valid_json() {
        let (out, ledger) = replace(Mode::Enforce, &RuleSet::defaults(), body(), &fresh());
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
        let (out, ledger) = replace(Mode::Observe, &RuleSet::defaults(), body(), &fresh());
        assert_eq!(out, body());
        assert!(ledger.is_empty());
    }

    #[test]
    fn off_does_not_even_look() {
        assert!(find(Mode::Off, &RuleSet::defaults(), &body()).is_empty());
        let (out, _) = replace(Mode::Off, &RuleSet::defaults(), body(), &fresh());
        assert_eq!(out, body());
    }

    #[test]
    fn a_binary_body_is_left_alone_instead_of_being_mangled() {
        // 按字节乱切一个非 UTF-8 的体，得到的是一份坏掉的请求。
        let raw = bytes::Bytes::from(vec![0xff, 0xfe, 0x00, 0x01]);
        let (out, l) = replace(Mode::Enforce, &RuleSet::defaults(), raw.clone(), &fresh());
        assert_eq!(out, raw);
        assert!(l.is_empty());
        assert!(find(Mode::Enforce, &RuleSet::defaults(), &raw).is_empty());
    }

    #[test]
    fn a_body_with_nothing_to_redact_is_returned_untouched() {
        let plain = bytes::Bytes::from_static(b"{\"messages\":[]}");
        let (out, l) = replace(Mode::Enforce, &RuleSet::defaults(), plain.clone(), &fresh());
        assert_eq!(out, plain);
        assert!(l.is_empty());
    }

    #[test]
    fn the_event_items_name_the_rule_and_never_carry_the_value() {
        let it = items(&find(Mode::Observe, &RuleSet::defaults(), &body()));
        assert_eq!(it[0].rule, "anthropic-api-key");
        assert_eq!(it[0].kind, tw_api::SecretKind::ApiKeys);
        assert!(!it[0].custom);
        assert!(!it[0].masked.contains("AAAAAAAAAAAA"), "{}", it[0].masked);
    }

    /// 每一跳接着原文那本账换：同一把密钥在每一跳都是同一个号，哪怕那一跳发出去的那份
    /// 把字段换了顺序（转换过格式，或者改写参数时按键名重排过）。以前各起一本账，下面
    /// 这一跳里 `system` 排到了 `messages` 后面，两把密钥的号就对调了
    #[test]
    fn every_hop_numbers_a_value_the_way_the_client_body_did() {
        let other = "ghp_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
        let client =
            format!(r#"{{"system":"{KEY}","messages":[{{"role":"user","content":"{other}"}}]}}"#);
        let (found, l0) = look(Mode::Enforce, &RuleSet::defaults(), client.as_bytes());
        assert_eq!((found.len(), l0.len()), (2, 2));
        let hop =
            format!(r#"{{"messages":[{{"role":"user","content":"{other}"}}],"system":"{KEY}"}}"#);
        let (out, ledger) = replace(Mode::Enforce, &RuleSet::defaults(), hop.clone().into(), &l0);
        assert_eq!(
            String::from_utf8(out.to_vec()).unwrap(),
            r#"{"messages":[{"role":"user","content":"<<TW_SECRET_2>>"}],"system":"<<TW_SECRET_1>>"}"#
        );
        assert_eq!(ledger.len(), 2);
        // 各起一本账的话号就对调了 —— 这条测试防的就是它
        let (alone, _) = replace(Mode::Enforce, &RuleSet::defaults(), hop.into(), &fresh());
        assert!(
            String::from_utf8(alone.to_vec())
                .unwrap()
                .contains(r#""system":"<<TW_SECRET_2>>""#)
        );
    }

    #[test]
    fn look_numbers_only_under_enforce_and_reports_the_same_either_way() {
        let (seen, l) = look(Mode::Observe, &RuleSet::defaults(), &body());
        assert_eq!(seen, find(Mode::Observe, &RuleSet::defaults(), &body()));
        assert!(l.is_empty(), "观察档不该编号");
        let (acted, l) = look(Mode::Enforce, &RuleSet::defaults(), &body());
        assert_eq!(acted, seen);
        assert_eq!(l.len(), 1);
        let (none, l) = look(Mode::Off, &RuleSet::defaults(), &body());
        assert!(none.is_empty() && l.is_empty());
    }

    /// 连接串里的占位符长得像口令，可它不是凭据：不再换一次、不报、原样留着
    #[test]
    fn a_placeholder_where_a_password_would_be_is_not_a_password() {
        let t =
            format!("postgres://app:<<TW_SECRET_2>>@db/x 和 postgres://app:hunter2@db/y 和 {KEY}");
        let found: Vec<String> = hits(&t, &RuleSet::defaults())
            .iter()
            .map(|h| t[h.bytes.clone()].to_string())
            .collect();
        assert_eq!(found, vec!["hunter2".to_string(), KEY.to_string()]);
        let body = format!(r#"{{"content":"{t}"}}"#);
        assert_eq!(
            find(Mode::Observe, &RuleSet::defaults(), body.as_bytes()).len(),
            2
        );
        let (out, _) = replace(
            Mode::Enforce,
            &RuleSet::defaults(),
            body.clone().into(),
            &ledger_for(body.as_bytes()),
        );
        let out = String::from_utf8(out.to_vec()).unwrap();
        assert!(out.contains("postgres://app:<<TW_SECRET_2>>@db/x"), "{out}");
        assert!(out.contains("postgres://app:<<TW_SECRET_3>>@db/y"), "{out}");
    }

    #[test]
    fn a_placeholder_already_in_the_client_body_is_not_handed_out_again() {
        // 用户把请求详情里看到的请求贴回了对话：里面写着 1 号
        let pasted = format!(
            r#"{{"messages":[{{"role":"user","content":"上次发的是 <<TW_SECRET_1>>，这次是 {KEY}"}}]}}"#
        );
        let (_, l0) = look(Mode::Enforce, &RuleSet::defaults(), pasted.as_bytes());
        let (out, ledger) = replace(Mode::Enforce, &RuleSet::defaults(), pasted.into(), &l0);
        let out = String::from_utf8(out.to_vec()).unwrap();
        assert!(out.contains("这次是 <<TW_SECRET_2>>"), "{out}");
        assert_eq!(
            tw_guard::redact::replace::restore("<<TW_SECRET_1>> / <<TW_SECRET_2>>", &ledger),
            format!("<<TW_SECRET_1>> / {KEY}")
        );
        // 重放用的那本新账也让开它
        let stored = format!("<<TW_SECRET_1>> {KEY}");
        let (out, _) = replace(
            Mode::Enforce,
            &RuleSet::defaults(),
            bytes::Bytes::from(stored.clone()),
            &ledger_for(stored.as_bytes()),
        );
        assert_eq!(&out[..], b"<<TW_SECRET_1>> <<TW_SECRET_2>>");
    }
}
