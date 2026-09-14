//! 谁能连这个网关。
//!
//! 前面的 auth 管「这把钥匙是谁」，这一层管「这个地址能不能连过来」。
//! 两件事分开是因为它们的失败含义不同：钥匙不对是
//! 配置问题，地址不对是安全边界问题。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub use tw_types::PRIVATE_RANGES;

/// 一条 CIDR。自己解析而不是引一个 crate —— 这里只需要「一个 IP 在不在
/// 这个段里」，而多一个依赖就多一份要跟着升级的东西。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum CidrError {
    #[error("`{0}` 不是合法的 CIDR。写法是 `192.168.0.0/16` 这样。")]
    Malformed(String),
    #[error("`{cidr}` 的前缀长度 {prefix} 超出范围（IPv4 最大 32，IPv6 最大 128）")]
    BadPrefix { cidr: String, prefix: u8 },
}

impl std::str::FromStr for Cidr {
    type Err = CidrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        // 不带 `/` 的当成单个地址。用户写 `192.168.1.5` 想表达「就这一台」
        // 是很自然的，报错反而显得刻板。
        let (ip_part, prefix_part) = match s.split_once('/') {
            Some((a, b)) => (a, Some(b)),
            None => (s, None),
        };
        let addr: IpAddr = ip_part
            .parse()
            .map_err(|_| CidrError::Malformed(s.to_string()))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix_part {
            Some(p) => p
                .parse::<u8>()
                .map_err(|_| CidrError::Malformed(s.to_string()))?,
            None => max,
        };
        if prefix > max {
            return Err(CidrError::BadPrefix {
                cidr: s.to_string(),
                prefix,
            });
        }
        Ok(Cidr { addr, prefix })
    }
}

impl Cidr {
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(x)) => {
                masked_v4(net, self.prefix) == masked_v4(x, self.prefix)
            }
            (IpAddr::V6(net), IpAddr::V6(x)) => {
                masked_v6(net, self.prefix) == masked_v6(x, self.prefix)
            }
            // **IPv4 映射的 IPv6 要能匹配 IPv4 段。**macOS 上一个绑
            // `0.0.0.0` 的监听器收到的对端地址可能是 `::ffff:192.168.1.5`，
            // 而用户写的白名单是 `192.168.0.0/16` —— 不处理的话白名单
            // 会莫名其妙地全部不命中。
            (IpAddr::V4(_), IpAddr::V6(x)) => match x.to_ipv4_mapped() {
                Some(v4) => self.contains(IpAddr::V4(v4)),
                None => false,
            },
            (IpAddr::V6(_), IpAddr::V4(_)) => false,
        }
    }
}

fn masked_v4(ip: Ipv4Addr, prefix: u8) -> u32 {
    let bits = u32::from(ip);
    if prefix == 0 {
        0
    } else {
        bits & (!0u32 << (32 - prefix))
    }
}

fn masked_v6(ip: Ipv6Addr, prefix: u8) -> u128 {
    let bits = u128::from(ip);
    if prefix == 0 {
        0
    } else {
        bits & (!0u128 << (128 - prefix))
    }
}

/// 来源白名单。
#[derive(Debug, Clone, Default)]
pub struct AllowList {
    ranges: Vec<Cidr>,
}

impl AllowList {
    /// 空列表 = 全放行。
    ///
    /// 这看起来危险，但它只在 `bind: loopback` 下成立 —— 那时候能连过来
    /// 的本来就只有本机。非 loopback 的默认值由配置层填成私网段。
    pub fn parse(entries: &[String]) -> Result<Self, CidrError> {
        Ok(Self {
            ranges: entries
                .iter()
                .map(|s| s.parse())
                .collect::<Result<_, _>>()?,
        })
    }

    pub fn private_default() -> Self {
        Self::parse(
            &PRIVATE_RANGES
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        )
        .expect("内置的私网段一定是合法 CIDR")
    }

    pub fn allows(&self, ip: IpAddr) -> bool {
        self.ranges.is_empty() || self.ranges.iter().any(|c| c.contains(ip))
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }
    fn cidr(s: &str) -> Cidr {
        s.parse().unwrap()
    }

    #[test]
    fn a_cidr_contains_what_it_should() {
        assert!(cidr("192.168.0.0/16").contains(ip("192.168.1.5")));
        assert!(!cidr("192.168.0.0/16").contains(ip("10.0.0.1")));
        assert!(cidr("10.0.0.0/8").contains(ip("10.255.255.255")));
        assert!(cidr("0.0.0.0/0").contains(ip("8.8.8.8")));
    }

    #[test]
    fn the_tricky_boundary_of_172_16_over_12() {
        // 这个段最容易写错：它是 172.16.0.0 到 172.31.255.255，
        // 不是整个 172.*。
        let c = cidr("172.16.0.0/12");
        assert!(c.contains(ip("172.16.0.1")));
        assert!(c.contains(ip("172.31.255.254")));
        assert!(!c.contains(ip("172.32.0.1")));
        assert!(!c.contains(ip("172.15.0.1")));
    }

    #[test]
    fn a_bare_address_means_just_that_host() {
        // 用户写 `192.168.1.5` 想表达「就这一台」是很自然的。
        let c = cidr("192.168.1.5");
        assert!(c.contains(ip("192.168.1.5")));
        assert!(!c.contains(ip("192.168.1.6")));
    }

    #[test]
    fn an_ipv4_mapped_ipv6_peer_still_matches_an_ipv4_range() {
        // macOS 上一个绑 0.0.0.0 的监听器收到的对端可能是
        // ::ffff:192.168.1.5，而用户写的是 192.168.0.0/16。不处理的话
        // 白名单会莫名其妙全部不命中 —— 而那看起来像「白名单坏了」。
        assert!(cidr("192.168.0.0/16").contains(ip("::ffff:192.168.1.5")));
        assert!(!cidr("10.0.0.0/8").contains(ip("::ffff:192.168.1.5")));
    }

    #[test]
    fn nonsense_is_refused_with_the_right_shape_in_the_message() {
        assert!(matches!(
            "not-an-ip".parse::<Cidr>(),
            Err(CidrError::Malformed(_))
        ));
        assert!(matches!(
            "192.168.0.0/99".parse::<Cidr>(),
            Err(CidrError::BadPrefix { .. })
        ));
        let e = "1.2.3".parse::<Cidr>().unwrap_err().to_string();
        assert!(e.contains("192.168.0.0/16"), "错误要给出正确写法：{e}");
    }

    #[test]
    fn an_empty_allow_list_permits_everything() {
        // 只在 loopback 下成立 —— 那时能连过来的本来就只有本机。
        let a = AllowList::default();
        assert!(a.allows(ip("8.8.8.8")));
        assert!(a.is_empty());
    }

    #[test]
    fn the_private_default_covers_the_three_rfc1918_ranges_and_loopback() {
        let a = AllowList::private_default();
        for good in [
            "127.0.0.1",
            "10.1.2.3",
            "172.20.0.5",
            "192.168.1.100",
            "::1",
        ] {
            assert!(a.allows(ip(good)), "{good} 该被放行");
        }
        for bad in ["8.8.8.8", "1.1.1.1", "203.0.113.5"] {
            assert!(!a.allows(ip(bad)), "{bad} 不该被放行");
        }
    }

    #[test]
    fn a_list_with_entries_only_permits_those() {
        let a = AllowList::parse(&["192.168.1.0/24".to_string()]).unwrap();
        assert!(a.allows(ip("192.168.1.50")));
        assert!(!a.allows(ip("192.168.2.50")));
        assert!(!a.allows(ip("127.0.0.1")), "写了白名单就只认白名单");
    }
}
