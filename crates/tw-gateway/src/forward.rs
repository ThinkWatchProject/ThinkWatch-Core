//! 转发：拿到选中的 provider，把请求原样送出去。
//!
//! **出站直通**：请求体一个字节都不改。这不只是省事 ——
//! cc-switch 有一次为了兼容性把 `role=system` 消息提到顶层，结果上游的
//! 前缀缓存命中率从 99% 掉到 20%，而且是静默的，只有账单会涨。任何 body
//! 改写都可能是缓存杀手。

use bytes::Bytes;
pub use tw_upstream::*;

use crate::error::GatewayError;
use tw_types::msg;

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
        tracing::warn!("the request body is not JSON; skipping the parameter rewrites");
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
            Dialect::Bedrock => {
                let c = obj
                    .entry("inferenceConfig")
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(c) = c.as_object_mut() {
                    c.insert("maxTokens".into(), t);
                }
            }
        }
    }
    if let Some(th) = set.thinking {
        if th {
            // 开启思考需要一个 budget，而我们没有一个合理的值可以编。
            // **只做「关掉」这一个方向** —— 那是降级场景真正需要的。
            tracing::warn!(
                "set.thinking: true is not supported yet (it needs budget_tokens); ignoring it"
            );
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
                // Converse 没有一等公民的思考开关，它在透传口袋里
                Dialect::Bedrock => {
                    if let Some(f) = obj
                        .get_mut("additionalModelRequestFields")
                        .and_then(serde_json::Value::as_object_mut)
                    {
                        f.remove("thinking");
                    }
                }
            }
        }
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

/// 请求体原样转发，只在这里做一次大小检查。
pub fn check_body_size(body: &Bytes, max: usize) -> Result<(), GatewayError> {
    if body.len() > max {
        return Err(GatewayError::request(msg!(
            "gw.request.body_too_large", size = body.len(), max = max =>
            "The request body is {size} bytes, over the {max}-byte limit."
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(e.message().contains("10") && e.message().contains('5'));
        assert!(check_body_size(&Bytes::from(vec![0u8; 5]), 5).is_ok());
    }
}
