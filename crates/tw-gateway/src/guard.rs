//! 数据面守卫：出站脱敏和入站审查的接线。
//!
//! 规则本身住在 [`tw_redact`] 和 [`tw_scan`] 里，这个文件只回答一个
//! 问题：**这一次请求，到底该按什么规格来。**
//!
//! # 三层配置，谁赢定死了
//!
//! | 层 | 字段 | 作用 |
//! |---|---|---|
//! | 全局 | `security.redact` 三态 | **总闸**。`off` 则下面两层全不生效；`observe` 则检测但不替换 |
//! | provider | `redact: [kinds…]` | **定这个上游脱哪些类别**。主要的配置位置 |
//! | route | `guard.redact` | **只能收紧，不能放松** |
//!
//! 最后一条让横切策略变得安全：**加一条 `guard` 规则永远不会让系统变得
//! 更不安全**，所以人敢往里加规则。反过来的话，一条写少了的规则会悄悄
//! 削掉 provider 上配好的保护，而没有人会发现。

use tw_config::Provider;
use tw_config::SecurityMode as Mode;
use tw_engine::Guard;
use tw_redact::redact::{Ledger, Redacted};
use tw_redact::rules::Kind;

/// 这一次到底脱哪几类。
///
/// **并集，不是覆盖。**provider 上写的是主要来源，route 上的 `guard`
/// 只能往上加。
pub fn effective_kinds(provider: &Provider, guard: &Guard) -> Vec<Kind> {
    let mut kinds = provider.effective_redact();
    for k in &guard.redact {
        if !kinds.contains(k) {
            kinds.push(*k);
        }
    }
    kinds
}

/// 脱敏一次出站请求体。
///
/// 返回换过之后的体，和一本用来还原的账。**总闸不在 `enforce` 时，
/// 账本是空的、体和进来时逐字节相同** —— 观察态那条路已经由
/// `leak::scan` 走过了，这里不重复做。
pub fn redact_outbound(
    mode: Mode,
    provider: &Provider,
    guard: &Guard,
    body: bytes::Bytes,
) -> (bytes::Bytes, Ledger) {
    if !mode.acts() {
        return (body, Ledger::default());
    }
    let kinds = effective_kinds(provider, guard);
    if kinds.is_empty() {
        return (body, Ledger::default());
    }
    // **不是 UTF-8 就不动。**图片之类的二进制体里不会有粘贴进来的 key，
    // 而按字节乱切一个非 UTF-8 的体，得到的是一份坏掉的请求
    let Ok(text) = std::str::from_utf8(&body) else {
        return (body, Ledger::default());
    };
    let Redacted { text, ledger } = tw_redact::redact::redact(text, &kinds);
    if ledger.is_empty() {
        // 没命中就原样返回，连一次拷贝都不做
        return (body, ledger);
    }
    (bytes::Bytes::from(text), ledger)
}

/// 这一次要不要按「不受信任」来对待这个上游。
///
/// route 上的 `guard.untrusted` **只能从「信任」收到「不信任」**，
/// 反过来写不生效。
pub fn effective_trust(provider: &Provider, guard: &Guard) -> tw_config::Trust {
    if guard.untrusted {
        return tw_config::Trust::Untrusted;
    }
    provider.effective_trust()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay() -> Provider {
        Provider {
            name: "中转".into(),
            base_url: "https://relay.example.com".into(),
            ..Default::default()
        }
    }
    fn official() -> Provider {
        Provider {
            name: "官方".into(),
            base_url: "https://api.anthropic.com".into(),
            ..Default::default()
        }
    }
    const KEY: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn body() -> bytes::Bytes {
        bytes::Bytes::from(format!(
            "{{\"messages\":[{{\"content\":\"我的 key 是 {KEY}\"}}]}}"
        ))
    }

    #[test]
    fn the_official_endpoint_gets_the_body_byte_for_byte() {
        // 核心：**你让 Claude Code 调试一个 .env 问题，它得真看见
        // 里面的值才帮得上忙。**
        let (out, l) = redact_outbound(Mode::Enforce, &official(), &Guard::default(), body());
        assert_eq!(out, body());
        assert!(l.is_empty());
    }

    #[test]
    fn a_relay_gets_a_placeholder_instead() {
        let (out, l) = redact_outbound(Mode::Enforce, &relay(), &Guard::default(), body());
        let text = String::from_utf8(out.to_vec()).unwrap();
        assert!(!text.contains(KEY), "{text}");
        assert!(text.contains("<<TW_SECRET_1>>"), "{text}");
        assert_eq!(l.len(), 1);
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
    fn the_master_switch_being_off_or_observing_changes_nothing() {
        // 观察态**只记录，不改变任何行为**。那条路由 leak::scan
        // 走，这里不重复做。
        for m in [Mode::Off, Mode::Observe] {
            let (out, l) = redact_outbound(m, &relay(), &Guard::default(), body());
            assert_eq!(out, body(), "{m:?} 下改了 body");
            assert!(l.is_empty());
        }
    }

    #[test]
    fn a_route_guard_can_only_add_categories() {
        // **加一条 guard 规则永远不会让系统变得更不安全。**
        let g = Guard {
            redact: vec![Kind::Internal],
            untrusted: false,
        };
        let kinds = effective_kinds(&relay(), &g);
        assert!(kinds.contains(&Kind::Internal), "没加上");
        assert!(kinds.contains(&Kind::ApiKeys), "把 provider 上配好的削掉了");

        // 官方那边本来是空的，guard 能给它加上
        let kinds = effective_kinds(&official(), &g);
        assert_eq!(kinds, vec![Kind::Internal]);
    }

    #[test]
    fn a_route_guard_cannot_take_a_category_away() {
        // 空的 guard 不是「什么都不脱」，是「不额外加」。
        let kinds = effective_kinds(&relay(), &Guard::default());
        assert!(kinds.contains(&Kind::ApiKeys));
    }

    #[test]
    fn a_guard_can_downgrade_trust_but_never_upgrade_it() {
        let untrusted = Guard {
            redact: vec![],
            untrusted: true,
        };
        assert_eq!(
            effective_trust(&official(), &untrusted),
            tw_config::Trust::Untrusted,
            "guard 该能把官方也当成不受信任"
        );
        // 反过来没有开关可写 —— Guard 里根本没有「设成 official」这个字段
        assert_eq!(
            effective_trust(&relay(), &Guard::default()),
            tw_config::Trust::Untrusted
        );
    }

    #[test]
    fn a_binary_body_is_left_alone_instead_of_being_mangled() {
        // 按字节乱切一个非 UTF-8 的体，得到的是一份坏掉的请求。
        let raw = bytes::Bytes::from(vec![0xff, 0xfe, 0x00, 0x01]);
        let (out, l) = redact_outbound(Mode::Enforce, &relay(), &Guard::default(), raw.clone());
        assert_eq!(out, raw);
        assert!(l.is_empty());
    }

    #[test]
    fn a_body_with_nothing_to_redact_is_returned_untouched() {
        let plain = bytes::Bytes::from_static(b"{\"messages\":[]}");
        let (out, l) = redact_outbound(Mode::Enforce, &relay(), &Guard::default(), plain.clone());
        assert_eq!(out, plain);
        assert!(l.is_empty());
    }
}
