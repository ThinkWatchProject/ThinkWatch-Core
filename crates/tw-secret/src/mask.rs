//! 打码。所有会被人看见的地方 —— 日志、UI、错误信息、导出、剪贴板 ——
//! 都必须过这里，而不是各写一遍。

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
        return "<not a URL>".to_string();
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
    let out = mask_line_by_field(line);
    // **兜底：不在「值」位置上的凭据 URL 也要打掉。**
    //
    // 按字段名那一层是按第一个 `:` 切的，而注释行
    // `# 上游换成了 https://bob:hunter2@relay/v1` 的第一个 `:` 在
    // `https` 后面 —— 于是键名是「# 上游换成了 https」，不是密钥名，
    // 整行原样放过。**而诊断包是带着注释一起交出去的。**
    if has_userinfo(&out) {
        redact_urls_within(&out)
    } else {
        out
    }
}

fn mask_line_by_field(line: &str) -> String {
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

    let masked = if is_url(bare) {
        // 值本身就是个 URL：凭据可能在 userinfo 或 path 里
        redact_url(bare)
    } else if has_userinfo(bare) {
        // **只是「值里面带了个 URL」，不是「值是个 URL」。**整段按 URL
        // 处理会把值的其余部分一起截掉，而那部分往往正是排查时要看的
        // 东西（`https://a.example.com/v1 那个` 会只剩前半句）。
        //
        // **路径不打，userinfo 打** —— 凭据在 `user:pass@` 里，而路径里
        // 真藏了密钥的话，下面按形状那一层还认得出来。
        redact_urls_within(bare)
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

/// `headers:` 段里的一行：公开的头和只由引用组成的值原样留着，其余打码。
fn mask_header_line(line: &str) -> String {
    let Some(colon) = line.find(':') else {
        return line.to_string();
    };
    let (head, rest) = line.split_at(colon + 1);
    let name = head.trim_end_matches(':').trim().trim_matches(['"', '\'']);
    let (value_raw, comment) = match rest.find(" #") {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let value = value_raw.trim();
    let quoted = value.starts_with(['"', '\'']) && value.len() >= 2;
    let bare = if quoted {
        &value[1..value.len() - 1]
    } else {
        value
    };
    if bare.is_empty() || is_public_header(name) || is_reference_only(bare) {
        return line.to_string();
    }
    let q = if quoted { &value[..1] } else { "" };
    format!("{head} {q}{}{q}{comment}", mask_secret(bare))
}

/// 值不是秘密的请求头。**只列确定的** —— 名单外的一律当密钥处理，
/// 打错一个的代价是界面上多一处打码，漏掉一个的代价是密钥进了诊断包。
pub fn is_public_header(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "anthropic-version"
            | "anthropic-beta"
            | "openai-organization"
            | "openai-project"
            | "openai-beta"
            | "user-agent"
            | "http-referer"
            | "referer"
            | "x-title"
            | "accept"
            | "accept-language"
            | "content-type"
    )
}

/// 值里只有环境变量引用、占位符，加上一个短前缀（`Bearer ${TOKEN}`、
/// `{{access_token}}`）—— 这样的值里没有秘密可泄。
pub fn is_reference_only(value: &str) -> bool {
    if !value.contains("${") && !value.contains("{{") {
        return false;
    }
    let mut rest = String::new();
    let mut s = value;
    loop {
        let next = [("${", "}"), ("{{", "}}")]
            .iter()
            .filter_map(|(open, close)| s.find(open).map(|i| (i, *open, *close)))
            .min_by_key(|(i, _, _)| *i);
        let Some((i, open, close)) = next else {
            rest.push_str(s);
            break;
        };
        rest.push_str(&s[..i]);
        match s[i + open.len()..].find(close) {
            Some(j) => s = &s[i + open.len() + j + close.len()..],
            // 没闭合：不是引用，剩下的全算文字
            None => {
                rest.push_str(&s[i..]);
                break;
            }
        }
    }
    let rest = rest.trim();
    // 前缀只认字母和空格（`Bearer `、`Token `）：`sk-live-${SUFFIX}` 里那段
    // `sk-live-` 就是密钥的一部分
    rest.len() <= 16 && rest.chars().all(|c| c.is_ascii_alphabetic() || c == ' ')
}

/// 这个值本身是不是一个 URL（而不是「一段包含 URL 的文本」）。
fn is_url(v: &str) -> bool {
    let Some(i) = v.find("://") else {
        return false;
    };
    !v[..i].is_empty()
        && v[..i]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
}

/// 这段文本里有没有 `scheme://user:pass@` 这样带凭据的 URL。
fn has_userinfo(text: &str) -> bool {
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        let after = &rest[i + 3..];
        let host_end = after
            .find(|c: char| c.is_whitespace() || matches!(c, '/' | '?' | '#' | '"' | '\'' | ','))
            .unwrap_or(after.len());
        if after[..host_end].contains('@') {
            return true;
        }
        rest = after;
    }
    false
}

/// 把一段文本里**带 userinfo 的** URL 各自打码，其余原样留着。
fn redact_urls_within(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        // scheme 是 `://` 前面那串合法字符，往前找到边界
        let head = &rest[..i];
        let scheme_start = head
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')))
            .map(|p| p + 1)
            .unwrap_or(0);
        out.push_str(&rest[..scheme_start]);
        let url_part = &rest[scheme_start..];
        // URL 到第一个空白或引号为止
        let end = url_part
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ']' | ')'))
            .unwrap_or(url_part.len());
        let one = &url_part[..end];
        // 没有 userinfo 的那些原样留着 —— 见调用处的注释
        if has_userinfo(one) {
            out.push_str(&redact_url(one));
        } else {
            out.push_str(one);
        }
        rest = &url_part[end..];
    }
    out.push_str(rest);
    out
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
        // OAuth 的四个。**`refresh` 是这里面最值钱的** ——
        // 它换得出无数个 access token，所以泄漏它比泄漏 access 严重。
        // 少了这几个名字，一份 OAuth 凭据在配置报错摘录里会原样进
        // 界面和日志（实测过）
        "refresh",
        "refresh_token",
        "access",
        "access_token",
        "client_secret",
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

/// 整份 config.yaml 打码：**逐行走 `mask_line`，再补上块标量**。
///
/// # 为什么不能只用 `mask_body`
///
/// `mask_body` 认的是**值的形状**（`sk-ant-…`、`ghp_…`）。而配置文件里
/// 有一类密钥根本没有形状 —— 实测这三个都原样穿过 `mask_body`：
///
/// ```yaml
/// key: mycompany-internal-token-9911      # 自建中转的 key
/// oauth:
///   refresh: 1//0gLdOPAQUE                 # OAuth 的 refresh token
///   client_secret: cs-whatever
/// ```
///
/// 而诊断包的第一句话是「这份内容里的密钥都已经打码」—— **一个做不到
/// 的承诺比不承诺更危险**，因为它正是用户决定「可以贴到 issue 里」的
/// 依据。
///
/// # 为什么复用 `mask_line` 而不自己写一套
///
/// 引号、行尾注释、URL 里的 userinfo，这三样它都已经处理对了。**两处
/// 标准不同的话，仔细的那一处等于白做** —— 这个函数第一版真的
/// 自己写了一套引号和注释规则，那是错的。
///
/// 这一层只加 `mask_line` 看不见的那件事：`key: |` 之后的几行。
///
pub fn mask_config_yaml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    // 块标量（`key: |`）的内容在后面几行上。`mask_line` 只看一行，看不见
    // 这种情况 —— 记住宿主那一层的缩进，把整块吃掉
    let mut block: Option<usize> = None;
    // 上游的 `headers:` 那一段。**请求头名说明不了值是不是秘密**
    // （`X-Relay-Token`），所以这一段里按请求头的规矩打码，不按字段名
    let mut headers: Option<usize> = None;
    for line in text.lines() {
        let indent = line.len() - line.trim_start().len();
        if let Some(owner) = block {
            if line.trim().is_empty() || indent > owner {
                out.push_str("…\n");
                continue;
            }
            block = None;
        }
        if let Some(owner) = headers {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                out.push_str(line);
                out.push('\n');
                continue;
            }
            if indent > owner {
                out.push_str(&mask_header_line(line));
                out.push('\n');
                continue;
            }
            headers = None;
        }
        if let Some((name, rest)) = line.trim_start().trim_start_matches("- ").split_once(':')
            && name.trim() == "headers"
        {
            let value = rest.split(" #").next().unwrap_or("").trim();
            if value.is_empty() {
                headers = Some(indent);
            } else {
                // 流式写法（`headers: { X-Token: t }`）整段打掉
                let head = &line[..line.find(':').unwrap_or(0) + 1];
                out.push_str(&format!("{head} {{…}}\n"));
                continue;
            }
        }
        // **块标量要在打码之前认出来。**`mask_line` 会把 `|` 自己也当成
        // 一个值打掉（变成 `key: …`），那之后就再也看不出这里开了个块 ——
        // 而密钥在下面几行上
        if let Some((name, rest)) = line.trim_start().trim_start_matches("- ").split_once(':')
            && is_secret_name(&name.trim().trim_matches(['"', '\'']).to_ascii_lowercase())
            && matches!(rest.trim().chars().next(), Some('|') | Some('>'))
        {
            block = Some(indent);
        }
        out.push_str(&mask_line(line));
        out.push('\n');
    }
    // 形状认得出来的照旧再过一遍 —— 密钥也可能出现在注释里，或者某个
    // 我们没把它当密钥的字段上。**两层是叠加的，不是二选一**
    mask_body(&out)
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
        assert_eq!(redact_url("sk-this-is-actually-a-key"), "<not a URL>");
    }
}

/// 一整段 body 里的密钥打掉。
///
/// 请求体里有 system prompt、工具定义，有时还有用户自己粘进去的密钥；
/// 而这段文字会出现在详情抽屉里、被复制到 issue 里（统一脱敏）。
///
/// **按值的形状判，不按键名。**body 是 JSON，键名五花八门（`api_key`、
/// `token`、`Authorization`、某个 MCP server 自己起的名字），而凭据的
/// 形状是有限的几种。
pub fn mask_body(text: &str) -> String {
    // 一个可能是凭据的 token 由这些字符组成
    fn is_tok(c: char) -> bool {
        c.is_ascii_alphanumeric() || "-_.".contains(c)
    }
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // 只在一个 token 的开头尝试匹配，否则 `xsk-abc` 里的 `sk-abc`
        // 会被当成密钥
        let at_boundary = i == 0 || !is_tok(chars[i - 1]);
        if at_boundary {
            let mut j = i;
            while j < chars.len() && is_tok(chars[j]) {
                j += 1;
            }
            let tok: String = chars[i..j].iter().collect();
            if looks_like_credential(&tok) {
                out.push_str(&mask_secret(&tok));
                i = j;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod body_tests {
    use super::*;

    #[test]
    fn a_key_pasted_into_a_request_body_is_masked() {
        // 用户在对话里粘一把 key 是常事（「帮我看看这个配置」）。而这段
        // 文字会出现在详情抽屉、被复制到 issue 里。
        let b = r#"{"messages":[{"role":"user","content":"我的 key 是 sk-ant-api03-abcdefghijklmnopqrstuvwxyz"}]}"#;
        let out = mask_body(b);
        assert!(!out.contains("abcdefghijklmnop"), "{out}");
        assert!(out.contains("messages"), "结构被打坏了：{out}");
    }

    #[test]
    fn ordinary_prose_and_code_survive_intact() {
        // **过度打码会让详情抽屉变得没法读**，而那正是它存在的理由。
        for s in [
            r#"{"model":"claude-sonnet-4-5","max_tokens":8000}"#,
            r#"{"content":"请帮我重构 src/main.rs 里的 handle_request 函数"}"#,
            r#"{"tools":[{"name":"read_file","description":"读一个文件"}]}"#,
            "for (let i = 0; i < 10; i++) { console.log(i); }",
        ] {
            assert_eq!(mask_body(s), s, "被改了：{s}");
        }
    }

    #[test]
    fn a_credential_glued_to_other_characters_is_not_half_masked() {
        // `xsk-abc…` 里的 `sk-abc…` 不是一把密钥。只在 token 边界上匹配。
        let s = "看看 xsk-ant-api03-abcdefghijklmnop 这个变量名";
        assert_eq!(mask_body(s), s);
    }

    #[test]
    fn several_keys_in_one_body_are_all_masked() {
        let s = "sk-ant-api03-aaaaaaaaaaaaaaaa 和 ghp_bbbbbbbbbbbbbbbbbbbb";
        let out = mask_body(s);
        assert!(!out.contains("aaaaaaaaaaaaaaaa"), "{out}");
        assert!(!out.contains("bbbbbbbbbbbbbbbb"), "{out}");
    }

    #[test]
    fn the_config_secrets_that_have_no_recognisable_shape_are_still_masked() {
        // **这三种实测都能原样穿过 `mask_body`。**它们是这个函数存在的理由
        let out = mask_config_yaml(
            "providers:\n  - name: b\n    key: mycompany-internal-token-9911\n  - name: c\n    oauth:\n      refresh: 1//0gLdOPAQUE-REFRESH\n      client_secret: cs-OPAQUE-SECRET\n      endpoint: https://auth.example.com/token\n",
        );
        assert!(
            !out.contains("internal-token-9911"),
            "自建中转的 key 漏了：{out}"
        );
        assert!(!out.contains("OPAQUE-REFRESH"), "refresh token 漏了：{out}");
        assert!(!out.contains("OPAQUE-SECRET"), "client_secret 漏了：{out}");
        // 不是密钥的东西要留着，否则诊断包就没用了
        assert!(out.contains("auth.example.com"), "端点被打掉了：{out}");
        assert!(out.contains("name: b"), "{out}");
    }

    #[test]
    fn a_credential_bearing_url_inside_a_longer_value_is_still_masked() {
        // 一段话里夹着一个带凭据的 URL。**userinfo 打，主机名留着** ——
        // 打掉主机名的话这条注释就没有意义了
        let out = mask_config_yaml("    # 上游换成了 https://bob:hunter2@relay.example.com/v1\n");
        assert!(!out.contains("hunter2"), "URL 里的凭据漏了：{out}");
        assert!(out.contains("relay.example.com"), "主机名不该打掉：{out}");
    }

    #[test]
    fn a_value_that_merely_contains_a_url_is_not_truncated_at_the_url() {
        // 看见 `://` 就把整段按 URL 处理的话，这条注释会在第一个 URL
        // 处被截断 —— 而**后半句往往正是排查时要看的东西**
        let out = mask_config_yaml("    # 从 https://a.example.com/v1 换到了 /v2，别忘了\n");
        assert!(out.contains("别忘了"), "后半句被截掉了：{out}");
        assert!(out.contains("/v1"), "路径不该被打掉：{out}");
    }

    #[test]
    fn a_trailing_comment_survives_but_the_value_does_not() {
        let out = mask_config_yaml("    key: sk-plain-value-here # 公司那个号\n");
        assert!(out.contains("公司那个号"), "注释被吃了：{out}");
        assert!(!out.contains("plain-value-here"), "{out}");
    }

    #[test]
    fn a_block_scalar_secret_does_not_leak_through_its_following_lines() {
        // 一行一行看的话，`key: |` 这一行没有值，而密钥在下面几行上
        let out = mask_config_yaml(
            "providers:\n  - name: a\n    key: |\n      sk-multiline-SECRET-HERE\n      second-line-SECRET\n    base_url: https://x.example.com\n",
        );
        assert!(!out.contains("SECRET"), "块标量里的密钥漏了：{out}");
        // 块结束之后的字段要回到正常
        assert!(out.contains("base_url: https://x.example.com"), "{out}");
    }

    #[test]
    fn a_quoted_value_stays_quoted_so_the_yaml_still_reads_as_yaml() {
        let out = mask_config_yaml("    key: \"sk-quoted-SECRET-value\"\n");
        assert!(!out.contains("SECRET"), "{out}");
        assert!(
            out.contains('"'),
            "引号没了，粘回去就不是合法 YAML 了：{out}"
        );
    }

    #[test]
    fn a_field_that_merely_ends_in_key_is_not_a_secret() {
        // `api_key_env` 之类装的是环境变量名，不是密钥。多打码的代价小，
        // 但也不能把整个文件都打掉
        let out = mask_config_yaml(
            "    monkey: not-a-secret-value\n    base_url: https://a.example.com\n",
        );
        assert!(out.contains("not-a-secret-value"), "{out}");
    }

    #[test]
    fn a_multibyte_config_does_not_panic() {
        for s in [
            "key: 中文密钥中文密钥\n",
            "  - name: 上游一号\n    key: |\n      🙂🙂🙂\n",
            "",
            "key:\n",
        ] {
            let _ = mask_config_yaml(s);
        }
    }

    #[test]
    fn a_multibyte_body_does_not_panic() {
        // 按字节切 &str 的坑在这一层同样存在。
        for s in [
            "中文中文中文 sk-ant-api03-abcdefghijklmnop 中文",
            "🙂🙂🙂",
            "",
            "日本語のテキストです",
        ] {
            let _ = mask_body(s);
        }
    }
}

#[cfg(test)]
mod header_tests {
    use super::*;

    #[test]
    fn header_values_in_a_config_are_masked_unless_public_or_references() {
        let cfg = "providers:\n  - name: relay\n    base_url: https://relay.example\n    headers:\n      X-Relay-Token: rt-verysecretvalue123\n      anthropic-version: 2023-06-01\n      Authorization: Bearer ${RELAY_TOKEN}\n      X-Tenant: \"team-alpha-verysecret\" # 注释留着\n    proxy: direct\n";
        let out = mask_config_yaml(cfg);
        assert!(!out.contains("verysecretvalue123"), "{out}");
        assert!(!out.contains("team-alpha-verysecret"), "{out}");
        assert!(out.contains("anthropic-version: 2023-06-01"), "{out}");
        assert!(out.contains("Bearer ${RELAY_TOKEN}"), "{out}");
        assert!(out.contains("# 注释留着"), "{out}");
        // 段落结束之后恢复按字段名打码的规矩
        assert!(out.contains("proxy: direct"), "{out}");
    }

    #[test]
    fn a_flow_style_headers_map_is_masked_whole() {
        let out = mask_config_yaml("    headers: { X-Token: t-verysecretvalue }\n");
        assert!(!out.contains("verysecret"), "{out}");
    }

    #[test]
    fn reference_only_values_are_recognised() {
        assert!(is_reference_only("${KEY}"));
        assert!(is_reference_only("Bearer ${KEY}"));
        assert!(is_reference_only("Token {{access_token}}"));
        assert!(!is_reference_only("sk-live-${SUFFIX}"));
        assert!(!is_reference_only("plain-secret"));
    }
}
