//! 提示缓存的断点，在编码好的请求体上。
//!
//! 转给 Claude 时客户端没标断点的，编码器替它标（`anthropic::request`、`bedrock::request`
//! 里的 `auto_cache`）。有的上游不认 —— 自称 Anthropic 格式的兼容接口、AWS 没列进支持表
//! 的老模型 —— 拒掉整个请求。网关这时去掉断点、同一家再发一次（见 `tw_gateway` 的
//! `cache_marks`），靠的就是这里。
//!
//! **只用在客户端自己一个断点都没标的请求上**：那时请求体里的断点全是编码器加的。客户端
//! 自己标的是它的决定，去掉了它的缓存就白建了。

use serde_json::Value;

use crate::ir::Dialect;

/// 请求体里可能有提示缓存断点吗：按字节找那个键名，不解析。有的不一定真是断点（正文里
/// 提到了这个词），没有的一定不是
pub fn may_have_marks(dialect: Dialect, body: &[u8]) -> bool {
    let key: &[u8] = match dialect {
        Dialect::Anthropic => b"cache_control",
        Dialect::Bedrock => b"cachePoint",
        _ => return false,
    };
    memchr::memmem::find(body, key).is_some()
}

/// 去掉请求体里的提示缓存断点：Anthropic 的 `cache_control`，Converse 的 `cachePoint` 块。
/// 只看工具、系统提示、消息这三处（编码器只标在这里）。
///
/// 没有可去的、不是这两种格式、不是 JSON 的返回 `None`，请求一个字节都不改。
pub fn strip_marks(dialect: Dialect, body: &[u8]) -> Option<Vec<u8>> {
    // 绝大多数请求在这里就回去了：一个字都没有，不必解析
    if !may_have_marks(dialect, body) {
        return None;
    }
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let o = v.as_object_mut()?;
    // 一处块列表：Anthropic 的块上去掉 `cache_control`，Converse 去掉 `cachePoint` 块
    let strip = |list: Option<&mut Value>| -> bool {
        let Some(Value::Array(list)) = list else {
            return false;
        };
        if dialect == Dialect::Anthropic {
            let mut changed = false;
            for b in list.iter_mut().filter_map(Value::as_object_mut) {
                changed |= b.remove("cache_control").is_some();
            }
            changed
        } else {
            let before = list.len();
            list.retain(|b| b.get("cachePoint").is_none());
            list.len() != before
        }
    };
    let mut changed = strip(o.get_mut("system"));
    changed |= strip(if dialect == Dialect::Anthropic {
        o.get_mut("tools")
    } else {
        o.get_mut("toolConfig").and_then(|c| c.get_mut("tools"))
    });
    if let Some(Value::Array(messages)) = o.get_mut("messages") {
        for m in messages.iter_mut() {
            changed |= strip(m.get_mut("content"));
        }
    }
    changed.then(|| v.to_string().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_breakpoints_and_only_them_are_taken_out() {
        let mark = json!({"type": "ephemeral"});
        let anthropic = json!({
            "model": "m",
            "tools": [{"name": "f", "input_schema": {}, "cache_control": mark}],
            "system": [{"type": "text", "text": "s", "cache_control": mark}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "mentions cache_control", "cache_control": mark}]},
                {"role": "assistant", "content": "plain"}
            ]
        });
        let out: Value = serde_json::from_slice(
            &strip_marks(Dialect::Anthropic, anthropic.to_string().as_bytes()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            out,
            json!({
                "model": "m",
                "tools": [{"name": "f", "input_schema": {}}],
                "system": [{"type": "text", "text": "s"}],
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "mentions cache_control"}]},
                    {"role": "assistant", "content": "plain"}
                ]
            })
        );

        let point = json!({"cachePoint": {"type": "default"}});
        let converse = json!({
            "system": [{"text": "s"}, point],
            "toolConfig": {"tools": [{"toolSpec": {"name": "f"}}, point]},
            "messages": [{"role": "user", "content": [{"text": "hi"}, point]}]
        });
        let out: Value = serde_json::from_slice(
            &strip_marks(Dialect::Bedrock, converse.to_string().as_bytes()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            out,
            json!({
                "system": [{"text": "s"}],
                "toolConfig": {"tools": [{"toolSpec": {"name": "f"}}]},
                "messages": [{"role": "user", "content": [{"text": "hi"}]}]
            })
        );
    }

    #[test]
    fn nothing_to_take_out_leaves_the_body_alone() {
        // 只是在正文里提到了这个词
        let body = json!({"messages": [{"role": "user", "content": "what is cache_control?"}]})
            .to_string();
        assert_eq!(strip_marks(Dialect::Anthropic, body.as_bytes()), None);
        assert_eq!(strip_marks(Dialect::Anthropic, b"{}"), None);
        assert_eq!(strip_marks(Dialect::Chat, b"{\"cache_control\":1}"), None);
        assert_eq!(strip_marks(Dialect::Bedrock, b"not json cachePoint"), None);
    }
}
