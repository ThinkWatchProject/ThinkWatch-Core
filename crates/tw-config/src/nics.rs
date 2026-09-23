//! 这台机器上有哪些网卡。
//!
//! **界面上「绑在哪张网卡」那个选择要的就是这份清单。**没有它，用户只
//! 能自己去 `ifconfig` 抄一个地址填进配置文件 —— 而填错的后果是网关起
//! 不来，错误信息只会说 "Can't assign requested address"。
//!
//! 问系统要，不引一个跨平台的封装：unix 上是 `getifaddrs`（`libc` 本来就在
//! 工作区依赖里），Windows 上是 `GetAdaptersAddresses`（`windows-sys` 是一层
//! 纯 Rust 的绑定，不编译任何 C）。两边加起来不到一百五十行，而一个封装
//! crate 要在这两套语义之间做的取舍，正是下面这些注释在说的事。
//!
//! **挑哪些、怎么排，两边共用一套规则**，在这个文件下半部分；平台各自只
//! 负责「把系统里的东西读出来」。

use std::net::{IpAddr, Ipv6Addr};

/// 一张网卡上的一个地址。
///
/// 同一张网卡可以有多个地址（IPv4 一个、IPv6 若干），所以这里一行是
/// 「网卡 + 地址」而不是「网卡」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nic {
    /// unix 上是 `en0`、`lo0`、`utun3`；Windows 上是「以太网」「Wi-Fi」这种
    /// 用户在系统设置里看得见的名字（见 [`windows::adapters`] 里那段注释）。
    pub name: String,
    pub addr: IpAddr,
}

/// 链路本地的 IPv6（`fe80::/10`）。
///
/// 它们要带 scope id 才能用（`fe80::1%en0`），而那个写法放进配置文件里几乎
/// 必然是个坑 —— 所以两个平台都跳过。
fn is_link_local_v6(a: &Ipv6Addr) -> bool {
    a.segments()[0] & 0xffc0 == 0xfe80
}

/// 列出所有已配好地址、并且已经 up 的网卡。
///
/// **跳过没有地址的和没 up 的** —— 一张插着网线但没拿到 IP 的网卡绑不
/// 上去，列在选单里只会让人选了之后发现起不来。
pub fn list() -> Vec<Nic> {
    let mut out = imp::list();
    // 回环排最后：它在选单里对应的是「仅本机」那一档，不该混在
    // 「选一张网卡」的候选里排在前面。
    //
    // **排序在这里，不在各自的平台实现里** —— 它是一条界面上的规矩，
    // 和系统怎么把网卡交给我们没有关系。
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
    imp::exists(name)
}

#[cfg(unix)]
use unix as imp;
#[cfg(windows)]
use windows as imp;

#[cfg(unix)]
mod unix {
    use super::{Nic, is_link_local_v6};
    use std::ffi::CStr;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

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
                        if is_link_local_v6(&a) {
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
        out
    }

    pub fn exists(name: &str) -> bool {
        let Ok(c) = std::ffi::CString::new(name) else {
            return false;
        };
        // SAFETY: 传进去的是一个以 NUL 结尾的字符串，函数只读它
        unsafe { libc::if_nametoindex(c.as_ptr()) != 0 }
    }
}

#[cfg(windows)]
mod windows {
    use super::{Nic, is_link_local_v6};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
        GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6,
    };

    /// `IfOperStatusUp`。**只认「up」这一档**，和 unix 那边的 `IFF_UP` 对齐：
    /// 「正在连」「已断开」的网卡绑不上去。
    const OPER_STATUS_UP: i32 = 1;

    /// 一张网卡上我们关心的东西。
    struct Adapter {
        name: String,
        up: bool,
        addrs: Vec<IpAddr>,
    }

    /// 走一遍系统给的适配器链表。
    ///
    /// **名字取 `FriendlyName`，不取 `AdapterName`。**后者是
    /// `{3F2504E0-4F89-...}` 这样的 GUID —— 配置文件里存的是这个名字，而它
    /// 既要让用户在选单里认得出来（他在系统设置里看到的就是「Wi-Fi」），
    /// 也要在下次启动时还能按它找回同一张网卡。GUID 满足后者但不满足前者，
    /// 而一个用户看不懂的 `bind:` 值等于逼他去别处抄一个地址。
    fn adapters() -> Vec<Adapter> {
        // 先问要多大，再按那个大小要一次。**中间网卡可能变**（插拔、VPN
        // 起落），所以第二次仍然可能说不够 —— 重试几次，而不是一次问不到
        // 就交白卷。
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
        let mut buf: Vec<u64> = Vec::new();
        let mut size: u32 = 0;
        for _ in 0..4 {
            // SAFETY: 第一次传空指针只为问尺寸；之后传的是一块至少 `size`
            // 字节、并且对齐得下 IP_ADAPTER_ADDRESSES_LH 的缓冲区（见 `words`）。
            let rc = unsafe {
                GetAdaptersAddresses(
                    AF_UNSPEC as u32,
                    flags,
                    std::ptr::null_mut(),
                    if buf.is_empty() {
                        std::ptr::null_mut()
                    } else {
                        buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH
                    },
                    &mut size,
                )
            };
            match rc {
                ERROR_SUCCESS if !buf.is_empty() => {
                    // SAFETY: 调用成功，缓冲区里是一条以空指针结尾的链表。
                    return unsafe { walk(buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH) };
                }
                // 第一次（空缓冲区）必然走这里，`size` 现在是要的字节数。
                ERROR_BUFFER_OVERFLOW | ERROR_SUCCESS => {
                    // 多要一点：问到尺寸和真正取数之间网卡还可能变多。
                    buf = vec![0u64; words((size as usize).saturating_add(4096))];
                }
                // 其余错误：给不出清单就给空的，和 unix 那边 getifaddrs
                // 失败时一样 —— 界面上会退到「让用户自己填地址」那一档。
                _ => return Vec::new(),
            }
        }
        Vec::new()
    }

    /// 这么多字节要几个 `u64`。
    ///
    /// **缓冲区是 `Vec<u64>` 而不是 `Vec<u8>`，为的是对齐。**我们要把这块
    /// 内存当成 `IP_ADAPTER_ADDRESSES_LH` 读，而 `Vec<u8>` 只保证 1 字节
    /// 对齐 —— 那是未定义行为，且编译器和测试都不会吭一声，在 x86 上甚至
    /// 多半「能跑」。`u64` 的对齐（8）盖得住那个结构体，下面这行保证这句话
    /// 将来仍然成立。
    const _: () =
        assert!(std::mem::align_of::<IP_ADAPTER_ADDRESSES_LH>() <= std::mem::align_of::<u64>());

    fn words(bytes: usize) -> usize {
        bytes.div_ceil(std::mem::size_of::<u64>())
    }

    /// SAFETY: `head` 要么是空指针，要么指向一条以空指针结尾、由
    /// `GetAdaptersAddresses` 填好的链表，且在这次调用期间一直有效。
    unsafe fn walk(head: *const IP_ADAPTER_ADDRESSES_LH) -> Vec<Adapter> {
        let mut out = Vec::new();
        let mut cur = head;
        while !cur.is_null() {
            let a = unsafe { &*cur };
            cur = a.Next;
            let Some(name) = (unsafe { wide_string(a.FriendlyName) }) else {
                continue;
            };
            let mut addrs = Vec::new();
            let mut ua = a.FirstUnicastAddress;
            while !ua.is_null() {
                let u = unsafe { &*ua };
                ua = u.Next;
                let sa = u.Address.lpSockaddr;
                if sa.is_null() {
                    continue;
                }
                match unsafe { (*sa).sa_family } {
                    AF_INET => {
                        let s = unsafe { &*(sa as *const SOCKADDR_IN) };
                        // `S_un.S_addr` 是网络字节序
                        let bits = unsafe { s.sin_addr.S_un.S_addr };
                        addrs.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(bits))));
                    }
                    AF_INET6 => {
                        let s = unsafe { &*(sa as *const SOCKADDR_IN6) };
                        let v6 = Ipv6Addr::from(unsafe { s.sin6_addr.u.Byte });
                        if is_link_local_v6(&v6) {
                            continue;
                        }
                        addrs.push(IpAddr::V6(v6));
                    }
                    _ => continue,
                }
            }
            out.push(Adapter {
                name,
                up: a.OperStatus as i32 == OPER_STATUS_UP,
                addrs,
            });
        }
        out
    }

    /// Windows 的字符串是 UTF-16，以 NUL 结尾。
    ///
    /// 名字可以是任何语言（中文系统上就是「以太网」），所以**不是 ASCII**，
    /// 而 `from_utf16_lossy` 保证一个畸形的名字变成一个难看的名字，
    /// 不是让整份清单消失。
    ///
    /// SAFETY: `p` 要么是空指针，要么指向一段以 NUL 结尾的 UTF-16。
    unsafe fn wide_string(p: *const u16) -> Option<String> {
        if p.is_null() {
            return None;
        }
        let mut len = 0usize;
        // SAFETY: 调用方保证有 NUL 结尾，所以这个循环会停。
        while unsafe { *p.add(len) } != 0 {
            len += 1;
        }
        if len == 0 {
            return None;
        }
        let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) });
        Some(s)
    }

    pub fn list() -> Vec<Nic> {
        adapters()
            .into_iter()
            .filter(|a| a.up)
            .flat_map(|a| {
                a.addrs.into_iter().map(move |addr| Nic {
                    name: a.name.clone(),
                    addr,
                })
            })
            .collect()
    }

    /// **不看 up、也不看有没有地址** —— 见 [`super::exists`] 上那段。
    pub fn exists(name: &str) -> bool {
        !name.is_empty() && adapters().iter().any(|a| a.name == name)
    }
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
                assert!(!is_link_local_v6(&v6), "漏掉了链路本地地址：{n:?}");
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

    /// 名字里可以有空格和中文 —— Windows 上「以太网」「Wi-Fi 2」都是常见的。
    #[test]
    fn a_name_with_spaces_or_non_ascii_is_still_one_row() {
        let nic = |name: &str, addr: &str| Nic {
            name: name.into(),
            addr: addr.parse().unwrap(),
        };
        let rows = one_per_name(vec![
            nic("以太网", "fd00::1"),
            nic("以太网", "192.168.1.2"),
            nic("Wi-Fi 2", "192.168.1.3"),
        ]);
        assert_eq!(
            rows,
            vec![nic("以太网", "192.168.1.2"), nic("Wi-Fi 2", "192.168.1.3")]
        );
    }

    /// 两个平台共用的那条规矩，单独测一次。
    #[test]
    fn link_local_v6_is_recognised_and_nothing_else_is() {
        let v6 = |s: &str| s.parse::<Ipv6Addr>().unwrap();
        assert!(is_link_local_v6(&v6("fe80::1")));
        assert!(is_link_local_v6(&v6("febf::1")), "fe80::/10 的另一头");
        assert!(!is_link_local_v6(&v6("fec0::1")), "刚好出界");
        assert!(
            !is_link_local_v6(&v6("fd00::1")),
            "唯一本地地址不是链路本地"
        );
        assert!(!is_link_local_v6(&v6("::1")));
        assert!(!is_link_local_v6(&v6("2001:db8::1")));
    }

    #[test]
    fn every_listed_interface_exists_and_a_made_up_one_does_not() {
        for n in by_name() {
            assert!(exists(&n.name), "{n:?}");
        }
        assert!(!exists("tw-no-such0"));
        assert!(!exists(""));
        #[cfg(unix)]
        assert!(!exists("bad\0name"));
    }
}
