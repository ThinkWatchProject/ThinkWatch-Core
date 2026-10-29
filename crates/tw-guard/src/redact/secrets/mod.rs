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

use super::anchor::Anchor;
use super::rules::{Hit, RuleSet};

pub(super) mod assign;
pub(super) mod crypto;
pub(super) mod place;
pub(crate) mod saved;
mod shapes;

pub(super) use place::ANCHORS as PLACE_ANCHORS;

/// 要看整个 token 形状的几条（`sk-` 短格式、智谱、Terraform、WIF、扩展私钥）的锚点：都至少
/// 35 个字符
pub(super) const SHAPE_ANCHORS: [&[Anchor]; 2] = [shapes::ANCHORS, crypto::KEY_ANCHORS];

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

    /// 中文助记词，自己找汉字（逐条扫描的老写法，对照测试用；两段式扫描的见 `engine`）
    #[cfg(test)]
    pub(super) fn text(&mut self, text: &str, _json: bool, out: &mut Vec<Hit>) {
        if self.mnemonic.is_some() {
            crypto::chinese(text, out);
        }
    }

    /// 每个 token 一次（逐条扫描的老写法，对照测试用）
    #[cfg(test)]
    pub(super) fn token(&mut self, text: &str, tok: &str, span: Range<usize>, out: &mut Vec<Hit>) {
        if let Some(words) = &mut self.mnemonic {
            words.feed(text, tok, span.clone(), out);
        }
        self.at_token(text, tok, span, out);
    }

    /// 一串助记词候选：第一段数出来的、连着 12 个以上像 BIP39 词的 token（见
    /// [`crypto::WORD_ANCHORS`]）从第一个的起点到最后一个的终点。前后都是断开的地方，从头
    /// 切 token、喂进去、喂完收尾 —— 这一段从一个 token 的开头起，切出来的和整段切的一样
    pub(super) fn word_run(&mut self, text: &str, run: Range<usize>, out: &mut Vec<Hit>) {
        if let Some(words) = &mut self.mnemonic {
            let off = run.start;
            super::rules::for_each_token(&text[run], |tok, r, _| {
                words.feed(text, tok, off + r.start..off + r.end, out);
            });
            words.finish(text, out);
        }
    }

    /// 助记词以外的几种：一个 token 自己认得出的，和从 token 出发找值的。**只在对得上
    /// [`PLACE_ANCHORS`]、[`assign::ANCHORS`]、[`SHAPE_ANCHORS`] 之一的 token 上跑**；别的
    /// token 在这里一样什么都不会认出来
    #[inline]
    pub(super) fn at_token(
        &mut self,
        text: &str,
        tok: &str,
        span: Range<usize>,
        out: &mut Vec<Hit>,
    ) {
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

    /// 按位置认的几条开着哪些：请求头、命令行、URL 参数、Webhook、Telegram、Azure、千帆
    pub(super) fn place_on(&self) -> bool {
        self.place.any()
    }

    pub(super) fn assign_on(&self) -> bool {
        self.assign
    }

    /// 要看整个 token 形状的几条开着没有
    pub(super) fn shapes_on(&self) -> bool {
        self.compat || self.zhipu || self.terraform || self.wif || self.xprv
    }

    pub(super) fn mnemonic_on(&self) -> bool {
        self.mnemonic.is_some()
    }

    /// 扫完了：攒着没交出去的交出去。
    ///
    /// **按位置认的让给别的规则**：请求头里的一个 JWT、赋值里的一把 `sk-ant-…`，报成「JWT」
    /// 「Anthropic API 密钥」比报成「请求头中的凭据」说得更清楚，危险命令那一层也只认后者。
    /// 机器人 Webhook、Telegram 令牌不在此列：它们本身就是一种凭据。
    ///
    /// **只让给把它整段盖住、而且要换成占位符的命中。**只盖住一截的（令牌中间像手机号的一段、
    /// 口令里像邮箱的一段）让了的话，换掉的只是那一截，其余照样发出去；只记录的规则（`jwt`
    /// 设成仅记录）根本不换，让给它等于整个值原样发出去。这两种都留着它，由 `resolve` 合成
    /// 一处、整段换掉
    pub(super) fn finish(&mut self, text: &str, set: &RuleSet, out: &mut Vec<Hit>) {
        if let Some(words) = &mut self.mnemonic {
            words.finish(text, out);
        }
        if self.placed.is_empty() {
            return;
        }
        let mut taken: Vec<Range<usize>> = out
            .iter()
            .filter(|h| !PLACED.contains(&h.rule.id()) && !set.records(&h.rule))
            .map(|h| h.bytes.clone())
            .collect();
        taken.sort_by_key(|r| r.start);
        // 并成互不相交的几段：首尾相接的也并上，两处挨着的命中一样把它整段盖住
        let mut cover: Vec<Range<usize>> = Vec::with_capacity(taken.len());
        for r in taken {
            match cover.last_mut() {
                Some(c) if r.start <= c.end => c.end = c.end.max(r.end),
                _ => cover.push(r),
            }
        }
        for h in self.placed.drain(..) {
            let at = cover.partition_point(|r| r.start <= h.bytes.start);
            if at == 0 || cover[at - 1].end < h.bytes.end {
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
