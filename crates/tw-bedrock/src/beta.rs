//! Anthropic 的 beta 功能在 Bedrock 上怎么开。
//!
//! 客户端直连 Anthropic 时把 beta 写在 `anthropic-beta` 请求头里。转成 Converse 之后
//! 没有这个头可发，Claude 的 beta 放进请求体的 `additionalModelRequestFields.anthropic_beta`
//! —— Claude Code 自己连 Bedrock 时也是把头里的值搬进请求体的 `anthropic_beta`。
//!
//! **只带 Bedrock 认的那几个。**直连 Anthropic 的客户端会带上一整串只有 Anthropic
//! 认的 beta（`oauth-…`、`prompt-caching-scope-…` 这些），原样交给 Bedrock，整个请求
//! 会因为不认识的 beta 被拒。名单取自 AWS 为 Bedrock 上的 Claude 列出的 beta，只留
//! 转成 Converse 之后还有意义的：服务端工具、上下文管理、压缩这些对应的请求字段在
//! 转换时已经丢掉了，只开 beta 没有用处。

use serde_json::{Map, Value};

/// Bedrock 上的 Claude 认、转成 Converse 之后仍然有用的 beta。
pub const SUPPORTED: &[&str] = &[
    // 思考和工具调用交替进行（Claude 4 起）
    "interleaved-thinking-2025-05-14",
    // 100 万 token 的上下文
    "context-1m-2025-08-07",
    // Claude 3.7 Sonnet 的 128k 输出
    "output-128k-2025-02-19",
    // 工具调用少用一些输出 token
    "token-efficient-tools-2025-02-19",
    // 工具参数边生成边流出来
    "fine-grained-tool-streaming-2025-05-14",
];

/// 客户端 `anthropic-beta` 头里 Bedrock 认的那些，去重、保持顺序。头可能分几行写，
/// 每行也可能是逗号隔开的一串。
pub fn supported<'a>(header_values: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in header_values {
        for b in v.split(',').map(str::trim) {
            if SUPPORTED.contains(&b) && !out.iter().any(|x| x == b) {
                out.push(b.to_string());
            }
        }
    }
    out
}

/// 把 beta 写进一份 Converse 请求体的 `additionalModelRequestFields.anthropic_beta`。
/// 请求体读不成 JSON 对象时原样返回 `None`，不猜。
pub fn with_betas(body: &[u8], betas: &[String]) -> Option<Vec<u8>> {
    if betas.is_empty() {
        return None;
    }
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let fields = v
        .as_object_mut()?
        .entry("additionalModelRequestFields")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()?;
    fields.insert(
        "anthropic_beta".into(),
        Value::Array(betas.iter().cloned().map(Value::String).collect()),
    );
    serde_json::to_vec(&v).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_what_bedrock_takes_is_kept_once_in_order() {
        let got = supported([
            "claude-code-20250219,interleaved-thinking-2025-05-14, oauth-2025-04-20",
            "context-1m-2025-08-07,interleaved-thinking-2025-05-14",
            "prompt-caching-scope-2026-01-05",
        ]);
        assert_eq!(
            got,
            ["interleaved-thinking-2025-05-14", "context-1m-2025-08-07"]
        );
        assert!(supported(["claude-code-20250219"]).is_empty());
    }

    #[test]
    fn betas_join_whatever_else_goes_to_the_model() {
        let body =
            br#"{"messages":[],"additionalModelRequestFields":{"thinking":{"type":"adaptive"}}}"#;
        let out = with_betas(body, &["context-1m-2025-08-07".to_string()]).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["additionalModelRequestFields"],
            serde_json::json!({
                "thinking": {"type": "adaptive"},
                "anthropic_beta": ["context-1m-2025-08-07"],
            })
        );
    }

    #[test]
    fn a_body_without_the_pocket_gets_one_and_nothing_is_touched_without_betas() {
        let out = with_betas(br#"{"messages":[]}"#, &["x".to_string()]).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["additionalModelRequestFields"]["anthropic_beta"][0], "x");
        assert!(with_betas(br#"{"messages":[]}"#, &[]).is_none());
        assert!(with_betas(b"not json", &["x".to_string()]).is_none());
    }
}
