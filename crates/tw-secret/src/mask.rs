//! 打码。所有会被人看见的地方 —— 日志、UI、错误信息、导出、剪贴板 ——
//! 都必须过这里，而不是各写一遍（DESIGN.md §9.7）。

/// 把一个密钥打成 `sk-a…7f9c` 这样：留头 5 尾 4，中间省略。
///
/// **必须按字符切，不能按字节。** cc-switch 在这件事上栽过两次，间隔八
/// 个月：一次是掩码 key，一次是掩码代理地址，一个粘进配置的中文字符串
/// 就够了。Rust 比 Go 还严格 —— Go 只是切出乱码，Rust 直接 panic。
pub fn mask_secret(s: &str) -> String {
    const HEAD: usize = 5;
    const TAIL: usize = 4;
    let n = s.chars().count();
    // 太短的串留不出信息量，整串打掉。这也覆盖了空串。
    if n <= HEAD + TAIL + 1 {
        return "…".repeat(n.min(3));
    }
    let head: String = s.chars().take(HEAD).collect();
    let tail: String = s.chars().skip(n - TAIL).collect();
    format!("{head}…{tail}")
}

/// URL 只留 origin。凭据可能嵌在 path 里，query 更不用说 —— 所以不是
/// 「去掉 query」，是「只留 scheme://host:port」。
pub fn redact_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return "<非 URL>".to_string();
    };
    let rest = &url[scheme_end + 3..];
    // userinfo（user:pass@host）整段丢掉
    let host_part = match rest.find('@') {
        Some(at) => &rest[at + 1..],
        None => rest,
    };
    let host_end = host_part.find(['/', '?', '#']).unwrap_or(host_part.len());
    format!("{}://{}", &url[..scheme_end], &host_part[..host_end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_a_typical_key() {
        assert_eq!(mask_secret("sk-ant-api03-abcdefghijklmnop"), "sk-an…mnop");
    }

    #[test]
    fn short_secrets_are_fully_hidden() {
        // 注意这里写字面量而不是 `"…"[..9]` —— 后者正是本文件在警告的
        // 那种按字节切串，在测试里写它等于自打嘴巴。
        // 留头留尾对短串等于没打码。
        assert_eq!(mask_secret("abc"), "………"); // 3 个字符，和输入等长
        assert_eq!(mask_secret(""), "");
    }

    #[test]
    fn multibyte_secrets_do_not_panic() {
        // 这是 cc-switch 栽了两次的那个坑。
        let s = "密钥密钥密钥密钥密钥密钥";
        let m = mask_secret(s);
        assert!(m.contains('…'));
    }

    #[test]
    fn url_keeps_only_origin() {
        assert_eq!(
            redact_url("https://api.example.com/v1/messages?key=sk-1"),
            "https://api.example.com"
        );
        assert_eq!(
            redact_url("http://127.0.0.1:8788/v1/x"),
            "http://127.0.0.1:8788"
        );
    }

    #[test]
    fn url_drops_userinfo() {
        // 凭据也可能藏在 user:pass@ 里。
        assert_eq!(
            redact_url("https://user:pw@relay.cn/v1"),
            "https://relay.cn"
        );
    }

    #[test]
    fn non_url_is_not_echoed_back() {
        assert_eq!(redact_url("sk-this-is-actually-a-key"), "<非 URL>");
    }
}
