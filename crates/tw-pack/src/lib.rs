//! 配置包（`.twconfig`）：导出、分享、本机备份与同步版本共用的一种文件。
//!
//! # 文件里有什么
//!
//! 未加密的配置包是一个 ZIP，里面只许有这几样：
//!
//! ```text
//! manifest.json               格式版本与内容说明（[`Manifest`]）
//! config.yaml                 配置
//! plugins/<id>.js             插件脚本
//! plugins/.approved/<id>.js   批准过的那一份（底稿）
//! client-prefs.json           Lite 的通用偏好，Core 不解析
//! ```
//!
//! 加密的配置包是 age 格式：口令加密（scrypt），或者加密给一把 X25519 公钥（同步位置的
//! 版本密钥）。解开之后是同样的 ZIP。两者按文件头区分（[`is_encrypted`]），扩展名相同。
//!
//! # 读的时候不信任文件
//!
//! 配置包会从别人那里来（分享、导入），所以读的时候按不可信的输入对待：整个文件和解压之后
//! 的总大小都不超过 [`MAX_BYTES`]，ZIP 里出现上面以外的路径（绝对路径、`..`、别的文件名）
//! 整包拒绝，插件 id 必须合规矩，同一个路径出现两次也拒绝。
//!
//! # 同样的内容，同样的字节
//!
//! 条目按固定的顺序写、时间戳固定，所以内容相同的两个包只差 `created_at_ms`。比较两个包是
//! 不是同一份配置用 [`Pack::content_id`]：它只看可以带走的内容（本机设置不算），按
//! [`model`] 规范化之后再算，注释、排版、默认值写没写都不影响它。

use std::collections::BTreeMap;
use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use tw_types::{Msg, msg};

pub mod backup;
pub mod disk;
pub mod export;
pub mod import;
pub mod model;
pub mod restore;

/// 配置包格式的版本。**读到更大的就拒绝**（[`PackError::TooNew`]）：新版本的格式旧版本
/// 猜不准，猜错了会悄悄丢东西
pub const FORMAT: u32 = 1;

/// 一个配置包（连同解压之后的内容）最大多少字节
pub const MAX_BYTES: usize = 16 * 1024 * 1024;

/// 口令最少几个字符
pub const MIN_PASSPHRASE: usize = 8;

/// 扩展名，不带点
pub const EXTENSION: &str = "twconfig";

/// 写进 manifest 的 Core 版本：这几个 crate 的版本，和 `Status.version` 的 CalVer 不是一回事
pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// ZIP 里的固定路径
const MANIFEST: &str = "manifest.json";
const CONFIG: &str = "config.yaml";
const PREFS: &str = "client-prefs.json";
const PLUGINS: &str = "plugins/";
const APPROVED: &str = "plugins/.approved/";

/// ZIP 里最多多少个条目：清单、配置、偏好，加上每个插件两份。再多只会是乱塞的
const MAX_ENTRIES: usize = 2048;

/// 一个配置包，解开之后的样子。
#[derive(Debug, Clone, PartialEq)]
pub struct Pack {
    pub manifest: Manifest,
    pub config_yaml: String,
    /// 插件 id → 文件
    pub plugins: BTreeMap<String, PluginFiles>,
    /// Lite 的通用偏好。**不解析**：Lite 定义它，Core 只是存着、带着走
    pub client_prefs: Option<serde_json::Value>,
}

/// 一个插件的两份文件。
#[derive(Clone, PartialEq, Eq)]
pub struct PluginFiles {
    pub script: Vec<u8>,
    /// 批准过的那一份。和脚本一样（或者没有）就是没有待批准的改动
    pub approved: Option<Vec<u8>>,
}

/// 脚本可能有几百 KB，Debug 里只说多长
impl std::fmt::Debug for PluginFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginFiles")
            .field("script", &format_args!("{} bytes", self.script.len()))
            .field(
                "approved",
                &self.approved.as_ref().map(|a| format!("{} bytes", a.len())),
            )
            .finish()
    }
}

/// `manifest.json`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub kind: PackKind,
    pub created_at_ms: u64,
    pub core_version: String,
    /// 配置文件格式的版本（`tw_config::SCHEMA_VERSION`）
    pub schema: u32,
    /// 带没带密钥。不带时上游密钥、请求头的值、AWS 凭据、代理密码、网关密钥都是空的
    pub secrets: bool,
    /// **读出来时由文件头决定**，文件里写的不算
    #[serde(default)]
    pub encrypted: bool,
    pub origin: PackOrigin,
    /// 写的时候按内容重算，读的时候也重算：文件里写的只是给人看的
    #[serde(default)]
    pub counts: Counts,
    /// 同步用的那几项（序号、设备、上一版本、冲突记录），同步那一路自己定义、自己读
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<serde_json::Value>,
}

impl Manifest {
    /// 一份新的说明：当前的格式和 Core 版本、此刻的时间、带着密钥、不加密
    pub fn new(kind: PackKind, origin: PackOrigin) -> Self {
        Self {
            format: FORMAT,
            kind,
            created_at_ms: now_ms(),
            core_version: CORE_VERSION.to_string(),
            schema: tw_config::SCHEMA_VERSION,
            secrets: true,
            encrypted: false,
            origin,
            counts: Counts::default(),
            sync: None,
        }
    }

    /// 由比这一份 Core 新的版本写的
    pub fn from_newer_core(&self) -> bool {
        newer_than_us(&self.core_version)
    }
}

/// 完整的配置，还是选出来的几项（选择项目导出、分享）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackKind {
    Full,
    Partial,
}

/// 这个包是怎么来的。导出和分享之外，和配置的一次改动的来源是同一套词。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackOrigin {
    Export,
    Share,
    Ui,
    Cli,
    External,
    Defaults,
    Sync,
    Import,
    Restore,
    Repair,
    /// 更新的版本才有的来源。**读得进来**：一个新词不该让整个包读不了
    #[serde(other)]
    Other,
}

impl PackOrigin {
    pub fn slug(&self) -> &'static str {
        match self {
            PackOrigin::Export => "export",
            PackOrigin::Share => "share",
            PackOrigin::Ui => "ui",
            PackOrigin::Cli => "cli",
            PackOrigin::External => "external",
            PackOrigin::Defaults => "defaults",
            PackOrigin::Sync => "sync",
            PackOrigin::Import => "import",
            PackOrigin::Restore => "restore",
            PackOrigin::Repair => "repair",
            PackOrigin::Other => "other",
        }
    }

    pub fn from_slug(s: &str) -> PackOrigin {
        serde_json::from_value(serde_json::Value::String(s.to_string()))
            .unwrap_or(PackOrigin::Other)
    }
}

impl From<tw_config::history::Origin> for PackOrigin {
    fn from(o: tw_config::history::Origin) -> Self {
        use tw_config::history::Origin as O;
        match o {
            O::Ui => PackOrigin::Ui,
            O::Cli => PackOrigin::Cli,
            O::External => PackOrigin::External,
            O::Defaults => PackOrigin::Defaults,
            O::Sync => PackOrigin::Sync,
            O::Import => PackOrigin::Import,
            O::Restore => PackOrigin::Restore,
            O::Repair => PackOrigin::Repair,
            // 轮换不留版本，走到这里也只是一个说不出来源的版本
            O::Rotation => PackOrigin::Other,
        }
    }
}

/// 包里有多少东西。给列表和预览看。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Counts {
    pub providers: u32,
    pub proxies: u32,
    pub clients: u32,
    pub aliases: u32,
    pub groups: u32,
    pub routes: u32,
    pub price_sheets: u32,
    pub security_rules: u32,
    pub plugins: u32,
}

impl Counts {
    /// 按配置原文数。**读不成 YAML 也不报错**：数不出来就是 0
    pub fn of(config_yaml: &str) -> Counts {
        let Ok(doc) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(config_yaml) else {
            return Counts::default();
        };
        let len = |path: &[&str]| -> u32 {
            let mut cur = &doc;
            for k in path {
                match cur.get(*k) {
                    Some(v) => cur = v,
                    None => return 0,
                }
            }
            match cur {
                serde_yaml_ng::Value::Sequence(s) => s.len() as u32,
                serde_yaml_ng::Value::Mapping(m) => m.len() as u32,
                _ => 0,
            }
        };
        Counts {
            providers: len(&["providers"]),
            proxies: len(&["proxies"]),
            clients: len(&["clients"]),
            aliases: len(&["aliases"]),
            groups: len(&["groups"]),
            routes: len(&["routes"]),
            price_sheets: len(&["pricing", "sheets"]),
            security_rules: len(&["security", "redact", "custom"])
                + len(&["security", "inspect_tools", "custom"])
                + len(&["security", "content", "custom"]),
            plugins: len(&["plugins"]),
        }
    }
}

/// 写的时候怎么封。
pub enum Seal<'a> {
    None,
    Passphrase(&'a str),
    Recipient(&'a age::x25519::Recipient),
}

/// 读的时候拿什么解。
pub enum Unseal<'a> {
    None,
    Passphrase(&'a str),
    Identity(&'a age::x25519::Identity),
}

/// 读写配置包时的失败。**英文只写一遍**：`Display` 就是 [`PackError::msg`] 的原句。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PackError {
    /// 口令加密的包，没给口令
    #[error("{}", self.msg())]
    NeedPassphrase,
    #[error("{}", self.msg())]
    WrongPassphrase,
    /// 加密给一把密钥的包（同步版本），没给密钥
    #[error("{}", self.msg())]
    NeedKey,
    #[error("{}", self.msg())]
    WrongKey,
    /// 格式版本比这一份 Core 认得的新
    #[error("{}", self.msg())]
    TooNew { format: u32, core_version: String },
    /// 读不成一个配置包：不是 ZIP、ZIP 坏了、清单读不懂、配置不是 UTF-8……
    #[error("{}", self.msg())]
    Corrupt { detail: String },
    /// ZIP 里有不该有的路径
    #[error("{}", self.msg())]
    BadEntry { name: String },
    /// 少了必须有的那一项（清单、配置）
    #[error("{}", self.msg())]
    MissingEntry { name: String },
    #[error("{}", self.msg())]
    TooLarge { limit: usize },
}

impl PackError {
    pub fn msg(&self) -> Msg {
        match self {
            PackError::NeedPassphrase => msg!(
                "pack.need_passphrase" =>
                "This configuration pack is encrypted with a passphrase. Enter the passphrase to open it."
            ),
            PackError::WrongPassphrase => msg!(
                "pack.wrong_passphrase" =>
                "The passphrase is not correct."
            ),
            PackError::NeedKey => msg!(
                "pack.need_key" =>
                "This configuration pack is encrypted for a sync location and cannot be opened with a passphrase."
            ),
            PackError::WrongKey => msg!(
                "pack.wrong_key" =>
                "This configuration pack is encrypted with a different key."
            ),
            PackError::TooNew {
                format,
                core_version,
            } => msg!(
                "pack.too_new", format = format, version = core_version =>
                "This configuration pack was exported by a newer version (Core {version}, format \
                 {format}). Update ThinkWatch Lite first."
            ),
            PackError::Corrupt { detail } => msg!(
                "pack.corrupt", detail = detail =>
                "This file is not a readable configuration pack: {detail}"
            ),
            PackError::BadEntry { name } => msg!(
                "pack.bad_entry", name = name =>
                "This configuration pack contains `{name}`, which a configuration pack may not \
                 contain, so it was not opened."
            ),
            PackError::MissingEntry { name } => msg!(
                "pack.missing_entry", name = name =>
                "This configuration pack has no {name}."
            ),
            PackError::TooLarge { limit } => msg!(
                "pack.too_large", limit = limit / (1024 * 1024) =>
                "This configuration pack is larger than {limit} MiB."
            ),
        }
    }
}

fn corrupt(detail: impl std::fmt::Display) -> PackError {
    PackError::Corrupt {
        detail: detail.to_string(),
    }
}

// ─────────────────────────────────────────────────────────── 写

/// 把一个包写成文件的字节。`counts` 和 `encrypted` 按内容重算，别的照 `pack.manifest`。
pub fn write(pack: &Pack, seal: Seal) -> Result<Vec<u8>, PackError> {
    let mut manifest = pack.manifest.clone();
    manifest.counts = Counts::of(&pack.config_yaml);
    manifest.encrypted = !matches!(seal, Seal::None);
    let plain = zip_bytes(pack, &manifest)?;
    if plain.len() > MAX_BYTES {
        return Err(PackError::TooLarge { limit: MAX_BYTES });
    }
    let out = match seal {
        Seal::None => plain,
        Seal::Passphrase(p) => {
            let mut r =
                age::scrypt::Recipient::new(age::secrecy::SecretString::from(p.to_string()));
            r.set_work_factor(work_factor());
            encrypt(&plain, &r)?
        }
        Seal::Recipient(r) => encrypt(&plain, r)?,
    };
    if out.len() > MAX_BYTES {
        return Err(PackError::TooLarge { limit: MAX_BYTES });
    }
    Ok(out)
}

fn zip_bytes(pack: &Pack, manifest: &Manifest) -> Result<Vec<u8>, PackError> {
    // **解压之后的总量也不超过上限**：读的时候按它挡，写出去的就得读得回来
    let unpacked = pack.config_yaml.len()
        + pack
            .plugins
            .values()
            .map(|f| f.script.len() + f.approved.as_ref().map_or(0, Vec::len))
            .sum::<usize>()
        + pack
            .client_prefs
            .as_ref()
            .map_or(0, |p| p.to_string().len() * 2);
    if unpacked > MAX_BYTES {
        return Err(PackError::TooLarge { limit: MAX_BYTES });
    }
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    // **时间戳固定**：同样的内容写出同样的字节
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::DEFAULT)
        .unix_permissions(0o600);
    let mut put = |name: &str, bytes: &[u8]| -> Result<(), PackError> {
        z.start_file(name, opts).map_err(corrupt)?;
        z.write_all(bytes).map_err(corrupt)
    };
    let manifest_json = serde_json::to_vec_pretty(manifest).map_err(corrupt)?;
    put(MANIFEST, &manifest_json)?;
    put(CONFIG, pack.config_yaml.as_bytes())?;
    if let Some(p) = &pack.client_prefs {
        put(PREFS, &serde_json::to_vec_pretty(p).map_err(corrupt)?)?;
    }
    for (id, files) in &pack.plugins {
        if !tw_config::plugins::valid_id(id) {
            return Err(PackError::BadEntry {
                name: format!("{PLUGINS}{id}.js"),
            });
        }
        put(&format!("{PLUGINS}{id}.js"), &files.script)?;
        if let Some(a) = &files.approved {
            put(&format!("{APPROVED}{id}.js"), a)?;
        }
    }
    Ok(z.finish().map_err(corrupt)?.into_inner())
}

fn encrypt(plain: &[u8], r: &dyn age::Recipient) -> Result<Vec<u8>, PackError> {
    let e = age::Encryptor::with_recipients(std::iter::once(r)).map_err(corrupt)?;
    let mut out = Vec::with_capacity(plain.len() + 256);
    let mut w = e.wrap_output(&mut out).map_err(corrupt)?;
    w.write_all(plain).map_err(corrupt)?;
    w.finish().map_err(corrupt)?;
    Ok(out)
}

/// scrypt 的工作量（`N = 2^log_n`）。**固定，不按这台机器的快慢挑**：同步位置上的版本由
/// 哪台设备写、哪台设备读都不一定，快的那台挑出来的工作量慢的那台可能解不开。18 是 age
/// 自己的下限，一台普通的电脑一秒以内
fn work_factor() -> u8 {
    match TEST_WORK_FACTOR.load(std::sync::atomic::Ordering::Relaxed) {
        0 => 18,
        n => n,
    }
}

/// 解开时最多接受多大的工作量。比写的时候大一些：将来调高了，旧版本照样读得了新包
const MAX_WORK_FACTOR: u8 = 22;

/// **只给测试用**：把口令加密的工作量调低，测试不用每次等一秒。0 是不改
#[doc(hidden)]
pub static TEST_WORK_FACTOR: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

// ─────────────────────────────────────────────────────────── 读

/// 这些字节是不是加密的配置包（age 的二进制格式）。
pub fn is_encrypted(bytes: &[u8]) -> bool {
    bytes.starts_with(b"age-encryption.org/")
}

/// 读一个配置包。
pub fn read(bytes: &[u8], unseal: Unseal) -> Result<Pack, PackError> {
    if bytes.len() > MAX_BYTES {
        return Err(PackError::TooLarge { limit: MAX_BYTES });
    }
    let encrypted = is_encrypted(bytes);
    let plain;
    let zip_bytes: &[u8] = if encrypted {
        plain = decrypt(bytes, unseal)?;
        &plain
    } else {
        bytes
    };
    let mut pack = unzip(zip_bytes)?;
    pack.manifest.encrypted = encrypted;
    pack.manifest.counts = Counts::of(&pack.config_yaml);
    Ok(pack)
}

fn decrypt(bytes: &[u8], unseal: Unseal) -> Result<Vec<u8>, PackError> {
    let d = age::Decryptor::new_buffered(bytes).map_err(|e| match e {
        age::DecryptError::UnknownFormat => PackError::TooNew {
            format: FORMAT + 1,
            core_version: String::new(),
        },
        other => corrupt(other),
    })?;
    let reader = if d.is_scrypt() {
        let Unseal::Passphrase(p) = unseal else {
            return Err(PackError::NeedPassphrase);
        };
        let mut id = age::scrypt::Identity::new(age::secrecy::SecretString::from(p.to_string()));
        id.set_max_work_factor(MAX_WORK_FACTOR);
        d.decrypt(std::iter::once(&id as &dyn age::Identity))
            .map_err(|e| match e {
                age::DecryptError::DecryptionFailed | age::DecryptError::NoMatchingKeys => {
                    PackError::WrongPassphrase
                }
                other => corrupt(other),
            })?
    } else {
        let Unseal::Identity(id) = unseal else {
            return Err(PackError::NeedKey);
        };
        d.decrypt(std::iter::once(id as &dyn age::Identity))
            .map_err(|e| match e {
                age::DecryptError::NoMatchingKeys | age::DecryptError::DecryptionFailed => {
                    PackError::WrongKey
                }
                other => corrupt(other),
            })?
    };
    let mut out = Vec::new();
    reader
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut out)
        .map_err(corrupt)?;
    if out.len() > MAX_BYTES {
        return Err(PackError::TooLarge { limit: MAX_BYTES });
    }
    Ok(out)
}

/// ZIP 里的一个路径是什么。
enum Entry {
    Manifest,
    Config,
    Prefs,
    Script(String),
    Approved(String),
    /// `plugins/`、`plugins/.approved/` 这两个目录条目：别的工具打包时会带上，无害
    Dir,
}

fn entry_of(name: &str) -> Option<Entry> {
    let plugin = |rest: &str| {
        let id = rest.strip_suffix(".js")?;
        tw_config::plugins::valid_id(id).then(|| id.to_string())
    };
    match name {
        MANIFEST => Some(Entry::Manifest),
        CONFIG => Some(Entry::Config),
        PREFS => Some(Entry::Prefs),
        PLUGINS | APPROVED => Some(Entry::Dir),
        _ => {
            if let Some(rest) = name.strip_prefix(APPROVED) {
                plugin(rest).map(Entry::Approved)
            } else if let Some(rest) = name.strip_prefix(PLUGINS) {
                plugin(rest).map(Entry::Script)
            } else {
                None
            }
        }
    }
}

fn unzip(bytes: &[u8]) -> Result<Pack, PackError> {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(corrupt)?;
    if z.len() > MAX_ENTRIES {
        return Err(corrupt(format!("it has {} entries", z.len())));
    }
    let mut manifest: Option<Manifest> = None;
    let mut config: Option<String> = None;
    let mut prefs: Option<serde_json::Value> = None;
    let mut scripts: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut approved: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut seen = std::collections::BTreeSet::new();
    // 解压出来的总量。**按真读到的算**：条目头里写的大小可以是假的
    let mut budget = MAX_BYTES;
    for i in 0..z.len() {
        let mut f = z.by_index(i).map_err(corrupt)?;
        let name = f.name().map_err(corrupt)?.to_string();
        let Some(kind) = entry_of(&name) else {
            return Err(PackError::BadEntry { name });
        };
        if !seen.insert(name.clone()) {
            return Err(PackError::BadEntry { name });
        }
        if matches!(kind, Entry::Dir) {
            continue;
        }
        if f.is_dir() {
            return Err(PackError::BadEntry { name });
        }
        let mut buf = Vec::new();
        (&mut f)
            .take(budget as u64 + 1)
            .read_to_end(&mut buf)
            .map_err(corrupt)?;
        if buf.len() > budget {
            return Err(PackError::TooLarge { limit: MAX_BYTES });
        }
        budget -= buf.len();
        match kind {
            Entry::Manifest => {
                // 先按最宽的读出格式版本：更新的格式整份读不懂时，要说的是「太新」
                let raw: serde_json::Value = serde_json::from_slice(&buf)
                    .map_err(|e| corrupt(format!("{MANIFEST}: {e}")))?;
                let format = raw.get("format").and_then(serde_json::Value::as_u64);
                if let Some(f) = format
                    && f > u64::from(FORMAT)
                {
                    return Err(PackError::TooNew {
                        format: f as u32,
                        core_version: raw
                            .get("core_version")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    });
                }
                manifest = Some(
                    serde_json::from_value(raw).map_err(|e| corrupt(format!("{MANIFEST}: {e}")))?,
                );
            }
            Entry::Config => {
                config = Some(
                    String::from_utf8(buf)
                        .map_err(|_| corrupt(format!("{CONFIG} is not UTF-8")))?,
                );
            }
            Entry::Prefs => {
                prefs = Some(
                    serde_json::from_slice(&buf).map_err(|e| corrupt(format!("{PREFS}: {e}")))?,
                );
            }
            Entry::Script(id) => {
                scripts.insert(id, buf);
            }
            Entry::Approved(id) => {
                approved.insert(id, buf);
            }
            Entry::Dir => {}
        }
    }
    let manifest = manifest.ok_or_else(|| PackError::MissingEntry {
        name: MANIFEST.to_string(),
    })?;
    let config_yaml = config.ok_or_else(|| PackError::MissingEntry {
        name: CONFIG.to_string(),
    })?;
    // 只有底稿、没有脚本的不是一个插件
    if let Some(id) = approved.keys().find(|id| !scripts.contains_key(*id)) {
        return Err(PackError::BadEntry {
            name: format!("{APPROVED}{id}.js"),
        });
    }
    let plugins = scripts
        .into_iter()
        .map(|(id, script)| {
            let approved = approved.remove(&id);
            (id, PluginFiles { script, approved })
        })
        .collect();
    Ok(Pack {
        manifest,
        config_yaml,
        plugins,
        client_prefs: prefs,
    })
}

// ─────────────────────────────────────────────────────────── 杂项

impl Pack {
    /// 这份内容的标识：可以带走的内容（[`model::snapshot`]）规范化之后的 blake3，前 12 位
    /// 十六进制。**本机设置不算**，所以本机备份和它上传到同步位置的那一版对得上；注释、
    /// 排版、默认值写没写都不影响它。读不成配置时按原文算
    pub fn content_id(&self) -> String {
        model::content_id(self)
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `0.77.0` 这样的版本号，比这一份 Core 新吗。读不懂的不算新
pub fn newer_than_us(version: &str) -> bool {
    match (parse_version(version), parse_version(CORE_VERSION)) {
        (Some(theirs), Some(ours)) => theirs > ours,
        _ => false,
    }
}

fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((
        it.next()??,
        it.next().flatten().unwrap_or(0),
        it.next().flatten().unwrap_or(0),
    ))
}

#[cfg(test)]
mod tests;
