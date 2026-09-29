//! 数 token 的本地估算。
//!
//! Claude Code 每一轮都要调一次 `/v1/messages/count_tokens`，Gemini 的客户端调
//! `:countTokens`。**这两个接口别的格式没有对应物**：转成一次真正的补全既花钱，
//! 回来的也不是客户端要的形状（见 [`crate::client_api::ClientApi::generates`]）。
//! 以前路由到别的格式的上游时直接报错，客户端那一轮就断了。
//!
//! 现在由网关自己估一个数回去：
//!
//! - 选中的上游不是客户端那种格式 —— **一个字节都不发给它**；
//! - 同格式的上游回 404 / 405 —— 不少中转只实现了生成回答，没实现数 token。
//!
//! **估的数只是量级。**客户端拿它判断上下文快满了没有，差个一两成不改变结论；
//! 精确要跑各家自己的 tokenizer，而这个请求本来就是为了不花那份钱。估出来的这一行
//! 记成网关自己答的（`local`），费用 0，不进 token 统计 —— 它没有真的用掉谁的 token。

use serde_json::Value;
use tw_dialect::ir::{Dialect, Part, Request, ToolInput, ToolKind};

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

/// 上游对数 token 回了这个状态码：它没实现这个接口，由网关来估。
///
/// 只认 404 和 405。**501 不算**：那是 Claude Code 认的「数不了」（见
/// [`crate::error::Source::NotSupported`]），它会自己去数一个准的。
pub fn unsupported(status: u16) -> bool {
    matches!(status, 404 | 405)
}

/// 估一个请求的输入 token 数。
///
/// 文字按 ASCII 四个字节一个 token、其余每个字符一个 token 算：英文和代码大约是
/// 这个比例，中日韩文字一个字常常就是一个 token —— 按字节数除以四会把中文估低
/// 四分之一。工具定义按它序列化之后的样子算，模型看到的就是那一段 JSON。
///
/// 推理（thinking）不算：早先几轮的推理上游不放进上下文。
pub fn estimate(r: &Request) -> u64 {
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
            text.add(&schema.to_string());
        }
    }
    text.tokens() + fixed
}

/// 一段内容：文字记进 `text`，图片、文件按固定的数返回。
fn part(text: &mut Text, p: &Part) -> u64 {
    match p {
        Part::Text(t) => text.add(t),
        Part::Thinking(_) => {}
        Part::ToolCall(c) => {
            text.add(&c.name);
            match &c.input {
                ToolInput::Json(v) => text.add(&v.to_string()),
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
        for c in s.chars() {
            if c.is_ascii() {
                self.ascii += 1;
            } else {
                self.other += 1;
            }
        }
    }

    fn tokens(&self) -> u64 {
        self.ascii.div_ceil(4) + self.other
    }
}

/// 替上游答这个数 token 的请求：要回给客户端的正文。`client` 是客户端的格式。
///
/// 解不开的请求体照样给一个数：按里面所有字符串的长度估。**数 token 不该因为
/// 我们的解析器不认识某个字段就失败** —— 客户端拿不到这个数，这一轮就断了。
pub fn answer(client: Dialect, path: &str, body: &[u8]) -> Vec<u8> {
    let v = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
    let n = decode(client, path, &v)
        .map(|r| estimate(&r))
        .unwrap_or_else(|| {
            let mut text = Text::default();
            strings(&v, &mut text);
            text.tokens()
        });
    let out = match client {
        Dialect::Gemini => serde_json::json!({ "totalTokens": n }),
        _ => serde_json::json!({ "input_tokens": n }),
    };
    out.to_string().into_bytes()
}

/// 按生成回答的格式解码。Anthropic 数 token 的请求体就是生成回答的那一份（少了
/// `max_tokens`）；Gemini 的要么就是 `contents` 那些，要么包在 `generateContentRequest` 里。
fn decode(client: Dialect, path: &str, v: &Value) -> Option<Request> {
    let (v, path) = match client {
        Dialect::Gemini => (
            v.get("generateContentRequest").unwrap_or(v),
            path.strip_suffix(":countTokens")
                .map(|p| format!("{p}:generateContent"))?,
        ),
        _ => (v, path.to_string()),
    };
    tw_dialect::convert::decode(client, v, &path, None)
        .ok()
        .map(|d| d.request)
}

/// 所有字符串值
fn strings(v: &Value, text: &mut Text) {
    match v {
        Value::String(s) => text.add(s),
        Value::Array(a) => a.iter().for_each(|v| strings(v, text)),
        Value::Object(o) => o.values().for_each(|v| strings(v, text)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn count(client: Dialect, path: &str, body: Value) -> Value {
        serde_json::from_slice(&answer(client, path, body.to_string().as_bytes())).unwrap()
    }

    #[test]
    fn an_anthropic_request_is_answered_in_anthropics_shape() {
        let v = count(
            Dialect::Anthropic,
            "/v1/messages/count_tokens",
            json!({
                "model": "claude-opus-4-7",
                "system": "abcdefgh",
                "messages": [{"role": "user", "content": "abcd"}],
            }),
        );
        // 8 + 4 个 ASCII 字符是 3 个 token，一条消息 3 个
        assert_eq!(v, json!({"input_tokens": 6}));
    }

    #[test]
    fn a_gemini_request_is_answered_in_geminis_shape_wrapped_or_not() {
        let contents = json!([{"role": "user", "parts": [{"text": "abcdabcd"}]}]);
        let path = "/v1beta/models/gemini-2.5-pro:countTokens";
        let bare = count(Dialect::Gemini, path, json!({"contents": contents}));
        assert_eq!(bare, json!({"totalTokens": 5}));
        let wrapped = count(
            Dialect::Gemini,
            path,
            json!({"generateContentRequest": {"model": "models/gemini-2.5-pro", "contents": contents}}),
        );
        assert_eq!(wrapped, bare);
    }

    #[test]
    fn chinese_counts_a_token_a_character() {
        let v = count(
            Dialect::Anthropic,
            "/v1/messages/count_tokens",
            json!({"model": "m", "messages": [{"role": "user", "content": "帮我重构这个文件"}]}),
        );
        assert_eq!(v["input_tokens"], 8 + PER_MESSAGE);
    }

    #[test]
    fn images_tools_and_tool_traffic_all_count() {
        let body = json!({
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
        let n = count(Dialect::Anthropic, "/v1/messages/count_tokens", body)["input_tokens"]
            .as_u64()
            .unwrap();
        // 两张图各按固定的数算，base64 的长度不算进去；推理不算
        assert!(n > 2 * IMAGE_TOKENS, "{n}");
        assert!(n < 2 * IMAGE_TOKENS + 100, "{n}");
    }

    #[test]
    fn a_body_we_cannot_decode_still_gets_a_number() {
        let v = count(
            Dialect::Anthropic,
            "/v1/messages/count_tokens",
            json!(["not an object", "abcdefgh"]),
        );
        assert!(v["input_tokens"].as_u64().unwrap() > 0, "{v}");
    }

    #[test]
    fn only_404_and_405_mean_the_upstream_cannot_count() {
        for (status, want) in [
            (404, true),
            (405, true),
            (501, false),
            (400, false),
            (500, false),
        ] {
            assert_eq!(unsupported(status), want, "{status}");
        }
    }
}
