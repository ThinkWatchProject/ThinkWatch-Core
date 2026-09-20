//! 认得出哪些东西是凭据。
//!
//! **桌面场景要脱的东西和企业完全不同。**企业关心合规 —— 客户的身份证
//! 号、手机号不能流到第三方模型。个人开发者关心的是：**我的 API key、
//! 私钥、内网地址，会不会被中转站顺走。**所以这套规则是凭据导向的，
//! 一条 PII 规则都没有。
//!
//! 贯穿全文件的一条：**宁可漏，不可吵。**一个天天误报的安全功能，用户
//! 第二天就关了 ——而关掉之后，它连该抓的那次也抓不到了。所以
//! 每一条规则都要求「前缀明确」或者「结构上无法误认」，「一长串看起来
//! 随机的字符」这种判据一条都不收。

use std::ops::Range;

use serde::{Deserialize, Serialize};

/// 脱哪一类。**配置里按类别开关**，不是一条条正则。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// `sk-ant-`、`ghp_`、`AKIA` 这些前缀明确的
    ApiKeys,
    /// `-----BEGIN ... PRIVATE KEY-----` 到 END 的整段
    PrivateKeys,
    /// 三段式 base64url，且首段解出来含 `"alg"`
    Jwt,
    /// `postgres://user:pass@` 这类带口令的 URI。**只换口令那一段**
    ConnStrings,
    /// RFC1918 地址、`.local` / `.internal` 域名
    Internal,
}

impl Kind {
    pub fn slug(&self) -> &'static str {
        match self {
            Kind::ApiKeys => "api-keys",
            Kind::PrivateKeys => "private-keys",
            Kind::Jwt => "jwt",
            Kind::ConnStrings => "conn-strings",
            Kind::Internal => "internal",
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Kind::ApiKeys => "API keys",
            Kind::PrivateKeys => "private keys",
            Kind::Jwt => "JWT",
            Kind::ConnStrings => "connection-string passwords",
            Kind::Internal => "internal addresses",
        }
    }
    pub fn all() -> &'static [Kind] {
        &[
            Kind::ApiKeys,
            Kind::PrivateKeys,
            Kind::Jwt,
            Kind::ConnStrings,
            Kind::Internal,
        ]
    }
}

/// 找到的一段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub kind: Kind,
    /// 在原文里的字节区间
    pub bytes: Range<usize>,
    /// 具体是哪种凭据：`anthropic-api-key` / `private-key` / `jwt` …
    ///
    /// **给的是 slug，不是名称。**界面拿它查自己的名称表；以前这里是
    /// 「Anthropic API key」「连接串里的口令」这样的显示文字，一路原样进了
    /// 事件、数据库和界面。
    pub secret: &'static str,
}

// ---------------------------------------------------------------- API 密钥

/// 前缀明确、长度有下限的那些。
///
/// **只收前缀明确的。**代码里的 hash、base64 的图片、UUID 全都长得像
/// 「一长串随机字符」，按那个判据匹配会疯狂误报。
const PREFIXED: &[(&str, &str, usize)] = &[
    // (前缀, 哪种凭据, 前缀之后至少还要有多少个字符)
    ("sk-ant-", "anthropic-api-key", 20),
    ("sk-proj-", "openai-project-key", 20),
    ("ghp_", "github-personal-token", 30),
    ("gho_", "github-oauth-token", 30),
    ("ghs_", "github-server-token", 30),
    ("ghu_", "github-user-token", 30),
    ("github_pat_", "github-fine-grained-token", 30),
    ("xoxb-", "slack-bot-token", 20),
    ("xoxp-", "slack-user-token", 20),
    ("xoxa-", "slack-app-token", 20),
    ("AKIA", "aws-access-key-id", 12),
    ("ASIA", "aws-temporary-key-id", 12),
    ("AIza", "google-api-key", 30),
    ("ya29.", "google-oauth-token", 20),
    ("glpat-", "gitlab-token", 15),
    ("sk_live_", "stripe-live-key", 20),
    ("rk_live_", "stripe-restricted-key", 20),
    ("npm_", "npm-token", 30),
    ("dop_v1_", "digitalocean-token", 30),
    ("SG.", "sendgrid-key", 30),
];

/// `sk-` 开头的 OpenAI 老式 key。
///
/// 前缀太短，**要靠长度把它和 `sk-test`、`sk-xxx` 这种占位符分开**。
const OPENAI_MIN: usize = 40;

fn is_tok(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'
}

fn classify_token(tok: &str) -> Option<&'static str> {
    for (prefix, secret, min_tail) in PREFIXED {
        if let Some(tail) = tok.strip_prefix(prefix)
            && tail.len() >= *min_tail
        {
            return Some(secret);
        }
    }
    if let Some(tail) = tok.strip_prefix("sk-")
        && tok.len() >= OPENAI_MIN
        && !tail.starts_with("ant-")
        && !tail.starts_with("proj-")
        && tail.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        // **全是字母或者全是数字的一长串多半是别的东西**（hash、id、
        // 或者一串占位用的 aaaa）。真的 key 两样都有
        && tail.chars().any(|c| c.is_ascii_digit())
        && tail.chars().any(|c| c.is_ascii_alphabetic())
    {
        return Some("openai-api-key");
    }
    None
}

/// 把文本切成 token，回调每个 token 的区间。
///
/// **在 token 边界上匹配。**`xsk-ant-…` 里的 `sk-ant-…` 不是一把密钥，
/// 而按子串找会把它算上 —— 那种误报没法解释。
fn for_each_token(text: &str, mut f: impl FnMut(&str, Range<usize>)) {
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if is_tok(c) {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            f(&text[s..i], s..i);
        }
    }
    if let Some(s) = start {
        f(&text[s..], s..text.len());
    }
}

// ---------------------------------------------------------------- 私钥块

const PEM_BEGIN: &str = "-----BEGIN ";
const PEM_KEY: &str = "PRIVATE KEY-----";

/// `-----BEGIN ... PRIVATE KEY-----` 到对应的 END。
///
/// **整段换掉，不是只换 base64。**只换中间那段的话，模型看到的是一个
/// 空壳 PEM，它会以为文件坏了然后建议你重新生成一把 —— 那是在帮倒忙。
fn private_keys(text: &str, out: &mut Vec<Hit>) {
    let mut from = 0;
    while let Some(b) = text[from..].find(PEM_BEGIN) {
        let begin = from + b;
        // **不能按换行找那一行的结尾。**请求体是 JSON，里面的换行是
        // `\n` 两个字符，不是一个换行符 —— 按换行找的话整个 PEM 会落在
        // 「同一行」里，判据当场失效。而那是这条规则实际会遇到的唯一形态。
        // 所以按 `-----` 这个界定符找。
        let head = begin + PEM_BEGIN.len();
        let Some(h) = text[head..].find("-----") else {
            from = head;
            continue;
        };
        let head_end = head + h + 5;
        // `RSA PRIVATE KEY-----` 是，`CERTIFICATE-----` 不是
        if !text[head..head_end]
            .trim_end_matches('-')
            .trim_end()
            .ends_with("PRIVATE KEY")
        {
            from = head;
            continue;
        }
        let end = match text[head_end..].find(PEM_KEY) {
            Some(e) => head_end + e + PEM_KEY.len(),
            // **没有 END 就不动。**一个半截的 PEM 换掉之后没法还原，
            // 而且它多半是文档里的示意，不是真钥匙
            None => {
                from = head_end;
                continue;
            }
        };
        out.push(Hit {
            kind: Kind::PrivateKeys,
            bytes: begin..end,
            secret: "private-key",
        });
        from = end;
    }
}

// ---------------------------------------------------------------- JWT

fn is_b64url(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '=')
}

/// 三段式 base64url，**且首段解出来含 `"alg"`**。
///
/// 那个解码是必需的：光看「三段用点分开的 base64」会把版本号、文件路径、
/// 甚至 `a.b.c` 这种普通标识符全算上。
fn looks_like_jwt(tok: &str) -> bool {
    let parts: Vec<&str> = tok.split('.').collect();
    if parts.len() != 3 || !parts.iter().all(|p| is_b64url(p)) || parts[0].len() < 8 {
        return false;
    }
    use base64::Engine as _;
    let Ok(head) =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[0].trim_end_matches('='))
    else {
        return false;
    };
    let Ok(head) = std::str::from_utf8(&head) else {
        return false;
    };
    head.contains("\"alg\"")
}

// ---------------------------------------------------------------- 连接串

/// `scheme://user:pass@host` 里的 `pass`。
///
/// **只换口令那一段。**整条换掉的话，模型看到的是一个它读不懂的东西，
/// 而你问的可能正是「这个连接串的 host 写对了吗」 —— 原则是可逆
/// 替换而不是删除，理由完全一样：删掉语义，模型就开始瞎猜。
fn conn_strings(text: &str, out: &mut Vec<Hit>) {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(i) = text[from..].find("://") {
        let sep = from + i;
        let after = sep + 3;
        // userinfo 到 `@` 为止；`@` 必须在同一段里（不能跨空白或斜杠）
        let mut j = after;
        let mut colon: Option<usize> = None;
        while j < bytes.len() {
            match bytes[j] {
                b'@' => break,
                b':' if colon.is_none() => colon = Some(j),
                c if c.is_ascii_whitespace() || c == b'/' || c == b'"' || c == b'\'' => {
                    j = bytes.len();
                    break;
                }
                _ => {}
            }
            j += 1;
        }
        if j < bytes.len()
            && bytes[j] == b'@'
            && let Some(c) = colon
            && j > c + 1
        {
            out.push(Hit {
                kind: Kind::ConnStrings,
                bytes: c + 1..j,
                secret: "conn-string-password",
            });
        }
        from = after;
    }
}

// ---------------------------------------------------------------- 内网标识

fn is_rfc1918(tok: &str) -> bool {
    let p: Vec<&str> = tok.split('.').collect();
    if p.len() != 4 {
        return false;
    }
    let nums: Vec<u16> = match p.iter().map(|x| x.parse::<u16>()).collect::<Result<_, _>>() {
        Ok(v) => v,
        Err(_) => return false,
    };
    if nums.iter().any(|n| *n > 255) {
        return false;
    }
    match (nums[0], nums[1]) {
        (10, _) => true,
        (172, b) if (16..=31).contains(&b) => true,
        (192, 168) => true,
        _ => false,
    }
}

/// 内网地址和内部域名。
///
/// **不含回环。**`127.0.0.1` 和 `localhost` 是这台机器自己 —— 而用户问
/// 的很可能正是「我本地这个服务为什么连不上」，把它换成占位符等于把问题
/// 本身藏起来了。何况我们的网关自己就住在那儿。
fn internal_token(tok: &str) -> Option<&'static str> {
    if is_rfc1918(tok) {
        return Some("internal-ip");
    }
    let lower = tok.to_ascii_lowercase();
    if (lower.ends_with(".local") || lower.ends_with(".internal") || lower.ends_with(".lan"))
        && lower.len() > 7
        && lower.contains('.')
    {
        return Some("internal-domain");
    }
    None
}

// ---------------------------------------------------------------- 入口

/// 扫一段文本，只找 `kinds` 里点名的类别。
///
/// 返回的区间**按起点排序且互不重叠** —— 替换要从后往前做，重叠会让
/// 偏移全乱。
pub fn scan(text: &str, kinds: &[Kind]) -> Vec<Hit> {
    let mut out = Vec::new();
    if kinds.contains(&Kind::PrivateKeys) {
        private_keys(text, &mut out);
    }
    if kinds.contains(&Kind::ConnStrings) {
        conn_strings(text, &mut out);
    }
    let want_api = kinds.contains(&Kind::ApiKeys);
    let want_jwt = kinds.contains(&Kind::Jwt);
    let want_int = kinds.contains(&Kind::Internal);
    if want_api || want_jwt || want_int {
        for_each_token(text, |tok, span| {
            if want_api && let Some(secret) = classify_token(tok) {
                out.push(Hit {
                    kind: Kind::ApiKeys,
                    bytes: span,
                    secret,
                });
                return;
            }
            if want_jwt && looks_like_jwt(tok) {
                out.push(Hit {
                    kind: Kind::Jwt,
                    bytes: span,
                    secret: "jwt",
                });
                return;
            }
            if want_int && let Some(secret) = internal_token(tok) {
                out.push(Hit {
                    kind: Kind::Internal,
                    bytes: span,
                    secret,
                });
            }
        });
    }
    out.sort_by_key(|h| (h.bytes.start, h.bytes.end));
    // **重叠的只留第一个。**私钥块里的 base64 会被 token 扫描当成别的
    // 东西，两段嵌在一起替换会把偏移彻底搞乱
    let mut kept: Vec<Hit> = Vec::with_capacity(out.len());
    for h in out {
        if kept.last().is_some_and(|p| p.bytes.end > h.bytes.start) {
            continue;
        }
        kept.push(h);
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds() -> Vec<Kind> {
        Kind::all().to_vec()
    }

    fn found(text: &str) -> Vec<(Kind, String)> {
        scan(text, &kinds())
            .into_iter()
            .map(|h| (h.kind, text[h.bytes].to_string()))
            .collect()
    }

    #[test]
    fn the_prefixed_keys_are_found_whole() {
        let t = "我的 key 是 sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA，别外传";
        let got = found(t);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, Kind::ApiKeys);
        assert!(got[0].1.starts_with("sk-ant-api03-"), "{}", got[0].1);
    }

    #[test]
    fn a_key_glued_to_other_characters_is_not_a_key() {
        // 在 token 边界上匹配。`xsk-ant-…` 里的那一段不是一把密钥，
        // 而按子串找会把它算上 —— 那种误报没法解释。
        assert!(found("xsk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA").is_empty());
    }

    #[test]
    fn ordinary_code_and_docs_do_not_trip_anything() {
        // **宁可漏，不可吵。**一个天天误报的安全功能，用户第二天就关了。
        for t in [
            "把 sk-test 换成你自己的 key",
            "const hash = 'a3f5c9e1b7d2f4a6c8e0b2d4f6a8c0e2';",
            "UUID 是 550e8400-e29b-41d4-a716-446655440000",
            "版本 1.2.3，路径 a.b.c",
            "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==",
            // 全字母或全数字的一长串是 hash / id，不是 key
            "sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "sk-111111111111111111111111111111111111111111",
            "本地跑在 127.0.0.1:8788",
            "localhost:3000 连不上",
            "https://api.anthropic.com/v1/messages",
            "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----",
        ] {
            assert!(found(t).is_empty(), "误报了：{t} → {:?}", found(t));
        }
    }

    #[test]
    fn a_private_key_block_is_taken_whole() {
        // 只换中间那段 base64 的话，模型看到一个空壳 PEM 会以为文件坏了，
        // 然后建议你重新生成一把 —— 那是在帮倒忙。
        let t = "配置里有：\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow\nAAAA\n-----END RSA PRIVATE KEY-----\n然后呢";
        let got = found(t);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, Kind::PrivateKeys);
        assert!(got[0].1.starts_with("-----BEGIN RSA"), "{}", got[0].1);
        assert!(got[0].1.ends_with("PRIVATE KEY-----"), "{}", got[0].1);
    }

    #[test]
    fn a_pem_inside_a_json_body_is_found_even_though_the_newlines_are_escaped() {
        // **请求体是 JSON**，里面的换行是 `\n` 两个字符。按真换行找
        // 「BEGIN 那一行」的话，整个 PEM 会落在同一行里，判据当场失效
        // —— 而这是这条规则实际会遇到的唯一形态。
        let body = r#"{"messages":[{"content":"看看这个：-----BEGIN RSA PRIVATE KEY-----\nMIIEow\nAAAA\n-----END RSA PRIVATE KEY-----\n好吗"}]}"#;
        let got = found(body);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, Kind::PrivateKeys);
        assert!(got[0].1.starts_with("-----BEGIN RSA"), "{}", got[0].1);
        assert!(got[0].1.ends_with("PRIVATE KEY-----"), "{}", got[0].1);
        // 换掉之后 JSON 还得是合法的
        let r = crate::redact::redact(body, &kinds());
        serde_json::from_str::<serde_json::Value>(&r.text).expect("换完不是合法 JSON");
        // 而且能一字不差地换回来
        assert_eq!(crate::redact::restore(&r.text, &r.ledger), body);
    }

    #[test]
    fn a_certificate_is_not_a_private_key() {
        assert!(
            found(r#"{"c":"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----"}"#)
                .is_empty()
        );
    }

    #[test]
    fn an_unterminated_pem_is_left_alone() {
        // 半截的 PEM 换掉之后没法还原，而且它多半是文档里的示意。
        assert!(found("-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n（略）").is_empty());
    }

    #[test]
    fn a_jwt_is_recognised_only_after_decoding_its_header() {
        // 光看「三段用点分开的 base64」会把版本号、路径、`a.b.c` 全算上。
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxIn0.abcdefghijk";
        let got = found(&format!("Authorization: Bearer {jwt}"));
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, Kind::Jwt);
        assert_eq!(got[0].1, jwt);
        // 三段 base64 但头部不是 JWT 头 —— 不算
        assert!(found("aaaaaaaa.bbbbbbbb.cccccccc").is_empty());
    }

    #[test]
    fn only_the_password_part_of_a_connection_string_is_taken() {
        // 你问的可能正是「这个 host 写对了吗」。删掉语义，模型就开始瞎猜。
        let t = "DATABASE_URL=postgres://admin:hunter2@db.example.com:5432/app";
        let got = found(t);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, Kind::ConnStrings);
        assert_eq!(got[0].1, "hunter2");
    }

    #[test]
    fn a_url_without_a_password_is_not_a_connection_string() {
        for t in [
            "https://example.com/path",
            "postgres://db.example.com:5432/app",
            "git@github.com:me/repo.git",
            "见 https://user@example.com/x",
        ] {
            assert!(
                !found(t).iter().any(|(k, _)| *k == Kind::ConnStrings),
                "误报了：{t} → {:?}",
                found(t)
            );
        }
    }

    #[test]
    fn private_network_addresses_are_found_but_loopback_is_not() {
        // **不含回环。**用户问的很可能正是「我本地这个服务为什么连不上」，
        // 把它换成占位符等于把问题本身藏起来。
        let got = found("内网 10.1.2.3 和 192.168.0.5，还有 172.20.1.1");
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(got.iter().all(|(k, _)| *k == Kind::Internal));
        assert!(found("127.0.0.1 和 8.8.8.8 和 172.32.0.1").is_empty());
    }

    #[test]
    fn only_the_kinds_that_were_asked_for_are_scanned() {
        // 核心：**走官方端点不该脱敏，走中转站才脱。**
        let t = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA 和 10.0.0.1";
        assert_eq!(scan(t, &[]).len(), 0);
        assert_eq!(scan(t, &[Kind::ApiKeys]).len(), 1);
        assert_eq!(scan(t, &[Kind::Internal]).len(), 1);
        assert_eq!(scan(t, &[Kind::ApiKeys, Kind::Internal]).len(), 2);
    }

    #[test]
    fn overlapping_hits_keep_only_the_outer_one() {
        // 私钥块里的 base64 会被 token 扫描当成别的东西，嵌在一起替换会
        // 把偏移彻底搞乱。
        let t = "-----BEGIN OPENSSH PRIVATE KEY-----\nsk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA\n-----END OPENSSH PRIVATE KEY-----";
        let got = scan(t, &kinds());
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].kind, Kind::PrivateKeys);
    }

    #[test]
    fn the_spans_can_actually_be_used_to_slice_multibyte_text() {
        // 这个项目已经被字节切片坑过三次。命中点前后全是中文。
        let t = "这是一段很长的中文说明，里面混着一把 sk-ant-api03-BBBBBBBBBBBBBBBBBBBBBBBBBB，后面继续写中文";
        let got = scan(t, &kinds());
        assert_eq!(got.len(), 1);
        assert!(
            t[got[0].bytes.clone()].starts_with("sk-ant-"),
            "{}",
            &t[got[0].bytes.clone()]
        );
    }

    #[test]
    fn the_hits_come_back_sorted_and_disjoint() {
        // 替换要从后往前做，重叠或乱序会让偏移全乱。
        let t = "10.0.0.1 然后 sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA 然后 192.168.1.1";
        let got = scan(t, &kinds());
        assert_eq!(got.len(), 3);
        for w in got.windows(2) {
            assert!(w[0].bytes.end <= w[1].bytes.start, "{got:?}");
        }
    }
}
