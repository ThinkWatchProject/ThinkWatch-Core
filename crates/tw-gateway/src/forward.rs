//! 转发：拿到选中的 provider，把请求原样送出去。
//!
//! **出站直通**（DESIGN.md §4.1）：请求体一个字节都不改。这不只是省事 ——
//! cc-switch 有一次为了兼容性把 `role=system` 消息提到顶层，结果上游的
//! 前缀缓存命中率从 99% 掉到 20%，而且是静默的，只有账单会涨。任何 body
//! 改写都可能是缓存杀手。

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use bytes::Bytes;

use crate::error::GatewayError;

/// 这些头是「我们和客户端之间」的，不该转给上游。
///
/// `x-stainless-timeout` 那条不显然：客户端 SDK 自带的超时头传到上游，
/// 会让上游按客户端的超时提前断流，表现为莫名其妙的截断。
const STRIP: &[&str] = &[
    "host",
    "authorization",
    "x-api-key",
    "x-goog-api-key",
    "content-length",
    "connection",
    "accept-encoding",
    "x-stainless-timeout",
];

fn should_strip(name: &HeaderName) -> bool {
    let n = name.as_str();
    STRIP.contains(&n)
}

/// 按上游协议把凭据放到它认的位置。
pub fn apply_credential(
    builder: reqwest::RequestBuilder,
    protocol: Option<tw_config::Protocol>,
    key: &str,
) -> reqwest::RequestBuilder {
    use tw_config::Protocol::*;
    match protocol {
        // 猜不出协议时按 Anthropic 走：桌面版的主用例是 Claude Code，
        // 而中转站绝大多数说的是 Anthropic 方言。
        Some(Anthropic) | None => builder.header("x-api-key", key),
        Some(Gemini) => builder.header("x-goog-api-key", key),
        Some(OpenaiChat) | Some(OpenaiResponses) => builder.bearer_auth(key),
    }
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
/// 方言互转要用它（§11 的 M6+）：翻译到另一边之后，方言专属的头全是
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
    match query {
        Some(q) if !q.is_empty() => format!("{base}/{path}?{q}"),
        _ => format!("{base}/{path}"),
    }
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

/// 按规则改写请求体。
///
/// **`set` 为空时原样返回，一个字节都不碰**（§4.1）。改写是用户显式要求
/// 的例外，不是默认行为 —— cc-switch 那次把缓存命中率从 99% 打到 20%，
/// 就是因为一个「看起来无害」的重写跑在了每个请求上。
pub fn apply_set(body: &Bytes, set: &tw_engine::SetAction) -> Bytes {
    if set.is_empty() {
        return body.clone();
    }
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(body) else {
        // 解不开就别动。我们的解析器不认识的东西，上游可能完全认识。
        tracing::warn!("请求体不是 JSON，跳过参数改写");
        return body.clone();
    };
    let Some(obj) = v.as_object_mut() else {
        return body.clone();
    };
    if let Some(m) = &set.model {
        obj.insert("model".into(), serde_json::Value::String(m.clone()));
    }
    if let Some(t) = set.max_tokens {
        obj.insert("max_tokens".into(), serde_json::Value::from(t));
    }
    if let Some(th) = set.thinking {
        if th {
            // 开启思考需要一个 budget，而我们没有一个合理的值可以编。
            // **只做「关掉」这一个方向** —— 那是降级场景真正需要的。
            tracing::warn!("set.thinking: true 暂不支持（需要 budget_tokens），已忽略");
        } else {
            obj.remove("thinking");
        }
    }
    match serde_json::to_vec(&v) {
        Ok(b) => Bytes::from(b),
        // 序列化不该失败，但真失败了宁可发原文也不要发半个 body
        Err(e) => {
            tracing::error!("改写后的请求体序列化失败，发原文：{e}");
            body.clone()
        }
    }
}

pub fn map_reqwest_error(e: reqwest::Error) -> GatewayError {
    // 分类要能让人看出该去哪儿修。
    if e.is_timeout() {
        GatewayError::upstream("上游超时")
    } else if e.is_connect() {
        GatewayError::upstream(format!(
            "连不上上游。检查 base_url 和网络，如果配了代理也检查代理：{}",
            tw_secret::redact_url(e.url().map(|u| u.as_str()).unwrap_or(""))
        ))
    } else {
        GatewayError::upstream(format!("转发失败：{e}"))
    }
}

/// 请求体原样转发，只在这里做一次大小检查。
pub fn check_body_size(body: &Bytes, max: usize) -> Result<(), GatewayError> {
    if body.len() > max {
        return Err(GatewayError::request(format!(
            "请求体 {} 字节，超过上限 {} 字节",
            body.len(),
            max
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

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
        // 订阅额度的头必须留着 —— 那是 §4.3.2 白捡的数据来源。
        assert!(out.contains_key("anthropic-ratelimit-unified-5h-utilization"));
    }

    #[test]
    fn an_empty_set_does_not_touch_a_single_byte() {
        // §4.1 的出站直通。cc-switch 那次把缓存命中率从 99% 打到 20%，
        // 就是因为一个「看起来无害」的重写跑在了每个请求上。
        let b = Bytes::from_static(br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#);
        assert_eq!(apply_set(&b, &tw_engine::SetAction::default()), b);
    }

    #[test]
    fn setting_a_model_replaces_only_that_field() {
        let b = Bytes::from_static(br#"{"model":"opus","max_tokens":100,"extra":"keep"}"#);
        let out = apply_set(
            &b,
            &tw_engine::SetAction {
                model: Some("haiku".into()),
                ..Default::default()
            },
        );
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "haiku");
        assert_eq!(v["max_tokens"], 100);
        assert_eq!(v["extra"], "keep", "没被点名的字段不能动");
    }

    #[test]
    fn turning_thinking_off_removes_the_field() {
        let b =
            Bytes::from_static(br#"{"model":"m","thinking":{"type":"enabled","budget_tokens":9}}"#);
        let out = apply_set(
            &b,
            &tw_engine::SetAction {
                thinking: Some(false),
                ..Default::default()
            },
        );
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert!(v.get("thinking").is_none());
    }

    #[test]
    fn a_body_we_cannot_parse_is_forwarded_untouched() {
        // 我们的解析器不认识的东西，上游可能完全认识（§4.1）。
        let b = Bytes::from_static(b"not json");
        let out = apply_set(
            &b,
            &tw_engine::SetAction {
                model: Some("x".into()),
                ..Default::default()
            },
        );
        assert_eq!(out, b);
    }

    #[test]
    fn body_size_limit_reports_both_numbers() {
        let e = check_body_size(&Bytes::from(vec![0u8; 10]), 5).unwrap_err();
        assert!(e.message.contains("10") && e.message.contains('5'));
        assert!(check_body_size(&Bytes::from(vec![0u8; 5]), 5).is_ok());
    }
}
