//! 一次调用的性质。
//!
//! 路由匹配的**不是目的地，是这次调用是个什么样的活儿**。
//! 网络代理的规则回答「这个连接去哪个 IP」；这里要回答「该交给谁干」。
//!
//! 下面这些维度，没有一个在网络代理里有对应物。

use serde::{Deserialize, Serialize};
use tw_dialect::ir::{Part, Request, ToolInput, ToolKind};

use crate::time::LocalTime;

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
    /// 路由这一刻的本地时间（core 所在机器的时区）。
    ///
    /// **从请求体里读不出来，由网关在路由时填**，试算填当下；测试和试算可以塞一个定死的
    /// 时刻。没填（`None`）时，带 `time` 条件的规则不命中 —— 当成某个固定时刻会让
    /// 规则悄悄在错的时候命中。
    #[serde(default)]
    pub time: Option<LocalTime>,
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
            // **路由只需要量级** —— 「超过 200k」和「小于 4k」这种判断，估算完全够用，
            // 而精确计数要跑一遍 tokenizer，那是每个请求都要付的成本。
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
            time: None,
        }
    }
}

/// 一张图按多少 token 算。
///
/// 各家按像素算，而这里不解码图片：取 Anthropic 一张接近上限的图（约 1.15 百万像素，
/// 宽 × 高 / 750）的数。**宁可高估**：客户端拿这个数判断要不要压缩上下文，低估的
/// 代价是请求超长被拒，高估只是早一点压缩。
pub const IMAGE_TOKENS: u64 = 1_600;

/// 一份文件（PDF 之类）按多少 token 算。页数不解析，按一页多一点算一个下限
pub const FILE_TOKENS: u64 = 1_600;

/// 每条消息的结构开销（角色、分隔符）
const PER_MESSAGE: u64 = 3;

/// 估一个请求的输入 token 数。
///
/// **整个 core 只有这一个估算**：路由条件里的 `input_tokens`、数 token 由网关自己答的
/// 那个数（`tw_gateway::count`）、上游体检里拿来和上游报的输入比的那个数，都是它。
/// 以前路由另有一份「4 字节 1 个 token」：中文估低四分之一、图片一律算 0、推理照算
/// —— 同一个请求在两处是两个数，而体检比的偏偏就是这个数准不准。
///
/// 文字按 ASCII 四个字节一个 token、其余每个字符一个 token 算：英文和代码大约是
/// 这个比例，中日韩文字一个字常常就是一个 token。工具定义和工具参数按它们序列化
/// 之后的样子算，模型看到的就是那一段 JSON。图片、文件按固定的数算 —— 把 base64 的
/// 字节数算进来会把一张截图估成几十万 token。
///
/// 推理（thinking）不算：早先几轮的推理上游不放进上下文。
///
/// **每个请求都要算一次**（路由要它），所以不分配：JSON 是边序列化边数，不真的写出
/// 一个字符串。
pub fn estimate_tokens(r: &Request) -> u64 {
    let mut text = Text::default();
    let mut fixed = 0u64;
    for s in &r.system {
        text.add(s);
    }
    for m in &r.messages {
        fixed += PER_MESSAGE;
        for p in &m.parts {
            fixed += part(&mut text, p);
        }
    }
    for t in &r.tools {
        text.add(&t.name);
        if let Some(d) = &t.description {
            text.add(d);
        }
        if let ToolKind::Function { schema, .. } = &t.kind {
            text.add_json(schema);
        }
    }
    text.tokens() + fixed
}

/// 一段 JSON 里所有字符串值，按 [`estimate_tokens`] 的算法估成 token。
///
/// 数 token 的请求体解不开时用：**数 token 不该因为我们的解析器不认识某个字段就失败**，
/// 客户端拿不到这个数，那一轮就断了。
pub fn estimate_strings(v: &serde_json::Value) -> u64 {
    fn walk(v: &serde_json::Value, text: &mut Text) {
        match v {
            serde_json::Value::String(s) => text.add(s),
            serde_json::Value::Array(a) => a.iter().for_each(|v| walk(v, text)),
            serde_json::Value::Object(o) => o.values().for_each(|v| walk(v, text)),
            _ => {}
        }
    }
    let mut text = Text::default();
    walk(v, &mut text);
    text.tokens()
}

/// 一段内容：文字记进 `text`，图片、文件按固定的数返回。
fn part(text: &mut Text, p: &Part) -> u64 {
    match p {
        Part::Text(t) => text.add(t),
        Part::Thinking(_) => {}
        Part::ToolCall(c) => {
            text.add(&c.name);
            match &c.input {
                ToolInput::Json(v) => text.add_json(v),
                ToolInput::Text(t) => text.add(t),
            }
        }
        Part::ToolResult(t) => return t.content.iter().map(|p| part(text, p)).sum(),
        Part::Image(_) => return IMAGE_TOKENS,
        Part::File { .. } => return FILE_TOKENS,
    }
    0
}

/// 攒着的文字：ASCII 字节数和其余字符数分开数。
#[derive(Default)]
struct Text {
    ascii: u64,
    other: u64,
}

impl Text {
    fn add(&mut self, s: &str) {
        self.add_bytes(s.as_bytes());
    }

    /// 一段 UTF-8：ASCII 一个字节一个，其余的按字符数 —— 一个字符恰好有一个不是
    /// 续字节（`10xxxxxx`）的首字节
    fn add_bytes(&mut self, b: &[u8]) {
        for &c in b {
            if c.is_ascii() {
                self.ascii += 1;
            } else if c & 0xC0 != 0x80 {
                self.other += 1;
            }
        }
    }

    /// 一个 JSON 值序列化之后的样子（和 `to_string()` 一字不差），边写边数
    fn add_json(&mut self, v: &serde_json::Value) {
        struct Count<'a>(&'a mut Text);
        impl std::io::Write for Count<'_> {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.add_bytes(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        // 往一个永远写得进去的地方写，不会失败
        let _ = serde_json::to_writer(Count(self), v);
    }

    fn tokens(&self) -> u64 {
        self.ascii.div_ceil(4) + self.other
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

    /// 中文一个字常常就是一个 token：按字节数除以四会把中文估低四分之一
    #[test]
    fn chinese_counts_a_token_a_character() {
        let f = facts(r#"{"messages":[{"role":"user","content":"帮我重构这个文件"}]}"#);
        assert_eq!(f.input_tokens, 8 + PER_MESSAGE);
    }

    /// 图片和文件按固定的数算，base64 有多长不算；工具定义和工具往来都算；推理不算
    #[test]
    fn images_tools_and_tool_traffic_all_count() {
        let body = serde_json::json!({
            "model": "m",
            "tools": [{"name": "read_file", "description": "Read a file",
                       "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}}],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "看图"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo".repeat(10_000)}},
                ]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "不该算进去的推理".repeat(100), "signature": "s"},
                    {"type": "tool_use", "id": "t1", "name": "read_file", "input": {"path": "/a"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": [
                        {"type": "text", "text": "file body"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                    ]},
                ]},
            ],
        });
        let n = facts(&body.to_string()).input_tokens;
        assert!(n > 2 * IMAGE_TOKENS, "{n}");
        assert!(n < 2 * IMAGE_TOKENS + 100, "{n}");
    }

    /// 工具定义和参数是边序列化边数的：**和 `to_string()` 之后再数一字不差**，没有漏掉
    /// 也没有多数（非 ASCII 的字符按字符数，不按字节数）
    #[test]
    fn json_is_counted_exactly_as_it_serializes() {
        for v in [
            serde_json::json!({"type": "object", "properties": {"路径": {"type": "string",
                "description": "文件的路径 🙂，带引号\"和反斜杠\\"}}, "required": ["路径"]}),
            serde_json::json!([1, 2.5, null, true, "x"]),
            serde_json::json!("只是一个字符串"),
        ] {
            let mut streamed = Text::default();
            streamed.add_json(&v);
            let mut written = Text::default();
            written.add(&v.to_string());
            assert_eq!(
                (streamed.ascii, streamed.other),
                (written.ascii, written.other),
                "{v}"
            );
        }
    }

    /// 解不开的请求体：按里面所有字符串估，不是 0
    #[test]
    fn strings_anywhere_in_a_body_still_give_a_number() {
        // 13 + 8 个 ASCII 字符；键名和数字不算
        let v = serde_json::json!(["not an object", {"k": "abcdefgh"}, 3]);
        assert_eq!(estimate_strings(&v), 21_u64.div_ceil(4));
    }
}
