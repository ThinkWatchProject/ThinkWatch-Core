//! 锚点：一条规则（一个判据）**只可能在哪里命中**。
//!
//! 扫描分两段（见 [`super::engine`]）：第一段一遍过整段文本，只看每个 token 对不对得上某个
//! 锚点；第二段只在对得上的 token 上跑那条规则原来的判据。锚点是命中的**必要条件** —— 宁可
//! 多放几个 token 进第二段，也不能漏掉一个判据会认的：两种扫法的结果由对照测试钉成一模一样。
//!
//! 锚点只有几种，都能在第一段里用一两次查表判断：
//!
//! - [`Anchor::Prefix`]、[`Anchor::Word`]：字面量，全部编进**一个** Aho-Corasick 自动机，
//!   从 token 的头一个字节起走（锚定在 token 开头，所以走不了几步就到死状态）；
//! - [`Anchor::First`]、[`Anchor::Last`]、[`Anchor::Ends`]：头一个（最后一个）字节属于哪一类、
//!   最后两个字母是什么、token 多长 ——
//!   号码、助记词这类没有固定文字的判据，靠它们和 [`Fine`] 的一个便宜的形状判断。
//!
//! 不看 token 边界的锚点（`-----BEGIN `、`://`、`@`、`<<`、`+`、汉字）各自的判据本来就是先用
//! SIMD 的 `memchr` / `memmem` 找到它们再往两边看，就是这个意思，不再并进来。

/// 一个字节集合（256 位），编译时拼好
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Bytes([u64; 4]);

impl Bytes {
    /// 这几个字节
    pub(super) const fn of(list: &[u8]) -> Bytes {
        let mut m = [0u64; 4];
        let mut i = 0;
        while i < list.len() {
            let b = list[i] as usize;
            m[b / 64] |= 1 << (b % 64);
            i += 1;
        }
        Bytes(m)
    }

    /// `lo..=hi`
    pub(super) const fn range(lo: u8, hi: u8) -> Bytes {
        let mut m = [0u64; 4];
        let mut b = lo as usize;
        while b <= hi as usize {
            m[b / 64] |= 1 << (b % 64);
            b += 1;
        }
        Bytes(m)
    }

    pub(super) const fn or(self, o: Bytes) -> Bytes {
        Bytes([
            self.0[0] | o.0[0],
            self.0[1] | o.0[1],
            self.0[2] | o.0[2],
            self.0[3] | o.0[3],
        ])
    }

    #[inline]
    pub(super) const fn has(&self, b: u8) -> bool {
        self.0[(b / 64) as usize] & (1 << (b % 64)) != 0
    }
}

pub(super) const DIGIT: Bytes = Bytes::range(b'0', b'9');
pub(super) const UPPER: Bytes = Bytes::range(b'A', b'Z');
pub(super) const LOWER: Bytes = Bytes::range(b'a', b'z');
pub(super) const ALNUM: Bytes = DIGIT.or(UPPER).or(LOWER);
/// token 里能有的全部字节（和 `rules::is_tok` 同一个判据）
pub(super) const TOKEN: Bytes = ALNUM.or(Bytes::of(b"-_."));

/// 不设上限
pub(super) const ANY_LEN: usize = usize::MAX;

/// token 里有哪几类字符（切 token 时顺手或起来的，见 [`class`]）
pub(super) const C_DOT: u8 = 1;
pub(super) const C_DIGIT: u8 = 2;
pub(super) const C_UPPER: u8 = 4;
pub(super) const C_LOWER: u8 = 8;
pub(super) const C_DASH: u8 = 16;
pub(super) const C_UNDER: u8 = 32;
pub(super) const C_LETTER: u8 = C_UPPER | C_LOWER;

/// 一个 token 字节属于哪一类；不是 token 里的字节是 0
pub(super) const fn class(b: u8) -> u8 {
    match b {
        b'.' => C_DOT,
        b'0'..=b'9' => C_DIGIT,
        b'A'..=b'Z' => C_UPPER,
        b'a'..=b'z' => C_LOWER,
        b'-' => C_DASH,
        b'_' => C_UNDER,
        _ => 0,
    }
}

/// 一组锚点对 token 里字符类的要求：`need` 里的每一类都得有，`forbid` 里的一类都不能有
#[derive(Clone, Copy)]
pub(super) struct Chars {
    pub need: u8,
    pub forbid: u8,
}

/// 不挑
pub(super) const ANY_CHARS: Chars = Chars { need: 0, forbid: 0 };

/// 一个锚点。长度都按整个 token 算（字节数，含句末的句号）
#[derive(Clone, Copy)]
pub(super) enum Anchor {
    /// token 以这段文字开头（分大小写），整个 token 至少 `.1` 个字节
    Prefix(&'static str, usize),
    /// 整个 token 就是这个词（不分大小写）
    Word(&'static str),
    /// 头一个字节在集合里，长度在 `.1..=.2` 里
    First(Bytes, usize, usize),
    /// 最后一个字节在集合里，长度至少 `.1`
    Last(Bytes, usize),
    /// 以其中一个词结尾（不分大小写，每个词至少两个字母），长度至少 `.1`。第一段只查最后两个
    /// 字母，整个词由 [`Fine`] 核对
    Ends(&'static [&'static str], usize),
}

/// 对得上锚点之后，进第二段之前再看一眼的形状（只看 token 本身，要便宜）。`None` 是不再看
pub(super) type Fine = Option<fn(&[u8]) -> bool>;

/// 同上，看的是 token 旁边：整段文本的字节、token 的起点和终点
pub(super) type Near = Option<fn(&[u8], usize, usize) -> bool>;

/// 一组锚点和它们共用的形状判断：一个判据（或者同一个 token 上一起跑的几个判据）声明一份
#[derive(Clone, Copy)]
pub(super) struct Anchors {
    pub list: &'static [Anchor],
    /// 字符类的要求（所有锚点共用）
    pub chars: Chars,
    pub fine: Fine,
    pub near: Near,
}

/// 去掉句末句号之后的那一段（号码类的判据都这么切）
#[inline]
pub(super) fn body(t: &[u8]) -> &[u8] {
    let mut n = t.len();
    while n > 0 && t[n - 1] == b'.' {
        n -= 1;
    }
    &t[..n]
}
