//! 和上游之间这条线上的共用件:URL 怎么拼、头怎么传。
//!
//! **出站直通**：请求体一个字节都不改。这不只是省事 ——
//! cc-switch 有一次为了兼容性把 `role=system` 消息提到顶层，结果上游的
//! 前缀缓存命中率从 99% 掉到 20%，而且是静默的，只有账单会涨。任何 body
//! 改写都可能是缓存杀手。
//!
//! 这一层刻意零业务依赖 —— 它不认识配置从哪来、谁在调用，所以企业版和
//! 桌面版都能直接用。**不含 HTTP 客户端的构造**：两边的超时和重定向策略
//! 是相反的（桌面版不设整体超时，因为一个跑六分钟的任务不该被中间层掐断；
//! 企业版设 300 秒并禁止重定向，因为它是多租户而且 base_url 由管理员填），
//! 那是各自的权衡，不是共用件。

#[cfg(feature = "bedrock")]
pub mod eventstream;
#[cfg(feature = "bedrock")]
pub mod sigv4;

use http::{HeaderMap, HeaderName, HeaderValue};

/// 这些头是「我们和客户端之间」的，不该转给上游。
///
/// `x-stainless-timeout` 那条不显然：客户端 SDK 自带的超时头传到上游，
/// 会让上游按客户端的超时提前断流，表现为莫名其妙的截断。
pub const STRIP: &[&str] = &[
    "host",
    "authorization",
    "x-api-key",
    "x-goog-api-key",
    "content-length",
    "connection",
    "accept-encoding",
    "x-stainless-timeout",
];

pub fn should_strip(name: &HeaderName) -> bool {
    let n = name.as_str();
    // `x-thinkwatch-*` 是客户端写给网关的（接管 Codex 时写进去的
    // `X-ThinkWatch-Client`），上游不该看见
    STRIP.contains(&n) || n.starts_with("x-thinkwatch-")
}

/// 把这家上游的请求头放上去。**凭据就在里面**（见 `tw_config::credential`）。
///
/// **WS 升级那条路也走同一份** `Provider::outbound_headers`：各写一份的话，
/// 两条路迟早会在「Gemini 用哪个头」这种事上不一致，而那时只有一条路是对的。
pub fn apply_headers(
    builder: reqwest::RequestBuilder,
    headers: &[(String, String)],
) -> reqwest::RequestBuilder {
    let mut b = builder;
    for (name, value) in headers {
        b = b.header(name.as_str(), value.as_str());
    }
    b
}

/// 客户端带来的这个头，是不是被这家上游配置的同名头盖掉了。
///
/// **盖掉，而不是并存。**配置里写的 `anthropic-version` 和客户端自己带的
/// 那一个同时发出去，上游收到的是两个值 —— 有的取第一个、有的直接 400。
pub fn overridden(headers: &[(String, String)], name: &str) -> bool {
    headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
}

/// 把客户端的请求头搬到上游请求上，剔掉不该走的那些。
pub fn forward_headers(
    builder: reqwest::RequestBuilder,
    incoming: &HeaderMap,
) -> reqwest::RequestBuilder {
    forward_headers_filtered(builder, incoming, |_| true)
}

/// 同上，但再过一道调用方给的筛子。
///
/// 方言互转要用它（M6+）：翻译到另一边之后，方言专属的头全是
/// 噪音，而有些 OpenAI 兼容实现会因为不认识的头直接 400。
pub fn forward_headers_filtered(
    builder: reqwest::RequestBuilder,
    incoming: &HeaderMap,
    keep: impl Fn(&str) -> bool,
) -> reqwest::RequestBuilder {
    let mut b = builder;
    for (name, value) in incoming.iter() {
        if should_strip(name) || !keep(name.as_str()) {
            continue;
        }
        b = b.header(name.clone(), value.clone());
    }
    b
}

/// 拼上游 URL。base_url 的尾斜杠在这里统一吃掉 —— 从浏览器地址栏粘一个
/// URL 就会白送一个尾斜杠，而 `https://host//v1/messages` 换来的是一个
/// 光秃秃的 404。
pub fn upstream_url(base_url: &str, path: &str, query: Option<&str>) -> String {
    let base = base_url.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    let query = query.map(without_gateway_key);
    match query {
        Some(q) if !q.is_empty() => format!("{base}/{path}?{q}"),
        _ => format!("{base}/{path}"),
    }
}

/// 去掉查询串里的 `key=`。
///
/// **那是网关密钥。**Gemini 的 REST 写法把密钥放在查询串里，网关认完身份之后
/// 原样把整个查询串拼到上游地址上，等于把它发给了上游。上游的凭据走请求头，
/// 用不着这一项。
fn without_gateway_key(query: &str) -> String {
    query
        .split('&')
        .filter(|pair| *pair != "key" && !pair.starts_with("key="))
        .collect::<Vec<_>>()
        .join("&")
}

/// 上游响应里也有不该原样回给客户端的头。
pub fn response_headers(upstream: &reqwest::header::HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in upstream.iter() {
        let n = name.as_str();
        // content-length 会和我们重新分块的 body 对不上；
        // transfer-encoding 由 axum 自己决定。
        if matches!(n, "content-length" | "transfer-encoding" | "connection") {
            continue;
        }
        if let (Ok(k), Ok(v)) = (
            HeaderName::from_bytes(name.as_ref()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.insert(k, v);
        }
    }
    out
}

/// Gemini 的模型写在路径里：`/v1beta/models/{model}:动作` 换成另一个模型
pub fn gemini_path_with_model(path: &str, model: &str) -> String {
    let Some((head, rest)) = path.split_once("/models/") else {
        return path.to_string();
    };
    match rest.rsplit_once(':') {
        Some((_, action)) => format!("{head}/models/{model}:{action}"),
        None => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_slash_in_base_url_does_not_double_up() {
        // 从浏览器粘 URL 会白送一个尾斜杠，而 `//v1/messages` 换来 404。
        assert_eq!(
            upstream_url("https://api.example.com/", "/v1/messages", None),
            "https://api.example.com/v1/messages"
        );
        assert_eq!(
            upstream_url("https://api.example.com", "v1/messages", None),
            "https://api.example.com/v1/messages"
        );
    }

    #[test]
    fn query_is_carried_through_but_empty_query_adds_no_question_mark() {
        assert_eq!(
            upstream_url("https://x.com", "/v1/m", Some("beta=true")),
            "https://x.com/v1/m?beta=true"
        );
        assert_eq!(
            upstream_url("https://x.com", "/v1/m", Some("")),
            "https://x.com/v1/m"
        );
    }

    #[test]
    fn the_gateway_key_in_the_query_never_reaches_the_upstream() {
        assert_eq!(
            upstream_url(
                "https://g.example",
                "/v1beta/models/m:generateContent",
                Some("key=tw-secret&alt=sse")
            ),
            "https://g.example/v1beta/models/m:generateContent?alt=sse"
        );
        assert_eq!(
            upstream_url(
                "https://g.example",
                "/v1beta/models/m",
                Some("key=tw-secret")
            ),
            "https://g.example/v1beta/models/m"
        );
        // 名字只是以 key 开头的参数不受影响
        assert_eq!(
            upstream_url("https://g.example", "/x", Some("keyword=a")),
            "https://g.example/x?keyword=a"
        );
    }

    #[test]
    fn headers_meant_for_the_gateway_never_reach_the_upstream() {
        assert!(should_strip(&HeaderName::from_static(
            "x-thinkwatch-client"
        )));
    }

    #[test]
    fn a_configured_header_replaces_the_one_the_client_sent() {
        let configured = vec![("Anthropic-Version".to_string(), "2023-06-01".to_string())];
        assert!(overridden(&configured, "anthropic-version"));
        assert!(!overridden(&configured, "anthropic-beta"));
    }

    #[test]
    fn our_own_auth_headers_never_reach_the_upstream() {
        // 客户端带的是**我们的**网关密钥。原样转出去等于把它送给中转站。
        for h in ["authorization", "x-api-key", "x-goog-api-key"] {
            assert!(should_strip(&HeaderName::from_static(h)), "{h} 应该被剔掉");
        }
    }

    #[test]
    fn client_sdk_timeout_headers_are_stripped() {
        // 传给上游会让它按客户端的超时提前断流，表现为莫名其妙的截断。
        assert!(should_strip(&HeaderName::from_static(
            "x-stainless-timeout"
        )));
    }

    #[test]
    fn dialect_headers_are_kept() {
        // anthropic-version / anthropic-beta 是上游要认的，剔掉就废了。
        for h in ["anthropic-version", "anthropic-beta", "content-type"] {
            assert!(!should_strip(&HeaderName::from_static(h)), "{h} 不该被剔掉");
        }
    }

    #[test]
    fn response_drops_length_and_framing_headers() {
        let mut up = reqwest::header::HeaderMap::new();
        up.insert("content-length", HeaderValue::from_static("123"));
        up.insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );
        up.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            HeaderValue::from_static("0.62"),
        );
        let out = response_headers(&up);
        assert!(!out.contains_key("content-length"));
        assert_eq!(out.get("content-type").unwrap(), "text/event-stream");
        // 订阅额度的头必须留着 —— 那是白捡的数据来源。
        assert!(out.contains_key("anthropic-ratelimit-unified-5h-utilization"));
    }
}
