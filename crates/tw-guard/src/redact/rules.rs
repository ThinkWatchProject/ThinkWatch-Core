//! 认得出哪些东西是凭据。
//!
//! **桌面场景要脱的东西和企业完全不同。**企业关心合规 —— 客户的身份证
//! 号、手机号不能流到第三方模型。个人开发者关心的是：**我的 API key、
//! 私钥、内网地址，会不会被中转站顺走。**所以这套规则是凭据导向的，
//! 一条 PII 规则都没有。
//!
//! 贯穿全文件的一条：**宁可漏，不可吵。**一个天天误报的安全功能，用户
//! 第二天就关了 ——而关掉之后，它连该抓的那次也抓不到了。所以
//! 每一条内置规则都要求「前缀明确」或者「结构上无法误认」，「一长串看起来
//! 随机的字符」这种判据一条都不收。
//!
//! # 规则是一条一条开关的
//!
//! 界面上每条规则都看得见、关得掉：误报的那一条关掉，别的照常工作。以前
//! 只能按五个类别整类开关，于是一条前缀太短的误报能让人关掉整类 API 密钥。
//! 类别（[`Kind`]）留着，只用来分组和说明。
//!
//! 用户还可以写自己的规则（正则）。它们和内置规则在同一遍里找、同一本账
//! 里换，于是「同一个值只占一个编号」这类纪律对它们同样成立。

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// 规则属于哪一类。**只用来分组和说明**，开关按条。
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
    /// 用户自己写的规则
    Custom,
}

impl Kind {
    pub fn slug(&self) -> &'static str {
        match self {
            Kind::ApiKeys => "api-keys",
            Kind::PrivateKeys => "private-keys",
            Kind::Jwt => "jwt",
            Kind::ConnStrings => "conn-strings",
            Kind::Internal => "internal",
            Kind::Custom => "custom",
        }
    }
}

/// 一条内置规则按什么认。**给界面说明用** —— 真正的判据在下面的函数里，
/// 两者由测试钉在一起（`every_builtin_rule_describes_what_it_matches`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matcher {
    /// 以 `prefix` 开头，其后至少还有 `min_tail` 个字符
    Prefix {
        prefix: &'static str,
        min_tail: usize,
    },
    /// `sk-` 开头的 OpenAI 老式密钥：全长至少 `min_len`，字母和数字都有
    OpenaiLegacy { min_len: usize },
    /// PEM 私钥块，BEGIN 到对应的 END 整段
    Pem,
    /// 三段 base64url，首段解码后含 `"alg"`
    Jwt,
    /// `协议://用户:口令@主机` 里的口令
    ConnString,
    /// RFC1918 私有地址，不含回环
    PrivateIp,
    /// 以这几个后缀结尾的域名
    DomainSuffix { suffixes: &'static [&'static str] },
}

/// 一条内置规则。
#[derive(Debug, Clone, Copy)]
pub struct Builtin {
    /// 规则的 id，也是命中之后报出去的那个词（`anthropic-api-key` …）
    pub id: &'static str,
    pub kind: Kind,
    /// 英文名。界面按 id 查自己的名称表，查不到才用它
    pub name: &'static str,
    /// 出厂时开不开。**只有内网地址那两条是关的**：RFC1918 地址在代码和
    /// 文档里到处都是，而它的危害远小于一把 key，想脱的人自己开
    pub on_by_default: bool,
    pub matcher: Matcher,
}

const fn prefix(
    id: &'static str,
    name: &'static str,
    prefix: &'static str,
    min_tail: usize,
) -> Builtin {
    Builtin {
        id,
        kind: Kind::ApiKeys,
        name,
        on_by_default: true,
        matcher: Matcher::Prefix { prefix, min_tail },
    }
}

/// `sk-` 开头的 OpenAI 老式 key 至少要多长。
///
/// 前缀太短，**要靠长度把它和 `sk-test`、`sk-xxx` 这种占位符分开**。
const OPENAI_MIN: usize = 40;

const INTERNAL_SUFFIXES: &[&str] = &[".local", ".internal", ".lan"];

/// 全部内置规则，**按界面上的顺序**。
///
/// API 密钥那一段**只收前缀明确、长度有下限的**。代码里的 hash、base64 的
/// 图片、UUID 全都长得像「一长串随机字符」，按那个判据匹配会疯狂误报。
pub const BUILTINS: &[Builtin] = &[
    prefix("anthropic-api-key", "Anthropic API key", "sk-ant-", 20),
    prefix("openai-project-key", "OpenAI project key", "sk-proj-", 20),
    Builtin {
        id: "openai-api-key",
        kind: Kind::ApiKeys,
        name: "OpenAI API key",
        on_by_default: true,
        matcher: Matcher::OpenaiLegacy {
            min_len: OPENAI_MIN,
        },
    },
    prefix(
        "github-personal-token",
        "GitHub personal access token",
        "ghp_",
        30,
    ),
    prefix("github-oauth-token", "GitHub OAuth token", "gho_", 30),
    prefix("github-server-token", "GitHub server token", "ghs_", 30),
    prefix("github-user-token", "GitHub user token", "ghu_", 30),
    prefix(
        "github-fine-grained-token",
        "GitHub fine-grained token",
        "github_pat_",
        30,
    ),
    prefix("slack-bot-token", "Slack bot token", "xoxb-", 20),
    prefix("slack-user-token", "Slack user token", "xoxp-", 20),
    prefix("slack-app-token", "Slack app token", "xoxa-", 20),
    prefix("aws-access-key-id", "AWS access key ID", "AKIA", 12),
    prefix(
        "aws-temporary-key-id",
        "AWS temporary access key ID",
        "ASIA",
        12,
    ),
    prefix("google-api-key", "Google API key", "AIza", 30),
    prefix("google-oauth-token", "Google OAuth token", "ya29.", 20),
    prefix("gitlab-token", "GitLab token", "glpat-", 15),
    prefix("stripe-live-key", "Stripe live key", "sk_live_", 20),
    prefix(
        "stripe-restricted-key",
        "Stripe restricted key",
        "rk_live_",
        20,
    ),
    prefix("npm-token", "npm token", "npm_", 30),
    prefix("digitalocean-token", "DigitalOcean token", "dop_v1_", 30),
    prefix("sendgrid-key", "SendGrid key", "SG.", 30),
    Builtin {
        id: "private-key",
        kind: Kind::PrivateKeys,
        name: "Private key",
        on_by_default: true,
        matcher: Matcher::Pem,
    },
    Builtin {
        id: "jwt",
        kind: Kind::Jwt,
        name: "JWT",
        on_by_default: true,
        matcher: Matcher::Jwt,
    },
    Builtin {
        id: "conn-string-password",
        kind: Kind::ConnStrings,
        name: "Connection string password",
        on_by_default: true,
        matcher: Matcher::ConnString,
    },
    Builtin {
        id: "internal-ip",
        kind: Kind::Internal,
        name: "Internal IP address",
        on_by_default: false,
        matcher: Matcher::PrivateIp,
    },
    Builtin {
        id: "internal-domain",
        kind: Kind::Internal,
        name: "Internal domain",
        on_by_default: false,
        matcher: Matcher::DomainSuffix {
            suffixes: INTERNAL_SUFFIXES,
        },
    },
];

pub fn builtin(id: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.id == id)
}

/// 命中的是哪一条规则。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Rule {
    Builtin(&'static str),
    /// 用户的规则，按名字认
    Custom(Arc<str>),
}

impl Rule {
    /// 内置规则的 id，或者自定义规则的名字
    pub fn id(&self) -> &str {
        match self {
            Rule::Builtin(id) => id,
            Rule::Custom(name) => name,
        }
    }
    pub fn custom(&self) -> bool {
        matches!(self, Rule::Custom(_))
    }
    pub fn kind(&self) -> Kind {
        match self {
            Rule::Builtin(id) => builtin(id).map_or(Kind::ApiKeys, |b| b.kind),
            Rule::Custom(_) => Kind::Custom,
        }
    }
}

/// 找到的一段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// 在原文里的字节区间
    pub bytes: Range<usize>,
    pub rule: Rule,
    /// 占位符里用的标签。`None` 用账本的默认标签
    pub label: Option<Arc<str>>,
}

/// 一条编译好的自定义规则。
#[derive(Debug, Clone)]
pub struct Custom {
    pub name: Arc<str>,
    pub re: regex::Regex,
    /// 占位符里的标签（企业版的 `EMAIL`、`PHONE`）。`None` 用账本的默认标签
    pub label: Option<Arc<str>>,
}

/// 一个自定义规则的正则写错了。
#[derive(Debug, Clone, thiserror::Error)]
#[error("the pattern of rule `{name}` is not a valid regular expression: {detail}")]
pub struct BadPattern {
    pub name: String,
    pub detail: String,
}

/// 编一条正则。**长度和编译后的大小都有上限** —— 这条正则要在每个请求体
/// 上跑，一个写得很大的正则不该拖慢每一个请求。
pub fn compile(name: &str, pattern: &str) -> Result<regex::Regex, BadPattern> {
    if pattern.is_empty() {
        return Err(BadPattern {
            name: name.to_string(),
            detail: "the pattern is empty".to_string(),
        });
    }
    crate::bounded(pattern).map_err(|e| BadPattern {
        name: name.to_string(),
        detail: e.to_string(),
    })
}

/// 这一次按哪些规则找。
///
/// **运行时建一次、跟着配置一起换**，不在每个请求上现编正则。
#[derive(Debug, Clone)]
pub struct RuleSet {
    on: HashSet<&'static str>,
    custom: Vec<Custom>,
}

impl Default for RuleSet {
    fn default() -> Self {
        Self::defaults()
    }
}

impl RuleSet {
    /// 出厂时的样子：内置规则按默认开关，没有自定义。
    pub fn defaults() -> Self {
        Self {
            on: BUILTINS
                .iter()
                .filter(|b| b.on_by_default)
                .map(|b| b.id)
                .collect(),
            custom: Vec::new(),
        }
    }

    /// 一条都不开。测试和「只试这一条正则」用。
    pub fn none() -> Self {
        Self {
            on: HashSet::new(),
            custom: Vec::new(),
        }
    }

    /// 只开这几条内置规则。
    pub fn only(ids: &[&str]) -> Self {
        Self {
            on: BUILTINS
                .iter()
                .filter(|b| ids.contains(&b.id))
                .map(|b| b.id)
                .collect(),
            custom: Vec::new(),
        }
    }

    /// 按配置建：在默认之上打开 `enable`、关掉 `disable`，再加上启用着的
    /// 自定义规则。
    ///
    /// 认不出的 id 在配置校验时就拒绝了（`tw_config::validate`），到不了这里。
    pub fn build<'a>(
        enable: &[String],
        disable: &[String],
        custom: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, BadPattern> {
        let mut set = Self::defaults();
        for b in enable.iter().filter_map(|id| builtin(id)) {
            set.on.insert(b.id);
        }
        for b in disable.iter().filter_map(|id| builtin(id)) {
            set.on.remove(b.id);
        }
        for (name, pattern) in custom {
            set.custom.push(Custom {
                name: Arc::from(name),
                re: compile(name, pattern)?,
                label: None,
            });
        }
        Ok(set)
    }

    /// 再加一条自定义规则。
    pub fn with_custom(self, name: &str, pattern: &str) -> Result<Self, BadPattern> {
        self.with_labeled(name, pattern, None)
    }

    /// 再加一条自定义规则，占位符用它自己的标签：`{{EMAIL_1}}` 里的 `EMAIL`。
    pub fn with_labeled(
        mut self,
        name: &str,
        pattern: &str,
        label: Option<&str>,
    ) -> Result<Self, BadPattern> {
        self.custom.push(Custom {
            name: Arc::from(name),
            re: compile(name, pattern)?,
            label: label.map(Arc::from),
        });
        Ok(self)
    }

    pub fn is_on(&self, id: &str) -> bool {
        self.on.contains(id)
    }

    /// 一条都没开。**调用方据此整条短路** —— 没有规则的话，找都不用找。
    pub fn is_empty(&self) -> bool {
        self.on.is_empty() && self.custom.is_empty()
    }
}

fn is_tok(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'
}

/// 一段 token 是哪种 API 密钥。**只看开着的那几条。**
fn classify_token(tok: &str, set: &RuleSet) -> Option<&'static str> {
    for b in BUILTINS {
        if let Matcher::Prefix { prefix, min_tail } = b.matcher
            && let Some(tail) = tok.strip_prefix(prefix)
            && tail.len() >= min_tail
        {
            // 前缀认出来了但这条关着 —— **不再往下猜**。`sk-ant-…` 关掉之后
            // 不该被当成一把 OpenAI 老式 key 换掉
            return set.is_on(b.id).then_some(b.id);
        }
    }
    if set.is_on("openai-api-key")
        && let Some(tail) = tok.strip_prefix("sk-")
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
///
/// **JSON 转义不属于任何 token。**扫的是请求体，而请求体是 JSON：换行在
/// 里面是 `\n` 两个字符。按字符切的话，行首的 `sk-ant-…` 会和那个 `n`
/// 粘成 `nsk-ant-…`，前缀就对不上了 —— 贴进对话的凭据文件里，密钥偏偏
/// 常在行首。`\t` 同理；`\uXXXX` 也一样（Python 写的客户端默认把中文
/// 都转成这种写法，`密钥：sk-…` 里的冒号就成了 `\uff1a`）。所以转义序列
/// 整个算作分隔。
fn for_each_token(text: &str, mut f: impl FnMut(&str, Range<usize>)) {
    let mut start: Option<usize> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '\\' {
            if let Some(s) = start.take() {
                f(&text[s..i], s..i);
            }
            // 反斜杠后面那个字符是转义的一部分；`\u` 再带四位十六进制
            if let Some((_, 'u')) = chars.next() {
                for _ in 0..4 {
                    if chars.next_if(|(_, h)| h.is_ascii_hexdigit()).is_none() {
                        break;
                    }
                }
            }
            continue;
        }
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
            bytes: begin..end,
            rule: Rule::Builtin("private-key"),
            label: None,
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
                bytes: c + 1..j,
                rule: Rule::Builtin("conn-string-password"),
                label: None,
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
fn internal_token(tok: &str, set: &RuleSet) -> Option<&'static str> {
    if set.is_on("internal-ip") && is_rfc1918(tok) {
        return Some("internal-ip");
    }
    let lower = tok.to_ascii_lowercase();
    if set.is_on("internal-domain")
        && INTERNAL_SUFFIXES.iter().any(|s| lower.ends_with(s))
        && lower.len() > 7
        && lower.contains('.')
    {
        return Some("internal-domain");
    }
    None
}

// ---------------------------------------------------------------- 自定义

/// 自定义规则的一处匹配，收成**在 JSON 字符串里换得安全**的一段。
///
/// 规则跑在请求体上，而请求体是 JSON：换行是 `\n` 两个字符，引号是 `\"`。
/// 内置规则的判据天生不会跨过它们；用户的正则不一定 —— `\S+` 在 JSON 里
/// 会一路吃过 `\n`，一个跨过引号的替换则直接把请求体写坏。所以：
///
/// - 从反斜杠或引号处截断：那是正文里一个换行、一个引号的位置，截在那儿
///   和同一条正则在纯文本上的结果一致；
/// - 起点落在转义序列中间（前面是奇数个反斜杠）的不要。
fn json_safe(text: &str, m: Range<usize>) -> Option<Range<usize>> {
    let bytes = text.as_bytes();
    let mut slashes = 0;
    while slashes < m.start && bytes[m.start - 1 - slashes] == b'\\' {
        slashes += 1;
    }
    if slashes % 2 == 1 {
        return None;
    }
    let end = text[m.clone()]
        .find(['\\', '"'])
        .map_or(m.end, |i| m.start + i);
    (end > m.start).then_some(m.start..end)
}

fn custom_hits(text: &str, set: &RuleSet, out: &mut Vec<Hit>) {
    for c in &set.custom {
        for m in c.re.find_iter(text) {
            if let Some(bytes) = json_safe(text, m.range()) {
                out.push(Hit {
                    bytes,
                    rule: Rule::Custom(c.name.clone()),
                    label: c.label.clone(),
                });
            }
        }
    }
}

// ---------------------------------------------------------------- 入口

/// 按 `set` 里开着的规则扫一段文本。
///
/// 返回的区间**按起点排序且互不重叠** —— 替换要从后往前做，重叠会让
/// 偏移全乱。
pub fn scan(text: &str, set: &RuleSet) -> Vec<Hit> {
    let mut out = Vec::new();
    if set.is_empty() {
        return out;
    }
    if set.is_on("private-key") {
        private_keys(text, &mut out);
    }
    if set.is_on("conn-string-password") {
        conn_strings(text, &mut out);
    }
    let want_jwt = set.is_on("jwt");
    for_each_token(text, |tok, span| {
        if let Some(id) = classify_token(tok, set) {
            out.push(Hit {
                bytes: span,
                rule: Rule::Builtin(id),
                label: None,
            });
            return;
        }
        if want_jwt && looks_like_jwt(tok) {
            out.push(Hit {
                bytes: span,
                rule: Rule::Builtin("jwt"),
                label: None,
            });
            return;
        }
        if let Some(id) = internal_token(tok, set) {
            out.push(Hit {
                bytes: span,
                rule: Rule::Builtin(id),
                label: None,
            });
        }
    });
    custom_hits(text, set, &mut out);
    disjoint(out)
}

/// 排好序、去掉重叠的。**重叠的只留第一个。**私钥块里的 base64 会被 token
/// 扫描当成别的东西，两段嵌在一起替换会把偏移彻底搞乱。同一个起点上留长的那段。
fn disjoint(mut out: Vec<Hit>) -> Vec<Hit> {
    out.sort_by_key(|h| (h.bytes.start, std::cmp::Reverse(h.bytes.end)));
    let mut kept: Vec<Hit> = Vec::with_capacity(out.len());
    for h in out {
        if kept.last().is_some_and(|p| p.bytes.end > h.bytes.start) {
            continue;
        }
        kept.push(h);
    }
    kept
}

/// 一个字符在 JSON 字符串里写出来有几个字节。**和 serde_json 的转义一致。**
fn json_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// 扫一段**纯文本**，结果和它出现在请求体里时一样。
///
/// 界面上的「测试」用它：用户贴进来的是一段正文，而网关扫的是装着这段
/// 正文的 JSON。直接扫纯文本的话，一条跨过换行的自定义规则会在测试里
/// 命中、在真的请求里却换不掉 —— 测试的结论就是错的。所以先编成 JSON
/// 字符串再扫，再把区间换算回原文。
pub fn scan_plain(text: &str, set: &RuleSet) -> Vec<Hit> {
    let encoded = serde_json::to_string(text).unwrap_or_default();
    // 原文每个字符的起点在编码后的位置。首尾那对引号不算
    let mut map: Vec<(usize, usize)> = Vec::with_capacity(text.len() + 1);
    let mut at = 1;
    for (i, c) in text.char_indices() {
        map.push((at, i));
        at += json_len(c);
    }
    map.push((at, text.len()));
    let back = |enc: usize| {
        map.binary_search_by_key(&enc, |(e, _)| *e)
            .ok()
            .map(|i| map[i].1)
    };
    scan(&encoded, set)
        .into_iter()
        .filter_map(|h| {
            let start = back(h.bytes.start)?;
            let end = back(h.bytes.end)?;
            (end > start).then_some(Hit {
                bytes: start..end,
                rule: h.rule,
                label: h.label,
            })
        })
        .collect()
}

/// 扫一段**解码过的正文**：自定义规则按正则本来的意思匹配。
///
/// 和 [`scan_plain`] 的差别只在自定义规则上。桌面版扫的是线上那份 JSON，所以
/// 自定义规则的命中止于引号和反斜杠（见 `json_safe`）；而一个先解码请求、在
/// 正文上找、再把结果带回去的调用方（企业版）看到的就是正文，`password="x"`
/// 截成 `password=` 只会让真值漏出去。它换下来的值可能带引号，整包还原时要用
/// [`crate::redact::replace::restore_json`]。
pub fn scan_text(text: &str, set: &RuleSet) -> Vec<Hit> {
    let builtins = RuleSet {
        on: set.on.clone(),
        custom: Vec::new(),
    };
    let mut out = scan_plain(text, &builtins);
    for c in &set.custom {
        for m in c.re.find_iter(text).filter(|m| !m.is_empty()) {
            out.push(Hit {
                bytes: m.range(),
                rule: Rule::Custom(c.name.clone()),
                label: c.label.clone(),
            });
        }
    }
    disjoint(out)
}

/// 报出去的样子。**一律打码** —— 「发现了 sk-ant-xxx」这句话本身就是一次
/// 泄漏，它会进日志、进界面、被复制到 issue 里。
///
/// 内网地址和内部域名例外：它们不是凭据，而打码之后的 `…` 让人无从判断
/// 那条记录说的是哪台机器。
pub fn masked(rule: &Rule, value: &str) -> String {
    if rule.kind() == Kind::Internal {
        return value.to_string();
    }
    tw_secret::mask_secret(value)
}

/// 一次扫描按「哪条规则 × 哪个值」合起来的结果，给事件和日志用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub rule: Rule,
    /// 已打码
    pub masked: String,
    /// 这个值在这段文本里出现了几次
    pub count: u64,
}

/// 把命中合并成「哪条规则 × 哪个值 × 几次」。**同一个值只报一次**：
/// 一把 key 在一个请求里出现三次，是一把 key，不是三把。
pub fn findings(text: &str, hits: &[Hit]) -> Vec<Finding> {
    let mut out: Vec<(Rule, &str, u64)> = Vec::new();
    for h in hits {
        let value = &text[h.bytes.clone()];
        match out.iter_mut().find(|(r, v, _)| *r == h.rule && *v == value) {
            Some((_, _, n)) => *n += 1,
            None => out.push((h.rule.clone(), value, 1)),
        }
    }
    out.into_iter()
        .map(|(rule, value, count)| Finding {
            masked: masked(&rule, value),
            rule,
            count,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> RuleSet {
        RuleSet::only(&BUILTINS.iter().map(|b| b.id).collect::<Vec<_>>())
    }

    fn found(text: &str) -> Vec<(String, String)> {
        scan(text, &all())
            .into_iter()
            .map(|h| (h.rule.id().to_string(), text[h.bytes].to_string()))
            .collect()
    }

    #[test]
    fn a_pattern_that_compiles_into_something_huge_is_refused() {
        // 默认上限下它能编过，然后每个请求都付几毫秒
        assert!(compile("大", "(a|aa|aaa){5000}").is_err());
        assert!(compile("正常", r"corp_[A-Za-z0-9]{12}").is_ok());
    }

    #[test]
    fn the_prefixed_keys_are_found_whole() {
        let t = "我的 key 是 sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA，别外传";
        let got = found(t);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, "anthropic-api-key");
        assert!(got[0].1.starts_with("sk-ant-api03-"), "{}", got[0].1);
    }

    #[test]
    fn a_key_right_after_a_json_escape_is_still_found() {
        // 扫的是请求体原文：换行、制表符在里面是 `\n`、`\t`，中文可能是
        // `\uXXXX`。转义里的那个字母不能和后面的密钥粘成一个 token
        let key = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl";
        for (t, rule, value) in [
            (format!(r"keys:\n{key}\nnext"), "anthropic-api-key", key),
            (format!(r"key\t{key}"), "anthropic-api-key", key),
            (
                format!(r"\u5bc6\u94a5\uff1a{key}"),
                "anthropic-api-key",
                key,
            ),
            (format!(r"token:\n{jwt}"), "jwt", jwt),
            // 转义的反斜杠：解出来是 `C:\sk-ant-…`，反斜杠后面照样是边界
            (format!(r"C:\\{key}"), "anthropic-api-key", key),
        ] {
            let got = found(&t);
            assert_eq!(got, vec![(rule.to_string(), value.to_string())], "{t}");
        }
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
        assert_eq!(got[0].0, "private-key");
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
        assert_eq!(got[0].0, "private-key");
        // 换掉之后 JSON 还得是合法的
        let r = crate::redact::replace::redact(
            body,
            &all(),
            crate::redact::replace::Ledger::new(crate::redact::replace::Scheme::SECRET),
        );
        serde_json::from_str::<serde_json::Value>(&r.text).expect("换完不是合法 JSON");
        // 而且能一字不差地换回来
        assert_eq!(crate::redact::replace::restore(&r.text, &r.ledger), body);
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
        assert_eq!(got[0].0, "jwt");
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
        assert_eq!(got[0].0, "conn-string-password");
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
                !found(t).iter().any(|(k, _)| k == "conn-string-password"),
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
        assert!(got.iter().all(|(k, _)| k == "internal-ip"));
        assert!(found("127.0.0.1 和 8.8.8.8 和 172.32.0.1").is_empty());
    }

    #[test]
    fn the_internal_rules_are_off_until_someone_turns_them_on() {
        // RFC1918 地址在代码和文档里到处都是，而它的危害远小于一把 key。
        let t = "内网 10.1.2.3，内部域名 build.corp.internal";
        assert!(scan(t, &RuleSet::defaults()).is_empty());
        let set = RuleSet::build(&["internal-ip".into()], &[], []).unwrap();
        let got = scan(t, &set);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].rule, Rule::Builtin("internal-ip"));
    }

    #[test]
    fn a_rule_that_is_off_is_not_used_and_does_not_hand_its_keys_to_another() {
        // 关掉 Anthropic 那条之后，`sk-ant-…` 不该被当成一把 OpenAI 老式
        // key 换掉 —— 用户关掉的正是「这种东西不要换」。
        let key = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA1234";
        let set = RuleSet::build(&[], &["anthropic-api-key".into()], []).unwrap();
        assert!(scan(key, &set).is_empty(), "{:?}", scan(key, &set));
        assert_eq!(scan(key, &RuleSet::defaults()).len(), 1);
    }

    #[test]
    fn a_custom_rule_is_found_alongside_the_builtin_ones() {
        let set = RuleSet::build(&[], &[], [("公司令牌", r"corp_[A-Za-z0-9]{8}")]).unwrap();
        let t = "令牌 corp_ABCD1234 和 sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";
        let got = scan(t, &set);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].rule, Rule::Custom(Arc::from("公司令牌")));
        assert_eq!(&t[got[0].bytes.clone()], "corp_ABCD1234");
        assert!(got[0].rule.custom());
        assert_eq!(got[0].rule.kind(), Kind::Custom);
    }

    #[test]
    fn a_broken_custom_pattern_is_an_error_that_names_the_rule() {
        let e = RuleSet::build(&[], &[], [("写坏了", "(")]).unwrap_err();
        assert_eq!(e.name, "写坏了");
        assert!(RuleSet::build(&[], &[], [("空的", "")]).is_err());
    }

    #[test]
    fn a_custom_match_never_crosses_an_escape_or_a_quote_in_the_body() {
        // `\S+` 在 JSON 里会一路吃过 `\n` 和 `\"`；换掉那样一段会把请求体写坏。
        let set = RuleSet::none().with_custom("口令", r"pw=\S+").unwrap();
        let body = serde_json::to_string(&serde_json::json!({
            "content": "pw=hunter2\n下一行 \"引号\" pw=abc\"def"
        }))
        .unwrap();
        let r = crate::redact::replace::redact(
            &body,
            &set,
            crate::redact::replace::Ledger::new(crate::redact::replace::Scheme::SECRET),
        );
        let v: serde_json::Value = serde_json::from_str(&r.text).expect("换完不是合法 JSON");
        let content = v["content"].as_str().unwrap();
        assert!(content.starts_with("<<TW_SECRET_1>>\n下一行"), "{content}");
        assert!(content.contains("<<TW_SECRET_2>>\"def"), "{content}");
        assert_eq!(crate::redact::replace::restore(&r.text, &r.ledger), body);
    }

    #[test]
    fn a_custom_match_that_starts_inside_an_escape_is_dropped() {
        // `\n` 里的 `n` 不是正文的一部分，从它开始换会留下一个坏掉的转义。
        let set = RuleSet::none().with_custom("n 开头", r"n[a-z]+").unwrap();
        let body = r#"{"c":"a\nbc"}"#;
        assert!(scan(body, &set).is_empty(), "{:?}", scan(body, &set));
    }

    #[test]
    fn scanning_plain_text_gives_what_the_body_would_give() {
        // 测试的结论必须和真的请求一致：跨过换行的那一截在请求体里换不掉，
        // 在测试里也不该显示成命中。
        let set = RuleSet::none().with_custom("口令", r"pw=\S+").unwrap();
        let text = "第一行 pw=hunter2\n第二行";
        let got = scan_plain(text, &set);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(&text[got[0].bytes.clone()], "pw=hunter2");
        // 内置规则同样换算回原文，包括跨过换行的私钥块
        let pem =
            "看：\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY-----\n完";
        let got = scan_plain(pem, &RuleSet::defaults());
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(pem[got[0].bytes.clone()].starts_with("-----BEGIN RSA"));
    }

    #[test]
    fn overlapping_hits_keep_only_the_outer_one() {
        // 私钥块里的 base64 会被 token 扫描当成别的东西，嵌在一起替换会
        // 把偏移彻底搞乱。
        let t = "-----BEGIN OPENSSH PRIVATE KEY-----\nsk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA\n-----END OPENSSH PRIVATE KEY-----";
        let got = scan(t, &all());
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].rule, Rule::Builtin("private-key"));
    }

    #[test]
    fn the_spans_can_actually_be_used_to_slice_multibyte_text() {
        // 这个项目已经被字节切片坑过三次。命中点前后全是中文。
        let t = "这是一段很长的中文说明，里面混着一把 sk-ant-api03-BBBBBBBBBBBBBBBBBBBBBBBBBB，后面继续写中文";
        let got = scan(t, &all());
        assert_eq!(got.len(), 1);
        assert!(t[got[0].bytes.clone()].starts_with("sk-ant-"));
    }

    #[test]
    fn the_hits_come_back_sorted_and_disjoint() {
        // 替换要从后往前做，重叠或乱序会让偏移全乱。
        let t = "10.0.0.1 然后 sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA 然后 192.168.1.1";
        let got = scan(t, &all());
        assert_eq!(got.len(), 3);
        for w in got.windows(2) {
            assert!(w[0].bytes.end <= w[1].bytes.start, "{got:?}");
        }
    }

    #[test]
    fn findings_say_which_rule_saw_which_value_and_how_often_but_never_the_value() {
        let key = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";
        let t = format!("{key} 又一次 {key} 和 10.0.0.1");
        let hits = scan(&t, &all());
        let f = findings(&t, &hits);
        assert_eq!(f.len(), 2, "{f:?}");
        assert_eq!(f[0].rule, Rule::Builtin("anthropic-api-key"));
        assert_eq!(f[0].count, 2);
        assert!(!f[0].masked.contains("AAAAAAAAAAAA"), "{}", f[0].masked);
        // 内网地址不是凭据，打码之后反而认不出是哪台机器
        assert_eq!(f[1].masked, "10.0.0.1");
    }

    #[test]
    fn every_builtin_rule_describes_what_it_matches() {
        // 界面上的「匹配」一栏来自 `matcher`；它说的必须就是扫描用的判据。
        for b in BUILTINS {
            if let Matcher::Prefix { prefix, min_tail } = b.matcher {
                let key = format!("{prefix}{}", "a1".repeat(min_tail));
                let hits = scan(&key, &RuleSet::only(&[b.id]));
                assert_eq!(hits.len(), 1, "{} 认不出它自己说的形状：{key}", b.id);
                let short = format!("{prefix}{}", "a".repeat(min_tail - 1));
                assert!(
                    scan(&short, &RuleSet::only(&[b.id])).is_empty(),
                    "{} 比它说的长度下限更短的也认了",
                    b.id
                );
            }
        }
        let ids: HashSet<&str> = BUILTINS.iter().map(|b| b.id).collect();
        assert_eq!(ids.len(), BUILTINS.len(), "内置规则的 id 重复了");
    }
}
