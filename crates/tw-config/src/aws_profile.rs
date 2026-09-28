//! AWS 的共享凭证文件：`~/.aws/credentials` 和 `~/.aws/config` 里的 profile。
//!
//! Bedrock 上游写 `aws.profile`，请求就用那个 profile 的访问密钥签名。**每个请求都按
//! 文件现在的样子取**：刷新临时凭证的工具（aws-vault、saml2aws、公司自己的脚本）把新
//! 的密钥写回这个文件，下一个请求就用上了，不用改配置、不用重启。文件没变就不重读，
//! 每个请求只多一次 stat。
//!
//! **只读文件里写着的密钥，不执行任何东西**（见 [`crate::credential`] 的模块注释）。
//! 要跑程序、要联网才拿得到凭证的 profile —— IAM Identity Center 登录、
//! `credential_process`、扮演角色、Web 身份令牌 —— 这里接不了，会说清是哪一种。
//!
//! 文件在哪儿和 AWS CLI 一样：`AWS_SHARED_CREDENTIALS_FILE`、`AWS_CONFIG_FILE` 优先，
//! 没有就是用户目录下的 `.aws`。**读的是 core 所在的那台机器**：连着远程 core 时是
//! 服务器上的文件。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use tw_types::{Msg, msg};

/// 要跑程序或者联网才拿得到凭证的写法，按 AWS CLI 认它们的先后排。
///
/// AWS CLI 先看扮演角色、Web 身份和 IAM Identity Center，再看文件里的密钥 —— 一个写了
/// `role_arn` 的 profile 就是要扮演那个角色，旁边写着的密钥只是扮演用的，拿它直接签名
/// 签出来的是另一个身份。`credential_process` 排在凭证文件的密钥之后、配置文件的密钥
/// 之前。
const BEFORE_KEYS: &[&str] = &[
    "role_arn",
    "web_identity_token_file",
    "sso_session",
    "sso_start_url",
    "sso_account_id",
];
const PROCESS: &str = "credential_process";

/// 取凭证失败的原因。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    #[error("{}", self.msg())]
    NoHome,
    #[error("{}", self.msg())]
    Unreadable { path: String, detail: String },
    #[error("{}", self.msg())]
    NotFound {
        profile: String,
        credentials: String,
        config: String,
    },
    #[error("{}", self.msg())]
    Unsupported {
        profile: String,
        setting: &'static str,
    },
    #[error("{}", self.msg())]
    NoKeys { profile: String },
}

impl ProfileError {
    pub fn msg(&self) -> Msg {
        match self {
            Self::NoHome => msg!(
                "config.aws_profile.no_home" =>
                "the home directory is unknown, so the AWS credential files cannot be found; set \
                 AWS_SHARED_CREDENTIALS_FILE and AWS_CONFIG_FILE"
            ),
            Self::Unreadable { path, detail } => msg!(
                "config.aws_profile.unreadable", path = path, detail = detail =>
                "{path} could not be read: {detail}"
            ),
            Self::NotFound {
                profile,
                credentials,
                config,
            } => msg!(
                "config.aws_profile.not_found",
                profile = profile, credentials = credentials, config = config =>
                "AWS profile `{profile}` is in neither {credentials} nor {config}"
            ),
            Self::Unsupported { profile, setting } => msg!(
                "config.aws_profile.unsupported", profile = profile, setting = setting =>
                "AWS profile `{profile}` gets its credential through `{setting}`, which ThinkWatch \
                 does not run; it only reads access keys written in the profile"
            ),
            Self::NoKeys { profile } => msg!(
                "config.aws_profile.no_keys", profile = profile =>
                "AWS profile `{profile}` has no access keys: aws_access_key_id and \
                 aws_secret_access_key are both needed"
            ),
        }
    }
}

/// 两个文件在哪儿。
#[derive(Debug, Clone, PartialEq)]
pub struct Files {
    pub credentials: PathBuf,
    pub config: PathBuf,
}

impl Files {
    /// 和 AWS CLI 同一个找法。
    pub fn locate() -> Result<Files, ProfileError> {
        let env = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        // Windows 上 AWS CLI 找的是 USERPROFILE，不是 HOME（Git Bash 会设一个 HOME）
        let home = || {
            let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
            env(var).map(|h| h.join(".aws")).ok_or(ProfileError::NoHome)
        };
        Ok(Files {
            credentials: match env("AWS_SHARED_CREDENTIALS_FILE") {
                Some(p) => p,
                None => home()?.join("credentials"),
            },
            config: match env("AWS_CONFIG_FILE") {
                Some(p) => p,
                None => home()?.join("config"),
            },
        })
    }
}

/// profile 的访问密钥，按文件现在的样子。
pub fn credentials(profile: &str) -> Result<tw_bedrock::Credentials, ProfileError> {
    credentials_in(profile, &Files::locate()?)
}

/// 同上，文件由调用方指定。
pub fn credentials_in(
    profile: &str,
    files: &Files,
) -> Result<tw_bedrock::Credentials, ProfileError> {
    let creds = read(&files.credentials)?;
    let config = read(&files.config)?;
    // 凭证文件里写 `[名字]`；配置文件里写 `[profile 名字]`，只有 default 是 `[default]`
    let in_creds = creds.as_ref().and_then(|ini| ini.get(profile));
    let in_config = config.as_ref().and_then(|ini| {
        ini.get(&format!("profile {profile}"))
            .or_else(|| ini.get(profile).filter(|_| profile == "default"))
    });
    if in_creds.is_none() && in_config.is_none() {
        return Err(ProfileError::NotFound {
            profile: profile.to_string(),
            credentials: shown(&files.credentials),
            config: shown(&files.config),
        });
    }
    let sections = || [in_creds, in_config].into_iter().flatten();
    let unsupported = |setting: &'static str| ProfileError::Unsupported {
        profile: profile.to_string(),
        setting,
    };
    if let Some(s) = BEFORE_KEYS
        .iter()
        .find(|s| sections().any(|sec| sec.contains_key(**s)))
    {
        return Err(unsupported(s));
    }
    if let Some(c) = in_creds.and_then(keys) {
        return Ok(c);
    }
    if sections().any(|sec| sec.contains_key(PROCESS)) {
        return Err(unsupported(PROCESS));
    }
    in_config.and_then(keys).ok_or(ProfileError::NoKeys {
        profile: profile.to_string(),
    })
}

type Section = HashMap<String, String>;
type Ini = HashMap<String, Section>;

/// 一节里的访问密钥。两样都在才算有
fn keys(sec: &Section) -> Option<tw_bedrock::Credentials> {
    let get = |k: &str| sec.get(k).filter(|v| !v.is_empty()).cloned();
    Some(tw_bedrock::Credentials {
        access_key_id: get("aws_access_key_id")?,
        secret_access_key: get("aws_secret_access_key")?,
        session_token: get("aws_session_token"),
    })
}

/// 解析过的文件，和解析时它的样子（修改时间、长度）。
struct Cached {
    stamp: (Option<SystemTime>, u64),
    ini: Arc<Ini>,
}

static CACHE: LazyLock<Mutex<HashMap<PathBuf, Cached>>> = LazyLock::new(Default::default);

/// 读一个文件。**没变就用上次解析的**；不存在是 `None`（两个文件缺一个是常事）。
fn read(path: &Path) -> Result<Option<Arc<Ini>>, ProfileError> {
    let unreadable = |e: std::io::Error| ProfileError::Unreadable {
        path: shown(path),
        detail: e.to_string(),
    };
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unreadable(e)),
    };
    let stamp = (meta.modified().ok(), meta.len());
    let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(c) = cache.get(path).filter(|c| c.stamp == stamp) {
        return Ok(Some(c.ini.clone()));
    }
    let ini = Arc::new(parse(&std::fs::read_to_string(path).map_err(unreadable)?));
    cache.insert(
        path.to_path_buf(),
        Cached {
            stamp,
            ini: ini.clone(),
        },
    );
    Ok(Some(ini))
}

/// AWS 那种 INI。
///
/// - 节名里的空白压成一个空格：`[profile   dev]` 就是 `profile dev`
/// - 键不分大小写，值去掉两头空白。**行尾的 `#` 不是注释** —— AWS CLI 也不这么认，
///   密钥里可以有它
/// - 缩进的行是上一个键的子项（`s3 =` 下面那种），不是这一节的键
fn parse(text: &str) -> Ini {
    let mut out = Ini::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') || l.starts_with(';') {
            continue;
        }
        if let Some(name) = l.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
            out.entry(name.clone()).or_default();
            current = Some(name);
            continue;
        }
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        if let (Some(sec), Some((k, v))) = (current.as_ref(), l.split_once('='))
            && let Some(s) = out.get_mut(sec)
        {
            s.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    out
}

/// 报错里的路径：用户目录写成 `~`。
fn shown(path: &Path) -> String {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    match std::env::var_os(var)
        .filter(|h| !h.is_empty())
        .and_then(|h| path.strip_prefix(h).ok().map(Path::to_path_buf))
    {
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个临时目录里的两个文件。每条测试各用各的目录：缓存按路径记
    fn files(credentials: Option<&str>, config: Option<&str>) -> (tempfile::TempDir, Files) {
        let dir = tempfile::tempdir().unwrap();
        let f = Files {
            credentials: dir.path().join("credentials"),
            config: dir.path().join("config"),
        };
        if let Some(t) = credentials {
            std::fs::write(&f.credentials, t).unwrap();
        }
        if let Some(t) = config {
            std::fs::write(&f.config, t).unwrap();
        }
        (dir, f)
    }

    #[test]
    fn keys_come_from_the_credentials_file_then_from_the_config_file() {
        let (_d, f) = files(
            Some(
                "[default]\naws_access_key_id = AKIADEFAULT\naws_secret_access_key = s1\n\n[dev]\naws_access_key_id=AKIADEV\naws_secret_access_key=s2\naws_session_token = t2\n",
            ),
            Some("[profile ci]\naws_access_key_id = AKIACI\naws_secret_access_key = s3\n"),
        );
        let dev = credentials_in("dev", &f).unwrap();
        assert_eq!(
            (dev.access_key_id.as_str(), dev.secret_access_key.as_str()),
            ("AKIADEV", "s2")
        );
        assert_eq!(dev.session_token.as_deref(), Some("t2"));
        assert_eq!(
            credentials_in("default", &f).unwrap().access_key_id,
            "AKIADEFAULT"
        );
        // 配置文件里也可以写密钥
        assert_eq!(credentials_in("ci", &f).unwrap().access_key_id, "AKIACI");
    }

    #[test]
    fn a_profile_that_needs_a_program_or_a_sign_in_says_which_it_is() {
        let (_d, f) = files(
            Some("[mixed]\naws_access_key_id = AKIASOURCE\naws_secret_access_key = s\n"),
            Some(
                "[profile sso]\nsso_session = corp\nsso_account_id = 1\n\n\
                 [profile proc]\ncredential_process = /usr/local/bin/get-creds\n\n\
                 [profile mixed]\nrole_arn = arn:aws:iam::123456789012:role/Dev\nsource_profile = default\n",
            ),
        );
        let setting = |p: &str| match credentials_in(p, &f) {
            Err(ProfileError::Unsupported { setting, .. }) => setting,
            other => panic!("{p}: {other:?}"),
        };
        assert_eq!(setting("sso"), "sso_session");
        assert_eq!(setting("proc"), "credential_process");
        // 写了 role_arn 就是要扮演角色：旁边的密钥不能拿来直接签名
        assert_eq!(setting("mixed"), "role_arn");
    }

    #[test]
    fn a_missing_profile_and_a_profile_without_keys_are_told_apart() {
        let (_d, f) = files(
            Some("[half]\naws_access_key_id = AKIAHALF\n"),
            Some("[profile regional]\nregion = us-west-2\n"),
        );
        assert!(matches!(
            credentials_in("nope", &f),
            Err(ProfileError::NotFound { .. })
        ));
        assert!(matches!(
            credentials_in("half", &f),
            Err(ProfileError::NoKeys { .. })
        ));
        assert!(matches!(
            credentials_in("regional", &f),
            Err(ProfileError::NoKeys { .. })
        ));
        // `[profile default]` 不是写 default 的地方，但 `[default]` 在配置文件里是
        let (_d, f) = files(
            None,
            Some("[default]\naws_access_key_id = AKIAD\naws_secret_access_key = s\n"),
        );
        assert_eq!(
            credentials_in("default", &f).unwrap().access_key_id,
            "AKIAD"
        );
        // 两个文件都没有
        let (_d, f) = files(None, None);
        assert!(matches!(
            credentials_in("default", &f),
            Err(ProfileError::NotFound { .. })
        ));
    }

    #[test]
    fn a_rewritten_file_is_read_again() {
        let (_d, f) = files(
            Some("[dev]\naws_access_key_id = AKIAOLD\naws_secret_access_key = s\n"),
            None,
        );
        assert_eq!(credentials_in("dev", &f).unwrap().access_key_id, "AKIAOLD");
        // 同样长短的新密钥，改动时间往后挪：只看长度的话会认成没变
        std::fs::write(
            &f.credentials,
            "[dev]\naws_access_key_id = AKIANEW\naws_secret_access_key = s\n",
        )
        .unwrap();
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&f.credentials)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(credentials_in("dev", &f).unwrap().access_key_id, "AKIANEW");
    }

    #[test]
    fn the_format_is_read_the_way_the_aws_cli_reads_it() {
        let ini = parse(
            "# comment\n; also a comment\n[profile   spaced]\nAWS_Access_Key_ID = AKIA#1\n\
             s3 =\n  max_concurrent_requests = 20\n  role_arn = nested\n",
        );
        let sec = &ini["profile spaced"];
        assert_eq!(sec["aws_access_key_id"], "AKIA#1", "行尾的 # 是值的一部分");
        assert!(!sec.contains_key("role_arn"), "缩进的行是 s3 的子项");
        assert!(!sec.contains_key("max_concurrent_requests"));
    }

    #[test]
    fn nothing_the_files_say_ends_up_in_the_error_text() {
        let (_d, f) = files(
            Some("[x]\naws_secret_access_key = wJalrXUtnFEMI/K7MDENG\n"),
            None,
        );
        let e = credentials_in("x", &f).unwrap_err().msg();
        assert!(!e.text.contains("wJalr"), "{}", e.text);
    }
}
