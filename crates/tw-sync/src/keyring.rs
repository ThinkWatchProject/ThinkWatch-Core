//! 加密同步的版本密钥。
//!
//! 首台设备生成一把随机的 age x25519 密钥，**以同步口令加密**（age 的口令模式，scrypt 派生）
//! 后存成同步位置上的 `keyring`。各版本、各设备记录都加密给这把密钥的公钥。于是：
//!
//! - 改口令只重新加密 keyring，已有的版本不用动；
//! - 没有口令的一方解不开 keyring，也就既读不了版本、也写不出别的设备认的版本（age 的认证
//!   加密，见 `tw_pack`）。
//!
//! scrypt 的强度是**写死的**（[`WORK_FACTOR`]），不按这台机器的速度现定：一台快的 Mac 按自己
//! 定的强度加密，慢的服务器可能解不开（age 拒绝超过本机上限的强度）。

use std::sync::atomic::{AtomicU8, Ordering};

use age::secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tw_types::{Msg, msg};

/// scrypt 的 N = 2^17：一次约 128 MiB 内存、零点几秒。只在打开 keyring 时算一次
pub const WORK_FACTOR: u8 = 17;

/// 解 keyring 时接受的最高强度。**比写的那个只高一点**：对同步位置有写权限的人能放一个
/// 强度极高的 keyring 进来，让每台设备一打开就耗尽内存
const MAX_WORK_FACTOR: u8 = WORK_FACTOR + 1;

static WORK: AtomicU8 = AtomicU8::new(WORK_FACTOR);

/// **只给测试用**：把 scrypt 的强度调低，一条测试不必每次都算零点几秒。解的时候照样认
pub fn set_work_factor_for_tests(log_n: u8) {
    WORK.store(log_n.clamp(1, WORK_FACTOR), Ordering::Relaxed);
}

/// keyring 解开之后的内容
#[derive(Serialize, Deserialize)]
struct Plain {
    /// 格式版本
    v: u32,
    /// `AGE-SECRET-KEY-1…`
    identity: String,
}

/// 一把解开了的版本密钥。**Debug 不打印它**
pub struct Keyring {
    identity: age::x25519::Identity,
}

impl std::fmt::Debug for Keyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keyring")
            .field("recipient", &self.identity.to_public().to_string())
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KeyringError {
    /// 口令不对。**和「坏了」分开**：前者要人重新输入（别的设备可能刚改过口令）
    #[error("{}", self.msg())]
    WrongPassphrase,
    #[error("{}", self.msg())]
    Corrupt,
    /// 格式版本比这个 core 认得的新
    #[error("{}", self.msg())]
    TooNew,
}

impl KeyringError {
    pub fn msg(&self) -> Msg {
        match self {
            KeyringError::WrongPassphrase => msg!(
                "sync.passphrase_wrong" => "the sync passphrase is not correct"
            ),
            KeyringError::Corrupt => msg!(
                "sync.keyring_corrupt" =>
                "the keyring at the sync location cannot be read. It may still be arriving, or \
                 it is damaged"
            ),
            KeyringError::TooNew => msg!(
                "sync.keyring_too_new" =>
                "the keyring at the sync location was written by a newer version. Update \
                 ThinkWatch Lite to keep syncing"
            ),
        }
    }
}

impl Keyring {
    /// 新的一把
    pub fn generate() -> Self {
        Self {
            identity: age::x25519::Identity::generate(),
        }
    }

    /// 以口令加密，存成 `keyring` 文件的内容
    pub fn seal(&self, passphrase: &str) -> Vec<u8> {
        let plain = Plain {
            v: 1,
            identity: self.identity.to_string().expose_secret().to_string(),
        };
        let json = serde_json::to_vec(&plain).expect("plain data serializes");
        let mut r = age::scrypt::Recipient::new(SecretString::from(passphrase.to_string()));
        r.set_work_factor(WORK.load(Ordering::Relaxed));
        age::encrypt(&r, &json).expect("encrypting to a passphrase does not fail")
    }

    /// 用口令解开 `keyring` 文件
    pub fn open(bytes: &[u8], passphrase: &str) -> Result<Self, KeyringError> {
        let mut id = age::scrypt::Identity::new(SecretString::from(passphrase.to_string()));
        id.set_max_work_factor(MAX_WORK_FACTOR);
        let json = age::decrypt(&id, bytes).map_err(|e| match e {
            age::DecryptError::DecryptionFailed
            | age::DecryptError::KeyDecryptionFailed
            | age::DecryptError::NoMatchingKeys => KeyringError::WrongPassphrase,
            age::DecryptError::UnknownFormat => KeyringError::TooNew,
            _ => KeyringError::Corrupt,
        })?;
        let plain: Plain = serde_json::from_slice(&json).map_err(|_| KeyringError::Corrupt)?;
        if plain.v > 1 {
            return Err(KeyringError::TooNew);
        }
        let identity = plain
            .identity
            .parse::<age::x25519::Identity>()
            .map_err(|_| KeyringError::Corrupt)?;
        Ok(Self { identity })
    }

    /// 加密用的公钥：版本、设备记录都加密给它
    pub fn recipient(&self) -> age::x25519::Recipient {
        self.identity.to_public()
    }

    /// 解密用的私钥
    pub fn identity(&self) -> &age::x25519::Identity {
        &self.identity
    }

    /// 加密一小段数据（设备记录）
    pub fn encrypt(&self, plain: &[u8]) -> Vec<u8> {
        age::encrypt(&self.recipient(), plain).expect("encrypting to an x25519 key does not fail")
    }

    /// 解密一小段数据。解不开（不是这把密钥加密的、被改过、没传完）是 `None`
    pub fn decrypt(&self, bytes: &[u8]) -> Option<Vec<u8>> {
        age::decrypt(&self.identity, bytes).ok()
    }

    /// 两把是不是同一把（公钥相同）
    pub fn same_as(&self, other: &Keyring) -> bool {
        self.recipient().to_string() == other.recipient().to_string()
    }
}

/// 一段字节是不是 age 加密过的（看文件头）
pub fn is_age(bytes: &[u8]) -> bool {
    bytes.starts_with(b"age-encryption.org/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_keyring_opens_with_its_passphrase_and_only_with_it() {
        set_work_factor_for_tests(4);
        let k = Keyring::generate();
        let sealed = k.seal("correct horse battery staple");
        assert!(is_age(&sealed));
        let back = Keyring::open(&sealed, "correct horse battery staple").unwrap();
        assert!(back.same_as(&k));
        assert!(matches!(
            Keyring::open(&sealed, "wrong passphrase!!"),
            Err(KeyringError::WrongPassphrase)
        ));
        assert!(matches!(
            Keyring::open(b"not a keyring", "x"),
            Err(KeyringError::Corrupt)
        ));
    }

    #[test]
    fn changing_the_passphrase_keeps_the_same_key() {
        set_work_factor_for_tests(4);
        let k = Keyring::generate();
        let sealed = k.seal("old passphrase 1");
        let opened = Keyring::open(&sealed, "old passphrase 1").unwrap();
        let resealed = opened.seal("new passphrase 2");
        let again = Keyring::open(&resealed, "new passphrase 2").unwrap();
        assert!(again.same_as(&k), "改口令只重新加密 keyring，密钥本身不变");
        let data = k.encrypt(b"device record");
        assert_eq!(again.decrypt(&data).unwrap(), b"device record");
        assert!(Keyring::generate().decrypt(&data).is_none());
    }

    #[test]
    fn a_keyring_that_demands_too_much_work_is_refused() {
        set_work_factor_for_tests(4);
        // 按强度 4 加密，再把文件头里写的强度改成 30：age 在算 scrypt 之前就按上限拒绝，
        // 测试不用真的去算一遍 2^30
        let sealed = Keyring::generate().seal("p");
        let text = String::from_utf8_lossy(&sealed).to_string();
        let line = text
            .lines()
            .find(|l| l.starts_with("-> scrypt "))
            .expect("口令模式的文件头有一行 scrypt")
            .to_string();
        let tampered = line.trim_end_matches(char::is_numeric).to_string() + "30";
        let mut bytes = sealed.clone();
        let at = text.find(&line).unwrap();
        bytes.splice(at..at + line.len(), tampered.bytes());
        assert!(matches!(
            Keyring::open(&bytes, "p"),
            Err(KeyringError::Corrupt)
        ));
    }

    #[test]
    fn debug_shows_only_the_public_half() {
        let k = Keyring::generate();
        let shown = format!("{k:?}");
        assert!(shown.contains("age1"), "{shown}");
        assert!(!shown.contains("AGE-SECRET-KEY"), "{shown}");
    }
}
