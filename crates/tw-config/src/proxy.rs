//! 出站代理。
//!
//! **不同上游需要的代理往往不一样**（DESIGN.md §3.7）：官方走代理，
//! 有的必须直连，本地 Ollama 走了代理必然连不上。所以代理不能是一个
//! 全局开关。

use serde::{Deserialize, Serialize};

/// 内置的两个名字，不用声明也不能覆盖。
pub const DIRECT: &str = "direct";
pub const SYSTEM: &str = "system";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProxyKind {
    /// 本地解析 DNS，把 IP 发给代理
    Socks5,
    /// **把域名原样发给代理，由代理端解析。**
    ///
    /// 这个差别在本地 DNS 不可信时是决定性的：解析出来的
    /// IP 根本连不通，即使代理本身是好的。所以它是默认值，UI 的下拉框
    /// 里也排在 `socks5` 前面。
    #[default]
    Socks5h,
    Http,
    Https,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyAuth {
    pub user: String,
    /// 支持 `${ENV}`，和 provider 的 key 同一套
    pub pass: crate::Secret,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proxy {
    pub name: String,
    #[serde(default, rename = "type")]
    pub kind: ProxyKind,
    /// `host:port`
    pub addr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ProxyAuth>,
}

impl Proxy {
    /// 拼成 reqwest 认的 URL。
    pub fn url(&self) -> Result<String, crate::SecretResolveError> {
        let scheme = match self.kind {
            ProxyKind::Socks5 => "socks5",
            ProxyKind::Socks5h => "socks5h",
            ProxyKind::Http => "http",
            ProxyKind::Https => "https",
        };
        Ok(match &self.auth {
            Some(a) => {
                let pass = a.pass.resolve()?;
                // userinfo 里的特殊字符要转义，否则一个带 @ 的密码会把
                // URL 切在错误的地方 —— 而表现是「代理地址不对」。
                format!(
                    "{scheme}://{}:{}@{}",
                    urlencode(&a.user),
                    urlencode(&pass),
                    self.addr
                )
            }
            None => format!("{scheme}://{}", self.addr),
        })
    }
}

/// userinfo 的最小转义。只处理会切断 URL 的那几个字符。
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '.' | '_' | '~' => out.push(c),
            _ => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).as_bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
    }
    out
}

/// 代理挂了怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OnProxyFail {
    /// **默认。** 静默降级会让请求以你意想不到的路径出去 —— 配了代理
    /// 来说直连官方大概率也失败，只是错误信息变得更难懂；更糟的是它真
    /// 的连上了，而你以为自己在走代理。
    #[default]
    Fail,
    Direct,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(kind: ProxyKind, auth: Option<ProxyAuth>) -> Proxy {
        Proxy {
            name: "x".into(),
            kind,
            addr: "127.0.0.1:7890".into(),
            auth,
        }
    }

    #[test]
    fn socks5h_is_the_default_because_of_dns_poisoning() {
        // 本地 DNS 被污染时，socks5 解析出来的 IP 根本连不通，即使代理
        // 本身是好的。这个默认值在那种环境下是决定性的。
        assert_eq!(ProxyKind::default(), ProxyKind::Socks5h);
        assert!(
            p(ProxyKind::default(), None)
                .url()
                .unwrap()
                .starts_with("socks5h://")
        );
    }

    #[test]
    fn each_kind_maps_to_its_scheme() {
        assert!(
            p(ProxyKind::Socks5, None)
                .url()
                .unwrap()
                .starts_with("socks5://")
        );
        assert!(
            p(ProxyKind::Http, None)
                .url()
                .unwrap()
                .starts_with("http://")
        );
        assert!(
            p(ProxyKind::Https, None)
                .url()
                .unwrap()
                .starts_with("https://")
        );
    }

    #[test]
    fn a_password_with_an_at_sign_does_not_cut_the_url_in_the_wrong_place() {
        // 代理密码里有 @ 是很常见的。不转义的话 URL 会被切在密码中间，
        // 而表现是一个费解的「代理地址不对」。
        let auth = ProxyAuth {
            user: "alice".into(),
            pass: crate::Secret::Literal("p@ss:w/rd".into()),
        };
        let url = p(ProxyKind::Http, Some(auth)).url().unwrap();
        assert!(!url.contains("p@ss"), "{url}");
        assert!(url.ends_with("@127.0.0.1:7890"), "{url}");
        assert!(url.contains("%40"), "{url}");
    }

    #[test]
    fn auth_reads_the_password_through_the_secret_machinery() {
        // 代理密码和 provider 的 key 走同一套：明文和 ${ENV}。
        unsafe { std::env::set_var("TW_TEST_PROXY_PASS", "hunter2") };
        let auth = ProxyAuth {
            user: "alice".into(),
            pass: crate::Secret::Literal("${TW_TEST_PROXY_PASS}".into()),
        };
        assert!(
            p(ProxyKind::Socks5h, Some(auth))
                .url()
                .unwrap()
                .contains("hunter2")
        );
    }

    #[test]
    fn failing_is_the_default_not_silently_going_direct() {
        // 静默降级最糟的情况不是失败，是它**真的连上了**，而你以为
        // 自己在走代理。
        assert_eq!(OnProxyFail::default(), OnProxyFail::Fail);
    }
}
