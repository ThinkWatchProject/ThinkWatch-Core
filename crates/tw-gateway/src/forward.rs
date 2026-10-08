//! 转发：拿到选中的 provider，把请求原样送出去。
//!
//! **出站直通**：请求体一个字节都不改。这不只是省事 ——
//! cc-switch 有一次为了兼容性把 `role=system` 消息提到顶层，结果上游的
//! 前缀缓存命中率从 99% 掉到 20%，而且是静默的，只有账单会涨。任何 body
//! 改写都可能是缓存杀手。

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue};
pub use tw_dialect::url::upstream_url;

use crate::error::GatewayError;
use tw_types::msg;

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

/// 按规则改写直通的请求体。转换的请求改在中间表示上（见 `translate::apply_set`）。
///
/// **`set` 为空时原样返回，一个字节都不碰**。改写是用户显式要求
/// 的例外，不是默认行为 —— cc-switch 那次把缓存命中率从 99% 打到 20%，
/// 就是因为一个「看起来无害」的重写跑在了每个请求上。
///
/// 字段名按客户端的格式写（[`tw_dialect::params`]）：同一个「最大输出」在四种格式里叫
/// 四个名字。认不出格式时按 Anthropic 写。Gemini 的模型在路径里，见
/// [`gemini_path_with_model`]。
///
/// **只动改了的那几个字段，别的字节一个不动**（见 [`crate::splice`]）：解析再写回的话，
/// 每个对象的键都按字母重排了，工具定义、工具参数一起变了样。改出来和原来一样的（规则
/// 写的值客户端本来就是这么写的）原样发。改到的对象里同一个键写了两遍的，才照旧写回。
pub fn apply_set(
    body: &Bytes,
    set: &tw_engine::SetAction,
    client: Option<tw_dialect::ir::Dialect>,
) -> Bytes {
    if set.is_empty() {
        return body.clone();
    }
    let Ok(before) = serde_json::from_slice::<serde_json::Value>(body) else {
        // 解不开就别动。我们的解析器不认识的东西，上游可能完全认识。
        tracing::warn!("the request body is not JSON; skipping the parameter rewrites");
        return body.clone();
    };
    if !before.is_object() {
        return body.clone();
    }
    let mut v = before.clone();
    set_params(&mut v, set, client);
    match crate::splice::rewrite(body, &before, &v) {
        crate::splice::Rewrite::Changed(b) => return Bytes::from(b),
        crate::splice::Rewrite::Same => return body.clone(),
        crate::splice::Rewrite::Unusual => {}
    }
    match serde_json::to_vec(&v) {
        Ok(b) => Bytes::from(b),
        // 序列化不该失败，但真失败了宁可发原文也不要发半个 body
        Err(e) => {
            tracing::error!(
                "the rewritten request body could not be serialized; sending the original instead: {e}"
            );
            body.clone()
        }
    }
}

/// 把 `set` 点名的参数写进解析出来的请求体 `v`（见 [`apply_set`]）。
fn set_params(
    v: &mut serde_json::Value,
    set: &tw_engine::SetAction,
    client: Option<tw_dialect::ir::Dialect>,
) {
    use tw_dialect::params;
    let client = client.unwrap_or(tw_dialect::ir::Dialect::Anthropic);
    if let Some(m) = &set.model {
        params::set_model(client, v, m);
    }
    if let Some(t) = set.max_tokens {
        params::set_max_output_tokens(client, v, t);
    }
    match set.thinking {
        // 开启思考需要一个 budget，而我们没有一个合理的值可以编。
        // **只做「关掉」这一个方向** —— 那是降级场景真正需要的。
        Some(true) => tracing::warn!(
            "set.thinking: true is not supported yet (it needs budget_tokens); ignoring it"
        ),
        Some(false) => params::disable_thinking(client, v),
        None => {}
    }
}

pub fn map_reqwest_error(e: reqwest::Error) -> GatewayError {
    // 分类要能让人看出该去哪儿修。
    if e.is_timeout() {
        GatewayError::upstream(msg!(
            "gw.upstream.timeout" => "The upstream did not answer in time."
        ))
    } else if e.is_connect() {
        GatewayError::upstream(msg!(
            "gw.upstream.unreachable",
            url = tw_secret::redact_url(e.url().map(|u| u.as_str()).unwrap_or("")) =>
            "The upstream could not be reached at {url}. Check the endpoint address, the network \
             and the proxy settings."
        ))
    } else {
        GatewayError::upstream(msg!(
            "gw.upstream.forward_failed", detail = e => "Forwarding failed: {detail}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configured_header_replaces_the_one_the_client_sent() {
        let configured = vec![("Anthropic-Version".to_string(), "2023-06-01".to_string())];
        assert!(overridden(&configured, "anthropic-version"));
        assert!(!overridden(&configured, "anthropic-beta"));
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

    fn set(
        model: Option<&str>,
        max_tokens: Option<u64>,
        thinking: Option<bool>,
    ) -> tw_engine::SetAction {
        tw_engine::SetAction {
            model: model.map(str::to_string),
            max_tokens,
            thinking,
            ..Default::default()
        }
    }

    /// 以前的写法：解析、改、整份写回（键按字母重排）
    fn whole(body: &[u8], s: &tw_engine::SetAction, d: Option<tw_dialect::ir::Dialect>) -> Bytes {
        let mut v: serde_json::Value = serde_json::from_slice(body).unwrap();
        set_params(&mut v, s, d);
        Bytes::from(serde_json::to_vec(&v).unwrap())
    }

    /// 规则改了模型、输出上限、关了思考：上游收到的是客户端的那些字节，只有这几个字段变了
    /// —— 工具定义里的键、工具参数里的键、整个请求的先后都照客户端写的
    #[test]
    fn a_rewritten_body_is_the_client_body_but_for_the_fields_set() {
        use tw_dialect::ir::Dialect;
        let tools = r#""tools":[{"name":"Read","description":"读 \"文件\"","input_schema":{"type":"object","properties":{"file_path":{"type":"string"},"offset":{"type":"number"},"limit":{"type":"number"}},"required":["file_path"],"additionalProperties":false,"$schema":"http://json-schema.org/draft-07/schema#"}}]"#;
        let messages = r#""messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Read","input":{"zeta":1,"alpha":{"b":2,"a":3}}}]}]"#;
        let body = format!(
            r#"{{"model":"claude-opus-5",{messages},"system":"s",{tools},"metadata":{{"user_id":"u"}},"max_tokens":32000,"thinking":{{"type":"enabled","budget_tokens":31999}},"stream":true}}"#
        );
        let out = apply_set(
            &Bytes::from(body.clone()),
            &set(Some("claude-haiku-5"), Some(4096), Some(false)),
            Some(Dialect::Anthropic),
        );
        assert_eq!(
            std::str::from_utf8(&out).unwrap(),
            format!(
                r#"{{"model":"claude-haiku-5",{messages},"system":"s",{tools},"metadata":{{"user_id":"u"}},"max_tokens":4096,"stream":true}}"#
            )
        );
        // 客户端没写的输出上限补在最后
        let out = apply_set(
            &Bytes::from(format!(r#"{{"model":"m",{tools}}}"#)),
            &set(None, Some(10), None),
            Some(Dialect::Anthropic),
        );
        assert_eq!(
            std::str::from_utf8(&out).unwrap(),
            format!(r#"{{"model":"m",{tools},"max_tokens":10}}"#)
        );
        // Gemini 的输出上限和思考在 generationConfig 里：只动里面那两个，别的照旧
        let gemini = format!(
            r#"{{"contents":[],{tools},"generationConfig": {{"temperature": 0.2, "thinkingConfig": {{"thinkingBudget": 9}}, "maxOutputTokens": 9, "topK": 3}}}}"#
        );
        let out = apply_set(
            &Bytes::from(gemini),
            &set(Some("ignored"), Some(512), Some(false)),
            Some(Dialect::Gemini),
        );
        assert_eq!(
            std::str::from_utf8(&out).unwrap(),
            format!(
                r#"{{"contents":[],{tools},"generationConfig": {{"temperature": 0.2, "maxOutputTokens": 512, "topK": 3}}}}"#
            )
        );
    }

    /// 改出来和原来一样（规则写的值客户端本来就这么写）：原样发，连拷贝都没有
    #[test]
    fn a_set_that_changes_nothing_sends_the_same_bytes() {
        let b = Bytes::from_static(br#"{ "max_tokens" : 100, "b":{"z":1,"a":2} }"#);
        let out = apply_set(&b, &set(None, Some(100), None), None);
        assert_eq!(out.as_ptr(), b.as_ptr());
    }

    /// 改到的对象里同一个键写了两遍：剪的结果说不准和 serde_json 认的一样（它留后一个），
    /// 照旧整份写回
    #[test]
    fn a_key_written_twice_falls_back_to_writing_the_body_back() {
        let b = br#"{"model":"a","x":1,"model":"b"}"#;
        let s = set(Some("c"), None, None);
        assert_eq!(
            apply_set(&Bytes::from_static(b), &s, None),
            whole(b, &s, None)
        );
    }

    /// 随机拼出来的请求、随机的改写、五种格式：改出来的和整份写回的意思一样；没改到的
    /// 顶层成员原样、按原来的先后都在
    #[test]
    fn rewriting_in_place_means_what_writing_back_meant() {
        use tw_dialect::ir::Dialect;
        struct Rng(u64);
        impl Rng {
            fn below(&mut self, n: usize) -> usize {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                (self.0 % n as u64) as usize
            }
            fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
                xs[self.below(xs.len())]
            }
        }
        let mut r = Rng(0x2545_f491_4f6c_dd1d);
        // 转义的键（`"model"`）由 `char::from(92)` 拼：测试里直接写出来的转义，经过
        // 某些编辑工具会变成真字符
        let escaped_model = format!("\"mod{}u0065l\"", char::from(92));
        let config = [
            r#"{"temperature":0.2,"maxOutputTokens":9,"thinkingConfig":{"thinkingBudget":1}}"#,
            r#"{"max_output_tokens":9,"thinking_config":{"a":1},"top_k":2}"#,
            r#"{ }"#,
            r#"{"thinkingConfig":1,"thinkingConfig":2}"#,
            r#""not an object""#,
        ];
        let members = [
            "\"model\":\"big\"",
            "\"max_tokens\":9",
            "\"max_completion_tokens\":7",
            "\"max_output_tokens\":5",
            "\"thinking\":{\"type\":\"enabled\",\"budget_tokens\":9}",
            "\"reasoning_effort\":\"high\"",
            "\"reasoning\":{\"effort\":\"low\"}",
            "\"inferenceConfig\":{\"temperature\":1,\"maxTokens\":3}",
            "\"additionalModelRequestFields\":{\"thinking\":{\"type\":\"enabled\"},\"k\":1}",
            "\"tools\":[{\"input_schema\":{\"type\":\"object\",\"properties\":{\"b\":{},\"a\":{}}}}]",
            "\"messages\":[{\"role\":\"user\",\"content\":\"max_tokens \\\"model\\\" }{\"}]",
        ];
        const WS: &[&str] = &["", " ", "\n  "];
        let dialects = [
            Dialect::Anthropic,
            Dialect::Chat,
            Dialect::Responses,
            Dialect::Gemini,
            Dialect::Bedrock,
        ];
        let (mut changed, mut unusual) = (0, 0);
        for _ in 0..5_000 {
            let mut top: Vec<String> = Vec::new();
            for _ in 0..r.below(7) {
                let m = match r.below(14) {
                    0 => format!("\"generationConfig\":{}", r.pick(&config)),
                    1 => format!("\"generation_config\":{}", r.pick(&config)),
                    2 => format!("{escaped_model}:\"esc\""),
                    _ => r.pick(&members).to_string(),
                };
                let (a, b) = (r.pick(WS), r.pick(WS));
                top.push(format!("{a}{m}{b}"));
            }
            let body = format!("{{{}}}", top.join(","));
            let s = set(
                [None, Some("small"), Some("big")][r.below(3)],
                [None, Some(9), Some(512)][r.below(3)],
                [None, Some(false), Some(true)][r.below(3)],
            );
            if s.is_empty() {
                continue;
            }
            let d = Some(dialects[r.below(dialects.len())]);
            let out = apply_set(&Bytes::from(body.clone()), &s, d);
            let value = |b: &[u8]| serde_json::from_slice::<serde_json::Value>(b).unwrap();
            assert_eq!(
                value(&out),
                value(&whole(body.as_bytes(), &s, d)),
                "{body} {s:?} {d:?}"
            );
            let before = value(body.as_bytes());
            let mut after = before.clone();
            set_params(&mut after, &s, d);
            if crate::splice::rewrite(body.as_bytes(), &before, &after)
                == crate::splice::Rewrite::Unusual
            {
                unusual += 1;
                continue;
            }
            changed += 1;
            // 没改到的顶层成员：原文那一段照样在，先后不变
            let b = body.as_bytes();
            let mut from = 0;
            for m in crate::splice::object(b, 0).unwrap() {
                let name: String = serde_json::from_slice(&b[m.key.clone()]).unwrap();
                if before.get(&name) != after.get(&name) {
                    continue;
                }
                let piece = &body[m.key.start..m.value.end];
                let at = std::str::from_utf8(&out).unwrap()[from..]
                    .find(piece)
                    .unwrap_or_else(|| {
                        panic!("{piece} moved in {}", String::from_utf8_lossy(&out))
                    });
                from += at + piece.len();
            }
        }
        assert!(changed > 1_000 && unusual > 50, "{changed} {unusual}");
    }
}
