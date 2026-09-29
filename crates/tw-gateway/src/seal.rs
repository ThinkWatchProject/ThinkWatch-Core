//! 别的账号封存的推理。
//!
//! 推理模型把上一轮的推理封存起来交给客户端，下一轮原样带回：Responses 的
//! `reasoning` 条目带着 `encrypted_content`，Anthropic 的 `thinking` 块带着 `signature`
//! （`redacted_thinking` 整块都是 `data`）。**封存和产生它的账号绑定**：故障转移、
//! 换了一个账号或者另一家中转之后，新的那一家解不开，整个请求被拒 ——
//!
//! - OpenAI：400，`error.code` 是 `invalid_encrypted_content`（「The encrypted content
//!   for item rs_… could not be verified」）；
//! - Anthropic：400，「Invalid `signature` in `thinking` block」。
//!
//! 客户端每一轮都把整段历史带回来，于是这段对话从此每一轮都被拒。
//!
//! 被拒了就**去掉封存的推理，同一家再发一次**（只一次）。说过的话、调过的工具都
//! 还在，去掉的只是模型写给自己的那几段笔记。再记下（这段对话，这一家）拒过哪些，
//! 下一轮发给它之前先去掉，不用每一轮都先被拒一次 —— **只去掉它拒过的那些**：
//! 它自己后来封存的照样带上，缓存和推理的连贯都还在。

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use serde_json::Value;
use tw_dialect::ir::Dialect;

/// 一段封存的推理的指纹
pub type Seal = [u8; 32];

/// 记着几段对话就清一次早就不说话的
const PRUNE_AT: usize = 4096;

/// 被拒的响应体最多看这么多。拒绝的原话很短，没必要把一个大响应读进来找
const REFUSAL_MAX: usize = 64 * 1024;

/// (对话, 上游) → (最后一次被拒的时刻, 拒过的封存)
type ByConversation = HashMap<(String, String), (u64, HashSet<Seal>)>;

/// 每段对话、每一家拒过的封存。**只在内存里，跨重载存活**：重启之后丢了也只是
/// 下一轮再被拒一次、再重发一次。
#[derive(Default)]
pub struct Refused {
    by: Mutex<ByConversation>,
}

impl Refused {
    /// `upstream` 在对话 `conversation` 里拒了这些封存。
    pub fn note(&self, conversation: &str, upstream: &str, seals: &[Seal], at_ms: u64) {
        if seals.is_empty() {
            return;
        }
        let mut by = self.by.lock().unwrap_or_else(|p| p.into_inner());
        if by.len() >= PRUNE_AT {
            by.retain(|_, (last, _)| at_ms.saturating_sub(*last) <= crate::session::SESSION_GAP_MS);
        }
        // 都还在说话：丢最老的那段。表不能无限长
        if by.len() >= PRUNE_AT
            && let Some(oldest) = by
                .iter()
                .min_by_key(|(_, (last, _))| *last)
                .map(|(k, _)| k.clone())
        {
            by.remove(&oldest);
        }
        let e = by
            .entry((conversation.to_string(), upstream.to_string()))
            .or_default();
        e.0 = e.0.max(at_ms);
        e.1.extend(seals.iter().copied());
    }

    /// `upstream` 在对话 `conversation` 里拒过的封存。没拒过的是 None
    pub fn of(&self, conversation: &str, upstream: &str) -> Option<HashSet<Seal>> {
        let by = self.by.lock().unwrap_or_else(|p| p.into_inner());
        by.get(&(conversation.to_string(), upstream.to_string()))
            .map(|(_, s)| s.clone())
    }
}

/// 上游这个 400 是在拒绝封存的推理吗。`wire` 是发给它的那一份的格式。
///
/// 看原话，不看错误外壳的结构：同一句话，OpenAI 放在 `error.code`，Codex 后端和
/// 中转各有各的包法。
pub fn refusal(wire: Dialect, body: &[u8]) -> bool {
    if body.len() > REFUSAL_MAX {
        return false;
    }
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    match wire {
        Dialect::Responses => {
            text.contains("invalid_encrypted_content")
                || ((text.contains("encrypted content") || text.contains("encrypted_content"))
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

/// 去掉请求体里封存的推理。`only` 给了的只去掉其中的那些，没给的全去掉。
///
/// 返回去掉之后的请求体和去掉了哪些。**什么都没去掉是 None** —— 解不开、不是这两种
/// 格式、里面没有封存的推理，都是原样发。
pub fn strip(
    wire: Dialect,
    body: &[u8],
    only: Option<&HashSet<Seal>>,
) -> Option<(Vec<u8>, Vec<Seal>)> {
    if !matches!(wire, Dialect::Responses | Dialect::Anthropic) {
        return None;
    }
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let drop = |s: &str| {
        let seal = seal(s);
        only.is_none_or(|o| o.contains(&seal)).then_some(seal)
    };
    let mut removed = Vec::new();
    match wire {
        Dialect::Responses => {
            let items = v.get_mut("input")?.as_array_mut()?;
            items.retain(|it| {
                match it
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .filter(|e| !e.is_empty() && is(it, "reasoning"))
                    .and_then(drop)
                {
                    Some(seal) => {
                        removed.push(seal);
                        false
                    }
                    None => true,
                }
            });
        }
        _ => {
            let messages = v.get_mut("messages")?.as_array_mut()?;
            // 最后一条 assistant 消息开头的推理被去掉了、它又在调工具：Anthropic 要求
            // 开着推理时这条消息以推理开头，只能把这一轮的推理关掉
            let last = messages
                .iter()
                .rposition(|m| m.get("role").and_then(Value::as_str) == Some("assistant"));
            let mut lost_lead = false;
            for (i, m) in messages.iter_mut().enumerate() {
                let Some(blocks) = m.get_mut("content").and_then(Value::as_array_mut) else {
                    continue;
                };
                let mut first = true;
                blocks.retain(|b| {
                    let lead = std::mem::take(&mut first);
                    let sealed = if is(b, "thinking") {
                        b.get("signature").and_then(Value::as_str)
                    } else if is(b, "redacted_thinking") {
                        b.get("data").and_then(Value::as_str)
                    } else {
                        None
                    };
                    match sealed.filter(|s| !s.is_empty()).and_then(drop) {
                        Some(seal) => {
                            removed.push(seal);
                            lost_lead |= lead && Some(i) == last;
                            false
                        }
                        None => true,
                    }
                });
                if lost_lead && Some(i) == last && !blocks.iter().any(|b| is(b, "tool_use")) {
                    lost_lead = false;
                }
            }
            // 只剩推理的 assistant 消息去掉之后是空的：空消息会被拒
            messages.retain(|m| {
                m.get("content")
                    .and_then(Value::as_array)
                    .is_none_or(|b| !b.is_empty())
            });
            if lost_lead && let Some(o) = v.as_object_mut() {
                o.remove("thinking");
            }
        }
    }
    if removed.is_empty() {
        return None;
    }
    Some((serde_json::to_vec(&v).ok()?, removed))
}

fn is(block: &Value, kind: &str) -> bool {
    block.get("type").and_then(Value::as_str) == Some(kind)
}

fn seal(s: &str) -> Seal {
    *blake3::hash(s.as_bytes()).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn responses(items: Value) -> Vec<u8> {
        json!({"model": "gpt-5", "input": items})
            .to_string()
            .into_bytes()
    }

    #[test]
    fn openais_refusal_is_recognised_and_other_400s_are_not() {
        let refused = json!({"error": {
            "message": "The encrypted content for item rs_68 could not be verified.",
            "type": "invalid_request_error", "param": null, "code": "invalid_encrypted_content"}});
        assert!(refusal(Dialect::Responses, refused.to_string().as_bytes()));
        // 没带码、只有那句话的中转
        assert!(refusal(
            Dialect::Responses,
            br#"{"detail":"Could not decrypt the provided encrypted_content."}"#
        ));
        let other = json!({"error": {"message": "max_output_tokens is too large", "code": "invalid_value"}});
        assert!(!refusal(Dialect::Responses, other.to_string().as_bytes()));
        // 格式不对上：Anthropic 的那句不算 Responses 的
        assert!(!refusal(Dialect::Chat, refused.to_string().as_bytes()));
    }

    #[test]
    fn anthropics_refusal_is_recognised() {
        let refused = json!({"type": "error", "error": {"type": "invalid_request_error",
            "message": "messages.1.content.0: Invalid `signature` in `thinking` block"}});
        assert!(refusal(Dialect::Anthropic, refused.to_string().as_bytes()));
        let other = json!({"type": "error", "error": {"type": "invalid_request_error",
            "message": "max_tokens: must be greater than thinking.budget_tokens"}});
        assert!(!refusal(Dialect::Anthropic, other.to_string().as_bytes()));
    }

    #[test]
    fn only_the_sealed_reasoning_leaves_a_responses_request() {
        let body = responses(json!([
            {"role": "user", "content": "hi"},
            {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "gAAA-other"},
            {"type": "function_call", "call_id": "c1", "name": "ls", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": "a.txt"},
            {"type": "reasoning", "id": "rs_2", "summary": [], "encrypted_content": "gAAA-mine"},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]},
        ]));
        let (out, removed) = strip(Dialect::Responses, &body, None).unwrap();
        assert_eq!(removed.len(), 2);
        let v: Value = serde_json::from_slice(&out).unwrap();
        let kinds: Vec<_> = v["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["type"].as_str().unwrap_or("message").to_string())
            .collect();
        assert_eq!(
            kinds,
            [
                "message",
                "function_call",
                "function_call_output",
                "message"
            ]
        );

        // 只去掉拒过的那个：这一家自己后来封存的照样带上
        let only: HashSet<Seal> = [seal("gAAA-other")].into();
        let (out, removed) = strip(Dialect::Responses, &body, Some(&only)).unwrap();
        assert_eq!(removed, [seal("gAAA-other")]);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(v.to_string().contains("gAAA-mine"));
        assert!(!v.to_string().contains("gAAA-other"));
    }

    #[test]
    fn nothing_to_strip_is_none() {
        let body = responses(json!([{"role": "user", "content": "hi"}]));
        assert!(strip(Dialect::Responses, &body, None).is_none());
        let only: HashSet<Seal> = [seal("x")].into();
        let body = responses(json!([{"type": "reasoning", "encrypted_content": "y"}]));
        assert!(strip(Dialect::Responses, &body, Some(&only)).is_none());
        assert!(strip(Dialect::Chat, &body, None).is_none());
    }

    #[test]
    fn signed_thinking_leaves_an_anthropic_request_and_so_does_thinking_when_it_must() {
        let body = json!({
            "model": "claude-opus-4-7", "max_tokens": 1000,
            "thinking": {"type": "enabled", "budget_tokens": 800},
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "想", "signature": "sig-a"},
                    {"type": "text", "text": "你好"},
                ]},
                {"role": "user", "content": "列一下文件"},
                {"role": "assistant", "content": [
                    {"type": "redacted_thinking", "data": "blob"},
                    {"type": "tool_use", "id": "t1", "name": "ls", "input": {}},
                ]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "a.txt"}]},
            ],
        })
        .to_string();
        let (out, removed) = strip(Dialect::Anthropic, body.as_bytes(), None).unwrap();
        assert_eq!(removed, [seal("sig-a"), seal("blob")]);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["messages"][1]["content"],
            json!([{"type": "text", "text": "你好"}])
        );
        assert_eq!(v["messages"][3]["content"][0]["type"], "tool_use");
        // 调工具的那条不再以推理开头：这一轮的推理只能关掉
        assert!(v.get("thinking").is_none(), "{v}");
    }

    #[test]
    fn thinking_stays_on_when_the_last_turn_still_leads_with_its_own_reasoning() {
        let body = json!({
            "model": "claude-opus-4-7", "max_tokens": 1000,
            "thinking": {"type": "enabled", "budget_tokens": 800},
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "旧", "signature": "old"}]},
                {"role": "user", "content": "列一下文件"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "新", "signature": "new"},
                    {"type": "tool_use", "id": "t1", "name": "ls", "input": {}},
                ]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "a.txt"}]},
            ],
        })
        .to_string();
        let only: HashSet<Seal> = [seal("old")].into();
        let (out, _) = strip(Dialect::Anthropic, body.as_bytes(), Some(&only)).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        // 只剩推理的那条去掉了
        assert_eq!(v["messages"].as_array().unwrap().len(), 4);
        assert_eq!(v["messages"][2]["content"][0]["signature"], "new");
        assert!(v.get("thinking").is_some());
    }

    #[test]
    fn refusals_are_remembered_per_conversation_and_upstream_and_bounded() {
        let r = Refused::default();
        r.note("conv", "relay", &[seal("a")], 1);
        r.note("conv", "relay", &[seal("b")], 2);
        assert_eq!(r.of("conv", "relay").unwrap().len(), 2);
        assert!(r.of("conv", "official").is_none());
        assert!(r.of("other", "relay").is_none());
        for i in 0..PRUNE_AT + 10 {
            r.note(&format!("c{i}"), "relay", &[seal("a")], 10 + i as u64);
        }
        assert!(r.by.lock().unwrap().len() <= PRUNE_AT);
    }
}
