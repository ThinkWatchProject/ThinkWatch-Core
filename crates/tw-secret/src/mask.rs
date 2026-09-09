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

/// 一行配置文本里的密钥打掉，其余原样。
///
/// 用在**要给人看的那一行原文**上：解析报错时的摘录、日志里的上下文、
/// 用户复制去 issue 的片段。出错的那一行完全可能就是写着密钥的那一行，
/// 而那时我们正忙着帮他定位问题 —— 最容易忘了脱敏的时刻。
///
/// **判据是键名加值的形状，两者取或。**只看键名会漏掉
/// `url: https://user:pass@host`；只看值的形状会漏掉一把长得普通的
/// 自定义密钥，而它旁边明明写着 `key:`。
pub fn mask_line(line: &str) -> String {
    let Some(colon) = line.find(':') else {
        return line.to_string();
    };
    let (head, rest) = line.split_at(colon + 1);
    // 键名：去掉缩进和列表标记
    let name = head
        .trim_end_matches(':')
        .trim()
        .trim_start_matches("- ")
        .trim()
        .trim_matches(['"', '\''])
        .to_ascii_lowercase();

    // 值和它后面可能跟着的注释分开 —— 注释不该被打码，它是用户写的话
    let value_all = rest;
    let (value_raw, comment) = match value_all.find(" #") {
        Some(i) => (&value_all[..i], &value_all[i..]),
        None => (value_all, ""),
    };
    let value = value_raw.trim();
    if value.is_empty() {
        return line.to_string();
    }
    let quoted = value.starts_with(['"', '\'']) && value.len() >= 2;
    let bare = if quoted {
        &value[1..value.len() - 1]
    } else {
        value
    };

    let masked = if bare.contains("://") {
        // URL 里的凭据可能在 userinfo 或 path 里
        redact_url(bare)
    } else if is_secret_name(&name) || looks_like_credential(bare) {
        mask_secret(bare)
    } else {
        return line.to_string();
    };
    if masked == bare {
        return line.to_string();
    }
    let lead: String = value_raw
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect();
    let q = if quoted { &value[..1] } else { "" };
    format!("{head}{lead}{q}{masked}{q}{comment}")
}

fn is_secret_name(name: &str) -> bool {
    const NAMES: &[&str] = &[
        "key",
        "api_key",
        "apikey",
        "secret",
        "token",
        "pass",
        "password",
        "passwd",
        "credential",
        "auth",
    ];
    NAMES.contains(&name)
}

/// 值本身长得像凭据吗。**宁可多打码。**打错一个 provider 名字的代价是
/// 摘录难读一点；漏掉一把密钥的代价是它进了 issue。
fn looks_like_credential(v: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "sk-",
        "sk_",
        "tw-",
        "ghp_",
        "gho_",
        "github_pat_",
        "xoxb-",
        "AKIA",
        "AIza",
        "ya29.",
        "Bearer ",
    ];
    if PREFIXES.iter().any(|p| v.starts_with(p)) {
        return true;
    }
    // 一长串没有空格的不透明字符 —— 真实的配置值（名字、模型名、路径）
    // 很少长成这样
    v.len() >= 32
        && !v.contains(char::is_whitespace)
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.=+/".contains(c))
        && v.chars().filter(|c| c.is_ascii_digit()).count() >= 2
}

#[cfg(test)]
mod line_tests {
    use super::*;

    #[test]
    fn a_key_line_is_masked_but_stays_readable() {
        let out = mask_line("    key: sk-ant-api03-abcdefghijklmnop");
        assert!(!out.contains("abcdefghijklmnop"), "{out}");
        assert!(out.starts_with("    key: "), "缩进和键名要留着：{out}");
    }

    #[test]
    fn an_ordinary_line_is_not_touched() {
        // 过度打码会让摘录变得没法读，而那正是它存在的理由。
        for l in [
            "  - name: 官方",
            "    base_url: https://api.anthropic.com",
            "version: 1",
            "    port: 8788",
            "    protocol: anthropic",
            "  # 这是一句注释",
        ] {
            assert_eq!(mask_line(l), l, "{l} 被改了");
        }
    }

    #[test]
    fn a_url_with_credentials_in_it_loses_them() {
        // 只看键名会漏掉这个 —— 键名是 `addr`，值里才有密码。
        let out = mask_line("    addr: socks5h://alice:hunter2@127.0.0.1:7890");
        assert!(!out.contains("hunter2"), "{out}");
        assert!(out.contains("127.0.0.1:7890"), "地址本身要留着：{out}");
    }

    #[test]
    fn a_secret_shaped_value_next_to_an_innocent_key_is_still_masked() {
        // 只看键名会漏掉这个。
        let out = mask_line("    whatever: sk-ant-api03-abcdefghijklmnop");
        assert!(!out.contains("abcdefghijklmnop"), "{out}");
    }

    #[test]
    fn a_custom_looking_key_next_to_a_secret_key_name_is_still_masked() {
        // 只看值的形状会漏掉这个 —— 值长得很普通，但它旁边写着 `key:`。
        let out = mask_line("    key: 我的密钥就是这一串");
        assert!(!out.contains("我的密钥就是这一串"), "{out}");
    }

    #[test]
    fn the_trailing_comment_is_not_mangled() {
        // 注释是用户写的话，不是密钥。
        let out = mask_line("    key: sk-ant-abcdefghijklmnop   # 官方那把");
        assert!(out.ends_with("# 官方那把"), "{out}");
        assert!(!out.contains("abcdefghijklmnop"), "{out}");
    }

    #[test]
    fn quotes_around_the_value_survive() {
        let out = mask_line("    key: \"sk-ant-abcdefghijklmnop\"");
        assert!(out.contains('"'), "引号没了：{out}");
        assert!(!out.contains("abcdefghijklmnop"), "{out}");
    }

    #[test]
    fn a_multibyte_line_does_not_panic() {
        // cc-switch 栽了两次的那个坑，在这一层同样存在。
        for l in [
            "    备注: 这是一段很长很长很长的中文说明文字，里面什么都有",
            "    key: 密钥密钥密钥密钥密钥密钥密钥密钥密钥密钥密钥密钥",
            "中文键: 中文值",
            "：",
            ":",
        ] {
            let _ = mask_line(l);
        }
    }

    #[test]
    fn a_line_without_a_colon_is_returned_as_is() {
        assert_eq!(mask_line("  - just an item"), "  - just an item");
        assert_eq!(mask_line(""), "");
    }
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
