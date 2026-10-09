//! 转给 Claude 时自动标的提示缓存断点，被上游拒了怎么办。
//!
//! 客户端没标断点的请求转成 Anthropic 格式、或者转给 Bedrock 上的 Claude 时，编码器替它
//! 标（`tw_dialect::anthropic::request` 的 `auto_cache`）。有的上游不认：自称 Anthropic
//! 格式的兼容接口不收 `cache_control`，AWS 改了支持哪些模型。不认的回 400，整个请求被拒。
//!
//! 被拒了就**去掉这些断点、同一家再发一次**（只一次），再记下这一家的这个模型不认，往后
//! 发给它之前先去掉，不用每次都先被拒一次。和别家封存的推理被拒时一样（见 [`crate::seal`]）：
//! 不换一家，这一家好好的，只是不认那几个记号。
//!
//! **只动编码器加的断点**：客户端自己标了的，请求体里的断点是它的决定，原样发，被拒了也
//! 原样交回去。

use std::collections::HashMap;
use std::sync::Mutex;

/// 最多记多少个（上游，模型）。表满了丢最早记下的那个：丢了也只是下次再被拒一次、再重发
const MAX: usize = 1024;

/// 被拒的响应体最多看这么多。拒绝的原话很短
const REFUSAL_MAX: usize = 64 * 1024;

/// 哪些（上游，模型）不认自动标的断点。**只在内存里，跨重载存活**：重启之后丢了也只是
/// 下一次再被拒一次、再重发一次。
#[derive(Default)]
pub struct Refused {
    by: Mutex<HashMap<(String, String), u64>>,
}

impl Refused {
    /// `upstream` 上的 `model` 拒了自动标的断点
    pub fn note(&self, upstream: &str, model: &str, at_ms: u64) {
        let mut by = self.by.lock().unwrap_or_else(|p| p.into_inner());
        if by.len() >= MAX
            && !by.contains_key(&(upstream.to_string(), model.to_string()))
            && let Some(oldest) = by.iter().min_by_key(|(_, at)| **at).map(|(k, _)| k.clone())
        {
            by.remove(&oldest);
        }
        by.insert((upstream.to_string(), model.to_string()), at_ms);
    }

    /// `upstream` 上的 `model` 拒过自动标的断点吗
    pub fn refused(&self, upstream: &str, model: &str) -> bool {
        let by = self.by.lock().unwrap_or_else(|p| p.into_inner());
        by.contains_key(&(upstream.to_string(), model.to_string()))
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.by.lock().unwrap().len()
    }
}

/// 上游这个 400 是在拒绝提示缓存的断点吗：原话里说到了 `cache_control`、`cachePoint`
/// 或者提示缓存。
///
/// 看原话，不看错误外壳的结构：兼容接口说「Extra inputs are not permitted:
/// messages.0.content.0.cache_control」「unknown field `cache_control`」，Bedrock 说
/// `ValidationException` 加一句提到 `cachePoint` 或 prompt caching 的话，写在哪个字段里
/// 各家不一样
pub fn refusal(said: &[u8]) -> bool {
    let said = String::from_utf8_lossy(&said[..said.len().min(REFUSAL_MAX)]).to_ascii_lowercase();
    [
        "cache_control",
        "cachepoint",
        "cache_point",
        "cache point",
        "prompt caching",
        "prompt_caching",
    ]
    .iter()
    .any(|w| said.contains(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_each_upstream_says_when_it_does_not_take_breakpoints() {
        for said in [
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.0.content.0.cache_control: Extra inputs are not permitted"}}"#,
            r#"{"error":{"message":"unknown field `cache_control`, expected one of `type`, `text`","code":"invalid_request"}}"#,
            r#"{"message":"The model returned the following errors: cachePoint is not supported for this model."}"#,
            r#"{"message":"Prompt caching is not supported for this model"}"#,
        ] {
            assert!(refusal(said.as_bytes()), "{said}");
        }
        for said in [
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: Field required"}}"#,
            r#"{"message":"Malformed input request: #/messages/0/content: expected type: JSONArray"}"#,
        ] {
            assert!(!refusal(said.as_bytes()), "{said}");
        }
    }

    #[test]
    fn the_memory_is_per_upstream_and_model_and_stays_bounded() {
        let r = Refused::default();
        r.note("relay", "claude-opus-4-7", 1);
        assert!(r.refused("relay", "claude-opus-4-7"));
        assert!(!r.refused("relay", "claude-sonnet-4-5"));
        assert!(!r.refused("official", "claude-opus-4-7"));
        for i in 0..MAX + 10 {
            r.note("relay", &format!("m{i}"), 10 + i as u64);
        }
        assert_eq!(r.len(), MAX);
        // 最早记下的先丢
        assert!(!r.refused("relay", "claude-opus-4-7"));
        assert!(r.refused("relay", &format!("m{}", MAX + 9)));
    }
}
