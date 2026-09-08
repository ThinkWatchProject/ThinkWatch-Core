//! 网关密钥 → 客户端身份。
//!
//! 这是整个设计的支点（DESIGN.md §3.3.1）：**密钥不只是认证，它就是身份**。
//! 所有客户端指向同一个 URL，靠这把钥匙区分是谁 —— 因此才能分开算钱、
//! 分开路由、分开限额。cc-switch 用 URL 路径前缀做这件事，而路径谁都能
//! 构造，也分不开同一种客户端的两个实例。

use axum::http::HeaderMap;

/// 四个位置，因为四种客户端各写各的。
///
/// 顺序有讲究：**先看专用头，最后才看 query**。query 里的 key 会进访问
/// 日志和浏览器历史，我们支持它只是因为 Gemini 的 SDK 这么发，不该让它
/// 盖过更干净的位置。
pub fn extract_key(headers: &HeaderMap, query: Option<&str>) -> Option<String> {
    // Anthropic 风格
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok())
        && !v.is_empty()
    {
        return Some(v.to_string());
    }
    // Gemini 风格
    if let Some(v) = headers.get("x-goog-api-key").and_then(|v| v.to_str().ok())
        && !v.is_empty()
    {
        return Some(v.to_string());
    }
    // OpenAI 风格
    if let Some(v) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        && let Some(rest) = v.strip_prefix("Bearer ")
        && !rest.is_empty()
    {
        return Some(rest.to_string());
    }
    // `?key=` —— Gemini 的 REST 形态
    if let Some(q) = query {
        for pair in q.split('&') {
            if let Some(v) = pair.strip_prefix("key=")
                && !v.is_empty()
            {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// 常数时间比较。密钥比对不该给出计时侧信道 —— 这在本机这个场景下威胁很低，
/// 但监听局域网是支持的配置（§5.4），而这行代码的成本是零。
pub fn key_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    fn h(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        m
    }

    #[test]
    fn reads_all_four_positions() {
        assert_eq!(
            extract_key(&h(&[("x-api-key", "tw-1")]), None).as_deref(),
            Some("tw-1")
        );
        assert_eq!(
            extract_key(&h(&[("x-goog-api-key", "tw-2")]), None).as_deref(),
            Some("tw-2")
        );
        assert_eq!(
            extract_key(&h(&[("authorization", "Bearer tw-3")]), None).as_deref(),
            Some("tw-3")
        );
        assert_eq!(
            extract_key(&HeaderMap::new(), Some("key=tw-4")).as_deref(),
            Some("tw-4")
        );
    }

    #[test]
    fn query_is_the_last_resort() {
        // ?key= 会进访问日志和浏览器历史。支持它是因为 Gemini SDK 这么
        // 发，但不该盖过更干净的位置。
        let m = h(&[("x-api-key", "from-header")]);
        assert_eq!(
            extract_key(&m, Some("key=from-query")).as_deref(),
            Some("from-header")
        );
    }

    #[test]
    fn an_empty_value_is_not_a_key() {
        // 空的 x-api-key 应该继续往下找，而不是当成「带了一把空钥匙」。
        let m = h(&[("x-api-key", ""), ("authorization", "Bearer tw-real")]);
        assert_eq!(extract_key(&m, None).as_deref(), Some("tw-real"));
    }

    #[test]
    fn bare_authorization_without_bearer_is_ignored() {
        assert_eq!(extract_key(&h(&[("authorization", "tw-1")]), None), None);
        assert_eq!(extract_key(&h(&[("authorization", "Bearer ")]), None), None);
    }

    #[test]
    fn no_key_anywhere_is_none() {
        assert_eq!(extract_key(&HeaderMap::new(), None), None);
        assert_eq!(extract_key(&HeaderMap::new(), Some("beta=true")), None);
    }

    #[test]
    fn key_comparison_is_length_safe_and_correct() {
        assert!(key_eq("tw-abc", "tw-abc"));
        assert!(!key_eq("tw-abc", "tw-abd"));
        assert!(!key_eq("tw-abc", "tw-abcd"));
        assert!(!key_eq("", "x"));
        assert!(key_eq("", ""));
    }

    #[test]
    fn query_parsing_does_not_match_a_suffix() {
        // `monkey=x` 里也有 "key="，但那不是我们的参数。
        assert_eq!(extract_key(&HeaderMap::new(), Some("monkey=x")), None);
        assert_eq!(
            extract_key(&HeaderMap::new(), Some("a=1&key=tw-9")).as_deref(),
            Some("tw-9")
        );
    }
}
