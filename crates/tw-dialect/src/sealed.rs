//! 上游封存的内容：只有封存它的那个账号读得出来的密文。
//!
//! OpenAI 的 Responses 不在服务端存对话时（`store: false`，Codex 一律这样发），把下一轮还要
//! 用的东西加密交给客户端，客户端每一轮原样带回：推理（`reasoning` 条目的
//! `encrypted_content`）、远程压缩（`compaction`）、Codex 多代理之间传的话（`agent_message`
//! 里 `encrypted_content` 那一段）、工具结果里加密的那段。**密钥在 OpenAI 那边、按账号分**：
//! 换了一个账号或者一家中转，对方解不开，整个请求被拒（`invalid_encrypted_content`）。客户端
//! 每一轮都带着整段历史，于是这段对话从此每一轮都被拒。
//!
//! 能去掉的只有推理：那是模型写给自己的笔记，去掉了对话照样接得上。别的去掉就丢了内容 ——
//! 多代理之间传的话，加密的就是正文本身。所以分两种（[`Sealed`]）。
//!
//! 网关自己写的「密文」（[`crate::ir::CARRIED`] 开头：转换时替别家带着的签名、压缩摘要）
//! 不算：那是网关读得懂的，换到哪一家都能接着转。

use serde_json::Value;

use crate::ir::{CARRIED, Dialect};

/// 请求里带着上游封存的内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sealed {
    /// 只有封存的推理：别家解不开时去掉就能接着发
    Reasoning,
    /// 还有去不掉的：压缩的前文、代理之间传的话、加密的工具结果。只有封存它的那个账号接得下去
    Content,
}

/// Responses 请求体里带着的上游封存的内容。没带的是 None。
///
/// 看 `input` 里每一项的每一层：哪儿有不是网关写的 `encrypted_content`，这一项就是封存的。
/// 不按条目类型列举 —— 新出来的一种封存（多代理的话就是后来才有的）照样认得出。
pub fn sealed(body: &Value) -> Option<Sealed> {
    let mut found = None;
    for item in body.get("input")?.as_array()? {
        if !carries(item) {
            continue;
        }
        if item.get("type").and_then(Value::as_str) == Some("reasoning") {
            found = Some(Sealed::Reasoning);
        } else {
            return Some(Sealed::Content);
        }
    }
    found
}

fn carries(v: &Value) -> bool {
    match v {
        Value::Object(o) => o.iter().any(|(k, v)| {
            (k == "encrypted_content"
                && v.as_str()
                    .is_some_and(|s| !s.is_empty() && !s.starts_with(CARRIED)))
                || carries(v)
        }),
        Value::Array(a) => a.iter().any(carries),
        _ => false,
    }
}

/// 上游这个错误是在拒绝别的账号封存的内容吗。`wire` 是发给它的那一份的格式，`said` 是
/// 错误正文或流里那一帧错误。
///
/// 看原话，不看错误外壳的结构：同一句话，OpenAI 放在 `error.code`，Codex 后端和中转各有
/// 各的包法，有的只有一句话。
///
/// - OpenAI：`invalid_encrypted_content`（「The encrypted content for item rs_… could not be
///   verified」「Encrypted function output content could not be decrypted or decoded」）；
/// - Anthropic：「Invalid `signature` in `thinking` block」。
pub fn refused(wire: Dialect, said: &[u8]) -> bool {
    let text = String::from_utf8_lossy(said).to_ascii_lowercase();
    match wire {
        Dialect::Responses => {
            text.contains("invalid_encrypted_content")
                || (text.contains("encrypted")
                    && [
                        "could not be verified",
                        "could not be decrypted",
                        "could not decrypt",
                    ]
                    .iter()
                    .any(|w| text.contains(w)))
        }
        Dialect::Anthropic => {
            text.contains("invalid") && text.contains("signature") && text.contains("thinking")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input(items: Value) -> Value {
        json!({"model": "gpt-5", "input": items})
    }

    #[test]
    fn sealed_reasoning_alone_can_be_left_out() {
        let body = input(json!([
            {"role": "user", "content": "hi"},
            {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "gAAAAB"},
            {"type": "function_call", "call_id": "c1", "name": "ls", "arguments": "{}"},
        ]));
        assert_eq!(sealed(&body), Some(Sealed::Reasoning));
    }

    #[test]
    fn what_the_agents_said_to_each_other_cannot() {
        // Codex 多代理：子代理交回来的话整段加密，发信人和任务名是明文
        let body = input(json!([
            {"type": "reasoning", "encrypted_content": "gAAAAB"},
            {"type": "agent_message", "author": "/root/worker", "recipient": "/root", "content": [
                {"type": "input_text", "text": "Message Type: MESSAGE\nPayload:\n"},
                {"type": "encrypted_content", "encrypted_content": "gAAAAC"}
            ]},
        ]));
        assert_eq!(sealed(&body), Some(Sealed::Content));
        // 远程压缩的前文、加密的工具结果也是
        for item in [
            json!({"type": "compaction", "encrypted_content": "gAAAAD"}),
            json!({"type": "function_call_output", "call_id": "c1", "output": [
                {"type": "encrypted_content", "encrypted_content": "gAAAAE"}
            ]}),
        ] {
            assert_eq!(sealed(&input(json!([item]))), Some(Sealed::Content));
        }
    }

    #[test]
    fn what_the_gateway_wrote_itself_and_empty_fields_are_not_sealed() {
        let body = input(json!([
            {"type": "reasoning", "encrypted_content": "tw1.a.sig"},
            {"type": "reasoning", "encrypted_content": ""},
            {"type": "compaction", "encrypted_content": "tw1.c.aGk_"},
            {"role": "user", "content": "hi"},
        ]));
        assert_eq!(sealed(&body), None);
        assert_eq!(sealed(&json!({"messages": []})), None);
    }

    #[test]
    fn refusals_to_read_sealed_content_are_recognised() {
        for said in [
            json!({"error": {"message": "The encrypted content for item rs_68 could not be verified.",
                "type": "invalid_request_error", "code": "invalid_encrypted_content"}})
            .to_string(),
            // Codex 后端在流里报的，只有这一句
            "Encrypted function output content could not be decrypted or decoded.".into(),
            r#"{"detail":"Could not decrypt the provided encrypted_content."}"#.into(),
        ] {
            assert!(refused(Dialect::Responses, said.as_bytes()), "{said}");
        }
        let other = json!({"error": {"message": "max_output_tokens is too large", "code": "invalid_value"}});
        assert!(!refused(Dialect::Responses, other.to_string().as_bytes()));
        let anthropic = json!({"type": "error", "error": {"type": "invalid_request_error",
            "message": "messages.1.content.0: Invalid `signature` in `thinking` block"}});
        assert!(refused(
            Dialect::Anthropic,
            anthropic.to_string().as_bytes()
        ));
        let budget = json!({"type": "error", "error": {"type": "invalid_request_error",
            "message": "max_tokens: must be greater than thinking.budget_tokens"}});
        assert!(!refused(Dialect::Anthropic, budget.to_string().as_bytes()));
        // 格式对不上：Anthropic 的那句不算 Responses 的，别的格式没有封存
        assert!(!refused(
            Dialect::Responses,
            anthropic.to_string().as_bytes()
        ));
        assert!(!refused(Dialect::Chat, other.to_string().as_bytes()));
    }
}
