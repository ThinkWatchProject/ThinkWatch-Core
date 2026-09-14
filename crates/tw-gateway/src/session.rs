//! 认出「这几十个请求是同一次任务」。
//!
//! Claude Code 的一次任务是几十到上百个请求，携带不断增长的上下文。
//! **孤立地看单个请求，看不出任何有用的东西** —— 「我一次重构花了 $4.7，
//! 其中 $3.1 是反复读同一个大文件」这种洞察只有会话视图能给。
//!
//! 判据是：**system prompt 指纹 + 首条 user message 指纹**，
//! 再加时间窗聚类（那一步在 recorder 里，因为它需要「上一条是什么时候」）。
//!
//! 为什么这两个指纹够用：一次对话里，客户端每一轮都把完整历史发上来，
//! 所以**开头那两段在整个会话里逐字不变**，而不同任务的开头几乎不会
//! 撞上。不完美，但对单机单用户场景足够 —— 而「足够」是这里的正确目标：
//! 为了那点边角情形去做精确的会话跟踪，要么得改协议，要么得让客户端配合。

use serde_json::Value;

/// 一段文本的短指纹。
fn fp(s: &str) -> String {
    blake3::hash(s.as_bytes()).to_hex()[..12].to_string()
}

/// 把 `system` 摊平成文本。它可能是字符串，也可能是内容块数组。
fn system_text(v: &Value) -> String {
    match v.get("system") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// 首条 user message 的文本。
fn first_user_text(v: &Value) -> String {
    let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) else {
        return String::new();
    };
    let Some(first) = msgs
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
    else {
        return String::new();
    };
    match first.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// 这次请求属于哪一段对话。
///
/// `None` = 认不出来。**认不出来就说认不出来** —— 硬凑一个指纹会把一堆
/// 互不相干的请求并成一个「会话」，那比没有会话视图更糟。
pub fn fingerprint(v: &Value) -> Option<String> {
    let sys = system_text(v);
    let user = first_user_text(v);
    // 两段都空 = 这个请求里没有任何能认人的东西
    if sys.is_empty() && user.is_empty() {
        return None;
    }
    Some(format!("{}-{}", fp(&sys), fp(&user)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(sys: &str, first: &str, extra_turns: usize) -> Value {
        let mut msgs = vec![json!({"role": "user", "content": first})];
        for i in 0..extra_turns {
            msgs.push(json!({"role": "assistant", "content": format!("回复 {i}")}));
            msgs.push(json!({"role": "user", "content": format!("再来 {i}")}));
        }
        json!({"model": "claude-sonnet-4-5", "system": sys, "messages": msgs})
    }

    #[test]
    fn the_same_conversation_keeps_its_fingerprint_as_it_grows() {
        // 这是整个机制成立的前提：客户端每轮都把完整历史发上来，
        // 所以开头那两段逐字不变。
        let a = fingerprint(&body("你是一个助手", "帮我重构这个文件", 0)).unwrap();
        for turns in [1, 5, 40] {
            let b = fingerprint(&body("你是一个助手", "帮我重构这个文件", turns)).unwrap();
            assert_eq!(a, b, "第 {turns} 轮的指纹变了");
        }
    }

    #[test]
    fn different_tasks_get_different_fingerprints() {
        let a = fingerprint(&body("你是一个助手", "帮我重构这个文件", 3)).unwrap();
        let b = fingerprint(&body("你是一个助手", "帮我写个测试", 3)).unwrap();
        assert_ne!(a, b);
        // system prompt 变了也算另一段 —— 换了个客户端或者换了个模式
        let c = fingerprint(&body("你是一个代码审查员", "帮我重构这个文件", 3)).unwrap();
        assert_ne!(a, c);
    }

    #[test]
    fn a_system_prompt_written_as_content_blocks_works_the_same() {
        // Claude Code 发的是数组形式，而且带 cache_control。
        let blocks = json!({
            "system": [
                {"type": "text", "text": "你是一个助手", "cache_control": {"type": "ephemeral"}}
            ],
            "messages": [{"role": "user", "content": "帮我重构这个文件"}]
        });
        let plain = body("你是一个助手", "帮我重构这个文件", 0);
        assert_eq!(
            fingerprint(&blocks),
            fingerprint(&plain),
            "两种写法该是同一段对话"
        );
    }

    #[test]
    fn a_request_with_nothing_to_go_on_says_so_rather_than_inventing_one() {
        // 硬凑一个指纹会把一堆互不相干的请求并成一个「会话」，
        // 那比没有会话视图更糟。
        assert_eq!(fingerprint(&json!({"model": "x"})), None);
        assert_eq!(fingerprint(&json!({"model": "x", "messages": []})), None);
        assert_eq!(fingerprint(&json!({"system": "", "messages": []})), None);
    }

    #[test]
    fn an_assistant_only_history_still_finds_the_first_user_turn() {
        let v = json!({
            "messages": [
                {"role": "assistant", "content": "我先说话"},
                {"role": "user", "content": "真正的第一句"}
            ]
        });
        let w = json!({"messages": [{"role": "user", "content": "真正的第一句"}]});
        assert_eq!(fingerprint(&v), fingerprint(&w));
    }

    #[test]
    fn the_fingerprint_carries_no_readable_content() {
        // 指纹会进数据库，而 system prompt 里常常有用户的私有上下文。
        let v = body("公司内部的机密提示词", "我的私密问题", 2);
        let f = fingerprint(&v).unwrap();
        assert!(!f.contains("机密"), "{f}");
        assert!(!f.contains("私密"), "{f}");
        assert_eq!(f.len(), 25, "两段 12 位加一个连字符：{f}");
    }
}
