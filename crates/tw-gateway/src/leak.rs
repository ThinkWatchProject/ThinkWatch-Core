//! 出站密钥检测（M3 那句「观察态里最便宜的那一条」）。
//!
//! **一组正则就够，不需要整套脱敏。**这一层只回答一个问题：这个请求
//! 体里有没有看起来像凭据的东西，正在发给一个我们知道名字的上游。
//!
//! 它跑在观察态：**只记录，不改变任何行为**。攒够一周之后，
//! 界面上出现的不是一句「我们有安全功能」，而是
//!
//! > 过去 7 天，有 3 个请求把你的 API key 发给了 relay-cn。
//!
//! 这比任何功能介绍都有说服力，因为它说的是**已经发生在你身上的事**。
//!
//! 两条纪律：
//!
//! **一、这一层只看，不动。**换成占位符是「拦截」态的事，而那要等
//! 那套完整的脱敏（要能在响应里换回来）。
//!
//! **二、报出来的东西一律打码。**「发现了 sk-ant-xxx」这句话本身就是
//! 一次泄漏 —— 它会进日志、进界面、被复制到 issue 里。

use serde::{Deserialize, Serialize};

/// 一次发现。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// 哪种凭据，见 [`tw_redact::rules::Hit::secret`]
    pub kind: String,
    /// **已打码**的样子，比如 `sk-an…7f9c`
    pub masked: String,
}

/// 扫一遍请求体。
///
/// **规则本身住在 [`tw_redact::rules`] 里，这里只是把它接到观察态上。**
/// 「什么东西看起来像凭据」这个知识不能有两份 —— 观察态报了而拦截态没
/// 脱、或者反过来，都是用户没法理解的行为，而且没人会注意到它在漂移。
pub fn scan(body: &[u8]) -> Vec<Finding> {
    // 只看 UTF-8 的那部分。二进制体（图片之类）里不会有粘贴进来的 key
    let text = String::from_utf8_lossy(body);
    let mut out: Vec<Finding> = Vec::new();
    // 观察态**看全部类别** —— 它的作用是攒证据，而不是按某一家的配置
    // 决定看什么
    for h in tw_redact::rules::scan(&text, tw_redact::rules::Kind::all()) {
        let f = Finding {
            kind: h.secret.to_string(),
            // **打码之后才记。**「发现了 sk-ant-xxx」这句话本身就是一次
            // 泄漏 —— 它会进日志、进界面、被复制到 issue 里
            masked: tw_secret::mask_secret(&text[h.bytes]),
        };
        if !out.contains(&f) {
            out.push(f);
        }
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(s: &str) -> Vec<String> {
        scan(s.as_bytes()).into_iter().map(|f| f.kind).collect()
    }

    #[test]
    fn a_pasted_anthropic_key_is_found() {
        let body = r#"{"messages":[{"role":"user","content":"我的 key 是 sk-ant-api03-abcdefghijklmnopqrstuvwxyz1234"}]}"#;
        assert_eq!(kinds(body), vec!["anthropic-api-key"]);
    }

    #[test]
    fn the_finding_never_carries_the_key_in_the_clear() {
        // **「发现了 sk-ant-xxx」这句话本身就是一次泄漏** —— 它会进
        // 日志、进界面、被复制到 issue 里。
        let key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz1234";
        let f = &scan(key.as_bytes())[0];
        assert!(!f.masked.contains("abcdefghijklmnop"), "{}", f.masked);
        assert!(f.masked.starts_with("sk-an"), "{}", f.masked);
    }

    #[test]
    fn several_kinds_in_one_body_are_all_reported() {
        let body =
            "sk-ant-api03-aaaaaaaaaaaaaaaaaaaaaaaa 还有 ghp_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let ks = kinds(body);
        assert!(ks.contains(&"anthropic-api-key".to_string()), "{ks:?}");
        assert!(ks.contains(&"github-personal-token".to_string()), "{ks:?}");
    }

    #[test]
    fn the_same_key_twice_is_reported_once() {
        // 一次发现说的是「这个请求里有一把 key」，不是「有几处提到它」。
        let k = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz1234";
        assert_eq!(scan(format!("{k} 和 {k}").as_bytes()).len(), 1);
    }

    // ── 不该报的。**这一组比上一组重要。** ──────────────────────────
    #[test]
    fn an_ordinary_request_body_reports_nothing() {
        // **一个天天误报的安全功能，用户第二天就关了**。
        for b in [
            r#"{"model":"claude-sonnet-4-5","max_tokens":8000,"messages":[]}"#,
            r#"{"content":"请帮我重构 src/main.rs 里的 handle_request 函数"}"#,
            r#"{"tools":[{"name":"read_file","input_schema":{"type":"object"}}]}"#,
            "commit 7f3a9c2e8b1d4f6a0c5e9b3d7f1a4c8e2b6d0f5a",
            "550e8400-e29b-41d4-a716-446655440000",
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==",
        ] {
            assert!(kinds(b).is_empty(), "误报了：{b}");
        }
    }

    #[test]
    fn a_placeholder_that_looks_like_a_key_is_not_reported() {
        // 文档、示例、测试里到处都是这些。
        for b in [
            "sk-test",
            "sk-xxx",
            "sk-your-key-here",
            "sk-ant-xxx",
            "AKIA",
        ] {
            assert!(kinds(b).is_empty(), "占位符被当成密钥了：{b}");
        }
    }

    #[test]
    fn a_key_shaped_substring_glued_to_something_else_is_not_a_key() {
        // `xsk-ant-…` 里的 `sk-ant-…` 不是一把密钥，而按子串找会把它
        // 算上 —— 那种误报没法解释。
        let s = "变量名 xsk-ant-api03-abcdefghijklmnopqrstuvwxyz1234";
        assert!(kinds(s).is_empty(), "{s}");
    }

    #[test]
    fn a_binary_body_does_not_panic_or_report() {
        let bytes: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let _ = scan(&bytes);
    }

    #[test]
    fn a_multibyte_body_does_not_panic() {
        // 按字节切 &str 的坑在这一层同样存在。
        for s in ["中文中文中文", "🙂🙂🙂", "", "日本語のテキスト"] {
            let _ = scan(s.as_bytes());
        }
    }

    #[test]
    fn a_long_hex_hash_is_not_mistaken_for_an_openai_key() {
        // **纯字母或纯数字的长串不该命中** —— 那是 hash、id、占位符的
        // 形状，而一把真的 key 两者都有。
        //
        //（`sk-` 加一串十六进制**会**命中，而且我不打算改：那个形状和
        // 一把真 OpenAI key 无法区分，而漏报一把真 key 比多问一句糟。）
        assert!(kinds("sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").is_empty());
        assert!(kinds("sk-111111111111111111111111111111111111111111").is_empty());
    }
}
