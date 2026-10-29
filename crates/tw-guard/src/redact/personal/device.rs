//! 车辆识别代号、MAC 地址、公网 IP；内网地址规则的 IPv6 那一半（`fc00::/7`）。

use std::net::{Ipv4Addr, Ipv6Addr};

use super::super::rules::{Hit, builtin_hit};
use super::{escape_before, near};

// ---------------------------------------------------------------- VIN

/// VIN 字母换成的数（GB 16735—2019 附录 A，与 ISO 3779 相同）。`I`、`O`、`Q` 不用
fn vin_value(c: u8) -> Option<u32> {
    Some(match c {
        b'0'..=b'9' => u32::from(c - b'0'),
        b'A' | b'J' => 1,
        b'B' | b'K' | b'S' => 2,
        b'C' | b'L' | b'T' => 3,
        b'D' | b'M' | b'U' => 4,
        b'E' | b'N' | b'V' => 5,
        b'F' | b'W' => 6,
        b'G' | b'P' | b'X' => 7,
        b'H' | b'Y' => 8,
        b'R' | b'Z' => 9,
        _ => return None,
    })
}

const VIN_WEIGHTS: [u32; 17] = [8, 7, 6, 5, 4, 3, 2, 10, 0, 9, 8, 7, 6, 5, 4, 3, 2];

/// 车辆识别代号：17 位大写字母数字，不含 `I`、`O`、`Q`；第 9 位是校验位（加权和除以 11 的
/// 余数，10 写成 `X`）。另外要求末四位是数字（年产量大小的厂商都是）、至少有两个字母 ——
/// 17 位的十六进制串（全是数字和 `A`–`F`）十一个里就有一个过得了校验
pub(super) fn vin(b: &[u8]) -> bool {
    if b.len() != 17 || !b[13..].iter().all(u8::is_ascii_digit) {
        return false;
    }
    let mut sum = 0;
    let mut letters = 0;
    for (i, &c) in b.iter().enumerate() {
        let Some(v) = vin_value(c) else {
            return false;
        };
        if c.is_ascii_uppercase() {
            letters += 1;
        }
        sum += v * VIN_WEIGHTS[i];
    }
    let check = match sum % 11 {
        10 => b'X',
        n => b'0' + n as u8,
    };
    letters >= 2 && b[8] == check && !b.iter().all(|c| c.is_ascii_hexdigit())
}

// ---------------------------------------------------------------- MAC 地址

/// 六个字节：全零、广播（全 `FF`）、组播（第一个字节最低位是 1）的不是一台设备
fn mac_bytes(groups: &[&[u8]]) -> bool {
    if groups.len() != 6
        || !groups
            .iter()
            .all(|g| g.len() == 2 && g.iter().all(u8::is_ascii_hexdigit))
    {
        return false;
    }
    let v: Vec<u8> = groups
        .iter()
        .filter_map(|g| u8::from_str_radix(std::str::from_utf8(g).ok()?, 16).ok())
        .collect();
    v.len() == 6 && v.iter().any(|&x| x != 0) && v[0] & 1 == 0
}

/// `00-1A-2B-3C-4D-5E`：连字符分隔，一个 token
pub(super) fn mac_dashed(b: &[u8]) -> bool {
    b.len() == 17 && mac_bytes(&b.split(|&c| c == b'-').collect::<Vec<_>>())
}

/// `00:1a:2b:3c:4d:5e`：冒号不在 token 里，六组是六个 token，在这里接起来。正好六组：
/// 前后还连着冒号的是更长的东西（IPv6、时间）
#[derive(Default)]
pub(super) struct MacRun {
    start: usize,
    end: usize,
    n: usize,
}

impl MacRun {
    pub(super) fn feed(&mut self, text: &str, body: &str, at: usize, out: &mut Vec<Hit>) {
        let b = text.as_bytes();
        let hex2 = body.len() == 2 && body.bytes().all(|c| c.is_ascii_hexdigit());
        if hex2 && self.n > 0 && at == self.end + 1 && b[self.end] == b':' {
            self.n += 1;
            self.end = at + 2;
            return;
        }
        self.finish(text, out);
        if hex2 && !(at > 0 && b[at - 1] == b':') {
            *self = MacRun {
                start: at,
                end: at + 2,
                n: 1,
            };
        }
    }

    pub(super) fn finish(&mut self, text: &str, out: &mut Vec<Hit>) {
        let b = text.as_bytes();
        if self.n == 6 && b.get(self.end) != Some(&b':') {
            let groups: Vec<&[u8]> = b[self.start..self.end].split(|&c| c == b':').collect();
            if mac_bytes(&groups) {
                out.push(builtin_hit("mac-address", self.start..self.end));
            }
        }
        self.n = 0;
    }
}

// ---------------------------------------------------------------- 公网 IP

/// 公共 DNS 之类人人都写的地址、文档里的示意地址：不是谁的位置，换掉只会让模型读不懂配置
const WELL_KNOWN: &[[u8; 4]] = &[
    [8, 8, 8, 8],
    [8, 8, 4, 4],
    [1, 1, 1, 1],
    [1, 0, 0, 1],
    [9, 9, 9, 9],
    [149, 112, 112, 112],
    [208, 67, 222, 222],
    [208, 67, 220, 220],
    [114, 114, 114, 114],
    [114, 114, 115, 115],
    [223, 5, 5, 5],
    [223, 6, 6, 6],
    [119, 29, 29, 29],
    [180, 76, 76, 76],
    // 文档里的示意地址
    [1, 2, 3, 4],
    [4, 3, 2, 1],
    [5, 6, 7, 8],
];

/// 一个 IPv4 地址是不是公网的：不是私有、运营商级 NAT、回环、链路本地、文档示例、基准测试、
/// 组播、保留和「本网络」地址
pub(super) fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (18..=19).contains(&b))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || WELL_KNOWN.contains(&ip.octets()))
}

/// 一个 token 是不是一个四段十进制写的公网 IPv4 地址。段里不能有多余的前导 0；**前面写着
/// 「version」「版本」的是版本号**（`10.2.3.4` 两样都像）
pub(super) fn public_ip_token(text: &str, at: usize, body: &str, json: bool) -> bool {
    let b = body.as_bytes();
    if b.len() < 7
        || b.len() > 15
        || b.iter().filter(|&&c| c == b'.').count() != 3
        || !b.iter().all(|c| c.is_ascii_digit() || *c == b'.')
        || body.split('.').any(|p| p.len() > 1 && p.starts_with('0'))
    {
        return false;
    }
    let Ok(ip) = body.parse::<Ipv4Addr>() else {
        return false;
    };
    // 末段是 0 的是一段网络或者版本号（`2.3.26.0`）；四段都是一位数的多半是章节号、版本号
    // （`OIDC Core 3.1.3.7`）
    let o = ip.octets();
    if o[3] == 0 || o.iter().all(|&x| x < 10) {
        return false;
    }
    public_v4(ip) && !near(text, at, json, VERSION_WORDS)
}

/// 写在这些词后面的四段数是版本号、章节号（`OIDC Core 3.1.3.7`），不是地址
const VERSION_WORDS: &[&str] = &[
    "version", "版本", "ver.", "section", "§", "rfc", "章节", "条款",
];

/// IPv6 地址里能有的字符
fn v6_char(c: u8) -> bool {
    c.is_ascii_hexdigit() || c == b':' || c == b'.'
}

/// IPv6 地址：公网的（`2000::/3`，去掉文档示例 `2001:db8::/32`、`3fff::/20` 和 `2001::/23`
/// 的协议专用段）报 `public-ip`，唯一本地地址（`fc00::/7`）报 `internal-ip`。
///
/// 冒号不在 token 里，按冒号找：每个冒号往两边扩出一段十六进制和冒号，解得开才算。前后紧挨着
/// 字母数字的不算（`std::fmt` 里的 `d::f`）
pub(in crate::redact) fn ipv6(text: &str, public: bool, internal: bool, out: &mut Vec<Hit>) {
    let b = text.as_bytes();
    let mut from = 0;
    while let Some(i) = memchr::memchr(b':', &b[from..]) {
        let p = from + i;
        let mut s = p;
        while s > 0 && v6_char(b[s - 1]) {
            s -= 1;
        }
        let mut e = p;
        while e < b.len() && v6_char(b[e]) {
            e += 1;
        }
        from = e.max(p + 1);
        // 句末的冒号、句号不是地址的一部分；`::` 结尾的是
        while e > s + 2 && matches!(b[e - 1], b'.' | b':') && !b[..e].ends_with(b"::") {
            e -= 1;
        }
        if e - s < 6
            || b[s..e].iter().filter(|&&c| c == b':').count() < 2
            || (s > 0
                && (b[s - 1].is_ascii_alphanumeric() || b[s - 1] == b'_')
                && escape_before(b, s) == 0)
            || b.get(e)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
        {
            continue;
        }
        // `fc00::`、`2002::` 是一段网络的写法，不是一台机器的地址
        if b[..e].ends_with(b"::") {
            continue;
        }
        let Ok(ip) = text[s..e].parse::<Ipv6Addr>() else {
            continue;
        };
        let seg = ip.segments();
        if internal && seg[0] & 0xfe00 == 0xfc00 {
            out.push(builtin_hit("internal-ip", s..e));
        } else if public
            && seg[0] & 0xe000 == 0x2000
            && !(seg[0] == 0x2001 && seg[1] == 0x0db8)
            && !(seg[0] == 0x2001 && seg[1] < 0x0200)
            && !(seg[0] == 0x3fff && seg[1] < 0x1000)
        {
            out.push(builtin_hit("public-ip", s..e));
        }
    }
}
