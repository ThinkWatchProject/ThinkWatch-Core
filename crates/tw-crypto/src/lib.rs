//! AES-256-GCM 信封加解密，以及「JSON 里的某个字段是密文」这一约定。
//!
//! 桌面版当前不调用（密钥明文写在 config.yaml，见 DESIGN.md §3.2），
//! 但企业版依赖它，所以它住在共用层。

pub mod crypto;
pub mod json_secret;

pub use crypto::{CURRENT_KEY_VERSION, decrypt, encrypt, parse_encryption_key};
pub use json_secret::{ENC_MARKER, JsonSecret};
