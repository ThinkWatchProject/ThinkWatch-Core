//! 认得出哪些东西是凭据，外加两种个人号码。
//!
//! **桌面场景要脱的东西和企业完全不同。**企业关心合规 —— 客户的身份证
//! 号、手机号不能流到第三方模型。个人开发者关心的是：**我的 API key、
//! 私钥、内网地址，会不会被中转站顺走。**所以这套规则是凭据导向的，
//! 手机号、邮箱、姓名这类个人信息一条都不收：它们没有能核对的结构，只能
//! 按「长得像」去猜。
//!
//! **身份证号和银行卡号是两个例外**，是产品上点名要的：一旦漏出去就收不
//! 回来，而且都能在结构上核对 —— 身份证号有省级地区码、真实的出生日期和
//! MOD 11-2 校验码，卡号有卡组织的号段、位数和 Luhn 校验 —— 达得到和凭据
//! 规则同一道门槛。它们自成一类（[`Kind::Personal`]），占位符写明是哪一种，
//! 报出去只留最后四位。
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
    /// 身份证号、银行卡号：个人信息，不是凭据。占位符写明是哪一种，报出去
    /// 只留最后四位
    Personal,
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
            Kind::Personal => "personal",
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
    /// 18 位的中华人民共和国居民身份证号码：头两位是省级行政区划代码，第 7–14 位
    /// 是 `born_since` 年 1 月 1 日到今天之间的真实日期，末位是对得上的
    /// ISO 7064 MOD 11-2 校验码（`0`–`9` 或 `X`）。15 位的老号码不认
    CnResidentId { born_since: u16 },
    /// 卡号：开头和位数属于其中一家卡组织，并且通过 Luhn 校验。连着写的，或者
    /// 四位一组、用一个空格或一个连字符隔开的（最后一组可以不足四位；American
    /// Express 另有 4-6-5、Diners Club 另有 4-6-4）。公开的测试卡号不算
    BankCard { networks: &'static [CardNetwork] },
}

/// 一家卡组织认哪些卡号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardNetwork {
    pub name: &'static str,
    /// 以哪几段开头，含两头、两头位数相同：`(51, 55)` 是 51 到 55，`(4, 4)` 就是 4
    pub prefixes: &'static [(u32, u32)],
    /// 一共几位
    pub lengths: &'static [u8],
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
    /// 占位符里的标签。`None` 用账本的默认标签（桌面版是 `TW_SECRET`）。
    ///
    /// 个人号码写明是哪一种：模型看到 `<<ID_NUMBER_1>>` 才知道那里原来是个
    /// 身份证号，答得像样
    pub label: Option<&'static str>,
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
        label: None,
    }
}

/// `sk-` 开头的 OpenAI 老式 key 至少要多长。
///
/// 前缀太短，**要靠长度把它和 `sk-test`、`sk-xxx` 这种占位符分开**。
const OPENAI_MIN: usize = 40;

const INTERNAL_SUFFIXES: &[&str] = &[".local", ".internal", ".lan"];

/// 18 位身份证号码的出生日期最早到哪一年。
const BORN_SINCE: u16 = 1900;

/// 认哪几家卡组织的卡号。**判据和界面上的说明用的是同一张表。**
///
/// 号段只收各家公开的主号段：一串数字要同时落进号段、位数对得上、过得了
/// Luhn，才算卡号。
pub const CARD_NETWORKS: &[CardNetwork] = &[
    CardNetwork {
        name: "UnionPay",
        prefixes: &[(62, 62)],
        lengths: &[16, 17, 18, 19],
    },
    CardNetwork {
        name: "Visa",
        prefixes: &[(4, 4)],
        lengths: &[16, 19],
    },
    CardNetwork {
        name: "Mastercard",
        prefixes: &[(51, 55), (2221, 2720)],
        lengths: &[16],
    },
    CardNetwork {
        name: "American Express",
        prefixes: &[(34, 34), (37, 37)],
        lengths: &[15],
    },
    CardNetwork {
        name: "JCB",
        prefixes: &[(3528, 3589)],
        lengths: &[16],
    },
    CardNetwork {
        name: "Discover",
        prefixes: &[(6011, 6011), (644, 649), (65, 65)],
        lengths: &[16],
    },
    CardNetwork {
        name: "Diners Club",
        prefixes: &[(300, 305), (36, 36), (38, 38)],
        lengths: &[14],
    },
];

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
        label: None,
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
        label: None,
    },
    Builtin {
        id: "jwt",
        kind: Kind::Jwt,
        name: "JWT",
        on_by_default: true,
        matcher: Matcher::Jwt,
        label: None,
    },
    Builtin {
        id: "conn-string-password",
        kind: Kind::ConnStrings,
        name: "Connection string password",
        on_by_default: true,
        matcher: Matcher::ConnString,
        label: None,
    },
    Builtin {
        id: "cn-resident-id",
        kind: Kind::Personal,
        name: "Chinese resident ID number",
        on_by_default: true,
        matcher: Matcher::CnResidentId {
            born_since: BORN_SINCE,
        },
        label: Some("ID_NUMBER"),
    },
    Builtin {
        id: "bank-card",
        kind: Kind::Personal,
        name: "Bank card number",
        on_by_default: true,
        matcher: Matcher::BankCard {
            networks: CARD_NETWORKS,
        },
        label: Some("CARD_NUMBER"),
    },
    Builtin {
        id: "internal-ip",
        kind: Kind::Internal,
        name: "Internal IP address",
        on_by_default: false,
        matcher: Matcher::PrivateIp,
        label: None,
    },
    Builtin {
        id: "internal-domain",
        kind: Kind::Internal,
        name: "Internal domain",
        on_by_default: false,
        matcher: Matcher::DomainSuffix {
            suffixes: INTERNAL_SUFFIXES,
        },
        label: None,
    },
];

pub fn builtin(id: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.id == id)
}

/// 内置规则的一处命中。**标签照规则表上写的**，不在各个找的地方各写一遍。
fn builtin_hit(id: &'static str, bytes: Range<usize>) -> Hit {
    Hit {
        bytes,
        rule: Rule::Builtin(id),
        label: builtin(id).and_then(|b| b.label).map(Arc::from),
    }
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
        out.push(builtin_hit("private-key", begin..end));
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
            out.push(builtin_hit("conn-string-password", c + 1..j));
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

// ---------------------------------------------------------------- 个人号码

/// 省级行政区划代码（GB/T 2260），身份证号码的头两位。
const PROVINCES: [u8; 34] = [
    11, 12, 13, 14, 15, // 华北
    21, 22, 23, // 东北
    31, 32, 33, 34, 35, 36, 37, // 华东
    41, 42, 43, 44, 45, 46, // 中南
    50, 51, 52, 53, 54, // 西南
    61, 62, 63, 64, 65, // 西北
    71, 81, 82, // 台湾、香港、澳门
];

/// ISO 7064 MOD 11-2：前 17 位各乘对应的权重，加起来除以 11 的余数在
/// [`ID_CHECK`] 里查出校验码。
const ID_WEIGHTS: [u32; 17] = [7, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
const ID_CHECK: &[u8; 11] = b"10X98765432";

/// 18 位居民身份证号码。`today` 写成 `YYYYMMDD`。
///
/// **地区码、日期、校验码缺一不可。**光看「18 位数字」的话，雪花 ID、订单号、
/// 纳秒时间戳全是；地区码挡掉一部分，真实的日期再挡掉一大半，校验码最后只放过
/// 十一分之一。
fn resident_id(s: &[u8], today: u32) -> bool {
    if s.len() != 18 || !s[..17].iter().all(u8::is_ascii_digit) {
        return false;
    }
    let d = |i: usize| u32::from(s[i] - b'0');
    let num = |r: Range<usize>| r.fold(0, |v, i| v * 10 + d(i));
    if !PROVINCES.contains(&(num(0..2) as u8)) {
        return false;
    }
    let (year, month, day) = (num(6..10), num(10..12), num(12..14));
    if year < u32::from(BORN_SINCE)
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in(year, month)
    {
        return false;
    }
    let sum: u32 = (0..17).map(|i| d(i) * ID_WEIGHTS[i]).sum();
    s[17].to_ascii_uppercase() == ID_CHECK[(sum % 11) as usize]
        // 出生在今天以后的人还没有身份证号
        && year * 10_000 + month * 100 + day <= today
}

/// 某年某月有几天。闰年是能被 4 整除而不能被 100 整除的，或者能被 400 整除的
/// —— 1900 年没有 2 月 29 日，2000 年有。
fn days_in(year: u32, month: u32) -> u32 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// 今天，写成 `YYYYMMDD`。**按北京时间**：身份证上的出生日期是按它记的。
fn today_ymd() -> u32 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (y, m, d) = civil((secs + 8 * 3600) / 86_400);
    y * 10_000 + m * 100 + d
}

/// 1970-01-01 之后的第 `days` 天是哪年哪月哪日（Howard Hinnant 的
/// `civil_from_days`）。为了一个日期不值得引一个时间库。
fn civil(days: u64) -> (u32, u32, u32) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = era * 400 + yoe + u64::from(m <= 2);
    (y as u32, m as u32, d as u32)
}

/// 卡号最多几位。
const CARD_MAX: usize = 19;

/// 公开的测试卡号。**开发者天天往代码、文档里贴它们**，换成占位符纯属打扰。
///
/// 不求全，只收最常被贴的那几张表，2026-10 对着各家的页面核对过：
///
/// - Stripe（docs.stripe.com/testing）：按卡组织、联名卡、拒付三张表，和最常用的
///   三张 3DS 卡；
/// - Braintree（developer.paypal.com/braintree/docs/guides/credit-cards/testing-go-live）：
///   Valid card numbers 和 unsuccessful verification 两张表；
/// - Adyen（docs.adyen.com/development-resources/test-cards-and-credentials/test-card-numbers）：
///   3D Secure 2 那张表，一家卡组织一两张。
///
/// 按国家、风控、争议细分的几百张不收。**每一张都得是这条规则本来会认的**
/// （测试里钉着）：认不出的用不着排除，列着只会让人以为它有用 —— Stripe 那张
/// 16 位的 Diners Club（3056 9300 0902 0004）就是这样，这里的 Diners Club 只认 14 位。
const TEST_CARDS: &[&str] = &[
    // Stripe：按卡组织
    "4242424242424242",
    "4000056655665556",
    "5555555555554444",
    "2223003122003222",
    "5200828282828210",
    "5105105105105100",
    "378282246310005",
    "371449635398431",
    "6011111111111117",
    "6011000990139424",
    "6011981111111113",
    "36227206271667",
    "6555900000604105",
    "3566002020360505",
    "6200000000000005",
    "6200000000000047",
    "6205500000000000004",
    // Stripe：联名卡（Cartes Bancaires、eftpos）
    "4000002500001001",
    "5555552500001001",
    "4000050360000001",
    "5555050360000080",
    // Stripe：拒付
    "4000000000000002",
    "4000000000009995",
    "4000000000009987",
    "4000000000009979",
    "4000000000000069",
    "4000000000000127",
    "4000000000000119",
    "4000000000006975",
    "4000000000000341",
    // Stripe：3DS
    "4000002500003155",
    "4000002760003184",
    "4000000000003220",
    // Braintree：Valid card numbers（和上面重复的不再列）
    "36259600000004",
    "6011000991300009",
    "3530111333300000",
    "2223000048400011",
    "4111111111111111",
    "4005519200000004",
    "4009348888881881",
    "4012000033330026",
    "4012000077777777",
    "4012888888881881",
    "4217651111111119",
    "4500600000000061",
    "6243030000000001",
    "6221261111117766",
    "6223164991230014",
    // Braintree：unsuccessful verification
    "4000111111111115",
    "378734493671000",
    "38520000009814",
    // Adyen：3D Secure 2
    "4871049999999910",
    "4035501428146300",
    "4360000001000005",
    "6250947000000014",
    "6250946000000016",
    "30569309025904",
    "3566111111111113",
    "5454545454545454",
    "2222400010000008",
    "4917610000000000",
    "4166676667666746",
];

/// 一串数字属于哪家卡组织：位数对得上，开头落在它的号段里。
fn card_network(d: &[u8]) -> Option<&'static CardNetwork> {
    CARD_NETWORKS.iter().find(|n| {
        // 先比位数：位数都对不上的话，开头那几位可能还没有这么长
        n.lengths.contains(&(d.len() as u8))
            && n.prefixes.iter().any(|&(from, to)| {
                let k = from.checked_ilog10().map_or(1, |l| l as usize + 1);
                let head = d[..k].iter().fold(0, |v, c| v * 10 + u32::from(c - b'0'));
                (from..=to).contains(&head)
            })
    })
}

/// Luhn 校验：从右数第二位起每隔一位乘 2（超过 9 的减 9），总和是 10 的倍数。
fn luhn(d: &[u8]) -> bool {
    let sum: u32 = d
        .iter()
        .rev()
        .enumerate()
        .map(|(i, c)| {
            let x = u32::from(c - b'0');
            match (i % 2, x * 2) {
                (0, _) => x,
                (_, y) if y > 9 => y - 9,
                (_, y) => y,
            }
        })
        .sum();
    sum % 10 == 0
}

/// 一段文本是不是一个卡号。`sep` 是分组用的那一种分隔符，`None` 是连着写的。
///
/// 分组只认两种写法：四位一组、最后一组可以不足四位（`6222 0212 3456 7894 123`），
/// 和 American Express、Diners Club 卡面上印的 4-6-5、4-6-4。**一个号码里只用一种
/// 分隔符**：`6222-0212 3456-7894` 不算。
///
/// 数字抄在栈上的一个定长数组里，不分配 —— 每个请求体里每个像样的数字串都要过一遍。
fn card_number(s: &str, sep: Option<u8>) -> bool {
    let mut digits = [0u8; CARD_MAX];
    let mut n = 0;
    let mut lens = [0usize; 5];
    let mut groups = 0;
    for g in s.as_bytes().split(|&b| Some(b) == sep) {
        if g.is_empty()
            || groups == lens.len()
            || n + g.len() > CARD_MAX
            || !g.iter().all(u8::is_ascii_digit)
        {
            return false;
        }
        digits[n..n + g.len()].copy_from_slice(g);
        n += g.len();
        lens[groups] = g.len();
        groups += 1;
    }
    let shape = &lens[..groups];
    let written = match shape {
        [] => false,
        [_] => sep.is_none(),
        [head @ .., last] => {
            sep.is_some()
                && ((head.iter().all(|&l| l == 4) && (1..=4).contains(last))
                    || shape == [4, 6, 5]
                    || shape == [4, 6, 4])
        }
    };
    let d = &digits[..n];
    written && card_network(d).is_some() && luhn(d) && !TEST_CARDS.iter().any(|t| t.as_bytes() == d)
}

/// 用一个空格隔开的几组数字：`6222 0212 3456 7894`。
///
/// 分词在空格处切开，这样的卡号到这里是一串 token，要接起来看。**只接隔着
/// 正好一个空格的**：两个空格、换行、制表符隔开的是几个数，不是一个分组写的
/// 号码。看的是接起来最长的那一整串 —— 从里面截一段去对的话，一张四位数的
/// 表里每隔几行就「有」一张卡。
#[derive(Default)]
struct Spaced {
    /// 第一组的起点。`None`：眼下没有在接的
    start: Option<usize>,
    /// 最后一组的起点
    last: usize,
    /// 最后一组的终点
    end: usize,
    groups: usize,
    /// 最后一组后面跟着句号：这句话说完了，后面的数字不再接上来
    closed: bool,
}

impl Spaced {
    /// 下一个 token。`body` 是它去掉句末句号的那一段，从 `at` 开始。
    fn feed(&mut self, text: &str, at: usize, body: &str, dotted: bool, out: &mut Vec<Hit>) {
        // 一组最多六位：4-6-5 中间那组
        let group = (1..=6).contains(&body.len()) && body.bytes().all(|b| b.is_ascii_digit());
        if group
            && !self.closed
            && self.start.is_some()
            && at == self.end + 1
            && text.as_bytes()[self.end] == b' '
        {
            self.last = at;
            self.end = at + body.len();
            self.groups += 1;
            self.closed = dotted;
            return;
        }
        self.finish(text, out);
        if group {
            *self = Spaced {
                start: Some(at),
                last: at,
                end: at + body.len(),
                groups: 1,
                closed: dotted,
            };
        }
    }

    /// 这一串接完了：整串是一个卡号就记下来。
    ///
    /// 不是的话，末尾那组不足四位时去掉它再看一次：卡号后面紧跟着的月份、安全码
    /// （`6222 0212 3456 7894 12/28`）不是号码的一部分。整串先看 —— 19 位的卡号
    /// 最后一组本来就只有三位。
    fn finish(&mut self, text: &str, out: &mut Vec<Hit>) {
        let Some(start) = self.start.take() else {
            return;
        };
        if self.groups < 2 {
            return;
        }
        if card_number(&text[start..self.end], Some(b' ')) {
            out.push(builtin_hit("bank-card", start..self.end));
        } else if self.groups > 2
            && self.end - self.last < 4
            && card_number(&text[start..self.last - 1], Some(b' '))
        {
            out.push(builtin_hit("bank-card", start..self.last - 1));
        }
    }
}

/// 一个 token 是不是身份证号，或者连着写、用连字符分组写的卡号。`body` 是
/// 去掉了句末句号的那一段，从 `at` 开始。
fn personal_token(
    body: &str,
    at: usize,
    want_id: bool,
    want_card: bool,
    today: u32,
    out: &mut Vec<Hit>,
) {
    let b = body.as_bytes();
    // 最短的卡号也有 14 位。开头不是数字的（`-6222…`、`x6222…`）不是一个号码
    if b.len() < 14 || !b[0].is_ascii_digit() {
        return;
    }
    let span = at..at + b.len();
    // 身份证号先认，认出来了就**不再往下猜**：甘肃的号码以 62 开头，有的碰巧也
    // 过得了 Luhn。关掉身份证号那条，是不要换身份证号，不是让它当成卡号换掉
    if resident_id(b, today) {
        if want_id {
            out.push(builtin_hit("cn-resident-id", span));
        }
    } else if want_card && card_number(body, body.contains('-').then_some(b'-')) {
        out.push(builtin_hit("bank-card", span));
    }
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
    let want_id = set.is_on("cn-resident-id");
    let want_card = set.is_on("bank-card");
    // 一次扫描问一次时钟。只开卡号那条时也要问：认出是身份证号的不当卡号换
    let today = if want_id || want_card { today_ymd() } else { 0 };
    let mut spaced = Spaced::default();
    for_each_token(text, |tok, span| {
        if want_id || want_card {
            // 句末的句号不是号码的一部分：`…卡号是 6222 0212 3456 7894.`
            let body = tok.trim_end_matches('.');
            if want_card {
                spaced.feed(text, span.start, body, body.len() < tok.len(), &mut out);
            }
            personal_token(body, span.start, want_id, want_card, today, &mut out);
        }
        if let Some(id) = classify_token(tok, set) {
            out.push(builtin_hit(id, span));
            return;
        }
        if want_jwt && looks_like_jwt(tok) {
            out.push(builtin_hit("jwt", span));
            return;
        }
        if let Some(id) = internal_token(tok, set) {
            out.push(builtin_hit(id, span));
        }
    });
    spaced.finish(text, &mut out);
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
///
/// 身份证号和卡号只留最后四位：「留头 5」留下的正好是身份证号的地区码、卡号的
/// 发卡行，而认出是哪一个号码，看最后四位就够了。
pub fn masked(rule: &Rule, value: &str) -> String {
    match rule.kind() {
        Kind::Internal => value.to_string(),
        Kind::Personal => last_four(value),
        _ => mask(value),
    }
}

/// 只留最后四位：`…1234`。分组写的卡号跳过分隔符数，`… 7894 123` 留 `4123`。
fn last_four(s: &str) -> String {
    let n = s.bytes().filter(u8::is_ascii_alphanumeric).count();
    let tail: String = s
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .skip(n.saturating_sub(4))
        .collect();
    format!("…{tail}")
}

/// 留头 5 尾 4，中间省略：`sk-a…7f9c`。
///
/// **按字符切，不按字节** —— 一个中文字符落在切口上，按字节切就是 panic。
fn mask(s: &str) -> String {
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

    #[test]
    fn a_reported_value_keeps_only_its_ends_and_never_splits_a_character() {
        assert_eq!(mask("sk-ant-api03-abcdef7f9c"), "sk-an…7f9c");
        assert_eq!(mask("密钥密钥密钥密钥密钥密钥"), "密钥密钥密…密钥密钥");
        assert_eq!(mask("short"), "………");
        assert_eq!(mask(""), "");
    }

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
            let only = RuleSet::only(&[b.id]);
            match b.matcher {
                Matcher::Prefix { prefix, min_tail } => {
                    let key = format!("{prefix}{}", "a1".repeat(min_tail));
                    let hits = scan(&key, &only);
                    assert_eq!(hits.len(), 1, "{} 认不出它自己说的形状：{key}", b.id);
                    let short = format!("{prefix}{}", "a".repeat(min_tail - 1));
                    assert!(
                        scan(&short, &only).is_empty(),
                        "{} 比它说的长度下限更短的也认了",
                        b.id
                    );
                }
                Matcher::CnResidentId { born_since } => {
                    let first = id_done(&format!("110105{born_since}0101001"));
                    assert_eq!(scan(&first, &only).len(), 1, "{first}");
                    let earlier = id_done(&format!("110105{}1231001", born_since - 1));
                    assert!(scan(&earlier, &only).is_empty(), "{earlier}");
                }
                Matcher::BankCard { networks } => {
                    // 每一家的每一段号段的两头、每一种位数都认；位数短一位、长一位的不认
                    for n in networks {
                        let (min, max) = (n.lengths[0], n.lengths[n.lengths.len() - 1]);
                        for &(from, to) in n.prefixes {
                            for p in [from, to] {
                                for &len in n.lengths {
                                    let card = card_of(p, len);
                                    assert_eq!(
                                        scan(&card, &only).len(),
                                        1,
                                        "{} 不认 {card}",
                                        n.name
                                    );
                                }
                                for len in [min - 1, max + 1] {
                                    let card = card_of(p, len);
                                    assert!(
                                        scan(&card, &only).is_empty(),
                                        "{} 认了 {card}",
                                        n.name
                                    );
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let ids: HashSet<&str> = BUILTINS.iter().map(|b| b.id).collect();
        assert_eq!(ids.len(), BUILTINS.len(), "内置规则的 id 重复了");
    }

    // ------------------------------------------------------------ 个人号码

    /// 补上 ISO 7064 MOD 11-2 校验码。**照标准另写一遍**（权重是 2^(17-i) mod 11），
    /// 不借被测的那张权重表 —— 借了的话，表抄错了这里也跟着错。
    fn id_done(first17: &str) -> String {
        let sum: u32 = first17
            .bytes()
            .enumerate()
            .map(|(i, c)| u32::from(c - b'0') * (2u32.pow(17 - i as u32) % 11))
            .sum();
        let c = (12 - sum % 11) % 11;
        let check = if c == 10 {
            'X'
        } else {
            char::from(b'0' + c as u8)
        };
        format!("{first17}{check}")
    }

    /// 补上 Luhn 校验位，同样另写一遍。
    fn luhn_done(body: &str) -> String {
        let sum: u32 = body
            .bytes()
            .rev()
            .enumerate()
            .map(|(i, c)| {
                let x = u32::from(c - b'0');
                if i % 2 == 0 {
                    (x * 2) / 10 + (x * 2) % 10
                } else {
                    x
                }
            })
            .sum();
        format!("{body}{}", (10 - sum % 10) % 10)
    }

    /// 以 `prefix` 开头、一共 `len` 位、过得了 Luhn 的一个号码。中间填的不是 0：
    /// `4` 后面一串 0 正好是 Stripe 的一张测试卡。
    fn card_of(prefix: u32, len: u8) -> String {
        let mut body = prefix.to_string();
        let fill = b"8642097531";
        while body.len() < usize::from(len) - 1 {
            body.push(char::from(fill[body.len() % fill.len()]));
        }
        luhn_done(&body)
    }

    fn personal() -> RuleSet {
        RuleSet::only(&["cn-resident-id", "bank-card"])
    }

    fn personal_found(text: &str) -> Vec<(String, String)> {
        scan(text, &personal())
            .into_iter()
            .map(|h| (h.rule.id().to_string(), text[h.bytes].to_string()))
            .collect()
    }

    #[test]
    fn the_test_helpers_agree_with_published_numbers() {
        // 两个补校验位的函数先对上公开的号码，后面的测试才站得住：GB 11643 里的
        // 示例号码，和 Stripe 的一张测试卡
        assert_eq!(id_done("11010519491231002"), "11010519491231002X");
        assert_eq!(id_done("44052418800101001"), "440524188001010014");
        assert_eq!(luhn_done("424242424242424"), "4242424242424242");
        assert_eq!(luhn_done("37828224631000"), "378282246310005");
    }

    #[test]
    fn a_resident_id_number_is_replaced_and_comes_back() {
        for id in [
            "11010519491231002X",
            "11010519491231002x",
            "110105199003071239",
            // 2000 年有 2 月 29 日
            "310104200002294568",
            "440306198510157896",
            "650102190001010016",
            // 台湾、香港、澳门
            "71000019991231444X",
            "810000197001015558",
            "820000201008083335",
        ] {
            let t = format!("身份证号：{id}，请核对");
            assert_eq!(
                personal_found(&t),
                vec![("cn-resident-id".to_string(), id.to_string())],
                "{t}"
            );
            let r = crate::redact::replace::redact(
                &t,
                &RuleSet::defaults(),
                crate::redact::replace::Ledger::new(crate::redact::replace::Scheme::SECRET),
            );
            assert_eq!(r.text, "身份证号：<<ID_NUMBER_1>>，请核对", "{id}");
            assert_eq!(crate::redact::replace::restore(&r.text, &r.ledger), t);
        }
    }

    #[test]
    fn an_id_number_that_does_not_check_out_is_left_alone() {
        let today = today_ymd();
        let next_year = today / 10_000 + 1;
        for (why, id) in [
            ("校验码不对", "110105199003071238".to_string()),
            ("校验码不对", "11010519491231002Y".to_string()),
            ("1900 年不是闰年", id_done("11010519000229001")),
            ("2 月没有 30 日", id_done("11010519900230001")),
            ("没有 13 月", id_done("11010519901301001")),
            ("没有 0 日", id_done("11010519900100001")),
            ("4 月没有 31 日", id_done("11010519900431001")),
            ("1900 年以前", id_done("11010518991231001")),
            ("还没出生", id_done(&format!("110105{next_year}0101001"))),
            ("15 位的老号码", "110105491231002".to_string()),
            ("17 位", "11010519491231002".to_string()),
        ] {
            assert!(
                personal_found(&id).is_empty(),
                "{why}：{id} → {:?}",
                personal_found(&id)
            );
        }
        // 地区码不在 GB/T 2260 的省级名单里 —— 其余几样都对
        for code in [
            10, 16, 20, 30, 38, 40, 47, 55, 60, 66, 70, 72, 80, 83, 90, 91, 99,
        ] {
            let id = id_done(&format!("{code}010519900307123"));
            assert!(personal_found(&id).is_empty(), "地区码 {code}：{id}");
        }
    }

    #[test]
    fn the_birth_date_is_checked_against_today() {
        // 今天出生的算，明天的不算
        let today = today_ymd();
        assert!(resident_id(
            id_done(&format!("110105{today}001")).as_bytes(),
            today
        ));
        assert!(!resident_id(
            id_done(&format!("110105{today}001")).as_bytes(),
            today - 1
        ));
    }

    #[test]
    fn day_counting_lands_on_real_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(11_016), (2000, 2, 29));
        assert_eq!(civil(20_727), (2026, 10, 1));
        assert_eq!(days_in(1900, 2), 28);
        assert_eq!(days_in(2000, 2), 29);
        assert_eq!(days_in(2024, 2), 29);
        assert_eq!(days_in(2026, 2), 28);
    }

    #[test]
    fn a_card_number_is_replaced_and_comes_back() {
        for card in [
            "6222021234567894",
            "62220212345678903",
            "622202123456789012",
            "6222021234567890128",
            "4532015112830366",
            "4532015112830360008",
            "5500000000000004",
            "2221000000000017",
            "2720999999999996",
            "340000000000009",
            "3528000000000007",
            "6011000000000004",
            "6500000000000002",
            "30000000000004",
            "36000000000008",
        ] {
            let t = format!("卡号 {card} 尾号对一下");
            assert_eq!(
                personal_found(&t),
                vec![("bank-card".to_string(), card.to_string())],
                "{t}"
            );
            let r = crate::redact::replace::redact(
                &t,
                &RuleSet::defaults(),
                crate::redact::replace::Ledger::new(crate::redact::replace::Scheme::SECRET),
            );
            assert_eq!(r.text, "卡号 <<CARD_NUMBER_1>> 尾号对一下", "{card}");
            assert_eq!(crate::redact::replace::restore(&r.text, &r.ledger), t);
        }
    }

    #[test]
    fn a_card_number_that_does_not_check_out_is_left_alone() {
        // 除了头两个，都是过得了 Luhn 的：挡住它们的是号段和位数
        for (why, card) in [
            ("Luhn 不对", "6222021234567895"),
            ("Luhn 不对", "4532015112830367"),
            ("Visa 没有 17 位的", "45320151128303665"),
            ("Visa 没有 15 位的", "453201511283034"),
            ("American Express 只有 15 位", "3400000000000000"),
            ("Mastercard 只有 16 位", "550000000000004"),
            ("Diners Club 只有 14 位", "3000000000000004"),
            ("银联最短 16 位", "622202123456782"),
            ("2220 不在 Mastercard 的号段里", "2220000000000018"),
            ("2721 也不在", "2721000000000012"),
            ("3527 不是 JCB", "3527000000000008"),
            ("643 不是 Discover", "6430000000000007"),
            ("306 不是 Diners Club", "30600000000001"),
            ("没有哪家以 1 开头", "1000000000000008"),
            ("位数超过 19", "62220212345678901234"),
        ] {
            assert!(
                personal_found(card).is_empty(),
                "{why}：{card} → {:?}",
                personal_found(card)
            );
        }
    }

    #[test]
    fn a_number_inside_a_longer_run_or_an_encoded_string_is_not_a_number() {
        // 在 token 边界上认：更长的数字串、十六进制、base64 里的那一段不是一个号码
        let card = "6222021234567894";
        let id = "11010519491231002X";
        for t in [
            format!("9{card}"),
            format!("{card}0"),
            format!("1{id}"),
            format!("{id}1"),
            format!("0x{card}"),
            format!("deadbeef{card}cafe"),
            format!("sha256:9f86d081{id}884c7d659a2f"),
            format!("data:image/png;base64,iVBORw0KGgo{card}AAAANSUhEUg=="),
            format!("QUJD{id}RUZH"),
            format!("v{card}"),
            format!("{card}_backup"),
            format!("-{card}"),
            format!("{card}.5"),
        ] {
            assert!(
                personal_found(&t).is_empty(),
                "{t} → {:?}",
                personal_found(&t)
            );
        }
    }

    #[test]
    fn a_number_right_after_a_json_escape_or_a_chinese_colon_is_found() {
        // 扫的是请求体：换行是 `\n`，Python 写的客户端把中文冒号写成 `：`
        let card = "6222021234567894";
        let id = "11010519491231002X";
        for (t, rule, value) in [
            (format!(r"卡号：\n{card}\n谢谢"), "bank-card", card),
            (format!(r"\t{card}"), "bank-card", card),
            (format!("身份证号：{id}"), "cn-resident-id", id),
            (format!(r"身份证号：{id}"), "cn-resident-id", id),
            (
                format!("持卡人张三，卡号：{card}，有效期至 2028 年"),
                "bank-card",
                card,
            ),
            (format!(r#"{{"card":"{card}","id":"{id}"}}"#), "", ""),
        ] {
            let got = personal_found(&t);
            if rule.is_empty() {
                // 两个都在
                assert_eq!(got.len(), 2, "{t} → {got:?}");
                continue;
            }
            assert_eq!(got, vec![(rule.to_string(), value.to_string())], "{t}");
        }
    }

    #[test]
    fn a_full_stop_after_a_number_does_not_hide_it() {
        let card = "6222021234567894";
        let id = "11010519491231002X";
        assert_eq!(
            personal_found(&format!("My card is {card}.")),
            vec![("bank-card".to_string(), card.to_string())]
        );
        assert_eq!(
            personal_found(&format!("ID: {id}. Next")),
            vec![("cn-resident-id".to_string(), id.to_string())]
        );
        assert_eq!(
            personal_found("Card 6222 0212 3456 7894..."),
            vec![("bank-card".to_string(), "6222 0212 3456 7894".to_string())]
        );
    }

    #[test]
    fn a_card_written_in_groups_is_one_value() {
        for card in [
            "6222 0212 3456 7894",
            "6222-0212-3456-7894",
            "6222 0212 3456 7890 128",
            "6222-0212-3456-7890-128",
            "6222 0212 3456 7890 3",
            // American Express 4-6-5、Diners Club 4-6-4，和四位一组的写法
            "3400 000000 00009",
            "3400 0000 0000 009",
            "3000 000000 0004",
            "3000-0000-0000-04",
        ] {
            let t = format!("卡号 {card}，到期 12/28");
            assert_eq!(
                personal_found(&t),
                vec![("bank-card".to_string(), card.to_string())],
                "{t}"
            );
            let r = crate::redact::replace::redact(
                &t,
                &RuleSet::defaults(),
                crate::redact::replace::Ledger::new(crate::redact::replace::Scheme::SECRET),
            );
            assert_eq!(r.ledger.len(), 1, "{}", r.text);
            assert_eq!(crate::redact::replace::restore(&r.text, &r.ledger), t);
        }
    }

    #[test]
    fn grouping_has_to_be_regular_and_consistent() {
        for t in [
            // 两种分隔符混着用
            "6222-0212 3456-7894",
            "6222 0212-3456-7894",
            // 两个空格、换行、制表符隔开的是几个数
            "6222  0212  3456  7894",
            r"6222\n0212\n3456\n7894",
            r"6222\t0212\t3456\t7894",
            "6222--0212--3456--7894",
            // 组不是四位一组
            "622 2021 2345 6789 4",
            "62220 2123 4567 894",
            "622202 1234567894",
            "62220212 34567894",
            "6222-021234567894",
            // 一张四位数的表：截一段出来正好能过的那几行也不算
            "1234 5678 6222 0212 3456 7894 0000",
        ] {
            assert!(
                personal_found(t).is_empty(),
                "{t} → {:?}",
                personal_found(t)
            );
        }
    }

    #[test]
    fn a_month_or_a_security_code_right_after_a_card_is_not_part_of_it() {
        // 卡号后面紧跟着的短数字（有效期的月份、安全码）不是号码的一部分
        for t in [
            "6222 0212 3456 7894 12/28",
            "6222 0212 3456 7894 123",
            "4532 0151 1283 0366 12/28 123",
        ] {
            let got = personal_found(t);
            assert_eq!(got.len(), 1, "{t} → {got:?}");
            assert!(t.starts_with(&got[0].1), "{t} → {got:?}");
            assert_eq!(got[0].1.len(), 19, "{t} → {got:?}");
        }
    }

    #[test]
    fn public_test_cards_are_not_replaced() {
        // 开发者天天往代码里贴它们，换掉纯属打扰。题目里点名的那几张，
        // 连着写、分组写都不认
        for card in [
            "4242424242424242",
            "4111111111111111",
            "4012888888881881",
            "4000056655665556",
            "5555555555554444",
            "5105105105105100",
            "2223003122003222",
            "5200828282828210",
            "378282246310005",
            "371449635398431",
            "6011111111111117",
            "6011000990139424",
            "3056930009020004",
            "36227206271667",
            "3566002020360505",
            "6200000000000005",
            "6205500000000000004",
        ] {
            assert!(personal_found(card).is_empty(), "{card}");
            let spaced: Vec<String> = card
                .as_bytes()
                .chunks(4)
                .map(|c| String::from_utf8(c.to_vec()).unwrap())
                .collect();
            for sep in [" ", "-"] {
                let t = spaced.join(sep);
                assert!(personal_found(&t).is_empty(), "{t}");
            }
        }
    }

    #[test]
    fn every_listed_test_card_is_one_the_rule_would_otherwise_take() {
        // 认不出的用不着排除，列着只会让人以为它有用
        let mut seen = HashSet::new();
        for t in TEST_CARDS {
            assert!(seen.insert(*t), "测试卡号列了两遍：{t}");
            let d = t.as_bytes();
            assert!(
                card_network(d).is_some() && luhn(d),
                "{t} 本来就认不出，不用列"
            );
        }
    }

    #[test]
    fn numbers_that_merely_look_long_are_left_alone() {
        // **宁可漏，不可吵。**这些都是请求里天天出现的长数字
        for t in [
            // 毫秒、微秒、纳秒时间戳
            "\"created\": 1727712345678",
            "ts=1759300000000000",
            "1759300000000000000",
            // 雪花 ID：开头不属于任何一家，或者开头对得上、Luhn 过不了
            "tweet 1453987635477184512",
            "id: 1846987139428634624",
            "4123456789012345678",
            "620000000000000001",
            "622848012345678901",
            // 订单号
            "订单号 202310011234567890",
            "订单 20231001123456789 已发货",
            "3456789012345678901",
            "E202310011234567",
            // 电话号码
            "13800138000",
            "+86 138 0013 8000",
            "+86-138-0013-8000",
            "021-6234-5678",
            "(021) 6234 5678",
            "400-820-8820",
            // UUID
            "550e8400-e29b-41d4-a716-446655440000",
            "12345678-1234-5678-1234-567812345678",
            // JavaScript 的整数上限、版本号、IP
            "9007199254740991",
            "1.2.3.4567890123456",
        ] {
            assert!(
                personal_found(t).is_empty(),
                "误报了：{t} → {:?}",
                personal_found(t)
            );
        }
    }

    #[test]
    fn an_id_number_that_also_passes_luhn_stays_an_id_number() {
        // 甘肃的号码以 62 开头，这一个碰巧也过得了 Luhn、位数也对得上银联
        let id = "620102198811110554";
        assert_eq!(
            personal_found(id),
            vec![("cn-resident-id".to_string(), id.to_string())]
        );
        // 关掉身份证号那条，是不要换身份证号 —— 不是让它当成卡号换掉
        let no_id = RuleSet::build(&[], &["cn-resident-id".into()], []).unwrap();
        assert!(scan(id, &no_id).is_empty(), "{:?}", scan(id, &no_id));
    }

    #[test]
    fn switching_one_personal_rule_off_leaves_the_other() {
        let t = "身份证 11010519491231002X，卡号 6222 0212 3456 7894";
        let both = scan(t, &RuleSet::defaults());
        assert_eq!(both.len(), 2, "{both:?}");
        let no_id = RuleSet::build(&[], &["cn-resident-id".into()], []).unwrap();
        let got = scan(t, &no_id);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].rule, Rule::Builtin("bank-card"));
        let no_card = RuleSet::build(&[], &["bank-card".into()], []).unwrap();
        let got = scan(t, &no_card);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].rule, Rule::Builtin("cn-resident-id"));
        let neither =
            RuleSet::build(&[], &["bank-card".into(), "cn-resident-id".into()], []).unwrap();
        assert!(scan(t, &neither).is_empty());
    }

    #[test]
    fn a_rule_set_built_from_nothing_finds_no_personal_numbers() {
        // 企业版从 `RuleSet::none()` 加自己的正则建规则集：新加的出厂规则不能
        // 混进去，否则它脱的东西就变了
        let t = "身份证 11010519491231002X，卡号 6222021234567894";
        assert!(scan(t, &RuleSet::none()).is_empty());
        let mine = RuleSet::none()
            .with_labeled("email", r"[a-z]+@[a-z]+\.com", Some("EMAIL"))
            .unwrap();
        assert!(scan_text(t, &mine).is_empty());
    }

    #[test]
    fn a_reported_personal_number_keeps_only_its_last_four() {
        // 「留头 5」留下的是身份证号的地区码、卡号的发卡行
        let id = Rule::Builtin("cn-resident-id");
        let card = Rule::Builtin("bank-card");
        assert_eq!(masked(&id, "11010519491231002X"), "…002X");
        assert_eq!(masked(&card, "6222021234567894"), "…7894");
        assert_eq!(masked(&card, "6222 0212 3456 7894"), "…7894");
        assert_eq!(masked(&card, "6222-0212-3456-7890-128"), "…0128");
        let t = "卡号 6222 0212 3456 7894，再说一遍：6222 0212 3456 7894";
        let f = findings(t, &scan(t, &RuleSet::defaults()));
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].masked, "…7894");
        assert_eq!(f[0].count, 2);
        assert_eq!(f[0].rule.kind(), Kind::Personal);
    }

    #[test]
    fn personal_placeholders_say_what_they_were_and_count_per_label() {
        // 模型看到 `<<ID_NUMBER_1>>` 才知道那里是个身份证号；编号按标签各数各的，
        // 和凭据的 `<<TW_SECRET_n>>` 互不相干
        let key = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";
        let body = serde_json::json!({
            "content": format!(
                "key {key}\n身份证 11010519491231002X\n卡 6222 0212 3456 7894\n另一张 4532-0151-1283-0366\n身份证 11010519491231002X"
            )
        })
        .to_string();
        let r = crate::redact::replace::redact(
            &body,
            &RuleSet::defaults(),
            crate::redact::replace::Ledger::new(crate::redact::replace::Scheme::SECRET),
        );
        let v: serde_json::Value = serde_json::from_str(&r.text).expect("换完不是合法 JSON");
        assert_eq!(
            v["content"],
            "key <<TW_SECRET_1>>\n身份证 <<ID_NUMBER_1>>\n卡 <<CARD_NUMBER_1>>\n另一张 <<CARD_NUMBER_2>>\n身份证 <<ID_NUMBER_1>>"
        );
        assert_eq!(crate::redact::replace::restore(&r.text, &r.ledger), body);
        // 流式还原认得带标签的占位符，切成一个字符一段也拼得回来
        let mut s = crate::redact::stream::Restorer::new(&r.ledger);
        let echoed = "你的卡号 <<CARD_NUMBER_1>> 和身份证 <<ID_NUMBER_1>>。";
        let mut out = String::new();
        for c in echoed.chars() {
            out.push_str(&s.process(&c.to_string()));
        }
        out.push_str(&s.flush());
        assert_eq!(
            out,
            "你的卡号 6222 0212 3456 7894 和身份证 11010519491231002X。"
        );
    }

    #[test]
    fn the_ui_test_box_marks_a_grouped_card_where_it_was_typed() {
        // 界面上的「测试」扫的是编成请求体的样子，区间要换算回用户贴的原文
        let text = "第一行\n卡号：6222 0212 3456 7894\n身份证：11010519491231002X";
        let got: Vec<&str> = scan_plain(text, &RuleSet::defaults())
            .iter()
            .map(|h| &text[h.bytes.clone()])
            .collect();
        assert_eq!(got, vec!["6222 0212 3456 7894", "11010519491231002X"]);
    }
}
