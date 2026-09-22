//! 「这条请求是哪个客户端发的」的**旁证**。
//!
//! 和鉴权那条路（[`crate::auth`]）分得很开，而且必须分得很开：
//!
//! | | 依据 | 能不能伪造 | 用来做什么 |
//! |---|---|---|---|
//! | 身份（`client`） | 网关密钥 | 不能 | 鉴权、路由、配额 |
//! | 旁证（`client_hint`） | 请求头 | **能** | 只用来显示和「接管生效了吗」 |
//!
//! **绝不能拿旁证做鉴权或路由决策**：那等于把 `when: { client: x }`
//! 变成一个任何人发个头就能满足的条件。
//!
//! 那为什么还要它？因为本项目是为「一个 key 就够」的人设计
//! 的 —— 那种用户的所有客户端共用同一把钥匙，于是 `client` 这一列对
//! 五个客户端是同一个值。而接管之后的观察窗口要回答的恰恰是「**Codex**
//! 那边生效了吗」。没有旁证，那个问题就答不了。
//!
//! 下面这些特征都是在本机用一个嗅探器实测出来的，不是从文档抄的。

use axum::http::HeaderMap;

/// 我们接管 Codex 时自己写进它配置里的头（`http_headers`）。实测原样送达。
pub const OURS: &str = "x-thinkwatch-client";

pub fn client_hint(headers: &HeaderMap) -> Option<String> {
    // 一、我们自己写进客户端配置里的。最可靠的一条 —— 但仍然是旁证
    if let Some(v) = headers.get(OURS).and_then(|v| v.to_str().ok())
        && !v.is_empty()
    {
        return Some(v.chars().take(40).collect());
    }
    let get = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("");
    // 二、Codex 自己带的。实测：`originator: codex_exec`
    let originator = get("originator");
    if originator.starts_with("codex") {
        return Some("codex".into());
    }
    // 三、User-Agent。实测：`claude-cli/2.1.195 (external, claude-desktop, …)`
    let ua = get("user-agent");
    for (needle, id) in [
        ("claude-cli", "claude-code"),
        ("codex", "codex"),
        ("opencode", "opencode"),
        ("aider", "aider"),
        ("Zed", "zed"),
        ("cursor", "cursor"),
    ] {
        if ua.contains(needle) {
            return Some(id.into());
        }
    }
    None
}

/// 请求从哪台机器来。**本机（回环）来的不记** —— 那是绝大多数请求，写上
/// 只是噪音；局域网来的才值得说一句「来自 192.168.1.23」。
///
/// 和上面的旁证不是一类东西：它是这条 TCP 连接对面的地址，**不能伪造**。
/// 几台机器共用一把网关密钥时，只有它分得开是哪台发的。
pub fn peer_of(ip: std::net::IpAddr) -> Option<String> {
    let ip = ip.to_canonical();
    (!ip.is_loopback()).then(|| ip.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        m
    }

    #[test]
    fn our_own_header_wins() {
        assert_eq!(
            client_hint(&h(&[(OURS, "codex"), ("user-agent", "claude-cli/1.0")])),
            Some("codex".into())
        );
    }

    #[test]
    fn the_headers_real_clients_actually_send_are_recognised() {
        // 这几条都是本机嗅探器抓到的原样。
        assert_eq!(
            client_hint(&h(&[
                (
                    "user-agent",
                    "claude-cli/2.1.195 (external, claude-desktop, agent-sdk/0.3.0)"
                ),
                ("x-app", "cli"),
            ])),
            Some("claude-code".into())
        );
        assert_eq!(
            client_hint(&h(&[
                ("originator", "codex_exec"),
                ("user-agent", "codex_exec/0.139.0 (Mac OS 26.6.2; arm64)"),
            ])),
            Some("codex".into())
        );
    }

    #[test]
    fn an_unknown_client_is_none_rather_than_a_guess() {
        // 猜错比不猜更糟：观察窗口会因此宣布「已生效」。
        assert_eq!(client_hint(&h(&[("user-agent", "curl/8.7.1")])), None);
        assert_eq!(client_hint(&HeaderMap::new()), None);
    }

    #[test]
    fn a_hostile_value_cannot_grow_without_bound() {
        let long = "x".repeat(10_000);
        let got = client_hint(&h(&[(OURS, &long)])).unwrap();
        assert!(got.chars().count() <= 40, "{}", got.len());
    }

    #[test]
    fn only_a_request_from_another_machine_has_a_peer() {
        let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
        assert_eq!(peer_of(ip("127.0.0.1")), None);
        assert_eq!(peer_of(ip("::1")), None);
        // 双栈监听时本机连过来是 IPv4 映射的 IPv6，也是本机
        assert_eq!(peer_of(ip("::ffff:127.0.0.1")), None);
        assert_eq!(peer_of(ip("192.168.1.23")).as_deref(), Some("192.168.1.23"));
        assert_eq!(
            peer_of(ip("::ffff:192.168.1.23")).as_deref(),
            Some("192.168.1.23")
        );
    }
}
