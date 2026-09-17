//! 一次调用的性质。
//!
//! 路由匹配的**不是目的地，是这次调用是个什么样的活儿**。
//! 网络代理的规则回答「这个连接去哪个 IP」；这里要回答「该交给谁干」。
//!
//! 下面这些维度，没有一个在网络代理里有对应物。

use serde::{Deserialize, Serialize};
use tw_dialect::ir::{Part, Request, ToolInput, ToolKind};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestFacts {
    /// 客户端要的模型名（**不是我们要发给上游的那个**）
    pub model: String,
    /// 谁发的。靠网关密钥认出来
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
    /// 推理（扩展思考）开着。token 单独计费且很贵
    pub thinking: bool,
    pub stream: bool,
    /// 这是客户端自己发的辅助请求吗。
    ///
    /// **由识别器打的标记，不是从 body 里读出来的** —— 所以
    /// `from_request` 不会填它，网关在识别之后单独设。空字符串
    /// 表示这是一个真实的用户请求。
    pub intent: String,
}

impl RequestFacts {
    /// 从解码好的请求里抽出这些性质。**四种格式的客户端读的是同一份中间表示**，
    /// 一条「带图片的走 A」的规则对 Claude Code 和 Gemini CLI 一样生效。
    ///
    /// `raw` 是原始请求体，只用来找 `cache_control` —— 中间表示不带缓存标记。
    /// **只读不改**：这里读错了顶多是路由走偏，读的时候动了 body 才是灾难。
    pub fn from_request(r: &Request, raw: &serde_json::Value) -> Self {
        Self {
            model: r.model.clone(),
            client: String::new(),
            intent: String::new(),
            dialect: String::new(),
            // 粗估：4 字节约 1 token。**路由只需要量级** —— 「超过 200k」
            // 和「小于 4k」这种判断，估算完全够用，而精确计数要跑一遍
            // tokenizer，那是每个请求都要付的成本。
            input_tokens: estimate_tokens(r),
            max_tokens: r.max_tokens,
            cache: has_cache_control(raw),
            tools: !r.tools.is_empty(),
            tool_count: r.tools.len(),
            image: r.messages.iter().flat_map(|m| &m.parts).any(|p| match p {
                Part::Image(_) => true,
                Part::ToolResult(t) => t.has_image(),
                _ => false,
            }),
            thinking: r.reasoning.as_ref().is_some_and(|x| x.enabled),
            stream: r.stream,
        }
    }
}

/// 4 字节约 1 token 的粗估。
///
/// 对中文会高估（一个汉字 3 字节但常常就是 1 个 token），但**路由只关心
/// 量级**：`>200k` 和 `<4k` 这种阈值，估算误差改变不了结论。精确计数要
/// 跑 tokenizer，那是每个请求都要付的成本，换来的精度没有用处。
///
/// 图片和文件不计：它们按 token 计价的方式各家不同，而把 base64 的字节数算进来
/// 会把一张截图估成几十万 token。
fn estimate_tokens(r: &Request) -> u64 {
    let mut bytes: usize = r.system.iter().map(String::len).sum();
    for p in r.messages.iter().flat_map(|m| &m.parts) {
        bytes += match p {
            Part::Text(t) => t.len(),
            Part::Thinking(t) => t.text.len(),
            Part::ToolCall(c) => {
                c.name.len()
                    + match &c.input {
                        ToolInput::Json(v) => json_text_len(v),
                        ToolInput::Text(t) => t.len(),
                    }
            }
            Part::ToolResult(t) => t.text().len(),
            Part::Image(_) | Part::File { .. } => 0,
        };
    }
    for t in &r.tools {
        bytes += t.name.len() + t.description.as_ref().map_or(0, String::len);
        if let ToolKind::Function { schema, .. } = &t.kind {
            bytes += json_text_len(schema);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    use tw_dialect::ir::{Dialect, Dropped};

    fn facts(json: &str) -> RequestFacts {
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        let r = tw_dialect::anthropic::decode_request(&v, &mut Dropped::new(Dialect::Anthropic))
            .unwrap();
        RequestFacts::from_request(&r, &v)
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
        // 带 50 个工具定义和带 2 个，成本差一个量级。
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
    fn thinking_is_on_only_when_it_is_enabled() {
        assert!(facts(r#"{"thinking":{"type":"enabled","budget_tokens":10000}}"#).thinking);
        assert!(facts(r#"{"thinking":{"type":"adaptive"}}"#).thinking);
        // 明确关掉的不算：以前只看字段在不在，`disabled` 也被当成开着
        assert!(!facts(r#"{"thinking":{"type":"disabled"}}"#).thinking);
        assert!(!facts(r#"{"thinking":null}"#).thinking);
        assert!(!facts(r#"{}"#).thinking);
    }

    #[test]
    fn a_chat_and_a_gemini_request_give_the_same_facts_as_an_anthropic_one() {
        // 同一个请求用三种格式写，规则看到的应该是同一件事
        let chat: serde_json::Value = serde_json::from_str(
            r#"{"model":"m","max_tokens":100,"stream":true,"reasoning_effort":"high",
                "messages":[{"role":"user","content":[{"type":"text","text":"看图"},
                    {"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}]}],
                "tools":[{"type":"function","function":{"name":"a","parameters":{"type":"object"}}}]}"#,
        )
        .unwrap();
        let c = tw_dialect::chat::decode_request(
            &chat,
            &mut Dropped::new(Dialect::Chat),
            &mut Default::default(),
        )
        .unwrap();
        let gemini: serde_json::Value = serde_json::from_str(
            r#"{"contents":[{"role":"user","parts":[{"text":"看图"},{"inlineData":{"mimeType":"image/png","data":"AAAA"}}]}],
                "tools":[{"functionDeclarations":[{"name":"a","parametersJsonSchema":{"type":"object"}}]}],
                "generationConfig":{"maxOutputTokens":100,"thinkingConfig":{"thinkingBudget":20000}}}"#,
        )
        .unwrap();
        let g = tw_dialect::gemini::decode_request(
            &gemini,
            "m",
            true,
            &mut Dropped::new(Dialect::Gemini),
        )
        .unwrap();
        for f in [
            RequestFacts::from_request(&c, &chat),
            RequestFacts::from_request(&g, &gemini),
        ] {
            assert_eq!(f.model, "m");
            assert_eq!(f.max_tokens, Some(100));
            assert!(f.stream && f.image && f.tools && f.thinking, "{f:?}");
            assert_eq!(f.tool_count, 1);
        }
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
