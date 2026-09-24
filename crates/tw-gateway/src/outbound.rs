//! 出站的 HTTP 客户端：每个上游一个，带着它该走的代理。

use crate::error::GatewayError;
use tw_types::msg;

/// 所有 Client 共享的那部分设置。**只写一遍** —— 分成两处的话，走代理
/// 的那批和不走代理的那批会慢慢长出不同的超时行为。
pub(crate) fn base_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        // 分段超时。**没有整体超时** —— 一个跑了六分钟的
        // Opus 任务不该被中间层掐断，让客户端自己决定何时放弃。
        .connect_timeout(std::time::Duration::from_secs(10))
        // 响应头超时覆盖不到 DNS 和 TCP 握手，所以上面那条必须显式
        // 设置：DNS 被污染解析到黑洞 IP 时，建连会等满内核重传。
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        // HTTP/2 的死连接探测。NAT / 代理会静默丢弃空闲连接，两端都
        // 以为还活着，下一个请求要等满内核重传（分钟级）。挂 Clash /
        // Surge 的桌面用户几乎必踩，而默认是不发 PING 的。
        .http2_keep_alive_interval(std::time::Duration::from_secs(15))
        .http2_keep_alive_timeout(std::time::Duration::from_secs(15))
        .http2_keep_alive_while_idle(true)
        // **不跟重定向。**这些客户端发出去的请求都带着凭据：上游的
        // `x-api-key`、`x-goog-api-key`、配置里写的自定义头、OAuth 的
        // refresh token。reqwest 默认跟到别的主机时只摘 `authorization`
        // 这几个标准头，别的照带，https 跳到 http 也照跟 —— 一个被劫持
        // 或写错的上游一句 302 就能把密钥引到任何地方。3xx 原样交还给
        // 客户端，由它决定
        .redirect(reqwest::redirect::Policy::none())
}

/// 拉公开数据（默认价目表）用的客户端。
///
/// **不带任何凭据**，所以可以跟重定向（托管地址搬家时 GitHub 会回 301）；
/// 但不从 https 降到 http —— 降级之后内容可以被路上任何人换掉。超时和
/// 系统代理同数据面那一套。
pub fn public_client() -> Result<reqwest::Client, GatewayError> {
    base_client_builder()
        .redirect(reqwest::redirect::Policy::custom(|a| {
            let downgrade = a.url().scheme() == "http"
                && a.previous().last().is_some_and(|u| u.scheme() == "https");
            if downgrade {
                a.error("refused to follow a redirect from https to http")
            } else if a.previous().len() >= 10 {
                a.error("too many redirects")
            } else {
                a.follow()
            }
        }))
        .build()
        .map_err(|e| {
            GatewayError::config(msg!(
                "gw.config.http_client", detail = e => "The HTTP client could not be created: {detail}"
            ))
        })
}

/// 给一个 provider 建 Client，带上它该走的代理。
/// 按这家上游的出站设置建一个 HTTP 客户端。
///
/// **公开是给控制面检测一个还没保存的上游用的** —— 检测必须和转发走同一条
/// 出站路径，否则「检测通了、转发不通」会成为可能。
pub fn client_for_provider(
    cfg: &tw_config::Config,
    p: &tw_config::Provider,
) -> Result<reqwest::Client, GatewayError> {
    let mut b = base_client_builder();
    match p.proxy.as_str() {
        tw_config::DIRECT => {
            // 强制直连，忽略一切系统设置 —— 本地 Ollama 走了代理必挂。
            b = b.no_proxy();
        }
        tw_config::SYSTEM => {
            // reqwest 默认就读系统代理，什么都不做即可。
        }
        name => {
            let proxy = cfg.proxies.iter().find(|x| x.name == name).ok_or_else(|| {
                GatewayError::config(msg!(
                    "gw.config.proxy_undefined", upstream = p.name.clone(), proxy = name =>
                    "Upstream `{upstream}` uses proxy `{proxy}`, which is not defined under \
                     `proxies`; the only built-in choices are direct and system."
                ))
            })?;
            let url = proxy.url().map_err(|e| {
                GatewayError::config(msg!(
                    "gw.config.proxy_password", proxy = name, detail = e =>
                    "The password for proxy `{proxy}` could not be read: {detail}"
                ))
            })?;
            match reqwest::Proxy::all(&url) {
                Ok(px) => b = b.proxy(px),
                Err(e) => {
                    // **默认让请求失败，不静默改走直连**。静默降级
                    // 最糟的情况不是失败，是它真的连上了，而你以为自己在
                    // 走代理。
                    if p.on_proxy_fail == tw_config::OnProxyFail::Direct {
                        tracing::warn!(
                            provider = %p.name, proxy = %name,
                            "proxy unusable, going direct as on_proxy_fail says: {e}"
                        );
                        b = b.no_proxy();
                    } else {
                        return Err(GatewayError::config(msg!(
                            "gw.config.proxy_unusable", upstream = p.name.clone(), proxy = name, detail = e =>
                            "Proxy `{proxy}`, used by upstream `{upstream}`, is unusable: {detail}"
                        )));
                    }
                }
            }
        }
    }
    b.build().map_err(|e| {
        GatewayError::config(msg!(
            "gw.config.http_client", detail = e => "The HTTP client could not be created: {detail}"
        ))
    })
}

/// 决定一个 Client 能不能复用的那几个字段。
///
/// base_url 和 key 都**不在**里面：Client 不绑 URL，凭据是每个请求现加
/// 的。把它们算进来只会让「改个 key」白白丢掉一整个连接池。
pub(crate) fn proxy_shape(cfg: &tw_config::Config, p: &tw_config::Provider) -> String {
    let px = cfg
        .proxies
        .iter()
        .find(|x| x.name == p.proxy)
        .map(|x| format!("{:?}|{}|{}", x.kind, x.addr, x.auth.is_some()))
        .unwrap_or_default();
    format!("{}|{:?}|{px}", p.proxy, p.on_proxy_fail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_with_proxy(proxy: &str) -> tw_config::Provider {
        tw_config::Provider {
            name: "p".into(),
            base_url: "https://x.com".into(),
            key: Some("k".into()),
            proxy: proxy.into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_default_proxy_is_direct_not_system() {
        // 显式优于隐式。默认跟随系统的话，用户在系统里开了全局
        // 代理，本地 Ollama 就会莫名连不上 —— 而配置文件里看不出任何线索。
        assert_eq!(tw_config::Provider::default().proxy, tw_config::DIRECT);
    }

    #[test]
    fn the_two_builtin_proxy_names_need_no_declaration() {
        let cfg = tw_config::Config::default();
        assert!(client_for_provider(&cfg, &provider_with_proxy(tw_config::DIRECT)).is_ok());
        assert!(client_for_provider(&cfg, &provider_with_proxy(tw_config::SYSTEM)).is_ok());
    }

    #[test]
    fn an_undeclared_proxy_name_fails_at_startup_and_lists_the_builtins() {
        // 启动时报，不要等请求进来。而且要说清有哪两个内置名字 ——
        // 用户十有八九是想写 `direct`。
        let cfg = tw_config::Config::default();
        let e = client_for_provider(&cfg, &provider_with_proxy("airport")).unwrap_err();
        assert_eq!(e.detail.code, "gw.config.proxy_undefined");
        assert_eq!(e.detail.arg("proxy"), "airport");
        assert!(e.message().contains("direct"), "{}", e.message());
        // 行续接留下的缩进不该进错误信息
        assert!(
            !e.message().contains("   "),
            "错误信息里有多余空格：{}",
            e.message()
        );
    }

    #[test]
    fn a_declared_proxy_builds() {
        let cfg = tw_config::Config {
            proxies: vec![tw_config::Proxy {
                name: "airport".into(),
                kind: tw_config::ProxyKind::Socks5h,
                addr: "127.0.0.1:7890".into(),
                auth: None,
            }],
            ..Default::default()
        };
        assert!(client_for_provider(&cfg, &provider_with_proxy("airport")).is_ok());
    }
}
