//! Windows 交接口令的纯函数部分：编码与比较。单独放，是为了在 Mac 上也能测。

/// 常数时间比较：耗时只取决于长度，不取决于从第几个字节开始不同。
/// 长度不同直接判否 —— 口令的长度是固定的，不是秘密。
pub(crate) fn eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0xf) as usize] as char);
    }
    s
}

pub(crate) fn unhex(s: &str) -> Option<Vec<u8>> {
    fn digit(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let s = s.as_bytes();
    if s.len() % 2 != 0 {
        return None;
    }
    s.chunks_exact(2)
        .map(|p| Some(digit(p[0])? << 4 | digit(p[1])?))
        .collect()
}
