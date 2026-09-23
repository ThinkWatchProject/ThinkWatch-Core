//! 数据面守卫：出站脱敏的接线。
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
use tw_guard::redact::replace::Ledger;
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
        return (body, Ledger::default());
    }
    // 按字节乱切一个非 UTF-8 的体，得到的是一份坏掉的请求
    let Ok(text) = std::str::from_utf8(&body) else {
        return (body, Ledger::default());
    };
    let hits = tw_guard::redact::rules::scan(text, rules);
    if hits.is_empty() {
        // 没命中就原样返回，连一次拷贝都不做
        return (body, Ledger::default());
    }
    let r = tw_guard::redact::replace::apply(text, &hits);
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
