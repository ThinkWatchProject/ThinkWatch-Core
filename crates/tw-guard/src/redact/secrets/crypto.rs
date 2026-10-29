//! 加密货币的助记词和私钥。
//!
//! 三种都能在结构上核对，危害等同于私钥：
//!
//! - **助记词**（BIP39）：12 / 15 / 18 / 21 / 24 个词，全在英文或简体中文词表里，最后几位是
//!   前面那些位的 SHA-256 的头几位。随便凑 12 个词碰上校验和的机会是 1/16，24 个是 1/256；
//!   而连着 12 个词全在一张 2048 个词的表里，平常的文字本来就很少见。
//! - **WIF 私钥**、**扩展私钥**（`xprv` 一类）：Base58Check，末四个字节是前面的两次 SHA-256，
//!   碰巧对上的机会是四十亿分之一。
//!
//! 64 位十六进制的以太坊私钥和交易哈希、SHA-256 摘要分不开，不收。
//!
//! 词表取自 BIP39 的官方仓库（bitcoin/bips 的 `bip-0039/`），原样放在 `bip39/` 下，每张
//! 2048 行；测试钉着行数、排序和几组官方测试向量。

use std::ops::Range;
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

use super::super::rules::{Hit, builtin_hit};

const ENGLISH: &str = include_str!("bip39/english.txt");
pub(super) const CHINESE: &str = include_str!("bip39/chinese_simplified.txt");

// ---------------------------------------------------------------- 词表

/// 英文词表：词（按字母排好，官方文件本来就是排好的，测试钉着），和「头三个字母 → 以它们
/// 开头的那一段（起点、个数）」。**像单词的 token 都要查一次**，二分查找在几 MB 的请求体上
/// 就是几毫秒；按头三个字母直接落到一两个候选上，大多数一个候选都没有
struct Lexicon {
    words: Vec<&'static str>,
    index: Vec<(u16, u8)>,
}

fn lexicon() -> &'static Lexicon {
    static L: OnceLock<Lexicon> = OnceLock::new();
    L.get_or_init(|| {
        let words: Vec<&'static str> = ENGLISH.lines().collect();
        let mut index = vec![(0u16, 0u8); 26 * 26 * 26];
        for (i, w) in words.iter().enumerate() {
            let k = key3(w.as_bytes());
            if index[k].1 == 0 {
                index[k].0 = i as u16;
            }
            index[k].1 += 1;
        }
        Lexicon { words, index }
    })
}

/// 英文词表
#[cfg(test)]
pub(super) fn english() -> &'static [&'static str] {
    &lexicon().words
}

/// 词表里出现过的「头三个字母」，按位存（编译时从词表算出来）：查表之前先问一次，平常的
/// 英文单词多数在这里就回去了
const HEADS: [u64; 26 * 26 * 26 / 64 + 1] = {
    let mut m = [0u64; 26 * 26 * 26 / 64 + 1];
    let b = ENGLISH.as_bytes();
    let mut i = 0;
    while i + 3 <= b.len() {
        let k = (b[i] - b'a') as usize * 676
            + (b[i + 1] - b'a') as usize * 26
            + (b[i + 2] - b'a') as usize;
        m[k / 64] |= 1 << (k % 64);
        // 跳到下一行
        while i < b.len() && b[i] != b'\n' {
            i += 1;
        }
        i += 1;
    }
    m
};

fn key3(w: &[u8]) -> usize {
    let c = |i: usize| usize::from(w[i] - b'a');
    c(0) * 676 + c(1) * 26 + c(2)
}

/// 一个英文词在词表里的序号。只认小写：钱包给出来的就是小写
fn english_word(w: &[u8]) -> Option<u16> {
    if !(3..=8).contains(&w.len()) || !w.iter().all(u8::is_ascii_lowercase) {
        return None;
    }
    let k = key3(w);
    if HEADS[k / 64] & (1 << (k % 64)) == 0 {
        return None;
    }
    let l = lexicon();
    let (start, n) = l.index[key3(w)];
    (usize::from(start)..usize::from(start) + usize::from(n))
        .find(|&i| l.words[i].as_bytes() == w)
        .map(|i| i as u16)
}

/// 中文词表里的字都在 CJK 统一表意文字的基本区（U+4E00–U+9FFF，测试钉着）：按码位直接查
const CJK_FROM: u32 = 0x4E00;
const CJK_TO: u32 = 0x9FFF;
const NONE: u16 = u16::MAX;

fn chinese_index() -> &'static [u16] {
    static I: OnceLock<Vec<u16>> = OnceLock::new();
    I.get_or_init(|| {
        let mut t = vec![NONE; (CJK_TO - CJK_FROM + 1) as usize];
        for (i, w) in CHINESE.lines().enumerate() {
            if let Some(c) = w.chars().next() {
                t[(c as u32 - CJK_FROM) as usize] = i as u16;
            }
        }
        t
    })
}

fn chinese_word(cp: u32) -> Option<u16> {
    if !(CJK_FROM..=CJK_TO).contains(&cp) {
        return None;
    }
    let i = chinese_index()[(cp - CJK_FROM) as usize];
    (i != NONE).then_some(i)
}

// ---------------------------------------------------------------- 校验

/// 这串词的序号是不是一组合规的助记词：个数对、校验和对得上，而且不是测试向量那种一眼假的
/// 样子（同一个词占了三分之一以上，像 `abandon … about`、Hardhat 的 `test … junk`；或者熵的
/// 每个字节都一样，像官方测试向量的 `0x7f7f…`、`0x8080…`）
pub(super) fn mnemonic(idx: &[u16]) -> bool {
    let n = idx.len();
    if !matches!(n, 12 | 15 | 18 | 21 | 24) {
        return false;
    }
    let mut counts = std::collections::HashMap::new();
    for &i in idx {
        *counts.entry(i).or_insert(0usize) += 1;
    }
    if counts.values().any(|&c| c * 3 > n) {
        return false;
    }
    let bits = n * 11;
    let cs = bits / 33;
    let ent = bits - cs;
    let mut buf = [0u8; 33];
    for (k, &i) in idx.iter().enumerate() {
        for j in 0..11 {
            if i & (1 << (10 - j)) != 0 {
                let at = k * 11 + j;
                buf[at / 8] |= 0x80 >> (at % 8);
            }
        }
    }
    let entropy = &buf[..ent / 8];
    if entropy.iter().all(|&x| x == entropy[0]) {
        return false;
    }
    let want = buf[ent / 8] >> (8 - cs);
    let hash = Sha256::digest(entropy);
    hash[0] >> (8 - cs) == want
}

const BASE58: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

/// Base58 解码（比特币的字母表）。前导的 `1` 是前导的零字节
fn base58(s: &[u8]) -> Option<Vec<u8>> {
    static MAP: OnceLock<[u8; 128]> = OnceLock::new();
    let map = MAP.get_or_init(|| {
        let mut m = [0xFF; 128];
        for (i, &c) in BASE58.iter().enumerate() {
            m[usize::from(c)] = i as u8;
        }
        m
    });
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    for &c in s {
        let d = *map.get(usize::from(c)).filter(|&&d| d != 0xFF)?;
        let mut carry = u32::from(d);
        for x in out.iter_mut().rev() {
            carry += u32::from(*x) * 58;
            *x = carry as u8;
            carry >>= 8;
        }
        while carry > 0 {
            out.insert(0, carry as u8);
            carry >>= 8;
        }
    }
    let zeros = s.iter().take_while(|&&c| c == b'1').count();
    let mut full = vec![0u8; zeros];
    full.extend(out);
    Some(full)
}

/// Base58Check：解码，末四个字节是前面那些的两次 SHA-256 的头四个字节。对得上返回前面那些
pub(super) fn base58check(s: &[u8]) -> Option<Vec<u8>> {
    let mut v = base58(s)?;
    if v.len() < 5 {
        return None;
    }
    let sum = v.split_off(v.len() - 4);
    let h = Sha256::digest(Sha256::digest(&v));
    (h[..4] == sum[..]).then_some(v)
}

/// WIF 私钥：51 位（未压缩，主网 `5`、测试网 `9` 开头）或 52 位（压缩，主网 `K` `L`、测试网
/// `c` 开头），Base58Check 解出来是版本字节 `0x80`（主网）/ `0xEF`（测试网）、32 字节私钥，
/// 压缩的再带一个 `0x01`
pub(super) fn wif(tok: &str) -> bool {
    let b = tok.as_bytes();
    let first_ok = match b.len() {
        51 => matches!(b[0], b'5' | b'9'),
        52 => matches!(b[0], b'K' | b'L' | b'c'),
        _ => false,
    };
    if !first_ok {
        return false;
    }
    let Some(v) = base58check(b) else {
        return false;
    };
    matches!(v.first(), Some(0x80 | 0xEF)) && (v.len() == 33 || (v.len() == 34 && v[33] == 0x01))
}

/// BIP32 扩展私钥：`xprv`（主网）、`tprv`（测试网）、`yprv`、`zprv`（BIP49、BIP84）开头的
/// 111 位，Base58Check 解出来 78 字节、头四个字节是对应的版本号
pub(super) fn extended_key(tok: &str) -> bool {
    const VERSIONS: [(&[u8], [u8; 4]); 4] = [
        (b"xprv", [0x04, 0x88, 0xAD, 0xE4]),
        (b"tprv", [0x04, 0x35, 0x83, 0x94]),
        (b"yprv", [0x04, 0x9D, 0x78, 0x78]),
        (b"zprv", [0x04, 0xB2, 0x43, 0x0C]),
    ];
    let b = tok.as_bytes();
    if b.len() != 111 {
        return false;
    }
    let Some((_, version)) = VERSIONS.iter().find(|(p, _)| b.starts_with(p)) else {
        return false;
    };
    base58check(b).is_some_and(|v| v.len() == 78 && v[..4] == version[..])
}

// ---------------------------------------------------------------- 助记词

/// 一串连着的词里最多看多少个：再长的一串（一份词表本身）只看头尾，见 [`Run::settle`]
const RUN_MAX: usize = 256;

/// 一串用空白、逗号、换行隔开的词表里的词
#[derive(Default)]
struct Run {
    idx: Vec<u16>,
    spans: Vec<Range<usize>>,
    /// 最后一个词（或编号）的终点
    last: usize,
}

impl Run {
    fn push(&mut self, i: u16, r: Range<usize>) {
        self.last = r.end;
        if self.idx.len() < RUN_MAX {
            self.idx.push(i);
            self.spans.push(r);
        }
    }

    /// 这一串断了：从头、从尾各取一段最长的、校验和对得上的，记下来。
    ///
    /// **只看两头**：前面多一个不相干的词（`seed: abandon …`）、后面多一个都认得出；而一串
    /// 几百个词（贴进来的一份词表）不会因此逐段算几百遍 SHA-256
    fn settle(&mut self, id: &'static str, out: &mut Vec<Hit>) {
        let n = self.idx.len();
        if n >= 12 {
            'found: for len in [24, 21, 18, 15, 12] {
                if len > n {
                    continue;
                }
                for from in [0, n - len] {
                    if mnemonic(&self.idx[from..from + len]) {
                        out.push(builtin_hit(
                            id,
                            self.spans[from].start..self.spans[from + len - 1].end,
                        ));
                        break 'found;
                    }
                }
            }
        }
        self.idx.clear();
        self.spans.clear();
    }
}

/// 两个词之间只隔着分隔：空格、逗号、换行（真的，或者 JSON 里的 `\n`）、编号后面的 `)`。不隔东西的不算 —— 连着写的十二个汉字是一句话，不是助记词
fn separated(gap: &[u8]) -> bool {
    if gap.is_empty() || gap.len() > 12 {
        return false;
    }
    let mut i = 0;
    while i < gap.len() {
        match gap[i] {
            b' ' | b',' | b'\t' | b'\n' | b'\r' | b')' => i += 1,
            b'\\' if matches!(gap.get(i + 1), Some(b'n' | b't' | b'r')) => i += 2,
            _ => return false,
        }
    }
    true
}

/// 英文助记词：一个 token 一个词，由切 token 那一遍喂进来。
///
/// **先只看形状**：小写、3 到 8 个字母、彼此只隔着分隔（和编号）的一串 token，头 11 个只记下
/// 位置，凑满 12 个才开始查词表（之后来一个查一个）。注释和文档里成串的小写单词到处都是，
/// 但多数串不到 12 个就断了，那些一次表都不用查
#[derive(Default)]
pub(super) struct Words {
    count: usize,
    /// 这一串头 12 个词的位置
    head: [(u32, u32); 12],
    /// 最后一个词（或编号）的终点
    last: usize,
    /// 查过表的、连着的词表里的词
    run: Run,
}

/// 3 到 8 个小写字母，头三个字母是词表里有的。**每个 token 都问一次**：八个字节读进一个整数一起比，不逐字节循环
#[inline]
fn word_shaped(t: &[u8]) -> bool {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let n = t.len();
    if !(3..=8).contains(&n) {
        return false;
    }
    // 不足八个字节的用 `a` 补上
    let mut buf = [b'a'; 8];
    buf[..n].copy_from_slice(t);
    let x = u64::from_le_bytes(buf);
    // 都是 ASCII，都不小于 `a`（加 0x1F 进到高位），都不大于 `z`（加 0x05 进不到高位）
    if x & HIGH != 0 || (x + 0x1F * ONES) & HIGH != HIGH || (x + 0x05 * ONES) & HIGH != 0 {
        return false;
    }
    // 头三个字母要是词表里某个词的头三个字母：平常的英文单词多数在这里就不算了
    let k = key3(t);
    HEADS[k / 64] & (1 << (k % 64)) != 0
}

fn numbering(t: &[u8]) -> bool {
    let t = t.strip_suffix(b".").unwrap_or(t);
    (1..=2).contains(&t.len()) && t.iter().all(u8::is_ascii_digit)
}

impl Words {
    #[inline]
    pub(super) fn feed(&mut self, text: &str, tok: &str, span: Range<usize>, out: &mut Vec<Hit>) {
        let t = tok.as_bytes();
        let shaped = word_shaped(t);
        // 绝大多数 token 走这一行：不在一串里，也不像个词
        if self.count == 0 {
            if shaped {
                self.count = 1;
                self.head[0] = (span.start as u32, span.end as u32);
                self.last = span.end;
            }
            return;
        }
        if !shaped && !numbering(t) {
            self.settle(out);
            return;
        }
        let joined = separated(&text.as_bytes()[self.last..span.start]);
        if !joined {
            self.settle(out);
            if shaped {
                self.count = 1;
                self.head[0] = (span.start as u32, span.end as u32);
                self.last = span.end;
            }
            return;
        }
        self.last = span.end;
        // 编号（`1.`、`12`）不打断一串，也不算进去
        if !shaped {
            return;
        }
        let b = text.as_bytes();
        if self.count < 12 {
            self.head[self.count] = (span.start as u32, span.end as u32);
            self.count += 1;
            if self.count == 12 {
                for (s, e) in self.head {
                    self.look(b, s as usize..e as usize, out);
                }
            }
        } else {
            self.count += 1;
            self.look(b, span, out);
        }
    }

    fn look(&mut self, b: &[u8], r: Range<usize>, out: &mut Vec<Hit>) {
        match english_word(&b[r.clone()]) {
            Some(w) => self.run.push(w, r),
            None => self.run.settle("crypto-mnemonic", out),
        }
    }

    /// 这一串断了
    fn settle(&mut self, out: &mut Vec<Hit>) {
        if self.count >= 12 {
            self.run.settle("crypto-mnemonic", out);
        }
        self.count = 0;
    }

    pub(super) fn finish(&mut self, _text: &str, out: &mut Vec<Hit>) {
        self.settle(out);
    }
}

/// `b[i..i + 16]` 读成一个整数（调用方保证够长）
#[inline(always)]
fn word16(b: &[u8], i: usize) -> u128 {
    b[i..]
        .first_chunk::<16>()
        .map_or(0, |c| u128::from_le_bytes(*c))
}

/// 简体中文助记词：汉字不是 token 里的字符，整段另走一遍。
///
/// 一段纯 ASCII 的文本里只可能是 `\uXXXX` 写的（Python 的客户端默认这么转义）：先看有没有
/// `\u`，没有就一个字节都不用再看。
///
/// 不是纯 ASCII 的，**按「一段连着的非 ASCII 字节」看**：助记词的词和词之间隔着 ASCII 的分隔
/// （钱包导出的就是空格），所以一个词自己就是一段，只有一段的头尾挨着分隔的那个字可能是词。
/// 中文正文里字挨着字，中间那些一个都不用解码、查表。全角逗号、顿号隔开的不认：它们在中文
/// 正文里到处都是，挨个查表的代价落在每一个中文请求上
pub(super) fn chinese(text: &str, out: &mut Vec<Hit>) {
    let b = text.as_bytes();
    let mut run = Run::default();
    let mut feed = |run: &mut Run, cp: u32, at: Range<usize>| {
        if let Some(w) = chinese_word(cp) {
            if run.idx.is_empty() || !separated(&b[run.last..at.start]) {
                run.settle("crypto-mnemonic", out);
            }
            run.push(w, at);
        }
    };
    if text.is_ascii() {
        for at in memchr::memmem::find_iter(b, b"\\u") {
            let cp = b
                .get(at + 2..at + 6)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u32::from_str_radix(h, 16).ok());
            // 前面是奇数个反斜杠才是转义（`\\u4e00` 是一个反斜杠加 `u4e00`）
            let mut n = 0;
            while at > n && b[at - 1 - n] == b'\\' {
                n += 1;
            }
            if let Some(cp) = cp
                && n % 2 == 0
            {
                feed(&mut run, cp, at..at + 6);
            }
        }
    } else {
        const HIGH: u128 = 0x8080_8080_8080_8080_8080_8080_8080_8080;
        let mut i = 0;
        while i < b.len() {
            // 跳到下一个非 ASCII 字节：一次看十六个，最低的那个高位在哪儿就落在哪儿
            if b[i] < 0x80 {
                let mut found = false;
                while i + 16 <= b.len() {
                    let w = word16(b, i);
                    let high = w & HIGH;
                    if high != 0 {
                        i += (high.trailing_zeros() / 8) as usize;
                        found = true;
                        break;
                    }
                    i += 16;
                }
                if !found {
                    while i < b.len() && b[i] < 0x80 {
                        i += 1;
                    }
                }
                continue;
            }
            // 这一段非 ASCII 到哪儿为止
            let start = i;
            loop {
                if i + 16 > b.len() {
                    while i < b.len() && b[i] >= 0x80 {
                        i += 1;
                    }
                    break;
                }
                let low = !word16(b, i) & HIGH;
                if low != 0 {
                    i += (low.trailing_zeros() / 8) as usize;
                    break;
                }
                i += 16;
            }
            let end = i;
            // 可能是词的只有这一段的头一个字和最后一个字（词和词之间隔着 ASCII 的分隔）
            let mut done = start;
            let mut try_at = |p: usize, run: &mut Run| {
                if p < done || p + 3 > end || !(0xE4..=0xE9).contains(&b[p]) {
                    return;
                }
                let cp = (u32::from(b[p] & 0x0F) << 12)
                    | (u32::from(b[p + 1] & 0x3F) << 6)
                    | u32::from(b[p + 2] & 0x3F);
                feed(run, cp, p..p + 3);
                done = p + 3;
            };
            // 头一个字前面、最后一个字后面要是分隔（ASCII 的空白、逗号，或者 JSON 里的 `\n`）
            if start > 0
                && matches!(
                    b[start - 1],
                    b' ' | b',' | b'\t' | b'\n' | b'\r' | b')' | b'n' | b't' | b'r'
                )
            {
                try_at(start, &mut run);
            }
            if b.get(end)
                .is_none_or(|c| matches!(c, b' ' | b',' | b'\t' | b'\n' | b'\r' | b'\\'))
            {
                try_at(end.saturating_sub(3).max(start), &mut run);
            }
        }
    }
    run.settle("crypto-mnemonic", out);
}
