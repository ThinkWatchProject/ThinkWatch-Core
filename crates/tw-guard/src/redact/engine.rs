//! 两段式扫描：**先预筛，再校验**。
//!
//! 规则扩充到一百多条之后，「每个 token 上把每条规则问一遍」的耗时跟着条数线性涨。现在
//! 每个判据声明自己的锚点（[`super::anchor`]，写在判据旁边），扫描分两段：
//!
//! 1. **预筛**（[`prescan`]）：切 token 的那一遍里，每个 token 查几张表 —— 头一个字节、最后
//!    一个字节、长度各一张，字面量锚点走一个锚定在 token 开头的 Aho-Corasick 自动机 —— 得出
//!    它可能是哪几组判据的候选。绝大多数 token 一张表就打发了。助记词另外数「连着几个像词的
//!    token」，不到 12 个的一串整串不要；
//! 2. **校验**（[`scan_tokens`]）：只在候选上跑各组原来的判据，顺序和老写法一样。
//!
//! **结果和逐条扫描一模一样**。锚点是命中的必要条件；跨 token 接起来看的几种（分组写的卡号、
//! 号码、IBAN、MAC、坐标、车牌、助记词）在老写法里，没对上锚点的 token 的作用只是把在接的那
//! 一串截断，这里按「和上一个候选之间隔着别的 token」照样截断。`rules::oracle` 里留着逐条扫描
//! 的老写法，对照测试拿两种扫法在同一份语料上比。

use std::ops::Range;
use std::sync::OnceLock;

use aho_corasick::automaton::Automaton;
use aho_corasick::dfa::DFA;
use aho_corasick::{Anchored, MatchKind, StartKind};

use super::anchor::{ANY_CHARS, Anchor, Chars, Fine, Near, class};
use super::rules::{
    Hit, RuleSet, Spaced, builtin_hit, classify_token, internal_token, looks_like_jwt,
    mobile_phones, personal_token, today_ymd,
};

// ---------------------------------------------------------------- 组

/// 前缀规则和 OpenAI 老式密钥（`rules::classify_token`）
const KEY: u32 = 1 << 0;
const JWT: u32 = 1 << 1;
const IP: u32 = 1 << 2;
const DOMAIN: u32 = 1 << 3;
/// 一个 token 里的大陆手机号（`rules::mobile_phones`）
const PHONE: u32 = 1 << 4;
/// 身份证号、卡号（含分组写的）
const CARD: u32 = 1 << 5;
const PLACE: u32 = 1 << 6;
const ASSIGN: u32 = 1 << 7;
const SHAPES: u32 = 1 << 8;
/// 英文助记词：不进候选，另数成串（见 [`prescan`]）
const WORDS: u32 = 1 << 9;
/// 个人信息那一组，按 [`super::personal::ANCHORS`] 的顺序占 7 位
const PERSONAL_SHIFT: u32 = 10;
const PERSONAL: u32 = 0x7F << PERSONAL_SHIFT;

/// 交给 `secrets::Want::at_token` 的几组
const SECRETS: u32 = PLACE | ASSIGN | SHAPES;
/// 只看 JSON 字符串里面的几组：字符串外面是 JSON 的数值，换掉请求体就坏了
const QUOTED_ONLY: u32 = PHONE | CARD | PERSONAL;

/// 长度表的格数：再长的都落在最后一格，判据自己再看
const LENS: usize = 129;

/// 一组：锚点（任一对上即可）和对上之后的形状判断
struct Group {
    /// 有字面量锚点（[`Anchor::Prefix`]、[`Anchor::Word`]）
    literal: bool,
    /// 其余的锚点
    shapes: Vec<Anchor>,
    /// 查表就已经说准了：没有字面量、只有一个头一个（最后一个）字节加长度的锚点，长度表
    /// 也分得清它的上下限
    exact: bool,
    fine: Fine,
    near: Near,
    chars: Chars,
}

/// 字面量锚点的一项：哪一组、文字、整个 token 至少多长、是不是要整个 token 对上
struct Lit {
    group: u32,
    text: &'static str,
    min_len: usize,
    whole: bool,
}

/// 编好的锚点。**和规则开没开无关**，一个进程编一次；开着哪几组每次扫描问一次（[`enabled`]）
struct Tables {
    groups: Vec<Group>,
    first: [u32; 256],
    last: [u32; 256],
    len: [u32; LENS],
    /// [`Anchor::Word`]：头一个字母（小写）× 长度
    word: [u32; 26 * 32],
    /// [`Anchor::Word`]：头两个字符（[`word2_key`]）
    word2: [u32; 26 * 28],
    /// [`Anchor::Ends`]：最后两个字母（小写）
    ends: [u32; 26 * 26],
    /// token 里的字符类（[`class`] 或起来）对得上哪几组的要求
    chars: [u32; 64],
    literal: u32,
    dfa: DFA,
    lits: Vec<Lit>,
}

/// 头两个字符（不分大小写）在 [`Tables::word2`] 里的位置：头一个是字母，第二个是字母、`-`
/// 或者别的；头一个不是字母是 `None`
#[inline]
fn word2_key(t: &[u8]) -> Option<usize> {
    let a = t[0] | 0x20;
    if !a.is_ascii_lowercase() || t.len() < 2 {
        return None;
    }
    let b = t[1] | 0x20;
    let b = if b.is_ascii_lowercase() {
        usize::from(b - b'a')
    } else if t[1] == b'-' {
        26
    } else {
        27
    };
    Some(usize::from(a - b'a') * 28 + b)
}

/// 最后两个字母（不分大小写）在 [`Tables::ends`] 里的位置；不是两个字母是 `None`
#[inline]
fn ends_key(a: u8, b: u8) -> Option<usize> {
    let (a, b) = (a | 0x20, b | 0x20);
    (a.is_ascii_lowercase() && b.is_ascii_lowercase())
        .then(|| usize::from(a - b'a') * 26 + usize::from(b - b'a'))
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(build)
}

fn build() -> Tables {
    let key = super::rules::key_anchors();
    let personal = super::personal::ANCHORS;
    let mut decl: Vec<(u32, Vec<Anchor>, Fine, Near, Chars)> = vec![
        (KEY, key, None, None, ANY_CHARS),
        (
            JWT,
            super::rules::JWT_ANCHORS.list.to_vec(),
            super::rules::JWT_ANCHORS.fine,
            None,
            super::rules::JWT_ANCHORS.chars,
        ),
        (
            IP,
            super::rules::IP_ANCHORS.list.to_vec(),
            super::rules::IP_ANCHORS.fine,
            None,
            super::rules::IP_ANCHORS.chars,
        ),
        (
            DOMAIN,
            super::rules::DOMAIN_ANCHORS.list.to_vec(),
            None,
            None,
            super::rules::DOMAIN_ANCHORS.chars,
        ),
        (
            PHONE,
            super::rules::PHONE_ANCHORS.list.to_vec(),
            super::rules::PHONE_ANCHORS.fine,
            None,
            super::rules::PHONE_ANCHORS.chars,
        ),
        (
            CARD,
            super::rules::CARD_ANCHORS.list.to_vec(),
            None,
            None,
            ANY_CHARS,
        ),
        (
            PLACE,
            super::secrets::PLACE_ANCHORS.to_vec(),
            None,
            None,
            ANY_CHARS,
        ),
        (
            ASSIGN,
            super::secrets::assign::ANCHORS.list.to_vec(),
            super::secrets::assign::ANCHORS.fine,
            None,
            super::secrets::assign::ANCHORS.chars,
        ),
        (
            SHAPES,
            super::secrets::SHAPE_ANCHORS.concat(),
            None,
            None,
            ANY_CHARS,
        ),
        (
            WORDS,
            super::secrets::crypto::WORD_ANCHORS.list.to_vec(),
            super::secrets::crypto::WORD_ANCHORS.fine,
            None,
            super::secrets::crypto::WORD_ANCHORS.chars,
        ),
    ];
    for (i, a) in personal.iter().enumerate() {
        decl.push((
            1 << (PERSONAL_SHIFT + i as u32),
            a.list.to_vec(),
            a.fine,
            a.near,
            a.chars,
        ));
    }
    let mut t = Tables {
        groups: (0..32)
            .map(|_| Group {
                literal: false,
                shapes: Vec::new(),
                exact: false,
                fine: None,
                near: None,
                chars: ANY_CHARS,
            })
            .collect(),
        first: [0; 256],
        last: [0; 256],
        len: [0; LENS],
        word: [0; 26 * 32],
        word2: [0; 26 * 28],
        ends: [0; 26 * 26],
        chars: [0; 64],
        literal: 0,
        // 先放一个空的，下面编好再换
        dfa: DFA::new(["\u{0}"]).expect("一个字面量"),
        lits: Vec::new(),
    };
    let lens = |t: &mut Tables, g: u32, lo: usize, hi: usize| {
        for l in lo.min(LENS - 1)..=hi.min(LENS - 1) {
            t.len[l] |= g;
        }
    };
    for (g, list, fine, near, chars) in decl {
        let k = g.trailing_zeros() as usize;
        t.groups[k].fine = fine;
        t.groups[k].near = near;
        t.groups[k].chars = chars;
        for a in list {
            match a {
                Anchor::Prefix(p, min_len) => {
                    t.groups[k].literal = true;
                    t.literal |= g;
                    t.first[usize::from(p.as_bytes()[0])] |= g;
                    lens(&mut t, g, min_len.max(p.len()), usize::MAX);
                    t.lits.push(Lit {
                        group: g,
                        text: p,
                        min_len,
                        whole: false,
                    });
                }
                Anchor::Word(w) => {
                    t.groups[k].literal = true;
                    t.literal |= g;
                    let c = w.as_bytes()[0].to_ascii_lowercase();
                    assert!(c.is_ascii_lowercase() && w.len() < 32, "{w}");
                    t.word[usize::from(c - b'a') * 32 + w.len()] |= g;
                    t.word2[word2_key(w.as_bytes()).expect("头一个是字母")] |= g;
                    t.lits.push(Lit {
                        group: g,
                        text: w,
                        min_len: w.len(),
                        whole: true,
                    });
                }
                Anchor::First(set, lo, hi) => {
                    for b in 0..=255u8 {
                        if set.has(b) {
                            t.first[usize::from(b)] |= g;
                        }
                    }
                    lens(&mut t, g, lo, hi);
                    t.groups[k].shapes.push(a);
                }
                Anchor::Last(set, lo) => {
                    for b in 0..=255u8 {
                        if set.has(b) {
                            t.last[usize::from(b)] |= g;
                        }
                    }
                    lens(&mut t, g, lo, usize::MAX);
                    t.groups[k].shapes.push(a);
                }
                Anchor::Ends(list, lo) => {
                    for w in list {
                        let e = &w.as_bytes()[w.len() - 2..];
                        t.ends[ends_key(e[0], e[1]).expect("两个字母")] |= g;
                    }
                    lens(&mut t, g, lo, usize::MAX);
                    t.groups[k].shapes.push(a);
                }
            }
        }
    }
    for (k, g) in t.groups.iter().enumerate() {
        for (c, ok) in t.chars.iter_mut().enumerate() {
            let c = c as u8;
            if c & g.chars.need == g.chars.need && c & g.chars.forbid == 0 {
                *ok |= 1 << k;
            }
        }
    }
    for g in &mut t.groups {
        g.exact = !g.literal
            && match g.shapes[..] {
                [Anchor::First(_, _, hi)] => hi < LENS - 1 || hi == usize::MAX,
                [Anchor::Last(..) | Anchor::Ends(..)] => true,
                _ => false,
            };
    }
    // 不分大小写地编：标记词本来就不分；分大小写的前缀对上之后再逐字节核对一遍
    t.dfa = DFA::builder()
        .ascii_case_insensitive(true)
        .match_kind(MatchKind::Standard)
        .start_kind(StartKind::Anchored)
        .build(t.lits.iter().map(|l| l.text))
        .expect("锚点的字面量编得成自动机");
    t
}

impl Tables {
    /// 这个 token 的开头对上了哪几组的字面量锚点
    #[inline]
    fn literals(&self, t: &[u8]) -> u32 {
        let Ok(mut sid) = self.dfa.start_state(Anchored::Yes) else {
            return 0;
        };
        let mut got = 0;
        for (i, &b) in t.iter().enumerate() {
            sid = self.dfa.next_state(Anchored::Yes, sid, b);
            if self.dfa.is_dead(sid) {
                break;
            }
            if self.dfa.is_match(sid) {
                let depth = i + 1;
                for k in 0..self.dfa.match_len(sid) {
                    let p = self.dfa.match_pattern(sid, k).as_usize();
                    let l = &self.lits[p];
                    if l.text.len() != depth || t.len() < l.min_len {
                        continue;
                    }
                    let ok = if l.whole {
                        t.len() == depth
                    } else {
                        &t[..depth] == l.text.as_bytes()
                    };
                    if ok {
                        got |= l.group;
                    }
                }
            }
        }
        got
    }

    /// `text[s..e]` 这个 token 对上了第 `k` 组的锚点（`lit`：对上的字面量锚点，
    /// [`Tables::literals`]），形状、旁边也对
    #[inline]
    fn fits(&self, k: usize, text: &[u8], s: usize, e: usize, lit: u32) -> bool {
        let g = &self.groups[k];
        let t = &text[s..e];
        if g.exact {
            return g.fine.is_none_or(|f| f(t)) && g.near.is_none_or(|f| f(text, s, e));
        }
        let l = t.len();
        let hit = (g.literal && lit & (1 << k) != 0)
            || g.shapes.iter().any(|a| match *a {
                Anchor::First(set, lo, hi) => set.has(t[0]) && (lo..=hi).contains(&l),
                Anchor::Last(set, lo) => set.has(t[l - 1]) && l >= lo,
                Anchor::Ends(list, lo) => {
                    l >= lo
                        && list.iter().any(|w| {
                            l >= w.len() && t[l - w.len()..].eq_ignore_ascii_case(w.as_bytes())
                        })
                }
                _ => false,
            });
        hit && g.fine.is_none_or(|f| f(t)) && g.near.is_none_or(|f| f(text, s, e))
    }
}

/// 这一次开着哪几组
fn enabled(set: &RuleSet, secrets: &super::secrets::Want, personal: &super::personal::Want) -> u32 {
    let mut on = 0;
    let mut add = |g: u32, yes: bool| {
        if yes {
            on |= g;
        }
    };
    add(KEY, super::rules::any_key_on(set));
    add(JWT, set.is_on("jwt"));
    add(IP, set.is_on("internal-ip"));
    add(DOMAIN, set.is_on("internal-domain"));
    add(PHONE, set.is_on("cn-mobile-phone"));
    add(CARD, set.is_on("bank-card") || set.is_on("cn-resident-id"));
    add(PLACE, secrets.place_on());
    add(ASSIGN, secrets.assign_on());
    add(SHAPES, secrets.shapes_on());
    add(WORDS, secrets.mnemonic_on());
    for (i, yes) in personal.anchors_on().into_iter().enumerate() {
        add(1 << (PERSONAL_SHIFT + i as u32), yes);
    }
    on
}

// ---------------------------------------------------------------- 第一段

/// 一个候选 token
struct Cand {
    span: Range<usize>,
    /// 第几个 token（从 0 数）：两个候选之间隔没隔着别的 token
    idx: usize,
    /// 它前面有几个 JSON 字符串外面的 token
    unquoted: usize,
    groups: u32,
}

/// 第一段的结果
#[derive(Default)]
struct Pre {
    cands: Vec<Cand>,
    /// 英文助记词候选：连着 12 个以上像词的 token 的那几段（从第一个词的起点到最后一个的终点）
    word_runs: Vec<Range<usize>>,
    /// 中文助记词候选：文本里有非 ASCII 字节时（`has_high`）是连着的非 ASCII 字节的段，纯 ASCII
    /// 时是 `\uXXXX`（`escapes`，每个 6 个字节）。都只留够长的几串（[`Streak`]）
    high: Vec<Range<usize>>,
    escapes: Vec<Range<usize>>,
    has_high: bool,
    /// 最后一个会截断个人信息里在接的号码的 token：JSON 字符串外面的，或者去掉句末句号
    /// 还剩东西的（见 `personal::Want::feed`）
    last_cut: Option<usize>,
    /// 最后一个 JSON 字符串外面的 token
    last_unquoted: Option<usize>,
    /// 没转义的引号的位置（要的时候才记，见 [`prescan`] 的 `quotes`）
    quotes: Vec<usize>,
}

impl Pre {
    /// JSON 字符串的区间：没转义的引号两两一对，最后一个没配上的到文本末尾。文本是合规的 JSON
    /// 时（字符串外面没有反斜杠）和 `rules::json_string_spans` 找的一样，对照测试比过
    fn string_spans(&self, len: usize) -> Vec<Range<usize>> {
        self.quotes
            .chunks(2)
            .map(|q| q[0] + 1..q.get(1).copied().unwrap_or(len))
            .collect()
    }
}

/// token 里的字节是哪一类（[`class`]；不是 token 里的字节是 0，和 `rules::is_tok` 同一个
/// 判据），查表
const CLS: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut b = 0;
    while b < 256 {
        t[b] = class(b as u8);
        b += 1;
    }
    t
};

/// 第一段：切 token（和 `rules::for_each_token` 切得一样，对照测试比过），每个 token 查表，
/// 记下候选；顺手记下非 ASCII 的段和 `\u` 转义（中文助记词只在这些地方找）。
/// `quotes`：记下没转义的引号的位置（按字段名认的、自定义规则要 JSON 字符串的区间）
fn prescan(text: &str, json: bool, on: u32, quotes: bool) -> Pre {
    let tab = tables();
    let b = text.as_bytes();
    let n = b.len();
    let mut pre = Pre::default();
    let words_on = on & WORDS != 0;
    // 个人信息那一组开着才要记最后几个会截断号码的 token 在哪儿
    let track = on & PERSONAL != 0;
    let (mut high, mut esc) = (Streak::default(), Streak::default());
    let mut idx = 0usize;
    let mut unquoted = 0usize;
    let mut quoted = false;
    // 正在数的一串助记词候选：起点、终点、其中像词的有几个
    let mut run: Option<(usize, usize)> = None;
    let mut shaped = 0usize;
    let mut i = 0usize;
    loop {
        // token 之间：反斜杠连同它转义掉的字符、引号、非 ASCII 的段
        while i < n {
            let c = b[i];
            if CLS[usize::from(c)] != 0 {
                break;
            }
            if c >= 0x80 {
                let s = i;
                i += 1;
                while i < n && b[i] >= 0x80 {
                    i += 1;
                }
                pre.has_high = true;
                if words_on {
                    high.push(s..i, &mut pre.high);
                }
                continue;
            }
            i += 1;
            if c == b'\\' {
                if i < n && b[i] == b'u' {
                    let at = i - 1;
                    i += 1;
                    let mut k = 0;
                    while k < 4 && i < n && b[i].is_ascii_hexdigit() {
                        i += 1;
                        k += 1;
                    }
                    if k == 4 && words_on {
                        esc.push(at..at + 6, &mut pre.escapes);
                    }
                } else if i < n {
                    if b[i] >= 0x80 {
                        // 转义掉的是一个多字节字符的头一个字节：它仍是一段非 ASCII 的开头
                        let s = i;
                        i += 1;
                        while i < n && b[i] >= 0x80 {
                            i += 1;
                        }
                        pre.has_high = true;
                        if words_on {
                            high.push(s..i, &mut pre.high);
                        }
                        continue;
                    }
                    i += 1;
                }
            } else if c == b'"' {
                quoted = !quoted;
                if quotes {
                    pre.quotes.push(i - 1);
                }
            }
        }
        if i >= n {
            break;
        }
        let start = i;
        let mut cls = CLS[usize::from(b[i])];
        i += 1;
        while i < n {
            let c = CLS[usize::from(b[i])];
            if c == 0 {
                break;
            }
            cls |= c;
            i += 1;
        }
        // ---- 这个 token
        let t = &b[start..i];
        let l = t.len();
        let k = idx;
        idx += 1;
        let outside = json && !quoted;
        if track {
            if outside {
                pre.last_unquoted = Some(k);
            }
            if outside || t[0] != b'.' || t.iter().any(|&c| c != b'.') {
                pre.last_cut = Some(k);
            }
        }
        let mut m = (tab.first[usize::from(t[0])] | tab.last[usize::from(t[l - 1])])
            & tab.len[l.min(LENS - 1)];
        let c = t[0] | 0x20;
        if l < 32
            && c.is_ascii_lowercase()
            && let Some(w2) = word2_key(t)
        {
            m |= tab.word[usize::from(c - b'a') * 32 + l] & tab.word2[w2];
        }
        if l >= 2
            && let Some(e) = ends_key(t[l - 2], t[l - 1])
        {
            m |= tab.ends[e] & tab.len[l.min(LENS - 1)];
        }
        m &= on & tab.chars[usize::from(cls)];
        if words_on {
            // 只数连着的、形状对得上的（3 到 8 个小写字母，或者编号）：一串里小写的不到 12 个的
            // 整串不要，够数的才逐个查像不像词（[`word_runs`]）
            if m & WORDS != 0 {
                let from = run.map_or(start, |r| r.0);
                run = Some((from, i));
                if t[0].is_ascii_lowercase() {
                    shaped += 1;
                }
            } else if let Some(r) = run.take() {
                if shaped >= 12 {
                    word_runs(text, r.0..r.1, &mut pre.word_runs);
                }
                shaped = 0;
            }
            m &= !WORDS;
        }
        if outside {
            m &= !QUOTED_ONLY;
        }
        if m != 0 {
            let lit = if m & tab.literal != 0 {
                tab.literals(t)
            } else {
                0
            };
            let mut groups = 0;
            let mut rest = m;
            while rest != 0 {
                let g = rest.trailing_zeros() as usize;
                rest &= rest - 1;
                if tab.fits(g, b, start, i, lit) {
                    groups |= 1 << g;
                }
            }
            if groups != 0 {
                pre.cands.push(Cand {
                    span: start..i,
                    idx: k,
                    unquoted,
                    groups,
                });
            }
        }
        if outside {
            unquoted += 1;
        }
    }
    if let Some((s, e)) = run
        && shaped >= 12
    {
        word_runs(text, s..e, &mut pre.word_runs);
    }
    high.flush(&mut pre.high);
    esc.flush(&mut pre.escapes);
    pre
}

/// 中文助记词的候选，边扫边接：连着 12 段以上、相邻两段之间隔着不到 13 个字节的非 ASCII 段（或者
/// `\uXXXX`）。两个词之间只能隔着至多 12 个字节的分隔（`crypto::separated`），中间夹着别的
/// 非 ASCII 字符、别的转义都不行，所以一组助记词的 12 个字一定落在这样连着的 12 段里；够不上
/// 的（中文正文里的标点、隔得远的字）一段都不用解码、查表
///
/// 一串够 12 段才交出去
#[derive(Default)]
struct Streak {
    cur: Vec<Range<usize>>,
}

impl Streak {
    #[inline]
    fn push(&mut self, r: Range<usize>, out: &mut Vec<Range<usize>>) {
        if self.cur.last().is_some_and(|l| r.start > l.end + 12) {
            self.flush(out);
        }
        self.cur.push(r);
    }

    fn flush(&mut self, out: &mut Vec<Range<usize>>) {
        if self.cur.len() >= 12 {
            out.append(&mut self.cur);
        }
        self.cur.clear();
    }
}

/// 一串形状对得上的 token（`streak`，从一个 token 的开头到一个 token 的结尾）里，连着的、
/// 像 BIP39 词的（[`super::secrets::crypto::WORD_ANCHORS`]）有 12 个以上的那几段
fn word_runs(text: &str, streak: Range<usize>, out: &mut Vec<Range<usize>>) {
    let off = streak.start;
    let mut run: Option<Range<usize>> = None;
    let mut shaped = 0;
    let mut close = |run: &mut Option<Range<usize>>, shaped: &mut usize| {
        if let Some(r) = run.take()
            && *shaped >= 12
        {
            out.push(r);
        }
        *shaped = 0;
    };
    super::rules::for_each_token(&text[streak], |tok, r, _| {
        let t = tok.as_bytes();
        if super::secrets::crypto::run_member(t) {
            let from = run.as_ref().map_or(off + r.start, |x| x.start);
            run = Some(from..off + r.end);
            if t[0].is_ascii_lowercase() {
                shaped += 1;
            }
        } else {
            close(&mut run, &mut shaped);
        }
    });
    close(&mut run, &mut shaped);
}

// ---------------------------------------------------------------- 第二段

/// 按 token 认的那些：第一段预筛，第二段在候选上跑各组的判据。整段文本过一遍的（中文助记词、
/// 护照机读区、住址……）在前面，和老写法的顺序一样
///
/// 返回切 token 时顺手记下的 JSON 字符串的区间（文本是 JSON、后面有人要的时候），自定义规则接着用
pub(super) fn scan_tokens(
    text: &str,
    set: &RuleSet,
    json: bool,
    out: &mut Vec<Hit>,
) -> Option<Vec<Range<usize>>> {
    let want_openai = set.is_on("openai-api-key");
    let want_ip = set.is_on("internal-ip");
    let want_domain = set.is_on("internal-domain");
    let want_id = set.is_on("cn-resident-id");
    let want_card = set.is_on("bank-card");
    let today = if want_id || want_card { today_ymd() } else { 0 };
    let mut secrets = super::secrets::Want::new(set);
    let mut personal = super::personal::Want::new(set);
    let on = enabled(set, &secrets, &personal);
    if on == 0 {
        personal.text(text, json, None, out);
        return None;
    }
    let pre = prescan(
        text,
        json,
        on,
        json && (personal.labeled_on() || set.has_custom()),
    );
    let spans = (json && !pre.quotes.is_empty()).then(|| pre.string_spans(text.len()));
    // 整段文本过一遍的（中文助记词；护照机读区、住址……）在按 token 认的前面，和老写法一样
    if on & WORDS != 0 {
        let mut zh = super::secrets::crypto::Chinese::default();
        let b = text.as_bytes();
        if !pre.has_high {
            for x in &pre.escapes {
                zh.escape(b, x.start, out);
            }
        } else {
            for x in &pre.high {
                zh.high_run(b, x.start, x.end, out);
            }
        }
        zh.finish(out);
    }
    personal.text(text, json, spans.as_deref(), out);
    let mut spaced = Spaced::default();
    // 上一个个人信息候选（第几个 token、它前面有几个字符串外面的）、上一个卡号候选
    let mut last_personal: Option<(usize, usize)> = None;
    let mut last_card: Option<usize> = None;
    for c in &pre.cands {
        let tok = &text[c.span.clone()];
        let at = c.span.start;
        if c.groups & SECRETS != 0 {
            secrets.at_token(text, tok, c.span.clone(), out);
        }
        if c.groups & PERSONAL != 0 {
            let gap = match last_personal {
                Some((i, u)) => (c.idx != i + 1).then_some(c.unquoted != u),
                None => Some(c.unquoted > 0),
            };
            personal.token_at(text, tok, c.span.clone(), json, gap, out);
            last_personal = Some((c.idx, c.unquoted));
        }
        if c.groups & PHONE != 0 {
            mobile_phones(tok, at, out);
        }
        if c.groups & CARD != 0 {
            // 和上一个之间隔着的 token 不是一组数字，在接的那一串到此为止
            if last_card.is_none_or(|i| c.idx != i + 1) {
                spaced.finish(text, out);
            }
            last_card = Some(c.idx);
            // 句末的句号不是号码的一部分：`…卡号是 6222 0212 3456 7894.`
            let body = tok.trim_end_matches('.');
            if want_card {
                spaced.feed(text, at, body, body.len() < tok.len(), out);
            }
            personal_token(body, at, want_id, want_card, today, out);
        }
        if c.groups & KEY != 0
            && let Some(id) = classify_token(tok, set, want_openai)
        {
            out.push(builtin_hit(id, c.span.clone()));
            continue;
        }
        if c.groups & JWT != 0 && looks_like_jwt(tok) {
            out.push(builtin_hit("jwt", c.span.clone()));
            continue;
        }
        if c.groups & (IP | DOMAIN) != 0
            && let Some(id) = internal_token(tok, want_ip, want_domain)
        {
            out.push(builtin_hit(id, c.span.clone()));
        }
    }
    // 老写法里，最后一个候选之后的 token 在扫的过程中就截断了在接的号码 —— 在按位置认的值
    // 让开别的规则（`secrets::Want::finish`）之前。这里照样在那之前截断
    if let Some((i, _)) = last_personal {
        if pre.last_unquoted.is_some_and(|u| u > i) {
            personal.cut(text, true, out);
        } else if pre.last_cut.is_some_and(|u| u > i) {
            personal.cut(text, false, out);
        }
    }
    for r in &pre.word_runs {
        secrets.word_run(text, r.clone(), out);
    }
    spaced.finish(text, out);
    secrets.finish(text, out);
    personal.finish(text, out);
    spans
}

/// 切 token 时记下的 JSON 字符串的区间（对照测试和 `rules::json_string_spans` 比）
#[cfg(test)]
pub(super) fn string_spans(text: &str) -> Vec<Range<usize>> {
    prescan(text, true, 0, true).string_spans(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_group_has_anchors_and_literals_compile() {
        let t = tables();
        for g in [
            KEY, JWT, IP, DOMAIN, PHONE, CARD, PLACE, ASSIGN, SHAPES, WORDS,
        ] {
            let k = g.trailing_zeros() as usize;
            assert!(
                t.groups[k].literal || !t.groups[k].shapes.is_empty(),
                "第 {k} 组没有锚点"
            );
        }
        for i in 0..super::super::personal::ANCHORS.len() {
            assert!(!t.groups[PERSONAL_SHIFT as usize + i].shapes.is_empty());
        }
        assert!(t.lits.len() > 100);
    }

    #[test]
    fn literal_anchors_are_matched_at_the_token_start_only() {
        let t = tables();
        let key = format!("sk-ant-{}", "A".repeat(30));
        assert_ne!(t.literals(key.as_bytes()) & KEY, 0);
        // 不在开头的不算；分大小写的前缀大小写不对的不算
        assert_eq!(t.literals(format!("x{key}").as_bytes()) & KEY, 0);
        assert_eq!(
            t.literals(format!("akia{}", "A".repeat(16)).as_bytes()) & KEY,
            0
        );
        assert_ne!(
            t.literals(format!("AKIA{}", "A".repeat(16)).as_bytes()) & KEY,
            0
        );
        // 标记词要整个 token 对上，不分大小写
        assert_ne!(t.literals(b"Authorization") & PLACE, 0);
        assert_eq!(t.literals(b"Authorizations") & PLACE, 0);
        // 太短的不算
        assert_eq!(t.literals(b"sk-ant-abc") & KEY, 0);
    }
}
