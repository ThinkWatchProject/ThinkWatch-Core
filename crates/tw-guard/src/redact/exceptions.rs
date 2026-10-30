//! 例外值：某个值反复被误认时，只把这个值列为例外，规则本身照常工作。
//!
//! **例外的是「这个值」，不是「这条规则下的这个值」**：列入之后，它在哪条规则下都不换、
//! 不记。存的是值的 SHA-256（小写十六进制），不存原值 —— 例外表随配置导出、同步、备份，
//! 原值写进去就是把要防的东西抄了一份带走。
//!
//! 值按**解开转义之后的样子**算哈希（和账本记的原值是同一个样子，见
//! [`crate::redact::replace::Ledger`]）：请求体里写着 `\n` 的、插件那一层在正文里看到的、
//! 用户粘贴进来的，是同一个值、同一个哈希。
//!
//! **保存的凭据不能列为例外**（规则 `thinkwatch-saved-secret`）：那是用户自己存进 ThinkWatch
//! 的密钥，本就该换。它的命中不查例外表；配置里写着这条规则的例外读得进来、但不生效
//! （[`crate::policy::RedactPolicy::exception_set`]）。

use std::collections::HashSet;

use sha2::{Digest, Sha256};

use super::rules::Rule;

/// 例外最多几个。界面上满了「添加例外」置灰；配置里写多了是配置错误
pub const EXCEPTIONS_MAX: usize = 500;

/// 保存的凭据那条规则的 id：它的命中不能列为例外
pub const SAVED_RULE: &str = "thinkwatch-saved-secret";

/// 一个值的 SHA-256，32 个字节
pub type Digest32 = [u8; 32];

/// 一个值（解开转义之后的样子）的 SHA-256，小写十六进制
pub fn sha256_hex(value: &str) -> String {
    hex(&Sha256::digest(value.as_bytes()).into())
}

/// 32 个字节写成 64 个小写十六进制字符
pub fn hex(d: &Digest32) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(64);
    for b in d {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 15) as usize] as char);
    }
    s
}

/// 反过来。**只认 64 个小写十六进制字符**：大写的、短一截的都不是我们写出去的样子，
/// 认下来的话两种写法的同一个哈希在去重时就成了两个
pub fn parse_hex(s: &str) -> Option<Digest32> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = (nib(b[2 * i])? << 4) | nib(b[2 * i + 1])?;
    }
    Some(out)
}

/// 这条规则的命中能不能列为例外：保存的凭据不能
pub fn exceptable(rule: &Rule) -> bool {
    !matches!(rule, Rule::Builtin(id) if *id == SAVED_RULE)
}

/// 编好的例外表。**跟着配置一起换**：和规则集一起建一次（[`crate::redact::rules::RuleSet::with_exceptions`]）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exceptions {
    set: HashSet<Digest32>,
    /// 排好序的全部哈希再算一次 SHA-256：例外表变了它就变（见 [`Self::fingerprint`]）
    fingerprint: Digest32,
}

impl Exceptions {
    /// 一个都没有是 `None`：调用方据此整个跳过，**例外表为空时一个哈希都不算**
    pub fn new(hashes: impl IntoIterator<Item = Digest32>) -> Option<Self> {
        let set: HashSet<Digest32> = hashes.into_iter().collect();
        if set.is_empty() {
            return None;
        }
        let mut sorted: Vec<&Digest32> = set.iter().collect();
        sorted.sort_unstable();
        let mut h = Sha256::new();
        for d in sorted {
            h.update(d);
        }
        Some(Self {
            set,
            fingerprint: h.finalize().into(),
        })
    }

    pub fn len(&self) -> usize {
        self.set.len()
    }

    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    /// 例外表的指纹：全部哈希排好序之后的 SHA-256。**同一组例外，不论写的先后，指纹一样**。
    /// 按「规则集版本」缓存扫描结果的地方把它算进版本里：改了例外表，缓存整体作废
    pub fn fingerprint(&self) -> Digest32 {
        self.fingerprint
    }

    /// 这个哈希在不在表里
    pub fn contains(&self, d: &Digest32) -> bool {
        self.set.contains(d)
    }

    /// 十六进制写的哈希在不在表里。写得不对的不在
    pub fn contains_hex(&self, s: &str) -> bool {
        parse_hex(s).is_some_and(|d| self.contains(&d))
    }

    /// 这个值（解开转义之后的样子）是不是例外
    pub fn excepts(&self, value: &str) -> bool {
        self.contains(&Sha256::digest(value.as_bytes()).into())
    }

    /// 这个值是不是例外，`raw` 是从 JSON 原文里截下来的一段：先解开转义（和账本一样，见
    /// [`crate::redact::replace::Ledger`]）
    pub(crate) fn excepts_json(&self, raw: &str) -> bool {
        if raw.contains('\\') {
            self.excepts(&crate::locate::unescape(raw))
        } else {
            self.excepts(raw)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hash_is_lowercase_hex_and_reads_back() {
        let h = sha256_hex("test");
        assert_eq!(
            h,
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
        assert_eq!(parse_hex(&h).map(|d| hex(&d)), Some(h.clone()));
        assert_eq!(
            parse_hex(&h.to_uppercase()),
            None,
            "大写的不是我们写出去的样子"
        );
        assert_eq!(parse_hex(&h[..63]), None);
        assert_eq!(parse_hex(""), None);
    }

    #[test]
    fn the_fingerprint_follows_the_set_not_the_order() {
        let a = parse_hex(&sha256_hex("a")).unwrap();
        let b = parse_hex(&sha256_hex("b")).unwrap();
        let ab = Exceptions::new([a, b]).unwrap();
        let ba = Exceptions::new([b, a]).unwrap();
        assert_eq!(ab.fingerprint(), ba.fingerprint());
        assert_ne!(
            ab.fingerprint(),
            Exceptions::new([a]).unwrap().fingerprint()
        );
        assert!(Exceptions::new([]).is_none(), "空表就是没有");
    }

    #[test]
    fn a_value_from_json_is_matched_unescaped() {
        let ex = Exceptions::new([parse_hex(&sha256_hex("a\nb")).unwrap()]).unwrap();
        assert!(ex.excepts("a\nb"));
        assert!(ex.excepts_json("a\\nb"));
        assert!(!ex.excepts("a\\nb"), "正文里的反斜杠不当成转义");
    }
}
