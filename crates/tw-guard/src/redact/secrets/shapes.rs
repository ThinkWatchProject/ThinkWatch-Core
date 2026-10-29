//! 前缀不够说明问题、要看整段结构才认的几种。
//!
//! 前三种是一个 token（[`sk_compat`]、[`zhipu`]、[`terraform`]）；后三种跨着 token 切不开的
//! 字符（`~`、`/`、`:`），由 [`super::place`] 那一遍多模式查找找到落点再交到这里。

use std::ops::Range;

/// `sk-` 开头的 OpenAI 兼容格式：`sk-` 加**恰好** 32 位字母数字、两样都有（DeepSeek、阿里云
/// 百炼的旧式），或者 `sk-` 加恰好 48 个小写字母（硅基流动）。
///
/// 位数要恰好：OpenAI 老式密钥那条要全长 40 以上，这一条接住它够不着的那一截，两条不重叠。
/// 48 个小写字母那种还要求**至少 12 种不同的字母** —— `sk-aaaa…` 这样的占位一种就够了，真
/// 密钥是随机的，48 位里出现的字母远多于 12 种。
pub(super) fn sk_compat(tok: &str) -> bool {
    let Some(t) = tok.strip_prefix("sk-") else {
        return false;
    };
    let b = t.as_bytes();
    match b.len() {
        32 => {
            b.iter().all(u8::is_ascii_alphanumeric)
                && b.iter().any(u8::is_ascii_digit)
                && b.iter().any(u8::is_ascii_alphabetic)
        }
        48 => {
            let mut seen = 0u32;
            for &c in b {
                if !c.is_ascii_lowercase() {
                    return false;
                }
                seen |= 1 << (c - b'a');
            }
            seen.count_ones() >= 12
        }
        _ => false,
    }
}

/// 智谱的 API Key：32 位小写十六进制、一个点、16 位字母数字。
///
/// 一个 MD5 加点加扩展名（`d41d8cd9….tar`）形状接近，所以**点后面那段要有大写字母**：
/// 扩展名、哈希都是小写的，智谱的那段是大小写混排的随机串。
pub(super) fn zhipu(tok: &str) -> bool {
    let b = tok.as_bytes();
    b.len() == 49
        && b[32] == b'.'
        && b[..32]
            .iter()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
        && b[33..].iter().all(u8::is_ascii_alphanumeric)
        && b[33..].iter().any(u8::is_ascii_uppercase)
}

/// Terraform Cloud / HCP 的令牌：14 位字母数字、`.atlasv1.`、60 到 70 位。中间那段标记就是
/// 它的前缀，只是不在开头
pub(super) fn terraform(tok: &str) -> bool {
    const MARK: &[u8] = b".atlasv1.";
    let b = tok.as_bytes();
    (14 + MARK.len() + 60..=14 + MARK.len() + 70).contains(&b.len())
        && &b[14..14 + MARK.len()] == MARK
        && b[..14].iter().all(u8::is_ascii_alphanumeric)
        && b[14 + MARK.len()..]
            .iter()
            .all(|&c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// Azure AD（Entra ID）应用的客户端密码里能有的字符
fn azure_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'~' | b'.' | b'-')
}

/// Azure AD 的客户端密码：3 个字符、1 位数字、`Q~`、31 到 34 个字符，`q` 是 `Q~` 的位置。
///
/// `~` 不是 token 里的字符，所以按 token 认不出它，在整段里找 `Q~`。句末的句号不算进去。
pub(super) fn azure(b: &[u8], q: usize) -> Option<Range<usize>> {
    if q < 4 || !b[q - 1].is_ascii_digit() || !b[q - 4..q - 1].iter().all(|&c| azure_char(c)) {
        return None;
    }
    let start = q - 4;
    if start > 0 && azure_char(b[start - 1]) && !super::after_escape(b, start) {
        return None;
    }
    let from = q + 2;
    let mut end = from;
    while end < b.len() && azure_char(b[end]) {
        end += 1;
    }
    if end - from == 35 && b[end - 1] == b'.' {
        end -= 1;
    }
    (31..=34).contains(&(end - from)).then_some(start..end)
}

/// 百度智能云千帆的 API Key：`bce-v3/ALTAK-`、21 位字母数字、`/`、40 位十六进制，**整段**。
/// `at` 是 `bce-v3/ALTAK-` 的起点
pub(super) fn qianfan(b: &[u8], at: usize) -> Option<Range<usize>> {
    const MARK: &[u8] = b"bce-v3/ALTAK-";
    if !b[at..].starts_with(MARK)
        || (at > 0
            && (super::tok_byte(b[at - 1]) || b[at - 1] == b'/')
            && !super::after_escape(b, at))
    {
        return None;
    }
    let ak = at + MARK.len();
    let slash = ak + 21;
    let end = slash + 1 + 40;
    let ok = b.len() >= end
        && b[ak..slash].iter().all(u8::is_ascii_alphanumeric)
        && b[slash] == b'/'
        && b[slash + 1..end].iter().all(u8::is_ascii_hexdigit)
        && b.get(end).is_none_or(|c| !c.is_ascii_alphanumeric());
    ok.then_some(at..end)
}

/// Telegram 机器人的令牌：8 到 10 位数字、`:AA`、33 个字符（字母数字、`_`、`-`），`colon` 是冒号
/// 的位置。数字前面可以紧挨着字母（`api.telegram.org/bot123…:AA…`），不能是别的数字
pub(super) fn telegram(b: &[u8], colon: usize) -> Option<Range<usize>> {
    let tail_ok = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'-';
    let mut start = colon;
    while start > 0 && b[start - 1].is_ascii_digit() {
        start -= 1;
    }
    if !(8..=10).contains(&(colon - start)) || b.get(colon + 1..colon + 3) != Some(b"AA") {
        return None;
    }
    let end = colon + 3 + 33;
    let ok = b.len() >= end
        && b[colon + 3..end].iter().all(|&c| tail_ok(c))
        && b.get(end).is_none_or(|&c| !tail_ok(c));
    ok.then_some(start..end)
}
