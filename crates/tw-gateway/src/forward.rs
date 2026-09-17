//! 转发：拿到选中的 provider，把请求原样送出去。
//!
//! **出站直通**：请求体一个字节都不改。这不只是省事 ——
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

/// 按规则改写直通的请求体。转换的请求改在中间表示上（见 `translate::apply_set`）。
///
/// **`set` 为空时原样返回，一个字节都不碰**。改写是用户显式要求
/// 的例外，不是默认行为 —— cc-switch 那次把缓存命中率从 99% 打到 20%，
/// 就是因为一个「看起来无害」的重写跑在了每个请求上。
///
/// 字段名按客户端的格式写：同一个「最大输出」在四种格式里叫四个名字。认不出格式时
/// 按 Anthropic 写。Gemini 的模型在路径里，见 [`gemini_path_with_model`]。
pub fn apply_set(
    body: &Bytes,
    set: &tw_engine::SetAction,
    client: Option<tw_dialect::ir::Dialect>,
) -> Bytes {
    use tw_dialect::ir::Dialect;
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
    let client = client.unwrap_or(Dialect::Anthropic);
    if let Some(m) = &set.model
        && client != Dialect::Gemini
    {
        obj.insert("model".into(), serde_json::Value::String(m.clone()));
    }
    if let Some(t) = set.max_tokens {
        let t = serde_json::Value::from(t);
        match client {
            Dialect::Anthropic => {
                obj.insert("max_tokens".into(), t);
            }
            Dialect::Chat => {
                let key = if obj.contains_key("max_completion_tokens") {
                    "max_completion_tokens"
                } else {
                    "max_tokens"
                };
                obj.insert(key.into(), t);
            }
            Dialect::Responses => {
                obj.insert("max_output_tokens".into(), t);
            }
            Dialect::Gemini => {
                let g = obj
                    .entry("generationConfig")
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(g) = g.as_object_mut() {
                    g.insert("maxOutputTokens".into(), t);
                }
            }
        }
    }
    if let Some(th) = set.thinking {
        if th {
            // 开启思考需要一个 budget，而我们没有一个合理的值可以编。
            // **只做「关掉」这一个方向** —— 那是降级场景真正需要的。
            tracing::warn!("set.thinking: true 暂不支持（需要 budget_tokens），已忽略");
        } else {
            match client {
                Dialect::Anthropic => {
                    obj.remove("thinking");
                }
                Dialect::Chat => {
                    obj.remove("reasoning_effort");
                }
                Dialect::Responses => {
                    obj.remove("reasoning");
                }
                Dialect::Gemini => {
                    if let Some(g) = obj
                        .get_mut("generationConfig")
                        .and_then(|g| g.as_object_mut())
                    {
                        g.remove("thinkingConfig");
                    }
                }
            }
        }
    }
    match serde_json::to_vec(&v) {
        Ok(b) => Bytes::from(b),
        // 序列化不该失败，但真失败了宁可发原文也不要发半个 body
        Err(e) => {
            tracing::error!("改写后的请求体序列化失败，改为发送原始请求体：{e}");
            body.clone()
        }
    }
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

pub fn map_reqwest_error(e: reqwest::Error) -> GatewayError {
    // 分类要能让人看出该去哪儿修。
    if e.is_timeout() {
        GatewayError::upstream("上游响应超时")
    } else if e.is_connect() {
        GatewayError::upstream(format!(
            "无法连接上游，请检查接口地址、网络和代理设置：{}",
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
            "请求体大小为 {} 字节，超过上限 {} 字节",
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

    #[test]
    fn an_empty_set_does_not_touch_a_single_byte() {
        // 出站直通。cc-switch 那次把缓存命中率从 99% 打到 20%，
        // 就是因为一个「看起来无害」的重写跑在了每个请求上。
        let b = Bytes::from_static(br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#);
        assert_eq!(apply_set(&b, &tw_engine::SetAction::default(), None), b);
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
            None,
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
            None,
        );
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert!(v.get("thinking").is_none());
    }

    #[test]
    fn a_body_we_cannot_parse_is_forwarded_untouched() {
        // 我们的解析器不认识的东西，上游可能完全认识。
        let b = Bytes::from_static(b"not json");
        let out = apply_set(
            &b,
            &tw_engine::SetAction {
                model: Some("x".into()),
                ..Default::default()
            },
            None,
        );
        assert_eq!(out, b);
    }

    #[test]
    fn a_set_writes_each_format_under_its_own_field_names() {
        use tw_dialect::ir::Dialect;
        let set = tw_engine::SetAction {
            model: Some("small".into()),
            max_tokens: Some(512),
            thinking: Some(false),
            ..Default::default()
        };
        let run = |body: &'static str, d| {
            serde_json::from_slice::<serde_json::Value>(&apply_set(
                &Bytes::from_static(body.as_bytes()),
                &set,
                Some(d),
            ))
            .unwrap()
        };
        let v = run(
            r#"{"model":"big","max_completion_tokens":9,"reasoning_effort":"high"}"#,
            Dialect::Chat,
        );
        assert_eq!(
            (v["model"].as_str(), v["max_completion_tokens"].as_u64()),
            (Some("small"), Some(512))
        );
        assert!(v.get("reasoning_effort").is_none() && v.get("max_tokens").is_none());
        let v = run(
            r#"{"model":"big","reasoning":{"effort":"high"}}"#,
            Dialect::Responses,
        );
        assert_eq!(v["max_output_tokens"], 512);
        assert!(v.get("reasoning").is_none());
        let v = run(
            r#"{"contents":[],"generationConfig":{"thinkingConfig":{"thinkingBudget":9}}}"#,
            Dialect::Gemini,
        );
        assert_eq!(
            v["generationConfig"],
            serde_json::json!({"maxOutputTokens": 512})
        );
        assert!(v.get("model").is_none(), "Gemini 的模型在路径里");
        assert_eq!(
            gemini_path_with_model("/v1beta/models/big:streamGenerateContent", "small"),
            "/v1beta/models/small:streamGenerateContent"
        );
    }

    #[test]
    fn body_size_limit_reports_both_numbers() {
        let e = check_body_size(&Bytes::from(vec![0u8; 10]), 5).unwrap_err();
        assert!(e.message.contains("10") && e.message.contains('5'));
        assert!(check_body_size(&Bytes::from(vec![0u8; 5]), 5).is_ok());
    }
}
