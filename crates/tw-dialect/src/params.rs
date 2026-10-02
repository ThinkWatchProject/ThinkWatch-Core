//! 请求参数的读写：同一个参数在各格式里叫不同的名字、放在不同的地方。
//!
//! 「最大输出」在 Anthropic 是 `max_tokens`，Chat 是 `max_completion_tokens` 或
//! `max_tokens`，Responses 是 `max_output_tokens`，Gemini 在 `generationConfig` 里叫
//! `maxOutputTokens`，Bedrock 在 `inferenceConfig` 里叫 `maxTokens`。改写请求参数的
//! 地方（桌面版路由规则的 `set`、企业版按模型限制最大输出）都从这里读写，名字只在
//! 这里写一遍。读的判据和解码器一致（同一个请求，这里读出来的数和中间表示里的
//! `max_tokens` 一样）。
//!
//! **改的是原文**（[`serde_json::Value`]），不经过中间表示：同格式直通的请求要保住中间
//! 表示不装的那些字段。没写这个参数、要写进去时，写在这种格式通常写的位置上。请求体
//! 不是 JSON 对象的，读是 `None`，写什么都不做。

use serde_json::{Map, Value};

use crate::ir::Dialect;

/// 客户端写的最大输出 token 数。没写（或者写的不是一个非负整数）是 `None`。
pub fn max_output_tokens(dialect: Dialect, body: &Value) -> Option<u64> {
    let u = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64);
    match dialect {
        Dialect::Anthropic => u(body, "max_tokens"),
        // 两个都写了的，解码器先认 max_completion_tokens
        Dialect::Chat => u(body, "max_completion_tokens").or_else(|| u(body, "max_tokens")),
        Dialect::Responses => u(body, "max_output_tokens"),
        // 和解码器一样：驼峰的在就认驼峰的，否则认下划线的
        Dialect::Gemini => either(
            either(body, "generationConfig", "generation_config")?,
            "maxOutputTokens",
            "max_output_tokens",
        )?
        .as_u64(),
        Dialect::Bedrock => body.get("inferenceConfig").and_then(|c| u(c, "maxTokens")),
    }
}

/// 把最大输出 token 数写成 `n`。
///
/// Chat 两个名字都可能写：客户端写了哪个（或者两个都写了）就改哪个，都没写时写
/// `max_tokens` —— OpenAI 官方的推理模型只认 `max_completion_tokens`，但它们的客户端
/// 本来就那么写；兼容实现大多只认 `max_tokens`。Gemini 的 `generationConfig` 客户端写成
/// 下划线（`generation_config`）的，写进它里面，不另起一个驼峰的。
pub fn set_max_output_tokens(dialect: Dialect, body: &mut Value, n: u64) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let n = Value::from(n);
    match dialect {
        Dialect::Anthropic => {
            obj.insert("max_tokens".into(), n);
        }
        Dialect::Chat => {
            let written: Vec<&str> = ["max_completion_tokens", "max_tokens"]
                .into_iter()
                .filter(|k| obj.contains_key(*k))
                .collect();
            if written.is_empty() {
                obj.insert("max_tokens".into(), n);
            } else {
                for k in written {
                    obj.insert(k.into(), n.clone());
                }
            }
        }
        Dialect::Responses => {
            obj.insert("max_output_tokens".into(), n);
        }
        Dialect::Gemini => {
            // 已有的生成参数里写，客户端写过哪个名字改哪个；没有就新建一个驼峰的
            let config = gemini_config_key(obj).unwrap_or("generationConfig");
            if let Some(g) = section(obj, config) {
                let field = gemini_max_key(g).unwrap_or(if config == "generation_config" {
                    "max_output_tokens"
                } else {
                    "maxOutputTokens"
                });
                g.insert(field.into(), n);
            }
        }
        Dialect::Bedrock => {
            if let Some(c) = section(obj, "inferenceConfig") {
                c.insert("maxTokens".into(), n);
            }
        }
    }
}

/// 把最大输出 token 数限制在 `cap` 以内：客户端写的比它大、或者没写，就写成 `cap`；
/// 写的不比它大就不动。返回改没改。
pub fn cap_max_output_tokens(dialect: Dialect, body: &mut Value, cap: u64) -> bool {
    if !body.is_object() || max_output_tokens(dialect, body).is_some_and(|n| n <= cap) {
        return false;
    }
    set_max_output_tokens(dialect, body, cap);
    true
}

/// 改要的模型。Gemini 和 Bedrock 的模型写在路径里，请求体里没有它，什么都不做（路径
/// 由调用方改）。
pub fn set_model(dialect: Dialect, body: &mut Value, model: &str) {
    if matches!(dialect, Dialect::Gemini | Dialect::Bedrock) {
        return;
    }
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".into(), Value::String(model.to_string()));
    }
}

/// 关掉推理：去掉开推理的那个字段。
///
/// **只做「关掉」这一个方向**：开启要一个预算，而这一层没有一个说得过去的值可以编。
pub fn disable_thinking(dialect: Dialect, body: &mut Value) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    match dialect {
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
            for config in ["generationConfig", "generation_config"] {
                if let Some(g) = obj.get_mut(config).and_then(Value::as_object_mut) {
                    g.remove("thinkingConfig");
                    g.remove("thinking_config");
                }
            }
        }
        // Converse 没有一等公民的思考开关，它在透传口袋里
        Dialect::Bedrock => {
            if let Some(f) = obj
                .get_mut("additionalModelRequestFields")
                .and_then(Value::as_object_mut)
            {
                f.remove("thinking");
            }
        }
    }
}

/// Gemini 的一个字段：驼峰的在就是它，否则是下划线写法的
fn either<'a>(v: &'a Value, camel: &str, snake: &str) -> Option<&'a Value> {
    v.get(camel).or_else(|| v.get(snake))
}

/// Gemini 的生成参数写在哪个键下：客户端写了哪个用哪个，先认驼峰（和解码器一样）
fn gemini_config_key(body: &Map<String, Value>) -> Option<&'static str> {
    ["generationConfig", "generation_config"]
        .into_iter()
        .find(|k| body.get(*k).is_some_and(Value::is_object))
}

/// 生成参数里最大输出写在哪个键下，先认驼峰
fn gemini_max_key(g: &Map<String, Value>) -> Option<&'static str> {
    ["maxOutputTokens", "max_output_tokens"]
        .into_iter()
        .find(|k| g.contains_key(*k))
}

/// `obj` 里名叫 `key` 的那个对象，没有就新建一个空的。那个键上写的不是对象的，`None`
fn section<'a>(obj: &'a mut Map<String, Value>, key: &str) -> Option<&'a mut Map<String, Value>> {
    obj.entry(key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ALL: [Dialect; 5] = [
        Dialect::Anthropic,
        Dialect::Chat,
        Dialect::Responses,
        Dialect::Gemini,
        Dialect::Bedrock,
    ];

    #[test]
    fn what_is_written_is_what_is_read_back_in_every_format() {
        for d in ALL {
            let mut v = json!({});
            assert_eq!(max_output_tokens(d, &v), None, "{d:?}");
            set_max_output_tokens(d, &mut v, 512);
            assert_eq!(max_output_tokens(d, &v), Some(512), "{d:?}: {v}");
        }
    }

    #[test]
    fn each_format_keeps_its_own_field_names() {
        let set = |d, mut v: Value| {
            set_max_output_tokens(d, &mut v, 7);
            v
        };
        assert_eq!(set(Dialect::Anthropic, json!({})), json!({"max_tokens": 7}));
        assert_eq!(set(Dialect::Chat, json!({})), json!({"max_tokens": 7}));
        assert_eq!(
            set(Dialect::Chat, json!({"max_completion_tokens": 9})),
            json!({"max_completion_tokens": 7})
        );
        assert_eq!(
            set(
                Dialect::Chat,
                json!({"max_completion_tokens": 9, "max_tokens": 9})
            ),
            json!({"max_completion_tokens": 7, "max_tokens": 7}),
            "两个都写了的两个都改，否则上游按哪一个都不一定"
        );
        assert_eq!(
            set(Dialect::Responses, json!({})),
            json!({"max_output_tokens": 7})
        );
        assert_eq!(
            set(
                Dialect::Gemini,
                json!({"generationConfig": {"temperature": 0}})
            ),
            json!({"generationConfig": {"temperature": 0, "maxOutputTokens": 7}})
        );
        assert_eq!(
            set(Dialect::Bedrock, json!({})),
            json!({"inferenceConfig": {"maxTokens": 7}})
        );
    }

    #[test]
    fn gemini_written_in_snake_case_stays_one_object() {
        // 另起一个驼峰的 generationConfig 的话，请求里就有两份生成参数
        let mut v = json!({"generation_config": {"max_output_tokens": 9, "top_k": 3}});
        assert_eq!(max_output_tokens(Dialect::Gemini, &v), Some(9));
        set_max_output_tokens(Dialect::Gemini, &mut v, 7);
        assert_eq!(
            v,
            json!({"generation_config": {"max_output_tokens": 7, "top_k": 3}})
        );
        let mut v = json!({"generation_config": {}});
        set_max_output_tokens(Dialect::Gemini, &mut v, 7);
        assert_eq!(v, json!({"generation_config": {"max_output_tokens": 7}}));
    }

    #[test]
    fn reading_agrees_with_the_decoder() {
        // 同一个请求，这里读出来的和中间表示里的一样
        let cases = [
            (
                Dialect::Chat,
                json!({"max_completion_tokens": 5, "max_tokens": 9, "messages": []}),
            ),
            (
                Dialect::Gemini,
                json!({"generation_config": {"max_output_tokens": 5}, "contents": []}),
            ),
            (
                Dialect::Bedrock,
                json!({"inferenceConfig": {"maxTokens": 5}, "messages": []}),
            ),
        ];
        for (d, v) in cases {
            let path = match d {
                Dialect::Gemini => "/v1beta/models/m:generateContent",
                Dialect::Bedrock => "/model/m/converse",
                _ => "/",
            };
            let ir = crate::convert::decode(d, &v, path, None).unwrap();
            assert_eq!(ir.request.max_tokens, max_output_tokens(d, &v), "{d:?}");
            assert_eq!(max_output_tokens(d, &v), Some(5));
        }
    }

    #[test]
    fn a_cap_lowers_or_fills_and_leaves_a_smaller_value_alone() {
        for d in ALL {
            let mut v = json!({});
            assert!(cap_max_output_tokens(d, &mut v, 100), "{d:?}");
            assert_eq!(max_output_tokens(d, &v), Some(100));
            set_max_output_tokens(d, &mut v, 50);
            assert!(!cap_max_output_tokens(d, &mut v, 100), "{d:?}");
            assert_eq!(max_output_tokens(d, &v), Some(50));
            set_max_output_tokens(d, &mut v, 500);
            assert!(cap_max_output_tokens(d, &mut v, 100), "{d:?}");
            assert_eq!(max_output_tokens(d, &v), Some(100));
        }
        let mut not_an_object = json!([1]);
        assert!(!cap_max_output_tokens(Dialect::Chat, &mut not_an_object, 1));
        assert_eq!(not_an_object, json!([1]));
    }

    #[test]
    fn the_model_lives_in_the_body_except_where_it_lives_in_the_path() {
        for d in ALL {
            let mut v = json!({"model": "big"});
            set_model(d, &mut v, "small");
            let want = if matches!(d, Dialect::Gemini | Dialect::Bedrock) {
                "big"
            } else {
                "small"
            };
            assert_eq!(v["model"], want, "{d:?}");
        }
    }

    #[test]
    fn turning_thinking_off_removes_the_switch_and_nothing_else() {
        let cases = [
            (
                Dialect::Anthropic,
                json!({"thinking": {"type": "enabled"}, "x": 1}),
            ),
            (Dialect::Chat, json!({"reasoning_effort": "high", "x": 1})),
            (
                Dialect::Responses,
                json!({"reasoning": {"effort": "high"}, "x": 1}),
            ),
            (
                Dialect::Gemini,
                json!({"generationConfig": {"thinkingConfig": {}}, "x": 1}),
            ),
            (
                Dialect::Bedrock,
                json!({"additionalModelRequestFields": {"thinking": {}}, "x": 1}),
            ),
        ];
        for (d, mut v) in cases {
            disable_thinking(d, &mut v);
            let text = v.to_string();
            assert!(
                !text.contains("thinking") && !text.contains("reasoning"),
                "{d:?}: {text}"
            );
            assert_eq!(v["x"], 1);
        }
    }
}
