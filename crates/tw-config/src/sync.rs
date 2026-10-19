//! `sync` 一节：这台设备怎么参与多设备同步。
//!
//! ```yaml
//! sync:
//!   device_id: 3f9a2c1e-…        # 开启同步时生成
//!   device_name: Mac mini
//!   encrypt: true                # 不写就是 true
//!   passphrase: "…"              # encrypt 为 true 时必填
//!   exclude: [本机 Ollama]        # 不同步的上游
//!   target:
//!     type: webdav               # folder | webdav | s3
//!     url: https://dav.jianguoyun.com/dav/ThinkWatch
//!     user: name@example.com
//!     pass: "…"
//!     proxy: direct              # direct 或出站代理名称
//! ```
//!
//! **这是本机设置**：配置包（导出、同步、分享）不带它，恢复备份时保留当前值。同步口令和
//! 同步位置的凭据和别的密钥一样明文写在这里 —— 这份文件本来就是 0600、只有本机用户读得到。
//!
//! 同步位置就是数据所在的那一层：`keyring`、`versions/`、`devices/` 直接放在配置的文件夹、
//! WebDAV 地址、S3 前缀底下，不再另套一层目录。

use serde::{Deserialize, Serialize};
use tw_types::{Msg, msg};

use crate::DIRECT;

/// 同步口令最少几个字符。**只在设口令的那一刻查**（开启同步、改口令）：加入别人建的同步时
/// 照原样输入，配置里读到的也照原样收
pub const PASSPHRASE_MIN: usize = 12;

/// 设备名称最长几个字符
pub const DEVICE_NAME_MAX: usize = 64;

/// 设备标识至少要有几个十六进制字符：同步位置上的文件名用的是它的前 8 个
pub const DEVICE_ID_PREFIX: usize = 8;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sync {
    /// 随机生成，开启同步时写下。同步位置上的文件名只用它的前 8 个十六进制字符
    pub device_id: String,
    /// 设备列表里显示的名字。缺省为计算机名称
    pub device_name: String,
    /// 同一同步位置上的所有设备采用同一设置。**总是写出来**：这是一件要能一眼看清的事，
    /// 不写时按 `true` 读
    #[serde(default = "yes")]
    pub encrypt: bool,
    /// 同步口令。加密时必填
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// 不同步的上游，按名字。引用它们的路由、别名、分组随之不同步
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    pub target: Target,
}

fn yes() -> bool {
    true
}

fn direct() -> String {
    DIRECT.to_string()
}

fn is_direct(v: &str) -> bool {
    v == DIRECT
}

/// 同步位置。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Target {
    /// 本机（或服务器上）的一个文件夹，由 iCloud Drive、Dropbox、Syncthing 这类客户端在
    /// 设备之间传送
    Folder {
        path: String,
        /// 文件夹用不到，为了三种写法一致才有
        #[serde(default = "direct", skip_serializing_if = "is_direct")]
        proxy: String,
    },
    Webdav {
        url: String,
        #[serde(default)]
        user: String,
        #[serde(default)]
        pass: String,
        #[serde(default = "direct", skip_serializing_if = "is_direct")]
        proxy: String,
    },
    S3 {
        endpoint: String,
        region: String,
        bucket: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        prefix: String,
        access_key_id: String,
        secret_access_key: String,
        #[serde(default = "direct", skip_serializing_if = "is_direct")]
        proxy: String,
    },
}

impl Target {
    /// 出站请求走哪个代理：`direct`、`system`，或 `proxies` 里的一个名字
    pub fn proxy(&self) -> &str {
        match self {
            Target::Folder { proxy, .. }
            | Target::Webdav { proxy, .. }
            | Target::S3 { proxy, .. } => proxy,
        }
    }

    /// `folder` / `webdav` / `s3`
    pub fn kind(&self) -> &'static str {
        match self {
            Target::Folder { .. } => "folder",
            Target::Webdav { .. } => "webdav",
            Target::S3 { .. } => "s3",
        }
    }

    /// 两个设置指的是不是同一个同步位置（凭据和代理不算）。换了位置要重新加入，
    /// 只换了凭据不用
    pub fn same_place(&self, other: &Target) -> bool {
        match (self, other) {
            (Target::Folder { path: a, .. }, Target::Folder { path: b, .. }) => {
                a.trim_end_matches(['/', '\\']) == b.trim_end_matches(['/', '\\'])
            }
            (Target::Webdav { url: a, .. }, Target::Webdav { url: b, .. }) => {
                a.trim_end_matches('/') == b.trim_end_matches('/')
            }
            (
                Target::S3 {
                    endpoint: e1,
                    bucket: b1,
                    prefix: p1,
                    ..
                },
                Target::S3 {
                    endpoint: e2,
                    bucket: b2,
                    prefix: p2,
                    ..
                },
            ) => {
                e1.trim_end_matches('/') == e2.trim_end_matches('/')
                    && b1 == b2
                    && p1.trim_matches('/') == p2.trim_matches('/')
            }
            _ => false,
        }
    }
}

/// **口令和凭据不打印。**`Config` 会整个落进 Debug 输出，而日志是会被贴进 issue 的
impl std::fmt::Debug for Sync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sync")
            .field("device_id", &self.device_id)
            .field("device_name", &self.device_name)
            .field("encrypt", &self.encrypt)
            .field(
                "passphrase",
                &self.passphrase.as_ref().map(|_| "<redacted>"),
            )
            .field("exclude", &self.exclude)
            .field("target", &self.target)
            .finish()
    }
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Folder { path, .. } => f.debug_struct("Folder").field("path", path).finish(),
            Target::Webdav {
                url, user, proxy, ..
            } => f
                .debug_struct("Webdav")
                .field("url", url)
                .field("user", user)
                .field("pass", &"<redacted>")
                .field("proxy", proxy)
                .finish(),
            Target::S3 {
                endpoint,
                region,
                bucket,
                prefix,
                proxy,
                ..
            } => f
                .debug_struct("S3")
                .field("endpoint", endpoint)
                .field("region", region)
                .field("bucket", bucket)
                .field("prefix", prefix)
                .field("access_key_id", &"<redacted>")
                .field("secret_access_key", &"<redacted>")
                .field("proxy", proxy)
                .finish(),
        }
    }
}

/// `sync` 一节写得不对。**英文只写一遍**：`Display` 就是 [`SyncError::msg`] 的原句。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SyncError {
    #[error("{}", self.msg())]
    DeviceId,
    #[error("{}", self.msg())]
    DeviceName,
    #[error("{}", self.msg())]
    PassphraseMissing,
    /// `field` 是那个字段在 `sync.target` 下的名字（`path`、`url`、`bucket`……）
    #[error("{}", self.msg())]
    Blank { field: &'static str },
    #[error("{}", self.msg())]
    FolderNotAbsolute { path: String },
    /// 地址不是 `https://`，或者是 `http://` 而主机不在本机或内网
    #[error("{}", self.msg())]
    Address { field: &'static str, url: String },
    #[error("{}", self.msg())]
    Bucket { bucket: String },
    #[error("{}", self.msg())]
    UnknownProxy { proxy: String },
    #[error("{}", self.msg())]
    BlankExclude,
    #[error("{}", self.msg())]
    MultiLine { field: &'static str },
}

impl SyncError {
    pub fn msg(&self) -> Msg {
        match self {
            SyncError::DeviceId => msg!(
                "sync.config.device_id" =>
                "sync.device_id has to start with at least 8 lowercase hexadecimal characters. \
                 It is generated when sync is turned on"
            ),
            SyncError::DeviceName => msg!(
                "sync.config.device_name", max = DEVICE_NAME_MAX =>
                "sync.device_name has to be between 1 and {max} characters on one line"
            ),
            SyncError::PassphraseMissing => msg!(
                "sync.config.passphrase_missing" =>
                "sync.encrypt is on, so sync.passphrase is required"
            ),
            SyncError::Blank { field } => msg!(
                "sync.config.blank", field = field =>
                "sync.target.{field} cannot be empty"
            ),
            SyncError::FolderNotAbsolute { path } => msg!(
                "sync.config.folder_relative", path = path =>
                "the sync folder {path} has to be an absolute path"
            ),
            SyncError::Address { field, url } => msg!(
                "sync.config.address", field = field, url = url =>
                "sync.target.{field} is {url}. Sync locations take https:// addresses; http:// \
                 works only for this machine and the local network"
            ),
            SyncError::Bucket { bucket } => msg!(
                "sync.config.bucket", bucket = bucket =>
                "{bucket} is not a bucket name: use lowercase letters, digits, dots and hyphens"
            ),
            SyncError::UnknownProxy { proxy } => msg!(
                "sync.config.unknown_proxy", proxy = proxy =>
                "the sync location goes through proxy `{proxy}`, which is not defined under \
                 `proxies`; the built-in choices are direct and system"
            ),
            SyncError::BlankExclude => msg!(
                "sync.config.blank_exclude" =>
                "sync.exclude has an empty upstream name"
            ),
            SyncError::MultiLine { field } => msg!(
                "sync.config.multiline", field = field =>
                "sync.{field} has to be on one line"
            ),
        }
    }
}

impl Sync {
    /// 只看这一节自己就能判断的。代理名对不对得上 `proxies` 由 [`check_proxy`] 查
    pub fn check(&self) -> Result<(), SyncError> {
        if !device_id_ok(&self.device_id) {
            return Err(SyncError::DeviceId);
        }
        let name = self.device_name.trim();
        if name.is_empty()
            || self.device_name.chars().count() > DEVICE_NAME_MAX
            || self.device_name.chars().any(char::is_control)
        {
            return Err(SyncError::DeviceName);
        }
        if self.encrypt && self.passphrase.as_deref().is_none_or(str::is_empty) {
            return Err(SyncError::PassphraseMissing);
        }
        if self
            .passphrase
            .as_deref()
            .is_some_and(|p| p.contains(['\n', '\r']))
        {
            return Err(SyncError::MultiLine {
                field: "passphrase",
            });
        }
        if self.exclude.iter().any(|e| e.trim().is_empty()) {
            return Err(SyncError::BlankExclude);
        }
        self.target.check()
    }
}

impl Target {
    pub fn check(&self) -> Result<(), SyncError> {
        let blank = |field: &'static str, v: &str| {
            if v.trim().is_empty() {
                Err(SyncError::Blank { field })
            } else if v.contains(['\n', '\r']) {
                Err(SyncError::MultiLine { field })
            } else {
                Ok(())
            }
        };
        match self {
            Target::Folder { path, .. } => {
                blank("path", path)?;
                if !is_absolute(path) {
                    return Err(SyncError::FolderNotAbsolute { path: path.clone() });
                }
            }
            Target::Webdav {
                url, user, pass, ..
            } => {
                blank("url", url)?;
                if user.contains(['\n', '\r']) {
                    return Err(SyncError::MultiLine { field: "user" });
                }
                if pass.contains(['\n', '\r']) {
                    return Err(SyncError::MultiLine { field: "pass" });
                }
                if !address_ok(url) {
                    return Err(SyncError::Address {
                        field: "url",
                        url: url.clone(),
                    });
                }
            }
            Target::S3 {
                endpoint,
                region,
                bucket,
                prefix,
                access_key_id,
                secret_access_key,
                ..
            } => {
                blank("endpoint", endpoint)?;
                blank("region", region)?;
                blank("bucket", bucket)?;
                blank("access_key_id", access_key_id)?;
                blank("secret_access_key", secret_access_key)?;
                if prefix.contains(['\n', '\r']) {
                    return Err(SyncError::MultiLine { field: "prefix" });
                }
                if !address_ok(endpoint) {
                    return Err(SyncError::Address {
                        field: "endpoint",
                        url: endpoint.clone(),
                    });
                }
                if !bucket_ok(bucket) {
                    return Err(SyncError::Bucket {
                        bucket: bucket.clone(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// 代理名对得上吗：`direct`、`system`，或者 `proxies` 里有的那一个
pub fn check_proxy(target: &Target, proxies: &[crate::Proxy]) -> Result<(), SyncError> {
    let p = target.proxy();
    if p == DIRECT || p == crate::SYSTEM || proxies.iter().any(|x| x.name == p) {
        return Ok(());
    }
    Err(SyncError::UnknownProxy {
        proxy: p.to_string(),
    })
}

/// 设备标识：开头至少 8 个小写十六进制字符，其余是十六进制或连字符（UUID 的写法）
pub fn device_id_ok(id: &str) -> bool {
    id.len() >= DEVICE_ID_PREFIX
        && id.len() <= 64
        && id
            .bytes()
            .take(DEVICE_ID_PREFIX)
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
}

/// 生成一个设备标识（UUID v4 的写法）
pub fn generate_device_id() -> String {
    let b: [u8; 16] = rand::random();
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-4{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[13..16],
        &h[16..20],
        &h[20..32]
    )
}

/// 文件夹路径是不是绝对路径。**两个平台都认**：远程 core 可能是 Linux，而填路径的那一端
/// 在 Windows 上 —— 判的是写法，不是这台机器
fn is_absolute(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/')
        || p.starts_with("\\\\")
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && matches!(b[2], b'\\' | b'/'))
}

/// 存储桶名：S3 的规矩（3 到 63 个字符，小写字母、数字、点和连字符，字母或数字开头结尾）
fn bucket_ok(b: &str) -> bool {
    (3..=63).contains(&b.len())
        && b.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'.' || c == b'-')
        && b.bytes().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && b.bytes().last().is_some_and(|c| c.is_ascii_alphanumeric())
}

/// 同步位置的地址收不收：`https://`，或者主机在本机、内网的 `http://`（[`plain_http_ok`]）。
/// 不带账号密码、查询串和片段 —— 凭据有自己的字段
pub fn address_ok(raw: &str) -> bool {
    let Ok(u) = url::Url::parse(raw.trim()) else {
        return false;
    };
    if !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return false;
    }
    let Some(host) = u.host() else { return false };
    match u.scheme() {
        "https" => true,
        "http" => plain_http_ok(&host),
        _ => false,
    }
}

/// 内网的主机名后缀：`http://` 收这几种。和桌面端导入链接的规则是同一份
const INTRANET_SUFFIXES: &[&str] = &[
    ".local",
    ".lan",
    ".internal",
    ".intranet",
    ".corp",
    ".home.arpa",
    ".localhost",
];

/// `http://` 收不收这个主机：本机回环地址（`localhost`、`127.0.0.1`、`[::1]`）；私有网段的
/// IPv4（`10/8`、`172.16/12`、`192.168/16`、`100.64/10`、`169.254/16`）和 IPv6（`fc00::/7`、
/// `fe80::/10`）；没有点的单段主机名，或者以内网后缀结尾的。别的一律只收 `https://` ——
/// 明文传过去的是同步位置的密码，和可能没加密的整份配置
pub fn plain_http_ok(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(d) => {
            let d = d.to_ascii_lowercase();
            d == "localhost" || !d.contains('.') || INTRANET_SUFFIXES.iter().any(|s| d.ends_with(s))
        }
        url::Host::Ipv4(ip) => {
            let [a, b, ..] = ip.octets();
            *ip == std::net::Ipv4Addr::LOCALHOST
                || a == 10
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 168)
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
        }
        url::Host::Ipv6(ip) => {
            let first = ip.segments()[0];
            *ip == std::net::Ipv6Addr::LOCALHOST
                || first & 0xfe00 == 0xfc00
                || first & 0xffc0 == 0xfe80
        }
    }
}

/// 从一份**读不进来**的配置原文里尽量找出 `sync` 一节（和它引用的出站代理）。
///
/// 安全模式下在服务的是一份临时的空配置，而出错页要「从同步位置恢复」：同步位置的连接参数
/// 只在磁盘上那份写坏了的原文里。这里只要 `sync` 和 `proxies` 两节各自读得进来就行，别处
/// 坏成什么样都不管。找不到、这一节本身也写坏了是 `None`
pub fn from_raw(text: &str) -> Option<(Sync, Vec<crate::Proxy>)> {
    let doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(text).ok()?;
    let s: Sync = serde_yaml_ng::from_value(doc.get("sync")?.clone()).ok()?;
    s.check().ok()?;
    let proxies = doc
        .get("proxies")
        .and_then(|p| serde_yaml_ng::from_value::<Vec<crate::Proxy>>(p.clone()).ok())
        .unwrap_or_default();
    Some((s, proxies))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "3f9a2c1e-0b1d-4e2f-9a3b-5c6d7e8f9a0b";

    fn webdav(url: &str) -> Sync {
        Sync {
            device_id: ID.into(),
            device_name: "Mac mini".into(),
            encrypt: true,
            passphrase: Some("correct horse battery".into()),
            exclude: vec![],
            target: Target::Webdav {
                url: url.into(),
                user: "u".into(),
                pass: "p".into(),
                proxy: DIRECT.into(),
            },
        }
    }

    #[test]
    fn the_section_round_trips_and_keeps_defaults_out() {
        let text = format!(
            "device_id: {ID}\ndevice_name: Mac mini\npassphrase: secret-secret\ntarget:\n  type: folder\n  path: /Users/x/Sync\n"
        );
        let s: Sync = serde_yaml_ng::from_str(&text).unwrap();
        assert!(s.encrypt, "不写 encrypt 就是加密");
        assert_eq!(s.target.proxy(), DIRECT);
        s.check().unwrap();
        let out = serde_yaml_ng::to_string(&s).unwrap();
        assert!(out.contains("encrypt: true"), "{out}");
        assert!(!out.contains("proxy"), "{out}");
        assert!(!out.contains("exclude"), "{out}");
        let back: Sync = serde_yaml_ng::from_str(&out).unwrap();
        assert!(back == s);
    }

    #[test]
    fn every_target_kind_parses_and_a_typo_is_refused() {
        let s3 = "type: s3\nendpoint: https://abc.r2.cloudflarestorage.com\nregion: auto\nbucket: tw-sync\nprefix: me/\naccess_key_id: AK\nsecret_access_key: SK\nproxy: corp\n";
        let t: Target = serde_yaml_ng::from_str(s3).unwrap();
        assert_eq!(t.kind(), "s3");
        assert_eq!(t.proxy(), "corp");
        t.check().unwrap();
        let typo = "type: webdav\nurl: https://dav.example.com/x\nusr: me\n";
        assert!(serde_yaml_ng::from_str::<Target>(typo).is_err());
        assert!(serde_yaml_ng::from_str::<Target>("type: ftp\npath: /x\n").is_err());
    }

    #[test]
    fn a_plain_http_address_is_only_taken_for_this_machine_and_the_local_network() {
        for ok in [
            "https://dav.jianguoyun.com/dav/ThinkWatch",
            "http://127.0.0.1:5005/dav",
            "http://192.168.1.10:9000",
            "http://10.0.0.2/webdav",
            "http://nas/webdav",
            "http://nas.local:5005",
            "http://[::1]:8080/",
            "http://[fd00::1]/",
        ] {
            assert!(address_ok(ok), "{ok}");
            webdav(ok).check().unwrap();
        }
        for bad in [
            "http://dav.example.com/dav",
            "http://8.8.8.8/",
            "ftp://nas/x",
            "https://user:pw@dav.example.com/",
            "https://dav.example.com/?a=1",
            "dav.example.com",
        ] {
            assert!(!address_ok(bad), "{bad}");
            assert!(
                matches!(webdav(bad).check(), Err(SyncError::Address { .. })),
                "{bad}"
            );
        }
    }

    #[test]
    fn encryption_needs_a_passphrase_and_the_device_id_needs_its_prefix() {
        let mut s = webdav("https://dav.example.com/");
        s.passphrase = None;
        assert_eq!(s.check(), Err(SyncError::PassphraseMissing));
        s.encrypt = false;
        s.check().unwrap();
        s.device_id = "XYZ".into();
        assert_eq!(s.check(), Err(SyncError::DeviceId));
        assert!(device_id_ok(&generate_device_id()));
        assert_ne!(generate_device_id(), generate_device_id());
    }

    #[test]
    fn folder_paths_have_to_be_absolute_on_either_platform() {
        for ok in [
            "/Users/x/iCloud/TW",
            "C:\\Users\\x\\Dropbox\\TW",
            "D:/Sync",
            "\\\\nas\\share",
        ] {
            assert!(is_absolute(ok), "{ok}");
        }
        for bad in ["Sync", "./Sync", "C:Sync", "~/Sync"] {
            assert!(!is_absolute(bad), "{bad}");
        }
    }

    #[test]
    fn debug_never_prints_a_credential() {
        let mut s = webdav("https://dav.example.com/");
        s.passphrase = Some("the-passphrase-itself".into());
        s.target = Target::Webdav {
            url: "https://dav.example.com/".into(),
            user: "me".into(),
            pass: "the-password-itself".into(),
            proxy: DIRECT.into(),
        };
        let shown = format!("{s:?}");
        assert!(!shown.contains("the-passphrase-itself"), "{shown}");
        assert!(!shown.contains("the-password-itself"), "{shown}");
        let s3 = Target::S3 {
            endpoint: "https://s3.example.com".into(),
            region: "auto".into(),
            bucket: "b-1".into(),
            prefix: String::new(),
            access_key_id: "AKIA-ID-VALUE".into(),
            secret_access_key: "SECRET-VALUE".into(),
            proxy: DIRECT.into(),
        };
        let shown = format!("{s3:?}");
        assert!(
            !shown.contains("AKIA-ID-VALUE") && !shown.contains("SECRET-VALUE"),
            "{shown}"
        );
    }

    #[test]
    fn the_section_is_found_in_a_config_that_does_not_load() {
        let text = format!(
            "version: 1\nnot_a_field: 1\nproxies:\n  - name: corp\n    addr: 127.0.0.1:7890\nsync:\n  device_id: {ID}\n  device_name: srv\n  encrypt: false\n  target:\n    type: folder\n    path: /srv/sync\n    proxy: corp\n"
        );
        let (s, proxies) = from_raw(&text).expect("sync 一节读得进来");
        assert_eq!(s.device_name, "srv");
        assert_eq!(proxies.len(), 1);
        assert!(from_raw("version: 1\n").is_none());
        assert!(from_raw("version: [\n").is_none());
    }

    #[test]
    fn the_same_place_ignores_credentials_and_trailing_slashes() {
        let a = webdav("https://dav.example.com/tw/").target;
        let mut b = webdav("https://dav.example.com/tw").target;
        if let Target::Webdav { pass, .. } = &mut b {
            *pass = "other".into();
        }
        assert!(a.same_place(&b));
        assert!(!a.same_place(&webdav("https://dav.example.com/other").target));
    }
}
