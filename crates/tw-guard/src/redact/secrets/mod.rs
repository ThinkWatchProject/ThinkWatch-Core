//! 扩充的凭据规则：多前缀、`sk-` 短格式、保存的密钥、按位置认的、配置赋值、加密货币。
//!
//! 和 [`super::rules`] 里原有的判据守同一条规矩：**宁可漏，不可吵。**这里每一条要么前缀
//! 明确、要么结构上核对得了（校验和、固定词表）、要么出现在一个只会放凭据的位置上（请求头、
//! 命令行口令、Webhook 地址、名字写明是口令的赋值）。按位置认的那几条只换值那一段，名字和
//! 地址留着，模型照样读得懂。
//!
//! 分成几块：
//!
//! - 多前缀的规则（[`super::rules::Matcher::Prefixes`]）在 [`super::rules`] 的快速排除和前缀表里
//!   认，前缀写在 `data/redact.yaml`；
//! - [`saved`]：按值认用户在 ThinkWatch 里保存的凭据；
//! - [`shapes`]：前缀不够、要看整段结构的几种（`sk-` 短格式、智谱、千帆、Azure、Terraform、
//!   Telegram）；
//! - [`place`]：按位置认的（请求头、命令行、URL 参数、机器人 Webhook），和 [`shapes`] 里
//!   跨 token 的那几种一样，从切 token 时经过的标记出发；
//! - [`assign`]：配置文件里的赋值；
//! - [`crypto`]：助记词、WIF、扩展私钥。

use std::ops::Range;

use super::rules::{Hit, RuleSet};

mod assign;
mod crypto;
mod place;
pub(crate) mod saved;
mod shapes;

/// 配置赋值认哪些变量名结尾（不分大小写，`_`、`-` 不算）。**界面上的说明和判据用的是同一张表**
pub(super) const ASSIGNMENT_SUFFIXES: &[&str] = &[
    "SECRET",
    "SECRET_KEY",
    "PASSWORD",
    "PASSWD",
    "PWD",
    "TOKEN",
    "API_KEY",
    "APIKEY",
    "ACCESS_KEY",
    "PRIVATE_KEY",
    "CLIENT_SECRET",
];

/// 按位置认的规则的 id。它们的命中**让给别的规则**（见 [`Want::finish`]）
const PLACED: &[&str] = &[
    "auth-header-credential",
    "command-line-password",
    "url-token",
    "config-assignment",
];

/// 这一组规则在一次扫描里的状态：扫之前问一次哪几条开着，扫的过程中攒着跨 token 的东西。
pub(super) struct Want {
    compat: bool,
    zhipu: bool,
    terraform: bool,
    wif: bool,
    xprv: bool,
    assign: bool,
    place: place::Want,
    mnemonic: Option<crypto::Words>,
    /// 按位置认的命中先攒着，扫完再看有没有别的规则认了同一处
    placed: Vec<Hit>,
}

impl Want {
    pub(super) fn new(set: &RuleSet) -> Self {
        Want {
            compat: set.is_on("sk-compat-key"),
            zhipu: set.is_on("zhipu-api-key"),
            terraform: set.is_on("terraform-token"),
            wif: set.is_on("crypto-wif-key"),
            xprv: set.is_on("crypto-extended-key"),
            assign: set.is_on("config-assignment"),
            place: place::Want::new(set),
            mnemonic: set.is_on("crypto-mnemonic").then(crypto::Words::default),
            placed: Vec::new(),
        }
    }

    /// 整段文本过一遍的判据（跨 token 的、按位置找的）
    pub(super) fn text(&mut self, text: &str, _json: bool, out: &mut Vec<Hit>) {
        if self.mnemonic.is_some() {
            crypto::chinese(text, out);
        }
    }

    /// 每个 token 一次。`quoted`：在 JSON 字符串里面（`json` 为真时才有意义）
    #[inline]
    pub(super) fn token(
        &mut self,
        text: &str,
        tok: &str,
        span: Range<usize>,
        _quoted: bool,
        _json: bool,
        out: &mut Vec<Hit>,
    ) {
        if let Some(words) = &mut self.mnemonic {
            words.feed(text, tok, span.clone(), out);
        }
        self.place
            .token(text, tok, span.clone(), out, &mut self.placed);
        let b = tok.as_bytes();
        // 绝大多数 token 在这一行就回去了：下面每一种都至少 20 个字符，或者是个变量名
        if b.len() < 3 {
            return;
        }
        if self.assign && assign::name_ends_right(tok) {
            assign::after_name(text, span.clone(), &mut self.placed);
        }
        if b.len() < 35 {
            return;
        }
        if self.compat && shapes::sk_compat(tok) {
            out.push(super::rules::builtin_hit("sk-compat-key", span));
            return;
        }
        if self.zhipu && shapes::zhipu(tok) {
            out.push(super::rules::builtin_hit("zhipu-api-key", span));
            return;
        }
        if self.terraform && shapes::terraform(tok) {
            out.push(super::rules::builtin_hit("terraform-token", span));
            return;
        }
        if self.wif && crypto::wif(tok) {
            out.push(super::rules::builtin_hit("crypto-wif-key", span));
            return;
        }
        if self.xprv && crypto::extended_key(tok) {
            out.push(super::rules::builtin_hit("crypto-extended-key", span));
        }
    }

    /// 扫完了：攒着没交出去的交出去。
    ///
    /// **按位置认的让给别的规则**：请求头里的一个 JWT、赋值里的一把 `sk-ant-…`，报成「JWT」
    /// 「Anthropic API 密钥」比报成「请求头中的凭据」说得更清楚，危险命令那一层也只认后者。
    /// 机器人 Webhook、Telegram 令牌不在此列：它们本身就是一种凭据
    pub(super) fn finish(&mut self, text: &str, out: &mut Vec<Hit>) {
        if let Some(words) = &mut self.mnemonic {
            words.finish(text, out);
        }
        if self.placed.is_empty() {
            return;
        }
        let mut taken: Vec<Range<usize>> = out
            .iter()
            .filter(|h| !PLACED.contains(&h.rule.id()))
            .map(|h| h.bytes.clone())
            .collect();
        taken.sort_by_key(|r| r.start);
        // reach[i]：前 i + 1 段里最远的终点
        let reach: Vec<usize> = taken
            .iter()
            .scan(0, |far, r| {
                *far = r.end.max(*far);
                Some(*far)
            })
            .collect();
        for h in self.placed.drain(..) {
            let before = taken.partition_point(|r| r.start < h.bytes.end);
            if before == 0 || reach[before - 1] <= h.bytes.start {
                out.push(h);
            }
        }
    }
}

/// 一个字节是不是 token 的一部分（和 [`super::rules`] 切 token 的是同一个判据）
fn tok_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.'
}

/// `at` 前面紧挨着的是不是一个 JSON 转义（`\n`、`\t`、`\uXXXX`）的尾巴。是的话 `at` 处照样
/// 是边界：请求体里行首的值前面是 `\n` 两个字符，那个 `n` 不该和值粘在一起
fn after_escape(b: &[u8], at: usize) -> bool {
    let odd = |end: usize| {
        let mut n = 0;
        while end > n && b[end - 1 - n] == b'\\' {
            n += 1;
        }
        n % 2 == 1
    };
    (at >= 2 && odd(at - 1))
        || (at >= 6
            && b[at - 5] == b'u'
            && b[at - 4..at].iter().all(u8::is_ascii_hexdigit)
            && odd(at - 5))
}

/// `start..end` 两头都在 token 边界上：前一个字节不和它粘成一个 token（或者是转义的尾巴），
/// 后一个字节也不
fn bounded(b: &[u8], start: usize, end: usize) -> bool {
    let head =
        start == 0 || !tok_byte(b[start]) || !tok_byte(b[start - 1]) || after_escape(b, start);
    let tail = end >= b.len() || !tok_byte(b[end - 1]) || !tok_byte(b[end]);
    head && tail
}

/// 一个按位置认到的值像不像真值：够长、不是占位写法、字母和数字都有，或者带着
/// base64、URL 编码、口令里才有的符号。**全是字母的一串是变量名、类型名、说明文字**
/// （`password: string`、`token = accessToken`），不是口令
fn credible(v: &str, min: usize) -> bool {
    let b = v.as_bytes();
    if b.len() < min || super::common::placeholder_value(v) || !v.is_ascii() {
        return false;
    }
    let digit = b.iter().any(u8::is_ascii_digit);
    let alpha = b.iter().any(u8::is_ascii_alphabetic);
    let symbol = b.iter().any(|c| b"!@#^&*+/=~?%".contains(c));
    (digit && alpha) || (symbol && (digit || alpha))
}

#[cfg(test)]
pub(super) mod tests;
