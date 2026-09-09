//! 一次调用的性质。
//!
//! 路由匹配的**不是目的地，是这次调用是个什么样的活儿**（DESIGN.md §3.4）。
//! 网络代理的规则回答「这个连接去哪个 IP」；这里要回答「该交给谁干」。
//!
//! 下面这些维度，没有一个在网络代理里有对应物。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestFacts {
    /// 客户端要的模型名（**不是我们要发给上游的那个**）
    pub model: String,
    /// 谁发的。靠网关密钥认出来（§3.3.1）
    pub client: String,
    /// 入站方言
    pub dialect: String,
    pub input_tokens: u64,
    pub max_tokens: Option<u64>,
    /// 带了 `cache_control`。**把它路由到不支持缓存的中转站，等于把最大
    /// 的省钱手段直接扔掉 —— 而且你不会察觉，因为请求照样成功返回。**
    pub cache: bool,
    pub tools: bool,
    pub tool_count: usize,
    pub image: bool,
    /// 扩展思考。token 单独计费且很贵
    pub thinking: bool,
    pub stream: bool,
    /// 这是客户端自己发的辅助请求吗（§4.8）。
    ///
    /// **由识别器打的标记，不是从 body 里读出来的** —— 所以
    /// `from_anthropic_body` 不会填它，网关在识别之后单独设。空字符串
    /// 表示这是一个真实的用户请求。
    pub intent: String,
}

impl RequestFacts {
    /// 从入站 body 里抽出这些性质。
    ///
    /// **只读不改**（§4.1：入站恒解析，出站直通）。这里读错了顶多是路由
    /// 走偏，读的时候动了 body 才是灾难。
    pub fn from_anthropic_body(v: &serde_json::Value) -> Self {
        let msgs = v.get("messages").and_then(|m| m.as_array());
        Self {
            model: v
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string(),
            client: String::new(),
            intent: String::new(),
            dialect: "anthropic".to_string(),
            // 粗估：4 字节约 1 token。**路由只需要量级** —— 「超过 200k」
            // 和「小于 4k」这种判断，估算完全够用，而精确计数要跑一遍
            // tokenizer，那是每个请求都要付的成本。
            input_tokens: estimate_tokens(v),
            max_tokens: v.get("max_tokens").and_then(|m| m.as_u64()),
            cache: has_cache_control(v),
            tools: v
                .get("tools")
                .and_then(|t| t.as_array())
                .is_some_and(|a| !a.is_empty()),
            tool_count: v
                .get("tools")
                .and_then(|t| t.as_array())
                .map(|a| a.len())
                .unwrap_or(0),
            image: has_image(msgs),
            thinking: v.get("thinking").is_some_and(|t| !t.is_null()),
            stream: v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false),
        }
    }
}

/// 4 字节约 1 token 的粗估。
///
/// 对中文会高估（一个汉字 3 字节但常常就是 1 个 token），但**路由只关心
/// 量级**：`>200k` 和 `<4k` 这种阈值，估算误差改变不了结论。精确计数要
/// 跑 tokenizer，那是每个请求都要付的成本，换来的精度没有用处。
fn estimate_tokens(v: &serde_json::Value) -> u64 {
    let mut bytes = 0usize;
    if let Some(s) = v.get("system") {
        bytes += json_text_len(s);
    }
    if let Some(m) = v.get("messages") {
        bytes += json_text_len(m);
    }
    if let Some(t) = v.get("tools") {
        bytes += json_text_len(t);
    }
    (bytes / 4) as u64
}

/// 只数文本内容的长度，不数 JSON 结构本身。
fn json_text_len(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::String(s) => s.len(),
        serde_json::Value::Array(a) => a.iter().map(json_text_len).sum(),
        serde_json::Value::Object(o) => o.values().map(json_text_len).sum(),
        _ => 0,
    }
}

fn has_cache_control(v: &serde_json::Value) -> bool {
    fn walk(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::Object(o) => o.contains_key("cache_control") || o.values().any(walk),
            serde_json::Value::Array(a) => a.iter().any(walk),
            _ => false,
        }
    }
    walk(v)
}

fn has_image(msgs: Option<&Vec<serde_json::Value>>) -> bool {
    let Some(msgs) = msgs else { return false };
    msgs.iter().any(|m| {
        m.get("content")
            .and_then(|c| c.as_array())
            .is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("image"))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(json: &str) -> RequestFacts {
        RequestFacts::from_anthropic_body(&serde_json::from_str(json).unwrap())
    }

    #[test]
    fn reads_the_obvious_fields() {
        let f = facts(
            r#"{"model":"claude-opus-4-5","max_tokens":8192,"stream":true,
                          "messages":[{"role":"user","content":"hi"}]}"#,
        );
        assert_eq!(f.model, "claude-opus-4-5");
        assert_eq!(f.max_tokens, Some(8192));
        assert!(f.stream);
    }

    #[test]
    fn cache_control_is_found_however_deep_it_sits() {
        // Anthropic 把 cache_control 挂在内容块上，位置随消息结构变化。
        // 找漏了的后果很具体：请求被路由到不支持缓存的中转站，**照样
        // 成功返回**，而账单悄悄翻几倍。
        assert!(
            facts(
                r#"{"messages":[{"role":"user","content":[
            {"type":"text","text":"x","cache_control":{"type":"ephemeral"}}]}]}"#
            )
            .cache
        );
        assert!(
            facts(
                r#"{"system":[{"type":"text","text":"x",
            "cache_control":{"type":"ephemeral"}}]}"#
            )
            .cache
        );
        assert!(!facts(r#"{"messages":[{"role":"user","content":"x"}]}"#).cache);
    }

    #[test]
    fn tools_are_counted_not_just_detected() {
        // 带 50 个工具定义和带 2 个，成本差一个量级（§3.4）。
        let f = facts(r#"{"tools":[{"name":"a"},{"name":"b"},{"name":"c"}]}"#);
        assert!(f.tools);
        assert_eq!(f.tool_count, 3);
        let none = facts(r#"{"tools":[]}"#);
        assert!(!none.tools, "空数组不算带工具");
        assert_eq!(none.tool_count, 0);
    }

    #[test]
    fn images_are_detected_in_content_blocks() {
        assert!(
            facts(
                r#"{"messages":[{"role":"user","content":[
            {"type":"image","source":{"type":"base64","data":"x"}}]}]}"#
            )
            .image
        );
        assert!(
            !facts(
                r#"{"messages":[{"role":"user","content":[
            {"type":"text","text":"x"}]}]}"#
            )
            .image
        );
    }

    #[test]
    fn thinking_is_on_when_the_field_is_present_and_not_null() {
        assert!(facts(r#"{"thinking":{"type":"enabled","budget_tokens":10000}}"#).thinking);
        assert!(!facts(r#"{"thinking":null}"#).thinking);
        assert!(!facts(r#"{}"#).thinking);
    }

    #[test]
    fn token_estimate_only_needs_to_get_the_order_of_magnitude_right() {
        // 路由的阈值是 >200k / <4k 这种量级判断，估算误差改变不了结论。
        // 精确计数要跑 tokenizer，那是每个请求都要付的成本。
        let long = format!(
            r#"{{"messages":[{{"role":"user","content":"{}"}}]}}"#,
            "x".repeat(40_000)
        );
        let f = facts(&long);
        assert!(
            f.input_tokens > 9_000 && f.input_tokens < 11_000,
            "估了 {}",
            f.input_tokens
        );
    }

    #[test]
    fn structural_strings_add_noise_but_not_a_different_answer() {
        // 只数字符串值，不数括号和键名 —— 但 `"role":"user"` 的 `user`
        // 和 `"type":"text"` 的 `text` 本身是字符串值，会被数进去。
        //
        // **这不修**。估算的承诺是「量级对」，而这点噪声在 >200k / <4k
        // 这种阈值上改变不了任何结论。写一个要求两种写法逐字节相等的
        // 断言，是在给函数强加一个它没做过的承诺。
        let a = facts(r#"{"messages":[{"role":"user","content":"hello"}]}"#);
        let b =
            facts(r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]}"#);
        assert!(
            a.input_tokens.abs_diff(b.input_tokens) <= 2,
            "{a:?} vs {b:?}"
        );

        // 真正要保证的是这个：一个长请求不会因为结构而被算成短的，反之亦然。
        let long = format!(
            r#"{{"messages":[{{"role":"user","content":"{}"}}]}}"#,
            "x".repeat(1_000_000)
        );
        assert!(facts(&long).input_tokens > 200_000);
        assert!(a.input_tokens < 4_000);
    }

    #[test]
    fn a_body_missing_everything_does_not_panic() {
        // 客户端会发各种东西过来。读不出来就是默认值，不该崩。
        let f = facts("{}");
        assert_eq!(f.model, "");
        assert_eq!(f.input_tokens, 0);
        assert!(!f.stream);
    }
}
