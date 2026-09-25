//! DeepSeek Harness（dsh）发的请求：认出它，发给别家之前去掉只有 DeepSeek 认的东西。
//!
//! dsh 是 DeepSeek 官方开源的编码代理（0.1.7 发 Anthropic Messages，0.1.5 发 Chat
//! Completions）。**不管配置的地址是哪里，它都照 DeepSeek 官方接口的样子发**：
//!
//! - 请求头：User-Agent 是 `deepseek-harness/<版本> (+<地址>)`，另有
//!   `x-deepseek-harness-user-id` / `-session-id` / `-compact`
//! - 顶层的 `dsh_*` 字段：`dsh_session_log`（整段会话日志，默认开着，单次最多 8 MiB）、
//!   `dsh_plugin_packages`
//! - messages 里 `role: system` 的条目（改过的系统提示），其中可以有 `tool_addition` /
//!   `tool_removal` 块（对话中途增删工具，配着 `anthropic-beta:
//!   mid-conversation-tool-changes-…` 和工具上的 `defer_loading`）
//! - 思考只写 `thinking.type: enabled` 加 `output_config.effort`，不带预算
//!
//! **上游是 DeepSeek 官方时一个字节都不改**：这些本来就是发给它的。别的上游不认，有的
//! 兼容实现还会因为不认识的字段整个拒绝；会话日志带着整段对话，更不该送给一个本来不收
//! 它的上游。那时由 [`clean`] 去掉（同格式直通），或者由解码器在转换时丢掉。请求头那
//! 两件（[`own_header`]、[`anthropic_beta`]）在网关里做。

use serde_json::{Map, Value, json};

use crate::ir::{Dialect, Effort, str_of};
use crate::think;

/// User-Agent 的产品名。实测：`deepseek-harness/0.1.7 (+https://github.com/…)`
const PRODUCT: &str = "deepseek-harness/";
/// dsh 自己的请求头的前缀
const HEADER_PREFIX: &str = "x-deepseek-harness-";
/// 请求体里 dsh 扩展字段的前缀。官方文档把这个前缀整个留给了 dsh
const FIELD_PREFIX: &str = "dsh_";
const SESSION_LOG: &str = "dsh_session_log";
/// 对话中途增删工具要开的 beta，日期部分会变
const TOOL_CHANGES_BETA: &str = "mid-conversation-tool-changes-";

/// 认出来的 dsh 请求。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Harness {
    /// 请求带着的会话日志（`dsh_session_log`）序列化之后有多少字节。没带是 None
    pub session_log_bytes: Option<u64>,
}

/// 这个请求是不是 dsh 发的：User-Agent 是它的，或者请求体里有 `dsh_*` 字段。
///
/// **两条任一即可**：User-Agent 可以被配置改掉，扩展字段也可以关掉。
pub fn detect(user_agent: Option<&str>, body: Option<&Value>) -> Option<Harness> {
    let by_agent = user_agent
        .and_then(|ua| ua.get(..PRODUCT.len()))
        .is_some_and(|p| p.eq_ignore_ascii_case(PRODUCT));
    let fields = body.and_then(Value::as_object);
    let by_fields = fields.is_some_and(|o| o.keys().any(|k| k.starts_with(FIELD_PREFIX)));
    if !by_agent && !by_fields {
        return None;
    }
    Some(Harness {
        session_log_bytes: fields
            .and_then(|o| o.get(SESSION_LOG))
            .filter(|v| !v.is_null())
            .map(json_len),
    })
}

/// 一个 JSON 值写出来有多少字节。**只数不存**：会话日志可以有 8 MiB
fn json_len(v: &Value) -> u64 {
    struct Count(u64);
    impl std::io::Write for Count {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0 += b.len() as u64;
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut c = Count(0);
    // 写进一个只计数的 writer，不会失败
    let _ = serde_json::to_writer(&mut c, v);
    c.0
}

/// 这是 dsh 自己的请求头（`x-deepseek-harness-*`）。发给 DeepSeek 官方以外的上游时不转发：
/// 里面是一个匿名用户 id 和会话 id，别家用不上。
pub fn own_header(name: &str) -> bool {
    name.get(..HEADER_PREFIX.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(HEADER_PREFIX))
}

/// `anthropic-beta` 去掉对话中途增删工具那一项（那些块已经被 [`clean`] 去掉了）。
/// 一项都不剩时是 None，这个头就不发了。
pub fn anthropic_beta(value: &str) -> Option<String> {
    let kept: Vec<&str> = value
        .split(',')
        .map(str::trim)
        .filter(|b| !b.is_empty() && !b.starts_with(TOOL_CHANGES_BETA))
        .collect();
    (!kept.is_empty()).then(|| kept.join(","))
}

/// 清理过的请求体。
#[derive(Debug, Clone, PartialEq)]
pub struct Cleaned {
    pub body: Vec<u8>,
    /// 转不过去、被丢掉的字段，写法和格式转换列的一样
    pub dropped: Vec<String>,
}

/// 同格式直通、上游又不是 DeepSeek 官方时，去掉 dsh 的扩展。没有可去的时候是 None。
///
/// - 所有格式：顶层的 `dsh_*` 字段去掉
/// - Anthropic：messages 里 `role: system` 的条目按顺序并进 `system`（Anthropic 的
///   messages 里没有这个角色），里面的 `tool_addition` / `tool_removal` 去掉并列为丢弃；
///   工具上的 `defer_loading` 去掉（没有了增加工具的块，被推迟的工具永远不会出现），也
///   列为丢弃；只写了 `thinking.type: enabled` 的，按模型补成 Anthropic 认的写法
///
/// Chat 的 messages 本来就有 `system` 角色，dsh 0.1.5 也不发增删工具的块，只去字段。
pub fn clean(client: Dialect, body: &[u8]) -> Option<Cleaned> {
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let o = v.as_object_mut()?;
    let before = o.len();
    o.retain(|k, _| !k.starts_with(FIELD_PREFIX));
    let mut changed = o.len() != before;
    let mut dropped = Vec::new();
    if client == Dialect::Anthropic {
        changed |= system_updates(o, &mut dropped);
        changed |= deferred_tools(o, &mut dropped);
        changed |= thinking(o);
    }
    changed.then(|| Cleaned {
        body: v.to_string().into_bytes(),
        dropped,
    })
}

/// 请求转换成另一种格式、发给 DeepSeek 官方时，把客户端请求里的 `dsh_*` 字段照原样
/// 带过去：直连 DeepSeek 时它们本来就在，两种格式它都收。客户端没带时是 None。
pub fn carry(client_body: &[u8], into: &[u8]) -> Option<Vec<u8>> {
    let client: Value = serde_json::from_slice(client_body).ok()?;
    let fields: Vec<(&String, &Value)> = client
        .as_object()?
        .iter()
        .filter(|(k, _)| k.starts_with(FIELD_PREFIX))
        .collect();
    if fields.is_empty() {
        return None;
    }
    let mut v: Value = serde_json::from_slice(into).ok()?;
    let o = v.as_object_mut()?;
    for (k, x) in fields {
        o.insert(k.clone(), x.clone());
    }
    Some(v.to_string().into_bytes())
}

/// messages 里 `role: system` 的条目并进 `system`，按出现的顺序接在后面。
fn system_updates(o: &mut Map<String, Value>, dropped: &mut Vec<String>) -> bool {
    let Some(messages) = o.get_mut("messages").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut texts: Vec<String> = Vec::new();
    let before = messages.len();
    messages.retain(|m| {
        if str_of(m, "role") != Some("system") {
            return true;
        }
        match m.get("content") {
            Some(Value::String(s)) if !s.is_empty() => texts.push(s.clone()),
            Some(Value::Array(blocks)) => {
                for b in blocks {
                    match str_of(b, "type") {
                        Some("text") => {
                            if let Some(t) = str_of(b, "text").filter(|t| !t.is_empty()) {
                                texts.push(t.to_string());
                            }
                        }
                        // tool_addition、tool_removal：别家没有对话中途增删工具的写法
                        other => note(
                            dropped,
                            format!("messages.content.{}", other.unwrap_or("unknown")),
                        ),
                    }
                }
            }
            _ => {}
        }
        false
    });
    if messages.len() == before {
        return false;
    }
    if !texts.is_empty() {
        match o.get_mut("system") {
            Some(Value::Array(blocks)) => {
                blocks.extend(
                    texts
                        .into_iter()
                        .map(|t| json!({ "type": "text", "text": t })),
                );
            }
            // dsh 自己拼系统提示也是用空行隔开
            Some(Value::String(s)) if !s.is_empty() => {
                for t in texts {
                    s.push_str("\n\n");
                    s.push_str(&t);
                }
            }
            _ => {
                o.insert("system".into(), Value::String(texts.join("\n\n")));
            }
        }
    }
    true
}

/// 工具上的 `defer_loading` 去掉：它等着一个 `tool_addition` 来加载，而那些块发不过去。
/// 留着的话，Anthropic 会要一个工具搜索工具，别家可能根本不认这个字段。
fn deferred_tools(o: &mut Map<String, Value>, dropped: &mut Vec<String>) -> bool {
    let mut any = false;
    if let Some(tools) = o.get_mut("tools").and_then(Value::as_array_mut) {
        for t in tools.iter_mut().filter_map(Value::as_object_mut) {
            any |= t.remove("defer_loading").is_some();
        }
    }
    if any {
        note(dropped, "tools.defer_loading".into());
    }
    any
}

/// dsh 的思考只写 `thinking.type: enabled` 加 `output_config.effort`，不带预算。
/// DeepSeek 认这个写法，Anthropic 不认（`enabled` 必须带 `budget_tokens`），按模型改成
/// 它认的：自适应的模型写 `adaptive`，强度留在 `output_config` 里；别的写成预算，强度
/// 折进预算里（折法和格式转换用的同一张表，见 [`think`]）。
fn thinking(o: &mut Map<String, Value>) -> bool {
    let Some(t) = o.get("thinking") else {
        return false;
    };
    if str_of(t, "type") != Some("enabled") || t.get("budget_tokens").is_some() {
        return false;
    }
    let model = o.get("model").and_then(Value::as_str).unwrap_or_default();
    if think::claude_adaptive(model) {
        o.insert("thinking".into(), json!({ "type": "adaptive" }));
        return true;
    }
    // dsh 不写强度时用的是 high
    let effort = o
        .get("output_config")
        .and_then(|c| str_of(c, "effort"))
        .and_then(think::parse_anthropic)
        .unwrap_or(Effort::High);
    // 预算不少于 1024，且必须小于 max_tokens；放不下就不开
    let budget = think::budget_of_effort(effort);
    let budget = match o.get("max_tokens").and_then(Value::as_u64) {
        Some(m) => budget.min(m.saturating_sub(1)),
        None => budget,
    };
    let thinking = if budget < 1024 {
        json!({ "type": "disabled" })
    } else {
        json!({ "type": "enabled", "budget_tokens": budget })
    };
    o.insert("thinking".into(), thinking);
    // 手动预算的模型不认强度，它说的已经折进预算了
    if let Some(c) = o.get_mut("output_config").and_then(Value::as_object_mut) {
        c.remove("effort");
        if c.is_empty() {
            o.remove("output_config");
        }
    }
    true
}

fn note(dropped: &mut Vec<String>, path: String) {
    if !dropped.contains(&path) {
        dropped.push(path);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// dsh 0.1.7 发的 Anthropic 请求：会话日志、插件清单、中途的系统提示和增删工具
    pub(crate) fn anthropic_request() -> Value {
        json!({
            "model": "deepseek-v4-pro",
            "stream": true,
            "max_tokens": 32000,
            "system": "You are DeepSeek Harness.",
            "thinking": { "type": "enabled" },
            "output_config": { "effort": "max" },
            "tools": [
                { "name": "read", "description": "Read a file", "input_schema": { "type": "object" } },
                { "name": "web", "description": "Search", "input_schema": { "type": "object" }, "defer_loading": true }
            ],
            "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
                { "role": "system", "content": [
                    { "type": "text", "text": "The project root is /work." },
                    { "type": "tool_addition", "tool": { "type": "tool_reference", "name": "web" } }
                ] },
                { "role": "assistant", "content": [{ "type": "text", "text": "hello" }] },
                { "role": "user", "content": [{ "type": "text", "text": "search" }] }
            ],
            "dsh_session_log": {
                "version": 1, "sessionFormatVersion": 2,
                "session": { "version": 2, "id": "s", "createdAt": 1780000000000u64 },
                "afterSeq": -1, "throughSeq": 0,
                "events": [{ "type": "turn/start", "seq": 0, "time": 1780000000001u64, "data": { "turn": 1 } }]
            },
            "dsh_plugin_packages": { "version": 1, "packages": [] }
        })
    }

    #[test]
    fn a_harness_request_is_known_by_its_agent_or_its_fields() {
        let ua = "deepseek-harness/0.1.7 (+https://github.com/deepseek-ai/deepseek-harness)";
        assert!(detect(Some(ua), None).is_some());
        assert!(detect(Some("DeepSeek-Harness/0.1.5"), None).is_some());
        let body = anthropic_request();
        let h = detect(Some("node"), Some(&body)).unwrap();
        let log = serde_json::to_vec(&body["dsh_session_log"]).unwrap();
        assert_eq!(h.session_log_bytes, Some(log.len() as u64));
        // 关掉了会话日志的 dsh：认得出，但没有会话日志
        let h = detect(Some(ua), Some(&json!({"model": "m", "messages": []}))).unwrap();
        assert_eq!(h.session_log_bytes, None);
        assert!(detect(Some("claude-cli/2.1.0"), Some(&json!({"model": "m"}))).is_none());
        assert!(detect(None, None).is_none());
        // 一个多字节字符不会让前缀比较越界
        assert!(detect(Some("深度求索"), None).is_none());
    }

    #[test]
    fn only_its_own_headers_and_its_beta_are_singled_out() {
        assert!(own_header("x-deepseek-harness-user-id"));
        assert!(own_header("X-DeepSeek-Harness-Session-Id"));
        assert!(!own_header("x-deepseek-request-id"));
        assert!(!own_header("user-agent"));
        assert_eq!(
            anthropic_beta("mid-conversation-tool-changes-2026-07-01"),
            None
        );
        assert_eq!(
            anthropic_beta("files-api-2025-04-14, mid-conversation-tool-changes-2026-07-01")
                .as_deref(),
            Some("files-api-2025-04-14")
        );
    }

    #[test]
    fn an_anthropic_request_is_cleaned_for_another_upstream() {
        let body = anthropic_request().to_string();
        let c = clean(Dialect::Anthropic, body.as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&c.body).unwrap();
        let o = v.as_object().unwrap();
        assert!(!o.keys().any(|k| k.starts_with("dsh_")), "{v}");
        // 系统提示的更新按顺序接在 system 后面，消息里不再有 system 角色
        assert_eq!(
            v["system"],
            "You are DeepSeek Harness.\n\nThe project root is /work."
        );
        let roles: Vec<&str> = v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant", "user"]);
        assert!(v["tools"][1].get("defer_loading").is_none());
        assert_eq!(
            c.dropped,
            ["messages.content.tool_addition", "tools.defer_loading"]
        );
        // 不是 Claude 的模型：强度折成预算，output_config 里只有它时整个去掉
        assert_eq!(
            v["thinking"],
            json!({"type": "enabled", "budget_tokens": 31999})
        );
        assert!(v.get("output_config").is_none());
    }

    #[test]
    fn thinking_is_written_the_way_the_model_takes_it() {
        let mut r = anthropic_request();
        r["model"] = json!("claude-opus-4-7");
        let c = clean(Dialect::Anthropic, r.to_string().as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&c.body).unwrap();
        assert_eq!(v["thinking"], json!({"type": "adaptive"}));
        assert_eq!(v["output_config"]["effort"], "max");

        // 预算放不下：不开
        let mut r = anthropic_request();
        r["max_tokens"] = json!(512);
        r["output_config"] =
            json!({"effort": "low", "format": {"type": "json_schema", "schema": {}}});
        let c = clean(Dialect::Anthropic, r.to_string().as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&c.body).unwrap();
        assert_eq!(v["thinking"], json!({"type": "disabled"}));
        assert!(v["output_config"].get("effort").is_none());
        assert_eq!(v["output_config"]["format"]["type"], "json_schema");
    }

    #[test]
    fn system_updates_join_whatever_shape_the_system_prompt_has() {
        let mut r = anthropic_request();
        r["system"] = json!([{"type": "text", "text": "A"}]);
        let c = clean(Dialect::Anthropic, r.to_string().as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&c.body).unwrap();
        assert_eq!(v["system"][1]["text"], "The project root is /work.");

        let mut r = anthropic_request();
        r.as_object_mut().unwrap().remove("system");
        let c = clean(Dialect::Anthropic, r.to_string().as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&c.body).unwrap();
        assert_eq!(v["system"], "The project root is /work.");
    }

    #[test]
    fn a_chat_request_only_loses_the_extension_fields() {
        let r = json!({
            "model": "deepseek-v4-flash",
            "messages": [
                {"role": "system", "content": "You are DeepSeek Harness."},
                {"role": "user", "content": "hi"}
            ],
            "thinking": {"type": "enabled"},
            "reasoning_effort": "high",
            "dsh_session_log": {"version": 1, "events": []}
        });
        let c = clean(Dialect::Chat, r.to_string().as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&c.body).unwrap();
        assert!(v.get("dsh_session_log").is_none());
        assert_eq!(v["messages"], r["messages"]);
        assert_eq!(v["thinking"], r["thinking"]);
        assert!(c.dropped.is_empty());
    }

    #[test]
    fn a_request_with_nothing_of_the_harness_is_left_alone() {
        let r = json!({"model": "claude-sonnet-4-5", "max_tokens": 10, "messages": [
            {"role": "user", "content": "hi"}
        ]});
        assert_eq!(clean(Dialect::Anthropic, r.to_string().as_bytes()), None);
        assert_eq!(clean(Dialect::Anthropic, b"not json"), None);
    }

    #[test]
    fn the_extension_fields_ride_along_to_deepseek_after_a_conversion() {
        let client = anthropic_request().to_string();
        let out = carry(
            client.as_bytes(),
            br#"{"model":"deepseek-v4-pro","messages":[]}"#,
        )
        .unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["dsh_session_log"], anthropic_request()["dsh_session_log"]);
        assert_eq!(v["dsh_plugin_packages"]["version"], 1);
        assert_eq!(carry(br#"{"model":"m"}"#, br#"{"model":"m"}"#), None);
    }
}
