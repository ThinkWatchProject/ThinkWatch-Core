//! OpenAI chat 响应 → Anthropic 响应。
//!
//! 非流式那一半在这里，流式的状态机在 [`crate::sse`]。

use serde_json::{Value, json};

/// `finish_reason` → `stop_reason`。
///
/// **这个映射错了是真事故**：`stop_reason` 决定客户端接下来干什么 ——
/// 看到 `tool_use` 它会去执行工具，看到 `max_tokens` 它可能会续写，
/// 而看到一个它不认识的值，多半就直接当结束了。
pub fn stop_reason(finish: Option<&str>) -> Option<&'static str> {
    match finish {
        Some("stop") => Some("end_turn"),
        Some("length") => Some("max_tokens"),
        Some("tool_calls") | Some("function_call") => Some("tool_use"),
        Some("content_filter") => Some("stop_sequence"),
        _ => None,
    }
}

/// OpenAI 的 usage → Anthropic 的 usage。
pub fn usage(u: Option<&Value>) -> Value {
    let get = |k: &str| {
        u.and_then(|x| x.get(k))
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
    };
    // OpenAI 把缓存命中放在 prompt_tokens_details.cached_tokens 里，
    // 而 Anthropic 用一个独立的 cache_read_input_tokens。**它要单独搬**
    // —— 少了它，缓存省下的钱会在成本面板上消失
    let cached = u
        .and_then(|x| x.get("prompt_tokens_details"))
        .and_then(|x| x.get("cached_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    json!({
        "input_tokens": get("prompt_tokens").saturating_sub(cached),
        "output_tokens": get("completion_tokens"),
        "cache_read_input_tokens": cached,
    })
}

/// 一整个非流式响应。
pub fn to_anthropic(v: &Value, model_fallback: &str) -> Value {
    let choice = v.get("choices").and_then(|c| c.get(0));
    let msg = choice.and_then(|c| c.get("message"));
    let mut content = Vec::new();

    if let Some(text) = msg.and_then(|m| m.get("content")).and_then(|c| c.as_str())
        && !text.is_empty()
    {
        content.push(json!({ "type": "text", "text": text }));
    }
    if let Some(Value::Array(calls)) = msg.and_then(|m| m.get("tool_calls")) {
        for c in calls {
            let f = c.get("function");
            let args = f
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .unwrap_or("{}");
            content.push(json!({
                "type": "tool_use",
                "id": c.get("id").and_then(|x| x.as_str()).unwrap_or_default(),
                "name": f.and_then(|f| f.get("name")).and_then(|x| x.as_str()).unwrap_or_default(),
                // **arguments 是字符串，要解回对象。**解不开就原样塞进
                // 一个字段里 —— 丢掉它会让客户端拿到一个没有参数的工具
                // 调用，那比一个形状奇怪的参数糟得多
                "input": serde_json::from_str::<Value>(args).unwrap_or_else(|_| json!({ "_raw": args })),
            }));
        }
    }

    json!({
        "id": v.get("id").and_then(|x| x.as_str()).unwrap_or("msg_converted"),
        "type": "message",
        "role": "assistant",
        "model": v.get("model").and_then(|x| x.as_str()).unwrap_or(model_fallback),
        "content": content,
        "stop_reason": stop_reason(choice.and_then(|c| c.get("finish_reason")).and_then(|x| x.as_str())),
        "stop_sequence": Value::Null,
        "usage": usage(v.get("usage")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(s: &str) -> Value {
        to_anthropic(&serde_json::from_str(s).unwrap(), "m")
    }

    #[test]
    fn a_plain_answer_comes_back_in_anthropic_shape() {
        let v = conv(
            r#"{"id":"chatcmpl-1","model":"deepseek-chat","choices":[{"message":{"role":"assistant","content":"你好"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":3}}"#,
        );
        assert_eq!(v["type"], "message");
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["model"], "deepseek-chat");
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "你好");
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["usage"]["input_tokens"], 10);
        assert_eq!(v["usage"]["output_tokens"], 3);
    }

    #[test]
    fn the_stop_reason_mapping_is_exhaustive_about_what_it_knows() {
        // **这个映射错了是真事故**：看到 `tool_use` 客户端会去执行工具，
        // 看到一个它不认识的值多半就直接当结束了。
        assert_eq!(stop_reason(Some("stop")), Some("end_turn"));
        assert_eq!(stop_reason(Some("length")), Some("max_tokens"));
        assert_eq!(stop_reason(Some("tool_calls")), Some("tool_use"));
        assert_eq!(stop_reason(Some("function_call")), Some("tool_use"));
        // 不认识的返回 None 而不是瞎猜一个 —— 客户端看到 null 至少知道
        // 「没说」，看到一个编出来的 end_turn 会当成正常结束
        assert_eq!(stop_reason(Some("这是什么")), None);
        assert_eq!(stop_reason(None), None);
    }

    #[test]
    fn a_tool_call_gets_its_arguments_parsed_back_into_an_object() {
        let v = conv(
            r#"{"choices":[{"message":{"tool_calls":[{"id":"call_1","type":"function","function":{"name":"Read","arguments":"{\"p\":\"/a\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        );
        assert_eq!(v["stop_reason"], "tool_use");
        assert_eq!(v["content"][0]["type"], "tool_use");
        assert_eq!(v["content"][0]["id"], "call_1");
        assert_eq!(v["content"][0]["name"], "Read");
        // **对象，不是字符串** —— Anthropic 的 input 是对象
        assert_eq!(v["content"][0]["input"]["p"], "/a");
    }

    #[test]
    fn unparseable_arguments_are_kept_rather_than_dropped() {
        // 丢掉它会让客户端拿到一个没有参数的工具调用，那比一个形状奇怪
        // 的参数糟得多。
        let v = conv(
            r#"{"choices":[{"message":{"tool_calls":[{"id":"c","function":{"name":"X","arguments":"不是 JSON"}}]},"finish_reason":"tool_calls"}]}"#,
        );
        assert_eq!(v["content"][0]["input"]["_raw"], "不是 JSON");
    }

    #[test]
    fn cached_tokens_move_into_the_field_anthropic_uses() {
        // 少了这一搬，缓存省下的钱会在成本面板上消失。
        let v = conv(
            r#"{"choices":[{"message":{"content":"x"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1000,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":800}}}"#,
        );
        assert_eq!(v["usage"]["cache_read_input_tokens"], 800);
        // **input 要减掉命中的那部分** —— Anthropic 的语义里两者不重叠，
        // 不减的话总量会被算成 1800
        assert_eq!(v["usage"]["input_tokens"], 200);
    }

    #[test]
    fn text_and_a_tool_call_in_one_answer_both_survive() {
        let v = conv(
            r#"{"choices":[{"message":{"content":"我看一下。","tool_calls":[{"id":"c","function":{"name":"Read","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        );
        let c = v["content"].as_array().unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0]["type"], "text");
        assert_eq!(c[1]["type"], "tool_use");
    }

    #[test]
    fn an_empty_response_does_not_panic() {
        let v = to_anthropic(&json!({}), "fallback-model");
        assert_eq!(v["model"], "fallback-model");
        assert!(v["content"].as_array().unwrap().is_empty());
        assert_eq!(v["stop_reason"], Value::Null);
    }
}
