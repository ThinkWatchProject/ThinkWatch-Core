//! Anthropic 请求 → OpenAI chat 请求（M6+）。
//!
//! # 为什么只做这一个方向
//!
//! 同方言直通覆盖九成五的场景，互转是剩下那 5%。而这 5% 里几乎
//! 全是同一件事：**用户手上有一把 DeepSeek / Kimi / GLM / 通义的 key，
//! 想让 Claude Code 用上。**那几家和 Ollama、LM Studio 一样说 OpenAI
//! chat 方言。
//!
//! 反方向（OpenAI 客户端 → Anthropic 上游）没做，因为那种用户手上本来
//! 就有 OpenAI 兼容的上游。**做一个能用的，比做四个半吊子的强** ——
//! 这一层的失败模式是**静默改坏请求**，而半吊子的转换正是它的温床。
//!
//! # 一条贯穿全文件的纪律
//!
//! **认不出来的东西要说出来，不能悄悄丢掉。**`thinking` 在 OpenAI 那边
//! 没有对应物，我们只能丢；但丢的时候必须留一句话，否则用户会发现
//! 「扩展思考开了却没生效」而完全不知道从哪儿查起。

use serde_json::{Map, Value, json};

/// 转完之后的东西。
#[derive(Debug, Clone)]
pub struct Converted {
    pub body: Value,
    /// 转不过去、只能丢掉的东西。**必须交出去给用户看**
    pub dropped: Vec<String>,
}

/// 把 Anthropic 的 `content` 摊成纯文本（用在 system 和 tool_result 上）。
fn flatten_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// 一条 Anthropic message 的 content 块 → OpenAI 的 content 项。
///
/// 返回 `None` 表示这个块不进 content（`tool_use` / `tool_result` 走
/// 别的路）。
fn content_part(block: &Value, dropped: &mut Vec<String>) -> Option<Value> {
    match block.get("type").and_then(|t| t.as_str()) {
        Some("text") => Some(json!({
            "type": "text",
            "text": block.get("text").and_then(|t| t.as_str()).unwrap_or_default(),
        })),
        Some("image") => {
            let src = block.get("source")?;
            match src.get("type").and_then(|t| t.as_str()) {
                Some("base64") => {
                    let media = src
                        .get("media_type")
                        .and_then(|m| m.as_str())
                        .unwrap_or("image/png");
                    let data = src.get("data").and_then(|d| d.as_str()).unwrap_or_default();
                    // OpenAI 收的是 data URI，Anthropic 收的是分开的两段
                    Some(json!({
                        "type": "image_url",
                        "image_url": { "url": format!("data:{media};base64,{data}") },
                    }))
                }
                Some("url") => Some(json!({
                    "type": "image_url",
                    "image_url": { "url": src.get("url").and_then(|u| u.as_str()).unwrap_or_default() },
                })),
                other => {
                    dropped.push(format!("一个 image 块的 source 类型是 {other:?}，转不过去"));
                    None
                }
            }
        }
        // 扩展思考在 OpenAI chat 里没有对应物
        Some("thinking") | Some("redacted_thinking") => {
            dropped.push("消息里的 thinking 块（OpenAI chat 方言没有它）".into());
            None
        }
        Some(other) => {
            dropped.push(format!("不认识的 content 块类型 `{other}`"));
            None
        }
        None => None,
    }
}

/// 一条 Anthropic message → 零到多条 OpenAI message。
///
/// **一条可能变成多条**：Anthropic 把 `tool_result` 塞在 user 消息的
/// content 里，而 OpenAI 要求它们各自是一条 `role: tool` 的消息。
fn convert_message(m: &Value, dropped: &mut Vec<String>) -> Vec<Value> {
    let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
    let content = m.get("content");
    let mut out = Vec::new();

    let Some(Value::Array(blocks)) = content else {
        // 纯字符串 content，直接过
        out.push(json!({ "role": role, "content": flatten_text(content.unwrap_or(&Value::Null)) }));
        return out;
    };

    // tool_result 先走 —— 它们在 OpenAI 里必须排在引用它们的 assistant
    // 消息之后、且各自成一条
    for b in blocks {
        if b.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
            out.push(json!({
                "role": "tool",
                "tool_call_id": b.get("tool_use_id").and_then(|x| x.as_str()).unwrap_or_default(),
                "content": flatten_text(b.get("content").unwrap_or(&Value::Null)),
            }));
        }
    }

    let mut parts = Vec::new();
    let mut tool_calls = Vec::new();
    for b in blocks {
        match b.get("type").and_then(|t| t.as_str()) {
            Some("tool_result") => {}
            Some("tool_use") => tool_calls.push(json!({
                "id": b.get("id").and_then(|x| x.as_str()).unwrap_or_default(),
                "type": "function",
                "function": {
                    "name": b.get("name").and_then(|x| x.as_str()).unwrap_or_default(),
                    // OpenAI 的 arguments 是**字符串**，Anthropic 的 input 是对象
                    "arguments": serde_json::to_string(b.get("input").unwrap_or(&json!({})))
                        .unwrap_or_else(|_| "{}".into()),
                },
            })),
            _ => {
                if let Some(p) = content_part(b, dropped) {
                    parts.push(p);
                }
            }
        }
    }

    if parts.is_empty() && tool_calls.is_empty() {
        return out;
    }
    let mut msg = Map::new();
    msg.insert("role".into(), json!(role));
    // 全是文本时用字符串 —— 那是绝大多数上游最稳的形状，数组形式有些
    // OpenAI 兼容实现认不全
    let all_text = parts
        .iter()
        .all(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"));
    if all_text && !parts.is_empty() {
        let text = parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        msg.insert("content".into(), json!(text));
    } else if parts.is_empty() {
        msg.insert("content".into(), Value::Null);
    } else {
        msg.insert("content".into(), Value::Array(parts));
    }
    if !tool_calls.is_empty() {
        msg.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    out.push(Value::Object(msg));
    out
}

/// Anthropic 请求体 → OpenAI chat 请求体。
pub fn to_openai(v: &Value) -> Converted {
    let mut dropped = Vec::new();
    let mut msgs = Vec::new();

    // system 变成第一条消息
    if let Some(sys) = v.get("system") {
        let text = flatten_text(sys);
        if !text.is_empty() {
            msgs.push(json!({ "role": "system", "content": text }));
        }
    }
    if let Some(Value::Array(list)) = v.get("messages") {
        for m in list {
            msgs.extend(convert_message(m, &mut dropped));
        }
    }

    let mut out = Map::new();
    out.insert(
        "model".into(),
        v.get("model").cloned().unwrap_or(Value::Null),
    );
    out.insert("messages".into(), Value::Array(msgs));
    // **`max_tokens` 在 Anthropic 是必填、在 OpenAI 是可选**，照搬就行
    for (from, to) in [
        ("max_tokens", "max_tokens"),
        ("temperature", "temperature"),
        ("top_p", "top_p"),
        ("stream", "stream"),
    ] {
        if let Some(x) = v.get(from) {
            out.insert(to.into(), x.clone());
        }
    }
    if let Some(s) = v.get("stop_sequences") {
        out.insert("stop".into(), s.clone());
    }
    if let Some(Value::Array(tools)) = v.get("tools") {
        let converted: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name").and_then(|x| x.as_str()).unwrap_or_default(),
                        "description": t.get("description").and_then(|x| x.as_str()).unwrap_or_default(),
                        "parameters": t.get("input_schema").cloned().unwrap_or(json!({"type":"object"})),
                    }
                })
            })
            .collect();
        if !converted.is_empty() {
            out.insert("tools".into(), Value::Array(converted));
        }
    }
    if let Some(tc) = v.get("tool_choice") {
        let mapped = match tc.get("type").and_then(|x| x.as_str()) {
            Some("auto") => Some(json!("auto")),
            Some("any") => Some(json!("required")),
            Some("tool") => Some(json!({
                "type": "function",
                "function": { "name": tc.get("name").and_then(|x| x.as_str()).unwrap_or_default() },
            })),
            Some("none") => Some(json!("none")),
            _ => None,
        };
        if let Some(m) = mapped {
            out.insert("tool_choice".into(), m);
        }
    }
    // **流式时要 usage。**不加这一条，OpenAI 流里根本不给 usage，
    // 而那会让成本面板对这些请求集体失明
    if v.get("stream").and_then(|x| x.as_bool()) == Some(true) {
        out.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    if v.get("thinking").is_some_and(|t| !t.is_null()) {
        dropped.push("thinking（扩展思考）：OpenAI chat 方言没有对应字段".into());
    }
    for k in ["top_k", "metadata"] {
        if v.get(k).is_some() {
            dropped.push(format!("{k}：OpenAI chat 方言没有对应字段"));
        }
    }
    Converted {
        body: Value::Object(out),
        dropped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(s: &str) -> Converted {
        to_openai(&serde_json::from_str(s).unwrap())
    }

    #[test]
    fn a_plain_conversation_comes_out_in_the_shape_openai_expects() {
        let c = conv(
            r#"{"model":"deepseek-chat","max_tokens":100,"system":"你是助手","messages":[{"role":"user","content":"你好"}]}"#,
        );
        assert_eq!(c.body["model"], "deepseek-chat");
        assert_eq!(c.body["max_tokens"], 100);
        assert_eq!(c.body["messages"][0]["role"], "system");
        assert_eq!(c.body["messages"][0]["content"], "你是助手");
        assert_eq!(c.body["messages"][1]["role"], "user");
        assert_eq!(c.body["messages"][1]["content"], "你好");
        assert!(c.dropped.is_empty(), "{:?}", c.dropped);
    }

    #[test]
    fn a_system_written_as_blocks_flattens() {
        // Claude Code 发的是数组形式，而且带 cache_control。
        let c = conv(
            r#"{"model":"m","system":[{"type":"text","text":"第一段","cache_control":{"type":"ephemeral"}},{"type":"text","text":"第二段"}],"messages":[]}"#,
        );
        assert_eq!(c.body["messages"][0]["content"], "第一段\n第二段");
    }

    #[test]
    fn tools_become_functions_with_the_schema_moved_over() {
        let c = conv(
            r#"{"model":"m","messages":[],"tools":[{"name":"Read","description":"读文件","input_schema":{"type":"object","properties":{"p":{"type":"string"}}}}]}"#,
        );
        let t = &c.body["tools"][0];
        assert_eq!(t["type"], "function");
        assert_eq!(t["function"]["name"], "Read");
        assert_eq!(t["function"]["description"], "读文件");
        assert_eq!(
            t["function"]["parameters"]["properties"]["p"]["type"],
            "string"
        );
    }

    #[test]
    fn a_tool_call_and_its_result_land_in_the_two_places_openai_wants() {
        // **一条 Anthropic 消息可能变成多条。**Anthropic 把 tool_result
        // 塞在 user 消息的 content 里，OpenAI 要求它们各自是一条
        // `role: tool`。
        let c = conv(
            r#"{"model":"m","messages":[
              {"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"Read","input":{"p":"/a"}}]},
              {"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"文件内容"}]}
            ]}"#,
        );
        let ms = c.body["messages"].as_array().unwrap();
        assert_eq!(ms.len(), 2, "{ms:#?}");
        assert_eq!(ms[0]["role"], "assistant");
        assert_eq!(ms[0]["tool_calls"][0]["id"], "tu_1");
        assert_eq!(ms[0]["tool_calls"][0]["function"]["name"], "Read");
        // **arguments 是字符串，不是对象** —— 这是两边最容易搞错的一处
        assert_eq!(
            ms[0]["tool_calls"][0]["function"]["arguments"],
            r#"{"p":"/a"}"#
        );
        assert_eq!(ms[1]["role"], "tool");
        assert_eq!(ms[1]["tool_call_id"], "tu_1");
        assert_eq!(ms[1]["content"], "文件内容");
    }

    #[test]
    fn an_image_becomes_a_data_uri() {
        let c = conv(
            r#"{"model":"m","messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":"AAAA"}},{"type":"text","text":"这是什么"}]}]}"#,
        );
        let parts = c.body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "image_url");
        assert_eq!(parts[0]["image_url"]["url"], "data:image/jpeg;base64,AAAA");
        assert_eq!(parts[1]["text"], "这是什么");
    }

    #[test]
    fn streaming_asks_for_usage_because_otherwise_the_cost_panel_goes_blind() {
        // 不加 `stream_options.include_usage`，OpenAI 流里根本不给
        // usage，而那会让成本面板对这些请求集体失明。
        let c = conv(r#"{"model":"m","messages":[],"stream":true}"#);
        assert_eq!(c.body["stream_options"]["include_usage"], true);
        // 非流式不该加它
        let c = conv(r#"{"model":"m","messages":[]}"#);
        assert!(c.body.get("stream_options").is_none());
    }

    #[test]
    fn what_cannot_be_translated_is_reported_not_silently_dropped() {
        // **用户会发现「扩展思考开了却没生效」而完全不知道从哪儿查起。**
        let c = conv(
            r#"{"model":"m","messages":[],"thinking":{"type":"enabled","budget_tokens":1024},"top_k":40}"#,
        );
        assert_eq!(c.dropped.len(), 2, "{:?}", c.dropped);
        assert!(
            c.dropped.iter().any(|d| d.contains("thinking")),
            "{:?}",
            c.dropped
        );
        assert!(
            c.dropped.iter().any(|d| d.contains("top_k")),
            "{:?}",
            c.dropped
        );
        // 而且请求本身不该带上它们
        assert!(c.body.get("thinking").is_none());
        assert!(c.body.get("top_k").is_none());
    }

    #[test]
    fn tool_choice_maps_across_all_four_shapes() {
        let m = |s: &str| {
            conv(&format!(
                r#"{{"model":"m","messages":[],"tool_choice":{s}}}"#
            ))
            .body["tool_choice"]
                .clone()
        };
        assert_eq!(m(r#"{"type":"auto"}"#), json!("auto"));
        // Anthropic 的 any = 必须调一个，OpenAI 叫 required
        assert_eq!(m(r#"{"type":"any"}"#), json!("required"));
        assert_eq!(m(r#"{"type":"none"}"#), json!("none"));
        assert_eq!(
            m(r#"{"type":"tool","name":"Read"}"#),
            json!({"type":"function","function":{"name":"Read"}})
        );
    }

    #[test]
    fn stop_sequences_get_the_name_openai_uses() {
        let c = conv(r#"{"model":"m","messages":[],"stop_sequences":["\n\n"]}"#);
        assert_eq!(c.body["stop"][0], "\n\n");
    }

    #[test]
    fn an_empty_request_does_not_panic() {
        let c = to_openai(&json!({}));
        assert!(c.body["messages"].as_array().unwrap().is_empty());
    }

    #[test]
    fn multiple_text_blocks_join_into_one_string() {
        // 数组形式有些 OpenAI 兼容实现认不全，全是文本时用字符串最稳。
        let c = conv(
            r#"{"model":"m","messages":[{"role":"user","content":[{"type":"text","text":"一"},{"type":"text","text":"二"}]}]}"#,
        );
        assert_eq!(c.body["messages"][0]["content"], "一\n二");
    }
}
