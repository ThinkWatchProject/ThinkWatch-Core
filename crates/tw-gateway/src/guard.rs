//! 数据面守卫：出站脱敏和内容过滤的接线。
//!
//! 出站脱敏怎么找、怎么编号、每一跳怎么换在 [`tw_guard::redact::flow`]，内容过滤怎么查、
//! 怎么删在 [`tw_guard::content`]（两个网关共用）。这里的几个函数是它们在桌面网关里的
//! 入口，外加把结论写成事件。
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
//! 拦截档下 [`look`] 按客户端原文里出现的先后给找到的值编好号，每一跳都接着这本账换
//! （见 [`tw_guard::redact::flow`]）。

use std::collections::HashSet;

use tw_config::SecurityMode as Mode;
use tw_guard::content::Screening;
use tw_guard::redact::flow;
use tw_guard::redact::replace::Ledger;
use tw_guard::redact::rules::{Finding, Hit, Rule, RuleSet};

/// 按规则找一遍，**不算我们自己的占位符，也不进 base64 载荷**（见 [`flow::hits`]）。
pub fn hits(text: &str, rules: &RuleSet) -> Vec<Hit> {
    flow::hits(text, rules)
}

/// 找一遍。**观察档和拦截档都找**，关闭时不找（见 [`flow::find`]）。
pub fn find(mode: Mode, rules: &RuleSet, body: &[u8]) -> Vec<Finding> {
    flow::find(mode, rules, body)
}

/// 一本新账，让开 `body` 里已经写着的占位符（见 [`flow::ledger_for`]）。
pub fn ledger_for(body: &[u8]) -> Ledger {
    flow::ledger_for(body)
}

/// 看一遍客户端发来的原文：报出去的记录，和这个请求的账本（见 [`flow::look`]）。存下来的
/// 那份请求也照这本账换（[`crate::bodies::Redaction`]）。
pub fn look(mode: Mode, rules: &RuleSet, body: &[u8]) -> (Vec<Finding>, Ledger) {
    flow::look(mode, rules, body)
}

/// [`look`]，连同找到的命中（见 [`flow::Look::hits`]）：同一份正文落盘前打码时不再找一遍
/// （[`crate::bodies::BodyRecord::found`]）。
pub fn look_hits(mode: Mode, rules: &RuleSet, body: &[u8]) -> flow::Look {
    flow::look_hits(
        mode,
        rules,
        body,
        Ledger::new(tw_guard::redact::replace::Scheme::SECRET),
    )
}

/// [`look`]，接着 `seed` 的账编号（见 [`flow::look_from`]）。
pub fn look_from(mode: Mode, rules: &RuleSet, body: &[u8], seed: Ledger) -> (Vec<Finding>, Ledger) {
    flow::look_from(mode, rules, body, seed)
}

/// 拦截档下换掉要发出去的这一份，**接着 `ledger` 的账**（见 [`flow::replace`]）。
pub fn replace(
    mode: Mode,
    rules: &RuleSet,
    body: bytes::Bytes,
    ledger: &Ledger,
) -> (bytes::Bytes, Ledger) {
    flow::replace(mode, rules, body, ledger)
}

/// [`replace`]，`found` 是按这套规则在 `body` 上已经找过的命中（[`look_hits`] 在同一份字节上
/// 找的）：照它换，不再找一遍。没找过的是 `None`，和 [`replace`] 一样。
pub fn replace_found(
    mode: Mode,
    rules: &RuleSet,
    body: bytes::Bytes,
    ledger: &Ledger,
    found: Option<&[Hit]>,
) -> (bytes::Bytes, Ledger) {
    let Some(hits) = found.filter(|_| mode.acts() && !rules.is_empty()) else {
        return replace(mode, rules, body, ledger);
    };
    // 没命中就原样返回，连一次拷贝都不做
    if hits.is_empty() {
        return (body, ledger.clone());
    }
    // 找得到命中的一定是 UTF-8（见 `flow::look_hits`）
    let Ok(text) = std::str::from_utf8(&body) else {
        return replace(mode, rules, body, ledger);
    };
    let r = tw_guard::redact::replace::apply(text, hits, ledger.clone());
    (bytes::Bytes::from(r.text), r.ledger)
}

/// `after` 里 `before` 没有的那些值：插件写进请求里的（见 [`crate::plugin::request`]）。
///
/// 按规则和打过码的样子比：同一个值在两份里打出来的码一样。客户端原话里就有的值，开头
/// 那一遍已经报过了，插件改过的那一份里再出现不再报一次。**查表比**，不两两比：两份里
/// 各有几万个值时，后者是平方级的。
pub fn more_found(before: &[Finding], after: Vec<Finding>) -> Vec<Finding> {
    let known: HashSet<(&Rule, &str)> = before
        .iter()
        .map(|b| (&b.rule, b.masked.as_str()))
        .collect();
    after
        .into_iter()
        .filter(|f| !known.contains(&(&f.rule, f.masked.as_str())))
        .collect()
}

/// 一个请求报出去的值最多几个。
///
/// 安全日志一个值一行（[`tw_api::Event::SecretsFound`] 的一项就是一行）。一个请求里贴进来
/// 几万个不同的密钥时（一份导出的凭据清单、一段日志），逐个报就是几万行：日志被一个请求
/// 刷满，真正要看的那几条反而淹没了，一条事件也有几 MB。前 100 个（按在请求里第一次出现
/// 的先后）足够说明这个请求带了什么。
///
/// **只少报，不少换**：替换按的是账本（[`look`]），和报了几个无关 —— 超出的值照样换成
/// 占位符、回答里照样换回来。
pub const REPORTED_MAX: usize = 100;

/// 找到的东西写成事件里的样子。`already` 是这个请求先前已经找到过几个（插件改写过的请求
/// 只报插件写进来的那些，见 [`more_found`]）：**一个请求加起来最多报 [`REPORTED_MAX`] 个**，
/// 先报先出现的。
pub fn items(found: &[Finding], already: usize) -> Vec<tw_api::SecretItem> {
    found
        .iter()
        .take(REPORTED_MAX.saturating_sub(already))
        .map(|f| tw_api::SecretItem {
            rule: f.rule.id().to_string(),
            custom: f.rule.custom(),
            kind: crate::wire::secret_kind(f.rule.kind()),
            masked: f.masked.clone(),
            count: f.count,
        })
        .collect()
}

/// 内容过滤此刻的档位和规则。
///
/// **HTTP 和 WebSocket 两条路共用**（见 [`screen`]）。升级那一刻取一次，一条连接
/// 活多久就按它开始时的配置走多久。
#[derive(Clone)]
pub struct Screen {
    pub mode: Mode,
    pub rules: std::sync::Arc<tw_guard::content::Rules>,
}

impl Screen {
    pub fn of(rt: &crate::state::Runtime) -> Self {
        Self {
            mode: rt.config.security.content.mode,
            rules: rt.content.clone(),
        }
    }
}

/// 查一个请求：`body` 是要发出去的那一份原文，`dialect` 是它的格式（见
/// [`tw_guard::content::screen`]）。处置档下删过的话，删过的请求体在
/// [`Screening::body`] 里。
///
/// **只下结论，不发事件**：开始事件和留档要用删过的那一份，记录要挂在请求号上，所以
/// 先查、再开始、再报（[`report`]）。可以重复调用：请求被改过之后（插件改写）再查一遍，
/// 用的也是它。
pub fn screen(s: &Screen, dialect: tw_dialect::ir::Dialect, body: &[u8]) -> Screening {
    tw_guard::content::screen(s.mode, &s.rules, dialect, body)
}

/// [`screen`]，请求体已经解析好了：`v` 是它解出来的样子（见
/// [`tw_guard::content::screen_value`]）。
pub fn screen_value(
    s: &Screen,
    dialect: tw_dialect::ir::Dialect,
    v: &serde_json::Value,
) -> Screening {
    tw_guard::content::screen_value(s.mode, &s.rules, dialect, v)
}

/// 把一次查下来的结论报出去：每条命中的规则一条 [`tw_api::Event::ContentMatched`]，挂在
/// 请求 `id` 上。要拒绝时返回告诉客户端的那句话。
pub fn report(
    bus: &tw_observe::EventBus,
    id: u64,
    provider: &str,
    sc: &Screening,
) -> Option<tw_types::Msg> {
    if sc.hits.is_empty() {
        return None;
    }
    let at_ms = crate::server::now_ms();
    for h in &sc.hits {
        let hit = &h.hit;
        bus.emit(tw_api::Event::ContentMatched {
            id,
            provider: provider.to_string(),
            rule: hit.rule.clone(),
            custom: hit.custom,
            matching: tw_api::ContentMatch::of(hit.matching),
            action: tw_guard::policy::ContentAction::of(hit.action).into(),
            outcome: h.outcome,
            in_tool_result: hit.in_tool_result,
            excerpt: hit.snippet.clone(),
            count: hit.count as u64,
            revealed: (!hit.revealed.is_empty()).then(|| hit.revealed.clone()),
            at_ms,
        });
    }
    // 命中的原文是调用方的正文，**不进应用日志**：日志只说哪条规则、做了什么
    tracing::info!(
        provider,
        refused = sc.refused.is_some(),
        stripped = sc.body.is_some(),
        rules = ?sc.hits.iter().map(|h| h.hit.rule.as_str()).collect::<Vec<_>>(),
        "the request matched content rules"
    );
    sc.refusal().map(|r| refusal(&r.hit))
}

/// 插件改过的请求再查一遍：**只报插件加进来的，也只按插件加进来的拒绝**。`before` 是插件
/// 拿到的那一份，`after` 是插件改过的那一份，都是 `dialect` 格式的原文。
///
/// 插件拿到的那一份在开头已经查过、报过了（[`screen`]、[`report`]）；改过的那一份整个再报
/// 一遍的话，同一条命中会在安全日志里出现两次。所以两份都查，原来就在的命中减掉（见
/// [`added`]）。
///
/// 处置档下，插件拿到的那一份一般不会命中拒绝规则 —— 命中了的请求在开头就拒了，走不到
/// 插件这一步。嵌入、旧版补全例外：开头不查，客户端的原话里可以有。**拒绝的只是原来就在
/// 的，不因为插件改了别处就拒**：那几条拒绝规则这一遍不算，再查一遍，插件加进来的字照样
/// 删、照样报、照样能拒。
pub fn rescreen(
    s: &Screen,
    dialect: tw_dialect::ir::Dialect,
    before: &[u8],
    after: &[u8],
) -> Screening {
    use tw_guard::content::Outcome;
    let mut s = s.clone();
    loop {
        let was = screen(&s, dialect, before);
        let now = screen(&s, dialect, after);
        // 拒绝时，拒绝规则的每一条命中都是「已拒绝」：有一条是插件加进来的就拒
        let blocking: Vec<&tw_guard::content::Hit> = now
            .hits
            .iter()
            .filter(|h| h.outcome == Outcome::Blocked)
            .map(|h| &h.hit)
            .collect();
        if now.refused.is_none() || blocking.iter().any(|h| !known(&was, h)) {
            return added(&was, now);
        }
        // 拒绝它的全是原来就在的：这几条不算，再查一遍。每一轮至少少一条规则
        let quiet: Vec<(String, bool)> = blocking
            .iter()
            .map(|h| (h.rule.clone(), h.custom))
            .collect();
        let rules = s
            .rules
            .rules
            .iter()
            .filter(|r| {
                !quiet
                    .iter()
                    .any(|(id, custom)| r.id == *id && r.custom == *custom)
            })
            .cloned()
            .collect();
        s.rules = std::sync::Arc::new(tw_guard::content::Rules { rules });
    }
}

/// 插件改过的那一份查下来的结论里，**只留插件加进来的**：`before` 里就有的命中减掉 ——
/// 同一条规则、处数没有变多的，算原来就在的。插件让一条规则多命中了几处，**整条再报**
/// （处数是改过之后的）。拒绝时说了算的是头一条插件加进来的「已拒绝」；删过的请求体
/// （[`Screening::body`]）照 `after`，这一跳发出去的就是它。
///
/// 拒绝它的全是原来就在的那种情况由 [`rescreen`] 先排除掉。
pub fn added(before: &Screening, after: Screening) -> Screening {
    let mut hits = Vec::with_capacity(after.hits.len());
    let mut refused = None;
    for h in after.hits {
        if known(before, &h.hit) {
            continue;
        }
        if refused.is_none() && h.outcome == tw_guard::content::Outcome::Blocked {
            refused = Some(hits.len());
        }
        hits.push(h);
    }
    Screening {
        hits,
        refused,
        body: after.body,
    }
}

/// 这一条命中在 `before` 里就有：同一条规则，处数没有变多
fn known(before: &Screening, h: &tw_guard::content::Hit) -> bool {
    before
        .hits
        .iter()
        .any(|b| b.hit.rule == h.rule && b.hit.custom == h.custom && h.count <= b.hit.count)
}

/// 拒绝时告诉客户端的那句话。码位规则命中的是看不见的字符，引一段片段没有用，说几个；
/// 在工具结果里和在调用方自己打的字里是两句话：前者要去查是哪个工具抓回来的
fn refusal(h: &tw_guard::content::Hit) -> tw_types::Msg {
    if h.matching != tw_guard::content::Match::Codepoints {
        return tw_types::msg!(
            "gw.content.refused",
            rule = h.rule.clone(), name = h.name.clone(), excerpt = h.snippet.clone() =>
            "Content rule “{name}” matched this request (“{excerpt}”), so it was not sent."
        );
    }
    if h.in_tool_result {
        tw_types::msg!(
            "gw.content.refused_invisible_tool_result",
            rule = h.rule.clone(), name = h.name.clone(), count = h.count =>
            "A tool result in this request contains {count} invisible characters that content \
             rule “{name}” refuses, so the request was not sent."
        )
    } else {
        tw_types::msg!(
            "gw.content.refused_invisible_message",
            rule = h.rule.clone(), name = h.name.clone(), count = h.count =>
            "The message contains {count} invisible characters that content rule “{name}” \
             refuses, so the request was not sent."
        )
    }
}

/// 没法按消息结构读的正文（解不开的 WebSocket 帧）：只用码位规则，查完就报（见
/// [`screen_raw`]）。拒绝时返回告诉客户端的那句话。
pub fn screen_text(
    bus: &tw_observe::EventBus,
    id: u64,
    provider: &str,
    s: &Screen,
    text: &str,
) -> Option<tw_types::Msg> {
    report(bus, id, provider, &screen_raw(s, text))
}

/// [`screen_text`] 的结论本身，不发事件：只用码位规则查整段原文，认得 JSON 的 `\uXXXX`
/// 写法（见 [`tw_guard::content::screen_text`]）。删过之后的文字在 [`Screening::body`] 里。
///
/// 关键词和正则按整段原文查的话，系统提示里的话也会被当成调用方的；看不见的字符在任何
/// 地方都没有正当用途，整段查没有误伤谁。
pub fn screen_raw(s: &Screen, text: &str) -> Screening {
    tw_guard::content::screen_text(s.mode, &s.rules, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tw_guard::redact::replace::Scheme;

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
        let it = items(&find(Mode::Observe, &RuleSet::defaults(), &body()), 0);
        assert_eq!(it[0].rule, "anthropic-api-key");
        assert_eq!(it[0].kind, tw_api::SecretKind::ApiKeys);
        assert!(!it[0].custom);
        assert!(!it[0].masked.contains("AAAAAAAAAAAA"), "{}", it[0].masked);
    }

    /// 第 `i` 个 AWS 访问密钥的样子，两两不同，打出来的码（头 5 尾 4）也两两不同
    fn aws_key(i: usize) -> String {
        let mut n = i;
        let d: Vec<char> = (0..5)
            .map(|_| {
                let c = char::from(b'A' + (n % 26) as u8);
                n /= 26;
                c
            })
            .collect();
        format!("AKIA{}QQQQQQQQQQQ{}{}{}{}", d[0], d[1], d[2], d[3], d[4])
    }

    #[test]
    fn a_request_reports_its_first_hundred_values_and_still_replaces_every_one() {
        let keys: Vec<String> = (0..150).map(aws_key).collect();
        let body = serde_json::json!({"messages": [{"content": keys.join(" ")}]}).to_string();
        let (found, ledger) = look(Mode::Enforce, &RuleSet::defaults(), body.as_bytes());
        // 找到的一个不少：总共几个不同的值，`found.len()` 说得出来
        assert_eq!(found.len(), 150);
        let it = items(&found, 0);
        assert_eq!(it.len(), REPORTED_MAX);
        // 报的是先出现的那些，按出现的先后
        let masked: Vec<String> = keys[..REPORTED_MAX]
            .iter()
            .map(|k| tw_guard::redact::rules::masked(&found[0].rule, k))
            .collect();
        assert_eq!(
            it.iter().map(|i| i.masked.clone()).collect::<Vec<_>>(),
            masked
        );
        // 插件改写过的请求再报一次（只报插件写进来的）：和开头那一条加起来不超过上限
        assert_eq!(items(&found, 30).len(), REPORTED_MAX - 30);
        assert!(items(&found, 150).is_empty());
        // 没报的照样换掉
        let (out, ledger) = replace(Mode::Enforce, &RuleSet::defaults(), body.into(), &ledger);
        let out = String::from_utf8(out.to_vec()).unwrap();
        assert!(!out.contains("AKIA"), "{out}");
        assert!(out.contains("<<TW_SECRET_150>>"), "{out}");
        assert_eq!(ledger.len(), 150);
    }

    /// 照开头那一遍找到的命中换，和自己再找一遍换的一字不差、账本一样
    #[test]
    fn replacing_with_the_hits_already_found_is_replacing() {
        let other = "ghp_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
        let body = bytes::Bytes::from(format!(
            r#"{{"system":"<<TW_SECRET_1>> {KEY}","messages":[{{"content":"{other} {KEY}"}}]}}"#
        ));
        let rules = RuleSet::defaults();
        let seen = look_hits(Mode::Enforce, &rules, &body);
        let hits = seen.hits.unwrap();
        let again = replace(Mode::Enforce, &rules, body.clone(), &seen.ledger);
        let known = replace_found(
            Mode::Enforce,
            &rules,
            body.clone(),
            &seen.ledger,
            Some(&hits),
        );
        assert_eq!(known.0, again.0);
        assert_eq!(known.1.table(), again.1.table());
        assert!(!String::from_utf8_lossy(&known.0).contains(KEY));
        // 观察档不换，找过也不换
        let (out, _) = replace_found(Mode::Observe, &rules, body.clone(), &fresh(), Some(&hits));
        assert_eq!(out, body);
        // 什么都没找到：原样，不拷贝
        let plain = bytes::Bytes::from_static(b"{\"messages\":[]}");
        let (out, l) = replace_found(Mode::Enforce, &rules, plain.clone(), &fresh(), Some(&[]));
        assert_eq!(out.as_ptr(), plain.as_ptr());
        assert!(l.is_empty());
    }

    /// 查表比和两两比，留下的一模一样：同一条规则下打码一样的算见过，换一条规则就不算
    #[test]
    fn more_found_keeps_exactly_what_comparing_every_pair_kept() {
        let text = |range: std::ops::Range<usize>| {
            let keys: Vec<String> = range.map(aws_key).collect();
            serde_json::json!({ "content": keys.join(" ") }).to_string()
        };
        let mut before = find(Mode::Observe, &RuleSet::defaults(), text(0..60).as_bytes());
        // 前 10 个当成是另一条规则认出来的：打码一样、规则不同，不算见过
        for f in &mut before[..10] {
            f.rule = Rule::Custom(std::sync::Arc::from("aws-again"));
        }
        let after_text = format!("{} {}", text(30..90), text(0..5));
        let after = find(Mode::Observe, &RuleSet::defaults(), after_text.as_bytes());
        let pairwise: Vec<Finding> = after
            .iter()
            .filter(|f| {
                !before
                    .iter()
                    .any(|b| b.rule == f.rule && b.masked == f.masked)
            })
            .cloned()
            .collect();
        let got = more_found(&before, after);
        assert_eq!(got, pairwise);
        assert_eq!(got.len(), 35, "60..90 是新的，0..5 换了规则");
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

    /// 内容过滤的几条规则：拒绝一个词、只记录一个词、删掉零宽字符
    fn filter(mode: Mode) -> Screen {
        use tw_guard::content::{Action, Match, RuleInput, Rules};
        let rule = |id, pattern, matching, action| RuleInput {
            id,
            name: id,
            custom: true,
            pattern,
            matching,
            action,
        };
        Screen {
            mode,
            rules: std::sync::Arc::new(
                Rules::build([
                    rule("no plan", "forbidden-plan", Match::Contains, Action::Block),
                    rule("falcon", "falcon", Match::Contains, Action::Record),
                    rule("zero width", "U+200B", Match::Codepoints, Action::Strip),
                ])
                .unwrap(),
            ),
        }
    }

    fn chat(text: &str) -> Vec<u8> {
        serde_json::json!({ "messages": [{ "role": "user", "content": text }] })
            .to_string()
            .into_bytes()
    }

    fn rules_hit(sc: &Screening) -> Vec<(&str, tw_guard::content::Outcome)> {
        sc.hits
            .iter()
            .map(|h| (h.hit.rule.as_str(), h.outcome))
            .collect()
    }

    /// 插件拿到的那一份里本来就有的命中（嵌入、补全开头不查，原话里可以有拒绝规则的词）
    /// 不再报、不拒；插件加进来的零宽字符照样删、照样报
    #[test]
    fn a_rewritten_request_is_judged_on_what_the_plugin_added() {
        use tw_guard::content::Outcome;
        let s = filter(Mode::Enforce);
        let before = chat("the forbidden-plan, and falcon");
        let after = chat("the forbidden-plan, and falcon, plus zero\u{200B}width");
        let sc = rescreen(&s, tw_dialect::ir::Dialect::Chat, &before, &after);
        assert_eq!(sc.refused, None);
        assert_eq!(rules_hit(&sc), [("zero width", Outcome::Stripped)]);
        let sent: serde_json::Value = serde_json::from_slice(sc.body.as_deref().unwrap()).unwrap();
        assert_eq!(
            sent["messages"][0]["content"],
            "the forbidden-plan, and falcon, plus zerowidth"
        );

        // 插件又写了一处拒绝规则的词：拒，说了算的是那一条
        let after = chat("the forbidden-plan, and falcon; also forbidden-plan");
        let sc = rescreen(&s, tw_dialect::ir::Dialect::Chat, &before, &after);
        assert_eq!(rules_hit(&sc), [("no plan", Outcome::Blocked)]);
        assert_eq!(sc.refusal().map(|h| h.hit.count), Some(2));
        assert!(sc.body.is_none());

        // 什么都没加：什么都不报
        let sc = rescreen(&s, tw_dialect::ir::Dialect::Chat, &before, &before);
        assert!(sc.hits.is_empty() && sc.refused.is_none() && sc.body.is_none());
    }

    /// 观察档：插件让一条规则多命中了几处，整条再报（处数是改过之后的），请求原样
    #[test]
    fn under_observe_only_more_of_a_rule_is_reported_again() {
        use tw_guard::content::Outcome;
        let s = filter(Mode::Observe);
        let before = chat("falcon");
        let sc = rescreen(
            &s,
            tw_dialect::ir::Dialect::Chat,
            &before,
            &chat("falcon falcon forbidden-plan"),
        );
        assert_eq!(
            rules_hit(&sc),
            [
                ("no plan", Outcome::Recorded),
                ("falcon", Outcome::Recorded)
            ]
        );
        assert_eq!(sc.hits[1].hit.count, 2);
        assert!(sc.refused.is_none() && sc.body.is_none());
    }
}
