//! 这台机器上有哪些网卡。
//!
//! **界面上「绑在哪张网卡」那个选择要的就是这份清单。**没有它，用户只
//! 能自己去 `ifconfig` 抄一个地址填进配置文件 —— 而填错的后果是网关起
//! 不来，错误信息只会说 "Can't assign requested address"。
//!
//! 用 `getifaddrs` 直接问系统，不引第三方 crate：`libc` 本来就在工作区
//! 依赖里（tw-store 在用），而这件事一共不到五十行。

use std::ffi::CStr;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// 一张网卡上的一个地址。
///
/// 同一张网卡可以有多个地址（IPv4 一个、IPv6 若干），所以这里一行是
/// 「网卡 + 地址」而不是「网卡」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nic {
    /// `en0`、`lo0`、`utun3`
    pub name: String,
    pub addr: IpAddr,
}

/// 列出所有已配好地址、并且已经 up 的网卡。
///
/// **跳过没有地址的和没 up 的** —— 一张插着网线但没拿到 IP 的网卡绑不
/// 上去，列在选单里只会让人选了之后发现起不来。
///
/// 也跳过 IPv6 的链路本地地址（`fe80::/10`）：它们要带 scope id 才能用
/// （`fe80::1%en0`），而那个写法放进配置文件里几乎必然是个坑。
pub fn list() -> Vec<Nic> {
    let mut out = Vec::new();
    // SAFETY: getifaddrs 成功时给出一条以空指针结尾的链表，freeifaddrs
    // 负责释放。中间只读不写，且在 free 之前把需要的字段都拷出来。
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut cur = head;
        while !cur.is_null() {
            let e = &*cur;
            cur = e.ifa_next;
            if e.ifa_addr.is_null() || e.ifa_flags & libc::IFF_UP as u32 == 0 {
                continue;
            }
            let name = match CStr::from_ptr(e.ifa_name).to_str() {
                Ok(n) => n.to_string(),
                Err(_) => continue,
            };
            let addr = match (*e.ifa_addr).sa_family as i32 {
                libc::AF_INET => {
                    let s = &*(e.ifa_addr as *const libc::sockaddr_in);
                    IpAddr::V4(Ipv4Addr::from(u32::from_be(s.sin_addr.s_addr)))
                }
                libc::AF_INET6 => {
                    let s = &*(e.ifa_addr as *const libc::sockaddr_in6);
                    let a = Ipv6Addr::from(s.sin6_addr.s6_addr);
                    if a.segments()[0] & 0xffc0 == 0xfe80 {
                        continue;
                    }
                    IpAddr::V6(a)
                }
                _ => continue,
            };
            out.push(Nic { name, addr });
        }
        libc::freeifaddrs(head);
    }
    // 回环排最后：它在选单里对应的是「仅本机」那一档，不该混在
    // 「选一张网卡」的候选里排在前面。
    out.sort_by_key(|n| (n.addr.is_loopback(), n.name.clone(), n.addr.to_string()));
    out
}

/// 每张网卡一行，地址是按名字绑它时真正监听的那一个。
///
/// **同一张网卡只出现一次。**配置里按名字存（`bind: en0`），一张网卡的
/// IPv4 和 IPv6 各列一行的话，两行存下去都是 `en0`，选单上选第二行等于
/// 选第一行。
///
/// **有 IPv4 就用 IPv4。**客户端配置里写的是 `http://<地址>:端口`，而一个
/// IPv6 地址在那个位置要加方括号，多数客户端的输入框对此毫无准备。
pub fn by_name() -> Vec<Nic> {
    one_per_name(list())
}

fn one_per_name(all: Vec<Nic>) -> Vec<Nic> {
    let mut out: Vec<Nic> = Vec::new();
    for n in all {
        match out.iter_mut().find(|x| x.name == n.name) {
            None => out.push(n),
            Some(x) if x.addr.is_ipv6() && n.addr.is_ipv4() => *x = n,
            Some(_) => {}
        }
    }
    out
}

/// 系统里有没有叫这个名字的网卡，**有没有地址都算**。
///
/// [`list`] 只列有地址的：网线拔了、Wi-Fi 断了的网卡不在里面。「没有这张
/// 网卡」和「这张网卡此刻没有地址」要分开说 —— 前者多半是拼错了，后者
/// 插上网线就好。
pub fn exists(name: &str) -> bool {
    let Ok(c) = std::ffi::CString::new(name) else {
        return false;
    };
    // SAFETY: 传进去的是一个以 NUL 结尾的字符串，函数只读它
    unsafe { libc::if_nametoindex(c.as_ptr()) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 这个测试不断言具体有哪些网卡 —— 那取决于跑它的机器。
    /// 它断言的是**任何一台机器都成立的事**。
    #[test]
    fn every_machine_has_a_loopback_and_no_entry_is_malformed() {
        let all = list();
        assert!(
            all.iter().any(|n| n.addr.is_loopback()),
            "一台机器不可能没有回环地址：{all:?}"
        );
        for n in &all {
            assert!(!n.name.is_empty(), "网卡名是空的：{n:?}");
            // 链路本地的 IPv6 要带 scope id 才能用，不该出现在这里
            if let IpAddr::V6(v6) = n.addr {
                assert_ne!(
                    v6.segments()[0] & 0xffc0,
                    0xfe80,
                    "漏掉了链路本地地址：{n:?}"
                );
            }
        }
    }

    #[test]
    fn loopback_sorts_last_so_it_does_not_head_the_picker() {
        let all = list();
        let first_loopback = all.iter().position(|n| n.addr.is_loopback());
        let last_real = all.iter().rposition(|n| !n.addr.is_loopback());
        if let (Some(lb), Some(real)) = (first_loopback, last_real) {
            assert!(lb > real, "回环排在真实网卡前面了：{all:?}");
        }
    }

    #[test]
    fn one_row_per_interface_with_the_ipv4_address_when_there_is_one() {
        let nic = |name: &str, addr: &str| Nic {
            name: name.into(),
            addr: addr.parse().unwrap(),
        };
        let rows = one_per_name(vec![
            nic("en0", "fd94:db59:5dfe:4525::1"),
            nic("en0", "10.0.3.7"),
            nic("en0", "10.0.3.8"),
            nic("utun3", "fd00::2"),
        ]);
        assert_eq!(
            rows,
            vec![nic("en0", "10.0.3.7"), nic("utun3", "fd00::2")],
            "同一张网卡列了两行，或者没挑 IPv4"
        );
    }

    #[test]
    fn every_listed_interface_exists_and_a_made_up_one_does_not() {
        for n in by_name() {
            assert!(exists(&n.name), "{n:?}");
        }
        assert!(!exists("tw-no-such0"));
        assert!(!exists("bad\0name"));
    }
}
