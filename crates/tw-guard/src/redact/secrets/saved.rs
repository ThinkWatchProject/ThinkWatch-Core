//! 用户在 ThinkWatch 里保存的凭据，**按值认，与格式无关**。
//!
//! 很多上游的密钥没有前缀（32 位十六进制、UUID），前缀规则认不出；而它们就写在配置里，
//! 是最确定的凭据。调用方（桌面版从 `config.yaml`，企业版从上游配置）把值交进来
//! （[`super::super::rules::RuleSet::with_saved`]），配置一换跟着换。
//!
//! - **整值、分大小写、在 token 边界上**：`abc…` 是一把密钥，`xabc…` 里的那一段不是；
//! - 请求体是 JSON，值里有引号、反斜杠的话写出来是转义过的样子，两种都认；
//! - **不足 12 个字符的不收**：短值（一个代理口令 `1234`）在正文里到处都是，按值找只会吵；
//! - 报出去不带值的任何部分（打码是空串），只说是哪一项（[`SavedSource`]）。

use std::collections::HashMap;

use aho_corasick::{AhoCorasick, MatchKind};
use serde::{Deserialize, Serialize};

use super::super::rules::{Hit, builtin_hit};

/// 保存的凭据是哪一种。界面按它说「上游「DeepSeek」的密钥」「代理「公司」的口令」
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "kebab-case")]
pub enum SavedSourceKind {
    /// 上游的密钥（含 OAuth 令牌、AWS 访问密钥）
    UpstreamKey,
    /// 上游自定义请求头里的密钥
    UpstreamHeader,
    /// 本机网关的访问密钥
    AccessKey,
    /// 出站代理的口令
    ProxyPassword,
    /// 控制面密钥
    ControlKey,
    /// 同步与备份的凭据（同步口令、WebDAV 口令、S3 Secret）
    SyncCredential,
}

impl SavedSourceKind {
    /// 全部取值，按声明的顺序
    pub const ALL: &'static [Self] = &[
        Self::UpstreamKey,
        Self::UpstreamHeader,
        Self::AccessKey,
        Self::ProxyPassword,
        Self::ControlKey,
        Self::SyncCredential,
    ];

    /// 线上的那个词
    pub fn slug(self) -> &'static str {
        match self {
            Self::UpstreamKey => "upstream-key",
            Self::UpstreamHeader => "upstream-header",
            Self::AccessKey => "access-key",
            Self::ProxyPassword => "proxy-password",
            Self::ControlKey => "control-key",
            Self::SyncCredential => "sync-credential",
        }
    }

    pub fn from_slug(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.slug() == s)
    }
}

/// 一个保存的凭据是哪一项。`name` 是上游名、访问密钥名、代理名；控制面密钥是空的
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SavedSource {
    pub kind: SavedSourceKind,
    pub name: String,
}

/// 交给脱敏引擎的一个保存的凭据。
#[derive(Clone)]
pub struct SavedSecret {
    /// 真值（`${VAR}` 已经展开）
    pub value: String,
    pub source: SavedSource,
    /// 它正当地会发往哪些主机（后缀匹配，含子域）：上游的 `base_url` 的主机。危险命令
    /// 「凭据发往陌生主机」按它判断；空的是哪儿都不该去（访问密钥、控制面密钥）—— 发往本机
    /// 以外的任何地方都算外传
    pub hosts: Vec<String>,
}

/// **Debug 不打印值**：规则集会进日志和 panic 信息
impl std::fmt::Debug for SavedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SavedSecret")
            .field("source", &self.source)
            .field("hosts", &self.hosts)
            .finish_non_exhaustive()
    }
}

/// 编好的一组保存的凭据：一个多模式查找，加上「值 → 是哪一项」。
#[derive(Clone)]
pub struct Saved {
    secrets: Vec<SavedSecret>,
    /// 每项两种写法：原样的，和 JSON 字符串里转义过的
    ac: AhoCorasick,
    /// 两种写法 → 第几项
    by_value: HashMap<String, usize>,
}

impl std::fmt::Debug for Saved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Saved")
            .field("secrets", &self.secrets)
            .finish_non_exhaustive()
    }
}

/// 不足这么多个字符的值不收
pub const SAVED_MIN_CHARS: usize = 12;

impl Saved {
    /// 编一组。不足 [`SAVED_MIN_CHARS`] 个字符的不收，同一个值只留第一项；一项都不剩是 `None`
    pub fn new(list: Vec<SavedSecret>) -> Option<Self> {
        let mut secrets: Vec<SavedSecret> = Vec::new();
        let mut by_value: HashMap<String, usize> = HashMap::new();
        let mut patterns: Vec<String> = Vec::new();
        for s in list {
            if s.value.chars().count() < SAVED_MIN_CHARS || by_value.contains_key(&s.value) {
                continue;
            }
            let k = secrets.len();
            let quoted = serde_json::to_string(&s.value).unwrap_or_default();
            let escaped = quoted
                .get(1..quoted.len().saturating_sub(1))
                .unwrap_or_default()
                .to_string();
            for form in [s.value.clone(), escaped] {
                if let std::collections::hash_map::Entry::Vacant(v) = by_value.entry(form.clone()) {
                    v.insert(k);
                    patterns.push(form);
                }
            }
            secrets.push(s);
        }
        if secrets.is_empty() {
            return None;
        }
        let ac = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .build(&patterns)
            .ok()?;
        Some(Self {
            secrets,
            ac,
            by_value,
        })
    }

    /// 有几项
    pub fn len(&self) -> usize {
        self.secrets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// 在 `text` 里找每一项，两头紧挨着的不是字母数字。
    ///
    /// **不按 token 边界**：`.`、`-`、`_` 在 token 里，但挨着一个保存的值时它们只是标点和
    /// 分隔 —— 句末的句号（`My key is <值>.`）、`MISTRAL_KEY_<值>`、`<值>_old`。按 token 边界的话
    /// 这几处整值就在那里却认不出，原样发出去。值是整个对上的，不会是别的东西的一截；只有
    /// 两头粘着字母数字的（`x<值>`）才可能是另一个更长的串。前面是转义的尾巴（`\n<值>`）也算断开
    pub(crate) fn find(&self, text: &str, out: &mut Vec<Hit>) {
        let b = text.as_bytes();
        for m in self.ac.find_iter(text) {
            let (start, end) = (m.start(), m.end());
            let head = start == 0
                || !b[start - 1].is_ascii_alphanumeric()
                || super::after_escape(b, start);
            let tail = b.get(end).is_none_or(|c| !c.is_ascii_alphanumeric());
            if head && tail {
                out.push(builtin_hit("thinkwatch-saved-secret", m.range()));
            }
        }
    }

    /// 每一项的真值，按收下的先后。规则集的指纹（[`super::super::rules::RuleSet::fingerprint`]）
    /// 和扫描缓存用：换了一项保存的凭据，以前扫的结果就不作数了
    pub(crate) fn values(&self) -> impl Iterator<Item = &str> {
        self.secrets.iter().map(|s| s.value.as_str())
    }

    fn item(&self, value: &str) -> Option<&SavedSecret> {
        self.by_value.get(value).map(|&k| &self.secrets[k])
    }

    /// 命中的这个值（原样或转义过的）是哪一项
    pub fn source_of(&self, value: &str) -> Option<&SavedSource> {
        self.item(value).map(|s| &s.source)
    }

    /// 命中的这个值正当地会发往哪些主机（见 [`SavedSecret::hosts`]）
    pub fn hosts_of(&self, value: &str) -> Option<&[String]> {
        self.item(value).map(|s| s.hosts.as_slice())
    }
}
