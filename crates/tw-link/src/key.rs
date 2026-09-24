//! 从 config.yaml 里读钥匙。
//!
//! **只读不写**：钥匙由 core 生成、补上（`twcore serve` 在控制面起来之前
//! 保证它在），桌面端和 `twcore call` 只从这里读。两边找的是同一个位置
//! （[`tw_api::control::KEY_PATH`]），用的是同一个解析。

use std::path::Path;

use tw_api::control::{ControlKey, KEY_PATH, KeyFormatError};

/// 读不到一把能用的钥匙。
#[derive(Debug, thiserror::Error)]
pub enum KeyReadError {
    /// 文件读不了。**不存在是常态**：core 第一次起来之前还没有配置，应用
    /// 把它和「socket 还没出现」当成同一件事 —— core 还没好，等一下再试
    #[error("{path} could not be read: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    /// 配置里没有 `listen.control.key`。core 起来时会补上，所以同样是「还没好」
    #[error("the configuration has no listen.control.key")]
    Missing,
    /// 写了，但不是一把钥匙
    #[error("listen.control.key is not a control key: {0}")]
    Invalid(KeyFormatError),
    /// 这份 YAML 找不到那个位置（写坏了，或者那里是个映射）
    #[error("listen.control.key could not be read from the configuration: {0}")]
    Unreadable(String),
}

/// 读 `path` 这份配置里的钥匙。
pub fn read_key(path: &Path) -> Result<ControlKey, KeyReadError> {
    let text = std::fs::read_to_string(path).map_err(|source| KeyReadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    key_in_config(&text)
}

/// 同 [`read_key`]，拿的是配置原文。
pub fn key_in_config(text: &str) -> Result<ControlKey, KeyReadError> {
    let at: Vec<tw_yaml::Step> = KEY_PATH.iter().map(|k| tw_yaml::Step::key(*k)).collect();
    match tw_yaml::find(text, &at) {
        Ok(found) => ControlKey::parse(&found.value).map_err(KeyReadError::Invalid),
        Err(tw_yaml::PatchError::NotFound(_)) => Err(KeyReadError::Missing),
        Err(e) => Err(KeyReadError::Unreadable(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    #[test]
    fn it_finds_the_key_in_block_and_flow_style() {
        let block = format!("version: 1\nlisten:\n  control:\n    key: {HEX}\n");
        assert_eq!(key_in_config(&block).unwrap().to_hex(), HEX);
        let quoted = format!("version: 1\nlisten:\n  control:\n    key: \"{HEX}\" # the key\n");
        assert_eq!(key_in_config(&quoted).unwrap().to_hex(), HEX);
        let flow = format!("version: 1\nlisten: {{ control: {{ key: '{HEX}' }} }}\n");
        assert_eq!(key_in_config(&flow).unwrap().to_hex(), HEX);
    }

    #[test]
    fn a_missing_key_says_missing_and_a_short_one_says_invalid() {
        assert!(matches!(
            key_in_config("version: 1\n"),
            Err(KeyReadError::Missing)
        ));
        assert!(matches!(
            key_in_config("version: 1\nlisten:\n  control:\n    key: abc\n"),
            Err(KeyReadError::Invalid(KeyFormatError::Length { len: 3 }))
        ));
        // 打码的那一串不是钥匙
        let masked = format!(
            "listen:\n  control:\n    key: {}\n",
            tw_api::control::KEY_MASK
        );
        assert!(matches!(
            key_in_config(&masked),
            Err(KeyReadError::Invalid(_))
        ));
    }

    #[test]
    fn a_missing_file_is_an_io_error_with_not_found() {
        let d = tempfile::tempdir().unwrap();
        match read_key(&d.path().join("config.yaml")) {
            Err(KeyReadError::Io { source, .. }) => {
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound)
            }
            other => panic!("{other:?}"),
        }
    }
}
