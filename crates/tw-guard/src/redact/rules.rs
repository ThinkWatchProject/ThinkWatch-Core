//! 认得出哪些东西是凭据，外加几种个人信息。
//!
//! **两个产品共用这一份目录。**个人开发者关心的是：**我的 API key、私钥、内网地址，
//! 会不会被中转站顺走**；企业关心合规 —— 客户的身份证号、手机号、邮箱不能流到第三方
//! 模型。凭据规则出厂就开；个人信息里只有能在结构上核对的两种出厂就开：
//!
//! - **身份证号和银行卡号**，是产品上点名要的：一旦漏出去就收不回来，而且都能在
//!   结构上核对 —— 身份证号有省级地区码、真实的出生日期和 MOD 11-2 校验码，卡号有
//!   卡组织的号段、位数和 Luhn 校验 —— 达得到和凭据规则同一道门槛；
//! - **邮箱和中国大陆手机号**出厂关着：它们没有能核对的结构，只能按「长得像」认，
//!   在代码和文档里误报得多。要的人（企业版的合规场景）自己打开。
//!
//! 个人信息自成几类（[`Kind::is_personal`]），占位符写明是哪一种，报出去只留认得出是
//! 哪一个的最少部分（号码留最后四位，邮箱留第一个字和域名）。
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

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

use super::anchor::{
    ALNUM, ANY_LEN, Anchor, Anchors, Bytes, C_DASH, C_DIGIT, C_DOT, C_LETTER, C_UNDER, Chars, DIGIT,
};
use super::secrets::ASSIGNMENT_SUFFIXES;
pub use super::secrets::saved::{
    SAVED_MIN_CHARS, Saved, SavedSecret, SavedSource, SavedSourceKind,
};

/// 规则属于哪一类。**只用来分组和说明**，开关按条。界面上按这个顺序分组。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// 用户在 ThinkWatch 里保存的凭据，按值认（上游密钥、访问密钥、代理口令……）
    Saved,
    /// AI 服务的 API 密钥（`sk-ant-`、`xai-`、`gsk_` …）
    AiServices,
    /// 代码托管与软件发布（GitHub、GitLab、npm、PyPI …）
    DevPlatforms,
    /// 云服务与基础设施（AWS、Google Cloud、阿里云、Vault …）
    Cloud,
    /// 支付、通信与协作（Stripe、Slack、Shopify、Notion …）
    Business,
    /// `-----BEGIN ... PRIVATE KEY-----` 到 END 的整段
    PrivateKeys,
    /// 三段式 base64url，且首段解出来含 `"alg"`
    Jwt,
    /// `postgres://user:pass@` 这类带口令的 URI。**只换口令那一段**
    ConnStrings,
    /// 按出现的位置认的凭据：请求头、命令行、URL 参数、机器人 Webhook、配置赋值。
    /// **只换值那一段**
    InContext,
    /// 加密货币的助记词、私钥
    Crypto,
    /// 证件号码（身份证、护照、港澳台证件……）
    IdDocuments,
    /// 境外证件号码，每国一条
    ForeignIds,
    /// 银行卡号、IBAN
    Financial,
    /// 手机号、固定电话、邮箱
    Contact,
    /// 详细地址、精确坐标
    Location,
    /// 车牌号、车辆识别代号
    Vehicle,
    /// IMEI、MAC 地址、公网 IP
    Device,
    /// 按字段名认的个人信息（姓名、住址、出生日期、账号、健康）
    LabeledPersonal,
    /// RFC1918 地址、`.local` / `.internal` 域名
    Internal,
    /// 用户自己写的规则
    Custom,
}

impl Kind {
    pub fn slug(&self) -> &'static str {
        match self {
            Kind::Saved => "saved",
            Kind::AiServices => "ai-services",
            Kind::DevPlatforms => "dev-platforms",
            Kind::Cloud => "cloud",
            Kind::Business => "business",
            Kind::PrivateKeys => "private-keys",
            Kind::Jwt => "jwt",
            Kind::ConnStrings => "conn-strings",
            Kind::InContext => "in-context",
            Kind::Crypto => "crypto",
            Kind::IdDocuments => "id-documents",
            Kind::ForeignIds => "foreign-ids",
            Kind::Financial => "financial",
            Kind::Contact => "contact",
            Kind::Location => "location",
            Kind::Vehicle => "vehicle",
            Kind::Device => "device",
            Kind::LabeledPersonal => "labeled-personal",
            Kind::Internal => "internal",
            Kind::Custom => "custom",
        }
    }

    /// 分组的英文名（`data/redact.yaml` 的 `kinds`）
    pub fn name(&self) -> &'static str {
        self.names().name
    }

    /// 分组的中文名
    pub fn name_zh(&self) -> &'static str {
        self.names().name_zh
    }

    fn names(&self) -> &'static KindName {
        KIND_NAMES
            .iter()
            .find(|n| n.kind == *self)
            // 每一类都在数据文件里，测试钉着（`every_kind_has_names_in_the_order_the_ui_groups_by`）
            .expect("every kind is named in data/redact.yaml")
    }

    /// 个人信息，不是凭据：占位符写明是哪一种，报出去只留认得出是哪一个的最少部分
    pub fn is_personal(&self) -> bool {
        matches!(
            self,
            Kind::IdDocuments
                | Kind::ForeignIds
                | Kind::Financial
                | Kind::Contact
                | Kind::Location
                | Kind::Vehicle
                | Kind::Device
                | Kind::LabeledPersonal
        )
    }

    /// 拿到就能用的凭据：危险命令「凭据发往陌生主机」认的那几类。JWT、连接串、按位置认的
    /// 不在里面（见 `tools::net::credentials`）
    pub fn is_credential(&self) -> bool {
        matches!(
            self,
            Kind::Saved
                | Kind::AiServices
                | Kind::DevPlatforms
                | Kind::Cloud
                | Kind::Business
                | Kind::PrivateKeys
                | Kind::Crypto
        )
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
    /// 内网地址：RFC1918 私有地址、运营商级 NAT（`100.64.0.0/10`）、IPv6 唯一本地地址
    /// （`fc00::/7`），不含回环
    PrivateIp,
    /// 以这几个后缀结尾的域名
    DomainSuffix { suffixes: &'static [&'static str] },
    /// 18 位的中华人民共和国居民身份证号码：头两位是省级行政区划代码（含港澳台居民居住证的
    /// `81`、`82`、`83`；新版外国人永久居留身份证首位是 `9`、第 2–3 位是省级代码、第 4–6 位是
    /// 国籍代码），第 7–14 位是 `born_since` 年 1 月 1 日到今天之间的真实日期，末位是对得上的
    /// ISO 7064 MOD 11-2 校验码（`0`–`9` 或 `X`）。15 位的老号码另有一条规则
    CnResidentId { born_since: u16 },
    /// 卡号：开头和位数属于其中一家卡组织，并且通过 Luhn 校验。连着写的，或者
    /// 四位一组、用一个空格或一个连字符隔开的（最后一组可以不足四位；American
    /// Express 另有 4-6-5、Diners Club 另有 4-6-4）。公开的测试卡号不算
    BankCard { networks: &'static [CardNetwork] },
    /// 邮箱地址：`本地部分@域名`，域名至少两段、最后一段是两个以上的字母。URL 里的
    /// 用户名（`https://user@host`、`user:pass@host`）和 `icon@2x.png` 这类文件名不算
    Email,
    /// 中国大陆手机号：11 位数字，`1` 开头、第二位是 `3`–`9`，前后不紧挨别的数字；也认
    /// `+86` / `0086` / `86-` 前缀，和 3-4-4 用空格或连字符分组的写法
    CnMobilePhone,
    /// 以其中任一个前缀开头，其后至少还有 `min_tail` 个字符，尾巴的字符按 `tail`。同一家
    /// 服务的几种令牌写成一条
    Prefixes { list: &'static [Pfx] },
    /// 用户在 ThinkWatch 里保存的凭据，按值认，与格式无关
    Saved,
    /// 配置赋值：变量名以其中一个词结尾的字面量值
    Assignment { suffixes: &'static [&'static str] },
    /// 按字段名认的值：字段名是其中一个
    Labeled { names: &'static [&'static str] },
    /// 代码里实现的判据（校验位、词表、位置……），没有可展示的模式；说明是规则的 `about`
    Code,
}

/// [`Matcher::Prefixes`] 里的一个前缀。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pfx {
    pub prefix: &'static str,
    pub min_tail: usize,
    pub tail: Tail,
}

/// 前缀后面那段允许哪些字符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tail {
    /// token 里能有的都行（字母、数字、`-`、`_`、`.`）
    Any,
    /// 只有十六进制数字
    Hex,
    /// 只有字母和数字
    Alnum,
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
    /// 英文名
    pub name: &'static str,
    /// 中文名
    pub name_zh: &'static str,
    /// 认什么，给人看的一句话（英文）。按位置认的写明只换哪一段；反引号括起来的一段是代码。
    /// 前缀类的规则由 build.rs 按前缀和长度生成
    pub about: &'static str,
    /// 同上，中文
    pub about_zh: &'static str,
    /// 出厂时开不开。**关着的是内网地址和邮箱、手机号**：它们在代码和文档里到处
    /// 都是，按「长得像」认误报得多，而危害远小于一把 key，想脱的人自己开
    pub on_by_default: bool,
    pub matcher: Matcher,
    /// 占位符里的标签。`None` 用账本的默认标签（桌面版是 `TW_SECRET`）。
    ///
    /// 个人号码写明是哪一种：模型看到 `<<TW_ID_NUMBER_1>>` 才知道那里原来是个
    /// 身份证号，答得像样
    pub label: Option<&'static str>,
    /// 这把凭据正当地会送往哪些服务商域名（后缀匹配，含子域）。危险命令「凭据发往陌生主机」
    /// 用它；空的（私钥、个人信息……）送往任何非本机地址都算外传
    pub hosts: &'static [&'static str],
}

/// `sk-` 开头的 OpenAI 老式 key 至少要多长。
///
/// 前缀太短，**要靠长度把它和 `sk-test`、`sk-xxx` 这种占位符分开**。
const OPENAI_MIN: usize = 40;

const INTERNAL_SUFFIXES: &[&str] = &[
    ".local",
    ".internal",
    ".lan",
    ".corp",
    ".home.arpa",
    ".intranet",
];

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

/// 全部内置规则，从 `data/redact.yaml` 编译期生成（见 build.rs）。界面上先按 [`Kind`] 的
/// 顺序分组，组内按这里的顺序（见 `view::redact`）；前缀表也按这里的顺序比。
///
/// API 密钥那一段**只收前缀明确、长度有下限的**。代码里的 hash、base64 的
/// 图片、UUID 全都长得像「一长串随机字符」，按那个判据匹配会疯狂误报。
pub const BUILTINS: &[Builtin] = include!(concat!(env!("OUT_DIR"), "/redact_builtins.rs"));

/// 一个分组叫什么（`data/redact.yaml` 的 `kinds`）。
#[derive(Debug, Clone, Copy)]
pub struct KindName {
    pub kind: Kind,
    pub name: &'static str,
    pub name_zh: &'static str,
}

/// 每个分组的名字，按界面上的顺序（和 [`Kind`] 的定义顺序一致，测试钉着）。
pub const KIND_NAMES: &[KindName] = include!(concat!(env!("OUT_DIR"), "/redact_kinds.rs"));

/// 判据写在代码里的规则：数据文件里没写前缀的那些按 id 在这里找自己的 [`Matcher`]。
///
/// **数据和代码各管一半**：名字、说明、分组、出厂开关在 `data/redact.yaml`，判据（校验位、
/// 词表、位置、整张号段表）在代码里，这张表把两边按 id 接起来。数据里有、这里没有的是编译错误
/// （[`detector`]）；这里有、数据里没有的由测试抓（`every_detector_has_a_rule_in_the_data`）。
/// 判据本身在哪个函数里：最早那一批在本文件，扩充的凭据在 `secrets/`，个人信息在 `personal/`；
/// 每一条都有样本测试（命中、不命中）钉着。
const DETECTORS: &[(&str, Matcher)] = &[
    // OpenAI 老式密钥的长度下限扫描时直接用 `OPENAI_MIN`，所以它算代码判据，不进数据文件
    (
        "openai-api-key",
        Matcher::OpenaiLegacy {
            min_len: OPENAI_MIN,
        },
    ),
    ("private-key", Matcher::Pem),
    ("jwt", Matcher::Jwt),
    ("conn-string-password", Matcher::ConnString),
    (
        "cn-resident-id",
        Matcher::CnResidentId {
            born_since: BORN_SINCE,
        },
    ),
    (
        "bank-card",
        Matcher::BankCard {
            networks: CARD_NETWORKS,
        },
    ),
    ("email", Matcher::Email),
    ("cn-mobile-phone", Matcher::CnMobilePhone),
    ("internal-ip", Matcher::PrivateIp),
    (
        "internal-domain",
        Matcher::DomainSuffix {
            suffixes: INTERNAL_SUFFIXES,
        },
    ),
    // 扩充的凭据规则（secrets/）
    ("thinkwatch-saved-secret", Matcher::Saved),
    ("zhipu-api-key", Matcher::Code),
    ("qianfan-api-key", Matcher::Code),
    ("sk-compat-key", Matcher::Code),
    ("azure-ad-client-secret", Matcher::Code),
    ("terraform-token", Matcher::Code),
    ("auth-header-credential", Matcher::Code),
    ("command-line-password", Matcher::Code),
    ("url-token", Matcher::Code),
    ("bot-webhook", Matcher::Code),
    ("telegram-bot-token", Matcher::Code),
    (
        "config-assignment",
        Matcher::Assignment {
            suffixes: ASSIGNMENT_SUFFIXES,
        },
    ),
    ("crypto-mnemonic", Matcher::Code),
    ("crypto-wif-key", Matcher::Code),
    ("crypto-extended-key", Matcher::Code),
    // 扩充的个人信息规则（personal/）
    ("cn-resident-id-15", Matcher::Code),
    ("passport-mrz", Matcher::Code),
    ("cn-passport", Matcher::Code),
    ("hk-macau-taiwan-id", Matcher::Code),
    ("mainland-travel-permit", Matcher::Code),
    ("us-ssn", Matcher::Code),
    ("uk-nino", Matcher::Code),
    ("sg-nric", Matcher::Code),
    ("kr-rrn", Matcher::Code),
    ("jp-my-number", Matcher::Code),
    ("in-aadhaar", Matcher::Code),
    ("br-cpf", Matcher::Code),
    ("iban", Matcher::Code),
    ("cn-landline", Matcher::Code),
    ("intl-phone", Matcher::Code),
    ("cn-address", Matcher::Code),
    ("geo-coordinates", Matcher::Code),
    ("cn-license-plate", Matcher::Code),
    ("vin", Matcher::Code),
    ("imei", Matcher::Code),
    ("mac-address", Matcher::Code),
    ("public-ip", Matcher::Code),
    (
        "labeled-name",
        Matcher::Labeled {
            names: super::personal::NAME_FIELDS,
        },
    ),
    (
        "labeled-address",
        Matcher::Labeled {
            names: super::personal::ADDRESS_FIELDS,
        },
    ),
    (
        "labeled-birth-date",
        Matcher::Labeled {
            names: super::personal::BIRTH_FIELDS,
        },
    ),
    (
        "labeled-account",
        Matcher::Labeled {
            names: super::personal::ACCOUNT_FIELDS,
        },
    ),
    (
        "labeled-health",
        Matcher::Labeled {
            names: super::personal::HEALTH_FIELDS,
        },
    ),
];

/// 数据文件里一条代码判据的规则在 [`DETECTORS`] 里的 [`Matcher`]。**在编译期求值**
/// （生成的 [`BUILTINS`] 里调它）：查不到就停在编译上，不会带着一条认不出任何东西的规则上线
const fn detector(id: &str) -> Matcher {
    let mut i = 0;
    while i < DETECTORS.len() {
        if same(DETECTORS[i].0, id) {
            return DETECTORS[i].1;
        }
        i += 1;
    }
    panic!("a rule in data/redact.yaml has no prefix and no detector in rules::DETECTORS")
}

/// 两个字符串相等。`==` 在 const fn 里用不了
const fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod catalog_tests;

pub fn builtin(id: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.id == id)
}

/// 内置规则的一处命中。**标签照规则表上写的**，不在各个找的地方各写一遍。
pub(super) fn builtin_hit(id: &'static str, bytes: Range<usize>) -> Hit {
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
            Rule::Builtin(id) => builtin(id).map_or(Kind::Custom, |b| b.kind),
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
    /// 写的样子和怎么认（[`Pattern`]），原样：安全日志记下命中那一刻的规则
    pub pattern: Pattern,
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

/// 一条自定义规则写的样子和怎么认。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    pub text: String,
    /// 是正则；不是就按原样找这段文字
    pub regex: bool,
    /// 分大小写
    pub case_sensitive: bool,
}

impl Pattern {
    /// 一段正则，分大小写
    pub fn regex(text: &str) -> Self {
        Self {
            text: text.to_string(),
            regex: true,
            case_sensitive: true,
        }
    }

    /// 编成正则：按原样找的那段文字先转义
    pub fn compile(&self, name: &str) -> Result<regex::Regex, BadPattern> {
        if self.text.is_empty() {
            return Err(BadPattern {
                name: name.to_string(),
                detail: "the pattern is empty".to_string(),
            });
        }
        let source = if self.regex {
            self.text.clone()
        } else {
            regex::escape(&self.text)
        };
        crate::bounded_with(&source, !self.case_sensitive).map_err(|e| BadPattern {
            name: name.to_string(),
            detail: e.to_string(),
        })
    }
}

/// 编一条正则。**长度和编译后的大小都有上限** —— 这条正则要在每个请求体
/// 上跑，一个写得很大的正则不该拖慢每一个请求。
pub fn compile(name: &str, pattern: &str) -> Result<regex::Regex, BadPattern> {
    Pattern::regex(pattern).compile(name)
}

/// 这一次按哪些规则找。
///
/// **运行时建一次、跟着配置一起换**，不在每个请求上现编正则。
#[derive(Debug, Clone)]
pub struct RuleSet {
    on: HashSet<&'static str>,
    custom: Vec<Custom>,
    /// 动作是「仅记录」的规则：照样找、照样报，不换成占位符（见 [`RuleSet::records`]）
    record: HashSet<Rule>,
    /// 用户在 ThinkWatch 里保存的凭据（见 [`RuleSet::with_saved`]）。规则 `thinkwatch-saved-secret`
    /// 开着才按它找
    saved: Option<Arc<Saved>>,
    /// 自定义规则编成的预筛（[`CustomIndex`]），头一次扫的时候编。改了自定义规则就作废
    index: OnceLock<Arc<CustomIndex>>,
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
            record: HashSet::new(),
            saved: None,
            index: OnceLock::new(),
        }
    }

    /// 一条都不开。测试和「只试这一条正则」用。
    pub fn none() -> Self {
        Self {
            on: HashSet::new(),
            custom: Vec::new(),
            record: HashSet::new(),
            saved: None,
            index: OnceLock::new(),
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
            record: HashSet::new(),
            saved: None,
            index: OnceLock::new(),
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
            set = set.with_labeled(name, pattern, None)?;
        }
        Ok(set)
    }

    /// 再加一条自定义规则。
    pub fn with_custom(self, name: &str, pattern: &str) -> Result<Self, BadPattern> {
        self.with_labeled(name, pattern, None)
    }

    /// 再加一条正则写的自定义规则，占位符用它自己的标签：`{{EMAIL_1}}` 里的 `EMAIL`。
    pub fn with_labeled(
        self,
        name: &str,
        pattern: &str,
        label: Option<&str>,
    ) -> Result<Self, BadPattern> {
        self.with_rule(name, Pattern::regex(pattern), label)
    }

    /// 再加一条自定义规则：按原样找一段文字或者按正则，分不分大小写由 `pattern` 说。
    pub fn with_rule(
        mut self,
        name: &str,
        pattern: Pattern,
        label: Option<&str>,
    ) -> Result<Self, BadPattern> {
        self.custom.push(Custom {
            name: Arc::from(name),
            re: pattern.compile(name)?,
            pattern,
            label: label.map(Arc::from),
        });
        self.index = OnceLock::new();
        Ok(self)
    }

    pub fn is_on(&self, id: &str) -> bool {
        self.on.contains(id)
    }

    /// 带上用户在 ThinkWatch 里保存的凭据，按值认（规则 `thinkwatch-saved-secret`）。不足
    /// [`SAVED_MIN_CHARS`] 个字符的不收；一个都不剩就和没带一样。**跟着配置一起换**：调用方
    /// 在建运行时的地方建一次
    pub fn with_saved(mut self, list: Vec<SavedSecret>) -> Self {
        self.saved = Saved::new(list).map(Arc::new);
        self
    }

    /// 带上一份已经编好的保存的凭据（和网关、工具调用审查共用同一份）。`None` 是不带
    pub fn with_saved_set(mut self, saved: Option<Arc<Saved>>) -> Self {
        self.saved = saved;
        self
    }

    /// 编好的保存的凭据，给工具调用审查的「凭据发往陌生主机」用（见
    /// [`crate::tools::rules::Rules::with_saved`]）。没有是 `None`
    pub fn saved(&self) -> Option<&Arc<Saved>> {
        self.saved.as_ref()
    }

    /// 命中的这个值是哪一项保存的凭据。不是保存的凭据是 `None`
    pub fn saved_source(&self, value: &str) -> Option<&SavedSource> {
        self.saved.as_ref()?.source_of(value)
    }

    /// 这条规则只记录：动作是「仅记录」
    pub fn recording(mut self, rule: Rule) -> Self {
        self.record.insert(rule);
        self
    }

    /// 这条规则只记录、不换。**照样找、照样报**：命中报出去，只是原值照发，不进账本
    pub fn records(&self, rule: &Rule) -> bool {
        !self.record.is_empty() && self.record.contains(rule)
    }

    /// 只有只记录的那几条规则（内置的照这一份的开关）。没有只记录的规则是 `None`
    pub fn recording_only(&self) -> Option<RuleSet> {
        if self.record.is_empty() {
            return None;
        }
        Some(RuleSet {
            on: self
                .on
                .iter()
                .copied()
                .filter(|id| self.record.contains(&Rule::Builtin(id)))
                .collect(),
            custom: self
                .custom
                .iter()
                .filter(|c| self.record.contains(&Rule::Custom(c.name.clone())))
                .cloned()
                .collect(),
            record: self.record.clone(),
            saved: self.saved.clone(),
            index: OnceLock::new(),
        })
    }

    /// 要换成占位符的那几处：去掉只记录的规则命中的（见 [`Self::records`]）。没有只记录的
    /// 规则时原样借出去，不抄一份
    pub fn to_replace<'h>(&self, hits: &'h [Hit]) -> std::borrow::Cow<'h, [Hit]> {
        if self.record.is_empty() || !hits.iter().any(|h| self.records(&h.rule)) {
            return std::borrow::Cow::Borrowed(hits);
        }
        std::borrow::Cow::Owned(
            hits.iter()
                .filter(|h| !self.records(&h.rule))
                .cloned()
                .collect(),
        )
    }

    /// 名字是 `name` 的自定义规则**写的样子和怎么认**。没有这条规则是 `None`。安全日志记下
    /// 命中那一刻的规则：之后改了、删了，那一条说的还是当时那一版
    pub fn custom_pattern(&self, name: &str) -> Option<&Pattern> {
        self.custom
            .iter()
            .find(|c| &*c.name == name)
            .map(|c| &c.pattern)
    }

    /// 有自定义规则
    pub(super) fn has_custom(&self) -> bool {
        !self.custom.is_empty()
    }

    /// 一条都没开。**调用方据此整条短路** —— 没有规则的话，找都不用找。
    pub fn is_empty(&self) -> bool {
        self.on.is_empty() && self.custom.is_empty()
    }

    /// 这套规则的指纹（SHA-256）：**找出来的命中可能因之而变的每一样**都在里面 —— 开着哪些
    /// 内置规则、每条自定义规则（顺序、写法、怎么认、标签）、哪些只记录、保存的凭据的值。
    /// 两套规则指纹一样，在同一段文字上找出来的就一样。扫描缓存（[`super::cache`]）拿它做键的
    /// 一部分：改了规则、改了保存的密钥，以前存下的结果整个不作数。
    ///
    /// **字段一个一个拆开写、不留 `..`**：往 [`RuleSet`] 里加一样会影响命中的东西（例外值……）
    /// 时，这里编译不过，逼着把它也喂进指纹 —— 漏了的话，缓存会把改之前的结果当成改之后的。
    /// 只影响报法、不影响命中的（编好的预筛）写成 `_`
    pub fn fingerprint(&self) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let RuleSet {
            on,
            custom,
            record,
            saved,
            index: _,
        } = self;
        let mut h = Sha256::new();
        // 每一段先写长度：`ab` + `c` 和 `a` + `bc` 不是同一个指纹
        let mut put = |tag: &[u8], b: &[u8]| {
            h.update(tag);
            h.update((b.len() as u64).to_le_bytes());
            h.update(b);
        };
        // HashSet 的顺序每个实例都不一样，排好再写
        let mut ids: Vec<&str> = on.iter().copied().collect();
        ids.sort_unstable();
        for id in ids {
            put(b"on", id.as_bytes());
        }
        // 自定义规则的顺序算数：同一处两条都认得时，合成一处后报的是先记下的那条
        for c in custom {
            put(b"custom", c.name.as_bytes());
            put(b"pattern", c.pattern.text.as_bytes());
            put(
                b"how",
                &[
                    u8::from(c.pattern.regex),
                    u8::from(c.pattern.case_sensitive),
                ],
            );
            put(b"label", c.label.as_deref().unwrap_or("").as_bytes());
            put(b"labeled", &[u8::from(c.label.is_some())]);
        }
        let mut rec: Vec<(bool, &str)> = record.iter().map(|r| (r.custom(), r.id())).collect();
        rec.sort_unstable();
        for (custom, id) in rec {
            put(b"record", &[u8::from(custom)]);
            put(b"record-id", id.as_bytes());
        }
        if let Some(saved) = saved {
            for v in saved.values() {
                put(b"saved", v.as_bytes());
            }
        }
        h.finalize().into()
    }
}

/// 一个字节是不是 token 的一部分。**token 只由 ASCII 组成**，所以按字节切和按字符切切出来的
/// 一样：多字节字符的每个字节都不小于 0x80，既落不进 token，也不是反斜杠和引号。
const fn is_tok(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.'
}

/// [`is_tok`] 查表：每个请求体的每个字节都要问一次
const TOK: [bool; 256] = {
    let mut t = [false; 256];
    let mut b = 0;
    while b < 256 {
        t[b] = is_tok(b as u8);
        b += 1;
    }
    t
};

/// 一把密钥最短有多长：前缀规则里最短的那一种（前缀加上至少要有的尾巴），和 OpenAI 老式
/// 密钥的下限，取小的。比它短的 token 哪一种都不是 —— 一段正文里绝大多数 token 在这里就
/// 回去了
const KEY_MIN: usize = {
    let mut min = OPENAI_MIN;
    let mut i = 0;
    while i < BUILTINS.len() {
        match BUILTINS[i].matcher {
            Matcher::Prefix { prefix, min_tail } => {
                if prefix.len() + min_tail < min {
                    min = prefix.len() + min_tail;
                }
            }
            Matcher::Prefixes { list } => {
                let mut j = 0;
                while j < list.len() {
                    if list[j].prefix.len() + list[j].min_tail < min {
                        min = list[j].prefix.len() + list[j].min_tail;
                    }
                    j += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    min
};

/// 一把密钥能以哪些字节开头：每条前缀规则（单前缀、多前缀）的头一个字节，和 OpenAI 老式
/// 密钥的 `s`
const KEY_FIRST: [bool; 256] = {
    let mut t = [false; 256];
    t[b's' as usize] = true;
    let mut i = 0;
    while i < BUILTINS.len() {
        match BUILTINS[i].matcher {
            Matcher::Prefix { prefix, .. } => t[prefix.as_bytes()[0] as usize] = true,
            Matcher::Prefixes { list } => {
                let mut j = 0;
                while j < list.len() {
                    t[list[j].prefix.as_bytes()[0] as usize] = true;
                    j += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    t
};

/// 前缀表里的一项：前缀、尾巴至少几个、尾巴的字符、哪条规则
type Entry3 = (&'static str, usize, Tail, &'static str);

/// 前缀表，按头一个字节分好，组内照 [`BUILTINS`] 里的顺序。**一个 token 只和同一个头字节的
/// 那几项比**：前缀规则有一百来项，过了快速排除的 token 挨个比一遍，在几 MB 的请求体上就是
/// 好几毫秒
fn prefix_table() -> &'static [Vec<Entry3>] {
    static T: std::sync::OnceLock<Vec<Vec<Entry3>>> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let mut t: Vec<Vec<Entry3>> = vec![Vec::new(); 256];
        for b in BUILTINS {
            match b.matcher {
                Matcher::Prefix { prefix, min_tail } => {
                    t[usize::from(prefix.as_bytes()[0])].push((prefix, min_tail, Tail::Any, b.id));
                }
                Matcher::Prefixes { list } => {
                    for p in list {
                        t[usize::from(p.prefix.as_bytes()[0])]
                            .push((p.prefix, p.min_tail, p.tail, b.id));
                    }
                }
                _ => {}
            }
        }
        t
    })
}

/// 前缀后面那段的字符对得上 `tail`。token 里本来只有 token 字符，`Any` 不用再看
fn tail_fits(tail: Tail, t: &str) -> bool {
    match tail {
        Tail::Any => true,
        Tail::Hex => t.bytes().all(|c| c.is_ascii_hexdigit()),
        Tail::Alnum => t.bytes().all(|c| c.is_ascii_alphanumeric()),
    }
}

/// 一段 token 是哪种 API 密钥。**只看开着的那几条。**`openai`：OpenAI 老式密钥那条开着
/// （扫之前问一次，不在每个 token 上查一遍表）。
///
/// 太短的、开头哪条前缀都对不上的直接回去：[`classify_prefixed`] 认得出的 token 都过得了
/// 这两关（[`KEY_MIN`]、[`KEY_FIRST`]）。
pub(super) fn classify_token(tok: &str, set: &RuleSet, openai: bool) -> Option<&'static str> {
    if tok.len() < KEY_MIN || !KEY_FIRST[usize::from(tok.as_bytes()[0])] {
        return None;
    }
    classify_prefixed(tok, set, openai)
}

/// [`classify_token`] 的判据本身。
fn classify_prefixed(tok: &str, set: &RuleSet, openai: bool) -> Option<&'static str> {
    let row = &prefix_table()[usize::from(tok.as_bytes()[0])];
    for &(prefix, min_tail, tail, id) in row {
        if let Some(t) = tok.strip_prefix(prefix)
            && t.len() >= min_tail
            && tail_fits(tail, t)
        {
            // 前缀认出来了但这条关着 —— **不再往下猜**。`sk-ant-…` 关掉之后
            // 不该被当成一把 OpenAI 老式 key 换掉
            return set.is_on(id).then_some(id);
        }
    }
    if openai
        && let Some(tail) = tok.strip_prefix("sk-")
        && tok.len() >= OPENAI_MIN
        // 别家的 `sk-` 前缀（`sk-ant-`、`sk-or-v1-`、`sk-lf-` ……）不归 OpenAI：尾巴不够长、
        // 字符不对的也不归 —— 报错了服务商，「凭据发往陌生主机」就会把发往它自家接口的
        // 请求当成外传
        && !row
            .iter()
            .any(|(prefix, ..)| prefix.starts_with("sk-") && tok.starts_with(prefix))
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

/// 前缀规则（单前缀、多前缀）和 OpenAI 老式密钥的锚点：前缀本身，整个 token 至少是「前缀 +
/// 至少要有的尾巴」那么长。**从规则表上的 [`Matcher`] 算出来**，不另写一份
pub(super) fn key_anchors() -> Vec<Anchor> {
    let mut v = vec![Anchor::Prefix("sk-", OPENAI_MIN)];
    for b in BUILTINS {
        match b.matcher {
            Matcher::Prefix { prefix, min_tail } => {
                v.push(Anchor::Prefix(prefix, prefix.len() + min_tail));
            }
            Matcher::Prefixes { list } => {
                v.extend(
                    list.iter()
                        .map(|p| Anchor::Prefix(p.prefix, p.prefix.len() + p.min_tail)),
                );
            }
            _ => {}
        }
    }
    v
}

/// 前缀规则或 OpenAI 老式密钥开着一条以上
pub(super) fn any_key_on(set: &RuleSet) -> bool {
    set.is_on("openai-api-key")
        || BUILTINS.iter().any(|b| {
            matches!(b.matcher, Matcher::Prefix { .. } | Matcher::Prefixes { .. })
                && set.is_on(b.id)
        })
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
///
/// 回调的第三个参数说这个 token 在不在一个 JSON 字符串里面（数没转义的引号；
/// 文本是 JSON 时才有意义）。
///
/// **按字节走，不按字符解码**：token、反斜杠、引号都是 ASCII（见 [`is_tok`]），多字节字符
/// 的字节在这里只是分隔。反斜杠后面跟着的是一个多字节字符时，跳过的只是它的头一个字节，
/// 剩下的几个照样只是分隔 —— 切出来的和按字符切的一样（测试拿两种切法对过）。token 的两头
/// 都落在 ASCII 字节上，切片一定在字符边界上。
pub(super) fn for_each_token(text: &str, mut f: impl FnMut(&str, Range<usize>, bool)) {
    let b = text.as_bytes();
    let mut start: Option<usize> = None;
    let mut quoted = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if TOK[usize::from(c)] {
            start.get_or_insert(i);
            i += 1;
            continue;
        }
        if let Some(s) = start.take() {
            f(&text[s..i], s..i, quoted);
        }
        i += 1;
        match c {
            // 反斜杠后面那个字符是转义的一部分；`\u` 再带四位十六进制
            b'\\' => {
                if b.get(i) == Some(&b'u') {
                    i += 1;
                    let mut n = 0;
                    while n < 4 && b.get(i).is_some_and(u8::is_ascii_hexdigit) {
                        i += 1;
                        n += 1;
                    }
                } else if i < b.len() {
                    i += 1;
                }
            }
            b'"' => quoted = !quoted,
            _ => {}
        }
    }
    if let Some(s) = start {
        f(&text[s..], s..text.len(), quoted);
    }
}

// ---------------------------------------------------------------- 私钥块

/// 从 `at` 起、到所在 JSON 字符串结尾（没转义的引号）之前。没有引号就到文本末尾
fn string_end(text: &str, at: usize) -> usize {
    let b = text.as_bytes();
    let mut i = at;
    while i < b.len() && b[i] != b'"' {
        i += if b[i] == b'\\' { 2 } else { 1 };
    }
    i.min(b.len())
}

const PEM_BEGIN: &str = "-----BEGIN ";
const PEM_KEY: &str = "PRIVATE KEY-----";

/// `-----BEGIN ... PRIVATE KEY-----` 到对应的 END。
///
/// **整段换掉，不是只换 base64。**只换中间那段的话，模型看到的是一个
/// 空壳 PEM，它会以为文件坏了然后建议你重新生成一把 —— 那是在帮倒忙。
fn private_keys(text: &str, json: bool, out: &mut Vec<Hit>) {
    let mut from = 0;
    // 按字节找（SIMD）：整个请求体都要过一遍，而它几乎从来不在里面
    while let Some(b) = memchr::memmem::find(&text.as_bytes()[from..], PEM_BEGIN.as_bytes()) {
        let begin = from + b;
        // **头和 END 都在同一个字符串里找。**按整个请求体往后找的话，一条消息里一个半截的
        // `-----BEGIN`（读到一半的文件、讲这条规则的代码）会一路接到后面另一条消息里的
        // `END`：中间几条历史整段换成一个占位符，前后两条拼成一个对象，发出去的已经不是
        // 原来那段对话
        let text = if json {
            &text[..string_end(text, begin)]
        } else {
            text
        };
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

/// JWT 的锚点：12 个字符以上、base64url 的字符开头、正好两个点（三段）
pub(super) const JWT_ANCHORS: Anchors = Anchors {
    near: None,
    chars: Chars {
        need: C_DOT,
        forbid: 0,
    },
    list: &[Anchor::First(ALNUM.or(Bytes::of(b"-_")), 12, ANY_LEN)],
    fine: Some(jwt_shaped),
};

/// 正好两个点、三段都不空、头一段至少 8 个字符：token 里只有 base64url 的字符和点，
/// [`looks_like_jwt`] 在解码之前看的就是这些。源码里 `self.config.name`、`a.b.c.d` 这类
/// 带点的标识符成千上万，只数点的话个个都要再进 [`looks_like_jwt`] 切一遍
fn jwt_shaped(t: &[u8]) -> bool {
    let mut dots = [0usize; 2];
    let mut n = 0;
    for (i, &c) in t.iter().enumerate() {
        if c == b'.' {
            if n == 2 {
                return false;
            }
            dots[n] = i;
            n += 1;
        }
    }
    n == 2 && dots[0] >= 8 && dots[1] > dots[0] + 1 && dots[1] + 1 < t.len()
}

fn is_b64url(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '=')
}

/// 三段式 base64url，**且首段解出来含 `"alg"`**。
///
/// 那个解码是必需的：光看「三段用点分开的 base64」会把版本号、文件路径、
/// 甚至 `a.b.c` 这种普通标识符全算上。
pub(super) fn looks_like_jwt(tok: &str) -> bool {
    // 首段至少 8 个字符，后两段各至少一个，加两个点：短于 12 的不可能是
    if tok.len() < 12 {
        return false;
    }
    // 先数点，不把三段收集起来：每个 token 都要过这一关，绝大多数在这里就回去了
    if tok.bytes().filter(|&b| b == b'.').count() != 2 || !tok.split('.').all(is_b64url) {
        return false;
    }
    let Some((head, _)) = tok.split_once('.') else {
        return false;
    };
    if head.len() < 8 {
        return false;
    }
    use base64::Engine as _;
    let Ok(head) =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(head.trim_end_matches('='))
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
    while let Some(i) = memchr::memmem::find(&bytes[from..], b"://") {
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

/// 内网 IPv4 地址的锚点：数字开头、至少 7 个字符、正好三个点
pub(super) const IP_ANCHORS: Anchors = Anchors {
    near: None,
    chars: Chars {
        need: C_DOT,
        forbid: C_LETTER | C_DASH | C_UNDER,
    },
    list: &[Anchor::First(DIGIT, 7, ANY_LEN)],
    fine: Some(|t| t.iter().filter(|&&c| c == b'.').count() == 3),
};

/// 内部域名的锚点：以 [`INTERNAL_SUFFIXES`] 之一的最后一个字母结尾（不分大小写），8 个字符以上
pub(super) const DOMAIN_ANCHORS: Anchors = Anchors {
    near: None,
    chars: Chars {
        need: C_DOT,
        forbid: 0,
    },
    list: &[Anchor::Last(Bytes::of(b"lLnNpPaAtT"), 8)],
    fine: None,
};

fn is_rfc1918(tok: &str) -> bool {
    // 四段读进一个定长数组，不收集：开着这条时每个 token 都要过一遍
    let mut nums = [0u16; 4];
    let mut parts = tok.split('.');
    for n in &mut nums {
        match parts.next().map(str::parse::<u16>) {
            Some(Ok(v)) if v <= 255 => *n = v,
            _ => return false,
        }
    }
    if parts.next().is_some() {
        return false;
    }
    match (nums[0], nums[1]) {
        (10, _) => true,
        (172, b) if (16..=31).contains(&b) => true,
        (192, 168) => true,
        // 运营商级 NAT：Tailscale 等组网工具给每台机器发的就是这一段
        (100, b) if (64..=127).contains(&b) => true,
        _ => false,
    }
}

/// 内网地址和内部域名。
///
/// **不含回环。**`127.0.0.1` 和 `localhost` 是这台机器自己 —— 而用户问
/// 的很可能正是「我本地这个服务为什么连不上」，把它换成占位符等于把问题
/// 本身藏起来了。何况我们的网关自己就住在那儿。
///
/// `ip`、`domain`：那两条开没开（扫之前问一次，不在每个 token 上查一遍表）。
pub(super) fn internal_token(tok: &str, ip: bool, domain: bool) -> Option<&'static str> {
    if ip && is_rfc1918(tok) {
        return Some("internal-ip");
    }
    // 后缀不分大小写地比，**不先转成小写**：每个 token 都要过这一关，转一次就是
    // 一次分配
    let t = tok.as_bytes();
    if domain
        && t.len() > 7
        && t.contains(&b'.')
        // 后缀前面得有一段主机名：光秃秃的 `.internal`、`.home.arpa` 是在讲这个后缀
        && INTERNAL_SUFFIXES.iter().any(|s| {
            t.len() > s.len() && t[t.len() - s.len()..].eq_ignore_ascii_case(s.as_bytes())
        })
    {
        return Some("internal-domain");
    }
    None
}

// ---------------------------------------------------------------- 个人号码

/// 省级行政区划代码（GB/T 2260），身份证号码的头两位。`83` 是台湾居民居住证（港澳的居住证
/// 用 `81`、`82`）。
const PROVINCES: [u8; 35] = [
    11, 12, 13, 14, 15, // 华北
    21, 22, 23, // 东北
    31, 32, 33, 34, 35, 36, 37, // 华东
    41, 42, 43, 44, 45, 46, // 中南
    50, 51, 52, 53, 54, // 西南
    61, 62, 63, 64, 65, // 西北
    71, 81, 82, 83, // 台湾、香港、澳门、台湾居民居住证
];

/// [`PROVINCES`] 里大陆的那几个（前 31 个）：新版外国人永久居留身份证第 2–3 位的受理地
const MAINLAND: usize = 31;

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
    // 新版外国人永久居留身份证（2023 起）：首位 `9`，第 2–3 位是受理地的省级代码，第 4–6 位是
    // 持证人国籍的 ISO 3166-1 数字代码。其余结构和校验与身份证相同
    let region = if s[0] == b'9' {
        PROVINCES[..MAINLAND].contains(&(num(1..3) as u8))
            && super::personal::nationality(num(3..6))
    } else {
        PROVINCES.contains(&(num(0..2) as u8))
    };
    if !region {
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
pub(super) fn days_in(year: u32, month: u32) -> u32 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// 今天，写成 `YYYYMMDD`。**按北京时间**：身份证上的出生日期是按它记的。
pub(super) fn today_ymd() -> u32 {
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
pub(super) fn luhn(d: &[u8]) -> bool {
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
pub(super) struct Spaced {
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
    pub(super) fn feed(
        &mut self,
        text: &str,
        at: usize,
        body: &str,
        dotted: bool,
        out: &mut Vec<Hit>,
    ) {
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
    pub(super) fn finish(&mut self, text: &str, out: &mut Vec<Hit>) {
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

/// 身份证号、卡号的锚点：数字开头（连写的号码；分组写的卡号的每一组）
pub(super) const CARD_ANCHORS: Anchors = Anchors {
    near: None,
    chars: super::anchor::ANY_CHARS,
    list: &[Anchor::First(DIGIT, 1, ANY_LEN)],
    fine: None,
};

/// 一个 token 里的手机号的锚点：11 个字符以上、其中至少 11 位数字
pub(super) const PHONE_ANCHORS: Anchors = Anchors {
    near: None,
    chars: Chars {
        need: C_DIGIT,
        forbid: 0,
    },
    list: &[Anchor::First(super::anchor::TOKEN, 11, ANY_LEN)],
    fine: Some(|t| t.iter().filter(|c| c.is_ascii_digit()).count() >= 11),
};

/// 一个 token 是不是身份证号，或者连着写、用连字符分组写的卡号。`body` 是
/// 去掉了句末句号的那一段，从 `at` 开始。
pub(super) fn personal_token(
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

/// 一个 token 里的中国大陆手机号：正好 11 位、`1` 开头、第二位是 `3`–`9` 的一串数字。
///
/// **前后不紧挨别的数字**：看的是一整串连着的数字，从更长的数字串里截一段去对的话，
/// 订单号、时间戳里每隔几位就「有」一个手机号。字母挨着没关系（`tel13800138000`）。
pub(super) fn mobile_phones(tok: &str, at: usize, out: &mut Vec<Hit>) {
    let b = tok.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i - start == 11 && b[start] == b'1' && (b'3'..=b'9').contains(&b[start + 1]) {
            out.push(builtin_hit("cn-mobile-phone", at + start..at + i));
        }
    }
}

/// 邮箱本地部分里能有的字符
fn is_local(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'%' | b'+' | b'-')
}

/// 邮箱地址：从每个 `@` 往两边找。
///
/// 往左是本地部分，往右是域名，**都止于 JSON 的转义**（请求体里的换行是 `\n` 两个
/// 字符：行首的地址不能和那个 `n` 粘在一起）。不算的：
///
/// - URL 里的用户名：`@` 前面那段紧跟在 `/` 或 `:` 后面（`https://user@host`、
///   `postgres://user:pass@host` —— 口令那一段归连接串那条规则）；
/// - 域名不像域名的：只有一段（`user@localhost`）、最后一段不是两个以上的字母
///   （`react@18.2.0`）；
/// - 高分屏图片的文件名：`icon@2x.png`。
fn emails(text: &str, out: &mut Vec<Hit>) {
    let b = text.as_bytes();
    for (at, _) in text.match_indices('@') {
        // 往左：本地部分
        let mut start = at;
        while start > 0 && is_local(b[start - 1]) {
            start -= 1;
        }
        // 前面是奇数个反斜杠：头一个字符是转义的一部分（`\n`、`\u00e9`）
        let mut slashes = 0;
        while start > slashes && b[start - 1 - slashes] == b'\\' {
            slashes += 1;
        }
        if slashes % 2 == 1 {
            let unicode = b[start] == b'u'
                && b.get(start + 1..start + 5)
                    .is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit));
            start = (start + if unicode { 5 } else { 1 }).min(at);
        } else if start > 0 && matches!(b[start - 1], b'/' | b':') {
            continue;
        }
        let local = &text[start..at];
        if local.is_empty()
            || local.len() > 64
            || local.starts_with('.')
            || local.ends_with('.')
            || local.contains("..")
        {
            continue;
        }
        // 往右：域名。句末的句号不是域名的一部分
        let mut end = at + 1;
        while end < b.len() && (b[end].is_ascii_alphanumeric() || matches!(b[end], b'.' | b'-')) {
            end += 1;
        }
        let domain = text[at + 1..end].trim_end_matches('.');
        let labels: Vec<&str> = domain.split('.').collect();
        let tld = labels.last().copied().unwrap_or_default();
        let retina = labels[0].len() >= 2
            && labels[0].ends_with('x')
            && labels[0][..labels[0].len() - 1]
                .bytes()
                .all(|c| c.is_ascii_digit());
        if labels.len() < 2
            || retina
            || !(2..=24).contains(&tld.len())
            || !tld.bytes().all(|c| c.is_ascii_alphabetic())
            || labels
                .iter()
                .any(|l| l.is_empty() || l.starts_with('-') || l.ends_with('-'))
        {
            continue;
        }
        out.push(builtin_hit("email", start..at + 1 + domain.len()));
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

/// `text` 是不是一整段合规的 JSON（请求体、存下来的回答）。**看开头不够**：解码过的正文也
/// 可能以引号、括号开头（`"quoted" then MYCORP-222222`、`[555-1234]`），当成 JSON 的话只在
/// 引号里面找，引号外面的值就漏了。开头像的才真的解一遍（不建树）；一个光秃秃的数也是 JSON，
/// 可那是一段话（「620102198811110554」）
pub(crate) fn is_json(text: &str) -> bool {
    text.trim_start().starts_with(['{', '[', '"'])
        && serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok()
}

/// 出站的请求体是不是 JSON：和 [`is_json`] 一样，只是字符串里原样写着的控制字符（换行、
/// 制表符）不挑 —— 有的客户端这么发，上游也收；把它当成一段话的话，`"seed":42` 里的数值会被
/// 换成占位符，请求就坏了。落盘打码用严格的 [`is_json`]：那边认不准时宁可多换
pub(crate) fn json_like(text: &str) -> bool {
    if is_json(text) {
        return true;
    }
    if !text.trim_start().starts_with(['{', '[', '"']) || !text.bytes().any(|b| b < 0x20) {
        return false;
    }
    // 控制字符换成空格再解一遍：字符串里外都还是合规的样子，结构不变
    let spaced: String = text
        .chars()
        .map(|c| if (c as u32) < 0x20 { ' ' } else { c })
        .collect();
    serde_json::from_str::<serde::de::IgnoredAny>(&spaced).is_ok()
}

/// 一段 JSON 原文里每个字符串引号里面的那一段，按先后。
///
/// 按 SIMD 跳着找：字符串外面只找引号，里面找引号和反斜杠（反斜杠连同它转义的那个字符跳过）。
/// 一段请求体里字符串之间、字符串里面的大段文字一个字节都不用逐个看
pub(crate) fn json_string_spans(text: &str) -> Vec<Range<usize>> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(q) = memchr::memchr(b'"', &b[i.min(b.len())..]) {
        let start = i + q + 1;
        let mut j = start;
        loop {
            match memchr::memchr2(b'"', b'\\', &b[j.min(b.len())..]) {
                Some(k) if b[j + k] == b'\\' => j += k + 2,
                Some(k) => {
                    j += k;
                    break;
                }
                None => {
                    j = b.len();
                    break;
                }
            }
        }
        let end = j.min(b.len());
        out.push(start..end);
        i = end + 1;
        if i > b.len() {
            break;
        }
    }
    out
}

/// 自定义规则的预筛。**每条自定义规则在每个正文字符串上各跑一遍**的话，条数一多就和内置规则
/// 一样线性变慢。所以：
///
/// - 按原样找的（「包含」）编进**一个** Aho-Corasick 自动机，整段文本过一遍，所有规则一起找。
///   不分大小写的照不分大小写编 —— 只收纯 ASCII、且不含 `k`、`s` 的：正则的不分大小写按
///   Unicode 折叠，`k` 还认开尔文符号 `K`、`s` 还认长 s `ſ`，自动机的 ASCII 不分大小写认不出
///   它们（测试钉着只有这两个字母这样）。收不进的和正则一起走下面那条路；
/// - 正则合成一个 [`regex::RegexSet`]，每个正文字符串先过一遍集合，得知哪几条在这一段里有
///   命中，只对这几条求位置。正则引擎对固定文字本来就有预筛，没有固定文字的（`\d{6}`）耗时
///   和逐条跑一样。合不成集合（太大、写法放进分组里不成立）就逐条跑。
///
/// 写法、匹配语义（逐个正文字符串、`^` `$`、大小写）不变：命中的还是同一条正则在同一段上
/// `find_iter` 的结果，按规则的顺序记下来
#[derive(Debug)]
struct CustomIndex {
    /// 自动机，和它的第几个字面量是第几条规则、分不分大小写
    contains: Option<(aho_corasick::AhoCorasick, Vec<(usize, bool)>)>,
    /// 正则集合，和它的第几个是第几条规则
    set: Option<(regex::RegexSet, Vec<usize>)>,
    /// 逐条跑的
    each: Vec<usize>,
}

/// 按原样找、能并进自动机的那几条
fn lit_rules_of(custom: &[Custom]) -> impl Iterator<Item = usize> + '_ {
    custom.iter().enumerate().filter_map(|(i, c)| {
        let p = &c.pattern;
        (!p.regex && (p.case_sensitive || ascii_fold_safe(&p.text))).then_some(i)
    })
}

/// 一条正则放进 [`regex::RegexSet`] 不吃亏：没有 Unicode 的词边界（`\b`）。集合要一次答出
/// 每一条有没有命中，只能整段跑一遍；带 Unicode 词边界的正则遇到非 ASCII 的正文时快的那种
/// 引擎跑不了、退到慢的那种，整个集合跟着慢上百倍。单独跑的时候它有固定文字可以预筛（`\bEMP`），
/// 慢的引擎只在候选附近跑，所以这种留着逐条跑
fn set_friendly(p: &Pattern) -> bool {
    let src = if p.regex {
        p.text.clone()
    } else {
        regex::escape(&p.text)
    };
    regex_syntax::ParserBuilder::new()
        .case_insensitive(!p.case_sensitive)
        .build()
        .parse(&src)
        .is_ok_and(|h| !h.properties().look_set().contains_word_unicode())
}

/// 一段按原样找、不分大小写的文字，按 ASCII 不分大小写找和按正则找结果一样
fn ascii_fold_safe(text: &str) -> bool {
    text.is_ascii() && !text.bytes().any(|c| matches!(c, b'k' | b'K' | b's' | b'S'))
}

impl CustomIndex {
    fn new(custom: &[Custom]) -> Self {
        let mut lits: Vec<&str> = Vec::new();
        let mut lit_rules = Vec::new();
        let mut rest = Vec::new();
        let mut each = Vec::new();
        for (i, c) in custom.iter().enumerate() {
            let p = &c.pattern;
            if !p.regex && (p.case_sensitive || ascii_fold_safe(&p.text)) {
                lits.push(&p.text);
                lit_rules.push((i, p.case_sensitive));
            } else if set_friendly(p) {
                rest.push(i);
            } else {
                each.push(i);
            }
        }
        let contains = if lits.is_empty() {
            None
        } else {
            aho_corasick::AhoCorasick::builder()
                .ascii_case_insensitive(lit_rules.iter().any(|&(_, cs)| !cs))
                .build(&lits)
                .ok()
                .map(|ac| (ac, lit_rules))
        };
        // 自动机编不成（不该发生）就把它的那几条也逐条跑
        if contains.is_none() {
            each.extend(lit_rules_of(custom));
        }
        let set = (rest.len() > 1)
            .then(|| {
                let sources = rest.iter().map(|&i| {
                    let p = &custom[i].pattern;
                    let src = if p.regex {
                        p.text.clone()
                    } else {
                        regex::escape(&p.text)
                    };
                    if p.case_sensitive {
                        src
                    } else {
                        format!("(?i:{src})")
                    }
                });
                // 每一条单独编都在上限以内（[`compile`]）；合起来的上限按条数放宽
                let limit = (1usize << 20) * rest.len();
                regex::RegexSetBuilder::new(sources)
                    .size_limit(limit)
                    .dfa_size_limit(limit)
                    .build()
                    .ok()
            })
            .flatten();
        match set {
            Some(set) => CustomIndex {
                contains,
                set: Some((set, rest)),
                each,
            },
            None => {
                each.extend(rest);
                each.sort_unstable();
                CustomIndex {
                    contains,
                    set: None,
                    each,
                }
            }
        }
    }
}

fn custom_hits(
    text: &str,
    set: &RuleSet,
    json: bool,
    spans: Option<Vec<Range<usize>>>,
    out: &mut Vec<Hit>,
) {
    if set.custom.is_empty() {
        return;
    }
    let index = set
        .index
        .get_or_init(|| Arc::new(CustomIndex::new(&set.custom)));
    // 是 JSON 的话（请求体、编成 JSON 字符串的一段正文）**一个字符串一个字符串地找**：
    // 用户的正则（`\S+`）在整段原文上会从 `{`、`:` 这些结构上的字开始，一路吃过引号 ——
    // 换掉结构上的字，请求体就不是 JSON 了；而它吃掉的那一段里真正该换的字也就没换
    let spans = match spans {
        Some(s) if json => s,
        _ if json => json_string_spans(text),
        _ => std::iter::once(0..text.len()).collect(),
    };
    let mut found: Vec<Vec<Range<usize>>> = vec![Vec::new(); set.custom.len()];
    if let Some((ac, rules)) = &index.contains {
        let b = text.as_bytes();
        // 每条规则上一处命中的终点：和正则一样，一处命中之后从它的终点往后接着找
        let mut next = vec![0usize; set.custom.len()];
        for m in ac.find_overlapping_iter(text) {
            let (r, cs) = rules[m.pattern().as_usize()];
            if (cs && &b[m.range()] != set.custom[r].pattern.text.as_bytes()) || m.start() < next[r]
            {
                continue;
            }
            // 整个落在一个正文字符串里
            let k = spans.partition_point(|s| s.end < m.end());
            if spans.get(k).is_none_or(|s| s.start > m.start()) {
                continue;
            }
            next[r] = m.end();
            found[r].push(m.range());
        }
    }
    let each = |r: usize, span: &Range<usize>, piece: &str, found: &mut Vec<Vec<Range<usize>>>| {
        for m in set.custom[r].re.find_iter(piece) {
            found[r].push(span.start + m.start()..span.start + m.end());
        }
    };
    for span in &spans {
        let Some(piece) = text.get(span.clone()) else {
            continue;
        };
        if let Some((rs, rules)) = &index.set {
            for k in rs.matches(piece).iter() {
                each(rules[k], span, piece, &mut found);
            }
        }
        for &r in &index.each {
            each(r, span, piece, &mut found);
        }
    }
    for (c, ms) in set.custom.iter().zip(found) {
        for m in ms {
            if let Some(bytes) = json_safe(text, m) {
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
/// 偏移全乱（重叠的怎么合见 [`resolve`]）。
pub fn scan(text: &str, set: &RuleSet) -> Vec<Hit> {
    resolve(scan_unresolved(text, set), set)
}

/// [`scan`] 还没合并重叠之前的样子：每条规则各自的命中，可能重叠、没排序。要先去掉一些
/// 命中再合并的调用方（[`crate::redact::flow::hits`]：压在占位符上的、载荷里的）用它 ——
/// 先合并的话，一处要换的值会跟着一处该去掉的命中一起被去掉
pub(crate) fn scan_unresolved(text: &str, set: &RuleSet) -> Vec<Hit> {
    let mut out = Vec::new();
    if set.is_empty() {
        return out;
    }
    // **个人号码只在 JSON 的字符串里找**（见 `engine`），私钥块不跨出它所在的那个字符串。不是
    // JSON 的文本（直接拿来扫的一段话）不分里外
    let json = json_like(text);
    // 保存的凭据最先找：同一处另一条规则也认得出时（一把保存的 `sk-ant-…`、连接串里的代理
    // 口令），报成哪一项保存的凭据说得最清楚（重叠的合成一处时，最先记下的说了算）
    if let Some(saved) = &set.saved
        && set.is_on("thinkwatch-saved-secret")
    {
        saved.find(text, &mut out);
    }
    // 不看 token 边界的几条：锚点（`-----BEGIN `、`://`、`@`）用 SIMD 找，找到再往两边看
    if set.is_on("private-key") {
        private_keys(text, json, &mut out);
    }
    if set.is_on("conn-string-password") {
        conn_strings(text, &mut out);
    }
    if set.is_on("email") {
        emails(text, &mut out);
    }
    // 按 token 认的：先预筛、再校验。切 token 时顺手记下了 JSON 字符串的区间的话，自定义规则
    // 接着用
    let spans = super::engine::scan_tokens(text, set, json, &mut out);
    custom_hits(text, set, json, spans, &mut out);
    out
}

/// 排好序、互不重叠的命中（见 [`united`]）。**要换成占位符的命中说了算**：
///
/// - 换成占位符的规则之间重叠的，合成一处、换成一个占位符 —— 只留其中一处的话，另一处
///   多出来的那一截（`aaa<<TW_SECRET_1>>ccc` 的 `ccc`）就原样发出去了；
/// - 只记录的规则（[`RuleSet::records`]）的命中让开它们：和要换的重叠的那一截不算它的，
///   剩下的几截照样是它的命中 —— 存下来、写进日志之前照样打码。反过来的话，一条只记录的
///   规则会把一处本该换掉的值整个盖住，那个值就原样发出去了。
///
/// 报出去的记录不按截开的几截算，见 [`crate::redact::flow::reported`]。
pub(crate) fn resolve(out: Vec<Hit>, set: &RuleSet) -> Vec<Hit> {
    if set.record.is_empty() {
        return united(out);
    }
    let (rec, rep): (Vec<Hit>, Vec<Hit>) = out.into_iter().partition(|h| set.records(&h.rule));
    let rep = united(rep);
    let mut all = Vec::with_capacity(rep.len());
    for h in united(rec) {
        all.extend(outside_of(&rep, h));
    }
    all.extend(rep);
    all.sort_by_key(|h| h.bytes.start);
    all
}

/// 排好序、重叠的合成一处：起点是最早的、终点是最远的，规则和标签照最先（同一个起点上最长）
/// 的那一处。私钥块里的 base64 会被 token 扫描当成别的东西，两段嵌在一起替换会把偏移彻底
/// 搞乱；合成一处之后整块换成一个占位符。只是挨着的不合
fn united(mut out: Vec<Hit>) -> Vec<Hit> {
    out.sort_by_key(|h| (h.bytes.start, std::cmp::Reverse(h.bytes.end)));
    let mut kept: Vec<Hit> = Vec::with_capacity(out.len());
    for h in out {
        match kept.last_mut() {
            Some(p) if p.bytes.end > h.bytes.start => p.bytes.end = p.bytes.end.max(h.bytes.end),
            _ => kept.push(h),
        }
    }
    kept
}

/// `h` 去掉和 `hits`（按起点排好、互不重叠）重叠的那几段之后剩下的几截，按先后
fn outside_of(hits: &[Hit], h: Hit) -> Vec<Hit> {
    let mut out = Vec::new();
    let mut at = h.bytes.start;
    let first = hits.partition_point(|x| x.bytes.end <= h.bytes.start);
    for x in hits[first..]
        .iter()
        .take_while(|x| x.bytes.start < h.bytes.end)
    {
        if x.bytes.start > at {
            out.push(Hit {
                bytes: at..x.bytes.start,
                ..h.clone()
            });
        }
        at = at.max(x.bytes.end);
    }
    if at < h.bytes.end {
        out.push(Hit {
            bytes: at..h.bytes.end,
            ..h
        });
    }
    out
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
    plain_with(text, |encoded| scan(encoded, set))
}

/// 把一段纯文本编成 JSON 字符串交给 `find` 去找，找到的区间换算回原文。跨过转义、
/// 落在转义序列中间的不要（见 [`scan_plain`]）。
pub(crate) fn plain_with(text: &str, find: impl FnOnce(&str) -> Vec<Hit>) -> Vec<Hit> {
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
    find(&encoded)
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
/// 截成 `password=` 只会让真值漏出去。它换下来的值是正文里的样子，可能带引号和反斜杠：
/// 记账用 [`crate::redact::replace::apply_text`]（不再解一次转义），还原时
/// [`crate::redact::replace::restore`] 放回 JSON 原文会转义一次。
pub fn scan_text(text: &str, set: &RuleSet) -> Vec<Hit> {
    let builtins = RuleSet {
        on: set.on.clone(),
        custom: Vec::new(),
        record: set.record.clone(),
        saved: set.saved.clone(),
        index: OnceLock::new(),
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
    resolve(out, set)
}

/// 报出去的样子。**一律打码** —— 「发现了 sk-ant-xxx」这句话本身就是一次
/// 泄漏，它会进日志、进界面、被复制到 issue 里。
///
/// 内网地址和内部域名例外：它们不是凭据，而打码之后的 `…` 让人无从判断
/// 那条记录说的是哪台机器。
///
/// 身份证号、卡号、手机号只留最后四位：「留头 5」留下的正好是身份证号的地区码、
/// 卡号的发卡行，而认出是哪一个号码，看最后四位就够了。邮箱留第一个字和域名。
pub fn masked(rule: &Rule, value: &str) -> String {
    // 住址留到区县、姓名留第一个字、坐标留两位小数……（见 `personal::masked`）
    if let Rule::Builtin(id) = rule
        && let Some(m) = super::personal::masked(id, value)
    {
        return m;
    }
    if matches!(rule, Rule::Builtin(id) if builtin(id).is_some_and(|b| b.matcher == Matcher::Email))
    {
        return mask_email(value);
    }
    match rule.kind() {
        Kind::Internal => value.to_string(),
        k if k.is_personal() => last_four(value),
        // 保存的凭据一个字都不留：报的是它是哪一项（[`Finding::source`]）
        Kind::Saved => String::new(),
        // 按位置认的多半是口令，没有前缀：留头五个就是留了大半个口令。只留最后四位，短的全打掉
        Kind::InContext => {
            let n = value.chars().count();
            if n >= 16 {
                let tail: String = value.chars().skip(n - 4).collect();
                format!("…{tail}")
            } else {
                "…".to_string()
            }
        }
        // 助记词的头几个字母就是头一个词
        Kind::Crypto if rule.id() == "crypto-mnemonic" => "…".to_string(),
        _ => mask(value),
    }
}

/// 邮箱只留本地部分的第一个字和域名：`j…@example.com`。认出是哪一个地址，看这些
/// 就够了；本地部分只有一个字的，那个字也不留
fn mask_email(s: &str) -> String {
    let Some((local, domain)) = s.rsplit_once('@') else {
        return mask(s);
    };
    let mut chars = local.chars();
    match (chars.next(), chars.next()) {
        (Some(first), Some(_)) => format!("{first}…@{domain}"),
        _ => format!("…@{domain}"),
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
    /// 是哪一项保存的凭据（规则 `thinkwatch-saved-secret`）；别的规则是 `None`
    pub source: Option<SavedSource>,
}

/// 把命中合并成「哪条规则 × 哪个值 × 几次」。**同一个值只报一次**：
/// 一把 key 在一个请求里出现三次，是一把 key，不是三把。
///
/// 按第一次出现的先后排，**一个不同的值一条、一条不少**：`len()` 就是这段文本里有几个
/// 不同的值。逐条报出去的调用方自己定报几条（桌面网关一个请求报前 100 个，见
/// `tw_gateway::guard::items`）；按规则聚合的（企业版的审计）拿得到全部。
///
/// **合并按「规则 × 值」查表**，不在已经合过的里面一条条找：一个请求里贴进来几万个不同
/// 的值时（一份导出的凭据清单），后者是平方级的，几十万个值要几分钟。
pub fn findings(text: &str, hits: &[Hit]) -> Vec<Finding> {
    findings_with(text, hits, None)
}

/// 同 [`findings`]，保存的凭据的命中带上是哪一项（[`Finding::source`]）。**按「规则 × 值」合并**，
/// 所以两项不同的保存的凭据是两条，即使它们打码之后一样（都是空串）
pub fn findings_in(text: &str, hits: &[Hit], set: &RuleSet) -> Vec<Finding> {
    findings_with(text, hits, Some(set))
}

fn findings_with(text: &str, hits: &[Hit], set: Option<&RuleSet>) -> Vec<Finding> {
    let mut out: Vec<(&Rule, &str, u64)> = Vec::new();
    let mut seen: HashMap<(&Rule, &str), usize> = HashMap::new();
    for h in hits {
        let value = &text[h.bytes.clone()];
        match seen.entry((&h.rule, value)) {
            Entry::Occupied(at) => out[*at.get()].2 += 1,
            Entry::Vacant(slot) => {
                slot.insert(out.len());
                out.push((&h.rule, value, 1));
            }
        }
    }
    out.into_iter()
        .map(|(rule, value, count)| Finding {
            masked: masked(rule, value),
            source: match (rule, set) {
                (Rule::Builtin("thinkwatch-saved-secret"), Some(set)) => {
                    set.saved_source(value).cloned()
                }
                _ => None,
            },
            rule: rule.clone(),
            count,
        })
        .collect()
}

#[cfg(test)]
mod oracle;

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

    /// 解码过的正文以引号、括号开头也不是 JSON：自定义规则照样在整段上找，不只在引号里
    #[test]
    fn decoded_text_that_starts_like_json_is_still_searched_whole() {
        let rules = RuleSet::none()
            .with_custom("corp", r"MYCORP-\d{6}")
            .unwrap();
        for t in [
            "said MYCORP-111111 and \"x\" then MYCORP-222222",
            "\"quoted\" then MYCORP-222222",
            "{ see } MYCORP-222222",
            "[1] MYCORP-222222",
        ] {
            let got: Vec<&str> = scan(t, &rules)
                .iter()
                .map(|h| &t[h.bytes.clone()])
                .collect();
            assert!(got.contains(&"MYCORP-222222"), "{t:?} → {got:?}");
        }
        // 一整段 JSON 还是只在字符串里找（结构上的字不换）
        let body = r#"{"a":"MYCORP-333333","n":1}"#;
        assert_eq!(scan(body, &rules).len(), 1);
        assert!(is_json(body) && is_json("[1, \"x\"]") && is_json("\"s\""));
        assert!(!is_json("\"quoted\" then") && !is_json("{ see }") && !is_json("[1] x"));
        assert!(!is_json("620102198811110554"), "光秃秃的一个数是一段话");
        assert!(
            !is_json("[555-1234]") && !is_json("{\"p\":555-1234}"),
            "要合规"
        );
    }

    /// 出站的请求体字符串里原样写着换行（不合规、上游也收）：照样当 JSON 读，字符串外面的
    /// 数值不换成占位符
    #[test]
    fn a_request_with_raw_newlines_in_strings_is_still_read_as_json() {
        let rules = RuleSet::none().with_custom("ids", r"\d{6,}").unwrap();
        let body = "{\"seed\":1234567,\"messages\":[{\"content\":\"line one\nmy id 7654321\"}]}";
        let got: Vec<&str> = crate::redact::flow::hits(body, &rules)
            .iter()
            .map(|h| &body[h.bytes.clone()])
            .collect();
        assert_eq!(got, ["7654321"]);
        assert!(json_like(body) && !is_json(body));
        assert!(!json_like("[555-1234]") && !json_like("\"quoted\" then\n x"));
    }

    #[test]
    fn a_pem_does_not_reach_into_the_next_string() {
        // 一条工具输出里只有半截的头（读的是讲这条规则的代码），END 在后面另一条消息里：
        // 按整个请求体往后找的话，中间几条历史会整段换成一个占位符
        let body = serde_json::json!({"input": [
            {"type": "function_call_output", "call_id": "c1", "output": "const PEM_BEGIN: &str = \"-----BEGIN \";\nconst PEM_KEY: &str = \"PRIVATE KEY-----\";"},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "好"}]},
            {"type": "function_call_output", "call_id": "c2", "output": "-----END RSA PRIVATE KEY-----"},
        ]})
        .to_string();
        assert!(found(&body).is_empty(), "{:?}", found(&body));
        // 头在一个字符串里、END 在下一个字符串里也不算
        let body = r#"{"a":"-----BEGIN RSA PRIVATE KEY-----\nMIIEow","b":"-----END RSA PRIVATE KEY-----"}"#;
        assert!(found(body).is_empty(), "{:?}", found(body));
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
        // 172.32.0.1 是公网地址：所有规则都开着时归「公网 IP」那条
        let internal = RuleSet::only(&["internal-ip"]);
        assert!(scan("127.0.0.1 和 8.8.8.8 和 172.32.0.1", &internal).is_empty());
    }

    #[test]
    fn internal_domains_are_found_whatever_their_case() {
        let set = RuleSet::only(&["internal-domain"]);
        let got: Vec<String> = scan(
            "构建机 build.corp.internal、打印机 Printer.LOCAL、路由 gw.home.lan",
            &set,
        )
        .into_iter()
        .map(|h| h.rule.id().to_string())
        .collect();
        assert_eq!(got, vec!["internal-domain"; 3]);
        // 太短的、只是后缀的、后缀不在词尾的，都不算
        for t in [
            "a.lan",
            ".local",
            "local",
            "x.internal.example.com",
            "localhost",
        ] {
            assert!(scan(t, &set).is_empty(), "{t}");
        }
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

    /// 查表合并和原来在合过的里面一条条找，合出来的一模一样：同样的条目、同样的先后、
    /// 同样的次数。同一个值被两条规则认出来的，是两条
    #[test]
    fn findings_merge_exactly_as_the_one_by_one_search_did() {
        fn one_by_one(text: &str, hits: &[Hit]) -> Vec<Finding> {
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
                    source: None,
                    masked: masked(&rule, value),
                    rule,
                    count,
                })
                .collect()
        }
        let pool = [
            "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA",
            "ghp_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            "AKIAQRSTUVWXYZ234567",
            "10.0.0.1",
            "10.0.0.2",
            "db.internal",
            "postgres://app:hunter2@db/x",
            "PRJ-12",
            "PRJ-7",
        ];
        let rules = all().with_custom("project", r"PRJ-\d+").unwrap();
        // 一个确定的伪随机序列：值有重复、先后打乱
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut text = String::new();
        for _ in 0..400 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            text.push_str(pool[(seed % pool.len() as u64) as usize]);
            text.push_str(if seed % 3 == 0 { "，" } else { " " });
        }
        let mut hits = scan(&text, &rules);
        // 同一个值换一条规则认：按「规则 × 值」合，两条
        hits.extend(
            scan(&text, &RuleSet::only(&["aws-access-key-id"]))
                .into_iter()
                .map(|h| Hit {
                    rule: Rule::Custom(Arc::from("aws-again")),
                    ..h
                }),
        );
        let got = findings(&text, &hits);
        assert_eq!(got, one_by_one(&text, &hits));
        assert!(got.len() >= pool.len(), "{got:?}");
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
                Matcher::Prefixes { list } => {
                    for p in list {
                        let key = format!("{}{}", p.prefix, &"a1".repeat(p.min_tail)[..p.min_tail]);
                        let hits = scan(&key, &only);
                        assert_eq!(hits.len(), 1, "{} 认不出它自己说的形状：{key}", b.id);
                        let short = &key[..key.len() - 1];
                        assert!(scan(short, &only).is_empty(), "{} 认了更短的 {short}", b.id);
                    }
                }
                Matcher::Saved => {
                    let value = "a1b2c3d4e5f6g7h8";
                    let set = only.clone().with_saved(vec![SavedSecret {
                        value: value.to_string(),
                        source: SavedSource {
                            kind: SavedSourceKind::UpstreamKey,
                            name: "x".into(),
                        },
                        hosts: Vec::new(),
                    }]);
                    assert_eq!(scan(value, &set).len(), 1);
                    assert!(scan(&value[1..], &set).is_empty());
                }
                Matcher::Assignment { suffixes } => {
                    for s in suffixes {
                        let t = format!("MY_{s}=Abc123def456");
                        let hits = scan(&t, &only);
                        assert_eq!(hits.len(), 1, "{t}");
                        assert_eq!(&t[hits[0].bytes.clone()], "Abc123def456");
                    }
                }
                // 代码实现的凭据规则：说明由界面按 id 给，判据由样本钉着（个人信息那一路有自己的）
                Matcher::Code if !b.kind.is_personal() => {
                    let s = super::super::secrets::tests::samples::sample(b.id)
                        .unwrap_or_else(|| panic!("{} 没有样本", b.id));
                    let (text, value) = &s.hits[0];
                    let hits = scan_plain(text, &only);
                    assert_eq!(hits.len(), 1, "{} 认不出它的样本 {text}", b.id);
                    assert_eq!(&text[hits[0].bytes.clone()], value);
                }
                Matcher::Labeled { names } => {
                    // 每个字段名都认（裸的 `name`、`address` 要有旁证），值是占位写法的不认
                    for n in names {
                        let value = match b.id {
                            "labeled-name" => "张三",
                            "labeled-address" => "北京市朝阳区建国路88号",
                            "labeled-birth-date" => "1990-01-01",
                            "labeled-account" => "6217000012345678",
                            _ => "高血压",
                        };
                        let text = format!(r#"{{"{n}": "{value}", "email": "z@example.com"}}"#);
                        let got: Vec<String> = scan_plain(&text, &only)
                            .into_iter()
                            .map(|h| text[h.bytes].to_string())
                            .collect();
                        assert_eq!(got, [value], "{} 不认字段名 {n}", b.id);
                        let text = format!(r#"{{"{n}": "${{VALUE}}", "email": "z@example.com"}}"#);
                        assert!(scan_plain(&text, &only).is_empty(), "{} 认了占位写法", b.id);
                    }
                }
                Matcher::Code if b.kind.is_personal() => {
                    let text = super::super::personal::tests::sample(b.id)
                        .unwrap_or_else(|| panic!("{} 没有样本", b.id));
                    let hits = scan_plain(&text, &only);
                    assert!(!hits.is_empty(), "{} 认不出自己的样本：{text}", b.id);
                    assert!(hits.iter().all(|h| h.rule.id() == b.id));
                }
                Matcher::DomainSuffix { suffixes } => {
                    for s in suffixes {
                        let host = format!("server01{s}");
                        assert_eq!(scan(&host, &only).len(), 1, "{host}");
                    }
                }
                Matcher::PrivateIp => {
                    for ip in [
                        "10.1.2.3",
                        "172.16.0.1",
                        "192.168.1.1",
                        "100.64.0.1",
                        "fd00::1",
                    ] {
                        assert_eq!(scan_plain(ip, &only).len(), 1, "{ip}");
                    }
                    for ip in ["127.0.0.1", "100.128.0.1", "fe80::1"] {
                        assert!(scan_plain(ip, &only).is_empty(), "{ip}");
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
            assert_eq!(r.text, "身份证号：<<TW_ID_NUMBER_1>>，请核对", "{id}");
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
            10, 16, 20, 30, 38, 40, 47, 55, 60, 66, 70, 72, 80, 84, 90, 91, 99,
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
            assert_eq!(r.text, "卡号 <<TW_CARD_NUMBER_1>> 尾号对一下", "{card}");
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
    fn a_number_outside_the_strings_of_a_body_is_a_json_number_and_left_alone() {
        // 工具调用的参数、`seed` 是 JSON 的数值：换成占位符，整个请求体就不是 JSON 了
        let body = r#"{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"get_order","input":{"order_id":6222021234567894,"ids":[4532015112830366, 6222021234567894]}}]},{"role":"user","content":"他说\"卡号是 6222021234567894\"，身份证 11010519491231002X"}],"seed":4532015112830366}"#;
        let got = personal_found(body);
        assert_eq!(
            got,
            vec![
                ("bank-card".to_string(), "6222021234567894".to_string()),
                (
                    "cn-resident-id".to_string(),
                    "11010519491231002X".to_string()
                ),
            ]
        );
        let r = crate::redact::replace::redact(
            body,
            &RuleSet::defaults(),
            crate::redact::replace::Ledger::new(crate::redact::replace::Scheme::SECRET),
        );
        let v: serde_json::Value = serde_json::from_str(&r.text).expect("换完不是合法 JSON");
        assert_eq!(v["seed"], 4532015112830366u64);
        assert_eq!(
            v["messages"][0]["content"][0]["input"]["order_id"],
            6222021234567894u64
        );
        assert_eq!(
            v["messages"][1]["content"],
            "他说\"卡号是 <<TW_CARD_NUMBER_1>>\"，身份证 <<TW_ID_NUMBER_1>>"
        );
        assert_eq!(crate::redact::replace::restore(&r.text, &r.ledger), body);
        // 不是 JSON 的一段话不分里外
        assert_eq!(personal_found("order_id: 6222021234567894").len(), 1);
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
        assert!(f[0].rule.kind().is_personal());
    }

    #[test]
    fn personal_placeholders_say_what_they_were_and_count_per_label() {
        // 模型看到 `<<TW_ID_NUMBER_1>>` 才知道那里是个身份证号；编号按标签各数各的，
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
            "key <<TW_SECRET_1>>\n身份证 <<TW_ID_NUMBER_1>>\n卡 <<TW_CARD_NUMBER_1>>\n另一张 <<TW_CARD_NUMBER_2>>\n身份证 <<TW_ID_NUMBER_1>>"
        );
        assert_eq!(crate::redact::replace::restore(&r.text, &r.ledger), body);
        // 流式还原认得带标签的占位符，切成一个字符一段也拼得回来
        let mut s = crate::redact::stream::Restorer::new(&r.ledger);
        let echoed = "你的卡号 <<TW_CARD_NUMBER_1>> 和身份证 <<TW_ID_NUMBER_1>>。";
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

    // ------------------------------------------------------------ 邮箱、手机号

    fn only(id: &'static str, text: &str) -> Vec<String> {
        scan(text, &RuleSet::only(&[id]))
            .into_iter()
            .map(|h| text[h.bytes].to_string())
            .collect()
    }

    #[test]
    fn email_and_mobile_numbers_ship_switched_off() {
        // 它们没有能核对的结构，按「长得像」认，在代码和文档里误报得多
        let t = "联系 zhang.san@example.com 或 13800138000";
        assert!(scan(t, &RuleSet::defaults()).is_empty());
        for id in ["email", "cn-mobile-phone"] {
            let b = builtin(id).unwrap();
            assert!(!b.on_by_default && b.kind.is_personal(), "{id}");
        }
        assert_eq!(builtin("email").unwrap().label, Some("TW_EMAIL"));
        assert_eq!(builtin("cn-mobile-phone").unwrap().label, Some("TW_PHONE"));
    }

    #[test]
    fn a_mobile_number_is_eleven_digits_on_their_own() {
        assert_eq!(
            only(
                "cn-mobile-phone",
                "电话13800138000，或者 tel:19912345678、+86 15012345678"
            ),
            ["13800138000", "19912345678", "15012345678"]
        );
        // 前后紧挨着数字的是更长的号码的一部分；第二位不是 3–9 的不是手机号
        for t in [
            "订单 138001380001",
            "时间戳 1713800138000",
            "12800138000",
            "10800138000",
            "1380013800",
        ] {
            assert!(only("cn-mobile-phone", t).is_empty(), "{t}");
        }
        // 字母挨着没关系
        assert_eq!(only("cn-mobile-phone", "id13800138000x"), ["13800138000"]);
        // JSON 的数值不是：换成占位符，请求体就不是 JSON 了
        let body = r#"{"n":13800138000,"s":"13800138000"}"#;
        let got = scan(body, &RuleSet::only(&["cn-mobile-phone"]));
        assert_eq!(got.len(), 1);
        assert_eq!(&body[got[0].bytes.clone()], "13800138000");
        assert!(
            got[0].bytes.start > body.find(':').unwrap() + 12,
            "数值那个不算"
        );
    }

    #[test]
    fn an_email_address_is_found_and_urls_and_file_names_are_not() {
        assert_eq!(
            only(
                "email",
                "写信给 zhang.san+ai@mail.example.com.cn。抄送 a_b@x.io, git@github.com"
            ),
            [
                "zhang.san+ai@mail.example.com.cn",
                "a_b@x.io",
                "git@github.com"
            ]
        );
        for t in [
            "https://user@example.com/x",
            "postgres://u:pw@db.example.com/app",
            "icon@2x.png",
            "npm i react@18.2.0",
            "user@localhost",
            "@scope/package",
            "a@b.c",
            ".a@b.com",
            "a..b@c.com",
        ] {
            assert!(only("email", t).is_empty(), "{t}: {:?}", only("email", t));
        }
    }

    #[test]
    fn an_email_after_a_json_escape_does_not_take_the_escape_with_it() {
        // 请求体里的换行是 `\n` 两个字符，中文可能是 `\uXXXX`
        for (t, want) in [
            (r#"{"c":"to:\nzhang@example.com"}"#, "zhang@example.com"),
            (r#"{"c":"\u90aezhang@example.com"}"#, "zhang@example.com"),
            (r#"{"c":"C:\\nzhang@example.com"}"#, "nzhang@example.com"),
        ] {
            assert_eq!(only("email", t), [want], "{t}");
        }
    }

    #[test]
    fn personal_values_are_reported_with_the_least_that_says_which_one() {
        let email = Rule::Builtin("email");
        assert_eq!(masked(&email, "zhang.san@example.com"), "z…@example.com");
        assert_eq!(masked(&email, "z@example.com"), "…@example.com");
        assert_eq!(
            masked(&Rule::Builtin("cn-mobile-phone"), "13800138000"),
            "…8000"
        );
    }

    #[test]
    fn email_and_phone_placeholders_say_what_they_were() {
        let set = RuleSet::only(&["email", "cn-mobile-phone"]);
        let body = serde_json::json!({
            "content": "张三 zhang@example.com 13800138000，李四 li@example.com"
        })
        .to_string();
        let r = crate::redact::replace::redact(
            &body,
            &set,
            crate::redact::replace::Ledger::new(crate::redact::replace::Scheme::SECRET),
        );
        let v: serde_json::Value = serde_json::from_str(&r.text).unwrap();
        assert_eq!(
            v["content"],
            "张三 <<TW_EMAIL_1>> <<TW_PHONE_1>>，李四 <<TW_EMAIL_2>>"
        );
        assert_eq!(crate::redact::replace::restore(&r.text, &r.ledger), body);
    }

    /// 以前的切法：按字符解码着走。按字节走的 [`for_each_token`] 切出来的要和它一模一样
    fn tokens_by_char(text: &str) -> Vec<(String, Range<usize>, bool)> {
        let mut out = Vec::new();
        let mut f = |t: &str, r: Range<usize>, q: bool| out.push((t.to_string(), r, q));
        let mut start: Option<usize> = None;
        let mut quoted = false;
        let mut chars = text.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            if c == '\\' {
                if let Some(s) = start.take() {
                    f(&text[s..i], s..i, quoted);
                }
                if let Some((_, 'u')) = chars.next() {
                    for _ in 0..4 {
                        if chars.next_if(|(_, h)| h.is_ascii_hexdigit()).is_none() {
                            break;
                        }
                    }
                }
                continue;
            }
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                start.get_or_insert(i);
                continue;
            }
            if let Some(s) = start.take() {
                f(&text[s..i], s..i, quoted);
            }
            if c == '"' {
                quoted = !quoted;
            }
        }
        if let Some(s) = start {
            f(&text[s..], s..text.len(), quoted);
        }
        out
    }

    fn tokens_by_byte(text: &str) -> Vec<(String, Range<usize>, bool)> {
        let mut out = Vec::new();
        for_each_token(text, |t, r, q| out.push((t.to_string(), r, q)));
        out
    }

    /// 随机拼出来的一段：ASCII 的字母数字和标点、引号、反斜杠和各种转义（含不完整的
    /// `\u`、反斜杠后面跟着中文和 emoji、结尾一个落单的反斜杠）、中文、emoji、组合字符
    fn random_text(next: &mut impl FnMut(usize) -> usize) -> String {
        const PIECES: &[&str] = &[
            "a",
            "Z",
            "9",
            "-",
            "_",
            ".",
            " ",
            ",",
            ":",
            "{",
            "}",
            "[",
            "]",
            "\"",
            "\\",
            "\\\\",
            "\\n",
            "\\t",
            "\\\"",
            "\\u",
            "\\u00e9",
            "\\u5bc6",
            "\\uD83D\\uDE00",
            "\\u12",
            "\\uzz",
            "\\é",
            "\\密",
            "\\😀",
            "u",
            "00e9",
            "sk-ant-",
            "AKIA",
            "eyJ",
            "密钥",
            "：",
            "😀",
            "👩‍💻",
            "e\u{301}",
            "\u{200B}",
            "\u{7f}",
            "\t",
            "\n",
            "é",
            "→",
            "ghp_",
            "@",
            "/",
            "+",
            "=",
        ];
        let n = next(40);
        (0..n).map(|_| PIECES[next(PIECES.len())]).collect()
    }

    #[test]
    fn cutting_by_byte_cuts_exactly_where_cutting_by_char_did() {
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for _ in 0..20_000 {
            let t = random_text(&mut next);
            assert_eq!(tokens_by_byte(&t), tokens_by_char(&t), "{t:?}");
        }
        for t in [
            "",
            "\\",
            "a\\",
            "\\u",
            "\\u0",
            "\\😀x",
            "\"a\\\"b\"c",
            "密\\钥sk",
            "x\u{1F600}",
        ] {
            assert_eq!(tokens_by_byte(t), tokens_by_char(t), "{t:?}");
        }
    }

    /// 太短的、开头哪条前缀都对不上的 token 直接回去：哪一条规则开着、关着，结论都和逐条
    /// 比前缀的一样
    #[test]
    fn the_quick_rejections_never_drop_a_key() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        const TAIL: &[u8] = b"abcXYZ0189-_.";
        let mut heads: Vec<&str> = BUILTINS
            .iter()
            .flat_map(|b| match b.matcher {
                Matcher::Prefix { prefix, .. } => vec![prefix],
                Matcher::Prefixes { list } => list.iter().map(|p| p.prefix).collect(),
                _ => Vec::new(),
            })
            .collect();
        heads.extend(["sk-", "s", "x", "", "Sk-", "ak"]);
        let sets = [
            all(),
            RuleSet::defaults(),
            RuleSet::none(),
            RuleSet::only(&["openai-api-key"]),
            RuleSet::only(&["anthropic-api-key", "aws-access-key-id"]),
        ];
        for _ in 0..20_000 {
            let mut tok = heads[next(heads.len())].to_string();
            for _ in 0..next(50) {
                tok.push(TAIL[next(TAIL.len())] as char);
            }
            if tok.is_empty() {
                continue;
            }
            for set in &sets {
                for openai in [true, false] {
                    assert_eq!(
                        classify_token(&tok, set, openai),
                        classify_prefixed(&tok, set, openai),
                        "{tok}"
                    );
                }
            }
        }
        // 按头字节分好的前缀表和逐条比前缀的结论一样
        for b in BUILTINS {
            let list: Vec<(&str, usize)> = match b.matcher {
                Matcher::Prefix { prefix, min_tail } => vec![(prefix, min_tail)],
                Matcher::Prefixes { list } => list.iter().map(|p| (p.prefix, p.min_tail)).collect(),
                _ => continue,
            };
            for (prefix, min_tail) in list {
                let tok = format!("{prefix}{}", &"a1".repeat(min_tail)[..min_tail]);
                assert_eq!(classify_prefixed(&tok, &all(), false), Some(b.id), "{tok}");
            }
        }
        // 最短的那种刚好够长时认得出
        assert_eq!(KEY_MIN, "AKIA".len() + 12);
        assert_eq!(
            classify_token("AKIAABCDEFGHIJKL", &all(), true),
            Some("aws-access-key-id")
        );
    }
}
